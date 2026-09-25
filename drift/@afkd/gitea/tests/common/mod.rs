//! The harness the drift gate drives afkd through. Every spawn runs the `afkd` first on
//! `PATH` ([`bin_path`]) with `$HOME` pinned to a temp dir, so nothing here builds afkd or
//! reaches the operator's own daemon. It is the headless half of `@afkd/web-top`'s harness,
//! copied rather than shared: this gate paints no screen, so it carries none of the PTY.
//!
//! The Gitea the plugin talks to is [`fake`], the stateful loopback fake the plugin's own
//! wire suite runs against — included from the plugin's tree, never copied, so the two
//! suites cannot come to disagree about what Gitea does.
//!
//! ## No hangs on failure
//!
//! Every wait is bounded, and every real spawn is held in a [`Daemon`] that kills and reaps it
//! on drop, so a failing leg fails loudly rather than hanging the test binary or leaking an
//! afkd that outlives the run.

#[path = "../../../../../@afkd/gitea/tests/common/fake.rs"]
pub mod fake;

use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The `afkd` this gate holds the plugin to: the first one on `PATH`, the install the
/// operator's own daemon runs. It is never built here — a plugin is checked against the afkd
/// it will meet — so a host without one fails every leg that needs it, and says why.
pub(crate) fn bin_path() -> PathBuf {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("afkd"))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .expect("no `afkd` on PATH: this gate runs the installed afkd and never builds one")
}

/// The afkd checkout `AFKD_SRC` names, for the legs that read afkd's own source rather than
/// run it. afkd lives in a repository of its own, so without one those legs skip loudly.
pub(crate) fn afkd_src() -> Option<PathBuf> {
    std::env::var_os("AFKD_SRC")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// afkd's config directory under a temp `$HOME` (ADR-0075). Every fixture that plants a
/// file where afkd will look for it goes through these rather than re-deriving the layout,
/// so the split lives in one place on the test side too.
pub(crate) fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("afkd")
}

/// afkd's state directory under a temp `$HOME` — lock, socket, `claims.json`,
/// `daemon.log[.old]` and the run corpus under `runs/`.
pub(crate) fn state_dir(home: &Path) -> PathBuf {
    home.join(".local").join("state").join("afkd")
}

/// The run corpus root under a temp `$HOME` — `<state dir>/runs`.
pub(crate) fn runs_root(home: &Path) -> PathBuf {
    state_dir(home).join("runs")
}

/// The default main config under a temp `$HOME` — `<config dir>/afkd.conf`.
pub(crate) fn main_conf(home: &Path) -> PathBuf {
    config_dir(home).join("afkd.conf")
}

/// The daemon log under a temp `$HOME` — `<state dir>/daemon.log` (ADR-0065).
pub(crate) fn daemon_log(home: &Path) -> PathBuf {
    state_dir(home).join("daemon.log")
}

/// The installed-plugin root under a temp `$HOME` — `<config dir>/plugins` (ADR-0082 §9).
pub(crate) fn plugins_root(home: &Path) -> PathBuf {
    config_dir(home).join("plugins")
}

/// Remove **every** anchor afkd reads except the `$HOME` the caller pins (ADR-0075), so a
/// developer with `$AFKD_HOME` or any `$XDG_*_HOME` exported still gets a suite that
/// resolves — and writes — inside its temp dir. Every spawn helper in this module routes
/// through it.
///
/// It clears cargo's target-directory overrides too: `afkd install` builds this plugin with
/// `cargo build`, and an inherited `CARGO_TARGET_DIR` would send that build's output away
/// from the `target/release/afkd-gitea` its `exec` names. `RUSTUP_HOME` and `CARGO_HOME`
/// stay: the pinned `$HOME` hides `~/.rustup` and `~/.cargo`, and the ones rustup's cargo
/// proxy exported into this test are what let that build find a toolchain at all.
pub(crate) fn clear_layout_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("AFKD_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
}

/// Arm `PR_SET_PDEATHSIG(SIGKILL)` on `cmd` so the **kernel** SIGKILLs the spawned
/// daemon the moment its spawning *thread* exits — the backstop for the abnormal
/// deaths the [`Daemon`] `Drop` guard cannot reach (a SIGKILLed or orphaned test
/// binary never unwinds, so its `Drop` never runs).
///
/// Two subtleties a future reader should not "fix":
///
/// - **Fires on spawning-*thread* exit, not process exit.** On the orderly path that
///   thread is the test's own body, which ends *after* `Drop` or a reap has
///   already reaped — so pdeathsig only ever bites the abnormal deaths, which is
///   exactly its job. It does not replace the `Drop` guard; it backstops it.
/// - **Survives the child's `execve` of the installed `afkd`.** `execve` clears the
///   pdeathsig disposition only for set-uid / set-gid / file-capability images; an
///   `afkd` install is none of those, so the arming outlives the exec.
fn arm_pdeathsig(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the forked child before `execve`, so it must be
    // async-signal-safe — a single `prctl` is. It never touches the parent's memory.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

/// A spawned real `afkd` process that **kills and reaps itself on `Drop`** —
/// including on unwind. **The rule this type exists to enforce: a test that spawns a
/// real process owns its lifetime, and must tear it down in `Drop`.**
///
/// Rust's `std::process::Child` does *not* kill on drop, so a bare `Child` held by a
/// **panicking** — or early-returning, or timed-out — test leaks a daemon that outlives the
/// run, and with it the plugin child it started.
///
/// ## Tearing down the whole tree, not just the daemon
///
/// The daemon puts every child it spawns in that child's **own** process group and reaps
/// them only on the **forced** shutdown: the *second* interrupt during the drain. So `Drop`
/// **escalates**: a first `SIGINT` for a graceful drain, then a second `SIGINT` to force
/// the reap of the child groups (the plugin, a `run_cmd`), and finally a `SIGKILL` floor so
/// a wedged daemon can never linger. On the normal path the guard is already emptied by
/// [`StreamingDaemon::reap`], so `Drop` no-ops.
pub(crate) struct Daemon(Option<Child>);

/// How long `Drop` waits for the first `SIGINT`'s graceful drain before forcing, and
/// then for the forced reap before the `SIGKILL` floor. Only ever paid on the leak path.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

impl Daemon {
    /// Wrap a freshly spawned child so it is killed and reaped on drop.
    pub(crate) fn new(child: Child) -> Self {
        Self(Some(child))
    }

    /// Consume the guard, handing back the raw `Child` — for a reap that takes ownership
    /// and waits on it itself ([`StreamingDaemon::reap`]). The emptied guard's later `Drop`
    /// is a no-op.
    pub(crate) fn into_child(mut self) -> Child {
        self.0.take().expect("Daemon child already taken")
    }
}

impl Deref for Daemon {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.0.as_ref().expect("Daemon child already taken")
    }
}

impl DerefMut for Daemon {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("Daemon child already taken")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return; // consumed on the normal path (into_child)
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = child.id() as libc::pid_t;
        // SAFETY: `kill` with a live child pid and a valid signal number.
        unsafe { libc::kill(pid, libc::SIGINT) }; // graceful drain
        if reap_within(&mut child, DRAIN_GRACE) {
            return;
        }
        // The daemon did not drain: a second SIGINT forces the quit, which reaps its
        // tracked child groups (the plugin, `run_cmd`).
        unsafe { libc::kill(pid, libc::SIGINT) };
        if reap_within(&mut child, DRAIN_GRACE) {
            return;
        }
        // Floor: SIGKILL the daemon itself so it can never linger, then reap it.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Poll `child.try_wait()` to `budget`, reaping and returning `true` as soon as it
/// exits; `false` if the deadline passes first. No thread is spawned (this runs inside
/// `Drop`), and a reaped child leaves no zombie behind.
fn reap_within(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One afkd subcommand, `args` whole (`install <path>`), run to completion with `$HOME`
/// pinned to `dir` and every other anchor cleared.
pub(crate) fn run_subcommand_args(dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(bin_path());
    clear_layout_env(&mut cmd)
        .current_dir(dir)
        .env("HOME", dir)
        .args(args)
        .output()
        .expect("run afkd subcommand")
}

/// A headless daemon whose **stderr is streamed** into a shared buffer while it runs, and
/// folded back into the collected [`Output`] by [`reap`](Self::reap). The inner [`Daemon`]
/// still guards the child on drop, so a panicking test before `reap` does not leak it.
pub(crate) struct StreamingDaemon {
    child: Daemon,
    stderr: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

impl StreamingDaemon {
    /// Send `signum` to the daemon — `libc::SIGINT` for the graceful drain.
    pub(crate) fn signal(&self, signum: libc::c_int) {
        // SAFETY: `kill` with a live child pid and a valid signal number.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, signum);
        }
    }

    /// Wait for the daemon to exit within `budget`, collecting its output, and SIGKILL it
    /// and fail when it does not. stderr was streamed out of the child (so
    /// `wait_with_output` sees none), so the streamed bytes are folded back into the
    /// returned [`Output`].
    pub(crate) fn reap(mut self, budget: Duration) -> Output {
        let child = self.child.into_child();
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let mut out = match rx.recv_timeout(budget) {
            Ok(res) => res.expect("collect child output"),
            Err(_) => {
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
                panic!("afkd (pid {pid}) did not exit within {budget:?}");
            }
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        out.stderr = self.stderr.lock().expect("stderr buffer").clone();
        out
    }
}

/// A bare, headless `afkd <args>` under the pinned `HOME`: stdin on `/dev/null`, stdout
/// piped, stderr streamed into a [`StreamingDaemon`], and the child held in a [`Daemon`] so
/// it dies with the test.
pub(crate) fn spawn_headless_streaming(dir: &Path, args: &[&str]) -> StreamingDaemon {
    let mut cmd = Command::new(bin_path());
    clear_layout_env(&mut cmd)
        .current_dir(dir)
        .env("HOME", dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    arm_pdeathsig(&mut cmd);
    let mut daemon = Daemon::new(cmd.spawn().expect("spawn afkd (headless)"));
    let mut pipe = daemon.stderr.take().expect("headless stderr is piped");
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let reader_buf = Arc::clone(&stderr);
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF (child exited) or a read error: end the thread.
                Ok(n) => reader_buf
                    .lock()
                    .expect("stderr buffer")
                    .extend_from_slice(&buf[..n]),
            }
        }
    });
    StreamingDaemon {
        child: daemon,
        stderr,
        reader: Some(reader),
    }
}
