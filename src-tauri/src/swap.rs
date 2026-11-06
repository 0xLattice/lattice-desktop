//! Buying NEX with BTC: lattice-swap's native buyer engine (`swap::app`)
//! behind the commands the website's driver offers a page (`Wallet` and
//! `Buyer` in lattice-swap's web/driver), as eigenwallet runs its swap
//! engine in its Tauri backend.
//!
//! - Bitcoin is read through an Esplora, mempool.space's by default; NEX
//!   through this app's own node, on the pinned loopback RPC port with its
//!   cookie, which the engine reads again whenever the node restarts. The
//!   wallet opens only once a port is pinned, and opens again on a new one.
//! - The page chooses only the Bitcoin network (mainnet by default, signet to
//!   test). The Esplora and the relay are the operator's, edited by hand in
//!   `<app_root>/swap/settings.json`: https, or http to loopback.
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
use swap::offer::Offer;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::{
    app_root, cookie_path, err, host_root, read_topology_at, write_atomically, write_new,
    PinnedPort, Result,
};

/// The relay whose offers are listed unless the settings name another.
pub const DEFAULT_RELAY: &str = "https://lattice-swap-relay.fly.dev";
/// The Bitcoin networks a swap may run on: mainnet, and signet to test.
const NETWORKS: [Network; 2] = [Network::Bitcoin, Network::Signet];
/// The most a relay's offer board or a recovery file may hold.
const MAX_BODY: u64 = 8 * 1024 * 1024;

/// Where swaps run: the Bitcoin network, and the Esplora and relay when not
/// the defaults. Kept in `<app_root>/swap/settings.json`; the page sets only
/// the network.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub network: Network,
    #[serde(default)]
    pub esplora: Option<String>,
    #[serde(default)]
    pub relay: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            network: Network::Bitcoin,
            esplora: None,
            relay: None,
        }
    }
}

/// A URL the swap engine may read: https, or plain http only to this
/// machine; with a host, and no user name in front of it.
fn check_url(url: &str) -> Result<()> {
    let refused = || {
        Err(format!(
            "refused: {url} is neither https nor http to loopback"
        ))
    };
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refused();
    }
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        (None, None) => return refused(),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return refused();
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if secure || ["127.0.0.1", "localhost", "::1"].contains(&host) {
        Ok(())
    } else {
        refused()
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
        for url in [&self.esplora, &self.relay].into_iter().flatten() {
            check_url(url)?;
        }
        Ok(())
    }

    fn esplora(&self) -> Result<String> {
        self.esplora
            .clone()
            .or_else(|| default_esplora(self.network).map(String::from))
            .ok_or_else(|| format!("no Esplora for {}", self.network))
    }

    fn relay(&self) -> &str {
        self.relay.as_deref().unwrap_or(DEFAULT_RELAY)
    }
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

/// The engine's config: the wallet's directory, the Esplora, and the app's
/// node on the pinned port, the one port its cookie goes to. None pinned,
/// none: the wallet waits for the node.
fn config_at(root: &Path, host: &Path, pinned: Option<u16>, settings: &Settings) -> Result<Config> {
    let port = pinned.ok_or("the node is not running: swaps reach NEX through it")?;
    Ok(Config::new(
        swap_dir(root, settings.network)?,
        settings.network,
        settings.esplora()?,
        format!("http://127.0.0.1:{port}"),
        cookie_path(host),
        swap_nexus_tx::MAINNET_GENESIS.into(),
    ))
}

fn pinned(app: &AppHandle) -> Result<Option<u16>> {
    Ok(*app.state::<PinnedPort>().0.lock().map_err(err)?)
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
    /// With no wallet open (before the node runs), whether the swap wallet
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

/// The wallet for `config`, opened if it is not and its swaps resumed. One
/// open for another config (a node on a new port) is closed first, and its
/// threads waited for off the lock.
fn open_with(engine: &Mutex<Engine>, config: Config, emit: &Emit) -> Result<Arc<Wallet>> {
    open_patiently(engine, config, emit, OPEN_PATIENCE)
}

fn open_patiently(
    engine: &Mutex<Engine>,
    config: Config,
    emit: &Emit,
    patience: Duration,
) -> Result<Arc<Wallet>> {
    let (waiting, detached) = {
        let mut held = lock(engine)?;
        if let Some(wallet) = held.serves(&config) {
            return Ok(wallet);
        }
        held.reopening = true;
        (waiting(&held), held.detach())
    };
    detached.finish();
    let mut held = lock(engine)?;
    held.reopening = false;
    if let Some(wallet) = held.serves(&config) {
        return Ok(wallet);
    }
    if held.wallet.is_some() {
        return Err("refused: the swap wallet was opened meanwhile; try again".into());
    }
    let wallet = open_wallet(&config, patience)?;
    held.hold(wallet, waiting, emit)
}

/// The policy a swap is started under: the amount and the price the buyer
/// agreed to, every bound the engine's default.
fn policy(btc_amount: u64, price: u64) -> Value {
    json!({ "btc_amount": btc_amount, "price": price })
}

/// A swap of `offer` at `btc_amount` and `price`, its NEX to `nex_address`,
/// started on an engine thread: its id. Refused while a swap runs.
fn start_with(
    engine: &Mutex<Engine>,
    config: Config,
    offer: &str,
    (btc_amount, price): (u64, u64),
    nex_address: &str,
    emit: &Emit,
) -> Result<String> {
    lock(engine)?.refuse_running()?;
    let wallet = open_with(engine, config, emit)?;
    let mut held = lock(engine)?;
    // Opening may have resumed one.
    held.refuse_running()?;
    let buy = wallet
        .prepare(offer, policy(btc_amount, price), nex_address)
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

/// Sets the network in `<root>/swap/settings.json`, the rest kept. Changing
/// it closes the wallet, refused while a swap runs on the one it leaves.
fn set_network_with(engine: &Mutex<Engine>, root: &Path, network: Network) -> Result<()> {
    let mut settings = load_settings(root)?;
    if settings.network == network {
        return Ok(());
    }
    settings.network = network;
    settings.check()?;
    let detached = {
        let mut held = lock(engine)?;
        held.refuse_running()?;
        if held.wallet.is_none() {
            let leaving = load_settings(root)?.network;
            if let Some(id) = running_in(&root.join("swap").join(leaving.to_string()), leaving)? {
                return Err(format!("refused: swap {id} is running on {leaving}"));
            }
        }
        held.detach()
    };
    detached.finish();
    private_dir(&root.join("swap"))?;
    write_atomically(
        &settings_path(root),
        &serde_json::to_vec_pretty(&settings).map_err(err)?,
        0o600,
    )
}

/// The engine, as the app manages it.
#[derive(Default)]
pub struct Swaps(Mutex<Engine>);

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

/// Opens the swap wallet, if there is one, once the node's port is pinned,
/// and again when it moves: at launch and after `lattice up`. Its swaps
/// carry on.
pub fn sync(app: &AppHandle) -> Result<()> {
    let root = app_root(app)?;
    let settings = load_settings(&root)?;
    if !wallet_exists(&root, settings.network) || pinned(app)?.is_none() {
        return Ok(());
    }
    open_with(
        &app.state::<Swaps>().0,
        config(app, &settings)?,
        &emitter(app),
    )
    .map(|_| ())
}

#[tauri::command]
pub fn swap_settings(app: AppHandle) -> Result<Settings> {
    load_settings(&app_root(&app)?)
}

/// Chooses the Bitcoin network swaps run on; the wallet on it opens once
/// the node runs. Refused while a swap runs.
#[tauri::command]
pub async fn swap_set_network(app: AppHandle, network: Network) -> Result<()> {
    blocking(app, move |app, engine| {
        set_network_with(engine, &app_root(app)?, network)?;
        sync(app)
    })
    .await
}

/// Whether there is a swap wallet, none made: as the driver's
/// `Wallet.exists`.
#[tauri::command]
pub fn swap_wallet_exists(app: AppHandle) -> Result<bool> {
    let root = app_root(&app)?;
    Ok(wallet_exists(&root, load_settings(&root)?.network))
}

/// Opens the swap wallet, made if there is none, and resumes its swaps: its
/// deposit address. Refused until the node runs.
#[tauri::command]
pub async fn swap_wallet_open(app: AppHandle) -> Result<String> {
    blocking(app, |app, engine| {
        let settings = load_settings(&app_root(app)?)?;
        let wallet = open_with(engine, config(app, &settings)?, &emitter(app))?;
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
        let settings = load_settings(&app_root(app)?)?;
        start_with(
            engine,
            config(app, &settings)?,
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
/// offer that does not decode or is not signed by its seller, one at a
/// price of 0, and one with a number the page cannot hold exactly.
fn listed(body: &str, network: Network, genesis: &str) -> Result<Vec<Listed>> {
    let board: Vec<Value> =
        serde_json::from_str(body).map_err(|e| format!("not an offer board: {e}"))?;
    let mut list = vec![];
    for item in board {
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

/// The relay's offers, `GET <relay>/offers`, as [`listed`] keeps them.
#[tauri::command]
pub async fn swap_offers(app: AppHandle) -> Result<Vec<Listed>> {
    let settings = load_settings(&app_root(&app)?)?;
    tauri::async_runtime::spawn_blocking(move || {
        let url = format!("{}/offers", settings.relay().trim_end_matches('/'));
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
            .take(MAX_BODY + 1)
            .read_to_string(&mut body)
            .map_err(err)?;
        if body.len() as u64 > MAX_BODY {
            return Err(format!("{url}: too large"));
        }
        listed(&body, settings.network, swap_nexus_tx::MAINNET_GENESIS)
    })
    .await
    .map_err(err)?
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
            nex_confirmations: 24,
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
    fn the_engine_reads_only_https_or_loopback_http() {
        for ok in [
            "https://mempool.space/api",
            "http://127.0.0.1:3002/api",
            "http://localhost:3000",
            "http://[::1]:3000/api",
            "http://127.0.0.1",
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
        assert_eq!(Settings::default().relay(), DEFAULT_RELAY);
        let signet: Settings = serde_json::from_value(json!({ "network": "signet" })).unwrap();
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
        ] {
            fs::write(settings_path(&root), bad.to_string()).unwrap();
            assert!(load_settings(&root).is_err(), "{bad}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_page_sets_only_the_network_and_never_off_a_running_swap() {
        let root = temp("network");
        let engine = Mutex::new(Engine::default());
        fs::create_dir_all(root.join("swap")).unwrap();
        let operator = json!({ "network": "bitcoin", "esplora": "http://127.0.0.1:3002/api", "relay": "https://relay.example" });
        fs::write(settings_path(&root), operator.to_string()).unwrap();
        set_network_with(&engine, &root, Network::Signet).unwrap();
        let mut expected: Settings = serde_json::from_value(operator).unwrap();
        expected.network = Network::Signet;
        assert_eq!(
            load_settings(&root).unwrap(),
            expected,
            "the operator's URLs are kept"
        );
        assert!(set_network_with(&engine, &root, Network::Regtest).is_err());

        // A swap running on signet, its wallet closed: signet is not left.
        let signet = swap_dir(&root, Network::Signet).unwrap();
        drop(Wallet::open(offline(&signet)).unwrap());
        let running = "55".repeat(32);
        record(&signet, &running, true, json!({ "Locking": {} }));
        let refused = set_network_with(&engine, &root, Network::Bitcoin).unwrap_err();
        assert!(refused.contains(&running), "{refused}");
        // Nor while the engine runs it.
        let (emit, _) = collect();
        open_with(&engine, offline(&signet), &emit).unwrap();
        let refused = set_network_with(&engine, &root, Network::Bitcoin).unwrap_err();
        assert!(refused.contains(&running), "{refused}");
        assert_eq!(load_settings(&root).unwrap().network, Network::Signet);
        close(&engine);

        // Ended, it is left, and the wallet open is closed.
        record(&signet, &running, true, json!("Done"));
        open_with(&engine, offline(&signet), &emit).unwrap();
        set_network_with(&engine, &root, Network::Bitcoin).unwrap();
        assert!(lock(&engine).unwrap().wallet.is_none());
        assert_eq!(load_settings(&root).unwrap().network, Network::Bitcoin);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nex_is_reached_only_on_the_pinned_port() {
        let root = temp("config");
        let host = root.join("host");
        let settings = Settings::default();
        assert!(
            config_at(&root, &host, None, &settings).is_err(),
            "no pin, no wallet"
        );
        let config = config_at(&root, &host, Some(18080), &settings).unwrap();
        assert_eq!(config.nexus, "http://127.0.0.1:18080");
        assert_eq!(config.nexus_cookie, host.join("chains/Nexus/.cookie"));
        assert_eq!(config.dir, root.join("swap").join("bitcoin"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_node_on_a_new_port_reopens_the_wallet_and_its_swaps_go_on() {
        let dir = temp("repin");
        let running = "66".repeat(32);
        record(&dir, &running, true, json!({ "Locking": {} }));
        let (emit, _) = collect();
        let engine = Mutex::new(Engine::default());
        let first = open_with(&engine, offline_at(&dir, 9), &emit).unwrap();
        let again = open_with(&engine, offline_at(&dir, 9), &emit).unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "the same node: the same wallet"
        );
        drop((first, again));
        let moved = open_with(&engine, offline_at(&dir, 10), &emit).unwrap();
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
        let reader = open_with(&engine, offline_at(&dir, 9), &emit).unwrap();
        // A balance read still holds the wallet when the node moves.
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(reader);
        });
        let moved = open_patiently(
            &engine,
            offline_at(&dir, 10),
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
        let wallet = open_with(&engine, offline_at(&dir, 9), &emit).unwrap();
        let run = Run::spawn(
            wallet.resume(&waiting).unwrap(),
            Duration::from_millis(100),
            emit.clone(),
        )
        .unwrap();
        lock(&engine).unwrap().followed = Some(run);
        drop(wallet);
        drop(open_with(&engine, offline_at(&dir, 10), &emit).unwrap());
        assert_eq!(followed(&engine), Some(waiting));
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
        assert!(open_with(&engine, offline(&dir), &emit).is_err());
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
        let wallet = open_with(&engine, offline(&dir), &emit).unwrap();
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
            offline(&dir),
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
        open_with(&engine, offline(&dir), &emit).unwrap();
        assert_eq!(followed(&engine), None);
        let refused = start_with(
            &engine,
            offline(&dir),
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
            .prepare(&offer, policy(50_000, 100_000), nex)
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
        let policy = policy(50_000, 100_000);
        assert_eq!(policy.as_object().unwrap().len(), 2);
        let parsed: Policy = serde_json::from_value(policy).unwrap();
        assert_eq!((parsed.btc_amount, parsed.price), (50_000, 100_000));
        assert_eq!(parsed.bounds, Bounds::DEFAULT);
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
        let kept = open_with(&engine, offline(&other), &emit)
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
        let reader = open_with(&engine, offline(&dir), &emit).unwrap();
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
        open_with(&engine, offline(&dir), &emit).unwrap();
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
        let wallet = open_with(&engine, offline(&dir), &emit).unwrap();
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
        drop(open_with(&engine, offline_at(&dir, 9), &emit).unwrap());
        drop(open_with(&engine, offline_at(&dir, 10), &emit).unwrap());
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
        open_with(&engine, offline(&dir), &emit).unwrap();
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
    }
}
