//! Buying NEX with BTC: lattice-swap's native buyer engine (`swap::app`)
//! behind the commands the website's driver offers a page (`Wallet` and
//! `Buyer` in lattice-swap's web/driver), as eigenwallet runs its swap
//! engine in its Tauri backend.
//!
//! - Bitcoin is read through an Esplora, mempool.space's by default; NEX
//!   through this app's own node, on the pinned loopback RPC port with its
//!   cookie, which the engine reads again whenever the node restarts.
//! - The swap wallet is a single key of its own in
//!   `<app_root>/swap/<network>/` (0700), not derived from the vault, so a
//!   refund needs no unlock. Bitcoin mainnet by default; signet for testing.
//! - One swap at a time: a swap is started only once the one before it has
//!   ended, been cancelled, or lost its offer.
//! - Each swap runs on an engine thread, and its status goes to the page as
//!   a `swap-status` event each poll; a failed round, which the thread runs
//!   again, as `swap-error`.
//! - At launch, and whenever the wallet is opened, every swap that has taken
//!   its offer and not ended is resumed, and every punished one, to ask the
//!   seller for `s` again.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bitcoin::Network;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use swap::app::{default_esplora, Buy, Config, Wallet};
use swap::offer::Offer;
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    app_root, cookie_path, err, host_root, read_topology_at, rpc_port, topology_path,
    write_atomically, write_new, PinnedPort, Result,
};

/// The relay whose offers are listed unless the settings name another.
pub const DEFAULT_RELAY: &str = "https://lattice-swap-relay.fly.dev";
/// The Bitcoin networks a swap may run on: mainnet, and signet to test.
const NETWORKS: [Network; 2] = [Network::Bitcoin, Network::Signet];
/// The most a relay's offer board or a recovery file may hold.
const MAX_BODY: u64 = 8 * 1024 * 1024;

/// Where swaps run: the Bitcoin network, and the Esplora and relay when not
/// the defaults. Kept in `<app_root>/swap/settings.json`.
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

impl Settings {
    fn check(&self) -> Result<()> {
        if !NETWORKS.contains(&self.network) {
            return Err(format!(
                "refused: swaps run on bitcoin or signet, not {}",
                self.network
            ));
        }
        for url in [&self.esplora, &self.relay].into_iter().flatten() {
            let web = url.starts_with("https://") || url.starts_with("http://");
            if !web || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("not an http(s) URL: {url}"));
            }
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

fn load_settings(root: &Path) -> Result<Settings> {
    Ok(read_topology_at(&settings_path(root))?
        .map(serde_json::from_value)
        .transpose()
        .map_err(err)?
        .unwrap_or_default())
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

/// The engine's config: the wallet's directory, the Esplora, and the app's
/// node on the pinned port (or lattice.json's, before the first `up`).
fn config(app: &AppHandle, settings: &Settings) -> Result<Config> {
    let pinned = *app.state::<PinnedPort>().0.lock().map_err(err)?;
    let port = match pinned {
        Some(port) => port,
        None => rpc_port(&read_topology_at(&topology_path(app)?)?)?,
    };
    Ok(Config::new(
        swap_dir(&app_root(app)?, settings.network)?,
        settings.network,
        settings.esplora()?,
        format!("http://127.0.0.1:{port}"),
        cookie_path(&host_root(app)?),
        swap_nexus_tx::MAINNET_GENESIS.into(),
    ))
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

/// A swap on its engine thread, and the thread that reports its status.
struct Run {
    swap_id: String,
    buy: Arc<Buy>,
    /// Ends the run: a cancel, or the engine closing.
    halt: Arc<AtomicBool>,
    /// Ends the status reports too, with no last one: the engine closing.
    stop: Arc<AtomicBool>,
    engine: JoinHandle<()>,
    reports: JoinHandle<()>,
}

impl Run {
    /// `buy` run until it ends, its offer is gone, it is cancelled, or the
    /// engine closes. A run that fails is run again each `poll`, as the
    /// CLI's `resume` would be. Its status is emitted each `poll`, and once
    /// more when the run ends.
    fn spawn(buy: Buy, poll: Duration, emit: Emit) -> Result<Self> {
        let swap_id = buy.swap_id();
        let buy = Arc::new(buy);
        let halt = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let engine = {
            let (buy, halt, done, emit, id) = (
                buy.clone(),
                halt.clone(),
                done.clone(),
                emit.clone(),
                swap_id.clone(),
            );
            std::thread::Builder::new()
                .name(format!("swap {id}"))
                .spawn(move || {
                    let halted = || halt.load(Ordering::SeqCst);
                    while let Err(e) = buy.run(halted) {
                        emit(
                            "swap-error",
                            json!({ "swapId": id, "error": e.to_string() }),
                        );
                        if wait(halted, poll) {
                            break;
                        }
                    }
                    done.store(true, Ordering::SeqCst);
                })
        }
        .map_err(err)?;
        let reports = {
            let (buy, stop) = (buy.clone(), stop.clone());
            std::thread::Builder::new()
                .name(format!("swap {swap_id} status"))
                .spawn(move || loop {
                    let ended = done.load(Ordering::SeqCst);
                    if let Ok(status) = buy.status() {
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
        }
        .map_err(err)?;
        Ok(Self {
            swap_id,
            buy,
            halt,
            stop,
            engine,
            reports,
        })
    }

    /// Whether the swap is still being run.
    fn active(&self) -> bool {
        !self.engine.is_finished()
    }

    /// Stops both threads and waits for them: the swap carries on from its
    /// files when it is resumed.
    fn close(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.halt.store(true, Ordering::SeqCst);
        let _ = self.engine.join();
        let _ = self.reports.join();
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

/// The open swap wallet and the swaps it runs.
#[derive(Default)]
pub struct Engine {
    wallet: Option<Arc<Wallet>>,
    /// The swap the page follows: the one it started, or the one resumed
    /// running.
    followed: Option<Run>,
    /// Punished swaps resumed to ask the seller for `s`.
    punished: Vec<Run>,
}

impl Engine {
    fn wallet(&self) -> Result<Arc<Wallet>> {
        self.wallet
            .clone()
            .ok_or_else(|| "no swap wallet open".to_string())
    }

    /// The wallet in `config.dir`, made if there is none; opened, it resumes
    /// what [`to_resume`] picks.
    fn open(&mut self, config: Config, emit: &Emit) -> Result<Arc<Wallet>> {
        if let Some(wallet) = &self.wallet {
            return Ok(wallet.clone());
        }
        let wallet = Wallet::open(config).map_err(err)?;
        self.wallet = Some(wallet.clone());
        self.resume(None, emit)?;
        Ok(wallet)
    }

    /// Runs what [`to_resume`] picks; else `waiting`, a swap that was
    /// waiting for its funds, if it is still there.
    fn resume(&mut self, waiting: Option<String>, emit: &Emit) -> Result<()> {
        let wallet = self.wallet()?;
        let poll = wallet.config().poll;
        let (running, punished) = to_resume(&wallet)?;
        let waiting = waiting.filter(|id| wallet.swaps().is_ok_and(|ids| ids.contains(id)));
        if let Some(id) = running.or(waiting) {
            self.followed = Some(Run::spawn(
                wallet.resume(&id).map_err(err)?,
                poll,
                emit.clone(),
            )?);
        }
        for id in punished {
            self.punished.push(Run::spawn(
                wallet.resume(&id).map_err(err)?,
                poll,
                emit.clone(),
            )?);
        }
        Ok(())
    }

    /// Stops every run and drops the wallet, releasing its directory.
    fn close(&mut self) {
        for run in self
            .followed
            .take()
            .into_iter()
            .chain(self.punished.drain(..))
        {
            run.close();
        }
        self.wallet = None;
    }

    fn busy(&self) -> bool {
        self.followed.as_ref().is_some_and(Run::active) || self.punished.iter().any(Run::active)
    }

    /// A swap of `offer` under `policy`, its NEX to `nex_address`, started
    /// on an engine thread: its id. Refused while the swap before it runs.
    fn start(
        &mut self,
        config: Config,
        offer: &str,
        policy: Value,
        nex_address: &str,
        emit: &Emit,
    ) -> Result<String> {
        if let Some(run) = self.followed.as_ref().filter(|run| run.active()) {
            return Err(format!(
                "refused: swap {} is in progress; one swap at a time",
                run.swap_id
            ));
        }
        let wallet = self.open(config, emit)?;
        // `open` may have resumed one.
        if let Some(run) = self.followed.as_ref().filter(|run| run.active()) {
            return Err(format!(
                "refused: swap {} is in progress; one swap at a time",
                run.swap_id
            ));
        }
        let buy = wallet.prepare(offer, policy, nex_address).map_err(err)?;
        if let Some(previous) = self.followed.take() {
            previous.close();
        }
        let run = Run::spawn(buy, wallet.config().poll, emit.clone())?;
        let id = run.swap_id.clone();
        self.followed = Some(run);
        Ok(id)
    }

    /// The wallet in a recovery `file`, put in `config.dir`: every run is
    /// stopped and the wallet dropped first, as the engine requires, and
    /// then resumed, the restored swaps with it. The wallet's address.
    fn restore(&mut self, config: Config, file: &str, emit: &Emit) -> Result<String> {
        let waiting = self
            .followed
            .as_ref()
            .filter(|run| run.active())
            .map(|run| run.swap_id.clone());
        self.close();
        let restored = Wallet::restore(config.clone(), file).map_err(err);
        let wallet = match restored {
            Ok(wallet) => wallet,
            Err(e) => {
                // As it was before.
                if Wallet::open(config).map(|w| self.wallet = Some(w)).is_ok() {
                    let _ = self.resume(waiting, emit);
                }
                return Err(e);
            }
        };
        self.wallet = Some(wallet.clone());
        self.resume(waiting, emit)?;
        Ok(wallet.address().to_string())
    }
}

/// The engine, as the app manages it.
#[derive(Default)]
pub struct Swaps(Mutex<Engine>);

fn emitter(app: &AppHandle) -> Emit {
    let app = app.clone();
    Arc::new(move |event, payload| {
        let _ = app.emit(event, payload);
    })
}

/// `act` on the engine, off the async runtime: it may wait on the network
/// and on engine threads.
async fn with_engine<T: Send + 'static>(
    app: AppHandle,
    act: impl FnOnce(&AppHandle, &mut Engine) -> Result<T> + Send + 'static,
) -> Result<T> {
    tauri::async_runtime::spawn_blocking(move || {
        let swaps = app.state::<Swaps>();
        let mut engine = swaps.0.lock().map_err(err)?;
        act(&app, &mut engine)
    })
    .await
    .map_err(err)?
}

/// `read` on the open wallet, off the async runtime and the engine's lock.
async fn with_wallet<T: Send + 'static>(
    app: AppHandle,
    read: impl FnOnce(&Wallet) -> Result<T> + Send + 'static,
) -> Result<T> {
    let wallet = with_engine(app, |_, engine| engine.wallet()).await?;
    tauri::async_runtime::spawn_blocking(move || read(&wallet))
        .await
        .map_err(err)?
}

/// At launch: if there is a swap wallet, open it and resume its swaps.
pub fn resume_at_launch(app: &AppHandle) -> Result<()> {
    let root = app_root(app)?;
    let settings = load_settings(&root)?;
    if !wallet_exists(&root, &settings) {
        return Ok(());
    }
    let config = config(app, &settings)?;
    let swaps = app.state::<Swaps>();
    let mut engine = swaps.0.lock().map_err(err)?;
    engine.open(config, &emitter(app)).map(|_| ())
}

#[tauri::command]
pub fn swap_settings(app: AppHandle) -> Result<Settings> {
    load_settings(&app_root(&app)?)
}

/// Saves the settings, taken at the next open; the wallet is closed for it.
/// Refused while a swap runs.
#[tauri::command]
pub async fn swap_set_settings(app: AppHandle, settings: Settings) -> Result<()> {
    settings.check()?;
    with_engine(app, move |app, engine| {
        if engine.busy() {
            return Err("refused: a swap is running".into());
        }
        engine.close();
        let root = app_root(app)?;
        private_dir(&root.join("swap"))?;
        write_atomically(
            &settings_path(&root),
            &serde_json::to_vec_pretty(&settings).map_err(err)?,
            0o600,
        )
    })
    .await
}

/// Whether there is a swap wallet, none made: as the driver's
/// `Wallet.exists`.
#[tauri::command]
pub fn swap_wallet_exists(app: AppHandle) -> Result<bool> {
    let root = app_root(&app)?;
    Ok(wallet_exists(&root, &load_settings(&root)?))
}

/// Whether `<root>/swap/<network>` holds a wallet key, as the engine keeps it.
fn wallet_exists(root: &Path, settings: &Settings) -> bool {
    root.join("swap")
        .join(settings.network.to_string())
        .join("wallet.json")
        .exists()
}

/// Opens the swap wallet, made if there is none, and resumes its swaps: its
/// deposit address.
#[tauri::command]
pub async fn swap_wallet_open(app: AppHandle) -> Result<String> {
    with_engine(app, |app, engine| {
        let settings = load_settings(&app_root(app)?)?;
        let wallet = engine.open(config(app, &settings)?, &emitter(app))?;
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
/// under `policy`, its NEX to `nex_address`: its id. The wallet is opened
/// first if it is not.
#[tauri::command]
pub async fn swap_start(
    app: AppHandle,
    offer: String,
    policy: Value,
    nex_address: String,
) -> Result<String> {
    with_engine(app, move |app, engine| {
        let settings = load_settings(&app_root(app)?)?;
        engine.start(
            config(app, &settings)?,
            &offer,
            policy,
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
    let (buy, halt) = with_engine(app, |_, engine| {
        let run = engine.followed.as_ref().ok_or("no swap to cancel")?;
        Ok((run.buy.clone(), run.halt.clone()))
    })
    .await?;
    tauri::async_runtime::spawn_blocking(move || {
        buy.cancel().map_err(err)?;
        halt.store(true, Ordering::SeqCst);
        Ok(())
    })
    .await
    .map_err(err)?
}

/// The followed swap's status, as the driver's `Buyer.status()`; null when
/// there is none.
#[tauri::command]
pub async fn swap_status(app: AppHandle) -> Result<Option<Value>> {
    let buy = with_engine(app, |_, engine| {
        Ok(engine.followed.as_ref().map(|run| run.buy.clone()))
    })
    .await?;
    let Some(buy) = buy else { return Ok(None) };
    tauri::async_runtime::spawn_blocking(move || buy.status().map(Some).map_err(err))
        .await
        .map_err(err)?
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

/// The wallet in the recovery `file`: its address.
#[tauri::command]
pub async fn swap_restore(app: AppHandle, file: String) -> Result<String> {
    if file.len() as u64 > MAX_BODY {
        return Err("not a recovery file".into());
    }
    with_engine(app, move |app, engine| {
        let settings = load_settings(&app_root(app)?)?;
        engine.restore(config(app, &settings)?, &file, &emitter(app))
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
    use swap::offer::{Place, Quote, Template};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lattice-swap-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A wallet's config that reaches no backend: every read fails at once.
    fn offline(dir: &Path) -> Config {
        let nowhere = "http://127.0.0.1:9".to_string();
        let mut config = Config::new(
            dir.into(),
            Network::Signet,
            nowhere.clone(),
            nowhere,
            dir.join(".cookie"),
            swap_nexus_tx::MAINNET_GENESIS.into(),
        );
        config.poll = Duration::from_millis(100);
        config
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
        assert!(wallet_exists(&root, &Settings::default()));
        assert!(!wallet_exists(
            &root,
            &serde_json::from_value(json!({ "network": "signet" })).unwrap()
        ));
        drop(wallet);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_default_to_mainnet_and_name_only_bitcoin_or_signet() {
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
        assert!(signet.check().is_ok());
        for bad in [
            json!({ "network": "regtest" }),
            json!({ "network": "testnet" }),
            json!({ "network": "bitcoin", "esplora": "file:///etc" }),
            json!({ "network": "bitcoin", "relay": "https://a b" }),
        ] {
            let settings: Settings = serde_json::from_value(bad.clone()).unwrap();
            assert!(settings.check().is_err(), "{bad}");
        }
    }

    #[test]
    fn one_swap_at_a_time() {
        let dir = temp("one");
        fs::create_dir_all(&dir).unwrap();
        let (emit, _) = collect();
        let offer = offer(100_000, Network::Signet, swap_nexus_tx::MAINNET_GENESIS, 1).to_string();
        let policy = json!({ "btc_amount": 50_000, "price": 100_000 });
        let nex = swap_nexus_tx::MAINNET_GENESIS;
        let mut engine = Engine::default();

        // One the page follows, waiting for its funds: a second is refused.
        let waiting = "aa".repeat(32);
        record(&dir, &waiting, false, json!({ "Start": {} }));
        let wallet = engine.open(offline(&dir), &emit).unwrap();
        assert!(
            engine.followed.is_none(),
            "a swap waiting for funds is not resumed at launch"
        );
        engine.followed = Some(
            Run::spawn(
                wallet.resume(&waiting).unwrap(),
                Duration::from_millis(100),
                emit.clone(),
            )
            .unwrap(),
        );
        let refused = engine
            .start(offline(&dir), &offer, policy.clone(), nex, &emit)
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
        drop(wallet);
        engine.close();

        // One that has taken its offer, and that the engine does not run:
        // the wallet refuses too.
        let taken = "bb".repeat(32);
        record(&dir, &taken, true, json!({ "Locking": {} }));
        let wallet = Wallet::open(offline(&dir)).unwrap();
        let refused = wallet
            .prepare(&offer, policy, nex)
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
    fn launch_resumes_taken_unfinished_and_punished_swaps() {
        let dir = temp("resume");
        fs::create_dir_all(&dir).unwrap();
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
        let mut engine = Engine::default();
        engine.open(offline(&dir), &emit).unwrap();
        assert_eq!(
            engine.followed.as_ref().map(|run| run.swap_id.clone()),
            Some(running.clone())
        );
        assert_eq!(
            engine
                .punished
                .iter()
                .map(|run| run.swap_id.clone())
                .collect::<Vec<_>>(),
            vec![punished.clone()]
        );
        // These states are only names: each run fails, says so, and is
        // run again.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !events
            .lock()
            .unwrap()
            .iter()
            .any(|(event, payload)| *event == "swap-error" && payload["swapId"] == running.as_str())
        {
            assert!(
                Instant::now() < deadline,
                "no swap-error for the resumed swap"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            engine.followed.as_ref().unwrap().active(),
            "a failed run is run again"
        );
        engine.close();
        assert!(engine.wallet.is_none());
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
