// The redaction pass every exported journal entry goes through, since "Copy as CLI" and recorded
// scenarios are pasted elsewhere. The page redacts what the user typed, in two layers:
//
// 1. By field: `nfc_tag.uid`, a `wifi` NDEF record's `key` (in `nfc_tag.ndef` and `nfc_tap`
//    `writeNdef`), and raw `nfc_tap` frames carrying the tag password or acknowledge (`PWD_AUTH`
//    `1b`, a WRITE `a2` to PWD page `2b` or PACK page `2c`, and the data frame after a
//    COMPAT_WRITE `a0` to either) become `<SECRET>`; a replay then fails visibly at that step.
// 2. By value: every value layer 1 dropped this session is masked inside every other string, as
//    text, hex in either case with or without separators, and base64. A Wi-Fi key counts from 6
//    characters; NFC values have no floor. MAC-like strings are not masked by shape.
//
// Not covered: a secret that exists only inside the machine (an eFuse MAC, an NVS credential echoed
// into a later argument). Refused outright: a call carrying a confirmation code, or asking for an
// unredacted export (`include_secrets: true`).

import type { CommandName } from "./commands";
import type { Json } from "./envelope";

/** The placeholder for a masked secret. */
export const SECRET = "<SECRET>";

/** A text credential shorter than this is masked by field only, not by value. */
export const MIN_SECRET_LENGTH = 6;

export interface RedactedCall {
  readonly args: Json;
  /** JSON-pointer-like paths (`ndef/0/key`) of the values that were replaced. */
  readonly redacted: readonly string[];
  readonly refused: string | null;
}

export type SecretKind =
  /** A credential typed as text (a Wi-Fi key): the 6-character floor applies. */
  | "text"
  | "hex";

interface SecretEntry {
  readonly text: string | null;
  readonly bytes: Uint8Array;
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\/-]/g, "\\$&");
}

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

export function hexBytes(text: string): Uint8Array | null {
  const digits = text.replace(/[\s:-]/g, "");
  if (digits.length === 0 || digits.length % 2 !== 0 || !/^[0-9a-f]+$/i.test(digits)) {
    return null;
  }
  return Uint8Array.from(digits.match(/../g) ?? [], (pair) => Number.parseInt(pair, 16));
}

/**
 * The secret values of a session. Each is masked as its typed text (text secrets only), as hex in
 * either case with no separator or one of `:`, `-` or a space between bytes, and as padded or
 * unpadded base64 of its bytes.
 */
export class SecretValues {
  private readonly entries = new Map<string, SecretEntry>();

  add(value: string, kind: SecretKind = "text"): void {
    if (value === "" || value === SECRET) {
      return;
    }
    const bytes = kind === "hex" ? hexBytes(value) : null;
    if (kind === "hex" && bytes !== null) {
      this.entries.set(`hex:${[...bytes].join(",")}`, { text: null, bytes });
      return;
    }
    if (value.length < MIN_SECRET_LENGTH && kind === "text") {
      return;
    }
    this.entries.set(`text:${value}`, { text: value, bytes: new TextEncoder().encode(value) });
  }

  get size(): number {
    return this.entries.size;
  }

  mask(text: string): string {
    let out = text;
    const ordered = [...this.entries.values()].sort((a, b) => b.bytes.length - a.bytes.length);
    for (const entry of ordered) {
      if (entry.text !== null) {
        out = out.split(entry.text).join(SECRET);
      }
      const hex = [...entry.bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("[\\s:-]?");
      out = out.replace(new RegExp(hex, "gi"), SECRET);
      const b64 = base64(entry.bytes);
      out = out.split(b64).join(SECRET);
      const bare = b64.replace(/=+$/, "");
      if (bare.length >= 4) {
        out = out.replace(new RegExp(`${escapeRegExp(bare)}(?![A-Za-z0-9+/])`, "g"), SECRET);
      }
    }
    return out;
  }
}

type Path = readonly (string | number)[];

function isObject(value: Json | undefined): value is { [key: string]: Json } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const NTAG213_PWD_PAGE = "2b";
const NTAG213_PACK_PAGE = "2c";

/**
 * The secret bytes (as hex) a raw NTAG21x command frame carries, or `null`. `previous` is the frame
 * before it, for COMPAT_WRITE's second phase.
 *
 * - `1B pwd(4)`: PWD_AUTH;
 * - `A2 2B pwd(4)` and `A2 2C pack(2) rfui(2)`: WRITE to the PWD or PACK page;
 * - `A0 2B` or `A0 2C`, then a 16-byte data frame: COMPAT_WRITE, whose data is the second frame.
 */
function ntagFrameSecret(hex: string, previous: string): string | null {
  if (/^1b[0-9a-f]{8}$/.test(hex)) {
    return hex.slice(2, 10);
  }
  const write = /^a2(2b|2c)([0-9a-f]{8})$/.exec(hex);
  if (write) {
    return write[1] === NTAG213_PWD_PAGE ? (write[2] ?? null) : (write[2]?.slice(0, 4) ?? null);
  }
  const compat = /^a0(2b|2c)$/.exec(previous);
  if (compat && /^[0-9a-f]{8,32}$/.test(hex)) {
    return compat[1] === NTAG213_PACK_PAGE ? hex.slice(0, 4) : hex.slice(0, 8);
  }
  return null;
}

interface FieldSecret {
  readonly path: Path;
  readonly values: ReadonlyArray<{ readonly value: string; readonly kind: SecretKind }>;
}

function fieldSecrets(command: CommandName, args: Json): FieldSecret[] {
  const out: FieldSecret[] = [];
  if (!isObject(args)) {
    return out;
  }
  const ndefKeys = (records: Json | undefined, base: Path) => {
    if (!Array.isArray(records)) {
      return;
    }
    records.forEach((record, index) => {
      if (isObject(record) && record.type === "wifi" && typeof record.key === "string") {
        out.push({ path: [...base, index, "key"], values: [{ value: record.key, kind: "text" }] });
      }
    });
  };
  if (command === "nfc_tag") {
    if (typeof args.uid === "string") {
      out.push({ path: ["uid"], values: [{ value: args.uid, kind: "hex" }] });
    }
    ndefKeys(args.ndef, ["ndef"]);
  }
  if (command === "nfc_tap" && Array.isArray(args.ops)) {
    args.ops.forEach((op, index) => {
      if (!isObject(op)) {
        return;
      }
      ndefKeys(op.ndef, ["ops", index, "ndef"]);
      if (Array.isArray(op.frames)) {
        const frames = op.frames.map((frame) => (typeof frame === "string" ? frame.replace(/\s+/g, "").toLowerCase() : ""));
        frames.forEach((hex, at) => {
          const path = ["ops", index, "frames", at];
          const secret = ntagFrameSecret(hex, frames[at - 1] ?? "");
          if (secret !== null) {
            out.push({ path, values: [{ value: secret, kind: "hex" }] });
          }
        });
      }
    });
  }
  return out;
}

/** Adds one call's by-field secrets to `secrets`; `UiJournal.record` calls it for every call. */
export function addCallSecrets(secrets: SecretValues, command: CommandName, args: Json): void {
  for (const field of fieldSecrets(command, args)) {
    for (const { value, kind } of field.values) {
      secrets.add(value, kind);
    }
  }
}

export function sessionSecrets(calls: Iterable<{ command: CommandName; args: Json }>): SecretValues {
  const secrets = new SecretValues();
  for (const call of calls) {
    addCallSecrets(secrets, call.command, call.args);
  }
  return secrets;
}

function setAt(root: Json, path: Path, value: Json): void {
  let node: Json = root;
  for (const key of path.slice(0, -1)) {
    node = (node as Record<string | number, Json>)[key] as Json;
  }
  const last = path[path.length - 1];
  if (last !== undefined) {
    (node as Record<string | number, Json>)[last] = value;
  }
}

function maskAll(value: Json, secrets: SecretValues, path: Path, hits: string[]): Json {
  if (typeof value === "string") {
    const masked = secrets.mask(value);
    if (masked !== value) {
      hits.push(path.join("/"));
    }
    return masked;
  }
  if (Array.isArray(value)) {
    return value.map((item, index) => maskAll(item, secrets, [...path, index], hits));
  }
  if (isObject(value)) {
    const out: { [key: string]: Json } = {};
    for (const [key, item] of Object.entries(value)) {
      out[key] = maskAll(item, secrets, [...path, key], hits);
    }
    return out;
  }
  return value;
}

/** Redacts one journaled call for export against the session's by-value set. Never modifies the input. */
export function redactCall(
  command: CommandName,
  args: Json,
  secrets: SecretValues = new SecretValues(),
): RedactedCall {
  if (isObject(args)) {
    if ("confirm" in args) {
      return {
        args: null,
        redacted: [],
        refused: `\`${command}\` carries a human confirmation code, which is single-use and never written into an output`,
      };
    }
    if (args.include_secrets === true) {
      return {
        args: null,
        redacted: [],
        refused: `\`${command} --include-secrets\` is an unredacted export that needs a human confirmation, so it is not recorded`,
      };
    }
  }
  const copy = structuredClone(args);
  const hits: string[] = [];
  for (const { path } of fieldSecrets(command, copy)) {
    setAt(copy, path, SECRET);
    hits.push(path.join("/"));
  }
  const masked = maskAll(copy, secrets, [], hits);
  return { args: masked, redacted: [...new Set(hits)], refused: null };
}
