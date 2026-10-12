// The desktop app's wiring: the shared Lattice Wallet UI, bound to this app's
// own node. The signer runs in this page (there is no other context to hold
// it); keys are encrypted at rest in the extension's vault format and the
// page's only network reach is the host's loopback-only node HTTP. The host
// is the Tauri shell in the app, and a Node harness in the end-to-end tests.

import { startWallet } from "../vendor/nexus-wallet/src/popup/app.ts";
import { createSigner, type VaultStorage } from "../vendor/nexus-wallet/src/lib/wallet/signer.ts";
import { walletClient } from "../vendor/nexus-wallet/src/lib/wallet/client.ts";
import { reader } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import type { KeyValueStore } from "../vendor/nexus-wallet/src/lib/wallet/settings.ts";
import { NodeManager, type HostOps } from "./node-manager.ts";
import { loopbackFetch, type NodeHTTP } from "./loopback.ts";
import { hostableChains, miningScreen, nodeNotices, nodeScreen, nodeSummary } from "./node-panel.ts";

export const AUTO_LOCK_MS = 10 * 60 * 1000;
/** How often the idle clock is read. */
const LOCK_BEAT_MS = 1000;
/** A beat this late was not a slow timer: the computer slept or the app was suspended. */
const SLEEP_GAP_MS = 30_000;

/** Everything privileged the page asks of its host. */
export interface AppHost {
  ops: HostOps;
  nodeHTTP: NodeHTTP;
  /** The node's current loopback cookie (null when not running). */
  nodeCookie(): Promise<string | null>;
  vault: VaultStorage;
  settings: KeyValueStore;
  saveBackup(name: string, text: string): Promise<string>;
  print(): void;
  /** Start over after the node's RPC port changed. */
  reload(): void;
  autoLockMs?: number;
  /**
   * Start the node when the app opens (default: yes). The app is a wallet on
   * its own node; without the node it can show nothing, so a person should not
   * have to find the switch first.
   */
  autoStart?: boolean;
}

export async function startApp(host: AppHost) {
  const manager = new NodeManager(host.ops);
  // First run scaffolds the host root (`lattice init`); the node is started by the user.
  const ownNode = await manager.rpcURL();

  // Idle auto-lock. "Use" is a signer request that needs keys or the person
  // touching the page, and idleness is read off the wall clock once a second,
  // not left to one long timer: a timer stops while the computer sleeps, and
  // the wallet would wake still unlocked. A beat that arrives late means just
  // that (the computer slept, or the app was suspended), and locks at once.
  // The wallet is told the moment the signer locks and covers the page.
  let unlocked = false;
  let lastUse = Date.now();
  let lastBeat = Date.now();
  let tellLocked = () => {};
  const used = () => { lastUse = Date.now(); };
  const lock = () => {
    if (!unlocked) return;
    unlocked = false;
    signer.lock();
    tellLocked();
  };
  const beat = setInterval(() => {
    const now = Date.now();
    const slept = now - lastBeat > SLEEP_GAP_MS;
    lastBeat = now;
    if (slept || now - lastUse >= (host.autoLockMs ?? AUTO_LOCK_MS)) lock();
  }, LOCK_BEAT_MS);
  (beat as unknown as { unref?: () => void }).unref?.(); // never keeps a headless host running
  for (const event of ["pointerdown", "keydown", "wheel", "touchstart"]) {
    document.addEventListener(event, used, { capture: true, passive: true });
  }
  // The signer calls this whenever a session opens or keys are touched.
  const signer = createSigner(host.vault, () => { unlocked = true; used(); });
  const wallet = walletClient(async (msg) => {
    // Passive reads are not use: the wallet checks in while it is in view, and
    // that must not keep it unlocked forever.
    if (msg.type !== "getState" && msg.type !== "nodeAuthorization") used();
    try {
      const answer = await signer.handle(msg);
      // The wallet's own Lock button, or a reset: the page already shows it.
      if (msg.type === "lock" || msg.type === "reset") unlocked = false;
      return answer;
    } catch (e) {
      return { ok: false, error: String((e as Error)?.message ?? e) };
    }
  });

  const fetch = loopbackFetch(ownNode, host.nodeHTTP);
  const reads = {
    health: (path: string) => reader(ownNode, path.split("/"), fetch).health(),
    cookie: host.nodeCookie,
    peers: async (path: string) => {
      const answer = await fetch(`${ownNode}/api/peers?chainPath=${encodeURIComponent(path)}`);
      return Number(((await answer.json()) as { count?: number }).count ?? 0);
    },
    children: async (path: string) => {
      const chain = reader(ownNode, path.split("/"), fetch);
      const latest = await chain.latestBlock();
      return (await chain.children(latest.hash))
        .filter((child) => child.directory && !child.directory.includes("/"))
        .map((child) => `${path}/${child.directory}`);
    },
    // The active account first: it is who mining pays unless told otherwise.
    accounts: async () => {
      const answer = await wallet.getState();
      if (!answer.ok || answer.state.locked) return [];
      const { accounts, active } = answer.state;
      return [...accounts].sort((a, b) => Number(b.address === active) - Number(a.address === active))
        .map(({ label, address }) => ({ label, address }));
    },
  };
  // Opening the app starts its node. Not waited for: the wallet opens at once
  // and its home screen says the node is starting, then syncing.
  if (host.autoStart !== false) void manager.start().catch(() => { /* the home screen offers Start again */ });
  await startWallet({
    wallet,
    onLocked: (tell) => { tellLocked = tell; },
    store: host.settings,
    // No origin is ever reachable but the app's own node.
    requestOrigins: async () => false,
    ownNode,
    fetch,
    keyFilePicker: true,
    // Backups: the webview may hold the camera (the shell's Info.plist asks
    // macOS); files go to ~/Downloads and printing through the shell.
    camera: true,
    saveFile: host.saveBackup,
    print: host.print,
    // One chain list: the chains this node hosts (lattice.json). Adding one in
    // the wallet hosts it (a running node restarts).
    chains: {
      list: () => manager.chains(),
      add: (chain) => manager.addHostedChain(chain),
      // The chain menu lists what the node sees and does not host, and only
      // switches: hosting a chain, like everything else about the node's
      // chains, is on the node's own pages.
      seen: async () => hostableChains((await manager.prefs()).hostedChains, reads),
      manage: (back, chain) => nodeScreen(manager, reads, back, host.reload, undefined, chain),
    },
    actions: [
      // Mining is something people do, so it sits beside Send and Receive. Its
      // "Choose hosted chains" opens the node page, and comes back once saved.
      {
        label: "Mining", on: "home",
        run: (back) => miningScreen(manager, reads, (saved) => (saved ? nodeScreen(manager, reads, back, host.reload, saved) : back()), host.reload),
      },
      // The node itself is set up once and left alone: it is in Settings.
      // Its row in Settings says what the node is doing, so checking on it needs no second page.
      { label: "Your node", on: "settings", run: (back) => nodeScreen(manager, reads, back, host.reload), summary: () => nodeSummary(manager, reads) },
    ],
    // The home screen says what the node is doing before its numbers are read.
    notices: (chain) => nodeNotices(manager, reads, chain),
  });
  return { manager, ownNode };
}
