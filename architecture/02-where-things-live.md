## Where things live

```
<OS data dir>/TFSApp/hub/apps/<id>/          the installed snapshot — what actually runs
<OS data dir>/TFSApp/hub/registry.json       what is installed, from where, at what version
<OS data dir>/TFSApp/hub/scratch/<pid>/      a release download/verify/extract, removed on exit
<OS data dir>/TFSApp/hub/bin/php             the interpreter shim (see below)
<OS data dir>/TFSApp/hub/bin/tfsapp-hub      the stable hub copy .desktop entries point at
<OS data dir>/TFSApp/<identifier>/           the app's own data, including uploads/ — CONTRACT.md §5
<OS data dir>/applications/<identifier>.desktop  the generated entry — XDG's own directory,
                                                  a sibling of TFSApp/, not a child of it
```

The last line is load-bearing and the reason the first three are siblings of it
rather than parents. An app's data directory derives from its `identifier`
alone: not from the hub, not from the handle it was given, not from where any
binary lives. **The hub never invents its own layout for app data.** Its root
holds installed source and its own registry, and nothing else.

That gives two distinct keys per app, kept apart on purpose:

| key | what it names | where it comes from |
| --- | --- | --- |
| `id` | a directory under the hub's root, and the handle you type | assigned at install, recorded in the registry |
| `identifier` | the data dir, the keyring namespace, the window identity | the app's own manifest |

`paths::RESERVED_IDENTIFIERS` is the single, case-sensitive vocabulary that
keeps app data out of the infrastructure above: `hub` is the hub root,
`TFSApp` the shared vendor directory, and `applications` the XDG desktop-entry
directory. `install` refuses those manifest `identifier` values and explicit
`purge` refuses them before it reads the registry. The hub-local `id` has a
separate collision rule: one ending in `.previous` would occupy the rollback
anchor an `update` deletes and recreates, so `install` refuses it whether it
was derived from `project_name` or passed with `--as`.

