// The session's own suite: `node --test @afkd/web-top/session.test.mjs`.
//
// Every board here is **folded from a recorded capture** under `fixtures/` at a fixed synthetic
// clock — the rule `fixtures/README.md` states, and the reason a re-capture cannot quietly make
// an assertion vacuous. Nothing hand-writes a wire frame, with one stated exception noted at its
// own test: `Meta::Error` appears in no capture (the recorder refuses to configure a provider
// trigger, which is how you earn one), so that arm is driven by a frame built from `proto.rs`'s
// shape and is called out as the hand-built frame it is.
//
// The board is folded from real captures for a second reason too: the gates this file asserts
// are badge predicates, and a hand-made board would let the test pick badges that make its own
// assertions pass.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import { fold, seed } from "./fold.mjs";
import { rowKey, runMetrics, scrollOffset, visibleRows as visibleRowsOf } from "./layout.mjs";
import {
  FLASH_TIMEOUT,
  flashOf,
  needleOf,
  newSession as bootSession,
  noteFrame,
  press,
  rowsOf,
  scrollInfo,
  selectedIndex,
  typingOf,
} from "./session.mjs";

const HERE = import.meta.dirname;

// Most of these suites drive the grouped tree, the one view the page used to have, so their
// sessions start grouped; the flat boot view's own tests start from `bootSession()`.
const newSession = () => ({ ...bootSession(), grouped: true });
const visibleRows = (board, options = {}) => visibleRowsOf(board, { grouped: true, ...options });

/// A synthetic clock, as `layout.test.mjs` keeps one: every anchor the fold computes is pure
/// arithmetic and identical across runs.
const BASE = 1_000_000;
const STEP = 10;

/**
 * Fold `file`'s frames into a board, stopping **before** the frame `until` answers true for (or
 * after `lines` of them). Returns the board and the instant the last folded frame landed at.
 */
function foldCapture(file, { until, lines } = {}) {
  let board = seed({ logLines: 2000 });
  let at = BASE;
  let n = 0;
  for (const line of readFileSync(join(HERE, "fixtures", file), "utf8").split("\n")) {
    if (line === "") continue;
    const frame = JSON.parse(line);
    if (until !== undefined && until(frame)) break;
    board = fold(board, frame, (at += STEP));
    n += 1;
    if (lines !== undefined && n >= lines) break;
  }
  return { board, at };
}

/// The capture's **live** board: every frame up to the `SIGINT` the recorder stayed attached
/// through. Every capture ends with that drain.
function liveBoard(file = "snapshot.jsonl") {
  return foldCapture(file, { until: (f) => f.type === "meta" && f.meta === "quitting" });
}

/// The same capture folded whole, drain included.
function drainedBoard(file = "snapshot.jsonl") {
  return foldCapture(file);
}

/// A chord, spelled the way `keymap.mjs` spells one.
function key(k) {
  return { ctrl: false, key: k };
}
function ctrl(k) {
  return { ctrl: true, key: k };
}

/// Press a run of chords in order, returning the last session and every command any of them
/// emitted — the shape most of these tests want.
function type(session, board, chords, at = BASE, height = 20) {
  let commands = [];
  let s = session;
  for (const chord of chords) {
    const out = press(s, board, chord, { now: at, bodyHeight: height });
    s = out.session;
    commands = commands.concat(out.commands);
  }
  return { session: s, commands };
}

/// The identity of the row the cursor sits on — what most navigation assertions read.
function cursorOn(session, board) {
  const rows = rowsOf(session, board);
  const at = selectedIndex(session, rows);
  return at === null ? null : rowKey(rows[at]);
}

// --- navigation over a real board ------------------------------------------------------

test("j and k walk every selectable row, in the sequence that paints", () => {
  const { board } = liveBoard();
  const rows = rowsOf(newSession(), board);
  // The capture's own sequence: ten services, two group headers, the section's spacer and
  // header, and one lane. The two unselectable rows are what make this non-trivial.
  const selectable = rows.map((r, i) => [rowKey(r), i]).filter(([k]) => k !== null);
  assert.equal(rows.length, 15, "the capture still lays out fifteen rows");
  assert.equal(selectable.length, 13, "…thirteen of which take the cursor");
  assert.deepEqual(
    rows.filter((r) => rowKey(r) === null).map((r) => r.kind),
    ["spacer", "queuesHeader"],
    "the spacer and the Queues header are the two that do not",
  );

  // `j` off the top walks every one of them, in order, skipping both unselectable rows with no
  // special case — which is the whole reason the cursor is a row identity.
  let session = newSession();
  const walked = [cursorOn(session, board)];
  for (let i = 1; i < selectable.length; i += 1) {
    session = press(session, board, key("j"), { now: BASE, bodyHeight: 20 }).session;
    walked.push(cursorOn(session, board));
  }
  assert.deepEqual(walked, selectable.map(([k]) => k), "j walks the selectable rows in paint order");
  // The last row is a lane, so the walk really crossed the `Queues` seam.
  assert.deepEqual(walked[walked.length - 1], { kind: "queue", key: "heavy" });

  // …and clamps rather than wrapping.
  const past = press(session, board, key("j"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual(cursorOn(past.session, board), walked[walked.length - 1], "j at the foot stays");
  assert.equal(past.handled, true, "…and is still the page's key, not the browser's");

  // `k` walks back over the same sequence.
  const back = [];
  let up = past.session;
  for (let i = 0; i < selectable.length - 1; i += 1) {
    up = press(up, board, key("k"), { now: BASE, bodyHeight: 20 }).session;
    back.push(cursorOn(up, board));
  }
  assert.deepEqual(back, walked.slice(0, -1).reverse(), "k walks back");
  assert.deepEqual(cursorOn(press(up, board, key("k"), { now: BASE, bodyHeight: 20 }).session, board), walked[0]);

  // The arrows reach the same actions — the alternates the table binds, which nothing on
  // screen advertises and a transcription is free to lose.
  const byArrow = type(newSession(), board, [key("down"), key("down")]).session;
  const byLetter = type(newSession(), board, [key("j"), key("j")]).session;
  assert.deepEqual(cursorOn(byArrow, board), cursorOn(byLetter, board), "↓ is j");
  assert.deepEqual(
    cursorOn(press(byArrow, board, key("up"), { now: BASE, bodyHeight: 20 }).session, board),
    cursorOn(press(byLetter, board, key("k"), { now: BASE, bodyHeight: 20 }).session, board),
    "↑ is k",
  );
});

test("g and G land on the first and last selectable rows", () => {
  const { board } = liveBoard();
  const rows = rowsOf(newSession(), board);
  const keys = rows.map(rowKey).filter((k) => k !== null);
  const last = type(newSession(), board, [key("G")]).session;
  assert.deepEqual(cursorOn(last, board), keys[keys.length - 1]);
  const first = type(last, board, [key("g")]).session;
  assert.deepEqual(cursorOn(first, board), keys[0]);
  // `G` lands on a **selectable** row: the last row of this capture is a lane, but the settle
  // is `nearest_selectable` searching *upward* from the foot, so a board whose last row were
  // the `Queues` header would still land on something.
  assert.notEqual(rowKey(rows[rows.length - 1]), null);
});

test("the cursor is a row identity, so an ambient fold does not slide it", () => {
  // The property `model::RowKey` exists for. `noise.jsonl` drops no service, so the drift is
  // driven the only other way a row set moves under a still cursor: the filter.
  const { board } = liveBoard();
  const onDeep = type(newSession(), board, [key("G"), key("k"), key("k")]).session;
  const before = cursorOn(onDeep, board);
  assert.equal(before.kind, "service");
  // A needle that keeps the row: the cursor stays on the *same service*, not on the same index.
  const filtered = { ...onDeep, filter: { mode: "active", needle: before.key } };
  assert.deepEqual(cursorOn(filtered, board), before, "the keyed row is found wherever it moved");
  assert.notEqual(
    rowsOf(filtered, board).length,
    rowsOf(onDeep, board).length,
    "…and it really did move: the row set is a different length",
  );
  // A needle that drops it: the cursor falls back to the nearest selectable row at the index it
  // was last seen at, rather than to nothing.
  const gone = { ...onDeep, filter: { mode: "active", needle: "janitor" } };
  const landed = cursorOn(gone, board);
  assert.notEqual(landed, null, "a vanished row settles rather than unselecting");
  assert.notDeepEqual(landed, before);
});

// --- the folds --------------------------------------------------------------------------

test("h folds a member's parent and moves the cursor onto it; l unfolds", () => {
  // Driven on the two-level `ops::db` nesting, so a fold of a fold is exercised rather than one
  // flat header.
  const { board } = liveBoard();
  const onVacuum = { ...newSession(), cursor: { kind: "service", key: "ops::db::vacuum" }, cursorRow: 6 };
  assert.deepEqual(cursorOn(onVacuum, board), { kind: "service", key: "ops::db::vacuum" });

  const folded = press(onVacuum, board, key("h"), { now: BASE, bodyHeight: 20 }).session;
  assert.deepEqual([...folded.collapsed], ["ops::db"], "h collapses the member's own parent");
  assert.deepEqual(cursorOn(folded, board), { kind: "group", key: "ops::db" }, "…and lands on it");
  const names = rowsOf(folded, board).map((r) => rowKey(r)?.key ?? "");
  assert.ok(!names.includes("ops::db::vacuum"), "the member is gone from the row set");
  assert.ok(names.includes("ops::db"), "its header is not");
  // The header wears the collapsed chevron, which is the only thing on screen saying so.
  assert.equal(rowsOf(folded, board).find((r) => r.kind === "group" && r.path === "ops::db").collapsed, true);

  // `h` again, now on the header, folds the *grandparent*, and the cursor follows there too.
  const outer = press(folded, board, key("h"), { now: BASE, bodyHeight: 20 }).session;
  assert.deepEqual([...outer.collapsed].sort(), ["ops::db"], "a header folds itself, already folded");
  // `l` on the folded header unfolds it again, and the member comes back.
  const open = press(folded, board, key("l"), { now: BASE, bodyHeight: 20 }).session;
  assert.deepEqual([...open.collapsed], []);
  assert.ok(rowsOf(open, board).some((r) => rowKey(r)?.key === "ops::db::vacuum"));
  // `l` from a **member** of an already-open group is not a silent no-op: it moves the cursor
  // onto that group's header, which is `expand_selected_group`'s own stated behaviour.
  const lifted = press(onVacuum, board, key("l"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual([...lifted.session.collapsed], [], "nothing folds");
  assert.deepEqual(cursorOn(lifted.session, board), { kind: "group", key: "ops::db" }, "…the cursor lifts");
  // `←`/`→` are the same two actions.
  assert.deepEqual(
    [...press(onVacuum, board, key("left"), { now: BASE, bodyHeight: 20 }).session.collapsed],
    ["ops::db"],
  );
});

test("Enter folds a group header and opens a service row's peek", () => {
  const { board } = liveBoard();
  const onOps = { ...newSession(), cursor: { kind: "group", key: "ops" }, cursorRow: 1 };
  const shut = press(onOps, board, key("enter"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual([...shut.session.collapsed], ["ops"]);
  assert.equal(shut.handled, true);
  // Everything under `ops` is gone; the root peers are not.
  const names = rowsOf(shut.session, board).map((r) => rowKey(r)?.key ?? "");
  assert.ok(!names.some((n) => n.startsWith("ops::")), "the whole subtree folds, not one level");
  assert.ok(names.includes("janitor") && names.includes("ops"), "the peers and the header stay");
  // `Space` is the alternate, and toggles back.
  const open = press(shut.session, board, key("space"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual([...open.session.collapsed], []);
  // On a service row it opens that service's activity peek, and a second press closes it —
  // `toggle_selected_collapse`'s other arm — with the fold set and the cursor untouched.
  const onJanitor = newSession();
  const peek = press(onJanitor, board, key("enter"), { now: BASE, bodyHeight: 20 });
  assert.equal(peek.handled, true, "Enter on a service row is this page's key");
  assert.deepEqual([...peek.session.peeked], ["janitor"], "…and it opens that service's peek");
  assert.deepEqual(peek.session.collapsed, onJanitor.collapsed, "the fold set is not what moved");
  assert.deepEqual(peek.commands, [], "a peek is a local view and posts nothing");
  const closed = press(peek.session, board, key("space"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual([...closed.session.peeked], [], "the alternate closes it again");
});

test("the board boots flat, and v flips it without losing the cursor", () => {
  // `toggle_view`: the flag and nothing else. A selected service keeps the selection in both
  // views, and a selected header lands on its first member flat and finds itself again grouped.
  const { board } = liveBoard();
  const boot = bootSession();
  assert.equal(boot.grouped, false, "the boot view is flat");
  assert.ok(rowsOf(boot, board).every((r) => r.kind !== "group"), "…with no header in it");
  const onOps = { ...boot, grouped: true, cursor: { kind: "group", key: "ops" }, cursorRow: 1 };
  const flat = press(onOps, board, key("v"), { now: BASE, bodyHeight: 20 });
  assert.equal(flat.handled, true);
  assert.equal(flat.session.grouped, false);
  assert.deepEqual(cursorOn(flat.session, board), { kind: "service", key: "ops::nightly" }, "the header's first member");
  const back = press(flat.session, board, key("v"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual(cursorOn(back.session, board), { kind: "group", key: "ops" }, "…and the header again");
  assert.deepEqual(back.session.collapsed, onOps.collapsed, "the fold set rode the round trip untouched");
  // Flat view has no headers, so the four fold keys are inert there rather than rewriting a set
  // nothing on screen reflects.
  for (const chord of [key("h"), key("l"), key("H"), key("L")]) {
    const out = press(flat.session, board, chord, { now: BASE, bodyHeight: 20 });
    assert.equal(out.handled, false, `${chord.key} is inert in flat view`);
    assert.deepEqual(out.session.collapsed, flat.session.collapsed);
  }
});

test("b lenses the board, and the cursor finds its row again when it lifts", () => {
  // `FilterBusyToggle`: state-only, touching neither the needle nor the fold set. A cursor the
  // lens hides falls back by position and keeps its key, so clearing the lens puts it back.
  const { board } = liveBoard();
  const onIdle = { ...bootSession(), cursor: { kind: "service", key: "archivist" }, cursorRow: 5 };
  const lensed = press(onIdle, board, key("b"), { now: BASE, bodyHeight: 20 });
  assert.equal(lensed.handled, true);
  assert.equal(lensed.session.busyOnly, true);
  assert.ok(rowsOf(lensed.session, board).filter((r) => r.kind === "service").every((r) => ["Starting", "Busy", "Stopping", "Crashed"].includes(r.svc.badge)));
  assert.deepEqual(lensed.session.filter, onIdle.filter, "the needle is its own axis");
  const lifted = press(lensed.session, board, key("b"), { now: BASE, bodyHeight: 20 });
  assert.equal(lifted.session.busyOnly, false);
  assert.deepEqual(cursorOn(lifted.session, board), { kind: "service", key: "archivist" }, "the cursor is back on its row");
});

test("H folds every group and L opens them all", () => {
  const { board } = liveBoard();
  const all = type(newSession(), board, [key("G"), key("H")]).session;
  assert.deepEqual([...all.collapsed].sort(), ["ops", "ops::db"], "every group path, nested included");
  const names = rowsOf(all, board).map((r) => rowKey(r)?.key ?? "");
  assert.ok(!names.some((n) => n.includes("::")), "no member of any group survives");
  // The cursor was on the last row (a lane), which H did not hide, so it stays there.
  assert.deepEqual(cursorOn(all, board), { kind: "queue", key: "heavy" });
  // A cursor H *does* hide settles onto a row that still exists rather than onto nothing.
  const inside = { ...newSession(), cursor: { kind: "service", key: "ops::db::vacuum" }, cursorRow: 6 };
  const hidden = press(inside, board, key("H"), { now: BASE, bodyHeight: 20 }).session;
  assert.notEqual(cursorOn(hidden, board), null);
  assert.ok(rowsOf(hidden, board).some((r) => {
    const k = rowKey(r);
    return k !== null && k.kind === cursorOn(hidden, board).kind && k.key === cursorOn(hidden, board).key;
  }), "the settled cursor names a row that is on screen");

  // The bulk twins do **not** rewrite the cursor's key: a member `H` folded away resolves
  // straight back onto itself once `L` re-opens its header. That is the whole reason the
  // cursor is a row identity rather than an index, so it is asserted over the round trip.
  const onMember = { ...newSession(), cursor: { kind: "service", key: "ops::db::vacuum" }, cursorRow: 6 };
  const roundTrip = type(onMember, board, [key("H"), key("L")]).session;
  assert.deepEqual([...roundTrip.collapsed], []);
  assert.deepEqual(
    cursorOn(roundTrip, board),
    { kind: "service", key: "ops::db::vacuum" },
    "H then L leaves the cursor exactly where it was",
  );

  const opened = press(all, board, key("L"), { now: BASE, bodyHeight: 20 }).session;
  assert.deepEqual([...opened.collapsed], []);
  assert.deepEqual(
    rowsOf(opened, board).map((r) => rowKey(r)),
    rowsOf(newSession(), board).map((r) => rowKey(r)),
    "L restores the boot row set exactly",
  );
});

// --- AC2: the footer's gating follows the cursor ------------------------------------------

test("the footer's gated hints track the selected row's own badge", () => {
  // Derived from the **badge**, not from the footer builder, so the assertion is not the
  // builder restated. Driven over every selectable row of two boards.
  const { liveRows, lay } = (() => {
    const { board, at } = liveBoard();
    return { liveRows: board, lay: at };
  })();
  const cases = [
    { board: liveRows, at: lay, name: "live" },
    { board: drainedBoard().board, at: drainedBoard().at, name: "drained" },
  ];
  for (const { board, at, name } of cases) {
    const rows = visibleRows(board, {});
    const quitting = board.quittingSince !== null;
    rows.forEach((row, i) => {
      if (rowKey(row) === null) return;
      const footer = footerTextAt(board, at, i);
      // The four operator cells, and the predicate each one's live/dim bit must equal.
      const members =
        row.kind === "service"
          ? [row.svc]
          : row.kind === "group"
            ? board.order.map((n) => board.services[n]).filter((s) => s !== undefined && inSubtree(s.group, row.path))
            : [];
      const some = (f) => members.some(f);
      const wedged = some((m) => m.badge === "Stopping");
      const want = {
        start: some((m) => ["Stopped", "Crashed"].includes(m.badge) && !m.poisoned),
        [wedged ? "force-stop" : "stop"]: wedged || some((m) => canStop(m.badge)),
        trigger: some((m) => m.badge === "Idle"),
        restart: some((m) => !m.poisoned && m.badge !== "Stopping"),
      };
      for (const [label, live] of Object.entries(want)) {
        if (row.kind === "queue") continue; // a lane row carries no operator cell at all
        const hit = footer.find((h) => h.label === label);
        if (quitting) {
          // Every operator verb is refused during the drain, so the cell is **absent** — not
          // dim. That is the unbound/gated distinction, and it is the one a footer may not blur.
          assert.equal(hit, undefined, `${name} row ${i}: ${label} is unbound while draining`);
          continue;
        }
        assert.notEqual(hit, undefined, `${name} row ${i}: the footer carries ${label}`);
        assert.equal(hit.live, live, `${name} row ${i} (${rowKey(row).key}): ${label} is ${live ? "live" : "gated"}`);
      }
    });
  }
});

/// The footer's key hints at one cursor position, as `{keys, label, live}` — read off the
/// rendered screen's own cells, so the gate observed is the gate painted.
function footerTextAt(board, at, selected) {
  const { layout } = layoutModule;
  const rows = layout(board, { cols: 140, rows: 30, now: at + 1000, version: "0.2.123", selected, grouped: true });
  const out = [];
  for (const row of rows.slice(-4)) {
    // A hint is a bright key cell, a space, then a legend label cell — the two `hintRowCells`
    // mints from one hint, which must agree on the dim bit.
    for (let i = 0; i + 2 < row.length; i += 1) {
      if (row[i].fg !== "bright" || row[i + 1].text !== " " || row[i + 2].fg !== "legend") continue;
      if (row[i].text.trim() === "") continue;
      out.push({ keys: row[i].text, label: row[i + 2].text, live: !row[i + 2].dim });
    }
  }
  return out;
}

const layoutModule = await import("./layout.mjs");

function canStop(badge) {
  return ["Starting", "Idle", "Queued", "Checking", "Busy"].includes(badge);
}
function inSubtree(group, path) {
  return group === path || group.startsWith(`${path}::`);
}

// --- AC3: a verb is posted, and the board does not move ------------------------------------

test("t on an armed service fires, and the row moves only when the daemon's event lands", () => {
  // `fire.jsonl` folded to just before its first `fire_started`, so the service really is armed
  // and the frame that moves it is the very next one in the capture.
  const file = "fire.jsonl";
  const { board, at } = foldCapture(file, {
    until: (f) => f.type === "event" && f.event === "fire_started",
  });
  const name = "ops::nightly";
  assert.equal(board.services[name].badge, "Idle", "the capture's target is armed before the fire");

  const session = { ...newSession(), cursor: { kind: "service", key: name }, cursorRow: 2 };
  const out = press(session, board, key("t"), { now: at, bodyHeight: 20 });
  assert.deepEqual(out.commands, [{ command: "fire", service: name }], "exactly one fire, on this row");
  assert.equal(out.handled, true);
  // The board did **not** move: no optimistic edge, no badge touched. That is AC3's premise.
  assert.equal(board.services[name].badge, "Idle", "the press folded nothing");
  // …and the ack is what tells the operator anything happened, since the row did not.
  assert.equal(flashOf(out.session, at), `Fired ${name}`);

  // Now fold the daemon's answering frame, and the row moves.
  const started = { type: "event", event: "fire_started", service: name };
  const moved = fold(board, started, at + STEP);
  assert.equal(moved.services[name].badge, "Busy", "the event is what moves it");

  // `t` on a row that is not armed emits nothing and flashes nothing — a dim hint is already
  // the refusal, and a second sentence saying so would be noise.
  const unarmed = Object.values(board.services).find((s) => s.badge !== "Idle");
  assert.notEqual(unarmed, undefined, "the capture holds a non-armed service to try it on");
  assert.equal(unarmed.badge, "Crashed", "…its crashed one, which `can_fire` refuses");
  const gated = press(
    { ...newSession(), cursor: { kind: "service", key: unarmed.name }, cursorRow: 0 },
    board,
    key("t"),
    { now: at, bodyHeight: 20 },
  );
  assert.deepEqual(gated.commands, []);
  assert.equal(flashOf(gated.session, at), null, "a gated press acknowledges nothing");
});

// --- AC4: the force modal ------------------------------------------------------------------

/// The board `noise.jsonl` folds to at its first `service_stopping` — the **only** fixture that
/// carries one before the drain (its lines 25 and 38, against `meta.quitting` at 64).
/// `fire.jsonl`'s nine all follow its own `quitting`, where every verb is refused, so a test
/// written on it would assert a drain refusal and call it a modal.
function stoppingBoard() {
  return foldCapture("noise.jsonl", { lines: 25 });
}

test("x on a wedged service raises the force modal and sends nothing", () => {
  const { board, at } = stoppingBoard();
  assert.equal(board.services["janitor"].badge, "Stopping", "the capture's own wedged service");
  assert.equal(board.quittingSince, null, "…and the daemon is not draining, so nothing is refused");

  const onJanitor = newSession();
  assert.deepEqual(cursorOn(onJanitor, board), { kind: "service", key: "janitor" });
  const raised = press(onJanitor, board, key("x"), { now: at, bodyHeight: 20 });
  assert.deepEqual(raised.commands, [], "the modal sends nothing");
  assert.deepEqual(raised.session.confirm, { verb: "force", targets: ["janitor"], skipped: 0 });
  assert.equal(flashOf(raised.session, at), null, "the modal is the acknowledgement");

  // `n` and `Esc` both cancel, and both send nothing.
  for (const chord of [key("n"), key("esc")]) {
    const cancelled = press(raised.session, board, chord, { now: at, bodyHeight: 20 });
    assert.equal(cancelled.session.confirm, null, `${chord.key} closes the modal`);
    assert.deepEqual(cancelled.commands, [], `${chord.key} sends nothing`);
    assert.equal(cancelled.handled, true, "…and is swallowed rather than left to the browser");
  }

  // `y` sends exactly one force — asserted on the array's length, so a double-send fails — and
  // flashes nothing: the modal already acknowledged it.
  const confirmed = press(raised.session, board, key("y"), { now: at, bodyHeight: 20 });
  assert.equal(confirmed.commands.length, 1, "one force, not two");
  assert.deepEqual(confirmed.commands, [{ command: "force", service: "janitor" }]);
  assert.equal(confirmed.session.confirm, null, "…and the modal closes");
  assert.equal(flashOf(confirmed.session, at), null, "force posts no ack of its own");

  // Every other key while the modal is up is swallowed and emits nothing. A stray key must
  // never confirm a force, so the sweep is over the whole vocabulary rather than two samples.
  for (const chord of [key("j"), key("t"), key("s"), key("q"), key("?"), key("enter"), key("space"), ctrl("r"), key("G")]) {
    const stray = press(raised.session, board, chord, { now: at, bodyHeight: 20 });
    assert.deepEqual(stray.commands, [], `${chord.ctrl ? "Ctrl+" : ""}${chord.key} sends nothing`);
    assert.deepEqual(stray.session.confirm, raised.session.confirm, "…and the modal stays up");
    assert.equal(stray.handled, true, "…captured, so it cannot leak to the surface beneath");
  }
});

test("a group verb over more than one member gates, and one member acts directly", () => {
  // The fan-out gate (ADR-0026). The footer paints `s`/`x`/`t`/`r` live on a group row whenever
  // one member can take them, so leaving them inert would make a live hint dead — and handling
  // them without the gate would restart four services on one keypress.
  const { board, at } = liveBoard();
  const onOps = { ...newSession(), cursor: { kind: "group", key: "ops" }, cursorRow: 1 };
  const raised = press(onOps, board, key("r"), { now: at, bodyHeight: 20 });
  assert.deepEqual(raised.commands, [], "a fan-out sends nothing until it is confirmed");
  assert.equal(raised.session.confirm.verb, "restart");
  assert.ok(raised.session.confirm.targets.length > 1, "…over the subtree, transitively");
  assert.ok(
    raised.session.confirm.targets.includes("ops::db::vacuum"),
    "including the member two levels down — the transitive reading ADR-0073 uses",
  );
  const confirmed = press(raised.session, board, key("y"), { now: at, bodyHeight: 20 });
  assert.deepEqual(
    confirmed.commands,
    raised.session.confirm.targets.map((service) => ({ command: "restart", service })),
    "y sends one command per displayed target, and no more",
  );
  assert.equal(flashOf(confirmed.session, at), `Restarting ${confirmed.commands.length} services`);

  // A group with exactly one eligible member acts directly — the gate is about a *set*, not
  // about the row kind. `ops::db` holds one service.
  const onDb = { ...newSession(), cursor: { kind: "group", key: "ops::db" }, cursorRow: 5 };
  const direct = press(onDb, board, key("r"), { now: at, bodyHeight: 20 });
  assert.equal(direct.session.confirm, null, "one eligible member raises no modal");
  assert.deepEqual(direct.commands, [{ command: "restart", service: "ops::db::vacuum" }]);
  assert.equal(flashOf(direct.session, at), "Restarting ops::db::vacuum", "…and the ack names it");

  // The ineligible members are counted rather than silently dropped, so the shorter list in
  // the modal does not read as a bug.
  const onOpsStart = press(onOps, board, key("s"), { now: at, bodyHeight: 20 });
  if (onOpsStart.session.confirm !== null) {
    const shown = onOpsStart.session.confirm;
    assert.equal(
      shown.targets.length + shown.skipped,
      board.order.map((n) => board.services[n]).filter((s) => inSubtree(s.group, "ops")).length,
      "displayed + skipped is the whole subtree",
    );
  }
});

// --- the filter ------------------------------------------------------------------------

test("/ types a needle that narrows live, Enter confirms it and Esc clears it", () => {
  const { board, at } = liveBoard();
  const whole = rowsOf(newSession(), board).length;

  const typing = press(newSession(), board, key("/"), { now: at, bodyHeight: 20 }).session;
  assert.equal(typingOf(typing), "", "/ opens the prompt");
  // Typing narrows as it goes — `Filter::Typing` filters too; it is only the footer that
  // differs between typing and active.
  const ops = type(typing, board, [key("o"), key("p"), key("s")]).session;
  assert.equal(typingOf(ops), "ops");
  assert.ok(rowsOf(ops, board).length < whole, "the rows narrow while the needle is still being typed");
  assert.ok(
    rowsOf(ops, board).every((r) => {
      const k = rowKey(r);
      return k === null || k.kind === "queue" || k.key.includes("ops");
    }),
    "…to the rows the needle matches",
  );
  // Backspace pops one grapheme.
  const op = press(ops, board, key("backspace"), { now: at, bodyHeight: 20 }).session;
  assert.equal(typingOf(op), "op");

  // Enter confirms: the needle stays, the prompt goes.
  const active = press(ops, board, key("enter"), { now: at, bodyHeight: 20 }).session;
  assert.equal(typingOf(active), null, "the prompt closes");
  assert.equal(needleOf(active), "ops", "the needle is still in force");
  // `/` over a confirmed needle starts an **empty** one — `Filter::begin`'s own arm — so a
  // second search is a fresh one rather than an edit of the last.
  assert.equal(typingOf(press(active, board, key("/"), { now: at, bodyHeight: 20 }).session), "");
  // Esc on a confirmed needle clears it; Esc while typing clears and leaves.
  const cleared = press(active, board, key("esc"), { now: at, bodyHeight: 20 });
  assert.equal(needleOf(cleared.session), "");
  assert.equal(cleared.handled, true);
  assert.equal(needleOf(press(ops, board, key("esc"), { now: at, bodyHeight: 20 }).session), "");
  // Esc with nothing to clear is **not** this page's key: there is nothing to do, so the
  // browser keeps it.
  assert.equal(press(newSession(), board, key("esc"), { now: at, bodyHeight: 20 }).handled, false);

  // Ctrl+R mid-typing inserts nothing and emits nothing — `translate_filter_key`'s arm, and the
  // reason it exists: a control combo must never become a literal char in the needle.
  const ctrlR = press(ops, board, ctrl("r"), { now: at, bodyHeight: 20 });
  assert.equal(typingOf(ctrlR.session), "ops", "Ctrl+R leaves the needle alone");
  assert.deepEqual(ctrlR.commands, [], "…and reloads nothing from inside the prompt");
  // A needle is literal input, so it takes the keys the list binds as characters.
  const literal = type(typing, board, [key("j"), key("G"), key("/"), key("?")]).session;
  assert.equal(typingOf(literal), "jG/?", "every printable key is a character while typing");
  // …including one that is not ASCII, measured as a grapheme by the surfaces that render it.
  const wide = type(typing, board, [key("監"), key("視")]).session;
  assert.equal(typingOf(wide), "監視");
  assert.equal(rowsOf(wide, board).filter((r) => rowKey(r)?.kind === "service").length, 0);
});

// --- the drain -----------------------------------------------------------------------------

test("a draining daemon refuses every operator verb and still navigates", () => {
  const { board, at } = drainedBoard();
  assert.notEqual(board.quittingSince, null);
  let session = newSession();
  for (const chord of [key("s"), key("x"), key("t"), key("r"), ctrl("r"), key("+"), key("-")]) {
    const out = press(session, board, chord, { now: at, bodyHeight: 20 });
    assert.deepEqual(out.commands, [], `${chord.key} reaches no wire while draining`);
    assert.equal(out.handled, false, `…and reads as unbound, exactly as the footer paints it`);
    assert.equal(out.session.confirm, null, "…and raises no modal");
    assert.equal(flashOf(out.session, at), null, "…and acknowledges nothing");
  }
  // The keys the drain leaves alone still work: a draining board still reflows.
  const moved = press(session, board, key("j"), { now: at, bodyHeight: 20 });
  assert.equal(moved.handled, true);
  assert.notDeepEqual(cursorOn(moved.session, board), cursorOn(session, board));
  const helped = press(session, board, key("?"), { now: at, bodyHeight: 20 });
  assert.equal(helped.session.help, true, "? still opens");
});

test("the drain frame drops the overlay, the modal and any filter typing", () => {
  // `begin_quitting`'s own behaviour: the drain frame owns the screen, and once quitting the
  // shell must not route keys into the filter.
  const { board, at } = stoppingBoard();
  const busy = {
    ...press(newSession(), board, key("x"), { now: at, bodyHeight: 20 }).session,
    help: true,
    filter: { mode: "typing", needle: "ja" },
  };
  assert.notEqual(busy.confirm, null);
  const quitting = { type: "meta", meta: "quitting" };
  const after = noteFrame(busy, quitting, at);
  assert.equal(after.confirm, null, "the modal goes");
  assert.equal(after.help, false, "the overlay goes");
  assert.equal(typingOf(after), null, "the typing goes");
  // A *confirmed* needle is not typing and survives, as it does in the terminal.
  const active = noteFrame({ ...busy, filter: { mode: "active", needle: "ja" } }, quitting, at);
  assert.equal(needleOf(active), "ja");
});

// --- the `?` overlay ------------------------------------------------------------------------

test("? opens the overlay and closes on its own key and on Esc", () => {
  const { board, at } = liveBoard();
  const open = press(newSession(), board, key("?"), { now: at, bodyHeight: 20 });
  assert.equal(open.session.help, true);
  assert.equal(open.handled, true);
  for (const chord of [key("?"), key("esc")]) {
    const shut = press(open.session, board, chord, { now: at, bodyHeight: 20 });
    assert.equal(shut.session.help, false, `${chord.key} closes the overlay`);
    assert.equal(shut.handled, true);
  }
  // It captures the surface: quit and reload stay inert beneath it, and nothing navigates.
  for (const chord of [ctrl("r"), key("q"), key("j"), key("t"), key("/")]) {
    const under = press(open.session, board, chord, { now: at, bodyHeight: 20 });
    assert.deepEqual(under.commands, [], "the overlay sends nothing");
    assert.equal(under.session.help, true, "…and stays up");
    assert.deepEqual(cursorOn(under.session, board), cursorOn(open.session, board));
  }
});

// --- AC6: two tabs, one daemon ----------------------------------------------------------------

test("two sessions over one board hold independent cursors, filters and folds", () => {
  const { board, at } = liveBoard();
  const first = newSession();
  const second = newSession();
  const moved = type(first, board, [key("j"), key("j"), key("h"), key("/"), key("d"), key("enter")]).session;
  // The second session is untouched — by identity, not by a field-by-field comparison that a
  // new field could quietly fall out of.
  assert.deepEqual(second, newSession(), "the second tab's session is exactly a fresh one");
  assert.deepEqual(cursorOn(second, board), cursorOn(newSession(), board));
  assert.equal(needleOf(second), "");
  assert.deepEqual([...second.collapsed], []);
  // …and the first really did all of it, so the comparison above is not vacuous.
  assert.notDeepEqual(cursorOn(moved, board), cursorOn(second, board));
  assert.equal(needleOf(moved), "d");
  assert.ok(moved.collapsed.size > 0);
  // Neither wrote through the other's `collapsed` set — the one field that is a mutable object.
  assert.notEqual(moved.collapsed, first.collapsed, "press returns a new set rather than mutating");
  assert.deepEqual([...first.collapsed], [], "the session it was handed is unchanged");
});

test("nothing here persists a cursor or pushes one back to the daemon", () => {
  // The mechanical half of AC6. `afkd top` makes folds daemon-held with `Frame::View`, which
  // survives a re-attach — and would make two tabs share one fold set, which is exactly what
  // must not happen. A storage API would do the same across two tabs of one browser.
  for (const file of ["session.mjs", "input.mjs", "top.mjs"]) {
    const code = readFileSync(join(HERE, file), "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/^\s*\/\/.*$/gm, "");
    for (const forbidden of ["localStorage", "sessionStorage", "indexedDB"]) {
      assert.equal(code.match(new RegExp(`\\b${forbidden}\\b`)), null, `${file} names ${forbidden}`);
    }
    assert.equal(code.match(/"view"|'view'|type:\s*"view"/), null, `${file} sends a view frame`);
  }
});

// --- the flash, all three producers ------------------------------------------------------------

test("a press acknowledges the verbs nothing else on screen will", () => {
  const { board, at } = liveBoard();
  const armed = Object.values(board.services).find((s) => s.badge === "Idle");
  const stopped = Object.values(board.services).find((s) => s.badge === "Stopped");
  assert.ok(armed !== undefined && stopped !== undefined, "the capture holds both badges");
  const on = (name, row) => ({ ...newSession(), cursor: { kind: "service", key: name }, cursorRow: row });

  assert.equal(flashOf(press(on(armed.name, 0), board, key("t"), { now: at, bodyHeight: 20 }).session, at),
    `Fired ${armed.name}`);
  assert.equal(flashOf(press(on(armed.name, 0), board, key("x"), { now: at, bodyHeight: 20 }).session, at),
    `Stopping ${armed.name}`);
  assert.equal(flashOf(press(on(armed.name, 0), board, key("r"), { now: at, bodyHeight: 20 }).session, at),
    `Restarting ${armed.name}`);
  assert.equal(flashOf(press(on(stopped.name, 0), board, key("s"), { now: at, bodyHeight: 20 }).session, at),
    `Starting ${stopped.name}`);
  // `Ctrl+R` posts a reload and no ack: the daemon's own `meta.reloaded.message` owns that line,
  // which the next test drives off a real capture.
  const reloaded = press(newSession(), board, ctrl("r"), { now: at, bodyHeight: 20 });
  assert.deepEqual(reloaded.commands, [{ command: "reload" }]);
  assert.equal(flashOf(reloaded.session, at), null, "the reload key acknowledges nothing itself");

  // The flash expires on its own clock, so the footer reverts with no timer of its own.
  const fired = press(on(armed.name, 0), board, key("t"), { now: at, bodyHeight: 20 }).session;
  assert.notEqual(flashOf(fired, at + FLASH_TIMEOUT - 1), null, "still up a millisecond before");
  assert.equal(flashOf(fired, at + FLASH_TIMEOUT), null, "and gone at the timeout");
});

test("the daemon's own two messages become the flash, and control_no_op does not", () => {
  // The frame half. `reload.jsonl` carries a real `meta.reloaded` — the fixture's own string,
  // not one written here — so a re-capture cannot make this vacuous.
  const line = readFileSync(join(HERE, "fixtures", "reload.jsonl"), "utf8")
    .split("\n")
    .map((l) => (l === "" ? null : JSON.parse(l)))
    .find((f) => f !== null && f.type === "meta" && f.meta === "reloaded");
  assert.notEqual(line, undefined, "reload.jsonl still holds a meta.reloaded");
  assert.equal(typeof line.message, "string", "…with the message the footer renders");
  assert.notEqual(line.message, "");
  assert.equal(flashOf(noteFrame(newSession(), line, BASE), BASE), line.message);

  // `meta.error` appears in **no** capture: earning one means configuring a provider trigger,
  // which `fixtures/README.md` says the recorder refuses to do. So this one frame is built from
  // `proto.rs`'s `Meta::Error` shape and is called out here as the hand-built frame it is.
  //
  // The daemon is no longer its only producer: the relay **composes** one of these when the
  // runs base its backfill would read is missing or unreadable, so the flash an operator sees
  // then is this arm. Its sentence is not copied here — that would be a second spelling to keep
  // in step — because the drift gate's `drift.rs` asserts the shape the relay really writes (a `meta.error`
  // naming the path, in this plugin's own voice) against a live daemon.
  const error = { type: "meta", meta: "error", message: "no such service: nightlyy" };
  assert.equal(flashOf(noteFrame(newSession(), error, BASE), BASE), error.message);

  // `meta.control_no_op` is deliberately **not** a flash: it exists to revert a pending
  // optimistic edge, and this page folds none. Asserted explicitly so the ignore has a guard
  // rather than being an omission nobody can see.
  const noop = { type: "meta", meta: "control_no_op", service: "nightly", command: "fire" };
  const before = newSession();
  assert.deepEqual(noteFrame(before, noop, BASE), before, "control_no_op leaves the session alone");
  // …as does every frame that is board state, which `fold.mjs` owns.
  for (const frame of [
    { type: "event", event: "fire_started", service: "nightly" },
    { type: "log", service: "nightly", line: "hello" },
    { type: "meta", meta: "host_load", cpu_pct: 3 },
    { type: "meta", meta: "reloaded" },
    null,
    "not an object",
  ]) {
    assert.deepEqual(noteFrame(before, frame, BASE), before, `${JSON.stringify(frame)} is not a flash`);
  }
});

// --- the scroll ------------------------------------------------------------------------------

test("the scroll is the minimal clamp, and the session takes the same one", () => {
  // ratatui's `get_row_bounds`, swept including the degenerate cases: a zero-height viewport,
  // a one-row one, and an offset stranded past the end of a shrunken board.
  const cases = [
    [0, 0, 20, 10, 0],
    [0, 9, 20, 10, 0],
    [0, 10, 20, 10, 1],
    [0, 19, 20, 10, 10],
    [10, 5, 20, 10, 5],
    [10, 10, 20, 10, 10],
    [5, 0, 20, 10, 0],
    [0, 0, 20, 0, 0],
    [0, 19, 20, 1, 19],
    [18, 0, 3, 10, 0],
    [7, 2, 3, 10, 0],
  ];
  for (const [prev, selected, count, height, want] of cases) {
    assert.equal(scrollOffset(prev, selected, count, height), want, `scrollOffset(${[prev, selected, count, height]})`);
  }
  // …and the session's own moves take **this** function — `press` imports it rather than
  // restating it, so there is no second copy of ratatui's three lines to drift. A cursor
  // driven off the foot of a short viewport brings the row it landed on into view.
  const { board } = liveBoard();
  const rows = rowsOf(newSession(), board);
  let session = newSession();
  const height = 5;
  for (let i = 0; i < rows.length; i += 1) {
    session = press(session, board, key("j"), { now: BASE, bodyHeight: height }).session;
    const at = selectedIndex(session, rowsOf(session, board));
    assert.ok(at >= session.offset, `row ${at} is not above the viewport (offset ${session.offset})`);
    assert.ok(at < session.offset + height, `row ${at} is not below it`);
  }
  // `g` scrolls all the way back.
  const top = press(session, board, key("g"), { now: BASE, bodyHeight: height }).session;
  assert.equal(top.offset, 0);
});

// --- precedence, from the input side ------------------------------------------------------------

test("a lane row's h narrows the lane rather than folding a group — and this page takes neither", () => {
  // The precedence AC the card states, observed through the dispatch rather than through
  // `resolve` alone: on a lane row `h` is `queues.narrow`, which the relay has no verb for, so
  // it is reported **unhandled** and folds nothing. Off a lane row the same key folds.
  const { board, at } = liveBoard();
  const onLane = { ...newSession(), cursor: { kind: "queue", key: "heavy" }, cursorRow: 14 };
  assert.equal(cursorOn(onLane, board).kind, "queue");
  const lane = press(onLane, board, key("h"), { now: at, bodyHeight: 20 });
  assert.equal(lane.handled, false, "the lane verb has no relay command, so the key is not taken");
  assert.deepEqual([...lane.session.collapsed], [], "…and it did not fall through to the group fold");
  const onMember = { ...newSession(), cursor: { kind: "service", key: "ops::nightly" }, cursorRow: 2 };
  assert.deepEqual(
    [...press(onMember, board, key("h"), { now: at, bodyHeight: 20 }).session.collapsed],
    ["ops"],
    "the same chord folds on any other row",
  );
});

test("the actions this page has no surface for are inert and unprevented", () => {
  const { board, at } = liveBoard();
  // Every action `keymap.mjs` notes as unreachable here, pressed at its own primary chord.
  // Neither `i` nor `o` is among them any more — they open a service's info page and its run
  // view — nor `v` and `b`, which flip the view and the busy lens; each partial boundary (a
  // group header, a lane row) is asserted with the page's other keys below.
  for (const chord of [key("+"), key("-"), key("q"), ctrl("c")]) {
    const out = press(newSession(), board, chord, { now: at, bodyHeight: 20 });
    assert.deepEqual(out.commands, [], `${chord.key} sends nothing`);
    assert.equal(out.handled, false, `${chord.key} is left to the browser`);
    assert.deepEqual(out.session, newSession(), `${chord.key} moves nothing`);
  }
  // …and so is a chord the table does not bind at all.
  const unbound = press(newSession(), board, key("z"), { now: at, bodyHeight: 20 });
  assert.equal(unbound.handled, false);
  assert.deepEqual(unbound.commands, []);
});

// --- the info page --------------------------------------------------------------------------

test("i opens the selected service's page, and i or Esc closes it", () => {
  const { board } = liveBoard();
  // The cursor starts on `janitor`, the first selectable row.
  const opened = press(newSession(), board, key("i"), { now: BASE, bodyHeight: 20 });
  assert.equal(opened.handled, true, "`i` is this page's key on a service row");
  assert.deepEqual(opened.commands, [], "…and it posts nothing: a detail page is a local view");
  assert.equal(opened.session.info, "janitor", "the page names the **service**, not a row index");
  assert.equal(opened.session.infoOffset, 0);

  // Both keys the `info.back` action binds close it, and each leaves the session it opened from.
  for (const chord of [key("i"), key("esc")]) {
    const closed = press(opened.session, board, chord, { now: BASE, bodyHeight: 20 });
    assert.equal(closed.handled, true, `${chord.key} closes the page`);
    assert.equal(closed.session.info, null);
    assert.deepEqual(closed.session, newSession(), `${chord.key} leaves nothing behind`);
  }
});

test("the page follows the cursor, and a group or a lane has none to open", () => {
  const { board } = liveBoard();
  // Walked to a member of a nested group, so the name the page holds is the qualified one.
  const onMember = type(newSession(), board, [key("j"), key("j")]).session;
  assert.deepEqual(cursorOn(onMember, board), { kind: "service", key: "ops::nightly" });
  assert.equal(press(onMember, board, key("i"), { now: BASE, bodyHeight: 20 }).session.info, "ops::nightly");

  // `toggle_info_view` opens on `selected_card()`, so a group header and a lane row have no
  // subject — the key is a no-op there and is reported **unhandled**, so the browser keeps it.
  const onGroup = press(newSession(), board, key("j"), { now: BASE, bodyHeight: 20 }).session;
  assert.deepEqual(cursorOn(onGroup, board), { kind: "group", key: "ops" });
  const group = press(onGroup, board, key("i"), { now: BASE, bodyHeight: 20 });
  assert.equal(group.handled, false, "a group header has no info page");
  assert.equal(group.session.info, null);

  const onLane = { ...newSession(), cursor: { kind: "queue", key: "heavy" }, cursorRow: 14 };
  assert.equal(cursorOn(onLane, board).kind, "queue");
  const lane = press(onLane, board, key("i"), { now: BASE, bodyHeight: 20 });
  assert.equal(lane.handled, false, "nor does a lane row");
  assert.equal(lane.session.info, null);
});

test("the open page owns its key surface", () => {
  // `translate_info_key`: the `global` keys live under it, `info.back` returns, everything else
  // is inert **and captured** — a key this surface has no arm for must not reach the list.
  const { board } = liveBoard();
  const open = press(newSession(), board, key("i"), { now: BASE, bodyHeight: 20 }).session;

  // The two globals it forwards, and the commands each does or does not post.
  const reload = press(open, board, ctrl("r"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual(reload.commands, [{ command: "reload" }], "Ctrl+R still reloads the daemon's config");
  assert.equal(reload.session.info, "janitor", "…without closing the page");
  const help = press(open, board, key("?"), { now: BASE, bodyHeight: 20 });
  assert.equal(help.session.help, true, "`?` raises the overlay over the page");
  assert.equal(help.session.info, "janitor", "…and the page is still underneath it");

  // The list's verbs and its navigation are **not** reachable through the page: `t` would fire
  // a service, `j` would move a cursor the page does not show, `/` would open a filter the page
  // has no row set for. Each is captured and each moves nothing.
  for (const chord of [key("t"), key("x"), key("s"), key("r"), key("j"), key("k"), key("/"), key("H")]) {
    const out = press(open, board, chord, { now: BASE, bodyHeight: 20 });
    assert.equal(out.handled, true, `${chord.key} is captured by the page`);
    assert.deepEqual(out.commands, [], `${chord.key} posts nothing`);
    assert.deepEqual(out.session, open, `${chord.key} moves nothing`);
  }
  // `q` is the browser's here as it is on the list — the page does not start swallowing it.
  assert.equal(press(open, board, ctrl("c"), { now: BASE, bodyHeight: 20 }).session, open);
});

test("the drain refuses a reload from the page, as it does from the list", () => {
  // One table refuses on both sides (`keys::REFUSED_WHILE_QUITTING`), so the page cannot send a
  // verb the footer has stopped advertising.
  const { board } = drainedBoard();
  const open = { ...newSession(), info: "janitor" };
  const out = press(open, board, ctrl("r"), { now: BASE, bodyHeight: 20 });
  assert.deepEqual(out.commands, [], "no reload crosses the drain");
  assert.equal(out.handled, true, "…and the key is still the page's, not the browser's");
});

test("the page scrolls between its floor and its ceiling", () => {
  const open = { ...newSession(), info: "janitor" };
  assert.equal(scrollInfo(open, 3, 9).infoOffset, 3);
  assert.equal(scrollInfo(open, -1, 9).infoOffset, 0, "the floor holds");
  assert.equal(scrollInfo(open, 40, 9).infoOffset, 9, "…and so does the ceiling");
  // A gesture that moves nothing returns the **same** session, which is what lets the caller
  // hand the flick back to the browser rather than eating it.
  assert.equal(scrollInfo(open, -5, 9), open, "at the floor, nothing moved");
  const atCeiling = { ...open, infoOffset: 9 };
  assert.equal(scrollInfo(atCeiling, 5, 9), atCeiling, "at the ceiling, nothing moved either");
  // A ceiling that fell under the offset — a wider viewport, a shorter page — pulls it back.
  assert.equal(scrollInfo({ ...open, infoOffset: 9 }, 0, 2).infoOffset, 2);
  // Closing the page resets the scroll: a page reopened is a page at its top.
  const { board } = liveBoard();
  const scrolled = { ...open, infoOffset: 4 };
  assert.equal(press(scrolled, board, key("esc"), { now: BASE, bodyHeight: 20 }).session.infoOffset, 0);
});

test("a quitting frame leaves the page open", () => {
  // `begin_quitting` drops the overlay, the modal and any filter typing because the drain frame
  // owns the screen. The info page is not one of those: it is a base view, and a drain the
  // operator is watching a service through must not throw them back to the list.
  const { board, at } = liveBoard();
  const open = press(newSession(), board, key("i"), { now: at, bodyHeight: 20 }).session;
  const drained = noteFrame({ ...open, help: true }, { type: "meta", meta: "quitting" }, at);
  assert.equal(drained.info, "janitor", "the page survives the drain frame");
  assert.equal(drained.help, false, "…the overlay over it does not");
});

// --- the run view -------------------------------------------------------------------------

/// The viewport these presses clamp against. Deliberately **short**: at 12 rows the pane keeps
/// seven, which `trace-burst`'s twenty log lines and ten tree rows both overflow — and a scroll
/// assertion against a pane nothing overflows is an assertion about nothing.
const RUN_ROWS = 12;
const RUN_COLS = 100;

/**
 * Press a run of chords with the run view's metrics **re-measured before each one** — exactly
 * what `top.mjs`'s `read()` does, and the reason a clamp here is against the arithmetic that
 * paints rather than a number the test chose. Returns the last session and every command emitted.
 */
function runType(session, board, chords, at = BASE) {
  let s = session;
  let commands = [];
  for (const chord of chords) {
    const metrics = runMetrics(board, {
      cols: RUN_COLS,
      rows: RUN_ROWS,
      now: at,
      run: s.run,
    });
    const out = press(s, board, chord, { now: at, bodyHeight: 20, run: metrics });
    s = out.session;
    commands = commands.concat(out.commands);
  }
  return { session: s, commands };
}

/// One press, with the same re-measure — for the assertions that need `handled` too.
function runPress(session, board, chord, at = BASE) {
  const metrics = runMetrics(board, { cols: RUN_COLS, rows: RUN_ROWS, now: at, run: session.run });
  return press(session, board, chord, { now: at, bodyHeight: 20, run: metrics });
}

/// The run view's metrics for a session, at the viewport these tests use.
function metricsOf(session, board, at = BASE) {
  return runMetrics(board, { cols: RUN_COLS, rows: RUN_ROWS, now: at, run: session.run });
}

test("o opens the selected service's run view, and o or Esc closes it where it was", () => {
  const { board, at } = liveBoard("trace-burst.jsonl");
  // Land the cursor on the one service this capture really fired, so the opened view has a tree
  // to show — and move there by pressing `j`, so "the cursor is where it was" is a claim about a
  // cursor that really moved rather than about the default.
  const rows = rowsOf(newSession(), board);
  const want = rows.findIndex((r) => r.kind === "service" && r.svc.name === "deep");
  assert.notEqual(want, -1, "the capture's fired service has a row");
  const moved = type(newSession(), board, Array.from({ length: want }, () => key("j")), at).session;
  const before = { cursor: moved.cursor, cursorRow: moved.cursorRow, offset: moved.offset };
  const subject = cursorOn(moved, board);
  assert.deepEqual(subject, { kind: "service", key: "deep" }, "the cursor sits on the fired service");
  assert.ok(board.services.deep.tree.roots.length > 0, "…which really folded a run tree");

  const opened = runPress(moved, board, key("o"), at);
  assert.equal(opened.handled, true, "`o` is this page's key on a service row");
  assert.deepEqual(opened.commands, [], "…and it posts nothing: a run view is a local surface");
  assert.equal(opened.session.run.service, subject.key, "the view names the **service**, not a row index");
  // ADR-0076: it opens on the **log** pane, the tree ready behind it — the full-screen log is
  // what an operator watching a fire wants first. `model::enter_run_view` picks the same pane.
  assert.equal(opened.session.run.focus, "log", "it opens on the log pane");
  assert.equal(opened.session.run.tree, "follow", "the tree opens following its frontier");
  assert.equal(opened.session.run.log, "follow", "and the log following its tail");
  assert.equal(opened.session.run.scoped, false, "unscoped");
  assert.equal(opened.session.run.cursor, board.services[subject.key].tree.roots[0], "on the first root");
  assert.deepEqual([...opened.session.run.overrides], [], "with no folds carried in from another service");
  assert.deepEqual([...opened.session.run.expanded], []);

  // Both keys close it, and the list cursor is untouched either way — the run view never wrote
  // to it, so there is nothing to restore.
  for (const chord of [key("o"), key("esc")]) {
    const closed = runPress(opened.session, board, chord, at);
    assert.equal(closed.handled, true, `${chord.key} closes the view`);
    assert.equal(closed.session.run, null);
    assert.deepEqual(
      { cursor: closed.session.cursor, cursorRow: closed.session.cursorRow, offset: closed.session.offset },
      before,
      `${chord.key} leaves the list cursor exactly where it was`,
    );
  }

  // A service that has never fired opens a view with a **null** cursor rather than throwing: the
  // tree is empty, so there is no first root to land on, and every nav key is inert until one
  // arrives.
  const idle = rows.findIndex((r) => r.kind === "service" && r.svc.name !== "deep");
  const onIdle = { ...newSession(), cursor: rowKey(rows[idle]), cursorRow: idle };
  const empty = runPress(onIdle, board, key("o"), at).session;
  assert.deepEqual(board.services[rowKey(rows[idle]).key].tree.roots, [], "it really folded no tree");
  assert.equal(empty.run.cursor, null, "…so the view opens with no cursor");
  assert.equal(runPress(empty, board, key("tab"), at).session.run.focus, "tree", "and the frame still works");
  assert.equal(runPress(empty, board, key("j"), at).session.run.cursor, null, "a nav key over an empty tree is inert");
});

test("o has no subject on a group header or a lane row", () => {
  const { board, at } = liveBoard();
  // The two rows `UiIntent::TreeOpen` has nothing to open on, each found on a real board rather
  // than assumed to be at some index.
  const rows = rowsOf(newSession(), board);
  for (const kind of ["group", "queue"]) {
    const at_ = rows.findIndex((r) => r.kind === kind);
    assert.notEqual(at_, -1, `the capture has a ${kind} row`);
    const session = { ...newSession(), cursor: rowKey(rows[at_]), cursorRow: at_ };
    const out = runPress(session, board, key("o"), at);
    assert.equal(out.handled, false, `\`o\` on a ${kind} row is left to the browser`);
    assert.equal(out.session.run, null, "…and opens nothing");
    assert.deepEqual(out.commands, []);
  }
});

test("Tab swaps the pane and s scopes the log to the tree cursor", () => {
  const { board, at } = liveBoard("trace-burst.jsonl");
  const open = runPress({ ...newSession(), cursor: { kind: "service", key: "deep" }, cursorRow: 0 }, board, key("o"), at);
  const onLog = open.session;
  assert.equal(onLog.run.focus, "log");
  const onTree = runPress(onLog, board, key("tab"), at).session;
  assert.equal(onTree.run.focus, "tree", "`Tab` swaps which pane is shown");
  assert.equal(runPress(onTree, board, key("tab"), at).session.run.focus, "log", "…and back");

  // `s` from the **tree** *sets* the scope and swaps to the log: setting rather than toggling is
  // what keeps a second `s` from the tree from landing you on an unscoped log.
  const scoped = runPress(onTree, board, key("s"), at).session;
  assert.equal(scoped.run.scoped, true, "`s` from the tree scopes the log");
  assert.equal(scoped.run.focus, "log", "…and shows it, which is the destination its hint names");
  assert.equal(scoped.run.log, "follow", "…re-following, so the new scope tails its own output");
  // From the **log** it is an in-place toggle of the scope that pane's own title names.
  const unscoped = runPress(scoped, board, key("s"), at).session;
  assert.equal(unscoped.run.scoped, false, "`s` on the log toggles the scope");
  assert.equal(unscoped.run.focus, "log", "…in place");
  assert.equal(runPress(unscoped, board, key("s"), at).session.run.scoped, true, "…both ways");
  // A second `s` from the tree still lands scoped, which is the whole reason the tree arm sets
  // rather than toggles.
  const twice = runType(onTree, board, [key("s"), key("tab"), key("s")], at).session;
  assert.equal(twice.run.scoped, true, "a second `s` from the tree never un-scopes");
});

test("the tree cursor walks the node rows, and h/l fold or step", () => {
  const { board, at } = liveBoard("trace-burst.jsonl");
  const tree = board.services.deep.tree;
  // On the tree pane, cursor on the root.
  const start = runType({ ...newSession(), cursor: { kind: "service", key: "deep" }, cursorRow: 0 }, board, [key("o"), key("tab")], at).session;
  assert.equal(start.run.cursor, 1, "the cursor opens on the first root");

  // `j` walks the **visible node** rows in flatten order — never a body row, which is not
  // selectable. Derived from the tree itself: the root's first child is the capture's node 2.
  const down = runPress(start, board, key("j"), at).session;
  assert.equal(down.run.cursor, tree.nodes[1].children[0], "`j` steps onto the first child");
  assert.equal(runPress(down, board, key("k"), at).session.run.cursor, 1, "`k` steps back");
  assert.equal(runPress(start, board, key("k"), at).session.run.cursor, 1, "`k` on the first row clamps rather than wrapping");
  assert.equal(runType(start, board, [key("G"), key("g")], at).session.run.cursor, 1, "`g` returns to the top");

  // `l` on a collapsed parent expands it; on an expanded one it steps onto its first child;
  // `h` collapses an expanded parent and, on a leaf, steps onto its parent.
  const collapsed = runPress(start, board, key("h"), at).session;
  assert.equal(collapsed.run.overrides.get(1), true, "`h` collapses the expanded root");
  const expanded = runPress(collapsed, board, key("l"), at).session;
  assert.equal(expanded.run.overrides.get(1), false, "`l` expands it again");
  const child = runPress(expanded, board, key("l"), at).session;
  assert.equal(child.run.cursor, tree.nodes[1].children[0], "a second `l` steps onto the first child");
  // `h` from a **leaf** walks back up: the deepest node in the capture, then its parent.
  const leaf = tree.nodes[tree.nodes[1].children[0]].children[0];
  const onLeaf = { ...child, run: { ...child.run, cursor: leaf } };
  assert.equal(
    runPress(onLeaf, board, key("h"), at).session.run.cursor,
    tree.nodes[leaf].parent,
    "`h` on a leaf steps onto its parent",
  );

  // `Enter` toggles the **effective** collapse, so it visually toggles whatever the row shows.
  const toggled = runPress(start, board, key("enter"), at).session;
  assert.equal(toggled.run.overrides.get(1), true, "`Enter` folds an expanded parent");
  assert.equal(runPress(toggled, board, key("enter"), at).session.run.overrides.get(1), false, "…and unfolds it");
  // On a childless `cmd` leaf it flips that leaf's own **body** instead.
  const onCmd = { ...start, run: { ...start.run, cursor: leaf } };
  const body = runPress(onCmd, board, key("enter"), at).session;
  assert.deepEqual([...body.run.expanded], [leaf], "`Enter` on a leaf reveals its own output");
  assert.deepEqual([...runPress(body, board, key("enter"), at).session.run.expanded], [], "…and hides it again");

  // `H`/`L` write a **blanket** override onto every parent, which is what makes `L` override the
  // per-kind agent default rather than merely clearing the map.
  const parents = Object.values(tree.nodes).filter((n) => n.children.length > 0).map((n) => n.id).sort();
  const all = runPress(start, board, key("H"), at).session;
  assert.deepEqual([...all.run.overrides.keys()].sort(), parents, "`H` folds every parent");
  assert.ok([...all.run.overrides.values()].every((v) => v === true));
  const open = runPress(all, board, key("L"), at).session;
  assert.deepEqual([...open.run.overrides.keys()].sort(), parents, "`L` opens every parent");
  assert.ok([...open.run.overrides.values()].every((v) => v === false));

  // A manual move freezes the tree's follow: a run read by hand must not yank itself away.
  assert.equal(start.run.tree, "follow", "it opened following");
  assert.equal(typeof runPress(start, board, key("j"), at).session.run.tree, "number", "a cursor move freezes it");
  // `G` re-engages follow on the frontier, `f` toggles it.
  const frontier = runPress(down, board, key("G"), at).session;
  assert.equal(frontier.run.tree, "follow", "`G` follows the frontier");
  assert.equal(frontier.run.cursor, tree.newest, "…and pins the cursor to it");
  const frozen = runPress(frontier, board, key("f"), at).session;
  assert.equal(typeof frozen.run.tree, "number", "`f` disengages follow");
  assert.equal(runPress(frozen, board, key("f"), at).session.run.tree, "follow", "…and re-engages it");
});

test("scrolling up in the log pane drops follow, and f or G restores it", () => {
  // A log longer than the pane, so the scroll has somewhere to go: `trace-burst`'s twenty log
  // frames against a 30-row viewport whose pane keeps 25 of them.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const start = runPress({ ...newSession(), cursor: { kind: "service", key: "deep" }, cursorRow: 0 }, board, key("o"), at).session;
  assert.equal(start.run.focus, "log", "the run view opens on the log");
  assert.equal(start.run.log, "follow", "…following its tail");
  const { viewport, logTotal } = metricsOf(start, board, at);
  assert.ok(logTotal > viewport, `the log (${logTotal}) overflows the pane (${viewport}) — the scroll is not vacuous`);
  const maxTop = logTotal - viewport;

  // One `k` steps off the bottom rather than from row 0 — the resolve-then-freeze rule — and
  // freezes strictly below `max_top`.
  const up = runPress(start, board, key("k"), at).session;
  assert.equal(up.run.log, maxTop - 1, "`k` steps one line off the bottom");
  assert.ok(up.run.log < maxTop, "…and follow is gone");
  // `f` restores it.
  assert.equal(runPress(up, board, key("f"), at).session.run.log, "follow", "`f` restores follow");
  // …and so does `G`, from a deeper scroll.
  const deeper = runType(start, board, [key("k"), key("k"), key("k")], at).session;
  assert.equal(deeper.run.log, maxTop - 3, "three `k`s step three lines");
  assert.equal(runPress(deeper, board, key("G"), at).session.run.log, "follow", "`G` returns to the bottom, following");
  // `g` goes to the top and freezes; `j` back to the bottom **re-engages** follow, which is the
  // arm that makes `G` not a special case.
  const top = runPress(start, board, key("g"), at).session;
  assert.equal(top.run.log, 0, "`g` freezes at the first line");
  const walked = runType(top, board, Array.from({ length: maxTop }, () => key("j")), at).session;
  assert.equal(walked.run.log, "follow", "walking `j` to the bottom re-engages tailing");
  // The paging keys step by the viewport and half of it, off the same resolved top.
  assert.equal(runPress(start, board, ctrl("u"), at).session.run.log, maxTop - Math.floor(viewport / 2), "Ctrl+U is half a page");
  assert.equal(runPress(start, board, key("pageup"), at).session.run.log, Math.max(0, maxTop - viewport), "PgUp is a whole one");
  // …and both clamp rather than running off the top.
  const floored = runType(start, board, [key("pageup"), key("pageup"), key("pageup"), key("pageup")], at).session;
  assert.equal(floored.run.log, 0, "paging past the top clamps at the first line");
});

test("the open run view owns its key surface", () => {
  const { board, at } = liveBoard("trace-burst.jsonl");
  const start = runPress({ ...newSession(), cursor: { kind: "service", key: "deep" }, cursorRow: 0 }, board, key("o"), at).session;
  // The two globals it forwards, exactly as the list and the info page do.
  assert.equal(runPress(start, board, key("?"), at).session.help, true, "`?` still raises the overlay");
  assert.deepEqual(runPress(start, board, ctrl("r"), at).commands, [{ command: "reload" }], "`Ctrl+R` still reloads");
  // Everything else is inert **and reported handled**: a key struck in the run view must not
  // fall through to the list underneath it and fire a service.
  for (const chord of [key("t"), key("x"), key("r"), key("/"), key("i"), key("v"), key("b"), key("z")]) {
    const out = runPress(start, board, chord, at);
    assert.equal(out.handled, true, `${chord.key} is captured by the open view`);
    assert.deepEqual(out.commands, [], `${chord.key} posts nothing`);
    assert.equal(out.session.run.service, "deep", "…and the view is still open on its service");
    assert.equal(out.session.filter.mode, "off", `${chord.key} did not reach the list's filter`);
  }
  // `s` is the one key in that family that **is** bound in the view (`output.scope`), so it is
  // excluded from the sweep above deliberately: here it scopes the log rather than starting a
  // service, which is exactly what "the view owns its key surface" has to mean for a key the
  // list also binds.
  const pressedS = runPress(start, board, key("s"), at);
  assert.deepEqual(pressedS.commands, [], "`s` posts no start from inside the run view");
  assert.equal(pressedS.session.run.scoped, true, "…it scopes the log pane instead");

  // The drain refuses a reload from here through the same table the footer stops advertising it
  // from, so the input side and the legend refuse exactly one set.
  const drained = drainedBoard("trace-burst.jsonl");
  const onDrained = { ...start, run: { ...start.run } };
  const refusedOut = runPress(onDrained, drained.board, ctrl("r"), drained.at);
  assert.deepEqual(refusedOut.commands, [], "a reload is refused while draining");
  assert.equal(refusedOut.handled, true, "…and still captured, so Ctrl+R never reloads the tab");
});

test("a run view whose service vanished keeps its place, and its keys stay inert", () => {
  // The `draw_split` fallback: the page falls back to the list while the session keeps the view
  // open, so a service that comes back comes back to its run view. A key pressed meanwhile has
  // no tree to walk and must not throw.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const start = runPress({ ...newSession(), cursor: { kind: "service", key: "deep" }, cursorRow: 0 }, board, key("o"), at).session;
  const gone = { ...board, services: {}, order: [] };
  for (const chord of [key("j"), key("l"), key("H"), key("G"), key("enter"), key("f")]) {
    const out = runPress(start, gone, chord, at);
    assert.equal(out.handled, true, `${chord.key} is captured`);
    assert.equal(out.session.run.service, "deep", "…and the view is still open on the name it was opened on");
  }
  // The frame keys still work on a view with no subject — closing it is the one thing an
  // operator definitely wants to be able to do.
  assert.equal(runPress(start, gone, key("esc"), at).session.run, null, "`Esc` still closes it");
  assert.equal(runPress(start, gone, key("tab"), at).session.run.focus, "tree", "`Tab` still swaps the pane");
});
