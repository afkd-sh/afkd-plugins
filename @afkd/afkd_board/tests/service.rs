//! The `board` kind's suite: the real `afkd-afkd_board` exec, armed by a `hello` and sent `poll`,
//! `finish`, `release` and `call` as afkd sends them, against the fake board an `afkd_board.Board`
//! reaches. No afkd is involved.
//!
//! The legs hold the claim: two armed children racing for one card, of which exactly one wins and
//! the loser takes the next card in the same poll; each filter holding a card back; the unit a
//! win hands over; and the owed release, which a child's `finish` writes down and its next `poll`
//! takes off, through two children sharing one directory and one owner. Every leg ends with the
//! fake's record of requests the plugin should never have sent empty.

mod common;

use std::path::Path;

use serde_json::{json, Value};

use common::fake::{FakeBoard, NewCard};
use common::{armed, board, hello, once, reply_of, Plugin, TempDir, MAX_CALL_LINE};

const KEY: &str = "afk_board_train-0123456789";

/// A board with `Todo`, `Doing` and `Done`, and the labels the legs put on cards.
fn world() -> FakeBoard {
    FakeBoard::start(
        "Release train 🚂",
        KEY,
        &["Todo", "Doing", "Done"],
        &[
            ("afkd/claimed", "blue"),
            ("hold", "red"),
            ("ready", "green"),
        ],
    )
}

/// `poll` the child and read its reply.
fn poll(plugin: &mut Plugin) -> Value {
    let line = plugin.call_raw(json!({"call": "poll"}));
    reply_of(&line, MAX_CALL_LINE)
}

/// The card handle a unit carries, as afkd passes it to a slot.
fn handle_of(unit: &Value) -> Value {
    json!({"id": unit["id"], "key": unit["key"]})
}

/// The labels the fake holds on the card `number`.
fn labels(fake: &FakeBoard, number: i64) -> Vec<String> {
    fake.card_state(number).expect("the card").labels
}

/// The claim markers on the card `number`.
fn markers(fake: &FakeBoard, number: i64) -> usize {
    fake.comments_on(number)
        .iter()
        .filter(|(body, _, by_key, _)| *by_key && body.starts_with("[afkd-claim] owner="))
        .count()
}

fn no_invalid(fake: &FakeBoard) {
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests the board's API refuses"
    );
}

/// The ledger the children of `dir` share, as the file holds it.
fn owed(dir: &Path) -> Vec<Value> {
    match std::fs::read_to_string(dir.join("owed-releases.json")) {
        Ok(text) => serde_json::from_str(&text).expect("the ledger is JSON"),
        Err(_) => Vec::new(),
    }
}

/// An accepted `hello` lists the one optional call, and a refused one says why and leaves the
/// child waiting: it is not ended until afkd sends it something before an accepted one.
#[test]
fn hello_arms_the_kind_and_refuses_what_it_cannot_arm_with() {
    let fake = world();
    let dir = TempDir::new("hello");
    let plugin = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    assert_eq!(plugin.finish(), "");

    let mut plugin = Plugin::spawn();
    let mut refused = hello(
        &fake,
        dir.path(),
        "afkd-1",
        json!(["Todo", "Todo"]),
        json!({}),
    );
    let line = plugin.call_raw(refused.clone());
    assert_eq!(
        reply_of(&line, MAX_CALL_LINE),
        json!({"ok": false, "proto": 2})
    );
    refused["settings"]["claim_from"] = json!([]);
    let line = plugin.call_raw(refused);
    assert_eq!(
        reply_of(&line, MAX_CALL_LINE),
        json!({"ok": false, "proto": 2})
    );
    let said = plugin.finish();
    assert!(
        said.contains("setting `claim_from`: the list `Todo` is named twice"),
        "{said}"
    );
    assert!(said.contains("`claim_from` is required"), "{said}");
    assert!(!said.contains(KEY), "the key is never printed: {said}");
}

/// Two armed children, each in a home of its own, race for the one card there is: the board holds
/// both markers, exactly one child wins and puts the label on, and the loser is idle.
#[test]
fn two_children_racing_one_card_exactly_one_wins() {
    let fake = world();
    fake.card(NewCard::new("Todo", "The only card"));
    let (da, db) = (TempDir::new("a"), TempDir::new("b"));
    let mut a = armed(&fake, da.path(), "afkd-a", json!("Todo"), json!({}));
    let mut b = armed(&fake, db.path(), "afkd-b", json!("Todo"), json!({}));
    a.send(&json!({"call": "poll"}));
    b.send(&json!({"call": "poll"}));
    let (ra, rb) = (a.recv(), b.recv());
    let fired: Vec<&Value> = [&ra, &rb]
        .into_iter()
        .filter(|r| r["fire"] == true)
        .collect();
    assert_eq!(fired.len(), 1, "exactly one wins: {ra} {rb}");
    assert_eq!(
        fired[0]["unit"]["key"],
        format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(1))
    );
    assert_eq!(labels(&fake, 1), ["afkd/claimed"]);
    assert_eq!(markers(&fake, 1), 2, "both raced");
    a.finish();
    b.finish();
    no_invalid(&fake);
}

/// Two cards, two children: each ends up with a card of its own, the loser of the first race
/// taking the second card in the same poll, and neither is claimed twice.
#[test]
fn the_loser_takes_the_next_card_in_the_same_poll() {
    let fake = world();
    fake.card(NewCard::new("Todo", "First"));
    fake.card(NewCard::new("Todo", "Second"));
    let (da, db) = (TempDir::new("a"), TempDir::new("b"));
    let mut a = armed(&fake, da.path(), "afkd-a", json!("Todo"), json!({}));
    let mut b = armed(&fake, db.path(), "afkd-b", json!("Todo"), json!({}));
    a.send(&json!({"call": "poll"}));
    b.send(&json!({"call": "poll"}));
    let (ra, rb) = (a.recv(), b.recv());
    assert_eq!(
        (&ra["fire"], &rb["fire"]),
        (&json!(true), &json!(true)),
        "{ra} {rb}"
    );
    let mut keys = vec![
        ra["unit"]["key"].as_str().unwrap().to_string(),
        rb["unit"]["key"].as_str().unwrap().to_string(),
    ];
    keys.sort();
    let mut want: Vec<String> = (1..=2)
        .map(|n| format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(n)))
        .collect();
    want.sort();
    assert_eq!(keys, want, "two different cards");
    for n in 1..=2 {
        assert_eq!(labels(&fake, n), ["afkd/claimed"], "#{n}");
    }
    assert_eq!(markers(&fake, 1), 2, "both raced for the first");
    assert_eq!(markers(&fake, 2), 1, "the loser alone raced for the second");
    a.finish();
    b.finish();
    no_invalid(&fake);
}

/// The unit a win hands over: the card's number as its id, its handle's key and fields (which
/// are the ones a read of the card gives, so the slot's card is `==` a read one), the env a run
/// carries, and the brief.
#[test]
fn the_unit_is_the_card_handle_with_its_env_and_brief() {
    let fake = world();
    fake.card(NewCard {
        description: "It retries forever.\n\n```rust\nlet x = 1;\n```".to_string(),
        checklist: vec![
            ("复现 the storm 🚨".to_string(), true),
            ("Bound it".to_string(), false),
        ],
        labels: vec!["ready".to_string()],
        ..NewCard::new("Todo", "修复 the \"retry\" storm 🚨")
    });
    fake.comment(1, "看起来不对 🚨", "Ed", false, None, 60);
    let dir = TempDir::new("unit");
    let mut plugin = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    let reply = poll(&mut plugin);
    let unit = &reply["unit"];
    assert_eq!(unit["id"], "1");

    let (read, _) = once("card", json!({"b": board(&fake), "number": 1}));
    assert_eq!(unit["key"], read["handle"]["key"]);
    assert_eq!(unit["id"], read["handle"]["id"]);
    assert_eq!(
        unit["fields"], read["handle"]["fields"],
        "the card's handle fields"
    );
    assert_eq!(unit["fields"]["labels"], json!(["ready", "afkd/claimed"]));
    assert_eq!(unit["thread"], unit["key"]);
    assert_eq!(
        unit["env"],
        json!({"AFKD_BOARD_URL": fake.base_url(),
               "AFKD_BOARD_AUTHORIZATION": format!("Bearer {KEY}"),
               "AFKD_BOARD_RUN": "{\"service\":\"develop\"}"})
    );
    assert_eq!(
        unit["args"],
        json!({"b": {"key": KEY, "base_url": fake.base_url()}}),
        "every slot is passed the board the settings make, as a call's b"
    );
    assert_eq!(unit["files"][0]["path"], "task/README.md");
    let brief = unit["files"][0]["text"].as_str().unwrap();
    for part in [
        "# #1 修复 the \"retry\" storm 🚨",
        "It retries forever.",
        "let x = 1;",
        "- [x] 复现 the storm 🚨",
        "- [ ] Bound it",
        "**Ed**",
        "看起来不对 🚨",
    ] {
        assert!(brief.contains(part), "{part:?} in:\n{brief}");
    }
    assert!(
        !brief.contains("[afkd-claim]"),
        "no marker in the brief:\n{brief}"
    );
    plugin.finish();
    no_invalid(&fake);
}

/// `only_blocked false` passes over a blocked card, left out the service claims it, and `true`
/// claims only a blocked one.
#[test]
fn only_blocked_is_the_plugins_own_filter() {
    let build = || {
        let fake = world();
        fake.card(NewCard::new("Doing", "The blocker"));
        fake.card(NewCard {
            blocked_by: vec![1],
            ..NewCard::new("Todo", "Blocked")
        });
        fake.card(NewCard::new("Todo", "Free"));
        fake
    };
    for (extra, claims) in [
        (json!({"only_blocked": false}), 3),
        (json!({}), 2),
        (json!({"only_blocked": true}), 2),
    ] {
        let fake = build();
        let dir = TempDir::new("blocked");
        let mut plugin = armed(&fake, dir.path(), "afkd-1", json!("Todo"), extra.clone());
        let reply = poll(&mut plugin);
        assert_eq!(reply["unit"]["id"], claims.to_string(), "{extra}");
        plugin.finish();
        no_invalid(&fake);
    }
    let fake = build();
    fake.card(NewCard::new("Todo", "Another free one"));
    let dir = TempDir::new("blocked");
    let mut plugin = armed(
        &fake,
        dir.path(),
        "afkd-1",
        json!("Todo"),
        json!({"only_blocked": false}),
    );
    assert_eq!(poll(&mut plugin)["unit"]["id"], "3");
    assert_eq!(markers(&fake, 2), 0, "a blocked card is never raced for");
    plugin.finish();
}

/// `only_labels` and `skip_labels` each hold a card back, and the card held back is never raced
/// for.
#[test]
fn only_labels_and_skip_labels_each_hold_a_card_back() {
    let fake = world();
    fake.card(NewCard::new("Todo", "Plain"));
    fake.card(NewCard {
        labels: vec!["ready".to_string()],
        ..NewCard::new("Todo", "Ready")
    });
    let dir = TempDir::new("only");
    let mut plugin = armed(
        &fake,
        dir.path(),
        "afkd-1",
        json!("Todo"),
        json!({"only_labels": ["ready"]}),
    );
    assert_eq!(poll(&mut plugin)["unit"]["id"], "2");
    assert_eq!(markers(&fake, 1), 0);
    plugin.finish();

    let fake = world();
    fake.card(NewCard {
        labels: vec!["hold".to_string()],
        ..NewCard::new("Todo", "Held")
    });
    fake.card(NewCard::new("Todo", "Plain"));
    let dir = TempDir::new("skip");
    let mut plugin = armed(
        &fake,
        dir.path(),
        "afkd-1",
        json!("Todo"),
        json!({"skip_labels": ["hold"]}),
    );
    assert_eq!(poll(&mut plugin)["unit"]["id"], "2");
    assert_eq!(markers(&fake, 1), 0);
    assert_eq!(poll(&mut plugin)["fire"], false, "the held card stays held");
    plugin.finish();
    no_invalid(&fake);
}

/// The lists are walked in the order written, and a list the board lacks, or a board that does
/// not know the key, is an idle beat that says so.
#[test]
fn lists_are_walked_in_order_and_a_missing_list_or_a_refused_key_is_idle() {
    let fake = world();
    fake.card(NewCard::new("Todo", "In todo"));
    fake.card(NewCard::new("Doing", "In doing"));
    let dir = TempDir::new("order");
    let mut plugin = armed(
        &fake,
        dir.path(),
        "afkd-1",
        json!(["Doing", "Todo"]),
        json!({}),
    );
    assert_eq!(poll(&mut plugin)["unit"]["id"], "2");
    assert_eq!(poll(&mut plugin)["unit"]["id"], "1");
    plugin.finish();

    let dir = TempDir::new("missing");
    let mut plugin = armed(&fake, dir.path(), "afkd-1", json!(["Nowhere"]), json!({}));
    assert_eq!(poll(&mut plugin), json!({"fire": false}));
    let said = plugin.finish();
    assert!(
        said.contains("poll: The board has no list `Nowhere`; it has `Todo`, `Doing` and `Done`."),
        "{said}"
    );

    let dir = TempDir::new("refused");
    let mut other = Plugin::spawn();
    let mut greeting = hello(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    greeting["settings"]["key"] = json!("afk_board_wrong-0123456789");
    let line = other.call_raw(greeting);
    assert_eq!(reply_of(&line, MAX_CALL_LINE)["ok"], true);
    assert_eq!(poll(&mut other), json!({"fire": false}));
    let said = other.finish();
    assert!(
        said.contains("the board key is missing, unknown or revoked"),
        "{said}"
    );
    assert!(
        !said.contains("wrong-0123456789"),
        "the key is never printed: {said}"
    );
}

/// `finish` writes the card down and leaves the label on; the next `poll` of the child takes it
/// off. A sibling sharing the directory and the owner leaves that card alone, and takes off its
/// own.
#[test]
fn finish_leaves_the_label_for_the_childs_next_poll_and_a_sibling_leaves_it_alone() {
    let fake = world();
    fake.card(NewCard::new("Todo", "First"));
    fake.card(NewCard::new("Todo", "Second"));
    let dir = TempDir::new("shared");
    let mut a = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    let mut b = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    let first = poll(&mut a)["unit"].clone();
    assert_eq!(first["id"], "1");
    // The run is done, and its card is moved on by the slots: it is done, not claimable.
    let (_, _) = once(
        "mark_done",
        json!({"b": board(&fake), "card": handle_of(&first)}),
    );

    let line = a.call_raw(json!({"call": "finish", "id": "1", "key": first["key"],
                                 "outcome": "clean", "facts": {"signal": "proceed"}}));
    assert_eq!(reply_of(&line, MAX_CALL_LINE), json!({"ok": true}));
    assert_eq!(
        labels(&fake, 1),
        ["afkd/claimed"],
        "finish takes nothing off"
    );
    let entries = owed(dir.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["key"], first["key"]);
    assert_eq!(entries[0]["number"], 1);
    assert_eq!(entries[0]["board"], fake.board_id());
    assert_eq!(entries[0]["owner"], "afkd-1");
    assert_eq!(entries[0]["pid"], a.pid());

    let second = poll(&mut b)["unit"].clone();
    assert_eq!(second["id"], "2", "B claims the next card");
    assert_eq!(
        labels(&fake, 1),
        ["afkd/claimed"],
        "B leaves A's card labelled"
    );
    assert_eq!(owed(dir.path()).len(), 1);

    assert_eq!(poll(&mut a)["fire"], false, "A has nothing left to claim");
    assert!(labels(&fake, 1).is_empty(), "A takes its own label off");
    assert_eq!(labels(&fake, 2), ["afkd/claimed"], "and leaves B's");
    assert_eq!(owed(dir.path()), Vec::<Value>::new());
    a.finish();
    b.finish();
    no_invalid(&fake);
}

/// An entry an earlier afkd process left is taken off by the first child of its board to poll and
/// by no other, and an entry of another board is left where it was.
#[test]
fn an_entry_of_an_earlier_afkd_is_taken_by_the_first_child_of_its_board_alone() {
    let fake = world();
    fake.card(NewCard {
        labels: vec!["afkd/claimed".to_string()],
        done: true,
        ..NewCard::new("Todo", "Left on by a crash")
    });
    let dir = TempDir::new("earlier");
    let entry = |board: &str, key: &str| json!({"key": key, "number": 1, "board": board, "owner": "afkd-old", "pid": 4242});
    let mine = entry(
        &fake.board_id(),
        &format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(1)),
    );
    let theirs = entry("another-board", "afkd_board:another-board/card-9");
    std::fs::write(
        dir.path().join("owed-releases.json"),
        json!([mine, theirs]).to_string(),
    )
    .expect("seed the ledger");

    let mut a = armed(&fake, dir.path(), "afkd-new", json!("Todo"), json!({}));
    let mut b = armed(&fake, dir.path(), "afkd-new", json!("Todo"), json!({}));
    assert_eq!(poll(&mut a)["fire"], false);
    assert!(labels(&fake, 1).is_empty(), "the first child took it off");
    let removals = |fake: &FakeBoard| {
        fake.writes()
            .iter()
            .filter(|w| w.method == "DELETE" && w.path.contains("/labels/"))
            .count()
    };
    assert_eq!(removals(&fake), 1);
    assert_eq!(poll(&mut b)["fire"], false);
    assert_eq!(removals(&fake), 1, "no other child took it");
    assert_eq!(owed(dir.path()), [theirs], "another board's entry is left");
    a.finish();
    b.finish();
    no_invalid(&fake);
}

/// `release` of a journalled key takes the label off the card it names, also for an archived
/// card and one that no longer carries it; a key of another board or of no shape is `null`.
#[test]
fn release_takes_the_label_off_a_journalled_key() {
    let fake = world();
    for title in ["Crashed", "Archived", "Clean"] {
        let labels = if title == "Clean" {
            vec![]
        } else {
            vec!["afkd/claimed".to_string()]
        };
        fake.card(NewCard {
            labels,
            ..NewCard::new("Todo", title)
        });
    }
    fake.archive(2, common::fake::T0 + 3600);
    let dir = TempDir::new("release");
    let mut plugin = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    let key = |n: i64| format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(n));
    for n in [1, 2, 3] {
        let line = plugin.call_raw(json!({"call": "release", "key": key(n)}));
        assert_eq!(
            reply_of(&line, MAX_CALL_LINE),
            json!({"released": true}),
            "#{n}"
        );
        assert!(labels(&fake, n).is_empty(), "#{n}");
    }
    for key in ["afkd_board:another-board/card-1", "garbage", ""] {
        let line = plugin.call_raw(json!({"call": "release", "key": key}));
        assert_eq!(
            reply_of(&line, MAX_CALL_LINE),
            json!({"released": null}),
            "{key:?}"
        );
    }
    let said = plugin.finish();
    assert!(said.contains("a card of board another-board"), "{said}");
    no_invalid(&fake);
}

/// `release` on a board that has no `afkd/claimed` label has nothing to take off, and says so.
#[test]
fn release_on_a_board_without_the_label_has_nothing_to_take_off() {
    let fake = FakeBoard::start("Bare", KEY, &["Todo"], &[]);
    fake.card(NewCard::new("Todo", "Plain"));
    let dir = TempDir::new("bare");
    let mut plugin = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    let key = format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(1));
    let line = plugin.call_raw(json!({"call": "release", "key": key}));
    assert_eq!(reply_of(&line, MAX_CALL_LINE), json!({"released": true}));
    assert_eq!(fake.writes(), Vec::new());
    plugin.finish();
    no_invalid(&fake);
}

/// An armed child serves a read through its `call` as a one-shot child does: the same reply.
#[test]
fn an_armed_childs_call_gives_the_reply_a_one_shot_child_gives() {
    let fake = world();
    fake.card(NewCard::new("Todo", "Only"));
    let dir = TempDir::new("call");
    let mut plugin = armed(&fake, dir.path(), "afkd-1", json!("Todo"), json!({}));
    let call = json!({"call": "call", "proto": 2, "action": "lists", "args": {"b": board(&fake)}});
    let line = plugin.call_raw(call);
    let (once_reply, _) = once("lists", json!({"b": board(&fake)}));
    assert_eq!(reply_of(&line, MAX_CALL_LINE), once_reply);
    plugin.finish();
    no_invalid(&fake);
}
