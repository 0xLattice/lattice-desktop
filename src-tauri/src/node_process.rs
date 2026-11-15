//! The app is its node's process manager. `lattice up` becomes
//! `lattice-node --root <root>` and runs in the foreground until the node
//! exits; the app runs it as its own child and acts on how it exits, as
//! systemd's `Restart=on-failure` with `RestartPreventExitStatus=78` would:
//!
//! - 0 (asked to stop: `POST /stop`, SIGTERM): left stopped;
//! - 75 (asked to restart: `POST /restart`, `POST /wipe`): started again at once;
//! - 78 (its configuration does not check): left stopped, its last lines kept
//!   for the page to show;
//! - anything else: started again after a delay that doubles from one second
//!   up to a minute, and starts over at one second once a run lasted a minute.
//!
//! Stopping marks the node unwanted before it signals it, so an exit that
//! follows is never taken for one to restart; a start while it stops starts
//! it again once it has exited. Its standard output and error go
//! to `<root>/log/lattice-node.log`, moved to `lattice-node.log.1` at 8 MiB.

use super::{configured_rpc, err, host_root, lattice_command, node_accepts_cookie, run_lattice, sync_swaps, PinnedPort, Result};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const RESTART: i32 = 75;
const CONFIG_FAULT: i32 = 78;
const FIRST_DELAY: Duration = Duration::from_secs(1);
const LONGEST_DELAY: Duration = Duration::from_secs(60);
/// The node's own bound on a clean stop (`lattice down` waits as long).
const STOP_PATIENCE: Duration = Duration::from_secs(180);
const TAIL_LINES: usize = 20;
const LOG_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Default)]
pub struct NodeProcess {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Whether the node should be running: set by `start`, cleared by `stop`
    /// and by an exit that leaves it stopped.
    wanted: bool,
    /// Whether a thread is supervising it (running it, or waiting to again).
    supervised: bool,
    /// Set by `start` during a delay: run it again now.
    hurry: bool,
    /// Set when `stop` asked the running node to exit, until it has: that
    /// exit is the stop's, whatever its code.
    stopping: bool,
    /// The running node; taken in the same lock that sees it has exited.
    child: Option<Child>,
    /// Why it stopped by itself, for the page.
    error: Option<String>,
    /// Its last lines of output.
    tail: VecDeque<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Report {
    /// Whether the node's process is running now.
    pub running: bool,
    /// Whether the app keeps it running: started, and not stopped since
    /// (between a restart's exit and its start it is wanted, not running).
    pub wanted: bool,
    pub error: Option<String>,
}

/// What follows an exit with `code` (None: killed by a signal).
#[derive(Debug, PartialEq)]
enum Next {
    Stay,
    Again,
    Later,
    Fault,
}

fn next_after(code: Option<i32>) -> Next {
    match code {
        Some(0) => Next::Stay,
        Some(RESTART) => Next::Again,
        Some(CONFIG_FAULT) => Next::Fault,
        _ => Next::Later,
    }
}

/// The delay before the next start after a failure: the last one doubled,
/// or one second after none, or after a run long enough to count as healthy.
fn next_delay(last: Option<Duration>, ran: Duration) -> Duration {
    match last {
        Some(last) if ran < LONGEST_DELAY => (last * 2).min(LONGEST_DELAY),
        _ => FIRST_DELAY,
    }
}

impl NodeProcess {
    pub fn report(&self) -> Result<Report> {
        let state = self.state.lock().map_err(err)?;
        Ok(Report { running: state.child.is_some(), wanted: state.wanted, error: state.error.clone() })
    }
}

/// Starts the node, or does nothing while it runs (while it stops, it is
/// started again once it has exited). A node this app did not
/// start (left by one that crashed) is stopped first, and its storage let go
/// of, so the one running is always this app's own child.
pub fn start(app: &AppHandle) -> Result<()> {
    let process = app.state::<NodeProcess>();
    {
        let mut state = process.state.lock().map_err(err)?;
        state.wanted = true;
        if state.supervised {
            state.hurry = true;
            process.changed.notify_all();
            return Ok(());
        }
        *state = State { wanted: true, supervised: true, ..State::default() };
    }
    let root = host_root(app)?;
    let spawned = (|| {
        let port = clear_storage(
            &root,
            |port| node_accepts_cookie(port, &root),
            || run_lattice(&root, "down").map(drop),
            STOP_PATIENCE,
        )?;
        spawn(app, &root, port)
    })();
    let mut state = process.state.lock().map_err(err)?;
    match spawned {
        Ok((child, readers)) => {
            if let Some(pid) = adopt(&mut state, child) {
                ask_to_stop(app, pid, &root);
            }
            drop(state);
            let app = app.clone();
            std::thread::spawn(move || supervise(app, root, readers));
            Ok(())
        }
        Err(e) => {
            state.wanted = false;
            state.supervised = false;
            process.changed.notify_all();
            Err(e)
        }
    }
}

/// Holds `child` as the node. A stop that came while it was being started
/// is to be passed on to it: its pid then, to signal while the lock is held
/// (so it cannot have been reaped, and its pid taken by another process).
fn adopt(state: &mut State, child: Child) -> Option<u32> {
    let pid = child.id();
    state.child = Some(child);
    state.stopping = !state.wanted;
    state.stopping.then_some(pid)
}

/// Takes `child` from the state once it has exited, in the lock that saw
/// it: its exit code (None: killed by a signal), or None while it runs.
fn reap(state: &mut State) -> Option<Option<i32>> {
    let code = match state.child.as_mut().map(Child::try_wait) {
        Some(Ok(None)) => return None,
        Some(Ok(Some(status))) => status.code(),
        _ => None,
    };
    state.child = None;
    Some(code)
}

/// After an exit with `code`: whether the node is started again, and how.
/// A stop's exit (`stopped`) leaves it stopped unless a start came since.
fn after_exit(state: &mut State, code: Option<i32>, stopped: bool) -> Option<Next> {
    if !state.wanted {
        return None;
    }
    if stopped {
        return Some(Next::Again);
    }
    match next_after(code) {
        Next::Stay => None,
        Next::Fault => {
            let said: Vec<String> = state.tail.iter().cloned().collect();
            state.error = Some(format!("The node's configuration does not check, so it stays stopped:\n{}", said.join("\n")));
            None
        }
        next => Some(next),
    }
}

/// Stops the node and waits until it has exited (killing it if it is still
/// there after `STOP_PATIENCE`); it is not started again unless a start
/// comes meanwhile.
pub fn stop(app: &AppHandle) -> Result<()> {
    let process = app.state::<NodeProcess>();
    let root = host_root(app)?;
    {
        let mut state = process.state.lock().map_err(err)?;
        state.wanted = false;
        process.changed.notify_all();
        let pid = state.child.as_ref().map(Child::id);
        state.stopping |= pid.is_some();
        // In the lock: the child is not reaped meanwhile, so the pid is its.
        if let Some(pid) = pid {
            ask_to_stop(app, pid, &root);
        }
    }
    let deadline = Instant::now() + STOP_PATIENCE;
    let mut state = process.state.lock().map_err(err)?;
    while state.supervised && (state.stopping || !state.wanted) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            if let Some(child) = state.child.as_mut() {
                let _ = child.kill();
            }
            break;
        }
        state = process.changed.wait_timeout(state, left.min(Duration::from_millis(200))).map_err(err)?.0;
    }
    Ok(())
}

/// SIGTERM, which the node takes as `POST /stop`; sent while the state's
/// lock is held, so `pid` is still the unreaped child's.
#[cfg(unix)]
fn ask_to_stop(_app: &AppHandle, pid: u32, _root: &Path) {
    // `lattice up` became the node, so this is the node's own process.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// `lattice down` (`POST /stop`): the child is wsl.exe, which passes no
/// signal on to the node. A node still starting has no RPC to ask yet, and
/// `down` then finds none running: asked again each second, on a thread of
/// its own (it is called in the state's lock), until the node has exited.
#[cfg(windows)]
fn ask_to_stop(app: &AppHandle, _pid: u32, root: &Path) {
    let app = app.clone();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let deadline = Instant::now() + STOP_PATIENCE;
        let process = app.state::<NodeProcess>();
        while Instant::now() < deadline {
            let stopped = run_lattice(&root, "down").is_ok_and(|said| said.code == 0 && said.stdout.trim() == "stopped");
            let Ok(state) = process.state.lock() else { return };
            if stopped || !state.stopping {
                return;
            }
            let _ = process.changed.wait_timeout(state, Duration::from_secs(1));
        }
    });
}

/// Gets the storage at `root` ready for a node of this app's, and returns
/// the RPC port that node will start on, read once, before anything stops.
/// A node already running on it is asked to stop (`down`) when it answers
/// with the cookie on that port (`answers`); either way the storage's lock
/// must be let go of within `patience`, else the new node would only fail
/// on it, again and again.
// Once the CLI reaches a running node on the port it listens on, `down`
// stops whatever node holds the storage, and `answers` can go: a node whose
// new port is saved but not yet in use is stopped too, not waited out.
fn clear_storage(
    root: &Path,
    answers: impl Fn(u16) -> bool,
    down: impl FnOnce() -> Result<()>,
    patience: Duration,
) -> Result<Option<u16>> {
    let port = configured_rpc(root);
    if !storage_held(root) {
        return Ok(port);
    }
    if port.is_some_and(answers) {
        down()?;
    }
    let deadline = Instant::now() + patience;
    while storage_held(root) {
        if Instant::now() >= deadline {
            return Err("Another node holds this storage and has not let go of it; stop it (`lattice down`), then start again.".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(port)
}

/// The lock a running node holds on its storage (`StorageDirectoryLock`).
fn storage_lock(root: &Path) -> PathBuf {
    root.join("chains").join("Nexus").join(".lattice-node.lock")
}

/// Whether a node holds the storage at `root`: its lock taken by another.
#[cfg(unix)]
fn storage_held(root: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = fs::File::open(storage_lock(root)) else { return false };
    let fd = file.as_raw_fd();
    // SAFETY: flock on a descriptor this function owns, closed with `file`.
    unsafe {
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 {
            libc::flock(fd, libc::LOCK_UN);
            return false;
        }
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

/// Whether a node holds the storage at `root`, asked inside WSL, where the
/// node takes its lock (`flock -n` fails with 1 while another holds it).
#[cfg(windows)]
fn storage_held(root: &Path) -> bool {
    if !storage_lock(root).is_file() {
        return false;
    }
    let Ok(session) = crate::wsl::session() else { return false };
    let Some(linux_root) = crate::wsl::linux_path(&session.distro, root) else { return false };
    let lock = format!("{linux_root}/chains/Nexus/.lattice-node.lock");
    let status = crate::wsl::command().args(["-d", &session.distro, "-e", "flock", "-n", &lock, "true"]).status();
    status.is_ok_and(|s| s.code() == Some(1))
}

/// `lattice up --root <root>` as this app's child, its output going to the
/// log (and its last lines to the tail), with `port` (the RPC port it is
/// started on) pinned for `node_http`.
fn spawn(app: &AppHandle, root: &Path, port: Option<u16>) -> Result<(Child, Vec<JoinHandle<()>>)> {
    fs::create_dir_all(root).map_err(err)?;
    let (mut command, linux_root) = lattice_command(root)?;
    command.args(["up", "--root"]).arg(linux_root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("could not run the bundled lattice: {e}"))?;
    let log = Arc::new(Mutex::new(Log::open(root.join("log").join("lattice-node.log"))));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(pump(app.clone(), out, log.clone()));
    }
    if let Some(out) = child.stderr.take() {
        readers.push(pump(app.clone(), out, log));
    }
    pin(app, port);
    Ok((child, readers))
}

/// Pins `port` for `node_http`; the swap wallet reaches NEX on it, opened again on a new one.
fn pin(app: &AppHandle, port: Option<u16>) {
    let state = app.state::<PinnedPort>();
    let Ok(mut pinned) = state.0.lock() else { return };
    let before = std::mem::replace(&mut *pinned, port);
    drop(pinned);
    if port.is_some() && port != before {
        sync_swaps(app);
    }
}

/// Copies each line of `out` to the log and the tail.
fn pump(app: AppHandle, out: impl Read + Send + 'static, log: Arc<Mutex<Log>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(out);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            if let Ok(mut log) = log.lock() {
                log.write(&line);
            }
            let text = String::from_utf8_lossy(&line).trim_end().to_string();
            if let Ok(mut state) = app.state::<NodeProcess>().state.lock() {
                if state.tail.len() == TAIL_LINES {
                    state.tail.pop_front();
                }
                state.tail.push_back(text);
            }
            line.clear();
        }
    })
}

/// Runs the node until it is no longer wanted.
fn supervise(app: AppHandle, root: PathBuf, mut readers: Vec<JoinHandle<()>>) {
    let process = app.state::<NodeProcess>();
    let mut delay = None;
    let mut began = Instant::now();
    loop {
        let (code, stopped) = loop {
            let Ok(mut state) = process.state.lock() else { return };
            if let Some(code) = reap(&mut state) {
                // The exit is seen: a stop or start from now on is about no process.
                break (code, std::mem::take(&mut state.stopping));
            }
            let _ = process.changed.wait_timeout(state, Duration::from_millis(200));
        };
        // Its last lines are all in before they are read.
        for reader in readers.drain(..) {
            let _ = reader.join();
        }
        let Ok(mut state) = process.state.lock() else { return };
        process.changed.notify_all();
        let Some(next) = after_exit(&mut state, code, stopped) else {
            state.wanted = false;
            state.supervised = false;
            process.changed.notify_all();
            drop(state);
            pin(&app, None);
            return;
        };
        if next == Next::Later {
            let wait = next_delay(delay, began.elapsed());
            delay = Some(wait);
            let until = Instant::now() + wait;
            state.hurry = false;
            while state.wanted && !state.hurry && Instant::now() < until {
                let left = until.saturating_duration_since(Instant::now());
                state = match process.changed.wait_timeout(state, left) {
                    Ok((state, _)) => state,
                    Err(_) => return,
                };
            }
            if !state.wanted {
                state.supervised = false;
                process.changed.notify_all();
                drop(state);
                pin(&app, None);
                return;
            }
        } else {
            delay = None;
        }
        drop(state);
        // Its process has exited, so nothing holds the storage: the port
        // read now is the one the node starts on.
        match spawn(&app, &root, configured_rpc(&root)) {
            Ok((child, more)) => {
                readers = more;
                began = Instant::now();
                if let Ok(mut state) = process.state.lock() {
                    if let Some(pid) = adopt(&mut state, child) {
                        ask_to_stop(&app, pid, &root);
                    }
                }
            }
            Err(e) => {
                if let Ok(mut state) = process.state.lock() {
                    state.error = Some(e);
                    state.wanted = false;
                    state.supervised = false;
                    process.changed.notify_all();
                }
                pin(&app, None);
                return;
            }
        }
    }
}

/// The node's log: appended to, and moved aside once it reaches `LOG_LIMIT`.
struct Log {
    path: PathBuf,
    file: Option<fs::File>,
    size: u64,
}

impl Log {
    fn open(path: PathBuf) -> Log {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        let size = file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
        Log { path, file, size }
    }

    fn write(&mut self, line: &[u8]) {
        if self.size + line.len() as u64 > LOG_LIMIT {
            self.file = None;
            let _ = fs::rename(&self.path, self.path.with_extension("log.1"));
            *self = Log::open(self.path.clone());
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(line).is_ok() {
                self.size += line.len() as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_are_taken_as_a_process_manager_takes_them() {
        assert_eq!(next_after(Some(0)), Next::Stay);
        assert_eq!(next_after(Some(75)), Next::Again);
        assert_eq!(next_after(Some(78)), Next::Fault);
        for other in [Some(1), Some(64), Some(134), None] {
            assert_eq!(next_after(other), Next::Later, "{other:?}");
        }
    }

    #[test]
    fn failures_back_off_up_to_a_minute_and_start_over_after_a_healthy_run() {
        let mut delay = None;
        let mut seen = Vec::new();
        for _ in 0..9 {
            delay = Some(next_delay(delay, Duration::from_secs(1)));
            seen.push(delay.unwrap().as_secs());
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(next_delay(Some(LONGEST_DELAY), Duration::from_secs(600)), FIRST_DELAY);
    }

    #[test]
    fn a_stops_exit_leaves_it_stopped_unless_a_start_came_since() {
        let wanted = |wanted| State { wanted, supervised: true, ..State::default() };
        // A stop's exit, whatever its code: stopped, or, started meanwhile, run again.
        for code in [Some(0), Some(1), None] {
            assert_eq!(after_exit(&mut wanted(false), code, true), None);
            assert_eq!(after_exit(&mut wanted(true), code, true), Some(Next::Again));
        }
        // An exit of its own: as the code says.
        assert_eq!(after_exit(&mut wanted(true), Some(0), false), None);
        assert_eq!(after_exit(&mut wanted(true), Some(75), false), Some(Next::Again));
        assert_eq!(after_exit(&mut wanted(true), Some(1), false), Some(Next::Later));
        assert_eq!(after_exit(&mut wanted(false), Some(75), false), None);
        let mut fault = wanted(true);
        fault.tail.push_back("unknown key publicRead".into());
        assert_eq!(after_exit(&mut fault, Some(78), false), None);
        assert!(fault.error.unwrap().ends_with("unknown key publicRead"));
    }

    #[test]
    fn a_stop_during_its_start_is_passed_on_and_its_exit_reaped_in_one_lock() {
        let mut state = State { wanted: false, supervised: true, ..State::default() };
        let child = std::process::Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let pid = child.id();
        // Stopped while it was being started: its pid is to be signalled, and its exit is the stop's.
        assert_eq!(adopt(&mut state, child), Some(pid));
        assert!(state.stopping);
        let code = loop {
            if let Some(code) = reap(&mut state) {
                break code;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(code, Some(3));
        assert!(state.child.is_none());
        // Wanted when it is adopted: nothing to pass on.
        let mut state = State { wanted: true, supervised: true, ..State::default() };
        let child = std::process::Command::new("sh").args(["-c", "sleep 5"]).spawn().unwrap();
        assert_eq!(adopt(&mut state, child), None);
        assert!(!state.stopping);
        assert_eq!(reap(&mut state), None);
        assert!(state.child.is_some());
        state.child.as_mut().unwrap().kill().unwrap();
        let _ = state.child.as_mut().unwrap().wait();
    }

    #[test]
    fn the_log_is_moved_aside_at_its_limit() {
        let dir = std::env::temp_dir().join(format!("lattice-node-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("log").join("lattice-node.log");
        let mut log = Log::open(path.clone());
        log.size = LOG_LIMIT - 4;
        log.write(b"abc\n");
        log.write(b"def\n");
        assert_eq!(fs::read(&path).unwrap(), b"def\n");
        assert_eq!(fs::read(path.with_extension("log.1")).unwrap(), b"abc\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A root whose lattice.json names `rpc`, with its node's storage lock
    /// held by the file returned, as a running node's would be.
    #[cfg(unix)]
    fn held_root(name: &str, rpc: u16) -> (PathBuf, fs::File) {
        use std::os::fd::AsRawFd;
        let root = std::env::temp_dir().join(format!("lattice-desktop-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(storage_lock(&root).parent().unwrap()).unwrap();
        fs::write(root.join("lattice.json"), format!(r#"{{"rpc": {rpc}}}"#)).unwrap();
        let holder = fs::File::create(storage_lock(&root)).unwrap();
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(storage_held(&root));
        (root, holder)
    }

    #[cfg(unix)]
    #[test]
    fn the_port_is_read_once_before_the_running_node_is_stopped() {
        let (root, holder) = held_root("port-once", 8080);
        let holder = Mutex::new(Some(holder));
        let asked = Mutex::new(Vec::new());
        let port = clear_storage(
            &root,
            |port| {
                asked.lock().unwrap().push(port);
                true
            },
            || {
                // A new port saved while it ran: the stopped node's next
                // start is on it, not the one this start pins.
                fs::write(root.join("merged.json"), r#"{"rpc": 9090}"#).unwrap();
                holder.lock().unwrap().take();
                Ok(())
            },
            Duration::from_secs(5),
        );
        assert_eq!(port, Ok(Some(8080)));
        assert_eq!(*asked.lock().unwrap(), vec![8080]);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_start_waits_until_the_storage_is_let_go_of() {
        let (root, holder) = held_root("let-go", 8081);
        let downs = Mutex::new(0);
        let began = Instant::now();
        let port = clear_storage(
            &root,
            |_| true,
            || {
                // `down` returns once the RPC is gone; the lock goes later.
                *downs.lock().unwrap() += 1;
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    drop(holder);
                });
                Ok(())
            },
            Duration::from_secs(5),
        );
        assert_eq!(port, Ok(Some(8081)));
        assert_eq!(*downs.lock().unwrap(), 1);
        assert!(began.elapsed() >= Duration::from_millis(400));
        assert!(!storage_held(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_node_that_keeps_the_storage_is_not_started_against() {
        // Not answering on the port (its new one saved, not yet in use): not
        // asked to stop, and the start fails once its patience is out.
        let (root, holder) = held_root("kept", 8082);
        let began = Instant::now();
        let port = clear_storage(&root, |_| false, || panic!("asked to stop"), Duration::from_millis(300));
        assert!(port.unwrap_err().contains("holds this storage"));
        assert!(began.elapsed() >= Duration::from_millis(300));
        drop(holder);
        // Let go of: the start goes ahead, and stops nothing.
        assert_eq!(clear_storage(&root, |_| true, || panic!("asked to stop"), Duration::ZERO), Ok(Some(8082)));
        fs::remove_dir_all(root).unwrap();
    }
}
