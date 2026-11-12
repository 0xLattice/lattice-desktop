//! Buying NEX with BTC: lattice-swap's native buyer engine (`swap::app`)
//! behind the commands the website's driver offers a page (`Wallet` and
//! `Buyer` in lattice-swap's web/driver), as eigenwallet runs its swap
//! engine in its Tauri backend.
//!
//! - Bitcoin is read through an Esplora, mempool.space's by default; NEX
//!   through the public read node [`PUBLIC_NEXUS`] by default, with no
//!   cookie, or this app's own node, on the pinned loopback RPC port with
//!   its cookie, which the engine reads again whenever the node restarts.
//!   On its own node, a new swap starts only once a port is pinned; the
//!   wallet opens with the node down too, on the port it will run on, so a
//!   taken swap's Bitcoin side goes on while its Nexus side waits for the
//!   node, and opens again on a new pinned port.
//! - The page chooses the Bitcoin network (mainnet by default, signet to
//!   test), the Esplora: mempool.space's by default, or the user's own,
//!   one per network, refused unless it is on that network, as Sparrow
//!   chooses between a public server and your own; and the Nexus node, per
//!   network: the public one, or your own at an https URL, each refused
//!   unless it is on the Nexus and takes transactions, as lattice-swap-web's
//!   Settings refuse one, or this app's. The network does not change while
//!   a swap runs; its Esplora and Nexus node may, and the swap goes on on them,
//!   which its resume checks again (a server that failed or misreports
//!   must not hold a refund). The relays whose offers are listed, and the
//!   relay directory more are found on, are the operator's, edited by hand
//!   in `<app_root>/swap/settings.json`. Every
//!   URL is https, or plain http to this machine or a local network (a
//!   private or link-local address, `localhost`, `.local`, `.lan`,
//!   `.home.arpa`) or an onion, as the engine's client parses it.
//! - The swap wallet is a single key of its own in
//!   `<app_root>/swap/<network>/` (0700), not derived from the vault, so a
//!   refund needs no unlock.
//! - One swap at a time: a swap is started only once the one before it, and
//!   every punished one asking for `s`, has stopped running.
//! - Each swap runs on an engine thread, and its status goes to the page as
//!   a `swap-status` event each poll; a failed round, which the thread runs
//!   again, as `swap-error`. Both name the swap: a punished one reports too.
//! - Whenever the wallet is opened (at launch, at `up`), every swap that has
//!   taken its offer and not ended is resumed, and every punished one, to ask
//!   the seller for `s` again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bitcoin::Network;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use swap::app::{default_esplora, Buy, Config, Wallet};
use swap::esplora::Esplora;
use swap::offer::Offer;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::{
    app_root, cookie_path, err, host_root, read_topology_at, rpc_port, write_atomically,
    write_new, PinnedPort, Result,
};

/// The relays whose offers are listed on a network the settings name no
/// relays for: this one.
pub const DEFAULT_RELAY: &str = "https://lattice-swap-relay.fly.dev";
/// The relay directory's chain on signet when the settings name none, as
/// lattice-swap-web's signet build reads it; mainnet has none yet.
pub const SIGNET_RELAY_DIRECTORY: &str = "Nexus/testnet/swap-relays";
/// The Nexus node read unless the settings choose this app's own: a public
/// read port that takes submits, as lattice-swap's CLI reads by default.
/// One for every Bitcoin network: there is one Nexus,
/// [`swap_nexus_tx::MAINNET_GENESIS`]'s, and signet swaps trade on it too.
pub const PUBLIC_NEXUS: &str = "https://lattice-mainnet-read.fly.dev";
/// The Bitcoin networks a swap may run on: mainnet, and signet to test.
const NETWORKS: [Network; 2] = [Network::Bitcoin, Network::Signet];
/// The most a recovery file may hold.
const MAX_BODY: u64 = 8 * 1024 * 1024;
/// The most entries of one relay's board that are read, and the most bytes
/// of the board of a relay of the settings' and of one found on the relay
/// directory, as lattice-swap-web reads them (`MAX_OFFERS`,
/// `MAX_OWN_RELAY_BYTES`, `MAX_FOUND_RELAY_BYTES`).
const MAX_OFFERS: usize = 100;
const MAX_OWN_BOARD: u64 = 48 << 20;
const MAX_FOUND_BOARD: u64 = 4 << 20;
/// How long a read of the relay directory may take, in all, as
/// lattice-swap-web's `DIRECTORY_TIMEOUT`.
const DIRECTORY_TIMEOUT: Duration = Duration::from_secs(15);

/// Where swaps run: the Bitcoin network, the user's own Esplora on each
/// network that has one, the networks whose swaps read Nexus on this app's
/// own node, and the relays and relay directory on each network that names
/// its own. Kept in
/// `<app_root>/swap/settings.json`; the page sets the network, the Esplora
/// and the Nexus node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Stored")]
pub struct Settings {
    pub network: Network,
    /// mempool.space's on a network missing here. Left out when empty, so
    /// a build from before one per network still reads the file.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub esplora: BTreeMap<Network, String>,
    /// [`PUBLIC_NEXUS`] on a network missing here. Left out when empty, as
    /// a build from before it ignores it.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub own_nexus: BTreeSet<Network>,
    /// Your own Nexus node on each network that reads one by its URL (see
    /// [`remote_nexus`]), never one in `own_nexus`. Left out when empty, as
    /// a build from before it ignores it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub nexus: BTreeMap<Network, String>,
    /// [`DEFAULT_RELAY`] on a network missing here.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub relays: BTreeMap<Network, Vec<String>>,
    /// [`SIGNET_RELAY_DIRECTORY`] on signet, none on mainnet, if missing
    /// here; none if its chain is empty.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub relay_directory: BTreeMap<Network, Directory>,
}

/// A relay directory: its chain, and a node hosting it to read it from
/// when this app's does not (empty for none).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Directory {
    #[serde(default)]
    pub node: String,
    pub chain: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            network: Network::Bitcoin,
            esplora: BTreeMap::new(),
            own_nexus: BTreeSet::new(),
            nexus: BTreeMap::new(),
            relays: BTreeMap::new(),
            relay_directory: BTreeMap::new(),
        }
    }
}

/// The settings as a file holds them: its Esplora one URL, as it was before
/// there was one per network, or one per network; its relay one URL, as it
/// was before there were several, for every network.
#[derive(Deserialize)]
struct Stored {
    network: Network,
    #[serde(default)]
    esplora: Option<Esploras>,
    #[serde(default, rename = "ownNexus")]
    own_nexus: BTreeSet<Network>,
    #[serde(default)]
    nexus: BTreeMap<String, String>,
    #[serde(default)]
    relay: Option<String>,
    #[serde(default)]
    relays: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "relayDirectory")]
    relay_directory: BTreeMap<String, Directory>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Esploras {
    One(String),
    Each(BTreeMap<String, String>),
}

impl TryFrom<Stored> for Settings {
    type Error = String;

    /// One URL is the network's the file was saved for; one per network
    /// names each network.
    fn try_from(stored: Stored) -> Result<Self> {
        let esplora = match stored.esplora {
            None => BTreeMap::new(),
            Some(Esploras::One(url)) => BTreeMap::from([(stored.network, url)]),
            Some(Esploras::Each(each)) => per_network(each, "an Esplora")?,
        };
        let mut relays = per_network(stored.relays, "relays")?;
        for network in NETWORKS {
            if let Some(url) = &stored.relay {
                relays.entry(network).or_insert_with(|| vec![url.clone()]);
            }
        }
        Ok(Self {
            network: stored.network,
            esplora,
            own_nexus: stored.own_nexus,
            nexus: per_network(stored.nexus, "a Nexus node")?,
            relays,
            relay_directory: per_network(stored.relay_directory, "a relay directory")?,
        })
    }
}

/// Values by the name of their network: refused for a name that is none.
fn per_network<T>(each: BTreeMap<String, T>, what: &str) -> Result<BTreeMap<Network, T>> {
    each.into_iter()
        .map(|(network, url)| match network.parse() {
            Ok(network) => Ok((network, url)),
            Err(_) => Err(format!(
                "{what} for {network:?}, which is no Bitcoin network"
            )),
        })
        .collect()
}

/// Whether plain http to `host` stays on this machine or a local network,
/// or goes to an onion: a loopback, private or link-local address,
/// `localhost`, or a name under `.local`, `.lan`, `.home.arpa` (RFC 8375)
/// or `.onion`. Not a name of one label: some top-level domains have an
/// address (`ws`), and a search domain can make one public.
fn local(host: &url::Host<&str>) -> bool {
    use std::net::Ipv4Addr;
    let v4 = |ip: Ipv4Addr| ip.is_loopback() || ip.is_private() || ip.is_link_local();
    match host {
        url::Host::Ipv4(ip) => v4(*ip),
        url::Host::Ipv6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => v4(mapped),
            None => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
        },
        url::Host::Domain(name) => {
            *name == "localhost"
                || [".local", ".lan", ".home.arpa", ".onion"]
                    .iter()
                    .any(|suffix| name.strip_suffix('.').unwrap_or(name).ends_with(suffix))
        }
    }
}

/// A URL the swap engine may read, as its client (ureq, on `url`) parses
/// it: https, or plain http only to this machine, a local network or an
/// onion (`local`); with no user name or password in front of its host.
fn check_url(text: &str) -> Result<()> {
    let refused = || {
        Err(format!(
            "refused: {text} is neither https nor http to this machine, a local network or an onion"
        ))
    };
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refused();
    }
    let Ok(url) = url::Url::parse(text) else {
        return refused();
    };
    if !url.username().is_empty() || url.password().is_some() {
        return refused();
    }
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => Ok(()),
        ("http", Some(host)) if local(&host) => Ok(()),
        _ => refused(),
    }
}

/// `text` as your own Nexus node is saved: an https URL, as lattice-swap-web's
/// Settings take one, that the engine may read (`check_url`), normalized
/// as the website compares servers: its scheme and host lowercased, no
/// trailing slash.
fn remote_nexus(text: &str) -> Result<String> {
    let text = text.trim();
    match url::Url::parse(text) {
        Ok(url) if url.scheme() == "https" => {
            check_url(text)?;
            Ok(url.as_str().trim_end_matches('/').to_string())
        }
        _ => Err("refused: must be an https URL".into()),
    }
}

impl Settings {
    fn check(&self) -> Result<()> {
        if !NETWORKS.contains(&self.network) {
            return Err(format!(
                "refused: swaps run on bitcoin or signet, not {}",
                self.network
            ));
        }
        let relays = self.relays.values().flatten();
        let directories = self.relay_directory.values().map(|d| &d.node).filter(|node| !node.is_empty());
        for url in self.esplora.values().chain(relays).chain(directories) {
            check_url(url)?;
        }
        for (network, url) in &self.nexus {
            // As saved: the engine reads the URL as it is stored.
            let normal = remote_nexus(url)?;
            if normal != *url {
                return Err(format!(
                    "refused: Nexus node {url:?} is not saved as {normal}"
                ));
            }
            if self.own_nexus.contains(network) {
                return Err(format!(
                    "refused: Nexus on {network} is read on this app's node and on {url}"
                ));
            }
        }
        Ok(())
    }

    /// The user's own Esplora on the network swaps run on, if any.
    fn own(&self) -> Option<&String> {
        self.esplora.get(&self.network)
    }

    fn esplora(&self) -> Result<String> {
        self.own()
            .cloned()
            .or_else(|| default_esplora(self.network).map(String::from))
            .ok_or_else(|| format!("no Esplora for {}", self.network))
    }

    /// The relays whose offers are listed on the network swaps run on.
    fn relays(&self) -> Vec<String> {
        self.relays
            .get(&self.network)
            .cloned()
            .unwrap_or_else(|| vec![DEFAULT_RELAY.into()])
    }

    /// The relay directory on the network swaps run on, if any.
    fn relay_directory(&self) -> Option<Directory> {
        let default = (self.network == Network::Signet).then(|| Directory {
            node: String::new(),
            chain: SIGNET_RELAY_DIRECTORY.into(),
        });
        self.relay_directory
            .get(&self.network)
            .cloned()
            .or(default)
            .filter(|directory| !directory.chain.is_empty())
    }

    /// Whether swaps on the network they run on read Nexus on this app's
    /// own node, not [`PUBLIC_NEXUS`].
    fn own_nexus(&self) -> bool {
        self.own_nexus.contains(&self.network)
    }

    /// Your own Nexus node, by its URL, on the network swaps run on, if any.
    fn remote_nexus(&self) -> Option<&String> {
        self.nexus.get(&self.network)
    }
}

/// Where swaps on a network read Nexus, as the page chooses it: the public
/// node, this app's own, or yours at a URL (`{"remote": url}`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NexusChoice {
    Public,
    Own,
    Remote(String),
}

/// Refuses an Esplora that cannot be read, or is on another chain than
/// `network`, by the engine's own check (`Esplora::check_network`, which
/// reads only block 0: the key is a throwaway).
fn check_esplora(url: &str, network: Network) -> Result<()> {
    let key = swap::wallet::Key::new(&network.to_string())?;
    Esplora::new(url, key, Duration::from_secs(20))
        .check_network(network)
        .map_err(err)?
        .map_err(err)
}

/// The lock's Bitcoin fee for `btc_amount` sat, priced for a wallet with
/// nothing to spend, before any is made: what the engine's `Wallet::need`
/// asks above the amount with no UTXOs (`swap::wallet::need`), at the rate
/// it funds at (`Esplora::wallet_rate`), read from the Esplora at `url`
/// (only its fee estimates: the key is a throwaway).
fn lock_fee(url: &str, network: Network, btc_amount: u64) -> Result<u64> {
    let key = swap::wallet::Key::new(&network.to_string())?;
    let rate = Esplora::new(url, key, Duration::from_secs(20))
        .wallet_rate()
        .map_err(err)?;
    let (_, need) = swap::wallet::need(vec![], btc_amount, rate);
    Ok(need - btc_amount)
}

/// Refuses a Nexus node that cannot be read, that takes no transactions,
/// or that is on another Nexus than [`swap_nexus_tx::MAINNET_GENESIS`]'s or
/// cannot say how deep a transaction is (`swap::session::check_nexus`), as
/// the engine refuses one for a new swap: a public read port at `url`, with
/// no cookie, or this app's node with its cookie.
fn check_nexus(url: &str, cookie: Option<&Path>) -> Result<()> {
    use swap_nexus_tx::rpc::Client;
    let timeout = Duration::from_secs(20);
    let client = match cookie {
        None => Client::public(url, timeout),
        Some(cookie) => Client::from_cookie_file(url, cookie, timeout),
    };
    let read = |e: swap_nexus_tx::rpc::Error| format!("{url}: {e}");
    if cookie.is_none() && client.chain_info().map_err(read)?["acceptsSubmit"] != true {
        return Err(format!("refused: {url} takes no transactions"));
    }
    swap::session::check_nexus(&client, swap_nexus_tx::MAINNET_GENESIS)
        .map_err(read)?
        .map_err(|refusal| refusal.to_string())
}

fn settings_path(root: &Path) -> PathBuf {
    root.join("swap").join("settings.json")
}

/// The settings, checked: an operator's typo is refused, not used.
fn load_settings(root: &Path) -> Result<Settings> {
    let settings: Settings = read_topology_at(&settings_path(root))?
        .map(serde_json::from_value)
        .transpose()
        .map_err(err)?
        .unwrap_or_default();
    settings.check()?;
    Ok(settings)
}

/// A private directory, made if it is missing.
fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(err)?;
    }
    Ok(())
}

/// The swap wallet's directory for `network`, `<root>/swap/<network>`, made
/// with both levels 0700.
fn swap_dir(root: &Path, network: Network) -> Result<PathBuf> {
    let swaps = root.join("swap");
    private_dir(&swaps)?;
    let dir = swaps.join(network.to_string());
    private_dir(&dir)?;
    Ok(dir)
}

/// Whether `<root>/swap/<network>` holds a wallet key, as the engine keeps it.
fn wallet_exists(root: &Path, network: Network) -> bool {
    root.join("swap")
        .join(network.to_string())
        .join("wallet.json")
        .exists()
}

/// The app's RPC port when lattice.json names none (node-manager.ts's
/// `APP_PORTS`).
const APP_RPC_PORT: u16 = 18080;

/// Why a new swap on the app's own node is refused while it is down.
const NODE_DOWN: &str =
    "Your node is not running: a new swap waits for it; a running swap's Bitcoin side goes on.";

/// The app's node: on the pinned port, the one its cookie is known to go
/// to; none pinned (the node is down), on the port `<host>/lattice.json`
/// names, or the app's default, where it will run.
fn own_node(host: &Path, pinned: Option<u16>) -> String {
    let port = pinned
        .or_else(|| rpc_port(&read_topology_at(&host.join("lattice.json")).ok()?).ok())
        .unwrap_or(APP_RPC_PORT);
    format!("http://127.0.0.1:{port}")
}

/// The engine's config: the wallet's directory, the Esplora, and the Nexus
/// node: [`PUBLIC_NEXUS`] or yours at its URL, with no cookie, or the app's
/// own with its cookie ([`own_node`]), even while it is down: the engine
/// resumes a swap on a node it cannot read, its Bitcoin side going on.
fn config_at(root: &Path, host: &Path, pinned: Option<u16>, settings: &Settings) -> Result<Config> {
    let (nexus, cookie) = if settings.own_nexus() {
        (own_node(host, pinned), Some(cookie_path(host)))
    } else {
        let url = settings.remote_nexus().map_or(PUBLIC_NEXUS, String::as_str);
        (url.to_string(), None)
    };
    Ok(Config::new(
        swap_dir(root, settings.network)?,
        settings.network,
        settings.esplora()?,
        nexus,
        cookie,
        swap_nexus_tx::MAINNET_GENESIS.into(),
    ))
}

fn pinned(app: &AppHandle) -> Result<Option<u16>> {
    Ok(*app.state::<PinnedPort>().0.lock().map_err(err)?)
}

/// [`config_at`] for a new swap: refused on the app's own node while it
/// is down.
fn start_config_at(root: &Path, host: &Path, pinned: Option<u16>, settings: &Settings) -> Result<Config> {
    if settings.own_nexus() && pinned.is_none() {
        return Err(NODE_DOWN.into());
    }
    config_at(root, host, pinned, settings)
}

fn config(app: &AppHandle, settings: &Settings) -> Result<Config> {
    config_at(&app_root(app)?, &host_root(app)?, pinned(app)?, settings)
}

/// Sends an event to the page.
type Emit = Arc<dyn Fn(&'static str, Value) + Send + Sync>;

/// Waits `time`, or less once `flag` holds: whether it holds.
fn wait(flag: impl Fn() -> bool, time: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < time {
        if flag() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50).min(time));
    }
    flag()
}

/// What a panic says, as an error.
fn panicked(panic: Box<dyn std::any::Any + Send>) -> String {
    let what = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    format!("the swap engine panicked: {what}")
}

/// A swap's two threads: the engine thread, which runs it, and the one that
/// reports its status.
struct Threads {
    /// Ends the run: a cancel, or the engine closing.
    halt: Arc<AtomicBool>,
    /// Ends the status reports too, with no last one: the engine closing.
    stop: Arc<AtomicBool>,
    engine: JoinHandle<()>,
    reports: JoinHandle<()>,
}

/// `run` on an engine thread until it returns, its failures (a panic is
/// one) emitted as `swap-error` and run again each `poll` until `halt`.
/// `status` is emitted each `poll`, and once more when the run ends.
fn spawn_threads(
    id: String,
    run: impl Fn(&dyn Fn() -> bool) -> Result<()> + Send + 'static,
    status: impl Fn() -> Option<Value> + Send + 'static,
    poll: Duration,
    emit: Emit,
) -> Result<Threads> {
    let halt = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let engine = {
        let (halt, done, emit, id) = (halt.clone(), done.clone(), emit.clone(), id.clone());
        std::thread::Builder::new()
            .name(format!("swap {id}"))
            .spawn(move || {
                let halted = || halt.load(Ordering::SeqCst);
                loop {
                    let ran = catch_unwind(AssertUnwindSafe(|| run(&halted)));
                    let Err(e) = ran.unwrap_or_else(|panic| Err(panicked(panic))) else {
                        break;
                    };
                    emit("swap-error", json!({ "swapId": id, "error": e }));
                    if wait(halted, poll) {
                        break;
                    }
                }
                done.store(true, Ordering::SeqCst);
            })
            .map_err(err)?
    };
    let reports = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name(format!("swap {id} status"))
            .spawn(move || loop {
                let ended = done.load(Ordering::SeqCst);
                if let Some(status) = status() {
                    if !stop.load(Ordering::SeqCst) {
                        emit("swap-status", status);
                    }
                }
                if ended || stop.load(Ordering::SeqCst) {
                    break;
                }
                wait(
                    || stop.load(Ordering::SeqCst) || done.load(Ordering::SeqCst),
                    poll,
                );
            })
    };
    let reports = match reports {
        Ok(reports) => reports,
        Err(e) => {
            // No engine thread left running unwatched.
            halt.store(true, Ordering::SeqCst);
            return Err(err(e));
        }
    };
    Ok(Threads {
        halt,
        stop,
        engine,
        reports,
    })
}

/// A swap on its engine thread.
struct Run {
    swap_id: String,
    buy: Arc<Buy>,
    threads: Threads,
}

impl Run {
    /// `buy` run until it ends, its offer is gone, it is cancelled, or the
    /// engine closes; a run that fails is run again, as the CLI's `resume`
    /// would be.
    fn spawn(buy: Buy, poll: Duration, emit: Emit) -> Result<Self> {
        let swap_id = buy.swap_id();
        let buy = Arc::new(buy);
        let (runs, reads) = (buy.clone(), buy.clone());
        let threads = spawn_threads(
            swap_id.clone(),
            move |halted| runs.run(halted).map(|_| ()).map_err(err),
            move || reads.status().ok(),
            poll,
            emit,
        )?;
        Ok(Self {
            swap_id,
            buy,
            threads,
        })
    }

    /// Whether the swap is still being run.
    fn active(&self) -> bool {
        !self.threads.engine.is_finished()
    }

    /// Whether the swap is still being run or has its last status still to
    /// report.
    fn busy(&self) -> bool {
        self.active() || !self.threads.reports.is_finished()
    }

    /// Stops both threads: theirs to wait for, off the engine's lock. The
    /// swap carries on from its files when it is resumed.
    fn close(self) -> [JoinHandle<()>; 2] {
        self.threads.stop.store(true, Ordering::SeqCst);
        self.threads.halt.store(true, Ordering::SeqCst);
        [self.threads.engine, self.threads.reports]
    }
}

/// The swaps in the wallet to resume: the one that has taken its offer and
/// not ended, if any (the engine runs one at a time), and every punished
/// one.
fn to_resume(wallet: &Wallet) -> Result<(Option<String>, Vec<String>)> {
    Ok((
        wallet.running().map_err(err)?,
        wallet.punished().map_err(err)?,
    ))
}

/// The runs [`to_resume`] picks, else `waiting` (a swap that was waiting
/// for its funds) if it is still there: the followed one and the punished
/// ones. Nothing is left running if any fails.
fn resume_runs(
    wallet: &Arc<Wallet>,
    waiting: Option<String>,
    emit: &Emit,
) -> Result<(Option<Run>, Vec<Run>)> {
    let poll = wallet.config().poll;
    let (running, punished) = to_resume(wallet)?;
    let waiting = waiting.filter(|id| wallet.swaps().is_ok_and(|ids| ids.contains(id)));
    let followed = match running.or(waiting) {
        Some(id) => Some(wallet.resume(&id).map_err(err)?),
        None => None,
    };
    let punished = punished
        .iter()
        .map(|id| wallet.resume(id).map_err(err))
        .collect::<Result<Vec<Buy>>>()?;
    let has_followed = followed.is_some();
    let mut runs = vec![];
    for buy in followed.into_iter().chain(punished) {
        match Run::spawn(buy, poll, emit.clone()) {
            Ok(run) => runs.push(run),
            Err(e) => {
                for run in runs {
                    // Ending on their own; nothing waits on them.
                    drop(run.close());
                }
                return Err(e);
            }
        }
    }
    let mut runs = runs.into_iter();
    let followed = if has_followed { runs.next() } else { None };
    Ok((followed, runs.collect()))
}

/// What was open, taken out of the engine to be stopped off its lock.
#[derive(Default)]
struct Detached {
    threads: Vec<JoinHandle<()>>,
    wallet: Option<Arc<Wallet>>,
}

impl Detached {
    /// Waits for the threads, then drops the wallet, which frees its
    /// directory once nothing else holds it.
    fn finish(self) {
        for thread in self.threads {
            let _ = thread.join();
        }
        drop(self.wallet);
    }
}

/// The open swap wallet and the swaps it runs.
#[derive(Default)]
pub struct Engine {
    wallet: Option<Arc<Wallet>>,
    /// The swap the page follows: the one it started, or the one resumed
    /// running.
    followed: Option<Run>,
    /// Punished swaps resumed to ask the seller for `s`.
    punished: Vec<Run>,
    /// Its runs detached to reopen the wallet, not yet held again: what it
    /// holds says nothing of the swaps.
    reopening: bool,
}

impl Engine {
    fn wallet(&self) -> Result<Arc<Wallet>> {
        self.wallet
            .clone()
            .ok_or_else(|| "no swap wallet open".to_string())
    }

    /// The open wallet, if it is `config`'s: its directory, Esplora and node.
    fn serves(&self, config: &Config) -> Option<Arc<Wallet>> {
        self.wallet.clone().filter(|wallet| {
            let open = wallet.config();
            open.dir == config.dir && open.nexus == config.nexus && open.esplora == config.esplora
        })
    }

    /// Everything open, taken out: the runs told to stop.
    fn detach(&mut self) -> Detached {
        let mut threads = vec![];
        for run in self
            .followed
            .take()
            .into_iter()
            .chain(self.punished.drain(..))
        {
            threads.extend(run.close());
        }
        Detached {
            threads,
            wallet: self.wallet.take(),
        }
    }

    /// The swap still running, the followed one or a punished one asking
    /// for `s`, which would make two running swaps once it gets it.
    fn running(&self) -> Option<&str> {
        self.followed
            .iter()
            .chain(&self.punished)
            .find(|run| run.active())
            .map(|run| run.swap_id.as_str())
    }

    fn refuse_running(&self) -> Result<()> {
        match self.running() {
            Some(id) => Err(format!(
                "refused: swap {id} is in progress; one swap at a time"
            )),
            None => Ok(()),
        }
    }

    /// `wallet`, its swaps resumed, as the engine's.
    fn hold(
        &mut self,
        wallet: Arc<Wallet>,
        waiting: Option<String>,
        emit: &Emit,
    ) -> Result<Arc<Wallet>> {
        let (followed, punished) = resume_runs(&wallet, waiting, emit)?;
        self.wallet = Some(wallet.clone());
        self.followed = followed;
        self.punished = punished;
        Ok(wallet)
    }
}

/// What the engine holds, for the shell's background behaviour (see
/// `background`).
#[derive(Default)]
pub struct Activity {
    /// Whether a run is still going, its last status report included.
    pub busy: bool,
    /// Every held run's swap, the followed one first.
    pub held: Vec<String>,
    /// The punished swaps resumed to ask for `s`.
    pub punished: Vec<String>,
    /// With no wallet open (before it opens), whether the swap wallet
    /// on disk has a running swap to resume (not a punished one: its ask
    /// can wait for the next start). False with a
    /// wallet open, whose runs say it. None while that is not known: the
    /// wallet is being reopened, or cannot be read.
    pub stored: Option<bool>,
}

impl Engine {
    /// What the engine holds; `wallet_dir` is where the swap wallet is kept,
    /// read only while none is open.
    fn activity(&self, wallet_dir: Option<(PathBuf, Network)>) -> Activity {
        let runs = || self.followed.iter().chain(&self.punished);
        let stored = if self.reopening {
            None
        } else if self.wallet.is_some() {
            Some(false)
        } else {
            wallet_dir.and_then(|(dir, network)| {
                running_in(&dir, network).ok().map(|id| id.is_some())
            })
        };
        Activity {
            busy: runs().any(Run::busy),
            held: runs().map(|run| run.swap_id.clone()).collect(),
            punished: self.punished.iter().map(|run| run.swap_id.clone()).collect(),
            stored,
        }
    }
}

/// What the app's engine holds now.
pub fn activity(app: &AppHandle) -> Result<Activity> {
    let wallet_dir = app_root(app).ok().and_then(|root| {
        let network = load_settings(&root).ok()?.network;
        Some((root.join("swap").join(network.to_string()), network))
    });
    Ok(lock(&app.state::<Swaps>().0)?.activity(wallet_dir))
}

fn lock(engine: &Mutex<Engine>) -> Result<MutexGuard<'_, Engine>> {
    engine.lock().map_err(err)
}

/// How long a reopen or a restore waits for the wallet's last holder
/// outside the engine (a balance read, a status read, a process this app
/// spawns, which holds the directory's lock until it starts) to let it go.
const OPEN_PATIENCE: Duration = Duration::from_secs(60);

/// `hold`, tried again while the wallet is still held elsewhere in this
/// app, for up to `patience`.
fn patiently(patience: Duration, hold: impl Fn() -> Result<Arc<Wallet>>) -> Result<Arc<Wallet>> {
    let start = Instant::now();
    loop {
        match hold() {
            Err(e) if e.contains("is open elsewhere") && start.elapsed() < patience => {
                std::thread::sleep(Duration::from_millis(100));
            }
            held => return held,
        }
    }
}

/// `Wallet::open`, patiently: a reopen must not lose to a read that is
/// about to end, or a taken swap would go undriven.
fn open_wallet(config: &Config, patience: Duration) -> Result<Arc<Wallet>> {
    patiently(patience, || Wallet::open(config.clone()).map_err(err))
}

/// The followed swap, while it runs: kept across a reopen even if it has
/// not taken its offer and is waiting for its funds.
fn waiting(held: &Engine) -> Option<String> {
    held.followed
        .as_ref()
        .filter(|run| run.active())
        .map(|run| run.swap_id.clone())
}

/// The wallet for `config`, opened if it is not and its swaps resumed.
/// `config` is read on the engine's lock, where the settings are saved, so
/// the wallet ends on the settings saved last. One open for another config
/// (a node on a new port, another server saved) is closed first, and its
/// threads waited for off the lock, and its swaps resumed on `config`,
/// which checks its servers again; refused if that would leave a running
/// swap's wallet, which it keeps to its end.
fn open_with(
    engine: &Mutex<Engine>,
    config: impl Fn() -> Result<Config>,
    emit: &Emit,
) -> Result<Arc<Wallet>> {
    open_patiently(engine, &config, &config, emit, OPEN_PATIENCE)
}

/// [`open_with`] on `config`, a new swap's: if it fails once the wallet
/// was taken out, the wallet is opened again on `resume` ([`config_at`],
/// which opens on the app's own node even while it is down), so its swaps
/// are still driven, and the failure returned.
fn open_patiently(
    engine: &Mutex<Engine>,
    config: impl Fn() -> Result<Config>,
    resume: impl Fn() -> Result<Config>,
    emit: &Emit,
    patience: Duration,
) -> Result<Arc<Wallet>> {
    // The followed swap waiting for its funds, kept from a wallet this
    // took out (see `waiting`).
    let mut kept = None;
    let mut reopened = false;
    loop {
        let detached = {
            let mut held = lock(engine)?;
            if reopened {
                held.reopening = false;
            }
            // The wallet held, or the config it is not open on.
            let opened = config().and_then(|config| {
                if let Some(wallet) = held.serves(&config) {
                    return Ok(Ok(wallet));
                }
                if held.wallet.is_none() {
                    return open_wallet(&config, patience)
                        .and_then(|wallet| held.hold(wallet, kept.clone(), emit))
                        .map(Ok);
                }
                Ok(Err(config))
            });
            let config = match opened {
                Ok(Ok(wallet)) => return Ok(wallet),
                Ok(Err(config)) => config,
                Err(e) => {
                    if reopened && held.wallet.is_none() {
                        let _ = resume()
                            .and_then(|config| open_wallet(&config, patience))
                            .and_then(|wallet| held.hold(wallet, kept, emit));
                    }
                    return Err(e);
                }
            };
            if let (Some(open), Some(id)) = (&held.wallet, held.running()) {
                if open.config().dir != config.dir {
                    return Err(format!(
                        "refused: swap {id} is in progress on the settings it started with"
                    ));
                }
            }
            held.reopening = true;
            reopened = true;
            kept = waiting(&held).or(kept);
            held.detach()
        };
        detached.finish();
    }
}

/// The policy a swap on `network` is started under: the amount and the
/// price the buyer agreed to, every bound the engine's default but one: off
/// Bitcoin's (signet here, regtest in the tests), `test_btc_for_mainnet_nex`,
/// as this app's test swaps trade mainnet's Nexus for test coins
/// (`swap::offer::check_networks`). Never on Bitcoin's.
fn policy(btc_amount: u64, price: u64, network: Network) -> Value {
    let mut policy = json!({ "btc_amount": btc_amount, "price": price });
    if network != Network::Bitcoin {
        policy["test_btc_for_mainnet_nex"] = json!(true);
    }
    policy
}

/// A swap of `offer` at `btc_amount` and `price`, its NEX to `nex_address`,
/// started on an engine thread: its id. `config` is the engine's config as
/// the settings saved say it, read again on the engine's lock, where the
/// settings are saved: refused if they moved on meanwhile, and while a swap
/// runs. `resume` is the config its wallet reopens on if `config` fails
/// once the wallet was taken out (see `open_patiently`).
fn start_with(
    engine: &Mutex<Engine>,
    config: impl Fn() -> Result<Config>,
    resume: impl Fn() -> Result<Config>,
    offer: &str,
    (btc_amount, price): (u64, u64),
    nex_address: &str,
    emit: &Emit,
) -> Result<String> {
    lock(engine)?.refuse_running()?;
    open_patiently(engine, &config, resume, emit, OPEN_PATIENCE)?;
    let mut held = lock(engine)?;
    // Opening may have resumed one.
    held.refuse_running()?;
    // Saved on this lock, the settings cannot move on before it starts;
    // a save meanwhile took the wallet out.
    let wallet = held
        .serves(&config()?)
        .ok_or("refused: the swap settings changed meanwhile; try again")?;
    let buy = wallet
        .prepare(offer, policy(btc_amount, price, wallet.config().network), nex_address)
        .map_err(err)?;
    let run = Run::spawn(buy, wallet.config().poll, emit.clone())?;
    let id = run.swap_id.clone();
    // The swap before, which has ended, finishes its last report alone.
    held.followed = Some(run);
    Ok(id)
}

/// The wallet in a recovery `file`, put in `config.dir`, its address: every
/// run is stopped and the wallet dropped first, as the engine requires, and
/// then resumed, the restored swaps with it. A restore that fails leaves
/// the wallet as it was: reopened if there was one, none made if not.
fn restore_with(engine: &Mutex<Engine>, config: Config, file: &str, emit: &Emit) -> Result<String> {
    let (waiting, detached) = {
        let mut held = lock(engine)?;
        held.reopening = true;
        (waiting(&held), held.detach())
    };
    detached.finish();
    let existed = config.dir.join("wallet.json").exists();
    let mut held = lock(engine)?;
    held.reopening = false;
    if held.wallet.is_some() {
        return Err("refused: the swap wallet was opened meanwhile; try again".into());
    }
    match patiently(OPEN_PATIENCE, || Wallet::restore(config.clone(), file).map_err(err)) {
        Ok(wallet) => Ok(held.hold(wallet, waiting, emit)?.address().to_string()),
        Err(e) => {
            if existed {
                if let Ok(wallet) = open_wallet(&config, OPEN_PATIENCE) {
                    let _ = held.hold(wallet, waiting, emit);
                }
            }
            Err(e)
        }
    }
}

/// The wallet in `dir`, opened to read its files; none if it holds no
/// wallet (none is made).
fn stored_wallet(dir: &Path, network: Network) -> Result<Option<Arc<Wallet>>> {
    if !dir.join("wallet.json").exists() {
        return Ok(None);
    }
    let config = Config::new(
        dir.into(),
        network,
        String::new(),
        String::new(),
        PathBuf::new(),
        String::new(),
    );
    Wallet::open(config).map(Some).map_err(err)
}

/// The swaps running in the wallet in `dir`, read from its files; none if
/// it holds no wallet.
fn running_in(dir: &Path, network: Network) -> Result<Option<String>> {
    match stored_wallet(dir, network)? {
        Some(wallet) => wallet.running().map_err(err),
        None => Ok(None),
    }
}

/// Saves `<root>/swap/settings.json` as `change` makes it of the settings
/// saved, one save at a time (`saving` held through it): refused if the
/// user's own Esplora it sets is not on its network (and as `change`
/// refuses, a public Nexus node that will not do), while the wallet is
/// reopened, which may resume a swap on the settings left, and, for
/// another network, while a swap runs on the one it leaves. Written on the
/// engine's lock, where a swap starts only on the settings saved. Another
/// network's: the wallet open is taken out there. Another server on the
/// same network: the wallet open stays, its swaps running, until it is
/// opened on the new settings (`open_with`), which resumes them there.
fn save_with(
    engine: &Mutex<Engine>,
    saving: &Mutex<()>,
    root: &Path,
    change: impl FnOnce(Settings) -> Result<Settings>,
) -> Result<()> {
    let _saving = saving.lock().map_err(err)?;
    let leaving = load_settings(root)?;
    let next = change(leaving.clone())?;
    if leaving == next {
        return Ok(());
    }
    next.check()?;
    // One saved before is not read again: a network whose Esplora is down
    // can still be chosen, to choose another.
    if let Some(url) = next.own().filter(|url| leaving.esplora.get(&next.network) != Some(url)) {
        check_esplora(url, next.network)?;
    }
    let moves = leaving.network != next.network;
    let detached = {
        let mut held = lock(engine)?;
        if held.reopening {
            return Err("refused: the swap wallet is being reopened; try again".into());
        }
        if moves {
            held.refuse_running()?;
        }
        if moves && held.wallet.is_none() {
            let network = leaving.network;
            if let Some(id) = running_in(&root.join("swap").join(network.to_string()), network)? {
                return Err(format!("refused: swap {id} is running on {network}"));
            }
        }
        private_dir(&root.join("swap"))?;
        write_atomically(
            &settings_path(root),
            &serde_json::to_vec_pretty(&next).map_err(err)?,
            0o600,
        )?;
        if moves {
            held.detach()
        } else {
            Detached::default()
        }
    };
    detached.finish();
    Ok(())
}

/// Sets the network, the rest kept.
fn set_network_with(
    engine: &Mutex<Engine>,
    saving: &Mutex<()>,
    root: &Path,
    network: Network,
) -> Result<()> {
    save_with(engine, saving, root, |settings| Ok(Settings { network, ..settings }))
}

/// Sets `network`'s servers in one save: its Esplora, the user's own at
/// `esplora` or none for the public default, and its Nexus node: this
/// app's own, or [`PUBLIC_NEXUS`] or yours at an https URL ([`remote_nexus`];
/// the public one's own URL is the public one), each refused unless
/// `check_node`, [`check_nexus`] with no cookie, passes its URL (one saved
/// before is not read again). Either refused, neither is written. The rest
/// kept. Refused once swaps run on another network: they were chosen for
/// the one they ran on.
fn set_servers_with(
    engine: &Mutex<Engine>,
    saving: &Mutex<()>,
    root: &Path,
    network: Network,
    (esplora, nexus): (Option<String>, NexusChoice),
    check_node: impl Fn(&str) -> Result<()>,
) -> Result<()> {
    let esplora = match esplora.as_deref().map(str::trim) {
        None => None,
        Some("") => return Err("refused: give your Esplora's URL".into()),
        Some(url) => Some(url.trim_end_matches('/').to_string()),
    };
    let nexus = match nexus {
        NexusChoice::Remote(url) => match remote_nexus(&url)? {
            url if url == PUBLIC_NEXUS => NexusChoice::Public,
            url => NexusChoice::Remote(url),
        },
        choice => choice,
    };
    save_with(engine, saving, root, |mut settings| {
        if settings.network != network {
            return Err(format!(
                "refused: swaps now run on {}, not {network}",
                settings.network
            ));
        }
        match esplora {
            Some(url) => settings.esplora.insert(network, url),
            None => settings.esplora.remove(&network),
        };
        let own = settings.own_nexus.remove(&network);
        let remote = settings.nexus.remove(&network);
        match nexus {
            NexusChoice::Own => {
                settings.own_nexus.insert(network);
            }
            NexusChoice::Public if own || remote.is_some() => check_node(PUBLIC_NEXUS)?,
            NexusChoice::Public => {}
            NexusChoice::Remote(url) => {
                if remote.as_ref() != Some(&url) {
                    check_node(&url)?;
                }
                settings.nexus.insert(network, url);
            }
        }
        Ok(settings)
    })
}

/// The engine, as the app manages it, the lock held through each save of
/// the settings (see `save_with`), and the relays the last read of the
/// relay directory found (see `board_cap`).
#[derive(Default)]
pub struct Swaps(Mutex<Engine>, Mutex<()>, Mutex<Vec<String>>);

fn emitter(app: &AppHandle) -> Emit {
    let app = app.clone();
    Arc::new(move |event, payload| {
        if event == "swap-status" {
            crate::background::report(&app, &payload);
        }
        let _ = app.emit(event, payload);
    })
}

/// `act` off the async runtime: it may wait on the network and on engine
/// threads.
async fn blocking<T: Send + 'static>(
    app: AppHandle,
    act: impl FnOnce(&AppHandle, &Mutex<Engine>) -> Result<T> + Send + 'static,
) -> Result<T> {
    tauri::async_runtime::spawn_blocking(move || act(&app, &app.state::<Swaps>().0))
        .await
        .map_err(err)?
}

/// `read` on the open wallet, off the async runtime and the engine's lock.
async fn with_wallet<T: Send + 'static>(
    app: AppHandle,
    read: impl FnOnce(&Wallet) -> Result<T> + Send + 'static,
) -> Result<T> {
    blocking(app, |_, engine| {
        let wallet = lock(engine)?.wallet()?;
        read(&wallet)
    })
    .await
}

/// Opens the swap wallet, if there is one, at launch, again when the
/// node's port is pinned or moves (after `lattice up`), and after a save.
/// Its swaps carry on.
pub fn sync(app: &AppHandle) -> Result<()> {
    sync_at(
        &app.state::<Swaps>().0,
        &app_root(app)?,
        &host_root(app)?,
        || pinned(app),
        &emitter(app),
    )
}

/// The wallet in `root`, if there is one, opened on the settings saved
/// ([`config_at`], read on the engine's lock: see `open_with`): on this
/// app's own node even while it is down, so a swap that has taken its
/// offer is driven.
fn sync_at(
    engine: &Mutex<Engine>,
    root: &Path,
    host: &Path,
    pinned: impl Fn() -> Result<Option<u16>>,
    emit: &Emit,
) -> Result<()> {
    if !wallet_exists(root, load_settings(root)?.network) {
        return Ok(());
    }
    open_with(engine, || config_at(root, host, pinned()?, &load_settings(root)?), emit).map(|_| ())
}

#[tauri::command]
pub fn swap_settings(app: AppHandle) -> Result<Shown> {
    let root = app_root(&app)?;
    let settings = load_settings(&root)?;
    // A lattice.json that cannot be read only means no directory.
    let nodes = host_root(&app).and_then(|host| directory_nodes(&settings, &host, pinned(&app)?));
    let directory = nodes.ok().flatten().is_some();
    Ok(Shown { file: settings_path(&root), settings, directory })
}

/// The settings as the page reads them: with the file they are kept in,
/// and whether a relay directory is read on the network swaps run on (one
/// is set, and a node to read it on: [`directory_nodes`]).
#[derive(Serialize)]
pub struct Shown {
    #[serde(flatten)]
    settings: Settings,
    file: PathBuf,
    directory: bool,
}

/// Chooses the Bitcoin network swaps run on; the wallet on it opens.
/// Refused while a swap runs.
#[tauri::command]
pub async fn swap_set_network(app: AppHandle, network: Network) -> Result<()> {
    blocking(app, move |app, engine| {
        set_network_with(engine, &app.state::<Swaps>().1, &app_root(app)?, network)?;
        sync(app)
    })
    .await
}

/// Chooses the servers swaps on `network` read, in one save: Bitcoin
/// through the user's own Esplora, or mempool.space's when `esplora` is
/// null, and Nexus through `nexus`: [`PUBLIC_NEXUS`], this app's node, or
/// yours at an https URL; a running swap goes on on them. Refused, neither
/// saved, for an Esplora on another network, a node that will not do, and
/// once swaps run on another network.
#[tauri::command]
pub async fn swap_set_servers(
    app: AppHandle,
    network: Network,
    esplora: Option<String>,
    nexus: NexusChoice,
) -> Result<()> {
    blocking(app, move |app, engine| {
        let saving = &app.state::<Swaps>().1;
        let check_node = |url: &str| check_nexus(url, None);
        set_servers_with(
            engine,
            saving,
            &app_root(app)?,
            network,
            (esplora, nexus),
            check_node,
        )?;
        sync(app)
    })
    .await
}

/// Whether the Esplora is reachable and on the network swaps run on: the
/// refusal if not.
#[tauri::command]
pub async fn swap_check_esplora(app: AppHandle) -> Result<()> {
    let settings = load_settings(&app_root(&app)?)?;
    tauri::async_runtime::spawn_blocking(move || check_esplora(&settings.esplora()?, settings.network))
        .await
        .map_err(err)?
}

/// Whether the Nexus node swaps read is reachable, on the Nexus, and (one
/// with no cookie: the public one or yours by URL) takes transactions: the
/// refusal if not.
#[tauri::command]
pub async fn swap_check_nexus(app: AppHandle) -> Result<()> {
    let settings = load_settings(&app_root(&app)?)?;
    let own = settings.own_nexus().then(|| host_root(&app)).transpose()?;
    let pinned = pinned(&app)?;
    let url = settings
        .remote_nexus()
        .map_or(PUBLIC_NEXUS, String::as_str)
        .to_string();
    tauri::async_runtime::spawn_blocking(move || match (own, pinned) {
        (None, _) => check_nexus(&url, None),
        (Some(_), None) => Err(NODE_DOWN.into()),
        (Some(host), pinned) => check_nexus(&own_node(&host, pinned), Some(&cookie_path(&host))),
    })
    .await
    .map_err(err)?
}

/// Whether there is a swap wallet, none made: as the driver's
/// `Wallet.exists`.
#[tauri::command]
pub fn swap_wallet_exists(app: AppHandle) -> Result<bool> {
    let root = app_root(&app)?;
    Ok(wallet_exists(&root, load_settings(&root)?.network))
}

/// Opens the swap wallet, made if there is none, and resumes its swaps: its
/// deposit address.
#[tauri::command]
pub async fn swap_wallet_open(app: AppHandle) -> Result<String> {
    blocking(app, |app, engine| {
        let config = || config(app, &load_settings(&app_root(app)?)?);
        let wallet = open_with(engine, config, &emitter(app))?;
        Ok(wallet.address().to_string())
    })
    .await
}

#[derive(Serialize)]
pub struct Balance {
    confirmed: u64,
    unconfirmed: u64,
}

#[tauri::command]
pub async fn swap_balance(app: AppHandle) -> Result<Balance> {
    with_wallet(app, |wallet| {
        let (confirmed, unconfirmed) = wallet.balance().map_err(err)?;
        Ok(Balance {
            confirmed,
            unconfirmed,
        })
    })
    .await
}

#[derive(Serialize)]
pub struct Need {
    enough: bool,
    need: u64,
}

/// Whether the spendable balance funds a swap of `btc_amount` sat, and the
/// balance it needs.
#[tauri::command]
pub async fn swap_need(app: AppHandle, btc_amount: u64) -> Result<Need> {
    with_wallet(app, move |wallet| {
        let (enough, need) = wallet.need(btc_amount).map_err(err)?;
        Ok(Need { enough, need })
    })
    .await
}

/// The lock's Bitcoin fee for `btc_amount` sat with no wallet yet, from the
/// Esplora the settings name: [`lock_fee`]. No wallet is opened or made.
#[tauri::command]
pub async fn swap_lock_fee(app: AppHandle, btc_amount: u64) -> Result<u64> {
    let settings = load_settings(&app_root(&app)?)?;
    tauri::async_runtime::spawn_blocking(move || {
        lock_fee(&settings.esplora()?, settings.network, btc_amount)
    })
    .await
    .map_err(err)?
}

/// Starts a swap of `offer` (an offer's text, as `swap_offers` lists it)
/// for `btc_amount` sat at `price`, its NEX to `nex_address`: its id. Every
/// other bound is the engine's default, never the page's. The wallet is
/// opened first if it is not.
#[tauri::command]
pub async fn swap_start(
    app: AppHandle,
    offer: String,
    btc_amount: u64,
    price: u64,
    nex_address: String,
) -> Result<String> {
    blocking(app, move |app, engine| {
        start_with(
            engine,
            || {
                let settings = load_settings(&app_root(app)?)?;
                start_config_at(&app_root(app)?, &host_root(app)?, pinned(app)?, &settings)
            },
            || config(app, &load_settings(&app_root(app)?)?),
            &offer,
            (btc_amount, price),
            &nex_address,
            &emitter(app),
        )
    })
    .await
}

/// Cancels the followed swap while it has not taken its offer; its last
/// status follows as a `swap-status` event.
#[tauri::command]
pub async fn swap_cancel(app: AppHandle) -> Result<()> {
    blocking(app, |_, engine| {
        let (buy, halt) = {
            let held = lock(engine)?;
            let run = held.followed.as_ref().ok_or("no swap to cancel")?;
            (run.buy.clone(), run.threads.halt.clone())
        };
        buy.cancel().map_err(err)?;
        halt.store(true, Ordering::SeqCst);
        Ok(())
    })
    .await
}

/// The followed swap's status, as the driver's `Buyer.status()`; null when
/// there is none.
#[tauri::command]
pub async fn swap_status(app: AppHandle) -> Result<Option<Value>> {
    blocking(app, |_, engine| {
        let buy = lock(engine)?.followed.as_ref().map(|run| run.buy.clone());
        buy.map(|buy| buy.status().map_err(err)).transpose()
    })
    .await
}

/// The recovery file, the wallet key and every swap not ended, saved as a
/// new 0600 file in ~/Downloads, as `save_backup` saves the vault's: its
/// path. The key never passes through the page.
#[tauri::command]
pub async fn swap_backup(app: AppHandle) -> Result<String> {
    let dir = app.path().download_dir().map_err(err)?;
    with_wallet(app, move |wallet| {
        let (name, file) = wallet.backup().map_err(err)?;
        Ok(write_new(&dir, &name, &file)?.display().to_string())
    })
    .await
}

/// A recovery file's text, read from `path`: refused past [`MAX_BODY`], and
/// unless it is a regular file.
fn read_recovery(path: &Path) -> Result<String> {
    // Not a FIFO or a device, which would block the read or never end.
    if !fs::metadata(path).map_err(err)?.is_file() {
        return Err("not a recovery file".into());
    }
    let mut text = String::new();
    fs::File::open(path)
        .map_err(err)?
        .take(MAX_BODY + 1)
        .read_to_string(&mut text)
        .map_err(|_| "not a recovery file".to_string())?;
    if text.len() as u64 > MAX_BODY {
        return Err("not a recovery file".into());
    }
    Ok(text)
}

/// The wallet in a recovery file the user picks in a native open dialog,
/// read here: the key never passes through the page. Its address; null
/// when no file is picked.
#[tauri::command]
pub async fn swap_restore_file(app: AppHandle) -> Result<Option<String>> {
    blocking(app, |app, engine| {
        let Some(picked) = app
            .dialog()
            .file()
            .set_title("Restore a swap wallet")
            .add_filter("Swap recovery file", &["json"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let file = read_recovery(&picked.into_path().map_err(err)?)?;
        let settings = load_settings(&app_root(app)?)?;
        restore_with(engine, config(app, &settings)?, &file, &emitter(app)).map(Some)
    })
    .await
}

/// Sends the wallet's whole spendable balance to `address`: the txid.
/// Refused while a swap runs.
#[tauri::command]
pub async fn swap_withdraw(app: AppHandle, address: String) -> Result<String> {
    with_wallet(app, move |wallet| {
        Ok(wallet.withdraw(&address).map_err(err)?.to_string())
    })
    .await
}

/// An offer on a relay's board: its text, which `swap_start` takes, and
/// what it says.
#[derive(Serialize, Debug)]
pub struct Listed {
    text: String,
    offer: Value,
}

/// Whether every number in `value` is one JavaScript holds exactly.
fn safe(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            n.as_u64().is_none_or(|n| n <= (1 << 53))
                && n.as_i64().is_none_or(|n| n.unsigned_abs() <= (1 << 53))
        }
        Value::Array(items) => items.iter().all(safe),
        Value::Object(fields) => fields.values().all(safe),
        _ => true,
    }
}

/// The offers in a relay's board, `body`, for `network` and the Nexus of
/// `genesis`, cheapest NEX first, as the website lists them. Left out: an
/// offer that does not decode or is not signed by its seller, any past the
/// first [`MAX_OFFERS`], one at a price of 0, and one with a number the
/// page cannot hold exactly. The page leaves out one that names another
/// relay than the one it is listed on.
fn listed(body: &str, network: Network, genesis: &str) -> Result<Vec<Listed>> {
    let board: Vec<Value> =
        serde_json::from_str(body).map_err(|e| format!("not an offer board: {e}"))?;
    let mut list = vec![];
    for item in board.into_iter().take(MAX_OFFERS) {
        let text = item.to_string();
        let Ok(offer) = Offer::decode(&text) else {
            continue;
        };
        if offer.quote.price == 0
            || offer.btc_network != network
            || offer.template.nexus_genesis != genesis
        {
            continue;
        }
        let shown = serde_json::to_value(&offer).map_err(err)?;
        if safe(&shown) {
            list.push((offer.quote.price, Listed { text, offer: shown }));
        }
    }
    list.sort_by_key(|(price, _)| *price);
    Ok(list.into_iter().map(|(_, listed)| listed).collect())
}

/// The most bytes of `relay`'s board read: [`MAX_OWN_BOARD`] for one of
/// the settings' relays on the network swaps run on, [`MAX_FOUND_BOARD`]
/// for one the last read of the relay directory found. Refused for any
/// other: the page reads no relay but these.
fn board_cap(settings: &Settings, found: &[String], relay: &str) -> Result<u64> {
    if settings.relays().iter().any(|own| own == relay) {
        Ok(MAX_OWN_BOARD)
    } else if found.iter().any(|url| url == relay) {
        Ok(MAX_FOUND_BOARD)
    } else {
        Err(format!("refused: {relay} is neither a relay of the settings nor one the relay directory lists"))
    }
}

/// The offers of `relay`, `GET <relay>/offers`, as [`listed`] keeps them,
/// its board read up to [`board_cap`].
#[tauri::command]
pub async fn swap_offers(app: AppHandle, relay: String) -> Result<Vec<Listed>> {
    let settings = load_settings(&app_root(&app)?)?;
    let max = board_cap(&settings, &app.state::<Swaps>().2.lock().map_err(err)?, &relay)?;
    check_url(&relay)?;
    tauri::async_runtime::spawn_blocking(move || {
        let url = format!("{}/offers", relay.trim_end_matches('/'));
        let response = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .redirects(0)
            .build()
            .get(&url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        let mut body = String::new();
        response
            .into_reader()
            .take(max + 1)
            .read_to_string(&mut body)
            .map_err(err)?;
        if body.len() as u64 > max {
            return Err(format!("{url}: too large"));
        }
        listed(&body, settings.network, swap_nexus_tx::MAINNET_GENESIS)
    })
    .await
    .map_err(err)?
}

/// The nodes the relay directory of the network swaps run on is read on,
/// in order, and its chain: this app's node when it hosts the chain and
/// runs (its port pinned, the one its cookie is known to go to), as the
/// node whose word on the tip is the user's own, then the node the
/// settings name; none if there is neither, or no directory.
fn directory_nodes(settings: &Settings, host: &Path, pinned: Option<u16>) -> Result<Option<(Vec<swap_nexus_tx::rpc::Client>, Vec<String>)>> {
    use swap_nexus_tx::rpc::Client;
    let Some(directory) = settings.relay_directory() else {
        return Ok(None);
    };
    let hosted = read_topology_at(&host.join("lattice.json"))?
        .and_then(|topology| topology.get("hostedChains").cloned())
        .is_some_and(|chains| chains.as_array().is_some_and(|c| c.contains(&json!(directory.chain))));
    let mut nodes = vec![];
    if hosted && pinned.is_some() {
        nodes.push(Client::from_cookie_file(&own_node(host, pinned), cookie_path(host), DIRECTORY_TIMEOUT));
    }
    if !directory.node.is_empty() {
        nodes.push(Client::public(&directory.node, DIRECTORY_TIMEOUT));
    }
    let chain = directory.chain.split('/').map(String::from).collect();
    Ok((!nodes.is_empty()).then_some((nodes, chain)))
}

/// The URLs of the relays the relay directory lists at its chain's tip
/// (`swap::relay_dir::list_hosted`), read on each of [`directory_nodes`]
/// in turn until one answers, each within [`DIRECTORY_TIMEOUT`]; none if
/// there is no node to read it on. The tip's node can hide relays or name
/// useless ones, never move funds: an offer is its seller's, and the page
/// lists it only on the relay it names.
fn read_directory(settings: &Settings, host: &Path, pinned: Option<u16>) -> Result<Vec<String>> {
    let Some((nodes, chain)) = directory_nodes(settings, host, pinned)? else {
        return Ok(vec![]);
    };
    let mut failed = vec![];
    for node in nodes {
        node.set_deadline(Some(Instant::now() + DIRECTORY_TIMEOUT));
        match swap::relay_dir::list_hosted(&node, &chain) {
            Ok(relays) => return Ok(relays.into_iter().map(|relay| relay.url).collect()),
            Err(e) => failed.push(e),
        }
    }
    Err(failed.join("; "))
}

/// [`read_directory`] for the network swaps run on; the relays found are
/// kept, as the only ones besides the settings' that `swap_offers` reads.
#[tauri::command]
pub async fn swap_relay_directory(app: AppHandle) -> Result<Vec<String>> {
    let settings = load_settings(&app_root(&app)?)?;
    let host = host_root(&app)?;
    let pinned = pinned(&app)?;
    let found = tauri::async_runtime::spawn_blocking(move || read_directory(&settings, &host, pinned))
        .await
        .map_err(err)??;
    app.state::<Swaps>().2.lock().map_err(err)?.clone_from(&found);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swap::offer::{Bounds, Place, Policy, Quote, Template};

    /// The page's driver lists offers for the Nexus this engine runs on, and
    /// shows the relay it reads when the settings name none.
    #[test]
    fn the_page_names_this_engines_nexus_and_relay() {
        let driver = include_str!("../../src/swap-driver/index.ts");
        assert!(driver.contains(&format!("\"{}\"", swap_nexus_tx::MAINNET_GENESIS)));
        let screen = include_str!("../../src/swap-screen.ts");
        assert!(screen.contains(&format!("\"{DEFAULT_RELAY}\"")));
        let host = PUBLIC_NEXUS.strip_prefix("https://").unwrap();
        assert!(screen.contains(&format!("\"{host}\"")));
    }

    /// A fresh directory of its own: named for the test, this process, and
    /// a count, so no two tests, nor two runs of one, ever share it.
    fn temp(name: &str) -> PathBuf {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNT.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("lattice-swap-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A wallet's config whose node is on `port` and whose backends nothing
    /// listens on: every read fails at once.
    fn offline_at(dir: &Path, port: u16) -> Config {
        let mut config = Config::new(
            dir.into(),
            Network::Signet,
            "http://127.0.0.1:9".into(),
            format!("http://127.0.0.1:{port}"),
            dir.join(".cookie"),
            swap_nexus_tx::MAINNET_GENESIS.into(),
        );
        config.poll = Duration::from_millis(100);
        config
    }

    fn offline(dir: &Path) -> Config {
        offline_at(dir, 9)
    }

    /// A swap record as the engine keeps one, its state `state`.
    fn record(dir: &Path, id: &str, taken: bool, state: Value) {
        let record = json!({ "swap_id": id, "offer": {}, "policy": {}, "state": state.to_string(), "pending": [], "taken": taken });
        fs::write(dir.join(format!("{id}.record")), record.to_string()).unwrap();
    }

    fn offer(price: u64, network: Network, genesis: &str, seed: u8) -> Value {
        let seller = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let template = Template {
            swap_id: [seed; 32],
            btc_fee: bitcoin::Amount::from_sat(1_000),
            nex_fee: 10,
            t1: 144,
            t2: 144,
            btc_confirmations: 6,
            nex_confirmations: 12,
            margin: 6,
            nexus_genesis: genesis.into(),
        };
        let quote = Quote {
            price,
            min_btc_amount: 10_000,
            max_btc_amount: 1_000_000,
        };
        let made = Offer::new(
            quote,
            template,
            network,
            Place::Relay(DEFAULT_RELAY.into()),
            &seller,
            [7; 32],
        );
        serde_json::to_value(made).unwrap()
    }

    type Events = Arc<Mutex<Vec<(&'static str, Value)>>>;

    fn collect() -> (Emit, Events) {
        let events = Arc::new(Mutex::new(vec![]));
        let sink = events.clone();
        (
            Arc::new(move |event, payload| sink.lock().unwrap().push((event, payload))),
            events,
        )
    }

    /// Waits until an event `event` matches `matches`.
    fn wait_for(events: &Events, event: &str, matches: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !events
            .lock()
            .unwrap()
            .iter()
            .any(|(name, payload)| *name == event && matches(payload))
        {
            assert!(Instant::now() < deadline, "no {event} event");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn close(engine: &Mutex<Engine>) {
        let detached = lock(engine).unwrap().detach();
        detached.finish();
    }

    /// The public node, or this app's when `own`.
    fn choice(own: bool) -> NexusChoice {
        if own {
            NexusChoice::Own
        } else {
            NexusChoice::Public
        }
    }

    /// The public node checked as the app checks it.
    fn public(url: &str) -> Result<()> {
        check_nexus(url, None)
    }

    fn followed(engine: &Mutex<Engine>) -> Option<String> {
        lock(engine)
            .unwrap()
            .followed
            .as_ref()
            .map(|run| run.swap_id.clone())
    }

    #[cfg(unix)]
    #[test]
    fn the_swap_wallet_lives_in_a_private_directory_per_network() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("dir");
        let dir = swap_dir(&root, Network::Bitcoin).unwrap();
        assert_eq!(dir, root.join("swap").join("bitcoin"));
        assert_eq!(
            swap_dir(&root, Network::Signet).unwrap(),
            root.join("swap").join("signet")
        );
        for dir in [root.join("swap"), dir.clone()] {
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                dir.display()
            );
        }
        // Made loose by hand: private again.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        swap_dir(&root, Network::Bitcoin).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Opened there, the wallet makes its key there.
        let wallet = Wallet::open(offline(&dir)).unwrap();
        assert!(wallet_exists(&root, Network::Bitcoin));
        assert!(!wallet_exists(&root, Network::Signet));
        drop(wallet);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_engine_reads_only_https_or_http_to_this_machine_a_local_network_or_an_onion() {
        for ok in [
            "https://mempool.space/api",
            "http://127.0.0.1:3002/api",
            "http://127.0.0.2:3002/api",
            "http://localhost:3000",
            "http://[::1]:3000/api",
            "http://127.0.0.1",
            "http://umbrel.local:3006/api",
            "http://UMBREL.LOCAL/api",
            "http://192.168.1.20:3002/api",
            "http://10.0.0.5/api",
            "http://172.16.0.1/api",
            "http://172.31.255.254/api",
            "http://169.254.1.1/api",
            "http://[fd00::1]:3000/api",
            "http://[fe80::1]/api",
            "http://explorerzydxu5ecjrkwceayqybizmpjjznk5izmitf2modhcusuqlid.onion/api",
            "http://umbrel.local./api",
            "http://node.lan/api",
            "http://node.home.arpa/api",
            "HTTP://192.168.1.20/api",
            "http://[::ffff:10.0.0.1]/api",
            "http://[::ffff:127.0.0.1]/api",
            // Spelled as ureq reads them: 192.168.1.1, 127.0.0.1, 10.0.0.1.
            "http://3232235777/api",
            "http://0177.0.0.1/api",
            "http://0xa.0.0.1/api",
            // The host is 192.168.1.20; `\` is `/`.
            "http://192.168.1.20\\evil.com/api",
            "http:/127.0.0.1/api",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://mempool.space/api",
            "http://127.0.0.1.evil.example/api",
            "http://127.0.0.1@evil.example/api",
            "http://user@127.0.0.1/api",
            "https://user@mempool.space/api",
            "https://:pass@mempool.space/api",
            "http://localhost.evil.example",
            "http://[::2]:3000",
            "http://8.8.8.8/api",
            "http://172.32.0.1/api",
            "http://192.168.1.20.evil.example/api",
            "http://umbrel.local.evil.example/api",
            // One label: `ws` has an address of its own.
            "http://umbrel:3006/api",
            "http://ws/api",
            "http://onion.example/api",
            "http://[2001:db8::1]/api",
            // `\` is `/` in an http URL: the host is the public one.
            "http://evil.com\\x.local/api",
            "http://evil.com\\.onion",
            "http://8.8.8.8\\.local",
            "http://evil.com\\@127.0.0.1/api",
            "http://127.0.0.1:80@evil.com/api",
            "http://[::ffff:8.8.8.8]/api",
            "http://[::]/api",
            "http://0.0.0.0/api",
            // 8.8.8.8, spelled otherwise.
            "http://134744072/api",
            "http://010.010.010.010/api",
            "http://0x8.0x8.0x8.0x8/api",
            "http://mempool.space./api",
            "http://com./api",
            "http://MEMPOOL.SPACE/api",
            "http://evil.lan.com/api",
            "http://home.arpa.evil.com/api",
            "http:/evil.com/api",
            "file:///etc/passwd",
            "ws://127.0.0.1",
            "https://",
            "https://a b",
            "",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_default_to_mainnet_and_a_bad_file_is_refused() {
        let root = temp("settings");
        assert_eq!(load_settings(&root).unwrap(), Settings::default());
        assert_eq!(
            Settings::default().esplora().unwrap(),
            "https://mempool.space/api"
        );
        assert_eq!(Settings::default().relays(), [DEFAULT_RELAY]);
        assert_eq!(Settings::default().relay_directory(), None);
        let signet: Settings = serde_json::from_value(json!({ "network": "signet" })).unwrap();
        assert_eq!(signet.relays(), [DEFAULT_RELAY]);
        assert_eq!(signet.relay_directory().unwrap().chain, SIGNET_RELAY_DIRECTORY);
        assert_eq!(
            signet.esplora().unwrap(),
            "https://mempool.space/signet/api"
        );
        fs::create_dir_all(root.join("swap")).unwrap();
        for bad in [
            json!({ "network": "regtest" }),
            json!({ "network": "testnet" }),
            json!({ "network": "bitcoin", "esplora": "http://lying.example/api" }),
            json!({ "network": "bitcoin", "relay": "file:///etc" }),
            json!({ "network": "bitcoin", "relays": { "signet": ["file:///etc"] } }),
            json!({ "network": "bitcoin", "relayDirectory": { "signet": { "node": "http://a.example", "chain": "C" } } }),
        ] {
            fs::write(settings_path(&root), bad.to_string()).unwrap();
            assert!(load_settings(&root).is_err(), "{bad}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_are_saved_as_a_build_before_one_esplora_per_network_reads_them() {
        // None of your own: no `esplora`, which such a build would refuse
        // as a map.
        let public = Settings::default();
        let saved = serde_json::to_value(&public).unwrap();
        assert_eq!(saved, json!({ "network": "bitcoin" }));
        assert_eq!(serde_json::from_value::<Settings>(saved).unwrap(), public);
        // Yours, one per network, read back as saved.
        let own = Settings {
            network: Network::Signet,
            esplora: BTreeMap::from([
                (Network::Bitcoin, "http://umbrel.local:3006/api".into()),
                (Network::Signet, "https://signet.example/api".into()),
            ]),
            own_nexus: BTreeSet::from([Network::Signet]),
            nexus: BTreeMap::from([(Network::Bitcoin, "https://node.example".into())]),
            relays: BTreeMap::from([(Network::Signet, vec![])]),
            relay_directory: BTreeMap::from([(
                Network::Bitcoin,
                Directory { node: "https://node.example".into(), chain: "Nexus/swap-relays".into() },
            )]),
        };
        let saved = serde_json::to_value(&own).unwrap();
        assert_eq!(saved["esplora"]["signet"], "https://signet.example/api");
        assert_eq!(saved["ownNexus"], json!(["signet"]));
        assert_eq!(saved["nexus"], json!({ "bitcoin": "https://node.example" }));
        assert_eq!(saved["relays"], json!({ "signet": [] }));
        assert_eq!(saved["relayDirectory"]["bitcoin"]["chain"], "Nexus/swap-relays");
        assert_eq!(serde_json::from_value::<Settings>(saved).unwrap(), own);
        // None listed: no relays, not the default.
        assert!(Settings { network: Network::Signet, ..own.clone() }.relays().is_empty());
        // An empty chain: no directory, not the default.
        let none: Settings = serde_json::from_value(
            json!({ "network": "signet", "relayDirectory": { "signet": { "chain": "" } } }),
        )
        .unwrap();
        assert_eq!(none.relay_directory(), None);
        // One URL, as such a build saves it: the network's it was saved for.
        let old: Settings = serde_json::from_value(
            json!({ "network": "signet", "esplora": "https://signet.example/api", "relay": null }),
        )
        .unwrap();
        assert_eq!(old.own().unwrap(), "https://signet.example/api");
        assert!(!old.esplora.contains_key(&Network::Bitcoin));
        assert_eq!(old.relays(), [DEFAULT_RELAY]);
        // One relay, as such a build saves it: every network's, unless one
        // names its own.
        let old: Settings = serde_json::from_value(json!({
            "network": "bitcoin",
            "relay": "https://relay.example",
            "relays": { "signet": ["https://signet-relay.example"] },
        }))
        .unwrap();
        assert_eq!(old.relays(), ["https://relay.example"]);
        assert_eq!(old.relays[&Network::Signet], ["https://signet-relay.example"]);
        // A network no one knows, named.
        let unknown = serde_json::from_value::<Settings>(
            json!({ "network": "bitcoin", "esplora": { "mainnet": "https://a.example/api" } }),
        )
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("\"mainnet\", which is no Bitcoin network"), "{unknown}");
    }

    #[test]
    fn the_page_sets_the_network_and_never_off_a_running_swap() {
        let root = temp("network");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        fs::create_dir_all(root.join("swap")).unwrap();
        let operator = json!({ "network": "bitcoin", "relay": "https://relay.example" });
        fs::write(settings_path(&root), operator.to_string()).unwrap();
        set_network_with(&engine, &saving, &root, Network::Signet).unwrap();
        let mut expected: Settings = serde_json::from_value(operator).unwrap();
        expected.network = Network::Signet;
        assert_eq!(
            load_settings(&root).unwrap(),
            expected,
            "the operator's relay is kept"
        );
        assert!(set_network_with(&engine, &saving, &root, Network::Regtest).is_err());

        // A swap running on signet, its wallet closed: signet is not left.
        let signet = swap_dir(&root, Network::Signet).unwrap();
        drop(Wallet::open(offline(&signet)).unwrap());
        let running = "55".repeat(32);
        record(&signet, &running, true, json!({ "Locking": {} }));
        let refused = set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap_err();
        assert!(refused.contains(&running), "{refused}");
        // Nor while the engine runs it.
        let (emit, _) = collect();
        open_with(&engine, || Ok(offline(&signet)), &emit).unwrap();
        let refused = set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap_err();
        assert!(refused.contains(&running), "{refused}");
        assert_eq!(load_settings(&root).unwrap().network, Network::Signet);
        close(&engine);

        // Ended, a punished one asking for `s`, its wallet closed, holds no
        // save: its ask has no deadline, and waits for the next start on
        // signet, as it does with the wallet open.
        record(&signet, &running, true, json!("Done"));
        let punished = "66".repeat(32);
        record(&signet, &punished, true, json!({ "Punished": {} }));
        set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap();
        set_network_with(&engine, &saving, &root, Network::Signet).unwrap();
        fs::remove_file(signet.join(format!("{punished}.record"))).unwrap();
        // Nor is it left while the wallet is reopened: the reopen may resume
        // a swap waiting for its funds on signet.
        lock(&engine).unwrap().reopening = true;
        let refused = set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap_err();
        assert!(refused.contains("being reopened"), "{refused}");
        lock(&engine).unwrap().reopening = false;
        assert_eq!(load_settings(&root).unwrap().network, Network::Signet);

        // Ended, it is left, and the wallet open is closed.
        open_with(&engine, || Ok(offline(&signet)), &emit).unwrap();
        set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap();
        assert!(lock(&engine).unwrap().wallet.is_none());
        assert_eq!(load_settings(&root).unwrap().network, Network::Bitcoin);
        fs::remove_dir_all(root).unwrap();
    }

    /// An Esplora whose block 0 is `network`'s genesis, on loopback: its
    /// URL. Every request is answered with that hash.
    fn esplora_on(network: Network) -> String {
        slow_esplora_on(network, Duration::ZERO).0
    }

    /// [`esplora_on`], answering each request only after `delay`: its URL,
    /// and whether it has been asked.
    fn slow_esplora_on(network: Network, delay: Duration) -> (String, Arc<AtomicBool>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        let genesis = bitcoin::constants::genesis_block(network).block_hash().to_string();
        let asked = Arc::new(AtomicBool::new(false));
        let asks = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                asks.store(true, Ordering::SeqCst);
                std::thread::sleep(delay);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    genesis.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(genesis.as_bytes());
            }
        });
        (url, asked)
    }

    /// An Esplora on loopback answering every request with `body`: its URL.
    fn answering(body: &'static str) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let _ = stream.read(&mut [0u8; 4096]);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        url
    }

    #[test]
    fn the_relay_directory_is_read_on_this_apps_node_then_the_settings_node() {
        let host = temp("directory");
        fs::create_dir_all(&host).unwrap();
        // This app's node down: nothing listens on its port, pinned as if it
        // had just stopped answering. lattice.json's port answers, as
        // another chain's node, so a read sent there would be seen.
        let down = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let pinned = Some(down);
        let unpinned: u16 = answering(r#"{"chain":["Nexus","unpinned"]}"#)
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/api")
            .parse()
            .unwrap();
        let hosting = |chains: Value| {
            let topology = json!({ "rpc": unpinned, "hostedChains": chains });
            fs::write(host.join("lattice.json"), topology.to_string()).unwrap();
        };
        let signet = |node: &str| Settings {
            network: Network::Signet,
            relay_directory: BTreeMap::from([(
                Network::Signet,
                Directory { node: node.into(), chain: SIGNET_RELAY_DIRECTORY.into() },
            )]),
            ..Settings::default()
        };
        // A node that answers for another chain: its refusal shows it was asked.
        let theirs = answering(r#"{"chain":["Nexus","other"]}"#);

        // Neither this app's node hosts the chain nor is one set: none read.
        hosting(json!(["Nexus/testnet"]));
        assert!(directory_nodes(&signet(""), &host, pinned).unwrap().is_none());
        assert_eq!(read_directory(&signet(""), &host, pinned).unwrap(), Vec::<String>::new());
        // No directory on mainnet.
        assert!(directory_nodes(&Settings::default(), &host, pinned).unwrap().is_none());
        // The settings' node alone.
        assert_eq!(directory_nodes(&signet(&theirs), &host, pinned).unwrap().unwrap().0.len(), 1);
        let only = read_directory(&signet(&theirs), &host, pinned).unwrap_err();
        assert!(only.contains("another chain"), "{only}");

        // This app's node hosts it: read there first, and on its failure on
        // the settings' node.
        hosting(json!(["Nexus/testnet", SIGNET_RELAY_DIRECTORY]));
        // Unless it runs: no port pinned, lattice.json's port is never read
        // with its cookie.
        assert!(directory_nodes(&signet(""), &host, None).unwrap().is_none());
        assert_eq!(directory_nodes(&signet(&theirs), &host, None).unwrap().unwrap().0.len(), 1);
        let unpinned = read_directory(&signet(&theirs), &host, None).unwrap_err();
        assert!(unpinned.contains("another chain") && !unpinned.contains("unpinned"), "{unpinned}");
        assert_eq!(directory_nodes(&signet(""), &host, pinned).unwrap().unwrap().0.len(), 1);
        let own = read_directory(&signet(""), &host, pinned).unwrap_err();
        assert!(!own.contains("another chain"), "{own}");
        assert_eq!(directory_nodes(&signet(&theirs), &host, pinned).unwrap().unwrap().0.len(), 2);
        let both = read_directory(&signet(&theirs), &host, pinned).unwrap_err();
        assert!(both.starts_with(&own) && both.contains("another chain"), "{both}");
        fs::remove_dir_all(host).unwrap();
    }

    #[test]
    fn offers_are_read_only_from_the_settings_relays_and_those_the_directory_found() {
        let mine = Settings {
            relays: BTreeMap::from([(Network::Bitcoin, vec!["https://mine.example".into()])]),
            ..Settings::default()
        };
        let found = ["https://found.example".to_string()];
        assert_eq!(board_cap(&mine, &found, "https://mine.example").unwrap(), MAX_OWN_BOARD);
        assert_eq!(board_cap(&mine, &found, "https://found.example").unwrap(), MAX_FOUND_BOARD);
        assert!(board_cap(&mine, &found, "https://other.example").unwrap_err().starts_with("refused:"));
        // The default relay is the settings' only where they name none.
        assert!(board_cap(&mine, &[], DEFAULT_RELAY).is_err());
        assert_eq!(board_cap(&Settings::default(), &[], DEFAULT_RELAY).unwrap(), MAX_OWN_BOARD);
        // Another network's relays are not this one's.
        let signet = Settings { network: Network::Signet, ..mine };
        assert!(board_cap(&signet, &[], "https://mine.example").is_err());
    }

    #[test]
    fn the_lock_fee_is_priced_for_an_empty_wallet_at_the_wallets_rate() {
        // One input, the lock and change: 153 vB, at the estimate rounded
        // up, and at 1 sat/vB with none.
        let estimates = answering(r#"{"1": 9.0, "2": 3.2, "6": 1.5}"#);
        assert_eq!(lock_fee(&estimates, Network::Signet, 50_000).unwrap(), 153 * 4);
        let none = answering("{}");
        assert_eq!(lock_fee(&none, Network::Signet, 50_000).unwrap(), 153);
        // As the engine's own need prices a wallet with nothing to spend.
        let (_, need) = swap::wallet::need(vec![], 50_000, 4);
        assert_eq!(need - 50_000, 153 * 4);
    }

    #[test]
    fn the_esplora_is_public_by_default_and_ones_own_only_on_its_network() {
        let root = temp("esplora");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        assert_eq!(load_settings(&root).unwrap().own(), None);
        let mainnet = esplora_on(Network::Bitcoin);
        let signet = esplora_on(Network::Signet);
        let set = |network, url: Option<&str>| {
            set_servers_with(
                &engine,
                &saving,
                &root,
                network,
                (url.map(String::from), NexusChoice::Public),
                public,
            )
        };

        // Your own, on the network swaps run on: saved, its trailing slash
        // dropped.
        set(Network::Bitcoin, Some(&format!(" {mainnet}/ "))).unwrap();
        assert_eq!(load_settings(&root).unwrap().own(), Some(&mainnet));
        assert_eq!(load_settings(&root).unwrap().esplora().unwrap(), mainnet);

        // Refused, nothing written: one on another network, one that cannot
        // be read, no URL, one plain http across the internet, and one for
        // a network swaps do not run on now.
        let refused = set(Network::Bitcoin, Some(&signet)).unwrap_err();
        assert!(refused.contains("genesis"), "{refused}");
        assert!(set(Network::Bitcoin, Some("http://127.0.0.1:9")).is_err());
        assert!(set(Network::Bitcoin, Some("  ")).is_err());
        assert!(set(Network::Bitcoin, Some("http://lying.example/api")).is_err());
        let refused = set(Network::Signet, Some(&signet)).unwrap_err();
        assert!(refused.contains("now run on bitcoin"), "{refused}");
        assert_eq!(
            load_settings(&root).unwrap().esplora,
            BTreeMap::from([(Network::Bitcoin, mainnet.clone())])
        );

        // Each network its own: signet reads mempool.space's until its own
        // is saved, mainnet's kept for mainnet.
        set_network_with(&engine, &saving, &root, Network::Signet).unwrap();
        assert_eq!(
            load_settings(&root).unwrap().esplora().unwrap(),
            "https://mempool.space/signet/api"
        );
        set(Network::Signet, Some(&signet)).unwrap();
        assert_eq!(load_settings(&root).unwrap().esplora().unwrap(), signet);
        set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap();
        assert_eq!(load_settings(&root).unwrap().esplora().unwrap(), mainnet);

        // A swap running, its wallet closed: the public one is chosen
        // again, still checked, and the swap resumes on it.
        let wallet = swap_dir(&root, Network::Bitcoin).unwrap();
        let mainnet_wallet = Config {
            network: Network::Bitcoin,
            ..offline(&wallet)
        };
        drop(Wallet::open(mainnet_wallet).unwrap());
        let running = "66".repeat(32);
        record(&wallet, &running, true, json!({ "Locking": {} }));
        assert!(set(Network::Bitcoin, Some(&signet)).is_err(), "still checked");
        set(Network::Bitcoin, None).unwrap();
        assert_eq!(load_settings(&root).unwrap().own(), None);
        assert_eq!(
            load_settings(&root).unwrap().esplora,
            BTreeMap::from([(Network::Signet, signet)])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_esplora_saved_before_is_the_networks_it_was_saved_for() {
        let root = temp("migrate");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        fs::create_dir_all(root.join("swap")).unwrap();
        // Saved before there was one per network, and since gone: nothing
        // answers there.
        let gone = "http://127.0.0.1:9/api";
        let before = json!({ "network": "signet", "esplora": gone, "relay": null });
        fs::write(settings_path(&root), before.to_string()).unwrap();
        let settings = load_settings(&root).unwrap();
        assert_eq!(settings.esplora, BTreeMap::from([(Network::Signet, gone.to_string())]));
        assert_eq!(settings.esplora().unwrap(), gone);
        let null = json!({ "network": "bitcoin", "esplora": null });
        assert_eq!(serde_json::from_value::<Settings>(null).unwrap(), Settings::default());

        // Mainnet reads mempool.space's; signet keeps its own, chosen again
        // without reading it, so another can be chosen in its place.
        set_network_with(&engine, &saving, &root, Network::Bitcoin).unwrap();
        assert_eq!(
            load_settings(&root).unwrap().esplora().unwrap(),
            "https://mempool.space/api"
        );
        set_network_with(&engine, &saving, &root, Network::Signet).unwrap();
        assert_eq!(load_settings(&root).unwrap().esplora().unwrap(), gone);
        let saved: Value = serde_json::from_slice(&fs::read(settings_path(&root)).unwrap()).unwrap();
        assert_eq!(saved["esplora"], json!({ "signet": gone }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saves_are_one_at_a_time_and_none_is_written_on_settings_it_did_not_read() {
        let root = temp("saves");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        let (slow, asked) = slow_esplora_on(Network::Bitcoin, Duration::from_millis(500));

        // Your own mainnet Esplora, slow to answer, and signet chosen while
        // it is read: the network waits for it, and both are kept.
        std::thread::scope(|scope| {
            let own = scope.spawn(|| {
                set_servers_with(
                    &engine,
                    &saving,
                    &root,
                    Network::Bitcoin,
                    (Some(slow.clone()), NexusChoice::Public),
                    public,
                )
            });
            assert!(wait(|| asked.load(Ordering::SeqCst), Duration::from_secs(10)));
            set_network_with(&engine, &saving, &root, Network::Signet).unwrap();
            own.join().unwrap().unwrap();
        });
        let settings = load_settings(&root).unwrap();
        assert_eq!(settings.network, Network::Signet);
        assert_eq!(settings.esplora, BTreeMap::from([(Network::Bitcoin, slow.clone())]));

        // One chosen for mainnet once swaps run on signet: refused, not
        // written over signet's.
        let refused = set_servers_with(
            &engine,
            &saving,
            &root,
            Network::Bitcoin,
            (None, NexusChoice::Public),
            public,
        )
        .unwrap_err();
        assert!(refused.contains("now run on signet"), "{refused}");
        assert_eq!(load_settings(&root).unwrap(), settings);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_swap_starts_only_on_the_settings_saved_and_a_running_one_is_kept() {
        let dir = temp("start-moved");
        let other = temp("start-moved-other");
        let (emit, _) = collect();
        let offer = offer(100_000, Network::Signet, swap_nexus_tx::MAINNET_GENESIS, 1).to_string();
        let nex = swap_nexus_tx::MAINNET_GENESIS;
        let engine = Mutex::new(Engine::default());

        // Saved anew while its wallet opened: refused, none started.
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let moved = || {
            let n = reads.fetch_add(1, Ordering::SeqCst);
            Ok(offline(if n == 0 { &dir } else { &other }))
        };
        let refused = start_with(&engine, moved, moved, &offer, (50_000, 100_000), nex, &emit).unwrap_err();
        assert!(refused.contains("settings changed"), "{refused}");
        assert_eq!(followed(&engine), None);

        // A swap running: a wallet for another network's directory is not
        // opened over it; another server is, the swap resumed on it.
        let waiting = "dd".repeat(32);
        record(&dir, &waiting, false, json!({ "Start": {} }));
        let wallet = open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        let run = Run::spawn(
            wallet.resume(&waiting).unwrap(),
            Duration::from_millis(100),
            emit.clone(),
        )
        .unwrap();
        lock(&engine).unwrap().followed = Some(run);
        drop(wallet);
        let elsewhere = Config {
            esplora: "http://127.0.0.1:10".into(),
            ..offline(&dir)
        };
        let public = Config {
            nexus: PUBLIC_NEXUS.into(),
            nexus_cookie: None,
            ..offline(&dir)
        };
        let refused = open_with(&engine, || Ok(offline(&other)), &emit).err().unwrap();
        assert!(refused.contains(&waiting), "{refused}");
        assert_eq!(lock(&engine).unwrap().wallet.as_ref().unwrap().config().dir, dir);
        for config in [elsewhere, public] {
            let moved = open_with(&engine, || Ok(config.clone()), &emit).unwrap();
            assert_eq!((moved.config().esplora.clone(), moved.config().nexus.clone()), (config.esplora, config.nexus));
            assert_eq!(lock(&engine).unwrap().running(), Some(waiting.as_str()), "kept running");
        }
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn nex_is_read_publicly_by_default_and_a_new_swap_on_ones_own_node_only_on_the_pinned_port() {
        let root = temp("config");
        let host = root.join("host");
        // Public: no node needed, and no cookie sent.
        let public = Settings::default();
        for pinned in [None, Some(18080)] {
            let config = config_at(&root, &host, pinned, &public).unwrap();
            assert_eq!(config.nexus, PUBLIC_NEXUS);
            assert_eq!(config.nexus_cookie, None);
        }
        // Your node, on this network only.
        let own = Settings {
            own_nexus: BTreeSet::from([Network::Bitcoin]),
            ..Settings::default()
        };
        let config = config_at(&root, &host, Some(18555), &own).unwrap();
        assert_eq!(config.nexus, "http://127.0.0.1:18555");
        assert_eq!(config.nexus_cookie, Some(host.join("chains/Nexus/.cookie")));
        assert_eq!(config.dir, root.join("swap").join("bitcoin"));
        assert_eq!(start_config_at(&root, &host, Some(18555), &own).unwrap().nexus, config.nexus);
        // None pinned (the node is down): the wallet opens on the port the
        // node will run on, lattice.json's or the app's default, but no new
        // swap starts.
        assert_eq!(config_at(&root, &host, None, &own).unwrap().nexus, "http://127.0.0.1:18080");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lattice.json"), r#"{"rpc": 18666}"#).unwrap();
        let down = config_at(&root, &host, None, &own).unwrap();
        assert_eq!(down.nexus, "http://127.0.0.1:18666");
        assert_eq!(down.nexus_cookie, config.nexus_cookie);
        assert_eq!(start_config_at(&root, &host, None, &own).unwrap_err(), NODE_DOWN);
        let signet = Settings {
            network: Network::Signet,
            ..own
        };
        assert_eq!(config_at(&root, &host, None, &signet).unwrap().nexus, PUBLIC_NEXUS);
        fs::remove_dir_all(root).unwrap();
    }

    /// A public Nexus read port on loopback, on the Nexus of `genesis`,
    /// taking transactions if `accepts`: its URL.
    fn nexus_on(genesis: &str, accepts: bool) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let genesis = genesis.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let n = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let body = if request.starts_with("GET /api/chain/info ") {
                    json!({
                        "chain": ["Nexus"], "genesisHash": genesis,
                        "height": "1", "tipCID": "tip", "acceptsSubmit": accepts,
                    })
                } else {
                    json!({ "phase": "active", "height": "1", "tipCID": "tip" })
                }
                .to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    #[test]
    fn the_nexus_node_is_public_by_default_and_ones_own_by_choice() {
        let root = temp("nexus");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        let genesis = swap_nexus_tx::MAINNET_GENESIS;
        let good = nexus_on(genesis, true);
        let set = |network, own, public: &str| {
            set_servers_with(
                &engine,
                &saving,
                &root,
                network,
                (None, choice(own)),
                |_| check_nexus(public, None),
            )
        };
        let own = || load_settings(&root).unwrap().own_nexus;
        assert!(!load_settings(&root).unwrap().own_nexus());

        // Yours: saved, for this network only, with nothing to read.
        set(Network::Bitcoin, true, "http://127.0.0.1:9").unwrap();
        assert_eq!(own(), BTreeSet::from([Network::Bitcoin]));
        let saved: Value = serde_json::from_slice(&fs::read(settings_path(&root)).unwrap()).unwrap();
        assert_eq!(saved["ownNexus"], json!(["bitcoin"]));

        // The public one, refused, nothing written: one on another Nexus,
        // one that takes no transactions, one that cannot be read, and one
        // for a network swaps do not run on now.
        let refused = set(Network::Bitcoin, false, &nexus_on("bafyreianothergenesis", true)).unwrap_err();
        assert!(refused.contains("genesis"), "{refused}");
        let refused = set(Network::Bitcoin, false, &nexus_on(genesis, false)).unwrap_err();
        assert!(refused.contains("takes no transactions"), "{refused}");
        assert!(set(Network::Bitcoin, false, "http://127.0.0.1:9").is_err());
        let refused = set(Network::Signet, true, &good).unwrap_err();
        assert!(refused.contains("now run on bitcoin"), "{refused}");
        assert_eq!(own(), BTreeSet::from([Network::Bitcoin]));

        // A swap running on your node: the public one is chosen, once it
        // passes, and the swap goes on, then moves to it when the wallet is
        // opened on the settings saved.
        let wallet = swap_dir(&root, Network::Bitcoin).unwrap();
        let on_own = Config {
            network: Network::Bitcoin,
            ..offline(&wallet)
        };
        let running = "67".repeat(32);
        record(&wallet, &running, true, json!({ "Locking": {} }));
        let (emit, _) = collect();
        open_with(&engine, || Ok(on_own.clone()), &emit).unwrap();
        assert!(set(Network::Bitcoin, false, &nexus_on(genesis, false)).is_err(), "still checked");
        set(Network::Bitcoin, false, &good).unwrap();
        assert_eq!(own(), BTreeSet::new());
        assert_eq!(lock(&engine).unwrap().running(), Some(running.as_str()), "it goes on");
        let on_public = Config {
            network: Network::Bitcoin,
            nexus: good.clone(),
            nexus_cookie: None,
            ..offline(&wallet)
        };
        let moved = open_with(&engine, || Ok(on_public.clone()), &emit).unwrap();
        assert_eq!((moved.config().nexus.as_str(), moved.config().nexus_cookie.as_ref()), (good.as_str(), None));
        assert_eq!(lock(&engine).unwrap().running(), Some(running.as_str()), "resumed on it");
        drop(moved);
        // Not its network, though.
        let refused = set_network_with(&engine, &saving, &root, Network::Signet).unwrap_err();
        assert!(refused.contains(&running), "{refused}");
        close(&engine);
        record(&wallet, &running, true, json!("Done"));
        let saved: Value = serde_json::from_slice(&fs::read(settings_path(&root)).unwrap()).unwrap();
        assert_eq!(saved.get("ownNexus"), None, "left out, as a build before it wrote");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_taken_swap_is_driven_on_ones_own_node_while_it_is_down_and_moves_to_the_servers_saved() {
        let root = temp("own-down");
        let host = root.join("host");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        let (emit, events) = collect();
        let offer = offer(100_000, Network::Bitcoin, swap_nexus_tx::MAINNET_GENESIS, 1).to_string();
        let nex = swap_nexus_tx::MAINNET_GENESIS;
        let esplora = esplora_on(Network::Bitcoin);
        let set = |esplora: &str, own, public: &str| {
            set_servers_with(
                &engine,
                &saving,
                &root,
                Network::Bitcoin,
                (Some(esplora.into()), choice(own)),
                |_| check_nexus(public, None),
            )
        };
        let unpinned = || Ok(None);
        // Where the node runs when it is up; nothing listens there now.
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lattice.json"), r#"{"rpc": 9}"#).unwrap();

        // Your node saved, and down: no new swap starts.
        set(&esplora, true, "http://127.0.0.1:9").unwrap();
        let start = || start_config_at(&root, &host, None, &load_settings(&root)?);
        let resume = || config_at(&root, &host, None, &load_settings(&root)?);
        let refused = start_with(&engine, start, resume, &offer, (50_000, 100_000), nex, &emit).unwrap_err();
        assert_eq!(refused, NODE_DOWN);
        assert!(lock(&engine).unwrap().wallet.is_none());

        // A taken swap on disk, the node still down: the wallet opens on
        // it and the swap is driven.
        let wallet = swap_dir(&root, Network::Bitcoin).unwrap();
        drop(Wallet::open(config_at(&root, &host, None, &load_settings(&root).unwrap()).unwrap()).unwrap());
        let running = "77".repeat(32);
        record(&wallet, &running, true, json!({ "Locking": {} }));
        sync_at(&engine, &root, &host, unpinned, &emit).unwrap();
        let open = lock(&engine).unwrap().wallet().unwrap();
        assert_eq!(open.config().nexus, "http://127.0.0.1:9");
        assert_eq!(open.config().esplora, esplora);
        drop(open);
        assert_eq!(followed(&engine), Some(running.clone()));
        wait_for(&events, "swap-error", |e| e["swapId"] == running.as_str());
        assert!(lock(&engine).unwrap().followed.as_ref().unwrap().active());

        // Both servers saved in one go, the swap running: it goes on, on
        // them, once synced; a sync again leaves it be.
        let other = esplora_on(Network::Bitcoin);
        set(&other, false, &nexus_on(nex, true)).unwrap();
        sync_at(&engine, &root, &host, unpinned, &emit).unwrap();
        let moved = lock(&engine).unwrap().wallet().unwrap();
        assert_eq!(
            (moved.config().esplora.as_str(), moved.config().nexus.as_str(), moved.config().nexus_cookie.as_ref()),
            (other.as_str(), PUBLIC_NEXUS, None)
        );
        sync_at(&engine, &root, &host, unpinned, &emit).unwrap();
        assert!(Arc::ptr_eq(&moved, &lock(&engine).unwrap().wallet().unwrap()), "the same wallet");
        drop(moved);
        assert_eq!(lock(&engine).unwrap().running(), Some(running.as_str()), "resumed on them");

        // Either refused: neither written.
        let before = load_settings(&root).unwrap();
        let refused = set(&esplora_on(Network::Signet), true, "http://127.0.0.1:9").unwrap_err();
        assert!(refused.contains("genesis"), "{refused}");
        assert_eq!(load_settings(&root).unwrap(), before);
        set(&other, true, "http://127.0.0.1:9").unwrap();
        let before = load_settings(&root).unwrap();
        let refused = set(&esplora, false, &nexus_on(nex, false)).unwrap_err();
        assert!(refused.contains("takes no transactions"), "{refused}");
        assert_eq!(load_settings(&root).unwrap(), before);
        close(&engine);
        record(&wallet, &running, true, json!("Done"));
        fs::remove_dir_all(root).unwrap();
    }

    /// Your own node, as the tests name it: https, read at a loopback node.
    const REMOTE: &str = "https://node.example";

    #[test]
    fn your_own_nexus_node_by_url_is_https_checked_and_normalized() {
        let root = temp("remote");
        let host = root.join("host");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        let genesis = swap_nexus_tx::MAINNET_GENESIS;
        let good = nexus_on(genesis, true);
        // Every URL is read at `node`.
        let set = |nexus: NexusChoice, node: &str| {
            set_servers_with(
                &engine,
                &saving,
                &root,
                Network::Bitcoin,
                (None, nexus),
                |_| check_nexus(node, None),
            )
        };
        let remote = |url: &str| NexusChoice::Remote(url.into());

        // Https, checked, normalized: its scheme and host lowercased, no
        // trailing slash.
        set(remote(" HTTPS://Node.Example/ "), &good).unwrap();
        let settings = load_settings(&root).unwrap();
        assert_eq!(
            settings.nexus,
            BTreeMap::from([(Network::Bitcoin, REMOTE.to_string())])
        );
        let saved: Value =
            serde_json::from_slice(&fs::read(settings_path(&root)).unwrap()).unwrap();
        assert_eq!(saved["nexus"], json!({ "bitcoin": REMOTE }));
        // Read as a public read port, with no cookie, whether or not this
        // app's node runs: a new swap is not held for it.
        for pinned in [None, Some(18555)] {
            let config = config_at(&root, &host, pinned, &settings).unwrap();
            assert_eq!((config.nexus.as_str(), config.nexus_cookie), (REMOTE, None));
            assert_eq!(
                start_config_at(&root, &host, pinned, &settings)
                    .unwrap()
                    .nexus,
                REMOTE
            );
        }

        // Refused, nothing written: not https, refused before it is read,
        // even to this machine; one on another Nexus; one that takes no
        // transactions; one that cannot be read.
        let unread = |url: &str| {
            set_servers_with(
                &engine,
                &saving,
                &root,
                Network::Bitcoin,
                (None, remote(url)),
                |_| panic!("read"),
            )
        };
        for url in [
            "http://127.0.0.1:9",
            "http://node.example",
            "node.example",
            "",
        ] {
            assert_eq!(unread(url).unwrap_err(), "refused: must be an https URL");
        }
        assert!(unread("https://user:pass@node.example").is_err());
        let elsewhere = "https://elsewhere.example";
        let refused = set(remote(elsewhere), &nexus_on("bafyreianothergenesis", true)).unwrap_err();
        assert!(
            refused.contains("genesis bafyreianothergenesis"),
            "{refused}"
        );
        let refused = set(remote(elsewhere), &nexus_on(genesis, false)).unwrap_err();
        assert!(refused.contains("takes no transactions"), "{refused}");
        assert!(set(remote(elsewhere), "http://127.0.0.1:9").is_err());
        assert_eq!(load_settings(&root).unwrap(), settings);

        // Saved again as it is, another Esplora with it: not read again.
        let esplora = esplora_on(Network::Bitcoin);
        let kept = (Some(esplora.clone()), remote(&format!("{REMOTE}/")));
        set_servers_with(&engine, &saving, &root, Network::Bitcoin, kept, |_| {
            panic!("read")
        })
        .unwrap();
        assert_eq!(
            load_settings(&root).unwrap().remote_nexus(),
            Some(&REMOTE.to_string())
        );

        // The public node's own URL is the public node; this app's node or
        // yours, one at a time.
        set(remote(&format!("{PUBLIC_NEXUS}/")), &good).unwrap();
        assert!(load_settings(&root).unwrap().nexus.is_empty());
        set(remote(REMOTE), &good).unwrap();
        set(NexusChoice::Own, "http://127.0.0.1:9").unwrap();
        let mine = load_settings(&root).unwrap();
        assert!(mine.own_nexus() && mine.nexus.is_empty());
        set(remote(REMOTE), &good).unwrap();
        let theirs = load_settings(&root).unwrap();
        assert!(!theirs.own_nexus() && theirs.remote_nexus().is_some());

        // A swap running: another node on its network is saved, checked,
        // and the swap goes on; its network is not left.
        let wallet = swap_dir(&root, Network::Bitcoin).unwrap();
        let running = "68".repeat(32);
        record(&wallet, &running, true, json!({ "Locking": {} }));
        let (emit, _) = collect();
        let on_wallet = Config {
            network: Network::Bitcoin,
            ..offline(&wallet)
        };
        open_with(&engine, || Ok(on_wallet.clone()), &emit).unwrap();
        assert!(set(remote(elsewhere), &nexus_on("bafyreianothergenesis", true)).is_err());
        set(remote(elsewhere), &good).unwrap();
        assert_eq!(
            lock(&engine).unwrap().running(),
            Some(running.as_str()),
            "it goes on"
        );
        let refused = set_network_with(&engine, &saving, &root, Network::Signet).unwrap_err();
        assert!(refused.contains(&running), "{refused}");
        close(&engine);
        record(&wallet, &running, true, json!("Done"));

        // A file naming one by plain http, or both nodes on one network, is
        // refused as an operator's typo is.
        let file = |settings: Value| {
            fs::write(settings_path(&root), settings.to_string()).unwrap();
            load_settings(&root)
        };
        assert!(file(
            json!({ "network": "bitcoin", "nexus": { "bitcoin": "http://node.example" } })
        )
        .is_err());
        let both = json!({ "network": "bitcoin", "ownNexus": ["bitcoin"], "nexus": { "bitcoin": REMOTE } });
        assert!(file(both).is_err());
        assert!(file(json!({ "network": "bitcoin", "nexus": { "mainnet": REMOTE } })).is_err());
        // One not as it is saved: the engine would read it as it is.
        for url in [" https://node.example", "https://Node.example/"] {
            let refused =
                file(json!({ "network": "bitcoin", "nexus": { "bitcoin": url } })).unwrap_err();
            assert!(
                refused.contains("not saved as https://node.example"),
                "{refused}"
            );
        }
        assert!(file(json!({ "network": "bitcoin", "nexus": { "bitcoin": REMOTE } })).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn on_your_own_node_by_url_down_a_taken_swap_is_driven_and_no_new_swap_starts() {
        let root = temp("remote-down");
        let host = root.join("host");
        let engine = Mutex::new(Engine::default());
        let saving = Mutex::new(());
        let (emit, events) = collect();
        let nex = swap_nexus_tx::MAINNET_GENESIS;
        let offer = offer(100_000, Network::Signet, nex, 1).to_string();
        // Your node by URL saved, as the page saves it (its check passed
        // then), and down now: nothing answers there.
        let down = "https://127.0.0.1:9";
        set_network_with(&engine, &saving, &root, Network::Signet).unwrap();
        let servers = (
            Some(esplora_on(Network::Signet)),
            NexusChoice::Remote(down.into()),
        );
        set_servers_with(
            &engine,
            &saving,
            &root,
            Network::Signet,
            servers,
            |_| Ok(()),
        )
        .unwrap();
        let config = config_at(&root, &host, None, &load_settings(&root).unwrap()).unwrap();
        assert_eq!((config.nexus.as_str(), config.nexus_cookie), (down, None));

        // No new swap starts on it.
        let start = || start_config_at(&root, &host, None, &load_settings(&root)?);
        let resume = || config_at(&root, &host, None, &load_settings(&root)?);
        let refused = start_with(
            &engine,
            start,
            resume,
            &offer,
            (50_000, 100_000),
            nex,
            &emit,
        )
        .unwrap_err();
        assert!(refused.contains("127.0.0.1:9"), "{refused}");
        assert_eq!(followed(&engine), None);
        let wallet = swap_dir(&root, Network::Signet).unwrap();
        assert!(
            lock(&engine)
                .unwrap()
                .wallet()
                .unwrap()
                .swaps()
                .unwrap()
                .is_empty(),
            "none prepared"
        );
        close(&engine);

        // A taken swap: the wallet opens on it, as at launch, and the swap
        // is driven.
        let running = "79".repeat(32);
        record(&wallet, &running, true, json!({ "Locking": {} }));
        sync_at(&engine, &root, &host, || Ok(None), &emit).unwrap();
        assert_eq!(
            lock(&engine).unwrap().wallet().unwrap().config().nexus,
            down
        );
        assert_eq!(followed(&engine), Some(running.clone()));
        wait_for(&events, "swap-error", |e| e["swapId"] == running.as_str());
        close(&engine);
        record(&wallet, &running, true, json!("Done"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_saved_before_a_nexus_choice_read_the_public_one() {
        let before: Settings = serde_json::from_value(
            json!({ "network": "signet", "esplora": { "signet": "https://signet.example/api" }, "relay": null }),
        )
        .unwrap();
        assert!(before.own_nexus.is_empty());
        assert!(!before.own_nexus());
        assert!(serde_json::from_value::<Settings>(json!({ "network": "bitcoin", "ownNexus": ["mainnet"] })).is_err());
    }

    #[test]
    fn a_node_on_a_new_port_reopens_the_wallet_and_its_swaps_go_on() {
        let dir = temp("repin");
        let running = "66".repeat(32);
        record(&dir, &running, true, json!({ "Locking": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let first = open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap();
        let again = open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "the same node: the same wallet"
        );
        drop((first, again));
        let moved = open_with(&engine, || Ok(offline_at(&dir, 10)), &emit).unwrap();
        assert_eq!(moved.config().nexus, "http://127.0.0.1:10");
        assert_eq!(followed(&engine), Some(running), "resumed on the new port");
        drop(moved);
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopen_waits_for_a_read_holding_the_wallet() {
        let dir = temp("held");
        let running = "88".repeat(32);
        record(&dir, &running, true, json!({ "Locking": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let reader = open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap();
        // A balance read still holds the wallet when the node moves.
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(reader);
        });
        let moved = open_patiently(
            &engine,
            || Ok(offline_at(&dir, 10)),
            || Ok(offline_at(&dir, 10)),
            &emit,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(moved.config().nexus, "http://127.0.0.1:10");
        assert_eq!(
            followed(&engine),
            Some(running),
            "the taken swap is driven again"
        );
        releasing.join().unwrap();
        drop(moved);
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopen_keeps_a_swap_waiting_for_its_funds() {
        let dir = temp("waiting");
        let waiting = "99".repeat(32);
        record(&dir, &waiting, false, json!({ "Start": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let wallet = open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap();
        let run = Run::spawn(
            wallet.resume(&waiting).unwrap(),
            Duration::from_millis(100),
            emit.clone(),
        )
        .unwrap();
        lock(&engine).unwrap().followed = Some(run);
        drop(wallet);
        drop(open_with(&engine, || Ok(offline_at(&dir, 10)), &emit).unwrap());
        assert_eq!(followed(&engine), Some(waiting));
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopen_ends_on_the_config_read_last() {
        let dir = temp("read-last");
        let waiting = "9a".repeat(32);
        record(&dir, &waiting, false, json!({ "Start": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let wallet = open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap();
        let run = Run::spawn(wallet.resume(&waiting).unwrap(), Duration::from_millis(100), emit.clone())
            .unwrap();
        lock(&engine).unwrap().followed = Some(run);
        drop(wallet);
        // Saved anew (another port pinned) while the wallet was taken out.
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let moving = || {
            let n = reads.fetch_add(1, Ordering::SeqCst);
            Ok(offline_at(&dir, if n == 0 { 10 } else { 11 }))
        };
        let moved = open_with(&engine, moving, &emit).unwrap();
        assert!(reads.load(Ordering::SeqCst) > 1, "the loop ran again");
        assert_eq!(moved.config().nexus, "http://127.0.0.1:11");
        assert_eq!(followed(&engine), Some(waiting), "still followed");
        drop(moved);
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopen_that_fails_reopens_on_the_resume_config() {
        let dir = temp("reopen-fails");
        let waiting = "9b".repeat(32);
        record(&dir, &waiting, false, json!({ "Start": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let wallet = open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap();
        let run = Run::spawn(wallet.resume(&waiting).unwrap(), Duration::from_millis(100), emit.clone())
            .unwrap();
        lock(&engine).unwrap().followed = Some(run);
        drop(wallet);
        // The node went down while the wallet was taken out.
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let failing = || match reads.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(offline_at(&dir, 10)),
            _ => Err(NODE_DOWN.to_string()),
        };
        let refused = open_patiently(&engine, failing, || Ok(offline_at(&dir, 9)), &emit, OPEN_PATIENCE)
            .err()
            .unwrap();
        assert_eq!(refused, NODE_DOWN);
        let open = lock(&engine).unwrap().wallet().unwrap();
        assert_eq!(open.config().nexus, "http://127.0.0.1:9", "open on the resume config");
        drop(open);
        assert_eq!(followed(&engine), Some(waiting), "still followed");
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_resume_leaves_no_wallet_open() {
        let dir = temp("resume-fails");
        fs::write(
            dir.join(format!("{}.record", "77".repeat(32))),
            "not a record",
        )
        .unwrap();
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        assert!(open_with(&engine, || Ok(offline(&dir)), &emit).is_err());
        assert!(lock(&engine).unwrap().wallet.is_none());
        // Nor the directory held.
        drop(Wallet::open(offline(&dir)).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_swap_at_a_time() {
        let dir = temp("one");
        let (emit, _) = collect();
        let offer = offer(100_000, Network::Signet, swap_nexus_tx::MAINNET_GENESIS, 1).to_string();
        let nex = swap_nexus_tx::MAINNET_GENESIS;
        let engine = Mutex::new(Engine::default());

        // One the page follows, waiting for its funds: a second is refused.
        let waiting = "aa".repeat(32);
        record(&dir, &waiting, false, json!({ "Start": {} }));
        let wallet = open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        assert_eq!(
            followed(&engine),
            None,
            "a swap waiting for funds is not resumed"
        );
        let run = Run::spawn(
            wallet.resume(&waiting).unwrap(),
            Duration::from_millis(100),
            emit.clone(),
        )
        .unwrap();
        lock(&engine).unwrap().followed = Some(run);
        drop(wallet);
        let refused = start_with(
            &engine,
            || Ok(offline(&dir)),
            || Ok(offline(&dir)),
            &offer,
            (50_000, 100_000),
            nex,
            &emit,
        )
        .unwrap_err();
        assert!(
            refused.contains(&format!(
                "swap {waiting} is in progress; one swap at a time"
            )),
            "{refused}"
        );
        assert!(
            dir.join(format!("{waiting}.record")).exists(),
            "the swap followed is kept"
        );
        close(&engine);
        fs::remove_file(dir.join(format!("{waiting}.record"))).unwrap();

        // A punished one asking for `s`: once it has it, it would run
        // beside a new one.
        let punished = "cc".repeat(32);
        record(&dir, &punished, true, json!({ "Punished": {} }));
        open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        assert_eq!(followed(&engine), None);
        let refused = start_with(
            &engine,
            || Ok(offline(&dir)),
            || Ok(offline(&dir)),
            &offer,
            (50_000, 100_000),
            nex,
            &emit,
        )
        .unwrap_err();
        assert!(
            refused.contains(&format!("swap {punished} is in progress")),
            "{refused}"
        );
        close(&engine);
        fs::remove_file(dir.join(format!("{punished}.record"))).unwrap();

        // One that has taken its offer: the wallet refuses too.
        let taken = "bb".repeat(32);
        record(&dir, &taken, true, json!({ "Locking": {} }));
        let wallet = Wallet::open(offline(&dir)).unwrap();
        let refused = wallet
            .prepare(&offer, policy(50_000, 100_000, Network::Bitcoin), nex)
            .err()
            .unwrap()
            .to_string();
        assert!(
            refused.contains(&format!("swap {taken} is running; one swap at a time")),
            "{refused}"
        );
        drop(wallet);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_swap_takes_only_its_amount_and_price_from_the_page() {
        let policy = policy(50_000, 100_000, Network::Bitcoin);
        assert_eq!(policy.as_object().unwrap().len(), 2);
        let parsed: Policy = serde_json::from_value(policy).unwrap();
        assert_eq!((parsed.btc_amount, parsed.price), (50_000, 100_000));
        assert_eq!(parsed.bounds, Bounds::DEFAULT);
    }

    #[test]
    fn a_signet_swap_opts_in_to_mainnet_nex_for_test_coins_and_a_bitcoin_one_never() {
        let policy = policy(50_000, 100_000, Network::Signet);
        assert_eq!(policy.as_object().unwrap().len(), 3);
        let parsed: Policy = serde_json::from_value(policy).unwrap();
        assert_eq!((parsed.btc_amount, parsed.price), (50_000, 100_000));
        let opted = Bounds {
            test_btc_for_mainnet_nex: true,
            ..Bounds::DEFAULT
        };
        assert_eq!(parsed.bounds, opted);
        // The engine takes mainnet NEX for signet BTC only so opted in.
        let genesis = swap_nexus_tx::MAINNET_GENESIS;
        assert!(swap::offer::check_networks(Network::Signet, genesis, true).is_ok());
        assert!(swap::offer::check_networks(Network::Signet, genesis, false).is_err());
    }

    #[test]
    fn a_failed_restore_leaves_the_wallet_as_it_was_and_can_be_tried_again() {
        let root = temp("restore");
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let (theirs, empty, other) = (root.join("theirs"), root.join("empty"), root.join("other"));
        let wallet = Wallet::open(offline(&theirs)).unwrap();
        let (address, file) = (wallet.address().to_string(), wallet.backup().unwrap().1);
        drop(wallet);

        // Into an empty directory: a bad file makes no key, and the good
        // one then restores.
        assert!(restore_with(&engine, offline(&empty), "not a recovery file", &emit).is_err());
        assert!(!empty.join("wallet.json").exists(), "no key made");
        assert!(lock(&engine).unwrap().wallet.is_none());
        assert_eq!(
            restore_with(&engine, offline(&empty), &file, &emit).unwrap(),
            address
        );
        assert_eq!(lock(&engine).unwrap().wallet().unwrap().address(), address);
        close(&engine);

        // Over another wallet: refused, and that wallet is open again.
        let kept = open_with(&engine, || Ok(offline(&other)), &emit)
            .unwrap()
            .address()
            .to_string();
        let refused = restore_with(&engine, offline(&other), &file, &emit).unwrap_err();
        assert!(refused.contains("another wallet"), "{refused}");
        assert_eq!(lock(&engine).unwrap().wallet().unwrap().address(), kept);
        close(&engine);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_restore_waits_for_a_read_holding_the_wallet() {
        let dir = temp("restore-held");
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let reader = open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        let (address, file) = (reader.address().to_string(), reader.backup().unwrap().1);
        // A balance read still holds the wallet when the restore begins.
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(reader);
        });
        assert_eq!(
            restore_with(&engine, offline(&dir), &file, &emit).unwrap(),
            address
        );
        releasing.join().unwrap();
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_recovery_file_is_read_from_its_path_up_to_the_cap() {
        let dir = temp("recovery-file");
        let path = dir.join("lattice-swap-x.json");
        fs::write(&path, "{\"format\":1}").unwrap();
        assert_eq!(read_recovery(&path).unwrap(), "{\"format\":1}");
        fs::write(&path, vec![b' '; MAX_BODY as usize]).unwrap();
        assert_eq!(read_recovery(&path).unwrap().len() as u64, MAX_BODY, "the cap itself is read");
        fs::write(&path, vec![b' '; MAX_BODY as usize + 1]).unwrap();
        assert_eq!(read_recovery(&path).unwrap_err(), "not a recovery file");
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(read_recovery(&path).unwrap_err(), "not a recovery file", "not text");
        // A FIFO is refused, not read: the read would wait for a writer.
        #[cfg(unix)]
        {
            let fifo = dir.join("fifo.json");
            assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
            assert_eq!(read_recovery(&fifo).unwrap_err(), "not a recovery file");
        }
        assert!(read_recovery(&dir).is_err(), "a directory");
        assert!(read_recovery(&dir.join("missing.json")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_engine_thread_survives_a_panic_and_stops_when_halted() {
        let (emit, events) = collect();
        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = rounds.clone();
        let threads = spawn_threads(
            "ee".into(),
            move |_| {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("round one");
                }
                Ok(())
            },
            || Some(json!({ "swapId": "ee" })),
            Duration::from_millis(20),
            emit.clone(),
        )
        .unwrap();
        threads.engine.join().unwrap();
        threads.reports.join().unwrap();
        assert_eq!(
            rounds.load(Ordering::SeqCst),
            2,
            "run again after the panic"
        );
        wait_for(&events, "swap-error", |e| {
            e["error"].as_str().unwrap().contains("panicked: round one")
        });
        wait_for(&events, "swap-status", |e| e["swapId"] == "ee");

        let failing = spawn_threads(
            "ff".into(),
            |_| Err("down".into()),
            || None,
            Duration::from_millis(20),
            emit,
        )
        .unwrap();
        wait_for(&events, "swap-error", |e| e["swapId"] == "ff");
        failing.halt.store(true, Ordering::SeqCst);
        failing.engine.join().unwrap();
        failing.reports.join().unwrap();
    }

    #[test]
    fn the_engine_reports_its_runs_busy_until_their_last_report() {
        let dir = temp("activity");
        let (running, punished) = ("12".repeat(32), "34".repeat(32));
        record(&dir, &running, true, json!({ "Locking": {} }));
        record(&dir, &punished, true, json!({ "Punished": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        assert!(!lock(&engine).unwrap().activity(None).busy, "nothing held");
        open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        let activity = lock(&engine).unwrap().activity(None);
        assert!(activity.busy);
        assert_eq!(activity.held, [running, punished.clone()], "the followed one first");
        assert_eq!(activity.punished, [punished]);
        // Halted, a run is busy until both its threads are done.
        {
            let held = lock(&engine).unwrap();
            for run in held.followed.iter().chain(&held.punished) {
                run.threads.halt.store(true, Ordering::SeqCst);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&engine).unwrap().activity(None).busy {
            assert!(Instant::now() < deadline, "still busy");
            std::thread::sleep(Duration::from_millis(20));
        }
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn before_the_wallet_opens_its_stored_swaps_count() {
        let dir = temp("stored");
        let at = || Some((dir.clone(), Network::Signet));
        let engine = Mutex::new(Engine::default());
        assert_eq!(lock(&engine).unwrap().activity(at()).stored, Some(false), "no wallet");
        drop(Wallet::open(offline(&dir)).unwrap());
        let ended = "56".repeat(32);
        record(&dir, &ended, true, json!("Done"));
        assert_eq!(lock(&engine).unwrap().activity(at()).stored, Some(false), "nothing to run");
        // Taken and not ended, before the node runs and the wallet opens.
        let running = "57".repeat(32);
        record(&dir, &running, true, json!({ "Locking": {} }));
        assert_eq!(lock(&engine).unwrap().activity(at()).stored, Some(true));
        fs::remove_file(dir.join(format!("{running}.record"))).unwrap();
        record(&dir, &"58".repeat(32), true, json!({ "Punished": {} }));
        assert_eq!(
            lock(&engine).unwrap().activity(at()).stored,
            Some(false),
            "a punished one alone does not hold the app"
        );
        // Open, its runs say it.
        let (emit, _) = collect();
        let wallet = open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        let activity = lock(&engine).unwrap().activity(at());
        assert_eq!((activity.stored, activity.busy), (Some(false), true));
        // Held elsewhere (a reopen's detached wallet): not known.
        close(&engine);
        assert_eq!(lock(&engine).unwrap().activity(at()).stored, None, "the wallet is still held");
        drop(wallet);
        // Mid-reopen, whatever the disk says: not known.
        lock(&engine).unwrap().reopening = true;
        assert_eq!(lock(&engine).unwrap().activity(at()).stored, None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopen_is_known_until_the_wallet_is_held_again() {
        let dir = temp("reopening");
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        drop(open_with(&engine, || Ok(offline_at(&dir, 9)), &emit).unwrap());
        drop(open_with(&engine, || Ok(offline_at(&dir, 10)), &emit).unwrap());
        assert!(!lock(&engine).unwrap().reopening, "held again");
        let file = lock(&engine).unwrap().wallet().unwrap().backup().unwrap().1;
        restore_with(&engine, offline_at(&dir, 10), &file, &emit).unwrap();
        assert!(!lock(&engine).unwrap().reopening, "restored");
        assert!(restore_with(&engine, offline_at(&dir, 10), "not a file", &emit).is_err());
        assert!(!lock(&engine).unwrap().reopening, "a failed restore too");
        close(&engine);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opening_resumes_taken_unfinished_and_punished_swaps() {
        let dir = temp("resume");
        let (running, punished, done, untaken) = (
            "11".repeat(32),
            "22".repeat(32),
            "33".repeat(32),
            "44".repeat(32),
        );
        record(&dir, &running, true, json!({ "Locking": {} }));
        record(&dir, &punished, true, json!({ "Punished": {} }));
        record(&dir, &done, true, json!("Done"));
        record(&dir, &untaken, false, json!({ "Start": {} }));
        let wallet = Wallet::open(offline(&dir)).unwrap();
        assert_eq!(
            to_resume(&wallet).unwrap(),
            (Some(running.clone()), vec![punished.clone()])
        );
        drop(wallet);

        let (emit, events) = collect();
        let engine = Mutex::new(Engine::default());
        open_with(&engine, || Ok(offline(&dir)), &emit).unwrap();
        assert_eq!(followed(&engine), Some(running.clone()));
        assert_eq!(
            lock(&engine)
                .unwrap()
                .punished
                .iter()
                .map(|run| run.swap_id.clone())
                .collect::<Vec<_>>(),
            vec![punished.clone()]
        );
        // These states are only names: each run fails, says so, and is
        // run again.
        wait_for(&events, "swap-error", |e| e["swapId"] == running.as_str());
        assert!(
            lock(&engine).unwrap().followed.as_ref().unwrap().active(),
            "a failed run is run again"
        );
        close(&engine);
        assert!(lock(&engine).unwrap().wallet.is_none());
        // Closed, the directory is free for another wallet.
        drop(Wallet::open(offline(&dir)).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn the_relay_board_lists_signed_offers_for_this_network_and_nexus_cheapest_first() {
        let genesis = swap_nexus_tx::MAINNET_GENESIS;
        let mut forged = offer(1_000, Network::Bitcoin, genesis, 6);
        forged["quote"]["price"] = json!(1);
        let board = json!([
            offer(300_000, Network::Bitcoin, genesis, 1),
            offer(200_000, Network::Bitcoin, genesis, 2),
            offer(100_000, Network::Signet, genesis, 3),
            offer(100_000, Network::Bitcoin, "bafyreianothergenesis", 4),
            offer(0, Network::Bitcoin, genesis, 5),
            forged,
            { "not": "an offer" },
        ]);
        let list = listed(&board.to_string(), Network::Bitcoin, genesis).unwrap();
        let prices: Vec<_> = list
            .iter()
            .map(|l| l.offer["quote"]["price"].as_u64().unwrap())
            .collect();
        assert_eq!(prices, [200_000, 300_000]);
        // Its text is the offer, as the engine takes it.
        assert_eq!(Offer::decode(&list[0].text).unwrap().quote.price, 200_000);
        // A number JavaScript would round is left out.
        let huge = json!([offer(u64::MAX, Network::Bitcoin, genesis, 7)]);
        assert!(listed(&huge.to_string(), Network::Bitcoin, genesis)
            .unwrap()
            .is_empty());
        assert!(listed("<html>", Network::Bitcoin, genesis).is_err());
        // Only the first MAX_OFFERS are read.
        let mut long = vec![json!({ "not": "an offer" }); MAX_OFFERS];
        long.push(offer(100_000, Network::Bitcoin, genesis, 8));
        assert!(listed(&json!(long).to_string(), Network::Bitcoin, genesis)
            .unwrap()
            .is_empty());
    }
}

#[cfg(all(test, feature = "swap-e2e"))]
mod e2e;
