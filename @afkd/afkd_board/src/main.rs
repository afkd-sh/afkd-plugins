//! `afkd-afkd_board`: the `@afkd/afkd_board` plugin.
//!
//! The plugin provides one trigger kind, `board` ([`service`]): `service(afkd_board)` in a config
//! claims a board's cards, one per run, by the `afkd/claimed` label and a race-safe claim
//! ([`claim`]), and keeps what it owes a card after its run ([`owed`]). afkd starts one
//! long-lived child per service and speaks to it in newline-delimited JSON on stdin and stdout
//! (afkd's `docs/plugins.md`, "The trigger protocol"): [`plugin`] answers `hello`, `poll`,
//! `finish`, `release` and `call`. stdout carries replies and nothing else; every diagnostic and
//! every success line goes to stderr, which afkd streams into the service log under
//! `[@afkd/afkd_board:err]`.
//!
//! Every other call a config makes is a child afkd starts for that one call, sent the one `call`
//! with no `hello` before it, answered the same way. The reads a config makes of a board on
//! app.afkd.sh (one card by its number, the cards a set of filters keeps, a card's comments and
//! its checklist, and the board's lists and labels) are [`reads`]'s. Each reaches the board only
//! through the `afkd_board.Board` it is passed ([`board`]), by its API key under its `base_url`,
//! over the board's HTTP API ([`client`]).
//!
//! The writes a config makes to it (cards, checklists, comments and lists) are [`writes`]'s. Each
//! write of a card, its checklist or a comment is made under the `afkd.Run` it is passed, which
//! the board shows beside the change; one passed none is shown under the key's name.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod board;
mod claim;
mod client;
mod common;
#[cfg(test)]
mod manifest;
mod owed;
mod plugin;
mod reads;
mod rfc3339;
mod service;
mod settings;
mod wire;
mod writes;

use std::io::{BufRead, Write};
use std::rc::Rc;

use crate::common::{StderrDiag, SystemClock};
use crate::plugin::{Answer, Plugin};

/// The brief at the scratch directory's root, which afkd writes framed.
pub(crate) const TASK_FILE: &str = "task/README.md";

fn main() {
    let mut plugin = Plugin::new(Rc::new(SystemClock), Rc::new(StderrDiag));
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("afkd-afkd_board: stdin is unreadable: {e}");
                std::process::exit(2);
            }
        };
        let request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("afkd-afkd_board: afkd sent a line that is not a request ({e})");
                std::process::exit(2);
            }
        };
        match plugin.answer(request) {
            Answer::Reply(reply) => {
                let written = writeln!(stdout, "{reply}").and_then(|()| stdout.flush());
                if let Err(e) = written {
                    eprintln!("afkd-afkd_board: stdout is closed: {e}");
                    std::process::exit(2);
                }
            }
            Answer::Fatal(reason) => {
                eprintln!("afkd-afkd_board: {reason}");
                std::process::exit(1);
            }
        }
    }
}
