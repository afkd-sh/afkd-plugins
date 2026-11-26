//! The service over a mock board: what each call answers, what the board is sent in what
//! order, and the state the service keeps between calls. The wire itself, a real child with
//! real JSON lines and a real HTTP board, is `tests/service.rs`.

use std::rc::Rc;
use std::time::Duration;

use serde_json::{json, Value};

use super::mock::{Handle, MockBoard};
use super::*;
use crate::claim::claim_text;
use crate::common::{CaptureDiag, FakeClock, TempDir};

struct Fixture {
    service: Service,
    board: Rc<MockBoard>,
    diag: Rc<CaptureDiag>,
    clock: Rc<FakeClock>,
    dir: TempDir,
}

const OWNER: &str = "afkd-4242";

fn config() -> ServiceConfig {
    ServiceConfig {
        key: "afk_board_k".into(),
        base_url: "http://127.0.0.1:9".into(),
        claim_from: vec!["Todo".into()],
        only_labels: Vec::new(),
        skip_labels: Vec::new(),
        only_blocked: None,
    }
}

/// A service of `cfg` over `board`, keeping its ledger in `dir`, as the child `pid` of the
/// afkd process `owner`.
fn service_of(
    cfg: ServiceConfig,
    board: &Rc<MockBoard>,
    dir: &std::path::Path,
    owner: &str,
    pid: u32,
    clock: &Rc<FakeClock>,
    diag: &Rc<CaptureDiag>,
) -> Service {
    let shared = Rc::clone(board);
    let connect: Connect =
        Rc::new(move |_, _, _| Box::new(Handle(Rc::clone(&shared))) as Box<dyn Api>);
    Service::new(
        cfg,
        Identity {
            service: "develop".into(),
            owner: owner.into(),
        },
        dir,
        pid,
        connect,
        Rc::clone(clock) as Rc<dyn Clock>,
        Rc::clone(diag) as Rc<dyn Diag>,
    )
}

impl Fixture {
    /// A service of `cfg` over a board with the lists `Todo` and `Later`.
    fn of(cfg: ServiceConfig) -> Self {
        let board = MockBoard::new();
        board.add_list("Todo");
        board.add_list("Later");
        let (clock, diag) = (Rc::new(FakeClock::new()), Rc::new(CaptureDiag::default()));
        let dir = TempDir::new("service");
        let service = service_of(cfg, &board, dir.path(), OWNER, 100, &clock, &diag);
        Self {
            service,
            board,
            diag,
            clock,
            dir,
        }
    }

    fn new() -> Self {
        Self::of(config())
    }

    /// A second child of the same afkd process, on the same board and directory.
    fn sibling(&self, pid: u32) -> Service {
        service_of(
            config(),
            &self.board,
            self.dir.path(),
            OWNER,
            pid,
            &self.clock,
            &self.diag,
        )
    }
}

fn parsed(line: &str) -> Value {
    assert!(
        line.len() <= crate::wire::MAX_REPLY && !line.contains('\n'),
        "{line}"
    );
    serde_json::from_str(line).unwrap()
}

/// The numbers of the cards the board's calls raced for: each claim marker posted.
fn raced(board: &MockBoard) -> Vec<String> {
    board
        .calls()
        .into_iter()
        .filter_map(|c| c.strip_prefix("comment:").map(str::to_string))
        .collect()
}

/// A poll hands over the first card of the first list and puts the label on it, after posting
/// its marker and settling three seconds; the next poll skips what carries the label.
#[test]
fn a_poll_claims_the_first_card_top_to_bottom_and_labels_it() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "First");
    f.board.add_card("Todo", "Second");
    let reply = parsed(&f.service.poll());
    assert_eq!(reply["fire"], true);
    assert_eq!(reply["unit"]["id"], "1");
    assert_eq!(reply["unit"]["key"], "afkd_board:board-1/card-1");
    assert_eq!(f.board.labels_of(1), ["afkd/claimed"]);
    assert_eq!(f.clock.sleeps(), [Duration::from_secs(3)]);
    let marks: Vec<String> = f.board.comments_on(1).into_iter().map(|c| c.body).collect();
    assert_eq!(marks, [claim_text(OWNER)]);
    assert_eq!(
        f.diag.narrated(),
        ["[afkd_board] claimed #1 \"First\"".to_string()]
    );
    assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());

    assert_eq!(parsed(&f.service.poll())["unit"]["id"], "2");
    assert_eq!(parsed(&f.service.poll())["fire"], false);
}

/// The lists are walked in the order written, each top to bottom, and a card a filter holds back
/// is passed over without a marker.
#[test]
fn the_lists_are_walked_in_order_and_a_held_card_is_passed_over() {
    let mut f = Fixture::of(ServiceConfig {
        claim_from: vec!["Later".into(), "Todo".into()],
        skip_labels: vec!["hold".into()],
        ..config()
    });
    f.board.add_card("Todo", "In todo");
    let held = f.board.add_card("Later", "Held");
    f.board.label(held, "hold");
    f.board.add_card("Later", "Free");
    let reply = parsed(&f.service.poll());
    assert_eq!(reply["unit"]["fields"]["title"], "Free");
    assert_eq!(reply["unit"]["fields"]["list"], "Later");
    assert_eq!(
        raced(&f.board),
        ["3"],
        "only the one claimable card was raced"
    );
    assert_eq!(
        f.board.calls()[..3],
        ["board", "open_cards:Later", "comment:3"]
    );
}

/// A rival whose marker is earlier wins the card: the poll moves on to the next card in the same
/// beat. A rival whose marker is later loses it.
#[test]
fn a_loser_takes_the_next_card_in_the_same_poll_and_a_winner_keeps_its_card() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Rival's");
    f.board.add_card("Todo", "Next");
    f.board.rival_before(1, "afkd-9");
    let reply = parsed(&f.service.poll());
    assert_eq!(reply["unit"]["id"], "2");
    assert_eq!(raced(&f.board), ["1", "2"]);
    assert!(f.board.labels_of(1).is_empty(), "the loser labels nothing");
    assert_eq!(f.clock.sleeps().len(), 2);
    assert_eq!(f.diag.narrated()[0], "[afkd_board] lost the claim of #1");

    let mut f = Fixture::new();
    f.board.add_card("Todo", "Ours");
    f.board.rival_after(1, "afkd-9");
    assert_eq!(parsed(&f.service.poll())["unit"]["id"], "1");
}

/// A rival that won while we settled has put the label on: the card is lost though our marker was
/// the first.
#[test]
fn a_card_that_gained_the_label_while_settling_is_lost() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Stolen");
    f.board.add_card("Todo", "Next");
    f.board.thief_on(1);
    assert_eq!(parsed(&f.service.poll())["unit"]["id"], "2");
    assert_eq!(
        f.board.labels_of(1),
        ["afkd/claimed"],
        "the thief's, untouched"
    );
    assert!(
        !f.board
            .calls()
            .contains(&"add_label:1:afkd/claimed".to_string()),
        "{:?}",
        f.board.calls()
    );
}

/// A person's comment that looks like a marker, even an earlier one, never beats ours.
#[test]
fn a_persons_marker_look_alike_does_not_cost_the_claim() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Only");
    f.board.person_comment(1, &claim_text("afkd-9"));
    assert_eq!(parsed(&f.service.poll())["unit"]["id"], "1");
}

/// With too little of the call's 45 s left no new race starts: twelve races of three seconds fit
/// and the thirteenth does not, and the poll says so and is idle.
#[test]
fn no_race_starts_with_too_little_of_the_budget_left() {
    let mut f = Fixture::new();
    for n in 1..=15 {
        f.board.add_card("Todo", &format!("Card {n}"));
        f.board.rival_before(n, "afkd-9");
    }
    assert_eq!(parsed(&f.service.poll()), json!({"fire": false}));
    assert_eq!(raced(&f.board).len(), 12);
    assert_eq!(
        f.diag.errs(),
        [
            "poll: less than 10s of the call's 45s budget is left; the rest of the scan waits for \
          the next poll"
        ]
    );
}

/// A board that does not answer is an idle beat naming the failure, and the next beat, with the
/// board back, claims.
#[test]
fn a_board_error_is_an_idle_beat_and_logged() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Only");
    f.board.fail("board");
    assert_eq!(parsed(&f.service.poll()), json!({"fire": false}));
    assert_eq!(f.diag.errs(), ["board: no response (mock failure)"]);
    f.board.clear_failure();
    assert_eq!(parsed(&f.service.poll())["fire"], true);

    let mut f = Fixture::new();
    f.board.add_card("Todo", "Only");
    f.board.fail("open_cards");
    assert_eq!(parsed(&f.service.poll()), json!({"fire": false}));
    assert_eq!(f.diag.errs(), ["open_cards: no response (mock failure)"]);
}

/// A `claim_from` list the board lacks is an idle beat naming it and the lists the board has, and
/// nothing is raced.
#[test]
fn a_missing_list_is_an_idle_beat_naming_it() {
    let mut f = Fixture::of(ServiceConfig {
        claim_from: vec!["Todo".into(), "Doing".into()],
        ..config()
    });
    f.board.add_card("Todo", "Only");
    assert_eq!(parsed(&f.service.poll()), json!({"fire": false}));
    assert_eq!(
        f.diag.errs(),
        ["poll: The board has no list `Doing`; it has `Todo` and `Later`."]
    );
    assert!(raced(&f.board).is_empty());
}

/// A won card the label cannot be put on is diagnosed and the beat is idle: nothing is handed
/// over, and the service holds no unit.
#[test]
fn a_label_that_cannot_be_put_on_is_diagnosed_and_idle() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Only");
    f.board.fail("add_label");
    assert_eq!(parsed(&f.service.poll()), json!({"fire": false}));
    assert_eq!(
        f.diag.errs(),
        [
            "poll: won the claim of #1 but could not put afkd/claimed on it: add_label: no \
          response (mock failure)"
        ]
    );
    assert_eq!(
        f.service.finish("afkd_board:board-1/card-1", Some("clean")),
        json!({"ok": true}).to_string(),
        "no unit is live"
    );
}

/// The unit's key is the card handle's, its id the card's number, its fields the handle's, its
/// env the address, the credential and the service, and its brief the card whole without the
/// markers.
#[test]
fn the_unit_carries_the_card_handle_its_env_and_a_brief_without_markers() {
    let mut f = Fixture::new();
    let n = f.board.add_card("Todo", "修复 the retry storm 🚨");
    f.board.edit(n, |c| {
        c.description = "It retries forever.\n\n```rust\nlet x = 1;\n```".into();
        c.checklist = vec![
            ItemAnswer {
                text: "Reproduce it".into(),
                done: true,
            },
            ItemAnswer {
                text: "Bound the backoff".into(),
                done: false,
            },
        ];
    });
    f.board.person_comment(n, "看起来不对 🚨");
    f.board
        .person_comment(n, &claim_text("a person quoting a marker"));
    let reply = parsed(&f.service.poll());
    let unit = &reply["unit"];
    assert_eq!(unit["id"], "1");
    assert_eq!(unit["key"], "afkd_board:board-1/card-1");
    assert_eq!(unit["thread"], unit["key"]);
    assert_eq!(unit["self"], OWNER);
    assert_eq!(
        unit["fields"],
        json!({"number": 1, "title": "修复 the retry storm 🚨",
               "description": "It retries forever.\n\n```rust\nlet x = 1;\n```",
               "list": "Todo", "labels": ["afkd/claimed"], "archived": false, "done": false,
               "blocked_by": [], "blocking": [], "blocked": false,
               "url": "https://app.afkd.sh/acme/boards/train/1",
               "created_at": 1_794_648_600_000_u64, "updated_at": 1_794_648_600_000_u64})
    );
    assert_eq!(
        unit["env"],
        json!({"AFKD_BOARD_URL": "http://127.0.0.1:9",
               "AFKD_BOARD_AUTHORIZATION": "Bearer afk_board_k",
               "AFKD_BOARD_RUN": "{\"service\":\"develop\"}"})
    );
    assert_eq!(unit["files"][0]["path"], "task/README.md");
    let brief = unit["files"][0]["text"].as_str().unwrap();
    for part in [
        "# #1 修复 the retry storm 🚨",
        "https://app.afkd.sh/acme/boards/train/1",
        "It retries forever.",
        "- [x] Reproduce it",
        "- [ ] Bound the backoff",
        "**Ed**",
        "看起来不对 🚨",
        "a person quoting a marker",
    ] {
        assert!(brief.contains(part), "{part:?} in:\n{brief}");
    }
    assert!(
        !brief.contains(&claim_text(OWNER)),
        "our own marker is not in the brief:\n{brief}"
    );
}

/// A brief over afkd's line is cut to fit it, with one diagnostic naming the card.
#[test]
fn an_oversized_brief_is_cut_and_said_so() {
    let mut f = Fixture::new();
    let n = f.board.add_card("Todo", "Big");
    f.board.edit(n, |c| {
        c.description = "看起来不对 🚨 — the retry path.\n".repeat(2_600);
    });
    let reply = parsed(&f.service.poll());
    assert_eq!(reply["fire"], true);
    let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with("read the whole card with the afkd_board skill]"),
        "{}",
        &brief[brief.len() - 120..]
    );
    assert_eq!(
        f.diag.errs(),
        [
            "the description field of #1 was cut to fit afkd's 64 KiB plugin line",
            "the brief for #1 was cut to fit afkd's 64 KiB plugin line"
        ]
    );
}

/// `finish` writes the card down and answers ok; the label stays until the child's next poll,
/// which takes it off before it claims anything.
#[test]
fn finish_leaves_the_label_and_the_next_poll_takes_it_off_first() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "First");
    f.board.add_card("Todo", "Second");
    let key = parsed(&f.service.poll())["unit"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        f.service.finish(&key, Some("clean")),
        json!({"ok": true}).to_string()
    );
    assert_eq!(
        f.board.labels_of(1),
        ["afkd/claimed"],
        "finish takes nothing off"
    );
    let owed = Ledger::new(f.dir.path()).entries().unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(
        (
            owed[0].key.as_str(),
            owed[0].number,
            owed[0].board.as_str(),
            owed[0].owner.as_str(),
            owed[0].pid
        ),
        (key.as_str(), 1, "board-1", OWNER, 100)
    );

    f.board.forget_calls();
    assert_eq!(parsed(&f.service.poll())["unit"]["id"], "2");
    assert!(f.board.labels_of(1).is_empty());
    let calls = f.board.calls();
    let off = calls
        .iter()
        .position(|c| c == "remove_label:1:afkd/claimed");
    let post = calls.iter().position(|c| c == "comment:2");
    assert!(
        off < post,
        "the label comes off before the next race: {calls:?}"
    );
    assert_eq!(Ledger::new(f.dir.path()).entries().unwrap(), []);
    assert!(f
        .diag
        .narrated()
        .contains(&"[afkd_board] released #1 (afkd/claimed is off)".to_string()));
}

/// Two children of one afkd process share one directory: a sibling's poll leaves the first's
/// card labelled, and the first's next poll takes it off.
#[test]
fn a_siblings_poll_leaves_a_card_owed_to_another_child() {
    let mut a = Fixture::new();
    a.board.add_card("Todo", "First");
    let key = parsed(&a.service.poll())["unit"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    a.service.finish(&key, Some("clean"));
    let mut b = a.sibling(101);
    assert_eq!(parsed(&b.poll())["fire"], false);
    assert_eq!(a.board.labels_of(1), ["afkd/claimed"], "B leaves A's card");
    assert_eq!(parsed(&a.service.poll())["fire"], false);
    assert!(a.board.labels_of(1).is_empty(), "A takes its own off");
}

/// An entry an earlier afkd process left is taken off by the first child of its board to poll, an
/// entry of another board is left, and nothing is taken twice.
#[test]
fn an_entry_of_an_earlier_afkd_process_is_taken_by_the_first_child_to_poll() {
    let mut a = Fixture::new();
    a.board.add_card("Todo", "Left on");
    a.board.label(1, "afkd/claimed");
    a.board.edit(1, |c| c.done = true);
    let ledger = Ledger::new(a.dir.path());
    let owed = |board: &str, owner: &str, pid| Entry {
        key: format!("afkd_board:{board}/card-1"),
        number: 1,
        board: board.into(),
        owner: owner.into(),
        pid,
    };
    ledger.record(owed("board-1", "afkd-1", 5)).unwrap();
    ledger
        .record(Entry {
            key: "afkd_board:board-2/card-9".into(),
            number: 9,
            ..owed("board-2", "afkd-1", 5)
        })
        .unwrap();
    assert_eq!(parsed(&a.service.poll())["fire"], false);
    assert!(a.board.labels_of(1).is_empty());
    let left = ledger.entries().unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].board, "board-2", "another board's entry is left");
}

/// A removal that fails leaves the entry for the next poll, which retries it.
#[test]
fn a_failed_removal_stays_owed_for_the_next_poll() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "First");
    let key = parsed(&f.service.poll())["unit"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    f.service.finish(&key, None);
    f.board.fail("remove_label");
    f.service.poll();
    assert_eq!(f.board.labels_of(1), ["afkd/claimed"]);
    assert_eq!(Ledger::new(f.dir.path()).entries().unwrap().len(), 1);
    assert!(f.diag.errs()[0].starts_with(
        "poll: could not take afkd/claimed off #1; it stays owed: remove_label: no response"
    ));
    f.board.clear_failure();
    f.service.poll();
    assert!(f.board.labels_of(1).is_empty());
    assert_eq!(Ledger::new(f.dir.path()).entries().unwrap(), []);
}

/// A card that is gone owes nothing: its entry is settled, not retried for ever.
#[test]
fn an_entry_of_a_card_that_is_gone_is_settled() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Other");
    f.board.label(1, "afkd/claimed");
    Ledger::new(f.dir.path())
        .record(Entry {
            key: "afkd_board:board-1/card-7".into(),
            number: 7,
            board: "board-1".into(),
            owner: OWNER.into(),
            pid: 100,
        })
        .unwrap();
    f.service.poll();
    assert_eq!(Ledger::new(f.dir.path()).entries().unwrap(), []);
    assert!(
        f.board
            .calls()
            .contains(&"remove_label:7:afkd/claimed".to_string()),
        "the removal was tried: {:?}",
        f.board.calls()
    );
    assert_eq!(f.diag.errs(), Vec::<String>::new());
}

/// A finish that cannot be written down is `held`, so afkd keeps the journal entry and sends
/// `release`; a finish of a key the service never handed over is ok and diagnosed.
#[test]
fn an_unwritable_finish_is_held_and_an_unknown_key_is_ok_and_diagnosed() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "First");
    let key = parsed(&f.service.poll())["unit"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    // The state directory is a file: nothing can be written under it.
    let file = f.dir.path().join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    f.service.ledger = Ledger::new(&file);
    assert_eq!(
        parsed(&f.service.finish(&key, Some("clean"))),
        json!({"ok": true, "held": true})
    );
    assert!(f.diag.errs()[0].starts_with(&format!(
        "could not write down the release owed for {key}: "
    )));
    assert_eq!(
        parsed(&f.service.finish("afkd_board:board-1/card-9", None)),
        json!({"ok": true})
    );
    assert_eq!(
        f.diag.errs()[1],
        "finish for afkd_board:board-1/card-9, which this plugin holds no claim on"
    );
}

/// `release` takes the label off the card its key names, archived or not, and answers `true`;
/// also for a card that no longer carries it; and forgets what was owed for it.
#[test]
fn release_takes_the_label_off_the_card_its_key_names() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Crashed");
    f.board.add_card("Todo", "Archived");
    f.board.add_card("Todo", "Clean");
    f.board.label(1, "afkd/claimed");
    f.board.label(2, "afkd/claimed");
    f.board.edit(2, |c| c.archived = true);
    let released = |f: &mut Fixture, n: i64| {
        parsed(&f.service.release(&format!("afkd_board:board-1/card-{n}")))
    };
    assert_eq!(released(&mut f, 1), json!({"released": true}));
    assert!(f.board.labels_of(1).is_empty());
    assert_eq!(released(&mut f, 2), json!({"released": true}));
    assert!(f.board.labels_of(2).is_empty(), "an archived card too");
    f.board.forget_calls();
    assert_eq!(released(&mut f, 3), json!({"released": true}));
    assert!(
        !f.board
            .calls()
            .iter()
            .any(|c| c.starts_with("remove_label")),
        "a card without the label is written nothing"
    );
    assert_eq!(
        released(&mut f, 99),
        json!({"released": true}),
        "no such card carries it"
    );
}

/// `release` drops the entry the ledger holds for the key, so the next poll takes nothing off
/// twice.
#[test]
fn release_drops_the_owed_entry_of_its_key() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "First");
    let key = parsed(&f.service.poll())["unit"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    f.service.finish(&key, None);
    assert_eq!(parsed(&f.service.release(&key)), json!({"released": true}));
    assert!(f.board.labels_of(1).is_empty());
    assert_eq!(Ledger::new(f.dir.path()).entries().unwrap(), []);
}

/// A key of another board or of no shape is `null` with a diagnostic, and a board that cannot
/// be reached is `false`: afkd keeps the key.
#[test]
fn release_nulls_a_foreign_key_and_keeps_one_the_board_could_not_answer() {
    let mut f = Fixture::new();
    f.board.add_card("Todo", "Crashed");
    f.board.label(1, "afkd/claimed");
    assert_eq!(
        parsed(&f.service.release("afkd_board:board-2/card-1")),
        json!({"released": null})
    );
    assert_eq!(
        f.diag.errs()[0],
        "release for afkd_board:board-2/card-1, a card of board board-2; this service reaches \
         board board-1"
    );
    for key in ["garbage", "", "linear:ENG-1"] {
        assert_eq!(
            parsed(&f.service.release(key)),
            json!({"released": null}),
            "{key:?}"
        );
    }
    assert_eq!(
        f.board.labels_of(1),
        ["afkd/claimed"],
        "nothing was touched"
    );
    f.board.fail("board");
    assert_eq!(
        parsed(&f.service.release("afkd_board:board-1/card-1")),
        json!({"released": false})
    );
    assert_eq!(f.board.labels_of(1), ["afkd/claimed"]);
}
