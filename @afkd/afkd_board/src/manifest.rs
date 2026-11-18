//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd types a
//! config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML file
//! and not generated from these constants — and why this test exists. The crate has no TOML
//! dependency, so the scan below reads exactly the shape the manifest is written in:
//! `[[table]]` headers, and `key = value` lines, each value kept as written.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::client::{LabelAnswer, ListAnswer};
use crate::reads::{decode, CardFields, CommentFields, READS};

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each with its
/// value as written (`"card"`, `true`, `"list[str]"`).
type Table = (String, BTreeMap<String, String>);

/// A table and the tables written under it: an `[[action]]` and its `[[action.param]]`s.
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

/// Each `parent` table with the `child` tables written under it, in order. A `parent` table
/// ends at the next table that is neither it nor its `child`.
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

/// `value` as written, its quotes taken off.
fn unquoted(value: &str) -> &str {
    value.trim_matches('"')
}

/// Each `parent` table by name, with the names of the `child` tables under it, in order.
fn names_under(tables: &[Table], parent: &str, child: &str) -> BTreeMap<String, Vec<String>> {
    grouped(tables, parent, child)
        .into_iter()
        .map(|(t, children)| {
            let names = children
                .iter()
                .map(|c| unquoted(&c["name"]).to_string())
                .collect();
            (unquoted(&t["name"]).to_string(), names)
        })
        .collect()
}

/// The names of the fields `value` serializes with.
fn keys<T: Serialize>(value: &T) -> BTreeSet<String> {
    let value = serde_json::to_value(value).unwrap();
    value.as_object().unwrap().keys().cloned().collect()
}

/// The names of `fields`, as a set.
fn set(fields: &[String]) -> BTreeSet<String> {
    fields.iter().cloned().collect()
}

/// The manifest's top level carries the plugin's fixed fields, at protocol 2, and no kind.
#[test]
fn the_manifest_names_the_plugin_and_its_finished_binary() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/afkd_board"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("exec", env!("CARGO_PKG_NAME")),
        ("version", env!("CARGO_PKG_VERSION")),
    ] {
        assert_eq!(top[key], format!("\"{value}\""), "{key}");
    }
    assert_eq!(top["proto"], crate::wire::PROTO.to_string());
    assert!(!top.contains_key("build"), "{top:?}");
    // Every call is a child for that one call, so no kind is declared, and a `hello` is
    // never answered.
    let headers: BTreeSet<&str> = tables.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        headers,
        BTreeSet::from([
            "",
            "struct",
            "struct.field",
            "handle",
            "handle.field",
            "action",
            "action.param"
        ])
    );
}

/// The actions are the reads the code answers, in order, each returning what the code
/// answers it with.
#[test]
fn the_actions_are_the_reads() {
    let actions: Vec<(String, String)> = grouped(&tables(MANIFEST), "action", "action.param")
        .into_iter()
        .map(|(t, _)| {
            (
                unquoted(&t["name"]).to_string(),
                unquoted(&t["returns"]).to_string(),
            )
        })
        .collect();
    let names: Vec<&str> = actions.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, READS);
    assert_eq!(
        actions,
        [
            ("card", "Card"),
            ("cards", "list[Card]"),
            ("comments", "list[Comment]"),
            ("lists", "list[List]"),
            ("labels", "list[Label]"),
        ]
        .map(|(n, r)| (n.to_string(), r.to_string()))
    );
}

/// A value of the manifest's type `ty`, as afkd sends an argument of it.
fn sample(ty: &str) -> Value {
    match ty {
        "Board" => json!({"key": "afk_board_k", "base_url": "http://127.0.0.1:9"}),
        "Card" => json!({"id": "17", "key": "afkd_board:b-1/c-17"}),
        "int" => json!(17),
        "str" => json!("retry"),
        "bool" | "bool?" => json!(true),
        "list[str]" => json!(["Todo"]),
        other => panic!("no sample of {other}"),
    }
}

/// Every parameter the manifest declares is one the code reads: with each given a value of
/// its type, every read decodes; with any one given a value of no type of it, the read is
/// refused naming that parameter.
#[test]
fn every_parameter_is_one_the_read_decodes() {
    for (action, params) in grouped(&tables(MANIFEST), "action", "action.param") {
        let action = unquoted(&action["name"]);
        let args: Map<String, Value> = params
            .iter()
            .map(|p| {
                (
                    unquoted(&p["name"]).to_string(),
                    sample(unquoted(&p["type"])),
                )
            })
            .collect();
        if let Err(e) = decode(action, &args) {
            panic!("{action} refuses {args:?}: {e}");
        }
        for param in args.keys() {
            let mut wrong = args.clone();
            wrong.insert(param.clone(), json!([{"no": "type"}]));
            let e = decode(action, &wrong).unwrap_err();
            assert!(
                e.contains(&format!("parameter {param} ")),
                "{action}({param}): {e}"
            );
        }
    }
}

/// The handles' fields are exactly the ones the code sends, and the structs' those it reads
/// and answers.
#[test]
fn the_handles_and_the_structs_carry_the_codes_fields() {
    let tables = tables(MANIFEST);
    let handles = names_under(&tables, "handle", "handle.field");
    let card = CardFields {
        number: 0,
        title: String::new(),
        description: String::new(),
        list: String::new(),
        labels: Vec::new(),
        archived: false,
        done: false,
        blocked_by: Vec::new(),
        blocking: Vec::new(),
        blocked: false,
        url: String::new(),
        created_at: 0,
        updated_at: 0,
    };
    let comment = CommentFields {
        card: 0,
        body: String::new(),
        author: None,
        run: None,
        created_at: 0,
        url: String::new(),
    };
    assert_eq!(handles.keys().collect::<Vec<_>>(), ["Card", "Comment"]);
    assert_eq!(set(&handles["Card"]), keys(&card));
    assert_eq!(set(&handles["Comment"]), keys(&comment));

    let structs = names_under(&tables, "struct", "struct.field");
    let list = ListAnswer {
        id: String::new(),
        name: String::new(),
    };
    let label = LabelAnswer {
        name: String::new(),
        colour: String::new(),
    };
    assert_eq!(
        structs.keys().collect::<Vec<_>>(),
        ["Board", "Item", "Label", "List"]
    );
    assert_eq!(structs["Board"], ["key", "base_url"]);
    assert_eq!(structs["Item"], ["text", "done"]);
    assert_eq!(set(&structs["List"]), keys(&list));
    assert_eq!(set(&structs["Label"]), keys(&label));
}
