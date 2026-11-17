//! The workspace reads suite: the real `afkd-linear` exec, sent each read as afkd sends one
//! from outside the plugin's own services — a child started for the one call, a `call` with
//! the protocol, an empty settings file and the plugin's directory, and no `hello` — against
//! the stateful fake Linear a `linear.Workspace` reaches. No afkd is involved.
//!
//! Every leg ends with the fake's record of requests that did not conform to Linear's schema
//! empty, and every child is held to the wire by [`once`]: one reply, nothing else on stdout,
//! a clean exit.

// A whole `linear.Issue` handle, written as one `json!`, nests deeper than the default limit.
#![recursion_limit = "256"]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{
    operation_document, png, Author, FakeLinear, API_KEY, CLIENT_ID, GLOBEX_API_KEY,
    GLOBEX_CLIENT_ID,
};
use common::{app_ws, once, ws, TempDir};

/// The moment the seeded issues are filed from, a whole second: 2026-10-26T07:33:20Z.
const T0: u64 = 1_793_000_000_000;
const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

/// The title of the issue every field is set on: wide CJK, an emoji and a quote.
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "The client retries forever.\n\n```rust\nlet backoff = 0;\n```";

/// Two teams of one workspace, `ENG` with a state of each category and `OPS` with two, and
/// the issues every filter and every order is held to, each filed a day after the one before
/// from [`T0`]:
///
/// - ENG-101, ENG Todo: every field set — urgent, estimate 3, due 2026-12-24, phil's,
///   delegated to the app, filed by chen, in the project and its milestone B, cycle 12, two
///   labels, a link, two subscribers, related and similar both ways — and updated last;
/// - ENG-102, ENG Todo: high, estimate 1, due 2026-11-30, chen's, `afkd/ready`, blocked by a
///   done issue;
/// - ENG-103, ENG In Progress: a sub-issue of ENG-101, `Hold`, "seconds" in its description;
/// - ENG-104, ENG Done: low, completed, blocks ENG-102;
/// - ENG-105, ENG Canceled: canceled;
/// - ENG-106, ENG Duplicate: a duplicate of ENG-101;
/// - OPS-107, OPS Todo: medium, in the project, cycle 3, blocks ENG-109;
/// - OPS-108, OPS Done: completed;
/// - ENG-109, ENG Todo: the viewer's, blocked by an open issue, above ENG-102 in the manual
///   order;
/// - ENG-110 and ENG-111, ENG Todo: one archived, one in the trash.
struct World {
    fake: FakeLinear,
    phil: String,
    chen: String,
    trackers: String,
    /// ENG-101 to ENG-106, then OPS-107 and OPS-108, then ENG-109 to ENG-111, by id.
    issues: Vec<String>,
}

impl World {
    /// The id of the issue whose number is `number`.
    fn issue(&self, number: usize) -> &str {
        &self.issues[number - 101]
    }
}

fn world() -> World {
    let fake = FakeLinear::start();
    fake.team("OPS", "Operations — 運用");
    for (state, kind) in [
        ("Triage", "triage"),
        ("Todo", "unstarted"),
        ("In Progress", "started"),
        ("Done", "completed"),
        ("Canceled", "canceled"),
        ("Duplicate", "duplicate"),
    ] {
        fake.state(state, kind);
    }
    for (state, kind) in [("Todo", "unstarted"), ("Done", "completed")] {
        fake.team_state("OPS", state, kind);
    }
    let phil = fake.user("Phil Ek", "phil", "phil@acme.example");
    let chen = fake.user("陳大文", "chen", "chen@acme.example");
    let group = fake.label_of("ENG", "Type", None);
    fake.label_of("ENG", "Bug 🐛", Some(&group));
    fake.label_of("OPS", "Infra", None);
    fake.workspace_label("Needs Info", "#f2c94c");
    let trackers = fake.project(
        "Trackers 🧭",
        &["ENG", "OPS"],
        &[("C — writes", 30.0), ("B — reads", 20.0)],
    );
    let twelve = fake.cycle("ENG", 12, Some("Sprint 12 — 冬"), T0, T0 + 14 * DAY, true);
    let three = fake.cycle("OPS", 3, None, T0, T0 + 7 * DAY, false);
    let issues: Vec<String> = [
        ("ENG", "Todo", TITLE, BODY),
        ("ENG", "Todo", "Cap the backoff at 30s", ""),
        (
            "ENG",
            "In Progress",
            "Read Retry-After",
            "Linear's header is in SECONDS, not ms.",
        ),
        ("ENG", "Done", "Upgrade ureq", ""),
        ("ENG", "Canceled", "Rewrite in Go", ""),
        ("ENG", "Duplicate", "Retry storm again", ""),
        ("OPS", "Todo", "Rotate the deploy keys 🔑", ""),
        ("OPS", "Done", "Patch the bastion", ""),
        ("ENG", "Todo", "Blocked one", ""),
        ("ENG", "Todo", "Old idea", ""),
        ("ENG", "Todo", "Spam", ""),
    ]
    .iter()
    .enumerate()
    .map(|(n, (team, state, title, body))| {
        let id = fake.team_issue(team, state, title, body);
        let filed = T0 + n as u64 * DAY;
        fake.set_times(&id, filed, filed + HOUR);
        id
    })
    .collect();
    let w = World {
        fake,
        phil,
        chen,
        trackers,
        issues,
    };
    let f = &w.fake;
    let (e1, e2, e3, e4) = (w.issue(101), w.issue(102), w.issue(103), w.issue(104));
    let (e5, e6, o1, o2) = (w.issue(105), w.issue(106), w.issue(107), w.issue(108));
    let e7 = w.issue(109);
    f.set_times(e1, T0, T0 + 9 * DAY);
    f.set_priority(e1, 1);
    f.set_estimate(e1, 3.0);
    f.set_due(e1, "2026-12-24");
    f.assign(e1, &w.phil);
    f.delegate(e1, &f.app_user());
    f.set_creator(e1, &w.chen);
    f.set_project(e1, &w.trackers, Some("B — reads"));
    f.set_cycle(e1, &twelve);
    f.label(e1, "afkd/ready");
    f.label(e1, "Bug 🐛");
    f.link(e1, "https://github.com/acme/widgets/pull/42");
    f.subscribe(e1, &w.phil);
    f.subscribe(e1, &w.chen);
    f.relate(e1, o2, "related");
    f.relate(e5, e1, "similar");
    f.set_priority(e2, 2);
    f.set_estimate(e2, 1.0);
    f.set_due(e2, "2026-11-30");
    f.assign(e2, &w.chen);
    f.label(e2, "afkd/ready");
    f.set_parent(e3, e1);
    f.label(e3, "Hold");
    f.set_priority(e4, 4);
    f.complete_at(e4, T0 + 3 * DAY + 2 * HOUR);
    f.relate(e4, e2, "blocks");
    f.cancel_at(e5, T0 + 4 * DAY + 2 * HOUR);
    f.relate(e6, e1, "duplicate");
    f.set_priority(o1, 3);
    f.set_project(o1, &w.trackers, None);
    f.set_cycle(o1, &three);
    f.complete_at(o2, T0 + 7 * DAY + 2 * HOUR);
    f.relate(o1, e7, "blocks");
    f.assign(e7, &f.key_user());
    f.set_sort_order(e7, 500.0);
    f.archive(w.issue(110));
    f.trash(w.issue(111));
    w
}

/// Send the read `action` with `args`, a workspace among them, to a child of its own sharing
/// `state`, the plugin's directory, and read the reply.
fn read(action: &str, args: Value, state: &TempDir) -> Value {
    once(action, args, state.path()).0
}

/// `args` with `ws` the workspace reaching `fake` by its API key.
fn with_ws(fake: &FakeLinear, args: Value) -> Value {
    let mut args = args;
    args["ws"] = ws(fake);
    args
}

/// The ids, identifiers for issues, of the handles an ok reply carries.
fn ids(reply: &Value) -> Vec<String> {
    assert_eq!(reply["ok"], true, "{reply}");
    reply["handles"]
        .as_array()
        .unwrap_or_else(|| panic!("no handles: {reply}"))
        .iter()
        .map(|h| h["id"].as_str().expect("an id").to_string())
        .collect()
}

/// The issues `issues` reads with `filters` from `w`'s workspace, by identifier.
fn issues(w: &World, filters: Value, state: &TempDir) -> Vec<String> {
    ids(&read("issues", with_ws(&w.fake, filters), state))
}

/// The identifiers numbered `numbers`, each in its team.
fn named(numbers: &[usize]) -> Vec<String> {
    numbers
        .iter()
        .map(|n| match n {
            107 | 108 => format!("OPS-{n}"),
            n => format!("ENG-{n}"),
        })
        .collect()
}

/// End a leg: every request the plugin sent was one Linear would take.
fn end(fake: &FakeLinear) {
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests Linear's schema refuses"
    );
}

// --- issues ---

/// `issues` hands back each issue with every field of its handle, as Linear holds it: the
/// richest issue's, keyed `linear:<organization>/<issue>` and identified by its identifier,
/// the issues it relates to both ways, and the relation fields of the issues around it.
#[test]
fn issues_hands_back_every_field_of_each_issue() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("reads-fields");
    let (reply, stderr) = once(
        "issues",
        with_ws(f, json!({"id": [w.issue(101)]})),
        state.path(),
    );
    let e1 = w.issue(101);
    assert_eq!(
        reply,
        json!({"ok": true, "handles": [{
            "id": "ENG-101",
            "key": format!("linear:{}/{e1}", f.org_id()),
            "fields": {
                "uuid": e1, "workspace": "acme", "team": "ENG", "title": TITLE,
                "url": f.url(e1), "description": BODY, "branch": f.branch(e1),
                "state": "Todo", "state_type": "unstarted", "priority": "urgent",
                "estimate": 3, "due": "2026-12-24", "assignee": "phil", "delegate": "afkd-app",
                "creator": "chen", "project": "Trackers 🧭", "milestone": "B — reads",
                "cycle": 12, "labels": ["afkd/ready", "Bug 🐛"], "parent": null,
                "sub_issues": ["ENG-103"], "blocks": [], "blocked_by": [],
                "related": ["OPS-108"], "similar": ["ENG-105"], "duplicates": ["ENG-106"],
                "duplicate_of": null, "links": ["https://github.com/acme/widgets/pull/42"],
                "subscribers": ["phil", "chen"], "created_at": T0, "updated_at": T0 + 9 * DAY,
                "completed_at": null, "canceled_at": null, "archived": false, "trashed": false,
            },
        }]})
    );
    assert_eq!(stderr, "[linear] read 1 issues from workspace acme\n");

    let reply = read(
        "issues",
        with_ws(
            f,
            json!({"id": ["ENG-102", "ENG-104", "ENG-106", "OPS-108", "ENG-103"]}),
        ),
        &state,
    );
    let fields = |id: &str| {
        reply["handles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {reply}"))["fields"]
            .clone()
    };
    assert_eq!(fields("ENG-102")["blocked_by"], json!(["ENG-104"]));
    assert_eq!(fields("ENG-104")["blocks"], json!(["ENG-102"]));
    assert_eq!(fields("ENG-104")["completed_at"], T0 + 3 * DAY + 2 * HOUR);
    assert_eq!(fields("ENG-106")["duplicate_of"], "ENG-101");
    assert_eq!(fields("ENG-106")["state_type"], "canceled");
    assert_eq!(fields("OPS-108")["related"], json!(["ENG-101"]));
    assert_eq!(fields("OPS-108")["team"], "OPS");
    assert_eq!(fields("ENG-103")["parent"], "ENG-101");
    assert_eq!(fields("ENG-103")["priority"], "none");
    end(f);
}

/// Each filter alone keeps exactly the issues it names, in the default order, archived and
/// trashed issues left out: `id` by identifier and by uuid, `team`, `state` in either team,
/// `state_type` (a duplicate's state reading `canceled`), `priority`, `labels` (every one),
/// `without_labels` (none), `assignee` and `delegate` by display name, email, `me` and
/// `none`, `project`, `milestone`, `cycle`, `parent`, `blocked` both ways (a done blocker
/// blocking nothing), `text` in the title or the description with its case ignored, each
/// time filter, and each due-date filter.
#[test]
fn issues_keeps_what_each_filter_names() {
    let w = world();
    let state = TempDir::new("reads-filters");
    let day = |n: f64| (T0 as f64 + n * DAY as f64) as u64;
    for (filters, kept) in [
        (json!({"id": ["ENG-103", w.issue(107)]}), named(&[103, 107])),
        (json!({"team": ["OPS"]}), named(&[107, 108])),
        (json!({"state": ["Done"]}), named(&[104, 108])),
        (json!({"state_type": ["canceled"]}), named(&[105, 106])),
        (
            json!({"state_type": ["unstarted", "started"]}),
            named(&[101, 109, 102, 103, 107]),
        ),
        (json!({"priority": ["urgent", "low"]}), named(&[101, 104])),
        (json!({"labels": ["afkd/ready"]}), named(&[101, 102])),
        (json!({"labels": ["afkd/ready", "Bug 🐛"]}), named(&[101])),
        (
            json!({"without_labels": ["afkd/ready", "Hold"]}),
            named(&[109, 104, 105, 106, 107, 108]),
        ),
        (json!({"assignee": ["phil"]}), named(&[101])),
        (json!({"assignee": ["chen@acme.example"]}), named(&[102])),
        (json!({"assignee": ["me"]}), named(&[109])),
        (
            json!({"assignee": ["none"]}),
            named(&[103, 104, 105, 106, 107, 108]),
        ),
        (json!({"assignee": ["nobody@acme.example"]}), Vec::new()),
        (json!({"delegate": ["afkd-app"]}), named(&[101])),
        (
            json!({"delegate": ["none"]}),
            named(&[109, 102, 103, 104, 105, 106, 107, 108]),
        ),
        (json!({"project": ["Trackers 🧭"]}), named(&[101, 107])),
        (json!({"milestone": ["B — reads"]}), named(&[101])),
        (json!({"cycle": [12]}), named(&[101])),
        (json!({"cycle": [3, 4]}), named(&[107])),
        (json!({"parent": ["ENG-101"]}), named(&[103])),
        (json!({"parent": [w.issue(101)]}), named(&[103])),
        (json!({"blocked": true}), named(&[109])),
        (
            json!({"blocked": false}),
            named(&[101, 102, 103, 104, 105, 106, 107, 108]),
        ),
        (json!({"text": "RETRY"}), named(&[101, 103, 106])),
        (json!({"text": "seconds"}), named(&[103])),
        (
            json!({"created_after": day(4.5)}),
            named(&[109, 106, 107, 108]),
        ),
        (json!({"created_before": day(1.5)}), named(&[101, 102])),
        (json!({"updated_after": day(8.5)}), named(&[101])),
        (json!({"updated_before": day(1.5)}), named(&[102])),
        (json!({"completed_after": day(3.5)}), named(&[108])),
        (json!({"completed_before": day(5.0)}), named(&[104])),
        (json!({"canceled_after": T0}), named(&[105])),
        (json!({"canceled_before": day(4.0)}), Vec::new()),
        (json!({"due_after": "2026-12-01"}), named(&[101])),
        (json!({"due_before": "2026-12-01"}), named(&[102])),
        (
            json!({"team": ["ENG"], "state_type": ["unstarted"], "without_labels": ["Hold"],
                   "priority": ["urgent", "high", "none"]}),
            named(&[101, 109, 102]),
        ),
    ] {
        assert_eq!(issues(&w, filters.clone(), &state), kept, "{filters}");
    }
    end(&w.fake);
}

/// `order` with each key alone reads the issues in the order Linear's `sort` gives for it:
/// the manual order, by team, by state, by priority (none last), by assignee, project and
/// cycle (none last), the due date newest first, by estimate and title, and the dates
/// newest first; every tie keeps Linear's own order.
#[test]
fn issues_orders_by_each_key_alone() {
    let w = world();
    let state = TempDir::new("reads-order");
    for (key, order) in [
        ("manual", [101, 109, 102, 103, 104, 105, 106, 107, 108]),
        ("team", [101, 102, 103, 104, 105, 106, 109, 107, 108]),
        ("state", [107, 101, 102, 108, 109, 103, 104, 105, 106]),
        ("priority", [101, 102, 107, 104, 103, 105, 106, 108, 109]),
        ("assignee", [109, 102, 101, 103, 104, 105, 106, 107, 108]),
        ("project", [101, 107, 102, 103, 104, 105, 106, 108, 109]),
        ("cycle", [107, 101, 102, 103, 104, 105, 106, 108, 109]),
        ("due", [101, 102, 103, 104, 105, 106, 107, 108, 109]),
        ("estimate", [102, 101, 103, 104, 105, 106, 107, 108, 109]),
        ("title", [109, 102, 108, 103, 106, 105, 107, 104, 101]),
        ("created", [109, 108, 107, 106, 105, 104, 103, 102, 101]),
        ("updated", [101, 109, 108, 107, 106, 105, 104, 103, 102]),
        ("completed", [108, 104, 101, 102, 103, 105, 106, 107, 109]),
    ] {
        assert_eq!(
            issues(&w, json!({ "order": [key] }), &state),
            named(&order),
            "{key}"
        );
    }
    end(&w.fake);
}

/// `order` with three keys, the most Linear sorts by, reads by the first, each tie broken by
/// the next: by priority, the issues of none by assignee, the unassigned among them by title,
/// sent as the three elements of one `sort`.
#[test]
fn issues_orders_by_three_keys_each_breaking_the_ties_of_the_one_before() {
    let w = world();
    let state = TempDir::new("reads-three-keys");
    assert_eq!(
        issues(
            &w,
            json!({"order": ["priority", "assignee", "title"]}),
            &state
        ),
        named(&[101, 102, 107, 104, 109, 108, 103, 106, 105])
    );
    let sorts: Vec<Value> = w
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.operation == "IssueList")
        .map(|s| s.variables["sort"].clone())
        .collect();
    assert_eq!(
        sorts,
        [
            json!([{"priority": {"nulls": "last"}}, {"assignee": {"nulls": "last"}},
                {"title": {"nulls": "last"}}])
        ]
    );
    end(&w.fake);
}

/// The fake refuses a `sort` of more than three elements as Linear does, before it reads
/// anything: the plugin's own `IssueList`, sent with four, is Linear's `Argument Validation
/// Error` naming the limit, and recorded; the same with three is answered.
#[test]
fn the_fake_refuses_a_sort_of_four_as_linear_does() {
    let fake = FakeLinear::start();
    fake.state("Todo", "unstarted");
    fake.issue("Todo", "排序 🧮", "");
    let query = operation_document("IssueList").unwrap();
    let request = |sort: Value| {
        json!({"operationName": "IssueList", "query": query, "variables": {
            "filter": {}, "sort": sort, "first": 1, "includeArchived": false}})
    };
    let key = |name: &str| json!({ name: {"nulls": "last"} });
    let (status, body) = fake.post_raw(&request(json!([
        key("team"),
        key("title"),
        key("estimate")
    ])));
    assert_eq!((status, body["errors"].is_null()), (200, true), "{body}");
    assert_eq!(body["data"]["issues"]["nodes"][0]["title"], "排序 🧮");
    let (status, body) = fake.post_raw(&request(json!([
        key("team"),
        key("title"),
        key("estimate"),
        key("assignee")
    ])));
    assert_eq!(status, 400);
    let error = &body["errors"][0];
    assert_eq!(error["message"], "Argument Validation Error");
    assert_eq!(error["extensions"]["code"], "INVALID_INPUT");
    assert_eq!(
        error["extensions"]["validationErrors"][0]["constraints"]["arrayMaxSize"],
        "sort must contain no more than 3 elements"
    );
    assert!(body["data"].is_null(), "nothing was read");
    assert_eq!(
        fake.invalid_requests(),
        ["sort must contain no more than 3 elements"]
    );
}

/// Left out, `order` reads by team, then by state in its team's workflow, then top to bottom
/// as Linear shows the state: two teams, each over two states, the manual order inside one
/// state differing from the order the issues were filed in.
#[test]
fn issues_default_order_is_team_then_state_then_manual() {
    let w = world();
    let state = TempDir::new("reads-default");
    let all = named(&[101, 109, 102, 103, 104, 105, 106, 107, 108]);
    assert_eq!(issues(&w, json!({}), &state), all);
    assert_eq!(
        issues(&w, json!({"order": ["team", "state", "manual"]}), &state),
        all
    );
    let sorts: Vec<Value> = w
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.operation == "IssueList")
        .map(|s| s.variables["sort"].clone())
        .collect();
    assert_eq!(sorts[0], sorts[1], "left out is the default");
    assert_eq!(
        sorts[0],
        json!([{"team": {"nulls": "last"}}, {"workflowState": {"nulls": "last"}},
               {"manual": {"order": "Ascending"}}])
    );
    end(&w.fake);
}

/// `limit` reads at most that many: one, in one page of one, and 250 of 251, in pages of 20
/// and a last of 10.
#[test]
fn issues_limit_reads_one_and_250() {
    let fake = FakeLinear::start();
    fake.state("Todo", "unstarted");
    for n in 0..251 {
        fake.issue("Todo", &format!("課題 {n} 🚧"), "");
    }
    let state = TempDir::new("reads-limit");
    let one = ids(&read("issues", with_ws(&fake, json!({"limit": 1})), &state));
    assert_eq!(one, ["ENG-101"]);
    let most = ids(&read(
        "issues",
        with_ws(&fake, json!({"limit": 250})),
        &state,
    ));
    assert_eq!(most.len(), 250);
    assert_eq!(most.last().map(String::as_str), Some("ENG-350"));
    let firsts: Vec<u64> = fake
        .seen()
        .into_iter()
        .filter(|s| s.operation == "IssueList")
        .map(|s| s.variables["first"].as_u64().unwrap())
        .collect();
    let mut want = vec![1];
    want.extend([20; 12]);
    want.push(10);
    assert_eq!(firsts, want);
    end(&fake);
}

/// `include_archived` adds the archived issue and the trashed one, each saying so in its
/// fields; left out, neither is read.
#[test]
fn issues_include_archived_adds_archived_and_trashed() {
    let w = world();
    let state = TempDir::new("reads-archived");
    assert_eq!(
        issues(&w, json!({"team": ["ENG"], "state": ["Todo"]}), &state),
        named(&[101, 109, 102])
    );
    let reply = read(
        "issues",
        with_ws(
            &w.fake,
            json!({"team": ["ENG"], "state": ["Todo"], "include_archived": true}),
        ),
        &state,
    );
    assert_eq!(ids(&reply), named(&[101, 109, 102, 110, 111]));
    let flags: Vec<(Value, Value)> = reply["handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["fields"]["archived"].clone(),
                h["fields"]["trashed"].clone(),
            )
        })
        .collect();
    assert_eq!(flags[3], (json!(true), json!(false)));
    assert_eq!(
        flags[4],
        (json!(true), json!(true)),
        "a trashed issue is archived too"
    );
    end(&w.fake);
}

/// Every argument `issues` cannot use is refused in its own sentence, and no request is sent
/// for it: an empty list, an `order` of more than the three keys Linear sorts by, a limit
/// outside 1 to 250, a date that is none or no day of the calendar, and an id that is none.
#[test]
fn issues_refuses_an_empty_list_a_bad_limit_date_or_id() {
    let fake = FakeLinear::start();
    let state = TempDir::new("reads-refusals");
    for (filters, error) in [
        (
            json!({"labels": []}),
            "issues: `labels` is empty; leave it out to keep every issue",
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
        (
            json!({"due_after": "2026-02-30T00:00"}),
            "issues: due_after \"2026-02-30T00:00\" is no YYYY-MM-DD date",
        ),
        (
            json!({"due_before": "2026-02-31"}),
            "issues: due_before \"2026-02-31\" is no YYYY-MM-DD date",
        ),
        (
            json!({"id": ["ENG-12", "eng/12"]}),
            "issues: id \"eng/12\" is neither an identifier (ENG-123) nor a uuid",
        ),
    ] {
        let (reply, stderr) = once("issues", with_ws(&fake, filters.clone()), state.path());
        assert_eq!(reply, json!({"ok": false, "error": error}), "{filters}");
        assert_eq!(stderr, format!("afkd-linear: {error}\n"));
    }
    assert!(fake.seen().is_empty(), "nothing was sent");
}

/// A read Linear rate limits is sent again after the `Retry-After` it named, within the
/// call, and answers: the caller sees the issues, not the limit.
#[test]
fn a_rate_limited_read_waits_its_retry_after_and_answers() {
    let w = world();
    w.fake.rate_limit("IssueList", 1, 1);
    let state = TempDir::new("reads-limited");
    assert_eq!(
        issues(&w, json!({"team": ["OPS"]}), &state),
        named(&[107, 108])
    );
    let sent: Vec<Instant> = w
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.operation == "IssueList")
        .map(|s| s.at)
        .collect();
    assert_eq!(sent.len(), 2, "limited once, then answered");
    assert!(
        sent[1] - sent[0] >= Duration::from_millis(950),
        "the retry waited out the second Linear asked for: {:?}",
        sent[1] - sent[0]
    );
    end(&w.fake);
}

/// Every list nested in an issue is read past the 20 its first read holds: 23 labels, 22
/// sub-issues, 21 relations each way, 24 links and 21 subscribers, each whole and in order.
#[test]
fn issues_reads_every_nested_list_past_its_first_page() {
    let fake = FakeLinear::start();
    fake.state("Todo", "unstarted");
    let issue = fake.issue("Todo", TITLE, "");
    let mut want: Vec<(&str, Vec<String>)> = vec![
        ("labels", Vec::new()),
        ("sub_issues", Vec::new()),
        ("blocks", Vec::new()),
        ("blocked_by", Vec::new()),
        ("links", Vec::new()),
        ("subscribers", Vec::new()),
    ];
    for n in 0..23 {
        let name = format!("label {n} 🏷");
        fake.label(&issue, &name);
        want[0].1.push(name);
    }
    for n in 0..22 {
        let child = fake.issue("Todo", &format!("子任务 {n}"), "");
        fake.set_parent(&child, &issue);
        want[1].1.push(fake.identifier(&child));
    }
    for n in 0..21 {
        let other = fake.issue("Todo", &format!("blocked {n}"), "");
        fake.relate(&issue, &other, "blocks");
        want[2].1.push(fake.identifier(&other));
        let blocker = fake.issue("Todo", &format!("blocker {n}"), "");
        fake.relate(&blocker, &issue, "blocks");
        want[3].1.push(fake.identifier(&blocker));
    }
    for n in 0..24 {
        let url = format!("https://github.com/acme/widgets/pull/{n}");
        fake.link(&issue, &url);
        want[4].1.push(url);
    }
    for n in 0..21 {
        let user = fake.user(
            &format!("User {n}"),
            &format!("user{n}"),
            &format!("u{n}@acme.example"),
        );
        fake.subscribe(&issue, &user);
        want[5].1.push(format!("user{n}"));
    }
    let state = TempDir::new("reads-nested");
    let reply = read("issues", with_ws(&fake, json!({"id": [issue]})), &state);
    let fields = &reply["handles"][0]["fields"];
    for (field, values) in want {
        assert_eq!(fields[field], json!(values), "{field}");
    }
    let follow_ups: BTreeSet<String> = fake
        .seen()
        .into_iter()
        .map(|s| s.operation)
        .filter(|op| op != "Organization" && op != "IssueList")
        .collect();
    assert_eq!(
        follow_ups,
        BTreeSet::from(
            [
                "IssueLabelNames",
                "IssueChildren",
                "IssueOutRelations",
                "IssueRelations",
                "IssueAttachments",
                "IssueSubscribers",
            ]
            .map(String::from)
        )
    );
    end(&fake);
}

// --- the other reads ---

/// `comments` reads an issue's thread oldest first, each reply right after the comment it
/// answers and a reply to a reply after that, two comments of one moment by their ids: every
/// field of each, its author a user's display name, an integration's or an external user's
/// name, its key the workspace's and its own uuid.
#[test]
fn comments_reads_the_thread_with_each_reply_after_what_it_answers() {
    let w = world();
    let f = &w.fake;
    let e1 = w.issue(101);
    let ask = f.comment_at(
        e1,
        Author::User(w.phil.clone()),
        "Please cap it at 30s.",
        T0 + HOUR,
        None,
    );
    let seen = f.comment_at(
        e1,
        Author::User(w.chen.clone()),
        "看起来不对 🚨 — it resets on every 401:\n\n```\nPOST /graphql 401\n```",
        T0 + 2 * HOUR,
        None,
    );
    let slack = f.comment_at(
        e1,
        Author::External("x-1".into()),
        "From Slack: same here",
        T0 + 2 * HOUR,
        None,
    );
    let which = f.comment_at(
        e1,
        Author::User(f.key_user()),
        "Which header?",
        T0 + 3 * HOUR,
        Some(&ask),
    );
    let answer = f.comment_at(
        e1,
        Author::User(w.phil.clone()),
        "Retry-After.",
        T0 + 4 * HOUR,
        Some(&which),
    );
    let bot = f.comment_at(
        e1,
        Author::Bot("bot-1".into()),
        "",
        T0 + 5 * HOUR,
        Some(&seen),
    );
    f.resolve(&ask, T0 + 6 * HOUR);
    f.comments_page(2);
    let state = TempDir::new("reads-comments");
    let handle =
        read("issues", with_ws(f, json!({"id": ["ENG-101"]})), &state)["handles"][0].clone();
    let (reply, stderr) = once(
        "comments",
        with_ws(
            f,
            json!({"issue": {"id": handle["id"], "key": handle["key"]}}),
        ),
        state.path(),
    );
    let (first, second) = if seen < slack {
        (&seen, &slack)
    } else {
        (&slack, &seen)
    };
    let mut order = vec![&ask, &which, &answer, first];
    if first == &seen {
        order.extend([&bot, second]);
    } else {
        order.extend([second, &bot]);
    }
    assert_eq!(
        ids(&reply),
        order.iter().map(|id| id.to_string()).collect::<Vec<_>>()
    );
    let url = f.url(e1);
    let comment = |id: &str| {
        reply["handles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        comment(&ask),
        json!({"id": ask, "key": format!("linear:{}/{ask}", f.org_id()), "fields": {
            "issue": "ENG-101", "body": "Please cap it at 30s.",
            "url": format!("{url}#comment-{}", &ask[..8]), "author": "phil", "reply_to": null,
            "resolved_at": T0 + 6 * HOUR, "created_at": T0 + HOUR, "edited_at": null,
        }})
    );
    assert_eq!(comment(&which)["fields"]["reply_to"], ask.as_str());
    assert_eq!(comment(&which)["fields"]["author"], "afkd");
    assert_eq!(comment(&answer)["fields"]["reply_to"], which.as_str());
    assert_eq!(comment(&slack)["fields"]["author"], "Álvaro (Slack)");
    assert_eq!(comment(&bot)["fields"]["author"], "afkd");
    assert_eq!(comment(&bot)["fields"]["body"], "");
    assert_eq!(
        stderr,
        "[linear] read 6 comments on ENG-101 from workspace acme\n"
    );
    let pages = f
        .seen()
        .into_iter()
        .filter(|s| s.operation == "IssueThread")
        .count();
    assert_eq!(pages, 3, "read two to the page, to the last");
    end(f);
}

/// `me` is the user the credential acts as: the API key's user, and with the app's
/// credentials the app's own user, an app.
#[test]
fn me_is_the_key_user_or_the_app() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("reads-me");
    let (reply, stderr) = once("me", with_ws(f, json!({})), state.path());
    assert_eq!(
        reply,
        json!({"ok": true, "handle": {
            "id": f.key_user(),
            "key": format!("linear:{}/{}", f.org_id(), f.key_user()),
            "fields": {"name": "afkd", "full_name": "afkd", "email": "afkd@acme.example",
                       "app": false},
        }})
    );
    assert_eq!(stderr, "[linear] read the viewer from workspace acme\n");
    let app = read("me", json!({"ws": app_ws(f)}), &state);
    assert_eq!(app["handle"]["id"], f.app_user());
    assert_eq!(
        app["handle"]["fields"],
        json!({"name": "afkd-app", "full_name": "afkd (app)", "email": "app@oauthapp.example",
               "app": true})
    );
    end(f);
}

/// `users` lists the workspace's users, an app among them, and leaves a disabled account out.
#[test]
fn users_lists_apps_and_leaves_a_disabled_account_out() {
    let w = world();
    let f = &w.fake;
    let vesper = f.app("Vesper", "vesper 🌑", "vesper@oauthapp.example");
    let gone = f.user("Former", "former", "former@acme.example");
    f.disable(&gone);
    let state = TempDir::new("reads-users");
    let reply = read("users", with_ws(f, json!({})), &state);
    let users: Vec<(String, bool)> = reply["handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["fields"]["name"].as_str().unwrap().to_string(),
                h["fields"]["app"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        users,
        [
            ("afkd".to_string(), false),
            ("afkd-app".to_string(), true),
            ("phil".to_string(), false),
            ("chen".to_string(), false),
            ("vesper 🌑".to_string(), true),
        ]
    );
    let chen = reply["handles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["fields"]["name"] == "chen")
        .unwrap();
    assert_eq!(chen["id"], w.chen.as_str());
    assert_eq!(
        chen["fields"],
        json!({"name": "chen", "full_name": "陳大文", "email": "chen@acme.example", "app": false})
    );
    assert!(!ids(&reply).contains(&gone));
    assert!(ids(&reply).contains(&vesper));
    end(f);
}

/// `teams` lists every team the credential reaches, each by its key, keyed by its uuid.
#[test]
fn teams_lists_every_team_by_its_key() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("reads-teams");
    let (reply, stderr) = once("teams", with_ws(f, json!({})), state.path());
    assert_eq!(ids(&reply), ["ENG", "OPS"]);
    assert_eq!(
        reply["handles"][0]["key"],
        format!("linear:{}/{}", f.org_id(), f.team_id())
    );
    assert_eq!(
        reply["handles"][1]["fields"],
        json!({"name": "Operations — 運用"})
    );
    assert_eq!(stderr, "[linear] read 2 teams from workspace acme\n");
    end(f);
}

/// `states` reads a team's states in its workflow's order, whatever order Linear lists them
/// in, a duplicate state's category read as `canceled`; another team's states are its own.
#[test]
fn states_reads_a_team_in_workflow_order() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("reads-states");
    let reply = read("states", with_ws(f, json!({"team": "ENG"})), &state);
    let names: Vec<(Value, Value)> = reply["handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["fields"]["name"].clone(), h["fields"]["type"].clone()))
        .collect();
    assert_eq!(
        names,
        [
            ("Triage", "triage"),
            ("Todo", "unstarted"),
            ("In Progress", "started"),
            ("Done", "completed"),
            ("Canceled", "canceled"),
            ("Duplicate", "canceled"),
        ]
        .map(|(n, t)| (json!(n), json!(t)))
    );
    assert!(reply["handles"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["fields"]["team"] == "ENG"
            && h["key"] == format!("linear:{}/{}", f.org_id(), h["id"].as_str().unwrap())));
    let triage = reply["handles"][0]["id"].as_str().unwrap().to_string();
    f.set_position(&triage, 10.0);
    let moved = read("states", with_ws(f, json!({"team": "ENG"})), &state);
    assert_eq!(
        moved["handles"][5]["fields"]["name"], "Triage",
        "workflow order"
    );
    let ops = read("states", with_ws(f, json!({"team": "OPS"})), &state);
    assert_eq!(
        ops["handles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["fields"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Todo", "Done"]
    );
    end(f);
}

/// `labels` reads the workspace's labels, and with a team that team's beside them, each with
/// its group and its team; another team's are never among them.
#[test]
fn labels_reads_the_workspace_and_a_team() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("reads-labels");
    let label_fields = |reply: &Value| -> Vec<Value> {
        reply["handles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["fields"].clone())
            .collect()
    };
    let workspace = read("labels", with_ws(f, json!({})), &state);
    assert_eq!(
        label_fields(&workspace),
        [json!({"name": "Needs Info", "group": null, "team": null})]
    );
    let eng = read("labels", with_ws(f, json!({"team": "ENG"})), &state);
    assert_eq!(
        label_fields(&eng),
        [
            json!({"name": "Type", "group": null, "team": "ENG"}),
            json!({"name": "Bug 🐛", "group": "Type", "team": "ENG"}),
            json!({"name": "Needs Info", "group": null, "team": null}),
            json!({"name": "afkd/ready", "group": null, "team": "ENG"}),
            json!({"name": "Hold", "group": null, "team": "ENG"}),
        ]
    );
    end(f);
}

/// `projects` reads every project, or a team's, each with its teams and its milestones in
/// their order — past the 50 a page holds for a project with 53.
#[test]
fn projects_reads_teams_and_milestones_in_order() {
    let w = world();
    let f = &w.fake;
    let milestones: Vec<(String, f64)> = (0..53)
        .map(|n| (format!("M{n:02}"), f64::from(52 - n)))
        .collect();
    let named_milestones: Vec<(&str, f64)> =
        milestones.iter().map(|(n, o)| (n.as_str(), *o)).collect();
    let roadmap = f.project("Roadmap 2027", &["OPS"], &named_milestones);
    let state = TempDir::new("reads-projects");
    let all = read("projects", with_ws(f, json!({})), &state);
    assert_eq!(ids(&all), [w.trackers.clone(), roadmap.clone()]);
    assert_eq!(
        all["handles"][0]["fields"],
        json!({"name": "Trackers 🧭", "teams": ["ENG", "OPS"],
               "milestones": ["B — reads", "C — writes"],
               "url": all["handles"][0]["fields"]["url"]})
    );
    assert!(all["handles"][0]["fields"]["url"]
        .as_str()
        .unwrap()
        .starts_with("https://linear.app/acme/project/trackers-"));
    let in_order: Vec<String> = (0..53).rev().map(|n| format!("M{n:02}")).collect();
    assert_eq!(all["handles"][1]["fields"]["milestones"], json!(in_order));
    assert_eq!(
        ids(&read(
            "projects",
            with_ws(f, json!({"team": "ENG"})),
            &state
        )),
        std::slice::from_ref(&w.trackers)
    );
    end(f);
}

/// `cycles` reads a team's cycles oldest first, whatever order Linear lists them in, each
/// with its number, its name or none, its dates and whether it is the one running.
#[test]
fn cycles_reads_a_team_oldest_first() {
    let w = world();
    let f = &w.fake;
    let eleven = f.cycle("ENG", 11, None, T0 - 14 * DAY, T0, false);
    let state = TempDir::new("reads-cycles");
    let reply = read("cycles", with_ws(f, json!({"team": "ENG"})), &state);
    assert_eq!(reply["handles"][0]["id"], eleven.as_str());
    assert_eq!(
        reply["handles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["fields"].clone())
            .collect::<Vec<_>>(),
        [
            json!({"number": 11, "name": null, "team": "ENG", "starts_at": T0 - 14 * DAY,
                   "ends_at": T0, "active": false}),
            json!({"number": 12, "name": "Sprint 12 — 冬", "team": "ENG", "starts_at": T0,
                   "ends_at": T0 + 14 * DAY, "active": true}),
        ]
    );
    assert_eq!(
        read("cycles", with_ws(f, json!({"team": "OPS"})), &state)["handles"][0]["fields"]
            ["number"],
        3
    );
    end(f);
}

/// `files` lists the urls of every file Linear hosts that the issue links: its description's
/// first, then its comments' oldest first, each once, a link to anywhere else left out and a
/// control marker's links with it. An issue that links none lists none.
#[test]
fn files_lists_the_linear_files_description_first() {
    let w = world();
    let f = &w.fake;
    let e3 = w.issue(103);
    let plan = f.seed_file("plan.md", "text/markdown", b"# Plan\n");
    let shot = f.seed_file("screen shot — 修复.png", "image/png", &png());
    let log = f.seed_file("run.log", "text/plain", b"cargo test: 3 failed\n");
    let later = f.seed_file("after.png", "image/png", &png());
    let marked = f.seed_file("marker.png", "image/png", &png());
    f.describe(
        e3,
        &format!(
            "Linear's header is in SECONDS.\n\n[plan.md]({plan}) and \
             [elsewhere](https://example.com/plan.md)"
        ),
    );
    f.comment_at(
        e3,
        Author::User(w.phil.clone()),
        &format!("![after]({later}) and again [the shot]({shot})"),
        T0 + 3 * HOUR,
        None,
    );
    f.comment_at(
        e3,
        Author::User(w.chen.clone()),
        &format!("The log: [run.log]({log}) and [the shot]({shot})"),
        T0 + 2 * HOUR,
        None,
    );
    f.comment_at(
        e3,
        Author::User(f.key_user()),
        &format!("[afkd-attempt] 1/2: see [marker.png]({marked})"),
        T0 + 4 * HOUR,
        None,
    );
    let state = TempDir::new("reads-files");
    let handle = |identifier: &str| {
        read("issues", with_ws(f, json!({"id": [identifier]})), &state)["handles"][0].clone()
    };
    let (reply, stderr) = once(
        "files",
        with_ws(f, json!({"issue": handle("ENG-103")})),
        state.path(),
    );
    assert_eq!(
        reply,
        json!({"ok": true, "value": [plan, log, shot, later]})
    );
    assert_eq!(
        stderr,
        "[linear] read 4 files of ENG-103 from workspace acme\n"
    );
    assert_eq!(
        read(
            "files",
            with_ws(f, json!({"issue": handle("ENG-104")})),
            &state
        ),
        json!({"ok": true, "value": []})
    );
    end(f);
}

/// `download_file` saves the file Linear hosts at a url to the path given, byte for byte,
/// fetched with the credential; a url Linear does not host is refused before anything is
/// sent to it, a relative path is refused before anything is read, and a directory that is
/// not there leaves nothing.
#[test]
fn download_file_saves_the_bytes_with_the_credential() {
    let w = world();
    let f = &w.fake;
    let url = f.seed_file("screen shot — 修复.png", "image/png", &png());
    let state = TempDir::new("reads-download");
    let dir = TempDir::new("reads-download-to");
    let to = dir.path().join("計画 1.png");
    let (reply, stderr) = once(
        "download_file",
        with_ws(f, json!({"url": url, "to": to})),
        state.path(),
    );
    assert_eq!(reply, json!({"ok": true}));
    assert_eq!(std::fs::read(&to).unwrap(), png());
    assert_eq!(
        stderr,
        format!(
            "[linear] downloading {url} to {} ({} bytes) from workspace acme\n",
            to.display(),
            png().len()
        )
    );
    let fetched = f
        .seen()
        .into_iter()
        .find(|s| s.method == "GET")
        .expect("the file was fetched");
    assert_eq!(fetched.authorization.as_deref(), Some(API_KEY));

    for (args, error) in [
        (
            json!({"url": "https://example.com/x.png", "to": dir.path().join("x.png")}),
            "linear fetch file: no file Linear hosts at https://example.com/x.png".to_string(),
        ),
        (
            json!({"url": url, "to": "x.png"}),
            "download_file: to must be absolute, not \"x.png\"".to_string(),
        ),
    ] {
        assert_eq!(
            read("download_file", with_ws(f, args), &state),
            json!({"ok": false, "error": error})
        );
    }
    let missing = dir.path().join("gone").join("x.png");
    let refused = read(
        "download_file",
        with_ws(f, json!({"url": url, "to": missing})),
        &state,
    );
    assert!(
        refused["error"].as_str().unwrap().starts_with(&format!(
            "download_file: cannot write {}: ",
            missing.display()
        )),
        "{refused}"
    );
    assert!(!dir.path().join("gone").exists());
    let gets = f.seen().into_iter().filter(|s| s.method == "GET").count();
    assert_eq!(gets, 2, "the refused url was never asked for");
    end(f);
}

// --- refusals ---

/// Every read refuses a workspace that holds no one credential — none, an empty key, a key
/// beside the app's id and secret or either, or half the app's — naming the fields, and
/// sends nothing.
#[test]
fn every_read_refuses_a_workspace_without_one_credential() {
    let fake = FakeLinear::start();
    let state = TempDir::new("reads-credentials");
    let base = fake.base_url();
    let issue = json!({"id": "ENG-1", "key": "linear:org/uuid"});
    let args = [
        ("issues", json!({})),
        ("comments", json!({"issue": issue})),
        ("me", json!({})),
        ("users", json!({})),
        ("teams", json!({})),
        ("states", json!({"team": "ENG"})),
        ("labels", json!({})),
        ("projects", json!({})),
        ("cycles", json!({"team": "ENG"})),
        ("files", json!({"issue": issue})),
        (
            "download_file",
            json!({"url": format!("{base}/uploads/x"), "to": "/tmp/afkd-linear-never"}),
        ),
    ];
    let none = "the linear.Workspace sets no credential; set api_key, or client_id and \
                client_secret";
    for (ws, sentence) in [
        (
            json!({"api_key": null, "client_id": null, "client_secret": null, "base_url": base}),
            none.to_string(),
        ),
        (
            json!({"api_key": "", "client_id": null, "client_secret": null, "base_url": base}),
            none.to_string(),
        ),
        (
            json!({"api_key": API_KEY, "client_id": CLIENT_ID, "client_secret": "s",
                   "base_url": base}),
            "the linear.Workspace sets api_key beside client_id and client_secret; set one or \
             the other"
                .to_string(),
        ),
        (
            json!({"api_key": API_KEY, "client_id": CLIENT_ID, "client_secret": null,
                   "base_url": base}),
            "the linear.Workspace sets api_key beside client_id; set one or the other".to_string(),
        ),
        (
            json!({"api_key": null, "client_id": CLIENT_ID, "client_secret": "", "base_url": base}),
            "the linear.Workspace sets client_id without client_secret".to_string(),
        ),
        (
            json!({"api_key": null, "client_id": null, "client_secret": "s", "base_url": base}),
            "the linear.Workspace sets client_secret without client_id".to_string(),
        ),
    ] {
        for (action, extra) in &args {
            let mut call = extra.clone();
            call["ws"] = ws.clone();
            let (reply, stderr) = once(action, call, state.path());
            let error = format!("{action}: {sentence}");
            assert_eq!(reply, json!({"ok": false, "error": error}), "{action} {ws}");
            assert_eq!(stderr, format!("afkd-linear: {error}\n"));
        }
    }
    assert!(fake.seen().is_empty(), "no read reached Linear");
    assert!(
        std::fs::read_dir(state.path()).unwrap().next().is_none(),
        "nothing was kept"
    );
}

/// A handle one workspace handed back, passed to a read of another, is refused naming both
/// url keys — the handle's as the plugin recorded it when its read ran — and with no record
/// of it, the handle's organization; nothing of the other workspace's is read.
#[test]
fn a_handle_of_another_workspace_is_refused_naming_both_url_keys() {
    let w = world();
    let globex = FakeLinear::start_globex();
    let state = TempDir::new("reads-other");
    let handle = read(
        "issues",
        with_ws(&w.fake, json!({"id": ["ENG-101"]})),
        &state,
    )["handles"][0]
        .clone();
    let issue = json!({"id": handle["id"], "key": handle["key"]});
    for action in ["comments", "files"] {
        let (reply, stderr) = once(
            action,
            json!({"ws": ws(&globex), "issue": issue}),
            state.path(),
        );
        let error = format!(
            "{action}: issue is ENG-101 of workspace acme, and the linear.Workspace reaches globex"
        );
        assert_eq!(reply, json!({"ok": false, "error": error}));
        assert!(
            stderr.contains(&format!("afkd-linear: {error}\n")),
            "{stderr}"
        );
    }
    let fresh = TempDir::new("reads-other-fresh");
    let reply = read(
        "comments",
        json!({"ws": ws(&globex), "issue": issue}),
        &fresh,
    );
    assert_eq!(
        reply["error"],
        format!(
            "comments: issue is ENG-101 of the workspace whose organization is {}, and the \
             linear.Workspace reaches globex",
            w.fake.org_id()
        )
    );
    let globex_ops: Vec<String> = globex.seen().into_iter().map(|s| s.operation).collect();
    assert!(
        globex_ops.iter().all(|op| op == "Organization"),
        "only the workspace itself was read: {globex_ops:?}"
    );
    end(&w.fake);
    end(&globex);
}

/// `states` and `cycles` name a team by its key, and a key the workspace has no team for is
/// refused naming it.
#[test]
fn states_and_cycles_refuse_a_team_the_workspace_lacks() {
    let w = world();
    let state = TempDir::new("reads-no-team");
    for action in ["states", "cycles"] {
        assert_eq!(
            read(action, with_ws(&w.fake, json!({"team": "QA 🧪"})), &state),
            json!({"ok": false, "error": "linear resolve team: no team with the key 'QA 🧪'"})
        );
    }
    end(&w.fake);
}

/// A handle whose key names no issue this plugin holds or handed out — a claim key in a
/// child that claimed nothing — is refused naming it, and the read sends nothing past the
/// workspace.
#[test]
fn a_key_that_names_nothing_is_refused() {
    let w = world();
    let state = TempDir::new("reads-nothing");
    let reply = read(
        "comments",
        with_ws(
            &w.fake,
            json!({"issue": {"id": "ENG-101", "key": "0f6c1d2e#5e2a9c71"}}),
        ),
        &state,
    );
    assert_eq!(
        reply,
        json!({"ok": false,
               "error": "comments: issue 0f6c1d2e#5e2a9c71 names no issue this plugin holds or \
                         handed out"})
    );
    let ops: Vec<String> = w.fake.seen().into_iter().map(|s| s.operation).collect();
    assert_eq!(ops, ["Organization"]);
    end(&w.fake);
}

// --- reaching a workspace ---

/// An OAuth app's token is fetched once across many children, each a call of its own, kept
/// in the plugin's directory by client id; one Linear refuses is fetched again, and the read
/// still answers. Another app's token is kept beside it, under its own id.
#[test]
fn an_apps_token_is_fetched_once_across_calls_and_again_after_a_401() {
    let w = world();
    let globex = FakeLinear::start_globex();
    let state = TempDir::new("reads-token");
    for _ in 0..3 {
        assert_eq!(
            ids(&read("teams", json!({"ws": app_ws(&w.fake)}), &state)),
            ["ENG", "OPS"]
        );
    }
    assert_eq!(w.fake.token_fetches(), 1, "one fetch across three children");
    let kept = state.path().join(format!("token-{CLIENT_ID}.json"));
    assert!(kept.is_file(), "the token is kept by client id");
    let token: Value = serde_json::from_str(&std::fs::read_to_string(&kept).unwrap()).unwrap();
    assert_eq!(token["token"], "lin_oauth_tok-1");
    w.fake.revoke_tokens();
    assert_eq!(
        ids(&read("teams", json!({"ws": app_ws(&w.fake)}), &state)),
        ["ENG", "OPS"]
    );
    assert_eq!(
        w.fake.token_fetches(),
        2,
        "a refused token is fetched again"
    );
    let token: Value = serde_json::from_str(&std::fs::read_to_string(&kept).unwrap()).unwrap();
    assert_eq!(token["token"], "lin_oauth_tok-2", "and kept in its place");
    assert_eq!(
        ids(&read("teams", json!({"ws": app_ws(&globex)}), &state)),
        ["ENG"]
    );
    assert!(state
        .path()
        .join(format!("token-{GLOBEX_CLIENT_ID}.json"))
        .is_file());
    assert_eq!(globex.token_fetches(), 1);
    end(&w.fake);
    end(&globex);
}

/// A kept token is fetched anew shortly before it expires: one that lasts 2 s is renewed
/// after 1 s, so a child 1.1 s later fetches a new one, and no request carries an expired
/// token.
#[test]
fn an_apps_token_is_fetched_anew_shortly_before_it_expires() {
    let w = world();
    w.fake.token_lifetime(2);
    let state = TempDir::new("reads-renew");
    assert_eq!(
        read("me", json!({"ws": app_ws(&w.fake)}), &state)["ok"],
        true
    );
    assert_eq!(
        read("me", json!({"ws": app_ws(&w.fake)}), &state)["ok"],
        true
    );
    assert_eq!(w.fake.token_fetches(), 1);
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        read("me", json!({"ws": app_ws(&w.fake)}), &state)["ok"],
        true
    );
    assert_eq!(w.fake.token_fetches(), 2, "renewed before it expired");
    end(&w.fake);
}

/// Two workspaces are two values used side by side: children reading one and then the other,
/// in turn, each answer from their own workspace with its own credential, and so does one
/// armed child reading both.
#[test]
fn two_workspaces_are_read_side_by_side() {
    let w = world();
    let globex = FakeLinear::start_globex();
    globex.team("GLX", "Globex — グロ");
    let state = TempDir::new("reads-two");
    for _ in 0..2 {
        assert_eq!(
            ids(&read("teams", with_ws(&w.fake, json!({})), &state)),
            ["ENG", "OPS"]
        );
        assert_eq!(
            ids(&read("teams", with_ws(&globex, json!({})), &state)),
            ["ENG", "GLX"]
        );
    }
    let mut plugin = common::Plugin::armed(json!({
        "team": "ENG", "base_url": w.fake.base_url(), "api_key": API_KEY,
        "pick_from": "Todo",
    }));
    let acme = plugin.read_call("teams", json!({"ws": ws(&w.fake)}));
    let other = plugin.read_call("teams", json!({"ws": ws(&globex)}));
    assert_eq!(ids(&acme), ["ENG", "OPS"]);
    assert_eq!(ids(&other), ["ENG", "GLX"]);
    assert_ne!(acme["handles"][0]["key"], other["handles"][0]["key"]);
    let stderr = plugin.finish();
    assert!(
        stderr.contains("[linear] read 2 teams from workspace globex\n"),
        "{stderr}"
    );
    for (fake, key) in [(&w.fake, API_KEY), (&globex, GLOBEX_API_KEY)] {
        assert!(
            fake.seen()
                .iter()
                .all(|s| s.authorization.as_deref() == Some(key)),
            "each workspace saw only its own credential"
        );
    }
    end(&w.fake);
    end(&globex);
}

/// A handle's key is `linear:<organization uuid>/<object uuid>` for an issue, a team and a
/// comment alike, so the issue read again after it moved team — its identifier changed, its
/// uuid not — carries the same key, which is what `==` on two handles compares.
#[test]
fn a_handles_key_holds_after_its_identifier_changes() {
    let w = world();
    let f = &w.fake;
    let e1 = w.issue(101);
    let said = f.comment_at(e1, Author::User(w.phil.clone()), "Still red 🚨", T0, None);
    let state = TempDir::new("reads-key");
    let before = read("issues", with_ws(f, json!({"id": [e1]})), &state)["handles"][0].clone();
    assert_eq!(before["id"], "ENG-101");
    assert_eq!(before["key"], format!("linear:{}/{e1}", f.org_id()));
    let comment = read(
        "comments",
        with_ws(f, json!({"issue": before.clone()})),
        &state,
    )["handles"][0]
        .clone();
    assert_eq!(comment["key"], format!("linear:{}/{said}", f.org_id()));
    f.move_team(e1, "OPS");
    let after = read("issues", with_ws(f, json!({"id": [e1]})), &state)["handles"][0].clone();
    let moved = f.identifier(e1);
    assert!(moved.starts_with("OPS-") && moved != "OPS-101", "{moved}");
    assert_eq!(after["id"], moved.as_str());
    assert_eq!(after["key"], before["key"], "the key holds");
    assert_eq!(after["fields"]["team"], "OPS");
    assert_eq!(
        ids(&read(
            "issues",
            with_ws(f, json!({"id": ["ENG-101"]})),
            &state
        )),
        Vec::<String>::new(),
        "the old identifier names nothing"
    );
    let again = read(
        "comments",
        with_ws(f, json!({"issue": {"id": moved, "key": before["key"]}})),
        &state,
    );
    assert_eq!(ids(&again), [said]);
    end(f);
}
