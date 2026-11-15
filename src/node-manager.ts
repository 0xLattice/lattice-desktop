// The app's node: one host root scaffolded and read through the bundled
// `lattice` CLI (init / status, and mine start / stop / status), exactly as
// an operator would. The host is the node's process manager: it runs
// `lattice up` (which becomes the signed `lattice-node`) as its own child and
// starts it again when it exits to restart. The app talks to the node only
// over its loopback RPC. Pure logic over injected host operations, so the
// same code runs in the app and in the headless e2e harness.

import type { NodeHTTP } from "./loopback.ts";

/**
 * The node's configuration as `lattice config get` prints it: lattice.json,
 * then each fragment of config.d, merged (hosted chains and peers are the
 * union of every layer's, with the ancestors hosted chains need). The app
 * changes only the `operator` fragment, one key or chain at a time, through
 * the CLI; what lattice.json or another fragment says stays theirs.
 */
export interface Topology {
  listen: number;
  rpc: number;
  /**
   * Each chain's peers, by chain path. Nexus's list: absent = the node's
   * built-in peers, [] = none. A hosted child chain's list is extra peers to
   * try for that chain, beside those it finds through its parent. A bare
   * array is Nexus's list.
   */
  peers?: string[] | Record<string, string[]>;
  hostedChains?: string[];
  /** Browser origins allowed on the loopback operator port (e.g. a paired extension). */
  rpcAllowedOrigins?: string[];
  [field: string]: unknown;
}

/**
 * The mining loop's settings (`<root>/miner.json`, or an older lattice.json's
 * `mine` until the loop copies it there). The app sets `workers`,
 * `recipients`, and `worker` to "cpu" or the bundled GPU worker's path; the
 * rest is the operator's.
 */
export interface MinerSettings {
  worker?: string;
  workers?: number;
  recipients?: Record<string, string>;
  [field: string]: unknown;
}

export type LifecycleVerb = "init" | "status" | "mine-start" | "mine-stop" | "mine-status";

/** The node process as its host runs it. */
export interface NodeProcess {
  /** Whether its process is running now. */
  running: boolean;
  /** Whether the host keeps it running: started, and not stopped since (it starts it again after a restart or a crash). */
  wanted: boolean;
  /** Why it stopped by itself: its configuration does not check (exit 78), in its own last words. */
  error?: string | null;
}

/** What `GET /config` answers, as far as the app reads it. */
interface NodeConfig {
  spec: Topology;
  status: { pendingRestart?: string[] };
}

export interface CommandResult {
  code: number;
  stdout: string;
  stderr: string;
}

/** What the host (Tauri shell, or the e2e harness) provides. */
export interface HostOps {
  /** Runs the bundled `lattice <verb> --root <host root>`. */
  lattice(verb: LifecycleVerb): Promise<CommandResult>;
  /**
   * Runs the node (`lattice up`) as the host's child, unless it already runs.
   * Exit 75 (a restart) starts it again at once, exit 0 leaves it stopped,
   * exit 78 leaves it stopped with the reason; any other exit starts it again
   * after a growing delay.
   */
  startNode(): Promise<void>;
  /** Stops the node and resolves once it has exited; it is not started again. */
  stopNode(): Promise<void>;
  nodeProcess(): Promise<NodeProcess>;
  /** The merged configuration (`lattice config get`); null before `lattice init`. */
  readConfig(): Promise<Topology | null>;
  /**
   * `lattice config set` of `set` (`rpc`, `listen`, `peers.<chain>`,
   * `rpcAllowedOrigins`, each to a JSON value), then `lattice config unset`
   * of `unset` (`peers.<chain>`): the operator fragment, through a running
   * node or in the file while it is stopped.
   */
  setConfig(set: Record<string, unknown>, unset: string[]): Promise<CommandResult>;
  /** `lattice child host <chain>`, or `unhost`: in the operator fragment, which a running node takes at once. */
  hostChain(chain: string, hosted: boolean): Promise<CommandResult>;
  readMiner(): Promise<MinerSettings | null>;
  writeMiner(settings: MinerSettings): Promise<void>;
  /** The bundled GPU mining worker's path, or null in a build without one. */
  gpuWorker(): Promise<string | null>;
  /** Whether that worker can search on this computer's graphics chip. */
  gpuWorkerUsable(): Promise<boolean>;
  /** The end of the mining loop's log (`<root>/log/mine.log`), "" when there is none. */
  miningLog(): Promise<string>;
}

/** The node settings the app exposes. */
export interface NodePrefs {
  rpc: number;
  listen: number;
  /** "default": the node's built-in Nexus peers; "none": no peers; or the user's list. */
  peers: "default" | "none" | string[];
  /**
   * Extra peers for a hosted child chain, by chain path. A chain left out
   * finds its peers through its parent, which is all most chains need.
   */
  chainPeers: Record<string, string[]>;
  /** Child chains to host beside Nexus, parent before child (e.g. "Nexus/testnet"). */
  hostedChains: string[];
}

/** The settings a configuration holds. */
export function prefsOf(topology: Topology, hosted: string[] = topology.hostedChains ?? []): NodePrefs {
  // An array is Nexus's list, as in every file written before chains had their own.
  const byChain = Array.isArray(topology.peers) ? { Nexus: topology.peers } : topology.peers ?? {};
  const nexus = byChain.Nexus;
  const hostedChains = [...hosted];
  return {
    rpc: topology.rpc,
    listen: topology.listen,
    peers: nexus === undefined ? "default" : nexus.length === 0 ? "none" : [...nexus],
    // For a child chain an empty list is the same as none.
    chainPeers: Object.fromEntries(hostedChains.filter((chain) => (byChain[chain]?.length ?? 0) > 0).map((chain) => [chain, [...byChain[chain]!]])),
    hostedChains,
  };
}

/**
 * The CLI's changes that take the configuration from `current` to `wanted`:
 * keys of the operator fragment to set and to unset, and chains to host and
 * to stop hosting, each one path (an ancestor a hosted chain needs stays
 * implied). Peers are the union of every layer's, so a chain's list is set
 * whole in the operator fragment, or unset to leave the others'.
 */
export function changesOf(current: NodePrefs, wanted: NodePrefs) {
  const set: Record<string, unknown> = {};
  const unset: string[] = [];
  if (wanted.rpc !== current.rpc) set.rpc = wanted.rpc;
  if (wanted.listen !== current.listen) set.listen = wanted.listen;
  if (canonical(wanted.peers) !== canonical(current.peers)) {
    if (wanted.peers === "default") unset.push("peers.Nexus");
    else set["peers.Nexus"] = wanted.peers === "none" ? [] : [...wanted.peers];
  }
  const host = wanted.hostedChains.filter((chain) => !current.hostedChains.includes(chain));
  const unhost = current.hostedChains.filter((chain) => !wanted.hostedChains.includes(chain)).reverse();
  for (const chain of new Set([...Object.keys(current.chainPeers), ...Object.keys(wanted.chainPeers)])) {
    // Only for chains still hosted: the node takes no list for any other.
    const list = wanted.hostedChains.includes(chain) ? wanted.chainPeers[chain] ?? [] : [];
    if (canonical(list) === canonical(current.chainPeers[chain] ?? [])) continue;
    if (list.length > 0) set[`peers.${chain}`] = [...list];
    else unset.push(`peers.${chain}`);
  }
  return { set, unset, host, unhost };
}

const PEER = /^[0-9a-f]{64}@[^\s@]+:[0-9]{1,5}$/;

/** The settings as typed, or why not. `lattice up` validates the rest. */
export function checkPrefs(prefs: NodePrefs): string | undefined {
  const port = (p: number) => Number.isInteger(p) && p > 0 && p < 65536;
  if (!port(prefs.rpc) || !port(prefs.listen)) return "Ports must be whole numbers from 1 to 65535.";
  if (prefs.rpc === prefs.listen) return "The RPC and peer ports must differ.";
  if (Array.isArray(prefs.peers)) {
    if (prefs.peers.length === 0) return "List at least one peer, or choose none.";
    const bad = prefs.peers.find((p) => !PEER.test(p));
    if (bad !== undefined) return `Not a peer (publicKey@host:port): ${bad}`;
  }
  for (const [chain, list] of Object.entries(prefs.chainPeers)) {
    if (list.length === 0) return `List at least one peer for ${chain}, or let it find its own.`;
    const bad = list.find((p) => !PEER.test(p));
    if (bad !== undefined) return `Not a peer (publicKey@host:port): ${bad}`;
  }
  const listed = new Set(["Nexus"]);
  for (const chain of prefs.hostedChains) {
    const parts = chain.split("/");
    const parent = parts.slice(0, -1).join("/");
    if (parts.length < 2 || !parts.every((p) => /^[A-Za-z0-9_-]{1,64}$/.test(p)) || !listed.has(parent) || listed.has(chain)) {
      return `Hosted chain ${chain} must be a new Nexus-rooted path listed after its parent.`;
    }
    listed.add(chain);
  }
  return undefined;
}

/**
 * What searches for blocks: this computer's processor (the CLI's bundled worker),
 * its graphics chip (the bundled GPU worker), or a worker the operator named
 * in miner.json by hand, which the app shows and keeps but cannot set.
 */
export type Searcher = "cpu" | "gpu" | "custom";

const NO_GPU = "This computer has no graphics chip Lattice can mine on: it needs a graphics driver that provides OpenCL. Choose the processor in Mining.";

/** The searcher miner.json's `worker` names, given this build's GPU worker. */
export function searcherOf(worker: string | undefined, gpuWorker: string | null): Searcher {
  if (worker === undefined || worker === "cpu") return "cpu";
  // By name, not by path: the app may have moved since it was saved.
  return gpuWorker !== null && worker.split("/").pop() === gpuWorker.split("/").pop() ? "gpu" : "custom";
}

/** The mining settings the app exposes. */
export interface MiningPrefs {
  searcher: Searcher;
  /** Workers searching in parallel. The graphics chip is one worker: more would only contend for it. */
  workers: number;
  /** Who is paid each chain's block reward and fees, by chain path. A chain left out burns its payout. */
  recipients: Record<string, string>;
}

export const MAX_WORKERS = 64;
const ADDRESS = /^b[a-z0-9]{39,127}$/;

/** The mining settings as chosen, or why not, for a node hosting `chains`. */
export function checkMining(prefs: MiningPrefs, chains: string[]): string | undefined {
  if (!Number.isInteger(prefs.workers) || prefs.workers < 1 || prefs.workers > MAX_WORKERS) {
    return `Workers must be a whole number from 1 to ${MAX_WORKERS}.`;
  }
  for (const [chain, address] of Object.entries(prefs.recipients)) {
    if (!chains.includes(chain)) return `This node does not host ${chain}.`;
    if (!ADDRESS.test(address)) return `Not an address for ${chain}: ${address}`;
  }
  return undefined;
}

export interface MiningStatus {
  running: boolean;
  pid?: number;
}

/** Parses `lattice mine status`: its first line says whether the loop runs. */
export function parseMiningStatus(stdout: string): MiningStatus {
  const pid = /^mining: running \(pid (\d+)\)/m.exec(stdout);
  return pid ? { running: true, pid: Number(pid[1]) } : { running: false };
}

/**
 * What the mining loop's log says about its current run: the blocks the node
 * accepted since the loop last started, and when the latest was.
 */
export function parseMiningLog(log: string): { found: number; last?: string } {
  const run = log.slice(log.lastIndexOf("mining loop start") + 1);
  const accepted = [...run.matchAll(/^(\S+) accepted tip=/gm)];
  const last = accepted.at(-1)?.[1];
  return { found: accepted.length, ...(last ? { last } : {}) };
}

export interface ChainStatus {
  path: string;
  /** "down", "unreachable", or the node's phase (e.g. "active"). */
  state: string;
  height?: bigint;
  /** The best header chain the node knows (from its /health): sync progress is height of this. */
  bestHeaderHeight?: bigint;
  tip?: string;
  mempool?: number;
}

/**
 * Parses `lattice status`: one line per hosted chain, its first. Lines after
 * them that say more of a chain, or of the configuration, are not chains.
 */
export function parseStatus(stdout: string): ChainStatus[] {
  const out: ChainStatus[] = [];
  for (const line of stdout.split("\n")) {
    const m = /^(Nexus(?:\/\S+)?): (.*)$/.exec(line.trim());
    if (!m) continue;
    const [, path, rest] = m as unknown as [string, string, string];
    if (out.some((c) => c.path === path)) continue;
    if (rest === "down") out.push({ path, state: "down" });
    else if (rest.startsWith("running, rpc unreachable")) out.push({ path, state: "unreachable" });
    else {
      const fields = /^(\S+) height=(\S+) tip=(\S+?)…? mempool=(\d+)$/.exec(rest);
      if (!fields) out.push({ path, state: rest });
      else {
        const [, phase, height, tip, mempool] = fields as unknown as string[];
        out.push({
          path, state: phase!,
          ...(/^\d+$/.test(height!) ? { height: BigInt(height!) } : {}),
          ...(tip !== "-" ? { tip } : {}),
          mempool: Number(mempool),
        });
      }
    }
  }
  return out;
}

/**
 * A browser extension's origin as the node lists it: a Chrome extension id
 * (32 letters a-p) becomes `chrome-extension://<id>`; an origin
 * `<scheme>://<host>` is taken as given. Undefined when neither.
 */
export function extensionOrigin(text: string): string | undefined {
  const t = text.trim();
  if (/^[a-p]{32}$/.test(t)) return `chrome-extension://${t}`;
  return /^[a-z][a-z0-9+.-]*-extension:\/\/[A-Za-z0-9._-]+$/.test(t) ? t : undefined;
}

/** First-run ports of the app's node; settable afterwards. */
export const APP_PORTS = { rpc: 18080, listen: 14001 } as const;

export class LifecycleError extends Error {
  constructor(verb: LifecycleVerb, result: CommandResult) {
    super(`lattice ${verb} failed (exit ${result.code}): ${(result.stderr || result.stdout).trim().slice(-500)}`);
  }
}

/** How long a restart is waited for: as long as `lattice restart` waits. */
const RESTART_PATIENCE_MS = 180_000;

/** JSON with every object's keys in order, to compare values however they were written. */
const canonical = (value: unknown) => JSON.stringify(value, (_, v: unknown) =>
  v && typeof v === "object" && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : 1))) : v);

export class NodeManager {
  readonly #host: HostOps;
  readonly #http: NodeHTTP;
  constructor(host: HostOps, http: NodeHTTP) {
    this.#host = host;
    this.#http = http;
  }

  async #run(verb: LifecycleVerb): Promise<CommandResult> {
    const result = await this.#host.lattice(verb);
    if (result.code !== 0) throw new LifecycleError(verb, result);
    return result;
  }

  /** Runs a CLI change, or says why the CLI refused it. */
  async #change(result: Promise<CommandResult>): Promise<void> {
    const said = await result;
    if (said.code !== 0) throw new Error((said.stderr || said.stdout).trim().replace(/^Error: /, ""));
  }

  /**
   * The host's merged configuration, scaffolding the host root with `lattice
   * init` on first run. The app's node then takes its own ports (APP_PORTS),
   * so it does not collide with a CLI-managed host on the same machine,
   * which `lattice init` puts on 8080/4001.
   */
  async ensureHost(): Promise<Topology> {
    const existing = await this.#merged();
    if (existing) return existing;
    await this.#run("init");
    await this.#change(this.#host.setConfig({ ...APP_PORTS }, []));
    const created = await this.#host.readConfig();
    if (!created) throw new Error("lattice init wrote no lattice.json");
    return created;
  }

  async prefs(): Promise<NodePrefs> {
    return prefsOf(await this.ensureHost());
  }

  /**
   * The merged configuration: the running node's (`GET /config`), else the
   * files' (`lattice config get`). Not the CLI while the node runs: a saved
   * `rpc` change points the CLI at the port the node will take, not the one
   * it answers on, and it waits there for an answer that never comes.
   */
  async #merged(): Promise<Topology | null> {
    return (await this.#config())?.spec ?? this.#host.readConfig();
  }

  /** The configuration the running node uses; undefined while none answers. */
  async #config(): Promise<NodeConfig | undefined> {
    try {
      const answer = await this.#http("GET", "/config", null);
      return answer.status === 200 ? (JSON.parse(answer.body) as NodeConfig) : undefined;
    } catch {
      return undefined;
    }
  }

  /**
   * Saves the settings: each change one key or chain of the operator
   * fragment, through the CLI. A running node hosts chains and takes a child
   * chain's peers at once, and restarts when it says the rest waits for one.
   * What lattice.json or another fragment sets stays: a chain or peer they
   * list cannot be removed here, and the CLI says where it is.
   */
  async configure(prefs: NodePrefs): Promise<void> {
    const problem = checkPrefs(prefs);
    if (problem) throw new Error(problem);
    const current = prefsOf(await this.ensureHost());
    // A chain no longer hosted is no longer paid. An operator's tuning entry
    // for it is theirs to remove: say so, and write nothing.
    const kept = new Set(["Nexus", ...prefs.hostedChains]);
    const mine = await this.#host.readMiner();
    const tuned = Object.keys((mine?.minWork as Record<string, unknown> | undefined) ?? {}).find((chain) => !kept.has(chain));
    if (tuned !== undefined) throw new Error(`miner.json's minWork names ${tuned}. Remove that entry there before you stop hosting it.`);
    const { set, unset, host, unhost } = changesOf(current, prefs);
    for (const chain of host) await this.#change(this.#host.hostChain(chain, true));
    if (Object.keys(set).length + unset.length > 0) await this.#change(this.#host.setConfig(set, unset));
    // Deepest first; a chain still hosted afterwards is said below, with why.
    const refused = new Map<string, string>();
    for (const chain of unhost) {
      const said = await this.#host.hostChain(chain, false);
      if (said.code !== 0) refused.set(chain, (said.stderr || said.stdout).trim().replace(/^Error: /, ""));
    }
    const after = prefsOf((await this.#merged()) ?? {} as Topology);
    const paid = mine?.recipients ?? {};
    const hosted = new Set(["Nexus", ...after.hostedChains]);
    // The loop reads its settings again each round.
    if (Object.keys(paid).some((chain) => !hosted.has(chain))) {
      await this.#host.writeMiner({ ...mine, recipients: Object.fromEntries(Object.entries(paid).filter(([chain]) => hosted.has(chain))) });
    }
    await this.#restartIfPending([...Object.keys(set), ...unset].some((key) => !key.startsWith("peers.Nexus/")));
    const stillHosted = unhost.find((chain) => after.hostedChains.includes(chain));
    if (stillHosted !== undefined) throw new Error(`${stillHosted} is still hosted: ${refused.get(stillHosted) ?? "a chain under it needs it"}.`);
    const chainPeers = Object.fromEntries(Object.entries(prefs.chainPeers).filter(([chain]) => prefs.hostedChains.includes(chain)));
    if (canonical(after.peers) !== canonical(prefs.peers) || canonical(after.chainPeers) !== canonical(chainPeers)) {
      throw new Error("Saved, but lattice.json or another configuration fragment lists more peers; change them there.");
    }
  }

  /**
   * Restarts a running node when it says a change waits for a restart
   * (`GET /config`'s pendingRestart). One not answering yet may have read its
   * configuration before a change that waits (`changed`): it is stopped and
   * started again.
   */
  async #restartIfPending(changed: boolean): Promise<void> {
    if (!(await this.#host.nodeProcess()).wanted) return;
    const config = await this.#config();
    if (config ? (config.status.pendingRestart?.length ?? 0) > 0 : changed) await this.#restart();
  }

  /**
   * Restarts the node: asked to (it exits for the host to start it again),
   * then waited for until a new process answers.
   */
  async #restart(): Promise<void> {
    const before = await this.#startedAt();
    if (before === undefined) {
      await this.#host.stopNode();
      return this.#host.startNode();
    }
    const asked = await this.#http("POST", "/restart", "{}");
    if (asked.status !== 202) throw new Error(`Saved, but the node would not restart to take it: ${asked.body}`);
    const deadline = Date.now() + RESTART_PATIENCE_MS;
    while (Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 250));
      const now = await this.#startedAt();
      if (now !== undefined && now !== before) return;
      const process = await this.#host.nodeProcess();
      if (!process.wanted) throw new Error(process.error ?? "The node stopped instead of restarting.");
    }
    throw new Error("The node did not come back within three minutes of its restart.");
  }

  /** When the node answering started (`GET /status`); undefined while none answers. */
  async #startedAt(): Promise<number | undefined> {
    try {
      const answer = await this.#http("GET", "/status", null);
      return answer.status === 200 ? (JSON.parse(answer.body) as { startedAt?: number }).startedAt : undefined;
    } catch {
      return undefined;
    }
  }

  /**
   * The wallet's chain list: Nexus and the chains this node hosts. One list,
   * the node's, for the node and the wallet alike.
   */
  async chains(): Promise<string[]> {
    return ["Nexus", ...(await this.prefs()).hostedChains];
  }

  /** Hosts `chain` too (its parent must be hosted already); a running node starts it at once. */
  async addHostedChain(chain: string): Promise<void> {
    const prefs = await this.prefs();
    if (chain === "Nexus" || prefs.hostedChains.includes(chain)) return;
    await this.configure({ ...prefs, hostedChains: [...prefs.hostedChains, chain] });
  }

  /** Lets a browser extension's origin reach the operator port (it still needs the cookie); a running node restarts. */
  async allowOrigin(origin: string): Promise<void> {
    const origins = (await this.ensureHost()).rpcAllowedOrigins ?? [];
    if (origins.includes(origin)) return;
    await this.#change(this.#host.setConfig({ rpcAllowedOrigins: [...origins, origin] }, []));
    await this.#restartIfPending(true);
  }

  async allowedOrigins(): Promise<string[]> {
    return [...((await this.ensureHost()).rpcAllowedOrigins ?? [])];
  }

  async start(): Promise<void> {
    await this.ensureHost();
    await this.#host.startNode();
  }

  /** Stops the node, and the mining loop with it: it mines on this node. */
  async stop(): Promise<void> {
    if ((await this.miningStatus()).running) await this.#run("mine-stop");
    await this.#host.stopNode();
  }

  /** Whether the app runs the node, and why it stopped by itself when it did. */
  nodeProcess(): Promise<NodeProcess> {
    return this.#host.nodeProcess();
  }

  /** The mining settings, or undefined before any were saved. */
  async miningPrefs(): Promise<MiningPrefs | undefined> {
    const mine = await this.#host.readMiner();
    if (!mine) return undefined;
    return {
      searcher: searcherOf(mine.worker, await this.#host.gpuWorker()),
      workers: mine.workers ?? 1,
      recipients: { ...(mine.recipients ?? {}) },
    };
  }

  /** Whether the app can mine on the graphics chip: it carries the worker, and the worker finds a hash on this computer. */
  async canMineOnGPU(): Promise<boolean> {
    return (await this.#host.gpuWorker()) !== null && (await this.#host.gpuWorkerUsable());
  }

  /** Whether the mining loop runs. Before any settings were saved it cannot. */
  async miningStatus(): Promise<MiningStatus> {
    await this.ensureHost();
    if (!(await this.#host.readMiner())) return { running: false };
    return parseMiningStatus((await this.#run("mine-status")).stdout);
  }

  /**
   * Saves the mining settings. A running loop is restarted to take them at
   * once, not at its next round.
   */
  async configureMining(prefs: MiningPrefs): Promise<void> {
    const problem = checkMining(prefs, await this.chains());
    if (problem) throw new Error(problem);
    const gpuWorker = await this.#host.gpuWorker();
    if (prefs.searcher === "gpu" && gpuWorker === null) throw new Error("This build has no graphics-chip worker.");
    if (prefs.searcher === "gpu" && !(await this.#host.gpuWorkerUsable())) throw new Error(NO_GPU);
    const wasMining = (await this.miningStatus()).running;
    if (wasMining) await this.#run("mine-stop");
    const mine: MinerSettings = {
      ...(await this.#host.readMiner()),
      workers: prefs.searcher === "gpu" ? 1 : prefs.workers,
      recipients: { ...prefs.recipients },
    };
    if (prefs.searcher === "cpu") mine.worker = "cpu";
    else if (prefs.searcher === "gpu") mine.worker = gpuWorker!;
    await this.#host.writeMiner(mine);
    if (wasMining) await this.#run("mine-start");
  }

  /** Starts the mining loop on this node, which must be running: the loop only retries a node that is not there. */
  async startMining(): Promise<void> {
    if (!(await this.#host.readMiner())) throw new Error("Choose who mining pays first.");
    if (!(await this.status()).some((c) => c.state !== "down")) throw new Error("Start the node first: mining runs on it.");
    // Settings saved when the graphics chip worked, or copied from a computer where it did.
    if ((await this.miningPrefs())?.searcher === "gpu" && !(await this.canMineOnGPU())) throw new Error(NO_GPU);
    await this.#run("mine-start");
  }

  async stopMining(): Promise<void> {
    await this.#run("mine-stop");
  }

  /** The end of the mining loop's log. */
  miningLog(): Promise<string> {
    return this.#host.miningLog();
  }

  async status(): Promise<ChainStatus[]> {
    await this.ensureHost();
    return parseStatus((await this.#run("status")).stdout);
  }

  /** The node's loopback operator URL (reads + this owner's submits). */
  async rpcURL(): Promise<string> {
    return `http://127.0.0.1:${(await this.ensureHost()).rpc}`;
  }
}
