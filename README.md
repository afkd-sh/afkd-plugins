# afkd plugins

The plugins afkd provides. One directory per plugin, named for the plugin
itself, holding that plugin's own `afkd-plugin.toml` at its root.

- [`@afkd/web-top`](@afkd/web-top) — a companion that relays `afkd top`'s
  control wire to a browser.

Each plugin carries its own README, its own `node --test` suite and its own
fixtures, and is imported here from the afkd repository. The PTY drift gate
that diffs web-top's screen against a real `afkd top` stays in that repository,
where the daemon it has to build lives; this copy is downstream of it.
