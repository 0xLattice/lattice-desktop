// The node-management layer over scripted host operations: first-run
// scaffolding, settings <-> lattice.json and the operator fragment, hosting
// live and restart-to-apply, status parsing.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  NodeManager, LifecycleError, checkMining, checkPrefs, parseMiningLog, parseMiningStatus, parseStatus, prefsOf, searcherOf, withAncestors, withPrefs, extensionOrigin,
  type HostOps, type MinerSettings, type Topology, type NodePrefs,
} from "../src/node-manager.ts";
import type { NodeHTTP } from "../src/loopback.ts";
import { alone, miningChoices, nodeNotices } from "../src/node-panel.ts";

const PEER = `${"ab".repeat(32)}@node.example.org:4001`;

function scripted(initial: Topology | null = null, running = false) {
  const calls: string[] = [];
  const state = {
    topology: initial, running, mining: false, gpuWorker: null as string | null, gpuUsable: true, log: "",
    operator: [] as string[], miner: null as MinerSettings | null, startedAt: 1,
    /** Chains another configuration layer lists. */
    others: {} as Record<string, string>,
    error: undefined as string | undefined,
  };
  const listed = () => [...(state.topology?.hostedChains ?? []), ...state.operator, ...Object.keys(state.others)];
  const host: HostOps = {
    async lattice(verb) {
      calls.push(verb);
      if (verb === "init") {
        if (state.topology) return { code: 1, stdout: "", stderr: "lattice.json already exists; edit it directly" };
        state.topology = { listen: 4001, rpc: 8080 };
      }
      if (verb === "mine-start") state.mining = true;
      if (verb === "mine-stop") state.mining = false;
      const stdout = verb === "status"
        ? (state.running ? "Nexus: active height=12 tip=bafyreiggtg4ezifboye… mempool=2\n" : "Nexus: down\n")
        : verb === "mine-status"
          ? `mining: ${state.mining ? "running (pid 4242)" : "stopped"}\nnode:   the node under /root\n`
          : "";
      return { code: 0, stdout, stderr: "" };
    },
    startNode: async () => { calls.push("start"); state.running = true; state.startedAt += 1; },
    stopNode: async () => { calls.push("stop"); state.running = false; },
    nodeProcess: async () => ({ running: state.running, error: state.error ?? null }),
    readTopology: async () => (state.topology ? structuredClone(state.topology) : null),
    writeTopology: async (t) => { calls.push("write"); state.topology = structuredClone(t); },
    operatorChains: async () => [...state.operator],
    async hostChains(chains) {
      calls.push(state.running ? "host (live)" : "host (file)");
      delete state.topology!.hostedChains;
      state.operator = [...chains];
    },
    readMiner: async () => (state.miner ? structuredClone(state.miner) : null),
    writeMiner: async (m) => { state.miner = structuredClone(m); },
    gpuWorker: async () => state.gpuWorker,
    gpuWorkerUsable: async () => state.gpuUsable,
    miningLog: async () => state.log,
  };
  // The node's own answers, while it runs.
  const http: NodeHTTP = async (method, path) => {
    if (!state.running) throw new Error("node unreachable: start the node first");
    if (method === "GET" && path === "/config") {
      const tree = withAncestors(listed());
      const from = (chain: string) => [
        ...(state.topology?.hostedChains?.includes(chain) ? ["lattice"] : []),
        ...(state.operator.includes(chain) ? ["operator"] : []),
        ...(state.others[chain] ? [state.others[chain]!] : []),
      ];
      const chains = tree.map((path) => ({ path, from: from(path), ...(from(path).length ? {} : { impliedBy: tree.filter((c) => c.startsWith(path + "/")) }) }));
      return { status: 200, body: JSON.stringify({ spec: { hostedChains: tree }, status: { chains } }) };
    }
    if (method === "GET" && path === "/status") return { status: 200, body: JSON.stringify({ startedAt: state.startedAt }) };
    if (method === "POST" && path === "/restart") {
      calls.push("restart");
      state.startedAt += 1;
      return { status: 202, body: "{}" };
    }
    return { status: 404, body: "" };
  };
  return { host, http, calls, state };
}

test("first run scaffolds the host with `lattice init` once, on the app's own ports; Nexus peers default to the node's built-ins", async () => {
  const { host, http, calls } = scripted();
  const manager = new NodeManager(host, http);
  // Its own ports, clear of a CLI host's 8080/4001 on the same machine.
  assert.equal(await manager.rpcURL(), "http://127.0.0.1:18080");
  assert.deepEqual(await manager.prefs(), { rpc: 18080, listen: 14001, peers: "default", chainPeers: {}, hostedChains: [] });
  assert.deepEqual(calls, ["init", "write"]);
});

test("settings map onto lattice.json and keep fields the app does not manage; hosted chains are not lattice.json's", () => {
  const base: Topology = { listen: 4001, rpc: 8080, minRelayFee: 5 };
  const none = withPrefs(base, { rpc: 18080, listen: 14001, peers: "none", chainPeers: {}, hostedChains: ["Nexus/testnet"] });
  assert.deepEqual(none, { listen: 14001, rpc: 18080, minRelayFee: 5, peers: [] });
  assert.deepEqual(prefsOf(none).peers, "none");
  const custom = withPrefs(none, { rpc: 18080, listen: 14001, peers: [PEER], chainPeers: {}, hostedChains: [] });
  assert.deepEqual(custom.peers, [PEER]);
  assert.equal("hostedChains" in custom, false);
  // The tree a node hosts for its listed chains: each with every ancestor, parent first.
  assert.deepEqual(withAncestors(["Nexus/a/b/c", "Nexus/d", "Nexus/a"]), ["Nexus/a", "Nexus/d", "Nexus/a/b", "Nexus/a/b/c"]);
  const back = withPrefs(custom, { ...prefsOf(custom), peers: "default" });
  assert.equal("peers" in back, false, "default = no peers key = the node's built-in peers");
});

test("each chain has its own peers: Nexus's list as before, a child's as extra peers for that chain", () => {
  const OTHER = `${"cd".repeat(32)}@198.51.100.2:4001`;
  // An array is Nexus's list, as in every file written before chains had their own.
  assert.deepEqual(prefsOf({ listen: 1, rpc: 2, peers: [PEER], hostedChains: ["Nexus/testnet"] }), {
    rpc: 2, listen: 1, peers: [PEER], chainPeers: {}, hostedChains: ["Nexus/testnet"],
  });
  // Keyed by chain: Nexus left out keeps the built-in peers; an empty child list is no list.
  const keyed = prefsOf({ listen: 1, rpc: 2, peers: { "Nexus/testnet": [OTHER], "Nexus/other": [] }, hostedChains: ["Nexus/testnet", "Nexus/other"] });
  assert.deepEqual([keyed.peers, keyed.chainPeers], ["default", { "Nexus/testnet": [OTHER] }]);
  assert.deepEqual(prefsOf({ listen: 1, rpc: 2, peers: { Nexus: [] } }).peers, "none");

  const base: Topology = { listen: 1, rpc: 2 };
  const prefs: NodePrefs = { rpc: 2, listen: 1, peers: "default", chainPeers: { "Nexus/testnet": [OTHER] }, hostedChains: ["Nexus/testnet"] };
  assert.deepEqual(withPrefs(base, prefs).peers, { "Nexus/testnet": [OTHER] });
  assert.deepEqual(withPrefs(base, { ...prefs, peers: "none" }).peers, { Nexus: [], "Nexus/testnet": [OTHER] });
  assert.deepEqual(withPrefs(base, { ...prefs, peers: [PEER] }).peers, { Nexus: [PEER], "Nexus/testnet": [OTHER] });
  // What is written is read back the same.
  assert.deepEqual(prefsOf(withPrefs(base, { ...prefs, peers: [PEER] }), prefs.hostedChains), { ...prefs, peers: [PEER] });
  // A chain no longer hosted takes its list with it: the node refuses a list for a chain it does not host.
  const dropped = withPrefs(base, { ...prefs, peers: [PEER], hostedChains: [] });
  assert.deepEqual(dropped.peers, [PEER], "with no child lists, Nexus's list is the array it always was");
  assert.equal("peers" in withPrefs(base, { ...prefs, hostedChains: [] }), false);

  assert.equal(checkPrefs(prefs), undefined);
  assert.match(checkPrefs({ ...prefs, chainPeers: { "Nexus/testnet": ["host:4001"] } })!, /Not a peer/);
  assert.match(checkPrefs({ ...prefs, chainPeers: { "Nexus/testnet": [] } })!, /at least one peer for Nexus\/testnet/);
});

test("settings are checked before they are written", () => {
  const ok: NodePrefs = { rpc: 8080, listen: 4001, peers: "default", chainPeers: {}, hostedChains: [] };
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

test("saving lattice.json restarts a running node (asked, then waited for), and leaves a stopped one stopped", async () => {
  const running = scripted({ listen: 4001, rpc: 8080 }, true);
  const manager = new NodeManager(running.host, running.http);
  await manager.configure({ rpc: 8080, listen: 4001, peers: "none", chainPeers: {}, hostedChains: [] });
  assert.deepEqual(running.calls, ["write", "restart"]);
  assert.deepEqual(running.state.topology?.peers, []);
  // Nothing changed: nothing written, nothing restarted, however the file orders its keys.
  running.state.topology!.peers = { "Nexus/a": [PEER], Nexus: [] };
  running.state.operator = ["Nexus/a"];
  running.calls.length = 0;
  await manager.configure(await manager.prefs());
  assert.deepEqual(running.calls, []);
  // A restart the node refuses (its configuration on disk would not start it) is said.
  const refusing = new NodeManager(running.host, async (method, path, body) =>
    method === "POST" ? { status: 409, body: "operatorListen: bad address" } : running.http(method, path, body));
  await assert.rejects(refusing.configure({ ...(await manager.prefs()), listen: 4002 }), /would not restart to take it: operatorListen/);

  const stopped = scripted({ listen: 4001, rpc: 8080 });
  await new NodeManager(stopped.host, stopped.http).configure({ rpc: 9090, listen: 4001, peers: "default", chainPeers: {}, hostedChains: [] });
  assert.deepEqual(stopped.calls, ["write"]);
  assert.equal(stopped.state.topology?.rpc, 9090);
  await assert.rejects(new NodeManager(stopped.host, stopped.http).configure({ rpc: 1, listen: 1, peers: "default", chainPeers: {}, hostedChains: [] }), /differ/);
});

test("a node that stops by itself on its configuration says why; start and stop go to the host", async () => {
  const { host, http, calls, state } = scripted({ listen: 4001, rpc: 8080 }, true);
  const manager = new NodeManager(host, http);
  // A restart that ends in exit 78: the host leaves it stopped, with its reason.
  const dying = new NodeManager(host, async (method, path, body) => {
    if (method === "POST") {
      state.running = false;
      state.error = "The node's configuration does not check, so it stays stopped:\nunknown key";
      return { status: 202, body: "{}" };
    }
    return http(method, path, body);
  });
  await assert.rejects(dying.configure({ ...(await manager.prefs()), listen: 4002 }), /does not check, so it stays stopped:\nunknown key/);
  assert.deepEqual(await manager.nodeProcess(), { running: false, error: state.error });
  await manager.start();
  await manager.stop();
  assert.deepEqual(calls.slice(-2), ["start", "stop"]);
});

test("a failing lifecycle verb surfaces the CLI's own words", async () => {
  const { host, http } = scripted({ listen: 4001, rpc: 8080 });
  host.lattice = async () => ({ code: 1, stdout: "", stderr: "lattice-node not found beside lattice or in /usr/local/bin\n" });
  const error = await new NodeManager(host, http).status().then(() => undefined, (e: unknown) => e);
  assert.ok(error instanceof LifecycleError);
  assert.match((error as Error).message, /lattice status failed \(exit 1\): lattice-node not found/);
});

test("`lattice status` lines parse into per-chain state and heights", () => {
  assert.deepEqual(parseStatus([
    "Nexus: active height=1234 tip=bafyreiggtg4ezifboye… mempool=3",
    "Nexus/testnet: running, rpc unreachable",
    "Nexus/other: down",
    "Nexus/new: syncing height=- tip=-… mempool=0",
    // What follows the chains is not one.
    "Nexus/testnet: implied by Nexus/testnet/x",
    "waiting for a restart: rpc, peers",
    "operatorListen: 10.0.0.5:8090 did not open",
    "inert: peers for Nexus/gone: not hosted",
    "",
  ].join("\n")), [
    { path: "Nexus", state: "active", height: 1234n, tip: "bafyreiggtg4ezifboye", mempool: 3 },
    { path: "Nexus/testnet", state: "unreachable" },
    { path: "Nexus/other", state: "down" },
    { path: "Nexus/new", state: "syncing", mempool: 0 },
  ]);
});

test("one chain list: the wallet's chains are the node's hosted chains, and adding one hosts it at once", async () => {
  const running = scripted({ listen: 4001, rpc: 8080, hostedChains: ["Nexus/a"] }, true);
  const manager = new NodeManager(running.host, running.http);
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a"]);
  await manager.addHostedChain("Nexus/a/b");
  // Into the operator fragment, lattice.json's own list with it; no restart.
  assert.deepEqual([running.state.operator, running.state.topology?.hostedChains], [["Nexus/a", "Nexus/a/b"], undefined]);
  assert.deepEqual(running.calls, ["host (live)"]);
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a", "Nexus/a/b"]);
  const before = running.calls.length;
  await manager.addHostedChain("Nexus/a");
  await manager.addHostedChain("Nexus");
  assert.equal(running.calls.length, before, "already hosted: nothing to do");
  await assert.rejects(manager.addHostedChain("Nexus/x/y"), /after its parent/);

  // A chain another layer lists is the node's too, and stays that layer's.
  running.state.others = { "Nexus/d": "deploy" };
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a", "Nexus/d", "Nexus/a/b"]);
  await manager.addHostedChain("Nexus/d/e");
  assert.deepEqual(running.state.operator, ["Nexus/a", "Nexus/a/b", "Nexus/d/e"]);
  await assert.rejects(manager.configure({ ...(await manager.prefs()), hostedChains: ["Nexus/a"] }), /Nexus\/d is still hosted: the deploy configuration lists it/);

  // Stopped: the operator fragment's file, and lattice.json's list, both read and both kept in one place.
  const stopped = scripted({ listen: 4001, rpc: 8080, hostedChains: ["Nexus/a"] });
  stopped.state.operator = ["Nexus/b/c"];
  const offline = new NodeManager(stopped.host, stopped.http);
  assert.deepEqual(await offline.chains(), ["Nexus", "Nexus/a", "Nexus/b", "Nexus/b/c"]);
  await offline.addHostedChain("Nexus/z");
  assert.deepEqual([stopped.state.operator, stopped.state.topology?.hostedChains], [["Nexus/a", "Nexus/b", "Nexus/b/c", "Nexus/z"], undefined]);
  assert.deepEqual(stopped.calls, ["host (file)"]);
});

test("pairing allows an extension origin in lattice.json (restarting a running node), once", async () => {
  assert.equal(extensionOrigin("abcdefghijklmnopabcdefghijklmnop"), "chrome-extension://abcdefghijklmnopabcdefghijklmnop");
  assert.equal(extensionOrigin(" chrome-extension://abcdefghijklmnopabcdefghijklmnop "), "chrome-extension://abcdefghijklmnopabcdefghijklmnop");
  for (const bad of ["", "abc", "https://evil.example", "chrome-extension://x/y", "chrome-extension://"]) {
    assert.equal(extensionOrigin(bad), undefined, bad);
  }
  const running = scripted({ listen: 4001, rpc: 8080, minRelayFee: 5 }, true);
  const manager = new NodeManager(running.host, running.http);
  const origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
  await manager.allowOrigin(origin);
  assert.deepEqual(running.state.topology, { listen: 4001, rpc: 8080, minRelayFee: 5, rpcAllowedOrigins: [origin] });
  assert.deepEqual(running.calls, ["write", "restart"]);
  await manager.allowOrigin(origin);
  assert.deepEqual(running.calls.length, 2, "already allowed");
  assert.deepEqual(await manager.allowedOrigins(), [origin]);
  // The node's settings keep it.
  await manager.configure({ ...(await manager.prefs()), peers: "none" });
  assert.deepEqual(running.state.topology?.rpcAllowedOrigins, [origin]);
});

const MINER = "bafyreifrbp6x4ncfbqakwfcwfqkl7qhbsdeygsakjxixekwkdprbomyut4";
const GPU = "/Applications/Lattice.app/Contents/MacOS/lattice-miner-gpu";

test("mining settings are checked against the chains this node hosts", () => {
  const chains = ["Nexus", "Nexus/testnet"];
  const prefs = (workers: number, recipients: Record<string, string>) => ({ searcher: "cpu" as const, workers, recipients });
  assert.equal(checkMining(prefs(2, { Nexus: MINER, "Nexus/testnet": MINER }), chains), undefined);
  assert.equal(checkMining(prefs(1, {}), chains), undefined, "paying no one is allowed: it burns");
  assert.match(checkMining(prefs(0, {}), chains)!, /Workers/);
  assert.match(checkMining(prefs(1.5, {}), chains)!, /Workers/);
  assert.match(checkMining(prefs(65, {}), chains)!, /Workers/);
  assert.match(checkMining(prefs(1, { "Nexus/other": MINER }), chains)!, /does not host Nexus\/other/);
  assert.match(checkMining(prefs(1, { Nexus: "someone" }), chains)!, /Not an address/);
  // The screen's choices: a chain set to pay no one is left out, so it burns;
  // the graphics chip is one worker whatever the field says.
  assert.deepEqual(miningChoices("cpu", " 3 ", { Nexus: MINER, "Nexus/testnet": "" }), { searcher: "cpu", workers: 3, recipients: { Nexus: MINER } });
  assert.deepEqual(miningChoices("gpu", "8", {}), { searcher: "gpu", workers: 1, recipients: {} });
});

test("what searches is read from miner.json's worker: the processor, this app's GPU worker, or the operator's own", () => {
  assert.equal(searcherOf(undefined, GPU), "cpu");
  assert.equal(searcherOf("cpu", GPU), "cpu");
  assert.equal(searcherOf(GPU, GPU), "gpu");
  // Saved before the app was moved: still this app's worker, by its name.
  assert.equal(searcherOf("/Users/me/Downloads/Lattice.app/Contents/MacOS/lattice-miner-gpu", GPU), "gpu");
  assert.equal(searcherOf("/opt/custom-miner", GPU), "custom");
  assert.equal(searcherOf(GPU, null), "custom", "a build without the GPU worker does not claim it");
});

test("`lattice mine status` says whether the loop runs", () => {
  assert.deepEqual(parseMiningStatus("mining: running (pid 81)\nchain:  Nexus and configured hosted descendants\nrecipient: Nexus=bafy\n"), { running: true, pid: 81 });
  assert.deepEqual(parseMiningStatus("mining: stopped\nrecipients: none configured (rewards and fees burn)\n"), { running: false });
});

test("mining: nothing to ask the CLI before it is set up; it needs the node; saving restarts a running loop and keeps the operator's fields", async () => {
  const { host, http, calls, state } = scripted({ listen: 14001, rpc: 18080 });
  const manager = new NodeManager(host, http);
  // `lattice mine status` fails without a mine section, so it is not asked.
  assert.deepEqual(await manager.miningStatus(), { running: false });
  assert.equal(await manager.miningPrefs(), undefined);
  assert.deepEqual(calls, []);
  await assert.rejects(manager.startMining(), /Choose who mining pays first/);

  // An operator's own worker and tuning are shown as such and kept.
  state.miner = { worker: "/opt/custom-miner", batchSize: 7 };
  assert.equal((await manager.miningPrefs())!.searcher, "custom");
  await manager.configureMining({ searcher: "custom", workers: 2, recipients: { Nexus: MINER } });
  assert.deepEqual(state.miner, { worker: "/opt/custom-miner", batchSize: 7, workers: 2, recipients: { Nexus: MINER } });
  await assert.rejects(manager.startMining(), /Start the node first/);
  assert.equal(state.mining, false);

  // The graphics chip: only in a build that carries its worker, and as one worker.
  assert.equal(await manager.canMineOnGPU(), false);
  await assert.rejects(manager.configureMining({ searcher: "gpu", workers: 1, recipients: {} }), /no graphics-chip worker/);
  state.gpuWorker = GPU;
  // And only where the worker finds a hash on this computer's chip.
  state.gpuUsable = false;
  assert.equal(await manager.canMineOnGPU(), false);
  await assert.rejects(manager.configureMining({ searcher: "gpu", workers: 1, recipients: {} }), /no graphics chip Lattice can mine on/);
  state.gpuUsable = true;
  assert.equal(await manager.canMineOnGPU(), true);
  await manager.configureMining({ searcher: "gpu", workers: 4, recipients: { Nexus: MINER } });
  assert.deepEqual(state.miner, { worker: GPU, batchSize: 7, workers: 1, recipients: { Nexus: MINER } });
  assert.deepEqual(await manager.miningPrefs(), { searcher: "gpu", workers: 1, recipients: { Nexus: MINER } });

  await manager.start();
  // Settings saved for a graphics chip that cannot be used now do not start a loop that would find nothing.
  state.gpuUsable = false;
  await assert.rejects(manager.startMining(), /no graphics chip Lattice can mine on/);
  assert.equal(state.mining, false);
  state.gpuUsable = true;
  await manager.startMining();
  assert.deepEqual(await manager.miningStatus(), { running: true, pid: 4242 });
  calls.length = 0;
  await manager.configureMining({ searcher: "cpu", workers: 3, recipients: {} });
  assert.deepEqual(calls, ["mine-status", "mine-stop", "mine-start"], "the loop reads its settings when it starts");
  assert.deepEqual(state.miner, { worker: "cpu", batchSize: 7, workers: 3, recipients: {} });
  await assert.rejects(manager.configureMining({ searcher: "cpu", workers: 1, recipients: { "Nexus/testnet": MINER } }), /does not host/);

  // Stopping the node stops the loop first: it mines on the node.
  calls.length = 0;
  await manager.stop();
  assert.deepEqual(calls, ["mine-status", "mine-stop", "stop"]);
  assert.equal(state.mining, false);
});

test("a chain no longer hosted is no longer paid: its recipient leaves miner.json, which the loop reads each round", async () => {
  const { host, http, calls, state } = scripted({ listen: 14001, rpc: 18080, hostedChains: ["Nexus/testnet"] }, true);
  state.miner = { worker: "cpu", workers: 1, recipients: { Nexus: MINER, "Nexus/testnet": MINER } };
  state.mining = true;
  const manager = new NodeManager(host, http);
  const prefs = await manager.prefs();
  await manager.configure({ ...prefs, chainPeers: {}, hostedChains: [] });
  assert.deepEqual(state.miner, { worker: "cpu", workers: 1, recipients: { Nexus: MINER } });
  assert.deepEqual([state.operator, state.topology!.hostedChains], [[], undefined]);
  assert.deepEqual(calls, ["host (live)"], "neither the node nor the loop restarts");

  // A change that drops no recipient leaves the miner alone.
  calls.length = 0;
  await manager.configure({ ...prefs, hostedChains: [], peers: "none" });
  assert.deepEqual(calls, ["write", "restart"]);

  // An operator's tuning entry for the chain is theirs to remove: say so, and write nothing.
  state.operator = ["Nexus/testnet"];
  state.miner = { recipients: {}, minWork: { "Nexus/testnet": "2^20" } };
  await assert.rejects(manager.configure({ ...prefs, chainPeers: {}, hostedChains: [] }), /miner\.json's minWork names Nexus\/testnet/);
  assert.deepEqual(state.operator, ["Nexus/testnet"]);
});

test("the mining log says how the current run is going", () => {
  const log = [
    "2026-10-11T01:00:00Z mining loop start",
    "2026-10-11T01:00:05Z accepted tip=bafyold",
    "2026-10-11T01:10:00Z mining loop start",
    "2026-10-11T01:10:02Z accepted tip=bafya",
    "2026-10-11T01:10:03Z the node refused this round's work; retrying",
    "2026-10-11T01:10:09Z accepted tip=bafyb",
    "",
  ].join("\n");
  // Only since the loop last started: an earlier run's blocks are not this one's.
  assert.deepEqual(parseMiningLog(log), { found: 2, last: "2026-10-11T01:10:09Z" });
  assert.deepEqual(parseMiningLog("2026-10-11T01:10:00Z mining loop start\n"), { found: 0 });
  assert.deepEqual(parseMiningLog(""), { found: 0 });
});

test("the home screen is told what the node is doing, with the one thing to do about it", async () => {
  const { host, http, state } = scripted({ listen: 14001, rpc: 18080 });
  const manager = new NodeManager(host, http);
  let peers = 3;
  const reads = { health: async () => ({ height: 12n, bestHeaderHeight: 12n }), peers: async () => peers };
  // Stopped: say so, and offer the way out.
  // Stopped by itself on its configuration: its reason.
  state.error = "The node's configuration does not check, so it stays stopped:\nunknown key";
  assert.equal((await nodeNotices(manager, reads, "Nexus"))[0]!.text, state.error);
  state.error = undefined;
  const [stopped] = await nodeNotices(manager, reads, "Nexus");
  assert.match(stopped!.text, /Your node is stopped/);
  assert.equal(stopped!.action?.label, "Start node");
  await stopped!.action!.run();
  assert.equal(state.running, true);
  // In sync and idle: nothing to say.
  assert.deepEqual(await nodeNotices(manager, reads, "Nexus"), []);
  // Behind: the numbers shown may be too.
  const behind = { health: async () => ({ height: 12n, bestHeaderHeight: 5000n }), peers: async () => 3 };
  assert.deepEqual((await nodeNotices(manager, behind, "Nexus")).map((n) => n.text), [`Nexus is syncing 12 of ${(5000).toLocaleString()}. Balances may be behind.`]);
  // A chain this node does not host cannot be read at all.
  assert.deepEqual((await nodeNotices(manager, reads, "Nexus/testnet")).map((n) => n.text), ["Your node does not host Nexus/testnet."]);
  // Running is not connected: with no peers it sees only itself, and says which kind of alone it is.
  peers = 0;
  assert.deepEqual((await nodeNotices(manager, reads, "Nexus")).map((n) => n.text), [alone("default")]);
  assert.match(alone("default"), /no peers yet/);
  state.topology!.peers = [];
  assert.deepEqual((await nodeNotices(manager, reads, "Nexus")).map((n) => n.text), [alone("none")]);
  assert.match(alone("none"), /set to connect to no one.*on the Node page/);
  peers = 3;
  // Mining is said wherever the person is.
  state.miner = { workers: 1, recipients: {} };
  state.mining = true;
  assert.deepEqual((await nodeNotices(manager, reads, "Nexus")).map((n) => n.text), ["Mining on this computer."]);
});
