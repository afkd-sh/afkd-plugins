//! The **workspace writes** a config calls with a `linear.Workspace` first: an issue filed, its
//! fields, its team and its place in its state changed, its labels, relations, links and
//! subscribers added or removed, the issue archived, put in the trash or taken back out, and a
//! comment written, edited, resolved or deleted.
//!
//! A write reaches Linear only through the workspace it is passed, as a read does
//! ([`crate::reads`]), so it answers alike in a child afkd starts for the one call and in a
//! `service(linear)`'s trigger child, where a slot may pass it the issue that service claimed.
//! Each write
//!
//! 1. decodes its arguments, a refusal sent before any request ([`decode`]);
//! 2. reads the files it puts up, before any request too;
//! 3. reads the workspace's organization, records its url key, and checks that each handle it
//!    was passed is of that workspace, before it changes anything;
//! 4. turns each name it was passed into Linear's id ([`crate::lookup`]);
//! 5. sends the change, none when nothing was given to change or Linear already holds it;
//! 6. answers with what it made or changed as Linear then holds it, read back as the reads read
//!    it, so its handle is the one `issues` or `comments` hands back; `delete_comment` answers
//!    nil.
//!
//! A write is never queued: it is done now, a request Linear could not take tried again within
//! the call's deadline, or refused with the reason.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::client::{LinearClient, OneComment};
use crate::common::Diag;
use crate::file::{embedded, read_upload, Upload};
use crate::issue::ACTION_TAG;
use crate::lookup::{self, Candidate};
use crate::reads::{
    fit_call, handle_arg, is_date, keyed, member, optional, reach, resolve, HandleArg, ReadContext,
};
use crate::settings::ME;
use crate::tracker::{LinearError, Organization, Priority, Team, Tracker};
use crate::wire::{IssueFields, WireHandle};
use crate::workspace::{read_key, Workspace};

/// The writes, in the order the manifest declares them, after the reads
/// ([`READS`](crate::reads::READS)).
pub(crate) const WRITES: &[&str] = &[
    "create_issue",
    "update_issue",
    "add_label",
    "remove_label",
    "add_relation",
    "remove_relation",
    "add_link",
    "remove_link",
    "add_subscriber",
    "remove_subscriber",
    "archive_issue",
    "unarchive_issue",
    "delete_issue",
    "create_comment",
    "update_comment",
    "resolve_comment",
    "unresolve_comment",
    "delete_comment",
];

/// How far apart `at` places an issue from the one at the top or the bottom of its state, in
/// Linear's `sortOrder`: the gap the kind's own put-back leaves.
const GAP: f64 = 1000.0;

/// A `T?` parameter of a write: left out, it leaves the field as it is; `nil`, it clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Change<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T> Change<T> {
    /// The change with what it sets turned by `f`.
    fn map<U>(&self, f: impl FnOnce(&T) -> U) -> Change<U> {
        match self {
            Change::Keep => Change::Keep,
            Change::Clear => Change::Clear,
            Change::Set(value) => Change::Set(f(value)),
        }
    }

    /// The change with what it sets turned by `f`, which may refuse it.
    fn try_map<U, E>(&self, f: impl FnOnce(&T) -> Result<U, E>) -> Result<Change<U>, E> {
        Ok(match self {
            Change::Keep => Change::Keep,
            Change::Clear => Change::Clear,
            Change::Set(value) => Change::Set(f(value)?),
        })
    }
}

/// Where `at` places an issue in its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum At {
    Top,
    Bottom,
}

/// Where `create_issue` or `update_issue` places an issue in its state: at its top or its
/// bottom, or right below another issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    At(At),
    After(HandleArg),
}

/// How `add_relation` and `remove_relation` relate an issue to another: afterwards the issue's
/// field of that name holds the other's identifier, or no longer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Relation {
    Blocks,
    BlockedBy,
    Related,
    Similar,
    DuplicateOf,
    Duplicates,
}

impl Relation {
    /// Every member, in the manifest's order.
    #[cfg(test)]
    pub(crate) const ALL: [Relation; 6] = [
        Relation::Blocks,
        Relation::BlockedBy,
        Relation::Related,
        Relation::Similar,
        Relation::DuplicateOf,
        Relation::Duplicates,
    ];

    /// Its name, the issue's field it shows in.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Relation::Blocks => "blocks",
            Relation::BlockedBy => "blocked_by",
            Relation::Related => "related",
            Relation::Similar => "similar",
            Relation::DuplicateOf => "duplicate_of",
            Relation::Duplicates => "duplicates",
        }
    }

    /// Linear's relation it is, and whether the issue holds it: `true` when the issue is the
    /// relation's `issueId` and the other its `relatedIssueId`, `false` the other way round.
    pub(crate) fn linear(self) -> (&'static str, bool) {
        match self {
            Relation::Blocks => ("blocks", true),
            Relation::BlockedBy => ("blocks", false),
            Relation::Related => ("related", true),
            Relation::Similar => ("similar", true),
            Relation::DuplicateOf => ("duplicate", true),
            Relation::Duplicates => ("duplicate", false),
        }
    }

    /// Whether it reads the same from either end, so the issue's field shows it whichever of
    /// the two holds it.
    fn symmetric(self) -> bool {
        matches!(self, Relation::Related | Relation::Similar)
    }
}

/// What `archive_issue`, `unarchive_issue` and `delete_issue` do with an issue, and its
/// sub-issues with it, as Linear does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Archival {
    Archive,
    /// Out of the archive, or out of the trash.
    Unarchive,
    /// Into the trash.
    Delete,
}

/// What `update_issue` was asked to change, each name as it was given.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct IssueUpdate {
    pub(crate) issue: HandleArg,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    /// The absolute paths of the files embedded after the description.
    pub(crate) files: Vec<String>,
    pub(crate) state: Option<String>,
    pub(crate) team: Option<String>,
    pub(crate) place: Option<Place>,
    pub(crate) priority: Option<Priority>,
    pub(crate) estimate: Change<i64>,
    pub(crate) due: Change<String>,
    pub(crate) assignee: Change<String>,
    pub(crate) delegate: Change<String>,
    pub(crate) project: Change<String>,
    pub(crate) milestone: Change<String>,
    pub(crate) cycle: Change<i64>,
    pub(crate) parent: Change<HandleArg>,
}

/// What `create_issue` files, each name as it was given; a `T?` parameter passed `nil` is
/// `None`, as one left out is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NewIssue {
    pub(crate) team: String,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    /// The absolute paths of the files embedded after the description.
    pub(crate) files: Vec<String>,
    pub(crate) state: Option<String>,
    pub(crate) place: Option<Place>,
    pub(crate) priority: Option<Priority>,
    pub(crate) estimate: Option<i64>,
    pub(crate) due: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) assignee: Option<String>,
    pub(crate) delegate: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) milestone: Option<String>,
    pub(crate) cycle: Option<i64>,
    pub(crate) parent: Option<HandleArg>,
}

/// One write, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteCall {
    CreateIssue(Box<NewIssue>),
    UpdateIssue(Box<IssueUpdate>),
    /// Every other write on an issue: what it changes of the issue `issue`.
    Issue {
        issue: HandleArg,
        change: IssueChange,
    },
    CreateComment(NewComment),
    /// Every other write on a comment: what it does with the comment `comment`.
    Comment {
        comment: HandleArg,
        change: CommentChange,
    },
}

/// What `create_comment` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewComment {
    pub(crate) issue: HandleArg,
    pub(crate) body: String,
    /// The absolute paths of the files embedded after the body.
    pub(crate) files: Vec<String>,
    /// The comment it answers, in whose thread it goes.
    pub(crate) reply_to: Option<HandleArg>,
}

/// What a write other than `create_comment` does with the comment it is passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommentChange {
    /// `update_comment`: the body rewritten, the files embedded after it.
    Update {
        body: String,
        files: Vec<String>,
    },
    /// `resolve_comment` when `true`, else `unresolve_comment`: the thread the comment is in.
    Resolve(bool),
    Delete,
}

/// What a write other than `update_issue` changes of the issue it is passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssueChange {
    /// `add_label` when `add`, else `remove_label`, the label as it was named.
    Label {
        label: String,
        add: bool,
    },
    /// `add_relation` when `add`, else `remove_relation`.
    Relation {
        relation: Relation,
        other: HandleArg,
        add: bool,
    },
    AddLink {
        url: String,
        title: Option<String>,
    },
    RemoveLink {
        url: String,
    },
    /// `add_subscriber` when `add`, else `remove_subscriber`, the user as it was named.
    Subscriber {
        user: String,
        add: bool,
    },
    Archive(Archival),
}

/// What `update_issue` sends Linear, every name turned into its id: Linear's
/// `IssueUpdateInput`, holding each field that changes and no other. `create_issue` sends it
/// too, each field it sets [`Change::Set`], as the `IssueCreateInput` it is a part of.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct IssueInput {
    pub(crate) team: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) sort_order: Option<f64>,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) priority: Option<Priority>,
    pub(crate) estimate: Change<i64>,
    pub(crate) due: Change<String>,
    pub(crate) assignee: Change<String>,
    pub(crate) delegate: Change<String>,
    pub(crate) project: Change<String>,
    pub(crate) milestone: Change<String>,
    pub(crate) cycle: Change<String>,
    pub(crate) parent: Change<String>,
}

impl IssueInput {
    /// The `IssueUpdateInput`: a key for each field set, `null` for each cleared, and none for
    /// a field left as it is.
    pub(crate) fn input(&self) -> Map<String, Value> {
        fn change<T: Serialize>(change: &Change<T>) -> Option<Value> {
            match change {
                Change::Keep => None,
                Change::Clear => Some(Value::Null),
                Change::Set(value) => Some(json!(value)),
            }
        }
        let fields = [
            ("teamId", self.team.as_ref().map(|id| json!(id))),
            ("stateId", self.state.as_ref().map(|id| json!(id))),
            ("sortOrder", self.sort_order.map(|order| json!(order))),
            ("title", self.title.as_ref().map(|title| json!(title))),
            (
                "description",
                self.description.as_ref().map(|text| json!(text)),
            ),
            ("priority", self.priority.map(|p| json!(p.linear()))),
            ("estimate", change(&self.estimate)),
            ("dueDate", change(&self.due)),
            ("assigneeId", change(&self.assignee)),
            ("delegateId", change(&self.delegate)),
            ("projectId", change(&self.project)),
            ("projectMilestoneId", change(&self.milestone)),
            ("cycleId", change(&self.cycle)),
            ("parentId", change(&self.parent)),
        ];
        fields
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_string(), value?)))
            .collect()
    }
}

/// Where [`sort_order_for`] puts an issue among its state's others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Spot {
    Top,
    Bottom,
    /// Right below the issue of this uuid.
    After(String),
}

/// The `sortOrder` that puts an issue at `spot` among `others`, the uuids and sort orders of
/// its state's other issues in manual order: above the first or below the last by [`GAP`], or
/// halfway between the issue it goes after and the one below that, else below it by the gap.
/// `None` when there is nothing to place it against: a state with no other issue, or an
/// `after` it does not hold. Two neighbours of one sort order give it theirs, and Linear then
/// orders the three as it orders any tie.
pub(crate) fn sort_order_for(spot: &Spot, others: &[(String, f64)]) -> Option<f64> {
    match spot {
        Spot::Top => others.first().map(|(_, order)| order - GAP),
        Spot::Bottom => others.last().map(|(_, order)| order + GAP),
        Spot::After(uuid) => {
            let at = others.iter().position(|(id, _)| id == uuid)?;
            let order = others[at].1;
            Some(match others.get(at + 1) {
                Some((_, next)) => (order + next) / 2.0,
                None => order + GAP,
            })
        }
    }
}

/// Do the write `action` with `args`, and answer it: the reply line, `ok` with what it
/// changed, or `ok:false` with the sentence refusing it, which is diagnosed too. A write that
/// lands is narrated.
pub(crate) fn answer(
    action: &str,
    args: &Map<String, Value>,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> String {
    let refuse = |error: String| {
        diag.err(&error);
        json!({ "ok": false, "error": error }).to_string()
    };
    let (ws, call) = match decode(action, args) {
        Ok(decoded) => decoded,
        Err(problem) => return refuse(problem),
    };
    let client = ws.client(ctx.state_dir, ctx.deadline);
    match run(action, &call, &client, ctx, diag).and_then(|(reply, said)| {
        let line = fit_call(action, reply)?;
        Ok((line, said))
    }) {
        Ok((line, said)) => {
            diag.narrate(&said);
            line
        }
        Err(problem) => refuse(problem),
    }
}

/// The workspace and the write `args` write for `action`, or the sentence refusing them.
///
/// # Errors
///
/// Returns the sentence for a `ws` that is no workspace or holds no one credential, an
/// argument missing or of another type than the manifest's, and the refusals of
/// [`new_issue`] and [`issue_update`].
pub(crate) fn decode(
    action: &str,
    args: &Map<String, Value>,
) -> Result<(Workspace, WriteCall), String> {
    let ws = Workspace::from_arg(action, args.get("ws"))?;
    if action == "create_issue" {
        let new = new_issue(action, args)?;
        return Ok((ws, WriteCall::CreateIssue(Box::new(new))));
    }
    if action == "update_issue" {
        let update = issue_update(action, args)?;
        return Ok((ws, WriteCall::UpdateIssue(Box::new(update))));
    }
    let required = |param: &str| {
        optional(action, args, param, text, "a string")?
            .ok_or_else(|| format!("{action}: parameter {param} is required"))
    };
    let comment = |change: CommentChange| -> Result<WriteCall, String> {
        Ok(WriteCall::Comment {
            comment: handle_arg(action, args, "comment")?,
            change,
        })
    };
    match action {
        "create_comment" => {
            let new = NewComment {
                issue: handle_arg(action, args, "issue")?,
                body: required("body")?,
                files: files(action, args)?,
                reply_to: match args.get("reply_to") {
                    None => None,
                    Some(_) => Some(handle_arg(action, args, "reply_to")?),
                },
            };
            return Ok((ws, WriteCall::CreateComment(new)));
        }
        "update_comment" => {
            let update = CommentChange::Update {
                body: required("body")?,
                files: files(action, args)?,
            };
            return Ok((ws, comment(update)?));
        }
        "resolve_comment" => return Ok((ws, comment(CommentChange::Resolve(true))?)),
        "unresolve_comment" => return Ok((ws, comment(CommentChange::Resolve(false))?)),
        "delete_comment" => return Ok((ws, comment(CommentChange::Delete)?)),
        _ => {}
    }
    let relation = |add: bool| -> Result<IssueChange, String> {
        let relation = optional(action, args, "relation", member::<Relation>, RELATIONS)?
            .ok_or_else(|| format!("{action}: parameter relation is required"))?;
        Ok(IssueChange::Relation {
            relation,
            other: handle_arg(action, args, "other")?,
            add,
        })
    };
    let subscriber = |add: bool| -> Result<IssueChange, String> {
        let user = optional(action, args, "user", text, "a string")?;
        Ok(IssueChange::Subscriber {
            user: user.unwrap_or_else(|| ME.to_string()),
            add,
        })
    };
    let label = |add: bool| -> Result<IssueChange, String> {
        Ok(IssueChange::Label {
            label: required("label")?,
            add,
        })
    };
    let change = match action {
        "add_label" => label(true)?,
        "remove_label" => label(false)?,
        "add_relation" => relation(true)?,
        "remove_relation" => relation(false)?,
        "add_link" => IssueChange::AddLink {
            url: required("url")?,
            title: optional(action, args, "title", text, "a string")?,
        },
        "remove_link" => IssueChange::RemoveLink {
            url: required("url")?,
        },
        "add_subscriber" => subscriber(true)?,
        "remove_subscriber" => subscriber(false)?,
        "archive_issue" => IssueChange::Archive(Archival::Archive),
        "unarchive_issue" => IssueChange::Archive(Archival::Unarchive),
        "delete_issue" => IssueChange::Archive(Archival::Delete),
        _ => return Err(format!("no action `{action}`")),
    };
    let issue = handle_arg(action, args, "issue")?;
    Ok((ws, WriteCall::Issue { issue, change }))
}

/// What a `relation` argument must be, as a refusal says it.
const RELATIONS: &str = "blocks, blocked_by, related, similar, duplicate_of or duplicates";

/// The `T?` argument `param` of `action` read by `read`: [`Change::Keep`] when it is left out,
/// [`Change::Clear`] for `nil`, or the sentence refusing a value that is not `what`.
fn change<T>(
    action: &str,
    args: &Map<String, Value>,
    param: &str,
    read: impl Fn(&Value) -> Option<T>,
    what: &str,
) -> Result<Change<T>, String> {
    match args.get(param) {
        None => Ok(Change::Keep),
        Some(Value::Null) => Ok(Change::Clear),
        Some(value) => read(value)
            .map(Change::Set)
            .ok_or_else(|| format!("{action}: parameter {param} must be {what}")),
    }
}

/// A string argument.
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// The `files` argument of `action`, none when it is left out, or the sentence refusing one
/// that is not a list of strings.
fn files(action: &str, args: &Map<String, Value>) -> Result<Vec<String>, String> {
    strings(action, args, "files")
}

/// The list argument `param` of `action`, none when it is left out, or the sentence refusing
/// one that is not a list of strings.
fn strings(action: &str, args: &Map<String, Value>, param: &str) -> Result<Vec<String>, String> {
    let not_strings = || format!("{action}: parameter {param} must be a list of strings");
    let Some(items) = optional(action, args, param, Value::as_array, "a list of strings")? else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| text(item).ok_or_else(not_strings))
        .collect()
}

/// The files at `paths` read to be put up, each when the write is called and before any
/// request.
fn uploads(action: &str, paths: &[String]) -> Result<Vec<Upload>, String> {
    paths.iter().map(|path| read_upload(action, path)).collect()
}

/// Where `args`' `at` or `after` place the issue `action` files or changes, `None` when
/// neither is given, or the sentence refusing the two together or one of another type.
fn place(action: &str, args: &Map<String, Value>) -> Result<Option<Place>, String> {
    let at = optional(action, args, "at", member::<At>, "top or bottom")?;
    let after = match args.get("after") {
        None => None,
        Some(_) => Some(handle_arg(action, args, "after")?),
    };
    match (at, after) {
        (Some(_), Some(_)) => Err(format!("{action}: at and after cannot both be given")),
        (Some(at), None) => Ok(Some(Place::At(at))),
        (None, Some(after)) => Ok(Some(Place::After(after))),
        (None, None) => Ok(None),
    }
}

/// The `due` argument of `action`, as [`change`] reads it, or the sentence refusing a date
/// that is no day of the calendar.
fn due(action: &str, args: &Map<String, Value>) -> Result<Change<String>, String> {
    let due = change(action, args, "due", text, "a string")?;
    if let Change::Set(day) = &due {
        if !is_date(day) {
            return Err(format!("{action}: due {day:?} is no YYYY-MM-DD date"));
        }
    }
    Ok(due)
}

/// The value a `T?` argument of `create_issue` sets, `None` for `nil` as for one left out.
fn given<T>(change: Change<T>) -> Option<T> {
    match change {
        Change::Set(value) => Some(value),
        Change::Keep | Change::Clear => None,
    }
}

/// What `create_issue` is asked to file, as `args` say.
///
/// # Errors
///
/// Returns the sentence refusing an argument missing or of another type than the manifest's,
/// `at` beside `after`, a due date that is no day of the calendar, and a `milestone` with no
/// `project` to be in.
pub(crate) fn new_issue(action: &str, args: &Map<String, Value>) -> Result<NewIssue, String> {
    let string = |param: &str| optional(action, args, param, text, "a string");
    let required = |param: &str| {
        string(param)?.ok_or_else(|| format!("{action}: parameter {param} is required"))
    };
    let (team, title) = (required("team")?, required("title")?);
    let place = place(action, args)?;
    let due = given(due(action, args)?);
    let optional_string = |param: &str| change(action, args, param, text, "a string").map(given);
    let project = optional_string("project")?;
    let milestone = optional_string("milestone")?;
    if let (Some(milestone), None) = (&milestone, &project) {
        return Err(format!(
            "{action}: milestone {milestone:?} needs a project to be in, and none is given"
        ));
    }
    let labels = strings(action, args, "labels")?;
    let parent = match args.get("parent") {
        None | Some(Value::Null) => None,
        Some(_) => Some(handle_arg(action, args, "parent")?),
    };
    Ok(NewIssue {
        team,
        title,
        description: string("description")?,
        files: files(action, args)?,
        state: string("state")?,
        place,
        priority: optional(action, args, "priority", member, "a linear.Priority")?,
        estimate: given(change(action, args, "estimate", Value::as_i64, "an int")?),
        due,
        labels,
        assignee: optional_string("assignee")?,
        delegate: optional_string("delegate")?,
        project,
        milestone,
        cycle: given(change(action, args, "cycle", Value::as_i64, "an int")?),
        parent,
    })
}

/// What `update_issue` is asked to change, as `args` say.
///
/// # Errors
///
/// Returns the sentence refusing an argument of another type than the manifest's, a `team`
/// without a `state` of it beside it, `files` without a `description`, `at` beside `after`, a
/// due date that is no day of the calendar, and a `milestone` beside `project=nil`, which
/// leaves it no project to be in.
pub(crate) fn issue_update(action: &str, args: &Map<String, Value>) -> Result<IssueUpdate, String> {
    let string = |param: &str| optional(action, args, param, text, "a string");
    let files = files(action, args)?;
    let (description, state, team) = (string("description")?, string("state")?, string("team")?);
    if let (Some(team), None) = (&team, &state) {
        return Err(format!(
            "{action}: team {team} needs a state of team {team} beside it"
        ));
    }
    if !files.is_empty() && description.is_none() {
        return Err(format!("{action}: files go only beside description"));
    }
    let place = place(action, args)?;
    let due = due(action, args)?;
    let project = change(action, args, "project", text, "a string")?;
    let milestone = change(action, args, "milestone", text, "a string")?;
    if let (Change::Set(milestone), Change::Clear) = (&milestone, &project) {
        return Err(format!(
            "{action}: milestone {milestone:?} needs a project to be in, and project is nil"
        ));
    }
    let parent = match args.get("parent") {
        None => Change::Keep,
        Some(Value::Null) => Change::Clear,
        Some(_) => Change::Set(handle_arg(action, args, "parent")?),
    };
    Ok(IssueUpdate {
        issue: handle_arg(action, args, "issue")?,
        title: string("title")?,
        description,
        files,
        state,
        team,
        place,
        priority: optional(action, args, "priority", member, "a linear.Priority")?,
        estimate: change(action, args, "estimate", Value::as_i64, "an int")?,
        due,
        assignee: change(action, args, "assignee", text, "a string")?,
        delegate: change(action, args, "delegate", text, "a string")?,
        project,
        milestone,
        cycle: change(action, args, "cycle", Value::as_i64, "an int")?,
        parent,
    })
}

/// Do `call`, the write `action`, through `client`: its ok reply line, and the line that
/// narrates it.
fn run(
    action: &str,
    call: &WriteCall,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    match call {
        WriteCall::CreateIssue(new) => create_issue(action, new, client, ctx, diag),
        WriteCall::UpdateIssue(update) => update_issue(action, update, client, ctx, diag),
        WriteCall::Issue { issue, change } => {
            change_issue(action, issue, change, client, ctx, diag)
        }
        WriteCall::CreateComment(new) => create_comment(action, new, client, ctx, diag),
        WriteCall::Comment { comment, change } => {
            change_comment(action, comment, change, client, ctx, diag)
        }
    }
}

/// The uuid of the comment the handle `comment`, `action`'s argument `param`, names in the
/// workspace `organization`.
///
/// # Errors
///
/// Returns the sentence refusing a handle of another workspace than `organization`, naming
/// both url keys, and a key that names no comment this plugin handed out.
fn comment_uuid(
    action: &str,
    param: &str,
    comment: &HandleArg,
    organization: &Organization,
    ctx: &ReadContext<'_>,
) -> Result<String, String> {
    keyed(action, param, comment, organization, ctx).unwrap_or_else(|| {
        Err(format!(
            "{action}: {param} {} names no comment this plugin handed out",
            comment.key
        ))
    })
}

/// The ok reply carrying `comment`, keyed as `comments` keys it.
fn comment_reply(organization: &Organization, comment: OneComment) -> String {
    let handle = WireHandle {
        key: read_key(&organization.id, &comment.uuid),
        id: comment.uuid,
        fields: comment.fields,
    };
    json!({ "ok": true, "handle": handle }).to_string()
}

/// Post the comment `new`, and read it back: the issue and the comment it answers are checked
/// before a file goes up, and a reply to a reply goes in the thread both are in, the one
/// level of threads Linear keeps.
fn create_comment(
    action: &str,
    new: &NewComment,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    let linear = |e: LinearError| e.to_string();
    let uploads = uploads(action, &new.files)?;
    let organization = reach(client, ctx, diag)?;
    let uuid = resolve(action, "issue", &new.issue, &organization, ctx)?;
    let thread = match &new.reply_to {
        None => None,
        Some(reply_to) => {
            let answered = comment_uuid(action, "reply_to", reply_to, &organization, ctx)?;
            let answered = client.comment(&answered).map_err(linear)?;
            if answered.issue != uuid {
                return Err(format!(
                    "{action}: reply_to {} is on {}, not {}",
                    reply_to.id, answered.fields.issue, new.issue.id
                ));
            }
            Some(answered.fields.reply_to.unwrap_or(answered.uuid))
        }
    };
    let body = embedded(client, &new.body, &uploads).map_err(linear)?;
    let posted = client
        .comment_post(&uuid, &body, thread.as_deref())
        .map_err(linear)?;
    let posted = client.comment(&posted).map_err(linear)?;
    let what = match &thread {
        Some(first) => format!("replied to comment {first} on {}", posted.fields.issue),
        None => format!("commented on {}", posted.fields.issue),
    };
    Ok((
        comment_reply(&organization, posted),
        format!("{ACTION_TAG} {what} in workspace {}", organization.url_key),
    ))
}

/// Do what `change` says with the comment `comment`, and read back what it changed: the
/// comment edited, or the first comment of the thread it resolved or unresolved; a deleted
/// comment answers nil.
fn change_comment(
    action: &str,
    comment: &HandleArg,
    change: &CommentChange,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    let linear = |e: LinearError| e.to_string();
    let uploads = match change {
        CommentChange::Update { files, .. } => uploads(action, files)?,
        CommentChange::Resolve(_) | CommentChange::Delete => Vec::new(),
    };
    let organization = reach(client, ctx, diag)?;
    let uuid = comment_uuid(action, "comment", comment, &organization, ctx)?;
    let in_workspace = format!("in workspace {}", organization.url_key);
    let (changed, what) = match change {
        CommentChange::Update { body, .. } => {
            let body = embedded(client, body, &uploads).map_err(linear)?;
            client.edit_comment(&uuid, &body).map_err(linear)?;
            (uuid, "edited")
        }
        CommentChange::Resolve(on) => {
            // A thread is resolved on its first comment, by the reply that resolves it.
            let read = client.comment(&uuid).map_err(linear)?;
            let (first, resolving) = match read.fields.reply_to {
                Some(first) => (first, Some(uuid.as_str())),
                None => (uuid.clone(), None),
            };
            client
                .resolve_comment(&first, resolving, *on)
                .map_err(linear)?;
            (first, if *on { "resolved" } else { "unresolved" })
        }
        CommentChange::Delete => {
            client.delete_comment(&uuid).map_err(linear)?;
            return Ok((
                json!({ "ok": true }).to_string(),
                format!("{ACTION_TAG} deleted comment {uuid} {in_workspace}"),
            ));
        }
    };
    let read = client.comment(&changed).map_err(linear)?;
    let said = format!(
        "{ACTION_TAG} {what} comment {changed} on {} {in_workspace}",
        read.fields.issue
    );
    Ok((comment_reply(&organization, read), said))
}

/// Change the issue `issue` as `change` says, and read it back: every handle is checked, and a
/// name looked up, before anything changes, and the change is sent only when Linear does not
/// already hold it.
fn change_issue(
    action: &str,
    issue: &HandleArg,
    change: &IssueChange,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    let linear = |e: LinearError| e.to_string();
    let organization = reach(client, ctx, diag)?;
    let uuid = resolve(action, "issue", issue, &organization, ctx)?;
    // The words that narrate the write, before the issue's identifier and after it.
    let said = |before: String| (before, "");
    let what = match change {
        IssueChange::Label { label, add } => {
            let place = client.issue_place(&uuid).map_err(linear)?;
            let label_id = lookup::label(client, action, &place.team, label)?;
            let carried = client.label_ids(&uuid).map_err(linear)?.contains(&label_id);
            // Linear refuses to take off a label the issue does not carry.
            if *add != carried {
                client.issue_label(&uuid, &label_id, *add).map_err(linear)?;
            }
            said(match (add, carried) {
                (true, false) => format!("put label {label} on"),
                (false, true) => format!("took label {label} off"),
                (true, true) => format!("label {label} already on"),
                (false, false) => format!("label {label} not on"),
            })
        }
        IssueChange::Relation {
            relation,
            other,
            add,
        } => {
            let other_uuid = resolve(action, "other", other, &organization, ctx)?;
            if other_uuid == uuid {
                return Err(format!("{action}: other is {} itself", issue.id));
            }
            let held = held(client, &uuid, &other_uuid, *relation).map_err(linear)?;
            let (kind, holds) = relation.linear();
            let field = relation.name();
            match (add, held.is_empty()) {
                (true, true) => {
                    let (from, to) = if holds {
                        (&uuid, &other_uuid)
                    } else {
                        (&other_uuid, &uuid)
                    };
                    client.relation_create(from, to, kind).map_err(linear)?;
                    said(format!("added {} to the {field} of", other.id))
                }
                (false, false) => {
                    for id in &held {
                        client.relation_delete(id).map_err(linear)?;
                    }
                    said(format!("removed {} from the {field} of", other.id))
                }
                (true, false) => said(format!("{} already in the {field} of", other.id)),
                (false, true) => said(format!("{} not in the {field} of", other.id)),
            }
        }
        IssueChange::AddLink { url, title } => {
            let linked = client.links(&uuid).map_err(linear)?;
            match (linked.iter().find(|link| link.url == *url), title) {
                (None, _) => {
                    client
                        .link_url(&uuid, url, title.as_deref())
                        .map_err(linear)?;
                    said(format!("linked {url} on"))
                }
                (Some(link), Some(title)) => {
                    client.link_update(&link.id, title).map_err(linear)?;
                    said(format!("retitled the link to {url} on"))
                }
                (Some(_), None) => said(format!("{url} already linked on")),
            }
        }
        IssueChange::RemoveLink { url } => {
            let linked = client.links(&uuid).map_err(linear)?;
            let mut unlinked = false;
            for link in linked.iter().filter(|link| link.url == *url) {
                client.link_delete(&link.id).map_err(linear)?;
                unlinked = true;
            }
            if unlinked {
                said(format!("unlinked {url} from"))
            } else {
                said(format!("{url} not linked on"))
            }
        }
        IssueChange::Subscriber { user, add } => {
            let user_id = lookup::user(client, action, user)?;
            client.subscribe(&uuid, &user_id, *add).map_err(linear)?;
            said(if *add {
                format!("subscribed {user} to")
            } else {
                format!("unsubscribed {user} from")
            })
        }
        IssueChange::Archive(archival) => {
            let (done, what) = match archival {
                Archival::Archive => (client.archive_issue(&uuid), ("archived", "")),
                Archival::Unarchive => (client.unarchive_issue(&uuid), ("unarchived", "")),
                Archival::Delete => (client.delete_issue(&uuid), ("put", " in the trash")),
            };
            done.map_err(linear)?;
            (what.0.to_string(), what.1)
        }
    };
    let (reply, identifier) = issue_reply(client, &organization, &uuid)?;
    let what = format!("{} {identifier}{}", what.0, what.1);
    Ok((
        reply,
        format!("{ACTION_TAG} {what} in workspace {}", organization.url_key),
    ))
}

/// The ids of the relations that relate the issue `uuid` to the issue `other` as `relation`:
/// the ones it holds, the ones that name it, or for a relation that reads the same from either
/// end, both.
fn held(
    client: &LinearClient,
    uuid: &str,
    other: &str,
    relation: Relation,
) -> Result<Vec<String>, LinearError> {
    let (kind, holds) = relation.linear();
    let mut ids = Vec::new();
    for held in [true, false] {
        if held == holds || relation.symmetric() {
            let relations = client.relations(uuid, held)?;
            ids.extend(
                relations
                    .into_iter()
                    .filter(|r| r.kind == kind && r.other == other)
                    .map(|r| r.id),
            );
        }
    }
    Ok(ids)
}

/// The ok reply carrying the issue `uuid` as Linear now holds it, read as `issues` reads it,
/// and its identifier.
fn issue_reply(
    client: &LinearClient,
    organization: &Organization,
    uuid: &str,
) -> Result<(String, String), String> {
    let issue = client.issue_full(uuid).map_err(|e| e.to_string())?;
    let handle = WireHandle {
        id: issue.identifier.clone(),
        key: read_key(&organization.id, uuid),
        fields: IssueFields::of(&issue, &organization.url_key),
    };
    Ok((
        json!({ "ok": true, "handle": handle }).to_string(),
        issue.identifier,
    ))
}

/// File the issue `new`, and read it back: every handle is checked and every name looked up,
/// and the issue it goes after checked, before a file goes up or the issue is filed. Placed
/// `at` or `after` with no `state`, it is placed in the team's default state.
fn create_issue(
    action: &str,
    new: &NewIssue,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    let linear = |e: LinearError| e.to_string();
    let uploads = uploads(action, &new.files)?;
    let organization = reach(client, ctx, diag)?;
    let resolve =
        |param: &str, handle: &HandleArg| resolve(action, param, handle, &organization, ctx);
    let spot = match &new.place {
        None => None,
        Some(Place::At(At::Top)) => Some(Spot::Top),
        Some(Place::At(At::Bottom)) => Some(Spot::Bottom),
        Some(Place::After(after)) => Some(Spot::After(resolve("after", after)?)),
    };
    let parent = match &new.parent {
        Some(parent) => Change::Set(resolve("parent", parent)?),
        None => Change::Keep,
    };
    let team = lookup::team(client, action, &new.team)?;
    let state = match (&new.state, &spot) {
        (Some(name), _) => Some(lookup::state(client, action, &team, name)?),
        (None, Some(_)) => {
            let (id, name) = client
                .default_state(&team.id)
                .map_err(linear)?
                .ok_or_else(|| {
                    format!(
                        "{action}: team {} has no default state to place the issue in; name its \
                     state",
                        team.key
                    )
                })?;
            Some(Candidate {
                id,
                name,
                alias: None,
            })
        }
        (None, None) => None,
    };
    let mut labels: Vec<String> = Vec::new();
    for name in &new.labels {
        let id = lookup::label(client, action, &team, name)?;
        if !labels.contains(&id) {
            labels.push(id);
        }
    }
    let set = |value: Option<String>| value.map_or(Change::Keep, Change::Set);
    let user = |name: &Option<String>| -> Result<Change<String>, String> {
        Ok(set(name
            .as_ref()
            .map(|name| lookup::user(client, action, name))
            .transpose()?))
    };
    let project = new
        .project
        .as_ref()
        .map(|name| lookup::project(client, action, name))
        .transpose()?;
    let milestone = match (&new.milestone, &project) {
        (Some(name), Some(project)) => Some(lookup::milestone(client, action, project, name)?),
        _ => None,
    };
    let cycle = new
        .cycle
        .map(|number| lookup::cycle(client, action, &team, number))
        .transpose()?;
    let mut input = IssueInput {
        team: Some(team.id.clone()),
        state: state.as_ref().map(|state| state.id.clone()),
        title: Some(new.title.clone()),
        priority: new.priority,
        estimate: new.estimate.map_or(Change::Keep, Change::Set),
        due: set(new.due.clone()),
        assignee: user(&new.assignee)?,
        delegate: user(&new.delegate)?,
        project: set(project.map(|project| project.id)),
        milestone: set(milestone),
        cycle: set(cycle),
        parent,
        ..IssueInput::default()
    };
    if let (Some(spot), Some(state)) = (&spot, &state) {
        if let Spot::After(after) = spot {
            placed_after(action, client, after, &team, state)?;
        }
        let others = in_state(client, &team, &state.id, None).map_err(linear)?;
        input.sort_order = sort_order_for(spot, &others);
    }
    if new.description.is_some() || !uploads.is_empty() {
        let text = new.description.as_deref().unwrap_or_default();
        input.description = Some(embedded(client, text, &uploads).map_err(linear)?);
    }
    let mut input = input.input();
    if !labels.is_empty() {
        input.insert("labelIds".into(), json!(labels));
    }
    let uuid = client.issue_create(&input).map_err(linear)?;
    let (reply, identifier) = issue_reply(client, &organization, &uuid)?;
    Ok((
        reply,
        format!(
            "{ACTION_TAG} filed {identifier} in workspace {}",
            organization.url_key
        ),
    ))
}

/// Change the issue as `update` asks, and read it back: every handle is checked and every name
/// looked up before a file goes up or anything changes.
fn update_issue(
    action: &str,
    update: &IssueUpdate,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    let linear = |e: LinearError| e.to_string();
    let uploads = uploads(action, &update.files)?;
    let organization = reach(client, ctx, diag)?;
    let resolve =
        |param: &str, handle: &HandleArg| resolve(action, param, handle, &organization, ctx);
    let uuid = resolve("issue", &update.issue)?;
    let spot = match &update.place {
        None => None,
        Some(Place::At(At::Top)) => Some(Spot::Top),
        Some(Place::At(At::Bottom)) => Some(Spot::Bottom),
        Some(Place::After(after)) => Some(Spot::After(resolve("after", after)?)),
    };
    let parent = update.parent.try_map(|parent| resolve("parent", parent))?;
    let place = client.issue_place(&uuid).map_err(linear)?;
    let team = match &update.team {
        Some(key) => lookup::team(client, action, key)?,
        None => place.team.clone(),
    };
    let state = match &update.state {
        Some(name) => lookup::state(client, action, &team, name)?,
        None => Candidate {
            id: place.state.id.clone(),
            name: place.state.name.clone(),
            alias: None,
        },
    };
    let project = update
        .project
        .try_map(|name| lookup::project(client, action, name))?;
    let milestone = update.milestone.try_map(|name| {
        let in_project = match (&project, &place.project) {
            (Change::Set(project), _) => project.clone(),
            (Change::Keep, Some(id)) => lookup::project(client, action, id)?,
            _ => {
                return Err(format!(
                    "{action}: milestone {name:?} needs a project to be in, and {} is in none",
                    place.identifier
                ))
            }
        };
        lookup::milestone(client, action, &in_project, name)
    })?;
    let mut input = IssueInput {
        team: update.team.as_ref().map(|_| team.id.clone()),
        state: update.state.as_ref().map(|_| state.id.clone()),
        title: update.title.clone(),
        priority: update.priority,
        estimate: update.estimate.clone(),
        due: update.due.clone(),
        assignee: update
            .assignee
            .try_map(|name| lookup::user(client, action, name))?,
        delegate: update
            .delegate
            .try_map(|name| lookup::user(client, action, name))?,
        project: project.map(|project| project.id.clone()),
        milestone,
        cycle: update
            .cycle
            .try_map(|number| lookup::cycle(client, action, &team, *number))?,
        parent,
        ..IssueInput::default()
    };
    if let Some(spot) = &spot {
        if let Spot::After(after) = spot {
            if *after == uuid {
                return Err(format!("{action}: after is {} itself", place.identifier));
            }
            placed_after(action, client, after, &team, &state)?;
        }
        let others = in_state(client, &team, &state.id, Some(&uuid)).map_err(linear)?;
        input.sort_order = sort_order_for(spot, &others);
    }
    if let Some(text) = &update.description {
        input.description = Some(embedded(client, text, &uploads).map_err(linear)?);
    }
    let input = input.input();
    if !input.is_empty() {
        client.issue_write(&uuid, &input).map_err(linear)?;
    }
    let (reply, identifier) = issue_reply(client, &organization, &uuid)?;
    let what = if identifier == place.identifier {
        format!("updated {identifier}")
    } else {
        format!("moved {} to {identifier}", place.identifier)
    };
    Ok((
        reply,
        format!("{ACTION_TAG} {what} in workspace {}", organization.url_key),
    ))
}

/// Check that the issue `after` can be placed after: an issue in the state `state` of the team
/// `team` the issue goes to.
fn placed_after(
    action: &str,
    client: &LinearClient,
    after: &str,
    team: &Team,
    state: &Candidate,
) -> Result<(), String> {
    let other = client.issue_place(after).map_err(|e| e.to_string())?;
    if other.state.id != state.id {
        return Err(format!(
            "{action}: after {} is in {} of {}, not {} of {}",
            other.identifier, other.state.name, other.team.key, state.name, team.key
        ));
    }
    Ok(())
}

/// The uuids and sort orders of the issues in the state `state_id` of `team`, in manual order,
/// the issue `uuid` left out when one is given.
fn in_state(
    client: &LinearClient,
    team: &Team,
    state_id: &str,
    uuid: Option<&str>,
) -> Result<Vec<(String, f64)>, LinearError> {
    let mut issues = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = client.issues_in_state(&team.id, state_id, &json!({}), after.as_deref())?;
        issues.extend(
            page.items
                .into_iter()
                .filter(|issue| Some(issue.id.as_str()) != uuid)
                .map(|issue| (issue.id, issue.sort_order)),
        );
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(issues),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `update_issue`'s arguments as afkd binds them: a workspace, the issue, and `extra`.
    fn args(extra: Value) -> Map<String, Value> {
        let mut args = Map::from_iter([
            ("ws".to_string(), json!({"api_key": "lin_api_k"})),
            (
                "issue".to_string(),
                json!({"id": "ENG-101", "key": "linear:org/uuid-101"}),
            ),
        ]);
        args.extend(extra.as_object().expect("an args object").clone());
        args
    }

    fn update(extra: Value) -> Result<IssueUpdate, String> {
        decode("update_issue", &args(extra)).map(|(_, call)| match call {
            WriteCall::UpdateIssue(update) => *update,
            other => panic!("update_issue decodes to an update, not {other:?}"),
        })
    }

    fn handle(id: &str) -> HandleArg {
        HandleArg {
            id: id.into(),
            key: format!("linear:org/{id}"),
        }
    }

    /// A `T?` parameter left out leaves its field, `nil` clears it, and a value sets it; the
    /// others are set when given, and `at` and `after` each place the issue.
    #[test]
    fn update_issue_keeps_what_is_left_out_and_clears_each_nil() {
        assert_eq!(
            update(json!({})),
            Ok(IssueUpdate {
                issue: HandleArg {
                    id: "ENG-101".into(),
                    key: "linear:org/uuid-101".into()
                },
                ..IssueUpdate::default()
            })
        );
        let cleared = update(json!({
            "estimate": null, "due": null, "assignee": null, "delegate": null, "project": null,
            "milestone": null, "cycle": null, "parent": null,
        }))
        .unwrap();
        assert_eq!(
            (
                cleared.estimate,
                cleared.due,
                cleared.assignee,
                cleared.delegate
            ),
            (Change::Clear, Change::Clear, Change::Clear, Change::Clear)
        );
        assert_eq!(
            (
                cleared.project,
                cleared.milestone,
                cleared.cycle,
                cleared.parent
            ),
            (Change::Clear, Change::Clear, Change::Clear, Change::Clear)
        );
        let set = update(json!({
            "title": "修复 the retry storm 🚨", "description": "", "files": ["/srv/a b/shot.png"],
            "state": "In Progress", "team": "OPS", "at": "top", "priority": "urgent",
            "estimate": 0, "due": "2024-02-29", "assignee": "chen@acme.example",
            "delegate": "me", "project": "Trackers 🧭", "milestone": "B — reads", "cycle": 12,
            "parent": {"id": "ENG-100", "key": "linear:org/ENG-100"},
        }))
        .unwrap();
        assert_eq!(
            set,
            IssueUpdate {
                issue: set.issue.clone(),
                title: Some("修复 the retry storm 🚨".into()),
                description: Some(String::new()),
                files: vec!["/srv/a b/shot.png".into()],
                state: Some("In Progress".into()),
                team: Some("OPS".into()),
                place: Some(Place::At(At::Top)),
                priority: Some(Priority::Urgent),
                estimate: Change::Set(0),
                due: Change::Set("2024-02-29".into()),
                assignee: Change::Set("chen@acme.example".into()),
                delegate: Change::Set("me".into()),
                project: Change::Set("Trackers 🧭".into()),
                milestone: Change::Set("B — reads".into()),
                cycle: Change::Set(12),
                parent: Change::Set(handle("ENG-100")),
            }
        );
        assert_eq!(
            update(json!({"after": {"id": "ENG-9", "key": "linear:org/ENG-9"}}))
                .unwrap()
                .place,
            Some(Place::After(handle("ENG-9")))
        );
        assert_eq!(
            update(json!({"at": "bottom"})).unwrap().place,
            Some(Place::At(At::Bottom))
        );
    }

    /// Each argument `update_issue` cannot use is refused in its own sentence, before any
    /// request: `team` alone, `files` alone, `at` beside `after`, a due date that is no day, a
    /// `milestone` beside `project=nil`, and a value of another type.
    #[test]
    fn update_issue_refuses_what_it_cannot_use() {
        let after = json!({"id": "ENG-9", "key": "linear:org/ENG-9"});
        for (extra, refusal) in [
            (
                json!({"team": "OPS"}),
                "update_issue: team OPS needs a state of team OPS beside it",
            ),
            (
                json!({"files": ["/tmp/shot.png"]}),
                "update_issue: files go only beside description",
            ),
            (
                json!({"files": ["/tmp/shot.png"], "title": "t"}),
                "update_issue: files go only beside description",
            ),
            (
                json!({"at": "top", "after": after}),
                "update_issue: at and after cannot both be given",
            ),
            (
                json!({"due": "2026-02-29"}),
                "update_issue: due \"2026-02-29\" is no YYYY-MM-DD date",
            ),
            (
                json!({"milestone": "B", "project": null}),
                "update_issue: milestone \"B\" needs a project to be in, and project is nil",
            ),
            (
                json!({"at": "middle"}),
                "update_issue: parameter at must be top or bottom",
            ),
            (
                json!({"priority": 1}),
                "update_issue: parameter priority must be a linear.Priority",
            ),
            (
                json!({"estimate": "3"}),
                "update_issue: parameter estimate must be an int",
            ),
            (
                json!({"files": "/tmp/shot.png", "description": ""}),
                "update_issue: parameter files must be a list of strings",
            ),
            (
                json!({"parent": "ENG-1"}),
                "update_issue: parameter parent must be an item handle",
            ),
            (
                json!({"title": null}),
                "update_issue: parameter title must be a string",
            ),
        ] {
            assert_eq!(update(extra.clone()), Err(refusal.to_string()), "{extra}");
        }
        assert_eq!(
            decode("update_issue", &Map::new()).map(|_| ()),
            Err("update_issue: parameter ws must be a linear.Workspace".into())
        );
        let mut no_issue = args(json!({}));
        no_issue.remove("issue");
        assert_eq!(
            decode("update_issue", &no_issue).map(|_| ()),
            Err("update_issue: parameter issue is required".into())
        );
        assert!(
            update(json!({"files": [], "title": "t"})).is_ok(),
            "no files"
        );
    }

    /// The input holds a key for each field set and `null` for each cleared, and none for a
    /// field left as it is.
    #[test]
    fn the_input_carries_only_what_changes() {
        assert_eq!(IssueInput::default().input(), Map::new());
        let title = IssueInput {
            title: Some("New".into()),
            ..IssueInput::default()
        };
        assert_eq!(Value::Object(title.input()), json!({"title": "New"}));
        let every = IssueInput {
            team: Some("t".into()),
            state: Some("s".into()),
            sort_order: Some(-1000.0),
            title: Some("修复 🚨".into()),
            description: Some(String::new()),
            priority: Some(Priority::Low),
            estimate: Change::Set(0),
            due: Change::Set("2026-12-24".into()),
            assignee: Change::Set("u".into()),
            delegate: Change::Set("a".into()),
            project: Change::Set("p".into()),
            milestone: Change::Set("m".into()),
            cycle: Change::Set("c".into()),
            parent: Change::Set("i".into()),
        };
        assert_eq!(
            Value::Object(every.input()),
            json!({"teamId": "t", "stateId": "s", "sortOrder": -1000.0, "title": "修复 🚨",
                   "description": "", "priority": 4, "estimate": 0, "dueDate": "2026-12-24",
                   "assigneeId": "u", "delegateId": "a", "projectId": "p",
                   "projectMilestoneId": "m", "cycleId": "c", "parentId": "i"})
        );
        let cleared = IssueInput {
            estimate: Change::Clear,
            due: Change::Clear,
            assignee: Change::Clear,
            delegate: Change::Clear,
            project: Change::Clear,
            milestone: Change::Clear,
            cycle: Change::Clear,
            parent: Change::Clear,
            ..IssueInput::default()
        };
        assert_eq!(
            Value::Object(cleared.input()),
            json!({"estimate": null, "dueDate": null, "assigneeId": null, "delegateId": null,
                   "projectId": null, "projectMilestoneId": null, "cycleId": null,
                   "parentId": null})
        );
    }

    /// `at` places an issue above the first or below the last by the gap, `after` halfway to
    /// the issue below or below the last by the gap, and nothing is placed against a state
    /// that holds no other issue or an `after` it does not hold.
    #[test]
    fn an_issue_is_placed_against_its_neighbours() {
        let others: Vec<(String, f64)> = [("a", -500.0), ("b", 0.0), ("c", 1500.5), ("d", 1500.5)]
            .iter()
            .map(|(id, order)| (id.to_string(), *order))
            .collect();
        let after = |id: &str| Spot::After(id.to_string());
        for (spot, order) in [
            (Spot::Top, Some(-1500.0)),
            (Spot::Bottom, Some(2500.5)),
            (after("a"), Some(-250.0)),
            (after("b"), Some(750.25)),
            (after("c"), Some(1500.5)),
            (after("d"), Some(2500.5)),
            (after("z"), None),
        ] {
            assert_eq!(sort_order_for(&spot, &others), order, "{spot:?}");
        }
        for spot in [Spot::Top, Spot::Bottom, after("a")] {
            assert_eq!(sort_order_for(&spot, &[]), None, "{spot:?}");
        }
    }

    /// `create_issue`'s arguments as afkd binds them: a workspace, the team, the title, and
    /// `extra`.
    fn new(extra: Value) -> Result<NewIssue, String> {
        let mut args = Map::from_iter([
            ("ws".to_string(), json!({"api_key": "lin_api_k"})),
            ("team".to_string(), json!("ENG")),
            ("title".to_string(), json!("修复 the retry storm 🚨")),
        ]);
        args.extend(extra.as_object().expect("an args object").clone());
        decode("create_issue", &args).map(|(_, call)| match call {
            WriteCall::CreateIssue(new) => *new,
            other => panic!("create_issue decodes to a new issue, not {other:?}"),
        })
    }

    /// `create_issue` takes the team and the title, and each other field given; a `T?` one
    /// passed `nil` is left empty, as one left out is, and `at` and `after` each place it.
    #[test]
    fn create_issue_decodes_what_it_files() {
        let bare = NewIssue {
            team: "ENG".into(),
            title: "修复 the retry storm 🚨".into(),
            ..NewIssue::default()
        };
        assert_eq!(new(json!({})), Ok(bare.clone()));
        assert_eq!(
            new(json!({
                "estimate": null, "due": null, "assignee": null, "delegate": null,
                "project": null, "milestone": null, "cycle": null, "parent": null,
            })),
            Ok(bare.clone()),
            "nil leaves each empty"
        );
        let set = new(json!({
            "description": "## Steps\n\n1. run it", "files": ["/srv/a b/shot.png"],
            "state": "In Progress", "at": "bottom", "priority": "high", "estimate": 0,
            "due": "2028-02-29", "labels": ["Bug 🐛", "Needs Info"],
            "assignee": "chen@acme.example", "delegate": "me", "project": "Trackers 🧭",
            "milestone": "B — reads", "cycle": 12,
            "parent": {"id": "ENG-100", "key": "linear:org/ENG-100"},
        }));
        assert_eq!(
            set,
            Ok(NewIssue {
                description: Some("## Steps\n\n1. run it".into()),
                files: vec!["/srv/a b/shot.png".into()],
                state: Some("In Progress".into()),
                place: Some(Place::At(At::Bottom)),
                priority: Some(Priority::High),
                estimate: Some(0),
                due: Some("2028-02-29".into()),
                labels: vec!["Bug 🐛".into(), "Needs Info".into()],
                assignee: Some("chen@acme.example".into()),
                delegate: Some("me".into()),
                project: Some("Trackers 🧭".into()),
                milestone: Some("B — reads".into()),
                cycle: Some(12),
                parent: Some(handle("ENG-100")),
                ..bare
            })
        );
        assert_eq!(
            new(json!({"after": {"id": "ENG-9", "key": "linear:org/ENG-9"}}))
                .unwrap()
                .place,
            Some(Place::After(handle("ENG-9")))
        );
        assert_eq!(
            new(json!({"files": ["/tmp/shot.png"]})).unwrap().files,
            ["/tmp/shot.png"],
            "files without a description"
        );
    }

    /// Each argument `create_issue` cannot use is refused in its own sentence, before any
    /// request: `at` beside `after`, a due date that is no day, a `milestone` with no
    /// `project`, a team or a title left out, and a value of another type.
    #[test]
    fn create_issue_refuses_what_it_cannot_use() {
        let after = json!({"id": "ENG-9", "key": "linear:org/ENG-9"});
        for (extra, refusal) in [
            (
                json!({"at": "top", "after": after}),
                "create_issue: at and after cannot both be given",
            ),
            (
                json!({"due": "2026-02-29"}),
                "create_issue: due \"2026-02-29\" is no YYYY-MM-DD date",
            ),
            (
                json!({"milestone": "B"}),
                "create_issue: milestone \"B\" needs a project to be in, and none is given",
            ),
            (
                json!({"milestone": "B", "project": null}),
                "create_issue: milestone \"B\" needs a project to be in, and none is given",
            ),
            (
                json!({"team": null}),
                "create_issue: parameter team must be a string",
            ),
            (
                json!({"labels": "Bug"}),
                "create_issue: parameter labels must be a list of strings",
            ),
            (
                json!({"labels": [7]}),
                "create_issue: parameter labels must be a list of strings",
            ),
            (
                json!({"priority": "p1"}),
                "create_issue: parameter priority must be a linear.Priority",
            ),
            (
                json!({"cycle": "12"}),
                "create_issue: parameter cycle must be an int",
            ),
            (
                json!({"parent": "ENG-1"}),
                "create_issue: parameter parent must be an item handle",
            ),
        ] {
            assert_eq!(new(extra.clone()), Err(refusal.to_string()), "{extra}");
        }
        let mut no_title = Map::new();
        no_title.insert("ws".into(), json!({"api_key": "lin_api_k"}));
        no_title.insert("team".into(), json!("ENG"));
        assert_eq!(
            decode("create_issue", &no_title).map(|_| ()),
            Err("create_issue: parameter title is required".into())
        );
    }

    /// The write `action` decoded from the workspace, the issue and `extra`.
    fn issue_write(action: &str, extra: Value) -> Result<(HandleArg, IssueChange), String> {
        decode(action, &args(extra)).map(|(_, call)| match call {
            WriteCall::Issue { issue, change } => (issue, change),
            other => panic!("{action} decodes to an issue change, not {other:?}"),
        })
    }

    /// Each issue write decodes the issue and what it changes: a label, a relation and the
    /// other issue, a link's url and its title or none, a subscriber named or `"me"` when left
    /// out, and what it does with the issue.
    #[test]
    fn the_issue_writes_decode_what_they_change() {
        let other = json!({"id": "ENG-9", "key": "linear:org/ENG-9"});
        let url = "https://github.com/acme/widgets/pull/42?tab=files#diff-修复";
        for (action, extra, change) in [
            (
                "add_label",
                json!({"label": "Bug 🐛"}),
                IssueChange::Label {
                    label: "Bug 🐛".into(),
                    add: true,
                },
            ),
            (
                "remove_label",
                json!({"label": "Needs Info"}),
                IssueChange::Label {
                    label: "Needs Info".into(),
                    add: false,
                },
            ),
            (
                "add_relation",
                json!({"relation": "duplicate_of", "other": other}),
                IssueChange::Relation {
                    relation: Relation::DuplicateOf,
                    other: handle("ENG-9"),
                    add: true,
                },
            ),
            (
                "remove_relation",
                json!({"relation": "blocked_by", "other": other}),
                IssueChange::Relation {
                    relation: Relation::BlockedBy,
                    other: handle("ENG-9"),
                    add: false,
                },
            ),
            (
                "add_link",
                json!({"url": url, "title": "PR #42 — \"backoff\" 🚦"}),
                IssueChange::AddLink {
                    url: url.into(),
                    title: Some("PR #42 — \"backoff\" 🚦".into()),
                },
            ),
            (
                "add_link",
                json!({"url": url}),
                IssueChange::AddLink {
                    url: url.into(),
                    title: None,
                },
            ),
            (
                "remove_link",
                json!({"url": url}),
                IssueChange::RemoveLink { url: url.into() },
            ),
            (
                "add_subscriber",
                json!({}),
                IssueChange::Subscriber {
                    user: "me".into(),
                    add: true,
                },
            ),
            (
                "remove_subscriber",
                json!({"user": "陳大文"}),
                IssueChange::Subscriber {
                    user: "陳大文".into(),
                    add: false,
                },
            ),
            (
                "archive_issue",
                json!({}),
                IssueChange::Archive(Archival::Archive),
            ),
            (
                "unarchive_issue",
                json!({}),
                IssueChange::Archive(Archival::Unarchive),
            ),
            (
                "delete_issue",
                json!({}),
                IssueChange::Archive(Archival::Delete),
            ),
        ] {
            let issue = HandleArg {
                id: "ENG-101".into(),
                key: "linear:org/uuid-101".into(),
            };
            assert_eq!(issue_write(action, extra), Ok((issue, change)), "{action}");
        }
    }

    /// An issue write refuses an argument missing or of another type than the manifest's, in a
    /// sentence of its own, before any request.
    #[test]
    fn the_issue_writes_refuse_what_they_cannot_use() {
        let other = json!({"id": "ENG-9", "key": "linear:org/ENG-9"});
        for (action, extra, refusal) in [
            (
                "add_label",
                json!({}),
                "add_label: parameter label is required",
            ),
            (
                "remove_label",
                json!({"label": ["Bug"]}),
                "remove_label: parameter label must be a string",
            ),
            (
                "add_relation",
                json!({"other": other}),
                "add_relation: parameter relation is required",
            ),
            (
                "add_relation",
                json!({"relation": "duplicate", "other": other}),
                "add_relation: parameter relation must be blocks, blocked_by, related, similar, \
                 duplicate_of or duplicates",
            ),
            (
                "remove_relation",
                json!({"relation": "related"}),
                "remove_relation: parameter other is required",
            ),
            ("add_link", json!({}), "add_link: parameter url is required"),
            (
                "add_link",
                json!({"url": "https://example.com", "title": 7}),
                "add_link: parameter title must be a string",
            ),
            (
                "remove_link",
                json!({"url": null}),
                "remove_link: parameter url must be a string",
            ),
            (
                "add_subscriber",
                json!({"user": ["me"]}),
                "add_subscriber: parameter user must be a string",
            ),
            (
                "archive_issue",
                json!({"issue": "ENG-1"}),
                "archive_issue: parameter issue must be an item handle",
            ),
        ] {
            assert_eq!(
                issue_write(action, extra),
                Err(refusal.to_string()),
                "{action}"
            );
        }
        assert_eq!(
            decode("unarchive_issue", &Map::new()).map(|_| ()),
            Err("unarchive_issue: parameter ws must be a linear.Workspace".into())
        );
    }

    /// A comment write's arguments as afkd binds them: a workspace, the comment, and `extra`.
    fn comment_write(action: &str, extra: Value) -> Result<WriteCall, String> {
        let mut args = Map::from_iter([
            ("ws".to_string(), json!({"api_key": "lin_api_k"})),
            (
                "comment".to_string(),
                json!({"id": "c-1", "key": "linear:org/c-1"}),
            ),
        ]);
        args.extend(extra.as_object().expect("an args object").clone());
        decode(action, &args).map(|(_, call)| call)
    }

    /// `create_comment` decodes the issue, the body, the files and the comment it answers;
    /// each other comment write the comment and what it does with it.
    #[test]
    fn the_comment_writes_decode_what_they_write() {
        let body = "看起来不对 🚨\n\n```\nretry 1\nretry 2\n```";
        assert_eq!(
            decode(
                "create_comment",
                &args(json!({"body": body, "files": ["/srv/a b/shot.png"],
                             "reply_to": {"id": "c-1", "key": "linear:org/c-1"}}))
            )
            .map(|(_, call)| call),
            Ok(WriteCall::CreateComment(NewComment {
                issue: HandleArg {
                    id: "ENG-101".into(),
                    key: "linear:org/uuid-101".into(),
                },
                body: body.into(),
                files: vec!["/srv/a b/shot.png".into()],
                reply_to: Some(handle("c-1")),
            }))
        );
        assert_eq!(
            decode("create_comment", &args(json!({"body": ""}))).map(|(_, call)| call),
            Ok(WriteCall::CreateComment(NewComment {
                issue: HandleArg {
                    id: "ENG-101".into(),
                    key: "linear:org/uuid-101".into(),
                },
                body: String::new(),
                files: Vec::new(),
                reply_to: None,
            }))
        );
        for (action, extra, change) in [
            (
                "update_comment",
                json!({"body": body, "files": ["/tmp/run.log"]}),
                CommentChange::Update {
                    body: body.into(),
                    files: vec!["/tmp/run.log".into()],
                },
            ),
            ("resolve_comment", json!({}), CommentChange::Resolve(true)),
            (
                "unresolve_comment",
                json!({}),
                CommentChange::Resolve(false),
            ),
            ("delete_comment", json!({}), CommentChange::Delete),
        ] {
            assert_eq!(
                comment_write(action, extra),
                Ok(WriteCall::Comment {
                    comment: handle("c-1"),
                    change
                }),
                "{action}"
            );
        }
    }

    /// A comment write refuses an argument missing or of another type than the manifest's, in
    /// a sentence of its own, before any request.
    #[test]
    fn the_comment_writes_refuse_what_they_cannot_use() {
        for (action, extra, refusal) in [
            (
                "create_comment",
                json!({}),
                "create_comment: parameter body is required",
            ),
            (
                "create_comment",
                json!({"body": "x", "files": [7]}),
                "create_comment: parameter files must be a list of strings",
            ),
            (
                "create_comment",
                json!({"body": "x", "reply_to": "c-1"}),
                "create_comment: parameter reply_to must be an item handle",
            ),
            (
                "update_comment",
                json!({"body": null}),
                "update_comment: parameter body must be a string",
            ),
            (
                "update_comment",
                json!({"comment": null, "body": "x"}),
                "update_comment: parameter comment must be an item handle",
            ),
        ] {
            let decoded = if action == "create_comment" {
                decode(action, &args(extra)).map(|(_, call)| call)
            } else {
                comment_write(action, extra)
            };
            assert_eq!(decoded, Err(refusal.to_string()), "{action}");
        }
        let mut args = args(json!({}));
        args.remove("issue");
        assert_eq!(
            decode("create_comment", &args).map(|_| ()),
            Err("create_comment: parameter issue is required".into())
        );
        assert_eq!(
            decode("delete_comment", &Map::new()).map(|_| ()),
            Err("delete_comment: parameter ws must be a linear.Workspace".into())
        );
        let mut no_comment = Map::new();
        no_comment.insert("ws".into(), json!({"api_key": "lin_api_k"}));
        assert_eq!(
            decode("resolve_comment", &no_comment).map(|_| ()),
            Err("resolve_comment: parameter comment is required".into())
        );
    }

    /// Each relation is Linear's, held by the issue or by the other: `blocked_by` and
    /// `duplicates` are the other's `blocks` and `duplicate`, and only `related` and `similar`
    /// read the same from either end.
    #[test]
    fn each_relation_is_linears_held_by_one_end() {
        let table: Vec<(&str, &str, bool, bool)> = Relation::ALL
            .iter()
            .map(|r| (r.name(), r.linear().0, r.linear().1, r.symmetric()))
            .collect();
        assert_eq!(
            table,
            [
                ("blocks", "blocks", true, false),
                ("blocked_by", "blocks", false, false),
                ("related", "related", true, true),
                ("similar", "similar", true, true),
                ("duplicate_of", "duplicate", true, false),
                ("duplicates", "duplicate", false, false),
            ]
        );
    }
}
