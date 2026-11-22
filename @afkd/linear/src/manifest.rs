//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd
//! types a config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = value` lines, each value kept as written, bar `doc`:
//! the line of prose on an item is for the editor, and `checks/` holds it to its rule.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::client::LINEAR_BASE;
use crate::plugin::LINEAR_KIND;
use crate::reads::{Sort, READS};
use crate::settings::SETTINGS;
use crate::tracker::{Priority, StateType};
use crate::wire::{
    CommentFields, CycleFields, IssueFields, LabelFields, ProjectFields, StateFields, TeamFields,
    UserFields,
};
use crate::writes::WRITES;

/// The name every slot passes the claimed issue under.
const HANDLE: &str = "issue";

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each
/// with its value as written (`"issue"`, `true`, `"list[str]"`).
type Table = (String, BTreeMap<String, String>);

/// A table and the tables written under it: a `[[kind.slot]]` and its `[[kind.slot.param]]`s.
type Group = (BTreeMap<String, String>, Vec<BTreeMap<String, String>>);

/// Scan the manifest into its tables, in order.
fn tables(text: &str) -> Vec<Table> {
    let mut tables: Vec<Table> = vec![(String::new(), BTreeMap::new())];
    for line in text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
    {
        if let Some(header) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            tables.push((header.to_string(), BTreeMap::new()));
        } else if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "doc" {
                continue;
            }
            let table = &mut tables.last_mut().unwrap().1;
            table.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    tables
}

/// The tables under `header`, in order.
fn named<'a>(tables: &'a [Table], header: &str) -> Vec<&'a BTreeMap<String, String>> {
    tables
        .iter()
        .filter(|(h, _)| h == header)
        .map(|(_, table)| table)
        .collect()
}

/// Each `parent` table with the `child` tables written under it, in order: a
/// `[[kind.slot.param]]` belongs to the `[[kind.slot]]` above it. A `parent` table ends
/// at the next table that is neither it nor its `child`.
fn grouped(tables: &[Table], parent: &str, child: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut open = false;
    for (header, t) in tables {
        if header == parent {
            groups.push((t.clone(), Vec::new()));
            open = true;
        } else if header == child && open {
            groups.last_mut().unwrap().1.push(t.clone());
        } else {
            open = false;
        }
    }
    groups
}

/// A table as written from `pairs`, each value already spelled as TOML.
fn table(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `s` as a TOML basic string.
fn quoted(s: &str) -> String {
    format!("\"{s}\"")
}

/// `items` as a TOML array of basic strings, as the manifest writes one.
fn quoted_list(items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|item| quoted(item)).collect();
    format!("[{}]", items.join(", "))
}

/// Each enum the manifest declares, by name, and its members in order.
fn enums(tables: &[Table]) -> BTreeMap<String, Vec<String>> {
    named(tables, "enum")
        .into_iter()
        .map(|t| {
            let members = t["values"]
                .trim_matches(['[', ']'])
                .split(',')
                .map(|m| m.trim().trim_matches('"').to_string())
                .collect();
            (t["name"].trim_matches('"').to_string(), members)
        })
        .collect()
}

/// The manifest's top level carries the plugin's fixed fields, at protocol 2.
#[test]
fn the_manifest_names_the_plugin_and_its_finished_binary() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/linear"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("exec", env!("CARGO_PKG_NAME")),
        ("version", env!("CARGO_PKG_VERSION")),
    ] {
        assert_eq!(top[key], quoted(value), "{key}");
    }
    assert_eq!(top["proto"], crate::wire::PROTO.to_string());
    // The release ships the crate's binary finished, beside the manifest; afkd never builds
    // a plugin, and refuses a manifest that asks it to.
    assert!(!top.contains_key("build"), "{top:?}");
    // No proto 1 table survives, nor the hooks slots replaced: afkd refuses a v2
    // manifest carrying one.
    for obsolete in ["trigger", "trigger.block", "worker", "command", "kind.hook"] {
        assert!(named(&tables, obsolete).is_empty(), "{obsolete}");
    }
}

/// The names `all`, every member of one of the code's enums, cross the wire as.
fn members<T: Serialize>(all: &[T]) -> Vec<String> {
    all.iter()
        .map(|member| {
            let value = serde_json::to_value(member).expect("a member serializes");
            value.as_str().expect("a member's name").to_string()
        })
        .collect()
}

/// The three enums are the ones the code reads and writes, member for member and in
/// Linear's order: each member the manifest lists is the name a value of the code's enum
/// crosses the wire as.
#[test]
fn the_enums_are_the_ones_the_code_speaks() {
    assert_eq!(
        enums(&tables(MANIFEST)),
        BTreeMap::from([
            (
                "Priority".to_string(),
                members(&[
                    Priority::None,
                    Priority::Urgent,
                    Priority::High,
                    Priority::Medium,
                    Priority::Low,
                ])
            ),
            (
                "State_Type".to_string(),
                members(&[
                    StateType::Triage,
                    StateType::Backlog,
                    StateType::Unstarted,
                    StateType::Started,
                    StateType::Completed,
                    StateType::Canceled,
                ])
            ),
            ("Sort".to_string(), members(&Sort::ALL)),
        ])
    );
}

/// The one `[[struct]]` is `linear.Workspace`: the two credential forms, each secret field
/// marked, all optional so a literal names one, and Linear's API root as the default.
#[test]
fn the_struct_is_the_workspace_a_read_reaches() {
    let tables = tables(MANIFEST);
    let field = |name: &str, ty: &str, extra: &[(&str, &str)]| {
        let mut t = table(extra);
        t.insert("name".into(), quoted(name));
        t.insert("type".into(), quoted(ty));
        t
    };
    assert_eq!(
        grouped(&tables, "struct", "struct.field"),
        [(
            table(&[("name", "\"Workspace\"")]),
            vec![
                field("api_key", "str?", &[("secret", "true")]),
                field("client_id", "str?", &[]),
                field("client_secret", "str?", &[("secret", "true")]),
                field("base_url", "str", &[("default", &quoted(LINEAR_BASE))]),
            ]
        )]
    );
}

/// Whether `value` has the JSON shape afkd reads a handle field of type `ty` from: `null`
/// for a `T?`, unix milliseconds for a `time`, and a member's name for one of `enums`.
fn shaped(value: &Value, ty: &str, enums: &BTreeMap<String, Vec<String>>) -> bool {
    if let Some(inner) = ty.strip_suffix('?') {
        return value.is_null() || shaped(value, inner, enums);
    }
    match ty {
        "str" => value.is_string(),
        "int" => value.is_i64() || value.is_u64(),
        "bool" => value.is_boolean(),
        "time" => value.is_u64(),
        "list[str]" => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        enumeration => enums.get(enumeration).is_some_and(|members| {
            value
                .as_str()
                .is_some_and(|m| members.iter().any(|member| member == m))
        }),
    }
}

/// A rich value and a bare one of each handle's fields, as the plugin sends them: every
/// field set, wide text and all, and every optional field `null` and every list empty.
fn handle_fields() -> Vec<(&'static str, [Value; 2])> {
    fn both<F: Serialize>(rich: &F, bare: &F) -> [Value; 2] {
        [
            serde_json::to_value(rich).unwrap(),
            serde_json::to_value(bare).unwrap(),
        ]
    }
    let words = |w: &[&str]| -> Vec<String> { w.iter().map(|s| s.to_string()).collect() };
    let rich_issue = IssueFields {
        uuid: "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5".into(),
        workspace: "acme".into(),
        team: "ENG".into(),
        title: "修复 the retry storm 🚨 — \"backoff\" resets".into(),
        url: "https://linear.app/acme/issue/ENG-421/retry-storm".into(),
        description: "## 背景\n\nIt resets.".into(),
        branch: "eng-421-retry-storm".into(),
        state: "Todo ✏️".into(),
        state_type: StateType::Unstarted,
        priority: Priority::Urgent,
        estimate: Some(3),
        due: Some("2026-12-24".into()),
        assignee: Some("陳大文".into()),
        delegate: Some("vesper".into()),
        creator: Some("phil".into()),
        project: Some("Trackers 🧭".into()),
        milestone: Some("B".into()),
        cycle: Some(12),
        labels: words(&["afkd/ready", "Väntar på svar"]),
        parent: Some("ENG-400".into()),
        sub_issues: words(&["ENG-422", "ENG-423"]),
        blocks: words(&["ENG-500"]),
        blocked_by: words(&["ENG-399"]),
        related: words(&["ENG-12"]),
        similar: words(&["ENG-13"]),
        duplicates: words(&["ENG-19"]),
        duplicate_of: Some("ENG-14".into()),
        links: words(&["https://github.com/acme/widgets/pull/42"]),
        subscribers: words(&["phil"]),
        created_at: 1_793_095_080_087,
        updated_at: 1_793_095_380_500,
        completed_at: Some(1_793_095_400_000),
        canceled_at: Some(1_793_095_500_000),
        archived: true,
        trashed: true,
    };
    let bare_issue = IssueFields {
        uuid: String::new(),
        workspace: String::new(),
        team: String::new(),
        title: String::new(),
        url: String::new(),
        description: String::new(),
        branch: String::new(),
        state: String::new(),
        state_type: StateType::Backlog,
        priority: Priority::None,
        estimate: None,
        due: None,
        assignee: None,
        delegate: None,
        creator: None,
        project: None,
        milestone: None,
        cycle: None,
        labels: Vec::new(),
        parent: None,
        sub_issues: Vec::new(),
        blocks: Vec::new(),
        blocked_by: Vec::new(),
        related: Vec::new(),
        similar: Vec::new(),
        duplicates: Vec::new(),
        duplicate_of: None,
        links: Vec::new(),
        subscribers: Vec::new(),
        created_at: 0,
        updated_at: 0,
        completed_at: None,
        canceled_at: None,
        archived: false,
        trashed: false,
    };
    let comment = |rich: bool| CommentFields {
        issue: "ENG-7".into(),
        body: if rich {
            "看起来不对 🚨\n\n```\nx\n```"
        } else {
            ""
        }
        .into(),
        url: "https://linear.app/acme/issue/ENG-7#comment-c1".into(),
        author: rich.then(|| "陳大文".into()),
        reply_to: rich.then(|| "c0".into()),
        resolved_at: rich.then_some(1_793_095_200_000),
        created_at: 1_793_095_080_087,
        edited_at: rich.then_some(1_793_095_380_500),
    };
    let label = |rich: bool| LabelFields {
        name: "Bug 🐛".into(),
        group: rich.then(|| "Type".into()),
        team: rich.then(|| "ENG".into()),
    };
    let project = |rich: bool| ProjectFields {
        name: "Trackers 🧭".into(),
        teams: if rich {
            words(&["ENG", "OPS"])
        } else {
            Vec::new()
        },
        milestones: if rich { words(&["A", "B"]) } else { Vec::new() },
        url: "https://linear.app/acme/project/p1".into(),
    };
    let cycle = |rich: bool| CycleFields {
        number: 12,
        name: rich.then(|| "Sprint 12 — 冬".into()),
        team: "ENG".into(),
        starts_at: 1_792_972_800_000,
        ends_at: 1_794_182_400_000,
        active: rich,
    };
    let team = TeamFields {
        name: "Engineering — エンジニアリング".into(),
    };
    let state = StateFields {
        name: "In Review 👀".into(),
        kind: StateType::Started,
        team: "ENG".into(),
    };
    let user = |app: bool| UserFields {
        name: "chen".into(),
        full_name: "陳大文".into(),
        email: "chen@acme.example".into(),
        app,
    };
    vec![
        ("Issue", both(&rich_issue, &bare_issue)),
        ("Comment", both(&comment(true), &comment(false))),
        ("Team", both(&team, &team)),
        ("State", both(&state, &state)),
        ("Label", both(&label(true), &label(false))),
        ("User", both(&user(true), &user(false))),
        ("Project", both(&project(true), &project(false))),
        ("Cycle", both(&cycle(true), &cycle(false))),
    ]
}

/// The `[[handle]]`s are the issue and the seven a read hands back, and each declares
/// exactly the fields its code sends — a rich value's and a bare one's alike — each in the
/// JSON shape of its declared type. afkd refuses a handle carrying a field undeclared or
/// mistyped, and a declared one never sent faults the slot that reads it, so a field added to
/// either side alone fails here. No handle declares `id` or `key`, which every handle has.
#[test]
fn every_declared_handle_field_is_sent_with_its_declared_type() {
    let tables = tables(MANIFEST);
    let enums = enums(&tables);
    let handles = grouped(&tables, "handle", "handle.field");
    let sent = handle_fields();
    assert_eq!(
        handles
            .iter()
            .map(|(h, _)| h["name"].trim_matches('"'))
            .collect::<Vec<_>>(),
        sent.iter().map(|(name, _)| *name).collect::<Vec<_>>()
    );
    for ((_, fields), (name, values)) in handles.iter().zip(&sent) {
        let declared: BTreeSet<&str> = fields.iter().map(|f| f["name"].trim_matches('"')).collect();
        assert!(
            !declared.contains("id") && !declared.contains("key"),
            "{name}"
        );
        for value in values {
            let value = value.as_object().expect("the fields are an object");
            assert_eq!(
                value.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                declared,
                "{name}"
            );
            for field in fields {
                let (field, ty) = (
                    field["name"].trim_matches('"'),
                    field["type"].trim_matches('"'),
                );
                assert!(
                    shaped(&value[field], ty, &enums),
                    "{name}.{field} is not a {ty}: {}",
                    value[field]
                );
            }
        }
    }
}

/// The one `[[kind]]` is the main trigger, claiming one issue per run, and declares the
/// settings the code reads, key for key and in order — never afkd's own claim keys — and
/// the five slots afkd runs, each passed the workspace beside the issue.
#[test]
fn the_kind_is_the_main_claiming_trigger_with_every_setting_typed() {
    let tables = tables(MANIFEST);
    assert_eq!(
        named(&tables, "kind"),
        [&table(&[
            ("name", &quoted(LINEAR_KIND)),
            ("role", "\"trigger\""),
            ("main", "true"),
            ("claims", "true"),
        ])]
    );
    let want: Vec<BTreeMap<String, String>> = SETTINGS
        .iter()
        .map(|(name, ty, extra)| {
            let mut t = table(extra);
            t.insert("name".into(), quoted(name));
            t.insert("type".into(), quoted(ty));
            t
        })
        .collect();
    let settings: Vec<BTreeMap<String, String>> = named(&tables, "kind.setting")
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(settings, want);
    for afkds in ["every", "max_attempts"] {
        assert!(
            settings.iter().all(|s| s["name"] != quoted(afkds)),
            "`{afkds}` is afkd's to declare"
        );
    }
    // `on_run` first and without `when`; each slot passed the run, except `on_claim`, run
    // before any run exists; the workspace; the issue; and after the run the outcome, in that
    // order.
    let param = |name: &str, ty: &str| table(&[("name", &quoted(name)), ("type", &quoted(ty))]);
    let slot = |name: &str, when: Option<&str>| {
        let mut t = table(&[("name", &quoted(name))]);
        let mut params = Vec::new();
        if when != Some("pre") {
            params.push(param("run", "afkd.Run"));
        }
        params.push(param("workspace", "Workspace"));
        params.push(param(HANDLE, "Issue"));
        if let Some(when) = when {
            t.insert("when".into(), quoted(when));
            if when == "post" {
                params.push(param("outcome", "afkd.Outcome"));
            }
        }
        (t, params)
    };
    assert_eq!(
        grouped(&tables, "kind.slot", "kind.slot.param"),
        [
            slot("on_run", None),
            slot("on_claim", Some("pre")),
            slot("on_done", Some("post")),
            slot("on_fail", Some("post")),
            slot("on_park", Some("post")),
        ]
    );
}

/// Each `[[action]]` and the `[[action.param]]`s written under it, by name.
fn action_params(tables: &[Table]) -> BTreeMap<String, Vec<BTreeMap<String, String>>> {
    let mut params: BTreeMap<String, Vec<BTreeMap<String, String>>> = BTreeMap::new();
    let mut action = String::new();
    for (header, t) in tables {
        match header.as_str() {
            "action" => {
                action = t["name"].trim_matches('"').to_string();
                params.entry(action.clone()).or_default();
            }
            "action.param" => params.entry(action.clone()).or_default().push(t.clone()),
            _ => {}
        }
    }
    params
}

/// The `[[action]]`s are the reads [`crate::reads`] answers, then the writes
/// [`crate::writes`] answers, each in its code's order, and nothing else. A plugin exposes no
/// values, so there is no `[[value]]`.
#[test]
fn the_actions_are_the_reads_and_the_writes_the_code_answers() {
    let tables = tables(MANIFEST);
    let actions: Vec<String> = named(&tables, "action")
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(
        actions,
        READS
            .iter()
            .chain(WRITES)
            .map(|a| quoted(a))
            .collect::<Vec<_>>()
    );
    assert!(
        named(&tables, "value").is_empty(),
        "a plugin exposes no values"
    );
}

/// Each read takes a `linear.Workspace` first and returns what it reads: `issues` every
/// filter named-only with no default, a list of strings, enums, ints, a bool, times or a
/// date, and then whether archived issues count, the order and the limit, each named-only
/// with its default; the rest the parameters they read.
#[test]
fn the_reads_take_a_workspace_and_return_what_they_read() {
    let tables = tables(MANIFEST);
    let returns: BTreeMap<String, String> = named(&tables, "action")
        .into_iter()
        .filter(|t| READS.contains(&t["name"].trim_matches('"')))
        .map(|t| {
            (
                t["name"].trim_matches('"').to_string(),
                t.get("returns").cloned().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        returns,
        BTreeMap::from(
            [
                ("issue", "\"Issue\""),
                ("issues", "\"list[Issue]\""),
                ("comments", "\"list[Comment]\""),
                ("me", "\"User\""),
                ("users", "\"list[User]\""),
                ("teams", "\"list[Team]\""),
                ("states", "\"list[State]\""),
                ("labels", "\"list[Label]\""),
                ("projects", "\"list[Project]\""),
                ("cycles", "\"list[Cycle]\""),
                ("files", "\"list[str]\""),
                ("download_file", ""),
            ]
            .map(|(name, ty)| (name.to_string(), ty.to_string()))
        )
    );

    let param = |name: &str, ty: &str, extra: &[(&str, &str)]| {
        let mut t = table(extra);
        t.insert("name".into(), quoted(name));
        t.insert("type".into(), quoted(ty));
        t
    };
    let required = |name: &str, ty: &str| param(name, ty, &[("required", "true")]);
    let named_only = |name: &str, ty: &str| param(name, ty, &[("named_only", "true")]);
    let ws = required("ws", "Workspace");
    let mut issues = vec![ws.clone()];
    for (name, ty) in [
        ("id", "list[str]"),
        ("team", "list[str]"),
        ("state", "list[str]"),
        ("state_type", "list[State_Type]"),
        ("priority", "list[Priority]"),
        ("labels", "list[str]"),
        ("without_labels", "list[str]"),
        ("assignee", "list[str]"),
        ("delegate", "list[str]"),
        ("project", "list[str]"),
        ("milestone", "list[str]"),
        ("cycle", "list[int]"),
        ("parent", "list[str]"),
        ("blocked", "bool"),
        ("text", "str"),
        ("created_after", "time"),
        ("created_before", "time"),
        ("updated_after", "time"),
        ("updated_before", "time"),
        ("completed_after", "time"),
        ("completed_before", "time"),
        ("canceled_after", "time"),
        ("canceled_before", "time"),
        ("due_after", "str"),
        ("due_before", "str"),
    ] {
        issues.push(named_only(name, ty));
    }
    let defaults = members(&Sort::DEFAULT);
    let defaults: Vec<&str> = defaults.iter().map(String::as_str).collect();
    issues.extend([
        param(
            "include_archived",
            "bool",
            &[("named_only", "true"), ("default", "false")],
        ),
        param(
            "order",
            "list[Sort]",
            &[("named_only", "true"), ("default", &quoted_list(&defaults))],
        ),
        param("limit", "int", &[("named_only", "true"), ("default", "50")]),
    ]);
    let issue = required("issue", "Issue");
    let team = named_only("team", "str");
    let want = BTreeMap::from([
        ("issue".to_string(), vec![ws.clone(), required("id", "str")]),
        ("issues".to_string(), issues),
        ("comments".to_string(), vec![ws.clone(), issue.clone()]),
        ("me".to_string(), vec![ws.clone()]),
        ("users".to_string(), vec![ws.clone()]),
        ("teams".to_string(), vec![ws.clone()]),
        (
            "states".to_string(),
            vec![ws.clone(), required("team", "str")],
        ),
        ("labels".to_string(), vec![ws.clone(), team.clone()]),
        ("projects".to_string(), vec![ws.clone(), team]),
        (
            "cycles".to_string(),
            vec![ws.clone(), required("team", "str")],
        ),
        ("files".to_string(), vec![ws.clone(), issue]),
        (
            "download_file".to_string(),
            vec![ws, required("url", "str"), required("to", "str")],
        ),
    ]);
    let mut params = action_params(&tables);
    params.retain(|action, _| READS.contains(&action.as_str()));
    assert_eq!(params, want);
}

/// Each write takes a `linear.Workspace` first and the handle of what it changes, and returns
/// it, but `delete_comment`, which returns nothing, and `create_issue`, which takes the team
/// and the title and returns the issue it files: `update_issue` every field it sets
/// named-only with no default, so one left out reaches the plugin as no key and leaves its
/// field; a field that can hold nothing is optional, so `nil` clears it; `at` is one of two
/// words and `after` and `parent` an issue's handle. A relation is one of six words and
/// `other` an issue's handle, a link's url is required and its title named-only, a subscriber
/// is `"me"` unless named, and a label is required. A comment's body is required, its files and
/// the comment it answers named-only.
#[test]
fn the_writes_take_a_workspace_and_return_what_they_change() {
    let tables = tables(MANIFEST);
    let returns: BTreeMap<String, String> = named(&tables, "action")
        .into_iter()
        .filter(|t| WRITES.contains(&t["name"].trim_matches('"')))
        .map(|t| {
            (
                t["name"].trim_matches('"').to_string(),
                t.get("returns").cloned().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        returns,
        WRITES
            .iter()
            .map(|write| {
                let returns = match *write {
                    "delete_comment" => String::new(),
                    w if w.ends_with("_comment") => quoted("Comment"),
                    _ => quoted("Issue"),
                };
                (write.to_string(), returns)
            })
            .collect::<BTreeMap<_, _>>()
    );
    let param = |name: &str, ty: &str, extra: &[(&str, &str)]| {
        let mut t = table(extra);
        t.insert("name".into(), quoted(name));
        t.insert("type".into(), quoted(ty));
        t
    };
    let required = |name: &str, ty: &str| param(name, ty, &[("required", "true")]);
    let named_only = |name: &str, ty: &str| param(name, ty, &[("named_only", "true")]);
    let mut update_issue = vec![required("ws", "Workspace"), required("issue", "Issue")];
    for (name, ty) in [
        ("title", "str"),
        ("description", "str"),
        ("files", "list[str]"),
        ("state", "str"),
        ("team", "str"),
    ] {
        update_issue.push(named_only(name, ty));
    }
    update_issue.push(param(
        "at",
        "enum",
        &[
            ("values", &quoted_list(&["top", "bottom"])),
            ("named_only", "true"),
        ],
    ));
    for (name, ty) in [
        ("after", "Issue"),
        ("priority", "Priority"),
        ("estimate", "int?"),
        ("due", "str?"),
        ("assignee", "str?"),
        ("delegate", "str?"),
        ("project", "str?"),
        ("milestone", "str?"),
        ("cycle", "int?"),
        ("parent", "Issue?"),
    ] {
        update_issue.push(named_only(name, ty));
    }
    let issue = [required("ws", "Workspace"), required("issue", "Issue")];
    let relations: Vec<&str> = crate::writes::Relation::ALL
        .iter()
        .map(|r| r.name())
        .collect();
    let relate = [
        issue.to_vec(),
        vec![
            param(
                "relation",
                "enum",
                &[("values", &quoted_list(&relations)), ("required", "true")],
            ),
            required("other", "Issue"),
        ],
    ]
    .concat();
    let url = required("url", "str");
    let user = param("user", "str", &[("default", &quoted("me"))]);
    // `create_issue` the team and the title, then every other field `update_issue` sets, and
    // the labels; a `T?` one so a field read off another issue can be passed straight in.
    let mut create_issue = vec![
        required("ws", "Workspace"),
        required("team", "str"),
        required("title", "str"),
    ];
    for field in &update_issue[2..] {
        if [quoted("team"), quoted("title")].contains(&field["name"]) {
            continue;
        }
        if field["name"] == quoted("assignee") {
            create_issue.push(named_only("labels", "list[str]"));
        }
        create_issue.push(field.clone());
    }
    let label = [issue.to_vec(), vec![required("label", "str")]].concat();
    let mut want = BTreeMap::from([
        ("create_issue".to_string(), create_issue),
        ("update_issue".to_string(), update_issue),
        ("add_label".to_string(), label.clone()),
        ("remove_label".to_string(), label),
    ]);
    for (write, params) in [
        ("add_relation", relate.clone()),
        ("remove_relation", relate),
        (
            "add_link",
            [
                issue.to_vec(),
                vec![url.clone(), named_only("title", "str")],
            ]
            .concat(),
        ),
        ("remove_link", [issue.to_vec(), vec![url]].concat()),
        (
            "add_subscriber",
            [issue.to_vec(), vec![user.clone()]].concat(),
        ),
        ("remove_subscriber", [issue.to_vec(), vec![user]].concat()),
        ("archive_issue", issue.to_vec()),
        ("unarchive_issue", issue.to_vec()),
        ("delete_issue", issue.to_vec()),
    ] {
        want.insert(write.to_string(), params);
    }
    let comment = [required("ws", "Workspace"), required("comment", "Comment")];
    let body = required("body", "str");
    let files = named_only("files", "list[str]");
    for (write, params) in [
        (
            "create_comment",
            [
                issue.to_vec(),
                vec![
                    body.clone(),
                    files.clone(),
                    named_only("reply_to", "Comment"),
                ],
            ]
            .concat(),
        ),
        (
            "update_comment",
            [comment.to_vec(), vec![body, files]].concat(),
        ),
        ("resolve_comment", comment.to_vec()),
        ("unresolve_comment", comment.to_vec()),
        ("delete_comment", comment.to_vec()),
    ] {
        want.insert(write.to_string(), params);
    }
    let mut params = action_params(&tables);
    params.retain(|action, _| WRITES.contains(&action.as_str()));
    assert_eq!(params, want);
    let words: Vec<Value> = crate::writes::Relation::ALL
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    assert_eq!(words, relations, "the words `relation` decodes");
    let at: Vec<Value> = [crate::writes::At::Top, crate::writes::At::Bottom]
        .iter()
        .map(|at| serde_json::to_value(at).unwrap())
        .collect();
    assert_eq!(at, ["top", "bottom"], "the words `at` decodes");
}

/// A plugin's kinds, actions, withables, handle types, enums and structs share one namespace
/// (reference §13.3), and afkd's install refuses a collision; so no name is declared twice
/// across them.
#[test]
fn no_kind_action_handle_enum_or_struct_shares_a_name() {
    let tables = tables(MANIFEST);
    let mut names: Vec<&str> = ["handle", "kind", "action", "enum", "struct"]
        .iter()
        .flat_map(|header| named(&tables, header))
        .map(|t| t["name"].as_str())
        .collect();
    let all = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all, "{names:?}");
}
