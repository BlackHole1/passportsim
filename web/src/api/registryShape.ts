// The two `CommandSpec` facts "Copy as CLI" and "Copy as scenario step" need (`cli.positional` and
// `scenario_step`), which the generated `.d.ts` files do not carry. `registryShape.test.ts` reads
// every `#[command(...)]` attribute under `crates/pemu-api/src/commands/` and fails on any mismatch.

import type { CommandName } from "./commands";

export interface RegistryShape {
  /** Whether a `#[command]` with this name exists; an unregistered one has no CLI line and no step. */
  readonly registered: boolean;
  /** `CommandSpec::cli.positional`, in declared order. */
  readonly positional: readonly string[];
  /** `CommandSpec::scenario_step`, or `null` when the command claims none. */
  readonly scenarioStep: string | null;
}

/** The shape `copy.ts` reads for a command this build lacks, as an old journal entry may name. */
export const UNREGISTERED: RegistryShape = { registered: false, positional: [], scenarioStep: null };

export const REGISTRY_SHAPE: Readonly<Record<CommandName, RegistryShape>> = {
  doctor: { registered: true, positional: [], scenarioStep: null },
  status: { registered: true, positional: ["instance"], scenarioStep: null },
  clock: { registered: true, positional: ["op"], scenarioStep: null },
  input: { registered: true, positional: ["button", "action"], scenarioStep: "press" },
  env: { registered: true, positional: [], scenarioStep: "env" },
  run: { registered: true, positional: ["until"], scenarioStep: "wait" },
  serial: { registered: true, positional: ["op"], scenarioStep: "serial.write" },
  ui: { registered: true, positional: [], scenarioStep: "ui.snapshot" },
  inspect: { registered: true, positional: ["what"], scenarioStep: "inspect.expect" },
  snapshot: { registered: true, positional: ["op", "name"], scenarioStep: "snapshot" },
  mic_set: { registered: true, positional: ["kind"], scenarioStep: "mic.set" },
  audio_capture: { registered: true, positional: [], scenarioStep: "audio.capture" },
  nfc_tag: { registered: true, positional: [], scenarioStep: "nfc.tag" },
  nfc_tap: { registered: true, positional: [], scenarioStep: "nfc.tap" },
  power: { registered: true, positional: ["op"], scenarioStep: null },
  usb: { registered: true, positional: ["state"], scenarioStep: null },
  ble_scan: { registered: true, positional: ["duration_ms"], scenarioStep: "ble.scan" },
  ble_connect: { registered: true, positional: ["addr"], scenarioStep: "ble.connect" },
  ble_gatt: { registered: true, positional: ["op"], scenarioStep: "ble.gatt" },
  // `net_http` is registered too, but the UI does not drive it.
  wifi_ap: { registered: true, positional: [], scenarioStep: "wifi.ap" },
  net_capture: { registered: true, positional: [], scenarioStep: "net.capture" },
};

/** The scenario@1 keys the runner implements itself (`pemu_api::scenario::BUILT_IN_STEPS`). */
export const BUILT_IN_STEPS = ["repeat", "set", "delay", "expect_not", "ui.expect"] as const;

/** The fields a step may carry beside its command key (`pemu_api::scenario::STEP_FIELDS`). */
export const STEP_FIELDS = ["name", "id", "timeout", "continue_on_error", "expect"] as const;

/** `pemu_api::scenario::SCHEMA`. */
export const SCENARIO_SCHEMA = "passportsim/scenario@1";
