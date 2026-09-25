//! The plugin wire, as this plugin speaks it: the requests afkd sends, the replies
//! written back, and the one budget every reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"): one
//! newline-delimited JSON request at a time on stdin, one reply line each on stdout.
//! Nothing here is `deny_unknown_fields`, because the wire is additive within a `proto`:
//! a newer afkd's extra field must not break this plugin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The plugin protocol version this plugin speaks.
pub(crate) const PROTO: u32 = 1;

/// The longest reply line afkd reads, **before** its newline: afkd caps a line at 64 KiB
/// including the `\n`, and a line of exactly 64 KiB not ending in one is over the cap.
pub(crate) const MAX_REPLY: usize = 64 * 1024 - 1;

/// One request, internally tagged on `call`.
#[derive(Debug, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Request {
    /// Sent once, when the service arms: the protocol, the kind, the settings block.
    Hello {
        proto: u32,
        kind: String,
        settings: serde_json::Value,
    },
    /// Sent once a beat.
    Poll,
    /// Sent after a unit's run ends.
    Finish {
        key: String,
        outcome: UnitOutcome,
        facts: Facts,
    },
    /// A leftover journal key, or a unit handed back by a stop.
    Release { key: String },
    /// A live claim's periodic renewal, counting from 1.
    Renew { key: String, renewal: u64 },
    /// The mid-run watch's read of a unit's comments.
    Comments { key: String },
    /// The attempt's verdict, with afkd's own and the attempt's scratch directory.
    Classify {
        scratch: String,
        outcome: UnitOutcome,
    },
    /// A call this plugin does not know — `attempt_failed`, which neither kind lists, or
    /// one from a later afkd.
    #[serde(other)]
    Unknown,
}

impl Request {
    /// The name `hello` lists this call under, when it is one a kind may or may not
    /// answer; `None` for the calls every kind answers, and for [`Unknown`](Self::Unknown).
    pub(crate) fn optional_call(&self) -> Option<&'static str> {
        match self {
            Request::Release { .. } => Some("release"),
            Request::Renew { .. } => Some("renew"),
            Request::Comments { .. } => Some("comments"),
            Request::Classify { .. } => Some("classify"),
            Request::Hello { .. } | Request::Poll | Request::Finish { .. } | Request::Unknown => {
                None
            }
        }
    }
}

/// A unit's terminal disposition, as `finish` and `classify` spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UnitOutcome {
    /// An attempt completed cleanly (`on_done`).
    Clean,
    /// An attempt asked for human input (the park, then `on_park`).
    Park,
    /// Every attempt faulted (`on_fail`).
    Failed,
}

/// The `finish` envelope's `facts`: what the run did. Only the fields a lifecycle comment
/// or the backstop reads are decoded; the rest (`tokens`) is ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Facts {
    /// The run's terminal control-flow word: `proceed`, `break` or `fault`.
    pub(crate) signal: String,
    /// A fault's own sentence; `null` otherwise.
    #[serde(default)]
    pub(crate) reason: Option<String>,
    /// The fire's wall-clock span in milliseconds.
    pub(crate) duration_ms: u64,
    /// The run's summed agent cost in USD.
    pub(crate) cost: f64,
    /// The agent turns, when every envelope confirmed a count.
    #[serde(default)]
    pub(crate) turns: Option<u32>,
    /// The run directory afkd minted, when it got that far.
    #[serde(default)]
    pub(crate) run_name: Option<String>,
}

impl Facts {
    /// The neutral facts of a moment with no run behind it (`on_claim`, the re-claim's
    /// awaiting-label drop) — afkd's `RunFacts::none`.
    pub(crate) fn none() -> Self {
        Self {
            signal: "proceed".to_string(),
            reason: None,
            duration_ms: 0,
            cost: 0.0,
            turns: None,
            run_name: None,
        }
    }

    /// The fault's sentence, when the run faulted.
    pub(crate) fn fault(&self) -> Option<&str> {
        match self.signal.as_str() {
            "fault" => Some(self.reason.as_deref().unwrap_or("")),
            _ => None,
        }
    }
}

/// One work item handed over in a `poll` reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireUnit {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) thread: String,
    pub(crate) seen: Vec<String>,
    #[serde(rename = "self")]
    pub(crate) me: String,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<WireFile>,
}

/// One file of the attempt's scratch layout, as data: afkd writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireFile {
    pub(crate) path: String,
    pub(crate) text: String,
}

/// One comment in a `comments` reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireComment {
    pub(crate) id: String,
    pub(crate) author: String,
    pub(crate) author_name: String,
    pub(crate) body: String,
    pub(crate) at: String,
}

/// The `poll` reply that hands `unit` over, serialized.
pub(crate) fn fire_line(unit: &WireUnit) -> String {
    serde_json::json!({ "fire": true, "unit": unit }).to_string()
}

/// The `comments` reply over `comments`, serialized.
pub(crate) fn comments_line(comments: &[WireComment]) -> String {
    serde_json::json!({ "comments": comments }).to_string()
}

/// Fit a `poll` reply under [`MAX_REPLY`] by cutting the unit's `task.md` text, if it has
/// to be cut at all, and return the line.
///
/// The cut keeps the longest prefix — at a char boundary — that still fits beside a
/// trailer saying the brief was cut and where the whole issue is. `None` when not even
/// the trailer alone fits, which only a unit whose *other* fields overflow can reach.
pub(crate) fn fit_poll(unit: &WireUnit) -> Option<String> {
    let line = fire_line(unit);
    if line.len() <= MAX_REPLY {
        return Some(line);
    }
    let brief = unit.files.iter().position(|f| f.path == crate::TASK_FILE)?;
    let text = &unit.files[brief].text;
    let with_prefix = |n: usize| {
        let mut cut = unit.clone();
        cut.files[brief].text = format!(
            "{}\n\n[afkd-gitea: this brief was cut at {n} of {} bytes to fit afkd's 64 KiB \
             plugin line — read the whole issue with the gitea skill]",
            &text[..n],
            text.len()
        );
        fire_line(&cut)
    };
    // The serialized length only grows with the prefix, so the longest prefix that fits
    // is a binary search over byte offsets, each rounded down to a char boundary.
    let floor = |mut n: usize| {
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        n
    };
    if with_prefix(0).len() > MAX_REPLY {
        return None;
    }
    let (mut fits, mut over) = (0, text.len());
    while over - fits > 1 {
        let mid = floor(fits + (over - fits) / 2);
        if mid == fits {
            break;
        }
        if with_prefix(mid).len() <= MAX_REPLY {
            fits = mid;
        } else {
            over = mid;
        }
    }
    Some(with_prefix(fits))
}

/// Fit a `comments` reply under [`MAX_REPLY`] by keeping the **newest** comments that fit,
/// oldest-first as `comments` is ordered. Returns the line and how many of the oldest were
/// left out.
pub(crate) fn fit_comments(comments: &[WireComment]) -> (String, usize) {
    // `{"comments":[` + the elements joined by `,` + `]}`: compact JSON serializes an
    // array element exactly as it serializes alone, so the line's length is additive.
    let frame = comments_line(&[]).len();
    let mut len = frame;
    let mut kept = 0;
    for comment in comments.iter().rev() {
        let size = serde_json::to_string(comment).map_or(usize::MAX, |s| s.len());
        let step = size.saturating_add(usize::from(kept > 0));
        if len.saturating_add(step) > MAX_REPLY {
            break;
        }
        len += step;
        kept += 1;
    }
    let dropped = comments.len() - kept;
    (comments_line(&comments[dropped..]), dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(text: &str) -> WireUnit {
        WireUnit {
            id: "7".into(),
            key: "acme/widgets#7#1000001".into(),
            thread: "acme/widgets#7".into(),
            seen: vec!["41".into(), "42".into()],
            me: "björn-öst[bot]".into(),
            env: BTreeMap::from([("GITEA_ISSUE_NUMBER".to_string(), "7".to_string())]),
            files: vec![
                WireFile {
                    path: crate::TASK_FILE.into(),
                    text: text.into(),
                },
                WireFile {
                    path: "issue/number".into(),
                    text: "7".into(),
                },
            ],
        }
    }

    /// afkd's own request shapes decode, the calls this plugin never lists fall to
    /// `Unknown`, and an extra field from a later afkd is ignored.
    #[test]
    fn afkd_requests_decode() {
        let finish: Request = serde_json::from_str(
            r#"{"call":"finish","id":"7","key":"acme/widgets#7#9","outcome":"park",
                "facts":{"signal":"fault","reason":"parked: awaiting a human reply",
                         "duration_ms":168000,"cost":0.4217,"turns":null,"tokens":null,
                         "run_name":"260925-100400-issue-7-1"},"later":1}"#,
        )
        .unwrap();
        let Request::Finish {
            key,
            outcome,
            facts,
        } = finish
        else {
            panic!("{finish:?}");
        };
        assert_eq!(key, "acme/widgets#7#9");
        assert_eq!(outcome, UnitOutcome::Park);
        assert_eq!(facts.fault(), Some("parked: awaiting a human reply"));
        assert!(matches!(
            serde_json::from_str(r#"{"call":"poll"}"#).unwrap(),
            Request::Poll
        ));
        assert!(matches!(
            serde_json::from_str(
                r#"{"call":"attempt_failed","key":"k","n":1,"max":2,"reason":null}"#
            )
            .unwrap(),
            Request::Unknown
        ));
    }

    #[test]
    fn a_reply_that_fits_is_left_alone() {
        let u = unit("修复 the retry storm 🚨\n\nIt retries forever.");
        assert_eq!(fit_poll(&u), Some(fire_line(&u)));
    }

    /// A brief far over the line, made of multi-byte text so a byte cut would split a
    /// character: the line fits, decodes, ends in the trailer, and keeps as much of the
    /// brief as the budget allows (within one character of the limit).
    #[test]
    fn an_oversized_brief_is_cut_at_a_char_boundary_to_fit() {
        let body = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit(&body);
        let line = fit_poll(&u).expect("the cut fits");
        assert!(line.len() <= MAX_REPLY, "{}", line.len());
        assert!(
            line.len() > MAX_REPLY - 8,
            "the cut wastes budget: {}",
            line.len()
        );
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let text = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("read the whole issue with the gitea skill]"),
            "{}",
            &text[text.len() - 200..]
        );
        assert!(
            text.contains(&format!("of {} bytes", body.len())),
            "names the whole size"
        );
        assert!(body.starts_with(text.split("\n\n[afkd-gitea:").next().unwrap()));
        // Everything but the brief is untouched.
        assert_eq!(reply["unit"]["key"], "acme/widgets#7#1000001");
        assert_eq!(reply["unit"]["files"][1]["text"], "7");
    }

    #[test]
    fn a_comments_reply_keeps_the_newest_that_fit() {
        let comments: Vec<WireComment> = (0..400)
            .map(|i| WireComment {
                id: i.to_string(),
                author: "álvaro".into(),
                author_name: "álvaro".into(),
                body: "x".repeat(300),
                at: "2026-09-25T09:58:00Z".into(),
            })
            .collect();
        let (line, dropped) = fit_comments(&comments);
        assert!(line.len() <= MAX_REPLY);
        assert!(dropped > 0 && dropped < comments.len());
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let kept = reply["comments"].as_array().unwrap();
        assert_eq!(kept.len(), 400 - dropped);
        assert_eq!(kept.last().unwrap()["id"], "399", "the newest survive");
        assert_eq!(kept[0]["id"], dropped.to_string());
        // One more comment would not have fit.
        assert!(comments_line(&comments[dropped - 1..]).len() > MAX_REPLY);
    }
}
