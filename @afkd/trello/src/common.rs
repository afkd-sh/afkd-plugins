//! The seams the kind is written against — the [`Clock`] the claim settles on and the
//! [`Diag`] sink its lines go to — plus the env spellings and the poll's scan budget.
//!
//! One thing the built-in has and this does not: afkd's stop. The plugin cannot see it,
//! so a claim never abandons mid-settle; afkd hands a unit polled during a stop straight
//! back with `release`, which reverses the claim.

use std::fmt::Display;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Env var carrying the board API key to the run.
pub(crate) const ENV_API_KEY: &str = "TRELLO_API_KEY";
/// Env var carrying the board token to the run.
pub(crate) const ENV_TOKEN: &str = "TRELLO_TOKEN";
/// Env var carrying the board id to the run.
pub(crate) const ENV_BOARD_ID: &str = "TRELLO_BOARD_ID";
/// Env var carrying the active card id to the run.
pub(crate) const ENV_CARD_ID: &str = "TRELLO_CARD_ID";

/// How long one `poll`'s scan may run before it stops considering further candidates,
/// leaving them to the next beat. afkd ends the service if a call takes 60 seconds, and
/// each claim attempt settles for a second; this keeps the scan well inside it. A claim
/// already under way is always finished.
pub(crate) const POLL_BUDGET: Duration = Duration::from_secs(20);

/// Lock `m`, recovering the data from a poisoned lock rather than panicking: a panic
/// elsewhere must not take every later caller down with it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The time the claim settles on and the poll budget is measured with.
pub(crate) trait Clock {
    /// Block for `d`.
    fn sleep(&self, d: Duration);
    /// The current monotonic instant.
    fn now(&self) -> Instant;
}

/// The real clock.
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Where the kind's lines go. stdout is afkd's protocol, so in the running plugin both
/// halves are stderr, which afkd streams into the service log under
/// `[@afkd/trello:err]`.
pub(crate) trait Diag {
    /// Surface one problem.
    fn err(&self, err: &dyn Display);

    /// Narrate one success — a claimed card, a lifecycle action that landed, a released
    /// claim, a parked card, a takeover — in the built-in trigger's own words.
    fn narrate(&self, line: &str);
}

/// The running plugin's [`Diag`]: one `afkd-trello: …` line per problem, and each success
/// line verbatim, so it reads exactly as the built-in's narration did.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-trello: {err}");
    }

    fn narrate(&self, line: &str) {
        eprintln!("{line}");
    }
}

/// One `poll`'s scan against [`POLL_BUDGET`], started when the scan starts.
pub(crate) struct ScanBudget<'a> {
    clock: &'a dyn Clock,
    diag: &'a dyn Diag,
    started: Instant,
}

impl<'a> ScanBudget<'a> {
    pub(crate) fn start(clock: &'a dyn Clock, diag: &'a dyn Diag) -> Self {
        Self {
            clock,
            diag,
            started: clock.now(),
        }
    }

    /// Whether the scan has run its budget — said on the diagnostic channel when it has,
    /// since the scan stops there.
    pub(crate) fn spent(&self) -> bool {
        let spent = self.clock.now().duration_since(self.started) >= POLL_BUDGET;
        if spent {
            self.diag.err(&format_args!(
                "trello poll: the scan ran past its {}s budget; the rest of it waits for the \
                 next poll",
                POLL_BUDGET.as_secs()
            ));
        }
        spent
    }
}

/// A [`Clock`] for tests: `sleep` returns at once, records the wait, and moves `now` on
/// by it; [`advance`](Self::advance) moves `now` without a sleep, and a
/// [`ticking`](Self::ticking) clock moves it on by a fixed step after every read — each
/// standing in for a slow board round trip.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeClock {
    elapsed: Mutex<Duration>,
    sleeps: Mutex<Vec<Duration>>,
    tick: Duration,
    base: std::sync::OnceLock<Instant>,
}

#[cfg(test)]
impl FakeClock {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A clock that moves on by `tick` after every `now` it answers.
    pub(crate) fn ticking(tick: Duration) -> Self {
        Self {
            tick,
            ..Self::default()
        }
    }

    /// Every `sleep` asked for, in order.
    pub(crate) fn sleeps(&self) -> Vec<Duration> {
        lock(&self.sleeps).clone()
    }

    /// Move `now` on by `d`.
    pub(crate) fn advance(&self, d: Duration) {
        *lock(&self.elapsed) += d;
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn sleep(&self, d: Duration) {
        lock(&self.sleeps).push(d);
        self.advance(d);
    }

    fn now(&self) -> Instant {
        let mut elapsed = lock(&self.elapsed);
        let now = *self.base.get_or_init(Instant::now) + *elapsed;
        *elapsed += self.tick;
        now
    }
}

/// A [`Diag`] for tests: every problem and every narrated line, kept apart — the two
/// streams the built-in's tests read as its sink's stderr and stdout.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct CaptureDiag {
    errs: Mutex<Vec<String>>,
    narrated: Mutex<Vec<String>>,
}

#[cfg(test)]
impl CaptureDiag {
    /// The problems, in order.
    pub(crate) fn errs(&self) -> Vec<String> {
        lock(&self.errs).clone()
    }

    /// The success lines, in order.
    pub(crate) fn narrated(&self) -> Vec<String> {
        lock(&self.narrated).clone()
    }
}

#[cfg(test)]
impl Diag for CaptureDiag {
    fn err(&self, err: &dyn Display) {
        lock(&self.errs).push(err.to_string());
    }

    fn narrate(&self, line: &str) {
        lock(&self.narrated).push(line.to_string());
    }
}

/// A scratch directory for tests, removed on drop — the crate carries no `tempfile`.
#[cfg(test)]
pub(crate) struct TempDir(std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    /// A fresh, empty directory under the system temp dir, unique to this process and
    /// call.
    pub(crate) fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "afkd-trello-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("create a scratch dir");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is spent at exactly [`POLL_BUDGET`], says so once per check that finds
    /// it spent, and is measured on the clock it was handed.
    #[test]
    fn a_scan_budget_is_spent_at_its_bound_and_says_so() {
        let clock = FakeClock::new();
        let diag = CaptureDiag::default();
        let budget = ScanBudget::start(&clock, &diag);
        clock.advance(POLL_BUDGET - Duration::from_millis(1));
        assert!(!budget.spent());
        assert!(diag.errs().is_empty());
        clock.advance(Duration::from_millis(1));
        assert!(budget.spent());
        assert_eq!(
            diag.errs(),
            [
                "trello poll: the scan ran past its 20s budget; the rest of it waits for the \
              next poll"
            ]
        );
        assert!(
            diag.narrated().is_empty(),
            "a spent budget is not a success"
        );
    }
}
