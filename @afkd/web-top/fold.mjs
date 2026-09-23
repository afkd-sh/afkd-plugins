// The fold: the daemon's control wire turned into the board `afkd top` draws.
//
// A `meta.snapshot` plus a stream of `event` / `log` / `trace` frames is not a dashboard.
// The dashboard is the **fold** of those frames into per-service state, and in afkd that
// fold lives in `crates/tui/src/model.rs` — a private crate the browser cannot reach. So
// it is re-stated here, in the plugin, arm for arm against the Rust it mirrors. Every arm
// cites the file it was read off, because the two can drift and the citation is the only
// thing that makes the drift findable.
//
// Two properties this module holds on purpose, and the reason for each:
//
// - **DOM-free and clock-free.** It imports nothing, touches neither `document` nor
//   `Date.now`, and takes `now` as an argument — exactly the way afkd's own core takes
//   timestamps as data (`crates/tui/src/lib.rs`: "Timestamps arrive as data (the shell
//   owns the clock)"). That is what lets it run under `node --test` against recorded wire
//   captures, and what will let a later card diff it against a real `afkd top`.
// - **Pure.** `fold(board, frame, now)` returns a new board and never writes through its
//   argument, so a caller may keep, compare or freeze any board it has been handed. The
//   tests deep-freeze theirs, which is the mechanical proof.
//
// `now` is **milliseconds on the caller's own monotonic clock** (`performance.now()` in the
// page, a plain counter in the tests). Every duration on the wire is relative and
// daemon-measured, so each is converted to an anchor in *that* domain once, at receipt —
// `nextFireAt = now + next_ms`, `lastActivityAt = now - last_activity_ms`. The daemon's
// clock is never a term in a subtraction.
//
// Unknown is ignored, never fatal: an unknown `type`, `event`, `meta` or `op` tag, an
// unknown field on a known frame and an event naming a service the board never saw all
// fold to nothing and leave the board standing (`docs/plugins.md`, the forward-compat
// rule). A newer daemon must not break this page.

/// How many log lines a service's ring retains before it drops its oldest.
/// Mirrors `LOG_RING_CAPACITY` (`crates/tui/src/logring.rs`).
export const LOG_LINES_DEFAULT = 2000;

/// How many trace nodes a service's run tree retains before it evicts its oldest
/// **completed** root. Mirrors `TRACE_TREE_CAPACITY` (`crates/tui/src/tracetree.rs`).
export const TRACE_NODES_CAPACITY = 2000;

/// How many of a served backfill tail's trailing lines a service's log seam remembers.
/// Mirrors `SEAM_GUARD_LINES` (`crates/tui/src/logring.rs`): the true overlap is only the
/// lines written in the `[attach, disk-read]` window, so this is orders of magnitude of
/// headroom while still bounding the worst-case over-drop a content key admits.
export const SEAM_GUARD_LINES = 256;

/// How many `meta.host_load` samples the load strip keeps. The daemon samples once a
/// second (ADR-0080), so this is a couple of minutes of trend — enough for the lanes a
/// later card draws, bounded so a tab left open overnight is not a memory leak.
export const LOAD_HISTORY_CAPACITY = 120;

/// The coarse wire state word → the badge `afkd top` renders. The mirror of
/// `top.rs::badge_from_state`, including its `_ => Idle` fallback: an unknown word reads
/// `Idle` on the badge while the service's `state` still says verbatim what the daemon
/// said. Note `armed → Idle` — the one asymmetry, and a deliberate one.
const BADGE_BY_STATE = {
  starting: "Starting",
  armed: "Idle",
  busy: "Busy",
  queued: "Queued",
  checking: "Checking",
  stopping: "Stopping",
  stopped: "Stopped",
  crashed: "Crashed",
};

// --- the board ---------------------------------------------------------------------

/**
 * An empty board. `options.logLines` sets the per-service ring capacity (the plugin's
 * `log_lines` setting); the module itself is setting-blind, so the caller reads the
 * number off the wire and hands it in here.
 */
export function seed(options = {}) {
  const logLines = Math.max(1, Math.trunc(toNumber(options.logLines, LOG_LINES_DEFAULT)));
  return {
    logLines,
    // The watched daemon's boot and drain edge, both anchored on the caller's clock.
    daemonStartedAt: null,
    quittingSince: null,
    // Every service name in the daemon's own order; `services` is keyed by the same names.
    order: [],
    services: {},
    load: { at: null, cpuPct: null, memUsed: null, memTotal: null, rxBps: null, txBps: null, history: [] },
    // What the fold has seen. `ignored` counts the frames whose `type`/`event`/`meta`/`op`
    // tag this module does not know — the forward-compat path, which is a count and never
    // a fault.
    frames: { total: 0, folded: 0, ignored: 0 },
  };
}

/**
 * Fold one wire frame into `board` as of `now`, returning the new board. Pure: `board` is
 * never written through, and an unfoldable frame returns a board equal to the one that
 * went in but for its `frames` tally.
 */
export function fold(board, frame, now) {
  const counted = {
    ...board,
    frames: { total: board.frames.total + 1, folded: board.frames.folded, ignored: board.frames.ignored },
  };
  if (!isObject(frame)) return ignore(counted);
  switch (frame.type) {
    case "event":
      return foldEvent(counted, frame, now);
    case "log":
      return foldLog(counted, frame, now);
    case "trace":
      return foldTrace(counted, frame, now, true);
    case "backfill":
      // A **replayed** tree event — the same payload `trace` carries, folded the same way
      // but **without** the last-activity stamp (`proto.rs`, `Frame::Backfill`: a replay is
      // history, not output, so the snapshot's `last_activity_ms` stays authoritative).
      // `model.rs::apply_replayed_trace` is the same one-flag split.
      return foldTrace(counted, frame, now, false);
    case "backfill_log":
      return foldBackfillLog(counted, frame, now);
    case "meta":
      return foldMeta(counted, frame, now);
    default:
      // `command`/`view` (inbound, never sent to a subscriber) and whatever a newer daemon
      // adds.
      return ignore(counted);
  }
}

/// Mark the just-counted frame as one this fold has no arm for. The connection stays up;
/// that is the whole of the §3 skew rule on the client side.
function ignore(board) {
  return { ...board, frames: { ...board.frames, ignored: board.frames.ignored + 1 } };
}

/// Mark the just-counted frame as understood, whether or not it moved anything: an event
/// naming a service the board never saw is folded (the tui's own rule is to drop it), not
/// ignored, because the vocabulary was known.
function folded(board) {
  return { ...board, frames: { ...board.frames, folded: board.frames.folded + 1 } };
}

/**
 * A new board with `name`'s record replaced by what `update` returns. `update` is handed a
 * **shallow copy** it may write to freely; any sub-object it means to change (the ring, the
 * node map) it must replace rather than mutate, since those are still shared with the board
 * that went in. A name the board does not hold is the tui's own no-op.
 */
function withService(board, name, update) {
  const current = board.services[name];
  if (current === undefined) return board;
  const next = update({ ...current });
  return { ...board, services: { ...board.services, [name]: next } };
}

// --- the snapshot seed -------------------------------------------------------------

/**
 * One service record seeded from its wire `ServiceState` (`crates/app/src/proto.rs`), the
 * mirror of `top.rs::seed_from_snapshot`. The badge is set **directly** rather than through
 * `enter`, which would clear the anchors the seed just computed; the tree and the ring start
 * **empty** and are never transplanted from a previous record, which is
 * `top.rs::seed_from_snapshot_cards_start_with_an_empty_tree`'s pinned choice.
 */
function seedService(entry, now) {
  const state = toText(entry.state);
  const badge = badgeFromState(state);
  const busyMs = toNumber(entry.busy_ms, 0);
  return {
    name: toText(entry.name),
    // The wire's word, verbatim and undegraded — including a word this fold does not know,
    // which is what lets a reader see daemon/client skew at all.
    state,
    badge,
    triggerLabel: toText(entry.trigger_label),
    triggerDetail: toText(entry.trigger_detail),
    triggerFields: Array.isArray(entry.trigger_fields)
      ? entry.trigger_fields.map((pair) => [toText(pair?.[0]), toText(pair?.[1])])
      : [],
    confined: entry.confined === true,
    group: toText(entry.group),
    icon: toText(entry.icon),
    description: toText(entry.description),
    queue: toText(entry.queue),
    queuePriority: toText(entry.queue_priority),
    // Absent means the daemon sent none (a lane-less service, or a husk that learned its
    // lane from a live event); `0` is a real width and must not read as absence.
    queueParallelism: entry.queue_parallelism === undefined || entry.queue_parallelism === null
      ? null
      : toNumber(entry.queue_parallelism, 0),
    orphan: entry.orphan === true,
    stale: entry.stale === true,
    poisoned: entry.poisoned === true,
    isPoller: entry.is_poller === true,
    // The four anchors, each re-based onto the caller's clock. A `state_ms` the daemon
    // omitted degrades to the attach instant, which is `DashboardModel::seed`'s fallback.
    stateEnteredAt: anchorPast(now, entry.state_ms) ?? now,
    nextFireAt: anchorFuture(now, entry.next_ms),
    lastActivityAt: anchorPast(now, entry.last_activity_ms),
    // A fire already running at attach is back-dated to its real start, so its elapsed
    // counts from when the fire began and not from when this page connected. An older
    // daemon sends no `busy_ms`; a `busy` badge then falls back to the attach instant.
    inFlightSince: busyMs > 0 ? now - busyMs : badge === "Busy" ? now : null,
    breadcrumb: "",
    lastError: null,
    // The two per-arm latches the event fold needs (`CardModel`, `crates/tui/src/model.rs`).
    crashCounted: false,
    faultedSinceArm: false,
    // The daemon's own run history and session usage, so a fresh attach reads real figures
    // rather than the zeros a client-session fold starts at. The live arms add on top.
    runs: toNumber(entry.runs_total, 0),
    failures: toNumber(entry.run_failures_total, 0),
    errors: 0,
    runTimeTotalMs: toNumber(entry.run_time_total_ms, 0),
    okRunTimeTotalMs: toNumber(entry.ok_run_time_total_ms, 0),
    tokens: toNumber(entry.tokens, 0),
    cost: toNumber(entry.cost, 0),
    turns: toNumber(entry.turns, 0),
    log: { lines: [], dropped: 0 },
    // The ring's idempotent key (`LogSeam`, `crates/tui/src/logring.rs`): `null` is
    // `Disarmed`, an array is `Armed { expect }`. Armed only by a **busy** service's served
    // backfill, whose in-progress run re-streams its trailing lines live.
    logSeam: null,
    // The served tail accumulating for that arm (`CardModel::served_log_backfill`), drained
    // on the burst's `last` line — push-then-arm, so the burst never filters itself.
    servedLog: [],
    tree: emptyTree(),
  };
}

/// The wire's coarse state word as a badge. `top.rs::badge_from_state`.
function badgeFromState(state) {
  const badge = BADGE_BY_STATE[state];
  return badge === undefined ? "Idle" : badge;
}

/**
 * Set `service`'s badge, stamping `stateEnteredAt` **only when the badge actually changes**
 * — `CardModel::enter` (`crates/tui/src/model.rs`). Re-applying the same badge is a no-op,
 * so a duplicate frame can never spuriously reset the dwell. A real transition invalidates
 * the seeded next-fire deadline (it was only authoritative for the state it was snapshotted
 * in, ADR-0070), so `enter` clears it.
 */
function enter(service, badge, now) {
  if (service.badge !== badge) {
    service.badge = badge;
    service.stateEnteredAt = now;
    service.nextFireAt = null;
  }
}

/**
 * Settle the in-flight fire — `DashboardModel::complete_fire`, guard asymmetry included.
 * The run **counts** bump in the caller, outside the guard; the two wall-time totals fold
 * **inside** it, so a stray completion with no fire in flight adds no time. The in-flight
 * marker clears either way.
 */
function completeFire(service, outcome, elapsedMs) {
  if (service.inFlightSince !== null) {
    service.runTimeTotalMs += elapsedMs;
    if (outcome === "ok") service.okRunTimeTotalMs += elapsedMs;
  }
  service.inFlightSince = null;
}

// --- events ------------------------------------------------------------------------

/**
 * One arm per `Event` variant (`crates/app/src/proto.rs`), folding as
 * `DashboardModel::apply` does. An unknown tag takes the ignore path; a known tag naming a
 * service the board never saw folds to nothing.
 */
function foldEvent(board, frame, now) {
  const name = toText(frame.service);
  switch (frame.event) {
    case "service_armed":
      return folded(
        withService(board, name, (svc) => {
          // A re-arm adopts any staged recipe, so the drift is gone; and a fresh instance
          // re-opens both per-arm latches.
          svc.stale = false;
          svc.faultedSinceArm = false;
          svc.crashCounted = false;
          // Settle `Idle` — but never downgrade a `Busy` card, whose fire settles itself.
          if (svc.badge !== "Busy") enter(svc, "Idle", now);
          return svc;
        }),
      );
    case "service_paused":
      return folded(
        withService(board, name, (svc) => {
          if (frame.paused === true) {
            // The force-abandon reclaim: the daemon sends this once the abandoned thread
            // finally joins, so the service is re-armable again. Cleared **outside** the
            // badge compare — `enter` is idempotent on an unchanged badge, but this write
            // is what makes the entry readable as re-armable and must land either way.
            svc.poisoned = false;
            enter(svc, "Stopped", now);
          } else {
            // The resume mirror, and it lands `Starting`, not `Idle`: the daemon emits it
            // before the fresh thread runs its `init`, and `service_armed` is what settles
            // it. Re-open the crash count, since an `init` that faults again never reaches
            // armed and would otherwise go uncounted.
            svc.crashCounted = false;
            enter(svc, "Starting", now);
          }
          return svc;
        }),
      );
    case "service_stopping":
      // The engine-transcribed drain (`ServiceEvent::Draining`): show `stopping` until the
      // settling `service_paused {paused:true}` lands it `stopped`.
      return folded(withService(board, name, (svc) => (enter(svc, "Stopping", now), svc)));
    case "service_queued":
      return folded(
        withService(board, name, (svc) => {
          // The lane the daemon named at the instant of the wait — idempotent for a service
          // the snapshot already seeded, and the only way a husk entry learns its lane. The
          // **level** is not learned here: it is config metadata, and a husk has none.
          svc.queue = toText(frame.queue);
          // …and deliberately nothing else: `inFlightSince` stays put, so a service parked
          // at a lane's door is never counted as burning anything.
          enter(svc, "Queued", now);
          return svc;
        }),
      );
    case "service_checking":
      // The beat holds its lane and is asking its vendor for work. Like the wait above, it
      // burns no fire clock.
      return folded(withService(board, name, (svc) => (enter(svc, "Checking", now), svc)));
    case "service_check_done":
      return folded(
        withService(board, name, (svc) => {
          // A **compare-and-set**: this close can land after whatever the check started, so
          // it may only move the service it still owns — the empty poll's.
          if (svc.badge === "Checking") enter(svc, "Idle", now);
          return svc;
        }),
      );
    case "fire_started":
      return folded(
        withService(board, name, (svc) => {
          enter(svc, "Busy", now);
          svc.inFlightSince = now;
          svc.breadcrumb = "";
          return svc;
        }),
      );
    case "step_entered":
      return folded(
        withService(board, name, (svc) => {
          svc.breadcrumb = toText(frame.breadcrumb);
          return svc;
        }),
      );
    case "fire_ok":
      return folded(
        withService(board, name, (svc) => {
          svc.runs += 1;
          completeFire(svc, "ok", toNumber(frame.elapsed_ms, 0));
          // Settle back to `Idle` — but only from `Busy`. A control stop that already moved
          // the service to `Stopping` mid-fire leaves it draining.
          if (svc.badge === "Busy") enter(svc, "Idle", now);
          return svc;
        }),
      );
    case "fire_failed":
      return folded(
        withService(board, name, (svc) => {
          // A **transient** fire fault (ADR-0030): it does not change service state. The
          // service stays armed and returns to `Idle`; the fault surfaces as the tallies
          // and the last-error line.
          const reason = toText(frame.reason);
          svc.runs += 1;
          svc.failures += 1;
          svc.faultedSinceArm = true;
          completeFire(svc, "failed", toNumber(frame.elapsed_ms, 0));
          if (svc.badge === "Busy") enter(svc, "Idle", now);
          svc.breadcrumb = reason;
          svc.lastError = reason;
          return svc;
        }),
      );
    case "agent_finished":
      return folded(
        withService(board, name, (svc) => {
          // Only the values the provider's envelope confirmed, so an unconfirmed field adds
          // nothing rather than a fabricated 0. These add *on top of* the snapshot baseline:
          // the daemon broadcasts a finish only after a client registered, so no double count.
          if (frame.tokens !== undefined && frame.tokens !== null) svc.tokens += toNumber(frame.tokens, 0);
          if (frame.turns !== undefined && frame.turns !== null) svc.turns += toNumber(frame.turns, 0);
          if (frame.cost !== undefined && frame.cost !== null) svc.cost += toNumber(frame.cost, 0);
          return svc;
        }),
      );
    case "service_error":
      return folded(
        withService(board, name, (svc) => {
          // A swallowed trigger diagnostic — not a fire and not a crash, so the run tallies
          // and the state are untouched.
          const reason = toText(frame.reason);
          svc.errors += 1;
          svc.breadcrumb = reason;
          svc.lastError = reason;
          return svc;
        }),
      );
    case "service_crashed":
      return folded(
        withService(board, name, (svc) => {
          // An **involuntary** exit, and unlike a transient fault this does change state.
          // The error is counted **once per alarm**: the daemon announces the same crash a
          // second time when it parks the crashed thread.
          const reason = toText(frame.reason);
          if (!svc.crashCounted) {
            svc.errors += 1;
            svc.crashCounted = true;
          }
          enter(svc, "Crashed", now);
          svc.inFlightSince = null;
          // Set-only within the crash fold: a plain init-fault crash says nothing about a
          // leaked thread, so it must not clear a poison. Only the reclaim above lifts it.
          if (frame.poisoned === true) svc.poisoned = true;
          svc.breadcrumb = reason;
          svc.lastError = reason;
          return svc;
        }),
      );
    case "service_next_fire":
      return folded(
        withService(board, name, (svc) => {
          // A direct field stamp, never a transition: re-anchoring a countdown is not a
          // state change, and routing it through `enter` would clear the very field it sets.
          svc.nextFireAt = anchorFuture(now, frame.next_ms);
          return svc;
        }),
      );
    default:
      return ignore(board);
  }
}

// --- logs --------------------------------------------------------------------------

/**
 * One captured child line into its service's bounded ring. The wire carries the line
 * **un-sanitized** — stripping is named a client-fold concern (`proto.rs`, `Log`) — and the
 * browser is the client, so it is sanitized here.
 */
function foldLog(board, frame, now) {
  return folded(
    withService(board, toText(frame.service), (svc) => {
      applyLog(
        svc,
        {
          // An unrecognised future stream value is kept verbatim rather than dropped.
          stream: toText(frame.stream),
          text: sanitize(toText(frame.line)),
          node: frame.node === undefined || frame.node === null ? null : toNumber(frame.node, 0),
        },
        board.logLines,
        now,
      );
      return svc;
    }),
  );
}

/**
 * `DashboardModel::apply_log`'s body, written through `svc` (the shallow copy `withService`
 * handed its caller): the seam gate, the bounded push, and the activity stamp.
 *
 * The **one** push both log arms run, which is `apply_served_log`'s own reuse of `apply_log`
 * — a served tail line and a live one land in the same ring, under the same bound and the
 * same gate, so the two can never drift on how a line is stored. `entry` carries the ring's
 * three fields; the fourth, `at`, is this page's own monotonic instant, stamped here.
 */
function applyLog(svc, entry, capacity, now) {
  const { seam, admitted } = admit(svc.logSeam, entry.text);
  svc.logSeam = seam;
  // The early return at `model.rs`'s seam check: a line the backfill already pushed is this
  // line's live replay across the seam, dropped whole — the activity stamp included.
  if (!admitted) return;
  const lines = svc.log.lines.slice();
  let dropped = svc.log.dropped;
  lines.push({ ...entry, at: now });
  while (lines.length > capacity) {
    lines.shift();
    dropped += 1;
  }
  svc.log = { lines, dropped };
  // A log line emitted **from a run** is observable activity. Output with no fire in
  // flight (the boot lifecycle's narration, `init`/`cleanup`) is not a run, so it does
  // not move the column — the `apply_log` gate.
  if (svc.inFlightSince !== null) svc.lastActivityAt = now;
}

/**
 * One **served** `run.log` tail line into the same ring the live `log` frames feed — the
 * mirror of `DashboardModel::apply_served_log` (`crates/tui/src/model.rs`).
 *
 * Three things it does that the live arm does not, each of them the Rust's:
 *
 * - the text is pushed **verbatim**. `Frame::BackfillLog`'s `line` is already the sanitized
 *   ring text (the producer ran the same transform this module's [`sanitize`] is), unlike
 *   live `Log`, whose line is raw and stripped on fold. Sanitizing twice would be a second
 *   pass over text that is already clean — and the seam keys on content, so the two spellings
 *   have to be one;
 * - `stream`/`node` are fixed to `stdout`/`null`, because a `run.log` is a flat transcript
 *   that records neither;
 * - the frame's civil `stamp` is **dropped on the floor, deliberately**. The log pane renders
 *   no timestamp column and no day marker at all (`layout.mjs`'s log pane, because a live
 *   `Frame::Log` carries no stamp and the entry is stamped with this page's own monotonic
 *   clock), so a civil stamp has nowhere to go here and a ring holding one entry stamped
 *   differently from every other would be the drift, not the fix.
 *
 * A **busy** service's served texts accumulate, and the `last` line drains them and arms the
 * seam — push-then-arm, so the burst never filters itself. A not-busy service's `run.log` is
 * final and can never re-stream, so it arms nothing.
 */
function foldBackfillLog(board, frame, now) {
  const busy = frame.busy === true;
  return folded(
    withService(board, toText(frame.service), (svc) => {
      const text = toText(frame.line);
      applyLog(svc, { stream: "stdout", text, node: null }, board.logLines, now);
      if (busy) svc.servedLog = svc.servedLog.concat([text]);
      if (frame.last === true) {
        const texts = svc.servedLog;
        svc.servedLog = [];
        if (busy) svc.logSeam = armSeam(texts);
      }
      return svc;
    }),
  );
}

/// Arm a seam with `texts`, the tail a backfill just pushed oldest → newest — `LogSeam::arm`.
/// Only the newest [`SEAM_GUARD_LINES`] are remembered, and an empty tail leaves the seam
/// disarmed: nothing was backfilled, so nothing can be a duplicate.
function armSeam(texts) {
  const expect = texts.slice(Math.max(0, texts.length - SEAM_GUARD_LINES));
  return expect.length === 0 ? null : expect;
}

/**
 * Whether `text` should be folded into the ring, and the seam that remains — `LogSeam::admit`
 * (`crates/tui/src/logring.rs`), pure like everything else here, so the caller writes the new
 * seam back onto its own service copy.
 *
 * A hit drops the line **and every remembered line before it**: the overlap is contiguous, so
 * anything earlier can no longer arrive. A miss is the first genuinely new line, which disarms
 * the guard for good — at most the overlap window is ever filtered.
 */
function admit(seam, text) {
  if (seam === null) return { seam, admitted: true };
  const at = seam.indexOf(text);
  if (at === -1) return { seam: null, admitted: true };
  const rest = seam.slice(at + 1);
  return { seam: rest.length === 0 ? null : rest, admitted: false };
}

/**
 * Turn one logical log line into printable text, the mirror of
 * `sanitize_log_line` (`crates/engine/src/logtext.rs`). A log line is not always afkd's own
 * text — a child prints whatever it prints — so its bytes must never be a write primitive
 * on the reader's screen:
 *
 * - an `ESC [` opens a CSI, dropped through its final byte in `@`..=`~` (an unterminated
 *   one to end of line); any other `ESC` drops just the escape byte, leaving its payload as
 *   inert printable residue;
 * - a tab becomes one space;
 * - every other C0 control and DEL is dropped — which is where an embedded `\r` is
 *   neutralised in place, so `foo\rbar` reads `foobar`;
 * - Unicode format characters (`\p{Cf}`: the bidi overrides, the zero-width set, the BOM)
 *   are dropped, since a line carrying them can display as text that reads differently from
 *   what it is;
 * - everything else, all printable Unicode included, passes through unchanged.
 */
export function sanitize(line) {
  // By code point, not by UTF-16 unit, so an astral character (an emoji) is never split.
  const chars = Array.from(line);
  let out = "";
  for (let i = 0; i < chars.length; i += 1) {
    const ch = chars[i];
    if (ch === "\x1b") {
      if (chars[i + 1] === "[") {
        i += 1;
        while (i + 1 < chars.length) {
          i += 1;
          const code = chars[i].codePointAt(0);
          if (code >= 0x40 && code <= 0x7e) break;
        }
      }
      continue;
    }
    if (ch === "\t") {
      out += " ";
      continue;
    }
    const code = ch.codePointAt(0);
    if (code < 0x20 || code === 0x7f || FORMAT_CHAR.test(ch)) continue;
    out += ch;
  }
  return out;
}

/// `General_Category=Cf`, read off the engine's regex property rather than the hand-rolled
/// range table `logtext.rs` carries (which exists only because `afkd-engine` may not take a
/// unicode dependency). The property escape is the more faithful of the two.
const FORMAT_CHAR = /\p{Cf}/u;

// --- the run tree ------------------------------------------------------------------

/// An empty per-service run tree. `newest` is the greatest id ever **opened** into this
/// tree — the frontier the generation guard reads — and is `null` on an empty or just-reset
/// one. It stands in for Rust's `TraceTree::newest()`, which is the greatest **retained**
/// id: the two agree here (eviction drops whole oldest *completed* roots, so the max-id
/// node is never the one evicted) and where they could differ, the carried frontier is the
/// stricter, which is the safer side for a guard.
function emptyTree() {
  return { nodes: {}, roots: [], newest: null };
}

/**
 * One `trace` frame into its service's run tree — `crates/tui/src/tracetree.rs`, whose four
 * loss-reconciliation rules this mirrors whole. Trace frames take the **lossy** prune class
 * (ADR-0068), so the fold must tolerate every gap and none of them may fault.
 *
 * `stamp` is `model.rs::fold_trace`'s own flag, and the only difference between a live
 * `trace` and a replayed `backfill`: the tree fold is identical, but a replay of minutes-old
 * history is not output *now*, so it must not move the list's last-activity anchor off the
 * authoritative value the snapshot seeded.
 */
function foldTrace(board, frame, now, stamp) {
  const event = frame.event;
  if (!isObject(event)) return ignore(board);
  const op = event.op;
  if (op !== "opened" && op !== "closed" && op !== "relabeled") return ignore(board);
  return folded(
    withService(board, toText(frame.service), (svc) => {
      const before = svc.tree;
      if (op === "opened") svc.tree = openNode(before, event.node, event.at, now);
      else if (op === "closed") svc.tree = closeNode(before, event);
      else svc.tree = relabelNode(before, event);
      // A trace event from a run is observable activity, on the same gate the ring uses.
      if (stamp && svc.tree !== before && svc.inFlightSince !== null) svc.lastActivityAt = now;
      return svc;
    }),
  );
}

/**
 * Fold an `Opened`. The order of the four decisions below is `TraceTree::open`'s order and
 * must stay so:
 *
 * 1. **the identical-replay dedup**, which returns *first* — so a replayed root does not
 *    spuriously abandon still-open siblings, and does not trip the generation guard below
 *    it (`tracetree.rs`'s own comment: "The check runs **before** rule 4");
 * 2. **the two resets** — a same id whose shape differs (the daemon restarted and re-minted
 *    its counter into ids we still hold), and a **regressed root** (a declared root at or
 *    below the frontier, which within one generation is impossible, so it can only be a
 *    fresh allocator). Either clears the tree and rebuilds from the incoming node;
 * 3. **rule 4** — a new root means the previous fire is over, so any node still running was
 *    abandoned mid-flight and settles `unknown`;
 * 4. **rule 2** — a declared parent we never saw makes this an orphan, re-rooted rather
 *    than lost.
 *
 * `now` is the caller's own monotonic instant, stamped onto the node as
 * [`openedAt`](#openedAt) beside the wire's civil `at` — the anchor the run view's ticking
 * `TOOK` measures against. It is **not** part of the dedup (see the field's own note).
 */
function openNode(tree, wire, at, now) {
  if (!isObject(wire)) return tree;
  const id = toNumber(wire.id, 0);
  const declaredParent = wire.parent === undefined || wire.parent === null ? null : toNumber(wire.parent, 0);
  const kind = toText(wire.kind);
  const label = toText(wire.label);
  const detail = toText(wire.detail);
  const stamp = isObject(at) ? { ...at } : null;

  const existing = tree.nodes[id];
  if (existing !== undefined) {
    // Resolve the incoming's declared parent the way the insert below will (rule 2), so an
    // orphan replayed identically still compares equal.
    const incomingParent = declaredParent !== null && tree.nodes[declaredParent] !== undefined ? declaredParent : null;
    if (
      existing.parent === incomingParent &&
      existing.kind === kind &&
      existing.label === label &&
      existing.detail === detail &&
      sameStamp(existing.at, stamp)
    ) {
      return tree;
    }
  }

  const regressedRoot = declaredParent === null && tree.newest !== null && id <= tree.newest;
  let nodes;
  let roots;
  let newest;
  if (regressedRoot || existing !== undefined) {
    nodes = {};
    roots = [];
    newest = null;
  } else {
    nodes = { ...tree.nodes };
    roots = tree.roots.slice();
    newest = tree.newest;
  }

  // Rule 4: runs are sequential, so anything still running when a fresh root opens lost its
  // close.
  if (declaredParent === null) {
    for (const key of Object.keys(nodes)) {
      if (nodes[key].status === "running") nodes[key] = { ...nodes[key], status: "unknown" };
    }
  }

  // Rule 2: a declared parent whose `Opened` was dropped makes this an orphan root.
  const parent = declaredParent !== null && nodes[declaredParent] !== undefined ? declaredParent : null;
  if (parent === null) roots.push(id);
  else nodes[parent] = { ...nodes[parent], children: nodes[parent].children.concat([id]) };

  nodes[id] = {
    id,
    parent,
    kind,
    label,
    // An open node's live headline, overlaid by a `relabeled` and left `null` otherwise —
    // the as-opened `label` is deliberately never touched, since it is what the dedup above
    // compares.
    relabel: null,
    detail,
    at: stamp,
    // The **monotonic** instant this frame folded, beside the wire's civil `at`. The run
    // view's ticking `TOOK` is `now - openedAt`: `treeview::took_content` measures a running
    // node as `civil_delta_ms(now, at)` against a civil clock the browser does not have, and
    // the one it does have is this one. A closed node still reads its authoritative
    // `elapsedMs`, so only the live estimate rides this field — and the dedup above
    // deliberately does **not** compare it, since a replayed `opened` arrives at a different
    // `now` and would otherwise reset the tree.
    openedAt: now,
    children: [],
    status: "running",
    elapsedMs: 0,
    out: null,
    cost: null,
    turns: null,
    tokens: null,
  };
  newest = newest === null || id > newest ? id : newest;
  return evictToCapacity({ nodes, roots, newest });
}

/**
 * Fold a `Closed`: a close for an id we never opened is dropped (rule 1); otherwise every
 * still-open descendant is abandoned `unknown` (rule 3, a lost child close) and the node
 * itself takes its terminal outcome.
 */
function closeNode(tree, event) {
  const id = toNumber(event.id, 0);
  if (tree.nodes[id] === undefined) return tree;
  const nodes = { ...tree.nodes };
  // Rule 3, iterative so a deep tree never overflows the stack.
  const stack = nodes[id].children.slice();
  while (stack.length > 0) {
    const childId = stack.pop();
    const child = nodes[childId];
    if (child === undefined) continue;
    if (child.status === "running") nodes[childId] = { ...child, status: "unknown" };
    stack.push(...child.children);
  }
  nodes[id] = {
    ...nodes[id],
    status: toText(event.status) || "unknown",
    elapsedMs: toNumber(event.elapsed_ms, 0),
    out: isObject(event.out) ? { ...event.out } : null,
    cost: event.cost === undefined || event.cost === null ? null : toNumber(event.cost, 0),
    turns: event.turns === undefined || event.turns === null ? null : toNumber(event.turns, 0),
    tokens: event.tokens === undefined || event.tokens === null ? null : toNumber(event.tokens, 0),
  };
  return { ...tree, nodes };
}

/// Fold a `Relabeled`: overlay an open node's live headline. A relabel for an id we never
/// opened is dropped, rule 1's posture.
function relabelNode(tree, event) {
  const id = toNumber(event.id, 0);
  if (tree.nodes[id] === undefined) return tree;
  return { ...tree, nodes: { ...tree.nodes, [id]: { ...tree.nodes[id], relabel: toText(event.label) } } };
}

/**
 * Enforce the node bound: while over capacity, drop the **oldest completed root** and its
 * whole subtree. A run at a time — a live root is never evicted and no subtree is half
 * removed, so the tree may transiently exceed the cap rather than show a partial run.
 */
function evictToCapacity(tree) {
  let { nodes, roots } = tree;
  while (Object.keys(nodes).length > TRACE_NODES_CAPACITY) {
    const pos = roots.findIndex((root) => nodes[root] !== undefined && nodes[root].status !== "running");
    if (pos === -1) break; // no completed root to reclaim — never evict the live run
    const stack = [roots[pos]];
    roots = roots.slice(0, pos).concat(roots.slice(pos + 1));
    nodes = { ...nodes };
    while (stack.length > 0) {
      const id = stack.pop();
      const node = nodes[id];
      if (node === undefined) continue;
      delete nodes[id];
      stack.push(...node.children);
    }
  }
  return { ...tree, nodes, roots };
}

/// Whether two `Opened` stamps are the same civil instant — the dedup's discriminant, and
/// what tells a genuine re-mint (seconds apart) from a replay of one emit.
function sameStamp(a, b) {
  if (a === null || b === null) return a === b;
  return (
    a.year === b.year &&
    a.month === b.month &&
    a.day === b.day &&
    a.hour === b.hour &&
    a.minute === b.minute &&
    a.second === b.second &&
    a.millis === b.millis
  );
}

// --- meta --------------------------------------------------------------------------

/// The `Meta` variants that move the board. The rest — `spawned`, `control_no_op`,
/// `status`/`runs`/`doctor`/`validate`, `queue_parallelism`, `error` — take the ignore path.
function foldMeta(board, frame, now) {
  switch (frame.meta) {
    case "snapshot": {
      // The seed **replaces** the board: every field re-derived, a service the new snapshot
      // does not name dropped rather than kept stale, and each record's tree and ring reset
      // to empty rather than carried over. That last one is `top.rs`'s pinned choice — "a
      // reconnect starts the tree fresh and fills forward, like the per-card log ring (never
      // transplanted)" — and it is also what keeps the generation guard's premise (ids
      // monotonic within a generation) true across a reconnect.
      const entries = Array.isArray(frame.services) ? frame.services : [];
      const services = {};
      const order = [];
      for (const entry of entries) {
        if (!isObject(entry)) continue;
        const name = toText(entry.name);
        if (name === "") continue;
        services[name] = seedService(entry, now);
        order.push(name);
      }
      return folded({
        ...board,
        services,
        order,
        daemonStartedAt: now - toNumber(frame.uptime_ms, 0),
        // Presence *is* the drain: a client attaching after the edge seeds `quitting`,
        // back-dated so it reads the drain's true age and not this connection's.
        quittingSince: anchorPast(now, frame.quitting_ms),
      });
    }
    case "quitting":
      // The live twin of the snapshot's back-dated anchor, broadcast once per drain. No
      // service record moves: the per-service stop edges arrive as their own events.
      return folded({ ...board, quittingSince: now });
    case "dropped":
      // One card pulled: an orphan reached `Stopped`, so it is now neither configured nor
      // running. The same fold a reload's `dropped` verdict takes, one name at a time.
      return folded(dropService(board, toText(frame.service)));
    case "host_load": {
      const sample = {
        at: now,
        cpuPct: toNumber(frame.cpu_pct, 0),
        memUsed: toNumber(frame.mem_used, 0),
        memTotal: toNumber(frame.mem_total, 0),
        rxBps: toNumber(frame.rx_bps, 0),
        txBps: toNumber(frame.tx_bps, 0),
      };
      const history = board.load.history.concat([sample]);
      return folded({
        ...board,
        load: { ...sample, history: history.slice(Math.max(0, history.length - LOAD_HISTORY_CAPACITY)) },
      });
    }
    case "reloaded": {
      // The whole reconcile verdict, not only the orphan/stale markers: an `added` service
      // that never appeared, or a `changed` one frozen on its boot-time seed, would leave the
      // board knowingly wrong the moment the operator reloads. `message` and `outcome` are
      // deliberately **not** folded — they are the footer flash `Ctrl+R` renders, not board
      // state, and this module paints nothing.
      let next = board;
      for (const entry of asArray(frame.added)) {
        if (!isObject(entry)) continue;
        const name = toText(entry.name);
        if (name === "") continue;
        next = {
          ...next,
          services: { ...next.services, [name]: seedService(entry, now) },
          order: next.order.includes(name) ? next.order : next.order.concat([name]),
        };
      }
      for (const entry of asArray(frame.changed)) {
        if (!isObject(entry)) continue;
        // Only the config-derived metadata: the badge, ring, tree, timing and in-flight
        // marker are the *live* state and survive a reload untouched.
        next = withService(next, toText(entry.name), (svc) => {
          const fresh = seedService(entry, now);
          for (const key of RELOAD_METADATA_KEYS) svc[key] = fresh[key];
          return svc;
        });
      }
      for (const name of asArray(frame.dropped)) next = dropService(next, toText(name));
      for (const name of asArray(frame.orphaned)) next = setFlag(next, toText(name), "orphan", true);
      for (const name of asArray(frame.adopted)) next = setFlag(next, toText(name), "orphan", false);
      for (const name of asArray(frame.stale)) next = setFlag(next, toText(name), "stale", true);
      const order = asArray(frame.order).map(toText).filter((name) => next.services[name] !== undefined);
      // The daemon is the single source of truth for config order; a name it does not list
      // (a still-running orphan) keeps its place after the ones it does.
      if (order.length > 0) {
        next = { ...next, order: order.concat(next.order.filter((name) => !order.includes(name))) };
      }
      return folded(next);
    }
    default:
      return ignore(board);
  }
}

/// The fields a reload's `changed` verdict overwrites — the config-derived metadata, and
/// deliberately nothing that is live state.
const RELOAD_METADATA_KEYS = [
  "triggerLabel",
  "triggerDetail",
  "triggerFields",
  "confined",
  "group",
  "icon",
  "description",
  "queue",
  "queuePriority",
  "queueParallelism",
  "isPoller",
];

/// Pull one service's record and its place in the order. An unknown name is a no-op.
function dropService(board, name) {
  if (board.services[name] === undefined) return board;
  const services = { ...board.services };
  delete services[name];
  return { ...board, services, order: board.order.filter((each) => each !== name) };
}

/// Set one boolean marker on one service, leaving its running badge alone — an orphan and a
/// stale service both keep running.
function setFlag(board, name, key, value) {
  return withService(board, name, (svc) => {
    svc[key] = value;
    return svc;
  });
}

// --- selectors ---------------------------------------------------------------------

/**
 * The group rollups, mirroring `GroupRollup` / `DashboardModel::group_rollup`
 * (`crates/tui/src/model.rs`, ADR-0066). A **selector**, not stored state: rolling these up
 * inside the reducer would recompute and re-copy them on every frame of an attach burst for
 * a value only a renderer reads.
 *
 * Two things it deliberately does **not** do, both recorded here so a later diff against a
 * real `afkd top` attributes a mismatch to this file rather than hunting it in the fold:
 *
 * - **No aggregate state word, and no ladder.** The aggregate afkd has is `anyCrashed`, and
 *   ADR-0066 is explicit that this is a refusal rather than an omission: "A group row shows
 *   **no** state badge in the resting case. The one exception: if a member is `Crashed` …
 *   Health is surfaced; ordinary running/stopped state is not."
 * - **Flat bucketing, not transitive.** `group_rollup` gathers members with `in_subtree`, so
 *   `ops` rolls up `ops::db::vacuum` too; this buckets by the **exact** `group` string. The
 *   fold has no tree to collapse into yet (this module paints nothing), a flat map is the
 *   honest shape for a JSON dump, and the nesting belongs with the paint. A service whose
 *   group is `""` is a top-level row and a member of no group at all.
 */
export function groups(board) {
  const buckets = [];
  const byPath = {};
  for (const name of board.order) {
    const svc = board.services[name];
    if (svc === undefined || svc.group === "") continue;
    let bucket = byPath[svc.group];
    if (bucket === undefined) {
      bucket = {
        group: svc.group,
        members: [],
        anyCrashed: false,
        latestActivityAt: null,
        oldestInFlightSince: null,
        busy: 0,
        runs: 0,
        failures: 0,
        errors: 0,
      };
      byPath[svc.group] = bucket;
      buckets.push(bucket);
    }
    bucket.members.push(name);
    bucket.anyCrashed = bucket.anyCrashed || svc.badge === "Crashed";
    // The *max* instant — "this group was last alive X ago" — and the mirror of the
    // in-flight *min*, which is the longest-running member, not the latest to start.
    if (svc.lastActivityAt !== null) {
      bucket.latestActivityAt =
        bucket.latestActivityAt === null ? svc.lastActivityAt : Math.max(bucket.latestActivityAt, svc.lastActivityAt);
    }
    if (svc.inFlightSince !== null) {
      bucket.oldestInFlightSince =
        bucket.oldestInFlightSince === null
          ? svc.inFlightSince
          : Math.min(bucket.oldestInFlightSince, svc.inFlightSince);
      bucket.busy += 1;
    }
    bucket.runs += svc.runs;
    bucket.failures += svc.failures;
    bucket.errors += svc.errors;
  }
  return buckets;
}

/**
 * The queue lanes, mirroring `QueueLane` / `DashboardModel::queue_lanes`. Sorted by lane
 * name, which for the resolved `a::b` spellings is the operator-visible order.
 *
 * `parallelism` is the **max** over members (a husk that learned its lane from a live event
 * reports none, and must not drag the lane's width down to nothing), `held` counts the
 * members holding a slot and `waiting` those at the door. `priority` is carried although
 * `QueueLane` does not: it is per-service config metadata, so the lane takes it from any
 * member that reports one and leaves it `""` when none does — exactly as the tui's `Queue`
 * row then renders no parenthetical.
 */
export function queues(board) {
  const lanes = [];
  const byName = {};
  for (const name of board.order) {
    const svc = board.services[name];
    if (svc === undefined || svc.queue === "") continue;
    let lane = byName[svc.queue];
    if (lane === undefined) {
      lane = { lane: svc.queue, members: [], priority: "", parallelism: null, held: 0, waiting: 0 };
      byName[svc.queue] = lane;
      lanes.push(lane);
    }
    lane.members.push(name);
    if (lane.priority === "") lane.priority = svc.queuePriority;
    if (svc.queueParallelism !== null) {
      lane.parallelism = lane.parallelism === null ? svc.queueParallelism : Math.max(lane.parallelism, svc.queueParallelism);
    }
    if (svc.badge === "Busy" || svc.badge === "Checking") lane.held += 1;
    if (svc.badge === "Queued") lane.waiting += 1;
  }
  lanes.sort((a, b) => (a.lane < b.lane ? -1 : a.lane > b.lane ? 1 : 0));
  return lanes;
}

// --- wire coercion -----------------------------------------------------------------
//
// The wire is JSON from a peer that may be newer than this file, so every read is
// defensive in exactly one direction: a field of the wrong shape degrades to the type's
// zero rather than faulting the frame. That is the §3 skew rule applied field by field.

function isObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asArray(value) {
  return Array.isArray(value) ? value : [];
}

function toText(value) {
  return typeof value === "string" ? value : "";
}

function toNumber(value, fallback) {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

/// An age on the wire (`now − ms`) re-anchored on the caller's clock; `null` for an absent
/// or malformed one, which is honest blankness rather than a fabricated zero.
function anchorPast(now, ms) {
  return typeof ms === "number" && Number.isFinite(ms) ? now - ms : null;
}

/// A remaining duration on the wire (`now + ms`) re-anchored the same way.
function anchorFuture(now, ms) {
  return typeof ms === "number" && Number.isFinite(ms) ? now + ms : null;
}
