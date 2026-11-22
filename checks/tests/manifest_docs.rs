//! The doc contract afkd holds a manifest to, held here too: **every kind, setting, slot,
//! action, action parameter and handle field a plugin declares says what it does in one line**.
//!
//! The editor on afkd.sh shows a manifest's `doc` under a completion and in a hover, and afkd
//! refuses an overlong or multi-line one when it loads the plugin. So a plugin that adds an
//! item without a doc, or with one that rule refuses, fails here before a release carries it.
//! The gate reads every `afkd-plugin.toml` git tracks with a line scan, since this crate has no
//! TOML dependency, and finds each documented item by its header ([`DOCUMENTED`]). A doc is
//! held to [`doc_fault`]:
//!
//! - it is there, as one quoted string;
//! - it is at most [`MAX_DOC`] characters, on one line (no `\n` escape, no `"""` string);
//! - it ends in no full stop, as the standard library's own docs do not;
//! - it holds no `#`, which the manifest tests of the plugins cut a line at.
//!
//! The structs, enums and a slot's parameters are not documented items, so they carry none.

mod common;

use std::path::Path;

use common::{tracked, REPO_ROOT};

/// The most characters a doc holds.
const MAX_DOC: usize = 80;

/// The table headers whose items carry a doc, each with the header of the table that holds it
/// (`""` for a top-level one), so a fault names an item by where it is.
const DOCUMENTED: &[(&str, &str)] = &[
    ("kind", ""),
    ("kind.setting", "kind"),
    ("kind.slot", "kind"),
    ("action", ""),
    ("action.param", "action"),
    ("handle.field", "handle"),
    ("config.setting", ""),
];

/// The headers of the tables that hold documented items, whose names the scan keeps.
const HOLDERS: &[&str] = &["kind", "action", "handle"];

/// One documented item of a manifest: the header of its table, its name qualified by the
/// table that holds it (`move_to.at`), and its `doc` as written, if it has one.
#[derive(Debug, PartialEq, Eq)]
struct Item {
    header: String,
    name: String,
    doc: Option<String>,
}

/// What is wrong with a doc, if anything: `value` is the doc as written in the manifest, after
/// the `=` and trimmed (`"Moves the card to a list"`), or `None` for an item with no `doc`.
fn doc_fault(value: Option<&str>) -> Option<String> {
    let Some(value) = value else {
        return Some("has no doc".to_owned());
    };
    if value.starts_with("\"\"\"") {
        return Some("is a multi-line string, and a doc is one line".to_owned());
    }
    let Some(text) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return Some(format!("is not one quoted string: {value}"));
    };
    if text.contains("\\n") {
        return Some("holds a line break, and a doc is one line".to_owned());
    }
    let length = text.chars().count();
    if length > MAX_DOC {
        return Some(format!("is {length} characters, over {MAX_DOC}"));
    }
    if text.ends_with('.') {
        return Some("ends in a full stop".to_owned());
    }
    if text.contains('#') {
        return Some("holds a `#`".to_owned());
    }
    None
}

/// The documented items of the manifest `text`, in order, found by their `[[header]]` lines.
/// A table's `name` and `doc` are the `key = value` lines under its header, up to the next
/// header; a comment line is not one, and a value is kept as written, `#` and all.
fn items(text: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    // The name of the last `[[kind]]`, `[[action]]` and `[[handle]]`, which the items under
    // each are named after.
    let mut holders: Vec<(&str, String)> = Vec::new();
    // The header of the table being read, `""` before the first.
    let mut header = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            header = line
                .strip_prefix("[[")
                .and_then(|l| l.strip_suffix("]]"))
                .map_or("", str::trim);
            if DOCUMENTED.iter().any(|(h, _)| *h == header) {
                items.push(Item {
                    header: header.to_owned(),
                    name: String::new(),
                    doc: None,
                });
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| !line.starts_with('#')) else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let documented = DOCUMENTED.iter().find(|(h, _)| *h == header);
        if key == "name" {
            let name = value.trim_matches('"');
            if let Some(holder) = HOLDERS.iter().find(|h| **h == header) {
                holders.retain(|(h, _)| h != holder);
                holders.push((*holder, name.to_owned()));
            }
            if let Some((_, holder)) = documented {
                let item = items.last_mut().expect("a documented table has an item");
                item.name = match holders.iter().find(|(h, _)| h == holder) {
                    Some((_, holder)) => format!("{holder}.{name}"),
                    None => name.to_owned(),
                };
            }
        } else if key == "doc" && documented.is_some() {
            items
                .last_mut()
                .expect("a documented table has an item")
                .doc = Some(value.to_owned());
        }
    }
    items
}

/// Every `afkd-plugin.toml` git tracks, by its path under the repository root.
fn manifests() -> Vec<String> {
    tracked()
        .into_iter()
        .filter(|path| path.ends_with("/afkd-plugin.toml"))
        .collect()
}

/// A doc as the manifest writes it.
fn written(text: &str) -> String {
    format!("\"{text}\"")
}

#[test]
fn a_doc_is_there_and_one_short_line_with_no_full_stop_or_hash() {
    let longest = "x".repeat(MAX_DOC);
    assert_eq!(doc_fault(Some(&written(&longest))), None);
    assert_eq!(doc_fault(Some(&written("Moves the card to a list"))), None);
    // Wide text counts by character, not by byte.
    assert_eq!(doc_fault(Some(&written(&"秘".repeat(MAX_DOC)))), None);

    let faults = [
        (None, "has no doc"),
        (
            Some(written(&"x".repeat(MAX_DOC + 1))),
            "is 81 characters, over 80",
        ),
        (
            Some(written("Moves the card\\nto a list")),
            "holds a line break",
        ),
        (
            Some("\"\"\"Moves the card\nto a list\"\"\"".to_owned()),
            "is a multi-line string",
        ),
        (
            Some(written("Moves the card to a list.")),
            "ends in a full stop",
        ),
        (Some(written("Card #17 moved")), "holds a `#`"),
        (
            Some("'Moves the card'".to_owned()),
            "is not one quoted string",
        ),
    ];
    for (value, fault) in faults {
        let found = doc_fault(value.as_deref());
        assert!(
            found.as_deref().is_some_and(|f| f.starts_with(fault)),
            "{value:?}: {found:?}"
        );
    }
}

#[test]
fn an_item_is_found_by_its_header_and_named_under_what_holds_it() {
    let manifest = r##"
name = "@afkd/demo"

# A comment, with a name = "not a name" in it.
[[enum]]
name = "Sort"

[[handle]]
name = "Card"

  [[handle.field]]
  name = "title"
  doc = "The card's title"

[[kind]]
name = "card"
doc = "A board"

  [[kind.setting]]
  name = "board"
  doc = "The board, as a #1 address."

  [[kind.slot]]
  name = "on_run"

    [[kind.slot.param]]
    name = "card"
    type = "Card"

[[action]]
name = "move_to"

  [[action.param]]
  name = "at"
  doc = """top
bottom"""
"##;
    let item = |header: &str, name: &str, doc: Option<&str>| Item {
        header: header.to_owned(),
        name: name.to_owned(),
        doc: doc.map(str::to_owned),
    };
    assert_eq!(
        items(manifest),
        [
            item("handle.field", "Card.title", Some("\"The card's title\"")),
            item("kind", "card", Some("\"A board\"")),
            item(
                "kind.setting",
                "card.board",
                Some("\"The board, as a #1 address.\"")
            ),
            item("kind.slot", "card.on_run", None),
            item("action", "move_to", None),
            item("action.param", "move_to.at", Some("\"\"\"top")),
        ]
    );
}

#[test]
fn every_first_party_manifest_documents_every_item() {
    let root = Path::new(REPO_ROOT);
    let manifests = manifests();
    assert!(
        manifests.len() >= 8,
        "the sweep reads the eight first-party manifests, but found {manifests:?}"
    );
    let mut found = Vec::new();
    for path in &manifests {
        let text = match std::fs::read_to_string(root.join(path)) {
            Ok(text) => text,
            // Deleted in the working tree but not yet from the index: not in the tree.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("cannot read {path}: {e}"),
        };
        let items = items(&text);
        assert!(
            !items.is_empty(),
            "{path}: the scan found no documented item, so it holds nothing to the rule"
        );
        for item in items {
            if let Some(fault) = doc_fault(item.doc.as_deref()) {
                found.push(format!(
                    "{path}: [[{}]] {}: {fault}",
                    item.header, item.name
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "{} item(s) of a manifest break the doc rule: one line of at most {MAX_DOC} characters, \
         no full stop, no `#`, on every kind, setting, slot, action, parameter and handle \
         field:\n  {}",
        found.len(),
        found.join("\n  ")
    );
}
