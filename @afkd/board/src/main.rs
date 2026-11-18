//! `afkd-board`: the `@afkd/board` provider plugin.
//!
//! The plugin declares no kind, so afkd starts a child for each call a config makes and sends
//! it that one `call`, with no `hello` before it (afkd's `docs/plugins.md`, "The trigger
//! protocol"): newline-delimited JSON on stdin, one reply line on stdout. stdout carries
//! replies and nothing else; every diagnostic and every success line goes to stderr, which
//! afkd streams into the service log under `[@afkd/board:err]`.
//!
//! The reads a config makes of a board on app.afkd.sh — one card by its number, the cards a
//! set of filters keeps, a card's comments, and the board's lists and labels — are
//! [`reads`]'s. Each reaches the board only through the `board.Board` it is passed
//! ([`board`]), by its API key under its `base_url`, over the board's HTTP API ([`client`]).

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod board;
mod client;
#[cfg(test)]
mod manifest;
mod reads;
mod rfc3339;
mod wire;

use std::io::{BufRead, Write};

use crate::wire::Answer;

fn main() {
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("afkd-board: stdin is unreadable: {e}");
                std::process::exit(2);
            }
        };
        let request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("afkd-board: afkd sent a line that is not a request ({e})");
                std::process::exit(2);
            }
        };
        match wire::answer(request) {
            Answer::Reply(reply) => {
                let written = writeln!(stdout, "{reply}").and_then(|()| stdout.flush());
                if let Err(e) = written {
                    eprintln!("afkd-board: stdout is closed: {e}");
                    std::process::exit(2);
                }
            }
            Answer::Fatal(reason) => {
                eprintln!("afkd-board: {reason}");
                std::process::exit(1);
            }
        }
    }
}
