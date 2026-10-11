// The node-management layer over scripted host operations: first-run
// scaffolding, settings <-> lattice.json, restart-to-apply, status parsing.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  NodeManager, LifecycleError, checkMining, checkPrefs, parseMiningStatus, parseStatus, prefsOf, withPrefs, extensionOrigin,
  type HostOps, type LifecycleVerb, type Topology, type NodePrefs,
} from "../src/node-manager.ts";
import { miningChoices } from "../src/node-panel.ts";

const PEER = `${"ab".repeat(32)}@node.example.org:4001`;

function scripted(initial: Topology | null = null, running = false) {
  const calls: LifecycleVerb[] = [];
  const state = { topology: initial, running, mining: false };
  const host: HostOps = {
    async lattice(verb) {
      calls.push(verb);
      if (verb === "init") {
        if (state.topology) return { code: 1, stdout: "", stderr: "lattice.json already exists; edit it directly" };
        state.topology = { listen: 4001, rpc: 8080 };
      }
      if (verb === "up") state.running = true;
      if (verb === "down") state.running = false;
      if (verb === "mine-start") state.mining = true;
      if (verb === "mine-stop") state.mining = false;
      const stdout = verb === "status"
        ? (state.running ? "Nexus: active height=12 tip=bafyreiggtg4ezifboye… mempool=2\n" : "Nexus: down\n")
        : verb === "mine-status"
          ? `mining: ${state.mining ? "running (pid 4242)" : "stopped"}\nchain:  Nexus and configured hosted descendants\n`
          : "";
      return { code: 0, stdout, stderr: "" };
    },
    readTopology: async () => (state.topology ? structuredClone(state.topology) : null),
    writeTopology: async (t) => { state.topology = structuredClone(t); },
  };
  return { host, calls, state };
}

test("first run scaffolds the host with `lattice init` once, on the app's own ports; Nexus peers default to the node's built-ins", async () => {
  const { host, calls } = scripted();
  const manager = new NodeManager(host);
  // Its own ports, clear of a CLI host's 8080/4001 on the same machine.
  assert.equal(await manager.rpcURL(), "http://127.0.0.1:18080");
  assert.deepEqual(await manager.prefs(), { rpc: 18080, listen: 14001, peers: "default", hostedChains: [] });
  assert.deepEqual(calls, ["init"]);
});

test("settings map onto lattice.json and keep fields the app does not manage", () => {
  const base: Topology = { listen: 4001, rpc: 8080, minRelayFee: 5, mine: { workers: 2 } };
  const none = withPrefs(base, { rpc: 18080, listen: 14001, peers: "none", hostedChains: ["Nexus/testnet"] });
  assert.deepEqual(none, { listen: 14001, rpc: 18080, minRelayFee: 5, mine: { workers: 2 }, peers: [], hostedChains: ["Nexus/testnet"] });
  assert.deepEqual(prefsOf(none).peers, "none");
  const custom = withPrefs(none, { rpc: 18080, listen: 14001, peers: [PEER], hostedChains: [] });
  assert.deepEqual(custom.peers, [PEER]);
  assert.equal("hostedChains" in custom, false);
  const back = withPrefs(custom, { ...prefsOf(custom), peers: "default" });
  assert.equal("peers" in back, false, "default = no peers key = the node's built-in peers");
});

test("settings are checked before they are written", () => {
  const ok: NodePrefs = { rpc: 8080, listen: 4001, peers: "default", hostedChains: [] };
  assert.equal(checkPrefs(ok), undefined);
  assert.match(checkPrefs({ ...ok, rpc: 0 })!, /Ports/);
  assert.match(checkPrefs({ ...ok, rpc: 70000 })!, /Ports/);
  assert.match(checkPrefs({ ...ok, listen: 8080 })!, /differ/);
  assert.match(checkPrefs({ ...ok, peers: [] })!, /at least one/);
  assert.match(checkPrefs({ ...ok, peers: ["host:4001"] })!, /Not a peer/);
  assert.equal(checkPrefs({ ...ok, peers: [PEER] }), undefined);
  assert.equal(checkPrefs({ ...ok, hostedChains: ["Nexus/a", "Nexus/a/b"] }), undefined);
  assert.match(checkPrefs({ ...ok, hostedChains: ["Nexus/a/b"] })!, /after its parent/);
  assert.match(checkPrefs({ ...ok, hostedChains: ["Other/a"] })!, /after its parent/);
  assert.match(checkPrefs({ ...ok, hostedChains: ["Nexus/a", "Nexus/a"] })!, /new/);
});

test("saving settings restarts a running node, and leaves a stopped one stopped", async () => {
  const running = scripted({ listen: 4001, rpc: 8080 }, true);
  await new NodeManager(running.host).configure({ rpc: 8080, listen: 4001, peers: "none", hostedChains: [] });
  assert.deepEqual(running.calls, ["status", "down", "up"]);
  assert.deepEqual(running.state.topology?.peers, []);

  const stopped = scripted({ listen: 4001, rpc: 8080 });
  await new NodeManager(stopped.host).configure({ rpc: 9090, listen: 4001, peers: "default", hostedChains: [] });
  assert.deepEqual(stopped.calls, ["status"]);
  assert.equal(stopped.state.topology?.rpc, 9090);
  await assert.rejects(new NodeManager(stopped.host).configure({ rpc: 1, listen: 1, peers: "default", hostedChains: [] }), /differ/);
});

test("a failing lifecycle verb surfaces the CLI's own words", async () => {
  const { host } = scripted({ listen: 4001, rpc: 8080 });
  host.lattice = async () => ({ code: 1, stdout: "", stderr: "lattice-node not found beside lattice or in /usr/local/bin\n" });
  const error = await new NodeManager(host).start().then(() => undefined, (e: unknown) => e);
  assert.ok(error instanceof LifecycleError);
  assert.match((error as Error).message, /lattice up failed \(exit 1\): lattice-node not found/);
});

test("`lattice status` lines parse into per-chain state and heights", () => {
  assert.deepEqual(parseStatus([
    "Nexus: active height=1234 tip=bafyreiggtg4ezifboye… mempool=3",
    "Nexus/testnet: running, rpc unreachable",
    "Nexus/other: down",
    "Nexus/new: syncing height=- tip=-… mempool=0",
    "",
  ].join("\n")), [
    { path: "Nexus", state: "active", height: 1234n, tip: "bafyreiggtg4ezifboye", mempool: 3 },
    { path: "Nexus/testnet", state: "unreachable" },
    { path: "Nexus/other", state: "down" },
    { path: "Nexus/new", state: "syncing", mempool: 0 },
  ]);
});

test("one chain list: the wallet's chains are the node's hosted chains, and adding one hosts it", async () => {
  const running = scripted({ listen: 4001, rpc: 8080, hostedChains: ["Nexus/a"] }, true);
  const manager = new NodeManager(running.host);
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a"]);
  await manager.addHostedChain("Nexus/a/b");
  assert.deepEqual(running.state.topology?.hostedChains, ["Nexus/a", "Nexus/a/b"]);
  assert.deepEqual(running.calls.slice(-3), ["status", "down", "up"], "a running node restarts to host it");
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a", "Nexus/a/b"]);
  const before = running.calls.length;
  await manager.addHostedChain("Nexus/a");
  await manager.addHostedChain("Nexus");
  assert.equal(running.calls.length, before, "already hosted: nothing to do");
  await assert.rejects(manager.addHostedChain("Nexus/x/y"), /after its parent/);
});

test("pairing allows an extension origin in lattice.json (restarting a running node), once", async () => {
  assert.equal(extensionOrigin("abcdefghijklmnopabcdefghijklmnop"), "chrome-extension://abcdefghijklmnopabcdefghijklmnop");
  assert.equal(extensionOrigin(" chrome-extension://abcdefghijklmnopabcdefghijklmnop "), "chrome-extension://abcdefghijklmnopabcdefghijklmnop");
  for (const bad of ["", "abc", "https://evil.example", "chrome-extension://x/y", "chrome-extension://"]) {
    assert.equal(extensionOrigin(bad), undefined, bad);
  }
  const running = scripted({ listen: 4001, rpc: 8080, minRelayFee: 5 }, true);
  const manager = new NodeManager(running.host);
  const origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
  await manager.allowOrigin(origin);
  assert.deepEqual(running.state.topology, { listen: 4001, rpc: 8080, minRelayFee: 5, rpcAllowedOrigins: [origin] });
  assert.deepEqual(running.calls, ["status", "down", "up"]);
  await manager.allowOrigin(origin);
  assert.deepEqual(running.calls.length, 3, "already allowed");
  assert.deepEqual(await manager.allowedOrigins(), [origin]);
  // Node settings keep it.
  await manager.configure({ ...(await manager.prefs()), peers: "none" });
  assert.deepEqual(running.state.topology?.rpcAllowedOrigins, [origin]);
});

const MINER = "bafyreifrbp6x4ncfbqakwfcwfqkl7qhbsdeygsakjxixekwkdprbomyut4";

test("mining settings are checked against the chains this node hosts", () => {
  const chains = ["Nexus", "Nexus/testnet"];
  assert.equal(checkMining({ workers: 2, recipients: { Nexus: MINER, "Nexus/testnet": MINER } }, chains), undefined);
  assert.equal(checkMining({ workers: 1, recipients: {} }, chains), undefined, "paying no one is allowed: it burns");
  assert.match(checkMining({ workers: 0, recipients: {} }, chains)!, /Workers/);
  assert.match(checkMining({ workers: 1.5, recipients: {} }, chains)!, /Workers/);
  assert.match(checkMining({ workers: 65, recipients: {} }, chains)!, /Workers/);
  assert.match(checkMining({ workers: 1, recipients: { "Nexus/other": MINER } }, chains)!, /does not host Nexus\/other/);
  assert.match(checkMining({ workers: 1, recipients: { Nexus: "someone" } }, chains)!, /Not an address/);
  // The screen's choices: a chain set to pay no one is left out, so it burns.
  assert.deepEqual(miningChoices(" 3 ", { Nexus: MINER, "Nexus/testnet": "" }), { workers: 3, recipients: { Nexus: MINER } });
});

test("`lattice mine status` says whether the loop runs", () => {
  assert.deepEqual(parseMiningStatus("mining: running (pid 81)\nchain:  Nexus and configured hosted descendants\nrecipient: Nexus=bafy\n"), { running: true, pid: 81 });
  assert.deepEqual(parseMiningStatus("mining: stopped\nrecipients: none configured (rewards and fees burn)\n"), { running: false });
});

test("mining: nothing to ask the CLI before it is set up; it needs the node; saving restarts a running loop and keeps the operator's fields", async () => {
  const { host, calls, state } = scripted({ listen: 14001, rpc: 18080, mine: undefined });
  delete state.topology!.mine;
  const manager = new NodeManager(host);
  // `lattice mine status` fails without a mine section, so it is not asked.
  assert.deepEqual(await manager.miningStatus(), { running: false });
  assert.equal(await manager.miningPrefs(), undefined);
  assert.deepEqual(calls, []);
  await assert.rejects(manager.startMining(), /Choose who mining pays first/);

  state.topology!.mine = { worker: "/opt/gpu-miner", batchSize: 7 };
  await manager.configureMining({ workers: 2, recipients: { Nexus: MINER } });
  assert.deepEqual(state.topology!.mine, { worker: "/opt/gpu-miner", batchSize: 7, workers: 2, recipients: { Nexus: MINER } });
  assert.deepEqual(await manager.miningPrefs(), { workers: 2, recipients: { Nexus: MINER } });
  await assert.rejects(manager.startMining(), /Start the node first/);
  assert.equal(state.mining, false);

  await manager.start();
  await manager.startMining();
  assert.deepEqual(await manager.miningStatus(), { running: true, pid: 4242 });
  calls.length = 0;
  await manager.configureMining({ workers: 3, recipients: {} });
  assert.deepEqual(calls, ["mine-status", "mine-stop", "mine-start"], "the loop reads its settings when it starts");
  assert.equal(state.topology!.mine!.workers, 3);
  await assert.rejects(manager.configureMining({ workers: 1, recipients: { "Nexus/testnet": MINER } }), /does not host/);

  // Stopping the node stops the loop first: it mines on the node.
  calls.length = 0;
  await manager.stop();
  assert.deepEqual(calls, ["mine-status", "mine-stop", "down"]);
  assert.equal(state.mining, false);
});
