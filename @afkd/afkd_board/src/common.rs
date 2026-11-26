//! The seams the service is written against: the [`Clock`] the claim settles on and the
//! [`Diag`] sink its lines go to, so the tests drive the 3 s settle and read what was said
//! without waiting or capturing a process's stderr.

use std::fmt::Display;
use std::time::{Duration, Instant};

/// The time the claim settles on and the poll's budget is measured with.
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

/// Where the service's lines go. stdout is afkd's protocol, so in the running plugin both
/// halves are stderr, which afkd streams into the service log under `[@afkd/afkd_board:err]`.
pub(crate) trait Diag {
    /// Surface one problem.
    fn err(&self, err: &dyn Display);

    /// Narrate one success: a claimed card, a released label.
    fn narrate(&self, line: &str);
}

/// The running plugin's sink: every line to stderr, behind the plugin's name.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-afkd_board: {err}");
    }

    fn narrate(&self, line: &str) {
        eprintln!("afkd-afkd_board: {line}");
    }
}

#[cfg(test)]
pub(crate) use tests_support::{CaptureDiag, FakeClock, TempDir};

/// What the unit tests of the service and the plugin share: a clock that never sleeps, a sink
/// that keeps what it is told, and a scratch directory.
#[cfg(test)]
mod tests_support {
    use std::fmt::Display;
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use super::{Clock, Diag};

    /// Lock `m`, recovering the data from a poisoned lock.
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A clock that moves only when told, or when slept on, which takes no time.
    pub(crate) struct FakeClock {
        base: Instant,
        elapsed: Mutex<Duration>,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl FakeClock {
        pub(crate) fn new() -> Self {
            Self {
                base: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
                sleeps: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn advance(&self, d: Duration) {
            *lock(&self.elapsed) += d;
        }

        /// Every sleep so far, in order.
        pub(crate) fn sleeps(&self) -> Vec<Duration> {
            lock(&self.sleeps).clone()
        }
    }

    impl Clock for FakeClock {
        fn sleep(&self, d: Duration) {
            lock(&self.sleeps).push(d);
            self.advance(d);
        }

        fn now(&self) -> Instant {
            self.base + *lock(&self.elapsed)
        }
    }

    /// A sink that keeps every problem and every narrated line, apart.
    #[derive(Default)]
    pub(crate) struct CaptureDiag {
        errs: Mutex<Vec<String>>,
        narrated: Mutex<Vec<String>>,
    }

    impl CaptureDiag {
        pub(crate) fn errs(&self) -> Vec<String> {
            lock(&self.errs).clone()
        }

        pub(crate) fn narrated(&self) -> Vec<String> {
            lock(&self.narrated).clone()
        }
    }

    impl Diag for CaptureDiag {
        fn err(&self, err: &dyn Display) {
            lock(&self.errs).push(err.to_string());
        }

        fn narrate(&self, line: &str) {
            lock(&self.narrated).push(line.to_string());
        }
    }

    /// A scratch directory, removed on drop: the crate carries no `tempfile`.
    pub(crate) struct TempDir(std::path::PathBuf);

    impl TempDir {
        pub(crate) fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "afkd_board-{label}-{}-{}",
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

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
