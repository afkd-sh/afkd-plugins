// The disk backfill's suite: `node --test @afkd/web_top/backfill.test.mjs`.
//
// The relay reads the daemon's own run corpus off disk and serves it as `backfill` /
// `backfill_log` frames ahead of a subscriber's live stream. This suite drives that read
// **as it ships** — by spawning `web_top --backfill <runs_dir> <service> busy|idle`, the
// same producer the stream seam calls — and folds what it prints through `fold.mjs`.
// Re-stating the disk-to-wire map in javascript would be a second implementation to keep in
// step, which is the trap the whole plugin avoids.
//
// Everything it reads is **recorded**: `fixtures/runs/` and `fixtures/runs-midfire/` are
// `cp -r` copies of a real daemon's `<state dir>/runs`, taken after and during one fire of
// one session, and `fixtures/backfill.jsonl` is that same session's live wire capture with
// the recorder attached mid-fire. See `fixtures/README.md` for the daemon and the config.
// So the overlap the seam tests is a real overlap — both inputs came out of one run.
// `fixtures/runs-blockwait/` is a third copy, of a session whose one kept run waits on a lane
// and makes one HTTP request, and `fixtures/blockwait-trace.txt` is `afkd trace`'s render of it.
//
// The python legs skip loudly without `python3`, the repo's own idiom.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { SEAM_GUARD_LINES, fold, sanitize, seed } from "./fold.mjs";
import { KIND_GLYPH, bodyHeight, layout, runMetrics, textWidth } from "./layout.mjs";
import { flashOf, needleOf, newSession, press, rowsOf, selectedIndex, typingOf } from "./session.mjs";
import { BASE, NO_AFKD_SRC, STEP, afkdSource, assertGolden, assertGrid, nodeKindWireName, paneBody, readCapture, readRecording, screenText } from "./testkit.mjs";

const HERE = import.meta.dirname;

/// The one service every fixture in this suite was recorded for — packaged and wide-glyph, so a
/// `package.name` run dir and a CJK name are on the path of every read here.
const SERVICE = "ops.監視";

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
 *
 * `place(run)` says which of the relay's two roots get each run dir: `"kept"`, the
 * `<root>/<segment>/` a `runs_dir` names; `"live"`, the `<temp>/afkd/runs/<segment>/` of a run
 * in flight, `temp` being the `$TMPDIR` the relay is spawned with; and `"kept-log-only"`, a
 * kept copy holding its `run.log` alone, as a failed run's is while it is copied over.
 */
function corpus(name, place = () => ["kept"]) {
  const at = mkdtempSync(join(tmpdir(), "afkd-web_top-"));
  const root = join(at, name);
  const temp = join(at, "tmp");
  const fixture = join(HERE, "fixtures", name);
  mkdirSync(root);
  mkdirSync(temp);
  for (const service of readdirSync(fixture)) {
    for (const run of readdirSync(join(fixture, service))) {
      const from = join(fixture, service, run);
      const copies = [];
      for (const where of place(run)) {
        if (where === "kept") {
          copies.push(join(root, service, run));
          cpSync(from, copies.at(-1), { recursive: true });
        } else if (where === "live") {
          copies.push(join(temp, "afkd", "runs", service, run));
          cpSync(from, copies.at(-1), { recursive: true });
        } else if (where === "kept-log-only") {
          copies.push(join(root, service, run));
          mkdirSync(copies.at(-1), { recursive: true });
          cpSync(join(from, "run.log"), join(copies.at(-1), "run.log"));
        } else {
          throw new Error(`no root is called ${where}`);
        }
      }
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
      for (const copy of copies) utimesSync(copy, when, when);
    }
  }
  return { root, temp, dispose: () => rmSync(at, { recursive: true, force: true }) };
}

/**
 * The burst `service` would be served out of the recorded corpus `name` — the relay's own
 * `--backfill` mode, spawned exactly as an operator would run it. `service` is `SERVICE`
 * unless a corpus recorded for another one is read.
 *
 * It is always spawned with a `$TMPDIR` of the corpus's own, so the live root it reads holds
 * what `place` put there and never a run this host has in flight. `kept: false` hands it a
 * `<runs_dir>` that is not there.
 */
function servedBurst(name, liveness, { place, kept = true, service = SERVICE } = {}) {
  const { root, temp, dispose } = corpus(name, place);
  try {
    const runsDir = kept ? root : join(temp, "..", "absent");
    const out = execFileSync("python3", [join(HERE, "web_top"), "--backfill", runsDir, service, liveness], {
      encoding: "utf8",
      env: { ...process.env, TMPDIR: temp },
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
  const raw = readFileSync(join(HERE, "fixtures", "runs", "ops.監視", NEWEST_RUN, "run.log"), "utf8")
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

// --- the relay's run-dir segment, afkd's ----------------------------------------------

/**
 * Loads the relay as a module and loads nothing else: its `main()` sits behind `__main__`, so
 * the import is inert. Loaded rather than spawned through its CLI, as the other legs are,
 * because `name_segment` and the relay's tables have no CLI surface of their own. The python
 * expression `sys.argv[2]` is evaluated with the module bound to `relay` and the JSON in
 * `sys.argv[3]` bound to `arg`, and its value is printed as JSON.
 */
const RELAY_LOADER = `
import importlib.machinery, importlib.util, json, sys
loader = importlib.machinery.SourceFileLoader("web_top_relay", sys.argv[1])
relay = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(relay)
print(json.dumps(eval(sys.argv[2], {"relay": relay, "arg": json.loads(sys.argv[3])}), ensure_ascii=False))
`;

/** The python expression `expr` over the relay loaded as a module, with `arg` bound to it. */
function relayEval(expr, arg = null) {
  const out = execFileSync(
    "python3",
    ["-I", "-c", RELAY_LOADER, join(HERE, "web_top"), expr, JSON.stringify(arg)],
    { encoding: "utf8" },
  );
  return JSON.parse(out);
}

/** The relay's `name_segment` over each of `names`, in order. */
function relayNameSegments(names) {
  return relayEval("[relay.name_segment(n) for n in arg]", names);
}

test("the relay's name_segment is afkd's: the case fold alone", { skip: SKIP_PYTHON }, () => {
  // The burst reads `<runs>/<segment>`, so a segment that differs from the one the daemon's
  // `scratch_root` wrote reads an empty corpus, with nothing red. The twin of this table is
  // `afkd_lang::name_segment`'s own test (`crates/lang/src/namespace.rs`), kept in step by
  // hand: this suite runs without `AFKD_SRC`, so it cannot read the Rust. `::` is not special
  // there any more, so it must not be here.
  const table = [
    ["a::b", "a::b"],
    ["Afkd::Develop", "afkd::develop"],
    ["selfdev.develop.0", "selfdev.develop.0"],
    ["Selfdev.Develop.1", "selfdev.develop.1"],
    [SERVICE, "ops.監視"],
    ["OPS.監視", "ops.監視"],
    ["MÜNCHEN.Bereitstellung", "münchen.bereitstellung"],
    ["prune_runs", "prune_runs"],
  ];
  assert.deepEqual(
    relayNameSegments(table.map(([name]) => name)),
    table.map(([, segment]) => segment),
  );
});

// --- the two roots, one burst ---------------------------------------------------------

/**
 * Asserts `burst` is the one-root burst of the mid-fire corpus, every run kept, and that it is
 * worth comparing: a tree and a tail, so an empty burst cannot equal an empty one.
 */
function assertServedAsKept(burst, what) {
  const kept = servedBurst("runs-midfire", "busy");
  assert.ok(kept.some((f) => f.type === "backfill"), "the kept burst carries the tree");
  assert.ok(kept.some((f) => f.type === "backfill_log"), "…and the log tail");
  assert.deepEqual(burst, kept, what);
}

test("a run in flight on the temp root is served as the kept one would be", { skip: SKIP_PYTHON }, () => {
  // The `keep_runs on_fail` shape mid-fire: the earlier runs are kept, and the newest is still
  // running under the daemon's temp dir. Read off both roots, the burst is the one-root burst
  // over the same recorded runs — the newest run's tree and its tail included.
  const burst = servedBurst("runs-midfire", "busy", {
    place: (run) => (run === NEWEST_RUN ? ["live"] : ["kept"]),
  });
  assertServedAsKept(burst, "the run in flight is read as if it were kept");
});

test("the temp root alone serves a service with no kept runs", { skip: SKIP_PYTHON }, () => {
  // A service that has never failed under `keep_runs on_fail` has no kept dir at all: every
  // run it is served is on the temp root, and the `runs_dir` holds nothing for it.
  const burst = servedBurst("runs-midfire", "busy", { place: () => ["live"] });
  assertServedAsKept(burst, "a service read off the temp root alone is served whole");
});

test("a run in both roots is read once, from the kept root", { skip: SKIP_PYTHON }, () => {
  // A failed run is in both while it is copied from the temp root to the kept one. Read twice,
  // its second copy would repeat its own ids and stop the window walk before the older runs.
  const burst = servedBurst("runs-midfire", "busy", {
    place: (run) => (run === NEWEST_RUN ? ["kept", "live"] : ["kept"]),
  });
  assertServedAsKept(burst, "a run in both roots is one run");
});

test("a kept copy with no trace yet yields to the live one", { skip: SKIP_PYTHON }, () => {
  // Part way through that copy, the kept dir holds a `run.log` and no `trace.jsonl` yet. It is
  // not a real run dir, so it claims no name, and the run is read whole off the temp root.
  const burst = servedBurst("runs-midfire", "busy", {
    place: (run) => (run === NEWEST_RUN ? ["kept-log-only", "live"] : ["kept"]),
  });
  assertServedAsKept(burst, "a half-copied kept run is read off the temp root");
});

test("a runs_dir that is not there is the tab's meta.error, then the temp root's run", { skip: SKIP_PYTHON }, () => {
  // `--backfill` reads its `<runs_dir>` as a named one, so a path that is not there is the
  // sentence a tab is flashed, and the live root is read all the same: a bad kept path has
  // nothing to do with a run in flight. The mode exits 0, or `execFileSync` would throw.
  const [error, ...burst] = servedBurst("runs-midfire", "busy", { place: () => ["live"], kept: false });
  assert.equal(error.type, "meta", `the burst opens with a meta frame: ${JSON.stringify(error)}`);
  assert.equal(error.meta, "error", "…an error");
  assert.match(
    error.message,
    /^@afkd\/web_top: no run corpus to read at \/.+\/absent, so a run view opens with no kept history$/,
    "…naming the path it cannot read",
  );
  assertServedAsKept(burst, "the temp root's runs follow the error, whole");
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
    selected: selectedIndex(session, rowsOf(session, board), board) ?? -1,
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

// --- every node kind afkd reads, read off disk ----------------------------------------

/**
 * `tracefile::kind_from_str` read out of the Rust: its `"wire" => NodeKind::Arm` pairs in the
 * order the match spells them, and the arm its `_` fallback degrades an unknown kind to.
 */
function kindFromStr() {
  const rust = afkdSource("crates", "app", "src", "tracefile.rs");
  const from = rust.indexOf("fn kind_from_str");
  const body = rust.slice(rust.indexOf("{", from), rust.indexOf("\n}", from));
  const pairs = [...body.matchAll(/"(\w+)" => NodeKind::(\w+)/g)].map(([, wire, arm]) => [wire, arm]);
  const fallback = body.match(/_ => NodeKind::(\w+)/)?.[1];
  return { pairs, fallback };
}

test("the relay reads every node kind afkd's kind_from_str reads", { skip: NO_AFKD_SRC || SKIP_PYTHON }, () => {
  // The relay maps a recorded `kind` through `NODE_KINDS` and degrades any other to
  // `UNKNOWN_KIND`, as the Rust degrades it to `Cmd`. A kind the Rust reads and the relay does
  // not would replay off disk as `cmd` and draw the gear, while the live wire, which passes the
  // kind through, draws its own glyph: so the relay's list is held to the Rust's by name.
  const { pairs, fallback } = kindFromStr();
  // A regex that rots reads no pair, and an empty list must not pass for an empty tuple.
  assert.ok(pairs.length >= 13, `kind_from_str reads as only ${pairs.length} pairs: the arm regex no longer matches`);
  assert.ok(fallback !== undefined, "kind_from_str still degrades an unknown kind by a `_` arm");
  // The fallback's wire name is derived, not read, so the rule that derives it is held to every
  // pair the Rust does spell.
  for (const [wire, arm] of pairs) {
    assert.equal(nodeKindWireName(arm), wire, `${arm} is spelled \`${wire}\` on the wire, by the snake_case rule`);
  }
  const relay = relayEval("[sorted(relay.NODE_KINDS), relay.UNKNOWN_KIND]");
  assert.deepEqual(
    relay[0],
    [...pairs.map(([wire]) => wire), nodeKindWireName(fallback)].sort(),
    "the relay's NODE_KINDS are the kinds kind_from_str reads, its fallback's included",
  );
  assert.equal(relay[1], nodeKindWireName(fallback), "…and an unknown kind degrades to the Rust's fallback");
});

test("the relay and the page name the same node kinds", { skip: SKIP_PYTHON }, () => {
  // web_top's two mirrors of afkd's node kinds, held to each other where no afkd checkout is
  // named: a kind the relay serves off disk has a glyph on the page, and a kind the page draws
  // is one the relay does not degrade.
  const [kinds, unknown] = relayEval("[sorted(relay.NODE_KINDS), relay.UNKNOWN_KIND]");
  assert.deepEqual(kinds, Object.keys(KIND_GLYPH).sort(), "NODE_KINDS and KIND_GLYPH name the same kinds");
  assert.ok(kinds.includes(unknown), `the relay degrades an unknown kind to one it names (${unknown})`);
});

/** The width `afkd trace` renders a run tree at, `tracefile::TRACE_RENDER_WIDTH`. */
const TRACE_WIDTH = 100;

/**
 * The service `runs-blockwait/` was recorded for: a `with queue(heavy)` block that waits behind
 * another service's run, then a `with queue(gpu)` block, then one `core:http` request.
 */
const BLOCKER = "blocker";

/** `row`'s text over its first `width` cells, a wide glyph's one cell counted as two. */
function textOver(row, width) {
  let text = "";
  let used = 0;
  for (const cell of row) {
    if (used + cell.width > width) break;
    text += cell.text;
    used += cell.width;
  }
  return text;
}

/** The display column `glyph` starts at on `line`, as a terminal counts columns. */
function glyphColumn(line, glyph) {
  return textWidth(line.slice(0, line.indexOf(glyph)));
}

test("a recorded block's wait replays off disk as afkd trace draws it", { skip: SKIP_PYTHON }, () => {
  // A block's wait and its admission are `queue_wait` and `queue` nodes, and a `core:http`
  // request an `http` node. Served off disk to a tab that opened after the run, each must keep
  // its own kind, so its row draws its own glyph at the column the terminal does, and the
  // request's row its body's size in the OUT cell. `fixtures/blockwait-trace.txt` is `afkd trace`'s stdout for
  // this very `trace.jsonl`, and the drift gate re-renders it with the afkd on `PATH`, so the
  // terminal side of the comparison is the TUI's own tree, not a hand-written one.
  const runs = readdirSync(join(HERE, "fixtures", "runs-blockwait", BLOCKER));
  assert.equal(runs.length, 1, `the corpus holds one recorded run (${runs})`);
  const recorded = readFileSync(join(HERE, "fixtures", "runs-blockwait", BLOCKER, runs[0], "trace.jsonl"), "utf8")
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line))
    .filter((event) => event.ev === "opened")
    .map((event) => event.kind);
  for (const kind of ["queue_wait", "queue", "http"]) {
    assert.ok(recorded.includes(kind), `the recorded trace opens a ${kind} node`);
  }

  const burst = servedBurst("runs-blockwait", "idle", { service: BLOCKER });
  assert.deepEqual(
    burst.filter((f) => f.type === "backfill" && f.event.op === "opened").map((f) => f.event.node.kind),
    recorded,
    "the relay serves every node with the kind the trace recorded",
  );

  // The snapshot of the session that recorded the same config, so the burst lands on a board
  // that knows the service.
  const snapshot = readRecording("block-wait.jsonl").find((f) => f.type === "meta" && f.meta === "snapshot");
  const { board, at } = replay([snapshot, ...burst]);
  const run = {
    service: BLOCKER,
    focus: "tree",
    cursor: null,
    overrides: new Map(),
    expanded: new Set(),
    tree: "follow",
    log: "follow",
    scoped: false,
  };
  // The run view's tree pane is `cols` less its one scrollbar cell, so it is as wide as
  // `afkd trace`'s render one column wider.
  const cols = TRACE_WIDTH + 1;
  const rows = layout(board, { cols, rows: 30, now: at + 1000, version: "0.2.487", run });
  assertGrid(rows, cols, 30);

  const want = readFileSync(join(HERE, "fixtures", "blockwait-trace.txt"), "utf8")
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => line.trimEnd());
  assert.equal(paneBody(rows).length, want.length, "the tree pane draws one row per row afkd trace prints");
  const drawn = rows.slice(4, 4 + want.length).map((row) => textOver(row, TRACE_WIDTH).trimEnd());
  for (const [kind, glyph] of [
    ["queue_wait", "⏳"],
    ["queue", "🚦"],
    ["http", "🌐"],
  ]) {
    const line = want.findIndex((text) => text.includes(glyph));
    assert.ok(line !== -1, `afkd trace draws the ${kind} node with ${glyph}`);
    assert.ok(drawn[line].includes(glyph), `the run view draws the ${kind} node with ${glyph}: ${drawn[line]}`);
    assert.equal(
      glyphColumn(drawn[line], glyph),
      glyphColumn(want[line], glyph),
      `the ${kind} node's ${glyph} is at the column afkd trace puts it`,
    );
  }
  assert.deepEqual(drawn, want, "every row is afkd trace's row: glyph, label and rail");
});
