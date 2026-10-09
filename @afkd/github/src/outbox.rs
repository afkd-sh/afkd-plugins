//! The plugin's own retries: the writes GitHub could not take, queued and sent again in the
//! background until they land or an hour has gone by.
//!
//! afkd retries no plugin action, and it waits only 60 seconds for a reply, so a write
//! GitHub could not take — the network down, a 429, a 5xx — cannot be retried inside the
//! call that asked for it. It is queued here instead, and the call answers as if it had
//! landed, so the slot's next action still runs (and is queued behind it). One worker
//! thread ([`Outbox::run`]) does the retrying: each item's writes strictly in the order they
//! arrived, the first retry [`RETRY_FIRST`] after the failure, the gap doubling up to
//! [`RETRY_CAP`], for up to [`RETRY_FOR`]. Then it gives up, with one loud line naming the
//! item and the write it lost.
//!
//! A refusal no retry can fix — another 4xx, a token without the scope — is never queued:
//! the one inline try's error is the caller's, as it always was. The queue lives in memory,
//! so a plugin afkd ends names what it still owes ([`Outbox::abandon`]).

use std::collections::{HashSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::client::{GithubClient, GithubError, Repo};
use crate::common::{do_action, lock, unit_key, Clock, Diag};
use crate::lifecycle::LifecycleAction;

/// How long after a failed write its first retry comes.
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(5);

/// The longest gap between two tries of one write; the gap doubles up to it.
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(5 * 60);

/// How long a write is retried, from its first failure, before it is given up.
pub(crate) const RETRY_FOR: Duration = Duration::from_secs(60 * 60);

/// How much of a comment a log line quotes.
const QUOTE_MAX: usize = 80;

/// Whether a later try of what failed with `e` can land: GitHub was not reached, or it
/// answered "too many requests" or a server error. Anything else is GitHub's answer about
/// the write itself, and asking again changes nothing — a `Decode` included, since GitHub
/// did answer, and its answer cannot be read.
pub(crate) fn transient(e: &GithubError) -> bool {
    match e {
        GithubError::Transport { .. } => true,
        GithubError::Status { status, .. } => *status == 429 || (500..=599).contains(status),
        GithubError::Decode { .. } => false,
    }
}

/// The issue or pull request a write is for, as the queue keeps it: enough to do the
/// write again, and to name the item in a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// `issue` or `pull request`.
    pub(crate) noun: &'static str,
    pub(crate) repo: Repo,
    pub(crate) number: u64,
    pub(crate) title: String,
    /// The login the item was claimed as, which the assignee verbs act as.
    pub(crate) me: String,
}

impl Item {
    /// The item's stable coordinate, `<owner>/<name>#<number>`: what its writes are kept in
    /// order by. An issue and a pull request share GitHub's numbers, but each kind has an
    /// outbox of its own, so within one the coordinate names one item.
    pub(crate) fn thread(&self) -> String {
        unit_key(&self.repo, self.number)
    }
}

/// One write to an item the plugin owes GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Write {
    /// An action a slot called.
    Act(LifecycleAction),
    /// The run-end delete of the claim marker the unit was won on.
    Release { marker: u64 },
}

impl Write {
    /// The short phrase every log line names the write by.
    pub(crate) fn what(&self) -> String {
        match self {
            Write::Act(action) => match action {
                LifecycleAction::AssignMe => "assigning it to the bot".to_string(),
                LifecycleAction::Unassign => "unassigning the bot".to_string(),
                LifecycleAction::LabelAdd(name) => format!("adding the label \"{name}\""),
                LifecycleAction::LabelRemove(name) => format!("removing the label \"{name}\""),
                LifecycleAction::Close => "closing it".to_string(),
                LifecycleAction::Comment(text) => format!("the comment {}", quoted(text)),
            },
            Write::Release { .. } => "deleting the claim marker".to_string(),
        }
    }
}

/// The first line of `text`, cut to [`QUOTE_MAX`] characters, in quotes: `…` when anything
/// but trailing whitespace was left out.
fn quoted(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut quote: String = line.chars().take(QUOTE_MAX).collect();
    // A prefix of `text`, so shorter than what is left once the trailing whitespace is off
    // exactly when something was left out.
    if quote.len() < text.trim_end().len() {
        quote.push('…');
    }
    format!("\"{quote}\"")
}

/// How a log line names an item: its title, which a person reads, and its coordinate,
/// which finds it.
pub(crate) fn named(item: &Item) -> String {
    format!("{} \"{}\" ({})", item.noun, item.title, item.thread())
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
/// landed — GitHub answered, and the reply could not be read — rather than that it is lost.
pub(crate) fn lost_line(
    item: &Item,
    write: &Write,
    e: &GithubError,
    after: Option<Duration>,
) -> String {
    let (item, what) = (named(item), write.what());
    match (e, after) {
        (GithubError::Decode { .. }, _) => format!(
            "github answered {item} with a reply the plugin could not read; {what} may or may \
             not have landed — {e}"
        ),
        (_, Some(after)) => format!(
            "gave up on {item} after {}: {what} is lost — {e}",
            span(after)
        ),
        (_, None) => format!("{item}: {what} is lost — {e}"),
    }
}

/// The line for a write queued because its inline try could not reach GitHub.
pub(crate) fn queued_line(item: &Item, write: &Write, e: &GithubError) -> String {
    format!(
        "github is unreachable for {}: {e}; {} is queued and retried for up to {}",
        named(item),
        write.what(),
        span(RETRY_FOR)
    )
}

/// The line for a write queued behind the writes its item is already owed.
pub(crate) fn queued_behind_line(item: &Item, write: &Write) -> String {
    format!(
        "github still owes {} earlier writes; {} is queued behind them",
        named(item),
        write.what()
    )
}

/// Do `write` on `item`, once. A claim marker already gone is released: nothing is left
/// for the delete to do.
pub(crate) fn perform(
    client: &dyn GithubClient,
    item: &Item,
    write: &Write,
) -> Result<(), GithubError> {
    let (repo, number) = (&item.repo, item.number);
    match write {
        Write::Act(action) => do_action(client, repo, number, action, &item.me),
        Write::Release { marker } => match client.delete_comment(repo, *marker) {
            Err(GithubError::Status { status: 404, .. }) => Ok(()),
            done => done,
        },
    }
}

/// Where a write [`deliver`](Outbox::deliver)ed went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// GitHub took it.
    Landed,
    /// GitHub could not be reached, or the item still owes earlier writes: the [`Outbox`]
    /// has it, and said so.
    Queued,
}

/// One write the queue holds.
struct Owed {
    /// Which entry this is, to find it again after its try.
    seq: u64,
    item: Item,
    write: Write,
    /// When it was queued: the start of its [`RETRY_FOR`].
    since: Instant,
    /// When it may be tried next.
    next_try: Instant,
    /// The gap its last failure set, zero before its first try from the queue.
    delay: Duration,
    /// How many tries have failed, the inline one included.
    tries: u32,
    /// Whether the worker is trying it right now. It stays queued meanwhile, so the item
    /// is still owed and a write behind it still waits.
    in_flight: bool,
}

#[derive(Default)]
struct Queue {
    owed: VecDeque<Owed>,
    next_seq: u64,
}

impl Queue {
    /// Each item's oldest entry: the only one of its writes that may be tried.
    fn heads_mut(&mut self) -> impl Iterator<Item = &mut Owed> {
        let mut seen = HashSet::new();
        self.owed
            .iter_mut()
            .filter(move |owed| seen.insert(owed.item.thread()))
    }

    /// When the earliest head not being tried is due.
    fn next_due(&mut self) -> Option<Instant> {
        self.heads_mut()
            .filter(|owed| !owed.in_flight)
            .map(|owed| owed.next_try)
            .min()
    }
}

/// The writes the plugin owes GitHub, in the order they arrived, and the worker that
/// delivers them. The request thread is the only producer and the worker the only
/// consumer; neither holds the lock across a forge request.
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

    /// Whether any write to the item `thread` names is still owed, the one being tried
    /// included: a new write to it must then queue behind, to keep the item's writes in
    /// order.
    pub(crate) fn owes(&self, thread: &str) -> bool {
        self.lock()
            .owed
            .iter()
            .any(|owed| owed.item.thread() == thread)
    }

    /// Do `write` on `item`, or queue it: behind the item's earlier writes while any is
    /// still owed, so one item's writes land in the order they were asked for; and for a
    /// retry when GitHub cannot be reached. Every write queued is said, one line. A refusal
    /// no retry can fix is the caller's error, and nothing is queued.
    pub(crate) fn deliver(
        &self,
        client: &dyn GithubClient,
        item: &Item,
        write: Write,
        now: Instant,
        diag: &dyn Diag,
    ) -> Result<Delivery, GithubError> {
        if self.owes(&item.thread()) {
            diag.err(&queued_behind_line(item, &write));
            self.queue_behind(item, write, now);
            return Ok(Delivery::Queued);
        }
        match perform(client, item, &write) {
            Ok(()) => Ok(Delivery::Landed),
            Err(e) if transient(&e) => {
                diag.err(&queued_line(item, &write, &e));
                self.queue_retry(item, write, now);
                Ok(Delivery::Queued)
            }
            Err(e) => Err(e),
        }
    }

    /// Queue `write`, whose inline try failed at `now`, for its first retry.
    pub(crate) fn queue_retry(&self, item: &Item, write: Write, now: Instant) {
        self.push(item, write, now, now + RETRY_FIRST, RETRY_FIRST, 1);
    }

    /// Queue `write` behind the writes `item` is already owed, to be tried as soon as they
    /// have settled.
    pub(crate) fn queue_behind(&self, item: &Item, write: Write, now: Instant) {
        self.push(item, write, now, now, Duration::ZERO, 0);
    }

    fn push(
        &self,
        item: &Item,
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
            item: item.clone(),
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
    fn take_due(&self, now: Instant) -> Option<(u64, Item, Write)> {
        let mut queue = self.lock();
        let owed = queue
            .heads_mut()
            .find(|owed| !owed.in_flight && owed.next_try <= now)?;
        owed.in_flight = true;
        Some((owed.seq, owed.item.clone(), owed.write.clone()))
    }

    /// One pass: try every item's oldest write that is due, and settle each try — landed,
    /// rescheduled, or given up. An item whose write settles has its next one tried in the
    /// same pass, so once GitHub is back an item's whole backlog lands at once, in order.
    /// When the earliest write left is due next, if any.
    pub(crate) fn deliver_due(
        &self,
        client: &dyn GithubClient,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Option<Instant> {
        while let Some((seq, item, write)) = self.take_due(clock.now()) {
            let tried = perform(client, &item, &write);
            let now = clock.now();
            let mut queue = self.lock();
            let Some(at) = queue.owed.iter().position(|owed| owed.seq == seq) else {
                continue;
            };
            match tried {
                Ok(()) => {
                    let since = queue.owed.remove(at).map_or(now, |owed| owed.since);
                    drop(queue);
                    diag.err(&format_args!(
                        "github is back for {}: {} landed after {}",
                        named(&item),
                        write.what(),
                        span(now - since)
                    ));
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
                        "github is unreachable for {}: {e}; {} is tried again in {} (try {})",
                        named(&item),
                        write.what(),
                        span(delay),
                        tries + 1
                    ));
                }
                Err(e) => {
                    let since = queue.owed.remove(at).map_or(now, |owed| owed.since);
                    drop(queue);
                    diag.err(&lost_line(&item, &write, &e, Some(now - since)));
                }
            }
        }
        self.lock().next_due()
    }

    /// The worker: deliver what is due, then sleep until the next write is due or a new
    /// one is queued. Runs for the life of the process, on its own thread and its own
    /// client, which no call's deadline ever reaches.
    pub(crate) fn run(&self, client: &dyn GithubClient, clock: &dyn Clock, diag: &dyn Diag) {
        loop {
            self.deliver_due(client, clock, diag);
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
                named(&owed.item)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! The queue and its policy over a mock forge, on a fake clock, one pass at a time:
    //! the worker thread itself is `tests/wire.rs`'s.

    use super::*;
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock, CLAIMED_LABEL};

    /// The issue every test writes to: the delimiter a log line wraps it in, wide CJK, an
    /// emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    /// The login the items were claimed as.
    const BOT: &str = "björn-öst[bot]";

    /// A human assigned beside the bot.
    const HUMAN: &str = "maría.garcía";

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    fn item(number: u64) -> Item {
        Item {
            noun: "issue",
            repo: repo(),
            number,
            title: TITLE.into(),
            me: BOT.into(),
        }
    }

    /// A forge with issues 7 and 8 open, claimed, and the bot assigned to both.
    fn forge() -> MockClient {
        let forge = MockClient::new(BOT);
        forge.add_issue_assigned(7, TITLE, "", &[CLAIMED_LABEL], &[BOT]);
        forge.add_issue_assigned(8, "第二の課題 🚧", "", &[CLAIMED_LABEL], &[BOT]);
        forge
    }

    fn label(name: &str) -> Write {
        Write::Act(LifecycleAction::LabelAdd(name.into()))
    }

    fn comment(text: &str) -> Write {
        Write::Act(LifecycleAction::Comment(text.into()))
    }

    fn labelled(index: u64, name: &str) -> Action {
        Action::Label {
            index,
            name: name.into(),
        }
    }

    #[test]
    fn transient_is_transport_429_and_5xx_only() {
        let status = |status| GithubError::Status {
            stage: "add label",
            status,
        };
        for (code, retried) in [
            (400, false),
            (401, false),
            (403, false),
            (404, false),
            (422, false),
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
        let stage = "set state";
        assert!(transient(&GithubError::Transport {
            stage,
            reason: "connection refused".into()
        }));
        let decode = GithubError::Decode {
            stage,
            reason: "expected an object".into(),
        };
        assert!(!transient(&decode), "{decode}");
    }

    /// Nothing is tried before it is due, and the gap after each failed try doubles from
    /// [`RETRY_FIRST`] to [`RETRY_CAP`], where it stays; each failure says when the next try
    /// comes.
    #[test]
    fn the_backoff_doubles_from_5s_and_caps_at_5min() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.go_down();
        let start = clock.now();
        outbox.queue_retry(&item(7), label("needs/human 🚧"), start);

        let mut due = start + RETRY_FIRST;
        let mut gaps = vec![(due - start).as_secs()];
        for _ in 0..8 {
            let before = diag.lines().len();
            clock.advance(due - clock.now() - Duration::from_millis(1));
            assert_eq!(outbox.deliver_due(&forge, &clock, &diag), Some(due));
            assert_eq!(diag.lines().len(), before, "tried before it was due");
            clock.advance(Duration::from_millis(1));
            let next = outbox
                .deliver_due(&forge, &clock, &diag)
                .expect("still owed");
            assert_eq!(diag.lines().len(), before + 1, "one try when due");
            gaps.push((next - due).as_secs());
            due = next;
        }
        assert_eq!(
            gaps[..],
            [5, 10, 20, 40, 80, 160, 300, 300, 300][..],
            "the first gap is the inline try's"
        );
        assert_eq!(
            diag.lines()[..3],
            [
                format!(
                    "github is unreachable for issue \"{TITLE}\" (acme/widgets#7): github add \
                     label: no response (mock failure); adding the label \"needs/human 🚧\" is \
                     tried again in 10s (try 3)"
                ),
                format!(
                    "github is unreachable for issue \"{TITLE}\" (acme/widgets#7): github add \
                     label: no response (mock failure); adding the label \"needs/human 🚧\" is \
                     tried again in 20s (try 4)"
                ),
                format!(
                    "github is unreachable for issue \"{TITLE}\" (acme/widgets#7): github add \
                     label: no response (mock failure); adding the label \"needs/human 🚧\" is \
                     tried again in 40s (try 5)"
                ),
            ]
        );
        assert!(forge.actions().is_empty());
    }

    #[test]
    fn a_write_is_given_up_after_an_hour_naming_the_item_and_what_was_lost() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.go_down();
        outbox.queue_retry(&item(7), Write::Act(LifecycleAction::Unassign), clock.now());
        while let Some(next) = outbox.deliver_due(&forge, &clock, &diag) {
            clock.advance(next - clock.now());
        }
        assert!(!outbox.owes("acme/widgets#7"));
        let gave_up: Vec<String> = diag
            .lines()
            .into_iter()
            .filter(|l| !l.starts_with("github is unreachable"))
            .collect();
        assert_eq!(
            gave_up,
            [format!(
                "gave up on issue \"{TITLE}\" (acme/widgets#7) after 1h: unassigning the bot is \
                 lost — github remove assignees: no response (mock failure)"
            )]
        );
        assert!(forge.actions().is_empty(), "nothing landed");
    }

    /// A forge that answers and refuses is not asked again: the entry is dropped after
    /// its one try from the queue, said loudly, and the item's next write is tried.
    #[test]
    fn a_refusal_is_not_retried() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.go_down();
        outbox.queue_retry(&item(7), label("needs/human 🚧"), clock.now());
        outbox.queue_behind(&item(7), Write::Act(LifecycleAction::Close), clock.now());
        forge.clear_failure();
        forge.refuse("add label", 403);
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&forge, &clock, &diag), None);
        assert_eq!(
            diag.lines(),
            [
                format!(
                    "gave up on issue \"{TITLE}\" (acme/widgets#7) after 5s: adding the label \
                     \"needs/human 🚧\" is lost — github add label: forge returned status 403"
                ),
                format!(
                    "github is back for issue \"{TITLE}\" (acme/widgets#7): closing it landed \
                     after 5s"
                ),
            ]
        );
        assert_eq!(
            forge.actions(),
            [Action::State {
                index: 7,
                state: "closed".into(),
            }]
        );
    }

    /// A reply that cannot be read may follow a write that landed: the entry is dropped,
    /// said as neither landed nor lost, and the item's next write is tried in the same
    /// pass.
    #[test]
    fn a_decode_on_a_queued_write_is_popped_as_unknown_not_lost() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        outbox.queue_retry(&item(7), Write::Act(LifecycleAction::Unassign), clock.now());
        outbox.queue_behind(&item(7), label("needs/human 🚧"), clock.now());
        forge.fail_decode("remove assignees");
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&forge, &clock, &diag), None);
        let lines = diag.lines();
        assert_eq!(
            lines[0],
            format!(
                "github answered issue \"{TITLE}\" (acme/widgets#7) with a reply the plugin \
                 could not read; unassigning the bot may or may not have landed — github \
                 remove assignees: undecodable response (mock failure)"
            )
        );
        assert!(!lines[0].contains("is lost"));
        assert_eq!(forge.actions(), [labelled(7, "needs/human 🚧")]);
    }

    #[test]
    fn one_pass_drains_an_items_backlog_once_its_head_lands() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.go_down();
        let note = "run did not complete: the agent ended with\n```\nexit 128\n```";
        outbox.queue_retry(&item(7), comment(note), clock.now());
        outbox.queue_behind(&item(7), label("needs/human 🚧"), clock.now());
        outbox.queue_behind(&item(7), Write::Act(LifecycleAction::Unassign), clock.now());
        clock.advance(RETRY_FIRST);
        let head_due = outbox
            .deliver_due(&forge, &clock, &diag)
            .expect("still owed");
        clock.advance(head_due - clock.now());
        let head_due = outbox
            .deliver_due(&forge, &clock, &diag)
            .expect("still owed");
        assert_eq!(head_due - clock.now(), Duration::from_secs(20));

        forge.clear_failure();
        clock.advance(head_due - clock.now());
        assert_eq!(outbox.deliver_due(&forge, &clock, &diag), None);
        assert_eq!(
            forge.actions(),
            [
                Action::Comment {
                    index: 7,
                    body: note.into(),
                },
                labelled(7, "needs/human 🚧"),
                Action::RemoveAssignees {
                    index: 7,
                    assignees: vec![BOT.into()],
                },
            ]
        );
        assert_eq!(
            diag.lines()[2..],
            [
                format!(
                    "github is back for issue \"{TITLE}\" (acme/widgets#7): the comment \"run \
                     did not complete: the agent ended with…\" landed after 35s"
                ),
                format!(
                    "github is back for issue \"{TITLE}\" (acme/widgets#7): adding the label \
                     \"needs/human 🚧\" landed after 35s"
                ),
                format!(
                    "github is back for issue \"{TITLE}\" (acme/widgets#7): unassigning the \
                     bot landed after 35s"
                ),
            ]
        );
    }

    #[test]
    fn one_items_writes_land_in_order_and_others_are_not_held_behind_it() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.fail("add label");
        outbox.queue_retry(&item(7), label("needs/human 🚧"), clock.now());
        outbox.queue_behind(&item(7), comment("attempts spent ✅"), clock.now());
        outbox.queue_behind(&item(7), label("afkd/reviewed ✅"), clock.now());
        outbox.queue_retry(&item(8), Write::Act(LifecycleAction::Close), clock.now());
        clock.advance(RETRY_FIRST);
        outbox.deliver_due(&forge, &clock, &diag);
        assert_eq!(
            forge.actions(),
            [Action::State {
                index: 8,
                state: "closed".into(),
            }],
            "#8 lands; #7's comment waits behind its label"
        );
        assert!(outbox.owes("acme/widgets#7") && !outbox.owes("acme/widgets#8"));

        forge.clear_failure();
        clock.advance(Duration::from_secs(10));
        assert_eq!(outbox.deliver_due(&forge, &clock, &diag), None);
        assert_eq!(
            forge.actions()[1..],
            [
                labelled(7, "needs/human 🚧"),
                Action::Comment {
                    index: 7,
                    body: "attempts spent ✅".into(),
                },
                labelled(7, "afkd/reviewed ✅"),
            ]
        );
    }

    /// A write being tried is still owed: a write to its item queues behind it rather
    /// than overtaking it.
    #[test]
    fn an_entry_in_flight_still_owes_its_item() {
        let (outbox, clock) = (Outbox::default(), FakeClock::new());
        outbox.queue_retry(&item(7), label("needs/human 🚧"), clock.now());
        clock.advance(RETRY_FIRST);
        let (_, taken, _) = outbox.take_due(clock.now()).expect("due");
        assert_eq!(taken.number, 7);
        assert!(outbox.owes("acme/widgets#7"));
        assert!(
            outbox.take_due(clock.now()).is_none(),
            "tried once at a time"
        );
        assert_eq!(outbox.lock().next_due(), None, "and not waited on");
    }

    /// A claim marker already off the thread — deleted by hand, or by a release that
    /// landed before its reply was lost — is released: no retry, no lost line.
    #[test]
    fn a_marker_already_gone_counts_as_released() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.refuse("delete comment", 404);
        let released = outbox.deliver(
            &forge,
            &item(7),
            Write::Release { marker: 41 },
            clock.now(),
            &diag,
        );
        assert_eq!(released.unwrap(), Delivery::Landed);
        assert!(diag.lines().is_empty(), "{:?}", diag.lines());

        // And from the queue: what the outage left owed lands once the forge says 404.
        forge.go_down();
        outbox.queue_retry(&item(7), Write::Release { marker: 41 }, clock.now());
        forge.clear_failure();
        forge.refuse("delete comment", 404);
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&forge, &clock, &diag), None);
        assert_eq!(
            diag.lines(),
            [format!(
                "github is back for issue \"{TITLE}\" (acme/widgets#7): deleting the claim \
                 marker landed after 5s"
            )]
        );
    }

    /// GitHub's unassign removes named logins and leaves the rest, so one replayed from
    /// the queue after a human assigned themself meanwhile takes only the bot off.
    #[test]
    fn a_retried_unassign_removes_only_the_bot() {
        let (outbox, forge, clock, diag) = (
            Outbox::default(),
            forge(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        forge.go_down();
        let unassigned = outbox.deliver(
            &forge,
            &item(7),
            Write::Act(LifecycleAction::Unassign),
            clock.now(),
            &diag,
        );
        assert_eq!(unassigned.unwrap(), Delivery::Queued);
        assert_eq!(
            diag.lines(),
            [format!(
                "github is unreachable for issue \"{TITLE}\" (acme/widgets#7): github remove \
                 assignees: no response (mock failure); unassigning the bot is queued and \
                 retried for up to 1h"
            )]
        );

        forge.clear_failure();
        forge
            .add_assignees(&repo(), 7, &[HUMAN.into()])
            .expect("a human assigns themself beside the bot");
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&forge, &clock, &diag), None);
        assert_eq!(forge.assignees_of(7), [HUMAN]);
        assert_eq!(
            forge.actions()[1..],
            [Action::RemoveAssignees {
                index: 7,
                assignees: vec![BOT.into()],
            }]
        );
    }

    #[test]
    fn abandon_names_every_write_still_owed() {
        let (outbox, clock, diag) = (Outbox::default(), FakeClock::new(), CaptureDiag::default());
        let reason = format!("{}\nsecond line", "修".repeat(100));
        let pr = Item {
            noun: "pull request",
            title: "Draft: 修复 the 🚨 pipeline".into(),
            ..item(8)
        };
        outbox.queue_retry(&item(7), comment(&reason), clock.now());
        outbox.queue_behind(&item(7), Write::Release { marker: 41 }, clock.now());
        outbox.queue_retry(&pr, Write::Act(LifecycleAction::Close), clock.now());
        outbox.abandon(&diag);
        assert_eq!(
            diag.lines(),
            [
                format!(
                    "afkd ended the plugin with the comment \"{}…\" for issue \"{TITLE}\" \
                     (acme/widgets#7) still owed",
                    "修".repeat(80)
                ),
                format!(
                    "afkd ended the plugin with deleting the claim marker for issue \"{TITLE}\" \
                     (acme/widgets#7) still owed"
                ),
                "afkd ended the plugin with closing it for pull request \"Draft: 修复 the 🚨 \
                 pipeline\" (acme/widgets#8) still owed"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn every_write_has_its_phrase() {
        for (write, what) in [
            (
                Write::Act(LifecycleAction::AssignMe),
                "assigning it to the bot",
            ),
            (Write::Act(LifecycleAction::Unassign), "unassigning the bot"),
            (label("reviewed ✅"), "adding the label \"reviewed ✅\""),
            (
                Write::Act(LifecycleAction::LabelRemove("Blockerat / Väntar".into())),
                "removing the label \"Blockerat / Väntar\"",
            ),
            (Write::Act(LifecycleAction::Close), "closing it"),
            (
                comment("## 完了 ✅\n\n- took 3m"),
                "the comment \"## 完了 ✅…\"",
            ),
            (comment(""), "the comment \"\""),
            (
                comment("reviewed, nothing to add"),
                "the comment \"reviewed, nothing to add\"",
            ),
            (Write::Release { marker: 41 }, "deleting the claim marker"),
        ] {
            assert_eq!(write.what(), what);
        }
    }

    /// A quote is the first line, at most [`QUOTE_MAX`] characters of it, with `…` only
    /// when something it left out is more than trailing whitespace.
    #[test]
    fn a_quote_cuts_at_the_first_line_and_marks_only_what_it_left_out() {
        let long = format!("{}🚨", "修复 café ".repeat(10));
        assert_eq!(long.chars().count(), 81);
        for (text, quote) in [
            ("fix", "\"fix\"".to_string()),
            ("fix\n", "\"fix\"".to_string()),
            ("fix\r\n\n  \n", "\"fix\"".to_string()),
            ("fix\nmore", "\"fix…\"".to_string()),
            ("\nmore", "\"…\"".to_string()),
            (long.as_str(), format!("\"{}…\"", "修复 café ".repeat(10))),
            (
                &format!("{}\n", "修复 café ".repeat(10)),
                format!("\"{}\"", "修复 café ".repeat(10)),
            ),
        ] {
            assert_eq!(quoted(text), quote, "{text:?}");
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
