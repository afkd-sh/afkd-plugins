//! `afkd-linear`: the `@afkd/linear` trigger plugin.
//!
//! One long-lived child per armed service, spoken to by afkd in newline-delimited JSON on
//! stdin and stdout (afkd's `docs/plugins.md`, "The trigger protocol"). stdout carries
//! replies and nothing else; every diagnostic and every success line goes to stderr, which
//! afkd streams into the service log under `[@afkd/linear:err]`.
//!
//! The `issue` kind ([`issue`]) — `service(linear)` in a config — claims a Linear team's
//! issues one per run, by the scheme `@afkd/trello` claims cards with: a claim comment raced
//! and settled on the issue, the same watermark, attempt and park comments, and the same
//! journal keys, session threads, run env and brief. It talks to Linear's GraphQL API
//! ([`client`]) as an API key or as an OAuth app ([`auth`]). The slots are afkd's, and the
//! actions they call on an issue — comment, describe, rename, move, label, file an issue or
//! a sub-issue, archive, and read its thread, description and sub-issues — are [`action`]'s.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod action;
mod auth;
mod claim;
mod client;
mod common;
mod http;
mod issue;
#[cfg(test)]
mod manifest;
mod outbox;
mod plugin;
mod rfc3339;
#[cfg(test)]
#[path = "../tests/common/sdl.rs"]
mod sdl;
mod settings;
mod tracker;
mod wire;

use std::io::{BufRead, Write};

use crate::common::{StderrDiag, SystemClock};
use crate::plugin::{Answer, Plugin};

/// The brief at the scratch directory's root, which afkd writes framed.
pub(crate) const TASK_FILE: &str = "task.md";

/// The park marker an agent's ask writes into the attempt's scratch directory, which
/// `classify` reads.
pub(crate) const PARK_FILE: &str = "park";

fn main() {
    let mut plugin = Plugin::new(Box::new(SystemClock), Box::new(StderrDiag));
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("afkd-linear: stdin is unreadable: {e}");
                std::process::exit(2);
            }
        };
        let request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("afkd-linear: afkd sent a line that is not a request ({e})");
                std::process::exit(2);
            }
        };
        match plugin.answer(request) {
            Answer::Reply(reply) => {
                let written = writeln!(stdout, "{reply}").and_then(|()| stdout.flush());
                if let Err(e) = written {
                    eprintln!("afkd-linear: stdout is closed: {e}");
                    std::process::exit(2);
                }
            }
            Answer::Fatal(reason) => {
                eprintln!("afkd-linear: {reason}");
                std::process::exit(1);
            }
        }
    }
    // afkd closed the pipe: the process ends, and its outbox with it.
    plugin.close();
}
