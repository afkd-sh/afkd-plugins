//! The **workspace reads** a config calls with a `linear.Workspace` first: one issue by its
//! identifier or its uuid, the issues a set of filters keeps, an issue's thread and the files it links, the viewer, the users, teams,
//! states, labels, projects and cycles, and one file downloaded.
//!
//! A read reaches Linear only through the workspace it is passed ([`Workspace`]): it reads no
//! service's settings, and the kind's own client is never used for it. So it answers the same
//! in a child afkd starts for the one call as in a `service(linear)`'s trigger child, where a
//! slot may also pass it the issue that service claimed ([`ReadContext::claimed`]). Each read
//!
//! 1. decodes its arguments, a refusal sent before any request ([`decode`]);
//! 2. reads the workspace's organization, and records its url key in the plugin's directory;
//! 3. checks that each handle it was passed is of that workspace ([`resolve`]);
//! 4. reads what it reads, each list to its end;
//! 5. answers with handles keyed `linear:<organization uuid>/<object uuid>`, a value, or
//!    nothing, in one reply under afkd's 8 MiB for a call ([`fit_call`]).
//!
//! A read is never queued: it is done now, a request Linear could not take tried again
//! within the call's deadline, or refused with the reason.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::client::{issue_list_vars, LinearClient};
use crate::common::{write_whole, Diag};
use crate::file::linked_issue_files;
use crate::issue::ACTION_TAG;
use crate::rfc3339::format_utc;
use crate::settings::{ME, NONE};
use crate::tracker::{LinearError, Organization, Priority, StateType, Tracker};
use crate::wire::{CommentFields, IssueFields, StateFields, WireHandle, MAX_CALL_REPLY};
use crate::workspace::{
    other_workspace, parse_read_key, read_key, record_organization, recorded_url_key, Workspace,
};

/// The reads, in the order the manifest declares them, before the writes
/// ([`WRITES`](crate::writes::WRITES)).
pub(crate) const READS: &[&str] = &[
    "issue",
    "issues",
    "comments",
    "me",
    "users",
    "teams",
    "states",
    "labels",
    "projects",
    "cycles",
    "files",
    "download_file",
];

/// How many issues `issues` reads when its `limit` is left out.
const DEFAULT_LIMIT: i64 = 50;

/// The most issues `issues` reads: Linear's largest page.
const MAX_LIMIT: i64 = 250;

/// The most keys `issues` orders by: Linear refuses a `sort` of more than three elements
/// before it reads anything.
const MAX_ORDER: usize = 3;

/// The time filters of `issues`, each with the field of Linear's it compares and how.
const TIME_FILTERS: &[(&str, &str, &str)] = &[
    ("created_after", "createdAt", "gt"),
    ("created_before", "createdAt", "lt"),
    ("updated_after", "updatedAt", "gt"),
    ("updated_before", "updatedAt", "lt"),
    ("completed_after", "completedAt", "gt"),
    ("completed_before", "completedAt", "lt"),
    ("canceled_after", "canceledAt", "gt"),
    ("canceled_before", "canceledAt", "lt"),
];

/// The due-date filters of `issues`, each with how it compares the due date.
const DUE_FILTERS: &[(&str, &str)] = &[("due_after", "gt"), ("due_before", "lt")];

/// A key of `issues`'s `order`, as `linear.Sort` names it: a key of Linear's own `sort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Sort {
    Manual,
    Team,
    State,
    Priority,
    Assignee,
    Project,
    Cycle,
    Due,
    Estimate,
    Title,
    Created,
    Updated,
    Completed,
}

impl Sort {
    /// Every key, in the manifest's order, for the tests that hold each to the manifest and to
    /// Linear's schema.
    #[cfg(test)]
    pub(crate) const ALL: [Sort; 13] = [
        Sort::Manual,
        Sort::Team,
        Sort::State,
        Sort::Priority,
        Sort::Assignee,
        Sort::Project,
        Sort::Cycle,
        Sort::Due,
        Sort::Estimate,
        Sort::Title,
        Sort::Created,
        Sort::Updated,
        Sort::Completed,
    ];

    /// The order `issues` reads in when its `order` is left out: by team, then by state, then
    /// top to bottom as Linear shows a state.
    pub(crate) const DEFAULT: [Sort; 3] = [Sort::Team, Sort::State, Sort::Manual];

    /// The element of Linear's `sort` this key is. `manual` reads top to bottom, a date
    /// newest first, and every other key as Linear orders it, which only the documented
    /// default `nulls` is sent for.
    pub(crate) fn input(self) -> Value {
        let linear = json!({ "nulls": "last" });
        let newest = json!({ "order": "Descending" });
        match self {
            Sort::Manual => json!({ "manual": { "order": "Ascending" } }),
            Sort::Team => json!({ "team": linear }),
            Sort::State => json!({ "workflowState": linear }),
            Sort::Priority => json!({ "priority": linear }),
            Sort::Assignee => json!({ "assignee": linear }),
            Sort::Project => json!({ "project": linear }),
            Sort::Cycle => json!({ "cycle": linear }),
            Sort::Estimate => json!({ "estimate": linear }),
            Sort::Title => json!({ "title": linear }),
            Sort::Due => json!({ "dueDate": newest }),
            Sort::Created => json!({ "createdAt": newest }),
            Sort::Updated => json!({ "updatedAt": newest }),
            Sort::Completed => json!({ "completedAt": newest }),
        }
    }
}

/// An issue an `id` or a `parent` filter names: by its uuid, or by its identifier, its team's
/// key and its number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssueRef {
    Uuid(String),
    Identifier { team: String, number: u64 },
}

/// What `issues` keeps and how it reads them, as its arguments say. A filter left out is
/// `None`, and keeps every issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueQuery {
    pub(crate) id: Option<Vec<IssueRef>>,
    pub(crate) team: Option<Vec<String>>,
    pub(crate) state: Option<Vec<String>>,
    pub(crate) state_type: Option<Vec<StateType>>,
    pub(crate) priority: Option<Vec<Priority>>,
    pub(crate) labels: Option<Vec<String>>,
    pub(crate) without_labels: Option<Vec<String>>,
    pub(crate) assignee: Option<Vec<String>>,
    pub(crate) delegate: Option<Vec<String>>,
    pub(crate) project: Option<Vec<String>>,
    pub(crate) milestone: Option<Vec<String>>,
    pub(crate) cycle: Option<Vec<i64>>,
    pub(crate) parent: Option<Vec<IssueRef>>,
    pub(crate) blocked: Option<bool>,
    pub(crate) text: Option<String>,
    /// Each time filter given: Linear's field, how it compares, and the time as RFC 3339.
    pub(crate) times: Vec<(&'static str, &'static str, String)>,
    /// Each due-date filter given: how it compares, and the date.
    pub(crate) due: Vec<(&'static str, String)>,
    pub(crate) include_archived: bool,
    pub(crate) order: Vec<Sort>,
    pub(crate) limit: usize,
}

/// A handle a read was passed: the id and the key it carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HandleArg {
    pub(crate) id: String,
    pub(crate) key: String,
}

/// One read, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadCall {
    /// `issue`: the issue whose identifier or uuid `id` is.
    Issue {
        id: String,
    },
    Issues(Box<IssueQuery>),
    Comments(HandleArg),
    Me,
    Users,
    Teams,
    States {
        team: String,
    },
    Labels {
        team: Option<String>,
    },
    Projects {
        team: Option<String>,
    },
    Cycles {
        team: String,
    },
    Files(HandleArg),
    DownloadFile {
        url: String,
        to: String,
    },
}

/// An issue a `service(linear)` claimed, which a slot of that service may pass a read: its
/// uuid, and the workspace the service reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claimed {
    pub(crate) uuid: String,
    pub(crate) organization: Organization,
}

/// What a read, or a write ([`crate::writes`]), needs of the child it runs in.
pub(crate) struct ReadContext<'a> {
    /// The plugin's own directory, when afkd sent one.
    pub(crate) state_dir: Option<&'a Path>,
    /// When the call must be answered by.
    pub(crate) deadline: Instant,
    /// The claimed issue whose handle carries a key, in a trigger child; never one in a
    /// child for one call.
    pub(crate) claimed: &'a dyn Fn(&str) -> Option<Claimed>,
}

/// Do the read `action` with `args`, and answer it: the reply line, `ok` with what it read,
/// or `ok:false` with the sentence refusing it, which is diagnosed too. A read that lands is
/// narrated.
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

/// `line`, the ok reply to `action`, when it fits the 8 MiB afkd reads of a call's reply; or
/// the sentence saying it does not, and how to read less.
///
/// # Errors
///
/// Returns the sentence when the line runs past [`MAX_CALL_REPLY`].
pub(crate) fn fit_call(action: &str, line: String) -> Result<String, String> {
    if line.len() <= MAX_CALL_REPLY {
        return Ok(line);
    }
    let fewer = if action == "issues" {
        "ask for fewer with limit"
    } else {
        "read fewer"
    };
    Err(format!(
        "{action}: the reply comes to {} bytes, over afkd's 8 MiB for one call; {fewer}",
        line.len()
    ))
}

/// The workspace and the read `args` write for `action`, or the sentence refusing them.
///
/// # Errors
///
/// Returns the sentence for a `ws` that is no workspace or holds no one credential, an
/// argument missing or of another type than the manifest's, and the refusals of
/// [`issue_query`].
pub(crate) fn decode(
    action: &str,
    args: &Map<String, Value>,
) -> Result<(Workspace, ReadCall), String> {
    let ws = Workspace::from_arg(action, args.get("ws"))?;
    let team = || optional(action, args, "team", Value::as_str, "a string");
    let call = match action {
        "issue" => ReadCall::Issue {
            id: string(action, args, "id")?,
        },
        "issues" => ReadCall::Issues(Box::new(issue_query(args)?)),
        "comments" => ReadCall::Comments(handle_arg(action, args, "issue")?),
        "me" => ReadCall::Me,
        "users" => ReadCall::Users,
        "teams" => ReadCall::Teams,
        "states" => ReadCall::States {
            team: string(action, args, "team")?,
        },
        "labels" => ReadCall::Labels {
            team: team()?.map(str::to_string),
        },
        "projects" => ReadCall::Projects {
            team: team()?.map(str::to_string),
        },
        "cycles" => ReadCall::Cycles {
            team: string(action, args, "team")?,
        },
        "files" => ReadCall::Files(handle_arg(action, args, "issue")?),
        "download_file" => {
            let (url, to) = (string(action, args, "url")?, string(action, args, "to")?);
            if !Path::new(&to).is_absolute() {
                return Err(format!("{action}: to must be absolute, not {to:?}"));
            }
            ReadCall::DownloadFile { url, to }
        }
        _ => return Err(format!("no action `{action}`")),
    };
    Ok((ws, call))
}

/// The key the handle `action` was passed as its argument `param` carries, as afkd sends a
/// handle, `{"id":…,"key":…}`, or the sentence refusing an argument that is missing or no
/// handle.
pub(crate) fn handle_key<'a>(
    action: &str,
    args: &'a Map<String, Value>,
    param: &str,
) -> Result<&'a str, String> {
    match args.get(param) {
        Some(Value::Object(handle)) => handle.get("key").and_then(Value::as_str),
        Some(_) => None,
        None => return Err(format!("{action}: parameter {param} is required")),
    }
    .ok_or_else(|| format!("{action}: parameter {param} must be an item handle"))
}

/// The string argument `param` of `action`, or the sentence refusing its absence or type.
pub(crate) fn string(
    action: &str,
    args: &Map<String, Value>,
    param: &str,
) -> Result<String, String> {
    match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    }
}

/// The handle `action` was passed as `param`.
pub(crate) fn handle_arg(
    action: &str,
    args: &Map<String, Value>,
    param: &str,
) -> Result<HandleArg, String> {
    let key = handle_key(action, args, param)?;
    Ok(HandleArg {
        id: args[param]
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_string(),
        key: key.to_string(),
    })
}

/// The argument `param` of `action` read by `read`, `None` when it is left out, or the
/// sentence refusing a value that is not `what`.
pub(crate) fn optional<'a, T>(
    action: &str,
    args: &'a Map<String, Value>,
    param: &str,
    read: impl Fn(&'a Value) -> Option<T>,
    what: &str,
) -> Result<Option<T>, String> {
    match args.get(param) {
        None => Ok(None),
        Some(value) => read(value)
            .map(Some)
            .ok_or_else(|| format!("{action}: parameter {param} must be {what}")),
    }
}

/// The list argument `param` of `issues`, each item read by `read`: `None` when it is left
/// out, and a refusal for one of another type and for an empty one, which names nothing to
/// keep.
fn list<'a, T>(
    args: &'a Map<String, Value>,
    param: &str,
    read: impl Fn(&'a Value) -> Option<T>,
    what: &str,
) -> Result<Option<Vec<T>>, String> {
    let items = optional("issues", args, param, Value::as_array, what)?;
    let Some(items) = items else {
        return Ok(None);
    };
    if items.is_empty() {
        return Err(if param == "order" {
            "issues: `order` is empty; leave it out for the default order".to_string()
        } else {
            format!("issues: `{param}` is empty; leave it out to keep every issue")
        });
    }
    items
        .iter()
        .map(|item| read(item).ok_or_else(|| format!("issues: parameter {param} must be {what}")))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// A member of one of the plugin's enums, by its name.
pub(crate) fn member<T: DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}

/// A string item.
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// Whether `text` is shaped as a uuid: 8-4-4-4-12 hexadecimal digits.
fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The issue `text` names in the filter `param`: a uuid, or an identifier `<key>-<number>`,
/// split at the last `-`.
fn issue_ref(param: &str, text: &str) -> Result<IssueRef, String> {
    if is_uuid(text) {
        return Ok(IssueRef::Uuid(text.to_string()));
    }
    let identifier = text
        .rsplit_once('-')
        .filter(|(team, number)| {
            !team.is_empty() && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        })
        .and_then(|(team, number)| Some((team, number.parse().ok()?)));
    match identifier {
        Some((team, number)) => Ok(IssueRef::Identifier {
            team: team.to_string(),
            number,
        }),
        None => Err(format!(
            "issues: {param} {text:?} is neither an identifier (ENG-123) nor a uuid"
        )),
    }
}

/// Whether `text` is a day of the calendar as `YYYY-MM-DD` writes one: a month that is one,
/// and a day that month has, 29 February only in a leap year.
pub(crate) fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let number = |range: std::ops::Range<usize>| text[range].parse::<u32>().unwrap_or(0);
    let (year, month, day) = (number(0..4), number(5..7), number(8..10));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=12).contains(&month) && (1..=days).contains(&day)
}

/// What `issues` keeps and how it reads them, as `args` say.
///
/// # Errors
///
/// Returns the sentence refusing an argument of another type than the manifest's, an empty
/// list, an `order` of more than three keys, a `limit` outside 1 to 250, a due date that is
/// no day of the calendar, and an `id` or a `parent` that is neither an identifier nor a
/// uuid.
pub(crate) fn issue_query(args: &Map<String, Value>) -> Result<IssueQuery, String> {
    let strings = |param: &str| list(args, param, text, "a list of strings");
    let refs = |param: &str| -> Result<Option<Vec<IssueRef>>, String> {
        strings(param)?
            .map(|items| items.iter().map(|item| issue_ref(param, item)).collect())
            .transpose()
    };
    let mut times = Vec::new();
    for (param, field, compare) in TIME_FILTERS {
        if let Some(ms) = optional("issues", args, param, Value::as_i64, "a time")? {
            // Linear holds no time before 1970, so one before it reads as 1970 itself: an
            // `_after` it keeps every issue, and a `_before` it none, as the time would.
            let at = UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0));
            times.push((*field, *compare, format_utc(at)));
        }
    }
    let mut due = Vec::new();
    for (param, compare) in DUE_FILTERS {
        if let Some(day) = optional("issues", args, param, Value::as_str, "a string")? {
            if !is_date(day) {
                return Err(format!("issues: {param} {day:?} is no YYYY-MM-DD date"));
            }
            due.push((*compare, day.to_string()));
        }
    }
    let limit =
        optional("issues", args, "limit", Value::as_i64, "an int")?.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("issues: limit {limit} is outside 1 to {MAX_LIMIT}"));
    }
    let order = list(args, "order", member, "a list of linear.Sort")?
        .unwrap_or_else(|| Sort::DEFAULT.to_vec());
    if order.len() > MAX_ORDER {
        return Err(format!(
            "issues: `order` names {} keys, and Linear sorts by at most {MAX_ORDER}",
            order.len()
        ));
    }
    Ok(IssueQuery {
        id: refs("id")?,
        team: strings("team")?,
        state: strings("state")?,
        state_type: list(args, "state_type", member, "a list of linear.State_Type")?,
        priority: list(args, "priority", member, "a list of linear.Priority")?,
        labels: strings("labels")?,
        without_labels: strings("without_labels")?,
        assignee: strings("assignee")?,
        delegate: strings("delegate")?,
        project: strings("project")?,
        milestone: strings("milestone")?,
        cycle: list(args, "cycle", Value::as_i64, "a list of ints")?,
        parent: refs("parent")?,
        blocked: optional("issues", args, "blocked", Value::as_bool, "a bool")?,
        text: optional("issues", args, "text", text, "a string")?,
        times,
        due,
        include_archived: optional("issues", args, "include_archived", Value::as_bool, "a bool")?
            .unwrap_or(false),
        order,
        limit: usize::try_from(limit).unwrap_or(1),
    })
}

/// The filter that keeps the issues each issue of `refs` names, any of them: the uuids by
/// Linear's id, and each identifier by its team's key and its number, which is what Linear
/// documents an issue's identifier as.
fn refs_filter(refs: &[IssueRef]) -> Value {
    let uuids: Vec<&str> = refs
        .iter()
        .filter_map(|r| match r {
            IssueRef::Uuid(uuid) => Some(uuid.as_str()),
            IssueRef::Identifier { .. } => None,
        })
        .collect();
    let mut any = Vec::new();
    if !uuids.is_empty() {
        any.push(json!({ "id": { "in": uuids } }));
    }
    for r in refs {
        if let IssueRef::Identifier { team, number } = r {
            any.push(json!({ "team": { "key": { "eq": team } }, "number": { "eq": number } }));
        }
    }
    json!({ "or": any })
}

/// The user filter that keeps an issue held by any of `who`: `me` is whoever the credential
/// acts as, `none` nobody, and any other text a user by display name or email.
fn member_filter(who: &[String]) -> Value {
    let mut any = Vec::new();
    for name in who {
        match name.as_str() {
            ME => any.push(json!({ "isMe": { "eq": true } })),
            NONE => any.push(json!({ "null": true })),
            name => {
                any.push(json!({ "displayName": { "eq": name } }));
                any.push(json!({ "email": { "eq": name } }));
            }
        }
    }
    json!({ "or": any })
}

/// The `IssueFilter` that keeps what `q` keeps: one clause per filter given, all of them
/// holding; `{}`, which keeps every issue, for none.
pub(crate) fn issue_filter(q: &IssueQuery) -> Value {
    let mut all = Vec::new();
    let any_of = |field: &str, values: Value| json!({ field: { "in": values } });
    if let Some(refs) = &q.id {
        all.push(refs_filter(refs));
    }
    if let Some(keys) = &q.team {
        all.push(json!({ "team": any_of("key", json!(keys)) }));
    }
    if let Some(names) = &q.state {
        all.push(json!({ "state": any_of("name", json!(names)) }));
    }
    if let Some(types) = &q.state_type {
        let linear: Vec<&str> = types
            .iter()
            .flat_map(|t| t.linear_types().iter().copied())
            .collect();
        all.push(json!({ "state": any_of("type", json!(linear)) }));
    }
    if let Some(priorities) = &q.priority {
        let linear: Vec<u8> = priorities.iter().map(|p| p.linear()).collect();
        all.push(any_of("priority", json!(linear)));
    }
    for label in q.labels.iter().flatten() {
        all.push(json!({ "labels": { "some": { "name": { "eq": label } } } }));
    }
    if let Some(labels) = &q.without_labels {
        all.push(json!({ "labels": { "every": { "name": { "nin": labels } } } }));
    }
    if let Some(who) = &q.assignee {
        all.push(json!({ "assignee": member_filter(who) }));
    }
    if let Some(who) = &q.delegate {
        all.push(json!({ "delegate": member_filter(who) }));
    }
    if let Some(names) = &q.project {
        all.push(json!({ "project": any_of("name", json!(names)) }));
    }
    if let Some(names) = &q.milestone {
        all.push(json!({ "projectMilestone": any_of("name", json!(names)) }));
    }
    if let Some(numbers) = &q.cycle {
        all.push(json!({ "cycle": any_of("number", json!(numbers)) }));
    }
    if let Some(refs) = &q.parent {
        all.push(json!({ "parent": refs_filter(refs) }));
    }
    if let Some(blocked) = q.blocked {
        all.push(json!({ "hasBlockedByRelations": { "eq": blocked } }));
    }
    if let Some(text) = &q.text {
        all.push(json!({ "or": [
            { "title": { "containsIgnoreCase": text } },
            { "description": { "containsIgnoreCase": text } },
        ] }));
    }
    for (field, compare, at) in &q.times {
        all.push(json!({ *field: { *compare: at } }));
    }
    for (compare, day) in &q.due {
        all.push(json!({ "dueDate": { *compare: day } }));
    }
    if all.is_empty() {
        json!({})
    } else {
        json!({ "and": all })
    }
}

/// Linear's `sort` for `order`: one element per key, each breaking the ties of the one
/// before it.
pub(crate) fn issue_sort(order: &[Sort]) -> Value {
    Value::Array(order.iter().map(|key| key.input()).collect())
}

/// `comments` in the order a thread reads: the comments that answer none oldest first, ties
/// broken by id, each followed at once by its replies in the same order, and theirs, depth
/// first. A reply to a comment the thread does not hold answers none.
pub(crate) fn threaded(mut comments: Vec<(String, CommentFields)>) -> Vec<(String, CommentFields)> {
    comments.sort_by(|a, b| (a.1.created_at, &a.0).cmp(&(b.1.created_at, &b.0)));
    let held: HashSet<&str> = comments.iter().map(|(id, _)| id.as_str()).collect();
    let mut replies: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (at, (_, c)) in comments.iter().enumerate() {
        match c.reply_to.as_deref().filter(|parent| held.contains(parent)) {
            Some(parent) => replies.entry(parent).or_default().push(at),
            None => roots.push(at),
        }
    }
    let mut order = Vec::with_capacity(comments.len());
    let mut placed = vec![false; comments.len()];
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(at) = stack.pop() {
        if std::mem::replace(&mut placed[at], true) {
            continue;
        }
        order.push(at);
        if let Some(answers) = replies.get(comments[at].0.as_str()) {
            stack.extend(answers.iter().rev());
        }
    }
    // Replies that answer one another in a ring answer no comment the thread reads first:
    // they go last, in the order they were posted.
    order.extend((0..comments.len()).filter(|at| !placed[*at]));
    let mut slots: Vec<Option<(String, CommentFields)>> = comments.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|at| slots[at].take())
        .collect()
}

/// The uuid of the issue the handle `issue`, `action`'s argument `param`, names in the
/// workspace `organization`: a read's handle by the uuid its key holds, and an issue the
/// service claimed by the uuid it holds.
///
/// # Errors
///
/// Returns the sentence refusing a handle of another workspace than `organization`, naming
/// both url keys, and a key that names no issue this plugin holds or handed out.
pub(crate) fn resolve(
    action: &str,
    param: &str,
    issue: &HandleArg,
    organization: &Organization,
    ctx: &ReadContext<'_>,
) -> Result<String, String> {
    if let Some(uuid) = keyed(action, param, issue, organization, ctx) {
        return uuid;
    }
    match (ctx.claimed)(&issue.key) {
        Some(claimed) if claimed.organization.id == organization.id => Ok(claimed.uuid),
        Some(claimed) => Err(other_workspace(
            action,
            param,
            &issue.id,
            &claimed.organization.id,
            Some(&claimed.organization.url_key),
            &organization.url_key,
        )),
        None => Err(format!(
            "{action}: {param} {} names no issue this plugin holds or handed out",
            issue.key
        )),
    }
}

/// The uuid the key of `handle`, `action`'s argument `param`, holds when it is a read's key
/// of the workspace `organization`, the sentence refusing it when it is one of another
/// workspace, naming both url keys, and `None` when it is no read's key.
pub(crate) fn keyed(
    action: &str,
    param: &str,
    handle: &HandleArg,
    organization: &Organization,
    ctx: &ReadContext<'_>,
) -> Option<Result<String, String>> {
    let (of, uuid) = parse_read_key(&handle.key)?;
    if of != organization.id {
        let recorded = ctx.state_dir.and_then(|dir| recorded_url_key(dir, of));
        return Some(Err(other_workspace(
            action,
            param,
            &handle.id,
            of,
            recorded.as_deref(),
            &organization.url_key,
        )));
    }
    Some(Ok(uuid.to_string()))
}

/// The workspace `client` reaches, its url key recorded in the plugin's directory when afkd
/// sent one; a record that cannot be written is diagnosed, and the call goes on.
///
/// # Errors
///
/// Returns the sentence of the read Linear refused.
pub(crate) fn reach(
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<Organization, String> {
    let organization = client.organization().map_err(|e| e.to_string())?;
    if let Some(dir) = ctx.state_dir {
        if let Err(e) = record_organization(dir, &organization) {
            diag.err(&format_args!(
                "cannot record the url key of workspace {} in {}: {e}",
                organization.url_key,
                dir.display()
            ));
        }
    }
    Ok(organization)
}

/// The ok reply carrying `handles`.
fn handles_line<F: Serialize>(handles: &[WireHandle<F>]) -> String {
    json!({ "ok": true, "handles": handles }).to_string()
}

/// Do `call`, the read `action`, through `client`: its ok reply line, and the line that
/// narrates it.
fn run(
    action: &str,
    call: &ReadCall,
    client: &LinearClient,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Result<(String, String), String> {
    let linear = |e: LinearError| e.to_string();
    let organization = reach(client, ctx, diag)?;
    let key = |uuid: &str| read_key(&organization.id, uuid);
    let handle = |id: &str, uuid: &str| (id.to_string(), key(uuid));
    let from = format!("from workspace {}", organization.url_key);
    let read = |what: String| format!("{ACTION_TAG} read {what} {from}");
    Ok(match call {
        ReadCall::Issue { id } => {
            let issue = client.issue_full(id).map_err(|e| match e {
                LinearError::NotFound { .. } => format!(
                    "{action}: no issue {id} in workspace {}: {e}",
                    organization.url_key
                ),
                e => e.to_string(),
            })?;
            let handle = WireHandle {
                id: issue.identifier.clone(),
                key: key(&issue.id),
                fields: IssueFields::of(&issue, &organization.url_key),
            };
            (
                json!({ "ok": true, "handle": handle }).to_string(),
                read(format!("issue {}", issue.identifier)),
            )
        }
        ReadCall::Issues(q) => {
            let (filter, sort) = (issue_filter(q), issue_sort(&q.order));
            let issues = client
                .issue_list(
                    |first, after| {
                        issue_list_vars(&filter, &sort, q.include_archived, first, after)
                    },
                    q.limit,
                )
                .map_err(linear)?;
            let handles: Vec<_> = issues
                .iter()
                .map(|issue| WireHandle {
                    id: issue.identifier.clone(),
                    key: key(&issue.id),
                    fields: IssueFields::of(issue, &organization.url_key),
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} issues", handles.len())),
            )
        }
        ReadCall::Comments(issue) => {
            let uuid = resolve(action, "issue", issue, &organization, ctx)?;
            let (identifier, comments) = client.thread(&uuid).map_err(linear)?;
            let handles: Vec<_> = threaded(comments)
                .into_iter()
                .map(|(uuid, fields)| WireHandle {
                    key: key(&uuid),
                    id: uuid,
                    fields,
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} comments on {identifier}", handles.len())),
            )
        }
        ReadCall::Me => {
            let (uuid, fields) = client.me().map_err(linear)?;
            let (id, key) = handle(&uuid, &uuid);
            let me = WireHandle { id, key, fields };
            (
                json!({ "ok": true, "handle": me }).to_string(),
                read("the viewer".to_string()),
            )
        }
        ReadCall::Users => {
            let handles: Vec<_> = client
                .user_list()
                .map_err(linear)?
                .into_iter()
                .map(|(uuid, fields)| {
                    let (id, key) = handle(&uuid, &uuid);
                    WireHandle { id, key, fields }
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} users", handles.len())),
            )
        }
        ReadCall::Teams => {
            let handles: Vec<_> = client
                .team_list()
                .map_err(linear)?
                .into_iter()
                .map(|(uuid, team_key, fields)| {
                    let (id, key) = handle(&team_key, &uuid);
                    WireHandle { id, key, fields }
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} teams", handles.len())),
            )
        }
        ReadCall::States { team } => {
            let team_id = client.resolve_team(team).map_err(linear)?.id;
            let mut states = client.states(&team_id).map_err(linear)?;
            states.sort_by(|a, b| a.position.total_cmp(&b.position));
            let handles: Vec<_> = states
                .into_iter()
                .map(|state| WireHandle {
                    key: key(&state.id),
                    id: state.id,
                    fields: StateFields {
                        name: state.name,
                        kind: state.kind,
                        team: team.clone(),
                    },
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} states of team {team}", handles.len())),
            )
        }
        ReadCall::Labels { team } => {
            let handles: Vec<_> = client
                .label_list(team.as_deref())
                .map_err(linear)?
                .into_iter()
                .map(|(uuid, fields)| {
                    let (id, key) = handle(&uuid, &uuid);
                    WireHandle { id, key, fields }
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} labels", handles.len())),
            )
        }
        ReadCall::Projects { team } => {
            let handles: Vec<_> = client
                .project_list(team.as_deref())
                .map_err(linear)?
                .into_iter()
                .map(|(uuid, fields)| {
                    let (id, key) = handle(&uuid, &uuid);
                    WireHandle { id, key, fields }
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} projects", handles.len())),
            )
        }
        ReadCall::Cycles { team } => {
            let team_id = client.resolve_team(team).map_err(linear)?.id;
            let mut cycles = client.cycle_list(&team_id).map_err(linear)?;
            cycles.sort_by_key(|(_, cycle)| (cycle.starts_at, cycle.number));
            let handles: Vec<_> = cycles
                .into_iter()
                .map(|(uuid, fields)| {
                    let (id, key) = handle(&uuid, &uuid);
                    WireHandle { id, key, fields }
                })
                .collect();
            (
                handles_line(&handles),
                read(format!("{} cycles of team {team}", handles.len())),
            )
        }
        ReadCall::Files(issue) => {
            let uuid = resolve(action, "issue", issue, &organization, ctx)?;
            let urls: Vec<String> = linked_issue_files(client, &uuid)?
                .into_iter()
                .map(|(_, url)| url)
                .collect();
            (
                json!({ "ok": true, "value": urls }).to_string(),
                read(format!("{} files of {}", urls.len(), issue.id)),
            )
        }
        ReadCall::DownloadFile { url, to } => {
            let bytes = client.download(url).map_err(linear)?;
            write_whole(Path::new(to), &bytes, 0o666)
                .map_err(|e| format!("{action}: cannot write {to}: {e}"))?;
            (
                json!({ "ok": true }).to_string(),
                format!(
                    "{ACTION_TAG} downloading {url} to {to} ({} bytes) {from}",
                    bytes.len()
                ),
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `issues`'s arguments as afkd binds them: a workspace and `extra`.
    fn args(extra: Value) -> Map<String, Value> {
        let mut args = Map::from_iter([("ws".to_string(), json!({"api_key": "lin_api_k"}))]);
        args.extend(extra.as_object().expect("an args object").clone());
        args
    }

    fn query(extra: Value) -> IssueQuery {
        issue_query(&args(extra)).expect("a query")
    }

    /// The `and` clauses of the filter `extra` gives, the default query's none.
    fn clauses(extra: Value) -> Value {
        issue_filter(&query(extra))["and"].clone()
    }

    /// Each filter alone sends exactly its clause, in the shape Linear's `IssueFilter`
    /// documents; none sends `{}`.
    #[test]
    fn each_filter_sends_its_clause() {
        assert_eq!(issue_filter(&query(json!({}))), json!({}));
        let uuid = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5";
        for (extra, clause) in [
            (
                json!({"id": ["ENG-12", uuid, "ÅÄÖ-7"]}),
                json!({"or": [{"id": {"in": [uuid]}},
                              {"team": {"key": {"eq": "ENG"}}, "number": {"eq": 12}},
                              {"team": {"key": {"eq": "ÅÄÖ"}}, "number": {"eq": 7}}]}),
            ),
            (
                json!({"team": ["ENG", "OPS"]}),
                json!({"team": {"key": {"in": ["ENG", "OPS"]}}}),
            ),
            (
                json!({"state": ["Todo", "Väntar på svar 🚦"]}),
                json!({"state": {"name": {"in": ["Todo", "Väntar på svar 🚦"]}}}),
            ),
            (
                json!({"state_type": ["started", "canceled"]}),
                json!({"state": {"type": {"in": ["started", "canceled", "duplicate"]}}}),
            ),
            (
                json!({"priority": ["urgent", "none"]}),
                json!({"priority": {"in": [1, 0]}}),
            ),
            (
                json!({"labels": ["afkd/ready", "見出し"]}),
                json!([{"labels": {"some": {"name": {"eq": "afkd/ready"}}}}]),
            ),
            (
                json!({"without_labels": ["Hold", "WIP"]}),
                json!({"labels": {"every": {"name": {"nin": ["Hold", "WIP"]}}}}),
            ),
            (
                json!({"assignee": ["me", "none", "陳大文"]}),
                json!({"assignee": {"or": [{"isMe": {"eq": true}}, {"null": true},
                                           {"displayName": {"eq": "陳大文"}},
                                           {"email": {"eq": "陳大文"}}]}}),
            ),
            (
                json!({"delegate": ["chen@acme.example"]}),
                json!({"delegate": {"or": [{"displayName": {"eq": "chen@acme.example"}},
                                           {"email": {"eq": "chen@acme.example"}}]}}),
            ),
            (
                json!({"project": ["Trackers 🧭"]}),
                json!({"project": {"name": {"in": ["Trackers 🧭"]}}}),
            ),
            (
                json!({"milestone": ["B"]}),
                json!({"projectMilestone": {"name": {"in": ["B"]}}}),
            ),
            (
                json!({"cycle": [3, 4]}),
                json!({"cycle": {"number": {"in": [3, 4]}}}),
            ),
            (
                json!({"parent": ["ENG-400"]}),
                json!({"parent": {"or": [
                    {"team": {"key": {"eq": "ENG"}}, "number": {"eq": 400}},
                ]}}),
            ),
            (
                json!({"blocked": false}),
                json!({"hasBlockedByRelations": {"eq": false}}),
            ),
            (
                json!({"text": "Retry \"storm\""}),
                json!({"or": [{"title": {"containsIgnoreCase": "Retry \"storm\""}},
                              {"description": {"containsIgnoreCase": "Retry \"storm\""}}]}),
            ),
            (
                json!({"created_after": 1_793_000_000_000_i64}),
                json!({"createdAt": {"gt": "2026-10-26T07:33:20Z"}}),
            ),
            (
                json!({"canceled_before": 0}),
                json!({"canceledAt": {"lt": "1970-01-01T00:00:00Z"}}),
            ),
            (
                json!({"created_before": -86_400_000_i64}),
                json!({"createdAt": {"lt": "1970-01-01T00:00:00Z"}}),
            ),
            (
                json!({"due_before": "2026-12-24"}),
                json!({"dueDate": {"lt": "2026-12-24"}}),
            ),
        ] {
            let all = clauses(extra.clone());
            let all = all.as_array().expect("an and");
            match clause {
                Value::Array(first) => assert_eq!(all[..first.len()], first[..], "{extra}"),
                clause => assert_eq!(all, &[clause], "{extra}"),
            }
        }
        assert_eq!(
            clauses(json!({"labels": ["a", "b"]})),
            json!([{"labels": {"some": {"name": {"eq": "a"}}}},
                   {"labels": {"some": {"name": {"eq": "b"}}}}]),
            "an issue carries every label named"
        );
    }

    /// Every filter at once sends one clause each, in the manifest's order, and the other
    /// arguments their own: archived ones, the order and the limit.
    #[test]
    fn every_filter_together_sends_a_clause_each() {
        let q = query(json!({
            "id": ["ENG-1"], "team": ["ENG"], "state": ["Todo"], "state_type": ["started"],
            "priority": ["low"], "labels": ["x"], "without_labels": ["y"],
            "assignee": ["me"], "delegate": ["none"], "project": ["P"], "milestone": ["M"],
            "cycle": [1], "parent": ["ENG-2"], "blocked": true, "text": "t",
            "created_after": 1000, "created_before": 2000, "updated_after": 3000,
            "updated_before": 4000, "completed_after": 5000, "completed_before": 6000,
            "canceled_after": 7000, "canceled_before": 8000,
            "due_after": "2026-01-01", "due_before": "2026-12-31",
            "include_archived": true, "order": ["priority", "created"], "limit": 250,
        }));
        let filter = issue_filter(&q);
        let keys: Vec<String> = filter["and"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                c.as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("+")
            })
            .collect();
        assert_eq!(
            keys,
            [
                "or",
                "team",
                "state",
                "state",
                "priority",
                "labels",
                "labels",
                "assignee",
                "delegate",
                "project",
                "projectMilestone",
                "cycle",
                "parent",
                "hasBlockedByRelations",
                "or",
                "createdAt",
                "createdAt",
                "updatedAt",
                "updatedAt",
                "completedAt",
                "completedAt",
                "canceledAt",
                "canceledAt",
                "dueDate",
                "dueDate",
            ]
        );
        assert!(q.include_archived);
        assert_eq!(q.order, [Sort::Priority, Sort::Created]);
        assert_eq!(q.limit, 250);
        let defaults = query(json!({}));
        assert!(!defaults.include_archived);
        assert_eq!(defaults.order, Sort::DEFAULT);
        assert_eq!(defaults.limit, 50);
    }

    /// Each order key is its element of Linear's `sort`, three of them are three elements in
    /// their order, the default reads by team, then state, then top to bottom, and every key
    /// is listed in the manifest's order.
    #[test]
    fn each_order_key_is_its_sort_element() {
        let linear = json!({"nulls": "last"});
        let newest = json!({"order": "Descending"});
        for (name, element) in [
            ("manual", json!({"manual": {"order": "Ascending"}})),
            ("team", json!({"team": linear})),
            ("state", json!({"workflowState": linear})),
            ("priority", json!({"priority": linear})),
            ("assignee", json!({"assignee": linear})),
            ("project", json!({"project": linear})),
            ("cycle", json!({"cycle": linear})),
            ("due", json!({"dueDate": newest})),
            ("estimate", json!({"estimate": linear})),
            ("title", json!({"title": linear})),
            ("created", json!({"createdAt": newest})),
            ("updated", json!({"updatedAt": newest})),
            ("completed", json!({"completedAt": newest})),
        ] {
            let order = query(json!({ "order": [name] })).order;
            assert_eq!(issue_sort(&order), json!([element]), "{name}");
        }
        assert_eq!(
            issue_sort(&Sort::DEFAULT),
            json!([{"team": linear}, {"workflowState": linear},
                   {"manual": {"order": "Ascending"}}])
        );
        assert_eq!(
            issue_sort(&query(json!({"order": ["completed", "assignee", "manual"]})).order),
            json!([{"completedAt": newest}, {"assignee": linear},
                   {"manual": {"order": "Ascending"}}]),
            "three keys, each breaking the ties of the one before"
        );
        let names: Vec<Value> = Sort::ALL
            .iter()
            .map(|key| serde_json::to_value(key).unwrap())
            .collect();
        assert_eq!(
            names,
            json!([
                "manual",
                "team",
                "state",
                "priority",
                "assignee",
                "project",
                "cycle",
                "due",
                "estimate",
                "title",
                "created",
                "updated",
                "completed"
            ])
            .as_array()
            .unwrap()[..]
        );
    }

    /// Every argument `issues` cannot use is refused in its own sentence before any request:
    /// an empty list, an `order` of more than the three keys Linear sorts by, a limit outside
    /// 1 to 250, a date that is none or no day of the calendar, an `id` or a `parent` that
    /// names no issue, and a value of another type.
    #[test]
    fn issues_refuses_what_it_cannot_use() {
        for (extra, refusal) in [
            (
                json!({"team": []}),
                "issues: `team` is empty; leave it out to keep every issue",
            ),
            (
                json!({"labels": []}),
                "issues: `labels` is empty; leave it out to keep every issue",
            ),
            (
                json!({"state_type": []}),
                "issues: `state_type` is empty; leave it out to keep every issue",
            ),
            (
                json!({"order": []}),
                "issues: `order` is empty; leave it out for the default order",
            ),
            (
                json!({"order": ["priority", "created", "title", "team"]}),
                "issues: `order` names 4 keys, and Linear sorts by at most 3",
            ),
            (json!({"limit": 0}), "issues: limit 0 is outside 1 to 250"),
            (
                json!({"limit": 251}),
                "issues: limit 251 is outside 1 to 250",
            ),
            (json!({"limit": -3}), "issues: limit -3 is outside 1 to 250"),
            (
                json!({"due_after": "2026-13-01"}),
                "issues: due_after \"2026-13-01\" is no YYYY-MM-DD date",
            ),
            (
                json!({"due_before": "24/12/2026"}),
                "issues: due_before \"24/12/2026\" is no YYYY-MM-DD date",
            ),
            (
                json!({"due_before": "2026-02-31"}),
                "issues: due_before \"2026-02-31\" is no YYYY-MM-DD date",
            ),
            (
                json!({"due_after": "2026-02-29"}),
                "issues: due_after \"2026-02-29\" is no YYYY-MM-DD date",
            ),
            (
                json!({"due_after": "2100-02-29"}),
                "issues: due_after \"2100-02-29\" is no YYYY-MM-DD date",
            ),
            (
                json!({"due_after": "2026-04-31"}),
                "issues: due_after \"2026-04-31\" is no YYYY-MM-DD date",
            ),
            (
                json!({"id": ["ENG"]}),
                "issues: id \"ENG\" is neither an identifier (ENG-123) nor a uuid",
            ),
            (
                json!({"parent": ["ENG-12a"]}),
                "issues: parent \"ENG-12a\" is neither an identifier (ENG-123) nor a uuid",
            ),
            (
                json!({"id": ["-12"]}),
                "issues: id \"-12\" is neither an identifier (ENG-123) nor a uuid",
            ),
            (
                json!({"state_type": ["paused"]}),
                "issues: parameter state_type must be a list of linear.State_Type",
            ),
            (
                json!({"priority": [2]}),
                "issues: parameter priority must be a list of linear.Priority",
            ),
            (
                json!({"order": ["sort"]}),
                "issues: parameter order must be a list of linear.Sort",
            ),
            (
                json!({"team": "ENG"}),
                "issues: parameter team must be a list of strings",
            ),
            (
                json!({"created_after": "yesterday"}),
                "issues: parameter created_after must be a time",
            ),
            (
                json!({"blocked": "yes"}),
                "issues: parameter blocked must be a bool",
            ),
        ] {
            assert_eq!(
                issue_query(&args(extra.clone())),
                Err(refusal.to_string()),
                "{extra}"
            );
        }
        assert_eq!(query(json!({"text": ""})).text.as_deref(), Some(""));
        assert_eq!(query(json!({"limit": 1})).limit, 1);
        assert_eq!(
            query(json!({"order": ["priority", "created", "title"]})).order,
            [Sort::Priority, Sort::Created, Sort::Title],
            "three keys are Linear's most"
        );
        for day in ["2024-02-29", "2000-02-29", "2026-12-31", "2026-04-30"] {
            assert_eq!(
                query(json!({ "due_after": day })).due,
                [("gt", day.to_string())],
                "{day}"
            );
        }
    }

    /// The other reads decode their arguments, a missing or mistyped one refused, and a
    /// `download_file` to a relative path refused before anything is read.
    #[test]
    fn every_read_decodes_its_arguments() {
        let handle = json!({"id": "ENG-7", "key": "linear:org/uuid"});
        let ok = |action: &str, extra: Value| decode(action, &args(extra)).map(|(_, call)| call);
        assert_eq!(
            ok("comments", json!({"issue": handle})),
            Ok(ReadCall::Comments(HandleArg {
                id: "ENG-7".into(),
                key: "linear:org/uuid".into()
            }))
        );
        assert_eq!(ok("me", json!({})), Ok(ReadCall::Me));
        for id in ["ENG-7", "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5"] {
            assert_eq!(
                ok("issue", json!({"id": id})),
                Ok(ReadCall::Issue { id: id.into() })
            );
        }
        assert_eq!(
            ok("states", json!({"team": "ENG"})),
            Ok(ReadCall::States { team: "ENG".into() })
        );
        assert_eq!(ok("labels", json!({})), Ok(ReadCall::Labels { team: None }));
        assert_eq!(
            ok("projects", json!({"team": "OPS"})),
            Ok(ReadCall::Projects {
                team: Some("OPS".into())
            })
        );
        assert_eq!(
            ok(
                "download_file",
                json!({"url": "u", "to": "/srv/acme/計画 1/shot.png"})
            ),
            Ok(ReadCall::DownloadFile {
                url: "u".into(),
                to: "/srv/acme/計画 1/shot.png".into()
            })
        );
        for (action, extra, refusal) in [
            ("issue", json!({}), "issue: parameter id is required"),
            (
                "issue",
                json!({"id": handle}),
                "issue: parameter id must be a string",
            ),
            (
                "comments",
                json!({}),
                "comments: parameter issue is required",
            ),
            (
                "files",
                json!({"issue": "ENG-7"}),
                "files: parameter issue must be an item handle",
            ),
            ("states", json!({}), "states: parameter team is required"),
            (
                "cycles",
                json!({"team": 7}),
                "cycles: parameter team must be a string",
            ),
            (
                "labels",
                json!({"team": ["ENG"]}),
                "labels: parameter team must be a string",
            ),
            (
                "download_file",
                json!({"url": "u", "to": "shot.png"}),
                "download_file: to must be absolute, not \"shot.png\"",
            ),
        ] {
            assert_eq!(ok(action, extra), Err(refusal.to_string()), "{action}");
        }
    }

    /// A comment of `id` posted at `created`, answering `reply_to`.
    fn comment(id: &str, created: u64, reply_to: Option<&str>) -> (String, CommentFields) {
        (
            id.to_string(),
            CommentFields {
                issue: "ENG-7".into(),
                body: format!("{id} 看起来不对 🚨"),
                url: format!("https://linear.app/acme/issue/ENG-7#comment-{id}"),
                author: Some("chen".into()),
                reply_to: reply_to.map(str::to_string),
                resolved_at: None,
                created_at: created,
                edited_at: None,
            },
        )
    }

    /// A thread reads root comments oldest first, ties broken by id, each followed by its
    /// replies depth first: a reply older than a later root still follows its own root, a
    /// reply to a reply follows that reply, a reply to a comment gone reads as a root, and a
    /// ring of replies, which answers nothing the thread reads first, goes last.
    #[test]
    fn a_thread_reads_each_reply_after_what_it_answers() {
        let thread = vec![
            comment("r2", 5_000, None),
            comment("a1", 3_000, Some("r1")),
            comment("r1", 1_000, None),
            comment("a1x", 4_000, Some("a1")),
            comment("b1", 2_000, Some("r1")),
            comment("r0", 5_000, None),
            comment("o1", 1_500, Some("gone")),
            comment("x", 9_000, Some("y")),
            comment("y", 9_000, Some("x")),
            comment("c2", 6_000, Some("r2")),
        ];
        let order: Vec<String> = threaded(thread).into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            order,
            ["r1", "b1", "a1", "a1x", "o1", "r0", "r2", "c2", "x", "y"]
        );
    }

    /// A reply of exactly afkd's 8 MiB is sent, and one byte more is refused naming its size,
    /// with how to read less.
    #[test]
    fn a_reply_past_eight_mib_is_refused() {
        let at_cap = "x".repeat(MAX_CALL_REPLY);
        assert_eq!(fit_call("users", at_cap.clone()), Ok(at_cap));
        let over = "x".repeat(MAX_CALL_REPLY + 1);
        assert_eq!(
            fit_call("issues", over.clone()),
            Err(
                "issues: the reply comes to 8388608 bytes, over afkd's 8 MiB for one call; ask \
                 for fewer with limit"
                    .to_string()
            )
        );
        assert_eq!(
            fit_call("comments", over),
            Err(
                "comments: the reply comes to 8388608 bytes, over afkd's 8 MiB for one call; \
                 read fewer"
                    .to_string()
            )
        );
    }

    /// A read's handle is resolved by the uuid its key holds when it is of the workspace, and
    /// refused naming both url keys when it is not; a claimed issue's key resolves through
    /// the service's claim the same way, and a key naming neither is refused.
    #[test]
    fn a_handle_resolves_only_in_its_own_workspace() {
        let acme = Organization {
            id: "org-a".into(),
            url_key: "acme".into(),
        };
        let globex = Organization {
            id: "org-g".into(),
            url_key: "globex".into(),
        };
        let claimed_acme = acme.clone();
        let claimed = move |key: &str| {
            (key == "uuid-7#claim-1").then(|| Claimed {
                uuid: "uuid-7".into(),
                organization: claimed_acme.clone(),
            })
        };
        let ctx = ReadContext {
            state_dir: None,
            deadline: Instant::now(),
            claimed: &claimed,
        };
        let handle = |key: &str| HandleArg {
            id: "ACME-7".into(),
            key: key.into(),
        };
        let read = handle(&read_key("org-a", "uuid-7"));
        assert_eq!(
            resolve("comments", "issue", &read, &acme, &ctx),
            Ok("uuid-7".into())
        );
        assert_eq!(
            resolve("comments", "issue", &read, &globex, &ctx),
            Err(
                "comments: issue is ACME-7 of the workspace whose organization is org-a, and \
                 the linear.Workspace reaches globex"
                    .into()
            )
        );
        let held = handle("uuid-7#claim-1");
        assert_eq!(
            resolve("files", "issue", &held, &acme, &ctx),
            Ok("uuid-7".into())
        );
        assert_eq!(
            resolve("files", "issue", &held, &globex, &ctx),
            Err(
                "files: issue is ACME-7 of workspace acme, and the linear.Workspace reaches \
                 globex"
                    .into()
            )
        );
        assert_eq!(
            resolve("files", "issue", &handle("uuid-9#claim-2"), &acme, &ctx),
            Err(
                "files: issue uuid-9#claim-2 names no issue this plugin holds or handed out".into()
            )
        );
    }
}
