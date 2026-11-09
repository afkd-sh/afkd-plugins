//! A Telegram message as afkd sees it: the one mapping from a Bot API `Message` to a
//! `telegram.Message` handle, used for a message the trigger hands over and for the one a
//! send gave back alike, so the two are one type in every call. Beside it, the key a handle
//! crosses back with, the brief a run reads, and the cut of a long text into messages.

use std::fmt::Write as _;

use serde::Serialize;

use crate::botapi::{TgFile, TgMessage};

/// The most UTF-16 code units Telegram takes in one message's text.
pub(crate) const TEXT_CAP: usize = 4096;

/// The most UTF-16 code units Telegram takes in a caption.
pub(crate) const CAPTION_CAP: usize = 1024;

/// The prefix every key carries.
const KEY_PREFIX: &str = "telegram:";

/// A `telegram.Message` handle: Telegram's message id, the key afkd claims it under and
/// every call names it by, and its fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Handle {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) fields: MessageFields,
}

/// The fields of a `telegram.Message`, in the manifest's order. Each is always sent, `""`
/// or an empty list for what a message does not carry, so afkd never finds one missing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct MessageFields {
    /// The chat's id.
    pub(crate) chat: String,
    /// The sender's user id, `""` for a message sent on behalf of a chat.
    pub(crate) sender: String,
    /// The sender's name, or `@username` when they have none.
    pub(crate) sender_name: String,
    /// The text, or a file's caption.
    pub(crate) text: String,
    /// The id of the message it replies to, `""` for none.
    pub(crate) reply_to: String,
    /// The file ids of the files it carries.
    pub(crate) files: Vec<String>,
}

/// The handle of `message`.
pub(crate) fn handle(message: &TgMessage) -> Handle {
    let chat = message.chat.id.to_string();
    let id = message.message_id.to_string();
    Handle {
        key: key(&chat, message.message_id),
        id,
        fields: MessageFields {
            sender: message
                .from
                .as_ref()
                .map(|u| u.id.to_string())
                .unwrap_or_default(),
            sender_name: sender_name(message),
            text: text_of(message).to_string(),
            reply_to: message
                .reply_to_message
                .as_ref()
                .map(|r| r.message_id.to_string())
                .unwrap_or_default(),
            files: files_of(message).into_iter().map(|f| f.id).collect(),
            chat,
        },
    }
}

/// The key of message `id` in `chat`: `telegram:<chat>/<id>`.
pub(crate) fn key(chat: &str, id: i64) -> String {
    format!("{KEY_PREFIX}{chat}/{id}")
}

/// The thread a chat's runs share, so a session agent carries a chat's conversation on.
pub(crate) fn thread(chat: &str) -> String {
    format!("{KEY_PREFIX}{chat}")
}

/// The chat and the message id a handle's `key` names, or the sentence refusing a key this
/// plugin never makes. The key is all that crosses back of a handle beside its id, and the
/// chat is in it alone.
pub(crate) fn parse_key(key: &str) -> Result<(String, i64), String> {
    let refuse = || format!("`{key}` is not a Telegram message's key (telegram:<chat>/<id>)");
    let rest = key.strip_prefix(KEY_PREFIX).ok_or_else(refuse)?;
    let (chat, id) = rest.rsplit_once('/').ok_or_else(refuse)?;
    let id: i64 = id.parse().map_err(|_| refuse())?;
    if chat.is_empty() || id <= 0 {
        return Err(refuse());
    }
    Ok((chat.to_string(), id))
}

/// The text a message carries: its text, else its caption, else `""`.
fn text_of(message: &TgMessage) -> &str {
    message
        .text
        .as_deref()
        .or(message.caption.as_deref())
        .unwrap_or("")
}

/// Who sent `message`, as a person reads it: their first and last name, else `@username`,
/// else the chat's title for a message sent on behalf of a chat.
fn sender_name(message: &TgMessage) -> String {
    match &message.from {
        Some(user) => {
            let name = match &user.last_name {
                Some(last) if !last.is_empty() => format!("{} {last}", user.first_name),
                _ => user.first_name.clone(),
            };
            match (&user.username, name.trim().is_empty()) {
                (Some(username), true) => format!("@{username}"),
                _ => name.trim().to_string(),
            }
        }
        None => message.chat.title.clone().unwrap_or_default(),
    }
}

/// One file a message carries, as the brief lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRef {
    /// `document`, `photo`, `video`, `audio` or `voice`.
    pub(crate) kind: &'static str,
    /// The file id `fetch_file` takes.
    pub(crate) id: String,
    /// Its name, when Telegram has one.
    pub(crate) name: Option<String>,
    /// Its size in bytes, when Telegram says.
    pub(crate) size: Option<u64>,
    /// A photo's width and height.
    pub(crate) dims: Option<(u32, u32)>,
}

/// The files `message` carries: its document, the largest size of its photo, its video,
/// audio or voice.
pub(crate) fn files_of(message: &TgMessage) -> Vec<FileRef> {
    let of = |kind, f: &TgFile| FileRef {
        kind,
        id: f.file_id.clone(),
        name: f.file_name.clone(),
        size: f.file_size,
        dims: None,
    };
    let mut files = Vec::new();
    if let Some(d) = &message.document {
        files.push(of("document", d));
    }
    if let Some(largest) = message
        .photo
        .iter()
        .flatten()
        .max_by_key(|p| u64::from(p.width) * u64::from(p.height))
    {
        files.push(FileRef {
            kind: "photo",
            id: largest.file_id.clone(),
            name: None,
            size: largest.file_size,
            dims: Some((largest.width, largest.height)),
        });
    }
    for (kind, file) in [
        ("video", &message.video),
        ("audio", &message.audio),
        ("voice", &message.voice),
    ] {
        if let Some(f) = file {
            files.push(of(kind, f));
        }
    }
    files
}

/// Whether `message` carries anything a run can work: a text, a caption or a file. A
/// sticker or a service message carries none of them.
pub(crate) fn has_content(message: &TgMessage) -> bool {
    !text_of(message).is_empty() || !files_of(message).is_empty()
}

/// The brief a run reads in `task.md`: who sent the message, where and when, what it
/// replies to, its text, and a line per file with what `fetch_file` takes. afkd frames it
/// as data before writing it.
pub(crate) fn brief(message: &TgMessage) -> String {
    let h = handle(message);
    let mut out = format!("# Telegram message {} in chat {}", h.id, h.fields.chat);
    if let Some(title) = &message.chat.title {
        let _ = write!(out, " ({title})");
    }
    out.push_str("\n\n");
    let from = match h.fields.sender.as_str() {
        "" => h.fields.sender_name.clone(),
        id => format!("{} (user {id})", h.fields.sender_name),
    };
    let _ = writeln!(out, "From: {from}");
    let _ = writeln!(out, "Sent: {}", utc(message.date));
    if !h.fields.reply_to.is_empty() {
        let _ = writeln!(out, "Replying to: message {}", h.fields.reply_to);
    }
    out.push('\n');
    match h.fields.text.as_str() {
        "" => out.push_str("(no text)\n"),
        text => {
            out.push_str(text);
            out.push('\n');
        }
    }
    let files = files_of(message);
    if !files.is_empty() {
        out.push_str("\nFiles, each fetched by its file id:\n");
        for f in files {
            let _ = write!(out, "- {}", f.kind);
            if let Some(name) = &f.name {
                let _ = write!(out, " {name}");
            }
            if let Some((w, h)) = f.dims {
                let _ = write!(out, ", {w}x{h}");
            }
            if let Some(size) = f.size {
                let _ = write!(out, ", {size} bytes");
            }
            let _ = writeln!(out, ": {}", f.id);
        }
    }
    out
}

/// `secs` since the epoch as an RFC 3339 UTC time, `2026-11-09T09:58:00Z`.
fn utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's days-to-civil, over the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// How many UTF-16 code units `text` takes, the unit Telegram counts its caps in.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text` cut into the messages Telegram takes, each at most `cap` UTF-16 units: a part
/// ends after the last `\n` inside its window, or at the cap when the window holds none,
/// and never inside a character. Telegram trims a message's text and refuses one left
/// empty, so a part that is only whitespace — the newline after a text that fills the cap —
/// is not sent, and a text with nothing else is refused.
pub(crate) fn split_text(text: &str, cap: usize) -> Result<Vec<&str>, String> {
    let mut parts = Vec::new();
    let mut rest = text;
    while utf16_len(rest) > cap {
        // The byte offset where the window of `cap` units ends, and the last newline in it.
        let (mut units, mut end, mut newline) = (0, 0, None);
        for (at, c) in rest.char_indices() {
            if units + c.len_utf16() > cap {
                break;
            }
            units += c.len_utf16();
            end = at + c.len_utf8();
            if c == '\n' {
                newline = Some(end);
            }
        }
        let cut = newline.unwrap_or(end);
        parts.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    parts.push(rest);
    parts.retain(|p| !p.trim().is_empty());
    if parts.is_empty() {
        return Err("the text is empty, and Telegram sends no empty message".into());
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture message, read as the trigger reads one.
    fn message(json: &str) -> TgMessage {
        serde_json::from_str(json).unwrap()
    }

    /// A photo with a caption, replying to an earlier message, from a user with a full
    /// name, in a supergroup: as Telegram sends one.
    const PHOTO_REPLY: &str = r#"{"message_id":4711,
        "from":{"id":111111111,"is_bot":false,"first_name":"Anna","last_name":"Lindqvist",
                "username":"annal","language_code":"sv"},
        "chat":{"id":-1001234567890,"title":"Drift 🚀","type":"supergroup"},
        "date":1794218280,
        "reply_to_message":{"message_id":4710,"from":{"id":222222222,"is_bot":false,
                "first_name":"陳"},"chat":{"id":-1001234567890,"title":"Drift 🚀",
                "type":"supergroup"},"date":1794218200,"text":"what broke?"},
        "photo":[{"file_id":"AgACAgQAAxkBAAIBPWcS_small","file_unique_id":"AQADs","file_size":1290,
                  "width":90,"height":68},
                 {"file_id":"AgACAgQAAxkBAAIBPWcS_large","file_unique_id":"AQADl",
                  "file_size":102400,"width":1280,"height":960},
                 {"file_id":"AgACAgQAAxkBAAIBPWcS_mid","file_unique_id":"AQADm",
                  "file_size":21734,"width":320,"height":240}],
        "caption":"デプロイ failed here — see the log ⬇"}"#;

    #[test]
    fn a_photo_reply_maps_every_field() {
        let h = handle(&message(PHOTO_REPLY));
        assert_eq!(h.id, "4711");
        assert_eq!(h.key, "telegram:-1001234567890/4711");
        assert_eq!(
            h.fields,
            MessageFields {
                chat: "-1001234567890".into(),
                sender: "111111111".into(),
                sender_name: "Anna Lindqvist".into(),
                text: "デプロイ failed here — see the log ⬇".into(),
                reply_to: "4710".into(),
                files: vec!["AgACAgQAAxkBAAIBPWcS_large".into()],
            },
            "the caption is the text, and the largest size is the photo"
        );
    }

    /// Each file kind lands in `files`; a message from a chat has no sender but the chat's
    /// title, and a user with no name is their `@username`.
    #[test]
    fn every_file_kind_and_sender_maps() {
        let base = |extra: &str| {
            format!(
                r#"{{"message_id":9,"chat":{{"id":42,"type":"private","first_name":"Ola"}},
                    "date":1794218280,{extra}}}"#
            )
        };
        for kind in ["document", "video", "audio", "voice"] {
            let m = message(&base(&format!(
                r#""from":{{"id":42,"is_bot":false,"first_name":"Ola"}},
                   "{kind}":{{"file_id":"{kind}-id","file_unique_id":"u","file_size":10}}"#
            )));
            assert_eq!(handle(&m).fields.files, [format!("{kind}-id")], "{kind}");
            assert_eq!(files_of(&m)[0].kind, kind);
            assert!(has_content(&m), "{kind}");
        }
        let channel = message(
            r#"{"message_id":3,"sender_chat":{"id":-1009,"title":"Ops","type":"channel"},
                "chat":{"id":-1009,"title":"Ops","type":"supergroup"},"date":1,"text":"hej"}"#,
        );
        let h = handle(&channel);
        assert_eq!(
            (h.fields.sender.as_str(), h.fields.sender_name.as_str()),
            ("", "Ops")
        );
        assert_eq!(h.fields.reply_to, "");
        assert!(h.fields.files.is_empty());

        let nameless = message(&base(
            r#""from":{"id":7,"is_bot":false,"first_name":"","username":"ghost"},"text":"x""#,
        ));
        assert_eq!(handle(&nameless).fields.sender_name, "@ghost");

        let sticker = message(&base(
            r#""from":{"id":7,"is_bot":false,"first_name":"Ola"},
               "sticker":{"file_id":"s","file_unique_id":"u","type":"regular","width":512,
                          "height":512,"is_animated":false,"is_video":false}"#,
        ));
        assert!(
            !has_content(&sticker),
            "a sticker carries nothing a run works"
        );
    }

    #[test]
    fn a_key_round_trips_and_a_foreign_one_is_refused() {
        assert_eq!(
            parse_key("telegram:-1001234567890/4711"),
            Ok(("-1001234567890".to_string(), 4711))
        );
        assert_eq!(
            parse_key("telegram:@ops_channel/12"),
            Ok(("@ops_channel".into(), 12))
        );
        assert_eq!(parse_key(&key("42", 9)), Ok(("42".into(), 9)));
        for bad in [
            "-1001234567890/4711",
            "telegram:-1001234567890",
            "telegram:-1001234567890/x",
            "telegram:/4711",
            "telegram:-100/0",
            "linear:ENG-1/2",
        ] {
            let e = parse_key(bad).unwrap_err();
            assert!(e.contains(bad), "{e}");
        }
    }

    #[test]
    fn the_brief_names_sender_reply_text_and_files() {
        assert_eq!(
            brief(&message(PHOTO_REPLY)),
            "# Telegram message 4711 in chat -1001234567890 (Drift 🚀)\n\n\
             From: Anna Lindqvist (user 111111111)\n\
             Sent: 2026-11-09T09:58:00Z\n\
             Replying to: message 4710\n\n\
             デプロイ failed here — see the log ⬇\n\n\
             Files, each fetched by its file id:\n\
             - photo, 1280x960, 102400 bytes: AgACAgQAAxkBAAIBPWcS_large\n"
        );
        let doc = message(
            r#"{"message_id":5,"from":{"id":1,"is_bot":false,"first_name":"Ola"},
                "chat":{"id":1,"type":"private","first_name":"Ola"},"date":0,
                "document":{"file_name":"deploy.log","mime_type":"text/plain",
                            "file_id":"BQACAgQ","file_unique_id":"u","file_size":2048}}"#,
        );
        assert_eq!(
            brief(&doc),
            "# Telegram message 5 in chat 1\n\nFrom: Ola (user 1)\nSent: 1970-01-01T00:00:00Z\n\n\
             (no text)\n\nFiles, each fetched by its file id:\n\
             - document deploy.log, 2048 bytes: BQACAgQ\n"
        );
    }

    #[test]
    fn utc_reads_the_calendar() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_794_218_280), "2026-11-09T09:58:00Z");
        assert_eq!(utc(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    /// Glue the parts back: nothing is lost or doubled.
    fn rejoined(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn a_long_text_is_cut_after_its_last_newline_in_the_window() {
        let line = "x".repeat(99) + "\n";
        let text = line.repeat(50);
        let parts = split_text(&text, TEXT_CAP).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0].len(),
            4000,
            "40 whole lines, the 41st would pass the cap"
        );
        assert!(parts[0].ends_with('\n'));
        assert_eq!(rejoined(&parts), text);
    }

    #[test]
    fn a_text_without_a_newline_is_cut_at_the_cap() {
        let text = "a".repeat(9000);
        let parts = split_text(&text, TEXT_CAP).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [4096, 4096, 808]
        );
        assert_eq!(rejoined(&parts), text);
    }

    /// An emoji outside the BMP is two UTF-16 units, and a cut never falls between them; a
    /// CJK character is one.
    #[test]
    fn units_are_utf16_and_a_cut_never_splits_a_character() {
        let emoji = "😀".repeat(2049);
        assert_eq!(utf16_len(&emoji), 4098);
        let parts = split_text(&emoji, TEXT_CAP).unwrap();
        assert_eq!(
            parts.iter().map(|p| utf16_len(p)).collect::<Vec<_>>(),
            [4096, 2]
        );
        let odd = "a".to_string() + &"😀".repeat(2048);
        let parts = split_text(&odd, TEXT_CAP).unwrap();
        assert_eq!(
            utf16_len(parts[0]),
            4095,
            "the pair that would straddle the cap moves on"
        );
        assert_eq!(rejoined(&parts), odd);

        let cjk = "監".repeat(4097);
        assert_eq!(utf16_len(&cjk), 4097);
        let parts = split_text(&cjk, TEXT_CAP).unwrap();
        assert_eq!(
            parts.iter().map(|p| utf16_len(p)).collect::<Vec<_>>(),
            [4096, 1]
        );
    }

    #[test]
    fn a_text_at_the_cap_is_one_part_and_an_empty_one_is_refused() {
        let exact = "😀".repeat(2048);
        assert_eq!(split_text(&exact, TEXT_CAP).unwrap(), [exact.as_str()]);
        assert_eq!(split_text("klart", TEXT_CAP).unwrap(), ["klart"]);
        for blank in ["", " \n\t"] {
            assert!(
                split_text(blank, TEXT_CAP).unwrap_err().contains("empty"),
                "{blank:?}"
            );
        }
    }

    /// A text that fills the cap and ends in a newline leaves a part of that newline alone,
    /// which Telegram would refuse as empty: it is not sent.
    #[test]
    fn a_blank_part_is_not_sent() {
        let text = "a".repeat(4096) + "\n";
        assert_eq!(split_text(&text, TEXT_CAP).unwrap(), [&text[..4096]]);
    }
}
