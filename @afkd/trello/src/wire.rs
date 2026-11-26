//! The plugin wire, as this plugin speaks it: the requests afkd sends, the replies
//! written back, and the one budget every reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"): one
//! newline-delimited JSON request at a time on stdin, one reply line each on stdout.
//! Nothing here is `deny_unknown_fields`, because the wire is additive within a `proto`:
//! a newer afkd's extra field must not break this plugin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::handles::CardFields;

/// The plugin protocol version this plugin speaks.
pub(crate) const PROTO: u32 = 2;

/// The longest reply line afkd reads, **before** its newline: afkd caps a line at 64 KiB
/// including the `\n`, and a line of exactly 64 KiB not ending in one is over the cap.
pub(crate) const MAX_REPLY: usize = 64 * 1024 - 1;

/// The longest reply to a `call` afkd reads, before its newline: afkd takes a call's reply
/// up to 8 MiB including the `\n`, so a page of handles with their fields fits.
pub(crate) const MAX_CALL_REPLY: usize = 8 * 1024 * 1024 - 1;

/// One request, internally tagged on `call`.
#[derive(Debug, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Request {
    /// Sent once, when the service arms: the protocol, the kind, the settings block, and
    /// who is asking — the configured service (instance suffix stripped), every service
    /// the daemon runs, and this afkd process's claim owner. The three identity fields
    /// decode to empty when an older afkd leaves them out, so the refusal names them
    /// rather than the line failing to decode. `state_dir` is the plugin's own directory,
    /// which outlives the child.
    Hello {
        proto: u32,
        kind: String,
        settings: serde_json::Value,
        #[serde(default)]
        service: String,
        #[serde(default)]
        roster: Vec<String>,
        #[serde(default)]
        owner: String,
        #[serde(default)]
        state_dir: Option<String>,
    },
    /// Sent once a beat.
    Poll,
    /// Sent after a unit's run ends.
    Finish {
        key: String,
        outcome: UnitOutcome,
        facts: Facts,
    },
    /// A leftover journal key, a `held` finish's, or a unit handed back by a stop.
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
    /// Attempt `n` of `max` faulted, with the fault's own sentence.
    AttemptFailed {
        key: String,
        n: u32,
        max: u32,
        #[serde(default)]
        reason: Option<String>,
    },
    /// One action a slot of afkd's calls, by its own name, its arguments bound by
    /// parameter name — the item it acts on among them, as the handle of a unit this
    /// plugin handed over. Answered by every proto 2 plugin, so it is never listed. A child
    /// afkd starts for this one call gets no `hello`: the call carries the `proto` a `hello`
    /// would have.
    Call {
        action: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
        #[serde(default)]
        proto: Option<u32>,
    },
    /// A call from a later afkd.
    #[serde(other)]
    Unknown,
}

/// A unit's terminal disposition, as `finish` and `classify` spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UnitOutcome {
    /// An attempt completed cleanly.
    Clean,
    /// An attempt asked for human input (the badge and the owner marker).
    Park,
    /// Every attempt faulted.
    Failed,
}

/// The `finish` envelope's `facts`: what the run did. Only the fields the backstop reads
/// are decoded; the rest (`duration_ms`, `cost`, `turns`, `tokens`, `run_name`) is
/// ignored — a comment that names them is afkd's to interpolate.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Facts {
    /// The run's terminal control-flow word: `proceed`, `break` or `fault`.
    pub(crate) signal: String,
    /// A fault's own sentence; `null` otherwise.
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

impl Facts {
    /// The facts of a run that proceeded — afkd's `RunFacts::none`.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            signal: "proceed".to_string(),
            reason: None,
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
    /// The claimed card's handle fields, every one the manifest's `Card` declares.
    pub(crate) fields: CardFields,
    pub(crate) thread: String,
    pub(crate) seen: Vec<String>,
    #[serde(rename = "self")]
    pub(crate) me: String,
    pub(crate) env: BTreeMap<String, String>,
    /// The values every slot of the kind is passed beside the run and the card, by name.
    pub(crate) args: Map<String, Value>,
    pub(crate) files: Vec<WireFile>,
}

/// A handle as a read's reply carries it, every one but the claimed card: its `id`, its key
/// (`trello:<Trello's id>`), and its fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireHandle<F> {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) fields: F,
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

/// A `poll` reply [`fit_poll`] made fit: the line, and what it cut.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fitted {
    pub(crate) line: String,
    /// Whether the handle's `description` field was cut.
    pub(crate) description_cut: bool,
    /// Whether the `task/README.md` brief was cut.
    pub(crate) brief_cut: bool,
}

/// `text`'s first `n` bytes and the trailer saying that `what` was cut there, of how many
/// bytes, and where the whole card is.
fn cut_text(text: &str, n: usize, what: &str) -> String {
    format!(
        "{}\n\n[afkd-trello: this {what} was cut at {n} of {} bytes to fit afkd's 64 KiB \
         plugin line — read the whole card with the trello skill]",
        &text[..n],
        text.len()
    )
}

/// The longest prefix of `text`, in bytes and at a char boundary, whose `line` is no longer
/// than [`MAX_REPLY`], or `None` when not even the empty prefix's is. The line only grows
/// with the prefix, so the search is a binary search over byte offsets, each rounded down to
/// a char boundary.
fn longest_fitting(text: &str, line: impl Fn(usize) -> String) -> Option<usize> {
    let floor = |mut n: usize| {
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        n
    };
    if line(0).len() > MAX_REPLY {
        return None;
    }
    let (mut fits, mut over) = (0, text.len());
    while over - fits > 1 {
        let mid = floor(fits + (over - fits) / 2);
        if mid == fits {
            break;
        }
        if line(mid).len() <= MAX_REPLY {
            fits = mid;
        } else {
            over = mid;
        }
    }
    Some(fits)
}

/// Fit a `poll` reply under [`MAX_REPLY`], and return the line and what was cut: nothing
/// when it fits as it is, else the handle's `description` field, the brief whole beside it if
/// it can be, and only then the unit's `task/README.md` text too.
///
/// Each cut keeps the longest prefix, at a char boundary, that still fits beside a trailer
/// saying what was cut and where the whole card is. `None` when not even both trailers
/// alone fit, which only a unit whose *other* fields overflow can reach.
pub(crate) fn fit_poll(unit: &WireUnit) -> Option<Fitted> {
    let line = fire_line(unit);
    if line.len() <= MAX_REPLY {
        return Some(Fitted {
            line,
            description_cut: false,
            brief_cut: false,
        });
    }
    let description = &unit.fields.description;
    let with_description = |n: usize| {
        let mut cut = unit.clone();
        cut.fields.description = cut_text(description, n, "description");
        cut
    };
    let mut unit = unit.clone();
    let mut description_cut = false;
    if !description.is_empty() {
        if let Some(n) = longest_fitting(description, |n| fire_line(&with_description(n))) {
            return Some(Fitted {
                line: fire_line(&with_description(n)),
                description_cut: true,
                brief_cut: false,
            });
        }
        // Not even the description's trailer alone fits beside the brief: keep the shorter of
        // the two, and cut the brief.
        let bare = with_description(0);
        if bare.fields.description.len() < description.len() {
            unit = bare;
            description_cut = true;
        }
    }
    let brief = unit.files.iter().position(|f| f.path == crate::TASK_FILE)?;
    let text = unit.files[brief].text.clone();
    let with_brief = |n: usize| {
        let mut cut = unit.clone();
        cut.files[brief].text = cut_text(&text, n, "brief");
        fire_line(&cut)
    };
    let n = longest_fitting(&text, with_brief)?;
    Some(Fitted {
        line: with_brief(n),
        description_cut,
        brief_cut: true,
    })
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

    /// A trello unit as the plugin hands one over: the short link as the id, the card's
    /// ObjectId as the key, the four env names the skill reads.
    fn unit(text: &str) -> WireUnit {
        unit_with("It retries forever.", text)
    }

    /// The same, the card's description `description` in its handle's fields beside the
    /// brief `text`.
    fn unit_with(description: &str, text: &str) -> WireUnit {
        WireUnit {
            id: "1Rkelydw".into(),
            key: "trello:6a4dd5de1234abcd5678ef90".into(),
            fields: CardFields {
                number: 12,
                name: "修复 the retry storm 🚨".into(),
                url: "https://trello.com/c/1Rkelydw/12-retry-storm".into(),
                description: description.into(),
                board: "BID".into(),
                list: "Up for Grabs".into(),
                labels: vec!["afkd/ready".into(), "Väntar på svar".into()],
                members: vec!["phil".into()],
                due: None,
                due_complete: false,
                archived: false,
                created_at: 1_793_607_330_000,
                last_activity_at: 1_793_607_400_000,
            },
            thread: "1Rkelydw".into(),
            seen: vec!["6a4dd5e01234abcd5678ef92".into()],
            me: "5f4d1c2b9a0000000000cc03".into(),
            env: BTreeMap::from([
                ("TRELLO_API_KEY".to_string(), "k".to_string()),
                ("TRELLO_BOARD_ID".to_string(), "BID".to_string()),
                (
                    "TRELLO_CARD_ID".to_string(),
                    "6a4dd5de1234abcd5678ef90".to_string(),
                ),
                ("TRELLO_TOKEN".to_string(), "t".to_string()),
            ]),
            args: Map::from_iter([(
                "account".to_string(),
                serde_json::json!({
                    "api_key": "k",
                    "token": "t",
                    "base_url": "https://api.trello.com/1",
                }),
            )]),
            files: vec![WireFile {
                path: crate::TASK_FILE.into(),
                text: text.into(),
            }],
        }
    }

    /// afkd's own request shapes decode — every optional call this plugin answers among
    /// them — and an extra field from a later afkd is ignored.
    #[test]
    fn afkd_requests_decode() {
        let finish: Request = serde_json::from_str(
            r#"{"call":"finish","id":"1Rkelydw","key":"c#m","outcome":"park",
                "facts":{"signal":"fault","reason":"parked: awaiting a human reply",
                         "duration_ms":168000,"cost":0.4217,"turns":null,"tokens":null,
                         "run_name":"260925-100400-unit-1Rkelydw-1"},"later":1}"#,
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
        assert_eq!(key, "c#m");
        assert_eq!(outcome, UnitOutcome::Park);
        assert_eq!(
            facts,
            Facts {
                signal: "fault".into(),
                reason: Some("parked: awaiting a human reply".into()),
            }
        );
        assert_eq!(facts.fault(), Some("parked: awaiting a human reply"));
        assert_eq!(Facts::none().fault(), None);

        let classify: Request = serde_json::from_str(
            r#"{"call":"classify","key":"c#m","scratch":"/tmp/s 監視","outcome":"failed"}"#,
        )
        .unwrap();
        assert!(matches!(
            &classify,
            Request::Classify { scratch, outcome: UnitOutcome::Failed } if scratch == "/tmp/s 監視"
        ));
        let failed: Request = serde_json::from_str(
            r#"{"call":"attempt_failed","key":"c#m","n":1,"max":2,"reason":null}"#,
        )
        .unwrap();
        assert!(matches!(
            &failed,
            Request::AttemptFailed { key, n: 1, max: 2, reason: None } if key == "c#m"
        ));
    }

    /// A `call` decodes with its arguments as afkd bound them, the card's handle among
    /// them; a top-level `key` an older afkd sent is not read, and a call that leaves
    /// `args` out still decodes, for the plugin to refuse by name.
    #[test]
    fn a_call_decodes_with_its_args() {
        let call: Request = serde_json::from_str(
            r#"{"call":"call","action":"move_to",
                "args":{"card":{"id":"1Rkelydw","key":"c#m"},"list":"Blockerat / Väntar",
                        "at":"bottom"},
                "key":"other#claim","later":1}"#,
        )
        .unwrap();
        let Request::Call {
            action,
            args,
            proto,
        } = call
        else {
            panic!("{call:?}");
        };
        assert_eq!(action, "move_to");
        assert_eq!(args["card"]["key"], "c#m");
        assert_eq!(args["list"], "Blockerat / Väntar");
        assert_eq!(args["at"], "bottom");
        assert_eq!(proto, None, "a trigger child's call names no protocol");
        assert!(
            !args.contains_key("key"),
            "the stray key is not an argument"
        );

        let bare: Request = serde_json::from_str(r#"{"call":"call","action":"archive"}"#).unwrap();
        assert!(matches!(
            bare,
            Request::Call { action, args, proto: None } if action == "archive" && args.is_empty()
        ));
    }

    /// The `call` a child afkd starts for one call gets, with no `hello` before it: it
    /// names the `proto` a `hello` would have, and a line from a later afkd with more
    /// fields still decodes.
    #[test]
    fn a_one_shot_call_decodes_with_its_proto() {
        let call: Request = serde_json::from_str(
            r#"{"call":"call","proto":2,"action":"cards",
                "args":{"acct":{"api_key":"k","token":"t","base_url":"http://127.0.0.1:1/1"},
                        "board":"https://trello.com/b/BID/x","limit":5},
                "state_dir":"/srv/afkd/plugin-state/@afkd/trello","later":1}"#,
        )
        .unwrap();
        let Request::Call {
            action,
            args,
            proto,
        } = call
        else {
            panic!("{call:?}");
        };
        assert_eq!((action.as_str(), proto), ("cards", Some(2)));
        assert_eq!(args["acct"]["token"], "t");
        assert_eq!(args["limit"], 5);
    }

    /// The `hello` afkd writes: the identity it carries decodes, and one without it (an
    /// older afkd) still decodes, to the empty identity the arm refuses by name.
    #[test]
    fn a_hello_decodes_with_and_without_the_identity() {
        let hello: Request = serde_json::from_str(
            r#"{"call":"hello","proto":2,"kind":"card","service":"afkd::develop",
                "roster":["afkd::develop","afkd::discuss"],"owner":"afkd-4242",
                "settings":{"board":"https://trello.com/b/BID/x"}}"#,
        )
        .unwrap();
        let Request::Hello {
            service,
            roster,
            owner,
            state_dir,
            ..
        } = hello
        else {
            panic!("{hello:?}");
        };
        assert_eq!(service, "afkd::develop");
        assert_eq!(roster, ["afkd::develop", "afkd::discuss"]);
        assert_eq!(owner, "afkd-4242");
        assert_eq!(state_dir, None, "an afkd that names no directory");

        let bare: Request =
            serde_json::from_str(r#"{"call":"hello","proto":2,"kind":"card","settings":{}}"#)
                .unwrap();
        assert!(matches!(
            bare,
            Request::Hello { service, roster, owner, state_dir: None, .. }
                if service.is_empty() && roster.is_empty() && owner.is_empty()
        ));
    }

    /// A `hello` that names the plugin's own directory decodes it, wide characters and all.
    #[test]
    fn a_hello_decodes_the_state_dir_it_names() {
        let hello: Request = serde_json::from_str(
            r#"{"call":"hello","proto":2,"kind":"card","settings":{},"service":"afkd::develop",
                "roster":[],"owner":"afkd-4242",
                "state_dir":"/home/you/.local/state/afkd/plugin-state/@afkd/trello 監視"}"#,
        )
        .unwrap();
        assert!(matches!(
            hello,
            Request::Hello { state_dir: Some(dir), .. }
                if dir == "/home/you/.local/state/afkd/plugin-state/@afkd/trello 監視"
        ));
    }

    #[test]
    fn a_reply_that_fits_is_left_alone() {
        let u = unit("修复 the retry storm 🚨\n\nIt retries forever.");
        assert_eq!(
            fit_poll(&u),
            Some(Fitted {
                line: fire_line(&u),
                description_cut: false,
                brief_cut: false
            })
        );
    }

    /// A brief far over the line, made of multi-byte text so a byte cut would split a
    /// character: the line fits, decodes, ends in the trailer, and keeps as much of the
    /// brief as the budget allows (within one character of the limit).
    #[test]
    fn an_oversized_brief_is_cut_at_a_char_boundary_to_fit() {
        let body = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit(&body);
        let Fitted {
            line,
            description_cut,
            brief_cut,
        } = fit_poll(&u).expect("the cut fits");
        assert!((description_cut, brief_cut) == (false, true));
        assert!(line.len() <= MAX_REPLY, "{}", line.len());
        assert!(
            line.len() > MAX_REPLY - 8,
            "the cut wastes budget: {}",
            line.len()
        );
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let text = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("read the whole card with the trello skill]"),
            "{}",
            &text[text.len() - 200..]
        );
        assert!(
            text.contains(&format!("of {} bytes", body.len())),
            "names the whole size"
        );
        assert!(body.starts_with(text.split("\n\n[afkd-trello:").next().unwrap()));
        // Everything but the brief is untouched.
        assert_eq!(reply["unit"]["key"], "trello:6a4dd5de1234abcd5678ef90");
        assert_eq!(reply["unit"]["env"]["TRELLO_TOKEN"], "t");
        assert_eq!(
            reply["unit"]["fields"]["description"],
            "It retries forever."
        );
        assert_eq!(
            reply["unit"]["args"],
            serde_json::to_value(&u.args).unwrap(),
            "the account survives the cut"
        );
        assert_eq!(reply["unit"]["args"]["account"]["token"], "t");
    }

    /// A description over the line beside a short brief is the field cut, at a char
    /// boundary and with its trailer, and the brief is left whole.
    #[test]
    fn an_oversized_description_is_cut_and_the_brief_left_whole() {
        let description = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit_with(&description, "short brief");
        let Fitted {
            line,
            description_cut,
            brief_cut,
        } = fit_poll(&u).expect("the cut fits");
        assert!((description_cut, brief_cut) == (true, false));
        assert!(
            line.len() <= MAX_REPLY && line.len() > MAX_REPLY - 8,
            "{}",
            line.len()
        );
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let cut = reply["unit"]["fields"]["description"].as_str().unwrap();
        assert!(cut.contains("this description was cut at"), "{cut}");
        assert!(cut.contains(&format!("of {} bytes", description.len())));
        assert!(description.starts_with(cut.split("\n\n[afkd-trello:").next().unwrap()));
        assert_eq!(reply["unit"]["files"][0]["text"], "short brief");
    }

    /// A description and a brief that both hold the whole card, over the line together: the
    /// description is cut to its trailer, and the brief to what is left.
    #[test]
    fn a_description_repeated_in_the_brief_cuts_both() {
        let description = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit_with(&description, &description);
        let Fitted {
            line,
            description_cut,
            brief_cut,
        } = fit_poll(&u).expect("the cut fits");
        assert!((description_cut, brief_cut) == (true, true));
        assert!(line.len() <= MAX_REPLY, "{}", line.len());
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let cut = reply["unit"]["fields"]["description"].as_str().unwrap();
        assert!(
            cut.starts_with("\n\n[afkd-trello: this description was cut at 0 of"),
            "{cut}"
        );
        assert!(reply["unit"]["files"][0]["text"].as_str().unwrap().len() > 60_000);
    }

    /// A unit whose fields besides the brief overflow the line cannot be handed over at
    /// all, and says so rather than cutting the brief to nothing and still overflowing.
    #[test]
    fn a_unit_over_the_line_without_its_brief_does_not_fit() {
        let mut u = unit("short");
        u.seen = (0..4_000)
            .map(|i| format!("6a4dd5e01234abcd5678{i:04}"))
            .collect();
        assert_eq!(fit_poll(&u), None);
    }

    #[test]
    fn a_comments_reply_keeps_the_newest_that_fit() {
        let comments: Vec<WireComment> = (0..400)
            .map(|i| WireComment {
                id: i.to_string(),
                author: "5f4d1c2b9a0000000000aa01".into(),
                author_name: "Álvaro Pérez".into(),
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
