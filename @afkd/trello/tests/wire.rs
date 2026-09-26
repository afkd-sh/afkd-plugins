//! The wire contract suite: the real `afkd-trello` exec, spoken to exactly as afkd speaks
//! to it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake Trello
//! reached through the kind's own `base_url`. No afkd is involved.
//!
//! Every leg ends by closing stdin and holding the child to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)) — so a line that strayed onto stdout fails
//! whichever leg wrote it.

mod common;

use serde_json::{json, Value};

use common::fake::{utc, FakeTrello, BOARD, KEY, TOKEN};
use common::{Plugin, TempDir, DEVELOP, DISCUSS, MAX_LINE, OWNER};

/// A card title carrying the delimiter a success line wraps it in, wide CJK, an emoji and
/// an em dash.
const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";
const BODY: &str = "The client retries forever once the token expires.\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";
/// The two humans on the card, and what they said before afkd ever looked.
const PHIL: &str = "Please cap it at 30s; the header says so.";
const CHEN: &str = "看起来不对 🚨 — it resets on every 401:\n\n```\nGET /1/members/me 401\n```";
const SHORT_LINK: &str = "Rk7eLy5w";

/// The board every leg starts from: the selfdev lists, and one card up for grabs with a
/// checklist and a two-person thread.
struct Board {
    fake: FakeTrello,
    card: String,
    phil: String,
    chen: String,
    /// The checklist's item ids, which the brief carries.
    items: Vec<String>,
    /// The two human comments' ids.
    asked: [String; 2],
}

fn board() -> Board {
    let fake = FakeTrello::start();
    for list in [
        "Up for Grabs",
        "In Progress",
        "Review",
        "Backlog",
        "Discussion",
    ] {
        fake.list(list);
    }
    let phil = fake.member("phil", "Phil Ek");
    let chen = fake.member("chen", "陳大文");
    fake.member("rival-host", "Rival");
    let card = fake.card("Up for Grabs", SHORT_LINK, TITLE, BODY);
    let items = fake.checklist(
        &card,
        "Acceptance",
        &[
            ("retry stops after 5 tries", true),
            ("backoff is capped at 30s", false),
        ],
    );
    let asked = [
        fake.comment(&card, &phil, PHIL, 3600),
        fake.comment(&card, &chen, CHEN, 1800),
    ];
    Board {
        fake,
        card,
        phil,
        chen,
        items,
        asked,
    }
}

/// The live selfdev block, lowered to JSON as afkd lowers it: repeatable keys as arrays,
/// flags as `true`, blocks as objects with the value beside them as `@value` — and afkd's
/// own three keys along for the ride.
fn settings(fake: &FakeTrello) -> Value {
    json!({
        "board": format!("https://trello.com/b/{BOARD}/afkd-selfdev"),
        "base_url": fake.base_url(),
        "api_key": KEY,
        "token": TOKEN,
        "pick_from": "Up for Grabs",
        "poll_interval": "30s",
        "max_attempts": "2",
        "follow_comments": "2m",
        "on_claim": {"add_member": ["self"], "move_to": [{"@value": "In Progress", "at": ["top"]}]},
        "on_done": {"move_to": [{"@value": "Review", "at": ["top"]}], "comment": [
            "afkd landed this card in @{run:duration} - @{run:cost}, @{run:turns} agent turns."
        ]},
        "on_fail": {"move_to": [{"@value": "Backlog", "at": ["bottom"]}], "add_label": ["Problem"]},
        "on_park": {"comment": ["parked after @{run:duration}, waiting on you"]},
    })
}

/// The grooming service's block: `discuss_with anyone` over its own list, no lifecycle.
fn discuss_settings(fake: &FakeTrello) -> Value {
    json!({
        "board": format!("https://trello.com/b/{BOARD}/afkd-selfdev"),
        "base_url": fake.base_url(),
        "api_key": KEY,
        "token": TOKEN,
        "pick_from": "Discussion",
        "discuss_with": ["anyone"],
    })
}

/// The `finish` envelope's facts, as afkd writes them.
fn facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": 3, "tokens": null, "run_name": "260926-095800-card-Rk7eLy5w-1"})
}

fn finish(plugin: &mut Plugin, unit: &Value, outcome: &str, facts: Value) -> Value {
    plugin.call(
        json!({"call": "finish", "id": unit["id"], "key": unit["key"],
                       "outcome": outcome, "facts": facts}),
    )
}

/// The claim comments on a card.
fn claims(fake: &FakeTrello, card: &str) -> Vec<common::fake::Comment> {
    fake.comments(card)
        .into_iter()
        .filter(|c| c.text.starts_with("[afkd-claim]"))
        .collect()
}

/// The texts on a card starting with `prefix`.
fn said(fake: &FakeTrello, card: &str, prefix: &str) -> Vec<String> {
    fake.comments(card)
        .into_iter()
        .filter(|c| c.text.starts_with(prefix))
        .map(|c| c.text)
        .collect()
}

// --- hello ---

#[test]
fn hello_arms_and_lists_every_call_it_answers() {
    let b = board();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello(settings(&b.fake)),
        json!({"ok": true, "proto": 1,
               "calls": ["release", "renew", "comments", "attempt_failed", "classify"]})
    );
    assert!(b.fake.seen().is_empty(), "hello touches no board");
    plugin.finish();
}

/// What `hello` cannot arm with answers `ok:false`, with the problem on stderr in the
/// built-in's own sentence — a kind this plugin does not provide, a block the manifest
/// cannot refuse, a `hello` from an afkd too old to say who is asking, and a protocol
/// from a later afkd.
#[test]
fn hello_refuses_what_it_cannot_arm_with() {
    let b = board();
    let hello = |kind: &str, proto: u32, settings: Value| {
        json!({"call": "hello", "proto": proto, "kind": kind, "service": DEVELOP,
               "roster": [DEVELOP, DISCUSS], "owner": OWNER, "settings": settings})
    };
    let mut flag_gate = settings(&b.fake);
    flag_gate["require_label"] = json!(true);
    let mut anonymous = hello("trello", 1, settings(&b.fake));
    for field in ["service", "roster", "owner"] {
        anonymous.as_object_mut().unwrap().remove(field);
    }
    for (request, sentence) in [
        (
            hello("gitea", 1, settings(&b.fake)),
            "kind `gitea` is not provided by @afkd/trello",
        ),
        (
            hello("trello", 1, flag_gate),
            "trigger trello: setting `require_label`: setting `require_label` expects a single \
             value",
        ),
        (
            anonymous,
            "afkd did not say which service this is; @afkd/trello needs an afkd whose `hello` \
             carries `service`, `roster` and `owner`",
        ),
        (
            hello("trello", 2, settings(&b.fake)),
            "afkd speaks plugin protocol 2, and this plugin speaks 1",
        ),
    ] {
        let mut plugin = Plugin::spawn();
        assert_eq!(plugin.call(request), json!({"ok": false, "proto": 1}));
        assert_eq!(
            plugin.stderr_soon(sentence),
            format!("afkd-trello: {sentence}\n")
        );
        plugin.finish();
    }
    assert!(b.fake.seen().is_empty(), "a refused hello touches no board");
}

// --- poll ---

/// The won race hands over exactly the unit the built-in would have run — its journal
/// key and session thread, the claim-read thread as `seen`, afkd's member id, the four
/// env names the skill reads, and the brief unframed — and the board holds the claim as
/// the built-in leaves it, `on_claim` run, every line narrated on stderr.
#[test]
fn a_won_race_hands_over_the_built_ins_unit() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let claim = claims(&b.fake, &b.card);
    assert_eq!(claim.len(), 1, "one claim, ours");
    let claim = &claim[0];
    assert_eq!(claim.text, "[afkd-claim] owner=afkd-4242");
    assert_eq!(claim.author, b.fake.me());
    assert_eq!(
        reply["unit"],
        json!({
            "id": SHORT_LINK,
            "key": format!("{}#{}", b.card, claim.id),
            "thread": SHORT_LINK,
            "seen": [claim.id, b.asked[1], b.asked[0]],
            "self": b.fake.me(),
            "env": {
                "TRELLO_API_KEY": KEY,
                "TRELLO_BOARD_ID": BOARD,
                "TRELLO_CARD_ID": b.card,
                "TRELLO_TOKEN": TOKEN,
            },
            "files": [{"path": "task.md", "text": format!(
                "{TITLE}\n\n{BODY}\n\n## Checklists\n\n### Acceptance (1/2)\
                 \n- [x] retry stops after 5 tries · id: {}\
                 \n- [ ] backoff is capped at 30s · id: {}\
                 \n\n## New comments\n\n**Phil Ek:** {PHIL}\n\n**陳大文:** {CHEN}\n",
                b.items[0], b.items[1]
            )}],
        })
    );

    assert_eq!(b.fake.list_of(&b.card), "In Progress");
    assert_eq!(b.fake.members(&b.card), ["afkd-bot"]);
    assert_eq!(
        plugin.stderr_soon("In Progress"),
        format!(
            "[trello] claimed card \"{TITLE}\"\n\
             [trello] adding member \"self\" to card \"{TITLE}\"\n\
             [trello] moving card \"{TITLE}\" to list \"In Progress\" (at top)\n"
        )
    );
    // The race is post → settle → re-read: the claim went up before the thread was read.
    let seen = b.fake.seen();
    let comments_path = format!("/1/cards/{}/actions", b.card);
    let post = seen
        .iter()
        .position(|r| r.method == "POST" && r.path == format!("{comments_path}/comments"))
        .expect("the claim was posted");
    assert!(seen[post + 1..]
        .iter()
        .any(|r| r.method == "GET" && r.path == comments_path));
    // The comment rides the body, never the request line.
    assert_eq!(
        seen[post].body,
        r#"{"text":"[afkd-claim] owner=afkd-4242"}"#
    );
    // And the claimed card, now out of `pick_from`, is not claimed again.
    assert_eq!(plugin.poll(), json!({"fire": false}));
    plugin.finish();
}

/// A rival's claim that lands a second ahead of ours wins the race: our claim is taken
/// back, the rival's stays, and the scan moves on to the next card in the same beat.
#[test]
fn a_lost_race_takes_its_marker_back_and_moves_on() {
    let b = board();
    let next = b
        .fake
        .card("Up for Grabs", "Nx2Pq8Za", "Cap the backoff at 30s", "");
    b.fake.race_on_next_claim(&b.card);
    let mut plugin = Plugin::armed(settings(&b.fake));

    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], "Nx2Pq8Za");
    // A bodyless card briefs with its title alone.
    assert_eq!(unit["files"][0]["text"], "Cap the backoff at 30s");
    let left: Vec<String> = claims(&b.fake, &b.card)
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(
        left,
        ["[afkd-claim] owner=afkd-17"],
        "only the rival's claim is left"
    );
    assert_eq!(
        b.fake.list_of(&b.card),
        "Up for Grabs",
        "no on_claim ran on it"
    );
    assert_eq!(b.fake.list_of(&next), "In Progress");
    plugin.finish();
}

/// A card someone holds a live claim on is stepped over in silence: nothing is posted to
/// it, and the card beneath is claimed.
#[test]
fn a_live_claim_is_stepped_over_in_silence() {
    let b = board();
    let next = b
        .fake
        .card("Up for Grabs", "Nx2Pq8Za", "Cap the backoff at 30s", "");
    let rival = b.fake.member("rival-host-2", "Rival Two");
    b.fake
        .comment(&b.card, &rival, "[afkd-claim] owner=afkd-17 renewal=3", 5);
    let mut plugin = Plugin::armed(settings(&b.fake));

    assert_eq!(plugin.poll()["unit"]["thread"], "Nx2Pq8Za");
    assert!(
        b.fake
            .seen()
            .iter()
            .all(|r| !(r.method != "GET" && r.path.contains(&b.card))),
        "nothing was written to the held card: {:?}",
        b.fake.seen()
    );
    assert_eq!(b.fake.list_of(&next), "In Progress");
    plugin.finish();
}

/// A board that cannot be read is the built-in's idle beat, said on stderr; the next
/// beat, with the board back, claims.
#[test]
fn a_board_error_is_an_idle_beat_and_the_next_poll_claims() {
    let b = board();
    b.fake.fail("list cards", 500);
    let mut plugin = Plugin::armed(settings(&b.fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        plugin.stderr_soon("list cards"),
        "afkd-trello: trello list cards: board returned status 500\n"
    );
    b.fake.heal("list cards");
    assert_eq!(plugin.poll()["unit"]["thread"], SHORT_LINK);
    plugin.finish();
}

// --- renew, comments, attempt_failed, classify ---

#[test]
fn renew_edits_the_marker_in_place() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let before = claims(&b.fake, &b.card)[0].clone();
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = claims(&b.fake, &b.card)[0].clone();
    assert_eq!(after.id, before.id, "edited, not re-posted");
    assert_eq!(after.text, "[afkd-claim] owner=afkd-4242 renewal=1");
    assert!(after.edited.is_some(), "Trello stamped the edit");
    let edit = b.fake.seen().last().cloned().unwrap();
    assert_eq!(
        (edit.method.as_str(), edit.path.as_str()),
        (
            "PUT",
            format!("/1/cards/{}/actions/{}/comments", b.card, before.id).as_str()
        )
    );
    // A key it does not hold is declined, not guessed at.
    assert_eq!(
        plugin.call(
            json!({"call": "renew", "key": format!("{}#{}", b.card, "6a4dd5ff1234abcd5678ef91"),
                           "renewal": 1})
        ),
        json!({"ok": false})
    );
    plugin.finish();
}

/// `comments` reports what afkd has not been told about, each as afkd's watch reads it:
/// what the brief carried, afkd's own words and every control marker are left out; a
/// second read carries only what is new; and a board that cannot be read is `null`, not
/// an empty thread.
#[test]
fn comments_report_only_what_afkd_has_not_seen() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let read = |plugin: &mut Plugin| plugin.call(json!({"call": "comments", "key": unit["key"]}));

    let late = b.fake.comment(
        &b.card,
        &b.chen,
        "别急着移走 🚨\n\n    max_backoff = 30\n",
        2,
    );
    b.fake.comment(&b.card, &b.fake.me(), "Looking into it.", 1);
    b.fake.comment(
        &b.card,
        &b.phil,
        "[afkd-attempt] 1/2: pasted from another card",
        1,
    );
    let rival = b.fake.member("rival-host-2", "Rival Two");
    b.fake
        .comment(&b.card, &rival, "[afkd-claim] owner=afkd-17", 1);
    let alvaro = b.fake.member("alvaro", "Álvaro Pérez");
    let also = b
        .fake
        .comment(&b.card, &alvaro, "Exponential, please — see §4 🙏", 0);
    let at = |id: &str| {
        let c = b
            .fake
            .comments(&b.card)
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        utc(c.posted)
    };
    assert_eq!(
        read(&mut plugin),
        json!({"comments": [
            {"id": late, "author": b.chen, "author_name": "陳大文",
             "body": "别急着移走 🚨\n\n    max_backoff = 30\n", "at": at(&late)},
            {"id": also, "author": alvaro, "author_name": "Álvaro Pérez",
             "body": "Exponential, please — see §4 🙏", "at": at(&also)},
        ]})
    );

    let more = b.fake.comment(&b.card, &alvaro, "…and cap it at 30s.", 0);
    let ids: Vec<String> = read(&mut plugin)["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [more]);

    b.fake.fail("read comments", 500);
    assert_eq!(read(&mut plugin), json!({"comments": null}));
    b.fake.heal("read comments");
    assert_eq!(read(&mut plugin), json!({"comments": []}));
    assert!(
        plugin
            .stderr_soon("read comments")
            .contains("afkd-trello: trello read comments: board returned status 500"),
        "{}",
        plugin.stderr()
    );
    plugin.finish();
}

#[test]
fn attempt_failed_marks_the_card() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let reason =
        "run_cmd `cargo test` failed: 101\n\n  thread 'retry' panicked at 看 src/retry.rs:42";
    assert_eq!(
        plugin.call(
            json!({"call": "attempt_failed", "key": unit["key"], "n": 1, "max": 2,
                           "reason": reason})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        plugin.call(
            json!({"call": "attempt_failed", "key": unit["key"], "n": 2, "max": 2,
                           "reason": null})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        said(&b.fake, &b.card, "[afkd-attempt]"),
        [format!("[afkd-attempt] 1/2: {reason}")],
        "a fault with no sentence marks nothing"
    );
    plugin.finish();
}

#[test]
fn classify_parks_on_the_marker_and_echoes_otherwise() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let scratch = TempDir::new("classify");
    let classify = |plugin: &mut Plugin, outcome: &str| {
        plugin.call(json!({"call": "classify", "key": format!("{}#1", b.card),
                           "scratch": scratch.path(), "outcome": outcome}))
    };
    assert_eq!(classify(&mut plugin, "clean"), json!({"outcome": "clean"}));
    assert_eq!(
        classify(&mut plugin, "failed"),
        json!({"outcome": "failed"})
    );
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(classify(&mut plugin, "failed"), json!({"outcome": "park"}));
    assert_eq!(classify(&mut plugin, "clean"), json!({"outcome": "park"}));
    assert!(
        b.fake.seen().is_empty(),
        "classify reads the scratch dir, not the board"
    );
    plugin.finish();
}

// --- release ---

/// A crashed run's claim is reversed — the claim deleted and the card moved to the
/// bottom of `pick_from`, where the poll finds it again; a unit afkd hands straight back
/// is reversed and forgotten; and a key with no `#` names nothing.
#[test]
fn release_reverses_a_crashed_claim_and_reads_every_key_shape() {
    let b = board();
    let crashed = b
        .fake
        .card("In Progress", "Cr4shd00", "Crashed mid-run", "");
    let claim = b.fake.comment(
        &crashed,
        &b.fake.me(),
        "[afkd-claim] owner=afkd-17 renewal=4",
        900,
    );
    let mut plugin = Plugin::armed(settings(&b.fake));

    assert_eq!(
        plugin.call(json!({"call": "release", "key": format!("{crashed}#{claim}")})),
        json!({"released": true})
    );
    assert!(claims(&b.fake, &crashed).is_empty());
    assert_eq!(
        b.fake.cards_in("Up for Grabs"),
        [b.card.clone(), crashed.clone()]
    );

    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], SHORT_LINK);
    assert_eq!(
        plugin.call(json!({"call": "release", "key": unit["key"]})),
        json!({"released": true})
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    assert_eq!(b.fake.cards_in("Up for Grabs"), [crashed, b.card.clone()]);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": false}),
        "a released unit is forgotten"
    );

    for key in ["garbage", ""] {
        assert_eq!(
            plugin.call(json!({"call": "release", "key": key})),
            json!({"released": null})
        );
    }
    plugin.finish();
}

// --- finish, per outcome ---

/// A clean finish runs `on_done` with the run's facts substituted, posts one `[afkd-ran]`
/// watermark carrying the boundary fixed at the claim read (pruning the prior one), and
/// releases the claim last.
#[test]
fn a_clean_finish_runs_on_done_and_leaves_one_watermark() {
    let b = board();
    let phil_at = b.fake.comments(&b.card)[0].posted;
    let prior = b.fake.comment(
        &b.card,
        &b.fake.me(),
        &format!("[afkd-ran] owner=afkd-17 upto={phil_at}"),
        2700,
    );
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    // The prior watermark delivered Phil's comment; only Chen's is new.
    let brief = unit["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with(&format!(
            "## Earlier conversation\n\n**Phil Ek:** {PHIL}\n\n\n## New comments\n\n**陳大文:** {CHEN}\n"
        )),
        "{brief}"
    );

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(b.fake.list_of(&b.card), "Review");
    assert_eq!(
        said(&b.fake, &b.card, "afkd landed"),
        ["afkd landed this card in 2m48s - $0.42, 3 agent turns."]
    );
    let chen_at = b
        .fake
        .comments(&b.card)
        .into_iter()
        .find(|c| c.id == b.asked[1])
        .unwrap()
        .posted;
    assert_eq!(
        said(&b.fake, &b.card, "[afkd-ran]"),
        [format!("[afkd-ran] owner=afkd-4242 upto={chen_at}")],
        "one watermark, the new one"
    );
    assert!(b.fake.comments(&b.card).iter().all(|c| c.id != prior));
    assert!(claims(&b.fake, &b.card).is_empty());
    assert!(plugin
        .stderr_soon("released claim")
        .ends_with(&format!("[trello] released claim on card \"{TITLE}\"\n")));
    plugin.finish();
}

#[test]
fn a_failed_finish_runs_on_fail() {
    let b = board();
    let queued = b
        .fake
        .card("Backlog", "Qu3u3d00", "Already in the backlog", "");
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        finish(
            &mut plugin,
            &unit,
            "failed",
            facts("fault", Some("cargo test: 3 failed")),
        ),
        json!({"ok": true})
    );
    assert_eq!(
        b.fake.cards_in("Backlog"),
        [queued, b.card.clone()],
        "at the bottom"
    );
    assert_eq!(b.fake.labels(&b.card), ["Problem"]);
    assert_eq!(said(&b.fake, &b.card, "[afkd-ran]").len(), 1);
    assert!(claims(&b.fake, &b.card).is_empty());
    plugin.finish();
}

/// A park badges the card itself, runs the `on_park` extras, posts the owner marker
/// naming the configured service, and only then releases the claim — leaving the card
/// where `on_claim` put it.
#[test]
fn a_park_finish_badges_marks_and_releases() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let scratch = TempDir::new("park");
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(
        plugin.call(
            json!({"call": "classify", "key": unit["key"], "scratch": scratch.path(),
                           "outcome": "failed"})
        ),
        json!({"outcome": "park"})
    );
    assert_eq!(
        finish(
            &mut plugin,
            &unit,
            "park",
            facts("fault", Some("parked: awaiting a human reply")),
        ),
        json!({"ok": true})
    );
    assert_eq!(b.fake.labels(&b.card), ["Awaiting Reply"]);
    assert_eq!(
        b.fake.list_of(&b.card),
        "In Progress",
        "a park does not move"
    );
    assert_eq!(
        said(&b.fake, &b.card, "parked after"),
        ["parked after 2m48s, waiting on you"]
    );
    assert_eq!(
        said(&b.fake, &b.card, "[afkd-park]"),
        ["[afkd-park] service=afkd::develop"]
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    // The release is the last write: every post a park owes is up before the claim goes.
    let writes: Vec<String> = b
        .fake
        .seen()
        .into_iter()
        .filter(|r| r.method == "POST" || r.method == "DELETE")
        .map(|r| format!("{} {}", r.method, r.path.rsplit('/').next().unwrap_or("")))
        .collect();
    assert_eq!(writes.last().map(String::as_str), Some("DELETE comments"));
    assert!(plugin.stderr_soon("parked card").contains(&format!(
        "[trello] parked card \"{TITLE}\" awaiting a reply (label \"Awaiting Reply\")\n"
    )));
    plugin.finish();
}

/// On the `discuss_with` path a turn that said nothing is backstopped, so afkd is the
/// last speaker and the card does not re-fire; no watermark is posted.
#[test]
fn a_discuss_turn_that_said_nothing_is_backstopped() {
    let b = board();
    let groomed = b
        .fake
        .card("Discussion", "Gr00md00", "Should we cap the backoff?", "");
    b.fake.comment(&groomed, &b.phil, "What do you think?", 60);
    let mut plugin = Plugin::armed_as(DISCUSS, discuss_settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], "Gr00md00");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    let mine: Vec<String> = b
        .fake
        .comments(&groomed)
        .into_iter()
        .filter(|c| c.author == b.fake.me())
        .map(|c| c.text)
        .collect();
    assert_eq!(mine, ["reviewed, nothing to add"]);
    assert!(said(&b.fake, &groomed, "[afkd-ran]").is_empty());
    assert_eq!(plugin.poll(), json!({"fire": false}), "afkd spoke last");
    plugin.finish();
}

/// A terminal lifecycle that does not land is `held`: the claim stays on the card, and
/// afkd's later `release` of the key replays what is owed — `false` while the board is
/// still down, `true` once it lands. A child that did not hold the finish — afkd
/// restarted in between — takes the crash reversal for the same key instead.
#[test]
fn an_undelivered_finish_is_held_and_release_delivers_it() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let key = unit["key"].as_str().unwrap().to_string();
    b.fake.fail("move card", 500);

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true, "held": true})
    );
    assert!(
        plugin
            .stderr_soon("afkd holds the claim")
            .ends_with(&format!(
                "afkd-trello: trello move card: board returned status 500\n\
             afkd-trello: could not deliver the terminal lifecycle for {key}; afkd holds the \
             claim and releases it on a later beat\n"
            )),
        "{}",
        plugin.stderr()
    );
    assert_eq!(claims(&b.fake, &b.card).len(), 1, "the lease is held");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": false}),
        "still owed"
    );
    assert_eq!(claims(&b.fake, &b.card).len(), 1);

    b.fake.heal("move card");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert_eq!(b.fake.list_of(&b.card), "Review");
    assert!(claims(&b.fake, &b.card).is_empty());
    assert_eq!(
        said(&b.fake, &b.card, "afkd landed").len(),
        1,
        "posted once"
    );
    plugin.finish();

    // After a restart: the same shape, held by a child that is gone.
    let b = board();
    let mut before = Plugin::armed(settings(&b.fake));
    let unit = before.poll()["unit"].clone();
    let key = unit["key"].as_str().unwrap().to_string();
    b.fake.fail("move card", 500);
    assert_eq!(
        finish(&mut before, &unit, "clean", facts("proceed", None)),
        json!({"ok": true, "held": true})
    );
    before.finish();
    b.fake.heal("move card");
    let mut after = Plugin::armed(settings(&b.fake));
    assert_eq!(
        after.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    assert_eq!(
        b.fake.cards_in("Up for Grabs").last(),
        Some(&b.card),
        "reversed to the bottom of pick_from"
    );
    after.finish();
}

// --- the park owner across two services ---

/// Two services on one board, each its own child: `afkd::develop` parks a card and a
/// human answers it. `afkd::discuss` sweeps it first and leaves it alone — nothing
/// written, badge and marker kept — and `afkd::develop` resumes it, taking both off. A
/// card parked by `afkd::gone`, which the roster no longer holds, is taken over by
/// whoever sweeps it, with one line saying so.
#[test]
fn the_park_owner_holds_across_two_services_on_one_board() {
    let b = board();
    let mut develop = Plugin::armed(settings(&b.fake));
    let mut discuss = Plugin::armed_as(DISCUSS, discuss_settings(&b.fake));

    let unit = develop.poll()["unit"].clone();
    let scratch = TempDir::new("owner");
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(
        develop.call(
            json!({"call": "classify", "key": unit["key"], "scratch": scratch.path(),
                            "outcome": "clean"})
        )["outcome"],
        "park"
    );
    b.fake
        .comment(&b.card, &b.fake.me(), "Which header — `Retry-After`? 🙏", 0);
    assert_eq!(
        finish(&mut develop, &unit, "park", facts("fault", Some("parked"))),
        json!({"ok": true})
    );
    b.fake.comment(
        &b.card,
        &b.phil,
        "Yes, `Retry-After` — seconds, not a date.",
        0,
    );

    let (comments, labels) = (b.fake.comments(&b.card), b.fake.labels(&b.card));
    let writes = |fake: &FakeTrello| fake.seen().iter().filter(|r| r.method != "GET").count();
    let before = writes(&b.fake);
    assert_eq!(
        discuss.poll(),
        json!({"fire": false}),
        "not the discusser's card"
    );
    assert_eq!(writes(&b.fake), before, "the discusser wrote nothing");
    assert_eq!(b.fake.comments(&b.card), comments);
    assert_eq!(b.fake.labels(&b.card), labels, "still badged");

    let resumed = develop.poll()["unit"].clone();
    assert_eq!(resumed["thread"], SHORT_LINK, "its parker resumes it");
    assert!(b.fake.labels(&b.card).is_empty(), "the badge comes off");
    assert!(
        said(&b.fake, &b.card, "[afkd-park]").is_empty(),
        "and the marker"
    );
    let brief = resumed["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with(
            "## New comments\n\n**Phil Ek:** Yes, `Retry-After` — seconds, not a date.\n"
        ),
        "{brief}"
    );

    // An orphan: parked by a service this daemon no longer runs.
    let orphan = b
        .fake
        .card("In Progress", "0rphan00", "Groom the retry docs", "");
    b.fake.label(&orphan, "Awaiting Reply");
    b.fake
        .comment(&orphan, &b.fake.me(), "Should the docs cover jitter?", 120);
    b.fake
        .comment(&orphan, &b.fake.me(), "[afkd-park] service=afkd::gone", 110);
    b.fake.comment(&orphan, &b.chen, "Yes — jitter too.", 60);
    assert_eq!(discuss.poll()["unit"]["thread"], "0rphan00");
    assert!(discuss.stderr_soon("no longer runs").contains(
        "[trello] resuming card \"Groom the retry docs\", parked by service \"afkd::gone\" \
         which this config no longer runs\n"
    ));
    assert!(b.fake.labels(&orphan).is_empty());
    develop.finish();
    discuss.finish();
}

// --- the line budget ---

/// A 100 KiB card description cannot cross in one 64 KiB line: the brief is cut, the
/// line fits and decodes, and the brief says where the rest is.
#[test]
fn an_oversized_brief_is_cut_to_fit_one_line() {
    let fake = FakeTrello::start();
    fake.list("Up for Grabs");
    fake.list("In Progress");
    let body = "看起来不对 🚨 — the retry path.\n".repeat(2_600);
    assert!(body.len() > 100 * 1024);
    fake.card("Up for Grabs", SHORT_LINK, TITLE, &body);
    let mut plugin = Plugin::armed(settings(&fake));
    let line = plugin.call_raw(json!({"call": "poll"}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.starts_with(&format!("{TITLE}\n\n看起来不对 🚨")),
        "{}",
        &brief[..80]
    );
    assert!(
        brief.ends_with("read the whole card with the trello skill]"),
        "{}",
        &brief[brief.len() - 200..]
    );
    assert!(plugin
        .stderr_soon("cut to fit")
        .contains(&format!("the brief for {SHORT_LINK} was cut to fit")));
    plugin.finish();
}

/// 400 comments of 300 bytes are over the line too: the reply keeps the newest that fit.
#[test]
fn an_overflowing_thread_is_cut_to_fit_one_line() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let mut last = String::new();
    for _ in 0..400 {
        last = b.fake.comment(&b.card, &b.phil, &"x".repeat(300), 0);
    }
    let line = plugin.call_raw(json!({"call": "comments", "key": unit["key"]}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    let kept = reply["comments"].as_array().unwrap();
    assert!(kept.len() > 100 && kept.len() < 400, "{}", kept.len());
    assert_eq!(kept.last().unwrap()["id"], last, "the newest survive");
    let stderr = plugin.stderr_soon("did not fit");
    assert!(
        stderr.contains("did not fit afkd's 64 KiB plugin line"),
        "{stderr}"
    );
    plugin.finish();
}
