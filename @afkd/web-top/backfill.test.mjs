// The disk backfill's suite: `node --test @afkd/web-top/backfill.test.mjs`.
//
// The relay reads the daemon's own run corpus off disk and serves it as `backfill` /
// `backfill_log` frames ahead of a subscriber's live stream. This suite drives that read
// **as it ships** — by spawning `web-top --backfill <runs_dir> <service> busy|idle`, the
// same producer the stream seam calls — and folds what it prints through `fold.mjs`.
// Re-stating the disk-to-wire map in javascript would be a second implementation to keep in
// step, which is the trap the whole plugin avoids.
//
// Everything it reads is **recorded**: `fixtures/runs/` and `fixtures/runs-midfire/` are
// `cp -r` copies of a real daemon's `<state dir>/runs`, taken after and during one fire of
// one session, and `fixtures/backfill.jsonl` is that same session's live wire capture with
// the recorder attached mid-fire. See `fixtures/README.md` for the daemon and the config.
// So the overlap the seam tests is a real overlap — both inputs came out of one run.
//
// The python legs skip loudly without `python3`, the repo's own idiom.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { SEAM_GUARD_LINES, fold, sanitize, seed } from "./fold.mjs";
import { bodyHeight, layout, runMetrics } from "./layout.mjs";
import { flashOf, needleOf, newSession, press, rowsOf, selectedIndex, typingOf } from "./session.mjs";
import { BASE, STEP, assertGolden, assertGrid, paneBody, readCapture, screenText } from "./testkit.mjs";

const HERE = import.meta.dirname;

/// The one service every fixture in this suite was recorded for — namespaced and wide-glyph,
/// so the `::` to `__` run-dir encoding and a CJK name are on the path of every read here.
const SERVICE = "ops::監視";

/// The recorded run this suite's log assertions are about — the newest dir of both corpora,
/// and the one the tail anchors on.
const NEWEST_RUN = "260923-003320";

/// Whether `python3` is on PATH; the relay is python and this suite spawns it.
function python3Available() {
  try {
    execFileSync("python3", ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

const SKIP_PYTHON = python3Available() ? false : "python3 is not on PATH, and the relay is python";

/**
 * A recorded run corpus, copied to a scratch dir with each run dir's **mtime** restored from
 * the instant its own name encodes.
 *
 * Restoring it is not decoration. `recent_run_dirs` orders a service's run dirs by mtime, and
 * git carries no mtimes at all — a fresh clone stamps every one of these with the checkout
 * instant, in whatever order the checkout happened to write them. The run name is the run's
 * own start stamp (`yymmdd-hhmmss`), and this session recorded four runs in four distinct
 * seconds, so the name is the chronology the corpus was taken with, and git *does* carry it.
 */
function corpus(name) {
  const at = mkdtempSync(join(tmpdir(), "afkd-web-top-"));
  const root = join(at, name);
  cpSync(join(HERE, "fixtures", name), root, { recursive: true });
  for (const service of readdirSync(root)) {
    for (const run of readdirSync(join(root, service))) {
      const [date, time] = run.split("-");
      const when =
        Date.UTC(
          2000 + Number(date.slice(0, 2)),
          Number(date.slice(2, 4)) - 1,
          Number(date.slice(4, 6)),
          Number(time.slice(0, 2)),
          Number(time.slice(2, 4)),
          Number(time.slice(4, 6)),
        ) / 1000;
      utimesSync(join(root, service, run), when, when);
    }
  }
  return { root, dispose: () => rmSync(at, { recursive: true, force: true }) };
}

/**
 * The burst `SERVICE` would be served out of the recorded corpus `name` — the relay's own
 * `--backfill` mode, spawned exactly as an operator would run it.
 */
function servedBurst(name, liveness) {
  const { root, dispose } = corpus(name);
  try {
    const out = execFileSync("python3", [join(HERE, "web-top"), "--backfill", root, SERVICE, liveness], {
      encoding: "utf8",
    });
    return out
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => JSON.parse(line));
  } finally {
    dispose();
  }
}

/**
 * The recorded capture split the way the relay splits it: the attach `snapshot`, and the
 * `live` frames that followed it up to the drain the recorder stayed attached through.
 *
 * Split in one place because the two must come out of **one** read. A second `meta.snapshot`
 * folded mid-replay re-seeds every card from scratch — the tree and the ring start empty on
 * every snapshot, which is `seed_from_snapshot`'s pinned choice — so a test that folded the
 * snapshot once at the head and again in place would silently wipe the burst it just served.
 */
function capture() {
  const frames = readCapture("backfill.jsonl");
  const drain = frames.findIndex((f) => f.type === "meta" && f.meta === "quitting");
  const live = drain === -1 ? frames : frames.slice(0, drain);
  const snapshot = live.find((f) => f.type === "meta" && f.meta === "snapshot");
  assert.ok(snapshot, "the capture opens with an attach snapshot");
  assert.equal(
    live.filter((f) => f.type === "meta" && f.meta === "snapshot").length,
    1,
    "…and exactly one, so the replays below never re-seed mid-stream",
  );
  return { snapshot, live: live.filter((f) => f !== snapshot) };
}

/// Fold `frames` into a fresh board on a synthetic clock, returning the board and the instant
/// the last one landed at.
function replay(frames, start = BASE) {
  let board = seed({ logLines: 2000 });
  let at = start;
  for (const frame of frames) board = fold(board, frame, (at += STEP));
  return { board, at };
}

/// The node ids one frame list **opens**, in order.
function openedIds(frames) {
  return frames
    .filter((f) => (f.type === "trace" || f.type === "backfill") && f.event?.op === "opened")
    .map((f) => f.event.node.id);
}

/// Every id reachable by walking a folded tree from its roots — the shape the pane draws,
/// rather than the `nodes` map the fold keys by, so a node reachable twice is counted twice.
function walkIds(tree) {
  const seen = [];
  const stack = tree.roots.slice().reverse();
  while (stack.length > 0) {
    const id = stack.pop();
    const node = tree.nodes[id];
    if (node === undefined) continue;
    seen.push(id);
    stack.push(...node.children.slice().reverse());
  }
  return seen;
}

// --- AC2: the disk replay and the live stream overlap on one run ---------------------

test("the disk replay and the live stream over one run fold to one tree and one ring", { skip: SKIP_PYTHON }, () => {
  // The mid-fire corpus and the live capture are two views of the **same** in-progress run:
  // the copy was taken while the fire was still printing, so the live frames legitimately
  // open nodes the copy never saw and re-stream lines the copy already holds. That overlap is
  // the whole subject — it is what `openNode`'s identical-replay dedup and the ring's
  // content-keyed seam exist for.
  const { snapshot, live } = capture();
  const burst = servedBurst("runs-midfire", "busy");

  // The fixtures have to actually overlap, or every assertion below is vacuous.
  const diskIds = openedIds(burst);
  const liveIds = openedIds(live);
  const sharedIds = diskIds.filter((id) => liveIds.includes(id));
  assert.ok(sharedIds.length >= 2, `the two inputs open the same nodes (shared: ${sharedIds})`);
  const diskTexts = burst.filter((f) => f.type === "backfill_log").map((f) => f.line);
  const liveTexts = live.filter((f) => f.type === "log").map((f) => sanitize(f.line));
  const sharedTexts = liveTexts.filter((t) => diskTexts.includes(t));
  assert.ok(sharedTexts.length >= 3, `…and carry the same lines (shared: ${sharedTexts.length})`);

  // The order a subscriber is served in: the snapshot, then the burst, then everything live.
  const { board } = replay([snapshot, ...burst, ...live]);
  const svc = board.services[SERVICE];
  assert.ok(svc !== undefined, "the snapshot seeded the service the burst names");

  // One tree, not two. The window replayed three whole runs, so three roots is right and
  // three is what the pane draws — what "not two" means here is that the **overlapped** run
  // has one root and not a second one under a repeated id.
  assert.equal(svc.tree.roots.length, 3, `the window's three runs are three roots, not ${svc.tree.roots.length}`);
  const walked = walkIds(svc.tree);
  assert.equal(new Set(walked).size, walked.length, `every node is on exactly one path: ${walked}`);
  assert.deepEqual(
    walked.slice().sort((a, b) => a - b),
    [...new Set([...diskIds, ...liveIds])].sort((a, b) => a - b),
    "the folded ids are exactly the union of what the disk replay and the live stream opened",
  );

  // …and one ring. Every served line and every live line is there, once, in order, with no
  // adjacent repeat — the seam dropped exactly the overlap and nothing else.
  const ring = svc.log.lines.map((e) => e.text);
  assert.equal(new Set(ring).size, ring.length, `no line is in the ring twice:\n${ring.join("\n")}`);
  for (let i = 1; i < ring.length; i += 1) {
    assert.notEqual(ring[i], ring[i - 1], `row ${i} repeats the row above it: ${JSON.stringify(ring[i])}`);
  }
  const wanted = [...diskTexts, ...liveTexts.filter((t) => !diskTexts.includes(t))];
  assert.deepEqual(ring, wanted, "the ring is the served tail continued by the genuinely new live lines");
  // The seam consumed itself: it was armed on the `last` served line and is disarmed once the
  // first line that was not in the tail arrived.
  assert.equal(svc.logSeam, null, "the seam disarmed on the first genuinely new line");
  assert.deepEqual(svc.servedLog, [], "…and the arm accumulator drained on the burst's `last`");
});

test("without the seam the same replay really would duplicate", { skip: SKIP_PYTHON }, () => {
  // The negative control for the test above. Serving the burst as **not busy** is exactly the
  // "seam never armed" case — `apply_served_log` arms only for a busy service — so the same
  // two inputs, folded the same way, must double every overlapping line. Without this, an
  // `admit` that always returned `true` would pass the assertion above for free.
  const { snapshot, live } = capture();
  const unarmed = servedBurst("runs-midfire", "idle");
  const { board } = replay([snapshot, ...unarmed, ...live]);
  const ring = board.services[SERVICE].log.lines.map((e) => e.text);
  const doubled = ring.filter((t, i) => ring.indexOf(t) !== i);
  assert.ok(doubled.length >= 3, `an unarmed seam duplicates the overlap, and it did not:\n${ring.join("\n")}`);
  assert.equal(board.services[SERVICE].logSeam, null, "a not-busy service's burst arms nothing");
});

// --- the two sanitizers, one content key --------------------------------------------

test("the relay's sanitizer and the page's agree byte for byte", { skip: SKIP_PYTHON }, () => {
  // The seam keys on **text**, so a one-character disagreement between the python that
  // sanitizes the served tail and the javascript that sanitizes the live line turns the guard
  // into a silent no-op — duplicates, with nothing red. This is the only thing that catches
  // it, which is why it is not optional.
  //
  // The inputs are the run's own raw `run.log` bytes, stamp prefix stripped: SGR, an OSC
  // residue, an embedded carriage return, tabs, a bidi override, CJK, an emoji and a
  // zero-width space, plus the daemon's own narration lines around them.
  const raw = readFileSync(join(HERE, "fixtures", "runs", "ops__監視", NEWEST_RUN, "run.log"), "utf8")
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => line.slice(20));
  assert.ok(raw.length >= 8, `the recorded transcript is worth comparing (${raw.length} lines)`);
  for (const [needle, what] of [
    ["[", "an SGR run"],
    ["]", "an OSC residue"],
    ["\r", "an embedded carriage return"],
    ["\t", "a tab"],
    ["‮", "a bidi override"],
    ["監視サービス", "CJK"],
    ["\u{1f680}", "an emoji"],
    ["​", "a zero-width space"],
  ]) {
    assert.ok(
      raw.some((l) => l.includes(needle)),
      `the transcript still carries ${what}`,
    );
  }

  const served = servedBurst("runs", "idle")
    .filter((f) => f.type === "backfill_log")
    .map((f) => f.line);
  assert.deepEqual(served, raw.map(sanitize), "the two sanitizers produce the same ring text");
  // …and they really did strip something, so an identity transform on both sides cannot pass.
  assert.notDeepEqual(served, raw, "the sanitizers are not the identity over this transcript");
});

// --- AC1/AC4: a committed run replayed through the fold, rendered, diffed ------------

/**
 * The run view opened on the fixture's service, driven through the page's **own** keys — `j`
 * onto the service row under its group header, then `o`. Driven rather than dialled so these
 * screens are ones an operator can actually reach.
 */
function openedRun(board, now, chords) {
  let session = newSession();
  const view = () => ({
    cols: 100,
    rows: 30,
    now,
    version: "0.2.130",
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
  for (const key of chords) {
    const out = press(session, board, { ctrl: false, key }, { now, bodyHeight: bodyHeight(board, view()) });
    assert.ok(out.handled, `the page takes \`${key}\``);
    session = out.session;
  }
  return view;
}

test("a finished run replayed off disk fills both panes", { skip: SKIP_PYTHON }, () => {
  // AC1 and AC4 as the page sees them: nothing live at all — the attach snapshot plus the
  // burst `--backfill` prints for a service that is **idle**, which is exactly what a tab
  // opened after the fire finished is served. Both panes are full, and both are diffed
  // against a committed golden, so a colour or weight drift on a backfilled pane is caught
  // like any other.
  //
  // The pinned header still reads the snapshot's own `Busy`: the capture was recorded
  // mid-fire, and this test deliberately folds no live frame that would settle it. The badge
  // is the snapshot's word; the two panes under it are what the burst filled.
  const burst = servedBurst("runs", "idle");
  assert.ok(
    burst.some((f) => f.type === "backfill"),
    "the burst carries the tree",
  );
  assert.ok(
    burst.some((f) => f.type === "backfill_log"),
    "…and the log tail",
  );
  const { board, at } = replay([capture().snapshot, ...burst]);
  const now = at + 1000;
  const svc = board.services[SERVICE];

  // The corpus is only worth diffing goldens over if it still holds what it was recorded for:
  // three whole runs of one daemon generation, with the fourth — an older generation, its ids
  // re-minted — stopped before by the window walk.
  assert.equal(svc.tree.roots.length, 3, "the window replayed the newest generation's three runs");
  assert.ok(svc.log.lines.length >= 8, `the log tail is the newest run's, whole (${svc.log.lines.length} lines)`);
  // A replayed **tree** is history, not output, so it leaves the snapshot's authoritative
  // last-activity anchor exactly where it seeded it — its own test below. (The served *log*
  // arm is the deliberate asymmetry: it pushes through the very same `apply_log` the live
  // stream does, stamp included, which is what `apply_served_log` does in the Rust.)

  // The log pane, which is the one `o` opens on.
  const logRows = layout(board, openedRun(board, now, ["j", "o"])());
  assertGrid(logRows, 100, 30);
  assertGolden("backfill-log-100x30.txt", logRows);
  const logLines = screenText(logRows).split("\n");
  // The title carries the scope alone (the `start–end/total` range is deliberately unpainted,
  // as `logview`'s own split pane leaves it), so the claim the range used to carry is made
  // where it lives: the scroll's total is the **whole** backfilled ring while the 30-row
  // viewport draws only its tail. Asserted as the honest way to say "the run view is not
  // empty" over a tail longer than the screen.
  assert.equal(logLines[2].trimEnd(), "Log · all", "the pane names the scope it opened on");
  assert.equal(
    runMetrics(board, openedRun(board, now, ["j", "o"])()).logTotal,
    svc.log.lines.length,
    "the scroll clamps against the whole backfilled ring",
  );
  assert.ok(
    paneBody(logRows).length < svc.log.lines.length,
    `…and the viewport really draws only its tail (${paneBody(logRows).length} of ${svc.log.lines.length})`,
  );
  // …and the tail it does draw is the newest end of that ring, following by default.
  const logBody = paneBody(logRows);
  const last = svc.log.lines[svc.log.lines.length - 1].text;
  assert.ok(logBody.some((l) => l.includes(last.slice(0, 30))), `the newest line is on screen: ${last}`);
  assert.ok(
    logBody.some((l) => l.includes("pass 1 line 7: CJK 監視サービスの担当者")),
    "…and so is the wide-glyph line, whole",
  );

  // …and the tree pane behind it, one `Tab` away.
  const treeRows = layout(board, openedRun(board, now, ["j", "o", "tab"])());
  assertGrid(treeRows, 100, 30);
  assertGolden("backfill-tree-100x30.txt", treeRows);
  const treeText = screenText(treeRows);
  for (const id of Object.keys(svc.tree.nodes).map(Number)) {
    const node = svc.tree.nodes[id];
    assert.ok(treeText.includes(node.relabel ?? node.label), `node ${id} (${node.label}) is on the screen`);
  }
  assert.ok(
    Object.values(svc.tree.nodes).every((n) => n.status !== "running"),
    "every replayed node carries the terminal status the trace recorded",
  );
});

// --- the two fold arms in their own right -------------------------------------------

test("a backfill frame folds the tree without stamping activity", { skip: SKIP_PYTHON }, () => {
  // `Frame::Backfill` is `Frame::Trace`'s payload tagged as history, and the one difference is
  // the last-activity stamp: a replay of minutes-old nodes must not overwrite the authoritative
  // `last_activity_ms` the snapshot just seeded. Asserted against the **same** frames folded as
  // live `trace`, so it is the tag that moves the anchor and nothing else.
  const { snapshot } = capture();
  const burst = servedBurst("runs", "idle").filter((f) => f.type === "backfill");
  assert.ok(burst.length > 0, "the corpus really replays a tree");

  const seeded = replay([snapshot]).board.services[SERVICE].lastActivityAt;
  assert.notEqual(seeded, null, "the snapshot really seeds an anchor for the burst to leave alone");
  const history = replay([snapshot, ...burst]).board.services[SERVICE];
  const asLive = replay([snapshot, ...burst.map((f) => ({ ...f, type: "trace" }))]).board.services[SERVICE];
  assert.deepEqual(history.tree, asLive.tree, "the tree fold is identical arm for arm");
  assert.notEqual(asLive.lastActivityAt, seeded, "…and folded as live the very same frames do move the anchor");
  assert.equal(history.lastActivityAt, seeded, "a replay leaves the daemon's own value standing");
});

test("a busy service's served tail arms the seam on the whole tail", { skip: SKIP_PYTHON }, () => {
  // `apply_served_log`'s accumulator and `LogSeam::arm`: every line of a busy service's tail
  // carries `busy`, exactly one carries `last`, and it is that line that drains the
  // accumulator and arms the guard on the whole served tail — capped to `SEAM_GUARD_LINES`.
  const burst = servedBurst("runs", "busy").filter((f) => f.type === "backfill_log");
  assert.ok(burst.length > 1, "the recorded tail is more than one line");
  assert.equal(burst.filter((f) => f.last).length, 1, "exactly one served line closes the burst");
  assert.equal(burst[burst.length - 1].last, true, "…and it is the last one");
  assert.ok(
    burst.every((f) => f.busy),
    "every line of a busy service's tail says so",
  );

  const { board } = replay([capture().snapshot, ...burst]);
  const svc = board.services[SERVICE];
  assert.ok(Array.isArray(svc.logSeam), "a busy service's burst arms the seam");
  assert.deepEqual(svc.logSeam, burst.map((f) => f.line), "…on the whole served tail, in order");
  assert.ok(svc.logSeam.length <= SEAM_GUARD_LINES, `the guard window bounds it (${svc.logSeam.length})`);
  // The burst never filters itself: arming happens after the pushes, so every served line is
  // in the ring even though each one is also in the guard.
  assert.equal(svc.log.lines.length, burst.length, "every served line reached the ring");
});
