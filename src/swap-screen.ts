// The Swap screen: lattice-swap-web's page (its `App`), its driver aliased to
// src/swap-driver over the shell's swap engine, with the Bitcoin network and
// a restore from a recovery file the shell reads (the page's own file input
// is hidden: its file's key would pass through the page) above it, and the
// servers folded away below them: Bitcoin through mempool.space's Esplora or
// your own, and NEX through a public Nexus node, this app's, or yours by URL,
// as Sparrow chooses a public server or your own. The page's own Esplora and
// Nexus fields are hidden (desktop.css); the relay is the operator's (the
// shell's swap/settings.json).

import { h as preact, render as mount } from "preact";
import { App } from "../vendor/lattice-swap-web/src/app.tsx";
import type { AppConfig } from "../vendor/lattice-swap-web/src/config.ts";
import "../vendor/lattice-swap-web/src/style.css";
import { h } from "../vendor/nexus-wallet/src/popup/app.ts";
import { connecting, plainly, SAVED_BY_SHELL } from "./swap-driver/index.ts";
import { swapCheckEsplora, swapCheckNexus, swapRestoreFile, swapSetNetwork, swapSetServers, swapSettings, type NexusChoice, type SwapNetwork, type SwapSettings } from "./swap-host.ts";

/** The relay the shell lists offers from when its settings name none
 * (swap.rs's `DEFAULT_RELAY`): shown only. */
const DEFAULT_RELAY = "https://lattice-swap-relay.fly.dev";
/** The public Nexus node the shell reads unless you choose this app's
 * (swap.rs's `PUBLIC_NEXUS`): one for every network. */
const PUBLIC_NEXUS = "lattice-mainnet-read.fly.dev";

const NETWORKS: [SwapNetwork, string][] = [["bitcoin", "Bitcoin"], ["signet", "Signet (test coins)"]];

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** A save the shell refused, in plain words: one refused for a running swap
 * says so without its id. */
async function refusal(e: unknown): Promise<string> {
  const plain = message(await plainly(e instanceof Error ? e : new Error(String(e))));
  return /swap \S+ is (in progress|running)/.test(plain) ? "A swap is running: change this once it ends." : plain;
}

/** Your own Esplora on the network swaps run on, if any. */
const ownEsplora = (settings: SwapSettings) => settings.esplora?.[settings.network] ?? null;

/** Whether swaps on the network they run on read Nexus on this app's node. */
const ownNexus = (settings: SwapSettings) => settings.ownNexus?.includes(settings.network) ?? false;

/** Your own Nexus node by URL on the network swaps run on, if any. */
const remoteNexus = (settings: SwapSettings) => settings.nexus?.[settings.network] ?? null;

/** `url`'s host, or `url` if it has none. */
function host(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/** Whether `url` is plain http off this machine (a local network or an
 * onion, as the shell allows): what it reads can be seen on the way. */
const plainHttp = (url: string) =>
  /^\s*http:\/\//i.test(url) && !/^\s*http:\/\/(localhost|127\.\d+\.\d+\.\d+|\[::1\])([:/?#]|$)/i.test(url);

/**
 * Where each chain is read, as a form with one Save: Bitcoin through
 * mempool.space's Esplora (the default) or your own, and Nexus through a
 * public node (the default), this app's, or yours by its https URL, each
 * with a line saying whether the one saved answers. A running swap
 * continues on the servers saved. `save` saves your own Esplora (null: the
 * public one) and where Nexus is read, the refusal if any, and the screen
 * then shows the settings on disk (`show`), unless it refused the save and
 * they are unchanged.
 */
function serversForm(save: (esplora: string | null, nexus: NexusChoice) => Promise<string | null>) {
  const bitcoin = h("select", { class: "picker", "aria-label": "Bitcoin server" },
    h("option", { value: "public" }, "Public (mempool.space)"),
    h("option", { value: "own" }, "Your Esplora")) as HTMLSelectElement;
  const url = h("input", { type: "text", spellcheck: "false", placeholder: "https://…/api", "aria-label": "Your Esplora URL" }) as HTMLInputElement;
  const plain = h("p", { class: "muted" }, "Plain http: anyone on the way to it can see what you ask it.");
  const state = h("p", { class: "muted", role: "status" });
  const trust = h("p", { class: "muted" }, "You trust mempool.space to show Bitcoin in time to refund: keep amounts small.");
  const nexus = h("select", { class: "picker", "aria-label": "Nexus server" },
    h("option", { value: "public" }, `Public (${PUBLIC_NEXUS})`),
    h("option", { value: "own" }, "Your node"),
    h("option", { value: "remote" }, "Your node at a URL")) as HTMLSelectElement;
  const nexusUrl = h("input", { type: "text", spellcheck: "false", placeholder: "https://…", "aria-label": "Your Nexus node URL" }) as HTMLInputElement;
  const nexusHint = h("p", { class: "muted" }, "Its public read port, started with --public-submit: an https URL.");
  const nexusState = h("p", { class: "muted", role: "status" });
  const nexusTrust = h("p", { class: "muted" }, `You trust ${PUBLIC_NEXUS} for how deep the seller's NEX lock is: keep amounts small.`);
  const toast = h("div", { class: "toast", role: "status" });
  const shown = () => {
    url.hidden = bitcoin.value !== "own";
    plain.hidden = url.hidden || !plainHttp(url.value);
    nexusUrl.hidden = nexusHint.hidden = nexus.value !== "remote";
  };
  const edited = () => {
    toast.textContent = "";
    shown();
  };
  bitcoin.addEventListener("change", edited);
  url.addEventListener("input", edited);
  nexus.addEventListener("change", edited);
  nexusUrl.addEventListener("input", edited);
  /** A line saying whether `server` answers as `ask` asks it; only the
   * latest check's answer is shown. */
  const checker = (line: HTMLElement, ask: () => Promise<void>) => {
    let checks = 0;
    return async (server: string) => {
      const n = ++checks;
      line.textContent = "…";
      let said: string;
      try {
        await ask();
        said = `Connected to ${server}`;
      } catch (e) {
        said = message(e);
      }
      if (n === checks) line.textContent = said;
    };
  };
  const check = checker(state, swapCheckEsplora);
  const checkNexus = checker(nexusState, swapCheckNexus);
  const button = h("button", { class: "btn block", "aria-label": "Save servers" }, "Save") as HTMLButtonElement;
  button.addEventListener("click", async () => {
    toast.textContent = "saving…";
    const choice: NexusChoice = nexus.value === "remote" ? { remote: nexusUrl.value } : nexus.value === "own" ? "own" : "public";
    toast.textContent = (await save(bitcoin.value === "own" ? url.value : null, choice)) ?? "";
  });
  const element = h("details", { class: "advanced" }, h("summary", {}, "Servers"),
    h("div", { class: "advanced-content stack" },
      h("label", { class: "k" }, "Bitcoin"), bitcoin, url, plain, state, trust,
      h("label", { class: "k" }, "Nexus"), nexus, nexusUrl, nexusHint, nexusState, nexusTrust,
      h("p", { class: "muted" }, "A running swap continues on the servers you save."),
      toast,
      button,
    ));
  return {
    element,
    /** The settings saved, shown: the choice, the trust note and the check. */
    show(settings: SwapSettings) {
      const own = ownEsplora(settings);
      bitcoin.value = own ? "own" : "public";
      url.value = own ?? "";
      trust.hidden = own !== null;
      const node = ownNexus(settings);
      const remote = remoteNexus(settings);
      nexus.value = node ? "own" : remote ? "remote" : "public";
      nexusUrl.value = remote ?? "";
      nexusTrust.hidden = nexus.value !== "public";
      shown();
      void check(own ?? "mempool.space");
      void checkNexus(node ? "your node" : remote ? host(remote) : PUBLIC_NEXUS);
    },
    /** Whether a save is under way, Save held meanwhile. */
    hold(on: boolean) {
      button.disabled = on;
    },
  };
}

/**
 * The Swap screen, its page mounted until `back` or until the screen leaves
 * the document. `nexAddress` fills the page's "Send NEX to" while it is empty:
 * the active account's address.
 */
export async function swapScreen(nexAddress: string | null, back: () => void): Promise<HTMLElement> {
  let settings = await swapSettings();
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
      esplora: ownEsplora(settings) ?? "",
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

  /** `act`, a save, one at a time: the network picker and the servers'
   * Save held until it ends, and then the settings on disk shown, whatever
   * it did (a save can be written and the wallet then fail to open), except
   * that a refused save that left them as they were keeps the servers as
   * typed. The refusal, if any. */
  const saving = async (act: () => Promise<void>): Promise<string | null> => {
    picker.disabled = true;
    servers.hold(true);
    let refused: string | null = null;
    try {
      await act();
    } catch (e) {
      refused = await refusal(e);
    }
    const before = settings;
    try {
      settings = await swapSettings();
    } catch (e) {
      refused ??= message(e);
    }
    picker.value = settings.network;
    const changed = settings.network !== before.network || ownEsplora(settings) !== ownEsplora(before) ||
      ownNexus(settings) !== ownNexus(before) || remoteNexus(settings) !== remoteNexus(before);
    if (changed) show(settings.network);
    if (changed || refused === null) servers.show(settings);
    picker.disabled = false;
    servers.hold(false);
    return refused;
  };

  const servers = serversForm((esplora, nexus) => saving(() => swapSetServers(settings.network, esplora, nexus)));
  servers.show(settings);

  picker.addEventListener("change", async () => {
    const network = picker.value as SwapNetwork;
    note.textContent = "";
    note.textContent = (await saving(() => swapSetNetwork(network))) ?? "";
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

  const screen = h("div", { class: "stack swap-screen" },
    h("div", { class: "node-head" }, h("h1", {}, "Swap"), h("div", { class: "row-actions" }, picker, restore)),
    note,
    servers.element,
    page,
    h("button", { class: "btn block", onclick: () => { leave(); back(); } }, "Back"),
  );
  show(settings.network);
  fill();
  return screen;
}
