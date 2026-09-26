//! Speaking the plugin wire to the real `afkd-trello` exec, as afkd does: one child, one
//! JSON request line at a time on its stdin, one reply line read back off its stdout,
//! every read bounded so a wedged child fails the test rather than hanging it.

#![allow(dead_code)]

pub mod fake;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long one call may take. afkd's own deadline is 60 s; a claim here costs one
/// second of settle and a handful of loopback round trips.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// afkd's line cap, newline included.
pub const MAX_LINE: usize = 64 * 1024;

/// The service the ordinary legs arm as.
pub const DEVELOP: &str = "afkd::develop";

/// The second service on the same board, in the park-owner legs.
pub const DISCUSS: &str = "afkd::discuss";

/// This afkd process's claim owner, as `hello` carries it.
pub const OWNER: &str = "afkd-4242";

/// One running plugin.
pub struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    /// Requests written, and replies read — equal whenever no call is outstanding.
    asked: usize,
    answered: usize,
}

impl Plugin {
    /// Spawn the exec cargo built for this test run.
    pub fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_afkd-trello"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn afkd-trello");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let err = child.stderr.take().expect("piped stderr");
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            stderr,
            asked: 0,
            answered: 0,
        }
    }

    /// Spawn and greet with `settings` as `afkd::develop`, asserting it arms.
    pub fn armed(settings: Value) -> Self {
        Self::armed_as(DEVELOP, settings)
    }

    /// Spawn and greet with `settings` as `service`, asserting it arms.
    pub fn armed_as(service: &str, settings: Value) -> Self {
        let mut plugin = Self::spawn();
        let reply = plugin.hello_as(service, settings);
        assert_eq!(reply["ok"], true, "hello refused: {}", plugin.stderr());
        plugin
    }

    /// Greet as today's afkd does, as `afkd::develop`.
    pub fn hello(&mut self, settings: Value) -> Value {
        self.hello_as(DEVELOP, settings)
    }

    /// Greet as today's afkd does: the kind and its settings, and the service's name,
    /// the daemon's roster (both services this board runs) and the claim owner beside
    /// them.
    pub fn hello_as(&mut self, service: &str, settings: Value) -> Value {
        self.call(json!({"call": "hello", "proto": 1, "kind": "trello",
                         "service": service, "roster": [DEVELOP, DISCUSS],
                         "owner": OWNER, "settings": settings}))
    }

    /// Write one request and read its reply line: under afkd's cap, newline-terminated,
    /// one JSON object.
    pub fn call(&mut self, request: Value) -> Value {
        let line = self.call_raw(request);
        assert!(
            line.len() <= MAX_LINE,
            "a reply line over afkd's cap: {}",
            line.len()
        );
        assert!(line.ends_with('\n'), "an unterminated reply: {line:?}");
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| {
            panic!("stdout carried a line that is not a JSON reply ({e}): {line:?}")
        });
        assert!(reply.is_object(), "{line}");
        reply
    }

    /// Write one request and read the raw reply line.
    pub fn call_raw(&mut self, request: Value) -> String {
        self.send(&request);
        let line = self.lines.recv_timeout(CALL_TIMEOUT).unwrap_or_else(|_| {
            panic!(
                "no reply to {request} within {CALL_TIMEOUT:?}; stderr: {}",
                self.stderr()
            )
        });
        self.answered += 1;
        line
    }

    /// Write one request without waiting for anything.
    pub fn send(&mut self, request: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{request}").expect("write a request");
        stdin.flush().expect("flush a request");
        self.asked += 1;
    }

    pub fn poll(&mut self) -> Value {
        self.call(json!({"call": "poll"}))
    }

    /// Everything the child has written to stderr so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Wait until stderr contains `needle`, for a line written just before an exit.
    pub fn stderr_soon(&self, needle: &str) -> String {
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let text = self.stderr();
            if text.contains(needle) || Instant::now() > deadline {
                return text;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until stderr holds at least `n` lines, for lines that share their text — a
    /// needle is found at its first copy.
    pub fn stderr_lines_soon(&self, n: usize) -> String {
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let text = self.stderr();
            if text.lines().count() >= n || Instant::now() > deadline {
                return text;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the child to exit on its own.
    pub fn exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait on the child") {
                return status;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Close stdin as afkd does when a service disarms, and hold the child to the
    /// wire's discipline: it exits cleanly, and stdout carried exactly one line per
    /// request and nothing else.
    pub fn finish(mut self) {
        self.stdin.take();
        let status = self.exit();
        assert!(
            status.success(),
            "exit {status:?}; stderr: {}",
            self.stderr()
        );
        assert_eq!(self.asked, self.answered, "a request went unanswered");
        let extra: Vec<String> = self.lines.try_iter().collect();
        assert!(
            extra.is_empty(),
            "stdout carried lines no request asked for: {extra:?}"
        );
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch directory standing in for an attempt's, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("afkd-trello-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create a temp dir");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
