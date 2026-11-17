//! The workspace writes suite: the real `afkd-linear` exec, sent each write as afkd sends one
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

use serde_json::{json, Value};

use common::fake::{now_ms, png, Author, FakeLinear};
use common::{once, ws, TempDir};

/// The title of the issue every field is set on: wide CJK, an emoji and a quote.
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "The client retries forever.\n\n```rust\nlet backoff = 0;\n```";

/// Two teams of one workspace — `ENG` with four states and two cycles, `OPS`, which allows an
/// estimate of 0, with two states and one cycle — a team label of each, a workspace label, a
/// project of both teams and one of `ENG` alone, each with milestones, and five issues:
///
/// - ENG-101, ENG Todo: every field set — urgent, estimate 3, due 2026-12-24, phil's,
///   delegated to the app, in the project of both teams and its milestone B, cycle 12, a
///   sub-issue of ENG-102, labelled `Bug 🐛` (ENG's) and `Needs Info` (the workspace's);
/// - ENG-102 and ENG-103, ENG Todo, below it in that order;
/// - ENG-104, ENG In Progress;
/// - ENG-105, ENG Done;
/// - OPS-106, OPS Todo.
struct World {
    fake: FakeLinear,
    /// The `OPS` team's id.
    ops: String,
    /// ENG-101 to ENG-105, then OPS-106, by id.
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
    let ops = fake.team("OPS", "Operations — 運用");
    fake.allow_zero_estimates("OPS");
    for (state, kind) in [
        ("Triage", "triage"),
        ("Todo", "unstarted"),
        ("In Progress", "started"),
        ("Done", "completed"),
    ] {
        fake.state(state, kind);
    }
    for (state, kind) in [("Todo", "unstarted"), ("Done", "completed")] {
        fake.team_state("OPS", state, kind);
    }
    let phil = fake.user("Phil Ek", "phil", "phil@acme.example");
    fake.user("陳大文", "chen", "chen@acme.example");
    fake.label_of("ENG", "Bug 🐛", None);
    fake.label_of("OPS", "Infra", None);
    fake.workspace_label("Needs Info", "#f2c94c");
    let trackers = fake.project(
        "Trackers 🧭",
        &["ENG", "OPS"],
        &[("C — writes", 30.0), ("B — reads", 20.0)],
    );
    fake.project("Billing", &["ENG"], &[("M1", 1.0)]);
    let twelve = fake.cycle("ENG", 12, Some("Sprint 12 — 冬"), 0, 1, true);
    fake.cycle("ENG", 13, None, 1, 2, false);
    fake.cycle("OPS", 3, None, 0, 1, true);
    let issues: Vec<String> = [
        ("ENG", "Todo", TITLE, BODY),
        (
            "ENG",
            "Todo",
            "Cap the backoff at 30s",
            "Linear's header is in seconds.",
        ),
        ("ENG", "Todo", "Read Retry-After", ""),
        ("ENG", "In Progress", "Upgrade ureq", ""),
        ("ENG", "Done", "Rewrite in Go", ""),
        ("OPS", "Todo", "Rotate the deploy keys 🔑", ""),
    ]
    .iter()
    .map(|(team, state, title, body)| fake.team_issue(team, state, title, body))
    .collect();
    let e1 = &issues[0];
    fake.set_priority(e1, 1);
    fake.set_estimate(e1, 3.0);
    fake.set_due(e1, "2026-12-24");
    fake.assign(e1, &phil);
    fake.delegate(e1, &fake.app_user());
    fake.set_project(e1, &trackers, Some("B — reads"));
    fake.set_cycle(e1, &twelve);
    fake.set_parent(e1, &issues[1]);
    fake.label(e1, "Bug 🐛");
    fake.put_label(e1, &label_id(&fake, "Needs Info"));
    World { fake, ops, issues }
}

/// The id of the label named `name`, read as `labels` reads it.
fn label_id(fake: &FakeLinear, name: &str) -> String {
    let state = TempDir::new("writes-label");
    let (reply, _) = once("labels", json!({"ws": ws(fake)}), state.path());
    reply["handles"]
        .as_array()
        .expect("labels")
        .iter()
        .find(|l| l["fields"]["name"] == name)
        .map(|l| l["id"].as_str().expect("an id").to_string())
        .expect("the label")
}

/// The handle `issues` reads of the issue `issue`, by identifier or uuid.
fn read_issue(fake: &FakeLinear, issue: &str, state: &TempDir) -> Value {
    let (reply, _) = once(
        "issues",
        json!({"ws": ws(fake), "id": [issue]}),
        state.path(),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    reply["handles"][0].clone()
}

/// `update_issue` on `handle` with `args` beside the workspace reaching `fake`: the reply, and
/// what the child wrote to stderr.
fn update(fake: &FakeLinear, handle: &Value, args: Value, state: &TempDir) -> (Value, String) {
    write(fake, "update_issue", handle, args, state)
}

/// The write `action` on `handle` with `args` beside the workspace reaching `fake`: the reply,
/// and what the child wrote to stderr.
fn write(
    fake: &FakeLinear,
    action: &str,
    handle: &Value,
    args: Value,
    state: &TempDir,
) -> (Value, String) {
    let mut args = args;
    args["ws"] = ws(fake);
    args["issue"] = handle_of(handle);
    once(action, args, state.path())
}

/// The writes the fake was sent, other than `IssueWrite`, each one's operation and variables,
/// in order.
fn changes_seen(fake: &FakeLinear) -> Vec<(String, Value)> {
    const CHANGES: &[&str] = &[
        "RelationCreate",
        "RelationDelete",
        "LinkUrl",
        "LinkUpdate",
        "LinkDelete",
        "IssueSubscribe",
        "IssueUnsubscribe",
        "IssueArchive",
        "IssueUnarchive",
        "IssueDelete",
    ];
    fake.seen()
        .into_iter()
        .filter(|seen| CHANGES.contains(&seen.operation.as_str()))
        .map(|seen| (seen.operation, seen.variables))
        .collect()
}

/// A handle as afkd passes one to an action: its id and its key.
fn handle_of(handle: &Value) -> Value {
    json!({"id": handle["id"], "key": handle["key"]})
}

/// The `IssueWrite` inputs the fake was sent, in order.
fn writes_seen(fake: &FakeLinear) -> Vec<Value> {
    fake.seen()
        .into_iter()
        .filter(|seen| seen.operation == "IssueWrite")
        .map(|seen| seen.variables["input"].clone())
        .collect()
}

/// The fields of `after` that differ from `before`'s, by name, sorted.
fn changed(before: &Value, after: &Value) -> Vec<String> {
    let (before, after) = (
        before["fields"].as_object().expect("fields"),
        after["fields"].as_object().expect("fields"),
    );
    let mut changed: Vec<String> = after
        .iter()
        .filter(|(name, value)| before.get(*name) != Some(value))
        .map(|(name, _)| name.clone())
        .collect();
    changed.sort();
    changed
}

/// End a leg: every request the plugin sent was one Linear would take.
fn end(fake: &FakeLinear) {
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests Linear's schema refuses"
    );
}

/// A refusal's reply, `ok:false` with the sentence, and the sentence diagnosed on stderr.
fn refused(reply: &(Value, String), sentence: &str) {
    assert_eq!(
        reply.0,
        json!({"ok": false, "error": sentence}),
        "stderr: {}",
        reply.1
    );
    assert!(reply.1.contains(sentence), "{}", reply.1);
}

/// `title` alone changes the title, and nothing else but what Linear makes of a title — the
/// url's slug and the branch name — and the time it changed: the reply is the issue as
/// `issues` then reads it, under the same key, and the fake was sent the title and no other
/// field.
#[test]
fn update_issue_leaves_out_what_is_not_given() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-leave");
    let before = read_issue(f, "ENG-101", &state);
    let (reply, stderr) = update(
        f,
        &before,
        json!({"title": "修复 the retry storm — \"backoff\" 🚦"}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let after = read_issue(f, "ENG-101", &state);
    assert_eq!(reply["handle"], after, "the reply is what Linear now holds");
    assert_eq!(after["key"], before["key"]);
    assert_eq!(
        changed(&before, &after),
        ["branch", "title", "updated_at", "url"]
    );
    assert_eq!(
        after["fields"]["title"],
        "修复 the retry storm — \"backoff\" 🚦"
    );
    assert_eq!(
        writes_seen(f),
        [json!({"title": "修复 the retry storm — \"backoff\" 🚦"})]
    );
    assert_eq!(stderr, "[linear] updated ENG-101 in workspace acme\n");
    end(f);
}

/// `nil` clears each field that can hold nothing — the estimate, the due date, the assignee
/// and the delegate, the project with its milestone, the cycle and the parent — in one write
/// carrying `null` for each, and leaves the rest.
#[test]
fn update_issue_clears_each_nil() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-clear");
    let before = read_issue(f, "ENG-101", &state);
    let (reply, _) = update(
        f,
        &before,
        json!({"estimate": null, "due": null, "assignee": null, "delegate": null,
               "project": null, "milestone": null, "cycle": null, "parent": null}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let fields = &reply["handle"]["fields"];
    for field in [
        "estimate",
        "due",
        "assignee",
        "delegate",
        "project",
        "milestone",
        "cycle",
        "parent",
    ] {
        assert_eq!(fields[field], Value::Null, "{field}");
    }
    assert_eq!(fields["title"], TITLE);
    assert_eq!(fields["priority"], "urgent");
    assert_eq!(fields["labels"], json!(["Bug 🐛", "Needs Info"]));
    assert_eq!(
        writes_seen(f),
        [
            json!({"estimate": null, "dueDate": null, "assigneeId": null, "delegateId": null,
                "projectId": null, "projectMilestoneId": null, "cycleId": null,
                "parentId": null})
        ]
    );
    assert_eq!(f.parent_of(w.issue(101)), None);
    end(f);
}

/// Every field set at once lands: a title, an emptied description, a state, a priority, an
/// estimate, a due date, the assignee by email, the delegate as `"me"`, a project and its
/// milestone, a cycle and a parent; the reply is the issue as `issues` then reads it.
#[test]
fn update_issue_sets_every_field() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-every");
    let issue = read_issue(f, "ENG-102", &state);
    let parent = read_issue(f, "ENG-103", &state);
    let (reply, stderr) = update(
        f,
        &issue,
        json!({"title": "Cap it — 30 s ⏱", "description": "", "state": "In Progress",
               "priority": "high", "estimate": 5, "due": "2027-01-31",
               "assignee": "chen@acme.example", "delegate": "me", "project": "Billing",
               "milestone": "M1", "cycle": 13, "parent": handle_of(&parent)}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let after = read_issue(f, "ENG-102", &state);
    assert_eq!(reply["handle"], after);
    let fields = &after["fields"];
    for (field, value) in [
        ("title", json!("Cap it — 30 s ⏱")),
        ("description", json!("")),
        ("state", json!("In Progress")),
        ("state_type", json!("started")),
        ("priority", json!("high")),
        ("estimate", json!(5)),
        ("due", json!("2027-01-31")),
        ("assignee", json!("chen")),
        ("delegate", json!("afkd")),
        ("project", json!("Billing")),
        ("milestone", json!("M1")),
        ("cycle", json!(13)),
        ("parent", json!("ENG-103")),
    ] {
        assert_eq!(fields[field], value, "{field}");
    }
    assert_eq!(f.description_of(w.issue(102)), "");
    assert_eq!(writes_seen(f).len(), 1, "one write");
    assert_eq!(stderr, "[linear] updated ENG-102 in workspace acme\n");
    end(f);
}

/// An estimate of 0 lands on a team that allows one, and reads back as 0, not `nil`; on a team
/// that does not, Linear's refusal is relayed.
#[test]
fn estimate_zero_holds_on_a_team_that_allows_it() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-zero");
    let ops = read_issue(f, "OPS-106", &state);
    let (reply, _) = update(f, &ops, json!({"estimate": 0}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"]["fields"]["estimate"], 0);
    assert_eq!(read_issue(f, "OPS-106", &state)["fields"]["estimate"], 0);
    let eng = read_issue(f, "ENG-101", &state);
    let reply = update(f, &eng, json!({"estimate": 0}), &state);
    refused(
        &reply,
        "linear update issue: Linear refused it (200): Invalid estimate: the team does not \
         use zero estimates",
    );
    assert_eq!(read_issue(f, "ENG-101", &state)["fields"]["estimate"], 3);
    end(f);
}

/// `team` with a `state` of that team moves the issue there: a new identifier, the new team's
/// state, its old team's label and its cycle dropped, the workspace's label and a project of
/// both teams kept, and the same key, so it is the same handle.
#[test]
fn team_with_state_moves_the_issue() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-move");
    let before = read_issue(f, "ENG-101", &state);
    let (reply, stderr) = update(f, &before, json!({"team": "OPS", "state": "Todo"}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    let handle = &reply["handle"];
    assert_eq!(handle["id"], "OPS-107");
    assert_eq!(handle["key"], before["key"]);
    let fields = &handle["fields"];
    assert_eq!(fields["uuid"], w.issue(101));
    assert_eq!(fields["team"], "OPS");
    assert_eq!(fields["state"], "Todo");
    assert_eq!(fields["labels"], json!(["Needs Info"]));
    assert_eq!(fields["cycle"], Value::Null);
    assert_eq!(fields["project"], "Trackers 🧭");
    assert_eq!(fields["milestone"], "B — reads");
    assert_eq!(*handle, read_issue(f, w.issue(101), &state));
    assert_eq!(
        stderr,
        "[linear] moved ENG-101 to OPS-107 in workspace acme\n"
    );
    let input = &writes_seen(f)[0];
    assert_eq!(input["teamId"], w.ops);
    end(f);
}

/// Each of `team` alone, `files` alone, `at` beside `after` and a file that is not absolute is
/// refused in its own sentence before any request: the fake saw none.
#[test]
fn what_cannot_be_written_is_refused_before_any_request() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-before");
    let handle = json!({"id": "ENG-101", "key": format!("linear:{}/{}", f.org_id(), w.issue(101))});
    let shot = state.path().join("shot.png");
    std::fs::write(&shot, png()).unwrap();
    let seen = f.seen().len();
    for (args, sentence) in [
        (
            json!({"team": "OPS"}),
            "update_issue: team OPS needs a state of team OPS beside it",
        ),
        (
            json!({"files": [shot], "title": "t"}),
            "update_issue: files go only beside description",
        ),
        (
            json!({"at": "top", "after": handle}),
            "update_issue: at and after cannot both be given",
        ),
        (
            json!({"files": ["shot.png"], "description": "d"}),
            "update_issue: files must be absolute, not \"shot.png\"",
        ),
    ] {
        let mut args = args;
        args["ws"] = ws(f);
        args["issue"] = handle.clone();
        refused(&once("update_issue", args, state.path()), sentence);
    }
    assert_eq!(f.seen().len(), seen, "no request: {:?}", &f.seen()[seen..]);
    end(f);
}

/// `at` places the issue above the first or below the last of its state, and `after` right
/// below another, mid-state or last; with a new state, in that state. An `after` in another
/// state is refused, as is the issue itself.
#[test]
fn at_and_after_place_the_issue() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-place");
    let todo = || f.in_state("Todo");
    let (e1, e2, e3, e4, e5) = (
        w.issue(101),
        w.issue(102),
        w.issue(103),
        w.issue(104),
        w.issue(105),
    );
    assert_eq!(todo(), [e1, e2, e3]);
    let handle = |id: &str| handle_of(&read_issue(f, id, &state));
    let place = |id: &str, args: Value| {
        let mut args = args;
        args["ws"] = ws(f);
        args["issue"] = handle(id);
        let (reply, _) = once("update_issue", args, state.path());
        assert_eq!(reply["ok"], true, "{reply}");
    };
    place(e3, json!({"at": "top"}));
    assert_eq!(todo(), [e3, e1, e2]);
    place(e3, json!({"at": "bottom"}));
    assert_eq!(todo(), [e1, e2, e3]);
    place(e3, json!({"after": handle(e1)}));
    assert_eq!(todo(), [e1, e3, e2], "after one mid-state");
    place(e1, json!({"after": handle(e2)}));
    assert_eq!(todo(), [e3, e2, e1], "after the last");
    place(e4, json!({"state": "Todo", "at": "top"}));
    assert_eq!(todo(), [e4, e3, e2, e1]);
    place(e5, json!({"state": "Todo", "after": handle(e3)}));
    assert_eq!(todo(), [e4, e3, e5, e2, e1]);
    assert!(f.in_state("In Progress").is_empty() && f.in_state("Done").is_empty());
    let mut args = json!({"ws": ws(f), "issue": handle(e1), "state": "Done", "after": handle(e2)});
    refused(
        &once("update_issue", args.clone(), state.path()),
        "update_issue: after ENG-102 is in Todo of ENG, not Done of ENG",
    );
    args["after"] = handle(e1);
    refused(
        &once("update_issue", args, state.path()),
        "update_issue: after is ENG-101 itself",
    );
    assert_eq!(todo(), [e4, e3, e5, e2, e1], "nothing moved");
    end(f);
}

/// A name that names nothing is refused naming what there is — a state of the issue's team, a
/// team, a user, a project, a milestone of the issue's project and a cycle of its team — and
/// nothing is written.
#[test]
fn an_unknown_name_is_refused_naming_what_there_is() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-unknown");
    let issue = read_issue(f, "ENG-101", &state);
    for (args, sentence) in [
        (
            json!({"state": "Doing"}),
            "update_issue: no state named \"Doing\" in team ENG; there are: Triage, Todo, In \
             Progress, Done",
        ),
        (
            json!({"team": "QA", "state": "Todo"}),
            "update_issue: no team named \"QA\"; there are: ENG, OPS",
        ),
        (
            json!({"assignee": "nobody@acme.example"}),
            "update_issue: no user named \"nobody@acme.example\"; there are: afkd, afkd-app, \
             phil, chen",
        ),
        (
            json!({"project": "Trackers"}),
            "update_issue: no project named \"Trackers\"; there are: Trackers 🧭, Billing",
        ),
        (
            json!({"milestone": "A — 草案"}),
            "update_issue: no milestone named \"A — 草案\" in project Trackers 🧭; there are: B \
             — reads, C — writes",
        ),
        (
            json!({"project": "Billing", "milestone": "B — reads"}),
            "update_issue: no milestone named \"B — reads\" in project Billing; there are: M1",
        ),
        (
            json!({"cycle": 3}),
            "update_issue: no cycle 3 in team ENG; there are: 12, 13",
        ),
        (
            json!({"team": "OPS", "state": "Todo", "cycle": 12}),
            "update_issue: no cycle 12 in team OPS; there are: 3",
        ),
    ] {
        refused(&update(f, &issue, args, &state), sentence);
    }
    let bare = read_issue(f, "ENG-103", &state);
    refused(
        &update(f, &bare, json!({"milestone": "M1"}), &state),
        "update_issue: milestone \"M1\" needs a project to be in, and ENG-103 is in none",
    );
    assert_eq!(writes_seen(f), Vec::<Value>::new(), "nothing written");
    assert_eq!(read_issue(f, "ENG-101", &state), issue);
    end(f);
}

/// A name several objects answer to is refused listing their ids — two users of one display
/// name, two states of one name in a team — and either id then lands in the name's place.
#[test]
fn an_ambiguous_name_lists_the_ids_and_the_id_stands_in() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-ambiguous");
    let sam = f.user("Sam Lee", "sam", "sam@acme.example");
    let sam_k = f.user("Sam Kim", "sam", "sam.k@acme.example");
    let first = f.state("Review", "started");
    let second = f.state("Review", "started");
    let issue = read_issue(f, "ENG-103", &state);
    refused(
        &update(f, &issue, json!({"assignee": "sam"}), &state),
        &format!(
            "update_issue: 2 users are named \"sam\": {sam}, {sam_k}; pass the id of the one \
             you mean in place of the name"
        ),
    );
    refused(
        &update(f, &issue, json!({"state": "Review"}), &state),
        &format!(
            "update_issue: 2 states are named \"Review\" in team ENG: {first}, {second}; pass \
             the id of the one you mean in place of the name"
        ),
    );
    assert_eq!(writes_seen(f), Vec::<Value>::new());
    let (reply, _) = update(
        f,
        &issue,
        json!({"assignee": sam_k, "state": second}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"]["fields"]["assignee"], "sam");
    assert_eq!(reply["handle"]["fields"]["state"], "Review");
    assert_eq!(
        writes_seen(f),
        [json!({"stateId": second, "assigneeId": sam_k})]
    );
    let (reply, _) = update(f, &issue, json!({"assignee": "sam.k@acme.example"}), &state);
    assert_eq!(reply["ok"], true, "an email names one: {reply}");
    end(f);
}

/// An issue of another workspace is refused as the issue of every write, as `after` and as
/// `parent`, and as a relation's `other`, each naming both workspaces, and nothing is written
/// to either.
#[test]
fn a_handle_of_another_workspace_is_refused() {
    let w = world();
    let globex = FakeLinear::start_globex();
    globex.state("Todo", "unstarted");
    globex.team_issue("ENG", "Todo", "Globex's own", "");
    let state = TempDir::new("writes-other");
    let theirs = handle_of(&read_issue(&globex, "ENG-101", &state));
    let ours = handle_of(&read_issue(&w.fake, "ENG-102", &state));
    for (issue, args, param) in [
        (&theirs, json!({"title": "x"}), "issue"),
        (&ours, json!({"after": theirs}), "after"),
        (&ours, json!({"parent": theirs}), "parent"),
    ] {
        let mut args = args;
        args["ws"] = ws(&w.fake);
        args["issue"] = issue.clone();
        refused(
            &once("update_issue", args, state.path()),
            &format!(
                "update_issue: {param} is ENG-101 of workspace globex, and the linear.Workspace \
                 reaches acme"
            ),
        );
    }
    let relation = |other: &Value| json!({"relation": "blocks", "other": other});
    for (action, issue, args, param) in [
        ("add_relation", &theirs, relation(&ours), "issue"),
        ("add_relation", &ours, relation(&theirs), "other"),
        ("remove_relation", &theirs, relation(&ours), "issue"),
        ("remove_relation", &ours, relation(&theirs), "other"),
        (
            "add_link",
            &theirs,
            json!({"url": "https://acme.example"}),
            "issue",
        ),
        (
            "remove_link",
            &theirs,
            json!({"url": "https://acme.example"}),
            "issue",
        ),
        ("add_subscriber", &theirs, json!({}), "issue"),
        ("remove_subscriber", &theirs, json!({"user": "me"}), "issue"),
        ("archive_issue", &theirs, json!({}), "issue"),
        ("unarchive_issue", &theirs, json!({}), "issue"),
        ("delete_issue", &theirs, json!({}), "issue"),
    ] {
        let mut args = args;
        args["ws"] = ws(&w.fake);
        args["issue"] = issue.clone();
        refused(
            &once(action, args, state.path()),
            &format!(
                "{action}: {param} is ENG-101 of workspace globex, and the linear.Workspace \
                 reaches acme"
            ),
        );
    }
    assert_eq!(writes_seen(&w.fake), Vec::<Value>::new());
    assert_eq!(writes_seen(&globex), Vec::<Value>::new());
    assert_eq!(changes_seen(&w.fake), Vec::<(String, Value)>::new());
    assert_eq!(changes_seen(&globex), Vec::<(String, Value)>::new());
    end(&w.fake);
    end(&globex);
}

/// `files` beside a description put each file up and embed it after the text, each on a
/// paragraph of its own: an image shown, any other file linked.
#[test]
fn update_issue_embeds_files_after_the_description() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-files");
    let dir = TempDir::new("writes-files-src");
    let shot = dir.path().join("screen shot — 修复.png");
    let log = dir.path().join("run.log");
    std::fs::write(&shot, png()).unwrap();
    std::fs::write(&log, "retry 1\nretry 2\n").unwrap();
    let issue = read_issue(f, "ENG-103", &state);
    let text = "## Steps\n\n1. run it\n2. watch it retry 🚨";
    let (reply, _) = update(
        f,
        &issue,
        json!({"description": text, "files": [shot, log]}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let uploads = f.uploads();
    assert_eq!(uploads.len(), 2);
    assert!(uploads.iter().all(|u| u.landed), "{uploads:?}");
    assert_eq!(f.puts().len(), 2);
    let description = f.description_of(w.issue(103));
    assert_eq!(
        description,
        format!(
            "{text}\n\n![screen shot — 修复.png]({})\n\n[run.log]({})",
            uploads[0].asset_url, uploads[1].asset_url
        )
    );
    assert_eq!(reply["handle"]["fields"]["description"], description);
    assert_eq!(f.file(&uploads[0].asset_url), Some(png()));
    let (files, _) = once(
        "files",
        json!({"ws": ws(f), "issue": handle_of(&issue)}),
        state.path(),
    );
    assert_eq!(
        files["value"],
        json!([uploads[0].asset_url, uploads[1].asset_url]),
        "the files read back as the issue's"
    );
    end(f);
}

/// The other end of each relation: the field of `other` that holds the issue once the issue's
/// field `relation` holds `other`.
fn mirror(relation: &str) -> &'static str {
    match relation {
        "blocks" => "blocked_by",
        "blocked_by" => "blocks",
        "related" => "related",
        "similar" => "similar",
        "duplicate_of" => "duplicates",
        "duplicates" => "duplicate_of",
        other => panic!("no relation {other}"),
    }
}

/// Whether the field `field` of a handle's `fields` holds the identifier `id`: a list that
/// has it, or `duplicate_of`, which is one identifier or `nil`.
fn holds(fields: &Value, field: &str, id: &str) -> bool {
    match &fields[field] {
        Value::Array(ids) => ids.iter().any(|held| held == id),
        held => held == id,
    }
}

/// For each member of `relation`, `add_relation` puts the other issue's identifier in the
/// issue's field of that name and the issue's in the other's matching one, by Linear's
/// relation of the right kind held by the right end, and `remove_relation` takes both out;
/// each gives back the issue as `issues` then reads it.
#[test]
fn add_and_remove_relation_for_each_member() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-relations");
    let (e3, e4) = (w.issue(103), w.issue(104));
    for (relation, kind, held_by_issue) in [
        ("blocks", "blocks", true),
        ("blocked_by", "blocks", false),
        ("related", "related", true),
        ("similar", "similar", true),
        ("duplicate_of", "duplicate", true),
        ("duplicates", "duplicate", false),
    ] {
        let issue = read_issue(f, "ENG-103", &state);
        let other = handle_of(&read_issue(f, "ENG-104", &state));
        let args = json!({"relation": relation, "other": other});
        let (reply, stderr) = write(f, "add_relation", &issue, args.clone(), &state);
        assert_eq!(reply["ok"], true, "{relation}: {reply}");
        assert_eq!(
            reply["handle"],
            read_issue(f, "ENG-103", &state),
            "{relation}"
        );
        assert!(
            holds(&reply["handle"]["fields"], relation, "ENG-104"),
            "{relation}: {reply}"
        );
        let fields = read_issue(f, "ENG-104", &state)["fields"].clone();
        assert!(
            holds(&fields, mirror(relation), "ENG-103"),
            "{relation}: {fields}"
        );
        let (from, to) = if held_by_issue { (e3, e4) } else { (e4, e3) };
        assert_eq!(
            changes_seen(f).last(),
            Some(&(
                "RelationCreate".to_string(),
                json!({"input": {"issueId": from, "relatedIssueId": to, "type": kind}})
            )),
            "{relation}"
        );
        assert_eq!(
            stderr,
            format!("[linear] added ENG-104 to the {relation} of ENG-103 in workspace acme\n")
        );
        let (reply, stderr) = write(f, "remove_relation", &issue, args, &state);
        assert_eq!(reply["ok"], true, "{relation}: {reply}");
        assert_eq!(
            reply["handle"],
            read_issue(f, "ENG-103", &state),
            "{relation}"
        );
        assert!(
            !holds(&reply["handle"]["fields"], relation, "ENG-104"),
            "{relation}"
        );
        let fields = read_issue(f, "ENG-104", &state)["fields"].clone();
        assert!(
            !holds(&fields, mirror(relation), "ENG-103"),
            "{relation}: {fields}"
        );
        assert_eq!(
            changes_seen(f).last().map(|c| c.0.as_str()),
            Some("RelationDelete")
        );
        assert_eq!(
            stderr,
            format!("[linear] removed ENG-104 from the {relation} of ENG-103 in workspace acme\n")
        );
        assert_eq!(f.relation_count(), 0, "{relation}");
    }
    end(f);
}

/// A relation that reads the same from either end is removed whichever issue holds it, and
/// one held both ways goes both ways.
#[test]
fn a_symmetric_relation_is_removed_from_either_side() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-symmetric");
    let (e3, e4) = (w.issue(103), w.issue(104));
    f.relate(e4, e3, "related");
    f.relate(e3, e4, "similar");
    f.relate(e4, e3, "similar");
    let issue = read_issue(f, "ENG-103", &state);
    let other = handle_of(&read_issue(f, "ENG-104", &state));
    assert_eq!(issue["fields"]["related"], json!(["ENG-104"]));
    assert_eq!(issue["fields"]["similar"], json!(["ENG-104"]));
    for relation in ["related", "similar"] {
        let args = json!({"relation": relation, "other": other});
        let (reply, _) = write(f, "remove_relation", &issue, args, &state);
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["handle"]["fields"][relation], json!([]), "{relation}");
        assert_eq!(
            read_issue(f, "ENG-104", &state)["fields"][relation],
            json!([])
        );
    }
    assert_eq!(f.relation_count(), 0);
    end(f);
}

/// Adding a relation the issue already holds, from either end where it reads the same both
/// ways, and removing one it does not hold, send Linear nothing and give back the issue.
#[test]
fn adding_a_held_relation_and_removing_a_missing_one_send_nothing() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-held");
    let (e3, e4) = (w.issue(103), w.issue(104));
    f.relate(e3, e4, "blocks");
    f.relate(e4, e3, "related");
    f.relate(e4, e3, "duplicate");
    let issue = read_issue(f, "ENG-103", &state);
    let other = handle_of(&read_issue(f, "ENG-104", &state));
    for (action, relation, said) in [
        (
            "add_relation",
            "blocks",
            "ENG-104 already in the blocks of ENG-103",
        ),
        (
            "add_relation",
            "related",
            "ENG-104 already in the related of ENG-103",
        ),
        (
            "add_relation",
            "duplicates",
            "ENG-104 already in the duplicates of ENG-103",
        ),
        (
            "remove_relation",
            "similar",
            "ENG-104 not in the similar of ENG-103",
        ),
        (
            "remove_relation",
            "blocked_by",
            "ENG-104 not in the blocked_by of ENG-103",
        ),
        (
            "remove_relation",
            "duplicate_of",
            "ENG-104 not in the duplicate_of of ENG-103",
        ),
    ] {
        let args = json!({"relation": relation, "other": other});
        let (reply, stderr) = write(f, action, &issue, args, &state);
        assert_eq!(reply["ok"], true, "{action} {relation}: {reply}");
        assert_eq!(reply["handle"], issue, "{action} {relation}");
        assert_eq!(stderr, format!("[linear] {said} in workspace acme\n"));
    }
    assert_eq!(
        changes_seen(f),
        Vec::<(String, Value)>::new(),
        "nothing sent"
    );
    assert_eq!(f.relation_count(), 3);
    end(f);
}

/// An issue is not related to itself, whether passed as one handle twice or as a read's
/// handle and the same issue's under another identifier, and nothing is sent.
#[test]
fn a_relation_to_itself_is_refused() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-itself");
    let issue = read_issue(f, "ENG-103", &state);
    for action in ["add_relation", "remove_relation"] {
        let args = json!({"relation": "blocks", "other": handle_of(&issue)});
        refused(
            &write(f, action, &issue, args, &state),
            &format!("{action}: other is ENG-103 itself"),
        );
    }
    f.move_team(w.issue(103), "OPS");
    let moved = handle_of(&read_issue(f, w.issue(103), &state));
    assert_eq!(moved["id"], "OPS-107");
    let args = json!({"relation": "related", "other": moved});
    refused(
        &write(f, "add_relation", &issue, args, &state),
        "add_relation: other is ENG-103 itself",
    );
    assert_eq!(changes_seen(f), Vec::<(String, Value)>::new());
    end(f);
}

/// `add_link` links a url on the issue, titled as given or, with no title, as Linear titles
/// it; adding a url it links again retitles that link, or with no title leaves it be. The
/// issue's `links` read back holds each url once.
#[test]
fn add_link_links_and_a_second_add_updates_its_title() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-link");
    let e3 = w.issue(103);
    let issue = read_issue(f, "ENG-103", &state);
    let pr = "https://github.com/acme/widgets/pull/42?tab=files#diff-修复";
    let log = "https://ci.acme.example/runs/9001/log.txt";
    let (reply, stderr) = write(
        f,
        "add_link",
        &issue,
        json!({"url": pr, "title": "PR #42 — \"backoff\" 🚦"}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"], read_issue(f, "ENG-103", &state));
    assert_eq!(reply["handle"]["fields"]["links"], json!([pr]));
    assert_eq!(
        f.links_of(e3),
        [(pr.into(), "PR #42 — \"backoff\" 🚦".into())]
    );
    assert_eq!(
        stderr,
        format!("[linear] linked {pr} on ENG-103 in workspace acme\n")
    );
    let (reply, stderr) = write(
        f,
        "add_link",
        &issue,
        json!({"url": pr, "title": "PR 42"}),
        &state,
    );
    assert_eq!(reply["handle"]["fields"]["links"], json!([pr]), "{reply}");
    assert_eq!(f.links_of(e3), [(pr.into(), "PR 42".into())]);
    assert_eq!(
        stderr,
        format!("[linear] retitled the link to {pr} on ENG-103 in workspace acme\n")
    );
    let (reply, stderr) = write(f, "add_link", &issue, json!({"url": pr}), &state);
    assert_eq!(reply["handle"]["fields"]["links"], json!([pr]), "{reply}");
    assert_eq!(f.links_of(e3), [(pr.into(), "PR 42".into())], "left be");
    assert_eq!(
        stderr,
        format!("[linear] {pr} already linked on ENG-103 in workspace acme\n")
    );
    let (reply, _) = write(f, "add_link", &issue, json!({"url": log}), &state);
    assert_eq!(
        reply["handle"]["fields"]["links"],
        json!([pr, log]),
        "{reply}"
    );
    assert_eq!(f.links_of(e3)[1], (log.into(), log.into()));
    let sent: Vec<String> = changes_seen(f).into_iter().map(|c| c.0).collect();
    assert_eq!(sent, ["LinkUrl", "LinkUpdate", "LinkUrl"]);
    end(f);
}

/// `remove_link` takes the link to a url off the issue, and of a url it does not link sends
/// nothing and gives back the issue.
#[test]
fn remove_link_unlinks_and_a_missing_one_is_ok() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-unlink");
    let e3 = w.issue(103);
    let pr = "https://github.com/acme/widgets/pull/42";
    let log = "https://ci.acme.example/runs/9001/log.txt";
    f.link(e3, pr);
    f.link(e3, log);
    let issue = read_issue(f, "ENG-103", &state);
    let (reply, stderr) = write(f, "remove_link", &issue, json!({"url": pr}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"], read_issue(f, "ENG-103", &state));
    assert_eq!(reply["handle"]["fields"]["links"], json!([log]));
    assert_eq!(
        stderr,
        format!("[linear] unlinked {pr} from ENG-103 in workspace acme\n")
    );
    let (reply, stderr) = write(f, "remove_link", &issue, json!({"url": pr}), &state);
    assert_eq!(reply["handle"]["fields"]["links"], json!([log]), "{reply}");
    assert_eq!(
        stderr,
        format!("[linear] {pr} not linked on ENG-103 in workspace acme\n")
    );
    let sent: Vec<String> = changes_seen(f).into_iter().map(|c| c.0).collect();
    assert_eq!(sent, ["LinkDelete"]);
    end(f);
}

/// `add_subscriber` subscribes whoever the credential acts as when no user is named, or `"me"`
/// is, and a user by display name or email; `remove_subscriber` likewise. A user named by no
/// one, or by several, is refused before anything is sent.
#[test]
fn add_subscriber_defaults_to_me_and_takes_a_name() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-subscribe");
    let e3 = w.issue(103);
    let chen = f.user("陳大文", "陳大文", "dawen@acme.example");
    let issue = read_issue(f, "ENG-103", &state);
    let (reply, stderr) = write(f, "add_subscriber", &issue, json!({}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"], read_issue(f, "ENG-103", &state));
    assert_eq!(reply["handle"]["fields"]["subscribers"], json!(["afkd"]));
    assert_eq!(
        stderr,
        "[linear] subscribed me to ENG-103 in workspace acme\n"
    );
    let (reply, _) = write(
        f,
        "add_subscriber",
        &issue,
        json!({"user": "dawen@acme.example"}),
        &state,
    );
    assert_eq!(
        reply["handle"]["fields"]["subscribers"],
        json!(["afkd", "陳大文"]),
        "{reply}"
    );
    let (reply, _) = write(f, "add_subscriber", &issue, json!({"user": "me"}), &state);
    assert_eq!(
        reply["handle"]["fields"]["subscribers"],
        json!(["afkd", "陳大文"]),
        "once"
    );
    assert_eq!(f.subscribers_of(e3), [f.key_user(), chen.clone()]);
    let (reply, stderr) = write(
        f,
        "remove_subscriber",
        &issue,
        json!({"user": "陳大文"}),
        &state,
    );
    assert_eq!(
        reply["handle"]["fields"]["subscribers"],
        json!(["afkd"]),
        "{reply}"
    );
    assert_eq!(
        stderr,
        "[linear] unsubscribed 陳大文 from ENG-103 in workspace acme\n"
    );
    let (reply, _) = write(f, "remove_subscriber", &issue, json!({}), &state);
    assert_eq!(
        reply["handle"]["fields"]["subscribers"],
        json!([]),
        "{reply}"
    );
    assert_eq!(
        changes_seen(f),
        [
            (
                "IssueSubscribe".to_string(),
                json!({"id": e3, "userId": f.key_user()})
            ),
            (
                "IssueSubscribe".to_string(),
                json!({"id": e3, "userId": chen})
            ),
            (
                "IssueSubscribe".to_string(),
                json!({"id": e3, "userId": f.key_user()})
            ),
            (
                "IssueUnsubscribe".to_string(),
                json!({"id": e3, "userId": chen})
            ),
            (
                "IssueUnsubscribe".to_string(),
                json!({"id": e3, "userId": f.key_user()})
            ),
        ]
    );
    let sam = f.user("Sam Lee", "sam", "sam@acme.example");
    let sam_k = f.user("Sam Kim", "sam", "sam.k@acme.example");
    let before = changes_seen(f).len();
    for action in ["add_subscriber", "remove_subscriber"] {
        refused(
            &write(f, action, &issue, json!({"user": "nobody"}), &state),
            &format!(
                "{action}: no user named \"nobody\"; there are: afkd, afkd-app, phil, chen, \
                 陳大文, sam, sam"
            ),
        );
        refused(
            &write(f, action, &issue, json!({"user": "sam"}), &state),
            &format!(
                "{action}: 2 users are named \"sam\": {sam}, {sam_k}; pass the id of the one \
                 you mean in place of the name"
            ),
        );
    }
    assert_eq!(changes_seen(f).len(), before, "nothing sent");
    let (reply, _) = write(f, "add_subscriber", &issue, json!({"user": sam_k}), &state);
    assert_eq!(
        reply["handle"]["fields"]["subscribers"],
        json!(["sam"]),
        "{reply}"
    );
    end(f);
}

/// `archive_issue` archives the issue and its sub-issue with it, and `unarchive_issue` takes
/// both back; each gives back the issue as Linear then holds it.
#[test]
fn archive_and_unarchive_take_the_sub_issues() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-archive");
    let (e1, e2) = (w.issue(101), w.issue(102));
    let parent = read_issue(f, "ENG-102", &state);
    assert_eq!(parent["fields"]["sub_issues"], json!(["ENG-101"]));
    let (reply, stderr) = write(f, "archive_issue", &parent, json!({}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    let fields = &reply["handle"]["fields"];
    assert_eq!(
        (&fields["archived"], &fields["trashed"]),
        (&json!(true), &json!(false))
    );
    assert!(f.archived(e2) && f.archived(e1), "the sub-issue with it");
    assert_eq!(stderr, "[linear] archived ENG-102 in workspace acme\n");
    let (reply, stderr) = write(f, "unarchive_issue", &parent, json!({}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"], read_issue(f, "ENG-102", &state));
    assert_eq!(reply["handle"]["fields"]["archived"], false);
    assert_eq!(reply["handle"]["fields"]["sub_issues"], json!(["ENG-101"]));
    assert!(
        !f.archived(e2) && !f.archived(e1),
        "the sub-issue back with it"
    );
    assert_eq!(stderr, "[linear] unarchived ENG-102 in workspace acme\n");
    end(f);
}

/// `delete_issue` puts the issue in the trash, its sub-issue with it, and `unarchive_issue`
/// takes both back out.
#[test]
fn delete_issue_trashes_with_sub_issues_and_unarchive_restores_it() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-trash");
    let (e1, e2) = (w.issue(101), w.issue(102));
    let parent = read_issue(f, "ENG-102", &state);
    let (reply, stderr) = write(f, "delete_issue", &parent, json!({}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    let fields = &reply["handle"]["fields"];
    assert_eq!(
        (&fields["archived"], &fields["trashed"]),
        (&json!(true), &json!(true))
    );
    assert!(f.trashed(e2) && f.trashed(e1), "the sub-issue with it");
    assert_eq!(
        stderr,
        "[linear] put ENG-102 in the trash in workspace acme\n"
    );
    let (reply, _) = write(f, "unarchive_issue", &parent, json!({}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    let fields = &reply["handle"]["fields"];
    assert_eq!(
        (&fields["archived"], &fields["trashed"]),
        (&json!(false), &json!(false))
    );
    assert_eq!(reply["handle"], read_issue(f, "ENG-102", &state));
    assert!(!f.trashed(e2) && !f.trashed(e1) && !f.archived(e1));
    let sent: Vec<String> = changes_seen(f).into_iter().map(|c| c.0).collect();
    assert_eq!(sent, ["IssueDelete", "IssueUnarchive"]);
    end(f);
}

/// An issue 29 days in the trash comes back out; one 31 days in it is gone for good, and
/// Linear's refusal is relayed.
#[test]
fn unarchive_after_thirty_days_is_refused() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-purged");
    let day = 24 * 3_600 * 1_000;
    let recent = read_issue(f, "ENG-104", &state);
    let old = read_issue(f, "ENG-105", &state);
    f.trash_at(w.issue(104), now_ms() - 29 * day);
    f.trash_at(w.issue(105), now_ms() - 31 * day);
    let (reply, _) = write(f, "unarchive_issue", &recent, json!({}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["handle"]["fields"]["trashed"], false);
    refused(
        &write(f, "unarchive_issue", &old, json!({}), &state),
        "linear unarchive issue: Entity not found: Issue",
    );
    end(f);
}

/// The handles `comments` reads of the issue whose identifier is `issue`, in thread order.
fn read_comments(fake: &FakeLinear, issue: &str, state: &TempDir) -> Vec<Value> {
    let handle = read_issue(fake, issue, state);
    let (reply, _) = once(
        "comments",
        json!({"ws": ws(fake), "issue": handle_of(&handle)}),
        state.path(),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    reply["handles"].as_array().expect("comments").clone()
}

/// The handle `comments` reads of the comment `id`, on the issue whose identifier is `issue`.
fn read_comment(fake: &FakeLinear, issue: &str, id: &str, state: &TempDir) -> Value {
    read_comments(fake, issue, state)
        .into_iter()
        .find(|c| c["id"] == id)
        .expect("the comment")
}

/// The comment write `action` on the comment `comment` with `args` beside the workspace
/// reaching `fake`: the reply, and what the child wrote to stderr.
fn on_comment(
    fake: &FakeLinear,
    action: &str,
    comment: &Value,
    args: Value,
    state: &TempDir,
) -> (Value, String) {
    let mut args = args;
    args["ws"] = ws(fake);
    args["comment"] = handle_of(comment);
    once(action, args, state.path())
}

/// The comment writes the fake was sent, each one's operation and variables, in order.
fn comment_writes_seen(fake: &FakeLinear) -> Vec<(String, Value)> {
    const WRITES: &[&str] = &[
        "CommentPost",
        "CommentUpdate",
        "CommentResolve",
        "CommentUnresolve",
        "CommentDelete",
    ];
    fake.seen()
        .into_iter()
        .filter(|seen| WRITES.contains(&seen.operation.as_str()))
        .map(|seen| (seen.operation, seen.variables))
        .collect()
}

/// `create_comment` posts the body and gives back the comment as `comments` then reads it;
/// `reply_to` files it in that comment's thread, and a reply to a reply in the thread both
/// are in, since Linear keeps threads one level deep. A `reply_to` on another issue is refused
/// before anything is posted.
#[test]
fn create_comment_gives_back_the_comment() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-comment");
    let issue = read_issue(f, "ENG-101", &state);
    let body = "看起来不对 🚨 — it resets on every 401:\n\n```\nPOST /graphql 401\n```";
    let (reply, stderr) = write(f, "create_comment", &issue, json!({"body": body}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    let first = reply["handle"].clone();
    let first_id = first["id"].as_str().expect("an id").to_string();
    assert_eq!(
        read_comments(f, "ENG-101", &state),
        std::slice::from_ref(&first)
    );
    assert_eq!(first["key"], format!("linear:{}/{first_id}", f.org_id()));
    let fields = &first["fields"];
    assert_eq!(
        (&fields["issue"], &fields["body"], &fields["reply_to"]),
        (&json!("ENG-101"), &json!(body), &Value::Null)
    );
    assert_eq!(stderr, "[linear] commented on ENG-101 in workspace acme\n");
    let (reply, stderr) = write(
        f,
        "create_comment",
        &issue,
        json!({"body": "Which header?", "reply_to": handle_of(&first)}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let which = reply["handle"].clone();
    assert_eq!(which["fields"]["reply_to"], first_id);
    assert_eq!(
        stderr,
        format!("[linear] replied to comment {first_id} on ENG-101 in workspace acme\n")
    );
    let (reply, _) = write(
        f,
        "create_comment",
        &issue,
        json!({"body": "Retry-After, in seconds.", "reply_to": handle_of(&which)}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let answer = reply["handle"].clone();
    assert_eq!(answer["fields"]["reply_to"], first_id, "in the one thread");
    assert_eq!(read_comments(f, "ENG-101", &state), [first, which, answer]);
    let parents: Vec<Value> = comment_writes_seen(f)
        .iter()
        .map(|(_, vars)| vars["input"]["parentId"].clone())
        .collect();
    assert_eq!(parents, [Value::Null, json!(first_id), json!(first_id)]);
    let other = read_issue(f, "ENG-102", &state);
    let first = read_comments(f, "ENG-101", &state)[0].clone();
    refused(
        &write(
            f,
            "create_comment",
            &other,
            json!({"body": "x", "reply_to": handle_of(&first)}),
            &state,
        ),
        &format!("create_comment: reply_to {first_id} is on ENG-101, not ENG-102"),
    );
    assert_eq!(comment_writes_seen(f).len(), 3, "nothing more posted");
    assert!(read_comments(f, "ENG-102", &state).is_empty());
    end(f);
}

/// `files` on `create_comment` and `update_comment` put each file up and embed it after the
/// body, each on a paragraph of its own: an image shown, any other file linked.
#[test]
fn create_and_update_comment_embed_files() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-comment-files");
    let dir = TempDir::new("writes-comment-files-src");
    let shot = dir.path().join("screen shot — 修复.png");
    let log = dir.path().join("run.log");
    std::fs::write(&shot, png()).unwrap();
    std::fs::write(&log, "retry 1\nretry 2\n").unwrap();
    let issue = read_issue(f, "ENG-103", &state);
    let text = "It retries forever 🚨:\n\n- run it\n- watch it";
    let (reply, _) = write(
        f,
        "create_comment",
        &issue,
        json!({"body": text, "files": [shot, log]}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let uploads = f.uploads();
    assert_eq!(uploads.len(), 2);
    assert!(uploads.iter().all(|u| u.landed), "{uploads:?}");
    let body = format!(
        "{text}\n\n![screen shot — 修复.png]({})\n\n[run.log]({})",
        uploads[0].asset_url, uploads[1].asset_url
    );
    assert_eq!(f.comments(w.issue(103))[0].body, body);
    assert_eq!(reply["handle"]["fields"]["body"], body);
    let comment = reply["handle"].clone();
    let (reply, _) = on_comment(
        f,
        "update_comment",
        &comment,
        json!({"body": "Only the log, then.", "files": [log]}),
        &state,
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let uploads = f.uploads();
    assert_eq!(uploads.len(), 3, "the log put up again");
    let body = format!("Only the log, then.\n\n[run.log]({})", uploads[2].asset_url);
    assert_eq!(f.comments(w.issue(103))[0].body, body);
    assert_eq!(reply["handle"]["fields"]["body"], body);
    let (files, _) = once(
        "files",
        json!({"ws": ws(f), "issue": handle_of(&issue)}),
        state.path(),
    );
    assert_eq!(files["value"], json!([uploads[2].asset_url]));
    end(f);
}

/// `update_comment` rewrites the body and gives back the comment as `comments` then reads it,
/// marked edited.
#[test]
fn update_comment_gives_back_the_edit() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-comment-edit");
    let sam = f.user("Sam Lee", "sam", "sam@acme.example");
    let id = f.comment(w.issue(101), Author::User(sam), "Cap it at 30s.", 60);
    let before = read_comment(f, "ENG-101", &id, &state);
    assert_eq!(before["fields"]["edited_at"], Value::Null);
    let body = "Cap it at 30s — \"Retry-After\" wins 🚦\n\n> quoted";
    let (reply, stderr) = on_comment(f, "update_comment", &before, json!({"body": body}), &state);
    assert_eq!(reply["ok"], true, "{reply}");
    let after = read_comment(f, "ENG-101", &id, &state);
    assert_eq!(reply["handle"], after, "the reply is what Linear now holds");
    assert_eq!(after["key"], before["key"]);
    assert_eq!(changed(&before, &after), ["body", "edited_at"]);
    assert_eq!(after["fields"]["body"], body);
    assert!(
        after["fields"]["edited_at"].as_u64() >= before["fields"]["created_at"].as_u64(),
        "{after}"
    );
    assert_eq!(
        comment_writes_seen(f),
        [(
            "CommentUpdate".to_string(),
            json!({"id": id, "input": {"body": body}})
        )]
    );
    assert_eq!(
        stderr,
        format!("[linear] edited comment {id} on ENG-101 in workspace acme\n")
    );
    end(f);
}

/// A thread is resolved and unresolved on its first comment, which carries `resolved_at` and
/// is what comes back; passed a reply, the write acts on the reply's thread, the reply named
/// as the one that resolved it.
#[test]
fn resolve_and_unresolve_a_first_comment_and_a_reply() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-comment-resolve");
    let sam = f.user("Sam Lee", "sam", "sam@acme.example");
    let first = f.comment(
        w.issue(101),
        Author::User(sam.clone()),
        "Which header?",
        120,
    );
    let reply_id = f.comment_at(
        w.issue(101),
        Author::External("x-1".into()),
        "Retry-After.",
        now_ms() - 60_000,
        Some(&first),
    );
    let first_handle = read_comment(f, "ENG-101", &first, &state);
    let reply_handle = read_comment(f, "ENG-101", &reply_id, &state);
    let start = now_ms();
    for (action, on, resolved, sent) in [
        (
            "resolve_comment",
            &first_handle,
            true,
            (
                "CommentResolve",
                json!({"id": first, "resolvingCommentId": null}),
            ),
        ),
        (
            "unresolve_comment",
            &first_handle,
            false,
            ("CommentUnresolve", json!({"id": first})),
        ),
        (
            "resolve_comment",
            &reply_handle,
            true,
            (
                "CommentResolve",
                json!({"id": first, "resolvingCommentId": reply_id}),
            ),
        ),
        (
            "unresolve_comment",
            &reply_handle,
            false,
            ("CommentUnresolve", json!({"id": first})),
        ),
    ] {
        let (reply, stderr) = on_comment(f, action, on, json!({}), &state);
        assert_eq!(reply["ok"], true, "{action}: {reply}");
        assert_eq!(
            reply["handle"]["id"], first,
            "{action}: the thread's first comes back"
        );
        assert_eq!(reply["handle"], read_comment(f, "ENG-101", &first, &state));
        let at = &reply["handle"]["fields"]["resolved_at"];
        if resolved {
            assert!(at.as_u64().is_some_and(|ms| ms >= start), "{action}: {at}");
        } else {
            assert_eq!(at, &Value::Null, "{action}");
        }
        assert_eq!(
            comment_writes_seen(f).last(),
            Some(&(sent.0.to_string(), sent.1)),
            "{action}"
        );
        let verb = action.trim_end_matches("_comment");
        assert_eq!(
            stderr,
            format!("[linear] {verb}d comment {first} on ENG-101 in workspace acme\n")
        );
    }
    assert_eq!(
        read_comment(f, "ENG-101", &reply_id, &state),
        reply_handle,
        "the reply itself never changes"
    );
    end(f);
}

/// `delete_comment` gives back nil; a reply goes alone, and a thread's first comment takes its
/// replies with it.
#[test]
fn delete_comment_gives_nil_and_takes_the_replies() {
    let w = world();
    let f = &w.fake;
    let state = TempDir::new("writes-comment-delete");
    let e1 = w.issue(101);
    let ago = |s: u64| now_ms() - s * 1000;
    let author = || Author::User(f.key_user());
    let first = f.comment_at(e1, author(), "Cap it at 30s.", ago(300), None);
    let a = f.comment_at(e1, author(), "Why 30?", ago(240), Some(&first));
    let b = f.comment_at(e1, author(), "Linear's own.", ago(180), Some(&first));
    let other = f.comment_at(e1, author(), "Upgrade ureq first 🔧", ago(120), None);
    let kept = f.comment_at(e1, author(), "Done.", ago(60), Some(&other));
    let ids = |state: &TempDir| -> Vec<String> {
        read_comments(f, "ENG-101", state)
            .iter()
            .map(|c| c["id"].as_str().expect("an id").to_string())
            .collect()
    };
    let gone = read_comment(f, "ENG-101", &kept, &state);
    let (reply, stderr) = on_comment(f, "delete_comment", &gone, json!({}), &state);
    assert_eq!(reply, json!({"ok": true}), "nil, no handle");
    assert_eq!(ids(&state), [first.clone(), a, b, other.clone()]);
    assert_eq!(
        stderr,
        format!("[linear] deleted comment {kept} in workspace acme\n")
    );
    let opener = read_comment(f, "ENG-101", &first, &state);
    let (reply, _) = on_comment(f, "delete_comment", &opener, json!({}), &state);
    assert_eq!(reply, json!({"ok": true}));
    assert_eq!(ids(&state), [other], "its replies with it");
    refused(
        &on_comment(f, "delete_comment", &opener, json!({}), &state),
        "linear delete comment: Entity not found: Comment",
    );
    end(f);
}

/// A comment of another workspace is refused as the comment of every comment write and as
/// `reply_to`, naming both workspaces, as is a key no read handed out; nothing is written to
/// either workspace.
#[test]
fn a_comment_of_another_workspace_is_refused() {
    let w = world();
    let globex = FakeLinear::start_globex();
    globex.state("Todo", "unstarted");
    let theirs_issue = globex.team_issue("ENG", "Todo", "Globex's own", "");
    let theirs_id = globex.comment(&theirs_issue, Author::User(globex.key_user()), "ours", 60);
    let state = TempDir::new("writes-comment-other");
    let theirs = read_comment(&globex, "ENG-101", &theirs_id, &state);
    let ours = read_issue(&w.fake, "ENG-102", &state);
    let sentence = |action: &str, param: &str| {
        format!(
            "{action}: {param} is {theirs_id} of workspace globex, and the linear.Workspace \
             reaches acme"
        )
    };
    for (action, args) in [
        ("update_comment", json!({"body": "x"})),
        ("resolve_comment", json!({})),
        ("unresolve_comment", json!({})),
        ("delete_comment", json!({})),
    ] {
        refused(
            &on_comment(&w.fake, action, &theirs, args, &state),
            &sentence(action, "comment"),
        );
    }
    refused(
        &write(
            &w.fake,
            "create_comment",
            &ours,
            json!({"body": "x", "reply_to": handle_of(&theirs)}),
            &state,
        ),
        &sentence("create_comment", "reply_to"),
    );
    let theirs_issue = read_issue(&globex, "ENG-101", &state);
    refused(
        &write(
            &w.fake,
            "create_comment",
            &theirs_issue,
            json!({"body": "x"}),
            &state,
        ),
        "create_comment: issue is ENG-101 of workspace globex, and the linear.Workspace \
         reaches acme",
    );
    let unknown = json!({"id": "c-1", "key": "trello:c-1"});
    refused(
        &on_comment(&w.fake, "resolve_comment", &unknown, json!({}), &state),
        "resolve_comment: comment trello:c-1 names no comment this plugin handed out",
    );
    assert_eq!(comment_writes_seen(&w.fake), Vec::<(String, Value)>::new());
    assert_eq!(comment_writes_seen(&globex), Vec::<(String, Value)>::new());
    end(&w.fake);
    end(&globex);
}
