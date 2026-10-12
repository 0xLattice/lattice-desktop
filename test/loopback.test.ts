// The webview's only network path: the shell's loopback node_http, adapted
// to the SDK's fetch, serving the app's own node and nothing else.

import { test } from "node:test";
import assert from "node:assert/strict";
import { loopbackFetch, type NodeHTTP } from "../src/loopback.ts";
import { reader, submitter } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { aloneOn, chosenChains, hostableChains, chainWords, nodeLine, nodeNotices, peersLine, rowCells, parsePeersField, withSync } from "../src/node-panel.ts";
import type { ChainStatus, NodeManager } from "../src/node-manager.ts";
import { lockDue } from "../src/app.ts";

const BASE = "http://127.0.0.1:18080";

test("SDK reads and submits travel through node_http as path + query", async () => {
  const calls: [string, string, string | null][] = [];
  const call: NodeHTTP = async (method, path, body) => {
    calls.push([method, path, body]);
    if (path.startsWith("/api/state/account/")) return { status: 200, body: JSON.stringify({ owner: "bafyalice", balance: "42", nonce: "7" }) };
    if (path === "/transactions") return { status: 200, body: JSON.stringify({ transactionCID: "bafytx" }) };
    return { status: 404, body: "{}" };
  };
  const fetch = loopbackFetch(BASE, call);
  const account = await reader(BASE, ["Nexus"], fetch).account("bafyalice");
  assert.equal(account.balance, 42n);
  assert.deepEqual(calls[0], ["GET", "/api/state/account/bafyalice?chainPath=Nexus", null]);
  const sent = await submitter(BASE, fetch).submit({ transaction: { signatures: {}, body: {} } } as never);
  assert.equal(sent.transactionCID, "bafytx");
  assert.equal(calls[1]![0], "POST");
  assert.equal(calls[1]![1], "/transactions");
});

test("anything but the app's own node is refused before the shell is asked", async () => {
  let asked = 0;
  const fetch = loopbackFetch(BASE, async () => { asked += 1; return { status: 200, body: "{}" }; });
  for (const url of ["http://127.0.0.1:8080/health", "https://reads.example.org/health", "http://localhost:18080/health"]) {
    await assert.rejects(fetch(url), /not this app's node/);
  }
  await assert.rejects(fetch(`${BASE}/health`, { method: "DELETE" }), /refused: DELETE/);
  assert.equal(asked, 0);
});

test("a chain's state is said in a user's words, not the node's", () => {
  // A chain that is caught up says nothing: its block and its peers are the whole of it.
  assert.equal(chainWords({ path: "Nexus", state: "active", height: 5000n, bestHeaderHeight: 5000n }), undefined);
  assert.deepEqual(rowCells({ path: "Nexus", state: "active", height: 1234n, mempool: 2 }, 3), { block: (1234).toLocaleString(), peers: "3" });
  assert.deepEqual(rowCells({ path: "Nexus", state: "active", height: 0n }, 0), { block: "0", peers: "no peers" });
  // Words are for what needs saying.
  assert.equal(chainWords({ path: "Nexus/testnet", state: "awaitingGenesis" }), "waiting for its first block");
  const behind = { path: "Nexus", state: "active", height: 1234n, bestHeaderHeight: 5000n };
  assert.equal(chainWords(behind), `syncing ${(1234).toLocaleString()} of ${(5000).toLocaleString()}`);
  assert.deepEqual(rowCells(behind, 3), { words: chainWords(behind), block: "", peers: "" });
  // A stopped or starting node is said once, of the node; its rows keep what they last knew and claim no peers.
  assert.deepEqual(rowCells({ path: "Nexus", state: "down" }, undefined), { block: "", peers: "" });
  assert.equal(nodeLine([{ path: "Nexus", state: "down" }, { path: "Nexus/testnet", state: "down" }]), "Stopped. Nothing can be read or sent.");
  assert.match(nodeLine([{ path: "Nexus", state: "unreachable" }]), /^Starting\./);
  assert.equal(nodeLine([{ path: "Nexus", state: "active", height: 1n }, { path: "Nexus/testnet", state: "awaitingGenesis" }]), "Running");
  assert.equal(parsePeersField("default", "x"), "default");
  assert.equal(parsePeersField("none", ""), "none");
  assert.deepEqual(parsePeersField("custom", " a@h:1\n b@h:2, "), ["a@h:1", "b@h:2"]);
});

test("node settings offer the chains the node can see, and a typed path for one it cannot", async () => {
  const children: Record<string, string[]> = { Nexus: ["Nexus/testnet", "Nexus/games"], "Nexus/testnet": ["Nexus/testnet/payments"] };
  const reads = { children: async (path: string) => { if (path === "Nexus/gone") throw new Error("not hosted"); return children[path] ?? []; } };
  // Hosted chains, and the children of Nexus and of each hosted chain; parents first.
  assert.deepEqual(await hostableChains(["Nexus/testnet", "Nexus/gone"], reads), ["Nexus/games", "Nexus/gone", "Nexus/testnet", "Nexus/testnet/payments"]);
  assert.deepEqual(await hostableChains([], reads), ["Nexus/games", "Nexus/testnet"]);
  assert.deepEqual(chosenChains(["Nexus/testnet/payments", "Nexus/testnet"], " Nexus/other  Nexus/testnet "), ["Nexus/other", "Nexus/testnet", "Nexus/testnet/payments"]);
  assert.deepEqual(chosenChains([], ""), []);
});

test("sync progress reads each running chain's best header height; a stopped or failing one is left as is", async () => {
  const chains = await withSync(
    [{ path: "Nexus", state: "active", height: 10n }, { path: "Nexus/a", state: "active", height: 1n }, { path: "Nexus/b", state: "down" }],
    { health: async (path) => { if (path === "Nexus/a") throw new Error("x"); return { height: 10n, bestHeaderHeight: 99n }; } },
  );
  assert.deepEqual(chains, [
    { path: "Nexus", state: "active", height: 10n, bestHeaderHeight: 99n },
    { path: "Nexus/a", state: "active", height: 1n },
    { path: "Nexus/b", state: "down" },
  ]);
});

test("a chain's peers are its own: the row counts them, and the home screen says which chain has none", async () => {
  assert.deepEqual([peersLine(0), peersLine(1), peersLine(1234)], ["no peers", "1 peer", `${(1234).toLocaleString()} peers`]);

  const chains: ChainStatus[] = [
    { path: "Nexus", state: "active", height: 10n },
    { path: "Nexus/testnet", state: "active", height: 5n },
  ];
  const manager = {
    status: async () => chains,
    prefs: async () => ({ rpc: 1, listen: 2, peers: "default", chainPeers: {}, hostedChains: ["Nexus/testnet"] }),
    miningStatus: async () => ({ running: false }),
  } as unknown as NodeManager;
  const notices = async (peers: Record<string, number>, chain: string) =>
    (await nodeNotices(manager, { health: async () => ({}), peers: async (path) => peers[path]! }, chain)).map((notice) => notice.text);

  // Nexus connected, the child not: said of the child, and only when it is the chain in view.
  assert.deepEqual(await notices({ Nexus: 3, "Nexus/testnet": 0 }, "Nexus/testnet"), [aloneOn("Nexus/testnet")]);
  assert.deepEqual(await notices({ Nexus: 3, "Nexus/testnet": 0 }, "Nexus"), []);
  assert.deepEqual(await notices({ Nexus: 3, "Nexus/testnet": 2 }, "Nexus/testnet"), []);
  // The node itself cut off: said once, as the node's, whichever chain is in view.
  for (const chain of ["Nexus", "Nexus/testnet"]) {
    assert.deepEqual(await notices({ Nexus: 0, "Nexus/testnet": 0 }, chain), ["Your node has no peers yet, so it cannot see the network. It keeps trying."]);
  }
});

test("the wallet locks when unused for its time, and at once when the computer has slept", () => {
  const TEN_MINUTES = 10 * 60 * 1000;
  const start = 1_000_000;
  // In use, with the clock ticking every second: not due.
  assert.equal(lockDue(start + 1000, start, start, TEN_MINUTES), false);
  assert.equal(lockDue(start + TEN_MINUTES - 1, start + TEN_MINUTES - 1001, start, TEN_MINUTES), false);
  // Unused for the whole time: due.
  assert.equal(lockDue(start + TEN_MINUTES, start + TEN_MINUTES - 1000, start, TEN_MINUTES), true);
  // Used a second before the lid closed, and the next beat comes a minute later: due, whatever the use.
  assert.equal(lockDue(start + 61_000, start + 1000, start + 1000, TEN_MINUTES), true);
  // A beat that was only late (a busy machine) is not a sleep.
  assert.equal(lockDue(start + 20_000, start + 1000, start + 1000, TEN_MINUTES), false);
});
