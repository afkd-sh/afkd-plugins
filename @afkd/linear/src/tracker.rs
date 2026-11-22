//! The mockable issue-tracker seam the `intake` kind drives, plus an in-memory mock tracker
//! (`MockTracker`, test-only) for offline tests.
//!
//! The kind does network I/O, but its *logic* — the comment-based claim lock, the gates,
//! the blocked-by rule, the park — is unit-tested with no network by talking only to this
//! trait. The real implementation lives beside it in [`crate::client`]; this module owns the
//! trait, the plain value types it exchanges, the typed [`LinearError`], and the mock.
//!
//! The trait carries only what the kind needs, **by meaning** — resolve a team or a user,
//! read a state's issues, read/post/edit/delete comments, badge an issue, put it back, and
//! the files a write puts up and a read fetches — so the GraphQL mapping stays confined to
//! the real adapter.

use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::file::Upload;
use crate::settings::MemberRef;

/// The workspace a credential reaches: its organization's uuid, which a handle's key carries,
/// and its url key (`acme` in `linear.app/acme`), which a person knows it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Organization {
    pub(crate) id: String,
    pub(crate) url_key: String,
}

/// A team, as `team` names it by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Team {
    pub(crate) id: String,
    pub(crate) key: String,
}

/// A workflow state of a team: what `claim_from` names, and what an issue sits in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct State {
    pub(crate) id: String,
    pub(crate) name: String,
    /// Linear's category for it.
    pub(crate) kind: StateType,
    /// Its place in the team's workflow, as `States` reads it: 0 where a read selects none.
    pub(crate) position: f64,
}

/// The category of a workflow state, as `linear.State_Type` names it. Linear's `duplicate`
/// reads as [`Canceled`](Self::Canceled): an issue closed as a duplicate is closed unfinished,
/// and the handle's enum has no member of its own for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StateType {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
}

impl StateType {
    /// The category Linear's `type` names, or `None` for one outside the seven it documents.
    pub(crate) fn from_linear(kind: &str) -> Option<Self> {
        Some(match kind {
            "triage" => StateType::Triage,
            "backlog" => StateType::Backlog,
            "unstarted" => StateType::Unstarted,
            "started" => StateType::Started,
            "completed" => StateType::Completed,
            "canceled" | "duplicate" => StateType::Canceled,
            _ => return None,
        })
    }

    /// The `type`s Linear writes for it: one each, and `canceled` with `duplicate`.
    pub(crate) fn linear_types(self) -> &'static [&'static str] {
        match self {
            StateType::Triage => &["triage"],
            StateType::Backlog => &["backlog"],
            StateType::Unstarted => &["unstarted"],
            StateType::Started => &["started"],
            StateType::Completed => &["completed"],
            StateType::Canceled => &["canceled", "duplicate"],
        }
    }
}

/// An issue's priority, as `linear.Priority` names it: Linear's 0 to 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Priority {
    #[default]
    None,
    Urgent,
    High,
    Medium,
    Low,
}

impl Priority {
    /// Every priority, in Linear's order, 0 first.
    const ALL: [Priority; 5] = [
        Priority::None,
        Priority::Urgent,
        Priority::High,
        Priority::Medium,
        Priority::Low,
    ];

    /// The priority Linear's number names, or `None` for one that is not 0 to 4.
    pub(crate) fn from_linear(value: f64) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| f64::from(p.linear()) == value)
    }

    /// Linear's number for it.
    pub(crate) fn linear(self) -> u8 {
        match self {
            Priority::None => 0,
            Priority::Urgent => 1,
            Priority::High => 2,
            Priority::Medium => 3,
            Priority::Low => 4,
        }
    }
}

/// An issue: the unit of work the kind turns into a run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Issue {
    /// The issue's uuid, which never changes.
    pub(crate) id: String,
    /// Its identifier (`ENG-123`), which changes when the issue moves team: the
    /// human-facing name a run is named after.
    pub(crate) identifier: String,
    pub(crate) title: String,
    /// Its description, markdown; empty when it has none.
    pub(crate) description: String,
    /// Its address on linear.app.
    pub(crate) url: String,
    /// The workflow state it sits in.
    pub(crate) state: State,
    /// Its label names, in Linear's order.
    pub(crate) labels: Vec<String>,
    /// The id of the user it is assigned to, if any.
    pub(crate) assignee: Option<String>,
    /// The id of the user (an app, typically) it is delegated to, if any: what an app the
    /// issue is handed to is, since `app:assignable` makes the app a delegate.
    pub(crate) delegate: Option<String>,
    /// Its parent, when it is a sub-issue.
    pub(crate) parent: Option<Related>,
    /// Its sub-issues, in Linear's order.
    pub(crate) children: Vec<Child>,
    /// Every issue that blocks it — the `blocks` relations it is the target of — open or
    /// closed.
    pub(crate) blockers: Vec<Blocker>,
    /// Its place in its state's manual order: what a put-back to the bottom goes past.
    pub(crate) sort_order: f64,
    /// What a full read of it knows beyond the claim's scan.
    pub(crate) detail: IssueDetail,
}

/// What a full read of an issue ([`Tracker::issue_full`]) knows beyond the claim's scan: the
/// fields of a `linear.Issue` handle the scan's `Issues` page does not select. Empty for an
/// issue only the scan read.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct IssueDetail {
    /// Its team's key.
    pub(crate) team: String,
    /// Linear's git branch name for it.
    pub(crate) branch: String,
    pub(crate) priority: Priority,
    pub(crate) estimate: Option<i64>,
    /// Its due date, `YYYY-MM-DD`.
    pub(crate) due: Option<String>,
    /// The display name of the user it is assigned to.
    pub(crate) assignee_name: Option<String>,
    /// The display name of the user it is delegated to.
    pub(crate) delegate_name: Option<String>,
    /// The display name of whoever filed it; `None` for an issue an integration filed.
    pub(crate) creator: Option<String>,
    /// Its project's name.
    pub(crate) project: Option<String>,
    /// Its project milestone's name.
    pub(crate) milestone: Option<String>,
    /// Its cycle's number.
    pub(crate) cycle: Option<i64>,
    /// The identifiers of the issues it blocks.
    pub(crate) blocks: Vec<String>,
    /// The identifiers of the issues related to it, either way.
    pub(crate) related: Vec<String>,
    /// The identifiers of the issues similar to it, either way.
    pub(crate) similar: Vec<String>,
    /// The identifiers of the issues marked duplicates of it.
    pub(crate) duplicates: Vec<String>,
    /// The identifier of the issue it is marked a duplicate of.
    pub(crate) duplicate_of: Option<String>,
    /// The urls of its links, Linear's attachments.
    pub(crate) links: Vec<String>,
    /// The display names of its subscribers.
    pub(crate) subscribers: Vec<String>,
    /// When it was filed, in unix milliseconds.
    pub(crate) created_at: u64,
    /// When it last changed, in unix milliseconds.
    pub(crate) updated_at: u64,
    /// When it was completed, in unix milliseconds.
    pub(crate) completed_at: Option<u64>,
    /// When it was canceled, in unix milliseconds.
    pub(crate) canceled_at: Option<u64>,
    /// Whether it is archived; a trashed issue is too.
    pub(crate) archived: bool,
    /// Whether it is in the trash.
    pub(crate) trashed: bool,
}

/// An issue another one names: its identifier and title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Related {
    pub(crate) identifier: String,
    pub(crate) title: String,
}

/// One sub-issue, as the brief lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Child {
    /// Its uuid, which a move of it names.
    pub(crate) id: String,
    pub(crate) identifier: String,
    pub(crate) title: String,
    /// The name of the state it sits in.
    pub(crate) state: String,
}

/// One issue that blocks another: its identifier, and the category of the state it sits
/// in, which says whether it is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Blocker {
    pub(crate) identifier: String,
    pub(crate) state_kind: String,
}

/// A comment on an issue: the kind's claims and markers are comments, so it reads them
/// back to judge a claim and to find what a human said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Comment {
    /// The comment's uuid.
    pub(crate) id: String,
    /// The comment body, markdown.
    pub(crate) text: String,
    /// The id of whoever posted it: the user, else the bot actor, else the external user,
    /// else `""`. An identity key the claim path and the `discuss_with` gate match on,
    /// never overwritten with a name.
    pub(crate) author: String,
    /// The commenter's display name, best available (never empty while the author has an
    /// id). Display only: the brief attributes each comment with this.
    pub(crate) author_name: String,
    /// When the comment was posted (`createdAt`, to the millisecond): the **order** half of
    /// the claim decision.
    pub(crate) posted_at: SystemTime,
    /// When it was last edited (`editedAt`), falling back to
    /// [`posted_at`](Self::posted_at): the **liveness** half, which a renewal moves.
    pub(crate) renewed_at: SystemTime,
}

/// One page of a paginated read, and the cursor of the next when there is one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Page<T> {
    pub(crate) items: Vec<T>,
    pub(crate) next: Option<String>,
}

/// The state of `states` named `name`, a state of the team `team_key`, or the refusal that
/// names both.
pub(crate) fn state_named(
    states: &[State],
    name: &str,
    team_key: &str,
) -> Result<State, LinearError> {
    states
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .ok_or_else(|| LinearError::NoSuch {
            stage: "resolve state",
            what: format!("workflow state named '{name}' in team {team_key}"),
        })
}

/// The `errors[].extensions` kinds Linear names a failure a later try can get past by:
/// a rate limit, a server fault, a lock held too long, a network fault on its side.
const TRANSIENT_KINDS: &[&str] = &[
    "ratelimited",
    "internal error",
    "lock timeout",
    "network error",
];

/// A failure reaching Linear, tagged with the stage it struck so a swallowed poll error
/// logs *where* it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinearError {
    /// The request never produced a response. `reason` is built URL-free.
    Transport { stage: &'static str, reason: String },
    /// Linear answered with a non-success status and no GraphQL `errors` to say why.
    Status {
        stage: &'static str,
        status: u16,
        /// The wait a `Retry-After` header asked for.
        retry_after: Option<Duration>,
    },
    /// Linear answered with an error of its own, on any status: a GraphQL `errors` body's
    /// first message and its `extensions.type`, else its `extensions.code`, lowercased; or
    /// the token endpoint's OAuth `error_description` and `error`.
    Refused {
        stage: &'static str,
        status: u16,
        message: String,
        kind: String,
        retry_after: Option<Duration>,
    },
    /// What the request named does not exist: Linear's `Entity not found` error.
    NotFound {
        stage: &'static str,
        message: String,
    },
    /// The reply could not be read as the shape asked for.
    Decode { stage: &'static str, reason: String },
    /// A name the settings give names nothing: a team, a workflow state or a user.
    NoSuch { stage: &'static str, what: String },
}

impl std::fmt::Display for LinearError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinearError::Transport { stage, reason } => {
                write!(f, "linear {stage}: no response ({reason})")
            }
            LinearError::Status { stage, status, .. } => {
                write!(f, "linear {stage}: Linear returned status {status}")
            }
            LinearError::Refused {
                stage,
                status,
                message,
                ..
            } => write!(f, "linear {stage}: Linear refused it ({status}): {message}"),
            LinearError::NotFound { stage, message } => write!(f, "linear {stage}: {message}"),
            LinearError::Decode { stage, reason } => {
                write!(f, "linear {stage}: undecodable response ({reason})")
            }
            LinearError::NoSuch { stage, what } => write!(f, "linear {stage}: no {what}"),
        }
    }
}

impl std::error::Error for LinearError {}

impl LinearError {
    /// Whether a later try of what failed this way can land: Linear was not reached, or it
    /// answered "too many requests", a server error, or an error of a
    /// [transient kind](TRANSIENT_KINDS). Anything else is Linear's answer about the request
    /// itself — a document it refused as invalid included — and asking again changes
    /// nothing.
    pub(crate) fn transient(&self) -> bool {
        let retryable = |status: u16| status == 429 || (500..=599).contains(&status);
        match self {
            LinearError::Transport { .. } => true,
            LinearError::Status { status, .. } => retryable(*status),
            LinearError::Refused { status, kind, .. } => {
                retryable(*status) || TRANSIENT_KINDS.contains(&kind.as_str())
            }
            LinearError::NotFound { .. }
            | LinearError::Decode { .. }
            | LinearError::NoSuch { .. } => false,
        }
    }

    /// The wait Linear asked for before the next try, when it named one.
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        match self {
            LinearError::Status { retry_after, .. } | LinearError::Refused { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }

    /// The stage label this error was tagged with.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            LinearError::Transport { stage, .. }
            | LinearError::Status { stage, .. }
            | LinearError::Refused { stage, .. }
            | LinearError::NotFound { stage, .. }
            | LinearError::Decode { stage, .. }
            | LinearError::NoSuch { stage, .. } => stage,
        }
    }
}

/// The issue-tracker operations the kind needs, by meaning (not by GraphQL shape).
pub(crate) trait Tracker {
    /// The id of whoever the credential authenticates as: the API key's user, or the app.
    fn viewer(&self) -> Result<String, LinearError>;

    /// The workspace the credential reaches.
    fn organization(&self) -> Result<Organization, LinearError>;

    /// Issue `issue_id` read whole: every field of its handle, each nested list to its end.
    fn issue_full(&self, issue_id: &str) -> Result<Issue, LinearError>;

    /// The team whose key is `key`.
    fn resolve_team(&self, key: &str) -> Result<Team, LinearError>;

    /// The team's workflow states.
    fn states(&self, team_id: &str) -> Result<Vec<State>, LinearError>;

    /// One page of the issues in state `state_id` that the `IssueFilter` `gate` keeps, `{}`
    /// keeping every one, in the team's manual order, from the cursor `after`.
    fn issues_in_state(
        &self,
        team_id: &str,
        state_id: &str,
        gate: &Value,
        after: Option<&str>,
    ) -> Result<Page<Issue>, LinearError>;

    /// Every issue of the team carrying the label `label`, wherever it sits: the read the
    /// parked sweep needs, since a parked issue is claimed from no state. Archived issues
    /// are not among them.
    fn parked_issues(&self, team_id: &str, label: &str) -> Result<Vec<Issue>, LinearError>;

    /// Every comment on issue `issue_id`.
    fn issue_comments(&self, issue_id: &str) -> Result<Vec<Comment>, LinearError>;

    /// Post `text` as a comment on issue `issue_id`, returning the created comment (its id,
    /// its author and its post time, which the claim lock needs).
    fn post_comment(&self, issue_id: &str, text: &str) -> Result<Comment, LinearError>;

    /// Rewrite comment `comment_id` to `text` (renewing a claim lease), which stamps its
    /// `editedAt`.
    fn edit_comment(&self, comment_id: &str, text: &str) -> Result<(), LinearError>;

    /// Delete comment `comment_id` (releasing a claim lease).
    fn delete_comment(&self, comment_id: &str) -> Result<(), LinearError>;

    /// Resolve `who` to a user id: [`MemberRef::Me`] to the [`viewer`](Tracker::viewer), a
    /// name to the one user whose display name or email it is. No such user, or two, is a
    /// [`LinearError::NoSuch`].
    fn resolve_user(&self, who: &MemberRef) -> Result<String, LinearError>;

    /// Put the label named `name` on issue `issue_id`: the team's label of that name, else
    /// the workspace's, else one created for the team.
    fn add_label(&self, team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError>;

    /// Take the label named `name` off issue `issue_id`. A name with no label is a no-op:
    /// there is nothing to take off, and nothing is created.
    fn remove_label(&self, team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError>;

    /// Move issue `issue_id` to state `state_id`, below every issue already there.
    fn move_to_bottom(
        &self,
        team_id: &str,
        issue_id: &str,
        state_id: &str,
    ) -> Result<(), LinearError>;

    /// Issue `issue_id`'s description as Linear holds it now, `""` when it has none.
    fn issue_description(&self, issue_id: &str) -> Result<String, LinearError>;

    /// Archive issue `issue_id`.
    fn archive_issue(&self, issue_id: &str) -> Result<(), LinearError>;

    /// Put `upload` in Linear's file storage — a signed upload asked for, then the bytes
    /// sent to it — and return the asset url a comment or a description links it at.
    fn upload_file(&self, upload: &Upload) -> Result<String, LinearError>;

    /// The bytes of the file Linear hosts at `url`, fetched with the credential. A url
    /// [`is_linear_file`](Tracker::is_linear_file) does not take is refused, and nothing is
    /// sent: the credential goes to no other host.
    fn download(&self, url: &str) -> Result<Vec<u8>, LinearError>;

    /// Whether `url` is a file Linear hosts, which [`download`](Tracker::download) fetches.
    fn is_linear_file(&self, url: &str) -> bool;

    /// The `Authorization` header value a run reaches Linear with, as of now: the API key, or
    /// `Bearer` and the app's current token.
    fn authorization(&self) -> Result<String, LinearError>;

    /// Hand over the current call's deadline, which every request until the next hand-over
    /// must finish by — set before a call, cleared with `None` after it. A tracker that
    /// sends no requests has nothing to bound and ignores it.
    fn set_call_deadline(&self, _deadline: Option<Instant>) {}
}

/// Sharing a tracker behind an [`Arc`](std::sync::Arc) keeps it a `Tracker`, so a test can
/// retain a handle while also handing the kind a boxed one.
impl<T: Tracker + ?Sized> Tracker for std::sync::Arc<T> {
    fn viewer(&self) -> Result<String, LinearError> {
        (**self).viewer()
    }
    fn organization(&self) -> Result<Organization, LinearError> {
        (**self).organization()
    }
    fn issue_full(&self, issue_id: &str) -> Result<Issue, LinearError> {
        (**self).issue_full(issue_id)
    }
    fn resolve_team(&self, key: &str) -> Result<Team, LinearError> {
        (**self).resolve_team(key)
    }
    fn states(&self, team_id: &str) -> Result<Vec<State>, LinearError> {
        (**self).states(team_id)
    }
    fn issues_in_state(
        &self,
        team_id: &str,
        state_id: &str,
        gate: &Value,
        after: Option<&str>,
    ) -> Result<Page<Issue>, LinearError> {
        (**self).issues_in_state(team_id, state_id, gate, after)
    }
    fn parked_issues(&self, team_id: &str, label: &str) -> Result<Vec<Issue>, LinearError> {
        (**self).parked_issues(team_id, label)
    }
    fn issue_comments(&self, issue_id: &str) -> Result<Vec<Comment>, LinearError> {
        (**self).issue_comments(issue_id)
    }
    fn post_comment(&self, issue_id: &str, text: &str) -> Result<Comment, LinearError> {
        (**self).post_comment(issue_id, text)
    }
    fn edit_comment(&self, comment_id: &str, text: &str) -> Result<(), LinearError> {
        (**self).edit_comment(comment_id, text)
    }
    fn delete_comment(&self, comment_id: &str) -> Result<(), LinearError> {
        (**self).delete_comment(comment_id)
    }
    fn resolve_user(&self, who: &MemberRef) -> Result<String, LinearError> {
        (**self).resolve_user(who)
    }
    fn add_label(&self, team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError> {
        (**self).add_label(team_id, issue_id, name)
    }
    fn remove_label(&self, team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError> {
        (**self).remove_label(team_id, issue_id, name)
    }
    fn move_to_bottom(
        &self,
        team_id: &str,
        issue_id: &str,
        state_id: &str,
    ) -> Result<(), LinearError> {
        (**self).move_to_bottom(team_id, issue_id, state_id)
    }
    fn issue_description(&self, issue_id: &str) -> Result<String, LinearError> {
        (**self).issue_description(issue_id)
    }
    fn archive_issue(&self, issue_id: &str) -> Result<(), LinearError> {
        (**self).archive_issue(issue_id)
    }
    fn upload_file(&self, upload: &Upload) -> Result<String, LinearError> {
        (**self).upload_file(upload)
    }
    fn download(&self, url: &str) -> Result<Vec<u8>, LinearError> {
        (**self).download(url)
    }
    fn is_linear_file(&self, url: &str) -> bool {
        (**self).is_linear_file(url)
    }
    fn authorization(&self) -> Result<String, LinearError> {
        (**self).authorization()
    }
    fn set_call_deadline(&self, deadline: Option<Instant>) {
        (**self).set_call_deadline(deadline)
    }
}

#[cfg(test)]
pub(crate) use mock::{Action, MockTracker, FILED_AT, MOCK_UPLOADS, SELF_ID, TEAM_ID};

#[cfg(test)]
mod mock {
    use super::*;
    use crate::common::lock;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

    /// The viewer's id on a mock tracker, and the author of every comment it posts.
    pub(crate) const SELF_ID: &str = "mock-self";

    /// The one team's id; its key is `ENG`.
    pub(crate) const TEAM_ID: &str = "team-eng";

    /// The mock's workspace: its organization's id.
    pub(crate) const MOCK_ORG_ID: &str = "mock-org";

    /// The mock's workspace: its url key.
    pub(crate) const MOCK_URL_KEY: &str = "acme";

    /// When every issue the mock holds was filed and last changed, in unix milliseconds.
    pub(crate) const FILED_AT: u64 = 1_793_000_000_000;

    /// Where the mock's files live: every asset url it hands out, and every one it fetches.
    pub(crate) const MOCK_UPLOADS: &str = "https://uploads.linear.app/";

    /// What a recorded write was.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Action {
        /// An issue was moved to the bottom of a state.
        Move { issue: String, state: String },
        /// A comment was deleted.
        DeleteComment(String),
        /// A comment's text was rewritten — a claim lease's renewal.
        EditComment { comment: String, text: String },
        /// A named label was put on an issue.
        AddLabel { issue: String, label: String },
        /// A named label was taken off an issue.
        RemoveLabel { issue: String, label: String },
        /// An issue was archived.
        Archive(String),
        /// A file was put up.
        Upload {
            name: String,
            content_type: String,
            len: usize,
        },
    }

    /// One issue on the mock, its comments in the order posted, and whether it is archived.
    struct Stored {
        issue: Issue,
        comments: Vec<Comment>,
        archived: bool,
    }

    /// An in-memory [`Tracker`] with controllable comment post times.
    ///
    /// Tests seed states, issues and comments (each with an explicit post time), then drive
    /// the kind against it and inspect the recorded writes. A stage can be made to fail, to
    /// exercise the swallow-and-continue path.
    #[derive(Default)]
    pub(crate) struct MockTracker {
        states: Mutex<Vec<State>>,
        /// In manual order, every state's issues together.
        issues: Mutex<Vec<Stored>>,
        /// Display name or email → user id, as `resolve_user` reads them.
        users: Mutex<HashMap<String, String>>,
        actions: Mutex<Vec<Action>>,
        /// A stage that fails, and the error it fails with.
        fail_stage: Mutex<Option<(&'static str, LinearError)>>,
        /// Whether every stage fails with a transport error: Linear out of reach.
        down: AtomicBool,
        /// The seconds since the epoch the next posted comment is stamped with; each post
        /// takes the next.
        next_post: AtomicU64,
        /// Every call's stage name, in order.
        calls: Mutex<Vec<&'static str>>,
        /// Every call deadline handed over, in order.
        call_deadlines: Mutex<Vec<Option<Instant>>>,
        /// The page size `issues_in_state` answers with; zero is "all of them".
        page: AtomicU64,
        /// Every gate `issues_in_state` was sent, in order.
        gates: Mutex<Vec<Value>>,
        /// Who a posted comment is authored by, when not [`SELF_ID`]: an app whose comments
        /// come back under its bot actor.
        post_author: Mutex<Option<String>>,
        /// The files the mock holds, by asset url: those put up.
        files: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl MockTracker {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Add a workflow state of the category `kind`, after every state seeded before it;
        /// its name doubles as its id.
        pub(crate) fn add_state(&self, name: &str, kind: &str) {
            let mut states = lock(&self.states);
            let position = states.len() as f64;
            states.push(State {
                id: name.to_string(),
                name: name.to_string(),
                kind: StateType::from_linear(kind).expect("a Linear state type"),
                position,
            });
        }

        /// Add an issue at the bottom of the state named `state`; its identifier doubles as
        /// its id.
        pub(crate) fn add_issue(&self, state: &str, identifier: &str, title: &str, body: &str) {
            let state = lock(&self.states)
                .iter()
                .find(|s| s.name == state)
                .cloned()
                .expect("a seeded state");
            let mut issues = lock(&self.issues);
            let sort_order = issues.len() as f64;
            issues.push(Stored {
                issue: Issue {
                    id: identifier.to_string(),
                    identifier: identifier.to_string(),
                    title: title.to_string(),
                    description: body.to_string(),
                    url: format!("https://linear.app/acme/issue/{identifier}"),
                    state,
                    labels: Vec::new(),
                    assignee: None,
                    delegate: None,
                    parent: None,
                    children: Vec::new(),
                    blockers: Vec::new(),
                    sort_order,
                    detail: IssueDetail {
                        team: "ENG".to_string(),
                        branch: identifier.to_lowercase(),
                        created_at: FILED_AT,
                        updated_at: FILED_AT,
                        ..IssueDetail::default()
                    },
                },
                comments: Vec::new(),
                archived: false,
            });
        }

        /// Change an issue in place.
        pub(crate) fn edit_issue(&self, id: &str, edit: impl FnOnce(&mut Issue)) {
            let mut issues = lock(&self.issues);
            let stored = issues
                .iter_mut()
                .find(|s| s.issue.id == id)
                .expect("a seeded issue");
            edit(&mut stored.issue);
        }

        /// Put a label on an issue, as Linear reports it.
        pub(crate) fn seed_label(&self, id: &str, label: &str) {
            self.edit_issue(id, |i| i.labels.push(label.to_string()));
        }

        /// Register a user by display name or email.
        pub(crate) fn seed_user(&self, name: &str, id: &str) {
            lock(&self.users).insert(name.to_string(), id.to_string());
        }

        /// Seed a comment posted `secs` after the epoch by the generic `"seed"` author.
        pub(crate) fn seed_comment(&self, id: &str, comment: &str, text: &str, secs: u64) {
            self.seed_comment_by(id, comment, text, "seed", secs);
        }

        /// Seed a comment by `author`, its display name the id.
        pub(crate) fn seed_comment_by(
            &self,
            id: &str,
            comment: &str,
            text: &str,
            author: &str,
            secs: u64,
        ) {
            self.push_comment(
                id,
                comment,
                text,
                author,
                UNIX_EPOCH + Duration::from_secs(secs),
            );
        }

        /// Seed a comment by `author` posted at `at`, unedited.
        pub(crate) fn push_comment(
            &self,
            id: &str,
            comment: &str,
            text: &str,
            author: &str,
            at: SystemTime,
        ) {
            let mut issues = lock(&self.issues);
            let stored = issues
                .iter_mut()
                .find(|s| s.issue.id == id)
                .expect("a seeded issue");
            stored.comments.push(Comment {
                id: comment.to_string(),
                text: text.to_string(),
                author: author.to_string(),
                author_name: author.to_string(),
                posted_at: at,
                renewed_at: at,
            });
        }

        /// Make every call of `stage` fail with `error` until cleared.
        pub(crate) fn fail(&self, stage: &'static str, error: LinearError) {
            *lock(&self.fail_stage) = Some((stage, error));
        }

        /// Make every call fail with a transport error until cleared.
        pub(crate) fn go_down(&self) {
            self.down.store(true, Ordering::SeqCst);
        }

        /// Stop failing.
        pub(crate) fn clear_failure(&self) {
            *lock(&self.fail_stage) = None;
            self.down.store(false, Ordering::SeqCst);
        }

        /// Stamp the next posted comment `secs` after the epoch.
        pub(crate) fn set_clock(&self, secs: u64) {
            self.next_post.store(secs, Ordering::SeqCst);
        }

        /// Answer `issues_in_state` `n` issues a page.
        pub(crate) fn set_page(&self, n: u64) {
            self.page.store(n, Ordering::SeqCst);
        }

        /// Author every comment posted from now on as `author`.
        pub(crate) fn post_as(&self, author: &str) {
            *lock(&self.post_author) = Some(author.to_string());
        }

        /// Have the mock hold `bytes` at `url`, as a file Linear hosts there.
        pub(crate) fn seed_file(&self, url: &str, bytes: &[u8]) {
            lock(&self.files).push((url.to_string(), bytes.to_vec()));
        }

        /// The bytes the mock holds at `url`.
        pub(crate) fn file(&self, url: &str) -> Option<Vec<u8>> {
            lock(&self.files)
                .iter()
                .find(|(at, _)| at == url)
                .map(|(_, bytes)| bytes.clone())
        }

        pub(crate) fn actions(&self) -> Vec<Action> {
            lock(&self.actions).clone()
        }

        pub(crate) fn calls(&self) -> Vec<&'static str> {
            lock(&self.calls).clone()
        }

        pub(crate) fn call_deadlines(&self) -> Vec<Option<Instant>> {
            lock(&self.call_deadlines).clone()
        }

        /// Every gate a state's scan was sent, in order.
        pub(crate) fn gates(&self) -> Vec<Value> {
            lock(&self.gates).clone()
        }

        /// The comments on an issue now, in the order posted.
        pub(crate) fn comments_on(&self, id: &str) -> Vec<Comment> {
            lock(&self.issues)
                .iter()
                .find(|s| s.issue.id == id)
                .map(|s| s.comments.clone())
                .unwrap_or_default()
        }

        /// An issue as it stands.
        pub(crate) fn issue(&self, id: &str) -> Issue {
            lock(&self.issues)
                .iter()
                .find(|s| s.issue.id == id)
                .map(|s| s.issue.clone())
                .expect("a seeded issue")
        }

        /// The identifiers in the state named `state`, in order.
        pub(crate) fn in_state(&self, state: &str) -> Vec<String> {
            let mut issues: Vec<Issue> = lock(&self.issues)
                .iter()
                .filter(|s| s.issue.state.name == state)
                .map(|s| s.issue.clone())
                .collect();
            issues.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
            issues.into_iter().map(|i| i.identifier).collect()
        }

        fn guard(&self, stage: &'static str) -> Result<(), LinearError> {
            lock(&self.calls).push(stage);
            if self.down.load(Ordering::SeqCst) {
                return Err(LinearError::Transport {
                    stage,
                    reason: "mock failure".into(),
                });
            }
            match &*lock(&self.fail_stage) {
                Some((failing, error)) if *failing == stage => Err(error.clone()),
                _ => Ok(()),
            }
        }

        fn issue_mut<T>(
            &self,
            id: &str,
            stage: &'static str,
            f: impl FnOnce(&mut Stored) -> T,
        ) -> Result<T, LinearError> {
            let mut issues = lock(&self.issues);
            match issues.iter_mut().find(|s| s.issue.id == id) {
                Some(stored) => Ok(f(stored)),
                None => Err(not_found(stage, "Issue")),
            }
        }
    }

    /// Whether `issue` passes the gate `gate`, as Linear reads it: the gate's skipped labels
    /// and its blocked-by clause, the two the mock evaluates. Any other clause panics, so no
    /// test leans on one the mock would let through; the fake Linear evaluates them all.
    fn keeps(gate: &Value, issue: &Issue) -> bool {
        let clauses = match gate.get("and") {
            Some(Value::Array(clauses)) => clauses.as_slice(),
            _ if gate.as_object().is_some_and(serde_json::Map::is_empty) => &[],
            _ => panic!("the mock evaluates no gate {gate}"),
        };
        clauses.iter().all(|clause| {
            if let Some(denied) = clause["labels"]["every"]["name"]["nin"].as_array() {
                return !issue
                    .labels
                    .iter()
                    .any(|l| denied.iter().any(|d| d.as_str() == Some(l.as_str())));
            }
            if let Some(blocked) = clause["hasBlockedByRelations"]["eq"].as_bool() {
                let open = issue.blockers.iter().any(|b| {
                    !["completed", "canceled", "duplicate"].contains(&b.state_kind.as_str())
                });
                return open == blocked;
            }
            panic!("the mock evaluates no gate clause {clause}")
        })
    }

    /// `issue` as the claim's scan reads it: with no detail, which only a full read selects.
    fn scanned(issue: &Issue) -> Issue {
        Issue {
            detail: IssueDetail::default(),
            ..issue.clone()
        }
    }

    /// Linear's not-found error, for the entity `what`.
    fn not_found(stage: &'static str, what: &str) -> LinearError {
        LinearError::NotFound {
            stage,
            message: format!("Entity not found: {what}"),
        }
    }

    impl Tracker for MockTracker {
        fn viewer(&self) -> Result<String, LinearError> {
            self.guard("viewer")?;
            Ok(SELF_ID.to_string())
        }

        fn organization(&self) -> Result<Organization, LinearError> {
            self.guard("read organization")?;
            Ok(Organization {
                id: MOCK_ORG_ID.to_string(),
                url_key: MOCK_URL_KEY.to_string(),
            })
        }

        fn issue_full(&self, issue_id: &str) -> Result<Issue, LinearError> {
            self.guard("read issue")?;
            self.issue_mut(issue_id, "read issue", |s| s.issue.clone())
        }

        fn resolve_team(&self, key: &str) -> Result<Team, LinearError> {
            self.guard("resolve team")?;
            if key != "ENG" {
                return Err(LinearError::NoSuch {
                    stage: "resolve team",
                    what: format!("team with the key '{key}'"),
                });
            }
            Ok(Team {
                id: TEAM_ID.to_string(),
                key: key.to_string(),
            })
        }

        fn states(&self, _team_id: &str) -> Result<Vec<State>, LinearError> {
            self.guard("read states")?;
            Ok(lock(&self.states).clone())
        }

        fn issues_in_state(
            &self,
            _team_id: &str,
            state_id: &str,
            gate: &Value,
            after: Option<&str>,
        ) -> Result<Page<Issue>, LinearError> {
            self.guard("read issues")?;
            lock(&self.gates).push(gate.clone());
            let mut issues: Vec<Issue> = lock(&self.issues)
                .iter()
                .filter(|s| s.issue.state.id == state_id && !s.archived && keeps(gate, &s.issue))
                .map(|s| scanned(&s.issue))
                .collect();
            issues.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
            let from: usize = after.map_or(0, |a| a.parse().expect("a mock cursor"));
            let page = match self.page.load(Ordering::SeqCst) {
                0 => issues.len(),
                n => n as usize,
            };
            let to = (from + page).min(issues.len());
            Ok(Page {
                items: issues[from..to].to_vec(),
                next: (to < issues.len()).then(|| to.to_string()),
            })
        }

        fn parked_issues(&self, _team_id: &str, label: &str) -> Result<Vec<Issue>, LinearError> {
            self.guard("read parked issues")?;
            Ok(lock(&self.issues)
                .iter()
                .filter(|s| !s.archived && s.issue.labels.iter().any(|l| l == label))
                .map(|s| scanned(&s.issue))
                .collect())
        }

        fn issue_comments(&self, issue_id: &str) -> Result<Vec<Comment>, LinearError> {
            self.guard("read comments")?;
            self.issue_mut(issue_id, "read comments", |s| s.comments.clone())
        }

        fn post_comment(&self, issue_id: &str, text: &str) -> Result<Comment, LinearError> {
            self.guard("post comment")?;
            let secs = self.next_post.fetch_add(1, Ordering::SeqCst);
            let at = UNIX_EPOCH + Duration::from_secs(secs);
            let author = lock(&self.post_author)
                .clone()
                .unwrap_or_else(|| SELF_ID.to_string());
            let comment = Comment {
                id: format!("c{secs}"),
                text: text.to_string(),
                author: author.clone(),
                author_name: author,
                posted_at: at,
                renewed_at: at,
            };
            self.issue_mut(issue_id, "post comment", |s| {
                s.comments.push(comment.clone())
            })?;
            Ok(comment)
        }

        fn edit_comment(&self, comment_id: &str, text: &str) -> Result<(), LinearError> {
            self.guard("edit comment")?;
            let edited = UNIX_EPOCH + Duration::from_secs(self.next_post.load(Ordering::SeqCst));
            for stored in lock(&self.issues).iter_mut() {
                for c in stored.comments.iter_mut().filter(|c| c.id == comment_id) {
                    c.text = text.to_string();
                    c.renewed_at = edited;
                }
            }
            lock(&self.actions).push(Action::EditComment {
                comment: comment_id.to_string(),
                text: text.to_string(),
            });
            Ok(())
        }

        fn delete_comment(&self, comment_id: &str) -> Result<(), LinearError> {
            self.guard("delete comment")?;
            // Recorded whether or not the id was live: the log is what Linear was *asked*
            // to do, so a redundant delete shows.
            lock(&self.actions).push(Action::DeleteComment(comment_id.to_string()));
            let mut found = false;
            for stored in lock(&self.issues).iter_mut() {
                let before = stored.comments.len();
                stored.comments.retain(|c| c.id != comment_id);
                found |= stored.comments.len() < before;
            }
            if found {
                Ok(())
            } else {
                Err(not_found("delete comment", "Comment"))
            }
        }

        fn resolve_user(&self, who: &MemberRef) -> Result<String, LinearError> {
            self.guard("resolve user")?;
            match who {
                MemberRef::Me => Ok(SELF_ID.to_string()),
                MemberRef::Named(name) => {
                    lock(&self.users)
                        .get(name)
                        .cloned()
                        .ok_or_else(|| LinearError::NoSuch {
                            stage: "resolve user",
                            what: format!("user whose display name or email is '{name}'"),
                        })
                }
            }
        }

        fn add_label(&self, _team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError> {
            self.guard("add label")?;
            self.issue_mut(issue_id, "add label", |s| {
                if !s.issue.labels.iter().any(|l| l == name) {
                    s.issue.labels.push(name.to_string());
                }
            })?;
            lock(&self.actions).push(Action::AddLabel {
                issue: issue_id.to_string(),
                label: name.to_string(),
            });
            Ok(())
        }

        fn remove_label(
            &self,
            _team_id: &str,
            issue_id: &str,
            name: &str,
        ) -> Result<(), LinearError> {
            self.guard("remove label")?;
            self.issue_mut(issue_id, "remove label", |s| {
                s.issue.labels.retain(|l| l != name)
            })?;
            lock(&self.actions).push(Action::RemoveLabel {
                issue: issue_id.to_string(),
                label: name.to_string(),
            });
            Ok(())
        }

        fn move_to_bottom(
            &self,
            _team_id: &str,
            issue_id: &str,
            state_id: &str,
        ) -> Result<(), LinearError> {
            self.guard("move issue")?;
            let state = lock(&self.states)
                .iter()
                .find(|s| s.id == state_id)
                .cloned()
                .ok_or_else(|| not_found("move issue", "WorkflowState"))?;
            let bottom = lock(&self.issues)
                .iter()
                .filter(|s| s.issue.state.id == state_id)
                .map(|s| s.issue.sort_order)
                .fold(0.0, f64::max);
            self.issue_mut(issue_id, "move issue", |s| {
                s.issue.state = state;
                s.issue.sort_order = bottom + 1000.0;
            })?;
            lock(&self.actions).push(Action::Move {
                issue: issue_id.to_string(),
                state: state_id.to_string(),
            });
            Ok(())
        }

        fn issue_description(&self, issue_id: &str) -> Result<String, LinearError> {
            self.guard("read description")?;
            self.issue_mut(issue_id, "read description", |s| {
                s.issue.description.clone()
            })
        }

        fn archive_issue(&self, issue_id: &str) -> Result<(), LinearError> {
            self.guard("archive issue")?;
            self.issue_mut(issue_id, "archive issue", |s| s.archived = true)?;
            lock(&self.actions).push(Action::Archive(issue_id.to_string()));
            Ok(())
        }

        fn upload_file(&self, upload: &Upload) -> Result<String, LinearError> {
            self.guard("upload file")?;
            let mut files = lock(&self.files);
            let url = format!("{MOCK_UPLOADS}acme/{n}/asset-{n}", n = files.len() + 1);
            files.push((url.clone(), upload.bytes.to_vec()));
            lock(&self.actions).push(Action::Upload {
                name: upload.name.clone(),
                content_type: upload.content_type.clone(),
                len: upload.bytes.len(),
            });
            Ok(url)
        }

        fn download(&self, url: &str) -> Result<Vec<u8>, LinearError> {
            self.guard("fetch file")?;
            self.file(url).ok_or(LinearError::Status {
                stage: "fetch file",
                status: 404,
                retry_after: None,
            })
        }

        fn is_linear_file(&self, url: &str) -> bool {
            url.starts_with(MOCK_UPLOADS)
        }

        fn authorization(&self) -> Result<String, LinearError> {
            Ok("lin_api_mock".to_string())
        }

        fn set_call_deadline(&self, deadline: Option<Instant>) {
            lock(&self.call_deadlines).push(deadline);
        }
    }

    /// Every transient error is one a retry can get past, and every other one is not: a
    /// refused document, a 400 and a missing entity among them.
    #[test]
    fn only_a_failure_a_retry_can_get_past_is_transient() {
        let refused = |status: u16, kind: &str| LinearError::Refused {
            stage: "s",
            status,
            message: "m".into(),
            kind: kind.into(),
            retry_after: None,
        };
        let status = |status: u16| LinearError::Status {
            stage: "s",
            status,
            retry_after: None,
        };
        for e in [
            LinearError::Transport {
                stage: "s",
                reason: "io".into(),
            },
            status(429),
            status(500),
            status(503),
            refused(429, "ratelimited"),
            refused(200, "ratelimited"),
            refused(200, "internal error"),
            refused(200, "lock timeout"),
            refused(500, "unknown"),
        ] {
            assert!(e.transient(), "{e:?}");
        }
        for e in [
            status(400),
            status(401),
            status(404),
            refused(400, "graphql error"),
            refused(200, "invalid input"),
            refused(401, "authentication error"),
            refused(200, "forbidden"),
            not_found("s", "Comment"),
            LinearError::Decode {
                stage: "s",
                reason: "r".into(),
            },
            LinearError::NoSuch {
                stage: "s",
                what: "w".into(),
            },
        ] {
            assert!(!e.transient(), "{e:?}");
        }
    }

    /// Each error reads as one line naming its stage, and the wait a rate limit asked for
    /// is kept beside it.
    #[test]
    fn an_error_names_its_stage_and_keeps_its_retry_after() {
        let limited = LinearError::Refused {
            stage: "post comment",
            status: 429,
            message: "Rate limit exceeded 🚦".into(),
            kind: "ratelimited".into(),
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(
            limited.to_string(),
            "linear post comment: Linear refused it (429): Rate limit exceeded 🚦"
        );
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(limited.stage(), "post comment");
        assert_eq!(
            not_found("delete comment", "Comment").to_string(),
            "linear delete comment: Entity not found: Comment"
        );
        assert_eq!(
            LinearError::NoSuch {
                stage: "resolve state",
                what: "workflow state named 'Doing' in team ENG".into()
            }
            .to_string(),
            "linear resolve state: no workflow state named 'Doing' in team ENG"
        );
        assert_eq!(not_found("s", "Issue").retry_after(), None);
    }

    /// Linear's seven state types read as the handle's six, `duplicate` as `canceled`, and
    /// `canceled` filters on both; an eighth reads as none. A priority is 0 to 4 and nothing
    /// else, each back to its own number.
    #[test]
    fn a_state_type_and_a_priority_read_as_linear_writes_them() {
        for (linear, read) in [
            ("triage", StateType::Triage),
            ("backlog", StateType::Backlog),
            ("unstarted", StateType::Unstarted),
            ("started", StateType::Started),
            ("completed", StateType::Completed),
            ("canceled", StateType::Canceled),
            ("duplicate", StateType::Canceled),
        ] {
            assert_eq!(StateType::from_linear(linear), Some(read), "{linear}");
            assert!(read.linear_types().contains(&linear), "{linear}");
        }
        assert_eq!(
            StateType::Canceled.linear_types(),
            ["canceled", "duplicate"]
        );
        for unknown in ["paused", "Started", ""] {
            assert_eq!(StateType::from_linear(unknown), None, "{unknown}");
        }
        assert_eq!(
            serde_json::to_value([StateType::Unstarted, StateType::Canceled]).unwrap(),
            serde_json::json!(["unstarted", "canceled"])
        );
        for (number, priority, name) in [
            (0.0, Priority::None, "none"),
            (1.0, Priority::Urgent, "urgent"),
            (2.0, Priority::High, "high"),
            (3.0, Priority::Medium, "medium"),
            (4.0, Priority::Low, "low"),
        ] {
            assert_eq!(Priority::from_linear(number), Some(priority));
            assert_eq!(f64::from(priority.linear()), number);
            assert_eq!(serde_json::to_value(priority).unwrap(), name);
        }
        for odd in [5.0, -1.0, 1.5, f64::NAN] {
            assert_eq!(Priority::from_linear(odd), None, "{odd}");
        }
    }

    /// The mock answers a state's issues a page at a time, in manual order.
    #[test]
    fn the_mock_pages_a_state_in_manual_order() {
        let t = MockTracker::new();
        t.add_state("Todo", "unstarted");
        for n in 1..=5 {
            t.add_issue("Todo", &format!("ENG-{n}"), "t", "");
        }
        t.set_page(2);
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = t
                .issues_in_state(TEAM_ID, "Todo", &serde_json::json!({}), after.as_deref())
                .unwrap();
            seen.extend(page.items.into_iter().map(|i| i.identifier));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, ["ENG-1", "ENG-2", "ENG-3", "ENG-4", "ENG-5"]);
    }
}
