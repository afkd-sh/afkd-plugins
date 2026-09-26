//! `afkd-gitlab`: the `@afkd/gitlab` trigger plugin.
//!
//! One long-lived child per armed service, spoken to by afkd in newline-delimited JSON on
//! stdin and stdout (afkd's `docs/plugins.md`, "The trigger protocol"). stdout carries
//! replies and nothing else; every diagnostic goes to stderr, which afkd streams into the
//! service log under `[@afkd/gitlab:err]`.
//!
//! Two kinds, each the vendor half of one of afkd's built-in GitLab triggers, ported: the
//! `gitlab` kind ([`issue`]) takes open, source-labelled issues, and the
//! `gitlab_mr_review` kind ([`mr`]) takes the bot's open merge requests that carry
//! feedback newer than its last word. Both keep the built-in's claim markers, lifecycle
//! comments, claim-journal keys, session threads, run env and brief, so a claim either one
//! left on a live issue or MR is recognised, renewed and released by the other. `hello`
//! names the kind, and [`plugin`] dispatches on it.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod claim;
mod client;
mod common;
mod http;
mod issue;
mod kind;
mod lifecycle;
#[cfg(test)]
mod manifest;
mod mr;
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

/// The scratch sub-location holding the bare issue iid, so the bundled skill can find
/// which issue a run belongs to.
pub(crate) const ISSUE_DIR: &str = "issue";

/// The scratch sub-location holding the bare MR iid, so the bundled skill can find which
/// merge request a run belongs to.
pub(crate) const MR_DIR: &str = "mr";

/// The file under [`ISSUE_DIR`] or [`MR_DIR`] holding the bare iid.
pub(crate) const NUMBER_FILE: &str = "number";

fn main() {
    let mut plugin = Plugin::new(Box::new(SystemClock), Box::new(StderrDiag));
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("afkd-gitlab: stdin is unreadable: {e}");
                std::process::exit(2);
            }
        };
        let request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("afkd-gitlab: afkd sent a line that is not a request ({e})");
                std::process::exit(2);
            }
        };
        match plugin.answer(request) {
            Answer::Reply(reply) => {
                let written = writeln!(stdout, "{reply}").and_then(|()| stdout.flush());
                if let Err(e) = written {
                    eprintln!("afkd-gitlab: stdout is closed: {e}");
                    std::process::exit(2);
                }
            }
            Answer::Fatal(reason) => {
                eprintln!("afkd-gitlab: {reason}");
                std::process::exit(1);
            }
        }
    }
}
