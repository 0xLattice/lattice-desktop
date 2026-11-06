// The Swap screen as a person uses it: lattice-swap-web's page, bundled as
// build.mjs bundles it (its driver aliased to src/swap-driver), in jsdom over
// a scripted swap engine standing in for the shell's commands and events.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";
import { JSDOM } from "jsdom";
import type { SwapOffer, SwapStatus } from "../src/swap-host.ts";

const root = fileURLToPath(new URL("..", import.meta.url));

/** The screen, the shell's IPC mocks and the shim, as one script for jsdom. */
let built: Promise<string> | undefined;
const bundle = () => (built ??= build());
async function build(): Promise<string> {
  const built = await esbuild.build({
    stdin: {
      contents: `
        import { mockIPC } from "@tauri-apps/api/mocks";
        import { emit } from "@tauri-apps/api/event";
        import { Buyer, isCid, Wallet } from "lattice-swap-driver";
        import { swapScreen } from "./src/swap-screen.ts";
        window.harness = { mockIPC, emit, isCid, swapScreen, Buyer, Wallet };
      `,
      resolveDir: root,
      loader: "ts",
    },
    bundle: true,
    write: false,
    format: "iife",
    target: "es2022",
    platform: "browser",
    jsx: "automatic",
    jsxImportSource: "preact",
    alias: { "lattice-swap-driver": "./src/swap-driver/index.ts" },
    loader: { ".css": "empty" },
    logLevel: "silent",
  });
  return built.outputFiles[0]!.text;
}

/** Lattice's spelling of a CID (CIDv1, dag-cbor, sha2-256) of `fill`'s digest. */
function cid(fill: number): string {
  const bytes = [0x01, 0x71, 0x12, 0x20, ...new Array(32).fill(fill)];
  let bits = "";
  for (const byte of bytes) bits += byte.toString(2).padStart(8, "0");
  bits = bits.padEnd(Math.ceil(bits.length / 5) * 5, "0");
  let out = "b";
  for (let i = 0; i < bits.length; i += 5) out += "abcdefghijklmnopqrstuvwxyz234567"[parseInt(bits.slice(i, i + 5), 2)];
  return out;
}

const ACCOUNT = cid(7);
const OFFER: SwapOffer = {
  quote: { price: 1_000, min_btc_amount: 10_000, max_btc_amount: 1_000_000 },
  template: {
    swap_id: "offer-1", btc_fee: 500, nex_fee: 1, t1: 72, t2: 144,
    btc_confirmations: 1, nex_confirmations: 1, margin: 0,
    nexus_genesis: "bafyreiggtg4ezifboyekbf4gxst2jr3mpjqcxsbmopy7w6fp46g4ngpgxa",
  },
  btc_network: "signet",
  relay: null,
  seller: "seller",
};
const OFFER_TEXT = JSON.stringify({ signed: "offer-1" });

function status(swapId: string, patch: Partial<SwapStatus> = {}): SwapStatus {
  return {
    swapId, step: "fund-wallet", outcome: null,
    wallet: { address: "tb1qwallet", confirmed: 0, unconfirmed: 0 },
    need: 60_500,
    confirmations: { funding: null, lockSpend: null, cancelSpend: null, nexLock: null },
    deadlines: { cancel: null, refund: null, punish: null },
    mustStayOpen: false, backupCovers: "wallet", running: false,
    btc_amount: 50_000, price: 1_000, nex_amount: 50_000_000,
    btc_confirmations: 1, nex_confirmations: 1,
    nexAddress: ACCOUNT, tip: 100, warning: null,
    ...patch,
  };
}

async function until(what: string, ok: () => boolean, ms = 5_000) {
  const start = Date.now();
  while (!ok()) {
    if (Date.now() - start > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

interface Harness {
  mockIPC: (cb: (cmd: string, args: Record<string, unknown>) => unknown, options: { shouldMockEvents: boolean }) => void;
  emit: (event: string, payload: unknown) => Promise<void>;
  isCid: (text: string) => boolean;
  swapScreen: (nexAddress: string | null, back: () => void) => Promise<HTMLElement>;
  Wallet: { open(): Promise<unknown> };
  Buyer: { resume(wallet: unknown, swapId: string): Promise<{ run(signal?: AbortSignal): Promise<string | null>; status(): SwapStatus }> };
}

type Engine = (cmd: string, args: Record<string, unknown>) => unknown;

/** jsdom with the bundle loaded, over a scripted engine: one wallet and the
 * swap it follows (`state.followed`), `engine` answering first where it
 * answers anything but undefined. */
async function page(t: { after(fn: () => void): void }, engine: Engine = () => undefined) {
  const dom = new JSDOM(`<!doctype html><body><main id="view"></main></body>`, { runScripts: "outside-only", pretendToBeVisual: true });
  const { window } = dom;
  t.after(() => window.close());
  window.eval(await bundle());
  const harness = (window as unknown as { harness: Harness }).harness;
  const calls: [string, Record<string, unknown>][] = [];
  const state = { followed: null as SwapStatus | null };
  harness.mockIPC((cmd, args) => {
    calls.push([cmd, args]);
    const answer = engine(cmd, args);
    if (answer !== undefined) return answer;
    switch (cmd) {
      case "swap_settings": return { network: "signet", esplora: null, relay: null };
      case "swap_wallet_exists": return true;
      case "swap_wallet_open": return "tb1qwallet";
      case "swap_balance": return { confirmed: 0, unconfirmed: 0 };
      case "swap_need": return { enough: false, need: 60_500 };
      case "swap_offers": return [{ text: OFFER_TEXT, offer: OFFER }];
      case "swap_status": return state.followed;
      case "swap_start":
        state.followed = status("swap-a");
        return "swap-a";
      case "swap_cancel":
        state.followed = status("swap-a", { step: "cancelled" });
        return null;
      case "swap_backup": return "/Users/someone/Downloads/lattice-swap-tb1qwallet.json";
      case "swap_check_esplora": return null;
      default: throw new Error(`unexpected ${cmd}`);
    }
  }, { shouldMockEvents: true });

  const doc = window.document;
  const text = () => doc.body.textContent ?? "";
  const button = (label: string, within = "button") => {
    const found = [...doc.querySelectorAll(within)].find((b) => b.textContent?.trim() === label);
    assert.ok(found, `a ${label} button`);
    return found as HTMLButtonElement;
  };
  const type = (selector: string, value: string) => {
    const input = doc.querySelector(selector) as HTMLInputElement;
    input.value = value;
    input.dispatchEvent(new window.Event("input", { bubbles: true }));
  };
  const mount = async () => doc.getElementById("view")!.append(await harness.swapScreen(ACCOUNT, () => {}));
  /** From the form to Start swap, for 0.0005 BTC. */
  const start = async () => {
    await until("the offer", () => text().includes("best of 1"));
    type("input#pay", "0.0005");
    // The form's Swap, not the header's tab of that name.
    await until("the amount", () => !button("Swap", "button.primary").disabled);
    button("Swap", "button.primary").click();
    await until("the review", () => text().includes("Review swap"));
    button("Start swap").click();
  };
  const settle = () => new Promise((resolve) => setTimeout(resolve, 100));
  return { window, doc, harness, calls, state, text, button, type, mount, start, settle };
}

test("the Swap screen runs a swap on the shell's engine: prepare, fund, status, cancel", async (t) => {
  const { doc, harness, calls, text, button, mount, start, settle } = await page(t);
  await mount();

  // The board is the shell's, and the NEX goes to the account in use.
  await until("the offer", () => text().includes("best of 1"));
  assert.equal((doc.querySelector("input#to") as HTMLInputElement).value, ACCOUNT);
  await start();

  // Two numbers and the address: every other bound is the shell's.
  await until("the funding screen", () => text().includes("Fund your wallet"));
  const started = calls.find(([cmd]) => cmd === "swap_start")!;
  assert.deepEqual(JSON.parse(JSON.stringify(started[1])), { offer: OFFER_TEXT, btcAmount: 50_000, price: 1_000, nexAddress: ACCOUNT });

  // A punished swap the shell resumed reports under its own id: not this one.
  await harness.emit("swap-status", status("swap-p", { step: "swapping", running: true, wallet: { address: "tb1qwallet", confirmed: 999_999, unconfirmed: 0 } }));
  await harness.emit("swap-error", { swapId: "swap-p", error: "the seller is away" });
  await settle();
  assert.ok(text().includes("Fund your wallet"));
  assert.ok(!text().includes("0.00999999"));
  assert.ok(!text().includes("the seller is away"));
  // This swap's status, as the shell polls it.
  await harness.emit("swap-status", status("swap-a", { wallet: { address: "tb1qwallet", confirmed: 25_000, unconfirmed: 5_000 } }));
  await until("this swap's balance", () => text().includes("0.00025000 BTC"));
  await harness.emit("swap-error", { swapId: "swap-a", error: "esplora unreachable" });
  await until("this swap's failed round", () => text().includes("esplora unreachable"));

  // Cancelled while it waits for funds: back to a new swap.
  button("Cancel swap").click();
  await until("the swap form", () => !!doc.querySelector("input#pay") && !text().includes("Fund your wallet"));
  assert.ok(calls.some(([cmd]) => cmd === "swap_cancel"));
  // Its run is over: a late report of it changes nothing.
  await harness.emit("swap-status", status("swap-a", { step: "swapping" }));
  await settle();
  assert.ok(doc.querySelector("input#pay"));

  // The recovery file is the shell's to write: the page only says where.
  button("Wallet · 0.00000000 BTC").click();
  await until("the wallet", () => text().includes("Download recovery file"));
  button("Download recovery file").click();
  await until("the saved note", () => text().includes("Recovery file saved to /Users/someone/Downloads/lattice-swap-tb1qwallet.json"));
});

test("a started swap is followed even when its first status reads fail", async (t) => {
  let failures = 0;
  const { text, mount, start } = await page(t, (cmd) => {
    if (cmd === "swap_start") failures = 2;
    if (cmd === "swap_status" && failures > 0) {
      failures -= 1;
      throw "https://mempool.space/signet/api: connection reset";
    }
    return undefined;
  });
  await mount();
  await start();
  await until("connecting", () => text().includes("Connecting to your swap…"));
  await until("the funding screen", () => text().includes("Fund your wallet"), 10_000);
  assert.ok(!text().includes("Connecting to your swap…"));
  assert.ok(!text().includes("connection reset"));
});

test("a start refused for a swap the page does not show says so plainly", async (t) => {
  const { text, mount, start } = await page(t, (cmd) => {
    if (cmd === "swap_start") throw "refused: swap swap-p is in progress; one swap at a time";
    return undefined;
  });
  await mount();
  await start();
  await until("the refusal", () => text().includes("A past swap is asking its seller for your NEX; try again shortly."));
  assert.ok(!text().includes("swap-p"));
});

test("a stalled swap stays stalled until a status shows progress", async (t) => {
  const swapping = (confirmations: Partial<SwapStatus["confirmations"]> = {}) => status("swap-a", {
    step: "swapping", running: true, backupCovers: "lock",
    confirmations: { funding: 1, lockSpend: null, cancelSpend: null, nexLock: null, ...confirmations },
  });
  const { harness, state, text, mount, settle } = await page(t);
  state.followed = swapping();
  await mount();
  await until("the swap", () => text().includes("BTC →"));
  assert.ok(!text().includes("The swap is stalled"));
  await harness.emit("swap-error", { swapId: "swap-a", error: "the seller is quiet" });
  await until("stalled", () => text().includes("The swap is stalled"));
  // The shell's next poll, between failed rounds: no progress, no flicker.
  await harness.emit("swap-status", swapping());
  await settle();
  assert.ok(text().includes("The swap is stalled"));
  assert.ok(text().includes("the seller is quiet"));
  // Progress: the NEX lock is seen.
  await harness.emit("swap-status", swapping({ nexLock: 0 }));
  await until("not stalled", () => !text().includes("The swap is stalled"));
});

for (const [step, outcome] of [["done", "swapped"], ["refunded", "refunded"]] as const) {
  test(`a run ends once its swap is ${step}`, async (t) => {
    const { harness, state, settle } = await page(t);
    state.followed = status("swap-a", { step: "swapping", running: true });
    const buyer = await harness.Buyer.resume(await harness.Wallet.open(), "swap-a");
    let ended: string | null | undefined;
    const run = buyer.run().then((answer) => (ended = answer));
    await settle();
    assert.equal(ended, undefined);
    await harness.emit("swap-status", status("swap-a", { step, outcome, running: false }));
    await until("the run's end", () => ended !== undefined);
    assert.equal(ended, outcome);
    await run;
    // No longer listening.
    await harness.emit("swap-status", status("swap-a", { step: "swapping", running: true }));
    await settle();
    assert.equal(buyer.status().step, step);
  });
}

test("the account's address never overwrites one typed", async (t) => {
  const { doc, text, button, type, mount, settle } = await page(t);
  await mount();
  await until("the offer", () => text().includes("best of 1"));
  const typed = cid(9);
  type("input#to", typed);
  button("Offers").click();
  await until("the offers", () => !doc.querySelector("input#to"));
  button("Swap", "button.tab").click();
  await until("the form", () => !!doc.querySelector("input#to"));
  await settle();
  assert.equal((doc.querySelector("input#to") as HTMLInputElement).value, typed);
});

test("the reminder (.ics) is not offered: the app notifies from the tray", async (t) => {
  const { window, doc, state, text, mount } = await page(t);
  state.followed = status("swap-a", {
    step: "swapping", running: true, backupCovers: "lock",
    deadlines: { cancel: { height: 200, eta: Date.now() + 3_600_000 }, refund: { from: 200, until: 300 }, punish: { height: 300, eta: Date.now() + 7_200_000 } },
  });
  const style = doc.createElement("style");
  style.textContent = readFileSync(new URL("../src/desktop.css", import.meta.url), "utf8");
  doc.head.append(style);
  await mount();
  await until("the reminder", () => text().includes("Save reminder (.ics)"));
  const reminder = [...doc.querySelectorAll("button")].find((b) => b.textContent?.trim() === "Save reminder (.ics)")!;
  assert.equal(window.getComputedStyle(reminder).display, "none");
});

test("a wallet is restored only from a file the shell reads", async (t) => {
  const picks: (string | null)[] = [null, "refused: the file is for bitcoin, not signet", "tb1qrestored"];
  const { window, doc, calls, text, button, mount, settle } = await page(t, (cmd) => {
    if (cmd !== "swap_restore_file") return undefined;
    const pick = picks.shift()!;
    if (pick !== null && pick.startsWith("refused")) throw pick;
    return pick;
  });
  const style = doc.createElement("style");
  style.textContent = readFileSync(new URL("../src/desktop.css", import.meta.url), "utf8");
  doc.head.append(style);
  await mount();
  // The page's own file input, whose file the page would read, is not shown.
  button("Settings").click();
  await until("the settings", () => !!doc.querySelector("input#restore"));
  const field = doc.querySelector("input#restore")!.closest(".field")!;
  assert.equal(window.getComputedStyle(field).display, "none");

  // Were it reached, the page's restore would hand the file's text to the
  // shim, which refuses it: no key leaves for the shell from the page.
  const input = doc.querySelector("input#restore") as HTMLInputElement;
  const file = new window.File(['{"wallet":"cV"}'], "lattice-swap.json");
  // jsdom's File has no text().
  Object.defineProperty(file, "text", { value: async () => '{"wallet":"cV"}' });
  Object.defineProperty(input, "files", { value: [file] });
  input.dispatchEvent(new window.Event("change", { bubbles: true }));
  await until("the shim's refusal", () => text().includes("refused: restore with Restore from file, above"));

  // No file picked: nothing happens.
  button("Restore from file").click();
  await settle();
  assert.equal(calls.filter(([cmd]) => cmd === "swap_restore_file").length, 1);
  assert.ok(doc.querySelector("input#restore"));
  // Refused: the shell's words.
  button("Restore from file").click();
  await until("the refusal", () => text().includes("refused: the file is for bitcoin, not signet"));
  // Restored: the page starts again on the restored wallet.
  const opened = calls.filter(([cmd]) => cmd === "swap_wallet_open").length;
  button("Restore from file").click();
  await until("restored", () => text().includes("Restored the wallet tb1qrestored"));
  await until("the page again", () => !doc.querySelector("input#restore") && calls.filter(([cmd]) => cmd === "swap_wallet_open").length > opened);
  assert.ok(!calls.some(([cmd]) => cmd === "swap_restore"));
});

test("Bitcoin is read through mempool.space until your own Esplora is saved, refused off its network", async (t) => {
  const OWN = "https://esplora.example/api";
  let saved: string | null = null;
  let refusals = 1;
  const { doc, calls, text, button, type, mount, window } = await page(t, (cmd, args) => {
    if (cmd === "swap_settings") return { network: "signet", esplora: saved, relay: null };
    if (cmd === "swap_set_esplora") {
      if (refusals-- > 0) throw `${OWN}: refused: the Esplora's genesis is 000000000019d6, not signet's`;
      saved = args.esplora as string | null;
      return null;
    }
    if (cmd === "swap_check_esplora" && saved !== null) throw `${OWN}: timed out`;
  });
  await mount();
  const server = doc.querySelector('select[aria-label="Bitcoin server"]') as HTMLSelectElement;
  const url = doc.querySelector('input[aria-label="Your Esplora URL"]') as HTMLInputElement;
  const trust = [...doc.querySelectorAll("p")].find((p) => p.textContent?.includes("keep amounts small"))!;
  await until("public's status", () => text().includes("Connected to mempool.space"));
  assert.equal(server.value, "public");
  assert.ok(url.hidden && !trust.hidden);

  server.value = "own";
  server.dispatchEvent(new window.Event("change", { bubbles: true }));
  assert.ok(!url.hidden && trust.hidden);
  type('input[aria-label="Your Esplora URL"]', OWN);
  // Refused: the shell's words, nothing saved.
  button("Save").click();
  await until("the refusal", () => text().includes("not signet's"));
  assert.equal(saved, null);
  button("Save").click();
  await until("your Esplora's status", () => text().includes(`${OWN}: timed out`));
  assert.equal(saved, OWN);
  assert.ok(!text().includes("not signet's"));

  // Back to the public one.
  server.value = "public";
  server.dispatchEvent(new window.Event("change", { bubbles: true }));
  button("Save").click();
  await until("public's status again", () => text().includes("Connected to mempool.space"));
  assert.equal(saved, null);
  assert.deepEqual(calls.filter(([cmd]) => cmd === "swap_set_esplora").map(([, args]) => args.esplora), [OWN, OWN, null]);
});

test("isCid spells a CID as swap_nexus_tx::is_cid does", async () => {
  const dom = new JSDOM(``, { runScripts: "outside-only" });
  dom.window.eval(await bundle());
  const { isCid } = (dom.window as unknown as { harness: { isCid: (text: string) => boolean } }).harness;
  assert.ok(isCid(ACCOUNT));
  assert.ok(isCid(OFFER.template.nexus_genesis));
  assert.ok(!isCid(ACCOUNT.toUpperCase()));
  assert.ok(!isCid(ACCOUNT.slice(0, -1)));
  assert.ok(!isCid(`${ACCOUNT}a`));
  // A spare bit set: not the canonical spelling.
  assert.equal(ACCOUNT.at(-1), "4");
  assert.ok(!isCid(ACCOUNT.slice(0, -1) + "5"));
  assert.ok(!isCid("tb1qwallet"));
  // Lattice's own address vectors, as swap-nexus-tx (the pinned lattice-swap) carries them.
  const vectors = JSON.parse(readFileSync(new URL("../vendor/lattice-swap-web/vendor/lattice-swap/crates/swap-nexus-tx/tests/vectors/addresses.json", import.meta.url), "utf8")) as { vectors: { address: string }[] };
  assert.ok(vectors.vectors.length > 0);
  for (const { address } of vectors.vectors) {
    assert.ok(isCid(address), address);
    assert.ok(!isCid(address.slice(0, -1)), address);
    assert.ok(!isCid(address.replace(/^b/, "B")), address);
  }
  dom.window.close();
});
