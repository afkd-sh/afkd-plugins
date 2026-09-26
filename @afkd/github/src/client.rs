//! The mockable GitHub client seam the issue kind drives, the plain value types it
//! exchanges, the typed [`GithubError`], the **real [`Github`] HTTP client** over the REST
//! v3 surface, and a `#[cfg(test)]` in-memory `MockClient` for offline tests.
//!
//! Ported from afkd's `crates/github/src/client.rs`, issue side. All GitHub-specific
//! knowledge lives here — every endpoint path, every request payload, the
//! `Authorization: Bearer <token>` credential, the cloud-vs-GHES host resolution, and the
//! JSON shapes — so the version-drift blast radius is **one file**. Each endpoint carries a
//! citing comment so a drift fix is a one-place edit. The plumbing underneath is
//! [`crate::http`] and [`crate::rfc3339`].
//!
//! GitHub diverges from Gitea in four documented places: the host resolution
//! ([`github_api_base`]), the `Bearer` auth header, the dedicated assignee add/remove
//! endpoints, and label removal by name (never the all-clearing bare `…/labels` path).
//!
//! The seam ([`GithubClient`]) carries only what the kind needs, **by meaning**; the REST
//! shape stays confined to the [`Github`] adapter. The response *parsing* is split into
//! pure functions tested with no network, and the HTTP layer itself is exercised only
//! against a loopback `Stub` (no external host).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http::{
    encode_segment, HttpClient, HttpError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT,
};
use crate::rfc3339::parse_rfc3339;

/// A repository coordinate: the `(owner, name)` pair parsed once from the
/// `repo "owner/name"` setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repo {
    /// The repository owner (a user or org login).
    pub(crate) owner: String,
    /// The repository name.
    pub(crate) name: String,
}

impl Repo {
    /// Parse an `"owner/name"` string into a [`Repo`], or `None` if it is not exactly two
    /// non-empty slash-separated segments.
    pub(crate) fn parse(full: &str) -> Option<Repo> {
        let (owner, name) = full.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// The `"owner/name"` rendering.
    pub(crate) fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// A GitHub user, identified by login. The token's own login is the claim identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct User {
    /// The user's login handle.
    pub(crate) login: String,
}

/// An issue: the unit of work the kind turns into a run. Labels are carried by **name**,
/// which is also how GitHub removes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    /// The per-repo issue number, the natural work-unit id.
    pub(crate) number: u64,
    /// The issue title (the first line of the task brief).
    pub(crate) title: String,
    /// The issue body (the task brief; empty when absent).
    pub(crate) body: String,
    /// The issue state (`open`/`closed`).
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

/// A top-level issue comment, with its author and its two times.
///
/// The two are **not** interchangeable, and each has one reader: `created_at` is when the
/// comment was written, the order a claim marker reads (an edit cannot reshuffle who spoke
/// first) and the `at` afkd's watch is told; `updated_at` is when it was last touched, the
/// liveness a renewed claim marker shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueComment {
    /// The comment's stable id.
    pub(crate) id: u64,
    /// The comment body.
    pub(crate) body: String,
    /// The comment author.
    pub(crate) user: User,
    /// When the comment was created, as GitHub reports it.
    pub(crate) created_at: SystemTime,
    /// When the comment was last updated, as GitHub reports it.
    pub(crate) updated_at: SystemTime,
}

/// A failure reaching GitHub, tagged with the stage it struck so a swallowed poll error
/// logs *where* it happened (afkd's built-in `GithubError`, variant for variant).
#[derive(Debug)]
pub(crate) enum GithubError {
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
impl std::fmt::Display for GithubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GithubError::Status { stage, status } => {
                write!(f, "github {stage}: forge returned status {status}")
            }
            GithubError::Transport { stage, reason } => {
                write!(f, "github {stage}: no response ({reason})")
            }
            GithubError::Decode { stage, reason } => {
                write!(f, "github {stage}: undecodable response ({reason})")
            }
        }
    }
}

impl GithubError {
    /// The stage label this error was tagged with.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            GithubError::Status { stage, .. }
            | GithubError::Transport { stage, .. }
            | GithubError::Decode { stage, .. } => stage,
        }
    }
}

/// The failure vocabulary the shared HTTP spine reports through: the three staged
/// variants above, so the plumbing builds a [`GithubError`] without naming GitHub.
impl HttpError for GithubError {
    fn status(stage: &'static str, status: u16) -> Self {
        GithubError::Status { stage, status }
    }
    fn transport(stage: &'static str, reason: String) -> Self {
        GithubError::Transport { stage, reason }
    }
    fn decode(stage: &'static str, reason: &str) -> Self {
        GithubError::Decode {
            stage,
            reason: reason.to_string(),
        }
    }
}

/// The GitHub operations the issue kind needs, by meaning (not by REST shape).
pub(crate) trait GithubClient {
    /// Resolve the authenticated user (the token's own login) — `GET /user`.
    fn current_user(&self) -> Result<User, GithubError>;

    /// List issues in `repo` filtered by `state` and (comma-separated) `labels` —
    /// `GET /repos/{o}/{r}/issues?state&labels`. Issues only: the pull requests GitHub
    /// folds into that endpoint are not returned.
    fn list_issues(
        &self,
        repo: &Repo,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GithubError>;

    /// Add assignees to an issue (additive) —
    /// `POST /repos/{o}/{r}/issues/{number}/assignees`.
    fn add_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<(), GithubError>;

    /// Remove assignees from an issue —
    /// `DELETE /repos/{o}/{r}/issues/{number}/assignees`.
    fn remove_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<(), GithubError>;

    /// Set an issue's state (`open`/`closed`) — `PATCH /repos/{o}/{r}/issues/{number}`.
    fn set_state(&self, repo: &Repo, index: u64, state: &str) -> Result<(), GithubError>;

    /// Add a label (by name) to an issue — `POST /repos/{o}/{r}/issues/{number}/labels`.
    fn add_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GithubError>;

    /// Remove one label (by name) from an issue —
    /// `DELETE /repos/{o}/{r}/issues/{number}/labels/{name}`.
    fn remove_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GithubError>;

    /// List an issue's top-level comments —
    /// `GET /repos/{o}/{r}/issues/{number}/comments`.
    fn list_issue_comments(
        &self,
        repo: &Repo,
        index: u64,
    ) -> Result<Vec<IssueComment>, GithubError>;

    /// Post a literal comment on an issue, returning the created comment (its id and
    /// creation time, which a claim marker needs to recognise and order its own word) —
    /// `POST /repos/{o}/{r}/issues/{number}/comments`.
    fn post_comment(
        &self,
        repo: &Repo,
        index: u64,
        text: &str,
    ) -> Result<IssueComment, GithubError>;

    /// Delete one comment by id (releasing a claim marker) —
    /// `DELETE /repos/{o}/{r}/issues/comments/{id}`. GitHub scopes the path to the
    /// **repo**, not the issue, so no number is carried.
    fn delete_comment(&self, repo: &Repo, comment_id: u64) -> Result<(), GithubError>;

    /// Rewrite one comment's body by id (renewing a claim marker) —
    /// `PATCH /repos/{o}/{r}/issues/comments/{id}`. Repo-scoped like the delete it sits
    /// beside, and it moves the comment's `updated_at` — which is what a rival's claim
    /// decision reads as liveness.
    fn edit_comment(&self, repo: &Repo, comment_id: u64, text: &str) -> Result<(), GithubError>;
}

/// Sharing a client behind an [`Arc`](std::sync::Arc) keeps it a [`GithubClient`], so a
/// test can retain a handle while also handing the kind a boxed client.
impl<T: GithubClient> GithubClient for std::sync::Arc<T> {
    fn current_user(&self) -> Result<User, GithubError> {
        (**self).current_user()
    }
    fn list_issues(
        &self,
        repo: &Repo,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GithubError> {
        (**self).list_issues(repo, state, labels)
    }
    fn add_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<(), GithubError> {
        (**self).add_assignees(repo, index, assignees)
    }
    fn remove_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<(), GithubError> {
        (**self).remove_assignees(repo, index, assignees)
    }
    fn set_state(&self, repo: &Repo, index: u64, state: &str) -> Result<(), GithubError> {
        (**self).set_state(repo, index, state)
    }
    fn add_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GithubError> {
        (**self).add_label(repo, index, name)
    }
    fn remove_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GithubError> {
        (**self).remove_label(repo, index, name)
    }
    fn list_issue_comments(
        &self,
        repo: &Repo,
        index: u64,
    ) -> Result<Vec<IssueComment>, GithubError> {
        (**self).list_issue_comments(repo, index)
    }
    fn post_comment(
        &self,
        repo: &Repo,
        index: u64,
        text: &str,
    ) -> Result<IssueComment, GithubError> {
        (**self).post_comment(repo, index, text)
    }
    fn delete_comment(&self, repo: &Repo, comment_id: u64) -> Result<(), GithubError> {
        (**self).delete_comment(repo, comment_id)
    }
    fn edit_comment(&self, repo: &Repo, comment_id: u64, text: &str) -> Result<(), GithubError> {
        (**self).edit_comment(repo, comment_id, text)
    }
}

/// Resolve a configured `host` to the REST API root every endpoint hangs off.
///
/// Cloud — empty, `github.com`, or `http(s)://github.com` — is `https://api.github.com`
/// (paths hang directly off it: `/repos/…`). Any other host is treated as GitHub
/// Enterprise Server, whose API lives under `/api/v3` of the host: the scheme is preserved
/// when present, else `https://` is prepended, and a trailing slash is trimmed.
pub(crate) fn github_api_base(host: &str) -> String {
    let host = host.trim();
    // Compare scheme-insensitively against the cloud hostname.
    let bare = host
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.is_empty() || bare == "github.com" {
        return "https://api.github.com".to_string();
    }
    let with_scheme = if host.starts_with("http://") || host.starts_with("https://") {
        host.trim_end_matches('/').to_string()
    } else {
        format!("https://{bare}")
    };
    format!("{with_scheme}/api/v3")
}

/// A [`GithubClient`] backed by the GitHub REST API.
pub(crate) struct Github {
    http: HttpClient<GithubError>,
}

impl Github {
    /// A client for `host` (empty/`github.com` → the cloud API, any other host a GHES
    /// base) authenticating with `token` as `Authorization: Bearer <token>`. The API root
    /// is resolved once here via [`github_api_base`].
    pub(crate) fn new(host: &str, token: &str) -> Self {
        Self {
            http: HttpClient::new(
                // The API root is resolved here, once; the spine neither trims nor
                // suffixes what it is handed.
                github_api_base(host),
                // Auth rides a header (never a URL query), so the token cannot leak
                // through a transport-error URL.
                ("Authorization".to_string(), format!("Bearer {token}")),
                DEFAULT_CONNECT_TIMEOUT,
                DEFAULT_READ_TIMEOUT,
            ),
        }
    }
}

impl GithubClient for Github {
    fn current_user(&self) -> Result<User, GithubError> {
        let stage = "current user";
        // GET /user → the authenticated user.
        let body = self.http.get(stage, "/user")?;
        parse_user(stage, &body)
    }

    fn list_issues(
        &self,
        repo: &Repo,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GithubError> {
        let stage = "list issues";
        // GET /repos/{o}/{r}/issues?state=&labels= → issues carrying the source label.
        // GitHub folds PRs into the issues list and the API offers no server-side filter
        // for that, so [`parse_issues`] drops the PR entries — PRs are not this kind's.
        let req = self
            .http
            .request(
                "GET",
                &format!("/repos/{}/{}/issues", repo.owner, repo.name),
            )
            .query("state", state)
            .query("labels", labels);
        let body = self.http.send(stage, req)?;
        parse_issues(stage, &body)
    }

    fn add_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<(), GithubError> {
        let stage = "add assignees";
        // POST /repos/{o}/{r}/issues/{number}/assignees { assignees } → add the named
        // users (additive; the dedicated endpoint, not the issue PATCH).
        let req = self.http.request(
            "POST",
            &format!(
                "/repos/{}/{}/issues/{index}/assignees",
                repo.owner, repo.name
            ),
        );
        self.http
            .send_json(stage, req, &json!({ "assignees": assignees }))
            .map(|_| ())
    }

    fn remove_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<(), GithubError> {
        let stage = "remove assignees";
        // DELETE /repos/{o}/{r}/issues/{number}/assignees { assignees } → remove the named
        // users (removes only the bot on `unassign`, never an all-clear).
        let req = self.http.request(
            "DELETE",
            &format!(
                "/repos/{}/{}/issues/{index}/assignees",
                repo.owner, repo.name
            ),
        );
        self.http
            .send_json(stage, req, &json!({ "assignees": assignees }))
            .map(|_| ())
    }

    fn set_state(&self, repo: &Repo, index: u64, state: &str) -> Result<(), GithubError> {
        let stage = "set state";
        // PATCH /repos/{o}/{r}/issues/{number} { state } → open→closed.
        let req = self.http.request(
            "PATCH",
            &format!("/repos/{}/{}/issues/{index}", repo.owner, repo.name),
        );
        self.http
            .send_json(stage, req, &json!({ "state": state }))
            .map(|_| ())
    }

    fn add_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GithubError> {
        let stage = "add label";
        // POST /repos/{o}/{r}/issues/{number}/labels { labels: [name] } → add the label by
        // name.
        let req = self.http.request(
            "POST",
            &format!("/repos/{}/{}/issues/{index}/labels", repo.owner, repo.name),
        );
        self.http
            .send_json(stage, req, &json!({ "labels": [name] }))
            .map(|_| ())
    }

    fn remove_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GithubError> {
        let stage = "remove label";
        // DELETE /repos/{o}/{r}/issues/{number}/labels/{name} → remove ONE label by name
        // (the bare `…/labels` path would clear ALL labels — never used).
        let req = self.http.request(
            "DELETE",
            &format!(
                "/repos/{}/{}/issues/{index}/labels/{}",
                repo.owner,
                repo.name,
                encode_segment(name)
            ),
        );
        self.http.send(stage, req).map(|_| ())
    }

    fn list_issue_comments(
        &self,
        repo: &Repo,
        index: u64,
    ) -> Result<Vec<IssueComment>, GithubError> {
        let stage = "list comments";
        // GET /repos/{o}/{r}/issues/{number}/comments → top-level discussion comments.
        let body = self.http.get(
            stage,
            &format!(
                "/repos/{}/{}/issues/{index}/comments",
                repo.owner, repo.name
            ),
        )?;
        parse_comments(stage, &body)
    }

    fn post_comment(
        &self,
        repo: &Repo,
        index: u64,
        text: &str,
    ) -> Result<IssueComment, GithubError> {
        let stage = "post comment";
        // POST /repos/{o}/{r}/issues/{number}/comments { body } → a top-level comment.
        // GitHub answers with the created comment; it is decoded rather than discarded, so
        // a claim marker can recognise and order its own word.
        let req = self.http.request(
            "POST",
            &format!(
                "/repos/{}/{}/issues/{index}/comments",
                repo.owner, repo.name
            ),
        );
        let body = self.http.send_json(stage, req, &json!({ "body": text }))?;
        parse_comment(stage, &body)
    }

    fn delete_comment(&self, repo: &Repo, comment_id: u64) -> Result<(), GithubError> {
        let stage = "delete comment";
        // DELETE /repos/{o}/{r}/issues/comments/{id} → drop ONE comment. The path is
        // repo-scoped: the issue number is not part of it.
        let req = self.http.request(
            "DELETE",
            &format!(
                "/repos/{}/{}/issues/comments/{comment_id}",
                repo.owner, repo.name
            ),
        );
        self.http.send(stage, req).map(|_| ())
    }

    fn edit_comment(&self, repo: &Repo, comment_id: u64, text: &str) -> Result<(), GithubError> {
        let stage = "edit comment";
        // PATCH /repos/{o}/{r}/issues/comments/{id} { body } → rewrite ONE comment, off the
        // same repo-scoped path the DELETE beside it uses. The reply is the updated
        // comment; a renewal needs nothing from it.
        let req = self.http.request(
            "PATCH",
            &format!(
                "/repos/{}/{}/issues/comments/{comment_id}",
                repo.owner, repo.name
            ),
        );
        self.http
            .send_json(stage, req, &json!({ "body": text }))
            .map(|_| ())
    }
}

// --- Pure parsing (no network): the GitHub JSON shapes. ----------------------

/// Parse the `GET /user` response into a [`User`].
pub(crate) fn parse_user(stage: &'static str, body: &str) -> Result<User, GithubError> {
    let value = GithubError::decode_json(stage, body)?;
    value_to_user(&value).ok_or_else(|| GithubError::decode(stage, "user missing login"))
}

/// Parse an issues array into [`Issue`]s, dropping the pull requests GitHub folds into
/// `GET /issues` (see [`is_pull_request`]).
pub(crate) fn parse_issues(stage: &'static str, body: &str) -> Result<Vec<Issue>, GithubError> {
    let value = GithubError::decode_json(stage, body)?;
    Ok(GithubError::as_array(stage, &value, "issues")?
        .iter()
        .filter(|v| !is_pull_request(v))
        .filter_map(value_to_issue)
        .collect())
}

/// Whether an issues-list entry is really a pull request. GitHub's REST API states every
/// PR is an issue, and marks the ones that are by hanging a `pull_request` member off the
/// entry; a plain issue omits the key. An explicit `null` there is read as the omission it
/// stands for, so a serializer that writes every field does not cost the repo its whole
/// issue intake.
fn is_pull_request(v: &Value) -> bool {
    v.get("pull_request").is_some_and(|pr| !pr.is_null())
}

/// Parse an issue-comments array into [`IssueComment`]s.
pub(crate) fn parse_comments(
    stage: &'static str,
    body: &str,
) -> Result<Vec<IssueComment>, GithubError> {
    let value = GithubError::decode_json(stage, body)?;
    Ok(GithubError::as_array(stage, &value, "comments")?
        .iter()
        .filter_map(value_to_comment)
        .collect())
}

/// Parse a single-comment response (the reply to a comment POST) into an
/// [`IssueComment`]. Strict: a reply that is not a comment object is a decode failure
/// rather than a synthesized value, so a claim can never be built on a fabricated id.
/// Shares `value_to_comment` with [`parse_comments`], so the POST reply and the list reply
/// cannot read a comment differently.
pub(crate) fn parse_comment(stage: &'static str, body: &str) -> Result<IssueComment, GithubError> {
    let value = GithubError::decode_json(stage, body)?;
    value_to_comment(&value).ok_or_else(|| GithubError::decode(stage, "comment missing id"))
}

fn value_to_user(v: &Value) -> Option<User> {
    let login = v.get("login")?.as_str()?.to_string();
    Some(User { login })
}

/// A GitHub label is either a string or an object with a `name`; either way only its name
/// is kept (removal is by name).
fn value_to_label_name(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    v.get("name").and_then(Value::as_str).map(str::to_string)
}

fn value_to_issue(v: &Value) -> Option<Issue> {
    let number = v.get("number")?.as_u64()?;
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(value_to_label_name).collect())
        .unwrap_or_default();
    Some(Issue {
        number,
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        body: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("open")
            .to_string(),
        labels,
    })
}

fn value_to_comment(v: &Value) -> Option<IssueComment> {
    let id = v.get("id")?.as_u64()?;
    let user = v.get("user").and_then(value_to_user).unwrap_or(User {
        login: String::new(),
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
    Some(IssueComment {
        id,
        body: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        user,
        created_at,
        updated_at,
    })
}

#[cfg(test)]
pub(crate) use mock::{Action, MockClient};

#[cfg(test)]
mod mock {
    //! An in-memory [`GithubClient`] (test-only). No network: every kind and settings
    //! test drives this mock; the real [`Github`] HTTP code is exercised only against the
    //! loopback `Stub` below.

    use super::*;
    use crate::common::lock;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A recorded mutation against the mock forge, for assertions.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Action {
        /// Assignees were added to an issue (additive).
        AddAssignees {
            /// The issue number.
            index: u64,
            /// The logins added.
            assignees: Vec<String>,
        },
        /// Assignees were removed from an issue.
        RemoveAssignees {
            /// The issue number.
            index: u64,
            /// The logins removed.
            assignees: Vec<String>,
        },
        /// A label was added to an issue.
        Label {
            /// The issue number.
            index: u64,
            /// The label name added.
            name: String,
        },
        /// A label was removed from an issue (by name — never the all-clearing path).
        Unlabel {
            /// The issue number.
            index: u64,
            /// The label name removed.
            name: String,
        },
        /// An issue's state was set.
        State {
            /// The issue number.
            index: u64,
            /// The new state (`open`/`closed`).
            state: String,
        },
        /// A literal comment was posted on an issue.
        Comment {
            /// The issue number.
            index: u64,
            /// The comment body posted.
            body: String,
        },
        /// A comment was deleted (by id).
        DeleteComment {
            /// The comment id removed.
            id: u64,
        },
        /// A comment's body was rewritten (by id) — a claim marker's renewal.
        EditComment {
            /// The comment id edited.
            id: u64,
            /// The body it now carries.
            body: String,
        },
    }

    /// One seeded issue, with the assignee set the kind never reads but the status
    /// writes change.
    struct Seeded {
        issue: Issue,
        assignees: Vec<String>,
    }

    /// An in-memory forge: tests seed issues and comments (each with an explicit
    /// timestamp), drive the kind against it, and inspect the recorded [`Action`]s. A
    /// stage can be made to fail, the second a posted comment is stamped with can be
    /// frozen ([`MockClient::set_clock`], which is what makes a *same-second* claim race
    /// expressible), and a rival claim marker can be injected into the next comment read
    /// to exercise the lost-race path.
    #[derive(Default)]
    pub(crate) struct MockClient {
        me: Mutex<String>,
        issues: Mutex<Vec<Seeded>>,
        comments: Mutex<HashMap<u64, Vec<IssueComment>>>,
        actions: Mutex<Vec<Action>>,
        /// How many times `current_user` was asked, so a test can pin the identity to
        /// one resolve per armed service.
        user_reads: Mutex<u32>,
        fail_stage: Mutex<Option<&'static str>>,
        /// How many comments have been posted through `post_comment`, so each returned
        /// comment gets a distinct id and a strictly newer timestamp than any seeded one.
        posted: Mutex<u64>,
        /// When set, every posted comment is stamped this many seconds after the epoch
        /// instead of the minted per-post one — so two posts land in the *same second*
        /// with distinct, increasing ids and the claim's tie-break is the only thing that
        /// can decide.
        post_clock: Mutex<Option<u64>>,
        /// When set, the next `list_issue_comments` first drops this rival claim marker
        /// into the thread: a marker that landed between our post and our re-read, with
        /// the test choosing its side of the `(created_at, id)` order.
        rival_on_read: Mutex<Option<IssueComment>>,
    }

    /// The id/timestamp base a `post_comment` mints from: far above the small ids and
    /// epoch-relative seconds tests seed, so a posted comment is always the newest.
    const POSTED_ID_BASE: u64 = 1_000_000;

    impl MockClient {
        /// A fresh forge whose authenticated user is `me`.
        pub(crate) fn new(me: &str) -> Self {
            let c = Self::default();
            *lock(&c.me) = me.to_string();
            c
        }

        /// Seed an issue (open, with the given labels and no assignees).
        pub(crate) fn add_issue(&self, number: u64, title: &str, body: &str, labels: &[&str]) {
            self.add_issue_assigned(number, title, body, labels, &[]);
        }

        /// Seed an open issue with the given labels **and** assignees. Unlike driving
        /// `add_assignees`, this records no `Action`, so it seeds a starting state without
        /// polluting the recorded-mutation assertions.
        pub(crate) fn add_issue_assigned(
            &self,
            number: u64,
            title: &str,
            body: &str,
            labels: &[&str],
            assignees: &[&str],
        ) {
            lock(&self.issues).push(Seeded {
                issue: Issue {
                    number,
                    title: title.to_string(),
                    body: body.to_string(),
                    state: "open".to_string(),
                    labels: labels.iter().map(|s| s.to_string()).collect(),
                },
                assignees: assignees.iter().map(|l| (*l).to_string()).collect(),
            });
        }

        /// Close a seeded issue, as a human does.
        pub(crate) fn close(&self, number: u64) {
            self.mutate(number, |s| s.issue.state = "closed".to_string());
        }

        /// Seed a comment on an issue by `author`, updated `secs` after epoch.
        pub(crate) fn add_comment(&self, index: u64, id: u64, author: &str, secs: u64) {
            self.add_comment_body(index, id, author, &format!("comment {id}"), secs);
        }

        /// Seed a comment with an explicit body — what a human wrote, and what seeds a
        /// stale or crashed claim marker.
        pub(crate) fn add_comment_body(
            &self,
            index: u64,
            id: u64,
            author: &str,
            body: &str,
            secs: u64,
        ) {
            // A seeded comment is unedited: written when it was last touched.
            self.add_comment_edited(index, id, author, body, secs, secs);
        }

        /// Seed a comment written at `created` and last edited at `updated` — the shape
        /// that tells the claim's order key (creation) apart from its liveness (the edit).
        pub(crate) fn add_comment_edited(
            &self,
            index: u64,
            id: u64,
            author: &str,
            body: &str,
            created: u64,
            updated: u64,
        ) {
            lock(&self.comments)
                .entry(index)
                .or_default()
                .push(IssueComment {
                    id,
                    body: body.to_string(),
                    user: User {
                        login: author.to_string(),
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

        /// Arm a concurrent rival: the next `list_issue_comments` finds a rival
        /// `[afkd-claim]` marker in the thread — one that landed between our post and our
        /// re-read — authored and owned by `owner`, with comment id `id` and created
        /// `secs` after the epoch.
        pub(crate) fn rival_claims_next(&self, owner: &str, id: u64, secs: u64) {
            *lock(&self.rival_on_read) = Some(IssueComment {
                id,
                body: crate::claim::claim_text(owner),
                user: User {
                    login: owner.to_string(),
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

        /// The current assignees on an issue (for assertions).
        pub(crate) fn assignees_of(&self, index: u64) -> Vec<String> {
            lock(&self.issues)
                .iter()
                .find(|s| s.issue.number == index)
                .map(|s| s.assignees.clone())
                .unwrap_or_default()
        }

        /// Whether issue `index` carries label `name` (for assertions).
        pub(crate) fn has_label(&self, index: u64, name: &str) -> bool {
            lock(&self.issues)
                .iter()
                .find(|s| s.issue.number == index)
                .is_some_and(|s| s.issue.has_label(name))
        }

        /// Apply a mutation to issue `index`, if it was seeded.
        fn mutate(&self, index: u64, f: impl FnOnce(&mut Seeded)) {
            if let Some(s) = lock(&self.issues)
                .iter_mut()
                .find(|s| s.issue.number == index)
            {
                f(s);
            }
        }

        fn guard(&self, stage: &'static str) -> Result<(), GithubError> {
            if *lock(&self.fail_stage) == Some(stage) {
                Err(GithubError::Transport {
                    stage,
                    reason: "mock failure".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    impl GithubClient for MockClient {
        fn current_user(&self) -> Result<User, GithubError> {
            *lock(&self.user_reads) += 1;
            self.guard("current user")?;
            Ok(User {
                login: lock(&self.me).clone(),
            })
        }

        fn list_issues(
            &self,
            _repo: &Repo,
            state: &str,
            labels: &str,
        ) -> Result<Vec<Issue>, GithubError> {
            self.guard("list issues")?;
            Ok(lock(&self.issues)
                .iter()
                .map(|s| &s.issue)
                .filter(|i| state == "all" || i.state == state)
                .filter(|i| labels.is_empty() || i.has_label(labels))
                .cloned()
                .collect())
        }

        fn add_assignees(
            &self,
            _repo: &Repo,
            index: u64,
            assignees: &[String],
        ) -> Result<(), GithubError> {
            self.guard("add assignees")?;
            self.mutate(index, |s| {
                // Additive: add each login not already present.
                for login in assignees {
                    if !s.assignees.contains(login) {
                        s.assignees.push(login.clone());
                    }
                }
            });
            lock(&self.actions).push(Action::AddAssignees {
                index,
                assignees: assignees.to_vec(),
            });
            Ok(())
        }

        fn remove_assignees(
            &self,
            _repo: &Repo,
            index: u64,
            assignees: &[String],
        ) -> Result<(), GithubError> {
            self.guard("remove assignees")?;
            self.mutate(index, |s| s.assignees.retain(|l| !assignees.contains(l)));
            lock(&self.actions).push(Action::RemoveAssignees {
                index,
                assignees: assignees.to_vec(),
            });
            Ok(())
        }

        fn set_state(&self, _repo: &Repo, index: u64, state: &str) -> Result<(), GithubError> {
            self.guard("set state")?;
            self.mutate(index, |s| s.issue.state = state.to_string());
            lock(&self.actions).push(Action::State {
                index,
                state: state.to_string(),
            });
            Ok(())
        }

        fn add_label(&self, _repo: &Repo, index: u64, name: &str) -> Result<(), GithubError> {
            self.guard("add label")?;
            self.mutate(index, |s| {
                if !s.issue.has_label(name) {
                    s.issue.labels.push(name.to_string());
                }
            });
            lock(&self.actions).push(Action::Label {
                index,
                name: name.to_string(),
            });
            Ok(())
        }

        fn remove_label(&self, _repo: &Repo, index: u64, name: &str) -> Result<(), GithubError> {
            self.guard("remove label")?;
            self.mutate(index, |s| s.issue.labels.retain(|l| l != name));
            lock(&self.actions).push(Action::Unlabel {
                index,
                name: name.to_string(),
            });
            Ok(())
        }

        fn list_issue_comments(
            &self,
            _repo: &Repo,
            index: u64,
        ) -> Result<Vec<IssueComment>, GithubError> {
            self.guard("list comments")?;
            // An armed rival's marker lands in the thread just before this read — the
            // concurrent claim that was posted while we were settling. It joins the
            // thread for good, as a real one would.
            if let Some(rival) = lock(&self.rival_on_read).take() {
                lock(&self.comments).entry(index).or_default().push(rival);
            }
            Ok(lock(&self.comments)
                .get(&index)
                .cloned()
                .unwrap_or_default())
        }

        fn post_comment(
            &self,
            _repo: &Repo,
            index: u64,
            text: &str,
        ) -> Result<IssueComment, GithubError> {
            self.guard("post comment")?;
            // A posted comment joins the thread, authored by the authenticated user and
            // newest — as the forge does. A later read must see it, or the claim could
            // never find the marker it just wrote.
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
            let posted = IssueComment {
                id: POSTED_ID_BASE + n,
                body: text.to_string(),
                user: User { login: me },
                created_at: at,
                updated_at: at,
            };
            lock(&self.comments)
                .entry(index)
                .or_default()
                .push(posted.clone());
            lock(&self.actions).push(Action::Comment {
                index,
                body: text.to_string(),
            });
            Ok(posted)
        }

        fn delete_comment(&self, _repo: &Repo, comment_id: u64) -> Result<(), GithubError> {
            self.guard("delete comment")?;
            for thread in lock(&self.comments).values_mut() {
                thread.retain(|c| c.id != comment_id);
            }
            lock(&self.actions).push(Action::DeleteComment { id: comment_id });
            Ok(())
        }

        fn edit_comment(
            &self,
            _repo: &Repo,
            comment_id: u64,
            text: &str,
        ) -> Result<(), GithubError> {
            self.guard("edit comment")?;
            // The forge rewrites the body and moves `updated_at` — the half a rival's
            // claim decision reads as liveness — leaving `created_at` (the order half)
            // exactly where it was.
            let edited_at =
                UNIX_EPOCH + Duration::from_secs(lock(&self.post_clock).unwrap_or(POSTED_ID_BASE));
            for thread in lock(&self.comments).values_mut() {
                for comment in thread.iter_mut().filter(|c| c.id == comment_id) {
                    comment.body = text.to_string();
                    comment.updated_at = edited_at;
                }
            }
            lock(&self.actions).push(Action::EditComment {
                id: comment_id,
                body: text.to_string(),
            });
            Ok(())
        }
    }

    fn repo() -> Repo {
        Repo {
            owner: "o".into(),
            name: "r".into(),
        }
    }

    #[test]
    fn add_assignees_is_additive_and_reread_reflects_it() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        c.add_assignees(&repo(), 7, &["me".into()]).unwrap();
        c.add_assignees(&repo(), 7, &["human".into()]).unwrap();
        c.add_assignees(&repo(), 7, &["me".into()]).unwrap();
        assert_eq!(
            c.assignees_of(7),
            vec!["me".to_string(), "human".to_string()]
        );
    }

    /// The lost-race knob: an armed rival marker shows up in the **next** comment read,
    /// carrying the id and second the test chose (so it can be placed either side of ours
    /// in the claim order) — and only once, however often the thread is read afterwards.
    #[test]
    fn rival_injection_lands_in_the_next_comment_read_once() {
        let c = MockClient::new("me");
        c.rival_claims_next("rival", 4242, 500);

        let first = c.list_issue_comments(&repo(), 7).unwrap();
        assert_eq!(first.len(), 1, "the rival joined the thread: {first:?}");
        assert_eq!(first[0].id, 4242);
        assert_eq!(first[0].user.login, "rival");
        assert_eq!(first[0].body, "[afkd-claim] owner=rival");
        assert_eq!(first[0].created_at, UNIX_EPOCH + Duration::from_secs(500));

        let again = c.list_issue_comments(&repo(), 7).unwrap();
        assert_eq!(
            again.len(),
            1,
            "injected once, not once per read: {again:?}"
        );
    }

    /// The frozen post clock: successive posts tie in one second while their ids keep
    /// increasing — the fixture a same-second claim race is staged on. Without it each
    /// post is stamped a second later than the last.
    #[test]
    fn a_frozen_post_clock_ties_the_second_but_not_the_ids() {
        let c = MockClient::new("me");
        c.set_clock(1_700_000_000);
        let a = c.post_comment(&repo(), 7, "[afkd-claim] owner=a").unwrap();
        let b = c.post_comment(&repo(), 7, "[afkd-claim] owner=b").unwrap();
        assert_eq!(a.created_at, b.created_at, "same second");
        assert!(a.id < b.id, "distinct, increasing ids: {} {}", a.id, b.id);

        let unfrozen = MockClient::new("me");
        let a = unfrozen.post_comment(&repo(), 7, "one").unwrap();
        let b = unfrozen.post_comment(&repo(), 7, "two").unwrap();
        assert!(a.created_at < b.created_at, "unset keeps the minted stamp");
    }

    #[test]
    fn label_add_then_remove_by_name() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        c.add_label(&repo(), 7, "afkd/claimed").unwrap();
        assert!(c.has_label(7, "afkd/claimed"));
        c.remove_label(&repo(), 7, "afkd/claimed").unwrap();
        assert!(!c.has_label(7, "afkd/claimed"));
        assert!(c
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Unlabel { name, .. } if name == "afkd/claimed")));
    }

    #[test]
    fn failure_injection_is_scoped_to_a_stage() {
        let c = MockClient::new("me");
        c.fail("list issues");
        assert!(c.list_issues(&repo(), "open", "").is_err());
        assert!(c.current_user().is_ok(), "another stage still answers");
        c.clear_failure();
        assert!(c.list_issues(&repo(), "open", "").is_ok());
    }

    /// The listing filters as the forge does: by state, and by the source label.
    #[test]
    fn list_issues_filters_on_state_and_label() {
        let c = MockClient::new("me");
        c.add_issue(1, "T", "B", &["afkd/ready"]);
        c.add_issue(2, "T", "B", &[]);
        c.add_issue(3, "T", "B", &["afkd/ready"]);
        c.close(3);
        let numbers = |state, label| -> Vec<u64> {
            c.list_issues(&repo(), state, label)
                .unwrap()
                .iter()
                .map(|i| i.number)
                .collect()
        };
        assert_eq!(numbers("open", "afkd/ready"), [1]);
        assert_eq!(numbers("open", ""), [1, 2]);
        assert_eq!(numbers("all", "afkd/ready"), [1, 3]);
    }

    /// The claim-marker round trip through the mock: the posted comment comes back
    /// identified (a minted id above every seeded one, and a creation time), joins the
    /// thread as the forge's own would — the claim's re-read has to find it — and deleting
    /// it by id takes it out again.
    #[test]
    fn post_then_delete_comment_round_trips_by_id() {
        let c = MockClient::new("björn-öst[bot]");
        c.add_comment(7, 42, "josefandersson", 100);
        let posted = c
            .post_comment(
                &repo(),
                7,
                "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
            )
            .unwrap();

        assert!(
            posted.id > 42,
            "a posted id must outrank every seeded one, got {}",
            posted.id
        );
        assert_eq!(posted.user.login, "björn-öst[bot]");
        assert_eq!(posted.created_at, posted.updated_at);
        assert!(posted.created_at > UNIX_EPOCH + Duration::from_secs(100));
        assert_eq!(
            c.list_issue_comments(&repo(), 7)
                .unwrap()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![42, posted.id],
            "the posted comment joins the thread, as the forge's does"
        );

        c.delete_comment(&repo(), posted.id).unwrap();
        assert_eq!(
            c.list_issue_comments(&repo(), 7)
                .unwrap()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![42],
            "the deleted comment left the thread"
        );
        assert!(c
            .actions()
            .contains(&Action::DeleteComment { id: posted.id }));
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn repo_parse_takes_owner_slash_name_only() {
        assert_eq!(
            Repo::parse("acme/widgets"),
            Some(Repo {
                owner: "acme".into(),
                name: "widgets".into(),
            })
        );
        assert_eq!(
            Repo::parse("björn-öst/verktyg").map(|r| r.full_name()),
            Some("björn-öst/verktyg".to_string())
        );
        for bad in ["", "not-a-repo", "/widgets", "acme/", "acme/sub/widgets"] {
            assert_eq!(Repo::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn github_api_base_resolves_cloud_and_ghes() {
        // Cloud spellings all collapse to api.github.com.
        assert_eq!(github_api_base(""), "https://api.github.com");
        assert_eq!(github_api_base("  "), "https://api.github.com");
        assert_eq!(github_api_base("github.com"), "https://api.github.com");
        assert_eq!(
            github_api_base("https://github.com"),
            "https://api.github.com"
        );
        assert_eq!(
            github_api_base("http://github.com/"),
            "https://api.github.com"
        );
        // A GHES host lives under /api/v3, scheme preserved (else https prepended).
        assert_eq!(
            github_api_base("ghe.example.com"),
            "https://ghe.example.com/api/v3"
        );
        assert_eq!(
            github_api_base("https://ghe.example.com/"),
            "https://ghe.example.com/api/v3"
        );
        assert_eq!(
            github_api_base("http://ghe.example.com"),
            "http://ghe.example.com/api/v3"
        );
        assert_eq!(
            github_api_base("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/api/v3"
        );
    }

    #[test]
    fn parse_issues_reads_number_state_title_and_labels() {
        let body = r#"[
            {"number":4,"title":"Fix","body":"do it","state":"open",
             "labels":[{"id":9,"name":"afkd/ready"}],
             "assignees":[{"login":"bot"}]}
        ]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(
            issues,
            [Issue {
                number: 4,
                title: "Fix".into(),
                body: "do it".into(),
                state: "open".into(),
                labels: vec!["afkd/ready".into()],
            }]
        );
    }

    /// An absent title, body or state degrades to the built-in's defaults rather than
    /// dropping the issue; one with no number is no issue at all.
    #[test]
    fn parse_issues_defaults_the_absent_fields() {
        let issues = parse_issues(
            "list issues",
            r#"[{"number":4,"body":null},{"title":"no number"}]"#,
        )
        .unwrap();
        assert_eq!(
            issues,
            [Issue {
                number: 4,
                title: String::new(),
                body: String::new(),
                state: "open".into(),
                labels: Vec::new(),
            }]
        );
    }

    #[test]
    fn parse_issues_reads_a_bare_string_label() {
        // GitHub's REST issues carry labels as `{ "name": … }` objects, but
        // `value_to_label_name` also accepts a bare string (the shape some search/expand
        // responses use); a label that is neither is dropped rather than faulting the
        // parse.
        let body = r#"[
            {"number":4,"title":"Fix","body":"do it","state":"open",
             "labels":["afkd/ready", 7],
             "assignees":[]}
        ]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].labels, ["afkd/ready"]);
    }

    #[test]
    fn parse_issues_drops_the_pull_requests_github_folds_into_the_list() {
        // A realistic `GET /issues` page: a rich issue (object labels, an assignee, a
        // multi-line unicode body), a PR carrying the *same* source label — GitHub marks
        // it with a `pull_request` member — and a second issue after it, so the filter is
        // proven not to cut off the rest of the page. That last one spells the member out
        // as `null`, the shape a write-every-field serializer emits for an issue.
        let body = r#"[
            {"number":4,"title":"Räkna om båten","state":"open",
             "body":"Steg ett\n\n- [ ] mät\n- [ ] räkna 📐\n",
             "labels":[{"id":9,"name":"afkd/ready"},{"id":10,"name":"bug"}],
             "assignees":[{"login":"bot"}]},
            {"number":5,"title":"Fix the boat","state":"open","body":"",
             "labels":[{"id":9,"name":"afkd/ready"}],"assignees":[],
             "pull_request":{"url":"https://api.github.com/repos/acme/widgets/pulls/5",
                             "html_url":"https://github.com/acme/widgets/pull/5"}},
            {"number":6,"title":"Also mine","state":"open","body":"plain",
             "labels":[{"id":9,"name":"afkd/ready"}],"assignees":[],
             "pull_request":null}
        ]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(
            issues.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![4, 6],
            "the labelled PR is dropped; both real issues survive"
        );
        assert_eq!(issues[0].body, "Steg ett\n\n- [ ] mät\n- [ ] räkna 📐\n");
        assert_eq!(issues[0].labels, ["afkd/ready", "bug"]);
    }

    /// The shapes a real thread mixes: a fresh comment, an **edited** one whose
    /// `created_at` is two hours before its `updated_at`, one the forge sent without a
    /// `created_at` at all, and a deleted account's. The two times are distinct fields,
    /// read through the same RFC-3339 reader, and a missing one degrades to the epoch
    /// rather than borrowing its sibling's value.
    #[test]
    fn parse_comments_read_authors_and_times() {
        let comments = parse_comments(
            "list comments",
            r#"[
                {"id":1,"body":"hi","user":{"login":"human"},
                 "created_at":"2021-01-01T00:00:00Z","updated_at":"2021-01-01T00:00:00Z"},
                {"id":2,"body":"The token expires mid-retry — see §4 🙏","user":{"login":"björn-öst"},
                 "created_at":"2021-01-01T00:00:00Z","updated_at":"2021-01-01T02:00:00Z"},
                {"id":3,"body":"no creation stamp","user":{"login":"human"},
                 "updated_at":"2021-01-01T00:00:00Z"},
                {"id":4,"body":"a ghost","user":null,
                 "created_at":"2021-01-01T00:00:00Z","updated_at":"2021-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(comments[0].user.login, "human");
        assert_eq!(comments[0].created_at, t0);
        assert_eq!(comments[0].updated_at, t0);

        // Edited: written at 00:00Z, touched two hours later.
        assert_eq!(comments[1].user.login, "björn-öst");
        assert_eq!(comments[1].body, "The token expires mid-retry — see §4 🙏");
        assert_eq!(comments[1].created_at, t0);
        assert_eq!(comments[1].updated_at, t0 + Duration::from_secs(7_200));

        // Absent `created_at` degrades to the epoch — distinguishable from a stamp equal
        // to `updated_at`, which is what a fallback to the sibling would give.
        assert_eq!(comments[2].created_at, UNIX_EPOCH);
        assert_eq!(comments[2].updated_at, t0);

        // A deleted account's comment keeps its place with an empty login.
        assert_eq!(comments[3].user.login, "");
    }

    /// The POST reply decodes through the very same `value_to_comment` the list reply
    /// does, and is **strict**: a reply carrying no `id` is a staged decode error, never a
    /// synthesized comment a claim could be built on.
    #[test]
    fn parse_comment_reads_the_posted_comment_or_fails_loudly() {
        let posted = parse_comment(
            "post comment",
            r#"{"id":90210,"body":"[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                "user":{"login":"björn-öst[bot]"},
                "created_at":"2026-07-20T09:00:00Z","updated_at":"2026-07-20T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(posted.id, 90210);
        assert_eq!(posted.user.login, "björn-öst[bot]");
        assert_eq!(
            posted.body,
            "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00"
        );
        assert_eq!(posted.created_at, posted.updated_at);

        let err = parse_comment("post comment", r#"{"message":"rate limited"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "github post comment: undecodable response (comment missing id)"
        );
    }

    #[test]
    fn decode_failures_are_tagged_with_their_stage() {
        let err = parse_issues("list issues", "not json").unwrap_err();
        assert_eq!(err.stage(), "list issues");
        let err = parse_issues("list issues", r#"{"not":"array"}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "github list issues: undecodable response (expected an array of issues)"
        );
        let err = parse_user("current user", r#"{"id":8}"#).unwrap_err();
        assert_eq!(
            err.to_string(),
            "github current user: undecodable response (user missing login)"
        );
    }

    /// Each variant reports its stage and renders the built-in's own sentence.
    #[test]
    fn github_error_stage_and_sentence_for_every_variant() {
        let status = GithubError::Status {
            stage: "list issues",
            status: 500,
        };
        assert_eq!(status.stage(), "list issues");
        assert_eq!(
            status.to_string(),
            "github list issues: forge returned status 500"
        );
        let transport = GithubError::Transport {
            stage: "current user",
            reason: "io: Connection refused".into(),
        };
        assert_eq!(transport.stage(), "current user");
        assert_eq!(
            transport.to_string(),
            "github current user: no response (io: Connection refused)"
        );
        let decode = GithubError::Decode {
            stage: "set state",
            reason: "y".into(),
        };
        assert_eq!(decode.stage(), "set state");
        assert_eq!(
            decode.to_string(),
            "github set state: undecodable response (y)"
        );
    }
}

#[cfg(test)]
mod http_tests {
    //! The real [`Github`] HTTP client's request construction, exercised against a
    //! loopback `Stub` (a local socket — **no external network**). Each test pins one
    //! endpoint's method + path so every cited path has a test in the one place the paths
    //! live. The stub is reached through a non-`github.com` host, so every path carries
    //! the GHES `/api/v3` prefix [`github_api_base`] resolves it to.

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
    /// captured request back, driving the real [`Github`] code over a socket.
    struct Stub {
        host: String,
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
                host: format!("http://127.0.0.1:{port}"),
                handle: Some(handle),
                rx,
            }
        }

        fn client(&self, token: &str) -> Github {
            Github::new(&self.host, token)
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

    /// The `201 Created` + created resource a comment POST really answers with.
    fn created_json(body: &str) -> String {
        format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// The `204 No Content` a comment DELETE really answers with.
    fn no_content() -> &'static str {
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
    }

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    #[test]
    fn current_user_gets_user_with_bearer_auth() {
        let stub = Stub::serve(ok_json(r#"{"login":"björn-öst[bot]","id":8}"#));
        let client = stub.client("SECRET");
        let user = client.current_user().unwrap();
        assert_eq!(user.login, "björn-öst[bot]");
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v3/user");
        assert_eq!(
            req.header("authorization").as_deref(),
            Some("Bearer SECRET")
        );
        assert_eq!(req.header("private-token"), None, "no second credential");
    }

    #[test]
    fn list_issues_builds_the_repos_path_with_state_and_labels() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client.list_issues(&repo(), "open", "afkd/ready").unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues");
        assert!(req.has_query("state", "open"));
        assert!(req.has_query("labels", "afkd/ready"));
    }

    #[test]
    fn list_issues_over_the_wire_hands_back_issues_only() {
        // The whole seam, over a socket: a `GET /issues` page that mixes a real issue and
        // a source-labelled PR yields only the issue, so no PR ever reaches the kind's
        // eligibility check.
        let stub = Stub::serve(ok_json(
            r#"[{"number":41,"title":"Räkna om båten","state":"open",
                 "body":"Steg ett\n\n- [ ] mät 📐\n",
                 "labels":[{"name":"afkd/ready"}],"assignees":[{"login":"bot"}]},
                {"number":42,"title":"Fix the boat","state":"open","body":"",
                 "labels":[{"name":"afkd/ready"}],"assignees":[],
                 "pull_request":{"html_url":"https://github.com/acme/widgets/pull/42"}}]"#,
        ));
        let client = stub.client("t");
        let issues = client.list_issues(&repo(), "open", "afkd/ready").unwrap();
        assert_eq!(
            issues.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![41]
        );
        assert_eq!(stub.captured().path(), "/api/v3/repos/acme/widgets/issues");
    }

    #[test]
    fn add_assignees_posts_the_dedicated_assignees_path() {
        let stub = Stub::serve(created_json(r#"{"number":5}"#));
        let client = stub.client("t");
        client
            .add_assignees(&repo(), 5, &["björn-öst[bot]".to_string()])
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues/5/assignees");
        assert_eq!(req.json(), json!({"assignees": ["björn-öst[bot]"]}));
        assert_eq!(
            req.header("content-type").as_deref(),
            Some("application/json")
        );
    }

    /// The removal is a `DELETE` **with a body** — the one verb here that carries a
    /// payload without a body-taking builder, which the spine forces.
    #[test]
    fn remove_assignees_deletes_the_dedicated_assignees_path_with_a_body() {
        let stub = Stub::serve(ok_json(r#"{"number":5}"#));
        let client = stub.client("t");
        client
            .remove_assignees(&repo(), 5, &["björn-öst[bot]".to_string()])
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "DELETE");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues/5/assignees");
        assert_eq!(req.json(), json!({"assignees": ["björn-öst[bot]"]}));
    }

    #[test]
    fn set_state_patches_the_state_body() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client.set_state(&repo(), 5, "closed").unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PATCH");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues/5");
        assert_eq!(req.json(), json!({"state": "closed"}));
    }

    #[test]
    fn add_label_posts_the_labels_body() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client.add_label(&repo(), 5, "afkd/claimed").unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues/5/labels");
        assert_eq!(req.json(), json!({"labels": ["afkd/claimed"]}));
    }

    #[test]
    fn remove_label_deletes_the_labels_slash_name_path() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        // A label name with a `/` (and a space) URL-encodes into one path segment.
        client
            .remove_label(&repo(), 5, "afkd/needs review")
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "DELETE");
        // The single-label, by-name path — NOT the bare all-clearing `…/labels`.
        assert_eq!(
            req.path(),
            "/api/v3/repos/acme/widgets/issues/5/labels/afkd%2Fneeds%20review"
        );
        assert_eq!(req.body, "", "a label removal carries no body");
    }

    #[test]
    fn list_issue_comments_reads_the_comments_path() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client.list_issue_comments(&repo(), 5).unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues/5/comments");
    }

    /// The wire contract *and* the read-back: the POST carries the body on the issue's
    /// comments path, and GitHub's `201` + created-comment reply is decoded into the id and
    /// creation time a claim marker needs. The stub answers a real GitHub comment object —
    /// a multi-line, non-ASCII claim body and the `Z` stamps GitHub emits.
    #[test]
    fn post_comment_posts_the_body_and_returns_the_created_comment() {
        let stub = Stub::serve(created_json(
            r#"{"id":90210,"body":"[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                "user":{"login":"björn-öst[bot]"},
                "created_at":"2026-07-20T09:00:00Z","updated_at":"2026-07-20T09:00:00Z"}"#,
        ));
        let client = stub.client("t");
        let posted = client
            .post_comment(
                &repo(),
                5,
                "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
            )
            .unwrap();
        assert_eq!(posted.id, 90210);
        assert_eq!(posted.user.login, "björn-öst[bot]");
        assert_eq!(
            posted.created_at,
            UNIX_EPOCH + Duration::from_secs(1_784_538_000)
        );
        assert_eq!(posted.created_at, posted.updated_at);

        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/v3/repos/acme/widgets/issues/5/comments");
        assert_eq!(
            req.json(),
            json!({"body": "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00"})
        );
    }

    /// A reply that is not a comment object (no `id`) is a staged decode error, not a
    /// silently-successful post: a claim built on a fabricated id is worse than a loud
    /// failure.
    #[test]
    fn post_comment_rejects_a_reply_that_is_not_a_comment() {
        let stub = Stub::serve(ok_json(r#"{"message":"rate limited"}"#));
        let client = stub.client("t");
        let err = client.post_comment(&repo(), 5, "hi").unwrap_err();
        assert!(
            matches!(
                err,
                GithubError::Decode {
                    stage: "post comment",
                    ..
                }
            ),
            "an id-less POST reply must map to a staged Decode error, got {err:?}"
        );
        let _ = stub.captured();
    }

    /// The delete path is **repo**-scoped: `…/issues/comments/{id}` carries no issue
    /// number, unlike the POST that created it.
    #[test]
    fn delete_comment_deletes_the_repo_scoped_comment_path() {
        let stub = Stub::serve(no_content());
        let client = stub.client("t");
        client.delete_comment(&repo(), 90210).unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "DELETE");
        assert_eq!(
            req.path(),
            "/api/v3/repos/acme/widgets/issues/comments/90210"
        );
    }

    /// The renewal's wire contract: a PATCH on the **same** repo-scoped comment path the
    /// DELETE uses, carrying the renewed body. Editing by id is what keeps a renewal from
    /// minting a second marker.
    #[test]
    fn edit_comment_patches_the_repo_scoped_comment_path() {
        let stub = Stub::serve(ok_json(r#"{"id":90210,"body":"x"}"#));
        let client = stub.client("t");
        client
            .edit_comment(
                &repo(),
                90210,
                "[afkd-claim] owner=björn-öst[bot] renewal=7",
            )
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PATCH");
        assert_eq!(
            req.path(),
            "/api/v3/repos/acme/widgets/issues/comments/90210"
        );
        assert_eq!(
            req.json(),
            json!({"body": "[afkd-claim] owner=björn-öst[bot] renewal=7"})
        );
    }

    #[test]
    fn non_success_status_maps_to_status_error_tagged_with_stage() {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let stub = Stub::serve(resp);
        let client = stub.client("t");
        let err = client.set_state(&repo(), 9, "closed").unwrap_err();
        assert!(matches!(
            err,
            GithubError::Status {
                stage: "set state",
                status: 404
            }
        ));
        let _ = stub.captured();
    }

    /// A reply that promises more body bytes (`Content-Length`) than it sends, then closes
    /// the socket: reading it to a string hits a premature EOF, which is a decode failure
    /// tagged with the request's stage — not a `Status`/`Transport`.
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
                GithubError::Decode {
                    stage: "current user",
                    ..
                }
            ),
            "a cut-short body must map to a staged Decode error, got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn list_issue_comments_propagates_a_forge_error() {
        let stub = Stub::serve("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        let client = stub.client("t");
        let err = client.list_issue_comments(&repo(), 5).unwrap_err();
        assert!(
            matches!(
                err,
                GithubError::Status {
                    stage: "list comments",
                    status: 500
                }
            ),
            "a 500 on the comments GET must propagate out of list_issue_comments, got {err:?}"
        );
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
        let client = Github::new(&format!("http://127.0.0.1:{port}"), token);
        let err = client.current_user().unwrap_err();
        let GithubError::Transport { reason, .. } = &err else {
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
