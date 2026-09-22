# Recorded wire captures

Every `.jsonl` here is what a real afkd daemon wrote to its control socket, line for line.
Nothing is hand-written: `fold.test.mjs` derives its expectations from these frames, so a
re-capture cannot quietly make an assertion vacuous, and a frame shape this fold has never
actually seen cannot sneak into the suite as a guess about the wire.

They ride into every operator's plugin directory on `afkd install`, so they stay small —
about 63 KB for the seven of them.

## How they were recorded

Against a **throwaway** daemon, never the host's live one: `afkd 0.2.123` (debug), an
isolated `HOME` under `/tmp`, `AFKD_HOME` / `XDG_CONFIG_HOME` / `XDG_STATE_HOME` all
cleared (the daemon honours all three, ADR-0075), and a config written from scratch with
**no provider trigger of any kind** — a hand-run daemon on a real config claims real work
items. Every cadence is `every 1h`, so nothing fires on its own and every fire in these
files was asked for by name over the socket.

```console
$ HOME=/tmp/afkd-fixture PATH=/tmp/afkd-fixture/bin:$PATH \
    setsid target/debug/afkd /tmp/afkd-fixture/afkd.conf
```

A ~200-line python recorder opens a second connection to `<state dir>/sock`, sends
`{"afkd":"hello","proto":1}`, and appends every line it reads. Commands are sent on the
same wire as `{"type":"command","command":"fire","service":"…"}` frames. Each file ends
with the recorder still attached while the daemon takes its `SIGINT`, so the drain's
`meta.quitting` is really in the recording rather than assumed.

A stub `claude` on `PATH` prints one success envelope with `num_turns`, `total_cost_usd`
and a `usage` block, so `agent_finished` carries confirmed turns, cost and tokens.

### The config's shape, and why

Ten services, each there for something an assertion needs: a bare one with an `icon` and a
`description`; a namespaced `ops::nightly`; a **two-level** `ops::db::vacuum` (which is what
makes the flat-vs-transitive group assertion non-vacuous); three on one
`queue heavy { parallelism 2 }` lane at `high` / `normal` / `low`; one carrying a
whole-service `sandbox`, so its snapshot entry is `confined`; one whose `init` faults, so
the board has a `crashed` service and a repeated crash alarm; two long `sleep` fires, so
the lane can be filled and a third fire parks `queued`; a service that prints an
adversarial line set; one that exits non-zero; one nested workflow
(`in_parallel` → `times` → `if`); and one removed by the reload.

## The seven files

| File | What was driven |
|---|---|
| `snapshot.jsonl` | Attach only, onto a board already holding `armed`, `busy`, `queued`, `stopped` and `crashed` services, two groups, a filled lane and services with distinct activity ages. The recorder attaches **after** the setup, so the whole board arrives in one `meta.snapshot`. |
| `fire.jsonl` | One workflow service's whole fire — `fire_started` → `step_entered` → `agent_finished` → `fire_ok` — with its logs and trace interleaved as the daemon emitted them, then the lane filled and the same service asked to fire again, which is the `service_queued` edge. |
| `trace-burst.jsonl` | A nested workflow, so the tree has depth (root → `in_parallel` → two leaves), sibling ordering, a `guard`, and a `times` loop that **relabels** its node twice. |
| `noise.jsonl` | The supervisor vocabulary and the adversarial log set: a fire that prints SGR, an OSC residue, an embedded `\r`, tabs, `U+202E`, CJK, an emoji and a zero-width space; a fire that fails; a stop/start round trip (`service_stopping` → `service_paused{true}` → `service_paused{false}` → `service_armed`); a re-arm whose `init` faults again (`service_crashed`, twice, which is the daemon's repeat alarm); and a **mid-fire** stop, whose `service_stopping` and settling `service_paused{true}` bracket the drained fire's own `fire_ok`. |
| `deep-fail.jsonl` | A nested workflow that **fails below its root**, which none of the five above do: a `guard` whose predicate came back false, closing `skipped` over an `else` body whose `run_cmd` failed; an agent (`mender`) that closed `ok` over a tool call the transcript marked `is_error`; and an agent (`scribe`) whose two tool calls both passed. Those are the tree pane's three non-obvious rules — the worst-hidden-status rollup, the recovered `↻` marker, and the per-kind `agent` collapse default with its failing-subtree escape hatch — and a golden that never sees them pins nothing. |
| `loud-fail.jsonl` | One service whose `run_cmd` prints eight lines and **exits 3**, so a `cmd` leaf closes `failed` with its own output attributed to it. That pairing exists nowhere else in the corpus: `noise.jsonl`'s failing fire and `deep-fail.jsonl`'s failing leaves all close with **zero** node-tagged ring lines, which renders `with_bodies`' auto-expand arm — a failed leaf showing its output unasked — true and invisible. Its stderr line also lands *third* in the ring though the shell printed it last, which is the real interleaving of two pipes and the reason the body is asserted in ring order. |
| `reload.jsonl` | A real reconcile: a running service removed (`orphaned`, then a `meta.dropped` when it stops), one whose display metadata drifted (`changed`), one whose recipe drifted while running (`stale`), one added (`added`), and the post-reload `order`. |

`meta.host_load` rides along in all of them.

### The second session, and what it did not touch

`deep-fail.jsonl` was recorded in its **own** session against `afkd 0.2.127` (debug), the same
way and under the same rules: a throwaway daemon, an isolated `HOME` under `/tmp`, `AFKD_HOME` /
`XDG_CONFIG_HOME` / `XDG_STATE_HOME` cleared, a config with no provider trigger and an
`every 1h` cadence, and the one service fired by name over the socket. The original five were
**not** re-recorded, so no existing golden moved.

Its config is one service over one workflow — `run_agent scribe`, `run_agent mender`, then an
`if run_cmd "false" { … } else { run_cmd "exit 3" }`. Each construct is there for a shape the
daemon's own code produces: `run_workflow` is the only step that opens a `NodeKind::Workflow`,
so it is what gives the capture a root; `crates/claude/src/stream.rs` opens a `Tool` node per
`tool_use` parented onto the agent's node and closes it `Failed` on an `is_error` result, while
the agent node itself closes off the worker's *success* envelope — which is the one real `↻`
case; and a false guard closes its own node `Skipped` while a faulting `else` body still
propagates, which is a non-failing parent hiding a `✗`.

The stub `claude` is the existing one plus a `stream-json` transcript per invocation, switched on
a counter file so the two calls differ deterministically: the first prints two `tool_result`
blocks with `is_error: false`, the second an `is_error: true` followed by a passing one. Both
print the same terminal `result` envelope, which is why both agent nodes close `ok`.

A guard *predicate* could not have stood in for either failure: `eval_guard` runs its leaves
through the un-bracketed step path and opens no node at all.

### The third session

`loud-fail.jsonl` was recorded the same way again — `afkd 0.2.127` (debug), a throwaway daemon, an
isolated `HOME` under `/tmp`, the three home variables cleared, one service on an `every 1h`
cadence fired by name over the socket — and neither the original five nor `deep-fail.jsonl` was
re-recorded, so again no existing golden moved. No stub `claude` is involved: its config is one
service and one step, `run_cmd` on a shell script that prints seven lines to stdout, one to
stderr, and exits 3. Eight lines is deliberate — `treeview::BODY_ELIDE` is five, so the rendered
body shows five and counts three, which is the elision boundary with a live case on both sides of
it. One line carries CJK, so the body row's own width measuring is exercised rather than assumed.

## What is *not* in here, and why nothing hand-writes it

`service_checking` and `service_check_done` are a **poller's** beat, and a poller is a
provider trigger — the one thing this capture refuses to configure. `service_error` is a
swallowed trigger diagnostic from the same place. Neither is hand-written into a fixture:
those arms are exercised by the whole-corpus no-throw replay and by nothing else, and no
acceptance criterion names them. A fabricated frame would be a guess about the wire wearing
a recording's clothes.
