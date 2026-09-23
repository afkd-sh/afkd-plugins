# `@afkd/web-top`

A **companion** plugin: a program the daemon runs alongside itself for as long as it runs.
This one is a **relay** — `afkd top`'s wire, put in front of a browser. Every subscriber
gets its own attach to the control socket, every frame that attach reads is forwarded
verbatim as one server-sent event, and the operator's intents come back the other way as
`command` frames on that same attach. The one thing it does on its own account is the
[disk backfill](#the-disk-backfill): the run history the local socket never serves.

It is python3, standard library only, one file, no build step and no toolchain on the host.
See [`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks.

## It is not authenticated. Read this first.

The relay puts an **unauthenticated** control channel on a TCP port: anyone who can reach
that port can start, stop, restart and fire every service the daemon runs, with the
daemon's own privileges. There is no password, no token and no session.

That is why `bind` defaults to `127.0.0.1`, where the trust boundary is the machine. The
setting exists because an operator with a private network and a reason may widen it, and a
`bind` that refused anything but loopback would be a setting that lies — but widening it is
a decision, not a default. Put a reverse proxy that authenticates in front of it, or leave
it on loopback and reach it through an SSH tunnel.

## Install it, then name it

Installing a companion does not start it; the config naming it does.

```console
$ afkd install @afkd/web-top
Installed @afkd/web-top 0.1.0 (companion)
  name it in a `plugin` block and the daemon will run it
```

```conf
plugin @afkd/web-top {
  port 8771
}
```

The daemon spawns it, hands it the socket it just bound, and pumps its output into the
daemon log under its own tag:

```text
companion @afkd/web-top: started (pid=5467)
companion @afkd/web-top: out (serving http://127.0.0.1:8771)
```

An empty block is legal and comes up on the defaults; a key the manifest does not declare
fails `afkd validate` rather than being quietly ignored.

## Settings

| Key | Default | What it does |
|---|---|---|
| `port` | `8771` | the port to serve on. `0` binds an ephemeral one and prints the port it got |
| `bind` | `127.0.0.1` | the IPv4 address to listen on. Read the section above before you change it |
| `max_clients` | `8` | how many subscribers may be attached at once; the next one is a `503` |
| `runs_dir` | `<state dir>/runs` | where the daemon keeps its run corpus. Read off disk on every subscriber's attach and served ahead of that subscriber's live frames, so a run view opened after a fire is not empty — see *The disk backfill*. Set it when afkd's own top-level `runs_dir` moves the corpus off the default |
| `log_lines` | `2000` | how many log lines the page keeps per service. The terminal dashboard's own `LOG_RING_CAPACITY`, so a browser and a terminal watching one daemon scroll back the same distance. Carried to the page on its `stream` event — the ring it bounds lives in the browser, not here |

Every value arrives as a string — afkd lowers a scalar to a JSON string and a bare key with
no value to `true` — so the program coerces and range-checks in one place and says so in a
sentence when it fails.

## Routes

| Route | What it is |
|---|---|
| `GET /` | the page, from `index.html` beside this file |
| `GET /top.mjs` | the client shell: the connection, the clock, the cell probe and the resize |
| `GET /fold.mjs` | the **fold** — a pure, DOM-free, clock-free reducer from wire frames to the board `afkd top` draws |
| `GET /layout.mjs` | the **layout** — a pure, DOM-free function from that board to a grid of cells |
| `GET /keymap.mjs` | the **keymap** — afkd's stock `DEFAULT_KEYS`, transcribed whole and pinned to the Rust by a test |
| `GET /session.mjs` | the **session** — this tab's cursor, folds, filter, modal, overlay and flash, and the dispatch that turns a press into wire commands |
| `GET /input.mjs` | the **key seam** — a browser `keydown` turned into an afkd chord, and the verbs posted back |
| `GET /paint.mjs` | the **painter** — those cells as DOM text, and nothing else |
| `GET /dashboard.css` | the palette and the grid's type |
| `GET /stream` | this subscriber's own attach, as an event stream |
| `POST /command` | `{"stream":"<id>","command":"fire","service":"nightly"}` → one `command` frame |

The program takes one argument form of its own, for asking what a tab opening *now* would be
served out of the run corpus:

```console
$ web-top --backfill <runs_dir> <service> busy|idle
```

It prints the burst as JSONL — one `backfill` / `backfill_log` frame per line — through the
same producer the stream seam calls, reads no stdin and binds no port.

Static files are served from this directory by an **allowlist of extensions** — `.html`,
`.mjs`, `.css` — as a single path segment. A nested path, a `..`, this README, the manifest
and the program itself are each a `404`.

### The stream

```text
event: stream    data: {"id":"9f1c…","log_lines":2000}   ← this subscriber's handle, and its ring bound
event: welcome   data: {"afkd":"welcome","proto":1,"daemon":"0.2.95","auth":"none"}
data: {"type":"meta","meta":"snapshot",…}  ← from here on, one verbatim wire frame per line
data: {"type":"event","event":"fire_started","service":"nightly"}
: keepalive                                ← every 15 s of quiet
```

and four ways it ends, each with its own event: `refused` (the daemon rejected the
handshake), `closed` (the daemon closed the attach), `bye` (the daemon is shutting down)
and `error` (a wire line past the 64 KiB cap both ends hold).

`POST /command` composes the frame itself from the verb and the service, so a client cannot
smuggle a field onto the wire. A verb outside the control wire's vocabulary is a `400` and
**nothing** is written to the socket; a `stream` id that names no open attach is a `404`.

## How it behaves

- **One attach per subscriber, and no model in this process.** The daemon serves each
  connection its own `meta.snapshot`, so a second browser is a second attach rather than a
  second reader of one mid-stream. The relay holds no replay ring: a subscriber the daemon
  pruned gets `closed` and reopens, which is the honest answer rather than a buffer that
  pretends it never missed anything. The **page** keeps the model — `fold.mjs` folds each
  frame into per-service state, log rings and run trees — and this process forwards. It
  composes frames of its own in exactly one place, the disk backfill below.
- **It is told where the socket is and never resolves one itself.** A companion under a
  relocated afkd home has to reach the daemon that started it.
- **Every duration on the wire is relative and daemon-measured.** The relay forwards them
  untouched; a client stamps them against its own clock. The daemon's clock is never
  subtracted from anyone else's.
- **EOF on stdin is the daemon saying stop.** The listener closes, open streams get a final
  `bye`, and the process exits **0** well inside the daemon's five-second grace — a
  `systemctl stop` must not look like a crash. It never reconnects on its own: the daemon
  restarting it and the page reopening its stream are the two moving parts.

## The fold

`fold.mjs` is the board `afkd top` draws, restated for the browser: `fold(board, frame, now)`
is a pure reducer from the wire's `meta.snapshot` + `event` / `log` / `trace` / `meta`
stream to per-service state — the state word and its badge, the trigger and queue metadata,
the four countdown anchors, the run and usage tallies, a bounded log ring and a bounded run
tree — plus `queues(board)`, the lane rollup the layout reads, and `groups(board)`, its flat
bucketing of the same (the *transitive* group fold belongs with the paint, and lives in
`layout.mjs`). The tui's
own arms are mirrored one for one, and each cites the file it was read off, because the two
can drift and the citation is what makes that findable.

Two properties it holds on purpose:

- **It imports nothing and reads no clock.** `now` is an argument, exactly the way afkd's own
  core takes timestamps as data, so the module runs under `node --test` as readily as in a
  tab. Every duration on the wire is relative and daemon-measured, so each is converted once
  to an anchor in the caller's own monotonic domain — the daemon's clock is never a term in a
  subtraction.
- **Unknown is ignored, never fatal.** An unknown frame type, event tag, meta tag or field
  folds to nothing and the connection stays up. A newer daemon must not break this page.

Its suite is `fold.test.mjs`, driven by JSONL captured off a real daemon under
`fixtures/` — see [`fixtures/README.md`](fixtures/README.md) for how each file was
recorded and what it holds. Run it with `node --test @afkd/web-top/fold.test.mjs`;
the drift gate's `cargo test` runs it too ([against a real afkd](#against-a-real-afkd)), and
skips loudly on a host with no `node`.

## The layout

`layout.mjs` is the second half of the same idea: `layout(board, {cols, rows, selected, offset,
filter, collapsed, typing, flash, confirm, help, now, version})` returns one array of `{text, fg, bg, dim, bold, width}` cells per
screen row — the title bar and its shed ladder, the host-load strip and its four trend lanes,
the five columns and *their* shed ladder, the service and group rows with their icons,
badges, markers, cadences, ages and countdowns, the `Queues` section, the footer legend laid out as
the terminal's own column grid, and the two popups over all of it — the verb-carrying confirm
modal and the `?` overlay, each framed by one `render_popup` that owns the gutter so no call
site pads its own body. It is `crates/tui/src/layout.rs`, `legend.rs` and
`shell.rs`'s width family, restated the way the fold restates `model.rs`, with the same
per-arm citations.

Three properties, and the reason for each:

- **Cells, never pixels.** A `width` is a count of *terminal* cells — a wide grapheme carries
  `2` — and every row sums to exactly `cols`. So the painter sizes a row in cells rather than
  trusting the browser's font metrics, and "nothing wraps into a broken row" is a property
  rather than a hope.
- **Roles, never hexes.** A cell names a palette role; `dashboard.css` is the only file that
  turns one into a colour. Those tones are `crates/tui/src/palette.rs`'s own truecolor arm,
  copied — not imported, the site does not ship to the plugin — from
  `web/src/styles/dashboard.css`'s ladder, and the suite reads the nine straight out of the
  Rust and holds the stylesheet to them.
- **DOM-free.** It names neither `document` nor `window`; `paint.mjs` does the DOM work and
  makes no layout decision. That split is what lets the whole dashboard be rendered to text
  and diffed against the committed golden screens under `goldens/`.

**The page measures its own cell.** On load, on resize and once the font is ready it probes a
hundred-character span for the monospace advance and the line height, derives `(cols, rows)`
from the viewport, and re-lays out. The terminal sheds against whatever pane it has; this
does the same against the browser window.

Its suites are `layout.test.mjs` — fixture-driven the same way and gated by the same `cargo
test` bridge — plus `keymap.test.mjs`, `session.test.mjs` and `input.test.mjs` beside it. A golden holds the screen as **three planes** — what it says, the palette role
each cell says it in, and its weight-or-band, one letter per cell so the planes read under the
text in any terminal. A cell's look is half its meaning here (the gated footer hints, the
trend lanes' recession, the bold column header, the selection bar), and a text-only golden
would pass over a page painted flat. They are regenerated deliberately — `node
@afkd/web-top/layout.test.mjs --write-goldens` — never by the test run.
`paint.test.mjs` and `input.test.mjs` cover the two DOM-touching modules against a **stub**
DOM: a painter that makes no layout decision should need no layout engine to test, and a key
seam that captures rather than steals should need no browser to prove it — the stub is the
sharpest way to say both.

## The keys

The board is pressable. `session.mjs` is the third pure module — a per-tab cursor, fold set,
filter, confirm modal, `?` overlay and flash, plus `press(session, board, chord)`, which is
`shell::translate_key` and `top::top_command` restated: the confirm modal and the help overlay
capture the surface, filter typing is the one non-rebindable arm, and the list resolves through
`global` → `queues` (on a lane row, which it shadows) → `overview`, exactly as the terminal
does. `input.mjs` is the browser half, and the only file here that touches an event.

| Keys | What they do |
|---|---|
| `j`/`k`, `↓`/`↑`, `g`, `G` | move the cursor; the body scrolls to keep it on screen |
| `h`/`←`, `l`/`→` | fold and unfold a group — from a member too, which moves the cursor onto its header |
| `Enter`/`Space` | toggle the group header under the cursor |
| `H` / `L` | fold every group, or open them all |
| `/`, `Esc` | type a needle (the rows narrow as you type), and clear it |
| `s` `x` `t` `r` | start, stop, trigger and restart the selected service — or, on a group header, every eligible member behind a confirm |
| `x` on a wedged service | the force-stop gate: `y` sends one `force`, `n` and `Esc` send nothing |
| `o` | open the selected **service**'s run view — the tree and the log; `o` or `Esc` closes it |
| `i` | open the selected **service**'s info page; `i` or `Esc` closes it |
| the wheel, on an open info page | scroll it — see below |
| `Ctrl+R` | reload the daemon's config. The page intercepts this chord — see below |
| `?` | the overlay: every bound action in all eight scopes, grouped by scope |

These are afkd's own chords, not a second set invented for a browser — `t` triggers and `f`
does not, because that is what `DEFAULT_KEYS` binds.

**Keys are captured, never stolen.** The grid takes a key only while it has focus, so the
address bar keeps working; `Ctrl+T`, `Ctrl+W`, `Ctrl+L`, `Ctrl+N`, `Ctrl+C` and `F5` are an
explicit table of the browser's own chords and are neither dispatched nor prevented. `Ctrl+R`
is the one exception: it is afkd's reload, so the page takes it and the tab does **not**
reload. Everything else is `preventDefault`ed only when the page actually acted on it.

**The board never moves optimistically.** A verb is posted to `POST /command` on this tab's
own attach and the row changes when the daemon's answering event arrives — which is what
`afkd top` does for a fire, and what this page does for all six. So a press that changes
nothing on screen is acknowledged in the footer instead (`Fired nightly`, `Starting 3
services`), on the terminal's own four-second flash timer; `force` is not, because the modal
already asked, and `Ctrl+R` is not, because the daemon's own reload summary owns that line.

**Every tab is its own.** Each browser gets its own attach, so each gets its own cursor,
needle and fold set. Nothing is persisted and nothing is pushed back to the daemon — `afkd
top` makes folds daemon-held with a `view` frame, and sending one would make two tabs share
one cursor.

## The info page

`i` on a service opens its detail page: the five `docs/tui-style.md` §1 sections — `Overview`,
`Trigger`, `Activity`, `Usage`, `Health` — over a full-width `About` band when the service
names a `description`, and none at all when it does not. Two columns at 100 cells and wider,
sections packed greedily onto the shorter one, a single column below that. While it is open the
page owns the key surface: the `global` keys live under it, `i` and `Esc` both return to the
list, and everything else is captured and inert, exactly as `translate_info_key` has it.

It diverges from `shell::render_info` on four points, deliberately, because it follows the
card and §1 rather than the Rust — `0655bf03` replaced §1's layout a day after §1 was written
and never updated the doc. The divergences are:

- **Borderless, with one page-wide label column.** The terminal draws bordered panels and
  aligns each panel's values to that panel's own widest label. Here a bold header and a blank
  spacer do the grouping and every value on the page aligns to one column. The cost is visible
  and is in the committed golden: `Diagnostics (swallowed)` is 23 cells, so a 49-cell column
  leaves 23 for a value and `State` and `Queue` really do wrap at 100 cells.
- **A long value wraps; it is never elided.** `section_body_lines` ellipsizes inside its panel.
  Nothing here does, at any width — which is what makes "no value is clipped" a property rather
  than a hope, and why a URL-shaped trigger value keeps its scheme where `infoview::trim_url_tail`
  would shorten it: §1 says a structured trigger value is echoed from the config, verbatim.
- **The page scrolls, on the wheel.** The terminal's info view is a static page. This one wraps,
  so it overflows a 24-row viewport in its single-column form, and the wheel moves it. The
  gesture is the wheel rather than a key because the `info` scope binds no nav action and
  inventing one would make `keymap.mjs`'s transcription of afkd's table a lie.
- **The phrases are elapsed-only.** `ServiceState`'s four anchors are all *durations*, so
  `afkd @<ip:port> top` never imports the daemon's clock skew — which means no time-of-day
  anchor and no last-fire record cross this wire. So `State` reads `Stopped for 5m` where the
  terminal reads `Stopped since 14:03 (5m ago)`, `Next run` reads `in 59m 55s` without its
  `HH:MM` prefix, and the `Started` and `Last run` rows are **absent** rather than guessed at.
  `Activity` spends its rows on what the wire does carry: `Last activity`, `Avg run time` and
  `Total run time`.

One row is here that `infoview.rs` has nowhere: a **stale** service's `Config` row.
`docs/tui-style.md` §7 already says the `(orphan)`/`(stale)` words live in the help legend and
the info view's `Config` row, and the Rust carries only the orphan half; the stale sentence is
written to the same `tag - actionable sentence` shape from the same vocabulary the 🕸️ legend
note uses. Reconciling the terminal's own two surfaces is not this page's to do.

## The run view

`o` on a service opens its **run view** — the combined trace tree and log pane (ADR-0071,
ADR-0076), the surface an operator actually watches a fire in. `o` or `Esc` returns to the list
with the cursor exactly where it was. One pane is on screen at a time and `Tab` swaps which,
which is `split_geometry`'s own frame: the shown pane takes the whole body and the hidden one a
zero-*height* rect. Above it sits the run service's own list row — composed through the same
cells the list composes it with, over the same shed column set, so it says what the list row
says at every width; from the stretch floor (90 cells) up the two are byte-identical, and below
it the header fits the `Service` column to its own name rather than to the widest on the board,
because it is the only row on its screen — and below it the single focus-aware footer.

| Keys | What they do |
|---|---|
| `Tab` | swap the pane on screen; the hint names its destination (`Tab log`, `Tab tree`) |
| `s` | from the tree, scope the log to the cursor's node **and** show it; from the log, toggle that scope in place |
| `o`/`Esc` | back to the list |
| **tree** `j`/`k`, `g` | move the cursor over the node rows; a leaf's body row is never selectable |
| **tree** `h`/`←`, `l`/`→` | collapse, or step to the parent; expand, step to the first child, or reveal a leaf's own output |
| **tree** `Enter`/`Space` | toggle the row's collapse — or, on a `cmd`/`tool` leaf, its body |
| **tree** `H` / `L` | fold every parent, or open them all, overriding the per-kind default |
| **tree** `f`, `G` | toggle following the frontier; jump to it and follow |
| **log** `j`/`k`, `Ctrl+D`/`Ctrl+U`, `PgDn`/`PgUp` | scroll a line, half a page, a page |
| **log** `g`, `G`, `f` | top; bottom and follow; toggle follow |

Scrolling up in the log drops follow and `f` or `G` restores it — which falls out of
`LogScroll`'s two methods rather than being a special case: **up** resolves the current effective
top first (so a scroll off a followed view steps off the bottom, not from row 0) and always
freezes, and **down** re-engages follow the moment it reaches the last page.

The tree pane is `treeview.rs` restated: one DFS pre-order walk, a collapsed node emitting its
own row and skipping its subtree, the per-kind collapse default (an `agent` folds; the
`sandbox ▸ workflow ▸ agent` skeleton does not) with the escape hatch that a **failing** agent
auto-expands so a `✗` is never unreachable, the rollup that puts the worst hidden status on a
collapsed parent, the `↻` a parent reads when its subtree failed but it came back green, and the
right-flushed `OUT`/`TOOK`/`ST` rail that sheds `OUT` below 64 cells and `TOOK` below 44 and
never sheds `ST`. Under a `failed`/`killed` leaf — or one opened with `Enter` — up to five of
that leaf's own output lines show indented beneath it, then a `… N more`. The log pane is
`logview.rs`: the ring wrapped by display width, never elided, with the scope — and only the
scope — in its own title. The `start–end/total` range `logview` composes is deliberately not
painted, because the terminal's split pane does not paint it either ("retained on the neutral
view … but the split pane renders a fixed `Log` label"); the scrollbar down the pane's right
edge is what says where in the ring it is sitting.

It diverges from the terminal on three points, each forced by the wire rather than by taste:

- **No timestamp column and no day marker.** A `Frame::Log` carries `service`/`stream`/`line`/
  `node` and **no stamp**, and the fold stamps each entry with the page's own monotonic clock.
  Rendering `logview`'s civil `HH:MM:SS.mmm` would be the browser's clock wearing the daemon's —
  the same refusal the info page makes for its elapsed-only phrases.
- **The continuation hang is 2 cells, not 13.** `logview::CONT_INDENT` measured the stamp that is
  now absent; 2 is the smallest indent that still reads as a continuation.
- **A running node's ticking `TOOK` is monotonic.** `took_content` measures a running node
  against a civil calendar this page has no clock for, so it reads `now - openedAt` — an instant
  `fold.mjs` stamps beside the wire's civil `at`. A **closed** node still reads its authoritative
  `elapsed_ms`, so only the live estimate rides the substituted clock.

Nothing in the Rust can fail if those three drift back: the terminal has a stamp, so no test
there is about their absence. They are pinned here instead — by this list, and by the committed
goldens, which would move.

## The disk backfill

A run view opened **after** a fire finished is not empty: on each subscriber's attach the relay
reads the daemon's own run corpus off disk and serves it as `backfill` / `backfill_log` frames —
the two wire variants afkd already carries for exactly this — before that subscriber's first live
frame.

It exists because the daemon will not serve the burst here. `host.rs` sends it only to a **TCP**
client, on the reasoning that a local-socket client shares the filesystem and can read the run
dirs itself; a companion is handed the local socket, so the page is the local client and this is
the read it would otherwise never get. The relay's own section mirrors `crates/app/src/backfill.rs`
and `host::served_backfill_lines` function for function, each one naming the Rust it was read off:

- the **tree** is that service's run-dir window — newest generation, bounded by the run tree's own
  `TRACE_TREE_CAPACITY` nodes, a fire-less lifecycle dir skipped, replayed oldest → newest;
- the **log** is the `run.log` tail of that window's newest dir, sanitized on this side because
  `BackfillLog` carries the already-clean ring text.

Duplicates between the disk read and the live stream are expected and harmless: an identical
same-id `opened` is a no-op in `fold.mjs`, and the ring's content-keyed seam drops a **busy**
service's re-streamed tail once and then disarms.

**When the runs base cannot be read** the relay serves no backfill. It never fails to start over
it, and it never guesses a second path; the verdict is resolved **per attach**, not latched, so a
base that appears later is served without a restart.

Whether it *says* so depends on which kind of "cannot be read" it is:

- the block **named** a `runs_dir` that is not there, or the base is there and this process may
  not read it — a mistake either way. The relay writes one sentence to stderr at start (which
  reaches the daemon log under `companion @afkd/web-top: err`) and sends each subscriber a
  `meta.error` naming the path it tried, which the page flashes;
- nobody named it and the default `<state dir>/runs` is simply **not there yet** — the state of
  every install before its first fire. Silent, on both channels. `afkd top` reads the same disk
  and says nothing about it either, and a page that flashed at every tab until the first fire
  would be louder than the terminal over the same state.

## Against a real afkd

Everything above is held to afkd by the drift gate beside this directory, in
[`drift/@afkd/web-top`](../../drift/@afkd/web-top). It installs this tree into a throwaway
`$HOME`, runs it under a real daemon, and diffs the page's screen against the one `afkd top`
paints on a PTY of the same size, cell for cell. Run it from the repository root:

```console
$ AFKD_SRC=/path/to/afkd cargo test
```

The daemon is the `afkd` first on `PATH` — the install an operator runs, never a build — so
the gate answers whether this page matches the afkd it will meet. `AFKD_SRC` names an afkd
checkout for the checks that read afkd's own source: the keymap and palette transcriptions,
the rail glyph table, the variation-selector scan, the index entry, and the `unicode-width`
every width here is measured against. Without one those checks skip and say so, which is how
the release workflow runs this directory's suites.

## What this card deliberately does not do

- **A few keys are bound and inert.** `v`, `b` and the two lane-width pairs resolve to an action
  this page has nowhere to send. They are **listed** in the `?` overlay all the same, dim and
  with the reason beside them — a chord with a binding is part of the keymap whether or not this
  surface can act on it — and they are reported unhandled, so the browser keeps them. `q` is one
  of these: a page cannot close itself, and `Ctrl+C` is the browser's copy. `o` and `i` on a
  group header or a lane row are a different shape of refusal, and are listed as **partial**:
  `TreeOpen` and `toggle_info_view` both open on the selected *card*, so there is no subject.
- **`H`/`L` in the tree are a blanket override, not a mode.** Both write an explicit override
  onto every parent the tree holds *now*, so a node that opens after the bulk fold takes its own
  per-kind default rather than the bulk choice. The terminal behaves identically, for the same
  reason — its override map is per node id too — and it is worth knowing before it looks like a
  bug the first time an operator opens all and a new agent lands folded.
- **The keymap is the stock one, and no `keys { … }` rebind is mirrored.** The keymap is not
  on the control wire, so `keymap.mjs` ships a transcription of afkd's `DEFAULT_KEYS` — all 51
  rows, all eight scopes, alternates included — pinned to `crates/config/src/keymap.rs` by
  `keymap.test.mjs`, so a rebind in the Rust is a failing test here rather than a page that
  stopped answering a key. What that buys is the rule it has to keep: an action with no chord
  renders **no hint at all** and takes no key — including every operator verb while the daemon
  is draining, which the input side refuses from the same table the footer reads. What it
  costs is that an operator who rebinds `x` sees `x` here and a different key in their
  terminal. A layout whose keyboard cannot reach `/` or `?` loses those actions, exactly as it
  would in a terminal.
- **Groups boot expanded, where the terminal boots flat.** `afkd top` tracks the groups an
  operator has *opened*; this page tracks the ones they have *closed*. The terminal's default
  is never the first thing an operator sees there (it boots the flat list), and this page has
  no flat arm — an opened-set default would show nothing but headers.
- **A popup dims the backdrop and nothing more.** ratatui unions `DIM` into a cell's modifier,
  which over the bold column header would leave it bold *and* dim; a cell here carries one
  weight, so a receded cell takes `dim` alone.
- **One row cell has no twin in `afkd top`.** The confinement marker: the terminal spells
  confinement on its info view (`Sandbox scoped`) and reserves nothing for it on a list row,
  where this page draws the run tree's own 🔒 hard against the name, on the reconcile markers'
  terms — nothing reserved unless the row has one.
- **No TCP, no password, on the daemon side.** It attaches to the local unix socket, where
  `auth` is `none`. The `challenge`/password leg of the handshake is not implemented.
- **No reconnect.** A stream that ends is ended. The page says which way it went and waits
  for a reload.
