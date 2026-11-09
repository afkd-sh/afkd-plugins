//! The plugin's state across calls, and one handler per call: the thin layer between afkd's
//! wire ([`crate::wire`]), the `message` kind ([`crate::trigger`]) and the actions
//! ([`crate::action`]).
//!
//! A child is one of two things. **Armed**, by a `hello` for a `service(telegram)`, it polls
//! and answers that service's calls as its bot. **Unarmed**, started by afkd for one call
//! made anywhere else, it answers that `call` alone, as the bot the plugin's settings file
//! names. Either way:
//!
//! - **Every call answers within 45 seconds**: each runs under [`CALL_BUDGET`], and a
//!   Telegram too slow for it reads as a Telegram out of reach.
//! - **The lines go to stderr**, which afkd files under `[@afkd/telegram:err]`.

use serde_json::{json, Map, Value};

use crate::action::{from_call, perform, RETURNING};
use crate::botapi::BotApi;
use crate::client::TelegramClient;
use crate::common::{Clock, Diag, CALL_BUDGET};
use crate::settings::{call_credentials, message_config, Credentials};
use crate::trigger::{Polled, Trigger, MESSAGE_KIND};
use crate::wire::{handle_line, refusal_line, Request, PROTO};

/// What the process does with one request.
#[derive(Debug, PartialEq)]
pub(crate) enum Answer {
    /// Write this line to stdout.
    Reply(String),
    /// Write this sentence to stderr and exit non-zero: afkd faults the service, and the
    /// sentence is the last thing in its log.
    Fatal(String),
}

/// Builds the Bot API a child speaks as the bot of the credentials — the real client in the
/// running plugin.
pub(crate) type Connect = Box<dyn Fn(&Credentials) -> Box<dyn BotApi>>;

/// The plugin across its whole life: unarmed until `hello`, then armed.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Box<dyn Clock>,
    diag: Box<dyn Diag>,
    armed: Option<Armed>,
}

/// The armed service: its bot, and its kind.
struct Armed {
    api: Box<dyn BotApi>,
    trigger: Trigger,
}

impl Plugin {
    /// A plugin that speaks to the real Bot API, at the settings' `base_url`.
    pub(crate) fn new(clock: Box<dyn Clock>, diag: Box<dyn Diag>) -> Self {
        Self {
            connect: Box::new(|creds| {
                Box::new(TelegramClient::new(&creds.base_url, &creds.token)) as Box<dyn BotApi>
            }),
            clock,
            diag,
            armed: None,
        }
    }

    /// Answer one request.
    pub(crate) fn answer(&mut self, request: Request) -> Answer {
        let (clock, diag) = (&*self.clock, &*self.diag);
        match request {
            Request::Hello {
                proto,
                kind,
                settings,
            } => {
                self.armed = arm(&self.connect, proto, &kind, &settings)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                let ok = self.armed.is_some();
                Answer::Reply(json!({"ok": ok, "proto": PROTO}).to_string())
            }
            Request::Poll => {
                let Some(armed) = &mut self.armed else {
                    return Answer::Fatal(
                        "afkd sent a poll before a `hello` this plugin accepted".into(),
                    );
                };
                armed.api.set_deadline(Some(clock.now() + CALL_BUDGET));
                let polled = armed.trigger.poll(&*armed.api, diag);
                armed.api.set_deadline(None);
                match polled {
                    Polled::Idle => Answer::Reply(json!({"fire": false}).to_string()),
                    Polled::Fire(line) => Answer::Reply(line),
                    Polled::Fatal(reason) => Answer::Fatal(reason),
                }
            }
            // A message has nothing to settle once its run ends.
            Request::Finish => Answer::Reply(json!({"ok": true}).to_string()),
            Request::Call {
                action,
                args,
                proto,
                settings,
            } => Answer::Reply(match self.call(&action, &args, proto, settings.as_ref()) {
                Ok(reply) => reply,
                Err(error) => refusal_line(&error),
            }),
            Request::Unknown => {
                diag.err(&"afkd sent a call this plugin did not list in its `hello` reply");
                Answer::Reply(json!({"ok": false}).to_string())
            }
        }
    }

    /// Do one action, on the armed service's bot or, in a child started for this call, on
    /// the bot the plugin's settings file names: the reply line, or the refusal's sentence.
    fn call(
        &self,
        action: &str,
        args: &Map<String, Value>,
        proto: Option<u32>,
        settings: Option<&Value>,
    ) -> Result<String, String> {
        if let Some(proto) = proto.filter(|p| *p != PROTO) {
            return Err(format!(
                "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
            ));
        }
        let call = from_call(action, args)?;
        let own;
        let api: &dyn BotApi = match &self.armed {
            Some(armed) => &*armed.api,
            None => {
                own = (self.connect)(&call_credentials(settings)?);
                &*own
            }
        };
        let deadline = self.clock.now() + CALL_BUDGET;
        api.set_deadline(Some(deadline));
        let done = perform(&call, api, &*self.clock, deadline);
        api.set_deadline(None);
        match done? {
            Some(handle) => Ok(handle_line(&handle)),
            None if RETURNING.contains(&action) => {
                Err(format!("{action}: Telegram sent no message back"))
            }
            None => Ok(json!({"ok": true}).to_string()),
        }
    }
}

/// Arm the kind `hello` names over its settings, or say why not. afkd has already held the
/// block to the manifest, so what can be wrong here is the protocol, the kind, or an empty
/// token or chat list. Nothing is sent to Telegram: an outage when the service arms is a
/// poll's idle beat, not a refused service.
fn arm(connect: &Connect, proto: u32, kind: &str, settings: &Value) -> Result<Armed, String> {
    if proto != PROTO {
        return Err(format!(
            "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
        ));
    }
    if kind != MESSAGE_KIND {
        return Err(format!(
            "kind `{kind}` is not provided by @afkd/telegram, whose one kind is `{MESSAGE_KIND}`"
        ));
    }
    let cfg = message_config(settings).map_err(|e| format!("trigger {kind}: {e}"))?;
    Ok(Armed {
        api: connect(&cfg.credentials),
        trigger: Trigger::new(cfg),
    })
}
