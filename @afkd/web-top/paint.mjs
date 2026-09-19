// The painter: cell rows to DOM text, and nothing else.
//
// Every layout decision — what a row says, how wide each run is, which palette role it wears,
// where the selection bar falls — was made by `layout.mjs`, which is pure and DOM-free. This
// module's whole job is to put what came out of there on screen: one `<div>` per row, one
// `<span>` per run of cells sharing a look, a class list naming the roles, and the run's own
// **cell count** as its box width. It measures nothing, branches on no content, and sets no
// colour of its own; the stylesheet owns the hexes and the geometry.
//
// That last part is not a flourish. A browser does not lay a monospace grid out the way a
// terminal does: the emoji and CJK a board is full of fall back to whatever face has them, at
// whatever advance that face uses, and a row painted as plain text drifts out of column at the
// first 🔹. So each run is given `width: <cells>ch` and clips — the layout already decided that
// a cluster is two cells, and the box is sized to *that* rather than to what the font happened
// to do. Driving the real page is what turned that from a design note into a bug: at 1200×700
// the `State` column landed on eight different pixel columns, one per row.
//
// Keeping the split strict is the rest of the point. It is what lets the whole dashboard be
// rendered to text and diffed against committed golden screens under `node --test` with no
// browser in sight — and what will let a later card put a key on it without a layout decision
// hiding in the DOM.

/// Whether two cells paint identically, and so may be coalesced into one `<span>`. Adjacent
/// runs of one look are the common case (a row of blanks, a name, a pad), so this keeps a
/// hundred-cell row at a handful of nodes rather than one per run.
function alike(a, b) {
  return a.fg === b.fg && a.bg === b.bg && a.dim === b.dim && a.bold === b.bold;
}

/// The class list one cell's look resolves to — `fg-<role>` always, `bg-<role>` when the cell
/// carries a band, and the two weight attributes. The role names come straight off the cell,
/// so a role the stylesheet does not spell shows up as unstyled text rather than as a wrong
/// colour.
function classesOf(cell) {
  const classes = [`fg-${cell.fg}`];
  if (cell.bg !== null) classes.push(`bg-${cell.bg}`);
  if (cell.dim) classes.push("dim");
  if (cell.bold) classes.push("bold");
  return classes.join(" ");
}

/**
 * Paint `rows` — the array of cell arrays `layout()` returned — into `root`.
 *
 * Row `<div>`s are **reused** across repaints and only their text and class lists rewritten,
 * so a 1 Hz repaint of a full screen does not churn the DOM: the node count follows the
 * viewport, not the frame count. Rows past the new screen's height are dropped and missing
 * ones appended, which is the only structural work a resize costs.
 */
export function paint(root, rows) {
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
      const last = runs[runs.length - 1];
      if (last !== undefined && alike(last, cell)) {
        last.text += cell.text;
        last.width += cell.width;
      } else {
        runs.push({
          text: cell.text,
          width: cell.width,
          fg: cell.fg,
          bg: cell.bg,
          dim: cell.dim,
          bold: cell.bold,
        });
      }
    }
    while (row.childElementCount > runs.length) row.lastElementChild.remove();
    while (row.childElementCount < runs.length) row.append(document.createElement("span"));
    runs.forEach((run, at) => {
      const span = row.children[at];
      const classes = classesOf(run);
      if (span.className !== classes) span.className = classes;
      // The run's own box, in cells — `--cells`, which the stylesheet turns into a `ch` width.
      // A glyph the visitor's font draws wider than the layout budgeted clips here rather than
      // shoving the columns right of it, which is the caveat a terminal already carries for a
      // glyph whose width its own font disagrees about.
      const cells = String(run.width);
      if (span.dataset.cells !== cells) {
        span.dataset.cells = cells;
        span.style.setProperty("--cells", cells);
      }
      if (span.textContent !== run.text) span.textContent = run.text;
    });
  });
}
