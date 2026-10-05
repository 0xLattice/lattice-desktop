//! The Lattice Desktop shell. Everything privileged lives here, behind a few
//! narrow commands the page invokes:
//!
//! - `lattice`: the bundled `lattice` CLI's lifecycle verbs (init/up/down/
//!   status) against the app's host root. `lattice up` spawns the bundled
//!   `lattice-node`; the app never runs the node any other way.
//! - `read_topology` / `write_topology`: the host's `lattice.json`.
//! - `store_get` / `store_set` / `store_remove`: the wallet's vault and
//!   settings, as 0600 files.
//! - `node_http`: HTTP to 127.0.0.1 on the RPC port in `lattice.json`, and
//!   nowhere else (the page's CSP gives it no network of its own).
//!
//! On quit the shell stops the node with `lattice down`.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tauri::{AppHandle, Manager, RunEvent};

const VERBS: [&str; 4] = ["init", "up", "down", "status"];
const STORE_KEYS: [&str; 2] = ["vault", "settings"];
const MAX_NODE_BODY: u64 = 8 * 1024 * 1024;

type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// ~/Library/Application Support/Lattice
fn app_root(app: &AppHandle) -> Result<PathBuf> {
    Ok(app.path().data_dir().map_err(err)?.join("Lattice"))
}

fn host_root(app: &AppHandle) -> Result<PathBuf> {
    Ok(app_root(app)?.join("host"))
}

/// A bundled binary: beside the app's executable (Contents/MacOS in the .app,
/// target/<profile> under `tauri dev`).
fn bundled(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(err)?;
    Ok(exe.parent().ok_or("no executable directory")?.join(name))
}

#[derive(Serialize, Debug)]
struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_lattice(root: &Path, verb: &str) -> Result<CommandResult> {
    if !VERBS.contains(&verb) {
        return Err(format!("refused: lattice {verb}"));
    }
    fs::create_dir_all(root).map_err(err)?;
    let output = Command::new(bundled("lattice")?)
        .arg(verb)
        .arg("--root")
        .arg(root)
        .output()
        .map_err(|e| format!("could not run the bundled lattice: {e}"))?;
    Ok(CommandResult {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[tauri::command]
async fn lattice(app: AppHandle, verb: String) -> Result<CommandResult> {
    let root = host_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || run_lattice(&root, &verb))
        .await
        .map_err(err)?
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
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    // A fresh file every time, so `mode` always applies (never a stale temp's).
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    fs::rename(&tmp, path).map_err(err)
}

/// The lattice.json fields the page may set. Everything else (public
/// listeners, external address, mining, ...) is the operator's, edited by
/// hand: the page cannot expose the node or point `node_http` elsewhere
/// beyond choosing the node's own loopback RPC port.
const MANAGED: [&str; 4] = ["rpc", "listen", "peers", "hostedChains"];

/// `current` with the page's managed fields applied (an absent `peers` or
/// `hostedChains` removes it); anything else from the page is refused.
fn merged_topology(current: Value, update: &Value) -> Result<Value> {
    let update = update.as_object().ok_or("lattice.json must be an object")?;
    if let Some(key) = update.keys().find(|k| !MANAGED.contains(&k.as_str())) {
        return Err(format!("refused: lattice.json field {key}"));
    }
    for port in ["rpc", "listen"] {
        rpc_port(&Some(serde_json::json!({ "rpc": update.get(port).cloned().unwrap_or(Value::Null) })))
            .map_err(|_| format!("{port} must be a port"))?;
    }
    for list in ["peers", "hostedChains"] {
        if let Some(v) = update.get(list) {
            if !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                return Err(format!("{list} must be a list of strings"));
            }
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

fn store_path(app: &AppHandle, key: &str) -> Result<PathBuf> {
    if !STORE_KEYS.contains(&key) {
        return Err(format!("refused: store key {key}"));
    }
    let dir = app_root(app)?.join("wallet");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(&dir).map_err(err)?;
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

fn node_request(port: u16, method: &str, path: &str, body: Option<String>) -> Result<NodeAnswer> {
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
    method: String,
    path: String,
    body: Option<String>,
) -> Result<NodeAnswer> {
    let port = rpc_port(&read_topology_at(&topology_path(&app)?)?)?;
    tauri::async_runtime::spawn_blocking(move || node_request(port, &method, &path, body))
        .await
        .map_err(err)?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            lattice,
            read_topology,
            write_topology,
            store_get,
            store_set,
            store_remove,
            node_http
        ])
        .build(tauri::generate_context!())
        .expect("error while building the app")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // A clean stop for the node this app runs (SIGTERM, then the
                // CLI's own grace); a no-op when it is not running.
                if let Ok(root) = host_root(app) {
                    let _ = run_lattice(&root, "down");
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lifecycle_verbs_run() {
        let root = std::env::temp_dir().join("lattice-desktop-test-verbs");
        for verb in ["mine", "tx", "wipe", "key", "--help", "init; rm -rf /"] {
            assert!(run_lattice(&root, verb).unwrap_err().starts_with("refused"));
        }
    }

    #[test]
    fn node_paths_cannot_leave_loopback() {
        assert!(checked_path("/api/state/account/x?chainPath=Nexus").is_ok());
        for bad in ["//evil.example/x", "http://evil/x", "x", "/a b", "/a\nb", "/a#b", "/a\\b", ""] {
            assert!(checked_path(bad).is_err(), "{bad}");
        }
        assert!(node_request(8080, "DELETE", "/health", None).unwrap_err().starts_with("refused"));
    }

    #[test]
    fn the_page_sets_only_managed_topology_fields() {
        let current = serde_json::json!({"listen": 4001, "rpc": 8080, "peers": ["a"], "minRelayFee": 5});
        let merged = merged_topology(current.clone(), &serde_json::json!({"rpc": 18080, "listen": 14001})).unwrap();
        assert_eq!(merged, serde_json::json!({"listen": 14001, "rpc": 18080, "minRelayFee": 5}));
        for bad in [
            serde_json::json!({"rpc": 18080, "listen": 14001, "publicRead": 80}),
            serde_json::json!({"rpc": 18080, "listen": 14001, "publicSubmit": true}),
            serde_json::json!({"rpc": 18080, "listen": 14001, "mine": {}}),
            serde_json::json!({"rpc": 0, "listen": 14001}),
            serde_json::json!({"listen": 14001}),
            serde_json::json!({"rpc": 1, "listen": 2, "peers": "x"}),
            serde_json::json!([]),
        ] {
            assert!(merged_topology(current.clone(), &bad).is_err(), "{bad}");
        }
    }

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
    fn rpc_port_comes_from_lattice_json() {
        assert_eq!(rpc_port(&Some(serde_json::json!({"rpc": 18080}))), Ok(18080));
        for bad in [None, Some(serde_json::json!({})), Some(serde_json::json!({"rpc": 0})), Some(serde_json::json!({"rpc": 70000}))] {
            assert!(rpc_port(&bad).is_err());
        }
    }
}
