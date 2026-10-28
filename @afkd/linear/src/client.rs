//! The real [`Tracker`]: the Linear GraphQL mapping behind the issue-tracker seam.
//!
//! Every request is one named operation out of `operations.graphql`, sent by its name
//! ([`operation`]), so a document that file does not hold cannot be sent. Every value that
//! varies rides as a variable, built by a pure `…_vars` function, and every reply is read
//! by a pure `parse_*` function: both are unit-tested with no network, and the documents
//! and the variables are held to Linear's published schema (`tests/schema.graphql`) by the
//! schema tests below. The HTTP mapping itself is exercised against a loopback `Stub`.
//!
//! Linear answers a request it refuses with an `errors` body, on a 200 as well as on a 4xx,
//! so every reply is read for `errors` before its `data` ([`graphql_data`]).
//!
//! A file goes up as Linear's upload guide has it: `FileUpload` signs an upload url, and the
//! bytes are PUT there with the headers Linear returned and never the credential. A file
//! comes back with the credential, from Linear's file host ([`UPLOADS_BASE`]) or the API
//! root and from no other.

use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use serde_json::{json, Value};

use crate::auth::Auth;
use crate::file::{Upload, MAX_FILE_BYTES};
use crate::http::{Http, Reply, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use crate::rfc3339::parse_rfc3339_millis;
use crate::settings::MemberRef;
use crate::tracker::{
    Blocker, Child, Comment, Issue, LinearError, Page, Related, State, Team, Tracker,
};

/// Linear's API root; GraphQL is at [`GRAPHQL_PATH`] under it.
pub(crate) const LINEAR_BASE: &str = "https://api.linear.app";

/// Where the GraphQL endpoint sits under the API root.
pub(crate) const GRAPHQL_PATH: &str = "/graphql";

/// Where Linear keeps the files put on its issues, each fetched with the credential.
pub(crate) const UPLOADS_BASE: &str = "https://uploads.linear.app";

/// The `Cache-Control` an upload's PUT carries, as Linear's upload guide writes it.
const UPLOAD_CACHE_CONTROL: &str = "public, max-age=31536000";

/// How many issues one `Issues` page holds: small, with its nested connections, to keep a
/// query far under Linear's complexity cap.
pub(crate) const ISSUES_PAGE: u32 = 25;

/// The operations the plugin sends, as written.
const OPERATIONS: &str = include_str!("operations.graphql");

/// The message Linear's error starts with when what a request names does not exist.
const NOT_FOUND: &str = "Entity not found";

/// Every operation in `operations.graphql`, by name, in the file's order: each runs from a
/// line starting `query ` or `mutation ` to the next one, and the comment lines before the
/// first are none of them.
pub(crate) fn operations() -> &'static [(&'static str, &'static str)] {
    static PARSED: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    PARSED.get_or_init(|| {
        let starts: Vec<usize> = OPERATIONS
            .match_indices('\n')
            .map(|(at, _)| at + 1)
            .filter(|&at| {
                let line = &OPERATIONS[at..];
                line.starts_with("query ") || line.starts_with("mutation ")
            })
            .collect();
        starts
            .iter()
            .enumerate()
            .map(|(n, &start)| {
                let end = starts.get(n + 1).copied().unwrap_or(OPERATIONS.len());
                let document = OPERATIONS[start..end].trim();
                let name = document
                    .split(|c: char| c.is_whitespace() || c == '(' || c == '{')
                    .nth(1)
                    .unwrap_or_default();
                (name, document)
            })
            .collect()
    })
}

/// The document of the operation named `name`.
pub(crate) fn operation(name: &str) -> Option<&'static str> {
    operations()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, document)| *document)
}

// --- the variables each operation is sent with ---

/// `TeamByKey`: the team whose key is `key`.
pub(crate) fn team_by_key_vars(key: &str) -> Value {
    json!({ "filter": { "key": { "eq": key } } })
}

/// `States`: the team's workflow states.
pub(crate) fn states_vars(team_id: &str) -> Value {
    json!({ "teamId": team_id })
}

/// `Issues`: one page of a state's issues in the team's manual order, from `after`.
pub(crate) fn issues_in_state_vars(team_id: &str, state_id: &str, after: Option<&str>) -> Value {
    json!({
        "filter": { "team": { "id": { "eq": team_id } }, "state": { "id": { "eq": state_id } } },
        "sort": [{ "manual": { "order": "Ascending" } }],
        "first": ISSUES_PAGE,
        "after": after,
    })
}

/// `Issues`: one page of the team's issues carrying the label `label`, from `after`.
pub(crate) fn parked_vars(team_id: &str, label: &str, after: Option<&str>) -> Value {
    json!({
        "filter": {
            "team": { "id": { "eq": team_id } },
            "labels": { "some": { "name": { "eq": label } } },
        },
        "sort": [{ "manual": { "order": "Ascending" } }],
        "first": ISSUES_PAGE,
        "after": after,
    })
}

/// `Issues`: the last issue in a state's manual order, which a put-back goes below.
pub(crate) fn last_in_state_vars(team_id: &str, state_id: &str) -> Value {
    json!({
        "filter": { "team": { "id": { "eq": team_id } }, "state": { "id": { "eq": state_id } } },
        "sort": [{ "manual": { "order": "Descending" } }],
        "first": 1,
    })
}

/// `IssueRelations`: the page of an issue's inverse relations after the cursor `after`.
pub(crate) fn relations_vars(issue_id: &str, after: &str) -> Value {
    json!({ "id": issue_id, "after": after })
}

/// `IssueDescription`: the issue's description.
pub(crate) fn description_vars(issue_id: &str) -> Value {
    json!({ "id": issue_id })
}

/// `IssueChildren`: one page of an issue's sub-issues, from `after`.
pub(crate) fn children_vars(issue_id: &str, after: Option<&str>) -> Value {
    json!({ "id": issue_id, "after": after })
}

/// `IssueComments`: one page of an issue's comments, from `after`.
pub(crate) fn comments_vars(issue_id: &str, after: Option<&str>) -> Value {
    json!({ "id": issue_id, "after": after })
}

/// `Users`: the users whose display name or email is `name`.
pub(crate) fn users_vars(name: &str) -> Value {
    json!({ "filter": { "or": [{ "displayName": { "eq": name } }, { "email": { "eq": name } }] } })
}

/// `Labels`: the labels named `name`, the team's and the workspace's.
pub(crate) fn labels_vars(name: &str) -> Value {
    json!({ "filter": { "name": { "eq": name } } })
}

/// `CommentCreate`: `body` as a new comment on the issue.
pub(crate) fn comment_create_vars(issue_id: &str, body: &str) -> Value {
    json!({ "input": { "issueId": issue_id, "body": body } })
}

/// `CommentUpdate`: the comment's body rewritten to `body`.
pub(crate) fn comment_update_vars(comment_id: &str, body: &str) -> Value {
    json!({ "id": comment_id, "input": { "body": body } })
}

/// `CommentDelete`: the comment.
pub(crate) fn comment_delete_vars(comment_id: &str) -> Value {
    json!({ "id": comment_id })
}

/// `IssueAddLabel` and `IssueRemoveLabel`: the issue and the label.
pub(crate) fn issue_label_vars(issue_id: &str, label_id: &str) -> Value {
    json!({ "id": issue_id, "labelId": label_id })
}

/// `IssueLabelCreate`: a label named `name` for the team.
pub(crate) fn label_create_vars(name: &str, team_id: &str) -> Value {
    json!({ "input": { "name": name, "teamId": team_id } })
}

/// `IssueUpdate`: the issue moved to the state, at `sort_order` in its manual order.
pub(crate) fn move_vars(issue_id: &str, state_id: &str, sort_order: f64) -> Value {
    json!({ "id": issue_id, "input": { "stateId": state_id, "sortOrder": sort_order } })
}

/// `IssueUpdate`: the issue retitled `title`.
pub(crate) fn title_vars(issue_id: &str, title: &str) -> Value {
    json!({ "id": issue_id, "input": { "title": title } })
}

/// `IssueUpdate`: the issue's description replaced with `text`.
pub(crate) fn description_update_vars(issue_id: &str, text: &str) -> Value {
    json!({ "id": issue_id, "input": { "description": text } })
}

/// `IssueUpdate`: the issue moved to the state, its place there Linear's to pick.
pub(crate) fn state_vars(issue_id: &str, state_id: &str) -> Value {
    json!({ "id": issue_id, "input": { "stateId": state_id } })
}

/// `IssueCreate`: a new issue in the team titled `title`, and only what else is given: the
/// description when it is not empty, the parent, the state.
pub(crate) fn issue_create_vars(
    team_id: &str,
    parent_id: Option<&str>,
    title: &str,
    description: &str,
    state_id: Option<&str>,
) -> Value {
    let mut input = json!({ "teamId": team_id, "title": title });
    if !description.is_empty() {
        input["description"] = json!(description);
    }
    if let Some(parent_id) = parent_id {
        input["parentId"] = json!(parent_id);
    }
    if let Some(state_id) = state_id {
        input["stateId"] = json!(state_id);
    }
    json!({ "input": input })
}

/// `IssueArchive`: the issue.
pub(crate) fn archive_vars(issue_id: &str) -> Value {
    json!({ "id": issue_id })
}

/// `FileUpload`: a signed upload for a file named `filename` of `size` bytes and the type
/// `content_type`. Private, Linear's default, since `makePublic` is left out.
pub(crate) fn file_upload_vars(content_type: &str, filename: &str, size: usize) -> Value {
    json!({ "contentType": content_type, "filename": filename, "size": size })
}

// --- the client ---

/// A [`Tracker`] backed by Linear's GraphQL API.
pub(crate) struct LinearClient {
    /// The spine, holding the API root and the timeout budgets.
    http: Http,
    /// The credential, shared with the outbox worker's client so an app fetches one token.
    auth: Arc<Auth>,
    /// The API root, its trailing `/` dropped: a file under it is Linear's too.
    base: String,
    /// The viewer's id, fetched once: the credential is fixed for the client's life.
    viewer: OnceLock<String>,
}

impl LinearClient {
    /// A client pointed at `base` — the `base_url` setting, which defaults to Linear's —
    /// sending `auth`, with the default connect/read timeouts.
    pub(crate) fn new(base: &str, auth: Arc<Auth>) -> Self {
        Self {
            http: Http::new(base, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT),
            auth,
            base: base.trim_end_matches('/').to_string(),
            viewer: OnceLock::new(),
        }
    }

    /// Send the operation named `name` with `variables`, and return its `data`. A request
    /// an app's token is refused for is sent once more with a fresh token; a second refusal
    /// is the caller's, and so is a key's first.
    fn post_graphql(
        &self,
        stage: &'static str,
        name: &str,
        variables: Value,
    ) -> Result<Value, LinearError> {
        let Some(query) = operation(name) else {
            return Err(LinearError::Decode {
                stage,
                reason: format!("operations.graphql holds no operation {name}"),
            });
        };
        let body =
            json!({ "operationName": name, "query": query, "variables": variables }).to_string();
        let send = || {
            let authorization = self.auth.header(&self.http, Instant::now())?;
            let reply = self.http.post(
                stage,
                GRAPHQL_PATH,
                "application/json",
                Some(&authorization),
                body.as_bytes(),
            )?;
            graphql_data(stage, &reply)
        };
        match send() {
            Err(e) if refused_credential(&e) && self.auth.invalidate() => send(),
            result => result,
        }
    }

    /// One page of an `Issues` read, each issue's blockers read whole: an issue whose inverse
    /// relations run past the page `Issues` reads them in has the rest read
    /// (`IssueRelations`) until the last, so a blocker behind other relations still holds it
    /// back.
    fn issue_page(&self, stage: &'static str, vars: Value) -> Result<Page<Issue>, LinearError> {
        let page = parse_issue_page(stage, &self.post_graphql(stage, "Issues", vars)?)?;
        let items = page
            .items
            .into_iter()
            .map(|(mut issue, mut after)| {
                while let Some(cursor) = after {
                    let vars = relations_vars(&issue.id, &cursor);
                    let data = self.post_graphql(stage, "IssueRelations", vars)?;
                    let connection = &data["issue"]["inverseRelations"];
                    issue.blockers.extend(blockers(stage, connection)?);
                    after = next_cursor(connection);
                }
                Ok(issue)
            })
            .collect::<Result<_, LinearError>>()?;
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// Every page of an `Issues` read, its variables built by `vars` from a cursor.
    fn all_issues(
        &self,
        stage: &'static str,
        vars: impl Fn(Option<&str>) -> Value,
    ) -> Result<Vec<Issue>, LinearError> {
        let mut issues = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.issue_page(stage, vars(after.as_deref()))?;
            issues.extend(page.items);
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(issues),
            }
        }
    }

    /// The id of the label named `name` for `team_id`: the team's own, else the workspace's
    /// (a label with no team), else `None`.
    fn find_label(
        &self,
        stage: &'static str,
        team_id: &str,
        name: &str,
    ) -> Result<Option<String>, LinearError> {
        let labels = parse_labels(
            stage,
            &self.post_graphql(stage, "Labels", labels_vars(name))?,
        )?;
        let pick = |team: Option<&str>| {
            labels
                .iter()
                .find(|(_, t)| t.as_deref() == team)
                .map(|(id, _)| id.clone())
        };
        Ok(pick(Some(team_id)).or_else(|| pick(None)))
    }
}

/// Whether `e` is Linear refusing the credential itself: a 401, or an `authentication
/// error`.
fn refused_credential(e: &LinearError) -> bool {
    match e {
        LinearError::Status { status, .. } => *status == 401,
        LinearError::Refused { status, kind, .. } => {
            *status == 401 || kind == "authentication error"
        }
        _ => false,
    }
}

impl Tracker for LinearClient {
    fn viewer(&self) -> Result<String, LinearError> {
        if let Some(id) = self.viewer.get() {
            return Ok(id.clone());
        }
        let stage = "read viewer";
        let data = self.post_graphql(stage, "Viewer", json!({}))?;
        let id = str_at(stage, &data["viewer"], "id")?;
        Ok(self.viewer.get_or_init(|| id).clone())
    }

    fn resolve_team(&self, key: &str) -> Result<Team, LinearError> {
        let stage = "resolve team";
        let data = self.post_graphql(stage, "TeamByKey", team_by_key_vars(key))?;
        parse_teams(stage, &data)?
            .into_iter()
            .find(|team| team.key == key)
            .ok_or_else(|| LinearError::NoSuch {
                stage,
                what: format!("team with the key '{key}'"),
            })
    }

    fn states(&self, team_id: &str) -> Result<Vec<State>, LinearError> {
        let stage = "read states";
        parse_states(
            stage,
            &self.post_graphql(stage, "States", states_vars(team_id))?,
        )
    }

    fn issues_in_state(
        &self,
        team_id: &str,
        state_id: &str,
        after: Option<&str>,
    ) -> Result<Page<Issue>, LinearError> {
        self.issue_page(
            "read issues",
            issues_in_state_vars(team_id, state_id, after),
        )
    }

    fn parked_issues(&self, team_id: &str, label: &str) -> Result<Vec<Issue>, LinearError> {
        self.all_issues("read parked issues", |after| {
            parked_vars(team_id, label, after)
        })
    }

    fn issue_comments(&self, issue_id: &str) -> Result<Vec<Comment>, LinearError> {
        let stage = "read comments";
        let mut comments = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let vars = comments_vars(issue_id, after.as_deref());
            let page =
                parse_comment_page(stage, &self.post_graphql(stage, "IssueComments", vars)?)?;
            comments.extend(page.items);
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(comments),
            }
        }
    }

    fn post_comment(&self, issue_id: &str, text: &str) -> Result<Comment, LinearError> {
        let stage = "post comment";
        let data =
            self.post_graphql(stage, "CommentCreate", comment_create_vars(issue_id, text))?;
        parse_comment(stage, &data["commentCreate"]["comment"])
    }

    fn edit_comment(&self, comment_id: &str, text: &str) -> Result<(), LinearError> {
        let stage = "edit comment";
        self.post_graphql(
            stage,
            "CommentUpdate",
            comment_update_vars(comment_id, text),
        )
        .map(drop)
    }

    fn delete_comment(&self, comment_id: &str) -> Result<(), LinearError> {
        let stage = "delete comment";
        self.post_graphql(stage, "CommentDelete", comment_delete_vars(comment_id))
            .map(drop)
    }

    fn resolve_user(&self, who: &MemberRef) -> Result<String, LinearError> {
        let stage = "resolve user";
        let name = match who {
            MemberRef::Me => return self.viewer(),
            MemberRef::Named(name) => name,
        };
        let users = parse_user_ids(stage, &self.post_graphql(stage, "Users", users_vars(name))?)?;
        match users.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(LinearError::NoSuch {
                stage,
                what: format!("user whose display name or email is '{name}'"),
            }),
            _ => Err(LinearError::NoSuch {
                stage,
                what: format!(
                    "one user whose display name or email is '{name}': more than one is; \
                     name them by email"
                ),
            }),
        }
    }

    fn add_label(&self, team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError> {
        let stage = "add label";
        let label_id = match self.find_label(stage, team_id, name)? {
            Some(id) => id,
            None => {
                let data =
                    self.post_graphql(stage, "IssueLabelCreate", label_create_vars(name, team_id))?;
                str_at(stage, &data["issueLabelCreate"]["issueLabel"], "id")?
            }
        };
        self.post_graphql(
            stage,
            "IssueAddLabel",
            issue_label_vars(issue_id, &label_id),
        )
        .map(drop)
    }

    fn remove_label(&self, team_id: &str, issue_id: &str, name: &str) -> Result<(), LinearError> {
        let stage = "remove label";
        let Some(label_id) = self.find_label(stage, team_id, name)? else {
            return Ok(());
        };
        self.post_graphql(
            stage,
            "IssueRemoveLabel",
            issue_label_vars(issue_id, &label_id),
        )
        .map(drop)
    }

    fn move_to_bottom(
        &self,
        team_id: &str,
        issue_id: &str,
        state_id: &str,
    ) -> Result<(), LinearError> {
        let stage = "move issue";
        let last = parse_issue_page(
            stage,
            &self.post_graphql(stage, "Issues", last_in_state_vars(team_id, state_id))?,
        )?;
        let bottom = last
            .items
            .first()
            .map_or(0.0, |(issue, _)| issue.sort_order);
        self.post_graphql(
            stage,
            "IssueUpdate",
            move_vars(issue_id, state_id, bottom + 1000.0),
        )
        .map(drop)
    }

    fn issue_description(&self, issue_id: &str) -> Result<String, LinearError> {
        let stage = "read description";
        let data = self.post_graphql(stage, "IssueDescription", description_vars(issue_id))?;
        Ok(data["issue"]
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    fn sub_issues(&self, issue_id: &str) -> Result<Vec<Child>, LinearError> {
        let stage = "read sub-issues";
        let mut children = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let vars = children_vars(issue_id, after.as_deref());
            let data = self.post_graphql(stage, "IssueChildren", vars)?;
            let connection = &data["issue"]["children"];
            children.extend(parse_children(stage, connection)?);
            match next_cursor(connection) {
                Some(next) => after = Some(next),
                None => return Ok(children),
            }
        }
    }

    fn set_title(&self, issue_id: &str, title: &str) -> Result<(), LinearError> {
        self.post_graphql("rename issue", "IssueUpdate", title_vars(issue_id, title))
            .map(drop)
    }

    fn set_description(&self, issue_id: &str, text: &str) -> Result<(), LinearError> {
        let vars = description_update_vars(issue_id, text);
        self.post_graphql("describe issue", "IssueUpdate", vars)
            .map(drop)
    }

    fn set_state(&self, issue_id: &str, state_id: &str) -> Result<(), LinearError> {
        self.post_graphql("move issue", "IssueUpdate", state_vars(issue_id, state_id))
            .map(drop)
    }

    fn create_issue(
        &self,
        team_id: &str,
        parent_id: Option<&str>,
        title: &str,
        description: &str,
        state_id: Option<&str>,
    ) -> Result<String, LinearError> {
        let stage = "create issue";
        let vars = issue_create_vars(team_id, parent_id, title, description, state_id);
        let data = self.post_graphql(stage, "IssueCreate", vars)?;
        str_at(stage, &data["issueCreate"]["issue"], "identifier")
    }

    fn archive_issue(&self, issue_id: &str) -> Result<(), LinearError> {
        self.post_graphql("archive issue", "IssueArchive", archive_vars(issue_id))
            .map(drop)
    }

    fn upload_file(&self, upload: &Upload) -> Result<String, LinearError> {
        let stage = "upload file";
        let vars = file_upload_vars(&upload.content_type, &upload.name, upload.bytes.len());
        let signed = parse_upload(stage, &self.post_graphql(stage, "FileUpload", vars)?)?;
        let headers = put_headers(&upload.content_type, signed.headers);
        let reply = self
            .http
            .put(stage, &signed.upload_url, &headers, &upload.bytes)?;
        if !(200..300).contains(&reply.status) {
            return Err(LinearError::Status {
                stage,
                status: reply.status,
                retry_after: reply.retry_after,
            });
        }
        Ok(signed.asset_url)
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, LinearError> {
        let stage = "fetch file";
        if !self.is_linear_file(url) {
            return Err(LinearError::NoSuch {
                stage,
                what: format!("file Linear hosts at {url}"),
            });
        }
        let send = || {
            let authorization = self.auth.header(&self.http, Instant::now())?;
            let reply = self
                .http
                .get_bytes(stage, url, Some(&authorization), MAX_FILE_BYTES)?;
            if (200..300).contains(&reply.status) {
                Ok(reply.body)
            } else {
                Err(LinearError::Status {
                    stage,
                    status: reply.status,
                    retry_after: reply.retry_after,
                })
            }
        };
        match send() {
            Err(e) if refused_credential(&e) && self.auth.invalidate() => send(),
            result => result,
        }
    }

    fn is_linear_file(&self, url: &str) -> bool {
        [UPLOADS_BASE, self.base.as_str()].iter().any(|root| {
            url.strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    fn authorization(&self) -> Result<String, LinearError> {
        self.auth.header(&self.http, Instant::now())
    }

    fn set_call_deadline(&self, deadline: Option<Instant>) {
        self.http.set_deadline(deadline)
    }
}

// --- reading the replies ---

/// Read a GraphQL reply: its `data`, or why there is none. An `errors` body is read on any
/// status, the first error speaking for the rest: Linear's `Entity not found` is
/// [`LinearError::NotFound`], any other a [`LinearError::Refused`] keeping its
/// `extensions.type` (else `extensions.code`), lowercased. A non-success status with no
/// `errors` is a [`LinearError::Status`].
pub(crate) fn graphql_data(stage: &'static str, reply: &Reply) -> Result<Value, LinearError> {
    let ok = (200..300).contains(&reply.status);
    let body: Value = match serde_json::from_str(&reply.body) {
        Ok(body) => body,
        Err(e) if ok => {
            return Err(LinearError::Decode {
                stage,
                reason: e.to_string(),
            })
        }
        Err(_) => Value::Null,
    };
    if let Some(first) = body
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
    {
        let message = first
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("an error with no message")
            .to_string();
        if message.starts_with(NOT_FOUND) {
            return Err(LinearError::NotFound { stage, message });
        }
        let extension = |key: &str| first.get("extensions")?.get(key)?.as_str();
        let kind = extension("type")
            .or_else(|| extension("code"))
            .unwrap_or_default()
            .to_lowercase();
        return Err(LinearError::Refused {
            stage,
            status: reply.status,
            message,
            kind,
            retry_after: reply.retry_after,
        });
    }
    if !ok {
        return Err(LinearError::Status {
            stage,
            status: reply.status,
            retry_after: reply.retry_after,
        });
    }
    match body.get("data") {
        Some(data) if data.is_object() => Ok(data.clone()),
        _ => Err(LinearError::Decode {
            stage,
            reason: "the reply carries no data".to_string(),
        }),
    }
}

/// What `FileUpload` signs: where to PUT the bytes, the headers to PUT them with, and the
/// asset url the file is linked at once they are there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignedUpload {
    pub(crate) upload_url: String,
    pub(crate) asset_url: String,
    pub(crate) headers: Vec<(String, String)>,
}

/// The signed upload of a `FileUpload` reply. Linear answers one it could not prepare with
/// `success: false` or no `uploadFile`, which is its refusal.
pub(crate) fn parse_upload(stage: &'static str, data: &Value) -> Result<SignedUpload, LinearError> {
    let payload = &data["fileUpload"];
    let file = &payload["uploadFile"];
    if payload["success"] != true || !file.is_object() {
        return Err(LinearError::Refused {
            stage,
            status: 200,
            message: "Linear could not prepare the upload".to_string(),
            kind: String::new(),
            retry_after: None,
        });
    }
    let headers = file
        .get("headers")
        .and_then(Value::as_array)
        .ok_or_else(|| LinearError::Decode {
            stage,
            reason: "an upload carries no `headers`".to_string(),
        })?
        .iter()
        .map(|h| Ok((str_at(stage, h, "key")?, str_at(stage, h, "value")?)))
        .collect::<Result<_, LinearError>>()?;
    Ok(SignedUpload {
        upload_url: str_at(stage, file, "uploadUrl")?,
        asset_url: str_at(stage, file, "assetUrl")?,
        headers,
    })
}

/// The headers an upload's PUT carries: its `Content-Type` and the `Cache-Control` Linear's
/// guide asks for, then every header Linear signed, each of those standing in for one of the
/// first two of the same name. No `Authorization`: the url is signed, and the credential is
/// not the storage's.
pub(crate) fn put_headers(
    content_type: &str,
    signed: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), content_type.to_string()),
        (
            "Cache-Control".to_string(),
            UPLOAD_CACHE_CONTROL.to_string(),
        ),
    ];
    for (name, value) in signed {
        headers.retain(|(kept, _)| !kept.eq_ignore_ascii_case(&name));
        headers.push((name, value));
    }
    headers
}

/// The string at `key` of `value`, or a decode error naming the key.
fn str_at(stage: &'static str, value: &Value, key: &str) -> Result<String, LinearError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| LinearError::Decode {
            stage,
            reason: format!("no string `{key}`"),
        })
}

/// The `nodes` array of the connection at `value`, or a decode error.
fn nodes<'a>(stage: &'static str, value: &'a Value) -> Result<&'a Vec<Value>, LinearError> {
    value
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| LinearError::Decode {
            stage,
            reason: "a connection carries no `nodes`".to_string(),
        })
}

/// The cursor of the page after the connection at `value`, `None` on its last page.
fn next_cursor(value: &Value) -> Option<String> {
    let page = value.get("pageInfo")?;
    if page.get("hasNextPage").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    page.get("endCursor")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The teams of a `TeamByKey` reply.
pub(crate) fn parse_teams(stage: &'static str, data: &Value) -> Result<Vec<Team>, LinearError> {
    nodes(stage, &data["teams"])?
        .iter()
        .map(|t| {
            Ok(Team {
                id: str_at(stage, t, "id")?,
                key: str_at(stage, t, "key")?,
            })
        })
        .collect()
}

/// The workflow states of a `States` reply.
pub(crate) fn parse_states(stage: &'static str, data: &Value) -> Result<Vec<State>, LinearError> {
    nodes(stage, &data["team"]["states"])?
        .iter()
        .map(|s| parse_state(stage, s))
        .collect()
}

fn parse_state(stage: &'static str, s: &Value) -> Result<State, LinearError> {
    Ok(State {
        id: str_at(stage, s, "id")?,
        name: str_at(stage, s, "name")?,
        kind: str_at(stage, s, "type")?,
    })
}

/// One page of an `Issues` reply: each issue, and the cursor of its inverse relations' next
/// page, `None` when `Issues` read them all.
pub(crate) fn parse_issue_page(
    stage: &'static str,
    data: &Value,
) -> Result<Page<(Issue, Option<String>)>, LinearError> {
    let connection = &data["issues"];
    Ok(Page {
        items: nodes(stage, connection)?
            .iter()
            .map(|i| Ok((parse_issue(stage, i)?, next_cursor(&i["inverseRelations"]))))
            .collect::<Result<_, LinearError>>()?,
        next: next_cursor(connection),
    })
}

/// The blockers among a connection of inverse relations: only `blocks` relations are kept;
/// `related`, `similar` and `duplicate` never hold an issue back.
fn blockers(stage: &'static str, connection: &Value) -> Result<Vec<Blocker>, LinearError> {
    nodes(stage, connection)?
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("blocks"))
        .map(|r| {
            Ok(Blocker {
                identifier: str_at(stage, &r["issue"], "identifier")?,
                state_kind: str_at(stage, &r["issue"]["state"], "type")?,
            })
        })
        .collect()
}

/// The sub-issues of a connection of them, as `Issues` and `IssueChildren` select them.
fn parse_children(stage: &'static str, connection: &Value) -> Result<Vec<Child>, LinearError> {
    nodes(stage, connection)?
        .iter()
        .map(|c| {
            Ok(Child {
                id: str_at(stage, c, "id")?,
                identifier: str_at(stage, c, "identifier")?,
                title: str_at(stage, c, "title")?,
                state: str_at(stage, &c["state"], "name")?,
            })
        })
        .collect()
}

/// One issue node, as `Issues` selects it, its blockers those on the page of inverse
/// relations it carries ([`blockers`]).
pub(crate) fn parse_issue(stage: &'static str, i: &Value) -> Result<Issue, LinearError> {
    let names = |connection: &Value| -> Result<Vec<String>, LinearError> {
        nodes(stage, connection)?
            .iter()
            .map(|n| str_at(stage, n, "name"))
            .collect()
    };
    let id_of = |user: &Value| user.get("id").and_then(Value::as_str).map(str::to_string);
    let parent = match &i["parent"] {
        Value::Null => None,
        p => Some(Related {
            identifier: str_at(stage, p, "identifier")?,
            title: str_at(stage, p, "title")?,
        }),
    };
    Ok(Issue {
        id: str_at(stage, i, "id")?,
        identifier: str_at(stage, i, "identifier")?,
        title: str_at(stage, i, "title")?,
        description: i
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: str_at(stage, i, "url")?,
        state: parse_state(stage, &i["state"])?,
        labels: names(&i["labels"])?,
        assignee: id_of(&i["assignee"]),
        delegate: id_of(&i["delegate"]),
        parent,
        children: parse_children(stage, &i["children"])?,
        blockers: blockers(stage, &i["inverseRelations"])?,
        sort_order: i.get("sortOrder").and_then(Value::as_f64).ok_or_else(|| {
            LinearError::Decode {
                stage,
                reason: "no number `sortOrder`".to_string(),
            }
        })?,
    })
}

/// One page of an `IssueComments` reply.
pub(crate) fn parse_comment_page(
    stage: &'static str,
    data: &Value,
) -> Result<Page<Comment>, LinearError> {
    let connection = &data["issue"]["comments"];
    Ok(Page {
        items: nodes(stage, connection)?
            .iter()
            .map(|c| parse_comment(stage, c))
            .collect::<Result<_, _>>()?,
        next: next_cursor(connection),
    })
}

/// One comment node. Its author is whoever Linear names: the user, else the bot actor an
/// integration or an app without a user posts as, else the external user, else no one; and
/// the name shown is that author's own, the user's full name before their display name.
pub(crate) fn parse_comment(stage: &'static str, c: &Value) -> Result<Comment, LinearError> {
    let text = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    let (author, author_name) = [
        (&c["user"], ["name", "displayName"]),
        (&c["botActor"], ["name", "name"]),
        (&c["externalUser"], ["name", "name"]),
    ]
    .into_iter()
    .find_map(|(who, names)| {
        let id = text(who, "id")?;
        let name = text(who, names[0]).or_else(|| text(who, names[1]));
        Some((id.clone(), name.unwrap_or(id)))
    })
    .unwrap_or_default();
    let time = |key: &str| {
        c.get(key)
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_millis)
    };
    let posted_at: SystemTime = time("createdAt").ok_or_else(|| LinearError::Decode {
        stage,
        reason: "a comment carries no readable `createdAt`".to_string(),
    })?;
    Ok(Comment {
        id: str_at(stage, c, "id")?,
        text: str_at(stage, c, "body")?,
        author,
        author_name,
        posted_at,
        renewed_at: time("editedAt").unwrap_or(posted_at),
    })
}

/// The user ids of a `Users` reply.
pub(crate) fn parse_user_ids(
    stage: &'static str,
    data: &Value,
) -> Result<Vec<String>, LinearError> {
    nodes(stage, &data["users"])?
        .iter()
        .map(|u| str_at(stage, u, "id"))
        .collect()
}

/// The labels of a `Labels` reply, each with its team's id, `None` for a workspace label.
pub(crate) fn parse_labels(
    stage: &'static str,
    data: &Value,
) -> Result<Vec<(String, Option<String>)>, LinearError> {
    nodes(stage, &data["issueLabels"])?
        .iter()
        .map(|l| {
            let team = l["team"]
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok((str_at(stage, l, "id")?, team))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdl::Sdl;
    use crate::settings::AuthConfig;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::{Duration, UNIX_EPOCH};

    fn sdl() -> Sdl {
        Sdl::parse(include_str!("../tests/schema.graphql")).expect("the excerpt parses")
    }

    /// Every operation the plugin can send, by name: the whole of `operations.graphql`.
    const NAMES: &[&str] = &[
        "Viewer",
        "TeamByKey",
        "States",
        "Issues",
        "IssueRelations",
        "IssueDescription",
        "IssueChildren",
        "IssueComments",
        "Users",
        "Labels",
        "CommentCreate",
        "CommentUpdate",
        "CommentDelete",
        "IssueAddLabel",
        "IssueRemoveLabel",
        "IssueLabelCreate",
        "IssueUpdate",
        "IssueCreate",
        "IssueArchive",
        "FileUpload",
    ];

    /// Every document conforms to Linear's schema — its selection, its variable types and
    /// their use, every argument and inline literal at every depth — and is named as the
    /// client sends it.
    #[test]
    fn every_operation_conforms_to_linears_schema() {
        let sdl = sdl();
        assert_eq!(
            operations()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            NAMES
        );
        for (name, document) in operations() {
            let checked = sdl
                .check_document(document)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(checked.name, *name);
        }
        assert_eq!(
            operation("Viewer"),
            Some("query Viewer {\n  viewer {\n    id\n  }\n}")
        );
        assert_eq!(operation("Viewr"), None);
    }

    /// Every variables builder, on realistic arguments — wide names, an email, a cursor and
    /// none, a sort order of 0 and of 1e15, a multi-line description and an empty one, an
    /// issue filed with every optional field and with none — sends exactly what its
    /// operation declares, and every operation that declares a variable is reached by one.
    #[test]
    fn every_variables_builder_conforms_to_its_operation() {
        let sdl = sdl();
        let team = "6f5e4d3c-2b1a-4098-8f7e-6d5c4b3a2918";
        let state = "1a2b3c4d-5e6f-4708-9a1b-2c3d4e5f6a7b";
        let issue = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5";
        let cursor = Some("eyJpZCI6IjBmNmMxZDJlIn0=");
        let rows: Vec<(&str, Value)> = vec![
            ("Viewer", json!({})),
            ("TeamByKey", team_by_key_vars("ENG")),
            ("TeamByKey", team_by_key_vars("ÅÄÖ")),
            ("States", states_vars(team)),
            ("Issues", issues_in_state_vars(team, state, None)),
            ("Issues", issues_in_state_vars(team, state, cursor)),
            ("Issues", parked_vars(team, "Awaiting Reply", None)),
            ("Issues", parked_vars(team, "Väntar på svar 🚦", cursor)),
            ("Issues", last_in_state_vars(team, state)),
            ("IssueRelations", relations_vars(issue, "eyJpZCI6InIyMCJ9")),
            ("IssueDescription", description_vars(issue)),
            ("IssueChildren", children_vars(issue, None)),
            ("IssueChildren", children_vars(issue, cursor)),
            ("IssueComments", comments_vars(issue, None)),
            ("IssueComments", comments_vars(issue, cursor)),
            ("Users", users_vars("陳大文")),
            ("Users", users_vars("chen@acme.example")),
            ("Labels", labels_vars("Awaiting Reply")),
            (
                "CommentCreate",
                comment_create_vars(issue, "[afkd-claim] owner=afkd-4242"),
            ),
            (
                "CommentCreate",
                comment_create_vars(issue, "看起来不对 🚨\n\n```\nx\n```"),
            ),
            (
                "CommentUpdate",
                comment_update_vars(issue, "[afkd-claim] owner=a renewal=3"),
            ),
            ("CommentDelete", comment_delete_vars(issue)),
            ("IssueAddLabel", issue_label_vars(issue, state)),
            ("IssueRemoveLabel", issue_label_vars(issue, state)),
            (
                "IssueLabelCreate",
                label_create_vars("Awaiting Reply", team),
            ),
            ("IssueUpdate", move_vars(issue, state, 0.0)),
            ("IssueUpdate", move_vars(issue, state, 1e15)),
            (
                "IssueUpdate",
                title_vars(issue, "修复 the retry storm 🚨 — \"backoff\" resets"),
            ),
            (
                "IssueUpdate",
                description_update_vars(issue, "## 完了 ✅\n\n```rust\nlet x = 1;\n```"),
            ),
            ("IssueUpdate", description_update_vars(issue, "")),
            ("IssueUpdate", state_vars(issue, state)),
            (
                "IssueCreate",
                issue_create_vars(team, None, "Cap the backoff at 30s", "", None),
            ),
            (
                "IssueCreate",
                issue_create_vars(
                    team,
                    Some(issue),
                    "子任务: read Retry-After 🚦",
                    "Linear's header is in seconds.\n\n- [ ] honour it",
                    Some(state),
                ),
            ),
            ("IssueArchive", archive_vars(issue)),
            (
                "FileUpload",
                file_upload_vars("image/png", "screen shot — 修复.png", 10 * 1024 * 1024),
            ),
            (
                "FileUpload",
                file_upload_vars("application/octet-stream", "Makefile", 0),
            ),
        ];
        for (name, vars) in &rows {
            let declared = sdl
                .check_document(operation(name).expect("a known operation"))
                .unwrap()
                .variables;
            sdl.check_variables(&declared, vars)
                .unwrap_or_else(|e| panic!("{name} {vars}: {e}"));
        }
        for (name, document) in operations() {
            let declares = !sdl.check_document(document).unwrap().variables.is_empty();
            assert!(
                !declares || rows.iter().any(|(row, _)| row == name),
                "no builder reaches {name}"
            );
        }
    }

    /// The parked sweep asks for issues with *a* label of the name — the collection filter's
    /// `some` — and a state scan for the state's issues in manual order; both page 25 at a
    /// time. The put-back reads the state's last issue.
    #[test]
    fn the_issue_filters_are_the_ones_the_scan_means() {
        assert_eq!(
            parked_vars("t", "Awaiting Reply", None),
            json!({
                "filter": {"team": {"id": {"eq": "t"}},
                           "labels": {"some": {"name": {"eq": "Awaiting Reply"}}}},
                "sort": [{"manual": {"order": "Ascending"}}],
                "first": 25,
                "after": null,
            })
        );
        assert_eq!(
            issues_in_state_vars("t", "s", Some("c")),
            json!({
                "filter": {"team": {"id": {"eq": "t"}}, "state": {"id": {"eq": "s"}}},
                "sort": [{"manual": {"order": "Ascending"}}],
                "first": 25,
                "after": "c",
            })
        );
        assert_eq!(
            last_in_state_vars("t", "s"),
            json!({
                "filter": {"team": {"id": {"eq": "t"}}, "state": {"id": {"eq": "s"}}},
                "sort": [{"manual": {"order": "Descending"}}],
                "first": 1,
            })
        );
        assert_eq!(
            move_vars("i", "s", 1012.5),
            json!({"id": "i", "input": {"stateId": "s", "sortOrder": 1012.5}})
        );
    }

    /// An issue is filed with only what it is given: no description key for an empty one,
    /// and the parent and the state only when named; each update sends its one field.
    #[test]
    fn a_new_issue_carries_only_what_it_is_given() {
        assert_eq!(
            issue_create_vars("t", None, "Cap it", "", None),
            json!({"input": {"teamId": "t", "title": "Cap it"}})
        );
        assert_eq!(
            issue_create_vars("t", Some("p"), "子任务", "Body\n", Some("s")),
            json!({"input": {"teamId": "t", "title": "子任务", "description": "Body\n",
                             "parentId": "p", "stateId": "s"}})
        );
        assert_eq!(
            title_vars("i", "New"),
            json!({"id": "i", "input": {"title": "New"}})
        );
        assert_eq!(
            description_update_vars("i", ""),
            json!({"id": "i", "input": {"description": ""}})
        );
        assert_eq!(
            state_vars("i", "s"),
            json!({"id": "i", "input": {"stateId": "s"}})
        );
    }

    fn reply(status: u16, retry_after: Option<u64>, body: &str) -> Reply {
        Reply {
            status,
            retry_after: retry_after.map(Duration::from_secs),
            body: body.to_string(),
        }
    }

    /// Linear's refusals, each as it arrives: an `errors` body on a 200 and on a 400 (an
    /// invalid document), a rate limit with its wait, a missing entity, a gateway's HTML,
    /// and a 200 that is not JSON or carries no data.
    #[test]
    fn a_reply_is_read_for_errors_before_its_data() {
        let stage = "read issues";
        let e = graphql_data(
            stage,
            &reply(
                200,
                None,
                r#"{"data":null,"errors":[{"message":"Argument Validation Error","extensions":{"type":"invalid input","code":"INVALID_INPUT","userPresentableMessage":"x"}}]}"#,
            ),
        )
        .unwrap_err();
        assert_eq!(
            e,
            LinearError::Refused {
                stage,
                status: 200,
                message: "Argument Validation Error".into(),
                kind: "invalid input".into(),
                retry_after: None,
            }
        );
        assert!(!e.transient());

        let e = graphql_data(
            stage,
            &reply(
                400,
                None,
                r#"{"errors":[{"message":"Cannot query field \"stateID\" on type \"IssueUpdateInput\".","extensions":{"code":"GRAPHQL_VALIDATION_FAILED","type":"graphql error"}}]}"#,
            ),
        )
        .unwrap_err();
        assert!(!e.transient(), "a refused document is the caller's: {e}");
        assert_eq!(
            e.to_string(),
            "linear read issues: Linear refused it (400): Cannot query field \"stateID\" on type \
             \"IssueUpdateInput\"."
        );

        let e = graphql_data(
            stage,
            &reply(
                429,
                Some(7),
                r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#,
            ),
        )
        .unwrap_err();
        assert!(e.transient());
        assert_eq!(e.retry_after(), Some(Duration::from_secs(7)));
        assert!(matches!(&e, LinearError::Refused { kind, .. } if kind == "ratelimited"));

        assert_eq!(
            graphql_data(
                "delete comment",
                &reply(200, None, r#"{"errors":[{"message":"Entity not found: Comment","extensions":{"type":"invalid input"}}]}"#)
            )
            .unwrap_err(),
            LinearError::NotFound {
                stage: "delete comment",
                message: "Entity not found: Comment".into()
            }
        );
        let e = graphql_data(stage, &reply(502, None, "<html>Bad Gateway</html>")).unwrap_err();
        assert_eq!(
            e,
            LinearError::Status {
                stage,
                status: 502,
                retry_after: None
            }
        );
        assert!(e.transient());
        assert!(matches!(
            graphql_data(stage, &reply(200, None, "<html>")),
            Err(LinearError::Decode { .. })
        ));
        assert!(matches!(
            graphql_data(stage, &reply(200, None, r#"{"data":null}"#)),
            Err(LinearError::Decode { .. })
        ));
        assert_eq!(
            graphql_data(
                stage,
                &reply(200, None, r#"{"data":{"viewer":{"id":"u"}}}"#)
            )
            .unwrap(),
            json!({"viewer": {"id": "u"}})
        );
    }

    /// An issue node as Linear sends one: wide text, a null description, assignee, delegate
    /// and parent, its children's states, and relations of every type, only `blocks` kept.
    #[test]
    fn an_issue_reads_with_its_family_and_only_its_blocking_relations() {
        let data = json!({"issues": {
            "nodes": [{
                "id": "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5",
                "identifier": "ENG-421",
                "title": "修复 the retry storm 🚨 — \"backoff\" resets",
                "description": null,
                "url": "https://linear.app/acme/issue/ENG-421/retry-storm",
                "sortOrder": -1234.5,
                "state": {"id": "s1", "name": "Todo ✏️", "type": "unstarted"},
                "labels": {"nodes": [{"name": "afkd/ready"}, {"name": "Väntar på svar"}]},
                "assignee": null,
                "delegate": {"id": "app-user"},
                "parent": null,
                "children": {"nodes": [
                    {"id": "c-422", "identifier": "ENG-422", "title": "子任务 one",
                     "state": {"name": "Done"}},
                ]},
                "inverseRelations": {"nodes": [
                    {"type": "blocks", "issue": {"identifier": "ENG-399", "state": {"type": "started"}}},
                    {"type": "related", "issue": {"identifier": "ENG-12", "state": {"type": "backlog"}}},
                    {"type": "duplicate", "issue": {"identifier": "ENG-13", "state": {"type": "backlog"}}},
                    {"type": "blocks", "issue": {"identifier": "ENG-398", "state": {"type": "canceled"}}},
                ]},
            }],
            "pageInfo": {"hasNextPage": true, "endCursor": "c2"},
        }});
        let page = parse_issue_page("read issues", &data).unwrap();
        assert_eq!(page.next.as_deref(), Some("c2"));
        let (issue, relations_after) = &page.items[0];
        assert_eq!(*relations_after, None, "no relations page follows");
        assert_eq!(issue.title, "修复 the retry storm 🚨 — \"backoff\" resets");
        assert_eq!(issue.description, "");
        assert_eq!(issue.state.kind, "unstarted");
        assert_eq!(issue.labels, ["afkd/ready", "Väntar på svar"]);
        assert_eq!(
            (issue.assignee.as_deref(), issue.delegate.as_deref()),
            (None, Some("app-user"))
        );
        assert_eq!(issue.parent, None);
        assert_eq!(
            issue.children,
            [Child {
                id: "c-422".into(),
                identifier: "ENG-422".into(),
                title: "子任务 one".into(),
                state: "Done".into()
            }]
        );
        assert_eq!(
            issue.blockers,
            [
                Blocker {
                    identifier: "ENG-399".into(),
                    state_kind: "started".into()
                },
                Blocker {
                    identifier: "ENG-398".into(),
                    state_kind: "canceled".into()
                },
            ]
        );
        assert_eq!(issue.sort_order, -1234.5);

        let mut broken = data.clone();
        broken["issues"]["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("identifier");
        assert_eq!(
            parse_issue_page("read issues", &broken)
                .unwrap_err()
                .to_string(),
            "linear read issues: undecodable response (no string `identifier`)"
        );
        let last =
            json!({"issues": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}});
        assert_eq!(parse_issue_page("read issues", &last).unwrap().next, None);
    }

    /// A comment's author is its user, else its bot actor, else its external user, else
    /// nobody, and its name is that author's own; its times are read to the millisecond.
    #[test]
    fn a_comments_author_is_the_user_the_bot_or_the_external_user() {
        let comment = |extra: Value| {
            let mut c = json!({
                "id": "c1", "body": "看起来不对 🚨", "createdAt": "2026-10-27T09:58:00.087Z",
                "editedAt": null, "user": null, "botActor": null, "externalUser": null,
            });
            for (k, v) in extra.as_object().unwrap() {
                c[k] = v.clone();
            }
            parse_comment("read comments", &c).unwrap()
        };
        let human =
            comment(json!({"user": {"id": "u-chen", "name": "陳大文", "displayName": "chen"}}));
        assert_eq!(
            (human.author.as_str(), human.author_name.as_str()),
            ("u-chen", "陳大文")
        );
        let handle = comment(json!({"user": {"id": "u-phil", "name": " ", "displayName": "phil"}}));
        assert_eq!(handle.author_name, "phil");
        let bot = comment(json!({"botActor": {"id": "bot-afkd", "name": "afkd"}}));
        assert_eq!(
            (bot.author.as_str(), bot.author_name.as_str()),
            ("bot-afkd", "afkd")
        );
        let nameless_bot = comment(json!({"botActor": {"id": null, "name": "Zapier"}}));
        assert_eq!(
            (
                nameless_bot.author.as_str(),
                nameless_bot.author_name.as_str()
            ),
            ("", "")
        );
        let external = comment(json!({"externalUser": {"id": "x-1", "name": "Álvaro (Slack)"}}));
        assert_eq!(external.author, "x-1");
        let posted = UNIX_EPOCH + Duration::from_millis(1_793_095_080_087);
        assert_eq!((human.posted_at, human.renewed_at), (posted, posted));
        let edited = comment(json!({"editedAt": "2026-10-27T10:03:00.500Z"}));
        assert_eq!(
            edited.renewed_at,
            UNIX_EPOCH + Duration::from_millis(1_793_095_380_500)
        );
        let mut undated = json!({"id": "c1", "body": "x", "createdAt": "yesterday"});
        undated["user"] = Value::Null;
        assert!(parse_comment("read comments", &undated).is_err());
    }

    // --- over a loopback stub ---

    /// What the stub saw for one request.
    struct Captured {
        method: String,
        path: String,
        headers: Vec<String>,
        body: String,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<String> {
            let want = format!("{}:", name.to_ascii_lowercase());
            self.headers.iter().find_map(|h| {
                h.to_ascii_lowercase()
                    .strip_prefix(&want)
                    .map(|_| h[want.len()..].trim().to_string())
            })
        }

        fn json(&self) -> Value {
            serde_json::from_str(&self.body).expect("a JSON body")
        }
    }

    /// A stub serving `responses` in order, one per connection, each closed after its reply.
    struct Stub {
        base: String,
        handle: Option<JoinHandle<()>>,
        rx: mpsc::Receiver<Captured>,
    }

    impl Stub {
        fn serve(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("accept");
                    let captured = read_request(&stream);
                    stream.write_all(response.as_bytes()).expect("write reply");
                    tx.send(captured).ok();
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                handle: Some(handle),
                rx,
            }
        }

        fn client(&self, auth: AuthConfig) -> LinearClient {
            LinearClient::new(&self.base, Arc::new(Auth::new(&auth)))
        }

        fn captured(self, n: usize) -> Vec<Captured> {
            let captured = (0..n)
                .map(|_| self.rx.recv().expect("the stub captured a request"))
                .collect();
            if let Some(h) = self.handle {
                let _ = h.join();
            }
            captured
        }
    }

    fn read_request(stream: &std::net::TcpStream) -> Captured {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut start = String::new();
        reader.read_line(&mut start).expect("read start line");
        let method = start.split(' ').next().unwrap_or("").to_string();
        let path = start.split(' ').nth(1).unwrap_or("").to_string();
        let mut headers = Vec::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header");
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = rest.trim().parse().unwrap_or(0);
            }
            headers.push(line);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("read body");
        Captured {
            method,
            path,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    /// A raw HTTP reply with a JSON body, its connection closed.
    fn http(status: u16, extra: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n{extra}\r\n{body}",
            body.len()
        )
    }

    const UNAUTHORIZED: &str = r#"{"errors":[{"message":"Authentication required, not authenticated","extensions":{"type":"authentication error","code":"AUTHENTICATION_ERROR"}}]}"#;

    fn app() -> AuthConfig {
        AuthConfig::App {
            client_id: "app-id-7f3a".into(),
            client_secret: "app secret&=9c1e".into(),
        }
    }

    fn token(n: u32) -> String {
        http(
            200,
            "",
            &format!(
                r#"{{"access_token":"tok-{n}","token_type":"Bearer","expires_in":2591999,"scope":"read write"}}"#
            ),
        )
    }

    /// An API key goes out bare, with the named operation, its document and its variables,
    /// to the GraphQL endpoint.
    #[test]
    fn an_api_key_is_sent_bare_with_the_named_operation() {
        let stub = Stub::serve(vec![http(200, "", r#"{"data":{"viewer":{"id":"u-me"}}}"#)]);
        let client = stub.client(AuthConfig::ApiKey("lin_api_7f3a".into()));
        assert_eq!(client.viewer().unwrap(), "u-me");
        assert_eq!(client.viewer().unwrap(), "u-me", "kept, not asked again");
        let [seen] = <[Captured; 1]>::try_from(stub.captured(1)).ok().unwrap();
        assert_eq!(seen.path, "/graphql");
        assert_eq!(
            seen.header("authorization").as_deref(),
            Some("lin_api_7f3a")
        );
        assert_eq!(
            seen.json(),
            json!({"operationName": "Viewer", "query": operation("Viewer").unwrap(), "variables": {}})
        );
    }

    /// An app trades its id and secret for a token with the `client_credentials` grant and
    /// the fixed scope, form-encoded, and sends it as `Bearer`; the token is kept for the
    /// next request.
    #[test]
    fn an_app_fetches_a_token_and_sends_it_as_bearer() {
        let stub = Stub::serve(vec![
            token(1),
            http(200, "", r#"{"data":{"viewer":{"id":"app-user"}}}"#),
            http(200, "", r#"{"data":{"commentDelete":{"success":true}}}"#),
        ]);
        let client = stub.client(app());
        assert_eq!(client.viewer().unwrap(), "app-user");
        client.delete_comment("c1").unwrap();
        let seen = stub.captured(3);
        assert_eq!(seen[0].path, "/oauth/token");
        assert_eq!(
            seen[0].header("content-type").as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            seen[0].body,
            "grant_type=client_credentials&scope=read%2Cwrite%2Capp%3Aassignable%2Capp%3Amentionable\
             &client_id=app-id-7f3a&client_secret=app+secret%26%3D9c1e"
        );
        assert_eq!(seen[0].header("authorization"), None);
        for request in &seen[1..] {
            assert_eq!(
                request.header("authorization").as_deref(),
                Some("Bearer tok-1")
            );
        }
    }

    /// A token Linear refuses is dropped and fetched again, and the request sent once more;
    /// refused a second time, it is the caller's error. An API key is never retried.
    #[test]
    fn a_refused_token_is_fetched_again_and_the_request_retried_once() {
        let stub = Stub::serve(vec![
            token(1),
            http(401, "", UNAUTHORIZED),
            token(2),
            http(200, "", r#"{"data":{"commentDelete":{"success":true}}}"#),
            http(401, "", UNAUTHORIZED),
            token(3),
            http(401, "", UNAUTHORIZED),
        ]);
        let client = stub.client(app());
        client.delete_comment("c1").unwrap();
        let e = client.delete_comment("c2").unwrap_err();
        assert!(
            matches!(&e, LinearError::Refused { status: 401, .. }),
            "{e}"
        );
        let seen = stub.captured(7);
        let auth: Vec<String> = seen
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    r.path,
                    r.header("authorization").unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(
            auth,
            [
                "/oauth/token ",
                "/graphql Bearer tok-1",
                "/oauth/token ",
                "/graphql Bearer tok-2",
                "/graphql Bearer tok-2",
                "/oauth/token ",
                "/graphql Bearer tok-3",
            ]
        );

        let stub = Stub::serve(vec![http(401, "", UNAUTHORIZED)]);
        let client = stub.client(AuthConfig::ApiKey("lin_api_revoked".into()));
        assert!(client.delete_comment("c1").is_err());
        assert_eq!(stub.captured(1).len(), 1, "a key is asked once");
    }

    /// A document Linear refuses as invalid is the caller's error, never a retry, and so is
    /// the same refusal on a 200; a 429's wait is read off its header.
    #[test]
    fn a_refused_document_is_the_callers_error() {
        let invalid = r#"{"errors":[{"message":"Unknown argument \"frist\" on field \"Issue.comments\".","extensions":{"code":"GRAPHQL_VALIDATION_FAILED","type":"graphql error"}}]}"#;
        let stub = Stub::serve(vec![
            http(400, "", invalid),
            http(200, "", invalid),
            http(
                429,
                "Retry-After: 7\r\n",
                r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#,
            ),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        for status in [400, 200] {
            let e = client.issue_comments("i").unwrap_err();
            assert!(
                matches!(&e, LinearError::Refused { status: s, kind, .. } if *s == status && kind == "graphql error"),
                "{e}"
            );
            assert!(!e.transient());
        }
        let e = client.delete_comment("c").unwrap_err();
        assert!(e.transient());
        assert_eq!(e.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(stub.captured(3).len(), 3, "nothing retried inline");
    }

    /// A user is resolved by display name or email, exactly one: none, or two, is a refusal
    /// naming the name; `me` is the viewer.
    #[test]
    fn a_user_resolves_to_exactly_one() {
        let users = |ids: &[&str]| {
            let nodes: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
            http(
                200,
                "",
                &json!({"data": {"users": {"nodes": nodes}}}).to_string(),
            )
        };
        let stub = Stub::serve(vec![
            users(&["u-chen"]),
            users(&[]),
            users(&["u-a", "u-b"]),
            http(200, "", r#"{"data":{"viewer":{"id":"u-me"}}}"#),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        let named = |n: &str| MemberRef::Named(n.into());
        assert_eq!(client.resolve_user(&named("陳大文")).unwrap(), "u-chen");
        assert_eq!(
            client
                .resolve_user(&named("nobody@acme.example"))
                .unwrap_err()
                .to_string(),
            "linear resolve user: no user whose display name or email is 'nobody@acme.example'"
        );
        assert_eq!(
            client.resolve_user(&named("alex")).unwrap_err().to_string(),
            "linear resolve user: no one user whose display name or email is 'alex': more \
             than one is; name them by email"
        );
        assert_eq!(client.resolve_user(&MemberRef::Me).unwrap(), "u-me");
        let seen = stub.captured(4);
        assert_eq!(seen[0].json()["variables"], users_vars("陳大文"));
    }

    /// The badge goes on as the team's own label before a workspace one of the same name, a
    /// label is created for the team when there is neither, and taking off a label no one
    /// made is a no-op.
    #[test]
    fn a_label_is_the_teams_then_the_workspaces_then_a_new_one() {
        let labels = |nodes: Value| {
            http(
                200,
                "",
                &json!({"data": {"issueLabels": {"nodes": nodes}}}).to_string(),
            )
        };
        let ok = |field: &str| {
            http(
                200,
                "",
                &json!({"data": {field: {"success": true}}}).to_string(),
            )
        };
        let stub = Stub::serve(vec![
            labels(json!([
                {"id": "l-ws", "team": null},
                {"id": "l-other", "team": {"id": "t-other"}},
                {"id": "l-eng", "team": {"id": "t-eng"}},
            ])),
            ok("issueAddLabel"),
            labels(
                json!([{"id": "l-other", "team": {"id": "t-other"}}, {"id": "l-ws", "team": null}]),
            ),
            ok("issueAddLabel"),
            labels(json!([{"id": "l-other", "team": {"id": "t-other"}}])),
            http(
                200,
                "",
                r#"{"data":{"issueLabelCreate":{"success":true,"issueLabel":{"id":"l-new"}}}}"#,
            ),
            ok("issueAddLabel"),
            labels(json!([])),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        for _ in 0..3 {
            client.add_label("t-eng", "i-1", "Awaiting Reply").unwrap();
        }
        client
            .remove_label("t-eng", "i-1", "Awaiting Reply")
            .unwrap();
        let seen = stub.captured(8);
        let sent: Vec<(String, Value)> = seen
            .iter()
            .map(|r| {
                (
                    r.json()["operationName"].as_str().unwrap().to_string(),
                    r.json()["variables"].clone(),
                )
            })
            .collect();
        assert_eq!(
            sent[1],
            ("IssueAddLabel".into(), issue_label_vars("i-1", "l-eng"))
        );
        assert_eq!(
            sent[3],
            ("IssueAddLabel".into(), issue_label_vars("i-1", "l-ws"))
        );
        assert_eq!(
            sent[5],
            (
                "IssueLabelCreate".into(),
                label_create_vars("Awaiting Reply", "t-eng")
            )
        );
        assert_eq!(
            sent[6],
            ("IssueAddLabel".into(), issue_label_vars("i-1", "l-new"))
        );
        assert_eq!(sent[7].0, "Labels", "a missing label is not taken off");
    }

    /// A put-back goes below the state's last issue, and to 1000 in a state that is empty.
    #[test]
    fn a_put_back_goes_below_the_states_last_issue() {
        let last = |nodes: Value| {
            http(200, "", &json!({"data": {"issues": {"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}}).to_string())
        };
        let node = json!({
            "id": "i-9", "identifier": "ENG-9", "title": "t", "description": "", "url": "u",
            "sortOrder": 12.5, "state": {"id": "s", "name": "Todo", "type": "unstarted"},
            "labels": {"nodes": []}, "assignee": null, "delegate": null, "parent": null,
            "children": {"nodes": []}, "inverseRelations": {"nodes": []},
        });
        let ok = http(200, "", r#"{"data":{"issueUpdate":{"success":true}}}"#);
        let stub = Stub::serve(vec![last(json!([node])), ok.clone(), last(json!([])), ok]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        client.move_to_bottom("t", "i-1", "s").unwrap();
        client.move_to_bottom("t", "i-1", "s").unwrap();
        let seen = stub.captured(4);
        assert_eq!(seen[0].json()["variables"], last_in_state_vars("t", "s"));
        assert_eq!(seen[1].json()["variables"], move_vars("i-1", "s", 1012.5));
        assert_eq!(seen[3].json()["variables"], move_vars("i-1", "s", 1000.0));
    }

    /// Comments are read to the last page, each page's cursor sent with the next.
    #[test]
    fn comments_are_read_to_the_last_page() {
        let page = |ids: &[&str], next: Option<&str>| {
            let nodes: Vec<Value> = ids
                .iter()
                .map(|id| json!({"id": id, "body": "b", "createdAt": "2026-10-27T09:58:00Z",
                                 "editedAt": null, "user": {"id": "u", "name": "n", "displayName": "d"},
                                 "botActor": null, "externalUser": null}))
                .collect();
            http(
                200,
                "",
                &json!({"data": {"issue": {"comments": {"nodes": nodes,
                "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}}}}})
                .to_string(),
            )
        };
        let stub = Stub::serve(vec![page(&["c1", "c2"], Some("k1")), page(&["c3"], None)]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        let ids: Vec<String> = client
            .issue_comments("i")
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["c1", "c2", "c3"]);
        let seen = stub.captured(2);
        assert_eq!(seen[0].json()["variables"], comments_vars("i", None));
        assert_eq!(seen[1].json()["variables"], comments_vars("i", Some("k1")));
    }

    /// An issue whose inverse relations run past the page `Issues` reads has the rest read
    /// from its cursor to the last page, and only the `blocks` relations among them become
    /// blockers; an issue with no next page costs no read.
    #[test]
    fn a_relations_cursor_is_read_and_only_blocks_are_kept_across_pages() {
        let relation = |kind: &str, identifier: &str, state: &str| {
            json!({"type": kind,
                   "issue": {"identifier": identifier, "state": {"type": state}}})
        };
        let connection = |nodes: Vec<Value>, next: Option<&str>| {
            json!({"nodes": nodes,
                   "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}})
        };
        let node = |id: &str, relations: Value| {
            json!({
                "id": id, "identifier": format!("ENG-{id}"), "title": "修复 🚨", "description": "",
                "url": "u", "sortOrder": 1.0,
                "state": {"id": "s", "name": "Todo", "type": "unstarted"},
                "labels": {"nodes": []}, "assignee": null, "delegate": null, "parent": null,
                "children": {"nodes": []}, "inverseRelations": relations,
            })
        };
        let related: Vec<Value> = (0..20)
            .map(|n| relation("related", &format!("ENG-{}", 300 + n), "started"))
            .collect();
        let issues = json!({"data": {"issues": {
            "nodes": [
                node("1", connection(related, Some("r20"))),
                node("2", connection(vec![relation("blocks", "ENG-9", "completed")], None)),
            ],
            "pageInfo": {"hasNextPage": false, "endCursor": null},
        }}});
        let relations = |nodes: Vec<Value>, next: Option<&str>| {
            http(
                200,
                "",
                &json!({"data": {"issue": {"inverseRelations": connection(nodes, next)}}})
                    .to_string(),
            )
        };
        let stub = Stub::serve(vec![
            http(200, "", &issues.to_string()),
            relations(
                vec![
                    relation("similar", "ENG-400", "backlog"),
                    relation("blocks", "ENG-401", "started"),
                ],
                Some("r22"),
            ),
            relations(vec![relation("blocks", "ENG-402", "canceled")], None),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        let page = client.issues_in_state("t", "s", None).unwrap();
        let blockers: Vec<Vec<&str>> = page
            .items
            .iter()
            .map(|i| i.blockers.iter().map(|b| b.identifier.as_str()).collect())
            .collect();
        assert_eq!(blockers, [vec!["ENG-401", "ENG-402"], vec!["ENG-9"]]);
        assert_eq!(page.items[0].blockers[1].state_kind, "canceled");
        let seen = stub.captured(3);
        assert_eq!(seen[1].json()["operationName"], "IssueRelations");
        assert_eq!(seen[1].json()["variables"], relations_vars("1", "r20"));
        assert_eq!(seen[2].json()["variables"], relations_vars("1", "r22"));
    }

    /// A description is read as Linear holds it, `""` for none; sub-issues are read to the
    /// last page, each with its uuid; a new issue's identifier comes back.
    #[test]
    fn the_reads_and_the_create_read_their_replies() {
        let children = |ids: &[&str], next: Option<&str>| {
            let nodes: Vec<Value> = ids
                .iter()
                .map(|id| {
                    json!({"id": id, "identifier": format!("ENG-{id}"), "title": "子任务 🚦",
                                 "state": {"name": "In Progress"}})
                })
                .collect();
            http(
                200,
                "",
                &json!({"data": {"issue": {"children": {"nodes": nodes,
                    "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}}}}})
                .to_string(),
            )
        };
        let stub = Stub::serve(vec![
            http(
                200,
                "",
                &json!({"data": {"issue": {"description": "## 背景\n\nIt resets."}}}).to_string(),
            ),
            http(200, "", r#"{"data":{"issue":{"description":null}}}"#),
            children(&["7", "8"], Some("k8")),
            children(&["9"], None),
            http(
                200,
                "",
                r#"{"data":{"issueCreate":{"success":true,"issue":{"identifier":"ENG-130"}}}}"#,
            ),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        assert_eq!(
            client.issue_description("i").unwrap(),
            "## 背景\n\nIt resets."
        );
        assert_eq!(client.issue_description("i").unwrap(), "");
        let subs = client.sub_issues("i").unwrap();
        assert_eq!(
            subs.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["7", "8", "9"]
        );
        assert_eq!(subs[2].identifier, "ENG-9");
        assert_eq!(subs[2].state, "In Progress");
        assert_eq!(
            client
                .create_issue("t", Some("i"), "Cap it", "", None)
                .unwrap(),
            "ENG-130"
        );
        let seen = stub.captured(5);
        assert_eq!(seen[3].json()["variables"], children_vars("i", Some("k8")));
        assert_eq!(
            seen[4].json()["variables"],
            issue_create_vars("t", Some("i"), "Cap it", "", None)
        );
    }

    /// A signed upload is read off its reply, every header Linear signed kept in order; an
    /// upload Linear could not prepare — `success: false`, or no `uploadFile` — is its
    /// refusal, and a reply missing a part is undecodable.
    #[test]
    fn a_signed_upload_is_read_or_refused() {
        let file = json!({"uploadUrl": "https://storage.example/signed/1?sig=a%2Bb",
                          "assetUrl": "https://uploads.linear.app/o/1/2",
                          "headers": [{"key": "x-goog-meta-afkd", "value": "t1"},
                                      {"key": "content-type", "value": "image/png"}]});
        assert_eq!(
            parse_upload(
                "upload file",
                &json!({"fileUpload": {"success": true, "uploadFile": file}})
            ),
            Ok(SignedUpload {
                upload_url: "https://storage.example/signed/1?sig=a%2Bb".into(),
                asset_url: "https://uploads.linear.app/o/1/2".into(),
                headers: vec![
                    ("x-goog-meta-afkd".into(), "t1".into()),
                    ("content-type".into(), "image/png".into()),
                ],
            })
        );
        for refused in [
            json!({"fileUpload": {"success": false, "uploadFile": file}}),
            json!({"fileUpload": {"success": true, "uploadFile": null}}),
        ] {
            assert_eq!(
                parse_upload("upload file", &refused)
                    .unwrap_err()
                    .to_string(),
                "linear upload file: Linear refused it (200): Linear could not prepare the upload"
            );
        }
        let mut headless = file.clone();
        headless.as_object_mut().unwrap().remove("headers");
        let e = parse_upload(
            "upload file",
            &json!({"fileUpload": {"success": true, "uploadFile": headless}}),
        )
        .unwrap_err();
        assert!(matches!(e, LinearError::Decode { .. }), "{e}");
        assert_eq!(
            put_headers(
                "image/png",
                vec![
                    ("x-goog-meta-afkd".into(), "t1".into()),
                    ("content-type".into(), "image/png; v=1".into()),
                ]
            ),
            [
                (
                    "Cache-Control".to_string(),
                    "public, max-age=31536000".to_string()
                ),
                ("x-goog-meta-afkd".to_string(), "t1".to_string()),
                ("content-type".to_string(), "image/png; v=1".to_string()),
            ],
            "a signed header stands in for ours of the same name"
        );
    }

    /// A file goes up in two requests: `FileUpload` to the API with the credential, then a
    /// PUT of the bytes to the signed url with its type, the cache header and every header
    /// Linear signed — and no credential. The asset url comes back; a storage that cannot
    /// take the bytes is a status a retry can get past.
    #[test]
    fn a_file_goes_up_signed_and_never_with_the_credential() {
        let storage = Stub::serve(vec![http(200, "", ""), http(503, "Retry-After: 4\r\n", "")]);
        let signed = |n: u32| {
            http(
                200,
                "",
                &json!({"data": {"fileUpload": {"success": true, "uploadFile": {
                    "uploadUrl": format!("{}/signed/{n}?sig=s%2B{n}", storage.base),
                    "assetUrl": format!("https://uploads.linear.app/o/{n}/a"),
                    "headers": [{"key": "x-goog-meta-afkd", "value": format!("tok {n}")}]}}}})
                .to_string(),
            )
        };
        let api = Stub::serve(vec![signed(1), signed(2)]);
        let client = api.client(AuthConfig::ApiKey("lin_api_7f3a".into()));
        let shot = crate::file::upload("screen shot — 修复.png");
        assert_eq!(
            client.upload_file(&shot).unwrap(),
            "https://uploads.linear.app/o/1/a"
        );
        let e = client.upload_file(&shot).unwrap_err();
        assert_eq!(
            e.to_string(),
            "linear upload file: Linear returned status 503"
        );
        assert!(e.transient());
        assert_eq!(e.retry_after(), Some(Duration::from_secs(4)));
        let asked = api.captured(2);
        assert_eq!(asked[0].path, "/graphql");
        assert_eq!(
            asked[0].header("authorization").as_deref(),
            Some("lin_api_7f3a")
        );
        assert_eq!(asked[0].json()["operationName"], "FileUpload");
        assert_eq!(
            asked[0].json()["variables"],
            file_upload_vars("image/png", "screen shot — 修复.png", shot.bytes.len())
        );
        let puts = storage.captured(2);
        for (n, put) in puts.iter().enumerate() {
            assert_eq!(
                (put.method.as_str(), put.path.as_str()),
                (
                    "PUT",
                    format!("/signed/{}?sig=s%2B{}", n + 1, n + 1).as_str()
                )
            );
            assert_eq!(put.header("content-type").as_deref(), Some("image/png"));
            assert_eq!(
                put.header("cache-control").as_deref(),
                Some("public, max-age=31536000")
            );
            assert_eq!(
                put.header("x-goog-meta-afkd"),
                Some(format!("tok {}", n + 1))
            );
            assert_eq!(
                put.header("authorization"),
                None,
                "no credential to storage"
            );
            assert_eq!(put.body, String::from_utf8_lossy(&shot.bytes));
        }
    }

    /// A file is fetched with the credential from Linear's file host or the API root only:
    /// any other url — a lookalike host, plain http, a port that only starts like the
    /// root's, another site — is refused before a request is built. A refused app token is
    /// fetched again and the download sent once more.
    #[test]
    fn a_file_is_fetched_with_the_credential_from_linear_only() {
        let stub = Stub::serve(vec![
            token(1),
            http(401, "", UNAUTHORIZED),
            token(2),
            http(200, "", "\u{0}\r\n--修复 🚨"),
        ]);
        let client = stub.client(app());
        let base = stub.base.clone();
        for url in [
            "https://example.com/x.png".to_string(),
            "http://uploads.linear.app/o/1/a".to_string(),
            "https://uploads.linear.app.evil.example/o/1/a".to_string(),
            "https://uploads.linear.app".to_string(),
            format!("{base}0/uploads/x"),
            String::new(),
        ] {
            assert!(!client.is_linear_file(&url), "{url}");
            assert_eq!(
                client.download(&url).unwrap_err().to_string(),
                format!("linear fetch file: no file Linear hosts at {url}")
            );
        }
        assert!(client.is_linear_file("https://uploads.linear.app/o/1/a"));
        let url = format!("{base}/uploads/o/1/a");
        assert!(client.is_linear_file(&url));
        assert_eq!(
            client.download(&url).unwrap(),
            "\u{0}\r\n--修复 🚨".as_bytes()
        );
        let seen = stub.captured(4);
        let sent: Vec<String> = seen
            .iter()
            .map(|r| {
                format!(
                    "{} {} {}",
                    r.method,
                    r.path,
                    r.header("authorization").unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(
            sent,
            [
                "POST /oauth/token ",
                "GET /uploads/o/1/a Bearer tok-1",
                "POST /oauth/token ",
                "GET /uploads/o/1/a Bearer tok-2",
            ],
            "only the one url was asked for"
        );
    }

    /// A file's body is read to its cap and no further: one byte past it is refused, the
    /// cap itself is taken.
    #[test]
    fn a_files_body_is_read_to_its_cap() {
        let stub = Stub::serve(vec![
            http(200, "", "0123456789abcdefX"),
            http(200, "", "0123456789abcdef"),
        ]);
        let http = Http::new(&stub.base, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT);
        let url = format!("{}/uploads/o/1/a", stub.base);
        assert_eq!(
            http.get_bytes("fetch file", &url, None, 16)
                .unwrap_err()
                .to_string(),
            "linear fetch file: undecodable response (over the 16 bytes a file may hold)"
        );
        assert_eq!(
            http.get_bytes("fetch file", &url, None, 16).unwrap().body,
            b"0123456789abcdef"
        );
        assert_eq!(stub.captured(2).len(), 2);
    }
}
