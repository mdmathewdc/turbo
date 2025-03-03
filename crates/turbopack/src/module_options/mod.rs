pub(crate) mod custom_module_type;
pub mod module_options_context;
pub mod module_rule;
pub mod rule_condition;

use anyhow::{Context, Result};
pub use custom_module_type::CustomModuleType;
pub use module_options_context::*;
pub use module_rule::*;
pub use rule_condition::*;
use turbo_tasks::Vc;
use turbo_tasks_fs::{glob::Glob, FileSystemPath};
use turbopack_core::{
    reference_type::{CssReferenceSubType, ReferenceType, UrlReferenceSubType},
    resolve::options::{ImportMap, ImportMapping},
};
use turbopack_css::{CssInputTransform, CssModuleAssetType};
use turbopack_ecmascript::{EcmascriptInputTransform, EcmascriptOptions, SpecifiedModuleType};
use turbopack_mdx::MdxTransformOptions;
use turbopack_node::transforms::{postcss::PostCssTransform, webpack::WebpackLoaders};
use turbopack_wasm::source::WebAssemblySourceType;

use crate::evaluate_context::node_evaluate_asset_context;

#[turbo_tasks::function]
async fn package_import_map_from_import_mapping(
    package_name: String,
    package_mapping: Vc<ImportMapping>,
) -> Result<Vc<ImportMap>> {
    let mut import_map = ImportMap::default();
    import_map.insert_exact_alias(
        format!("@vercel/turbopack/{}", package_name),
        package_mapping,
    );
    Ok(import_map.cell())
}

#[turbo_tasks::function]
async fn package_import_map_from_context(
    package_name: String,
    context_path: Vc<FileSystemPath>,
) -> Result<Vc<ImportMap>> {
    let mut import_map = ImportMap::default();
    import_map.insert_exact_alias(
        format!("@vercel/turbopack/{}", package_name),
        ImportMapping::PrimaryAlternative(package_name, Some(context_path)).cell(),
    );
    Ok(import_map.cell())
}

#[turbo_tasks::value(cell = "new", eq = "manual")]
pub struct ModuleOptions {
    pub rules: Vec<ModuleRule>,
}

#[turbo_tasks::value_impl]
impl ModuleOptions {
    #[turbo_tasks::function]
    pub async fn new(
        path: Vc<FileSystemPath>,
        module_options_context: Vc<ModuleOptionsContext>,
    ) -> Result<Vc<ModuleOptions>> {
        let ModuleOptionsContext {
            enable_jsx,
            enable_types,
            enable_tree_shaking,
            ref enable_typescript_transform,
            ref decorators,
            enable_mdx,
            enable_mdx_rs,
            enable_raw_css,
            ref enable_postcss_transform,
            ref enable_webpack_loaders,
            preset_env_versions,
            ref custom_ecma_transform_plugins,
            ref custom_rules,
            execution_context,
            ref rules,
            ..
        } = *module_options_context.await?;
        if !rules.is_empty() {
            let path_value = path.await?;

            for (condition, new_context) in rules.iter() {
                if condition.matches(&path_value).await? {
                    return Ok(ModuleOptions::new(path, *new_context));
                }
            }
        }

        let (before_transform_plugins, after_transform_plugins) =
            if let Some(transform_plugins) = custom_ecma_transform_plugins {
                let transform_plugins = transform_plugins.await?;
                (
                    transform_plugins
                        .source_transforms
                        .iter()
                        .cloned()
                        .map(EcmascriptInputTransform::Plugin)
                        .collect(),
                    transform_plugins
                        .output_transforms
                        .iter()
                        .cloned()
                        .map(EcmascriptInputTransform::Plugin)
                        .collect(),
                )
            } else {
                (vec![], vec![])
            };

        let mut transforms = before_transform_plugins;

        // Order of transforms is important. e.g. if the React transform occurs before
        // Styled JSX, there won't be JSX nodes for Styled JSX to transform.
        // If a custom plugin requires specific order _before_ core transform kicks in,
        // should use `before_transform_plugins`.
        if let Some(enable_jsx) = enable_jsx {
            let jsx = enable_jsx.await?;

            transforms.push(EcmascriptInputTransform::React {
                development: jsx.development,
                refresh: jsx.react_refresh,
                import_source: Vc::cell(jsx.import_source.clone()),
                runtime: Vc::cell(jsx.runtime.clone()),
            });
        }

        let ecmascript_options = EcmascriptOptions {
            split_into_parts: enable_tree_shaking,
            import_parts: enable_tree_shaking,
            ..Default::default()
        };

        if let Some(env) = preset_env_versions {
            transforms.push(EcmascriptInputTransform::PresetEnv(env));
        }

        let ts_transform = if let Some(options) = enable_typescript_transform {
            let options = options.await?;
            Some(EcmascriptInputTransform::TypeScript {
                use_define_for_class_fields: options.use_define_for_class_fields,
            })
        } else {
            None
        };

        let decorators_transform = if let Some(options) = &decorators {
            let options = options.await?;
            options
                .decorators_kind
                .as_ref()
                .map(|kind| EcmascriptInputTransform::Decorators {
                    is_legacy: kind == &DecoratorsKind::Legacy,
                    is_ecma: kind == &DecoratorsKind::Ecma,
                    emit_decorators_metadata: options.emit_decorators_metadata,
                    use_define_for_class_fields: options.use_define_for_class_fields,
                })
        } else {
            None
        };

        let vendor_transforms = Vc::cell(vec![]);
        let ts_app_transforms = if let Some(transform) = &ts_transform {
            let base_transforms = if let Some(decorators_transform) = &decorators_transform {
                vec![decorators_transform.clone(), transform.clone()]
            } else {
                vec![transform.clone()]
            };
            Vc::cell(
                base_transforms
                    .iter()
                    .cloned()
                    .chain(transforms.iter().cloned())
                    .chain(after_transform_plugins.iter().cloned())
                    .collect(),
            )
        } else {
            Vc::cell(transforms.clone())
        };

        let css_transforms = Vc::cell(vec![CssInputTransform::Nested]);
        let mdx_transforms = Vc::cell(
            if let Some(transform) = &ts_transform {
                if let Some(decorators_transform) = &decorators_transform {
                    vec![decorators_transform.clone(), transform.clone()]
                } else {
                    vec![transform.clone()]
                }
            } else {
                vec![]
            }
            .iter()
            .cloned()
            .chain(transforms.iter().cloned())
            .chain(after_transform_plugins.iter().cloned())
            .collect(),
        );

        // Apply decorators transform for the ModuleType::Ecmascript as well after
        // constructing ts_app_transforms. Ecmascript can have decorators for
        // the cases of 1. using jsconfig, to enable ts-specific runtime
        // decorators (i.e legacy) 2. ecma spec decorators
        //
        // Since typescript transform (`ts_app_transforms`) needs to apply decorators
        // _before_ stripping types, we create ts_app_transforms first in a
        // specific order with typescript, then apply decorators to app_transforms.
        let app_transforms = Vc::cell(
            if let Some(decorators_transform) = &decorators_transform {
                vec![decorators_transform.clone()]
            } else {
                vec![]
            }
            .iter()
            .cloned()
            .chain(transforms.iter().cloned())
            .chain(after_transform_plugins.iter().cloned())
            .collect(),
        );

        let mut rules = vec![
            ModuleRule::new(
                ModuleRuleCondition::ResourcePathEndsWith(".json".to_string()),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Json)],
            ),
            ModuleRule::new_all(
                ModuleRuleCondition::any(vec![
                    ModuleRuleCondition::ResourcePathEndsWith(".js".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".jsx".to_string()),
                ]),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Ecmascript {
                    transforms: app_transforms,
                    options: ecmascript_options,
                })],
            ),
            ModuleRule::new_all(
                ModuleRuleCondition::ResourcePathEndsWith(".mjs".to_string()),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Ecmascript {
                    transforms: app_transforms,
                    options: EcmascriptOptions {
                        specified_module_type: SpecifiedModuleType::EcmaScript,
                        ..ecmascript_options
                    },
                })],
            ),
            ModuleRule::new_all(
                ModuleRuleCondition::ResourcePathEndsWith(".cjs".to_string()),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Ecmascript {
                    transforms: app_transforms,
                    options: EcmascriptOptions {
                        specified_module_type: SpecifiedModuleType::CommonJs,
                        ..ecmascript_options
                    },
                })],
            ),
            ModuleRule::new_all(
                ModuleRuleCondition::any(vec![
                    ModuleRuleCondition::ResourcePathEndsWith(".ts".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".tsx".to_string()),
                ]),
                vec![if enable_types {
                    ModuleRuleEffect::ModuleType(ModuleType::TypescriptWithTypes {
                        transforms: ts_app_transforms,
                        options: ecmascript_options,
                    })
                } else {
                    ModuleRuleEffect::ModuleType(ModuleType::Typescript {
                        transforms: ts_app_transforms,
                        options: ecmascript_options,
                    })
                }],
            ),
            ModuleRule::new_all(
                ModuleRuleCondition::any(vec![
                    ModuleRuleCondition::ResourcePathEndsWith(".mts".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".mtsx".to_string()),
                ]),
                vec![if enable_types {
                    ModuleRuleEffect::ModuleType(ModuleType::TypescriptWithTypes {
                        transforms: ts_app_transforms,
                        options: EcmascriptOptions {
                            specified_module_type: SpecifiedModuleType::EcmaScript,
                            ..ecmascript_options
                        },
                    })
                } else {
                    ModuleRuleEffect::ModuleType(ModuleType::Typescript {
                        transforms: ts_app_transforms,
                        options: EcmascriptOptions {
                            specified_module_type: SpecifiedModuleType::EcmaScript,
                            ..ecmascript_options
                        },
                    })
                }],
            ),
            ModuleRule::new_all(
                ModuleRuleCondition::any(vec![
                    ModuleRuleCondition::ResourcePathEndsWith(".cts".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".ctsx".to_string()),
                ]),
                vec![if enable_types {
                    ModuleRuleEffect::ModuleType(ModuleType::TypescriptWithTypes {
                        transforms: ts_app_transforms,
                        options: EcmascriptOptions {
                            specified_module_type: SpecifiedModuleType::CommonJs,
                            ..ecmascript_options
                        },
                    })
                } else {
                    ModuleRuleEffect::ModuleType(ModuleType::Typescript {
                        transforms: ts_app_transforms,
                        options: EcmascriptOptions {
                            specified_module_type: SpecifiedModuleType::CommonJs,
                            ..ecmascript_options
                        },
                    })
                }],
            ),
            ModuleRule::new(
                ModuleRuleCondition::ResourcePathEndsWith(".d.ts".to_string()),
                vec![ModuleRuleEffect::ModuleType(
                    ModuleType::TypescriptDeclaration {
                        transforms: vendor_transforms,
                        options: ecmascript_options,
                    },
                )],
            ),
            ModuleRule::new(
                ModuleRuleCondition::any(vec![
                    ModuleRuleCondition::ResourcePathEndsWith(".apng".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".avif".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".gif".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".ico".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".jpg".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".jpeg".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".png".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".svg".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".webp".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".woff2".to_string()),
                ]),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Static)],
            ),
            ModuleRule::new(
                ModuleRuleCondition::any(vec![ModuleRuleCondition::ResourcePathEndsWith(
                    ".wasm".to_string(),
                )]),
                vec![ModuleRuleEffect::ModuleType(ModuleType::WebAssembly {
                    source_ty: WebAssemblySourceType::Binary,
                })],
            ),
            ModuleRule::new(
                ModuleRuleCondition::any(vec![ModuleRuleCondition::ResourcePathEndsWith(
                    ".wat".to_string(),
                )]),
                vec![ModuleRuleEffect::ModuleType(ModuleType::WebAssembly {
                    source_ty: WebAssemblySourceType::Text,
                })],
            ),
            ModuleRule::new(
                ModuleRuleCondition::ResourcePathHasNoExtension,
                vec![ModuleRuleEffect::ModuleType(ModuleType::Ecmascript {
                    transforms: vendor_transforms,
                    options: ecmascript_options,
                })],
            ),
            ModuleRule::new(
                ModuleRuleCondition::ReferenceType(ReferenceType::Url(
                    UrlReferenceSubType::Undefined,
                )),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Static)],
            ),
        ];

        if enable_raw_css {
            rules.extend([
                ModuleRule::new(
                    ModuleRuleCondition::all(vec![ModuleRuleCondition::ResourcePathEndsWith(
                        ".css".to_string(),
                    )]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::Css {
                        ty: CssModuleAssetType::Default,
                        transforms: css_transforms,
                    })],
                ),
                ModuleRule::new(
                    ModuleRuleCondition::all(vec![ModuleRuleCondition::ResourcePathEndsWith(
                        ".module.css".to_string(),
                    )]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::Css {
                        ty: CssModuleAssetType::Module,
                        transforms: css_transforms,
                    })],
                ),
            ]);
        } else {
            rules.extend([
                ModuleRule::new(
                    ModuleRuleCondition::all(vec![
                        ModuleRuleCondition::ResourcePathEndsWith(".css".to_string()),
                        // Only create a global CSS asset if not `@import`ed from CSS already.
                        ModuleRuleCondition::not(ModuleRuleCondition::ReferenceType(
                            ReferenceType::Css(CssReferenceSubType::AtImport),
                        )),
                    ]),
                    [
                        if let Some(options) = enable_postcss_transform {
                            let execution_context = execution_context.context(
                                "execution_context is required for the postcss_transform",
                            )?;

                            let import_map = if let Some(postcss_package) = options.postcss_package
                            {
                                package_import_map_from_import_mapping(
                                    "postcss".to_string(),
                                    postcss_package,
                                )
                            } else {
                                package_import_map_from_context("postcss".to_string(), path)
                            };
                            Some(ModuleRuleEffect::SourceTransforms(Vc::cell(vec![
                                Vc::upcast(PostCssTransform::new(
                                    node_evaluate_asset_context(
                                        execution_context,
                                        Some(import_map),
                                        None,
                                        "postcss".to_string(),
                                    ),
                                    execution_context,
                                )),
                            ])))
                        } else {
                            None
                        },
                        Some(ModuleRuleEffect::ModuleType(ModuleType::CssGlobal)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                ),
                ModuleRule::new(
                    ModuleRuleCondition::all(vec![
                        ModuleRuleCondition::ResourcePathEndsWith(".module.css".to_string()),
                        // Only create a module CSS asset if not `@import`ed from CSS already.
                        // NOTE: `composes` references should not be treated as `@import`s and
                        // should also create a module CSS asset.
                        ModuleRuleCondition::not(ModuleRuleCondition::ReferenceType(
                            ReferenceType::Css(CssReferenceSubType::AtImport),
                        )),
                    ]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::CssModule)],
                ),
                ModuleRule::new(
                    ModuleRuleCondition::all(vec![
                        ModuleRuleCondition::ResourcePathEndsWith(".css".to_string()),
                        // Create a normal CSS asset if `@import`ed from CSS already.
                        ModuleRuleCondition::ReferenceType(ReferenceType::Css(
                            CssReferenceSubType::AtImport,
                        )),
                    ]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::Css {
                        ty: CssModuleAssetType::Default,
                        transforms: css_transforms,
                    })],
                ),
                ModuleRule::new(
                    ModuleRuleCondition::all(vec![
                        ModuleRuleCondition::ResourcePathEndsWith(".module.css".to_string()),
                        // Create a normal CSS asset if `@import`ed from CSS already.
                        ModuleRuleCondition::ReferenceType(ReferenceType::Css(
                            CssReferenceSubType::AtImport,
                        )),
                    ]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::Css {
                        ty: CssModuleAssetType::Module,
                        transforms: css_transforms,
                    })],
                ),
                ModuleRule::new_internal(
                    ModuleRuleCondition::all(vec![ModuleRuleCondition::ResourcePathEndsWith(
                        ".css".to_string(),
                    )]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::Css {
                        ty: CssModuleAssetType::Default,
                        transforms: css_transforms,
                    })],
                ),
                ModuleRule::new_internal(
                    ModuleRuleCondition::all(vec![ModuleRuleCondition::ResourcePathEndsWith(
                        ".module.css".to_string(),
                    )]),
                    vec![ModuleRuleEffect::ModuleType(ModuleType::Css {
                        ty: CssModuleAssetType::Module,
                        transforms: css_transforms,
                    })],
                ),
            ]);
        }

        if enable_mdx || enable_mdx_rs.is_some() {
            let (jsx_runtime, jsx_import_source) = if let Some(enable_jsx) = enable_jsx {
                let jsx = enable_jsx.await?;
                (jsx.runtime.clone(), jsx.import_source.clone())
            } else {
                (None, None)
            };

            let mdx_options = enable_mdx_rs
                .unwrap_or(MdxTransformModuleOptions::default())
                .await?;

            let mdx_transform_options = (MdxTransformOptions {
                development: true,
                preserve_jsx: false,
                jsx_runtime,
                jsx_import_source,
                provider_import_source: mdx_options.provider_import_source.clone(),
            })
            .cell();

            rules.push(ModuleRule::new(
                ModuleRuleCondition::any(vec![
                    ModuleRuleCondition::ResourcePathEndsWith(".md".to_string()),
                    ModuleRuleCondition::ResourcePathEndsWith(".mdx".to_string()),
                ]),
                vec![ModuleRuleEffect::ModuleType(ModuleType::Mdx {
                    transforms: mdx_transforms,
                    options: mdx_transform_options,
                })],
            ));
        }

        if let Some(webpack_loaders_options) = enable_webpack_loaders {
            let webpack_loaders_options = webpack_loaders_options.await?;
            let execution_context =
                execution_context.context("execution_context is required for webpack_loaders")?;
            let import_map = if let Some(loader_runner_package) =
                webpack_loaders_options.loader_runner_package
            {
                package_import_map_from_import_mapping(
                    "loader-runner".to_string(),
                    loader_runner_package,
                )
            } else {
                package_import_map_from_context("loader-runner".to_string(), path)
            };
            for (glob, rule) in webpack_loaders_options.rules.await?.iter() {
                rules.push(ModuleRule::new(
                    ModuleRuleCondition::All(vec![
                        if !glob.contains('/') {
                            ModuleRuleCondition::ResourceBasePathGlob(
                                Glob::new(glob.clone()).await?,
                            )
                        } else {
                            ModuleRuleCondition::ResourcePathGlob {
                                base: execution_context.project_path().await?,
                                glob: Glob::new(glob.clone()).await?,
                            }
                        },
                        ModuleRuleCondition::not(ModuleRuleCondition::ResourceIsVirtualSource),
                    ]),
                    vec![
                        // By default, loaders are expected to return ecmascript code.
                        // This can be overriden by specifying e. g. `as: "*.css"` in the rule.
                        ModuleRuleEffect::ModuleType(ModuleType::Ecmascript {
                            transforms: app_transforms,
                            options: ecmascript_options,
                        }),
                        ModuleRuleEffect::SourceTransforms(Vc::cell(vec![Vc::upcast(
                            WebpackLoaders::new(
                                node_evaluate_asset_context(
                                    execution_context,
                                    Some(import_map),
                                    None,
                                    "webpack_loaders".to_string(),
                                ),
                                execution_context,
                                rule.loaders,
                                rule.rename_as.clone(),
                            ),
                        )])),
                    ],
                ));
            }
        }

        rules.extend(custom_rules.iter().cloned());

        Ok(ModuleOptions::cell(ModuleOptions { rules }))
    }
}
