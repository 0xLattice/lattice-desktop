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
import { execFile } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { NodeManager, type HostOps, type Topology } from "../src/node-manager.ts";
import { withSync } from "../src/node-panel.ts";
import { loopbackFetch, type NodeHTTP } from "../src/loopback.ts";
import { createSigner, type VaultStorage } from "../vendor/nexus-wallet/src/lib/wallet/signer.ts";
import { walletClient } from "../vendor/nexus-wallet/src/lib/wallet/client.ts";
import { reader, submitter, submitChecked, sentStatus, feeWarning } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { importPrivateKey } from "../vendor/nexus-wallet/src/lib/crypto/accounts.ts";
import type { Vault } from "../vendor/nexus-wallet/src/lib/crypto/keystore.ts";

const bin = process.env.LATTICE_NODE_BIN;
const run = promisify(execFile);

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolve(typeof address === "object" && address ? address.port : 0));
    });
  });
}

async function until<T>(probe: () => Promise<T | undefined>, what: string, ms = 120_000): Promise<T> {
  const deadline = Date.now() + ms;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const value = await probe();
      if (value !== undefined) return value;
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last}` : ""}`);
}

/** The shell's commands, in Node: the same CLI, files, and loopback-only HTTP with the node's cookie. */
function harnessHost(root: string): HostOps & { nodeHTTP: NodeHTTP } {
  const topologyFile = join(root, "lattice.json");
  return {
    async lattice(verb) {
      try {
        const { stdout, stderr } = await run(join(bin!, "lattice"), [verb, "--root", root]);
        return { code: 0, stdout, stderr };
      } catch (e) {
        const failed = e as { code?: number; stdout?: string; stderr?: string };
        return { code: typeof failed.code === "number" ? failed.code : 1, stdout: failed.stdout ?? "", stderr: failed.stderr ?? String(e) };
      }
    },
    async readTopology() {
      try {
        return JSON.parse(readFileSync(topologyFile, "utf8")) as Topology;
      } catch {
        return null;
      }
    },
    async writeTopology(topology) {
      writeFileSync(topologyFile, JSON.stringify(topology, null, 2));
    },
    async nodeHTTP(method, path, body) {
      const { rpc } = JSON.parse(readFileSync(topologyFile, "utf8")) as Topology;
      const response = await fetch(`http://127.0.0.1:${rpc}${path}`, {
        method,
        headers: {
          Authorization: "Basic " + btoa(readFileSync(join(root, "chains", "Nexus", ".cookie"), "utf8").trim()),
          ...(body === null ? {} : { "Content-Type": "application/json" }),
        },
        ...(body === null ? {} : { body }),
      });
      return { status: response.status, body: await response.text() };
    },
  };
}

test("the app's node layer: init, start, fund by mining, send with a custom fee, see inclusion, stop", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-e2e-"));
  const host = harnessHost(root);
  const manager = new NodeManager(host);
  try {
    // First run: `lattice init`, then a private node (no peers) on free ports.
    const prefs = await manager.prefs();
    assert.equal(prefs.peers, "default", "init leaves the node's built-in peers");
    await manager.configure({ ...prefs, rpc: await freePort(), listen: await freePort(), peers: "none" });
    assert.deepEqual((await host.readTopology())?.peers, [], "peers: [] = --no-default-peers");
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

    const mine = async (blocks: number) => {
      const target = ((await reads.chainInfo()).height ?? 0n) + BigInt(blocks);
      await until(async () => {
        await run(join(bin!, "lattice-mining-coordinator"), [
          "--node", url, "--rpc-cookie-file", join(root, "chains", "Nexus", ".cookie"), "--workers", "2", "--recipient", `Nexus=${from}`, "--once", "--no-stale-probe",
        ]);
        return ((await reads.chainInfo()).height ?? 0n) >= target || undefined;
      }, `height ${target}`);
    };
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

    const [nexus] = await manager.status();
    assert.ok(nexus!.height! >= included.height, "status reports the synced height");

    // Pairing an extension: its origin is allowed (the node restarts with a
    // fresh cookie); with that cookie, requests from that origin are served,
    // any other origin is refused.
    const origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
    const oldCookie = readFileSync(join(root, "chains", "Nexus", ".cookie"), "utf8");
    await manager.allowOrigin(origin);
    const cookie = await until(async () => {
      const c = readFileSync(join(root, "chains", "Nexus", ".cookie"), "utf8");
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
    await host.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
