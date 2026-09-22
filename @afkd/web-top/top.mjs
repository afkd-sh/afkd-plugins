// The client: it opens the relay's stream, folds every frame into a board, measures the
// viewport in cells, lays the board out on that grid and paints it.
//
// The two modules under it decide everything and know nothing about a browser: `fold.mjs` is
// the board (pure, DOM-free, clock-free) and `layout.mjs` is the screen (pure, DOM-free,
// cells rather than pixels). This module is the shell around them — the connection, the
// clock, the cell probe and the resize — and it is deliberately the only file here that names
// `document` or `window`. `session.mjs` is the third pure module (this tab's cursor, folds,
// filter, modal, overlay and flash) and `input.mjs` the key seam, which reaches the DOM only
// through the one element it is handed.

import { fold, seed } from "./fold.mjs";
import { installKeys } from "./input.mjs";
import { bodyHeight, cell, infoScrollMax, layout, runMetrics, textWidth, truncateWidth } from "./layout.mjs";
import { paint } from "./paint.mjs";
import { flashOf, needleOf, newSession, noteFrame, rowsOf, selectedIndex, typingOf } from "./session.mjs";

const probe = document.getElementById("probe");
const screen = document.getElementById("screen");

/// The probe's own length in cells, so the advance is a hundredth of a measured run rather
/// than one glyph's rounded box.
const PROBE_CELLS = 100;
/// A trailing throttle on the repaint, so an attach burst cannot melt the tab: the fold runs
/// on every frame, the layout and the paint do not.
const REPAINT_MS = 250;
/// The idle repaint cadence — `shell::HEARTBEAT_CADENCE`. The countdowns, the activity ages
/// and the badge elapseds are all measured against `now`, so they tick off this whether or
/// not a frame arrives.
const HEARTBEAT_MS = 1000;

let board = seed();
// This tab's own cursor, folds, filter, modal, overlay and flash. It is per **subscriber**:
// every browser gets its own attach from the relay, so every browser gets its own session, and
// nothing here is persisted or pushed back to the daemon.
let session = newSession();
let streamId = null;
let version = "";
let notice = "connecting";
let repaint = null;

/// The viewport in cells. The probe is re-read on every measure rather than cached: a zoom, a
/// font swap and a device-pixel-ratio change each move the advance without moving the layout,
/// and a cached advance would leave every row a fraction off.
function grid() {
  const box = probe.getBoundingClientRect();
  const advance = box.width / PROBE_CELLS;
  const line = box.height;
  if (!(advance > 0) || !(line > 0)) return { cols: 80, rows: 24 };
  // The one measurement, published to the stylesheet as well as spent here. A run's box is
  // `--cells × --cell-w`, so the width a row is composed to and the width it paints at are the
  // same arithmetic — `1ch` is not, and on a host whose monospace stack resolves one face for
  // `ch` and paints with another the row overruns by a pixel a cell and the columns right of
  // the overrun are clipped away.
  const root = document.documentElement.style;
  root.setProperty("--cell-w", `${advance}px`);
  root.setProperty("--cell-h", `${line}px`);
  return {
    cols: Math.max(1, Math.floor(window.innerWidth / advance)),
    rows: Math.max(1, Math.floor(window.innerHeight / line)),
  };
}

/// The options `layout()` and `bodyHeight()` are both read through — one description of what
/// this tab is looking at, so the height a scroll is clamped against is the height that paints.
function view(cols, rows, now) {
  return {
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
  };
}

/// Lay the board out at whatever the viewport currently measures and paint it.
function render() {
  const { cols, rows } = grid();
  const rendered = layout(board, view(cols, rows, performance.now()));
  // The connection's own state is not board state — the fold paints nothing and knows nothing
  // about a socket — so it is overwritten onto the screen's last row, where a terminal would
  // put a flash. It is the one string on the page the layout did not produce.
  if (notice !== "" && rendered.length > 0) {
    // Measured in cells like every other row on the page: a daemon's refusal is its own
    // sentence and can carry anything, and a `slice` by UTF-16 unit would let one wide
    // grapheme overrun the grid it is the last row of.
    const text = truncateWidth(notice, cols);
    const pad = " ".repeat(Math.max(0, cols - textWidth(text)));
    rendered[rendered.length - 1] = [cell(text, { fg: "legend" }), cell(pad)];
  }
  paint(screen, rendered);
}

function schedulePaint() {
  if (repaint !== null) return;
  repaint = setTimeout(() => {
    repaint = null;
    render();
  }, REPAINT_MS);
}

function say(message) {
  notice = message;
  render();
}

const stream = new EventSource("/stream");

stream.addEventListener("stream", (e) => {
  // The handle a POST /command names to reach this subscriber's own attach, and the ring
  // bound the operator's `log_lines` settled — the fold itself is setting-blind, so the
  // number is handed to it here.
  const hello = JSON.parse(e.data);
  streamId = hello.id;
  board = seed({ logLines: hello.log_lines });
  say("connecting");
});

stream.addEventListener("welcome", (e) => {
  // The one field the page needs off the handshake. `fold` has no arm for it — a handshake is
  // not a wire frame — so the shell reads it and hands it to `layout` as an option, exactly
  // as it reads `log_lines` off `stream` and hands it to `seed`.
  version = JSON.parse(e.data).daemon ?? "";
  say("");
});

// Every unnamed event is one control-wire frame, forwarded verbatim. An unparseable one is
// dropped rather than taking the page down: the fold's whole posture is that a frame it
// cannot use is not an error.
stream.addEventListener("message", (e) => {
  let frame;
  try {
    frame = JSON.parse(e.data);
  } catch {
    return;
  }
  const now = performance.now();
  board = fold(board, frame, now);
  // The two frames `fold.mjs` will not fold, because they are not board state: a reload's
  // summary and a daemon refusal are footer lines, and this is where they become one.
  session = noteFrame(session, frame, now);
  schedulePaint();
});

stream.addEventListener("refused", (e) => {
  say(`refused: ${JSON.parse(e.data).message ?? e.data}`);
  stream.close();
});

stream.addEventListener("closed", () => {
  say("the daemon closed this stream; reload to reconnect");
  stream.close();
});

stream.addEventListener("bye", () => {
  say("the daemon is shutting down");
  stream.close();
});

stream.addEventListener("error", (e) => {
  // Two different things arrive here: the relay's own named `error` event, which carries a
  // sentence, and the browser's transport error, which carries nothing.
  say(e.data ? `error: ${JSON.parse(e.data).message}` : "disconnected");
});

// A resize re-measures and re-lays out; the terminal sheds against whatever pane it has, and
// this does the same against the viewport.
new ResizeObserver(() => render()).observe(document.documentElement);
setInterval(render, HEARTBEAT_MS);
// The first paint waits for the face the probe is measured in: measuring against a fallback
// and re-measuring after the swap would lay the first screen out on a grid the page never
// draws on.
document.fonts.ready.then(render);
render();

installKeys({
  element: screen,
  read: () => {
    const { cols, rows } = grid();
    const at = view(cols, rows, performance.now());
    return {
      session,
      board,
      streamId,
      bodyHeight: bodyHeight(board, at),
      // The info page's own ceiling, threaded exactly as the list's height is: the scroll is
      // clamped against the arithmetic that paints, not against a second count of the rows.
      infoMax: infoScrollMax(board, at),
      // The run view's viewport and its two panes' row totals, on the same terms and for the
      // same reason: every clamp a run-view key applies is against the walks that rendered.
      run: runMetrics(board, at),
    };
  },
  write: (next) => {
    session = next;
  },
  post: (body) =>
    fetch("/command", { method: "POST", body: JSON.stringify(body) })
      .then((reply) => (reply.ok ? { ok: true, message: "" } : reply.text().then((text) => ({ ok: false, message: faultOf(text) }))))
      // A fetch that never reached the relay is a refusal too, and a silent one would leave the
      // ack on screen claiming a command that was never posted.
      .catch((err) => ({ ok: false, message: `the command did not reach the relay: ${err.message}` })),
  now: () => performance.now(),
  // A key press repaints at once rather than waiting on the throttle: the throttle is there to
  // survive an attach burst, and an operator's own keystroke is not one.
  repaint: render,
});

/// The relay's fault sentence out of its JSON body, or the body itself when it is not JSON —
/// the page never guesses at a refusal it was told about.
function faultOf(text) {
  try {
    return JSON.parse(text).message ?? text;
  } catch {
    return text;
  }
}

// The stream handle, so a console can reach this subscriber's own attach.
window.afkdStream = () => streamId;
