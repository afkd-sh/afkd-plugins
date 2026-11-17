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
//!
//! The workspace reads ([`crate::reads`]) read whole: a list to its last page, and every list
//! nested in what it holds — an issue's labels, sub-issues, relations both ways, links and
//! subscribers, a project's teams and milestones — past its first page by the documents that
//! read the pages after it ([`ISSUE_CONNECTIONS`]). A client made [`retrying`] tries a request
//! Linear could not take again within the call's deadline. A workspace write
//! ([`crate::writes`]) reads where an issue sits ([`LinearClient::issue_place`]), changes it
//! with one `IssueWrite`, kept apart from the `IssueUpdate` the skill shares, and reads it back
//! as the reads do. A relation or a link it removes is found by its id, which the reads leave
//! out ([`LinearClient::relations`], [`LinearClient::links`]).
//!
//! [`retrying`]: LinearClient::retrying

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::auth::Auth;
use crate::file::{Upload, MAX_FILE_BYTES};
use crate::http::{Http, Reply, DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT};
use crate::rfc3339::parse_rfc3339_millis;
use crate::settings::MemberRef;
use crate::tracker::{
    Blocker, Child, Comment, Issue, IssueDetail, LinearError, Organization, Page, Priority,
    Related, State, StateType, Team, Tracker,
};
use crate::wire::{CommentFields, CycleFields, LabelFields, ProjectFields, TeamFields, UserFields};

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

/// How many issues one `IssueList` page holds at most. Each carries six nested connections of
/// 20, so by Linear's documented scoring (an object 1, a field 0.1, a connection times its
/// `first`) a page of 20 costs some 4 800 points against the 10 000 cap.
pub(crate) const ISSUE_LIST_PAGE: usize = 20;

/// Each connection nested in an issue node `IssueList` and `IssueFull` read 20 of, and the
/// document that reads the pages after it.
const ISSUE_CONNECTIONS: &[(&str, &str)] = &[
    ("labels", "IssueLabelNames"),
    ("children", "IssueChildren"),
    ("relations", "IssueOutRelations"),
    ("inverseRelations", "IssueRelations"),
    ("attachments", "IssueAttachments"),
    ("subscribers", "IssueSubscribers"),
];

/// Each connection nested in a project node `ProjectList` reads 50 of, and the document that
/// reads the pages after it.
const PROJECT_CONNECTIONS: &[(&str, &str)] = &[
    ("teams", "ProjectTeams"),
    ("projectMilestones", "ProjectMilestones"),
];

/// The first wait a [`retrying`](LinearClient::retrying) client takes before it tries again,
/// when Linear named none; each wait after it is twice the one before.
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// What a retry's wait leaves of the call's deadline at the least: the request after it, and
/// the reply, need some of it too.
const RETRY_MARGIN: Duration = Duration::from_secs(2);

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

/// The page after the cursor `after` of a connection nested in the issue or the project
/// `id`: `IssueRelations`, `IssueOutRelations`, `IssueLabelNames`, `IssueAttachments`,
/// `IssueSubscribers`, `IssueChildren`, `ProjectTeams` and `ProjectMilestones`.
pub(crate) fn follow_up_vars(id: &str, after: &str) -> Value {
    json!({ "id": id, "after": after })
}

/// `IssueFull`, `IssuePlace`, `IssueUnarchive` and `IssueDelete`: the issue; `RelationDelete`
/// and `LinkDelete`: the relation or the link; `CommentOne` and `CommentUnresolve`: the
/// comment.
pub(crate) fn issue_vars(issue_id: &str) -> Value {
    json!({ "id": issue_id })
}

/// `IssueThread`: one page of an issue's comments, from `after`.
pub(crate) fn thread_vars(issue_id: &str, after: Option<&str>) -> Value {
    json!({ "id": issue_id, "after": after })
}

/// `IssueList`: one page of `first` issues matching `filter` in the order `sort` gives, from
/// `after`, archived and trashed ones too when `include_archived`.
pub(crate) fn issue_list_vars(
    filter: &Value,
    sort: &Value,
    include_archived: bool,
    first: usize,
    after: Option<&str>,
) -> Value {
    json!({
        "filter": filter,
        "sort": sort,
        "first": first,
        "after": after,
        "includeArchived": include_archived,
    })
}

/// `TeamList` and `UserList`: one page, from `after`.
pub(crate) fn page_vars(after: Option<&str>) -> Value {
    json!({ "after": after })
}

/// `LabelList`: one page of the workspace's labels and, with `team`, that team's, by key.
pub(crate) fn label_list_vars(team: Option<&str>, after: Option<&str>) -> Value {
    let workspace = json!({ "team": { "null": true } });
    let filter = match team {
        Some(key) => json!({ "or": [workspace, { "team": { "key": { "eq": key } } }] }),
        None => workspace,
    };
    json!({ "filter": filter, "after": after })
}

/// `ProjectList`: one page of the projects, every one or, with `team`, those of that team, by
/// key.
pub(crate) fn project_list_vars(team: Option<&str>, after: Option<&str>) -> Value {
    let filter = match team {
        Some(key) => json!({ "accessibleTeams": { "some": { "key": { "eq": key } } } }),
        None => json!({}),
    };
    json!({ "filter": filter, "after": after })
}

/// `CycleList`: one page of the team's cycles, by its id.
pub(crate) fn cycle_list_vars(team_id: &str, after: Option<&str>) -> Value {
    json!({ "filter": { "team": { "id": { "eq": team_id } } }, "after": after })
}

/// `MilestoneList`: one page of the project's milestones, by its id, from `after`.
pub(crate) fn milestone_list_vars(project_id: &str, after: Option<&str>) -> Value {
    json!({ "id": project_id, "after": after })
}

/// `IssueWrite`: the issue changed as `input`, an `IssueUpdateInput`, says: each key it holds
/// set, a `null` one cleared, and every field it leaves out left as it is.
pub(crate) fn issue_write_vars(issue_id: &str, input: &Map<String, Value>) -> Value {
    json!({ "id": issue_id, "input": input })
}

/// `RelationsOut`, `RelationsIn` and `IssueLinks`: one page of the relations the issue holds,
/// of those that name it, or of its links, from `after`.
pub(crate) fn issue_page_vars(issue_id: &str, after: Option<&str>) -> Value {
    json!({ "id": issue_id, "after": after })
}

/// `RelationCreate`: `issue_id` related to `related_id` as Linear's relation `kind`, so a
/// `blocks` one makes the issue `issue_id` a blocker of `related_id`.
pub(crate) fn relation_create_vars(issue_id: &str, related_id: &str, kind: &str) -> Value {
    json!({ "input": { "issueId": issue_id, "relatedIssueId": related_id, "type": kind } })
}

/// `LinkUrl`: `url` linked on the issue, titled `title` when one is given.
pub(crate) fn link_url_vars(issue_id: &str, url: &str, title: Option<&str>) -> Value {
    json!({ "issueId": issue_id, "url": url, "title": title })
}

/// `LinkUpdate`: the link `link_id` titled `title`.
pub(crate) fn link_update_vars(link_id: &str, title: &str) -> Value {
    json!({ "id": link_id, "input": { "title": title } })
}

/// `IssueSubscribe` and `IssueUnsubscribe`: the user `user_id` subscribed to the issue, or no
/// longer.
pub(crate) fn subscribe_vars(issue_id: &str, user_id: &str) -> Value {
    json!({ "id": issue_id, "userId": user_id })
}

/// `CommentPost`: `body` as a new comment on the issue, in the thread of the comment
/// `parent_id` when one is given.
pub(crate) fn comment_post_vars(issue_id: &str, body: &str, parent_id: Option<&str>) -> Value {
    let mut input = json!({ "issueId": issue_id, "body": body });
    if let Some(parent_id) = parent_id {
        input["parentId"] = json!(parent_id);
    }
    json!({ "input": input })
}

/// `CommentResolve`: the thread the comment `comment_id` opens resolved, by the reply
/// `resolving_id` when one is given.
pub(crate) fn comment_resolve_vars(comment_id: &str, resolving_id: Option<&str>) -> Value {
    json!({ "id": comment_id, "resolvingCommentId": resolving_id })
}

/// `IssueDescription`: the issue's description.
pub(crate) fn description_vars(issue_id: &str) -> Value {
    json!({ "id": issue_id })
}

/// `IssueLabelIds`: the ids of the labels on the issue.
pub(crate) fn issue_label_ids_vars(issue_id: &str) -> Value {
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

/// `IssueUpdate`: the issue delegated to the user, or its delegate taken off for `None`.
pub(crate) fn delegate_vars(issue_id: &str, user_id: Option<&str>) -> Value {
    json!({ "id": issue_id, "input": { "delegateId": user_id } })
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
    /// The workspace the credential reaches, read once, for the same reason.
    organization: OnceLock<Organization>,
    /// Whether a request Linear could not take is tried again within the call's deadline.
    retry: bool,
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
            organization: OnceLock::new(),
            retry: false,
        }
    }

    /// The same client, trying a request Linear could not take again: one it rate limited,
    /// answered with a 5xx, or did not answer is sent again after the `Retry-After` it named,
    /// else after a second, then two, doubling. A wait that would leave less than
    /// [`RETRY_MARGIN`] of the call's deadline is not taken, and the last error is the
    /// request's. Outside a call, with no deadline to bound the waits, nothing is retried.
    #[must_use]
    pub(crate) fn retrying(self) -> Self {
        Self {
            retry: true,
            ..self
        }
    }

    /// `attempt`, tried again as [`retrying`](Self::retrying) says when this client retries.
    fn retried<T>(&self, attempt: impl Fn() -> Result<T, LinearError>) -> Result<T, LinearError> {
        let mut wait = FIRST_RETRY_WAIT;
        loop {
            match attempt() {
                Err(e) if self.retry && e.transient() => {
                    let pause = e.retry_after().unwrap_or(wait);
                    let room = self
                        .http
                        .time_left()
                        .is_some_and(|left| left > pause + RETRY_MARGIN);
                    if !room {
                        return Err(e);
                    }
                    std::thread::sleep(pause);
                    wait = wait.saturating_mul(2);
                }
                result => return result,
            }
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
        self.retried(|| match send() {
            Err(e) if refused_credential(&e) && self.auth.invalidate() => send(),
            result => result,
        })
    }

    /// Every node of the connection `field` of the `root` (`issue` or `project`) whose id is
    /// `id`, past the cursor `after`: the pages after the one first read, each read with the
    /// document `name` until the last. Nothing, and no request, for no cursor.
    fn rest_of(
        &self,
        stage: &'static str,
        name: &str,
        root: &str,
        field: &str,
        id: &str,
        mut after: Option<String>,
    ) -> Result<Vec<Value>, LinearError> {
        let mut rest = Vec::new();
        while let Some(cursor) = after {
            let data = self.post_graphql(stage, name, follow_up_vars(id, &cursor))?;
            let connection = &data[root][field];
            rest.extend(nodes(stage, connection)?.iter().cloned());
            after = next_cursor(connection);
        }
        Ok(rest)
    }

    /// Complete each of `connections` nested in `node`, a `root` read with its id: the nodes
    /// [`rest_of`](Self::rest_of) reads appended to the first page's, so the node reads as
    /// though each list came whole.
    fn complete(
        &self,
        stage: &'static str,
        root: &str,
        node: &mut Value,
        connections: &[(&str, &str)],
    ) -> Result<(), LinearError> {
        let id = str_at(stage, node, "id")?;
        for (field, document) in connections {
            let rest =
                self.rest_of(stage, document, root, field, &id, next_cursor(&node[field]))?;
            if let Some(nodes) = node[field].get_mut("nodes").and_then(Value::as_array_mut) {
                nodes.extend(rest);
            }
        }
        Ok(())
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
            .map(|(mut issue, after)| {
                let rest = self.rest_of(
                    stage,
                    "IssueRelations",
                    "issue",
                    "inverseRelations",
                    &issue.id,
                    after,
                )?;
                issue
                    .blockers
                    .extend(blockers(stage, &json!({ "nodes": rest }))?);
                Ok(issue)
            })
            .collect::<Result<_, LinearError>>()?;
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// Every node of a list `name` reads a page at a time, the connection at `field` of each
    /// reply, its variables built by `vars` from a cursor.
    fn all_nodes(
        &self,
        stage: &'static str,
        name: &str,
        field: &str,
        vars: impl Fn(Option<&str>) -> Value,
    ) -> Result<Vec<Value>, LinearError> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data = self.post_graphql(stage, name, vars(after.as_deref()))?;
            let connection = &data[field];
            all.extend(nodes(stage, connection)?.iter().cloned());
            match next_cursor(connection) {
                Some(next) => after = Some(next),
                None => return Ok(all),
            }
        }
    }

    /// The first `limit` issues an `IssueList` read gives, its variables built by `vars` from
    /// the page size and a cursor: read 20 to the page, or what is left of `limit`, each issue
    /// read whole ([`ISSUE_CONNECTIONS`]).
    pub(crate) fn issue_list(
        &self,
        vars: impl Fn(usize, Option<&str>) -> Value,
        limit: usize,
    ) -> Result<Vec<Issue>, LinearError> {
        let stage = "read issues";
        let mut issues = Vec::new();
        let mut after: Option<String> = None;
        while issues.len() < limit {
            let first = (limit - issues.len()).min(ISSUE_LIST_PAGE);
            let data = self.post_graphql(stage, "IssueList", vars(first, after.as_deref()))?;
            let connection = &data["issues"];
            for node in nodes(stage, connection)? {
                let mut node = node.clone();
                self.complete(stage, "issue", &mut node, ISSUE_CONNECTIONS)?;
                issues.push(parse_full_issue(stage, &node)?);
            }
            match next_cursor(connection) {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(issues.into_iter().take(limit).collect())
    }

    /// The thread of issue `issue_id`: its identifier, and every comment on it, as Linear
    /// pages them, each with its uuid.
    pub(crate) fn thread(
        &self,
        issue_id: &str,
    ) -> Result<(String, Vec<(String, CommentFields)>), LinearError> {
        let stage = "read comments";
        let mut identifier: Option<String> = None;
        let mut comments = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let vars = thread_vars(issue_id, after.as_deref());
            let data = self.post_graphql(stage, "IssueThread", vars)?;
            let issue = &data["issue"];
            let id = match &identifier {
                Some(id) => id.clone(),
                None => identifier
                    .insert(str_at(stage, issue, "identifier")?)
                    .clone(),
            };
            let connection = &issue["comments"];
            for node in nodes(stage, connection)? {
                comments.push(parse_thread_comment(stage, node, &id)?);
            }
            match next_cursor(connection) {
                Some(next) => after = Some(next),
                None => return Ok((id, comments)),
            }
        }
    }

    /// The user the credential acts as: its uuid and its handle's fields.
    pub(crate) fn me(&self) -> Result<(String, UserFields), LinearError> {
        let stage = "read viewer";
        parse_user(stage, &self.post_graphql(stage, "Me", json!({}))?["viewer"])
    }

    /// Every user of the workspace Linear lists, apps among them and disabled accounts not.
    pub(crate) fn user_list(&self) -> Result<Vec<(String, UserFields)>, LinearError> {
        let stage = "read users";
        self.all_nodes(stage, "UserList", "users", page_vars)?
            .iter()
            .map(|node| parse_user(stage, node))
            .collect()
    }

    /// Every team the credential can reach: each one's uuid, key and fields.
    pub(crate) fn team_list(&self) -> Result<Vec<(String, String, TeamFields)>, LinearError> {
        let stage = "read teams";
        self.all_nodes(stage, "TeamList", "teams", page_vars)?
            .iter()
            .map(|node| {
                Ok((
                    str_at(stage, node, "id")?,
                    str_at(stage, node, "key")?,
                    TeamFields {
                        name: str_at(stage, node, "name")?,
                    },
                ))
            })
            .collect()
    }

    /// The workspace's labels and, with `team`, that team's, by key.
    pub(crate) fn label_list(
        &self,
        team: Option<&str>,
    ) -> Result<Vec<(String, LabelFields)>, LinearError> {
        let stage = "read labels";
        self.all_nodes(stage, "LabelList", "issueLabels", |after| {
            label_list_vars(team, after)
        })?
        .iter()
        .map(|node| parse_label(stage, node))
        .collect()
    }

    /// Every project or, with `team`, every project of that team, by key: each read whole,
    /// its milestones in their order.
    pub(crate) fn project_list(
        &self,
        team: Option<&str>,
    ) -> Result<Vec<(String, ProjectFields)>, LinearError> {
        let stage = "read projects";
        let mut projects = Vec::new();
        for mut node in self.all_nodes(stage, "ProjectList", "projects", |after| {
            project_list_vars(team, after)
        })? {
            self.complete(stage, "project", &mut node, PROJECT_CONNECTIONS)?;
            projects.push(parse_project(stage, &node)?);
        }
        Ok(projects)
    }

    /// Every cycle of the team whose id is `team_id`.
    pub(crate) fn cycle_list(
        &self,
        team_id: &str,
    ) -> Result<Vec<(String, CycleFields)>, LinearError> {
        let stage = "read cycles";
        self.all_nodes(stage, "CycleList", "cycles", |after| {
            cycle_list_vars(team_id, after)
        })?
        .iter()
        .map(|node| parse_cycle(stage, node))
        .collect()
    }

    /// Where the issue `issue_id` sits: what a write that moves it, places it in its state or
    /// names a milestone of its project starts from.
    pub(crate) fn issue_place(&self, issue_id: &str) -> Result<IssuePlace, LinearError> {
        let stage = "read issue";
        let data = self.post_graphql(stage, "IssuePlace", issue_vars(issue_id))?;
        parse_place(stage, &data["issue"])
    }

    /// The milestones of the project whose id is `project_id`, each one's id and name, in
    /// their order.
    pub(crate) fn milestone_list(
        &self,
        project_id: &str,
    ) -> Result<Vec<(String, String)>, LinearError> {
        let stage = "read milestones";
        let mut milestones = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let vars = milestone_list_vars(project_id, after.as_deref());
            let data = self.post_graphql(stage, "MilestoneList", vars)?;
            let connection = &data["project"]["projectMilestones"];
            for m in nodes(stage, connection)? {
                milestones.push((
                    number_at(stage, m, "sortOrder")?,
                    str_at(stage, m, "id")?,
                    str_at(stage, m, "name")?,
                ));
            }
            match next_cursor(connection) {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        milestones.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(milestones
            .into_iter()
            .map(|(_, id, name)| (id, name))
            .collect())
    }

    /// Change the issue `issue_id` as `input`, an `IssueUpdateInput`, says.
    pub(crate) fn issue_write(
        &self,
        issue_id: &str,
        input: &Map<String, Value>,
    ) -> Result<(), LinearError> {
        let vars = issue_write_vars(issue_id, input);
        self.post_graphql("update issue", "IssueWrite", vars)
            .map(drop)
    }

    /// Every node of the connection `field` of the issue `issue_id`, read a page at a time by
    /// the document `name`.
    fn issue_nodes(
        &self,
        stage: &'static str,
        name: &str,
        field: &str,
        issue_id: &str,
    ) -> Result<Vec<Value>, LinearError> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let vars = issue_page_vars(issue_id, after.as_deref());
            let data = self.post_graphql(stage, name, vars)?;
            let connection = &data["issue"][field];
            all.extend(nodes(stage, connection)?.iter().cloned());
            match next_cursor(connection) {
                Some(next) => after = Some(next),
                None => return Ok(all),
            }
        }
    }

    /// The relations of the issue `issue_id`, the ones it holds when `held`, else the ones
    /// that name it, each with the other issue's id.
    pub(crate) fn relations(
        &self,
        issue_id: &str,
        held: bool,
    ) -> Result<Vec<RelationRow>, LinearError> {
        let stage = "read relations";
        let (name, field, other) = if held {
            ("RelationsOut", "relations", "relatedIssue")
        } else {
            ("RelationsIn", "inverseRelations", "issue")
        };
        self.issue_nodes(stage, name, field, issue_id)?
            .iter()
            .map(|r| {
                Ok(RelationRow {
                    id: str_at(stage, r, "id")?,
                    kind: str_at(stage, r, "type")?,
                    other: str_at(stage, &r[other], "id")?,
                })
            })
            .collect()
    }

    /// Relate the issue `issue_id` to `related_id` as Linear's relation `kind`.
    pub(crate) fn relation_create(
        &self,
        issue_id: &str,
        related_id: &str,
        kind: &str,
    ) -> Result<(), LinearError> {
        let vars = relation_create_vars(issue_id, related_id, kind);
        self.post_graphql("relate issues", "RelationCreate", vars)
            .map(drop)
    }

    /// Remove the relation `relation_id`.
    pub(crate) fn relation_delete(&self, relation_id: &str) -> Result<(), LinearError> {
        self.post_graphql("unrelate issues", "RelationDelete", issue_vars(relation_id))
            .map(drop)
    }

    /// The links of the issue `issue_id`, Linear's attachments.
    pub(crate) fn links(&self, issue_id: &str) -> Result<Vec<Link>, LinearError> {
        let stage = "read links";
        self.issue_nodes(stage, "IssueLinks", "attachments", issue_id)?
            .iter()
            .map(|l| {
                Ok(Link {
                    id: str_at(stage, l, "id")?,
                    url: str_at(stage, l, "url")?,
                })
            })
            .collect()
    }

    /// Link `url` on the issue `issue_id`, titled `title` when one is given.
    pub(crate) fn link_url(
        &self,
        issue_id: &str,
        url: &str,
        title: Option<&str>,
    ) -> Result<(), LinearError> {
        let vars = link_url_vars(issue_id, url, title);
        self.post_graphql("link url", "LinkUrl", vars).map(drop)
    }

    /// Title the link `link_id` `title`.
    pub(crate) fn link_update(&self, link_id: &str, title: &str) -> Result<(), LinearError> {
        let vars = link_update_vars(link_id, title);
        self.post_graphql("update link", "LinkUpdate", vars)
            .map(drop)
    }

    /// Remove the link `link_id`.
    pub(crate) fn link_delete(&self, link_id: &str) -> Result<(), LinearError> {
        self.post_graphql("remove link", "LinkDelete", issue_vars(link_id))
            .map(drop)
    }

    /// Subscribe the user `user_id` to the issue `issue_id`, or, when not `on`, no longer.
    pub(crate) fn subscribe(
        &self,
        issue_id: &str,
        user_id: &str,
        on: bool,
    ) -> Result<(), LinearError> {
        let (stage, name) = if on {
            ("subscribe user", "IssueSubscribe")
        } else {
            ("unsubscribe user", "IssueUnsubscribe")
        };
        self.post_graphql(stage, name, subscribe_vars(issue_id, user_id))
            .map(drop)
    }

    /// Take the issue `issue_id` back out of the archive, or out of the trash.
    pub(crate) fn unarchive_issue(&self, issue_id: &str) -> Result<(), LinearError> {
        self.post_graphql("unarchive issue", "IssueUnarchive", issue_vars(issue_id))
            .map(drop)
    }

    /// Put the issue `issue_id` in the trash.
    pub(crate) fn delete_issue(&self, issue_id: &str) -> Result<(), LinearError> {
        self.post_graphql("delete issue", "IssueDelete", issue_vars(issue_id))
            .map(drop)
    }

    /// The comment `comment_id`, read as `comments` reads it.
    pub(crate) fn comment(&self, comment_id: &str) -> Result<OneComment, LinearError> {
        let stage = "read comment";
        let data = self.post_graphql(stage, "CommentOne", issue_vars(comment_id))?;
        let node = &data["comment"];
        let issue = &node["issue"];
        let (uuid, fields) =
            parse_thread_comment(stage, node, &str_at(stage, issue, "identifier")?)?;
        Ok(OneComment {
            uuid,
            issue: str_at(stage, issue, "id")?,
            fields,
        })
    }

    /// Post `body` as a comment on the issue `issue_id`, in the thread of the comment
    /// `parent_id` when one is given: the new comment's id.
    pub(crate) fn comment_post(
        &self,
        issue_id: &str,
        body: &str,
        parent_id: Option<&str>,
    ) -> Result<String, LinearError> {
        let stage = "post comment";
        let vars = comment_post_vars(issue_id, body, parent_id);
        let data = self.post_graphql(stage, "CommentPost", vars)?;
        str_at(stage, &data["commentCreate"]["comment"], "id")
    }

    /// Resolve the thread the comment `comment_id` opens, by the reply `resolving_id` when one
    /// is given, or, when not `on`, unresolve it.
    pub(crate) fn resolve_comment(
        &self,
        comment_id: &str,
        resolving_id: Option<&str>,
        on: bool,
    ) -> Result<(), LinearError> {
        if on {
            let vars = comment_resolve_vars(comment_id, resolving_id);
            self.post_graphql("resolve comment", "CommentResolve", vars)
        } else {
            self.post_graphql(
                "unresolve comment",
                "CommentUnresolve",
                issue_vars(comment_id),
            )
        }
        .map(drop)
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

    fn organization(&self) -> Result<Organization, LinearError> {
        if let Some(organization) = self.organization.get() {
            return Ok(organization.clone());
        }
        let stage = "read organization";
        let organization =
            parse_organization(stage, &self.post_graphql(stage, "Organization", json!({}))?)?;
        Ok(self.organization.get_or_init(|| organization).clone())
    }

    fn issue_full(&self, issue_id: &str) -> Result<Issue, LinearError> {
        let stage = "read issue";
        let mut node = self.post_graphql(stage, "IssueFull", issue_vars(issue_id))?["issue"].take();
        self.complete(stage, "issue", &mut node, ISSUE_CONNECTIONS)?;
        parse_full_issue(stage, &node)
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
        // Linear refuses to take off a label the issue does not carry.
        let carried = self.post_graphql(stage, "IssueLabelIds", issue_label_ids_vars(issue_id))?;
        if !nodes(stage, &carried["issue"]["labels"])?
            .iter()
            .any(|l| l["id"].as_str() == Some(label_id.as_str()))
        {
            return Ok(());
        }
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

    fn set_delegate(&self, issue_id: &str, user_id: Option<&str>) -> Result<(), LinearError> {
        let stage = if user_id.is_some() {
            "delegate issue"
        } else {
            "undelegate issue"
        };
        self.post_graphql(stage, "IssueUpdate", delegate_vars(issue_id, user_id))
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
        self.retried(|| match send() {
            Err(e) if refused_credential(&e) && self.auth.invalidate() => send(),
            result => result,
        })
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

/// One workflow state node: its `type` one of the seven Linear documents, and its `position`
/// 0 where the read selects none.
fn parse_state(stage: &'static str, s: &Value) -> Result<State, LinearError> {
    let kind = str_at(stage, s, "type")?;
    Ok(State {
        id: str_at(stage, s, "id")?,
        name: str_at(stage, s, "name")?,
        kind: StateType::from_linear(&kind).ok_or_else(|| LinearError::Decode {
            stage,
            reason: format!("a workflow state of a type Linear does not document: {kind:?}"),
        })?,
        position: s.get("position").and_then(Value::as_f64).unwrap_or(0.0),
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
        sort_order: number_at(stage, i, "sortOrder")?,
        detail: IssueDetail::default(),
    })
}

/// The number at `key` of `value`, or a decode error naming the key.
fn number_at(stage: &'static str, value: &Value, key: &str) -> Result<f64, LinearError> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| LinearError::Decode {
            stage,
            reason: format!("no number `{key}`"),
        })
}

/// The string at `key` of `value`, or `None` when it is `null` or not there.
fn opt_str_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The time at `key` of `value`, in unix milliseconds: `None` when it is `null` or not there,
/// and a decode error when it is not a time.
fn opt_millis_at(
    stage: &'static str,
    value: &Value,
    key: &str,
) -> Result<Option<u64>, LinearError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(time) => time
            .as_str()
            .and_then(parse_rfc3339_millis)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| Some(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
            .ok_or_else(|| LinearError::Decode {
                stage,
                reason: format!("no readable time `{key}`"),
            }),
    }
}

/// The time at `key` of `value`, in unix milliseconds, or a decode error naming the key.
fn millis_at(stage: &'static str, value: &Value, key: &str) -> Result<u64, LinearError> {
    opt_millis_at(stage, value, key)?.ok_or_else(|| LinearError::Decode {
        stage,
        reason: format!("no time `{key}`"),
    })
}

/// A whole number Linear sends as a float: an estimate, or a cycle's number.
fn whole(number: f64) -> i64 {
    number.round() as i64
}

/// `items` with each one already among them left out.
fn once_each(items: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        if !kept.contains(&item) {
            kept.push(item);
        }
    }
    kept
}

/// One issue node as `IssueList` and `IssueFull` select it, every nested connection read whole
/// ([`LinearClient::complete`]): the issue, every field of its handle among it.
///
/// Its relations are read both ways. Those it holds give the issues it `blocks`, is related
/// and similar to, and the one it is a duplicate of; those that name it give its blockers, the
/// issues related and similar to it, and those that are duplicates of it. A related or similar
/// issue is listed once, the issue's own relations first.
pub(crate) fn parse_full_issue(stage: &'static str, i: &Value) -> Result<Issue, LinearError> {
    let mut issue = parse_issue(stage, i)?;
    let names = |connection: &Value, key: &str| -> Result<Vec<String>, LinearError> {
        nodes(stage, connection)?
            .iter()
            .map(|n| str_at(stage, n, key))
            .collect()
    };
    let display = |who: &Value| opt_str_at(who, "displayName");
    let priority = number_at(stage, i, "priority")?;
    let mut detail = IssueDetail {
        team: str_at(stage, &i["team"], "key")?,
        branch: str_at(stage, i, "branchName")?,
        priority: Priority::from_linear(priority).ok_or_else(|| LinearError::Decode {
            stage,
            reason: format!("a priority Linear does not have: {priority}"),
        })?,
        estimate: i.get("estimate").and_then(Value::as_f64).map(whole),
        due: opt_str_at(i, "dueDate"),
        assignee_name: display(&i["assignee"]),
        delegate_name: display(&i["delegate"]),
        creator: display(&i["creator"]),
        project: opt_str_at(&i["project"], "name"),
        milestone: opt_str_at(&i["projectMilestone"], "name"),
        cycle: i["cycle"].get("number").and_then(Value::as_f64).map(whole),
        links: names(&i["attachments"], "url")?,
        subscribers: names(&i["subscribers"], "displayName")?,
        created_at: millis_at(stage, i, "createdAt")?,
        updated_at: millis_at(stage, i, "updatedAt")?,
        completed_at: opt_millis_at(stage, i, "completedAt")?,
        canceled_at: opt_millis_at(stage, i, "canceledAt")?,
        archived: opt_millis_at(stage, i, "archivedAt")?.is_some(),
        trashed: i.get("trashed").and_then(Value::as_bool) == Some(true),
        ..IssueDetail::default()
    };
    let (mut related, mut similar) = (Vec::new(), Vec::new());
    for relation in nodes(stage, &i["relations"])? {
        let other = str_at(stage, &relation["relatedIssue"], "identifier")?;
        match relation.get("type").and_then(Value::as_str) {
            Some("blocks") => detail.blocks.push(other),
            Some("related") => related.push(other),
            Some("similar") => similar.push(other),
            Some("duplicate") => {
                detail.duplicate_of.get_or_insert(other);
            }
            _ => {}
        }
    }
    for relation in nodes(stage, &i["inverseRelations"])? {
        let other = str_at(stage, &relation["issue"], "identifier")?;
        match relation.get("type").and_then(Value::as_str) {
            Some("related") => related.push(other),
            Some("similar") => similar.push(other),
            Some("duplicate") => detail.duplicates.push(other),
            _ => {}
        }
    }
    detail.related = once_each(related);
    detail.similar = once_each(similar);
    issue.detail = detail;
    Ok(issue)
}

/// Where an issue sits, as `IssuePlace` reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IssuePlace {
    pub(crate) identifier: String,
    /// Its place in its state's manual order.
    pub(crate) sort_order: f64,
    pub(crate) team: Team,
    pub(crate) state: State,
    /// Its project's id, when it is in one.
    pub(crate) project: Option<String>,
}

/// One relation between two issues, as `RelationsOut` and `RelationsIn` read it: its id,
/// Linear's relation, and the id of the issue at its other end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationRow {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) other: String,
}

/// One comment as `CommentOne` reads it: its uuid, the uuid of the issue it is on, and its
/// handle's fields.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OneComment {
    pub(crate) uuid: String,
    pub(crate) issue: String,
    pub(crate) fields: CommentFields,
}

/// One link on an issue, Linear's attachment, as `IssueLinks` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub(crate) id: String,
    pub(crate) url: String,
}

/// The issue node of an `IssuePlace` reply.
pub(crate) fn parse_place(stage: &'static str, i: &Value) -> Result<IssuePlace, LinearError> {
    Ok(IssuePlace {
        identifier: str_at(stage, i, "identifier")?,
        sort_order: number_at(stage, i, "sortOrder")?,
        team: Team {
            id: str_at(stage, &i["team"], "id")?,
            key: str_at(stage, &i["team"], "key")?,
        },
        state: parse_state(stage, &i["state"])?,
        project: opt_str_at(&i["project"], "id"),
    })
}

/// The workspace of an `Organization` reply.
pub(crate) fn parse_organization(
    stage: &'static str,
    data: &Value,
) -> Result<Organization, LinearError> {
    let organization = &data["organization"];
    Ok(Organization {
        id: str_at(stage, organization, "id")?,
        url_key: str_at(stage, organization, "urlKey")?,
    })
}

/// One comment node as `IssueThread` selects it, on the issue whose identifier is `issue`:
/// its uuid, and its handle's fields. Its author is the user's display name, else the name
/// of the integration or the external user that wrote it, else no one.
pub(crate) fn parse_thread_comment(
    stage: &'static str,
    c: &Value,
    issue: &str,
) -> Result<(String, CommentFields), LinearError> {
    let author = opt_str_at(&c["user"], "displayName")
        .or_else(|| opt_str_at(&c["botActor"], "name"))
        .or_else(|| opt_str_at(&c["externalUser"], "name"));
    Ok((
        str_at(stage, c, "id")?,
        CommentFields {
            issue: issue.to_string(),
            body: str_at(stage, c, "body")?,
            url: str_at(stage, c, "url")?,
            author,
            reply_to: opt_str_at(&c["parent"], "id"),
            resolved_at: opt_millis_at(stage, c, "resolvedAt")?,
            created_at: millis_at(stage, c, "createdAt")?,
            edited_at: opt_millis_at(stage, c, "editedAt")?,
        },
    ))
}

/// One user node as `Me` and `UserList` select it: its uuid, and its handle's fields.
pub(crate) fn parse_user(
    stage: &'static str,
    u: &Value,
) -> Result<(String, UserFields), LinearError> {
    Ok((
        str_at(stage, u, "id")?,
        UserFields {
            name: str_at(stage, u, "displayName")?,
            full_name: str_at(stage, u, "name")?,
            email: str_at(stage, u, "email")?,
            app: u.get("app").and_then(Value::as_bool) == Some(true),
        },
    ))
}

/// One label node as `LabelList` selects it: its uuid, and its handle's fields, its group the
/// label it sits under.
pub(crate) fn parse_label(
    stage: &'static str,
    l: &Value,
) -> Result<(String, LabelFields), LinearError> {
    Ok((
        str_at(stage, l, "id")?,
        LabelFields {
            name: str_at(stage, l, "name")?,
            group: opt_str_at(&l["parent"], "name"),
            team: opt_str_at(&l["team"], "key"),
        },
    ))
}

/// One project node as `ProjectList` selects it, its teams and milestones read whole: its
/// uuid, and its handle's fields, the milestones in their order.
pub(crate) fn parse_project(
    stage: &'static str,
    p: &Value,
) -> Result<(String, ProjectFields), LinearError> {
    let teams = nodes(stage, &p["teams"])?
        .iter()
        .map(|t| str_at(stage, t, "key"))
        .collect::<Result<_, _>>()?;
    let mut milestones = nodes(stage, &p["projectMilestones"])?
        .iter()
        .map(|m| Ok((number_at(stage, m, "sortOrder")?, str_at(stage, m, "name")?)))
        .collect::<Result<Vec<_>, LinearError>>()?;
    milestones.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok((
        str_at(stage, p, "id")?,
        ProjectFields {
            name: str_at(stage, p, "name")?,
            teams,
            milestones: milestones.into_iter().map(|(_, name)| name).collect(),
            url: str_at(stage, p, "url")?,
        },
    ))
}

/// One cycle node as `CycleList` selects it: its uuid, and its handle's fields.
pub(crate) fn parse_cycle(
    stage: &'static str,
    c: &Value,
) -> Result<(String, CycleFields), LinearError> {
    Ok((
        str_at(stage, c, "id")?,
        CycleFields {
            number: whole(number_at(stage, c, "number")?),
            name: opt_str_at(c, "name"),
            team: str_at(stage, &c["team"], "key")?,
            starts_at: millis_at(stage, c, "startsAt")?,
            ends_at: millis_at(stage, c, "endsAt")?,
            active: c.get("isActive").and_then(Value::as_bool) == Some(true),
        },
    ))
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
        "IssueLabelIds",
        "IssueChildren",
        "IssueComments",
        "Users",
        "Labels",
        "Organization",
        "Me",
        "TeamList",
        "UserList",
        "LabelList",
        "ProjectList",
        "ProjectTeams",
        "ProjectMilestones",
        "CycleList",
        "IssueList",
        "IssueFull",
        "IssueThread",
        "IssueLabelNames",
        "IssueOutRelations",
        "IssueAttachments",
        "IssueSubscribers",
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
        "IssuePlace",
        "MilestoneList",
        "IssueWrite",
        "RelationsOut",
        "RelationsIn",
        "RelationCreate",
        "RelationDelete",
        "IssueLinks",
        "LinkUrl",
        "LinkUpdate",
        "LinkDelete",
        "IssueSubscribe",
        "IssueUnsubscribe",
        "IssueUnarchive",
        "IssueDelete",
        "CommentOne",
        "CommentPost",
        "CommentResolve",
        "CommentUnresolve",
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
        // Every filter `issues` takes as the read sends them, and every key of its order, at
        // most three to a request as Linear takes them.
        let query = crate::reads::issue_query(
            json!({
                "id": ["ENG-12", issue], "team": ["ENG", "ÅÄÖ"], "state": ["Todo 🚦"],
                "state_type": ["canceled", "started"], "priority": ["urgent", "none"],
                "labels": ["afkd/ready", "見出し"], "without_labels": ["Hold"],
                "assignee": ["me", "none", "chen@acme.example"], "delegate": ["陳大文"],
                "project": ["Trackers"], "milestone": ["B"], "cycle": [3], "parent": ["ENG-1"],
                "blocked": false, "text": "Retry \"storm\"",
                "created_after": 1_793_000_000_000_i64, "created_before": 1_793_000_000_000_i64,
                "updated_after": 0, "updated_before": 0, "completed_after": 0,
                "completed_before": 0, "canceled_after": 0, "canceled_before": 0,
                "due_after": "2026-01-01", "due_before": "2026-12-31",
            })
            .as_object()
            .unwrap(),
        )
        .unwrap();
        let every_filter = crate::reads::issue_filter(&query);
        let mut rows: Vec<(&str, Value)> = vec![
            ("Viewer", json!({})),
            ("TeamByKey", team_by_key_vars("ENG")),
            ("TeamByKey", team_by_key_vars("ÅÄÖ")),
            ("States", states_vars(team)),
            ("Issues", issues_in_state_vars(team, state, None)),
            ("Issues", issues_in_state_vars(team, state, cursor)),
            ("Issues", parked_vars(team, "Needs Info", None)),
            ("Issues", parked_vars(team, "Väntar på svar 🚦", cursor)),
            ("Issues", last_in_state_vars(team, state)),
            ("IssueRelations", follow_up_vars(issue, "eyJpZCI6InIyMCJ9")),
            ("Organization", json!({})),
            ("Me", json!({})),
            ("TeamList", page_vars(None)),
            ("TeamList", page_vars(cursor)),
            ("UserList", page_vars(cursor)),
            ("LabelList", label_list_vars(None, None)),
            ("LabelList", label_list_vars(Some("ÅÄÖ"), cursor)),
            ("ProjectList", project_list_vars(None, None)),
            ("ProjectList", project_list_vars(Some("ENG"), cursor)),
            ("ProjectTeams", follow_up_vars(issue, "p50")),
            ("ProjectMilestones", follow_up_vars(issue, "p50")),
            ("CycleList", cycle_list_vars(team, None)),
            ("CycleList", cycle_list_vars(team, cursor)),
            ("IssueFull", issue_vars(issue)),
            ("IssuePlace", issue_vars(issue)),
            ("MilestoneList", milestone_list_vars(team, None)),
            ("MilestoneList", milestone_list_vars(team, cursor)),
            ("IssueThread", thread_vars(issue, None)),
            ("IssueThread", thread_vars(issue, cursor)),
            ("IssueLabelNames", follow_up_vars(issue, "l20")),
            ("IssueOutRelations", follow_up_vars(issue, "r20")),
            ("IssueAttachments", follow_up_vars(issue, "a20")),
            ("IssueSubscribers", follow_up_vars(issue, "s20")),
            (
                "IssueList",
                issue_list_vars(&json!({}), &json!([]), false, 20, None),
            ),
            ("IssueDescription", description_vars(issue)),
            ("IssueLabelIds", issue_label_ids_vars(issue)),
            ("IssueChildren", children_vars(issue, None)),
            ("IssueChildren", children_vars(issue, cursor)),
            ("IssueComments", comments_vars(issue, None)),
            ("IssueComments", comments_vars(issue, cursor)),
            ("Users", users_vars("陳大文")),
            ("Users", users_vars("chen@acme.example")),
            ("Labels", labels_vars("Needs Info")),
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
            ("IssueLabelCreate", label_create_vars("Needs Info", team)),
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
                "IssueUpdate",
                delegate_vars(issue, Some("8e7d6c5b-4a39-4281-b0c9-d8e7f6a5b4c3")),
            ),
            ("IssueUpdate", delegate_vars(issue, None)),
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
        // `update_issue` with every field set, and with every one it can clear cleared.
        let every = crate::writes::IssueInput {
            team: Some(team.into()),
            state: Some(state.into()),
            sort_order: Some(1e15),
            title: Some("修复 the retry storm 🚨 — \"backoff\" resets".into()),
            description: Some("## 完了 ✅\n\n![shot.png](https://uploads.linear.app/x)".into()),
            priority: Some(crate::tracker::Priority::Urgent),
            estimate: crate::writes::Change::Set(0),
            due: crate::writes::Change::Set("2026-12-24".into()),
            assignee: crate::writes::Change::Set(issue.into()),
            delegate: crate::writes::Change::Set(issue.into()),
            project: crate::writes::Change::Set(issue.into()),
            milestone: crate::writes::Change::Set(issue.into()),
            cycle: crate::writes::Change::Set(issue.into()),
            parent: crate::writes::Change::Set(issue.into()),
        };
        let cleared = crate::writes::IssueInput {
            estimate: crate::writes::Change::Clear,
            due: crate::writes::Change::Clear,
            assignee: crate::writes::Change::Clear,
            delegate: crate::writes::Change::Clear,
            project: crate::writes::Change::Clear,
            milestone: crate::writes::Change::Clear,
            cycle: crate::writes::Change::Clear,
            parent: crate::writes::Change::Clear,
            ..crate::writes::IssueInput::default()
        };
        rows.push(("IssueWrite", issue_write_vars(issue, &every.input())));
        rows.push(("IssueWrite", issue_write_vars(issue, &cleared.input())));
        for name in ["RelationsOut", "RelationsIn", "IssueLinks"] {
            rows.push((name, issue_page_vars(issue, None)));
            rows.push((name, issue_page_vars(issue, cursor)));
        }
        for kind in ["blocks", "duplicate", "related", "similar"] {
            rows.push(("RelationCreate", relation_create_vars(issue, state, kind)));
        }
        let url = "https://github.com/acme/widgets/pull/42?tab=files#diff-修复";
        rows.extend([
            ("RelationDelete", issue_vars(state)),
            (
                "LinkUrl",
                link_url_vars(issue, url, Some("PR #42 — \"backoff\" 🚦")),
            ),
            ("LinkUrl", link_url_vars(issue, url, None)),
            ("LinkUpdate", link_update_vars(state, "")),
            ("LinkDelete", issue_vars(state)),
            ("IssueSubscribe", subscribe_vars(issue, team)),
            ("IssueUnsubscribe", subscribe_vars(issue, team)),
            ("IssueUnarchive", issue_vars(issue)),
            ("IssueDelete", issue_vars(issue)),
            ("CommentOne", issue_vars(state)),
            (
                "CommentPost",
                comment_post_vars(
                    issue,
                    "看起来不对 🚨\n\n![shot.png](https://uploads.linear.app/x)",
                    None,
                ),
            ),
            ("CommentPost", comment_post_vars(issue, "", Some(state))),
            ("CommentResolve", comment_resolve_vars(state, None)),
            ("CommentResolve", comment_resolve_vars(state, Some(team))),
            ("CommentUnresolve", issue_vars(state)),
        ]);
        for keys in crate::reads::Sort::ALL.chunks(3) {
            let order = crate::reads::issue_sort(keys);
            rows.push((
                "IssueList",
                issue_list_vars(&every_filter, &order, true, 1, cursor),
            ));
        }
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
            parked_vars("t", "Needs Info", None),
            json!({
                "filter": {"team": {"id": {"eq": "t"}},
                           "labels": {"some": {"name": {"eq": "Needs Info"}}}},
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
        assert_eq!(
            delegate_vars("i", Some("u")),
            json!({"id": "i", "input": {"delegateId": "u"}})
        );
        assert_eq!(
            delegate_vars("i", None),
            json!({"id": "i", "input": {"delegateId": null}})
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
        assert_eq!(issue.state.kind, StateType::Unstarted);
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
            client.add_label("t-eng", "i-1", "Needs Info").unwrap();
        }
        client.remove_label("t-eng", "i-1", "Needs Info").unwrap();
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
                label_create_vars("Needs Info", "t-eng")
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
        assert_eq!(seen[1].json()["variables"], follow_up_vars("1", "r20"));
        assert_eq!(seen[2].json()["variables"], follow_up_vars("1", "r22"));
    }

    /// An issue node as `IssueList` and `IssueFull` select it, every field set: wide text,
    /// a float priority and estimate, Linear's times to the millisecond, each relation type
    /// both ways, a trashed issue (archived too), and a bare one, every optional field
    /// `null` and every list empty.
    fn full_node(rich: bool) -> Value {
        let connection = |nodes: Value| json!({"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}});
        let user = |name: &str| json!({"id": format!("u-{name}"), "displayName": name});
        let out =
            |kind: &str, other: &str| json!({"type": kind, "relatedIssue": {"identifier": other}});
        let inverse = |kind: &str, other: &str, state: &str| json!({"type": kind, "issue": {"identifier": other, "state": {"type": state}}});
        let mut node = json!({
            "id": "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5", "identifier": "ENG-421",
            "title": "修复 the retry storm 🚨 — \"backoff\" resets",
            "description": null, "url": "https://linear.app/acme/issue/ENG-421/retry-storm",
            "branchName": "eng-421-retry-storm", "sortOrder": -12.5, "priority": 0.0,
            "estimate": null, "dueDate": null, "createdAt": "2026-10-27T09:58:00.087Z",
            "updatedAt": "2026-10-27T10:03:00.500Z", "completedAt": null, "canceledAt": null,
            "archivedAt": null, "trashed": null, "team": {"key": "ENG"},
            "state": {"id": "s1", "name": "Todo ✏️", "type": "unstarted"},
            "assignee": null, "delegate": null, "creator": null, "project": null,
            "projectMilestone": null, "cycle": null, "parent": null,
            "labels": connection(json!([])), "children": connection(json!([])),
            "relations": connection(json!([])), "inverseRelations": connection(json!([])),
            "attachments": connection(json!([])), "subscribers": connection(json!([])),
        });
        if rich {
            for (key, value) in [
                ("description", json!("## 背景\n\nIt resets.")),
                ("priority", json!(2.0)),
                ("estimate", json!(3.0)),
                ("dueDate", json!("2026-12-24")),
                ("completedAt", json!("2026-11-01T08:00:00.000Z")),
                ("canceledAt", json!("2026-11-02T08:00:00Z")),
                ("archivedAt", json!("2026-11-03T08:00:00Z")),
                ("trashed", json!(true)),
                ("assignee", user("陳大文")),
                ("delegate", user("vesper")),
                ("creator", json!({"displayName": "phil"})),
                ("project", json!({"name": "Trackers 🧭"})),
                ("projectMilestone", json!({"name": "B — reads"})),
                ("cycle", json!({"number": 12.0})),
                ("parent", json!({"identifier": "ENG-400", "title": "Epic"})),
                (
                    "labels",
                    connection(json!([{"name": "afkd/ready"}, {"name": "Väntar på svar"}])),
                ),
                (
                    "relations",
                    connection(json!([
                        out("blocks", "ENG-500"),
                        out("related", "ENG-12"),
                        out("similar", "ENG-13"),
                        out("duplicate", "ENG-14"),
                        out("duplicate", "ENG-15"),
                        out("related", "ENG-16"),
                    ])),
                ),
                (
                    "inverseRelations",
                    connection(json!([
                        inverse("blocks", "ENG-399", "started"),
                        inverse("related", "ENG-12", "backlog"),
                        inverse("related", "ENG-17", "backlog"),
                        inverse("similar", "ENG-18", "completed"),
                        inverse("duplicate", "ENG-19", "canceled"),
                    ])),
                ),
                (
                    "attachments",
                    connection(json!([{"url": "https://github.com/acme/widgets/pull/42"}])),
                ),
                (
                    "subscribers",
                    connection(json!([{"displayName": "phil"}, {"displayName": "陳大文"}])),
                ),
            ] {
                node[key] = value;
            }
        }
        node
    }

    /// A full issue node reads every field of the handle: the detail, the family and each
    /// relation both ways — its own relations first and each related or similar issue once —
    /// and a bare one reads `None` and empty lists. A priority Linear does not have, a state
    /// type it does not document and a time that is none are undecodable.
    #[test]
    fn a_full_issue_reads_every_field_and_every_relation_both_ways() {
        let issue = parse_full_issue("read issues", &full_node(true)).unwrap();
        assert_eq!(issue.description, "## 背景\n\nIt resets.");
        assert_eq!(issue.labels, ["afkd/ready", "Väntar på svar"]);
        assert_eq!(
            issue.parent.as_ref().map(|p| p.identifier.as_str()),
            Some("ENG-400")
        );
        assert_eq!(
            issue.blockers,
            [Blocker {
                identifier: "ENG-399".into(),
                state_kind: "started".into()
            }]
        );
        let ms = |t: &str| {
            parse_rfc3339_millis(t)
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        assert_eq!(
            issue.detail,
            IssueDetail {
                team: "ENG".into(),
                branch: "eng-421-retry-storm".into(),
                priority: Priority::High,
                estimate: Some(3),
                due: Some("2026-12-24".into()),
                assignee_name: Some("陳大文".into()),
                delegate_name: Some("vesper".into()),
                creator: Some("phil".into()),
                project: Some("Trackers 🧭".into()),
                milestone: Some("B — reads".into()),
                cycle: Some(12),
                blocks: vec!["ENG-500".into()],
                related: vec!["ENG-12".into(), "ENG-16".into(), "ENG-17".into()],
                similar: vec!["ENG-13".into(), "ENG-18".into()],
                duplicates: vec!["ENG-19".into()],
                duplicate_of: Some("ENG-14".into()),
                links: vec!["https://github.com/acme/widgets/pull/42".into()],
                subscribers: vec!["phil".into(), "陳大文".into()],
                created_at: 1_793_095_080_087,
                updated_at: 1_793_095_380_500,
                completed_at: Some(ms("2026-11-01T08:00:00Z")),
                canceled_at: Some(ms("2026-11-02T08:00:00Z")),
                archived: true,
                trashed: true,
            }
        );

        let bare = parse_full_issue("read issues", &full_node(false)).unwrap();
        assert_eq!(bare.description, "");
        assert_eq!(bare.parent, None);
        assert_eq!(
            bare.detail,
            IssueDetail {
                team: "ENG".into(),
                branch: "eng-421-retry-storm".into(),
                created_at: 1_793_095_080_087,
                updated_at: 1_793_095_380_500,
                ..IssueDetail::default()
            }
        );

        for (key, value, reason) in [
            ("priority", json!(5.0), "a priority Linear does not have: 5"),
            (
                "priority",
                json!(1.5),
                "a priority Linear does not have: 1.5",
            ),
            (
                "state",
                json!({"id": "s", "name": "Paused", "type": "paused"}),
                "a workflow state of a type Linear does not document: \"paused\"",
            ),
            (
                "createdAt",
                json!("yesterday"),
                "no readable time `createdAt`",
            ),
            ("updatedAt", Value::Null, "no time `updatedAt`"),
        ] {
            let mut node = full_node(true);
            node[key] = value;
            assert_eq!(
                parse_full_issue("read issues", &node).unwrap_err(),
                LinearError::Decode {
                    stage: "read issues",
                    reason: reason.into()
                },
                "{key}"
            );
        }
        let mut duplicate = full_node(false);
        duplicate["state"] = json!({"id": "s", "name": "Duplicate", "type": "duplicate"});
        assert_eq!(
            parse_full_issue("read issues", &duplicate)
                .unwrap()
                .state
                .kind,
            StateType::Canceled,
            "a duplicate state reads as canceled"
        );
    }

    /// The other nodes a read selects read as their handles' fields: a comment by a user, an
    /// integration and an external user, and one Linear names no author for; a user, an app;
    /// a label in a group and a workspace one; a project, its milestones in their order; and
    /// a cycle with no name.
    #[test]
    fn the_read_nodes_read_as_their_handles_fields() {
        let comment = |who: Value| {
            let mut c = json!({"id": "c1", "body": "看起来不对 🚨\n\n```\nx\n```",
                               "url": "https://linear.app/acme/issue/ENG-7#comment-c1",
                               "createdAt": "2026-10-27T09:58:00.087Z", "editedAt": null,
                               "resolvedAt": "2026-10-27T10:00:00Z", "parent": {"id": "c0"},
                               "user": null, "botActor": null, "externalUser": null});
            for (k, v) in who.as_object().unwrap() {
                c[k] = v.clone();
            }
            parse_thread_comment("read comments", &c, "ENG-7").unwrap()
        };
        let (id, fields) = comment(json!({"user": {"displayName": "chen"}}));
        assert_eq!(id, "c1");
        assert_eq!(
            fields,
            CommentFields {
                issue: "ENG-7".into(),
                body: "看起来不对 🚨\n\n```\nx\n```".into(),
                url: "https://linear.app/acme/issue/ENG-7#comment-c1".into(),
                author: Some("chen".into()),
                reply_to: Some("c0".into()),
                resolved_at: Some(1_793_095_200_000),
                created_at: 1_793_095_080_087,
                edited_at: None,
            }
        );
        assert_eq!(
            comment(json!({"botActor": {"name": "Zapier"}}))
                .1
                .author
                .as_deref(),
            Some("Zapier")
        );
        assert_eq!(
            comment(json!({"externalUser": {"name": "Álvaro (Slack)"}}))
                .1
                .author
                .as_deref(),
            Some("Álvaro (Slack)")
        );
        assert_eq!(comment(json!({})).1.author, None);

        assert_eq!(
            parse_user(
                "read users",
                &json!({"id": "u1", "name": "陳大文", "displayName": "chen",
                        "email": "chen@acme.example", "app": false})
            )
            .unwrap(),
            (
                "u1".to_string(),
                UserFields {
                    name: "chen".into(),
                    full_name: "陳大文".into(),
                    email: "chen@acme.example".into(),
                    app: false
                }
            )
        );
        assert!(
            parse_user(
                "read users",
                &json!({"id": "u2", "name": "afkd", "displayName": "afkd", "email": "a@o.example",
                        "app": true})
            )
            .unwrap()
            .1
            .app
        );
        assert_eq!(
            parse_label(
                "read labels",
                &json!({"id": "l1", "name": "Bug 🐛", "team": {"key": "ENG"},
                        "parent": {"name": "Type"}})
            )
            .unwrap()
            .1,
            LabelFields {
                name: "Bug 🐛".into(),
                group: Some("Type".into()),
                team: Some("ENG".into())
            }
        );
        assert_eq!(
            parse_label(
                "read labels",
                &json!({"id": "l2", "name": "afkd/ready", "team": null, "parent": null})
            )
            .unwrap()
            .1,
            LabelFields {
                name: "afkd/ready".into(),
                group: None,
                team: None
            }
        );
        let connection = |nodes: Value| json!({"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}});
        assert_eq!(
            parse_project(
                "read projects",
                &json!({"id": "p1", "name": "Trackers 🧭",
                "url": "https://linear.app/acme/project/p1",
                "teams": connection(json!([{"key": "ENG"}, {"key": "OPS"}])),
                "projectMilestones": connection(json!([
                    {"name": "C", "sortOrder": 30.0}, {"name": "A", "sortOrder": -1.5},
                    {"name": "B", "sortOrder": 2.0},
                ]))})
            )
            .unwrap()
            .1,
            ProjectFields {
                name: "Trackers 🧭".into(),
                teams: vec!["ENG".into(), "OPS".into()],
                milestones: vec!["A".into(), "B".into(), "C".into()],
                url: "https://linear.app/acme/project/p1".into(),
            }
        );
        assert_eq!(
            parse_cycle(
                "read cycles",
                &json!({"id": "y1", "number": 12.0, "name": null, "team": {"key": "ENG"},
                        "startsAt": "2026-10-26T00:00:00.000Z", "endsAt": "2026-11-09T00:00:00Z",
                        "isActive": true})
            )
            .unwrap()
            .1,
            CycleFields {
                number: 12,
                name: None,
                team: "ENG".into(),
                starts_at: 1_792_972_800_000,
                ends_at: 1_794_182_400_000,
                active: true,
            }
        );
        assert_eq!(
            parse_organization(
                "read organization",
                &json!({"organization": {"id": "o1", "urlKey": "acme"}})
            )
            .unwrap(),
            Organization {
                id: "o1".into(),
                url_key: "acme".into()
            }
        );
    }

    /// A full issue whose nested lists run past the 20 its read selects has each read on from
    /// its cursor to the last page, by the document that reads it, 50 to the page; a list
    /// with no next page costs no read.
    #[test]
    fn a_full_issues_nested_lists_are_read_to_their_last_page() {
        let page = |nodes: Value, next: Option<&str>| json!({"nodes": nodes, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}});
        let mut node = full_node(false);
        node["labels"] = page(json!([{"name": "l1"}]), Some("l20"));
        node["subscribers"] = page(json!([{"displayName": "phil"}]), Some("s20"));
        let reply = |root: &str, field: &str, connection: Value| {
            http(
                200,
                "",
                &json!({"data": {root: {field: connection}}}).to_string(),
            )
        };
        let stub = Stub::serve(vec![
            http(200, "", &json!({"data": {"issue": node}}).to_string()),
            reply(
                "issue",
                "labels",
                page(json!([{"name": "l2"}]), Some("l70")),
            ),
            reply("issue", "labels", page(json!([{"name": "l3 🏷"}]), None)),
            reply(
                "issue",
                "subscribers",
                page(json!([{"displayName": "陳大文"}]), None),
            ),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        let issue = client.issue_full("i-421").unwrap();
        assert_eq!(issue.labels, ["l1", "l2", "l3 🏷"]);
        assert_eq!(issue.detail.subscribers, ["phil", "陳大文"]);
        let seen = stub.captured(4);
        let sent: Vec<(String, Value)> = seen
            .iter()
            .map(|r| {
                (
                    r.json()["operationName"].as_str().unwrap().to_string(),
                    r.json()["variables"].clone(),
                )
            })
            .collect();
        let id = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5";
        assert_eq!(
            sent,
            [
                ("IssueFull".to_string(), issue_vars("i-421")),
                ("IssueLabelNames".to_string(), follow_up_vars(id, "l20")),
                ("IssueLabelNames".to_string(), follow_up_vars(id, "l70")),
                ("IssueSubscribers".to_string(), follow_up_vars(id, "s20")),
            ]
        );
    }

    /// A project whose milestones run past the 50 its list selects has the rest read before
    /// its milestones are ordered.
    #[test]
    fn a_projects_milestones_are_read_to_their_last_page() {
        let page = |nodes: Value, next: Option<&str>| json!({"nodes": nodes, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}});
        let projects = json!({"data": {"projects": page(json!([{
            "id": "p1", "name": "Trackers", "url": "https://linear.app/acme/project/p1",
            "teams": page(json!([{"key": "ENG"}]), None),
            "projectMilestones": page(json!([{"name": "B", "sortOrder": 2.0}]), Some("m50")),
        }]), None)}});
        let stub = Stub::serve(vec![
            http(200, "", &projects.to_string()),
            http(
                200,
                "",
                &json!({"data": {"project": {"projectMilestones":
                    page(json!([{"name": "A", "sortOrder": 1.0}]), None)}}})
                .to_string(),
            ),
        ]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        let projects = client.project_list(Some("ENG")).unwrap();
        assert_eq!(projects[0].1.milestones, ["A", "B"]);
        let seen = stub.captured(2);
        assert_eq!(
            seen[0].json()["variables"],
            project_list_vars(Some("ENG"), None)
        );
        assert_eq!(seen[1].json()["operationName"], "ProjectMilestones");
        assert_eq!(seen[1].json()["variables"], follow_up_vars("p1", "m50"));
    }

    /// A retrying client sends a request Linear rate limited again after the `Retry-After` it
    /// named, within the call's deadline, and answers; one whose wait would pass the deadline
    /// is the request's error at once, unsent again. A client that does not retry, the
    /// kind's own, sends once.
    #[test]
    fn a_retrying_client_waits_out_a_rate_limit_within_the_deadline() {
        let limited = http(
            429,
            "Retry-After: 0\r\n",
            r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#,
        );
        let org = http(
            200,
            "",
            r#"{"data":{"organization":{"id":"o1","urlKey":"acme"}}}"#,
        );
        let stub = Stub::serve(vec![limited.clone(), limited.clone(), org]);
        let client = stub.client(AuthConfig::ApiKey("k".into())).retrying();
        client.set_call_deadline(Some(Instant::now() + Duration::from_secs(30)));
        assert_eq!(client.organization().unwrap().url_key, "acme");
        assert_eq!(stub.captured(3).len(), 3, "two refusals, then the answer");

        let far = http(
            429,
            "Retry-After: 60\r\n",
            r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#,
        );
        let stub = Stub::serve(vec![far]);
        let client = stub.client(AuthConfig::ApiKey("k".into())).retrying();
        client.set_call_deadline(Some(Instant::now() + Duration::from_secs(30)));
        let started = Instant::now();
        let e = client.organization().unwrap_err();
        assert_eq!(e.retry_after(), Some(Duration::from_secs(60)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no wait was taken"
        );
        assert_eq!(stub.captured(1).len(), 1);

        let stub = Stub::serve(vec![limited]);
        let client = stub.client(AuthConfig::ApiKey("k".into()));
        client.set_call_deadline(Some(Instant::now() + Duration::from_secs(30)));
        assert!(client.organization().unwrap_err().transient());
        assert_eq!(stub.captured(1).len(), 1, "the kind's client sends once");
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
