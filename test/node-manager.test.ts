// The node-management layer over scripted host operations: first-run
// scaffolding, settings <-> the CLI's operator fragment over lattice.json,
// hosting one path at a time, restarting only when the node says a change
// waits for it, status parsing.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  NodeManager, LifecycleError, changesOf, checkMining, checkPrefs, parseMiningLog, parseMiningStatus, parseStatus, prefsOf, searcherOf, extensionOrigin,
  type HostOps, type MinerSettings, type Topology, type NodePrefs,
} from "../src/node-manager.ts";
import type { NodeHTTP } from "../src/loopback.ts";
import { alone, miningChoices, nodeNotices } from "../src/node-panel.ts";

const PEER = `${"ab".repeat(32)}@node.example.org:4001`;
const OTHER = `${"cd".repeat(32)}@198.51.100.2:4001`;

type Layer = { hostedChains?: string[]; peers?: string[] | Record<string, string[]>; [key: string]: unknown };
const peersOf = (layer: Layer | null | undefined): Record<string, string[]> =>
  Array.isArray(layer?.peers) ? { Nexus: layer.peers } : { ...(layer?.peers ?? {}) };

/** `chains` with every ancestor they need, parent first, as the node hosts them. */
function withAncestors(chains: string[]): string[] {
  const tree = new Set<string>();
  for (const chain of chains) {
    const parts = chain.split("/");
    for (let depth = 2; depth <= parts.length; depth++) tree.add(parts.slice(0, depth).join("/"));
  }
  return [...tree].sort((a, b) => a.split("/").length - b.split("/").length || (a < b ? -1 : 1));
}

/**
 * A host whose CLI keeps the layers as the node does: lattice.json, the
 * operator fragment (which `config set|unset` and `child host|unhost`
 * change), and other fragments; merged, hosted chains and peers are each
 * layer's together, and other keys the last layer's that sets them.
 */
function scripted(initial: Topology | null = null, running = false) {
  const calls: string[] = [];
  const state = {
    topology: initial as Layer | null, running, mining: false, gpuWorker: null as string | null, gpuUsable: true, log: "",
    operator: {} as Layer, miner: null as MinerSettings | null, startedAt: 1,
    /** Other fragments, by name. */
    others: {} as Record<string, Layer>,
    /** Keys changed while it ran that it takes only at a restart. */
    pending: new Set<string>(),
    error: undefined as string | undefined,
  };
  const layers = (): [string, Layer][] => [["lattice", state.topology ?? {}], ...Object.entries(state.others), ["operator", state.operator]];
  const merged = (): Topology => {
    const out: Layer = {};
    const peers: Record<string, string[]> = {};
    const listed: string[] = [];
    for (const [, layer] of layers()) {
      const { hostedChains, peers: _, ...rest } = layer;
      Object.assign(out, rest);
      listed.push(...(hostedChains ?? []));
      for (const [chain, list] of Object.entries(peersOf(layer))) peers[chain] = [...new Set([...(peers[chain] ?? []), ...list])];
    }
    if (listed.length) out.hostedChains = withAncestors(listed);
    if (Object.keys(peers).length) out.peers = peers;
    return structuredClone(out) as Topology;
  };
  const said = (code: number, stderr = "") => ({ code, stdout: "", stderr });
  const host: HostOps = {
    async lattice(verb) {
      calls.push(verb);
      if (verb === "init") {
        if (state.topology) return said(1, "lattice.json already exists; edit it directly");
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
    startNode: async () => { calls.push("start"); state.running = true; state.startedAt += 1; state.pending.clear(); },
    stopNode: async () => { calls.push("stop"); state.running = false; },
    nodeProcess: async () => ({ running: state.running, wanted: state.running, error: state.error ?? null }),
    readConfig: async () => (state.topology ? merged() : null),
    async setConfig(set, unset) {
      if (Object.keys(set).length) calls.push(`set ${Object.keys(set).join(",")}`);
      if (unset.length) calls.push(`unset ${unset.join(",")}`);
      for (const [key, value] of [...Object.entries(set), ...unset.map((key) => [key, undefined] as const)]) {
        const chain = key.startsWith("peers.") ? key.slice("peers.".length) : undefined;
        if (chain === undefined) state.operator[key] = value;
        else {
          const peers = peersOf(state.operator);
          if (value === undefined) delete peers[chain];
          else peers[chain] = value as string[];
          state.operator.peers = peers;
        }
        // A child chain's peers are taken at once; the rest at the next start.
        if (state.running && !key.startsWith("peers.Nexus/")) state.pending.add(key);
      }
      return said(0);
    },
    async hostChain(chain, hosted) {
      calls.push(`${hosted ? "host" : "unhost"} ${chain}`);
      const own = state.operator.hostedChains ?? [];
      if (hosted) {
        state.operator.hostedChains = [...own, chain];
        return said(0);
      }
      if (own.includes(chain)) {
        state.operator.hostedChains = own.filter((c) => c !== chain);
        return said(0);
      }
      const layer = layers().find(([, l]) => l.hostedChains?.includes(chain))?.[0];
      if (layer) return said(1, `Error: ${chain} is listed by ${layer}, not by operator: change it there\n`);
      const under = (merged().hostedChains ?? []).filter((c) => c.startsWith(chain + "/"));
      return said(1, `Error: ${chain} is not listed: it is hosted for ${under.join(", ")}; unhost those\n`);
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
    if (method === "GET" && path === "/config") return { status: 200, body: JSON.stringify({ spec: merged(), status: { pendingRestart: [...state.pending] } }) };
    if (method === "GET" && path === "/status") return { status: 200, body: JSON.stringify({ startedAt: state.startedAt }) };
    if (method === "POST" && path === "/restart") {
      calls.push("restart");
      state.startedAt += 1;
      state.pending.clear();
      return { status: 202, body: "{}" };
    }
    return { status: 404, body: "" };
  };
  return { host, http, calls, state, merged };
}

test("first run scaffolds the host with `lattice init` once, on the app's own ports; Nexus peers default to the node's built-ins", async () => {
  const { host, http, calls, state } = scripted();
  const manager = new NodeManager(host, http);
  // Its own ports, clear of a CLI host's 8080/4001 on the same machine.
  assert.equal(await manager.rpcURL(), "http://127.0.0.1:18080");
  assert.deepEqual(await manager.prefs(), { rpc: 18080, listen: 14001, peers: "default", chainPeers: {}, hostedChains: [] });
  assert.deepEqual(calls, ["init", "set rpc,listen"]);
  // In the operator fragment: lattice.json is as `lattice init` wrote it.
  assert.deepEqual([state.topology, state.operator], [{ listen: 4001, rpc: 8080 }, { rpc: 18080, listen: 14001 }]);
});

test("settings become single changes of the operator fragment: a key, a chain's peers, or one chain hosted", () => {
  const base: NodePrefs = { rpc: 18080, listen: 14001, peers: "default", chainPeers: {}, hostedChains: ["Nexus/a", "Nexus/a/b"] };
  assert.deepEqual(changesOf(base, base), { set: {}, unset: [], host: [], unhost: [] });
  assert.deepEqual(changesOf(base, { ...base, rpc: 9090, peers: "none" }), { set: { rpc: 9090, "peers.Nexus": [] }, unset: [], host: [], unhost: [] });
  assert.deepEqual(changesOf({ ...base, peers: [PEER] }, base).unset, ["peers.Nexus"], "default: the operator's list unset");
  // One path each, never the tree: an ancestor stays implied.
  assert.deepEqual(changesOf(base, { ...base, hostedChains: [...base.hostedChains, "Nexus/a/b/c"] }).host, ["Nexus/a/b/c"]);
  assert.deepEqual(changesOf(base, { ...base, hostedChains: [] }).unhost, ["Nexus/a/b", "Nexus/a"], "deepest first");
  // A child chain's list set whole, or unset; a chain no longer hosted takes its list with it.
  const withPeers = { ...base, chainPeers: { "Nexus/a/b": [OTHER] } };
  assert.deepEqual(changesOf(base, withPeers).set, { "peers.Nexus/a/b": [OTHER] });
  assert.deepEqual(changesOf(withPeers, base).unset, ["peers.Nexus/a/b"]);
  assert.deepEqual(changesOf(withPeers, { ...withPeers, hostedChains: ["Nexus/a"] }), { set: {}, unset: ["peers.Nexus/a/b"], host: [], unhost: ["Nexus/a/b"] });
});

test("each chain has its own peers: Nexus's list as before, a child's as extra peers for that chain", () => {
  // An array is Nexus's list, as in every file written before chains had their own.
  assert.deepEqual(prefsOf({ listen: 1, rpc: 2, peers: [PEER], hostedChains: ["Nexus/testnet"] }), {
    rpc: 2, listen: 1, peers: [PEER], chainPeers: {}, hostedChains: ["Nexus/testnet"],
  });
  // Keyed by chain: Nexus left out keeps the built-in peers; an empty child list is no list.
  const keyed = prefsOf({ listen: 1, rpc: 2, peers: { "Nexus/testnet": [OTHER], "Nexus/other": [] }, hostedChains: ["Nexus/testnet", "Nexus/other"] });
  assert.deepEqual([keyed.peers, keyed.chainPeers], ["default", { "Nexus/testnet": [OTHER] }]);
  assert.deepEqual(prefsOf({ listen: 1, rpc: 2, peers: { Nexus: [] } }).peers, "none");

  const prefs: NodePrefs = { rpc: 2, listen: 1, peers: "default", chainPeers: { "Nexus/testnet": [OTHER] }, hostedChains: ["Nexus/testnet"] };
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

test("a running node restarts when it says a change waits for one (asked, then waited for); a stopped one stays stopped", async () => {
  const running = scripted({ listen: 4001, rpc: 8080 }, true);
  const manager = new NodeManager(running.host, running.http);
  await manager.configure({ rpc: 8080, listen: 4001, peers: "none", chainPeers: {}, hostedChains: [] });
  assert.deepEqual(running.calls, ["set peers.Nexus", "restart"]);
  assert.deepEqual([running.state.operator, running.state.topology], [{ peers: { Nexus: [] } }, { listen: 4001, rpc: 8080 }]);
  // Nothing changed: nothing written, nothing restarted, however the layers order their keys.
  running.state.topology!.peers = { "Nexus/a": [PEER], Nexus: [] };
  running.state.operator.hostedChains = ["Nexus/a"];
  running.calls.length = 0;
  await manager.configure(await manager.prefs());
  assert.deepEqual(running.calls, []);
  // A restart the node refuses (its configuration on disk would not start it) is said.
  const refusing = new NodeManager(running.host, async (method, path, body) =>
    method === "POST" ? { status: 409, body: "operatorListen: bad address" } : running.http(method, path, body));
  await assert.rejects(refusing.configure({ ...(await manager.prefs()), listen: 4002 }), /would not restart to take it: operatorListen/);

  const stopped = scripted({ listen: 4001, rpc: 8080 });
  await new NodeManager(stopped.host, stopped.http).configure({ rpc: 9090, listen: 4001, peers: "default", chainPeers: {}, hostedChains: [] });
  assert.deepEqual(stopped.calls, ["set rpc"]);
  assert.equal(stopped.merged().rpc, 9090);
  await assert.rejects(new NodeManager(stopped.host, stopped.http).configure({ rpc: 1, listen: 1, peers: "default", chainPeers: {}, hostedChains: [] }), /differ/);
});

test("a hosted chain and its own peers are taken live: no restart", async () => {
  const running = scripted({ listen: 4001, rpc: 8080 }, true);
  const manager = new NodeManager(running.host, running.http);
  await manager.configure({ ...(await manager.prefs()), hostedChains: ["Nexus/a"], chainPeers: { "Nexus/a": [OTHER] } });
  assert.deepEqual(running.calls, ["host Nexus/a", "set peers.Nexus/a"]);
  await manager.configure({ ...(await manager.prefs()), chainPeers: {} });
  assert.deepEqual(running.calls.slice(2), ["unset peers.Nexus/a"]);
  // A peer another layer lists stays: said, not hidden.
  running.state.others = { deploy: { peers: { "Nexus/a": [PEER] } } };
  await assert.rejects(manager.configure({ ...(await manager.prefs()), chainPeers: {} }), /another configuration fragment lists more peers/);
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
  assert.deepEqual(await manager.nodeProcess(), { running: false, wanted: false, error: state.error });
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

test("one chain list: the wallet's chains are the node's hosted chains, and adding one hosts that path alone, at once", async () => {
  // lattice.json's list stays lattice.json's: the base layer.
  const running = scripted({ listen: 4001, rpc: 8080, hostedChains: ["Nexus/a"] }, true);
  const manager = new NodeManager(running.host, running.http);
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a"]);
  await manager.addHostedChain("Nexus/a/b");
  assert.deepEqual([running.state.operator.hostedChains, running.state.topology?.hostedChains], [["Nexus/a/b"], ["Nexus/a"]]);
  assert.deepEqual(running.calls, ["host Nexus/a/b"], "no restart");
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a", "Nexus/a/b"]);
  const before = running.calls.length;
  await manager.addHostedChain("Nexus/a");
  await manager.addHostedChain("Nexus");
  assert.equal(running.calls.length, before, "already hosted: nothing to do");
  await assert.rejects(manager.addHostedChain("Nexus/x/y"), /after its parent/);

  // A chain another layer lists is the node's too, and stays that layer's: the CLI says where.
  running.state.others = { deploy: { hostedChains: ["Nexus/d"] } };
  assert.deepEqual(await manager.chains(), ["Nexus", "Nexus/a", "Nexus/d", "Nexus/a/b"]);
  await manager.addHostedChain("Nexus/d/e");
  assert.deepEqual(running.state.operator.hostedChains, ["Nexus/a/b", "Nexus/d/e"]);
  await assert.rejects(manager.configure({ ...(await manager.prefs()), hostedChains: ["Nexus/a", "Nexus/a/b"] }), /Nexus\/d is still hosted: Nexus\/d is listed by deploy, not by operator: change it there/);
  assert.deepEqual(running.state.operator.hostedChains, ["Nexus/a/b"], "what it could, it did");

  // Stopped, the same: an ancestor only implied stays implied, never written.
  const stopped = scripted({ listen: 4001, rpc: 8080 });
  stopped.state.operator.hostedChains = ["Nexus/b/c"];
  const offline = new NodeManager(stopped.host, stopped.http);
  assert.deepEqual(await offline.chains(), ["Nexus", "Nexus/b", "Nexus/b/c"]);
  await offline.addHostedChain("Nexus/z");
  assert.deepEqual(stopped.state.operator.hostedChains, ["Nexus/b/c", "Nexus/z"]);
  assert.deepEqual(stopped.calls, ["host Nexus/z"]);
  // Asked away while a chain under it needs it: refused before anything is written.
  await assert.rejects(offline.configure({ ...(await offline.prefs()), hostedChains: ["Nexus/b/c", "Nexus/z"] }), /Nexus\/b\/c must be .* listed after its parent/);
  assert.deepEqual(stopped.calls, ["host Nexus/z"]);
  // Taking the chain under it away takes the implied ancestor too.
  await offline.configure({ ...(await offline.prefs()), hostedChains: ["Nexus/z"] });
  assert.deepEqual(await offline.chains(), ["Nexus", "Nexus/z"]);
});

test("pairing allows an extension origin in the operator fragment (restarting a running node, which says it waits for one), once", async () => {
  assert.equal(extensionOrigin("abcdefghijklmnopabcdefghijklmnop"), "chrome-extension://abcdefghijklmnopabcdefghijklmnop");
  assert.equal(extensionOrigin(" chrome-extension://abcdefghijklmnopabcdefghijklmnop "), "chrome-extension://abcdefghijklmnopabcdefghijklmnop");
  for (const bad of ["", "abc", "https://evil.example", "chrome-extension://x/y", "chrome-extension://"]) {
    assert.equal(extensionOrigin(bad), undefined, bad);
  }
  const running = scripted({ listen: 4001, rpc: 8080, minRelayFee: 5 }, true);
  const manager = new NodeManager(running.host, running.http);
  const origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
  await manager.allowOrigin(origin);
  assert.deepEqual([running.state.topology, running.state.operator], [{ listen: 4001, rpc: 8080, minRelayFee: 5 }, { rpcAllowedOrigins: [origin] }]);
  assert.deepEqual(running.calls, ["set rpcAllowedOrigins", "restart"]);
  await manager.allowOrigin(origin);
  assert.deepEqual(running.calls.length, 2, "already allowed");
  assert.deepEqual(await manager.allowedOrigins(), [origin]);
  // The node's settings keep it.
  await manager.configure({ ...(await manager.prefs()), peers: "none" });
  assert.deepEqual(running.merged().rpcAllowedOrigins, [origin]);
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
  const { host, http, calls, state } = scripted({ listen: 14001, rpc: 18080 }, true);
  state.operator.hostedChains = ["Nexus/testnet"];
  state.miner = { worker: "cpu", workers: 1, recipients: { Nexus: MINER, "Nexus/testnet": MINER } };
  state.mining = true;
  const manager = new NodeManager(host, http);
  const prefs = await manager.prefs();
  await manager.configure({ ...prefs, chainPeers: {}, hostedChains: [] });
  assert.deepEqual(state.miner, { worker: "cpu", workers: 1, recipients: { Nexus: MINER } });
  assert.deepEqual(state.operator.hostedChains, []);
  assert.deepEqual(calls, ["unhost Nexus/testnet"], "neither the node nor the loop restarts");

  // A change that drops no recipient leaves the miner alone.
  calls.length = 0;
  await manager.configure({ ...prefs, hostedChains: [], peers: "none" });
  assert.deepEqual(calls, ["set peers.Nexus", "restart"]);

  // An operator's tuning entry for the chain is theirs to remove: say so, and write nothing.
  state.operator.hostedChains = ["Nexus/testnet"];
  state.miner = { recipients: {}, minWork: { "Nexus/testnet": "2^20" } };
  calls.length = 0;
  await assert.rejects(manager.configure({ ...prefs, chainPeers: {}, hostedChains: [] }), /miner\.json's minWork names Nexus\/testnet/);
  assert.deepEqual([state.operator.hostedChains, calls], [["Nexus/testnet"], []]);
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
