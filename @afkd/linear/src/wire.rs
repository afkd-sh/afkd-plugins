//! The plugin wire, as this plugin speaks it: the requests afkd sends, the replies
//! written back, the handles they carry, and the budgets every reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"): one
//! newline-delimited JSON request at a time on stdin, one reply line each on stdout.
//! Nothing here is `deny_unknown_fields`, because the wire is additive within a `proto`:
//! a newer afkd's extra field must not break this plugin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::tracker::{Issue, Priority, StateType};

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
    /// A leftover journal key, a `held` finish's, a unit whose `on_claim` failed, or a unit
    /// handed back by a stop. `handed_back` marks the last: afkd gave the unit back between the
    /// `poll` and its run, so nothing ran on it.
    Release {
        key: String,
        #[serde(default)]
        handed_back: bool,
    },
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
    /// One action a slot of afkd's calls, by its own name, and the arguments afkd bound to
    /// its parameters, by name. Answered by every proto 2 plugin, so it is never listed. A
    /// child afkd starts for this one call gets no `hello`: the call carries the `proto` and
    /// the `state_dir` a `hello` would have.
    Call {
        action: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
        #[serde(default)]
        proto: Option<u32>,
        #[serde(default)]
        state_dir: Option<String>,
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
    /// The claimed issue's handle fields, every one the manifest's `Issue` declares.
    pub(crate) fields: IssueFields,
    pub(crate) thread: String,
    pub(crate) seen: Vec<String>,
    #[serde(rename = "self")]
    pub(crate) me: String,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<WireFile>,
}

/// The fields of a `linear.Issue` handle, in the manifest's order, as the issue was read.
/// Each is always sent, so afkd never finds one missing: one that holds nothing is `null`,
/// and a list with nothing in it is empty. A time is unix milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct IssueFields {
    /// Linear's id for the issue, the same after it moves team.
    pub(crate) uuid: String,
    /// The url key of the workspace it is in.
    pub(crate) workspace: String,
    /// Its team's key.
    pub(crate) team: String,
    pub(crate) title: String,
    pub(crate) url: String,
    /// Markdown, `""` when it has none.
    pub(crate) description: String,
    /// Linear's git branch name for it.
    pub(crate) branch: String,
    /// The name of the workflow state it sits in.
    pub(crate) state: String,
    pub(crate) state_type: StateType,
    pub(crate) priority: Priority,
    pub(crate) estimate: Option<i64>,
    /// Its due date, `YYYY-MM-DD`.
    pub(crate) due: Option<String>,
    /// Display names.
    pub(crate) assignee: Option<String>,
    pub(crate) delegate: Option<String>,
    pub(crate) creator: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) milestone: Option<String>,
    /// Its cycle's number.
    pub(crate) cycle: Option<i64>,
    pub(crate) labels: Vec<String>,
    /// The parent issue's identifier.
    pub(crate) parent: Option<String>,
    /// The sub-issues' identifiers, in Linear's order.
    pub(crate) sub_issues: Vec<String>,
    /// Identifiers, one list per relation, done or not.
    pub(crate) blocks: Vec<String>,
    pub(crate) blocked_by: Vec<String>,
    pub(crate) related: Vec<String>,
    pub(crate) similar: Vec<String>,
    pub(crate) duplicates: Vec<String>,
    pub(crate) duplicate_of: Option<String>,
    /// The urls of its links, Linear's attachments.
    pub(crate) links: Vec<String>,
    /// Display names.
    pub(crate) subscribers: Vec<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) completed_at: Option<u64>,
    pub(crate) canceled_at: Option<u64>,
    pub(crate) archived: bool,
    pub(crate) trashed: bool,
}

impl IssueFields {
    /// The handle's fields of `issue`, read in the workspace whose url key is `workspace`: the
    /// one builder a claimed issue and an issue a read hands back both go through, so the two
    /// carry the same fields of the same issue.
    pub(crate) fn of(issue: &Issue, workspace: &str) -> Self {
        let detail = &issue.detail;
        IssueFields {
            uuid: issue.id.clone(),
            workspace: workspace.to_string(),
            team: detail.team.clone(),
            title: issue.title.clone(),
            url: issue.url.clone(),
            description: issue.description.clone(),
            branch: detail.branch.clone(),
            state: issue.state.name.clone(),
            state_type: issue.state.kind,
            priority: detail.priority,
            estimate: detail.estimate,
            due: detail.due.clone(),
            assignee: detail.assignee_name.clone(),
            delegate: detail.delegate_name.clone(),
            creator: detail.creator.clone(),
            project: detail.project.clone(),
            milestone: detail.milestone.clone(),
            cycle: detail.cycle,
            labels: issue.labels.clone(),
            parent: issue.parent.as_ref().map(|p| p.identifier.clone()),
            sub_issues: issue
                .children
                .iter()
                .map(|c| c.identifier.clone())
                .collect(),
            blocks: detail.blocks.clone(),
            blocked_by: issue
                .blockers
                .iter()
                .map(|b| b.identifier.clone())
                .collect(),
            related: detail.related.clone(),
            similar: detail.similar.clone(),
            duplicates: detail.duplicates.clone(),
            duplicate_of: detail.duplicate_of.clone(),
            links: detail.links.clone(),
            subscribers: detail.subscribers.clone(),
            created_at: detail.created_at,
            updated_at: detail.updated_at,
            completed_at: detail.completed_at,
            canceled_at: detail.canceled_at,
            archived: detail.archived,
            trashed: detail.trashed,
        }
    }
}

/// A handle as a read's reply carries it, every one but the claimed issue: its `id`, its key
/// `linear:<organization uuid>/<object uuid>`, and its fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireHandle<F> {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) fields: F,
}

/// The fields of a `linear.Comment` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CommentFields {
    /// The identifier of the issue it is on.
    pub(crate) issue: String,
    pub(crate) body: String,
    pub(crate) url: String,
    /// Its author's display name; `None` for a comment Linear names no author for.
    pub(crate) author: Option<String>,
    /// The id of the comment it answers.
    pub(crate) reply_to: Option<String>,
    pub(crate) resolved_at: Option<u64>,
    pub(crate) created_at: u64,
    pub(crate) edited_at: Option<u64>,
}

/// The fields of a `linear.Team` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct TeamFields {
    pub(crate) name: String,
}

/// The fields of a `linear.State` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct StateFields {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) kind: StateType,
    /// Its team's key.
    pub(crate) team: String,
}

/// The fields of a `linear.Label` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LabelFields {
    pub(crate) name: String,
    /// The name of the label group it is in.
    pub(crate) group: Option<String>,
    /// Its team's key; `None` for a workspace label.
    pub(crate) team: Option<String>,
}

/// The fields of a `linear.User` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct UserFields {
    /// The display name.
    pub(crate) name: String,
    pub(crate) full_name: String,
    pub(crate) email: String,
    /// Whether the user is an app's.
    pub(crate) app: bool,
}

/// The fields of a `linear.Project` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ProjectFields {
    pub(crate) name: String,
    /// Its teams' keys.
    pub(crate) teams: Vec<String>,
    /// Its milestones' names, in order.
    pub(crate) milestones: Vec<String>,
    pub(crate) url: String,
}

/// The fields of a `linear.Cycle` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CycleFields {
    pub(crate) number: i64,
    pub(crate) name: Option<String>,
    /// Its team's key.
    pub(crate) team: String,
    pub(crate) starts_at: u64,
    pub(crate) ends_at: u64,
    pub(crate) active: bool,
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
    /// Whether the `task.md` brief was cut.
    pub(crate) brief_cut: bool,
}

/// `text`'s first `n` bytes and the trailer saying that `what` was cut there, of how many
/// bytes, and where the whole issue is.
fn cut_text(text: &str, n: usize, what: &str) -> String {
    format!(
        "{}\n\n[afkd-linear: this {what} was cut at {n} of {} bytes to fit afkd's 64 KiB \
         plugin line — read the whole issue in Linear]",
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
/// it can be, and only then the unit's `task.md` text too.
///
/// Each cut keeps the longest prefix, at a char boundary, that still fits beside a trailer
/// saying what was cut and where the whole issue is. `None` when not even both trailers
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
    use crate::tracker::{Blocker, Child, IssueDetail, Related, State};

    /// A linear unit as the plugin hands one over: the identifier as the id, the issue and
    /// claim comment uuids as the key, the five env names a run reads, and the issue's
    /// description `description` in its handle's fields beside the brief `text`.
    fn unit_with(description: &str, text: &str) -> WireUnit {
        WireUnit {
            id: "ENG-421".into(),
            key: "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5#5e2a9c71-0d4b-4f3e-9a8c-7b6d5e4f3a21".into(),
            fields: IssueFields::of(&issue(description), "acme"),
            thread: "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5".into(),
            seen: vec!["c3a1e8f0-6b2d-4e7a-9f1c-2d3e4f5a6b7c".into()],
            me: "8d7c6b5a-4e3f-4a1b-9c8d-7e6f5a4b3c2d".into(),
            env: BTreeMap::from([
                (
                    "LINEAR_API_URL".to_string(),
                    "https://api.linear.app/graphql".to_string(),
                ),
                ("LINEAR_AUTHORIZATION".to_string(), "lin_api_k".to_string()),
                (
                    "LINEAR_ISSUE_ID".to_string(),
                    "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5".to_string(),
                ),
                ("LINEAR_ISSUE".to_string(), "ENG-421".to_string()),
                (
                    "LINEAR_TEAM_ID".to_string(),
                    "2b3c4d5e-6f70-4812-93a4-b5c6d7e8f901".to_string(),
                ),
            ]),
            files: vec![WireFile {
                path: crate::TASK_FILE.into(),
                text: text.into(),
            }],
        }
    }

    /// [`unit_with`] an issue with no description.
    fn unit(text: &str) -> WireUnit {
        unit_with("", text)
    }

    /// `ENG-421`, a sub-issue of `ENG-400` with one sub-issue of its own, described
    /// `description`.
    fn issue(description: &str) -> Issue {
        Issue {
            id: "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5".into(),
            identifier: "ENG-421".into(),
            title: "修复 the retry storm 🚨".into(),
            description: description.into(),
            url: "https://linear.app/acme/issue/ENG-421/retry-storm".into(),
            state: State {
                id: "s-todo".into(),
                name: "Todo".into(),
                kind: StateType::Unstarted,
                position: 1.0,
            },
            labels: vec!["afkd/ready".into(), "Väntar på svar".into()],
            assignee: None,
            delegate: None,
            parent: Some(Related {
                identifier: "ENG-400".into(),
                title: "Epic".into(),
            }),
            children: vec![Child {
                id: "c-422".into(),
                identifier: "ENG-422".into(),
                title: "子任务".into(),
                state: "Done".into(),
            }],
            blockers: vec![Blocker {
                identifier: "ENG-399".into(),
                state_kind: "completed".into(),
            }],
            sort_order: 1000.0,
            detail: IssueDetail {
                team: "ENG".into(),
                branch: "eng-421-retry-storm".into(),
                created_at: 1_793_000_000_000,
                updated_at: 1_793_000_500_000,
                ..IssueDetail::default()
            },
        }
    }

    /// afkd's own request shapes decode — every optional call this plugin answers among
    /// them — and an extra field from a later afkd is ignored.
    #[test]
    fn afkd_requests_decode() {
        let finish: Request = serde_json::from_str(
            r#"{"call":"finish","id":"ENG-421","key":"c#m","outcome":"park",
                "facts":{"signal":"fault","reason":"parked: awaiting a human reply",
                         "duration_ms":168000,"cost":0.4217,"turns":null,"tokens":null,
                         "run_name":"260925-100400-unit-ENG-421-1"},"later":1}"#,
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

    /// A `call` decodes with its action's name and the arguments afkd bound, each as it was
    /// sent, a field it does not know aside; one that leaves `args` out has none. A trigger
    /// child's call carries no `proto` and no `state_dir`, and a child for one call's carries
    /// both.
    #[test]
    fn a_call_decodes_with_its_action() {
        let call: Request = serde_json::from_str(
            r#"{"call":"call","action":"comment",
                "args":{"issue":{"id":"ENG-421","key":"c#m"},"text":"Väntar på svar 🚨\n\n- ok"},
                "later":1}"#,
        )
        .unwrap();
        let Request::Call {
            action,
            args,
            proto,
            state_dir,
        } = call
        else {
            panic!("not a call: {call:?}");
        };
        assert_eq!(action, "comment");
        assert_eq!(
            serde_json::Value::Object(args),
            serde_json::json!({"issue": {"id": "ENG-421", "key": "c#m"},
                               "text": "Väntar på svar 🚨\n\n- ok"})
        );
        assert_eq!((proto, state_dir), (None, None));

        let bare: Request = serde_json::from_str(r#"{"call":"call","action":"archive"}"#).unwrap();
        assert!(matches!(
            bare,
            Request::Call { action, args, .. } if action == "archive" && args.is_empty()
        ));

        let once: Request = serde_json::from_str(
            r#"{"call":"call","proto":2,"action":"teams","args":{"ws":{"api_key":"k"}},
                "settings":{},"state_dir":"/srv/afkd/plugin-state/@afkd/linear 監視"}"#,
        )
        .unwrap();
        assert!(matches!(
            once,
            Request::Call { action, proto: Some(2), state_dir: Some(dir), .. }
                if action == "teams" && dir.ends_with("@afkd/linear 監視")
        ));
    }

    /// The `hello` afkd writes: the identity and the state directory it carries decode, and
    /// one without them (an older afkd) still decodes, to the empty identity the arm refuses
    /// by name and no state directory.
    #[test]
    fn a_hello_decodes_with_and_without_the_identity() {
        let hello: Request = serde_json::from_str(
            r#"{"call":"hello","proto":2,"kind":"issue","service":"afkd::develop",
                "roster":["afkd::develop","afkd::discuss"],"owner":"afkd-4242",
                "settings":{"team":"ENG"},
                "state_dir":"/home/you/.local/state/afkd/plugin-state/@afkd/linear"}"#,
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
        assert_eq!(
            state_dir.as_deref(),
            Some("/home/you/.local/state/afkd/plugin-state/@afkd/linear")
        );

        let bare: Request =
            serde_json::from_str(r#"{"call":"hello","proto":2,"kind":"issue","settings":{}}"#)
                .unwrap();
        assert!(matches!(
            bare,
            Request::Hello { service, roster, owner, state_dir: None, .. }
                if service.is_empty() && roster.is_empty() && owner.is_empty()
        ));
    }

    /// An issue's handle fields carry what the issue holds — its family, its blockers, its
    /// detail and the workspace it was read in — and every field the issue lacks is `null`
    /// or an empty list, never missing.
    #[test]
    fn an_issues_fields_are_read_off_the_issue_whole() {
        let fields =
            serde_json::to_value(IssueFields::of(&issue("## 背景\n\nIt resets."), "acme")).unwrap();
        assert_eq!(
            fields,
            serde_json::json!({
                "uuid": "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5", "workspace": "acme", "team": "ENG",
                "title": "修复 the retry storm 🚨",
                "url": "https://linear.app/acme/issue/ENG-421/retry-storm",
                "description": "## 背景\n\nIt resets.", "branch": "eng-421-retry-storm",
                "state": "Todo", "state_type": "unstarted", "priority": "none",
                "estimate": null, "due": null, "assignee": null, "delegate": null,
                "creator": null, "project": null, "milestone": null, "cycle": null,
                "labels": ["afkd/ready", "Väntar på svar"], "parent": "ENG-400",
                "sub_issues": ["ENG-422"], "blocks": [], "blocked_by": ["ENG-399"],
                "related": [], "similar": [], "duplicates": [], "duplicate_of": null,
                "links": [], "subscribers": [],
                "created_at": 1_793_000_000_000_u64, "updated_at": 1_793_000_500_000_u64,
                "completed_at": null, "canceled_at": null, "archived": false, "trashed": false,
            })
        );
    }

    #[test]
    fn a_reply_that_fits_is_left_alone() {
        let u = unit("修复 the retry storm 🚨\n\nIt retries forever.");
        assert_eq!(
            fit_poll(&u),
            Some(Fitted {
                line: fire_line(&u),
                description_cut: false,
                brief_cut: false,
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
        let fitted = fit_poll(&u).expect("the cut fits");
        assert!(fitted.brief_cut && !fitted.description_cut, "{fitted:?}");
        let line = fitted.line;
        assert!(line.len() <= MAX_REPLY, "{}", line.len());
        assert!(
            line.len() > MAX_REPLY - 8,
            "the cut wastes budget: {}",
            line.len()
        );
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        let text = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("read the whole issue in Linear]"),
            "{}",
            &text[text.len() - 200..]
        );
        assert!(
            text.contains(&format!("of {} bytes", body.len())),
            "names the whole size"
        );
        assert!(body.starts_with(text.split("\n\n[afkd-linear:").next().unwrap()));
        // Everything but the brief is untouched.
        assert_eq!(
            reply["unit"]["key"],
            "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5#5e2a9c71-0d4b-4f3e-9a8c-7b6d5e4f3a21"
        );
        assert_eq!(reply["unit"]["env"]["LINEAR_AUTHORIZATION"], "lin_api_k");
        assert_eq!(reply["unit"]["fields"]["parent"], "ENG-400");
    }

    /// A description that, carried twice — the handle's field and the brief — overflows the
    /// line is cut in the field first, and the brief is kept whole when that is enough: a
    /// 40 KB one. A 104 KB one, which the brief alone overflows, is cut to its trailer in the
    /// field and the brief is cut too. Either line fits and decodes, and each cut text keeps
    /// a prefix of what it cut and names its whole size.
    #[test]
    fn an_oversized_description_is_cut_in_the_field_before_the_brief() {
        let line = "看起来不对 🚨 — the retry path resets on every 401.\n";
        for (size, brief_cut) in [(40_000, false), (104_000, true)] {
            let description = line.repeat(size / line.len() + 1);
            let brief = format!("修复 the retry storm 🚨\n\n{description}");
            let fitted = fit_poll(&unit_with(&description, &brief)).expect("the cut fits");
            assert!(fitted.description_cut, "{size}");
            assert_eq!(fitted.brief_cut, brief_cut, "{size}");
            assert!(
                fitted.line.len() <= MAX_REPLY,
                "{size}: {}",
                fitted.line.len()
            );
            let reply: serde_json::Value = serde_json::from_str(&fitted.line).unwrap();
            let field = reply["unit"]["fields"]["description"].as_str().unwrap();
            assert!(
                field.ends_with(&format!(
                    "this description was cut at {} of {} bytes to fit afkd's 64 KiB plugin \
                     line — read the whole issue in Linear]",
                    field.split("\n\n[afkd-linear:").next().unwrap().len(),
                    description.len()
                )),
                "{size}: {}",
                &field[field.len().saturating_sub(200)..]
            );
            assert!(description.starts_with(field.split("\n\n[afkd-linear:").next().unwrap()));
            let text = reply["unit"]["files"][0]["text"].as_str().unwrap();
            if brief_cut {
                assert!(
                    text.contains("this brief was cut at")
                        && text.contains(&format!("of {} bytes", brief.len())),
                    "{size}"
                );
                assert!(field.starts_with("\n\n[afkd-linear: this description was cut at 0 of"));
            } else {
                assert_eq!(text, brief, "{size}: the brief is whole");
            }
        }
    }

    /// A unit whose fields besides the brief overflow the line cannot be handed over at
    /// all, and says so rather than cutting the brief to nothing and still overflowing.
    #[test]
    fn a_unit_over_the_line_without_its_brief_does_not_fit() {
        let mut u = unit("short");
        u.seen = (0..4_000)
            .map(|i| format!("c3a1e8f0-6b2d-4e7a-9f1c-2d3e4f5a{i:04}"))
            .collect();
        assert_eq!(fit_poll(&u), None);
    }

    #[test]
    fn a_comments_reply_keeps_the_newest_that_fit() {
        let comments: Vec<WireComment> = (0..400)
            .map(|i| WireComment {
                id: i.to_string(),
                author: "8d7c6b5a-4e3f-4a1b-9c8d-7e6f5a4b3c2d".into(),
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
