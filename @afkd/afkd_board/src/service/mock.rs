//! A board that is a mock, for the unit tests of the service and the plugin: one board's lists,
//! labels, cards and comments in memory, answering the [`Api`] the way the board's API does, and
//! recording each call made to it.

use std::cell::RefCell;
use std::rc::Rc;

use super::Api;
use crate::client::{BoardAnswer, CardAnswer, CommentAnswer, LabelAnswer, ListAnswer, RunAnswer};

/// The moment the first comment is made, a whole second: 2026-11-14T09:30:00Z.
const T0: u64 = 1_794_648_600;

/// A rival's claim a comment posted on a card triggers.
#[derive(Debug, Clone)]
enum Rival {
    /// Posted just before ours: the rival is earlier in the order.
    Before(String),
    /// Posted just after ours: the rival is later.
    After(String),
}

#[derive(Debug, Default)]
struct State {
    id: String,
    lists: Vec<String>,
    labels: Vec<String>,
    cards: Vec<CardAnswer>,
    comments: Vec<(i64, CommentAnswer)>,
    /// How many comments have been made: each is stamped a second after the one before.
    stamp: u64,
    calls: Vec<String>,
    failing: Option<String>,
    /// The rival posted when a claim marker is posted on the card.
    rivals: Vec<(i64, Rival)>,
    /// The cards that gain `afkd/claimed` when their comments are read after a marker: a rival
    /// that won the race while we settled.
    thieves: Vec<i64>,
}

/// One board, shared by the test and the service that reaches it.
#[derive(Debug, Default)]
pub(crate) struct MockBoard {
    state: RefCell<State>,
}

/// The `Api` a service is handed: a handle on the shared board.
pub(crate) struct Handle(pub(crate) Rc<MockBoard>);

/// `secs` since the epoch as the API writes an instant, `2026-11-14T09:30:00Z`.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

impl MockBoard {
    /// A board with the uuid `board-1`, no lists and no labels.
    pub(crate) fn new() -> Rc<Self> {
        let board = Self::default();
        board.state.borrow_mut().id = "board-1".to_string();
        Rc::new(board)
    }

    pub(crate) fn add_list(&self, name: &str) {
        self.state.borrow_mut().lists.push(name.to_string());
    }

    /// A card at the bottom of `list`, with the next number, which comes back.
    pub(crate) fn add_card(&self, list: &str, title: &str) -> i64 {
        let mut s = self.state.borrow_mut();
        let number = i64::try_from(s.cards.len()).unwrap_or(0) + 1;
        s.cards.push(CardAnswer {
            id: format!("card-{number}"),
            number,
            title: title.to_string(),
            description: String::new(),
            list: list.to_string(),
            labels: Vec::new(),
            done: false,
            blocked: false,
            blocked_by: Vec::new(),
            blocking: Vec::new(),
            archived: false,
            checklist: Vec::new(),
            url: format!("https://app.afkd.sh/acme/boards/train/{number}"),
            created_at: rfc3339(T0),
            updated_at: rfc3339(T0),
        });
        number
    }

    /// Change the card `number` with `edit`.
    pub(crate) fn edit(&self, number: i64, edit: impl FnOnce(&mut CardAnswer)) {
        let mut s = self.state.borrow_mut();
        let card = s.cards.iter_mut().find(|c| c.number == number);
        edit(card.expect("a card"));
    }

    /// Put `label` on the card `number`, and on the board when it lacks it.
    pub(crate) fn label(&self, number: i64, label: &str) {
        let mut s = self.state.borrow_mut();
        if !s.labels.iter().any(|l| l == label) {
            s.labels.push(label.to_string());
        }
        let card = s.cards.iter_mut().find(|c| c.number == number);
        card.expect("a card").labels.push(label.to_string());
    }

    /// The names of the labels on the card `number`.
    pub(crate) fn labels_of(&self, number: i64) -> Vec<String> {
        let s = self.state.borrow();
        let card = s.cards.iter().find(|c| c.number == number);
        card.expect("a card").labels.clone()
    }

    /// A person's comment, `body`, on the card `number`.
    pub(crate) fn person_comment(&self, number: i64, body: &str) {
        self.push_comment(number, body, false);
    }

    /// The comments on the card `number`, oldest first.
    pub(crate) fn comments_on(&self, number: i64) -> Vec<CommentAnswer> {
        let s = self.state.borrow();
        s.comments
            .iter()
            .filter(|(n, _)| *n == number)
            .map(|(_, c)| c.clone())
            .collect()
    }

    /// Whenever a claim marker is posted on the card `number`, a rival's is posted just before
    /// it: the rival is earlier, and wins.
    pub(crate) fn rival_before(&self, number: i64, owner: &str) {
        let rival = Rival::Before(crate::claim::claim_text(owner));
        self.state.borrow_mut().rivals.push((number, rival));
    }

    /// Whenever a claim marker is posted on the card `number`, a rival's is posted just after
    /// it: the rival is later, and loses.
    pub(crate) fn rival_after(&self, number: i64, owner: &str) {
        let rival = Rival::After(crate::claim::claim_text(owner));
        self.state.borrow_mut().rivals.push((number, rival));
    }

    /// When the comments of the card `number` are read, a rival that won while we settled has put
    /// the label on it.
    pub(crate) fn thief_on(&self, number: i64) {
        self.state.borrow_mut().thieves.push(number);
    }

    /// Every call made so far, `name` or `name:argument`, in order.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.state.borrow().calls.clone()
    }

    /// Forget the calls made so far.
    pub(crate) fn forget_calls(&self) {
        self.state.borrow_mut().calls.clear();
    }

    /// Every call named `name` refuses, as a board that does not answer.
    pub(crate) fn fail(&self, name: &str) {
        self.state.borrow_mut().failing = Some(name.to_string());
    }

    pub(crate) fn clear_failure(&self) {
        self.state.borrow_mut().failing = None;
    }

    /// Make a comment by a key (`by_key`) or by a person on the card `number`.
    fn push_comment(&self, number: i64, body: &str, by_key: bool) -> CommentAnswer {
        let mut s = self.state.borrow_mut();
        s.stamp += 1;
        let comment = CommentAnswer {
            id: format!("comment-{}", s.stamp),
            body: body.to_string(),
            by: Some(if by_key { "nightly" } else { "Ed" }.to_string()),
            by_key,
            run: None::<RunAnswer>,
            created_at: rfc3339(T0 + s.stamp),
        };
        s.comments.push((number, comment.clone()));
        comment
    }

    /// Record the call `what`, and refuse it when `name` is the one failing.
    fn call(&self, name: &str, what: &str) -> Result<(), String> {
        let mut s = self.state.borrow_mut();
        s.calls.push(what.to_string());
        if s.failing.as_deref() == Some(name) {
            return Err(format!("{name}: no response (mock failure)"));
        }
        Ok(())
    }

    fn find(&self, number: i64) -> Result<CardAnswer, String> {
        let s = self.state.borrow();
        s.cards
            .iter()
            .find(|c| c.number == number)
            .cloned()
            .ok_or_else(|| format!("card: The board has no card #{number}."))
    }
}

impl Api for Handle {
    fn board(&self) -> Result<BoardAnswer, String> {
        self.0.call("board", "board")?;
        let s = self.0.state.borrow();
        Ok(BoardAnswer {
            id: s.id.clone(),
            name: "Release train".to_string(),
            lists: s
                .lists
                .iter()
                .map(|name| ListAnswer {
                    id: format!("list-{name}"),
                    name: name.clone(),
                })
                .collect(),
            labels: s
                .labels
                .iter()
                .map(|name| LabelAnswer {
                    name: name.clone(),
                    colour: "blue".to_string(),
                })
                .collect(),
        })
    }

    fn open_cards(&self, list: &str) -> Result<Vec<CardAnswer>, String> {
        self.0.call("open_cards", &format!("open_cards:{list}"))?;
        let s = self.0.state.borrow();
        Ok(s.cards
            .iter()
            .filter(|c| c.list == list && !c.archived && !c.done)
            .cloned()
            .collect())
    }

    fn cards_labelled(&self, label: &str) -> Result<Vec<CardAnswer>, String> {
        self.0
            .call("cards_labelled", &format!("cards_labelled:{label}"))?;
        let s = self.0.state.borrow();
        Ok(s.cards
            .iter()
            .filter(|c| c.labels.iter().any(|l| l == label))
            .cloned()
            .collect())
    }

    fn card(&self, number: i64) -> Result<CardAnswer, String> {
        self.0.call("card", &format!("card:{number}"))?;
        self.0.find(number)
    }

    fn comments(&self, number: i64) -> Result<Vec<CommentAnswer>, String> {
        self.0.call("comments", &format!("comments:{number}"))?;
        let steals = self.0.state.borrow().thieves.contains(&number);
        if steals && !self.0.labels_of(number).iter().any(|l| l == "afkd/claimed") {
            self.0.label(number, "afkd/claimed");
        }
        Ok(self.0.comments_on(number))
    }

    fn comment(&self, number: i64, text: &str) -> Result<CommentAnswer, String> {
        self.0.call("comment", &format!("comment:{number}"))?;
        self.0.find(number)?;
        let rivals: Vec<Rival> = self
            .0
            .state
            .borrow()
            .rivals
            .iter()
            .filter(|(n, _)| *n == number)
            .map(|(_, r)| r.clone())
            .collect();
        for rival in &rivals {
            if let Rival::Before(text) = rival {
                self.0.push_comment(number, text, true);
            }
        }
        let mine = self.0.push_comment(number, text, true);
        for rival in &rivals {
            if let Rival::After(text) = rival {
                self.0.push_comment(number, text, true);
            }
        }
        Ok(mine)
    }

    fn add_label(&self, number: i64, label: &str) -> Result<CardAnswer, String> {
        self.0
            .call("add_label", &format!("add_label:{number}:{label}"))?;
        self.0.find(number)?;
        if !self.0.labels_of(number).iter().any(|l| l == label) {
            self.0.label(number, label);
        }
        self.0.find(number)
    }

    fn remove_label(&self, number: i64, label: &str) -> Result<CardAnswer, String> {
        self.0
            .call("remove_label", &format!("remove_label:{number}:{label}"))?;
        self.0.find(number)?;
        if !self.0.state.borrow().labels.iter().any(|l| l == label) {
            return Err(format!("remove_label: The board has no label `{label}`."));
        }
        self.0.edit(number, |c| c.labels.retain(|l| l != label));
        self.0.find(number)
    }
}
