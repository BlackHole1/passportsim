// The BLE card: drives the virtual central over the emulated LE controller, never a host radio.

import type { BleConnectArgs, BleGattArgs, BleScanArgs } from "../../api/commands";
import { CommandError } from "../../api/envelope";
import { PLACEHOLDER_OUI } from "./wifi";

/**
 * One advertiser a scan found. No `rssi`: the virtual air carries no path loss, so a signal
 * strength would be invented.
 */
export interface BlePeer {
  readonly addr: string;
  readonly name: string;
  readonly pdu?: string;
  /** Whether a central may connect to it (Core Vol 6 Part B 2.3.1). */
  readonly connectable?: boolean;
  /** Whether the scan that listed it heard it; `false` for one the central remembers. */
  readonly heard?: boolean;
  readonly rssi?: number;
}

export interface BleNotification {
  readonly handle: number;
  readonly uuid: string | null;
  readonly indication: boolean;
  readonly text: string | null;
  readonly value: string;
}

export interface GattNode {
  readonly uuid: string;
  readonly handle: number;
  readonly kind: "service" | "characteristic" | "descriptor";
  readonly properties?: string;
  readonly children?: readonly GattNode[];
}

export interface BleState {
  readonly scanMs: number;
  readonly peers: readonly BlePeer[];
  readonly connected: string | null;
  readonly tree: readonly GattNode[];
  readonly selected: string | null;
  /** The characteristic the card last subscribed to, which is where answers arrive. */
  readonly subscribed: string | null;
  readonly notifications: readonly BleNotification[];
}

/** Long enough to catch an advertiser at any interval the virtual air produces, inside a `run` timeout. */
export const DEFAULT_SCAN_MS = 2_000;

export const DEFAULT_BLE: BleState = {
  scanMs: DEFAULT_SCAN_MS,
  peers: [],
  connected: null,
  tree: [],
  selected: null,
  subscribed: null,
  notifications: [],
};

const ADDR = /^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$/;

export function isValidAddr(addr: string): boolean {
  return ADDR.test(addr);
}

/**
 * Whether an address carries the `02:00:00` placeholder prefix the emulator uses. Anything else
 * is a real advertiser somebody pasted in, and the card marks it before it reaches a btsnoop.
 */
export function isPlaceholderAddr(addr: string): boolean {
  return addr.toLowerCase().startsWith(`${PLACEHOLDER_OUI}:`);
}

export class BleFormError extends Error {
  constructor(
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "BleFormError";
  }
}

export function scanArgs(scanMs: number): BleScanArgs {
  if (!Number.isInteger(scanMs) || scanMs <= 0) {
    throw new BleFormError("scanMs", "a scan is a whole number of milliseconds above 0");
  }
  return { duration_ms: scanMs };
}

export function connectArgs(addr: string): BleConnectArgs {
  if (!isValidAddr(addr)) {
    throw new BleFormError("addr", `${addr} is not a BLE address`);
  }
  return { addr };
}

export function disconnectArgs(addr: string): BleConnectArgs {
  if (!isValidAddr(addr)) {
    throw new BleFormError("addr", `${addr} is not a BLE address`);
  }
  return { addr, disconnect: true };
}

export function gattArgs(
  op: BleGattArgs["op"],
  node?: { uuid?: string; handle?: number },
  value?: string,
  withResponse = true,
): BleGattArgs {
  if (op === "discover") {
    return { op };
  }
  if (!node?.uuid && node?.handle === undefined) {
    throw new BleFormError("uuid", `${op} needs a characteristic UUID or handle`);
  }
  const base: {
    op: BleGattArgs["op"];
    uuid?: string;
    handle?: number;
    value?: string;
    with_response?: boolean;
  } = { op };
  if (node.uuid) {
    base.uuid = node.uuid;
  }
  if (node.handle !== undefined) {
    base.handle = node.handle;
  }
  if (op !== "write") {
    return base;
  }
  const hex = (value ?? "").trim();
  if (hex.length === 0 || hex.length % 2 !== 0 || !/^[0-9a-fA-F]+$/.test(hex)) {
    throw new BleFormError("value", "a write value is a whole number of hex bytes");
  }
  return { ...base, value: hex, with_response: withResponse };
}

export function fromConnectArgs(args: BleConnectArgs, base: BleState = DEFAULT_BLE): BleState {
  if (args.disconnect === true) {
    return { ...base, connected: null, subscribed: null, notifications: [] };
  }
  return { ...base, connected: args.addr ?? null };
}

export function fromScanArgs(args: BleScanArgs, base: BleState = DEFAULT_BLE): BleState {
  return { ...base, scanMs: args.duration_ms ?? base.scanMs };
}

/**
 * The result object of a command answer. The readers below take the shape the generated
 * `BleScanResult` and `BleGattResult` describe and ignore anything else, so a field the command
 * stops sending is a missing row, not a thrown page.
 */
function fields(json: unknown): Record<string, unknown> {
  return typeof json === "object" && json !== null ? (json as Record<string, unknown>) : {};
}

function str(row: Record<string, unknown>, key: string): string | undefined {
  const value = row[key];
  return typeof value === "string" ? value : undefined;
}

function rows(row: Record<string, unknown>, key: string): Record<string, unknown>[] {
  const value = row[key];
  return Array.isArray(value) ? value.map(fields) : [];
}

export const NO_NAME = "(no name)";

export function peersFromResult(json: unknown): BlePeer[] {
  return rows(fields(json), "found").flatMap((row) => {
    const addr = str(row, "addr");
    if (addr === undefined) {
      return [];
    }
    return [
      {
        addr,
        name: str(row, "name") ?? NO_NAME,
        pdu: str(row, "pdu"),
        connectable: row.connectable === true,
        heard: row.heard !== false,
      },
    ];
  });
}

function nodeFromResult(row: Record<string, unknown>): GattNode | null {
  const uuid = str(row, "uuid");
  const kind = str(row, "kind");
  if (
    uuid === undefined ||
    (kind !== "service" && kind !== "characteristic" && kind !== "descriptor")
  ) {
    return null;
  }
  const children = rows(row, "children").flatMap((child) => {
    const node = nodeFromResult(child);
    return node === null ? [] : [node];
  });
  const properties = str(row, "properties");
  return {
    uuid,
    handle: typeof row.handle === "number" ? row.handle : 0,
    kind,
    ...(properties === undefined || properties === "" ? {} : { properties }),
    ...(children.length === 0 ? {} : { children }),
  };
}

export function treeFromResult(json: unknown): GattNode[] {
  return rows(fields(json), "tree").flatMap((row) => {
    const node = nodeFromResult(row);
    return node === null ? [] : [node];
  });
}

export function notificationsFromResult(json: unknown): BleNotification[] {
  return rows(fields(json), "notifications").map((row) => ({
    handle: typeof row.handle === "number" ? row.handle : 0,
    uuid: str(row, "uuid") ?? null,
    indication: row.indication === true,
    text: str(row, "text") ?? null,
    value: str(row, "value") ?? "",
  }));
}

export function newCount(json: unknown): number {
  const value = fields(json).new;
  return typeof value === "number" ? value : 0;
}

export function characteristics(nodes: readonly GattNode[]): GattNode[] {
  return flattenTree(nodes)
    .map((row) => row.node)
    .filter((node) => node.kind === "characteristic");
}

export function subscribeArgs(uuid: string): BleGattArgs {
  if (uuid === "") {
    throw new BleFormError("characteristic", "pick a characteristic to subscribe to");
  }
  return { op: "subscribe", uuid };
}

/**
 * Writes as `text`: what a person types into the Value box is a firmware command line
 * (`{"cmd":"hello"}` plus the newline `pk_protocol.c` frames on).
 */
export function writeTextArgs(uuid: string, text: string, withResponse = true): BleGattArgs {
  if (uuid === "") {
    throw new BleFormError("characteristic", "pick a characteristic to write to");
  }
  if (text === "") {
    throw new BleFormError("value", "a write needs a value");
  }
  return { op: "write", uuid, text, with_response: withResponse };
}

/**
 * Lets `settleMs` of virtual time pass and reports what was notified. `settle_ms` rather than
 * `contains`: the card cannot know which bytes the firmware will answer.
 */
export function notificationsArgs(uuid: string | null, settleMs?: number): BleGattArgs {
  return {
    op: "notifications",
    ...(uuid === null ? {} : { uuid }),
    ...(settleMs === undefined ? {} : { settle_ms: settleMs }),
  };
}

export const POLL_SETTLE_MS = 250;
export const POLL_TRIES = 20;

export function flattenTree(
  nodes: readonly GattNode[],
  depth = 0,
): readonly { readonly node: GattNode; readonly depth: number }[] {
  return nodes.flatMap((node) => [
    { node, depth },
    ...flattenTree(node.children ?? [], depth + 1),
  ]);
}

export interface Advertising {
  readonly addr: string;
  readonly pdu: string;
  readonly connectable: boolean;
  readonly name: string | null;
}

/**
 * What the radio is doing. The first three come from a refusal's `detail.ble`; the rest from the
 * `radio` object every BLE answer carries (`ble_scan.rs` `radio_json`).
 */
export type RadioState =
  | { readonly kind: "unknown" }
  | { readonly kind: "not_bound"; readonly binding: string | null; readonly elf: boolean }
  | { readonly kind: "not_started" }
  | { readonly kind: "stopped" }
  | { readonly kind: "idle" }
  | { readonly kind: "advertising"; readonly adv: Advertising }
  | { readonly kind: "connected"; readonly adv: Advertising | null };

export const UNKNOWN_RADIO: RadioState = { kind: "unknown" };

/** The `ble_scan` arguments of a read: no scan, nothing journaled, no time spent. */
export const READ_ARGS: BleScanArgs = { duration_ms: 0 };

function advertisingOf(value: unknown): Advertising | null {
  const row = fields(value);
  const addr = str(row, "addr");
  const pdu = str(row, "pdu");
  if (addr === undefined || pdu === undefined) {
    return null;
  }
  return { addr, pdu, connectable: row.connectable === true, name: str(row, "name") ?? null };
}

export function radioFromResult(json: unknown): RadioState | null {
  const radio = fields(fields(json).radio);
  const adv = advertisingOf(radio.advertising);
  switch (str(radio, "state")) {
    case "connected":
      return { kind: "connected", adv };
    case "advertising":
      return adv === null ? { kind: "idle" } : { kind: "advertising", adv };
    case "idle":
      return { kind: "idle" };
    default:
      return null;
  }
}

function detailOf(error: unknown): Record<string, unknown> {
  return error instanceof CommandError && error.body.code === "E_STATE" ? fields(error.body.detail) : {};
}

export function radioFromRefusal(error: unknown): RadioState | null {
  const detail = detailOf(error);
  switch (str(detail, "ble")) {
    case "not_bound":
      return { kind: "not_bound", binding: str(detail, "binding") ?? null, elf: detail.elf !== false };
    case "not_started":
      return { kind: "not_started" };
    case "stopped":
      return { kind: "stopped" };
    default:
      // A core without `detail.ble` says only this, and it only ever meant "no module".
      return error instanceof CommandError && error.body.code === "E_STATE" && (error.body.message ?? "").includes("no bound BLE module")
        ? { kind: "not_bound", binding: null, elf: true }
        : null;
  }
}

export type Why =
  | { readonly why: "non_connectable"; readonly addr: string; readonly pdu: string }
  | { readonly why: "not_connected" | "not_advertising" | "peer_not_seen" | "no_answer" };

export function whyOf(error: unknown): Why | null {
  const detail = detailOf(error);
  const why = str(detail, "ble");
  switch (why) {
    case "non_connectable":
      return { why, addr: str(detail, "addr") ?? "", pdu: str(detail, "pdu") ?? "" };
    case "not_connected":
    case "not_advertising":
    case "peer_not_seen":
    case "no_answer":
      return { why };
    default:
      return null;
  }
}

export interface ScanOutcome {
  readonly outcome: "heard" | "silent" | "none_matching" | "read";
  readonly heard: number;
  readonly advEvents: number;
}

export function scanOutcomeFromResult(json: unknown): ScanOutcome | null {
  const row = fields(json);
  const outcome = str(row, "outcome");
  if (outcome !== "heard" && outcome !== "silent" && outcome !== "none_matching" && outcome !== "read") {
    return null;
  }
  return {
    outcome,
    heard: typeof row.heard === "number" ? row.heard : 0,
    advEvents: typeof row.adv_events === "number" ? row.adv_events : 0,
  };
}

export function heardPeers(peers: readonly BlePeer[]): BlePeer[] {
  return peers.filter((peer) => peer.heard !== false);
}
