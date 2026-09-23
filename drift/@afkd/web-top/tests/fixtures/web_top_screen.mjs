// A headless driver for `@afkd/web-top`'s renderer: it folds a captured JSONL of control-wire
// frames into a board, replays a few chords through the page's own session seam, lays that
// board out on a fixed `cols`×`rows` grid, and prints the screen one field per **column**.
//
// It exists for `drift.rs`'s drift leg, which diffs this screen against the one real
// `afkd top` paints on a PTY of the same size. The three modules it imports are DOM-free by
// construction — `top.mjs` is the only file in the plugin that names `document` or `window` —
// so no browser is needed here. The price is that the option bag `layout()` reads through has
// to be built in this file, and it is copied from `top.mjs`'s `view()` field for field: an
// option the page grows and this driver does not is then a visible omission rather than a
// silent divergence.
//
//   node web_top_screen.mjs --plugin <dir> --frames <jsonl> --cols 100 --rows 30 \
//                           --version <daemon version> --keys g
//
// `--plugin` is resolved at run time rather than hard-wired, so the leg's negative arm can
// point this at a mutated copy of the tree without touching the shipped one.
//
// The output is one line per screen row, its fields separated by U+001F, one field per screen
// **column**: a wide grapheme sits in its own field and the cell it covers is the empty field
// beside it — exactly the shape `vt100`'s grid has, which is what lets the two screens be
// compared column against column rather than char against char. A row that does not come to
// `--cols` fields is a width bug in the renderer, and exits 2 with the row quoted rather than
// printing a short row the diff would then blame on the terminal.

import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

/// Die with a sentence on stderr. Exit 2 throughout, so the caller can tell "the driver
/// refused" from "node itself fell over".
function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(2);
}

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) {
  const flag = process.argv[i];
  if (!flag.startsWith("--")) fail(`expected a --flag, got ${JSON.stringify(flag)}`);
  args.set(flag.slice(2), process.argv[i + 1] ?? "");
}
const need = (name) => {
  const value = args.get(name);
  if (value === undefined || value === "") fail(`--${name} is required`);
  return value;
};

// The trailing slash matters: without it the last path segment is replaced rather than
// descended into, and every import would resolve one directory too high.
const root = pathToFileURL(`${resolve(need("plugin"))}/`);
const { fold, seed } = await import(new URL("fold.mjs", root));
const { bodyHeight, layout, textWidth } = await import(new URL("layout.mjs", root));
const { flashOf, needleOf, newSession, noteFrame, press, rowsOf, selectedIndex, typingOf } =
  await import(new URL("session.mjs", root));

const cols = Number(need("cols"));
const rows = Number(need("rows"));
const version = args.get("version") ?? "";
/// One press per character. `tab` is spelled `\t`, so the run view's pane swap is reachable
/// without a second flag — the only named key any leg needs.
const keys = [...(args.get("keys") ?? "")].map((c) => (c === "\t" ? "tab" : c));

// The fold is clock-driven — every badge elapsed, activity age and countdown is measured
// against the `now` it is handed — so the capture is replayed on a synthetic clock that only
// ever moves forward. The steps are small and even because the frames' *spacing* is not what
// this leg compares: every duration on either screen is normalised before the diff.
let board = seed();
let session = newSession();
let at = Date.now();
for (const line of readFileSync(need("frames"), "utf8").split("\n")) {
  if (line === "") continue;
  let frame;
  try {
    frame = JSON.parse(line);
  } catch (err) {
    fail(`a captured frame is not JSON (${err.message}): ${line}`);
  }
  board = fold(board, frame, (at += 10));
  // `top.mjs`'s own second call on every frame: a reload summary and a daemon refusal are not
  // board state, they are footer lines, and this is where they become one. Without it the two
  // frames that flash would render as nothing at all on this screen.
  session = noteFrame(session, frame, at);
}
const now = at + 10;

/// `top.mjs`'s `view()`, minus the viewport probe a browser does the measuring with.
const view = () => ({
  cols,
  rows,
  now,
  version,
  selected: selectedIndex(session, rowsOf(session, board)) ?? -1,
  offset: session.offset,
  filter: needleOf(session),
  collapsed: session.collapsed,
  typing: typingOf(session),
  flash: flashOf(session, now),
  confirm: session.confirm,
  help: session.help,
  info: session.info,
  infoOffset: session.infoOffset,
  run: session.run,
});
for (const key of keys) {
  // A chord is `{ctrl, key}`, the shape `keymap.mjs` resolves — a bare string resolves to
  // nothing at all and every press would be a silent no-op. `--keys` carries one character
  // per press, so `ctrl` is always false here; a Ctrl chord would need a spelling of its own.
  //
  // The commands a chord would post are dropped: this driver renders, it does not drive a
  // daemon. The legs only ever send chords that move the cursor or open a view.
  session = press(session, board, { ctrl: false, key }, { now, bodyHeight: bodyHeight(board, view()) }).session;
}

const SEPARATOR = "\u001f";
const segmenter = new Intl.Segmenter("en", { granularity: "grapheme" });
const out = [];
for (const [index, row] of layout(board, view()).entries()) {
  const fields = [];
  for (const { text } of row) {
    for (const { segment } of segmenter.segment(text)) {
      const width = textWidth(segment);
      if (width <= 0) {
        // A zero-width grapheme is not a cell of its own: it rides in the one before it,
        // which is what a terminal does with a combining mark too.
        if (fields.length > 0) fields[fields.length - 1] += segment;
        continue;
      }
      fields.push(segment);
      for (let covered = 1; covered < width; covered += 1) fields.push("");
    }
  }
  if (fields.length !== cols) {
    fail(
      `row ${index} came to ${fields.length} columns, not ${cols}: ` +
        JSON.stringify(row.map((c) => c.text).join("")),
    );
  }
  out.push(fields.join(SEPARATOR));
}
process.stdout.write(`${out.join("\n")}\n`);
