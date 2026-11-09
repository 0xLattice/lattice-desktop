// The shell's swap commands and events, typed: buying NEX with BTC on
// lattice-swap's native engine, which runs in the Rust shell (src-tauri/src/
// swap.rs). The shapes are the website driver's (`Wallet` and `Buyer` in
// lattice-swap's web/driver), as lattice-swap-web's screens use them.

import { invoke } from "@tauri-apps/api/core";

export type SwapNetwork = "bitcoin" | "signet";

/** Where swaps run. The page sets the network, your own Esplora on each
 * network (one missing: mempool.space's), and the networks whose swaps read
 * Nexus on this app's node or on your own by its https URL (one in neither:
 * the public lattice-mainnet-read.fly.dev); a relay is the operator's, in
 * settings.json (null: lattice-swap-relay.fly.dev). */
export interface SwapSettings {
  network: SwapNetwork;
  esplora?: Partial<Record<SwapNetwork, string>> | null;
  ownNexus?: SwapNetwork[] | null;
  nexus?: Partial<Record<SwapNetwork, string>> | null;
  relay?: string | null;
}

/** Where a network's swaps read Nexus: the public node, this app's, or
 * yours at an https URL. */
export type NexusChoice = "public" | "own" | { remote: string };

export type SwapStep = "fund-wallet" | "swapping" | "finishing" | "asking" | "received" | "done" | "refunding" | "refunded" | "offer-gone" | "cancelled";
export type SwapOutcome = "swapped" | "refunded" | "punished" | "drained" | "aborted";

export interface SwapDeadline {
  height: number;
  /** ms since the epoch */
  eta: number;
}

/** A swap's status, as the driver's `Buyer#status()`. */
export interface SwapStatus {
  swapId: string;
  step: SwapStep;
  outcome: SwapOutcome | null;
  wallet: { address: string; confirmed: number | null; unconfirmed: number | null };
  need: number | null;
  confirmations: { funding: number | null; lockSpend: number | null; cancelSpend: number | null; nexLock: number | null };
  deadlines: { cancel: SwapDeadline | null; refund: { from: number; until: number } | null; punish: SwapDeadline | null };
  mustStayOpen: boolean;
  backupCovers: "wallet" | "lock";
  running: boolean;
  btc_amount: number;
  price: number;
  nex_amount: number;
  btc_confirmations: number;
  nex_confirmations: number;
  nexAddress: string | null;
  tip: number | null;
  warning: string | null;
}

/** An offer as a relay's board lists it. */
export interface SwapOffer {
  quote: { price: number; min_btc_amount: number; max_btc_amount: number };
  template: {
    swap_id: string;
    btc_fee: number;
    nex_fee: number;
    t1: number;
    t2: number;
    btc_confirmations: number;
    nex_confirmations: number;
    margin: number;
    nexus_genesis: string;
  };
  btc_network: string;
  relay: string | null;
  seller: string;
}

/** An offer on the board: its text, which `swapStart` takes, and what it says. */
export interface ListedOffer {
  text: string;
  offer: SwapOffer;
}

/** Events the shell sends: a swap's status each poll and when its run ends,
 * and a failed round of its run, which the shell runs again. Both name the
 * swap, and punished swaps resumed to ask for `s` report too: filter by
 * `swapId`. */
export const SWAP_STATUS_EVENT = "swap-status";
export const SWAP_ERROR_EVENT = "swap-error";
export interface SwapError {
  swapId: string;
  error: string;
}

export const swapSettings = () => invoke<SwapSettings>("swap_settings");
export const swapSetNetwork = (network: SwapNetwork) => invoke<void>("swap_set_network", { network });
/** Chooses `network`'s servers in one save: your own Esplora at `esplora`,
 * or mempool.space's when null, and `nexus` for Nexus; a running swap goes
 * on on them. Refused, neither saved, for an Esplora on another network, a
 * URL of your node that is not https, a node by URL (the public one or
 * yours) that is unreachable, on another Nexus or takes no transactions,
 * and once swaps run on another network. */
export const swapSetServers = (network: SwapNetwork, esplora: string | null, nexus: NexusChoice) =>
  invoke<void>("swap_set_servers", { network, esplora, nexus });
/** Refused unless the Esplora is reachable and on the swaps' network. */
export const swapCheckEsplora = () => invoke<void>("swap_check_esplora");
/** Refused unless the Nexus node is reachable, on the Nexus, and (one by
 * URL) takes transactions. */
export const swapCheckNexus = () => invoke<void>("swap_check_nexus");
export const swapWalletExists = () => invoke<boolean>("swap_wallet_exists");
/** Opens the swap wallet, made if there is none, and resumes its swaps: its
 * deposit address. */
export const swapWalletOpen = () => invoke<string>("swap_wallet_open");
export const swapBalance = () => invoke<{ confirmed: number; unconfirmed: number }>("swap_balance");
export const swapNeed = (btcAmount: number) => invoke<{ enough: boolean; need: number }>("swap_need", { btcAmount });
export const swapOffers = () => invoke<ListedOffer[]>("swap_offers");
/** Starts a swap of an offer's text for `btcAmount` sat at `price`: its id.
 * Every other bound is the engine's default. */
export const swapStart = (offer: string, btcAmount: number, price: number, nexAddress: string) =>
  invoke<string>("swap_start", { offer, btcAmount, price, nexAddress });
export const swapCancel = () => invoke<void>("swap_cancel");
export const swapStatus = () => invoke<SwapStatus | null>("swap_status");
/** Saves the recovery file as a new file in ~/Downloads: its path. */
export const swapBackup = () => invoke<string>("swap_backup");
/** Restores the wallet in a recovery file the user picks in a native open
 * dialog, read by the shell: its address, or null when none is picked. */
export const swapRestoreFile = () => invoke<string | null>("swap_restore_file");
/** Sends the whole spendable balance to `address`: the txid. */
export const swapWithdraw = (address: string) => invoke<string>("swap_withdraw", { address });
