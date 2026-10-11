// The packaged app, launched for real with a throwaway home directory. The
// other end-to-end tests replace the shell; this one is the shell: the bundle
// starts, the webview loads the page under its content-security policy, the
// page reaches the Rust commands, and the bundled `lattice` CLI scaffolds the
// host root on the app's own ports. It cannot see or click the window.
//
//   LATTICE_APP=src-tauri/target/release/bundle/macos/Lattice.app npm test
//
// (an app built with the real node binaries staged, not CI's placeholders).

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { until } from "./helpers/real-node.ts";

const app = process.env.LATTICE_APP;

test("the packaged app boots and scaffolds its node through the bundled CLI", { skip: !app, timeout: 180_000 }, async () => {
  const home = mkdtempSync(join(tmpdir(), "lattice-desktop-home-"));
  const macOS = join(resolve(app!), "Contents", "MacOS");
  for (const binary of ["lattice-desktop", "lattice", "lattice-node", "lattice-mining-coordinator", "lattice-miner", "lattice-miner-gpu"]) assert.ok(existsSync(join(macOS, binary)), `${binary} is in the bundle`);
  const child = spawn(join(macOS, "lattice-desktop"), [], { env: { ...process.env, HOME: home }, stdio: "ignore" });
  const exited = new Promise<void>((done) => child.once("exit", () => done()));
  try {
    const data = join(home, "Library", "Application Support", "Lattice");
    const topologyFile = join(data, "host", "lattice.json");
    // Nothing but the page asks for `lattice init`, and then moves the node to
    // the app's own ports: this file, with those ports, means the page ran and
    // reached the shell twice (the CLI's defaults are 8080 and 4001).
    const ports = () => {
      const topology = JSON.parse(readFileSync(topologyFile, "utf8")) as { rpc: number; listen: number };
      return `${topology.rpc}/${topology.listen}`;
    };
    await until(() => (existsSync(topologyFile) && ports() === "18080/14001" ? true : undefined), "the app to scaffold its host root on its own ports", 120_000);
    assert.equal(child.exitCode, null, "the app is still running");
    // First run: no wallet was created and no node was started on the user's behalf.
    assert.equal(existsSync(join(data, "wallet", "vault.json")), false);
    assert.equal(existsSync(join(data, "host", "chains", "Nexus", ".cookie")), false);
  } finally {
    child.kill();
    await exited;
    rmSync(home, { recursive: true, force: true });
  }
});
