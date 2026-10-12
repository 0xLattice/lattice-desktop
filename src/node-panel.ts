// The desktop's node screen: start/stop/status of the app's own node, its
// sync heights, the chains it hosts, each chain's peers, its ports; and, separately,
// mining on it. Rendered with the wallet's own helpers and styles.

import { h, navRow, outline, render } from "../vendor/nexus-wallet/src/popup/app.ts";
import { checkMining, checkPrefs, extensionOrigin, MAX_WORKERS, parseMiningLog, type ChainStatus, type MiningPrefs, type NodeManager, type NodePrefs, type Searcher } from "./node-manager.ts";

/** What the panel reads beyond `lattice status`: a chain's /health and children, the node's cookie, and the wallet's accounts. */
export interface NodeReads {
  health(path: string): Promise<{ height?: bigint; bestHeaderHeight?: bigint }>;
  /** The child chains `path`'s latest block carries, as full paths. */
  children(path: string): Promise<string[]>;
  /** How many of the node's peers serve the chain at `path`. */
  peers(path: string): Promise<number>;
  cookie(): Promise<string | null>;
  /** The wallet's accounts, the active one first: who mining can pay. Empty while locked. */
  accounts(): Promise<{ label: string; address: string }[]>;
}

const POLL_MS = 3000;

/**
 * What a chain needs said about it, in the words of someone using the app:
 * that it has no block yet, or how far behind it is. A chain that is caught
 * up says nothing; its block and its peers are the whole of it.
 */
export function chainWords(s: ChainStatus): string | undefined {
  if (s.state === "down" || s.state === "unreachable") return undefined;
  if (s.state === "awaitingGenesis") return "waiting for its first block";
  const behind = s.height !== undefined && s.bestHeaderHeight !== undefined && s.bestHeaderHeight > s.height;
  if (behind) return `syncing ${s.height!.toLocaleString()} of ${s.bestHeaderHeight!.toLocaleString()}`;
  return s.state === "active" ? undefined : s.state;
}

/** The node in a few words, for the row that leads to its page: whether it runs, how many chains, how many peers. */
export async function nodeSummary(manager: NodeManager, reads: Pick<NodeReads, "peers">): Promise<string> {
  const chains = await manager.status();
  if (chains.every((c) => c.state === "down")) return "stopped";
  if (chains.some((c) => c.state === "unreachable")) return "starting";
  const peers = await reads.peers("Nexus").catch(() => undefined);
  return ["running", `${chains.length} ${chains.length === 1 ? "chain" : "chains"}`, ...(peers === undefined ? [] : [peersLine(peers)])].join(" · ");
}

/** The node as a whole, said once: its chains' rows do not repeat it. */
export function nodeLine(chains: ChainStatus[]): string {
  if (chains.every((c) => c.state === "down")) return "Stopped. Nothing can be read or sent.";
  if (chains.some((c) => c.state === "unreachable")) return "Starting. It checks its stored chains before it answers, which can take a minute.";
  return "Running";
}

/** A chain's row: words when something needs saying, otherwise its block and its peers. */
export function rowCells(s: ChainStatus, peers: number | undefined): { words?: string; block: string; peers: string } {
  const words = chainWords(s);
  if (words !== undefined) return { words, block: "", peers: "" };
  const up = s.state !== "down" && s.state !== "unreachable";
  return {
    block: s.height === undefined ? "" : s.height.toLocaleString(),
    peers: !up || peers === undefined ? "" : peers === 0 ? "no peers" : peers.toLocaleString(),
  };
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

/** What a child chain with no peers of its own should say, on a node that has peers. */
export function aloneOn(chain: string): string {
  return `None of your node's peers host ${chain} yet, so it cannot see new blocks there. It keeps looking.`;
}

/** A chain's own peers, in a row. */
export function peersLine(peers: number): string {
  return peers === 0 ? "no peers" : peers === 1 ? "1 peer" : `${peers.toLocaleString()} peers`;
}

export async function nodeNotices(manager: NodeManager, reads: Pick<NodeReads, "health" | "peers">, chain: string): Promise<Notice[]> {
  const chains = await withSync(await manager.status(), reads);
  if (chains.every((c) => c.state === "down")) {
    return [{ text: "Your node is stopped, so nothing can be read or sent.", action: { label: "Start node", run: () => manager.start() } }];
  }
  const notices: Notice[] = [];
  const shown = chains.find((c) => c.path === chain);
  const root = chains[0];
  const line = (c: ChainStatus) => chainWords(c) ?? "";
  if (chains.some((c) => c.state === "unreachable")) notices.push({ text: "Your node is starting. It checks its stored chains before it answers, which can take a minute." });
  else if (shown === undefined) notices.push({ text: `Your node does not host ${chain}.` });
  else if (shown.state === "awaitingGenesis") notices.push({ text: `${chain} is waiting for its first block on your node.` });
  else if (line(shown).startsWith("syncing")) notices.push({ text: `${chain} is ${line(shown)}. Balances may be behind.` });
  else if (root && root !== shown && line(root).startsWith("syncing")) notices.push({ text: `${root.path} is ${line(root)}. ${chain} follows it.` });
  // Running is not the same as connected: a node with no peers answers every
  // read, at whatever height it has, and would otherwise look in sync.
  // It is said of the chain in view: Nexus without peers is the node cut
  // off; a child without peers on a connected node is that chain alone.
  if (!chains.some((c) => c.state === "unreachable") && root) {
    const count = (path: string) => reads.peers(path).catch(() => undefined);
    if ((await count(root.path)) === 0) notices.push({ text: alone((await manager.prefs()).peers) });
    else if (shown && shown !== root && shown.state === "active" && (await count(shown.path)) === 0) notices.push({ text: aloneOn(shown.path) });
  }
  if ((await manager.miningStatus().catch(() => ({ running: false }))).running) notices.push({ text: "Mining on this computer." });
  return notices;
}

/** Where the node's pages go when they are done, and what they are made of. */
interface NodePages {
  manager: NodeManager;
  reads: NodeReads;
  back: () => void;
  onPrefsChanged: () => void;
  /**
   * Where a change of the hosted chains goes instead of back to the page it
   * was made on (the Mining screen sends people here to host a chain, and
   * wants them back).
   */
  saved?: () => void;
}

interface TreeRow {
  path: string;
  /** Absent: a chain the node can see and does not host. */
  status?: ChainStatus;
  peers?: number;
}

const nameOf = (path: string) => path.split("/").pop()!;
const parentOf = (path: string) => path.split("/").slice(0, -1).join("/");

/**
 * Chains as they are: a tree. One row a chain, a child under its parent and
 * named by the last part of its path, each with the same two numbers.
 * Pressing a row opens that chain. Rows are kept between updates, so the one
 * in focus stays in focus.
 */
function chainTree(open: (path: string) => void) {
  const list = h("ul", {});
  const box = h("div", { class: "tree" },
    h("div", { class: "cols", "aria-hidden": "true" }, h("span", {}, "Chain"), h("span", {}, "Block"), h("span", {}, "Peers"), h("span", {})),
    list,
  );
  const cells = new Map<string, { row: HTMLElement; words: HTMLElement; block: HTMLElement; peers: HTMLElement }>();
  let shape = "";
  const build = (rows: TreeRow[]) => {
    cells.clear();
    const status = new Map(rows.map((r) => [r.path, r.status]));
    list.replaceChildren(...outline(rows.map((r) => r.path), (path, guide) => {
      const [words, block, peers] = [h("span", { class: "words" }), h("span", { class: "n" }), h("span", { class: "n" })];
      const row = h("button", { class: status.get(path) ? "chain" : "chain off", "data-chain": path, onclick: () => open(path) },
        h("span", { class: "name" }, h("span", { class: "tree-guide", "aria-hidden": "true" }, guide), h("span", {}, nameOf(path))),
        words, block, peers,
        h("span", { class: "go", "aria-hidden": "true" }, "›"),
      );
      cells.set(path, { row, words, block, peers });
      return row;
    }));
  };
  return {
    box,
    update(rows: TreeRow[]) {
      const next = rows.map((r) => `${r.path}:${r.status ? 1 : 0}`).join("|");
      if (next !== shape) { build(rows); shape = next; }
      for (const r of rows) {
        const cell = cells.get(r.path)!;
        const said = r.status ? rowCells(r.status, r.peers) : { words: "not hosted", block: "", peers: "" };
        cell.words.textContent = said.words ?? "";
        cell.words.hidden = said.words === undefined;
        cell.block.textContent = said.block;
        cell.peers.textContent = said.peers;
        cell.block.hidden = cell.peers.hidden = said.words !== undefined;
        // Read out whole: the row's cells carry no units of their own.
        cell.row.setAttribute("aria-label", said.words !== undefined ? `${r.path}: ${said.words}`
          : [r.path, said.block && `block ${said.block}`, said.peers && (said.peers === "no peers" ? said.peers : peersLine(r.peers!))].filter(Boolean).join(", "));
      }
    },
  };
}

/** The tree's rows: the chains hosted, with their peers, and the chains the node can see under them. */
async function treeRows(chains: ChainStatus[], hosted: string[], reads: NodeReads): Promise<TreeRow[]> {
  const peers = new Map(await Promise.all(chains.map(async (c) =>
    [c.path, c.state === "down" || c.state === "unreachable" ? undefined : await reads.peers(c.path).catch(() => undefined)] as const)));
  const status = new Map(chains.map((c) => [c.path, c]));
  const seen = await hostableChains(hosted, reads);
  return [...new Set([...chains.map((c) => c.path), ...seen])].sort(byDepth)
    .map((path) => ({ path, status: status.get(path), peers: peers.get(path) }));
}

/**
 * Changes which chains are hosted (the node restarts to take it), then goes
 * on: to `pages.saved` when there is one, otherwise to `then`. Whoever left
 * `screen` meanwhile is not pulled back.
 */
async function setHosted(pages: NodePages, screen: HTMLElement, toast: HTMLElement, change: (hosted: string[]) => string[], doing: string, then: () => void) {
  toast.textContent = doing;
  try {
    const prefs = await pages.manager.prefs();
    const next = { ...prefs, hostedChains: change(prefs.hostedChains) };
    const problem = checkPrefs(next);
    if (problem) { toast.textContent = problem; return; }
    await pages.manager.configure(next);
    if (document.body.contains(screen)) (pages.saved ?? then)();
  } catch (e) {
    toast.textContent = (e as Error).message;
  }
}

/**
 * The node on one page: whether it runs, and its chains as a tree, each with
 * its block and its peers. Everything that can be changed about a chain is
 * on that chain's own page; the node's ports and pairing are folded away.
 */
export function nodeScreen(manager: NodeManager, reads: NodeReads, back: () => void, onPrefsChanged: () => void, saved?: () => void, chain?: string) {
  const pages = { manager, reads, back, onPrefsChanged, saved };
  // Opened for one chain (the wallet's chain menu does this), it opens on that chain's page.
  if (chain) chainPage(pages, chain); else nodePage(pages);
}

function nodePage(pages: NodePages) {
  const { manager, reads } = pages;
  const summary = h("p", { class: "muted", role: "status" }, "…");
  // One button, saying the one thing it will do: the node is either running or it is not.
  const toggle = h("button", { class: "btn" }, "…") as HTMLButtonElement;
  const lonely = h("p", { class: "warn", role: "status", hidden: "true" });
  const toast = h("div", { class: "toast", role: "status" });
  let running = false;
  let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const leave = (go: () => void) => () => { clearTimeout(timer); go(); };
  const tree = chainTree((path) => leave(() => chainPage(pages, path))());
  const screen = h("div", { class: "stack" });

  const refresh = async () => {
    clearTimeout(timer);
    try {
      const [chains, prefs] = await Promise.all([
        manager.status().then((status) => withSync(status, reads)),
        manager.prefs(),
      ]);
      running = chains.some((c) => c.state !== "down");
      const rows = await treeRows(chains, prefs.hostedChains, reads);
      tree.update(rows);
      summary.textContent = nodeLine(chains);
      // Without peers the node sees only itself; a child chain without them, on a node that has some, sees nothing new.
      const count = (path: string) => rows.find((r) => r.path === path)?.peers;
      const alones = count("Nexus") === 0 ? [alone(prefs.peers)]
        : rows.filter((r) => r.status?.state === "active" && r.peers === 0 && (count(parentOf(r.path)) ?? 0) > 0).map((r) => aloneOn(r.path));
      lonely.hidden = alones.length === 0;
      lonely.textContent = alones.join(" ");
      if (!busy) toast.textContent = "";
    } catch (e) {
      toast.textContent = (e as Error).message;
    }
    if (!busy) {
      toggle.textContent = running ? "Stop node" : "Start node";
      // Starting is the step forward; stopping is not, so it is not the filled button.
      toggle.className = running ? "btn" : "btn btn--primary";
    }
    if (document.body.contains(screen)) timer = setTimeout(refresh, POLL_MS);
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

  // Ports and pairing are set once, if ever: folded away until asked for.
  const ports = h("div", {});
  const advanced = h("details", { class: "advanced" }, h("summary", {}, "Advanced"), ports,
    h("div", { class: "advanced-content nav-list" },
      navRow("Pair browser extension", leave(() => pairScreen(manager, reads, () => nodePage(pages))))));
  void networkForm(manager, () => { if (document.body.contains(screen)) leave(() => nodePage(pages))(); }, pages.onPrefsChanged)
    .then((form) => ports.append(form))
    .catch((e) => ports.append(h("p", { class: "warn" }, (e as Error).message)));

  screen.append(
    h("div", { class: "node-head" },
      h("div", {}, h("h1", {}, "Your node"), summary),
      toggle,
    ),
    tree.box,
    lonely,
    toast,
    advanced,
    h("button", { class: "btn block", onclick: leave(pages.back) }, "Back to wallet"),
  );
  render(screen);
  void refresh();
}

/**
 * One chain's page, the same at every level: where it is in the tree, its
 * block and peers, the chains under it (and a name to host another there),
 * whom it connects to, and the way to stop hosting it. A chain the node sees
 * and does not host has one thing on its page: hosting it.
 */
function chainPage(pages: NodePages, path: string) {
  const { manager, reads } = pages;
  const name = nameOf(path);
  const parent = parentOf(path);
  const screen = h("div", { class: "stack" });
  const toast = h("div", { class: "toast", role: "status" });
  let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const leave = (go: () => void) => () => { clearTimeout(timer); go(); };
  const up = leave(() => (parent ? chainPage(pages, parent) : nodePage(pages)));
  const here = () => leave(() => chainPage(pages, path))();
  const change = async (to: (hosted: string[]) => string[], doing: string, then: () => void) => {
    if (busy) return;
    busy = true;
    try { await setHosted(pages, screen, toast, to, doing, leave(then)); } finally { busy = false; }
  };

  // Where this chain is: each chain above it is one press away.
  const parts = path.split("/");
  const crumbs = h("nav", { class: "crumbs", "aria-label": "Chain path" },
    h("button", { class: "link", onclick: leave(() => nodePage(pages)) }, "Your node"),
    ...parts.slice(0, -1).flatMap((part, i) => [
      h("span", { "aria-hidden": "true" }, "/"),
      h("button", { class: "link", onclick: leave(() => chainPage(pages, parts.slice(0, i + 1).join("/"))) }, part),
    ]),
    h("span", { "aria-hidden": "true" }, "/"), h("span", { "aria-current": "page" }, name),
  );
  const heading = [h("h1", { class: "chain-name" }, name), ...(parent ? [h("p", { class: "muted mono" }, path)] : [])];
  const body = h("div", { class: "stack" }, h("p", { class: "muted" }, "…"));
  screen.append(crumbs, ...heading, body, toast, h("button", { class: "btn block", onclick: up }, "Back"));
  render(screen);

  const block = h("span", { class: "v" });
  const peers = h("span", { class: "v" });
  const pending = h("span", { class: "v" });
  const pendingRow = h("div", { class: "row", hidden: "true" }, h("span", { class: "k" }, "Pending"), pending);
  const tree = chainTree((child) => leave(() => chainPage(pages, child))());
  const none = h("p", { class: "muted" }, "None yet.");
  let built: "hosted" | "seen" | undefined;

  const buildHosted = (prefs: NodePrefs) => {
    const other = h("input", { type: "text", placeholder: "name of a chain to host here", spellcheck: "false", "aria-label": `Name of a chain under ${path}` }) as HTMLInputElement;
    const add = () => {
      const typed = other.value.trim();
      if (!/^[A-Za-z0-9_-]{1,64}$/.test(typed)) { toast.textContent = "Type the chain's name: letters, digits, - and _."; return; }
      void change((hosted) => chosenChains(hosted, `${path}/${typed}`), `adding ${path}/${typed} (the node restarts)…`, () => chainPage(pages, path));
    };
    other.addEventListener("keydown", (event) => { if ((event as KeyboardEvent).key === "Enter") add(); });
    const confirm = h("div", { class: "warn stack", hidden: "true" });
    const stop = h("button", { class: "btn block", onclick: () => {
      const under = prefs.hostedChains.filter((chain) => chain.startsWith(path + "/")).length;
      confirm.replaceChildren(
        h("p", {}, `Stop hosting ${name}${under ? ` and the ${under === 1 ? "chain" : `${under} chains`} under it` : ""}? The wallet can no longer use ${under ? "them" : "it"}, and mining stops paying there. The node restarts.`),
        h("div", { class: "row-actions" },
          h("button", { class: "btn", "aria-label": `Stop hosting ${path}`, onclick: () => { void change((list) => list.filter((chain) => chain !== path && !chain.startsWith(path + "/")), `removing ${path} (the node restarts)…`, () => nodePage(pages)); } }, "Stop hosting"),
          h("button", { class: "btn", onclick: () => { confirm.hidden = true; stop.hidden = false; } }, "Keep it"),
        ),
      );
      confirm.hidden = false;
      stop.hidden = true;
    } }, `Stop hosting ${name}`);
    body.replaceChildren(
      h("div", { class: "kv" },
        h("div", { class: "row" }, h("span", { class: "k" }, "Block"), block),
        h("div", { class: "row" }, h("span", { class: "k" }, "Peers"), peers),
        pendingRow,
      ),
      h("div", { class: "stack" },
        h("h2", { class: "nav-label" }, `Chains under ${name}`),
        tree.box, none,
        h("div", { class: "add-chain" }, other, h("button", { class: "btn", onclick: add }, "Add")),
      ),
      connectForm(manager, prefs, path, () => { if (document.body.contains(screen)) here(); }),
      // Nexus is the root: always hosted. A chain goes with the chains under it.
      ...(parent ? [stop, confirm] : []),
    );
  };
  const buildSeen = () => {
    body.replaceChildren(
      h("p", {}, `Your node sees ${name} under ${parent} and does not host it. Hosting it lets the wallet use it, and lets you mine it.`),
      h("p", { class: "muted" }, "The node restarts to take it."),
      h("button", { class: "block", "aria-label": `Host ${path}`, onclick: () => { void change((hosted) => chosenChains(hosted, path), `adding ${path} (the node restarts)…`, () => chainPage(pages, path)); } }, `Host ${name}`),
    );
  };

  const refresh = async () => {
    clearTimeout(timer);
    try {
      const [chains, prefs] = await Promise.all([
        manager.status().then((status) => withSync(status, reads)),
        manager.prefs(),
      ]);
      const status = chains.find((c) => c.path === path);
      const kind = status ? "hosted" : "seen";
      if (built !== kind) { (status ? buildHosted : buildSeen)(prefs); built = kind; }
      if (status) {
        const rows = (await treeRows(chains, prefs.hostedChains, reads));
        const own = rows.find((r) => r.path === path)?.peers;
        const above = parent ? rows.find((r) => r.path === parent)?.peers : undefined;
        block.textContent = status.state === "down" ? "node stopped" : status.state === "unreachable" ? "starting…"
          : chainWords(status) ?? (status.height === undefined ? "" : status.height.toLocaleString());
        peers.textContent = own === undefined ? "" : own === 0 ? "none yet"
          : above === undefined ? own.toLocaleString() : `${own.toLocaleString()} of ${nameOf(parent)}'s ${above.toLocaleString()}`;
        pendingRow.hidden = !status.mempool;
        pending.textContent = status.mempool ? status.mempool.toLocaleString() : "";
        const below = rows.filter((r) => r.path.startsWith(path + "/"));
        tree.box.hidden = below.length === 0;
        none.hidden = below.length !== 0;
        tree.update(below);
      }
      if (!busy) toast.textContent = "";
    } catch (e) {
      if (!busy) toast.textContent = (e as Error).message;
    }
    if (document.body.contains(screen)) timer = setTimeout(refresh, POLL_MS);
  };
  void refresh();
}

/** Parents before children, then by name: the order lattice.json wants, and the order to read them in. */
const byDepth = (a: string, b: string) => a.split("/").length - b.split("/").length || a.localeCompare(b);

/**
 * The chains the node page lists: those hosted, and the child chains
 * the hosted ones carry (what the node can see without being told a path).
 */
export async function hostableChains(hosted: string[], reads: Pick<NodeReads, "children">): Promise<string[]> {
  const found = new Set(hosted);
  await Promise.all(["Nexus", ...hosted].map(async (chain) => {
    for (const child of await reads.children(chain).catch(() => [])) found.add(child);
  }));
  return [...found].sort(byDepth);
}

/** The chains hosted, with one more added by its path: parents first, each once. */
export function chosenChains(ticked: string[], typed: string): string[] {
  return [...new Set([...ticked, ...typed.split(/[\s,]+/).filter(Boolean)])].sort(byDepth);
}

/**
 * Whom one chain connects to, as a form. Nexus has the network's built-in
 * peers, a list of the user's own instead, or no one. A child chain finds its
 * peers through its parent, and may be given more to try.
 */
function connectForm(manager: NodeManager, prefs: NodePrefs, chain: string, saved: () => void) {
  const root = chain === "Nexus";
  const parent = chain.split("/").slice(0, -1).join("/");
  const now = root ? prefs.peers : prefs.chainPeers[chain] ?? "default";
  const mode = h("select", { class: "picker", "aria-label": `Peers of ${chain}` },
    ...(root
      ? [
          h("option", { value: "default" }, "The network's built-in peers"),
          h("option", { value: "custom" }, "Only peers I list"),
          h("option", { value: "none" }, "No one (a private node)"),
        ]
      : [
          h("option", { value: "default" }, `Peers found through ${parent}`),
          h("option", { value: "custom" }, "Those, and peers I list"),
        ]),
  ) as HTMLSelectElement;
  mode.value = Array.isArray(now) ? "custom" : now;
  const peers = h("textarea", { placeholder: "publicKey@host:port, one per line", spellcheck: "false", "aria-label": `Peer list of ${chain}` }) as HTMLTextAreaElement;
  peers.value = Array.isArray(now) ? now.join("\n") : "";
  // Shown only when it is what was chosen.
  const showPeers = () => { peers.hidden = mode.value !== "custom"; };
  mode.addEventListener("change", showPeers);
  showPeers();
  const err = h("div", { class: "toast", role: "status" });
  return h("div", { class: "stack" },
    h("label", { class: "k" }, "Connect to"), mode, peers,
    h("p", { class: "muted" }, "Saving restarts the node if it is running."),
    err,
    h("button", { class: "btn block", "aria-label": `Save peers of ${chain}`, onclick: async () => {
      const current = await manager.prefs();
      const typed = parsePeersField(mode.value, peers.value);
      const { [chain]: _, ...others } = current.chainPeers;
      const next: NodePrefs = root
        ? { ...current, peers: typed }
        : { ...current, chainPeers: Array.isArray(typed) ? { ...others, [chain]: typed } : others };
      const problem = checkPrefs(next);
      if (problem) { err.textContent = problem; return; }
      err.textContent = "saving…";
      try {
        await manager.configure(next);
        saved();
      } catch (e) { err.textContent = (e as Error).message; }
    } }, "Save peers"),
  );
}

/** The node's ports, as a form, and Save. `saved` runs once they are written. */
async function networkForm(manager: NodeManager, saved: () => void, onPrefsChanged: () => void) {
  const prefs = await manager.prefs();
  const rpc = h("input", { type: "text", inputmode: "numeric", "aria-label": "Local API port", value: String(prefs.rpc) }) as HTMLInputElement;
  const listen = h("input", { type: "text", inputmode: "numeric", "aria-label": "Peer port", value: String(prefs.listen) }) as HTMLInputElement;
  const err = h("div", { class: "toast", role: "status" });
  return h("div", { class: "advanced-content stack" },
    h("label", { class: "k" }, "Local API port (this computer only)"), rpc,
    h("label", { class: "k" }, "Peer port"), listen,
    h("p", { class: "muted" }, "Saving restarts the node if it is running."),
    err,
    h("button", { class: "block", onclick: async () => {
      // The chains and their peers are whatever they are now: they are changed above, not here.
      const next: NodePrefs = {
        ...(await manager.prefs()),
        rpc: Number(rpc.value.trim()),
        listen: Number(listen.value.trim()),
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
  );
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

/** An account as mining names it: its label, or its address when it is not this wallet's. */
function payee(address: string | undefined, accounts: { label: string; address: string }[]): string {
  if (!address) return "no one";
  return accounts.find((a) => a.address === address)?.label ?? `${address.slice(0, 12)}…${address.slice(-8)} (not in this wallet)`;
}

/** The chains as a tree, with one thing beside each: here, who it pays. */
function payTree(chains: string[], cell: (chain: string) => HTMLElement | string) {
  return h("div", { class: "tree paying" },
    h("div", { class: "cols", "aria-hidden": "true" }, h("span", {}, "Chain"), h("span", {}, "Pays")),
    h("ul", {}, ...outline(chains, (chain, guide) => h("div", { class: "chain", "data-chain": chain },
      h("span", { class: "name" }, h("span", { class: "tree-guide", "aria-hidden": "true" }, guide), h("span", {}, nameOf(chain))),
      h("span", { class: "pays" }, cell(chain)),
    ))),
  );
}

/** What is said of the chains that pay no one: their rewards are destroyed, so it is said out loud. */
const burnt = (unpaid: string[], will = false) => (unpaid.length
  ? `${unpaid.join(" and ")} ${will ? "will pay" : unpaid.length === 1 ? "pays" : "pay"} no one: every reward and fee mined there is destroyed.` : "");

/**
 * Mining on this node with this computer: the bundled `lattice mine` loop, on the
 * graphics chip or the processor, paying each hosted chain's reward and fees
 * to an account of this wallet. One search covers Nexus and the chains it
 * carries (merged mining), so the chains mined are the chains hosted.
 *
 * This page says what mining is doing: whether it runs, with Start or Stop,
 * what it has found, and who each chain pays. Nothing on it is a setting;
 * those are on their own page. `back` leaves; given `saved`, it is asked to
 * open the node page to host a chain, and to return (`saved`) once that is
 * saved.
 */
export async function miningScreen(manager: NodeManager, reads: NodeReads, back: (saved?: () => void) => void, onPrefsChanged: () => void) {
  const [chains, saved, accounts, gpu] = await Promise.all([manager.chains(), manager.miningPrefs(), reads.accounts(), manager.canMineOnGPU()]);
  const again = () => miningScreen(manager, reads, back, onPrefsChanged);
  // Never set up, it searches on the graphics chip when this computer has one
  // Lattice can use, and every chain pays the account in use: Start needs no setup.
  const prefs: MiningPrefs = saved ?? {
    searcher: gpu ? "gpu" : "cpu",
    workers: 1,
    recipients: accounts[0] ? Object.fromEntries(chains.map((chain) => [chain, accounts[0]!.address])) : {},
  };
  let timer: ReturnType<typeof setTimeout> | undefined;
  const leave = (go: () => void) => () => { clearTimeout(timer); go(); };
  const state = h("p", { class: "muted", role: "status" }, "…");
  const toast = h("div", { class: "toast", role: "status" });
  const toggle = h("button", { class: "btn" }, "…") as HTMLButtonElement;
  const screen = h("div", { class: "stack" });
  let mining = false;
  let busy = false;
  const show = async () => {
    clearTimeout(timer);
    try {
      const [status, log] = await Promise.all([manager.miningStatus(), manager.miningLog().catch(() => "")]);
      mining = status.running;
      // Proof that it is working, in the only terms that matter: blocks found.
      // None yet is the ordinary case for a while, and is said as that.
      const run = parseMiningLog(log);
      state.textContent = !mining ? "Not mining"
        : `Mining on ${SEARCHERS[prefs.searcher]}. ` + (run.found === 0
          ? "No block found yet: a search can run a long while before its first."
          : `${run.found.toLocaleString()} ${run.found === 1 ? "block" : "blocks"} found, the last at ${run.last ? new Date(run.last).toLocaleTimeString() : "?"}.`);
    } catch (e) { state.textContent = (e as Error).message; }
    if (!busy) {
      toggle.textContent = mining ? "Stop mining" : "Start mining";
      // Starting is the step forward; stopping is not, so it is not the filled button.
      toggle.className = mining ? "btn" : "btn btn--primary";
    }
    if (document.body.contains(screen)) timer = setTimeout(show, POLL_MS);
  };
  toggle.addEventListener("click", async () => {
    if (busy) return;
    busy = true;
    toggle.disabled = true;
    toast.textContent = mining ? "stopping (it finishes the round it is on)…" : "starting…";
    try {
      if (mining) await manager.stopMining();
      else {
        // A first start saves what the page shows; after that, starting is only starting.
        if (!saved) {
          const problem = checkMining(prefs, chains);
          if (problem) throw new Error(problem);
          await manager.configureMining(prefs);
        }
        await manager.startMining();
      }
      toast.textContent = "";
    } catch (e) { toast.textContent = (e as Error).message; }
    busy = false;
    toggle.disabled = false;
    await show();
  });
  const unpaid = chains.filter((chain) => !prefs.recipients[chain]);
  screen.append(
    h("div", { class: "node-head" },
      h("div", {}, h("h1", {}, "Mining"), state),
      toggle,
    ),
    payTree(chains, (chain) => payee(prefs.recipients[chain], accounts)),
    ...(unpaid.length && accounts.length ? [h("p", { class: "warn", role: "status" }, burnt(unpaid))] : []),
    ...(accounts.length ? [] : [h("p", { class: "warn" }, "The wallet is locked or has no account, so there is no one of yours to pay. Unlock it first.")]),
    toast,
    // What is set is one row away, and the row says how it searches now.
    h("div", { class: "nav-list" }, navRow("Mining settings", leave(() => miningSettings(manager, reads, back, { chains, prefs, accounts, gpu }, again)), SEARCHERS[prefs.searcher])),
    h("button", { class: "btn block", onclick: leave(() => back()) }, "Back"),
  );
  render(screen);
  await show();
}

/**
 * How mining searches and who each chain pays: set once, so on a page of
 * their own, as one form with one Save. Saving while mining restarts the
 * loop, and the button says so.
 */
async function miningSettings(
  manager: NodeManager, reads: NodeReads, back: (saved?: () => void) => void,
  now: { chains: string[]; prefs: MiningPrefs; accounts: { label: string; address: string }[]; gpu: boolean },
  done: () => void,
) {
  const { chains, prefs, accounts, gpu } = now;
  const running = (await manager.miningStatus().catch(() => ({ running: false }))).running;
  const searcher = h("select", { class: "picker", "aria-label": "Search with" },
    ...(gpu ? [h("option", { value: "gpu" }, "This computer's graphics chip")] : []),
    h("option", { value: "cpu" }, "This computer's processor"),
    ...(prefs.searcher === "custom" ? [h("option", { value: "custom" }, "The worker set in lattice.json")] : []),
  ) as HTMLSelectElement;
  // Saved for a graphics chip this computer cannot use (now): the processor, until it is saved otherwise.
  searcher.value = prefs.searcher !== "gpu" || gpu ? prefs.searcher : "cpu";
  const cores = Math.max(1, Math.min(MAX_WORKERS, globalThis.navigator?.hardwareConcurrency ?? 1));
  const workers = h("input", { type: "text", inputmode: "numeric", "aria-label": "Workers", value: String(prefs.workers) }) as HTMLInputElement;
  const workersField = h("div", { class: "stack" }, h("label", { class: "k" }, `Processor workers (this computer has ${cores} cores)`), workers);
  const pickers = new Map<string, HTMLSelectElement>();
  for (const chain of chains) {
    // What is saved is what is shown, an address that is not this wallet's included.
    const current = prefs.recipients[chain] ?? "";
    const picker = h("select", { class: "picker", "aria-label": `Recipient on ${chain}` },
      ...accounts.map((a) => h("option", { value: a.address }, `${a.label} · ${a.address.slice(0, 12)}…${a.address.slice(-8)}`)),
      ...(current && !accounts.some((a) => a.address === current) ? [h("option", { value: current }, payee(current, accounts))] : []),
      h("option", { value: "" }, "No one"),
    ) as HTMLSelectElement;
    picker.value = current;
    pickers.set(chain, picker);
  }
  // Paying no one is allowed, and costly: it is said out loud before it happens.
  const burn = h("p", { class: "warn", role: "status" });
  const toast = h("div", { class: "toast", role: "status" });
  const reflect = () => {
    const unpaid = [...pickers].filter(([, picker]) => picker.value === "").map(([chain]) => chain);
    burn.hidden = unpaid.length === 0;
    burn.textContent = burnt(unpaid, true);
    workersField.hidden = searcher.value !== "cpu";
  };
  searcher.addEventListener("change", reflect);
  for (const picker of pickers.values()) picker.addEventListener("change", reflect);
  reflect();
  const save = h("button", { class: "block" }, running ? "Save and restart mining" : "Save") as HTMLButtonElement;
  save.addEventListener("click", async () => {
    const chosen = miningChoices(searcher.value as Searcher, workers.value, Object.fromEntries([...pickers].map(([chain, picker]) => [chain, picker.value])));
    const problem = checkMining(chosen, chains);
    if (problem) { toast.textContent = problem; return; }
    save.disabled = true;
    toast.textContent = running ? "saving (mining restarts)…" : "saving…";
    try { await manager.configureMining(chosen); done(); } catch (e) { toast.textContent = (e as Error).message; save.disabled = false; }
  });
  render(
    h("div", { class: "stack" },
      h("h1", {}, "Mining settings"),
      h("p", { class: "muted" }, "One search covers every chain your node hosts. Each block found pays that chain's reward and fees to the account chosen here."),
      h("label", { class: "k" }, "Search with"), searcher,
      workersField,
      h("h2", { class: "nav-label" }, "Who each chain pays"),
      payTree(chains, (chain) => pickers.get(chain)!),
      burn,
      ...(accounts.length ? [] : [h("p", { class: "warn" }, "The wallet is locked or has no account, so there is no one of yours to pay. Unlock it first.")]),
      h("p", { class: "muted" }, "Chains mined are the chains your node hosts."),
      // Hosting a chain is the node's business: its page is opened, and comes back to Mining once a chain is added.
      h("button", { class: "btn block", onclick: () => back(done) }, "Manage chains on your node"),
      toast,
      save,
      h("button", { class: "btn block", onclick: done }, "Cancel"),
    ),
  );
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
