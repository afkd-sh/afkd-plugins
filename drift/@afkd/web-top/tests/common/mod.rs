//! The harness the drift gate drives afkd through. Every spawn runs the `afkd` first on
//! `PATH` ([`bin_path`]) with `$HOME` pinned to a temp dir that holds a seeded `afkd.conf`,
//! so nothing here builds afkd or reaches the operator's own daemon.
//!
//! ## Two ways to run a daemon
//!
//! - **Headless** ([`spawn_headless_streaming`]): a bare `afkd` with stdin on `/dev/null`,
//!   its stderr streamed so a test can wait on an emitted line instead of sleeping, and its
//!   shutdown signal-driven — a `SIGINT` drains, a second forces.
//! - **Captured** ([`spawn_captured_sized`]): `afkd top` on a PTY of a given size, every byte
//!   it paints fed through a `vt100::Parser`, so a test reads the screen a terminal would show.
//!
//! ## No hangs on failure
//!
//! Every wait is bounded, and every real spawn is held in a [`Daemon`] that kills and reaps it
//! on drop, so a failing leg fails loudly rather than hanging the test binary or leaking an
//! afkd that outlives the run.

use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::TempDir;

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

/// afkd's three directories under a temp `$HOME` (ADR-0075), spelled **once** for
/// the whole gate. Every fixture that plants a file where afkd will look for it
/// — or asserts one is absent — goes through these rather than re-deriving the
/// layout, so the split lives in one place on the test side too.
///
/// `home` is the directory the spawn helpers pin `$HOME` to (and clear every other
/// anchor around, see [`clear_layout_env`]), so these are exactly what the spawned
/// binary resolves.
pub(crate) fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("afkd")
}

/// afkd's state directory under a temp `$HOME` — lock, socket, `status.json`,
/// `claims.json`, `due.json`, `codex-sessions.json`, `daemon.log[.old]` and the run
/// corpus under `runs/`.
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

/// Remove **every** anchor afkd reads except the `$HOME` the caller pins (ADR-0075).
/// Without this a developer with `$AFKD_HOME` or any `$XDG_*_HOME` exported gets a
/// suite that resolves — and writes — outside its temp dir, which is precisely the
/// failure mode the single `.env_remove("AFKD_HOME")` guard used to prevent for one
/// of the three. Every spawn helper in this module routes through it.
pub(crate) fn clear_layout_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("AFKD_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
}

/// Arm `PR_SET_PDEATHSIG(SIGKILL)` on `cmd` so the **kernel** SIGKILLs the spawned
/// daemon the moment its spawning *thread* exits — the backstop for the abnormal
/// deaths the [`Daemon`] `Drop` guard cannot reach (a SIGKILLed or orphaned test
/// binary never unwinds, so its `Drop` never runs). Every real-`afkd` spawn path
/// routes through here.
///
/// Two subtleties a future reader should not "fix":
///
/// - **Fires on spawning-*thread* exit, not process exit.** On the orderly path that
///   thread is the test's own body, which ends *after* `Drop` or a reap has
///   already reaped — so pdeathsig only ever bites the abnormal deaths, which is
///   exactly its job. It does not replace the `Drop` guard; it backstops it.
/// - **Survives the child's `execve` of the installed `afkd`.** `execve` clears the
///   pdeathsig disposition only for set-uid / set-gid / file-capability images; an
///   `afkd` install is none of those, so the arming outlives the exec — it is not a
///   non-bug to be removed.
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
/// Rust's `std::process::Child` does *not* kill on drop (documented std behaviour, not
/// an oversight), so a bare `Child` held by a **panicking** — or early-returning, or
/// timed-out — test leaks a daemon that outlives the run. Because selfdev runs
/// `cargo test` inside afkd's own systemd cgroup, a leaked daemon wedges the unit's
/// restart: systemd will not mark the unit inactive until its cgroup is empty, and
/// `Restart=always` only fires after deactivation. This guard closes that hole — hold
/// every real spawn in a `Daemon` and the child dies with the test.
///
/// ## Tearing down the whole tree, not just the daemon
///
/// The daemon puts every child it spawns in that child's **own** process group and
/// reaps them (`kill_all` — a `killpg` SIGKILL over each tracked group) only on the
/// **forced** shutdown: the *second* interrupt during the drain. A lone `killpg` of the
/// daemon's own group would therefore reach only the daemon and orphan its children one
/// layer down (an agent worker, a sandboxed `bwrap`, a `run_cmd`). So `Drop`
/// **escalates**: a first `SIGINT` for a graceful drain, then — if that does not land —
/// a second `SIGINT` to force `kill_all` (reaping the child groups), and finally a
/// `SIGKILL` floor so a wedged daemon can never linger. On the normal path the guard is
/// already emptied by [`StreamingDaemon::reap`] (through [`into_child`](Self::into_child)),
/// so `Drop` no-ops and none of this teardown cost is paid.
pub(crate) struct Daemon(Option<Child>);

/// How long `Drop` waits for the first `SIGINT`'s graceful drain before forcing, and
/// then for the forced `kill_all` before the `SIGKILL` floor. Only ever paid on the
/// leak path (panic / early return); the normal path consumes the guard first, so its
/// `Drop` no-ops.
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
            return; // consumed on the normal path (wait_or_kill / into_child)
        };
        // Already exited (e.g. a graceful reap that never consumed the guard)? Reap the
        // zombie and we are done.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = child.id() as libc::pid_t;
        // SAFETY: `kill` with a live child pid and a valid signal number.
        unsafe { libc::kill(pid, libc::SIGINT) }; // graceful drain
        if reap_within(&mut child, DRAIN_GRACE) {
            return;
        }
        // The daemon did not drain: a second SIGINT forces the quit, which runs
        // `kill_all` over its tracked child groups (agent worker, `bwrap`, `run_cmd`).
        unsafe { libc::kill(pid, libc::SIGINT) };
        if reap_within(&mut child, DRAIN_GRACE) {
            return;
        }
        // Floor: SIGKILL the daemon itself so it can never linger, then reap it so no
        // zombie is left behind.
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

/// A directory that doubles as the controlled `$HOME`, seeded with the **default**
/// config at `<dir>/.config/afkd/afkd.conf` (ADR-0020 §2, ADR-0075). The no-`[PATH]`
/// spawn helpers point `HOME` at this dir, so `afkd run`/`validate` resolve the default
/// to this seeded file while the run's CWD stays `dir` (where sentinels/artifacts
/// land), keeping these suites independent of the developer's real `$HOME`.
pub(crate) fn dir_with_config(config: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let conf = main_conf(dir.path());
    std::fs::create_dir_all(conf.parent().expect("has parent")).expect("mk the config dir");
    std::fs::write(&conf, config).expect("write default config");
    dir
}

/// The read/write **master** end of a pseudo-terminal whose slave is the spawned
/// child's stdin. Owns the master fd; writing to it delivers keystrokes to the TUI
/// exactly as a terminal would (crossterm reads key events from stdin when stdin is
/// a TTY — which the slave is).
pub(crate) struct PtyMaster {
    fd: OwnedFd,
}

impl PtyMaster {
    /// Write raw bytes (keystrokes) to the terminal master — e.g. `b"q"` to begin a
    /// graceful quit. Panics on a write error; a short-write loops until drained.
    pub(crate) fn write_all(&self, bytes: &[u8]) {
        let mut written = 0;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                )
            };
            assert!(
                n > 0,
                "write to pty master failed: {}",
                std::io::Error::last_os_error()
            );
            written += n as usize;
        }
    }
}

/// Open a fresh pseudo-terminal, returning the owned `(master, slave)` fds. `winsize`
/// sizes the terminal: the frame-capturing harness passes `Some(...)` because it needs
/// the columns to render every list cell in full, and its child reads that geometry
/// back through crossterm's `size()`. `None` opens a null window — retained as the
/// seam's neutral default. Sole caller is [`spawn_captured_sized`].
fn open_pty(winsize: Option<libc::winsize>) -> (OwnedFd, OwnedFd) {
    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let ws_ptr = winsize
        .as_ref()
        .map_or(std::ptr::null(), |w| w as *const libc::winsize);
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            ws_ptr,
        )
    };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
    // SAFETY: `openpty` just handed us two fresh, exclusively-owned fds.
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
    (master, slave)
}

/// A **frame-capturing** TUI session (the `afkd top` client): unlike the headless
/// sentinel harness (which discards any render and asserts on disk), this wires the
/// child's dashboard **stdout onto the PTY slave**
/// too, so the alternate-screen paint flows back to the master, and a dedicated reader
/// thread feeds every drained byte through a `vt100::Parser` that reconstructs the
/// screen grid. Tests [`send`](Self::send) keystrokes to the master and
/// [`wait_until`](Self::wait_until) the *rendered* screen satisfies a predicate — a
/// redraw signal, never a fixed settle sleep.
pub(crate) struct CapturedTui {
    /// The `afkd top` client, guarded so a panicking capture test does not leak it (and
    /// its blocked reader thread). Its `Drop` kills+reaps the client, which closes the
    /// slave fds so the master read hits `EIO` and the `_reader` thread ends.
    _child: Daemon,
    /// The write end of the PTY master (keystrokes to the TUI).
    master: PtyMaster,
    /// The reconstructed screen, updated by the reader thread on every redraw.
    parser: Arc<Mutex<vt100::Parser>>,
    /// The drain thread; ends on its own when the child's slave fds close at exit
    /// (Linux returns `EIO` on the master then), so no explicit join is needed.
    _reader: JoinHandle<()>,
}

impl CapturedTui {
    /// Write raw keystroke bytes to the terminal master — e.g. `b"x"` to stop the
    /// selected service, `b"\x12"` for Ctrl-R (reload). Reuses [`PtyMaster::write_all`].
    pub(crate) fn send(&self, bytes: &[u8]) {
        self.master.write_all(bytes);
    }

    /// The current reconstructed visible screen as text (rows joined by `\n`, trailing
    /// blanks trimmed). Assertions are plain substring checks against this.
    pub(crate) fn screen(&self) -> String {
        self.parser
            .lock()
            .expect("parser mutex")
            .screen()
            .contents()
    }

    /// Screen row `row` as one string **per column**, left to right — the read a test needs
    /// when it has to compare this screen against another surface's, column against column.
    ///
    /// It does not hand back a cell's raw `contents()`, because two very different cells
    /// spell themselves the same way there: a blank the renderer never had to emit (ratatui diffs against a blank
    /// buffer, so an unchanged space is never written) and the covered half of a wide glyph
    /// both come back empty. Here the first becomes `" "` and only the second stays `""`, so
    /// the index **is** the screen column and a `監視` occupies exactly the two it paints on.
    ///
    /// vt100 sizes a cell by its base char alone, so a VS16 sequence (`▪️`, `🕸️`) reads one
    /// column wide there. This reader measures each cell with `unicode-width` — the crate
    /// afkd's layout budgets with — and returns the column afkd reserved after such a glyph as
    /// its covered `""`, which is how a VS16-honouring terminal (and the plugin's grid) shows
    /// it. Whatever vt100 still holds in that column is stale ink `pin_wide_glyph_widths` never
    /// repaints, covered on such a terminal, so it is dropped here. [`screen`](Self::screen)
    /// still models the terminal that ignores VS16.
    pub(crate) fn row_grid(&self, row: u16) -> Vec<String> {
        let parser = self.parser.lock().expect("parser mutex");
        let screen = parser.screen();
        let (_, cols) = screen.size();
        let mut covered = false;
        let mut grid = Vec::with_capacity(usize::from(cols));
        for c in 0..cols {
            if std::mem::take(&mut covered) {
                grid.push(String::new());
                continue;
            }
            grid.push(screen.cell(row, c).map_or_else(
                || " ".to_string(),
                |cell| match cell.contents() {
                    "" if cell.is_wide_continuation() => String::new(),
                    "" => " ".to_string(),
                    text => {
                        covered =
                            !cell.is_wide() && unicode_width::UnicodeWidthStr::width(text) > 1;
                        text.to_string()
                    }
                },
            ));
        }
        grid
    }

    /// Bounded-deadline poll: return `true` as soon as the **rendered** screen satisfies
    /// `pred`, or `false` when `budget` elapses. The reader thread keeps the screen
    /// current independently, so the decision to proceed is driven by *what has
    /// rendered* — the 20 ms cadence is only how often we re-inspect, never a settle
    /// delay.
    pub(crate) fn wait_until(&self, budget: Duration, pred: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if pred(&self.screen()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `afkd <args>` on a `cols`×`rows` PTY under the pinned `HOME`, with each `(name, value)`
/// in `extra_env` set on it too. The `vt100::Parser` is sized to match, so `screen()` reconstructs the
/// same geometry the child laid its frame out against — which is what lets a test observe
/// a *width-dependent* layout (the list hides its `Runs`/`Errors` columns on a narrow
/// terminal) end-to-end rather than only through a `TestBackend`.
pub(crate) fn spawn_captured_sized(
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
    cols: u16,
    rows: u16,
) -> CapturedTui {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (master, slave) = open_pty(Some(winsize));
    // The child's stdout is a *second* handle onto the same slave, so its
    // alternate-screen paint flows to the master (stdin is the slave itself).
    let slave_out = slave.try_clone().expect("dup pty slave for stdout");
    let mut cmd = Command::new(bin_path());
    clear_layout_env(&mut cmd)
        .current_dir(dir)
        .env("HOME", dir)
        .envs(extra_env.iter().copied())
        .args(args)
        .stdin(Stdio::from(slave))
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::piped());
    arm_pdeathsig(&mut cmd);
    let child = cmd.spawn().expect("spawn afkd");
    // Both slave handles were moved into the child; the parent now holds neither, so the
    // child is the sole slave owner and the master reads `EIO` once it exits.

    // Two independent master handles: one drives the reader thread (reads), the struct
    // keeps the other for `send` (writes). The two directions of a pty are independent,
    // so concurrent use from the two threads is sound.
    let read_master = master.try_clone().expect("dup pty master for reader");
    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let reader_parser = Arc::clone(&parser);
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    read_master.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            // `n <= 0` is EOF or `EIO` (Linux, once the child's slave fds close at exit):
            // either way the child is gone, so drop the drainer and end the thread.
            if n <= 0 {
                break;
            }
            reader_parser
                .lock()
                .expect("parser mutex")
                .process(&buf[..n as usize]);
        }
    });
    CapturedTui {
        _child: Daemon::new(child),
        master: PtyMaster { fd: master },
        parser,
        _reader: reader,
    }
}

/// One afkd subcommand, `args` whole (`install <path>`), run to completion under the
/// pinned `$HOME`: [`run_subcommand_args_with_env`] with nothing added to the environment.
pub(crate) fn run_subcommand_args(dir: &Path, args: &[&str]) -> Output {
    run_subcommand_args_with_env(dir, args, &[])
}

/// The one spawn every one-shot subcommand routes through: `$HOME` pinned to `dir`, every
/// other anchor cleared, `extra_env` applied on top — for a verb that needs a test-only
/// override as well, like the loopback index `AFKD_PLUGIN_INDEX_URL` points it at.
pub(crate) fn run_subcommand_args_with_env(
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    let mut cmd = Command::new(bin_path());
    clear_layout_env(&mut cmd)
        .current_dir(dir)
        .env("HOME", dir)
        .envs(extra_env.iter().copied())
        .args(args)
        .output()
        .expect("run afkd subcommand")
}

/// Send `signum` to `child` — a best-effort `kill(2)`. Used to drive the **headless**
/// supervisor's shutdown (`libc::SIGINT`/`libc::SIGTERM` → graceful drain, a second
/// → forced) and its config reload (`libc::SIGHUP`). The pid comes straight from the
/// just-spawned `Child`.
pub(crate) fn signal_child(child: &Child, signum: libc::c_int) {
    // SAFETY: `kill` with a live child pid and a valid signal number.
    unsafe {
        libc::kill(child.id() as libc::pid_t, signum);
    }
}

/// A bare, headless `afkd <args>` under the pinned `HOME`, with each `(name, value)` in
/// `extra_env` set on it too: stdin on `/dev/null`, both output streams piped, and the child
/// held in a [`Daemon`] so it dies with the test.
pub(crate) fn spawn_headless_with_env(
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Daemon {
    let mut cmd = Command::new(bin_path());
    clear_layout_env(&mut cmd)
        .current_dir(dir)
        .env("HOME", dir)
        .envs(extra_env.iter().copied())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    arm_pdeathsig(&mut cmd);
    Daemon::new(cmd.spawn().expect("spawn afkd (headless)"))
}

/// A headless daemon whose **stderr is streamed** line-by-line into a shared buffer while
/// it runs, so a test can block on an emitted **line** as an ordering barrier instead of a
/// fixed sleep. Two orderings need this — neither leaves an on-disk sentinel to wait on:
///
/// - **Handler installed.** The daemon prints `supervising N service(s)` only *after* its
///   SIGINT/SIGTERM handler is installed; a test whose service never fires (so has no
///   marker to wait on) blocks on that line before its first signal, so the signal is
///   caught by the handler rather than taking SIGINT's default (terminate) disposition.
/// - **Draining.** On the first interrupt the supervisor prints `Press interrupt again to
///   force quit.`; a double-interrupt test blocks on it before the second signal, so the
///   two arrive as *distinct* interrupt events — a fixed inter-signal gap only papers over
///   the OS/`signal-hook` coalescing this barrier removes.
///
/// stdout stays piped and is folded back together with the streamed stderr at exit by
/// [`reap`](StreamingDaemon::reap). The inner [`Daemon`] still guards the child on drop, so
/// a panicking test before `reap` does not leak the daemon.
pub(crate) struct StreamingDaemon {
    child: Daemon,
    stderr: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

impl StreamingDaemon {
    /// Send `signum` to the daemon (the [`signal_child`] shape, for a streaming spawn).
    pub(crate) fn signal(&self, signum: libc::c_int) {
        signal_child(&self.child, signum);
    }

    /// Wait for the daemon to exit within `budget`, collecting its output, and SIGKILL it
    /// and fail when it does not. stderr was streamed out of the child
    /// (so `wait_with_output` sees none), so the streamed bytes are folded back into the
    /// returned [`Output`] and its callers' `stderr` diagnostics still read.
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

/// A headless `afkd <args>` whose stderr streams into a [`StreamingDaemon`], so a test
/// can block on an emitted line (the graceful-stop notice / the supervising banner)
/// as an ordering barrier instead of a fixed sleep.
pub(crate) fn spawn_headless_streaming(dir: &Path, args: &[&str]) -> StreamingDaemon {
    spawn_headless_streaming_with_env(dir, args, &[])
}

/// [`spawn_headless_streaming`] with `extra_env`, the streaming counterpart of
/// [`spawn_headless_with_env`].
pub(crate) fn spawn_headless_streaming_with_env(
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> StreamingDaemon {
    let mut daemon = spawn_headless_with_env(dir, args, extra_env);
    // Take the child's stderr pipe so a reader thread can drain it live; `reap` folds the
    // accumulated bytes back into the collected `Output`.
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

/// The installed-plugin root under a temp `$HOME` — `<config dir>/plugins` (ADR-0082 §9).
pub(crate) fn plugins_root(home: &Path) -> PathBuf {
    config_dir(home).join("plugins")
}

/// Whether `python3` is on PATH (the bundled-skill suite skips loudly if it is
/// not — the `git_available` idiom, so a python-less CI image stays green).
pub(crate) fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The absolute path to the first `python3` on `PATH`. A bundled-skill test
/// invokes the interpreter **by absolute path** so the child's own `PATH` can be
/// set independently (e.g. a stub `git` present, or an empty dir so `git` is
/// absent) without disturbing how python itself is located. Panics if none
/// resolves — callers gate on [`python3_available`] first.
pub(crate) fn python3_path() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("python3 on PATH (gate on python3_available first)")
}

/// A loopback stand-in for the plugin name index on afkd.sh (ADR-0082 §11), which keeps
/// answering and **counts** every request that reaches it.
///
/// The counter is the point. "A URL or a path never asks the index" is a claim about an
/// *absence*, and a one-shot server proves nothing about one: what it takes is a server
/// that was up, was reachable, and still saw zero. Stopped by its own [`Drop`], so no
/// thread outlives the test.
pub(crate) struct IndexServer {
    /// The URL to point `AFKD_PLUGIN_INDEX_URL` at.
    pub(crate) url: String,
    /// The port, so [`Drop`] can wake the accept.
    port: u16,
    /// Requests that reached the listener.
    hits: Arc<AtomicUsize>,
    /// Set by [`Drop`] before the wake-up connection.
    stop: Arc<AtomicBool>,
    /// The accept loop.
    server: Option<JoinHandle<()>>,
}

impl IndexServer {
    /// How many requests have arrived so far.
    pub(crate) fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for IndexServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // `accept` blocks, so the flag alone would never be read. One connection wakes it.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Serve `body` as `/plugins.json` on loopback, as the deployed site serves it.
pub(crate) fn serve_index(body: &str) -> IndexServer {
    serve_index_with_status(body, "200 OK")
}

/// [`serve_index`] with the status line spelled out — the leg where the index route itself
/// answers something that is not a success.
pub(crate) fn serve_index_with_status(body: &str, status: &str) -> IndexServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let payload = body.to_string();
    let (counter, flag) = (Arc::clone(&hits), Arc::clone(&stop));
    let server = std::thread::spawn(move || {
        while let Ok((mut socket, _)) = listener.accept() {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(payload.as_bytes());
            let _ = socket.flush();
        }
    });
    IndexServer {
        url: format!("http://127.0.0.1:{port}/plugins.json"),
        port,
        hits,
        stop,
        server: Some(server),
    }
}
