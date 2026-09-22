// The layout's own suite: `node --test plugins/@afkd/web-top/layout.test.mjs`.
//
// Every board here is **folded from a recorded capture** under `fixtures/` at a fixed
// synthetic clock — the rule `fixtures/README.md` states, and the reason a re-capture cannot
// quietly make an assertion vacuous. Nothing hand-writes a wire frame.
//
// The screens are diffed against committed goldens under `goldens/`. The suite never writes
// one: a mismatch fails with the whole screen in the message, and regenerating is a
// deliberate act (`node layout.test.mjs --write-goldens`), so a drift has to be looked at
// before it is accepted.

import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import { fold, seed } from "./fold.mjs";
import { DEFAULT_KEYS, DESCRIPTIONS, SCOPES, all, idOf, primary } from "./keymap.mjs";
import { ROLES, TREND_LADDER, bodyHeight, formatElapsed, infoScrollMax, infoView, layout, runMetrics, scrollOffset, textWidth, visibleRows } from "./layout.mjs";
import { newSession, press } from "./session.mjs";

const HERE = import.meta.dirname;
const REPO = join(HERE, "..", "..", "..");
/// `node layout.test.mjs --write-goldens` rewrites them; `node --test` never does.
const WRITING = process.argv.includes("--write-goldens");

// --- the fixtures ------------------------------------------------------------------

/// A synthetic clock, so every anchor the fold computes and every age the layout renders is
/// pure arithmetic and identical across runs. The base is far from zero so a `now` before the
/// first frame is still a positive instant.
const BASE = 1_000_000;
/// The step between two frames of a capture, in milliseconds.
const STEP = 10;

/**
 * Fold `file`'s frames into a board, stopping at `until` (a predicate on the frame) when one
 * is given. Returns the board and the instant the last folded frame landed at, so a test
 * renders against a clock that really is "just after the capture".
 */
function foldCapture(file, until) {
  let board = seed({ logLines: 2000 });
  let at = BASE;
  for (const line of readFileSync(join(HERE, "fixtures", file), "utf8").split("\n")) {
    if (line === "") continue;
    const frame = JSON.parse(line);
    if (until !== undefined && until(frame)) break;
    board = fold(board, frame, (at += STEP));
  }
  return { board, at };
}

/// The capture's **live** board: every frame up to the `SIGINT` the recorder stayed attached
/// through. Every capture ends with that drain, so a board folded whole is a board
/// of stopped services — true, and the subject of its own screen below, but not the board a
/// dashboard spends its life showing.
function liveBoard(file = "snapshot.jsonl") {
  return foldCapture(file, (f) => f.type === "meta" && f.meta === "quitting");
}

/// The same capture folded **whole**, drain included: the `Quitting` header, the suppressed
/// reconcile markers and the shelf of `Stopped` rows.
function drainedBoard(file = "snapshot.jsonl") {
  return foldCapture(file);
}

/// One screen rendered to text, one row per line — the first of a golden's three planes.
function screenText(rows) {
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
function goldenOf(rows) {
  return `${screenText(rows)}\n\n${ROLE_RULE}\n${plane(rows, (c) => ROLE_LETTER[c.fg] ?? "?")}\n\n${WEIGHT_RULE}\n${plane(rows, weightLetter)}\n`;
}

/// Diff a rendered screen against its committed golden, failing with the whole screen rather
/// than a character offset — a grid is only readable whole.
function assertGolden(name, rows) {
  const path = join(HERE, "goldens", name);
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
/// this suite renders: each row sums to exactly `cols` (so nothing wraps into a broken row and
/// nothing overruns), and each cell reports the width its own text measures (so a painter that
/// trusts `width` paints what the layout planned).
function assertGrid(rows, cols, height) {
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

// --- AC1: the whole overview -------------------------------------------------------

test("renders the whole overview at 100x30", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" });
  assertGrid(rows, 100, 30);
  assertGolden("overview-100x30.txt", rows);

  // …and the golden really holds each thing the criterion names, so a golden regenerated over
  // a screen that lost half its bands cannot pass by agreeing with itself.
  const text = screenText(rows);
  const lines = text.split("\n");
  assert.match(lines[0], /^afkd 0\.2\.123 · ● Running \d+s · 8 up · 2 down/, "the title bar");
  assert.match(lines[1], /^CPU .*· Mem .*· Net ↓/, "the host-load strip, with its three segments");
  for (const label of ["Service", "State", "Trigger", "Last Activity", "Next Run"]) {
    assert.ok(lines[3].includes(label), `the column header carries ${label}`);
  }
  // Every one of the capture's ten services, each on exactly one row. The needle is the
  // **leaf**, because a member row drops the namespace its header already carries.
  assert.equal(Object.keys(board.services).length, 10, "the capture still holds its ten services");
  for (const name of Object.keys(board.services)) {
    const leaf = name.includes("::") ? name.slice(name.lastIndexOf("::") + 2) : name;
    assert.equal(lines.filter((l) => l.includes(leaf)).length, 1, `${name} has exactly one row`);
  }
  // The confined one wears its marker, hard against its name like a reconcile marker, and
  // nothing else does.
  assert.equal(lines.filter((l) => l.includes("🔒")).length, 1, "one confined service, one marker");
  assert.ok(lines.some((l) => l.includes("🔒archivist")), "…on the service whose snapshot entry is confined");
  // Both group headers, including the nested one — the two-level `ops::db` is what makes the
  // transitive rollup non-vacuous.
  assert.ok(lines.some((l) => l.includes("⊟ 📦 ops")), "the `ops` header");
  assert.ok(lines.some((l) => l.includes("⊟ 📦 db")), "the nested `db` header");
  // The nested header's own rollup: a crash in `ops::broken` surfaces on `ops`, and the
  // busiest member's activity reaches the header it hangs under.
  const opsRow = lines.find((l) => l.includes("⊟ 📦 ops"));
  assert.match(opsRow, /✕ Crashed/, "the crash three rows down surfaces on the header");
  // The `Queues` section and its one filled lane.
  const queueHeader = lines.findIndex((l) => l.startsWith("Queue "));
  assert.ok(queueHeader > 0, "the `Queues` section has a header");
  for (const label of ["Queue", "Slots", "Parallelism", "Held", "Waiting"]) {
    assert.ok(lines[queueHeader].includes(label), `the section's ${label} column`);
  }
  assert.match(lines[queueHeader + 1], /^🧵 heavy \(high\) +▮▮ ╎ ∙ +2 +2 +1/, "the `heavy` lane, held and waited on");
  // The footer, with a real hint on it.
  assert.ok(lines.slice(-3).some((l) => l.includes("? help")), "the footer legend");
});

test("the drain is its own screen", () => {
  // The same capture folded **whole**. Adversarial against the live screen in four ways at
  // once: the header reads `Quitting` off a different anchor with its own drain count, every
  // operator hint is gone (the input side refuses them, so they read as unbound), the rows are
  // a shelf of receding `Stopped` ones, and the lane is empty.
  const { board, at } = drainedBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" });
  assertGrid(rows, 100, 30);
  assertGolden("drain-100x30.txt", rows);
  const lines = screenText(rows).split("\n");
  assert.match(lines[0], /^afkd 0\.2\.123 · ■ Quitting \d+s/, "the drain owns the header, dot and all");
  const footer = lines.slice(-3).join("\n");
  for (const gone of ["s start", "x stop", "t trigger", "r restart", "Ctrl+R reload"]) {
    assert.ok(!footer.includes(gone), `${gone} is refused while draining, so it is not advertised`);
  }
  assert.ok(footer.includes("? help"), "the keys that still work are still there");
});

test("the reconcile markers ride the name", () => {
  // The other capture: a reload that leaves one service **orphaned** (its definition is gone,
  // the process is not) and one **stale** (the definition moved under a running process).
  // `snapshot.jsonl` folds to neither, so without this screen the two sigils the card names
  // are implemented and never rendered. It carries an eleventh service too — `reports`,
  // `Stopped` — so the row sequence is not the one every other golden holds.
  const { board, at } = liveBoard("reload.jsonl");
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" });
  assertGrid(rows, 100, 30);
  assertGolden("reload-100x30.txt", rows);
  const lines = screenText(rows).split("\n");
  assert.ok(lines.some((l) => l.includes("🪦spare")), "the orphan wears its tombstone, hard against the name");
  assert.ok(lines.some((l) => l.includes("🕸️noisy")), "the stale one its cobweb");
  // One marker per row and no row wearing two: `reconcile_marker` resolves the tie, it does
  // not concatenate.
  const marked = lines.filter((l) => l.includes("🪦") || l.includes("🕸️"));
  assert.equal(marked.length, 2, "two marked rows, no more");
  for (const line of marked) {
    assert.ok(!(line.includes("🪦") && line.includes("🕸️")), "a row wears one marker at most");
  }
  // No fixture folds a **poisoned** card, so the footer's `✕` note and its `POISON_RECOVERY`
  // sentence are unreached by every golden here. They are reachable only from a wire the
  // recorder never captured; saying so is cheaper than a hand-written frame the fixtures'
  // own rule forbids.
  assert.ok(
    Object.values(board.services).every((svc) => !svc.poisoned),
    "no capture folds a poisoned card — the note arm is unrendered, deliberately",
  );
});

test("a board left alone recedes what has gone quiet", () => {
  // The same live board twenty minutes on — the tab nobody closed. Every age crosses the
  // five-minute freshness window, so the `Last Activity` column recedes as a column rather
  // than a cell, the countdowns walk down, and the host-load strip ages out of its own window
  // and leaves the band empty. All of it is clock, none of it is wire: the fixture is the one
  // the overview renders, read at a later instant.
  const { board, at } = liveBoard();
  const fresh = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" });
  const aged = layout(board, { cols: 100, rows: 30, now: at + 20 * 60 * 1000, version: "0.2.123" });
  assertGrid(aged, 100, 30);
  assertGolden("aged-100x30.txt", aged);
  // The activity ages carry the recession the fresh screen's do not — the one bit
  // `liveness_cell` mints beside the number, asserted as the difference between two clocks
  // over one board rather than as a tone written down.
  const ages = (rows) =>
    rows.flatMap((row) => row.filter((c) => /^\d+[smhd]( \d+[smhd])?$/.test(c.text)).map((c) => [c.text, c.fg]));
  assert.ok(ages(fresh).some(([, fg]) => fg !== "recede"), "the fresh screen keeps its ages at full weight");
  assert.ok(
    ages(aged).filter(([text]) => text.startsWith("20m")).every(([, fg]) => fg === "recede"),
    "…and every age past the fresh window recedes",
  );
  assert.equal(screenText(aged).split("\n")[1].trim(), "", "the load strip ages out of its own window with them");
});

// --- AC2: narrowing re-lays out ----------------------------------------------------

/// The column labels the header row carries at `cols` — read off the rendered screen, so the
/// shed ladder is observed rather than restated.
function columnsAt(board, at, cols) {
  const header = screenText(layout(board, { cols, rows: 30, now: at + 1000, version: "0.2.123" })).split("\n")[3];
  return ["Service", "State", "Trigger", "Last Activity", "Next Run"].filter((l) => header.includes(l));
}

test("sheds right to left as the width falls", () => {
  const { board, at } = liveBoard();
  // The ladder's own rungs, **derived** by walking the width down and watching the header
  // rather than written down: a column widening in the Rust moves these with it.
  const rungs = [];
  let previous = null;
  for (let cols = 140; cols >= 30; cols -= 1) {
    const set = columnsAt(board, at, cols).join(",");
    if (previous !== null && set !== previous) rungs.push({ cols: cols + 1, set: previous });
    previous = set;
  }
  rungs.push({ cols: 30, set: previous });
  assert.deepEqual(
    rungs.map((r) => r.set.split(",")),
    [
      ["Service", "State", "Trigger", "Last Activity", "Next Run"],
      ["Service", "State", "Last Activity", "Next Run"],
      ["Service", "State", "Last Activity"],
      ["Service", "State"],
    ],
    "the ladder sheds Trigger, then Next Run, then Last Activity — static configuration before the live clocks",
  );
  assert.deepEqual(rungs.map((r) => r.cols), [90, 71, 60, 30], "at the rungs the reserves derive");

  // One golden per rung, each a width the ladder really changes shape at.
  let right = null;
  for (const cols of [100, 80, 60, 40]) {
    const rows = layout(board, { cols, rows: 30, now: at + 1000, version: "0.2.123" });
    assertGrid(rows, cols, 30);
    assertGolden(`overview-${cols}x30.txt`, rows);
    const lines = screenText(rows).split("\n");

    // The header sheds **right to left**: the telemetry cluster at each narrower width is a
    // suffix of the one before it, never a resliced set of segments. It is read off the
    // header row's **last cell** — the one `planHeader` composed it as — rather than scraped
    // back out of the rendered line, so an empty cluster reads as empty instead of as
    // whatever the left cluster's tail happens to look like.
    const tail = rows[0][rows[0].length - 1].text;
    const cluster = tail.trim() === "" ? "" : tail;
    if (right !== null) {
      assert.ok(right.endsWith(cluster), `the header sheds right-to-left at ${cols}: ${JSON.stringify(cluster)} is not a suffix of ${JSON.stringify(right)}`);
    }
    right = cluster;

    // Nothing clips a service name to nothing: every service still spells at least its first
    // grapheme, at every width the ladder walks.
    for (const name of Object.keys(board.services)) {
      const leaf = name.includes("::") ? name.slice(name.lastIndexOf("::") + 2) : name;
      assert.ok(lines.some((l) => l.includes(leaf)), `${name} still reads whole at ${cols}`);
    }
  }
});

test("a viewport too short for its chrome still composes a whole screen", () => {
  // The degenerate end of the height clip (there is no scroll: a cursor is the next card). At
  // 100×6 the pinned bands and the footer take everything, so the body is empty — and the
  // screen is still exactly six whole rows rather than a fault.
  const { board, at } = liveBoard();
  for (const height of [1, 4, 6, 12]) {
    const rows = layout(board, { cols: 100, rows: height, now: at + 1000, version: "0.2.123" });
    assertGrid(rows, 100, height);
  }
});

// --- AC3: a wide glyph costs the cells it costs ------------------------------------

/// The board with one service renamed — the fixture-derived board, one field moved. This is a
/// width property, not a wire behaviour, so it needs no fabricated frame.
function renamed(board, from, to) {
  const svc = { ...board.services[from], name: to };
  const services = { ...board.services, [to]: svc };
  delete services[from];
  return { ...board, services, order: board.order.map((n) => (n === from ? to : n)) };
}

/// The cumulative cell offsets a row's cells start at — the row's geometry, independent of
/// what any of them says.
function boundaries(row) {
  const out = [];
  let at = 0;
  for (const cell of row) {
    out.push(at);
    at += cell.width;
  }
  return out;
}

test("a wide name costs its cells and moves nothing", () => {
  const { board, at } = liveBoard();
  const opts = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" };
  // Two names of the **same twelve cells**: one all-ASCII, one all-wide. If the wide one were
  // measured by `length` it would cost six and the row would run short; if a cluster were
  // taken for four cells it would cost twenty-four and the row would overrun.
  assert.equal(textWidth("監視サービス"), 12);
  assert.equal(textWidth("abcdefghijkl"), 12);
  const wide = layout(renamed(board, "ops::nightly", "ops::監視サービス"), opts);
  const ascii = layout(renamed(board, "ops::nightly", "ops::abcdefghijkl"), opts);
  assertGrid(wide, 100, 30);
  assertGrid(ascii, 100, 30);

  const wideRow = wide.find((r) => r.map((c) => c.text).join("").includes("監視"));
  const asciiRow = ascii.find((r) => r.map((c) => c.text).join("").includes("abcdefghijkl"));
  assert.ok(wideRow !== undefined && asciiRow !== undefined, "both rows render");
  // The strongest form of "the row's total cell count is unchanged by the glyph": with the two
  // names cut out, the two rows are the **same string**. Every pad, every column origin and
  // every following cell is where it was.
  const strip = (row) =>
    row.map((c) => c.text).join("").replace("監視サービス", "").replace("abcdefghijkl", "");
  assert.equal(strip(wideRow), strip(asciiRow), "the wide name costs exactly the cells the ASCII one does");
  // …and the geometry itself, cell by cell: the `State` column starts on the same cell.
  const stateAt = (row) => boundaries(row)[row.findIndex((c) => c.text.startsWith("▷ Queued"))];
  assert.equal(stateAt(wideRow), stateAt(asciiRow), "the `State` column is where it always is");
  // The whole screen holds too, not just the one row: the two boards differ in one name, so
  // every other row is byte-identical.
  const others = (rows) => screenText(rows).split("\n").filter((l) => !l.includes("監視") && !l.includes("abcdefghijkl"));
  assert.deepEqual(others(wide), others(ascii), "no other row moved");

  // A VS16 sequence in a name is the other half of the class: two cells by emoji presentation
  // rather than by its base scalar, which is *one* (Ambiguous).
  const vs16 = layout(renamed(board, "ops::nightly", "ops::▪️probe"), opts);
  assertGrid(vs16, 100, 30);
});

test("the glyph vocabulary measures as the tui measures it", () => {
  // Every glyph this file and the wire can put on a row, and the width `unicode-width` 0.2
  // gives it in the terminal. A future glyph outside the modelled classes fails here rather
  // than mismeasuring a row in silence.
  for (const glyph of ["🔹", "🔸", "🔷", "🔶", "▪️", "🪦", "🕸️", "📦", "📜", "🧵", "🔒", "🧹"]) {
    assert.equal(textWidth(glyph), 2, `${glyph} is two cells`);
  }
  for (const glyph of ["●", "▷", "◎", "▶", "■", "□", "✕", "◌", "⊟", "⊞", "▮", "▯", "∙", "╎", "│", "─", "↓", "↑", "·", "…", "▁", "█"]) {
    assert.equal(textWidth(glyph), 1, `${glyph} is one cell`);
  }
  assert.equal(textWidth("監視"), 4, "CJK is two cells a glyph");
  assert.equal(textWidth("ａｂ"), 4, "so are fullwidth forms");
  assert.equal(textWidth("é"), 1, "a combining mark rides its base");
  assert.equal(textWidth("a​b"), 2, "a zero-width space takes no cell");
  assert.equal(textWidth(""), 0);
});

// --- AC4: the palette ---------------------------------------------------------------

test("the stylesheet spends only the product's palette", () => {
  const raw = readFileSync(join(HERE, "dashboard.css"), "utf8");
  // Comments first: the file's own header names the two near-twins it dropped on the way
  // across from the site (`#8c949d`, `#fcfcfc`), and a scan that counted those as colours on
  // screen would fail on the very sentence that explains why they are not.
  const css = raw.replace(/\/\*[\s\S]*?\*\//g, "");
  // The nine the terminal paints, read **out of the Rust** rather than transcribed — the same
  // relation `web/scripts/check-dist.mjs` holds for the site's window, so a rename in
  // `palette.rs` reddens here.
  const rust = readFileSync(join(REPO, "crates", "tui", "src", "palette.rs"), "utf8");
  const product = [...rust.matchAll(/Color::from_u32\(0x00([0-9a-f]{2})_([0-9a-f]{4})\)/g)].map(
    ([, hi, lo]) => `#${hi}${lo}`,
  );
  assert.equal(product.length, 9, "palette.rs's truecolor arm still spells nine hues");
  // …plus the screen they sit on and the three recession tones this page adds for the text
  // that is not a status.
  const own = ["#0d1117", "#c9d1d9", "#a0a6ae", "#777f8b"];
  const found = [...new Set([...css.matchAll(/#[0-9a-fA-F]{3,8}\b/g)].map(([hex]) => hex.toLowerCase()))].sort();
  assert.deepEqual(found, [...new Set([...product, ...own])].sort(), "the stylesheet's hexes are exactly the product's plus this page's four");

  // Every role the layout can emit is spelled, and nothing else is: a role the stylesheet
  // forgot would paint as unstyled text, and one it spells for nobody is dead weight.
  const roles = [...css.matchAll(/^\.fg-([a-z-]+) \{/gm)].map(([, role]) => role).sort();
  // Swept over **both** boards: the live one never reaches a `Starting`/`Stopping` badge, so a
  // sweep of it alone would leave `caution` looking like a rule nobody emits.
  const emitted = new Set();
  for (const { board, at } of [liveBoard(), drainedBoard()]) {
    for (const cols of [100, 80, 60, 40]) {
      for (const row of layout(board, { cols, rows: 30, now: at + 1000, version: "0.2.123" })) {
        for (const cell of row) emitted.add(cell.fg);
      }
    }
  }
  for (const role of emitted) assert.ok(roles.includes(role), `dashboard.css spells the ${role} role`);
  // …and equality the other way too, so a rule that stops being reachable is caught as much as
  // one that was never written. `selection` is a background, so it is checked as its own rule.
  assert.deepEqual([...emitted].sort(), roles, "every role the stylesheet spells is one the layout emits");
  assert.match(css, /^\.bg-selection \{/m, "the selection bar has its band");
});

// --- AC6: the layout is DOM-free ----------------------------------------------------

test("the layout names neither document nor window", () => {
  // The scan is over **code**, not prose: the comments above `netScale` say the word "window"
  // about a window of readings, and a scan that tripped on that would be a scan nobody could
  // keep. Comments are stripped first, then the two globals are looked for as identifiers.
  const code = readFileSync(join(HERE, "layout.mjs"), "utf8")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^\s*\/\/.*$/gm, "");
  for (const forbidden of ["document", "window"]) {
    const hit = code.match(new RegExp(`\\b${forbidden}\\b`));
    assert.equal(hit, null, `layout.mjs names \`${forbidden}\`: the split with paint.mjs is what makes the goldens possible`);
  }
  // …and the mechanical half of the same proof, which is the half a rename cannot dodge: it
  // really runs under node, where there is no DOM at all, and lays a real board out.
  const { board, at } = liveBoard();
  assert.equal(layout(board, { cols: 100, rows: 30, now: at }).length, 30);
  // Its partner is the one that does the DOM work, so the split is asserted from both sides.
  const painter = readFileSync(join(HERE, "paint.mjs"), "utf8");
  assert.match(painter, /document\.createElement/, "paint.mjs is where the DOM lives");
});

// --- the footer's own contract ------------------------------------------------------

/// The footer rows of a screen — everything below the last body row, which is the tail the
/// ladder painted.
function footerOf(rows, height) {
  return screenText(rows).split("\n").slice(-height);
}

test("the footer advertises no unpressable key", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 120, rows: 30, now: at + 1000, version: "0.2.123" });
  const footer = footerOf(rows, 3).join("\n");
  // A bound key the selection cannot take **holds its slot**: the first row is `janitor`,
  // which is `Stopped`, so `s start` is live and `x stop`/`t trigger` are there but gated.
  for (const cell of ["s start", "x stop", "t trigger", "r restart", "o output", "? help", "q quit"]) {
    assert.ok(footer.includes(cell), `the footer carries ${cell}`);
  }
  // …while an **unbound** action renders nothing at all. The drain is the reachable case: it
  // empties the chord list for every operator verb, and the cells vanish rather than dimming.
  const drained = drainedBoard();
  const quitFooter = footerOf(
    layout(drained.board, { cols: 120, rows: 30, now: drained.at + 1000, version: "0.2.123" }),
    3,
  ).join("\n");
  for (const cell of ["s start", "x stop", "t trigger", "r restart", "+ widen", "- narrow", "Ctrl+R reload"]) {
    assert.ok(!quitFooter.includes(cell), `${cell} is unbound while draining, so it is absent — not dim`);
  }
});

// --- the look: what is dim, what is bold, and where the band falls -------------------
//
// The goldens carry all three as planes, so every one of these is guarded there too. They are
// written out as well because a plane diff says *a* cell moved and these say **which** rule
// broke — and because three of them are behaviours the card names in its own words.

test("a gated hint is dim and a live one is not", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 120, rows: 30, now: at + 1000, version: "0.2.123" });
  // The `legend`-toned label cell of a footer hint, with the `bright` key glyph that precedes
  // it — the two cells `hintRowCells` mints from one hint, which must agree on the bit.
  const hint = (label) => {
    for (const row of rows) {
      const at = row.findIndex((c) => c.fg === "legend" && c.text === label);
      if (at === -1) continue;
      const key = row.slice(0, at).reverse().find((c) => c.fg === "bright");
      return { label: row[at], key };
    }
    return undefined;
  };
  // The cursor is on `janitor`, which is `Stopped`: `s` can act on it; `x` and `t` are bound
  // and cannot. That is the whole gated/unbound distinction in one row.
  for (const [label, dim] of [["start", false], ["stop", true], ["trigger", true]]) {
    const cells = hint(label);
    assert.ok(cells !== undefined, `the footer carries ${label}`);
    assert.equal(cells.label.dim, dim, `${label} is ${dim ? "gated, so dim" : "live, so lit"} against a Stopped row`);
    assert.equal(cells.key.dim, dim, `${label}'s key glyph carries the same weight its label does`);
  }
  // …and the shape is the point: gated or live, the cell is the same width in the same slot.
  assert.equal(hint("stop").label.width, textWidth("stop"));
});

test("the selection bar sits over the cursor's row and follows it", () => {
  const { board, at } = liveBoard();
  const banded = (selected) => {
    const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", selected });
    assertGrid(rows, 100, 30);
    const at_ = rows.findIndex((row) => row.some((c) => c.bg === "selection"));
    assert.notEqual(at_, -1, `some row carries the band at selected=${selected}`);
    assert.equal(
      rows.filter((row) => row.some((c) => c.bg === "selection")).length,
      1,
      "exactly one row is banded",
    );
    // The band is the **whole** row, edge to edge: a bar that stopped at the last glyph would
    // leave the cursor's row half-lit at every width.
    assert.ok(rows[at_].every((c) => c.bg === "selection"), "every cell of the banded row carries it");
    return at_;
  };
  const first = banded(0);
  // The first body row is the one below the pinned chrome — title, strip, spacer, columns.
  assert.equal(first, 4, "the cursor's row is the first body row");
  assert.equal(banded(2), first + 2, "the band moves with the cursor, row for row");
  // Out of range is a real arm, not a fault: no band at all, and the screen still composes.
  const none = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", selected: 999 });
  assertGrid(none, 100, 30);
  assert.ok(none.every((row) => row.every((c) => c.bg === null)), "a cursor off the list bands nothing");
});

test("the load strip lights its newest reading and recedes the history", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" });
  const strip = rows[1];
  const isLane = (c) => c.text !== "" && [...c.text].every((ch) => TREND_LADDER.includes(ch));
  const lit = strip.map((c, i) => [c, i]).filter(([c]) => isLane(c) && !c.dim);
  assert.equal(lit.length, 4, "four lanes — cpu, mem, net down, net up — each with one lit cell");
  for (const [c, i] of lit) {
    assert.equal(c.width, 1, "the lit cell is the newest reading alone");
    assert.ok(strip[i - 1].dim, "…and everything older than it recedes");
  }
  // The other half of the recession: the labels and connectors are dim, the values are not.
  const label = strip.find((c) => c.text === "CPU ");
  assert.ok(label !== undefined && label.dim, "the lane's label recedes");
  // The column header is the page's one bold band — the labels, nothing else on the row.
  const header = rows[3];
  for (const c of header) {
    assert.equal(c.bold, c.text.trim() !== "", `${JSON.stringify(c.text)} is bold exactly when it is a label`);
  }
});

test("the key glyphs are canonical", () => {
  // The javascript restatement of `legend::assert_canonical_key_glyphs`: named and modifier
  // keys take the help overlay's Title-case spelling, and no key cell wears a bracket — keys
  // whiten structurally, never with a `[…]` delimiter.
  const { board, at } = liveBoard();
  const seen = new Set();
  for (const cols of [120, 100, 80]) {
    for (const line of footerOf(layout(board, { cols, rows: 30, now: at + 1000, version: "0.2.123" }), 3)) {
      for (const word of line.split(/\s+/)) if (word !== "") seen.add(word);
    }
  }
  const text = [...seen].join(" ");
  for (const bad of ["ctrl-", "[", "]"]) {
    assert.ok(!text.includes(bad), `the footer spells no ${bad}: ${text}`);
  }
  for (const bad of ["enter", "esc", "pgup", "pgdn"]) {
    assert.ok(!seen.has(bad), `${bad} takes its Title-case form`);
  }
  assert.ok(seen.has("Enter"), "…which is what it takes: Enter");
  assert.ok(seen.has("Ctrl+R"), "and Ctrl+R");
});

// --- the `/` needle ------------------------------------------------------------------

test("a needle filters the board and says so in the footer", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", filter: "ops" });
  assertGrid(rows, 100, 30);
  assertGolden("filtered-100x30.txt", rows);
  const text = screenText(rows);
  assert.ok(text.includes("⊟ 📦 ops"), "the surviving group keeps its header");
  assert.ok(!text.includes("janitor"), "a non-matching root row is gone");
  assert.ok(text.includes("/ops (esc)"), "and the footer carries the clear affordance at its own glyph");
});

// --- the shared formatter -------------------------------------------------------------

test("durations spell the two largest units", () => {
  // `layout::format_elapsed`'s own tiers, including the whole-unit case that drops its
  // trailing zero — the one formatter the header, a badge, an age and a countdown all read.
  assert.equal(formatElapsed(0), "0s");
  assert.equal(formatElapsed(45_000), "45s");
  assert.equal(formatElapsed(62_000), "1m 2s");
  assert.equal(formatElapsed(60_000), "1m");
  assert.equal(formatElapsed(14_580_000), "4h 3m");
  assert.equal(formatElapsed(3_600_000), "1h");
  assert.equal(formatElapsed(273_600_000), "3d 4h");
});

// --- the cursor ------------------------------------------------------------------------
//
// The cursor's three surfaces: the band it wears, the footer arm it selects, and the rows a
// fold hides under it. One golden per row **kind**, because each selects a different footer
// arm and a screen that agreed with itself on one of them would say nothing about the others.

test("the cursor on a service row mid-board", () => {
  // Row 7 is `archivist` — a root peer below the whole `ops` subtree, so the band sits past
  // two group headers and the nesting, where an off-by-one in the row sequence would show.
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", selected: 7 });
  assertGrid(rows, 100, 30);
  assertGolden("cursor-service-100x30.txt", rows);
  const lines = screenText(rows).split("\n");
  const banded = rows.findIndex((row) => row.some((c) => c.bg === "selection"));
  assert.ok(lines[banded].includes("archivist"), "the band is on the row the index names");
  // `archivist` is `Idle`, so the footer's four operator cells read off *its* badge: stop,
  // trigger and restart are live, start is not.
  const footer = footerOf(rows, 3).join("\n");
  for (const cell of ["s start", "x stop", "t trigger", "r restart"]) {
    assert.ok(footer.includes(cell), `the footer carries ${cell}`);
  }
});

test("the cursor on a group header, with its subtree folded away", () => {
  // The fold and the group footer arm in one screen: `ops` is collapsed, so its chevron flips,
  // its four members are gone, and the footer names `expand` rather than `collapse` — the verb
  // the key would do **next**.
  const { board, at } = liveBoard();
  const rows = layout(board, {
    cols: 100,
    rows: 30,
    now: at + 1000,
    version: "0.2.123",
    selected: 1,
    collapsed: new Set(["ops"]),
  });
  assertGrid(rows, 100, 30);
  assertGolden("cursor-group-100x30.txt", rows);
  const text = screenText(rows);
  assert.ok(text.includes("⊞ 📦 ops"), "the folded header wears the squared plus");
  assert.ok(!text.includes("⊟ 📦 ops"), "…and not the squared minus");
  for (const gone of ["nightly", "sleeper", "broken", "vacuum", "⊟ 📦 db"]) {
    assert.ok(!text.includes(gone), `${gone} is folded away, header and all`);
  }
  assert.ok(text.includes("janitor") && text.includes("archivist"), "the root peers stay");
  // The group row still carries its transitive rollup: a crash three levels down surfaces on
  // the header that is now hiding it, which is the whole point of folding transitively.
  const header = screenText(rows).split("\n").find((l) => l.includes("⊞ 📦 ops"));
  assert.match(header, /✕ Crashed/, "the hidden crash still reaches the header");
  const footer = footerOf(rows, 3).join("\n");
  assert.ok(footer.includes("expand"), "the collapse hint names the verb the key would do next");
  assert.ok(!footer.includes("collapse"), "…and not the one it just did");
});

test("the cursor on a queue lane", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", selected: 14 });
  assertGrid(rows, 100, 30);
  assertGolden("cursor-lane-100x30.txt", rows);
  const banded = rows.findIndex((row) => row.some((c) => c.bg === "selection"));
  assert.ok(screenText(rows).split("\n")[banded].includes("🧵 heavy"), "the band is on the lane");
  // A lane row's footer is the `queues` scope's two cells and no operator verb: a lane is not a
  // card, and `h`/`l` there are the lane's own width keys.
  const footer = footerOf(rows, 3).join("\n");
  for (const cell of ["h narrow", "l widen"]) assert.ok(footer.includes(cell), `the footer carries ${cell}`);
  for (const gone of ["s start", "t trigger", "r restart"]) {
    assert.ok(!footer.includes(gone), `${gone} needs a card, so a lane row does not advertise it`);
  }
});

test("the body scrolls to keep the cursor on screen", () => {
  // A viewport far shorter than the board. There is no wrap and no jump: the offset is the
  // minimal clamp, so walking down pushes one row at a time and `g` comes straight back.
  const { board, at } = liveBoard();
  const listRows = visibleRows(board, {});
  // The height is read **at the selection**, because the footer's own ladder is
  // selection-aware: a lane row's legend is one line where a service row's is three, and a
  // scroll clamped against the wrong height would leave the cursor half a screen away.
  const heightAt = (selected) => bodyHeight(board, { cols: 100, rows: 12, now: at, selected });
  assert.ok(heightAt(0) > 0 && heightAt(0) < listRows.length, "a 12-row viewport is shorter than the board");
  const shown = (selected, offset) =>
    screenText(layout(board, { cols: 100, rows: 12, now: at + 1000, version: "0.2.123", selected, offset }))
      .split("\n")
      .slice(4, 4 + heightAt(selected));
  // `G`: the last row is on screen, the first is not.
  const last = listRows.length - 1;
  const bottom = shown(last, scrollOffset(0, last, listRows.length, heightAt(last)));
  assert.ok(bottom.some((l) => l.includes("🧵 heavy")), "the last row is visible after G");
  assert.ok(!bottom.some((l) => l.includes("janitor")), "…and the first has scrolled off");
  // `g`: straight back to the top.
  const top = shown(0, scrollOffset(scrollOffset(0, last, listRows.length, heightAt(last)), 0, listRows.length, heightAt(0)));
  assert.ok(top.some((l) => l.includes("janitor")), "g brings the first row back");
  // The offset never strands the body: at every cursor position the selected row is on screen
  // and the screen is still a whole 12 rows.
  let offset = 0;
  for (let selected = 0; selected < listRows.length; selected += 1) {
    offset = scrollOffset(offset, selected, listRows.length, heightAt(selected));
    const rows = layout(board, { cols: 100, rows: 12, now: at + 1000, version: "0.2.123", selected, offset });
    assertGrid(rows, 100, 12);
    assert.equal(
      rows.filter((row) => row.some((c) => c.bg === "selection")).length,
      1,
      `the cursor's row is on screen at selected=${selected}`,
    );
  }
  // A stale offset from a taller viewport is clamped into range rather than blanking the body:
  // it lands on the last screenful, not past the end. (The cursor is never stranded there in
  // practice — `scrollOffset` pulls the offset back down to a selection above it — but the
  // layout takes the number it is handed, and the number can be stale by a resize.)
  const stale = layout(board, { cols: 100, rows: 12, now: at + 1000, version: "0.2.123", selected: 0, offset: 999 });
  assertGrid(stale, 100, 12);
  assert.ok(screenText(stale).includes("🧵 heavy"), "the last screenful, not an empty body");
  assert.equal(scrollOffset(999, 0, listRows.length, heightAt(0)), 0, "…and a real scroll would have come back");
});

// --- the confirm modal ---------------------------------------------------------------------

/// The board `noise.jsonl` folds to at its first `service_stopping` — the only fixture that
/// carries one before the drain. `janitor` is wedged in `Stopping` there, which is the one
/// badge that makes the force gate reachable.
function wedgedBoard() {
  let board = seed({ logLines: 2000 });
  let at = BASE;
  let n = 0;
  for (const line of readFileSync(join(HERE, "fixtures", "noise.jsonl"), "utf8").split("\n")) {
    if (line === "") continue;
    board = fold(board, JSON.parse(line), (at += STEP));
    n += 1;
    if (n >= 25) break;
  }
  return { board, at };
}

test("the force modal states what force does before it asks", () => {
  const { board, at } = wedgedBoard();
  assert.equal(board.services["janitor"].badge, "Stopping", "the capture's own wedged service");
  const confirm = { verb: "force", targets: ["janitor"], skipped: 0 };
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", confirm });
  assertGrid(rows, 100, 30);
  assertGolden("force-100x30.txt", rows);
  const text = screenText(rows);
  // The title is Sentence case (§8) and the body asks with the `stuck` qualifier that only
  // fits there.
  assert.ok(text.includes("┌ Force-stop "), "the title is Sentence case, in the border");
  assert.ok(text.includes("Force-stop 1 stuck service?"), "the body asks, singular");
  assert.ok(text.includes("🧹 janitor"), "the target is listed with its own status icon");
  // **Both** honesty halves, before the `y`: what force ends, and what it leaves behind. A
  // future edit that drops either fails here.
  assert.ok(text.includes("Force kills the running work & abandons the wedged thread"), "the reach");
  assert.ok(text.includes("The service reclaims once the abandoned thread finishes"), "the limitation");
  // The hints are the `confirm` scope's own glyphs, unbracketed like every other hint on the
  // page, and the accept label is the bare verb.
  assert.ok(text.includes("y force  n cancel"), "y force  n cancel");
  assert.ok(!text.includes("[y]") && !text.includes("[n]"), "no bracket dresses a key");
});

test("the fan-out modal names its verb, its count and what it skipped", () => {
  // The four operator gates share one body, so the verb picks two words and the four modals
  // cannot drift apart. Driven over a group whose members are a mix, so `skipped` is real.
  const { board, at } = liveBoard();
  const members = board.order.map((n) => board.services[n]).filter((s) => s.group.startsWith("ops"));
  // `stop` is the verb this capture really splits on: three members are live and one is
  // `Crashed`, which `can_stop` refuses — so the modal has both a list and a skipped count.
  const eligible = members.filter((s) => s.badge !== "Crashed").map((s) => s.name);
  assert.ok(eligible.length > 1 && eligible.length < members.length, "a real mix of eligible and not");
  const confirm = { verb: "stop", targets: eligible, skipped: members.length - eligible.length };
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", confirm });
  assertGrid(rows, 100, 30);
  assertGolden("fanout-100x30.txt", rows);
  const text = screenText(rows);
  assert.ok(text.includes(`┌ Stop ${eligible.length} services? `), "the title asks, and its count is the list's");
  for (const name of eligible) assert.ok(text.includes(name), `${name} is listed`);
  assert.ok(text.includes(`${confirm.skipped} skipped`), "the ineligible are counted, not silently dropped");
  assert.ok(text.includes("y stop  n cancel"), "the accept label is the bare verb");
  // The name cap lives in the **content**, not in the clipping: past ten targets the modal says
  // how many it did not list.
  const many = Array.from({ length: 14 }, (_, i) => `service-${i}`);
  const capped = screenText(
    layout(board, {
      cols: 100,
      rows: 30,
      now: at + 1000,
      version: "0.2.123",
      confirm: { verb: "start", targets: many, skipped: 0 },
    }),
  );
  assert.ok(capped.includes("service-9"), "the tenth is listed");
  assert.ok(!capped.includes("service-10"), "the eleventh is not");
  assert.ok(capped.includes("… and 4 more"), "…and is counted instead");
  assert.ok(capped.includes("┌ Start 14 services? "), "the title's count is the whole set, not the shown one");
});

// --- the `?` overlay -------------------------------------------------------------------------

test("the overlay lists every bound action, grouped by scope", () => {
  // At a viewport that can hold the whole table. 51 rows of prose across eight scopes needs
  // room; the next test is what happens when there is none.
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 160, rows: 44, now: at + 1000, version: "0.2.123", help: true });
  assertGrid(rows, 160, 44);
  assertGolden("help-160x44.txt", rows);
  const text = screenText(rows);
  // Every scope heading and every one of the 51 rows, once, in **reading order** — each
  // category runs down its own column and the columns sit side by side, so the sequence is
  // read column by column rather than line by line. Read off the *cells*, because two scopes
  // may legitimately spell one row the same way (`g Top` is both `output.tree.first` and
  // `output.log.top`) and a substring scan would count one twice and the other never.
  const expected = [];
  for (const scope of SCOPES) {
    expected.push(scope);
    for (const row of DEFAULT_KEYS.filter((r) => r.scope === scope)) {
      expected.push(`${all(idOf(row))} ${DESCRIPTIONS[idOf(row)]}`);
    }
  }
  assert.deepEqual(overlayReading(rows), expected, "every scope and every row, once, in table order");
  assert.equal(overlayRows(rows).length, DEFAULT_KEYS.length, "all 51 rows are laid out");
  assert.ok(!text.includes("more keys"), "nothing was shed at this size");
  // The unhandled rows are dim and carry their reason; the handled ones are not dim.
  const dimOf = (label) => {
    for (const row of rows) {
      const at_ = row.findIndex((c) => c.fg === "legend" && c.text === label);
      if (at_ !== -1) return row[at_].dim;
    }
    return null;
  };
  assert.equal(dimOf("Move selection up"), false, "a key this page takes is lit");
  assert.equal(dimOf("Info view"), false, "…including the one that opens the info page");
  assert.equal(dimOf("Output"), false, "…and the one that opens the run view");
  assert.equal(dimOf("Toggle collapse / output"), false, "…and a run-view pane key");
  assert.equal(dimOf("Widen the service's lane"), true, "one it has no surface for is dim");
  // …and the reason is beside it, once per scope where a whole scope shares one.
  assert.ok(text.includes("Widen the service's lane — The relay has no lane verb"), "the per-row reason");
  assert.ok(text.includes("queues — The relay has no lane verb"), "the per-scope one, on the heading");
  // The run view's own note is **gone**: the three `output` scopes are handled now, so a line
  // saying this page has no run view would be an explanation of nothing.
  assert.ok(!text.includes("No run view on this page"), "the output scopes' old note is gone");
  for (const scope of ["output", "output.tree", "output.log"]) {
    assert.ok(overlayReading(rows).includes(scope), `the ${scope} heading stands bare`);
  }
  // The two page-level facts the card requires.
  assert.ok(text.includes("a rebound `keys { … }` block is not mirrored here"), "the rebind caveat");
  assert.ok(text.includes("Ctrl+R is afkd's reload"), "the interception, stated where the keys are");
  // The partial actions carry their boundary rather than reading as fully live.
  assert.ok(text.includes("Toggle group — Groups only — no activity peek"));
  assert.ok(text.includes("Info view — Services only — a group has no info page"));
  assert.ok(text.includes("Output — Services only — a group has no run"));
  // The `info` scope is **handled** now, so neither its heading nor its row carries a reason —
  // the note that said this page had no info view would be an explanation of nothing.
  assert.ok(!text.includes("No info view on this page"), "the info scope's old note is gone");
  assert.match(overlayReading(rows).find((e) => e === "info") ?? "", /^info$/u, "its heading stands bare");
  // Two rows spell `Back to list` — `output.back` and `info.back` — and both are lit now that
  // both pages exist. Counted rather than looked up by name, because `dimOf` finds whichever
  // sits higher on the screen.
  const backs = rows.flatMap((row) => row.filter((c) => c.fg === "legend" && c.text === "Back to list"));
  assert.deepEqual(backs.map((c) => c.dim), [false, false], "both backs are lit");
});

test("an overlay too big for the viewport sheds and says how much", () => {
  // The honest failure. 51 rows of prose do not fit 100×30 at any rung, so the overlay shows
  // the columns that fit and states the count it could not — never quietly claiming to be the
  // whole keymap when a column of it is off the edge.
  const { board, at } = liveBoard();
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", help: true });
  assertGrid(rows, 100, 30);
  const text = screenText(rows);
  const shed = text.match(/… and (\d+) more keys — widen the viewport/);
  assert.notEqual(shed, null, "it says how many it could not lay out");
  assert.equal(overlayRows(rows).length + Number(shed[1]), DEFAULT_KEYS.length, "shown + shed is the whole table");
  // The preamble survives the shed: the two facts are page-level and are not one of the rows.
  assert.ok(text.includes("Ctrl+R is afkd's reload"));
  // A wider viewport sheds strictly less — the ladder goes one way.
  const wider = screenText(layout(board, { cols: 130, rows: 34, now: at + 1000, version: "0.2.123", help: true }));
  const widerShed = wider.match(/… and (\d+) more keys/);
  assert.ok(widerShed === null || Number(widerShed[1]) < Number(shed[1]), "a bigger viewport sheds less");
});

// --- §8: what a popup owes the screen behind it ------------------------------------------------

test("a popup recedes the frame behind it and changes nothing else", () => {
  // The differential: the same board, rendered with and without the overlay. Every cell outside
  // the popup must differ in exactly one bit — `dim` — and a bold cell must lose its weight
  // rather than carry both, which the golden planes could not spell.
  const { board, at } = liveBoard();
  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", selected: 3 };
  const plain = layout(board, options);
  const withPopup = layout(board, { ...options, confirm: { verb: "force", targets: ["janitor"], skipped: 0 } });
  assertGrid(withPopup, 100, 30);
  // The rows above and below the box are the backdrop whole, so they are the clean comparison.
  const boxed = withPopup.map((row) => row.some((c) => c.fg === "recede" && c.text.startsWith("│")));
  let compared = 0;
  withPopup.forEach((row, i) => {
    if (boxed[i] || row.some((c) => c.text.includes("┌") || c.text.includes("└"))) return;
    compared += 1;
    assert.deepEqual(
      row.map((c) => ({ ...c, dim: null, bold: null })),
      plain[i].map((c) => ({ ...c, dim: null, bold: null })),
      `row ${i} is the same cells behind the popup`,
    );
    for (const c of row) assert.equal(c.dim, true, `row ${i}: every backdrop cell recedes`);
    for (const c of row) assert.equal(c.bold, false, `row ${i}: a receded cell is dim, never dim and bold`);
  });
  assert.ok(compared >= 4, `the differential really compared some rows (${compared})`);
  // The column header is the page's one bold band, and it is one of the rows above the box —
  // so "a bold cell recedes to dim alone" is exercised rather than assumed.
  assert.ok(plain[3].some((c) => c.bold), "the column header is bold on a bare frame");
});

test("every popup owns exactly one gutter row and two gutter columns at each end", () => {
  // §8's gutter, over the inputs that break a hand-padded one: a very long name, a wide-CJK
  // one, and one carrying a combining mark. The rule is that no call site pads its own body —
  // the frame does — so the geometry must not move with the content.
  //
  // The box is found by its **cells**, not by scanning for a corner glyph: a tree connector
  // spells `└─ ` too, and a text scan would find the `ops::db` row before the border.
  const { board, at } = liveBoard();
  const names = [
    "ops::a-very-long-service-name-that-runs-past-any-reasonable-column",
    "ops::監視サービス",
    "ops::café́",
    "ops::x",
  ];
  for (const name of names) {
    const moved = renamed(board, "ops::nightly", name);
    const confirm = { verb: "fire", targets: [name], skipped: 0 };
    const rows = layout(moved, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", confirm });
    assertGrid(rows, 100, 30);
    const corner = (glyph) => rows.findIndex((row) => row.some((c) => c.fg === "recede" && c.text === glyph));
    const top = corner("┌");
    const bottom = corner("└");
    assert.ok(top !== -1 && bottom > top, `${name}: the box has both borders`);
    // The body rows sit between them, bounded by the two `│` cells. Sliced in **cells**, so a
    // wide grapheme inside the box is measured the way the box was sized.
    const body = rows.slice(top + 1, bottom).map((row) => {
      let atCell = 0;
      const edges = [];
      for (const c of row) {
        if (c.fg === "recede" && c.text === "│") edges.push(atCell);
        atCell += c.width;
      }
      assert.equal(edges.length, 2, `${name}: a body row has exactly two vertical borders`);
      return { row, left: edges[0] + 1, right: edges[1] };
    });
    assert.ok(body.length >= 3, `${name}: a body with a gutter at each end and content between`);
    /// One body row's inside, as text.
    const inside = ({ row, left, right }) => {
      let atCell = 0;
      let out = "";
      for (const c of row) {
        if (atCell >= left && atCell + c.width <= right) out += c.text;
        atCell += c.width;
      }
      return out;
    };
    assert.equal(inside(body[0]).trim(), "", `${name}: one blank row inside the top border`);
    assert.equal(inside(body[body.length - 1]).trim(), "", `${name}: and one inside the bottom`);
    assert.notEqual(inside(body[1]).trim(), "", `${name}: exactly one — the row below it is content`);
    assert.notEqual(inside(body[body.length - 2]).trim(), "", `${name}: and the row above the bottom is too`);
    // Two blank columns inside each vertical border, on every content row — the gutter the
    // frame owns, which is why a name of any width leaves it alone.
    for (const line of body) {
      const text = inside(line);
      if (text.trim() === "") continue;
      assert.equal(text.slice(0, 2), "  ", `${name}: two gutter columns on the left`);
      assert.equal(text.slice(-2), "  ", `${name}: two on the right`);
    }
    // …and the name really is inside, whole: the cap is about count, not about width.
    assert.ok(body.some((line) => inside(line).includes(name)), `${name} is listed whole`);
  }
});

/// The overlay read in its own reading order: each category runs **down** a column and the
/// columns sit side by side, so the sequence is `(column, row)` rather than `(row, column)`.
/// Returns one string per entry — a scope heading, or `keys label` for a row — which is the
/// projection a table-order assertion can be written against.
function overlayReading(rows) {
  const wanted = new Set(Object.values(DESCRIPTIONS));
  const found = [];
  rows.forEach((row, r) => {
    let col = 0;
    for (let i = 0; i < row.length; i += 1) {
      const c = row[i];
      // A heading: the one bold cell the overlay mints. The backdrop's own bold column header
      // cannot collide, because a receded cell loses its weight.
      if (c.bold && c.fg === "bright" && c.text.trim() !== "") found.push({ col, r, text: c.text.trim() });
      else if (
        c.fg === "bright" &&
        row[i + 1]?.text === " " &&
        row[i + 2]?.fg === "legend" &&
        wanted.has(row[i + 2].text)
      ) {
        found.push({ col, r, text: `${c.text.trim()} ${row[i + 2].text}` });
      }
      col += c.width;
    }
  });
  found.sort((a, b) => (a.col === b.col ? a.r - b.r : a.col - b.col));
  return found.map((f) => f.text);
}

/// The `?` overlay's rows as `{keys, label}` pairs, read off the rendered cells: a bright key
/// cell, the one joining space, and the legend label beside it — the three `overlayCells` mints
/// from one row. Only pairs whose label is a **description** count, so the footer's own
/// lower-case hints below the popup cannot drift into the tally.
function overlayRows(rows) {
  const wanted = new Set(Object.values(DESCRIPTIONS));
  const out = [];
  for (const row of rows) {
    for (let i = 0; i + 2 < row.length; i += 1) {
      if (row[i].fg !== "bright" || row[i + 1].text !== " " || row[i + 2].fg !== "legend") continue;
      if (!wanted.has(row[i + 2].text)) continue;
      out.push({ keys: row[i].text.trim(), label: row[i + 2].text });
    }
  }
  return out;
}

test("the key glyphs are canonical on every surface, not only the footer", () => {
  // `legend::assert_canonical_key_glyphs`, widened to the two surfaces this card adds. A modal
  // and an overlay are full of key cells, and a bracketed or lower-cased one there would be as
  // wrong as in the footer.
  const { board, at } = wedgedBoard();
  const screens = [
    layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", confirm: { verb: "force", targets: ["janitor"], skipped: 0 } }),
    layout(board, { cols: 160, rows: 44, now: at + 1000, version: "0.2.123", help: true }),
  ];
  for (const rows of screens) {
    // The key cells **structurally** — `bright`-toned runs, which is what the whitening is —
    // rather than words scraped back out of the rendered line.
    const keys = rows.flat().filter((c) => c.fg === "bright" && c.text.trim() !== "").map((c) => c.text.trim());
    assert.ok(keys.length > 0, "the surface carries key cells");
    for (const key of keys) {
      for (const bad of ["[", "]", "ctrl-"]) {
        assert.ok(!key.includes(bad), `${JSON.stringify(key)} spells no ${bad}`);
      }
      for (const bad of ["enter", "esc", "pgup", "pgdn", "space", "tab", "backspace"]) {
        assert.ok(!key.split("/").includes(bad), `${JSON.stringify(key)} takes ${bad}'s Title-case form`);
      }
    }
  }
  // …and the overlay really does carry the Title-case ones, so the scan above is not vacuous.
  const overlay = screenText(screens[1]);
  for (const glyph of ["Enter/Space", "Ctrl+R", "PgUp", "PgDn", "Esc", "Tab", "↑", "↓", "←", "→"]) {
    assert.ok(overlay.includes(glyph), `the overlay spells ${glyph}`);
  }
  // A dialog's title is Sentence case: a leading capital and no Title Case run.
  const titles = screenText(screens[0]).split("\n").concat(overlay.split("\n"))
    .map((l) => l.match(/┌ (.+?) ─/))
    .filter((m) => m !== null)
    .map((m) => m[1]);
  assert.deepEqual(titles, ["Force-stop", "Help"], "the two titles on screen");
  for (const title of titles) assert.match(title, /^[A-Z][^A-Z]*$/, `${title} is Sentence case`);
});

// --- the flash -------------------------------------------------------------------------------

test("a flash owns the footer for four seconds and reverts byte for byte", () => {
  const { board, at } = liveBoard();
  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", selected: 7 };
  const before = layout(board, options);
  const flashed = layout(board, { ...options, flash: "Fired ops::nightly" });
  assertGrid(flashed, 100, 30);
  // The flash **is** the footer, at precedence 2: one line, replacing the contextual legend
  // rather than sitting under it.
  const footer = footerOf(flashed, 1)[0];
  assert.ok(footer.startsWith("Fired ops::nightly"), "the flash is the footer's own line");
  assert.ok(!screenText(flashed).includes("? help"), "…and the contextual hints are not beside it");
  // A footer of one row leaves the body two rows taller, which is why expiry has to revert
  // exactly rather than approximately.
  assert.deepEqual(
    screenText(layout(board, { ...options, flash: null })),
    screenText(before),
    "with no flash the screen is the one it was before, byte for byte",
  );
  // Precedence 1 beats it: while typing, the prompt owns the footer.
  const typing = layout(board, { ...options, flash: "Fired ops::nightly", typing: "night" });
  assertGrid(typing, 100, 30);
  const prompt = footerOf(typing, 1)[0];
  assert.ok(prompt.startsWith("/ filter: night_"), "the prompt, with its own key glyph and its caret");
  assert.ok(!prompt.includes("Fired"), "…and the flash is not on screen at all");
  // An overlong message is cut to the row by **measured width**, not by UTF-16 unit: a daemon's
  // refusal is its own sentence and can carry a wide grapheme at the edge.
  const long = `監視サービス ${"x".repeat(200)}`;
  const cut = layout(board, { ...options, flash: long });
  assertGrid(cut, 100, 30);
  assert.ok(footerOf(cut, 1)[0].startsWith("監視サービス"), "the wide head survives");
  // …and one made **entirely** of wide graphemes, so the cut lands mid-cluster rather than
  // conveniently between two.
  const wide = layout(board, { ...options, flash: "監".repeat(120) });
  assertGrid(wide, 100, 30);
});

test("a flash and a filter affordance are the two the footer arms on", () => {
  // The `Active` affordance is precedence 3's, and a live flash outranks it — so a needle set
  // while a flash is up does not put two answers on one row.
  const { board, at } = liveBoard();
  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", filter: "ops" };
  assert.ok(screenText(layout(board, options)).includes("/ops (esc)"), "the affordance, with no flash");
  const both = layout(board, { ...options, flash: "Starting archivist" });
  assertGrid(both, 100, 30);
  assert.equal(footerOf(both, 1)[0].trim(), "Starting archivist", "the flash alone");
});

// --- the info page ------------------------------------------------------------------------
//
// `i` on a service opens its detail page: the five `docs/tui-style.md` §1 sections laid into a
// responsive grid over the board the fold already holds. Every board below is a fixture's, and
// the two cases no capture carries are made by **moving one field** on a folded board — a width
// or a wire-skew property, not a wire behaviour, which is the precedent `renamed()` sets above.

/// The info page's options at a viewport — one description of what the tab is looking at, the
/// same object the shell threads through `layout()` and `infoScrollMax()` alike.
function infoAt(name, cols, rows, at, extra = {}) {
  return { cols, rows, now: at + 1000, version: "0.2.123", info: name, infoOffset: 0, ...extra };
}

/// The board with one service's fields moved — a one-field edit on a fixture-folded board, the
/// way `renamed()` makes a width case: these are wire *skews* (a level the daemon did not send,
/// a kind outside the expansion set), not behaviours the fold would produce differently.
function moved(board, name, patch) {
  return { ...board, services: { ...board.services, [name]: { ...board.services[name], ...patch } } };
}

/// The cell a row starts at exactly `x` cells in, or `null`. Read off the composed row rather
/// than scraped back out of its text, so a pad and a value are distinguishable.
function cellAt(row, x) {
  let at = 0;
  for (const c of row) {
    if (at === x) return c;
    if (at > x) return null;
    at += c.width;
  }
  return null;
}

/**
 * The whole value the page spells for `label`, reassembled from the lines it wrapped onto: the
 * first line's value cell plus every continuation below it, which is the row whose label column
 * is blank and whose value column is not.
 *
 * This is what makes "nothing is clipped" assertable. Comparing the reassembly against the
 * model's own string catches a lost tail, a dropped continuation and an ellipsis alike —
 * whereas a golden regenerated over a page that lost half a value would agree with itself.
 */
function valueOf(rows, label) {
  for (let r = 0; r < rows.length; r += 1) {
    let x = 0;
    for (const c of rows[r]) {
      if (!(c.dim && c.text.startsWith(label) && c.text.trimEnd() === label)) {
        x += c.width;
        continue;
      }
      const head = cellAt(rows[r], x + c.width);
      // The badge glyph rides its own styled cell ahead of the value on the one accented row.
      const glyphW = head !== null && head.width === 2 && head.text.endsWith(" ") ? 2 : 0;
      const valueX = x + c.width + glyphW;
      const parts = [cellAt(rows[r], valueX)?.text ?? ""];
      for (let n = r + 1; n < rows.length; n += 1) {
        const pad = cellAt(rows[n], x);
        const next = cellAt(rows[n], valueX);
        if (pad === null || next === null || pad.text.trim() !== "") break;
        parts.push(next.text);
      }
      return parts.join(" ");
    }
  }
  return null;
}

/// Every `(label, value)` the page's own model holds — `InfoView::rows`, the flat cell stream a
/// `Pair`'s two scalars are found in exactly like a `Field`'s.
function modelCells(board, options) {
  return infoView(board, options).sections.flatMap((s) =>
    s.rows.flatMap((r) =>
      r.kind === "field" ? [[r.label, r.value]] : r.cells.map((c) => [c.label, c.value]),
    ),
  );
}

/// Whitespace-blind equality: a hard break inside one over-long token puts a line end where
/// there was no space, so the reassembly is compared on what it *says*, not where it broke.
function sameText(a, b) {
  return a.replace(/\s+/gu, "") === b.replace(/\s+/gu, "");
}

const INFO_HEADERS = ["Overview", "Trigger", "Activity", "Usage", "Health"];

test("i opens a service's page with all five sections", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, infoAt("ops::nightly", 100, 30, at));
  assertGrid(rows, 100, 30);
  assertGolden("info-nightly-100x30.txt", rows);
  const lines = screenText(rows).split("\n");

  assert.equal(lines[0].trimEnd(), "afkd › ops::nightly · Queued", "the title names the card and its state");
  // Every section header, once, bold, in §1's order — read off the **cells**, so a value that
  // happened to spell `Health` could not stand in for the header.
  const headers = rows.flatMap((row, i) =>
    row.filter((c) => c.bold && c.fg === "bright" && INFO_HEADERS.includes(c.text)).map((c) => [c.text, i]),
  );
  assert.deepEqual(
    headers.map(([h]) => h).sort(),
    [...INFO_HEADERS].sort(),
    "all five section headers, once each",
  );
  // The list is **gone**: its column header and its rows do not bleed through behind the page.
  for (const label of ["Last Activity", "Next Run", "⊟ 📦 ops", "🧵 heavy"]) {
    assert.ok(!screenText(rows).includes(label), `the list's ${label} is not on the info page`);
  }
  // The footer is `infoview::footer`'s three hints, and `back` teaches **both** keys the
  // `info.back` action binds rather than only the first.
  const footer = lines[lines.length - 1];
  // Derived from the keymap, not written down: `back` teaches **every** chord `info.back`
  // binds (`Keys::all`, no `Ctrl+` elision), so a rebind moves this hint with the table.
  assert.equal(
    footer.trimEnd(),
    `${all("info.back")} back  ${primary("global.help")} help  ${primary("global.quit")} quit`,
    "the footer is `infoview::footer`'s three hints, sourced from the keymap",
  );
});

test("two columns when the surface is wide, one when it is narrow", () => {
  const { board, at } = liveBoard();
  const wide = layout(board, infoAt("ops::nightly", 100, 30, at));
  const narrow = layout(board, infoAt("ops::nightly", 60, 30, at));
  assertGrid(narrow, 60, 30);
  assertGolden("info-nightly-60x30.txt", narrow);

  /// The x-origin of every section header on a screen — the grid's shape, read structurally
  /// rather than off a golden that could agree with a page that lost a column.
  const headerOrigins = (screen) => {
    const out = [];
    for (const row of screen) {
      let x = 0;
      for (const c of row) {
        if (c.bold && INFO_HEADERS.includes(c.text)) out.push(x);
        x += c.width;
      }
    }
    return out;
  };
  const wideOrigins = new Set(headerOrigins(wide));
  assert.equal(wideOrigins.size, 2, `a wide surface lays two columns, not ${[...wideOrigins]}`);
  assert.deepEqual([...wideOrigins].sort((a, b) => a - b), [0, 51], "column 0 and, past the gutter, column 1");
  assert.deepEqual([...new Set(headerOrigins(narrow))], [0], "a narrow one stacks in a single column");

  // The boundary, walked rather than asserted at one width: two columns at exactly 100, one at
  // 99, which is `shell::INFO_TWO_COL_MIN_WIDTH` and not a number this file chose.
  assert.equal(new Set(headerOrigins(layout(board, infoAt("ops::nightly", 100, 40, at)))).size, 2);
  assert.equal(new Set(headerOrigins(layout(board, infoAt("ops::nightly", 99, 40, at)))).size, 1);
});

test("no value is clipped at any width", () => {
  // The adversarial board: a CJK service name, a description carrying wide glyphs and an emoji,
  // a multi-word trigger key and a value with no space in it at all — a board URL, which is the
  // one token wider than any column here, so the hard break is exercised rather than described.
  const { board, at } = liveBoard();
  const adversarial = moved(renamed(board, "ops::nightly", "ops::監視サービス"), "ops::監視サービス", {
    description: "掃除します 🧹 between passes, and keeps the scratch tree small enough to walk",
    triggerDetail: "trello · board https://trello.com/b/Vt2SxD9n/foodlab",
    triggerFields: [
      ["board", "https://trello.com/b/Vt2SxD9n/foodlab"],
      ["pick_from", "Up for Grabs"],
      ["require_member", "false"],
    ],
  });
  // Tall enough that the whole body is on screen at every width — at 30 cells the page-wide
  // label column leaves four cells for a value and the wrapping is extreme, which is the point:
  // extreme and **lossless** is the property, and a viewport that cut it off would hide that.
  for (const cols of [100, 60, 30]) {
    const options = infoAt("ops::監視サービス", cols, 200, at);
    const rows = layout(adversarial, options);
    assertGrid(rows, cols, 200);
    const text = screenText(rows);
    assert.ok(!text.includes("…"), `nothing is elided at ${cols} cells`);
    for (const [label, value] of modelCells(adversarial, options)) {
      const got = valueOf(rows, label);
      assert.notEqual(got, null, `${label} is on screen at ${cols} cells`);
      assert.ok(sameText(got, value), `${label} reads ${JSON.stringify(got)} at ${cols}, not ${JSON.stringify(value)}`);
    }
    // The band is not a row, so it is checked on its own: it wraps whole, scheme and all.
    assert.ok(
      text.replace(/\s+/gu, "").includes(adversarial.services["ops::監視サービス"].description.replace(/\s+/gu, "")),
      `the About band wraps whole at ${cols}`,
    );
  }
});

test("the page scrolls rather than truncating", () => {
  // A 24-row terminal, the height the single-column form really overflows at.
  const { board, at } = liveBoard();
  const options = infoAt("ops::nightly", 60, 24, at);
  const max = infoScrollMax(board, options);
  assert.ok(max > 0, "the single-column page overflows a 24-row viewport");
  const top = layout(board, options);
  const body = (screen) => screenText(screen).split("\n").slice(2, -1);

  // One row of offset drops exactly the first body row and reveals exactly one new one — the
  // page moves by a row, it does not re-lay out.
  const one = layout(board, infoAt("ops::nightly", 60, 24, at, { infoOffset: 1 }));
  assertGrid(one, 60, 24);
  assert.deepEqual(body(one).slice(0, -1), body(top).slice(1), "offset 1 is the same page, one row down");

  // The last reachable offset shows the final body row: nothing below the fold is unreachable,
  // which is the whole claim a scrolling surface makes.
  const last = layout(board, infoAt("ops::nightly", 60, 24, at, { infoOffset: max }));
  assertGrid(last, 60, 24);
  assert.ok(body(last).some((l) => l.startsWith("Sandbox")), "the final row is reachable");
  // …and an offset past the end is clamped rather than blanking the page.
  const past = layout(board, infoAt("ops::nightly", 60, 24, at, { infoOffset: max + 50 }));
  assert.deepEqual(screenText(past), screenText(last), "an over-scrolled offset clamps to the last page");
  // A page that fits does not scroll at all.
  assert.equal(infoScrollMax(board, infoAt("ops::nightly", 100, 40, at)), 0);
});

test("the Trigger section names the kind, then its structured keys", () => {
  const { board, at } = liveBoard();
  const rows = layout(board, infoAt("ops::nightly", 100, 30, at));
  assert.equal(valueOf(rows, "Kind"), "interval", "the bare kind, off the flat detail's lead segment");
  assert.equal(valueOf(rows, "Every"), "1h", "…then the config key on its own row, value verbatim");
  assert.ok(!screenText(rows).includes("interval · every 1h"), "and not the ` · `-joined line");

  // A multi-word key and a URL value — the prettified label, and the value **untrimmed**: §1's
  // lowercase-data exemption says a structured trigger value is echoed from the config, so
  // `trim_url_tail`'s `…/b/…` shortening is deliberately not transcribed here.
  const trello = moved(board, "ops::nightly", {
    triggerDetail: "trello · board foodlab · pick_from Up for Grabs",
    triggerFields: [
      ["board", "https://trello.com/b/Vt2SxD9n/foodlab"],
      ["pick_from", "Up for Grabs"],
      ["require_member", "false"],
    ],
  });
  const structured = layout(trello, infoAt("ops::nightly", 100, 30, at));
  assert.equal(valueOf(structured, "Kind"), "trello");
  assert.ok(sameText(valueOf(structured, "Board"), "https://trello.com/b/Vt2SxD9n/foodlab"), "scheme and host kept");
  assert.equal(valueOf(structured, "Pick from"), "Up for Grabs", "`pick_from` → `Pick from`, value verbatim");
  assert.equal(valueOf(structured, "Require member"), "false", "…only the first character is cased");

  // The fallback: a kind outside the expansion set (or an older daemon) sends no fields, and the
  // section degrades to a lone `Kind` row carrying the flat line. That is the wire contract.
  const flat = moved(board, "ops::nightly", { triggerFields: [] });
  const degraded = layout(flat, infoAt("ops::nightly", 100, 30, at));
  assert.equal(valueOf(degraded, "Kind"), "interval · every 1h", "the flat detail line");
  assert.equal(valueOf(degraded, "Every"), null, "…and no structured row beside it");
});

test("the Queue row carries the lane, its level and its live parallelism", () => {
  const { board, at } = liveBoard();
  const lane = layout(board, infoAt("ops::nightly", 100, 30, at));
  assert.equal(valueOf(lane, "Queue"), "heavy (high) · parallelism 2");

  // A lane-less service spends no row on one: the row's **presence** is the fact.
  const none = layout(board, infoAt("janitor", 100, 30, at));
  assert.equal(valueOf(none, "Queue"), null, "no lane, no `Queue` row at all");

  // Zero is a real lane width (ADR-0079 §2h) — a paused lane — and must not read as silence.
  const paused = moved(board, "ops::nightly", { queueParallelism: 0 });
  assert.equal(valueOf(layout(paused, infoAt("ops::nightly", 100, 30, at)), "Queue"), "heavy (high) · parallelism 0");
  // A width the daemon did not send drops the suffix, rather than printing a trailing ` · `.
  const unknown = moved(board, "ops::nightly", { queueParallelism: null });
  assert.equal(valueOf(layout(unknown, infoAt("ops::nightly", 100, 30, at)), "Queue"), "heavy (high)");
  // …and a level it did not send drops the parenthetical rather than printing an empty one.
  const levelless = moved(board, "ops::nightly", { queuePriority: "" });
  assert.equal(valueOf(layout(levelless, infoAt("ops::nightly", 100, 30, at)), "Queue"), "heavy · parallelism 2");
});

test("a service with no description draws no About band", () => {
  const { board, at } = liveBoard();
  const withBand = layout(board, infoAt("janitor", 100, 30, at));
  assertGolden("info-janitor-100x30.txt", withBand);
  assertGrid(withBand, 100, 30);
  const banded = screenText(withBand).split("\n");
  assert.equal(banded[2], "About".padEnd(100), "the band's own bold header");
  assert.equal(banded[3].trimEnd(), "sweeps the scratch tree between passes", "…and the service's own sentence");

  // The one with none: no header, and the band costs **zero** rows rather than an empty box —
  // the first section header sits exactly where the banded page's sits, minus the band's rows.
  const without = layout(board, infoAt("ops::nightly", 100, 30, at));
  const plain = screenText(without).split("\n");
  assert.ok(!plain.includes("About".padEnd(100)), "no band");
  assert.equal(banded.findIndex((l) => l.startsWith("Overview")), 5);
  assert.equal(plain.findIndex((l) => l.startsWith("Overview")), 2, "the band's three rows, and not one more");

  // A sentence long enough to wrap several times wraps **whole** — the terminal's two-row cap
  // protects a height budget a scrolling surface does not have.
  const long = "sweeps the scratch tree between passes, prunes every run directory older than the "
    + "retention window, and re-checks the worktree union so a stale mount never outlives the "
    + "service that asked for it";
  const wrapped = layout(moved(board, "janitor", { description: long }), infoAt("janitor", 100, 40, at));
  assertGrid(wrapped, 100, 40);
  const text = screenText(wrapped);
  assert.ok(!text.includes("…"), "nothing is elided out of the band");
  assert.ok(text.replace(/\s+/gu, "").includes(long.replace(/\s+/gu, "")), "the whole sentence is on screen");
});

test("orphan and stale each read their own tag under Health", () => {
  // One fixture board carrying **both** verdicts — `reload.jsonl`'s live board has `spare`
  // orphaned and `noisy` stale off real reconcile frames. Asserted as a pair over that one
  // board rather than as two hand-written strings, so the two cannot drift apart or converge.
  const { board, at } = liveBoard("reload.jsonl");
  const orphan = layout(board, infoAt("spare", 100, 30, at));
  const stale = layout(board, infoAt("noisy", 100, 30, at));
  assertGrid(orphan, 100, 30);
  assertGolden("info-orphan-100x30.txt", orphan);

  const tags = [valueOf(orphan, "Config"), valueOf(stale, "Config")];
  for (const value of tags) {
    assert.notEqual(value, null, "both verdicts render on a `Config` row");
    assert.match(value, /^[a-z]/, `the tag is lowercase inline data, not a label: ${value}`);
  }
  assert.ok(tags[0].startsWith("orphan "), `the orphan's own tag: ${tags[0]}`);
  assert.ok(tags[1].startsWith("stale "), `the stale one's: ${tags[1]}`);
  assert.notEqual(tags[0], tags[1], "each carries its own sentence, not the other's");
  for (const value of tags) assert.match(value, / - .*[a-z]/, "…and an actionable sentence after it");

  // The `Config` row is under **Health**, not loose: its label column sits below that header
  // and above nothing else. Read off the composed row, so a value spelling `Health` cannot lie.
  const headerRow = orphan.findIndex((row) => row.some((c) => c.bold && c.text === "Health"));
  const configRow = orphan.findIndex((row) => row.some((c) => c.dim && c.text.trimEnd() === "Config"));
  assert.ok(headerRow !== -1 && configRow > headerRow, "the `Config` row is inside the Health section");

  // A healthy service has none at all — the fixed-label contract sees the row only when set.
  assert.equal(valueOf(layout(board, infoAt("reports", 100, 30, at)), "Config"), null);
  assert.equal(valueOf(layout(board, infoAt("reports", 100, 30, at)), "Recovery"), null, "…nor a Recovery row");
});

test("the page and the list read one value for one datum", () => {
  // The duplicated-surface rule (`docs/agents/00-conventions.md`): the list row and the info
  // page fold the same three anchors, so they are read off **both rendered surfaces** over one
  // fixture board rather than against two hand-written expectations.
  const { board, at } = liveBoard();
  const name = "ops::nightly";
  const listRows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" });
  const page = layout(board, infoAt(name, 100, 30, at));
  const listRow = listRows.find((row) => row.map((c) => c.text).join("").includes("nightly"));
  assert.notEqual(listRow, undefined, "the service has a list row");
  const listText = listRow.map((c) => c.text).join("");

  // `State`: the list cell is `<glyph> <Badge> <elapsed>`, the page's phrase `<Badge> for
  // <elapsed>` — the same badge and the same elapsed off the same anchor.
  const state = valueOf(page, "State");
  const elapsed = state.match(/(\d+[smhd](?: \d+[smhd])?)/u)[1];
  assert.ok(listText.includes(`▷ Queued ${elapsed}`), `the list says ${JSON.stringify(listText)}, the page ${state}`);

  // `Last activity`: byte for byte the list's `Last Activity` cell, through `livenessCell`.
  const activity = valueOf(page, "Last activity");
  assert.ok(listText.includes(activity), `the list's activity cell carries ${activity}`);

  // `Next run`: the page prefixes `in `, and the countdown behind it is the `Next` cell's own.
  // Taken on a service that really has a deadline, so the assertion is not vacuous.
  const armed = layout(board, infoAt("archivist", 100, 30, at));
  const next = valueOf(armed, "Next run");
  assert.match(next, /^in \d/u, `an armed service counts down: ${next}`);
  const armedRow = listRows.find((row) => row.map((c) => c.text).join("").includes("archivist"));
  assert.ok(armedRow.map((c) => c.text).join("").trimEnd().endsWith(next.slice(3)), "the same countdown as the list");
});

test("a viewport too short for the info page still composes a whole screen", () => {
  // The degenerate end, the list's own twin: at three rows the title band and the footer take
  // everything and the body takes nothing — and the screen is still exactly three whole rows
  // rather than a fault. `janitor` is the adversarial subject: its `About` band alone is taller
  // than several of these viewports, and a band that overran would break the `rows` contract
  // every golden rests on.
  const { board, at } = liveBoard();
  for (const cols of [100, 60]) {
    for (const height of [1, 3, 5, 8, 14]) {
      assertGrid(layout(board, infoAt("janitor", cols, height, at)), cols, height);
      assertGrid(layout(board, infoAt("ops::nightly", cols, height, at)), cols, height);
    }
  }
});

test("a page whose service vanished falls back to the list", () => {
  // `draw_info`'s own arm: `info_view()` returns `None` and the list is drawn, while the key
  // table stays the info one. The session deliberately does not clear `info`, so a service that
  // comes back comes back to its page.
  const { board, at } = liveBoard();
  const gone = layout(board, infoAt("no-such-service", 100, 30, at));
  assertGrid(gone, 100, 30);
  assert.deepEqual(
    screenText(gone),
    screenText(layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" })),
    "the list, byte for byte",
  );
  assert.equal(infoScrollMax(board, infoAt("no-such-service", 100, 30, at)), 0, "and nothing to scroll");
});

test("the overlays still outrank the info page", () => {
  // `shell.rs` puts `render_overlays` above the view choice, so the confirm modal and the `?`
  // overlay rise over the info page exactly as they rise over the list. An overlay that only
  // rose over one of them would be an overlay the operator can hide behind.
  const { board, at } = liveBoard();
  const help = layout(board, infoAt("ops::nightly", 160, 44, at, { help: true }));
  assertGrid(help, 160, 44);
  assert.ok(screenText(help).includes("Ctrl+R is afkd's reload"), "the `?` overlay is up over the page");
  const confirm = layout(board, infoAt("ops::nightly", 100, 30, at, {
    confirm: { verb: "force", targets: ["ops::sleeper"], skipped: 0 },
  }));
  assertGrid(confirm, 100, 30);
  assert.ok(screenText(confirm).includes("Force"), "and so is the confirm modal");
});

test("a flash still owns the info page's footer", () => {
  // The footer's precedence is the terminal's: a daemon refusal must not be buried by a legend,
  // so the flash outranks the info hints and the hints come back byte for byte when it expires.
  const { board, at } = liveBoard();
  const quiet = layout(board, infoAt("ops::nightly", 100, 30, at));
  const flashed = layout(board, infoAt("ops::nightly", 100, 30, at, { flash: "reloaded: 1 changed" }));
  assertGrid(flashed, 100, 30);
  const lastOf = (screen) => screenText(screen).split("\n").at(-1);
  assert.equal(lastOf(flashed).trimEnd(), "reloaded: 1 changed");
  assert.match(lastOf(quiet), /^i\/Esc back/u, "…and the hints are back once it ages out");
});

test("a wide glyph on the info page costs the cells it costs", () => {
  // The same twelve-cell pair the list's own width test uses: measured by `length` a CJK name
  // would cost six and every row would run short; taken for four cells it would overrun.
  const { board, at } = liveBoard();
  const wide = renamed(board, "ops::nightly", "ops::監視サービス");
  const ascii = renamed(board, "ops::nightly", "ops::abcdefghijkl");
  for (const cols of [100, 60]) {
    const a = layout(wide, infoAt("ops::監視サービス", cols, 30, at));
    const b = layout(ascii, infoAt("ops::abcdefghijkl", cols, 30, at));
    assertGrid(a, cols, 30);
    assertGrid(b, cols, 30);
    // With the two names cut out of the title, the two screens are the **same** text: every
    // pad, every column origin and every wrap below is where it was.
    const strip = (screen) => screenText(screen).replace(/監視サービス|abcdefghijkl/gu, "");
    assert.equal(strip(a), strip(b), `a wide name moves nothing at ${cols} cells`);
  }
  // …and a value carrying a wide grapheme breaks on a **cell** boundary, never inside a cluster.
  const cjk = moved(board, "ops::nightly", {
    triggerFields: [["pick_from", "監視サービスの担当者を選ぶ列 Up for Grabs"]],
  });
  const rows = layout(cjk, infoAt("ops::nightly", 100, 30, at));
  assertGrid(rows, 100, 30);
  assert.ok(sameText(valueOf(rows, "Pick from"), "監視サービスの担当者を選ぶ列 Up for Grabs"), "the value reads whole");
});

// --- the run view -------------------------------------------------------------------

/**
 * One tab's open run view, the shape `session.mjs`'s `openRun` mints. Spelled here rather than
 * driven through `press()` because these are *layout* tests and the state has to be dialled to
 * a focus, a fold and a scroll a keypress sequence would take a dozen presses to reach — but the
 * shape is anchored to the real one by `the_layout_reads_the_session_run_openRun_mints` below,
 * so a field renamed in `session.mjs` reddens here rather than leaving these screens rendering a
 * state no key can produce. That anchor compares field *names*, not values: the default `focus`
 * here is deliberately `"tree"`, the pane `openRun` does **not** open on, so a tree screen reads
 * as the plain case and a log screen has to ask for it by name.
 */
function runState(service, patch = {}) {
  return {
    service,
    focus: "tree",
    cursor: null,
    overrides: new Map(),
    expanded: new Set(),
    tree: "follow",
    log: "follow",
    scoped: false,
    ...patch,
  };
}

/// The run view's body rows — the pane's window, between its title band and the footer. Read off
/// the screen rather than off a plan, so an assertion sees what an operator sees.
function paneBody(rows) {
  const text = screenText(rows).split("\n");
  const footerAt = text.findIndex((l, i) => i > 4 && /^[a-zA-Z?].* (quit|help)( |$)/.test(l));
  return text.slice(4, footerAt === -1 ? text.length : footerAt).filter((l) => l.trim() !== "");
}

test("o opens the run tree over a real fire, with its depth, its siblings and its rail", () => {
  // `trace-burst.jsonl` is the capture with **depth**: a `workflow` root over an `in_parallel`
  // with two leaves, a `times` loop whose node the daemon relabelled twice, and a `guard` with
  // its own leaf. Everything the tree pane composes — the guides, the connectors, the chevrons,
  // the kind glyphs, the right-flushed OUT/TOOK/ST rail — is on one screen.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", run: runState("deep") });
  assertGrid(rows, 100, 30);
  assertGolden("run-tree-100x30.txt", rows);

  const body = paneBody(rows);
  const text = screenText(rows);
  // Every node the capture's own `opened` frames declare, on exactly one row — derived from the
  // frames rather than typed in, so a re-capture cannot make this vacuous. The `times` node is
  // the exception and is asserted below: its row reads the **relabel**, not the as-opened label.
  const tree = board.services.deep.tree;
  const ids = Object.keys(tree.nodes).map(Number);
  assert.equal(ids.length, 9, "the capture still folds its nine nodes");
  for (const id of ids) {
    const node = tree.nodes[id];
    const headline = node.relabel ?? node.label;
    assert.equal(
      body.filter((l) => l.includes(` ${headline}`)).length >= 1,
      true,
      `node ${id} (${headline}) has a row`,
    );
  }
  // The **relabel** is what shows: the node opened `times 2` and was relabelled twice, so a row
  // reading the as-opened label would be a row a second behind the daemon.
  assert.equal(tree.nodes[5].label, "times 2", "the capture's as-opened label");
  assert.equal(tree.nodes[5].relabel, "times 2/2", "…and the live one the daemon relabelled to");
  assert.ok(text.includes("🌀 times 2/2"), "the row reads the relabel");
  assert.ok(!text.includes("🌀 times 2 "), "…and not the label it opened with");
  // The nesting: a root draws no connector, a non-last sibling an elbow with a `│` under it, the
  // last a `└─`, and a grandchild's own elbow hangs off its parent's guide column.
  assert.match(body[0], /^⊟ 🚀 deep_pass/, "the root sits flush at column 0");
  assert.match(body[1], /^├─ ⊟ ∥︎ in_parallel/, "a non-last child draws `├─`");
  assert.match(body[2], /^│ {2}├─ ⚙︎ echo left/, "its first grandchild hangs off a `│` guide");
  assert.match(body[3], /^│ {2}└─ ⚙︎ echo right/, "…and the last off the same guide with `└─`");
  assert.match(body[body.length - 1], /^ {3}└─ ⚙︎ echo guarded/, "the last leaf's guide column is blank");
  // The rail: `ST` lands on **one** column on every row regardless of depth, which is the
  // alignment the VS15'd glyphs and the right flush exist for. Measured off the cells, because
  // that is the only place a wide glyph's second column is visible.
  const st = rows
    .slice(4, 4 + body.length)
    .map((row) => boundaries(row)[row.length - 2]);
  assert.equal(new Set(st).size, 1, `ST is on one column at every depth, not ${[...new Set(st)].join("/")}`);
  // …and the whole rail is there: a cmd leaf's `N ln`, a container's blank OUT, every node's
  // `TOOK`, and the `✓` each of them closed with.
  assert.ok(body[2].includes("1 ln"), "a cmd leaf sizes its output");
  assert.ok(body.every((l) => l.trimEnd().endsWith("✓︎")), "every node in this capture closed ok");
});

test("a narrow run tree sheds OUT and keeps its status column", () => {
  // The shed ladder, in a golden: at 60 cells the pane is under `TREE_RAIL_SHOW_OUT` (64) and
  // over `TREE_RAIL_SHOW_TOOK` (44), so `OUT` is gone, `TOOK` is not, and `ST` never sheds. Same
  // board as the wide screen, so the only difference is the width.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const options = { rows: 30, now: at + 1000, version: "0.2.123", run: runState("deep") };
  const rows = layout(board, { ...options, cols: 60 });
  assertGrid(rows, 60, 30);
  assertGolden("run-tree-60x30.txt", rows);
  const narrow = paneBody(rows);
  assert.ok(narrow.every((l) => !l.includes("1 ln")), "OUT is shed at 60 cells");
  assert.ok(narrow.some((l) => l.includes("0.0s")), "TOOK is not");
  assert.ok(narrow.every((l) => l.trimEnd().endsWith("✓︎")), "and ST never is");
  // Below `TREE_RAIL_SHOW_TOOK` only `ST` is left, and the label elides rather than the rail
  // moving — the two rungs of the one ladder.
  const tiny = paneBody(layout(board, { ...options, cols: 30 }));
  assert.ok(tiny.every((l) => !l.includes("0.0s")), "TOOK sheds next");
  assert.ok(tiny.every((l) => l.trimEnd().endsWith("✓︎")), "ST still does not");
  // The elision needs a label long enough to overrun, which `trace-burst`'s `echo pass` is not:
  // the deep capture's `Bash cargo check --workspace --all-targets` sits two levels down, so its
  // gutter is widest and its budget smallest — the deepest rows elide first.
  const deep = liveBoard("deep-fail.jsonl");
  const elided = paneBody(
    layout(deep.board, { ...options, cols: 40, now: deep.at + 1000, run: runState("tree::retried") }),
  );
  const cargo = elided.find((l) => l.includes("Bash cargo"));
  assert.ok(cargo !== undefined, "the long tool call is on screen");
  assert.ok(cargo.includes("…"), `the over-wide label elides: ${JSON.stringify(cargo)}`);
  assert.ok(cargo.trimEnd().endsWith("✗︎"), "…and the rail keeps its column while it does");
  // A `RAIL_GAP` is reserved so the `…` never butts the rail it was elided for.
  assert.ok(/…\s{2,}/u.test(cargo), "the ellipsis keeps its gap before the rail");
  // A degenerate width never throws and never loses the grid: the label budget floors at one.
  for (const cols of [1, 2, 3, 8]) {
    assertGrid(layout(board, { ...options, cols }), cols, 30);
  }
});

test("the run log wraps the adversarial line set by display width", () => {
  // `noise.jsonl`'s own fire prints the set that breaks a naive wrap: an SGR run, an OSC
  // residue, an embedded `\r`, tabs, a `U+202E` bidi override, CJK, an emoji and a zero-width
  // space. The fold sanitized them; the pane has to *measure* what is left.
  const { board, at } = liveBoard("noise.jsonl");
  const options = { rows: 30, now: at + 1000, version: "0.2.123", run: runState("noisy", { focus: "log" }) };
  const wide = layout(board, { ...options, cols: 100 });
  assertGrid(wide, 100, 30);
  assertGolden("run-log-100x30.txt", wide);
  // The pane title says what it is scoped to and where in the ring it is sitting.
  assert.match(screenText(wide).split("\n")[2], /^Log · all · 1–10\/10/, "the scope and the range");

  const narrow = layout(board, { ...options, cols: 40 });
  assertGrid(narrow, 40, 30);
  assertGolden("run-log-40x30.txt", narrow);
  const body = paneBody(narrow);
  // Every wrapped continuation hangs exactly two cells, and no first row does — the aligned
  // hang is the only thing marking a continuation, so it has to be exact.
  const hung = body.filter((l) => l.startsWith("  "));
  assert.ok(hung.length > 0, "the narrow pane really wrapped");
  assert.ok(hung.every((l) => !l.startsWith("   ")), "a continuation hangs two cells, not three");
  // Exactly one un-hung row per ring entry: a continuation is marked by the hang and nothing
  // else, so an entry that started a second un-hung row would read as two lines.
  const ring = board.services.noisy.log.lines.map((e) => e.text);
  assert.equal(body.length - hung.length, ring.length, "one first row per entry, and no more");
  // Nothing is lost. Compared with **every space removed** from both sides, because the only
  // thing the wrap inserts is spaces (the hang and the row's pad) — so a missing or duplicated
  // non-space character is exactly what this catches, and a break that lands on a space is not
  // a loss. The goldens pin the exact bytes; this pins the content across the wrap.
  const squash = (line) => line.replace(/ /gu, "");
  const painted = squash(body.join(""));
  for (const line of ring) {
    assert.ok(painted.includes(squash(line)), `the pane still holds ${JSON.stringify(line)}`);
  }
  // A wide glyph is never split down the middle: every wrapped line measures at most the pane's
  // own width (the screen less the one-column scrollbar gutter), counted in **cells**, which a
  // `slice` by UTF-16 unit would not hold. Read off the content cell rather than the rendered
  // row, because the row also carries the pad and the gutter.
  const content = narrow.slice(4, 4 + body.length).map((row) => row[0].text);
  for (const line of content) {
    assert.ok(textWidth(line) <= 39, `${JSON.stringify(line)} (${textWidth(line)} cells) fits the pane`);
  }
  // …and the straddle really bites somewhere: swept across widths, a row that ends **one cell
  // short** with a wide grapheme on it is the signature of a two-cell cluster pushed whole to the
  // next row rather than split down the middle. Swept rather than pinned at one width, because
  // whether a given line's CJK lands on the edge is an accident of that width.
  const straddles = [];
  for (let cols = 20; cols <= 70; cols += 1) {
    const at40 = layout(board, { ...options, cols });
    assertGrid(at40, cols, 30);
    for (const row of at40.slice(4)) {
      const text = row[0].text;
      if (text.trim() === "" || textWidth(text) !== cols - 2) continue;
      const last = [...new Intl.Segmenter("en", { granularity: "grapheme" }).segment(text)].pop()?.segment ?? "";
      if (textWidth(last) === 2) straddles.push([cols, text]);
    }
  }
  assert.ok(straddles.length > 0, "a wide cluster is pushed whole at some width rather than halved");
  // The degenerate floor: at a width narrower than the hang itself both budgets floor to one
  // column, so the wrap is a minimum-one-cell hard split rather than an infinite loop.
  for (const cols of [1, 2, 3]) {
    assertGrid(layout(board, { ...options, cols }), cols, 30);
  }
});

test("s scopes the log pane to one node's own lines, and says so in its title", () => {
  // The filter has to *narrow*, which only a board whose ring mixes attributed and unattributed
  // lines can show: `noise.jsonl`'s `noisy` ring is ten entries, eight of them tagged onto the
  // one `cmd` node and two — the supervisor's own `[RUN]` and `[OK]` brackets — tagged onto no
  // node at all. Counted off the ring rather than typed in, so a `nodeEntries` that returned
  // everything and one that returned nothing are both a failure here.
  const { board, at } = liveBoard("noise.jsonl");
  const ring = board.services.noisy.log.lines;
  const own = ring.filter((e) => e.node === 1);
  const supervisor = ring.filter((e) => e.node === null);
  assert.equal(ring.length, 10, "the capture's ring is still ten entries");
  assert.equal(own.length, 8, "…eight of them the node's own");
  assert.equal(supervisor.length, 2, "…and two the supervisor's, attributed to no node");

  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" };
  const at_ = (patch) => ({ ...options, run: runState("noisy", { focus: "log", ...patch }) });
  const wide = layout(board, at_({}));
  const scoped = layout(board, at_({ scoped: true, cursor: 1 }));
  assertGrid(wide, 100, 30);
  assertGrid(scoped, 100, 30);
  const titleOf = (rows) => screenText(rows).split("\n")[2].trimEnd();
  // The two counts, off one board: the range in the title and the total the scroll clamps
  // against both move, and they move to the ring's own two numbers.
  assert.equal(titleOf(wide), "Log · all · 1–10/10", "unscoped, the pane is the whole ring");
  assert.equal(
    titleOf(scoped),
    "Log · /tmp/afkd-fixture/bin/noise.sh · 1–8/8",
    "scoped, it names the node and holds only its lines",
  );
  assert.equal(runMetrics(board, at_({})).logTotal, ring.length, "the unscoped total is the ring");
  assert.equal(runMetrics(board, at_({ scoped: true, cursor: 1 })).logTotal, own.length, "…the scoped one the node's");
  // …and the lines it dropped are named, so a filter that narrowed by the wrong rule is caught
  // as well as one that did not narrow at all.
  const bodyOf = (rows) => paneBody(rows).map((l) => l.trimEnd());
  assert.deepEqual(bodyOf(wide), ring.map((e) => e.text), "the unscoped pane is the ring, in order");
  assert.deepEqual(bodyOf(scoped), own.map((e) => e.text), "…and the scoped one the node's eight");
  for (const dropped of supervisor) {
    assert.ok(bodyOf(wide).includes(dropped.text), `the unscoped pane holds ${JSON.stringify(dropped.text)}`);
    assert.ok(!bodyOf(scoped).includes(dropped.text), "…and the scoped one does not");
  }
  // A node that emitted nothing scopes to an **empty** pane rather than falling back to the
  // ring: `trace-burst`'s `times` node is a container and the capture tags no line onto it.
  const burst = liveBoard("trace-burst.jsonl");
  const empty = layout(burst.board, {
    ...options,
    now: burst.at + 1000,
    run: runState("deep", { focus: "log", scoped: true, cursor: 5 }),
  });
  assertGrid(empty, 100, 30);
  assert.ok(burst.board.services.deep.log.lines.length > 0, "the board's ring is not itself empty");
  assert.equal(titleOf(empty), "Log · times 2/2 · 0–0/0", "a node with no output scopes to nothing");
  assert.deepEqual(paneBody(empty), [], "…and the pane really is empty");
});

test("an expanded leaf hangs its own output under it, at its depth and elided past the fifth", () => {
  // `noise.jsonl`'s `noisy` is the only committed capture whose ring is node-tagged **and**
  // over `BODY_ELIDE`: eight lines on one `cmd` root, and the eight are the adversarial set —
  // an SGR run, an OSC residue, an embedded `\r`, tabs, a bidi override, CJK, an emoji and a
  // zero-width space — so the elision boundary and the body row's own measuring are one screen.
  const { board, at } = liveBoard("noise.jsonl");
  const leaf = board.services.noisy.tree.nodes[1];
  const own = board.services.noisy.log.lines.filter((e) => e.node === 1);
  assert.equal(leaf.kind, "cmd", "the capture still folds one `cmd` leaf");
  assert.deepEqual(leaf.out, { n: own.length, out: "lines" }, "…whose OUT is the ring's own count for it");
  assert.ok(own.length > 5, `the capture's ${own.length} lines still overrun BODY_ELIDE`);

  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" };
  const opened = { ...options, run: runState("noisy", { expanded: new Set([1]) }) };
  const rows = layout(board, opened);
  assertGrid(rows, 100, 30);
  assertGolden("run-tree-body-100x30.txt", rows);

  const body = paneBody(rows).map((l) => l.trimEnd());
  assert.match(body[0], /^⚙︎ \/tmp\/afkd-fixture\/bin\/noise\.sh/, "the leaf's own row is still first");
  // Five shown, three counted — the boundary, both sides of it, derived from the ring so a
  // re-capture with a longer fire moves the count rather than making the assertion vacuous.
  const shown = body.slice(1, -1);
  assert.deepEqual(shown, own.slice(0, 5).map((e) => `▏ ${e.text}`), "the first five lines hang under it");
  assert.equal(body.at(-1), `▏ … ${own.length - 5} more`, "…and the rest are counted, not drawn");
  assert.ok(
    own.slice(5).every((e) => !screenText(rows).includes(e.text)),
    "the elided lines are really off the tree pane — they are one `s` away, in the log",
  );
  // The clamp==render identity, made **non-vacuous**: `withBodies` inserted six rows into the
  // one walk the scroll clamps against, so a second count of the tree would be off by six.
  const nodes = Object.keys(board.services.noisy.tree.nodes).length;
  assert.equal(runMetrics(board, opened).treeTotal, body.length, "the tree's total is what it painted");
  assert.equal(body.length, nodes + 6, "…and it is the node rows plus the body, not the node rows");
  assert.equal(runMetrics(board, { ...options, run: runState("noisy") }).treeTotal, nodes, "collapsed, it is the nodes alone");

  // The **depth**: a body hangs at its leaf's *content* indent — the leaf's own guides plus a
  // column for its level — so the `▏` sits where a child's elbow would, not where the leaf's
  // glyph does. `trace-burst` has the three shapes on one screen: a non-last child under a
  // continuing guide, a last child under the same one, and a last child of a last child.
  const burst = liveBoard("trace-burst.jsonl");
  const deep = layout(burst.board, {
    ...options,
    now: burst.at + 1000,
    run: runState("deep", { expanded: new Set([3, 4, 9]) }),
  });
  assertGrid(deep, 100, 30);
  const bars = paneBody(deep).map((l) => l.trimEnd()).filter((l) => l.includes("▏"));
  assert.deepEqual(
    bars,
    [
      "│  │  ▏ [deep]            [echo] left",
      "│     ▏ [deep]            [echo] right",
      "      ▏ [deep]            [echo] guarded",
    ],
    "each body hangs at its own leaf's content depth, guides and all",
  );
  // One line each, so no `… N more` — and the identity holds at depth too.
  assert.equal(
    runMetrics(burst.board, { ...options, now: burst.at + 1000, run: runState("deep", { expanded: new Set([3, 4, 9]) }) }).treeTotal,
    paneBody(deep).length,
    "the tree's total is what it painted, with bodies interleaved at depth",
  );
});

test("a failed leaf opens its own output without being asked", () => {
  // The **auto-expand** arm of `withBodies`, which is a different disjunct from the `expanded`
  // one above: a `cmd`/`tool` leaf that closed `failed`/`killed` shows its body with no override
  // anywhere, so a failure's output is never a keypress away. `loud-fail.jsonl` is the capture
  // recorded for it — the other six either close every leaf `ok` or attribute no ring line to
  // the leaf that failed, which renders the arm true and invisible.
  const { board, at } = liveBoard("loud-fail.jsonl");
  const leaf = board.services.loud_fail.tree.nodes[1];
  const own = board.services.loud_fail.log.lines.filter((e) => e.node === 1);
  assert.equal(leaf.kind, "cmd", "the capture's one node is a `cmd`");
  assert.deepEqual(leaf.children, [], "…a leaf");
  assert.equal(leaf.status, "failed", "…that failed");
  assert.ok(own.length > 5, `…with ${own.length} attributed lines, over BODY_ELIDE`);

  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" };
  const plain = { ...options, run: runState("loud_fail") };
  assert.equal(plain.run.expanded.size, 0, "nothing is expanded by hand on this screen");
  const rows = layout(board, plain);
  assertGrid(rows, 100, 30);
  assertGolden("run-tree-autoexpand-100x30.txt", rows);

  const body = paneBody(rows).map((l) => l.trimEnd());
  assert.match(body[0].trimEnd(), /✗︎$/, "the leaf's own row reads its failure");
  assert.deepEqual(body.slice(1, -1), own.slice(0, 5).map((e) => `▏ ${e.text}`), "its first five lines hang under it");
  assert.equal(body.at(-1), `▏ … ${own.length - 5} more`, "…and the rest are counted");
  // The ring's order is the **wire's**, not the script's: the run's `stderr` line landed third
  // even though the shell printed it last, so the body is what the daemon sent, in that order —
  // which a body row-sorted by anything else would quietly change.
  assert.ok(body[2].includes(":err] step 7/7 FAILED"), "the stderr line sits where the wire put it");
  // And the arm really is the **status**, not a default that opens every leaf: `noise.jsonl`'s
  // leaf is the same kind with the same node-tagged ring and closed `ok`, and it draws its row
  // alone. Two real captures, so neither half can be satisfied by one wrong answer.
  const ok = liveBoard("noise.jsonl");
  const okLeaf = ok.board.services.noisy.tree.nodes[1];
  assert.equal(okLeaf.kind, leaf.kind, "the control leaf is the same kind");
  assert.equal(okLeaf.status, "ok", "…and closed ok");
  assert.equal(
    paneBody(layout(ok.board, { ...options, now: ok.at + 1000, run: runState("noisy") })).length,
    1,
    "an ok leaf keeps its output folded away",
  );
});

test("a collapsed parent never buries a failure, and an agent folds unless it did", () => {
  // `deep-fail.jsonl` is the sixth capture, recorded for exactly these three rules: a `guard`
  // that closed `skipped` over a leaf that `failed`, an `agent` that closed `ok` over a tool
  // that did not, and an `agent` whose tools all passed. No other capture holds a failure
  // anywhere below a root, so without it all three ship unguarded.
  const { board, at } = liveBoard("deep-fail.jsonl");
  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" };
  const svc = "tree::retried";
  const tree = board.services[svc].tree;
  // The capture's own shape, asserted off the fold before anything is rendered against it — a
  // re-capture that lost one of the three would fail here rather than silently pass below.
  assert.equal(tree.nodes[8].kind, "guard", "the guard node");
  assert.equal(tree.nodes[8].status, "skipped", "…closed skipped, so it is not itself a failure");
  assert.equal(tree.nodes[9].parent, 8, "…over a child");
  assert.equal(tree.nodes[9].status, "failed", "…that failed");
  assert.equal(tree.nodes[5].kind, "agent", "the `mender` agent");
  assert.equal(tree.nodes[5].status, "ok", "…closed ok");
  assert.ok(
    tree.nodes[5].children.some((id) => tree.nodes[id].status === "failed"),
    "…over a tool call that did not",
  );
  assert.equal(tree.nodes[2].kind, "agent", "the `scribe` agent");
  assert.ok(tree.nodes[2].children.every((id) => tree.nodes[id].status === "ok"), "…whose tools all passed");

  const rowFor = (rows, needle) => paneBody(rows).find((l) => l.includes(needle));
  // The **per-kind default**, with no override anywhere: `scribe` folds (its subtree is deep
  // detail you opt into) while `mender` is **expanded** — the auto-expand escape hatch, so a
  // `✗` is never unreachable. Both read off one flatten of one tree, so the default and its
  // exception cannot both be satisfied by a single wrong answer.
  const plain = layout(board, { ...options, run: runState(svc) });
  assertGrid(plain, 100, 30);
  assertGolden("run-tree-fail-100x30.txt", plain);
  assert.match(rowFor(plain, "scribe"), /⊞ 🤖 scribe/, "an agent whose tools passed lands folded");
  assert.ok(!screenText(plain).includes("Read /tmp/notes.md"), "…and its tool calls are not on screen");
  assert.match(rowFor(plain, "mender"), /⊟ 🤖 mender/, "an agent with a failed tool auto-expands");
  assert.ok(screenText(plain).includes("🔧 Bash cargo check"), "…so the failing call is reachable");
  // The **recovered** marker: fold `mender` by hand and its row reads `↻` — it came back ok, but
  // something under it did not, and neither `✓` nor `✗` says that. Expanded, the same row reads
  // its own `✓`. The `display_status` pair, both ways over one node.
  const folded = layout(board, { ...options, run: runState(svc, { overrides: new Map([[5, true]]) }) });
  assertGrid(folded, 100, 30);
  assert.match(rowFor(folded, "mender").trimEnd(), /↻︎$/, "a collapsed ok-over-failed reads recovered");
  assert.match(rowFor(plain, "mender").trimEnd(), /✓︎$/, "expanded, it reads its own status");
  // The **rollup**: fold the `skipped` guard and its row reads `✗`, because `worst(skipped,
  // failed)` is the failure it is hiding. Expanded it reads its own `⊘`. A non-failing parent,
  // so neither half of the assertion is vacuous.
  const guard = layout(board, { ...options, run: runState(svc, { overrides: new Map([[8, true]]) }) });
  assertGrid(guard, 100, 30);
  assert.match(rowFor(guard, "⊘︎ false").trimEnd(), /✗︎$/, "a collapsed parent surfaces the worst it hides");
  assert.match(rowFor(plain, "⊘︎ false").trimEnd(), /⊘︎$/, "expanded, it reads its own skip");
});

test("a fire streams into both panes, and the panes' totals are the rows they draw", () => {
  // The live half, folded **incrementally**: no daemon runs under `node --test`, so motion is
  // driven by cutting the same capture at three points and asserting the panes grow with it —
  // which is the evidence a live fire would produce.
  const cuts = [3, 10, 20];
  const seen = [];
  for (const cut of cuts) {
    let trace = 0;
    const { board, at } = foldCapture("trace-burst.jsonl", (f) => f.type === "trace" && trace++ >= cut);
    const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", run: runState("deep") };
    const rows = layout(board, options);
    assertGrid(rows, 100, 30);
    const metrics = runMetrics(board, options);
    // The clamp==render identity: the total the scroll clamps against is the row count the pane
    // really drew. On this capture no leaf is expanded or failed, so `withBodies` interleaves
    // nothing and this is the node walk alone — the *interleaved* case, where the desync it
    // exists to close would actually bite, is asserted over the expanded and auto-expanded
    // boards above.
    const drawn = paneBody(rows).length;
    assert.equal(metrics.treeTotal, drawn, `at cut ${cut} the tree's total is what it painted`);
    seen.push(drawn);
  }
  assert.deepEqual(
    seen,
    [...seen].sort((a, b) => a - b),
    `the tree grows as the capture is folded further: ${seen.join(" → ")}`,
  );
  assert.ok(seen[2] > seen[0], "…and it really grew, rather than staying one row");

  // The log half, over the capture's own `log` frames: the pane holds every line the ring holds,
  // in the ring's own order, at every cut.
  let logs = 0;
  const { board, at } = foldCapture("trace-burst.jsonl", (f) => f.type === "log" && logs++ >= 8);
  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", run: runState("deep", { focus: "log" }) };
  const body = paneBody(layout(board, options));
  const ring = board.services.deep.log.lines.map((e) => e.text);
  assert.ok(ring.length > 0, "the capture's log frames really folded");
  assert.equal(runMetrics(board, options).logTotal, body.length, "the log's total is what it painted");
  // Trimmed of the pad every row carries out to the pane's width, which is a cell fact rather
  // than a content one.
  assert.deepEqual(body.slice(0, ring.length).map((l) => l.trimEnd()), ring, "the pane is the ring, in order");
});

test("Tab swaps the pane, and the footer follows the one on screen", () => {
  // **One** board, two screens, so the two cannot both be right by accident. The tree is
  // following its frontier while the log is frozen, which is what makes the `f ●/○` indicator
  // non-vacuous: the same board reads `●` on one screen and `○` on the other.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const state = runState("deep", { tree: "follow", log: 0 });
  const options = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" };
  const onTree = layout(board, { ...options, run: { ...state, focus: "tree" } });
  const onLog = layout(board, { ...options, run: { ...state, focus: "log" } });
  assertGrid(onTree, 100, 30);
  assertGrid(onLog, 100, 30);
  const titleOf = (rows) => screenText(rows).split("\n")[2];
  assert.equal(titleOf(onTree), `Tree${" ".repeat(96)}`, "the tree pane names itself");
  assert.match(titleOf(onLog), /^Log · all · /, "…and so does the log");
  assert.notDeepEqual(paneBody(onTree), paneBody(onLog), "the body is the shown pane's, not both");
  const footerOf = (rows) => screenText(rows).split("\n").slice(-3).join("\n");
  // The nav hints are the shown pane's: the tree teaches its cursor, the log its scroll.
  assert.ok(footerOf(onTree).includes("j/k move"), "the tree footer teaches the cursor");
  assert.ok(!footerOf(onTree).includes("scroll"), "…and not the log's scroll");
  assert.ok(footerOf(onLog).includes("k/j or ↑↓ scroll"), "the log footer teaches the scroll, aliases and all");
  assert.ok(!footerOf(onLog).includes("move"), "…and not the tree's cursor");
  // `Tab` names its **destination**, and so does `s`.
  assert.ok(footerOf(onTree).includes("Tab log"), "`Tab` names where it lands");
  assert.ok(footerOf(onLog).includes("Tab tree"), "…from either side");
  assert.ok(footerOf(onTree).includes("s node output"), "`s` goes somewhere from the tree");
  assert.ok(footerOf(onLog).includes("s scope"), "…and toggles in place on the log");
  // The follow indicator is the **focused** pane's own state.
  assert.ok(footerOf(onTree).includes("f follow ●"), "the tree is following its frontier");
  assert.ok(footerOf(onLog).includes("f follow ○"), "the log is frozen, on the same board");
  // Scoped, the log pane's title says which node it is filtered to — through the same
  // `node_line` the tree row's headline reads, so the two cannot spell one node two ways.
  const scoped = layout(board, {
    ...options,
    run: { ...state, focus: "log", scoped: true, cursor: 5 },
  });
  assertGrid(scoped, 100, 30);
  assert.match(titleOf(scoped), /^Log · times 2\/2 · /, "the scope names the cursor's node, relabel and all");
});

test("the run view's pinned header is the list's own row for that service", () => {
  // The header band claims to be the run service's *list row*, composed through the one
  // `bodyRowCells` seam — so it has to shed what the list sheds and say what the list says at
  // every width, which is the JS twin of the Rust's
  // `the_run_headers_columns_track_the_lists_at_every_ladder_step`. One board, one predicate,
  // both surfaces, **swept** across the ladder rather than pinned at a rung a shed threshold
  // could move out from under.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const rowText = (rows) => rows.map((row) => row.map((c) => c.text).join(""));
  // The row is found by **index** rather than by a name needle: below ~31 cells the `Service`
  // column clips `deep` to `dee`, and a needle that misses there would quietly stop asserting
  // exactly where the ladder is most interesting. The list body opens one row under the column
  // header, which is the only row that names the `Service` column.
  const listRows = visibleRows(board, { filter: "", collapsed: new Set() });
  const at_ = listRows.findIndex((row) => row.kind === "service" && row.svc.name === "deep");
  assert.ok(at_ >= 0, "the capture's `deep` still has a list row");
  // Compared with runs of blanks collapsed: the pad is the one thing the two legitimately
  // spend differently below the stretch floor (see the byte-identity sweep below), so squashing
  // it leaves exactly the columns, their order and their values — which must not differ at all.
  const squash = (line) => line.trim().replace(/ {2,}/gu, " ");
  let identical = null;
  for (let cols = 20; cols <= 200; cols += 1) {
    const options = { cols, rows: 40, now: at + 1000, version: "0.2.123" };
    const header = rowText(layout(board, { ...options, run: runState("deep") }))[0];
    const list = rowText(layout(board, { ...options, selected: 0 }));
    const listed = list[list.findIndex((l) => l.startsWith("Service")) + 1 + at_];
    assert.equal(squash(header), squash(listed), `the header and the row say the same at ${cols} cells`);
    if (header !== listed) identical = null;
    else if (identical === null) identical = cols;
  }
  // …and above the **stretch floor** they are byte-identical, because there `serviceColWidth`
  // ignores the content fit entirely and both rows take the pure remainder. Below it the list
  // fits its column to the widest identity over every row while the header — the only row on
  // its screen — fits its own, which is the one deliberate difference and is why the sweep
  // above squashes. Found rather than asserted at 90, so a moved floor reads as a moved number.
  assert.equal(identical, 90, "the two rows agree byte for byte from the stretch floor up");
});

test("the layout reads the session run openRun mints", () => {
  // The one coupling between the two suites: these screens are dialled by hand, so the shape
  // they are dialled into has to be the shape a keypress really produces. Pressing `o` on a
  // service row of a folded capture and rendering **that** session is what pins it.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const opened = press(newSession(), board, { ctrl: false, key: "o" }, { now: at, bodyHeight: 20 });
  assert.notEqual(opened.session.run, null, "`o` opened a run view");
  assert.deepEqual(
    Object.keys(opened.session.run).sort(),
    Object.keys(runState("deep")).sort(),
    "the fields these screens dial are the fields the session holds",
  );
  const rows = layout(board, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", run: opened.session.run });
  assertGrid(rows, 100, 30);
  // It really is the run view rather than the list falling through: the list's column header is
  // gone and the pane's own title is there.
  assert.ok(!screenText(rows).includes("Last Activity"), "the list's column header is not on screen");
  assert.match(screenText(rows).split("\n")[2], /^Log · all · /, "the run view opens on the log pane");
});

test("both panes carry the scrollbar over their own total, viewport and top", () => {
  // At a viewport nothing overflows there is no bar at all — a track drawn over a pane you can
  // see the whole of is a lie about there being more.
  const { board, at } = liveBoard("trace-burst.jsonl");
  const tall = { cols: 100, rows: 30, now: at + 1000, version: "0.2.123", run: runState("deep") };
  const bar = (rows) => paneBody(rows).map((l) => l.slice(-1));
  assert.ok(
    runMetrics(board, tall).treeTotal <= runMetrics(board, tall).viewport,
    "the tree fits this viewport",
  );
  assert.deepEqual([...new Set(bar(layout(board, tall)))], [" "], "a pane that fits draws no bar");

  // Short enough that both panes overflow. The thumb is proportional, at least one row, never the
  // whole track — so there is always a visible "there is more" — and it rides the pane's own
  // `total`/`viewport`/`top` rather than a second count.
  for (const [focus, scroll] of [["tree", {}], ["log", { focus: "log" }]]) {
    const short = { cols: 100, rows: 12, now: at + 1000, version: "0.2.123", run: runState("deep", scroll) };
    const { viewport, treeTotal, logTotal } = runMetrics(board, short);
    const total = focus === "tree" ? treeTotal : logTotal;
    assert.ok(total > viewport, `the ${focus} pane (${total}) overflows its viewport (${viewport})`);
    const rows = layout(board, short);
    assertGrid(rows, 100, 12);
    const track = rows.slice(4, 4 + viewport).map((row) => row[row.length - 1].text);
    assert.equal(track.length, viewport, `the ${focus} track is the viewport's own height`);
    assert.deepEqual(
      [...new Set(track)].sort(),
      ["░", "█"].sort(),
      `the ${focus} track is drawn in both glyphs`,
    );
    const thumb = track.filter((g) => g === "█").length;
    assert.ok(thumb >= 1, "the thumb is at least a row");
    assert.ok(thumb < viewport, "…and never the whole track");
    // Following pins to the end, so the thumb sits at the bottom of its track.
    assert.equal(track[track.length - 1], "█", `a following ${focus} pane rides the bottom`);
    // …and a frozen top moves it: the same pane at row 0 puts the thumb at the top instead.
    const frozen = layout(board, {
      ...short,
      run: runState("deep", { ...scroll, [focus]: 0, cursor: 1 }),
    });
    assert.equal(
      frozen.slice(4)[0][frozen[4].length - 1].text,
      "█",
      `a ${focus} pane frozen at the top puts the thumb there`,
    );
  }
});

test("the rail glyphs measure as the tui measures them", () => {
  // Every glyph `treeview::kind_glyph` and `status_glyph` put on a row, keyed by the Rust's own
  // arm names and pinned to the width `unicode-width` 0.2 gives it in the terminal.
  //
  // This gate exists because `assertGrid` **structurally cannot** catch a mismeasured glyph:
  // both sides of its `cell.width === textWidth(cell.text)` check go through the one
  // measurement, and a golden's role/weight planes are `letter.repeat(c.width)`, so a glyph
  // measured 1 and painted 2 passes every other assertion in this file while overrunning its
  // `--cells × --cell-w` box in the browser. `🚀` did exactly that until this test was written.
  const kind = {
    Workflow: ["🚀", 2],
    Agent: ["🤖", 2],
    Cmd: ["⚙︎", 1],
    Tool: ["🔧", 2],
    Sandbox: ["🔒", 2],
    Worktree: ["🌿", 2],
    Guard: ["⊘︎", 1],
    Parallel: ["∥︎", 1],
    Repeat: ["🌀", 2],
    Sleep: ["⏱︎", 1],
    Fs: ["✎︎", 1],
    Lifecycle: ["⚑︎", 1],
  };
  const status = { Ok: ["✓︎", 1], Failed: ["✗︎", 1], Skipped: ["⊘︎", 1], Running: ["⋯︎", 1], recovered: ["↻︎", 1] };
  for (const [arm, [glyph, want]] of [...Object.entries(kind), ...Object.entries(status)]) {
    assert.equal(textWidth(glyph), want, `${arm}'s ${glyph} is ${want} cell(s)`);
  }
  // Read the two tables **out of the Rust** and hold this one to them, so a glyph changed or
  // added there reddens here rather than shipping a row that overruns behind a golden that pins
  // the overrun as correct.
  const rust = readFileSync(join(REPO, "crates", "tui", "src", "treeview.rs"), "utf8");
  const armsOf = (from) => {
    const body = rust.slice(rust.indexOf("{", from), rust.indexOf("\n}", from));
    return [...body.matchAll(/NodeKind::(\w+) => "([^"]+)"/g)].map(([, arm, glyph]) => [arm, glyph]);
  };
  const kinds = armsOf(rust.indexOf("pub fn kind_glyph"));
  assert.equal(kinds.length, Object.keys(kind).length, "treeview.rs still spells twelve kind glyphs");
  for (const [arm, glyph] of kinds) {
    assert.equal(unescapeRust(glyph), kind[arm]?.[0], `${arm}'s glyph is the Rust's`);
  }
  // The status arms pair several statuses onto one glyph (`Killed` folds into `✗`,
  // `Running`/`Unknown` both read `⋯`), so they are matched as a **set** of glyph literals.
  const from = rust.indexOf("pub(crate) fn status_glyph");
  const statusBody = rust.slice(from, rust.indexOf("\n}", from));
  const spelled = new Set(
    [...statusBody.matchAll(/"((?:[^"\\]|\\u\{[0-9a-f]+\})+)"/g)].map(([, lit]) => unescapeRust(lit)),
  );
  assert.deepEqual(
    [...spelled].sort(),
    [...new Set(Object.values(status).map(([g]) => g))].sort(),
    "the status glyphs are the Rust's, recovered marker included",
  );
});

/// One Rust string literal's `\u{XXXX}` escapes resolved — the VS15 selector every width-1 rail
/// glyph carries is spelled that way in `treeview.rs`, and it is the whole point of the
/// comparison, so it cannot be dropped. `JSON.parse` rejects the brace form, hence this.
function unescapeRust(literal) {
  return literal.replace(/\\u\{([0-9a-fA-F]+)\}/g, (_, hex) => String.fromCodePoint(parseInt(hex, 16)));
}
