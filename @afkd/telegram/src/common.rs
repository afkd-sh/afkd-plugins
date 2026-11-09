//! The seams the plugin is written against — the [`Clock`] a 429 is waited out on and the
//! [`Diag`] sink its lines go to — plus the env spellings a run gets and the call budget.

use std::fmt::Display;
use std::time::{Duration, Instant};

/// Env var carrying the bot's token to the run.
pub(crate) const ENV_TOKEN: &str = "TELEGRAM_BOT_TOKEN";
/// Env var carrying the Bot API's root to the run, `https://api.telegram.org` unless the
/// service's `base_url` names another.
pub(crate) const ENV_API_URL: &str = "TELEGRAM_API_URL";
/// Env var carrying the message's chat id to the run.
pub(crate) const ENV_CHAT_ID: &str = "TELEGRAM_CHAT_ID";
/// Env var carrying the message's own id to the run.
pub(crate) const ENV_MESSAGE_ID: &str = "TELEGRAM_MESSAGE_ID";

/// The longest any one call afkd sends may take, request after request. afkd's reply
/// deadline is 60 seconds and a missed reply ends the service; the 15 seconds between the
/// two cover building the reply and the pipe. Every request of the call is clipped to what
/// is left of it, past it none is sent, and a 429 is waited out only while its wait fits in
/// what is left.
pub(crate) const CALL_BUDGET: Duration = Duration::from_secs(45);

/// The time a 429 is waited out on and the call budget is measured with.
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

/// Where the plugin's lines go. stdout is afkd's protocol, so in the running plugin this is
/// stderr, which afkd streams into the service log under `[@afkd/telegram:err]`.
pub(crate) trait Diag {
    /// Surface one line: a problem, or a message skipped.
    fn err(&self, err: &dyn Display);
}

/// The running plugin's [`Diag`]: one `afkd-telegram: …` line each.
pub(crate) struct StderrDiag;

impl Diag for StderrDiag {
    fn err(&self, err: &dyn Display) {
        eprintln!("afkd-telegram: {err}");
    }
}
