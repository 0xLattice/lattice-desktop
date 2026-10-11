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
import { nodeScreen } from "./node-panel.ts";

export const AUTO_LOCK_MS = 10 * 60 * 1000;

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
}

export async function startApp(host: AppHost) {
  const manager = new NodeManager(host.ops);
  // First run scaffolds the host root (`lattice init`); the node is started by the user.
  const ownNode = await manager.rpcURL();

  // Idle auto-lock, re-armed by every signer request (as the extension's worker
  // does). The page is left as it is: the wallet notices the lock, says so, and
  // asks for the password over the current screen when a step needs keys, so a
  // half-entered send is not thrown away.
  let idle: ReturnType<typeof setTimeout> | undefined;
  const arm = () => {
    clearTimeout(idle);
    idle = setTimeout(() => signer.lock(), host.autoLockMs ?? AUTO_LOCK_MS);
    (idle as unknown as { unref?: () => void }).unref?.(); // never keeps a headless host running
  };
  const signer = createSigner(host.vault, arm);
  const wallet = walletClient(async (msg) => {
    // Passive reads are not use: the wallet checks in while it is in view, and
    // that must not keep it unlocked forever.
    if (msg.type !== "getState" && msg.type !== "nodeAuthorization") arm();
    try {
      return await signer.handle(msg);
    } catch (e) {
      return { ok: false, error: String((e as Error)?.message ?? e) };
    }
  });

  const fetch = loopbackFetch(ownNode, host.nodeHTTP);
  const reads = {
    health: (path: string) => reader(ownNode, path.split("/"), fetch).health(),
    cookie: host.nodeCookie,
    // The active account first: it is who mining pays unless told otherwise.
    accounts: async () => {
      const answer = await wallet.getState();
      if (!answer.ok || answer.state.locked) return [];
      const { accounts, active } = answer.state;
      return [...accounts].sort((a, b) => Number(b.address === active) - Number(a.address === active))
        .map(({ label, address }) => ({ label, address }));
    },
  };
  await startWallet({
    wallet,
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
    chains: { list: () => manager.chains(), add: (chain) => manager.addHostedChain(chain) },
    actions: [{ label: "Node", run: (back) => nodeScreen(manager, reads, back, host.reload) }],
  });
  return { manager, ownNode };
}
