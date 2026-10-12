// End to end, headless: keeping, recovering and protecting a wallet in the
// desktop app, on the real `lattice` binaries with throwaway storage and no
// peers. Only the shell is replaced (see e2e-app.test.ts).
//
//   LATTICE_NODE_BIN=<lattice-node checkout>/.build/debug npm test

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startApp } from "../src/app.ts";
import { NodeManager } from "../src/node-manager.ts";
import { loopbackFetch } from "../src/loopback.ts";
import { reader } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { importPrivateKey } from "../vendor/nexus-wallet/src/lib/crypto/accounts.ts";
import { bin, button, cookieFile, freePort, harnessHost, input, installPage, mine, pageText, run, shows, until, openNode } from "./helpers/real-node.ts";

const PASSWORD = "correct horse battery";

test("the app: key file import, auto-lock mid-send, fee replacement, backup and restore, extension pairing", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-custody-"));
  const other = mkdtempSync(join(tmpdir(), "lattice-desktop-restore-"));
  // The idle lock is ten minutes in the app; the test shortens it for one scene.
  let autoLockMs = 10 * 60 * 1000;
  const host = { ...harnessHost(root), get autoLockMs() { return autoLockMs; } };
  try {
    const setup = new NodeManager(host.ops);
    await setup.configure({ ...(await setup.prefs()), rpc: await freePort(), listen: await freePort(), peers: "none" });
    const dom = await installPage();
    const { manager, ownNode } = await startApp(host);
    const reads = reader(ownNode, ["Nexus"], loopbackFetch(ownNode, host.nodeHTTP));
    const height = async () => (await reads.chainInfo()).height ?? 0n;

    button("Create wallet").click();
    input("password (min 8)").value = PASSWORD;
    input("confirm password").value = PASSWORD;
    button("Continue").click();
    button("I saved it — create").click();
    await shows(/Balance/);
    const first = document.querySelector(".account-address")!.textContent!;
    await manager.start();
    await until(async () => ((await manager.status())[0]?.state === "active" ? true : undefined), "the node up");

    // ---- a key the node's own CLI generated, imported from its file ----
    const keyFile = join(root, "miner.key");
    await run(join(bin!, "lattice"), ["key", "generate", "--out", keyFile]);
    const cliKey = JSON.parse(readFileSync(keyFile, "utf8")) as { address: string };
    const picker = () => document.querySelector("select.account-picker") as HTMLSelectElement;
    picker().value = "__import";
    picker().dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    await shows(/Import key/);
    const keyInput = document.querySelector('input[type="file"]') as HTMLInputElement;
    Object.defineProperty(keyInput, "files", { configurable: true, value: [new File([readFileSync(keyFile)], "miner.key")] });
    keyInput.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    await until(() => ((document.querySelector("textarea") as HTMLTextAreaElement).value ? true : undefined), "the key file to load");
    button("Import").click();
    await until(() => (document.querySelector(".account-address")?.textContent === cliKey.address ? true : undefined), "the imported account");
    await mine(root, ownNode, { Nexus: cliKey.address }, 2, height);
    const funded = (await reads.account(cliKey.address)).balance;

    // ---- the wallet locks itself while a send is half entered ----
    autoLockMs = 1500;
    const before = document.querySelector("#view")!.firstChild;
    picker().value = cliKey.address;
    picker().dispatchEvent(new dom.window.Event("change", { bubbles: true })); // a signer request: the short timer starts
    await until(() => (document.querySelector("#view")!.firstChild !== before && /Balance/.test(pageText()) ? true : undefined), "the home screen again");
    button("Send").click();
    await shows(/Review/);
    const bob = importPrivateKey("b0".repeat(32)).address;
    input("recipient address (bafy…)").value = bob;
    (document.querySelector('input[aria-label="Amount"]') as HTMLInputElement).value = "1000";
    // Idle past the lock. Looking is not use: the window coming forward (and
    // the wallet reading its signer's state then) must not put the lock off.
    for (let waited = 0; waited < 2500; waited += 250) {
      document.dispatchEvent(new dom.window.Event("visibilitychange"));
      await new Promise((r) => setTimeout(r, 250));
    }
    // Nothing was pressed: the app locked on its own clock, told the wallet,
    // and the wallet covered the page, the half-entered send underneath it.
    await shows(/Wallet locked/);
    autoLockMs = 10 * 60 * 1000;
    const cover = document.querySelector(".lock-cover")!;
    assert.deepEqual([...cover.querySelectorAll("button")].map((item) => item.textContent), ["Unlock"], "a locked wallet offers only to unlock");
    assert.equal(input("recipient address (bafy…)").value, bob, "what was entered is still there");
    (cover.querySelector("input") as HTMLInputElement).value = PASSWORD;
    (cover.querySelector("button") as HTMLButtonElement).click();
    await until(() => (document.querySelector(".lock-cover") ? undefined : true), "the cover to lift");
    button("Review").click();
    await shows(/Sign & send/);
    button("Sign & send").click();
    await shows(/Admitted to the node's pool/);
    button("Done").click();
    await shows(/Balance/);

    // ---- stuck at a low fee: replace it with a higher one ----
    button("Activity").click();
    button("Transactions").click();
    await shows(/Increase fee/);
    button("Increase fee").click();
    await shows(/Same payment and nonce, higher fee/);
    (document.querySelector('[aria-label="Replacement fee"]') as HTMLInputElement).value = "7";
    button("Sign replacement").click();
    await shows(/Replacement submitted\. Both attempts remain saved/);
    await mine(root, ownNode, { Nexus: first }, 1, height);
    // One payment, at the higher fee: the replacement was mined, not both.
    assert.equal((await reads.account(bob)).balance, 1000n);
    assert.equal(funded - (await reads.account(cliKey.address)).balance, 1007n);
    button("Back").click();
    await shows(/Transactions/);
    button("Refresh transactions").click();
    await shows(/not included; its nonce was spent by another transaction|replaced/i);
    button("Back").click();

    // ---- pair a browser extension with this node ----
    button("Back").click();
    await shows(/Balance/);
    openNode();
    await shows(/Your node/);
    button("Pair browser extension").click();
    await shows(/Allow the extension's origin/);
    button("Allow this extension").click();
    await shows(/Enter the extension's id/);
    const extension = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
    const oldCookie = readFileSync(cookieFile(root), "utf8");
    input("the extension's id").value = "abcdefghijklmnopabcdefghijklmnop";
    button("Allow this extension").click();
    await shows(new RegExp("Allowed " + extension));
    await until(async () => (readFileSync(cookieFile(root), "utf8") !== oldCookie && (await fetch(`${ownNode}/health`)).ok ? true : undefined), "the node restarted with a new cookie");
    // The page has no clipboard here, so the cookie is shown instead: it is the node's current one.
    button("Copy the node's cookie").click();
    const cookie = readFileSync(cookieFile(root), "utf8").trim();
    await until(() => (pageText().includes(cookie) ? true : undefined), "the node's cookie");
    const as = (origin: string) => fetch(`${ownNode}/api/chain/info`, { headers: { Origin: origin, Authorization: "Basic " + btoa(cookie) } });
    assert.equal((await as(extension)).status, 200);
    assert.equal((await as("chrome-extension://someoneelseeeeeeeeeeeeeeeeeeeeeee")).status, 403);

    // ---- back it up to a file ----
    button("Back").click();
    await shows(/Your node/);
    button("Back to wallet").click();
    await shows(/Balance/);
    document.getElementById("settings-button")!.click();
    button("Backup & recovery").click();
    await shows(/Backup & transfer/);
    button("Encrypted backup (QR or file)").click();
    input("re-enter your password").value = "not the password";
    button("Show backup").click();
    await shows(/Wrong password/);
    input("re-enter your password").value = PASSWORD;
    button("Show backup").click();
    await shows(/Save file/);
    button("Save file").click();
    await shows(/saved: /);
    const backupName = readdirSync(root).find((name) => name.startsWith("lattice-wallet-backup-"))!;
    const backup = readFileSync(join(root, backupName), "utf8");

    // ---- a new machine: nothing but the backup file and its password ----
    document.body.replaceChildren();
    const fresh = harnessHost(other);
    await installPage();
    await startApp(fresh);
    button("Restore or import").click();
    await shows(/Choose the recovery method you already have/);
    button("Backup, SeedQR or another device").click();
    await shows(/Restore/);
    button("Encrypted backup (QR or file)").click();
    await shows(/Restore a backup/);
    const fileInput = document.querySelector('input[type="file"]') as HTMLInputElement;
    Object.defineProperty(fileInput, "files", { configurable: true, value: [new File([backup], backupName, { type: "text/plain" })] });
    fileInput.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    await shows(/Decrypt backup/);
    input("the backup's password (it becomes this wallet's)").value = "not the password";
    button("Restore").click();
    await shows(/Wrong password/);
    input("the backup's password (it becomes this wallet's)").value = PASSWORD;
    button("Restore").click();
    await shows(/Balance/);
    // Both accounts are back, the imported key among them; this machine's node is not running.
    const restored = [...picker().options].map((option) => option.value);
    assert.deepEqual(restored.slice(0, 2), [first, cliKey.address]);
    assert.match(pageText(), /Your node is stopped/);
    // The backup's password is now this wallet's.
    document.body.replaceChildren();
    await installPage();
    await startApp(fresh);
    await shows(/Unlock/);
    input("password").value = PASSWORD;
    button("Unlock").click();
    await shows(/Balance/);
  } catch (e) {
    try {
      console.error(readFileSync(join(root, "log", "lattice-node.log"), "utf8").slice(-4000));
    } catch {}
    throw e;
  } finally {
    document.body.replaceChildren();
    await host.ops.lattice("down");
    await harnessHost(other).ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
    rmSync(other, { recursive: true, force: true });
  }
});
