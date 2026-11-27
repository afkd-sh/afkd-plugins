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

/// The live selfdev service's settings, lowered to JSON as afkd lowers them: every value a
/// string, and no slot, since afkd runs those.
fn settings(fake: &FakeTrello) -> Value {
    json!({
        "board": format!("https://trello.com/b/{BOARD}/afkd-selfdev"),
        "base_url": fake.base_url(),
        "api_key": KEY,
        "token": TOKEN,
        "claim_from": "Up for Grabs",
    })
}

/// One beat that hands a unit over. The slots afkd runs on it reach the board through
/// `trello.Account` calls, which `writes.rs`'s legs hold to what they do; a leg here that
/// depends on where a slot left the card puts it there itself, on the fake.
fn claim_one(plugin: &mut Plugin) -> Value {
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    reply["unit"].clone()
}

/// The grooming service's settings: `discuss_with [ "anyone" ]` over its own list.
fn discuss_settings(fake: &FakeTrello) -> Value {
    json!({
        "board": format!("https://trello.com/b/{BOARD}/afkd-selfdev"),
        "base_url": fake.base_url(),
        "api_key": KEY,
        "token": TOKEN,
        "claim_from": "Discussion",
        "discuss_with": ["anyone"],
    })
}

/// When the object an ObjectId names was made, in the unix milliseconds a `time` field is.
fn made(id: &str) -> u64 {
    u64::from_str_radix(&id[..8], 16).expect("an ObjectId") * 1000
}

/// Every field the `Card` handle declares for the suite's own card as the board holds it now,
/// in the unix milliseconds a `time` is: its labels and members as the caller says, the list
/// it is in, and the last activity the fake has stamped on it.
fn card_fields(b: &Board, list: &str, labels: &[&str], members: &[&str]) -> Value {
    json!({
        "number": 1,
        "name": TITLE,
        "url": b.fake.url(&b.card),
        "description": BODY,
        "board": BOARD,
        "list": list,
        "labels": labels,
        "members": members,
        "due": null,
        "due_complete": false,
        "archived": false,
        "created_at": made(&b.card),
        "last_activity_at": b.fake.last_activity(&b.card) * 1000,
    })
}

/// The `finish` envelope's facts, as afkd writes them: every field, though the plugin
/// reads only the signal and the reason.
fn facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": 3, "tokens": null, "run_name": "260926-095800-unit-Rk7eLy5w-1"})
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

/// The accepted `hello` lists every optional call it answers.
#[test]
fn hello_arms_and_lists_every_call_it_answers() {
    let b = board();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello(settings(&b.fake)),
        json!({"ok": true, "proto": 2,
               "calls": ["release", "renew", "comments", "attempt_failed", "classify"]})
    );
    assert!(b.fake.seen().is_empty(), "hello touches no board");
    plugin.finish();
}

/// What `hello` cannot arm with answers `ok:false`, with the problem on stderr in the
/// built-in's own sentence — a kind this plugin does not provide (proto 1's
/// `trello_card` and the `card` the kind was called before it was `intake` among them), a block the manifest cannot refuse, a block without the
/// `claim_from` afkd requires — one that writes the old `pick_from` in its place among them —
/// a `hello` from an afkd too old to say who is asking, and the protocol of an afkd from
/// before v2 of the afk language.
#[test]
fn hello_refuses_what_it_cannot_arm_with() {
    let b = board();
    let hello = |kind: &str, proto: u32, settings: Value| {
        json!({"call": "hello", "proto": proto, "kind": kind, "service": DEVELOP,
               "roster": [DEVELOP, DISCUSS], "owner": OWNER, "settings": settings})
    };
    let mut flag_gate = settings(&b.fake);
    flag_gate["only_labels"] = json!(true);
    // The old name where `claim_from` belongs: afkd refuses it as unknown, and a `hello`
    // that carries it anyway has no `claim_from`.
    let mut pick_from_for_claim_from = settings(&b.fake);
    let block = pick_from_for_claim_from.as_object_mut().unwrap();
    block.remove("claim_from");
    block.insert("pick_from".into(), json!("Up for Grabs"));
    let mut anonymous = hello("intake", 2, settings(&b.fake));
    for field in ["service", "roster", "owner"] {
        anonymous.as_object_mut().unwrap().remove(field);
    }
    for (request, sentence) in [
        (
            hello("gitea_issue", 2, settings(&b.fake)),
            "kind `gitea_issue` is not provided by @afkd/trello",
        ),
        (
            hello("trello_card", 2, settings(&b.fake)),
            "kind `trello_card` is not provided by @afkd/trello",
        ),
        (
            hello("card", 2, settings(&b.fake)),
            "kind `card` is not provided by @afkd/trello",
        ),
        (
            hello("intake", 2, flag_gate),
            "trigger intake: setting `only_labels`: `only_labels` expects one or more label \
             names",
        ),
        (
            anonymous,
            "afkd did not say which service this is; @afkd/trello needs an afkd whose `hello` \
             carries `service`, `roster` and `owner`",
        ),
        (
            hello("trello_card", 1, settings(&b.fake)),
            "afkd speaks plugin protocol 1, and this plugin speaks 2",
        ),
        (
            hello("intake", 2, pick_from_for_claim_from),
            "trigger intake: setting `claim_from`: `claim_from` is required: name the list or \
             lists new cards are claimed from",
        ),
    ] {
        let mut plugin = Plugin::spawn();
        assert_eq!(plugin.call(request), json!({"ok": false, "proto": 2}));
        assert_eq!(
            plugin.stderr_soon(sentence),
            format!("afkd-trello: {sentence}\n")
        );
        plugin.finish();
    }
    assert!(b.fake.seen().is_empty(), "a refused hello touches no board");
}

// --- poll ---

/// The won race hands over exactly the unit the built-in would have run — its session
/// thread, the claim-read thread as `seen`, afkd's member id, the four env names the skill
/// reads, and the brief unframed — keyed by the card, `trello:<card id>`, with every field the
/// manifest's `Card` declares, its labels by name and a colour-only one left out, and the board
/// holds the claim as the built-in leaves it. The `poll` moves nothing itself — the slots afkd
/// runs after it do their own work through account calls — and the win is narrated on stderr.
#[test]
fn a_won_race_hands_over_the_built_ins_unit() {
    let b = board();
    for label in ["afkd/ready", "", "Väntar på svar"] {
        b.fake.label(&b.card, label);
    }
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
            "key": format!("trello:{}", b.card),
            "fields": card_fields(&b, "Up for Grabs", &["afkd/ready", "Väntar på svar"], &[]),
            "thread": SHORT_LINK,
            "seen": [claim.id, b.asked[1], b.asked[0]],
            "self": b.fake.me(),
            "env": {
                "TRELLO_API_KEY": KEY,
                "TRELLO_BOARD_ID": BOARD,
                "TRELLO_CARD_ID": b.card,
                "TRELLO_TOKEN": TOKEN,
            },
            "args": {"account": b.fake.account()},
            "files": [{"path": "task/README.md", "text": format!(
                "{TITLE}\n\n{BODY}\n\n## Checklists\n\n### Acceptance (1/2)\
                 \n- [x] retry stops after 5 tries · id: {}\
                 \n- [ ] backoff is capped at 30s · id: {}\
                 \n\n## New comments\n\n**Phil Ek:** {PHIL}\n\n**陳大文:** {CHEN}\n",
                b.items[0], b.items[1]
            )}],
        })
    );

    assert_eq!(
        b.fake.list_of(&b.card),
        "Up for Grabs",
        "the poll moves nothing"
    );
    assert!(b.fake.members(&b.card).is_empty());
    assert_eq!(
        plugin.stderr_soon("claimed card"),
        format!("[trello] claimed card \"{TITLE}\"\n")
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
    // And the claimed card, holding our live claim, is stepped over.
    assert_eq!(plugin.poll(), json!({"fire": false}));
    plugin.finish();
}

/// A card with no label, member or due date still sends every field — its `labels` and
/// `members` empty, its `due` `null` — afkd finds every declared field there whatever the
/// board holds.
#[test]
fn an_unlabelled_card_hands_over_its_labels_empty() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    assert_eq!(
        reply["unit"]["fields"],
        card_fields(&b, "Up for Grabs", &[], &[])
    );
    plugin.finish();
}

/// A claimed card carries every field the `Card` handle declares, filled from Trello as the
/// board holds the card once the claim is won: its list's name and board's short link, its
/// members' usernames, its due date and whether that is complete, its `idShort` as `number`,
/// and its last activity — which the claim comment itself moved on. Each field is the one a
/// read of the same card gives, so the two handles are `==`.
#[test]
fn a_claimed_card_carries_every_card_field() {
    let b = board();
    b.fake.label(&b.card, "afkd/ready");
    b.fake.card_member(&b.card, &b.phil);
    b.fake.card_member(&b.card, &b.chen);
    b.fake.due(&b.card, 86_400 * 3, true);
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = plugin.poll()["unit"].clone();
    let mut want = card_fields(&b, "Up for Grabs", &["afkd/ready"], &["phil", "chen"]);
    want["due"] = json!(b.fake.due_of(&b.card).expect("a due date") * 1000);
    want["due_complete"] = json!(true);
    assert_eq!(unit["fields"], want);
    // The same card read as `trello.cards` reads it: the same handle, field for field.
    let read = plugin.read_armed(
        "cards",
        json!({"acct": b.fake.account(), "board": BOARD, "limit": 1}),
    );
    assert_eq!(read["ok"], true, "{}", plugin.stderr());
    assert_eq!(
        read["handles"][0],
        json!({"id": SHORT_LINK, "key": unit["key"], "fields": want})
    );
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

    let unit = claim_one(&mut plugin);
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
    assert_eq!(b.fake.list_of(&b.card), "Up for Grabs", "nothing moved it");
    assert_eq!(b.fake.list_of(&next), "Up for Grabs");
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

    assert_eq!(claim_one(&mut plugin)["thread"], "Nx2Pq8Za");
    assert!(
        b.fake
            .seen()
            .iter()
            .all(|r| !(r.method != "GET" && r.path.contains(&b.card))),
        "nothing was written to the held card: {:?}",
        b.fake.seen()
    );
    assert_eq!(b.fake.list_of(&next), "Up for Grabs");
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
/// bottom of `claim_from`, where the poll finds it again; a unit afkd hands straight back
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

    let unit = claim_one(&mut plugin);
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

// --- several `claim_from` lists ---

/// A second source list beside the suite's board, its card claimable; the board's own card
/// in `Up for Grabs` is held back by a `skip_labels` gate. Returns the list's id and its
/// card's id.
fn second_list(b: &Board) -> (String, String) {
    b.fake.label(&b.card, "afkd/hold");
    let list = b.fake.list("Hjälpvideor 🎬");
    let card = b.fake.card(
        "Hjälpvideor 🎬",
        "Hj4lpV1d",
        "Hjälpvideo: så bokar du — steg för steg 🎬",
        "Två minuter, utan musik.\n\nManus i kommentarerna.",
    );
    (list, card)
}

/// The suite's settings with `claim_from` naming two lists, in the shape afkd lowers a
/// list setting to — one entry holding both names — and the gate that holds the first
/// list's card back.
fn two_list_settings(fake: &FakeTrello) -> Value {
    let mut settings = settings(fake);
    settings["claim_from"] = json!([["Up for Grabs", "Hjälpvideor 🎬"]]);
    settings["skip_labels"] = json!([["afkd/hold"]]);
    settings
}

/// With nothing claimable in the first list, the poll claims from the second, and the
/// key is the card's own — the list the card came from is the plugin's to keep.
#[test]
fn claim_from_names_two_lists_and_claims_from_the_second() {
    let b = board();
    let (_, card) = second_list(&b);
    let mut plugin = Plugin::armed(two_list_settings(&b.fake));

    let unit = claim_one(&mut plugin);
    assert_eq!(unit["id"], "Hj4lpV1d");
    let claim = claims(&b.fake, &card);
    assert_eq!(claim.len(), 1, "one claim, ours");
    assert_eq!(
        unit["key"],
        format!("trello:{card}"),
        "the key is the card's, whichever list it came from"
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    assert_eq!(b.fake.list_of(&card), "Hjälpvideor 🎬");
    assert_eq!(b.fake.list_of(&b.card), "Up for Grabs");
    plugin.finish();
}

/// The source list rides in the plugin's record of the claim, in the directory afkd names
/// for it, so a fresh process sharing that directory — the restart after a crash — returns a
/// card claimed from the second list to the bottom of that list, not to the first.
#[test]
fn release_after_a_restart_returns_a_second_list_card_to_its_list() {
    let b = board();
    let (_, card) = second_list(&b);
    let queued = b.fake.card("Hjälpvideor 🎬", "Qu3u3d00", "Nästa video", "");
    let state = TempDir::new("state");
    let mut before = Plugin::armed_with(
        DEVELOP,
        OWNER,
        Some(state.path()),
        two_list_settings(&b.fake),
    );
    let unit = claim_one(&mut before);
    assert_eq!(unit["id"], "Hj4lpV1d");
    // The slot afkd runs on the claim puts it out of `claim_from`.
    b.fake.move_card(&card, "In Progress");
    assert!(
        state.path().join("claims").is_dir(),
        "the record is written before the unit is handed over"
    );
    before.finish();

    let mut after = Plugin::armed_with(
        DEVELOP,
        OWNER,
        Some(state.path()),
        two_list_settings(&b.fake),
    );
    assert_eq!(
        after.call(json!({"call": "release", "key": unit["key"]})),
        json!({"released": true})
    );
    assert!(claims(&b.fake, &card).is_empty());
    assert_eq!(b.fake.cards_in("Hjälpvideor 🎬"), [queued, card]);
    assert_eq!(
        b.fake.cards_in("Up for Grabs"),
        std::slice::from_ref(&b.card)
    );
    assert_eq!(
        std::fs::read_dir(state.path().join("claims"))
            .unwrap()
            .count(),
        0,
        "the record goes with the release"
    );
    after.finish();
}

/// A restart with no record — the directory is not shared, or afkd named none, and the
/// process is armed under another claim owner — releases a `trello:` key by the rule that
/// holds without one: every claim this account's member posted is pruned but the ones
/// carrying the new owner, a rival member's claim is kept, and the card moves back to the
/// bottom of the `claim_from` list it sits in — here none, since the slot moved it out, so
/// the first.
#[test]
fn a_restart_with_no_record_prunes_the_first_processs_claim_and_keeps_a_rivals() {
    let b = board();
    let (_, card) = second_list(&b);
    let queued = b.fake.card("Hjälpvideor 🎬", "Qu3u3d00", "Nästa video", "");
    let mut before = Plugin::armed(two_list_settings(&b.fake));
    let unit = claim_one(&mut before);
    assert_eq!(unit["id"], "Hj4lpV1d");
    // The slot afkd runs on the claim puts it out of `claim_from`.
    b.fake.move_card(&card, "In Progress");
    before.finish();
    let rival = b.fake.member("rival-host", "Rival");
    let rivals = b
        .fake
        .comment(&card, &rival, "[afkd-claim] owner=afkd-17", 30);
    let ours = b.fake.comment(
        &card,
        &b.fake.me(),
        "[afkd-claim] owner=afkd-5151 renewal=2",
        20,
    );
    assert_eq!(claims(&b.fake, &card).len(), 3);

    let mut after = Plugin::armed_with(DEVELOP, "afkd-5151", None, two_list_settings(&b.fake));
    assert_eq!(
        after.call(json!({"call": "release", "key": unit["key"]})),
        json!({"released": true}),
        "{}",
        after.stderr()
    );
    let left: Vec<String> = claims(&b.fake, &card).into_iter().map(|c| c.id).collect();
    assert_eq!(
        left,
        [rivals, ours],
        "the first process's claim is the one gone"
    );
    assert_eq!(b.fake.cards_in("Hjälpvideor 🎬"), [queued]);
    assert_eq!(b.fake.cards_in("Up for Grabs"), [b.card.clone(), card]);
    assert!(b.fake.cards_in("In Progress").is_empty());
    after.finish();
}

/// A key an older release journaled, `card#claim#list`, is still released: its claim deleted
/// and the card returned to the list it names, with no record to read.
#[test]
fn an_older_releases_key_is_still_released() {
    let b = board();
    let (list, card) = second_list(&b);
    let queued = b.fake.card("Hjälpvideor 🎬", "Qu3u3d00", "Nästa video", "");
    let claim = b.fake.comment(
        &card,
        &b.fake.me(),
        "[afkd-claim] owner=afkd-17 renewal=3",
        600,
    );
    // The older release's `on_claim` moved it out of the list it was claimed from.
    b.fake.move_card(&card, "In Progress");
    let mut plugin = Plugin::armed(two_list_settings(&b.fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": format!("{card}#{claim}#{list}")})),
        json!({"released": true})
    );
    assert!(claims(&b.fake, &card).is_empty());
    assert_eq!(b.fake.cards_in("Hjälpvideor 🎬"), [queued, card]);
    plugin.finish();
}

/// Release the key of a card the sweep claimed in `plugin`: from `plugin` itself, or —
/// with `restart` — from a fresh process sharing the plugin's directory `state`, the plugin
/// that claimed it gone.
fn release_swept(b: &Board, plugin: Plugin, key: &Value, restart: bool, state: &TempDir) {
    let mut plugin = if restart {
        plugin.finish();
        Plugin::armed_with(
            DEVELOP,
            OWNER,
            Some(state.path()),
            two_list_settings(&b.fake),
        )
    } else {
        plugin
    };
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    plugin.finish();
}

/// The discusser's shape over two lists: a card claimed from the second list with no
/// `on_claim` to move it is parked where it sits, answered, and claimed again by the
/// sweep — whose record names the list, so a crash returns it to the bottom of that list,
/// before a restart or after one.
#[test]
fn a_parked_card_answered_in_the_second_list_goes_back_there() {
    for restart in [false, true] {
        let b = board();
        let (_, card) = second_list(&b);
        let queued = b.fake.card("Hjälpvideor 🎬", "Qu3u3d00", "Nästa video", "");
        let state = TempDir::new("state");
        let mut plugin = Plugin::armed_with(
            DEVELOP,
            OWNER,
            Some(state.path()),
            two_list_settings(&b.fake),
        );

        let reply = plugin.poll();
        assert_eq!(reply["fire"], true, "{}", plugin.stderr());
        let unit = reply["unit"].clone();
        assert_eq!(unit["id"], "Hj4lpV1d");
        assert_eq!(unit["key"], format!("trello:{card}"));
        let scratch = TempDir::new("park");
        std::fs::write(scratch.path().join("park"), b"").unwrap();
        assert_eq!(
            plugin.call(
                json!({"call": "classify", "key": unit["key"], "scratch": scratch.path(),
                               "outcome": "clean"})
            ),
            json!({"outcome": "park"})
        );
        b.fake.comment(
            &card,
            &b.fake.me(),
            "Ska videon visa appen eller webben? 📱",
            2,
        );
        assert_eq!(
            finish(&mut plugin, &unit, "park", facts("fault", Some("parked"))),
            json!({"ok": true})
        );
        assert_eq!(b.fake.labels(&card), ["Awaiting Reply"]);
        assert_eq!(
            b.fake.list_of(&card),
            "Hjälpvideor 🎬",
            "parked where it sat"
        );
        b.fake
            .comment(&card, &b.phil, "Appen — webben får en egen video.", 0);

        let swept = plugin.poll();
        assert_eq!(swept["fire"], true, "{}", plugin.stderr());
        let unit = swept["unit"].clone();
        assert_eq!(unit["id"], "Hj4lpV1d", "the sweep claims it again");
        assert!(b.fake.labels(&card).is_empty(), "the badge comes off");
        let claim = claims(&b.fake, &card);
        assert_eq!(claim.len(), 1, "one claim, ours");
        assert_eq!(unit["key"], format!("trello:{card}"));
        let record = std::fs::read_to_string(state.path().join(format!("claims/{card}.json")))
            .expect("the sweep's claim is recorded");
        let record: Value = serde_json::from_str(&record).unwrap();
        assert_eq!(
            record["list_id"],
            b.fake.list_id("Hjälpvideor 🎬"),
            "the record names the list the card sat in"
        );
        assert_eq!(record["claim_id"], claim[0].id);

        release_swept(&b, plugin, &unit["key"], restart, &state);
        assert!(claims(&b.fake, &card).is_empty());
        assert_eq!(
            b.fake.cards_in("Hjälpvideor 🎬"),
            [queued, card],
            "restart: {restart}"
        );
        assert_eq!(
            b.fake.cards_in("Up for Grabs"),
            std::slice::from_ref(&b.card)
        );
    }
}

/// A card parked outside every `claim_from` list — where the selfdev `on_claim` put it —
/// is recorded under no list when the sweep claims it, and goes back to the first list,
/// before a restart or after one.
#[test]
fn a_parked_card_answered_outside_claim_from_goes_to_the_first_list() {
    for restart in [false, true] {
        let b = board();
        second_list(&b);
        let state = TempDir::new("state");
        let card = b
            .fake
            .card("In Progress", "1nPr0g00", "Bakgrundsvideo: färjan ⛴️", "");
        b.fake.label(&card, "Awaiting Reply");
        b.fake
            .comment(&card, &b.fake.me(), "Med eller utan speakerröst?", 120);
        b.fake.comment(
            &card,
            &b.fake.me(),
            &format!("[afkd-park] service={DEVELOP}"),
            110,
        );
        b.fake.comment(&card, &b.phil, "Utan — bara musik.", 60);
        let mut plugin = Plugin::armed_with(
            DEVELOP,
            OWNER,
            Some(state.path()),
            two_list_settings(&b.fake),
        );

        let swept = plugin.poll();
        assert_eq!(swept["fire"], true, "{}", plugin.stderr());
        let unit = swept["unit"].clone();
        assert_eq!(unit["id"], "1nPr0g00");
        assert_eq!(claims(&b.fake, &card).len(), 1);
        assert_eq!(unit["key"], format!("trello:{card}"));
        let record = std::fs::read_to_string(state.path().join(format!("claims/{card}.json")))
            .expect("the sweep's claim is recorded");
        let record: Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["list_id"], Value::Null, "no list to go back to");

        release_swept(&b, plugin, &unit["key"], restart, &state);
        assert!(claims(&b.fake, &card).is_empty());
        assert_eq!(
            b.fake.cards_in("Up for Grabs"),
            [b.card.clone(), card],
            "restart: {restart}"
        );
    }
}

// --- finish, per outcome ---

/// A clean finish moves nothing: it posts one `[afkd-ran]` watermark carrying the
/// boundary fixed at the claim read (pruning the prior one) and releases the claim last.
/// What `on_done` does after it is afkd's, through account calls.
#[test]
fn a_clean_finish_leaves_one_watermark_and_releases_the_claim() {
    let b = board();
    let phil_at = b.fake.comments(&b.card)[0].posted;
    let prior = b.fake.comment(
        &b.card,
        &b.fake.me(),
        &format!("[afkd-ran] owner=afkd-17 upto={phil_at}"),
        2700,
    );
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut plugin);
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
    assert_eq!(
        b.fake.list_of(&b.card),
        "Up for Grabs",
        "finish moves nothing"
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

/// A failed finish leaves the one watermark and releases the claim, and labels and moves
/// nothing: the Problem label and the move to the backlog are `on_fail`'s, through account
/// calls.
#[test]
fn a_failed_finish_leaves_the_watermark_and_releases_the_claim_alone() {
    let b = board();
    let queued = b
        .fake
        .card("Backlog", "Qu3u3d00", "Already in the backlog", "");
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut plugin);
    assert_eq!(
        finish(
            &mut plugin,
            &unit,
            "failed",
            facts("fault", Some("cargo test: 3 failed")),
        ),
        json!({"ok": true})
    );
    assert!(b.fake.labels(&b.card).is_empty(), "finish labels nothing");
    assert_eq!(said(&b.fake, &b.card, "[afkd-ran]").len(), 1);
    assert!(claims(&b.fake, &b.card).is_empty());
    assert_eq!(b.fake.cards_in("Backlog"), [queued], "finish moves nothing");
    assert_eq!(b.fake.list_of(&b.card), "Up for Grabs");
    plugin.finish();
}

/// A park badges the card itself, posts the owner marker naming the configured service,
/// and only then releases the claim, leaving the card where it was; what `on_park` does
/// after the finish is afkd's, through account calls.
#[test]
fn a_park_finish_badges_marks_and_releases() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut plugin);
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
        "Up for Grabs",
        "a park does not move"
    );
    assert_eq!(
        said(&b.fake, &b.card, "[afkd-park]"),
        ["[afkd-park] service=afkd::develop"]
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    // The release is the finish's last write: every post a park owes is up before the
    // claim goes.
    let writes = |fake: &FakeTrello| -> Vec<String> {
        fake.seen()
            .into_iter()
            .filter(|r| r.method == "POST" || r.method == "DELETE")
            .map(|r| format!("{} {}", r.method, r.path.rsplit('/').next().unwrap_or("")))
            .collect()
    };
    assert_eq!(
        writes(&b.fake).last().map(String::as_str),
        Some("DELETE comments")
    );
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

/// A finish that does not land is `held`: the claim stays on the card, and afkd's later
/// `release` of the key replays what is owed — `false` while the board is still down, `true`
/// once it lands — and nothing else. A child that did not hold the finish — afkd restarted in between, under a
/// claim owner of its own — takes the crash reversal for the same key instead: the first
/// process's claim is pruned, since it carries another owner.
#[test]
fn an_undelivered_finish_is_held_and_release_delivers_it() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut plugin);
    let key = unit["key"].as_str().unwrap().to_string();
    b.fake.fail("delete comment", 500);

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true, "held": true})
    );
    assert!(
        plugin
            .stderr_soon("afkd holds the claim")
            .ends_with(&format!(
                "afkd-trello: trello delete comment: board returned status 500\n\
             afkd-trello: could not deliver the finish for {key}; afkd holds the claim and \
             releases it on a later beat\n"
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

    b.fake.heal("delete comment");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert_eq!(
        b.fake.list_of(&b.card),
        "Up for Grabs",
        "the owed release moves no card"
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    plugin.finish();

    // After a restart: the same shape, held by a child that is gone.
    let b = board();
    let mut before = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut before);
    let key = unit["key"].as_str().unwrap().to_string();
    b.fake.fail("delete comment", 500);
    assert_eq!(
        finish(&mut before, &unit, "clean", facts("proceed", None)),
        json!({"ok": true, "held": true})
    );
    before.finish();
    b.fake.heal("delete comment");
    let mut after = Plugin::armed_with(DEVELOP, "afkd-5151", None, settings(&b.fake));
    assert_eq!(
        after.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert!(claims(&b.fake, &b.card).is_empty());
    assert_eq!(
        b.fake.cards_in("Up for Grabs").last(),
        Some(&b.card),
        "reversed to the bottom of claim_from"
    );
    after.finish();
}

// --- a call that is no read and no write ---

/// The actions a slot used to call on a claimed card are gone — the four label and member ones
/// are now writes that take an account, the other six nothing — and each of the six is refused
/// by name, in the armed child of a service that claimed the very card it names and in a child
/// of its own alike, and nothing reaches the board.
#[test]
fn an_action_a_slot_used_to_call_is_refused_by_name() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut plugin);
    let card = json!({"id": unit["id"], "key": unit["key"]});
    let writes = |fake: &FakeTrello| fake.seen().iter().filter(|r| r.method != "GET").count();
    let before = writes(&b.fake);
    for action in [
        "mark_complete",
        "archive",
        "move_to",
        "comment",
        "attach",
        "fetch_attachment",
    ] {
        let error = format!("no action `{action}`");
        assert_eq!(
            plugin.read_armed(action, json!({"card": card, "text": "x"})),
            json!({"ok": false, "error": error}),
            "{action}"
        );
        assert!(plugin
            .stderr_soon(&error)
            .contains(&format!("afkd-trello: {error}\n")));
    }
    assert_eq!(writes(&b.fake), before, "nothing reached the board");
    plugin.finish();

    let mut alone = Plugin::spawn();
    assert_eq!(
        alone.read("archive", json!({"card": card})),
        json!({"ok": false, "error": "no action `archive`"})
    );
    alone.finish();
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

    let unit = claim_one(&mut develop);
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
        .comment(&b.card, &b.fake.me(), "Which header — `Retry-After`? 🙏", 2);
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

/// CGRDWyFv: a card a develop run built once, stamping `upto=0`, then discussed — every
/// human comment answered — and parked on afkd's question. The discusser's sweep leaves
/// it alone until a human speaks after that question, and the brief that reply resumes
/// holds it alone as new.
#[test]
fn a_parked_discuss_card_built_once_waits_for_a_reply_after_afkds_question() {
    let b = board();
    let me = b.fake.me();
    let mut discuss = Plugin::armed_as(DISCUSS, discuss_settings(&b.fake));
    let card = b
        .fake
        .card("Discussion", "CGRDWyFv", "Ladda upp bilderna", "");
    b.fake
        .comment(&card, &me, "[afkd-ran] owner=afkd-512737 upto=0", 60);
    b.fake.comment(
        &card,
        &b.phil,
        "ladda upp dom och fixa min lokala databas.",
        50,
    );
    b.fake.comment(
        &card,
        &me,
        "The upload failed: the bucket answered `403 Forbidden`.\nI need a fresh key.",
        45,
    );
    b.fake.comment(&card, &b.phil, "nå? 🚗", 40);
    b.fake.comment(
        &card,
        &me,
        "Which key should I use — the one in `.env` or a new one?\n\n```sh\naws s3 ls s3://bokningar\n```",
        30,
    );
    b.fake
        .comment(&card, &me, &format!("[afkd-park] service={DISCUSS}"), 29);
    b.fake.label(&card, "Awaiting Reply");

    let (comments, labels) = (b.fake.comments(&card), b.fake.labels(&card));
    let writes = |fake: &FakeTrello| fake.seen().iter().filter(|r| r.method != "GET").count();
    let before = writes(&b.fake);
    assert_eq!(
        discuss.poll(),
        json!({"fire": false}),
        "{}",
        discuss.stderr()
    );
    assert_eq!(writes(&b.fake), before, "nothing written");
    assert_eq!(b.fake.comments(&card), comments);
    assert_eq!(b.fake.labels(&card), labels, "still badged");

    let reply = "nyckeln ligger i 1Password — 「bokningar-s3」 🔑\nprova igen.";
    b.fake.comment(&card, &b.phil, reply, 0);
    let resumed = discuss.poll()["unit"].clone();
    assert_eq!(resumed["thread"], "CGRDWyFv", "{}", discuss.stderr());
    assert!(b.fake.labels(&card).is_empty(), "the badge comes off");
    assert!(
        said(&b.fake, &card, "[afkd-park]").is_empty(),
        "and the marker"
    );
    let brief = resumed["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with(&format!("## New comments\n\n**Phil Ek:** {reply}\n")),
        "{brief}"
    );
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

// --- the files a card holds ---

/// A claimed card's files come with it into the task folder: a PNG its description embeds
/// and a PDF a comment links, both uploads on the card, are downloaded into the plugin's
/// directory and handed over as `from` entries whose bytes are Trello's own, the README links
/// each by its name with no Trello url left, the handle's `description` is Trello's text, and
/// `finish` deletes the folder.
#[test]
fn a_claimed_cards_files_come_with_it_in_the_task_folder() {
    let b = board();
    let png = b"\x89PNG\r\n\x1a\n\0\xff\xfe--afkd--\r\n".to_vec();
    let pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\r\n\0\xff--afkd--\r\n1 0 obj\n".to_vec();
    let shot = b.fake.attachment(&b.card, "screen shot.png", &png, 600);
    let report = b.fake.attachment(&b.card, "Q3 report.pdf", &pdf, 500);
    let (shot_url, report_url) = (b.fake.download_url(&shot), b.fake.download_url(&report));
    let description = format!("{BODY}\n\n![screen shot.png]({shot_url})");
    b.fake.describe(&b.card, &description);
    b.fake.comment(
        &b.card,
        &b.chen,
        &format!("The numbers: [Q3 report.pdf]({report_url})"),
        60,
    );
    let state = TempDir::new("task-files");
    let mut plugin = Plugin::armed_with(DEVELOP, OWNER, Some(state.path()), settings(&b.fake));
    let unit = claim_one(&mut plugin);

    let files = unit["files"].as_array().unwrap();
    assert_eq!(files.len(), 3, "{files:?}");
    assert_eq!(files[0]["path"], "task/README.md");
    assert_eq!(files[1]["path"], "task/screen shot.png");
    assert_eq!(files[2]["path"], "task/Q3 report.pdf");
    for (entry, bytes) in [(&files[1], png), (&files[2], pdf)] {
        assert_eq!(
            entry.as_object().unwrap().len(),
            2,
            "path and from: {entry}"
        );
        let from = std::path::Path::new(entry["from"].as_str().unwrap());
        assert!(
            from.is_absolute() && from.starts_with(state.path().join("task-files")),
            "{from:?}"
        );
        assert_eq!(std::fs::read(from).unwrap(), bytes);
    }
    let readme = files[0]["text"].as_str().unwrap();
    assert!(
        readme.contains("![screen shot.png](<screen shot.png>)"),
        "{readme}"
    );
    assert!(
        readme.contains("[Q3 report.pdf](<Q3 report.pdf>)"),
        "{readme}"
    );
    assert!(
        !readme.contains(&shot_url) && !readme.contains(&report_url),
        "no Trello url is left: {readme}"
    );
    assert!(
        !readme.contains("## Files"),
        "every file is linked: {readme}"
    );
    assert_eq!(unit["fields"]["description"], description);
    assert!(
        !plugin.stderr().contains("not downloaded"),
        "{}",
        plugin.stderr()
    );

    let folder = std::path::Path::new(files[1]["from"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(folder.is_dir());
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert!(!folder.exists(), "the folder is deleted at finish");
    plugin.finish();
}

// --- cards a 0.17 release claimed or parked ---

/// A card a 0.17 release claimed carries its `[afkd-claim] owner=… renewal=N` comment, by the
/// token's member, and the release's record in the plugin's directory. A 0.18 child, armed in
/// the same directory under an owner of its own, recognises the claim as live — posted past the
/// claim's lifetime ago, but renewed by an edit since, as 0.17 renews one — and steps over it in
/// silence; and releases it by the `trello:<id>` key, deleting the claim and returning the card
/// to the list the record names.
#[test]
fn a_card_a_0_17_release_claimed_is_stepped_over_renewed_and_released() {
    let b = board();
    let (list, card) = second_list(&b);
    let queued = b.fake.card("Hjälpvideor 🎬", "Qu3u3d00", "Nästa video", "");
    let state = TempDir::new("carry-over");
    // What 0.17 left: the claim, by the token's own member, from the second list, posted two
    // hours ago and renewed since; its record naming that list; and the card out where its
    // `on_claim` put it.
    let claim = b.fake.comment(
        &card,
        &b.fake.me(),
        "[afkd-claim] owner=afkd-17 renewal=3",
        7200,
    );
    b.fake
        .edit_comment(&claim, "[afkd-claim] owner=afkd-17 renewal=4");
    std::fs::create_dir_all(state.path().join("claims")).unwrap();
    std::fs::write(
        state.path().join(format!("claims/{card}.json")),
        serde_json::to_vec(&json!({"card_id": card, "claim_id": claim,
                                   "list_id": b.fake.list_id("Hjälpvideor 🎬")}))
        .unwrap(),
    )
    .unwrap();
    b.fake.move_card(&card, "In Progress");
    let key = json!(format!("trello:{card}"));

    let mut plugin = Plugin::armed_with(
        DEVELOP,
        "afkd-5151",
        Some(state.path()),
        two_list_settings(&b.fake),
    );
    // It is recognised as live: the scan steps over it to the card queued behind, and no claim
    // is posted on it — the board holds it as it was.
    let polled = plugin.poll();
    assert_eq!(polled["unit"]["id"], "Qu3u3d00", "{}", plugin.stderr());
    assert_eq!(claims(&b.fake, &card).len(), 1, "the 0.17 claim alone");
    assert_eq!(
        b.fake
            .comments(&card)
            .into_iter()
            .filter(|c| c.text.starts_with("[afkd-claim]"))
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        std::slice::from_ref(&claim),
        "stepped over in silence"
    );
    let held = b.fake.comments(&card);
    assert_eq!(
        held.iter().find(|c| c.id == claim).map(|c| c.text.as_str()),
        Some("[afkd-claim] owner=afkd-17 renewal=4"),
        "left as its holder renewed it"
    );

    // The release by its `trello:<id>` key finds the record: the claim goes, and the card
    // returns to the bottom of the list the record names, not the first `claim_from` list.
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true}),
        "{}",
        plugin.stderr()
    );
    assert!(b
        .fake
        .comments(&card)
        .iter()
        .all(|c| !c.text.starts_with("[afkd-claim]")));
    assert_eq!(b.fake.cards_in("Hjälpvideor 🎬"), [queued, card.clone()]);
    assert!(
        !state.path().join(format!("claims/{card}.json")).exists(),
        "the record goes with the release"
    );
    assert_eq!(list, b.fake.list_id("Hjälpvideor 🎬"));
    plugin.finish();
}

/// A 0.18 child that claims a card renews its own claim in place — the comment a 0.17 release
/// would have edited the same way, its text `[afkd-claim] owner=… renewal=N` — and the claim it
/// leaves is one a 0.17 release reads as live: its renewal is the edit, its owner the 0.18
/// child's, and its author the token's member.
#[test]
fn a_claim_this_release_renews_reads_as_a_0_17_claim_does() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let unit = claim_one(&mut plugin);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 7})),
        json!({"ok": true})
    );
    let held = claims(&b.fake, &b.card);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].text, "[afkd-claim] owner=afkd-4242 renewal=7");
    assert_eq!(held[0].author, b.fake.me());
    assert!(held[0].edited.is_some(), "renewed by an edit in place");
    plugin.finish();
}

/// A card a 0.17 release parked — badged `Awaiting Reply`, carrying its `[afkd-park]
/// service=…` marker — is claimed again, after a human reply, by the service that parked it:
/// the badge and the marker come off, a service that did not park it leaves it alone, and the
/// 0.18 finish releases it.
#[test]
fn a_card_a_0_17_release_parked_is_claimed_again_by_its_service_and_released() {
    let b = board();
    let parked = b
        .fake
        .card("In Progress", "P4rk3d00", "Bakgrundsvideo: färjan ⛴️", "");
    b.fake.label(&parked, "Awaiting Reply");
    b.fake.comment(
        &parked,
        &b.fake.me(),
        "[afkd-ran] owner=afkd-17 upto=0",
        300,
    );
    let question = b
        .fake
        .comment(&parked, &b.fake.me(), "Med eller utan speakerröst?", 120);
    let marker = b.fake.comment(
        &parked,
        &b.fake.me(),
        &format!("[afkd-park] service={DEVELOP}"),
        110,
    );
    let mut develop = Plugin::armed(settings(&b.fake));
    let mut discuss = Plugin::armed_as(DISCUSS, discuss_settings(&b.fake));

    // Unanswered, it is left alone by everyone, badge and marker kept.
    let writes = |fake: &FakeTrello| fake.seen().iter().filter(|r| r.method != "GET").count();
    let before = writes(&b.fake);
    assert_eq!(
        discuss.poll(),
        json!({"fire": false}),
        "not the discusser's"
    );
    assert_eq!(writes(&b.fake), before, "the discusser wrote nothing");
    assert_eq!(b.fake.labels(&parked), ["Awaiting Reply"]);
    assert_eq!(said(&b.fake, &parked, "[afkd-park]").len(), 1);
    let first = develop.poll();
    assert_eq!(
        first["unit"]["thread"],
        SHORT_LINK,
        "the board's own card is claimed on: {}",
        develop.stderr()
    );
    assert!(
        claims(&b.fake, &parked).is_empty(),
        "no claim on the unanswered card"
    );
    assert_eq!(b.fake.labels(&parked), ["Awaiting Reply"]);

    // A human answers; the parking service claims it again, ahead of `claim_from`.
    b.fake.comment(&parked, &b.phil, "Utan — bara musik.", 0);
    let again = develop.poll();
    assert_eq!(again["fire"], true, "{}", develop.stderr());
    let unit = again["unit"].clone();
    assert_eq!(unit["id"], "P4rk3d00");
    assert_eq!(unit["key"], format!("trello:{parked}"));
    assert!(b.fake.labels(&parked).is_empty(), "the badge comes off");
    assert!(
        said(&b.fake, &parked, "[afkd-park]").is_empty(),
        "and the marker"
    );
    assert!(b.fake.comments(&parked).iter().all(|c| c.id != marker));
    let brief = unit["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with("**Phil Ek:** Utan — bara musik.\n"),
        "{brief}"
    );
    assert!(
        brief.contains("Med eller utan speakerröst?")
            && b.fake.comments(&parked).iter().any(|c| c.id == question),
        "the question is the earlier conversation"
    );

    // Its 0.18 finish releases it, leaving the watermark.
    assert_eq!(
        finish(&mut develop, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert!(claims(&b.fake, &parked).is_empty());
    assert_eq!(said(&b.fake, &parked, "[afkd-ran]").len(), 1);
    develop.finish();
    discuss.finish();
}
