// The Swap screen: lattice-swap-web's page (its `App`), its driver aliased to
// src/swap-driver over the shell's swap engine, with the one swap setting the
// page may change, the Bitcoin network, and a restore from a recovery file
// the shell reads (the page's own file input is hidden: its file's key would
// pass through the page), above it. The Esplora and the relay
// are the operator's (the shell's swap/settings.json), so the page's own
// Esplora field is hidden (desktop.css), and NEX is read on this app's node.

import { h as preact, render as mount } from "preact";
import { App } from "../vendor/lattice-swap-web/src/app.tsx";
import type { AppConfig } from "../vendor/lattice-swap-web/src/config.ts";
import "../vendor/lattice-swap-web/src/style.css";
import { h } from "../vendor/nexus-wallet/src/popup/app.ts";
import { connecting, SAVED_BY_SHELL } from "./swap-driver/index.ts";
import { swapRestoreFile, swapSetNetwork, swapSettings, type SwapNetwork } from "./swap-host.ts";

/** The relay the shell lists offers from when its settings name none
 * (swap.rs's `DEFAULT_RELAY`): shown only. */
const DEFAULT_RELAY = "https://lattice-swap-relay.fly.dev";

const NETWORKS: [SwapNetwork, string][] = [["bitcoin", "Bitcoin"], ["signet", "Signet (test coins)"]];

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
      note.textContent = e instanceof Error ? e.message : String(e);
      return;
    }
    settings.network = network;
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
      note.textContent = e instanceof Error ? e.message : String(e);
    }
    restore.disabled = false;
  });

  const screen = h("div", { class: "stack swap-screen" },
    h("div", { class: "node-head" }, h("h1", {}, "Swap"), h("div", { class: "row-actions" }, picker, restore)),
    note,
    page,
    h("button", { class: "btn block", onclick: () => { leave(); back(); } }, "Back"),
  );
  show(settings.network);
  fill();
  return screen;
}
