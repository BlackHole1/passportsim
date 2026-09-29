// The virtual phone tap on the NFC card: presets that build one `nfc_tap`. A tap is field on,
// the listed ops, field off, with `dwellMs` bounding the ops: an op that does not finish in time
// fails like a real removal. The tap has no MCU connection, so it is validated against the
// command it emits, not guest behaviour.

import type { NdefRecord, NfcOp, NfcTapArgs } from "../api/commands";

/** The default dwell: about how long a phone is held against a tag for a read. */
export const DEFAULT_DWELL_MS = 300;

/** The dwell that makes a write tear: too short for any write to finish. */
export const TEARING_DWELL_MS = 1;

export type TapPreset = "read" | "write-uri" | "write-text" | "write-wifi" | "raw" | "tear";

export interface TapForm {
  readonly preset: TapPreset;
  readonly dwellMs?: number;
  readonly uri?: string;
  readonly text?: string;
  readonly lang?: string;
  readonly ssid?: string;
  readonly auth?: string;
  readonly encr?: string;
  readonly key?: string;
  readonly frames?: readonly string[];
}

export class TapFormError extends Error {
  constructor(
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "TapFormError";
  }
}

const HEX_FRAME = /^[0-9a-fA-F]+$/;

/**
 * Builds the `nfc_tap` arguments for a filled-in preset. The tearing preset is the write preset
 * with {@link TEARING_DWELL_MS}: tearing comes from the dwell, not from a separate op.
 */
export function tapArgs(form: TapForm): NfcTapArgs {
  const dwell = form.dwellMs ?? (form.preset === "tear" ? TEARING_DWELL_MS : DEFAULT_DWELL_MS);
  if (!Number.isInteger(dwell) || dwell <= 0) {
    throw new TapFormError("dwellMs", "the dwell time is a whole number of milliseconds above 0");
  }
  return { ops: tapOps(form), dwell_ms: dwell };
}

export function tapOps(form: TapForm): readonly NfcOp[] {
  switch (form.preset) {
    case "read":
      return [{ op: "readNdef" }];
    case "raw": {
      const frames = (form.frames ?? []).map((frame) => frame.trim()).filter((f) => f.length > 0);
      if (frames.length === 0) {
        throw new TapFormError("frames", "a raw tap needs at least one frame");
      }
      for (const frame of frames) {
        if (!HEX_FRAME.test(frame) || frame.length % 2 !== 0) {
          throw new TapFormError("frames", `${frame} is not a whole number of hex bytes`);
        }
      }
      return [{ op: "raw", frames }];
    }
    default:
      return [{ op: "writeNdef", ndef: [ndefRecord(form)] }];
  }
}

export function ndefRecord(form: TapForm): NdefRecord {
  switch (form.preset) {
    case "write-text":
      if (!form.text) {
        throw new TapFormError("text", "a text record needs text");
      }
      return form.lang
        ? { type: "text", text: form.text, lang: form.lang }
        : { type: "text", text: form.text };
    case "write-wifi": {
      if (!form.ssid) {
        throw new TapFormError("ssid", "a Wi-Fi record needs an SSID");
      }
      if (!form.key) {
        throw new TapFormError("key", "a Wi-Fi record needs a network key");
      }
      return {
        type: "wifi",
        ssid: form.ssid,
        auth: form.auth ?? "wpa2-personal",
        encr: form.encr ?? "aes",
        key: form.key,
      };
    }
    default: {
      if (!form.uri) {
        throw new TapFormError("uri", "a URI record needs a URI");
      }
      return { type: "uri", uri: form.uri };
    }
  }
}

/**
 * The tap a drop on the NFC zone means: a URI record when the text parses as a URI, else a text
 * record. A Wi-Fi record needs four fields, so only the card writes one.
 */
export function droppedTap(text: string, dwellMs = DEFAULT_DWELL_MS): NfcTapArgs {
  const trimmed = text.trim();
  const record: NdefRecord = isUri(trimmed) ? { type: "uri", uri: trimmed } : { type: "text", text: trimmed };
  return { ops: [{ op: "writeNdef", ndef: [record] }], dwell_ms: dwellMs };
}

function isUri(text: string): boolean {
  return /^[a-zA-Z][a-zA-Z0-9+.-]*:\/?\/?\S+$/.test(text) && !text.includes(" ");
}

/** Locking needs a confirmation: the OTP lock bits can never be cleared. */
export const LOCK_NEEDS_CONFIRMATION = true;

export const LOCK_CONFIRMATION =
  "Locking sets the OTP bits of the virtual NTAG213. It cannot be undone for this machine. Lock the tag?";
