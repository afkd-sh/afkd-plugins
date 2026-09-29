//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd
//! types a config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = value` lines, each value kept as written.

use std::collections::BTreeMap;

use crate::lifecycle::ACTIONS;
use crate::plugin::TRELLO_KIND;
use crate::settings::{ME, SETTINGS};

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each
/// with its value as written (`"card"`, `true`, `["top", "bottom"]`).
type Table = (String, BTreeMap<String, String>);

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
    // No proto 1 table survives: afkd refuses a v2 manifest carrying one.
    for proto1 in ["trigger", "trigger.block", "worker", "command"] {
        assert!(named(&tables, proto1).is_empty(), "{proto1}");
    }
}

/// The one `[[kind]]` is the main trigger, claiming one card per run, and declares the
/// settings the code reads, key for key and in order, and the four hooks afkd runs.
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
    let hooks: Vec<BTreeMap<String, String>> =
        named(&tables, "kind.hook").into_iter().cloned().collect();
    assert_eq!(
        hooks,
        [
            ("on_claim", "pre"),
            ("on_done", "post"),
            ("on_fail", "post"),
            ("on_park", "post"),
        ]
        .map(|(name, when)| table(&[("name", &quoted(name)), ("when", &quoted(when))]))
    );
}

/// The `[[action]]`s are the ones [`from_call`](crate::lifecycle::from_call) decodes, in
/// its order, each with the parameters it reads; and the one `[[value]]` is `me`.
#[test]
fn the_actions_and_the_value_are_the_ones_the_code_answers() {
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
    let mut want = BTreeMap::from([
        ("add_label".to_string(), required("label")),
        ("remove_label".to_string(), required("label")),
        ("add_member".to_string(), required("member")),
        ("remove_member".to_string(), required("member")),
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
    assert_eq!(params, want, "mark_complete and archive take none");

    assert_eq!(
        named(&tables, "value"),
        [&table(&[("name", &quoted("me")), ("type", "\"string\"")])]
    );
    assert_eq!(ME, "me", "the value `hello` supplies is Trello's own alias");
}

/// A plugin's kinds, actions and values share one namespace (lang-v2 §16.3), and afkd's
/// install refuses a collision; so no name is declared twice across them.
#[test]
fn no_kind_action_or_value_shares_a_name() {
    let tables = tables(MANIFEST);
    let mut names: Vec<&str> = ["kind", "action", "value"]
        .iter()
        .flat_map(|header| named(&tables, header))
        .map(|t| t["name"].as_str())
        .collect();
    let all = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all, "{names:?}");
}
