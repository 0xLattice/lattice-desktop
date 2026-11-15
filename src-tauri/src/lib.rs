//! The Lattice Desktop shell. Everything privileged lives here, behind a few
//! narrow commands the page invokes:
//!
//! - `lattice`: the bundled `lattice` CLI's verbs (init/status, and the
//!   mining loop's) against the app's host root.
//! - `node_start` / `node_stop` / `node_process`: the node itself, which the
//!   app runs as its own child (`lattice up`, which becomes the bundled
//!   `lattice-node`) and starts again as a process manager would (see
//!   `node_process`); the app never runs the node any other way.
//! - `read_topology` / `write_topology`: the host's `lattice.json`.
//! - `operator_chains` / `host_chains`: the chains the app hosts, in the
//!   `operator` configuration fragment (`config.d/operator.json`).
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

/// The RPC port the node was started on: read from lattice.json at launch
/// (a node left running by an earlier session) and each time the app starts
/// the node. The page can rewrite lattice.json, but `node_http` and the
/// cookie go only to this port.
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

/// Writes miner.json, and drops the `mine` object an older lattice.json
/// still holds: the miner reads miner.json from now on, and the node ignores it.
fn write_miner_settings(root: &Path, settings: &Value) -> Result<()> {
    write_atomically(&root.join("miner.json"), &serde_json::to_vec_pretty(settings).map_err(err)?, 0o644)?;
    let topology = root.join("lattice.json");
    if let Some(mut current) = read_topology_at(&topology)? {
        if current.as_object_mut().and_then(|fields| fields.remove("mine")).is_some() {
            write_atomically(&topology, &serde_json::to_vec_pretty(&current).map_err(err)?, 0o644)?;
        }
    }
    Ok(())
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

fn topology_path(app: &AppHandle) -> Result<PathBuf> {
    Ok(host_root(app)?.join("lattice.json"))
}

fn read_topology_at(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(err),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(e)),
    }
}

#[tauri::command]
fn read_topology(app: AppHandle) -> Result<Option<Value>> {
    read_topology_at(&topology_path(&app)?)
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

/// The lattice.json fields the page may set. Everything else (public
/// listeners, external address, ...) is the operator's, edited by hand: the
/// page cannot expose the node or point `node_http` elsewhere beyond choosing
/// the node's own loopback RPC port. Hosted chains are in the `operator`
/// fragment (`host_chains`), and mining's settings in miner.json, of which
/// the page may set only who is paid, how many workers search, and which
/// bundled worker does (see `merged_mine`).
const MANAGED: [&str; 4] = ["rpc", "listen", "peers", "rpcAllowedOrigins"];

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
/// searches. `mine.worker` is a program the CLI runs, so the page may name
/// only `cpu` (the CLI's own bundled worker) or `gpu_worker`, the GPU worker
/// bundled in this app; left out, the worker stays as the operator set it.
/// Every tuning field is kept as it is.
fn merged_mine(current: Option<&Value>, update: &Value, gpu_worker: Option<&str>) -> Result<Value> {
    let update = update.as_object().ok_or("mine must be an object")?;
    if let Some(key) = update.keys().find(|k| !["workers", "recipients", "worker"].contains(&k.as_str())) {
        return Err(format!("refused: lattice.json field mine.{key}"));
    }
    let worker = match update.get("worker") {
        None => None,
        Some(value) => match value.as_str() {
            Some("cpu") => Some("cpu"),
            Some(path) if gpu_worker == Some(path) => Some(path),
            _ => return Err("refused: mine.worker may be only a worker bundled with this app".into()),
        },
    };
    let workers = update.get("workers").and_then(Value::as_u64).filter(|n| (1..=MAX_WORKERS).contains(n))
        .ok_or(format!("mine.workers must be a whole number from 1 to {MAX_WORKERS}"))?;
    let recipients = update.get("recipients").and_then(Value::as_object).ok_or("mine.recipients must map chain paths to addresses")?;
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

/// `current` with the page's managed fields applied (an absent `peers` or
/// `rpcAllowedOrigins` removes it); anything else from the page is refused.
fn merged_topology(current: Value, update: &Value) -> Result<Value> {
    let update = update.as_object().ok_or("lattice.json must be an object")?;
    if let Some(key) = update.keys().find(|k| !MANAGED.contains(&k.as_str())) {
        return Err(format!("refused: lattice.json field {key}"));
    }
    for port in ["rpc", "listen"] {
        rpc_port(&Some(serde_json::json!({ "rpc": update.get(port).cloned().unwrap_or(Value::Null) })))
            .map_err(|_| format!("{port} must be a port"))?;
    }
    let strings = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(Value::is_string));
    if update.get("rpcAllowedOrigins").is_some_and(|v| !strings(v)) {
        return Err("rpcAllowedOrigins must be a list of strings".into());
    }
    // Nexus's list, or a list per chain path.
    if let Some(v) = update.get("peers") {
        let per_chain = v.as_object().is_some_and(|m| m.iter().all(|(chain, list)| is_chain_path(chain) && strings(list)));
        if !strings(v) && !per_chain {
            return Err("peers must be a list of strings, or one per chain path".to_string());
        }
    }
    if let Some(origins) = update.get("rpcAllowedOrigins").and_then(Value::as_array) {
        if let Some(bad) = origins.iter().filter_map(Value::as_str).find(|o| !is_origin(o)) {
            return Err(format!("not an origin: {bad}"));
        }
    }
    let mut out = current.as_object().cloned().ok_or("lattice.json is not an object")?;
    for key in MANAGED {
        match update.get(key) {
            Some(v) => {
                out.insert(key.to_string(), v.clone());
            }
            None => {
                out.remove(key);
            }
        }
    }
    Ok(Value::Object(out))
}

#[tauri::command]
fn write_topology(app: AppHandle, topology: Value) -> Result<()> {
    let path = topology_path(&app)?;
    let current = read_topology_at(&path)?.ok_or("no lattice.json; run lattice init first")?;
    let merged = merged_topology(current, &topology)?;
    let bytes = serde_json::to_vec_pretty(&merged).map_err(err)?;
    write_atomically(&path, &bytes, 0o644)
}

/// The `operator` fragment, the configuration layer that holds the chains
/// the app hosts (and that `lattice child host` writes too).
fn operator_fragment(root: &Path) -> PathBuf {
    root.join("config.d").join("operator.json")
}

/// `fragment` with `hostedChains` set to `chains` (none: left out), its other keys kept.
fn with_hosted(fragment: Option<Value>, chains: &[String]) -> Result<Value> {
    let mut fields = match fragment {
        None => serde_json::Map::new(),
        Some(Value::Object(fields)) => fields,
        Some(_) => return Err("config.d/operator.json is not a JSON object".into()),
    };
    fields.remove("hostedChains");
    if !chains.is_empty() {
        fields.insert("hostedChains".into(), Value::from(chains.to_vec()));
    }
    Ok(Value::Object(fields))
}

/// The chains the operator fragment lists, from its file.
#[tauri::command]
fn operator_chains(app: AppHandle) -> Result<Vec<String>> {
    let fragment = read_topology_at(&operator_fragment(&host_root(&app)?))?;
    let listed = fragment.as_ref().and_then(|f| f.get("hostedChains")).and_then(Value::as_array);
    Ok(listed.map(|chains| chains.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default())
}

/// Sets the chains the operator fragment lists. A running node is told
/// through its API, and hosts them (and their ancestors) at once; a stopped
/// one's file is written, for its next start. A list lattice.json still holds
/// from before is moved into the fragment, so the app's chains are in one place.
#[tauri::command]
async fn host_chains(app: AppHandle, pinned: State<'_, PinnedPort>, process: State<'_, node_process::NodeProcess>, chains: Vec<String>) -> Result<()> {
    if let Some(bad) = chains.iter().find(|c| !is_chain_path(c) || c.as_str() == "Nexus") {
        return Err(format!("not a child chain path: {bad}"));
    }
    let root = host_root(&app)?;
    let port = *pinned.0.lock().map_err(err)?;
    let node = port.filter(|_| process.report().is_ok_and(|p| p.running)).map(|port| (port, cookie_authorization(&root)));
    tauri::async_runtime::spawn_blocking(move || set_hosted(&root, node, &chains)).await.map_err(err)?
}

fn set_hosted(root: &Path, node: Option<(u16, Option<String>)>, chains: &[String]) -> Result<()> {
    let topology = root.join("lattice.json");
    let before = read_topology_at(&topology)?;
    // Out of lattice.json first, so a running node, re-reading its layers
    // once the fragment changes, drops a chain only lattice.json listed.
    let mut without = before.clone();
    let moved = without.as_mut().and_then(Value::as_object_mut).and_then(|fields| fields.remove("hostedChains")).is_some();
    if moved {
        write_atomically(&topology, &serde_json::to_vec_pretty(&without).map_err(err)?, 0o644)?;
    }
    let written = match node {
        Some((port, authorization)) => put_hosted(port, authorization.as_deref(), chains),
        None => {
            let path = operator_fragment(root);
            let fragment = with_hosted(read_topology_at(&path)?, chains)?;
            fs::create_dir_all(path.parent().ok_or("no config.d")?).map_err(err)?;
            write_atomically(&path, &serde_json::to_vec_pretty(&fragment).map_err(err)?, 0o644)
        }
    };
    if written.is_err() && moved {
        if let Some(before) = before {
            write_atomically(&topology, &serde_json::to_vec_pretty(&before).map_err(err)?, 0o644)?;
        }
    }
    written
}

/// The operator fragment's `hostedChains`, through the node: read, changed,
/// and written only while it is still the version read, read again otherwise.
fn put_hosted(port: u16, authorization: Option<&str>, chains: &[String]) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/config/fragments/operator");
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).redirects(0).build();
    let with_auth = |request: ureq::Request| match authorization {
        Some(value) => request.set("Authorization", value),
        None => request,
    };
    let answer = |result: std::result::Result<ureq::Response, ureq::Error>| match result {
        Ok(r) | Err(ureq::Error::Status(_, r)) => Ok(r),
        Err(e) => Err(format!("the node is not answering (it may still be starting): {e}")),
    };
    for _ in 0..5 {
        let current = answer(with_auth(agent.get(&url)).call())?;
        let (fragment, precondition) = match current.status() {
            200 => {
                let etag = current.header("ETag").ok_or("the node gave the operator fragment no version")?.to_string();
                (Some(serde_json::from_str::<Value>(&current.into_string().map_err(err)?).map_err(err)?), ("If-Match", etag))
            }
            404 => (None, ("If-None-Match", "*".to_string())),
            status => return Err(format!("the node did not give the operator fragment ({status}): {}", current.into_string().unwrap_or_default())),
        };
        let body = serde_json::to_string(&with_hosted(fragment, chains)?).map_err(err)?;
        let put = with_auth(agent.put(&url)).set("Content-Type", "application/json").set(precondition.0, &precondition.1);
        let written = answer(put.send_string(&body))?;
        match written.status() {
            200 => return Ok(()),
            // Changed by someone else since it was read.
            412 => continue,
            status => return Err(format!("the node refused the hosted chains ({status}): {}", written.into_string().unwrap_or_default())),
        }
    }
    Err("the operator fragment kept changing; try again".into())
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

fn rpc_port(topology: &Option<Value>) -> Result<u16> {
    topology
        .as_ref()
        .and_then(|t| t.get("rpc"))
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| "no rpc port in lattice.json".to_string())
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
            // Only a running node (it keeps a cookie; it removes it at exit)
            // is pinned at launch; otherwise the first `up` pins.
            // A crash leaves a stale cookie, so the port is pinned only when
            // the node there accepts that cookie on an operator route.
            let root = host_root(handle).ok();
            let port = topology_path(handle)
                .and_then(|p| read_topology_at(&p))
                .ok()
                .and_then(|t| rpc_port(&t).ok())
                .filter(|p| root.as_deref().is_some_and(|r| node_accepts_cookie(*p, r)));
            app.manage(PinnedPort(Mutex::new(port)));
            app.manage(node_process::NodeProcess::default());
            app.manage(swap::Swaps::default());
            // Before any swap reports.
            background::start(handle)?;
            // Swaps that must carry on, resumed on their engine threads once
            // the node's port is pinned (here, or at the first `up`).
            sync_swaps(handle);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            lattice,
            node_start,
            node_stop,
            node_process,
            read_topology,
            write_topology,
            operator_chains,
            host_chains,
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
        assert_eq!(read_topology_at(&topology).unwrap(), Some(serde_json::json!({ "rpc": 1 })), "lattice.json no longer holds mining");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stopped_nodes_hosted_chains_move_into_the_operator_fragment() {
        let dir = std::env::temp_dir().join(format!("lattice-hosted-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let topology = dir.join("lattice.json");
        fs::write(&topology, serde_json::to_vec(&serde_json::json!({ "rpc": 1, "hostedChains": ["Nexus/a"] })).unwrap()).unwrap();
        set_hosted(&dir, None, &["Nexus/a".to_string(), "Nexus/a/b".to_string()]).unwrap();
        assert_eq!(read_topology_at(&topology).unwrap(), Some(serde_json::json!({ "rpc": 1 })));
        assert_eq!(read_topology_at(&operator_fragment(&dir)).unwrap(), Some(serde_json::json!({ "hostedChains": ["Nexus/a", "Nexus/a/b"] })));
        // The fragment's other keys are its writer's; no chains leaves the key out.
        fs::write(operator_fragment(&dir), serde_json::to_vec(&serde_json::json!({ "relayFee": 2, "hostedChains": ["Nexus/a"] })).unwrap()).unwrap();
        set_hosted(&dir, None, &[]).unwrap();
        assert_eq!(read_topology_at(&operator_fragment(&dir)).unwrap(), Some(serde_json::json!({ "relayFee": 2 })));
        assert!(with_hosted(Some(serde_json::json!([])), &[]).is_err());
        // Through a node that does not answer: nothing is moved.
        fs::write(&topology, serde_json::to_vec(&serde_json::json!({ "rpc": 1, "hostedChains": ["Nexus/a"] })).unwrap()).unwrap();
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(set_hosted(&dir, Some((free, None)), &[]).is_err());
        assert_eq!(read_topology_at(&topology).unwrap(), Some(serde_json::json!({ "rpc": 1, "hostedChains": ["Nexus/a"] })));
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
    fn the_page_sets_only_managed_topology_fields() {
        let current = serde_json::json!({"listen": 4001, "rpc": 8080, "peers": ["a"], "minRelayFee": 5});
        let merged = merged_topology(current.clone(), &serde_json::json!({"rpc": 18080, "listen": 14001})).unwrap();
        assert_eq!(merged, serde_json::json!({"listen": 14001, "rpc": 18080, "minRelayFee": 5}));
        let per_chain = serde_json::json!({"rpc": 18080, "listen": 14001, "peers": {"Nexus": [], "Nexus/testnet": ["a"]}});
        assert_eq!(
            merged_topology(current.clone(), &per_chain).unwrap()["peers"],
            serde_json::json!({"Nexus": [], "Nexus/testnet": ["a"]})
        );
        for bad in [
            serde_json::json!({"rpc": 18080, "listen": 14001, "publicRead": 80}),
            serde_json::json!({"rpc": 18080, "listen": 14001, "publicSubmit": true}),
            serde_json::json!({"rpc": 18080, "listen": 14001, "mine": {}}),
            serde_json::json!({"rpc": 18080, "listen": 14001, "hostedChains": ["Nexus/a"]}),
            serde_json::json!({"rpc": 0, "listen": 14001}),
            serde_json::json!({"listen": 14001}),
            serde_json::json!({"rpc": 1, "listen": 2, "peers": "x"}),
            serde_json::json!({"rpc": 1, "listen": 2, "peers": {"Nexus/testnet": "x"}}),
            serde_json::json!({"rpc": 1, "listen": 2, "peers": {"not a path": ["a"]}}),
            serde_json::json!([]),
        ] {
            assert!(merged_topology(current.clone(), &bad).is_err(), "{bad}");
        }
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
    fn allowed_origins_are_bare_origins() {
        let current = serde_json::json!({"listen": 4001, "rpc": 8080});
        let ok = serde_json::json!({"rpc": 8080, "listen": 4001, "rpcAllowedOrigins": ["chrome-extension://abcdefghijklmnopabcdefghijklmnop"]});
        assert_eq!(merged_topology(current.clone(), &ok).unwrap()["rpcAllowedOrigins"], ok["rpcAllowedOrigins"]);
        for bad in ["*", "null", "https://a.example", "chrome-extension://x/y", "https://a.example/", "http://a b", "x://", "://x", "https://u@h", "a"] {
            let update = serde_json::json!({"rpc": 8080, "listen": 4001, "rpcAllowedOrigins": [bad]});
            assert!(merged_topology(current.clone(), &update).is_err(), "{bad}");
        }
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

    #[test]
    fn rpc_port_comes_from_lattice_json() {
        assert_eq!(rpc_port(&Some(serde_json::json!({"rpc": 18080}))), Ok(18080));
        for bad in [None, Some(serde_json::json!({})), Some(serde_json::json!({"rpc": 0})), Some(serde_json::json!({"rpc": 70000}))] {
            assert!(rpc_port(&bad).is_err());
        }
    }
}
