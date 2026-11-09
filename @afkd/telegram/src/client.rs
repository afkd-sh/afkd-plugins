//! The real [`BotApi`]: Telegram's Bot API over HTTPS (core.telegram.org/bots/api,
//! "Making requests"), a method per request at `<base>/bot<token>/<method>`.
//!
//! Every method but an upload sends its parameters as a JSON body; an upload sends a
//! `multipart/form-data` one ([`crate::multipart`]). Every answer is the Bot API's envelope,
//! `{"ok":true,"result":…}` or `{"ok":false,"error_code":…,"description":…}`, on a 200 or a
//! 4xx alike, and a refusal is read into [`ApiError::Refused`] with Telegram's description
//! as it is and a 429's `parameters.retry_after`. Nothing here retries: a call's caller
//! decides what a 429 is worth waiting for.

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::botapi::{ApiError, BotApi, Chat, FileKind, TgFileInfo, TgMessage, TgUpdate, Upload};
use crate::http::{Http, Reply};
use crate::multipart::{form, FilePart};

/// The Bot API's root, which a service's `base_url` replaces.
pub(crate) const TELEGRAM_BASE: &str = "https://api.telegram.org";

/// The Bot API spoken as one bot.
pub(crate) struct TelegramClient {
    base: String,
    token: String,
    http: Http,
}

impl TelegramClient {
    /// A client against the API root `base`, as the bot of `token`.
    pub(crate) fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: Http::new(token),
        }
    }

    /// Call `method` with `params` as a JSON body, and read its result.
    fn call<T: DeserializeOwned>(&self, method: &str, params: &Value) -> Result<T, ApiError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        let reply = self
            .http
            .post(&url, "application/json", params.to_string().as_bytes())?;
        read_reply(&reply)
    }
}

/// A chat as the Bot API takes it: an id as a number, `@channel` as a string.
fn chat_id(chat: Chat) -> Value {
    chat.parse::<i64>()
        .map_or_else(|_| json!(chat), |id| json!(id))
}

/// The Bot API's envelope, either way it comes.
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error_code: Option<u16>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Parameters>,
}

/// A refusal's `ResponseParameters`.
#[derive(Deserialize)]
struct Parameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

/// The result `reply` carries, or the refusal it is.
fn read_reply<T: DeserializeOwned>(reply: &Reply) -> Result<T, ApiError> {
    let envelope: Envelope = serde_json::from_slice(&reply.body).map_err(|_| {
        ApiError::Decode(format!("HTTP {} without the Bot API's JSON", reply.status))
    })?;
    if !envelope.ok {
        return Err(ApiError::Refused {
            code: envelope.error_code.unwrap_or(reply.status),
            description: envelope
                .description
                .unwrap_or_else(|| format!("HTTP {}", reply.status)),
            retry_after: envelope
                .parameters
                .and_then(|p| p.retry_after)
                .map(Duration::from_secs),
        });
    }
    serde_json::from_value(envelope.result).map_err(|e| ApiError::Decode(e.to_string()))
}

impl BotApi for TelegramClient {
    fn set_deadline(&self, deadline: Option<Instant>) {
        self.http.set_deadline(deadline);
    }

    fn get_updates(&self, offset: Option<i64>, limit: u32) -> Result<Vec<TgUpdate>, ApiError> {
        let mut params = json!({"limit": limit, "timeout": 0, "allowed_updates": ["message"]});
        if let Some(offset) = offset {
            params["offset"] = json!(offset);
        }
        self.call("getUpdates", &params)
    }

    fn send_message(
        &self,
        chat: Chat,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<TgMessage, ApiError> {
        let mut params = json!({"chat_id": chat_id(chat), "text": text});
        if let Some(id) = reply_to {
            params["reply_parameters"] = json!({"message_id": id});
        }
        self.call("sendMessage", &params)
    }

    fn send_file(
        &self,
        chat: Chat,
        kind: FileKind,
        upload: &Upload,
        caption: &str,
    ) -> Result<TgMessage, ApiError> {
        let mut fields = vec![("chat_id", chat)];
        if !caption.is_empty() {
            fields.push(("caption", caption));
        }
        let (content_type, body) = form(
            &fields,
            &FilePart {
                field: kind.word(),
                name: &upload.name,
                bytes: &upload.bytes,
            },
        );
        let url = format!("{}/bot{}/{}", self.base, self.token, kind.method());
        read_reply(&self.http.post(&url, &content_type, &body)?)
    }

    fn set_reaction(&self, chat: Chat, message: i64, emoji: Option<&str>) -> Result<(), ApiError> {
        let reaction = match emoji {
            Some(emoji) => json!([{"type": "emoji", "emoji": emoji}]),
            None => json!([]),
        };
        self.call::<Value>(
            "setMessageReaction",
            &json!({"chat_id": chat_id(chat), "message_id": message, "reaction": reaction}),
        )
        .map(drop)
    }

    fn edit_text(&self, chat: Chat, message: i64, text: &str) -> Result<(), ApiError> {
        self.call::<Value>(
            "editMessageText",
            &json!({"chat_id": chat_id(chat), "message_id": message, "text": text}),
        )
        .map(drop)
    }

    fn edit_caption(&self, chat: Chat, message: i64, caption: &str) -> Result<(), ApiError> {
        self.call::<Value>(
            "editMessageCaption",
            &json!({"chat_id": chat_id(chat), "message_id": message, "caption": caption}),
        )
        .map(drop)
    }

    fn delete(&self, chat: Chat, message: i64) -> Result<(), ApiError> {
        self.call::<Value>(
            "deleteMessage",
            &json!({"chat_id": chat_id(chat), "message_id": message}),
        )
        .map(drop)
    }

    fn get_file(&self, file_id: &str) -> Result<TgFileInfo, ApiError> {
        self.call("getFile", &json!({"file_id": file_id}))
    }

    fn download(&self, file_path: &str, cap: u64) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}/file/bot{}/{file_path}", self.base, self.token);
        let reply = self.http.get_bytes(&url, cap)?;
        if reply.status == 200 {
            return Ok(reply.body);
        }
        // A file that is not there is answered in the envelope; anything else is a status.
        Err(read_reply::<Value>(&reply)
            .err()
            .unwrap_or(ApiError::Refused {
                code: reply.status,
                description: format!("HTTP {}", reply.status),
                retry_after: None,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    /// A refusal is read with Telegram's description as it is, and a 429's wait.
    #[test]
    fn a_refusal_reads_its_code_description_and_wait() {
        let e = read_reply::<Value>(&reply(
            429,
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7",
                "parameters":{"retry_after":7}}"#,
        ))
        .unwrap_err();
        assert_eq!(
            e,
            ApiError::Refused {
                code: 429,
                description: "Too Many Requests: retry after 7".into(),
                retry_after: Some(Duration::from_secs(7)),
            }
        );
        assert_eq!(e.to_string(), "Telegram rate-limited the bot for 7s");

        let e = read_reply::<Value>(&reply(
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
        ))
        .unwrap_err();
        assert!(e.says("Bad Request: chat not found"));
        assert_eq!(
            e.to_string(),
            "Telegram refused it: Bad Request: chat not found"
        );
    }

    /// A body that is not the envelope — a proxy's HTML page — is undecodable, naming the
    /// status.
    #[test]
    fn a_reply_that_is_not_the_envelope_is_undecodable() {
        let e = read_reply::<Value>(&reply(502, "<html>Bad Gateway</html>")).unwrap_err();
        assert_eq!(
            e,
            ApiError::Decode("HTTP 502 without the Bot API's JSON".into())
        );
    }

    #[test]
    fn a_chat_is_a_number_unless_it_is_a_username() {
        assert_eq!(chat_id("-1001234567890"), json!(-1_001_234_567_890_i64));
        assert_eq!(chat_id("@ops_channel"), json!("@ops_channel"));
    }
}
