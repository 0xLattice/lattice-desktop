// The desktop's node screen: start/stop/status of the app's own node, its
// sync heights, and its settings (ports, Nexus bootstrap peers, hosted child
// chains). Rendered with the wallet's own helpers and styles.

import { h, render } from "../vendor/nexus-wallet/src/popup/app.ts";
import { checkPrefs, type ChainStatus, type NodeManager, type NodePrefs } from "./node-manager.ts";

const POLL_MS = 3000;

export function statusLine(s: ChainStatus): string {
  if (s.state === "down") return "stopped";
  if (s.state === "unreachable") return "starting (RPC not answering yet)";
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

export function nodeScreen(manager: NodeManager, back: () => void, onPrefsChanged: () => void) {
  const table = h("div", { class: "kv" }, h("div", { class: "row" }, h("span", { class: "muted" }, "…")));
  const toast = h("div", { class: "toast" });
  let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refresh = async () => {
    clearTimeout(timer);
    try {
      const chains = await manager.status();
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
      h("button", { class: "btn block", onclick: () => settingsScreen(manager, () => nodeScreen(manager, back, onPrefsChanged), onPrefsChanged) }, "Node settings"),
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
