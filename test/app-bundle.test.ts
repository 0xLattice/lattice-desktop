// The packaged app, launched for real with a throwaway home directory. The
// other end-to-end tests replace the shell; this one is the shell: the bundle
// starts, the webview loads the page under its content-security policy, the
// page reaches the Rust commands, and they run the bundled CLI, which starts
// the bundled node. It cannot see or click the window.
//
//   LATTICE_APP=src-tauri/target/release/bundle/macos/Lattice.app npm test
//
// (an app built with the real node binaries staged, not CI's placeholders).

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { freePort, run, until } from "./helpers/real-node.ts";

const app = process.env.LATTICE_APP;

test("the packaged app opens and starts its own node through the bundled CLI", { skip: !app, timeout: 240_000 }, async () => {
  const home = mkdtempSync(join(tmpdir(), "lattice-desktop-home-"));
  const macOS = join(resolve(app!), "Contents", "MacOS");
  for (const binary of ["lattice-desktop", "lattice", "lattice-node", "lattice-mining-coordinator", "lattice-miner", "lattice-miner-gpu"]) {
    assert.ok(existsSync(join(macOS, binary)), `${binary} is in the bundle`);
  }
  const data = join(home, "Library", "Application Support", "Lattice");
  const root = join(data, "host");
  const topologyFile = join(root, "lattice.json");
  // A host root the app will find: a private node (no peers) on free ports, so
  // the test joins no network and collides with no node already on this machine.
  await run(join(macOS, "lattice"), ["init", "--root", root]);
  const rpc = await freePort();
  writeFileSync(topologyFile, JSON.stringify({ ...JSON.parse(readFileSync(topologyFile, "utf8")), rpc, listen: await freePort(), peers: [] }, null, 2));
  const child = spawn(join(macOS, "lattice-desktop"), [], { env: { ...process.env, HOME: home }, stdio: "ignore" });
  const exited = new Promise<void>((done) => child.once("exit", () => done()));
  try {
    // Nothing but the page starts the node: its answering means the page ran,
    // reached the shell, and the shell ran the bundled CLI and node.
    await until(async () => ((await fetch(`http://127.0.0.1:${rpc}/health`)).ok ? true : undefined), "the app to start its node", 180_000);
    assert.ok(existsSync(join(root, "chains", "Nexus", ".cookie")), "the node the app started wrote its cookie");
    assert.equal(child.exitCode, null, "the app is still running");
    // Opening the app creates no wallet and starts no miner.
    assert.equal(existsSync(join(data, "wallet", "vault.json")), false);
    assert.match((await run(join(macOS, "lattice"), ["status", "--root", root])).stdout, /^Nexus: active/m);
    assert.equal(existsSync(join(root, "log", "mine.log")), false);
  } finally {
    // Killing the app does not run its exit handler, so stop its node as the handler would.
    await run(join(macOS, "lattice"), ["down", "--root", root]).catch(() => {});
    child.kill();
    await exited;
    rmSync(home, { recursive: true, force: true });
  }
});
