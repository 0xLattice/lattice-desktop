// What the end-to-end tests share: the shell's commands in Node (the same
// CLI, files, and loopback-only HTTP with the node's cookie), a miner, and
// the app's real page in a DOM. Nothing here touches a live chain.

import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";
import { promisify } from "node:util";
import type { AppHost } from "../../src/app.ts";
import type { Topology } from "../../src/node-manager.ts";

/** The directory holding lattice, lattice-node and lattice-mining-coordinator. */
export const bin = process.env.LATTICE_NODE_BIN;
export const run = promisify(execFile);
/** The GPU mining worker (a built `lattice-miner-gpu`), when the test has one. */
export const gpuMiner = process.env.LATTICE_GPU_MINER;

export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolve(typeof address === "object" && address ? address.port : 0));
    });
  });
}

export async function until<T>(probe: () => Promise<T | undefined> | T | undefined, what: string | (() => string), ms = 120_000): Promise<T> {
  const deadline = Date.now() + ms;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const value = await probe();
      if (value !== undefined) return value;
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`timed out waiting for ${typeof what === "string" ? what : what()}${last ? `: ${last}` : ""}`);
}

export const cookieFile = (root: string) => join(root, "chains", "Nexus", ".cookie");

/** The shell, in Node, over the host root `root`. The wallet's files live beside it, as in the app. */
export function harnessHost(root: string): AppHost {
  const topologyFile = join(root, "lattice.json");
  const walletDir = join(root, "wallet");
  const file = (key: string) => join(walletDir, `${key}.json`);
  const load = (key: string): unknown => (existsSync(file(key)) ? JSON.parse(readFileSync(file(key), "utf8")) : null);
  const save = (key: string, value: unknown) => {
    mkdirSync(walletDir, { recursive: true });
    writeFileSync(file(key), JSON.stringify(value));
  };
  return {
    ops: {
      async lattice(verb) {
        try {
          // "mine-start" is `lattice mine start`, as the shell maps it.
          const { stdout, stderr } = await run(join(bin!, "lattice"), [...verb.split("-"), "--root", root]);
          return { code: 0, stdout, stderr };
        } catch (e) {
          const failed = e as { code?: number; stdout?: string; stderr?: string };
          return { code: typeof failed.code === "number" ? failed.code : 1, stdout: failed.stdout ?? "", stderr: failed.stderr ?? String(e) };
        }
      },
      async readTopology() {
        try {
          return JSON.parse(readFileSync(topologyFile, "utf8")) as Topology;
        } catch {
          return null;
        }
      },
      async writeTopology(topology) {
        writeFileSync(topologyFile, JSON.stringify(topology, null, 2));
      },
      gpuWorker: async () => gpuMiner ?? null,
      gpuWorkerUsable: async () => gpuMiner !== undefined,
      async miningLog() {
        try {
          return readFileSync(join(root, "log", "mine.log"), "utf8").slice(-64 * 1024);
        } catch {
          return "";
        }
      },
    },
    async nodeHTTP(method, path, body) {
      const { rpc } = JSON.parse(readFileSync(topologyFile, "utf8")) as Topology;
      const response = await fetch(`http://127.0.0.1:${rpc}${path}`, {
        method,
        headers: {
          Authorization: "Basic " + btoa(readFileSync(cookieFile(root), "utf8").trim()),
          ...(body === null ? {} : { "Content-Type": "application/json" }),
        },
        ...(body === null ? {} : { body }),
      });
      return { status: response.status, body: await response.text() };
    },
    async nodeCookie() {
      try {
        return readFileSync(cookieFile(root), "utf8").trim();
      } catch {
        return null;
      }
    },
    vault: {
      load: async () => load("vault") as never,
      save: async (vault) => save("vault", vault),
      remove: async () => rmSync(file("vault"), { force: true }),
    },
    settings: {
      get: async (key) => (key === "settings" ? { settings: load("settings") ?? undefined } : {}),
      set: async (items) => {
        if ("settings" in items) save("settings", items.settings);
      },
    },
    saveBackup: async (name, text) => {
      writeFileSync(join(root, name), text);
      return join(root, name);
    },
    print: () => {},
    reload: () => {},
    // The tests start the node themselves, to see the app with it stopped;
    // e2e-first-run leaves this on, as the shipped app has it.
    autoStart: false,
  };
}

/** One coordinator batch at a time until `height()` has advanced by `blocks`. */
export async function mine(root: string, url: string, recipients: Record<string, string>, blocks: number, height: () => Promise<bigint>) {
  const target = (await height()) + BigInt(blocks);
  await until(async () => {
    await run(join(bin!, "lattice-mining-coordinator"), [
      "--node", url, "--rpc-cookie-file", cookieFile(root), "--workers", "2",
      ...Object.entries(recipients).flatMap(([chain, address]) => ["--recipient", `${chain}=${address}`]),
      "--once", "--no-stale-probe",
    ]);
    return (await height()) >= target || undefined;
  }, `height ${target}`);
}

/** The app's own page (src/index.html), without its script, as the document. */
export async function installPage() {
  const { JSDOM } = await import("jsdom");
  const html = readFileSync(new URL("../../src/index.html", import.meta.url), "utf8");
  const dom = new JSDOM(html, { url: "https://tauri.localhost/" });
  for (const name of ["window", "document", "navigator", "Node", "HTMLElement", "HTMLButtonElement", "HTMLInputElement", "HTMLTextAreaElement", "HTMLSelectElement", "KeyboardEvent", "Event", "DOMParser"]) {
    Object.defineProperty(globalThis, name, { configurable: true, value: (dom.window as unknown as Record<string, unknown>)[name] });
  }
  return dom;
}

export const pageText = () => (document.body.textContent ?? "").replace(/\s+/g, " ").trim();

/** A button by its text or its accessible name. */
export function button(text: string): HTMLButtonElement {
  const found = [...document.querySelectorAll("button")].find((item) => item.textContent === text || item.getAttribute("aria-label") === text || item.getAttribute("data-row") === text);
  assert.ok(found, `button "${text}" exists; page was: ${pageText()}`);
  return found as HTMLButtonElement;
}

export function input(placeholder: string): HTMLInputElement {
  const found = document.querySelector(`input[placeholder="${placeholder}"]`);
  assert.ok(found, `input "${placeholder}" exists; page was: ${pageText()}`);
  return found as HTMLInputElement;
}

/** Wait for the page to say `pattern`. */
export const shows = (pattern: RegExp, ms = 60_000) =>
  until(() => (pattern.test(pageText()) ? true : undefined), () => `the page to show ${pattern}; it shows: ${pageText().slice(0, 600)}`, ms);

/** What the home screen shows as the balance: digits, or a sentence when it could not be read. */
export const balanceShown = () => document.querySelector(".balance-value .v")?.textContent ?? "";
/** Whether the home screen shows a balance above zero. */
export const funded = () => /^[\d,]*[1-9][\d,]*$/.test(balanceShown());

/** Open the node page from the wallet's home screen: it is in Settings. */
export function openNode(): void {
  document.getElementById("settings-button")!.click();
  button("Your node").click();
}

/** Press a button once the screen offers it: one whose label follows what the app has just learned. */
export async function press(text: string): Promise<void> {
  const found = await until(() => [...document.querySelectorAll("button")].find((item) => item.textContent === text && !item.disabled) as HTMLButtonElement | undefined,
    () => `button "${text}"; page was: ${pageText().slice(0, 400)}`, 60_000);
  found.click();
}
