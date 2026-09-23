# afkd plugins

The plugins afkd provides. One directory per plugin, named for the plugin
itself, holding that plugin's own `afkd-plugin.toml` at its root.

- [`@afkd/web-top`](@afkd/web-top) — a companion that relays `afkd top`'s
  control wire to a browser.

Each plugin carries its own README, its own `node --test` suite and its own
fixtures. A script a plugin is built with but does not ship lives under
`tools/` at the plugin's own path. A plugin that has to be held to a real afkd
also has a drift gate, under `drift/` at the plugin's own path. Every gate is a
member of the Cargo workspace at the root, so one command runs them all:

```console
$ AFKD_SRC=/path/to/afkd cargo test
```

A gate runs the `afkd` first on `PATH`, the installed one, and never builds
afkd. `AFKD_SRC` names an afkd checkout for the tests that read afkd's own
source, in a gate and in a plugin's own suites alike; without one those tests
skip and say so. That is how `.github/workflows/release.yml` runs a plugin's
suites on every push, and a red suite does not publish.

## Installing

Plugins here are published as per-plugin release tarballs and listed in afkd's
plugin index, so a name is enough:

```
afkd install @afkd/web-top
```

`afkd update @afkd/web-top` picks up the next release, provided it raises the
`version` in the plugin's `afkd-plugin.toml`: afkd decides that an install is
already current on the version alone. Each plugin's release tag is fixed and
moves onto the newest release, because `afkd update` re-fetches the source URL
it recorded at install time.
