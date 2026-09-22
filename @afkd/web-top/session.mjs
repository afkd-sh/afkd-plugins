// The session: one tab's own cursor, folds, filter, modal, overlay and flash, and the
// dispatch that turns a key press into wire commands.
//
// `fold.mjs` answers *what the daemon is doing* and `layout.mjs` *what that looks like*; this
// answers *what this operator is looking at and has asked for*. In afkd that third half is
// `DashboardModel`'s `UiState` plus `shell::translate_key` plus `top::top_command` — three
// private surfaces of two unpublished crates — so, like the fold and the layout, it is
// re-stated here arm for arm with the file each arm was read off cited.
//
// Three properties it holds on purpose:
//
// - **Per tab, never shared.** Every browser gets its own attach from the relay, so every
//   browser gets its own session: a second tab moving its cursor, typing a needle or folding
//   a group moves nothing here. Nothing in this module reaches a storage API or sends a
//   `Frame::View`, which is how `afkd top` makes folds daemon-held and survive a re-attach —
//   doing that would make two tabs share one cursor, which is exactly what must not happen.
// - **Pure, DOM-free, clock-free.** `press()` and `noteFrame()` take `now` as an argument and
//   return a new session plus the commands to post; they never write through the one they
//   were given. So they run under `node --test` with no browser underneath.
// - **The board moves only on a frame.** Nothing here touches a badge. A verb is posted and
//   the row changes when the daemon's answering event lands — which is what `afkd top` does
//   for a fire, and what this page does for all six.

import { HANDLED, REFUSED_WHILE_QUITTING, resolve } from "./keymap.mjs";
import { rowKey, runTreeNav, scrollOffset, visibleRows } from "./layout.mjs";

/// `model::FLASH_TIMEOUT` — how long a transient footer message stays up. Long enough to read
/// a one-line reload summary, short enough that it gets out of the way.
export const FLASH_TIMEOUT = 4000;

/**
 * A new, empty session — the state one tab holds between frames.
 *
 * `cursor` is a row **identity** (`layout::rowKey`), mirroring `model::RowKey`, so a fold that
 * adds or drops a service cannot slide the cursor onto a neighbour; `cursorRow` is where that
 * key last resolved, the index-domain last resort `resolve_selection` falls back to when the
 * keyed row has vanished entirely. `collapsed` holds the **folded** group paths — absent means
 * expanded, the inversion of the terminal's `expanded` set and the reason is in `layout.mjs`'s
 * `buildRows`.
 *
 * `info` is the **name** of the service whose info page is open, or `null` for the list —
 * `model::info_service`, a name rather than a row index for the same reason the cursor is a key:
 * a fold that adds or drops a service must not slide the page onto a neighbour. It is
 * deliberately **not** cleared when that service vanishes: `layout()` falls back to the list for
 * a page with no subject (`draw_info`'s own arm) while the key surface stays the info one, so a
 * service that comes back comes back to its page. `infoOffset` is that page's scroll top.
 *
 * `run` is the open **run view**, or `null` for the list — [`openRun`]'s record, on the same
 * terms as `info` and for the same reason: it names its service rather than indexing a row, and
 * it is not cleared when that service vanishes, so `layout()` falls back to the list
 * (`draw_split`'s own arm) while the key surface stays the run one.
 */
export function newSession() {
  return {
    cursor: null,
    cursorRow: 0,
    offset: 0,
    collapsed: new Set(),
    filter: { mode: "off", needle: "" },
    confirm: null,
    help: false,
    flash: null,
    info: null,
    infoOffset: 0,
    run: null,
  };
}

// --- reading the session -------------------------------------------------------------

/// The `/` needle in force, or `""` — `Filter::Active`'s half. A needle being typed does not
/// filter yet in the terminal's `Filter::Typing`… except that it does: `visible_cards` reads
/// the needle from either arm, so the rows narrow live as the operator types.
export function needleOf(session) {
  return session.filter.mode === "off" ? "" : session.filter.needle;
}

/// The needle being typed, or `null` — the footer's precedence-1 arm.
export function typingOf(session) {
  return session.filter.mode === "typing" ? session.filter.needle : null;
}

/// The live flash message at `now`, or `null` once it has aged past [`FLASH_TIMEOUT`].
export function flashOf(session, now) {
  const flash = session.flash;
  if (flash === null || now - flash.postedAt >= FLASH_TIMEOUT) return null;
  return flash.message;
}

/// This session's rows of `board` — the one sequence the cursor indexes and the paint draws.
export function rowsOf(session, board) {
  return visibleRows(board, { filter: needleOf(session), collapsed: session.collapsed });
}

/// Whether a row may carry the cursor — `model::is_selectable`. The section's spacer and its
/// `Queues` header are the two that may not, so `j` off the last service lands on the first
/// lane with no special case.
function selectable(row) {
  return rowKey(row) !== null;
}

/// `model::nearest_selectable` — the nearest selectable row to `from`, searching the preferred
/// direction first and then the other, and falling back to `from` when the row set holds none.
function nearestSelectable(rows, from, preferDown) {
  const at = Math.min(Math.max(0, from), Math.max(0, rows.length - 1));
  if (rows.length === 0) return 0;
  if (selectable(rows[at])) return at;
  let down = -1;
  for (let i = at + 1; i < rows.length; i += 1) {
    if (selectable(rows[i])) {
      down = i;
      break;
    }
  }
  let up = -1;
  for (let i = at - 1; i >= 0; i -= 1) {
    if (selectable(rows[i])) {
      up = i;
      break;
    }
  }
  const [first, second] = preferDown ? [down, up] : [up, down];
  if (first !== -1) return first;
  if (second !== -1) return second;
  return at;
}

/**
 * The index this session's cursor resolves to over `rows` — `DashboardModel::resolve_selection`:
 * the keyed row wherever it still exists, else the nearest selectable row at the index it was
 * last seen at. `null` for an empty row set, which is a real arm (`layout::footer`'s no-selection
 * case), not a fault.
 */
export function selectedIndex(session, rows) {
  if (rows.length === 0) return null;
  if (session.cursor !== null) {
    const at = rows.findIndex((row) => {
      const key = rowKey(row);
      return key !== null && key.kind === session.cursor.kind && key.key === session.cursor.key;
    });
    if (at !== -1) return at;
  }
  return nearestSelectable(rows, session.cursorRow, true);
}

/// The session with its cursor moved onto `rows[at]` — both halves of `Selection`, so the
/// index-domain fallback names where the row **was** after an ambient shift.
function pointAt(session, rows, at) {
  const settled = nearestSelectable(rows, at, true);
  return { ...session, cursor: rowKey(rows[settled]), cursorRow: settled };
}

// --- the verbs -------------------------------------------------------------------------

/// `Badge::can_start` / `can_stop` / `can_fire` — the gates `command_for_card` applies, and the
/// same predicates `layout.mjs`'s footer weights its hints by. Restated here rather than
/// imported because the two files read them for different reasons and neither owns the other.
function canStart(badge) {
  return badge === "Stopped" || badge === "Crashed";
}
function canStop(badge) {
  return ["Starting", "Idle", "Queued", "Checking", "Busy"].includes(badge);
}
function canFire(badge) {
  return badge === "Idle";
}

/// `model::in_subtree` — a group path covers itself and every descendant, on `::` boundaries.
function inSubtree(group, path) {
  return group === path || group.startsWith(`${path}::`);
}

/// `top::command_for_card` — whether `verb` is eligible against one service, and nothing else:
/// the poison gate on `start`/`restart` (a force-abandoned card's leaked thread may still hold
/// the trigger's claim, so the client never sends a re-arm the daemon would no-op), the badge
/// gate on the rest.
function eligible(verb, svc) {
  switch (verb) {
    case "start":
      return canStart(svc.badge) && !svc.poisoned;
    case "stop":
      return canStop(svc.badge);
    case "fire":
      return canFire(svc.badge);
    case "restart":
      // A live service restarts through the stop edge; a stopped or crashed one re-arms, with
      // the same poison gate `start` carries. A `Stopping` card is a gated no-op: its thread is
      // already joining and the supervisor drops the restart.
      return canStop(svc.badge) || (["Stopped", "Crashed"].includes(svc.badge) && !svc.poisoned);
    case "force":
      return svc.badge === "Stopping";
    default:
      return false;
  }
}

/// The services a row's verb reaches — `DashboardModel::selected_targets`: one card on a
/// service row, every member of the subtree on a group header (the transitive reading ADR-0073
/// and the footer both use), none on a lane row or the section's header.
function targetsOf(board, row) {
  if (row === undefined) return [];
  if (row.kind === "service") return [row.svc];
  if (row.kind !== "group") return [];
  return board.order
    .map((name) => board.services[name])
    .filter((svc) => svc !== undefined && inSubtree(svc.group, row.path));
}

/// `ack_flash`'s verbs, and the word each acknowledges with.
///
/// The terminal acks a verb when **nothing else on screen will** — its own doc's rule. There,
/// `start`/`stop`/`restart` fold an optimistic edge and the row visibly flips, so three of
/// these are `None`; here the board deliberately folds no edge at all (the row moves when the
/// daemon's event lands), so that premise is gone for exactly those three and their keypress
/// would otherwise produce no feedback whatever. `force` and `reload` are absent for reasons
/// that **do** transplant: the modal was the acknowledgement, and the daemon's own
/// `meta.reloaded.message` owns the reload line.
const ACK = { fire: "Fired", restart: "Restarting", start: "Starting", stop: "Stopping" };

/// `ack_flash` — the press-time acknowledgement for a **dispatched**, post-gate batch. A gated
/// no-op and a verb refused during the drain both dispatch nothing and so acknowledge nothing,
/// because nothing happened; one command names its service, several read `N services`.
function ackFlash(verb, commands) {
  const word = ACK[verb];
  if (word === undefined || commands.length === 0) return null;
  if (commands.length === 1) return `${word} ${commands[0].service}`;
  return `${word} ${commands.length} services`;
}

/// One posted command body, in the shape `POST /command` composes a frame from.
function command(verb, service) {
  return { command: verb, service };
}

/// `shell::fanout_confirm_modal`'s gate: 0 eligible targets is a silent no-op, exactly 1 acts
/// directly, and >1 raises the confirm modal — a multi-service op must say what it is about to
/// do and wait for a `y`. `force` is always gated, even at one target: it abandons a wedged
/// thread, and the modal is the one place that is stated before the key.
function verbFor(session, board, verb, row, now) {
  const targets = targetsOf(board, row).filter((svc) => eligible(verb, svc));
  if (targets.length === 0) return { session, commands: [] };
  const skipped = targetsOf(board, row).length - targets.length;
  if (verb === "force" || targets.length > 1) {
    return {
      session: { ...session, confirm: { verb, targets: targets.map((s) => s.name), skipped } },
      commands: [],
    };
  }
  const commands = [command(verb, targets[0].name)];
  return { session: flashed(session, ackFlash(verb, commands), now), commands };
}

/**
 * The session with `message` in its flash, or unchanged when there is nothing to say —
 * `DashboardModel::flash`. Posting overwrites, so a refusal replaces the ack that preceded it
 * rather than queueing behind it.
 */
export function flash(session, message, now) {
  return flashed(session, message, now);
}

/// The session with `message` in its flash, or unchanged when there is nothing to say.
function flashed(session, message, now) {
  return message === null ? session : { ...session, flash: { message, postedAt: now } };
}

// --- the dispatch ------------------------------------------------------------------------

/// The scopes a chord resolves through at the cursor's row — `translate_list_key`'s precedence
/// as data: `global` first, then `queues` **only** while the cursor sits on a lane row (where it
/// shadows the `overview` group fold, live at the same instant but consulted second), then
/// `overview`.
function scopesAt(row) {
  return row !== undefined && row.kind === "queue"
    ? ["global", "queues", "overview"]
    : ["global", "overview"];
}

/// Whether the drain refuses `id` on this board — `keys::refused_while_quitting`, read from the
/// same table the footer drops its hints by, so the input side and the legend refuse exactly
/// one set and a drain can never leave a pressable key advertised.
function refused(board, id) {
  return board.quittingSince !== null && REFUSED_WHILE_QUITTING.includes(id);
}

/// The confirm modal's own arm — `translate_confirm_key`. The modal captures the whole
/// surface: only `y`/`n`/`Esc` act, every other key is inert, because a stray key must never
/// confirm a force.
function pressConfirm(session, chord, now) {
  const id = resolve(["confirm"], chord);
  if (id === "confirm.accept") {
    const { verb, targets } = session.confirm;
    const commands = targets.map((name) => command(verb, name));
    const cleared = { ...session, confirm: null };
    return { session: flashed(cleared, ackFlash(verb, commands), now), commands, handled: true };
  }
  if (id === "confirm.cancel") {
    return { session: { ...session, confirm: null }, commands: [], handled: true };
  }
  return { session, commands: [], handled: true };
}

/// The `?` overlay's arm — `translate_help_key`. It captures the surface too: its own binding
/// toggles it back off, a literal `Esc` closes it, and quit and reload stay inert beneath it.
function pressHelp(session, chord) {
  if (resolve(["global"], chord) === "global.help") {
    return { session: { ...session, help: false }, commands: [], handled: true };
  }
  if (!chord.ctrl && chord.key === "esc") {
    return { session: { ...session, help: false }, commands: [], handled: true };
  }
  return { session, commands: [], handled: true };
}

/// The info page's arm — `translate_info_key`. The page owns its key surface while it is open
/// (ADR-0030): the `global` keys live in every base view, then the `info` scope's `back` — `i`
/// **or** `Esc`, one action since `info.back` collapsed the old toggle/exit split — returns to
/// the list. Everything else is inert *and reported handled*, the same capture `pressHelp`
/// makes: a key this surface has no arm for must not fall through to the list underneath it.
///
/// The two globals it forwards are the two the list takes; `global.quit` is the browser's here
/// as it is there, and a `global.reload` refused by the drain is dropped by the same table the
/// footer stops advertising it from.
function pressInfo(session, board, chord) {
  const id = resolve(["global", "info"], chord);
  if (id === "info.back") {
    return { session: { ...session, info: null, infoOffset: 0 }, commands: [], handled: true };
  }
  if (id === "global.help") return { session: { ...session, help: true }, commands: [], handled: true };
  if (id === "global.reload" && !refused(board, id)) {
    return { session, commands: [{ command: "reload" }], handled: true };
  }
  return { session, commands: [], handled: true };
}

// --- the run view ----------------------------------------------------------------------
//
// `model`'s `ViewMode::Run` plus `UiState`'s tree/log halves plus `shell::translate_split_key`
// — ADR-0071 for the combined view, ADR-0076 for the one-pane frame, ADR-0068 for the tree and
// ADR-0027 for the log's scroll.

/// The metrics a press falls back on before the first paint has measured a pane: a one-row
/// viewport and two empty panes, so every clamp floors rather than reaching into `undefined`.
const NO_RUN_METRICS = { viewport: 0, treeTotal: 0, logTotal: 0 };

/**
 * `model::enter_run_view` — the run view opened on one service.
 *
 * It opens on the **log** pane with the tree ready behind it (ADR-0076: the full-screen log is
 * what an operator watching a fire actually wants first), the collapse overrides and the
 * body-expanded set cleared, the tree following its frontier, the log following its tail, the
 * scope off and the tree cursor on the first root.
 *
 * The overrides are cleared on every (re)enter for two reasons: the tree then opens at its
 * per-kind defaults (agents folded, the skeleton expanded), and node ids are **per service**, so
 * one service's folds must never bleed onto another's tree.
 */
function openRun(session, svc) {
  return {
    ...session,
    run: {
      // The qualified name, the way `info` names its page's subject.
      service: svc.name,
      focus: "log",
      cursor: svc.tree.roots[0] ?? null,
      // `ui.tree_overrides` — the per-node collapse override, `id → collapsed?`.
      overrides: new Map(),
      // `ui.tree_body_expanded` — the leaf ids showing their own output beneath them.
      expanded: new Set(),
      // `TreeScroll` and `LogScroll`, each as a **sum**: `"follow"` or a frozen top row, never
      // both. `model.rs` makes the point that a `{top, follow}` pair can encode "following with
      // a stale top"; this cannot.
      tree: "follow",
      log: "follow",
      // `ui.log_scoped` — whether the log pane is filtered to the tree cursor's own node. The
      // node itself is resolved off the cursor at paint (`model::log_scope_node`), so a cursor
      // move re-scopes with no second piece of state to keep in step.
      scoped: false,
    },
  };
}

/// `model::half_page` — the `Ctrl+U`/`Ctrl+D` step for a `viewport` of rows: half the viewport,
/// at least one line so a one-row pane still moves. [`fullPage`] is the whole viewport, same
/// floor.
function halfPage(viewport) {
  return Math.max(1, Math.floor(viewport / 2));
}
function fullPage(viewport) {
  return Math.max(1, viewport);
}

/// A `Map` with one entry rewritten — the collapse override map is per node id, so a bulk fold
/// and a single toggle both go through this and neither mutates the map it was handed.
function withEntry(map, key, value) {
  const next = new Map(map);
  next.set(key, value);
  return next;
}

/// A `Set` with one member added or dropped — the body-expanded leaf ids.
function withMember(set, key, member) {
  const next = new Set(set);
  if (member) next.add(key);
  else next.delete(key);
  return next;
}

/**
 * `model::resettle_tree` — re-settle the run view's cursor and scroll after a fold mutation.
 *
 * An emptied tree drops the cursor and pins the top; a cursor now hidden under a
 * newly-collapsed ancestor falls back to the first visible row. Then the scroll re-anchors:
 * `reveal` (a mutation that **opened** rows) pulls the cursor toward the viewport top so its
 * new subtree shows below it, while a collapse or a plain step keeps the cursor clamp — so a
 * collapse never jumps and the cursor is never left dangling on a hidden node.
 *
 * Either way the result is **frozen**: a fold is the reader taking control of the viewport.
 */
function resettleRun(board, run, viewport, reveal) {
  const nav = runTreeNav(board, run, viewport);
  if (nav === null) return run;
  if (nav.nodes.length === 0) return { ...run, cursor: null, tree: 0 };
  const settled = nav.nodes.some((n) => n.id === run.cursor) ? run : { ...run, cursor: nav.nodes[0].id };
  // Re-read against the settled cursor: the anchor is measured from where the cursor **is**,
  // not from where it was before the fold moved it. A cursor the fold left alone needs no
  // second walk.
  const after = settled === run ? nav : runTreeNav(board, settled, viewport);
  return { ...settled, tree: reveal ? after.reveal : after.clamp };
}

/**
 * The run view's arm — `translate_split_key` plus the `Tree*`/`Log*` intents it emits.
 *
 * Scopes resolve **frame before pane** (`global`, then the `output` frame, then
 * `output.tree`/`output.log` by which pane is *shown*), so a stray key can never cross panes —
 * the pane guards in `apply_intent` make that structural there, and here it is the scope list.
 * Like [`pressInfo`], a chord this view has no arm for is captured and reported **handled**: a
 * key struck in the run view must not fall through to the list underneath it.
 *
 * `metrics` is [`runMetrics`]' `{viewport, treeTotal, logTotal}` — the arithmetic that *paints*,
 * so every clamp here is against the rows the pane really drew rather than a second count.
 */
function pressRun(session, board, chord, metrics) {
  const run = session.run;
  const vp = Math.max(1, metrics.viewport);
  const id = resolve(["global", "output", run.focus === "tree" ? "output.tree" : "output.log"], chord);
  const captured = { session, commands: [], handled: true };
  const next = (patch) => ({ session: { ...session, run: { ...run, ...patch } }, commands: [], handled: true });
  /// A fold mutation, then `model::resettle_tree`'s re-anchor over the walk it produced.
  const refold = (patch, reveal) => ({
    session: { ...session, run: resettleRun(board, { ...run, ...patch }, vp, reveal) },
    commands: [],
    handled: true,
  });

  if (id === "output.back") {
    // The list session's `cursor`/`cursorRow`/`offset` were never touched, so the cursor is
    // already exactly where it was left.
    return { session: { ...session, run: null }, commands: [], handled: true };
  }
  if (id === "global.help") return { session: { ...session, help: true }, commands: [], handled: true };
  if (id === "global.reload" && !refused(board, id)) {
    return { session, commands: [{ command: "reload" }], handled: true };
  }
  if (id === "output.focus_next") return next({ focus: run.focus === "tree" ? "log" : "tree" });
  if (id === "output.scope") {
    // `model::toggle_run_scope` — pane-dependent, because with one pane on screen the key means
    // two different things. From the **tree** it *sets* the scope and swaps to the log: setting
    // rather than toggling is what keeps a second `s` from the tree from landing you on an
    // *un*scoped log, which is never what the key means there. From the **log** it is an
    // in-place toggle of the scope that pane's own title names. Both arms re-follow, so the new
    // scope tails its own latest output rather than a stale offset into a different
    // physical-row space.
    return run.focus === "tree"
      ? next({ scoped: true, focus: "log", log: "follow" })
      : next({ scoped: !run.scoped, log: "follow" });
  }

  const nav = runTreeNav(board, run, vp);
  if (nav === null) return captured;
  const cursor = nav.nodes.find((n) => n.id === run.cursor);
  /// `model::move_tree_cursor` — `pick` chooses the destination row over the **node** rows (a
  /// body row is never selectable), the cursor is stored as that row's node id so a later
  /// insert-above keeps it, and the scroll re-clamps against the full walk so it stays on
  /// screen. A manual move freezes follow: a run read by hand must not yank itself away.
  const moveTo = (pick) => {
    if (nav.nodes.length === 0) return captured;
    const at = nav.nodes.findIndex((n) => n.id === run.cursor);
    const to = Math.min(Math.max(0, pick(at === -1 ? 0 : at, nav.nodes.length)), nav.nodes.length - 1);
    const moved = { ...run, cursor: nav.nodes[to].id };
    return { session: { ...session, run: { ...moved, tree: runTreeNav(board, moved, vp).clamp } }, commands: [], handled: true };
  };

  switch (id) {
    case "output.tree.up":
      return moveTo((at) => at - 1);
    case "output.tree.down":
      return moveTo((at) => at + 1);
    case "output.tree.first":
      return moveTo(() => 0);
    case "output.tree.follow_frontier":
      // `model::tree_engage_follow` — pin the cursor to the frontier and follow, so the view
      // jumps to and tracks the growing edge. On a completed run the frontier *is* the last
      // node, so `G` still lands there (it subsumes a jump-to-last).
      return next({ cursor: nav.frontier ?? run.cursor ?? null, tree: "follow" });
    case "output.tree.follow":
      // `model::tree_toggle_follow` — disengaging pins the cursor to the frontier and freezes
      // **where we look** (the resolved anchor, so the view does not jump); re-engaging is `G`.
      if (run.tree !== "follow") return next({ cursor: nav.frontier ?? run.cursor ?? null, tree: "follow" });
      return nav.frontier === null ? next({ tree: 0 }) : next({ cursor: nav.frontier, tree: nav.top });
    case "output.tree.toggle": {
      // `model::tree_toggle_collapse` — a node with children flips its collapse; a childless
      // `cmd`/`tool` leaf flips its own **body**, so `Enter` on a leaf reveals its output.
      if (cursor === undefined) return captured;
      if (cursor.hasChildren) {
        return refold({ overrides: withEntry(run.overrides, cursor.id, !cursor.collapsed) }, cursor.collapsed);
      }
      if (!cursor.bodyCapable) return captured;
      const shown = run.expanded.has(cursor.id);
      return refold({ expanded: withMember(run.expanded, cursor.id, !shown) }, !shown);
    }
    case "output.tree.collapse": {
      // `model::tree_collapse` — collapse an expanded parent, hide an expanded leaf body, else
      // step the cursor onto its parent (selection-follows-collapse, the shape the list's group
      // fold has). Nothing opens, so the anchor is the plain clamp.
      if (cursor === undefined) return captured;
      if (cursor.hasChildren && !cursor.collapsed) {
        return refold({ overrides: withEntry(run.overrides, cursor.id, true) }, false);
      }
      if (cursor.bodyCapable && run.expanded.has(cursor.id)) {
        return refold({ expanded: withMember(run.expanded, cursor.id, false) }, false);
      }
      return cursor.parent === null ? captured : refold({ cursor: cursor.parent }, false);
    }
    case "output.tree.expand": {
      // `model::tree_expand` — the mirror: expand a collapsed parent, show a `cmd`/`tool`
      // leaf's body, else step onto its first child. The two that *reveal* rows anchor toward
      // the top; stepping onto an already-visible child is a plain clamp.
      if (cursor === undefined) return captured;
      if (cursor.collapsed) return refold({ overrides: withEntry(run.overrides, cursor.id, false) }, true);
      if (cursor.firstChild !== null) return refold({ cursor: cursor.firstChild }, false);
      if (!cursor.bodyCapable) return captured;
      return refold({ expanded: withMember(run.expanded, cursor.id, true) }, true);
    }
    case "output.tree.collapse_all":
    case "output.tree.expand_all": {
      // `model::tree_collapse_all` / `tree_expand_all` — a **blanket override** on every parent,
      // written explicitly rather than by clearing the map: `L` has to override the per-kind
      // default too, and a clear would leave every agent at its collapsed one. The cost, which
      // the terminal pays identically because its map is also per id: a node that opens *after*
      // the bulk fold takes its own default rather than the bulk choice.
      const collapse = id === "output.tree.collapse_all";
      let overrides = run.overrides;
      for (const parent of nav.parents) overrides = withEntry(overrides, parent, collapse);
      return refold({ overrides }, false);
    }
    // --- the log pane ---------------------------------------------------------------
    //
    // `model::LogScroll`'s two methods, which is why "scrolling up drops follow; `f` and `G`
    // restore it" needs no special case: **up** resolves the current effective top first (so a
    // scroll-up from a followed view steps off the bottom rather than from row 0) and always
    // freezes, and **down** re-engages follow the moment it reaches `max_top`.
    case "output.log.up":
      return next({ log: logUp(run.log, metrics.logTotal, vp, 1) });
    case "output.log.down":
      return next({ log: logDown(run.log, metrics.logTotal, vp, 1) });
    case "output.log.half_up":
      return next({ log: logUp(run.log, metrics.logTotal, vp, halfPage(vp)) });
    case "output.log.half_down":
      return next({ log: logDown(run.log, metrics.logTotal, vp, halfPage(vp)) });
    case "output.log.page_up":
      return next({ log: logUp(run.log, metrics.logTotal, vp, fullPage(vp)) });
    case "output.log.page_down":
      return next({ log: logDown(run.log, metrics.logTotal, vp, fullPage(vp)) });
    case "output.log.top":
      return next({ log: 0 });
    case "output.log.bottom":
      return next({ log: "follow" });
    case "output.log.follow":
      // `LogScroll::toggle_follow` — engaging snaps to the bottom on the next paint (the
      // `"follow"` arm resolves there); disengaging freezes the current effective top, so the
      // view does not jump.
      return next({ log: run.log === "follow" ? logResolved(run.log, metrics.logTotal, vp) : "follow" });
    default:
      return captured;
  }
}

/// `LogScroll::top_line` — the effective top a `"follow" | number` scroll resolves to. The twin
/// of `layout.mjs`'s own `logTop`, restated here because the dispatch must resolve it *before*
/// the paint does and the two read one rule.
function logResolved(scroll, total, viewport) {
  const max = Math.max(0, total - Math.max(1, viewport));
  return scroll === "follow" ? max : Math.min(Math.max(0, scroll), max);
}

/// `LogScroll::scroll_up_by` — off the resolved top, clamped at 0, always **frozen**: the
/// reader took manual control of the viewport.
function logUp(scroll, total, viewport, n) {
  return Math.max(0, logResolved(scroll, total, viewport) - n);
}

/// `LogScroll::scroll_down_by` — off the resolved top, clamped at `max_top`, and **re-engaging
/// follow** once it gets there, so paging to the end resumes tailing.
function logDown(scroll, total, viewport, n) {
  const max = Math.max(0, total - Math.max(1, viewport));
  const at = Math.min(logResolved(scroll, total, viewport) + n, max);
  return at >= max ? "follow" : at;
}

/**
 * The info page scrolled by `lines` rows, clamped into `[0, max]` — the wheel's one effect.
 *
 * The `info` scope binds no nav key and this page invents none: a key not in afkd's table would
 * make `keymap.mjs`'s pinning of `DEFAULT_KEYS` a lie. The wheel is a browser affordance in the
 * same register as `RESERVED`/`pointerdown` focus, and costs no keymap row.
 */
export function scrollInfo(session, lines, max) {
  const ceiling = Math.max(0, Math.trunc(max));
  const at = Math.min(Math.max(0, Math.trunc(session.infoOffset + lines)), ceiling);
  return at === session.infoOffset ? session : { ...session, infoOffset: at };
}

/// Filter typing — `translate_filter_key`, the one **non-rebindable** surface: every printable
/// key is literal input, so it consults no keymap. A control-modified char is inert (so
/// `Ctrl+R` while typing never inserts an `r`), `Esc` clears and leaves, `Enter` confirms and
/// leaves, `Backspace` pops, anything else printable pushes.
function pressTyping(session, chord) {
  const needle = session.filter.needle;
  if (chord.ctrl) return { session, commands: [], handled: true };
  if (chord.key === "esc") {
    return { session: { ...session, filter: { mode: "off", needle: "" } }, commands: [], handled: true };
  }
  if (chord.key === "enter") {
    return {
      session: { ...session, filter: { mode: needle === "" ? "off" : "active", needle } },
      commands: [],
      handled: true,
    };
  }
  if (chord.key === "backspace") {
    return {
      session: { ...session, filter: { mode: "typing", needle: needle.slice(0, -1) } },
      commands: [],
      handled: true,
    };
  }
  if (chord.key.length === 1) {
    return {
      session: { ...session, filter: { mode: "typing", needle: needle + chord.key } },
      commands: [],
      handled: true,
    };
  }
  return { session, commands: [], handled: true };
}

/**
 * Press one chord against `board`, returning the next session, the command bodies the shell
 * should post on this tab's own attach, and whether the key was **handled** — which is what
 * decides `preventDefault`, so an unbound key keeps whatever the browser does with it.
 *
 * Precedence is `translate_key`'s, as early returns: the confirm modal, then the help overlay
 * (both capturing), then the info page, then the run view (capturing too), then filter typing,
 * then the list. Filter typing sits below both pages exactly as it does in the Rust — the filter
 * is a list-only affordance, so it can never be open at the same time as either.
 *
 * `options.run` is [`runMetrics`]' `{viewport, treeTotal, logTotal}` for the open run view, so a
 * scroll is clamped against the arithmetic that paints. Absent, every clamp floors — which is
 * the honest answer before the first paint has measured a viewport.
 */
export function press(session, board, chord, options) {
  const now = options.now;
  const height = Math.max(0, options.bodyHeight ?? 0);
  if (session.confirm !== null) return pressConfirm(session, chord, now);
  if (session.help) return pressHelp(session, chord);
  if (session.info !== null) return pressInfo(session, board, chord);
  if (session.run !== null) return pressRun(session, board, chord, options.run ?? NO_RUN_METRICS);
  if (session.filter.mode === "typing") return pressTyping(session, chord);

  const rows = rowsOf(session, board);
  const at = selectedIndex(session, rows);
  const row = at === null ? undefined : rows[at];
  const id = resolve(scopesAt(row), chord);
  if (id === null || !HANDLED.has(id) || refused(board, id)) {
    return { session, commands: [], handled: false };
  }
  // A key this page recognises but has nothing to do with *here* — the peek on a service row,
  // a fold key on a row with no group. It is reported **unhandled**, so the browser keeps
  // whatever it would have done with it: the page eats a chord only when it acted on one.
  const nothingToDo = { session, commands: [], handled: false };
  /// A cursor move, then the minimal scroll that brings it back into view.
  const moveTo = (next) => {
    const moved = pointAt(session, rows, next);
    return { session: scrolled(moved, rows, height), commands: [], handled: true };
  };
  /// The group path a fold key acts on: a header acts on itself, a member on its **parent** —
  /// `UiIntent::GroupCollapse`'s stated behaviour in both directions.
  const foldPath = () => {
    if (row === undefined) return null;
    if (row.kind === "group") return row.path;
    if (row.kind === "service" && row.svc.group !== "") return row.svc.group;
    return null;
  };
  /// A fold set rewritten, then the scroll re-clamped against the row set it produced.
  ///
  /// `onto` is the group header the cursor should **move** to — `collapse_selected_group` and
  /// `expand_selected_group` both do that, so a fold from inside a subtree leaves the cursor on
  /// the header it folded into, where the operator's eye already is. `null` leaves the cursor's
  /// *key* alone, which is what the bulk twins want: a member `H` folded away resolves straight
  /// back onto itself once `L` re-opens its header, because the cursor was never rewritten.
  const refold = (collapsed, onto) => {
    const next = { ...session, collapsed };
    const rebuilt = rowsOf(next, board);
    if (onto === null) return { session: scrolled(next, rebuilt, height), commands: [], handled: true };
    const to = rebuilt.findIndex((r) => {
      const key = rowKey(r);
      return key !== null && key.kind === "group" && key.key === onto;
    });
    const settled = pointAt(next, rebuilt, to === -1 ? next.cursorRow : to);
    return { session: scrolled(settled, rebuilt, height), commands: [], handled: true };
  };

  switch (id) {
    case "overview.up":
      return moveTo(previousSelectable(rows, at));
    case "overview.down":
      return moveTo(nextSelectable(rows, at));
    case "overview.first":
      return moveTo(nearestSelectable(rows, 0, true));
    case "overview.last":
      return moveTo(nearestSelectable(rows, rows.length - 1, false));
    case "overview.group_collapse":
    case "overview.service_peek": {
      // `Enter`/`Space` folds a **group** header; on a service row the terminal opens an
      // activity peek, which this page has no surface for, so it is inert there. `h`/`←` folds
      // the parent from a member too, which is the half that makes a deep tree navigable.
      if (id === "overview.service_peek" && row?.kind !== "group") return nothingToDo;
      if (id === "overview.service_peek" && row.collapsed) {
        const collapsed = new Set(session.collapsed);
        collapsed.delete(row.path);
        return refold(collapsed, row.path);
      }
      const path = foldPath();
      if (path === null) return nothingToDo;
      const collapsed = new Set(session.collapsed);
      collapsed.add(path);
      return refold(collapsed, path);
    }
    case "overview.group_expand": {
      // `expand_selected_group`, mirror for mirror: it resolves leaf→parent the same way and
      // **moves the cursor onto the header** either way — on an already-open parent that is all
      // it does, which is why `l` from a member is never a silent no-op.
      const path = foldPath();
      if (path === null) return nothingToDo;
      const collapsed = new Set(session.collapsed);
      collapsed.delete(path);
      return refold(collapsed, path);
    }
    case "overview.group_collapse_all": {
      const collapsed = new Set(
        rowsOf({ ...session, collapsed: new Set() }, board)
          .filter((r) => r.kind === "group")
          .map((r) => r.path),
      );
      return refold(collapsed, null);
    }
    case "overview.group_expand_all":
      return refold(new Set(), null);
    case "overview.filter":
      // `Filter::begin` — `/` starts an **empty** needle, so a second `/` over a confirmed one
      // is a fresh search rather than an edit of the last.
      return {
        session: { ...session, filter: { mode: "typing", needle: "" } },
        commands: [],
        handled: true,
      };
    case "overview.filter_clear":
      // `Esc` on a confirmed needle clears it; with no needle up there is nothing to clear, and
      // the key is inert rather than swallowed.
      if (session.filter.mode === "off") return { session, commands: [], handled: false };
      return {
        session: { ...session, filter: { mode: "off", needle: "" } },
        commands: [],
        handled: true,
      };
    case "overview.service_start":
    case "overview.service_fire":
    case "overview.service_restart": {
      const verb = { service_start: "start", service_fire: "fire", service_restart: "restart" }[
        id.slice("overview.".length)
      ];
      const out = verbFor(session, board, verb, row, now);
      return { ...out, handled: true };
    }
    case "overview.service_stop": {
      // The **second `x`**: a stop gesture whose selection holds ≥1 service already wedged in
      // `Stopping` is the force gate, not a plain stop — the first press already sent the
      // `Stop`. A mixed group captures only its `Stopping` members.
      const wedged = targetsOf(board, row).some((svc) => svc.badge === "Stopping");
      const out = verbFor(session, board, wedged ? "force" : "stop", row, now);
      return { ...out, handled: true };
    }
    case "overview.show_output":
      // `UiIntent::TreeOpen` opens on `selected_card()`, so a group header and a lane row have
      // no subject and the key is a no-op there — reported unhandled, exactly as `i` is.
      if (row?.kind !== "service") return nothingToDo;
      return { session: openRun(session, row.svc), commands: [], handled: true };
    case "overview.show_info":
      // `toggle_info_view` opens on `selected_card()`, so a group header and a lane row have no
      // subject and the key is a no-op there — reported unhandled, like every other row-shaped
      // refusal here, so the browser keeps whatever it would have done with an `i`.
      if (row?.kind !== "service") return nothingToDo;
      return { session: { ...session, info: row.svc.name, infoOffset: 0 }, commands: [], handled: true };
    case "global.reload":
      // The one global verb: no selection needed, and no press-time ack — the daemon's own
      // `meta.reloaded.message` owns that line.
      return { session, commands: [{ command: "reload" }], handled: true };
    case "global.help":
      return { session: { ...session, help: true }, commands: [], handled: true };
    default:
      return nothingToDo;
  }
}

/// The next selectable row below `at`, or `at` when there is none — the clamp, not a wrap.
function nextSelectable(rows, at) {
  if (at === null) return 0;
  for (let i = at + 1; i < rows.length; i += 1) {
    if (selectable(rows[i])) return i;
  }
  return at;
}

/// The next selectable row above `at`, or `at` when there is none.
function previousSelectable(rows, at) {
  if (at === null) return 0;
  for (let i = at - 1; i >= 0; i -= 1) {
    if (selectable(rows[i])) return i;
  }
  return at;
}

/// The session with its scroll top settled against the cursor — `layout.mjs`'s own minimal
/// clamp, imported rather than restated: the shell clamps through the same function when the
/// viewport resizes under a still cursor, and two copies of ratatui's three lines would be two
/// things to keep in step.
function scrolled(session, rows, height) {
  const at = selectedIndex(session, rows);
  if (at === null) return { ...session, offset: 0 };
  return { ...session, offset: scrollOffset(session.offset, at, rows.length, height) };
}

// --- the frames that are not board state ---------------------------------------------------

/**
 * Fold one stream frame's **flash** into `session`. `fold.mjs` will not fold these — they are
 * not board state, and it says so at the two sites — so the shell hands each frame here beside
 * `fold()`:
 *
 * - `meta.reloaded` → its `message`. The reload key posts no ack of its own precisely because
 *   this line is the daemon's, and `afkd top` pins that.
 * - `meta.error` → its `message`, which `top.rs` flashes too. Without this arm an asynchronous
 *   daemon refusal would reach the tab on the stream and vanish; the relay's own non-2xx covers
 *   only a socket write that failed, which is a different failure.
 * - **`meta.control_no_op` deliberately does not flash.** It exists to revert a pending
 *   optimistic edge; this page folds none, and the terminal posts no message for it either.
 *
 * `meta.quitting` is the one frame that moves the session *without* flashing:
 * `begin_quitting` drops the help overlay, the confirm modal and any filter typing, because
 * the drain frame owns the screen — and once quitting, keys must not route into the filter.
 *
 * Everything else returns the session unchanged.
 */
export function noteFrame(session, frame, now) {
  if (frame === null || typeof frame !== "object" || frame.type !== "meta") return session;
  if (frame.meta === "quitting") {
    return {
      ...session,
      help: false,
      confirm: null,
      filter: session.filter.mode === "typing" ? { mode: "off", needle: "" } : session.filter,
    };
  }
  if (frame.meta !== "reloaded" && frame.meta !== "error") return session;
  const message = typeof frame.message === "string" ? frame.message : "";
  return message === "" ? session : flashed(session, message, now);
}
