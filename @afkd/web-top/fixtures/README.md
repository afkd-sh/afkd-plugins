# Recorded wire captures

Every `.jsonl` here is what a real afkd daemon wrote to its control socket, line for line.
Nothing is hand-written: `fold.test.mjs` derives its expectations from these frames, so a
re-capture cannot quietly make an assertion vacuous, and a frame shape this fold has never
actually seen cannot sneak into the suite as a guess about the wire.

They ride into every operator's plugin directory on `afkd install`, so they stay small —
about 50 KB for the five of them.

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

## The five files

| File | What was driven |
|---|---|
| `snapshot.jsonl` | Attach only, onto a board already holding `armed`, `busy`, `queued`, `stopped` and `crashed` services, two groups, a filled lane and services with distinct activity ages. The recorder attaches **after** the setup, so the whole board arrives in one `meta.snapshot`. |
| `fire.jsonl` | One workflow service's whole fire — `fire_started` → `step_entered` → `agent_finished` → `fire_ok` — with its logs and trace interleaved as the daemon emitted them, then the lane filled and the same service asked to fire again, which is the `service_queued` edge. |
| `trace-burst.jsonl` | A nested workflow, so the tree has depth (root → `in_parallel` → two leaves), sibling ordering, a `guard`, and a `times` loop that **relabels** its node twice. |
| `noise.jsonl` | The supervisor vocabulary and the adversarial log set: a fire that prints SGR, an OSC residue, an embedded `\r`, tabs, `U+202E`, CJK, an emoji and a zero-width space; a fire that fails; a stop/start round trip (`service_stopping` → `service_paused{true}` → `service_paused{false}` → `service_armed`); a re-arm whose `init` faults again (`service_crashed`, twice, which is the daemon's repeat alarm); and a **mid-fire** stop, whose `service_stopping` and settling `service_paused{true}` bracket the drained fire's own `fire_ok`. |
| `reload.jsonl` | A real reconcile: a running service removed (`orphaned`, then a `meta.dropped` when it stops), one whose display metadata drifted (`changed`), one whose recipe drifted while running (`stale`), one added (`added`), and the post-reload `order`. |

`meta.host_load` rides along in all five.

## What is *not* in here, and why nothing hand-writes it

`service_checking` and `service_check_done` are a **poller's** beat, and a poller is a
provider trigger — the one thing this capture refuses to configure. `service_error` is a
swallowed trigger diagnostic from the same place. Neither is hand-written into a fixture:
those arms are exercised by the whole-corpus no-throw replay and by nothing else, and no
acceptance criterion names them. A fabricated frame would be a guess about the wire wearing
a recording's clothes.
