//! The plugin's own retries: the writes the board could not take, queued and sent again in
//! the background until they land or an hour has gone by.
//!
//! afkd retries no plugin action, and it waits only 60 seconds for a reply, so a write
//! Trello could not take — the network down, a 429, a 5xx — cannot be retried inside the
//! call that asked for it. It is queued here instead, and the call answers as if it had
//! landed, so the slot's next action still runs (and is queued behind it). One worker
//! thread ([`Outbox::run`]) does the retrying: each card's writes strictly in the order they
//! arrived, the first retry [`RETRY_FIRST`] after the failure, the gap doubling up to
//! [`RETRY_CAP`], for up to [`RETRY_FOR`]. Then it gives up, with one loud line naming the
//! card and the write it lost.
//!
//! A refusal no retry can fix — another 4xx, a list or member the board does not have — is
//! never queued: the one inline try's error is the caller's, as it always was. The queue
//! lives in memory, so a plugin afkd ends names what it still owes ([`Outbox::abandon`]).

use std::collections::{HashSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::board::{BoardClient, BoardError, Card};
use crate::card::{action_line, member_word, perform_action, ACTION_TAG};
use crate::common::{lock, Clock, Diag};
use crate::lifecycle::LifecycleAction;

/// How long after a failed write its first retry comes.
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(5);

/// The longest gap between two tries of one write; the gap doubles up to it.
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(5 * 60);

/// How long a write is retried, from its first failure, before it is given up.
pub(crate) const RETRY_FOR: Duration = Duration::from_secs(60 * 60);

/// How much of a comment a log line quotes.
const QUOTE_MAX: usize = 80;

/// Whether a later try of what failed with `e` can land: the board was not reached, or it
/// answered "too many requests" or a server error. Anything else is the board's answer
/// about the write itself, and asking again changes nothing — a `Decode` included, since
/// the board did answer, and its answer cannot be read.
pub(crate) fn transient(e: &BoardError) -> bool {
    match e {
        BoardError::Transport { .. } => true,
        BoardError::Status { status, .. } => *status == 429 || (500..=599).contains(status),
        BoardError::Decode { .. }
        | BoardError::ListNotFound { .. }
        | BoardError::MemberNotFound { .. } => false,
    }
}

/// One write to a card the plugin owes the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Write {
    /// An action a slot called.
    Act(LifecycleAction),
    /// A comment of the plugin's own: an attempt note, or the discuss path's backstop.
    Note(String),
    /// The run-end `[afkd-ran]` watermark, and the prior watermarks it supersedes, which
    /// are deleted once it has landed.
    Watermark { text: String, priors: Vec<String> },
}

impl Write {
    /// The short phrase every log line names the write by.
    pub(crate) fn what(&self) -> String {
        match self {
            Write::Act(action) => match action {
                LifecycleAction::MoveTo { list, .. } => format!("moving it to \"{list}\""),
                LifecycleAction::AddLabel { name } => format!("adding the label \"{name}\""),
                LifecycleAction::RemoveLabel { name } => {
                    format!("removing the label \"{name}\"")
                }
                LifecycleAction::AddMember(member) => {
                    format!("adding the member \"{}\"", member_word(member))
                }
                LifecycleAction::RemoveMember(member) => {
                    format!("removing the member \"{}\"", member_word(member))
                }
                LifecycleAction::MarkComplete => "marking it complete".to_string(),
                LifecycleAction::Archive => "archiving it".to_string(),
                LifecycleAction::Comment(text) => format!("the comment {}", quoted(text)),
            },
            Write::Note(text) => format!("the comment {}", quoted(text)),
            Write::Watermark { .. } => "the [afkd-ran] watermark".to_string(),
        }
    }
}

/// The first line of `text`, cut to [`QUOTE_MAX`] characters, in quotes: `…` when anything
/// was left out.
fn quoted(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut quote: String = line.chars().take(QUOTE_MAX).collect();
    // A prefix of `text`, so shorter exactly when something was left out.
    if quote.len() < text.len() {
        quote.push('…');
    }
    format!("\"{quote}\"")
}

/// How a log line names a card: its title, which a person reads, and its short link, which
/// finds it.
pub(crate) fn named(card: &Card) -> String {
    format!("card \"{}\" ({})", card.title, card.short_link)
}

/// A span for a log line: `20s`, `4m10s`, `1h`, `1h5m`.
pub(crate) fn span(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

/// The line for a write the plugin stops trying after it failed with `e`, `after` that long
/// in the queue (`None` for a write tried once, inline). A `Decode` says the write may have
/// landed — the board answered, and the reply could not be read — rather than that it is
/// lost.
pub(crate) fn lost_line(
    card: &Card,
    write: &Write,
    e: &BoardError,
    after: Option<Duration>,
) -> String {
    let (card, what) = (named(card), write.what());
    match (e, after) {
        (BoardError::Decode { .. }, _) => format!(
            "trello answered {card} with a reply the plugin could not read; {what} may or may \
             not have landed — {e}"
        ),
        (_, Some(after)) => format!(
            "gave up on {card} after {}: {what} is lost — {e}",
            span(after)
        ),
        (_, None) => format!("{card}: {what} is lost — {e}"),
    }
}

/// The line for a write queued because its inline try could not reach the board.
pub(crate) fn queued_line(card: &Card, write: &Write, e: &BoardError) -> String {
    format!(
        "trello is unreachable for {}: {e}; {} is queued and retried for up to {}",
        named(card),
        write.what(),
        span(RETRY_FOR)
    )
}

/// The line for a write queued behind the writes its card is already owed.
pub(crate) fn queued_behind_line(card: &Card, write: &Write) -> String {
    format!(
        "trello still owes {} earlier writes; {} is queued behind them",
        named(card),
        write.what()
    )
}

/// Do `write` on `card`, once. The watermark's priors are not touched: that is
/// [`prune`]'s, once the watermark has landed.
pub(crate) fn perform(
    board: &dyn BoardClient,
    board_id: &str,
    card: &Card,
    write: &Write,
) -> Result<(), BoardError> {
    match write {
        Write::Act(action) => perform_action(board, board_id, card, action),
        Write::Note(text) | Write::Watermark { text, .. } => {
            board.post_comment(&card.id, text).map(drop)
        }
    }
}

/// Delete the prior watermarks a landed one supersedes. Best-effort, each failure said and
/// dropped: the newest watermark is the one a run reads, and it has landed.
pub(crate) fn prune(board: &dyn BoardClient, card_id: &str, priors: &[String], diag: &dyn Diag) {
    for prior in priors {
        if let Err(e) = board.delete_comment(card_id, prior) {
            diag.err(&e);
        }
    }
}

/// What a write that landed from the queue leaves behind: its success line, and for a
/// watermark, the pruned priors.
fn landed(board: &dyn BoardClient, card: &Card, write: &Write, diag: &dyn Diag) {
    match write {
        Write::Act(action) => diag.narrate(&action_line(action, card)),
        Write::Note(_) | Write::Watermark { .. } => diag.narrate(&format!(
            "{ACTION_TAG} posting {} on card \"{}\"",
            write.what(),
            card.title
        )),
    }
    if let Write::Watermark { priors, .. } = write {
        prune(board, &card.id, priors, diag);
    }
}

/// One write the queue holds.
struct Owed {
    /// Which entry this is, to find it again after its try.
    seq: u64,
    card: Card,
    write: Write,
    /// When it was queued: the start of its [`RETRY_FOR`].
    since: Instant,
    /// When it may be tried next.
    next_try: Instant,
    /// The gap its last failure set, zero before its first try from the queue.
    delay: Duration,
    /// How many tries have failed, the inline one included.
    tries: u32,
    /// Whether the worker is trying it right now. It stays queued meanwhile, so the card
    /// is still owed and a write behind it still waits.
    in_flight: bool,
}

#[derive(Default)]
struct Queue {
    owed: VecDeque<Owed>,
    next_seq: u64,
}

impl Queue {
    /// Each card's oldest entry: the only one of its writes that may be tried.
    fn heads_mut(&mut self) -> impl Iterator<Item = &mut Owed> {
        let mut seen = HashSet::new();
        self.owed
            .iter_mut()
            .filter(move |owed| seen.insert(owed.card.id.clone()))
    }

    /// When the earliest head not being tried is due.
    fn next_due(&mut self) -> Option<Instant> {
        self.heads_mut()
            .filter(|owed| !owed.in_flight)
            .map(|owed| owed.next_try)
            .min()
    }
}

/// The writes the plugin owes the board, in the order they arrived, and the worker that
/// delivers them. The request thread is the only producer and the worker the only
/// consumer; neither holds the lock across a board request.
#[derive(Default)]
pub(crate) struct Outbox {
    queue: Mutex<Queue>,
    /// Signalled on every push, so a waiting worker looks again.
    wake: Condvar,
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        lock(&self.queue)
    }

    /// Whether any write to `card_id` is still owed, the one being tried included: a new
    /// write to it must then queue behind, to keep the card's writes in order.
    pub(crate) fn owes(&self, card_id: &str) -> bool {
        self.lock().owed.iter().any(|owed| owed.card.id == card_id)
    }

    /// Queue `write`, whose inline try failed at `now`, for its first retry.
    pub(crate) fn queue_retry(&self, card: &Card, write: Write, now: Instant) {
        self.push(card, write, now, now + RETRY_FIRST, RETRY_FIRST, 1);
    }

    /// Queue `write` behind the writes `card` is already owed, to be tried as soon as they
    /// have settled.
    pub(crate) fn queue_behind(&self, card: &Card, write: Write, now: Instant) {
        self.push(card, write, now, now, Duration::ZERO, 0);
    }

    fn push(
        &self,
        card: &Card,
        write: Write,
        since: Instant,
        next_try: Instant,
        delay: Duration,
        tries: u32,
    ) {
        let mut queue = self.lock();
        let seq = queue.next_seq;
        queue.next_seq += 1;
        queue.owed.push_back(Owed {
            seq,
            card: card.clone(),
            write,
            since,
            next_try,
            delay,
            tries,
            in_flight: false,
        });
        drop(queue);
        self.wake.notify_all();
    }

    /// Mark the first due head as being tried and hand back what to try.
    fn take_due(&self, now: Instant) -> Option<(u64, Card, Write)> {
        let mut queue = self.lock();
        let owed = queue
            .heads_mut()
            .find(|owed| !owed.in_flight && owed.next_try <= now)?;
        owed.in_flight = true;
        Some((owed.seq, owed.card.clone(), owed.write.clone()))
    }

    /// One pass: try every card's oldest write that is due, and settle each try — landed,
    /// rescheduled, or given up. A card whose write settles has its next one tried in the
    /// same pass, so once the board is back a card's whole backlog lands at once, in order.
    /// When the earliest write left is due next, if any.
    pub(crate) fn deliver_due(
        &self,
        board: &dyn BoardClient,
        board_id: &str,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Option<Instant> {
        while let Some((seq, card, write)) = self.take_due(clock.now()) {
            let tried = perform(board, board_id, &card, &write);
            let now = clock.now();
            let mut queue = self.lock();
            let Some(at) = queue.owed.iter().position(|owed| owed.seq == seq) else {
                continue;
            };
            match tried {
                Ok(()) => {
                    queue.owed.remove(at);
                    drop(queue);
                    landed(board, &card, &write, diag);
                }
                Err(e) if transient(&e) && now < queue.owed[at].since + RETRY_FOR => {
                    let owed = &mut queue.owed[at];
                    owed.in_flight = false;
                    owed.tries += 1;
                    owed.delay = if owed.delay.is_zero() {
                        RETRY_FIRST
                    } else {
                        (owed.delay * 2).min(RETRY_CAP)
                    };
                    owed.next_try = now + owed.delay;
                    let (tries, delay) = (owed.tries, owed.delay);
                    drop(queue);
                    diag.err(&format_args!(
                        "trello is unreachable for {}: {e}; {} is tried again in {} (try {})",
                        named(&card),
                        write.what(),
                        span(delay),
                        tries + 1
                    ));
                }
                Err(e) => {
                    let since = queue.owed.remove(at).map_or(now, |owed| owed.since);
                    drop(queue);
                    diag.err(&lost_line(&card, &write, &e, Some(now - since)));
                }
            }
        }
        self.lock().next_due()
    }

    /// The worker: deliver what is due, then sleep until the next write is due or a new
    /// one is queued. Runs for the life of the process, on its own thread and its own
    /// board client, which no call's deadline ever reaches.
    pub(crate) fn run(
        &self,
        board: &dyn BoardClient,
        board_id: &str,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) {
        loop {
            self.deliver_due(board, board_id, clock, diag);
            let mut queue = self.lock();
            // Looked at again under the lock each time, so a push between the pass and
            // the wait is never missed.
            loop {
                let now = clock.now();
                queue = match queue.next_due() {
                    Some(due) if due <= now => break,
                    Some(due) => {
                        self.wake
                            .wait_timeout(queue, due - now)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    }
                    None => self
                        .wake
                        .wait(queue)
                        .unwrap_or_else(PoisonError::into_inner),
                };
            }
        }
    }

    /// Say every write still owed, one line each: afkd has ended the plugin, and they go
    /// with it.
    pub(crate) fn abandon(&self, diag: &dyn Diag) {
        for owed in &self.lock().owed {
            diag.err(&format_args!(
                "afkd ended the plugin with {} for {} still owed",
                owed.write.what(),
                named(&owed.card)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! The queue and its policy over a mock board, on a fake clock, one pass at a time:
    //! the worker thread itself is `tests/wire.rs`'s.

    use super::*;
    use crate::board::{Action, MockBoard};
    use crate::common::{CaptureDiag, FakeClock};
    use crate::lifecycle::ListPosition;

    /// The card every test writes to: the delimiter a log line wraps it in, wide CJK, an
    /// emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    fn card(id: &str) -> Card {
        Card {
            id: id.into(),
            short_link: format!("sl-{id}"),
            title: TITLE.into(),
            description: String::new(),
            checklists: Vec::new(),
            members: Vec::new(),
            labels: Vec::new(),
            created_at: None,
            comments: None,
        }
    }

    fn board() -> MockBoard {
        let board = MockBoard::new();
        board.add_list("Backlog");
        board.add_card("Backlog", "card1", TITLE, "");
        board.add_card("Backlog", "card2", "第二のカード 🚧", "");
        board
    }

    fn label(name: &str) -> Write {
        Write::Act(LifecycleAction::AddLabel { name: name.into() })
    }

    fn move_to(list: &str) -> Write {
        Write::Act(LifecycleAction::MoveTo {
            list: list.into(),
            position: ListPosition::Bottom,
        })
    }

    #[test]
    fn transient_is_transport_429_and_5xx_only() {
        let status = |status| BoardError::Status {
            stage: "add label",
            status,
        };
        for (code, retried) in [
            (400, false),
            (401, false),
            (404, false),
            (428, false),
            (429, true),
            (499, false),
            (500, true),
            (503, true),
            (599, true),
            (600, false),
        ] {
            assert_eq!(transient(&status(code)), retried, "{code}");
        }
        let stage = "move card";
        assert!(transient(&BoardError::Transport {
            stage,
            reason: "connection refused".into()
        }));
        for refusal in [
            BoardError::Decode {
                stage,
                reason: "expected an object".into(),
            },
            BoardError::ListNotFound {
                stage,
                name: "Backlog".into(),
            },
            BoardError::MemberNotFound {
                stage,
                name: "marisa".into(),
            },
        ] {
            assert!(!transient(&refusal), "{refusal}");
        }
    }

    /// Nothing is tried before it is due, and the gap after each failed try doubles from
    /// [`RETRY_FIRST`] to [`RETRY_CAP`], where it stays; each failure says when the next try
    /// comes.
    #[test]
    fn the_backoff_doubles_from_5s_and_caps_at_5min() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        board.go_down();
        let start = clock.now();
        outbox.queue_retry(&card("card1"), label("Problem"), start);

        let mut due = start + RETRY_FIRST;
        let mut gaps = vec![(due - start).as_secs()];
        for _ in 0..8 {
            let before = board.calls().len();
            clock.advance(due - clock.now() - Duration::from_millis(1));
            assert_eq!(outbox.deliver_due(&board, "BID", &clock, &diag), Some(due));
            assert_eq!(board.calls().len(), before, "tried before it was due");
            clock.advance(Duration::from_millis(1));
            let next = outbox
                .deliver_due(&board, "BID", &clock, &diag)
                .expect("still owed");
            assert_eq!(board.calls().len(), before + 1, "one try when due");
            gaps.push((next - due).as_secs());
            due = next;
        }
        assert_eq!(
            gaps[..],
            [5, 10, 20, 40, 80, 160, 300, 300, 300][..],
            "the first gap is the inline try's"
        );
        assert_eq!(
            diag.errs()[..3],
            [
                format!(
                    "trello is unreachable for card \"{TITLE}\" (sl-card1): trello add label: no \
                     response (mock failure); adding the label \"Problem\" is tried again in 10s \
                     (try 3)"
                ),
                format!(
                    "trello is unreachable for card \"{TITLE}\" (sl-card1): trello add label: no \
                     response (mock failure); adding the label \"Problem\" is tried again in 20s \
                     (try 4)"
                ),
                format!(
                    "trello is unreachable for card \"{TITLE}\" (sl-card1): trello add label: no \
                     response (mock failure); adding the label \"Problem\" is tried again in 40s \
                     (try 5)"
                ),
            ]
        );
    }

    #[test]
    fn a_write_is_given_up_after_an_hour_naming_the_card_and_what_was_lost() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        board.go_down();
        outbox.queue_retry(&card("card1"), move_to("Backlog"), clock.now());
        while let Some(next) = outbox.deliver_due(&board, "BID", &clock, &diag) {
            clock.advance(next - clock.now());
        }
        assert!(!outbox.owes("card1"));
        let gave_up: Vec<String> = diag
            .errs()
            .into_iter()
            .filter(|l| !l.starts_with("trello is unreachable"))
            .collect();
        assert_eq!(
            gave_up,
            [format!(
                "gave up on card \"{TITLE}\" (sl-card1) after 1h: moving it to \"Backlog\" is \
                 lost — trello resolve list: no response (mock failure)"
            )]
        );
        assert!(diag.narrated().is_empty(), "nothing landed");
    }

    /// A board that answers and refuses is not asked again: the entry is dropped after
    /// its one try from the queue, said loudly, and the card's next write is tried.
    #[test]
    fn a_refusal_is_not_retried() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        board.go_down();
        outbox.queue_retry(&card("card1"), label("Problem"), clock.now());
        outbox.queue_behind(&card("card1"), move_to("Backlog"), clock.now());
        board.clear_failure();
        board.fail_status("add label", 400);
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&board, "BID", &clock, &diag), None);
        assert_eq!(
            diag.errs(),
            [format!(
                "gave up on card \"{TITLE}\" (sl-card1) after 5s: adding the label \"Problem\" \
                 is lost — trello add label: board returned status 400"
            )]
        );
        assert_eq!(
            board.actions(),
            [Action::Move {
                card: "card1".into(),
                list: "Backlog".into(),
                position: ListPosition::Bottom,
            }]
        );
    }

    /// A reply that cannot be read may follow a write that landed, or a read before it
    /// that never let it leave: the entry is dropped, said as neither landed nor lost, and
    /// the card's next write is tried in the same pass.
    #[test]
    fn a_decode_on_a_queued_write_is_popped_as_unknown_not_lost() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        outbox.queue_retry(&card("card1"), move_to("Backlog"), clock.now());
        outbox.queue_behind(&card("card1"), label("Problem"), clock.now());
        board.fail_decode("resolve list");
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&board, "BID", &clock, &diag), None);
        let errs = diag.errs();
        assert_eq!(
            errs,
            [format!(
                "trello answered card \"{TITLE}\" (sl-card1) with a reply the plugin could not \
                 read; moving it to \"Backlog\" may or may not have landed — trello resolve \
                 list: undecodable response (mock failure)"
            )]
        );
        assert!(!errs[0].contains("is lost"));
        assert_eq!(
            board.actions(),
            [Action::AddLabel {
                card: "card1".into(),
                label: "Problem".into(),
            }]
        );
    }

    #[test]
    fn one_pass_drains_a_cards_backlog_once_its_head_lands() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        board.go_down();
        outbox.queue_retry(
            &card("card1"),
            Write::Note("[afkd-attempt] 1/2: boom".into()),
            clock.now(),
        );
        outbox.queue_behind(&card("card1"), move_to("Backlog"), clock.now());
        outbox.queue_behind(&card("card1"), label("Problem"), clock.now());
        clock.advance(RETRY_FIRST);
        let head_due = outbox
            .deliver_due(&board, "BID", &clock, &diag)
            .expect("still owed");
        clock.advance(head_due - clock.now());
        let head_due = outbox
            .deliver_due(&board, "BID", &clock, &diag)
            .expect("still owed");
        assert_eq!(head_due - clock.now(), Duration::from_secs(20));

        board.clear_failure();
        clock.advance(head_due - clock.now());
        assert_eq!(outbox.deliver_due(&board, "BID", &clock, &diag), None);
        assert_eq!(
            board.comments_on("card1").last().map(|c| c.text.clone()),
            Some("[afkd-attempt] 1/2: boom".to_string())
        );
        assert_eq!(
            board.actions(),
            [
                Action::Move {
                    card: "card1".into(),
                    list: "Backlog".into(),
                    position: ListPosition::Bottom,
                },
                Action::AddLabel {
                    card: "card1".into(),
                    label: "Problem".into(),
                },
            ]
        );
        assert_eq!(
            diag.narrated(),
            [
                format!(
                    "[trello] posting the comment \"[afkd-attempt] 1/2: boom\" on card \"{TITLE}\""
                ),
                format!("[trello] moving card \"{TITLE}\" to list \"Backlog\" (at bottom)"),
                format!("[trello] adding label \"Problem\" to card \"{TITLE}\""),
            ]
        );
    }

    /// A watermark that lands from the queue prunes the priors it supersedes; a prune that
    /// fails is said and dropped, never queued again.
    #[test]
    fn a_landed_watermark_prunes_its_priors_and_a_failed_prune_is_dropped() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        board.seed_comment("card1", "ran1", "[afkd-ran] owner=a upto=1", 1);
        board.seed_comment("card1", "ran2", "[afkd-ran] owner=a upto=2", 2);
        let watermark = Write::Watermark {
            text: "[afkd-ran] owner=a upto=3".into(),
            priors: vec!["ran1".into(), "ran2".into()],
        };
        outbox.queue_retry(&card("card1"), watermark, clock.now());
        board.fail("delete comment");
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&board, "BID", &clock, &diag), None);
        assert!(!outbox.owes("card1"), "nothing queued again");
        assert_eq!(
            diag.errs(),
            [
                "trello delete comment: no response (mock failure)",
                "trello delete comment: no response (mock failure)",
            ]
        );
        assert_eq!(
            diag.narrated(),
            [format!(
                "[trello] posting the [afkd-ran] watermark on card \"{TITLE}\""
            )]
        );

        // And with the board well, both go.
        board.clear_failure();
        let watermark = Write::Watermark {
            text: "[afkd-ran] owner=a upto=4".into(),
            priors: vec!["ran1".into(), "ran2".into()],
        };
        outbox.queue_retry(&card("card1"), watermark, clock.now());
        clock.advance(RETRY_FIRST);
        outbox.deliver_due(&board, "BID", &clock, &diag);
        let texts: Vec<String> = board
            .comments_on("card1")
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(
            texts,
            ["[afkd-ran] owner=a upto=3", "[afkd-ran] owner=a upto=4"]
        );
    }

    #[test]
    fn one_cards_writes_land_in_order_and_others_are_not_held_behind_it() {
        let (outbox, board, clock, diag) = (
            Outbox::default(),
            board(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        board.fail("add label");
        outbox.queue_retry(&card("card1"), label("Problem"), clock.now());
        outbox.queue_behind(&card("card1"), move_to("Backlog"), clock.now());
        outbox.queue_behind(&card("card1"), label("Redo"), clock.now());
        outbox.queue_retry(&card("card2"), move_to("Backlog"), clock.now());
        clock.advance(RETRY_FIRST);
        outbox.deliver_due(&board, "BID", &clock, &diag);
        assert_eq!(
            board.actions(),
            [Action::Move {
                card: "card2".into(),
                list: "Backlog".into(),
                position: ListPosition::Bottom,
            }],
            "card2 lands; card1's move waits behind its label"
        );
        assert!(outbox.owes("card1") && !outbox.owes("card2"));

        board.clear_failure();
        clock.advance(Duration::from_secs(10));
        assert_eq!(outbox.deliver_due(&board, "BID", &clock, &diag), None);
        assert_eq!(
            board.actions()[1..],
            [
                Action::AddLabel {
                    card: "card1".into(),
                    label: "Problem".into(),
                },
                Action::Move {
                    card: "card1".into(),
                    list: "Backlog".into(),
                    position: ListPosition::Bottom,
                },
                Action::AddLabel {
                    card: "card1".into(),
                    label: "Redo".into(),
                },
            ]
        );
    }

    /// A write being tried is still owed: a write to its card queues behind it rather
    /// than overtaking it.
    #[test]
    fn an_entry_in_flight_still_owes_its_card() {
        let (outbox, clock) = (Outbox::default(), FakeClock::new());
        outbox.queue_retry(&card("card1"), label("Problem"), clock.now());
        clock.advance(RETRY_FIRST);
        let (_, taken, _) = outbox.take_due(clock.now()).expect("due");
        assert_eq!(taken.id, "card1");
        assert!(outbox.owes("card1"));
        assert!(
            outbox.take_due(clock.now()).is_none(),
            "tried once at a time"
        );
        assert_eq!(outbox.lock().next_due(), None, "and not waited on");
    }

    #[test]
    fn abandon_names_every_write_still_owed() {
        let (outbox, clock, diag) = (Outbox::default(), FakeClock::new(), CaptureDiag::default());
        let reason = format!("{}\nsecond line", "修".repeat(100));
        outbox.queue_retry(&card("card1"), Write::Note(reason), clock.now());
        outbox.queue_behind(
            &card("card1"),
            Write::Watermark {
                text: "[afkd-ran] owner=a upto=3".into(),
                priors: Vec::new(),
            },
            clock.now(),
        );
        outbox.queue_retry(
            &card("card2"),
            Write::Act(LifecycleAction::Archive),
            clock.now(),
        );
        outbox.abandon(&diag);
        assert_eq!(
            diag.errs(),
            [
                format!(
                    "afkd ended the plugin with the comment \"{}…\" for card \"{TITLE}\" \
                     (sl-card1) still owed",
                    "修".repeat(80)
                ),
                format!(
                    "afkd ended the plugin with the [afkd-ran] watermark for card \"{TITLE}\" \
                     (sl-card1) still owed"
                ),
                format!(
                    "afkd ended the plugin with archiving it for card \"{TITLE}\" (sl-card2) \
                     still owed"
                ),
            ]
        );
    }

    #[test]
    fn every_write_has_its_phrase() {
        use crate::settings::MemberRef;
        for (write, what) in [
            (
                move_to("Blockerat / Väntar"),
                "moving it to \"Blockerat / Väntar\"",
            ),
            (label("reviewed ✅"), "adding the label \"reviewed ✅\""),
            (
                Write::Act(LifecycleAction::RemoveLabel {
                    name: "Redo".into(),
                }),
                "removing the label \"Redo\"",
            ),
            (
                Write::Act(LifecycleAction::AddMember(MemberRef::SelfMember)),
                "adding the member \"me\"",
            ),
            (
                Write::Act(LifecycleAction::RemoveMember(MemberRef::Username(
                    "björn-öst".into(),
                ))),
                "removing the member \"björn-öst\"",
            ),
            (
                Write::Act(LifecycleAction::MarkComplete),
                "marking it complete",
            ),
            (Write::Act(LifecycleAction::Archive), "archiving it"),
            (
                Write::Act(LifecycleAction::Comment("## 完了 ✅\n\n- took 3m".into())),
                "the comment \"## 完了 ✅…\"",
            ),
            (Write::Note(String::new()), "the comment \"\""),
            (
                Write::Watermark {
                    text: "[afkd-ran]".into(),
                    priors: Vec::new(),
                },
                "the [afkd-ran] watermark",
            ),
        ] {
            assert_eq!(write.what(), what);
        }
    }

    #[test]
    fn a_span_reads_at_a_glance() {
        for (secs, span_) in [
            (0, "0s"),
            (20, "20s"),
            (300, "5m"),
            (250, "4m10s"),
            (3600, "1h"),
            (3605, "1h"),
            (3900, "1h5m"),
        ] {
            assert_eq!(span(Duration::from_secs(secs)), span_);
        }
    }
}
