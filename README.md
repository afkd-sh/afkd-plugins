# afkd plugins

The plugins afkd provides. One directory per plugin, named for the plugin
itself, holding that plugin's own `afkd-plugin.toml` at its root.

- [`@afkd/web_top`](@afkd/web_top) — a companion that relays `afkd top`'s
  control wire to a browser.
- [`@afkd/gitea`](@afkd/gitea) — Gitea issues and pull-request reviews as afkd
  triggers, with the skill an agent answers them through.
- [`@afkd/trello`](@afkd/trello) — Trello cards as afkd triggers, with the skill an
  agent works the card through.
- [`@afkd/gitlab`](@afkd/gitlab) — GitLab issues and merge-request reviews as afkd
  triggers, with the skill an agent answers them through.
- [`@afkd/github`](@afkd/github) — GitHub issues and pull-request reviews as afkd
  triggers, with the skill an agent answers them through.
- [`@afkd/linear`](@afkd/linear) — Linear issues as afkd triggers, with the skill an
  agent works the issue through.

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
plugin index, so a name is enough. A release is finished and holds only what the
plugin runs: the five compiled plugins publish one archive per platform (x86_64
and aarch64 Linux, a static binary each) beside their manifest and any skill, and
web_top the relay, the page and its modules. afkd never builds a plugin, so no
operator's host needs a toolchain. `tools/pack-release` is the one list of what
each release ships.

```
afkd install @afkd/web_top
```

`afkd update @afkd/web_top` picks up the next release, provided it raises the
`version` in the plugin's `afkd-plugin.toml`: afkd decides that an install is
already current on the version alone. Each plugin's release tag is fixed and
moves onto the newest release, because `afkd update` re-fetches the source URL
it recorded at install time.
