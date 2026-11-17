//! The seams the kind is written against — the [`Clock`] the claim settles on and the
//! [`Diag`] sink its lines go to — plus the env spellings, the call budget, the poll's scan
//! budget, and the one way a file is written whole.
//!
//! afkd's stop is invisible to the plugin, so a claim never abandons mid-settle; afkd hands
//! a unit polled during a stop straight back with `release`, which reverses the claim.

use std::fmt::Display;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Env var carrying Linear's GraphQL endpoint to the run.
pub(crate) const ENV_API_URL: &str = "LINEAR_API_URL";
/// Env var carrying the exact `Authorization` header value to the run: the API key as it is,
/// or `Bearer <token>` for an app, as of the claim. One variable for both credentials, so a
/// script reaching Linear has one code path.
pub(crate) const ENV_AUTHORIZATION: &str = "LINEAR_AUTHORIZATION";
/// Env var carrying the active issue's id (its uuid) to the run.
pub(crate) const ENV_ISSUE_ID: &str = "LINEAR_ISSUE_ID";
/// Env var carrying the active issue's identifier (`ENG-123`) to the run.
pub(crate) const ENV_ISSUE: &str = "LINEAR_ISSUE";

/// How long one `poll`'s scan may run before it stops considering further candidates,
/// leaving them to the next beat. afkd ends the service if a call takes 60 seconds, and
/// each claim attempt settles for three seconds; this keeps the scan well inside it. A
/// claim already under way is always finished.
pub(crate) const POLL_BUDGET: Duration = Duration::from_secs(20);

/// The longest any one call afkd sends may take, request after request. afkd's reply
/// deadline is 60 seconds and a missed reply ends the service; the 15 seconds between
/// the two cover the claim's three-second settle, building the reply and the pipe. Every
/// request of the call is clipped to what is left of it, and past it none is sent, so a
/// Linear too slow for it answers as a Linear that is down.
pub(crate) const CALL_BUDGET: Duration = Duration::from_secs(45);

/// A scan with less than this left of the call's [`CALL_BUDGET`] posts no claim: a claim
/// cut off after its comment lands leaves a live lease that `drop_claim` can no longer
/// delete, keeping every service off the issue for the lease's lifetime. Ten seconds holds
/// the claim's [`CLAIM_SETTLE`](crate::claim::CLAIM_SETTLE) and leaves seven for the post
/// before it and the re-read and release after it.
pub(crate) const CLAIM_RESERVE: Duration = Duration::from_secs(10);

/// Lock `m`, recovering the data from a poisoned lock rather than panicking: a panic
/// elsewhere must not take every later caller down with it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `text` as a part of a file name: every byte but `A-Z a-z 0-9 _ -` written `%XX`, so no
/// text, a client id or an organization's uuid, can name another path or hide a file.
pub(crate) fn file_safe(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            safe.push(char::from(b));
        } else {
            safe.push_str(&format!("%{b:02X}"));
        }
    }
    safe
}

/// Write `bytes` to `dest` whole: to a temp file beside it, `.<name>.afkd-linear-<pid>`, made
/// with `mode` (less the umask), then renamed onto it. A reader, another child of this plugin
/// among them, sees the old file or the new one and never half of either, and when two
/// children race the later rename wins. The temp file goes when the write fails.
///
/// # Errors
///
/// Returns the I/O error of the first step that failed: a `dest` that names no file, a
/// directory that is not there or cannot be written, or a rename onto a directory.
pub(crate) fn write_whole(dest: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let Some(name) = dest.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path names no file",
        ));
    };
    let temp = dest.with_file_name(format!(
        ".{}.afkd-linear-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // A temp file a crashed write of this pid left behind would keep its own mode.
    let _ = std::fs::remove_file(&temp);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| std::fs::rename(&temp, dest));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
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
/// `[@afkd/linear:err]`.
pub(crate) trait Diag {
    /// Surface one problem.
    fn err(&self, err: &dyn Display);

    /// Narrate one success — a claimed issue, a released claim, a parked issue, a takeover,
    /// a read or a write done.
    fn narrate(&self, line: &str);
}

/// The running plugin's [`Diag`]: one `afkd-linear: …` line per problem, and each success
/// line verbatim.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-linear: {err}");
    }

    fn narrate(&self, line: &str) {
        eprintln!("{line}");
    }
}

/// One `poll`'s scan against [`POLL_BUDGET`], started when the scan starts, and against
/// the [`CLAIM_RESERVE`] of the call's deadline, when the call has one.
pub(crate) struct ScanBudget<'a> {
    clock: &'a dyn Clock,
    diag: &'a dyn Diag,
    started: Instant,
    /// The call's deadline, `None` outside an armed call.
    deadline: Option<Instant>,
}

impl<'a> ScanBudget<'a> {
    pub(crate) fn start(
        clock: &'a dyn Clock,
        diag: &'a dyn Diag,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            clock,
            diag,
            started: clock.now(),
            deadline,
        }
    }

    /// Whether the scan has run its budget, or is short of the claim reserve — said on
    /// the diagnostic channel when it is, since the scan stops there. One line per check,
    /// and the scan's own budget is the one named when both are out.
    pub(crate) fn spent(&self) -> bool {
        let now = self.clock.now();
        if now.duration_since(self.started) >= POLL_BUDGET {
            self.diag.err(&format_args!(
                "linear poll: the scan ran past its {}s budget; the rest of it waits for the \
                 next poll",
                POLL_BUDGET.as_secs()
            ));
            return true;
        }
        self.reserve_short(now)
    }

    /// Whether less than the claim reserve of the call's budget is left — checked right
    /// before a claim is posted, since a candidate's own reads sit between the scan's
    /// check and its claim. Reads no clock when the call has no deadline.
    pub(crate) fn claim_reserve_short(&self) -> bool {
        self.deadline.is_some() && self.reserve_short(self.clock.now())
    }

    /// Whether less than [`CLAIM_RESERVE`] is left of the call's deadline at `now`, said
    /// on the diagnostic channel when it is.
    fn reserve_short(&self, now: Instant) -> bool {
        let short = self
            .deadline
            .is_some_and(|d| d.saturating_duration_since(now) < CLAIM_RESERVE);
        if short {
            self.diag.err(&format_args!(
                "linear poll: less than {}s of the call's {}s budget is left; the rest of the \
                 scan waits for the next poll",
                CLAIM_RESERVE.as_secs(),
                CALL_BUDGET.as_secs()
            ));
        }
        short
    }
}

/// A [`Clock`] for tests: `sleep` returns at once, records the wait, and moves `now` on
/// by it; [`advance`](Self::advance) moves `now` without a sleep, and a
/// [`ticking`](Self::ticking) clock moves it on by a fixed step after every read — each
/// standing in for a slow Linear round trip.
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

/// A [`Diag`] for tests: every problem and every narrated line, kept apart.
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
            "afkd-linear-{label}-{}-{}",
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
    use std::os::unix::fs::PermissionsExt;

    /// A file is written whole in the mode asked for, over an older one, and no temp file is
    /// left beside it; a write into a directory that is not there fails and leaves nothing.
    #[test]
    fn a_file_is_written_whole_in_its_mode() {
        let dir = TempDir::new("whole");
        let dest = dir.path().join("token-app 監視.json");
        std::fs::write(&dest, "old").unwrap();
        write_whole(&dest, "{\"token\":\"t 🚨\"}".as_bytes(), 0o600).unwrap();
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "{\"token\":\"t 🚨\"}"
        );
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["token-app 監視.json"], "no temp file is left");
        let missing = dir.path().join("gone").join("x.json");
        assert!(write_whole(&missing, b"x", 0o600).is_err());
        assert!(!dir.path().join("gone").exists());
        assert!(write_whole(Path::new("/"), b"x", 0o600).is_err());
    }

    /// The budget is spent at exactly [`POLL_BUDGET`], says so once per check that finds
    /// it spent, and is measured on the clock it was handed.
    #[test]
    fn a_scan_budget_is_spent_at_its_bound_and_says_so() {
        let clock = FakeClock::new();
        let diag = CaptureDiag::default();
        let budget = ScanBudget::start(&clock, &diag, None);
        clock.advance(POLL_BUDGET - Duration::from_millis(1));
        assert!(!budget.spent());
        assert!(diag.errs().is_empty());
        clock.advance(Duration::from_millis(1));
        assert!(budget.spent());
        assert_eq!(
            diag.errs(),
            [
                "linear poll: the scan ran past its 20s budget; the rest of it waits for the \
              next poll"
            ]
        );
        assert!(
            diag.narrated().is_empty(),
            "a spent budget is not a success"
        );
    }

    /// Short of [`CLAIM_RESERVE`] the budget is spent, strictly: exactly the reserve left
    /// is not. Without a deadline the pre-claim check reads no clock, and with both bounds
    /// out only the scan's own is named.
    #[test]
    fn a_scan_budget_is_spent_short_of_the_claim_reserve() {
        let reserve_line = "linear poll: less than 10s of the call's 45s budget is left; the rest \
                            of the scan waits for the next poll";

        let clock = FakeClock::new();
        let diag = CaptureDiag::default();
        let budget = ScanBudget::start(&clock, &diag, Some(clock.now() + CLAIM_RESERVE));
        assert!(!budget.spent());
        assert!(!budget.claim_reserve_short());
        assert!(diag.errs().is_empty());
        clock.advance(Duration::from_millis(1));
        assert!(budget.spent());
        assert!(budget.claim_reserve_short());
        assert_eq!(diag.errs(), [reserve_line, reserve_line]);

        let clock = FakeClock::ticking(Duration::from_secs(1));
        let diag = CaptureDiag::default();
        let budget = ScanBudget::start(&clock, &diag, None);
        let before = clock.now();
        assert!(!budget.claim_reserve_short());
        assert_eq!(
            clock.now() - before,
            Duration::from_secs(1),
            "no clock read in between"
        );
        assert!(diag.errs().is_empty());

        let clock = FakeClock::new();
        let diag = CaptureDiag::default();
        let budget = ScanBudget::start(&clock, &diag, Some(clock.now() + CLAIM_RESERVE));
        clock.advance(POLL_BUDGET);
        assert!(budget.spent());
        assert_eq!(
            diag.errs(),
            [
                "linear poll: the scan ran past its 20s budget; the rest of it waits for the \
              next poll"
            ]
        );
    }
}
