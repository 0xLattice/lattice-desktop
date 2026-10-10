// End to end, headless: a cross-chain trade through the desktop app, on the
// real `lattice` binaries with throwaway storage and no peers. A private
// Nexus/testnet is created on that node. One account sells child-chain funds
// for parent-chain funds, another buys them and withdraws. Only the shell is
// replaced (see e2e-app.test.ts). Nothing touches a live chain.
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
import { reader, activeDeposits } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { bin, button, freePort, harnessHost, input, installPage, mine, pageText, run, shows, until } from "./helpers/real-node.ts";

const PASSWORD = "correct horse battery";
const CHILD = "Nexus/testnet";

const orderURI = (intent: Record<string, unknown>) => {
  const body = { version: 1, asset: "LAT", parentChain: ["Nexus"], childChain: CHILD.split("/"), expiresAt: new Date(Date.now() + 3_600_000).toISOString(), ...intent };
  return `lattice://order?v=1&intent=${Buffer.from(JSON.stringify(body)).toString("base64url")}`;
};

/** Hand the app an order as a person would: dropped on the window as text. */
function dropOrder(dom: Awaited<ReturnType<typeof installPage>>, text: string) {
  const drop = new dom.window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { getData: (type: string) => (type === "text/plain" ? text : ""), files: [], dropEffect: "none" } });
  document.dispatchEvent(drop);
}

test("the app: sell on the child chain, buy on the parent, withdraw", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-trade-"));
  const host = harnessHost(root);
  try {
    const setup = new NodeManager(host.ops);
    await setup.configure({ ...(await setup.prefs()), rpc: await freePort(), listen: await freePort(), peers: "none" });
    const dom = await installPage();
    const { manager, ownNode } = await startApp(host);
    const reads = (chain: string) => reader(ownNode, chain.split("/"), loopbackFetch(ownNode, host.nodeHTTP));
    const height = async () => (await reads("Nexus").chainInfo()).height ?? 0n;
    const balance = async (chain: string, account: string) => (await reads(chain).account(account)).balance;

    // A wallet with two accounts: a seller and a buyer.
    button("Create wallet").click();
    input("password (min 8)").value = PASSWORD;
    input("confirm password").value = PASSWORD;
    button("Continue").click();
    button("I saved it — create").click();
    await shows(/Balance/);
    const seller = document.querySelector(".account-address")!.textContent!;
    const pick = (value: string) => {
      const picker = document.querySelector("select.account-picker") as HTMLSelectElement;
      picker.value = value;
      picker.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    };
    pick("__add");
    const buyer = await until(() => {
      const shown = document.querySelector(".account-address")?.textContent;
      return shown && shown !== seller ? shown : undefined;
    }, "a second account");

    // The node, with a private Nexus/testnet: the seller is paid the child's
    // rewards, the buyer the parent's.
    await manager.start();
    await until(async () => ((await manager.status())[0]?.state === "active" ? true : undefined), "the node up");
    await run(join(bin!, "lattice"), ["child", "create", CHILD, "--root", root, "--reward", "100000"]);
    await until(async () => ((await manager.status())[0]?.state === "active" ? true : undefined), "the node back up");
    const recipients = { Nexus: buyer, [CHILD]: seller };
    await mine(root, ownNode, recipients, 3, height);
    assert.ok(await balance(CHILD, seller) > 0n && await balance("Nexus", buyer) > 0n);

    // ---- the seller offers 5,000 on the child for 700 on the parent ----
    pick(seller);
    await until(() => (document.querySelector(".account-address")?.textContent === seller ? true : undefined), "the seller's account");
    const sell = orderURI({ side: "sell_child", orderType: "limit", amountDeposited: "5000", amountDemanded: "700" });
    // A sell order is signed on the chain it sells; on the wrong one the app says so and signs nothing.
    dropOrder(dom, sell);
    await shows(/Select Nexus\/testnet in the wallet before opening this request/);
    const openMenu = async () => { document.getElementById("net-badge")!.click(); await shows(/testnet›/); };
    await openMenu();
    ([...document.querySelectorAll(".chain-menu-item")].find((entry) => entry.firstChild?.textContent === "testnet") as HTMLButtonElement).click();
    await until(() => (document.getElementById("net-badge")!.textContent === `${CHILD} ▾` ? true : undefined), "the wallet on the child chain");
    await shows(/Balance[\d,]*[1-9]/);
    dropOrder(dom, sell);
    await shows(/Review sell order/);
    assert.match(pageText(), /You lock5,000You ask700 on Nexus/);
    const sellerChildBefore = await balance(CHILD, seller);
    button("Lock funds & create order").click();
    await until(() => (/Review sell order/.test(pageText()) ? undefined : true), "the deposit to be sent");
    await mine(root, ownNode, recipients, 2, height);
    const fetch = loopbackFetch(ownNode, host.nodeHTTP);
    const [deposit] = await until(async () => {
      const listed = await activeDeposits(ownNode, CHILD.split("/"), fetch);
      return listed.length ? listed : undefined;
    }, "the deposit to be listed");
    button("Done").click();
    await shows(/Balance/);
    document.getElementById("settings-button")!.click();
    await shows(/Settings/);
    document.getElementById("settings-button")!.click();
    await shows(/Balance/);

    // ---- the buyer takes exactly that deposit ----
    pick(buyer);
    await until(() => (document.querySelector(".account-address")?.textContent === buyer ? true : undefined), "the buyer's account");
    dropOrder(dom, orderURI({ side: "buy_child", orderType: "take", deposits: [{
      demander: deposit!.demander, amountDemanded: deposit!.amountDemanded.toString(),
      amountDeposited: deposit!.amountDeposited.toString(), depositNonce: deposit!.depositNonce.toString(),
    }] }));
    await shows(/Review purchase/);
    assert.match(pageText(), /You pay700 on NexusYou receive5,000 on Nexus\/testnet/);
    button("Pay & reserve tokens").click();
    await until(() => (/Review purchase/.test(pageText()) ? undefined : true), "the payment to be sent");
    await shows(/Complete purchase/);
    assert.match(pageText(), /Paid700 on NexusClaim5,000 on Nexus\/testnet/);
    // The receipt is not in a block yet: the withdrawal is refused, not signed blind.
    button("Check & withdraw tokens").click();
    await until(() => (/Check the parent receipt before withdrawing\.$|Checking parent receipt/.test(document.querySelector(".toast")?.textContent ?? "x") ? undefined : true), "an answer about the receipt");
    await shows(/The parent receipt is not confirmed yet/);
    await mine(root, ownNode, recipients, 2, height);

    // ---- the app is closed and reopened between paying and withdrawing ----
    document.body.replaceChildren();
    const relaunched = await installPage();
    await startApp(host);
    await shows(/Unlock/);
    input("password").value = PASSWORD;
    button("Unlock").click();
    await shows(/Balance/);
    document.getElementById("settings-button")!.click();
    await shows(/Pending purchases \(1\)/);
    button("Pending purchases (1)").click();
    await shows(/Complete/);
    (document.querySelector(".chain-menu-item") as HTMLButtonElement).click();
    await shows(/Complete purchase/);
    const buyerChildBefore = await balance(CHILD, buyer);
    button("Check & withdraw tokens").click();
    await until(() => (/Withdrawal|withdrawal submitted/i.test(pageText()) ? true : undefined), "the withdrawal to be sent");
    await mine(root, ownNode, recipients, 2, height);
    // The buyer holds the child funds less the withdrawal fee; the seller was paid on the parent.
    assert.equal(await balance(CHILD, buyer) - buyerChildBefore, 4999n);
    assert.equal(await balance("Nexus", seller), 700n);
    assert.deepEqual(await activeDeposits(ownNode, CHILD.split("/"), fetch), [], "the deposit is spent");

    // ---- settled: what each side is told, and the records the wallet keeps ----
    await mine(root, ownNode, recipients, 6, height);
    button("Back").click();
    await shows(/Pending purchases/);
    (document.querySelector(".chain-menu-item") as HTMLButtonElement).click();
    await shows(/Node reports withdrawal .* included/);
    button("Back").click();
    await shows(/Pending purchases/);
    button("Back").click();
    await shows(/Pending sales \(1\)/);
    button("Pending sales (1)").click();
    await shows(/5,000 on Nexus\/testnetfor 700 on Nexus/);
    button("Check payment").click();
    await shows(/Paid by /);
    button("Back").click();
    await shows(/Settings/);
    document.getElementById("settings-button")!.click();
    await shows(/Balance/);

    // The same deposit cannot be bought twice: the review refuses before anything is signed.
    dropOrder(relaunched, orderURI({ side: "buy_child", orderType: "take", deposits: [{
      demander: deposit!.demander, amountDemanded: deposit!.amountDemanded.toString(),
      amountDeposited: deposit!.amountDeposited.toString(), depositNonce: deposit!.depositNonce.toString(),
    }] }));
    await shows(/is no longer locked with these terms\. Nothing was paid\./);
    assert.equal(await balance("Nexus", seller), 700n);
  } catch (e) {
    try {
      console.error(readFileSync(join(root, "log", "lattice-node.log"), "utf8").slice(-4000));
    } catch {}
    throw e;
  } finally {
    document.body.replaceChildren();
    await host.ops.lattice("down");
    rmSync(root, { recursive: true, force: true });
  }
});
