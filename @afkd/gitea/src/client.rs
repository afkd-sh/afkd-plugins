//! The mockable Gitea client seam the issue kind drives, the plain value types it
//! exchanges, the typed [`GiteaError`], the **real [`Gitea`] HTTP client** over the
//! `/api/v1` surface, and a `#[cfg(test)]` in-memory `MockClient` for offline tests.
//!
//! Ported from afkd's `crates/gitea/src/client.rs`, issue side. All Gitea-specific
//! knowledge lives here — every `/api/v1` endpoint path, every request payload, the
//! `Authorization: token <PAT>` credential, and the JSON shapes — so the version-drift
//! blast radius is **one file**. Each endpoint carries a citing comment so a drift fix is
//! a one-place edit. The plumbing underneath is [`crate::http`] and [`crate::rfc3339`].
//!
//! The seam ([`GiteaClient`]) carries only what the kind needs, **by meaning** (resolve
//! the current user, list issues, claim by marker and label, read comments). The response
//! *parsing* is split into pure functions ([`parse_issues`] and friends) that are
//! unit-tested with no network; the HTTP layer itself is exercised against a loopback
//! `Stub` (no external host).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http::{HttpClient, HttpError, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use crate::rfc3339::parse_rfc3339;

/// A repository coordinate: the `(owner, name)` pair parsed once from the
/// `repo "owner/name"` setting (or each entry of an org's repo list).
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

/// A Gitea user, identified by login. The token's own login is the claim identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct User {
    /// The user's login handle.
    pub(crate) login: String,
}

/// An issue: the unit of work the kind turns into a run. Labels are carried by **name**;
/// resolving a name to the id a removal needs is [`crate::common`]'s business.
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
    /// The users currently assigned to the issue.
    pub(crate) assignees: Vec<User>,
}

impl Issue {
    /// Whether the issue carries a label named `name`.
    pub(crate) fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|l| l == name)
    }

    /// Whether `login` is one of the issue's assignees.
    pub(crate) fn assigned_to(&self, login: &str) -> bool {
        self.assignees.iter().any(|u| u.login == login)
    }
}

/// A top-level issue comment, with its author and its two times.
///
/// The two are **not** interchangeable, and each has one reader: `created_at` is when the
/// comment was written, the order a claim marker reads (an edit cannot reshuffle who spoke
/// first); `updated_at` is when it was last touched, the one the reply watermark compares
/// (an edit *is* something new to answer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueComment {
    /// The comment's stable id.
    pub(crate) id: u64,
    /// The comment body.
    pub(crate) body: String,
    /// The comment author.
    pub(crate) user: User,
    /// When the comment was created, as Gitea reports it.
    pub(crate) created_at: SystemTime,
    /// When the comment was last updated, as Gitea reports it.
    pub(crate) updated_at: SystemTime,
}

/// A repository label: a name, the numeric id needed to remove it from an issue
/// (the bare `DELETE …/labels` clears *all* labels, so removal must name the id),
/// and whether Gitea treats it as an **exclusive** scoped label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Label {
    /// The label's stable numeric id.
    pub(crate) id: u64,
    /// The label's display name (e.g. `afkd/claimed`).
    pub(crate) name: String,
    /// Whether the label is *exclusive* within its `scope/` (`Label.Exclusive`,
    /// `models/issues/label.go`). Adding an exclusive label **removes** every other
    /// exclusive label of the issue sharing its scope
    /// (`RemoveDuplicateExclusiveIssueLabels`, `models/issues/issue_label.go`, on
    /// the API path too) — which is why afkd's own managed labels must never be
    /// exclusive: the next `on_claim` add in the same scope would strip the claim
    /// gate off the issue and it would be claimed again on every poll.
    pub(crate) exclusive: bool,
}

/// A failure reaching Gitea, tagged with the stage it struck so a swallowed poll
/// error logs *where* it happened (afkd's built-in `GiteaError`, variant for variant).
#[derive(Debug)]
pub(crate) enum GiteaError {
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
    /// The forge answered **success** but did not do what was asked. Gitea silently
    /// drops a label name the repository does not define — `prepareForReplaceOrAdd`
    /// resolves names through `GetLabelIDsInRepoByNames`, an exact-name repo-scoped
    /// lookup with no error path (`routers/api/v1/repo/issue_label.go`) — and still
    /// answers `200` with the issue's resulting labels, so only that answer proves
    /// the write landed.
    Refused {
        /// The stage of work the call belonged to.
        stage: &'static str,
        /// What the forge did not do, in operator words.
        reason: String,
    },
}

/// The same four sentences afkd's `thiserror` attributes render, so a diagnostic reads
/// the same in `run.log` whichever of the two triggers wrote it.
impl std::fmt::Display for GiteaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GiteaError::Status { stage, status } => {
                write!(f, "gitea {stage}: forge returned status {status}")
            }
            GiteaError::Transport { stage, reason } => {
                write!(f, "gitea {stage}: no response ({reason})")
            }
            GiteaError::Decode { stage, reason } => {
                write!(f, "gitea {stage}: undecodable response ({reason})")
            }
            GiteaError::Refused { stage, reason } => write!(f, "gitea {stage}: {reason}"),
        }
    }
}

impl GiteaError {
    /// The stage label this error was tagged with.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        match self {
            GiteaError::Status { stage, .. }
            | GiteaError::Transport { stage, .. }
            | GiteaError::Decode { stage, .. }
            | GiteaError::Refused { stage, .. } => stage,
        }
    }
}

/// The failure vocabulary the shared HTTP spine reports through: the three staged
/// variants above, so the plumbing builds a [`GiteaError`] without naming Gitea.
impl HttpError for GiteaError {
    fn status(stage: &'static str, status: u16) -> Self {
        GiteaError::Status { stage, status }
    }
    fn transport(stage: &'static str, reason: String) -> Self {
        GiteaError::Transport { stage, reason }
    }
    fn decode(stage: &'static str, reason: &str) -> Self {
        GiteaError::Decode {
            stage,
            reason: reason.to_string(),
        }
    }
}

/// The Gitea operations the issue kind needs, by meaning (not by REST shape).
pub(crate) trait GiteaClient {
    /// Resolve the authenticated user (the PAT's own login) — `GET /user`.
    fn current_user(&self) -> Result<User, GiteaError>;

    /// List an org's repositories — `GET /orgs/{org}/repos`.
    fn org_repos(&self, org: &str) -> Result<Vec<Repo>, GiteaError>;

    /// List issues in `repo` filtered by `state` and (comma-separated) `labels` —
    /// `GET /repos/{o}/{r}/issues?type=issues&state&labels`.
    fn list_issues(&self, repo: &Repo, state: &str, labels: &str)
        -> Result<Vec<Issue>, GiteaError>;

    /// Read one issue (the claim re-read) — `GET /repos/{o}/{r}/issues/{index}`.
    fn get_issue(&self, repo: &Repo, index: u64) -> Result<Issue, GiteaError>;

    /// Replace an issue's assignees — `PATCH /repos/{o}/{r}/issues/{index}`.
    fn patch_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<Issue, GiteaError>;

    /// Set an issue's state (`open`/`closed`) —
    /// `PATCH /repos/{o}/{r}/issues/{index}`.
    fn set_state(&self, repo: &Repo, index: u64, state: &str) -> Result<(), GiteaError>;

    /// List the labels the **repository defines** — `GET /repos/{o}/{r}/labels`.
    /// The whole [`Label`], not just an id: a caller needs the id to remove a label
    /// and the `exclusive` flag to know whether afkd may use it as a marker.
    fn list_labels(&self, repo: &Repo) -> Result<Vec<Label>, GiteaError>;

    /// Define a label in the repository — `POST /repos/{o}/{r}/labels`. Gitea drops
    /// a label name the repo does not define, so afkd's own managed labels are
    /// created (always **non-exclusive**) before they are relied on.
    fn create_label(
        &self,
        repo: &Repo,
        name: &str,
        color: &str,
        exclusive: bool,
    ) -> Result<(), GiteaError>;

    /// Add a label (by name) to an issue —
    /// `POST /repos/{o}/{r}/issues/{index}/labels`. Succeeds only if the label is on
    /// the issue afterwards: an undefined name is dropped by Gitea under a `200`, so
    /// the reply's label list is checked and a drop is a
    /// [`Refused`](GiteaError::Refused).
    fn add_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GiteaError>;

    /// Remove one label (by id) from an issue —
    /// `DELETE /repos/{o}/{r}/issues/{index}/labels/{id}`.
    fn remove_label(&self, repo: &Repo, index: u64, id: &str) -> Result<(), GiteaError>;

    /// List an issue/PR's top-level comments —
    /// `GET /repos/{o}/{r}/issues/{index}/comments`.
    fn list_issue_comments(&self, repo: &Repo, index: u64)
        -> Result<Vec<IssueComment>, GiteaError>;

    /// Post a literal comment on an issue/PR, returning the created comment (its
    /// id and creation time, which a claim marker needs to recognise and order its
    /// own word) — `POST /repos/{o}/{r}/issues/{index}/comments`.
    fn post_comment(&self, repo: &Repo, index: u64, text: &str)
        -> Result<IssueComment, GiteaError>;

    /// Delete one comment by id (releasing a claim marker) —
    /// `DELETE /repos/{o}/{r}/issues/comments/{id}`. Gitea scopes the path to the
    /// **repo**, not the issue, so no index is carried.
    fn delete_comment(&self, repo: &Repo, comment_id: u64) -> Result<(), GiteaError>;

    /// Rewrite one comment's body by id (renewing a claim marker) —
    /// `PATCH /repos/{o}/{r}/issues/comments/{id}`. Repo-scoped like the delete it
    /// sits beside, and it moves the comment's `updated_at` — which is what a rival's
    /// claim decision reads as liveness.
    fn edit_comment(&self, repo: &Repo, comment_id: u64, text: &str) -> Result<(), GiteaError>;
}

/// Sharing a client behind an [`Arc`](std::sync::Arc) keeps it a [`GiteaClient`],
/// so a caller can retain a handle while also handing a trigger a boxed client.
impl<T: GiteaClient> GiteaClient for std::sync::Arc<T> {
    fn current_user(&self) -> Result<User, GiteaError> {
        (**self).current_user()
    }
    fn org_repos(&self, org: &str) -> Result<Vec<Repo>, GiteaError> {
        (**self).org_repos(org)
    }
    fn list_issues(
        &self,
        repo: &Repo,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GiteaError> {
        (**self).list_issues(repo, state, labels)
    }
    fn get_issue(&self, repo: &Repo, index: u64) -> Result<Issue, GiteaError> {
        (**self).get_issue(repo, index)
    }
    fn patch_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<Issue, GiteaError> {
        (**self).patch_assignees(repo, index, assignees)
    }
    fn set_state(&self, repo: &Repo, index: u64, state: &str) -> Result<(), GiteaError> {
        (**self).set_state(repo, index, state)
    }
    fn list_labels(&self, repo: &Repo) -> Result<Vec<Label>, GiteaError> {
        (**self).list_labels(repo)
    }
    fn create_label(
        &self,
        repo: &Repo,
        name: &str,
        color: &str,
        exclusive: bool,
    ) -> Result<(), GiteaError> {
        (**self).create_label(repo, name, color, exclusive)
    }
    fn add_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GiteaError> {
        (**self).add_label(repo, index, name)
    }
    fn remove_label(&self, repo: &Repo, index: u64, id: &str) -> Result<(), GiteaError> {
        (**self).remove_label(repo, index, id)
    }
    fn list_issue_comments(
        &self,
        repo: &Repo,
        index: u64,
    ) -> Result<Vec<IssueComment>, GiteaError> {
        (**self).list_issue_comments(repo, index)
    }
    fn post_comment(
        &self,
        repo: &Repo,
        index: u64,
        text: &str,
    ) -> Result<IssueComment, GiteaError> {
        (**self).post_comment(repo, index, text)
    }
    fn delete_comment(&self, repo: &Repo, comment_id: u64) -> Result<(), GiteaError> {
        (**self).delete_comment(repo, comment_id)
    }
    fn edit_comment(&self, repo: &Repo, comment_id: u64, text: &str) -> Result<(), GiteaError> {
        (**self).edit_comment(repo, comment_id, text)
    }
}

/// The `/api/v1` path prefix every endpoint hangs off the configured base URL.
const API_V1: &str = "/api/v1";

/// A [`GiteaClient`] backed by the Gitea `/api/v1` REST API.
pub(crate) struct Gitea {
    http: HttpClient<GiteaError>,
}

impl Gitea {
    /// A client authenticating against `base_url` (the instance root, e.g.
    /// `https://gitea.example.com`) with `token` as `Authorization: token <PAT>`.
    pub(crate) fn new(base_url: &str, token: &str) -> Self {
        // Normalize the configured base before it becomes the API root: a trailing
        // slash or stray whitespace would otherwise ride into every request. A
        // trailing slash doubles the separator on the wire — `…/gitea/` requests
        // `/gitea//api/v1/…`, a bare host `//api/v1/…` — and whitespace is worse:
        // the padded string does not parse as a URL at all, so every call fails at
        // the transport layer until it is trimmed. The spine never trims, so this is
        // the one place it happens.
        let base = base_url.trim().trim_end_matches('/');
        Self {
            http: HttpClient::new(
                format!("{base}{API_V1}"),
                // Auth rides a header (never a URL query), so the token cannot
                // leak through a transport-error URL.
                ("Authorization".to_string(), format!("token {token}")),
                DEFAULT_CONNECT_TIMEOUT,
                DEFAULT_READ_TIMEOUT,
            ),
        }
    }
}

impl GiteaClient for Gitea {
    fn current_user(&self) -> Result<User, GiteaError> {
        let stage = "current user";
        // GET /api/v1/user → the authenticated user.
        let body = self.http.get(stage, "/user")?;
        parse_user(stage, &body)
    }

    fn org_repos(&self, org: &str) -> Result<Vec<Repo>, GiteaError> {
        let stage = "list org repos";
        // GET /api/v1/orgs/{org}/repos → the org's repositories.
        let body = self.http.get(stage, &format!("/orgs/{org}/repos"))?;
        parse_repos(stage, &body)
    }

    fn list_issues(
        &self,
        repo: &Repo,
        state: &str,
        labels: &str,
    ) -> Result<Vec<Issue>, GiteaError> {
        let stage = "list issues";
        // GET /api/v1/repos/{o}/{r}/issues?type=issues&state=&labels= → open issues
        // carrying the source label. `type=issues` excludes PRs (Gitea models them
        // as issues too).
        let req = self
            .http
            .request(
                "GET",
                &format!("/repos/{}/{}/issues", repo.owner, repo.name),
            )
            .query("type", "issues")
            .query("state", state)
            .query("labels", labels);
        let body = self.http.send(stage, req)?;
        parse_issues(stage, &body)
    }

    fn get_issue(&self, repo: &Repo, index: u64) -> Result<Issue, GiteaError> {
        let stage = "get issue";
        // GET /api/v1/repos/{o}/{r}/issues/{index} → one issue (the claim re-read).
        let body = self.http.get(
            stage,
            &format!("/repos/{}/{}/issues/{index}", repo.owner, repo.name),
        )?;
        parse_issue(stage, &body)
    }

    fn patch_assignees(
        &self,
        repo: &Repo,
        index: u64,
        assignees: &[String],
    ) -> Result<Issue, GiteaError> {
        let stage = "patch assignees";
        // PATCH /api/v1/repos/{o}/{r}/issues/{index} { assignees } → replace the
        // assignee set (Gitea serializes this server-side, so the re-read detects a
        // lost race).
        let req = self.http.request(
            "PATCH",
            &format!("/repos/{}/{}/issues/{index}", repo.owner, repo.name),
        );
        let body = self
            .http
            .send_json(stage, req, &json!({ "assignees": assignees }))?;
        parse_issue(stage, &body)
    }

    fn set_state(&self, repo: &Repo, index: u64, state: &str) -> Result<(), GiteaError> {
        let stage = "set state";
        // PATCH /api/v1/repos/{o}/{r}/issues/{index} { state } → open→closed.
        let req = self.http.request(
            "PATCH",
            &format!("/repos/{}/{}/issues/{index}", repo.owner, repo.name),
        );
        self.http
            .send_json(stage, req, &json!({ "state": state }))
            .map(|_| ())
    }

    fn list_labels(&self, repo: &Repo) -> Result<Vec<Label>, GiteaError> {
        let stage = "list labels";
        // GET /api/v1/repos/{o}/{r}/labels → the labels the repository defines (the
        // ids the by-id removal path needs, and the `exclusive` flag the managed
        // labels are checked against).
        let body = self.http.get(
            stage,
            &format!("/repos/{}/{}/labels", repo.owner, repo.name),
        )?;
        parse_labels(stage, &body)
    }

    fn create_label(
        &self,
        repo: &Repo,
        name: &str,
        color: &str,
        exclusive: bool,
    ) -> Result<(), GiteaError> {
        let stage = "create label";
        // POST /api/v1/repos/{o}/{r}/labels { name, color, exclusive } → define a
        // label in the repository (`CreateLabelOption` requires `name` + `color`).
        // The created label's id is never needed — every later call names the label
        // — so the reply is not decoded.
        let req = self.http.request(
            "POST",
            &format!("/repos/{}/{}/labels", repo.owner, repo.name),
        );
        self.http
            .send_json(
                stage,
                req,
                &json!({ "name": name, "color": color, "exclusive": exclusive }),
            )
            .map(|_| ())
    }

    fn add_label(&self, repo: &Repo, index: u64, name: &str) -> Result<(), GiteaError> {
        let stage = "add label";
        // POST /api/v1/repos/{o}/{r}/issues/{index}/labels { labels: [name] } →
        // add the label (Gitea accepts label names in the array) and answer with the
        // issue's RESULTING label list. That answer is the only proof the label
        // landed: a name the repository does not define is resolved by
        // `GetLabelIDsInRepoByNames`, which has no error path, so Gitea drops it and
        // still answers success (ADR-0031 keeps that decode here, in the one file
        // that owns the shapes).
        let req = self.http.request(
            "POST",
            &format!("/repos/{}/{}/issues/{index}/labels", repo.owner, repo.name),
        );
        let body = self
            .http
            .send_json(stage, req, &json!({ "labels": [name] }))?;
        if parse_labels(stage, &body)?.iter().any(|l| l.name == name) {
            return Ok(());
        }
        Err(GiteaError::Refused {
            stage,
            reason: format!(
                "label “{name}” was not applied (the repository defines no such label)"
            ),
        })
    }

    fn remove_label(&self, repo: &Repo, index: u64, id: &str) -> Result<(), GiteaError> {
        let stage = "remove label";
        // DELETE /api/v1/repos/{o}/{r}/issues/{index}/labels/{id} → remove ONE
        // label by id (the bare `…/labels` path would clear all labels — ADR-0031).
        let req = self.http.request(
            "DELETE",
            &format!(
                "/repos/{}/{}/issues/{index}/labels/{id}",
                repo.owner, repo.name
            ),
        );
        self.http.send(stage, req).map(|_| ())
    }

    fn list_issue_comments(
        &self,
        repo: &Repo,
        index: u64,
    ) -> Result<Vec<IssueComment>, GiteaError> {
        let stage = "list comments";
        // GET /api/v1/repos/{o}/{r}/issues/{index}/comments → top-level discussion
        // comments (a PR is an issue, so its general comments live on this path).
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
    ) -> Result<IssueComment, GiteaError> {
        let stage = "post comment";
        // POST /api/v1/repos/{o}/{r}/issues/{index}/comments { body } → a
        // top-level comment (a PR is an issue, so its comments live on this path).
        // Gitea answers with the created comment; it is decoded rather than
        // discarded, so a claim marker can recognise and order its own word.
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

    fn delete_comment(&self, repo: &Repo, comment_id: u64) -> Result<(), GiteaError> {
        let stage = "delete comment";
        // DELETE /api/v1/repos/{o}/{r}/issues/comments/{id} → drop ONE comment.
        // The path is repo-scoped: the issue index is not part of it (ADR-0031).
        let req = self.http.request(
            "DELETE",
            &format!(
                "/repos/{}/{}/issues/comments/{comment_id}",
                repo.owner, repo.name
            ),
        );
        self.http.send(stage, req).map(|_| ())
    }

    fn edit_comment(&self, repo: &Repo, comment_id: u64, text: &str) -> Result<(), GiteaError> {
        let stage = "edit comment";
        // PATCH /api/v1/repos/{o}/{r}/issues/comments/{id} { body } → rewrite ONE
        // comment. Repo-scoped exactly like the DELETE beside it (ADR-0031); the
        // reply is the updated comment, and the renewal needs nothing from it.
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

// --- Pure parsing (no network): the Gitea JSON shapes. -----------------------
// The JSON extraction and the RFC-3339 reader are the spine's; what is Gitea's is
// which field each value type reads.

/// Parse the `GET /user` response into a [`User`].
pub(crate) fn parse_user(stage: &'static str, body: &str) -> Result<User, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    value_to_user(&value).ok_or_else(|| GiteaError::decode(stage, "user missing login"))
}

/// Parse an org-repos array into [`Repo`]s (each `full_name` → `owner/name`).
pub(crate) fn parse_repos(stage: &'static str, body: &str) -> Result<Vec<Repo>, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    Ok(GiteaError::as_array(stage, &value, "repos")?
        .iter()
        .filter_map(|r| {
            let full = r.get("full_name").and_then(Value::as_str)?;
            Repo::parse(full)
        })
        .collect())
}

/// Parse an issues array into [`Issue`]s.
pub(crate) fn parse_issues(stage: &'static str, body: &str) -> Result<Vec<Issue>, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    Ok(GiteaError::as_array(stage, &value, "issues")?
        .iter()
        .filter_map(value_to_issue)
        .collect())
}

/// Parse a single-issue response into an [`Issue`].
pub(crate) fn parse_issue(stage: &'static str, body: &str) -> Result<Issue, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    value_to_issue(&value).ok_or_else(|| GiteaError::decode(stage, "issue missing number"))
}

/// Parse a labels array into [`Label`]s.
pub(crate) fn parse_labels(stage: &'static str, body: &str) -> Result<Vec<Label>, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    Ok(GiteaError::as_array(stage, &value, "labels")?
        .iter()
        .filter_map(value_to_label)
        .collect())
}

/// Parse an issue-comments array into [`IssueComment`]s.
pub(crate) fn parse_comments(
    stage: &'static str,
    body: &str,
) -> Result<Vec<IssueComment>, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    Ok(GiteaError::as_array(stage, &value, "comments")?
        .iter()
        .filter_map(value_to_comment)
        .collect())
}

/// Parse a single-comment response (the reply to a comment POST) into an
/// [`IssueComment`]. Strict: a reply that is not a comment object is a decode
/// failure rather than a synthesized value, so a claim can never be built on a
/// fabricated id. Shares `value_to_comment` with [`parse_comments`], so the POST
/// reply and the list reply cannot read a comment differently.
pub(crate) fn parse_comment(stage: &'static str, body: &str) -> Result<IssueComment, GiteaError> {
    let value = GiteaError::decode_json(stage, body)?;
    value_to_comment(&value).ok_or_else(|| GiteaError::decode(stage, "comment missing id"))
}

fn value_to_user(v: &Value) -> Option<User> {
    let login = v.get("login")?.as_str()?.to_string();
    Some(User { login })
}

fn value_to_label(v: &Value) -> Option<Label> {
    Some(Label {
        id: v.get("id")?.as_u64()?,
        name: v.get("name")?.as_str()?.to_string(),
        // A label object always carries `exclusive`; an absent one is the plain
        // (non-exclusive) kind, which is also the safe reading — it is exclusivity,
        // never its absence, that afkd refuses to build its gate on.
        exclusive: v.get("exclusive").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn value_to_issue(v: &Value) -> Option<Issue> {
    let number = v.get("number")?.as_u64()?;
    // A unit carries label *names*; the id an unlabel needs is resolved separately
    // through `label_id`, so it is dropped here rather than threaded onto the issue.
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(value_to_label)
                .map(|l| l.name)
                .collect()
        })
        .unwrap_or_default();
    let assignees = v
        .get("assignees")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(value_to_user).collect())
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
        assignees,
    })
}

fn value_to_comment(v: &Value) -> Option<IssueComment> {
    let id = v.get("id")?.as_u64()?;
    let user = v.get("user").and_then(value_to_user).unwrap_or(User {
        login: String::new(),
    });
    // Both instants read through the same RFC-3339 reader, and both degrade to the
    // epoch when absent — an absent field stays distinguishable from a present one
    // rather than being masked by its sibling's value.
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
    //! An in-memory [`GiteaClient`] (test-only). No network: every trigger and
    //! settings test drives this mock; the real [`Gitea`] HTTP code is exercised
    //! only against the loopback `Stub` below.

    use super::*;
    use crate::common::lock;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A recorded mutation against the mock forge, for assertions.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Action {
        /// An issue's assignees were replaced.
        Assign {
            /// The issue/PR number.
            index: u64,
            /// The new assignee set.
            assignees: Vec<String>,
        },
        /// A label was added to an issue.
        Label {
            /// The issue/PR number.
            index: u64,
            /// The label name added.
            name: String,
        },
        /// A label was removed from an issue (by id).
        Unlabel {
            /// The issue/PR number.
            index: u64,
            /// The label id removed (never the all-clearing bare path).
            id: String,
        },
        /// An issue's state was set.
        State {
            /// The issue/PR number.
            index: u64,
            /// The new state (`open`/`closed`).
            state: String,
        },
        /// A literal comment was posted on an issue/PR.
        Comment {
            /// The issue/PR number.
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

    /// An in-memory forge: tests seed issues/PRs/comments/reviews (each with an
    /// explicit timestamp), drive a trigger against it, and inspect the recorded
    /// [`Action`]s. A stage can be made to fail or panic, the second a posted
    /// comment is stamped with can be frozen ([`MockClient::set_clock`], which is
    /// what makes a *same-second* claim race expressible), and a rival claim marker
    /// can be injected into the next comment read to exercise the lost-race path.
    #[derive(Default)]
    pub(crate) struct MockClient {
        me: Mutex<String>,
        repos: Mutex<HashMap<String, Vec<Repo>>>,
        issues: Mutex<Vec<Issue>>,
        labels: Mutex<Vec<Label>>,
        comments: Mutex<HashMap<u64, Vec<IssueComment>>>,
        actions: Mutex<Vec<Action>>,
        /// How many `list_issue_comments` calls were attempted — the seam a test
        /// reads to pin the default claim path's comment traffic at zero.
        comment_reads: Mutex<u32>,
        fail_stage: Mutex<Option<&'static str>>,
        /// The stage a `refuse` knob makes answer a definite HTTP status, and which
        /// status — the *definite* half of the failure vocabulary (`fail_stage` only
        /// ever produces the transient transport error).
        refuse_stage: Mutex<Option<(&'static str, u16)>>,
        /// How many comments have been posted through `post_comment`, so each gets a
        /// distinct id and a strictly newer timestamp than any seeded one.
        posted: Mutex<u64>,
        /// When set, every posted comment is stamped this many seconds after the
        /// epoch instead of the minted per-post one — so two posts land in the *same
        /// second* with distinct, increasing ids and the claim's tie-break is the
        /// only thing that can decide.
        post_clock: Mutex<Option<u64>>,
        /// When set, the next `list_issue_comments` first drops this rival claim
        /// marker into the thread: a marker that landed between our post and our
        /// re-read, with the test choosing its side of the `(created_at, id)` order.
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

        /// Seed an org's repository list.
        pub(crate) fn set_org_repos(&self, org: &str, repos: &[Repo]) {
            lock(&self.repos).insert(org.to_string(), repos.to_vec());
        }

        /// Seed an issue (open, with the given labels and no assignees).
        pub(crate) fn add_issue(&self, number: u64, title: &str, body: &str, labels: &[&str]) {
            self.add_issue_assigned(number, title, body, labels, &[]);
        }

        /// Seed an open issue with the given labels **and** assignees. Unlike
        /// driving `patch_assignees`, this records no `Action`, so it seeds a
        /// starting state without polluting the recorded-mutation assertions.
        pub(crate) fn add_issue_assigned(
            &self,
            number: u64,
            title: &str,
            body: &str,
            labels: &[&str],
            assignees: &[&str],
        ) {
            // `self.label` is called for its side effect — registering the name in
            // the repo's label set, which `list_issues` reads to model real Gitea's
            // degenerate `labels=` filter — while the issue carries only the name.
            let labels = labels
                .iter()
                .map(|n| self.label(n).name)
                .collect::<Vec<_>>();
            let assignees = assignees
                .iter()
                .map(|l| User {
                    login: (*l).to_string(),
                })
                .collect();
            lock(&self.issues).push(Issue {
                number,
                title: title.to_string(),
                body: body.to_string(),
                state: "open".to_string(),
                labels,
                assignees,
            });
        }

        /// Seed a comment on an issue/PR by `author`, updated `secs` after epoch.
        pub(crate) fn add_comment(&self, index: u64, id: u64, author: &str, secs: u64) {
            self.add_comment_body(index, id, author, &format!("comment {id}"), secs);
        }

        /// Seed a comment with an explicit body — what the brief actually renders,
        /// so an attribution test can carry real prose rather than `comment <id>`.
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

        /// Seed a comment written at `created` and last edited at `updated` — the
        /// shape that tells the claim's order key (creation) apart from the reply
        /// gate's (the edit).
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

        /// Make every call belonging to `stage` fail with a transport error — the
        /// **transient** failure, the one a retry might get past.
        pub(crate) fn fail(&self, stage: &'static str) {
            *lock(&self.fail_stage) = Some(stage);
        }

        /// Make every call belonging to `stage` answer HTTP `status` — the
        /// **definite** failure a 4xx is, so the fatal/transient split is
        /// expressible against the same stage [`fail`](Self::fail) makes flaky.
        pub(crate) fn refuse(&self, stage: &'static str, status: u16) {
            *lock(&self.refuse_stage) = Some((stage, status));
        }

        /// Stop failing.
        pub(crate) fn clear_failure(&self) {
            *lock(&self.fail_stage) = None;
            *lock(&self.refuse_stage) = None;
        }

        /// Freeze the second every subsequent `post_comment` is stamped with, leaving
        /// the id counter alone. Two claims posted under one frozen clock therefore
        /// carry **equal** `created_at` and distinct, increasing ids — the
        /// same-second race, where the id tie-break is the only thing that can decide
        /// the winner. Unset (the default) keeps the minted, strictly-increasing
        /// stamp.
        pub(crate) fn set_clock(&self, secs: u64) {
            *lock(&self.post_clock) = Some(secs);
        }

        /// Arm a concurrent rival: the next `list_issue_comments` finds a rival
        /// `[afkd-claim]` marker in the thread — one that landed between our post and
        /// our re-read — authored by and owned by `owner`, with comment id `id` and
        /// created `secs` after the epoch. The test picks `id`/`secs` to place the
        /// rival either side of our marker in the `(created_at, id)` order.
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

        /// How many comment reads the driver attempted (see `comment_reads`).
        pub(crate) fn comment_reads(&self) -> u32 {
            *lock(&self.comment_reads)
        }

        /// The current assignees on an issue/PR (for assertions).
        pub(crate) fn assignees_of(&self, index: u64) -> Vec<String> {
            lock(&self.issues)
                .iter()
                .find(|i| i.number == index)
                .map(|i| i.assignees.iter().map(|u| u.login.clone()).collect())
                .unwrap_or_default()
        }

        /// Whether issue/PR `index` carries label `name` (for assertions).
        pub(crate) fn has_label(&self, index: u64, name: &str) -> bool {
            lock(&self.issues)
                .iter()
                .find(|i| i.number == index)
                .map(|i| i.has_label(name))
                .unwrap_or(false)
        }

        /// Define a label in the repository's own label set, as a repo that has
        /// already been set up by hand (or by an earlier afkd run) has it. This is
        /// the seam a fixture uses to say "this repo defines `needs-triage`" —
        /// without it, an `add_label` naming it is dropped, exactly as Gitea drops
        /// it. Re-registering a known name re-flags its exclusivity.
        pub(crate) fn register_label(&self, name: &str, exclusive: bool) {
            let mut labels = lock(&self.labels);
            if let Some(l) = labels.iter_mut().find(|l| l.name == name) {
                l.exclusive = exclusive;
                return;
            }
            let id = labels.len() as u64 + 1;
            labels.push(Label {
                id,
                name: name.to_string(),
                exclusive,
            });
        }

        /// Resolve a label by name, lazily defining an unknown one (non-exclusive)
        /// and minting a stable id. Only the issue seeders mint: a label on a seeded
        /// issue *is* one the repository defines. `add_label` deliberately does not.
        /// A name the fixture already defined keeps the flag it was given.
        fn label(&self, name: &str) -> Label {
            if let Some(label) = self.label_named(name) {
                return label;
            }
            self.register_label(name, false);
            self.label_named(name)
                .unwrap_or_else(|| panic!("just-registered label '{name}'"))
        }

        /// The repository's definition of `name`, if it has one.
        fn label_named(&self, name: &str) -> Option<Label> {
            lock(&self.labels).iter().find(|l| l.name == name).cloned()
        }

        fn guard(&self, stage: &'static str) -> Result<(), GiteaError> {
            if let Some((refused, status)) = *lock(&self.refuse_stage) {
                if refused == stage {
                    return Err(GiteaError::Status { stage, status });
                }
            }
            if *lock(&self.fail_stage) == Some(stage) {
                Err(GiteaError::Transport {
                    stage,
                    reason: "mock failure".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    /// Gitea's `Label.ExclusiveScope()` (`models/issues/label.go`): the text before
    /// the **last** `/` of an *exclusive* label's name — and no scope at all for a
    /// non-exclusive one, or a name whose `/` is missing, leading, or trailing.
    /// Two exclusive labels sharing a scope cannot both sit on one issue.
    fn exclusive_scope(label: &Label) -> Option<&str> {
        if !label.exclusive {
            return None;
        }
        let cut = label.name.rfind('/')?;
        if cut == 0 || cut + 1 == label.name.len() {
            return None;
        }
        Some(&label.name[..cut])
    }

    impl GiteaClient for MockClient {
        fn current_user(&self) -> Result<User, GiteaError> {
            self.guard("current user")?;
            Ok(User {
                login: lock(&self.me).clone(),
            })
        }

        fn org_repos(&self, org: &str) -> Result<Vec<Repo>, GiteaError> {
            self.guard("list org repos")?;
            Ok(lock(&self.repos).get(org).cloned().unwrap_or_default())
        }

        fn list_issues(
            &self,
            _repo: &Repo,
            state: &str,
            labels: &str,
        ) -> Result<Vec<Issue>, GiteaError> {
            self.guard("list issues")?;
            // Real Gitea's `labels=` filter is DEGENERATE when the named label is
            // not defined in the repo: it silently drops the filter and returns
            // *every* open issue (the over-claim bug the kind must guard). Model
            // that here — only honour the label filter when the repo defines it.
            let defined = labels.is_empty() || lock(&self.labels).iter().any(|l| l.name == labels);
            Ok(lock(&self.issues)
                .iter()
                .filter(|i| state == "all" || i.state == state)
                .filter(|i| labels.is_empty() || !defined || i.has_label(labels))
                .cloned()
                .collect())
        }

        fn get_issue(&self, _repo: &Repo, index: u64) -> Result<Issue, GiteaError> {
            self.guard("get issue")?;
            lock(&self.issues)
                .iter()
                .find(|i| i.number == index)
                .cloned()
                .ok_or(GiteaError::Status {
                    stage: "get issue",
                    status: 404,
                })
        }

        fn patch_assignees(
            &self,
            _repo: &Repo,
            index: u64,
            assignees: &[String],
        ) -> Result<Issue, GiteaError> {
            self.guard("patch assignees")?;
            let mut issues = lock(&self.issues);
            let issue =
                issues
                    .iter_mut()
                    .find(|i| i.number == index)
                    .ok_or(GiteaError::Status {
                        stage: "patch assignees",
                        status: 404,
                    })?;
            // Gitea's PATCH is a replace-set: whatever the caller sends becomes the
            // whole assignee list.
            issue.assignees = assignees
                .iter()
                .map(|l| User { login: l.clone() })
                .collect();
            lock(&self.actions).push(Action::Assign {
                index,
                assignees: assignees.to_vec(),
            });
            Ok(issue.clone())
        }

        fn set_state(&self, _repo: &Repo, index: u64, state: &str) -> Result<(), GiteaError> {
            self.guard("set state")?;
            if let Some(issue) = lock(&self.issues).iter_mut().find(|i| i.number == index) {
                issue.state = state.to_string();
            }
            lock(&self.actions).push(Action::State {
                index,
                state: state.to_string(),
            });
            Ok(())
        }

        fn list_labels(&self, _repo: &Repo) -> Result<Vec<Label>, GiteaError> {
            self.guard("list labels")?;
            Ok(lock(&self.labels).clone())
        }

        fn create_label(
            &self,
            _repo: &Repo,
            name: &str,
            _color: &str,
            exclusive: bool,
        ) -> Result<(), GiteaError> {
            self.guard("create label")?;
            self.register_label(name, exclusive);
            Ok(())
        }

        fn add_label(&self, _repo: &Repo, index: u64, name: &str) -> Result<(), GiteaError> {
            self.guard("add label")?;
            // The mock does NOT invent labels: real Gitea silently drops a name the
            // repository does not define (`GetLabelIDsInRepoByNames` has no error
            // path) and answers success with the issue's resulting labels, which the
            // client reads back as a `Refused`. Model both halves — nothing lands on
            // the issue, and the call is an error — so no fixture can prove a claim
            // gate that real Gitea would have thrown away.
            //
            // Resolve the name and the exclusive siblings this add would strip
            // together, under the labels lock and before the issues one (the
            // labels-then-issues order every mutation here takes).
            let (label, stripped) = {
                let labels = lock(&self.labels);
                let Some(label) = labels.iter().find(|l| l.name == name).cloned() else {
                    return Err(GiteaError::Refused {
                        stage: "add label",
                        reason: format!(
                            "label “{name}” was not applied \
                             (the repository defines no such label)"
                        ),
                    });
                };
                // Adding an exclusive label removes every other exclusive label of
                // the issue sharing its scope (`RemoveDuplicateExclusiveIssueLabels`,
                // `models/issues/issue_label.go`) — the trap that would silently strip
                // an exclusive claim gate on the next `on_claim` add.
                let stripped: Vec<String> = match exclusive_scope(&label) {
                    Some(scope) => labels
                        .iter()
                        .filter(|l| l.name != label.name && exclusive_scope(l) == Some(scope))
                        .map(|l| l.name.clone())
                        .collect(),
                    None => Vec::new(),
                };
                (label, stripped)
            };
            if let Some(issue) = lock(&self.issues).iter_mut().find(|i| i.number == index) {
                issue.labels.retain(|l| !stripped.contains(l));
                if !issue.has_label(name) {
                    issue.labels.push(label.name);
                }
            }
            lock(&self.actions).push(Action::Label {
                index,
                name: name.to_string(),
            });
            Ok(())
        }

        fn remove_label(&self, _repo: &Repo, index: u64, id: &str) -> Result<(), GiteaError> {
            self.guard("remove label")?;
            // Gitea removes by id, but the issue names its labels: resolve the id
            // through the repo's label set first, taking the locks in `add_label`'s
            // labels-then-issues order.
            let name = lock(&self.labels)
                .iter()
                .find(|l| l.id.to_string() == id)
                .map(|l| l.name.clone());
            if let Some(issue) = lock(&self.issues).iter_mut().find(|i| i.number == index) {
                issue.labels.retain(|l| Some(l) != name.as_ref());
            }
            lock(&self.actions).push(Action::Unlabel {
                index,
                id: id.to_string(),
            });
            Ok(())
        }

        fn list_issue_comments(
            &self,
            _repo: &Repo,
            index: u64,
        ) -> Result<Vec<IssueComment>, GiteaError> {
            // Counted before the guard: an attempted read is traffic even when the
            // stage is failed.
            *lock(&self.comment_reads) += 1;
            self.guard("list comments")?;
            // An armed rival's marker lands in the thread just before this read —
            // the concurrent claim that was posted while we were settling. It joins
            // the thread for good, as a real one would.
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
        ) -> Result<IssueComment, GiteaError> {
            self.guard("post comment")?;
            // A posted comment joins the thread, authored by the authenticated user
            // and newest — as the forge does. A later read must see it, or the
            // last-speaker logic could never observe afkd's own word.
            let me = lock(&self.me).clone();
            let n = {
                let mut posted = lock(&self.posted);
                *posted += 1;
                *posted
            };
            // The stamp is the frozen clock when one is set (so successive posts tie
            // in the same second), else the minted, strictly-increasing one.
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

        fn delete_comment(&self, _repo: &Repo, comment_id: u64) -> Result<(), GiteaError> {
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
        ) -> Result<(), GiteaError> {
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
    fn claim_patch_then_reread_reflects_the_assignee() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &["afkd/ready"]);
        let issue = c.patch_assignees(&repo(), 7, &["me".into()]).unwrap();
        assert_eq!(issue.assignees, vec![User { login: "me".into() }]);
        assert_eq!(c.assignees_of(7), vec!["me".to_string()]);
    }

    /// The lost-race knob: an armed rival marker shows up in the **next** comment
    /// read, carrying the id and second the test chose (so it can be placed either
    /// side of ours in the claim order) — and only once, however often the thread is
    /// read afterwards.
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

    /// The frozen post clock: successive posts tie in one second while their ids
    /// keep increasing — the fixture a same-second claim race is staged on. Without
    /// it each post is stamped a second later than the last.
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

    /// A label the repository **defines** lands on the issue and comes back off by
    /// its id — the round trip every lifecycle `label_add`/`label_remove` pair makes.
    #[test]
    fn a_registered_label_lands_and_removes_by_id() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        c.register_label("afkd/claimed", false);
        c.add_label(&repo(), 7, "afkd/claimed").unwrap();
        assert!(c.has_label(7, "afkd/claimed"));
        let id = c
            .list_labels(&repo())
            .unwrap()
            .into_iter()
            .find(|l| l.name == "afkd/claimed")
            .map(|l| l.id.to_string())
            .expect("the repo defines it");
        c.remove_label(&repo(), 7, &id).unwrap();
        assert!(!c.has_label(7, "afkd/claimed"));
        assert!(c
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Unlabel { id: got, .. } if got == &id)));
    }

    /// The seam the whole card rests on: the mock models real Gitea's **silent drop**
    /// of a label name the repository does not define. Nothing lands, nothing is
    /// recorded, and the call is the same `Refused` the real client raises from the
    /// POST reply — so a fixture can no longer prove a gate Gitea would have thrown
    /// away. A `create_label` (what the kind's ensure does) makes the very same
    /// add succeed.
    #[test]
    fn the_mock_does_not_invent_labels() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);

        let err = c
            .add_label(&repo(), 7, "afkd/claimed")
            .expect_err("an undefined label name is dropped, not minted");
        assert!(
            matches!(
                err,
                GiteaError::Refused {
                    stage: "add label",
                    ..
                }
            ),
            "got {err:?}"
        );
        assert!(!c.has_label(7, "afkd/claimed"));
        assert!(
            c.list_labels(&repo()).unwrap().is_empty(),
            "the drop defined nothing either: {:?}",
            c.list_labels(&repo())
        );
        assert!(
            !c.actions()
                .iter()
                .any(|a| matches!(a, Action::Label { .. })),
            "a dropped add is not a recorded mutation: {:?}",
            c.actions()
        );

        c.create_label(&repo(), "afkd/claimed", "#7057ff", false)
            .unwrap();
        c.add_label(&repo(), 7, "afkd/claimed").unwrap();
        assert!(c.has_label(7, "afkd/claimed"));
        assert_eq!(
            c.list_labels(&repo())
                .unwrap()
                .into_iter()
                .map(|l| (l.name, l.exclusive))
                .collect::<Vec<_>>(),
            vec![("afkd/claimed".to_string(), false)],
            "the create defined it non-exclusive"
        );
    }

    /// The exclusive trap, modelled: adding an **exclusive** label removes every
    /// other exclusive label of the issue sharing its scope
    /// (`RemoveDuplicateExclusiveIssueLabels`). The scope is the text before the
    /// **last** `/`, and a name with no usable `/` — none at all, leading, or
    /// trailing — has none, so it strips nothing however exclusive it is. A
    /// non-exclusive label is never stripped by anything, which is why afkd's gate
    /// must be one.
    #[test]
    fn an_exclusive_label_add_strips_its_scope_siblings() {
        let c = MockClient::new("me");
        c.add_issue(7, "T", "B", &[]);
        c.register_label("afkd/claimed", false);
        c.register_label("afkd/inprogress", true);
        c.register_label("afkd/review/deep", true);
        c.register_label("afkd/review/quick", true);
        // The scope-less shapes: no `/`, a leading one, a trailing one.
        c.register_label("blocked", true);
        c.register_label("/rooted", true);
        c.register_label("afkd/", true);

        for name in [
            "afkd/claimed",
            "afkd/inprogress",
            "afkd/review/deep",
            "blocked",
            "/rooted",
            "afkd/",
        ] {
            c.add_label(&repo(), 7, name).unwrap();
        }
        assert!(
            c.has_label(7, "afkd/claimed"),
            "a NON-exclusive label survives an exclusive add in its own scope"
        );
        assert!(c.has_label(7, "afkd/inprogress"));
        for name in ["blocked", "/rooted", "afkd/"] {
            assert!(
                c.has_label(7, name),
                "{name} has no scope, so nothing stripped it"
            );
        }

        // A sibling in the *same* scope evicts the exclusive one that was there;
        // `afkd/inprogress` sits in scope `afkd`, not `afkd/review`, and stays.
        c.add_label(&repo(), 7, "afkd/review/quick").unwrap();
        assert!(!c.has_label(7, "afkd/review/deep"), "same scope, evicted");
        assert!(c.has_label(7, "afkd/review/quick"));
        assert!(
            c.has_label(7, "afkd/inprogress"),
            "a different scope is untouched"
        );
        assert!(
            c.has_label(7, "afkd/claimed"),
            "and the plain label still holds"
        );
    }

    /// The definite/transient knobs are distinct failures on the same stage: `fail`
    /// is a transport blip, `refuse` the forge's own 4xx verdict.
    #[test]
    fn refuse_is_a_status_where_fail_is_a_transport_error() {
        let c = MockClient::new("me");
        c.refuse("create label", 403);
        let err = c
            .create_label(&repo(), "afkd/claimed", "#7057ff", false)
            .unwrap_err();
        assert!(
            matches!(
                err,
                GiteaError::Status {
                    stage: "create label",
                    status: 403
                }
            ),
            "got {err:?}"
        );
        c.clear_failure();
        c.fail("create label");
        assert!(matches!(
            c.create_label(&repo(), "afkd/claimed", "#7057ff", false)
                .unwrap_err(),
            GiteaError::Transport { .. }
        ));
        c.clear_failure();
        c.create_label(&repo(), "afkd/claimed", "#7057ff", false)
            .unwrap();
    }

    #[test]
    fn failure_injection_is_scoped_to_a_stage() {
        let c = MockClient::new("me");
        c.fail("list issues");
        assert!(c.list_issues(&repo(), "open", "").is_err());
        c.clear_failure();
        assert!(c.list_issues(&repo(), "open", "").is_ok());
    }

    /// The claim-marker round trip through the mock: the posted comment comes back
    /// identified (a minted id above every seeded one, and a creation time), a read
    /// sees it in the thread, and deleting it by that id takes it back out.
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
            "the posted comment joined the thread"
        );

        c.delete_comment(&repo(), posted.id).unwrap();
        assert_eq!(
            c.list_issue_comments(&repo(), 7)
                .unwrap()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![42],
            "only the deleted comment left the thread"
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

    /// The `owner/name` grammar the settings feed: exactly two non-empty segments, and
    /// `full_name` round-trips what `parse` took.
    #[test]
    fn repo_parse_rejects_malformed() {
        assert_eq!(
            Repo::parse("a/b"),
            Some(Repo {
                owner: "a".into(),
                name: "b".into()
            })
        );
        for bad in ["no-slash", "a/b/c", "/b", "a/", "/", ""] {
            assert!(Repo::parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            Repo::parse("björn-öst/verktyg").map(|r| r.full_name()),
            Some("björn-öst/verktyg".to_string())
        );
    }

    /// The two issue readers the eligibility rules gate on match exactly: no prefix
    /// match, no case folding, no unicode folding.
    #[test]
    fn label_and_assignee_lookups_match_exactly() {
        let issue = Issue {
            number: 42,
            title: "Küchenspüle leaks".into(),
            body: String::new(),
            state: "open".into(),
            labels: vec!["afkd/ready".into(), "afkd/ready-soon".into(), "P1".into()],
            assignees: vec![
                User {
                    login: "bob-döner".into(),
                },
                User {
                    login: "alice".into(),
                },
            ],
        };
        assert!(issue.has_label("afkd/ready"));
        assert!(issue.has_label("afkd/ready-soon"));
        assert!(!issue.has_label("afkd/read"), "no prefix match");
        assert!(!issue.has_label("p1"), "label match is case-sensitive");
        assert!(issue.assigned_to("bob-döner"));
        assert!(!issue.assigned_to("bob-doner"), "no unicode folding");
        assert!(!issue.assigned_to("Alice"), "login match is case-sensitive");
        let bare = Issue {
            labels: Vec::new(),
            assignees: Vec::new(),
            ..issue
        };
        assert!(!bare.has_label("afkd/ready"));
        assert!(!bare.assigned_to("alice"));
    }

    #[test]
    fn parse_issues_reads_number_state_labels_and_assignees() {
        let body = r#"[
            {"number":4,"title":"Fix","body":"do it","state":"open",
             "labels":[{"id":9,"name":"afkd/ready"}],
             "assignees":[{"login":"bot"}]}
        ]"#;
        let issues = parse_issues("list issues", body).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 4);
        assert_eq!(issues[0].title, "Fix");
        assert!(issues[0].has_label("afkd/ready"));
        assert_eq!(issues[0].assignees[0].login, "bot");
    }

    #[test]
    fn parse_repos_reads_full_name_pairs() {
        let body = r#"[{"full_name":"acme/widgets"},{"full_name":"acme/gadgets"}]"#;
        let repos = parse_repos("list org repos", body).unwrap();
        assert_eq!(
            repos,
            vec![
                Repo {
                    owner: "acme".into(),
                    name: "widgets".into()
                },
                Repo {
                    owner: "acme".into(),
                    name: "gadgets".into()
                },
            ]
        );
    }

    /// The three shapes a real thread mixes: a fresh comment, an **edited** one
    /// whose `created_at` is two hours before its `updated_at`, and one the forge
    /// sent without a `created_at` at all. The two times are distinct fields, read
    /// through the same RFC-3339 reader (so a `+02:00` stamp lands where a `Z` one
    /// would), and a missing one degrades to the epoch rather than borrowing its
    /// sibling's value.
    #[test]
    fn parse_comments_reads_authors_and_times() {
        let comments = parse_comments(
            "list comments",
            r#"[
                {"id":1,"body":"hi","user":{"login":"human"},
                 "created_at":"2021-01-01T00:00:00Z","updated_at":"2021-01-01T00:00:00Z"},
                {"id":2,"body":"The token expires mid-retry — see §4 🙏","user":{"login":"björn-öst"},
                 "created_at":"2021-01-01T02:00:00+02:00","updated_at":"2021-01-01T02:00:00Z"},
                {"id":3,"body":"no creation stamp","user":{"login":"human"},
                 "updated_at":"2021-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(comments[0].user.login, "human");
        assert_eq!(comments[0].created_at, t0);
        assert_eq!(comments[0].updated_at, t0);

        // Edited: written at 00:00Z (spelled `02:00+02:00`), touched two hours later.
        assert_eq!(comments[1].user.login, "björn-öst");
        assert_eq!(comments[1].created_at, t0);
        assert_eq!(comments[1].updated_at, t0 + Duration::from_secs(7_200));
        assert!(
            comments[1].created_at < comments[1].updated_at,
            "an edited comment was created before it was updated"
        );

        // Absent `created_at` degrades to the epoch — distinguishable from a stamp
        // equal to `updated_at`, which is what a fallback to the sibling would give.
        assert_eq!(comments[2].created_at, UNIX_EPOCH);
        assert_eq!(comments[2].updated_at, t0);
    }

    /// The POST reply decodes through the very same `value_to_comment` the list
    /// reply does, and is **strict**: a reply carrying no `id` is a staged decode
    /// error, never a synthesized comment a claim could be built on.
    #[test]
    fn parse_comment_reads_the_posted_comment_or_fails_loudly() {
        let posted = parse_comment(
            "post comment",
            r#"{"id":90210,"body":"[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                "user":{"login":"björn-öst[bot]"},
                "created_at":"2026-07-20T11:00:00+02:00","updated_at":"2026-07-20T09:00:00Z"}"#,
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
        assert!(
            matches!(
                err,
                GiteaError::Decode {
                    stage: "post comment",
                    ..
                }
            ),
            "a reply with no id must be a staged decode error, got {err:?}"
        );
    }

    /// The `exclusive` flag comes off the label JSON as Gitea spells it, and a label
    /// object without the field is the plain kind — the reading a repo's own
    /// hand-made labels and afkd's created pair both rely on.
    #[test]
    fn parse_labels_reads_exclusive_defaulting_to_false() {
        let labels = parse_labels(
            "list labels",
            r#"[{"id":1,"name":"afkd/claimed","exclusive":false},
                {"id":2,"name":"afkd/inprogress","exclusive":true},
                {"id":3,"name":"needs-triage"},
                {"id":4,"name":"prioritet/hög 🚨","exclusive":true}]"#,
        )
        .unwrap();
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.name.as_str(), l.exclusive))
                .collect::<Vec<_>>(),
            vec![
                ("afkd/claimed", false),
                ("afkd/inprogress", true),
                ("needs-triage", false),
                ("prioritet/hög 🚨", true),
            ]
        );
    }

    #[test]
    fn decode_failures_are_tagged_with_their_stage() {
        let err = parse_issues("list issues", "not json").unwrap_err();
        assert_eq!(err.stage(), "list issues");
        let err = parse_repos("list org repos", r#"{"not":"array"}"#).unwrap_err();
        assert!(matches!(err, GiteaError::Decode { .. }));
    }

    #[test]
    fn gitea_error_stage_is_reported_for_every_variant() {
        assert_eq!(
            GiteaError::Status {
                stage: "list issues",
                status: 500
            }
            .stage(),
            "list issues"
        );
        assert_eq!(
            GiteaError::Transport {
                stage: "get issue",
                reason: "x".into()
            }
            .stage(),
            "get issue"
        );
        assert_eq!(
            GiteaError::Decode {
                stage: "set state",
                reason: "y".into()
            }
            .stage(),
            "set state"
        );
        assert_eq!(
            GiteaError::Refused {
                stage: "add label",
                reason: "z".into()
            }
            .stage(),
            "add label"
        );
    }
}

#[cfg(test)]
mod http_tests {
    //! The real [`Gitea`] HTTP client's request construction, exercised against a
    //! loopback `Stub` (a local socket — **no external network**, the posture
    //! ADR-0031 endorses). Each test pins one endpoint's method + path so every
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
            // Match the header name case-insensitively, but return the value with
            // its original case preserved (the token's case matters).
            let want = format!("{}:", name.to_ascii_lowercase());
            self.headers.iter().find_map(|h| {
                if h.to_ascii_lowercase().starts_with(&want) {
                    Some(h[want.len()..].trim().to_string())
                } else {
                    None
                }
            })
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

    /// A one-shot HTTP/1.1 stub: serves one canned reply to one request and hands
    /// the captured request back, driving the real [`Gitea`] code over a socket.
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

        fn client(&self, token: &str) -> Gitea {
            Gitea::new(&self.base, token)
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
    fn current_user_gets_user_with_token_auth() {
        let stub = Stub::serve(ok_json(r#"{"login":"bot"}"#));
        let client = stub.client("SECRET");
        let user = client.current_user().unwrap();
        assert_eq!(user.login, "bot");
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v1/user");
        assert_eq!(req.header("authorization").as_deref(), Some("token SECRET"));
    }

    #[test]
    fn a_padded_or_trailing_slash_base_url_still_requests_one_slash() {
        // A sub-path instance (`https://example.com/gitea/`) shows the doubling
        // unambiguously in the captured target: untrimmed, the request line reads
        // `/gitea//api/v1/user`.
        let stub = Stub::serve(ok_json(r#"{"login":"bot"}"#));
        Gitea::new(&format!("{}/gitea/", stub.base), "t")
            .current_user()
            .unwrap();
        assert_eq!(stub.captured().path(), "/gitea/api/v1/user");
        // A bare host is not spared — nothing collapses the empty first segment, so
        // untrimmed this sends `GET //api/v1/user`.
        let stub = Stub::serve(ok_json(r#"{"login":"bot"}"#));
        Gitea::new(&format!("{}/", stub.base), "t")
            .current_user()
            .unwrap();
        assert_eq!(stub.captured().path(), "/api/v1/user");
        // Whitespace around a pasted base is not a path problem but a parse one: the
        // padded string is not a URL (here, `127.0.0.1:PORT  ` is an invalid port),
        // so untrimmed this never reaches the socket at all.
        let stub = Stub::serve(ok_json(r#"{"login":"bot"}"#));
        Gitea::new(&format!("  {}  ", stub.base), "t")
            .current_user()
            .unwrap();
        assert_eq!(stub.captured().path(), "/api/v1/user");
    }

    #[test]
    fn list_issues_sends_type_state_and_labels() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client.list_issues(&repo(), "open", "afkd/ready").unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues");
        assert!(req.has_query("type", "issues"));
        assert!(req.has_query("state", "open"));
        assert!(req.has_query("labels", "afkd/ready"));
    }

    #[test]
    fn get_issue_reads_the_index_path() {
        let stub = Stub::serve(ok_json(r#"{"number":5,"title":"T","state":"open"}"#));
        let client = stub.client("t");
        let issue = client.get_issue(&repo(), 5).unwrap();
        assert_eq!(issue.number, 5);
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5");
    }

    #[test]
    fn patch_assignees_patches_the_assignee_body() {
        let stub = Stub::serve(ok_json(
            r#"{"number":5,"title":"T","state":"open","assignees":[{"login":"bot"}]}"#,
        ));
        let client = stub.client("t");
        client
            .patch_assignees(&repo(), 5, &["bot".to_string()])
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PATCH");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5");
        assert!(req.body.contains("assignees"));
        assert!(req.body.contains("bot"));
    }

    #[test]
    fn set_state_patches_the_state_body() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client.set_state(&repo(), 5, "closed").unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "PATCH");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5");
        assert!(req.body.contains("closed"));
    }

    /// The repo's label set, read whole: the id a removal names *and* the
    /// `exclusive` flag the ensure refuses a gate on, over the labels path.
    #[test]
    fn list_labels_reads_the_labels_path() {
        let stub = Stub::serve(ok_json(
            r#"[{"id":3,"name":"afkd/claimed","exclusive":false},
                {"id":4,"name":"afkd/inprogress","exclusive":true},
                {"id":5,"name":"needs-triage"}]"#,
        ));
        let client = stub.client("t");
        let labels = client.list_labels(&repo()).unwrap();
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.id, l.name.as_str(), l.exclusive))
                .collect::<Vec<_>>(),
            vec![
                (3, "afkd/claimed", false),
                (4, "afkd/inprogress", true),
                (5, "needs-triage", false),
            ]
        );
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/labels");
    }

    /// The create carries all three fields Gitea's `CreateLabelOption` reads — and
    /// `exclusive: false` **explicitly**, since that flag is the whole point of
    /// creating the label ourselves.
    #[test]
    fn create_label_posts_name_color_and_exclusive() {
        let stub = Stub::serve(created_json(
            r#"{"id":7,"name":"afkd/claimed","color":"7057ff","exclusive":false}"#,
        ));
        let client = stub.client("t");
        client
            .create_label(&repo(), "afkd/claimed", "#7057ff", false)
            .unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/labels");
        assert!(
            req.body.contains(r#""name":"afkd/claimed""#),
            "{}",
            req.body
        );
        assert!(req.body.contains(r##""color":"#7057ff""##), "{}", req.body);
        assert!(req.body.contains(r#""exclusive":false"#), "{}", req.body);
    }

    /// The POST's reply is the issue's **resulting** label list, and reading it is
    /// what turns Gitea's silent drop into an error: here the repo's other labels
    /// come back but not the requested name — the exact shape Gitea answers for a
    /// name it does not define — and the add is a `Refused`, not a success.
    #[test]
    fn add_label_errors_when_the_response_omits_the_name() {
        let stub = Stub::serve(ok_json(
            r#"[{"id":9,"name":"afkd/ready"},{"id":4,"name":"afkd/inprogress","exclusive":true}]"#,
        ));
        let client = stub.client("t");
        let err = client.add_label(&repo(), 5, "afkd/claimed").unwrap_err();
        assert!(
            matches!(
                err,
                GiteaError::Refused {
                    stage: "add label",
                    ..
                }
            ),
            "a dropped label must be an error, got {err:?}"
        );
        assert!(
            err.to_string().contains("afkd/claimed"),
            "the error names the label: {err}"
        );
        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5/labels");
        assert!(req.body.contains("afkd/claimed"));
    }

    /// The other half of the same contract: a reply that *does* carry the name (among
    /// the issue's other labels) is a success.
    #[test]
    fn add_label_accepts_a_response_that_carries_the_name() {
        let stub = Stub::serve(ok_json(
            r#"[{"id":9,"name":"afkd/ready"},{"id":3,"name":"afkd/claimed","exclusive":false}]"#,
        ));
        let client = stub.client("t");
        client.add_label(&repo(), 5, "afkd/claimed").unwrap();
        let req = stub.captured();
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5/labels");
    }

    #[test]
    fn remove_label_deletes_the_id_path() {
        let stub = Stub::serve(ok_json("{}"));
        let client = stub.client("t");
        client.remove_label(&repo(), 5, "3").unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "DELETE");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5/labels/3");
    }

    #[test]
    fn list_issue_comments_reads_the_comments_path() {
        let stub = Stub::serve(ok_json("[]"));
        let client = stub.client("t");
        client.list_issue_comments(&repo(), 5).unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5/comments");
    }

    /// The wire contract *and* the read-back: the POST still carries the body on
    /// the issue's comments path, and Gitea's `201` + created-comment reply is
    /// decoded into the id and creation time a claim marker needs. The stub answers
    /// a real Gitea comment object — a multi-line, non-ASCII claim body and a
    /// `+02:00` creation stamp against a `Z` update one.
    #[test]
    fn post_comment_posts_the_body_and_returns_the_created_comment() {
        let stub = Stub::serve(created_json(
            r#"{"id":90210,"body":"[afkd-claim] owner=björn-öst[bot]\nheld until 12:00",
                "user":{"login":"björn-öst[bot]"},
                "created_at":"2026-07-20T11:00:00+02:00","updated_at":"2026-07-20T09:00:00Z"}"#,
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
            posted.body,
            "[afkd-claim] owner=björn-öst[bot]\nheld until 12:00"
        );
        // 2026-07-20T09:00:00Z, whichever offset spelled it.
        assert_eq!(
            posted.created_at,
            UNIX_EPOCH + Duration::from_secs(1_784_538_000)
        );
        assert_eq!(posted.created_at, posted.updated_at);

        let req = stub.captured();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/v1/repos/acme/widgets/issues/5/comments");
        assert!(req.body.contains("afkd-claim"));
        assert!(req.body.contains("björn-öst[bot]"));
    }

    /// A reply that is not a comment object (no `id`) is a staged decode error, not
    /// a silently-successful post: a claim built on a fabricated id is worse than a
    /// loud failure.
    #[test]
    fn post_comment_rejects_a_reply_that_is_not_a_comment() {
        let stub = Stub::serve(ok_json(r#"{"message":"rate limited"}"#));
        let client = stub.client("t");
        let err = client.post_comment(&repo(), 5, "hi").unwrap_err();
        assert!(
            matches!(
                err,
                GiteaError::Decode {
                    stage: "post comment",
                    ..
                }
            ),
            "an id-less POST reply must map to a staged Decode error, got {err:?}"
        );
        let _ = stub.captured();
    }

    /// The delete path is **repo**-scoped: `…/issues/comments/{id}` carries no
    /// issue index, unlike the POST that created it.
    #[test]
    fn delete_comment_deletes_the_repo_scoped_comment_path() {
        let stub = Stub::serve(no_content());
        let client = stub.client("t");
        client.delete_comment(&repo(), 90210).unwrap();
        let req = stub.captured();
        assert_eq!(req.method, "DELETE");
        assert_eq!(
            req.path(),
            "/api/v1/repos/acme/widgets/issues/comments/90210"
        );
    }

    /// The renewal's wire contract: a PATCH on the **same** repo-scoped comment path
    /// the DELETE uses, carrying the renewed body. Editing by id is what keeps a
    /// renewal from minting a second marker.
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
            "/api/v1/repos/acme/widgets/issues/comments/90210"
        );
        assert!(req.body.contains("afkd-claim"), "{}", req.body);
        assert!(req.body.contains("renewal=7"), "{}", req.body);
        assert!(req.body.contains("björn-öst[bot]"), "{}", req.body);
    }

    #[test]
    fn org_repos_reads_the_orgs_path() {
        let stub = Stub::serve(ok_json(r#"[{"full_name":"acme/widgets"}]"#));
        let client = stub.client("t");
        let repos = client.org_repos("acme").unwrap();
        assert_eq!(repos.len(), 1);
        let req = stub.captured();
        assert_eq!(req.path(), "/api/v1/orgs/acme/repos");
    }

    #[test]
    fn non_success_status_maps_to_status_error_tagged_with_stage() {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let stub = Stub::serve(resp);
        let client = stub.client("t");
        let err = client.get_issue(&repo(), 9).unwrap_err();
        assert!(matches!(
            err,
            GiteaError::Status {
                stage: "get issue",
                status: 404
            }
        ));
        let _ = stub.captured();
    }

    /// A reply that promises more body bytes (`Content-Length`) than it sends, then closes
    /// the socket: reading it to a string hits a premature EOF. Drives the `body` decode
    /// arm the ordinary `ok_json` shape (an exact length) never reaches.
    fn truncated_body() -> String {
        // Declare 64 body bytes but send only one, then the stub thread drops the stream
        // (EOF). `read_to_string()` reads to the promised length, sees the socket close
        // early, and returns an `io::Error` — mapped to `GiteaError::Decode`.
        "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nContent-Type: application/json\r\n\r\n{"
            .to_string()
    }

    /// A bare `500` with no body — a forge fault mid-request, distinct from the `404`
    /// eligibility miss above. Drives the `?`-propagation early-return of the three
    /// GET-then-parse methods (`list_labels`/`list_issue_comments`).
    fn server_error() -> &'static str {
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
    }

    #[test]
    fn a_truncated_body_maps_to_a_decode_error() {
        // `body` reads the response text BEFORE any parse, so the premature EOF surfaces as
        // `Decode` tagged with the request's stage — not a `Status`/`Transport`.
        let stub = Stub::serve(truncated_body());
        let client = stub.client("t");
        let err = client.current_user().unwrap_err();
        assert!(
            matches!(
                err,
                GiteaError::Decode {
                    stage: "current user",
                    ..
                }
            ),
            "a truncated body must map to a staged Decode error, got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn list_labels_propagates_a_forge_error() {
        let stub = Stub::serve(server_error());
        let client = stub.client("t");
        let err = client.list_labels(&repo()).unwrap_err();
        assert!(
            matches!(
                err,
                GiteaError::Status {
                    stage: "list labels",
                    status: 500
                }
            ),
            "a 500 on the labels GET must propagate out of list_labels, got {err:?}"
        );
        let _ = stub.captured();
    }

    #[test]
    fn list_issue_comments_propagates_a_forge_error() {
        let stub = Stub::serve(server_error());
        let client = stub.client("t");
        let err = client.list_issue_comments(&repo(), 5).unwrap_err();
        assert!(
            matches!(
                err,
                GiteaError::Status {
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
        // A transport failure must not fold the token into the logged error. Point
        // a client (with a recognizable token) at a dead port and assert the secret
        // appears nowhere in the resulting error, Display included.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let token = "SUPERSECRET-PAT-9f3a";
        let client = Gitea::new(&format!("http://127.0.0.1:{port}"), token);
        let err = client.current_user().unwrap_err();
        let GiteaError::Transport { reason, .. } = &err else {
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
