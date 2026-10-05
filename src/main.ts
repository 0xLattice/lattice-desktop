// Lattice Desktop: the shared Nexus Wallet UI, bound to this app's own node.
// The signer runs in this page (there is no other context to hold it); keys
// are encrypted at rest in the extension's vault format and the page's only
// network reach is the shell's loopback-only node_http.

import { startWallet, refresh } from "../vendor/nexus-wallet/src/popup/app.ts";
import { createSigner } from "../vendor/nexus-wallet/src/lib/wallet/signer.ts";
import { walletClient } from "../vendor/nexus-wallet/src/lib/wallet/client.ts";
import { NodeManager } from "./node-manager.ts";
import { loopbackFetch } from "./loopback.ts";
import { nodeScreen } from "./node-panel.ts";
import { hostOps, nodeHTTP, vaultFile, settingsFile } from "./tauri-host.ts";

const AUTO_LOCK_MS = 10 * 60 * 1000;

async function boot() {
  const manager = new NodeManager(hostOps);
  // First run scaffolds the host root (`lattice init`); the node is started by the user.
  const ownNode = await manager.rpcURL();

  // Idle auto-lock, re-armed by every signer request (as the extension's worker does).
  let idle: ReturnType<typeof setTimeout> | undefined;
  const arm = () => {
    clearTimeout(idle);
    idle = setTimeout(() => { signer.lock(); refresh(); }, AUTO_LOCK_MS);
  };
  const signer = createSigner(vaultFile, arm);
  const wallet = walletClient(async (msg) => {
    arm();
    try {
      return await signer.handle(msg);
    } catch (e) {
      return { ok: false, error: String((e as Error)?.message ?? e) };
    }
  });

  await startWallet({
    wallet,
    store: settingsFile,
    // No origin is ever reachable but the app's own node.
    requestOrigins: async () => false,
    ownNode,
    fetch: loopbackFetch(ownNode, nodeHTTP),
    keyFilePicker: true,
    actions: [{ label: "Node", run: (back) => nodeScreen(manager, back, () => location.reload()) }],
  });
}

boot().catch((e) => {
  document.getElementById("view")!.textContent = "Could not start: " + String((e as Error)?.message ?? e);
});
