//! The gate that holds the **shipped `@afkd/web-top` companion** — the relay that puts the
//! daemon's control wire in front of a browser — to a real afkd: the `afkd` first on `PATH`,
//! the installed one and never a build, since a plugin is checked against the afkd it will
//! meet ([`bin_path`]). The legs that read afkd's own source as well want an afkd checkout
//! named by `AFKD_SRC` ([`afkd_src`]), and skip loudly without one.
//!
//! web-top folds **nothing**, and that is the shape the suite is built around.
//! Every subscriber opens its own attach, every frame that attach reads is forwarded
//! verbatim, and the commands a page posts are keyed back to that subscriber's own socket.
//! So the claims here are about *per-subscriber* behaviour — two snapshots rather than one
//! shared ring, a ceiling that is really a ceiling, a fire that fans to both tabs — which
//! no single-client suite can make.
//!
//! The relay is python3, so every python-driven leg **gates on `python3`** and skips loudly
//! without it ([`python3_available`]).
//!
//! Two shapes of daemon appear, deliberately:
//!
//! - The **real** one, for everything an operator can see: the install, the page, the
//!   streams, the fires they drive and the drain.
//! - A **stub** ([`Stub`]) — a `UnixListener` in a temp dir with the relay spawned as the
//!   test's own child — for the three things a correct daemon cannot show. A conforming
//!   daemon never rejects a correct hello, never writes a line past the wire's own 64 KiB
//!   cap, and never starts a companion whose settings it would have refused at `validate`.
//!   Being the parent is the only honest oracle for all three.
//!
//! Every blocking read is bounded by a deadline or a socket timeout: a frame that never
//! arrives must **fail**, not hang. No network anywhere — the only sockets are the daemon's
//! own and a loopback listener the relay binds.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The relay's name, as the manifest spells it and every verb reads it back.
const NAME: &str = "@afkd/web-top";

/// The relay's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, because the install report echoes the resolved path
/// and the test compares against it.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/web-top");
    std::fs::canonicalize(root).expect("the shipped relay resolves")
}

/// The version the shipped manifest declares — **parsed**, so a version bump moves the
/// plugin and not this file.
fn manifest_version() -> String {
    let manifest =
        std::fs::read_to_string(plugin_root().join("afkd-plugin.toml")).expect("the manifest");
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("the manifest declares a version")
}

/// The release after `version` in its minor digit, `0.1.4` → `0.2.0`: the version the update
/// leg's second tarball declares, derived rather than written down so that a bump of the
/// shipped manifest can never make "newer" name the version already installed.
fn next_minor(version: &str) -> String {
    let mut digits = version.split('.').map(|digit| {
        digit
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("{version} is not a dotted version"))
    });
    let major = digits.next().expect("a major digit");
    let minor = digits.next().expect("a minor digit");
    format!("{major}.{}.0", minor + 1)
}

// --- the fixtures ------------------------------------------------------------------

/// Two services the relay has to carry.
///
/// The fired one is **namespaced and wide-glyph on purpose**: its name has to survive the
/// snapshot frame, a `POST /command` body, the frame the relay composes from it and the
/// daemon's own `[name]`-prefixed log lines — and its `group` is a prefix of its own
/// qualified name. `never` is the negative control nothing ever fires, so a command that
/// must not reach the wire has a file on disk to be absent.
const SERVICES: &str = r#"service ops::監視 {
  description "ウェブ — the wide one"
  trigger none
  run {
    run_cmd "touch fired.txt"
    run_cmd "echo ライン one"
  }
}

service never {
  trigger none
  run { run_cmd "touch never.txt" }
}
"#;

/// The fired service's fully-qualified name, as the wire carries it.
const BUSY: &str = "ops::監視";

/// `SERVICES` plus a `plugin` block naming the shipped relay.
fn config_with_block(body: &str) -> String {
    format!("{SERVICES}\nplugin {NAME} {{\n{body}}}\n")
}

/// Install the shipped relay into `home` through the real verb.
fn install_relay(home: &Path) -> (String, String, Option<i32>) {
    let out = run_subcommand_args(home, &["install", &plugin_root().display().to_string()]);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// A free loopback port, taken by binding an ephemeral one and closing it. Used only where
/// the **same** address must be served twice.
fn free_loopback_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

// --- budgets ------------------------------------------------------------------------

/// How long a daemon-log line, a page reply or a child's exit may take. Generous, because
/// it bounds a failure rather than timing a success.
const BUDGET: Duration = Duration::from_secs(20);

/// The per-read timeout on every socket this file opens, so a peer that goes quiet fails
/// the leg at its own deadline instead of wedging the test binary.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

// --- the daemon ----------------------------------------------------------------------

/// Poll the daemon log until it holds `needle`, up to `budget`, returning what it holds.
fn wait_for_daemon_log(home: &Path, needle: &str, budget: Duration) -> String {
    let deadline = Instant::now() + budget;
    loop {
        let log = std::fs::read_to_string(daemon_log(home)).unwrap_or_default();
        if log.contains(needle) || Instant::now() >= deadline {
            return log;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The port the relay's own serving line announced, off whatever pumped it.
///
/// Confined to that **one line**: the daemon pumps the companion's two streams
/// independently, so a stderr sentence can land after the serving line in the same
/// millisecond — and a search that ran to the end of the log would then read its last colon
/// instead of the port's.
fn serving_port(text: &str) -> Option<u16> {
    text.rsplit_once("serving http://")
        .and_then(|(_, rest)| rest.lines().next())
        .and_then(|line| line.rsplit_once(':'))
        .map(|(_, digits)| {
            digits
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse().ok())
}

/// Start a headless daemon over `config` with the relay installed, and wait for the page it
/// serves. Returns the daemon, its `$HOME`, and the port the relay bound.
fn daemon_serving(config: &str) -> (StreamingDaemon, TempDir, u16) {
    let dir = dir_with_config(config);
    let (out, err, code) = install_relay(dir.path());
    assert_eq!(code, Some(0), "installing the shipped relay: {out}{err}");
    let daemon = spawn_headless_streaming(dir.path(), &[]);
    let port = relay_port(dir.path());
    (daemon, dir, port)
}

/// The port the relay announced into `home`'s daemon log, or a panic quoting the log.
fn relay_port(home: &Path) -> u16 {
    let log = wait_for_daemon_log(home, "serving http://", BUDGET);
    serving_port(&log).unwrap_or_else(|| panic!("the relay never announced a port:\n{log}"))
}

// --- a minimal HTTP client -----------------------------------------------------------

/// One HTTP/1.1 exchange against the relay: send `request`, read to EOF, and split the
/// reply into its status code, its headers and its body. `Connection: close` on the way out
/// is what makes "read to EOF" the whole answer.
fn http(port: u16, request: &str) -> (u16, String, String) {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).expect("the relay's listener accepts a connection");
    stream.set_read_timeout(Some(BUDGET)).expect("read timeout");
    stream
        .write_all(request.as_bytes())
        .expect("write the request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the whole reply");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("the reply has no header/body split:\n{text}"));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("the reply has no status code:\n{head}"));
    (status, head.to_string(), body.to_string())
}

fn get(port: u16, path: &str) -> (u16, String, String) {
    http(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"),
    )
}

/// `POST /command` with `body` — the exact shape the page's own client sends.
fn post_command(port: u16, body: &serde_json::Value) -> (u16, String) {
    let body = body.to_string();
    let (status, _, reply) = http(
        port,
        &format!(
            "POST /command HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    (status, reply)
}

// --- the event stream ---------------------------------------------------------------

/// `GET /stream`, held open on a socket with a read deadline.
///
/// The relay's own signals are **named** events (`stream`, `welcome`, `refused`, `closed`,
/// `bye`, `error`) and every unnamed one is a verbatim control-wire frame, so this reader
/// keeps the distinction rather than flattening it — the difference is the whole contract.
struct Sse {
    reader: BufReader<TcpStream>,
    status: u16,
}

impl Sse {
    /// Open the stream and consume its status line and headers.
    fn open(port: u16) -> (Sse, Vec<String>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the stream");
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .expect("read timeout");
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write the request");
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .expect("read the status line");
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("GET /stream has no status code: {status_line}"));
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read a header line");
            if read == 0 || line.trim().is_empty() {
                break;
            }
            headers.push(line.trim().to_string());
        }
        (Sse { reader, status }, headers)
    }

    /// Open the stream and assert it was admitted.
    fn admitted(port: u16) -> Sse {
        let (sse, headers) = Sse::open(port);
        assert_eq!(sse.status, 200, "GET /stream was refused: {headers:?}");
        assert!(
            headers
                .iter()
                .any(|h| h.eq_ignore_ascii_case("Content-Type: text/event-stream")),
            "an event stream says so in its headers: {headers:?}"
        );
        sse
    }

    /// The next line, or `None` once `deadline` passes or the stream ends.
    fn line(&mut self, deadline: Instant) -> Option<String> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => return Some(line.trim_end().to_string()),
                // A quiet stream is not a dead one: the read timeout is short so the
                // deadline, not the socket, decides when this leg gives up.
                Err(_) if Instant::now() < deadline => continue,
                Err(_) => return None,
            }
        }
    }

    /// The next `(event, payload)` pair: `None` for the unnamed events that carry a
    /// verbatim wire frame, `Some(name)` for the relay's own. Keepalive comments are
    /// skipped — they are the absence of news, not news.
    fn next(&mut self, budget: Duration) -> Option<(Option<String>, String)> {
        let deadline = Instant::now() + budget;
        let mut named = None;
        while let Some(line) = self.line(deadline) {
            if let Some(name) = line.strip_prefix("event: ") {
                named = Some(name.to_string());
            } else if let Some(payload) = line.strip_prefix("data: ") {
                return Some((named, payload.to_string()));
            }
        }
        None
    }

    /// The next named event and its payload, or a panic naming what did arrive.
    fn named(&mut self, budget: Duration) -> (String, serde_json::Value) {
        match self.next(budget) {
            Some((Some(name), payload)) => (
                name,
                serde_json::from_str(&payload)
                    .unwrap_or_else(|e| panic!("an event payload is not JSON ({e}): {payload}")),
            ),
            Some((None, payload)) => panic!("expected a named event, got a wire frame: {payload}"),
            None => panic!("no event arrived inside {budget:?}"),
        }
    }

    /// The next verbatim wire frame, decoded — named events in between are skipped.
    fn frame(&mut self, budget: Duration) -> serde_json::Value {
        let deadline = Instant::now() + budget;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match self.next(Duration::from_millis(500)) {
                Some((None, payload)) => {
                    return serde_json::from_str(&payload).unwrap_or_else(|e| {
                        panic!("a forwarded frame is not JSON ({e}): {payload}")
                    });
                }
                Some((Some(name), payload)) => seen.push(format!("{name}: {payload}")),
                None => continue,
            }
        }
        panic!("no wire frame arrived inside {budget:?}; the stream said {seen:?}");
    }

    /// Frames until `pred` holds over one, or a panic naming what did arrive.
    fn frame_matching(
        &mut self,
        budget: Duration,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + budget;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let frame = self.frame(deadline.saturating_duration_since(Instant::now()));
            if pred(&frame) {
                return frame;
            }
            seen.push(frame);
        }
        panic!("no matching frame inside {budget:?}; the stream carried {seen:#?}");
    }

    /// This subscriber's handshake: its id, and the welcome. The two things every stream
    /// opens with, in the order the relay fixes.
    fn handshake(&mut self) -> String {
        self.handshake_with_welcome().0
    }

    /// [`handshake`](Self::handshake), keeping the welcome itself rather than dropping it.
    /// The drift leg renders the page's header off `welcome["daemon"]` — the one field
    /// `top.mjs` reads off the handshake — so the version it holds against `afkd top`'s is
    /// the daemon's own answer and not a literal this file would have to bump.
    fn handshake_with_welcome(&mut self) -> (String, serde_json::Value) {
        let (event, payload) = self.named(BUDGET);
        assert_eq!(event, "stream", "a stream opens with its own id: {payload}");
        let id = payload["id"]
            .as_str()
            .unwrap_or_else(|| panic!("the stream id is not a string: {payload}"))
            .to_string();
        assert_eq!(
            id.len(),
            16,
            "the id is 16 hex characters, not something a tab next door could count to: {id}"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "the id is hex: {id}"
        );
        // The ring bound the page's fold needs before the first frame lands, settled from
        // `log_lines` — so a page never has to guess how far back it may scroll.
        assert!(
            payload["log_lines"].as_u64().is_some_and(|n| n > 0),
            "the id comes with the fold's ring bound: {payload}"
        );
        let (event, welcome) = self.named(BUDGET);
        assert_eq!(
            event, "welcome",
            "…then the daemon's own welcome: {welcome}"
        );
        assert_eq!(
            welcome["afkd"], "welcome",
            "the welcome crosses verbatim: {welcome}"
        );
        (id, welcome)
    }

    /// Collect every verbatim wire frame this stream forwards, appending each one's raw
    /// payload to `into`, until `pred` holds over one or `budget` elapses. Returns whether
    /// `pred` ever held.
    ///
    /// Raw, because the drift leg's whole input is the JSONL a browser's fold would have
    /// seen — a re-serialization would be this file's spelling of the wire rather than the
    /// daemon's. Named events are counted out: they are the relay's own signals, and the
    /// page hands them nowhere near `fold`.
    fn collect_frames(
        &mut self,
        budget: Duration,
        into: &mut Vec<String>,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            let Some((named, payload)) = self.next(Duration::from_millis(250)) else {
                continue;
            };
            if named.is_some() {
                continue;
            }
            let frame: serde_json::Value = serde_json::from_str(&payload)
                .unwrap_or_else(|e| panic!("a forwarded frame is not JSON ({e}): {payload}"));
            into.push(payload);
            if pred(&frame) {
                return true;
            }
        }
        false
    }

    /// Whether the stream ends inside `budget`, taking any final event on the way.
    fn ends_within(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if self.line(deadline).is_none() {
                return true;
            }
        }
        false
    }
}

/// `GET /stream` expecting a **refusal**: the status, and the sentence that came with it.
///
/// Bounded by its own deadline rather than by EOF, because an admitted stream never ends:
/// a ceiling that stopped refusing has to **fail** this leg on the status, not hang it on
/// a body that keeps arriving.
fn refused_stream(port: u16) -> (u16, String) {
    let (mut sse, _) = Sse::open(port);
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut body = String::new();
    while Instant::now() < deadline {
        match sse.line(deadline) {
            Some(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            None => break,
        }
    }
    (sse.status, body)
}

// --- the stub daemon ------------------------------------------------------------------

/// The relay spawned as the **test's own child**, against a hand-built daemon.
///
/// It carries a `Drop` that ends and reaps the child, so a failed assertion cannot leak a
/// process still holding a port.
struct Stub {
    dir: TempDir,
    listener: UnixListener,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
}

impl Stub {
    /// Bind a socket, spawn the relay against it, and send it `hello(socket)` — which is a
    /// closure so a leg can hand it a malformed line or an impossible setting.
    fn spawn(hello: impl FnOnce(&Path) -> String) -> Stub {
        let dir = TempDir::new().expect("tempdir");
        let socket = dir.path().join("sock");
        let listener = UnixListener::bind(&socket).expect("bind the stub's control socket");
        listener
            .set_nonblocking(true)
            .expect("a bounded accept, never a blocking one");

        let stderr = std::fs::File::create(dir.path().join("stderr.log")).expect("stderr log");
        let mut cmd = Command::new(python3_path());
        clear_layout_env(&mut cmd)
            .arg(plugin_root().join("web-top"))
            .current_dir(plugin_root())
            .env("HOME", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        let mut child = cmd.spawn().expect("spawn the shipped relay");

        // stdout is pumped into a channel, so every read of it is bounded by a recv
        // timeout rather than by the child's willingness to speak.
        let out = child.stdout.take().expect("piped stdout");
        let (tx, stdout) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });

        let mut stub = Stub {
            stdin: child.stdin.take(),
            child: Some(child),
            stdout,
            listener,
            dir,
        };
        let line = hello(&socket);
        stub.write_stdin(&line);
        stub
    }

    /// The ordinary `hello`: proto 1, this plugin's name, the stub's socket, `settings`.
    fn spawn_with(settings: serde_json::Value) -> Stub {
        Stub::spawn(|socket| {
            serde_json::json!({
                "call": "hello",
                "proto": 1,
                "name": NAME,
                "socket": socket,
                "settings": settings,
            })
            .to_string()
        })
    }

    fn write_stdin(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("the child's stdin is open");
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .expect("write to the child's stdin");
        stdin.flush().expect("flush the child's stdin");
    }

    /// The next line the child wrote to stdout, or `None` at its deadline — a child that
    /// went quiet **and** one that died both read as "nothing came".
    fn stdout_line(&self, budget: Duration) -> Option<String> {
        self.stdout.recv_timeout(budget).ok()
    }

    /// Read and check the `hello` reply, which is the only protocol the pipe carries.
    fn expect_hello_reply(&self) {
        let line = self
            .stdout_line(BUDGET)
            .unwrap_or_else(|| panic!("the relay never answered the `hello`:\n{}", self.stderr()));
        let reply: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("the reply is not JSON ({e}): {line}"));
        assert_eq!(
            reply,
            serde_json::json!({"ok": true, "proto": 1}),
            "the `hello` reply is the one the protocol fixes"
        );
    }

    /// The port the relay announced on its serving line.
    fn serving_port(&self) -> u16 {
        let line = self
            .stdout_line(BUDGET)
            .unwrap_or_else(|| panic!("the relay never announced a port:\n{}", self.stderr()));
        serving_port(&line).unwrap_or_else(|| panic!("no port on the serving line: {line}"))
    }

    /// Accept one attach, bounded, and answer its `hello` with `reply`.
    fn handshake(&self, reply: serde_json::Value) -> Wire {
        let deadline = Instant::now() + BUDGET;
        let mut wire = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break Wire::new(stream),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("the relay never attached ({e}):\n{}", self.stderr()),
            }
        };
        let hello = wire.recv();
        assert_eq!(
            hello,
            serde_json::json!({"afkd": "hello", "proto": 1}),
            "every subscriber's attach opens with the handshake the wire fixes"
        );
        wire.send(&reply);
        wire
    }

    /// Everything the child has written to stderr so far.
    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("stderr.log")).unwrap_or_default()
    }

    /// The child's exit code, or a panic if it outlives `budget` — a hang must fail.
    fn exit_code(&mut self, budget: Duration) -> i32 {
        let child = self.child.as_mut().expect("the child is not reaped yet");
        let deadline = Instant::now() + budget;
        loop {
            match child.try_wait().expect("wait on the child") {
                Some(status) => {
                    self.child.take();
                    return status.code().unwrap_or_else(|| {
                        panic!("the relay was signalled, not exited:\n{}", self.stderr())
                    });
                }
                None if Instant::now() >= deadline => {
                    panic!(
                        "the relay is still running after {budget:?}:\n{}",
                        self.stderr()
                    )
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The stub's side of one attach: one reader, one writer, both bounded.
struct Wire {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Wire {
    fn new(stream: UnixStream) -> Wire {
        stream
            .set_read_timeout(Some(BUDGET))
            .expect("a bounded read on the stub's wire");
        let writer = stream.try_clone().expect("clone the stub's wire");
        Wire {
            reader: BufReader::new(stream),
            writer,
        }
    }

    fn recv(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).expect("read a frame");
        assert!(read > 0, "the relay closed the wire mid-conversation");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("a frame is not JSON ({e}): {line}"))
    }

    fn send(&mut self, frame: &serde_json::Value) {
        self.send_raw(&frame.to_string());
    }

    fn send_raw(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .expect("write a frame");
        self.writer.flush().expect("flush the wire");
    }
}

// --- the legs --------------------------------------------------------------------------

#[test]
fn the_verbs_accept_the_shipped_manifest() {
    // AC1, in both directions. `install` places the tree, `plugins` reads the manifest
    // back, `validate` takes a block naming every declared setting and refuses one it does
    // not declare, and
    // `doctor` is the surface that sees the plugin: silent and **green** while the manifest
    // is sound, and the thing that names the file the moment it is not.
    //
    // A companion declares no host requirements — `plugins.rs` says so where it builds the
    // kind ("`[config]` has no `requires` twin, so `doctor` does not probe a companion's
    // tools") — so a companion row in a green `doctor` would mean editing a crate, which
    // this card forbids. Ungated on python: installing runs nothing, which is the same
    // claim as "no toolchain on the host".
    let dir = dir_with_config(SERVICES);
    let (out, err, code) = install_relay(dir.path());
    assert_eq!(code, Some(0), "{out}{err}");

    let version = manifest_version();
    assert!(
        out.contains(&format!("Installed {NAME} {version} (companion)")),
        "the report names the scoped spelling, the version and the shape: {out}"
    );

    let listed = run_subcommand_args(dir.path(), &["plugins"]);
    let listing = String::from_utf8_lossy(&listed.stdout).into_owned();
    let row = listing
        .lines()
        .find(|line| line.split_whitespace().next() == Some(NAME))
        .unwrap_or_else(|| panic!("`afkd plugins` never listed {NAME}:\n{listing}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        cells.get(1).copied(),
        Some(version.as_str()),
        "the listing reads the manifest's version back: {row}"
    );
    assert_eq!(
        cells.get(2).copied(),
        Some("companion"),
        "…under the shape the daemon runs: {row}"
    );

    // The tree is placed whole, two levels down, with the program still executable — and
    // there is no build step, so what is placed is what runs.
    let placed = plugins_root(dir.path()).join("@afkd").join("web-top");
    for leaf in [
        "afkd-plugin.toml",
        "README.md",
        "index.html",
        "top.mjs",
        // The fold, the keymap, the layout, the session, the key seam, the painter, the
        // stylesheet, the recorded captures and the golden screens all travel with the
        // plugin: a shipped file nothing asserts is installed can silently stop shipping,
        // and a page whose `import` lands on a 404 is a blank screen.
        "fold.mjs",
        "keymap.mjs",
        "layout.mjs",
        "session.mjs",
        "input.mjs",
        "paint.mjs",
        "dashboard.css",
        "fixtures/snapshot.jsonl",
        "goldens/overview-100x30.txt",
    ] {
        assert!(placed.join(leaf).is_file(), "the install placed {leaf}");
    }
    let exec = placed.join("web-top");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&exec)
        .expect("stat the program")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o111,
        0o111,
        "the executable bit survives the install"
    );

    // Every declared setting validates, and a key the manifest does not declare does not.
    let full = config_with_block(
        "  port 8771\n  bind \"127.0.0.1\"\n  max_clients 4\n  runs_dir \"/tmp/afkd-runs\"\n  log_lines 500\n",
    );
    std::fs::write(main_conf(dir.path()), &full).expect("write the config");
    let ok = run_subcommand_args(dir.path(), &["validate"]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "every declared setting validates: {}{}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(
        run_subcommand_args(dir.path(), &["doctor"]).status.code(),
        Some(0),
        "a sound manifest leaves `doctor` green"
    );

    std::fs::write(main_conf(dir.path()), config_with_block("  prot 8771\n"))
        .expect("write the config");
    let typo = run_subcommand_args(dir.path(), &["validate"]);
    let report =
        String::from_utf8_lossy(&typo.stdout).into_owned() + &String::from_utf8_lossy(&typo.stderr);
    assert_eq!(typo.status.code(), Some(1), "a typo'd key is a refusal");
    assert!(
        report.contains("`prot`") && report.contains("`port`"),
        "…naming the key it did not know and the one it meant: {report}"
    );

    // …and the other half of the doctor clause: skew the **placed** manifest's `proto` and
    // `doctor` is the surface that names the file. That is the whole of what `doctor` can
    // say about a companion today, and it is a positive claim rather than a shed one.
    std::fs::write(main_conf(dir.path()), &full).expect("write the config");
    let manifest = placed.join("afkd-plugin.toml");
    let skewed = std::fs::read_to_string(&manifest)
        .expect("the placed manifest")
        .replace("proto = 1", "proto = 99");
    std::fs::write(&manifest, skewed).expect("skew the placed manifest");
    let broken = run_subcommand_args(dir.path(), &["doctor"]);
    let report = String::from_utf8_lossy(&broken.stdout).into_owned()
        + &String::from_utf8_lossy(&broken.stderr);
    assert_eq!(broken.status.code(), Some(1), "a skewed manifest is a red");
    assert!(
        report.contains(&manifest.display().to_string()) && report.contains("speaks 1"),
        "…naming the manifest and both versions: {report}"
    );
}

// --- the published index ---------------------------------------------------------------

/// The URL afkd's index lists the relay at — the release this repository's workflow cuts
/// onto the fixed `web-top` tag.
///
/// The tag is fixed on purpose: `afkd update` re-fetches the source it **recorded** at
/// install time and never re-reads the index, so a moving tag behind one address is what
/// lets an already-installed operator reach the next version at all.
const RELEASE_URL: &str =
    "https://github.com/afkd-sh/afkd-plugins/releases/download/web-top/afkd-web-top.tar.gz";

/// The name the shipped manifest declares — **parsed**, like [`manifest_version`], so the
/// index-key assertion below compares two files rather than one file and a literal.
fn manifest_name() -> String {
    let manifest =
        std::fs::read_to_string(plugin_root().join("afkd-plugin.toml")).expect("the manifest");
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("name = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("the manifest declares a name")
}

/// The index afkd serves at <https://afkd.sh/plugins.json>, as bytes: `web/public/plugins.json`
/// in the checkout [`afkd_src`] names, or `None` without one.
fn shipped_index() -> Option<String> {
    let path = afkd_src()?.join("web/public/plugins.json");
    let bytes = std::fs::read_to_string(&path);
    Some(bytes.unwrap_or_else(|err| panic!("the index at {}: {err}", path.display())))
}

/// Whether `tar` is on `PATH` — the [`python3_available`] idiom, so a `tar`-less host skips
/// loudly. afkd's own extraction is proved hermetically in its `crates/app/src/fetch.rs`;
/// `tar` here only *builds* the fixture, the way `.github/workflows/release.yml` builds the
/// real one.
fn tar_available() -> bool {
    Command::new("tar")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Copy `src` onto `dst` recursively. `std::fs::copy` carries the source's mode on Unix, so
/// `web-top`'s executable bit rides into the archive exactly as the workflow's `cp -a` does.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("mk dst");
    for entry in std::fs::read_dir(src).expect("read src") {
        let entry = entry.expect("a directory entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).expect("copy");
        }
    }
}

/// Pack the **shipped** plugin tree the way `.github/workflows/release.yml` does: one
/// top-level `afkd-web-top/` directory and nothing else, mode bits carried.
///
/// `version` rewrites the packed manifest's version line, which is how the update leg gets
/// a genuinely newer artifact at the same address without a second server.
fn pack_release(at: &Path, version: Option<&str>) -> Vec<u8> {
    let staged = at.join(format!("pack-{}", version.unwrap_or("shipped")));
    let wrapper = staged.join("afkd-web-top");
    copy_tree(&plugin_root(), &wrapper);
    if let Some(version) = version {
        let manifest = wrapper.join("afkd-plugin.toml");
        let bumped = std::fs::read_to_string(&manifest)
            .expect("the packed manifest")
            .replace(
                &format!("version = \"{}\"", manifest_version()),
                &format!("version = \"{version}\""),
            );
        std::fs::write(&manifest, bumped).expect("bump the packed manifest");
    }
    let packed = Command::new("tar")
        .current_dir(&staged)
        .args(["czf", "afkd-web-top.tar.gz", "afkd-web-top"])
        .status()
        .expect("run tar");
    assert!(packed.success(), "tar refused to pack the shipped tree");
    std::fs::read(staged.join("afkd-web-top.tar.gz")).expect("read the archive")
}

/// A loopback server that keeps answering `/<leaf>` with **whatever bytes it holds now**,
/// counting every request that reaches it.
///
/// `common::IndexServer`'s byte twin, and the reason a one-shot server cannot stand in:
/// this suite installs and then *updates* through one address, which is two
/// fetches, and the update leg's whole claim is that the second one finds a newer artifact
/// at the **same** URL. Stopped by its own [`Drop`], so no thread outlives the test.
struct TarballServer {
    /// The URL an index entry points at.
    url: String,
    /// The port, so [`Drop`] can wake the accept.
    port: u16,
    /// The body served next — swapped by [`TarballServer::publish`].
    body: Arc<Mutex<Vec<u8>>>,
    /// Requests that reached the listener.
    hits: Arc<AtomicUsize>,
    /// Set by [`Drop`] before the wake-up connection.
    stop: Arc<AtomicBool>,
    /// The accept loop.
    server: Option<std::thread::JoinHandle<()>>,
}

impl TarballServer {
    /// Replace the artifact this address serves — the forge moving the fixed tag.
    fn publish(&self, body: Vec<u8>) {
        *self.body.lock().expect("the served body") = body;
    }

    /// How many requests have arrived so far.
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for TarballServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // `accept` blocks, so the flag alone would never be read. One connection wakes it.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Serve `body` as `/<leaf>` on loopback, as the forge serves a release asset.
fn serve_tarball(leaf: &str, body: Vec<u8>) -> TarballServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let body = Arc::new(Mutex::new(body));
    let hits = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (payload, counter, flag) = (Arc::clone(&body), Arc::clone(&hits), Arc::clone(&stop));
    let server = std::thread::spawn(move || {
        while let Ok((mut socket, _)) = listener.accept() {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request);
            let served = payload.lock().expect("the served body").clone();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                served.len()
            );
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(&served);
            let _ = socket.flush();
        }
    });
    TarballServer {
        url: format!("http://127.0.0.1:{port}/{leaf}"),
        port,
        body,
        hits,
        stop,
        server: Some(server),
    }
}

/// The index body listing the relay at `url` — the shipped file's shape with the address
/// swapped for a loopback one.
fn index_listing(url: &str) -> String {
    format!("{{\"index\": 1, \"plugins\": {{\"{NAME}\": {{\"source\": \"{url}\"}}}}}}")
}

#[test]
fn the_published_index_entry_matches_the_shipped_plugin() {
    // afkd never cross-checks an index key against the manifest it fetches, so a key that
    // drifts from the declared name installs under one spelling and lists under another —
    // a fault an operator discovers and no parse catches. Caught here instead, over the two
    // files that would drift: the index afkd deploys and the manifest this repository ships.
    let Some(shipped) = shipped_index() else {
        eprintln!("SKIP: AFKD_SRC names no afkd checkout, and the index is afkd's");
        return;
    };
    let index: serde_json::Value =
        serde_json::from_str(&shipped).expect("the shipped index is JSON");
    let plugins = index["plugins"]
        .as_object()
        .expect("the index has a `plugins` object");

    let key = plugins
        .keys()
        .find(|key| key.as_str() == NAME)
        .unwrap_or_else(|| panic!("the shipped index does not list {NAME}: {plugins:?}"));
    assert_eq!(
        key.as_str(),
        manifest_name(),
        "the index key and the manifest's declared name have drifted apart"
    );
    assert_eq!(
        plugins[NAME]["source"].as_str(),
        Some(RELEASE_URL),
        "the entry does not point at this repository's release asset"
    );
    // The `.tar.gz` suffix is not decoration: `PluginSource::classify` routes on it, and a
    // release URL without it would be taken for a git remote and cloned.
    assert!(
        RELEASE_URL.ends_with(".tar.gz"),
        "the release URL is not classified as a tarball"
    );
}

#[test]
fn installs_by_name_through_the_published_index_and_updates_from_the_same_url() {
    // The publishing claim end to end, with nothing of this repo reachable but the two
    // things a stranger's host can reach: an index, and a tarball at the address it names.
    //
    // The fixture is the **actual shipped relay** — 72 files, a 179 KB `layout.mjs`, 27
    // golden screens, a recorded run corpus two directories deep with a CJK segment in its
    // path, a `@scope/leaf` name whose `@` is in every archived path, and a program with a
    // shebang and an executable bit. That is the adversarial input for a tarball round-trip;
    // a two-file placeholder would prove none of it.
    if !tar_available() {
        eprintln!("SKIP: tar is not on PATH");
        return;
    }
    let dir = dir_with_config(SERVICES);
    let packing = TempDir::new().expect("a packing dir");
    let tarball = serve_tarball("afkd-web-top.tar.gz", pack_release(packing.path(), None));
    let index = serve_index(&index_listing(&tarball.url));
    let pointed = [("AFKD_PLUGIN_INDEX_URL", index.url.as_str())];

    let out = run_subcommand_args_with_env(dir.path(), &["install", NAME], &pointed);
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    let fault = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{report}{fault}");

    let version = manifest_version();
    assert!(
        report.contains(&format!("Installed {NAME} {version} (companion)")),
        "the report names the scoped spelling, the version and the shape: {report}"
    );
    assert_eq!(
        index.hits(),
        1,
        "a bare name resolves through the index once"
    );
    assert_eq!(tarball.hits(), 1, "…and fetches the asset it named");

    // The tree is placed whole, under the scope, with the archive's own wrapper stepped
    // through rather than planted — and the program still executable, since there is no
    // build step and what is placed is what the daemon runs.
    let placed = plugins_root(dir.path()).join("@afkd").join("web-top");
    for leaf in [
        "afkd-plugin.toml",
        "web-top",
        "index.html",
        "top.mjs",
        "layout.mjs",
        "session.mjs",
        "fixtures/snapshot.jsonl",
        "goldens/overview-100x30.txt",
    ] {
        assert!(
            placed.join(leaf).is_file(),
            "the install placed {leaf} out of the archive"
        );
    }
    assert!(
        !placed.join("afkd-web-top").exists(),
        "the archive's one wrapper directory is stepped through, not placed"
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("stat the program")
            .permissions()
            .mode()
    };
    assert_eq!(
        mode(&placed.join("web-top")) & 0o111,
        0o111,
        "the executable bit survives the tarball round-trip"
    );

    let listed = |args: &[&str]| -> Vec<String> {
        let out = run_subcommand_args_with_env(dir.path(), args, &pointed);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|line| line.split_whitespace().next() == Some(NAME))
            .map(|line| line.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| panic!("`afkd {args:?}` never listed {NAME}"))
    };
    let row = listed(&["plugins"]);
    assert_eq!(row.get(1).map(String::as_str), Some(version.as_str()));
    assert_eq!(row.get(2).map(String::as_str), Some("companion"));
    assert_eq!(
        row.last().map(String::as_str),
        Some(tarball.url.as_str()),
        "the recorded source is the URL the index gave, verbatim: {row:?}"
    );
    // …and in the placed manifest's own bytes, which is what `update` re-resolves later.
    // Read directly rather than only through the column, since the column is a rendering
    // of it and the two could not both be wrong in the same direction by accident.
    let recorded = std::fs::read_to_string(placed.join("afkd-plugin.toml")).expect("the manifest");
    assert!(
        recorded.contains(&format!("source = \"{}\"", tarball.url)),
        "the placed manifest does not record the fetched URL verbatim:\n{recorded}"
    );

    // The forge moves the fixed tag onto a newer release. `update` must find it at the
    // address it recorded — and must not go back to the index to do so, which is the whole
    // reason the tag is fixed rather than versioned.
    let newer = next_minor(&version);
    tarball.publish(pack_release(packing.path(), Some(&newer)));
    let out = run_subcommand_args_with_env(dir.path(), &["update", NAME], &pointed);
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    let fault = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{report}{fault}");

    let row = listed(&["plugins"]);
    assert_eq!(
        row.get(1).map(String::as_str),
        Some(newer.as_str()),
        "the update picked up the newer release at the same URL: {row:?}"
    );
    assert_eq!(
        row.last().map(String::as_str),
        Some(tarball.url.as_str()),
        "…and the recorded source is unchanged: {row:?}"
    );
    assert_eq!(
        index.hits(),
        1,
        "`update` re-read the index; it must re-fetch the source it recorded"
    );
    assert_eq!(tarball.hits(), 2, "…which is one more fetch of that URL");
    assert_eq!(
        mode(&placed.join("web-top")) & 0o111,
        0o111,
        "the executable bit survives the update too"
    );
}

#[test]
fn the_daemon_runs_the_relay_and_the_stream_climbs() {
    // AC2. The block says `port 0`, so the port the relay answers on is one it could only
    // have got by reading its own settings. The page comes up, the stream opens with this
    // subscriber's own id and the daemon's welcome, the first forwarded frame is the
    // snapshot — and the count **rises** with nothing fired, because the daemon's
    // once-a-second `host_load` is on the wire. No browser runs in this suite, so the
    // page's own "connected + count" is guarded at its source: the module the page loads is
    // the oracle for what a browser would do with those events.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let (daemon, dir, port) = daemon_serving(&config_with_block("  port 0\n"));

    let (status, head, page) = get(port, "/");
    assert_eq!(status, 200, "GET /: {page}");
    assert!(
        head.contains("Content-Type: text/html"),
        "the page is served as HTML: {head}"
    );
    assert!(
        page.starts_with("<!doctype html>") && page.contains(r#"src="/top.mjs""#),
        "the page is one self-contained document loading the module beside it: {page}"
    );
    assert!(
        !page.contains("http://") && !page.contains("https://"),
        "no CDN, no bundler: the page fetches nothing off the host"
    );

    let (status, head, module) = get(port, "/top.mjs");
    assert_eq!(status, 200, "GET /top.mjs: {module}");
    assert!(
        head.contains("Content-Type: text/javascript"),
        "a module is served as javascript or the browser refuses it: {head}"
    );
    assert!(
        module.contains(r#"new EventSource("/stream")"#),
        "the module opens the stream this relay serves: {module}"
    );
    for event in ["stream", "welcome", "refused", "closed", "bye"] {
        assert!(
            module.contains(&format!(r#"addEventListener("{event}""#)),
            "the module listens for the `{event}` event the relay deals"
        );
    }

    // The live half.
    let mut sse = Sse::admitted(port);
    sse.handshake();
    let snapshot = sse.frame(BUDGET);
    assert_eq!(
        snapshot["meta"], "snapshot",
        "the first forwarded frame is the attach snapshot: {snapshot:#}"
    );
    let names: Vec<&str> = snapshot["services"]
        .as_array()
        .expect("the snapshot carries a services array")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&BUSY) && names.contains(&"never"),
        "…verbatim, wide glyphs and namespace intact: {names:?}"
    );

    // A count that rises, with nothing fired.
    for nth in 1..=2 {
        let frame = sse.frame(BUDGET);
        assert!(
            frame["type"].is_string(),
            "frame {nth} after the snapshot is a wire frame: {frame:#}"
        );
    }

    daemon.signal(libc::SIGTERM);
    let out = daemon.reap(BUDGET);
    assert_eq!(out.status.code(), Some(0), "a clean drain");
    let log = std::fs::read_to_string(daemon_log(dir.path())).expect("daemon log");
    assert!(
        log.contains(&format!("companion {NAME}: started")),
        "the daemon ran the companion:\n{log}"
    );
    assert!(
        !log.contains(&format!("companion {NAME}: err")),
        "the relay wrote nothing to stderr for the whole run:\n{log}"
    );
    assert!(
        log.contains(&format!("companion {NAME}: stopped (eof)")),
        "…and it went on the EOF, not on the grace expiring:\n{log}"
    );
}

#[test]
fn two_subscribers_each_get_their_own_snapshot_and_the_third_is_refused() {
    // AC3, which is the whole reason this relay holds no model: a second browser is a
    // second **attach**, so it gets its own `meta.snapshot` rather than joining tab one
    // mid-stream. The ceiling is real, it says so in a sentence, and it is released when a
    // subscriber goes — the relay learns that on its next write, which against a live
    // daemon is within the second.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let (_daemon, _dir, port) = daemon_serving(&config_with_block("  port 0\n  max_clients 2\n"));

    let mut first = Sse::admitted(port);
    let id_first = first.handshake();
    assert_eq!(
        first.frame(BUDGET)["meta"],
        "snapshot",
        "the first subscriber is seeded"
    );

    let mut second = Sse::admitted(port);
    let id_second = second.handshake();
    assert_eq!(
        second.frame(BUDGET)["meta"],
        "snapshot",
        "…and so is the second, on its own attach rather than tab one's mid-stream"
    );
    assert_ne!(
        id_first, id_second,
        "two subscribers are two attaches, so they are two ids"
    );

    let (status, body) = refused_stream(port);
    assert_eq!(status, 503, "the third is past the ceiling: {body}");
    assert!(
        body.contains("max_clients") && body.lines().count() == 1,
        "…and it is told so in one sentence naming the setting: {body:?}"
    );

    // The slot comes back when a subscriber goes.
    drop(first);
    let deadline = Instant::now() + BUDGET;
    let mut admitted = loop {
        let (sse, _) = Sse::open(port);
        if sse.status == 200 {
            break sse;
        }
        assert!(
            Instant::now() < deadline,
            "the slot the closed subscriber held was never released"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let id_third = admitted.handshake();
    assert!(
        id_third != id_first && id_third != id_second,
        "the freed slot is a fresh attach, not the dead one's: {id_third}"
    );
    assert_eq!(
        admitted.frame(BUDGET)["meta"],
        "snapshot",
        "…seeded like every other"
    );
}

#[test]
fn a_fire_from_one_tab_arrives_on_every_stream() {
    // AC4. The daemon fans each engine event out to every attached client, so a command
    // posted onto **one** subscriber's attach is seen by both — which is what makes the
    // per-subscriber design safe rather than isolating. Two negatives ride along, because
    // they are claims about what does **not** reach the socket: a verb outside the control
    // wire's vocabulary, and a stream id that names no attach.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let (_daemon, dir, port) = daemon_serving(&config_with_block("  port 0\n"));

    let mut one = Sse::admitted(port);
    let id_one = one.handshake();
    one.frame(BUDGET);
    let mut two = Sse::admitted(port);
    two.handshake();
    two.frame(BUDGET);

    // Neither of these may reach the wire, so both are posted **before** the real fire: the
    // service they name has a file on disk that must still be absent afterwards.
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id_one, "command": "detonate", "service": "never"}),
    );
    assert_eq!(status, 400, "a verb the wire has no command for: {body}");
    assert!(
        body.contains("detonate") && body.contains("restart"),
        "…named, alongside the vocabulary it is not in: {body}"
    );
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": "0123456789abcdef", "command": "fire", "service": "never"}),
    );
    assert_eq!(status, 404, "a stream id that names no attach: {body}");
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id_one, "command": "fire"}),
    );
    assert_eq!(status, 400, "a service verb with no service: {body}");

    // …and the one that must.
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id_one, "command": "fire", "service": BUSY}),
    );
    assert_eq!(status, 200, "POST /command: {body}");

    for (label, sse) in [("the posting tab", &mut one), ("the other tab", &mut two)] {
        let started = sse.frame_matching(BUDGET, |f| f["event"] == "fire_started");
        assert_eq!(
            started["service"], BUSY,
            "{label} sees the fire, wide glyphs and namespace intact: {started:#}"
        );
    }

    // The fire really ran, and the negative control really did not.
    let deadline = Instant::now() + BUDGET;
    while !dir.path().join("fired.txt").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        dir.path().join("fired.txt").exists(),
        "the fire the page posted reached the engine and ran"
    );
    assert!(
        !dir.path().join("never.txt").exists(),
        "a refused command wrote nothing to the socket: `never` ran"
    );
}

#[test]
fn killing_the_daemon_ends_every_stream_and_a_restart_reconnects() {
    // AC6. A daemon that dies takes its companion's stdin with it, which is the relay's
    // stop signal: every open stream ends and the port is freed. The port is **fixed**
    // here, so the second daemon serves the same address the first did — which is also the
    // guard that the orphaned relay really exited, because a lingering one would hold the
    // port and redden the restart.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let port = free_loopback_port();
    let config = config_with_block(&format!("  port {port}\n"));
    let dir = dir_with_config(&config);
    let (out, err, code) = install_relay(dir.path());
    assert_eq!(code, Some(0), "installing the shipped relay: {out}{err}");

    let first = spawn_headless_streaming(dir.path(), &[]);
    assert_eq!(
        relay_port(dir.path()),
        port,
        "the relay bound the port its block named"
    );
    let mut sse = Sse::admitted(port);
    sse.handshake();
    assert_eq!(sse.frame(BUDGET)["meta"], "snapshot", "the stream is live");

    first.signal(libc::SIGKILL);
    let _ = first.reap(BUDGET);
    assert!(
        sse.ends_within(BUDGET),
        "an open stream outlived the daemon that fed it"
    );
    let deadline = Instant::now() + BUDGET;
    while TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(
            Instant::now() < deadline,
            "something is still accepting on 127.0.0.1:{port}; the relay outlived its daemon"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The crash released the run lock, so a second daemon comes up over the same home and
    // starts its own relay on the same address. Nothing reconnects by itself: the daemon
    // restarting the relay and the page reopening its stream are the two moving parts.
    std::fs::remove_file(daemon_log(dir.path())).expect("clear the daemon log");
    let second = spawn_headless_streaming(dir.path(), &[]);
    assert_eq!(
        relay_port(dir.path()),
        port,
        "the second daemon's relay took the same address"
    );
    let mut reopened = Sse::admitted(port);
    reopened.handshake();
    assert_eq!(
        reopened.frame(BUDGET)["meta"],
        "snapshot",
        "a reopened stream is seeded like any other"
    );
    second.signal(libc::SIGTERM);
    let _ = second.reap(BUDGET);
}

#[test]
fn the_static_allowlist_serves_three_extensions_and_nothing_else() {
    // Not an acceptance criterion, and it needs its own oracle anyway: this process runs
    // unconfined with the daemon's privileges, so what a browser can read out of its
    // directory is a security-shaped claim. The allowlist is by extension **and** by
    // segment — a nested path, a traversal, a percent-encoded traversal, the manifest, the
    // README and the program itself are each a 404 with a sentence.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let (_daemon, dir, port) = daemon_serving(&config_with_block("  port 0\n"));

    // A file with an **allowlisted extension** outside the plugin directory, placed where
    // a traversal would really reach it: the tree is installed at
    // `<home>/.config/afkd/plugins/@afkd/web-top`, so five `..` land on `<home>`. Without
    // it the traversals below are refused by the extension check alone and the
    // one-segment rule is never exercised.
    const SECRET: &str = "<!doctype html>the operator's own files";
    std::fs::write(dir.path().join("secret.html"), SECRET).expect("plant the file");

    for (path, kind) in [
        ("/", "text/html"),
        ("/index.html", "text/html"),
        ("/top.mjs", "text/javascript"),
        ("/fold.mjs", "text/javascript"),
        ("/keymap.mjs", "text/javascript"),
        ("/layout.mjs", "text/javascript"),
        ("/session.mjs", "text/javascript"),
        ("/input.mjs", "text/javascript"),
        ("/paint.mjs", "text/javascript"),
        ("/dashboard.css", "text/css"),
    ] {
        let (status, head, body) = get(port, path);
        assert_eq!(status, 200, "GET {path}: {body}");
        assert!(
            head.contains(kind),
            "GET {path} is served as {kind}: {head}"
        );
    }

    // A name past `NAME_MAX`, built rather than spelled: it reaches `open()` like any other
    // miss and has to come back reading the same.
    let too_long = format!("/{}.html", "a".repeat(5000));

    for path in [
        "/README.md",
        "/afkd-plugin.toml",
        "/web-top",
        "/sub/dir.mjs",
        "/../../etc/passwd",
        "/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/../../../../../secret.html",
        "/%2e%2e%2f%2e%2e%2f%2e%2e%2f%2e%2e%2f%2e%2e%2fsecret.html",
        "/.hidden.html",
        "/absent.html",
        // The three shapes that are refused by what `open()` would *raise*, not by what it
        // would find. An embedded NUL raises `ValueError`, which no `except OSError` catches:
        // unguarded it answers nothing at all and writes a traceback to the stderr the daemon
        // copies into its log — so it passes every check above (one segment, no leading dot,
        // an allowlisted extension) and has to be refused by name.
        "/%00.html",
        "/index%00.html",
        // A malformed escape `unquote` leaves standing, and a name no filesystem will take.
        "/%zz.html",
        too_long.as_str(),
    ] {
        let (status, _, body) = get(port, path);
        assert_eq!(status, 404, "GET {path} must not be served: {body}");
        assert_eq!(
            body.lines().count(),
            1,
            "…and the refusal is one sentence: {body:?}"
        );
        assert!(!body.contains("Traceback"), "…never a traceback: {body}");
    }
    // The negative control for the whole leg: a traversal must not merely 404, it must not
    // have read anything either — and the file it aimed at is one this test knows is there,
    // with an extension the allowlist does admit, so only the one-segment rule stands
    // between the two.
    for path in [
        "/../../../../../secret.html",
        "/%2e%2e%2f%2e%2e%2f%2e%2e%2f%2e%2e%2f%2e%2e%2fsecret.html",
    ] {
        let (_, _, body) = get(port, path);
        assert!(
            !body.contains("the operator's own files"),
            "GET {path} served a file outside the plugin directory: {body}"
        );
    }
    // The other half of every refusal above: the browser is told, and the **operator** is
    // not. The relay's stderr is the daemon's log, so an unhandled exception on this route
    // is a traceback any client that can reach the port can pump into it at will.
    let log = wait_for_daemon_log(
        dir.path(),
        &format!("companion {NAME}: err"),
        Duration::from_secs(2),
    );
    assert!(
        !log.contains(&format!("companion {NAME}: err")),
        "a refused path wrote to the relay's stderr:\n{log}"
    );
}

#[test]
fn a_bad_setting_is_one_sentence_and_exit_one() {
    // AC5, widened past `port` because the card asks for every setting to be range-checked.
    // A start failure has to be a **sentence and an exit**, never a traceback and never a
    // companion that came up wrong: the daemon's ladder parks a plugin that keeps failing,
    // and the operator's only view of why is the line this writes. The stub is the oracle
    // because a real daemon refuses these at `validate` and never spawns the child.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    for (settings, key) in [
        (serde_json::json!({"port": "eight"}), "port"),
        (serde_json::json!({"port": "70000"}), "port"),
        (serde_json::json!({"port": true}), "port"),
        (serde_json::json!({"max_clients": "0"}), "max_clients"),
        (serde_json::json!({"max_clients": "65"}), "max_clients"),
        (serde_json::json!({"bind": ""}), "bind"),
        (serde_json::json!({"runs_dir": "runs"}), "runs_dir"),
        (serde_json::json!({"log_lines": "0"}), "log_lines"),
        (serde_json::json!({"log_lines": "100001"}), "log_lines"),
        (serde_json::json!({"log_lines": "lots"}), "log_lines"),
    ] {
        let mut stub = Stub::spawn_with(settings.clone());
        assert_eq!(
            stub.exit_code(BUDGET),
            1,
            "{settings} must be a start failure:\n{}",
            stub.stderr()
        );
        let stderr = stub.stderr();
        assert_eq!(
            stderr.lines().count(),
            1,
            "{settings} is **one** sentence: {stderr:?}"
        );
        assert!(
            stderr.contains(&format!("`{key}`")) && !stderr.contains("Traceback"),
            "{settings} names the key and is not a traceback: {stderr}"
        );
        assert!(
            stub.stdout_line(Duration::from_millis(200)).is_none(),
            "{settings} must not answer the `hello` ok; the daemon has to see a start failure"
        );
    }

    // …and the two ways the `hello` itself can be wrong, before a setting is even read.
    let mut junk = Stub::spawn(|_| "this is not json".to_string());
    assert_eq!(junk.exit_code(BUDGET), 1, "{}", junk.stderr());
    assert!(
        junk.stderr().contains("not JSON") && !junk.stderr().contains("Traceback"),
        "a malformed hello is a sentence: {}",
        junk.stderr()
    );
    let mut skewed = Stub::spawn(|socket| {
        serde_json::json!({"call": "hello", "proto": 9, "name": NAME, "socket": socket}).to_string()
    });
    assert_eq!(skewed.exit_code(BUDGET), 1, "{}", skewed.stderr());
    let stderr = skewed.stderr();
    assert!(
        stderr.contains("skew") && stderr.contains("v1") && stderr.contains("v9"),
        "a plugin-protocol skew names both versions: {stderr}"
    );
}

#[test]
fn a_rejected_handshake_and_an_over_cap_line_each_end_one_stream() {
    // Two things a conforming daemon cannot show, so the stub is the only honest oracle:
    // it never rejects a correct hello, and it never writes a line past the 64 KiB cap both
    // ends hold. Both are per-**subscriber** faults — they end that one stream with its own
    // named event and leave the relay serving, which is the posture a relay with no model
    // can afford and a folding client cannot.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }

    // (a) A refusal crosses verbatim and closes the stream.
    let refusal = "this endpoint is password-gated and your hello carried none";
    let stub = Stub::spawn_with(serde_json::json!({"port": "0"}));
    stub.expect_hello_reply();
    let port = stub.serving_port();
    let mut sse = Sse::admitted(port);
    let (event, payload) = sse.named(BUDGET);
    assert_eq!(event, "stream", "the id comes first: {payload}");
    let _wire = stub.handshake(serde_json::json!({
        "afkd": "reject", "proto": 1, "message": refusal
    }));
    let (event, payload) = sse.named(BUDGET);
    assert_eq!(
        event, "refused",
        "a rejected attach is named as one: {payload}"
    );
    assert_eq!(
        payload["message"], refusal,
        "the daemon's own sentence crosses verbatim: {payload}"
    );
    assert!(
        sse.ends_within(BUDGET),
        "a refused stream is closed, not left half-open"
    );

    // (b) A line past the cap ends that stream with `error` — and is **not** forwarded, no
    //     matter how the bytes were chunked on the way in. The terminator arrives in the
    //     same write as the body here on purpose: a cap judged per `recv` rather than per
    //     line lets exactly this through.
    let stub = Stub::spawn_with(serde_json::json!({"port": "0"}));
    stub.expect_hello_reply();
    let port = stub.serving_port();
    let mut sse = Sse::admitted(port);
    sse.named(BUDGET);
    let mut wire = stub.handshake(serde_json::json!({
        "afkd": "welcome", "proto": 1, "daemon": "0.0.0-stub", "auth": "none"
    }));
    let (event, _) = sse.named(BUDGET);
    assert_eq!(event, "welcome");
    let huge = format!(
        r#"{{"type":"log","service":"{BUSY}","stream":"stdout","line":"{}"}}"#,
        "ラ".repeat(30_000)
    );
    assert!(huge.len() > 64 * 1024, "the fixture is really over the cap");
    wire.send_raw(&huge);
    let (event, payload) = sse.named(BUDGET);
    assert_eq!(
        event, "error",
        "an over-cap line is named as one: {payload}"
    );
    assert!(
        payload["message"]
            .as_str()
            .is_some_and(|m| m.contains("65536")),
        "…quoting the cap it broke: {payload}"
    );
    assert!(
        sse.ends_within(BUDGET),
        "…and that one stream ends rather than resyncing mid-frame"
    );
    assert!(
        stub.stderr().is_empty(),
        "nothing goes to stderr: the browser is the party that needs telling, and a \
         daemon-log line per over-cap frame would be the wrong place: {}",
        stub.stderr()
    );
}

#[test]
fn a_late_attach_is_served_the_finished_run_off_disk_before_any_live_frame() {
    // The card's first and second criteria, against a **real** daemon and its own disk.
    //
    // The daemon serves its history burst only to a TCP client, and a companion is handed the
    // local socket — so without the relay's own read a tab opened after a fire would show two
    // empty panes. This leg fires, waits for the run to finish, opens a **second** subscriber,
    // and asserts its stream carries that run's tree and its `run.log` tail.
    //
    // The ordering half is what a burst injected in the wrong place would fail: the service is
    // fired a second time **immediately** after the attach and before a single frame is
    // drained, so live frames are genuinely racing the disk read. Every backfilled frame must
    // still sit after the `meta.snapshot` and before that fire's first `trace` — and that
    // `trace` is asserted present, so neither half can pass on an empty stream.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    if !node_available() {
        eprintln!("skipping: node is not on PATH, and the render half needs it");
        return;
    }
    let (_daemon, dir, port) = daemon_serving(&config_with_block("  port 0\n"));

    // One fire, run to completion, on a subscriber this leg then leaves open as the sender.
    let mut driver = Sse::admitted(port);
    let driver_id = driver.handshake();
    driver.frame(BUDGET);
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": driver_id, "command": "fire", "service": BUSY}),
    );
    assert_eq!(status, 200, "POST /command: {body}");
    driver.frame_matching(BUDGET, |f| f["event"] == "fire_ok");
    let corpus = runs_root(dir.path()).join("ops__監視");
    assert!(
        corpus.is_dir(),
        "the fire left a run corpus at {}",
        corpus.display()
    );

    // The late attach. Nothing is drained before the second fire is posted, so the daemon's
    // live frames for it queue behind whatever the relay is still writing.
    let mut late = Sse::admitted(port);
    late.handshake();
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": driver_id, "command": "fire", "service": BUSY}),
    );
    assert_eq!(status, 200, "the racing second fire: {body}");

    let mut raw = Vec::new();
    let saw_live = late.collect_frames(BUDGET, &mut raw, |f| f["type"] == "trace");
    assert!(
        saw_live,
        "the second fire's live trace never arrived; the stream carried {raw:?}"
    );
    let frames: Vec<serde_json::Value> = raw
        .iter()
        .map(|line| serde_json::from_str(line).expect("a forwarded frame is JSON"))
        .collect();

    // The snapshot opens the stream, as it does on every attach.
    assert_eq!(
        frames[0]["meta"], "snapshot",
        "the first forwarded frame is still the attach snapshot: {:#}",
        frames[0]
    );

    // The tree: a replayed `opened` for the first fire's own node, naming the service
    // verbatim — wide glyphs and namespace intact through the `::` → `__` dir encoding.
    let opened: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|f| f["type"] == "backfill" && f["event"]["op"] == "opened")
        .collect();
    assert!(
        !opened.is_empty(),
        "the burst replayed the finished run's tree; the stream carried {frames:#?}"
    );
    assert!(
        opened.iter().all(|f| f["service"] == BUSY),
        "…tagged with the service, verbatim: {opened:#?}"
    );
    assert!(
        opened
            .iter()
            .any(|f| f["event"]["node"]["label"] == "echo ライン one"),
        "…including the step the config runs: {opened:#?}"
    );

    // The log: the served `run.log` tail, already sanitized, carrying the fire's own output.
    let served: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|f| f["type"] == "backfill_log")
        .collect();
    assert!(
        served
            .iter()
            .any(|f| f["line"].as_str().is_some_and(|l| l.contains("ライン one"))),
        "the burst served the run's own log tail: {served:#?}"
    );
    let last = served.last().expect("a served tail is not empty");
    assert_eq!(last["last"], true, "the final served line closes the burst");
    assert_eq!(
        served.iter().filter(|f| f["last"] == true).count(),
        1,
        "…and it is the only one that does"
    );
    assert!(
        served
            .iter()
            .all(|f| f["stamp"]["year"].as_i64().is_some_and(|y| y > 2000)),
        "…each carrying the civil stamp its `run.log` line was persisted with, parsed rather \
         than degraded to the zero sentinel: {served:#?}"
    );

    // The ordering, which is the half a burst injected anywhere else would fail.
    let last_backfilled = frames
        .iter()
        .rposition(|f| f["type"] == "backfill" || f["type"] == "backfill_log")
        .expect("the burst is on the stream");
    let first_live = frames
        .iter()
        .position(|f| f["type"] == "trace")
        .expect("the racing fire's trace is on the stream");
    assert!(
        last_backfilled < first_live,
        "every backfilled frame precedes the first live trace ({last_backfilled} vs \
         {first_live}); the stream carried {frames:#?}"
    );

    // …and the page really draws it. The collected JSONL is exactly what a browser's fold
    // would have seen, rendered through the plugin's own modules with `j` onto the service
    // row and `o` into its run view, then `Tab` onto the tree pane.
    let jsonl = dir.path().join("late-attach.jsonl");
    std::fs::write(&jsonl, format!("{}\n", raw.join("\n"))).expect("write the collected frames");
    let log_pane = render_through_plugin(&plugin_root(), &jsonl, "0.0.0", "jo");
    let log_text = (0..usize::from(DRIFT_ROWS))
        .map(|at| log_pane.row_text(at))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        log_text.contains("ライン one"),
        "the run view's log pane is not empty:\n{}",
        log_pane.render()
    );
    let tree_pane = render_through_plugin(&plugin_root(), &jsonl, "0.0.0", "jo\t");
    let tree_text = (0..usize::from(DRIFT_ROWS))
        .map(|at| tree_pane.row_text(at))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tree_text.contains("echo ライン one"),
        "…and neither is its tree pane:\n{}",
        tree_pane.render()
    );
}

#[test]
fn an_unreadable_runs_dir_starts_anyway_says_so_once_and_flashes_every_tab() {
    // The card's third criterion, and the shape of the fault it fixes. A `runs_dir` that is
    // not there is **not** a startup failure: `<state dir>/runs` is made by the daemon the
    // first time a service fires, so a fresh install has none and a relay that refused to
    // come up over it would be worse than one that finds nothing to read.
    //
    // Four halves, and the split between the last two is the whole design: the stderr
    // sentence is a **report**, written once at start, while the verdict is resolved **per
    // burst**. Two tabs therefore see two flashes while the daemon log still holds exactly
    // one `err` line.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let missing = "/nonexistent/afkd-web-top/does-not-exist";
    let (daemon, dir, port) = daemon_serving(&config_with_block(&format!(
        "  port 0\n  runs_dir \"{missing}\"\n"
    )));

    // It came up, and it serves.
    let (status, _, page) = get(port, "/");
    assert_eq!(status, 200, "the relay serves its page anyway: {page}");

    // Both tabs are told, on the stream, in the relay's own voice — which is what the page
    // turns into a flash.
    for tab in 1..=2 {
        let mut sse = Sse::admitted(port);
        sse.handshake();
        let mut raw = Vec::new();
        let told = sse.collect_frames(Duration::from_secs(3), &mut raw, |f| f["meta"] == "error");
        assert!(
            told,
            "tab {tab} was told why its run view is empty; the stream carried {raw:?}"
        );
        let frames: Vec<serde_json::Value> = raw
            .iter()
            .map(|line| serde_json::from_str(line).expect("a forwarded frame is JSON"))
            .collect();
        assert_eq!(frames[0]["meta"], "snapshot", "tab {tab} is seeded first");
        let refusal = frames
            .last()
            .expect("the refusal is the frame collection ended on");
        let message = refusal["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(missing) && message.contains(NAME),
            "tab {tab}'s refusal names the path it tried, in this relay's own voice: {refusal:#}"
        );
        // …and it is a refusal *instead of* a burst, not beside one.
        assert!(
            !frames
                .iter()
                .any(|f| f["type"] == "backfill" || f["type"] == "backfill_log"),
            "tab {tab} was served nothing off a path that is not there: {frames:#?}"
        );

        // The flash the criterion names, read where a flash lives — off the **rendered
        // screen**, through the page's own `noteFrame`, rather than off the frame again.
        if node_available() {
            let jsonl = dir.path().join(format!("refused-{tab}.jsonl"));
            std::fs::write(&jsonl, format!("{}\n", raw.join("\n"))).expect("write the frames");
            let screen = render_through_plugin(&plugin_root(), &jsonl, "0.0.0", "");
            let text = (0..usize::from(DRIFT_ROWS))
                .map(|at| screen.row_text(at))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains(missing),
                "tab {tab}'s page flashes the path it tried:\n{}",
                screen.render()
            );
        }
    }

    daemon.signal(libc::SIGTERM);
    let out = daemon.reap(BUDGET);
    assert_eq!(out.status.code(), Some(0), "a clean drain");
    let log = std::fs::read_to_string(daemon_log(dir.path())).expect("daemon log");
    let sentences: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(&format!("companion {NAME}: err")))
        .collect();
    assert_eq!(
        sentences.len(),
        1,
        "exactly one sentence on stderr for the whole run, not one per attach:\n{log}"
    );
    assert!(
        sentences[0].contains(missing),
        "…and it names the path it tried: {}",
        sentences[0]
    );
    assert!(
        !log.contains("\"type\": \"backfill\""),
        "nothing was backfilled off a path that is not there:\n{log}"
    );
}

#[test]
fn a_runs_dir_that_appears_after_the_daemon_started_is_served() {
    // The anti-latch. `<state dir>/runs` does not exist until a service first fires, so a
    // verdict remembered from start would mean **no backfill ever** on a fresh install, even
    // once the corpus appeared — and would still pass the leg above, which is why this one
    // exists. The base named here is absent when the daemon starts and is filled in before
    // the attach; a subscriber must then be served a real burst.
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH, and the relay is python");
        return;
    }
    let dir = dir_with_config(config_with_block("  port 0\n  runs_dir \"RUNS\"\n").as_str());
    let late = dir.path().join("late-runs");
    let config = std::fs::read_to_string(main_conf(dir.path())).expect("read the config");
    std::fs::write(
        main_conf(dir.path()),
        config.replace("RUNS", &late.display().to_string()),
    )
    .expect("point the block at the late base");
    assert!(!late.exists(), "the base really is absent at start");

    let (out, err, code) = install_relay(dir.path());
    assert_eq!(code, Some(0), "installing the shipped relay: {out}{err}");
    let _daemon = spawn_headless_streaming(dir.path(), &[]);
    let port = relay_port(dir.path());

    // Fire once, so the daemon writes a real run corpus of its own…
    let mut driver = Sse::admitted(port);
    let driver_id = driver.handshake();
    driver.frame(BUDGET);
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": driver_id, "command": "fire", "service": BUSY}),
    );
    assert_eq!(status, 200, "POST /command: {body}");
    driver.frame_matching(BUDGET, |f| f["event"] == "fire_ok");

    // …then put it where the setting points, **after** the relay started.
    std::fs::create_dir_all(&late).expect("mk the late base");
    copy_tree(&runs_root(dir.path()), &late);

    let mut sse = Sse::admitted(port);
    sse.handshake();
    assert_eq!(sse.frame(BUDGET)["meta"], "snapshot", "the tab is seeded");
    let replayed = sse.frame_matching(BUDGET, |f| f["type"] == "backfill" || f["meta"] == "error");
    assert_eq!(
        replayed["type"], "backfill",
        "a base that appeared after start is read, not remembered as missing: {replayed:#}"
    );
    assert_eq!(
        replayed["service"], BUSY,
        "…for the service whose corpus was put there: {replayed:#}"
    );
}

#[test]
fn the_plugin_tree_imports_nothing_outside_the_standard_library() {
    // AC7. The manifest declares no `build` line, so the tree installs on a host with no
    // toolchain — which is only true while every import resolves out of the standard
    // library. The allowlist is written here rather than derived, so a new import is a
    // deliberate edit to this file and not a silent dependency; when python3 is present the
    // interpreter's own `sys.stdlib_module_names` is the real oracle on top.
    const ALLOWED: &[&str] = &[
        "http",
        "json",
        "os",
        "socket",
        "stat",
        "sys",
        "threading",
        "unicodedata",
        "urllib",
    ];

    let mut imported: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(plugin_root()).expect("read the plugin tree") {
        let path = entry.expect("a directory entry").path();
        if !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Matched by shebang, not by extension: the program is `web-top`, with no suffix.
        let is_python = path.extension().is_some_and(|e| e == "py")
            || text.starts_with("#!/usr/bin/env python3");
        if !is_python {
            continue;
        }
        let file = path
            .file_name()
            .expect("a leaf")
            .to_string_lossy()
            .into_owned();
        for line in text.lines() {
            let module = line
                .strip_prefix("import ")
                .or_else(|| line.strip_prefix("from "))
                .map(|rest| rest.split([' ', '.', ',']).next().unwrap_or("").to_string());
            if let Some(module) = module.filter(|m| !m.is_empty()) {
                imported.push((file.clone(), module));
            }
        }
    }
    assert!(
        imported.iter().any(|(file, _)| file == "web-top"),
        "the walk found the relay itself; it collected {imported:?}"
    );
    for (file, module) in &imported {
        assert!(
            ALLOWED.contains(&module.as_str()),
            "{file} imports `{module}`, which is not in this test's allowlist — add it here \
             deliberately, and only if it is standard library"
        );
    }

    if !python3_available() {
        eprintln!("skipping the interpreter's own oracle: python3 is not on PATH");
        return;
    }
    let probe = format!(
        "import sys; print(','.join(m for m in {:?} if m not in sys.stdlib_module_names))",
        ALLOWED
    );
    let out = Command::new(python3_path())
        .args(["-c", &probe])
        .output()
        .expect("run python3");
    let outside = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        outside.is_empty(),
        "this test's allowlist names {outside}, which python does not ship"
    );
}

/// Whether `node` is on PATH — the `python3_available` idiom, so a node-less CI image skips
/// this leg loudly rather than reddening. `node --test` is the only runner the plugin's
/// javascript has, and this is the only place `cargo test` reaches it.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn the_plugins_own_suites_run_under_node() {
    // The plugin's fold, its keymap, its layout, its session, its key seam and its painter are
    // javascript. The release workflow runs these suites too, but on a host with no afkd, so
    // every test in them that reads afkd's Rust is skipped there. Here `cargo test` shells out
    // to `node --test` over each one with this run's environment, `AFKD_SRC` included, and
    // fails on its exit code — so a broken fold, a dashboard that stopped laying out the way
    // its committed golden screens say it does, or a transcription the Rust has moved away
    // from reddens the same run the screen diffs do.
    //
    // All seven are fixture-driven — every expectation comes off the recorded captures under
    // `fixtures/` — so this leg also proves those files are still readable and still hold the
    // variety each test opens by asserting. Two of them reach further still: `keymap.test.mjs`
    // reads afkd's `crates/config/src/keymap.rs` and holds the page's transcription of
    // `DEFAULT_KEYS` to it row for row, so a rebind in the Rust reddens the javascript in the
    // same run; and
    // `backfill.test.mjs` spawns the relay itself (`web-top --backfill …`), so the disk read is
    // exercised as it ships rather than re-stated in javascript.
    if !node_available() {
        eprintln!("skipping: node is not on PATH, and the plugin's suites are javascript");
        return;
    }
    let mut passed = 0u32;
    for name in [
        "backfill.test.mjs",
        "fold.test.mjs",
        "keymap.test.mjs",
        "layout.test.mjs",
        "paint.test.mjs",
        "session.test.mjs",
        "input.test.mjs",
    ] {
        let suite = plugin_root().join(name);
        assert!(suite.is_file(), "{name} ships beside the module it covers");

        let out = Command::new("node")
            .arg("--test")
            .arg(&suite)
            .current_dir(plugin_root())
            .output()
            .expect("run node --test");
        let report = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "`node --test {}` is red:\n{report}",
            suite.display()
        );
        // A runner that found no test file exits 0 with nothing run, which would make this
        // leg a silent pass forever.
        passed += report
            .lines()
            .find_map(|line| line.trim().strip_prefix("# pass "))
            .or_else(|| {
                report
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("ℹ pass "))
            })
            .and_then(|n| n.trim().parse::<u32>().ok())
            .unwrap_or_else(|| panic!("node --test reported no pass count for {name}:\n{report}"));
    }
    // A floor over all seven suites, so it only ever under-counts: a test added to any of them
    // is not a reason to touch this number, and a suite that stopped running is.
    assert!(passed >= 112, "the suites really ran ({passed} tests)");
}

// --- the drift leg: the plugin's screen against the terminal's own ----------------------

/// The grid both screens are captured on. Wide enough that all five list columns render —
/// the list sheds `Last Activity` and `Next Run` on a narrow terminal — and tall enough to
/// hold the whole footer legend, which is one of the things compared.
const DRIFT_COLS: u16 = 100;
const DRIFT_ROWS: u16 = 30;

/// The board the drift leg diffs, chosen for what the comparison has to carry rather than
/// for what a daemon does:
///
/// - `janitor` — a custom `icon` and a cadence, so the `Service` cell opens with a glyph
///   each surface has to measure for itself and the `Trigger`/`Next Run` cells are full;
/// - `ops::監視` — namespaced **and** wide-glyph, so a name either side mis-measures pushes
///   its own row's later cells out of the spans the column header marks out. It is also the
///   one the **run-view** leg fires, which is what its `run` block is shaped for and why the
///   block is what it is: the nested `times` puts a non-last container at depth 1 so its
///   children carry the `│  ` guide rail — a flat tree would let the depth claim pass
///   vacuously — and the failing `seq 1 6` is both the `✗` beside the `✓`s and a leaf whose
///   six tagged lines auto-expand to `BODY_ELIDE` body rows plus a `… 1 more` elision. Every
///   line it prints is sized under [`LOG_MESSAGE_BUDGET`], so neither log pane wraps;
/// - `ops::backup` — the group's second member, so both tree connectors are on screen, and
///   an operator `icon` on a base with no emoji presentation sequence ([`OPERATOR_ICON`]), so
///   a VS16 the terminal ignores is measured by both surfaces;
/// - `never` — `trigger none`, and the one the leg stops, so a second badge (`□ Stopped`)
///   and a `1 down` segment of the header are on the board.
///
/// No `queue` block: with no lanes configured neither surface draws a `Queues` section, so
/// the one part of the overview the plugin diverges on by design is absent by construction
/// rather than suppressed. [`ledger_config`] adds the lane back for the ledger leg
/// ([`web_tops_divergences_from_afkd_top_are_exactly_the_ledgers`]), which holds that
/// divergence to [`LEDGER`].
const DRIFT_SERVICES: &str = r#"service janitor {
  icon "🧹"
  description "sweeps the floor"
  trigger interval { every "1h" }
  run { run_cmd "true" }
}

service ops::監視 {
  description "ウェブ — the wide one"
  trigger interval { every "1h" }
  run {
    times 2 {
      times 1 { run_cmd "echo ライン deep" }
    }
    run_cmd "seq 1 6 && false"
  }
}

service ops::backup {
  icon "★️"
  trigger interval { every "1h" }
  run { run_cmd "true" }
}

service never {
  trigger none
  run { run_cmd "true" }
}
"#;

/// The service the drift leg stops, so the board it compares carries two distinct badges.
const STOPPED: &str = "never";

/// What has to be on a screen before comparing it means anything: the fixture's five rows,
/// two distinct badges among them, and the whole footer legend. They are the floors [`drift`]
/// closes with **and** the condition the terminal's own capture waits for, deliberately the
/// same numbers — a screen caught mid-paint then fails the wait, with itself printed, rather
/// than reading downstream as a row or a hint the plugin is missing.
const SERVICE_ROW_FLOOR: usize = 5;
const BADGE_FLOOR: usize = 2;
const FOOTER_HINT_FLOOR: usize = 12;

/// One reconstructed screen as **columns**: `grid[row][col]` is that cell's contents, and a
/// wide glyph's covered cell is the empty string beside it.
///
/// Columns rather than characters because a char-index slice cannot survive `監視` or `🧹`:
/// the terminal's grid and the plugin's own `textWidth` both put a wide glyph in one cell
/// and leave the next empty, and this is the one shape in which the two are comparable.
struct Screen {
    grid: Vec<Vec<String>>,
}

impl Screen {
    /// Row `at` as one string — for a human reading a failure, and for the blank-row tests
    /// the readers below key off.
    fn row_text(&self, at: usize) -> String {
        self.grid[at].concat()
    }

    /// The index of the column header — the first row whose leading segment is `Service`.
    /// `None` when the screen has no list on it at all, which is a failure with its own
    /// sentence rather than a panic that would print only one of the two screens.
    fn column_header_row(&self) -> Option<usize> {
        (0..self.grid.len()).find(|&at| {
            segments(&self.grid[at])
                .first()
                .is_some_and(|(label, _)| label == "Service")
        })
    }

    /// The column header's labels, each with the screen column it starts at.
    fn column_header(&self) -> Option<Vec<(String, usize)>> {
        self.column_header_row().map(|at| segments(&self.grid[at]))
    }

    /// The service rows: everything between the column header and the first blank row under
    /// it. A blank row is the list's own end on both surfaces — the terminal pads the body
    /// out to the footer, and so does `layout()`.
    fn service_rows(&self) -> Vec<&[String]> {
        let Some(header) = self.column_header_row() else {
            return Vec::new();
        };
        (header + 1..self.grid.len())
            .take_while(|&at| !self.row_text(at).trim().is_empty())
            .map(|at| self.grid[at].as_slice())
            .collect()
    }

    /// The footer's hints, sorted: the maximal trailing run of non-blank rows, each split on
    /// the runs of two-or-more blanks that separate one hint from the next.
    ///
    /// Sorted rather than positional, because the footer's *shelf* — which hint sits in
    /// which of the three rows — is a width shed both surfaces compute for themselves, and
    /// the card scopes this to the hint **set**.
    fn footer_hints(&self) -> Vec<String> {
        let mut hints: Vec<String> = (self.footer_start()..self.grid.len())
            .flat_map(|at| segments(&self.grid[at]).into_iter().map(|(text, _)| text))
            .collect();
        hints.sort();
        hints
    }

    /// The first row of the footer: the start of the maximal trailing run of non-blank rows.
    /// Factored out of [`footer_hints`](Self::footer_hints) rather than open-coded twice, so
    /// the run view's body reader and the hint reader cannot disagree about where the body
    /// stops — a pane that overflowed its viewport would otherwise be swallowed by one and
    /// kept by the other.
    fn footer_start(&self) -> usize {
        let mut at = self.grid.len();
        while at > 0 && !self.row_text(at - 1).trim().is_empty() {
            at -= 1;
        }
        at
    }

    /// The screen column of row `at`'s first ink, or `None` on a blank row.
    ///
    /// Its own reader because it is the one claim [`normalise`] structurally **cannot** make:
    /// that reader splits on whitespace, so every blank before the first token produces no
    /// token at all and a lead-in is invisible downstream of it. Built on the same two
    /// predicates [`segments`] reads ink with — a rendered blank is background, and the empty
    /// covered half of a wide glyph carries no ink of its own — so the two cannot drift about
    /// what "ink" is.
    fn first_ink(&self, at: usize) -> Option<usize> {
        self.grid[at]
            .iter()
            .position(|cell| !is_gap(cell) && !cell.is_empty())
    }

    /// The run view's shown pane title row, **verbatim** but for its right pad — `Tree`, or
    /// `Log · <scope>`. Not read through [`segments`]: a title's parts are single-space
    /// joined, so a collapse would have nothing to do, and the untrimmed head is what would
    /// show a lead-in the title is not supposed to have.
    fn pane_title(&self) -> String {
        self.row_text(PANE_TITLE_ROW).trim_end().to_string()
    }

    /// The run view's shown pane body: the rows between its title band and the footer, with
    /// the pane's own trailing blanks dropped — both surfaces pad an under-filled viewport
    /// out to the footer.
    fn pane_body(&self) -> Vec<&[String]> {
        let mut rows: Vec<&[String]> = (PANE_BODY_ROW..self.footer_start())
            .map(|at| self.grid[at].as_slice())
            .collect();
        while rows
            .last()
            .is_some_and(|row| row.concat().trim().is_empty())
        {
            rows.pop();
        }
        rows
    }

    /// The body rows' last column, concatenated — the one-column scrollbar gutter both panes
    /// reserve (`shell::render_log_pane`/`render_tree_pane`, `layout.mjs`'s `scrollbarOf`).
    /// A pane that overflowed its viewport paints `░`/`█` down it on every row, which would
    /// also make [`footer_start`](Self::footer_start)'s trailing-run scan swallow the pane;
    /// the leg asserts this is blank so that shows up as its own sentence rather than as a
    /// footer with no hints on it.
    fn scrollbar_gutter(&self) -> String {
        (PANE_BODY_ROW..self.footer_start())
            .filter_map(|at| self.grid[at].last().cloned())
            .collect()
    }

    /// The whole screen as numbered rows, for a failure message. Both screens go into every
    /// one: a drift is only readable next to the thing it drifted from.
    fn render(&self) -> String {
        self.grid
            .iter()
            .enumerate()
            .map(|(at, row)| format!("{at:2} |{}|", row.concat().trim_end()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Whether a grid cell is background — a **rendered** blank. The empty string is not one: it
/// is the cell a wide glyph covers, and counting it as a gap would split a name in two.
fn is_gap(cell: &str) -> bool {
    !cell.is_empty() && cell.chars().all(char::is_whitespace)
}

/// A row's ink, split into segments at every run of two-or-more rendered blanks, each paired
/// with the screen column it starts at. One space inside a segment is kept, which is what
/// keeps `Last Activity` and `Ctrl+R reload` whole while `Service`/`State` stay apart.
fn segments(row: &[String]) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut text = String::new();
    let mut gap = 0usize;
    for (at, cell) in row.iter().enumerate() {
        if is_gap(cell) {
            gap += 1;
            if gap >= 2 {
                if let Some(from) = start.take() {
                    out.push((text.trim_end().to_string(), from));
                    text.clear();
                }
            } else if start.is_some() {
                text.push_str(cell);
            }
            continue;
        }
        // The covered half of a wide glyph carries no ink and opens no gap.
        if cell.is_empty() {
            continue;
        }
        gap = 0;
        start.get_or_insert(at);
        text.push_str(cell);
    }
    if let Some(from) = start {
        out.push((text.trim_end().to_string(), from));
    }
    out
}

/// Whether `token` is a duration as both surfaces spell one — `0s`, `59m`, `1h`, `3d`,
/// `120ms`. `format_elapsed` and `formatElapsed` each emit at most two of these side by side
/// (`59m 54s`), so every token normalises on its own.
fn is_duration(token: &str) -> bool {
    let digits = token.chars().take_while(char::is_ascii_digit).count();
    let (head, unit) = token.split_at(digits);
    !head.is_empty() && !unit.is_empty() && unit.chars().all(|c| matches!(c, 'd' | 'h' | 'm' | 's'))
}

/// The placeholder a reading `token` stands in for, `None` when it is not a reading:
/// elapseds and countdowns (`<t>`), the cost (`<cost>`), and the token tally (`<tok>`, known
/// by the `tok` that follows it). [`normalise`] and [`column_cell`] both read through this, so
/// the two cannot drift on what a reading is.
fn placeholder(token: &str, next: Option<&str>) -> Option<&'static str> {
    if is_duration(token) {
        Some("<t>")
    } else if token.starts_with('$') {
        Some("<cost>")
    } else if next == Some("tok") {
        Some("<tok>")
    } else {
        None
    }
}

/// Every reading that moves between the two captures, replaced by a [`placeholder`]:
/// elapseds, countdowns, the token tally and the cost.
///
/// The two screens are seconds apart by construction — one is a live terminal, the other a
/// fold of the frames captured before it attached — so asserting on any of these would make
/// the leg flap rather than gate anything. Whitespace collapses on the way through: this
/// serves the header line, the footer hints and the tree segments, where content is the
/// claim. A column cell keeps its whitespace, in [`column_cell`].
fn normalise(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    tokens
        .iter()
        .enumerate()
        .map(|(at, token)| placeholder(token, tokens.get(at + 1).copied()).unwrap_or(token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The header line reduced to its shape: its segments normalised and rejoined with one
/// marker, so `afkd <version> · ● Running <t> · 3 up · 1 down ‖ <tok> tok · <cost>` is what
/// the two are held to — the counts and the version exactly, the readings not at all.
fn header_shape(row: &[String]) -> String {
    segments(row)
        .into_iter()
        .map(|(text, _)| normalise(&text))
        .collect::<Vec<_>>()
        .join(" ‖ ")
}

/// Each column header label with the screen columns it owns: from its own start to the next
/// label's, the last one to the right edge.
fn column_spans(header: &[(String, usize)], cols: usize) -> Vec<(String, (usize, usize))> {
    header
        .iter()
        .enumerate()
        .map(|(at, (label, start))| {
            let end = header.get(at + 1).map_or(cols, |(_, next)| *next);
            (label.clone(), (*start, end))
        })
        .collect()
}

/// One column's cell off a row as its *content*: the slice its column header's span marks
/// out, trimmed, its internal whitespace collapsed, its readings normalised. The floors and
/// row lookups read this ([`badges`], [`has_countdown`], the `├` and `監視` finds), and so
/// does the ledger's `Queues` compare; an overview cell is *compared* through
/// [`column_cell`].
fn cell_text(row: &[String], (from, to): (usize, usize)) -> String {
    normalise(&row[from.min(row.len())..to.min(row.len())].concat())
}

/// The columns `layout::Column::align` flushes right — the two durations, whose magnitudes
/// stack on one screen column.
const RIGHT_FLUSHED: [&str; 2] = ["Last Activity", "Next Run"];

/// One column's cell as the comparison reads it: the slice **verbatim**, every whitespace
/// run kept — the leading pad, the interior runs and the trailing pad — with only its
/// readings swapped for their [`placeholder`]s. The covered half of a wide glyph is `""` and
/// always follows its glyph, so any shift inside the span moves a pad, and comparing this
/// string is comparing the cell column for column.
///
/// A cell that holds a reading gives up one pad: the one on its **free** side — the leading
/// pad of a [`RIGHT_FLUSHED`] column, the trailing pad of a left-aligned one. That pad is as
/// wide as the reading, and the two captures are seconds apart by construction (`9s` against
/// `12s`), so it would flap; the other edge is the alignment, and it holds still.
fn column_cell(row: &[String], label: &str, (from, to): (usize, usize)) -> String {
    let slice = row[from.min(row.len())..to.min(row.len())].concat();
    let tokens: Vec<&str> = slice.split_whitespace().collect();
    let (mut cell, mut rest, mut read) = (String::new(), slice.as_str(), false);
    for (at, token) in tokens.iter().enumerate() {
        let pad = rest.len() - rest.trim_start().len();
        cell.push_str(&rest[..pad]);
        match placeholder(token, tokens.get(at + 1).copied()) {
            Some(reading) => {
                read = true;
                cell.push_str(reading);
            }
            None => cell.push_str(token),
        }
        rest = &rest[pad + token.len()..];
    }
    cell.push_str(rest);
    match (read, RIGHT_FLUSHED.contains(&label)) {
        (false, _) => cell,
        (true, true) => cell.trim_start().to_string(),
        (true, false) => cell.trim_end().to_string(),
    }
}

/// The distinct badges a screen's service rows carry, deduped — the `State` column's own
/// cells, so a board that never left one state is visible as such.
fn badges(rows: &[&[String]], spans: &[(String, (usize, usize))]) -> Vec<String> {
    let mut seen: Vec<String> = rows
        .iter()
        .map(|row| cell_text(row, span_of(spans, "State")))
        .filter(|badge| !badge.is_empty())
        .collect();
    seen.sort();
    seen.dedup();
    seen
}

/// Whether any service row carries a `Next Run` countdown — the floor that keeps
/// [`column_cell`]'s edge claim from comparing two blank cells and calling them aligned.
/// [`DRIFT_SERVICES`] declares three `every "1h"` services, so one is always counting down.
fn has_countdown(rows: &[&[String]], spans: &[(String, (usize, usize))]) -> bool {
    rows.iter()
        .any(|row| !cell_text(row, span_of(spans, "Next Run")).is_empty())
}

/// The one span of `label`, or a panic: every caller reads a label the comparison already
/// proved is on both screens.
fn span_of(spans: &[(String, (usize, usize))], label: &str) -> (usize, usize) {
    spans
        .iter()
        .find_map(|(name, span)| (name == label).then_some(*span))
        .unwrap_or_else(|| panic!("the column header has no `{label}` column: {spans:?}"))
}

/// Diff the terminal's overview against the plugin's, on the stable fields.
///
/// A `Result` rather than a wall of assertions, because the leg's negative arm has to be
/// able to *demand* a failure and read what it says. Every message carries both whole
/// screens: a drift that only names a cell is not diagnosable.
///
/// The order is the order a reader wants it in — the header, the load strip's identity, the
/// column header, every service row, the footer's hints — and the non-vacuity floors come
/// last, so an honest disagreement is always reported before "there was nothing on screen".
fn drift(top: &Screen, plugin: &Screen) -> Result<(), String> {
    let both = format!(
        "\n\n--- afkd top, {DRIFT_COLS}x{DRIFT_ROWS} ---\n{}\n\n--- {NAME}, {DRIFT_COLS}x{DRIFT_ROWS} ---\n{}",
        top.render(),
        plugin.render()
    );

    let (top_head, plugin_head) = (header_shape(&top.grid[0]), header_shape(&plugin.grid[0]));
    if top_head != plugin_head {
        return Err(format!(
            "the header line differs:\n  afkd top: {top_head}\n  web-top:  {plugin_head}{both}"
        ));
    }

    // The load strip's *identity*, not its readings: the two trend lanes hold different
    // sample counts by construction — the terminal's ring is as old as the daemon, the
    // plugin's as old as the capture — so only the fact that both draw one is a claim.
    for (whose, screen) in [("afkd top", top), ("web-top", plugin)] {
        let strip = screen.row_text(1);
        if !strip.trim_start().starts_with("CPU") {
            return Err(format!(
                "{whose}'s load strip does not open with CPU: {strip:?}{both}"
            ));
        }
    }

    let (Some(top_header), Some(plugin_header)) = (top.column_header(), plugin.column_header())
    else {
        return Err(format!(
            "one of the two screens has no column header on it{both}"
        ));
    };
    if top_header != plugin_header {
        return Err(format!(
            "the column header differs:\n  afkd top: {top_header:?}\n  web-top:  {plugin_header:?}{both}"
        ));
    }
    let spans = column_spans(&top_header, usize::from(DRIFT_COLS));

    let (top_rows, plugin_rows) = (top.service_rows(), plugin.service_rows());
    if top_rows.len() != plugin_rows.len() {
        return Err(format!(
            "afkd top lists {} service rows, web-top {}{both}",
            top_rows.len(),
            plugin_rows.len()
        ));
    }
    for (at, (top_row, plugin_row)) in top_rows.iter().zip(&plugin_rows).enumerate() {
        for (label, span) in &spans {
            let (mine, theirs) = (
                column_cell(top_row, label, *span),
                column_cell(plugin_row, label, *span),
            );
            if mine != theirs {
                return Err(format!(
                    "service row {at}'s `{label}` cell differs:\n  afkd top: {mine:?}\n  web-top:  {theirs:?}{both}"
                ));
            }
        }
    }

    let (top_hints, plugin_hints) = (top.footer_hints(), plugin.footer_hints());
    if top_hints != plugin_hints {
        return Err(format!(
            "the footer's hint set differs:\n  afkd top: {top_hints:?}\n  web-top:  {plugin_hints:?}{both}"
        ));
    }

    // …and the floors, so a leg that captured an empty board cannot agree with an empty
    // renderer and call it parity.
    if top_rows.len() < SERVICE_ROW_FLOOR {
        return Err(format!(
            "only {} service rows were on screen; the fixture ships {SERVICE_ROW_FLOOR}{both}",
            top_rows.len()
        ));
    }
    if badges(&top_rows, &spans).len() < BADGE_FLOOR {
        return Err(format!(
            "the board carried one badge; the fixture stops a service so it carries two{both}"
        ));
    }
    if !top_rows
        .iter()
        .any(|row| cell_text(row, span_of(&spans, "Service")).contains('├'))
    {
        return Err(format!(
            "no group connector was on screen, so the grouped tree was never compared{both}"
        ));
    }
    if !top_rows
        .iter()
        .any(|row| cell_text(row, span_of(&spans, "Service")).contains(STOPPED_ICON))
    {
        return Err(format!(
            "no {STOPPED_ICON} switched-off icon was on screen, so a VS16 glyph's padding was never compared{both}"
        ));
    }
    if !top_rows
        .iter()
        .any(|row| cell_text(row, span_of(&spans, "Service")).contains(OPERATOR_ICON))
    {
        return Err(format!(
            "no {OPERATOR_ICON} operator icon was on screen, so a VS16 on a non-emoji base was never compared{both}"
        ));
    }
    if !has_countdown(&top_rows, &spans) {
        return Err(format!(
            "no `Next Run` countdown was on screen, so its right edge was never compared{both}"
        ));
    }
    if top_hints.len() < FOOTER_HINT_FLOOR {
        return Err(format!(
            "only {} footer hints were on screen{both}",
            top_hints.len()
        ));
    }
    Ok(())
}

/// Poll the terminal until its screen holds everything the comparison needs to mean
/// something — the fixture's rows, both badges, the whole hint legend — and hand that screen
/// back. Gives up at [`BUDGET`] with the screen printed.
///
/// Read through the very readers [`drift`] compares with, and against the very floors it
/// closes on, so this waits for the *thing being asserted* rather than for a string. A
/// half-painted frame, or a flash sitting over a hint row, fails **here** with the terminal's
/// own screen to look at; nothing about the terminal's wording is pinned on the way.
fn settled_terminal(tui: &CapturedTui) -> Screen {
    settled(tui, "a board worth comparing", board_settled)
}

/// [`settled_terminal`]'s condition on its own, so a leg that waits on the board **and**
/// something more can wait on the conjunction rather than on two screens in turn.
fn board_settled(screen: &Screen) -> bool {
    let rows = screen.service_rows();
    screen.column_header().is_some_and(|header| {
        let spans = column_spans(&header, usize::from(DRIFT_COLS));
        rows.len() >= SERVICE_ROW_FLOOR
            && badges(&rows, &spans).len() >= BADGE_FLOOR
            && has_countdown(&rows, &spans)
    }) && screen.footer_hints().len() >= FOOTER_HINT_FLOOR
        // The load strip is drawn only once a `host_load` sample has landed, a
        // second after the attach — so it is waited for as ink on its row rather
        // than asserted as a string here, and [`drift`] says what that ink has to be.
        && !screen.row_text(1).trim().is_empty()
}

/// Poll the terminal until `ready` holds for its screen and hand that screen back, or give
/// up at [`BUDGET`] with the screen printed and `what` it was waiting for named.
fn settled(tui: &CapturedTui, what: &str, ready: impl Fn(&Screen) -> bool) -> Screen {
    let deadline = Instant::now() + BUDGET;
    loop {
        let screen = Screen {
            grid: (0..DRIFT_ROWS).map(|row| tui.row_grid(row)).collect(),
        };
        if ready(&screen) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "the terminal never settled onto {what}:\n{}",
            screen.render()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The node driver that renders a captured JSONL through the plugin's own modules.
fn screen_driver() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/web_top_screen.mjs"
    ))
}

/// Render `frames` through the plugin tree at `plugin`, on the grid the terminal was
/// captured on, and read the driver's column-faithful output back as a [`Screen`].
///
/// `plugin` is a parameter rather than [`plugin_root`] so the negative arm can point this at
/// a mutated copy without touching the shipped tree.
fn render_through_plugin(plugin: &Path, frames: &Path, version: &str, keys: &str) -> Screen {
    let out = Command::new("node")
        .arg(screen_driver())
        .args(["--plugin", &plugin.display().to_string()])
        .args(["--frames", &frames.display().to_string()])
        .args(["--cols", &DRIFT_COLS.to_string()])
        .args(["--rows", &DRIFT_ROWS.to_string()])
        .args(["--version", version])
        .args(["--keys", keys])
        .output()
        .expect("run the plugin's renderer under node");
    assert!(
        out.status.success(),
        "the plugin's renderer is red:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("the driver writes UTF-8");
    let grid: Vec<Vec<String>> = text
        .lines()
        .map(|line| line.split('\u{1f}').map(str::to_string).collect())
        .collect();
    assert_eq!(
        grid.len(),
        usize::from(DRIFT_ROWS),
        "the renderer laid out {} rows, not {DRIFT_ROWS}",
        grid.len()
    );
    Screen { grid }
}

/// `layout::STOPPED_ICON`, the switched-off service's icon — mirrored because the constant is
/// `pub(crate)`, and held to the source by its membership in [`VARIATION_GLYPHS`].
const STOPPED_ICON: &str = "\u{25AA}\u{FE0F}";

/// `ops::backup`'s `icon` in [`DRIFT_SERVICES`]: an operator icon on a base with **no** emoji
/// presentation sequence, so `unicode-width` ignores the selector and it measures one cell
/// (FdK5ONtJ). Held to the fixture by [`drift`]'s floor on it.
const OPERATOR_ICON: &str = "\u{2605}\u{FE0F}";

/// Every variation-selector glyph afkd top's source (`crates/tui/src`) or web-top's
/// (`@afkd/web-top/*.mjs`) spells, by name — the set
/// `every_variation_selector_glyph_measures_alike_in_afkd_top_and_web_top` checks.
const VARIATION_GLYPHS: [&str; 16] = [
    "\u{21BB}\u{FE0E}",
    "\u{2225}\u{FE0E}",
    "\u{2298}\u{FE0E}",
    "\u{22EF}\u{FE0E}",
    "\u{23F1}\u{FE0E}",
    STOPPED_ICON,
    OPERATOR_ICON,
    "\u{2691}\u{FE0E}",
    "\u{2699}\u{FE0E}",
    "\u{2699}\u{FE0F}",
    "\u{270E}\u{FE0E}",
    "\u{2713}\u{FE0E}",
    "\u{2717}\u{FE0E}",
    "\u{2744}\u{FE0F}",
    "\u{1F56F}\u{FE0F}",
    "\u{1F578}\u{FE0F}",
];

/// `text` with every `\u{HEX}` (Rust and javascript) and `\uHHHH` (javascript) escape decoded
/// to its char, everything else left alone — so a glyph spelled as an escape and one spelled
/// literally scan the same.
fn decode_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\\u") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 2..];
        let (hex, used) = match tail.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            },
            None => (tail.get(..4).unwrap_or(""), 4),
        };
        match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
            Some(ch) => {
                out.push(ch);
                rest = &tail[used..];
            }
            None => {
                out.push_str("\\u");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every variation-selector glyph `text` spells: a non-ASCII char followed by U+FE0E or
/// U+FE0F, after [`decode_escapes`]. The ASCII-base exclusion drops the spellings of a bare
/// selector — `'\u{FE0F}'`, `.contains('\u{FE0F}')`, a javascript `"\u{FE0F}"`.
fn variation_glyphs(text: &str) -> BTreeSet<String> {
    let chars: Vec<char> = decode_escapes(text).chars().collect();
    chars
        .windows(2)
        .filter(|pair| !pair[0].is_ascii() && matches!(pair[1], '\u{FE0E}' | '\u{FE0F}'))
        .map(|pair| pair.iter().collect())
        .collect()
}

/// [`variation_glyphs`] over every `.ext` file under `dir`, into subdirectories when
/// `recurse`.
fn source_glyphs(dir: &Path, ext: &str, recurse: bool) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            if recurse {
                found.extend(source_glyphs(&path, ext, recurse));
            }
        } else if path.extension().is_some_and(|e| e == ext) {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            found.extend(variation_glyphs(&text));
        }
    }
    found
}

/// A glyph as its code points, `U+25AA U+FE0F` — a selector is invisible in a failure message.
fn code_points(glyph: &str) -> String {
    glyph
        .chars()
        .map(|c| format!("U+{:04X}", u32::from(c)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every version of `name` a `Cargo.lock` resolves, in the order it lists them.
fn locked_versions(lock: &Path, name: &str) -> Vec<String> {
    let text = std::fs::read_to_string(lock)
        .unwrap_or_else(|err| panic!("the lockfile at {}: {err}", lock.display()));
    let wanted = format!("name = \"{name}\"");
    let lines: Vec<&str> = text.lines().collect();
    lines
        .windows(2)
        .filter(|pair| pair[0] == wanted)
        .filter_map(|pair| pair[1].strip_prefix("version = "))
        .map(|version| version.trim_matches('"').to_string())
        .collect()
}

#[test]
fn the_unicode_width_measured_here_is_afkd_tops() {
    // Every width-parity leg reads the terminal's side off this crate's own `unicode-width`,
    // which stands in for the one afkd top links only while the two lockfiles resolve the
    // same version of it. afkd's crates ask for `0.2`, so the 0.2 line is what has to agree.
    let Some(afkd) = afkd_src() else {
        eprintln!("SKIP: AFKD_SRC names no afkd checkout, and the other lockfile is afkd's");
        return;
    };
    let on_the_line = |versions: Vec<String>| -> Vec<String> {
        versions
            .into_iter()
            .filter(|v| v.starts_with("0.2."))
            .collect()
    };
    let ours = on_the_line(locked_versions(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../Cargo.lock"),
        "unicode-width",
    ));
    let theirs = on_the_line(locked_versions(&afkd.join("Cargo.lock"), "unicode-width"));
    assert!(
        !theirs.is_empty(),
        "afkd's lockfile resolves no unicode-width 0.2"
    );
    assert_eq!(
        ours, theirs,
        "this crate measures with a different unicode-width than afkd top links; pin it with \
         `cargo update -p unicode-width --precise {}`",
        theirs[0]
    );
}

#[test]
fn every_variation_selector_glyph_measures_alike_in_afkd_top_and_web_top() {
    // One glyph, one width: a variation-selector glyph the two dashboards measure apart pads
    // its row apart, and every later column on it moves (B6D5ahX2). The terminal's reading is
    // `unicode-width`'s — `layout.rs` budgets with it, `shell::pin_wide_glyph_widths` reads
    // ratatui's `cell_width()`, which is `unicode-width` too, and afkd's lockfile resolves
    // one version for all three — the version this crate measures with, held to that lock by
    // `the_unicode_width_measured_here_is_afkd_tops`. The plugin's is `layout.mjs`'s own
    // `textWidth`, run under node.
    // Both are measured here; no width is written down.
    if !node_available() {
        eprintln!(
            "skipping the variation-glyph parity test: node is not on PATH, and web-top's \
             `textWidth` is javascript"
        );
        return;
    }
    let Some(afkd) = afkd_src() else {
        eprintln!(
            "skipping the variation-glyph parity test: AFKD_SRC names no afkd checkout, and \
             half the glyphs are spelled in its `crates/tui`"
        );
        return;
    };
    let tui = source_glyphs(&afkd.join("crates/tui/src"), "rs", true);
    let web = source_glyphs(&plugin_root(), "mjs", false);
    for glyph in [STOPPED_ICON, "\u{1F578}\u{FE0F}"] {
        assert!(
            tui.contains(glyph) && web.contains(glyph),
            "{glyph} ({}) is spelled in both trees, so both measure it; the scan found it in \
             crates/tui: {}, web-top: {}",
            code_points(glyph),
            tui.contains(glyph),
            web.contains(glyph)
        );
    }

    let named: BTreeSet<String> = VARIATION_GLYPHS.iter().map(|g| (*g).to_string()).collect();
    let scanned: BTreeSet<String> = tui.union(&web).cloned().collect();
    if scanned != named {
        let side = |g: &str| match (tui.contains(g), web.contains(g)) {
            (true, true) => "both",
            (true, false) => "crates/tui",
            (false, true) => "web-top",
            (false, false) => "neither",
        };
        let added: Vec<String> = scanned
            .difference(&named)
            .map(|g| format!("{g} ({}, {})", code_points(g), side(g)))
            .collect();
        let gone: Vec<String> = named
            .difference(&scanned)
            .map(|g| format!("{g} ({})", code_points(g)))
            .collect();
        panic!(
            "the variation-selector glyphs the two dashboards spell are not VARIATION_GLYPHS: \
             new {added:?}, no longer spelled {gone:?}. A new glyph goes into \
             VARIATION_GLYPHS, and this test then measures it on both sides"
        );
    }

    let out = Command::new("node")
        .args(["--input-type=module", "-e"])
        .arg(
            "const { pathToFileURL } = await import(\"node:url\"); \
             const { textWidth } = await import(pathToFileURL(process.argv[1]).href); \
             console.log(JSON.stringify(JSON.parse(process.argv[2]).map(textWidth)));",
        )
        .arg(plugin_root().join("layout.mjs"))
        .arg(serde_json::to_string(&VARIATION_GLYPHS).expect("the glyphs as JSON"))
        .output()
        .expect("run web-top's textWidth under node");
    assert!(
        out.status.success(),
        "web-top's textWidth is red:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let widths: Vec<usize> =
        serde_json::from_slice(&out.stdout).expect("textWidth's widths as a JSON array");
    assert_eq!(
        widths.len(),
        VARIATION_GLYPHS.len(),
        "textWidth measured {widths:?}, one per glyph"
    );

    let mismatches: Vec<String> = VARIATION_GLYPHS
        .iter()
        .zip(&widths)
        .filter_map(|(glyph, web_top)| {
            let afkd_top = unicode_width::UnicodeWidthStr::width(*glyph);
            (afkd_top != *web_top).then(|| {
                format!(
                    "{glyph} ({}): afkd top {afkd_top}, web-top {web_top}",
                    code_points(glyph)
                )
            })
        })
        .collect();
    let checked: Vec<String> = VARIATION_GLYPHS
        .iter()
        .map(|g| format!("{g} ({})", code_points(g)))
        .collect();
    assert!(
        mismatches.is_empty(),
        "the two dashboards measure these variation-selector glyphs apart: {mismatches:#?}\n\
         checked: {checked:?}"
    );
}

/// Unicode's list of emoji presentation sequences, vendored verbatim at the version
/// `unicode-width` was generated from — the file its `starts_emoji_presentation_seq` table
/// and `layout.mjs`'s `EMOJI_PRESENTATION_BASES` are both built off.
fn emoji_variation_sequences() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/emoji-variation-sequences.txt");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The bases `text` lists as starting an emoji presentation sequence: every `<base> FE0F ;
/// emoji style` line, read by the rule `unicode-width`'s own `scripts/unicode.py` applies.
fn emoji_presentation_bases(text: &str) -> BTreeSet<u32> {
    text.lines()
        .filter_map(|line| {
            let (sequence, style) = line.split_once(';')?;
            let mut points = sequence.split_whitespace();
            let base = u32::from_str_radix(points.next()?, 16).ok()?;
            (points.next() == Some("FE0F")
                && points.next().is_none()
                && style.trim_start().starts_with("emoji style"))
            .then_some(base)
        })
        .collect()
}

#[test]
fn every_emoji_presentation_base_measures_alike_in_afkd_top_and_web_top() {
    // A VS16 widens a cluster only on a base that starts an emoji presentation sequence;
    // anywhere else `unicode-width` ignores the selector and the base's own width stands, so
    // `★️` is one cell in afkd top (FdK5ONtJ). Every scalar in planes 0 and 1 — every listed
    // base is among them — is measured bare and with VS16 appended, by `unicode-width` and by
    // `layout.mjs`'s `textWidth` under node. Both are measured here; no width is written down.
    if !node_available() {
        eprintln!(
            "skipping the emoji-presentation parity test: node is not on PATH, and web-top's \
             `textWidth` is javascript"
        );
        return;
    }
    let file = emoji_variation_sequences();
    assert!(
        file.lines().any(|line| line.trim() == "# Version: 17.0")
            && unicode_width::UNICODE_VERSION == (17, 0, 0),
        "the vendored emoji-variation-sequences.txt is Unicode 17.0 and unicode-width is built \
         from {:?}: a unicode-width bump needs the file re-vendored at the crate's Unicode \
         version and `layout.mjs`'s EMOJI_PRESENTATION_BASES regenerated from it",
        unicode_width::UNICODE_VERSION
    );
    let listed = emoji_presentation_bases(&file);
    // The card's four unlisted bases — `★` `●` `✓` `a` — and one listed one, so a parser
    // that read nothing, or everything, fails here rather than checking an empty list.
    const UNLISTED: [u32; 4] = [0x2605, 0x25CF, 0x2713, 0x61];
    assert!(
        listed.len() >= 300
            && listed.contains(&0x25AA)
            && UNLISTED.iter().all(|cp| !listed.contains(cp)),
        "the file lists {} emoji presentation bases, with U+25AA among them and none of \
         {UNLISTED:x?}",
        listed.len()
    );

    // Every scalar from the space up, less the C1 controls and the surrogates; generated on
    // both sides rather than passed, so nothing crosses argv.
    let scalars = || {
        (0x20..=0x1FFFF)
            .filter(|cp| !(0x7F..=0x9F).contains(cp))
            .filter_map(char::from_u32)
    };
    let out = Command::new("node")
        .args(["--input-type=module", "-e"])
        .arg(
            "const { pathToFileURL } = await import(\"node:url\"); \
             const { textWidth } = await import(pathToFileURL(process.argv[1]).href); \
             const out = []; \
             for (let cp = 0x20; cp <= 0x1ffff; cp++) { \
               if ((cp >= 0x7f && cp <= 0x9f) || (cp >= 0xd800 && cp <= 0xdfff)) continue; \
               const c = String.fromCodePoint(cp); \
               out.push([cp, textWidth(c), textWidth(c + \"\\u{FE0F}\")]); \
             } \
             console.log(JSON.stringify(out));",
        )
        .arg(plugin_root().join("layout.mjs"))
        .output()
        .expect("run web-top's textWidth under node");
    assert!(
        out.status.success(),
        "web-top's textWidth is red:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let measured: Vec<(u32, usize, usize)> =
        serde_json::from_slice(&out.stdout).expect("textWidth's widths as a JSON array");
    assert_eq!(
        measured.len(),
        scalars().count(),
        "textWidth measured one bare and one VS16 width per scalar"
    );

    let (mut mismatches, mut unlisted_checked, mut unlisted_seen) =
        (Vec::new(), 0usize, BTreeSet::new());
    for &(cp, web_bare, web_vs16) in &measured {
        let base = char::from_u32(cp).expect("node measured a scalar");
        let glyph = format!("{base}\u{FE0F}");
        let top_bare = unicode_width::UnicodeWidthStr::width(base.to_string().as_str());
        let top_vs16 = unicode_width::UnicodeWidthStr::width(glyph.as_str());
        if listed.contains(&cp) {
            // `top_vs16 == 2` holds the vendored file to the crate's own list.
            if top_vs16 != 2 || web_vs16 != top_vs16 {
                mismatches.push(format!(
                    "{glyph} ({}, listed): afkd top {top_vs16}, web-top {web_vs16}",
                    code_points(&glyph)
                ));
            }
        } else if top_bare == web_bare && top_bare != 0 {
            // A base the two already measure apart (`⌚`: web-top 1, afkd top 2) or a
            // zero-width one (a mark, with VS16 after it) is a gap in the base widths, not in
            // the selector's, and outside this card.
            unlisted_checked += 1;
            if UNLISTED.contains(&cp) {
                unlisted_seen.insert(cp);
            }
            if web_vs16 != top_vs16 {
                mismatches.push(format!(
                    "{glyph} ({}): afkd top {top_vs16}, web-top {web_vs16}",
                    code_points(&glyph)
                ));
            }
        }
    }
    assert!(
        unlisted_checked >= 100_000 && unlisted_seen == BTreeSet::from(UNLISTED),
        "the sweep checked {unlisted_checked} unlisted bases, among them {unlisted_seen:x?} of \
         the card's {UNLISTED:x?}"
    );
    assert!(
        mismatches.is_empty(),
        "the two dashboards measure {} VS16 clusters apart, the first 20: {:#?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(20)]
    );
}

#[test]
fn the_plugins_overview_matches_afkd_tops_own_screen() {
    // The card's point. The relay's page re-derives `afkd top`'s board in its own javascript
    // — the layout is crate-internal and afkd's crates are unpublished — so nothing but this
    // leg holds the two together, and the day a badge, a column or a footer hint moves in
    // `crates/tui/` the plugin would go on painting yesterday's dashboard in silence. The
    // plugin's own `node --test` goldens only prove it agrees with itself.
    //
    // The scope is the **overview**, deliberately. The `Queues` section, the info page and
    // the run view each diverge from the terminal on purpose and say so in the plugin's own
    // source (`layout.mjs`'s lane priority, its two-column info grid), and the flat arm is
    // not modelled there at all — which is why the terminal is brought onto the grouped arm
    // below rather than the plugin onto the flat one. The run view has its own drift leg
    // (`the_plugins_run_view_matches_afkd_tops_own_screen`), and the other three are held to
    // [`LEDGER`] by `web_tops_divergences_from_afkd_top_are_exactly_the_ledgers`, which keeps
    // them from growing a fourth.
    if !python3_available() {
        eprintln!("skipping the drift leg: python3 is not on PATH, and the relay that carries the frames is python");
        return;
    }
    if !node_available() {
        eprintln!(
            "skipping the drift leg: node is not on PATH, and the plugin's renderer is javascript"
        );
        return;
    }
    let config = format!("{DRIFT_SERVICES}\nplugin {NAME} {{\n  port 0\n}}\n");
    let (_daemon, dir, port) = daemon_serving(&config);
    let scratch = TempDir::new().expect("a tempdir for the capture");

    // One stream serves all three duties — the snapshot, the stop's own events, and the
    // capture the renderer folds — which is why the order below is fixed: the snapshot lands
    // before the stop, so the fold reaches the same stopped board the terminal's own later
    // attach starts from. It is also the page's real input (the relay forwards every wire
    // frame verbatim), so the renderer is fed exactly what a browser gets.
    let mut sse = Sse::admitted(port);
    let (id, welcome) = sse.handshake_with_welcome();
    let version = welcome["daemon"]
        .as_str()
        .unwrap_or_else(|| panic!("the welcome names the daemon's version: {welcome}"))
        .to_string();

    let mut frames: Vec<String> = Vec::new();
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["meta"] == "snapshot"),
        "the attach snapshot never arrived; the stream carried {frames:?}"
    );
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id, "command": "stop", "service": STOPPED}),
    );
    assert_eq!(status, 200, "POST /command stop: {body}");
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["event"] == "service_paused"
            && f["service"] == STOPPED
            && f["paused"] == true),
        "the stop never settled on the stream; it carried {frames:?}"
    );

    let tui = spawn_captured_sized(dir.path(), &["top"], &[], DRIFT_COLS, DRIFT_ROWS);
    // `afkd top` boots **flat** and the plugin models only the grouped tree, so the two are
    // brought onto the same arm before anything is compared — verified rather than assumed,
    // because the daemon holds the view state across attaches and a blind press would be a
    // coin flip against a home that had been driven before.
    tui.send(b"v");
    if !tui.wait_until(Duration::from_secs(5), |s| s.contains('├')) {
        tui.send(b"v");
        assert!(
            tui.wait_until(BUDGET, |s| s.contains('├')),
            "two `v` presses and the terminal is still not on the grouped tree:\n{}",
            tui.screen()
        );
    }
    tui.send(b"g");
    let terminal = settled_terminal(&tui);
    // …and the daemon's once-a-second host reading, so the renderer draws a load strip too:
    // the terminal's own ring is seeded from the same broadcast, and a capture that ended
    // before one arrived would leave the two screens disagreeing about a row neither is at
    // fault for.
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["meta"] == "host_load"),
        "no host reading crossed the stream; the capture carried {} frames",
        frames.len()
    );

    let capture = scratch.path().join("frames.jsonl");
    std::fs::write(&capture, frames.join("\n") + "\n").expect("write the captured frames");
    // `g` on the plugin too: the cursor is style-only on both surfaces, but entering from
    // the same row is what keeps it that way if either ever grows a marker.
    let page = render_through_plugin(&plugin_root(), &capture, &version, "g");
    if let Err(fault) = drift(&terminal, &page) {
        panic!("{fault}");
    }

    // The negative arm, on the same capture: a column header renamed in a **copy** of the
    // plugin has to redden this leg, and the failure has to be readable. `Cadence` is the
    // same seven cells as `Trigger`, so the width shed is untouched and the label is the
    // only thing that moved.
    let mutated = TempDir::new().expect("a tempdir for the mutated copy");
    let copy = mutated.path().join("web-top");
    copy_tree(&plugin_root(), &copy);
    let layout = copy.join("layout.mjs");
    let before = std::fs::read_to_string(&layout).expect("the copy's layout module");
    const COLUMN: &str = r#"{ key: "trigger", label: "Trigger", align: "left" }"#;
    // An arm that mutated nothing would pass this whole block by comparing the shipped tree
    // against itself and calling the agreement a refusal, so the edit proves it landed —
    // once, in the declaration, and not in some doc comment quoting it.
    assert_eq!(
        before.matches(COLUMN).count(),
        1,
        "`layout.mjs` spells its `Trigger` column once; this arm's rewrite is no longer \
         aimed at the one declaration"
    );
    let renamed = before.replace(
        COLUMN,
        r#"{ key: "trigger", label: "Cadence", align: "left" }"#,
    );
    std::fs::write(&layout, &renamed).expect("write the mutated layout module");
    let drifted = render_through_plugin(&copy, &capture, &version, "g");
    let fault = drift(&terminal, &drifted)
        .expect_err("a renamed column header is drift, and this leg has to say so");
    assert!(
        fault.contains("Trigger") && fault.contains("Cadence"),
        "the failure names the label that moved and what it moved to: {fault}"
    );
    assert!(
        fault.contains(&format!("--- afkd top, {DRIFT_COLS}x{DRIFT_ROWS} ---"))
            && fault.contains(&format!("--- {NAME}, {DRIFT_COLS}x{DRIFT_ROWS} ---"))
            && fault.matches("監視").count() == 2,
        "the failure prints both screens, so a drift is diagnosable from the output alone: {fault}"
    );

    // The second arm: `Next Run` flushed **left** in a copy has to redden the leg too — the
    // one-cell short edge `column_cell` exists to see, which a trimmed comparison let through.
    const NEXT: &str = r#"{ key: "next", label: "Next Run", align: "right" }"#;
    assert_eq!(
        before.matches(NEXT).count(),
        1,
        "`layout.mjs` spells its `Next Run` column once; this arm's rewrite is no longer \
         aimed at the one declaration"
    );
    let flipped = before.replace(NEXT, r#"{ key: "next", label: "Next Run", align: "left" }"#);
    std::fs::write(&layout, &flipped).expect("write the mutated layout module");
    let misaligned = render_through_plugin(&copy, &capture, &version, "g");
    let fault = drift(&terminal, &misaligned).expect_err(
        "a countdown one cell short of its header's edge is drift, and this leg has to say so",
    );
    assert!(
        fault.contains("`Next Run` cell differs"),
        "the failure names the column whose edge moved: {fault}"
    );

    // The third arm: `▪️` measured one cell narrower in a copy has to redden the leg — B6D5ahX2
    // in reverse. The icon's covered column is the `Service` cell's own padding, which a
    // collapsed comparison let through (both cells read `▪️ never`).
    const VS16_RULE: &str = "if (startsEmojiPresentation(cluster, base)) return 2;";
    assert_eq!(
        before.matches(VS16_RULE).count(),
        1,
        "`layout.mjs` spells its VS16 width rule once; this arm's rewrite is no longer aimed \
         at the one rule"
    );
    let narrowed = before.replace(
        VS16_RULE,
        "if (startsEmojiPresentation(cluster, base)) return 1;",
    );
    std::fs::write(&layout, &narrowed).expect("write the mutated layout module");
    let shifted = render_through_plugin(&copy, &capture, &version, "g");
    let fault = drift(&terminal, &shifted).expect_err(
        "a VS16 icon one cell narrower than afkd top's is drift, and this leg has to say so",
    );
    assert!(
        fault.contains("`Service` cell differs"),
        "the failure names the column whose padding moved: {fault}"
    );

    // The fourth arm: FdK5ONtJ restored — every VS16 cluster two cells, whatever its base.
    // `ops::backup`'s `★️` is one cell in the terminal, which ignores the selector on a base
    // with no emoji presentation sequence, so the widened icon pushes that row's name out.
    let widened = before.replace(VS16_RULE, r#"if (cluster.includes("\u{FE0F}")) return 2;"#);
    std::fs::write(&layout, &widened).expect("write the mutated layout module");
    let pushed = render_through_plugin(&copy, &capture, &version, "g");
    let fault = drift(&terminal, &pushed)
        .expect_err("a VS16 widening a non-emoji base is drift, and this leg has to say so");
    assert!(
        fault.contains("`Service` cell differs"),
        "the failure names the column the widened icon pushed: {fault}"
    );
}

// --- the run view's own drift leg ------------------------------------------------------

/// The run view's fixed bands, the same four rows on both surfaces: the pinned list-row
/// header, a blank spacer, the shown pane's title, a blank spacer, then the body. The
/// terminal's come from `shell::split_geometry` over `TITLE_ROWS` and `PANE_TITLE_ROWS`
/// (both 2); the plugin's from `layout.mjs`'s `runFrame`, which pushes exactly those four
/// rows before its first body row.
const RUN_HEADER_ROW: usize = 0;
const PANE_TITLE_ROW: usize = 2;
const PANE_BODY_ROW: usize = 4;

/// `logview::render_line`'s lead-in: `stamp_hms`'s twelve columns plus the one space it
/// joins the message with. The terminal spends it on every log row and the plugin spends
/// none of it — the first of the three log divergences `plugins/@afkd/web-top/README.md`
/// already records — so [`run_drift`] matches it against its exact shape and accounts for
/// it, rather than stripping it.
const LOG_STAMP_W: usize = 13;

/// The widest log **message** [`DRIFT_SERVICES`] may print before the two panes stop
/// agreeing. The headroom is asymmetric and tighter than the screen: the terminal wraps
/// `stamp + " " + text` at `log_content_width(100)` = 99, leaving the text itself
/// `99 - LOG_STAMP_W` cells, while the plugin wraps the stamp-less text at `paneW` = 99. A
/// line between 87 and 99 cells therefore wraps on the terminal only. The fixture's `echo`
/// and `seq` output is sized against **this** number, not against `DRIFT_COLS`.
const LOG_MESSAGE_BUDGET: usize = DRIFT_COLS as usize - 1 - LOG_STAMP_W;

/// What has to be on the run view before agreeing with the renderer means anything, the
/// same discipline the overview leg's floors carry. The fixture's nested `times` puts six
/// nodes on the tree and the failed leaf's auto-expanded body six more under it; its output
/// plus the supervisor's own `[RUN]`/`[OK]`/`[ERR]` brackets fill the log. Both are floors,
/// so a step added to the fixture is not a reason to touch them.
const RUN_TREE_ROW_FLOOR: usize = 8;
const RUN_LOG_LINE_FLOOR: usize = 8;
/// Both panes' footers carry ten hints today; nine only ever under-counts.
const RUN_FOOTER_HINT_FLOOR: usize = 9;

/// Which pane the run view is showing, and so which body rule [`run_drift`] applies. The
/// two panes are two screens on both surfaces: each opens on the log (`model.rs` "o lands on
/// the log", `layout.test.mjs` "the run view opens on the log pane") and `Tab` swaps.
#[derive(Clone, Copy)]
enum RunPane {
    Log,
    Tree,
}

impl RunPane {
    /// The title this pane paints — the string [`settled_run`] waits for and [`run_drift`]
    /// compares verbatim. `Log · all` is the unscoped spelling both surfaces open on.
    fn title(self) -> &'static str {
        match self {
            RunPane::Log => "Log · all",
            RunPane::Tree => "Tree",
        }
    }
}

/// Whether `text` is `logview::render_line`'s lead-in — `HH:MM:SS.mmm` and the space that
/// joins it to the message, [`LOG_STAMP_W`] columns in all. Matched rather than consumed by
/// a wildcard, so a stamp that changed *shape* fails the leg instead of being absorbed by it.
fn is_stamp(text: &str) -> bool {
    let shape: Vec<char> = text.chars().collect();
    shape.len() == LOG_STAMP_W
        && shape.iter().enumerate().all(|(at, c)| match at {
            2 | 5 => *c == ':',
            8 => *c == '.',
            12 => *c == ' ',
            _ => c.is_ascii_digit(),
        })
}

/// Whether `text` is `logview::day_marker`'s `-- YYYY-MM-DD --` rollover row. The terminal
/// synthesizes one ahead of the first line and at every civil-day change; the plugin draws
/// none at all (the same recorded divergence the stamp belongs to).
fn is_day_marker(text: &str) -> bool {
    text.strip_prefix("-- ")
        .and_then(|rest| rest.strip_suffix(" --"))
        .is_some_and(|date| {
            date.len() == 10
                && date.chars().enumerate().all(|(at, c)| {
                    if at == 4 || at == 7 {
                        c == '-'
                    } else {
                        c.is_ascii_digit()
                    }
                })
        })
}

/// Diff the terminal's **run view** against the plugin's, on its stable parts: the pinned
/// header row's lead-in and cells, the shown pane's title, that pane's body, and the
/// footer's hint set.
///
/// [`drift`]'s twin in shape — a `Result`, so the negative arm can demand a failure and read
/// what it says, with both whole screens in every message — and its twin in tolerance, which
/// is still "normalise inside a span already pinned by column". What differs is where each
/// pin comes from, because the run view paints no column header of its own:
///
/// - the header row's comes from `spans`, the terminal's *overview* column ladder. The header
///   is the list's own row for the service through the shared `layout::service_row` seam, over
///   the columns `visible_columns(size.width)` sheds at the same terminal width, so the list's
///   spans read it. Its **lead-in**, which no span can see, gets a claim of its own first;
/// - a tree row's comes from the row itself: [`segments`] already hands back each segment's
///   start column, so a rail glyph lives in segment 0's text, a node's depth in the label
///   segment's start, and the right-flushed `N ln`/`TOOK`/status tail in its own columns.
///   [`normalise`] runs *inside* a segment's text and nowhere else;
/// - a log row's is the asserted [`LOG_STAMP_W`] offset, and the remainder is compared
///   **verbatim** — log text carries no live reading for `normalise` to have an opinion about.
fn run_drift(
    top: &Screen,
    plugin: &Screen,
    spans: &[(String, (usize, usize))],
    pane: RunPane,
) -> Result<(), String> {
    let both = format!(
        "\n\n--- afkd top, {DRIFT_COLS}x{DRIFT_ROWS} ---\n{}\n\n--- {NAME}, {DRIFT_COLS}x{DRIFT_ROWS} ---\n{}",
        top.render(),
        plugin.render()
    );

    // The header's lead-in, before any cell: an indent on one surface only is named as one
    // here, in its own sentence, before a cell comparison below reports it as a difference in
    // whichever cell it lands in.
    let (top_ink, plugin_ink) = (
        top.first_ink(RUN_HEADER_ROW),
        plugin.first_ink(RUN_HEADER_ROW),
    );
    if top_ink != plugin_ink {
        return Err(format!(
            "the run header's lead-in differs: afkd top's first ink is at column {top_ink:?}, web-top's at {plugin_ink:?}\n  afkd top: {:?}\n  web-top:  {:?}{both}",
            top.row_text(RUN_HEADER_ROW).trim_end(),
            plugin.row_text(RUN_HEADER_ROW).trim_end(),
        ));
    }
    for (label, span) in spans {
        let (mine, theirs) = (
            column_cell(&top.grid[RUN_HEADER_ROW], label, *span),
            column_cell(&plugin.grid[RUN_HEADER_ROW], label, *span),
        );
        if mine != theirs {
            return Err(format!(
                "the run header's `{label}` cell differs:\n  afkd top: {mine:?}\n  web-top:  {theirs:?}{both}"
            ));
        }
    }

    let (top_title, plugin_title) = (top.pane_title(), plugin.pane_title());
    if top_title != plugin_title {
        return Err(format!(
            "the pane title differs:\n  afkd top: {top_title:?}\n  web-top:  {plugin_title:?}{both}"
        ));
    }

    let (top_body, plugin_body) = (top.pane_body(), plugin.pane_body());
    // How many day markers the terminal drew, so the log arm's floor below can say the
    // markers were really matched rather than never encountered.
    let mut markers = 0usize;
    match pane {
        RunPane::Tree => {
            if top_body.len() != plugin_body.len() {
                return Err(format!(
                    "afkd top draws {} tree rows, web-top {}{both}",
                    top_body.len(),
                    plugin_body.len()
                ));
            }
            for (at, (top_row, plugin_row)) in top_body.iter().zip(&plugin_body).enumerate() {
                let (mine, theirs) = (segments(top_row), segments(plugin_row));
                if mine.len() != theirs.len() {
                    return Err(format!(
                        "tree row {at} splits into {} segments on afkd top and {} on web-top:\n  afkd top: {mine:?}\n  web-top:  {theirs:?}{both}",
                        mine.len(),
                        theirs.len()
                    ));
                }
                for (nth, ((mine_text, mine_at), (their_text, their_at))) in
                    mine.iter().zip(&theirs).enumerate()
                {
                    let (mine_text, their_text) = (normalise(mine_text), normalise(their_text));
                    if mine_text != their_text || mine_at != their_at {
                        return Err(format!(
                            "tree row {at}'s segment {nth} differs:\n  afkd top: ({mine_text:?}, {mine_at})\n  web-top:  ({their_text:?}, {their_at}){both}"
                        ));
                    }
                }
            }
        }
        RunPane::Log => {
            // The fixture's messages are sized under `LOG_MESSAGE_BUDGET` so neither pane
            // wraps, which is what keeps the second recorded log divergence — the plugin's
            // 2-cell continuation hang against the terminal's 13 — off this screen rather
            // than tolerated on it. Asserted first, because a wrap would otherwise surface
            // downstream as a stamp that would not parse or a pane one row too long.
            for (whose, rows, hang) in [
                ("afkd top", &top_body, "             "),
                ("web-top", &plugin_body, "  "),
            ] {
                if let Some(row) = rows
                    .iter()
                    .map(|row| row.concat())
                    .find(|text| text.starts_with(hang) && !text.trim().is_empty())
                {
                    return Err(format!(
                        "{whose} wrapped a log line — {:?} opens with its {}-cell continuation hang. The fixture's messages are sized against {LOG_MESSAGE_BUDGET} cells, the terminal's 99-column wrap less its {LOG_STAMP_W}-cell stamp{both}",
                        row.trim_end(),
                        hang.len()
                    ));
                }
            }
            let mut theirs = plugin_body.iter();
            for (at, row) in top_body.iter().enumerate() {
                let painted = row.concat();
                let painted = painted.trim_end();
                if is_day_marker(painted) {
                    markers += 1;
                    continue;
                }
                let Some(stamp) = painted.get(..LOG_STAMP_W).filter(|head| is_stamp(head)) else {
                    return Err(format!(
                        "afkd top's log row {at} does not open with `logview::render_line`'s `HH:MM:SS.mmm ` stamp: {painted:?}{both}"
                    ));
                };
                let Some(mine) = theirs.next() else {
                    return Err(format!(
                        "afkd top's log pane holds a line web-top's does not: {painted:?}{both}"
                    ));
                };
                let mine = mine.concat();
                let mine = mine.trim_end();
                if painted != format!("{stamp}{mine}").trim_end() {
                    return Err(format!(
                        "log line {at} differs:\n  afkd top: {painted:?}\n  web-top:  {mine:?} (under afkd top's {stamp:?}){both}"
                    ));
                }
            }
            if let Some(extra) = theirs.next() {
                return Err(format!(
                    "web-top's log pane holds a line afkd top's does not: {:?}{both}",
                    extra.concat().trim_end()
                ));
            }
        }
    }

    let (top_hints, plugin_hints) = (top.footer_hints(), plugin.footer_hints());
    if top_hints != plugin_hints {
        return Err(format!(
            "the run view's footer hint set differs:\n  afkd top: {top_hints:?}\n  web-top:  {plugin_hints:?}{both}"
        ));
    }

    // …and the floors, so an empty pane cannot agree with an empty renderer and call it
    // parity. Last, as [`drift`] does it, so an honest disagreement is always reported before
    // "there was nothing on screen".
    for (whose, screen) in [("afkd top", top), ("web-top", plugin)] {
        let gutter = screen.scrollbar_gutter();
        if !gutter.trim().is_empty() {
            return Err(format!(
                "{whose}'s pane overflowed its viewport — its scrollbar gutter reads {gutter:?}, so the footer reader can no longer find where the body stops{both}"
            ));
        }
        if !cell_text(&screen.grid[RUN_HEADER_ROW], span_of(spans, "Service")).contains("監視") {
            return Err(format!(
                "{whose}'s run header does not name the fired service, so the two cursors never landed on the same row{both}"
            ));
        }
    }
    match pane {
        RunPane::Tree => {
            if top_body.len() < RUN_TREE_ROW_FLOOR {
                return Err(format!(
                    "only {} tree rows were on screen; the fixture's run ships {RUN_TREE_ROW_FLOOR}{both}",
                    top_body.len()
                ));
            }
            let painted: String = top_body.iter().map(|row| row.concat()).collect();
            if !painted.contains('✓') || !painted.contains('✗') {
                return Err(format!(
                    "the tree carried one status glyph; the fixture's last step fails so it carries both a ✓ and a ✗{both}"
                ));
            }
            if !painted.contains("… ") {
                return Err(format!(
                    "no elision row was on screen, so the failed leaf's body never reached BODY_ELIDE{both}"
                ));
            }
            if !top_body
                .iter()
                .any(|row| segments(row).iter().any(|(_, at)| *at >= 3))
            {
                return Err(format!(
                    "every tree row's ink starts at column 0, so a flat tree would pass the depth claim vacuously; the fixture nests a `times` inside a `times` for exactly this{both}"
                ));
            }
        }
        RunPane::Log => {
            if top_body.len() < RUN_LOG_LINE_FLOOR {
                return Err(format!(
                    "only {} log rows were on screen; the fixture's run ships {RUN_LOG_LINE_FLOOR}{both}",
                    top_body.len()
                ));
            }
            if markers == 0 {
                return Err(format!(
                    "no `-- YYYY-MM-DD --` day marker was on screen, so the terminal's marker rows were never matched and skipping them proved nothing{both}"
                ));
            }
        }
    }
    if top_hints.len() < RUN_FOOTER_HINT_FLOOR {
        return Err(format!(
            "only {} footer hints were on the run view{both}",
            top_hints.len()
        ));
    }
    Ok(())
}

/// Poll the terminal until it is on `pane`'s run view with something to compare — the pane's
/// own title, a body under it, a header row with ink on it and the whole footer — and hand
/// that screen back. [`settled_terminal`]'s twin: read through the very readers [`run_drift`]
/// compares with, so a half-painted frame fails **here**, with the terminal's own screen to
/// look at, rather than downstream as a row the plugin is missing.
fn settled_run(tui: &CapturedTui, pane: RunPane) -> Screen {
    let deadline = Instant::now() + BUDGET;
    loop {
        let screen = Screen {
            grid: (0..DRIFT_ROWS).map(|row| tui.row_grid(row)).collect(),
        };
        if screen.pane_title() == pane.title()
            && !screen.pane_body().is_empty()
            && screen.first_ink(RUN_HEADER_ROW).is_some()
            && screen.footer_hints().len() >= RUN_FOOTER_HINT_FLOOR
        {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "the terminal never settled onto the run view's `{}` pane:\n{}",
            pane.title(),
            screen.render()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_plugins_run_view_matches_afkd_tops_own_screen() {
    // The overview leg's point, one surface further in. `e5ae8e76` shipped the run view in
    // the plugin — the trace tree and the log pane — and nothing but the plugin's own goldens
    // held it to `crates/tui/`'s until now, so a pane header, a rail glyph or a body-elision
    // rule could move in `treeview.rs`/`logview.rs` and the page would go on painting
    // yesterday's run in silence.
    //
    // The card named two known gaps and asked for a decision rather than a discovery. **Both
    // are closed in the plugin**, because in each case the deciding Rust says the plugin was
    // re-deriving something `afkd top` does not paint:
    //
    // 1. the log pane's title read `Log · all · 0-0/0` there and `Log · all` here.
    //    `crates/tui/src/logview.rs:39-42`, on the field the range comes from: *"The
    //    title-bar text: `afkd › N service · state · elapsed · start–end/total`. Retained on
    //    the neutral view … but the split pane renders a fixed `Log` label"*. The plugin had
    //    ported a retired title's payload; the scrollbar it already draws is the live answer
    //    to "where in the ring am I", so `paneTitleCells` dropped the range cell;
    // 2. the run title row carried the grouped list's two-space indent in the plugin and not
    //    in the terminal. `crates/tui/src/layout.rs:1410-1414`, on the shared row seam:
    //    *"`connector` is the tree-connector prefix the caller resolved (`└─ `/`├─ `, or `""`
    //    for the pinned header). Only the **grouped** tree supplies a non-empty one for a
    //    top-level row … in the flat overview the list row for a top-level service is
    //    *byte-identical* to the pinned header row"*. `runHeaderCells` already passed
    //    `prefix: ""` on purpose and `identityOf` was overriding it from `depth === 0`; the
    //    lead-in moved to `buildRows`' walk, the one place the tree is walked.
    //
    // Neither is absorbed by a widened normalisation. Gap 1 reddens a **verbatim** pane-title
    // compare; gap 2 reddens `Screen::first_ink`, a reader added for it because every other
    // reader in this file begins at the first ink and so cannot see a lead-in at all.
    if !python3_available() {
        eprintln!("skipping the run-view drift leg: python3 is not on PATH, and the relay that carries the frames is python");
        return;
    }
    if !node_available() {
        eprintln!("skipping the run-view drift leg: node is not on PATH, and the plugin's renderer is javascript");
        return;
    }
    let config = format!("{DRIFT_SERVICES}\nplugin {NAME} {{\n  port 0\n}}\n");
    let (_daemon, dir, port) = daemon_serving(&config);
    let scratch = TempDir::new().expect("a tempdir for the capture");

    let mut sse = Sse::admitted(port);
    let (id, welcome) = sse.handshake_with_welcome();
    let version = welcome["daemon"]
        .as_str()
        .unwrap_or_else(|| panic!("the welcome names the daemon's version: {welcome}"))
        .to_string();
    let mut frames: Vec<String> = Vec::new();
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["meta"] == "snapshot"),
        "the attach snapshot never arrived; the stream carried {frames:?}"
    );
    // The same stop the overview leg makes, for the same reason: [`settled_terminal`] waits
    // on a board carrying two distinct badges, and a fire leaves every row `Idle`.
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id, "command": "stop", "service": STOPPED}),
    );
    assert_eq!(status, 200, "POST /command stop: {body}");
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["event"] == "service_paused"
            && f["service"] == STOPPED
            && f["paused"] == true),
        "the stop never settled on the stream; it carried {frames:?}"
    );

    // The terminal attaches **before** the fire, deliberately: the control socket serves no
    // run history (the plugin's own backfill read, `a72fbdc7`, exists because of that), so a
    // run the terminal did not watch happen is a run it cannot open. Both surfaces therefore
    // read the same live frames.
    let tui = spawn_captured_sized(dir.path(), &["top"], &[], DRIFT_COLS, DRIFT_ROWS);
    tui.send(b"v");
    if !tui.wait_until(Duration::from_secs(5), |s| s.contains('├')) {
        tui.send(b"v");
        assert!(
            tui.wait_until(BUDGET, |s| s.contains('├')),
            "two `v` presses and the terminal is still not on the grouped tree:\n{}",
            tui.screen()
        );
    }
    tui.send(b"g");

    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id, "command": "fire", "service": BUSY}),
    );
    assert_eq!(status, 200, "POST /command fire: {body}");
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["event"] == "fire_failed"
            && f["service"] == BUSY),
        "the fire never failed on the stream; it carried {frames:?}"
    );
    let terminal = settled_terminal(&tui);
    // The fire's own trailing frames, plus the daemon's once-a-second host reading: the
    // reporter emits `fire_failed` before the root node's closing `trace` necessarily lands,
    // and a capture cut at the failure would leave the plugin's tree showing a running root
    // against the terminal's closed one. Taken *after* the terminal settled, so the capture
    // is never the older of the two.
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["meta"] == "host_load"),
        "no host reading crossed the stream; the capture carried {} frames",
        frames.len()
    );
    let capture = scratch.path().join("frames.jsonl");
    std::fs::write(&capture, frames.join("\n") + "\n").expect("write the captured frames");

    // The cursor: counted on the terminal's own settled board and replayed on both surfaces,
    // so neither is told where the row is. The column ladder comes off that same board — the
    // run view paints no column header, and its pinned header is the list's row over the very
    // columns the list shed at this width.
    let header = terminal
        .column_header()
        .expect("the settled overview carries a column header");
    let spans = column_spans(&header, usize::from(DRIFT_COLS));
    let steps = terminal
        .service_rows()
        .iter()
        .position(|row| cell_text(row, span_of(&spans, "Service")).contains("監視"))
        .expect("the fired service is on the settled board");
    let keys = format!("g{}o", "j".repeat(steps));

    for key in keys.trim_start_matches('g').bytes() {
        tui.send(&[key]);
    }
    let top_log = settled_run(&tui, RunPane::Log);
    let plugin_log = render_through_plugin(&plugin_root(), &capture, &version, &keys);
    if let Err(fault) = run_drift(&top_log, &plugin_log, &spans, RunPane::Log) {
        panic!("{fault}");
    }

    // `Tab` swaps to the tree on both surfaces; the driver spells it `\t`.
    tui.send(b"\t");
    let top_tree = settled_run(&tui, RunPane::Tree);
    let tree_keys = format!("{keys}\t");
    let plugin_tree = render_through_plugin(&plugin_root(), &capture, &version, &tree_keys);
    if let Err(fault) = run_drift(&top_tree, &plugin_tree, &spans, RunPane::Tree) {
        panic!("{fault}");
    }

    // The negative arm, on the same capture and in the overview arm's shape: a pane header
    // renamed in a **copy** of the plugin has to redden this leg, and the failure has to be
    // readable. `Tree` is the string the terminal paints (`shell.rs:2046`; the comment at
    // `shell.rs:945` still says `Trace` and is stale), so the rename runs that way round.
    let mutated = TempDir::new().expect("a tempdir for the mutated copy");
    let copy = mutated.path().join("web-top");
    copy_tree(&plugin_root(), &copy);
    let layout = copy.join("layout.mjs");
    let before = std::fs::read_to_string(&layout).expect("the copy's layout module");
    const PANE_HEADER: &str = r#"cell("Tree", { fg: "bright", bold: true })"#;
    // An arm that mutated nothing would compare the shipped tree against itself and call the
    // agreement a refusal, so the edit proves it landed — once, in the declaration.
    assert_eq!(
        before.matches(PANE_HEADER).count(),
        1,
        "`layout.mjs` spells its `Tree` pane header once; this arm's rewrite is no longer \
         aimed at the one declaration"
    );
    let renamed = before.replace(
        PANE_HEADER,
        r#"cell("Trace", { fg: "bright", bold: true })"#,
    );
    std::fs::write(&layout, &renamed).expect("write the mutated layout module");
    let drifted = render_through_plugin(&copy, &capture, &version, &tree_keys);
    let fault = run_drift(&top_tree, &drifted, &spans, RunPane::Tree)
        .expect_err("a renamed pane header is drift, and this leg has to say so");
    assert!(
        fault.contains("Tree") && fault.contains("Trace"),
        "the failure names the pane header that moved and what it moved to: {fault}"
    );
    let banner = format!("--- {NAME}, {DRIFT_COLS}x{DRIFT_ROWS} ---");
    let (top_half, plugin_half) = fault.split_once(&banner).unwrap_or_else(|| {
        panic!("the failure prints the plugin's screen under its banner: {fault}")
    });
    assert!(
        top_half.contains(&format!("--- afkd top, {DRIFT_COLS}x{DRIFT_ROWS} ---"))
            && top_half.contains("監視")
            && plugin_half.contains("監視"),
        "the failure prints both screens, so a drift is diagnosable from the output alone: {fault}"
    );
}

// --- the divergence ledger: where web-top differs from afkd top on purpose -----------------

/// One place `@afkd/web-top` differs from `afkd top` **on purpose**: the surface and field
/// it covers, a phrase quoted off the `layout.mjs` line that intends it, and why.
///
/// `intent` is a verbatim phrase rather than a line number because a number goes stale on
/// every unrelated edit above it — and a stale one still "points" somewhere — while a phrase
/// either still occurs exactly once or the entry reddens ([`intent_line`]). Every failure
/// prints the line it resolves to today.
struct Divergence {
    surface: &'static str,
    field: &'static str,
    intent: &'static str,
    why: &'static str,
}

impl Divergence {
    /// The key [`observe`] files a difference under.
    fn key(&self) -> String {
        format!("{} · {}", self.surface, self.field)
    }
}

/// The plugin module every [`Divergence::intent`] is quoted off.
const LEDGER_SOURCE: &str = "layout.mjs";

/// The ledger: every field the plugin's screens read differently from the terminal's over
/// [`ledger_config`], and nothing else. The leg below **derives** the observed set and holds
/// it equal to this one, so a divergence the code grew reddens it as unlisted and one the
/// code closed reddens it as stale — this table is the contract, not a second copy of it.
const LEDGER: &[Divergence] = &[
    Divergence {
        surface: "Queues",
        field: "Queue",
        intent: "the tui's own `Queues` section has no room for it",
        why: "a lane reads `heavy (normal)`, the priority the terminal's section drops",
    },
    Divergence {
        surface: "Queues",
        field: "Slots origin",
        intent: "column is fit to whatever it costs",
        why: "the `Queue` column is fit to that longer identity, so `Slots` opens later; the \
              columns after it are right-anchored and do not move",
    },
    Divergence {
        surface: "Info",
        field: "Started",
        intent: "`Started` and `Last run` are **absent**, not forgotten",
        why: "a wall-clock row, and no time-of-day anchor crosses the control wire",
    },
    Divergence {
        surface: "Info",
        field: "Last run",
        intent: "`Started` and `Last run` are **absent**, not forgotten",
        why: "a wall-clock row, and no time-of-day anchor crosses the control wire",
    },
    Divergence {
        surface: "Info",
        field: "Last activity",
        intent: "The `Activity` section spends its rows on what the wire does",
        why: "the `Activity` section spends the rows those two vacate on an elapsed the wire \
              does carry",
    },
    Divergence {
        surface: "Info",
        field: "Next run",
        intent: "`infoview::next_run_phrase` minus its `HH:MM` prefix",
        why: "the countdown without the terminal's `HH:MM` projection, for the same missing \
              anchor",
    },
    Divergence {
        surface: "Info",
        field: "layout",
        intent: "The two **disagree**, and this surface follows the card",
        why: "a borderless two-column grid (`docs/tui-style.md` §1) against `render_info`'s \
              stacked boxes — and it goes two-up on `cols` where the terminal measures its \
              inset body, so at exactly 100 columns the two disagree on the column count too",
    },
    Divergence {
        surface: "Flat",
        field: "rows",
        intent: "The **flat** arm (`v`) is not modelled",
        why: "the page paints only the grouped tree, so `v` leaves the group header on screen",
    },
    Divergence {
        surface: "Flat",
        field: "footer",
        intent: "listed-but-inert (`keymap.mjs`'s `NOTES`)",
        why: "`v` is listed but inert, so the page's legend still offers `v flat`",
    },
];

/// The 1-based line of `intent`'s one occurrence in `source`, or why there is no such line —
/// a phrase that occurs twice names no line at all, which is as broken as one that is gone.
fn intent_line(source: &str, intent: &str) -> Result<usize, String> {
    let at: Vec<usize> = source.match_indices(intent).map(|(at, _)| at).collect();
    match at.as_slice() {
        [at] => Ok(source[..*at].matches('\n').count() + 1),
        _ => Err(format!(
            "`{intent}` occurs {} times in {LEDGER_SOURCE}, not once",
            at.len()
        )),
    }
}

/// Where `entry` points in the shipped tree today, for a failure message.
fn intent_at(entry: &Divergence) -> String {
    let source = std::fs::read_to_string(plugin_root().join(LEDGER_SOURCE))
        .unwrap_or_else(|err| panic!("read the plugin's {LEDGER_SOURCE}: {err}"));
    match intent_line(&source, entry.intent) {
        Ok(line) => format!("{LEDGER_SOURCE}:{line}"),
        Err(why) => why,
    }
}

/// The lane [`ledger_config`] declares. A bare `queue heavy` resolves to the `normal`
/// priority, which is the card's own `heavy (normal)`.
const LANE: &str = "heavy";

/// The service the ledger leg opens the info view on, and the keys that get both surfaces
/// there from the grouped board's first row — `janitor`, the `ops` header, `監視`, `backup`,
/// the grouped order the two share.
///
/// `ops::backup` because it is the lane's member, so the conditional `Queue` row is on the
/// page, and because the terminal's stacked boxes then just fit a 30-row screen (rows 2–27):
/// `janitor`'s and `監視`'s `About` band pushes `Total run time` off the bottom. A row the
/// terminal grows there clips a field off the end, which reads here as one-sided on the
/// plugin — red, with both screens printed.
const INFO_SERVICE: &str = "ops::backup";
const INFO_KEYS: &str = "gjjji";

/// The floors the ledger closes on and the terminal's captures wait for, the drift leg's
/// discipline: the flat arm lists the fixture's four services, and `ops::backup`'s info view
/// carries sixteen fields in five sections on the terminal and fifteen on the page. Floors,
/// so a field added to the fixture is not a reason to touch them.
const FLAT_ROW_FLOOR: usize = 4;
const INFO_FIELD_FLOOR: usize = 14;
const INFO_SECTIONS: usize = 5;

/// [`DRIFT_SERVICES`] with the one thing the drift leg keeps off its board: a lane, so both
/// surfaces draw a `Queues` section and the info view a `Queue` row.
fn ledger_config() -> String {
    const OPENER: &str = "service ops::backup {\n";
    // A fixture edit that renamed the service would otherwise drop the lane in silence, and
    // the `Queues` entries would then read as stale for a reason neither surface is at.
    assert_eq!(
        DRIFT_SERVICES.matches(OPENER).count(),
        1,
        "`DRIFT_SERVICES` opens `ops::backup` once; the ledger's lane is no longer aimed at it"
    );
    let services = DRIFT_SERVICES.replace(OPENER, &format!("{OPENER}  queue {LANE}\n"));
    format!("queue {LANE} {{\n  parallelism 2\n}}\n\n{services}\nplugin {NAME} {{\n  port 0\n}}\n")
}

impl Screen {
    /// The `Queues` section's column header — the first row whose leading segment is
    /// `Queue` — as [`column_header`](Self::column_header) reads the list's.
    fn queue_header_row(&self) -> Option<usize> {
        (0..self.grid.len()).find(|&at| {
            segments(&self.grid[at])
                .first()
                .is_some_and(|(label, _)| label == "Queue")
        })
    }

    /// The lane rows: everything between the `Queues` header and the first blank row under it.
    fn queue_rows(&self) -> Vec<&[String]> {
        let Some(header) = self.queue_header_row() else {
            return Vec::new();
        };
        (header + 1..self.grid.len())
            .take_while(|&at| !self.row_text(at).trim().is_empty())
            .map(|at| self.grid[at].as_slice())
            .collect()
    }
}

/// Every field the two surfaces read differently, keyed `Surface · field`, each with a
/// one-line account of the two readings for the failure message.
type Observed = BTreeMap<String, String>;

/// File `surface · field` in `out` when the two readings differ.
fn note<T: PartialEq + std::fmt::Debug>(
    out: &mut Observed,
    surface: &str,
    field: &str,
    top: T,
    plugin: T,
) {
    if top != plugin {
        out.insert(
            format!("{surface} · {field}"),
            format!("afkd top: {top:?} · web-top: {plugin:?}"),
        );
    }
}

/// The overview-shaped comparison, as a **set** rather than [`drift`]'s first failure: the
/// header line, the load strip's identity, the column header, the row count, each column's
/// cells, and the footer's hints. Run for the grouped board and for the flat arm.
///
/// A row count that differs skips the per-column compare — every column would differ for
/// that one reason, and the ledger names it once.
fn list_divergences(surface: &str, top: &Screen, plugin: &Screen, out: &mut Observed) {
    note(
        out,
        surface,
        "header line",
        header_shape(&top.grid[0]),
        header_shape(&plugin.grid[0]),
    );
    let strip = |screen: &Screen| screen.row_text(1).trim_start().starts_with("CPU");
    note(out, surface, "load strip", strip(top), strip(plugin));
    let (top_header, plugin_header) = (top.column_header(), plugin.column_header());
    note(out, surface, "columns", &top_header, &plugin_header);
    let (top_rows, plugin_rows) = (top.service_rows(), plugin.service_rows());
    note(out, surface, "rows", top_rows.len(), plugin_rows.len());
    if let Some(header) = top_header.filter(|header| Some(header) == plugin_header.as_ref()) {
        if top_rows.len() == plugin_rows.len() {
            for (label, span) in column_spans(&header, usize::from(DRIFT_COLS)) {
                let cells = |rows: &[&[String]]| -> Vec<String> {
                    rows.iter()
                        .map(|row| column_cell(row, &label, span))
                        .collect()
                };
                note(out, surface, &label, cells(&top_rows), cells(&plugin_rows));
            }
        }
    }
    note(
        out,
        surface,
        "footer",
        top.footer_hints(),
        plugin.footer_hints(),
    );
}

/// The `Queues` section: its labels, each label's origin, the lane-row count, and each
/// column's cells — each surface sliced by **its own** header's spans, since the plugin's
/// wider `Queue` column would otherwise leak `(normal)` into the terminal's `Slots` span and
/// report a `Slots` difference that is really the `Queue` one.
fn queue_divergences(top: &Screen, plugin: &Screen, out: &mut Observed) {
    let header = |screen: &Screen| {
        screen
            .queue_header_row()
            .map(|at| segments(&screen.grid[at]))
    };
    let (top_header, plugin_header) = (header(top), header(plugin));
    let labels = |header: &Option<Vec<(String, usize)>>| -> Option<Vec<String>> {
        header
            .as_ref()
            .map(|header| header.iter().map(|(label, _)| label.clone()).collect())
    };
    let (top_labels, plugin_labels) = (labels(&top_header), labels(&plugin_header));
    let (top_rows, plugin_rows) = (top.queue_rows(), plugin.queue_rows());
    note(out, "Queues", "rows", top_rows.len(), plugin_rows.len());
    if top_labels != plugin_labels {
        note(out, "Queues", "header", top_labels, plugin_labels);
        return;
    }
    let (Some(top_header), Some(plugin_header)) = (top_header, plugin_header) else {
        return;
    };
    for ((label, mine), (_, theirs)) in top_header.iter().zip(&plugin_header) {
        note(out, "Queues", &format!("{label} origin"), mine, theirs);
    }
    if top_rows.len() != plugin_rows.len() {
        return;
    }
    let cols = usize::from(DRIFT_COLS);
    let (top_spans, plugin_spans) = (
        column_spans(&top_header, cols),
        column_spans(&plugin_header, cols),
    );
    for ((label, mine), (_, theirs)) in top_spans.iter().zip(&plugin_spans) {
        let cells = |rows: &[&[String]], span: (usize, usize)| -> Vec<String> {
            rows.iter().map(|row| cell_text(row, span)).collect()
        };
        note(
            out,
            "Queues",
            label,
            cells(&top_rows, *mine),
            cells(&plugin_rows, *theirs),
        );
    }
}

/// The glyphs `shell::render_info` draws its panels in.
const BOX_GLYPHS: [char; 6] = ['┌', '┐', '└', '┘', '│', '─'];

/// One info screen read as a page: its title, its sections in reading order with the column
/// each sits in, its fields by label, whether it is drawn in boxes, and its footer's hints.
///
/// A model rather than a cell diff because the two info screens share no geometry at all —
/// the terminal stacks bordered boxes, the page lays an open grid — so the only honest
/// comparison is of what each *says*, with the shape reported as one field of its own.
struct InfoPage {
    title: String,
    sections: Vec<(String, usize)>,
    fields: BTreeMap<String, String>,
    boxed: bool,
    footer: Vec<String>,
}

impl InfoPage {
    /// The page's shape as one comparable line — `boxed; col0: Overview, Trigger` — the
    /// section headers in reading order, column by column.
    fn layout(&self) -> String {
        let columns = self
            .sections
            .iter()
            .map(|(_, at)| at + 1)
            .max()
            .unwrap_or(0);
        let mut shape = String::from(if self.boxed { "boxed" } else { "open" });
        for column in 0..columns {
            let names: Vec<&str> = self
                .sections
                .iter()
                .filter(|(_, at)| *at == column)
                .map(|(name, _)| name.as_str())
                .collect();
            shape += &format!("; col{column}: {}", names.join(", "));
        }
        shape
    }
}

/// One segment of an info row, and whether it is a section header.
struct InfoSegment {
    text: String,
    at: usize,
    header: bool,
}

/// The terminal's info screen: a `┌ Title ──┐` row opens a box, its `┌` column plus one is
/// the box's content origin, and the header is the segment two cells right of the `┌`. Every
/// box glyph is blanked before the row is segmented, so a border reads as background and a
/// bottom edge as a blank row.
fn read_boxed_info(screen: &Screen) -> InfoPage {
    let mut origins = BTreeSet::new();
    let mut rows = Vec::new();
    for at in 2..screen.footer_start() {
        let row = &screen.grid[at];
        let corners: Vec<usize> = (0..row.len()).filter(|&col| row[col] == "┌").collect();
        origins.extend(corners.iter().map(|corner| corner + 1));
        let blanked: Vec<String> = row
            .iter()
            .map(|cell| {
                if !cell.is_empty() && cell.chars().all(|c| BOX_GLYPHS.contains(&c)) {
                    " ".to_string()
                } else {
                    cell.clone()
                }
            })
            .collect();
        rows.push(
            segments(&blanked)
                .into_iter()
                .map(|(text, at)| InfoSegment {
                    header: corners.iter().any(|corner| corner + 2 == at),
                    text,
                    at,
                })
                .collect(),
        );
    }
    fold_info(screen, true, rows, origins.into_iter().collect())
}

/// The page's info screen: an open grid with no borders to key off, so a section header is a
/// segment whose text is one of the **terminal's** box titles, and its origin is the origin of
/// the column it heads. A section the page renamed then reads as a field — and surfaces as a
/// `layout` difference and a one-sided field, which is the red it should be.
fn read_open_info(screen: &Screen, titles: &[String]) -> InfoPage {
    let mut origins = BTreeSet::new();
    let mut rows = Vec::new();
    for at in 2..screen.footer_start() {
        let row: Vec<InfoSegment> = segments(&screen.grid[at])
            .into_iter()
            .map(|(text, at)| InfoSegment {
                header: titles.contains(&text),
                text,
                at,
            })
            .collect();
        origins.extend(row.iter().filter(|seg| seg.header).map(|seg| seg.at));
        rows.push(row);
    }
    fold_info(screen, false, rows, origins.into_iter().collect())
}

/// Fold segmented info rows into an [`InfoPage`], the one reading both surfaces share.
///
/// Each segment belongs to the column with the greatest content origin at or left of it. Per
/// column and row, a lone header opens a section; a row whose first segment sits at the
/// column's origin is a field — its label that segment, its value the rest joined by one
/// space, which reads a `Runs … Tokens …` pair row the same on both surfaces; anything else
/// is a **wrap continuation** of that column's last field (the page wraps a long value where
/// the terminal ellipsizes one).
fn fold_info(
    screen: &Screen,
    boxed: bool,
    rows: Vec<Vec<InfoSegment>>,
    origins: Vec<usize>,
) -> InfoPage {
    let mut sections = Vec::new();
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Vec<Option<String>> = vec![None; origins.len().max(1)];
    for row in rows {
        let mut columns: Vec<Vec<InfoSegment>> = (0..last.len()).map(|_| Vec::new()).collect();
        for seg in row {
            let column = origins
                .iter()
                .rposition(|&origin| origin <= seg.at)
                .unwrap_or(0);
            columns[column].push(seg);
        }
        for (column, segs) in columns.into_iter().enumerate() {
            let origin = origins.get(column).copied().unwrap_or(0);
            let Some(first) = segs.first() else {
                continue;
            };
            let rest = || -> String {
                segs[1..]
                    .iter()
                    .map(|seg| seg.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            if first.header && segs.len() == 1 {
                sections.push((first.text.clone(), column));
                last[column] = None;
            } else if first.at == origin {
                fields.insert(first.text.clone(), rest());
                last[column] = Some(first.text.clone());
            } else {
                let more: Vec<&str> = segs.iter().map(|seg| seg.text.as_str()).collect();
                let more = more.join(" ");
                match last[column]
                    .as_ref()
                    .and_then(|label| fields.get_mut(label))
                {
                    Some(value) => {
                        value.push(' ');
                        value.push_str(&more);
                    }
                    None => {
                        fields.insert(more, String::new());
                    }
                }
            }
        }
    }
    InfoPage {
        title: normalise(screen.row_text(0).trim()),
        sections,
        fields,
        boxed,
        footer: screen.footer_hints(),
    }
}

/// [`normalise`] for a prose value: a token's leading `(` and trailing `)`/`,` peeled first,
/// and a run of adjacent durations collapsed to one `<t>` — so `(in 59m 55s)` and `in 1h`
/// read alike wherever the two captures straddle a unit boundary. [`normalise`] itself keeps
/// the drift legs' exact semantics.
fn normalise_phrase(text: &str) -> String {
    let peeled: Vec<&str> = text
        .split_whitespace()
        .map(|token| token.trim_start_matches('(').trim_end_matches([')', ',']))
        .filter(|token| !token.is_empty())
        .collect();
    let mut words: Vec<String> = Vec::new();
    for word in normalise(&peeled.join(" ")).split(' ') {
        if word == "<t>" && words.last().is_some_and(|prev| prev == "<t>") {
            continue;
        }
        words.push(word.to_string());
    }
    words.join(" ")
}

/// The two info screens read as pages — the terminal's boxes first, since its titles are
/// what the page's open grid is read against.
fn info_pages(top: &Screen, plugin: &Screen) -> (InfoPage, InfoPage) {
    let top = read_boxed_info(top);
    let titles: Vec<String> = top.sections.iter().map(|(name, _)| name.clone()).collect();
    let plugin = read_open_info(plugin, &titles);
    (top, plugin)
}

/// The info view: its title, its footer, its shape, and every field by label — present on
/// one side only, or read differently once [`normalise_phrase`]d.
fn info_divergences(top: &InfoPage, plugin: &InfoPage, out: &mut Observed) {
    note(out, "Info", "title", &top.title, &plugin.title);
    note(out, "Info", "footer", &top.footer, &plugin.footer);
    note(out, "Info", "layout", top.layout(), plugin.layout());
    let labels: BTreeSet<&String> = top.fields.keys().chain(plugin.fields.keys()).collect();
    for label in labels {
        let value = |page: &InfoPage| page.fields.get(label).map(|value| normalise_phrase(value));
        note(out, "Info", label, value(top), value(plugin));
    }
}

/// One surface's three screens: the grouped overview (which carries the `Queues` section),
/// the flat arm, and `INFO_SERVICE`'s info view.
struct Captured {
    overview: Screen,
    flat: Screen,
    info: Screen,
}

/// Every field the two surfaces read differently, over all three screens.
fn observe(top: &Captured, plugin: &Captured) -> Observed {
    let mut out = Observed::new();
    list_divergences("Overview", &top.overview, &plugin.overview, &mut out);
    queue_divergences(&top.overview, &plugin.overview, &mut out);
    list_divergences("Flat", &top.flat, &plugin.flat, &mut out);
    let (top_info, plugin_info) = info_pages(&top.info, &plugin.info);
    info_divergences(&top_info, &plugin_info, &mut out);
    out
}

/// Render `frames` through the plugin tree at `plugin` onto the three screens the ledger
/// compares, each through the keys that reach it.
fn capture_plugin(plugin: &Path, frames: &Path, version: &str) -> Captured {
    Captured {
        overview: render_through_plugin(plugin, frames, version, "g"),
        flat: render_through_plugin(plugin, frames, version, "v"),
        info: render_through_plugin(plugin, frames, version, INFO_KEYS),
    }
}

/// Why the observed divergence set is not the ledger's: the keys observed and not listed,
/// the keys listed and not observed, and the sentence that says so with every screen pair.
#[derive(Debug)]
struct LedgerFault {
    unlisted: Vec<String>,
    stale: Vec<String>,
    message: String,
}

/// Hold `observed` to [`LEDGER`], then to the floors that keep an empty reading from agreeing
/// with an empty half of it. Every message carries all three screen pairs.
fn check_ledger(observed: &Observed, top: &Captured, plugin: &Captured) -> Result<(), LedgerFault> {
    let screens: String = [
        ("Overview", &top.overview, &plugin.overview),
        ("Flat", &top.flat, &plugin.flat),
        ("Info", &top.info, &plugin.info),
    ]
    .into_iter()
    .map(|(surface, mine, theirs)| {
        format!(
            "\n\n=== {surface} ===\n--- afkd top, {DRIFT_COLS}x{DRIFT_ROWS} ---\n{}\n\n--- {NAME}, {DRIFT_COLS}x{DRIFT_ROWS} ---\n{}",
            mine.render(),
            theirs.render()
        )
    })
    .collect();
    let fault = |message: String| LedgerFault {
        unlisted: Vec::new(),
        stale: Vec::new(),
        message: format!("{message}{screens}"),
    };

    let listed: BTreeSet<String> = LEDGER.iter().map(Divergence::key).collect();
    let unlisted: Vec<String> = observed
        .keys()
        .filter(|key| !listed.contains(*key))
        .cloned()
        .collect();
    let stale: Vec<String> = LEDGER
        .iter()
        .map(Divergence::key)
        .filter(|key| !observed.contains_key(key))
        .collect();
    if !unlisted.is_empty() || !stale.is_empty() {
        let mut message = String::from("web-top's divergences from afkd top are not the ledger's");
        for key in &unlisted {
            message += &format!(
                "\n  unlisted `{key}` — {}; ledger it with the line that intends it, or close it",
                observed[key]
            );
        }
        for entry in LEDGER.iter().filter(|entry| stale.contains(&entry.key())) {
            message += &format!(
                "\n  stale `{}` ({}: {}) — the two surfaces now agree here; drop the entry",
                entry.key(),
                intent_at(entry),
                entry.why
            );
        }
        return Err(LedgerFault {
            unlisted,
            stale,
            message: format!("{message}{screens}"),
        });
    }

    // …and the floors, last, as [`drift`] closes: a reader that saw nothing on both sides
    // agrees with itself, and would pass every entry that is not about that screen.
    for (whose, screens) in [("afkd top", top), ("web-top", plugin)] {
        let rows = screens.overview.service_rows().len();
        if rows < SERVICE_ROW_FLOOR {
            return Err(fault(format!(
                "{whose}'s overview listed {rows} service rows; the fixture ships {SERVICE_ROW_FLOOR}"
            )));
        }
        if screens.overview.queue_rows().is_empty() {
            return Err(fault(format!(
                "{whose}'s overview drew no lane row, so the `Queues` entries were never compared"
            )));
        }
        let rows = screens.flat.service_rows().len();
        if rows < FLAT_ROW_FLOOR {
            return Err(fault(format!(
                "{whose}'s flat arm listed {rows} rows; the fixture ships {FLAT_ROW_FLOOR} services"
            )));
        }
    }
    let (top_info, plugin_info) = info_pages(&top.info, &plugin.info);
    for (whose, page) in [("afkd top", &top_info), ("web-top", &plugin_info)] {
        if page.fields.len() < INFO_FIELD_FLOOR || page.sections.len() != INFO_SECTIONS {
            return Err(fault(format!(
                "{whose}'s info view read as {} fields in {} sections ({:?}); `{INFO_SERVICE}` carries at least {INFO_FIELD_FLOOR} in {INFO_SECTIONS}",
                page.fields.len(),
                page.sections.len(),
                page.layout()
            )));
        }
    }
    Ok(())
}

#[test]
fn web_tops_divergences_from_afkd_top_are_exactly_the_ledgers() {
    // The overview leg covers the one surface the two agree on, and is silent — by scope —
    // about the three places the plugin diverges on purpose: the `Queues` section's lane
    // priority, the info view, and the flat arm. So a *fourth* divergence in those places
    // would redden nothing. This leg reads all three screens off both surfaces for one
    // fixture, collects every field that differs, and holds that set **equal** to
    // [`LEDGER`]: a new divergence is unlisted, a closed one is stale, and both are red.
    if !python3_available() {
        eprintln!("skipping the ledger leg: python3 is not on PATH, and the relay that carries the frames is python");
        return;
    }
    if !node_available() {
        eprintln!(
            "skipping the ledger leg: node is not on PATH, and the plugin's renderer is javascript"
        );
        return;
    }
    let (_daemon, dir, port) = daemon_serving(&ledger_config());
    let scratch = TempDir::new().expect("a tempdir for the capture");

    // The overview leg's order, for its reasons: the snapshot lands before the stop, so the
    // fold reaches the same stopped board the terminal's later attach starts from.
    let mut sse = Sse::admitted(port);
    let (id, welcome) = sse.handshake_with_welcome();
    let version = welcome["daemon"]
        .as_str()
        .unwrap_or_else(|| panic!("the welcome names the daemon's version: {welcome}"))
        .to_string();
    let mut frames: Vec<String> = Vec::new();
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["meta"] == "snapshot"),
        "the attach snapshot never arrived; the stream carried {frames:?}"
    );
    let (status, body) = post_command(
        port,
        &serde_json::json!({"stream": id, "command": "stop", "service": STOPPED}),
    );
    assert_eq!(status, 200, "POST /command stop: {body}");
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["event"] == "service_paused"
            && f["service"] == STOPPED
            && f["paused"] == true),
        "the stop never settled on the stream; it carried {frames:?}"
    );

    // `afkd top` boots **flat** on a fresh home, which is the one screen of the three only it
    // can paint — so it is caught first, before any `v`. A daemon that ever booted grouped
    // fails this wait with the grouped screen printed rather than comparing the wrong arm.
    let tui = spawn_captured_sized(dir.path(), &["top"], &[], DRIFT_COLS, DRIFT_ROWS);
    let flat = settled(&tui, "the flat arm", |screen| {
        screen.column_header().is_some()
            && screen.service_rows().len() >= FLAT_ROW_FLOOR
            && !screen.render().contains('├')
            && screen.footer_hints().len() >= FOOTER_HINT_FLOOR
            && !screen.row_text(1).trim().is_empty()
    });
    tui.send(b"v");
    if !tui.wait_until(Duration::from_secs(5), |s| s.contains('├')) {
        tui.send(b"v");
        assert!(
            tui.wait_until(BUDGET, |s| s.contains('├')),
            "two `v` presses and the terminal is still not on the grouped tree:\n{}",
            tui.screen()
        );
    }
    tui.send(b"g");
    let overview = settled(&tui, "a board with a `Queues` section", |screen| {
        board_settled(screen) && !screen.queue_rows().is_empty()
    });
    for key in INFO_KEYS.trim_start_matches('g').bytes() {
        tui.send(&[key]);
    }
    let info = settled(&tui, &format!("`{INFO_SERVICE}`'s info view"), |screen| {
        let page = read_boxed_info(screen);
        screen.row_text(0).contains(INFO_SERVICE)
            && page.fields.len() >= INFO_FIELD_FLOOR
            && page.sections.len() == INFO_SECTIONS
            && !page.footer.is_empty()
    });
    let terminal = Captured {
        overview,
        flat,
        info,
    };
    assert!(
        sse.collect_frames(BUDGET, &mut frames, |f| f["meta"] == "host_load"),
        "no host reading crossed the stream; the capture carried {} frames",
        frames.len()
    );
    let capture = scratch.path().join("frames.jsonl");
    std::fs::write(&capture, frames.join("\n") + "\n").expect("write the captured frames");

    let page = capture_plugin(&plugin_root(), &capture, &version);
    if let Err(fault) = check_ledger(&observe(&terminal, &page), &terminal, &page) {
        panic!("{}", fault.message);
    }

    // The two negative arms, on the same capture and terminal screens, each on a **copy** of
    // the plugin — one per direction the ledger can be wrong in. Each rewrite proves it
    // landed once, in the declaration, or the arm would compare the shipped tree against
    // itself and call the agreement a refusal.
    let mutate = |from: &str, to: &str| -> Captured {
        let mutated = TempDir::new().expect("a tempdir for the mutated copy");
        let copy = mutated.path().join("web-top");
        copy_tree(&plugin_root(), &copy);
        let layout = copy.join(LEDGER_SOURCE);
        let before = std::fs::read_to_string(&layout).expect("the copy's layout module");
        assert_eq!(
            before.matches(from).count(),
            1,
            "`{LEDGER_SOURCE}` spells `{from}` once; this arm's rewrite is no longer aimed at \
             the one declaration"
        );
        std::fs::write(&layout, before.replace(from, to)).expect("write the mutated module");
        capture_plugin(&copy, &capture, &version)
    };

    // A fourth divergence: an info label renamed on the page only. `Faults` is shorter than
    // `Diagnostics (swallowed)`, so the page-wide label column does not move and the rename
    // is the only thing that did — read as one field the page lost and one it grew.
    let renamed = mutate(r#"field("Failures", "#, r#"field("Faults", "#);
    let fault = check_ledger(&observe(&terminal, &renamed), &terminal, &renamed)
        .expect_err("a divergence the ledger does not name has to redden it");
    assert_eq!(
        (fault.unlisted.as_slice(), fault.stale.as_slice()),
        (
            ["Info · Failures".to_string(), "Info · Faults".to_string()].as_slice(),
            [].as_slice()
        ),
        "the fault names exactly the field that moved and what it moved to: {}",
        fault.message
    );
    let banner = format!("--- {NAME}, {DRIFT_COLS}x{DRIFT_ROWS} ---");
    let (top_half, plugin_half) = fault.message.split_once(&banner).unwrap_or_else(|| {
        panic!(
            "the failure prints the plugin's screens under its banner: {}",
            fault.message
        )
    });
    assert!(
        top_half.contains(&format!("--- afkd top, {DRIFT_COLS}x{DRIFT_ROWS} ---"))
            && top_half.contains("監視")
            && plugin_half.contains("監視")
            && plugin_half.contains("Faults"),
        "the failure prints both surfaces' screens, so it is diagnosable from the output alone: {}",
        fault.message
    );

    // A closed divergence: the lane's priority dropped from the page's identity. The `Queue`
    // column is then fit to the bare name, exactly as the terminal's is, and both `Queues`
    // entries describe a difference the code no longer has.
    let closed = mutate(
        "return lane.priority === \"\" ? lane.lane : `${lane.lane} (${lane.priority})`;",
        "return lane.lane;",
    );
    let fault = check_ledger(&observe(&terminal, &closed), &terminal, &closed)
        .expect_err("a divergence the code closed has to redden the ledger that still names it");
    assert_eq!(
        (fault.unlisted.as_slice(), fault.stale.as_slice()),
        (
            [].as_slice(),
            [
                "Queues · Queue".to_string(),
                "Queues · Slots origin".to_string()
            ]
            .as_slice()
        ),
        "the fault names exactly the two entries the code closed: {}",
        fault.message
    );
    assert!(
        fault.message.contains(&format!("{LEDGER_SOURCE}:")),
        "a stale entry is printed with the line that intended it: {}",
        fault.message
    );
}

#[test]
fn every_ledger_entry_names_a_line_that_intends_it() {
    // The ledger's third column, checked where it can be without node or python: each
    // `intent` still occurs exactly once in the module it quotes, so an entry cannot outlive
    // the comment that justified it, and no two entries claim one field.
    let source = std::fs::read_to_string(plugin_root().join(LEDGER_SOURCE))
        .expect("the plugin's layout module");
    let mut keys = BTreeSet::new();
    for entry in LEDGER {
        if let Err(why) = intent_line(&source, entry.intent) {
            panic!("the ledger's `{}` entry names no line: {why}", entry.key());
        }
        assert!(
            keys.insert(entry.key()),
            "the ledger names `{}` twice",
            entry.key()
        );
    }
    // The card's three: the `Queues` section, the info view and the flat arm each have an
    // entry, so a ledger trimmed to agree with a broken reader cannot drop a whole surface.
    for surface in ["Queues", "Info", "Flat"] {
        assert!(
            LEDGER.iter().any(|entry| entry.surface == surface),
            "the ledger covers the `{surface}` surface"
        );
    }
}

/// The field names of a `view` object literal, read out of `source` between `opener` and
/// `closer` — the two spellings the page and the driver each use for the same shape.
fn view_fields(source: &str, opener: &str, closer: &str) -> Vec<String> {
    let body = source
        .split_once(opener)
        .unwrap_or_else(|| panic!("this source has no `{opener}`"))
        .1
        .split_once(closer)
        .unwrap_or_else(|| panic!("the view literal after `{opener}` never closes"))
        .0;
    let mut fields: Vec<String> = body
        .lines()
        .filter_map(|line| {
            let name = line.trim().split([':', ',']).next()?.to_string();
            (!name.is_empty() && name.chars().all(char::is_alphanumeric)).then_some(name)
        })
        .collect();
    fields.sort();
    fields
}

#[test]
fn the_drift_drivers_view_matches_the_pages_own() {
    // The drift leg only means anything while its driver reads `layout()` through the **same**
    // view the page does. Nothing enforces that at run time: an option `top.mjs` grows and
    // `web_top_screen.mjs` does not would simply render a different screen, and the leg would
    // report it as the terminal disagreeing with the plugin — blaming the two surfaces for a
    // gap in the harness between them. So the two field lists are held against each other
    // here, textually, which is also why this leg needs neither node nor python and runs
    // wherever `cargo test` does.
    let page = std::fs::read_to_string(plugin_root().join("top.mjs")).expect("the page's shell");
    let driver = std::fs::read_to_string(screen_driver()).expect("the drift leg's driver");
    let theirs = view_fields(
        &page,
        "function view(cols, rows, now) {\n  return {\n",
        "\n  };",
    );
    let ours = view_fields(&driver, "const view = () => ({\n", "\n});");
    assert_eq!(
        ours, theirs,
        "`web_top_screen.mjs`'s view bag has drifted from `top.mjs`'s; copy the page's field \
         for field, or the drift leg is comparing two different screens"
    );
    // A floor, so a parse that matched nothing cannot agree with another parse that matched
    // nothing: the bag carries the viewport, the clock, the version and the whole session.
    assert!(
        theirs.len() >= 13,
        "the page's view bag reads as only {theirs:?}; the opener this scan keys off has moved"
    );
}
