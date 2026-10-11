#![cfg_attr(not(windows), allow(dead_code))]
//! Windows: the node runs in the Windows Subsystem for Linux.
//!
//! The node is built for macOS and Linux only, so on Windows the app carries
//! the Linux binaries and runs them, unmodified, in the PC's default WSL
//! distribution:
//!
//! - The binaries are copied once per node version into
//!   `~/.local/share/Lattice/bin/<commit>` inside the distribution.
//! - The node's host root is `~/.local/share/Lattice/host` there, on the
//!   distribution's own filesystem. This process reads and writes it through
//!   `\\wsl$\<distribution>\...`; the CLI is told the Linux path.
//! - The node's loopback RPC port reaches Windows through WSL's localhost
//!   forwarding, so `node_http` connects to 127.0.0.1 as on every platform.
//! - WSL stops a distribution once nothing started from Windows is running
//!   in it, taking background programs with it. The app holds one idle
//!   process there for as long as it is open, so its node keeps running.

use std::path::{Path, PathBuf};

/// The default distribution and its default user's home directory.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub distro: String,
    pub home: String,
}

pub const NEEDS_WSL: &str = "Lattice runs its node in the Windows Subsystem for Linux, which is not set up on this PC. Open PowerShell as administrator, run \"wsl --install\", restart, open Ubuntu once to finish setting it up, then open Lattice again.";

/// The app's directory inside the distribution, as Linux names it.
pub fn data_dir(session: &Session) -> String {
    format!("{}/.local/share/Lattice", session.home)
}

/// A Linux path inside `distro` as Windows reaches it.
pub fn windows_path(distro: &str, linux: &str) -> PathBuf {
    PathBuf::from(format!(r"\\wsl$\{distro}{}", linux.replace('/', r"\")))
}

/// The Linux path of a `windows_path` in `distro`; None for any other path.
pub fn linux_path(distro: &str, windows: &Path) -> Option<String> {
    let text = windows.to_str()?;
    let rest = text.strip_prefix(&format!(r"\\wsl$\{distro}"))?;
    (rest.starts_with('\\')).then(|| rest.replace('\\', "/"))
}

/// A session from what `printenv` printed, if both are plain values: a name
/// with no separators, and an absolute path with no spaces.
pub fn session_from(distro: &str, home: &str) -> Option<Session> {
    let (distro, home) = (distro.trim(), home.trim());
    let plain = |s: &str| !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\' || c == '"');
    (plain(distro) && !distro.contains('/') && plain(home) && home.starts_with('/'))
        .then(|| Session { distro: distro.to_string(), home: home.to_string() })
}

/// Output as text: a Linux program's is UTF-8, wsl.exe's own messages are UTF-16.
pub fn decode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).all(|b| *b == 0) {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(windows)]
mod running {
    use super::*;
    use std::fs;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    /// The binaries the app carries for the node, as resources under `linux/`.
    const BINARIES: [&str; 4] = ["lattice", "lattice-node", "lattice-mining-coordinator", "lattice-miner"];
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    static SESSION: Mutex<Option<Session>> = Mutex::new(None);
    static KEEP_ALIVE: Mutex<Option<Child>> = Mutex::new(None);

    /// `wsl.exe`, without a console window flashing up for each call.
    pub fn command() -> Command {
        let mut command = Command::new("wsl.exe");
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }

    fn printenv(name: &str) -> Option<String> {
        let output = command().args(["-e", "printenv", name]).output().ok()?;
        output.status.success().then(|| decode(&output.stdout))
    }

    /// The PC's default distribution, found once; a PC without one is asked
    /// again on the next call, so setting WSL up does not need the app reopened.
    pub fn session() -> Result<Session, String> {
        let mut held = SESSION.lock().map_err(|e| e.to_string())?;
        if let Some(session) = held.as_ref() {
            return Ok(session.clone());
        }
        let session = printenv("WSL_DISTRO_NAME")
            .zip(printenv("HOME"))
            .and_then(|(distro, home)| session_from(&distro, &home))
            .ok_or(NEEDS_WSL)?;
        keep_alive(&session);
        *held = Some(session.clone());
        Ok(session)
    }

    /// One idle process in the distribution, held until the app exits.
    fn keep_alive(session: &Session) {
        let Ok(mut held) = KEEP_ALIVE.lock() else { return };
        if held.is_none() {
            *held = command()
                .args(["-d", &session.distro, "-e", "sleep", "infinity"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
    }

    /// Let the distribution stop once nothing else is running in it.
    pub fn release() {
        if let Ok(mut held) = KEEP_ALIVE.lock() {
            if let Some(mut child) = held.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// The node binaries inside the distribution, copied there from the
    /// app's resources unless this version already was. Returns the Linux
    /// directory they are in.
    pub fn install(session: &Session, resources: &Path) -> Result<String, String> {
        let source = resources.join("linux");
        let commit = fs::read_to_string(source.join("COMMIT")).map_err(|e| format!("this build carries no node binaries: {e}"))?;
        let commit = commit.trim();
        if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("this build's node binaries name no commit".into());
        }
        let linux = format!("{}/bin/{commit}", data_dir(session));
        let target = windows_path(&session.distro, &linux);
        // The marker is written last: a copy cut short is done again.
        if target.join("COMMIT").is_file() {
            return Ok(linux);
        }
        fs::create_dir_all(&target).map_err(|e| format!("could not write into WSL: {e}"))?;
        for name in BINARIES {
            fs::copy(source.join(name), target.join(name)).map_err(|e| format!("could not copy {name} into WSL: {e}"))?;
        }
        let mut chmod = command();
        chmod.args(["-d", &session.distro, "-e", "chmod", "755"]);
        for name in BINARIES {
            chmod.arg(format!("{linux}/{name}"));
        }
        let output = chmod.output().map_err(|e| format!("could not run wsl.exe: {e}"))?;
        if !output.status.success() {
            return Err(format!("could not make the node binaries runnable: {}", decode(&output.stderr).trim()));
        }
        fs::write(target.join("COMMIT"), commit).map_err(|e| format!("could not write into WSL: {e}"))?;
        Ok(linux)
    }
}

#[cfg(windows)]
pub use running::{command, install, release, session};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_root_has_a_windows_path_and_a_linux_path() {
        let session = session_from("Ubuntu\n", "/home/ada\n").unwrap();
        let root = format!("{}/host", data_dir(&session));
        assert_eq!(root, "/home/ada/.local/share/Lattice/host");
        let windows = windows_path(&session.distro, &root);
        assert_eq!(windows.to_str().unwrap(), r"\\wsl$\Ubuntu\home\ada\.local\share\Lattice\host");
        assert_eq!(linux_path("Ubuntu", &windows).as_deref(), Some(root.as_str()));
        // Only a path inside that distribution has a Linux path.
        for other in [r"C:\Users\ada\host", r"\\wsl$\Debian\home\ada", r"\\wsl$\Ubuntu-22.04\home\ada", "/home/ada"] {
            assert_eq!(linux_path("Ubuntu", Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn a_session_needs_a_plain_name_and_an_absolute_home() {
        assert!(session_from("Ubuntu-22.04", "/root").is_some());
        for (distro, home) in [("", "/home/a"), ("Ubuntu", ""), ("Ubuntu", "home/a"), ("Ubuntu", "/home/a b"), ("a/b", "/home/a"), ("a\\b", "/home/a"), ("Ubuntu", "/home/\"a")] {
            assert_eq!(session_from(distro, home), None, "{distro} {home}");
        }
    }

    #[test]
    fn wsl_exes_own_messages_are_utf16() {
        let utf16: Vec<u8> = "no distributions".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode(&utf16), "no distributions");
        assert_eq!(decode(b"Ubuntu\n"), "Ubuntu\n");
        assert_eq!(decode(b""), "");
    }
}
