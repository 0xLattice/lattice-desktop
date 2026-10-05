// Node HTTP for the webview. The page has no network reach of its own (its
// CSP allows only Tauri IPC): every node call goes through the shell's
// `node_http` command, which dials only 127.0.0.1 on the RPC port in
// lattice.json. This adapts that command to the SDK's fetch.

import type { Fetch } from "../vendor/nexus-wallet/vendor/lattice-sdk/packages/client/src/index.ts";

export interface LoopbackAnswer {
  status: number;
  body: string;
}

export type NodeHTTP = (method: "GET" | "POST", path: string, body: string | null) => Promise<LoopbackAnswer>;

/** A fetch that serves only `base` (the node's loopback URL), through `call`. */
export function loopbackFetch(base: string, call: NodeHTTP): Fetch {
  const origin = new URL(base).origin;
  return async (input, init) => {
    const url = new URL(input);
    if (url.origin !== origin) throw new TypeError(`refused: ${url.origin} is not this app's node`);
    const method = (init?.method ?? "GET").toUpperCase();
    if (method !== "GET" && method !== "POST") throw new TypeError(`refused: ${method}`);
    const body = typeof init?.body === "string" ? init.body : null;
    init?.signal?.throwIfAborted();
    const answer = await call(method, url.pathname + url.search, body);
    // 1xx/204/304 cannot carry a body in a Response.
    const empty = answer.status === 204 || answer.status === 304;
    return new Response(empty ? null : answer.body, {
      status: answer.status,
      headers: { "Content-Type": "application/json" },
    });
  };
}
