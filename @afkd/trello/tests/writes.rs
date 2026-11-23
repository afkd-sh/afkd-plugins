//! The writes a config calls with a `trello.Account` first, over the real `afkd-trello` exec
//! and the stateful fake Trello: through a child afkd starts for one call, which gets no
//! `hello` and names its `proto` on the call itself, and through the armed trigger child of a
//! `service(trello)`, whose slots may call a write too.
//!
//! Each write is held to what the brief says it gives: the handle of what it made or changed,
//! read back so it is `==` to the one the matching read hands back, or `{"ok":true}` for a
//! delete. Every leg ends by closing stdin and holding the child to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)).

mod common;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{FakeTrello, BOARD, KEY, ME, TOKEN};
use common::{Plugin, TempDir};

/// A card name carrying the delimiter a success line wraps it in, wide CJK, an emoji and an
/// em dash.
const NAME: &str = "修复 the retry storm 🚨 — \"backoff\" resets";

/// The suite's board: `Backlog`, `Doing` and `Twin` twice, the labels `bug`, `ready` and `dup`
/// twice and one that is a colour only, the members `phil` and `chen` beside the token's own,
/// and four cards.
///
/// ```text
/// Backlog   AAAA0001  BBBB0002
/// Doing     CCCC0003  DDDD0004
/// ```
struct Board {
    fake: FakeTrello,
    phil: String,
    /// The ids of the cards, by short link.
    ids: Vec<(&'static str, String)>,
}

impl Board {
    /// The id of the card whose short link is `link`.
    fn id(&self, link: &str) -> &str {
        &self
            .ids
            .iter()
            .find(|(l, _)| *l == link)
            .expect("a seeded card")
            .1
    }

    /// The handle of the card whose short link is `link`, as afkd binds it.
    fn card(&self, link: &str) -> Value {
        json!({"id": link, "key": format!("trello:{}", self.id(link))})
    }

    /// `args` with the account first, as afkd binds a write.
    fn args(&self, extra: Value) -> Value {
        let mut args = json!({"acct": self.fake.account()});
        args.as_object_mut()
            .expect("an object")
            .extend(extra.as_object().expect("an args object").clone());
        args
    }

    /// Call `action` in a child of its own, as afkd does for a call no service is behind.
    fn call(&self, plugin: &mut Plugin, action: &str, extra: Value) -> Value {
        plugin.read(action, self.args(extra))
    }

    /// The short links of the open cards of the list `list`, top to bottom, as `trello.cards`
    /// reads them.
    fn order(&self, plugin: &mut Plugin, list: &str) -> Vec<String> {
        let reply = plugin.read("cards", self.args(json!({"board": BOARD, "list": [list]})));
        handles(&reply)
            .iter()
            .map(|h| h["id"].as_str().expect("a handle id").to_string())
            .collect()
    }

    /// How many `method` requests the fake has seen.
    fn count(&self, method: &str) -> usize {
        self.fake
            .seen()
            .iter()
            .filter(|r| r.method == method)
            .count()
    }
}

fn board() -> Board {
    let fake = FakeTrello::start();
    for list in ["Backlog", "Doing", "Twin", "Twin"] {
        fake.list(list);
    }
    let phil = fake.member("phil", "Phil Ek");
    fake.member("chen", "陳大文");
    for (name, color) in [
        ("bug", Some("red")),
        ("ready", Some("green")),
        ("dup", None),
        ("dup", None),
        ("", Some("blue")),
    ] {
        fake.board_label(BOARD, name, color);
    }
    let mut ids = Vec::new();
    for (list, link, name) in [
        ("Backlog", "AAAA0001", "Backlog one"),
        ("Backlog", "BBBB0002", "Backlog two"),
        ("Doing", "CCCC0003", "Doing one"),
        ("Doing", "DDDD0004", "Doing two"),
    ] {
        ids.push((link, fake.card(list, link, name, "")));
    }
    Board { fake, phil, ids }
}

/// The short links `links`, as a list of what `trello.cards` is expected to answer.
fn want(links: &[&str]) -> Vec<String> {
    links.iter().map(|link| link.to_string()).collect()
}

/// The `handles` a reply carries, asserting it is an ok one.
fn handles(reply: &Value) -> &Vec<Value> {
    assert_eq!(reply["ok"], true, "{reply}");
    reply["handles"].as_array().expect("a list of handles")
}

/// The `handle` a write answers with, asserting it is an ok reply.
fn handle(reply: &Value) -> &Value {
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        reply.as_object().expect("an object").len(),
        2,
        "an ok and a handle, nothing else: {reply}"
    );
    &reply["handle"]
}

/// A handle as afkd binds it into a call: its `id` and `key`.
fn arg(handle: &Value) -> Value {
    json!({"id": handle["id"], "key": handle["key"]})
}

/// Asserts a delete's reply: `ok` and nothing else.
fn deleted(reply: &Value) {
    assert_eq!(reply, &json!({"ok": true}));
}

/// The refusal a reply is, its sentence.
fn refusal(reply: &Value) -> &str {
    assert_eq!(reply["ok"], false, "{reply}");
    reply["error"].as_str().expect("a sentence")
}

/// The suite's own settings for a `service(trello)` over `fake`'s board, for the legs that
/// call a write from an armed child.
fn settings(fake: &FakeTrello) -> Value {
    json!({
        "board": format!("https://trello.com/b/{BOARD}/afkd"),
        "base_url": fake.base_url(),
        "api_key": KEY,
        "token": TOKEN,
        "pick_from": "Backlog",
        "every": "30s",
        "max_attempts": "1",
    })
}

// --- cards ---

/// `create_card` resolves its list, labels and members by name against the board and sends
/// them as ids, and answers the card as Trello now holds it, every field of the `Card` handle:
/// the labels and members given, the due date to the millisecond. `trello.cards` hands back
/// the same handle.
#[test]
fn create_card_answers_the_card_as_trello_holds_it() {
    let b = board();
    let mut plugin = Plugin::spawn();
    let due = 1_798_106_400_250u64;
    let reply = b.call(
        &mut plugin,
        "create_card",
        json!({"board": format!("https://trello.com/b/{BOARD}/afkd"), "list": "Backlog",
               "name": NAME, "description": "## 背景\n\nIt retries forever.", "at": "top",
               "labels": ["bug", "ready"], "members": ["me", "phil"], "due": due}),
    );
    let card = handle(&reply);
    let id = card["key"]
        .as_str()
        .unwrap()
        .strip_prefix("trello:")
        .expect("a card key")
        .to_string();
    assert_eq!(card["id"].as_str().unwrap().len(), 8, "the short link");
    let fields = &card["fields"];
    assert_eq!(fields["name"], NAME);
    assert_eq!(fields["title"], NAME);
    assert_eq!(fields["number"], 5);
    assert_eq!(fields["description"], "## 背景\n\nIt retries forever.");
    assert_eq!(fields["board"], BOARD);
    assert_eq!(fields["list"], "Backlog");
    assert_eq!(fields["labels"], json!(["bug", "ready"]));
    assert_eq!(fields["members"], json!([ME, "phil"]));
    assert_eq!(fields["due"], due);
    assert_eq!(fields["due_complete"], false);
    assert_eq!(fields["archived"], false);
    assert!(fields["url"]
        .as_str()
        .unwrap()
        .starts_with("https://trello.com/c/"));
    assert_eq!(b.fake.due_millis_of(&id), Some(due), "to the millisecond");

    // The card made is the card `trello.cards` hands back, field for field.
    let read = plugin.read(
        "cards",
        b.args(json!({"board": BOARD, "list": ["Backlog"]})),
    );
    let same = handles(&read)
        .iter()
        .find(|h| h["key"] == card["key"])
        .expect("the card is on the board");
    assert_eq!(same, card);

    // A card with no options is a bare one at the bottom.
    let bare = b.call(
        &mut plugin,
        "create_card",
        json!({"board": BOARD, "list": "Doing", "name": "bare"}),
    );
    let fields = &handle(&bare)["fields"];
    assert_eq!(fields["description"], "");
    assert_eq!(fields["labels"], json!([]));
    assert_eq!(fields["members"], json!([]));
    assert_eq!(fields["due"], Value::Null);
    let stderr = plugin.finish();
    assert!(stderr.contains("[trello] created card"), "{stderr}");
}

/// `create_card` at `.top` and at `.bottom`, and `update_card` moving a card to another list at
/// each — and, with no `at`, at the top — leave it where asked, read back with `trello.cards`;
/// `at` alone places a card in the list it is in.
#[test]
fn a_card_made_or_moved_lands_where_it_was_asked() {
    let b = board();
    let mut plugin = Plugin::spawn();
    let make = |plugin: &mut Plugin, extra: Value| -> Value {
        let mut args = json!({"board": BOARD, "list": "Backlog", "name": "made"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        handle(&b.call(plugin, "create_card", args)).clone()
    };
    // Left out, `at` is the bottom.
    let bottom = make(&mut plugin, json!({}));
    let bottom = bottom["id"].as_str().unwrap().to_string();
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&["AAAA0001", "BBBB0002", &bottom])
    );
    let top = make(&mut plugin, json!({"at": "top"}));
    let top = top["id"].as_str().unwrap().to_string();
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&[&top, "AAAA0001", "BBBB0002", &bottom])
    );
    let explicit = make(&mut plugin, json!({"at": "bottom"}));
    let explicit = explicit["id"].as_str().unwrap().to_string();
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&[&top, "AAAA0001", "BBBB0002", &bottom, &explicit])
    );

    let update = |plugin: &mut Plugin, link: &str, extra: Value| -> Value {
        let mut args = json!({"card": b.card(link)});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b.call(plugin, "update_card", args)
    };
    // To another list with no `at`: the top, as `move_to` puts it.
    let reply = update(&mut plugin, "CCCC0003", json!({"list": "Backlog"}));
    assert_eq!(handle(&reply)["fields"]["list"], "Backlog");
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&["CCCC0003", &top, "AAAA0001", "BBBB0002", &bottom, &explicit])
    );
    assert_eq!(b.order(&mut plugin, "Doing"), want(&["DDDD0004"]));
    // To another list at the bottom, and at the top.
    update(
        &mut plugin,
        "BBBB0002",
        json!({"list": "Doing", "at": "bottom"}),
    );
    assert_eq!(
        b.order(&mut plugin, "Doing"),
        want(&["DDDD0004", "BBBB0002"])
    );
    update(
        &mut plugin,
        "AAAA0001",
        json!({"list": "Doing", "at": "top"}),
    );
    assert_eq!(
        b.order(&mut plugin, "Doing"),
        want(&["AAAA0001", "DDDD0004", "BBBB0002"])
    );
    // `at` alone places a card in its own list, and so does naming the list it is in.
    update(&mut plugin, "CCCC0003", json!({"at": "bottom"}));
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&[&top, &bottom, &explicit, "CCCC0003"])
    );
    update(
        &mut plugin,
        "CCCC0003",
        json!({"list": "Backlog", "at": "top"}),
    );
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&["CCCC0003", &top, &bottom, &explicit])
    );
    // Naming the list it is in, with no `at`, leaves it where it is.
    update(&mut plugin, "CCCC0003", json!({"list": "Backlog"}));
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&["CCCC0003", &top, &bottom, &explicit])
    );
    plugin.finish();
}

/// `update_card` sends one `PUT` holding only what was given: a name, a description (which `""`
/// empties), a due date to the millisecond (which `nil` clears) and whether it is complete; and
/// with nothing given it sends none, and answers the card as it is.
#[test]
fn update_card_changes_only_what_it_is_given() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    let mut plugin = Plugin::spawn();
    let update = |plugin: &mut Plugin, extra: Value| -> Value {
        let mut args = json!({"card": b.card("AAAA0001")});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b.call(plugin, "update_card", args)
    };
    let last_put = |b: &Board| -> Value {
        let put = b
            .fake
            .seen()
            .into_iter()
            .rev()
            .find(|r| r.method == "PUT")
            .expect("a PUT");
        assert_eq!(put.path, format!("/1/cards/{id}"));
        assert_eq!(put.content_type.as_deref(), Some("application/json"));
        serde_json::from_str(&put.body).expect("a JSON body")
    };

    let reply = update(&mut plugin, json!({"name": NAME}));
    assert_eq!(handle(&reply)["fields"]["name"], NAME);
    assert_eq!(last_put(&b), json!({"name": NAME}));

    let reply = update(&mut plugin, json!({"description": "text 文"}));
    assert_eq!(handle(&reply)["fields"]["description"], "text 文");
    assert_eq!(handle(&reply)["fields"]["name"], NAME, "the name is left");
    assert_eq!(last_put(&b), json!({"desc": "text 文"}));
    let reply = update(&mut plugin, json!({"description": ""}));
    assert_eq!(handle(&reply)["fields"]["description"], "");
    assert_eq!(last_put(&b), json!({"desc": ""}));

    let due = 1_798_106_400_250u64;
    let reply = update(&mut plugin, json!({"due": due, "due_complete": true}));
    assert_eq!(handle(&reply)["fields"]["due"], due);
    assert_eq!(handle(&reply)["fields"]["due_complete"], true);
    assert_eq!(b.fake.due_millis_of(&id), Some(due));
    assert_eq!(
        last_put(&b),
        json!({"due": "2026-12-24T10:00:00.250Z", "dueComplete": true})
    );
    let reply = update(&mut plugin, json!({"due": null}));
    assert_eq!(handle(&reply)["fields"]["due"], Value::Null);
    assert_eq!(handle(&reply)["fields"]["due_complete"], true, "left");
    assert_eq!(b.fake.due_of(&id), None);
    assert_eq!(last_put(&b), json!({"due": null}));

    // Nothing given: no request that writes, and the card as it is.
    let puts = b.count("PUT");
    let reply = update(&mut plugin, json!({}));
    assert_eq!(b.count("PUT"), puts, "no PUT was sent");
    assert_eq!(handle(&reply)["fields"]["name"], NAME);
    assert_eq!(handle(&reply)["id"], "AAAA0001");
    plugin.finish();
}

/// A card is archived and unarchived, each answering the card as Trello now holds it, and
/// deleted for good with its comments and attachments, answering nothing.
#[test]
fn a_card_is_archived_unarchived_and_deleted() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    b.fake.comment(&id, &b.phil, "a word", 60);
    let attached = b.fake.attachment(&id, "note.txt", b"x", 60);
    let mut plugin = Plugin::spawn();
    let card = b.card("AAAA0001");

    let reply = b.call(&mut plugin, "archive_card", json!({"card": card}));
    assert_eq!(handle(&reply)["fields"]["archived"], true);
    assert_eq!(handle(&reply)["key"], card["key"]);
    assert!(b.fake.archived(&id));
    assert_eq!(b.order(&mut plugin, "Backlog"), want(&["BBBB0002"]));
    let kept = plugin.read(
        "cards",
        b.args(json!({"board": BOARD, "list": ["Backlog"], "include_archived": true})),
    );
    assert!(handles(&kept)
        .iter()
        .any(|h| h["id"] == "AAAA0001" && h["fields"]["archived"] == true));

    let reply = b.call(&mut plugin, "unarchive_card", json!({"card": card}));
    assert_eq!(handle(&reply)["fields"]["archived"], false);
    assert!(!b.fake.archived(&id));
    assert_eq!(
        b.order(&mut plugin, "Backlog"),
        want(&["AAAA0001", "BBBB0002"])
    );
    let read = plugin.read(
        "cards",
        b.args(json!({"board": BOARD, "list": ["Backlog"]})),
    );
    assert_eq!(&handles(&read)[0], handle(&reply));

    let reply = b.call(&mut plugin, "delete_card", json!({"card": card}));
    deleted(&reply);
    assert!(!b.fake.has_card(&id));
    assert!(!b.fake.has_attachment(&attached), "its attachments went");
    assert_eq!(b.order(&mut plugin, "Backlog"), want(&["BBBB0002"]));
    // It is gone for good: a second delete is Trello's own 404.
    let again = b.call(&mut plugin, "delete_card", json!({"card": card}));
    assert_eq!(
        refusal(&again),
        "trello delete card: board returned status 404: The requested resource was not found."
    );
    let stderr = plugin.finish();
    for said in ["archived card", "unarchived card", "deleted card"] {
        assert!(
            stderr.contains(&format!("[trello] {said} AAAA0001")),
            "{stderr}"
        );
    }
}

// --- comments ---

/// A comment is written, rewritten and deleted, each write answering the comment as it is now
/// — `trello.comments` reads it back the same — and the card it is on is the short link.
#[test]
fn a_comment_is_written_updated_and_deleted() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    let mut plugin = Plugin::spawn();
    let card = b.card("AAAA0001");
    let read = |plugin: &mut Plugin| plugin.read("comments", b.args(json!({"card": card})));

    let reply = b.call(
        &mut plugin,
        "create_comment",
        json!({"card": card, "body": "看起来不对 🚨\n\n```\nx\n```"}),
    );
    let comment = handle(&reply).clone();
    assert_eq!(comment["fields"]["card"], "AAAA0001");
    assert_eq!(comment["fields"]["body"], "看起来不对 🚨\n\n```\nx\n```");
    assert_eq!(comment["fields"]["author"], ME);
    assert_eq!(comment["fields"]["edited_at"], Value::Null);
    assert_eq!(handles(&read(&mut plugin)), std::slice::from_ref(&comment));

    let reply = b.call(
        &mut plugin,
        "update_comment",
        json!({"comment": arg(&comment), "body": "edited 編集"}),
    );
    let edited = handle(&reply).clone();
    assert_eq!(edited["id"], comment["id"]);
    assert_eq!(edited["key"], comment["key"]);
    assert_eq!(edited["fields"]["body"], "edited 編集");
    assert!(edited["fields"]["edited_at"].as_u64().is_some(), "{edited}");
    assert_eq!(handles(&read(&mut plugin)), std::slice::from_ref(&edited));
    assert_eq!(b.fake.comments(&id)[0].text, "edited 編集");

    let reply = b.call(
        &mut plugin,
        "delete_comment",
        json!({"comment": arg(&edited)}),
    );
    deleted(&reply);
    assert!(handles(&read(&mut plugin)).is_empty());
    assert!(!b.fake.has_comment(edited["id"].as_str().unwrap()));
    plugin.finish();
}

/// Trello lets a token edit and delete only its own comments, so `update_comment` and
/// `delete_comment` on another member's comment are refused in Trello's own words, and the
/// comment is left as it was.
#[test]
fn another_members_comment_is_refused_in_trellos_words() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    let theirs = b.fake.comment(&id, &b.phil, "phil's word", 60);
    let target = json!({"id": theirs, "key": format!("trello:{theirs}")});
    let mut plugin = Plugin::spawn();
    let reply = b.call(
        &mut plugin,
        "update_comment",
        json!({"comment": target, "body": "mine now"}),
    );
    assert_eq!(
        refusal(&reply),
        "trello update comment: board returned status 401: unauthorized permission requested"
    );
    let reply = b.call(&mut plugin, "delete_comment", json!({"comment": target}));
    assert_eq!(
        refusal(&reply),
        "trello delete comment: board returned status 401: unauthorized permission requested"
    );
    assert_eq!(b.fake.comments(&id)[0].text, "phil's word");
    assert!(b.fake.has_comment(&theirs));
    let stderr = plugin.finish();
    assert!(
        stderr.contains("unauthorized permission requested"),
        "{stderr}"
    );
}

// --- checklists ---

/// A checklist is made with its items in the order given, an item added at the bottom and at
/// the top, ticked and renamed, and an item and the checklist deleted, each write answering what
/// it made or changed and `trello.checklists` and `trello.check_items` reading the same back.
#[test]
fn a_checklist_and_its_items_are_made_changed_and_deleted() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    let mut plugin = Plugin::spawn();
    let card = b.card("AAAA0001");
    let lists = |plugin: &mut Plugin| plugin.read("checklists", b.args(json!({"card": card})));
    let items = |plugin: &mut Plugin| plugin.read("check_items", b.args(json!({"card": card})));
    let names = |reply: &Value| -> Vec<String> {
        handles(reply)
            .iter()
            .map(|i| i["fields"]["name"].as_str().unwrap().to_string())
            .collect()
    };

    let reply = b.call(
        &mut plugin,
        "create_checklist",
        json!({"card": card, "name": "Acceptance ✅", "items": ["one", "二", "three"]}),
    );
    let checklist = handle(&reply).clone();
    assert_eq!(checklist["fields"]["card"], "AAAA0001");
    assert_eq!(checklist["fields"]["name"], "Acceptance ✅");
    assert_eq!(checklist["fields"]["items"], json!(["one", "二", "three"]));
    assert_eq!(
        handles(&lists(&mut plugin)),
        std::slice::from_ref(&checklist)
    );
    assert_eq!(names(&items(&mut plugin)), ["one", "二", "three"]);
    assert!(
        handles(&items(&mut plugin))
            .iter()
            .all(|i| i["fields"]["done"] == false),
        "its items are unticked"
    );

    // An item at the bottom — the default — and one at the top.
    let reply = b.call(
        &mut plugin,
        "create_check_item",
        json!({"checklist": arg(&checklist), "name": "four"}),
    );
    let four = handle(&reply).clone();
    let item_id = four["id"].as_str().unwrap().to_string();
    assert_eq!(four["key"], format!("trello:{id}/{item_id}"));
    assert_eq!(four["fields"]["card"], "AAAA0001");
    assert_eq!(four["fields"]["checklist"], checklist["id"]);
    assert_eq!(four["fields"]["name"], "four");
    assert_eq!(four["fields"]["done"], false);
    let reply = b.call(
        &mut plugin,
        "create_check_item",
        json!({"checklist": arg(&checklist), "name": "zero", "at": "top"}),
    );
    let zero = handle(&reply).clone();
    assert_eq!(
        names(&items(&mut plugin)),
        ["zero", "one", "二", "three", "four"]
    );
    assert!(handles(&items(&mut plugin)).contains(&four));
    assert!(handles(&items(&mut plugin)).contains(&zero));
    assert_eq!(
        handles(&lists(&mut plugin))[0]["fields"]["items"],
        json!(["zero", "one", "二", "three", "four"])
    );

    // Ticked, renamed, and both.
    let reply = b.call(
        &mut plugin,
        "update_check_item",
        json!({"item": arg(&four), "done": true}),
    );
    let ticked = handle(&reply).clone();
    assert_eq!(ticked["key"], four["key"]);
    assert_eq!(ticked["fields"]["done"], true);
    assert_eq!(ticked["fields"]["name"], "four");
    assert!(handles(&items(&mut plugin)).contains(&ticked));
    let reply = b.call(
        &mut plugin,
        "update_check_item",
        json!({"item": arg(&four), "name": "4 — 四", "done": false}),
    );
    let renamed = handle(&reply).clone();
    assert_eq!(renamed["fields"]["name"], "4 — 四");
    assert_eq!(renamed["fields"]["done"], false);
    assert!(handles(&items(&mut plugin)).contains(&renamed));
    // Nothing given: no `PUT`, and the item as it is.
    let puts = b.count("PUT");
    let reply = b.call(
        &mut plugin,
        "update_check_item",
        json!({"item": arg(&four)}),
    );
    assert_eq!(b.count("PUT"), puts);
    assert_eq!(handle(&reply), &renamed);

    // An item deleted, then the checklist.
    let reply = b.call(
        &mut plugin,
        "delete_check_item",
        json!({"item": arg(&four)}),
    );
    deleted(&reply);
    assert_eq!(names(&items(&mut plugin)), ["zero", "one", "二", "three"]);
    assert_eq!(
        handles(&lists(&mut plugin))[0]["fields"]["items"],
        json!(["zero", "one", "二", "three"])
    );
    let reply = b.call(
        &mut plugin,
        "delete_checklist",
        json!({"checklist": arg(&checklist)}),
    );
    deleted(&reply);
    assert!(handles(&lists(&mut plugin)).is_empty());
    assert!(handles(&items(&mut plugin)).is_empty());
    assert!(b.fake.checklists_on(&id).is_empty());
    plugin.finish();
}

/// An item Trello refuses part way through `create_checklist` leaves the checklist as far as it
/// got, and the refusal names the checklist and how many of its items were made.
#[test]
fn a_checklist_whose_item_fails_says_how_far_it_got() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    let mut plugin = Plugin::spawn();
    // The fake fails every item, so the first is the one that fails.
    b.fake.fail("create check item", 400);
    let reply = b.call(
        &mut plugin,
        "create_checklist",
        json!({"card": b.card("AAAA0001"), "name": "Acceptance", "items": ["one", "two"]}),
    );
    let made = b.fake.checklists_on(&id);
    assert_eq!(made.len(), 1);
    assert_eq!(
        refusal(&reply),
        format!(
            "create_checklist: checklist \"Acceptance\" ({}) was made on card AAAA0001 with 0 of \
             its 2 items, then: trello create check item: board returned status 400: injected",
            made[0].0
        )
    );
    plugin.finish();
}

// --- attachments ---

/// A file is put on a card as the file's own name, or as `name`, and answers the attachment as
/// Trello holds it — `trello.attachments` reads it back the same, and `trello.download_attachment`
/// saves it byte for byte; an attachment is deleted; a relative path, a file over 10 MiB and one
/// that is not there are each refused before anything is sent.
#[test]
fn an_attachment_is_made_read_back_downloaded_and_deleted() {
    let b = board();
    let id = b.id("AAAA0001").to_string();
    let dir = TempDir::new("writes-attachment");
    let bytes: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let file = dir.path().join("計画 plan.bin");
    std::fs::write(&file, &bytes).unwrap();
    let mut plugin = Plugin::spawn();
    let card = b.card("AAAA0001");
    let listed = |plugin: &mut Plugin| plugin.read("attachments", b.args(json!({"card": card})));

    let reply = b.call(
        &mut plugin,
        "create_attachment",
        json!({"card": card, "path": file}),
    );
    let made = handle(&reply).clone();
    let attachment_id = made["id"].as_str().unwrap().to_string();
    assert_eq!(made["key"], format!("trello:{id}/{attachment_id}"));
    assert_eq!(made["fields"]["card"], "AAAA0001");
    assert_eq!(
        made["fields"]["name"], "計画 plan.bin",
        "the file's own name"
    );
    assert_eq!(made["fields"]["bytes"], 70_000);
    assert_eq!(made["fields"]["is_upload"], true);
    assert_eq!(made["fields"]["mime_type"], "application/octet-stream");
    assert_eq!(handles(&listed(&mut plugin)), std::slice::from_ref(&made));

    let named = b.call(
        &mut plugin,
        "create_attachment",
        json!({"card": card, "path": file, "name": "the plan — 案"}),
    );
    let named = handle(&named).clone();
    assert_eq!(named["fields"]["name"], "the plan — 案");
    assert_eq!(handles(&listed(&mut plugin)).len(), 2);
    assert!(handles(&listed(&mut plugin)).contains(&named));

    let saved = dir.path().join("saved.bin");
    let reply = b.call(
        &mut plugin,
        "download_attachment",
        json!({"attachment": arg(&made), "to": saved}),
    );
    assert_eq!(reply, json!({"ok": true}), "{}", plugin.stderr());
    assert_eq!(std::fs::read(&saved).unwrap(), bytes, "byte for byte");
    assert_eq!(
        b.fake.attachments(&id),
        [
            ("計画 plan.bin".to_string(), bytes.clone()),
            ("the plan — 案".to_string(), bytes.clone())
        ]
    );

    let reply = b.call(
        &mut plugin,
        "delete_attachment",
        json!({"attachment": arg(&made)}),
    );
    deleted(&reply);
    assert!(!b.fake.has_attachment(&attachment_id));
    assert_eq!(handles(&listed(&mut plugin)), &[named]);

    // Each refusal comes before any request that writes.
    let posts = b.count("POST");
    let nowhere = dir.path().join("missing.bin");
    let huge = dir.path().join("huge.bin");
    std::fs::write(&huge, vec![0u8; 10 * 1024 * 1024 + 1]).unwrap();
    for (path, sentence) in [
        (
            json!("relative.bin"),
            "create_attachment: path must be absolute, not \"relative.bin\"".to_string(),
        ),
        (
            json!(nowhere),
            format!("attach: no file at {}", nowhere.display()),
        ),
        (
            json!(huge),
            format!(
                "attach: {} is over the 10 MiB Trello takes in one attachment",
                huge.display()
            ),
        ),
    ] {
        let reply = b.call(
            &mut plugin,
            "create_attachment",
            json!({"card": card, "path": path}),
        );
        assert_eq!(refusal(&reply), sentence);
    }
    assert_eq!(b.count("POST"), posts, "nothing was sent");
    plugin.finish();
}

// --- refusals ---

/// A list, a label and a member the board lacks are each refused naming the ones there are, a
/// list name and a label name two share each listing their ids, and a description over Trello's
/// 16 384 characters before anything is sent; and in every case nothing is written.
#[test]
fn create_card_refuses_a_name_the_board_lacks_or_shares() {
    let b = board();
    let mut plugin = Plugin::spawn();
    let make = |plugin: &mut Plugin, extra: Value| -> Value {
        let mut args = json!({"board": BOARD, "list": "Backlog", "name": "n"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b.call(plugin, "create_card", args)
    };
    let labels = plugin.read("labels", b.args(json!({"board": BOARD})));
    let dup_labels: Vec<String> = handles(&labels)
        .iter()
        .filter(|l| l["fields"]["name"] == "dup")
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(dup_labels.len(), 2);

    let reply = make(&mut plugin, json!({"list": "Nowhere"}));
    assert_eq!(
        refusal(&reply),
        "create_card: the board has no list named \"Nowhere\"; its lists are \"Backlog\", \
         \"Doing\", \"Twin\""
    );
    let reply = make(&mut plugin, json!({"labels": ["bug", "nope"]}));
    assert_eq!(
        refusal(&reply),
        "create_card: the board has no label named \"nope\"; its labels are \"bug\", \"ready\", \
         \"dup\""
    );
    let reply = make(&mut plugin, json!({"members": ["phil", "ghost"]}));
    assert_eq!(
        refusal(&reply),
        format!(
            "create_card: the board has no member named \"ghost\"; its members are \"{ME}\", \
             \"phil\", \"chen\""
        )
    );
    let twins: Vec<String> = {
        let reply = plugin.read("lists", b.args(json!({"board": BOARD})));
        handles(&reply)
            .iter()
            .filter(|l| l["fields"]["name"] == "Twin")
            .map(|l| l["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(twins.len(), 2);
    let reply = make(&mut plugin, json!({"list": "Twin"}));
    assert_eq!(
        refusal(&reply),
        format!(
            "create_card: 2 lists are named \"Twin\" (ids {}, {}); name one of them by renaming \
             the other",
            twins[0], twins[1]
        )
    );
    let reply = make(&mut plugin, json!({"labels": ["dup"]}));
    assert_eq!(
        refusal(&reply),
        format!(
            "create_card: 2 labels are named \"dup\" (ids {}, {}); name one of them by renaming \
             the other",
            dup_labels[0], dup_labels[1]
        )
    );
    assert_eq!(
        b.count("POST"),
        0,
        "no card was made, and nothing else written"
    );
    assert_eq!(b.count("PUT"), 0);

    // A description of 16 385 characters is refused before any request at all.
    let before = b.fake.seen().len();
    let reply = make(&mut plugin, json!({"description": "é".repeat(16_385)}));
    assert_eq!(
        refusal(&reply),
        "create_card: description is 16385 characters, over the 16384 Trello takes"
    );
    assert_eq!(b.fake.seen().len(), before, "no request was sent");
    // 16 384 is a card.
    let reply = make(&mut plugin, json!({"description": "é".repeat(16_384)}));
    assert_eq!(
        handle(&reply)["fields"]["description"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        16_384
    );
    plugin.finish();
}

/// `update_card` resolves a new list against the card's own board, and refuses one it lacks or
/// two share, as `create_card` does, and sends no `PUT`; a body over the cap is refused too.
#[test]
fn update_card_and_the_comment_writes_refuse_what_they_cannot_send() {
    let b = board();
    let mut plugin = Plugin::spawn();
    let card = b.card("AAAA0001");
    let reply = b.call(
        &mut plugin,
        "update_card",
        json!({"card": card, "list": "Nowhere"}),
    );
    assert_eq!(
        refusal(&reply),
        "update_card: the board has no list named \"Nowhere\"; its lists are \"Backlog\", \
         \"Doing\", \"Twin\""
    );
    let reply = b.call(
        &mut plugin,
        "update_card",
        json!({"card": card, "list": "Twin"}),
    );
    assert!(
        refusal(&reply).starts_with("update_card: 2 lists are named \"Twin\" (ids "),
        "{reply}"
    );
    assert_eq!(b.count("PUT"), 0);
    let before = b.fake.seen().len();
    let reply = b.call(
        &mut plugin,
        "create_comment",
        json!({"card": card, "body": "x".repeat(16_385)}),
    );
    assert_eq!(
        refusal(&reply),
        "create_comment: body is 16385 characters, over the 16384 Trello takes"
    );
    let reply = b.call(
        &mut plugin,
        "update_card",
        json!({"card": card, "description": "x".repeat(16_385)}),
    );
    assert_eq!(
        refusal(&reply),
        "update_card: description is 16385 characters, over the 16384 Trello takes"
    );
    assert_eq!(b.fake.seen().len(), before, "no request was sent");
    plugin.finish();
}

// --- a 429, a 5xx ---

/// A 429 on a write is waited out as its rate-limit headers say and the write answers `ok`, in
/// the armed child of a service that has an outbox: the card is made once, and nothing is queued.
#[test]
fn a_429_on_a_write_is_retried_to_ok() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    b.fake.throttle("create card", 2, Some(50));
    let started = Instant::now();
    let reply = plugin.read_armed(
        "create_card",
        b.args(json!({"board": BOARD, "list": "Backlog", "name": "after the wait"})),
    );
    assert_eq!(
        handle(&reply)["fields"]["name"],
        "after the wait",
        "{}",
        plugin.stderr()
    );
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "two waits of 50 ms: {:?}",
        started.elapsed()
    );
    assert_eq!(
        b.fake.requests_to("/1/cards"),
        3,
        "two 429s and the one that made the card"
    );
    assert_eq!(b.fake.cards_in("Backlog").len(), 3, "made once");

    // A 429 on a PUT is retried the same way.
    b.fake.throttle("update card", 1, Some(50));
    let reply = plugin.read_armed(
        "update_card",
        b.args(json!({"card": b.card("AAAA0001"), "name": "renamed"})),
    );
    assert_eq!(handle(&reply)["fields"]["name"], "renamed");
    let id = b.id("AAAA0001");
    let puts = b
        .fake
        .seen()
        .iter()
        .filter(|r| r.method == "PUT" && r.path == format!("/1/cards/{id}"))
        .count();
    assert_eq!(puts, 2, "a 429 and the answer");

    // And on a DELETE.
    b.fake.throttle("delete card", 1, Some(50));
    let reply = plugin.read_armed("delete_card", b.args(json!({"card": b.card("BBBB0002")})));
    deleted(&reply);
    assert!(!b.fake.has_card(b.id("BBBB0002")));

    let stderr = plugin.finish();
    assert!(
        !stderr.contains("queued") && !stderr.contains("owes"),
        "a write is never queued: {stderr}"
    );
}

/// A 429 whose wait would end past the call's deadline is not waited for: the write is refused
/// at once with Trello's last words, after the one request, and nothing is made.
#[test]
fn a_429_lasting_past_the_deadline_is_refused_at_once() {
    let b = board();
    b.fake.throttle("create card", 1_000, Some(60_000));
    let mut plugin = Plugin::spawn();
    let started = Instant::now();
    let reply = b.call(
        &mut plugin,
        "create_card",
        json!({"board": BOARD, "list": "Backlog", "name": "never"}),
    );
    assert_eq!(
        refusal(&reply),
        "trello create card: board returned status 429: API_TOKEN_LIMIT_EXCEEDED"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no wait was taken: {:?}",
        started.elapsed()
    );
    assert_eq!(b.fake.requests_to("/1/cards"), 1);
    assert_eq!(b.fake.cards_in("Backlog").len(), 2, "nothing was made");
    plugin.finish();
}

/// A 5xx on `create_card` is refused at once and never sent twice, since Trello may already
/// have made the card; nothing is queued for later, in the armed child as in a child of its own.
#[test]
fn a_5xx_on_create_card_is_refused_and_never_sent_twice() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    b.fake.fail("create card", 500);
    let started = Instant::now();
    let reply = plugin.read_armed(
        "create_card",
        b.args(json!({"board": BOARD, "list": "Backlog", "name": "never"})),
    );
    assert_eq!(
        refusal(&reply),
        "trello create card: board returned status 500: injected"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no retry was waited for: {:?}",
        started.elapsed()
    );
    assert_eq!(b.fake.requests_to("/1/cards"), 1, "sent once");
    let stderr = plugin.finish();
    assert!(
        stderr.contains("trello create card: board returned status 500"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("queued") && !stderr.contains("owes"),
        "nothing is queued: {stderr}"
    );
    assert_eq!(b.fake.cards_in("Backlog").len(), 2, "no card was made");

    let mut plugin = Plugin::spawn();
    let reply = b.call(
        &mut plugin,
        "create_card",
        json!({"board": BOARD, "list": "Backlog", "name": "never"}),
    );
    assert_eq!(
        refusal(&reply),
        "trello create card: board returned status 500: injected"
    );
    assert_eq!(
        b.fake.requests_to("/1/cards"),
        2,
        "once more, for the new call"
    );
    plugin.finish();
}

/// An account with no token is refused in a reply before any request, a write that is not
/// declared is refused by name, and the armed child's own units are untouched by either.
#[test]
fn a_write_is_refused_before_any_request_for_an_account_it_cannot_use() {
    let b = board();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let before = b.fake.seen().len();
    let reply = plugin.read_armed(
        "delete_card",
        json!({"acct": {"api_key": "k", "token": ""}, "card": b.card("AAAA0001")}),
    );
    assert_eq!(refusal(&reply), "delete_card: acct.token is empty");
    assert_eq!(b.fake.seen().len(), before, "no request was sent");
    let reply = plugin.read_armed("erase_card", b.args(json!({"card": b.card("AAAA0001")})));
    assert_eq!(refusal(&reply), "no action `erase_card`");
    assert_eq!(plugin.poll()["fire"], true, "{}", plugin.stderr());
    plugin.finish();
}
