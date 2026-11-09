//! The Bot API as the plugin uses it: the objects it reads (core.telegram.org/bots/api,
//! "Available types"), the error a request comes back with, and [`BotApi`], the one trait
//! every call is written against — the real client ([`crate::client`]) in the running
//! plugin.
//!
//! Only the members the plugin reads are decoded, and none is `deny_unknown_fields`:
//! Telegram adds members to its objects with every Bot API version.

use std::fmt;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// One `getUpdates` result. Only a message is asked for (`allowed_updates`), so an update
/// without one is a kind a later Telegram sends anyway.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgUpdate {
    pub(crate) update_id: i64,
    #[serde(default)]
    pub(crate) message: Option<TgMessage>,
}

/// A `Message`: one the bot received, or the one a send gave back.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgMessage {
    pub(crate) message_id: i64,
    /// The sender; absent for a message sent on behalf of a chat.
    #[serde(default)]
    pub(crate) from: Option<TgUser>,
    pub(crate) chat: TgChat,
    /// When it was sent, in Unix seconds.
    pub(crate) date: i64,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) caption: Option<String>,
    /// A photo's sizes, smallest first.
    #[serde(default)]
    pub(crate) photo: Option<Vec<TgPhotoSize>>,
    #[serde(default)]
    pub(crate) document: Option<TgFile>,
    #[serde(default)]
    pub(crate) video: Option<TgFile>,
    #[serde(default)]
    pub(crate) audio: Option<TgFile>,
    #[serde(default)]
    pub(crate) voice: Option<TgFile>,
    /// The message in the same chat this one replies to.
    #[serde(default)]
    pub(crate) reply_to_message: Option<Box<TgMessage>>,
}

/// A `User`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgUser {
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) first_name: String,
    #[serde(default)]
    pub(crate) last_name: Option<String>,
    #[serde(default)]
    pub(crate) username: Option<String>,
}

/// A `Chat`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgChat {
    pub(crate) id: i64,
    /// A group's or channel's title.
    #[serde(default)]
    pub(crate) title: Option<String>,
}

/// One size of a photo.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgPhotoSize {
    pub(crate) file_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    #[serde(default)]
    pub(crate) file_size: Option<u64>,
}

/// A `Document`, `Video`, `Audio` or `Voice`: the members the four share that the plugin
/// reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgFile {
    pub(crate) file_id: String,
    #[serde(default)]
    pub(crate) file_name: Option<String>,
    #[serde(default)]
    pub(crate) file_size: Option<u64>,
}

/// A `File`, as `getFile` answers: where its bytes are fetched from.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TgFileInfo {
    /// The path under `/file/bot<token>/`; absent when Telegram will not hand the file over.
    #[serde(default)]
    pub(crate) file_path: Option<String>,
}

/// A binary megabyte, the unit Telegram's file caps are counted in here.
pub(crate) const MIB: u64 = 1024 * 1024;

/// The most a bot may fetch of a file it received: Telegram's `getFile` hands over files of
/// up to 20 MB.
pub(crate) const FETCH_CAP: u64 = 20 * MIB;

/// What a file is sent as: the Bot API method that sends it, and the name of the part its
/// bytes go in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Document,
    Photo,
    Video,
    Audio,
    Voice,
}

impl FileKind {
    /// Every kind, in the order the manifest's `values` lists them.
    pub(crate) const ALL: [FileKind; 5] = [
        FileKind::Document,
        FileKind::Photo,
        FileKind::Video,
        FileKind::Audio,
        FileKind::Voice,
    ];

    /// The word a config writes, `.photo`, and the part the bytes go in.
    pub(crate) fn word(self) -> &'static str {
        match self {
            FileKind::Document => "document",
            FileKind::Photo => "photo",
            FileKind::Video => "video",
            FileKind::Audio => "audio",
            FileKind::Voice => "voice",
        }
    }

    /// The kind a config's word names.
    pub(crate) fn from_word(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.word() == word)
    }

    /// The Bot API method that sends it.
    pub(crate) fn method(self) -> &'static str {
        match self {
            FileKind::Document => "sendDocument",
            FileKind::Photo => "sendPhoto",
            FileKind::Video => "sendVideo",
            FileKind::Audio => "sendAudio",
            FileKind::Voice => "sendVoice",
        }
    }

    /// The most Telegram takes from a bot of it: 10 MB for a photo, 50 MB for the rest, in
    /// the binary megabytes afkd's own skill guarded with.
    pub(crate) fn cap(self) -> u64 {
        match self {
            FileKind::Photo => 10 * MIB,
            _ => 50 * MIB,
        }
    }
}

/// A file to send: the name Telegram shows, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Upload {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
}

/// What a Bot API request came back with when it did not give its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// Telegram answered `{"ok":false,…}`: its `error_code`, its `description` as it is,
    /// and the wait a 429's `parameters.retry_after` asked for.
    Refused {
        code: u16,
        description: String,
        retry_after: Option<Duration>,
    },
    /// No answer arrived: Telegram is out of reach, or the call's budget ran out.
    Transport(String),
    /// An answer arrived that does not read as the Bot API's.
    Decode(String),
}

impl ApiError {
    /// Whether Telegram refused with exactly this `description`, its code aside.
    pub(crate) fn says(&self, description: &str) -> bool {
        matches!(self, ApiError::Refused { description: d, .. } if d == description)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Refused {
                code: 429,
                retry_after: Some(wait),
                ..
            } => write!(f, "Telegram rate-limited the bot for {}s", wait.as_secs()),
            ApiError::Refused { description, .. } => {
                write!(f, "Telegram refused it: {description}")
            }
            ApiError::Transport(reason) => write!(f, "Telegram is out of reach: {reason}"),
            ApiError::Decode(reason) => write!(f, "Telegram's answer did not read: {reason}"),
        }
    }
}

/// A chat as a request names it: its id, or `@channel` for a public chat.
pub(crate) type Chat<'a> = &'a str;

/// The Bot API, one method per request the plugin sends. Every method that can be refused
/// says so as an [`ApiError`]; none retries.
pub(crate) trait BotApi {
    /// Clip every request sent until the next set to `deadline`, or with `None` lift the
    /// clip.
    fn set_deadline(&self, deadline: Option<Instant>);

    /// `getUpdates`: up to `limit` messages at `offset` on, every update below `offset`
    /// confirmed by the asking. Never a long poll.
    fn get_updates(&self, offset: Option<i64>, limit: u32) -> Result<Vec<TgUpdate>, ApiError>;

    /// `sendMessage`: `text` to `chat`, as a reply to `reply_to` there when given.
    fn send_message(
        &self,
        chat: Chat,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<TgMessage, ApiError>;

    /// `sendDocument` and its kin: `upload` to `chat` as `kind`, `caption` under it unless
    /// empty.
    fn send_file(
        &self,
        chat: Chat,
        kind: FileKind,
        upload: &Upload,
        caption: &str,
    ) -> Result<TgMessage, ApiError>;

    /// `setMessageReaction`: the bot's reaction to `message` set to `emoji`, or taken off
    /// with `None`.
    fn set_reaction(&self, chat: Chat, message: i64, emoji: Option<&str>) -> Result<(), ApiError>;

    /// `editMessageText`.
    fn edit_text(&self, chat: Chat, message: i64, text: &str) -> Result<(), ApiError>;

    /// `editMessageCaption`.
    fn edit_caption(&self, chat: Chat, message: i64, caption: &str) -> Result<(), ApiError>;

    /// `deleteMessage`.
    fn delete(&self, chat: Chat, message: i64) -> Result<(), ApiError>;

    /// `getFile`.
    fn get_file(&self, file_id: &str) -> Result<TgFileInfo, ApiError>;

    /// The bytes at a `getFile` path, at most `cap` of them.
    fn download(&self, file_path: &str, cap: u64) -> Result<Vec<u8>, ApiError>;
}
