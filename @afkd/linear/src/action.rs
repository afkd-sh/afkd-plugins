//! The **action vocabulary** afkd's slots call: what a slot may do to the issue it is
//! passed, the decode of one `call` request into that and the issue it acts on, and the
//! doing of it.
//!
//! afkd runs the slots (`on_claim`, `on_run`, `on_done`, `on_fail`, `on_park`) as code, and
//! each plugin action a slot calls — `linear.move_to(issue, "Done")` — crosses as one
//! `call`, its arguments already bound and typed against the manifest and any `#{…}` in a
//! text already interpolated. The issue is the handle the action takes first, never an
//! implied current one.
//!
//! A write — a comment, a new title or description, a move, a label, a new issue, an
//! archive — goes the way the kind's own writes go: in order behind the issue's earlier
//! ones, and retried by the [`Outbox`](crate::outbox::Outbox) when Linear cannot take it.
//! A read cannot hand a value back, since a plugin exposes none, so it writes what it read
//! to the absolute path `to`: now or refused, never queued.
//!
//! A comment or a description can carry files: `files` names them by absolute path, each is
//! read when the slot calls and put up when the write lands, and the text goes out with each
//! embedded ([`crate::file`]). `fetch_files` writes the files an issue carries back.

use std::path::Path;

use serde_json::{Map, Value};

use crate::common::Diag;
use crate::file::{self, Upload};
use crate::issue::{is_control_marker, render_comments, sub_issue_line, SelfIds, ACTION_TAG};
use crate::outbox::named;
use crate::tracker::{state_named, Issue, LinearError, Team, Tracker};

/// The parameter every action takes first: the issue it acts on, as a `linear.Issue`
/// handle.
pub(crate) const HANDLE: &str = "issue";

/// The actions afkd may `call`, in the order the manifest declares them: what
/// `manifest.rs`'s test holds `afkd-plugin.toml` to, and [`from_call`] decodes.
pub(crate) const ACTIONS: &[&str] = &[
    "comment",
    "describe",
    "rename",
    "move_to",
    "add_label",
    "remove_label",
    "create_issue",
    "create_sub_issue",
    "archive",
    "list_comments",
    "read_description",
    "list_sub_issues",
    "move_sub_issue",
    "fetch_files",
];

/// An issue to file: its title, its description (`""` for none), and the name of the state
/// it starts in, `None` for the team's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewIssue {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) state: Option<String>,
}

/// A write a slot asks for on the issue it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssueAction {
    /// Post a comment, its text verbatim and each file embedded after it.
    Comment { text: String, files: Vec<Upload> },
    /// Replace the description, its text verbatim and each file embedded after it.
    Describe { text: String, files: Vec<Upload> },
    /// Retitle the issue.
    Rename(String),
    /// Move the issue to the team's state of this name.
    MoveTo { state: String },
    /// Put the label of this name on: the team's, else the workspace's, else a new one for
    /// the team.
    AddLabel { name: String },
    /// Take the label of this name off; a name with no label is a no-op.
    RemoveLabel { name: String },
    /// File an issue in the service's team.
    CreateIssue(NewIssue),
    /// File an issue under this one, in the service's team.
    CreateSubIssue(NewIssue),
    /// Archive the issue.
    Archive,
    /// Move this issue's sub-issue of this identifier to the team's state of this name.
    MoveSubIssue { sub_issue: String, state: String },
}

/// What a read writes to its `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    /// The thread, oldest first, as the brief renders it.
    Comments,
    /// The description as Linear holds it now.
    Description,
    /// The sub-issues, a line each, as the brief lists them.
    SubIssues,
}

impl Read {
    /// The action that asks for it.
    fn action(self) -> &'static str {
        match self {
            Read::Comments => "list_comments",
            Read::Description => "read_description",
            Read::SubIssues => "list_sub_issues",
        }
    }
}

/// Where a text with files goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Post {
    /// A new comment.
    Comment,
    /// The description, replaced.
    Description,
}

impl Post {
    /// The action that asks for it.
    fn action(self) -> &'static str {
        match self {
            Post::Comment => "comment",
            Post::Description => "describe",
        }
    }
}

/// What one decoded `call` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// A write to the issue, delivered — or queued — like every other.
    Act(IssueAction),
    /// A comment or a description, with the files at the absolute `paths` in it: a write
    /// once [`posted`] has read them.
    Post {
        into: Post,
        text: String,
        paths: Vec<String>,
    },
    /// A read of the issue, written to the absolute path `to`.
    Read { read: Read, to: String },
    /// The files the issue carries, written into the directory `to`.
    FetchFiles { to: String },
}

/// One decoded `call`: the issue it acts on, by the key its handle carries, and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// afkd's claim key for the issue, the handle's `key`.
    pub(crate) key: String,
    /// What to do to it.
    pub(crate) op: Op,
}

/// Decode one `call` — the action's name and its arguments by parameter name — into the
/// issue it acts on and the action it asks for, or the sentence that refuses it.
///
/// afkd binds every argument against the manifest before it sends the call, so a missing or
/// mistyped argument only comes from a hand-written wire; it is refused rather than guessed
/// at. The issue is read off its handle's `key` alone, the key this plugin handed it over
/// under. An argument the action has no parameter for is ignored, since the wire is
/// additive.
pub(crate) fn from_call(action: &str, args: &Map<String, Value>) -> Result<Call, String> {
    if !ACTIONS.contains(&action) {
        return Err(format!("no action `{action}`"));
    }
    let key = match args.get(HANDLE) {
        Some(Value::Object(handle)) => handle.get("key").and_then(Value::as_str),
        Some(_) => None,
        None => return Err(format!("{action}: parameter {HANDLE} is required")),
    };
    let Some(key) = key else {
        return Err(format!(
            "{action}: parameter {HANDLE} must be an item handle"
        ));
    };
    Ok(Call {
        key: key.to_string(),
        op: decode(action, args)?,
    })
}

/// What `action` asks for with `args`, its handle aside.
fn decode(action: &str, args: &Map<String, Value>) -> Result<Op, String> {
    let string = |param: &str| string(action, args, param);
    let read = |read: Read| {
        Ok::<_, String>(Op::Read {
            read,
            to: string("to")?,
        })
    };
    let new_issue = || {
        Ok::<_, String>(NewIssue {
            title: string("title")?,
            description: string("description")?,
            state: match args.get("state") {
                None => None,
                Some(_) => Some(string("state")?),
            },
        })
    };
    let post = |into: Post| {
        Ok::<_, String>(Op::Post {
            into,
            text: string("text")?,
            paths: strings(action, args, "files")?,
        })
    };
    Ok(Op::Act(match action {
        "comment" => return post(Post::Comment),
        "describe" => return post(Post::Description),
        "rename" => IssueAction::Rename(string("title")?),
        "move_to" => IssueAction::MoveTo {
            state: string("state")?,
        },
        "add_label" => IssueAction::AddLabel {
            name: string("label")?,
        },
        "remove_label" => IssueAction::RemoveLabel {
            name: string("label")?,
        },
        "create_issue" => IssueAction::CreateIssue(new_issue()?),
        "create_sub_issue" => IssueAction::CreateSubIssue(new_issue()?),
        "archive" => IssueAction::Archive,
        "move_sub_issue" => IssueAction::MoveSubIssue {
            sub_issue: string("sub_issue")?,
            state: string("state")?,
        },
        "list_comments" => return read(Read::Comments),
        "read_description" => return read(Read::Description),
        "list_sub_issues" => return read(Read::SubIssues),
        "fetch_files" => return Ok(Op::FetchFiles { to: string("to")? }),
        _ => return Err(format!("no action `{action}`")),
    }))
}

/// The string argument `param` of `action`, or the sentence refusing its absence or type.
fn string(action: &str, args: &Map<String, Value>, param: &str) -> Result<String, String> {
    match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    }
}

/// The list of strings `param` of `action`, `[]` when absent, or the sentence refusing its
/// type.
fn strings(action: &str, args: &Map<String, Value>, param: &str) -> Result<Vec<String>, String> {
    let refuse = || format!("{action}: parameter {param} must be a list of strings");
    match args.get(param) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string).ok_or_else(refuse))
            .collect(),
        Some(_) => Err(refuse()),
    }
}

/// The write a [`Op::Post`] asks for, each file at `paths` read now, or the sentence that
/// refuses the first that cannot be.
pub(crate) fn posted(into: Post, text: String, paths: &[String]) -> Result<IssueAction, String> {
    let files = paths
        .iter()
        .map(|path| file::read_upload(into.action(), path))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(match into {
        Post::Comment => IssueAction::Comment { text, files },
        Post::Description => IssueAction::Describe { text, files },
    })
}

/// The id of `team`'s state named `name`.
fn state_id(tracker: &dyn Tracker, team: &Team, name: &str) -> Result<String, LinearError> {
    Ok(state_named(&tracker.states(&team.id)?, name, &team.key)?.id)
}

/// Do `action` on `issue` once, in the service's `team`: the one or two requests it takes,
/// each `?`-propagated, so a failing action is the caller's to queue or refuse.
pub(crate) fn perform(
    tracker: &dyn Tracker,
    team: &Team,
    issue: &Issue,
    action: &IssueAction,
) -> Result<(), LinearError> {
    let create = |parent: Option<&str>, new: &NewIssue| {
        let state = match &new.state {
            Some(name) => Some(state_id(tracker, team, name)?),
            None => None,
        };
        tracker
            .create_issue(
                &team.id,
                parent,
                &new.title,
                &new.description,
                state.as_deref(),
            )
            .map(drop)
    };
    match action {
        // Verbatim: afkd has already interpolated whatever `#{…}` the slot wrote.
        IssueAction::Comment { text, files } => {
            let body = file::embedded(tracker, text, files)?;
            tracker.post_comment(&issue.id, &body).map(drop)
        }
        IssueAction::Describe { text, files } => {
            let body = file::embedded(tracker, text, files)?;
            tracker.set_description(&issue.id, &body)
        }
        IssueAction::Rename(title) => tracker.set_title(&issue.id, title),
        IssueAction::MoveTo { state } => {
            tracker.set_state(&issue.id, &state_id(tracker, team, state)?)
        }
        IssueAction::AddLabel { name } => tracker.add_label(&team.id, &issue.id, name),
        IssueAction::RemoveLabel { name } => tracker.remove_label(&team.id, &issue.id, name),
        IssueAction::CreateIssue(new) => create(None, new),
        IssueAction::CreateSubIssue(new) => create(Some(&issue.id), new),
        IssueAction::Archive => tracker.archive_issue(&issue.id),
        IssueAction::MoveSubIssue { sub_issue, state } => {
            let children = tracker.sub_issues(&issue.id)?;
            let Some(child) = children.iter().find(|c| c.identifier == *sub_issue) else {
                return Err(LinearError::NoSuch {
                    stage: "move issue",
                    what: format!("sub-issue {sub_issue} of {}", issue.identifier),
                });
            };
            tracker.set_state(&child.id, &state_id(tracker, team, state)?)
        }
    }
}

/// How a log line names the files a write carries: ` with ` and their names, or nothing.
pub(crate) fn with_files(files: &[Upload]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    format!(" with {}", names.join(", "))
}

/// The success line for `action` done on `issue`: the issue named, and what was done in
/// the words the slot wrote it with — never a comment's or a description's text, which can
/// be long and many lines, but the names of the files it carries.
pub(crate) fn action_line(action: &IssueAction, team: &Team, issue: &Issue) -> String {
    let issue = named(issue);
    let done = match action {
        IssueAction::Comment { files, .. } => {
            format!("commenting on {issue}{}", with_files(files))
        }
        IssueAction::Describe { files, .. } => format!("describing {issue}{}", with_files(files)),
        IssueAction::Rename(title) => format!("renaming {issue} to \"{title}\""),
        IssueAction::MoveTo { state } => format!("moving {issue} to state \"{state}\""),
        IssueAction::AddLabel { name } => format!("adding label \"{name}\" to {issue}"),
        IssueAction::RemoveLabel { name } => format!("removing label \"{name}\" from {issue}"),
        IssueAction::CreateIssue(new) => format!(
            "creating issue \"{}\" in team {} from {issue}",
            new.title, team.key
        ),
        IssueAction::CreateSubIssue(new) => {
            format!("creating sub-issue \"{}\" under {issue}", new.title)
        }
        IssueAction::Archive => format!("archiving {issue}"),
        IssueAction::MoveSubIssue { sub_issue, state } => {
            format!("moving sub-issue {sub_issue} of {issue} to state \"{state}\"")
        }
    };
    format!("{ACTION_TAG} {done}")
}

/// Do `read` of `issue` now, and write what it read to the absolute path `to`, its comments
/// rendered as the brief renders them for a reader whose own ids are `me`. The text goes to
/// a temp file beside `to` and is renamed onto it, so `to` never holds part of it; a missing
/// parent directory is an error, not created.
pub(crate) fn read(
    tracker: &dyn Tracker,
    issue: &Issue,
    me: &SelfIds,
    read: Read,
    to: &str,
    diag: &dyn Diag,
) -> Result<(), String> {
    let action = read.action();
    let dest = Path::new(to);
    if !dest.is_absolute() {
        return Err(format!("{action}: to must be absolute, not {to:?}"));
    }
    let Some(file) = dest.file_name() else {
        return Err(format!("{action}: to names no file: {to}"));
    };
    let (text, what) = match read {
        Read::Comments => {
            let mut thread = tracker
                .issue_comments(&issue.id)
                .map_err(|e| e.to_string())?;
            thread.retain(|c| !is_control_marker(&c.text));
            thread.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));
            let n = thread.len();
            let thread: Vec<_> = thread.iter().collect();
            (
                render_comments(&thread, me),
                format!("the comments on {} to {to} ({n} comments)", named(issue)),
            )
        }
        Read::Description => (
            tracker
                .issue_description(&issue.id)
                .map_err(|e| e.to_string())?,
            format!("the description of {} to {to}", named(issue)),
        ),
        Read::SubIssues => {
            let children = tracker.sub_issues(&issue.id).map_err(|e| e.to_string())?;
            let text: String = children
                .iter()
                .map(|c| format!("{}\n", sub_issue_line(c)))
                .collect();
            (
                text,
                format!(
                    "the sub-issues of {} to {to} ({} sub-issues)",
                    named(issue),
                    children.len()
                ),
            )
        }
    };
    let temp = dest.with_file_name(format!(
        ".{}.afkd-linear-{}",
        file.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temp, &text)
        .and_then(|()| std::fs::rename(&temp, dest))
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("{action}: cannot write {to}: {e}")
        })?;
    diag.narrate(&format!("{ACTION_TAG} writing {what}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{CaptureDiag, TempDir};
    use crate::file::upload;
    use crate::tracker::{Action, Child, MockTracker, MOCK_UPLOADS, SELF_ID, TEAM_ID};
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    /// The key an issue's handle carries: its uuid and its claim comment's.
    const KEY: &str = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5#7a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    /// An issue title carrying the delimiter a success line wraps it in, wide CJK, an emoji
    /// and an em dash.
    const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";

    /// Decode `action` over `args` as afkd binds them, the issue's handle first, and hold
    /// the call to the issue the handle names.
    fn call(action: &str, args: serde_json::Value) -> Result<Op, String> {
        let mut bound =
            Map::from_iter([(HANDLE.to_string(), json!({"id": "ENG-421", "key": KEY}))]);
        bound.extend(args.as_object().expect("an args object").clone());
        from_call(action, &bound).map(|call| {
            assert_eq!(call.key, KEY, "{action}");
            call.op
        })
    }

    fn team() -> Team {
        Team {
            id: TEAM_ID.into(),
            key: "ENG".into(),
        }
    }

    /// A team with a state of each kind a move names, its issue `ENG-1` in Todo with two
    /// sub-issues, `ENG-2` and `ENG-3`.
    fn tracker() -> MockTracker {
        let t = MockTracker::new();
        for (state, kind) in [
            ("Todo", "unstarted"),
            ("In Progress", "started"),
            ("Done ✅", "completed"),
        ] {
            t.add_state(state, kind);
        }
        t.add_issue("Todo", "ENG-1", TITLE, "The body.");
        t.add_issue("In Progress", "ENG-2", "Cap the backoff at 30s", "");
        t.add_issue("Todo", "ENG-3", "子任务: read Retry-After 🚦", "");
        t.edit_issue("ENG-1", |i| {
            i.children = ["ENG-2", "ENG-3"]
                .iter()
                .map(|id| Child {
                    id: (*id).into(),
                    identifier: (*id).into(),
                    title: String::new(),
                    state: String::new(),
                })
                .collect();
        });
        t
    }

    /// Every action in [`ACTIONS`] decodes from the arguments afkd binds for it, on the issue
    /// its handle names, each text byte for byte: a multi-line markdown comment carrying CJK
    /// and a literal `@{run:x}` afkd did not interpolate with two files, an empty
    /// description with none, a wide state, an emoji label, a new issue with and without a
    /// state, and absolute paths with spaces and wide glyphs.
    #[test]
    fn every_action_decodes_from_its_bound_arguments() {
        let markdown = "## 完了 ✅\n\n- took 3m\n- log: @{run:x}\n\nsee the run log.";
        let shots = [
            "/srv/acme/計画 1/screen shot — 修复.png",
            "/srv/acme/run.log",
        ];
        let decoded: Vec<Op> = [
            ("comment", json!({"text": markdown, "files": shots})),
            ("describe", json!({"text": "", "files": []})),
            ("rename", json!({"title": TITLE})),
            ("move_to", json!({"state": "Blockerat / Väntar"})),
            ("add_label", json!({"label": "needs review 👀"})),
            ("remove_label", json!({"label": "Redo"})),
            (
                "create_issue",
                json!({"title": "Cap it", "description": markdown, "state": "Backlog 📥"}),
            ),
            (
                "create_sub_issue",
                json!({"title": "子任务", "description": ""}),
            ),
            ("archive", json!({})),
            ("list_comments", json!({"to": "/srv/acme/計画 1/thread.md"})),
            (
                "read_description",
                json!({"to": "/srv/acme/description.md"}),
            ),
            ("list_sub_issues", json!({"to": "/srv/acme/subs.md"})),
            (
                "move_sub_issue",
                json!({"sub_issue": "ENG-124", "state": "Done ✅"}),
            ),
            ("fetch_files", json!({"to": "/srv/acme/計画 1/files"})),
        ]
        .into_iter()
        .map(|(action, args)| call(action, args).expect(action))
        .collect();
        assert_eq!(
            decoded,
            [
                Op::Post {
                    into: Post::Comment,
                    text: markdown.into(),
                    paths: shots.map(str::to_string).to_vec(),
                },
                Op::Post {
                    into: Post::Description,
                    text: String::new(),
                    paths: Vec::new(),
                },
                Op::Act(IssueAction::Rename(TITLE.into())),
                Op::Act(IssueAction::MoveTo {
                    state: "Blockerat / Väntar".into()
                }),
                Op::Act(IssueAction::AddLabel {
                    name: "needs review 👀".into()
                }),
                Op::Act(IssueAction::RemoveLabel {
                    name: "Redo".into()
                }),
                Op::Act(IssueAction::CreateIssue(NewIssue {
                    title: "Cap it".into(),
                    description: markdown.into(),
                    state: Some("Backlog 📥".into()),
                })),
                Op::Act(IssueAction::CreateSubIssue(NewIssue {
                    title: "子任务".into(),
                    description: String::new(),
                    state: None,
                })),
                Op::Act(IssueAction::Archive),
                Op::Read {
                    read: Read::Comments,
                    to: "/srv/acme/計画 1/thread.md".into()
                },
                Op::Read {
                    read: Read::Description,
                    to: "/srv/acme/description.md".into()
                },
                Op::Read {
                    read: Read::SubIssues,
                    to: "/srv/acme/subs.md".into()
                },
                Op::Act(IssueAction::MoveSubIssue {
                    sub_issue: "ENG-124".into(),
                    state: "Done ✅".into(),
                }),
                Op::FetchFiles {
                    to: "/srv/acme/計画 1/files".into()
                },
            ]
        );
        assert_eq!(decoded.len(), ACTIONS.len());
        assert_eq!(
            call("describe", json!({"text": "x"})),
            Ok(Op::Post {
                into: Post::Description,
                text: "x".into(),
                paths: Vec::new(),
            }),
            "no files is none"
        );
        assert_eq!(
            call("archive", json!({"state": "Done"})),
            Ok(Op::Act(IssueAction::Archive)),
            "an argument the action has no parameter for is ignored"
        );
    }

    /// Each shape afkd would never bind is refused with a sentence naming the action and
    /// the parameter: an unknown action, a missing parameter, one of the wrong type, and a
    /// new issue's state that is no string.
    #[test]
    fn a_call_afkd_would_not_bind_is_refused_by_name() {
        for (action, args, problem) in [
            ("assign", json!({"to": "me"}), "no action `assign`"),
            ("", json!({}), "no action ``"),
            ("comment", json!({}), "comment: parameter text is required"),
            (
                "comment",
                json!({"text": {"@value": "x"}}),
                "comment: parameter text must be a string",
            ),
            (
                "describe",
                json!({"title": "x"}),
                "describe: parameter text is required",
            ),
            (
                "comment",
                json!({"text": "x", "files": "/srv/acme/shot.png"}),
                "comment: parameter files must be a list of strings",
            ),
            (
                "describe",
                json!({"text": "x", "files": ["/srv/acme/shot.png", 7]}),
                "describe: parameter files must be a list of strings",
            ),
            (
                "fetch_files",
                json!({}),
                "fetch_files: parameter to is required",
            ),
            (
                "rename",
                json!({"title": 7}),
                "rename: parameter title must be a string",
            ),
            ("move_to", json!({}), "move_to: parameter state is required"),
            (
                "move_to",
                json!({"state": ["A", "B"]}),
                "move_to: parameter state must be a string",
            ),
            (
                "add_label",
                json!({}),
                "add_label: parameter label is required",
            ),
            (
                "remove_label",
                json!({"label": true}),
                "remove_label: parameter label must be a string",
            ),
            (
                "create_issue",
                json!({"description": ""}),
                "create_issue: parameter title is required",
            ),
            (
                "create_issue",
                json!({"title": "t"}),
                "create_issue: parameter description is required",
            ),
            (
                "create_sub_issue",
                json!({"title": "t", "description": "", "state": null}),
                "create_sub_issue: parameter state must be a string",
            ),
            (
                "list_comments",
                json!({}),
                "list_comments: parameter to is required",
            ),
            (
                "read_description",
                json!({"to": 7}),
                "read_description: parameter to must be a string",
            ),
            (
                "move_sub_issue",
                json!({"state": "Done"}),
                "move_sub_issue: parameter sub_issue is required",
            ),
            (
                "move_sub_issue",
                json!({"sub_issue": "ENG-2"}),
                "move_sub_issue: parameter state is required",
            ),
        ] {
            assert_eq!(
                call(action, args.clone()),
                Err(problem.to_string()),
                "{action} {args}"
            );
        }
    }

    /// The issue is the handle afkd passes first, and a call without a usable one is refused
    /// by name before its other parameters are read; the key alone names the issue.
    #[test]
    fn a_call_without_an_issue_handle_is_refused_by_name() {
        let required = "move_to: parameter issue is required";
        let malformed = "move_to: parameter issue must be an item handle";
        for (args, problem) in [
            (json!({}), required),
            (json!({"state": "Done"}), required),
            (json!({"issue": null}), malformed),
            (json!({"issue": "ENG-421", "state": "Done"}), malformed),
            (json!({"issue": {}, "state": "Done"}), malformed),
            (json!({"issue": {"id": "ENG-421"}}), malformed),
            (json!({"issue": {"id": "ENG-421", "key": 7}}), malformed),
        ] {
            assert_eq!(
                from_call("move_to", args.as_object().unwrap()),
                Err(problem.to_string()),
                "{args}"
            );
        }
        assert_eq!(
            from_call(
                "archive",
                json!({"issue": {"key": KEY}}).as_object().unwrap()
            ),
            Ok(Call {
                key: KEY.into(),
                op: Op::Act(IssueAction::Archive),
            })
        );
    }

    /// Every write is the request it means, in the service's team: a state named to its id,
    /// a new issue under its parent or none, a label by the team, and a sub-issue moved by
    /// its own id. A name the team lacks is refused with the sentence `sources` uses, and a
    /// sub-issue that is not this issue's is refused naming both.
    #[test]
    fn each_write_does_what_it_names() {
        let t = tracker();
        let issue = t.issue("ENG-1");
        let new = |state: Option<&str>| NewIssue {
            title: "子任务: cap it".into(),
            description: "Linear's header is in seconds.".into(),
            state: state.map(str::to_string),
        };
        for action in [
            IssueAction::Comment {
                text: "## 完了 ✅\n\nok".into(),
                files: Vec::new(),
            },
            IssueAction::Describe {
                text: "New body.".into(),
                files: Vec::new(),
            },
            IssueAction::Rename("Renamed 🚀".into()),
            IssueAction::MoveTo {
                state: "Done ✅".into(),
            },
            IssueAction::AddLabel {
                name: "reviewed".into(),
            },
            IssueAction::RemoveLabel {
                name: "reviewed".into(),
            },
            IssueAction::CreateIssue(new(Some("In Progress"))),
            IssueAction::CreateSubIssue(new(None)),
            IssueAction::MoveSubIssue {
                sub_issue: "ENG-3".into(),
                state: "In Progress".into(),
            },
            IssueAction::Archive,
        ] {
            perform(&t, &team(), &issue, &action).unwrap_or_else(|e| panic!("{action:?}: {e}"));
        }
        assert_eq!(t.comments_on("ENG-1")[0].text, "## 完了 ✅\n\nok");
        let create = |parent: Option<&str>, state: Option<&str>| Action::Create {
            team: TEAM_ID.into(),
            parent: parent.map(str::to_string),
            title: "子任务: cap it".into(),
            description: "Linear's header is in seconds.".into(),
            state: state.map(str::to_string),
        };
        assert_eq!(
            t.actions(),
            [
                Action::SetDescription {
                    issue: "ENG-1".into(),
                    text: "New body.".into()
                },
                Action::SetTitle {
                    issue: "ENG-1".into(),
                    title: "Renamed 🚀".into()
                },
                Action::SetState {
                    issue: "ENG-1".into(),
                    state: "Done ✅".into()
                },
                Action::AddLabel {
                    issue: "ENG-1".into(),
                    label: "reviewed".into()
                },
                Action::RemoveLabel {
                    issue: "ENG-1".into(),
                    label: "reviewed".into()
                },
                create(None, Some("In Progress")),
                create(Some("ENG-1"), None),
                Action::SetState {
                    issue: "ENG-3".into(),
                    state: "In Progress".into()
                },
                Action::Archive("ENG-1".into()),
            ]
        );
        assert!(t.archived("ENG-1"));

        let before = t.actions().len();
        for (action, refusal) in [
            (
                IssueAction::MoveTo {
                    state: "Nirgendwo 🚫".into(),
                },
                "linear resolve state: no workflow state named 'Nirgendwo 🚫' in team ENG",
            ),
            (
                IssueAction::CreateIssue(new(Some("Nirgendwo 🚫"))),
                "linear resolve state: no workflow state named 'Nirgendwo 🚫' in team ENG",
            ),
            (
                IssueAction::MoveSubIssue {
                    sub_issue: "ENG-9".into(),
                    state: "Done ✅".into(),
                },
                "linear move issue: no sub-issue ENG-9 of ENG-1",
            ),
        ] {
            let e = perform(&t, &team(), &issue, &action).unwrap_err();
            assert_eq!(e.to_string(), refusal);
            assert!(!e.transient(), "{e}");
        }
        assert_eq!(t.actions().len(), before, "a refused write sends nothing");
    }

    /// A comment or a description with files puts each up before it is sent, an image
    /// embedded and any other file linked after the text, and with no text the files alone.
    /// The outbox's copy of the write, retried after it landed, puts nothing up again; an
    /// upload Linear cannot take sends nothing, and is the caller's to queue.
    #[test]
    fn a_write_with_files_puts_each_up_once_and_embeds_it() {
        let t = tracker();
        let issue = t.issue("ENG-1");
        let files = vec![upload("screen shot — 修复.png"), upload("run.log")];
        let comment = IssueAction::Comment {
            text: "After the fix ✅".into(),
            files: files.clone(),
        };
        let queued = comment.clone();
        perform(&t, &team(), &issue, &comment).unwrap();
        let url = |n: usize| format!("{MOCK_UPLOADS}acme/{n}/asset-{n}");
        let body = format!(
            "After the fix ✅\n\n![screen shot — 修复.png]({})\n\n[run.log]({})",
            url(1),
            url(2)
        );
        assert_eq!(t.comments_on("ENG-1")[0].text, body);
        assert_eq!(t.file(&url(1)).as_deref(), Some(&*files[0].bytes));
        assert_eq!(t.file(&url(2)).as_deref(), Some(&*files[1].bytes));
        perform(&t, &team(), &issue, &queued).unwrap();
        assert_eq!(
            t.comments_on("ENG-1")[1].text,
            body,
            "the retry sends the same text"
        );
        let uploads = |t: &MockTracker| {
            t.actions()
                .into_iter()
                .filter(|a| matches!(a, Action::Upload { .. }))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            uploads(&t),
            [
                Action::Upload {
                    name: "screen shot — 修复.png".into(),
                    content_type: "image/png".into(),
                    len: files[0].bytes.len(),
                },
                Action::Upload {
                    name: "run.log".into(),
                    content_type: "text/plain".into(),
                    len: files[1].bytes.len(),
                },
            ],
            "each put up once"
        );

        let describe = IssueAction::Describe {
            text: String::new(),
            files: vec![upload("shot.png")],
        };
        t.fail(
            "upload file",
            LinearError::Status {
                stage: "upload file",
                status: 503,
                retry_after: None,
            },
        );
        let e = perform(&t, &team(), &issue, &describe).unwrap_err();
        assert!(e.transient(), "{e}");
        assert_eq!(t.issue("ENG-1").description, "The body.", "nothing sent");
        t.clear_failure();
        perform(&t, &team(), &issue, &describe).unwrap();
        assert_eq!(
            t.issue("ENG-1").description,
            format!("![shot.png]({})", url(3))
        );
        assert_eq!(uploads(&t).len(), 3);
    }

    /// A post's files are read at the call, each named by its path's last part; the first
    /// that cannot be read refuses the whole write in its action's words.
    #[test]
    fn a_posts_files_are_read_at_the_call() {
        let dir = TempDir::new("posted");
        let shot = dir.path().join("screen shot — 修复.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n\0\xff").unwrap();
        let shot = shot.display().to_string();
        let Ok(IssueAction::Comment { text, files }) = posted(
            Post::Comment,
            "After ✅".into(),
            std::slice::from_ref(&shot),
        ) else {
            panic!("a comment with its file");
        };
        assert_eq!(text, "After ✅");
        assert_eq!(files.len(), 1);
        assert_eq!(
            (
                files[0].name.as_str(),
                files[0].content_type.as_str(),
                &*files[0].bytes
            ),
            (
                "screen shot — 修复.png",
                "image/png",
                &b"\x89PNG\r\n\x1a\n\0\xff"[..]
            )
        );
        assert_eq!(
            posted(Post::Description, String::new(), &[shot, "shot.png".into()]),
            Err("describe: files must be absolute, not \"shot.png\"".to_string())
        );
    }

    /// The success line names the issue and what was done to it in the slot's own words —
    /// the state, the label, the new title — and never a comment's or a description's text.
    #[test]
    fn action_line_names_the_issue_and_its_operand_for_every_action() {
        let t = tracker();
        let issue = t.issue("ENG-1");
        let named = format!("issue ENG-1 \"{TITLE}\"");
        let new = |title: &str| NewIssue {
            title: title.into(),
            description: "a long\nbody".into(),
            state: None,
        };
        for (action, line) in [
            (
                IssueAction::Comment {
                    text: "## 完了 ✅\n\nmany lines".into(),
                    files: Vec::new(),
                },
                format!("[linear] commenting on {named}"),
            ),
            (
                IssueAction::Comment {
                    text: "## 完了 ✅".into(),
                    files: vec![upload("screen shot — 修复.png"), upload("run.log")],
                },
                format!("[linear] commenting on {named} with screen shot — 修复.png, run.log"),
            ),
            (
                IssueAction::Describe {
                    text: "New body\nwith lines".into(),
                    files: Vec::new(),
                },
                format!("[linear] describing {named}"),
            ),
            (
                IssueAction::Describe {
                    text: String::new(),
                    files: vec![upload("shot.png")],
                },
                format!("[linear] describing {named} with shot.png"),
            ),
            (
                IssueAction::Rename("新しい 🚀".into()),
                format!("[linear] renaming {named} to \"新しい 🚀\""),
            ),
            (
                IssueAction::MoveTo {
                    state: "Done ✅".into(),
                },
                format!("[linear] moving {named} to state \"Done ✅\""),
            ),
            (
                IssueAction::AddLabel {
                    name: "needs review 👀".into(),
                },
                format!("[linear] adding label \"needs review 👀\" to {named}"),
            ),
            (
                IssueAction::RemoveLabel {
                    name: "Redo".into(),
                },
                format!("[linear] removing label \"Redo\" from {named}"),
            ),
            (
                IssueAction::CreateIssue(new("Cap it")),
                format!("[linear] creating issue \"Cap it\" in team ENG from {named}"),
            ),
            (
                IssueAction::CreateSubIssue(new("子任务")),
                format!("[linear] creating sub-issue \"子任务\" under {named}"),
            ),
            (IssueAction::Archive, format!("[linear] archiving {named}")),
            (
                IssueAction::MoveSubIssue {
                    sub_issue: "ENG-124".into(),
                    state: "Done ✅".into(),
                },
                format!("[linear] moving sub-issue ENG-124 of {named} to state \"Done ✅\""),
            ),
        ] {
            assert_eq!(action_line(&action, &team(), &issue), line);
        }
    }

    /// Each read writes what Linear holds now to `to`, whole, and says so: the thread oldest
    /// first, markers dropped and afkd's own words as afkd's; the description verbatim; and
    /// each sub-issue on a line. An empty description and an empty thread are empty files.
    #[test]
    fn a_read_writes_what_linear_holds_now() {
        let t = tracker();
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        t.push_comment(
            "ENG-1",
            "c2",
            "看起来不对 🚨\n\n```\nPOST 401\n```",
            "u-chen",
            at(20),
        );
        t.push_comment("ENG-1", "c1", "Please cap it.", "u-phil", at(10));
        t.push_comment(
            "ENG-1",
            "m1",
            "[afkd-claim] owner=afkd-4242",
            SELF_ID,
            at(15),
        );
        t.push_comment("ENG-1", "c3", "Done, capped at 30s.", SELF_ID, at(30));
        t.edit_issue("ENG-1", |i| {
            i.description = "## 背景\n\nIt resets.\n".into();
            i.children[0].state = "In Progress".into();
            i.children[0].title = "Cap the backoff at 30s".into();
            i.children[1].state = "Todo".into();
            i.children[1].title = "子任务: read Retry-After 🚦".into();
        });
        let issue = t.issue("ENG-1");
        let me = SelfIds::viewer(SELF_ID.into());
        let (dir, diag) = (TempDir::new("read"), CaptureDiag::default());
        let path = |name: &str| dir.path().join(name).display().to_string();
        let read_to = |what: Read, name: &str| {
            read(&t, &issue, &me, what, &path(name), &diag).unwrap();
            std::fs::read_to_string(path(name)).unwrap()
        };
        assert_eq!(
            read_to(Read::Comments, "thread.md"),
            "**u-phil:** Please cap it.\n\n**u-chen:** 看起来不对 🚨\n\n```\nPOST 401\n```\n\n\
             **afkd:** Done, capped at 30s.\n"
        );
        assert_eq!(
            read_to(Read::Description, "description.md"),
            "## 背景\n\nIt resets.\n"
        );
        assert_eq!(
            read_to(Read::SubIssues, "subs.md"),
            "- ENG-2 (In Progress) Cap the backoff at 30s\n\
             - ENG-3 (Todo) 子任务: read Retry-After 🚦\n"
        );
        let named = format!("issue ENG-1 \"{TITLE}\"");
        assert_eq!(
            diag.narrated(),
            [
                format!(
                    "[linear] writing the comments on {named} to {} (3 comments)",
                    path("thread.md")
                ),
                format!(
                    "[linear] writing the description of {named} to {}",
                    path("description.md")
                ),
                format!(
                    "[linear] writing the sub-issues of {named} to {} (2 sub-issues)",
                    path("subs.md")
                ),
            ]
        );

        let bare = t.issue("ENG-2");
        for what in [Read::Comments, Read::Description, Read::SubIssues] {
            read(&t, &bare, &me, what, &path("bare.md"), &diag).unwrap();
            assert_eq!(std::fs::read_to_string(path("bare.md")).unwrap(), "");
        }
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(left.is_empty(), "no temp file is left: {left:?}");
    }

    /// A read is refused, and nothing written, for a `to` that is relative, names no file,
    /// or lies in a directory that does not exist; and for a Linear that cannot be read.
    #[test]
    fn a_read_refuses_what_it_cannot_write() {
        let t = tracker();
        let issue = t.issue("ENG-1");
        let me = SelfIds::viewer(SELF_ID.into());
        let (dir, diag) = (TempDir::new("refuse"), CaptureDiag::default());
        let missing = dir.path().join("nowhere/thread.md").display().to_string();
        for (what, to, refusal) in [
            (
                Read::Comments,
                "thread.md".to_string(),
                "list_comments: to must be absolute, not \"thread.md\"".to_string(),
            ),
            (
                Read::Description,
                "/".to_string(),
                "read_description: to names no file: /".to_string(),
            ),
            (
                Read::SubIssues,
                missing.clone(),
                format!(
                    "list_sub_issues: cannot write {missing}: No such file or directory (os \
                     error 2)"
                ),
            ),
        ] {
            assert_eq!(read(&t, &issue, &me, what, &to, &diag), Err(refusal));
        }
        t.go_down();
        let to = dir.path().join("thread.md").display().to_string();
        assert_eq!(
            read(&t, &issue, &me, Read::Comments, &to, &diag),
            Err("linear read comments: no response (mock failure)".to_string())
        );
        assert!(!dir.path().join("thread.md").exists());
        assert!(diag.narrated().is_empty());
    }
}
