//! The plugin's own retries: the writes Linear could not take, queued and sent again in the
//! background until they land or an hour has gone by.
//!
//! afkd waits only 60 seconds for a reply, so a write Linear could not take — the network
//! down, a rate limit, a 5xx — cannot be retried inside the call that asked for it. It is
//! queued here instead, and the call answers as if it had landed. One worker thread
//! ([`Outbox::run`]) does the retrying: each issue's writes strictly in the order they
//! arrived, the first retry [`RETRY_FIRST`] after the failure, the gap doubling up to
//! [`RETRY_CAP`], for up to [`RETRY_FOR`] — and never sooner than the `Retry-After` a rate
//! limit named, which is what Linear asks for. Then it gives up, with one loud line naming
//! the issue and the write it lost.
//!
//! A refusal no retry can fix — a document Linear refused, an entity it does not have — is
//! never queued: the one inline try's error is the caller's. The queue lives in memory, so a
//! plugin afkd ends names what it still owes ([`Outbox::abandon`]).

use std::collections::{HashSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::common::{lock, Clock, Diag};
use crate::issue::ACTION_TAG;
use crate::tracker::{Issue, LinearError, Tracker};

/// How long after a failed write its first retry comes.
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(5);

/// The longest gap between two tries of one write; the gap doubles up to it, and a
/// `Retry-After` is honoured up to it.
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(5 * 60);

/// How long a write is retried, from its first failure, before it is given up.
pub(crate) const RETRY_FOR: Duration = Duration::from_secs(60 * 60);

/// How much of a comment a log line quotes.
const QUOTE_MAX: usize = 80;

/// One write to an issue the plugin owes Linear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Write {
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
            Write::Note(text) => format!("the comment {}", quoted(text)),
            Write::Watermark { .. } => "the [afkd-ran] watermark".to_string(),
        }
    }

    /// Do the write on `issue`, once. The watermark's priors are not touched: that is
    /// [`prune`]'s, once the watermark has landed.
    pub(crate) fn perform(&self, tracker: &dyn Tracker, issue: &Issue) -> Result<(), LinearError> {
        match self {
            Write::Note(text) | Write::Watermark { text, .. } => {
                tracker.post_comment(&issue.id, text).map(drop)
            }
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

/// How a log line names an issue: its identifier, which finds it, and its title, which a
/// person reads.
pub(crate) fn named(issue: &Issue) -> String {
    format!("issue {} \"{}\"", issue.identifier, issue.title)
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
/// landed — Linear answered, and the reply could not be read — rather than that it is lost.
pub(crate) fn lost_line(
    issue: &Issue,
    write: &Write,
    e: &LinearError,
    after: Option<Duration>,
) -> String {
    let (issue, what) = (named(issue), write.what());
    match (e, after) {
        (LinearError::Decode { .. }, _) => format!(
            "linear answered {issue} with a reply the plugin could not read; {what} may or may \
             not have landed — {e}"
        ),
        (_, Some(after)) => format!(
            "gave up on {issue} after {}: {what} is lost — {e}",
            span(after)
        ),
        (_, None) => format!("{issue}: {what} is lost — {e}"),
    }
}

/// The line for a write queued because its inline try could not land.
pub(crate) fn queued_line(issue: &Issue, write: &Write, e: &LinearError) -> String {
    format!(
        "linear could not take a write for {}: {e}; {} is queued and retried for up to {}",
        named(issue),
        write.what(),
        span(RETRY_FOR)
    )
}

/// The line for a write queued behind the writes its issue is already owed.
pub(crate) fn queued_behind_line(issue: &Issue, write: &Write) -> String {
    format!(
        "linear still owes {} earlier writes; {} is queued behind them",
        named(issue),
        write.what()
    )
}

/// Delete the prior watermarks a landed one supersedes. Best-effort, each failure said and
/// dropped: the newest watermark is the one a run reads, and it has landed.
pub(crate) fn prune(tracker: &dyn Tracker, priors: &[String], diag: &dyn Diag) {
    for prior in priors {
        if let Err(e) = tracker.delete_comment(prior) {
            diag.err(&e);
        }
    }
}

/// What a write that landed from the queue leaves behind: its success line, and for a
/// watermark, the pruned priors.
fn landed(tracker: &dyn Tracker, issue: &Issue, write: &Write, diag: &dyn Diag) {
    diag.narrate(&format!(
        "{ACTION_TAG} posting {} on {}",
        write.what(),
        named(issue)
    ));
    if let Write::Watermark { priors, .. } = write {
        prune(tracker, priors, diag);
    }
}

/// The gap before the next try of a write whose last gap was `delay`, after a failure that
/// asked for `retry_after`: doubled from [`RETRY_FIRST`] up to [`RETRY_CAP`], and never
/// shorter than the wait asked for, itself capped at [`RETRY_CAP`].
fn next_gap(delay: Duration, retry_after: Option<Duration>) -> Duration {
    let doubled = if delay.is_zero() {
        RETRY_FIRST
    } else {
        (delay * 2).min(RETRY_CAP)
    };
    doubled.max(retry_after.unwrap_or_default().min(RETRY_CAP))
}

/// One write the queue holds.
struct Owed {
    /// Which entry this is, to find it again after its try.
    seq: u64,
    issue: Issue,
    write: Write,
    /// When it was queued: the start of its [`RETRY_FOR`].
    since: Instant,
    /// When it may be tried next.
    next_try: Instant,
    /// The gap its last failure set, zero before its first try from the queue.
    delay: Duration,
    /// How many tries have failed, the inline one included.
    tries: u32,
    /// Whether the worker is trying it right now. It stays queued meanwhile, so the issue is
    /// still owed and a write behind it still waits.
    in_flight: bool,
}

#[derive(Default)]
struct Queue {
    owed: VecDeque<Owed>,
    next_seq: u64,
}

impl Queue {
    /// Each issue's oldest entry: the only one of its writes that may be tried.
    fn heads_mut(&mut self) -> impl Iterator<Item = &mut Owed> {
        let mut seen = HashSet::new();
        self.owed
            .iter_mut()
            .filter(move |owed| seen.insert(owed.issue.id.clone()))
    }

    /// When the earliest head not being tried is due.
    fn next_due(&mut self) -> Option<Instant> {
        self.heads_mut()
            .filter(|owed| !owed.in_flight)
            .map(|owed| owed.next_try)
            .min()
    }
}

/// The writes the plugin owes Linear, in the order they arrived, and the worker that
/// delivers them. The request thread is the only producer and the worker the only consumer;
/// neither holds the lock across a request.
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

    /// Whether any write to `issue_id` is still owed, the one being tried included: a new
    /// write to it must then queue behind, to keep the issue's writes in order.
    pub(crate) fn owes(&self, issue_id: &str) -> bool {
        self.lock()
            .owed
            .iter()
            .any(|owed| owed.issue.id == issue_id)
    }

    /// Queue `write`, whose inline try failed at `now` asking for `retry_after`, for its
    /// first retry.
    pub(crate) fn queue_retry(
        &self,
        issue: &Issue,
        write: Write,
        now: Instant,
        retry_after: Option<Duration>,
    ) {
        let gap = next_gap(Duration::ZERO, retry_after);
        self.push(issue, write, now, now + gap, gap, 1);
    }

    /// Queue `write` behind the writes `issue` is already owed, to be tried as soon as they
    /// have settled.
    pub(crate) fn queue_behind(&self, issue: &Issue, write: Write, now: Instant) {
        self.push(issue, write, now, now, Duration::ZERO, 0);
    }

    fn push(
        &self,
        issue: &Issue,
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
            issue: issue.clone(),
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
    fn take_due(&self, now: Instant) -> Option<(u64, Issue, Write)> {
        let mut queue = self.lock();
        let owed = queue
            .heads_mut()
            .find(|owed| !owed.in_flight && owed.next_try <= now)?;
        owed.in_flight = true;
        Some((owed.seq, owed.issue.clone(), owed.write.clone()))
    }

    /// One pass: try every issue's oldest write that is due, and settle each try — landed,
    /// rescheduled, or given up. An issue whose write settles has its next one tried in the
    /// same pass, so once Linear is back an issue's whole backlog lands at once, in order.
    /// When the earliest write left is due next, if any.
    pub(crate) fn deliver_due(
        &self,
        tracker: &dyn Tracker,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Option<Instant> {
        while let Some((seq, issue, write)) = self.take_due(clock.now()) {
            let tried = write.perform(tracker, &issue);
            let now = clock.now();
            let mut queue = self.lock();
            let Some(at) = queue.owed.iter().position(|owed| owed.seq == seq) else {
                continue;
            };
            match tried {
                Ok(()) => {
                    queue.owed.remove(at);
                    drop(queue);
                    landed(tracker, &issue, &write, diag);
                }
                Err(e) if e.transient() && now < queue.owed[at].since + RETRY_FOR => {
                    let owed = &mut queue.owed[at];
                    owed.in_flight = false;
                    owed.tries += 1;
                    owed.delay = next_gap(owed.delay, e.retry_after());
                    owed.next_try = now + owed.delay;
                    let (tries, delay) = (owed.tries, owed.delay);
                    drop(queue);
                    diag.err(&format_args!(
                        "linear could not take a write for {}: {e}; {} is tried again in {} \
                         (try {})",
                        named(&issue),
                        write.what(),
                        span(delay),
                        tries + 1
                    ));
                }
                Err(e) => {
                    let since = queue.owed.remove(at).map_or(now, |owed| owed.since);
                    drop(queue);
                    diag.err(&lost_line(&issue, &write, &e, Some(now - since)));
                }
            }
        }
        self.lock().next_due()
    }

    /// The worker: deliver what is due, then sleep until the next write is due or a new one
    /// is queued. Runs for the life of the process, on its own thread and its own tracker
    /// client, which no call's deadline ever reaches.
    pub(crate) fn run(&self, tracker: &dyn Tracker, clock: &dyn Clock, diag: &dyn Diag) {
        loop {
            self.deliver_due(tracker, clock, diag);
            let mut queue = self.lock();
            // Looked at again under the lock each time, so a push between the pass and the
            // wait is never missed.
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

    /// Say every write still owed, one line each: afkd has ended the plugin, and they go with
    /// it.
    pub(crate) fn abandon(&self, diag: &dyn Diag) {
        for owed in &self.lock().owed {
            diag.err(&format_args!(
                "afkd ended the plugin with {} for {} still owed",
                owed.write.what(),
                named(&owed.issue)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! The queue and its policy over a mock tracker, on a fake clock, one pass at a time:
    //! the worker thread itself is `tests/wire.rs`'s.

    use super::*;
    use crate::common::{CaptureDiag, FakeClock};
    use crate::tracker::MockTracker;

    /// The issue every test writes to: the delimiter a log line wraps it in, wide CJK, an
    /// emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    fn tracker() -> MockTracker {
        let tracker = MockTracker::new();
        tracker.add_state("Todo", "unstarted");
        tracker.add_issue("Todo", "ENG-1", TITLE, "");
        tracker.add_issue("Todo", "ENG-2", "第二の課題 🚧", "");
        tracker
    }

    fn issue(tracker: &MockTracker, id: &str) -> Issue {
        tracker.issue(id)
    }

    fn note(text: &str) -> Write {
        Write::Note(text.to_string())
    }

    /// The texts on an issue, in the order posted.
    fn texts(tracker: &MockTracker, id: &str) -> Vec<String> {
        tracker
            .comments_on(id)
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    /// Nothing is tried before it is due, and the gap after each failed try doubles from
    /// [`RETRY_FIRST`] to [`RETRY_CAP`], where it stays; each failure says when the next try
    /// comes.
    #[test]
    fn the_backoff_doubles_from_5s_and_caps_at_5min() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        tracker.go_down();
        let start = clock.now();
        outbox.queue_retry(
            &issue(&tracker, "ENG-1"),
            note("[afkd-attempt] 1/2: boom"),
            start,
            None,
        );

        let mut due = start + RETRY_FIRST;
        let mut gaps = vec![(due - start).as_secs()];
        for _ in 0..8 {
            let before = tracker.calls().len();
            clock.advance(due - clock.now() - Duration::from_millis(1));
            assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), Some(due));
            assert_eq!(tracker.calls().len(), before, "tried before it was due");
            clock.advance(Duration::from_millis(1));
            let next = outbox
                .deliver_due(&tracker, &clock, &diag)
                .expect("still owed");
            assert_eq!(tracker.calls().len(), before + 1, "one try when due");
            gaps.push((next - due).as_secs());
            due = next;
        }
        assert_eq!(
            gaps[..],
            [5, 10, 20, 40, 80, 160, 300, 300, 300][..],
            "the first gap is the inline try's"
        );
        assert_eq!(
            diag.errs()[0],
            format!(
                "linear could not take a write for issue ENG-1 \"{TITLE}\": linear post comment: \
                 no response (mock failure); the comment \"[afkd-attempt] 1/2: boom\" is tried \
                 again in 10s (try 3)"
            )
        );
    }

    /// A rate limit that names its wait is not asked again sooner: the first retry waits the
    /// `Retry-After` when it is longer than the backoff's gap, a later one too, and a wait
    /// past [`RETRY_CAP`] is held to it. One shorter than the gap changes nothing.
    #[test]
    fn a_retry_after_floor_delays_the_next_attempt() {
        assert_eq!(
            next_gap(Duration::ZERO, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            next_gap(Duration::ZERO, Some(Duration::from_secs(1))),
            RETRY_FIRST
        );
        assert_eq!(
            next_gap(Duration::from_secs(10), Some(Duration::from_secs(90))),
            Duration::from_secs(90)
        );
        assert_eq!(
            next_gap(Duration::from_secs(10), Some(Duration::from_secs(3600))),
            RETRY_CAP
        );
        assert_eq!(next_gap(Duration::from_secs(160), None), RETRY_CAP);

        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        let limited = LinearError::Refused {
            stage: "post comment",
            status: 429,
            message: "Rate limit exceeded".into(),
            kind: "ratelimited".into(),
            retry_after: Some(Duration::from_secs(40)),
        };
        let start = clock.now();
        outbox.queue_retry(
            &issue(&tracker, "ENG-1"),
            note("note"),
            start,
            Some(Duration::from_secs(7)),
        );
        assert_eq!(
            outbox.lock().next_due(),
            Some(start + Duration::from_secs(7))
        );
        tracker.fail("post comment", limited);
        clock.advance(Duration::from_secs(7));
        let next = outbox
            .deliver_due(&tracker, &clock, &diag)
            .expect("still owed");
        assert_eq!(
            next - clock.now(),
            Duration::from_secs(40),
            "the second wait is Linear's"
        );
        tracker.clear_failure();
        clock.advance(Duration::from_secs(39));
        assert_eq!(
            outbox.deliver_due(&tracker, &clock, &diag),
            Some(next),
            "not before it"
        );
        clock.advance(Duration::from_secs(1));
        assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), None);
        assert_eq!(texts(&tracker, "ENG-1"), ["note"]);
    }

    #[test]
    fn a_write_is_given_up_after_an_hour_naming_the_issue_and_what_was_lost() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        tracker.go_down();
        outbox.queue_retry(
            &issue(&tracker, "ENG-1"),
            note("reviewed, nothing to add"),
            clock.now(),
            None,
        );
        while let Some(next) = outbox.deliver_due(&tracker, &clock, &diag) {
            clock.advance(next - clock.now());
        }
        assert!(!outbox.owes("ENG-1"));
        let gave_up: Vec<String> = diag
            .errs()
            .into_iter()
            .filter(|l| !l.starts_with("linear could not take"))
            .collect();
        assert_eq!(
            gave_up,
            [format!(
                "gave up on issue ENG-1 \"{TITLE}\" after 1h: the comment \"reviewed, nothing to \
                 add\" is lost — linear post comment: no response (mock failure)"
            )]
        );
        assert!(diag.narrated().is_empty(), "nothing landed");
    }

    /// A Linear that answers and refuses is not asked again: the entry is dropped after its
    /// one try from the queue, said loudly, and the issue's next write is tried.
    #[test]
    fn a_refusal_is_not_retried() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        let first = issue(&tracker, "ENG-1");
        outbox.queue_retry(&first, note("first"), clock.now(), None);
        outbox.queue_behind(&first, note("second"), clock.now());
        tracker.fail(
            "post comment",
            LinearError::Refused {
                stage: "post comment",
                status: 400,
                message: "Argument Validation Error".into(),
                kind: "invalid input".into(),
                retry_after: None,
            },
        );
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), None);
        assert_eq!(
            diag.errs(),
            [
                format!(
                    "gave up on issue ENG-1 \"{TITLE}\" after 5s: the comment \"first\" is lost — \
                     linear post comment: Linear refused it (400): Argument Validation Error"
                ),
                format!(
                    "gave up on issue ENG-1 \"{TITLE}\" after 5s: the comment \"second\" is lost — \
                     linear post comment: Linear refused it (400): Argument Validation Error"
                ),
            ]
        );
    }

    /// A reply that cannot be read may follow a write that landed: the entry is dropped,
    /// said as neither landed nor lost.
    #[test]
    fn a_decode_on_a_queued_write_is_popped_as_unknown_not_lost() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        outbox.queue_retry(&issue(&tracker, "ENG-1"), note("first"), clock.now(), None);
        tracker.fail(
            "post comment",
            LinearError::Decode {
                stage: "post comment",
                reason: "mock failure".into(),
            },
        );
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), None);
        let errs = diag.errs();
        assert_eq!(
            errs,
            [format!(
                "linear answered issue ENG-1 \"{TITLE}\" with a reply the plugin could not read; \
                 the comment \"first\" may or may not have landed — linear post comment: \
                 undecodable response (mock failure)"
            )]
        );
    }

    /// Once its head lands, an issue's whole backlog lands in the same pass, in order, each
    /// write narrated.
    #[test]
    fn one_pass_drains_an_issues_backlog_once_its_head_lands() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        let first = issue(&tracker, "ENG-1");
        tracker.go_down();
        outbox.queue_retry(&first, note("[afkd-attempt] 1/2: boom"), clock.now(), None);
        outbox.queue_behind(&first, note("[afkd-attempt] 2/2: boom again"), clock.now());
        outbox.queue_behind(
            &first,
            Write::Watermark {
                text: "[afkd-ran] owner=a upto=3".into(),
                priors: Vec::new(),
            },
            clock.now(),
        );
        clock.advance(RETRY_FIRST);
        let due = outbox
            .deliver_due(&tracker, &clock, &diag)
            .expect("still owed");
        tracker.clear_failure();
        clock.advance(due - clock.now());
        assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), None);
        assert_eq!(
            texts(&tracker, "ENG-1"),
            [
                "[afkd-attempt] 1/2: boom",
                "[afkd-attempt] 2/2: boom again",
                "[afkd-ran] owner=a upto=3"
            ]
        );
        assert_eq!(
            diag.narrated(),
            [
                format!(
                    "[linear] posting the comment \"[afkd-attempt] 1/2: boom\" on issue ENG-1 \
                     \"{TITLE}\""
                ),
                format!(
                    "[linear] posting the comment \"[afkd-attempt] 2/2: boom again\" on issue \
                     ENG-1 \"{TITLE}\""
                ),
                format!("[linear] posting the [afkd-ran] watermark on issue ENG-1 \"{TITLE}\""),
            ]
        );
    }

    /// A watermark that lands from the queue prunes the priors it supersedes; a prune that
    /// fails is said and dropped, never queued again.
    #[test]
    fn a_landed_watermark_prunes_its_priors_and_a_failed_prune_is_dropped() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        tracker.seed_comment("ENG-1", "ran1", "[afkd-ran] owner=a upto=1000", 1);
        tracker.seed_comment("ENG-1", "ran2", "[afkd-ran] owner=a upto=2000", 2);
        let watermark = |upto: u64| Write::Watermark {
            text: format!("[afkd-ran] owner=a upto={upto}"),
            priors: vec!["ran1".into(), "ran2".into()],
        };
        outbox.queue_retry(
            &issue(&tracker, "ENG-1"),
            watermark(3000),
            clock.now(),
            None,
        );
        tracker.fail(
            "delete comment",
            LinearError::Transport {
                stage: "delete comment",
                reason: "mock failure".into(),
            },
        );
        clock.advance(RETRY_FIRST);
        assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), None);
        assert!(!outbox.owes("ENG-1"), "nothing queued again");
        assert_eq!(
            diag.errs(),
            [
                "linear delete comment: no response (mock failure)",
                "linear delete comment: no response (mock failure)",
            ]
        );

        tracker.clear_failure();
        outbox.queue_retry(
            &issue(&tracker, "ENG-1"),
            watermark(4000),
            clock.now(),
            None,
        );
        clock.advance(RETRY_FIRST);
        outbox.deliver_due(&tracker, &clock, &diag);
        assert_eq!(
            texts(&tracker, "ENG-1"),
            [
                "[afkd-ran] owner=a upto=3000",
                "[afkd-ran] owner=a upto=4000"
            ]
        );
    }

    /// One issue's writes wait behind its own head, and no other issue's do.
    #[test]
    fn one_issues_writes_land_in_order_and_others_are_not_held_behind_it() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        let (first, second) = (issue(&tracker, "ENG-1"), issue(&tracker, "ENG-2"));
        outbox.queue_retry(
            &first,
            note("one"),
            clock.now(),
            Some(Duration::from_secs(60)),
        );
        outbox.queue_behind(&first, note("two"), clock.now());
        outbox.queue_retry(&second, note("other"), clock.now(), None);
        clock.advance(RETRY_FIRST);
        outbox.deliver_due(&tracker, &clock, &diag);
        assert_eq!(texts(&tracker, "ENG-2"), ["other"], "ENG-2 lands");
        assert!(
            texts(&tracker, "ENG-1").is_empty(),
            "ENG-1 waits for its head's minute"
        );
        assert!(outbox.owes("ENG-1") && !outbox.owes("ENG-2"));
        clock.advance(Duration::from_secs(55));
        assert_eq!(outbox.deliver_due(&tracker, &clock, &diag), None);
        assert_eq!(texts(&tracker, "ENG-1"), ["one", "two"]);
        assert!(tracker.actions().is_empty());
    }

    /// A write being tried is still owed: a write to its issue queues behind it rather than
    /// overtaking it.
    #[test]
    fn an_entry_in_flight_still_owes_its_issue() {
        let (outbox, tracker, clock) = (Outbox::default(), tracker(), FakeClock::new());
        outbox.queue_retry(&issue(&tracker, "ENG-1"), note("x"), clock.now(), None);
        clock.advance(RETRY_FIRST);
        let (_, taken, _) = outbox.take_due(clock.now()).expect("due");
        assert_eq!(taken.id, "ENG-1");
        assert!(outbox.owes("ENG-1"));
        assert!(
            outbox.take_due(clock.now()).is_none(),
            "tried once at a time"
        );
        assert_eq!(outbox.lock().next_due(), None, "and not waited on");
    }

    #[test]
    fn abandon_names_every_write_still_owed() {
        let (outbox, tracker, clock, diag) = (
            Outbox::default(),
            tracker(),
            FakeClock::new(),
            CaptureDiag::default(),
        );
        let reason = format!("{}\nsecond line", "修".repeat(100));
        outbox.queue_retry(
            &issue(&tracker, "ENG-1"),
            Write::Note(reason),
            clock.now(),
            None,
        );
        outbox.queue_behind(
            &issue(&tracker, "ENG-1"),
            Write::Watermark {
                text: "[afkd-ran] owner=a upto=3".into(),
                priors: Vec::new(),
            },
            clock.now(),
        );
        outbox.abandon(&diag);
        assert_eq!(
            diag.errs(),
            [
                format!(
                    "afkd ended the plugin with the comment \"{}…\" for issue ENG-1 \"{TITLE}\" \
                     still owed",
                    "修".repeat(80)
                ),
                format!(
                    "afkd ended the plugin with the [afkd-ran] watermark for issue ENG-1 \
                     \"{TITLE}\" still owed"
                ),
            ]
        );
    }

    #[test]
    fn every_write_has_its_phrase() {
        for (write, what) in [
            (
                note("## 完了 ✅\n\n- took 3m"),
                "the comment \"## 完了 ✅…\"",
            ),
            (note(""), "the comment \"\""),
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
