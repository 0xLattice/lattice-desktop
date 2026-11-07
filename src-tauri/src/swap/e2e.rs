//! Whole BTC->NEX swaps through this app's engine: `open_with` and
//! `start_with` as the commands call them, its engine threads and their
//! `swap-status` reports, and the notices `background` makes of them,
//! against a regtest bitcoind, a real lattice-node and a lattice-swap CLI
//! seller, as lattice-swap's own e2e runs its native buyer
//! (crates/lattice-swap/tests/e2e.rs). The seller offers its tool's default
//! terms and the engine takes them under its default bounds, as the app
//! does. The test, not the parties, mines blocks on both chains.
//!
//! Built only with the `swap-e2e` feature, which downloads bitcoind, and
//! ignored even then: run with `--features swap-e2e`, `LATTICE_NODE_BIN`
//! naming the directory holding lattice-node and
//! lattice-mining-coordinator, `LATTICE_SWAP_BIN` a lattice-swap built at
//! the pinned rev, and `-- --ignored`.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bitcoin::{Amount, Denomination, Network};
use corepc_node::{Conf, Node};
use serde_json::{json, Value};
use swap::app::Config;
use swap::bitcoind::Bitcoind;
use swap::SecretBytes;
use swap_nexus_tx::rpc::Client;

use super::{lock, open_with, start_with, Emit, Engine};
use crate::background::{Notice, Tracker};

mod esplora;

const TIMEOUT: Duration = Duration::from_secs(10);
const BTC_AMOUNT: Amount = Amount::from_sat(50_000_000);
const BTC_FEE: Amount = Amount::from_sat(2_000);
/// A sat below 50 a base unit: `BTC_AMOUNT` buys `NEX_AMOUNT`, rounded
/// down.
const PRICE: u64 = 49_999_999;
const NEX_AMOUNT: u64 = 1_000_000;
const NEX_FEE: u64 = 10;
/// The seller's default `--btc-confirmations`: each Bitcoin block the test
/// mines is this many, so every transaction is at once as deep as the
/// swap asks and the timelocks stay far.
const BTC_BURST: usize = 6;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(250));
    }
}

/// A fresh directory of its own, for the test `name` in this process.
fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lattice-swap-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A process, killed if the test ends first.
struct Party(Child);

impl Drop for Party {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A lattice-node on loopback, with no peers and throwaway storage, its log
/// in a file, printed if the test fails.
struct Nexus {
    process: Party,
    bin: PathBuf,
    log: PathBuf,
    url: String,
    cookie: PathBuf,
}

impl Nexus {
    fn start(dir: &std::path::Path) -> Self {
        let bin = PathBuf::from(
            std::env::var("LATTICE_NODE_BIN").expect("LATTICE_NODE_BIN names the node binaries"),
        );
        let (rpc, p2p) = (free_port(), free_port());
        let data = dir.join("node");
        let log = dir.join("node.log");
        let out = fs::File::create(&log).unwrap();
        let process = Command::new(bin.join("lattice-node"))
            .arg("--data-directory")
            .arg(&data)
            .arg("--identity-key")
            .arg(dir.join("node.key"))
            .args(["--listen-port", &p2p.to_string()])
            .args(["--rpc-port", &rpc.to_string()])
            .args(["--websocket-port", "0", "--no-default-peers"])
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        let nexus = Nexus {
            process: Party(process),
            bin,
            log,
            url: format!("http://127.0.0.1:{rpc}"),
            cookie: data.join(".cookie"),
        };
        wait_for("the node's RPC", || {
            nexus.client().min_relay_fee().ok()
        });
        nexus
    }

    /// The node's RPC, which reads its cookie at each request until it has
    /// one.
    fn client(&self) -> Client {
        Client::from_cookie_file(&self.url, &self.cookie, TIMEOUT)
    }

    fn balance(&self, address: &str) -> u64 {
        self.client().account(address).unwrap().balance
    }

    /// Mines one block paying `recipient`, and waits until the node
    /// executes it.
    fn mine(&self, recipient: &str) {
        let client = self.client();
        let height = client.health().unwrap().height;
        wait_for("a mined block", || {
            if client.health().unwrap().height > height {
                return Some(());
            }
            let out = Command::new(self.bin.join("lattice-mining-coordinator"))
                .args(["--node", &self.url, "--rpc-cookie-file"])
                .arg(&self.cookie)
                .args([
                    "--workers",
                    "1",
                    "--recipient",
                    &format!("Nexus={recipient}"),
                ])
                .args(["--once", "--no-stale-probe"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            None
        });
    }
}

impl Drop for Nexus {
    fn drop(&mut self) {
        let _ = self.process.0.kill();
        if thread::panicking() {
            let log = fs::read_to_string(&self.log).unwrap_or_default();
            let tail: Vec<_> = log.lines().rev().take(80).collect();
            eprintln!("--- node log, last lines ---");
            for line in tail.into_iter().rev() {
                eprintln!("{line}");
            }
        }
    }
}

fn wallet_client(node: &Node, wallet: &str) -> Bitcoind {
    let cookie = fs::read_to_string(&node.params.cookie_file).unwrap();
    let url = match wallet {
        "" => node.rpc_url(),
        wallet => node.rpc_url_with_wallet(wallet),
    };
    Bitcoind::new(&url, &cookie, Network::Regtest, TIMEOUT)
}

/// The wallet's confirmed balance.
fn btc_balance(wallet: &Bitcoind) -> Amount {
    let balances = wallet.call("getbalances", json!([])).unwrap();
    let trusted = balances["mine"]["trusted"].as_f64().unwrap();
    Amount::from_float_in(trusted, Denomination::Bitcoin).unwrap()
}

/// What the engine emits, as the app's emitter passes it on: each status
/// report to a `background` tracker, whose notices are kept.
#[derive(Default)]
struct Seen {
    statuses: Mutex<Vec<Value>>,
    notices: Mutex<Vec<Notice>>,
}

impl Seen {
    fn emitter() -> (Emit, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let tracker = Mutex::new(Tracker::default());
        let sink = seen.clone();
        let emit: Emit = Arc::new(move |event, payload| {
            if event != "swap-status" {
                eprintln!("{event}: {payload}");
                return;
            }
            if let Some(notice) = tracker.lock().unwrap().report(&payload) {
                sink.notices.lock().unwrap().push(notice);
            }
            sink.statuses.lock().unwrap().push(payload);
        });
        (emit, seen)
    }

    fn of(&self, id: &str) -> Vec<Value> {
        let statuses = self.statuses.lock().unwrap();
        statuses
            .iter()
            .filter(|status| status["swapId"] == id)
            .cloned()
            .collect()
    }

    /// The steps reported for swap `id`, each once in a row.
    fn steps(&self, id: &str) -> Vec<String> {
        let mut steps: Vec<String> = self
            .of(id)
            .iter()
            .map(|status| status["step"].as_str().unwrap().to_string())
            .collect();
        steps.dedup();
        steps
    }

    fn ended(&self, id: &str) -> bool {
        self.of(id).iter().any(|status| status["step"] == "done")
    }

    fn notices(&self) -> Vec<Notice> {
        self.notices.lock().unwrap().clone()
    }

    /// Swap `id` ended swapped, its NEX lock counted up while received.
    fn assert_swapped(&self, id: &str) {
        let statuses = self.of(id);
        let last = statuses.last().unwrap();
        assert_eq!(
            (&last["step"], &last["outcome"]),
            (&json!("done"), &json!("swapped")),
            "{last}"
        );
        let depths: Vec<u64> = statuses
            .iter()
            .filter(|status| status["step"] == "received")
            .map(|status| status["confirmations"]["nexLock"].as_u64().unwrap())
            .collect();
        assert!(
            depths.windows(2).all(|pair| pair[0] <= pair[1]),
            "{depths:?}"
        );
        assert!(
            depths.first() < depths.last(),
            "the claim's depth counts up: {depths:?}"
        );
    }
}

fn close(engine: &Mutex<Engine>) {
    let detached = lock(engine).unwrap().detach();
    detached.finish();
}

/// A regtest bitcoind with a seller wallet and a buyer wallet holding 1
/// BTC, an Esplora over it, a Nexus node whose first blocks pay the
/// seller, and a directory for the seller and the engine's wallet.
struct World {
    dir: PathBuf,
    bitcoind: Node,
    miner: bitcoin::Address,
    nexus: Nexus,
    nexus_genesis: String,
    esplora: String,
    key_file: PathBuf,
    seller_nex: String,
    /// What the seller's NEX address held before the swap.
    seller_nex_before: u64,
    buyer_nex: String,
    /// Where the blocks mined while a swap runs pay.
    elsewhere: String,
}

impl World {
    fn new(name: &str) -> Self {
        let dir = temp(name);
        let mut conf = Conf::default();
        conf.args.push("-txindex");
        let bitcoind = Node::from_downloaded_with_conf(&conf).expect("start bitcoind");
        let miner = bitcoind.client.new_address().unwrap();
        bitcoind.client.generate_to_address(101, &miner).unwrap();
        for name in ["seller", "buyer"] {
            bitcoind.create_wallet(name).unwrap();
        }
        let buyer_btc = wallet_client(&bitcoind, "buyer")
            .call("getnewaddress", json!([]))
            .unwrap();
        wallet_client(&bitcoind, "default")
            .call("sendtoaddress", json!([buyer_btc, 1.0]))
            .unwrap();
        bitcoind.client.generate_to_address(1, &miner).unwrap();
        // The parties refuse a node whose index is still catching up.
        wait_for("the txindex", || {
            wallet_client(&bitcoind, "buyer").check_node().ok().and_then(|ready| ready.ok())
        });
        let esplora = esplora::esplora(wallet_client(&bitcoind, ""));

        let nexus = Nexus::start(&dir);
        let seller_key = SecretBytes([7; 32]);
        let seller_nex = seller_key.nex_address();
        for _ in 0..2 {
            nexus.mine(&seller_nex);
        }
        let seller_nex_before = nexus.balance(&seller_nex);
        assert!(
            seller_nex_before > NEX_AMOUNT + NEX_FEE,
            "{seller_nex_before}"
        );
        let nexus_genesis = nexus.client().genesis().unwrap();
        let key_file = dir.join("nex.key");
        let hex: String = seller_key.0.iter().map(|b| format!("{b:02x}")).collect();
        fs::write(&key_file, hex).unwrap();
        fs::set_permissions(
            &key_file,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        World {
            dir,
            bitcoind,
            miner,
            nexus,
            nexus_genesis,
            esplora,
            key_file,
            seller_nex,
            seller_nex_before,
            buyer_nex: SecretBytes([8; 32]).nex_address(),
            elsewhere: SecretBytes([9; 32]).nex_address(),
        }
    }

    /// The engine's wallet config, as `config_at` makes it but for regtest,
    /// this Esplora, and this node's genesis.
    fn config(&self) -> Config {
        let mut config = Config::new(
            self.dir.join("swap"),
            Network::Regtest,
            self.esplora.clone(),
            self.nexus.url.clone(),
            self.nexus.cookie.clone(),
            self.nexus_genesis.clone(),
        );
        config.poll = Duration::from_millis(500);
        config
    }

    /// The seller's `offer`, listening on loopback, on its default terms:
    /// the seller, its offer, and the rest of its output once it ends.
    fn offer(&self) -> (Party, String, JoinHandle<String>) {
        let bin = std::env::var("LATTICE_SWAP_BIN").expect("LATTICE_SWAP_BIN names lattice-swap");
        let mut seller = Party(
            Command::new(bin)
                .args(["offer", "--listen", "127.0.0.1:0"])
                .args(["--price", &PRICE.to_string()])
                .args([
                    "--min-btc-amount",
                    "10000000",
                    "--max-btc-amount",
                    "60000000",
                ])
                .args(["--btc-fee", &BTC_FEE.to_sat().to_string()])
                .args(["--nex-fee", &NEX_FEE.to_string()])
                // Regtest has no fee estimate; nothing is bumped.
                .args(["--min-btc-fee-rate", "1"])
                .args(["--fallback-fee-rate", "1", "--max-fee-rate", "1"])
                .arg("--dir")
                .arg(self.dir.join("seller"))
                .arg("--nex-key-file")
                .arg(&self.key_file)
                .args([
                    "--btc-url",
                    &self.bitcoind.rpc_url(),
                    "--btc-wallet",
                    "seller",
                ])
                .arg("--btc-cookie")
                .arg(&self.bitcoind.params.cookie_file)
                .args(["--btc-network", "regtest", "--nexus-url", &self.nexus.url])
                .arg("--nexus-cookie")
                .arg(&self.nexus.cookie)
                .args(["--nexus-genesis", &self.nexus_genesis])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut reader = BufReader::new(seller.0.stdout.take().unwrap());
        let mut offer = String::new();
        reader.read_line(&mut offer).unwrap();
        let rest = thread::spawn(move || {
            let mut rest = String::new();
            reader.read_to_string(&mut rest).unwrap();
            rest
        });
        (seller, offer.trim().to_string(), rest)
    }

    /// The engine opens its wallet and starts a swap of `offer`, as
    /// `swap_wallet_open` and `swap_start` do, and the buyer's bitcoind
    /// wallet deposits what it needs: the swap's id and the wallet's
    /// address.
    fn take(&self, engine: &Mutex<Engine>, offer: &str, emit: &Emit) -> (String, String) {
        let wallet = open_with(engine, || Ok(self.config()), emit).unwrap();
        let id = start_with(
            engine,
            || Ok(self.config()),
            offer,
            (BTC_AMOUNT.to_sat(), PRICE),
            &self.buyer_nex,
            emit,
        )
        .unwrap();
        let (_, need) = wallet.need(BTC_AMOUNT.to_sat()).unwrap();
        let address = wallet.address().to_string();
        wallet_client(&self.bitcoind, "buyer")
            .call(
                "sendtoaddress",
                json!([address, Amount::from_sat(need).to_btc()]),
            )
            .unwrap();
        (id, address)
    }

    /// Mines until `done`: Bitcoin, `BTC_BURST` blocks whenever its pool
    /// holds something; Nexus, a block every round, so the NEX lock and
    /// the claim reach their 24 confirmations.
    fn drive(&self, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(900);
        while !done() {
            assert!(Instant::now() < deadline, "the swap timed out");
            let mempool = wallet_client(&self.bitcoind, "")
                .call("getmempoolinfo", json!([]))
                .unwrap();
            if mempool["size"].as_u64() > Some(0) {
                self.bitcoind
                    .client
                    .generate_to_address(BTC_BURST, &self.miner)
                    .unwrap();
            }
            self.nexus.mine(&self.elsewhere);
            thread::sleep(Duration::from_millis(300));
        }
    }

    /// Drives until the seller exits and swap `id` has ended: the seller's
    /// output.
    fn finish(
        &self,
        seller: &mut Party,
        seller_out: JoinHandle<String>,
        id: &str,
        seen: &Seen,
    ) -> String {
        let mut exited = None;
        self.drive(|| {
            exited = exited.or(seller.0.try_wait().unwrap());
            exited.is_some() && seen.ended(id)
        });
        assert!(exited.unwrap().success());
        seller_out.join().unwrap()
    }

    /// What bitcoind holds at `address` in blocks.
    fn received(&self, address: &str) -> Amount {
        let received = wallet_client(&self.bitcoind, "")
            .call(
                "scantxoutset",
                json!(["start", [format!("addr({address})")]]),
            )
            .unwrap();
        Amount::from_btc(received["total_amount"].as_f64().unwrap()).unwrap()
    }

    /// Each side has what the swap gave it.
    fn assert_balances(&self, wallet: &str) {
        assert_eq!(self.nexus.balance(&self.buyer_nex), NEX_AMOUNT - NEX_FEE);
        assert_eq!(
            self.nexus.balance(&self.seller_nex),
            self.seller_nex_before - NEX_AMOUNT - NEX_FEE
        );
        let seller = wallet_client(&self.bitcoind, "seller");
        assert_eq!(btc_balance(&seller), BTC_AMOUNT - BTC_FEE * 2);
        // The deposit less the lock and the funding's fee: under 1000 sat.
        let change = self.received(wallet);
        assert!(change < Amount::from_sat(1_000), "{change}");
    }
}

/// A whole swap through the engine: its status reports pass through every
/// step to the swap, the tray's tracker says the lock and the NEX once
/// each, and each side has what it swapped for.
#[test]
#[ignore = "needs LATTICE_NODE_BIN and LATTICE_SWAP_BIN"]
fn a_swap_through_the_engine_on_real_chains() {
    let world = World::new("swap");
    let (mut seller, offer, seller_out) = world.offer();
    let engine = Mutex::new(Engine::default());
    let (emit, seen) = Seen::emitter();
    let (id, wallet) = world.take(&engine, &offer, &emit);
    let out = world.finish(&mut seller, seller_out, &id, &seen);
    close(&engine);
    assert_eq!(out, "outcome: Swapped\n");
    assert_eq!(
        seen.steps(&id),
        ["fund-wallet", "swapping", "finishing", "received", "done"]
    );
    seen.assert_swapped(&id);
    assert_eq!(seen.notices(), [Notice::Locked, Notice::Received]);
    world.assert_balances(&wallet);
}

/// The engine closed once the BTC lock is confirmed, as the app quit, and a
/// new one opened on the same wallet, as at its next launch: the swap is
/// resumed, meets the seller again, and completes.
#[test]
#[ignore = "needs LATTICE_NODE_BIN and LATTICE_SWAP_BIN"]
fn a_swap_the_engine_reopens_mid_way_completes() {
    let world = World::new("reopen");
    let (mut seller, offer, seller_out) = world.offer();
    let engine = Mutex::new(Engine::default());
    let (emit, seen) = Seen::emitter();
    let (id, wallet) = world.take(&engine, &offer, &emit);
    world.drive(|| seen.notices().contains(&Notice::Locked));
    close(&engine);
    drop(engine);
    assert!(!seen.ended(&id));

    let engine = Mutex::new(Engine::default());
    let (emit, again) = Seen::emitter();
    drop(open_with(&engine, || Ok(world.config()), &emit).unwrap());
    let followed = lock(&engine)
        .unwrap()
        .followed
        .as_ref()
        .map(|run| run.swap_id.clone());
    assert_eq!(followed, Some(id.clone()), "resumed at the open");
    let out = world.finish(&mut seller, seller_out, &id, &again);
    close(&engine);
    assert_eq!(out, "outcome: Swapped\n");
    let steps = again.steps(&id);
    assert!(
        steps.ends_with(&["finishing".into(), "received".into(), "done".into()]),
        "{steps:?}"
    );
    again.assert_swapped(&id);
    // The relaunched app's tracker is new: it says the confirmed lock
    // again, then the NEX, once each.
    assert_eq!(again.notices(), [Notice::Locked, Notice::Received]);
    world.assert_balances(&wallet);
}
