// The registry commands the web UI drives. Types come from the generated
// `web/src/gen/commands/<command>.d.ts` wherever one exists (`commands.test.ts` enforces it). Shapes
// are flat objects of scalars, so each call has a one-line CLI form to copy.

import type { DoctorArgs, DoctorResult } from "../gen/commands/doctor";
import type { ClockArgs, ClockResult } from "../gen/commands/clock";
import type { InputArgs, InputResult } from "../gen/commands/input";
import type { RunArgs, RunResult } from "../gen/commands/run";
import type { SerialArgs, SerialResult } from "../gen/commands/serial";
import type { StatusArgs, StatusResult } from "../gen/commands/status";
import type { EnvArgs, EnvResult } from "../gen/commands/env";
import type { InspectArgs, InspectResult } from "../gen/commands/inspect";
import type { SnapshotArgs, SnapshotResult } from "../gen/commands/snapshot";
import type { UiArgs, UiResult } from "../gen/commands/ui";
import type { MicSetArgs, MicSetResult } from "../gen/commands/mic_set";
import type {
  AudioCaptureArgs,
  AudioCaptureResult,
} from "../gen/commands/audio_capture";
// `nfc_tag` and `nfc_tap` results are generated; the argument shapes stay the UI's readonly
// declarations, which the generated enums accept.
import type { NfcTagResult } from "../gen/commands/nfc_tag";
import type { NfcTapResult } from "../gen/commands/nfc_tap";
// `power` and `usb` (opt-in `power` group) journal the inputs `input` and `env` already carry.
import type { PowerArgs, PowerResult } from "../gen/commands/power";
import type { UsbArgs, UsbResult } from "../gen/commands/usb";
import type { BleScanArgs, BleScanResult } from "../gen/commands/ble_scan";
import type { BleConnectArgs, BleConnectResult } from "../gen/commands/ble_connect";
import type { BleGattArgs, BleGattResult } from "../gen/commands/ble_gatt";
// `net_capture` takes an `op`, not `start|stop`: the capture is always-on module state.
import type { WifiApArgs, WifiApResult } from "../gen/commands/wifi_ap";
import type { NetCaptureArgs, NetCaptureResult } from "../gen/commands/net_capture";

export type { DoctorArgs, DoctorResult };
export type { ClockArgs, ClockResult, InputArgs, InputResult, RunArgs, RunResult };
export type { SerialArgs, SerialResult, StatusArgs, StatusResult };
export type { EnvArgs, EnvResult, InspectArgs, InspectResult };
export type { SnapshotArgs, SnapshotResult, UiArgs, UiResult };
export type { MicSetArgs, MicSetResult, AudioCaptureArgs, AudioCaptureResult };
export type { NfcTagResult, NfcTapResult };
export type { PowerArgs, PowerResult, UsbArgs, UsbResult };
export type { BleScanArgs, BleScanResult, BleConnectArgs, BleConnectResult };
export type { BleGattArgs, BleGattResult };
export type { WifiApArgs, WifiApResult, NetCaptureArgs, NetCaptureResult };

export type ButtonName = "up" | "ok" | "down";

export type SerialChannel = "usj" | "uart0";


/** `nfc_tag`: loads the virtual NTAG213. */
export interface NfcTagArgs {
  readonly ndef?: readonly NdefRecord[];
  /** A 7-byte UID as hex; derived from the seed when absent. */
  readonly uid?: string;
  /** Sets the OTP lock bits, irreversibly, so the UI confirms first. */
  readonly lock?: boolean;
  /** Arms the NFC counter (NFC_CNT_EN of the ACCESS page); without it a tap never moves it. */
  readonly counter?: boolean;
}

export type NdefRecord =
  | { readonly type: "uri"; readonly uri: string }
  | { readonly type: "text"; readonly text: string; readonly lang?: string }
  | {
      readonly type: "wifi";
      readonly ssid: string;
      readonly auth: string;
      readonly encr: string;
      readonly key: string;
    };

export type NfcOp =
  | { readonly op: "readNdef" }
  | { readonly op: "writeNdef"; readonly ndef: readonly NdefRecord[] }
  | { readonly op: "raw"; readonly frames: readonly string[] };

/**
 * `nfc_tap`: field on, anticollision, the listed ops, field off. `dwell_ms` bounds the ops; an op
 * that does not finish in time fails like a real removal, which is how tearing is tested.
 */
export interface NfcTapArgs {
  readonly ops: readonly NfcOp[];
  readonly dwell_ms?: number;
}

/**
 * Every command the UI drives, by name: what the journal records and "Copy as CLI" renders. A
 * control needing a command not listed here would have no agent equivalent.
 */
export interface WebCommands {
  doctor: { args: DoctorArgs; result: DoctorResult };
  input: { args: InputArgs; result: InputResult };
  env: { args: EnvArgs; result: EnvResult };
  mic_set: { args: MicSetArgs; result: MicSetResult };
  nfc_tag: { args: NfcTagArgs; result: NfcTagResult };
  nfc_tap: { args: NfcTapArgs; result: NfcTapResult };
  power: { args: PowerArgs; result: PowerResult };
  usb: { args: UsbArgs; result: UsbResult };
  wifi_ap: { args: WifiApArgs; result: WifiApResult };
  net_capture: { args: NetCaptureArgs; result: NetCaptureResult };
  ble_scan: { args: BleScanArgs; result: BleScanResult };
  ble_connect: { args: BleConnectArgs; result: BleConnectResult };
  ble_gatt: { args: BleGattArgs; result: BleGattResult };
  audio_capture: { args: AudioCaptureArgs; result: AudioCaptureResult };
  inspect: { args: InspectArgs; result: InspectResult };
  snapshot: { args: SnapshotArgs; result: SnapshotResult };
  clock: { args: ClockArgs; result: ClockResult };
  run: { args: RunArgs; result: RunResult };
  serial: { args: SerialArgs; result: SerialResult };
  status: { args: StatusArgs; result: StatusResult };
  ui: { args: UiArgs; result: UiResult };
}

export type CommandName = keyof WebCommands;

/**
 * The caps group of each command, to explain a refusal: a daemon without `--caps nfc` answers
 * `nfc_tap` with `E_HOST_UNSUPPORTED`, and naming the missing group helps more than the code.
 */
export const COMMAND_GROUP: Readonly<
  Record<CommandName, "core" | "audio" | "radio" | "nfc" | "power">
> = {
  doctor: "core",
  input: "core",
  env: "core",
  inspect: "core",
  snapshot: "core",
  clock: "core",
  run: "core",
  serial: "core",
  status: "core",
  ui: "core",
  audio_capture: "audio",
  mic_set: "audio",
  wifi_ap: "radio",
  net_capture: "radio",
  ble_scan: "radio",
  ble_connect: "radio",
  ble_gatt: "radio",
  nfc_tag: "nfc",
  nfc_tap: "nfc",
  power: "power",
  usb: "power",
};
