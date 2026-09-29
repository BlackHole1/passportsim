// A machine that stopped by itself (any stop but a slice limit or a user pause), which the page
// must state because it otherwise looks like a frozen screen.

import { StopCode } from "../worker/layout";
import type { MessageKey } from "./i18n";

/**
 * `fault`: cannot go on, only a restart is offered. `debug`: an armed breakpoint, watchpoint or
 * matcher, continuing is normal. `waiting`: the hart sleeps with nothing that could wake it.
 */
export type StopClass = "fault" | "debug" | "waiting";

export interface MachineStop {
  readonly code: number;
  readonly name: string;
  readonly class: StopClass;
  readonly detail: string | null;
  readonly vtPs: bigint | null;
}

const NAMES = new Map<number, string>(Object.entries(StopCode).map(([name, code]) => [code, name]));

/** The class of a stop code; an unknown code is a fault, the reading that promises nothing. */
export function stopClass(code: number): StopClass {
  switch (code) {
    case StopCode.Breakpoint:
    case StopCode.Watchpoint:
    case StopCode.Matcher:
      return "debug";
    case StopCode.Deadlock:
      return "waiting";
    default:
      return "fault";
  }
}

export function parseStop(code: number, json: string | null): MachineStop {
  let detail: string | null = null;
  let vtPs: bigint | null = null;
  if (json !== null) {
    try {
      const parsed = JSON.parse(json) as Record<string, unknown>;
      if (typeof parsed.detail === "string" && parsed.detail !== "") {
        detail = parsed.detail;
      }
      if (typeof parsed.vt_ps === "string" && /^\d+$/.test(parsed.vt_ps)) {
        vtPs = BigInt(parsed.vt_ps);
      }
    } catch {
    }
  }
  return { code, name: NAMES.get(code) ?? `Stop ${code}`, class: stopClass(code), detail, vtPs };
}

/** The code the page gives a core error that ended the pacing loop; no `StopCode` uses it. */
export const CORE_ERROR_STOP = -1;

export function coreErrorStop(message: string, code?: string): MachineStop {
  return { code: CORE_ERROR_STOP, name: code ?? "CoreError", class: "fault", detail: message, vtPs: null };
}

export function canContinue(stop: MachineStop): boolean {
  return stop.class !== "fault";
}

const REASON_KEYS: Readonly<Record<number, MessageKey>> = {
  [StopCode.Matcher]: "stop.reason.Matcher",
  [StopCode.Breakpoint]: "stop.reason.Breakpoint",
  [StopCode.Watchpoint]: "stop.reason.Watchpoint",
  [StopCode.GuestPanic]: "stop.reason.GuestPanic",
  [StopCode.Deadlock]: "stop.reason.Deadlock",
  [StopCode.Stuck]: "stop.reason.Stuck",
  [StopCode.Tripwire]: "stop.reason.Tripwire",
  [StopCode.Unmodeled]: "stop.reason.Unmodeled",
  [StopCode.Hle]: "stop.reason.Hle",
  [StopCode.Halted]: "stop.reason.Halted",
  [StopCode.ChipReset]: "stop.reason.ChipReset",
  [StopCode.Sleep]: "stop.reason.Sleep",
  [CORE_ERROR_STOP]: "stop.reason.CoreError",
};

export function stopReasonKey(code: number): MessageKey {
  return REASON_KEYS[code] ?? "stop.reason.unknown";
}
