// The fold's suite, driven by **recorded** wire captures.
//
// Every fixture under `fixtures/` is JSONL a python recorder read off a real afkd daemon's
// control socket, verbatim (see `fixtures/README.md` for the daemon, the config and the
// verbs that drove each file). Nothing in here hand-writes a frame: an expectation is
// derived from the fixture's own frames, so a re-capture cannot silently make an assertion
// vacuous, and the two places a frame *is* synthesised — the unknown-tag legs and the
// tracetree branch isolations — say so and build it by editing a recorded one.
//
// Each fixture-driven test opens by asserting the recording actually contains the variety
// it claims, so a degenerate capture fails loudly instead of passing.
//
// Run: `node --test @afkd/web-top/`

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { LOG_LINES_DEFAULT, TRACE_NODES_CAPACITY, fold, groups, queues, sanitize, seed } from "./fold.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));

/** Every frame of one recorded capture, in the order the daemon sent it. */
function readFixture(name) {
  const text = readFileSync(join(HERE, "fixtures", `${name}.jsonl`), "utf8");
  return text
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line));
}

/**
 * Fold `frames` into a fresh board, one per tick of a plain counter standing in for the
 * caller's monotonic clock. Returns the board, the per-frame `now` and the board after each
 * frame, so an assertion can name the instant a given frame was folded at.
 */
function replay(frames, options = {}) {
  const step = options.step ?? 1000;
  let now = options.start ?? 1_000_000;
  let board = seed(options);
  const clock = [];
  const boards = [];
  for (const frame of frames) {
    clock.push(now);
    board = fold(board, frame, now);
    boards.push(board);
    now += step;
  }
  return { board, clock, boards, end: now };
}

/** The board's shape with its frame tally dropped — what an "unchanged" claim means. */
function sansFrames(board) {
  const { frames, ...rest } = board;
  return rest;
}

/** Recursively freeze, so a stray write anywhere in a board throws in strict mode. */
function deepFreeze(value) {
  if (value === null || typeof value !== "object" || Object.isFrozen(value)) return value;
  Object.freeze(value);
  for (const key of Object.keys(value)) deepFreeze(value[key]);
  return value;
}

/** The frames of one capture that are `trace` events for `service`. */
function traceFrames(frames, service) {
  return frames.filter((f) => f.type === "trace" && f.service === service);
}

/** A structured clone of a recorded frame, so an edit never writes through the fixture. */
function copy(frame) {
  return JSON.parse(JSON.stringify(frame));
}

// --- AC2: a snapshot alone is one row per service ------------------------------------

test("a snapshot alone seeds one row per service, with its whole wire entry", () => {
  const frames = readFixture("snapshot");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  assert.ok(snapshot, "the capture opens with an attach snapshot");

  // The capture is only worth asserting over if it holds the variety it was recorded for.
  const states = new Set(snapshot.services.map((s) => s.state));
  for (const want of ["armed", "busy", "queued", "stopped", "crashed"]) {
    assert.ok(states.has(want), `the snapshot carries a \`${want}\` service (has ${[...states]})`);
  }
  assert.ok(
    snapshot.services.some((s) => s.group === "ops") && snapshot.services.some((s) => s.group === "ops::db"),
    "…a namespaced service and a two-level one",
  );
  assert.ok(snapshot.services.some((s) => s.group === ""), "…and a bare one at the top level");
  assert.ok(snapshot.services.some((s) => s.confined), "…one confined");
  assert.ok(
    snapshot.services.some((s) => s.icon !== "" && s.description !== ""),
    "…one with an icon and a sentence",
  );
  assert.ok(
    snapshot.services.filter((s) => s.queue !== "").length >= 2,
    "…at least two on one lane, with levels and a live parallelism",
  );
  assert.ok(
    snapshot.services.some((s) => s.runs_total > 0 && s.tokens > 0),
    "…and one carrying real run history and usage, so the seed is not all zeros",
  );

  const now = 1_000_000;
  const board = fold(seed(), snapshot, now);
  assert.equal(board.order.length, snapshot.services.length);
  assert.equal(Object.keys(board.services).length, snapshot.services.length);
  assert.deepEqual(
    board.order,
    snapshot.services.map((s) => s.name),
    "the order is the daemon's own",
  );
  assert.equal(board.daemonStartedAt, now - snapshot.uptime_ms);

  const badgeOf = { armed: "Idle", busy: "Busy", queued: "Queued", stopped: "Stopped", crashed: "Crashed" };
  for (const entry of snapshot.services) {
    const svc = board.services[entry.name];
    const at = `${entry.name}`;
    // The wire's word is kept verbatim; only the badge degrades.
    assert.equal(svc.state, entry.state, at);
    assert.equal(svc.badge, badgeOf[entry.state], at);
    assert.equal(svc.triggerLabel, entry.trigger_label, at);
    assert.equal(svc.triggerDetail, entry.trigger_detail, at);
    assert.deepEqual(svc.triggerFields, entry.trigger_fields, at);
    assert.equal(svc.icon, entry.icon, at);
    assert.equal(svc.description, entry.description, at);
    assert.equal(svc.group, entry.group, at);
    assert.equal(svc.queue, entry.queue, at);
    assert.equal(svc.queuePriority, entry.queue_priority, at);
    assert.equal(svc.queueParallelism, entry.queue_parallelism ?? null, at);
    assert.equal(svc.confined, entry.confined, at);
    assert.equal(svc.orphan, entry.orphan, at);
    assert.equal(svc.stale, entry.stale, at);
    assert.equal(svc.poisoned, entry.poisoned, at);
    assert.equal(svc.isPoller, entry.is_poller, at);
    // The daemon's own history and session usage, carried rather than zeroed.
    assert.equal(svc.runs, entry.runs_total, at);
    assert.equal(svc.failures, entry.run_failures_total, at);
    assert.equal(svc.runTimeTotalMs, entry.run_time_total_ms, at);
    assert.equal(svc.okRunTimeTotalMs, entry.ok_run_time_total_ms, at);
    assert.equal(svc.tokens, entry.tokens, at);
    assert.equal(svc.cost, entry.cost, at);
    assert.equal(svc.turns, entry.turns, at);
    // Every countdown re-anchored on the caller's clock; an absent one is blank, never 0.
    assert.equal(svc.nextFireAt, entry.next_ms === undefined ? null : now + entry.next_ms, at);
    assert.equal(svc.lastActivityAt, entry.last_activity_ms === undefined ? null : now - entry.last_activity_ms, at);
    assert.equal(svc.stateEnteredAt, entry.state_ms === undefined ? now : now - entry.state_ms, at);
    assert.equal(svc.inFlightSince, entry.busy_ms > 0 ? now - entry.busy_ms : null, at);
    // A seeded record starts with an empty tree and an empty ring, never a transplant.
    assert.deepEqual(svc.log, { lines: [], dropped: 0 }, at);
    assert.deepEqual(svc.tree, { nodes: {}, roots: [], newest: null }, at);
  }
});

test("a second snapshot replaces the board rather than merging into it", () => {
  const frames = readFixture("snapshot");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const logs = readFixture("noise").filter((f) => f.type === "log");
  assert.ok(logs.length > 0, "the noise capture carries real log lines to fill a ring with");

  let board = fold(seed(), snapshot, 1_000);
  // Give one service a ring and a tree, then re-attach.
  const subject = logs[0].service;
  const relabelled = { ...copy(logs[0]), service: snapshot.services[0].name };
  board = fold(board, relabelled, 2_000);
  const traced = traceFrames(readFixture("trace-burst"), "deep")[0];
  board = fold(board, { ...copy(traced), service: snapshot.services[0].name }, 3_000);
  assert.ok(board.services[snapshot.services[0].name].log.lines.length > 0, `${subject} filled its ring`);

  // A snapshot with one service dropped from it.
  const narrowed = copy(snapshot);
  const gone = narrowed.services.pop().name;
  board = fold(board, narrowed, 4_000);
  assert.equal(board.services[gone], undefined, "a service the new snapshot does not name is dropped");
  assert.deepEqual(board.services[snapshot.services[0].name].log, { lines: [], dropped: 0 }, "the ring is not carried");
  assert.deepEqual(
    board.services[snapshot.services[0].name].tree,
    { nodes: {}, roots: [], newest: null },
    "…nor the tree",
  );
});

// --- AC3: a fire's whole lifecycle ---------------------------------------------------

test("a fire's lifecycle moves the service through busy and back, tallies and all", () => {
  const frames = readFixture("fire");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const subject = frames.find((f) => f.type === "event" && f.event === "fire_started").service;

  const mine = (f) => f.type === "event" && f.service === subject;
  const starts = frames.filter((f) => mine(f) && f.event === "fire_started");
  const oks = frames.filter((f) => mine(f) && f.event === "fire_ok");
  const fails = frames.filter((f) => mine(f) && f.event === "fire_failed");
  const steps = frames.filter((f) => mine(f) && f.event === "step_entered");
  const agents = frames.filter((f) => mine(f) && f.event === "agent_finished");
  const queued = frames.filter((f) => mine(f) && f.event === "service_queued");
  assert.equal(starts.length, 1, "the capture holds exactly one fire for the subject");
  assert.equal(oks.length, 1, "…which finished");
  assert.ok(steps.length >= 2, "…through at least two steps");
  assert.equal(agents.length, 1, "…running an agent that reported its usage");
  assert.ok(agents[0].turns > 0 && agents[0].tokens > 0 && agents[0].cost > 0, "…all three confirmed");
  assert.equal(queued.length, 1, "…and the lane door it waits at on the next fire");

  const { boards } = replay(frames);
  // The badge after each frame, so the transition is read off the replay and not asserted
  // at one hand-picked index.
  const trace = [];
  for (const board of boards) {
    const badge = board.services[subject]?.badge;
    if (badge !== undefined && trace[trace.length - 1] !== badge) trace.push(badge);
  }
  assert.deepEqual(
    trace.slice(0, 4),
    ["Idle", "Busy", "Idle", "Queued"],
    "armed → busy → armed → parked at the lane",
  );
  // …and the tail is the daemon's own drain, which every capture ends with.
  assert.deepEqual(trace.slice(4), ["Stopping", "Stopped"], "…then the drain the capture closes on");

  // In flight exactly while Busy.
  for (const board of boards) {
    const svc = board.services[subject];
    if (svc === undefined) continue;
    if (svc.badge === "Busy") assert.notEqual(svc.inFlightSince, null, "a busy service has a fire clock");
    if (svc.badge === "Idle") assert.equal(svc.inFlightSince, null, "…and an idle one has none");
  }

  const entry = snapshot.services.find((s) => s.name === subject);
  const final = boards[boards.length - 1].services[subject];
  assert.equal(final.runs, entry.runs_total + oks.length + fails.length);
  assert.equal(final.failures, entry.run_failures_total + fails.length);
  // Both wall-time totals grow by the fixture's own `elapsed_ms`; the ok-only one by the
  // `fire_ok` verdicts alone.
  assert.equal(
    final.runTimeTotalMs,
    entry.run_time_total_ms + [...oks, ...fails].reduce((sum, f) => sum + f.elapsed_ms, 0),
  );
  assert.equal(final.okRunTimeTotalMs, entry.ok_run_time_total_ms + oks.reduce((sum, f) => sum + f.elapsed_ms, 0));
  // Summed in the fold's own order, so the float arithmetic is the same arithmetic.
  let tokens = entry.tokens;
  let turns = entry.turns;
  let cost = entry.cost;
  for (const f of agents) {
    if (f.tokens !== undefined) tokens += f.tokens;
    if (f.turns !== undefined) turns += f.turns;
    if (f.cost !== undefined) cost += f.cost;
  }
  assert.equal(final.tokens, tokens);
  assert.equal(final.turns, turns);
  assert.equal(final.cost, cost);
  assert.equal(final.breadcrumb, steps[steps.length - 1].breadcrumb, "the breadcrumb is the last step entered");
  assert.equal(final.queue, queued[0].queue, "…and the lane the wait named");
});

test("a completion with no fire in flight counts the run and none of its time", () => {
  // `complete_fire`'s guard asymmetry: the counts bump outside the `in_flight` guard, the
  // two wall-time totals fold inside it. The frame is a recorded `fire_ok`, replayed
  // against a board seeded from a snapshot in which the service is parked, not firing.
  const snapshot = readFixture("snapshot").find((f) => f.type === "meta" && f.meta === "snapshot");
  const ok = readFixture("fire").find((f) => f.type === "event" && f.event === "fire_ok");
  const entry = snapshot.services.find((s) => s.name === ok.service);
  assert.ok(entry, `the snapshot names ${ok.service}`);
  assert.ok(entry.busy_ms === 0, "…and it is not firing there");
  assert.ok(ok.elapsed_ms > 0, "the recorded completion carries a real elapsed");

  const board = fold(fold(seed(), snapshot, 1_000), ok, 2_000);
  const svc = board.services[ok.service];
  assert.equal(svc.runs, entry.runs_total + 1, "the run still counts");
  assert.equal(svc.runTimeTotalMs, entry.run_time_total_ms, "…and none of its time is credited");
  assert.equal(svc.okRunTimeTotalMs, entry.ok_run_time_total_ms);
  assert.equal(svc.inFlightSince, null);
});

// --- AC4: the run tree ---------------------------------------------------------------

test("a trace burst builds the fixture's own tree", () => {
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const traces = traceFrames(frames, subject);
  const opens = traces.filter((f) => f.event.op === "opened").map((f) => f.event);
  const closes = traces.filter((f) => f.event.op === "closed").map((f) => f.event);
  const relabels = traces.filter((f) => f.event.op === "relabeled").map((f) => f.event);

  assert.ok(opens.length >= 8, `the burst has real breadth (${opens.length} opens)`);
  assert.ok(
    opens.some((e) => e.node.parent !== undefined && opens.some((p) => p.node.id === e.node.parent && p.node.parent !== undefined)),
    "…and real depth: a node whose parent is itself a child",
  );
  assert.ok(new Set(opens.map((e) => e.node.kind)).size >= 4, "…across several node kinds");
  assert.ok(relabels.length >= 1, "…with a relabel");

  const { board } = replay(frames);
  const tree = board.services[subject].tree;

  assert.deepEqual(
    Object.keys(tree.nodes).map(Number).sort((a, b) => a - b),
    opens.map((e) => e.node.id).sort((a, b) => a - b),
    "the node set is exactly what was opened",
  );
  for (const e of opens) {
    const node = tree.nodes[e.node.id];
    const declared = e.node.parent ?? null;
    // The frame's own parentage, or `null` where that parent was never opened (rule 2).
    const expected = declared !== null && opens.some((p) => p.node.id === declared) ? declared : null;
    assert.equal(node.parent, expected, `node ${e.node.id}'s parent`);
    assert.equal(node.kind, e.node.kind);
    assert.equal(node.label, e.node.label, "the as-opened label is never overwritten");
    assert.deepEqual(node.at, e.at);
  }
  for (const e of closes) {
    const node = tree.nodes[e.id];
    assert.equal(node.status, e.status, `node ${e.id}'s status`);
    assert.equal(node.elapsedMs, e.elapsed_ms);
    assert.equal(node.turns, e.turns ?? null);
    assert.equal(node.tokens, e.tokens ?? null);
    assert.equal(node.cost, e.cost ?? null);
    assert.deepEqual(node.out, e.out ?? null);
  }
  for (const e of relabels) {
    // The overlay is the *last* relabel for that id; the as-opened label stands beneath it.
    const last = relabels.filter((r) => r.id === e.id).pop();
    assert.equal(tree.nodes[e.id].relabel, last.label, `node ${e.id}'s live headline`);
  }
  assert.deepEqual(
    tree.roots,
    opens.filter((e) => (e.node.parent ?? null) === null).map((e) => e.node.id),
    "the roots are the parentless opens, in open order",
  );
  assert.equal(tree.newest, Math.max(...opens.map((e) => e.node.id)));
  // Every child is listed by its parent, exactly once.
  for (const [id, node] of Object.entries(tree.nodes)) {
    for (const child of node.children) assert.equal(tree.nodes[child].parent, Number(id));
  }
});

test("a close for an id that was never opened changes nothing", () => {
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const { board, end } = replay(frames);
  const newest = board.services[subject].tree.newest;

  // A recorded close, shifted past every id the capture ever opened.
  const stray = copy(traceFrames(frames, subject).filter((f) => f.event.op === "closed").pop());
  stray.event.id = newest + 100;
  const after = fold(board, stray, end);
  assert.deepEqual(sansFrames(after), sansFrames(board), "the board is untouched");
  assert.equal(after.frames.folded, board.frames.folded + 1, "…and the frame was understood, not ignored");
});

test("an identical replayed root below the frontier is a no-op — the dedup runs first", () => {
  // The dedup and the generation guard both see a **below-frontier declared root**; the
  // dedup returns before the guard is even computed (`tracetree.rs`: "The check runs before
  // rule 4 below"), so this input must leave the tree whole. That ordering is the rule the
  // card names as an acceptance criterion.
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const { board, end } = replay(frames);
  const root = traceFrames(frames, subject).find((f) => f.event.op === "opened" && f.event.node.parent === undefined);
  assert.ok(root, "the capture has a declared root");
  assert.ok(root.event.node.id < board.services[subject].tree.newest, "…strictly below the tree's frontier");

  const after = fold(board, copy(root), end);
  assert.deepEqual(sansFrames(after), sansFrames(board), "no reset, no duplicate, children and closes intact");
  // The replay lands at `end`, thousands of ticks after the root was first folded, so this also
  // pins that the node's monotonic `openedAt` is **not** part of the dedup comparison: if it
  // were, every replayed `opened` would reset the tree — and the run view's disk backfill, which
  // replays a finished run's whole `trace.jsonl` over a live stream, depends on it not doing so.
  const node = after.services[subject].tree.nodes[root.event.node.id];
  assert.notEqual(end, node.openedAt, "the replay really arrived at a different instant");
});

test("a same-id open whose shape differs resets the tree to the incoming node", () => {
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const { board, end } = replay(frames);
  const root = copy(traceFrames(frames, subject).find((f) => f.event.op === "opened" && f.event.node.parent === undefined));
  root.event.node.label = `${root.event.node.label} (re-minted)`;

  const tree = fold(board, root, end).services[subject].tree;
  assert.deepEqual(Object.keys(tree.nodes), [String(root.event.node.id)], "the tree is only the new node");
  assert.deepEqual(tree.roots, [root.event.node.id]);
  assert.equal(tree.nodes[root.event.node.id].label, root.event.node.label);
});

test("a regressed root id resets the tree, and one above the frontier never does", () => {
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const { board, end } = replay(frames);
  const before = board.services[subject].tree;
  const root = traceFrames(frames, subject).find((f) => f.event.op === "opened" && f.event.node.parent === undefined);

  // The guard's *other* side first, which is also what opens a gap of unused ids below the
  // frontier: a root above `newest` collides with nothing and resets nothing.
  const ahead = copy(root);
  ahead.event.node.id = before.newest + 5;
  const widened = fold(board, ahead, end).services[subject].tree;
  assert.equal(widened.newest, before.newest + 5);
  assert.deepEqual(widened.roots, before.roots.concat([ahead.event.node.id]), "every retained root still stands");
  for (const id of Object.keys(before.nodes)) assert.ok(widened.nodes[id] !== undefined, `node ${id} survived`);

  // Now the guard, isolated. This id was **never opened**, so the same-id disjunct is false
  // and `regressed_root` is the only branch that can fire — the shape of a fresh
  // generation's low root colliding with nothing the old generation retains.
  const regressed = copy(root);
  regressed.event.node.id = before.newest + 1;
  assert.equal(widened.nodes[regressed.event.node.id], undefined, "the id is one the capture never opened");
  assert.ok(regressed.event.node.id < widened.newest, "…and below the frontier");
  const after = fold(fold(board, ahead, end), regressed, end + 1000).services[subject].tree;
  assert.deepEqual(Object.keys(after.nodes), [String(regressed.event.node.id)], "the tree is the new generation's");
  assert.deepEqual(after.roots, [regressed.event.node.id]);
});

test("an open whose declared parent was never seen re-roots as an orphan", () => {
  // Rule 2, driven by dropping one middle `opened` out of the recorded burst — the loss the
  // lossy prune class really produces.
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const traces = traceFrames(frames, subject);
  const opens = traces.filter((f) => f.event.op === "opened");
  const lost = opens.find((f) => opens.some((c) => c.event.node.parent === f.event.node.id));
  assert.ok(lost, "the capture has a node with children to lose");
  const orphaned = opens.filter((f) => f.event.node.parent === lost.event.node.id).map((f) => f.event.node.id);

  const { board } = replay(frames.filter((f) => f !== lost));
  const tree = board.services[subject].tree;
  assert.equal(tree.nodes[lost.event.node.id], undefined, "the dropped node is not in the tree");
  for (const id of orphaned) {
    assert.equal(tree.nodes[id].parent, null, `node ${id} re-rooted`);
    assert.ok(tree.roots.includes(id), `…and is a root`);
  }
});

test("nodes still open when the next fire's root lands settle unknown", () => {
  // Rules 3 and 4 together: replay the burst with **every** close dropped, so the whole
  // tree is still running, then open a fresh root above the frontier.
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const opens = traceFrames(frames, subject).filter((f) => f.event.op === "opened");
  const openOnly = frames.filter((f) => !(f.type === "trace" && f.service === subject && f.event.op === "closed"));
  assert.ok(opens.length >= 8, `the burst leaves plenty open (${opens.length} nodes)`);

  const { board, end } = replay(openOnly);
  const running = board.services[subject].tree;
  assert.ok(
    Object.values(running.nodes).every((n) => n.status === "running"),
    "with no closes, every node is still running",
  );

  const next = copy(opens[0]);
  next.event.node.id = running.newest + 1;
  delete next.event.node.parent;
  const tree = fold(board, next, end).services[subject].tree;
  for (const f of opens) {
    assert.equal(tree.nodes[f.event.node.id].status, "unknown", `node ${f.event.node.id} was abandoned`);
  }
  assert.equal(tree.nodes[next.event.node.id].status, "running", "…and the fresh root is not");
});

test("closing a node abandons the descendants whose own closes were lost", () => {
  // Rule 3 on its own: drop the closes of one container's children, then fold the
  // container's **recorded** close.
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const traces = traceFrames(frames, subject);
  const opens = traces.filter((f) => f.event.op === "opened");
  const container = opens.find((f) => opens.some((c) => c.event.node.parent === f.event.node.id && f.event.node.parent !== undefined));
  assert.ok(container, "the capture has a nested container");
  const children = opens.filter((f) => f.event.node.parent === container.event.node.id).map((f) => f.event.node.id);
  assert.ok(children.length >= 1);

  const lossy = frames.filter(
    (f) => !(f.type === "trace" && f.service === subject && f.event.op === "closed" && children.includes(f.event.id)),
  );
  const { board } = replay(lossy);
  const tree = board.services[subject].tree;
  for (const id of children) assert.equal(tree.nodes[id].status, "unknown", `child ${id} settled unknown`);
});

// --- AC5: unknown is a no-op, and nothing throws -------------------------------------

test("an invented type, an invented event tag and an extra field all fold to a no-op", () => {
  const frames = readFixture("fire");
  const { board, end } = replay(frames);

  const hologram = { ...copy(frames.find((f) => f.type === "event")), type: "hologram" };
  const teleported = { ...copy(frames.find((f) => f.type === "event")), event: "service_teleported" };
  const decorated = { ...copy(frames.find((f) => f.type === "event" && f.event === "fire_ok")), quarks: { up: 3 } };

  let current = board;
  for (const [frame, label, counter] of [
    [hologram, "an invented `type`", "ignored"],
    [teleported, "an invented `event` tag", "ignored"],
  ]) {
    const next = fold(current, frame, end);
    assert.deepEqual(sansFrames(next), sansFrames(current), `${label} moves nothing`);
    assert.equal(next.frames[counter], current.frames[counter] + 1, `…and is counted, never fatal`);
    current = next;
  }
  // An extra field on a **known** frame is not a no-op — the frame still folds — but the
  // unknown key must contribute nothing, so folding it twice lands exactly where folding
  // the clean recording twice does.
  const clean = copy(frames.find((f) => f.type === "event" && f.event === "fire_ok"));
  assert.deepEqual(
    sansFrames(fold(current, decorated, end)),
    sansFrames(fold(current, clean, end)),
    "an unknown field on a known frame contributes nothing",
  );
});

test("every fixture replays end to end without throwing, ring bound included", () => {
  for (const name of ["snapshot", "fire", "trace-burst", "noise", "reload", "deep-fail", "loud-fail"]) {
    const frames = readFixture(name);
    assert.ok(frames.length > 0, `${name} is not empty`);
    assert.doesNotThrow(() => replay(frames), `${name} folds clean`);
    // …and again against a board whose ring evicts constantly, so the drop path runs.
    assert.doesNotThrow(() => replay(frames, { logLines: 3 }), `${name} folds clean under a tiny ring`);
    const { board } = replay(frames);
    assert.equal(board.frames.total, frames.length);
    assert.equal(board.frames.folded + board.frames.ignored, frames.length);
  }
});

// --- AC6: no imports, no document, no clock ------------------------------------------

test("the module names no import, no document and no clock", () => {
  const source = readFileSync(join(HERE, "fold.mjs"), "utf8");
  // Strip comments, then string and template literals, so a *mention* in prose is not a
  // use. Done in that order: a `//` inside a string would otherwise eat the line.
  const code = source
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .replace(/\/\/[^\n]*/g, " ")
    .replace(/"(?:[^"\\\n]|\\.)*"/g, '""')
    .replace(/'(?:[^'\\\n]|\\.)*'/g, "''")
    .replace(/`(?:[^`\\]|\\.)*`/g, "``");
  for (const banned of ["import", "require(", "document", "window", "globalThis", "Date.now", "performance"]) {
    assert.ok(!code.includes(banned), `fold.mjs must not use \`${banned}\`:\n${code.split("\n").filter((l) => l.includes(banned)).join("\n")}`);
  }
});

test("the fold reads no clock and no document at runtime", () => {
  const frames = [...readFixture("snapshot"), ...readFixture("trace-burst"), ...readFixture("noise")];
  const trap = (what) => () => {
    throw new Error(`the fold reached for ${what}`);
  };
  let board = seed();
  let now = 5_000;
  for (const frame of frames) {
    // Armed around the `fold` call alone and restored in a `finally`: `node:test` reads the
    // clock for its own durations, so a suite-wide swap takes the runner down with it.
    const realDateNow = Date.now;
    const realPerfNow = performance.now;
    const hadDocument = "document" in globalThis;
    const realDocument = globalThis.document;
    Date.now = trap("Date.now");
    performance.now = trap("performance.now");
    globalThis.document = new Proxy(
      {},
      { get: trap("document"), set: trap("document"), has: trap("document") },
    );
    try {
      board = fold(board, frame, now);
      groups(board);
      queues(board);
    } finally {
      Date.now = realDateNow;
      performance.now = realPerfNow;
      if (hadDocument) globalThis.document = realDocument;
      else delete globalThis.document;
    }
    now += 100;
  }
  assert.equal(board.frames.total, frames.length);
});

test("the fold never writes through a frozen board", () => {
  // Module code is strict, so a stray write to a frozen object throws. Freezing *every*
  // intermediate board is what makes this a proof rather than a check of the first fold.
  const frames = [...readFixture("snapshot"), ...readFixture("fire"), ...readFixture("reload")];
  let board = deepFreeze(seed({ logLines: 4 }));
  let now = 0;
  for (const frame of frames) {
    board = deepFreeze(fold(board, deepFreeze(frame), now));
    now += 10;
  }
  assert.equal(board.frames.total, frames.length);
});

// --- the sanitizer -------------------------------------------------------------------

test("the sanitizer neutralises the capture's adversarial log lines", () => {
  const lines = readFixture("noise")
    .filter((f) => f.type === "log")
    .map((f) => f.line);
  // The capture's own service prints the set on purpose; if it stops, this fails loudly
  // rather than asserting over plain text.
  const find = (needle, what) => {
    const line = lines.find((l) => l.includes(needle));
    assert.ok(line, `the capture carries ${what}`);
    return line;
  };

  const sgr = find("[31m", "an SGR sequence");
  assert.equal(sanitize(sgr), sgr.replaceAll("[31m", "").replaceAll("[0m", ""));
  assert.ok(!sanitize(sgr).includes(""), "no escape survives");

  const osc = find("]0;", "an OSC residue");
  // An `ESC` that is not a CSI drops just the escape byte, leaving inert printable text.
  assert.ok(sanitize(osc).includes("]0;window title"), "the OSC payload is left as inert text");
  assert.ok(!sanitize(osc).includes("") && !sanitize(osc).includes(""), "…with no control bytes");

  const cr = find("carriage\rreturn", "an embedded carriage return");
  assert.ok(sanitize(cr).includes("carriagereturn"), "an embedded \\r is dropped in place");

  const tab = find("a\ttab", "tabs");
  assert.ok(sanitize(tab).includes("a tab separated line"), "a tab becomes one space");

  const bidi = find("‮", "a bidi override");
  assert.ok(!sanitize(bidi).includes("‮"), "the override is dropped");
  assert.ok(sanitize(bidi).includes("bidi override"), "…and the text around it survives");

  const zwsp = find("​", "a zero-width space");
  assert.ok(sanitize(zwsp).includes("zerowidth joiner"), "the zero-width character is dropped");

  const cjk = find("検査サービス", "wide CJK");
  assert.ok(sanitize(cjk).includes("検査サービス"), "wide glyphs pass through untouched");

  const emoji = find("🔥", "an emoji and a VS16 glyph");
  assert.ok(sanitize(emoji).includes("🔥"), "an astral character is never split");
  assert.ok(sanitize(emoji).includes("⚙️"), "…and a VS16 selector is kept, being Mn and not Cf");

  // Every line is idempotent under a second pass, and none of them grows.
  for (const line of lines) {
    assert.equal(sanitize(sanitize(line)), sanitize(line), `idempotent: ${JSON.stringify(line)}`);
    assert.ok(Array.from(sanitize(line)).length <= Array.from(line).length);
  }
});

test("the ring drops its oldest at the configured bound and counts the loss", () => {
  const frames = readFixture("noise");
  const logs = frames.filter((f) => f.type === "log");
  const subject = logs[0].service;
  const mine = logs.filter((f) => f.service === subject);
  assert.ok(mine.length > 4, `${subject} prints more than a tiny ring holds (${mine.length} lines)`);

  const { board } = replay(frames, { logLines: 3 });
  const ring = board.services[subject].log;
  assert.equal(ring.lines.length, 3, "the ring holds its bound");
  assert.equal(ring.dropped, mine.length - 3, "…and counts every line it dropped");
  assert.deepEqual(
    ring.lines.map((l) => l.text),
    mine.slice(-3).map((f) => sanitize(f.line)),
    "…keeping the newest, sanitized",
  );
  for (const line of ring.lines) assert.ok(["stdout", "stderr"].includes(line.stream), "each line keeps its stream");

  // The default is the log ring's own capacity, so nothing is dropped at this volume.
  const wide = replay(frames).board.services[subject].log;
  assert.equal(wide.dropped, 0);
  assert.equal(wide.lines.length, mine.length);
  assert.equal(seed().logLines, LOG_LINES_DEFAULT);
  assert.equal(TRACE_NODES_CAPACITY, 2000);
});

test("a log line moves the activity anchor only while a fire is in flight", () => {
  const frames = readFixture("noise");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const idle = frames.find(
    (f) => f.type === "log" && snapshot.services.find((s) => s.name === f.service)?.busy_ms === 0,
  );
  const busy = frames.find(
    (f) => f.type === "log" && snapshot.services.find((s) => s.name === f.service)?.busy_ms > 0,
  );
  assert.ok(idle && busy, "the capture has output from both a parked and a firing service");

  const board = fold(seed(), snapshot, 1_000);
  const before = { idle: board.services[idle.service].lastActivityAt, busy: board.services[busy.service].lastActivityAt };
  assert.equal(fold(board, idle, 9_000).services[idle.service].lastActivityAt, before.idle, "a parked service's stands");
  assert.equal(fold(board, busy, 9_000).services[busy.service].lastActivityAt, 9_000, "…a firing one's moves to now");
});

// --- the supervisor vocabulary -------------------------------------------------------

test("a mid-fire stop reads stopping until its paused settles it", () => {
  // The drain edge, from the capture's own `stop` on a service that was firing: a
  // `service_stopping`, the fire's own `fire_ok` (which must NOT lift it back to idle), and
  // the settling `service_paused {paused:true}`.
  const frames = readFixture("noise");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const stopping = frames.filter((f) => f.type === "event" && f.event === "service_stopping");
  const subject = stopping.map((f) => f.service).find((name) => snapshot.services.find((s) => s.name === name)?.busy_ms > 0);
  assert.ok(subject, "the capture stops a service that was mid-fire");

  const upTo = frames.indexOf(stopping.find((f) => f.service === subject));
  const { boards } = replay(frames);
  assert.equal(boards[upTo - 1].services[subject].badge, "Busy", "it was busy before the stop");
  assert.equal(boards[upTo].services[subject].badge, "Stopping", "…and the drain edge shows at once");

  // Its own completion lands while draining and leaves it draining.
  const okAt = frames.findIndex((f, i) => i > upTo && f.type === "event" && f.event === "fire_ok" && f.service === subject);
  assert.ok(okAt > upTo, "the drained fire finished after the stop");
  assert.equal(boards[okAt].services[subject].badge, "Stopping", "a completion never lifts a draining service");
  assert.equal(boards[okAt].services[subject].inFlightSince, null, "…though the fire clock does clear");

  const pausedAt = frames.findIndex(
    (f, i) => i > okAt && f.type === "event" && f.event === "service_paused" && f.service === subject && f.paused,
  );
  assert.ok(pausedAt > okAt, "…and the settle follows");
  assert.equal(boards[pausedAt].services[subject].badge, "Stopped");
});

test("a stop/start round trip re-arms, and a re-arm that faults crashes once per alarm", () => {
  const frames = readFixture("noise");
  const armed = frames.filter((f) => f.type === "event" && f.event === "service_armed");
  const crashed = frames.filter((f) => f.type === "event" && f.event === "service_crashed");
  assert.ok(armed.length >= 1, "the capture re-arms a service");
  assert.ok(crashed.length >= 2, "…and re-crashes another, twice (the daemon's repeat alarm)");
  assert.equal(new Set(crashed.map((f) => f.service)).size, 1, "…both alarms for the same service");

  const { boards } = replay(frames);
  const rearmed = armed[0].service;
  const armedAt = frames.indexOf(armed[0]);
  assert.equal(boards[armedAt].services[rearmed].badge, "Idle", "the arm settles it idle");
  assert.equal(boards[armedAt].services[rearmed].stale, false, "…clearing the staged-recipe mark");
  assert.equal(boards[armedAt].services[rearmed].crashCounted, false, "…and re-opening the crash count");
  // The resume that preceded it lands `Starting`, not `Idle`.
  const resumeAt = frames.findIndex(
    (f, i) => i < armedAt && f.type === "event" && f.event === "service_paused" && f.service === rearmed && !f.paused,
  );
  assert.ok(resumeAt > -1, "…and it was preceded by a resume");
  assert.equal(boards[resumeAt].services[rearmed].badge, "Starting");

  // The crash alarm counts once however many times the daemon repeats it.
  const victim = crashed[0].service;
  const lastCrashAt = frames.lastIndexOf(crashed[crashed.length - 1]);
  const svc = boards[lastCrashAt].services[victim];
  assert.equal(svc.badge, "Crashed");
  assert.equal(svc.errors, 1, `${crashed.length} alarms, one error`);
  assert.equal(svc.lastError, crashed[0].reason);
  assert.equal(svc.breadcrumb, crashed[0].reason);
});

test("the reclaim paused clears poison without re-stamping the dwell", () => {
  // The daemon sends a *second* `paused: true` on the force-abandon reclaim. Its badge move
  // is a no-op, but the poison clear is not — so folding the same recorded frame twice must
  // clear the flag and leave `stateEnteredAt` where the first one put it.
  const frames = readFixture("noise");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const paused = frames.find((f) => f.type === "event" && f.event === "service_paused" && f.paused === true);
  assert.ok(paused, "the capture carries a settling paused event");

  // Seed the service poisoned, the way a reconnect after a force-abandon does.
  const poisoned = copy(snapshot);
  poisoned.services = poisoned.services.map((s) => (s.name === paused.service ? { ...s, poisoned: true } : s));
  const board = fold(seed(), poisoned, 1_000);
  assert.equal(board.services[paused.service].poisoned, true, "the snapshot's poison bit is carried");

  const once = fold(board, paused, 2_000);
  const twice = fold(once, copy(paused), 3_000);
  assert.equal(once.services[paused.service].poisoned, false, "the reclaim clears the poison");
  assert.equal(twice.services[paused.service].poisoned, false, "…and stays clear on the repeat");
  assert.equal(once.services[paused.service].badge, "Stopped");
  assert.equal(
    twice.services[paused.service].stateEnteredAt,
    once.services[paused.service].stateEnteredAt,
    "an unchanged badge never re-stamps the dwell",
  );
});

test("an event naming a service the board never saw folds to nothing", () => {
  const frames = readFixture("fire");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const board = fold(seed(), snapshot, 1_000);
  for (const frame of frames.filter((f) => f.type === "event" || f.type === "log" || f.type === "trace")) {
    const stranger = { ...copy(frame), service: "nobody::here" };
    const after = fold(board, stranger, 2_000);
    assert.deepEqual(sansFrames(after), sansFrames(board), `${frame.event ?? frame.type} for an unknown service`);
    assert.equal(after.frames.folded, board.frames.folded + 1, "…understood, not ignored");
  }
});

// --- the drain and the reconcile -----------------------------------------------------

test("the drain metas move the daemon anchor and pull a card", () => {
  const frames = readFixture("reload");
  const quitting = frames.find((f) => f.type === "meta" && f.meta === "quitting");
  const dropped = frames.find((f) => f.type === "meta" && f.meta === "dropped");
  assert.ok(quitting, "the capture ends with the daemon draining");
  assert.ok(dropped, "…and an orphan's card pulled out of band");

  const { boards } = replay(frames);
  const quitAt = frames.indexOf(quitting);
  const before = boards[quitAt - 1];
  const after = boards[quitAt];
  assert.equal(before.quittingSince, null, "a running daemon has no drain anchor");
  assert.notEqual(after.quittingSince, null, "…and the broadcast stamps one");
  assert.deepEqual(after.services, before.services, "no service record moves on the drain edge");

  const dropAt = frames.indexOf(dropped);
  assert.ok(boards[dropAt - 1].services[dropped.service] !== undefined, `${dropped.service} was on the board`);
  assert.equal(boards[dropAt].services[dropped.service], undefined, "…and its card is pulled");
  assert.ok(!boards[dropAt].order.includes(dropped.service), "…out of the order too");
  const { [dropped.service]: _gone, ...rest } = boards[dropAt - 1].services;
  assert.deepEqual(boards[dropAt].services, rest, "and nothing else moved");
});

test("a reload applies its whole reconcile verdict", () => {
  const frames = readFixture("reload");
  const reloaded = frames.find((f) => f.type === "meta" && f.meta === "reloaded");
  assert.ok(reloaded, "the capture holds an applied reload");
  for (const key of ["added", "changed", "orphaned", "stale", "order"]) {
    assert.ok((reloaded[key] ?? []).length > 0, `…carrying a \`${key}\` verdict`);
  }

  const { boards } = replay(frames);
  const at = frames.indexOf(reloaded);
  const before = boards[at - 1];
  const after = boards[at];

  for (const entry of reloaded.added) {
    assert.equal(before.services[entry.name], undefined, `${entry.name} was not on the board`);
    const svc = after.services[entry.name];
    assert.ok(svc, `${entry.name} was added`);
    assert.equal(svc.triggerLabel, entry.trigger_label, "…with its full seed, not a blank husk");
    assert.equal(svc.icon, entry.icon);
    assert.ok(after.order.includes(entry.name));
  }
  for (const entry of reloaded.changed) {
    const was = before.services[entry.name];
    const now = after.services[entry.name];
    assert.equal(now.description, entry.description, "the fresh metadata lands");
    assert.notEqual(was.description, entry.description, "…and it really did drift");
    // …while the live state is untouched.
    assert.equal(now.badge, was.badge);
    assert.equal(now.stateEnteredAt, was.stateEnteredAt);
    assert.equal(now.inFlightSince, was.inFlightSince);
    assert.deepEqual(now.log, was.log);
    assert.deepEqual(now.tree, was.tree);
  }
  for (const name of reloaded.orphaned) {
    assert.equal(before.services[name].orphan, false);
    assert.equal(after.services[name].orphan, true, `${name} lights its orphan marker`);
    assert.equal(after.services[name].badge, before.services[name].badge, "…keeping its running badge");
  }
  for (const name of reloaded.stale) {
    assert.equal(before.services[name].stale, false);
    assert.equal(after.services[name].stale, true, `${name} lights its stale marker`);
  }
  // The daemon's post-reload order wins, with anything it does not name (a still-running
  // orphan) kept after it.
  assert.deepEqual(after.order.slice(0, reloaded.order.length), reloaded.order);
  for (const name of Object.keys(after.services)) assert.ok(after.order.includes(name), `${name} has a place`);
});

test("the host load strip keeps the latest sample and a bounded history", () => {
  const frames = readFixture("noise");
  const loads = frames.filter((f) => f.type === "meta" && f.meta === "host_load");
  assert.ok(loads.length >= 10, `the capture carries a real load series (${loads.length} samples)`);

  const { board, clock } = replay(frames);
  const last = loads[loads.length - 1];
  assert.equal(board.load.cpuPct, last.cpu_pct);
  assert.equal(board.load.memUsed, last.mem_used);
  assert.equal(board.load.memTotal, last.mem_total);
  assert.equal(board.load.rxBps, last.rx_bps);
  assert.equal(board.load.txBps, last.tx_bps);
  assert.equal(board.load.at, clock[frames.lastIndexOf(last)]);
  assert.equal(board.load.history.length, loads.length);
  assert.deepEqual(
    board.load.history.map((s) => s.cpuPct),
    loads.map((f) => f.cpu_pct),
    "every sample, in order",
  );
  // The bound is enforced by dropping the oldest, which one capture is too short to reach —
  // so drive it with the capture's own samples, replayed until it does.
  const many = [];
  while (many.length < 300) many.push(...loads);
  const long = replay(many).board;
  assert.equal(long.load.history.length, 120);
  assert.deepEqual(
    long.load.history.map((s) => s.cpuPct),
    many.slice(-120).map((f) => f.cpu_pct),
    "the newest 120 survive",
  );
});

// --- the selectors -------------------------------------------------------------------

test("the group rollup takes the latest activity and the oldest fire over its members", () => {
  const frames = readFixture("snapshot");
  const snapshot = frames.find((f) => f.type === "meta" && f.meta === "snapshot");
  const board = fold(seed(), snapshot, 1_000_000);
  const rolled = groups(board);

  const paths = new Set(snapshot.services.map((s) => s.group).filter((g) => g !== ""));
  assert.deepEqual(new Set(rolled.map((g) => g.group)), paths, "one bucket per group path on the board");
  assert.ok(rolled.some((g) => g.members.length >= 2), "…and at least one with several members");

  for (const bucket of rolled) {
    const members = snapshot.services.filter((s) => s.group === bucket.group);
    assert.deepEqual(bucket.members, members.map((s) => s.name), `${bucket.group}'s members, in board order`);
    assert.equal(bucket.anyCrashed, members.some((s) => s.state === "crashed"), `${bucket.group}'s crash verdict`);
    assert.equal(bucket.busy, members.filter((s) => s.busy_ms > 0).length, `${bucket.group}'s in-flight count`);
    assert.equal(bucket.runs, members.reduce((n, s) => n + s.runs_total, 0));
    assert.equal(bucket.failures, members.reduce((n, s) => n + s.run_failures_total, 0));
    // The *max* activity instant — the smallest `last_activity_ms`, i.e. the most recent —
    // and the *min* in-flight instant, i.e. the longest-running member.
    const ages = members.map((s) => s.last_activity_ms).filter((ms) => ms !== undefined);
    assert.equal(
      bucket.latestActivityAt,
      ages.length === 0 ? null : 1_000_000 - Math.min(...ages),
      `${bucket.group}'s newest activity`,
    );
    const fires = members.map((s) => s.busy_ms).filter((ms) => ms > 0);
    assert.equal(
      bucket.oldestInFlightSince,
      fires.length === 0 ? null : 1_000_000 - Math.max(...fires),
      `${bucket.group}'s longest-running fire`,
    );
  }

  // A max is only a claim if two members really do carry different ages — otherwise the
  // round-1 min/max inversion would pass here unnoticed.
  const spread = rolled.find((g) => g.members.length >= 2);
  const ages = snapshot.services
    .filter((s) => s.group === spread.group)
    .map((s) => s.last_activity_ms)
    .filter((ms) => ms !== undefined);
  assert.ok(new Set(ages).size >= 2, `${spread.group}'s members carry different activity ages (${ages})`);
  assert.ok(rolled.some((g) => g.anyCrashed), "…and a crashed member is really in a group");
});

test("a nested group does not count toward its parent — the stated flat-bucketing divergence", () => {
  // `afkd top`'s own rollup is transitive (`in_subtree`), and this one is not: it buckets by
  // the exact `group` string, because there is no tree to collapse into until the card that
  // paints one. Pinned rather than left to be discovered.
  const snapshot = readFixture("snapshot").find((f) => f.type === "meta" && f.meta === "snapshot");
  const nested = snapshot.services.find((s) => s.group.includes("::"));
  assert.ok(nested, "the capture has a two-level namespace");
  const parent = nested.group.slice(0, nested.group.indexOf("::"));
  assert.ok(
    snapshot.services.some((s) => s.group === parent),
    `…whose parent \`${parent}\` is itself a group on the board`,
  );

  const rolled = groups(fold(seed(), snapshot, 1_000));
  const parentBucket = rolled.find((g) => g.group === parent);
  assert.ok(!parentBucket.members.includes(nested.name), `${nested.name} is not a member of ${parent}`);
  assert.deepEqual(rolled.find((g) => g.group === nested.group).members, [nested.name]);
});

test("the queue lanes read held, waiting and the max parallelism", () => {
  const snapshot = readFixture("snapshot").find((f) => f.type === "meta" && f.meta === "snapshot");
  const onLanes = snapshot.services.filter((s) => s.queue !== "");
  assert.ok(onLanes.length >= 3, `the capture puts several services on a lane (${onLanes.length})`);
  assert.ok(onLanes.some((s) => s.state === "busy"), "…one holding a slot");
  assert.ok(onLanes.some((s) => s.state === "queued"), "…one waiting at the door");
  assert.ok(new Set(onLanes.map((s) => s.queue_priority)).size >= 2, "…at more than one level");

  const lanes = queues(fold(seed(), snapshot, 1_000));
  assert.deepEqual(
    lanes.map((l) => l.lane),
    [...new Set(onLanes.map((s) => s.queue))].sort(),
    "one lane per named queue, sorted by name",
  );
  for (const lane of lanes) {
    const members = onLanes.filter((s) => s.queue === lane.lane);
    assert.deepEqual(lane.members, members.map((s) => s.name));
    assert.equal(lane.held, members.filter((s) => s.state === "busy" || s.state === "checking").length, "held");
    assert.equal(lane.waiting, members.filter((s) => s.state === "queued").length, "waiting");
    const widths = members.map((s) => s.queue_parallelism).filter((n) => n !== undefined);
    assert.equal(lane.parallelism, widths.length === 0 ? null : Math.max(...widths), "the max over members");
    // The level is per-service metadata, so the lane takes it from the first member that
    // reports one — and the lane's own members really do disagree, which is the point.
    assert.equal(lane.priority, members.find((s) => s.queue_priority !== "")?.queue_priority ?? "");
  }
  const heavy = lanes.find((l) => l.held > 0);
  assert.ok(heavy && heavy.waiting > 0, "a lane reads both its holders and its waiters");
});

test("a lane-less service is in no lane, and an ungrouped one is in no group", () => {
  const snapshot = readFixture("snapshot").find((f) => f.type === "meta" && f.meta === "snapshot");
  const bare = snapshot.services.filter((s) => s.group === "" && s.queue === "");
  assert.ok(bare.length >= 2, "the capture has bare, top-level services");

  const board = fold(seed(), snapshot, 1_000);
  const named = new Set(groups(board).flatMap((g) => g.members));
  const laned = new Set(queues(board).flatMap((l) => l.members));
  for (const svc of bare) {
    assert.ok(!named.has(svc.name), `${svc.name} is a top-level row, not a group member`);
    assert.ok(!laned.has(svc.name), `…and on no lane`);
  }
});

test("the run tree evicts a whole completed root at a time, and never the live run", () => {
  // The node bound. No single capture reaches 2000 nodes — and one that did would be a
  // 200 KB fixture riding into every operator's plugin dir — so the burst is replayed past
  // the bound with its ids shifted a generation at a time, which is exactly what a service
  // that keeps firing produces. The *shape* of every run is the recording's.
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const traces = traceFrames(frames, subject);
  const perRun = traces.filter((f) => f.event.op === "opened").length;
  assert.ok(perRun > 1, `a run is several nodes (${perRun})`);

  /** The burst again, every id shifted by `offset` — one more fire of the same workflow. */
  const generation = (offset) =>
    traces.map((f) => {
      const shifted = copy(f);
      if (shifted.event.op === "opened") {
        shifted.event.node.id += offset;
        if (shifted.event.node.parent !== undefined) shifted.event.node.parent += offset;
      } else {
        shifted.event.id += offset;
      }
      return shifted;
    });

  const runs = Math.ceil(TRACE_NODES_CAPACITY / perRun) + 3;
  // The capture's own snapshot first, so the service is on the board to fold onto.
  const all = [frames.find((f) => f.type === "meta" && f.meta === "snapshot")];
  for (let i = 0; i < runs; i += 1) all.push(...generation(i * perRun * 10));
  const tree = replay(all).board.services[subject].tree;

  assert.ok(Object.keys(tree.nodes).length <= TRACE_NODES_CAPACITY, "the bound holds");
  assert.ok(Object.keys(tree.nodes).length > TRACE_NODES_CAPACITY - perRun * 2, "…without over-evicting");
  assert.equal(Object.keys(tree.nodes).length % perRun, 0, "whole runs, never a half-evicted one");
  // The oldest roots went first, and the newest fire is intact.
  assert.equal(tree.roots[tree.roots.length - 1], (runs - 1) * perRun * 10 + 1);
  assert.ok(tree.roots[0] > 1, "the first fire's root was reclaimed");
  for (const root of tree.roots) {
    // Every retained root still carries its whole subtree.
    const stack = [root];
    let n = 0;
    while (stack.length > 0) {
      const node = tree.nodes[stack.pop()];
      assert.ok(node, `a retained root's subtree is whole under ${root}`);
      n += 1;
      stack.push(...node.children);
    }
    assert.equal(n, perRun, `run ${root} is whole`);
  }

  // …and a run still open is never the one reclaimed, even past the bound.
  const live = generation(runs * perRun * 10).filter((f) => f.event.op === "opened");
  const withLive = replay([...all, ...live]).board.services[subject].tree;
  const liveRoot = runs * perRun * 10 + 1;
  assert.ok(withLive.roots.includes(liveRoot), "the live run's root is retained");
  assert.equal(withLive.nodes[liveRoot].status, "running");
});

test("a node's openedAt is the instant its frame folded", () => {
  // The monotonic anchor the run view's ticking `TOOK` is measured against. The wire carries a
  // **civil** stamp (`at`) and no monotonic one, and the browser has no civil clock worth
  // subtracting against, so the fold stamps its own `now` beside it — which only works if the
  // instant it stamps really is the instant the frame arrived.
  const frames = readFixture("trace-burst");
  const subject = frames.find((f) => f.type === "trace").service;
  const { boards, clock } = replay(frames);
  const opens = frames
    .map((frame, i) => ({ frame, i }))
    .filter(({ frame }) => frame.type === "trace" && frame.service === subject && frame.event.op === "opened");
  assert.ok(opens.length >= 5, "the capture opens several nodes at distinct instants");
  for (const { frame, i } of opens) {
    const node = boards[i].services[subject].tree.nodes[frame.event.node.id];
    assert.equal(node.openedAt, clock[i], `node ${frame.event.node.id} is stamped where it landed`);
    // …and the civil stamp is still carried verbatim beside it: the substitution is the run
    // view's, not the fold's, so nothing downstream loses the daemon's own instant.
    assert.deepEqual(node.at, frame.event.at, "the wire's civil stamp survives untouched");
  }
  // A `closed` does not re-stamp it: the anchor is the moment the node *opened*, and a close
  // carries the authoritative `elapsed_ms` the rail reads instead.
  const { board } = replay(frames);
  const root = board.services[subject].tree.nodes[opens[0].frame.event.node.id];
  assert.equal(root.openedAt, clock[opens[0].i], "a closed node still names when it opened");
  assert.ok(root.elapsedMs > 0, "…and carries the engine's own span for the rail");
});
