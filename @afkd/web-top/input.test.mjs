// The key seam's own suite: `node --test @afkd/web-top/input.test.mjs`.
//
// `input.mjs` is the one module here that touches a browser event, so it is exercised against a
// **stub** one — an element with an `addEventListener`, a `focus` and an `ownerDocument` that
// reports an `activeElement`, which is all it uses. That is deliberate rather than a shortcut,
// and it is `paint.test.mjs`'s bargain: the module's whole contract is that it captures keys
// without stealing them, and a stub with no browser underneath is the sharpest way to say so.
// Anything it got right only because a real browser was there would fail here.
//
// The board is folded from a recorded capture, as every other suite's is, so the presses below
// resolve against real badges rather than ones chosen to make an assertion pass.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import { fold, seed } from "./fold.mjs";
import { RESERVED, chordOf, installKeys } from "./input.mjs";
import { DEFAULT_KEYS } from "./keymap.mjs";
import { infoScrollMax } from "./layout.mjs";
import { flashOf, newSession } from "./session.mjs";

const HERE = import.meta.dirname;
const BASE = 1_000_000;

// --- the stub -----------------------------------------------------------------------------

/// One `keydown`, as a browser would deliver it, with a `preventDefault` that records having
/// been called. `key` is `event.key`'s own spelling — `ArrowUp`, `" "`, `Escape` — never the
/// table's token, because the mapping between the two is the thing under test.
function keydown(key, { ctrl = false, alt = false, meta = false } = {}) {
  return {
    key,
    ctrlKey: ctrl,
    altKey: alt,
    metaKey: meta,
    prevented: false,
    preventDefault() {
      this.prevented = true;
    },
  };
}

/// The stub element: listeners by type, a focus that moves its document's `activeElement`, and
/// nothing else. No layout, no cascade, no event system — `input.mjs` needs none of those, and
/// a stub that offered them would let it start depending on one.
function element() {
  const el = {
    listeners: {},
    ownerDocument: { activeElement: null },
    focus() {
      el.ownerDocument.activeElement = el;
    },
    addEventListener(type, handler) {
      (el.listeners[type] ??= []).push(handler);
    },
    /// Deliver one event, as the browser would, and hand it back for its `prevented` bit.
    dispatch(type, event) {
      for (const handler of el.listeners[type] ?? []) handler(event);
      return event;
    },
  };
  return el;
}

/// The capture's live board — the same fixture `layout.test.mjs` and `session.test.mjs` render.
function liveBoard(file = "snapshot.jsonl") {
  let board = seed({ logLines: 2000 });
  let at = BASE;
  for (const line of readFileSync(join(HERE, "fixtures", file), "utf8").split("\n")) {
    if (line === "") continue;
    const frame = JSON.parse(line);
    if (frame.type === "meta" && frame.meta === "quitting") break;
    board = fold(board, frame, (at += 10));
  }
  return { board, at };
}

/// The viewport the stub reports, in cells — narrow and short enough that the info page
/// overflows it by more than one notch of a wheel, so the gesture's own arithmetic is what the
/// assertions below read rather than the clamp.
const VIEWPORT = { cols: 60, rows: 16 };

/**
 * A wired-up grid: the stub element, a recording `post`, and the session the presses move.
 * `post` resolves to acceptance unless a test hands `reply` something else, which is the
 * relay-refused arm.
 */
function wire({ board, session = newSession(), reply = { ok: true, message: "" } } = {}) {
  const el = element();
  const state = {
    element: el,
    posted: [],
    painted: 0,
    session,
    /// The promises the handler started, so a test can await the reply arm rather than sleep.
    inflight: [],
  };
  installKeys({
    element: el,
    read: () => ({
      session: state.session,
      board,
      streamId: "9f1c",
      bodyHeight: 20,
      // The ceiling the shell really threads — `infoScrollMax` over the same options
      // `layout()` paints from — rather than a number picked to make a wheel test pass.
      infoMax: infoScrollMax(board, {
        cols: VIEWPORT.cols,
        rows: VIEWPORT.rows,
        now: BASE,
        info: state.session.info,
      }),
    }),
    write: (next) => {
      state.session = next;
    },
    post: (body) => {
      state.posted.push(body);
      const promise = Promise.resolve(reply);
      state.inflight.push(promise);
      return promise;
    },
    now: () => BASE,
    repaint: () => {
      state.painted += 1;
    },
  });
  return state;
}

// --- chordOf: the join between the pinned table and the dispatch ------------------------------

test("chordOf produces the binding table's own tokens", () => {
  // If `ArrowUp` yielded `"Up"`, or `" "` did not become `space`, every keymap and session test
  // would stay green and the page would be dead. This is the one place that join is asserted.
  const named = [
    ["ArrowUp", "up"],
    ["ArrowDown", "down"],
    ["ArrowLeft", "left"],
    ["ArrowRight", "right"],
    ["Enter", "enter"],
    [" ", "space"],
    ["Escape", "esc"],
    ["Tab", "tab"],
    ["PageUp", "pageup"],
    ["PageDown", "pagedown"],
    ["Backspace", "backspace"],
  ];
  for (const [browser, token] of named) {
    assert.deepEqual(chordOf(keydown(browser)), { ctrl: false, key: token }, `${browser} → ${token}`);
  }
  // A printable char passes through with its **case**, which is semantic: `g` and `G` are two
  // keys, and `H`/`L` are the fold-all pair.
  for (const c of ["g", "G", "h", "H", "l", "L", "?", "+", "-", "/", "j", "k", "s", "x", "t", "r", "y", "n"]) {
    assert.deepEqual(chordOf(keydown(c)), { ctrl: false, key: c }, `${c} passes through`);
  }
  // …including one that is not ASCII, because a needle is literal input and a non-US layout
  // reaches a glyph the table has no letter for.
  assert.deepEqual(chordOf(keydown("監")), { ctrl: false, key: "監" });
  assert.deepEqual(chordOf(keydown("é")), { ctrl: false, key: "é" });
  // The `ctrl` bit rides through on both the named and the char arm.
  assert.deepEqual(chordOf(keydown("r", { ctrl: true })), { ctrl: true, key: "r" });
  assert.deepEqual(chordOf(keydown("u", { ctrl: true })), { ctrl: true, key: "u" });
  assert.deepEqual(chordOf(keydown("ArrowDown", { ctrl: true })), { ctrl: true, key: "down" });
  // `Alt` and `Meta` chords have no spelling in the grammar, so they are inert rather than
  // swallowed; so is a dead key, a lone modifier and every F-key.
  for (const event of [
    keydown("r", { alt: true }),
    keydown("r", { meta: true }),
    keydown("ArrowUp", { meta: true }),
    keydown("Dead"),
    keydown("Shift"),
    keydown("Control"),
    keydown("F5"),
    keydown("F12"),
    keydown("Home"),
    keydown("Insert"),
    keydown(undefined),
  ]) {
    assert.equal(chordOf(event), null, `${event.key} has no chord`);
  }
});

test("every token the 51 rows spell is one chordOf can emit", () => {
  // The coverage assertion, back against the pinned table: a token `keymap.mjs` gains that no
  // browser event maps to would otherwise ship as a dead binding — bound, listed in the
  // overlay, and unreachable. The sweep is the whole of `chordOf`'s vocabulary.
  const sweep = [
    "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Enter", " ", "Escape", "Tab",
    "PageUp", "PageDown", "Backspace",
    ..."abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789?/+-=.,;'[]\\`",
  ];
  const emitted = new Set();
  for (const key of sweep) {
    for (const ctrl of [false, true]) {
      const chord = chordOf(keydown(key, { ctrl }));
      if (chord !== null) emitted.add(chord.key);
    }
  }
  const wanted = new Set(DEFAULT_KEYS.flatMap((row) => row.binding.split(" ").map((a) => a.replace(/^ctrl-/, ""))));
  assert.ok(wanted.size >= 20, `the table's token set read as ${wanted.size} tokens — too few to be the real one`);
  for (const token of wanted) {
    assert.ok(emitted.has(token), `no keydown in the sweep produces the chord token ${JSON.stringify(token)}`);
  }
});

// --- AC7: Ctrl+R is afkd's, and the browser's chords are the browser's --------------------------

test("Ctrl+R reloads the config and does not reload the page", () => {
  const { board } = liveBoard();
  const grid = wire({ board });
  grid.element.focus();
  const event = grid.element.dispatch("keydown", keydown("r", { ctrl: true }));
  // Both halves, because either alone passes for the wrong reason: a page that prevented and
  // sent nothing, or one that sent a reload and then reloaded the tab out from under it.
  assert.deepEqual(grid.posted, [{ stream: "9f1c", command: "reload" }], "one reload, on this tab's attach");
  assert.equal(event.prevented, true, "…and the browser's own reload is taken");
  assert.equal(grid.posted.length, 1, "exactly one, not one per listener");
});

test("Ctrl+R is taken even when the dispatch refuses it", () => {
  // A drained daemon refuses the reload, so `press` reports it unhandled — but the page must
  // still eat the chord. A Ctrl+R that sometimes reloads the tab is worse than one that never
  // does, so the reserved table decides this rather than the dispatch's return.
  let board = seed({ logLines: 2000 });
  let at = BASE;
  for (const line of readFileSync(join(HERE, "fixtures", "snapshot.jsonl"), "utf8").split("\n")) {
    if (line === "") continue;
    board = fold(board, JSON.parse(line), (at += 10));
  }
  assert.notEqual(board.quittingSince, null, "the capture really is drained");
  const grid = wire({ board });
  grid.element.focus();
  const event = grid.element.dispatch("keydown", keydown("r", { ctrl: true }));
  assert.deepEqual(grid.posted, [], "the drain refuses it, so nothing reaches the wire");
  assert.equal(event.prevented, true, "…and the tab still does not reload");
});

test("the browser's own chords are neither dispatched nor prevented", () => {
  const { board } = liveBoard();
  for (const reserved of RESERVED.left) {
    const grid = wire({ board });
    grid.element.focus();
    const event = grid.element.dispatch("keydown", keydown(reserved.key, { ctrl: reserved.ctrl }));
    const spelling = `${reserved.ctrl ? "Ctrl+" : ""}${reserved.key}`;
    assert.deepEqual(grid.posted, [], `${spelling} posts nothing`);
    assert.equal(event.prevented, false, `${spelling} stays the browser's`);
    assert.deepEqual(grid.session, newSession(), `${spelling} moves nothing`);
  }
  // The table is the card's own list plus the two the page owes the operator: a new window, and
  // the clipboard — `global.quit` is unhandled here anyway, so `Ctrl+C` costs nothing to leave.
  assert.deepEqual(
    RESERVED.left.map((c) => `${c.ctrl ? "Ctrl+" : ""}${c.key}`),
    ["Ctrl+t", "Ctrl+w", "Ctrl+l", "Ctrl+n", "Ctrl+c", "F5"],
  );
  assert.deepEqual(RESERVED.taken, [{ ctrl: true, key: "r" }], "one chord is taken, and it is named");
});

test("preventDefault is called exactly when the press was handled", () => {
  const { board } = liveBoard();
  const cases = [
    // A bound key this page takes.
    ["j", {}, true],
    ["ArrowDown", {}, true],
    ["?", {}, true],
    // `i` opens the selected service's info page and `o` its run view, so both are this page's
    // keys now.
    ["i", {}, true],
    ["o", {}, true],
    // A bound key that resolves to an action with **no surface here** — it must not prevent, or
    // the page would eat a chord it does nothing with.
    ["v", {}, false],
    ["b", {}, false],
    ["q", {}, false],
    // An unbound key.
    ["z", {}, false],
    ["9", {}, false],
    // A key with no chord at all.
    ["F12", {}, false],
    ["r", { alt: true }, false],
  ];
  for (const [key, mods, want] of cases) {
    const grid = wire({ board });
    grid.element.focus();
    const event = grid.element.dispatch("keydown", keydown(key, mods));
    assert.equal(event.prevented, want, `${key} ${want ? "is" : "is not"} this page's key`);
  }
});

test("a keydown while the grid is not focused does nothing at all", () => {
  // The focus gate, asserted rather than assumed: a key struck while the operator is in the
  // address bar is not this page's, and `t` is the one that would otherwise fire a service.
  const { board } = liveBoard();
  const grid = wire({ board });
  grid.element.ownerDocument.activeElement = { somethingElse: true };
  const event = grid.element.dispatch("keydown", keydown("t"));
  assert.deepEqual(grid.posted, [], "no command");
  assert.equal(event.prevented, false, "no interception");
  assert.deepEqual(grid.session, newSession(), "no state moved");
  assert.equal(grid.painted, 0, "not even a repaint");
  // Even the chord the page otherwise always takes.
  const ctrlR = grid.element.dispatch("keydown", keydown("r", { ctrl: true }));
  assert.equal(ctrlR.prevented, false, "Ctrl+R off-focus is the browser's reload");
  assert.deepEqual(grid.posted, []);
  // …and the grid takes focus back on a pointer down, which is how an operator returns to it.
  grid.element.dispatch("pointerdown", {});
  assert.equal(grid.element.ownerDocument.activeElement, grid.element);
  assert.equal(grid.element.dispatch("keydown", keydown("j")).prevented, true);
});

// --- the posted body ---------------------------------------------------------------------------

test("a verb posts {stream, command, service} and a global verb posts no service", () => {
  const { board } = liveBoard();
  const armed = Object.values(board.services).find((s) => s.badge === "Idle");
  assert.notEqual(armed, undefined, "the capture holds an armed service");
  const grid = wire({
    board,
    session: { ...newSession(), cursor: { kind: "service", key: armed.name }, cursorRow: 0 },
  });
  grid.element.focus();
  grid.element.dispatch("keydown", keydown("t"));
  // All three fields: the relay keys a command to **this** subscriber's own attach and 404s a
  // body whose `stream` names none, and that 404 would read as a daemon refusal.
  assert.deepEqual(grid.posted, [{ stream: "9f1c", command: "fire", service: armed.name }]);
  assert.deepEqual(Object.keys(grid.posted[0]), ["stream", "command", "service"]);
  // A global verb carries no `service` key at all, rather than an empty one: the relay composes
  // the frame from the verb, and a field it did not ask for would be smuggled onto the wire.
  grid.element.dispatch("keydown", keydown("r", { ctrl: true }));
  assert.deepEqual(grid.posted[1], { stream: "9f1c", command: "reload" });
  assert.ok(!("service" in grid.posted[1]));
  // A press that dispatched nothing posts nothing.
  const painted = grid.painted;
  grid.element.dispatch("keydown", keydown("j"));
  assert.equal(grid.posted.length, 2, "a cursor move is not a command");
  assert.ok(grid.painted > painted, "…but it does repaint, rather than waiting on the throttle");
});

test("a confirmed fan-out posts one body per target, and exactly one", () => {
  const { board } = liveBoard();
  const grid = wire({
    board,
    session: { ...newSession(), cursor: { kind: "group", key: "ops" }, cursorRow: 1 },
  });
  grid.element.focus();
  grid.element.dispatch("keydown", keydown("r"));
  assert.deepEqual(grid.posted, [], "the gate posts nothing");
  const targets = grid.session.confirm.targets;
  assert.ok(targets.length > 1);
  grid.element.dispatch("keydown", keydown("y"));
  assert.deepEqual(
    grid.posted,
    targets.map((service) => ({ stream: "9f1c", command: "restart", service })),
  );
  // A second `y` after the modal closed posts nothing: the modal is gone, so the key is the
  // list's, and `y` binds nothing there.
  const after = grid.posted.length;
  grid.element.dispatch("keydown", keydown("y"));
  assert.equal(grid.posted.length, after, "a stray y sends nothing");
});

test("a relay refusal replaces the ack rather than appending to it", async () => {
  const { board } = liveBoard();
  const armed = Object.values(board.services).find((s) => s.badge === "Idle");
  const grid = wire({
    board,
    session: { ...newSession(), cursor: { kind: "service", key: armed.name }, cursorRow: 0 },
    reply: { ok: false, message: "no open stream '9f1c'; open GET /stream and post the id it deals you" },
  });
  grid.element.focus();
  grid.element.dispatch("keydown", keydown("t"));
  // The ack is written **before** the await, so for an instant the footer says the fire
  // happened — which is the honest thing to say while the request is in flight.
  assert.equal(flashOf(grid.session, BASE), `Fired ${armed.name}`);
  await Promise.all(grid.inflight);
  assert.equal(
    flashOf(grid.session, BASE),
    "no open stream '9f1c'; open GET /stream and post the id it deals you",
    "the refusal overwrites it, so a command that failed leaves no ack on screen",
  );
  // The board is untouched either way: nothing here folds a badge.
  assert.equal(board.services[armed.name].badge, "Idle");
});

// --- the module's own posture ---------------------------------------------------------------

test("input.mjs names no browser global", () => {
  // The scan is over **code**, not prose. The module reaches the DOM only through the one
  // element it is handed, which is what makes the stub above a fair oracle rather than a
  // convenient one.
  const code = readFileSync(join(HERE, "input.mjs"), "utf8")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^\s*\/\/.*$/gm, "");
  for (const forbidden of ["document", "window", "fetch", "performance", "setTimeout"]) {
    assert.equal(
      code.match(new RegExp(`(^|[^.\\w])${forbidden}\\b`)),
      null,
      `input.mjs names \`${forbidden}\`: every effect it has must arrive as a dependency`,
    );
  }
});

// --- the wheel: the info page's scroll gesture -----------------------------------------------

/// One `wheel`, as a browser would deliver it. `deltaMode` is the browser's own unit axis —
/// `0` pixels, `1` lines, `2` pages — and getting it wrong is the classic way a wheel handler
/// scrolls a hundred rows per notch, so all three are delivered here.
function wheel(deltaY, deltaMode = 0) {
  return {
    deltaY,
    deltaMode,
    prevented: false,
    preventDefault() {
      this.prevented = true;
    },
  };
}

test("the wheel scrolls the info page and nothing else", () => {
  const { board } = liveBoard();
  // A viewport the page really overflows, so the gesture has somewhere to go.
  const max = infoScrollMax(board, { cols: VIEWPORT.cols, rows: VIEWPORT.rows, now: BASE, info: "janitor" });
  assert.ok(max > 10, `the page overflows the viewport by more than one notch (${max})`);

  // Over the **list**, a wheel is the browser's: the page has no scroll surface there (the
  // cursor is what moves the body), so the flick must scroll the tab.
  const list = wire({ board });
  list.element.focus();
  const overList = list.element.dispatch("wheel", wheel(120));
  assert.equal(overList.prevented, false, "a wheel over the list stays the browser's");
  assert.deepEqual(list.session, newSession(), "…and moves nothing");

  // Over the page it scrolls, in **rows**: 120 pixels is the browser's three lines here, not
  // a hundred and twenty.
  const grid = wire({ board, session: { ...newSession(), info: "janitor" } });
  grid.element.focus();
  const down = grid.element.dispatch("wheel", wheel(120));
  assert.equal(down.prevented, true, "the page takes the gesture it acted on");
  assert.equal(grid.session.infoOffset, 8, "120px is eight rows at 16px a row");
  grid.element.dispatch("wheel", wheel(-120));
  assert.equal(grid.session.infoOffset, 0, "…and back up again");
});

test("the wheel honours the browser's own delta unit", () => {
  const { board } = liveBoard();
  for (const [delta, mode, want] of [
    [32, 0, 2], // pixels: two rows
    [3, 1, 3], // lines: already rows
    [1, 2, 10], // pages
    [1, 0, 1], // a flick smaller than a row still moves one, rather than nothing
    [-1, 0, 0], // …and one upward at the floor is refused, not rounded to zero and eaten
  ]) {
    const grid = wire({ board, session: { ...newSession(), info: "janitor" } });
    grid.element.focus();
    grid.element.dispatch("wheel", wheel(delta, mode));
    assert.equal(grid.session.infoOffset, want, `deltaY ${delta} in mode ${mode} is ${want} rows`);
  }
});

test("a wheel that moves nothing is handed back to the browser", () => {
  // At either end of the page the gesture is the browser's again, so an over-scrolled page does
  // not silently eat the flick — the one thing a hijacked wheel must never do.
  const { board } = liveBoard();
  const max = infoScrollMax(board, { cols: VIEWPORT.cols, rows: VIEWPORT.rows, now: BASE, info: "janitor" });
  const atTop = wire({ board, session: { ...newSession(), info: "janitor" } });
  atTop.element.focus();
  assert.equal(atTop.element.dispatch("wheel", wheel(-120)).prevented, false, "up at the top is not taken");
  const atFoot = wire({ board, session: { ...newSession(), info: "janitor", infoOffset: max } });
  atFoot.element.focus();
  assert.equal(atFoot.element.dispatch("wheel", wheel(120)).prevented, false, "down at the foot is not taken");
  assert.equal(atFoot.session.infoOffset, max, "…and neither moved the page");
});

test("a wheel while the grid is not focused does nothing at all", () => {
  // The same focus gate the keydown listener holds: a flick while the operator is scrolling some
  // other part of the page is not this grid's.
  const { board } = liveBoard();
  const grid = wire({ board, session: { ...newSession(), info: "janitor" } });
  grid.element.ownerDocument.activeElement = { somethingElse: true };
  const event = grid.element.dispatch("wheel", wheel(120));
  assert.equal(event.prevented, false, "no interception");
  assert.equal(grid.session.infoOffset, 0, "no scroll");
});
