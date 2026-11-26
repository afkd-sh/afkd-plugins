//! The handlers over a mock board: what each request answers, and when the child ends. The wire
//! itself, a real child with real JSON lines and a real HTTP board, is `tests/service.rs`.

use std::rc::Rc;

use serde_json::{json, Value};

use super::*;
use crate::common::{CaptureDiag, FakeClock, TempDir};
use crate::service::mock::{Handle, MockBoard};

struct Fixture {
    plugin: Plugin,
    board: Rc<MockBoard>,
    diag: Rc<CaptureDiag>,
    dir: TempDir,
}

impl Fixture {
    /// A plugin over a mock board with `Todo` holding one card, not yet armed.
    fn new() -> Self {
        let board = MockBoard::new();
        board.add_list("Todo");
        board.add_card("Todo", "First");
        let diag = Rc::new(CaptureDiag::default());
        let shared = Rc::clone(&board);
        let connect: Connect =
            Rc::new(move |_, _, _| Box::new(Handle(Rc::clone(&shared))) as Box<dyn Api>);
        let plugin = Plugin::with_connect(
            connect,
            Rc::new(FakeClock::new()),
            Rc::clone(&diag) as Rc<dyn Diag>,
            100,
        );
        Self {
            plugin,
            board,
            diag,
            dir: TempDir::new("plugin"),
        }
    }

    fn armed() -> Self {
        let mut f = Self::new();
        let hello = f.hello();
        assert_eq!(f.call(hello)["ok"], true, "{:?}", f.diag.errs());
        f
    }

    /// The `hello` today's afkd writes, naming this fixture's directory.
    fn hello(&self) -> Value {
        json!({"call": "hello", "proto": 2, "kind": "board", "service": "develop",
               "owner": "afkd-4242", "state_dir": self.dir.path(),
               "settings": {"key": "afk_board_k", "claim_from": "Todo"}})
    }

    /// Send one request as afkd writes it, and read the reply line back as JSON.
    fn call(&mut self, request: Value) -> Value {
        match self.plugin.answer(serde_json::from_value(request).unwrap()) {
            Answer::Reply(line) => serde_json::from_str(&line).unwrap(),
            Answer::Fatal(reason) => panic!("unexpected fatal: {reason}"),
        }
    }

    fn fatal(&mut self, request: Value) -> String {
        match self.plugin.answer(serde_json::from_value(request).unwrap()) {
            Answer::Fatal(reason) => reason,
            Answer::Reply(line) => panic!("unexpected reply: {line}"),
        }
    }
}

/// The accepted `hello` lists the one optional call the kind answers, and touches no board.
#[test]
fn hello_lists_release_and_touches_no_board() {
    let mut f = Fixture::new();
    let hello = f.hello();
    assert_eq!(
        f.call(hello),
        json!({"ok": true, "proto": 2, "calls": ["release"]})
    );
    assert!(f.diag.errs().is_empty());
    assert!(f.board.calls().is_empty());
}

/// Each refusal is `ok:false` saying why on the diagnostic channel, and leaves the plugin
/// unarmed: the next `poll` ends the child.
#[test]
fn hello_refuses_what_the_kind_cannot_arm_with_saying_why() {
    let identity = "afkd did not say which service this is; @afkd/afkd_board needs an afkd \
                    whose `hello` carries `service` and `owner`";
    let edit = |f: &Fixture, change: &dyn Fn(&mut Value)| {
        // A `hello` the case's change has altered.
        let mut hello = f.hello();
        change(&mut hello);
        hello
    };
    type Change<'a> = &'a dyn Fn(&mut Value);
    let cases: Vec<(Change, &str)> = vec![
        (
            &|h| h["proto"] = json!(1),
            "afkd speaks plugin protocol 1, and this plugin speaks 2",
        ),
        (
            &|h| {
                h.as_object_mut().unwrap().remove("proto");
            },
            "afkd sent a hello with no protocol, and this plugin speaks 2",
        ),
        (
            &|h| h["kind"] = json!("intake"),
            "kind `intake` is not provided by @afkd/afkd_board",
        ),
        (&|h| h["service"] = json!(""), identity),
        (&|h| h["owner"] = json!(""), identity),
        (
            &|h| {
                h.as_object_mut().unwrap().remove("service");
            },
            identity,
        ),
        (
            &|h| {
                h.as_object_mut().unwrap().remove("state_dir");
            },
            "afkd did not name the plugin's state directory; @afkd/afkd_board needs an afkd \
             whose `hello` carries `state_dir`",
        ),
        (
            &|h| h["settings"]["key"] = json!(""),
            "trigger board: setting `key`: `key` is empty; set it to one of the board's API \
             keys, made in its Board settings",
        ),
        (
            &|h| h["settings"]["claim_from"] = json!([]),
            "trigger board: setting `claim_from`: `claim_from` is required: name the list or \
             lists cards are claimed from",
        ),
        (
            &|h| h["settings"]["claim_from"] = json!(["Todo", "Todo"]),
            "trigger board: setting `claim_from`: the list `Todo` is named twice",
        ),
        (
            &|h| h["settings"]["only_blocked"] = json!("perhaps"),
            "trigger board: setting `only_blocked`: `only_blocked` is `true` or `false`",
        ),
    ];
    for (change, problem) in cases {
        let mut f = Fixture::new();
        let hello = edit(&f, change);
        assert_eq!(
            f.call(hello.clone()),
            json!({"ok": false, "proto": 2}),
            "{hello}"
        );
        assert_eq!(f.diag.errs(), [problem], "{hello}");
        assert!(f
            .fatal(json!({"call": "poll"}))
            .contains("before a `hello`"));
    }
}

/// A refused `hello` after an accepted one unarms the plugin, and an accepted one arms it again.
#[test]
fn a_later_hello_replaces_the_armed_service() {
    let mut f = Fixture::armed();
    let mut bad = f.hello();
    bad["kind"] = json!("other");
    assert_eq!(f.call(bad)["ok"], false);
    assert!(f
        .fatal(json!({"call": "poll"}))
        .contains("before a `hello`"));
    let hello = f.hello();
    assert_eq!(f.call(hello)["ok"], true);
    assert_eq!(f.call(json!({"call": "poll"}))["fire"], true);
}

/// A request before an accepted `hello`, a call of a child's own excepted, ends the child.
#[test]
fn a_request_before_hello_ends_the_child() {
    for request in [
        json!({"call": "poll"}),
        json!({"call": "finish", "key": "afkd_board:b/c", "outcome": "clean"}),
        json!({"call": "release", "key": "afkd_board:b/c"}),
        json!({"call": "renew", "key": "afkd_board:b/c", "renewal": 1}),
        json!({"call": "rewind"}),
    ] {
        let mut f = Fixture::new();
        let reason = f.fatal(request.clone());
        assert_eq!(
            reason, "afkd sent a request before a `hello` this plugin accepted",
            "{request}"
        );
    }
}

/// A call from a later afkd is refused and the next request is still answered.
#[test]
fn an_unknown_call_is_refused_when_armed() {
    let mut f = Fixture::armed();
    assert_eq!(
        f.call(json!({"call": "rewind", "key": "k"})),
        json!({"ok": false})
    );
    assert_eq!(
        f.diag.errs(),
        ["afkd sent a call this plugin did not list in its `hello` reply"]
    );
    assert_eq!(f.call(json!({"call": "poll"}))["fire"], true);
}

/// `poll`, `finish` and `release` reach the service: a unit is handed over, finished with the
/// label left on, and its key released.
#[test]
fn poll_finish_and_release_reach_the_service() {
    let mut f = Fixture::armed();
    let unit = f.call(json!({"call": "poll"}))["unit"].clone();
    assert_eq!(unit["key"], "afkd_board:board-1/card-1");
    assert_eq!(
        f.call(
            json!({"call": "finish", "id": "1", "key": unit["key"], "outcome": "clean",
                      "facts": {"signal": "proceed"}})
        ),
        json!({"ok": true})
    );
    assert_eq!(f.board.labels_of(1), ["afkd/claimed"]);
    assert_eq!(
        f.call(json!({"call": "release", "key": unit["key"]})),
        json!({"released": true})
    );
    assert!(f.board.labels_of(1).is_empty());
    assert_eq!(
        f.call(json!({"call": "release", "key": "garbage"})),
        json!({"released": null})
    );
}

/// The one `call` of a child with no `hello` names its `proto`: a protocol this plugin does not
/// speak, or none, is refused in a reply naming it; an action no one declared is refused naming
/// the ones there are; and a read is decoded, and refused before any request.
#[test]
fn a_one_shot_call_is_served_or_refused_in_a_reply() {
    let mut f = Fixture::new();
    let old = f.call(json!({"call": "call", "proto": 1, "action": "lists"}));
    assert_eq!(
        old,
        json!({"ok": false, "error": "afkd sent a call of protocol 1, and this plugin speaks plugin protocol 2"})
    );
    let none = f.call(json!({"call": "call", "action": "lists"}));
    assert_eq!(
        none["error"],
        "afkd sent a call of no protocol, and this plugin speaks plugin protocol 2"
    );
    let unknown = f.call(json!({"call": "call", "proto": 2, "action": "block_card"}));
    assert_eq!(
        unknown["error"],
        "no action `block_card`; @afkd/afkd_board has card, cards, comments, checklist, lists, \
         labels, create_card, update_card, move_card, add_label, remove_label, add_item, \
         check_item, remove_item, mark_done, add_blocked_by, remove_blocked_by, create_comment, \
         archive_card, unarchive_card, delete_card, create_list, update_list and delete_list"
    );
    let refused = f.call(json!({"call": "call", "proto": 2, "action": "cards",
                                "args": {"b": {"key": "k", "base_url": "http://127.0.0.1:9"},
                                         "limit": 0}}));
    assert_eq!(
        refused,
        json!({"ok": false, "error": "cards: limit is 1 to 250, not 0"})
    );
    assert!(
        f.board.calls().is_empty(),
        "the service's board is never touched"
    );
}

/// An armed child answers a call the same way, a read or a write, and a refusal is a reply that
/// leaves the service claiming on.
#[test]
fn an_armed_child_serves_a_call_and_goes_on_claiming() {
    let mut f = Fixture::armed();
    let b = json!({"key": "afk_board_k", "base_url": "http://127.0.0.1:9"});
    assert_eq!(
        f.call(json!({"call": "call", "action": "cards", "args": {"b": b, "limit": 251}})),
        json!({"ok": false, "error": "cards: limit is 1 to 250, not 251"})
    );
    assert_eq!(
        f.call(json!({"call": "call", "action": "move_card",
                      "args": {"b": b, "card": {"id": "1", "key": "afkd_board:board-1/card-1"}}})),
        json!({"ok": false, "error": "move_card: parameter list is required"})
    );
    assert_eq!(
        f.call(json!({"call": "call", "action": "assign", "args": {}}))["ok"],
        false
    );
    assert_eq!(
        f.call(json!({"call": "poll"}))["fire"],
        true,
        "the service claims on"
    );
}
