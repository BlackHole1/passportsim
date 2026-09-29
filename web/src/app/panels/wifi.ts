// The Wi-Fi card: scripted APs, LAN services, pcap and the bridge toggle. Every BSSID it generates
// starts `02:00:00`, so no real access point's address reaches a pcap through the UI. A live
// bridge makes the run `live` rather than `replayable`: host sockets are not virtual time.

import type { EnvArgs, NetCaptureArgs, WifiApArgs } from "../../api/commands";

/** The auth modes the card offers: those with a name and an IDF number in `AUTH_MODES`. */
export type CardAuth = "open" | "wep" | "wpa-psk" | "wpa2-psk" | "wpa3-psk" | "wpa2-enterprise";

export interface AccessPoint {
  readonly ssid: string;
  readonly bssid: string;
  /** dBm; the driver reports it unchanged. */
  readonly rssi: number;
  readonly channel: number;
  readonly auth: CardAuth;
  /**
   * The pre-shared key, empty for an open network; a key means WPA2-PSK. Never echoed by `env` and
   * never rendered by the card.
   */
  readonly key: string;
}

export interface WifiState {
  readonly aps: readonly AccessPoint[];
  /** Bridge mode proxies allowlisted host sockets and journals payloads. */
  readonly bridge: boolean;
  readonly capture: string | null;
}

/** The placeholder MAC prefix; never a real device's. */
export const PLACEHOLDER_OUI = "02:00:00";

export const CHANNEL_RANGE = { min: 1, max: 11 } as const;

export const DEFAULT_WIFI: WifiState = { aps: [], bridge: false, capture: null };

/** A deterministic BSSID for the `index`-th AP, so scenario output is stable across runs. */
export function placeholderBssid(index: number): string {
  const value = Math.max(0, Math.trunc(index)) & 0xff_ff_ff;
  const bytes = [(value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff];
  return `${PLACEHOLDER_OUI}:${bytes.map((b) => b.toString(16).padStart(2, "0")).join(":")}`;
}

export function isPlaceholderBssid(bssid: string): boolean {
  return bssid.toLowerCase().startsWith(`${PLACEHOLDER_OUI}:`);
}

export class WifiFormError extends Error {
  constructor(
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "WifiFormError";
  }
}

export function normalizeAp(ap: Partial<AccessPoint>, index: number): AccessPoint {
  const ssid = ap.ssid?.trim() ?? "";
  if (ssid.length === 0 || ssid.length > 32) {
    throw new WifiFormError("ssid", "an SSID is 1 to 32 characters");
  }
  const bssid = ap.bssid?.trim() ?? placeholderBssid(index);
  if (!isPlaceholderBssid(bssid)) {
    throw new WifiFormError(
      "bssid",
      `a BSSID starts ${PLACEHOLDER_OUI}: the emulator never carries a real access point's address`,
    );
  }
  const channel = ap.channel ?? 1;
  if (!Number.isInteger(channel) || channel < CHANNEL_RANGE.min || channel > CHANNEL_RANGE.max) {
    throw new WifiFormError("channel", `a channel is ${CHANNEL_RANGE.min} to ${CHANNEL_RANGE.max}`);
  }
  const rssi = ap.rssi ?? -55;
  if (!Number.isInteger(rssi) || rssi > 0 || rssi < -110) {
    throw new WifiFormError("rssi", "an RSSI is a negative whole number of dBm");
  }
  const key = ap.key ?? "";
  if (key !== "" && (key.length < 8 || key.length > 63)) {
    throw new WifiFormError("key", "a WPA2 pre-shared key is 8 to 63 characters");
  }
  // The auth mode follows the key, because that is the pair `env` accepts.
  return { ssid, bssid, rssi, channel, auth: ap.auth ?? (key === "" ? "open" : "wpa2-psk"), key };
}

export function toArgs(ap: AccessPoint): WifiApArgs {
  return {
    ssid: ap.ssid,
    bssid: ap.bssid,
    rssi: ap.rssi,
    channel: ap.channel,
    auth: ap.auth,
  };
}

/** The ESP-IDF `wifi_auth_mode_t` of each mode (`esp_wifi_types.h`), which `env` takes as a number. */
export const AUTH_MODES: Record<CardAuth, number> = {
  open: 0,
  wep: 1,
  "wpa-psk": 2,
  "wpa2-psk": 3,
  "wpa3-psk": 6,
  "wpa2-enterprise": 5,
};

/**
 * The `env` arguments that script the whole scanned air, replacing the set before. The card owns
 * the list, so one call replaces it and the air never keeps a row the card forgot; `toArgs` and
 * `removeArgs` are the per-AP `wifi_ap` form.
 */
export function envArgs(aps: readonly AccessPoint[]): EnvArgs {
  return {
    wifi: {
      aps: aps.map((ap) => ({
        ssid: ap.ssid,
        bssid: ap.bssid,
        rssi: ap.rssi,
        channel: ap.channel,
        auth: AUTH_MODES[ap.auth],
        ...(ap.key === "" ? {} : { psk: ap.key }),
      })),
    },
  };
}

export function removeArgs(ap: AccessPoint): WifiApArgs {
  return { ssid: ap.ssid, bssid: ap.bssid, remove: true };
}

/** The AP a set of `wifi_ap` arguments describes; an auth mode the card cannot render is dropped. */
export function fromArgs(args: WifiApArgs, index = 0): AccessPoint {
  const auth = typeof args.auth === "string" && args.auth in AUTH_MODES ? (args.auth as CardAuth) : undefined;
  return normalizeAp(
    {
      ssid: args.ssid,
      bssid: args.bssid,
      rssi: args.rssi,
      channel: args.channel,
      auth,
    },
    index,
  );
}

/**
 * The `net_capture` arguments of "Save pcap". The capture is always-on module state bounded to its
 * newest 64 KiB, so the only action is writing it out; the command names the file by virtual instant.
 */
export function captureArgs(label?: string): NetCaptureArgs {
  return label === undefined ? { op: "save" } : { op: "save", label };
}

