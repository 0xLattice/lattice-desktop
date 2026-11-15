// What the end-to-end tests share: the shell's commands in Node (the same
// CLI, files, and loopback-only HTTP with the node's cookie), a miner, and
// the app's real page in a DOM. Nothing here touches a live chain.

import assert from "node:assert/strict";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, openSync, closeSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";
import { promisify } from "node:util";
import type { AppHost } from "../../src/app.ts";
import type { HostOps, MinerSettings, Topology } from "../../src/node-manager.ts";

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

const readJSON = <T>(file: string): T | null => {
  try {
    return JSON.parse(readFileSync(file, "utf8")) as T;
  } catch {
    return null;
  }
};

/**
 * The node as the shell runs it: `lattice up` as this process's child,
 * started again when it exits 75, left stopped on any other exit (the shell
 * also starts it again after a crash, which no test here needs), its output
 * appended to `<root>/log/lattice-node.log`.
 */
function nodeProcess(root: string): Pick<HostOps, "startNode" | "stopNode" | "nodeProcess"> {
  let child: ChildProcess | undefined;
  let wanted = false;
  let error: string | undefined;
  let exited = Promise.resolve();
  const up = () => {
    mkdirSync(join(root, "log"), { recursive: true });
    const log = openSync(join(root, "log", "lattice-node.log"), "a");
    const running = spawn(join(bin!, "lattice"), ["up", "--root", root], { stdio: ["ignore", log, log] });
    closeSync(log);
    child = running;
    exited = new Promise((done) => running.once("exit", (code) => {
      child = undefined;
      if (wanted && code === 75) up();
      else {
        if (wanted && code === 78) error = `The node's configuration does not check (exit 78); see ${join(root, "log", "lattice-node.log")}`;
        wanted = false;
      }
      done();
    }));
  };
  return {
    async startNode() {
      wanted = true;
      error = undefined;
      if (!child) up();
    },
    async stopNode() {
      wanted = false;
      // Through each restart it may be in.
      while (child) {
        child.kill("SIGTERM");
        await exited;
      }
    },
    nodeProcess: async () => ({ running: wanted, error: error ?? null }),
  };
}

/** The shell, in Node, over the host root `root`. The wallet's files live beside it, as in the app. */
export function harnessHost(root: string): AppHost {
  const topologyFile = join(root, "lattice.json");
  const operatorFile = join(root, "config.d", "operator.json");
  const minerFile = join(root, "miner.json");
  const walletDir = join(root, "wallet");
  const file = (key: string) => join(walletDir, `${key}.json`);
  const load = (key: string): unknown => (existsSync(file(key)) ? JSON.parse(readFileSync(file(key), "utf8")) : null);
  const save = (key: string, value: unknown) => {
    mkdirSync(walletDir, { recursive: true });
    writeFileSync(file(key), JSON.stringify(value));
  };
  const node = nodeProcess(root);
  const nodeHTTP: AppHost["nodeHTTP"] = async (method, path, body) => {
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
  };
  return {
    ops: {
      ...node,
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
      readTopology: async () => readJSON<Topology>(topologyFile),
      async writeTopology(topology) {
        writeFileSync(topologyFile, JSON.stringify(topology, null, 2));
      },
      operatorChains: async () => readJSON<{ hostedChains?: string[] }>(operatorFile)?.hostedChains ?? [],
      // As the shell does: lattice.json's list moved into the operator
      // fragment, through the node while it runs (written only while still
      // the version read), or into the file while it is stopped.
      async hostChains(chains) {
        const topology = readJSON<Topology>(topologyFile)!;
        const { hostedChains: _, ...rest } = topology;
        writeFileSync(topologyFile, JSON.stringify(rest, null, 2));
        const set = (fragment: Record<string, unknown> | null) => {
          const { hostedChains: _old, ...others } = fragment ?? {};
          return chains.length ? { ...others, hostedChains: chains } : others;
        };
        if (!(await node.nodeProcess()).running) {
          mkdirSync(join(root, "config.d"), { recursive: true });
          writeFileSync(operatorFile, JSON.stringify(set(readJSON(operatorFile)), null, 2));
          return;
        }
        const { rpc } = topology;
        const url = `http://127.0.0.1:${rpc}/config/fragments/operator`;
        const authorization = "Basic " + btoa(readFileSync(cookieFile(root), "utf8").trim());
        for (;;) {
          const current = await fetch(url, { headers: { Authorization: authorization } });
          const precondition: Record<string, string> = current.status === 200 ? { "If-Match": current.headers.get("ETag")! } : { "If-None-Match": "*" };
          const written = await fetch(url, {
            method: "PUT",
            headers: { Authorization: authorization, "Content-Type": "application/json", ...precondition },
            body: JSON.stringify(set(current.status === 200 ? await current.json() : null)),
          });
          if (written.status === 412) continue;
          if (written.status !== 200) {
            writeFileSync(topologyFile, JSON.stringify(topology, null, 2));
            throw new Error(`the node refused the hosted chains (${written.status}): ${await written.text()}`);
          }
          return;
        }
      },
      readMiner: async () => readJSON<MinerSettings>(minerFile) ?? readJSON<Topology & { mine?: MinerSettings }>(topologyFile)?.mine ?? null,
      async writeMiner(settings) {
        writeFileSync(minerFile, JSON.stringify(settings, null, 2));
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
    nodeHTTP,
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
  // Asked until it answers: a node that has just restarted may not have written its cookie yet.
  const target = (await until(async () => height(), "the node's height")) + BigInt(blocks);
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
