## Source layout

```
core/src/          identity-agnostic supervision (see "The two crates")
hub/src/           the hub proper — one module per concern, tests beside each
hub/Caddyfile.desktop   written into each app's data dir at launch
hub/resources/     the fetched FrankenPHP and composer.phar
```

Every module has its tests in a sibling `*_tests.rs`, and every module carries a
header explaining what it is for and which decision it implements. Those headers
are the primary documentation of the code; this file is the map above them.

