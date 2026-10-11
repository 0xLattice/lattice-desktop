// End to end, headless: opening the app as it ships. Nothing here starts the
// node: the app does, when it opens, and the home screen says what it is
// doing. On the real `lattice` binaries with throwaway storage and no peers;
// only the shell is replaced (see e2e-app.test.ts).
//
//   LATTICE_NODE_BIN=<lattice-node checkout>/.build/debug npm test

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startApp } from "../src/app.ts";
import { NodeManager } from "../src/node-manager.ts";
import { balanceShown, bin, button, freePort, harnessHost, input, installPage, pageText, shows, until } from "./helpers/real-node.ts";

test("the app starts its node when it opens: a new wallet lands on a balance, not an error", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-first-run-"));
  const host = { ...harnessHost(root), autoStart: true };
  try {
    const setup = new NodeManager(host.ops);
    await setup.configure({ ...(await setup.prefs()), rpc: await freePort(), listen: await freePort(), peers: "none" });
    await installPage();
    const { manager } = await startApp(host);
    // Before there is a wallet, the header offers nothing.
    assert.equal(document.getElementById("settings-button")!.style.visibility, "hidden");
    button("Create wallet").click();
    input("password (min 8)").value = "correct horse battery";
    input("confirm password").value = "correct horse battery";
    button("Continue").click();
    button("I saved it — create").click();
    await shows(/Balance on Nexus/);
    // No one pressed Start: the node came up with the app, and the wallet reads it.
    await until(() => {
      if (balanceShown() === "0") return true;
      if (/Balance/.test(pageText())) button("Refresh balance").click();
      return undefined;
    }, "a balance read with no one starting the node");
    assert.equal((await manager.status())[0]?.state, "active");
    assert.doesNotMatch(pageText(), /Your node is stopped/);
    await shows(/Your node is set to connect to no one, so it sees only its own blocks/);
    assert.equal(document.querySelector("#settings-button .brand-label")?.textContent, "Settings");
  } catch (e) {
    try {
      console.error(readFileSync(join(root, "log", "lattice-node.log"), "utf8").slice(-3000));
    } catch {}
    throw e;
  } finally {
    globalThis.document?.body.replaceChildren();
    await host.ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
