//! The plugin wire, as this plugin speaks it: the requests afkd sends, the replies written
//! back, and the one budget every reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"): one
//! newline-delimited JSON request at a time on stdin, one reply line each on stdout.
//! Nothing here is `deny_unknown_fields`, because the wire is additive within a `proto`:
//! a newer afkd's extra field must not break this plugin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::message::Handle;

/// The plugin protocol version this plugin speaks.
pub(crate) const PROTO: u32 = 2;

/// The longest reply line afkd reads, **before** its newline: afkd caps a line at 64 KiB
/// including the `\n`, and a line of exactly 64 KiB not ending in one is over the cap.
pub(crate) const MAX_REPLY: usize = 64 * 1024 - 1;

/// One request, internally tagged on `call`.
#[derive(Debug, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Request {
    /// Sent once, when the service arms: the protocol, the kind and the settings block. Who
    /// is asking (`service`, `roster`, `owner`) rides along and is not read: nothing this
    /// plugin does is shared between services.
    Hello {
        proto: u32,
        kind: String,
        settings: serde_json::Value,
    },
    /// Sent once a beat.
    Poll,
    /// Sent after a unit's run ends. A message has nothing to settle, so nothing of it is
    /// read.
    Finish,
    /// One action a config calls, by its own name, and the arguments afkd bound to its
    /// parameters, by name. A child started for this one call gets no `hello`: the request
    /// carries the `proto` a `hello` would have, and the plugin's own settings file, typed
    /// by the manifest's `[config]`.
    Call {
        action: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
        #[serde(default)]
        proto: Option<u32>,
        #[serde(default)]
        settings: Option<serde_json::Value>,
    },
    /// A call from a later afkd.
    #[serde(other)]
    Unknown,
}

/// One work item handed over in a `poll` reply: the message's handle, and what its run
/// gets.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireUnit {
    #[serde(flatten)]
    pub(crate) handle: Handle,
    pub(crate) thread: String,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<WireFile>,
}

/// One file of the attempt's scratch layout, as data: afkd writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireFile {
    pub(crate) path: String,
    pub(crate) text: String,
}

/// The `poll` reply that hands `unit` over, serialized, or `None` when it would not fit
/// afkd's line. A message's text is at most 4096 UTF-16 units and a caption 1024, so a unit
/// carrying one twice, in its `text` field and in its brief, stays far under the line; this
/// is the check that says so rather than writing a line afkd refuses.
pub(crate) fn fire_line(unit: &WireUnit) -> Option<String> {
    let line = serde_json::json!({ "fire": true, "unit": unit }).to_string();
    (line.len() <= MAX_REPLY).then_some(line)
}

/// The reply to a returning action: `handle`, the message it made.
pub(crate) fn handle_line(handle: &Handle) -> String {
    serde_json::json!({ "ok": true, "handle": handle }).to_string()
}

/// The reply refusing a call with `error`.
pub(crate) fn refusal_line(error: &str) -> String {
    serde_json::json!({ "ok": false, "error": error }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageFields;

    /// A unit as the trigger hands one over, its text `text` in both the field and the
    /// brief.
    fn unit(text: &str) -> WireUnit {
        WireUnit {
            handle: Handle {
                id: "4711".into(),
                key: "telegram:-1001234567890/4711".into(),
                fields: MessageFields {
                    chat: "-1001234567890".into(),
                    sender: "111111111".into(),
                    sender_name: "Anna Lindqvist".into(),
                    text: text.into(),
                    reply_to: String::new(),
                    files: Vec::new(),
                },
            },
            thread: "telegram:-1001234567890".into(),
            env: BTreeMap::from([("TELEGRAM_CHAT_ID".into(), "-1001234567890".into())]),
            files: vec![WireFile {
                path: crate::TASK_FILE.into(),
                text: format!("Message 4711\n\n{text}\n"),
            }],
        }
    }

    /// afkd's own request shapes decode, an extra field from a later afkd ignored: the
    /// `hello` with who is asking beside the settings, a `finish` with its facts, a `call`
    /// on the armed child, and a child's one `call` with its `proto` and settings.
    #[test]
    fn afkd_requests_decode() {
        let hello: Request = serde_json::from_str(
            r#"{"call":"hello","proto":2,"kind":"message","service":"ops","roster":["ops"],
                "owner":"afkd-4242","settings":{"token":"t","chats":["-100"],"every":"5s"}}"#,
        )
        .unwrap();
        assert!(matches!(
            &hello,
            Request::Hello { proto: 2, kind, settings } if kind == "message" && settings["chats"][0] == "-100"
        ));
        let finish: Request = serde_json::from_str(
            r#"{"call":"finish","id":"4711","key":"telegram:-100/4711","outcome":"clean",
                "facts":{"signal":"proceed","reason":null,"duration_ms":3,"cost":0.0,
                         "turns":null,"tokens":null,"run_name":"261109-100400-unit-4711-1"}}"#,
        )
        .unwrap();
        assert!(matches!(finish, Request::Finish));

        let armed: Request = serde_json::from_str(
            r#"{"call":"call","action":"reply",
                "args":{"message":{"id":"4711","key":"telegram:-100/4711"},"text":"klart ✓"},
                "later":1}"#,
        )
        .unwrap();
        let Request::Call {
            action,
            args,
            proto,
            settings,
        } = armed
        else {
            panic!("not a call: {armed:?}");
        };
        assert_eq!(action, "reply");
        assert_eq!(args["text"], "klart ✓");
        assert_eq!((proto, settings), (None, None));

        let child: Request = serde_json::from_str(
            r#"{"call":"call","proto":2,"action":"send","args":{"chat":"-100","text":"ビルド"},
                "settings":{"token":"t"}}"#,
        )
        .unwrap();
        assert!(matches!(
            child,
            Request::Call { proto: Some(2), settings: Some(s), .. } if s["token"] == "t"
        ));
        let later: Request = serde_json::from_str(r#"{"call":"renew","key":"k"}"#).unwrap();
        assert!(matches!(later, Request::Unknown));
    }

    /// The unit's handle members sit at the top of `unit`, beside what its run gets, as
    /// afkd reads a unit.
    #[test]
    fn a_unit_line_carries_the_handle_flat() {
        let line = fire_line(&unit("deploy 🚀")).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["fire"], true);
        let unit = &reply["unit"];
        assert_eq!(unit["id"], "4711");
        assert_eq!(unit["key"], "telegram:-1001234567890/4711");
        assert_eq!(unit["fields"]["text"], "deploy 🚀");
        assert_eq!(unit["thread"], "telegram:-1001234567890");
        assert_eq!(unit["files"][0]["path"], "task.md");
        assert!(unit.get("handle").is_none(), "{unit}");
    }

    /// The largest text Telegram delivers, 4096 UTF-16 units, made of the characters that
    /// cost the most on the line — three UTF-8 bytes each, or a control character JSON
    /// escapes to six — fits afkd's line carried twice.
    #[test]
    fn the_largest_text_fits_the_line_twice() {
        for text in ["監".repeat(4096), "\u{1}".repeat(4096)] {
            let line = fire_line(&unit(&text)).expect("fits");
            assert!(line.len() <= MAX_REPLY, "{}", line.len());
        }
        let over = "\u{1}".repeat(6_000);
        assert_eq!(
            fire_line(&unit(&over)),
            None,
            "a line past the cap is not written"
        );
    }
}
