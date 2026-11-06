// The Swap screen: lattice-swap-web's page (its `App`), its driver aliased to
// src/swap-driver over the shell's swap engine, with the Bitcoin network and
// a restore from a recovery file the shell reads (the page's own file input
// is hidden: its file's key would pass through the page) above it, and the
// servers folded away below them: Bitcoin through mempool.space's Esplora or
// your own, as Sparrow chooses a public server or your own. The page's own
// Esplora field is hidden (desktop.css); NEX is read on this app's node, and
// the relay is the operator's (the shell's swap/settings.json).

import { h as preact, render as mount } from "preact";
import { App } from "../vendor/lattice-swap-web/src/app.tsx";
import type { AppConfig } from "../vendor/lattice-swap-web/src/config.ts";
import "../vendor/lattice-swap-web/src/style.css";
import { h } from "../vendor/nexus-wallet/src/popup/app.ts";
import { connecting, SAVED_BY_SHELL } from "./swap-driver/index.ts";
import { swapCheckEsplora, swapRestoreFile, swapSetEsplora, swapSetNetwork, swapSettings, type SwapNetwork, type SwapSettings } from "./swap-host.ts";

/** The relay the shell lists offers from when its settings name none
 * (swap.rs's `DEFAULT_RELAY`): shown only. */
const DEFAULT_RELAY = "https://lattice-swap-relay.fly.dev";

const NETWORKS: [SwapNetwork, string][] = [["bitcoin", "Bitcoin"], ["signet", "Signet (test coins)"]];

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * Where each chain is read, as a form with one Save: Bitcoin through
 * mempool.space's Esplora (the default) or your own, with a line saying
 * whether it answers on the swaps' network; Nexus through this app's node.
 * A change is for the next swap: the shell refuses it while one runs.
 */
function serversForm(settings: SwapSettings) {
  const bitcoin = h("select", { class: "picker", "aria-label": "Bitcoin server" },
    h("option", { value: "public" }, "Public (mempool.space)"),
    h("option", { value: "own" }, "Your Esplora")) as HTMLSelectElement;
  const url = h("input", { type: "text", spellcheck: "false", placeholder: "https://…/api", "aria-label": "Your Esplora URL" }) as HTMLInputElement;
  const state = h("p", { class: "muted", role: "status" });
  const trust = h("p", { class: "muted" }, "You trust mempool.space to show Bitcoin in time to refund: keep amounts small.");
  const toast = h("div", { class: "toast", role: "status" });
  const shown = () => {
    url.hidden = bitcoin.value !== "own";
    trust.hidden = bitcoin.value !== "public";
  };
  const reset = () => {
    bitcoin.value = settings.esplora ? "own" : "public";
    url.value = settings.esplora ?? "";
    shown();
  };
  bitcoin.addEventListener("change", shown);
  reset();
  const check = async () => {
    const server = settings.esplora ?? "mempool.space";
    state.textContent = "…";
    try {
      await swapCheckEsplora();
      state.textContent = `Connected to ${server}`;
    } catch (e) {
      state.textContent = message(e);
    }
  };
  const save = h("button", { class: "btn block", "aria-label": "Save servers" }, "Save") as HTMLButtonElement;
  save.addEventListener("click", async () => {
    const esplora = bitcoin.value === "own" ? url.value : null;
    toast.textContent = "saving…";
    save.disabled = true;
    try {
      await swapSetEsplora(esplora);
      settings.esplora = (await swapSettings()).esplora;
      toast.textContent = "";
      reset();
      void check();
    } catch (e) {
      toast.textContent = message(e);
    }
    save.disabled = false;
  });
  void check();
  return h("details", { class: "advanced" }, h("summary", {}, "Servers"),
    h("div", { class: "advanced-content stack" },
      h("label", { class: "k" }, "Bitcoin"), bitcoin, url, state, trust,
      h("label", { class: "k" }, "Nexus"), h("p", {}, "Your node"),
      h("p", { class: "muted" }, "Changes are for the next swap: a running swap keeps its servers."),
      toast,
      save,
    ));
}

/**
 * The Swap screen, its page mounted until `back` or until the screen leaves
 * the document. `nexAddress` fills the page's "Send NEX to" while it is empty:
 * the active account's address.
 */
export async function swapScreen(nexAddress: string | null, back: () => void): Promise<HTMLElement> {
  const settings = await swapSettings();
  const note = h("p", { class: "muted", role: "status" });
  const page = h("div");
  const picker = h("select", { class: "picker", "aria-label": "Bitcoin network" },
    ...NETWORKS.map(([value, label]) => h("option", { value }, label))) as HTMLSelectElement;
  picker.value = settings.network;

  /** Files the page hands the buyer: only the recovery file, which the
   * shell already saved. Its reminder (.ics) is not offered (desktop.css):
   * the app stays in the tray and notifies instead. */
  const save = (name: string, text: string) => {
    note.textContent = text === SAVED_BY_SHELL ? `Recovery file saved to ${name}` : `Not saved: ${name}`;
  };
  // A started swap's first status read, tried again.
  const connected = (event: Event) => {
    note.textContent = (event as CustomEvent<string | null>).detail ?? "";
  };
  connecting.addEventListener("change", connected);

  const show = (network: SwapNetwork) => {
    const config: AppConfig = {
      network,
      esplora: settings.esplora ?? "",
      nexus: "",
      relay: settings.relay ?? DEFAULT_RELAY,
      ownNode: true,
      save,
    };
    mount(null, page);
    mount(preact(App, { config }), page);
  };

  // The page's "Send NEX to", whenever it is shown empty.
  const filled = new WeakSet<Element>();
  const fill = () => {
    const to = page.querySelector<HTMLInputElement>("input#to");
    if (!nexAddress || !to || filled.has(to) || to.value !== "") return;
    filled.add(to);
    to.value = nexAddress;
    to.dispatchEvent(new Event("input", { bubbles: true }));
  };
  new MutationObserver(fill).observe(page, { childList: true, subtree: true });

  let shown = false;
  const leave = () => {
    gone.disconnect();
    connecting.removeEventListener("change", connected);
    mount(null, page);
  };
  // The wallet's own navigation replaces the screen without calling `back`.
  const gone = new MutationObserver(() => {
    if (screen.isConnected) shown = true;
    else if (shown) leave();
  });
  gone.observe(document.body, { childList: true, subtree: true });

  picker.addEventListener("change", async () => {
    const network = picker.value as SwapNetwork;
    note.textContent = "";
    try {
      await swapSetNetwork(network);
      show(network);
    } catch (e) {
      picker.value = settings.network;
      note.textContent = message(e);
      return;
    }
    settings.network = network;
    servers.replaceWith(servers = serversForm(settings));
  });

  const restore = h("button", { class: "btn" }, "Restore from file") as HTMLButtonElement;
  restore.addEventListener("click", async () => {
    note.textContent = "";
    restore.disabled = true;
    try {
      const address = await swapRestoreFile();
      if (address !== null) {
        show(settings.network);
        note.textContent = `Restored the wallet ${address}`;
      }
    } catch (e) {
      note.textContent = message(e);
    }
    restore.disabled = false;
  });

  let servers = serversForm(settings);
  const screen = h("div", { class: "stack swap-screen" },
    h("div", { class: "node-head" }, h("h1", {}, "Swap"), h("div", { class: "row-actions" }, picker, restore)),
    note,
    servers,
    page,
    h("button", { class: "btn block", onclick: () => { leave(); back(); } }, "Back"),
  );
  show(settings.network);
  fill();
  return screen;
}
