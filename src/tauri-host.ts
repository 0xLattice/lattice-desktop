// The Tauri shell's commands, typed. Everything privileged (running the
// bundled CLI, files under ~/Library/Application Support/Lattice, loopback
// HTTP) lives in the Rust shell; the page only invokes these.

import { invoke } from "@tauri-apps/api/core";
import type { HostOps, LifecycleVerb, CommandResult, Topology } from "./node-manager.ts";
import type { LoopbackAnswer, NodeHTTP } from "./loopback.ts";
import type { VaultStorage } from "../vendor/nexus-wallet/src/lib/wallet/signer.ts";
import type { KeyValueStore } from "../vendor/nexus-wallet/src/lib/wallet/settings.ts";
import type { Vault } from "../vendor/nexus-wallet/src/lib/crypto/keystore.ts";

const gpuWorker = () => invoke<string | null>("gpu_worker");

export const hostOps: HostOps = {
  lattice: (verb: LifecycleVerb) => invoke<CommandResult>("lattice", { verb }),
  readTopology: () => invoke<Topology | null>("read_topology"),
  // Only the fields the app manages; the shell merges them and refuses others.
  writeTopology: async ({ rpc, listen, peers, hostedChains, rpcAllowedOrigins, mine }: Topology) => {
    // Of mining: who is paid, the worker count, and the worker only when it is
    // one bundled with the app (the shell refuses any other, and keeps the rest).
    const bundledWorker = mine?.worker === "cpu" || (mine?.worker !== undefined && mine.worker === await gpuWorker());
    await invoke<void>("write_topology", {
      topology: {
        rpc, listen,
        ...(peers ? { peers } : {}),
        ...(hostedChains ? { hostedChains } : {}),
        ...(rpcAllowedOrigins ? { rpcAllowedOrigins } : {}),
        ...(mine ? { mine: { workers: mine.workers ?? 1, recipients: mine.recipients ?? {}, ...(bundledWorker ? { worker: mine.worker } : {}) } } : {}),
      },
    });
  },
  gpuWorker: () => gpuWorker(),
};

/** The node's current loopback cookie, for pairing a browser extension (null when not running). */
export const nodeCookie = () => invoke<string | null>("node_cookie");

export const nodeHTTP: NodeHTTP = (method, path, body) =>
  invoke<LoopbackAnswer>("node_http", { method, path, body });

const get = (key: string) => invoke<unknown>("store_get", { key });
const set = (key: string, value: unknown) => invoke<void>("store_set", { key, value });

/** The one encrypted vault, in the extension's keystore format, as a 0600 file. */
export const vaultFile: VaultStorage = {
  load: async () => ((await get("vault")) as Vault | null) ?? null,
  save: (vault) => set("vault", vault),
  remove: () => invoke<void>("store_remove", { key: "vault" }),
};

/** Non-secret wallet settings, as a file beside the vault. */
export const settingsFile: KeyValueStore = {
  get: async (key) => (key === "settings" ? { settings: (await get("settings")) ?? undefined } : {}),
  set: async (items) => {
    if ("settings" in items) await set("settings", items.settings);
  },
};

/** Save an encrypted backup as a new file in ~/Downloads; resolves to its path. */
export const saveBackup = (name: string, text: string) => invoke<string>("save_backup", { name, text });

/** The webview's print dialog. */
export const printPage = () => { invoke<void>("print_page").catch(() => {}); };
