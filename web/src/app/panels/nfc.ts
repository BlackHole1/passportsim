// The NFC card. `nfc_tag` sets what the virtual NTAG213 holds, persistent state of the instance;
// `nfc_tap` is an event over it. The firmware has no connection to the tag at all.

import type { NdefRecord, NfcTagArgs, NfcTapArgs } from "../../api/commands";
import { DEFAULT_DWELL_MS, tapArgs, type TapForm } from "../tap";

export interface NfcState {
  readonly records: readonly NdefRecord[];
  /**
   * The tag UID as hex, or `null` for the one derived from the machine seed. The UI never invents
   * one: a hand-typed UID can be a real card's.
   */
  readonly uid: string | null;
  /** Whether the tag's OTP lock bits are set; irreversible. */
  readonly locked: boolean;
  readonly dwellMs: number;
  /** Whether the tag's NFC counter is armed (NFC_CNT_EN of the ACCESS page); unarmed, it never moves. */
  readonly counter: boolean;
}

/** The factory state: CC `E1 10 12 00`, an empty NDEF TLV, the seed's UID. */
export const DEFAULT_NFC: NfcState = {
  records: [],
  uid: null,
  locked: false,
  dwellMs: DEFAULT_DWELL_MS,
  counter: false,
};

const UID_HEX = /^[0-9a-fA-F]{14}$/;

export function isValidUid(uid: string): boolean {
  return UID_HEX.test(uid);
}

/** The `nfc_tag` arguments for a card state. `lock` is never sent `false`: there is no unlock. */
export function toArgs(state: NfcState): NfcTagArgs {
  const args: {
    ndef: readonly NdefRecord[];
    uid?: string;
    lock?: boolean;
    counter?: boolean;
  } = { ndef: state.records };
  if (state.uid !== null && isValidUid(state.uid)) {
    args.uid = state.uid;
  }
  if (state.locked) {
    args.lock = true;
  }
  if (state.counter) {
    // Kept on a rewrite: the ACCESS bit is the tag's, not the record list's.
    args.counter = true;
  }
  return args;
}

/**
 * The `nfc_tag` arguments that arm the counter and nothing else, so arming never rewrites `ndef`.
 * Only `true` is sent; the card cannot disarm it.
 */
export function counterArgs(): NfcTagArgs {
  return { counter: true };
}

/**
 * The `nfc_tag` arguments that lock the tag and nothing else. A `lock` sent with `ndef` would
 * rewrite the tag at the same moment, and an empty record list would then be locked in for good.
 */
export function lockArgs(): NfcTagArgs {
  return { lock: true };
}

export function fromArgs(args: NfcTagArgs, base: NfcState = DEFAULT_NFC): NfcState {
  return {
    records: args.ndef ?? base.records,
    uid: args.uid !== undefined && isValidUid(args.uid) ? args.uid : base.uid,
    locked: args.lock ?? base.locked,
    dwellMs: base.dwellMs,
    counter: args.counter ?? base.counter,
  };
}

export function tapFor(state: NfcState, form: TapForm): NfcTapArgs {
  return tapArgs({ ...form, dwellMs: form.dwellMs ?? state.dwellMs });
}

/** A record for the editor list. A Wi-Fi key is shown as its length, never as the key. */
export function describeRecord(record: NdefRecord): string {
  switch (record.type) {
    case "uri":
      return `URI ${record.uri}`;
    case "text":
      return `Text ${record.lang ? `[${record.lang}] ` : ""}${record.text}`;
    case "wifi":
      return `Wi-Fi ${record.ssid} (${record.auth}/${record.encr}, key ${record.key.length} chars)`;
  }
}
