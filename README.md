# Lattice Desktop

A full Lattice node and the Lattice Wallet in one macOS app (Tauri v2).

The same app is packaged for Linux in
[lattice-desktop-linux](https://github.com/adalinxx/lattice-desktop-linux) and
for Windows in
[lattice-desktop-windows](https://github.com/adalinxx/lattice-desktop-windows).
Both pin this repository and add only their packaging, so the code here runs
on all three: the paths below are the macOS ones, a Linux package keeps the
node binaries in its own resources, and on Windows the shell runs the Linux
node in WSL (`src-tauri/src/wsl.rs`). The graphics-chip mining worker is each
platform's own: Metal on macOS, OpenCL on Linux and Windows.

- **Your own full node.** The app runs the bundled, unmodified `lattice-node`
  through the bundled `lattice` CLI (`init` / `up` / `down` / `status`), exactly
  as an operator would. The host root is
  `~/Library/Application Support/Lattice/host`. On first run the app scaffolds it
  with `lattice init` and gives the node its own ports (RPC 18080, peers 14001)
  so that it doesn't clash with a CLI-managed host on 8080/4001. Nexus bootstrap
  peers are the node's built-in defaults; you can change them in **Node →
  Node settings** (built-in, your own list, or none). Opening the app starts
  the node, and the wallet's home screen says what it is doing until it is in
  sync: stopped (with a Start button), starting, or how far along it is.
  Quitting the app stops the node cleanly (`lattice down`, which can take a
  few seconds). If the app is
  killed or crashes, the node keeps running; the next launch finds it by its
  pidfile. The app may change only `rpc`, `listen`, `peers` and `hostedChains`
  in `lattice.json`, and of `mine` only `workers`, `recipients`, and `worker`
  as one of the two bundled workers. Other fields, such as public listeners
  and mining's tuning, are left to the operator.
- **Mining** (Node → Mining) runs the bundled `lattice mine` loop on this node,
  searching on this computer's graphics chip (the bundled worker from
  [lattice-miner-gpu](https://github.com/adalinxx/lattice-miner-gpu), the
  default) or on its processor. The graphics chip is offered only where the
  worker finds a hash on it when the app asks, once per launch: a machine
  with no chip or driver it can use mines on the processor, and settings
  saved for a chip it cannot use are refused when mining starts. One search covers Nexus and every chain the
  node hosts (merged mining), so the chains mined are the chains hosted: to
  mine another, or stop mining one, change what the node hosts (the screen
  links to it). Each chain's reward and fees go to the account you choose, the
  wallet's active account unless you change it, or to no one (they burn).
  While it runs the screen shows the blocks found in this run and when the
  last was, the Node screen and the wallet's home screen say that it is
  mining, and a chain set to pay no one is warned about before it starts.
  Applying a change while it runs restarts the loop. Stopping the node or quitting the app
  stops mining first. The page can choose only between the two bundled workers:
  a custom `mine.worker` set by hand in `lattice.json` is shown and kept, but
  set only by hand.
- **The Lattice Wallet, shared.** The UI and signer come from
  [lattice-wallet](https://github.com/adalinxx/lattice-wallet) (pinned submodule
  `vendor/nexus-wallet`, which pins the lattice-sdk). Keys use BIP39 +
  SLIP-0010 `m/44'/7878'/n'`, the same as the extension. The vault uses the
  extension's keystore format (Argon2id + AES-256-GCM), stored at
  `~/Library/Application Support/Lattice/wallet/vault.json` with mode 0600.
  `lattice key generate` key files import as standalone accounts.
- **Loopback only.** The webview has no network access: its CSP allows Tauri
  IPC only. Every node call goes through the shell's `node_http` command, which
  connects only to `127.0.0.1` on the RPC port the node was started on (read
  from `lattice.json` at launch and at each `lattice up`, never what the page
  writes later) and attaches the node's cookie: the node's operator port
  requires its bitcoind-style cookie (`<root>/chains/Nexus/.cookie`, rewritten
  at every node start) on everything but `/health`. Sends are
  signed in the app and submitted to the node's loopback operator route
  `POST /transactions`. The app checks the transaction CID the node returns
  against the CID it computed itself. There is no default endpoint and no relay.
- **Idle lock.** The wallet locks after ten minutes without a request that
  needs keys; looking at balances does not put it off. The screen stays as it
  is, and the next step that needs keys asks for the password over it.
- **Fees** can be set per transaction, with a default per chain. The app warns
  when a fee is below the node's `minRelayFee` but never clamps it.
- **Status** comes from an inclusion lookup (block height), and **balances**
  are shown for Nexus and for the child chains the node hosts. There is one
  chain list, `hostedChains` in `lattice.json`: adding a chain in the wallet's
  chain picker hosts it, and a chain added in Node settings appears in the
  picker (a running node restarts to take either).
- **Node settings** list the chains the node can see as boxes to tick (a path
  is typed only for one it cannot), with the ports under Ports. Saving restarts
  a running node.
- **Settings** open from the mark at the top left, which says so. The chain name beside it
  lists the current chain's child chains, and `↑` goes to its parent. A child
  chain this node does not host yet is marked "add to node": choosing it hosts
  it first (a running node restarts).
- **Cross-chain orders** (Settings → Open cross-chain order): sell deposits,
  purchases and their withdrawals, as in the extension. Both chains are read
  and submitted through this node, so it must host the child chain as well as
  its parent. The signed bytes of every unsettled transfer, deposit and
  purchase are kept in the settings file for status checks and exact
  resubmission (Settings → pending lists).
- **Sync progress**: the node screen shows each chain's executed height
  against the best header height the node knows (`/health`'s
  `bestHeaderHeight`).
- **Pair browser extension** (Node → Pair browser extension): allows the
  Lattice Wallet extension's origin in `rpcAllowedOrigins` (the node restarts)
  and copies the node's cookie for the extension's Node screen. The cookie
  changes whenever the node restarts; pair again then.

## Build

```sh
git clone --recurse-submodules https://github.com/adalinxx/lattice-desktop.git
cd lattice-desktop
npm ci                                   # also installs + builds the pinned wallet/SDK
scripts/build-node-binaries.sh           # builds the node, CLI and miner binaries at NODE_COMMIT (debug), and the GPU worker
npx tauri build --bundles app --no-sign  # -> src-tauri/target/release/bundle/macos/Lattice.app
npx tauri dev                            # or run in development
```

### Node binaries

The app bundles the `lattice`, `lattice-node`, `lattice-mining-coordinator` and
`lattice-miner` binaries from
[adalinxx/lattice-node](https://github.com/adalinxx/lattice-node) at the commit
pinned in `NODE_COMMIT`. `scripts/build-node-binaries.sh` builds them from
that commit and refuses a checkout that has local changes. It stages them as
Tauri sidecars in `src-tauri/binaries/` and records the commit in
`src-tauri/binaries/COMMIT`. They are built in the **debug** configuration on
purpose: Swift 6.3's `-O` miscompiles the node on macOS (the bug is in
`Task.sleep` loops), and the Mac Nexus node is built unoptimized for the same
reason. macOS binaries have to be built on macOS. To bundle a newer node, bump
`NODE_COMMIT` and rerun the script.

The same script builds the GPU mining worker, `lattice-miner-gpu`, from
[adalinxx/lattice-miner-gpu](https://github.com/adalinxx/lattice-miner-gpu) at
the commit pinned in `MINER_GPU_COMMIT`, optimized: it is a plain compute
program with no node code in it. It needs a Rust toolchain, as the shell does.

## Direct-download macOS releases

See [macOS release instructions](docs/macos-release.md) for the ad-hoc-signed
Apple Silicon build, checksum verification, per-app Gatekeeper exception,
repeat-build comparison, and publication gates. Release preparation does not
publish artifacts or create a project signing key.

## Tests

- `npm test`: unit tests for node management (first-run scaffolding, how
  settings map to `lattice.json`, restart-to-apply, parsing `lattice status`),
  the loopback-only fetch, and node panel wording. The keystore, fee and status
  logic is tested in nexus-wallet.
- `cargo test --manifest-path src-tauri/Cargo.toml`: the shell only runs the
  lifecycle verbs, only accepts loopback paths, and only uses the RPC port from
  `lattice.json`.
- End-to-end tests, headless:
  `LATTICE_NODE_BIN=<lattice-node>/.build/debug npm test`. They use the real
  CLI and node on throwaway storage with no peers, and nothing touches a live
  chain. `e2e-node` drives the node-management layer alone. The others run the
  app itself (`src/app.ts` on its own page) with a Node harness standing in
  for the shell, and click through it as a person would:
  - `e2e-app`: first run with the node stopped, a send with a custom fee, a
    wrong password, a relaunch, hosting a child chain from the chain menu, and
    the node being killed under the app.
  - `e2e-trade`: a private `Nexus/testnet` and a trade across it: the sell
    deposit, the purchase, the withdrawal, the purchase archived once it is
    proven settled, and the refusals on the way (wrong chain, an unmined
    receipt, a deposit bought twice).
  - `e2e-mining`: mining from the Mining screen with no other miner: refused
    while the node is stopped, the wallet's balance growing, a second account
    paid after a save, the chain standing still once stopped, and the node's
    Stop taking the miner with it. With `LATTICE_GPU_MINER=<a built
    lattice-miner-gpu>` it mines on the graphics chip first, where the
    machine has one it can use.
  - `e2e-settings`: Node settings typed wrong (refused, nothing written) and
    right, a port change that reloads the app, a child chain hosted from the
    Mining screen, both chains mined to different accounts with one paying no
    one, the node restarted under the miner, a chain dropped while mining
    still paid it, and a relaunch showing what was saved.
  - `e2e-first-run`: the app as it ships, starting its own node when it
    opens: a new wallet lands on a balance with no one pressing Start.
  - `e2e-custody`: a `lattice key generate` key file, the idle lock firing
    in the middle of a send, a fee replacement, pairing an extension, and a
    backup file restored on a second host.
- The packaged app: `LATTICE_APP=<path to Lattice.app> npm test` launches the
  bundle with a throwaway home directory and a private host root, and checks
  that the app itself brought its node up: the page loaded, reached the shell,
  and the shell ran the bundled CLI and node. It cannot see or click the
  window, so the camera, printing and file drops on the real window are
  checked by hand.
- Live, opt-in: `LATTICE_LIVE=1 LATTICE_NODE_BIN=… npm test` starts the app's
  node on the real network with its built-in peers and watches it sync. It is
  read-only and needs the network, so CI does not run it.
