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
//! follows is never taken for one to restart. Its standard output and error go
//! to `<root>/log/lattice-node.log`, moved to `lattice-node.log.1` at 8 MiB.

use super::{err, host_root, lattice_command, node_accepts_cookie, read_topology_at, rpc_port, run_lattice, sync_swaps, topology_path, PinnedPort, Result};
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
    child: Option<Child>,
    /// Why it stopped by itself, for the page.
    error: Option<String>,
    /// Its last lines of output.
    tail: VecDeque<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Report {
    pub running: bool,
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
        Ok(Report { running: state.wanted, error: state.error.clone() })
    }
}

/// Starts the node, or does nothing while it runs. A node this app did not
/// start (left by one that crashed) is stopped first, so the one running is
/// always this app's own child.
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
        let port = rpc_port(&read_topology_at(&topology_path(app)?)?).ok();
        if port.is_some_and(|port| node_accepts_cookie(port, &root)) {
            run_lattice(&root, "down")?;
        }
        spawn(app, &root)
    })();
    let mut state = process.state.lock().map_err(err)?;
    match spawned {
        Ok((child, readers)) => {
            let late_stop = adopt(&mut state, child);
            drop(state);
            if let Some(pid) = late_stop {
                ask_to_stop(pid, &root);
            }
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
/// is to be passed on to it: its pid then, to signal once the lock is let go.
fn adopt(state: &mut State, child: Child) -> Option<u32> {
    let pid = child.id();
    state.child = Some(child);
    (!state.wanted).then_some(pid)
}

/// Stops the node and waits until it has exited (killing it if it is still
/// there after `STOP_PATIENCE`); it is not started again.
pub fn stop(app: &AppHandle) -> Result<()> {
    let process = app.state::<NodeProcess>();
    let pid = {
        let mut state = process.state.lock().map_err(err)?;
        state.wanted = false;
        process.changed.notify_all();
        state.child.as_ref().map(Child::id)
    };
    if let Some(pid) = pid {
        ask_to_stop(pid, &host_root(app)?);
    }
    let deadline = Instant::now() + STOP_PATIENCE;
    let mut state = process.state.lock().map_err(err)?;
    while state.supervised {
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

/// SIGTERM, which the node takes as `POST /stop`.
#[cfg(unix)]
fn ask_to_stop(pid: u32, _root: &Path) {
    // `lattice up` became the node, so this is the node's own process.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// `lattice down` (`POST /stop`): the child is wsl.exe, which passes no
/// signal on to the node.
#[cfg(windows)]
fn ask_to_stop(_pid: u32, root: &Path) {
    let _ = run_lattice(root, "down");
}

/// `lattice up --root <root>` as this app's child, its output going to the
/// log (and its last lines to the tail), with the RPC port it is started on
/// pinned for `node_http`.
fn spawn(app: &AppHandle, root: &Path) -> Result<(Child, Vec<JoinHandle<()>>)> {
    fs::create_dir_all(root).map_err(err)?;
    let port = rpc_port(&read_topology_at(&topology_path(app)?)?).ok();
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
        let code = loop {
            let Ok(mut state) = process.state.lock() else { return };
            match state.child.as_mut().map(Child::try_wait) {
                Some(Ok(Some(status))) => break status.code(),
                Some(Ok(None)) => {}
                _ => break None,
            }
            let _ = process.changed.wait_timeout(state, Duration::from_millis(200));
        };
        // Its last lines are all in before they are read.
        for reader in readers.drain(..) {
            let _ = reader.join();
        }
        let Ok(mut state) = process.state.lock() else { return };
        state.child = None;
        let next = next_after(code);
        if !state.wanted || matches!(next, Next::Stay | Next::Fault) {
            if next == Next::Fault && state.wanted {
                let said: Vec<String> = state.tail.iter().cloned().collect();
                state.error = Some(format!("The node's configuration does not check, so it stays stopped:\n{}", said.join("\n")));
            }
            state.wanted = false;
            state.supervised = false;
            process.changed.notify_all();
            drop(state);
            pin(&app, None);
            return;
        }
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
        match spawn(&app, &root) {
            Ok((child, more)) => {
                readers = more;
                began = Instant::now();
                let late_stop = process.state.lock().ok().and_then(|mut state| adopt(&mut state, child));
                if let Some(pid) = late_stop {
                    ask_to_stop(pid, &root);
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
}
