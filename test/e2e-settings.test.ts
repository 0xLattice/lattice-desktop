// End to end, headless: configuring the node and mining what it hosts, from
// the desktop app's own screens, on the real `lattice` binaries with throwaway
// storage and no peers. One session: the node's settings typed wrong and then right,
// a port change that reloads the app, a child chain hosted from the Mining
// screen, both chains mined to different accounts, the node restarted under
// the miner, and a relaunch. Only the shell is replaced (see e2e-app.test.ts).
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
import { balanceShown, bin, button, freePort, harnessHost, input, installPage, pageText, press, run, shows, until, openNode } from "./helpers/real-node.ts";

const PASSWORD = "correct horse battery";
const CHILD = "Nexus/testnet";

test("the app: node settings, a hosted child chain, and mining both", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-settings-"));
  let reloads = 0;
  const host = { ...harnessHost(root), reload: () => { reloads += 1; } };
  // On the processor here, whatever this machine has; e2e-mining covers the graphics chip.
  host.ops.gpuWorker = async () => null;
  const topology = () => JSON.parse(readFileSync(join(root, "lattice.json"), "utf8")) as Topology;
  try {
    const setup = new NodeManager(host.ops);
    await setup.configure({ ...(await setup.prefs()), rpc: await freePort(), listen: await freePort(), peers: "none" });
    const dom = await installPage();
    let app = await startApp(host);
    const reads = (chain: string) => reader(app.ownNode, chain.split("/"), loopbackFetch(app.ownNode, host.nodeHTTP));
    const balance = async (chain: string, account: string) => (await reads(chain).account(account)).balance;

    button("Create wallet").click();
    input("password (min 8)").value = PASSWORD;
    input("confirm password").value = PASSWORD;
    button("Continue").click();
    button("I saved it — create").click();
    await shows(/Balance/);
    const first = document.querySelector(".account-address")!.textContent!;
    const picker = document.querySelector("select.account-picker") as HTMLSelectElement;
    picker.value = "__add";
    picker.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    const second = await until(() => {
      const shown = document.querySelector(".account-address")?.textContent;
      return shown && shown !== first ? shown : undefined;
    }, "a second account");

    // ---- the node's settings, typed wrong: each is refused with its reason and nothing is written ----
    openNode();
    await shows(/Your node/);
    await press("Start node");
    await shows(/Your nodeRunning/);
    await shows(/Local API port/);
    const before = readFileSync(join(root, "lattice.json"), "utf8");
    const rpc = () => document.querySelector('input[aria-label="Local API port"]') as HTMLInputElement;
    const listen = () => document.querySelector('input[aria-label="Peer port"]') as HTMLInputElement;
    // Whom a chain connects to, the chains under it and the way to stop hosting it are on the chain's own page.
    const peerMode = (chain = "Nexus") => document.querySelector(`select[aria-label="Peers of ${chain}"]`) as HTMLSelectElement;
    const peerList = (chain = "Nexus") => document.querySelector(`textarea[aria-label="Peer list of ${chain}"]`) as HTMLTextAreaElement;
    const chainRow = (chain: string) => document.querySelector(`button[data-chain="${chain}"]`) as HTMLButtonElement | null;
    const openChain = async (chain: string) => {
      await until(() => (chainRow(chain) ? true : undefined), `the row of ${chain}`);
      chainRow(chain)!.click();
      await until(() => (peerMode(chain) ? true : undefined), `the page of ${chain}`);
    };
    const toNodePage = async () => { button("Your node").click(); await shows(/Local API port/); };
    // A chain stops being hosted from its page, after being asked once.
    const stopHosting = async (chain: string) => {
      await openChain(chain);
      button(`Stop hosting ${chain.split("/").pop()}`).click();
      (document.querySelector(`button[aria-label="Stop hosting ${chain}"]`) as HTMLButtonElement).click();
    };
    // A chain is hosted by its name, typed on the page of the chain it is under.
    const childName = (parent: string) => document.querySelector(`input[aria-label="Name of a chain under ${parent}"]`) as HTMLInputElement;
    const [oldRPC, oldListen] = [rpc().value, listen().value];
    rpc().value = "0";
    button("Save").click();
    await shows(/Ports must be whole numbers from 1 to 65535/);
    rpc().value = oldListen;
    button("Save").click();
    await shows(/The RPC and peer ports must differ/);
    rpc().value = oldRPC;
    await openChain("Nexus");
    assert.equal(peerMode().value, "none");
    peerMode().value = "custom";
    peerList().value = "not-a-peer";
    button("Save peers").click();
    await shows(/Not a peer \(publicKey@host:port\): not-a-peer/);
    peerMode().value = "none";
    // A name is all there is to type: where the chain goes is the page it is typed on.
    childName("Nexus").value = "testnet/payments";
    button("Add").click();
    await shows(/Type the chain's name: letters, digits, - and _\./);
    assert.equal(readFileSync(join(root, "lattice.json"), "utf8"), before);

    // ---- typed right: a new RPC port and a peer of the user's own; the app reloads onto the new port ----
    const newRPC = String(await freePort());
    const peer = `${"ab".repeat(32)}@127.0.0.1:${await freePort()}`;
    childName("Nexus").value = "";
    // A chain's peers are saved on its own page; the ports on the node's, under Advanced.
    peerMode().value = "custom";
    peerList().value = peer;
    button("Save peers").click();
    await until(() => (topology().peers?.length === 1 ? true : undefined), "the peer saved");
    await until(() => (peerList()?.value === peer ? true : undefined), "the saved peer shown");
    await toNodePage();
    await shows(/Your nodeRunning/);
    rpc().value = newRPC;
    button("Save").click();
    await until(() => (reloads === 1 ? true : undefined), "the app to reload for the new RPC port");
    assert.deepEqual([topology().rpc, topology().peers], [Number(newRPC), [peer]]);
    document.body.replaceChildren();
    await installPage();
    app = await startApp(host);
    assert.equal(app.ownNode, `http://127.0.0.1:${newRPC}`);
    await shows(/Unlock/);
    input("password").value = PASSWORD;
    button("Unlock").click();
    // The running node was restarted onto the new port, and the wallet reads through it.
    await until(() => {
      if (balanceShown() === "0") return true;
      if (/Balance/.test(pageText())) button("Refresh balance").click();
      return undefined;
    }, "a balance read on the new port");
    openNode();
    await shows(/Your nodeRunning/);
    await shows(/Local API port/);
    assert.equal(rpc().value, newRPC);
    await openChain("Nexus");
    assert.deepEqual([peerMode().value, peerList().value], ["custom", peer]);
    // Back to a private node: no peers.
    peerMode().value = "none";
    button("Save peers").click();
    await until(() => (topology().peers?.length === 0 ? true : undefined), "the peers saved");
    assert.deepEqual(topology().peers, []);
    assert.equal(reloads, 1, "only a port change reloads");
    await until(() => (peerMode()?.value === "none" ? true : undefined), "the saved choice shown");
    await toNodePage();
    await shows(/Your nodeRunning/);

    // ---- a child chain exists (made with the CLI), and mining is set up while only Nexus is hosted ----
    await run(join(bin!, "lattice"), ["child", "create", CHILD, "--root", root, "--reward", "100000"]);
    await until(async () => ((await app.manager.status())[0]?.state === "active" ? true : undefined), "the node back up");
    // The chain was made outside the app: the page lists it when next opened.
    button("Back to wallet").click();
    await shows(/Balance/);
    openNode();
    await stopHosting(CHILD);
    await until(() => (topology().hostedChains === undefined ? true : undefined), "the child no longer hosted");
    await shows(/Your nodeRunning/);
    button("Back to wallet").click();
    await shows(/Balance/);
    button("Mining").click();
    await shows(/Chains mined are the chains hosted: Nexus\./);
    const recipient = (chain: string) => document.querySelector(`select[aria-label="Recipient on ${chain}"]`) as HTMLSelectElement | null;
    assert.equal(recipient(CHILD), null);

    // ---- host the child from the Mining screen's link; it comes back with a recipient to choose ----
    button("Choose hosted chains").click();
    await shows(/Your nodeRunning/);
    // Its first block is not mined yet, so the node cannot list it: its name is typed, on Nexus's page.
    assert.equal(chainRow(CHILD), null);
    await openChain("Nexus");
    childName("Nexus").value = "testnet";
    button("Add").click();
    await shows(/Chains mined are the chains hosted: Nexus, Nexus\/testnet\./);
    assert.deepEqual(topology().hostedChains, [CHILD]);
    // Each chain pays the active account unless told otherwise.
    assert.deepEqual([recipient("Nexus")!.value, recipient(CHILD)!.value], [second, second]);

    // ---- mine both: the child pays the first account, Nexus pays no one ----
    recipient("Nexus")!.value = "";
    recipient("Nexus")!.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
    recipient(CHILD)!.value = first;
    // Paying no one is allowed, and said out loud before it happens.
    await shows(/Nexus will pay no one: every reward and fee mined there is destroyed\./);
    button("Start mining").click();
    await shows(/Statusmining on the (graphics chip|processor)/);
    assert.deepEqual(topology().mine?.recipients, { [CHILD]: first });
    await until(async () => ((await balance(CHILD, first)) > 0n ? true : undefined), "child-chain rewards for the first account");
    const nexusHeight = async () => (await reads("Nexus").chainInfo()).height ?? 0n;
    assert.ok((await nexusHeight()) > 0n);
    assert.deepEqual([await balance("Nexus", first), await balance("Nexus", second)], [0n, 0n], "Nexus blocks were mined to no one");

    // ---- now Nexus pays the second account: saved while mining, the loop restarts with it ----
    recipient("Nexus")!.value = second;
    button("Apply changes").click();
    await until(() => (topology().mine?.recipients?.Nexus === second ? true : undefined), "the Nexus recipient saved");
    await until(async () => ((await balance("Nexus", second)) > 0n ? true : undefined), "Nexus rewards for the second account");
    assert.equal(await balance(CHILD, second), 0n);

    // ---- the node restarts under the miner (a settings change): mining carries on ----
    button("Back").click();
    await shows(/Balance/);
    openNode();
    await shows(/Local API port/);
    // The child is a row under Nexus, named by the last part of its path.
    await until(() => (chainRow(CHILD) ? true : undefined), "the child chain's row");
    assert.equal(chainRow(CHILD)!.querySelector(".name")!.textContent, "└ testnet");
    await openChain("Nexus");
    peerMode().value = "custom";
    peerList().value = peer;
    button("Save peers").click();
    await until(() => (topology().peers?.length === 1 ? true : undefined), "the peer saved");
    assert.deepEqual(topology().peers, [peer]);
    await until(() => (peerList()?.value === peer ? true : undefined), "the saved peer shown");
    // The child chain has its own page and its own peers: found through Nexus
    // unless it is given more to try. It is one press from its parent's page.
    await openChain(CHILD);
    await shows(/Your node\/Nexus\/testnet/);
    const childMode = () => peerMode(CHILD);
    const childList = () => peerList(CHILD);
    assert.deepEqual([...childMode().options].map((option) => option.textContent), ["Peers found through Nexus", "Those, and peers I list"]);
    assert.equal(childMode().value, "default");
    childMode().value = "custom";
    childList().value = "not-a-peer";
    (document.querySelector(`button[aria-label="Save peers of ${CHILD}"]`) as HTMLButtonElement).click();
    await shows(/Not a peer \(publicKey@host:port\): not-a-peer/);
    childList().value = peer;
    (document.querySelector(`button[aria-label="Save peers of ${CHILD}"]`) as HTMLButtonElement).click();
    await until(() => (Array.isArray(topology().peers) ? undefined : true), "the child chain's peer saved");
    assert.deepEqual(topology().peers, { Nexus: [peer], [CHILD]: [peer] });
    // The node starts on it, and the row shows what was saved.
    await until(() => (childMode()?.value === "custom" && childList().value === peer ? true : undefined), "the child chain's peers shown");
    await toNodePage();
    button("Back to wallet").click();
    await shows(/Balance/);
    button("Mining").click();
    await shows(/Statusmining on the (graphics chip|processor)/);
    const afterRestart = await until(async () => nexusHeight(), "the node back up");
    await until(async () => ((await nexusHeight()) > afterRestart + 1n ? true : undefined), "blocks mined after the restart");

    // ---- stop hosting the child while mining still names a recipient for it ----
    button("Choose hosted chains").click();
    await stopHosting(CHILD);
    // The node will not start on a lattice.json whose mining names a chain it
    // does not host, so the chain stops being paid when it stops being hosted.
    await shows(/Chains mined are the chains hosted: Nexus\./);
    await shows(/Statusmining on the (graphics chip|processor)/);
    assert.deepEqual(topology().mine?.recipients, { Nexus: second });
    assert.equal(topology().hostedChains, undefined);
    assert.deepEqual(topology().peers, [peer], "a chain no longer hosted takes its peers with it");
    assert.equal(recipient(CHILD), null);
    assert.deepEqual((await app.manager.status()).map((chain) => chain.path), ["Nexus"]);
    const unhostedAt = await until(async () => nexusHeight(), "the node back up");
    await until(async () => ((await nexusHeight()) > unhostedAt + 1n ? true : undefined), "Nexus still mined after the child was dropped");

    // ---- relaunch: the next session shows what this one saved, and mining is still running ----
    document.body.replaceChildren();
    await installPage();
    app = await startApp(host);
    await shows(/Unlock/);
    input("password").value = PASSWORD;
    button("Unlock").click();
    await shows(/Balance/);
    openNode();
    await shows(/Your nodeRunning/);
    await shows(/Local API port/);
    assert.deepEqual([rpc().value, chainRow(CHILD) !== null], [newRPC, false]);
    await openChain("Nexus");
    assert.deepEqual([peerMode().value, peerList().value], ["custom", peer]);
    await toNodePage();
    button("Back to wallet").click();
    await shows(/Balance/);
    button("Mining").click();
    await shows(/Statusmining on the (graphics chip|processor)/);
    assert.equal(recipient("Nexus")!.value, second);
    assert.equal((document.querySelector('select[aria-label="Search with"]') as HTMLSelectElement).value, "cpu");
    button("Stop mining").click();
    await shows(/Statusnot mining/);
    button("Back").click();
    await shows(/Balance/);
    openNode();
    await press("Stop node");
    await shows(/Your nodeStopped/);
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
