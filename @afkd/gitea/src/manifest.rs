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
use crate::plugin::ISSUE_KIND;
use crate::settings::{
    ALLOWED_ISSUE_KEYS, DURATION_KEYS, REPEATABLE_ISSUE_KEYS, REQUIRED_ISSUE_KEYS,
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
        ("name", "@afkd/gitea"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("build", "cargo build --release --locked"),
        ("exec", "target/release/afkd-gitea"),
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

/// The `[[trigger]]` declares the built-in's vocabulary, key for key, and each
/// `[[trigger.block]]` the six lifecycle keys, all repeatable.
#[test]
fn a_manifest_declares_the_ported_vocabulary() {
    let tables = tables(MANIFEST);
    let triggers: Vec<_> = tables.iter().filter(|(h, _)| h == "trigger").collect();
    assert_eq!(triggers.len(), 1, "one kind in this build");
    let trigger = &triggers[0].1;
    assert_eq!(trigger["kind"], [ISSUE_KIND]);
    assert_eq!(trigger["settings"], owned(ALLOWED_ISSUE_KEYS));
    assert_eq!(trigger["required"], owned(REQUIRED_ISSUE_KEYS));
    assert_eq!(trigger["repeatable"], owned(REPEATABLE_ISSUE_KEYS));
    assert_eq!(trigger["durations"], owned(DURATION_KEYS));
    assert_eq!(trigger["run_kind"], ["issue"]);
    assert_eq!(trigger["display_name"], ["Gitea issues"]);
    assert_eq!(trigger["info_keys"], Vec::<String>::new());

    let blocks: Vec<_> = tables
        .iter()
        .filter(|(h, _)| h == "trigger.block")
        .collect();
    let paths: Vec<&str> = blocks.iter().map(|(_, b)| b["path"][0].as_str()).collect();
    assert_eq!(paths, ["on_claim", "on_done", "on_fail", "on_park"]);
    for (_, block) in blocks {
        assert_eq!(
            block["settings"],
            owned(LIFECYCLE_KEYS),
            "{:?}",
            block["path"]
        );
        assert_eq!(
            block["repeatable"],
            owned(LIFECYCLE_KEYS),
            "{:?}",
            block["path"]
        );
        assert!(!block.contains_key("required"));
    }
    // Every `on_*` setting has its block, and nothing else does.
    let moments: Vec<String> = ALLOWED_ISSUE_KEYS
        .iter()
        .filter(|k| k.starts_with("on_"))
        .map(|k| k.to_string())
        .collect();
    assert_eq!(paths, moments);
}
