// End to end, headless: the desktop app itself (src/app.ts on its own page,
// src/index.html) driven as a person would, against the real `lattice`
// binaries on throwaway storage with no peers. Only the shell is replaced: a
// Node harness does what the Tauri commands do. Nothing touches a live chain.
//
//   LATTICE_NODE_BIN=<lattice-node checkout>/.build/debug npm test

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startApp } from "../src/app.ts";
import { NodeManager } from "../src/node-manager.ts";
import { loopbackFetch } from "../src/loopback.ts";
import { reader } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { importPrivateKey } from "../vendor/nexus-wallet/src/lib/crypto/accounts.ts";
import { balanceShown, bin, button, freePort, funded, harnessHost, input, installPage, mine, pageText, press, run, shows, until } from "./helpers/real-node.ts";

const PASSWORD = "correct horse battery";

test("the app: first run, send, lock, relaunch, host a child chain from the menu, survive a node crash", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-app-"));
  const host = harnessHost(root);
  try {
    // A private node on free ports (the app's defaults may be taken on this machine).
    const setup = new NodeManager(host.ops);
    await setup.configure({ ...(await setup.prefs()), rpc: await freePort(), listen: await freePort(), peers: "none" });

    // ---- first run: no wallet, node stopped ----
    await installPage();
    const { manager, ownNode } = await startApp(host);
    const reads = (chain: string) => reader(ownNode, chain.split("/"), loopbackFetch(ownNode, host.nodeHTTP));
    const height = async (chain = "Nexus") => (await reads(chain).chainInfo()).height ?? 0n;
    assert.match(pageText(), /Create wallet/);
    button("Create wallet").click();
    input("password (min 8)").value = PASSWORD;
    input("confirm password").value = PASSWORD;
    button("Continue").click();
    const phrase = document.querySelector(".mnemonic")?.textContent ?? "";
    assert.equal(phrase.trim().split(/\s+/).length, 12);
    button("I saved it — create").click();
    await shows(/Balance/);
    const address = document.querySelector(".account-address")?.textContent ?? "";

    // The node is stopped (this test leaves auto-start off to see that). The
    // home screen says so in words, in ordinary type, and offers the way out.
    await shows(/Your node is stopped, so nothing can be read or sent\.Start node/);
    assert.equal(document.querySelector(".balance-value .v")!.classList.contains("unread"), true);
    await press("Start node");
    await until(() => (balanceShown() === "0" ? true : undefined), "a balance read from the started node");
    await until(() => (document.querySelector(".notice") ? undefined : true), "the notice to clear once the node is in sync");
    // The Node screen says the same in its own terms, with one button for the one thing to do.
    button("Node").click();
    await shows(/Nexusin sync · block 0Miningoff/);
    assert.deepEqual([...document.querySelectorAll("#view button")].map((item) => item.textContent).slice(0, 2), ["Stop node", "Mining"]);
    button("Back to wallet").click();
    await shows(/Balance/);
    await mine(root, ownNode, { Nexus: address }, 2, height);
    button("Refresh balance").click();
    await until(() => (funded() ? true : undefined), "a funded balance");
    button("Send").click();
    await shows(/Review/);
    const bob = importPrivateKey("b0".repeat(32)).address;
    input("recipient address (bafy…)").value = bob;
    (document.querySelector('input[aria-label="Amount"]') as HTMLInputElement).value = "1000";
    (document.querySelector("details.advanced input") as HTMLInputElement).value = "23";
    button("Review").click();
    await shows(/Sign & send/);
    assert.match(pageText(), /Amount1,000 on NexusNetwork fee23 on NexusTotal1,023 on Nexus/);
    button("Sign & send").click();
    await shows(/Admitted to the node's pool/);
    await mine(root, ownNode, { Nexus: address }, 1, height);
    assert.equal((await reads("Nexus").account(bob)).balance, 1000n);
    button("Done").click();
    await shows(/Balance/);
    document.getElementById("settings-button")!.click();
    button("Transactions").click();
    await shows(/Sent1,000Nexus\d+ confirmation/);
    button("Back").click();

    // ---- lock, a wrong password, unlock ----
    button("Lock wallet").click();
    await shows(/Unlock/);
    input("password").value = "not the password";
    button("Unlock").click();
    await shows(/Wrong password/);
    input("password").value = PASSWORD;
    button("Unlock").click();
    await shows(/Balance/);

    // ---- relaunch: the vault and settings files are all that carries over ----
    document.body.replaceChildren();
    await installPage();
    await startApp(host);
    await shows(/Unlock/);
    input("password").value = PASSWORD;
    button("Unlock").click();
    await shows(/Balance/);
    assert.equal(document.querySelector(".account-address")?.textContent, address);
    document.getElementById("settings-button")!.click();
    button("Transactions").click();
    await shows(/Sent1,000Nexus\d+ confirmation/);
    button("Back").click();
    document.getElementById("settings-button")!.click(); // on Settings, the mark closes it
    await shows(/Balance/);

    // ---- a child chain exists on Nexus, but this node does not host it ----
    await run(join(bin!, "lattice"), ["child", "create", "Nexus/testnet", "--root", root, "--reward", "100000"]);
    // `child create` restarts the node; the child's genesis is mined with the next Nexus block.
    await until(async () => ((await manager.status())[0]?.state === "active" ? true : undefined), "the node back up");
    await mine(root, ownNode, { Nexus: address, "Nexus/testnet": address }, 3, height);
    await until(async () => ((await manager.status()).every((c) => c.state === "active") ? true : undefined), "both chains active");
    button("Node").click();
    await shows(/Nexus\/testnetin sync/);
    button("Node settings").click();
    await shows(/Chains this node hosts/);
    // The chain is listed with its box ticked: hosting it or not is a tick, not a typed path.
    const hosted = document.querySelector('input[aria-label="Nexus/testnet"]') as HTMLInputElement;
    assert.equal(hosted.checked, true);
    hosted.checked = false;
    button("Save").click();
    await shows(/Your node/);
    await shows(/Nexusin sync/);
    assert.deepEqual(await manager.chains(), ["Nexus"]);
    button("Back to wallet").click();
    await shows(/Balance/);

    // The chain menu offers it, saying what choosing it does; choosing it hosts it.
    document.getElementById("net-badge")!.click();
    await shows(/testnetadd to node ›/);
    const item = [...document.querySelectorAll(".chain-menu-item")].find((entry) => entry.firstChild?.textContent === "testnet") as HTMLButtonElement;
    item.click();
    await until(async () => ((await manager.chains()).includes("Nexus/testnet") ? true : undefined), "the node to host Nexus/testnet");
    await until(() => (document.getElementById("net-badge")!.textContent === "Nexus/testnet ▾" ? true : undefined), "the wallet on Nexus/testnet");
    // The node restarted to take the chain: the balance reads once it is back.
    await until(() => {
      if (funded()) return true;
      button("Refresh balance").click();
      return undefined;
    }, "the child-chain balance");
    assert.ok((await reads("Nexus/testnet").account(address)).balance > 0n);
    document.getElementById("parent-chain")!.click();
    await until(() => (document.getElementById("net-badge")!.textContent === "Nexus ▾" ? true : undefined), "the wallet back on Nexus");
    await shows(/Balance/);
    document.getElementById("net-badge")!.click();
    await shows(/testnet›/);
    assert.doesNotMatch(pageText(), /add to node/);
    document.getElementById("net-badge")!.click();

    // ---- the node dies under the app; the wallet says so and the node screen brings it back ----
    await run("pkill", ["-9", "-f", root]);
    await until(() => {
      if (/node unreachable/.test(pageText())) return true;
      button("Refresh balance").click();
      return undefined;
    }, "the wallet to report the dead node");
    button("Node").click();
    await shows(/Your node/);
    await until(() => (/Nexus(stopped|starting)/.test(pageText()) ? true : undefined), "the node screen to show it down");
    await press("Start node");
    await shows(/Nexusin sync/);
    button("Back to wallet").click();
    await until(() => {
      if (funded()) return true;
      button("Refresh balance").click();
      return undefined;
    }, "the balance after the restart");

    button("Node").click();
    await shows(/Your node/);
    await press("Stop node");
    await shows(/Nexusstopped/);
  } catch (e) {
    try {
      console.error(readFileSync(join(root, "log", "lattice-node.log"), "utf8").slice(-4000));
    } catch {}
    throw e;
  } finally {
    document.body.replaceChildren(); // the node screen stops polling once it is off the page
    await host.ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
