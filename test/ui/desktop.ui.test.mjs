// The app itself, in its own window, driven as a person drives it: the
// packaged executable, its real shell, the bundled CLI and node, and real
// clicks and keys through WebDriver. The other end-to-end tests run the page
// against a stand-in for the shell; this one has no stand-in anywhere.
//
// It walks the things the app is for, and the mistakes people make on the
// way: making a wallet (a short password, two that differ), the node the app
// starts, sending (a bad address, one's own, a bad amount, more than there
// is), mining (asked for with the node stopped, then for real), a transfer
// seen through to a confirmation, locking (a wrong password), node settings
// typed wrong, and a relaunch that finds everything as it was left.
//
//   LATTICE_UI_APP=<the app's executable> node --test test/ui/desktop.ui.test.mjs
//
// It needs tauri-driver on the PATH (`cargo install tauri-driver --locked`)
// and the platform's webview driver: WebKitWebDriver on Linux (the
// webkit2gtk-driver package), msedgedriver on Windows, named in
// LATTICE_UI_NATIVE_DRIVER. macOS has no driver for its webview.
//
// The node must join no network: the app is given a host root with no peers.
// Off Windows the test makes one itself, in a throwaway home directory, with
// the CLI in LATTICE_APP_BINARIES (beside the app unless said). On Windows the
// host root is inside WSL, so whoever runs the test prepares it and the test
// uses the machine's own data directories: it is for a CI runner there.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { promisify } from "node:util";
import { Session } from "./webdriver.mjs";

const app = process.env.LATTICE_UI_APP;
const run = promisify(execFile);
const windows = process.platform === "win32";
const PASSWORD = "correct horse battery";
const DRIVER = "http://127.0.0.1:4444";

const freePort = () => new Promise((done, fail) => {
  const server = createServer();
  server.once("error", fail);
  server.listen(0, "127.0.0.1", () => {
    const { port } = server.address();
    server.close(() => done(port));
  });
});

async function until(probe, what, ms = 120_000) {
  const deadline = Date.now() + ms;
  let last;
  for (;;) {
    try {
      const value = await probe();
      if (value !== undefined && value !== null && value !== false) return value;
    } catch (e) {
      last = e;
    }
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${typeof what === "string" ? what : await what()}${last ? `: ${last}` : ""}`);
    await new Promise((wake) => setTimeout(wake, 300));
  }
}

test("the app in its own window: a wallet, its node, mining, a transfer, and the mistakes on the way", { skip: !app, timeout: 1_500_000 }, async () => {
  // ---- a private node for the app to find, and somewhere to keep its data ----
  const home = windows ? undefined : mkdtempSync(join(tmpdir(), "lattice-ui-home-"));
  let root;
  if (home) {
    const binaries = resolve(process.env.LATTICE_APP_BINARIES ?? dirname(resolve(app)));
    root = join(home, ".local", "share", "Lattice", "host");
    await run(join(binaries, "lattice"), ["init", "--root", root]);
    const file = join(root, "lattice.json");
    writeFileSync(file, JSON.stringify({ ...JSON.parse(readFileSync(file, "utf8")), rpc: await freePort(), listen: await freePort(), peers: [] }, null, 2));
  }
  const driver = spawn("tauri-driver", process.env.LATTICE_UI_NATIVE_DRIVER ? ["--native-driver", process.env.LATTICE_UI_NATIVE_DRIVER] : [], {
    stdio: "inherit",
    env: { ...process.env, ...(home ? { HOME: home, XDG_DATA_HOME: "", XDG_CONFIG_HOME: "", XDG_CACHE_HOME: "" } : {}) },
  });
  await until(async () => (await fetch(`${DRIVER}/status`)).ok, "tauri-driver to listen", 30_000);

  let session;
  const open = async () => {
    session = await Session.start(DRIVER, resolve(app));
  };
  // ---- the page, as a person reads and works it ----
  const pageText = () => session.run("return (document.body.textContent || '').replace(/\\s+/g, ' ').trim()");
  const shows = (pattern, ms = 90_000) => until(async () => pattern.test(await pageText()), async () => `the page to show ${pattern}; it shows: ${(await pageText()).slice(0, 700)}`, ms);
  const element = (script, ...args) => until(() => session.run(script, ...args), async () => `${args.join(" ")} on the page; it shows: ${(await pageText()).slice(0, 500)}`, 60_000);
  /** Clicks the button with this text or accessible name, once the screen offers it. */
  const press = async (text) => session.click(await element("return [...document.querySelectorAll('button')].find((b) => !b.disabled && (b.textContent === arguments[0] || b.getAttribute('aria-label') === arguments[0])) || null", text));
  /** Opens a folded section by clicking its heading. */
  const unfold = async (heading) => session.click(await element("return [...document.querySelectorAll('summary')].find((s) => s.textContent === arguments[0]) || null", heading));
  const field = (selector) => element("return document.querySelector(arguments[0])", selector);
  /** The node page, from the wallet's home screen: it is in Settings. */
  const openNode = async () => { await session.click(await field("#settings-button")); await press("Node"); };
  const type = async (selector, text) => session.type(await field(selector), text);
  const valueOf = (selector) => session.run("return document.querySelector(arguments[0])?.value ?? null", selector);
  const textOf = (selector) => session.run("return document.querySelector(arguments[0])?.textContent ?? null", selector);
  /** Chooses an option of a drop-down, as picking it from the list does. */
  const choose = (selector, value) => session.run("const s = document.querySelector(arguments[0]); s.value = arguments[1]; s.dispatchEvent(new Event('change', { bubbles: true })); return true", selector, value);
  const balance = () => textOf(".balance-value .v");
  const funded = async () => {
    if (/^[\d,]*[1-9][\d,]*$/.test((await balance()) ?? "")) return true;
    if (/Balance/.test(await pageText())) await press("Refresh balance");
    return false;
  };
  /** The transfer listed with a confirmation: the list is read when asked, so it is asked until then. */
  const confirmed = () => until(async () => {
    if (/Sent1,?000Nexus\d+ confirmation/.test(await pageText())) return true;
    await press("Refresh transactions");
    return false;
  }, async () => `the transfer confirmed; the page shows: ${(await pageText()).slice(0, 400)}`, 300_000);
  const RECIPIENT = 'input[placeholder="recipient address (bafy…)"]';
  const AMOUNT = 'input[aria-label="Amount"]';

  try {
    await open();

    // ---- a wallet: a password too short, two that differ, then one that will do ----
    await press("Create wallet");
    await type('input[placeholder="password (min 8)"]', "short");
    await type('input[placeholder="confirm password"]', "short");
    await press("Continue");
    await shows(/Password must be at least 8 characters\./);
    await type('input[placeholder="password (min 8)"]', PASSWORD);
    await type('input[placeholder="confirm password"]', "correct horse batter");
    await press("Continue");
    await shows(/Passwords do not match\./);
    await type('input[placeholder="confirm password"]', PASSWORD);
    await press("Continue");
    const phrase = await until(() => textOf(".mnemonic"), "a recovery phrase");
    assert.equal(phrase.trim().split(/\s+/).length, 12, "twelve words to write down");
    await press("I saved it — create");
    await shows(/Balance on Nexus/);
    const first = await textOf(".account-address");

    // ---- the node: no one started it; the app did, and the wallet reads it ----
    await until(async () => {
      if ((await balance()) === "0") return true;
      await press("Refresh balance");
      return false;
    }, "a balance read from the node the app started", 300_000);
    await shows(/Your node is set to connect to no one, so it sees only its own blocks/);

    // ---- a second account, which mining will pay and the transfer will leave ----
    await choose("select.account-picker", "__add");
    const second = await until(async () => {
      const shown = await textOf(".account-address");
      return shown && shown !== first ? shown : undefined;
    }, "a second account");

    // ---- sending, wrong four ways, with nothing sent ----
    await press("Send");
    await shows(/Review/);
    await type(RECIPIENT, "not an address");
    await type(AMOUNT, "1000");
    await press("Review");
    await shows(/Enter a valid recipient address\./);
    await type(RECIPIENT, second);
    await press("Review");
    await shows(/That is this account\./);
    await type(RECIPIENT, first);
    await type(AMOUNT, "1.5");
    await press("Review");
    await shows(/Enter a whole, positive amount\./);
    await type(AMOUNT, "1000");
    await press("Review");
    await shows(/Insufficient balance \(have 0, need [\d,]+\)\./);
    await press("Cancel");
    await shows(/Balance on Nexus/);

    // ---- the node stopped: the wallet says so, and mining will not start ----
    await openNode();
    await shows(/Your node/);
    await shows(/Your nodeRunning/);
    await press("Stop node");
    await shows(/Your nodeStopped/);
    await press("Back to wallet");
    await shows(/Your node is stopped, so nothing can be read or sent\./);
    await press("Mining");
    await shows(/Mine on your node/);
    await shows(/Statusnot mining/);
    // Unless told otherwise, mining pays the account in use.
    assert.equal(await valueOf('select[aria-label="Recipient on Nexus"]'), second);
    await press("Start mining");
    await shows(/Start the node first: mining runs on it/);
    await press("Back");
    await shows(/Balance on Nexus/);
    await openNode();
    await press("Start node");
    await shows(/Your nodeRunning/, 300_000);

    // ---- mining: blocks found, and the wallet paid, with no other miner ----
    await press("Back to wallet");
    await press("Mining");
    await shows(/Statusnot mining/);
    await press("Start mining");
    await shows(/Statusmining on the (graphics chip|processor)/);
    console.log(`[desktop ui] ${(/Statusmining on the (graphics chip|processor)/.exec(await pageText()) ?? [""])[0].slice(6)}`);
    await shows(/This run[\d,]+ blocks? found · last at /, 300_000);
    await press("Back");
    await until(funded, "mined funds in the wallet", 300_000);
    await shows(/Mining on this computer\./);

    // ---- a transfer, from the review to a confirmation ----
    await press("Send");
    await shows(/Review/);
    await type(RECIPIENT, first);
    await type(AMOUNT, "1000");
    await press("Review");
    await shows(/Sign & send/);
    // Grouped as the system's language groups digits, or not at all.
    assert.match(await pageText(), /Amount1,?000 on Nexus/);
    await press("Sign & send");
    await shows(/Admitted to the node's pool/);
    await press("Done");
    await shows(/Balance on Nexus/);
    await session.click(await field("#settings-button"));
    await press("Transactions");
    // Mining is still on, so the transfer is mined and counted.
    await confirmed();
    await press("Back");

    // ---- locked: a wrong password is refused, the right one lets back in ----
    await press("Lock wallet");
    await shows(/Unlock/);
    await type('input[placeholder="password"]', "not the password");
    await press("Unlock");
    await shows(/Wrong password/);
    await type('input[placeholder="password"]', PASSWORD);
    await press("Unlock");
    await shows(/Balance on Nexus/);

    // ---- mining off, from the screen that turned it on ----
    await press("Mining");
    await shows(/Statusmining/);
    await press("Stop mining");
    await shows(/Statusnot mining/);
    await press("Back");
    await shows(/Balance on Nexus/);
    await openNode();

    // ---- node settings typed wrong: refused, and nothing changes ----
    await shows(/Your node/);
    const RPC = 'input[aria-label="Local API port"]';
    // Ports are folded away until asked for.
    await unfold("Advanced");
    const [rpc, listen] = [await valueOf(RPC), await valueOf('input[aria-label="Peer port"]')];
    await type(RPC, "0");
    await press("Save");
    await shows(/Ports must be whole numbers from 1 to 65535/);
    await type(RPC, listen);
    await press("Save");
    await shows(/The RPC and peer ports must differ/);
    // Leave and come back: what was refused was never kept.
    await press("Back to wallet");
    await shows(/Balance on Nexus/);
    await openNode();
    await shows(/Your node/);
    assert.equal(await valueOf(RPC), rpc, "the port is what it was");
    await press("Back to wallet");
    await shows(/Balance on Nexus/);

    // ---- the app closed and opened again: locked, and everything as it was left ----
    await session.end();
    await open();
    await shows(/Unlock/);
    await type('input[placeholder="password"]', PASSWORD);
    await press("Unlock");
    await shows(/Balance on Nexus/);
    assert.equal(await textOf(".account-address"), second, "the account in use is the one it was");
    // Closing the window this way stops no node; the app finds it running.
    await until(funded, "the balance, read from the node still running", 300_000);
    await session.click(await field("#settings-button"));
    await press("Transactions");
    await confirmed();
    await press("Back");
    await session.click(await field("#settings-button"));
    await shows(/Balance on Nexus/);

    // ---- and the node stopped from its own screen ----
    await openNode();
    await shows(/Your nodeRunning/);
    await press("Stop node");
    await shows(/Your nodeStopped/);
  } catch (e) {
    const shown = session ? await pageText().catch(() => "(the page could not be read)") : "(no window)";
    console.error(`[desktop ui] failed with the page showing: ${shown.slice(0, 1500)}`);
    if (root) {
      for (const log of ["lattice-node.log", "mine.log"]) {
        try {
          console.error(`[desktop ui] ${log}:\n${readFileSync(join(root, "log", log), "utf8").slice(-2500)}`);
        } catch {}
      }
    }
    throw e;
  } finally {
    await session?.end();
    driver.kill();
    if (home) {
      const lattice = join(resolve(process.env.LATTICE_APP_BINARIES ?? dirname(resolve(app))), "lattice");
      await run(lattice, ["mine", "stop", "--root", root]).catch(() => {});
      await run(lattice, ["down", "--root", root]).catch(() => {});
      rmSync(home, { recursive: true, force: true });
    }
  }
});
