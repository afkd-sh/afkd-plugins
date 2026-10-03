//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd
//! types a config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = value` lines, each value kept as written.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::lifecycle::{Vocabulary, ACTIONS, ISSUE_VOCABULARY, MR_VOCABULARY};
use crate::plugin::{ISSUE_KIND, MR_KIND};
use crate::settings::{Declared, ISSUE_SETTINGS, MR_SETTINGS};
use crate::wire::{Fields, IssueFields, MrFields};

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each
/// with its value as written (`"issue"`, `true`, `"30s"`).
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

/// The `child` tables grouped under the `parent` table written above each, keyed by the
/// parent's `name` as written: `[[kind.setting]]` under its `[[kind]]`, `[[action.param]]`
/// under its `[[action]]`.
fn children(
    tables: &[Table],
    parent: &str,
    child: &str,
) -> BTreeMap<String, Vec<BTreeMap<String, String>>> {
    let mut grouped: BTreeMap<String, Vec<BTreeMap<String, String>>> = BTreeMap::new();
    let mut owner = String::new();
    for (header, t) in tables {
        if header == parent {
            owner = t["name"].clone();
            grouped.entry(owner.clone()).or_default();
        } else if header == child {
            grouped.entry(owner.clone()).or_default().push(t.clone());
        }
    }
    grouped
}

/// Each `[[kind]]`'s `[[kind.slot]]`s, keyed by the kind's `name` as written, each with the
/// `[[kind.slot.param]]`s written under it: a slot's name repeats across kinds, so the
/// slots are grouped under their kind and the params under their slot, in order.
fn slots(tables: &[Table]) -> BTreeMap<String, Vec<Group>> {
    let mut grouped: BTreeMap<String, Vec<Group>> = BTreeMap::new();
    let mut kind = String::new();
    for (header, t) in tables {
        match header.as_str() {
            "kind" => kind = t["name"].clone(),
            "kind.slot" => grouped
                .entry(kind.clone())
                .or_default()
                .push((t.clone(), Vec::new())),
            "kind.slot.param" => grouped
                .get_mut(&kind)
                .and_then(|slots| slots.last_mut())
                .expect("a param under a slot")
                .1
                .push(t.clone()),
            _ => {}
        }
    }
    grouped
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

/// The `[[kind.setting]]` tables `declared` spells.
fn setting_tables(declared: &[Declared]) -> Vec<BTreeMap<String, String>> {
    declared
        .iter()
        .map(|(name, ty, extra)| {
            let mut t = table(extra);
            t.insert("name".into(), quoted(name));
            t.insert("type".into(), quoted(ty));
            t
        })
        .collect()
}

/// The manifest's top level carries the plugin's fixed fields, at protocol 2.
#[test]
fn the_manifest_names_the_plugin_and_how_it_builds() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/gitlab"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("build", "cargo build --release --locked"),
        ("exec", "target/release/afkd-gitlab"),
        ("version", env!("CARGO_PKG_VERSION")),
    ] {
        assert_eq!(top[key], quoted(value), "{key}");
    }
    assert_eq!(top["proto"], crate::wire::PROTO.to_string());
    // `exec` names what `build` produces: the crate's own binary.
    assert_eq!(
        top["exec"],
        quoted(&format!("target/release/{}", env!("CARGO_PKG_NAME")))
    );
    // No proto 1 table survives, nor the hooks slots replaced: afkd refuses a v2
    // manifest carrying one.
    for obsolete in ["trigger", "trigger.block", "worker", "command", "kind.hook"] {
        assert!(named(&tables, obsolete).is_empty(), "{obsolete}");
    }
}

/// The two `[[handle]]`s are the items the two kinds claim, each with the fields the
/// plugin knows about it beyond the `id` and `key` every handle has.
#[test]
fn the_handles_are_the_two_kinds_items_with_their_fields() {
    let tables = tables(MANIFEST);
    let field = |name: &str, ty: &str| table(&[("name", &quoted(name)), ("type", &quoted(ty))]);
    let common = || {
        vec![
            field("title", "string"),
            field("url", "string"),
            field("number", "int"),
        ]
    };
    let mut issue = common();
    issue.push(field("labels", "list[string]"));
    let mut mr = common();
    mr.push(field("branch", "string"));
    assert_eq!(
        named(&tables, "handle"),
        [
            &table(&[("name", "\"Issue\"")]),
            &table(&[("name", "\"Merge_Request\"")]),
        ]
    );
    assert_eq!(
        children(&tables, "handle", "handle.field"),
        BTreeMap::from([(quoted("Issue"), issue), (quoted("Merge_Request"), mr),])
    );
}

/// Whether `value` has the JSON shape afkd reads a handle field of type `ty` from.
fn shaped(value: &Value, ty: &str) -> bool {
    match ty {
        "string" => value.is_string(),
        "int" => value.is_i64(),
        "list[string]" => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        _ => false,
    }
}

/// The fields a claim sends for each handle are exactly the ones its `[[handle]]`
/// declares, each in the JSON shape of its declared type — a rich item's and a bare
/// one's alike. afkd refuses a unit carrying a field undeclared or mistyped, and a
/// declared one never sent faults the slot that reads it, so a field added to either
/// side alone fails here.
#[test]
fn every_declared_handle_field_is_sent_with_its_declared_type() {
    let tables = tables(MANIFEST);
    let sent = [
        (
            "Issue",
            Fields::Issue(IssueFields {
                title: "修复 the retry storm 🚨 — \"backoff\" resets".into(),
                url: "https://gitlab.example.com/acme/sub.group/widgets/-/issues/7".into(),
                number: 7,
                labels: vec!["afkd::ready".into(), "優先::high".into()],
            }),
        ),
        (
            "Issue",
            Fields::Issue(IssueFields {
                title: String::new(),
                url: String::new(),
                number: 1,
                labels: Vec::new(),
            }),
        ),
        (
            "Merge_Request",
            Fields::Mr(MrFields {
                title: "Cap the retry backoff — \"重试\" 上限 🚦".into(),
                url: "https://gitlab.example.com/acme/sub.group/widgets/-/merge_requests/12".into(),
                number: 12,
                branch: "feature/重试-backoff".into(),
            }),
        ),
    ];
    let declared = children(&tables, "handle", "handle.field");
    assert_eq!(
        declared.keys().cloned().collect::<BTreeSet<_>>(),
        sent.iter().map(|(handle, _)| quoted(handle)).collect(),
        "every handle is sent"
    );
    for (handle, fields) in sent {
        let fields = serde_json::to_value(fields).unwrap();
        let fields = fields.as_object().expect("the fields are an object");
        let declared = &declared[&quoted(handle)];
        assert_eq!(
            fields.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            declared
                .iter()
                .map(|f| f["name"].trim_matches('"'))
                .collect(),
            "{handle}"
        );
        for field in declared {
            let (name, ty) = (
                field["name"].trim_matches('"'),
                field["type"].trim_matches('"'),
            );
            assert!(
                shaped(&fields[name], ty),
                "{handle}.{name} is not a {ty}: {}",
                fields[name]
            );
        }
    }
}

/// The two `[[kind]]`s are claiming triggers — the issue kind the main one — and each
/// declares the settings the code reads, key for key and in order, and the four slots
/// afkd runs for it: neither kind parks, so neither has `on_park`.
#[test]
fn the_kinds_are_the_claiming_triggers_with_every_setting_typed() {
    let tables = tables(MANIFEST);
    assert_eq!(
        named(&tables, "kind"),
        [
            &table(&[
                ("name", &quoted(ISSUE_KIND)),
                ("role", "\"trigger\""),
                ("main", "true"),
                ("claims", "true"),
            ]),
            &table(&[
                ("name", &quoted(MR_KIND)),
                ("role", "\"trigger\""),
                ("claims", "true"),
            ]),
        ]
    );
    let settings = children(&tables, "kind", "kind.setting");
    assert_eq!(
        settings,
        BTreeMap::from([
            (quoted(ISSUE_KIND), setting_tables(ISSUE_SETTINGS)),
            (quoted(MR_KIND), setting_tables(MR_SETTINGS)),
        ])
    );
    // `on_run` first and without `when`; each slot passed the run, the kind's item, and
    // after the run the outcome, in that order.
    let param = |name: &str, ty: &str| table(&[("name", &quoted(name)), ("type", &quoted(ty))]);
    let kind_slots = |item: &str, handle: &str, park: bool| {
        let mut names = vec![
            ("on_run", None),
            ("on_claim", Some("pre")),
            ("on_done", Some("post")),
            ("on_fail", Some("post")),
        ];
        if park {
            names.push(("on_park", Some("post")));
        }
        names
            .into_iter()
            .map(|(name, when)| {
                let mut t = table(&[("name", &quoted(name))]);
                let mut params = vec![param("run", "afkd.Run"), param(item, handle)];
                if let Some(when) = when {
                    t.insert("when".into(), quoted(when));
                    if when == "post" {
                        params.push(param("outcome", "afkd.Outcome"));
                    }
                }
                (t, params)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        slots(&tables),
        BTreeMap::from([
            (
                quoted(ISSUE_KIND),
                kind_slots(ISSUE_VOCABULARY.handle, "Issue", false)
            ),
            (
                quoted(MR_KIND),
                kind_slots(MR_VOCABULARY.handle, "Merge_Request", false)
            ),
        ])
    );
}

/// The `[[action]]`s are the ones [`from_call`](crate::lifecycle::from_call) decodes: the
/// issue kind's [`ACTIONS`], then the same under the mr kind's prefix, in its order. Each
/// takes its kind's handle first, and then the parameters it reads — a prefixed action
/// exactly its issue twin's — and the one `[[value]]` is `me`.
#[test]
fn the_actions_and_the_value_are_the_ones_the_code_answers() {
    let tables = tables(MANIFEST);
    let names = |vocabulary: &Vocabulary| {
        ACTIONS
            .iter()
            .map(|verb| quoted(&format!("{}{verb}", vocabulary.prefix)))
            .collect::<Vec<_>>()
    };
    let actions: Vec<String> = named(&tables, "action")
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(
        actions,
        [names(&ISSUE_VOCABULARY), names(&MR_VOCABULARY)].concat()
    );

    let required = |name: &str| {
        vec![table(&[
            ("name", &quoted(name)),
            ("type", "\"string\""),
            ("required", "true"),
        ])]
    };
    let none = Vec::new;
    let reads = BTreeMap::from([
        ("assign_me", none()),
        ("unassign", none()),
        ("label_add", required("label")),
        ("label_remove", required("label")),
        ("close", none()),
        ("comment", required("text")),
    ]);
    let params = children(&tables, "action", "action.param");
    for (vocabulary, handle) in [
        (ISSUE_VOCABULARY, "Issue"),
        (MR_VOCABULARY, "Merge_Request"),
    ] {
        let item = table(&[
            ("name", &quoted(vocabulary.handle)),
            ("type", &quoted(handle)),
            ("required", "true"),
        ]);
        for verb in ACTIONS {
            let name = format!("{}{verb}", vocabulary.prefix);
            assert_eq!(
                params[&quoted(&name)],
                [vec![item.clone()], reads[verb].clone()].concat(),
                "{name}"
            );
        }
    }

    assert_eq!(
        named(&tables, "value"),
        [&table(&[("name", &quoted("me")), ("type", "\"string\"")])]
    );
}

/// A plugin's kinds, actions, values and handle types share one namespace (lang-v2
/// §16.3), and afkd's install refuses a collision; so no name is declared twice across
/// them.
#[test]
fn no_kind_action_value_or_handle_shares_a_name() {
    let tables = tables(MANIFEST);
    let mut names: Vec<&str> = ["handle", "kind", "action", "value"]
        .iter()
        .flat_map(|header| named(&tables, header))
        .map(|t| t["name"].as_str())
        .collect();
    let all = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all, "{names:?}");
}
