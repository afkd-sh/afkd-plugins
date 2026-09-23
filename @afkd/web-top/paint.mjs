// The painter: cell rows to DOM text, and nothing else.
//
// Every layout decision — what a row says, how wide each run is, which palette role it wears,
// where the selection bar falls — was made by `layout.mjs`, which is pure and DOM-free. This
// module's whole job is to put what came out of there on screen: one `<div>` per row, one
// `<span>` per run of cells sharing a look, a class list naming the roles, and the run's own
// **cell count** as its box width. It measures nothing itself, branches on no content, and
// sets no colour of its own; the stylesheet owns the hexes and the geometry.
//
// That last part is not a flourish. A browser does not lay a monospace grid out the way a
// terminal does: the emoji and CJK a board is full of fall back to whatever face has them, at
// whatever advance that face uses, and a row painted as plain text drifts out of column at the
// first 🔹. So each run is given a box `<cells>` probed cells wide and clips — the layout
// already decided that a cluster is two cells, and the box is sized to *that* rather than to
// what the font happened to do. Driving the real page is what turned that from a design note
// into a bug: at 1200×700 the `State` column landed on eight different pixel columns, one per
// row.
//
// A run's box only holds its *edges*, though. Inside one, a glyph drawn wider than its cells
// still pushes everything after it right, and the box then clips the run's last cell: a
// `▷ Queued 1h 18m` whose `▷` came from a wider face read `▷ Queued 1h 18`. So only printable
// ASCII, the one repertoire every monospace face draws at its own advance, coalesces into
// runs; every other cluster gets a box of its own, the way a terminal gives each cell one.
//
// And like a terminal, the page keeps that glyph inside its box. Left where a fallback face
// puts it, a `▷` drawn a full em wide lays itself across the blank after it and `▷ Queued`
// reads `▷Queued`; clipped, it loses its right edge; shrunk until that em fits one cell, it is
// a speck, since most of the em is margin. So the one thing the painter cannot know — where
// the visitor's face really puts a glyph's ink — is handed in by `top.mjs`, and the glyph's
// box gets the indent (`--shift`) that centres the ink on its cells and the factor (`--fit`)
// that shrinks it only when it is wider than they are.
//
// Keeping the split strict is the rest of the point. It is what lets the whole dashboard be
// rendered to text and diffed against committed golden screens under `node --test` with no
// browser in sight — and what will let a later card put a key on it without a layout decision
// hiding in the DOM.

import { clusters } from "./layout.mjs";

/// Whether two cells paint identically, and so may be coalesced into one `<span>`. Adjacent
/// runs of one look are the common case (a row of blanks, a name, a pad), so this keeps a
/// hundred-cell row at a handful of nodes rather than one per run.
function alike(a, b) {
  return a.fg === b.fg && a.bg === b.bg && a.dim === b.dim && a.bold === b.bold && a.italic === b.italic;
}

/// Whether a cluster is printable ASCII, and so safe to share a box with its neighbours.
const PLAIN = /^[\x20-\x7e]+$/;

/// Where a glyph sits in the `cells` it was given, from how the visitor's face draws it: its
/// `advance`, and its ink's `left` and `right` edges from the pen, all in cells. `fit` is the
/// factor it is drawn at and `shift` how far its pen moves, in cells.
///
/// A face that draws the glyph at the grid's own advance (to a hundredth: layout rounding)
/// placed it in its cell on purpose — a `├` meets the next cell's line at the edge — so it is
/// left exactly where it is. Any other face is a fallback, whose margins say nothing about a
/// cell, so its ink is centred on the cells at its own size and shrunk only when it is wider
/// than they are. A glyph with no ink has nothing to place.
function placeOf(metrics, cells) {
  const { advance, left, right } = metrics ?? {};
  const ink = right - left;
  if (!(ink > 0) || Math.abs(advance - cells) <= cells / 100) return { fit: 1, shift: 0 };
  const fit = ink > cells ? Math.floor((cells / ink) * 1000) / 1000 : 1;
  return { fit, shift: Math.round(((cells - ink * fit) / 2 - left * fit) * 1000) / 1000 };
}

/// The class list one run's look resolves to — `fg-<role>` always, `glyph` on a box of its
/// own, `bg-<role>` when the run carries a band, and the dim, bold and italic attributes. The
/// role names come straight off the cell, so a role the stylesheet does not spell shows up as
/// unstyled text rather than as a wrong colour.
function classesOf(cell) {
  const classes = [`fg-${cell.fg}`];
  if (cell.plain === false) classes.push("glyph");
  if (cell.bg !== null) classes.push(`bg-${cell.bg}`);
  if (cell.dim) classes.push("dim");
  if (cell.bold) classes.push("bold");
  if (cell.italic) classes.push("italic");
  return classes.join(" ");
}

/**
 * Paint `rows` — the array of cell arrays `layout()` returned — into `root`.
 *
 * Row `<div>`s are **reused** across repaints and only their text and class lists rewritten,
 * so a 1 Hz repaint of a full screen does not churn the DOM: the node count follows the
 * viewport, not the frame count. Rows past the new screen's height are dropped and missing
 * ones appended, which is the only structural work a resize costs.
 *
 * `measure(text, classes)` is how the page's face really draws a glyph dressed in the
 * classes it is painted with: `{ advance, left, right }`, in cells from the pen. Without one,
 * every glyph is drawn where its face puts it.
 */
export function paint(root, rows, measure = null) {
  while (root.childElementCount > rows.length) root.lastElementChild.remove();
  while (root.childElementCount < rows.length) {
    const row = document.createElement("div");
    row.className = "row";
    root.append(row);
  }
  rows.forEach((cells, i) => {
    const row = root.children[i];
    // Coalesce first, then reconcile against what the row already holds, so the common case
    // — a repaint that changes one countdown — rewrites one span's text and touches nothing
    // else.
    const runs = [];
    for (const cell of cells) {
      for (const { text, width } of clusters(cell.text)) {
        const plain = PLAIN.test(text);
        const last = runs[runs.length - 1];
        if (plain && last !== undefined && last.plain && alike(last, cell)) {
          last.text += text;
          last.width += width;
        } else {
          runs.push({ text, width, plain, fg: cell.fg, bg: cell.bg, dim: cell.dim, bold: cell.bold, italic: cell.italic });
        }
      }
    }
    while (row.childElementCount > runs.length) row.lastElementChild.remove();
    while (row.childElementCount < runs.length) row.append(document.createElement("span"));
    runs.forEach((run, at) => {
      const span = row.children[at];
      const classes = classesOf(run);
      if (span.className !== classes) span.className = classes;
      // The run's own box, in cells — `--cells`, which the stylesheet turns into a width of
      // probed cells. A run the visitor's font draws wider than the layout budgeted clips here
      // rather than shoving the columns right of it.
      const cells = String(run.width);
      if (span.dataset.cells !== cells) {
        span.dataset.cells = cells;
        span.style.setProperty("--cells", cells);
      }
      // …and where its glyph sits inside that box — `--fit` and `--shift`, which the stylesheet
      // turns into a font size and an indent. Printable ASCII is drawn at the grid's own
      // advance, so only a glyph is measured; every span carries both, so a reused one never
      // keeps a stale glyph's.
      const place = run.plain || measure === null ? { fit: 1, shift: 0 } : placeOf(measure(run.text, classes), run.width);
      const fit = String(place.fit);
      const shift = String(place.shift);
      if (span.dataset.fit !== fit) {
        span.dataset.fit = fit;
        span.style.setProperty("--fit", fit);
      }
      if (span.dataset.shift !== shift) {
        span.dataset.shift = shift;
        span.style.setProperty("--shift", shift);
      }
      if (span.textContent !== run.text) span.textContent = run.text;
    });
  });
}
