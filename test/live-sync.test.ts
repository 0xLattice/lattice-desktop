// Opt-in, live: the app's node on the real network. With the node's built-in
// Nexus peers and throwaway storage, the app starts its node, the Node screen
// shows it catching up, and the wallet reads through it meanwhile. Read-only:
// nothing is signed or submitted. It needs the network, so it is not part of
// the default run.
//
//   LATTICE_LIVE=1 LATTICE_NODE_BIN=<lattice-node checkout>/.build/debug npm test

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startApp } from "../src/app.ts";
import { NodeManager } from "../src/node-manager.ts";
import { balanceShown, bin, button, freePort, harnessHost, input, installPage, pageText, press, shows, until, openNode } from "./helpers/real-node.ts";

const live = process.env.LATTICE_LIVE === "1" && !!bin;

test("live: the app's node syncs Nexus from its built-in peers, and the wallet reads through it", { skip: !live, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-live-"));
  const host = harnessHost(root);
  try {
    // Free ports only: the peers stay the node's built-in ones, as on a first run.
    const setup = new NodeManager(host.ops);
    const prefs = await setup.prefs();
    assert.equal(prefs.peers, "default");
    await setup.configure({ ...prefs, rpc: await freePort(), listen: await freePort() });
    await installPage();
    const { manager } = await startApp(host);
    button("Create wallet").click();
    input("password (min 8)").value = "correct horse battery";
    input("confirm password").value = "correct horse battery";
    button("Continue").click();
    button("I saved it — create").click();
    await shows(/Balance/);

    openNode();
    await shows(/Your node/);
    await press("Start node");
    // Headers arrive ahead of execution: the screen says how far along it is.
    await shows(/Nexussyncing · [\d,]+ of [\d,]+/, 300_000);
    const [early] = await manager.status();
    const progressed = await until(async () => {
      const [now] = await manager.status();
      return now?.height !== undefined && now.height > (early?.height ?? 0n) && now.height > 0n ? now.height : undefined;
    }, "executed height to advance", 300_000);
    assert.ok(progressed > 0n);

    // A new account holds nothing on the real chain; the read is answered by this node.
    button("Back to wallet").click();
    await until(() => {
      if (balanceShown() === "0") return true;
      button("Refresh balance").click();
      return undefined;
    }, "a balance read from the syncing node");

    openNode();
    await shows(/Your node/);
    await press("Stop node");
    await shows(/Nexusstopped/);
  } catch (e) {
    try {
      console.error(readFileSync(join(root, "log", "lattice-node.log"), "utf8").slice(-4000));
    } catch {}
    throw e;
  } finally {
    document.body?.replaceChildren();
    await host.ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
