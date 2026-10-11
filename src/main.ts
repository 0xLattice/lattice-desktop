// Lattice Desktop: the app's wiring (app.ts) on the Tauri shell.

import { startApp } from "./app.ts";
import { hostOps, nodeHTTP, nodeCookie, vaultFile, settingsFile, saveBackup, printPage } from "./tauri-host.ts";

startApp({
  ops: hostOps, nodeHTTP, nodeCookie,
  vault: vaultFile, settings: settingsFile,
  saveBackup, print: printPage,
  reload: () => location.reload(),
}).catch((e) => {
  document.getElementById("view")!.textContent = "Could not start: " + String((e as Error)?.message ?? e);
});
