// The keymap: afkd's stock bindings, transcribed for a browser.
//
// The keymap is **not on the control wire** — `crates/app/src/proto.rs` carries no keys
// field — so a companion never sees the operator's effective `Keymap`. What it can do is
// transcribe the *defaults* verbatim: [`DEFAULT_KEYS`] below is `crates/config/src/keymap.rs`'s
// 51-row table, same scopes, same action names, same binding strings including the alternates
// (`quit` is `q` or `ctrl-c`; `cancel` is `n` or `esc`). `keymap.test.mjs` reads the Rust table
// and deep-equals it against this one, so a row gained, lost or rebound there is a failing test
// here rather than a page that stopped answering a key.
//
// What it costs is stated rather than hidden: an operator who rebinds `x` sees `x` on this page
// and a different key in their terminal. What it buys is the rule every surface here keeps —
// an action with no chord renders no hint and takes no key, because a page must never advertise
// something unpressable.
//
// Three vocabularies live here because all three are per-action and must not drift from the
// rows: the glyphs (`crates/tui/src/keys.rs`'s `glyph`/`named_glyph`), the Sentence-case
// descriptions the `?` overlay spells (`crates/tui/src/help.rs`'s `help_lines` where it has
// one), and the notes saying why an action this page has no surface for is listed but inert.

// --- the table ----------------------------------------------------------------------

/// The scopes, in `Scope`'s own declaration order and spelled with its `dsl_name`
/// (`keymap.rs:99`). The order is load-bearing twice: it is the order the `?` overlay
/// groups by, and its prefix `global` → `queues` → `overview` is the precedence
/// `translate_list_key` walks.
export const SCOPES = [
  "global",
  "overview",
  "queues",
  "output",
  "output.tree",
  "output.log",
  "info",
  "confirm",
];

/// `afkd_config`'s `DEFAULT_KEYS` (`crates/config/src/keymap.rs:285-346`), row for row and in
/// table order. `binding` is the Rust's own spelling — a whitespace-separated run of
/// alternative **atoms**, either of which fires — so the pinning test compares strings it did
/// not have to normalise.
export const DEFAULT_KEYS = [
  // global
  { scope: "global", action: "quit", binding: "q ctrl-c" },
  { scope: "global", action: "reload", binding: "ctrl-r" },
  { scope: "global", action: "help", binding: "?" },
  // overview
  { scope: "overview", action: "service_start", binding: "s" },
  { scope: "overview", action: "service_stop", binding: "x" },
  { scope: "overview", action: "service_fire", binding: "t" },
  { scope: "overview", action: "service_restart", binding: "r" },
  { scope: "overview", action: "service_peek", binding: "enter space" },
  { scope: "overview", action: "show_output", binding: "o" },
  { scope: "overview", action: "show_info", binding: "i" },
  { scope: "overview", action: "group_collapse", binding: "h left" },
  { scope: "overview", action: "group_expand", binding: "l right" },
  { scope: "overview", action: "group_collapse_all", binding: "H" },
  { scope: "overview", action: "group_expand_all", binding: "L" },
  { scope: "overview", action: "view", binding: "v" },
  { scope: "overview", action: "up", binding: "k up" },
  { scope: "overview", action: "down", binding: "j down" },
  { scope: "overview", action: "first", binding: "g" },
  { scope: "overview", action: "last", binding: "G" },
  { scope: "overview", action: "filter", binding: "/" },
  { scope: "overview", action: "filter_clear", binding: "esc" },
  { scope: "overview", action: "filter_busy", binding: "b" },
  { scope: "overview", action: "queue_widen", binding: "+" },
  { scope: "overview", action: "queue_narrow", binding: "-" },
  // queues (the overview's lane rows) — `h`/`l` here shadow the `overview` group fold, which
  // is live at the same instant but consulted second.
  { scope: "queues", action: "narrow", binding: "h left" },
  { scope: "queues", action: "widen", binding: "l right" },
  // output frame
  { scope: "output", action: "focus_next", binding: "tab" },
  { scope: "output", action: "scope", binding: "s" },
  { scope: "output", action: "back", binding: "o esc" },
  // output.tree
  { scope: "output.tree", action: "up", binding: "k up" },
  { scope: "output.tree", action: "down", binding: "j down" },
  { scope: "output.tree", action: "toggle", binding: "enter space" },
  { scope: "output.tree", action: "collapse", binding: "h left" },
  { scope: "output.tree", action: "expand", binding: "l right" },
  { scope: "output.tree", action: "first", binding: "g" },
  { scope: "output.tree", action: "follow", binding: "f" },
  { scope: "output.tree", action: "follow_frontier", binding: "G" },
  { scope: "output.tree", action: "expand_all", binding: "L" },
  { scope: "output.tree", action: "collapse_all", binding: "H" },
  // output.log
  { scope: "output.log", action: "up", binding: "k up" },
  { scope: "output.log", action: "down", binding: "j down" },
  { scope: "output.log", action: "half_up", binding: "ctrl-u" },
  { scope: "output.log", action: "half_down", binding: "ctrl-d" },
  { scope: "output.log", action: "page_up", binding: "pageup" },
  { scope: "output.log", action: "page_down", binding: "pagedown" },
  { scope: "output.log", action: "top", binding: "g" },
  { scope: "output.log", action: "bottom", binding: "G" },
  { scope: "output.log", action: "follow", binding: "f" },
  // info
  { scope: "info", action: "back", binding: "i esc" },
  // confirm
  { scope: "confirm", action: "accept", binding: "y" },
  { scope: "confirm", action: "cancel", binding: "n esc" },
];

/// One row's dotted id — `overview.service_fire` — the spelling every caller here and in
/// `layout.mjs` names an action by.
export function idOf(row) {
  return `${row.scope}.${row.action}`;
}

/// Every action's dotted id, in table order.
export const ACTIONS = DEFAULT_KEYS.map(idOf);

// --- chords -------------------------------------------------------------------------

/// The named tokens the binding grammar admits (`config::keymap`'s `NamedKey`), mapped to the
/// Title-case / arrow glyph `keys::named_glyph` spells them with.
const NAMED_GLYPH = {
  up: "↑",
  down: "↓",
  left: "←",
  right: "→",
  enter: "Enter",
  space: "Space",
  esc: "Esc",
  tab: "Tab",
  pageup: "PgUp",
  pagedown: "PgDn",
  backspace: "Backspace",
};

/// One atom parsed into a chord — `{ctrl, key}`, `key` being a named token or a single char.
/// The grammar's own shape: a `ctrl-` prefix modifies exactly one following atom, and the
/// lowercase spelling is the only one the Rust admits (`ConfigError::KeyCtrlUppercase`).
function chordOfAtom(atom) {
  const ctrl = atom.startsWith("ctrl-");
  const key = ctrl ? atom.slice(5) : atom;
  return { ctrl, key };
}

const CHORDS = new Map(
  DEFAULT_KEYS.map((row) => [idOf(row), row.binding.split(" ").filter((a) => a !== "").map(chordOfAtom)]),
);

/**
 * Every chord bound to `id`, in table order. An unknown id has none — the "no chord ⇒ no
 * hint, no key" rule's one seam.
 */
export function chords(id) {
  return CHORDS.get(id) ?? [];
}

/**
 * `keys::glyph` — the canonical display spelling of one chord: a named key Title-cased or
 * arrowed, `Ctrl+<X>` with the letter uppercased (lossless: the grammar admits only the
 * lowercase spelling), a bare char verbatim because its case is semantic (`g` and `G` are two
 * keys).
 */
export function glyph(chord) {
  const named = NAMED_GLYPH[chord.key];
  const key = named ?? (chord.ctrl ? chord.key.toUpperCase() : chord.key);
  return chord.ctrl ? `Ctrl+${key}` : key;
}

/// `Keys::primary` — the display glyph for `id`'s **first** chord, or `null` when unbound.
/// One glyph per action: the alias is an escape hatch, not an advertisement.
export function primary(id) {
  const first = chords(id)[0];
  return first === undefined ? null : glyph(first);
}

/// `Keys::all` — every chord `/`-joined (`Enter/Space`, `h/←`), for the cells that
/// deliberately teach both spellings. `null` when unbound.
export function all(id) {
  const bound = chords(id);
  return bound.length === 0 ? null : bound.map(glyph).join("/");
}

/// Every glyph bound to `id`, as an array — what `layout.mjs`'s `keysOf` reads its chord
/// list off, so the footer and this table cannot drift.
export function glyphs(id) {
  return chords(id).map(glyph);
}

/// Whether two chords are the same key press.
function sameChord(a, b) {
  return a.ctrl === b.ctrl && a.key === b.key;
}

/**
 * The action `chord` resolves to, walking `scopes` in order and taking the first hit — the
 * precedence `translate_list_key` implements, expressed as data rather than as an `if` chain.
 * A caller on a lane row passes `["global", "queues", "overview"]`; anywhere else
 * `["global", "overview"]`, and `queues` shadows nothing it should not.
 *
 * `null` when no scope in the list binds it.
 */
export function resolve(scopes, chord) {
  for (const scope of scopes) {
    for (const row of DEFAULT_KEYS) {
      if (row.scope !== scope) continue;
      if (chords(idOf(row)).some((c) => sameChord(c, chord))) return idOf(row);
    }
  }
  return null;
}

/// `keys::REFUSED_WHILE_QUITTING` — the display *and* input half of the daemon's blanket drain
/// refusal (`top_command`'s `refused_now`). A refused action reads as **unbound** on both
/// sides from this one table, so the footer cannot advertise a key the drain would swallow and
/// the dispatch cannot send one the footer stopped advertising.
export const REFUSED_WHILE_QUITTING = [
  "overview.service_start",
  "overview.service_stop",
  "overview.service_fire",
  "overview.service_restart",
  "overview.queue_widen",
  "overview.queue_narrow",
  "queues.narrow",
  "queues.widen",
  "global.reload",
];

// --- what each action is, and whether this page takes it -------------------------------

/// One Sentence-case line per action — lifted from `help::help_lines` wherever that file
/// spells one, and written in its register where it pairs two actions into a row this page
/// lists separately (`Move selection` becomes an up and a down; `First / last` a first and a
/// last). The `?` overlay is their one reader.
export const DESCRIPTIONS = {
  "global.quit": "Quit",
  "global.reload": "Reload config",
  "global.help": "Close help",
  "overview.service_start": "Start service",
  "overview.service_stop": "Stop service",
  "overview.service_fire": "Trigger an idle service",
  "overview.service_restart": "Restart service",
  "overview.service_peek": "Toggle group",
  "overview.show_output": "Output",
  "overview.show_info": "Info view",
  "overview.group_collapse": "Collapse group",
  "overview.group_expand": "Expand group",
  "overview.group_collapse_all": "Collapse all groups",
  "overview.group_expand_all": "Expand all groups",
  "overview.view": "Flat view",
  "overview.up": "Move selection up",
  "overview.down": "Move selection down",
  "overview.first": "First row",
  "overview.last": "Last row",
  "overview.filter": "Find (filter)",
  "overview.filter_clear": "Clear filter",
  "overview.filter_busy": "Show only busy services",
  "overview.queue_widen": "Widen the service's lane",
  "overview.queue_narrow": "Narrow the service's lane",
  "queues.narrow": "Narrow the lane at the cursor",
  "queues.widen": "Widen the lane at the cursor",
  "output.focus_next": "Show the other pane",
  "output.scope": "Scope log to node / all",
  "output.back": "Back to list",
  "output.tree.up": "Move cursor up",
  "output.tree.down": "Move cursor down",
  "output.tree.toggle": "Toggle collapse / output",
  "output.tree.collapse": "Collapse / to parent",
  "output.tree.expand": "Expand / to child",
  "output.tree.first": "Top",
  "output.tree.follow": "Toggle follow",
  "output.tree.follow_frontier": "Follow the frontier",
  "output.tree.expand_all": "Expand all",
  "output.tree.collapse_all": "Collapse all",
  "output.log.up": "Scroll up a line",
  "output.log.down": "Scroll down a line",
  "output.log.half_up": "Half page up",
  "output.log.half_down": "Half page down",
  "output.log.page_up": "Page up",
  "output.log.page_down": "Page down",
  "output.log.top": "Top",
  "output.log.bottom": "Bottom",
  "output.log.follow": "Toggle follow",
  "info.back": "Back to list",
  "confirm.accept": "Confirm",
  "confirm.cancel": "Cancel",
};

/// The actions this page's dispatch really takes. Everything else in the table is **bound and
/// listed** — the `?` overlay names it with the note below — because a chord with a binding is
/// part of the keymap whether or not this surface has somewhere to send it.
export const HANDLED = new Set([
  "global.reload",
  "global.help",
  "overview.service_start",
  "overview.service_stop",
  "overview.service_fire",
  "overview.service_restart",
  "overview.service_peek",
  "overview.show_info",
  "overview.group_collapse",
  "overview.group_expand",
  "overview.group_collapse_all",
  "overview.group_expand_all",
  "overview.up",
  "overview.down",
  "overview.first",
  "overview.last",
  "overview.filter",
  "overview.filter_clear",
  "info.back",
  "confirm.accept",
  "confirm.cancel",
]);

/// Why an action is not taken here, or — for the one action this page takes only on some rows
/// — where it stops. Short by design: the `?` overlay lists each note beside its row, and a
/// sentence wide enough to explain itself twice would set the overlay's column width.
export const NOTES = {
  "global.quit": "The browser owns Ctrl+C",
  "overview.service_peek": "Groups only — no activity peek",
  "overview.show_output": "No run view on this page",
  "overview.show_info": "Services only — a group has no info page",
  "overview.view": "No flat view on this page",
  "overview.filter_busy": "No busy lens on this page",
  "overview.queue_widen": "The relay has no lane verb",
  "overview.queue_narrow": "The relay has no lane verb",
  "queues.narrow": "The relay has no lane verb",
  "queues.widen": "The relay has no lane verb",
  "output.focus_next": "No run view on this page",
  "output.scope": "No run view on this page",
  "output.back": "No run view on this page",
  "output.tree.up": "No run view on this page",
  "output.tree.down": "No run view on this page",
  "output.tree.toggle": "No run view on this page",
  "output.tree.collapse": "No run view on this page",
  "output.tree.expand": "No run view on this page",
  "output.tree.first": "No run view on this page",
  "output.tree.follow": "No run view on this page",
  "output.tree.follow_frontier": "No run view on this page",
  "output.tree.expand_all": "No run view on this page",
  "output.tree.collapse_all": "No run view on this page",
  "output.log.up": "No run view on this page",
  "output.log.down": "No run view on this page",
  "output.log.half_up": "No run view on this page",
  "output.log.half_down": "No run view on this page",
  "output.log.page_up": "No run view on this page",
  "output.log.page_down": "No run view on this page",
  "output.log.top": "No run view on this page",
  "output.log.bottom": "No run view on this page",
  "output.log.follow": "No run view on this page",
};

/// The two actions that are in **both** sets, in table order. `Enter`/`Space` folds a group
/// header here and, on a service row — where the terminal opens an activity peek — does
/// nothing; `i` opens a **service**'s info page and, on a group header or a lane row, has no
/// subject to open. Named rather than inferred, so the assert below can hold every other action
/// to exactly one side.
export const PARTIAL = ["overview.service_peek", "overview.show_info"];

// Every row is described, and is either handled or noted — so a row a future transcription
// adds cannot land with no description, no dispatch and no explanation. The intersection is
// pinned to `PARTIAL` rather than merely allowed, so a second half-handled action has to be
// declared here before it ships.
for (const id of ACTIONS) {
  if (DESCRIPTIONS[id] === undefined) throw new Error(`keymap.mjs: ${id} has no description`);
  if (!HANDLED.has(id) && NOTES[id] === undefined) {
    throw new Error(`keymap.mjs: ${id} is neither handled nor noted`);
  }
}
{
  const both = ACTIONS.filter((id) => HANDLED.has(id) && NOTES[id] !== undefined);
  if (both.join(",") !== PARTIAL.join(",")) {
    throw new Error(`keymap.mjs: ${both.join(", ") || "nothing"} is both handled and noted; PARTIAL says ${PARTIAL.join(", ")}`);
  }
}
