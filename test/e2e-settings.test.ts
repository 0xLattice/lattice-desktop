// End to end, headless: configuring the node and mining what it hosts, from
// the desktop app's own screens, on the real `lattice` binaries with throwaway
// storage and no peers. One session: Node settings typed wrong and then right,
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
import { bin, button, freePort, harnessHost, input, installPage, pageText, run, shows, until } from "./helpers/real-node.ts";

const PASSWORD = "correct horse battery";
const CHILD = "Nexus/testnet";

test("the app: node settings, a hosted child chain, and mining both", { skip: !bin, timeout: 600_000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), "lattice-desktop-settings-"));
  let reloads = 0;
  const host = { ...harnessHost(root), reload: () => { reloads += 1; } };
  // On the processor here, whatever this machine has; e2e-mining covers the graphics chip.
  host.ops.gpuWorker = async () => null;
  const topology = () => JSON.parse(readFileSync(join(root, "lattice.json"), "utf8")) as Topology;
  const field = (label: string) => {
    const labels = [...document.querySelectorAll("label")];
    const found = labels.find((item) => item.textContent === label)?.nextElementSibling;
    assert.ok(found, `field "${label}" exists; page was: ${pageText()}`);
    return found as HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement;
  };
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

    // ---- Node settings, typed wrong: each is refused with its reason and nothing is written ----
    button("Node").click();
    await shows(/Your node/);
    button("Start").click();
    await shows(/Nexusactive/);
    button("Node settings").click();
    await shows(/Hosted child chains/);
    const before = readFileSync(join(root, "lattice.json"), "utf8");
    const rpc = () => field("RPC port (loopback only)") as HTMLInputElement;
    const listen = () => field("Peer port") as HTMLInputElement;
    const peerMode = () => field("Nexus bootstrap peers") as HTMLSelectElement;
    const peerList = () => document.querySelector('textarea[placeholder^="publicKey@host:port"]') as HTMLTextAreaElement;
    const hosted = () => field("Hosted child chains") as HTMLTextAreaElement;
    const [oldRPC, oldListen] = [rpc().value, listen().value];
    assert.equal(peerMode().value, "none");
    rpc().value = "0";
    button("Save").click();
    await shows(/Ports must be whole numbers from 1 to 65535/);
    rpc().value = oldListen;
    button("Save").click();
    await shows(/The RPC and peer ports must differ/);
    rpc().value = oldRPC;
    peerMode().value = "custom";
    peerList().value = "not-a-peer";
    button("Save").click();
    await shows(/Not a peer \(publicKey@host:port\): not-a-peer/);
    peerMode().value = "none";
    hosted().value = "Nexus/testnet/payments";
    button("Save").click();
    await shows(/must be a new Nexus-rooted path listed after its parent/);
    assert.equal(readFileSync(join(root, "lattice.json"), "utf8"), before);

    // ---- typed right: a new RPC port and a peer of the user's own; the app reloads onto the new port ----
    const newRPC = String(await freePort());
    const peer = `${"ab".repeat(32)}@127.0.0.1:${await freePort()}`;
    hosted().value = "";
    rpc().value = newRPC;
    peerMode().value = "custom";
    peerList().value = peer;
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
      if (/Balance0/.test(pageText())) return true;
      if (/Balance/.test(pageText())) button("Refresh balance").click();
      return undefined;
    }, "a balance read on the new port");
    button("Node").click();
    await shows(/Nexusactive/);
    button("Node settings").click();
    await shows(/Hosted child chains/);
    assert.deepEqual([rpc().value, peerMode().value, peerList().value], [newRPC, "custom", peer]);
    // Back to a private node: no peers.
    peerMode().value = "none";
    button("Save").click();
    await shows(/Your node/);
    assert.deepEqual(topology().peers, []);
    assert.equal(reloads, 1, "only a port change reloads");
    await shows(/Nexusactive/);

    // ---- a child chain exists (made with the CLI), and mining is set up while only Nexus is hosted ----
    await run(join(bin!, "lattice"), ["child", "create", CHILD, "--root", root, "--reward", "100000"]);
    await until(async () => ((await app.manager.status())[0]?.state === "active" ? true : undefined), "the node back up");
    button("Node settings").click();
    await shows(/Hosted child chains/);
    assert.equal(hosted().value, CHILD);
    hosted().value = "";
    button("Save").click();
    await shows(/Your node/);
    await shows(/Nexusactive/);
    button("Mining").click();
    await shows(/Chains mined are the chains hosted: Nexus\./);
    const recipient = (chain: string) => document.querySelector(`select[aria-label="Recipient on ${chain}"]`) as HTMLSelectElement | null;
    assert.equal(recipient(CHILD), null);

    // ---- host the child from the Mining screen's link; it comes back with a recipient to choose ----
    button("Choose hosted chains").click();
    await shows(/Hosted child chains/);
    hosted().value = CHILD;
    button("Save").click();
    await shows(/Chains mined are the chains hosted: Nexus, Nexus\/testnet\./);
    assert.deepEqual(topology().hostedChains, [CHILD]);
    // Each chain pays the active account unless told otherwise.
    assert.deepEqual([recipient("Nexus")!.value, recipient(CHILD)!.value], [second, second]);

    // ---- mine both: the child pays the first account, Nexus pays no one ----
    recipient("Nexus")!.value = "";
    recipient(CHILD)!.value = first;
    button("Start mining").click();
    await shows(/Statusmining \(pid \d+\)/);
    assert.deepEqual(topology().mine?.recipients, { [CHILD]: first });
    await until(async () => ((await balance(CHILD, first)) > 0n ? true : undefined), "child-chain rewards for the first account");
    const nexusHeight = async () => (await reads("Nexus").chainInfo()).height ?? 0n;
    assert.ok((await nexusHeight()) > 0n);
    assert.deepEqual([await balance("Nexus", first), await balance("Nexus", second)], [0n, 0n], "Nexus blocks were mined to no one");

    // ---- now Nexus pays the second account: saved while mining, the loop restarts with it ----
    recipient("Nexus")!.value = second;
    button("Save").click();
    await until(() => (topology().mine?.recipients?.Nexus === second ? true : undefined), "the Nexus recipient saved");
    await until(async () => ((await balance("Nexus", second)) > 0n ? true : undefined), "Nexus rewards for the second account");
    assert.equal(await balance(CHILD, second), 0n);

    // ---- the node restarts under the miner (a settings change): mining carries on ----
    button("Choose hosted chains").click();
    await shows(/Hosted child chains/);
    peerMode().value = "custom";
    peerList().value = peer;
    button("Save").click();
    await shows(/Statusmining \(pid \d+\)/);
    assert.deepEqual(topology().peers, [peer]);
    const afterRestart = await until(async () => nexusHeight(), "the node back up");
    await until(async () => ((await nexusHeight()) > afterRestart + 1n ? true : undefined), "blocks mined after the restart");

    // ---- stop hosting the child while mining still names a recipient for it ----
    button("Choose hosted chains").click();
    await shows(/Hosted child chains/);
    hosted().value = "";
    button("Save").click();
    // The node will not start on a lattice.json whose mining names a chain it
    // does not host, so the chain stops being paid when it stops being hosted.
    await shows(/Chains mined are the chains hosted: Nexus\./);
    await shows(/Statusmining \(pid \d+\)/);
    assert.deepEqual(topology().mine?.recipients, { Nexus: second });
    assert.equal(topology().hostedChains, undefined);
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
    button("Node").click();
    await shows(/Nexusactive/);
    button("Node settings").click();
    await shows(/Hosted child chains/);
    assert.deepEqual([rpc().value, peerMode().value, peerList().value, hosted().value], [newRPC, "custom", peer, ""]);
    button("Back").click();
    await shows(/Your node/);
    button("Mining").click();
    await shows(/Statusmining \(pid \d+\)/);
    assert.equal(recipient("Nexus")!.value, second);
    assert.equal((document.querySelector('select[aria-label="Search with"]') as HTMLSelectElement).value, "cpu");
    button("Stop mining").click();
    await shows(/Statusnot mining/);
    button("Back").click();
    await shows(/Your node/);
    button("Stop").click();
    await shows(/Nexusstopped/);
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
