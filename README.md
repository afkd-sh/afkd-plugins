# afkd plugins

The plugins afkd provides. One directory per plugin, named for the plugin
itself, holding that plugin's own `afkd-plugin.toml` at its root.

- [`@afkd/web-top`](@afkd/web-top) — a companion that relays `afkd top`'s
  control wire to a browser.

Each plugin carries its own README, its own `node --test` suite and its own
fixtures, and is imported here from the afkd repository. The PTY drift gate
that diffs web-top's screen against a real `afkd top` stays in that repository,
where the daemon it has to build lives; this copy is downstream of it. So do
the suites that read afkd's Rust sources and hold the javascript to them —
`keymap.test.mjs`, and two tests inside `layout.test.mjs` — for the same
reason: the sources they read are not here. Everything else runs on every push,
in `.github/workflows/release.yml`, and a red suite does not publish.

## Installing

Plugins here are published as per-plugin release tarballs and listed in afkd's
plugin index, so a name is enough:

```
afkd install @afkd/web-top
```

`afkd update @afkd/web-top` picks up the next release. Each plugin's release
tag is fixed and moves onto the newest release, because `afkd update`
re-fetches the source URL it recorded at install time.
