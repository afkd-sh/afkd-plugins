//! The plugin's state across requests, and one handler per request: the thin layer between
//! afkd's wire ([`crate::wire`]) and the `board` kind ([`crate::service`]).
//!
//! The plugin has one kind, the main one: a config writes `service(afkd_board)`. afkd starts a
//! long-lived child for the service and arms it with a `hello`; it then sends `poll` once a
//! beat, `finish` after a unit's run ends and `release` for a journal key, and a `call` for each
//! read or write a slot makes. Every other call a config makes, from anywhere, is a child afkd
//! starts for that one `call`, which is sent no `hello` and carries the `proto` a `hello` would
//! have. Either way a call is answered the same, by [`reads`] or [`writes`], through the board
//! it names itself: no call reads the service's settings.
//!
//! Three things the wire forces:
//!
//! - **Who is asking arrives in `hello`.** The service, the claim owner and the plugin's own
//!   directory; a `hello` without them is refused, saying which is missing.
//! - **A request before an accepted `hello` ends the child.** afkd sends nothing but `hello`
//!   before one it has seen accepted, so the child writes why on stderr and exits non-zero.
//! - **Every reply fits one line**, and every armed call answers within its budget
//!   ([`crate::service`]).

use std::path::PathBuf;
use std::rc::Rc;

use serde_json::{json, Map, Value};

use crate::common::{Clock, Diag};
use crate::reads::{self, READS};
use crate::service::{Api, Connect, Identity, Remote, Service};
use crate::settings::ServiceConfig;
use crate::wire::{refusal, Request, PROTO};
use crate::writes::{self, WRITES};

/// The one kind this plugin provides, its main one: a config writes `service(afkd_board)`.
pub(crate) const KIND: &str = "board";

/// The sentence the process ends on when a request that is no call for a child of its own comes
/// before a `hello` this plugin accepted: afkd is out of step.
const BEFORE_HELLO: &str = "afkd sent a request before a `hello` this plugin accepted";

/// The optional calls the kind answers, exactly as `hello` lists them. `call` is protocol 2's own
/// and is never listed, and nothing else is asked of a board that has no notes to keep.
const CALLS: &[&str] = &["release"];

/// What the process does with one request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Write this line to stdout.
    Reply(String),
    /// Write this sentence to stderr and exit non-zero: afkd faults the service, and the
    /// sentence is the last thing in its log.
    Fatal(String),
}

/// The plugin across its whole life: unarmed until `hello`, then armed.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Rc<dyn Clock>,
    diag: Rc<dyn Diag>,
    /// This child's own pid, which tells it from the siblings sharing its identity.
    pid: u32,
    armed: Option<Service>,
}

impl Plugin {
    /// A plugin that arms against the real board, as the child of this pid.
    pub(crate) fn new(clock: Rc<dyn Clock>, diag: Rc<dyn Diag>) -> Self {
        let connect: Connect = Rc::new(|board, deadline, action| {
            Box::new(Remote::new(board, deadline, action)) as Box<dyn Api>
        });
        Self::with_connect(connect, clock, diag, std::process::id())
    }

    /// A plugin that arms through `connect`, as the child `pid`.
    pub(crate) fn with_connect(
        connect: Connect,
        clock: Rc<dyn Clock>,
        diag: Rc<dyn Diag>,
        pid: u32,
    ) -> Self {
        Self {
            connect,
            clock,
            diag,
            pid,
            armed: None,
        }
    }

    /// Answer one request.
    pub(crate) fn answer(&mut self, request: Request) -> Answer {
        match request.call.as_str() {
            "hello" => self.hello(&request),
            "poll" => self.armed(Service::poll),
            "finish" => self.armed(|s| s.finish(&request.key, request.outcome.as_deref())),
            "release" => self.armed(|s| s.release(&request.key)),
            "call" if self.armed.is_some() => Answer::Reply(served(&request.action, &request.args)),
            "call" => once(&request),
            _ if self.armed.is_some() => {
                self.diag
                    .err(&"afkd sent a call this plugin did not list in its `hello` reply");
                Answer::Reply(json!({"ok": false}).to_string())
            }
            _ => Answer::Fatal(BEFORE_HELLO.to_string()),
        }
    }

    /// Arm on `hello`, or refuse it saying why, and leave the plugin unarmed.
    fn hello(&mut self, request: &Request) -> Answer {
        self.armed = None;
        let reply = match self.arm(request) {
            Ok(service) => {
                self.armed = Some(service);
                json!({"ok": true, "proto": PROTO, "calls": CALLS})
            }
            Err(why) => {
                self.diag.err(&why);
                json!({"ok": false, "proto": PROTO})
            }
        };
        Answer::Reply(reply.to_string())
    }

    /// The service `hello` asks for, or the sentence saying why it cannot be armed. afkd has
    /// already held the block to the manifest, so what can be wrong here is the protocol, the
    /// kind, a missing identity or directory, or a rule the manifest cannot express.
    fn arm(&self, hello: &Request) -> Result<Service, String> {
        match hello.proto {
            Some(PROTO) => {}
            Some(proto) => {
                return Err(format!(
                    "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
                ))
            }
            None => {
                return Err(format!(
                    "afkd sent a hello with no protocol, and this plugin speaks {PROTO}"
                ))
            }
        }
        if hello.kind != KIND {
            return Err(format!(
                "kind `{}` is not provided by @afkd/afkd_board",
                hello.kind
            ));
        }
        if hello.service.is_empty() || hello.owner.is_empty() {
            return Err(
                "afkd did not say which service this is; @afkd/afkd_board needs an afkd \
                        whose `hello` carries `service` and `owner`"
                    .to_string(),
            );
        }
        let Some(state_dir) = hello.state_dir.as_deref().filter(|dir| !dir.is_empty()) else {
            return Err(
                "afkd did not name the plugin's state directory; @afkd/afkd_board needs \
                        an afkd whose `hello` carries `state_dir`"
                    .to_string(),
            );
        };
        let cfg = ServiceConfig::from_settings(&hello.settings)
            .map_err(|e| format!("trigger {KIND}: {e}"))?;
        let id = Identity {
            service: hello.service.clone(),
            owner: hello.owner.clone(),
        };
        Ok(Service::new(
            cfg,
            id,
            &PathBuf::from(state_dir),
            self.pid,
            Rc::clone(&self.connect),
            Rc::clone(&self.clock),
            Rc::clone(&self.diag),
        ))
    }

    /// Run `f` on the armed service, or end the process, since afkd sends nothing but `hello`
    /// before a `hello` it has seen accepted.
    fn armed(&mut self, f: impl FnOnce(&mut Service) -> String) -> Answer {
        match &mut self.armed {
            Some(service) => Answer::Reply(f(service)),
            None => Answer::Fatal(BEFORE_HELLO.to_string()),
        }
    }
}

/// Answer the one `call` a child afkd started for it gets, with no `hello` before it: a read or a
/// write. A call of another protocol, or of none, is refused in a reply, as is an action no one
/// declared.
fn once(request: &Request) -> Answer {
    if request.proto != Some(PROTO) {
        let proto = request
            .proto
            .map_or_else(|| "no protocol".to_string(), |p| format!("protocol {p}"));
        return Answer::Reply(refusal(&format!(
            "afkd sent a call of {proto}, and this plugin speaks plugin protocol {PROTO}"
        )));
    }
    Answer::Reply(served(&request.action, &request.args))
}

/// The reply to the call `action` with `args`: a read, a write, or the refusal naming the
/// actions there are.
fn served(action: &str, args: &Map<String, Value>) -> String {
    if READS.contains(&action) {
        return reads::answer(action, args);
    }
    if WRITES.contains(&action) {
        return writes::answer(action, args);
    }
    let names: Vec<&str> = READS.iter().chain(WRITES).copied().collect();
    let has = match names.split_last() {
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    };
    refusal(&format!("no action `{action}`; @afkd/afkd_board has {has}"))
}

#[cfg(test)]
mod tests;
