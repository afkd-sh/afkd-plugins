//! Speaking the plugin wire to the real `afkd-afkd_board` exec, as afkd does: a child started for
//! one call, one JSON request line on its stdin, one reply line read back off its stdout,
//! every read bounded so a wedged child fails the test rather than hanging it.

#![allow(dead_code)]

pub mod fake;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long one call may take: a handful of loopback round trips.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// afkd's cap on a call's reply, newline included.
pub const MAX_CALL_LINE: usize = 8 * 1024 * 1024;

/// One running plugin.
pub struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    /// The thread copying stderr in, which ends once the child has closed it.
    stderr_reader: Option<thread::JoinHandle<()>>,
    /// Requests written, and replies read — equal whenever no call is outstanding.
    asked: usize,
    answered: usize,
}

impl Plugin {
    /// Spawn the exec cargo built for this test run.
    pub fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_afkd-afkd_board"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn afkd-afkd_board");
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
        let stderr_reader = thread::spawn(move || {
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
            stderr_reader: Some(stderr_reader),
            asked: 0,
            answered: 0,
        }
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

    /// Everything the child has written to stderr so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
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

    /// Wait for the child to exit and for its stderr to close: what it wrote there comes
    /// back with its status, and stdout must have carried nothing.
    pub fn exit_unanswered(mut self) -> (ExitStatus, String) {
        let status = self.exit();
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("the stderr reader");
        }
        let extra: Vec<String> = self.lines.try_iter().collect();
        assert!(extra.is_empty(), "stdout carried {extra:?}");
        (status, self.stderr())
    }

    /// Close stdin as afkd does once a call is answered, and hold the child to the wire's
    /// discipline: it exits cleanly, and stdout carried exactly one line per request and
    /// nothing else. Everything it wrote to stderr comes back.
    pub fn finish(mut self) -> String {
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
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("the stderr reader");
        }
        self.stderr()
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `line` read as one reply: no longer than `cap`, newline-terminated, one JSON object.
pub fn reply_of(line: &str, cap: usize) -> Value {
    assert!(
        line.len() <= cap,
        "a reply line over afkd's cap of {cap}: {}",
        line.len()
    );
    assert!(line.ends_with('\n'), "an unterminated reply: {line:?}");
    let reply: Value = serde_json::from_str(line).unwrap_or_else(|e| {
        panic!("stdout carried a line that is not a JSON reply ({e}): {line:?}")
    });
    assert!(reply.is_object(), "{line}");
    reply
}

/// Call the action `action` with `args` as afkd does: a child started for the one call, sent
/// the one `call` with the protocol, an empty settings file and the plugin's directory, and no
/// `hello`. The child is held to the wire: it answers one line under afkd's 8 MiB, writes
/// nothing else to stdout, and exits cleanly once its stdin closes. The reply comes back with
/// everything it wrote to stderr.
pub fn once(action: &str, args: Value) -> (Value, String) {
    let mut plugin = Plugin::spawn();
    let state_dir = std::env::temp_dir().join("afkd-afkd_board-state");
    let line = plugin.call_raw(json!({"call": "call", "proto": 2, "action": action,
                                      "args": args, "settings": {}, "state_dir": state_dir}));
    let reply = reply_of(&line, MAX_CALL_LINE);
    (reply, plugin.finish())
}

/// An `afkd_board.Board` reaching `fake` with its key, every field sent as afkd sends a struct.
pub fn board(fake: &fake::FakeBoard) -> Value {
    json!({"key": fake.key(), "base_url": fake.base_url()})
}
