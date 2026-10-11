// The packaged app, launched for real with a throwaway home directory. The
// other end-to-end tests replace the shell; this one is the shell: the bundle
// starts, the webview loads the page under its content-security policy, the
// page reaches the Rust commands, and they run the bundled CLI, which starts
// the bundled node. It cannot see or click the window.
//
//   LATTICE_APP=src-tauri/target/release/bundle/macos/Lattice.app npm test
//
// (an app built with the real node binaries staged, not CI's placeholders).
// Off macOS, LATTICE_APP is the app's executable, and LATTICE_APP_BINARIES the
// directory its bundled binaries are in (beside it unless said).

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { freePort, run, until } from "./helpers/real-node.ts";

const app = process.env.LATTICE_APP;

test("the packaged app opens and starts its own node through the bundled CLI", { skip: !app, timeout: 240_000 }, async () => {
  const home = mkdtempSync(join(tmpdir(), "lattice-desktop-home-"));
  const bundle = resolve(app!).endsWith(".app");
  const executable = bundle ? join(resolve(app!), "Contents", "MacOS", "lattice-desktop") : resolve(app!);
  const binaries = bundle ? dirname(executable) : resolve(process.env.LATTICE_APP_BINARIES ?? dirname(executable));
  assert.ok(existsSync(executable), "the app's executable is there");
  // The graphics-chip worker ships where there is one for the platform: macOS.
  for (const binary of ["lattice", "lattice-node", "lattice-mining-coordinator", "lattice-miner", ...(bundle ? ["lattice-miner-gpu"] : [])]) {
    assert.ok(existsSync(join(binaries, binary)), `${binary} is in the bundle`);
  }
  // Where the platform keeps a user's application data, under the throwaway home.
  const data = process.platform === "darwin" ? join(home, "Library", "Application Support", "Lattice") : join(home, ".local", "share", "Lattice");
  const root = join(data, "host");
  const topologyFile = join(root, "lattice.json");
  // A host root the app will find: a private node (no peers) on free ports, so
  // the test joins no network and collides with no node already on this machine.
  await run(join(binaries, "lattice"), ["init", "--root", root]);
  const rpc = await freePort();
  writeFileSync(topologyFile, JSON.stringify({ ...JSON.parse(readFileSync(topologyFile, "utf8")), rpc, listen: await freePort(), peers: [] }, null, 2));
  const child = spawn(executable, [], { env: { ...process.env, HOME: home, XDG_DATA_HOME: "" }, stdio: "ignore" });
  const exited = new Promise<void>((done) => child.once("exit", () => done()));
  try {
    // Nothing but the page starts the node: its answering means the page ran,
    // reached the shell, and the shell ran the bundled CLI and node.
    await until(async () => ((await fetch(`http://127.0.0.1:${rpc}/health`)).ok ? true : undefined), "the app to start its node", 180_000);
    assert.ok(existsSync(join(root, "chains", "Nexus", ".cookie")), "the node the app started wrote its cookie");
    assert.equal(child.exitCode, null, "the app is still running");
    // Opening the app creates no wallet and starts no miner.
    assert.equal(existsSync(join(data, "wallet", "vault.json")), false);
    assert.match((await run(join(binaries, "lattice"), ["status", "--root", root])).stdout, /^Nexus: active/m);
    assert.equal(existsSync(join(root, "log", "mine.log")), false);
  } finally {
    // Killing the app does not run its exit handler, so stop its node as the handler would.
    await run(join(binaries, "lattice"), ["down", "--root", root]).catch(() => {});
    child.kill();
    await exited;
    rmSync(home, { recursive: true, force: true });
  }
});
