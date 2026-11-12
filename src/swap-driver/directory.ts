// lattice-swap-web's relay directory (its src/directory.ts), read by the
// shell: the page reaches no network, and the shell reads the directory
// with the engine's own reader, on this app's node when it hosts the
// directory's chain (`swap_relay_directory`). build.mjs points the page's
// imports of it here.

import type { Directory } from "../../vendor/lattice-swap-web/src/config.ts";
import type { DirectoryRead } from "../../vendor/lattice-swap-web/src/directory.ts";
import { swapRelayDirectory } from "../swap-host.ts";

export * from "../../vendor/lattice-swap-web/src/directory.ts";

/** The relays the directory lists, as the shell reads it, whole or not at
 * all: `directory` and `fetch` are the page's, and unused. */
export const readDirectory = async (_directory: Directory, _fetch?: unknown, _timeout?: number): Promise<DirectoryRead> => ({
  relays: await swapRelayDirectory(),
  partial: null,
});
