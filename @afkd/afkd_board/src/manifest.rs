//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd types a
//! config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML file
//! and not generated from these constants — and why this test exists. The crate has no TOML
//! dependency, so the scan below reads exactly the shape the manifest is written in:
//! `[[table]]` headers, and `key = value` lines, each value kept as written, bar `doc`: the
//! line of prose on an item is for the editor, and `checks/` holds it to its rule.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::client::{ItemAnswer, LabelAnswer, ListAnswer};
use crate::reads::{decode, CardFields, CommentFields, READS};
use crate::writes::{decode as decode_write, WRITES};

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
            if key.trim() == "doc" {
                continue;
            }
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

/// The actions are the reads and then the writes the code answers, in order, each returning
/// what the code answers it with, a write of a list's deletion nothing.
#[test]
fn the_actions_are_the_reads_and_the_writes() {
    let actions: Vec<(String, String)> = grouped(&tables(MANIFEST), "action", "action.param")
        .into_iter()
        .map(|(t, _)| {
            (
                unquoted(&t["name"]).to_string(),
                t.get("returns").map_or("", |r| unquoted(r)).to_string(),
            )
        })
        .collect();
    let names: Vec<&str> = actions.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, [READS, WRITES].concat());
    assert_eq!(
        actions,
        [
            ("card", "Card"),
            ("cards", "list[Card]"),
            ("comments", "list[Comment]"),
            ("checklist", "list[Item]"),
            ("lists", "list[List]"),
            ("labels", "list[Label]"),
            ("create_card", "Card"),
            ("update_card", "Card"),
            ("move_card", "Card"),
            ("add_label", "Card"),
            ("remove_label", "Card"),
            ("add_item", "Card"),
            ("check_item", "Card"),
            ("remove_item", "Card"),
            ("mark_done", "Card"),
            ("add_blocked_by", "Card"),
            ("remove_blocked_by", "Card"),
            ("create_comment", "Comment"),
            ("archive_card", "Card"),
            ("unarchive_card", "Card"),
            ("delete_card", "Card"),
            ("create_list", "List"),
            ("update_list", "List"),
            ("delete_list", ""),
        ]
        .map(|(n, r)| (n.to_string(), r.to_string()))
    );
}

/// Each write's signature as the manifest declares it, `afkd_board.name(…)`: a positional
/// parameter bare, one with a default as `name = value`, a named-only one as `~name`.
fn signatures() -> Vec<String> {
    grouped(&tables(MANIFEST), "action", "action.param")
        .into_iter()
        .filter(|(action, _)| WRITES.contains(&unquoted(&action["name"])))
        .map(|(action, params)| {
            let params: Vec<String> = params
                .iter()
                .map(|p| {
                    let name = unquoted(&p["name"]);
                    match (p.get("named_only"), p.get("default")) {
                        (Some(named), _) if named == "true" => format!("~{name}"),
                        (_, Some(default)) => format!("{name} = {default}"),
                        _ => name.to_string(),
                    }
                })
                .collect();
            format!(
                "afkd_board.{}({})",
                unquoted(&action["name"]),
                params.join(", ")
            )
        })
        .collect()
}

/// Every write has the signature the brief gives it, positional or named parameter by
/// parameter: `title` positional in `create_card` and named in `update_card`, `name`
/// positional in `create_list` and named in `update_list`, `run` named on every write of a card,
/// a checklist or a comment and on no write of a list.
#[test]
fn every_signature_is_the_briefs() {
    assert_eq!(
        signatures(),
        [
            "afkd_board.create_card(b, list, title, ~description, ~labels, ~checklist, ~at, ~after, ~run)",
            "afkd_board.update_card(b, card, ~title, ~description, ~run)",
            "afkd_board.move_card(b, card, list, ~at, ~after, ~run)",
            "afkd_board.add_label(b, card, label, ~run)",
            "afkd_board.remove_label(b, card, label, ~run)",
            "afkd_board.add_item(b, card, text, ~run)",
            "afkd_board.check_item(b, card, index, done = true, ~run)",
            "afkd_board.remove_item(b, card, index, ~run)",
            "afkd_board.mark_done(b, card, done = true, ~run)",
            "afkd_board.add_blocked_by(b, card, other, ~run)",
            "afkd_board.remove_blocked_by(b, card, other, ~run)",
            "afkd_board.create_comment(b, card, body, ~run)",
            "afkd_board.archive_card(b, card, ~run)",
            "afkd_board.unarchive_card(b, card, ~run)",
            "afkd_board.delete_card(b, card, ~run)",
            "afkd_board.create_list(b, name, ~at, ~after)",
            "afkd_board.update_list(b, list, ~name, ~at, ~after)",
            "afkd_board.delete_list(b, list)",
        ]
    );
}

/// The parameters the writes take by type, as the manifest writes them.
#[test]
fn the_writes_type_their_parameters() {
    let types = |action: &str, param: &str| -> (String, Option<String>) {
        let (_, params) = grouped(&tables(MANIFEST), "action", "action.param")
            .into_iter()
            .find(|(a, _)| unquoted(&a["name"]) == action)
            .unwrap();
        let p = params
            .iter()
            .find(|p| unquoted(&p["name"]) == param)
            .unwrap();
        (unquoted(&p["type"]).to_string(), p.get("values").cloned())
    };
    for action in ["create_card", "move_card", "create_list", "update_list"] {
        assert_eq!(
            types(action, "at"),
            ("enum".to_string(), Some(r#"["top", "bottom"]"#.to_string())),
            "{action}"
        );
    }
    assert_eq!(types("create_card", "after").0, "Card?");
    assert_eq!(types("move_card", "after").0, "Card?");
    assert_eq!(types("create_list", "after").0, "str");
    assert_eq!(types("update_list", "after").0, "str");
    assert_eq!(types("create_card", "checklist").0, "list[Item]");
    for action in WRITES {
        if !action.ends_with("_list") {
            assert_eq!(types(action, "run").0, "afkd.Run?", "{action}");
        }
    }
}

/// Decode `args` as a read or a write of `action`.
fn decode_any(action: &str, args: &Map<String, Value>) -> Result<(), String> {
    if READS.contains(&action) {
        decode(action, args).map(|_| ())
    } else {
        decode_write(action, args).map(|_| ())
    }
}

/// A value of the manifest's type `ty`, as afkd sends an argument of it.
fn sample(ty: &str) -> Value {
    match ty {
        "Board" => json!({"key": "afk_board_k", "base_url": "http://127.0.0.1:9"}),
        "Card" | "Card?" => json!({"id": "17", "key": "afkd_board:b-1/c-17"}),
        "int" => json!(17),
        "str" => json!("retry"),
        "bool" | "bool?" => json!(true),
        "list[str]" => json!(["Todo"]),
        "list[Item]" => json!([{"text": "Reproduce it", "done": false}]),
        "enum" => json!("top"),
        "afkd.Run?" => json!({"machine": "nexus", "service": "develop/review.0", "instance": "0",
                              "id": "261116-163511-card-17-2"}),
        other => panic!("no sample of {other}"),
    }
}

/// Every parameter the manifest declares is one the code reads: with each given a value of
/// its type, every read and write decodes; with any one given a value of no type of it, the
/// call is refused naming that parameter. `after` is left out wherever `at` is given, since the
/// two together are refused, and decoded in a pass of its own without `at`.
#[test]
fn every_parameter_is_one_the_read_decodes() {
    for (action, params) in grouped(&tables(MANIFEST), "action", "action.param") {
        let action = unquoted(&action["name"]);
        let samples: Map<String, Value> = params
            .iter()
            .map(|p| {
                (
                    unquoted(&p["name"]).to_string(),
                    sample(unquoted(&p["type"])),
                )
            })
            .collect();
        let with_at = samples.contains_key("at");
        let mut args = samples.clone();
        if with_at {
            args.remove("after");
        }
        if let Err(e) = decode_any(action, &args) {
            panic!("{action} refuses {args:?}: {e}");
        }
        for param in args.keys() {
            let mut wrong = args.clone();
            wrong.insert(param.clone(), json!([{"no": "type"}]));
            let e = decode_any(action, &wrong).unwrap_err();
            assert!(
                e.contains(&format!("parameter {param} ")),
                "{action}({param}): {e}"
            );
        }
        if with_at {
            let mut after = samples.clone();
            after.remove("at");
            if let Err(e) = decode_any(action, &after) {
                panic!("{action} refuses {after:?}: {e}");
            }
            after.insert("after".to_string(), json!([{"no": "type"}]));
            let e = decode_any(action, &after).unwrap_err();
            assert!(e.contains("parameter after "), "{action}(after): {e}");
            let mut both = samples;
            both.insert("at".to_string(), json!("top"));
            assert_eq!(
                decode_any(action, &both).unwrap_err(),
                format!("{action}: at and after cannot both be given")
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
    let item = ItemAnswer {
        text: String::new(),
        done: false,
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
    assert_eq!(set(&structs["Item"]), keys(&item));
    assert_eq!(set(&structs["List"]), keys(&list));
    assert_eq!(set(&structs["Label"]), keys(&label));
}
