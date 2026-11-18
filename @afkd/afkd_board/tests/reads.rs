//! The reads suite: the real `afkd-afkd_board` exec, sent each read as afkd sends one — a child
//! started for the one call, a `call` with the protocol, an empty settings file and the
//! plugin's directory, and no `hello` — against the fake board an `afkd_board.Board` reaches. No
//! afkd is involved.
//!
//! Every leg ends with the fake's record of requests the plugin should never have sent empty,
//! and every child is held to the wire by [`once`]: one reply, nothing else on stdout, a clean
//! exit.

mod common;

use std::net::TcpListener;

use serde_json::{json, Value};

use common::fake::{FakeBoard, NewCard, Run, Seen, T0};
use common::{board, once, Plugin};

/// The title every field is set on: wide CJK, an emoji and a quote.
const TITLE: &str = "修复 the \"retry\" storm 🚨";
const BODY: &str = "The client retries forever.\n\n```rust\nlet backoff = 0;\n```";

const KEY: &str = "afk_board_acme-0123456789";

/// The board every read is held to, its lists `Todo`, `In progress` and `Done` and its labels
/// `bug`, `afkd` and `hold`, and its cards, each made a minute after the one before:
///
/// - #1, Todo: every field set — two labels, a checklist, a markdown description — and three
///   comments, by Ed, by the key `nightly` in a run, and by Ana;
/// - #2, In progress: `bug`;
/// - #3, Done: done;
/// - #4, Todo: `afkd`, an empty description, blocked by #2, which is not done;
/// - #5, Done: done, archived two hours after T0;
/// - #6, Todo: archived an hour after T0;
/// - #7, In progress: `hold`, blocked by #3, which is done;
/// - #8, Todo: "SECONDS" in its description.
///
/// So the board's order is #1, #4, #8, #2, #7, #3, and with its archived cards #1, #4, #8, #6,
/// #2, #7, #3, #5.
fn world() -> FakeBoard {
    let fake = FakeBoard::start(
        "Release train",
        KEY,
        &["Todo", "In progress", "Done"],
        &[("bug", "red"), ("afkd", "blue"), ("hold", "grey")],
    );
    fake.card(NewCard {
        description: BODY.to_string(),
        labels: vec!["bug".to_string(), "afkd".to_string()],
        checklist: vec![
            ("Reproduce it".to_string(), true),
            ("Bound the backoff".to_string(), false),
        ],
        ..NewCard::new("Todo", TITLE)
    });
    fake.card(NewCard {
        labels: vec!["bug".to_string()],
        ..NewCard::new("In progress", "Cut the tag")
    });
    fake.card(NewCard {
        done: true,
        ..NewCard::new("Done", "Ship 0.1")
    });
    fake.card(NewCard {
        labels: vec!["afkd".to_string()],
        blocked_by: vec![2],
        ..NewCard::new("Todo", "Write the \"release\" notes")
    });
    fake.card(NewCard {
        done: true,
        ..NewCard::new("Done", "Old done")
    });
    fake.card(NewCard::new("Todo", "Archived todo"));
    fake.card(NewCard {
        labels: vec!["hold".to_string()],
        blocked_by: vec![3],
        ..NewCard::new("In progress", "Paused")
    });
    fake.card(NewCard {
        description: "The retry takes SECONDS.".to_string(),
        ..NewCard::new("Todo", "Bound it")
    });
    fake.archive(5, T0 + 7_200);
    fake.archive(6, T0 + 3_600);
    fake.comment(1, "Seen on 決済 — twice.", "Ed", false, None, 600);
    let run = Run {
        machine: "hexagon".to_string(),
        service: "afkd::develop".to_string(),
        instance: 1,
        id: "261118-112008".to_string(),
    };
    fake.comment(1, "Claimed 🚀", "nightly", true, Some(run), 700);
    fake.comment(1, "Thanks!", "Ana", false, None, 800);
    fake
}

/// The reply's ok `handles`, held to be an ok reply.
fn handles(reply: &Value) -> &Vec<Value> {
    assert_eq!(reply["ok"], true, "{reply}");
    reply["handles"].as_array().expect("handles")
}

/// The numbers of the cards a `cards` reply hands back, in order.
fn numbers(reply: &Value) -> Vec<i64> {
    handles(reply)
        .iter()
        .map(|h| h["fields"]["number"].as_i64().expect("a number"))
        .collect()
}

/// `cards` of `fake` with `filters` beside its board.
fn cards(fake: &FakeBoard, filters: Value) -> Value {
    let mut args = filters;
    args["b"] = board(fake);
    once("cards", args).0
}

/// The `error` of a refused reply.
fn refused(reply: &Value) -> &str {
    assert_eq!(reply["ok"], false, "{reply}");
    reply["error"].as_str().expect("an error")
}

/// The paths the fake saw, in order.
fn paths(seen: &[Seen]) -> Vec<&str> {
    seen.iter().map(|s| s.path.as_str()).collect()
}

fn no_invalid(fake: &FakeBoard) {
    assert_eq!(fake.invalid_requests(), Vec::<String>::new());
}

/// A card is read whole: every field the manifest declares, its id its number, its key its
/// board's uuid and its own, its times in unix milliseconds, `updated_at` its newest comment.
#[test]
fn card_answers_the_whole_handle() {
    let fake = world();
    let (reply, stderr) = once("card", json!({"b": board(&fake), "number": 1}));
    assert_eq!(
        reply,
        json!({"ok": true, "handle": {
            "id": "1",
            "key": format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(1)),
            "fields": {
                "number": 1,
                "title": TITLE,
                "description": BODY,
                "list": "Todo",
                "labels": ["bug", "afkd"],
                "archived": false,
                "done": false,
                "blocked_by": [],
                "blocking": [],
                "blocked": false,
                "url": fake.card_url(1),
                "created_at": T0 * 1000,
                "updated_at": (T0 + 800) * 1000,
            }
        }})
    );
    assert_eq!(stderr, "afkd-afkd_board: card: card #1\n");

    let (blocked, _) = once("card", json!({"b": board(&fake), "number": 4}));
    let fields = &blocked["handle"]["fields"];
    assert_eq!(
        (
            &fields["description"],
            &fields["blocked_by"],
            &fields["blocked"]
        ),
        (&json!(""), &json!([2]), &json!(true))
    );
    let (archived, _) = once("card", json!({"b": board(&fake), "number": 6}));
    assert_eq!(archived["handle"]["fields"]["archived"], true);
    let (blocking, _) = once("card", json!({"b": board(&fake), "number": 2}));
    assert_eq!(blocking["handle"]["fields"]["blocking"], json!([4]));

    let (none, stderr) = once("card", json!({"b": board(&fake), "number": 99}));
    assert_eq!(refused(&none), "card: The board has no card #99.");
    assert_eq!(
        stderr,
        "afkd-afkd_board: card: The board has no card #99.\n"
    );
    let seen = fake.seen();
    assert!(seen.iter().all(|s| s.method == "GET"), "{seen:?}");
    assert_eq!(
        paths(&seen[seen.len() - 2..]),
        ["/api/board", "/api/board/cards/99"]
    );
    no_invalid(&fake);
}

/// A card's comments come oldest first: a person's names them, a key's names nobody and
/// carries its run's id, and each is at its card's url.
#[test]
fn comments_come_oldest_first_with_their_author() {
    let fake = world();
    let (card, _) = once("card", json!({"b": board(&fake), "number": 1}));
    let (reply, _) = once(
        "comments",
        json!({"b": board(&fake), "card": {"id": "1", "key": card["handle"]["key"]}}),
    );
    let comments = handles(&reply);
    let url = fake.card_url(1);
    let fields: Vec<Value> = comments.iter().map(|c| c["fields"].clone()).collect();
    assert_eq!(
        fields,
        [
            json!({"card": 1, "body": "Seen on 決済 — twice.", "author": "Ed", "run": null,
                   "created_at": (T0 + 600) * 1000, "url": url}),
            json!({"card": 1, "body": "Claimed 🚀", "author": null, "run": "261118-112008",
                   "created_at": (T0 + 700) * 1000, "url": url}),
            json!({"card": 1, "body": "Thanks!", "author": "Ana", "run": null,
                   "created_at": (T0 + 800) * 1000, "url": url}),
        ]
    );
    for comment in comments {
        let id = comment["id"].as_str().expect("an id");
        assert_eq!(
            comment["key"],
            format!("afkd_board:{}/{id}", fake.board_id())
        );
    }

    fake.delete_account("Ed");
    let (reply, _) = once(
        "comments",
        json!({"b": board(&fake), "card": {"id": "1", "key": card["handle"]["key"]}}),
    );
    let authors: Vec<&Value> = handles(&reply)
        .iter()
        .map(|c| &c["fields"]["author"])
        .collect();
    assert_eq!(authors, [&json!(null), &json!(null), &json!("Ana")]);

    let (none, _) = once(
        "comments",
        json!({"b": board(&fake), "card": card["handle"].clone()}),
    );
    assert_eq!(handles(&none).len(), 3);
    let (empty, _) = once(
        "comments",
        json!({"b": board(&fake), "card": {"id": "2",
               "key": format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(2))}}),
    );
    assert_eq!(handles(&empty), &Vec::<Value>::new());
    no_invalid(&fake);
}

/// The lists come in order, each with its id, and the labels with their colours.
#[test]
fn lists_and_labels_come_with_the_board() {
    let fake = world();
    let (lists, stderr) = once("lists", json!({"b": board(&fake)}));
    assert_eq!(
        lists,
        json!({"ok": true, "value": [
            {"id": fake.list_id("Todo"), "name": "Todo"},
            {"id": fake.list_id("In progress"), "name": "In progress"},
            {"id": fake.list_id("Done"), "name": "Done"},
        ]})
    );
    assert_eq!(stderr, "afkd-afkd_board: lists: 3 lists\n");
    let (labels, _) = once("labels", json!({"b": board(&fake)}));
    assert_eq!(
        labels,
        json!({"ok": true, "value": [
            {"name": "bug", "colour": "red"},
            {"name": "afkd", "colour": "blue"},
            {"name": "hold", "colour": "grey"},
        ]})
    );
    assert_eq!(paths(&fake.seen()), ["/api/board", "/api/board"]);
    no_invalid(&fake);
}

/// An empty key is refused by every read before any request, naming the field.
#[test]
fn an_empty_key_is_refused_by_every_read() {
    let fake = world();
    let b = json!({"key": "", "base_url": fake.base_url()});
    let card =
        json!({"id": "1", "key": format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(1))});
    for (action, args) in [
        ("card", json!({"b": b, "number": 1})),
        ("cards", json!({"b": b})),
        ("comments", json!({"b": b, "card": card})),
        ("lists", json!({"b": b})),
        ("labels", json!({"b": b})),
    ] {
        let (reply, _) = once(action, args);
        assert_eq!(
            refused(&reply),
            format!(
                "{action}: the afkd_board.Board's key is empty; set key to one of the board's API \
                 keys, made in its Board settings"
            )
        );
    }
    assert_eq!(fake.seen(), Vec::<Seen>::new());
}

/// A key the board does not know is the API's own refusal, and a board nobody serves names its
/// host and never the key.
#[test]
fn a_refused_key_and_an_unreachable_board_are_told() {
    let fake = world();
    let (reply, _) = once(
        "lists",
        json!({"b": {"key": "afk_board_revoked", "base_url": fake.base_url()}}),
    );
    assert_eq!(
        refused(&reply),
        "lists: the board key is missing, unknown or revoked"
    );
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let (reply, _) = once(
        "cards",
        json!({"b": {"key": KEY, "base_url": format!("http://127.0.0.1:{port}")}}),
    );
    let error = refused(&reply);
    assert!(
        error.starts_with(&format!(
            "cards: cannot reach the board at 127.0.0.1:{port}: "
        )),
        "{error}"
    );
    assert!(!error.contains(KEY), "{error}");
    no_invalid(&fake);
}

/// A card of another board is refused by `comments`, naming both boards, once the board's own
/// id is read and before any card is.
#[test]
fn a_card_of_another_board_is_refused() {
    let acme = world();
    let globex = FakeBoard::start("Globex", "afk_board_globex", &["Todo"], &[]);
    globex.card(NewCard::new("Todo", "Theirs"));
    let theirs = cards(&globex, json!({}));
    let card = handles(&theirs)[0].clone();
    let (reply, _) = once("comments", json!({"b": board(&acme), "card": card}));
    assert_eq!(
        refused(&reply),
        format!(
            "comments: card is #1 of board {}, and the afkd_board.Board reaches board {}",
            globex.board_id(),
            acme.board_id()
        )
    );
    assert_eq!(paths(&acme.seen()), ["/api/board"]);

    let (reply, _) = once(
        "comments",
        json!({"b": board(&acme), "card": {"id": "1", "key": "linear:org/issue"}}),
    );
    assert_eq!(
        refused(&reply),
        "comments: card is no afkd_board.Card: its id is `1` and its key `linear:org/issue`"
    );
    no_invalid(&acme);
    no_invalid(&globex);
}

/// A list the board lacks is refused naming the lists it has, alone or among known ones, and
/// no card is read.
#[test]
fn an_unknown_list_names_the_lists_the_board_has() {
    let fake = world();
    for list in [json!(["Doing"]), json!(["Todo", "Doing"])] {
        let reply = cards(&fake, json!({ "list": list }));
        assert_eq!(
            refused(&reply),
            "cards: The board has no list `Doing`; it has `Todo`, `In progress` and `Done`."
        );
    }
    assert_eq!(paths(&fake.seen()), ["/api/board", "/api/board"]);
    let reply = cards(&fake, json!({"labels": ["nope"]}));
    assert_eq!(
        refused(&reply),
        "cards: The board has no label `nope`; it has `bug`, `afkd` and `hold`."
    );
    no_invalid(&fake);
}

/// Each filter keeps what it says, and left out keeps every card.
#[test]
fn cards_keeps_what_each_filter_says() {
    let fake = world();
    assert_eq!(numbers(&cards(&fake, json!({}))), [1, 4, 8, 2, 7, 3]);
    assert_eq!(
        numbers(&cards(&fake, json!({"list": ["In progress"]}))),
        [2, 7]
    );
    assert_eq!(numbers(&cards(&fake, json!({"labels": ["bug"]}))), [1, 2]);
    assert_eq!(
        numbers(&cards(&fake, json!({"labels": ["bug", "afkd"]}))),
        [1]
    );
    assert_eq!(
        numbers(&cards(&fake, json!({"labels": []}))),
        [1, 4, 8, 2, 7, 3]
    );
    assert_eq!(
        numbers(&cards(&fake, json!({"without_labels": ["afkd", "hold"]}))),
        [8, 2, 3]
    );
    assert_eq!(numbers(&cards(&fake, json!({"done": true}))), [3]);
    assert_eq!(
        numbers(&cards(&fake, json!({"done": false}))),
        [1, 4, 8, 2, 7]
    );
    assert_eq!(
        numbers(&cards(&fake, json!({"done": null}))),
        [1, 4, 8, 2, 7, 3]
    );
    assert_eq!(numbers(&cards(&fake, json!({"blocked": true}))), [4]);
    assert_eq!(
        numbers(&cards(&fake, json!({"blocked": false}))),
        [1, 8, 2, 7, 3]
    );
    assert_eq!(numbers(&cards(&fake, json!({"text": "retry"}))), [1, 8]);
    assert_eq!(numbers(&cards(&fake, json!({"text": "seconds"}))), [8]);
    assert_eq!(
        numbers(&cards(&fake, json!({"list": []}))),
        Vec::<i64>::new()
    );
    no_invalid(&fake);
}

/// Several lists, written out of the board's order, are read one query each, as written, and
/// merged in the board's order.
#[test]
fn cards_of_several_lists_merge_in_board_order() {
    let fake = world();
    let reply = cards(&fake, json!({"list": ["Done", "Todo"]}));
    assert_eq!(numbers(&reply), [1, 4, 8, 3]);
    let queried: Vec<Vec<(String, String)>> = fake
        .seen()
        .into_iter()
        .filter(|s| s.path == "/api/board/cards")
        .map(|s| s.query)
        .collect();
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
    assert_eq!(
        queried,
        [
            vec![
                pair("list", "Done"),
                pair("archived", "false"),
                pair("limit", "50")
            ],
            vec![
                pair("list", "Todo"),
                pair("archived", "false"),
                pair("limit", "50")
            ],
        ]
    );
    no_invalid(&fake);
}

/// `include_archived` brings the archived cards in, each list's after its others and the last
/// archived first; left out, they stay out.
#[test]
fn include_archived_reads_the_archived_cards_too() {
    let fake = world();
    assert_eq!(
        numbers(&cards(&fake, json!({"include_archived": true}))),
        [1, 4, 8, 6, 2, 7, 3, 5]
    );
    assert_eq!(
        numbers(&cards(
            &fake,
            json!({"include_archived": true, "list": ["Done", "Todo"]})
        )),
        [1, 4, 8, 6, 3, 5]
    );
    assert_eq!(
        numbers(&cards(&fake, json!({"include_archived": true, "limit": 5}))),
        [1, 4, 8, 6, 2]
    );
    assert_eq!(
        numbers(&cards(&fake, json!({"include_archived": false}))),
        [1, 4, 8, 2, 7, 3]
    );
    let reply = cards(&fake, json!({"include_archived": true, "done": true}));
    assert!(handles(&reply)[1]["fields"]["archived"] == true, "{reply}");
    no_invalid(&fake);
}

/// `limit` 1 gives the first card and 250 the first 250; 50 when it is left out; and outside 1
/// to 250 it is refused before any request.
#[test]
fn cards_takes_a_limit_of_1_to_250() {
    let fake = world();
    assert_eq!(numbers(&cards(&fake, json!({"limit": 1}))), [1]);
    for limit in [0, 251] {
        let before = fake.seen().len();
        let reply = cards(&fake, json!({ "limit": limit }));
        assert_eq!(
            refused(&reply),
            format!("cards: limit is 1 to 250, not {limit}")
        );
        assert_eq!(fake.seen().len(), before, "a request was sent");
    }

    let long = FakeBoard::start("Long", "afk_board_long", &["Todo"], &[]);
    for n in 0..260 {
        long.card(NewCard::new("Todo", &format!("card {n}")));
    }
    let all: Vec<i64> = (1..=250).collect();
    assert_eq!(numbers(&cards(&long, json!({"limit": 250}))), all);
    assert_eq!(numbers(&cards(&long, json!({}))), all[..50]);
    no_invalid(&fake);
    no_invalid(&long);
}

/// Two boards are two `afkd_board.Board`s used side by side: each read gives its own board's cards,
/// keyed by its own board, and each board sees its own key alone.
#[test]
fn two_boards_are_read_side_by_side() {
    let acme = world();
    let globex = FakeBoard::start("Globex", "afk_board_globex", &["Inbox"], &[]);
    globex.card(NewCard::new("Inbox", "Theirs"));
    let ours = cards(&acme, json!({"limit": 2}));
    let theirs = cards(&globex, json!({}));
    assert_eq!(numbers(&ours), [1, 4]);
    assert_eq!(numbers(&theirs), [1]);
    assert_eq!(handles(&theirs)[0]["fields"]["title"], "Theirs");
    assert_ne!(acme.board_id(), globex.board_id());
    for (fake, reply) in [(&acme, &ours), (&globex, &theirs)] {
        for handle in handles(reply) {
            let key = handle["key"].as_str().expect("a key");
            assert!(
                key.starts_with(&format!("afkd_board:{}/", fake.board_id())),
                "{key}"
            );
        }
        let bearer = format!("Bearer {}", fake.key());
        assert!(
            fake.seen()
                .iter()
                .all(|s| s.authorization.as_deref() == Some(bearer.as_str())),
            "{:?}",
            fake.seen()
        );
        no_invalid(fake);
    }
}

/// One card read by `card` and by `cards` is one handle: the same id and the same key, which
/// afkd's `==` compares.
#[test]
fn one_card_read_by_two_calls_is_one_handle() {
    let fake = world();
    let (one, _) = once("card", json!({"b": board(&fake), "number": 8}));
    let many = cards(&fake, json!({"list": ["Todo"]}));
    let same = handles(&many)
        .iter()
        .find(|h| h["fields"]["number"] == 8)
        .expect("#8 among the cards");
    assert_eq!(
        (&one["handle"]["id"], &one["handle"]["key"]),
        (&same["id"], &same["key"])
    );
    assert_eq!(one["handle"], *same);
    no_invalid(&fake);
}

/// A kind's call ends the child with the reason, since the plugin declares no kind; a call of
/// another protocol or another action is refused.
#[test]
fn the_wire_serves_one_call_of_protocol_2() {
    let mut plugin = Plugin::spawn();
    plugin.send(&json!({"call": "hello", "proto": 2, "kind": "board", "settings": {}}));
    let (status, stderr) = plugin.exit_unanswered();
    assert!(!status.success(), "{status:?}");
    assert_eq!(
        stderr,
        "afkd-afkd_board: afkd sent `hello`, and @afkd/afkd_board provides no kind\n"
    );

    let fake = world();
    let (reply, _) = once("create_card", json!({"b": board(&fake)}));
    assert_eq!(
        refused(&reply),
        "no action `create_card`; @afkd/afkd_board has card, cards, comments, lists and labels"
    );
    let mut plugin = Plugin::spawn();
    let line = plugin.call_raw(json!({"call": "call", "proto": 1, "action": "lists",
                                      "args": {"b": board(&fake)}}));
    let reply = common::reply_of(&line, common::MAX_CALL_LINE);
    assert_eq!(
        refused(&reply),
        "afkd sent a call of protocol 1, and this plugin speaks plugin protocol 2"
    );
    plugin.finish();
    assert_eq!(fake.seen(), Vec::<Seen>::new());
}
