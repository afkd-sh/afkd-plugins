//! The reads a config calls with a `trello.Account` first, over the real `afkd-trello` exec
//! and the stateful fake Trello: through a child afkd starts for one call, which gets no
//! `hello` and names its `proto` on the call itself, and through the armed trigger child of a
//! `service(trello)`, whose slots may call a read too.
//!
//! Every leg ends by closing stdin and holding the child to the wire's discipline — a clean
//! exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)).

mod common;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{FakeTrello, BOARD, KEY, TOKEN};
use common::{Plugin, TempDir};

/// A card name carrying the delimiter a success line wraps it in, wide CJK, an emoji and an
/// em dash.
const NAME: &str = "修复 the retry storm 🚨 — \"backoff\" resets";

/// The suite's board: five lists — `Doing` twice, and `Old` archived — and eight cards.
struct Rich {
    fake: FakeTrello,
    phil: String,
    chen: String,
    /// The ids of the cards, by short link.
    ids: Vec<(&'static str, String)>,
}

impl Rich {
    /// The id of the card whose short link is `link`.
    fn id(&self, link: &str) -> &str {
        &self
            .ids
            .iter()
            .find(|(l, _)| *l == link)
            .expect("a seeded card")
            .1
    }

    /// `trello.cards` over the board, with `extra` arguments, as afkd binds them.
    fn args(&self, extra: Value) -> Value {
        let mut args = json!({"acct": self.fake.account(),
                              "board": format!("https://trello.com/b/{BOARD}/afkd")});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().expect("an args object").clone());
        args
    }
}

/// The suite's board. The cards are made in an order that is not the board's: the `Review`
/// card first, and the first `Backlog` card moved to the bottom of its list, so a read that
/// ordered by id, or by list and then id, would read it wrong.
///
/// ```text
/// Backlog   BklgTwo2  BklgTre3  Archived (closed)  BklgOne1
/// Doing     DoingOne
/// Doing     DoingTwo
/// Review    RvwOne01
/// Old       OldOne001                  (the list is archived)
/// ```
fn rich() -> Rich {
    let fake = FakeTrello::start();
    let backlog = fake.list("Backlog");
    let doing = fake.list("Doing");
    let doing_again = fake.list("Doing");
    let review = fake.list("Review");
    let old = fake.list("Old");
    let phil = fake.member("phil", "Phil Ek");
    let chen = fake.member("chen", "陳大文");
    let mut ids = Vec::new();
    for (list, link, name, desc) in [
        (&review, "RvwOne01", "Review one", ""),
        (&backlog, "BklgOne1", NAME, "## 背景\n\nIt retries forever."),
        (&backlog, "BklgTwo2", "Backlog two", ""),
        (&backlog, "BklgTre3", "Backlog three", ""),
        (&doing, "DoingOne", "Doing one", ""),
        (&doing_again, "DoingTwo", "Doing two", ""),
        (&old, "OldOne001", "Old one", ""),
        (&backlog, "Archived", "Archived card", ""),
    ] {
        ids.push((link, fake.card_in(list, link, name, desc)));
    }
    let rich = Rich {
        fake,
        phil,
        chen,
        ids,
    };
    let f = &rich.fake;
    f.archive_list(&old);
    f.archive(rich.id("Archived"));
    f.move_card(rich.id("BklgOne1"), "Backlog");
    for (link, label) in [
        ("BklgOne1", "afkd/ready"),
        ("BklgTwo2", "afkd/ready"),
        ("BklgTwo2", "Hold"),
        ("RvwOne01", "afkd/ready"),
        ("BklgTre3", ""),
    ] {
        f.label(rich.id(link), label);
    }
    f.card_member(rich.id("BklgOne1"), &rich.phil);
    f.card_member(rich.id("RvwOne01"), &rich.chen);
    f.card_member(rich.id("RvwOne01"), &f.me());
    f.card_member(rich.id("BklgTre3"), &rich.chen);
    f.due(rich.id("BklgOne1"), 86_400 * 2, false);
    rich
}

/// The `handles` a reply carries, asserting it is an ok one.
fn handles(reply: &Value) -> &Vec<Value> {
    assert_eq!(reply["ok"], true, "{reply}");
    reply["handles"].as_array().expect("a list of handles")
}

/// The `id`s of a reply's handles, in order.
fn ids(reply: &Value) -> Vec<String> {
    handles(reply)
        .iter()
        .map(|h| h["id"].as_str().expect("a handle id").to_string())
        .collect()
}

/// The refusal a reply is, its sentence.
fn refusal(reply: &Value) -> &str {
    assert_eq!(reply["ok"], false, "{reply}");
    reply["error"].as_str().expect("a sentence")
}

/// The unix milliseconds an ObjectId's own second names.
fn made(id: &str) -> u64 {
    u64::from_str_radix(&id[..8], 16).expect("an ObjectId") * 1000
}

/// The suite's own settings for a `service(trello)` over `fake`'s board, for the legs that
/// call a read from an armed child.
fn settings(fake: &FakeTrello) -> Value {
    json!({
        "board": format!("https://trello.com/b/{BOARD}/afkd"),
        "base_url": fake.base_url(),
        "api_key": KEY,
        "token": TOKEN,
        "claim_from": "Backlog",
    })
}

// --- cards ---

/// A board read answers list by list in the board's order, each list top to bottom, leaving
/// out the archived card and the card of the archived list; and each card carries every field
/// of the `Card` handle, filled — checked field for field on the one with a description, a
/// label, a member and a due date.
#[test]
fn cards_reads_a_board_list_by_list_top_to_bottom_with_every_field() {
    let b = rich();
    let mut plugin = Plugin::spawn();
    let reply = plugin.read("cards", b.args(json!({})));
    assert_eq!(
        ids(&reply),
        ["BklgTwo2", "BklgTre3", "BklgOne1", "DoingOne", "DoingTwo", "RvwOne01"],
        "{}",
        plugin.stderr()
    );
    let id = b.id("BklgOne1");
    let due = b.fake.due_of(id).expect("a due date") * 1000;
    assert_eq!(
        handles(&reply)[2],
        json!({
            "id": "BklgOne1",
            "key": format!("trello:{id}"),
            "fields": {
                "number": 2,
                "name": NAME,
                "url": b.fake.url(id),
                "description": "## 背景\n\nIt retries forever.",
                "board": BOARD,
                "list": "Backlog",
                "labels": ["afkd/ready"],
                "members": ["phil"],
                "due": due,
                "due_complete": false,
                "archived": false,
                "created_at": made(id),
                "last_activity_at": b.fake.last_activity(id) * 1000,
            }
        })
    );
    assert!(
        handles(&reply)[2]["fields"].get("title").is_none(),
        "a Card has no title"
    );
    // A card with a colour-only label and no due date: the label is left out, and `due` is nil.
    let bare = &handles(&reply)[1]["fields"];
    assert_eq!(bare["labels"], json!([]));
    assert_eq!(bare["members"], json!(["chen"]));
    assert_eq!(bare["due"], Value::Null);
    // The board by its short link reads the same.
    assert_eq!(
        plugin.read("cards", json!({"acct": b.fake.account(), "board": BOARD})),
        reply
    );
    // Only the board, its lists, its members, its labels and its cards were read.
    let paths: Vec<String> = b.fake.seen().into_iter().map(|r| r.path).collect();
    assert!(
        paths.iter().all(|p| p.starts_with("/1/boards/")),
        "{paths:?}"
    );
    plugin.finish();
}

/// Each filter alone, then together: a list by name, the labels a card carries every one of,
/// the labels it carries none of, the members any of whom is on it — `me` the token's own —
/// and the archived cards and the cards of archived lists that `include_archived` keeps.
#[test]
fn cards_filters_each_alone_and_together() {
    let b = rich();
    let mut plugin = Plugin::spawn();
    for (extra, want) in [
        (
            json!({"list": ["Backlog", "Review"]}),
            vec!["BklgTwo2", "BklgTre3", "BklgOne1", "RvwOne01"],
        ),
        (json!({"list": ["Review"]}), vec!["RvwOne01"]),
        (
            json!({"labels": ["afkd/ready"]}),
            vec!["BklgTwo2", "BklgOne1", "RvwOne01"],
        ),
        (json!({"labels": ["afkd/ready", "Hold"]}), vec!["BklgTwo2"]),
        (
            json!({"without_labels": ["Hold"]}),
            vec!["BklgTre3", "BklgOne1", "DoingOne", "DoingTwo", "RvwOne01"],
        ),
        (
            json!({"without_labels": ["Hold", "afkd/ready"]}),
            vec!["BklgTre3", "DoingOne", "DoingTwo"],
        ),
        (json!({"members": ["phil"]}), vec!["BklgOne1"]),
        (json!({"members": ["chen"]}), vec!["BklgTre3", "RvwOne01"]),
        (json!({"members": ["me"]}), vec!["RvwOne01"]),
        (
            json!({"members": ["phil", "chen"]}),
            vec!["BklgTre3", "BklgOne1", "RvwOne01"],
        ),
        (
            json!({"labels": ["afkd/ready"], "members": ["chen"]}),
            vec!["RvwOne01"],
        ),
        (
            json!({"list": ["Backlog"], "without_labels": ["Hold"], "limit": 1}),
            vec!["BklgTre3"],
        ),
        (
            json!({"include_archived": true}),
            vec![
                "BklgTwo2",
                "BklgTre3",
                "Archived",
                "BklgOne1",
                "DoingOne",
                "DoingTwo",
                "RvwOne01",
                "OldOne001",
            ],
        ),
        (
            json!({"include_archived": true, "list": ["Old"]}),
            vec!["OldOne001"],
        ),
        (
            json!({"include_archived": true, "list": ["Backlog"], "labels": []}),
            vec![],
        ),
    ] {
        let reply = plugin.read("cards", b.args(extra.clone()));
        if want.is_empty() {
            assert!(refusal(&reply).contains("`labels` is empty"), "{reply}");
            continue;
        }
        assert_eq!(ids(&reply), want, "{extra}: {}", plugin.stderr());
    }
    // An archived card is itself marked so, and its list is the one it sits in.
    let reply = plugin.read("cards", b.args(json!({"include_archived": true})));
    let archived: Vec<&Value> = handles(&reply)
        .iter()
        .filter(|h| h["fields"]["archived"] == true)
        .collect();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0]["id"], "Archived");
    assert_eq!(archived[0]["fields"]["list"], "Backlog");
    plugin.finish();
}

/// `limit` cuts the ordered cards at 1 and at 1000, and a read with none gives the first
/// hundred.
#[test]
fn cards_cuts_at_the_limit() {
    let b = rich();
    let mut plugin = Plugin::spawn();
    let one = plugin.read("cards", b.args(json!({"limit": 1})));
    assert_eq!(ids(&one), ["BklgTwo2"]);
    let many = plugin.read("cards", b.args(json!({"limit": 1000})));
    assert_eq!(handles(&many).len(), 6);
    let first_four = plugin.read("cards", b.args(json!({"limit": 4})));
    assert_eq!(
        ids(&first_four),
        ["BklgTwo2", "BklgTre3", "BklgOne1", "DoingOne"]
    );
    for limit in [0, 1001, -1] {
        let reply = plugin.read("cards", b.args(json!({ "limit": limit })));
        assert_eq!(
            refusal(&reply),
            format!("cards: limit {limit} is outside 1 to 1000")
        );
    }
    plugin.finish();
}

/// A board of more than Trello's 1000 cards is read to its end, in two pages, and ordered as a
/// whole: the first thousand, top to bottom, are the oldest, which the newest-first first page
/// does not hold. A default read gives a hundred, and `include_archived` reads the closed
/// cards past the thousand too.
#[test]
fn cards_reads_a_board_of_more_than_a_thousand_cards() {
    let fake = FakeTrello::start();
    let list = fake.list("Big board");
    for n in 1..=1100 {
        fake.card_in(&list, &format!("Big{n:05}"), &format!("Card {n}"), "");
    }
    let mut plugin = Plugin::spawn();
    let args = |extra: Value| {
        let mut args = json!({"acct": fake.account(), "board": BOARD});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };
    let reply = plugin.read("cards", args(json!({"limit": 1000})));
    let got = ids(&reply);
    assert_eq!(got.len(), 1000, "{}", plugin.stderr());
    let want: Vec<String> = (1..=1000).map(|n| format!("Big{n:05}")).collect();
    assert_eq!(got, want, "the oldest thousand, top to bottom");
    let pages: Vec<_> = fake
        .seen()
        .into_iter()
        .filter(|r| r.path.ends_with("/cards"))
        .collect();
    assert_eq!(pages.len(), 2, "two pages of cards: {pages:?}");
    assert!(pages[0].query.iter().all(|(k, _)| k != "before"));
    assert!(
        pages[1].query.iter().any(|(k, _)| k == "before"),
        "the second page is read from below the first's lowest id"
    );

    let default = plugin.read("cards", args(json!({})));
    assert_eq!(handles(&default).len(), 100);
    // Closed cards past the thousand are read too, with `include_archived`.
    fake.archive(&fake.card_in(&list, "Big01101", "Last", ""));
    let all = plugin.read(
        "cards",
        args(json!({"include_archived": true, "limit": 1000})),
    );
    assert_eq!(handles(&all).len(), 1000);
    plugin.finish();
}

/// The names a filter holds are checked against the board before any card is read: a list,
/// label or member it has none of is refused naming the ones it has, and a list name two lists
/// share is refused listing their ids — each costing the four requests that read the board,
/// and not its cards.
#[test]
fn cards_refuses_a_name_the_board_lacks_or_shares() {
    let b = rich();
    let mut plugin = Plugin::spawn();
    let reply = plugin.read("cards", b.args(json!({"list": ["Backlog", "Nowhere"]})));
    assert_eq!(
        refusal(&reply),
        "cards: the board has no list named \"Nowhere\"; its lists are \"Backlog\", \"Doing\", \
         \"Review\""
    );
    let reply = plugin.read("cards", b.args(json!({"list": ["Old"]})));
    assert!(
        refusal(&reply).starts_with("cards: the board has no list named \"Old\""),
        "an archived list is named only with include_archived: {reply}"
    );
    let reply = plugin.read("cards", b.args(json!({"list": ["Doing"]})));
    let sentence = refusal(&reply).to_string();
    // The lists' ids are the ones the `lists` read gives, in the board's order.
    let lists = plugin.read("lists", b.args(json!({})));
    let two: Vec<String> = handles(&lists)
        .iter()
        .filter(|l| l["fields"]["name"] == "Doing")
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(two.len(), 2);
    assert_eq!(
        sentence,
        format!(
            "cards: 2 lists are named \"Doing\" (ids {}, {}); name one of them by renaming the \
             other",
            two[0], two[1]
        )
    );
    let reply = plugin.read("cards", b.args(json!({"labels": ["afkd/ready", "Nope"]})));
    assert_eq!(
        refusal(&reply),
        "cards: the board has no label named \"Nope\"; its labels are \"afkd/ready\", \"Hold\""
    );
    let reply = plugin.read("cards", b.args(json!({"without_labels": ["Nope 🚫"]})));
    assert!(
        refusal(&reply).contains("no label named \"Nope 🚫\""),
        "{reply}"
    );
    let reply = plugin.read("cards", b.args(json!({"members": ["ghost"]})));
    assert_eq!(
        refusal(&reply),
        "cards: the board has no member named \"ghost\"; its members are \"afkd-bot\", \
         \"phil\", \"chen\""
    );
    let reply = plugin.read(
        "cards",
        json!({"acct": b.fake.account(), "board": "Nowhere1"}),
    );
    assert!(
        refusal(&reply).starts_with("cards: no board \"Nowhere1\" on this account"),
        "{reply}"
    );
    plugin.finish();
}

/// A reply past afkd's 8 MiB for a call is refused naming the `limit` to lower, and the same
/// read with a lower one answers.
#[test]
fn a_reply_past_eight_mib_is_refused_naming_the_limit() {
    let fake = FakeTrello::start();
    let list = fake.list("Essays");
    let essay = "ä".repeat(7_000);
    for n in 0..600 {
        fake.card_in(&list, &format!("Essay{n:03}"), "An essay", &essay);
    }
    let mut plugin = Plugin::spawn();
    let reply = plugin.read(
        "cards",
        json!({"acct": fake.account(), "board": BOARD, "limit": 600}),
    );
    let sentence = refusal(&reply);
    assert!(
        sentence.starts_with("cards: the reply comes to ")
            && sentence.ends_with(" bytes, over afkd's 8 MiB for one call; lower limit"),
        "{sentence}"
    );
    let reply = plugin.read(
        "cards",
        json!({"acct": fake.account(), "board": BOARD, "limit": 100}),
    );
    assert_eq!(handles(&reply).len(), 100);
    assert!(plugin.stderr().contains("lower limit"));
    plugin.finish();
}

// --- an account ---

/// An empty key or an empty token is refused naming the field, before anything is sent to
/// Trello; so is an account that is no struct, and one with a field of another type.
#[test]
fn an_account_without_a_key_or_a_token_is_refused_and_nothing_is_sent() {
    let fake = FakeTrello::start();
    let mut plugin = Plugin::spawn();
    for (acct, sentence) in [
        (
            json!({"api_key": "", "token": TOKEN, "base_url": fake.base_url()}),
            "boards: acct.api_key is empty",
        ),
        (
            json!({"api_key": KEY, "token": "", "base_url": fake.base_url()}),
            "boards: acct.token is empty",
        ),
        (json!({"token": TOKEN}), "boards: acct.api_key is empty"),
        (
            json!("an account"),
            "boards: parameter acct must be a trello.Account struct",
        ),
        (
            json!({"api_key": 7, "token": TOKEN}),
            "boards: acct.api_key must be a string",
        ),
    ] {
        let reply = plugin.read("boards", json!({ "acct": acct }));
        assert_eq!(refusal(&reply), sentence, "{acct}");
    }
    let reply = plugin.read("boards", json!({}));
    assert_eq!(refusal(&reply), "boards: parameter acct is required");
    assert!(fake.seen().is_empty(), "no request left the plugin");
    let stderr = plugin.stderr();
    assert!(
        !stderr.contains(KEY) && !stderr.contains(TOKEN),
        "no secret is logged: {stderr}"
    );
    plugin.finish();
}

/// Two accounts reach two fakes side by side in one child: each read answers from its own
/// Trello, in one armed trigger child that also resolves `me` once for each account, and
/// never for the other's.
#[test]
fn two_accounts_reach_two_fakes_in_one_child() {
    let (one, two) = (FakeTrello::start(), FakeTrello::start());
    for (fake, list, link) in [
        (&one, "Alpha list", "Alpha001"),
        (&two, "Beta list", "Beta0001"),
    ] {
        let id = fake.list(list);
        fake.card_in(&id, link, &format!("{link} card"), "");
        let member = fake.member("shared-name", "Shared Name");
        fake.card_member(
            &fake.card_in(&id, &format!("{link}x"), "with a member", ""),
            &member,
        );
        fake.card_member(
            &fake.card_in(&id, &format!("{link}y"), "with me", ""),
            &fake.me(),
        );
    }
    let mut plugin = Plugin::armed(settings(&one));
    let read = |plugin: &mut Plugin, fake: &FakeTrello, extra: Value| {
        let mut args = json!({"acct": fake.account(), "board": BOARD});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        plugin.read_armed("cards", args)
    };
    let mine =
        |plugin: &mut Plugin, fake: &FakeTrello| read(plugin, fake, json!({"members": ["me"]}));
    assert_eq!(
        ids(&read(&mut plugin, &one, json!({}))),
        ["Alpha001", "Alpha001x", "Alpha001y"]
    );
    assert_eq!(
        ids(&read(&mut plugin, &two, json!({}))),
        ["Beta0001", "Beta0001x", "Beta0001y"]
    );
    assert_eq!(ids(&mine(&mut plugin, &one)), ["Alpha001y"]);
    assert_eq!(ids(&mine(&mut plugin, &two)), ["Beta0001y"]);
    assert_eq!(ids(&mine(&mut plugin, &one)), ["Alpha001y"]);
    assert_eq!(ids(&mine(&mut plugin, &two)), ["Beta0001y"]);
    // `me` was resolved once for each account, for the child's life, not once a read.
    assert_eq!(one.requests_to("/1/members/me"), 1);
    assert_eq!(two.requests_to("/1/members/me"), 1);
    plugin.finish();
}

// --- the other reads ---

/// `me` is the member the token acts as — the username as the id, the key from Trello's own
/// id of the member — and `boards` the open boards that member reaches, a closed one not.
#[test]
fn me_and_boards_read_the_token_and_what_it_reaches() {
    let fake = FakeTrello::start();
    fake.board("OtherBrd", "Another 板");
    fake.board("ClosedBd", "Closed");
    fake.close_board("ClosedBd");
    let mut plugin = Plugin::spawn();
    let me = plugin.read("me", json!({"acct": fake.account()}));
    assert_eq!(
        me,
        json!({"ok": true, "handle": {"id": "afkd-bot", "key": format!("trello:{}", fake.me()),
                                      "fields": {"full_name": "afkd"}}})
    );
    let boards = plugin.read("boards", json!({"acct": fake.account()}));
    let got: Vec<(String, String, bool)> = handles(&boards)
        .iter()
        .map(|h| {
            (
                h["id"].as_str().unwrap().to_string(),
                h["fields"]["name"].as_str().unwrap().to_string(),
                h["fields"]["archived"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (BOARD.to_string(), "afkd".to_string(), false),
            ("OtherBrd".to_string(), "Another 板".to_string(), false)
        ]
    );
    assert_eq!(
        handles(&boards)[1]["fields"]["url"],
        "https://trello.com/b/OtherBrd/another"
    );
    assert!(handles(&boards)[0]["key"]
        .as_str()
        .unwrap()
        .starts_with("trello:"));
    plugin.finish();
}

/// `lists` gives a board's open lists in its order, an archived one not, each naming the
/// board by its short link; `members` its members; and a board by its address or its short
/// link is the same.
#[test]
fn lists_and_members_read_a_board() {
    let b = rich();
    let mut plugin = Plugin::spawn();
    let lists = plugin.read("lists", b.args(json!({})));
    let names: Vec<&str> = handles(&lists)
        .iter()
        .map(|l| l["fields"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Backlog", "Doing", "Doing", "Review"]);
    assert!(handles(&lists)
        .iter()
        .all(|l| l["fields"]["board"] == BOARD && l["fields"]["archived"] == false));
    assert_eq!(
        plugin.read("lists", json!({"acct": b.fake.account(), "board": BOARD})),
        lists
    );
    let members = plugin.read("members", b.args(json!({})));
    let got: Vec<(&str, &str)> = handles(&members)
        .iter()
        .map(|m| {
            (
                m["id"].as_str().unwrap(),
                m["fields"]["full_name"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("afkd-bot", "afkd"),
            ("phil", "Phil Ek"),
            ("chen", "陳大文")
        ]
    );
    assert_eq!(handles(&members)[1]["key"], format!("trello:{}", b.phil));
    plugin.finish();
}

/// `labels` gives every label of a board, past the 50 Trello answers unless asked: named and
/// unnamed, coloured and not, each with its colour or `null`, the plugin asking for 1000.
#[test]
fn labels_reads_every_label_of_a_board_past_fifty() {
    let fake = FakeTrello::start();
    fake.board("OtherBrd", "Other");
    for n in 0..120 {
        let (name, color) = match n % 4 {
            0 => (format!("label {n:03} 🏷️"), Some("green")),
            1 => (format!("label {n:03}"), None),
            2 => (String::new(), Some("red")),
            _ => (String::new(), None),
        };
        fake.board_label(BOARD, &name, color);
    }
    fake.board_label("OtherBrd", "not this board's", None);
    let mut plugin = Plugin::spawn();
    let reply = plugin.read("labels", json!({"acct": fake.account(), "board": BOARD}));
    let got = handles(&reply);
    assert_eq!(got.len(), 120, "{}", plugin.stderr());
    assert_eq!(
        got[0]["fields"],
        json!({"name": "label 000 🏷️", "color": "green", "board": BOARD})
    );
    assert_eq!(got[1]["fields"]["color"], Value::Null);
    assert_eq!(
        got[2]["fields"],
        json!({"name": "", "color": "red", "board": BOARD})
    );
    assert_eq!(
        got[3]["fields"],
        json!({"name": "", "color": null, "board": BOARD})
    );
    assert!(fake
        .seen()
        .iter()
        .any(|r| r.path.ends_with("/labels")
            && r.query.iter().any(|(k, v)| k == "limit" && v == "1000")));
    plugin.finish();
}

/// The newest `limit` comments of a card with more than Trello's 1000, oldest first: the
/// last 1000 by default-less asks, the default 200, and one; the same through an armed child.
#[test]
fn comments_gives_the_newest_limit_oldest_first() {
    let b = rich();
    let card = b.id("BklgOne1").to_string();
    let mut posted = Vec::new();
    for n in 0..1_050 {
        posted.push(
            b.fake
                .comment(&card, &b.phil, &format!("comment {n:04}"), 5_000 - n),
        );
    }
    let handle = json!({"id": "BklgOne1", "key": format!("trello:{card}")});
    let mut plugin = Plugin::spawn();
    let bodies = |reply: &Value| -> Vec<String> {
        handles(reply)
            .iter()
            .map(|c| c["fields"]["body"].as_str().unwrap().to_string())
            .collect()
    };
    let all = plugin.read(
        "comments",
        json!({"acct": b.fake.account(), "card": handle, "limit": 1000}),
    );
    let got = bodies(&all);
    assert_eq!(got.len(), 1000, "{}", plugin.stderr());
    assert_eq!(got[0], "comment 0050", "the newest thousand of 1050");
    assert_eq!(got[999], "comment 1049");
    assert_eq!(
        handles(&all)[0]["id"],
        posted[50],
        "each comment is its action's id"
    );
    assert_eq!(handles(&all)[0]["key"], format!("trello:{}", posted[50]));
    assert_eq!(
        handles(&all)[999]["fields"],
        json!({"card": "BklgOne1", "body": "comment 1049", "author": "phil",
               "created_at": made(&posted[999 + 50]), "edited_at": null})
    );

    let default = plugin.read(
        "comments",
        json!({"acct": b.fake.account(), "card": handle}),
    );
    let got = bodies(&default);
    assert_eq!(got.len(), 200, "the default is the newest 200");
    assert_eq!(
        (got[0].as_str(), got[199].as_str()),
        ("comment 0850", "comment 1049")
    );
    let one = plugin.read(
        "comments",
        json!({"acct": b.fake.account(), "card": handle, "limit": 1}),
    );
    assert_eq!(bodies(&one), ["comment 1049"]);
    // From an armed child, whose slot is passed the same handle.
    plugin.finish();

    let mut armed = Plugin::armed(settings(&b.fake));
    let reply = armed.read_armed(
        "comments",
        json!({"acct": b.fake.account(), "card": handle, "limit": 3}),
    );
    assert_eq!(
        bodies(&reply),
        ["comment 1047", "comment 1048", "comment 1049"]
    );
    armed.finish();
}

/// An edited comment carries its last edit, and a comment its author's username.
#[test]
fn a_comment_carries_its_author_and_edit() {
    let b = rich();
    let card = b.id("BklgTwo2").to_string();
    let first = b.fake.comment(&card, &b.chen, "看起来不对 🚨", 600);
    let mut plugin = Plugin::spawn();
    let handle = json!({"id": "BklgTwo2", "key": format!("trello:{card}")});
    b.fake.edit_comment(&first, "看起来不对 🚨 (edited)");
    let reply = plugin.read(
        "comments",
        json!({"acct": b.fake.account(), "card": handle}),
    );
    let fields = &handles(&reply)[0]["fields"];
    assert_eq!(fields["body"], "看起来不对 🚨 (edited)");
    assert_eq!(fields["author"], "chen");
    assert!(fields["edited_at"].as_u64().unwrap() >= fields["created_at"].as_u64().unwrap());
    plugin.finish();
}

/// A card's checklists and their check items come in the card's order — by `pos`, whatever
/// order they were made in — each item in its own; the items' names are the checklist's.
#[test]
fn checklists_and_check_items_come_in_the_cards_order() {
    let b = rich();
    let card = b.id("BklgOne1").to_string();
    // Made in the wrong order: the later checklist sits first.
    let second = b.fake.checklist_at(
        &card,
        "Second",
        20_000.0,
        &[("two-a", false), ("two-b", true)],
    );
    let first = b.fake.checklist_at(
        &card,
        "First",
        10_000.0,
        &[("one-a", true), ("one-b", false), ("one-c", false)],
    );
    b.fake.move_check_item(&first[0], 99_000.0);
    let handle = json!({"id": "BklgOne1", "key": format!("trello:{card}")});
    let mut plugin = Plugin::spawn();
    let lists = plugin.read(
        "checklists",
        json!({"acct": b.fake.account(), "card": handle}),
    );
    let got: Vec<(String, String, Vec<String>)> = handles(&lists)
        .iter()
        .map(|l| {
            (
                l["fields"]["name"].as_str().unwrap().to_string(),
                l["fields"]["card"].as_str().unwrap().to_string(),
                l["fields"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i.as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                "First".to_string(),
                "BklgOne1".to_string(),
                vec![
                    "one-b".to_string(),
                    "one-c".to_string(),
                    "one-a".to_string()
                ]
            ),
            (
                "Second".to_string(),
                "BklgOne1".to_string(),
                vec!["two-a".to_string(), "two-b".to_string()]
            ),
        ]
    );
    assert_eq!(
        handles(&lists)[0]["key"].as_str().unwrap().len(),
        "trello:".len() + 24
    );

    let items = plugin.read(
        "check_items",
        json!({"acct": b.fake.account(), "card": handle}),
    );
    let names: Vec<(&str, bool)> = handles(&items)
        .iter()
        .map(|i| {
            (
                i["fields"]["name"].as_str().unwrap(),
                i["fields"]["done"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("one-b", false),
            ("one-c", false),
            ("one-a", true),
            ("two-a", false),
            ("two-b", true)
        ]
    );
    assert_eq!(handles(&items)[0]["id"], first[1]);
    assert_eq!(handles(&items)[3]["id"], second[0]);
    // An item's key carries its card's id, as an attachment's does: Trello has no route to an
    // item that does not go through its card.
    assert_eq!(
        handles(&items)[0]["key"],
        format!("trello:{card}/{}", first[1])
    );
    assert_eq!(
        handles(&items)[3]["key"],
        format!("trello:{card}/{}", second[0])
    );
    assert_eq!(
        handles(&items)[0]["fields"]["checklist"],
        handles(&lists)[0]["id"]
    );
    assert_eq!(handles(&items)[0]["fields"]["card"], "BklgOne1");
    plugin.finish();
}

/// A card with no checklist reads as none, and a card Trello does not have is refused.
#[test]
fn a_card_with_no_checklist_reads_empty_and_a_gone_card_is_refused() {
    let b = rich();
    let handle = json!({"id": "BklgTwo2", "key": format!("trello:{}", b.id("BklgTwo2"))});
    let mut plugin = Plugin::spawn();
    for action in ["checklists", "check_items", "attachments", "comments"] {
        let reply = plugin.read(action, json!({"acct": b.fake.account(), "card": handle}));
        assert!(handles(&reply).is_empty(), "{action}: {reply}");
    }
    let gone = json!({"id": "Gone0001", "key": "trello:6a4dd5de1234abcd5678ef90"});
    let reply = plugin.read(
        "checklists",
        json!({"acct": b.fake.account(), "card": gone}),
    );
    assert_eq!(
        refusal(&reply),
        "trello read checklists: board returned status 404"
    );
    for key in [
        "6a4dd5de1234abcd5678ef90#6a4dd5ff1234abcd5678ef91",
        "trello:x/y",
        "garbage",
    ] {
        let reply = plugin.read(
            "comments",
            json!({"acct": b.fake.account(), "card": {"id": "Gone0001", "key": key}}),
        );
        assert_eq!(
            refusal(&reply),
            format!("comments: card Gone0001 has a key this plugin did not hand out: {key}")
        );
    }
    plugin.finish();
}

// --- attachments ---

/// Attachments come oldest first, uploaded and linked alike: an upload with its size and type,
/// a link with neither, each keyed by its card and its own id.
#[test]
fn attachments_come_oldest_first_files_and_links() {
    let b = rich();
    let card = b.id("BklgOne1").to_string();
    let newest = b
        .fake
        .attachment(&card, "latest.patch", b"diff --git a b\n", 100);
    let oldest = b
        .fake
        .attachment(&card, "plan 計画.md", "## 計画\n".as_bytes(), 300);
    let link = b
        .fake
        .link_attachment(&card, "the spec", "https://example.com/spec", 200);
    let handle = json!({"id": "BklgOne1", "key": format!("trello:{card}")});
    let mut plugin = Plugin::spawn();
    let reply = plugin.read(
        "attachments",
        json!({"acct": b.fake.account(), "card": handle}),
    );
    assert_eq!(
        handles(&reply)
            .iter()
            .map(|a| a["id"].clone())
            .collect::<Vec<_>>(),
        [json!(oldest), json!(link), json!(newest)]
    );
    assert_eq!(
        handles(&reply)[0]["key"],
        format!("trello:{card}/{oldest}"),
        "an attachment's key carries its card, as Trello has no route to it outside the card"
    );
    let file = &handles(&reply)[0]["fields"];
    assert_eq!(file["card"], "BklgOne1");
    assert_eq!(file["name"], "plan 計画.md");
    assert_eq!(file["bytes"], "## 計画\n".len());
    assert_eq!(file["mime_type"], "application/octet-stream");
    assert_eq!(file["is_upload"], true);
    assert!(file["url"]
        .as_str()
        .unwrap()
        .contains(&format!("/attachments/{oldest}/download/")));
    let link = &handles(&reply)[1]["fields"];
    assert_eq!(link["is_upload"], false);
    assert_eq!(link["bytes"], Value::Null);
    assert_eq!(link["mime_type"], Value::Null);
    assert_eq!(link["url"], "https://example.com/spec");
    plugin.finish();
}

/// An uploaded file is saved to the absolute path given, through a temp file that is gone
/// after, byte for byte — the `Authorization` header Trello's download asks for sent, and
/// no credential in the query; a link, a file over 10 MiB, a relative path and a directory
/// that is not there are each refused, the first two before anything is downloaded.
#[test]
fn download_attachment_saves_an_upload_and_refuses_the_rest() {
    let b = rich();
    let card = b.id("BklgOne1").to_string();
    let bytes: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    b.fake.attachment(&card, "blob.bin", &bytes, 300);
    b.fake
        .link_attachment(&card, "the spec", "https://example.com/spec", 200);
    b.fake
        .attachment_sized(&card, "huge.bin", 10 * 1024 * 1024 + 1, 100);
    let handle = json!({"id": "BklgOne1", "key": format!("trello:{card}")});
    let mut plugin = Plugin::spawn();
    let listed = plugin.read(
        "attachments",
        json!({"acct": b.fake.account(), "card": handle}),
    );
    let [blob, link, huge] = [0, 1, 2].map(|at| {
        let a = &handles(&listed)[at];
        json!({"id": a["id"], "key": a["key"]})
    });

    let dir = TempDir::new("download");
    let to = dir.path().join("saved 計画.bin");
    let reply = plugin.read(
        "download_attachment",
        json!({"acct": b.fake.account(), "attachment": blob, "to": to}),
    );
    assert_eq!(reply, json!({"ok": true}), "{}", plugin.stderr());
    assert_eq!(std::fs::read(&to).unwrap(), bytes);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no temp file is left beside it"
    );
    let download = b
        .fake
        .seen()
        .into_iter()
        .find(|r| r.path.contains("/download/"))
        .expect("the file was downloaded");
    assert_eq!(download.authorization, Some(common::fake::oauth_header()));
    assert!(download.query.is_empty(), "{:?}", download.query);

    let downloads = |fake: &FakeTrello| {
        fake.seen()
            .iter()
            .filter(|r| r.path.contains("/download/"))
            .count()
    };
    let before = downloads(&b.fake);
    for (attachment, to, sentence) in [
        (
            link,
            dir.path().join("link"),
            "download_attachment: attachment \"the spec\" is a link to https://example.com/spec, \
             not a file Trello holds"
                .to_string(),
        ),
        (
            huge,
            dir.path().join("huge"),
            "download_attachment: attachment \"huge.bin\" is 10485761 bytes, over the 10 MiB the \
             plugin fetches"
                .to_string(),
        ),
    ] {
        let reply = plugin.read(
            "download_attachment",
            json!({"acct": b.fake.account(), "attachment": attachment, "to": to}),
        );
        assert_eq!(refusal(&reply), sentence);
        assert!(!to.exists());
    }
    assert_eq!(downloads(&b.fake), before, "neither was downloaded");

    let reply = plugin.read(
        "download_attachment",
        json!({"acct": b.fake.account(), "attachment": blob, "to": "relative.bin"}),
    );
    assert_eq!(
        refusal(&reply),
        "download_attachment: to must be absolute, not \"relative.bin\""
    );
    let nowhere = dir.path().join("missing/saved.bin");
    let reply = plugin.read(
        "download_attachment",
        json!({"acct": b.fake.account(), "attachment": blob, "to": nowhere}),
    );
    assert!(
        refusal(&reply).starts_with(&format!(
            "download_attachment: cannot write {}: ",
            nowhere.display()
        )),
        "{reply}"
    );
    let reply = plugin.read(
        "download_attachment",
        json!({"acct": b.fake.account(), "to": to,
               "attachment": {"id": "a9", "key": format!("trello:{card}/6a4dd5de1234abcd5678ef90")}}),
    );
    assert!(
        refusal(&reply).starts_with("download_attachment: no attachment a9 on card"),
        "{reply}"
    );
    plugin.finish();
}

// --- a 429 ---

/// A 429 that carries rate-limit headers is waited out as they say and the read answers `ok`;
/// one that carries none is waited out one second; and neither queues anything, in the armed
/// child that has an outbox.
#[test]
fn a_429_is_retried_to_ok_with_and_without_rate_headers() {
    let b = rich();
    let mut plugin = Plugin::armed(settings(&b.fake));
    b.fake.throttle("labels", 2, Some(50));
    let started = Instant::now();
    let reply = plugin.read_armed("labels", json!({"acct": b.fake.account(), "board": BOARD}));
    assert_eq!(handles(&reply).len(), 3, "{}", plugin.stderr());
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "two waits of 50 ms: {:?}",
        started.elapsed()
    );
    let tries = b
        .fake
        .requests_to(&format!("/1/boards/{}/labels", b.fake.board_id(BOARD)));
    assert_eq!(tries, 3, "two 429s and the answer");

    b.fake.throttle("members", 1, None);
    let started = Instant::now();
    let reply = plugin.read_armed("members", json!({"acct": b.fake.account(), "board": BOARD}));
    assert_eq!(handles(&reply).len(), 3, "{}", plugin.stderr());
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "one second with no header to go by: {:?}",
        started.elapsed()
    );

    let stderr = plugin.finish();
    assert!(
        !stderr.contains("queued") && !stderr.contains("owes"),
        "a read is never queued: {stderr}"
    );
    assert!(
        b.fake.seen().iter().all(|r| r.method == "GET"),
        "nothing was written"
    );
}

/// A 429 whose wait would end past the call's deadline is not waited for: the read is refused
/// at once, with the 429 it last got, after the one request.
#[test]
fn a_429_lasting_past_the_deadline_is_refused_at_once() {
    let b = rich();
    b.fake.throttle("lists", 1_000, Some(60_000));
    let mut plugin = Plugin::spawn();
    let started = Instant::now();
    let reply = plugin.read("lists", b.args(json!({})));
    assert_eq!(
        refusal(&reply),
        "trello read lists: board returned status 429"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no wait was taken: {:?}",
        started.elapsed()
    );
    let board = b.fake.board_id(BOARD);
    assert_eq!(b.fake.requests_to(&format!("/1/boards/{board}/lists")), 1);
    plugin.finish();
}

// --- the claimed card ---

/// A claimed card and the same card read with `trello.cards` are `==`: the same key and every
/// field, though the claim read the card by itself and `cards` read it with its board.
#[test]
fn a_claimed_card_and_the_same_card_read_are_equal() {
    let b = rich();
    let mut plugin = Plugin::armed(settings(&b.fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let unit = &reply["unit"];
    let read = plugin.read_armed("cards", b.args(json!({"list": ["Backlog"]})));
    let same = handles(&read)
        .iter()
        .find(|h| h["key"] == unit["key"])
        .expect("the claimed card is on the board");
    assert_eq!(same["id"], unit["id"]);
    assert_eq!(same["fields"], unit["fields"]);
    assert_eq!(same["key"], format!("trello:{}", b.id("BklgTwo2")));
    plugin.finish();
}
