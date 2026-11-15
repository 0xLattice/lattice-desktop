//! The Lattice Desktop shell. Everything privileged lives here, behind a few
//! narrow commands the page invokes:
//!
//! - `lattice`: the bundled `lattice` CLI's verbs (init/status, and the
//!   mining loop's) against the app's host root.
//! - `node_start` / `node_stop` / `node_process`: the node itself, which the
//!   app runs as its own child (`lattice up`, which becomes the bundled
//!   `lattice-node`) and starts again as a process manager would (see
//!   `node_process`); the app never runs the node any other way.
//! - `read_config` / `set_config` / `host_chain`: the node's configuration,
//!   through the CLI (`lattice config get|set|unset`, `lattice child
//!   host|unhost`): read merged, written to the `operator` fragment, and of
//!   it only the node's ports, peers, allowed origins and hosted chains.
//! - `read_miner` / `write_miner`: the mining loop's settings (`miner.json`).
//! - `store_get` / `store_set` / `store_remove`: the wallet's vault and
//!   settings, as 0600 files.
//! - `node_http`: HTTP to 127.0.0.1 on the RPC port the node was started on
//!   (pinned at app launch and each time the app starts the node, never
//!   whatever the page writes later), and nowhere else (the page's CSP gives
//!   it no network of its own). It attaches the node's cookie: the operator
//!   port requires it.
//! - `node_cookie`: that cookie, for pairing a browser extension.
//! - `save_backup`: an encrypted wallet backup as a new 0600 text file in
//!   ~/Downloads (the webview has no downloads of its own).
//! - `print_page`: the webview's print dialog (it has no `window.print`).
//! - `swap_*`: buying NEX with BTC, on lattice-swap's native engine (see
//!   `swap`), its status sent to the page as events.
//!
//! On quit the shell stops the mining loop and the node. While a swap needs
//! the app, it keeps running in the tray, and a quit asks first (see
//! `background`).
//!
//! On Windows the node runs in WSL (see `wsl`): the host root is inside the
//! default distribution, and the CLI is run there.

mod background;
mod node_process;
mod swap;
mod wsl;

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Manager, RunEvent, State, WebviewWindow, WindowEvent};

/// The RPC port the node was started on: the merged configuration's
/// (`lattice config get`), read at launch (a node left running by an earlier
/// session) and each time the app starts the node. The page can change the
/// configuration, but `node_http` and the cookie go only to this port.
struct PinnedPort(Mutex<Option<u16>>);

/// The CLI invocations the shell runs, by name: the host's setup and status,
/// stopping a node (`POST /stop`), and the mining loop's (`lattice mine
/// start|stop|status`). Never `lattice up`: it runs until the node exits, and
/// only `node_process` runs the node.
fn verb_arguments(verb: &str) -> Option<&'static [&'static str]> {
    match verb {
        "init" => Some(&["init"]),
        "down" => Some(&["down"]),
        "status" => Some(&["status"]),
        "mine-start" => Some(&["mine", "start"]),
        "mine-stop" => Some(&["mine", "stop"]),
        "mine-status" => Some(&["mine", "status"]),
        _ => None,
    }
}
const STORE_KEYS: [&str; 2] = ["vault", "settings"];
const MAX_NODE_BODY: u64 = 8 * 1024 * 1024;

type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The platform's per-user data directory, then `Lattice`:
/// ~/Library/Application Support/Lattice on macOS, ~/.local/share/Lattice
/// (or $XDG_DATA_HOME/Lattice) on Linux.
fn app_root(app: &AppHandle) -> Result<PathBuf> {
    Ok(app.path().data_dir().map_err(err)?.join("Lattice"))
}

/// The node's host root, as this process reads and writes it.
#[cfg(not(windows))]
fn host_root(app: &AppHandle) -> Result<PathBuf> {
    Ok(app_root(app)?.join("host"))
}

/// The node's host root, as this process reads and writes it: inside the
/// WSL distribution the node runs in.
#[cfg(windows)]
fn host_root(_app: &AppHandle) -> Result<PathBuf> {
    let session = wsl::session()?;
    Ok(wsl::windows_path(&session.distro, &format!("{}/host", wsl::data_dir(&session))))
}

/// The package's resource directory, set once at startup. A package may keep
/// the bundled binaries there instead of beside the app's executable, in
/// `<resources>/bin`: a Linux package installs its executable into a shared
/// directory (/usr/bin), where a `lattice` beside it may be someone else's.
static RESOURCES: OnceLock<PathBuf> = OnceLock::new();

/// A bundled binary: in the package's own `<resources>/bin` when it has one,
/// otherwise beside the app's executable (Contents/MacOS in the .app,
/// target/<profile> under `tauri dev`).
fn bundled(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(err)?;
    let beside = exe.parent().ok_or("no executable directory")?.join(name);
    Ok(bundled_in(RESOURCES.get().map(|dir| dir.join("bin")).as_deref(), name).unwrap_or(beside))
}

fn bundled_in(resources: Option<&Path>, name: &str) -> Option<PathBuf> {
    resources.map(|dir| dir.join(name)).filter(|path| path.is_file())
}

#[derive(Serialize, Debug)]
struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

/// The bundled CLI, ready for a verb's arguments, against `root`.
#[cfg(not(windows))]
fn lattice_command(root: &Path) -> Result<(Command, std::ffi::OsString)> {
    Ok((Command::new(bundled("lattice")?), root.as_os_str().to_owned()))
}

/// The bundled CLI, ready for a verb's arguments, against `root`: the Linux
/// binary inside the WSL distribution, told the root's Linux path.
#[cfg(windows)]
fn lattice_command(root: &Path) -> Result<(Command, std::ffi::OsString)> {
    let session = wsl::session()?;
    let linux_root = wsl::linux_path(&session.distro, root).ok_or("the host root is not inside WSL")?;
    let binaries = wsl::install(&session, RESOURCES.get().ok_or("no resource directory")?)?;
    let mut command = wsl::command();
    command.args(["-d", &session.distro, "-e", &format!("{binaries}/lattice")]);
    Ok((command, linux_root.into()))
}

fn run_lattice(root: &Path, verb: &str) -> Result<CommandResult> {
    let arguments = verb_arguments(verb).ok_or_else(|| format!("refused: lattice {verb}"))?;
    run_cli(root, arguments)
}

/// The bundled CLI with `arguments`, against `root`. Only the shell's own
/// callers name the arguments: the page reaches it through checked commands.
fn run_cli(root: &Path, arguments: &[impl AsRef<std::ffi::OsStr>]) -> Result<CommandResult> {
    fs::create_dir_all(root).map_err(err)?;
    let (mut command, root) = lattice_command(root)?;
    let output = command
        .args(arguments)
        .arg("--root")
        .arg(root)
        .output()
        .map_err(|e| format!("could not run the bundled lattice: {e}"))?;
    Ok(CommandResult {
        code: output.status.code().unwrap_or(-1),
        stdout: wsl::decode(&output.stdout),
        stderr: wsl::decode(&output.stderr),
    })
}

#[tauri::command]
async fn lattice(app: AppHandle, verb: String) -> Result<CommandResult> {
    let root = host_root(&app)?;
    if verb == "mine-start" {
        let miner_root = root.clone();
        // Settings saved when the graphics chip worked (or copied from a
        // machine where it did) must not start a loop that finds nothing.
        let usable = tauri::async_runtime::spawn_blocking(move || -> Result<bool> {
            Ok(!mines_on_gpu(&miner_root)? || gpu_searches())
        })
        .await
        .map_err(err)??;
        if !usable {
            return Err(NO_GPU.into());
        }
        repoint_gpu_worker(&root, gpu_worker_path().as_deref())?;
    }
    tauri::async_runtime::spawn_blocking(move || run_lattice(&root, &verb))
        .await
        .map_err(err)?
}

/// Starts the node (see `node_process`); resolves once it is running.
#[tauri::command]
async fn node_start(app: AppHandle) -> Result<()> {
    tauri::async_runtime::spawn_blocking(move || node_process::start(&app)).await.map_err(err)?
}

/// Stops the node; resolves once it has exited.
#[tauri::command]
async fn node_stop(app: AppHandle) -> Result<()> {
    tauri::async_runtime::spawn_blocking(move || node_process::stop(&app)).await.map_err(err)?
}

/// Whether the app runs the node, and why it stopped by itself when it did.
#[tauri::command]
fn node_process(process: State<'_, node_process::NodeProcess>) -> Result<node_process::Report> {
    process.report()
}

/// Opens the swap wallet on the pinned port, off this thread, its swaps
/// resumed (see `swap::sync`).
fn sync_swaps(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = swap::sync(&app) {
            eprintln!("lattice-desktop: could not resume swaps: {e}");
        }
    });
}

/// The GPU mining worker is this platform's own program everywhere: Metal on
/// macOS, OpenCL on Linux and on Windows, where it is the one bundled binary
/// that is not a Linux one (WSL offers Linux programs no graphics chip the
/// worker could use, but runs a Windows program the mining loop names).
const GPU_WORKER: &str = if cfg!(windows) { "lattice-miner-gpu.exe" } else { "lattice-miner-gpu" };

/// The bundled GPU mining worker, if this build carries one.
fn gpu_worker_file() -> Option<PathBuf> {
    bundled(GPU_WORKER).ok().filter(|path| path.is_file())
}

/// The bundled GPU worker's path as miner.json names it, which is how the
/// mining loop reaches it.
#[cfg(not(windows))]
fn gpu_worker_path() -> Option<String> {
    gpu_worker_file().map(|path| path.to_string_lossy().into_owned())
}

/// The bundled GPU worker's path as miner.json names it, which is how the
/// mining loop reaches it: from inside WSL, where Windows' drives are mounted.
#[cfg(windows)]
fn gpu_worker_path() -> Option<String> {
    // Kept once found. A failure is asked again: the distribution may only
    // have been starting.
    static MOUNTED: Mutex<Option<String>> = Mutex::new(None);
    let mut mounted = MOUNTED.lock().ok()?;
    if mounted.is_none() {
        *mounted = wsl::mounted_path(&wsl::session().ok()?, &gpu_worker_file()?);
    }
    mounted.clone()
}

/// Whether `worker` can search on this machine's graphics chip: it is given a
/// small range every hash of which wins. A machine with no chip the worker
/// can use, or no driver for it, fails here instead of mining nothing. A
/// worker still silent after `patience` (a driver that hangs) is stopped and
/// counts as one that cannot.
fn gpu_worker_searches(worker: &Path, patience: Duration) -> bool {
    use std::process::Stdio;
    let mut command = Command::new(worker);
    command.args(["--work-id", "probe", "--prefix-hex", "00", "--start-nonce", "0", "--count", "1024", "--target", &"ff".repeat(32)]);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = command.spawn() else { return false };
    let deadline = std::time::Instant::now() + patience;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    };
    // One short line: it fits the pipe, so reading after the exit cannot block.
    let mut answer = String::new();
    let _ = child.stdout.take().map(|mut out| out.read_to_string(&mut answer));
    status.success() && answer.contains(r#""status":"found""#)
}

const GPU_PROBE_PATIENCE: Duration = Duration::from_secs(30);
const NO_GPU: &str = "This computer has no graphics chip Lattice can mine on: it needs a graphics driver that provides OpenCL. Choose the processor in Mining.";

/// Whether the bundled GPU worker can search on this machine, found out once
/// per launch.
fn gpu_searches() -> bool {
    static SEARCHES: OnceLock<bool> = OnceLock::new();
    let Some(file) = gpu_worker_file() else { return false };
    *SEARCHES.get_or_init(|| gpu_worker_searches(&file, GPU_PROBE_PATIENCE))
}

/// Whether the miner's settings have it search with a GPU worker (by file
/// name: the path is repointed at this build's before it is used).
fn mines_on_gpu(root: &Path) -> Result<bool> {
    let current = miner_settings(root)?;
    let worker = current.as_ref().and_then(|t| t.get("worker")).and_then(Value::as_str);
    Ok(worker.is_some_and(|worker| Path::new(worker).file_name().is_some_and(|name| name == GPU_WORKER)))
}

/// The bundled GPU worker's path, the only worker path the page may name
/// back to `write_miner`: None in a build that carries none.
#[tauri::command]
async fn gpu_worker() -> Option<String> {
    tauri::async_runtime::spawn_blocking(gpu_worker_path).await.ok().flatten()
}

/// Whether the page may offer the graphics chip: this build carries the GPU
/// worker and it finds a hash on this machine.
#[tauri::command]
async fn gpu_worker_usable() -> bool {
    tauri::async_runtime::spawn_blocking(gpu_searches).await.unwrap_or(false)
}

/// The end of the mining loop's own log, for the page to say how it is going:
/// at most the last 64 KiB of `<root>/log/mine.log`, "" when there is none.
#[tauri::command]
fn mine_log(app: AppHandle) -> Result<String> {
    Ok(tail(&host_root(&app)?.join("log").join("mine.log"), 64 * 1024))
}

fn tail(path: &Path, most: usize) -> String {
    let Ok(bytes) = fs::read(path) else { return String::new() };
    let start = bytes.len().saturating_sub(most);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// The miner's settings name the GPU worker by its path inside this app. If
/// the app was moved since, point them at where the worker is now, so mining
/// does not fail on (or run) a file at the old place.
fn repoint_gpu_worker(root: &Path, bundled: Option<&str>) -> Result<()> {
    let Some(mut current) = miner_settings(root)? else { return Ok(()) };
    let Some(worker) = current.get("worker").and_then(Value::as_str) else { return Ok(()) };
    let names_gpu = Path::new(worker).file_name().is_some_and(|name| name == GPU_WORKER);
    let Some(bundled) = bundled.filter(|path| names_gpu && *path != worker) else { return Ok(()) };
    current["worker"] = Value::from(bundled);
    write_miner_settings(root, &current)
}

/// The mining loop's settings, as it reads them: `<root>/miner.json`, or,
/// while there is none, the `mine` object an older lattice.json holds (which
/// the miner copies to miner.json, without its `enabled`, when it starts).
fn miner_settings(root: &Path) -> Result<Option<Value>> {
    if let Some(settings) = read_topology_at(&root.join("miner.json"))? {
        return Ok(Some(settings));
    }
    let mut mine = read_topology_at(&root.join("lattice.json"))?.and_then(|mut t| t.get_mut("mine").map(Value::take));
    if let Some(Value::Object(fields)) = mine.as_mut() {
        fields.remove("enabled");
    }
    Ok(mine)
}

/// Writes miner.json, which the miner reads from then on. A `mine` object
/// an older lattice.json holds is left: neither the miner nor the node reads
/// it once miner.json exists.
fn write_miner_settings(root: &Path, settings: &Value) -> Result<()> {
    write_atomically(&root.join("miner.json"), &serde_json::to_vec_pretty(settings).map_err(err)?, 0o644)
}

#[tauri::command]
fn read_miner(app: AppHandle) -> Result<Option<Value>> {
    miner_settings(&host_root(&app)?)
}

#[tauri::command]
fn write_miner(app: AppHandle, settings: Value) -> Result<()> {
    let root = host_root(&app)?;
    let merged = merged_mine(miner_settings(&root)?.as_ref(), &settings, gpu_worker_path().as_deref())?;
    write_miner_settings(&root, &merged)
}

/// The cookie the node writes at every start (`lattice up` gives it
/// `<root>/chains/Nexus` as its data directory), re-read on every use.
fn cookie_path(root: &Path) -> PathBuf {
    root.join("chains").join("Nexus").join(".cookie")
}

fn read_cookie(root: &Path) -> Option<String> {
    let text = fs::read_to_string(cookie_path(root)).ok()?;
    let cookie = text.trim();
    (!cookie.is_empty()).then(|| cookie.to_string())
}

/// `Authorization: Basic base64(<cookie file content>)`, bitcoind-style.
fn cookie_authorization(root: &Path) -> Option<String> {
    use base64::Engine;
    read_cookie(root).map(|c| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(c)))
}

/// Whether the node on `port` answers an operator route with this root's cookie.
fn node_accepts_cookie(port: u16, root: &Path) -> bool {
    let Some(authorization) = cookie_authorization(root) else { return false };
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .redirects(0)
        .build()
        .get(&format!("http://127.0.0.1:{port}/status"))
        .set("Authorization", &authorization)
        .call()
        .is_ok_and(|r| r.status() == 200)
}

#[tauri::command]
fn node_cookie(app: AppHandle) -> Result<Option<String>> {
    Ok(read_cookie(&host_root(&app)?))
}

fn read_topology_at(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(err),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(e)),
    }
}

/// Writes `bytes` to `path` via a same-directory temp file and rename, with `mode`.
fn write_atomically(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_extension("tmp");
    // A fresh file every time, so `mode` always applies (never a stale temp's).
    let _ = fs::remove_file(&tmp);
    let mut file = new_file(mode).open(&tmp).map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    synced(&file, &tmp)?;
    fs::rename(&tmp, path).map_err(err)
}

/// Options for a file that must not exist yet, private to `mode` where the
/// platform has file modes. On Windows a file is as private as its
/// directory: the user's own profile, or their home inside WSL.
fn new_file(mode: u32) -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    options
}

/// Flushed to disk before it is renamed into place. The WSL file share does
/// not take a flush request, so for a file there a refusal is not an error.
fn synced(file: &fs::File, path: &Path) -> Result<()> {
    let flushed = file.sync_all();
    if cfg!(windows) && path.to_string_lossy().starts_with(r"\\wsl$\") {
        return Ok(());
    }
    flushed.map_err(err)
}

/// The most CPU workers the page may ask for.
const MAX_WORKERS: u64 = 64;

/// A chain path as configuration names one: `Nexus` or `Nexus/a/b`.
fn is_chain_path(value: &str) -> bool {
    let mut parts = value.split('/');
    parts.next() == Some("Nexus")
        && parts.all(|p| !p.is_empty() && p.len() <= 64 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// An account address: a base32 CID, as the wallet shows it.
fn is_address(value: &str) -> bool {
    (40..=128).contains(&value.len())
        && value.starts_with('b')
        && value.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `current` mining settings with the page's choices applied: the worker
/// count, each chain's recipient, and optionally which bundled worker
/// searches. `worker` is a program the miner runs, so the page may name
/// only `cpu` (the CLI's own bundled worker) or `gpu_worker`, the GPU worker
/// bundled in this app; left out, the worker stays as the operator set it.
/// Every tuning field is kept as it is.
fn merged_mine(current: Option<&Value>, update: &Value, gpu_worker: Option<&str>) -> Result<Value> {
    let update = update.as_object().ok_or("the miner's settings must be an object")?;
    if let Some(key) = update.keys().find(|k| !["workers", "recipients", "worker"].contains(&k.as_str())) {
        return Err(format!("refused: miner.json field {key}"));
    }
    let worker = match update.get("worker") {
        None => None,
        Some(value) => match value.as_str() {
            Some("cpu") => Some("cpu"),
            Some(path) if gpu_worker == Some(path) => Some(path),
            _ => return Err("refused: the worker may be only one bundled with this app".into()),
        },
    };
    let workers = update.get("workers").and_then(Value::as_u64).filter(|n| (1..=MAX_WORKERS).contains(n))
        .ok_or(format!("workers must be a whole number from 1 to {MAX_WORKERS}"))?;
    let recipients = update.get("recipients").and_then(Value::as_object).ok_or("recipients must map chain paths to addresses")?;
    for (chain, address) in recipients {
        if !is_chain_path(chain) {
            return Err(format!("not a chain path: {chain}"));
        }
        if !address.as_str().is_some_and(is_address) {
            return Err(format!("not an address for {chain}"));
        }
    }
    let mut out = current.and_then(Value::as_object).cloned().unwrap_or_default();
    out.insert("workers".into(), Value::from(workers));
    out.insert("recipients".into(), Value::Object(recipients.clone()));
    if let Some(worker) = worker {
        out.insert("worker".into(), Value::from(worker));
    }
    Ok(Value::Object(out))
}

/// A browser origin as the node's `rpcAllowedOrigins` lists it:
/// `scheme://host[:port]`, nothing after.
fn is_origin(value: &str) -> bool {
    match value.split_once("://") {
        Some((scheme, rest)) => {
            scheme.ends_with("-extension")
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
                && !rest.is_empty()
                && !rest.chars().any(|c| c == '/' || c.is_whitespace() || c.is_control() || c == '\\' || c == '?' || c == '#' || c == '@')
        }
        None => false,
    }
}

/// The `lattice config set` assignments (`key=<JSON>`) the page may ask
/// for, checked: the node's loopback RPC port and peer port, a chain's peer
/// list, and the browser origins allowed on the operator port. Anything else
/// (public listeners, external address, ...) is the operator's: the page
/// cannot expose the node or point `node_http` elsewhere beyond choosing the
/// node's own loopback RPC port.
fn config_assignments(set: &serde_json::Map<String, Value>) -> Result<Vec<String>> {
    let strings = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(Value::is_string));
    let mut out = Vec::new();
    for (key, value) in set {
        let ok = match key.as_str() {
            "rpc" | "listen" => value.as_u64().is_some_and(|p| (1..=65535).contains(&p)),
            "rpcAllowedOrigins" => value.as_array().is_some_and(|a| a.iter().all(|o| o.as_str().is_some_and(is_origin))),
            _ => key.strip_prefix("peers.").is_some_and(is_chain_path) && strings(value),
        };
        if !ok {
            return Err(format!("refused: {key}={value}"));
        }
        out.push(format!("{key}={value}"));
    }
    Ok(out)
}

/// The `lattice config unset` keys the page may ask for: a chain's peer list.
fn config_removals(unset: &[String]) -> Result<Vec<String>> {
    match unset.iter().find(|key| !key.strip_prefix("peers.").is_some_and(is_chain_path)) {
        Some(bad) => Err(format!("refused: unset {bad}")),
        None => Ok(unset.to_vec()),
    }
}

/// The merged configuration (`lattice config get`, the running node's when
/// it runs); None before `lattice init`.
fn merged_config(root: &Path) -> Result<Option<Value>> {
    if !root.join("lattice.json").is_file() {
        return Ok(None);
    }
    let result = config_get(root)?;
    if result.code != 0 {
        return Err(format!("lattice config get failed: {}", result.stderr.trim()));
    }
    serde_json::from_str(&result.stdout).map(Some).map_err(err)
}

/// How long `lattice config get` may take. It asks a running node, and a
/// node that holds the storage but does not answer (one exiting, or one
/// between a restart's exit and its start) would keep it waiting for minutes.
#[cfg_attr(test, allow(dead_code))]
const CONFIG_PATIENCE: Duration = Duration::from_secs(5);

/// `lattice config get`, given up after `CONFIG_PATIENCE`.
#[cfg(not(test))]
fn config_get(root: &Path) -> Result<CommandResult> {
    run_cli_within(root, &["config", "get"], CONFIG_PATIENCE)
}

/// `run_cli`, given up after `patience`.
fn run_cli_within(root: &Path, arguments: &[&str], patience: Duration) -> Result<CommandResult> {
    fs::create_dir_all(root).map_err(err)?;
    let (mut command, root) = lattice_command(root)?;
    command.args(arguments).arg("--root").arg(root);
    output_within(command, patience)
}

/// What `command` printed and how it exited; killed, and an error, if it
/// has not exited after `patience`.
fn output_within(mut command: Command, patience: Duration) -> Result<CommandResult> {
    use std::process::Stdio;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("could not run the bundled lattice: {e}"))?;
    // Read as it comes, so a long answer cannot fill the pipe and stall it.
    let read = |out: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut out) = out {
                let _ = out.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|o| Box::new(o) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + patience;
    let status = loop {
        match child.try_wait().map_err(err)? {
            Some(status) => break status,
            None if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("lattice gave no answer in {}s", patience.as_secs()));
            }
        }
    };
    Ok(CommandResult {
        code: status.code().unwrap_or(-1),
        stdout: wsl::decode(&stdout.join().unwrap_or_default()),
        stderr: wsl::decode(&stderr.join().unwrap_or_default()),
    })
}

/// For tests, what `lattice config get` prints, without a process: the
/// root's `merged.json` (a configuration lattice.json alone would not make),
/// else its lattice.json. A process spawned here would hold, until it
/// starts, the lock of a swap wallet another test has just closed.
#[cfg(test)]
fn config_get(root: &Path) -> Result<CommandResult> {
    let stdout = fs::read_to_string(root.join("merged.json")).or_else(|_| fs::read_to_string(root.join("lattice.json"))).map_err(err)?;
    Ok(CommandResult { code: 0, stdout, stderr: String::new() })
}

/// The RPC port the merged configuration names: the one the node starts on.
fn configured_rpc(root: &Path) -> Option<u16> {
    let rpc = merged_config(root).ok()??.get("rpc")?.as_u64()?;
    u16::try_from(rpc).ok().filter(|p| *p != 0)
}

/// The RPC port a running node listens on: from the address it writes
/// beside its cookie at each start (and removes at exit), else, with none
/// written, the one its configuration names. A node that crashed leaves
/// it: whether a node answers there is the caller's to find out.
fn live_rpc(root: &Path) -> Option<u16> {
    match fs::read_to_string(root.join("chains").join("Nexus").join(".rpc-address")) {
        Ok(address) => address.trim().rsplit_once(':')?.1.trim_end_matches('/').parse().ok().filter(|p| *p != 0),
        Err(_) => configured_rpc(root),
    }
}

#[tauri::command]
async fn read_config(app: AppHandle) -> Result<Option<Value>> {
    let root = host_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || merged_config(&root)).await.map_err(err)?
}

/// Sets and removes keys of the operator fragment (`lattice config set`,
/// then `lattice config unset`), as checked by `config_assignments` and
/// `config_removals`. The CLI writes through a running node, or the file
/// while it is stopped; the first failure is answered.
#[tauri::command]
async fn set_config(app: AppHandle, set: serde_json::Map<String, Value>, unset: Vec<String>) -> Result<CommandResult> {
    let root = host_root(&app)?;
    let mut steps: Vec<Vec<String>> = Vec::new();
    let assignments = config_assignments(&set)?;
    if !assignments.is_empty() {
        steps.push([vec!["config".into(), "set".into()], assignments].concat());
    }
    let removals = config_removals(&unset)?;
    if !removals.is_empty() {
        steps.push([vec!["config".into(), "unset".into()], removals].concat());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let mut said = CommandResult { code: 0, stdout: String::new(), stderr: String::new() };
        for step in steps {
            let result = run_cli(&root, &step)?;
            said.stdout += &result.stdout;
            said.stderr += &result.stderr;
            said.code = result.code;
            if result.code != 0 {
                break;
            }
        }
        Ok(said)
    })
    .await
    .map_err(err)?
}

/// Hosts one child chain, or stops hosting it (`lattice child host|unhost`):
/// one path, so the ancestors it needs stay implied, and a chain another
/// layer lists is refused by the CLI, which names it.
#[tauri::command]
async fn host_chain(app: AppHandle, path: String, hosted: bool) -> Result<CommandResult> {
    if !is_chain_path(&path) || path == "Nexus" {
        return Err(format!("not a child chain path: {path}"));
    }
    let root = host_root(&app)?;
    let verb = if hosted { "host" } else { "unhost" };
    tauri::async_runtime::spawn_blocking(move || run_cli(&root, &["child", verb, &path])).await.map_err(err)?
}

fn store_path(app: &AppHandle, key: &str) -> Result<PathBuf> {
    if !STORE_KEYS.contains(&key) {
        return Err(format!("refused: store key {key}"));
    }
    let dir = app_root(app)?.join("wallet");
    fs::create_dir_all(&dir).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(err)?;
    }
    Ok(dir.join(format!("{key}.json")))
}

#[tauri::command]
fn store_get(app: AppHandle, key: String) -> Result<Option<Value>> {
    read_topology_at(&store_path(&app, &key)?)
}

#[tauri::command]
fn store_set(app: AppHandle, key: String, value: Value) -> Result<()> {
    let path = store_path(&app, &key)?;
    write_atomically(&path, &serde_json::to_vec(&value).map_err(err)?, 0o600)
}

#[tauri::command]
fn store_remove(app: AppHandle, key: String) -> Result<()> {
    match fs::remove_file(store_path(&app, &key)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
        _ => Ok(()),
    }
}

/// A backup file name the page may ask for: a plain `.txt` name, no path.
fn backup_name(name: &str) -> Result<&str> {
    let ok = name.len() <= 100
        && name.ends_with(".txt")
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(name)
    } else {
        Err(format!("refused: file name {name}"))
    }
}

/// Write `text` to a new file in `dir` (never over an existing one), 0600.
fn write_new(dir: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "txt"));
    for n in 0..100 {
        let file = if n == 0 { name.to_string() } else { format!("{stem}-{n}.{ext}") };
        let path = dir.join(file);
        match new_file(0o600).open(&path) {
            Ok(mut f) => {
                f.write_all(text.as_bytes()).map_err(err)?;
                synced(&f, &path)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(err(e)),
        }
    }
    Err("refused: too many files with that name".into())
}

#[tauri::command]
fn save_backup(app: AppHandle, name: String, text: String) -> Result<String> {
    if text.len() > 1024 * 1024 || !text.contains("ur:lattice-vault/") {
        return Err("refused: not a wallet backup".into());
    }
    let dir = app.path().download_dir().map_err(err)?;
    Ok(write_new(&dir, backup_name(&name)?, &text)?.display().to_string())
}

#[tauri::command]
fn print_page(window: WebviewWindow) -> Result<()> {
    window.print().map_err(err)
}

#[derive(Serialize, Debug)]
struct NodeAnswer {
    status: u16,
    body: String,
}

/// A request path for the node: absolute, no authority, no whitespace.
fn checked_path(path: &str) -> Result<&str> {
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('#')
        && !path.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\');
    if ok {
        Ok(path)
    } else {
        Err(format!("refused: path {path}"))
    }
}

fn node_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<String>,
    authorization: Option<String>,
) -> Result<NodeAnswer> {
    let url = format!("http://127.0.0.1:{port}{}", checked_path(path)?);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .redirects(0)
        .build();
    let request = match method {
        "GET" => agent.get(&url),
        "POST" => agent.post(&url).set("Content-Type", "application/json"),
        other => return Err(format!("refused: {other}")),
    }
    .set("Accept", "application/json");
    let request = match authorization {
        Some(value) => request.set("Authorization", &value),
        None => request,
    };
    let response = match body {
        Some(body) if method == "POST" => request.send_string(&body),
        _ => request.call(),
    };
    let response = match response {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("node unreachable: {e}")),
    };
    let status = response.status();
    let mut text = String::new();
    response
        .into_reader()
        .take(MAX_NODE_BODY + 1)
        .read_to_string(&mut text)
        .map_err(err)?;
    if text.len() as u64 > MAX_NODE_BODY {
        return Err("node answer too large".into());
    }
    Ok(NodeAnswer { status, body: text })
}

#[tauri::command]
async fn node_http(
    app: AppHandle,
    pinned: State<'_, PinnedPort>,
    method: String,
    path: String,
    body: Option<String>,
) -> Result<NodeAnswer> {
    let port = (*pinned.0.lock().map_err(err)?).ok_or("node unreachable: start the node first")?;
    let authorization = cookie_authorization(&host_root(&app)?);
    tauri::async_runtime::spawn_blocking(move || node_request(port, &method, &path, body, authorization))
        .await
        .map_err(err)?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        // First, as the plugin requires: a second launch shows this one.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            background::show(app)
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .on_menu_event(|app, event| background::menu_event(app, event.id().as_ref()))
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if background::hold_close(window) {
                    api.prevent_close();
                }
            }
        });
    #[cfg(target_os = "macos")]
    let builder = builder.menu(background::app_menu);
    builder
        .setup(|app| {
            let handle = app.handle();
            if let Ok(resources) = handle.path().resource_dir() {
                let _ = RESOURCES.set(resources);
            }
            app.manage(PinnedPort(Mutex::new(None)));
            app.manage(node_process::NodeProcess::default());
            app.manage(swap::Swaps::default());
            // Before any swap reports.
            background::start(handle)?;
            // Swaps that must carry on, resumed on their engine threads once
            // the node's port is pinned (below, or at the first `up`).
            sync_swaps(handle);
            // Only a running node (it keeps a cookie; it removes it at exit)
            // is pinned at launch; otherwise the first `up` pins.
            // A crash leaves a stale cookie, so the port is pinned only when
            // the node there accepts that cookie on an operator route.
            // Off the main thread: reading the configuration runs the CLI.
            let launched = handle.clone();
            std::thread::spawn(move || {
                let Ok(root) = host_root(&launched) else { return };
                let Some(port) = live_rpc(&root).filter(|p| node_accepts_cookie(*p, &root)) else { return };
                let state = launched.state::<PinnedPort>();
                let Ok(mut pinned) = state.0.lock() else { return };
                // An `up` since launch pinned its own port: that one stands.
                if pinned.is_none() {
                    *pinned = Some(port);
                    drop(pinned);
                    sync_swaps(&launched);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            lattice,
            node_start,
            node_stop,
            node_process,
            read_config,
            set_config,
            host_chain,
            read_miner,
            write_miner,
            store_get,
            store_set,
            store_remove,
            node_http,
            node_cookie,
            gpu_worker,
            gpu_worker_usable,
            mine_log,
            save_backup,
            print_page,
            swap::swap_settings,
            swap::swap_set_network,
            swap::swap_set_servers,
            swap::swap_check_esplora,
            swap::swap_check_nexus,
            swap::swap_wallet_exists,
            swap::swap_wallet_open,
            swap::swap_balance,
            swap::swap_need,
            swap::swap_lock_fee,
            swap::swap_start,
            swap::swap_cancel,
            swap::swap_status,
            swap::swap_backup,
            swap::swap_restore_file,
            swap::swap_withdraw,
            swap::swap_offers,
            swap::swap_relay_directory
        ])
        .build(tauri::generate_context!())
        .expect("error while building the app")
        .run(|app, event| match event {
            RunEvent::ExitRequested { api, .. } => {
                if background::hold_exit(app) {
                    api.prevent_exit();
                }
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => background::show(app),
            RunEvent::Exit => {
                // A clean stop for the mining loop and the node this app
                // runs; each is a no-op when it is not running. Mining
                // first: it mines on the node.
                if let Ok(root) = host_root(app) {
                    let _ = run_lattice(&root, "mine-stop");
                }
                let _ = node_process::stop(app);
                #[cfg(windows)]
                wsl::release();
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backups_save_as_new_plain_files_only() {
        assert!(backup_name("lattice-wallet-backup-2026-10-05.txt").is_ok());
        for bad in ["../x.txt", "a/b.txt", ".hidden.txt", "x.sh", "x .txt", ""] {
            assert!(backup_name(bad).is_err(), "{bad}");
        }
        let dir = std::env::temp_dir().join(format!("lattice-save-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = write_new(&dir, "b.txt", "one").unwrap();
        let b = write_new(&dir, "b.txt", "two").unwrap();
        assert_ne!(a, b);
        assert_eq!(fs::read_to_string(&a).unwrap(), "one", "never overwritten");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_page_sets_only_who_mining_pays_and_how_many_workers() {
        let address = "bafyreifrbp6x4ncfbqakwfcwfqkl7qhbsdeygsakjxixekwkdprbomyut4";
        let current = serde_json::json!({ "worker": "/opt/gpu-miner", "batchSize": 5, "workers": 9, "recipients": { "Nexus": "old" } });
        let update = serde_json::json!({ "workers": 2, "recipients": { "Nexus": address, "Nexus/testnet": address } });
        assert_eq!(merged_mine(Some(&current), &update, None).unwrap(), serde_json::json!({
            "worker": "/opt/gpu-miner", "batchSize": 5, "workers": 2,
            "recipients": { "Nexus": address, "Nexus/testnet": address },
        }));
        // The page never names the program the CLI runs, nor tunes the search.
        for bad in [
            serde_json::json!({ "workers": 1, "recipients": {}, "worker": "/tmp/evil" }),
            serde_json::json!({ "workers": 1, "recipients": {}, "minWork": { "Nexus": "1" } }),
            serde_json::json!({ "workers": 0, "recipients": {} }),
            serde_json::json!({ "workers": 65, "recipients": {} }),
            serde_json::json!({ "workers": 1 }),
            serde_json::json!({ "workers": 1, "recipients": { "Nexus": "not an address" } }),
            serde_json::json!({ "workers": 1, "recipients": { "../etc": address } }),
            serde_json::json!({ "workers": 1, "recipients": { "Other": address } }),
        ] {
            assert!(merged_mine(Some(&current), &bad, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_page_names_only_a_bundled_worker() {
        let gpu = "/Applications/Lattice.app/Contents/MacOS/lattice-miner-gpu";
        let current = serde_json::json!({ "worker": "/opt/custom-miner", "workers": 1, "recipients": {} });
        let choose = |worker: &str, bundled: Option<&str>| merged_mine(
            Some(&current),
            &serde_json::json!({ "workers": 1, "recipients": {}, "worker": worker }),
            bundled,
        );
        assert_eq!(choose(gpu, Some(gpu)).unwrap()["worker"], gpu);
        assert_eq!(choose("cpu", Some(gpu)).unwrap()["worker"], "cpu");
        // Not the operator's own worker, not a look-alike elsewhere, and not the GPU worker in a build without one.
        for bad in ["/opt/custom-miner", "/tmp/lattice-miner-gpu", "gpu", ""] {
            assert!(choose(bad, Some(gpu)).is_err(), "{bad}");
        }
        assert!(choose(gpu, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_gpu_worker_is_offered_only_where_it_finds_a_hash() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lattice-gpu-probe-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let worker = |name: &str, script: &str| {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // It is asked for a range it cannot miss in, and has to answer with a find.
        let asked = worker("asked", r#"[ "$*" = "--work-id probe --prefix-hex 00 --start-nonce 0 --count 1024 --target ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff" ] && echo '{"workId":"probe","status":"found","nonce":0}'"#);
        let patient = Duration::from_secs(30);
        assert!(gpu_worker_searches(&asked, patient));
        // No device: the worker says why and fails.
        assert!(!gpu_worker_searches(&worker("no-device", "echo 'error: opencl backend failed' >&2; exit 1"), patient));
        // A worker that searched nothing must not pass for one that works.
        assert!(!gpu_worker_searches(&worker("exhausted", r#"echo '{"workId":"probe","status":"exhausted"}'"#), patient));
        assert!(!gpu_worker_searches(&worker("found-but-failed", r#"echo '{"status":"found"}'; exit 3"#), patient));
        assert!(!gpu_worker_searches(&dir.join("missing"), patient));
        // A driver that hangs is given up on, not waited for.
        let began = std::time::Instant::now();
        assert!(!gpu_worker_searches(&worker("hangs", "exec sleep 60"), Duration::from_millis(300)));
        assert!(began.elapsed() < Duration::from_secs(10));
        // Mining is on the graphics chip when miner.json names a GPU worker, wherever it was.
        assert!(!mines_on_gpu(&dir).unwrap(), "no settings yet");
        for (worker, on_gpu) in [(format!("/moved/{GPU_WORKER}"), true), ("cpu".to_string(), false), ("/opt/custom-miner".to_string(), false)] {
            fs::write(dir.join("miner.json"), serde_json::to_vec(&serde_json::json!({ "worker": worker })).unwrap()).unwrap();
            assert_eq!(mines_on_gpu(&dir).unwrap(), on_gpu, "{worker}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_moved_app_repoints_its_gpu_worker_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("lattice-repoint-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("miner.json");
        let write = |worker: &str| fs::write(&file, serde_json::to_vec(&serde_json::json!({ "worker": worker, "workers": 1 })).unwrap()).unwrap();
        let worker = || read_topology_at(&file).unwrap().unwrap()["worker"].as_str().unwrap().to_string();
        // The worker's file name is the platform's (`.exe` on Windows).
        let (now, before, old) = (format!("/app/now/{GPU_WORKER}"), format!("/app/before/{GPU_WORKER}"), format!("/old/{GPU_WORKER}"));
        let now = now.as_str();
        write(&before);
        repoint_gpu_worker(&dir, Some(now)).unwrap();
        assert_eq!(worker(), now);
        for kept in ["cpu", "/opt/custom-miner"] {
            write(kept);
            repoint_gpu_worker(&dir, Some(now)).unwrap();
            assert_eq!(worker(), kept);
        }
        write(&old);
        repoint_gpu_worker(&dir, None).unwrap();
        assert_eq!(worker(), old, "a build without the worker changes nothing");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mining_settings_are_miner_json_or_an_older_lattice_json_mine() {
        let dir = std::env::temp_dir().join(format!("lattice-miner-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(miner_settings(&dir).unwrap(), None);
        let topology = dir.join("lattice.json");
        fs::write(&topology, serde_json::to_vec(&serde_json::json!({ "rpc": 1, "mine": { "enabled": true, "workers": 2 } })).unwrap()).unwrap();
        // As the miner copies it: without `enabled`, which miner.json does not take.
        assert_eq!(miner_settings(&dir).unwrap(), Some(serde_json::json!({ "workers": 2 })));
        write_miner_settings(&dir, &serde_json::json!({ "workers": 3 })).unwrap();
        assert_eq!(miner_settings(&dir).unwrap(), Some(serde_json::json!({ "workers": 3 })));
        assert_eq!(read_topology_at(&topology).unwrap().unwrap()["rpc"], 1, "lattice.json is not written");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_packages_own_binaries_are_used_before_any_beside_the_executable() {
        let dir = std::env::temp_dir().join(format!("lattice-resources-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(bundled_in(Some(&dir), "lattice"), None, "not there: fall back to beside the executable");
        assert_eq!(bundled_in(None, "lattice"), None);
        fs::write(dir.join("lattice"), b"").unwrap();
        assert_eq!(bundled_in(Some(&dir), "lattice"), Some(dir.join("lattice")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_mining_log_is_read_from_its_end_and_may_be_absent() {
        let dir = std::env::temp_dir().join(format!("lattice-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mine.log");
        assert_eq!(tail(&file, 8), "");
        fs::write(&file, "0123456789").unwrap();
        assert_eq!(tail(&file, 4), "6789");
        assert_eq!(tail(&file, 64), "0123456789");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_lifecycle_verbs_run() {
        let root = std::env::temp_dir().join("lattice-desktop-test-verbs");
        for verb in ["up", "mine", "mine-run", "mine run", "child", "tx", "wipe", "key", "--help", "init; rm -rf /"] {
            assert!(run_lattice(&root, verb).unwrap_err().starts_with("refused"));
        }
        assert_eq!(verb_arguments("mine-start"), Some(&["mine", "start"][..]));
        assert_eq!(verb_arguments("mine-stop"), Some(&["mine", "stop"][..]));
    }

    #[test]
    fn node_paths_cannot_leave_loopback() {
        assert!(checked_path("/api/state/account/x?chainPath=Nexus").is_ok());
        for bad in ["//evil.example/x", "http://evil/x", "x", "/a b", "/a\nb", "/a#b", "/a\\b", ""] {
            assert!(checked_path(bad).is_err(), "{bad}");
        }
        assert!(node_request(8080, "DELETE", "/health", None, None).unwrap_err().starts_with("refused"));
    }

    #[test]
    fn the_page_sets_only_ports_peers_and_origins_through_the_cli() {
        let set = |v: Value| config_assignments(v.as_object().unwrap());
        let peer = "ab".repeat(32) + "@127.0.0.1:4001";
        assert_eq!(
            set(serde_json::json!({ "rpc": 18080, "listen": 14001, "peers.Nexus": [peer], "peers.Nexus/testnet": [] })).unwrap(),
            vec!["listen=14001".to_string(), format!("peers.Nexus=[\"{peer}\"]"), "peers.Nexus/testnet=[]".into(), "rpc=18080".into()],
        );
        for bad in [
            serde_json::json!({ "publicRead": 80 }),
            serde_json::json!({ "publicSubmit": true }),
            serde_json::json!({ "hostedChains": ["Nexus/a"] }),
            serde_json::json!({ "mine": {} }),
            serde_json::json!({ "rpc": 0 }),
            serde_json::json!({ "rpc": 70000 }),
            serde_json::json!({ "rpc": "18080" }),
            serde_json::json!({ "peers": [] }),
            serde_json::json!({ "peers.Other": [] }),
            serde_json::json!({ "peers.Nexus": "x" }),
            serde_json::json!({ "peers.Nexus/a b": [] }),
        ] {
            assert!(set(bad.clone()).is_err(), "{bad}");
        }
        let origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
        assert_eq!(set(serde_json::json!({ "rpcAllowedOrigins": [origin] })).unwrap(), vec![format!("rpcAllowedOrigins=[\"{origin}\"]")]);
        for bad in ["*", "null", "https://a.example", "chrome-extension://x/y", "https://a.example/", "http://a b", "x://", "://x", "https://u@h", "a"] {
            assert!(set(serde_json::json!({ "rpcAllowedOrigins": [bad] })).is_err(), "{bad}");
        }
        assert!(config_removals(&["peers.Nexus".into(), "peers.Nexus/a".into()]).is_ok());
        for bad in ["rpc", "peers", "hostedChains", "operatorTokens", "peers.Other"] {
            assert!(config_removals(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_rpc_port_is_the_merged_configurations() {
        let root = std::env::temp_dir().join(format!("lattice-desktop-rpc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // No lattice.json yet: no configuration, and no port.
        assert_eq!(merged_config(&root).unwrap(), None);
        assert_eq!(configured_rpc(&root), None);
        // The port the CLI's merged configuration names, not lattice.json's:
        // a fragment may set another.
        fs::write(root.join("lattice.json"), r#"{"rpc": 8080, "listen": 4001}"#).unwrap();
        assert_eq!(configured_rpc(&root), Some(8080));
        fs::write(root.join("merged.json"), r#"{"rpc": 18080, "listen": 14001}"#).unwrap();
        assert_eq!(configured_rpc(&root), Some(18080));
        assert_eq!(merged_config(&root).unwrap().unwrap()["listen"], 14001);
        fs::write(root.join("merged.json"), r#"{"rpc": 0}"#).unwrap();
        assert_eq!(configured_rpc(&root), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_running_nodes_port_is_the_one_it_wrote_down() {
        let root = std::env::temp_dir().join(format!("lattice-desktop-live-rpc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("chains").join("Nexus")).unwrap();
        // A new port saved, not yet in use: the configuration's is the next start's.
        fs::write(root.join("lattice.json"), r#"{"rpc": 9090}"#).unwrap();
        assert_eq!(live_rpc(&root), Some(9090));
        let address = root.join("chains").join("Nexus").join(".rpc-address");
        fs::write(&address, "http://127.0.0.1:8080\n").unwrap();
        assert_eq!(live_rpc(&root), Some(8080));
        fs::write(&address, "http://[::1]:8081\n").unwrap();
        assert_eq!(live_rpc(&root), Some(8081));
        assert_eq!(configured_rpc(&root), Some(9090));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_does_not_answer_is_given_up_on() {
        let began = std::time::Instant::now();
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 10"]);
        let said = output_within(slow, Duration::from_millis(300));
        assert!(said.unwrap_err().contains("no answer"));
        assert!(began.elapsed() < Duration::from_secs(5));
        // An answer longer than a pipe holds is read whole.
        let mut long = Command::new("sh");
        long.args(["-c", "head -c 200000 /dev/zero | tr '\\0' a; echo oops >&2; exit 3"]);
        let said = output_within(long, Duration::from_secs(10)).unwrap();
        assert_eq!((said.code, said.stdout.len(), said.stderr.trim()), (3, 200000, "oops"));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writes_take_their_mode_even_over_a_stale_temp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lattice-desktop-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vault.json");
        fs::write(path.with_extension("tmp"), b"stale").unwrap();
        fs::set_permissions(path.with_extension("tmp"), fs::Permissions::from_mode(0o666)).unwrap();
        write_atomically(&path, b"{}", 0o600).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stale_cookie_with_no_node_is_not_pinned() {
        let root = std::env::temp_dir().join(format!("lattice-desktop-stale-{}", std::process::id()));
        fs::create_dir_all(root.join("chains/Nexus")).unwrap();
        fs::write(cookie_path(&root), "__cookie__:left-by-a-crash").unwrap();
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(!node_accepts_cookie(free, &root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_cookie_is_read_fresh_as_basic_auth() {
        let root = std::env::temp_dir().join(format!("lattice-desktop-cookie-{}", std::process::id()));
        assert_eq!(cookie_authorization(&root), None);
        fs::create_dir_all(root.join("chains/Nexus")).unwrap();
        fs::write(cookie_path(&root), "__cookie__:abc\n").unwrap();
        assert_eq!(cookie_authorization(&root).as_deref(), Some("Basic X19jb29raWVfXzphYmM="));
        fs::write(cookie_path(&root), "__cookie__:def").unwrap();
        assert_eq!(read_cookie(&root).as_deref(), Some("__cookie__:def"), "rotated: re-read");
        fs::remove_dir_all(root).unwrap();
    }

}
