//! Holds `afkd-plugin.toml` to the vocabulary this crate ported, so the manifest afkd
//! reads and the code that reads the settings cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = "string"` or `key = ["string", …]` lines, an array
//! possibly spread over several lines.

use std::collections::BTreeMap;

use crate::lifecycle::LIFECYCLE_KEYS;
use crate::plugin::{ISSUE_KIND, PR_KIND};
use crate::settings::{
    ALLOWED_ISSUE_KEYS, ALLOWED_PR_KEYS, DURATION_KEYS, REPEATABLE_ISSUE_KEYS, REPEATABLE_PR_KEYS,
    REQUIRED_ISSUE_KEYS, REQUIRED_PR_KEYS,
};

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each
/// with the strings its value holds.
type Table = (String, BTreeMap<String, Vec<String>>);

/// Scan the manifest into its tables, in order.
fn tables(text: &str) -> Vec<Table> {
    let mut tables: Vec<Table> = vec![(String::new(), BTreeMap::new())];
    let mut open: Option<(String, String)> = None;
    for line in text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
    {
        if let Some((key, mut held)) = open.take() {
            held.push(' ');
            held.push_str(line);
            if line.ends_with(']') {
                let table = &mut tables.last_mut().unwrap().1;
                table.insert(key, strings(&held));
            } else {
                open = Some((key, held));
            }
            continue;
        }
        if let Some(header) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            tables.push((header.to_string(), BTreeMap::new()));
        } else if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim().to_string(), value.trim());
            if value.starts_with('[') && !value.ends_with(']') {
                open = Some((key, value.to_string()));
            } else {
                tables.last_mut().unwrap().1.insert(key, strings(value));
            }
        }
    }
    tables
}

/// Every double-quoted string in `value`, in order (the manifest escapes none).
fn strings(value: &str) -> Vec<String> {
    value
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn owned(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

/// The manifest's top level carries the card's fixed fields.
#[test]
fn the_manifest_names_the_plugin_and_how_it_builds() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/github"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("build", "cargo build --release --locked"),
        ("exec", "target/release/afkd-github"),
        ("version", env!("CARGO_PKG_VERSION")),
    ] {
        assert_eq!(top[key], [value], "{key}");
    }
    assert!(MANIFEST.lines().any(|l| l.trim() == "proto = 1"));
    // `exec` names what `build` produces: the crate's own binary.
    assert_eq!(
        top["exec"][0],
        format!("target/release/{}", env!("CARGO_PKG_NAME"))
    );
}

/// One `[[trigger]]` and the `[[trigger.block]]` tables written after it, before the
/// next `[[trigger]]`.
struct Trigger<'a> {
    table: &'a BTreeMap<String, Vec<String>>,
    blocks: Vec<&'a BTreeMap<String, Vec<String>>>,
}

/// The manifest's triggers, in order, each with its blocks.
fn triggers(tables: &[Table]) -> Vec<Trigger<'_>> {
    let mut triggers: Vec<Trigger> = Vec::new();
    for (header, table) in tables {
        match header.as_str() {
            "trigger" => triggers.push(Trigger {
                table,
                blocks: Vec::new(),
            }),
            "trigger.block" => triggers
                .last_mut()
                .expect("a block belongs to a trigger")
                .blocks
                .push(table),
            _ => {}
        }
    }
    triggers
}

/// Each `[[trigger]]` declares its built-in's vocabulary, key for key, and each of its
/// `[[trigger.block]]`s the six lifecycle keys, all repeatable — one block per `on_*`
/// setting the kind has, and no other.
#[test]
fn a_manifest_declares_the_ported_vocabulary() {
    let tables = tables(MANIFEST);
    let triggers = triggers(&tables);
    let kinds: Vec<&str> = triggers
        .iter()
        .map(|t| t.table["kind"][0].as_str())
        .collect();
    assert_eq!(kinds, [ISSUE_KIND, PR_KIND], "both kinds, in order");
    for (trigger, (allowed, required, repeatable, run_kind, display_name, paths)) in
        triggers.iter().zip([
            (
                ALLOWED_ISSUE_KEYS,
                REQUIRED_ISSUE_KEYS,
                REPEATABLE_ISSUE_KEYS,
                "issue",
                "GitHub issues",
                &["on_claim", "on_done", "on_fail"][..],
            ),
            (
                ALLOWED_PR_KEYS,
                REQUIRED_PR_KEYS,
                REPEATABLE_PR_KEYS,
                "pr",
                "GitHub PRs",
                &["on_claim", "on_done", "on_fail"][..],
            ),
        ])
    {
        let (kind, table) = (&trigger.table["kind"][0], trigger.table);
        assert_eq!(table["settings"], owned(allowed), "{kind}");
        assert_eq!(table["required"], owned(required), "{kind}");
        assert_eq!(table["repeatable"], owned(repeatable), "{kind}");
        assert_eq!(table["durations"], owned(DURATION_KEYS), "{kind}");
        assert_eq!(table["run_kind"], [run_kind], "{kind}");
        assert_eq!(table["display_name"], [display_name], "{kind}");
        assert_eq!(table["info_keys"], Vec::<String>::new(), "{kind}");

        let blocks: Vec<&str> = trigger
            .blocks
            .iter()
            .map(|b| b["path"][0].as_str())
            .collect();
        assert_eq!(blocks, paths, "{kind}");
        for block in &trigger.blocks {
            let at = (kind, &block["path"]);
            assert_eq!(block["settings"], owned(LIFECYCLE_KEYS), "{at:?}");
            assert_eq!(block["repeatable"], owned(LIFECYCLE_KEYS), "{at:?}");
            assert!(!block.contains_key("required"), "{at:?}");
        }
        // Every `on_*` setting has its block, and nothing else does.
        let moments: Vec<&str> = allowed
            .iter()
            .copied()
            .filter(|k| k.starts_with("on_"))
            .collect();
        assert_eq!(blocks, moments, "{kind}");
    }
}
