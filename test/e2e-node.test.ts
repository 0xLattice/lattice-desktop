// End to end, headless: the app's own node-management layer (NodeManager)
// drives the real `lattice` CLI against throwaway storage with no peers, the
// shared wallet signer funds an account by mining, sends with a custom fee to
// the node's loopback operator route through the same loopback fetch the app
// uses, and sees the transaction's block. Nothing touches a live chain.
//
//   LATTICE_NODE_BIN=<lattice-node checkout>/.build/debug npm test
//
// (the directory holding lattice, lattice-node and lattice-mining-coordinator).

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeManager } from "../src/node-manager.ts";
import { withSync } from "../src/node-panel.ts";
import { loopbackFetch } from "../src/loopback.ts";
import { bin, cookieFile, freePort, harnessHost, mine as mineBlocks, until } from "./helpers/real-node.ts";
import { createSigner, type VaultStorage } from "../vendor/nexus-wallet/src/lib/wallet/signer.ts";
import { walletClient } from "../vendor/nexus-wallet/src/lib/wallet/client.ts";
import { reader, submitter, submitChecked, sentStatus, feeWarning, activeDeposits } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { importPrivateKey } from "../vendor/nexus-wallet/src/lib/crypto/accounts.ts";
import type { Vault } from "../vendor/nexus-wallet/src/lib/crypto/keystore.ts";

test("the app's node layer: init, start, fund by mining, send with a custom fee, see inclusion, stop", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-e2e-"));
  const host = harnessHost(root);
  const manager = new NodeManager(host.ops);
  try {
    // First run: `lattice init`, then a private node (no peers) on free ports.
    const prefs = await manager.prefs();
    assert.equal(prefs.peers, "default", "init leaves the node's built-in peers");
    await manager.configure({ ...prefs, rpc: await freePort(), listen: await freePort(), peers: "none" });
    assert.deepEqual((await host.ops.readTopology())?.peers, [], "peers: [] = --no-default-peers");
    assert.deepEqual(await manager.status(), [{ path: "Nexus", state: "down" }]);
    await manager.start();
    const active = await until(async () => {
      const [nexus] = await manager.status();
      return nexus?.state === "active" ? nexus : undefined;
    }, "node active");
    assert.equal(active.height, 0n);

    const url = await manager.rpcURL();
    const loopback = loopbackFetch(url, host.nodeHTTP);
    const reads = reader(url, ["Nexus"], loopback);
    const info = await reads.chainInfo();
    assert.equal(info.acceptsSubmit, true, "the loopback operator route accepts the owner's submits");
    // The operator port needs the cookie the shell attaches; without it, refused.
    assert.equal((await fetch(`${url}/api/chain/info`)).status, 401);
    // Sync progress: the node's best header height beside its executed height.
    const [synced] = await withSync(await manager.status(), { health: (path) => reader(url, path.split("/"), loopback).health() });
    assert.equal(synced?.bestHeaderHeight, 0n);

    // The wallet: the shared signer over an in-memory vault.
    let vault: Vault | null = null;
    const vaults: VaultStorage = { load: async () => vault, save: async (v) => { vault = v; }, remove: async () => { vault = null; } };
    const wallet = walletClient(createSigner(vaults).handle);
    const created = await wallet.create("e2e password", {
      mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
    });
    assert.ok(created.ok);
    const from = created.state.active!;
    const to = importPrivateKey("b0".repeat(32)).address;

    const mine = (blocks: number) =>
      mineBlocks(root, url, { Nexus: from }, blocks, async () => (await reads.chainInfo()).height ?? 0n);
    await mine(2);
    const funded = await reads.account(from);
    assert.ok(funded.balance > 0n, "mining funded the wallet");

    const fee = 23n; // the user's per-send choice
    assert.equal(feeWarning(fee, info.minRelayFee), undefined);
    const signed = await wallet.signTransfer({
      from, to, amount: "1000", fee: fee.toString(), nonce: funded.nonce.toString(), chainPath: ["Nexus"],
    });
    assert.ok(signed.ok);
    const cid = await submitChecked(submitter(url, loopback), signed.signedSubmit);
    assert.deepEqual(await sentStatus(reads, cid), { kind: "pending" });
    const before = (await reads.chainInfo()).height!;
    await mine(1);
    const included = await until(async () => {
      const s = await sentStatus(reads, cid);
      return s.kind === "included" ? s : undefined;
    }, "inclusion");
    assert.ok(included.kind === "included" && included.height > before);
    assert.equal((await reads.block(included.height)).hash, included.hash);
    assert.equal((await reads.account(to)).balance, 1000n);
    console.log(`[desktop e2e] ${cid} included at height ${included.height}, fee ${fee}`);
    // Cross-chain orders read deposits with a state proof the wallet checks
    // against the tip: the bundled node serves that read on the operator port.
    assert.deepEqual(await activeDeposits(url, ["Nexus"], loopback), []);

    const [nexus] = await manager.status();
    assert.ok(nexus!.height! >= included.height, "status reports the synced height");

    // Pairing an extension: its origin is allowed (the node restarts with a
    // fresh cookie); with that cookie, requests from that origin are served,
    // any other origin is refused.
    const origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
    const oldCookie = readFileSync(cookieFile(root), "utf8");
    await manager.allowOrigin(origin);
    const cookie = await until(async () => {
      const c = readFileSync(cookieFile(root), "utf8");
      return c !== oldCookie && (await fetch(`${url}/health`)).ok ? c : undefined;
    }, "restart with a new cookie");
    const as = (o: string) => fetch(`${url}/api/chain/info`, { headers: { Origin: o, Authorization: "Basic " + btoa(cookie.trim()) } });
    const paired = await as(origin);
    assert.equal(paired.status, 200);
    assert.equal(paired.headers.get("access-control-allow-origin"), origin);
    assert.equal((await as("chrome-extension://someoneelseeeeeeeeeeeeeeeeeeeeeee")).status, 403);
    await manager.stop();
    assert.deepEqual(await manager.status(), [{ path: "Nexus", state: "down" }]);
  } catch (e) {
    try {
      console.error(readFileSync(join(root, "log", "lattice-node.log"), "utf8").slice(-4000));
    } catch {}
    throw e;
  } finally {
    await host.ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
