// The wallet binds its header controls by element id. The desktop page is its
// own file, so a control the wallet's page gains must be added here too: one
// that is missing is silently unreachable (Settings, the parent chain).

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const ids = (file: string) =>
  [...readFileSync(new URL(file, import.meta.url), "utf8").matchAll(/\sid="([^"]+)"/g)].map((m) => m[1]).sort();

test("the desktop page has every control the wallet's own page has", () => {
  assert.deepEqual(ids("../src/index.html"), ids("../vendor/nexus-wallet/src/popup/index.html"));
});
