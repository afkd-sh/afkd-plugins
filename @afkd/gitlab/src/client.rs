//! The mockable GitLab client seam both kinds drive, the plain value types it
//! exchanges, the typed [`GitlabError`], the **real [`Gitlab`] HTTP client** over the
//! `/api/v4` surface, and a `#[cfg(test)]` in-memory `MockClient` for offline tests.
//!
//! Ported from afkd's `crates/gitlab/src/client.rs`, issue and merge-request sides. All
//! GitLab-specific knowledge lives here — every `/api/v4` endpoint path, every request
//! payload, the `PRIVATE-TOKEN` credential, the project-id encoding, and the JSON shapes —
//! so the version-drift blast radius is **one file**. Each endpoint carries a citing
//! comment so a drift fix is a one-place edit. The plumbing underneath is [`crate::http`]
//! and [`crate::rfc3339`].
//!
//! GitLab diverges from Gitea in the deepest places: identity is an id **and** a username
//! ([`User`]), an item's path segment is chosen by an [`ItemKind`], assignment writes
//! numeric ids (`assignee_ids`), the whole lifecycle rides one `PUT`, and notes are scoped
//! to the item they sit on. A project id is a numeric id or a URL-encoded
//! path-with-namespace ([`encode_project`]).
//!
//! The seam ([`GitlabClient`]) carries only what the kinds need, **by meaning**; the REST
//! shape stays confined to the [`Gitlab`] adapter. The response *parsing* is split into
//! pure functions tested with no network, and the HTTP layer itself is exercised only
//! against a loopback `Stub` (no external host).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http::{
    encode_segment, HttpClient, HttpError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT,
};
use crate::rfc3339::parse_rfc3339;

/// A project coordinate: the raw `project` setting (a numeric id or a
/// path-with-namespace), which knows how to render itself as the `:id` path segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    raw: String,
}

impl Project {
    /// Wrap a raw `project` setting value.
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// The raw setting, as written (the `GITLAB_PROJECT` env value and the
    /// claim-journal key's location half).
    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }

    /// The `:id` path segment: an all-digit id passes through; any other value (a
    /// path-with-namespace) is percent-encoded so `group/subgroup/widgets` becomes
    /// `group%2Fsubgroup%2Fwidgets`.
    pub(crate) fn encoded(&self) -> String {
        encode_project(&self.raw)
    }
}

/// A GitLab user, identified by numeric **id** and **username**. Assignment writes ids
/// (`assignee_ids`), so the assignee verbs compare by id; the marker's owner, a note's
/// author and the unit's `self` compare by username.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct User {
    /// The user's numeric id (what assignment writes).
    pub(crate) id: u64,
    /// The user's username handle.
    pub(crate) username: String,
}

/// Which resource an item is: an issue or a merge request. Threaded through the claim and
/// the lifecycle so one set of calls drives both of GitLab's distinct `/issues/:iid` and
/// `/merge_requests/:iid` paths. [`Hash`] because the mock keys its note threads on
/// `(kind, iid)` — an issue and an MR sharing one iid are two distinct threads on the
/// forge, and must be here too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ItemKind {
    /// An issue (`/projects/:id/issues/:iid`).
    Issue,
    /// A merge request (`/projects/:id/merge_requests/:iid`).
    MergeRequest,
}

impl ItemKind {
    /// The path segment for this kind (`issues` / `merge_requests`).
    pub(crate) fn path(&self) -> &'static str {
        match self {
            ItemKind::Issue => "issues",
            ItemKind::MergeRequest => "merge_requests",
        }
    }
}

/// An issue: the unit of work the kind turns into a run. Labels are held by **name**
/// (removal is by name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    /// The project-scoped issue id (`iid`), the natural work-unit id.
    pub(crate) iid: u64,
    /// The issue title (the first line of the task brief).
    pub(crate) title: String,
    /// The issue description/body (the task brief; empty when absent).
    pub(crate) body: String,
    /// The issue state (`opened`/`closed`).
    pub(crate) state: String,
    /// The names of the labels currently on the issue.
    pub(crate) labels: Vec<String>,
}

impl Issue {
    /// Whether the issue carries a label named `name`.
    pub(crate) fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|l| l == name)
    }
}

/// A merge request: the unit of work the MR kind iterates. Only what the kind reads —
/// the listing is filtered to open MRs server-side, and the lifecycle reads assignees
/// through the item calls, not off this record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeRequest {
    /// The project-scoped MR id (`iid`).
    pub(crate) iid: u64,
    /// The MR's source branch (the pushed branch the review run checks out).
    pub(crate) source_branch: String,
    /// The MR author (matched by username against the bot for `author_me`).
    pub(crate) author: User,
}

/// A **note** (comment) on an item, with its author and its two times.
///
/// The two times are **not** interchangeable, and each has one reader: `created_at` is
/// when the note was written, the order a claim marker reads (an edit cannot reshuffle who
/// spoke first) and the `at` the `comments` reply carries; `updated_at` is when it was last
/// touched, the liveness a renewed claim marker shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    /// The note's stable id.
    pub(crate) id: u64,
    /// The note body.
    pub(crate) body: String,
    /// The note author.
    pub(crate) author: User,
    /// When the note was created, as the forge reports it (RFC-3339).
    pub(crate) created_at: SystemTime,
    /// When the note was last updated, as the forge reports it (RFC-3339).
    pub(crate) updated_at: SystemTime,
}

/// A failure reaching GitLab, tagged with the stage it struck so a swallowed poll error
/// logs *where* it happened (afkd's built-in `GitlabError`, variant for variant).
#[derive(Debug)]
pub(crate) enum GitlabError {
    /// The forge answered with a non-success HTTP status.
    Status {
        /// The stage of work the call belonged to (e.g. `list issues`).
        stage: &'static str,
        /// The HTTP status code returned.
        status: u16,
    },
    /// The request never produced a response (connection/transport failure).
    Transport {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// A human-readable transport reason (built URL-free, so no token leaks).
        reason: String,
    },
    /// The response body could not be decoded into the expected shape.
    Decode {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// A human-readable decode reason.
        reason: String,
    },
}

/// The same three sentences afkd's `thiserror` attributes render, so a diagnostic reads
/// the same in `run.log` whichever of the two triggers wrote it.
impl std::fmt::Display for GitlabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitlabError::Status { stage, status } => {
                write!(f, "gitlab {stage}: forge returned status {status}")
            }
            GitlabError::Transport { stage, reason } => {
                write!(f, "gitlab {stage}: no response ({reason})")
            }
            GitlabError::Decode { stage, reason } => {
                write!(f, "gitlab {stage}: undecodable response ({reason})")
            }
        }
    }
}

impl GitlabError {
    /// The stage label this error was tagged with.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            GitlabError::Status { stage, .. }
            | GitlabError::Transport { stage, .. }
            | GitlabError::Decode { stage, .. } => stage,
        }
    }
}

/// The failure vocabulary the shared HTTP spine reports through: the three staged
/// variants above, so the plumbing builds a [`GitlabError`] without naming GitLab.
impl HttpError for GitlabError {
    fn status(stage: &'static str, status: u16) -> Self {
        GitlabError::Status { stage, status }
    }
    fn transport(stage: &'static str, reason: String) -> Self {
        GitlabError::Transport { stage, reason }
    }
    fn decode(stage: &'static str, reason: &str) -> Self {
        GitlabError::Decode {
            stage,
            reason: reason.to_string(),
        }
    }
}

/// The GitLab operations the kinds need, by meaning (not by REST shape).
pub(crate) trait GitlabClient {
    /// Resolve the authenticated user (id + username) — `GET /user`.
    fn current_user(&self) -> Result<User, GitlabError>;

    /// List a project's issues filtered by `state` and (comma-separated) `labels` —
    /// `GET /projects/:id/issues?state&labels`.
    fn list_issues(
        &self,
        project: &Project,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GitlabError>;

    /// List a project's open merge requests —
    /// `GET /projects/:id/merge_requests?state=opened`.
    fn list_open_mrs(&self, project: &Project) -> Result<Vec<MergeRequest>, GitlabError>;

    /// Read the assignees of one item — `GET /projects/:id/{issues|merge_requests}/:iid`.
    fn get_assignees(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
    ) -> Result<Vec<User>, GitlabError>;

    /// Replace an item's assignees (by id) —
    /// `PUT /projects/:id/{issues|merge_requests}/:iid { assignee_ids }`.
    fn set_assignees(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        ids: &[u64],
    ) -> Result<(), GitlabError>;

    /// Add a label (by name) to an item —
    /// `PUT /projects/:id/{issues|merge_requests}/:iid { add_labels }`.
    fn add_label(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        name: &str,
    ) -> Result<(), GitlabError>;

    /// Remove one label (by name) from an item —
    /// `PUT /projects/:id/{issues|merge_requests}/:iid { remove_labels }`.
    fn remove_label(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        name: &str,
    ) -> Result<(), GitlabError>;

    /// Set an item's state via a state event (`close`) —
    /// `PUT /projects/:id/{issues|merge_requests}/:iid { state_event }`.
    fn set_state(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        event: &str,
    ) -> Result<(), GitlabError>;

    /// List an item's notes — `GET /projects/:id/{issues|merge_requests}/:iid/notes`. The
    /// claim re-reads them to decide who holds the item, the MR kind reads them for new
    /// feedback, and afkd's mid-run watch reads them through the `comments` call.
    fn list_notes(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
    ) -> Result<Vec<Note>, GitlabError>;

    /// Post a literal comment (note) on an item, returning the created note (its id and
    /// creation time, which a claim marker needs to recognise and order its own word) —
    /// `POST /projects/:id/{issues|merge_requests}/:iid/notes { body }`.
    fn post_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        text: &str,
    ) -> Result<Note, GitlabError>;

    /// Delete one note by id (releasing a claim marker) —
    /// `DELETE /projects/:id/{issues|merge_requests}/:iid/notes/:note_id`. GitLab scopes
    /// the path to the **item**, so the `kind`/`iid` ride along.
    fn delete_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        note_id: u64,
    ) -> Result<(), GitlabError>;

    /// Rewrite one note's body by id (renewing a claim marker) —
    /// `PUT /projects/:id/{issues|merge_requests}/:iid/notes/:note_id { body }`.
    /// Item-scoped like the delete it sits beside, and it moves the note's `updated_at` —
    /// which is what a rival's claim decision reads as liveness.
    fn edit_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        note_id: u64,
        text: &str,
    ) -> Result<(), GitlabError>;
}

/// Sharing a client behind an [`Arc`](std::sync::Arc) keeps it a [`GitlabClient`], so a
/// caller can retain a handle while also handing the kind a boxed client.
impl<T: GitlabClient> GitlabClient for std::sync::Arc<T> {
    fn current_user(&self) -> Result<User, GitlabError> {
        (**self).current_user()
    }
    fn list_issues(
        &self,
        project: &Project,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GitlabError> {
        (**self).list_issues(project, state, labels)
    }
    fn list_open_mrs(&self, project: &Project) -> Result<Vec<MergeRequest>, GitlabError> {
        (**self).list_open_mrs(project)
    }
    fn get_assignees(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
    ) -> Result<Vec<User>, GitlabError> {
        (**self).get_assignees(project, kind, iid)
    }
    fn set_assignees(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        ids: &[u64],
    ) -> Result<(), GitlabError> {
        (**self).set_assignees(project, kind, iid, ids)
    }
    fn add_label(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        name: &str,
    ) -> Result<(), GitlabError> {
        (**self).add_label(project, kind, iid, name)
    }
    fn remove_label(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        name: &str,
    ) -> Result<(), GitlabError> {
        (**self).remove_label(project, kind, iid, name)
    }
    fn set_state(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        event: &str,
    ) -> Result<(), GitlabError> {
        (**self).set_state(project, kind, iid, event)
    }
    fn list_notes(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
    ) -> Result<Vec<Note>, GitlabError> {
        (**self).list_notes(project, kind, iid)
    }
    fn post_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        text: &str,
    ) -> Result<Note, GitlabError> {
        (**self).post_comment(project, kind, iid, text)
    }
    fn delete_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        note_id: u64,
    ) -> Result<(), GitlabError> {
        (**self).delete_comment(project, kind, iid, note_id)
    }
    fn edit_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        note_id: u64,
        text: &str,
    ) -> Result<(), GitlabError> {
        (**self).edit_comment(project, kind, iid, note_id, text)
    }
}

/// Resolve a configured `base_url` to the `/api/v4` root every endpoint hangs off. Empty
/// falls back to `https://gitlab.com`; surrounding whitespace and a trailing slash are
/// trimmed; cloud and self-managed are identical (both under `/api/v4`).
pub(crate) fn gitlab_api_base(base_url: &str) -> String {
    let b = base_url.trim();
    let root = if b.is_empty() {
        "https://gitlab.com"
    } else {
        b.trim_end_matches('/')
    };
    format!("{root}/api/v4")
}

/// Render a `project` value as its `:id` path segment: an all-ASCII-digit id passes
/// through; anything else (a path-with-namespace) is percent-encoded, escaping every byte
/// outside the unreserved set (`A–Z a–z 0–9 - _ . ~`), so `group/widgets` becomes
/// `group%2Fwidgets`.
pub(crate) fn encode_project(project: &str) -> String {
    if !project.is_empty() && project.bytes().all(|b| b.is_ascii_digit()) {
        return project.to_string();
    }
    encode_segment(project)
}

/// The `/projects/:id/{issues|merge_requests}/:iid` path for an item — the shape every
/// lifecycle write and every note call hangs off.
fn item_path(project: &Project, kind: ItemKind, iid: u64) -> String {
    format!("/projects/{}/{}/{iid}", project.encoded(), kind.path())
}

/// A [`GitlabClient`] backed by the GitLab `/api/v4` REST API.
pub(crate) struct Gitlab {
    http: HttpClient<GitlabError>,
}

impl Gitlab {
    /// A client authenticating against `base_url` (empty → `https://gitlab.com`) with
    /// `token` as the `PRIVATE-TOKEN` header. The `/api/v4` root is computed once here via
    /// [`gitlab_api_base`].
    pub(crate) fn new(base_url: &str, token: &str) -> Self {
        Self {
            http: HttpClient::new(
                // The `/api/v4` root is resolved here, once; the spine neither trims nor
                // suffixes what it is handed.
                gitlab_api_base(base_url),
                // Auth rides a header (never a URL query), so the token cannot leak
                // through a transport-error URL.
                ("PRIVATE-TOKEN".to_string(), token.to_string()),
                DEFAULT_CONNECT_TIMEOUT,
                DEFAULT_READ_TIMEOUT,
            ),
        }
    }

    /// PUT `payload` to an item's path (the one-call lifecycle write).
    fn put_item(
        &self,
        stage: &'static str,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        payload: &Value,
    ) -> Result<(), GitlabError> {
        let req = self.http.request("PUT", &item_path(project, kind, iid));
        self.http.send_json(stage, req, payload).map(|_| ())
    }
}

impl GitlabClient for Gitlab {
    fn current_user(&self) -> Result<User, GitlabError> {
        let stage = "current user";
        // GET /api/v4/user → the authenticated user (id + username).
        let body = self.http.get(stage, "/user")?;
        parse_user(stage, &body)
    }

    fn list_issues(
        &self,
        project: &Project,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GitlabError> {
        let stage = "list issues";
        // GET /api/v4/projects/:id/issues?state=&labels= → issues carrying the source
        // label (a scoped `afkd::ready` survives verbatim as a labels value).
        let req = self
            .http
            .request("GET", &format!("/projects/{}/issues", project.encoded()))
            .query("state", state)
            .query("labels", labels);
        let body = self.http.send(stage, req)?;
        parse_issues(stage, &body)
    }

    fn list_open_mrs(&self, project: &Project) -> Result<Vec<MergeRequest>, GitlabError> {
        let stage = "list merge requests";
        // GET /api/v4/projects/:id/merge_requests?state=opened → the open MRs; a merged or
        // closed MR leaves this set, which is what ends a review loop.
        let req = self
            .http
            .request(
                "GET",
                &format!("/projects/{}/merge_requests", project.encoded()),
            )
            .query("state", "opened");
        let body = self.http.send(stage, req)?;
        parse_mrs(stage, &body)
    }

    fn get_assignees(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
    ) -> Result<Vec<User>, GitlabError> {
        let stage = "get item";
        // GET /api/v4/projects/:id/{issues|merge_requests}/:iid → the item (the assignee
        // verbs read its assignees before replacing them).
        let body = self.http.get(stage, &item_path(project, kind, iid))?;
        parse_assignees(stage, &body)
    }

    fn set_assignees(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        ids: &[u64],
    ) -> Result<(), GitlabError> {
        let stage = "set assignees";
        // PUT …/:iid { assignee_ids } → replace the assignee set (GitLab writes numeric
        // ids).
        self.put_item(stage, project, kind, iid, &json!({ "assignee_ids": ids }))
    }

    fn add_label(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        name: &str,
    ) -> Result<(), GitlabError> {
        let stage = "add label";
        // PUT …/:iid { add_labels } → add the label by name (no id dance).
        self.put_item(stage, project, kind, iid, &json!({ "add_labels": name }))
    }

    fn remove_label(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        name: &str,
    ) -> Result<(), GitlabError> {
        let stage = "remove label";
        // PUT …/:iid { remove_labels } → remove the single named label (by name — GitLab
        // has no all-clearing label path).
        self.put_item(stage, project, kind, iid, &json!({ "remove_labels": name }))
    }

    fn set_state(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        event: &str,
    ) -> Result<(), GitlabError> {
        let stage = "set state";
        // PUT …/:iid { state_event } → opened→closed via `close`.
        self.put_item(stage, project, kind, iid, &json!({ "state_event": event }))
    }

    fn list_notes(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
    ) -> Result<Vec<Note>, GitlabError> {
        let stage = "list notes";
        // GET /api/v4/projects/:id/{issues|merge_requests}/:iid/notes → the item's notes
        // (the claim's re-read, and the mid-run watch). Off the same `item_path` the note
        // post and delete build from, so the kind routing cannot drift.
        let body = self
            .http
            .get(stage, &format!("{}/notes", item_path(project, kind, iid)))?;
        parse_notes(stage, &body)
    }

    fn post_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        text: &str,
    ) -> Result<Note, GitlabError> {
        let stage = "post comment";
        // POST /api/v4/projects/:id/{issues|merge_requests}/:iid/notes { body } → a note
        // (GitLab's comment). GitLab answers with the created note; it is decoded rather
        // than discarded, so a claim marker can recognise and order its own word.
        let req = self
            .http
            .request("POST", &format!("{}/notes", item_path(project, kind, iid)));
        let body = self.http.send_json(stage, req, &json!({ "body": text }))?;
        parse_note(stage, &body)
    }

    fn delete_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        note_id: u64,
    ) -> Result<(), GitlabError> {
        let stage = "delete comment";
        // DELETE /api/v4/projects/:id/{issues|merge_requests}/:iid/notes/:note_id → drop
        // ONE note, off the same `item_path` every other item write uses.
        let req = self.http.request(
            "DELETE",
            &format!("{}/notes/{note_id}", item_path(project, kind, iid)),
        );
        self.http.send(stage, req).map(|_| ())
    }

    fn edit_comment(
        &self,
        project: &Project,
        kind: ItemKind,
        iid: u64,
        note_id: u64,
        text: &str,
    ) -> Result<(), GitlabError> {
        let stage = "edit comment";
        // PUT /api/v4/projects/:id/{issues|merge_requests}/:iid/notes/:note_id { body } →
        // rewrite ONE note, off the same `item_path` the delete beside it builds from.
        let req = self.http.request(
            "PUT",
            &format!("{}/notes/{note_id}", item_path(project, kind, iid)),
        );
        self.http
            .send_json(stage, req, &json!({ "body": text }))
            .map(|_| ())
    }
}

// --- Pure parsing (no network): the GitLab JSON shapes. ----------------------
// The JSON extraction and the RFC-3339 reader are the spine's; what is GitLab's is which
// field each value type reads (`iid`, `description`, id+username).

/// Parse the `GET /user` response into a [`User`].
pub(crate) fn parse_user(stage: &'static str, body: &str) -> Result<User, GitlabError> {
    let value = GitlabError::decode_json(stage, body)?;
    value_to_user(&value).ok_or_else(|| GitlabError::decode(stage, "user missing id/username"))
}

/// Parse an issues array into [`Issue`]s.
pub(crate) fn parse_issues(stage: &'static str, body: &str) -> Result<Vec<Issue>, GitlabError> {
    let value = GitlabError::decode_json(stage, body)?;
    Ok(GitlabError::as_array(stage, &value, "issues")?
        .iter()
        .filter_map(value_to_issue)
        .collect())
}

/// Parse a merge-requests array into [`MergeRequest`]s.
pub(crate) fn parse_mrs(stage: &'static str, body: &str) -> Result<Vec<MergeRequest>, GitlabError> {
    let value = GitlabError::decode_json(stage, body)?;
    Ok(GitlabError::as_array(stage, &value, "merge requests")?
        .iter()
        .filter_map(value_to_mr)
        .collect())
}

/// Parse a single item's `assignees` array.
pub(crate) fn parse_assignees(stage: &'static str, body: &str) -> Result<Vec<User>, GitlabError> {
    let value = GitlabError::decode_json(stage, body)?;
    Ok(value
        .get("assignees")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(value_to_user).collect())
        .unwrap_or_default())
}

/// Parse a notes array into [`Note`]s.
pub(crate) fn parse_notes(stage: &'static str, body: &str) -> Result<Vec<Note>, GitlabError> {
    let value = GitlabError::decode_json(stage, body)?;
    Ok(GitlabError::as_array(stage, &value, "notes")?
        .iter()
        .filter_map(value_to_note)
        .collect())
}

/// Parse a single-note response (the reply to a note POST) into a [`Note`]. Strict: a
/// reply that is not a note object is a decode failure rather than a synthesized value, so
/// a claim can never be built on a fabricated id. Shares `value_to_note` with
/// [`parse_notes`], so the POST reply and the list reply cannot read a note differently.
pub(crate) fn parse_note(stage: &'static str, body: &str) -> Result<Note, GitlabError> {
    let value = GitlabError::decode_json(stage, body)?;
    value_to_note(&value).ok_or_else(|| GitlabError::decode(stage, "note missing id"))
}

fn value_to_user(v: &Value) -> Option<User> {
    let id = v.get("id")?.as_u64()?;
    let username = v
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(User { id, username })
}

fn value_to_label_name(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    v.get("name").and_then(Value::as_str).map(str::to_string)
}

fn value_to_labels(v: &Value) -> Vec<String> {
    v.get("labels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(value_to_label_name).collect())
        .unwrap_or_default()
}

fn value_to_issue(v: &Value) -> Option<Issue> {
    let iid = v.get("iid")?.as_u64()?;
    Some(Issue {
        iid,
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        body: v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("opened")
            .to_string(),
        labels: value_to_labels(v),
    })
}

fn value_to_mr(v: &Value) -> Option<MergeRequest> {
    let iid = v.get("iid")?.as_u64()?;
    Some(MergeRequest {
        iid,
        source_branch: v
            .get("source_branch")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        author: v.get("author").and_then(value_to_user).unwrap_or_default(),
    })
}

fn value_to_note(v: &Value) -> Option<Note> {
    let id = v.get("id")?.as_u64()?;
    let author = v.get("author").and_then(value_to_user).unwrap_or(User {
        id: 0,
        username: String::new(),
    });
    // Both instants read through the same RFC-3339 reader, and both degrade to the epoch
    // when absent — an absent field stays distinguishable from a present one rather than
    // being masked by its sibling's value.
    let created_at = v
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(UNIX_EPOCH);
    let updated_at = v
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(UNIX_EPOCH);
    Some(Note {
        id,
        body: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        author,
        created_at,
        updated_at,
    })
}

#[cfg(test)]
pub(crate) use mock::{Action, MockClient};

#[cfg(test)]
mod mock {
    //! An in-memory [`GitlabClient`] (test-only). No network: every kind and settings
    //! test drives this mock; the real [`Gitlab`] HTTP code is exercised only against the
    //! loopback `Stub` below.

    use super::*;
    use crate::common::lock;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A recorded mutation against the mock forge, for assertions. Each carries the
    /// [`ItemKind`] so a test can assert the routing.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Action {
        /// An item's assignees were replaced (by id).
        Assign {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The new assignee ids.
            ids: Vec<u64>,
        },
        /// A label was added to an item.
        Label {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The label name added.
            name: String,
        },
        /// A label was removed from an item (by name — never an all-clearing path).
        Unlabel {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The label name removed.
            name: String,
        },
        /// An item's state event was applied.
        State {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The state event (`close`).
            event: String,
        },
        /// A literal comment (note) was posted on an item.
        Comment {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The comment body posted.
            body: String,
        },
        /// A note was deleted from an item (by id).
        DeleteComment {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The note id removed.
            id: u64,
        },
        /// A note's body was rewritten (by id) — a claim marker's renewal.
        EditComment {
            /// Which resource.
            kind: ItemKind,
            /// The item's iid.
            iid: u64,
            /// The note id edited.
            id: u64,
            /// The body it now carries.
            body: String,
        },
    }

    /// One seeded issue, with the assignee set the listing leaves out.
    struct Seeded {
        issue: Issue,
        assignees: Vec<User>,
    }

    /// One seeded merge request, with the state, labels and assignees its trimmed record
    /// leaves out.
    struct SeededMr {
        mr: MergeRequest,
        state: String,
        labels: Vec<String>,
        assignees: Vec<User>,
    }

    /// An in-memory forge: tests seed issues, merge requests and notes (each with an
    /// explicit timestamp), drive the kind against it, and inspect the recorded
    /// [`Action`]s. A stage can be made to fail, the second a posted note is stamped with
    /// can be frozen ([`MockClient::set_clock`], which is what makes a *same-second* claim
    /// race expressible), and a rival claim marker can be injected into the next note read
    /// to exercise the lost-race path.
    #[derive(Default)]
    pub(crate) struct MockClient {
        me: Mutex<User>,
        issues: Mutex<Vec<Seeded>>,
        mrs: Mutex<Vec<SeededMr>>,
        /// Note threads, keyed by `(kind, iid)`.
        notes: Mutex<HashMap<(ItemKind, u64), Vec<Note>>>,
        actions: Mutex<Vec<Action>>,
        /// How many times `current_user` was asked, so a test can pin the identity to
        /// one resolve per armed service.
        user_reads: Mutex<u32>,
        fail_stage: Mutex<Option<&'static str>>,
        /// How many notes have been posted through `post_comment`, so each returned note
        /// gets a distinct id and a strictly newer timestamp than any seeded one.
        posted: Mutex<u64>,
        /// When set, every posted note is stamped this many seconds after the epoch
        /// instead of the minted per-post one — so two posts land in the *same second*
        /// with distinct, increasing ids and the claim's tie-break is the only thing that
        /// can decide.
        post_clock: Mutex<Option<u64>>,
        /// When set, the next `list_notes` first drops this rival claim marker into the
        /// thread: a marker that landed between our post and our re-read, with the test
        /// choosing its side of the `(created_at, id)` order.
        rival_on_read: Mutex<Option<Note>>,
    }

    /// The id/timestamp base a `post_comment` mints from: far above the small ids and
    /// epoch-relative seconds tests seed, so a posted note is always the newest.
    const POSTED_ID_BASE: u64 = 1_000_000;

    impl MockClient {
        /// A fresh forge whose authenticated user is `(id, username)`.
        pub(crate) fn new(id: u64, username: &str) -> Self {
            let c = Self::default();
            *lock(&c.me) = User {
                id,
                username: username.to_string(),
            };
            c
        }

        /// Seed an issue (open, with the given labels and no assignees).
        pub(crate) fn add_issue(&self, iid: u64, title: &str, body: &str, labels: &[&str]) {
            self.add_issue_assigned(iid, title, body, labels, &[]);
        }

        /// Seed an open issue with the given labels **and** assignees (`(id, username)`
        /// each). Unlike driving `set_assignees`, this records no `Action`, so it seeds a
        /// starting state without polluting the recorded-mutation assertions.
        pub(crate) fn add_issue_assigned(
            &self,
            iid: u64,
            title: &str,
            body: &str,
            labels: &[&str],
            assignees: &[(u64, &str)],
        ) {
            lock(&self.issues).push(Seeded {
                issue: Issue {
                    iid,
                    title: title.to_string(),
                    body: body.to_string(),
                    state: "opened".to_string(),
                    labels: labels.iter().map(|s| s.to_string()).collect(),
                },
                assignees: assignees
                    .iter()
                    .map(|(id, username)| User {
                        id: *id,
                        username: (*username).to_string(),
                    })
                    .collect(),
            });
        }

        /// Close a seeded issue, as a human does.
        pub(crate) fn close(&self, iid: u64) {
            self.mutate_item(ItemKind::Issue, iid, |_assignees, _labels, state| {
                *state = "closed".to_string();
            });
        }

        /// Seed an open MR authored by `(author_id, author_name)`, on `source_branch`, with
        /// no labels or assignees.
        pub(crate) fn add_mr(
            &self,
            iid: u64,
            author_id: u64,
            author_name: &str,
            source_branch: &str,
        ) {
            lock(&self.mrs).push(SeededMr {
                mr: MergeRequest {
                    iid,
                    source_branch: source_branch.to_string(),
                    author: User {
                        id: author_id,
                        username: author_name.to_string(),
                    },
                },
                state: "opened".to_string(),
                labels: Vec::new(),
                assignees: Vec::new(),
            });
        }

        /// Merge or close a seeded MR, as a human does: it leaves the open set.
        pub(crate) fn close_mr(&self, iid: u64) {
            self.mutate_item(ItemKind::MergeRequest, iid, |_assignees, _labels, state| {
                *state = "merged".to_string();
            });
        }

        /// Seed a note on item `(kind, iid)` by `(author_id, author_name)`, updated `secs`
        /// after epoch, with a synthesized `note {id}` body.
        pub(crate) fn add_note(
            &self,
            kind: ItemKind,
            iid: u64,
            id: u64,
            author_id: u64,
            author_name: &str,
            secs: u64,
        ) {
            self.add_note_body(
                kind,
                iid,
                id,
                author_id,
                author_name,
                &format!("note {id}"),
                secs,
            );
        }

        /// Seed a note with an explicit body — what a human wrote, and what seeds a stale
        /// or crashed claim marker.
        // A seeding helper: every field is one column of the note it writes, and the
        // `(kind, iid)` head is GitLab's item coordinate.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn add_note_body(
            &self,
            kind: ItemKind,
            iid: u64,
            id: u64,
            author_id: u64,
            author_name: &str,
            body: &str,
            secs: u64,
        ) {
            // A seeded note is unedited: written when it was last touched.
            self.add_note_edited(kind, iid, id, author_id, author_name, body, secs, secs);
        }

        /// Seed a note written at `created` and last edited at `updated` — the shape that
        /// tells the claim's order key (creation) apart from its liveness (the edit).
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn add_note_edited(
            &self,
            kind: ItemKind,
            iid: u64,
            id: u64,
            author_id: u64,
            author_name: &str,
            body: &str,
            created: u64,
            updated: u64,
        ) {
            lock(&self.notes)
                .entry((kind, iid))
                .or_default()
                .push(Note {
                    id,
                    body: body.to_string(),
                    author: User {
                        id: author_id,
                        username: author_name.to_string(),
                    },
                    created_at: UNIX_EPOCH + Duration::from_secs(created),
                    updated_at: UNIX_EPOCH + Duration::from_secs(updated),
                });
        }

        /// Make every call belonging to `stage` fail with a transport error.
        pub(crate) fn fail(&self, stage: &'static str) {
            *lock(&self.fail_stage) = Some(stage);
        }

        /// Stop failing.
        pub(crate) fn clear_failure(&self) {
            *lock(&self.fail_stage) = None;
        }

        /// Freeze the second every subsequent `post_comment` is stamped with, leaving the
        /// id counter alone. Two claims posted under one frozen clock therefore carry
        /// **equal** `created_at` and distinct, increasing ids — the same-second race,
        /// where the id tie-break is the only thing that can decide the winner.
        pub(crate) fn set_clock(&self, secs: u64) {
            *lock(&self.post_clock) = Some(secs);
        }

        /// Arm a concurrent rival: the next `list_notes` finds a rival `[afkd-claim]`
        /// marker in the thread — one that landed between our post and our re-read —
        /// authored by `(owner_id, owner)`, with note id `id` and created `secs` after
        /// the epoch.
        pub(crate) fn rival_claims_next(&self, owner_id: u64, owner: &str, id: u64, secs: u64) {
            *lock(&self.rival_on_read) = Some(Note {
                id,
                body: crate::claim::claim_text(owner),
                author: User {
                    id: owner_id,
                    username: owner.to_string(),
                },
                created_at: UNIX_EPOCH + Duration::from_secs(secs),
                updated_at: UNIX_EPOCH + Duration::from_secs(secs),
            });
        }

        /// The recorded mutations, in order.
        pub(crate) fn actions(&self) -> Vec<Action> {
            lock(&self.actions).clone()
        }

        /// How many times `current_user` has been asked.
        pub(crate) fn user_reads(&self) -> u32 {
            *lock(&self.user_reads)
        }

        /// The current assignee ids on an item (for assertions).
        pub(crate) fn assignee_ids(&self, kind: ItemKind, iid: u64) -> Vec<u64> {
            self.with_item(kind, iid, |item| item.iter().map(|u| u.id).collect())
                .unwrap_or_default()
        }

        /// Whether item `iid` carries label `name` (for assertions).
        pub(crate) fn has_label(&self, kind: ItemKind, iid: u64, name: &str) -> bool {
            match kind {
                ItemKind::Issue => lock(&self.issues)
                    .iter()
                    .find(|s| s.issue.iid == iid)
                    .is_some_and(|s| s.issue.has_label(name)),
                ItemKind::MergeRequest => lock(&self.mrs)
                    .iter()
                    .find(|s| s.mr.iid == iid)
                    .is_some_and(|s| s.labels.iter().any(|l| l == name)),
            }
        }

        /// Read the assignees of an item via a closure.
        fn with_item<T>(
            &self,
            kind: ItemKind,
            iid: u64,
            f: impl FnOnce(&[User]) -> T,
        ) -> Option<T> {
            match kind {
                ItemKind::Issue => lock(&self.issues)
                    .iter()
                    .find(|s| s.issue.iid == iid)
                    .map(|s| f(&s.assignees)),
                ItemKind::MergeRequest => lock(&self.mrs)
                    .iter()
                    .find(|s| s.mr.iid == iid)
                    .map(|s| f(&s.assignees)),
            }
        }

        /// Apply a mutation to an item's `(assignees, labels, state)` tuple.
        fn mutate_item(
            &self,
            kind: ItemKind,
            iid: u64,
            f: impl FnOnce(&mut Vec<User>, &mut Vec<String>, &mut String),
        ) {
            match kind {
                ItemKind::Issue => {
                    if let Some(s) = lock(&self.issues).iter_mut().find(|s| s.issue.iid == iid) {
                        f(&mut s.assignees, &mut s.issue.labels, &mut s.issue.state);
                    }
                }
                ItemKind::MergeRequest => {
                    if let Some(s) = lock(&self.mrs).iter_mut().find(|s| s.mr.iid == iid) {
                        f(&mut s.assignees, &mut s.labels, &mut s.state);
                    }
                }
            }
        }

        fn guard(&self, stage: &'static str) -> Result<(), GitlabError> {
            if *lock(&self.fail_stage) == Some(stage) {
                Err(GitlabError::Transport {
                    stage,
                    reason: "mock failure".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    impl GitlabClient for MockClient {
        fn current_user(&self) -> Result<User, GitlabError> {
            *lock(&self.user_reads) += 1;
            self.guard("current user")?;
            Ok(lock(&self.me).clone())
        }

        fn list_issues(
            &self,
            _project: &Project,
            state: &str,
            labels: &str,
        ) -> Result<Vec<Issue>, GitlabError> {
            self.guard("list issues")?;
            Ok(lock(&self.issues)
                .iter()
                .map(|s| &s.issue)
                .filter(|i| state == "all" || i.state == state)
                .filter(|i| labels.is_empty() || i.has_label(labels))
                .cloned()
                .collect())
        }

        fn list_open_mrs(&self, _project: &Project) -> Result<Vec<MergeRequest>, GitlabError> {
            self.guard("list merge requests")?;
            Ok(lock(&self.mrs)
                .iter()
                .filter(|s| s.state == "opened")
                .map(|s| s.mr.clone())
                .collect())
        }

        fn get_assignees(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
        ) -> Result<Vec<User>, GitlabError> {
            self.guard("get item")?;
            self.with_item(kind, iid, |a| a.to_vec())
                .ok_or(GitlabError::Status {
                    stage: "get item",
                    status: 404,
                })
        }

        fn set_assignees(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            ids: &[u64],
        ) -> Result<(), GitlabError> {
            self.guard("set assignees")?;
            self.mutate_item(kind, iid, |assignees, _labels, _state| {
                *assignees = ids
                    .iter()
                    .map(|id| User {
                        id: *id,
                        username: String::new(),
                    })
                    .collect();
            });
            lock(&self.actions).push(Action::Assign {
                kind,
                iid,
                ids: ids.to_vec(),
            });
            Ok(())
        }

        fn add_label(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            name: &str,
        ) -> Result<(), GitlabError> {
            self.guard("add label")?;
            self.mutate_item(kind, iid, |_assignees, labels, _state| {
                if !labels.iter().any(|l| l == name) {
                    labels.push(name.to_string());
                }
            });
            lock(&self.actions).push(Action::Label {
                kind,
                iid,
                name: name.to_string(),
            });
            Ok(())
        }

        fn remove_label(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            name: &str,
        ) -> Result<(), GitlabError> {
            self.guard("remove label")?;
            self.mutate_item(kind, iid, |_assignees, labels, _state| {
                labels.retain(|l| l != name);
            });
            lock(&self.actions).push(Action::Unlabel {
                kind,
                iid,
                name: name.to_string(),
            });
            Ok(())
        }

        fn set_state(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            event: &str,
        ) -> Result<(), GitlabError> {
            self.guard("set state")?;
            self.mutate_item(kind, iid, |_assignees, _labels, state| {
                if event == "close" {
                    *state = "closed".to_string();
                }
            });
            lock(&self.actions).push(Action::State {
                kind,
                iid,
                event: event.to_string(),
            });
            Ok(())
        }

        fn list_notes(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
        ) -> Result<Vec<Note>, GitlabError> {
            self.guard("list notes")?;
            // An armed rival's marker lands in the thread just before this read — the
            // concurrent claim that was posted while we were settling. It joins the
            // thread for good, as a real one would.
            if let Some(rival) = lock(&self.rival_on_read).take() {
                lock(&self.notes)
                    .entry((kind, iid))
                    .or_default()
                    .push(rival);
            }
            Ok(lock(&self.notes)
                .get(&(kind, iid))
                .cloned()
                .unwrap_or_default())
        }

        fn post_comment(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            text: &str,
        ) -> Result<Note, GitlabError> {
            self.guard("post comment")?;
            // A posted note joins the item's thread, authored by the authenticated user
            // and newest — as the forge does. A later read must see it, or the claim
            // could never find the marker it just wrote.
            let me = lock(&self.me).clone();
            let n = {
                let mut posted = lock(&self.posted);
                *posted += 1;
                *posted
            };
            // The stamp is the frozen clock when one is set (so successive posts tie in
            // the same second), else the minted, strictly-increasing one.
            let at = UNIX_EPOCH
                + Duration::from_secs(lock(&self.post_clock).unwrap_or(POSTED_ID_BASE + n));
            let posted = Note {
                id: POSTED_ID_BASE + n,
                body: text.to_string(),
                author: me,
                created_at: at,
                updated_at: at,
            };
            lock(&self.notes)
                .entry((kind, iid))
                .or_default()
                .push(posted.clone());
            lock(&self.actions).push(Action::Comment {
                kind,
                iid,
                body: text.to_string(),
            });
            Ok(posted)
        }

        fn delete_comment(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            note_id: u64,
        ) -> Result<(), GitlabError> {
            self.guard("delete comment")?;
            if let Some(thread) = lock(&self.notes).get_mut(&(kind, iid)) {
                thread.retain(|n| n.id != note_id);
            }
            lock(&self.actions).push(Action::DeleteComment {
                kind,
                iid,
                id: note_id,
            });
            Ok(())
        }

        fn edit_comment(
            &self,
            _project: &Project,
            kind: ItemKind,
            iid: u64,
            note_id: u64,
            text: &str,
        ) -> Result<(), GitlabError> {
            self.guard("edit comment")?;
            // The forge rewrites the body and moves `updated_at` — the half a rival's
            // claim decision reads as liveness — leaving `created_at` (the order half)
            // exactly where it was.
            let edited_at =
                UNIX_EPOCH + Duration::from_secs(lock(&self.post_clock).unwrap_or(POSTED_ID_BASE));
            if let Some(thread) = lock(&self.notes).get_mut(&(kind, iid)) {
                for note in thread.iter_mut().filter(|n| n.id == note_id) {
                    note.body = text.to_string();
                    note.updated_at = edited_at;
                }
            }
            lock(&self.actions).push(Action::EditComment {
                kind,
                iid,
                id: note_id,
                body: text.to_string(),
            });
            Ok(())
        }
    }

    fn project() -> Project {
        Project::new("group/widgets")
    }

    #[test]
    fn set_then_reread_reflects_the_assignee_ids() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.set_assignees(&project(), ItemKind::Issue, 7, &[1])
            .unwrap();
        assert_eq!(c.assignee_ids(ItemKind::Issue, 7), vec![1]);
    }

    /// An armed rival's marker joins the thread on the **next read**, not on a write:
    /// that is where a concurrent claim becomes visible to us, and it stays there
    /// afterwards (a real rival's marker does not evaporate).
    #[test]
    fn rival_injection_lands_in_the_next_note_read() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &["afkd::ready"]);
        c.rival_claims_next(99, "rival", 42, 100);

        let first = c.list_notes(&project(), ItemKind::Issue, 7).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|n| (n.id, n.body.clone()))
                .collect::<Vec<_>>(),
            vec![(42, "[afkd-claim] owner=rival".to_string())]
        );
        // Armed once; the marker persists, but no second rival appears.
        assert_eq!(
            c.list_notes(&project(), ItemKind::Issue, 7).unwrap().len(),
            1
        );
        // And it is scoped to the item read: issue #8 is a different thread.
        assert!(c
            .list_notes(&project(), ItemKind::Issue, 8)
            .unwrap()
            .is_empty());
    }

    /// The same-second race is only expressible because the frozen post clock stamps
    /// every posted note alike while the ids keep climbing — exactly the shape
    /// `won_claim`'s `(created_at, id)` tie-break exists for.
    #[test]
    fn a_frozen_post_clock_ties_the_second_but_not_the_ids() {
        let c = MockClient::new(1, "me");
        c.add_issue(7, "T", "B", &[]);
        c.set_clock(1_700_000_000);

        let a = c
            .post_comment(&project(), ItemKind::Issue, 7, "[afkd-claim] owner=a")
            .unwrap();
        let b = c
            .post_comment(&project(), ItemKind::Issue, 7, "[afkd-claim] owner=b")
            .unwrap();

        assert_eq!(a.created_at, b.created_at, "one frozen second");
        assert!(a.id < b.id, "distinct, increasing ids: {} {}", a.id, b.id);
        // Both joined the thread, in post order.
        assert_eq!(
            c.list_notes(&project(), ItemKind::Issue, 7)
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![a.id, b.id]
        );
    }

    /// A label write lands on the item its `kind` names: an issue and an MR sharing iid 3
    /// are two items, and the MR's label never shows on the issue.
    #[test]
    fn label_add_then_remove_by_name_records_the_kind() {
        let c = MockClient::new(1, "me");
        c.add_issue(3, "T", "B", &[]);
        c.add_mr(3, 1, "me", "feature/x");
        c.add_label(&project(), ItemKind::MergeRequest, 3, "afkd::claimed")
            .unwrap();
        assert!(c.has_label(ItemKind::MergeRequest, 3, "afkd::claimed"));
        assert!(!c.has_label(ItemKind::Issue, 3, "afkd::claimed"));
        c.remove_label(&project(), ItemKind::MergeRequest, 3, "afkd::claimed")
            .unwrap();
        assert!(!c.has_label(ItemKind::MergeRequest, 3, "afkd::claimed"));
        assert!(c.actions().iter().any(|a| matches!(
            a,
            Action::Unlabel { kind: ItemKind::MergeRequest, name, .. } if name == "afkd::claimed"
        )));
    }

    /// The open-MR listing is the open set only: a merged MR drops out of it, and the
    /// listing has its own failure stage.
    #[test]
    fn list_open_mrs_lists_only_the_open_set() {
        let c = MockClient::new(1, "me");
        c.add_mr(7, 1, "me", "feature/重试-backoff");
        c.add_mr(8, 2, "陳大文", "fix/y");
        c.close_mr(8);
        assert_eq!(
            c.list_open_mrs(&project()).unwrap(),
            [MergeRequest {
                iid: 7,
                source_branch: "feature/重试-backoff".into(),
                author: User {
                    id: 1,
                    username: "me".into()
                },
            }]
        );
        c.fail("list merge requests");
        assert_eq!(
            c.list_open_mrs(&project()).unwrap_err().stage(),
            "list merge requests"
        );
    }

    #[test]
    fn failure_injection_is_scoped_to_a_stage() {
        let c = MockClient::new(1, "me");
        c.fail("list issues");
        assert!(c.list_issues(&project(), "opened", "").is_err());
        assert!(c.current_user().is_ok(), "another stage still answers");
        c.clear_failure();
        assert!(c.list_issues(&project(), "opened", "").is_ok());
    }

    /// The claim-marker round trip through the mock: the posted note comes back
    /// identified (a minted id above every seeded one, and a creation time) **and** joins
    /// the item's thread — the claim's re-read has to find the marker it just wrote — and
    /// deleting a note by id takes it back out, with the `kind` recorded, as every other
    /// mutation is.
    #[test]
    fn post_then_delete_comment_round_trips_by_id() {
        let c = MockClient::new(7, "björn-öst[bot]");
        c.add_note(ItemKind::MergeRequest, 3, 42, 99, "josefandersson", 100);
        let posted = c
            .post_comment(
                &project(),
                ItemKind::MergeRequest,
                3,
                "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
            )
            .unwrap();

        assert!(
            posted.id > 42,
            "a posted id must outrank every seeded one, got {}",
            posted.id
        );
        assert_eq!(posted.author.username, "björn-öst[bot]");
        assert_eq!(posted.created_at, posted.updated_at);
        assert!(posted.created_at > UNIX_EPOCH + Duration::from_secs(100));
        assert_eq!(
            c.list_notes(&project(), ItemKind::MergeRequest, 3)
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![42, posted.id],
            "the posted note joined the thread the claim re-reads"
        );
        // It joined *that item's* thread: MR !4 is a separate one, and so is issue #3.
        assert!(c
            .list_notes(&project(), ItemKind::MergeRequest, 4)
            .unwrap()
            .is_empty());
        assert!(c
            .list_notes(&project(), ItemKind::Issue, 3)
            .unwrap()
            .is_empty());

        c.delete_comment(&project(), ItemKind::MergeRequest, 3, posted.id)
            .unwrap();
        assert_eq!(
            c.list_notes(&project(), ItemKind::MergeRequest, 3)
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![42],
            "the deleted note left the thread"
        );
        assert!(c.actions().contains(&Action::DeleteComment {
            kind: ItemKind::MergeRequest,
            iid: 3,
            id: posted.id,
        }));
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn encode_project_passes_ids_and_encodes_paths() {
        // A numeric id passes through untouched.
        assert_eq!(encode_project("123"), "123");
        // A path-with-namespace percent-encodes the slashes (and only the slashes).
        assert_eq!(encode_project("acme/widgets"), "acme%2Fwidgets");
        assert_eq!(
            encode_project("group/subgroup/widgets"),
            "group%2Fsubgroup%2Fwidgets"
        );
        // The unreserved set passes through; everything else is escaped byte by byte.
        assert_eq!(encode_project("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(encode_project("björn/verktyg"), "bj%C3%B6rn%2Fverktyg");
        // Not all digits is a path, however numeric it starts.
        assert_eq!(encode_project("42/widgets"), "42%2Fwidgets");
        assert_eq!(encode_project(""), "");
    }

    #[test]
    fn gitlab_api_base_defaults_and_trims() {
        assert_eq!(gitlab_api_base(""), "https://gitlab.com/api/v4");
        assert_eq!(gitlab_api_base("   "), "https://gitlab.com/api/v4");
        assert_eq!(
            gitlab_api_base("https://gitlab.example.com"),
            "https://gitlab.example.com/api/v4"
        );
        assert_eq!(
            gitlab_api_base(" https://gitlab.example.com/ "),
            "https://gitlab.example.com/api/v4"
        );
    }

    #[test]
    fn parse_issues_reads_iid_state_title_and_labels() {
        let body = r#"[
            {"iid":4,"title":"修复 the retry storm 🚨","description":"do it\n\n```\nx\n```",
             "state":"opened","labels":["afkd::ready"],
             "assignees":[{"id":8,"username":"bot"}]}
        ]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].iid, 4);
        assert_eq!(issues[0].title, "修复 the retry storm 🚨");
        assert_eq!(issues[0].body, "do it\n\n```\nx\n```");
        assert_eq!(issues[0].state, "opened");
        assert!(issues[0].has_label("afkd::ready"));
    }

    /// An issue with no description, no state and no labels still parses — the brief is
    /// the title alone, and the state defaults to open as the built-in reads it — and an
    /// entry with no `iid` is skipped rather than faulting the whole list.
    #[test]
    fn parse_issues_defaults_the_absent_fields() {
        let body = r#"[{"iid":5,"title":"Bare","description":null},{"title":"no iid"}]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(
            issues,
            [Issue {
                iid: 5,
                title: "Bare".into(),
                body: String::new(),
                state: "opened".into(),
                labels: Vec::new(),
            }]
        );
    }

    #[test]
    fn parse_issues_reads_labels_given_as_objects() {
        // GitLab returns labels as bare strings *or* as `{ "name": … }` objects (the
        // `with_labels_details` shape). The object form resolves through the `name`
        // field; a label that is neither a string nor a named object is dropped rather
        // than faulting the parse.
        let body = r#"[
            {"iid":4,"title":"Fix","description":"","state":"opened",
             "labels":[{"id":9,"name":"afkd::ready"}, 12],
             "assignees":[]}
        ]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].labels, ["afkd::ready"]);
    }

    #[test]
    fn parse_mrs_reads_source_branch_and_author() {
        let body = r#"[
            {"iid":12,"title":"修复 the retry storm 🚨","description":"","source_branch":"feature/重试-backoff",
             "author":{"id":8,"username":"björn-öst[bot]"},"state":"opened","labels":["afkd::claimed"]},
            {"iid":13,"source_branch":"fix/y"},
            {"title":"no iid"}
        ]"#;
        let mrs = parse_mrs("list merge requests", body).unwrap();
        assert_eq!(
            mrs,
            [
                MergeRequest {
                    iid: 12,
                    source_branch: "feature/重试-backoff".into(),
                    author: User {
                        id: 8,
                        username: "björn-öst[bot]".into()
                    },
                },
                // A missing author is nobody (id 0, no username), as the built-in reads it
                // — never `me`, so `author_me` skips it — and an entry with no `iid` is
                // dropped rather than faulting the list.
                MergeRequest {
                    iid: 13,
                    source_branch: "fix/y".into(),
                    author: User::default(),
                },
            ]
        );
        let err = parse_mrs("list merge requests", r#"{"not":"array"}"#).unwrap_err();
        assert_eq!(err.stage(), "list merge requests");
    }

    #[test]
    fn parse_assignees_reads_the_item_assignees() {
        let body =
            r#"{"iid":4,"assignees":[{"id":1,"username":"me"},{"id":2,"username":"陳大文"}]}"#;
        let users = parse_assignees("get item", body).unwrap();
        assert_eq!(
            users,
            [
                User {
                    id: 1,
                    username: "me".into()
                },
                User {
                    id: 2,
                    username: "陳大文".into()
                }
            ]
        );
        // An item with no `assignees` field has none.
        assert!(parse_assignees("get item", r#"{"iid":4}"#)
            .unwrap()
            .is_empty());
    }

    /// The three shapes a real thread mixes: a fresh note, an **edited** one whose
    /// `created_at` is two hours before its `updated_at`, and one the forge sent without a
    /// `created_at` at all. The two times are distinct fields, read through the same
    /// RFC-3339 reader (so GitLab's fractional-second stamps land where a bare `Z` one
    /// would), and a missing one degrades to the epoch rather than borrowing its
    /// sibling's value.
    #[test]
    fn parse_notes_read_authors_and_times() {
        let notes = parse_notes(
            "list notes",
            r#"[
                {"id":1,"body":"hi","author":{"id":9,"username":"human"},
                 "created_at":"2021-01-01T00:00:00.000Z","updated_at":"2021-01-01T00:00:00.000Z"},
                {"id":2,"body":"The token expires mid-retry — see §4 🙏","author":{"id":10,"username":"björn-öst"},
                 "created_at":"2021-01-01T00:00:00.123Z","updated_at":"2021-01-01T02:00:00.456Z"},
                {"id":3,"body":"no creation stamp","author":{"id":9,"username":"human"},
                 "updated_at":"2021-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(notes[0].author.username, "human");
        assert_eq!(notes[0].created_at, t0);
        assert_eq!(notes[0].updated_at, t0);

        // Edited: written at 00:00Z, touched two hours later.
        assert_eq!(notes[1].author.username, "björn-öst");
        assert_eq!(notes[1].body, "The token expires mid-retry — see §4 🙏");
        assert_eq!(notes[1].created_at, t0);
        assert_eq!(notes[1].updated_at, t0 + Duration::from_secs(7_200));

        // Absent `created_at` degrades to the epoch — distinguishable from a stamp equal
        // to `updated_at`, which is what a fallback to the sibling would give.
        assert_eq!(notes[2].created_at, UNIX_EPOCH);
        assert_eq!(notes[2].updated_at, t0);
    }

    /// The POST reply decodes through the very same `value_to_note` the list reply does,
    /// and is **strict**: a reply carrying no `id` is a staged decode error, never a
    /// synthesized note a claim could be built on.
    #[test]
    fn parse_note_reads_the_posted_note_or_fails_loudly() {
        let posted = parse_note(
            "post comment",
            r#"{"id":90210,"body":"[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                "author":{"id":7,"username":"björn-öst[bot]"},
                "created_at":"2026-07-20T09:00:00.512Z","updated_at":"2026-07-20T09:00:00.512Z"}"#,
        )
        .unwrap();
        assert_eq!(posted.id, 90210);
        assert_eq!(posted.author.username, "björn-öst[bot]");
        assert_eq!(
            posted.body,
            "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00"
        );
        assert_eq!(posted.created_at, posted.updated_at);

        let err = parse_note("post comment", r#"{"message":"403 Forbidden"}"#).unwrap_err();
        assert!(
            matches!(
                err,
                GitlabError::Decode {
                    stage: "post comment",
                    ..
                }
            ),
            "a reply with no id must be a staged decode error, got {err:?}"
        );
    }

    #[test]
    fn decode_failures_are_tagged_with_their_stage() {
        let err = parse_issues("list issues", "not json").unwrap_err();
        assert_eq!(err.stage(), "list issues");
        let err = parse_issues("list issues", r#"{"not":"array"}"#).unwrap_err();
        assert!(matches!(err, GitlabError::Decode { .. }));
        let err = parse_user("current user", r#"{"username":"no id"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "gitlab current user: undecodable response (user missing id/username)"
        );
    }

    /// Each variant reports its stage and renders the built-in's own sentence.
    #[test]
    fn gitlab_error_stage_and_sentence_for_every_variant() {
        let status = GitlabError::Status {
            stage: "list issues",
            status: 500,
        };
        assert_eq!(status.stage(), "list issues");
        assert_eq!(
            status.to_string(),
            "gitlab list issues: forge returned status 500"
        );
        let transport = GitlabError::Transport {
            stage: "get item",
            reason: "io: Connection refused".into(),
        };
        assert_eq!(transport.stage(), "get item");
        assert_eq!(
            transport.to_string(),
            "gitlab get item: no response (io: Connection refused)"
        );
        let decode = GitlabError::Decode {
            stage: "set state",
            reason: "y".into(),
        };
        assert_eq!(decode.stage(), "set state");
        assert_eq!(
            decode.to_string(),
            "gitlab set state: undecodable response (y)"
        );
    }
}

#[cfg(test)]
mod http_tests {
    //! The real [`Gitlab`] HTTP client's request construction, exercised against a
    //! loopback `Stub` (a local socket — **no external network**). Each test pins one
    //! endpoint's method + path (with the `/api/v4` prefix the client builds) so every
    //! cited path has a test in the one place the paths live.

    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// What the stub saw on the wire: method, target (path?query), headers, body.
    struct Captured {
        method: String,
        target: String,
        headers: Vec<String>,
        body: String,
    }

    impl Captured {
        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or("")
        }
        fn has_query(&self, key: &str, value: &str) -> bool {
            let q = match self.target.split_once('?') {
                Some((_, q)) => q,
                None => return false,
            };
            q.split('&').any(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                k == key && url_decode(v) == value
            })
        }
        fn header(&self, name: &str) -> Option<String> {
            // Match the header name case-insensitively, but return the value with its
            // original case preserved (the token's case matters).
            let want = format!("{}:", name.to_ascii_lowercase());
            self.headers.iter().find_map(|h| {
                if h.to_ascii_lowercase().starts_with(&want) {
                    Some(h[want.len()..].trim().to_string())
                } else {
                    None
                }
            })
        }
        fn json(&self) -> Value {
            serde_json::from_str(&self.body).expect("a JSON body")
        }
    }

    fn url_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    let hi = (bytes[i + 1] as char).to_digit(16);
                    let lo = (bytes[i + 2] as char).to_digit(16);
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                        continue;
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A one-shot HTTP/1.1 stub: serves one canned reply to one request and hands the
    /// captured request back, driving the real [`Gitlab`] code over a socket.
    struct Stub {
        base: String,
        handle: Option<JoinHandle<()>>,
        rx: mpsc::Receiver<Captured>,
    }

    impl Stub {
        fn serve(response: impl Into<String>) -> Self {
            let response = response.into();
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut start = String::new();
                reader.read_line(&mut start).expect("read start line");
                let mut parts = start.trim_end().split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let mut headers = Vec::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read header");
                    let trimmed = line.trim_end().to_string();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        content_length = rest.trim().parse().unwrap_or(0);
                    }
                    headers.push(trimmed);
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    reader.read_exact(&mut body).expect("read body");
                }
                stream.write_all(response.as_bytes()).expect("write reply");
                stream.flush().ok();
                tx.send(Captured {
                    method,
                    target,
                    headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                })
                .ok();
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                handle: Some(handle),
                rx,
            }
        }

        fn client(&self, token: &str) -> Gitlab {
            Gitlab::new(&self.base, token)
        }

        fn captured(self) -> Captured {
            let captured = self.rx.recv().expect("stub captured a request");
            if let Some(h) = self.handle {
                let _ = h.join();
            }
            captured
        }
    }

    fn ok_json(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// The `201 Created` + created resource a note POST really answers with.
    fn created_json(body: &str) -> String {
        format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// The `204 No Content` a note DELETE really answers with.
    fn no_content() -> &'static str {
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
    }

    fn project() -> Project {
        Project::new("acme/widgets")
    }

    #[test]
    fn current_user_gets_user_with_private_token_auth() {
        let stub = Stub::serve(ok_json(r#"{"id":8,"username":"björn-öst[bot]"}"#));
        let client = stub.client("SECRET");
        let user = client.current_user().unwrap();
        assert_eq!(user.username, "björn-öst[bot]");
        assert_eq!(user.id, 8);
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v4/user");
        assert_eq!(req.header("private-token").as_deref(), Some("SECRET"));
        assert_eq!(req.header("authorization"), None, "no second credential");
    }

    #[test]
    fn list_issues_encodes_the_project_and_sends_state_and_labels() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        // A scoped label survives verbatim as the `labels` query value.
        client
            .list_issues(&project(), "opened", "afkd::ready")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/issues");
        assert!(req.has_query("state", "opened"));
        assert!(req.has_query("labels", "afkd::ready"));
    }

    #[test]
    fn project_is_encoded_as_id() {
        // A numeric id passes through as `/projects/123/…`.
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client
            .list_issues(&Project::new("123"), "opened", "")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.path(), "/api/v4/projects/123/issues");

        // A nested path-with-namespace percent-encodes every slash.
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client
            .list_issues(&Project::new("g/sub/widgets"), "opened", "")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.path(), "/api/v4/projects/g%2Fsub%2Fwidgets/issues");
    }

    #[test]
    fn list_open_mrs_sends_state_opened() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client.list_open_mrs(&project()).unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/merge_requests");
        assert!(req.has_query("state", "opened"));
    }

    #[test]
    fn get_assignees_reads_the_issue_item_path() {
        let stub = Stub::serve(ok_json(
            r#"{"iid":5,"assignees":[{"id":1,"username":"me"}]}"#,
        ));
        let client = stub.client("t");
        let users = client
            .get_assignees(&project(), ItemKind::Issue, 5)
            .unwrap();
        assert_eq!(users[0].id, 1);
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/issues/5");
    }

    #[test]
    fn set_assignees_puts_the_assignee_ids_on_the_issue_path() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client
            .set_assignees(&project(), ItemKind::Issue, 5, &[99, 1])
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/issues/5");
        assert_eq!(req.json(), json!({"assignee_ids": [99, 1]}));
        assert_eq!(
            req.header("content-type").as_deref(),
            Some("application/json")
        );
    }

    #[test]
    fn set_assignees_puts_the_assignee_ids_on_the_mr_path() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client
            .set_assignees(&project(), ItemKind::MergeRequest, 5, &[1])
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        // The MR path segment, not the issue one.
        assert_eq!(
            req.path(),
            "/api/v4/projects/acme%2Fwidgets/merge_requests/5"
        );
        assert_eq!(req.json(), json!({"assignee_ids": [1]}));
    }

    #[test]
    fn add_label_puts_add_labels_by_name() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client
            .add_label(&project(), ItemKind::Issue, 5, "afkd::claimed")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/issues/5");
        assert_eq!(req.json(), json!({"add_labels": "afkd::claimed"}));
    }

    #[test]
    fn remove_label_puts_remove_labels_with_the_exact_scoped_name() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client
            .remove_label(&project(), ItemKind::Issue, 5, "afkd::ready")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/issues/5");
        // A single named label in `remove_labels` — never an all-clearing path.
        assert_eq!(req.json(), json!({"remove_labels": "afkd::ready"}));
    }

    #[test]
    fn set_state_puts_state_event_close() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client
            .set_state(&project(), ItemKind::Issue, 5, "close")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path(), "/api/v4/projects/acme%2Fwidgets/issues/5");
        assert_eq!(req.json(), json!({"state_event": "close"}));
    }

    /// The notes list is `kind`-routed like every other item call: the MR kind reads the
    /// `merge_requests` thread, the issue claim's re-read the `issues` one. A single
    /// hard-coded path would silently read the wrong thread for one of them.
    #[test]
    fn list_notes_reads_the_notes_path_of_each_kind() {
        for (kind, path) in [
            (
                ItemKind::MergeRequest,
                "/api/v4/projects/acme%2Fwidgets/merge_requests/5/notes",
            ),
            (
                ItemKind::Issue,
                "/api/v4/projects/acme%2Fwidgets/issues/5/notes",
            ),
        ] {
            let stub = Stub::serve(ok_json("[]"));
            let client = stub.client("t");
            client.list_notes(&project(), kind, 5).unwrap();
            let req = stub.captured();
            assert_eq!(req.method, "GET");
            assert_eq!(req.path(), path, "{kind:?}");
        }
    }

    /// The wire contract *and* the read-back, on each kind's notes path: GitLab's `201` +
    /// created-note reply is decoded into the id and creation time a claim marker needs.
    /// The body is a multi-line, non-ASCII claim, and the stamps carry the fractional
    /// seconds GitLab emits.
    #[test]
    fn post_comment_posts_the_body_on_the_kind_notes_path() {
        for (kind, path) in [
            (
                ItemKind::Issue,
                "/api/v4/projects/acme%2Fwidgets/issues/5/notes",
            ),
            (
                ItemKind::MergeRequest,
                "/api/v4/projects/acme%2Fwidgets/merge_requests/5/notes",
            ),
        ] {
            let stub = Stub::serve(created_json(
                r#"{"id":90210,"body":"[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                    "author":{"id":7,"username":"björn-öst[bot]"},
                    "created_at":"2026-07-20T09:00:00.512Z","updated_at":"2026-07-20T09:00:00.512Z"}"#,
            ));
            let client = stub.client("t");
            let posted = client
                .post_comment(
                    &project(),
                    kind,
                    5,
                    "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                )
                .unwrap();
            assert_eq!(posted.id, 90210);
            assert_eq!(posted.author.username, "björn-öst[bot]");
            assert_eq!(
                posted.body,
                "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00"
            );
            // Second granularity: the fractional part is dropped, and both stamps read
            // alike.
            assert_eq!(
                posted.created_at,
                UNIX_EPOCH + Duration::from_secs(1_784_538_000)
            );
            assert_eq!(posted.created_at, posted.updated_at);

            let req = stub.captured();
            assert_eq!(req.method, "POST");
            assert_eq!(req.path(), path, "{kind:?}");
            assert_eq!(
                req.json(),
                json!({"body": "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00"})
            );
        }
    }

    /// A reply that is not a note object (no `id`) is a staged decode error, not a
    /// silently-successful post: a claim built on a fabricated id is worse than a loud
    /// failure.
    #[test]
    fn post_comment_rejects_a_reply_that_is_not_a_note() {
        let stub = Stub::serve(ok_json(r#"{"message":"403 Forbidden"}"#));
        let client = stub.client("t");
        let err = client
            .post_comment(&project(), ItemKind::Issue, 5, "hi")
            .unwrap_err();
        assert!(
            matches!(
                err,
                GitlabError::Decode {
                    stage: "post comment",
                    ..
                }
            ),
            "an id-less POST reply must map to a staged Decode error, got {err:?}"
        );
        let _ = stub.captured();
    }

    /// The delete path is **item**-scoped, hanging off the same `item_path` the POST
    /// does — so the `kind` routes the deletion exactly as it routes the post.
    #[test]
    fn delete_comment_deletes_the_kind_note_path() {
        for (kind, path) in [
            (
                ItemKind::Issue,
                "/api/v4/projects/acme%2Fwidgets/issues/5/notes/90210",
            ),
            (
                ItemKind::MergeRequest,
                "/api/v4/projects/acme%2Fwidgets/merge_requests/5/notes/90210",
            ),
        ] {
            let stub = Stub::serve(no_content());
            let client = stub.client("t");
            client.delete_comment(&project(), kind, 5, 90210).unwrap();
            let req = stub.captured();
            assert_eq!(req.method, "DELETE");
            assert_eq!(req.path(), path, "{kind:?}");
        }
    }

    /// The renewal's wire contract: a PUT on the **same** item-scoped note path the
    /// DELETE uses, carrying the renewed body — both kinds, so the routing cannot drift.
    #[test]
    fn edit_comment_puts_the_kind_note_path() {
        for (kind, path) in [
            (
                ItemKind::Issue,
                "/api/v4/projects/acme%2Fwidgets/issues/5/notes/90210",
            ),
            (
                ItemKind::MergeRequest,
                "/api/v4/projects/acme%2Fwidgets/merge_requests/5/notes/90210",
            ),
        ] {
            let stub = Stub::serve(ok_json(r#"{"id":90210,"body":"x"}"#));
            let client = stub.client("t");
            client
                .edit_comment(
                    &project(),
                    kind,
                    5,
                    90210,
                    "[afkd-claim] owner=björn-öst[bot] renewal=7",
                )
                .unwrap();
            let req = stub.captured();
            assert_eq!(req.method, "PUT");
            assert_eq!(req.path(), path, "{kind:?}");
            assert_eq!(
                req.json(),
                json!({"body": "[afkd-claim] owner=björn-öst[bot] renewal=7"})
            );
        }
    }

    /// A reply that promises more body bytes (`Content-Length`) than it sends, then closes
    /// the socket: reading it to a string hits a premature EOF, which surfaces as a
    /// `Decode` tagged with the request's stage — not a `Status`/`Transport`.
    #[test]
    fn a_cut_short_body_maps_to_a_decode_error() {
        let stub = Stub::serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nContent-Type: application/json\r\n\r\n{",
        );
        let client = stub.client("t");
        let err = client.current_user().unwrap_err();
        assert!(
            matches!(
                err,
                GitlabError::Decode {
                    stage: "current user",
                    ..
                }
            ),
            "a cut-short body must map to a staged Decode error, got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn non_success_status_maps_to_status_error_tagged_with_stage() {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let stub = Stub::serve(resp);
        let client = stub.client("t");
        let err = client
            .get_assignees(&project(), ItemKind::Issue, 9)
            .unwrap_err();
        assert!(matches!(
            err,
            GitlabError::Status {
                stage: "get item",
                status: 404
            }
        ));
        let _ = stub.captured();
    }

    #[test]
    fn transport_error_reason_never_carries_the_token() {
        // A transport failure must not fold the token into the logged error. Point a
        // client (with a recognizable token) at a dead port and assert the secret appears
        // nowhere in the resulting error, Display included.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let token = "SUPERSECRET-PAT-9f3a";
        let client = Gitlab::new(&format!("http://127.0.0.1:{port}"), token);
        let err = client.current_user().unwrap_err();
        let GitlabError::Transport { reason, .. } = &err else {
            panic!("expected a transport error, got {err:?}");
        };
        assert!(
            !reason.contains(token),
            "token leaked into reason: {reason}"
        );
        assert!(
            !err.to_string().contains(token),
            "token leaked into Display: {err}"
        );
    }
}
