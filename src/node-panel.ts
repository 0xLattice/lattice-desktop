// The desktop's node screen: start/stop/status of the app's own node, its
// sync heights, its settings (ports, Nexus bootstrap peers, hosted child
// chains), and mining on it. Rendered with the wallet's own helpers and styles.

import { h, render } from "../vendor/nexus-wallet/src/popup/app.ts";
import { checkMining, checkPrefs, extensionOrigin, MAX_WORKERS, type ChainStatus, type MiningPrefs, type NodeManager, type NodePrefs } from "./node-manager.ts";

/** What the panel reads beyond `lattice status`: a chain's /health, the node's cookie, and the wallet's accounts. */
export interface NodeReads {
  health(path: string): Promise<{ height?: bigint; bestHeaderHeight?: bigint }>;
  cookie(): Promise<string | null>;
  /** The wallet's accounts, the active one first: who mining can pay. Empty while locked. */
  accounts(): Promise<{ label: string; address: string }[]>;
}

const POLL_MS = 3000;

export function statusLine(s: ChainStatus): string {
  if (s.state === "down") return "stopped";
  if (s.state === "unreachable") return "starting (RPC not answering yet)";
  const behind = s.height !== undefined && s.bestHeaderHeight !== undefined && s.bestHeaderHeight > s.height;
  if (behind) return `syncing · ${s.height!.toLocaleString()} of ${s.bestHeaderHeight!.toLocaleString()}`;
  const height = s.height === undefined ? "" : ` · height ${s.height.toLocaleString()}`;
  const pool = s.mempool ? ` · ${s.mempool} pending` : "";
  return `${s.state}${height}${pool}`;
}

/** The peers field as typed: blank = the node's built-in peers. */
export function parsePeersField(mode: string, text: string): NodePrefs["peers"] {
  if (mode === "default") return "default";
  if (mode === "none") return "none";
  return text.split(/[\s,]+/).filter(Boolean);
}

/** Each running chain's status with its best header height from the node's /health. */
export async function withSync(chains: ChainStatus[], reads: Pick<NodeReads, "health">): Promise<ChainStatus[]> {
  return Promise.all(chains.map(async (c) => {
    if (c.state === "down" || c.state === "unreachable") return c;
    try {
      const { bestHeaderHeight } = await reads.health(c.path);
      return bestHeaderHeight === undefined ? c : { ...c, bestHeaderHeight };
    } catch {
      return c;
    }
  }));
}

export function nodeScreen(manager: NodeManager, reads: NodeReads, back: () => void, onPrefsChanged: () => void) {
  const table = h("div", { class: "kv" }, h("div", { class: "row" }, h("span", { class: "muted" }, "…")));
  const toast = h("div", { class: "toast" });
  let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refresh = async () => {
    clearTimeout(timer);
    try {
      const chains = await withSync(await manager.status(), reads);
      table.replaceChildren(...chains.map((c) => h("div", { class: "row" },
        h("span", { class: "k mono" }, c.path),
        h("span", { class: "v" }, statusLine(c)),
      )));
    } catch (e) {
      table.replaceChildren(h("div", { class: "row" }, h("span", { class: "warn" }, (e as Error).message)));
    }
    if (document.body.contains(table)) timer = setTimeout(refresh, POLL_MS);
  };

  const act = (label: string, run: () => Promise<void>) => async () => {
    if (busy) return;
    busy = true;
    toast.textContent = label + "…";
    try {
      await run();
      toast.textContent = "";
    } catch (e) {
      toast.textContent = (e as Error).message;
    } finally {
      busy = false;
      refresh();
    }
  };

  render(
    h("div", { class: "stack" },
      h("h1", {}, "Your node"),
      h("p", { class: "muted" }, "A full Lattice node on this Mac. The wallet reads and sends only through it, over loopback."),
      table,
      h("div", { class: "row-actions" },
        h("button", { class: "btn", onclick: act("starting", () => manager.start()) }, "Start"),
        h("button", { class: "btn", onclick: act("stopping", () => manager.stop()) }, "Stop"),
        h("button", { class: "btn", onclick: () => refresh() }, "Refresh"),
      ),
      toast,
      h("button", { class: "btn block", onclick: () => { clearTimeout(timer); miningScreen(manager, reads, () => nodeScreen(manager, reads, back, onPrefsChanged)); } }, "Mining"),
      h("button", { class: "btn block", onclick: () => { clearTimeout(timer); settingsScreen(manager, () => nodeScreen(manager, reads, back, onPrefsChanged), onPrefsChanged); } }, "Node settings"),
      h("button", { class: "btn block", onclick: () => { clearTimeout(timer); pairScreen(manager, reads, () => nodeScreen(manager, reads, back, onPrefsChanged)); } }, "Pair browser extension"),
      h("button", { class: "btn block", onclick: () => { clearTimeout(timer); back(); } }, "Back to wallet"),
    ),
  );
  refresh();
}

async function settingsScreen(manager: NodeManager, back: () => void, onPrefsChanged: () => void) {
  const prefs = await manager.prefs();
  const rpc = h("input", { type: "text", inputmode: "numeric", value: String(prefs.rpc) }) as HTMLInputElement;
  const listen = h("input", { type: "text", inputmode: "numeric", value: String(prefs.listen) }) as HTMLInputElement;
  const mode = h("select", { class: "picker" },
    h("option", { value: "default" }, "Built-in Nexus peers"),
    h("option", { value: "custom" }, "My peers"),
    h("option", { value: "none" }, "No peers (private)"),
  ) as HTMLSelectElement;
  mode.value = Array.isArray(prefs.peers) ? "custom" : prefs.peers;
  const peers = h("textarea", { placeholder: "publicKey@host:port, one per line", spellcheck: "false" }) as HTMLTextAreaElement;
  peers.value = Array.isArray(prefs.peers) ? prefs.peers.join("\n") : "";
  const hosted = h("textarea", { placeholder: "child chains to host, e.g. Nexus/testnet, one per line", spellcheck: "false" }) as HTMLTextAreaElement;
  hosted.value = prefs.hostedChains.join("\n");
  const err = h("div", { class: "toast" });
  render(
    h("div", { class: "stack" },
      h("h1", {}, "Node settings"),
      h("label", { class: "k" }, "RPC port (loopback only)"), rpc,
      h("label", { class: "k" }, "Peer port"), listen,
      h("label", { class: "k" }, "Nexus bootstrap peers"), mode, peers,
      h("label", { class: "k" }, "Hosted child chains"), hosted,
      err,
      h("button", { class: "block", onclick: async () => {
        const next: NodePrefs = {
          rpc: Number(rpc.value.trim()),
          listen: Number(listen.value.trim()),
          peers: parsePeersField(mode.value, peers.value),
          hostedChains: hosted.value.split(/[\s,]+/).filter(Boolean),
        };
        const problem = checkPrefs(next);
        if (problem) { err.textContent = problem; return; }
        err.textContent = "saving (a running node restarts)…";
        try {
          await manager.configure(next);
          if (next.rpc !== prefs.rpc) return onPrefsChanged();
          back();
        } catch (e) { err.textContent = (e as Error).message; }
      } }, "Save"),
      h("button", { class: "btn block", onclick: back }, "Back"),
    ),
  );
}

/** The recipient choices as entered: a chain set to "burn" is left out. */
export function miningChoices(workers: string, chosen: Record<string, string>): MiningPrefs {
  return {
    workers: Number(workers.trim()),
    recipients: Object.fromEntries(Object.entries(chosen).filter(([, address]) => address !== "")),
  };
}

/**
 * Mining on this node with this Mac's CPU: the bundled `lattice mine` loop,
 * paying each hosted chain's reward and fees to an account of this wallet.
 * One search covers Nexus and the chains it carries (merged mining).
 */
async function miningScreen(manager: NodeManager, reads: NodeReads, back: () => void) {
  const [chains, saved, accounts] = await Promise.all([manager.chains(), manager.miningPrefs(), reads.accounts()]);
  const cores = Math.max(1, Math.min(MAX_WORKERS, globalThis.navigator?.hardwareConcurrency ?? 1));
  const workers = h("input", { type: "text", inputmode: "numeric", "aria-label": "Workers", value: String(saved?.workers ?? 1) }) as HTMLInputElement;
  const pickers = new Map<string, HTMLSelectElement>();
  for (const chain of chains) {
    // A fresh setup pays the active account; a saved one shows what was saved,
    // an address that is not this wallet's included.
    const current = saved ? saved.recipients[chain] ?? "" : accounts[0]?.address ?? "";
    const picker = h("select", { class: "picker", "aria-label": `Recipient on ${chain}` },
      ...accounts.map((a) => h("option", { value: a.address }, `${a.label} · ${a.address.slice(0, 12)}…${a.address.slice(-8)}`)),
      ...(current && !accounts.some((a) => a.address === current) ? [h("option", { value: current }, `${current.slice(0, 12)}…${current.slice(-8)} (not in this wallet)`)] : []),
      h("option", { value: "" }, "No one (reward and fees burn)"),
    ) as HTMLSelectElement;
    picker.value = current;
    pickers.set(chain, picker);
  }
  const state = h("span", { class: "v" }, "…");
  const toast = h("div", { class: "toast", role: "status" });
  const toggle = h("button", { class: "block" }, "Start mining") as HTMLButtonElement;
  let mining = false;
  let busy = false;
  const show = async () => {
    try {
      const status = await manager.miningStatus();
      mining = status.running;
      state.textContent = mining ? `mining (pid ${status.pid})` : "not mining";
    } catch (e) { state.textContent = (e as Error).message; }
    toggle.textContent = mining ? "Stop mining" : "Start mining";
  };
  const chosen = () => miningChoices(workers.value, Object.fromEntries([...pickers].map(([chain, picker]) => [chain, picker.value])));
  const act = (label: string, run: () => Promise<void>) => async () => {
    if (busy) return;
    busy = true;
    toast.textContent = label + "…";
    try { await run(); toast.textContent = ""; } catch (e) { toast.textContent = (e as Error).message; }
    busy = false;
    await show();
  };
  const save = async () => {
    const prefs = chosen();
    const problem = checkMining(prefs, chains);
    if (problem) throw new Error(problem);
    await manager.configureMining(prefs);
  };
  toggle.addEventListener("click", () => {
    void (mining
      ? act("stopping (it finishes the round it is on)", () => manager.stopMining())()
      : act("starting", async () => { await save(); await manager.startMining(); })());
  });
  render(
    h("div", { class: "stack" },
      h("h1", {}, "Mining"),
      h("p", { class: "muted" }, "Mine on your node with this Mac's processor. One search covers Nexus and every chain your node hosts; each block found pays that chain's reward and fees to the account you choose here."),
      h("div", { class: "kv" }, h("div", { class: "row" }, h("span", { class: "k" }, "Status"), state)),
      ...chains.flatMap((chain) => [h("label", { class: "k" }, `${chain} pays`), pickers.get(chain)!]),
      ...(accounts.length ? [] : [h("p", { class: "warn" }, "The wallet is locked or has no account, so there is no one of yours to pay. Unlock it first.")]),
      h("label", { class: "k" }, `Workers (this Mac has ${cores} cores)`), workers,
      toast,
      toggle,
      h("button", { class: "btn block", onclick: act("saving (a running miner restarts)", save) }, "Save"),
      h("button", { class: "btn block", onclick: back }, "Back"),
    ),
  );
  await show();
}

/**
 * Pairing the Lattice Wallet extension with this node: the node lets the
 * extension's origin reach its loopback operator port (rpcAllowedOrigins,
 * written here on request; the node restarts), and the extension holds the
 * node's cookie, which the node rewrites at every start.
 */
async function pairScreen(manager: NodeManager, reads: NodeReads, back: () => void) {
  const id = h("input", { type: "text", placeholder: "extension id (chrome://extensions) or chrome-extension://…", spellcheck: "false" }) as HTMLInputElement;
  const toast = h("div", { class: "toast" });
  const allowed = h("div", { class: "kv" });
  const showAllowed = async () => {
    const origins = await manager.allowedOrigins();
    allowed.replaceChildren(...(origins.length
      ? origins.map((o) => h("div", { class: "row" }, h("span", { class: "v mono" }, o)))
      : [h("div", { class: "row" }, h("span", { class: "muted" }, "No browser origin is allowed yet."))]));
  };
  render(
    h("div", { class: "stack" },
      h("h1", {}, "Pair browser extension"),
      h("p", { class: "muted" }, "1. Allow the extension's origin. Find its id on chrome://extensions. The node restarts to take it."),
      id,
      h("button", { class: "block", onclick: async () => {
        const origin = extensionOrigin(id.value);
        if (!origin) { toast.textContent = "Enter the extension's id (32 letters) or its chrome-extension:// origin."; return; }
        toast.textContent = "allowing (a running node restarts)…";
        try { await manager.allowOrigin(origin); toast.textContent = "Allowed " + origin; } catch (e) { toast.textContent = (e as Error).message; }
        showAllowed();
      } }, "Allow this extension"),
      allowed,
      h("p", { class: "muted" }, `2. In the extension's Node screen, enter ${await manager.rpcURL()} and paste the node's cookie. The node writes a new cookie each time it starts, so pair again after a restart.`),
      h("button", { class: "btn block", onclick: async () => {
        const cookie = await reads.cookie().catch(() => null);
        if (!cookie) { toast.textContent = "The node is not running: start it first."; return; }
        try { await navigator.clipboard.writeText(cookie); toast.textContent = "Cookie copied."; } catch { toast.textContent = cookie; }
      } }, "Copy the node's cookie"),
      toast,
      h("button", { class: "btn block", onclick: back }, "Back"),
    ),
  );
  showAllowed();
}
