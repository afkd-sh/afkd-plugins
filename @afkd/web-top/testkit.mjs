// The machinery the suites share: a rendered grid's three planes, the committed golden they
// are diffed against, the structural invariants every screen holds, and the read of afkd's
// own source the transcription tests hold the javascript to.
//
// It lives beside the modules rather than inside `layout.test.mjs` because
// `backfill.test.mjs` renders the same panes off a **disk** replay and diffs them against
// goldens of the same shape. A second copy of `goldenOf` would be two spellings of one
// file format — and the day a role or a weight letter changed, half the committed goldens
// would move and the other half would not.
//
// Not a module the page loads: nothing here is imported by `top.mjs` or anything it reaches.

import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { fold, seed } from "./fold.mjs";
import { ROLES, textWidth } from "./layout.mjs";

/// `node <suite>.mjs --write-goldens` rewrites them; `node --test` never does.
const WRITING = process.argv.includes("--write-goldens");

const HERE = import.meta.dirname;
/// Where the committed screens live, beside this file.
const GOLDENS = join(HERE, "goldens");

// --- afkd's own source ---------------------------------------------------------------

/// The afkd checkout `AFKD_SRC` names. afkd lives in a repository of its own, so a test that
/// reads its Rust runs only where one is named, and skips everywhere else — the release
/// workflow included — with [`NO_AFKD_SRC`] as its reason rather than failing on a missing file.
const AFKD_SRC = process.env.AFKD_SRC ?? "";

/// The `skip` option for a test that reads afkd's Rust: `false` with a checkout, the reason
/// without one.
export const NO_AFKD_SRC = AFKD_SRC === "" ? "AFKD_SRC names no afkd checkout to read the Rust from" : false;

/// One of afkd's own source files, `parts` relative to the checkout's root.
export function afkdSource(...parts) {
  return readFileSync(join(AFKD_SRC, ...parts), "utf8");
}

// --- the fixtures ------------------------------------------------------------------

/// A synthetic clock, so every anchor the fold computes and every age the layout renders is
/// pure arithmetic and identical across runs. The base is far from zero so a `now` before the
/// first frame is still a positive instant.
export const BASE = 1_000_000;
/// The step between two frames of a capture, in milliseconds.
export const STEP = 10;

/// Every frame of one recorded capture under `fixtures/`, in the order the daemon sent it.
export function readCapture(file) {
  return readFileSync(join(HERE, "fixtures", file), "utf8")
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line));
}

/**
 * Fold `frames` into a board from `start`, one [`STEP`] apart. Returns the board and the
 * instant the last frame landed at, so a test renders against a clock that really is "just
 * after the capture".
 */
export function foldFrames(frames, start = BASE) {
  let board = seed({ logLines: 2000 });
  let at = start;
  for (const frame of frames) board = fold(board, frame, (at += STEP));
  return { board, at };
}

/**
 * Fold `file`'s frames into a board, stopping at `until` (a predicate on the frame) when one
 * is given.
 */
export function foldCapture(file, until) {
  const frames = readCapture(file);
  const stop = until === undefined ? frames.length : frames.findIndex(until);
  return foldFrames(stop === -1 ? frames : frames.slice(0, stop));
}

/// The capture's **live** board: every frame up to the `SIGINT` the recorder stayed attached
/// through. Every capture ends with that drain, so a board folded whole is a board
/// of stopped services — true, and the subject of its own screen below, but not the board a
/// dashboard spends its life showing.
export function liveBoard(file = "snapshot.jsonl") {
  return foldCapture(file, (f) => f.type === "meta" && f.meta === "quitting");
}

/// The same capture folded **whole**, drain included: the `Quitting` header, the suppressed
/// reconcile markers and the shelf of `Stopped` rows.
export function drainedBoard(file = "snapshot.jsonl") {
  return foldCapture(file);
}

/// One screen rendered to text, one row per line — the first of a golden's three planes.
export function screenText(rows) {
  return rows.map((row) => row.map((c) => c.text).join("")).join("\n");
}

/// The palette role each cell wears, one letter per **cell** — so the plane a golden holds
/// lines up under the text plane in any terminal, a wide glyph's two columns over its two
/// letters.
const ROLE_LETTER = {
  ink: "n",
  bright: "b",
  legend: "l",
  recede: "r",
  accent: "a",
  "accent-dim": "e",
  caution: "c",
  alarm: "x",
  ok: "k",
  idle: "i",
  muted: "m",
};
/// Every foreground role the layout can emit has a letter, so a new tone cannot slip into a
/// golden as a `?` nobody reads — the same equality the stylesheet's scan holds, one file
/// over. `selection` is the one background role and is spelled by the weight plane instead.
/// It is checked here rather than inside a test because `--write-goldens` writes before any
/// test the runner would have ordered after it, and a `?` must never reach a golden.
const FG_ROLES = ROLES.filter((r) => r !== "selection");
assert.deepEqual(Object.keys(ROLE_LETTER).sort(), [...FG_ROLES].sort(), "every foreground role has a plane letter");
assert.equal(new Set(Object.values(ROLE_LETTER)).size, FG_ROLES.length, "…and no two roles share one");

const ROLE_RULE = "── fg · n ink · b bright · l legend · r recede · a accent · e accent-dim · c caution · x alarm · k ok · i idle · m muted";
const WEIGHT_RULE = "── weight · . plain · d dim · b bold · s selection band · D band, dim · B band, bold";

/// The weight-and-band letter for one cell. `dim` and `bold` are the terminal's two weight
/// attributes and never co-occur here (`assertGrid` holds them to that), and the selection
/// bar is the only background the page paints — so one letter spells all three: the weight
/// in lower case off the band, in upper case on it.
function weightLetter(c) {
  const band = c.bg === "selection";
  if (c.dim) return band ? "D" : "d";
  if (c.bold) return band ? "B" : "b";
  return band ? "s" : ".";
}

/// One plane: `letter(cell)` repeated across each cell's own width, one line per row.
function plane(rows, letter) {
  return rows.map((row) => row.map((c) => letter(c).repeat(c.width)).join("")).join("\n");
}

/**
 * The three planes a golden holds — what the screen **says**, what colour it says it in, and
 * what weight. The text plane alone would leave four of a cell's six fields unasserted: the
 * footer's gated hints, the trend lanes' recession, the column header's weight and the
 * selection bar's position are all look and no text, and a golden blind to them is a golden
 * that passes over a page painted flat.
 */
export function goldenOf(rows) {
  return `${screenText(rows)}\n\n${ROLE_RULE}\n${plane(rows, (c) => ROLE_LETTER[c.fg] ?? "?")}\n\n${WEIGHT_RULE}\n${plane(rows, weightLetter)}\n`;
}

/// Diff a rendered screen against its committed golden, failing with the whole screen rather
/// than a character offset — a grid is only readable whole.
export function assertGolden(name, rows) {
  const path = join(GOLDENS, name);
  const got = goldenOf(rows);
  if (WRITING) {
    writeFileSync(path, got);
    return;
  }
  let want;
  try {
    want = readFileSync(path, "utf8");
  } catch {
    assert.fail(`no golden ${name}; the screen it would hold is:\n${got}`);
  }
  assert.equal(got, want, `${name} drifted. The screen now reads:\n${got}\nthe golden holds:\n${want}`);
}

/// The two structural invariants every screen holds by construction, asserted on every screen
/// these suites render: each row sums to exactly `cols` (so nothing wraps into a broken row and
/// nothing overruns), and each cell reports the width its own text measures (so a painter that
/// trusts `width` paints what the layout planned).
export function assertGrid(rows, cols, height) {
  assert.equal(rows.length, height, "the screen is exactly as tall as the viewport");
  rows.forEach((row, i) => {
    let width = 0;
    for (const cell of row) {
      assert.equal(cell.width, textWidth(cell.text), `row ${i}: cell ${JSON.stringify(cell.text)} misreports its width`);
      assert.ok(!cell.text.includes("\n"), `row ${i} carries a newline`);
      // …and that the look is one the planes above can spell: an unknown role or a second
      // background would go into a golden as a `?`, and a cell both dim and bold would lose
      // its weight silently.
      assert.ok(ROLE_LETTER[cell.fg] !== undefined, `row ${i}: ${JSON.stringify(cell.text)} wears the unknown role ${cell.fg}`);
      assert.ok(cell.bg === null || cell.bg === "selection", `row ${i}: the only band is the selection bar, not ${cell.bg}`);
      assert.ok(!(cell.dim && cell.bold), `row ${i}: ${JSON.stringify(cell.text)} is both dim and bold`);
      width += cell.width;
    }
    assert.equal(width, cols, `row ${i} is ${width} cells, not ${cols}: ${JSON.stringify(row.map((c) => c.text).join(""))}`);
  });
}

/// The run view's body rows — the pane's window, between its title band and the footer. Read
/// off the screen rather than off a plan, so an assertion sees what an operator sees.
export function paneBody(rows) {
  const text = screenText(rows).split("\n");
  const footerAt = text.findIndex((l, i) => i > 4 && /^[a-zA-Z?].* (quit|help)( |$)/.test(l));
  return text.slice(4, footerAt === -1 ? text.length : footerAt).filter((l) => l.trim() !== "");
}
