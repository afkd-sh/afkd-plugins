//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd
//! types a config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = value` lines, each value kept as written.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::lifecycle::{ACTIONS, HANDLE};
use crate::plugin::TRELLO_KIND;
use crate::settings::{ME, SETTINGS};
use crate::wire::CardFields;

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each
/// with its value as written (`"card"`, `true`, `["top", "bottom"]`).
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

/// The manifest's top level carries the plugin's fixed fields, at protocol 2.
#[test]
fn the_manifest_names_the_plugin_and_how_it_builds() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/trello"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("build", "cargo build --release --locked"),
        ("exec", "target/release/afkd-trello"),
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

/// The one `[[handle]]` is the card every slot is passed and every action takes first,
/// with the fields the plugin knows about a card beyond the `id` and `key` every handle
/// has.
#[test]
fn the_handle_is_the_card_with_its_fields() {
    let tables = tables(MANIFEST);
    let field = |name: &str, ty: &str| table(&[("name", &quoted(name)), ("type", &quoted(ty))]);
    assert_eq!(
        grouped(&tables, "handle", "handle.field"),
        [(
            table(&[("name", "\"Card\"")]),
            vec![
                field("title", "string"),
                field("url", "string"),
                field("labels", "list[string]"),
            ]
        )]
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

/// The fields a claim sends are exactly the ones the `[[handle]]` declares, each in the
/// JSON shape of its declared type — a rich card's and a bare one's alike. afkd refuses a
/// unit carrying a field undeclared or mistyped, and a declared one never sent faults the
/// slot that reads it, so a field added to either side alone fails here.
#[test]
fn every_declared_handle_field_is_sent_with_its_declared_type() {
    let tables = tables(MANIFEST);
    let rich = CardFields {
        title: "修复 the retry storm 🚨 — \"backoff\" resets".into(),
        url: "https://trello.com/c/Rk7eLy5w/12-the-retry-storm".into(),
        labels: vec!["afkd/ready".into(), "Väntar på svar".into()],
    };
    let bare = CardFields {
        title: String::new(),
        url: String::new(),
        labels: Vec::new(),
    };
    let handles = grouped(&tables, "handle", "handle.field");
    assert_eq!(handles.len(), 1, "one handle, the card");
    let (handle, fields) = &handles[0];
    assert_eq!(handle["name"], quoted("Card"));
    let declared: BTreeSet<&str> = fields.iter().map(|f| f["name"].trim_matches('"')).collect();
    for sent in [rich, bare] {
        let sent = serde_json::to_value(sent).unwrap();
        let sent = sent.as_object().expect("the fields are an object");
        assert_eq!(
            sent.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            declared
        );
        for field in fields {
            let (name, ty) = (
                field["name"].trim_matches('"'),
                field["type"].trim_matches('"'),
            );
            assert!(
                shaped(&sent[name], ty),
                "`{name}` is not a {ty}: {}",
                sent[name]
            );
        }
    }
}

/// The one `[[kind]]` is the main trigger, claiming one card per run, and declares the
/// settings the code reads, key for key and in order, and the five slots afkd runs.
#[test]
fn the_kind_is_the_main_claiming_trigger_with_every_setting_typed() {
    let tables = tables(MANIFEST);
    assert_eq!(
        named(&tables, "kind"),
        [&table(&[
            ("name", &quoted(TRELLO_KIND)),
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
    // `on_run` first and without `when`; each slot passed the run, except `on_claim`, run
    // before any run exists; the card; and after the run the outcome, in that order.
    let param = |name: &str, ty: &str| table(&[("name", &quoted(name)), ("type", &quoted(ty))]);
    let slot = |name: &str, when: Option<&str>| {
        let mut t = table(&[("name", &quoted(name))]);
        let mut params = Vec::new();
        if when != Some("pre") {
            params.push(param("run", "afkd.Run"));
        }
        params.push(param(HANDLE, "Card"));
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

/// The `[[action]]`s are the ones [`from_call`](crate::lifecycle::from_call) decodes, in
/// its order, each taking the card's handle first and then the parameters it reads; a member
/// action's `member` defaults to the token's own member. A plugin exposes no values, so
/// there is no `[[value]]`.
#[test]
fn the_actions_are_the_ones_the_code_answers() {
    let tables = tables(MANIFEST);
    let actions: Vec<String> = named(&tables, "action")
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(
        actions,
        ACTIONS.iter().map(|a| quoted(a)).collect::<Vec<_>>()
    );

    // Each `[[action.param]]` belongs to the `[[action]]` above it.
    let mut params: BTreeMap<String, Vec<BTreeMap<String, String>>> = BTreeMap::new();
    let mut action = String::new();
    for (header, t) in &tables {
        match header.as_str() {
            "action" => action = t["name"].trim_matches('"').to_string(),
            "action.param" => params.entry(action.clone()).or_default().push(t.clone()),
            _ => {}
        }
    }
    let required = |name: &str| {
        vec![table(&[
            ("name", &quoted(name)),
            ("type", "\"string\""),
            ("required", "true"),
        ])]
    };
    let member = table(&[
        ("name", "\"member\""),
        ("type", "\"string\""),
        ("default", &quoted(ME)),
    ]);
    let card = table(&[
        ("name", &quoted(HANDLE)),
        ("type", "\"Card\""),
        ("required", "true"),
    ]);
    let mut want = BTreeMap::from([
        ("add_label".to_string(), required("label")),
        ("remove_label".to_string(), required("label")),
        ("add_member".to_string(), vec![member.clone()]),
        ("remove_member".to_string(), vec![member]),
        ("comment".to_string(), required("text")),
    ]);
    let mut move_to = required("list");
    move_to.push(table(&[
        ("name", "\"at\""),
        ("type", "\"enum\""),
        ("values", "[\"top\", \"bottom\"]"),
        ("named_only", "true"),
        ("default", "\"top\""),
    ]));
    want.insert("move_to".to_string(), move_to);
    for action in ACTIONS {
        want.entry(action.to_string())
            .or_default()
            .insert(0, card.clone());
    }
    assert_eq!(
        params, want,
        "mark_complete and archive take the card alone"
    );

    assert!(
        named(&tables, "value").is_empty(),
        "a plugin exposes no values"
    );
    assert_eq!(ME, "me", "the member default is Trello's own alias");
}

/// A plugin's kinds, actions, withables and handle types share one namespace (lang-v2
/// §16.3), and afkd's install refuses a collision; so no name is declared twice across
/// them.
#[test]
fn no_kind_action_or_handle_shares_a_name() {
    let tables = tables(MANIFEST);
    let mut names: Vec<&str> = ["handle", "kind", "action"]
        .iter()
        .flat_map(|header| named(&tables, header))
        .map(|t| t["name"].as_str())
        .collect();
    let all = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all, "{names:?}");
}
