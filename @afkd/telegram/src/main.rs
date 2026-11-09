//! `afkd-telegram`: the `@afkd/telegram` plugin.
//!
//! One long-lived child per armed service, spoken to by afkd in newline-delimited JSON on
//! stdin and stdout (afkd's `docs/plugins.md`, "The trigger protocol"), or one child for
//! one call made outside a `service(telegram)` ("A child for one call"). stdout carries
//! replies and nothing else; every diagnostic goes to stderr, which afkd streams into the
//! service log under `[@afkd/telegram:err]`.
//!
//! The `message` kind ([`trigger`]) — `service(telegram)` in a config — hands over the
//! messages a bot receives in the chats it lists, one per run, each confirmed to Telegram
//! before it is handed over. The actions ([`action`]) send a text or a file, reply, react,
//! edit, delete and fetch a file. A message received and a message sent are one handle
//! ([`message`]), and every call speaks Telegram's Bot API ([`client`]) through one trait
//! ([`botapi`]).

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod action;
mod botapi;
mod client;
mod common;
#[cfg(test)]
#[path = "../tests/common/form.rs"]
mod form;
mod http;
#[cfg(test)]
mod manifest;
mod message;
mod multipart;
mod plugin;
mod settings;
mod trigger;
mod wire;

use std::io::{BufRead, Write};

use crate::common::{StderrDiag, SystemClock};
use crate::plugin::{Answer, Plugin};

/// The brief at the scratch directory's root, which afkd writes framed.
pub(crate) const TASK_FILE: &str = "task.md";

fn main() {
    let mut plugin = Plugin::new(Box::new(SystemClock), Box::new(StderrDiag));
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("afkd-telegram: stdin is unreadable: {e}");
                std::process::exit(2);
            }
        };
        let request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("afkd-telegram: afkd sent a line that is not a request ({e})");
                std::process::exit(2);
            }
        };
        match plugin.answer(request) {
            Answer::Reply(reply) => {
                let written = writeln!(stdout, "{reply}").and_then(|()| stdout.flush());
                if let Err(e) = written {
                    eprintln!("afkd-telegram: stdout is closed: {e}");
                    std::process::exit(2);
                }
            }
            Answer::Fatal(reason) => {
                eprintln!("afkd-telegram: {reason}");
                std::process::exit(1);
            }
        }
    }
}
