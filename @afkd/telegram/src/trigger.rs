//! The `message` kind — `service(telegram)` in a config: a message the bot receives in a
//! chat the service lists, with a text, a file or both, handed over as one unit per run.
//!
//! **One reader.** Telegram keeps a bot's updates until they are confirmed, and confirms
//! every update below the `offset` a `getUpdates` asks with; two readers of one token get a
//! 409. So the trigger is the only thing that reads them, and never long-polls, since afkd
//! asks for a prompt `poll`.
//!
//! **At most once.** Before a message is handed over it is confirmed, with a
//! `getUpdates` at the offset past it, so a daemon restart never starts a second run for
//! it: missing one message on a crash costs less than an agent acting twice in a chat. A
//! confirm that fails hands nothing over, and the message comes back on the next poll.
//!
//! **Skipped messages move the offset on.** A message from a chat `chats` does not list, or
//! one with neither text nor file (a sticker, a service message), is skipped with one line
//! naming its chat, never its text. A batch with nothing to hand over moves the offset past
//! its last update, so the next poll confirms the batch: a skipped message is fetched and
//! said once, and a backlog of them never stands in front of one that matches.

use std::collections::BTreeMap;

use crate::botapi::{ApiError, BotApi, TgMessage};
use crate::common::{Diag, ENV_API_URL, ENV_CHAT_ID, ENV_MESSAGE_ID, ENV_TOKEN};
use crate::message::{brief, handle, has_content, thread};
use crate::settings::MessageConfig;
use crate::wire::{fire_line, WireFile, WireUnit};

/// The one kind this plugin provides, its main one: `service(telegram)` in a config.
pub(crate) const MESSAGE_KIND: &str = "message";

/// The most updates one poll reads, Telegram's own most.
const BATCH: u32 = 100;

/// What one poll comes to.
#[derive(Debug, PartialEq)]
pub(crate) enum Polled {
    /// Nothing to hand over.
    Idle,
    /// The reply line handing a message over.
    Fire(String),
    /// A fault no later poll can fix: the service ends, saying why.
    Fatal(String),
}

/// The armed kind: its settings, and where the next read starts.
pub(crate) struct Trigger {
    cfg: MessageConfig,
    /// The `offset` the next `getUpdates` asks with, `None` before the first read: then
    /// Telegram starts at the oldest update it holds.
    offset: Option<i64>,
}

impl Trigger {
    pub(crate) fn new(cfg: MessageConfig) -> Self {
        Self { cfg, offset: None }
    }

    /// One beat: read the pending updates, skip what does not start a run, and hand over the
    /// first message that does, once it is confirmed.
    pub(crate) fn poll(&mut self, api: &dyn BotApi, diag: &dyn Diag) -> Polled {
        let updates = match api.get_updates(self.offset, BATCH) {
            Ok(updates) => updates,
            Err(ApiError::Refused { code: 401, .. }) => {
                return Polled::Fatal(
                    "Telegram refused the bot token (401 Unauthorized): check the service's \
                     `token`"
                        .into(),
                )
            }
            Err(ApiError::Refused {
                code: 409,
                description,
                ..
            }) => {
                return Polled::Fatal(format!(
                    "Telegram will not hand this service the bot's messages ({description}): a \
                     bot token has one reader, so run one service(telegram) per token and no \
                     webhook beside it"
                ))
            }
            Err(e) => {
                diag.err(&format_args!("telegram poll: {e}"));
                return Polled::Idle;
            }
        };
        for update in &updates {
            let Some(message) = &update.message else {
                diag.err(&format_args!(
                    "skipped update {}: it carries no message",
                    update.update_id
                ));
                continue;
            };
            let chat = message.chat.id.to_string();
            if !self.cfg.chats.contains(&chat) {
                diag.err(&format_args!(
                    "skipped message {} in chat {chat}, which `chats` does not list",
                    message.message_id
                ));
                continue;
            }
            if !has_content(message) {
                diag.err(&format_args!(
                    "skipped message {} in chat {chat}: it carries neither text nor a file",
                    message.message_id
                ));
                continue;
            }
            // Confirm it, and every update before it, before afkd hears of it.
            let next = update.update_id + 1;
            if let Err(e) = api.get_updates(Some(next), 1) {
                diag.err(&format_args!(
                    "telegram poll: message {} in chat {chat} could not be confirmed, so it \
                     waits for the next poll: {e}",
                    message.message_id
                ));
                return Polled::Idle;
            }
            self.offset = Some(next);
            return match fire_line(&self.unit(message)) {
                Some(line) => Polled::Fire(line),
                None => {
                    diag.err(&format_args!(
                        "message {} in chat {chat} is over afkd's 64 KiB plugin line and was \
                         dropped",
                        message.message_id
                    ));
                    Polled::Idle
                }
            };
        }
        if let Some(last) = updates.last() {
            self.offset = Some(last.update_id + 1);
        }
        Polled::Idle
    }

    /// The unit handing `message` over: its handle, the chat's thread, the env a run reaches
    /// Telegram and the message with, and the brief.
    fn unit(&self, message: &TgMessage) -> WireUnit {
        let handle = handle(message);
        let creds = &self.cfg.credentials;
        WireUnit {
            thread: thread(&handle.fields.chat),
            env: BTreeMap::from([
                (ENV_TOKEN.to_string(), creds.token.clone()),
                (ENV_API_URL.to_string(), creds.base_url.clone()),
                (ENV_CHAT_ID.to_string(), handle.fields.chat.clone()),
                (ENV_MESSAGE_ID.to_string(), handle.id.clone()),
            ]),
            files: vec![WireFile {
                path: crate::TASK_FILE.to_string(),
                text: brief(message),
            }],
            handle,
        }
    }
}
