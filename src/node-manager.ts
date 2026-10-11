// The app's node: one host root scaffolded and driven through the bundled
// `lattice` CLI (init / up / down / status, and mine start / stop / status),
// exactly as an operator would.
// `lattice up` spawns the signed `lattice-node` as a child; the app talks to
// it only over its loopback RPC. Pure logic over injected host operations,
// so the same code runs in the app and in the headless e2e harness.

/** lattice.json: the CLI's declarative topology (fields the app does not manage are kept). */
export interface Topology {
  listen: number;
  rpc: number;
  /** Absent: the node's built-in bootstrap peers. []: none. */
  peers?: string[];
  hostedChains?: string[];
  /** Browser origins allowed on the loopback operator port (e.g. a paired extension). */
  rpcAllowedOrigins?: string[];
  /**
   * The mining loop's settings. The app sets `workers`, `recipients`, and
   * `worker` to "cpu" or the bundled GPU worker's path; the rest is the operator's.
   */
  mine?: { worker?: string; workers?: number; recipients?: Record<string, string>; [field: string]: unknown };
  [field: string]: unknown;
}

export type LifecycleVerb = "init" | "up" | "down" | "status" | "mine-start" | "mine-stop" | "mine-status";

export interface CommandResult {
  code: number;
  stdout: string;
  stderr: string;
}

/** What the host (Tauri shell, or the e2e harness) provides. */
export interface HostOps {
  /** Runs the bundled `lattice <verb> --root <host root>`. */
  lattice(verb: LifecycleVerb): Promise<CommandResult>;
  readTopology(): Promise<Topology | null>;
  writeTopology(topology: Topology): Promise<void>;
  /** The bundled GPU mining worker's path, or null in a build without one. */
  gpuWorker(): Promise<string | null>;
  /** The end of the mining loop's log (`<root>/log/mine.log`), "" when there is none. */
  miningLog(): Promise<string>;
}

/** The node settings the app exposes. */
export interface NodePrefs {
  rpc: number;
  listen: number;
  /** "default": the node's built-in Nexus peers; "none": no peers; or the user's list. */
  peers: "default" | "none" | string[];
  /** Child chains to host beside Nexus, parent before child (e.g. "Nexus/testnet"). */
  hostedChains: string[];
}

export function prefsOf(topology: Topology): NodePrefs {
  return {
    rpc: topology.rpc,
    listen: topology.listen,
    peers: topology.peers === undefined ? "default" : topology.peers.length === 0 ? "none" : [...topology.peers],
    hostedChains: [...(topology.hostedChains ?? [])],
  };
}

/** The topology with the app's settings applied; every other field is kept. */
export function withPrefs(topology: Topology, prefs: NodePrefs): Topology {
  const next: Topology = { ...topology, rpc: prefs.rpc, listen: prefs.listen };
  delete next.peers;
  if (prefs.peers === "none") next.peers = [];
  else if (prefs.peers !== "default") next.peers = [...prefs.peers];
  delete next.hostedChains;
  if (prefs.hostedChains.length > 0) next.hostedChains = [...prefs.hostedChains];
  return next;
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
 * What searches for blocks: this Mac's processor (the CLI's bundled worker),
 * its graphics chip (the bundled GPU worker), or a worker the operator named
 * in lattice.json by hand, which the app shows and keeps but cannot set.
 */
export type Searcher = "cpu" | "gpu" | "custom";

/** The searcher lattice.json's `mine.worker` names, given this build's GPU worker. */
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

/** Parses `lattice status`: one line per hosted chain. */
export function parseStatus(stdout: string): ChainStatus[] {
  const out: ChainStatus[] = [];
  for (const line of stdout.split("\n")) {
    const m = /^(\S+): (.*)$/.exec(line.trim());
    if (!m) continue;
    const [, path, rest] = m as unknown as [string, string, string];
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

export class NodeManager {
  readonly #host: HostOps;
  constructor(host: HostOps) {
    this.#host = host;
  }

  async #run(verb: LifecycleVerb): Promise<CommandResult> {
    const result = await this.#host.lattice(verb);
    if (result.code !== 0) throw new LifecycleError(verb, result);
    return result;
  }

  /**
   * The host's topology, scaffolding the host root with `lattice init` on
   * first run. The app's node then takes its own ports (APP_PORTS), so it
   * does not collide with a CLI-managed host on the same machine, which
   * `lattice init` puts on 8080/4001.
   */
  async ensureHost(): Promise<Topology> {
    const existing = await this.#host.readTopology();
    if (existing) return existing;
    await this.#run("init");
    const created = await this.#host.readTopology();
    if (!created) throw new Error("lattice init wrote no lattice.json");
    const topology = { ...created, ...APP_PORTS };
    await this.#host.writeTopology(topology);
    return topology;
  }

  async prefs(): Promise<NodePrefs> {
    return prefsOf(await this.ensureHost());
  }

  /**
   * Saves the settings. A running node restarts to take them: `up` alone
   * restarts only when the hosted chain set changed, so stop it first.
   */
  async configure(prefs: NodePrefs): Promise<void> {
    const problem = checkPrefs(prefs);
    if (problem) throw new Error(problem);
    const topology = await this.ensureHost();
    const next = withPrefs(topology, prefs);
    // The CLI refuses a lattice.json whose mining names a chain it does not
    // host: `up`, and `status` with it. A chain no longer hosted is no longer
    // paid; a tuning entry for it is the operator's to remove.
    const hosted = new Set(["Nexus", ...prefs.hostedChains]);
    const tuned = Object.keys((topology.mine?.minWork as Record<string, unknown> | undefined) ?? {}).find((chain) => !hosted.has(chain));
    if (tuned !== undefined) throw new Error(`lattice.json's mine.minWork names ${tuned}. Remove that entry there before you stop hosting it.`);
    const paid = topology.mine?.recipients ?? {};
    const dropped = Object.keys(paid).filter((chain) => !hosted.has(chain));
    if (dropped.length === 0) return this.#rewrite(next);
    next.mine = { ...topology.mine, recipients: Object.fromEntries(Object.entries(paid).filter(([chain]) => hosted.has(chain))) };
    // A running loop was started with the old recipients: restart it with these.
    const wasMining = (await this.miningStatus()).running;
    if (wasMining) await this.#run("mine-stop");
    await this.#rewrite(next);
    if (wasMining) await this.#run("mine-start");
  }

  /** Writes the topology; a running node restarts to take it. */
  async #rewrite(topology: Topology): Promise<void> {
    const wasRunning = (await this.status()).some((c) => c.state !== "down");
    if (wasRunning) await this.#run("down");
    await this.#host.writeTopology(topology);
    if (wasRunning) await this.#run("up");
  }

  /**
   * The wallet's chain list: Nexus and the chains this node hosts. One list,
   * kept in lattice.json, for the node and the wallet alike.
   */
  async chains(): Promise<string[]> {
    return ["Nexus", ...((await this.ensureHost()).hostedChains ?? [])];
  }

  /** Hosts `chain` too (its parent must be hosted already); a running node restarts. */
  async addHostedChain(chain: string): Promise<void> {
    const prefs = await this.prefs();
    if (chain === "Nexus" || prefs.hostedChains.includes(chain)) return;
    await this.configure({ ...prefs, hostedChains: [...prefs.hostedChains, chain] });
  }

  /** Lets a browser extension's origin reach the operator port (it still needs the cookie). */
  async allowOrigin(origin: string): Promise<void> {
    const topology = await this.ensureHost();
    const origins = topology.rpcAllowedOrigins ?? [];
    if (origins.includes(origin)) return;
    await this.#rewrite({ ...topology, rpcAllowedOrigins: [...origins, origin] });
  }

  async allowedOrigins(): Promise<string[]> {
    return [...((await this.ensureHost()).rpcAllowedOrigins ?? [])];
  }

  async start(): Promise<void> {
    await this.ensureHost();
    await this.#run("up");
  }

  /** Stops the node, and the mining loop with it: it mines on this node. */
  async stop(): Promise<void> {
    if ((await this.miningStatus()).running) await this.#run("mine-stop");
    await this.#run("down");
  }

  /** The mining settings, or undefined before any were saved. */
  async miningPrefs(): Promise<MiningPrefs | undefined> {
    const mine = (await this.ensureHost()).mine;
    if (!mine) return undefined;
    return {
      searcher: searcherOf(mine.worker, await this.#host.gpuWorker()),
      workers: mine.workers ?? 1,
      recipients: { ...(mine.recipients ?? {}) },
    };
  }

  /** Whether this build can mine on the graphics chip. */
  async canMineOnGPU(): Promise<boolean> {
    return (await this.#host.gpuWorker()) !== null;
  }

  /** Whether the mining loop runs. Before any settings were saved it cannot. */
  async miningStatus(): Promise<MiningStatus> {
    if (!(await this.ensureHost()).mine) return { running: false };
    return parseMiningStatus((await this.#run("mine-status")).stdout);
  }

  /**
   * Saves the mining settings. The loop reads them when it starts, so a
   * running one is restarted to take them.
   */
  async configureMining(prefs: MiningPrefs): Promise<void> {
    const problem = checkMining(prefs, await this.chains());
    if (problem) throw new Error(problem);
    const gpuWorker = await this.#host.gpuWorker();
    if (prefs.searcher === "gpu" && gpuWorker === null) throw new Error("This build has no graphics-chip worker.");
    const wasMining = (await this.miningStatus()).running;
    if (wasMining) await this.#run("mine-stop");
    const topology = await this.ensureHost();
    const mine: NonNullable<Topology["mine"]> = {
      ...topology.mine,
      workers: prefs.searcher === "gpu" ? 1 : prefs.workers,
      recipients: { ...prefs.recipients },
    };
    if (prefs.searcher === "cpu") mine.worker = "cpu";
    else if (prefs.searcher === "gpu") mine.worker = gpuWorker!;
    await this.#host.writeTopology({ ...topology, mine });
    if (wasMining) await this.#run("mine-start");
  }

  /** Starts the mining loop on this node, which must be running: the loop only retries a node that is not there. */
  async startMining(): Promise<void> {
    if (!(await this.ensureHost()).mine) throw new Error("Choose who mining pays first.");
    if (!(await this.status()).some((c) => c.state !== "down")) throw new Error("Start the node first: mining runs on it.");
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
