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
use crate::plugin::TRELLO_KIND;
use crate::settings::{ALLOWED_KEYS, DURATION_KEYS, REPEATABLE_KEYS, REQUIRED_KEYS};

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
        ("name", "@afkd/trello"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("build", "cargo build --release --locked"),
        ("exec", "target/release/afkd-trello"),
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

/// The one `[[trigger]]` declares the built-in's vocabulary, key for key and in its
/// order, and the three presentation keys the card names.
#[test]
fn the_trigger_declares_the_ported_vocabulary() {
    let tables = tables(MANIFEST);
    let triggers: Vec<&BTreeMap<String, Vec<String>>> = tables
        .iter()
        .filter(|(header, _)| header == "trigger")
        .map(|(_, table)| table)
        .collect();
    assert_eq!(triggers.len(), 1, "one kind");
    let table = triggers[0];
    assert_eq!(table["kind"], [TRELLO_KIND]);
    assert_eq!(table["settings"], owned(ALLOWED_KEYS));
    assert_eq!(table["required"], owned(REQUIRED_KEYS));
    assert_eq!(table["repeatable"], owned(REPEATABLE_KEYS));
    assert_eq!(table["durations"], owned(DURATION_KEYS));
    assert_eq!(table["run_kind"], ["card"]);
    assert_eq!(table["display_name"], ["Trello"]);
    assert_eq!(
        table["info_keys"],
        owned(&[
            "board",
            "pick_from",
            "require_label",
            "require_member",
            "poll_interval",
            "max_attempts",
        ])
    );
    // afkd refuses an info key outside `settings`.
    assert!(table["info_keys"]
        .iter()
        .all(|k| ALLOWED_KEYS.contains(&k.as_str())));
}

/// Each `on_*` setting has its block, declaring the eight lifecycle keys, all
/// repeatable; each of those has a `move_to` block declaring `at`, repeatable too (the
/// built-in reads the first `at` and ignores a second); and no other block exists.
#[test]
fn every_moment_declares_the_lifecycle_vocabulary_and_its_move_to_block() {
    let tables = tables(MANIFEST);
    let blocks: Vec<&BTreeMap<String, Vec<String>>> = tables
        .iter()
        .filter(|(header, _)| header == "trigger.block")
        .map(|(_, table)| table)
        .collect();
    let moments: Vec<&str> = ALLOWED_KEYS
        .iter()
        .copied()
        .filter(|k| k.starts_with("on_"))
        .collect();
    assert_eq!(moments, ["on_claim", "on_done", "on_fail", "on_park"]);
    let mut want_paths = Vec::new();
    for moment in &moments {
        want_paths.push(moment.to_string());
        want_paths.push(format!("{moment}.move_to"));
    }
    let paths: Vec<String> = blocks.iter().map(|b| b["path"][0].clone()).collect();
    assert_eq!(paths, want_paths);
    for block in blocks {
        let path = &block["path"][0];
        assert!(!block.contains_key("required"), "{path}");
        if path.ends_with(".move_to") {
            assert_eq!(block["settings"], ["at"], "{path}");
            assert_eq!(block["repeatable"], ["at"], "{path}");
        } else {
            assert_eq!(block["settings"], owned(LIFECYCLE_KEYS), "{path}");
            assert_eq!(block["repeatable"], owned(LIFECYCLE_KEYS), "{path}");
        }
    }
}
