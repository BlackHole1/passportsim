// The Battery card. Each control is its own `env` call carrying only its field: the board applies
// `soc` then `mv` (`passport.rs` `set_battery`), so one call naming both ends at the voltage, and a
// slider moved with a stale voltage beside it would land elsewhere. The card derives the other
// reading from the cell's open-circuit curve (`battery.rs` `OCV_MV`, pinned by `battery.test.ts`).
// The model is class C: a plausible curve, not a measured one.

import type { EnvArgs } from "../../api/commands";

export interface BatteryState {
  readonly soc: number;
  readonly mv: number;
  readonly tempC: number;
  readonly connected: boolean;
}

/**
 * Open-circuit voltage in mV at 0, 10, ... 100 % (`crates/pemu-board/src/battery.rs` `OCV_MV`).
 * Nothing loads or charges the cell while a machine runs, so its voltage stays on this curve.
 */
export const OCV_MV: readonly number[] = [2950, 3500, 3620, 3680, 3730, 3780, 3850, 3920, 4000, 4100, 4200];

const OCV_STEP_PERCENT = 10;

/** A new board's cell, and the one a reconnect leaves: full (`passport.rs` `FULL_SOC_MILLI`). */
export const FULL_SOC = 100;

/** Room temperature, where a new cell starts (`battery.rs` `ROOM_TEMP_DECI_C`). */
export const ROOM_TEMP_C = 25;

/** The open-circuit voltage at `socMilli` thousandths of a percent, in `BatteryConfig::ocv_mv`'s integer math. */
export function ocvMv(socMilli: number): number {
  const soc = Math.min(100_000, Math.max(0, Math.trunc(socMilli)));
  const span = OCV_STEP_PERCENT * 1_000;
  const index = Math.trunc(soc / span);
  if (index >= OCV_MV.length - 1) {
    return OCV_MV[OCV_MV.length - 1]!;
  }
  const low = OCV_MV[index]!;
  const high = OCV_MV[index + 1]!;
  return low + Math.trunc(((high - low) * (soc - index * span)) / span);
}

export function socMilliAtOcv(mv: number): number {
  if (mv <= OCV_MV[0]!) {
    return 0;
  }
  const span = OCV_STEP_PERCENT * 1_000;
  for (let index = 0; index < OCV_MV.length - 1; index += 1) {
    const low = OCV_MV[index]!;
    const high = OCV_MV[index + 1]!;
    if (mv < high) {
      return index * span + Math.trunc(((mv - low) * span) / (high - low));
    }
  }
  return 100_000;
}

/**
 * The millivolts the firmware prints: the gauge stores VCELL in 312.5 uV steps (`cw2017.rs`
 * `vcell_raw`) and the BSP converts back with `raw * 3125 / 10000`, truncating both ways
 * (`bsp_battery.c` `bsp_battery_mv`), so some voltages read one millivolt low.
 */
export function firmwareMv(mv: number): number {
  const raw = Math.min(0x3fff, Math.trunc((mv * 10_000) / 3_125));
  return Math.trunc((raw * 3_125) / 10_000);
}

export const DEFAULT_BATTERY: BatteryState = {
  soc: FULL_SOC,
  mv: ocvMv(FULL_SOC * 1_000),
  tempC: ROOM_TEMP_C,
  connected: true,
};

export const SOC_RANGE = { min: 0, max: 100 } as const;

/** Cell voltage bounds: the brownout region up to above charge termination, and no typo past it. */
export const MV_RANGE = { min: 2_500, max: 4_500 } as const;

export const TEMP_RANGE = { min: -20, max: 85 } as const;

function clamp(value: number, range: { min: number; max: number }, fallback: number): number {
  if (!Number.isFinite(value)) {
    return fallback;
  }
  return Math.min(range.max, Math.max(range.min, Math.round(value)));
}

export type BatteryChange =
  | { readonly kind: "soc"; readonly soc: number }
  | { readonly kind: "mv"; readonly mv: number }
  | { readonly kind: "temp"; readonly tempC: number }
  | { readonly kind: "connected"; readonly connected: boolean };

export function changeArgs(change: BatteryChange): EnvArgs {
  switch (change.kind) {
    case "soc":
      return { battery: { soc: clamp(change.soc, SOC_RANGE, DEFAULT_BATTERY.soc) } };
    case "mv":
      return { battery: { mv: clamp(change.mv, MV_RANGE, DEFAULT_BATTERY.mv) } };
    case "temp":
      return { battery: { temp_c: clamp(change.tempC, TEMP_RANGE, DEFAULT_BATTERY.tempC) } };
    case "connected":
      return { battery: { present: change.connected } };
  }
}

/**
 * The cell after a set of `env` arguments, in the board's order (`passport.rs` `set_battery`):
 * disconnect (leaving a full cell), charge, voltage, temperature. Each moves the other reading
 * along the curve.
 */
export function fromArgs(args: EnvArgs, base: BatteryState = DEFAULT_BATTERY): BatteryState {
  const set = args.battery;
  if (set === undefined) {
    return base;
  }
  let state = base;
  if (set.present === false) {
    state = { ...state, soc: FULL_SOC, mv: ocvMv(FULL_SOC * 1_000), connected: false };
  } else if (set.present === true) {
    state = { ...state, connected: true };
  }
  if (set.soc !== undefined) {
    const soc = clamp(set.soc, SOC_RANGE, state.soc);
    state = { ...state, soc, mv: ocvMv(soc * 1_000) };
  }
  if (set.mv !== undefined) {
    const mv = clamp(set.mv, MV_RANGE, state.mv);
    state = { ...state, mv, soc: Math.trunc(socMilliAtOcv(mv) / 1_000) };
  }
  if (set.temp_c !== undefined) {
    state = { ...state, tempC: clamp(set.temp_c, TEMP_RANGE, state.tempC) };
  }
  return state;
}

export function applyChange(state: BatteryState, change: BatteryChange): BatteryState {
  return fromArgs(changeArgs(change), state);
}

/**
 * `battery.fresh()`: `env` has no `fresh`, but a disconnect and reconnect leave the gauge at its
 * power-on defaults, so `bsp_battery_init` takes the slow profile-write path at the next boot.
 */
export const FRESH_ARGS: readonly EnvArgs[] = [
  { battery: { present: false } },
  { battery: { present: true } },
];
