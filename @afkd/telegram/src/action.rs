//! The **action vocabulary** a config calls: the decode of one `call` request into what it
//! asks for, and the doing of it on the Bot API.
//!
//! Each call — `telegram.send(chat, "klart")`, `telegram.edit(msg, "klart nu")` — crosses as
//! one `call`, its arguments already bound and typed against the manifest. A message is the
//! handle a `service(telegram)` was passed or a send gave back, the same type either way,
//! and only its `key` names the chat and the message ([`parse_key`]).
//!
//! **Nothing is queued.** `send`, `send_file` and `reply` give back the message Telegram
//! made, which a queued write could not, so every call is done now or refused, and an
//! unreachable Telegram is a refusal. A 429 is waited out while its wait fits what is left
//! of the call's budget ([`patiently`]), else refused with the wait Telegram asked for.

use std::path::Path;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::botapi::{ApiError, BotApi, FileKind, Upload, FETCH_CAP};
use crate::common::Clock;
use crate::message::{handle, parse_key, split_text, utf16_len, Handle, CAPTION_CAP, TEXT_CAP};

/// The parameter every action on a message takes first: the message, as a
/// `telegram.Message` handle.
pub(crate) const HANDLE: &str = "message";

/// The actions afkd may `call`, in the order the manifest declares them: what
/// `manifest.rs`'s test holds `afkd-plugin.toml` to, and [`from_call`] decodes.
pub(crate) const ACTIONS: &[&str] = &[
    "send",
    "send_file",
    "reply",
    "react",
    "edit",
    "delete",
    "fetch_file",
];

/// The actions that give back the message they made: their manifest `returns`.
pub(crate) const RETURNING: &[&str] = &["send", "send_file", "reply"];

/// Telegram's description when `editMessageText` is asked to edit a message that carries a
/// file, whose text is its caption.
const NO_TEXT_TO_EDIT: &str = "Bad Request: there is no text in the message to edit";

/// A message a call acts on: its chat and its id, off the handle's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) chat: String,
    pub(crate) id: i64,
}

/// What one decoded `call` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    /// A text to a chat, split at Telegram's cap.
    Send { chat: String, text: String },
    /// The file at an absolute path to a chat, as `kind`, with a caption unless empty.
    SendFile {
        chat: String,
        path: String,
        kind: FileKind,
        caption: String,
    },
    /// A text to the message's chat, the first part replying to it.
    Reply { to: Target, text: String },
    /// The bot's reaction to the message set to `emoji`, or taken off with `None`.
    React { to: Target, emoji: Option<String> },
    /// The message's text, or its caption, replaced.
    Edit { to: Target, text: String },
    /// The message deleted.
    Delete { to: Target },
    /// The file of a file id written to an absolute path.
    FetchFile { file: String, to: String },
}

/// Decode one `call` — the action's name and its arguments by parameter name — into what it
/// asks for, or the sentence that refuses it.
///
/// afkd binds every argument against the manifest before it sends the call, so a missing or
/// mistyped argument only comes from a hand-written wire; it is refused rather than guessed
/// at. An argument the action has no parameter for is ignored, since the wire is additive.
pub(crate) fn from_call(action: &str, args: &Map<String, Value>) -> Result<Call, String> {
    let string = |param: &str| string(action, args, param);
    let target = || target(action, args);
    Ok(match action {
        "send" => Call::Send {
            chat: string("chat")?,
            text: string("text")?,
        },
        "send_file" => Call::SendFile {
            chat: string("chat")?,
            path: string("path")?,
            kind: match args.get("kind") {
                None => FileKind::Document,
                Some(_) => {
                    let word = string("kind")?;
                    FileKind::from_word(&word).ok_or_else(|| {
                        format!(
                            "{action}: parameter kind is `{word}`, not one of {}",
                            FileKind::ALL.map(FileKind::word).join(", ")
                        )
                    })?
                }
            },
            caption: match args.get("caption") {
                None => String::new(),
                Some(_) => string("caption")?,
            },
        },
        "reply" => Call::Reply {
            to: target()?,
            text: string("text")?,
        },
        "react" => Call::React {
            to: target()?,
            emoji: Some(string("emoji")?).filter(|e| !e.is_empty()),
        },
        "edit" => Call::Edit {
            to: target()?,
            text: string("text")?,
        },
        "delete" => Call::Delete { to: target()? },
        "fetch_file" => Call::FetchFile {
            file: string("file")?,
            to: string("to")?,
        },
        _ => {
            return Err(format!(
                "no action `{action}`; @afkd/telegram has {}",
                ACTIONS.join(", ")
            ))
        }
    })
}

/// The message `action`'s handle names, off its key.
fn target(action: &str, args: &Map<String, Value>) -> Result<Target, String> {
    let key = match args.get(HANDLE) {
        Some(Value::Object(handle)) => handle.get("key").and_then(Value::as_str),
        Some(_) => None,
        None => return Err(format!("{action}: parameter {HANDLE} is required")),
    };
    let key =
        key.ok_or_else(|| format!("{action}: parameter {HANDLE} must be a message handle"))?;
    let (chat, id) = parse_key(key).map_err(|e| format!("{action}: {e}"))?;
    Ok(Target { chat, id })
}

/// The string argument `param` of `action`, or the sentence refusing its absence or type.
fn string(action: &str, args: &Map<String, Value>, param: &str) -> Result<String, String> {
    match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    }
}

/// Run `request` until it is not a 429 whose wait fits before `deadline`, waiting out each
/// one that does on `clock`. A 429 that does not fit comes back as it is, to be refused with
/// the wait Telegram asked for.
fn patiently<T>(
    clock: &dyn Clock,
    deadline: Instant,
    mut request: impl FnMut() -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    loop {
        match request() {
            Err(ApiError::Refused {
                code: 429,
                retry_after: Some(wait),
                ..
            }) if clock.now() + wait < deadline => clock.sleep(wait),
            other => return other,
        }
    }
}

/// Do `call` on `api` before `deadline`: the message it made for a returning action, `None`
/// for any other, or the sentence refusing it, which names the action.
pub(crate) fn perform(
    call: &Call,
    api: &dyn BotApi,
    clock: &dyn Clock,
    deadline: Instant,
) -> Result<Option<Handle>, String> {
    match call {
        Call::Send { chat, text } => send_text(api, clock, deadline, "send", chat, text, None),
        Call::SendFile {
            chat,
            path,
            kind,
            caption,
        } => {
            let upload = read_upload(path, *kind, caption)?;
            patiently(clock, deadline, || {
                api.send_file(chat, *kind, &upload, caption)
            })
            .map(|m| Some(handle(&m)))
            .map_err(|e| format!("send_file: {e}"))
        }
        Call::Reply { to, text } => {
            send_text(api, clock, deadline, "reply", &to.chat, text, Some(to.id))
        }
        Call::React { to, emoji } => patiently(clock, deadline, || {
            api.set_reaction(&to.chat, to.id, emoji.as_deref())
        })
        .map(|()| None)
        .map_err(|e| format!("react: {e}")),
        Call::Edit { to, text } => edit(api, clock, deadline, to, text).map(|()| None),
        Call::Delete { to } => patiently(clock, deadline, || api.delete(&to.chat, to.id))
            .map(|()| None)
            .map_err(|e| format!("delete: {e}")),
        Call::FetchFile { file, to } => fetch_file(api, clock, deadline, file, to).map(|()| None),
    }
}

/// Send `text` to `chat` as `action` does, split at Telegram's cap, the first part replying
/// to `reply_to` when given; the last message sent, the newest, is the one given back. A part
/// refused after others went out says how many did.
fn send_text(
    api: &dyn BotApi,
    clock: &dyn Clock,
    deadline: Instant,
    action: &str,
    chat: &str,
    text: &str,
    reply_to: Option<i64>,
) -> Result<Option<Handle>, String> {
    let parts = split_text(text, TEXT_CAP).map_err(|e| format!("{action}: {e}"))?;
    let mut last = None;
    for (n, part) in parts.iter().enumerate() {
        let reply_to = reply_to.filter(|_| n == 0);
        let sent =
            patiently(clock, deadline, || api.send_message(chat, part, reply_to)).map_err(|e| {
                match n {
                    0 => format!("{action}: {e}"),
                    n => format!("{action}: sent {n} of {} parts, then: {e}", parts.len()),
                }
            })?;
        last = Some(sent);
    }
    Ok(last.as_ref().map(handle))
}

/// The file at `path`, read to be sent as `kind` under `caption`, or the sentence refusing
/// it before anything is sent: a relative path, a file that is not there, a caption or a
/// file over Telegram's cap.
fn read_upload(path: &str, kind: FileKind, caption: &str) -> Result<Upload, String> {
    let refuse = |why: String| format!("send_file: {why}");
    if !Path::new(path).is_absolute() {
        return Err(refuse(format!("path `{path}` is not absolute")));
    }
    let units = utf16_len(caption);
    if units > CAPTION_CAP {
        return Err(refuse(format!(
            "the caption is {units} UTF-16 units, over the {CAPTION_CAP} Telegram takes; a \
             caption is not cut"
        )));
    }
    let meta = std::fs::metadata(path).map_err(|e| refuse(format!("cannot read {path}: {e}")))?;
    if !meta.is_file() {
        return Err(refuse(format!("{path} is not a file")));
    }
    if meta.len() > kind.cap() {
        return Err(refuse(format!(
            "{path} is {} bytes, over the {} bytes Telegram takes in a {}",
            meta.len(),
            kind.cap(),
            kind.word()
        )));
    }
    let bytes = std::fs::read(path).map_err(|e| refuse(format!("cannot read {path}: {e}")))?;
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Upload { name, bytes })
}

/// Replace the text of the message `to` with `text`, or its caption when it carries a file:
/// `editMessageText` first, and `editMessageCaption` when Telegram says the message has no
/// text, since a handle does not say which it has. Never split.
fn edit(
    api: &dyn BotApi,
    clock: &dyn Clock,
    deadline: Instant,
    to: &Target,
    text: &str,
) -> Result<(), String> {
    let refuse = |why: String| format!("edit: {why}");
    if text.trim().is_empty() {
        return Err(refuse(
            "the text is empty, and Telegram sends no empty message".into(),
        ));
    }
    let units = utf16_len(text);
    if units > TEXT_CAP {
        return Err(refuse(format!(
            "the text is {units} UTF-16 units, over the {TEXT_CAP} Telegram takes; an edit is \
             not split"
        )));
    }
    match patiently(clock, deadline, || api.edit_text(&to.chat, to.id, text)) {
        Err(e) if e.says(NO_TEXT_TO_EDIT) => {
            if units > CAPTION_CAP {
                return Err(refuse(format!(
                    "the message carries a file, and the text is {units} UTF-16 units, over the \
                     {CAPTION_CAP} a caption holds"
                )));
            }
            patiently(clock, deadline, || api.edit_caption(&to.chat, to.id, text))
                .map_err(|e| refuse(e.to_string()))
        }
        other => other.map_err(|e| refuse(e.to_string())),
    }
}

/// Write the file of file id `file` to the absolute path `to`, whose directory must exist.
fn fetch_file(
    api: &dyn BotApi,
    clock: &dyn Clock,
    deadline: Instant,
    file: &str,
    to: &str,
) -> Result<(), String> {
    let refuse = |why: String| format!("fetch_file: {why}");
    let dest = Path::new(to);
    if !dest.is_absolute() {
        return Err(refuse(format!("`to` is `{to}`, not an absolute path")));
    }
    if !dest.parent().is_some_and(Path::is_dir) {
        return Err(refuse(format!("the directory of {to} does not exist")));
    }
    let info =
        patiently(clock, deadline, || api.get_file(file)).map_err(|e| refuse(e.to_string()))?;
    let path = info
        .file_path
        .ok_or_else(|| refuse("Telegram gave no path to fetch the file from".into()))?;
    let bytes = patiently(clock, deadline, || api.download(&path, FETCH_CAP))
        .map_err(|e| refuse(e.to_string()))?;
    std::fs::write(dest, bytes).map_err(|e| refuse(format!("cannot write {to}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn every_action_decodes_with_its_arguments() {
        let msg = json!({"id": "4711", "key": "telegram:-1001234567890/4711"});
        let to = Target {
            chat: "-1001234567890".into(),
            id: 4711,
        };
        assert_eq!(
            from_call("send", &args(json!({"chat": "@ops", "text": "klart"}))),
            Ok(Call::Send {
                chat: "@ops".into(),
                text: "klart".into()
            })
        );
        assert_eq!(
            from_call(
                "send_file",
                &args(json!({"chat": "42", "path": "/tmp/a.png", "kind": "photo",
                             "caption": "ビルド"}))
            ),
            Ok(Call::SendFile {
                chat: "42".into(),
                path: "/tmp/a.png".into(),
                kind: FileKind::Photo,
                caption: "ビルド".into()
            })
        );
        assert_eq!(
            from_call("send_file", &args(json!({"chat": "42", "path": "/tmp/a"}))),
            Ok(Call::SendFile {
                chat: "42".into(),
                path: "/tmp/a".into(),
                kind: FileKind::Document,
                caption: String::new()
            }),
            "the manifest's defaults, for a hand-written wire that leaves them out"
        );
        assert_eq!(
            from_call("reply", &args(json!({"message": msg, "text": "ok"}))),
            Ok(Call::Reply {
                to: to.clone(),
                text: "ok".into()
            })
        );
        assert_eq!(
            from_call("react", &args(json!({"message": msg, "emoji": ""}))),
            Ok(Call::React {
                to: to.clone(),
                emoji: None
            }),
            "an empty emoji takes the reaction off"
        );
        assert_eq!(
            from_call("delete", &args(json!({"message": msg}))),
            Ok(Call::Delete { to })
        );
        assert_eq!(
            from_call("fetch_file", &args(json!({"file": "BQAC", "to": "/tmp/x"}))),
            Ok(Call::FetchFile {
                file: "BQAC".into(),
                to: "/tmp/x".into()
            })
        );
    }

    #[test]
    fn a_malformed_call_is_refused_naming_what_is_wrong() {
        for (action, a, needle) in [
            (
                "forward",
                json!({}),
                "no action `forward`; @afkd/telegram has send, send_file",
            ),
            (
                "send",
                json!({"chat": "42"}),
                "send: parameter text is required",
            ),
            (
                "send",
                json!({"chat": 42, "text": "x"}),
                "send: parameter chat must be a string",
            ),
            (
                "send_file",
                json!({"chat": "1", "path": "/a", "kind": "gif"}),
                "not one of document",
            ),
            (
                "edit",
                json!({"text": "x"}),
                "edit: parameter message is required",
            ),
            (
                "edit",
                json!({"message": "4711", "text": "x"}),
                "must be a message handle",
            ),
            (
                "delete",
                json!({"message": {"id": "1", "key": "ENG-1#c"}}),
                "delete: `ENG-1#c` is not a Telegram message's key",
            ),
        ] {
            let e = from_call(action, &args(a)).unwrap_err();
            assert!(e.contains(needle), "{action}: {e}");
        }
    }

    #[test]
    fn the_returning_actions_are_among_the_actions() {
        assert!(RETURNING.iter().all(|a| ACTIONS.contains(a)));
    }
}
