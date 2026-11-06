// `lattice-swap-driver`, as lattice-swap-web's screens import it, over the
// shell's swap engine: the commands and events in swap-host.ts. The page's
// `Wallet` and `Buyer` are views of the one wallet and the one followed swap
// the shell runs; nothing here holds a key or reaches a network. The site's
// driver (lattice-swap's web/driver) is the reference for every shape.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  SWAP_ERROR_EVENT, SWAP_STATUS_EVENT, swapBackup, swapBalance, swapCancel, swapNeed, swapOffers,
  swapStart, swapStatus, swapWalletExists, swapWalletOpen, swapWithdraw,
  type SwapError, type SwapOffer, type SwapStatus,
} from "../swap-host.ts";

/** The shell's refusal as an `Error`: a command rejects with its text,
 * and the page shows `message`. */
const failed = <T>(answer: Promise<T>): Promise<T> =>
  answer.catch((e: unknown) => {
    throw e instanceof Error ? e : new Error(String(e));
  });

/** The settings the page reads. `nexusGenesis` is the Nexus the shell lists
 * offers for, `swap_nexus_tx::MAINNET_GENESIS` (a cargo test checks they
 * agree); `poll` the page's balance read. */
export const DEFAULTS = {
  nexusGenesis: "bafyreiggtg4ezifboyekbf4gxst2jr3mpjqcxsbmopy7w6fp46g4ngpgxa",
  poll: 10_000,
};

/** The shell holds the wallet's files: the page's store is never read. */
export class IndexedDBStore {}

/** The engine runs in the shell: nothing to load. */
export const load = async (): Promise<void> => {};

/** Whether every number in `value` is an integer JavaScript holds exactly. */
export function safe(value: unknown): boolean {
  if (typeof value === "number") return Number.isSafeInteger(value);
  if (value && typeof value === "object") return Object.values(value).every(safe);
  return true;
}

/** Whether `text` is spelled as Lattice spells a CID (`swap_nexus_tx::is_cid`):
 * multibase base32, lowercase, of CIDv1 dag-cbor sha2-256. */
export function isCid(text: string): boolean {
  if (!/^b[a-z2-7]+$/.test(text)) return false;
  let bits = 0;
  let value = 0;
  const bytes: number[] = [];
  for (const c of text.slice(1)) {
    value = (value << 5) | "abcdefghijklmnopqrstuvwxyz234567".indexOf(c);
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      bytes.push((value >> bits) & 0xff);
    }
  }
  // Unpadded and canonical: no whole spare character, and its spare bits zero.
  if (bits >= 5 || (value & ((1 << bits) - 1)) !== 0) return false;
  return bytes.length === 36 && bytes[0] === 0x01 && bytes[1] === 0x71 && bytes[2] === 0x12 && bytes[3] === 0x20;
}

/** The last board read: the shell decodes and checks each offer, so the page's
 * `decodeOffer` is a lookup in it. */
let board = new Map<string, SwapOffer>();

/** The offers the shell's relay lists (an operator setting: `relay` is only
 * shown), each as its text. */
export async function offers(_relay?: string): Promise<string[]> {
  const listed = await failed(swapOffers());
  board = new Map(listed.map(({ text, offer }) => [text, offer]));
  return listed.map(({ text }) => text);
}

/** The offer `text` says, as the shell decoded it; throws on one it did not list. */
export function decodeOffer(text: string): SwapOffer {
  const offer = board.get(text);
  if (offer === undefined) throw new Error("not an offer on the relay's board");
  return offer;
}

/** The shell's recovery file is written by the shell, never read here:
 * `Wallet#backup` gives this as its text, and `name` is where it was saved. */
export const SAVED_BY_SHELL = "saved by the app";

/** The shell's swap wallet. `config` is the page's, and unused: the shell's
 * settings say where swaps run. */
export class Wallet {
  readonly address: string;
  private constructor(address: string) {
    this.address = address;
  }

  static exists(_config?: unknown): Promise<boolean> {
    return failed(swapWalletExists());
  }

  /** Opens the wallet, made if there is none, its swaps resumed by the shell. */
  static async open(_config?: unknown): Promise<Wallet> {
    return new Wallet(await failed(swapWalletOpen()));
  }

  /** Refused: a recovery file is read by the shell (`swapRestoreFile`, the
   * screen's "Restore from file"), so its key never passes through the page. */
  static async restore(_config: unknown, _file: string): Promise<Wallet> {
    throw new Error("refused: restore with Restore from file, above");
  }

  balance() {
    return failed(swapBalance());
  }

  need(btcAmount: number) {
    return failed(swapNeed(btcAmount));
  }

  /** The swap the shell follows, if any: the only one the page carries on.
   * Punished swaps the shell resumes run there alone. */
  async swaps(): Promise<string[]> {
    const status = await failed(swapStatus());
    return status === null ? [] : [status.swapId];
  }

  async running(): Promise<string | null> {
    const status = await failed(swapStatus());
    return status?.running ? status.swapId : null;
  }

  withdraw(address: string): Promise<string> {
    return failed(swapWithdraw(address));
  }

  /** Saves the recovery file to ~/Downloads in the shell: the key never
   * reaches the page. `name` is the file's path. */
  async backup(): Promise<{ name: string; file: string }> {
    return { name: await failed(swapBackup()), file: SAVED_BY_SHELL };
  }
}

/** The first wait before a started swap's status is read again; the
 * waits double up to ten times it. */
const CONNECT_RETRY_MS = 1_000;

/** `change` events, their detail what to show while a started swap's first
 * status is read again, then null. */
export const connecting = new EventTarget();

/** A start refused for a swap the page does not show, a punished one the
 * shell resumed to ask its seller for the NEX, in plain words. */
export async function plainly(e: Error): Promise<Error> {
  const busy = /swap (\S+) is in progress/.exec(e.message);
  if (busy === null) return e;
  const followed = await swapStatus().catch(() => null);
  return followed?.swapId === busy[1] ? e : new Error("A past swap is asking its seller for your NEX; try again shortly.");
}

/** Whether a status moved on from `before`: its step or a confirmation. */
const progressed = (before: SwapStatus, after: SwapStatus) =>
  before.step !== after.step || JSON.stringify(before.confirmations) !== JSON.stringify(after.confirmations);

/** Whether the shell has stopped running a swap of this `status`. */
const over = (status: SwapStatus) => status.outcome !== null || status.step === "offer-gone" || status.step === "cancelled";

/** The swap the shell follows. Its status comes from the shell's events,
 * those of other swaps (punished ones the shell resumed) left out. */
export class Buyer extends EventTarget {
  #status: SwapStatus;
  /** The last failed round's error, shown until a status shows progress:
   * the shell's polls between failures carry no warning. */
  #stalled: string | null = null;
  private constructor(status: SwapStatus) {
    super();
    this.#status = status;
  }

  get swapId(): string {
    return this.#status.swapId;
  }

  static async #followed(swapId: string): Promise<Buyer> {
    const status = await failed(swapStatus());
    if (status === null || status.swapId !== swapId) throw new Error(`no swap ${swapId} is followed`);
    return new Buyer(status);
  }

  /** Starts a swap of `offer` for `policy.btc_amount` at `policy.price`;
   * every other bound is the shell's. Once the shell has started it, the
   * swap is followed whatever its first status read meets: that read is
   * tried again, `connecting` saying so meanwhile, so no swap runs that the
   * page cannot show or cancel. */
  static async prepare(
    _wallet: Wallet,
    { offer, policy, nexAddress }: { offer: string; policy: { btc_amount: number; price: number }; nexAddress: string },
  ): Promise<Buyer> {
    if (!isCid(nexAddress)) throw new Error(`${nexAddress} is not a Nexus address`);
    const swapId = await failed(swapStart(offer, policy.btc_amount, policy.price, nexAddress)).catch(async (e: Error) => {
      throw await plainly(e);
    });
    for (let wait = CONNECT_RETRY_MS; ; wait = Math.min(wait * 2, 10 * CONNECT_RETRY_MS)) {
      const status = await swapStatus().catch(() => null);
      if (status?.swapId === swapId) {
        connecting.dispatchEvent(new CustomEvent("change", { detail: null }));
        return new Buyer(status);
      }
      connecting.dispatchEvent(new CustomEvent("change", { detail: "Connecting to your swap…" }));
      await new Promise((resolve) => setTimeout(resolve, wait));
    }
  }

  static resume(_wallet: Wallet, swapId: string): Promise<Buyer> {
    return Buyer.#followed(swapId);
  }

  status(): SwapStatus {
    return this.#status;
  }

  #announce(status: SwapStatus) {
    this.#status = status;
    this.dispatchEvent(new CustomEvent("status", { detail: status }));
  }

  /** Follows the swap the shell runs until it ends, its offer goes, it is
   * cancelled, or `signal` aborts: its outcome, null if none. */
  async run(signal?: AbortSignal): Promise<string | null> {
    let done = false;
    let stops: UnlistenFn[] = [];
    const stop = () => {
      done = true;
      for (const unlisten of stops.splice(0)) unlisten();
    };
    await new Promise<void>((resolve, reject) => {
      const end = () => {
        stop();
        resolve();
      };
      if (signal?.aborted) return end();
      signal?.addEventListener("abort", end, { once: true });
      const take = (status: SwapStatus | null) => {
        if (done || status?.swapId !== this.swapId) return;
        if (status.warning !== null || progressed(this.#status, status)) this.#stalled = null;
        this.#announce(this.#stalled === null ? status : { ...status, warning: this.#stalled });
        if (over(status)) end();
      };
      Promise.all([
        listen<SwapStatus>(SWAP_STATUS_EVENT, (event) => take(event.payload)),
        listen<SwapError>(SWAP_ERROR_EVENT, ({ payload }) => {
          if (done || payload.swapId !== this.swapId) return;
          this.#stalled = payload.error;
          this.#announce({ ...this.#status, warning: payload.error });
        }),
      ]).then(
        async (unlisten) => {
          stops = unlisten;
          if (done) return stop();
          // What was missed before listening.
          take(await swapStatus().catch(() => null));
          if (!done && over(this.#status)) end();
        },
        (e) => {
          stop();
          reject(e);
        },
      );
    });
    return this.#status.outcome;
  }

  /** Cancels the swap while it has not taken its offer; refused once it has. */
  async cancel(): Promise<void> {
    await failed(swapCancel());
    this.#announce({ ...this.#status, step: "cancelled" });
  }
}
