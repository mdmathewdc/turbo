Setup
  $ . ${TESTDIR}/../../../helpers/setup.sh
  $ . ${TESTDIR}/../_helpers/setup_monorepo.sh $(pwd)
  $ rm -rf .turbo/runs

  $ ${TURBO} run build --summarize --filter=my-app
  \xe2\x80\xa2 Packages in scope: my-app (esc)
  \xe2\x80\xa2 Running build in 1 packages (esc)
  \xe2\x80\xa2 Remote caching disabled (esc)
  my-app:build: cache miss, executing b42768856d16033a
  my-app:build: 
  my-app:build: > build
  my-app:build: > echo 'building'
  my-app:build: 
  my-app:build: building
  
    Tasks:    1 successful, 1 total
   Cached:    0 cached, 1 total
     Time:\s*[\.0-9]+m?s  (re)
  Summary:    .+\.turbo\/runs\/[a-zA-Z0-9]+.json (re)
  
