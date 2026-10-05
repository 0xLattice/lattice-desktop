// The webview's only network path: the shell's loopback node_http, adapted
// to the SDK's fetch, serving the app's own node and nothing else.

import { test } from "node:test";
import assert from "node:assert/strict";
import { loopbackFetch, type NodeHTTP } from "../src/loopback.ts";
import { reader, submitter } from "../vendor/nexus-wallet/src/lib/wallet/node.ts";
import { statusLine, parsePeersField } from "../src/node-panel.ts";

const BASE = "http://127.0.0.1:18080";

test("SDK reads and submits travel through node_http as path + query", async () => {
  const calls: [string, string, string | null][] = [];
  const call: NodeHTTP = async (method, path, body) => {
    calls.push([method, path, body]);
    if (path.startsWith("/api/state/account/")) return { status: 200, body: JSON.stringify({ owner: "bafyalice", balance: "42", nonce: "7" }) };
    if (path === "/transactions") return { status: 200, body: JSON.stringify({ transactionCID: "bafytx" }) };
    return { status: 404, body: "{}" };
  };
  const fetch = loopbackFetch(BASE, call);
  const account = await reader(BASE, ["Nexus"], fetch).account("bafyalice");
  assert.equal(account.balance, 42n);
  assert.deepEqual(calls[0], ["GET", "/api/state/account/bafyalice?chainPath=Nexus", null]);
  const sent = await submitter(BASE, fetch).submit({ transaction: { signatures: {}, body: {} } } as never);
  assert.equal(sent.transactionCID, "bafytx");
  assert.equal(calls[1]![0], "POST");
  assert.equal(calls[1]![1], "/transactions");
});

test("anything but the app's own node is refused before the shell is asked", async () => {
  let asked = 0;
  const fetch = loopbackFetch(BASE, async () => { asked += 1; return { status: 200, body: "{}" }; });
  for (const url of ["http://127.0.0.1:8080/health", "https://reads.example.org/health", "http://localhost:18080/health"]) {
    await assert.rejects(fetch(url), /not this app's node/);
  }
  await assert.rejects(fetch(`${BASE}/health`, { method: "DELETE" }), /refused: DELETE/);
  assert.equal(asked, 0);
});

test("node panel wording", () => {
  assert.equal(statusLine({ path: "Nexus", state: "down" }), "stopped");
  assert.equal(statusLine({ path: "Nexus", state: "active", height: 1234n, mempool: 2 }), `active · height ${(1234).toLocaleString()} · 2 pending`);
  assert.equal(parsePeersField("default", "x"), "default");
  assert.equal(parsePeersField("none", ""), "none");
  assert.deepEqual(parsePeersField("custom", " a@h:1\n b@h:2, "), ["a@h:1", "b@h:2"]);
});
