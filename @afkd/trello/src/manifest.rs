//! Holds `afkd-plugin.toml` to the vocabulary this crate reads, so the manifest afkd
//! types a config against and the code that answers it cannot drift apart.
//!
//! The manifest is data afkd reads without running anything, which is why it is a TOML
//! file and not generated from these constants — and why this test exists. The crate has
//! no TOML dependency, so the scan below reads exactly the shape the manifest is written
//! in: `[[table]]` headers, and `key = value` lines, each value kept as written, bar `doc`:
//! the line of prose on an item is for the editor, and `checks/` holds it to its rule.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::client::TRELLO_BASE;
use crate::handles::{
    AttachmentFields, BoardFields, CardFields, CheckItemFields, ChecklistFields, CommentFields,
    LabelFields, ListFields, MemberFields,
};
use crate::lifecycle::{ACTIONS, HANDLE};
use crate::plugin::TRELLO_KIND;
use crate::reads::READS;
use crate::settings::{ME, SETTINGS};
use crate::writes::WRITES;

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
            if key.trim() == "doc" {
                continue;
            }
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
fn the_manifest_names_the_plugin_and_its_finished_binary() {
    let tables = tables(MANIFEST);
    let top = &tables[0].1;
    for (key, value) in [
        ("name", "@afkd/trello"),
        ("shape", "provider"),
        ("source", "https://github.com/afkd-sh/afkd-plugins"),
        ("exec", env!("CARGO_PKG_NAME")),
        ("version", env!("CARGO_PKG_VERSION")),
    ] {
        assert_eq!(top[key], quoted(value), "{key}");
    }
    assert_eq!(top["proto"], crate::wire::PROTO.to_string());
    // The release ships the crate's binary finished, beside the manifest; afkd never builds
    // a plugin, and refuses a manifest that asks it to.
    assert!(!top.contains_key("build"), "{top:?}");
    // No proto 1 table survives, nor the hooks slots replaced: afkd refuses a v2
    // manifest carrying one.
    for obsolete in ["trigger", "trigger.block", "worker", "command", "kind.hook"] {
        assert!(named(&tables, obsolete).is_empty(), "{obsolete}");
    }
}

/// The one `[[struct]]` is `trello.Account`: the key and the token, each marked secret, and
/// Trello's API root as the base's default.
#[test]
fn the_struct_is_the_account_a_read_reaches() {
    let tables = tables(MANIFEST);
    let field = |name: &str, ty: &str, extra: &[(&str, &str)]| {
        let mut t = table(extra);
        t.insert("name".into(), quoted(name));
        t.insert("type".into(), quoted(ty));
        t
    };
    assert_eq!(
        grouped(&tables, "struct", "struct.field"),
        [(
            table(&[("name", "\"Account\"")]),
            vec![
                field("api_key", "str", &[("secret", "true")]),
                field("token", "str", &[("secret", "true")]),
                field("base_url", "str", &[("default", &quoted(TRELLO_BASE))]),
            ]
        )]
    );
}

/// Whether `value` has the JSON shape afkd reads a handle field of type `ty` from: `null`
/// for a `T?`, and unix milliseconds for a `time`.
fn shaped(value: &Value, ty: &str) -> bool {
    if let Some(inner) = ty.strip_suffix('?') {
        return value.is_null() || shaped(value, inner);
    }
    match ty {
        "str" => value.is_string(),
        "int" => value.is_i64() || value.is_u64(),
        "bool" => value.is_boolean(),
        "time" => value.is_u64(),
        "list[str]" => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        _ => false,
    }
}

/// A rich value and a bare one of each handle's fields, as the plugin sends them: every
/// field set, wide text and all, and every optional field `null` and every list empty.
fn handle_fields() -> Vec<(&'static str, [Value; 2])> {
    fn both<F: Serialize>(rich: &F, bare: &F) -> [Value; 2] {
        [
            serde_json::to_value(rich).unwrap(),
            serde_json::to_value(bare).unwrap(),
        ]
    }
    let words = |w: &[&str]| -> Vec<String> { w.iter().map(|s| s.to_string()).collect() };
    let rich_card = CardFields {
        title: "修复 the retry storm 🚨 — \"backoff\" resets".into(),
        number: 12,
        name: "修复 the retry storm 🚨 — \"backoff\" resets".into(),
        url: "https://trello.com/c/Rk7eLy5w/12-the-retry-storm".into(),
        description: "## 背景\n\nIt retries forever.".into(),
        board: "1Rkelydw".into(),
        list: "Väntar / Blockerat".into(),
        labels: words(&["afkd/ready", "Väntar på svar"]),
        members: words(&["phil", "陳大文"]),
        due: Some(1_798_106_400_250),
        due_complete: true,
        archived: true,
        created_at: 1_793_095_080_000,
        last_activity_at: 1_793_095_380_500,
    };
    let bare_card = CardFields {
        title: String::new(),
        number: 0,
        name: String::new(),
        url: String::new(),
        description: String::new(),
        board: String::new(),
        list: String::new(),
        labels: Vec::new(),
        members: Vec::new(),
        due: None,
        due_complete: false,
        archived: false,
        created_at: 0,
        last_activity_at: 0,
    };
    let comment = |rich: bool| CommentFields {
        card: "Rk7eLy5w".into(),
        body: if rich {
            "看起来不对 🚨\n\n```\nx\n```"
        } else {
            ""
        }
        .into(),
        author: rich.then(|| "陳大文".into()),
        created_at: 1_793_095_080_087,
        edited_at: rich.then_some(1_793_095_380_500),
    };
    let checklist = |rich: bool| ChecklistFields {
        card: "Rk7eLy5w".into(),
        name: "Acceptance ✅".into(),
        items: if rich {
            words(&["one", "二"])
        } else {
            Vec::new()
        },
    };
    let check_item = |rich: bool| CheckItemFields {
        card: "Rk7eLy5w".into(),
        checklist: "6a4dd5de1234abcd5678ef90".into(),
        name: "二 — the second".into(),
        done: rich,
    };
    let attachment = |rich: bool| AttachmentFields {
        card: "Rk7eLy5w".into(),
        name: "plan 計画.md".into(),
        url: "https://trello.com/1/cards/c/attachments/a/download/plan.md".into(),
        bytes: rich.then_some(1234),
        mime_type: rich.then(|| "text/markdown".into()),
        created_at: 1_793_095_080_007,
        is_upload: rich,
    };
    let board = |rich: bool| BoardFields {
        name: "afkd 板".into(),
        url: "https://trello.com/b/1Rkelydw/afkd".into(),
        archived: rich,
    };
    let list = |rich: bool| ListFields {
        name: "Up for Grabs 🎁".into(),
        board: "1Rkelydw".into(),
        archived: rich,
    };
    let label = |rich: bool| LabelFields {
        name: if rich { "Bug 🐛" } else { "" }.into(),
        color: rich.then(|| "red".into()),
        board: "1Rkelydw".into(),
    };
    let member = MemberFields {
        full_name: "陳大文".into(),
    };
    vec![
        ("Card", both(&rich_card, &bare_card)),
        ("Comment", both(&comment(true), &comment(false))),
        ("Checklist", both(&checklist(true), &checklist(false))),
        ("Check_Item", both(&check_item(true), &check_item(false))),
        ("Attachment", both(&attachment(true), &attachment(false))),
        ("Board", both(&board(true), &board(false))),
        ("List", both(&list(true), &list(false))),
        ("Label", both(&label(true), &label(false))),
        ("Member", both(&member, &member)),
    ]
}

/// The `[[handle]]`s are the card every slot is passed and every action takes first, and the
/// eight other objects a read or a write hands back, in that order, each with the line saying
/// what it is.
#[test]
fn the_handles_are_the_card_and_the_eight_a_read_hands_back() {
    let tables = tables(MANIFEST);
    let handles = named(&tables, "handle");
    assert_eq!(
        handles
            .iter()
            .map(|h| h["name"].trim_matches('"'))
            .collect::<Vec<_>>(),
        handle_fields()
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    );
    // The scan keeps no `doc`, so a handle's own line is held to its presence here.
    let text = MANIFEST.lines().map(str::trim).collect::<Vec<_>>();
    for handle in &handles {
        let at = text
            .iter()
            .position(|l| *l == format!("name = {}", handle["name"]))
            .expect("the handle's name line");
        assert!(
            text[at + 1].starts_with("doc = \""),
            "{} has no doc line",
            handle["name"]
        );
    }
}

/// Every declared `[[handle]]` declares exactly the fields its code sends — a rich value's
/// and a bare one's alike — each in the JSON shape of its declared type (`time`, `time?`,
/// `bool`, `int`, `int?`, `str`, `str?` or `list[str]`). afkd refuses a handle carrying a field
/// undeclared or mistyped, and a declared one never sent faults the slot that reads it, so a
/// field added to either side alone fails here. No handle declares `id` or `key`, which every
/// handle has.
#[test]
fn every_declared_handle_field_is_sent_with_its_declared_type() {
    let tables = tables(MANIFEST);
    let handles = grouped(&tables, "handle", "handle.field");
    let sent = handle_fields();
    assert_eq!(handles.len(), sent.len());
    for ((handle, fields), (name, values)) in handles.iter().zip(&sent) {
        assert_eq!(handle["name"], quoted(name));
        let declared: BTreeSet<&str> = fields.iter().map(|f| f["name"].trim_matches('"')).collect();
        assert_eq!(
            declared.len(),
            fields.len(),
            "{name} declares a field twice"
        );
        assert!(
            !declared.contains("id") && !declared.contains("key"),
            "{name}"
        );
        for value in values {
            let value = value.as_object().expect("the fields are an object");
            assert_eq!(
                value.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                declared,
                "{name}"
            );
            for field in fields {
                let (field, ty) = (
                    field["name"].trim_matches('"'),
                    field["type"].trim_matches('"'),
                );
                assert!(
                    shaped(&value[field], ty),
                    "{name}.{field} is not a {ty}: {}",
                    value[field]
                );
            }
        }
    }
    // The shapes the brief tables, spelled out for the card, whose fields are the most.
    let card = &handles[0].1;
    let types: Vec<(&str, &str)> = card
        .iter()
        .map(|f| (f["name"].trim_matches('"'), f["type"].trim_matches('"')))
        .collect();
    assert_eq!(
        types,
        [
            ("title", "str"),
            ("number", "int"),
            ("name", "str"),
            ("url", "str"),
            ("description", "str"),
            ("board", "str"),
            ("list", "str"),
            ("labels", "list[str]"),
            ("members", "list[str]"),
            ("due", "time?"),
            ("due_complete", "bool"),
            ("archived", "bool"),
            ("created_at", "time"),
            ("last_activity_at", "time"),
        ]
    );
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

/// Each `[[action]]` and the `[[action.param]]`s written under it, by name: a `[[action.param]]`
/// belongs to the `[[action]]` above it.
fn action_params(tables: &[Table]) -> BTreeMap<String, Vec<BTreeMap<String, String>>> {
    let mut params: BTreeMap<String, Vec<BTreeMap<String, String>>> = BTreeMap::new();
    let mut action = String::new();
    for (header, t) in tables {
        match header.as_str() {
            "action" => {
                action = t["name"].trim_matches('"').to_string();
                params.entry(action.clone()).or_default();
            }
            "action.param" => params.entry(action.clone()).or_default().push(t.clone()),
            _ => {}
        }
    }
    params
}

/// The `[[action]]`s are today's actions, [`ACTIONS`], then the reads [`READS`] answers, then
/// the writes [`WRITES`] does, each in its code's order, and nothing else. A plugin exposes no
/// values, so there is no `[[value]]`.
#[test]
fn the_actions_are_the_actions_then_the_reads() {
    let tables = tables(MANIFEST);
    let actions: Vec<String> = named(&tables, "action")
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(
        actions,
        ACTIONS
            .iter()
            .chain(READS)
            .chain(WRITES)
            .map(|a| quoted(a))
            .collect::<Vec<_>>()
    );
    assert!(
        named(&tables, "value").is_empty(),
        "a plugin exposes no values"
    );
}

/// The `[[action]]`s are the ones [`from_call`](crate::lifecycle::from_call) decodes, in
/// its order, each taking the card's handle first and then the parameters it reads; a member
/// action's `member` defaults to the token's own member.
#[test]
fn the_actions_are_the_ones_the_code_answers() {
    let tables = tables(MANIFEST);
    let mut params = action_params(&tables);
    params.retain(|action, _| ACTIONS.contains(&action.as_str()));
    let required = |name: &str| {
        vec![table(&[
            ("name", &quoted(name)),
            ("type", "\"str\""),
            ("required", "true"),
        ])]
    };
    let member = table(&[
        ("name", "\"member\""),
        ("type", "\"str\""),
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
        (
            "attach".to_string(),
            [required("path"), required("name")].concat(),
        ),
        (
            "fetch_attachment".to_string(),
            [required("name"), required("to")].concat(),
        ),
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
    assert_eq!(ME, "me", "the member default is Trello's own alias");
}

/// Each read takes a `trello.Account` first and returns what it reads: `cards` the board by
/// address or short link and then every filter named-only with none defaulted, bar whether
/// archived cards count and the limit, which are named-only with their defaults; the rest the
/// parameters they read, a handle where the read acts on one.
#[test]
fn the_reads_take_an_account_and_return_what_they_read() {
    let tables = tables(MANIFEST);
    let returns: BTreeMap<String, String> = named(&tables, "action")
        .into_iter()
        .filter(|t| READS.contains(&t["name"].trim_matches('"')))
        .map(|t| {
            (
                t["name"].trim_matches('"').to_string(),
                t.get("returns").cloned().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        returns,
        BTreeMap::from(
            [
                ("cards", "\"list[Card]\""),
                ("comments", "\"list[Comment]\""),
                ("checklists", "\"list[Checklist]\""),
                ("check_items", "\"list[Check_Item]\""),
                ("attachments", "\"list[Attachment]\""),
                ("download_attachment", ""),
                ("me", "\"Member\""),
                ("boards", "\"list[Board]\""),
                ("lists", "\"list[List]\""),
                ("labels", "\"list[Label]\""),
                ("members", "\"list[Member]\""),
            ]
            .map(|(name, ty)| (name.to_string(), ty.to_string()))
        )
    );

    let param = |name: &str, ty: &str, extra: &[(&str, &str)]| {
        let mut t = table(extra);
        t.insert("name".into(), quoted(name));
        t.insert("type".into(), quoted(ty));
        t
    };
    let required = |name: &str, ty: &str| param(name, ty, &[("required", "true")]);
    let named_only = |name: &str, ty: &str| param(name, ty, &[("named_only", "true")]);
    let acct = required("acct", "Account");
    let card = required("card", "Card");
    let board = required("board", "str");
    let cards = vec![
        acct.clone(),
        board.clone(),
        named_only("list", "list[str]"),
        named_only("labels", "list[str]"),
        named_only("without_labels", "list[str]"),
        named_only("members", "list[str]"),
        param(
            "include_archived",
            "bool",
            &[("named_only", "true"), ("default", "false")],
        ),
        param(
            "limit",
            "int",
            &[("named_only", "true"), ("default", "100")],
        ),
    ];
    let want = BTreeMap::from([
        ("cards".to_string(), cards),
        (
            "comments".to_string(),
            vec![
                acct.clone(),
                card.clone(),
                param(
                    "limit",
                    "int",
                    &[("named_only", "true"), ("default", "200")],
                ),
            ],
        ),
        ("checklists".to_string(), vec![acct.clone(), card.clone()]),
        ("check_items".to_string(), vec![acct.clone(), card.clone()]),
        ("attachments".to_string(), vec![acct.clone(), card]),
        (
            "download_attachment".to_string(),
            vec![
                acct.clone(),
                required("attachment", "Attachment"),
                required("to", "str"),
            ],
        ),
        ("me".to_string(), vec![acct.clone()]),
        ("boards".to_string(), vec![acct.clone()]),
        ("lists".to_string(), vec![acct.clone(), board.clone()]),
        ("labels".to_string(), vec![acct.clone(), board.clone()]),
        ("members".to_string(), vec![acct, board]),
    ]);
    let mut params = action_params(&tables);
    params.retain(|action, _| READS.contains(&action.as_str()));
    assert_eq!(params, want);
}

/// Each write takes a `trello.Account` first and returns what it makes or changes — a card, a
/// comment, a checklist, a check item or an attachment — and each delete nothing; and the
/// parameters are the code's: the handle each takes typed by its kind, the required ones first,
/// then the named-only ones, `at` an enum of top and bottom that defaults to the bottom where
/// the write makes something and has no default where it changes a card.
#[test]
fn the_writes_take_an_account_and_return_what_they_write() {
    let tables = tables(MANIFEST);
    let returns: BTreeMap<String, String> = named(&tables, "action")
        .into_iter()
        .filter(|t| WRITES.contains(&t["name"].trim_matches('"')))
        .map(|t| {
            (
                t["name"].trim_matches('"').to_string(),
                t.get("returns").cloned().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        returns,
        BTreeMap::from(
            [
                ("create_card", "\"Card\""),
                ("update_card", "\"Card\""),
                ("archive_card", "\"Card\""),
                ("unarchive_card", "\"Card\""),
                ("delete_card", ""),
                ("create_comment", "\"Comment\""),
                ("update_comment", "\"Comment\""),
                ("delete_comment", ""),
                ("create_checklist", "\"Checklist\""),
                ("delete_checklist", ""),
                ("create_check_item", "\"Check_Item\""),
                ("update_check_item", "\"Check_Item\""),
                ("delete_check_item", ""),
                ("create_attachment", "\"Attachment\""),
                ("delete_attachment", ""),
            ]
            .map(|(name, ty)| (name.to_string(), ty.to_string()))
        )
    );

    let param = |name: &str, ty: &str, extra: &[(&str, &str)]| {
        let mut t = table(extra);
        t.insert("name".into(), quoted(name));
        t.insert("type".into(), quoted(ty));
        t
    };
    let required = |name: &str, ty: &str| param(name, ty, &[("required", "true")]);
    let named_only = |name: &str, ty: &str| param(name, ty, &[("named_only", "true")]);
    let at = |default: Option<&str>| {
        let mut extra = vec![("named_only", "true"), ("values", "[\"top\", \"bottom\"]")];
        let default = default.map(quoted);
        if let Some(default) = &default {
            extra.push(("default", default.as_str()));
        }
        param("at", "enum", &extra)
    };
    let acct = required("acct", "Account");
    let card = required("card", "Card");
    let comment = required("comment", "Comment");
    let checklist = required("checklist", "Checklist");
    let item = required("item", "Check_Item");
    let on_card = |extra: &[BTreeMap<String, String>]| {
        let mut params = vec![acct.clone(), card.clone()];
        params.extend(extra.iter().cloned());
        params
    };
    let want = BTreeMap::from([
        (
            "create_card".to_string(),
            vec![
                acct.clone(),
                required("board", "str"),
                required("list", "str"),
                required("name", "str"),
                named_only("description", "str"),
                at(Some("bottom")),
                named_only("labels", "list[str]"),
                named_only("members", "list[str]"),
                named_only("due", "time?"),
            ],
        ),
        (
            "update_card".to_string(),
            on_card(&[
                named_only("name", "str"),
                named_only("description", "str"),
                named_only("list", "str"),
                at(None),
                named_only("due", "time?"),
                named_only("due_complete", "bool"),
            ]),
        ),
        ("archive_card".to_string(), on_card(&[])),
        ("unarchive_card".to_string(), on_card(&[])),
        ("delete_card".to_string(), on_card(&[])),
        (
            "create_comment".to_string(),
            on_card(&[required("body", "str")]),
        ),
        (
            "update_comment".to_string(),
            vec![acct.clone(), comment.clone(), required("body", "str")],
        ),
        ("delete_comment".to_string(), vec![acct.clone(), comment]),
        (
            "create_checklist".to_string(),
            on_card(&[required("name", "str"), named_only("items", "list[str]")]),
        ),
        (
            "delete_checklist".to_string(),
            vec![acct.clone(), checklist.clone()],
        ),
        (
            "create_check_item".to_string(),
            vec![
                acct.clone(),
                checklist,
                required("name", "str"),
                at(Some("bottom")),
            ],
        ),
        (
            "update_check_item".to_string(),
            vec![
                acct.clone(),
                item.clone(),
                named_only("name", "str"),
                named_only("done", "bool"),
            ],
        ),
        ("delete_check_item".to_string(), vec![acct.clone(), item]),
        (
            "create_attachment".to_string(),
            on_card(&[required("path", "str"), named_only("name", "str")]),
        ),
        (
            "delete_attachment".to_string(),
            vec![acct, required("attachment", "Attachment")],
        ),
    ]);
    let mut params = action_params(&tables);
    params.retain(|action, _| WRITES.contains(&action.as_str()));
    assert_eq!(params, want);
}

/// A plugin's kinds, actions, withables and handle types share one namespace
/// (reference §13.3), and afkd's install refuses a collision; so no name is declared twice
/// across them, the struct's included.
#[test]
fn no_kind_action_or_handle_shares_a_name() {
    let tables = tables(MANIFEST);
    let mut names: Vec<&str> = ["struct", "handle", "kind", "action"]
        .iter()
        .flat_map(|header| named(&tables, header))
        .map(|t| t["name"].as_str())
        .collect();
    let all = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all, "{names:?}");
}
