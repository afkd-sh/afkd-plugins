// The painter's own suite: `node --test @afkd/web-top/paint.test.mjs`.
//
// `paint.mjs` is the one module here that touches the DOM, so it is exercised against a
// **stub** one — a dozen lines of `createElement`/`append`/`remove`, which is all it uses.
// That is deliberate rather than a shortcut: the painter's whole contract is that it makes no
// layout decision, and a stub with no layout engine at all is the sharpest possible way to
// say so. Anything it got right only because a real browser was underneath would fail here.
//
// Both assertions below exist because driving the real page found the bugs they guard: a row
// whose runs were painted as plain text drifted out of column at the first emoji, because the
// cell count the layout measured never reached the DOM.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import { fold, seed } from "./fold.mjs";
import { cell, layout, textWidth } from "./layout.mjs";
import { paint } from "./paint.mjs";

const HERE = import.meta.dirname;

// --- the stub DOM --------------------------------------------------------------------

/// One element: children, a class, a text body, a `dataset` and a `style` that records what
/// was set on it. No layout, no measurement, no cascade — the painter needs none of those,
/// and a stub that offered them would let it start depending on one.
function element(tag) {
  const el = {
    tag,
    className: "",
    children: [],
    dataset: {},
    style: {
      props: {},
      setProperty(name, value) {
        el.style.props[name] = value;
      },
    },
    textContent: "",
    get childElementCount() {
      return el.children.length;
    },
    get lastElementChild() {
      return el.children[el.children.length - 1] ?? null;
    },
    append(child) {
      el.children.push(child);
      child.remove = () => {
        el.children.splice(el.children.indexOf(child), 1);
      };
    },
  };
  return el;
}

globalThis.document = { createElement: element };

/// The live board, folded from the recorded capture up to the drain — the same fixture the
/// layout's own suite renders.
function board() {
  let folded = seed({ logLines: 2000 });
  let at = 1_000_000;
  for (const line of readFileSync(join(HERE, "fixtures", "snapshot.jsonl"), "utf8").split("\n")) {
    if (line === "") continue;
    const frame = JSON.parse(line);
    if (frame.type === "meta" && frame.meta === "quitting") break;
    folded = fold(folded, frame, (at += 10));
  }
  return { board: folded, at };
}

// --- the contract --------------------------------------------------------------------

test("every run reaches the DOM carrying the cells it was measured at", () => {
  // The bug this guards: a browser does not lay a monospace grid out the way a terminal does.
  // `ui-monospace` has no 🔹 and, on many hosts, no CJK, so those fall back to a face at some
  // other advance — and a row painted as plain text drifts out of column at the first one.
  // The fix is that each run's box is sized from the count the layout measured, so the count
  // has to actually arrive: `--cells` on every span, summing per row to the grid's width.
  const { board: live, at } = board();
  const root = element("div");
  for (const cols of [100, 80, 60, 40]) {
    const rows = layout(live, { cols, rows: 30, now: at + 1000, version: "0.2.123" });
    paint(root, rows);
    assert.equal(root.children.length, 30, `${cols}: one div per screen row`);
    root.children.forEach((row, i) => {
      assert.equal(row.className, "row");
      let cells = 0;
      for (const span of row.children) {
        const declared = Number(span.style.props["--cells"]);
        assert.equal(declared, textWidth(span.textContent), `${cols}, row ${i}: ${JSON.stringify(span.textContent)} is boxed at ${declared} cells`);
        cells += declared;
      }
      assert.equal(cells, cols, `${cols}, row ${i}: the painted row is ${cells} cells`);
    });
  }
});

test("a repaint reuses its nodes and rewrites only what moved", () => {
  // A 1 Hz repaint of a full screen must not churn the DOM: the node count follows the
  // viewport, not the frame count. Painted twice a second apart, the same screen keeps every
  // node it had — and the countdowns that moved are the spans whose text changed.
  const { board: live, at } = board();
  const root = element("div");
  const opts = { cols: 100, rows: 30, version: "0.2.123" };
  paint(root, layout(live, { ...opts, now: at + 1000 }));
  const nodes = root.children.map((row) => [row, ...row.children]);
  const before = root.children.map((row) => row.children.map((s) => s.textContent).join(""));
  paint(root, layout(live, { ...opts, now: at + 2000 }));
  // Identity, span by span — not array identity, which the stub would satisfy even if every
  // node in it had been replaced.
  root.children.forEach((row, i) => {
    assert.equal(row, nodes[i][0], `row ${i} is the same div`);
    row.children.forEach((span, at) => {
      assert.equal(span, nodes[i][at + 1], `row ${i} span ${at} is the same node`);
    });
  });
  const after = root.children.map((row) => row.children.map((s) => s.textContent).join(""));
  assert.notDeepEqual(after, before, "a second of clock really moved the board");
  // …and only the rows that carry a clock moved: the column header and the `Queues` header
  // are the same strings they were.
  assert.equal(after[3], before[3], "the column header did not move");

  // A shorter screen drops rows rather than leaving orphans behind.
  paint(root, layout(live, { ...opts, rows: 12, now: at + 3000 }));
  assert.equal(root.children.length, 12, "the screen shrank to the new viewport");
});

test("the painter styles from roles and sets no colour of its own", () => {
  // The other half of the split: the painter names classes and never a hex. A colour here
  // would be a tone outside the palette the stylesheet is scanned for.
  const source = readFileSync(join(HERE, "paint.mjs"), "utf8");
  assert.equal(source.match(/#[0-9a-fA-F]{3,8}\b/), null, "paint.mjs spells no colour");
  const { board: live, at } = board();
  const root = element("div");
  paint(root, layout(live, { cols: 100, rows: 30, now: at + 1000, version: "0.2.123" }));
  const classes = new Set(root.children.flatMap((row) => row.children.map((s) => s.className)));
  for (const list of classes) {
    for (const name of list.split(" ")) {
      assert.match(name, /^(fg-[a-z-]+|bg-[a-z-]+|dim|bold|italic|glyph)$/, `${name} is a role, a weight or the glyph box`);
    }
  }
  // Every look the layout puts on a cell reaches the DOM — not just the roles. A painter that
  // dropped a weight would paint a page flat while every golden stayed green, because the bit
  // it lost never left `layout.mjs`.
  for (const name of ["dim", "bold", "bg-selection"]) {
    assert.ok([...classes].some((c) => c.split(" ").includes(name)), `the screen publishes ${name}`);
  }
});

test("a cell's whole look reaches its span, and a run splits on any of it", () => {
  // The coalescer's contract, over a row built to break it: six cells whose looks differ one
  // field at a time — weight, then weight again, then role, then band — and two two-cell
  // graphemes in the last of them. Adjacent cells merge only when **all four** fields agree,
  // and a grapheme outside ASCII never merges at all, so this row paints as seven spans.
  const root = element("div");
  const row = [
    cell("Aa", { fg: "accent", bold: true }),
    cell("Bb", { fg: "accent" }),
    cell("Cc", { fg: "accent", dim: true }),
    cell("Dd", { fg: "accent" }),
    cell("Ee", { fg: "ok" }),
    cell("監視", { fg: "ok", bg: "selection" }),
  ];
  paint(root, [row]);
  const spans = root.children[0].children;
  assert.deepEqual(
    spans.map((s) => s.className),
    ["fg-accent bold", "fg-accent", "fg-accent dim", "fg-accent", "fg-ok", "fg-ok glyph bg-selection", "fg-ok glyph bg-selection"],
    "each field of the look is on the span, and each one splits the run",
  );
  assert.deepEqual(spans.map((s) => s.textContent), ["Aa", "Bb", "Cc", "Dd", "Ee", "監", "視"]);
  assert.deepEqual(
    spans.map((s) => s.style.props["--cells"]),
    ["2", "2", "2", "2", "2", "2", "2"],
    "…and each wide grapheme is boxed at the two cells it costs, not the one it counts",
  );
  // The other direction: one look across two cells really is one span, which is what keeps a
  // hundred-cell row at a handful of nodes.
  paint(root, [[cell("ab", { fg: "ink" }), cell("cd", { fg: "ink" })]]);
  assert.equal(root.children[0].children.length, 1, "two cells of one look coalesce");
  assert.equal(root.children[0].children[0].textContent, "abcd");
});

test("a glyph outside ASCII is boxed alone, so a wider face cannot clip the text after it", () => {
  // The bug a real browser showed: the `State` cell's `▷` came from a fallback face drawn
  // wider than its one cell, and inside one shared box it pushed ` Queued 1h 18m` right until
  // the box clipped its last cell and the page read `▷ Queued 1h 18`. Each such glyph now
  // owns a box of its measured width, so its overhang is the only thing that can clip, and
  // the ASCII after it still coalesces into one run starting on its own column.
  const root = element("div");
  paint(root, [[cell("▷ Queued 1h 18m", { fg: "info" }), cell("🔹 archivist", { fg: "ink" }), cell("├─ ", { fg: "recede" })]]);
  const spans = root.children[0].children;
  assert.deepEqual(
    spans.map((s) => [s.textContent, s.style.props["--cells"]]),
    [["▷", "1"], [" Queued 1h 18m", "14"], ["🔹", "2"], [" archivist", "10"], ["├", "1"], ["─", "1"], [" ", "1"]],
    "every non-ASCII cluster is a box of its own cells, and the ASCII between them one run",
  );
  // …and each such box is marked, so the stylesheet can let its overhang paint over the next
  // cell rather than clip it — while an ASCII run keeps the clip it has always had.
  assert.deepEqual(
    spans.map((s) => s.className.split(" ").includes("glyph")),
    [true, false, true, false, true, true, false],
    "the glyph boxes carry `glyph`, the ASCII runs do not",
  );
  const css = readFileSync(join(HERE, "dashboard.css"), "utf8");
  assert.match(css, /\.row > span\.glyph \{[^}]*overflow:\s*visible/, "a glyph box lets its overhang show");
});

test("a glyph from a fallback face is centred on its cells, and shrunk only as far as its ink needs", () => {
  // Two bugs a real browser showed, one after the other. Left at its own size, a fallback face
  // that draws `▷` a full em wide laid it across the blank after it and `▷ Queued` read
  // `▷Queued`. Shrunk until that *advance* fit one cell, the same face drew a speck, because
  // most of its advance is empty margin — and `▯` went the same way. So `top.mjs` measures
  // where a glyph's ink really falls, and a glyph its face does not draw at the grid's own
  // advance is centred on its cells at its own size, shrunk only when its ink is wider than
  // they are. A face that does draw it at the grid's advance placed it there on purpose — a
  // `├` meets its neighbour at the cell's edge — so it is left exactly where it is.
  const root = element("div");
  const asked = [];
  // In cells from the pen: the face's advance, and its ink's left and right edges.
  const drawn = {
    "▷": { advance: 2, left: 0.5, right: 1.75 }, // a CJK face's: a wide margin, ink wider than its cell
    "▯": { advance: 1.5, left: 0.5, right: 1 }, // a wide margin, ink that fits
    "●": { advance: 1.004, left: 0.1, right: 0.9 }, // the grid's own face, to layout rounding
    "🔹": { advance: 2.25, left: 0.25, right: 2 }, // an emoji face, a little wider than two cells
    "├": { advance: 1, left: 0.45, right: 1 }, // the grid's own face, meeting the next cell's line
    "　": { advance: 1.667, left: 0, right: 0 }, // a wide blank, with no ink to place
  };
  const measure = (text, classes) => {
    asked.push([text, classes]);
    return drawn[text];
  };
  const row = [
    cell("▷ Queued", { fg: "accent-dim" }),
    cell("▯", { fg: "recede" }),
    cell("● Idle", { fg: "idle", bold: true }),
    cell("🔹├　", { fg: "ink" }),
  ];
  paint(root, [row], measure);
  const spans = root.children[0].children;
  assert.deepEqual(
    spans.map((s) => [s.textContent, s.style.props["--fit"], s.style.props["--shift"]]),
    [
      ["▷", "0.8", "-0.4"],
      [" Queued", "1", "0"],
      ["▯", "1", "-0.25"],
      ["●", "1", "0"],
      [" Idle", "1", "0"],
      ["🔹", "1", "-0.125"],
      ["├", "1", "0"],
      ["　", "1", "0"],
    ],
    "a fallback glyph is centred and shrunk only past its ink, the grid's own face and ASCII are never touched",
  );
  assert.deepEqual(
    asked,
    [
      ["▷", "fg-accent-dim glyph"],
      ["▯", "fg-recede glyph"],
      ["●", "fg-idle glyph bold"],
      ["🔹", "fg-ink glyph"],
      ["├", "fg-ink glyph"],
      ["　", "fg-ink glyph"],
    ],
    "only a glyph is measured, and in the look it is painted in, since the weight moves the ink",
  );
  // A span the next frame hands an ASCII run gives its old placement back rather than keeping it.
  paint(root, [[cell("Queued ok", { fg: "accent-dim" })]], measure);
  const reused = root.children[0].children[0].style.props;
  assert.deepEqual([reused["--fit"], reused["--shift"]], ["1", "0"], "a reused span drops the glyph's placement");
  // With nothing measured — the stub DOM, a probe not laid out yet — everything is drawn as is.
  paint(root, [row]);
  const unmeasured = root.children[0].children.map((s) => [s.style.props["--fit"], s.style.props["--shift"]]);
  assert.ok(unmeasured.every(([fit, shift]) => fit === "1" && shift === "0"), "no measure, no placement");
  const css = readFileSync(join(HERE, "dashboard.css"), "utf8");
  assert.match(css, /\.row > span\.glyph \{[^}]*font-size:\s*calc\(var\(--fit, 1\) \* 1em\)/, "the stylesheet draws a glyph at its factor");
  assert.match(css, /\.row > span\.glyph \{[^}]*text-indent:\s*calc\(var\(--shift, 0\) \* var\(--cell-w\)\)/, "…moved by its shift in cells");
  assert.match(css, /\.row > span\.glyph \{[^}]*line-height:\s*var\(--cell-h\)/, "…on a box still a whole row tall");
  const shell = readFileSync(join(HERE, "top.mjs"), "utf8");
  assert.match(shell, /paint\(screen, rendered, .*measure\(/, "top.mjs hands the painter its measure");
});

test("the stylesheet spends what the painter publishes", () => {
  // The two halves of the geometry have to meet: the painter writes a per-run `--cells` and
  // `top.mjs` writes the probed `--cell-w`/`--cell-h`, and the stylesheet is the only place
  // they become a box. A rule that stopped reading either would leave the page laying itself
  // out on the font's own advance again, which is the bug the real page showed.
  const css = readFileSync(join(HERE, "dashboard.css"), "utf8");
  assert.match(css, /width:\s*calc\(var\(--cells[^)]*\) \* var\(--cell-w\)\)/, "a run's box is its cell count times the probed cell");
  assert.match(css, /height:\s*var\(--cell-h\)/, "a row's height is the probed line");
  assert.match(css, /overflow:\s*hidden/, "and a run the font drew wider than its box clips rather than shoving the row");
  const shell = readFileSync(join(HERE, "top.mjs"), "utf8");
  for (const property of ["--cell-w", "--cell-h"]) {
    assert.ok(shell.includes(`"${property}"`), `top.mjs publishes ${property} from its own probe`);
  }
});

test("the grid draws no frame round itself", () => {
  // The screen takes focus on load so it can take keys, and 0.3 drew an inset accent ring on
  // `:focus-visible` to say so — which `autofocus` matches, so the page opened with a cyan line
  // round the whole screen that `afkd top` never draws. Neither the browser's outline nor a ring
  // of the page's own may come back.
  const css = readFileSync(join(HERE, "dashboard.css"), "utf8");
  assert.match(css, /#screen:focus \{[^}]*outline:\s*none/, "the browser's outline is off");
  assert.doesNotMatch(css, /box-shadow|border:|outline:(?!\s*none)/, "and nothing else frames the grid");
});
