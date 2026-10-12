// The desktop's node screen: start/stop/status of the app's own node, its
// sync heights, its settings (ports, Nexus bootstrap peers, hosted child
// chains), and mining on it. Rendered with the wallet's own helpers and styles.

import { h, render } from "../vendor/nexus-wallet/src/popup/app.ts";
import { checkMining, checkPrefs, extensionOrigin, MAX_WORKERS, parseMiningLog, type ChainStatus, type MiningPrefs, type NodeManager, type NodePrefs, type Searcher } from "./node-manager.ts";

/** What the panel reads beyond `lattice status`: a chain's /health and children, the node's cookie, and the wallet's accounts. */
export interface NodeReads {
  health(path: string): Promise<{ height?: bigint; bestHeaderHeight?: bigint }>;
  /** The child chains `path`'s latest block carries, as full paths. */
  children(path: string): Promise<string[]>;
  /** How many peers the node is connected to. */
  peers(): Promise<number>;
  cookie(): Promise<string | null>;
  /** The wallet's accounts, the active one first: who mining can pay. Empty while locked. */
  accounts(): Promise<{ label: string; address: string }[]>;
}

const POLL_MS = 3000;

/** A chain's state in the words of someone using the app, not running the node. */
export function statusLine(s: ChainStatus, connected = true): string {
  if (s.state === "down") return "stopped";
  if (s.state === "unreachable") return "starting…";
  if (s.state === "awaitingGenesis") return "waiting for its first block";
  const behind = s.height !== undefined && s.bestHeaderHeight !== undefined && s.bestHeaderHeight > s.height;
  if (behind) return `syncing · ${s.height!.toLocaleString()} of ${s.bestHeaderHeight!.toLocaleString()}`;
  if (s.state !== "active") return s.state;
  const height = s.height === undefined ? "" : ` · block ${s.height.toLocaleString()}`;
  const pool = s.mempool ? ` · ${s.mempool} pending` : "";
  // "In sync" is a claim about the network; a node with no peers cannot make it.
  return `${connected ? "in sync" : "running"}${height}${pool}`;
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

export interface Notice {
  text: string;
  action?: { label: string; run(): Promise<void> };
}

/**
 * What the wallet's home screen should say about this node before its numbers
 * are trusted, for the chain in view: stopped (with the way to start it),
 * starting, waiting, or syncing; and that it is mining, when it is. Nothing
 * when the node is in sync and idle.
 */
/** What a node with no peers should say: it runs, and sees only itself. */
export function alone(peers: NodePrefs["peers"]): string {
  return peers === "none"
    ? "Your node is set to connect to no one, so it sees only its own blocks. To join the network, choose its peers on the Node page."
    : "Your node has no peers yet, so it cannot see the network. It keeps trying.";
}

export async function nodeNotices(manager: NodeManager, reads: Pick<NodeReads, "health" | "peers">, chain: string): Promise<Notice[]> {
  const chains = await withSync(await manager.status(), reads);
  if (chains.every((c) => c.state === "down")) {
    return [{ text: "Your node is stopped, so nothing can be read or sent.", action: { label: "Start node", run: () => manager.start() } }];
  }
  const notices: Notice[] = [];
  const shown = chains.find((c) => c.path === chain);
  const root = chains[0];
  const line = (c: ChainStatus) => statusLine(c);
  if (chains.some((c) => c.state === "unreachable")) notices.push({ text: "Your node is starting…" });
  else if (shown === undefined) notices.push({ text: `Your node does not host ${chain}.` });
  else if (shown.state === "awaitingGenesis") notices.push({ text: `${chain} is waiting for its first block on your node.` });
  else if (line(shown).startsWith("syncing")) notices.push({ text: `${chain} is ${line(shown)}. Balances may be behind.` });
  else if (root && root !== shown && line(root).startsWith("syncing")) notices.push({ text: `${root.path} is ${line(root)}. ${chain} follows it.` });
  // Running is not the same as connected: a node with no peers answers every
  // read, at whatever height it has, and would otherwise look in sync.
  if (!chains.some((c) => c.state === "unreachable") && (await reads.peers().catch(() => undefined)) === 0) {
    notices.push({ text: alone((await manager.prefs()).peers) });
  }
  if ((await manager.miningStatus().catch(() => ({ running: false }))).running) notices.push({ text: "Mining on this computer." });
  return notices;
}

/**
 * The node on one page: what it is doing, starting and stopping it, and its
 * settings. `saved` is where a save of the settings goes instead of back to
 * this page (the Mining screen sends people here to host a chain, and wants
 * them back).
 */
export function nodeScreen(manager: NodeManager, reads: NodeReads, back: () => void, onPrefsChanged: () => void, saved?: () => void) {
  const table = h("div", { class: "kv" }, h("div", { class: "row" }, h("span", { class: "muted" }, "…")));
  const toast = h("div", { class: "toast", role: "status" });
  // One button, saying the one thing it will do: the node is either running or it is not.
  const toggle = h("button", { class: "block" }, "…") as HTMLButtonElement;
  const lonely = h("p", { class: "warn", role: "status", hidden: "true" });
  let running = false;
  let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const again = (savedTo?: () => void) => nodeScreen(manager, reads, back, onPrefsChanged, savedTo);
  const leave = (go: () => void) => () => { clearTimeout(timer); go(); };
  // The node's settings, on the same page as what they change. Filled once
  // read; the status above keeps refreshing without touching what is typed.
  const settings = h("div", { class: "stack" });
  void settingsForm(manager, reads, leave(saved ?? (() => again())), onPrefsChanged)
    .then((form) => {
      settings.replaceChildren(...form);
      // Sent here to host a chain: bring that part into view.
      if (saved) document.getElementById("hosted-chains")?.scrollIntoView?.();
    })
    .catch((e) => settings.replaceChildren(h("p", { class: "warn" }, (e as Error).message)));

  const refresh = async () => {
    clearTimeout(timer);
    try {
      const [chains, mining, prefs] = await Promise.all([
        manager.status().then((status) => withSync(status, reads)),
        manager.miningStatus().catch(() => ({ running: false })),
        manager.prefs(),
      ]);
      running = chains.some((c) => c.state !== "down");
      const peers = running ? await reads.peers().catch(() => undefined) : undefined;
      lonely.hidden = peers !== 0;
      lonely.textContent = peers === 0 ? alone(prefs.peers) : "";
      table.replaceChildren(
        ...chains.map((c) => h("div", { class: "row" },
          h("span", { class: "k mono" }, c.path),
          h("span", { class: "v" }, statusLine(c, peers !== 0)),
        )),
        ...(peers === undefined ? [] : [h("div", { class: "row" }, h("span", { class: "k" }, "Peers"), h("span", { class: "v" }, peers === 0 ? "none" : peers.toLocaleString()))]),
        h("div", { class: "row" }, h("span", { class: "k" }, "Mining"), h("span", { class: "v" }, mining.running ? "on" : "off")),
      );
    } catch (e) {
      table.replaceChildren(h("div", { class: "row" }, h("span", { class: "warn" }, (e as Error).message)));
    }
    if (!busy) {
      toggle.textContent = running ? "Stop node" : "Start node";
      // Starting is the step forward; stopping is not, so it is not the filled button.
      toggle.className = running ? "btn block" : "block";
    }
    if (document.body.contains(table)) timer = setTimeout(refresh, POLL_MS);
  };

  toggle.addEventListener("click", async () => {
    if (busy) return;
    busy = true;
    toggle.disabled = true;
    toast.textContent = running ? "stopping (mining stops with it)…" : "starting…";
    try {
      await (running ? manager.stop() : manager.start());
      toast.textContent = "";
    } catch (e) {
      toast.textContent = (e as Error).message;
    } finally {
      busy = false;
      toggle.disabled = false;
      void refresh();
    }
  });

  render(
    h("div", { class: "stack" },
      h("h1", {}, "Your node"),
      h("p", { class: "muted" }, "A full Lattice node on this computer. The wallet reads and sends only through it."),
      table,
      lonely,
      toggle,
      toast,
      h("button", { class: "btn block", onclick: leave(() => miningScreen(manager, reads, again, onPrefsChanged)) }, "Mining"),
      h("button", { class: "btn block", onclick: leave(() => pairScreen(manager, reads, () => again())) }, "Pair browser extension"),
      settings,
      h("button", { class: "btn block", onclick: leave(back) }, "Back to wallet"),
    ),
  );
  void refresh();
}

/** Parents before children, then by name: the order lattice.json wants, and the order to read them in. */
const byDepth = (a: string, b: string) => a.split("/").length - b.split("/").length || a.localeCompare(b);

/**
 * The chains the node page offers to host: those hosted, and the child chains
 * the hosted ones carry (what the node can see without being told a path).
 */
export async function hostableChains(hosted: string[], reads: Pick<NodeReads, "children">): Promise<string[]> {
  const found = new Set(hosted);
  await Promise.all(["Nexus", ...hosted].map(async (chain) => {
    for (const child of await reads.children(chain).catch(() => [])) found.add(child);
  }));
  return [...found].sort(byDepth);
}

/** The chains ticked, with a typed path added: parents first, each once. */
export function chosenChains(ticked: string[], typed: string): string[] {
  return [...new Set([...ticked, ...typed.split(/[\s,]+/).filter(Boolean)])].sort(byDepth);
}

/** The node's settings as a form: hosted chains, peers, ports, and Save. `saved` runs once they are written. */
async function settingsForm(manager: NodeManager, reads: NodeReads, saved: () => void, onPrefsChanged: () => void) {
  const prefs = await manager.prefs();
  const offered = await hostableChains(prefs.hostedChains, reads);
  // Recognised, not recalled: the chains the node can see are ticked or not; a path is typed only for one it cannot.
  const boxes = offered.map((chain) => {
    const box = h("input", { type: "checkbox", "aria-label": chain, id: `host-${chain}` }) as HTMLInputElement;
    box.checked = prefs.hostedChains.includes(chain);
    return { chain, box };
  });
  const other = h("input", { type: "text", placeholder: "another chain's path, e.g. Nexus/testnet", spellcheck: "false", "aria-label": "Another chain" }) as HTMLInputElement;
  const mode = h("select", { class: "picker", "aria-label": "Peers" },
    h("option", { value: "default" }, "The network's built-in peers"),
    h("option", { value: "custom" }, "Only peers I list"),
    h("option", { value: "none" }, "No one (a private node)"),
  ) as HTMLSelectElement;
  mode.value = Array.isArray(prefs.peers) ? "custom" : prefs.peers;
  const peers = h("textarea", { placeholder: "publicKey@host:port, one per line", spellcheck: "false", "aria-label": "Peer list" }) as HTMLTextAreaElement;
  peers.value = Array.isArray(prefs.peers) ? prefs.peers.join("\n") : "";
  // Shown only when it is what was chosen.
  const showPeers = () => { peers.hidden = mode.value !== "custom"; };
  mode.addEventListener("change", showPeers);
  showPeers();
  const rpc = h("input", { type: "text", inputmode: "numeric", "aria-label": "Local API port", value: String(prefs.rpc) }) as HTMLInputElement;
  const listen = h("input", { type: "text", inputmode: "numeric", "aria-label": "Peer port", value: String(prefs.listen) }) as HTMLInputElement;
  const err = h("div", { class: "toast", role: "status" });
  return [
    h("p", { class: "section-label", id: "hosted-chains" }, "Chains this node hosts"),
    h("div", { class: "kv" },
      h("label", { class: "row check" }, h("span", {}, "Nexus"), h("span", { class: "muted" }, "always")),
      ...boxes.map(({ chain, box }) => h("label", { class: "row check", for: box.id }, h("span", { class: "mono" }, chain), box)),
    ),
    other,
    h("p", { class: "section-label" }, "Connect to"),
    mode, peers,
    // Rarely needed, so out of the way until asked for.
    h("details", { class: "advanced" },
      h("summary", {}, "Ports"),
      h("div", { class: "advanced-content stack" },
        h("label", { class: "k" }, "Local API port (this computer only)"), rpc,
        h("label", { class: "k" }, "Peer port"), listen,
      ),
    ),
    h("p", { class: "muted" }, "Saving restarts the node if it is running."),
    err,
    h("button", { class: "block", onclick: async () => {
      const next: NodePrefs = {
        rpc: Number(rpc.value.trim()),
        listen: Number(listen.value.trim()),
        peers: parsePeersField(mode.value, peers.value),
        hostedChains: chosenChains(boxes.filter(({ box }) => box.checked).map(({ chain }) => chain), other.value),
      };
      const problem = checkPrefs(next);
      if (problem) { err.textContent = problem; return; }
      err.textContent = "saving…";
      try {
        await manager.configure(next);
        if (next.rpc !== prefs.rpc) return onPrefsChanged();
        saved();
      } catch (e) { err.textContent = (e as Error).message; }
    } }, "Save"),
  ];
}

/** The recipient choices as entered: a chain set to pay no one is left out. */
export function miningChoices(searcher: Searcher, workers: string, chosen: Record<string, string>): MiningPrefs {
  return {
    searcher,
    // The graphics chip is one worker.
    workers: searcher === "gpu" ? 1 : Number(workers.trim()),
    recipients: Object.fromEntries(Object.entries(chosen).filter(([, address]) => address !== "")),
  };
}

const SEARCHERS: Record<Searcher, string> = { gpu: "the graphics chip", cpu: "the processor", custom: "the worker set in lattice.json" };

/**
 * Mining on this node with this computer: the bundled `lattice mine` loop, on the
 * graphics chip or the processor, paying each hosted chain's reward and fees
 * to an account of this wallet. One search covers Nexus and the chains it
 * carries (merged mining), so the chains mined are the chains hosted.
 */
async function miningScreen(manager: NodeManager, reads: NodeReads, back: (saved?: () => void) => void, onPrefsChanged: () => void) {
  const [chains, saved, accounts, gpu] = await Promise.all([manager.chains(), manager.miningPrefs(), reads.accounts(), manager.canMineOnGPU()]);
  const again = () => miningScreen(manager, reads, back, onPrefsChanged);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const leave = (go: () => void) => () => { clearTimeout(timer); go(); };
  // A fresh setup searches on the graphics chip when the app carries its worker.
  const searcher = h("select", { class: "picker", "aria-label": "Search with" },
    ...(gpu ? [h("option", { value: "gpu" }, "This computer's graphics chip")] : []),
    h("option", { value: "cpu" }, "This computer's processor"),
    ...(saved?.searcher === "custom" ? [h("option", { value: "custom" }, "The worker set in lattice.json")] : []),
  ) as HTMLSelectElement;
  // Saved for a graphics chip this computer cannot use (now): the processor, until it is saved otherwise.
  searcher.value = saved && (saved.searcher !== "gpu" || gpu) ? saved.searcher : gpu ? "gpu" : "cpu";
  const cores = Math.max(1, Math.min(MAX_WORKERS, globalThis.navigator?.hardwareConcurrency ?? 1));
  const workers = h("input", { type: "text", inputmode: "numeric", "aria-label": "Workers", value: String(saved?.workers ?? 1) }) as HTMLInputElement;
  const workersField = h("div", { class: "stack" }, h("label", { class: "k" }, `Processor workers (this computer has ${cores} cores)`), workers);
  const pickers = new Map<string, HTMLSelectElement>();
  for (const chain of chains) {
    // A fresh setup pays the active account; a saved one shows what was saved,
    // an address that is not this wallet's included.
    const current = saved ? saved.recipients[chain] ?? "" : accounts[0]?.address ?? "";
    const picker = h("select", { class: "picker", "aria-label": `Recipient on ${chain}` },
      ...accounts.map((a) => h("option", { value: a.address }, `${a.label} · ${a.address.slice(0, 12)}…${a.address.slice(-8)}`)),
      ...(current && !accounts.some((a) => a.address === current) ? [h("option", { value: current }, `${current.slice(0, 12)}…${current.slice(-8)} (not in this wallet)`)] : []),
      h("option", { value: "" }, "No one"),
    ) as HTMLSelectElement;
    picker.value = current;
    pickers.set(chain, picker);
  }
  // Paying no one is allowed, and costly: it is said out loud before it happens.
  const burn = h("p", { class: "warn", role: "status" });
  const state = h("span", { class: "v" }, "…");
  const session = h("span", { class: "v" }, "");
  const sessionRow = h("div", { class: "row" }, h("span", { class: "k" }, "This run"), session);
  const toast = h("div", { class: "toast", role: "status" });
  const toggle = h("button", { class: "block" }, "Start mining") as HTMLButtonElement;
  let mining = false;
  let busy = false;
  const reflect = () => {
    const unpaid = [...pickers].filter(([, picker]) => picker.value === "").map(([chain]) => chain);
    burn.hidden = unpaid.length === 0;
    burn.textContent = unpaid.length ? `${unpaid.join(" and ")} will pay no one: every reward and fee mined there is destroyed.` : "";
    workersField.hidden = searcher.value !== "cpu";
  };
  searcher.addEventListener("change", reflect);
  for (const picker of pickers.values()) picker.addEventListener("change", reflect);
  reflect();
  const apply = h("button", { class: "btn block" }, "Apply changes") as HTMLButtonElement;
  const show = async () => {
    clearTimeout(timer);
    try {
      const [status, log] = await Promise.all([manager.miningStatus(), manager.miningLog().catch(() => "")]);
      mining = status.running;
      state.textContent = mining ? `mining on ${SEARCHERS[saved?.searcher ?? (searcher.value as Searcher)]}` : "not mining";
      // Proof that it is working, in the only terms that matter: blocks found.
      const run = parseMiningLog(log);
      sessionRow.hidden = !mining;
      session.textContent = run.found === 0 ? "no block found yet" : `${run.found.toLocaleString()} ${run.found === 1 ? "block" : "blocks"} found · last at ${run.last ? new Date(run.last).toLocaleTimeString() : "?"}`;
    } catch (e) { state.textContent = (e as Error).message; }
    if (!busy) {
      toggle.textContent = mining ? "Stop mining" : "Start mining";
      // Starting is the step forward; stopping is not, so it is not the filled button.
      toggle.className = mining ? "btn block" : "block";
    }
    // Stopped, starting saves what is chosen; running, a change has its own button.
    apply.hidden = !mining;
    if (document.body.contains(toggle)) timer = setTimeout(show, POLL_MS);
  };
  const chosen = () => miningChoices(searcher.value as Searcher, workers.value, Object.fromEntries([...pickers].map(([chain, picker]) => [chain, picker.value])));
  const act = (label: string, run: () => Promise<void>, then?: () => void) => async () => {
    if (busy) return;
    busy = true;
    toggle.disabled = apply.disabled = true;
    toast.textContent = label + "…";
    let done = false;
    try { await run(); toast.textContent = ""; done = true; } catch (e) { toast.textContent = (e as Error).message; }
    busy = false;
    toggle.disabled = apply.disabled = false;
    if (done && then) return then();
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
      : act("starting", async () => { await save(); await manager.startMining(); }, leave(again))());
  });
  apply.addEventListener("click", () => { void act("applying (mining restarts)", save, leave(again))(); });
  render(
    h("div", { class: "stack" },
      h("h1", {}, "Mining"),
      h("p", { class: "muted" }, "Mine on your node with this computer. One search covers every chain your node hosts; each block found pays that chain's reward and fees to the account you choose."),
      h("div", { class: "kv" }, h("div", { class: "row" }, h("span", { class: "k" }, "Status"), state), sessionRow),
      h("label", { class: "k" }, "Search with"), searcher,
      workersField,
      ...chains.flatMap((chain) => [h("label", { class: "k" }, `${chain} pays`), pickers.get(chain)!]),
      burn,
      ...(accounts.length ? [] : [h("p", { class: "warn" }, "The wallet is locked or has no account, so there is no one of yours to pay. Unlock it first.")]),
      h("p", { class: "muted" }, `Chains mined are the chains hosted: ${chains.join(", ")}.`),
      toast,
      toggle,
      apply,
      h("button", { class: "btn block", onclick: leave(() => back(again)) }, "Choose hosted chains"),
      h("button", { class: "btn block", onclick: leave(() => back()) }, "Back"),
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
  const id = h("input", { type: "text", placeholder: "the extension's id", spellcheck: "false" }) as HTMLInputElement;
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
