//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd
//! types a config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = value` lines, each value kept as written.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::action::{ACTIONS, HANDLE as ACTION_HANDLE, RETURNING};
use crate::botapi::FileKind;
use crate::message::{handle, MessageFields};
use crate::settings::{Declared, CONFIG_SETTINGS, SETTINGS};
use crate::trigger::MESSAGE_KIND;

/// The name every slot passes the message under.
const HANDLE: &str = "message";

/// The handle type's name.
const MESSAGE: &str = "Message";

const MANIFEST: &str = include_str!("../afkd-plugin.toml");

/// One table of the manifest: its header (`""` for the top level) and its keys, each
/// with its value as written (`"message"`, `true`, `"List[str]"`).
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

/// Each `parent` table with the `child` tables written under it, in order. A `parent`
/// table ends at the next table that is neither it nor its `child`.
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

/// The tables `declared` spells, each its name, its type and its other keys.
fn declared(declared: &[Declared]) -> Vec<BTreeMap<String, String>> {
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
fn the_manifest_names_the_plugin_and_its_finished_binary() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/telegram"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("exec", env!("CARGO_PKG_NAME")),
        ("version", env!("CARGO_PKG_VERSION")),
    ] {
        assert_eq!(top[key], quoted(value), "{key}");
    }
    assert_eq!(top["proto"], crate::wire::PROTO.to_string());
    assert!(!top.contains_key("build"), "{top:?}");
    for obsolete in ["trigger", "trigger.block", "worker", "command", "kind.hook"] {
        assert!(named(&tables, obsolete).is_empty(), "{obsolete}");
    }
}

/// The plugin's own settings file declares what a child started for one call reads, and
/// requires neither, so a config that imports the plugin needs no file.
#[test]
fn the_config_declares_the_call_credentials() {
    let tables = tables(MANIFEST);
    let config: Vec<BTreeMap<String, String>> = named(&tables, "config.setting")
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(config, declared(CONFIG_SETTINGS));
    assert!(config.iter().all(|s| !s.contains_key("required")));
}

/// Whether `value` has the JSON shape afkd reads a handle field of type `ty` from.
fn shaped(value: &Value, ty: &str) -> bool {
    match ty {
        "str" => value.is_string(),
        "List[str]" => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        _ => false,
    }
}

/// The fields a handle carries are exactly the ones the `[[handle]]` declares, each in the
/// JSON shape of its declared type — a rich message's and a bare one's alike, the bare one a
/// message a send gave back. afkd refuses a unit or a returned handle carrying a field
/// undeclared or mistyped, and a declared one never sent faults the slot that reads it.
#[test]
fn every_declared_handle_field_is_sent_with_its_declared_type() {
    let tables = tables(MANIFEST);
    let rich = MessageFields {
        chat: "-1001234567890".into(),
        sender: "111111111".into(),
        sender_name: "Anna Lindqvist".into(),
        text: "デプロイ failed 🚨".into(),
        reply_to: "4710".into(),
        files: vec!["AgACAgQAAxkBAAIBPWcS".into()],
    };
    let sent = handle(
        &serde_json::from_str(
            r#"{"message_id":4712,"from":{"id":7712345678,"is_bot":true,"first_name":"afkd"},
                "chat":{"id":-1001234567890,"type":"supergroup","title":"Drift"},"date":1,
                "text":"klart"}"#,
        )
        .unwrap(),
    )
    .fields;
    let handles = grouped(&tables, "handle", "handle.field");
    assert_eq!(handles.len(), 1, "one handle, the message");
    let (handle, fields) = &handles[0];
    assert_eq!(handle["name"], quoted(MESSAGE));
    let declared: BTreeSet<&str> = fields.iter().map(|f| f["name"].trim_matches('"')).collect();
    for fields_sent in [rich, sent] {
        let value = serde_json::to_value(fields_sent).unwrap();
        let object = value.as_object().expect("the fields are an object");
        assert_eq!(
            object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            declared
        );
        for field in fields {
            let (name, ty) = (
                field["name"].trim_matches('"'),
                field["type"].trim_matches('"'),
            );
            assert!(
                shaped(&object[name], ty),
                "`{name}` is not a {ty}: {}",
                object[name]
            );
        }
    }
}

/// The one `[[kind]]` is the main trigger, claiming one message per run, and declares the
/// settings the code reads, key for key and in order — never afkd's own claim keys — and
/// the four slots afkd runs.
#[test]
fn the_kind_is_the_main_claiming_trigger_with_every_setting_typed() {
    let tables = tables(MANIFEST);
    assert_eq!(
        named(&tables, "kind"),
        [&table(&[
            ("name", &quoted(MESSAGE_KIND)),
            ("role", "\"trigger\""),
            ("main", "true"),
            ("claims", "true"),
        ])]
    );
    let settings: Vec<BTreeMap<String, String>> = named(&tables, "kind.setting")
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(settings, declared(SETTINGS));
    for afkds in ["every", "max_attempts"] {
        assert!(
            settings.iter().all(|s| s["name"] != quoted(afkds)),
            "`{afkds}` is afkd's to declare"
        );
    }
    let param = |name: &str, ty: &str| table(&[("name", &quoted(name)), ("type", &quoted(ty))]);
    let slot = |name: &str, when: Option<&str>| {
        let mut t = table(&[("name", &quoted(name))]);
        let mut params = Vec::new();
        if when != Some("pre") {
            params.push(param("run", "afkd.Run"));
        }
        params.push(param(HANDLE, MESSAGE));
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
        ],
        "no on_park: nothing parks a message"
    );
}

/// The `[[action]]`s are the ones [`from_call`](crate::action::from_call) decodes, in its
/// order: a chat and a file are strings in every call, a message is the handle a slot is
/// passed, and the three that make a message return it.
#[test]
fn the_actions_are_the_ones_the_code_answers() {
    let tables = tables(MANIFEST);
    let actions = grouped(&tables, "action", "action.param");
    assert_eq!(
        actions
            .iter()
            .map(|(a, _)| a["name"].clone())
            .collect::<Vec<_>>(),
        ACTIONS.iter().map(|a| quoted(a)).collect::<Vec<_>>()
    );
    for (action, _) in &actions {
        let name = action["name"].trim_matches('"');
        let returns = RETURNING.contains(&name).then(|| quoted(MESSAGE));
        assert_eq!(action.get("returns").cloned(), returns, "{name}");
    }

    let required = |name: &str, ty: &str| {
        table(&[
            ("name", &quoted(name)),
            ("type", &quoted(ty)),
            ("required", "true"),
        ])
    };
    let message = required(ACTION_HANDLE, MESSAGE);
    let words = FileKind::ALL.map(|k| quoted(k.word())).join(", ");
    let want = BTreeMap::from([
        (
            "send",
            vec![required("chat", "str"), required("text", "str")],
        ),
        (
            "send_file",
            vec![
                required("chat", "str"),
                required("path", "str"),
                table(&[
                    ("name", "\"kind\""),
                    ("type", "\"enum\""),
                    ("values", &format!("[{words}]")),
                    ("named_only", "true"),
                    ("default", "\"document\""),
                ]),
                table(&[
                    ("name", "\"caption\""),
                    ("type", "\"str\""),
                    ("named_only", "true"),
                    ("default", "\"\""),
                ]),
            ],
        ),
        ("reply", vec![message.clone(), required("text", "str")]),
        ("react", vec![message.clone(), required("emoji", "str")]),
        ("edit", vec![message.clone(), required("text", "str")]),
        ("delete", vec![message]),
        (
            "fetch_file",
            vec![required("file", "str"), required("to", "str")],
        ),
    ]);
    let got: BTreeMap<&str, Vec<BTreeMap<String, String>>> = actions
        .iter()
        .map(|(a, params)| (a["name"].trim_matches('"'), params.clone()))
        .collect();
    assert_eq!(got, want);
    assert_eq!(
        ACTION_HANDLE, HANDLE,
        "an action takes the message a slot is passed"
    );
    assert!(
        named(&tables, "value").is_empty(),
        "a plugin exposes no values"
    );
}

/// A plugin's kinds, actions, withables and handle types share one namespace, and afkd's
/// install refuses a collision; so no name is declared twice across them.
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
