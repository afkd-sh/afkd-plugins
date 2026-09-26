//! `afkd-trello`: the `@afkd/trello` trigger plugin.
//!
//! One long-lived child per armed service, spoken to by afkd in newline-delimited JSON on
//! stdin and stdout (afkd's `docs/plugins.md`, "The trigger protocol"). stdout carries
//! replies and nothing else; every diagnostic and every success line goes to stderr, which
//! afkd streams into the service log under `[@afkd/trello:err]`.
//!
//! The `trello` kind ([`card`]) is the vendor half of afkd's built-in Trello trigger,
//! ported. It keeps the built-in's claim, watermark, attempt and park comments, its
//! claim-journal keys, session threads, run env and brief, so a claim either one left on a
//! live card is recognised, renewed and released by the other, and a card parked by one is
//! resumed by the other.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod board;
mod card;
mod claim;
mod client;
mod common;
mod http;
mod lifecycle;
#[cfg(test)]
mod manifest;
mod plugin;
mod rfc3339;
mod run_ref;
mod settings;
mod wire;

use std::io::{BufRead, Write};

use crate::common::{StderrDiag, SystemClock};
use crate::plugin::{Answer, Plugin};

/// The brief at the scratch directory's root, which afkd writes framed.
pub(crate) const TASK_FILE: &str = "task.md";

/// The park marker the trello skill's ask action writes into the attempt's scratch
/// directory, which `classify` reads.
pub(crate) const PARK_FILE: &str = "park";

fn main() {
    let mut plugin = Plugin::new(Box::new(SystemClock), Box::new(StderrDiag));
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("afkd-trello: stdin is unreadable: {e}");
                std::process::exit(2);
            }
        };
        let request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("afkd-trello: afkd sent a line that is not a request ({e})");
                std::process::exit(2);
            }
        };
        match plugin.answer(request) {
            Answer::Reply(reply) => {
                let written = writeln!(stdout, "{reply}").and_then(|()| stdout.flush());
                if let Err(e) = written {
                    eprintln!("afkd-trello: stdout is closed: {e}");
                    std::process::exit(2);
                }
            }
            Answer::Fatal(reason) => {
                eprintln!("afkd-trello: {reason}");
                std::process::exit(1);
            }
        }
    }
}
