// The app's node: one host root scaffolded and driven through the bundled
// `lattice` CLI (init / up / down / status), exactly as an operator would.
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
  [field: string]: unknown;
}

export type LifecycleVerb = "init" | "up" | "down" | "status";

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

export interface ChainStatus {
  path: string;
  /** "down", "unreachable", or the node's phase (e.g. "active"). */
  state: string;
  height?: bigint;
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
    const wasRunning = (await this.status()).some((c) => c.state !== "down");
    if (wasRunning) await this.#run("down");
    await this.#host.writeTopology(withPrefs(topology, prefs));
    if (wasRunning) await this.#run("up");
  }

  async start(): Promise<void> {
    await this.ensureHost();
    await this.#run("up");
  }

  async stop(): Promise<void> {
    await this.#run("down");
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
