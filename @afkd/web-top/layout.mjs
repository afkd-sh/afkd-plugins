// The layout: the folded board turned into a character grid.
//
// `fold.mjs` answers *what the daemon is doing*; this answers *what that looks like on a
// screen of `cols × rows` cells*. In afkd that second half is `crates/tui/src/layout.rs`
// (13k lines), `crates/tui/src/legend.rs` and the width arithmetic in
// `crates/tui/src/shell.rs` — three private modules of an unpublished crate — so, like the
// fold, it is re-stated here arm for arm, and every arm cites the file it was read off
// because the two can drift and the citation is the only thing that makes the drift
// findable.
//
// Three properties this module holds on purpose:
//
// - **DOM-free.** It names neither `document` nor `window`, imports only `fold.mjs`'s
//   selectors, and takes the viewport as two numbers. `paint.mjs` turns what comes out of
//   here into DOM text and decides nothing; keeping that split strict is what lets the whole
//   layout be diffed against committed golden screens under `node --test`.
// - **Cells, never pixels.** A row is an array of `{text, fg, bg, dim, bold, width}` cells
//   whose `width` is the count of *terminal cells* the text occupies — a wide grapheme
//   carries `2` — so the painter sizes a row in cells rather than trusting the browser's
//   font metrics. Every row returned sums to exactly `cols`.
// - **Roles, never hexes.** `fg`/`bg` name a palette role (`"accent"`, `"alarm"`, …). The
//   hex lives only in `dashboard.css`, which is what makes "the page spends only the
//   product's palette" a closed question a test can read off one file.
//
// `now` is milliseconds on the caller's own monotonic clock — the same domain every anchor
// on the board was converted into at receipt, so a subtraction here is always two instants
// of one clock.

import { queues } from "./fold.mjs";
import {
  DEFAULT_KEYS,
  DESCRIPTIONS,
  HANDLED,
  NOTES,
  REFUSED_WHILE_QUITTING,
  SCOPES,
  all,
  glyphs,
  idOf,
} from "./keymap.mjs";

// --- measurement -------------------------------------------------------------------
//
// There is no `unicode-width` for javascript and this plugin takes no dependencies, so the
// width rule is modelled for the classes the board can actually emit rather than ported
// whole from UAX#11. `the_glyph_vocabulary_measures_as_the_tui_measures_it` names every
// glyph this file and the wire can put on screen and pins each one's width, so a glyph
// outside the modelled classes fails a test instead of silently mismeasuring a row.

/// Grapheme clusters, so a base scalar and its combining marks are measured as the one cell
/// they paint. `Intl.Segmenter` is the module's single platform assumption (node ≥ 16, every
/// browser since 2022); the plugin already assumes ES modules and `EventSource`.
const GRAPHEMES = new Intl.Segmenter("en", { granularity: "grapheme" });

/// The East Asian *Wide* and *Fullwidth* blocks a service name, a group name or a lane name
/// can realistically carry. Deliberately not the whole of UAX#11: the ranges left out are
/// *Ambiguous*, which `unicode-width` 0.2 — the crate the terminal measures with — resolves
/// to **one** cell, which is this table's own default. So `▶`, `●`, `▁`…`█`, `─`, `│` and the
/// rest of the board's box-drawing and geometric vocabulary measure 1 here exactly as they do
/// there.
const WIDE_RANGES = [
  [0x1100, 0x115f], // Hangul Jamo, initial consonants
  [0x2e80, 0x303e], // CJK radicals, Kangxi, CJK symbols and punctuation
  [0x3041, 0x33ff], // kana, Hangul compatibility jamo, CJK compatibility
  [0x3400, 0x4dbf], // CJK unified ideographs extension A
  [0x4e00, 0x9fff], // CJK unified ideographs
  [0xa000, 0xa4cf], // Yi
  [0xa960, 0xa97f], // Hangul Jamo extended-A
  [0xac00, 0xd7a3], // Hangul syllables
  [0xf900, 0xfaff], // CJK compatibility ideographs
  [0xfe10, 0xfe19], // vertical forms
  [0xfe30, 0xfe6f], // CJK compatibility forms, small form variants
  [0xff00, 0xff60], // fullwidth forms
  [0xffe0, 0xffe6], // fullwidth signs
  [0x1f300, 0x1f64f], // misc symbols and pictographs, emoticons
  [0x1f900, 0x1faff], // supplemental symbols and pictographs (🧵 🪦 …)
  [0x20000, 0x3fffd], // CJK unified ideographs, planes 2 and 3
];

/// U+FE0F, the emoji variation selector. A cluster carrying it is drawn in emoji
/// presentation and measures **two** cells even where its base scalar is Ambiguous or
/// Neutral — the dependency `layout::STOPPED_ICON` (`▪️`) and `layout::STALE_MARKER` (`🕸️`)
/// both already record, and the reason neither may be spelled without it.
const VS16 = "️";

/// Marks and format characters that take no cell of their own: a combining mark rides the
/// grapheme it modifies, and a zero-width joiner or bidi control paints nothing.
const ZERO_WIDTH = /^[\p{Mn}\p{Me}\p{Cf}]$/u;

/// The cells one **grapheme cluster** occupies: `0` for a lone zero-width scalar, `2` for an
/// emoji-presentation or East Asian Wide/Fullwidth cluster, `1` otherwise.
function clusterWidth(cluster) {
  if (cluster.length === 1 && ZERO_WIDTH.test(cluster)) return 0;
  if (cluster.includes(VS16)) return 2;
  const base = cluster.codePointAt(0);
  if (base === undefined) return 0;
  for (const [lo, hi] of WIDE_RANGES) {
    if (base >= lo && base <= hi) return 2;
  }
  return 1;
}

/**
 * The display width of `text` in terminal cells — the one measurement every width decision
 * in this file is made through, so a row's arithmetic and the cells it reports cannot
 * disagree.
 */
export function textWidth(text) {
  let width = 0;
  for (const { segment } of GRAPHEMES.segment(text)) width += clusterWidth(segment);
  return width;
}

/**
 * `text` cut to `budget` cells with a trailing `…` when it overruns — the mirror of
 * `treeview::truncate_width`, ellipsis cell and `budget.max(1)` floor included. Used
 * wherever the terminal elides rather than clips: the `Trigger` cell and a lane's name.
 */
export function truncateWidth(text, budget) {
  if (textWidth(text) <= budget) return text;
  const target = Math.max(1, budget) - 1;
  let out = "";
  let width = 0;
  for (const { segment } of GRAPHEMES.segment(text)) {
    const w = clusterWidth(segment);
    if (width + w > target) break;
    out += segment;
    width += w;
  }
  return out + "…";
}

/// `text` cut to `budget` cells with **no** ellipsis — what a ratatui `Constraint::Length`
/// cell does, and so what the fixed columns and the over-wide title bar do ("identity last",
/// `layout::plan_header`). A wide cluster straddling the edge is dropped whole rather than
/// half-painted, so the result can measure one cell short; the row's pad closes it.
function clipWidth(text, budget) {
  if (textWidth(text) <= budget) return text;
  let out = "";
  let width = 0;
  for (const { segment } of GRAPHEMES.segment(text)) {
    const w = clusterWidth(segment);
    if (width + w > budget) break;
    out += segment;
    width += w;
  }
  return out;
}

/// `text` padded out to `width` cells with spaces — by display width, never by `length`, so
/// a CJK name does not smear every column after it (`layout::queue_compose`'s rule).
function padWidth(text, width) {
  const pad = width - textWidth(text);
  return pad > 0 ? text + " ".repeat(pad) : text;
}

// --- cells and rows ----------------------------------------------------------------

/**
 * One cell run. `fg`/`bg` are palette **roles** — `dashboard.css` owns the hexes — `dim` and
 * `bold` are the terminal's two weight attributes, and `width` is the run's own measured
 * cell count, so a painter never measures anything.
 */
export function cell(text, options = {}) {
  return {
    text,
    fg: options.fg ?? "ink",
    bg: options.bg ?? null,
    dim: options.dim === true,
    bold: options.bold === true,
    width: textWidth(text),
  };
}

/// The summed cell width of a row.
function rowWidth(cells) {
  let width = 0;
  for (const c of cells) width += c.width;
  return width;
}

/// A run of blanks, for a pad or a whole empty row.
function blank(width, options = {}) {
  return cell(" ".repeat(Math.max(0, width)), options);
}

/**
 * `cells` fitted to exactly `cols`: whole cells while they fit, the crossing one clipped,
 * and a trailing pad. **The** invariant of this module — every row it returns sums to `cols`
 * — which is what makes "nothing wraps into a broken row" a property rather than a hope.
 */
function fitRow(cells, cols) {
  const out = [];
  let width = 0;
  for (const c of cells) {
    if (width >= cols) break;
    if (width + c.width <= cols) {
      if (c.width > 0) out.push(c);
      width += c.width;
      continue;
    }
    const text = clipWidth(c.text, cols - width);
    if (text !== "") {
      out.push({ ...c, text, width: textWidth(text) });
      width += textWidth(text);
    }
    break;
  }
  if (width < cols) out.push(blank(cols - width));
  return out;
}

/// Stamp the selection bar over a fitted row: the bar is a full-width band, which is why it
/// is applied after the pad rather than to the composed cells.
function selectRow(cells) {
  return cells.map((c) => ({ ...c, bg: "selection" }));
}

// --- the palette's roles -----------------------------------------------------------

/// Every role a cell may name. The stylesheet spells each one and nothing else, so the
/// palette scan is an equality rather than a containment: a role that stops being emitted is
/// as much a drift as a hex that appears from nowhere.
export const ROLES = [
  "accent",
  "accent-dim",
  "alarm",
  "bright",
  "caution",
  "idle",
  "ink",
  "legend",
  "muted",
  "ok",
  "recede",
  "selection",
];

// --- the state vocabulary ----------------------------------------------------------
//
// `crates/tui/src/model.rs`'s `Badge`: its glyph, its Title-case label, the three gates the
// footer reads off it, and the hue/weight `shell::badge_style` paints it at.

/// `Badge::glyph` — circles arm, squares tear down.
const BADGE_GLYPH = {
  Starting: "◌",
  Idle: "●",
  Queued: "▷",
  Checking: "◎",
  Busy: "▶",
  Stopping: "■",
  Stopped: "□",
  Crashed: "✕",
};

/// `shell::badge_style`'s role, and the bold/dim baseline it applies under it: `crashed` and
/// `busy` bold, `stopped`/`queued`/`checking` dim. The hue carries the tier at truecolor, so
/// the `dim` bit here is the terminal's own attribute carried forward, not a second colour.
const BADGE_STYLE = {
  Starting: { fg: "caution" },
  Idle: { fg: "idle" },
  Queued: { fg: "accent-dim", dim: true },
  Checking: { fg: "accent-dim", dim: true },
  Busy: { fg: "accent", bold: true },
  Stopping: { fg: "caution" },
  Stopped: { fg: "muted", dim: true },
  Crashed: { fg: "alarm", bold: true },
};

/// `Badge::can_start` — only a terminal (stopped/crashed) service starts.
function canStart(badge) {
  return badge === "Stopped" || badge === "Crashed";
}

/// `Badge::can_stop` — only a live one stops.
function canStop(badge) {
  return ["Starting", "Idle", "Queued", "Checking", "Busy"].includes(badge);
}

/// `Badge::can_fire` — only an armed, waiting one fires.
function canFire(badge) {
  return badge === "Idle";
}

/// `Badge::is_alive` — the header's `N up` tally.
function isAlive(badge) {
  return badge !== "Stopped" && badge !== "Crashed";
}

/// `CardModel::is_executing` — a command of the service's **own** is in flight, which is
/// what grows the row's diamond. Restated from `model.rs`, not from the badge alone.
function isExecuting(svc) {
  return svc.badge === "Starting" || svc.badge === "Stopping" || svc.inFlightSince !== null;
}

// --- durations ---------------------------------------------------------------------

/// `layout::format_two_units` — a whole unit drops its trailing zero.
function twoUnits(hi, hiUnit, lo, loUnit) {
  return lo === 0 ? `${hi}${hiUnit}` : `${hi}${hiUnit} ${lo}${loUnit}`;
}

/**
 * `layout::format_elapsed`: the two largest units — `45s`, `1m 2s`, `4h 3m`, `3d 4h`. The one
 * duration formatter on the board, so the header's uptime, a badge's elapsed, an activity
 * age and a countdown can never spell the same span two ways.
 */
export function formatElapsed(ms) {
  const secs = Math.floor(Math.max(0, ms) / 1000);
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return twoUnits(Math.floor(secs / 60), "m", secs % 60, "s");
  if (secs < 86400) {
    return twoUnits(Math.floor(secs / 3600), "h", Math.floor((secs % 3600) / 60), "m");
  }
  return twoUnits(Math.floor(secs / 86400), "d", Math.floor((secs % 86400) / 3600), "h");
}

/// `layout::format_tokens` — the largest unit that keeps the leading number small.
function formatTokens(n) {
  if (n < 1000) return `${Math.trunc(n)}`;
  if (n < 1000000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1000000).toFixed(1)}M`;
}

/// `layout::format_bytes` — powers of 1024, one decimal below ten, none at or above.
function formatBytes(n) {
  const units = ["k", "M", "G", "T"];
  if (n < 1024) return `${Math.trunc(n)}`;
  let value = n / 1024;
  let unit = 0;
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024;
    unit += 1;
  }
  const rounded = Math.round(value * 10) / 10;
  return rounded < 10
    ? `${rounded.toFixed(1)}${units[unit]}`
    : `${rounded.toFixed(0)}${units[unit]}`;
}

/// A number right-aligned in `width` cells — the `{:>3}`/`{:>4}` pads the strip spends so a
/// reading crossing an octave does not shift the row sideways.
function rightPad(text, width) {
  return text.length >= width ? text : " ".repeat(width - text.length) + text;
}

// --- the title bar -----------------------------------------------------------------
//
// `layout::plan_header` and `layout::header_right`: two clusters, a measured pad between
// them, and a five-rung shed ladder that drops whole telemetry segments right-to-left until
// they fit. The **skew** segment (`top <client> · `) is dropped here: the page is not a `top`
// build and has no second version to disagree with the daemon's.

/// `layout::HEADER_MIN_GAP` — the clusters collide, and the next rung is taken, when the pad
/// would fall below one space.
const HEADER_MIN_GAP = 1;

/// The left cluster's rest after the accent dot — ` <state> <elapsed> · <health>` — with
/// `collapsed` folding the tally into `<alive>/<total> up` (the last shed step).
/// `header_left_rest`.
function headerLeftRest(view, collapsed) {
  if (view.draining !== null) {
    const suffix = view.draining > 0 ? ` (${view.draining} busy)` : "";
    return ` ${view.stateWord} ${view.elapsed}${suffix}`;
  }
  const { alive, total } = view;
  const down = total - alive;
  let health;
  if (collapsed) health = `${alive}/${total} up`;
  else if (down > 0) health = `${alive} up · ${down} down`;
  else health = `${alive} up`;
  return ` ${view.stateWord} ${view.elapsed} · ${health}`;
}

/// The right telemetry cluster at shed `level`: `<tok> tok · $<cost>`. The ladder drops `tok`
/// at 1 and `cost` at 3; level 2 is the **target** segment's rung, which this page never
/// carries (it reaches its daemon through the relay's own socket, never a named endpoint), so
/// the rung is walked and spends nothing. `header_right`.
function headerRight(view, level) {
  const segs = [];
  if (level < 1) segs.push(`${view.tokens} tok`);
  if (level < 3) segs.push(`$${view.cost.toFixed(2)}`);
  return segs.join(" · ");
}

/// Plan the title bar at `cols`, walking the shed ladder to the first rung that fits (or the
/// last, then clipping — "identity last"). Returns the cells left-to-right.
function planHeader(view, cols) {
  const prefix = view.version === "" ? "afkd · " : `afkd ${view.version} · `;
  const fixedLeft = textWidth(prefix) + textWidth(view.dot);
  for (let level = 0; level <= 4; level += 1) {
    const leftRest = headerLeftRest(view, level >= 4);
    const right = headerRight(view, level);
    const leftW = fixedLeft + textWidth(leftRest);
    const rightW = textWidth(right);
    if (level === 4 || leftW + HEADER_MIN_GAP + rightW <= cols) {
      const pad = Math.max(0, cols - leftW - rightW);
      // The dot is the run state's own accent — `shell::header_accent_style`: green while
      // running, and while quitting it **is** the `Stopping` badge's caution, sourced from
      // the badge table rather than copied so the header's drain look cannot drift from the
      // list badge's. The rest stays calm while running and reads as one coloured run while
      // draining (`header_rest_style`).
      const accent = view.draining !== null ? BADGE_STYLE.Stopping.fg : "ok";
      const restFg = view.draining !== null ? accent : "ink";
      return [
        cell(prefix),
        cell(view.dot, { fg: accent }),
        cell(leftRest, { fg: restFg }),
        blank(pad),
        cell(right),
      ];
    }
  }
  /* c8 ignore next */
  throw new Error("the shed loop returns at level 4");
}

/// The typed header the ladder above sheds — `layout::HeaderView`, derived from the board.
function headerView(board, now, version) {
  let alive = 0;
  let draining = 0;
  let tokens = 0;
  let cost = 0;
  for (const name of board.order) {
    const svc = board.services[name];
    if (svc === undefined) continue;
    if (isAlive(svc.badge)) alive += 1;
    if (svc.inFlightSince !== null) draining += 1;
    tokens += svc.tokens;
    cost += svc.cost;
  }
  const quitting = board.quittingSince !== null;
  return {
    version,
    // `●` while running; the drain's dot is `Badge::Stopping`'s own glyph, asked for rather
    // than copied — a drain is a drain (`list_view`'s run-state match).
    dot: quitting ? BADGE_GLYPH.Stopping : "●",
    stateWord: quitting ? "Quitting" : "Running",
    // Each state names the anchor it is measured from, so the word and the number can never
    // disagree. A board that has seen no snapshot yet has no boot anchor and reads `0s`.
    elapsed: formatElapsed(quitting ? now - board.quittingSince : now - (board.daemonStartedAt ?? now)),
    draining: quitting ? draining : null,
    alive,
    total: board.order.length,
    tokens: formatTokens(tokens),
    cost,
  };
}

// --- the host-load strip -----------------------------------------------------------
//
// `layout::plan_load_strip` and ADR-0080: four elastic trend lanes, each hard against the
// number it produced, then a three-rung shed — the trends as one group, then net, then mem.

/// `layout::TREND_LADDER` — index 0 is the floor, so a zero reading draws `▁` and a blank
/// cell in a lane means one thing only: no history yet.
export const TREND_LADDER = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// `layout::TREND_MIN_CELLS` — below four of these the whole trend group sheds at once.
const TREND_MIN_CELLS = 6;
/// `layout::TREND_MAX_CELLS` — the deepest history any lane draws.
const TREND_MAX_CELLS = 256;
/// `layout::NET_SCALE_MIN` — the net lanes' noise floor, never a guess at the link's speed.
const NET_SCALE_MIN = 16 * 1024;
/// `layout::STRIP_MAX_AGE` in milliseconds — three heartbeats, the gap the walk cuts at and
/// the age past which the strip blanks rather than freezing a dead number.
const STRIP_MAX_AGE = 3000;
/// The strip's three field labels (ADR-0032 Title Case; `CPU` is an initialism). Each const
/// is the whole **drawn** run, trailing space and `Net`'s data `↓` included, so the width
/// arithmetic and the lane reader agree on one string. `layout::LOAD_*_LABEL`.
const LOAD_CPU_LABEL = "CPU ";
const LOAD_MEM_LABEL = "Mem ";
const LOAD_NET_LABEL = "Net ↓";

/// `layout::pct_level` — the absolute 0..=100 scale cpu and mem share, rounded half up. Never
/// a window autoscale: a box at 28% memory all day must *look* flat.
function pctLevel(pct) {
  const clamped = Math.min(100, Math.max(0, Math.trunc(pct)));
  return Math.min(Math.floor((clamped * 7 + 50) / 100), TREND_LADDER.length - 1);
}

/// `layout::mem_pct` — the memory segment's share, so the numbers beside the lane can read
/// `used/total` rather than a second percentage.
function memPct(load) {
  if (load.memTotal === 0) return 0;
  const doubled = Math.floor((200 * load.memUsed) / load.memTotal);
  return Math.min(100, Math.ceil(doubled / 2));
}

/// The smallest power of two at or above `n`; `1` for zero, matching Rust's
/// `checked_next_power_of_two`.
function nextPowerOfTwo(n) {
  let p = 1;
  while (p < n) p *= 2;
  return p;
}

/// `layout::net_scale` — one octave scale over **both** directions, taken over the window
/// actually drawn and floored at the noise floor. Scaling the two apart would draw a 2 MiB/s
/// download and a 20 KiB/s upload at the same height, throwing away the asymmetry the split
/// exists to show.
function netScale(readings, cells) {
  let peak = 0;
  for (const load of trendTail(readings, cells)) peak = Math.max(peak, load.rxBps, load.txBps);
  return Math.max(nextPowerOfTwo(peak), NET_SCALE_MIN);
}

/// `layout::rate_level` — a rate's ladder index against the octave, half up.
function rateLevel(v, scale) {
  const doubled = Math.floor((v * 14) / Math.max(1, scale));
  return Math.min(Math.ceil(doubled / 2), TREND_LADDER.length - 1);
}

/// `layout::trend_tail` — the newest `cells` readings, oldest → newest.
function trendTail(readings, cells) {
  return readings.slice(Math.max(0, readings.length - cells));
}

/// `layout::trend_spans` — one lane, `cells` wide: a blank head where the history is shorter
/// than the lane, the older cells dim, and the newest cell alone and lit. The lane is *not*
/// sized to the history it has, so a fresh page draws one lit cell and blanks and fills in at
/// one cell a second rather than reflowing the whole row for that long.
function trendSpans(levels, cells) {
  const drawn = levels.slice(Math.max(0, levels.length - cells));
  const out = [];
  if (drawn.length < cells) out.push(blank(cells - drawn.length, { dim: true }));
  if (drawn.length > 0) {
    const older = drawn.slice(0, -1);
    if (older.length > 0) {
      out.push(cell(older.map((l) => TREND_LADDER[Math.min(l, 7)]).join(""), { dim: true }));
    }
    out.push(cell(TREND_LADDER[Math.min(drawn[drawn.length - 1], 7)]));
  }
  return out;
}

/// `layout::trend_row` — the four-lane rung, or `null` when `cols` cannot hold four lanes at
/// their floor beside the **measured** text. The budget splits four ways with the odd cells
/// going to cpu then mem and the last one, when there is one, going undrawn, which is what
/// keeps every lane monotone in the pane width.
function trendRow(readings, load, cols) {
  const cpuValue = ` ${rightPad(String(Math.trunc(load.cpuPct)), 3)}%`;
  const memValue = ` ${rightPad(formatBytes(load.memUsed), 4)}/${formatBytes(load.memTotal)}`;
  const rxValue = ` ${rightPad(formatBytes(load.rxBps), 4)}/s`;
  const txValue = ` ${rightPad(formatBytes(load.txBps), 4)}/s`;
  const text = [
    LOAD_CPU_LABEL,
    cpuValue,
    " · ",
    LOAD_MEM_LABEL,
    memValue,
    " · ",
    LOAD_NET_LABEL,
    rxValue,
    " ↑",
    txValue,
  ].reduce((sum, t) => sum + textWidth(t), 0);
  const budget = cols - text;
  if (budget < 0 || budget < 4 * TREND_MIN_CELLS) return null;
  const share = Math.floor(budget / 4);
  const odd = budget % 4;
  const net = Math.min(share, TREND_MAX_CELLS);
  const cpu = Math.min(share + (odd >= 1 ? 1 : 0), TREND_MAX_CELLS);
  const mem = Math.min(share + (odd >= 2 ? 1 : 0), TREND_MAX_CELLS);
  const scale = netScale(readings, net);
  const levels = (cells, of) => trendTail(readings, cells).map(of);
  const rates = (cells, of) => trendTail(readings, cells).map((l) => rateLevel(of(l), scale));
  return [
    cell(LOAD_CPU_LABEL, { dim: true }),
    ...trendSpans(levels(cpu, (l) => pctLevel(l.cpuPct)), cpu),
    cell(cpuValue),
    cell(" · ", { dim: true }),
    cell(LOAD_MEM_LABEL, { dim: true }),
    ...trendSpans(levels(mem, (l) => pctLevel(memPct(l))), mem),
    cell(memValue),
    cell(" · ", { dim: true }),
    cell(LOAD_NET_LABEL, { dim: true }),
    ...trendSpans(rates(net, (l) => l.rxBps), net),
    cell(rxValue),
    cell(" ↑", { dim: true }),
    ...trendSpans(rates(net, (l) => l.txBps), net),
    cell(txValue),
  ];
}

/// `layout::shed_row` — a shed rung (1..=3): the same segments with no lanes at all, rung 2
/// dropping net and rung 3 mem too. With no lane to light, the **values** take normal weight
/// and the labels and connectors stay dim.
function shedRow(load, rung) {
  const segments = [[cell(LOAD_CPU_LABEL, { dim: true }), cell(`${rightPad(String(Math.trunc(load.cpuPct)), 3)}%`)]];
  if (rung < 3) {
    segments.push([
      cell(LOAD_MEM_LABEL, { dim: true }),
      cell(`${formatBytes(load.memUsed)}/${formatBytes(load.memTotal)}`),
    ]);
  }
  if (rung < 2) {
    segments.push([
      cell(LOAD_NET_LABEL, { dim: true }),
      cell(`${formatBytes(load.rxBps)}/s`),
      cell(" ↑", { dim: true }),
      cell(`${formatBytes(load.txBps)}/s`),
    ]);
  }
  const out = [];
  for (const segment of segments) {
    if (out.length > 0) out.push(cell(" · ", { dim: true }));
    out.push(...segment);
  }
  return out;
}

/// `layout::load_window` — the readings the lanes may draw as of `now`: the retained history
/// walked back from the newest and **cut at the first gap** wider than three heartbeats.
/// Gaps are cut, never bridged: a suspended laptop or a reattach leaves a hole nobody
/// sampled, and a lane that drew a slope across it would be inventing seconds.
function loadWindow(board, now) {
  const drawn = [];
  let newer = null;
  for (let i = board.load.history.length - 1; i >= 0; i -= 1) {
    const sample = board.load.history[i];
    if (sample.at > now) continue;
    if (newer !== null && newer - sample.at > STRIP_MAX_AGE) break;
    drawn.push(sample);
    newer = sample.at;
  }
  drawn.reverse();
  if (drawn.length === 0 && board.load.history.length > 0) {
    drawn.push(board.load.history[board.load.history.length - 1]);
  }
  return drawn;
}

/// `layout::plan_load_strip` — the strip at `cols`, or `null` when there is nothing honest to
/// draw: no sample yet, a newest reading older than three heartbeats, or a pane too narrow
/// for even the bare `CPU  38%`. All of them paint the identical blank row, because the band
/// reserves it either way and the board must not jump when the first sample lands.
function planLoadStrip(board, now, cols) {
  const readings = loadWindow(board, now);
  const load = readings[readings.length - 1];
  if (load === undefined || now - load.at > STRIP_MAX_AGE) return null;
  for (let rung = 0; rung <= 3; rung += 1) {
    const cells = rung === 0 ? trendRow(readings, load, cols) : shedRow(load, rung);
    if (cells === null) continue;
    if (rowWidth(cells) <= cols) return cells;
  }
  return null;
}

// --- the columns -------------------------------------------------------------------
//
// `crates/tui/src/shell.rs`'s width family: five columns, a shed ladder whose rungs are
// *derived* from the reserves rather than written down, and a stretch above the top rung
// where `Trigger` takes a capped quarter of the slack and `Service` the rest.

/// `layout::COLUMN_ORDER` and each column's Title-case label (ADR-0032). `Column::label` is
/// crate-internal in afkd, so — like `web/frames`'s `COLUMNS` — this is the spelling restated
/// here, in the order the five render.
const COLUMNS = [
  { key: "service", label: "Service" },
  { key: "state", label: "State" },
  { key: "trigger", label: "Trigger" },
  { key: "liveness", label: "Last Activity" },
  { key: "next", label: "Next Run" },
];

/// The fixed reserves, `shell::*_COL_WIDTH`. `service` has none: it is the content-fit column
/// below the stretch floor and the flex sink above it.
const COL_WIDTH = { state: 19, trigger: 16, liveness: 13, next: 8 };
/// `shell::COLUMN_SPACING` — the gutter between adjacent columns, spent by this table and by
/// the `Queues` section's own five alike.
const COLUMN_SPACING = 3;
/// `shell::SERVICE_COL_WANT` — the `Service` budget the narrow shed protects. Assumed, never
/// measured, so adding a long-named service can never drop a column.
const SERVICE_COL_WANT = 22;
/// `shell::SERVICE_COL_MIN` — the floor the column may shrink to: `"Service"` plus one cell.
const SERVICE_COL_MIN = 8;
/// `shell::SHED_ORDER` — static configuration before the live clocks. `Trigger` goes first
/// (widest reserve, and the cadence the `.conf` already states); `Last Activity` goes last.
const SHED_ORDER = ["trigger", "next", "liveness"];
/// `shell::TRIGGER_SLACK_SHARE` / `TRIGGER_STRETCH_MAX` — a quarter of the slack, capped at
/// double the named reserve, with `Service` taking the rest.
const TRIGGER_SLACK_SHARE = 4;
const TRIGGER_STRETCH_MAX = COL_WIDTH.trigger;

/// `shell::kept_columns` — the set left once the first `shed` of the ladder are dropped.
function keptColumns(shed) {
  const dropped = SHED_ORDER.slice(0, shed);
  return COLUMNS.filter((c) => !dropped.includes(c.key));
}

/// `shell::column_reserve` — the fixed widths of a set plus one gutter between each adjacent
/// pair. `Service` contributes nothing, so this is exactly what is *not* left for it.
function columnReserve(cols) {
  const fixed = cols.reduce((sum, c) => sum + (COL_WIDTH[c.key] ?? 0), 0);
  return fixed + COLUMN_SPACING * Math.max(0, cols.length - 1);
}

/// `shell::visible_columns` — the columns visible at `cols`, shedding whole columns while the
/// set would not leave `Service` its budget. The thresholds (90 / 71 / 60 today) are derived
/// from the reserves, never typed, so widening any column moves every rung with it.
function visibleColumns(cols) {
  let shed = 0;
  while (shed < SHED_ORDER.length && cols < columnReserve(keptColumns(shed)) + SERVICE_COL_WANT) {
    shed += 1;
  }
  return keptColumns(shed);
}

/// `shell::stretch_floor` — the rung at or above which the table stretches to fill the pane.
function stretchFloor() {
  return columnReserve(COLUMNS) + SERVICE_COL_WANT;
}

/// `shell::trigger_col_width` — the reserve plus its capped share of the slack.
function triggerColWidth(cols) {
  const slack = Math.max(0, cols - stretchFloor());
  return COL_WIDTH.trigger + Math.min(Math.floor(slack / TRIGGER_SLACK_SHARE), TRIGGER_STRETCH_MAX);
}

/// `shell::service_col_width` — content-fit below the stretch floor (clamped to the floor and
/// to what the visible set leaves), and the pure remainder above it, where `content` is
/// deliberately ignored so no column moves as a service is added, renamed or filtered.
function serviceColWidth(content, cols) {
  const avail = Math.max(0, cols - columnReserve(visibleColumns(cols)));
  if (cols < stretchFloor()) {
    return Math.min(Math.max(content, SERVICE_COL_MIN), Math.max(avail, SERVICE_COL_MIN));
  }
  return avail - (triggerColWidth(cols) - COL_WIDTH.trigger);
}

/// `shell::column_render_width` — the on-screen width of one column, so the header labels,
/// the body cells and their eliding budgets are three projections of one number.
function columnRenderWidth(key, content, cols) {
  if (key === "service") return serviceColWidth(content, cols);
  if (key === "trigger") return triggerColWidth(cols);
  return COL_WIDTH[key];
}

// --- the rows ----------------------------------------------------------------------
//
// `layout::service_row`, `layout::liveness_cell`, `layout::next_cell` and the tree
// vocabulary they hang off (ADR-0066/0068/0073).

/// `layout::TREE_CONNECTOR_MID` / `_LAST` / `TREE_GUIDE_BAR` / `_BLANK` /
/// `TREE_TOP_LEVEL_INDENT` — three display cells each, and the two-cell chevron lead-in a
/// top-level row spends so its icon lands in the group-icon column.
const TREE_CONNECTOR_MID = "├─ ";
const TREE_CONNECTOR_LAST = "└─ ";
const TREE_GUIDE_BAR = "│  ";
const TREE_GUIDE_BLANK = "   ";
const TREE_TOP_LEVEL_INDENT = "  ";
/// `layout::TREE_CHEVRON_EXPANDED` / `TREE_CHEVRON_COLLAPSED` — a squared minus and a squared
/// plus, not triangles: a collapsed group must not share a silhouette with the `▶` running
/// badge.
const TREE_CHEVRON_EXPANDED = "⊟";
const TREE_CHEVRON_COLLAPSED = "⊞";

/// `layout::DEFAULT_ICON` and its three twins — the 2×2 over (trouble, executing), plus the
/// stopped refinement of the clean-and-parked cell.
const DEFAULT_ICON = "🔹";
const WARN_ICON = "🔸";
const EXEC_ICON = "🔷";
const EXEC_WARN_ICON = "🔶";
const STOPPED_ICON = "▪️";
/// `layout::ORPHAN_MARKER` / `STALE_MARKER` — the tombstone and the cobweb (ADR-0028).
const ORPHAN_MARKER = "🪦";
const STALE_MARKER = "🕸️";
/// The **confinement** marker: a service the daemon runs under an `in_sandbox`/`in_worktree`
/// scope. This is the one row cell with no twin in `afkd top`, which spells confinement on
/// its info view instead (`infoview::sandbox_label` — `Sandbox scoped` / `Sandbox host`) and
/// reserves nothing for it on a list row. The card asks the page to carry it on the row, so
/// it is drawn with the run tree's own confinement glyph (`treeview::kind_glyph`'s
/// `NodeKind::Sandbox`) rather than a new one, in the reconcile markers' slot and on their
/// terms: nothing is reserved for it on an unconfined row, so only a confined row pays its
/// width. Recorded here rather than left to be discovered as drift.
const CONFINED_MARKER = "🔒";
/// `layout::group_icon` — 📜 for a `.conf` group, 📦 for a namespace. Every production group
/// name is a bare namespace since ADR-0072, so in practice every header renders 📦.
const GROUP_CONF_ICON = "📜";
const GROUP_NS_ICON = "📦";

/// `layout::ACTIVITY_FRESH_WINDOW` — how recent a service's last output must be for its
/// `Last Activity` cell to keep full weight.
const ACTIVITY_FRESH_WINDOW = 5 * 60 * 1000;
/// `layout::NEXT_SOON_WINDOW` — the FUTURE-side twin: how soon a parked service's next fire
/// must be for its countdown to keep full weight.
const NEXT_SOON_WINDOW = 10 * 60 * 1000;

/// `layout::card_icon` — the 2×2 over *trouble* (`faultedSinceArm || Crashed`) and
/// *executing*, with the stopped glyph as a guard on the clean-and-parked cell so the
/// precedence is structural: trouble outranks stopped, and executing outranks both. A custom
/// icon is returned verbatim; a card wearing the default flips.
function cardIcon(svc) {
  if (svc.icon !== "" && svc.icon !== DEFAULT_ICON) return svc.icon;
  const trouble = svc.faultedSinceArm || svc.badge === "Crashed";
  const executing = isExecuting(svc);
  if (trouble) return executing ? EXEC_WARN_ICON : WARN_ICON;
  if (executing) return EXEC_ICON;
  return svc.badge === "Stopped" ? STOPPED_ICON : DEFAULT_ICON;
}

/// `layout::reconcile_marker` — orphan wins the tie (the two are mutually exclusive), and an
/// unmarked row emits nothing at all.
function reconcileMarker(svc) {
  if (svc.orphan) return ORPHAN_MARKER;
  if (svc.stale) return STALE_MARKER;
  return "";
}

/// `layout::group_icon`.
function groupIcon(path) {
  return path.endsWith(".conf") ? GROUP_CONF_ICON : GROUP_NS_ICON;
}

/// `layout::leaf_segment` — everything after the last `::`, or the whole string. The one leaf
/// rule both display surfaces read: a nested header's spelled segment and a member row's own
/// name.
function leafSegment(name) {
  const at = name.lastIndexOf("::");
  return at === -1 ? name : name.slice(at + 2);
}

/// `layout::displayed_name` — a member sits under a header that already spells its namespace,
/// so it shows only its leaf; a root row keeps its whole qualified name.
function displayedName(svc, depth) {
  if (depth === 0 || svc.group === "") return svc.name;
  const prefix = `${svc.group}::`;
  return svc.name.startsWith(prefix) ? svc.name.slice(prefix.length) : svc.name;
}

/// `layout::liveness_cell` — the `Last Activity` age (ADR-0030), and the freshness bit that
/// dims it, minted together from one `elapsed` so the emphasis can never outlive the number.
/// A sub-second age reads `now`; a never-active row is blank, never a `0s`.
function livenessCell(lastActivityAt, now) {
  const elapsed = lastActivityAt === null ? null : Math.max(0, now - lastActivityAt);
  const ageText = (e) => (e < 1000 ? "now" : formatElapsed(e));
  // The tui reads two more inputs here — whether the service's run tree holds an **open
  // command leaf**, and, under that gate alone, the in-flight instant it started at — which
  // together force the cell fresh while a long buffered command is executing. The fold keeps
  // the tree, so the page could read it; it is left out because a leaf's openness is a
  // run-view fact and this card paints no run view. With the gate always shut the in-flight
  // anchor has no reader at all, so it is not taken. The consequence is bounded and stated: a
  // service whose command has been silent past the fresh window reads its climbing silence
  // age here where the terminal reads `now`.
  const text = elapsed === null ? "" : ageText(elapsed);
  const stale = text !== "" && elapsed > ACTIVITY_FRESH_WINDOW;
  return { text, stale };
}

/// `layout::next_cell` — the countdown to a **parked** service's next fire, populated only
/// for an `Idle` card carrying a deadline; every other case is blank, never a `—`. The `far`
/// bit dims a fire past the soon window through the same recede the age dim uses.
function nextCell(svc, now) {
  if (svc.badge !== "Idle" || svc.nextFireAt === null) return { text: "", far: false };
  const countdown = Math.max(0, svc.nextFireAt - now);
  return { text: formatElapsed(countdown), far: countdown > NEXT_SOON_WINDOW };
}

/// The `Service` cell's cells: the tree connector at full weight
/// (`shell::service_prefix_spans` — a stopped member's tint must not fade the group's guide),
/// then the icon, the markers and the name, with the name's `::` separators receded
/// (`shell::dim_separator_spans`).
function identityCells(prefix, body, name, tint) {
  const cells = [];
  if (prefix !== "") cells.push(cell(prefix, { fg: "recede" }));
  if (body !== "") cells.push(cell(body, { fg: tint }));
  let rest = name;
  while (rest !== "") {
    const at = rest.indexOf("::");
    if (at === -1) {
      cells.push(cell(rest, { fg: tint }));
      break;
    }
    if (at > 0) cells.push(cell(rest.slice(0, at), { fg: tint }));
    cells.push(cell("::", { fg: "recede" }));
    rest = rest.slice(at + 2);
  }
  return cells;
}

/// The `State` cell's text — `"{glyph} {label}"`, with the elapsed appended when the badge
/// carries one (`shell::badge_state_text`). The elapsed rides five badges, each off its own
/// clock: a live `▶ Busy` fire shows the fire's age, and the four transitional/pre-fire
/// states their dwell in that state, which is what keeps `Busy <elapsed>` meaning "has been
/// running for" (`layout::service_row`).
function stateText(svc, now) {
  const glyph = BADGE_GLYPH[svc.badge] ?? BADGE_GLYPH.Idle;
  let since = null;
  if (svc.badge === "Busy") since = svc.inFlightSince;
  else if (["Starting", "Queued", "Checking", "Stopping"].includes(svc.badge)) {
    since = svc.stateEnteredAt;
  }
  const elapsed = since === null ? "" : ` ${formatElapsed(Math.max(0, now - since))}`;
  return `${glyph} ${svc.badge}${elapsed}`;
}

/// `DashboardModel::visible_cards`'s name axis — the `/` needle, matched as a substring over
/// the **qualified** name so a member stays findable by the namespace its row no longer
/// spells. One rule, read by the row build and by the rollup fold alike.
function keptByFilter(name, filter) {
  return filter === "" || name.toLowerCase().includes(filter.toLowerCase());
}

/// `DashboardModel::group_rollup` — the **transitive** fold (ADR-0073): every service in the
/// subtree counts, so a parent header reflects a crash three levels down. Two aggregates and
/// no state ladder: ADR-0066 is explicit that a group row shows **no** state badge in the
/// resting case, and health is the one exception. The activity instant is the *maximum* —
/// "this group was last alive X ago". The Rust folds a third aggregate, the oldest in-flight
/// instant, which it spends only on the run view's open-leaf gate; with no run view here it
/// would have no reader, so it is not folded (see `livenessCell`).
function groupRollup(members, path) {
  let anyCrashed = false;
  let latestActivityAt = null;
  for (const svc of members) {
    if (!inSubtree(svc.group, path)) continue;
    anyCrashed = anyCrashed || svc.badge === "Crashed";
    if (svc.lastActivityAt !== null) {
      latestActivityAt =
        latestActivityAt === null ? svc.lastActivityAt : Math.max(latestActivityAt, svc.lastActivityAt);
    }
  }
  return { anyCrashed, latestActivityAt };
}

/// The visible row sequence — `DashboardModel::build_rows`'s **grouped** arm (ADR-0073): one
/// header per namespace *segment*, its children (sub-namespace headers and member services
/// alike) beneath it in first-appearance config order, and a bare un-namespaced service as a
/// root peer row taking its place in that same sequence. Every visible service appears
/// exactly once, contiguous under its group's header.
///
/// The **flat** arm (`v`) is not modelled: this page paints the grouped board and `v` is
/// listed-but-inert (`keymap.mjs`'s `NOTES`).
///
/// `collapsed` is the set of group paths whose descendants are folded away — the page's
/// inversion of `DashboardModel`'s `expanded` set, and the reason is D3: the terminal boots
/// flat, so its collapse default is never the first thing an operator sees, while this page
/// has no flat arm and an expanded-set default would boot to nothing but headers. A non-empty
/// `filter` force-expands the survivors for display without writing the set, which is
/// `visible_rows`'s own rule.
function buildRows(board, filter, collapsed) {
  const keep = (name) => keptByFilter(name, filter);
  // Insert: each card walks the `::` boundaries of its group, find-or-creating one node per
  // prefix path, and lands as a leaf in the deepest node. Find-or-create appends on first
  // sight, so children keep first-appearance order per level.
  const roots = [];
  const insert = (level, group, at, svc) => {
    if (at >= group.length) {
      level.push({ kind: "service", svc });
      return;
    }
    const sep = group.indexOf("::", at);
    const end = sep === -1 ? group.length : sep;
    const path = group.slice(0, end);
    let node = level.find((e) => e.kind === "group" && e.path === path);
    if (node === undefined) {
      node = { kind: "group", path, children: [] };
      level.push(node);
    }
    insert(node.children, group, end + 2, svc);
  };
  for (const name of board.order) {
    const svc = board.services[name];
    if (svc === undefined || !keep(name)) continue;
    insert(roots, svc.group, 0, svc);
  }
  // Flatten: a DFS pre-order walk stamping each row's depth, its tree prefix and, for a
  // header, the rollup its members fold to. A root row draws no connector, so its `last` is
  // moot — pinned true, the spelling a bare top-level row has always carried.
  const rows = [];
  const folded = (path) => filter === "" && collapsed.has(path);
  const walk = (level, depth, guides) => {
    level.forEach((entry, i) => {
      const last = depth === 0 || i + 1 === level.length;
      const prefix = depth === 0 ? "" : guides.join("") + (last ? TREE_CONNECTOR_LAST : TREE_CONNECTOR_MID);
      if (entry.kind === "service") {
        rows.push({ kind: "service", svc: entry.svc, depth, prefix });
      } else {
        rows.push({ kind: "group", path: entry.path, depth, prefix, collapsed: folded(entry.path) });
        if (folded(entry.path)) return;
        walk(entry.children, depth + 1, depth === 0 ? [] : guides.concat([last ? TREE_GUIDE_BLANK : TREE_GUIDE_BAR]));
      }
    });
  };
  walk(roots, 0, []);
  return rows;
}

/**
 * The whole visible row sequence at one filter and one fold set — the service and group rows,
 * then the `Queues` section's blank spacer, header and lanes. `layout()` builds the body from
 * exactly this, and the cursor indexes exactly this, so the row a key acts on is the row that
 * paints and there is no second sequence to drift.
 *
 * `options.filter` is the `/` needle (default none) and `options.collapsed` the folded group
 * paths (default none).
 */
export function visibleRows(board, options = {}) {
  const rows = buildRows(board, options.filter ?? "", options.collapsed ?? new Set());
  const lanes = queues(board);
  if (lanes.length > 0) {
    if (rows.length > 0) rows.push({ kind: "spacer" });
    rows.push({ kind: "queuesHeader" });
    for (const lane of lanes) rows.push({ kind: "queue", lane });
  }
  return rows;
}

/**
 * One row's identity — `model::row_key`: a group by its full path, a service by its qualified
 * name, a lane by its canonical name, and `null` for the section's spacer and header, which
 * are the two rows the cursor may not sit on. The cursor holds one of these rather than an
 * index, so a fold that adds or drops a service cannot slide it onto a neighbour.
 */
export function rowKey(row) {
  if (row === undefined) return null;
  if (row.kind === "group") return { kind: "group", key: row.path };
  if (row.kind === "service") return { kind: "service", key: row.svc.name };
  // A lane’s identity is its canonical name, which `queues()` spells `lane`.
  if (row.kind === "queue") return { kind: "queue", key: row.lane.lane };
  return null;
}

/// The identity parts of one list row — the tree prefix, the icon-and-markers body and the
/// displayed name — composed the way `layout::service_row` composes its cell, so the string
/// the `Service` column is measured against is exactly the one that renders.
function identityOf(row) {
  if (row.kind === "group") {
    // `layout::group_row_label`: the collapse chevron leads the icon and the **leaf** segment
    // — a header's ancestors are the indent it hangs off. A header's name carries no health
    // tint of its own: a crashed member surfaces through the `State` cell's own badge.
    return {
      prefix: row.prefix,
      body: `${row.collapsed ? TREE_CHEVRON_COLLAPSED : TREE_CHEVRON_EXPANDED} ${groupIcon(row.path)} `,
      name: leafSegment(row.path),
      tint: "ink",
    };
  }
  const svc = row.svc;
  return {
    prefix: row.depth === 0 ? TREE_TOP_LEVEL_INDENT : row.prefix,
    body: `${cardIcon(svc)} ${reconcileMarker(svc)}${svc.confined ? CONFINED_MARKER : ""}`,
    name: displayedName(svc, row.depth),
    // `shell::row_line_style`: a **stopped** row recedes whole-line so a shelf of switched-off
    // services sinks below the live ones; under the cursor it lifts back to full weight.
    tint: svc.badge === "Stopped" ? "muted" : "ink",
  };
}

/// The composed `Service`-cell text of one row — `shell::service_cell_content_width`'s
/// measured string. The `Queues` section's rows measure **zero**: the section is its own
/// table, laid from column 0, and a lane name has no business sizing the column every service
/// row shares.
function identityText(row) {
  if (row.kind !== "service" && row.kind !== "group") return "";
  const { prefix, body, name } = identityOf(row);
  return `${prefix}${body}${name}`;
}

// --- the `Queues` section ----------------------------------------------------------
//
// `layout::queue_cols` / `queue_header_line` / `queue_lane_line`. The section deliberately
// does **not** ride the list's grid: a lane is not a service, so its five columns are laid
// from column 0 on their own ladder, with the same `COLUMN_SPACING` gutter between them.

/// `layout::QUEUE_LANE_GLYPH` and its one space — the *icon, one space, name* shape every
/// identity cell above it spends.
const QUEUE_LANE_GLYPH = "🧵";
const QUEUE_LANE_PREFIX_W = 3;
/// `layout::QUEUE_SLOT_HELD` / `_FREE` / `_WAITING` — a held slot, a free one, and a
/// lane-mate standing outside the door, deliberately a round dot a size class below the bars.
const QUEUE_SLOT_HELD = "▮";
const QUEUE_SLOT_FREE = "▯";
const QUEUE_SLOT_WAITING = "∙";
/// `layout::QUEUE_FENCE` / `_LEAD` — the door between a lane's bar and the fires standing
/// outside it; a bar-less field *opens* on it and spends one cell less.
const QUEUE_FENCE = " ╎ ";
const QUEUE_FENCE_W = 3;
const QUEUE_FENCE_LEAD = "╎ ";
const QUEUE_FENCE_LEAD_W = 2;
/// `layout::QUEUE_BAR_MAX` / `QUEUE_WAIT_MAX` / `QUEUE_WAIT_CELLS` — the cells the field's
/// floor pays for, never a bound on what is drawn.
const QUEUE_BAR_MAX = 8;
const QUEUE_WAIT_CELLS = 4 + 1;
/// `layout::QUEUE_SLOTS_MIN` — folded from the three, so the width the field never goes below
/// is the width the three of them cost.
const QUEUE_SLOTS_MIN = QUEUE_BAR_MAX + QUEUE_FENCE_W + QUEUE_WAIT_CELLS;
/// `layout::QUEUE_NAME_COL_MIN` — the `"Queue"` label plus one cell of breathing room.
const QUEUE_NAME_COL_MIN = 6;
/// `layout::QUEUE_COLUMN_ORDER` and its Title-case labels.
const QUEUE_COLUMNS = ["Queue", "Slots", "Parallelism", "Held", "Waiting"];

/// `layout::queue_floor_cols` — what the section costs past its name column with `Slots` at
/// its own floor. Folded from the order rather than typed, so a sixth column cannot leave a
/// stale constant behind.
function queueFloorCols() {
  const widths = [0, QUEUE_SLOTS_MIN, ...QUEUE_COLUMNS.slice(2).map(textWidth)];
  return widths.reduce((a, b) => a + b, 0) + COLUMN_SPACING * (QUEUE_COLUMNS.length - 1);
}

/// `layout::queue_cols` — the section's two elastic widths: `Queue` is the content fit over
/// the section's **own** lane identities, `Slots` is the sink that takes everything the other
/// four and their gutters leave. The five and their four gutters sum to `cols` exactly.
function queueCols(cols, widestLane) {
  const cap = Math.max(0, cols - queueFloorCols());
  const want = QUEUE_LANE_PREFIX_W + widestLane;
  const name = Math.min(Math.max(want, QUEUE_NAME_COL_MIN), Math.max(cap, QUEUE_NAME_COL_MIN));
  const rest = queueFloorCols() - QUEUE_SLOTS_MIN;
  const slots = Math.max(cols - (name + rest), QUEUE_SLOTS_MIN);
  return { name, slots };
}

/// The width of one section column at the folded elastic pair.
function queueColumnWidth(label, cols) {
  if (label === "Queue") return cols.name;
  if (label === "Slots") return cols.slots;
  return textWidth(label);
}

/// `layout::queue_overflow_mark` — the "and `N` more behind them" mark, its leading space
/// included so it never abuts the last glyph it stands for.
function queueOverflowMark(hidden) {
  return ` +${hidden}`;
}

/// `layout::queue_run` — draw `n` things into `budget` cells: all of them when they fit, else
/// as many as fit beside a mark naming the rest. Scans down from `budget` because `N`'s digit
/// count changes with `k`. A budget too small for even one glyph beside its mark draws
/// nothing, never a mark alone.
function queueRun(n, budget) {
  if (n <= budget) return [n, null];
  for (let k = budget; k >= 1; k -= 1) {
    if (k + queueOverflowMark(n - k).length <= budget) return [k, n - k];
  }
  return [0, null];
}

/// `layout::queue_bar_cells` — one cell per **holder**, and one per free slot the declared
/// width still has, the wider of the two. A lane still draining a narrow holds more than it
/// is wide and grows past its width rather than contradicting the `Held` cell beside it.
function queueBarCells(parallelism, held, budget) {
  const capacity = parallelism === null ? 0 : Math.max(0, Math.trunc(parallelism));
  return queueRun(Math.max(capacity, held), budget);
}

/// The lane's identity text — `🧵 <lane> (<priority>)`, with no parenthetical when the lane
/// reports none. The spelling the terminal's info view already uses for a lane
/// (`infoview`'s `Queue` row); the tui's own `Queues` section has no room for it, this page's
/// column is fit to whatever it costs, and the card asks for the priority on the row.
function laneIdentity(lane) {
  return lane.priority === "" ? lane.lane : `${lane.lane} (${lane.priority})`;
}

/// One lane row's five cells at the section's elastic widths — `layout::queue_lane_line`,
/// hued as `shell::queue_lane_spans` hues it: the bar's held cells take the `Busy` badge's
/// accent, its free cells and the door recede, and the pips take `Queued`'s own tier, because
/// a fire outside the door is exactly a queued one.
function queueLaneCells(cols, lane) {
  const identity = truncateWidth(laneIdentity(lane), Math.max(0, cols.name - QUEUE_LANE_PREFIX_W));
  const barBudget = Math.max(0, cols.slots - (QUEUE_FENCE_W + QUEUE_WAIT_CELLS));
  const [cells, barHidden] = queueBarCells(lane.parallelism, lane.held, barBudget);
  const filled = Math.min(lane.held, cells);
  const field = [];
  if (filled > 0) field.push(cell(QUEUE_SLOT_HELD.repeat(filled), BADGE_STYLE.Busy));
  if (cells - filled > 0) field.push(cell(QUEUE_SLOT_FREE.repeat(cells - filled), { fg: "recede" }));
  let barW = cells;
  if (barHidden !== null) {
    const mark = queueOverflowMark(barHidden);
    barW += mark.length;
    field.push(cell(mark, { fg: "recede" }));
  }
  // The bar's share of the field is a **reservation**, computed without reading `waiting`, so
  // a fire queueing or being admitted can never move a slot cell. The pips then take the true
  // remainder. A bar-less lane opens its field on the door and spends one cell less on it.
  const door = cells === 0 ? QUEUE_FENCE_LEAD : QUEUE_FENCE;
  const pipBudget =
    cells === 0
      ? Math.max(0, cols.slots - QUEUE_FENCE_LEAD_W)
      : Math.max(0, cols.slots - (barW + QUEUE_FENCE_W));
  const [dots, pipHidden] = queueRun(lane.waiting, pipBudget);
  if (dots > 0 || pipHidden !== null) {
    field.push(cell(door, { fg: "recede" }));
    let pips = QUEUE_SLOT_WAITING.repeat(dots);
    if (pipHidden !== null) pips += queueOverflowMark(pipHidden);
    field.push(cell(pips, BADGE_STYLE.Queued));
  }
  const width = lane.parallelism === null ? "?" : String(lane.parallelism);
  return [
    [cell(`${QUEUE_LANE_GLYPH} `), ...identityCells("", "", identity, "ink")],
    field,
    [cell(width)],
    [cell(String(lane.held))],
    [cell(String(lane.waiting))],
  ];
}

/// Lay five already-styled column cell groups over the section's own table: each padded to
/// its column's width by **display** width and joined with the shared gutter, so a column
/// origin the header paints is by construction the one the lanes paint.
function queueCompose(cols, groupsOfCells) {
  const out = [];
  QUEUE_COLUMNS.forEach((label, i) => {
    if (i > 0) out.push(blank(COLUMN_SPACING));
    const group = groupsOfCells[i];
    out.push(...group);
    out.push(blank(queueColumnWidth(label, cols) - rowWidth(group)));
  });
  return out;
}

// --- the footer legend -------------------------------------------------------------
//
// `crates/tui/src/legend.rs` (the hint model, the wrap and the column grid),
// `crates/tui/src/layout.rs`'s `footer` (which cells each selection composes) and
// `crates/tui/src/keys.rs` (where every advertised glyph comes from).
//
// The chords themselves live in `keymap.mjs` — the whole 51-row transcription of afkd's
// `DEFAULT_KEYS`, pinned against the Rust by its own suite — because the input side reads the
// same table. What this file spends it on is the rule the card names: an action with no chord
// renders no hint at all, because a footer must never advertise an unpressable key.

/// `legend::HELP_LABEL` — matched by the hide rung to find the one key it keeps, so the match
/// cannot drift from what the builders emit.
const HELP_LABEL = "help";
/// `model::POISON_RECOVERY` — the note a force-abandoned card's footer carries instead of two
/// unexplained dim cells.
const POISON_RECOVERY = "force-abandoned; its thread has not finished yet";

/// The `Keys` display view (`keys::Keys`): the chord table plus this frame's drain bit.
function keysOf(quitting) {
  const chords = (action) => (quitting && REFUSED_WHILE_QUITTING.includes(action) ? [] : glyphs(action));
  const primary = (action) => chords(action)[0] ?? null;
  return {
    primary,
    /// `Keys::hint` — an unbound action yields nothing, so the caller omits the cell.
    hint(action, label) {
      const key = primary(action);
      return key === null ? null : { kind: "key", keys: key, label, live: true };
    },
    /// `Keys::hint_gated` — the two "not now" cases are deliberately different: an unbound
    /// action still yields nothing (there is no glyph to print), while a bound key this
    /// selection cannot take yields a **dim** cell that holds its slot, so the footer's shape
    /// stops tracking the badge.
    hintGated(action, label, live) {
      const key = primary(action);
      return key === null ? null : { kind: "key", keys: key, label, live };
    },
    /// `Keys::hint_all` — `Keys::all` in a key cell: every glyph bound to one action,
    /// `/`-joined (`i/Esc`), for a cell that deliberately teaches both spellings. The info
    /// footer's `back` is its one caller, because the two keys that close the page are equally
    /// canonical and advertising one would leave the other undiscoverable.
    ///
    /// Deliberately **no** shared-`Ctrl+` elision: `hint_pair` does that for two *different*
    /// actions sharing a cell, and `all` does not. Read through the same drain-aware `chords`
    /// every builder here reads, so a refused action yields nothing rather than a whole cell.
    hintAll(action, label) {
      const bound = chords(action);
      return bound.length === 0 ? null : { kind: "key", keys: bound.join("/"), label, live: true };
    },
    /// `Keys::hint_pair` — two actions in one cell as `a/b`, with a shared `Ctrl+` elided on
    /// the second glyph. Nothing when **either** is unbound: half a pair would advertise an
    /// unpressable key.
    hintPair(first, second, label) {
      const a = primary(first);
      const b = primary(second);
      if (a === null || b === null) return null;
      const tail = a.startsWith("Ctrl+") && b.startsWith("Ctrl+") ? b.slice(5) : b;
      return { kind: "key", keys: `${a}/${tail}`, label, live: true };
    },
  };
}

/// A non-key legend row aligned in the key column — the poison ✕ note.
function note(glyph, label) {
  return { kind: "note", glyph, label };
}
/// Free prose, **glued** to its neighbour: it carries its own connector spacing inside the
/// string and is never split from the hint beside it.
function bare(text) {
  return { kind: "bare", text };
}
/// A zero-width category seam. It carries nothing — the grouping is positional — and paints
/// nothing: the grid splits on it, the flat wrap strips it first.
const BREAK = { kind: "break" };

/// `legend::hint_width` — a `Key`/`Note` renders `field label` with one joining space; a dim
/// cell measures like any other, which is what holds its slot still.
function hintWidth(hint) {
  if (hint.kind === "key") return textWidth(hint.keys) + 1 + textWidth(hint.label);
  if (hint.kind === "note") return textWidth(hint.glyph) + 1 + textWidth(hint.label);
  if (hint.kind === "bare") return textWidth(hint.text);
  return 0;
}

/// `legend::glued` / `gap` — a `Bare` (and a `Break`) takes no gap on either side; two
/// key cells take the load-bearing two-space separator.
const COLUMN_GAP = 2;
function glued(hint) {
  return hint.kind === "bare" || hint.kind === "break";
}
function gap(prev, next) {
  return glued(prev) || glued(next) ? 0 : COLUMN_GAP;
}

/// `legend::GRID_ROWS` — the ceiling: a terminal too narrow for this rung hides the hotkeys
/// rather than reflowing to a fourth row.
const GRID_ROWS = 3;
/// `legend::CATEGORY_GUTTER` — wide enough to read as a break without a rule or a label.
const CATEGORY_GUTTER = 4;

/// `legend::decompose` — split a categorised footer into the parts that are never grid cells
/// (the trailing affordance, which becomes the header row with its ` · ` shed, and the notes)
/// and the cells that are. `null` for a footer carrying no seam.
function decompose(hints) {
  if (!hints.some((h) => h.kind === "break")) return null;
  const notes = hints.filter((h) => h.kind === "note");
  const cells = hints.filter((h) => h.kind !== "note");
  let header = null;
  const last = cells[cells.length - 1];
  if (last !== undefined && last.kind === "bare") {
    header = last.text.startsWith(" · ") ? last.text.slice(3) : last.text;
    cells.pop();
  }
  return { header, notes, cells };
}

/// `legend::categories_of` — the cells split on their seams; a category with no cells at all
/// is simply a column that is not there.
function categoriesOf(cells) {
  const out = [];
  let current = [];
  for (const hint of cells) {
    if (hint.kind === "break") {
      if (current.length > 0) out.push(current);
      current = [];
    } else current.push(hint);
  }
  if (current.length > 0) out.push(current);
  return out;
}

/// `legend::grid_shape` — one rung laid at an explicit row count: the painted rows with their
/// padding baked in as whitespace `Bare`s (glued, so no gap is injected), and the block's
/// total rendered width. A category past `rows` continues in a sibling column at the ordinary
/// gap rather than at the category gutter.
function gridShape(categories, rows) {
  const columns = [];
  categories.forEach((category) => {
    for (let at = 0, chunk = 0; at < category.length; at += rows, chunk += 1) {
      const gutter = columns.length === 0 ? 0 : chunk === 0 ? CATEGORY_GUTTER : COLUMN_GAP;
      columns.push({ gutter, cells: category.slice(at, at + rows) });
    }
  });
  const widths = columns.map((c) => c.cells.reduce((w, h) => Math.max(w, hintWidth(h)), 0));
  const total = columns.reduce((sum, c, i) => sum + c.gutter + widths[i], 0);
  const out = [];
  for (let r = 0; r < rows; r += 1) {
    const row = [];
    let pad = 0;
    columns.forEach((column, i) => {
      pad += column.gutter;
      const hint = column.cells[r];
      if (hint === undefined) {
        pad += widths[i];
        return;
      }
      if (pad > 0) row.push(bare(" ".repeat(pad)));
      pad = widths[i] - hintWidth(hint);
      row.push(hint);
    });
    out.push(row);
  }
  return { rows: out, total };
}

/// `legend::grid_rows` — the column grid: each category runs *down* a column, the columns sit
/// side by side, so the grouping is spatial and costs no label. The shape is a **ladder**: the
/// fewest rows whose measured block fits `cols`, so within a width band nothing moves at all.
/// `null` when the footer carries no seam, has no cells, or overflows even the tallest rung.
function gridRows(hints, cols) {
  const parts = decompose(hints);
  if (parts === null) return null;
  const categories = categoriesOf(parts.cells);
  if (categories.length === 0) return null;
  const natural = Math.min(categories.reduce((n, c) => Math.max(n, c.length), 0), GRID_ROWS);
  for (let rows = 1; rows <= natural; rows += 1) {
    const shape = gridShape(categories, rows);
    if (shape.total <= cols) {
      const out = parts.header === null ? [] : [[bare(parts.header)]];
      out.push(...shape.rows);
      out.push(...parts.notes.map((n) => [n]));
      return out;
    }
  }
  return null;
}

/// `legend::shed_dim` — the rung between the full grid and the hide: a terminal too narrow
/// for the whole block sheds the cells that **cannot be pressed** before it hides the ones
/// that can.
function shedDim(hints) {
  return hints.filter((h) => !(h.kind === "key" && !h.live));
}

/// `legend::hidden_rows` — the hotkeys hidden: the header row, the notes, then the help cell
/// alone, so the way *into* the hotkeys is never lost.
function hiddenRows(hints) {
  const parts = decompose(hints);
  if (parts === null) return null;
  const out = parts.header === null ? [] : [[bare(parts.header)]];
  out.push(...parts.notes.map((n) => [n]));
  const help = parts.cells.find((h) => h.kind === "key" && h.label === HELP_LABEL);
  if (help !== undefined) out.push([help]);
  return out;
}

/// `shell::shed_hidden` — the hide shed to `cap` rows. The paint order is header · notes ·
/// help, but the ranking under a squeeze is different: the notes go first (bottom-up), then
/// the help row. The way in to the hotkeys outranks the explanation of why a key is missing.
function shedHidden(rows, cap) {
  const out = rows.slice();
  while (out.length > cap) {
    const noteAt = out.map((r) => r[0]?.kind === "note").lastIndexOf(true);
    if (noteAt !== -1) out.splice(noteAt, 1);
    else out.pop();
  }
  return out;
}

/// `legend::wrap_hints` — the flat fallback for a footer carrying no seam: rows of at most
/// `cols` cells, breaking only between two-space-gap hints so a glued run is never split.
function wrapHints(hints, cols) {
  const flat = hints.filter((h) => h.kind !== "break");
  if (flat.length === 0) return [[]];
  const width = flat.reduce((w, h, i) => w + (i > 0 ? gap(flat[i - 1], h) : 0) + hintWidth(h), 0);
  if (width <= cols) return [flat];
  const atoms = [];
  flat.forEach((hint, i) => {
    if (i > 0 && gap(flat[i - 1], hint) === 0) atoms[atoms.length - 1].push(hint);
    else atoms.push([hint]);
  });
  const rows = [];
  let current = [];
  let currentW = 0;
  for (const atom of atoms) {
    const w = atom.reduce((sum, h) => sum + hintWidth(h), 0);
    if (current.length === 0) {
      current = atom.slice();
      currentW = w;
    } else if (currentW + 2 + w > cols) {
      rows.push(current);
      current = atom.slice();
      currentW = w;
    } else {
      current.push(...atom);
      currentW += 2 + w;
    }
  }
  if (current.length > 0) rows.push(current);
  return rows.length === 0 ? [flat] : rows;
}

/// `shell::footer_rows` — the ladder the footer takes, and the single source of its geometry:
/// the grid wherever a rung fits, else the grid of the **shed** footer, else the hide, else
/// the flat wrap — each capped so the body keeps its floor under the pinned title band.
function footerRows(hints, cols, rows) {
  const cap = Math.max(1, rows - 2 - 3);
  const grid = gridRows(hints, cols);
  if (grid !== null && grid.length <= cap) return grid;
  const shed = gridRows(shedDim(hints), cols);
  if (shed !== null && shed.length <= cap) return shed;
  const hidden = hiddenRows(hints);
  if (hidden !== null) return shedHidden(hidden, cap);
  return wrapHints(hints, cols).slice(0, cap);
}

/// One footer row's cells: the key glyph whitened **structurally** (it is the `keys` field,
/// never a bracket-scanned run), its label in the calm legend tone, and a gated cell's `dim`
/// bit carried onto both.
function hintRowCells(row) {
  const out = [];
  row.forEach((hint, i) => {
    if (i > 0 && gap(row[i - 1], hint) === 2) out.push(blank(COLUMN_GAP));
    if (hint.kind === "key") {
      out.push(cell(hint.keys, { fg: "bright", dim: !hint.live }));
      out.push(blank(1));
      out.push(cell(hint.label, { fg: "legend", dim: !hint.live }));
    } else if (hint.kind === "note") {
      out.push(cell(hint.glyph, { fg: "alarm" }));
      out.push(blank(1));
      out.push(cell(hint.label, { fg: "legend" }));
    } else if (hint.kind === "bare") {
      out.push(cell(hint.text, { fg: "legend" }));
    }
  });
  return out;
}

/// `layout::control_hints` / `stop_cell` — the `s`/`x` pair. Both are always emitted and the
/// gate picks their *weight*, so the footer's shape is a constant of the arm rather than of
/// the badge; only an **unbound** action renders nothing at all. The `x` cell is a single cell
/// for a single gesture: its label flips to `force-stop` exactly when pressing `x` would open
/// the force-confirm modal.
function controlHints(keys, badge, poisoned) {
  const out = [];
  const start = keys.hintGated("overview.service_start", "start", canStart(badge) && !poisoned);
  if (start !== null) out.push(start);
  const stop =
    badge === "Stopping"
      ? keys.hintGated("overview.service_stop", "force-stop", true)
      : keys.hintGated("overview.service_stop", "stop", canStop(badge));
  if (stop !== null) out.push(stop);
  return out;
}

/// `layout::group_control_hints` — the group-level reading of the same gating: the same four
/// cells in the same `s x t r` order, each live when **at least one member** can take it. The
/// lane pair is the exception that does not fan out — a header is not a card — so it is
/// always dim here, emitted rather than dropped because the footer's shape must not track the
/// selection.
function groupControlHints(keys, members) {
  const out = [];
  const some = (f) => members.some(f);
  const push = (hint) => {
    if (hint !== null) out.push(hint);
  };
  push(keys.hintGated("overview.service_start", "start", some((m) => canStart(m.badge) && !m.poisoned)));
  push(
    some((m) => m.badge === "Stopping")
      ? keys.hintGated("overview.service_stop", "force-stop", true)
      : keys.hintGated("overview.service_stop", "stop", some((m) => canStop(m.badge))),
  );
  push(keys.hintGated("overview.service_fire", "trigger", some((m) => canFire(m.badge))));
  push(
    keys.hintGated(
      "overview.service_restart",
      "restart",
      some((m) => !m.poisoned && m.badge !== "Stopping"),
    ),
  );
  push(keys.hintGated("overview.queue_widen", "widen", false));
  push(keys.hintGated("overview.queue_narrow", "narrow", false));
  return out;
}

/// `layout::footer` — the selection-aware legend, resolved highest-precedence-first: the
/// filter prompt while typing (this page has no typing state yet, so a set `filter` is always
/// the confirmed `Active` form), then the contextual hints for the selected row and nothing
/// else. It names neither the selection nor its state — the row's own cells carry both, a
/// line or two above under the selection bar.
///
/// The seams (`BREAK`) cut the same way the terminal cuts them — act on this · inspect it ·
/// move around · the daemon — so the grid runs each category down its own column.
function planFooter(board, rows, selected, filter, quitting, typing, flash, info) {
  const keys = keysOf(quitting);
  // Precedence 1 — while typing, the prompt owns the footer. Its leading glyph is the
  // `filter` key itself, so a prose-embedded key still tracks the table.
  if (typing !== null) {
    return [bare(`${keys.primary("overview.filter") ?? ""} filter: ${typing}_`)];
  }
  // Precedence 2 — a live flash replaces the contextual footer. Placed **before** the
  // contextual build so expiry reverts to the byte-for-byte same line.
  if (flash !== null) return [bare(flash)];
  // Precedence 3 — the info page owns its key surface while it is open, so it owns the line
  // that teaches it: `infoview::footer`'s three hints and nothing else. It sits *below* the
  // flash for the same reason the list's contextual footer does — a daemon refusal must not be
  // buried by a legend — and above the list's arms because there is no selection to describe.
  if (info === true) {
    return [
      keys.hintAll("info.back", "back"),
      keys.hint("global.help", HELP_LABEL),
      keys.hint("global.quit", "quit"),
    ].filter((hint) => hint !== null);
  }
  const push = (into, hint) => {
    if (hint !== null) into.push(hint);
  };
  // The `Active` filter's compact clear affordance, so a confirmed filter never makes the
  // contextual footer unreachable. Both glyphs are keymap-sourced like every other key; an
  // unbound clear drops its whole parenthetical rather than printing an empty `()`.
  let affordance = null;
  if (filter !== "") {
    const slash = keys.primary("overview.filter") ?? "";
    const clear = keys.primary("overview.filter_clear");
    affordance = bare(` · ${slash}${filter}${clear === null ? "" : ` (${clear.toLowerCase()})`}`);
  }
  // The busy lens's state rides its own key cell — the `f follow ●/○` convention: the glyph
  // sits in the *label* so only the key whitens. Nothing toggles it on this page yet.
  const busy = keys.hint("overview.filter_busy", "busy ○");
  const globals = [];
  push(globals, keys.hint("global.reload", "reload"));
  push(globals, keys.hint("global.help", HELP_LABEL));
  push(globals, keys.hint("global.quit", "quit"));

  const row = rows[selected];
  const hints = [];
  if (row !== undefined && row.kind === "service") {
    const svc = row.svc;
    hints.push(...controlHints(keys, svc.badge, svc.poisoned));
    // A poisoned `Crashed` card has neither a live `s` nor a live `r`; rather than leave the
    // operator guessing at two muted slots, an inert note states why and what lifts the gate.
    if (svc.poisoned) hints.push(note("✕", POISON_RECOVERY));
    push(hints, keys.hintGated("overview.service_fire", "trigger", canFire(svc.badge)));
    push(
      hints,
      keys.hintGated("overview.service_restart", "restart", !svc.poisoned && svc.badge !== "Stopping"),
    );
    // `+`/`-` move the selected service's **lane**, so they are gated on the selection naming
    // one rather than on its badge: a lane-less service renders both dim.
    const onALane = svc.queue !== "";
    push(hints, keys.hintGated("overview.queue_widen", "widen", onALane));
    push(hints, keys.hintGated("overview.queue_narrow", "narrow", onALane));
    hints.push(BREAK);
    push(hints, keys.hint("overview.show_output", "output"));
    push(hints, keys.hint("overview.show_info", "info"));
    push(hints, keys.hint("overview.service_peek", "peek"));
    hints.push(BREAK);
    push(hints, keys.hintPair("overview.first", "overview.last", "first/last"));
    // The label names the view the key would switch *to*, so it reads as a destination.
    push(hints, keys.hint("overview.view", "flat"));
    push(hints, busy);
    push(hints, keys.hint("overview.filter", "find"));
  } else if (row !== undefined && row.kind === "group") {
    const members = board.order
      .map((name) => board.services[name])
      .filter((svc) => svc !== undefined && inSubtree(svc.group, row.path));
    hints.push(...groupControlHints(keys, members));
    hints.push(BREAK);
    // The collapse hint names both `Enter` and the directional key, and the verb it names is
    // the one the key would *do* next — `expand` over a folded header, `collapse` over an
    // open one, the `Tab tree`/`Tab log` destination convention.
    push(
      hints,
      row.collapsed
        ? keys.hintPair("overview.service_peek", "overview.group_expand", "expand")
        : keys.hintPair("overview.service_peek", "overview.group_collapse", "collapse"),
    );
    push(hints, keys.hintPair("overview.group_collapse_all", "overview.group_expand_all", "fold all"));
    push(hints, keys.hint("overview.view", "flat"));
    push(hints, keys.hintPair("overview.first", "overview.last", "first/last"));
    push(hints, busy);
    push(hints, keys.hint("overview.filter", "find"));
  } else if (row !== undefined && row.kind === "queue") {
    push(hints, keys.hint("queues.narrow", "narrow"));
    push(hints, keys.hint("queues.widen", "widen"));
    hints.push(BREAK);
    push(hints, keys.hintPair("overview.first", "overview.last", "first/last"));
    push(hints, keys.hint("overview.view", "flat"));
    push(hints, busy);
    push(hints, keys.hint("overview.filter", "find"));
  } else {
    // No selection: start/stop/restart need one, so the actions category is absent.
    push(hints, keys.hint("overview.show_output", "output"));
    push(hints, keys.hint("overview.show_info", "info"));
    hints.push(BREAK);
    push(hints, keys.hintPair("overview.first", "overview.last", "first/last"));
    push(hints, busy);
    push(hints, keys.hint("overview.filter", "find"));
  }
  hints.push(BREAK);
  hints.push(...globals);
  if (affordance !== null) hints.push(affordance);
  return hints;
}

/// `model::in_subtree` — a group path covers itself and every descendant path, on `::`
/// boundaries so `ops` never swallows `opsworks`.
function inSubtree(group, path) {
  return group === path || group.startsWith(`${path}::`);
}

// --- the body's height and its scroll ------------------------------------------------

/// `shell::LIST_TITLE_ROWS` plus the column header — the four rows pinned above the body.
const LIST_CHROME_ROWS = 4;

/**
 * How many body rows a `cols × rows` viewport leaves for `board` — the chrome above plus the
 * footer the legend ladder settles on, subtracted from the height.
 *
 * It plans the footer a second time per repaint, deliberately: the alternative is a
 * return-shape change to `layout()` that would churn the painter, every golden and three
 * suites for an allocation at 4 Hz. The shell needs the number *before* it lays out, because
 * the scroll offset is an input to the very layout that would otherwise report it.
 */
export function bodyHeight(board, options) {
  const cols = Math.max(1, Math.trunc(options.cols));
  const rows = Math.max(1, Math.trunc(options.rows));
  const listRows = visibleRows(board, options);
  const footer = footerRows(
    planFooter(
      board,
      listRows,
      options.selected ?? 0,
      options.filter ?? "",
      board.quittingSince !== null,
      options.typing ?? null,
      options.flash ?? null,
      (options.info ?? null) !== null,
    ),
    cols,
    rows,
  );
  return Math.max(0, rows - LIST_CHROME_ROWS - footer.length);
}

/**
 * The body's scroll top after the cursor moved to `selected` — ratatui's **minimal** clamp,
 * which is what `TableState` gives the terminal through `get_row_bounds` (named at
 * `shell.rs:1418`): scroll only far enough to bring the selection back into view, then stay
 * put. Deliberately **not** `peek_floor_offset`, the downward bias that keeps a selected
 * service's expanded peek tail off the viewport floor — this page has no peek surface, so
 * transcribing that would scroll for a row that does not exist.
 *
 * Clamped into `0..=count-height` at the end, so a shrunk board or a grown viewport never
 * leaves blank rows above real ones.
 */
export function scrollOffset(prev, selected, count, height) {
  if (height <= 0) return 0;
  let offset = Math.max(0, Math.trunc(prev));
  if (selected < offset) offset = selected;
  else if (selected >= offset + height) offset = selected - height + 1;
  return Math.min(Math.max(0, offset), Math.max(0, count - height));
}

// --- the popups -----------------------------------------------------------------------
//
// `shell::render_popup` and the two surfaces it frames: the verb-carrying confirm modal
// (ADR-0026) and the `?` overlay. One frame, so no call site pads its own body — the bug
// `docs/tui-style.md` §8's gutter rule was written for.

/// §8's gutter: two blank columns inside each vertical border, one blank row inside each
/// horizontal one. `render_popup`'s `Padding::new(2, 2, 1, 1)`.
const POPUP_PAD_X = 2;
const POPUP_PAD_Y = 1;
/// The box-drawing frame, in `layout::BORDER`'s own vocabulary.
const POPUP_BORDER = { tl: "┌", tr: "┐", bl: "└", br: "┘", h: "─", v: "│" };

/// `render_popup`'s `Padding` indent for a dialog's **listed** item (`shell::DIALOG_ITEM_INDENT`):
/// a target name and the `… and N more` tail sit one step in from the flush prose; a note, the
/// force consequence lines and the hint legend all stay at the gutter.
const DIALOG_ITEM_INDENT = "  ";

/// `cells` from cell `start` to cell `end`. A grapheme straddling either edge is replaced by
/// blanks of the width it covered — a terminal's own answer when a popup lands mid-glyph, and
/// the reason the rows this composes still sum to exactly `cols`.
function sliceCells(cells, start, end) {
  const out = [];
  let at = 0;
  for (const c of cells) {
    const from = at;
    const to = at + c.width;
    at = to;
    if (to <= start || from >= end) continue;
    if (from >= start && to <= end) {
      out.push(c);
      continue;
    }
    // Partially covered: keep the whole clusters inside the window, blank the straddlers.
    let cursor = from;
    let text = "";
    let lead = 0;
    let tail = 0;
    for (const { segment } of GRAPHEMES.segment(c.text)) {
      const w = clusterWidth(segment);
      if (cursor >= start && cursor + w <= end) text += segment;
      else if (cursor < start) lead += Math.max(0, Math.min(cursor + w, end) - Math.max(cursor, start));
      else tail += Math.max(0, Math.min(cursor + w, end) - Math.max(cursor, start));
      cursor += w;
    }
    if (lead > 0) out.push(blank(lead, { fg: c.fg, bg: c.bg, dim: c.dim }));
    if (text !== "") out.push({ ...c, text, width: textWidth(text) });
    if (tail > 0) out.push(blank(tail, { fg: c.fg, bg: c.bg, dim: c.dim }));
  }
  return out;
}

/// `BACKDROP_RECEDE` — the whole frame behind a popup dims. ratatui *unions* `DIM` into the
/// modifier, which over a bold column header would leave a cell both bold and dim; this page's
/// golden planes spell one weight per cell and `assertGrid` refuses that pair, so a receded
/// cell takes `dim` alone (D6).
function recede(cells) {
  return cells.map((c) => ({ ...c, dim: true, bold: false }));
}

/**
 * Lay `popup` — `{title, lines}`, each line an array of cells — centered over `frame`, with
 * everything behind it receded. Returns the new screen; `frame` is not written through.
 *
 * The box is sized from the content, so the gutter is this function's and never a call site's:
 * `content + 2 gutter each side + 2 borders` wide, `lines + 1 blank row each side + 2 borders`
 * tall, clamped to the viewport.
 */
function renderPopup(frame, popup, cols, rows) {
  const lines = popup.lines;
  const content = lines.reduce((w, line) => Math.max(w, rowWidth(line)), textWidth(popup.title));
  const width = Math.min(cols, content + 2 * POPUP_PAD_X + 2);
  const height = Math.min(rows, lines.length + 2 * POPUP_PAD_Y + 2);
  const left = Math.max(0, Math.floor((cols - width) / 2));
  const top = Math.max(0, Math.floor((rows - height) / 2));
  const inner = width - 2;
  // The title rides the top border as ` Title `, bright, the weight `render_popup` gives it.
  const titleText = clipWidth(` ${popup.title} `, Math.max(0, inner));
  const titleRow = [
    cell(POPUP_BORDER.tl, { fg: "recede" }),
    cell(titleText, { fg: "bright" }),
    cell(POPUP_BORDER.h.repeat(Math.max(0, inner - textWidth(titleText))), { fg: "recede" }),
    cell(POPUP_BORDER.tr, { fg: "recede" }),
  ];
  const bottomRow = [
    cell(POPUP_BORDER.bl, { fg: "recede" }),
    cell(POPUP_BORDER.h.repeat(Math.max(0, inner)), { fg: "recede" }),
    cell(POPUP_BORDER.br, { fg: "recede" }),
  ];
  /// One framed body row: border, gutter, the line clipped to the content width, gutter, border.
  const bodyRow = (line) => {
    const budget = Math.max(0, inner - 2 * POPUP_PAD_X);
    const kept = sliceCells(line, 0, budget);
    return [
      cell(POPUP_BORDER.v, { fg: "recede" }),
      blank(POPUP_PAD_X),
      ...kept,
      blank(budget - rowWidth(kept)),
      blank(POPUP_PAD_X),
      cell(POPUP_BORDER.v, { fg: "recede" }),
    ];
  };
  const box = [titleRow];
  for (let i = 0; i < POPUP_PAD_Y; i += 1) box.push(bodyRow([]));
  for (const line of lines) box.push(bodyRow(line));
  for (let i = 0; i < POPUP_PAD_Y; i += 1) box.push(bodyRow([]));
  box.push(bottomRow);

  return frame.map((row, i) => {
    const receded = recede(row);
    const at = i - top;
    if (at < 0 || at >= height || box[at] === undefined) return receded;
    return fitRow(
      [
        ...sliceCells(receded, 0, left),
        ...sliceCells(box[at], 0, width),
        ...sliceCells(receded, left + width, cols),
      ],
      cols,
    );
  });
}

/// A popup body line of plain prose, in the calm legend tone the dialogs paint.
function dialogLine(text) {
  return text === "" ? [] : [cell(text, { fg: "legend" })];
}

/// `shell::dialog_item_line` — one listed item, indented inside the gutter. The name is plain
/// by construction: a name may never dress as a key.
function dialogItem(text) {
  return [cell(`${DIALOG_ITEM_INDENT}${text}`, { fg: "ink" })];
}

/// `shell::confirm_hints` rendered as one line — `y <verb>  n cancel`, both glyphs from the
/// `confirm` scope so the modal cannot advertise a key the dispatch would ignore, and
/// unbracketed like every other hint on the page.
function confirmHints(keys, acceptLabel) {
  const out = [];
  const accept = keys.hint("confirm.accept", acceptLabel);
  const cancel = keys.hint("confirm.cancel", "cancel");
  if (accept !== null) out.push(accept);
  if (cancel !== null) out.push(cancel);
  return hintRowCells(wrapHints(out, Number.MAX_SAFE_INTEGER)[0]);
}

/// The title word and the hint's verb for each confirmable verb — the Sentence-case one for
/// the title (§8), the lower-case one for the key legend.
const CONFIRM_WORDS = {
  force: { title: "Force-stop", accept: "force" },
  fire: { title: "Fire", accept: "fire" },
  start: { title: "Start", accept: "start" },
  stop: { title: "Stop", accept: "stop" },
  restart: { title: "Restart", accept: "restart" },
};

/// `shell::fanout_confirm_modal`'s `NAME_CAP` — the cap lives in the content, never in the
/// clipping, so the modal says how many it did not list.
const CONFIRM_NAME_CAP = 10;

/// The confirm modal (ADR-0026): the force-stop gate and the four operator fan-out gates,
/// transcribed from `shell::render_overlays`' `ConfirmVerb` arms. The force arm asks in its
/// first body line, where its `stuck` qualifier fits; the fan-out arms ask in the title, which
/// is their only prompt — the accepted divergence `docs/tui-style.md` §8 records.
function confirmModal(board, confirm, quitting) {
  const keys = keysOf(quitting);
  const words = CONFIRM_WORDS[confirm.verb] ?? CONFIRM_WORDS.force;
  const count = confirm.targets.length;
  const noun = count === 1 ? "service" : "services";
  const icon = (name) => {
    const svc = board.services[name];
    return svc === undefined ? name : `${cardIcon(svc)} ${name}`;
  };
  if (confirm.verb === "force") {
    // Two honesty lines before the `y`, because the "do not oversell it" bar wants both the
    // reach and the limitation stated: what force ends, and what it leaves behind.
    return {
      title: words.title,
      lines: [
        dialogLine(`Force-stop ${count} stuck ${noun}?`),
        dialogLine(""),
        ...confirm.targets.map((name) => dialogItem(icon(name))),
        dialogLine(""),
        dialogLine("Force kills the running work & abandons the wedged thread"),
        dialogLine("The service reclaims once the abandoned thread finishes"),
        dialogLine(""),
        confirmHints(keys, words.accept),
      ],
    };
  }
  const lines = confirm.targets.slice(0, CONFIRM_NAME_CAP).map((name) => dialogItem(icon(name)));
  if (count > CONFIRM_NAME_CAP) lines.push(dialogItem(`… and ${count - CONFIRM_NAME_CAP} more`));
  if ((confirm.skipped ?? 0) > 0) {
    // A bare note, so the shorter list does not read as a bug.
    lines.push(dialogLine(""));
    lines.push(dialogLine(`${confirm.skipped} skipped`));
  }
  lines.push(dialogLine(""));
  lines.push(confirmHints(keys, words.accept));
  return { title: `${words.title} ${count} ${noun}?`, lines };
}

// --- the `?` overlay --------------------------------------------------------------------

/// The two page-level facts the overlay states before it lists anything: that this is the
/// stock keymap and a rebind is not mirrored, and that `Ctrl+R` is afkd's reload and is taken
/// from the browser rather than left to it.
const OVERLAY_PREAMBLE = [
  "These are afkd's stock keys — a rebound `keys { … }` block is not mirrored here.",
  "Ctrl+R is afkd's reload: the page takes it, so the browser does not reload the tab.",
];

/// The gutter between two overlay columns — `legend::CATEGORY_GUTTER`, the same seam the
/// footer's grid reads as a break without a rule or a label.
const OVERLAY_GUTTER = CATEGORY_GUTTER;

/// The overlay's entries in reading order: one heading per scope, then that scope's rows, an
/// unhandled one carrying the note saying why this page does not take it (D2 — the rule is
/// still "no binding ⇒ no row"; every one of these *has* a binding).
///
/// A scope whose rows share **one** reason carries it on the heading instead, once: the four
/// output/info scopes are unreachable here wholesale, and twenty repetitions of one sentence
/// would set the column's width for nothing.
function overlayEntries() {
  const out = [];
  for (const scope of SCOPES) {
    const rows = DEFAULT_KEYS.filter((row) => row.scope === scope);
    if (rows.length === 0) continue;
    const notes = new Set(rows.map((row) => NOTES[idOf(row)] ?? null));
    const shared = notes.size === 1 && !notes.has(null) ? [...notes][0] : null;
    if (out.length > 0) out.push({ kind: "blank" });
    out.push({ kind: "heading", text: scope, note: shared });
    for (const row of rows) {
      const id = idOf(row);
      out.push({
        kind: "row",
        keys: all(id) ?? "",
        label: DESCRIPTIONS[id],
        note: shared === null ? (NOTES[id] ?? null) : null,
        live: HANDLED.has(id),
      });
    }
  }
  return out;
}

/// One entry's cells at a column width — a heading bold with its shared note beside it, a live
/// row's glyph bright and its label calm, an unhandled row's whole cell dim with its own note
/// trailing at the same weight.
function overlayCells(entry, keyWidth) {
  if (entry.kind === "blank") return [];
  if (entry.kind === "heading") {
    const out = [cell(entry.text, { fg: "bright", bold: true })];
    // The note is **not** bold: it explains the scope, it does not title it. Dim rather than
    // plain, because everything it covers is dim.
    if (entry.note !== null) out.push(cell(` — ${entry.note}`, { fg: "legend", dim: true }));
    return out;
  }
  const out = [
    cell(padWidth(entry.keys, keyWidth), { fg: "bright", dim: !entry.live }),
    blank(1),
    cell(entry.label, { fg: "legend", dim: !entry.live }),
  ];
  if (entry.note !== null) out.push(cell(` — ${entry.note}`, { fg: "legend", dim: true }));
  return out;
}

/// The overlay's body laid into columns: the entries flow down a column and on into the next,
/// never orphaning a scope heading at a column's foot. `height` rows a column, as many columns
/// as the entries need; the caller picks the height that fits.
function overlayColumns(entries, height) {
  const columns = [];
  let current = [];
  for (let i = 0; i < entries.length; i += 1) {
    const entry = entries[i];
    // A heading with no room for at least one of its rows moves down with them.
    const orphan = entry.kind === "heading" && current.length + 1 >= height;
    if (current.length >= height || orphan) {
      columns.push(current);
      current = [];
    }
    // A column never opens on a blank: the seam between two scopes is the column edge itself.
    if (entry.kind === "blank" && current.length === 0) continue;
    current.push(entry);
  }
  if (current.length > 0) columns.push(current);
  return columns;
}

/// The `?` overlay: every bound action in every scope, grouped by scope, in table order. It
/// lists all 51 rows at any viewport that can hold them and, at one that cannot, says how many
/// it could not show rather than clipping into silence.
function helpOverlay(cols, rows) {
  const entries = overlayEntries();
  const keyWidth = entries.reduce((w, e) => (e.kind === "row" ? Math.max(w, textWidth(e.keys)) : w), 0);
  const budgetW = Math.max(1, cols - 2 * POPUP_PAD_X - 2);
  const budgetH = Math.max(1, rows - 2 * POPUP_PAD_Y - 2 - OVERLAY_PREAMBLE.length - 1);
  /// One candidate shape at a column height: the painted rows, and how many **rows** of the
  /// table had to be shed because their column did not fit `budget`.
  const shapeAt = (height, budget) => {
    const columns = overlayColumns(entries, height).map((column) => ({
      entries: column,
      cells: column.map((entry) => overlayCells(entry, keyWidth)),
    }));
    const widths = columns.map((c) => c.cells.reduce((w, line) => Math.max(w, rowWidth(line)), 0));
    let kept = 0;
    let spent = 0;
    for (let i = 0; i < columns.length; i += 1) {
      const next = spent + widths[i] + (i > 0 ? OVERLAY_GUTTER : 0);
      // The first column always lands, even where it overruns: an overlay with nothing in it
      // would be worse than one clipped at the edge.
      if (next > budget && i > 0) break;
      spent = next;
      kept += 1;
    }
    const dropped = columns
      .slice(kept)
      .reduce((n, c) => n + c.entries.filter((e) => e.kind === "row").length, 0);
    const shown = columns.slice(0, kept);
    const out = [];
    for (let r = 0; r < height; r += 1) {
      const line = [];
      shown.forEach((column, i) => {
        if (i > 0) line.push(blank(OVERLAY_GUTTER));
        const cells = column.cells[r] ?? [];
        line.push(...cells, blank(widths[i] - rowWidth(cells)));
      });
      out.push(sliceCells(line, 0, budget));
    }
    // A shape whose tail columns are empty (the last rung often is) paints no blank rows.
    while (out.length > 0 && rowWidth(out[out.length - 1]) === 0) out.pop();
    return { rows: out, dropped };
  };
  // The fewest rows whose block fits the width — the footer grid's own ladder, walked over a
  // taller range because eight scopes of prose never fit three rows.
  let shape = null;
  for (let height = 1; height <= budgetH; height += 1) {
    const candidate = shapeAt(height, budgetW);
    if (candidate.dropped === 0) {
      shape = candidate;
      break;
    }
  }
  // Past the tallest rung the block still overruns the width: take the tallest shape and shed
  // whole columns off the right, which loses the fewest entries and keeps the ones it shows
  // readable.
  if (shape === null) shape = shapeAt(budgetH, budgetW);
  const lines = [
    ...OVERLAY_PREAMBLE.map((text) => dialogLine(text)),
    [],
    ...shape.rows,
  ];
  // What a too-narrow viewport could not lay out, stated as **content**: the overlay must
  // never quietly claim to be the whole keymap when a column of it is off the right edge.
  if (shape.dropped > 0) {
    lines.push([]);
    lines.push(dialogLine(`… and ${shape.dropped} more keys — widen the viewport`));
  }
  return { title: "Help", lines };
}

// --- the info surface --------------------------------------------------------------------
//
// `crates/tui/src/infoview.rs` — the pure row model (the five sections, every value phrase,
// `pretty_key`, the conditional `Queue`/`Config`/`Recovery` rows) — and `shell::render_info`,
// which is the geometry. The two **disagree**, and this surface follows the card and
// `docs/tui-style.md` §1 rather than `render_info`, on four points. Naming them is the point
// of citing both:
//
// - **Borderless, page-wide labels.** §1 says bold section header, a blank spacer before every
//   section but the first in its column, and every value aligned to one *page-wide* label
//   column. `render_info` draws bordered panels and aligns per panel (`0655bf03` replaced §1's
//   layout one day after it was written and never updated the doc).
// - **Greedy packing, not a fixed assignment.** §1 packs sections onto the shorter column;
//   `info_section_column` pins Trigger/Usage right.
// - **Wrapping, never ellipsis.** `section_body_lines` ellipsizes a value inside its panel.
//   Here a long value wraps and the page scrolls, which is what makes "no value clipped at
//   either width" true at all. `trim_url_tail` is deliberately **not** transcribed for the same
//   reason: §1 says a structured trigger value is echoed verbatim from the config.
// - **Elapsed-only phrases.** The relay carries no time-of-day anchor and no last-fire record
//   — `ServiceState`'s four anchors are all *durations*, so `afkd @<ip:port> top` never imports
//   the daemon's clock skew — so `Started`, `Last run` and every `HH:MM` the terminal projects
//   have nothing to project from. The `Activity` section spends its rows on what the wire does
//   carry, and says so rather than printing a clock it guessed.

/// `shell::INFO_TWO_COL_MIN_WIDTH` — at or above this the page opens a second column, below it
/// the sections stack in one. Read off the terminal so the two surfaces break at one width.
const INFO_TWO_COL_MIN = 100;
/// `shell::INFO_COL_GUTTER` — the seam between the grid's two columns. No perimeter inset: the
/// terminal's 1-col margin exists to keep panel borders off the edge, and there are no borders.
const INFO_COL_GUTTER = 2;
/// `section_body_lines`' label→value gutter, the three spaces a padded label is followed by.
const INFO_LABEL_GUTTER = 3;
/// `shell::TITLE_ROWS` — the title line plus its spacer, pinned above the band and the body.
const INFO_TITLE_ROWS = 2;
/// `infoview::ABOUT_HEADER` — the full-width band above the grid. **Uncapped** here:
/// `ABOUT_MAX_ROWS` protects a height budget a scrolling surface does not have.
const ABOUT_HEADER = "About";
/// `infoview::sandbox_label` — the scope a service applies, seeded from config and never probed.
const SANDBOX_SCOPED = "scoped";
const SANDBOX_HOST = "host";
/// The ADR-0028 reconcile verdicts, on one `Config` row. The orphan sentence is
/// `infoview.rs`'s verbatim; the stale one is written to the same `tag - actionable sentence`
/// shape from the same vocabulary the 🕸️ legend note uses ("restart to adopt"), because
/// `infoview.rs` carries no stale row at all and `docs/tui-style.md` §7 says this row is where
/// both words live. The two are mutually exclusive on the wire, and `reconcileMarker` above
/// already encodes orphan winning the tie, so this reads the verdict the same way round.
const CONFIG_ORPHAN = "orphan - removed from config; restart to stop, or re-add it";
const CONFIG_STALE = "stale - new config staged; restart to adopt it";

/// `infoview::group_label` (ADR-0066) — an un-namespaced service is a top-level row, not a
/// bucket header, so an empty group reads as the word rather than as a blank.
function groupLabel(svc) {
  return svc.group === "" ? "top-level" : svc.group;
}

/// `infoview::state_phrase`, minus its two wall-clock arms: `Stopped`/`Crashed` read
/// `<Word> for <elapsed>` like the transitional badges rather than `since <HH:MM>`, because no
/// time-of-day anchor crosses this wire. The elapsed is the **same** anchor the list `State`
/// cell reads for this badge (`stateText`), so the two surfaces cannot drift.
function statePhrase(svc, now) {
  const since = svc.badge === "Busy" && svc.inFlightSince !== null ? svc.inFlightSince : svc.stateEnteredAt;
  const elapsed = formatElapsed(Math.max(0, now - since));
  if (svc.badge === "Busy") return `Busy ${elapsed} (running)`;
  // An empty parenthetical says less than none: a husk card the snapshot never seeded with
  // config metadata is `Queued` on a lane it cannot name.
  if (svc.badge === "Queued" && svc.queue !== "") {
    return `Queued for ${elapsed} (waiting on ${svc.queue})`;
  }
  return `${svc.badge} for ${elapsed}`;
}

/// `infoview::next_run_phrase` minus its `HH:MM` prefix: the countdown to a **parked**
/// service's next fire, gated exactly as the list's `nextCell` is (an `Idle` card carrying a
/// deadline), every other case a calm `none`. One formatter with the list cell, so the info
/// countdown and the `Next Run` column cannot spell one instant two ways.
function nextRunPhrase(svc, now) {
  const next = nextCell(svc, now);
  return next.text === "" ? "none" : `in ${next.text}`;
}

/// `infoview::overview_rows`' conditional `Queue` value: the daemon's resolved lane name, the
/// level it waits at as a parenthetical, and the lane's **live** parallelism on a ` · ` suffix.
/// A level the daemon did not send drops the parenthetical rather than printing an empty one; a
/// parallelism it did not send drops the suffix. A width of `0` is **not** that case — it is a
/// width the lane holds, so it reads `· parallelism 0` (ADR-0079 §2h).
function queueValue(svc) {
  let lane = svc.queuePriority === "" ? svc.queue : `${svc.queue} (${svc.queuePriority})`;
  if (svc.queueParallelism !== null) lane += ` · parallelism ${svc.queueParallelism}`;
  return lane;
}

/// `infoview::trigger_kind` — the bare kind off the flat detail's leading segment
/// (`"trello · board …"` → `"trello"`), which the app always builds as `<kind>` + ` · <key>
/// <value>` pairs, so the two projections name one kind by construction.
function triggerKind(svc) {
  return svc.triggerDetail.split(" · ")[0] ?? svc.triggerDetail;
}

/// `infoview::pretty_key` — a raw config key as a field label: `_`→space and the first
/// character upper-cased (`pick_from`→`Pick from`). Only the first: an already-lowercase
/// remainder is a word, not an acronym, so `poll_interval`→`Poll interval`.
function prettyKey(key) {
  const spaced = key.replace(/_/g, " ");
  return spaced === "" ? spaced : spaced[0].toUpperCase() + spaced.slice(1);
}

/// `infoview::trigger_rows` — the kind on its own `Kind` row, then one row per structured
/// config key, each label prettified and each **value echoed verbatim** (the lowercase-data
/// exemption `docs/tui-style.md` §1 names). A card carrying no `trigger_fields` — a kind
/// outside the expansion set, or an older daemon — degrades to a lone `Kind` row holding the
/// flat `trigger_detail` line. That fallback is in the wire contract, not an invention.
function triggerRows(svc) {
  if (svc.triggerFields.length === 0) return [field("Kind", svc.triggerDetail)];
  return [
    field("Kind", triggerKind(svc)),
    ...svc.triggerFields.map(([key, value]) => field(prettyKey(key), value)),
  ];
}

/// `infoview::avg_run_phrase` — the mean wall time of a **successful** run over the daemon's
/// whole run history. Successful only: a failed run's wall time still counts toward
/// `Total run time` and toward `Failures`, but not toward how long a run usually takes, so a
/// service whose every run failed reads `n/a` rather than a misleading `0s`.
function avgRunPhrase(runs, failures, okRunTimeTotalMs) {
  const ok = Math.max(0, runs - failures);
  return ok === 0 ? "n/a" : formatElapsed(okRunTimeTotalMs / ok);
}

/// `infoview::stats_phrase` — the ok/fail success rate as `<pct>% ok`, a cheap fold of the two
/// counters. No run on record reads `n/a`; counters that arrive off the wire impossible
/// (`failures > runs`) read `0% ok` rather than a negative percentage, the clamp the Rust's
/// `saturating_sub` is there for.
function statsPhrase(runs, failures) {
  if (runs === 0) return "n/a";
  return `${Math.floor((Math.max(0, runs - failures) * 100) / runs)}% ok`;
}

/// One `Field` row. `badge` names the card's badge on the lone `State` row, which is the page's
/// **one** coloured accent — every other value is neutral, exactly as `render_info` has it.
function field(label, value, badge = null) {
  return { kind: "field", label, value, badge };
}
/// One `Pair` row — two short scalars two-up on one line (`infoview::Row::Pair`).
function pair(la, va, lb, vb) {
  return { kind: "pair", cells: [{ label: la, value: va }, { label: lb, value: vb }] };
}

/// `infoview::info_view`'s five sections, in `docs/tui-style.md` §1's order and vocabulary.
function infoSections(svc, now) {
  const overview = [field("State", statePhrase(svc, now), svc.badge), field("Group", groupLabel(svc))];
  // Only when the service joined a lane — a lane-less service is the common case and a `none`
  // row would be noise, the same "empty means absent" key the `About` band folds on.
  if (svc.queue !== "") overview.push(field("Queue", queueValue(svc)));
  overview.push(field("Next run", nextRunPhrase(svc, now)));

  // The total folds the live tail of an in-flight fire on top of the completed-fire total, so
  // a service mid-run reads an honest running total rather than the `0s` its history holds.
  const inFlight = svc.inFlightSince === null ? 0 : Math.max(0, now - svc.inFlightSince);
  const activity = livenessCell(svc.lastActivityAt, now);
  const health = [
    field("Failures", `${svc.failures}`),
    field("Diagnostics (swallowed)", `${svc.errors}`),
    field("Last error", svc.lastError ?? "none"),
    field("Sandbox", svc.confined ? SANDBOX_SCOPED : SANDBOX_HOST),
  ];
  // A healthy service has no `Config` row at all, so the row's presence *is* the verdict.
  if (svc.orphan) health.push(field("Config", CONFIG_ORPHAN));
  else if (svc.stale) health.push(field("Config", CONFIG_STALE));
  // Not named by the card, but the fold tracks `poisoned` and the footer already carries
  // `POISON_RECOVERY`: a `Health` section that omitted a fault signal the board holds would be
  // wrong rather than minimal. Independent of the reconcile branch — the two are orthogonal.
  if (svc.poisoned) health.push(field("Recovery", POISON_RECOVERY));

  return [
    { header: "Overview", rows: overview },
    { header: "Trigger", rows: triggerRows(svc) },
    {
      header: "Activity",
      rows: [
        // `Started` and `Last run` are **absent**, not forgotten: both are wall-clock rows the
        // terminal projects from `started_tod`, and no such anchor crosses this wire. The value
        // here is the one the list's `Last Activity` cell shows, through the same seam.
        field("Last activity", activity.text === "" ? "none" : activity.text),
        field("Avg run time", avgRunPhrase(svc.runs, svc.failures, svc.okRunTimeTotalMs)),
        field("Total run time", formatElapsed(svc.runTimeTotalMs + inFlight)),
      ],
    },
    {
      header: "Usage",
      rows: [
        pair("Runs", `${svc.runs}`, "Tokens", formatTokens(svc.tokens)),
        pair("Cost", `$${svc.cost.toFixed(2)}`, "Stats", statsPhrase(svc.runs, svc.failures)),
      ],
    },
    { header: "Health", rows: health },
  ];
}

/// `text` split at `budget` cells on a **grapheme** boundary — the prefix that fits and the
/// rest — so a hard break never lands inside a cluster or between a base and its mark.
function splitWidth(text, budget) {
  const head = clipWidth(text, budget);
  return [head, text.slice(head.length)];
}

/**
 * `text` wrapped into lines of at most `budget` cells: greedy word wrap, then a grapheme-safe
 * **hard break** for a single token still wider than the column (a board URL, a long lane
 * name). Nothing is ever elided — which is the whole reason this surface wraps at all, and
 * what makes "no value clipped at either width" a property rather than a hope.
 *
 * Width is **display** width, so a CJK or emoji value breaks where it paints.
 */
function wrapValue(text, budget) {
  const width = Math.max(2, budget);
  const words = text.split(/\s+/).filter((w) => w !== "");
  if (words.length === 0) return [""];
  const wrapped = [];
  let current = "";
  for (const word of words) {
    const addition = textWidth(current) + 1 + textWidth(word);
    if (current === "" || addition > width) {
      if (current !== "") wrapped.push(current);
      current = word;
      continue;
    }
    current = `${current} ${word}`;
  }
  wrapped.push(current);
  const out = [];
  for (let line of wrapped) {
    while (textWidth(line) > width) {
      const [head, rest] = splitWidth(line, width);
      out.push(head);
      line = rest;
    }
    out.push(line);
  }
  return out;
}

/// One `Field` row's lines at a column width: the dim label padded to the **page-wide** label
/// column, the gutter, then the value wrapped into what is left. A continuation line indents to
/// the value's own origin, so a wrapped value reads as one block rather than as new fields.
/// The `State` row's badge glyph rides in its own styled cell ahead of the value and eats two
/// cells of the budget, exactly as `section_body_lines` spends them.
function infoFieldLines(row, labelW, width) {
  const style = row.badge === null ? { fg: "ink" } : (BADGE_STYLE[row.badge] ?? BADGE_STYLE.Idle);
  const glyphW = row.badge === null ? 0 : 2;
  const lead = labelW + INFO_LABEL_GUTTER;
  const budget = width - lead - glyphW;
  return wrapValue(row.value, budget).map((text, i) => {
    if (i > 0) return [blank(lead + glyphW), cell(text, style)];
    const cells = [cell(padWidth(row.label, labelW) + " ".repeat(INFO_LABEL_GUTTER), { fg: "legend", dim: true })];
    if (row.badge !== null) cells.push(cell(`${BADGE_GLYPH[row.badge] ?? BADGE_GLYPH.Idle} `, style));
    cells.push(cell(text, style));
    return cells;
  });
}

/// `section_body_lines`' two-up arithmetic for a `Pair` row: `la va`, a gap to the aligned
/// second column, then `lb vb`. Both labels dim and both values neutral — the `State` accent is
/// a `Field`, never a paired scalar.
function infoPairLine(row, pairLabelW, pairFirstW) {
  const [a, b] = row.cells;
  const used = pairLabelW + 1 + textWidth(a.value);
  const gap = Math.max(0, pairFirstW - used) + INFO_LABEL_GUTTER;
  return [
    cell(`${padWidth(a.label, pairLabelW)} `, { fg: "legend", dim: true }),
    cell(a.value),
    blank(gap),
    cell(`${padWidth(b.label, pairLabelW)} `, { fg: "legend", dim: true }),
    cell(b.value),
  ];
}

/// The label widths the whole page aligns to — **page-wide**, across every section, which is
/// §1's bolded ask and the one thing that makes two columns read as one page. The `Pair` widths
/// are computed the same way for the same reason.
function infoLabelWidths(sections) {
  let labelW = 0;
  let pairLabelW = 0;
  let pairFirstW = 0;
  for (const section of sections) {
    for (const row of section.rows) {
      if (row.kind === "field") labelW = Math.max(labelW, textWidth(row.label));
      else for (const c of row.cells) pairLabelW = Math.max(pairLabelW, textWidth(c.label));
    }
  }
  for (const section of sections) {
    for (const row of section.rows) {
      if (row.kind === "pair") {
        pairFirstW = Math.max(pairFirstW, pairLabelW + 1 + textWidth(row.cells[0].value));
      }
    }
  }
  return { labelW, pairLabelW, pairFirstW };
}

/// One section rendered to lines at `width`: the bold header, then its rows. A `Pair` whose
/// composed width overruns the column **degrades to two `Field` rows** rather than clipping, so
/// the no-clipping rule has no exception at any width.
function infoSectionLines(section, width, widths) {
  const lines = [[cell(section.header, { fg: "bright", bold: true })]];
  for (const row of section.rows) {
    if (row.kind === "pair") {
      const line = infoPairLine(row, widths.pairLabelW, widths.pairFirstW);
      if (rowWidth(line) <= width) {
        lines.push(line);
        continue;
      }
      for (const c of row.cells) lines.push(...infoFieldLines(field(c.label, c.value), widths.labelW, width));
      continue;
    }
    lines.push(...infoFieldLines(row, widths.labelW, width));
  }
  return lines;
}

/**
 * The page's body: the five sections laid into one or two columns and zipped side by side.
 *
 * Two columns at `INFO_TWO_COL_MIN` cells and wider, one below it. Sections are packed
 * **greedily onto the shorter column** in display order, ties to the left, and a blank spacer
 * sits before every section but the first *in its column* — §1's rule, not `render_info`'s
 * fixed Trigger/Usage-right assignment. The left column is fitted to its own width before the
 * gutter, so a value that wrapped cannot smear into the right one.
 */
function infoBody(sections, cols) {
  const widths = infoLabelWidths(sections);
  if (cols < INFO_TWO_COL_MIN) {
    const out = [];
    for (const section of sections) {
      if (out.length > 0) out.push([]);
      out.push(...infoSectionLines(section, cols, widths));
    }
    return out;
  }
  const left = Math.floor((cols - INFO_COL_GUTTER) / 2);
  const right = cols - INFO_COL_GUTTER - left;
  const columns = [[], []];
  for (const section of sections) {
    const at = columns[1].length < columns[0].length ? 1 : 0;
    const lines = infoSectionLines(section, at === 0 ? left : right, widths);
    if (columns[at].length > 0) columns[at].push([]);
    columns[at].push(...lines);
  }
  const height = Math.max(columns[0].length, columns[1].length);
  const out = [];
  for (let i = 0; i < height; i += 1) {
    const tail = columns[1][i] ?? [];
    out.push([...fitRow(columns[0][i] ?? [], left), blank(INFO_COL_GUTTER), ...tail]);
  }
  return out;
}

/// The full-width `About` band: its bold header, the service's own sentence wrapped whole, and
/// the spacer below it. A service naming no `description` gets **no rows at all** — the band
/// costs zero, rather than drawing an empty box (`infoview`'s `about: None`).
function infoBandRows(about, cols) {
  if (about === null) return [];
  return [
    [cell(ABOUT_HEADER, { fg: "bright", bold: true })],
    ...wrapValue(about, cols).map((text) => [cell(text)]),
    [],
  ];
}

/**
 * The info page for `options.info`, or `null` when it names no service on this board — the
 * `info_view(…) -> Option<InfoView>` seam. `null` is a real arm, not a fault: a service that
 * vanished under an open page falls back to the list, exactly as `draw_info` does.
 */
export function infoView(board, options) {
  const name = options.info ?? null;
  if (name === null) return null;
  const svc = board.services[name];
  if (svc === undefined) return null;
  return {
    title: `afkd › ${svc.name} · ${svc.badge}`,
    // The card's own sentence, when it has one — the same "empty means absent" key the
    // conditional `Queue` row folds on.
    about: svc.description === "" ? null : svc.description,
    sections: infoSections(svc, options.now),
  };
}

/// The page's parts at a viewport: the band, the whole unbounded body, the footer, and how many
/// body rows the viewport leaves between them. `null` when there is no page to draw.
function infoPlan(board, options) {
  const view = infoView(board, options);
  if (view === null) return null;
  const cols = Math.max(1, Math.trunc(options.cols));
  const rows = Math.max(1, Math.trunc(options.rows));
  const footer = footerRows(
    planFooter(board, [], -1, "", board.quittingSince !== null, null, options.flash ?? null, true),
    cols,
    rows,
  );
  const band = infoBandRows(view.about, cols);
  const body = infoBody(view.sections, cols);
  const height = Math.max(0, rows - INFO_TITLE_ROWS - band.length - footer.length);
  return { view, band, body, footer, height, cols, rows };
}

/**
 * How far the info page can scroll at this viewport — the twin of [`bodyHeight`], so the shell
 * clamps against the same arithmetic that paints. `0` when the page fits (or when there is no
 * page), which is what makes the wheel inert on a page with nothing below the fold.
 */
export function infoScrollMax(board, options) {
  const plan = infoPlan(board, options);
  if (plan === null) return 0;
  return Math.max(0, plan.body.length - plan.height);
}

/// The info page composed onto a `cols × rows` grid: the title band and the `About` band pinned
/// above, the body sliced from a clamped `infoOffset`, the footer pinned below.
function infoFrame(plan, options) {
  const { cols, rows } = plan;
  const offset = Math.min(
    Math.max(0, Math.trunc(options.infoOffset ?? 0)),
    Math.max(0, plan.body.length - plan.height),
  );
  const screen = [fitRow([cell(`${plan.view.title} `)], cols), fitRow([], cols)];
  for (const line of plan.band) screen.push(fitRow(line, cols));
  for (let i = 0; i < plan.height; i += 1) screen.push(fitRow(plan.body[offset + i] ?? [], cols));
  for (const row of plan.footer) screen.push(fitRow(hintRowCells(row), cols));
  return screen.slice(0, rows);
}

// --- the whole screen --------------------------------------------------------------

/**
 * Lay `board` out on a grid of `cols × rows` cells as of `now`, returning one array of cells
 * per screen row — exactly `rows` of them, each summing to exactly `cols`.
 *
 * The bands, top to bottom, are `shell::render_list`'s: a pinned title band (the title bar,
 * the host-load strip and the spacer above the columns — `shell::LIST_TITLE_ROWS`), the
 * column header, the flexed body, and the footer pinned at the bottom. The body is the one
 * sequence `list_view` renders — the service and group rows, then the `Queues` section's
 * blank spacer, header and lanes — so the row the cursor indexes is the row that paints.
 *
 * Options:
 * - `selected` — the cursor's index over the body sequence. Out of range means no selection,
 *   which is a real footer arm (`layout::footer`'s `None`), not a fault.
 * - `offset` — the body's scroll top, the index of the first row painted. `scrollOffset()`
 *   computes it; the shell holds it between frames because a scroll position is per tab.
 * - `filter` — the `/` needle, matched as a substring over the qualified name.
 * - `collapsed` — the folded group paths (a `Set`), absent meaning expanded (D3).
 * - `typing` — the needle being typed, or `null`. It owns the footer while set.
 * - `flash` — the transient one-line message, or `null`. Second in the footer's precedence.
 * - `confirm` — `{verb, targets, skipped}`, the pending confirm modal, or `null`.
 * - `help` — whether the `?` overlay is up.
 * - `version` — the daemon's version for the title bar's identity prefix. The fold ignores
 *   the handshake frame (it is not a wire frame), so the shell reads it off the `welcome`
 *   event and hands it in here, exactly as it reads `log_lines` off `stream` and hands it to
 *   `seed()`. Absent, the prefix is a bare `afkd · `.
 *
 * **The body scrolls, the popups do not.** The body takes what the pinned bands leave and is
 * sliced from `offset`; a popup is centered on the finished frame and clipped to it, with
 * everything behind it receded (`shell::render_popup`).
 */
export function layout(board, options) {
  const cols = Math.max(1, Math.trunc(options.cols));
  const rows = Math.max(1, Math.trunc(options.rows));
  // The info page replaces the list, but not the overlays: the confirm modal and the `?`
  // overlay outrank it, exactly as `shell.rs` puts `render_overlays` above the view choice. A
  // page whose service has vanished under it returns `null` and the list is drawn instead —
  // `draw_info`'s own fallback — while the session keeps the page open, so a service that
  // comes back comes back to its page.
  const info = infoPlan(board, options);
  if (info !== null) return withOverlays(infoFrame(info, options), board, options, cols, rows);
  const now = options.now;
  const selected = options.selected ?? 0;
  const filter = options.filter ?? "";
  const collapsed = options.collapsed ?? new Set();
  const typing = options.typing ?? null;
  const flash = options.flash ?? null;
  const version = options.version ?? "";
  const quitting = board.quittingSince !== null;

  const listRows = visibleRows(board, { filter, collapsed });
  const lanes = queues(board);

  // The `Service` column's content fit, folded over the **same** rows that render and
  // through the same composition, so the measured content is exactly what is drawn.
  const content = listRows.reduce((w, row) => Math.max(w, textWidth(identityText(row))), 0);
  const visible = visibleColumns(cols);
  const widths = {};
  for (const column of visible) widths[column.key] = columnRenderWidth(column.key, content, cols);
  const laneCols = queueCols(cols, lanes.reduce((w, l) => Math.max(w, textWidth(laneIdentity(l))), 0));
  // The rollups a header shows. `fold.mjs`'s `groups()` buckets by the **exact** group
  // string and says so: "the fold has no tree to collapse into yet … the nesting belongs
  // with the paint". This is the paint, so the transitive fold (ADR-0073 — a crash three
  // levels down still surfaces on the root header that hides it) happens here, over the
  // filtered card set the rows were built from, mirroring `DashboardModel::group_rollup`.
  const members = board.order
    .map((name) => board.services[name])
    .filter((svc) => svc !== undefined && keptByFilter(svc.name, filter));
  const rollups = {};
  for (const row of listRows) {
    if (row.kind === "group") rollups[row.path] = groupRollup(members, row.path);
  }

  const footer = footerRows(
    planFooter(board, listRows, selected, filter, quitting, typing, flash, false),
    cols,
    rows,
  );
  // The title band and the column header are pinned above, the footer below; the body flexes
  // into whatever is left, and takes nothing when the viewport cannot hold the chrome.
  const height = Math.max(0, rows - LIST_CHROME_ROWS - footer.length);
  // Clamped here as well as in `scrollOffset`, because a caller that never scrolled still
  // hands in the `0` default and a stale offset from a taller viewport must not blank the body.
  const offset = Math.min(Math.max(0, Math.trunc(options.offset ?? 0)), Math.max(0, listRows.length - height));

  const screen = [];
  screen.push(fitRow(planHeader(headerView(board, now, version), cols), cols));
  const strip = planLoadStrip(board, now, cols);
  screen.push(fitRow(strip ?? [], cols));
  screen.push(fitRow([], cols));
  screen.push(fitRow(columnHeaderCells(visible, widths), cols));
  for (let i = 0; i < height; i += 1) {
    const at = offset + i;
    const row = listRows[at];
    if (row === undefined) {
      screen.push(fitRow([], cols));
      continue;
    }
    const cells = fitRow(bodyRowCells(row, visible, widths, laneCols, rollups, now), cols);
    screen.push(at === selected ? selectRow(cells) : cells);
  }
  for (const row of footer) screen.push(fitRow(hintRowCells(row), cols));
  const frame = screen.slice(0, rows);

  // The overlays, highest precedence first — `shell::render_overlays`' own order, minus the
  // disconnected notice, which on this page is the connection `notice` the shell overwrites
  // the last row with. The confirm modal outranks help: it captures the keys, so it must own
  // the screen too.
  return withOverlays(frame, board, options, cols, rows);
}

/// `shell::render_overlays`' own order over a finished frame, minus the disconnected notice
/// (on this page that is the connection `notice` the shell overwrites the last row with). The
/// confirm modal outranks help: it captures the keys, so it must own the screen too. Hoisted
/// out of [`layout`] because the info page and the list share it — both are base views, and an
/// overlay that only rose over one of them would be an overlay the operator can hide behind.
function withOverlays(frame, board, options, cols, rows) {
  if (options.confirm != null) {
    return renderPopup(frame, confirmModal(board, options.confirm, board.quittingSince !== null), cols, rows);
  }
  if (options.help === true) return renderPopup(frame, helpOverlay(cols, rows), cols, rows);
  return frame;
}

/// The column header row — the visible labels in their own columns, painted bold, the same
/// weight `render_list` gives it.
function columnHeaderCells(visible, widths) {
  const out = [];
  visible.forEach((column, i) => {
    if (i > 0) out.push(blank(COLUMN_SPACING));
    out.push(cell(padWidth(clipWidth(column.label, widths[column.key]), widths[column.key]), {
      fg: "bright",
      bold: true,
    }));
  });
  return out;
}

/// One body row's cells. A service or group row is laid over the list's visible columns; the
/// `Queues` section's three kinds are laid over the section's **own** five, from column 0.
function bodyRowCells(row, visible, widths, laneCols, rollups, now) {
  if (row.kind === "spacer") return [];
  if (row.kind === "queuesHeader") {
    return queueCompose(
      laneCols,
      QUEUE_COLUMNS.map((label) => [cell(label, { fg: "bright", bold: true })]),
    );
  }
  if (row.kind === "queue") return queueCompose(laneCols, queueLaneCells(laneCols, row.lane));

  const identity = identityOf(row);
  const parts = {
    service: identityCells(identity.prefix, identity.body, identity.name, identity.tint),
    state: [],
    trigger: [],
    liveness: [],
    next: [],
  };
  if (row.kind === "service") {
    const svc = row.svc;
    const style = BADGE_STYLE[svc.badge] ?? BADGE_STYLE.Idle;
    parts.state = [cell(stateText(svc, now), style)];
    parts.trigger = [cell(truncateWidth(svc.triggerLabel, widths.trigger ?? 0), { fg: identity.tint })];
    const liveness = livenessCell(svc.lastActivityAt, now);
    parts.liveness = [cell(liveness.text, { fg: liveness.stale ? "recede" : identity.tint })];
    const next = nextCell(svc, now);
    parts.next = [cell(next.text, { fg: next.far ? "recede" : identity.tint })];
  } else {
    // A group header carries **no** state badge in the resting case (ADR-0066): health is
    // surfaced, ordinary running/stopped state is not. The one exception is a crashed member,
    // which shows through on the header as the `Crashed` badge itself. Its `Trigger` and
    // `Next` are blank — a group belongs to no single member, and a min-of-members countdown
    // is a number that belongs to nobody.
    const rollup = rollups[row.path];
    if (rollup !== undefined && rollup.anyCrashed) {
      parts.state = [cell(`${BADGE_GLYPH.Crashed} Crashed`, BADGE_STYLE.Crashed)];
    }
    // The group `Last Activity` cell folds the rollup's instant through the same seam a
    // service row folds its own anchor through, so the header inherits the identical
    // freshness threshold with no parallel logic.
    const liveness = livenessCell(rollup?.latestActivityAt ?? null, now);
    parts.liveness = [cell(liveness.text, { fg: liveness.stale ? "recede" : "ink" })];
  }

  const out = [];
  visible.forEach((column, i) => {
    if (i > 0) out.push(blank(COLUMN_SPACING));
    const width = widths[column.key];
    const group = parts[column.key];
    const fitted = [];
    let spent = 0;
    for (const c of group) {
      if (spent >= width) break;
      if (spent + c.width <= width) {
        if (c.width > 0) fitted.push(c);
        spent += c.width;
        continue;
      }
      const text = clipWidth(c.text, width - spent);
      if (text !== "") {
        fitted.push({ ...c, text, width: textWidth(text) });
        spent += textWidth(text);
      }
      break;
    }
    out.push(...fitted);
    out.push(blank(width - spent));
  });
  return out;
}
