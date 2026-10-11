// End to end, headless: mining from the desktop app, on the real `lattice`
// binaries with throwaway storage and no peers. The app's Mining screen runs
// the bundled mining loop (`lattice mine`), which runs the bundled coordinator
// and CPU worker; nothing in this test mines on its own. Only the shell is
// replaced (see e2e-app.test.ts).
//
//   LATTICE_NODE_BIN=<lattice-node checkout>/.build/debug npm test

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startApp } from "../src/app.ts";
import { NodeManager, type Topology } from "../src/node-manager.ts";
import { loopbackFetch } from "../src/loopback.ts";
import { reader } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { bin, button, freePort, harnessHost, input, installPage, pageText, shows, until } from "./helpers/real-node.ts";

test("the app: mine to the wallet, change who is paid, stop", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-mining-"));
  const host = harnessHost(root);
  const topology = () => JSON.parse(readFileSync(join(root, "lattice.json"), "utf8")) as Topology;
  try {
    const setup = new NodeManager(host.ops);
    await setup.configure({ ...(await setup.prefs()), rpc: await freePort(), listen: await freePort(), peers: "none" });
    const dom = await installPage();
    const { manager, ownNode } = await startApp(host);
    const reads = reader(ownNode, ["Nexus"], loopbackFetch(ownNode, host.nodeHTTP));
    const balance = async (account: string) => (await reads.account(account)).balance;

    button("Create wallet").click();
    input("password (min 8)").value = "correct horse battery";
    input("confirm password").value = "correct horse battery";
    button("Continue").click();
    button("I saved it — create").click();
    await shows(/Balance/);
    const first = document.querySelector(".account-address")!.textContent!;

    // ---- mining needs the node: asked for without it, the app says so ----
    button("Node").click();
    await shows(/Your node/);
    button("Mining").click();
    await shows(/Mine on your node/);
    await shows(/Statusnot mining/);
    // Unless told otherwise, each chain pays the wallet's active account.
    const recipient = () => document.querySelector('select[aria-label="Recipient on Nexus"]') as HTMLSelectElement;
    assert.equal(recipient().value, first);
    button("Start mining").click();
    await shows(/Start the node first: mining runs on it/);
    assert.deepEqual(await manager.miningStatus(), { running: false });
    button("Back").click();
    await shows(/Your node/);
    button("Start").click();
    await shows(/Nexusactive/);

    // ---- mine: the wallet's balance grows with no other miner ----
    button("Mining").click();
    await shows(/Statusnot mining/);
    button("Start mining").click();
    await shows(/Statusmining \(pid \d+\)/);
    assert.deepEqual(topology().mine, { workers: 1, recipients: { Nexus: first } });
    button("Back").click();
    await shows(/Your node/);
    button("Back to wallet").click();
    await until(() => {
      if (/Balance[\d,]*[1-9]/.test(pageText())) return true;
      button("Refresh balance").click();
      return undefined;
    }, "mined funds in the wallet");

    // ---- pay a second account instead: saved while mining, the loop restarts ----
    const picker = document.querySelector("select.account-picker") as HTMLSelectElement;
    picker.value = "__add";
    picker.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    const second = await until(() => {
      const shown = document.querySelector(".account-address")?.textContent;
      return shown && shown !== first ? shown : undefined;
    }, "a second account");
    assert.equal(await balance(second), 0n);
    button("Node").click();
    await shows(/Your node/);
    button("Mining").click();
    await shows(/Statusmining/);
    // What was saved is shown, not the account that happens to be active now.
    assert.equal(recipient().value, first);
    recipient().value = second;
    (document.querySelector('input[aria-label="Workers"]') as HTMLInputElement).value = "2";
    button("Save").click();
    await until(() => (topology().mine?.recipients?.Nexus === second ? true : undefined), "the new recipient saved");
    await shows(/Statusmining \(pid \d+\)/);
    assert.equal(topology().mine?.workers, 2);
    await until(async () => ((await balance(second)) > 0n ? true : undefined), "blocks paying the second account");

    // ---- stop: no more blocks ----
    button("Stop mining").click();
    await shows(/Statusnot mining/);
    const stoppedAt = (await reads.chainInfo()).height;
    await new Promise((r) => setTimeout(r, 3000));
    assert.equal((await reads.chainInfo()).height, stoppedAt, "the chain stands still once mining stops");

    // ---- stopping the node stops a running miner with it ----
    button("Start mining").click();
    await shows(/Statusmining/);
    button("Back").click();
    await shows(/Your node/);
    button("Stop").click();
    await shows(/Nexusstopped/);
    assert.deepEqual(await manager.miningStatus(), { running: false });
  } catch (e) {
    for (const log of ["lattice-node.log", "mine.log"]) {
      try {
        console.error(readFileSync(join(root, "log", log), "utf8").slice(-3000));
      } catch {}
    }
    throw e;
  } finally {
    globalThis.document?.body.replaceChildren();
    await host.ops.lattice("mine-stop");
    await host.ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
