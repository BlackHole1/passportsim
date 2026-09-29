// The USB card. It round-trips a `(cable, rail, client)` triple rather than a U-state, because
// the triple is what `input` carries and the U-state is what it implies.

import type { InputArgs } from "../../api/commands";
import { DEFAULT_USB_STATE, USB_STATES, type UsbStateInfo } from "../skin";

export type UsbStateInfoId = UsbStateInfo["id"];

export interface UsbState {
  readonly cable: boolean;
  /** The MCU rail, which the power button owns; the card only reports it. */
  readonly rail: boolean;
  readonly clientOpen: boolean;
}

export const DEFAULT_USB: UsbState = { cable: true, rail: true, clientOpen: true };

/**
 * The U-state a triple is in. A client cannot hold open a port that never enumerated, so U0 and U1
 * ignore `clientOpen`.
 */
export function usbState(state: UsbState): UsbStateInfo["id"] {
  if (!state.cable) {
    return "U0";
  }
  if (!state.rail) {
    return "U1";
  }
  return state.clientOpen ? "U3" : "U2";
}

export function fromUsbState(id: UsbStateInfo["id"], base: UsbState = DEFAULT_USB): UsbState {
  switch (id) {
    case "U0":
      return { ...base, cable: false };
    case "U1":
      return { ...base, cable: true, rail: false };
    case "U2":
      return { cable: true, rail: true, clientOpen: false };
    case "U3":
      return { cable: true, rail: true, clientOpen: true };
  }
}

/**
 * The `input` calls from `from` to `to`, only for what changed: a cable change is a
 * re-enumeration, so a redundant one would be a real event in the journal.
 */
export function toArgs(from: UsbState, to: UsbState): readonly InputArgs[] {
  const calls: InputArgs[] = [];
  if (from.cable !== to.cable) {
    calls.push({ button: "usb", action: to.cable ? "plug" : "unplug" });
  }
  if (from.clientOpen !== to.clientOpen && to.cable) {
    calls.push({ button: "usb", action: to.clientOpen ? "open" : "close" });
  }
  return calls;
}

/** The state a set of `input` arguments leaves the card in; any other `input` leaves it as it was. */
export function fromArgs(base: UsbState, args: InputArgs): UsbState {
  if (args.button !== "usb") {
    return base;
  }
  switch (args.action) {
    case "plug":
      return { ...base, cable: true };
    case "unplug":
      return { ...base, cable: false };
    case "open":
      return { ...base, clientOpen: true };
    case "close":
      return { ...base, clientOpen: false };
    default:
      return base;
  }
}

export function applyAll(base: UsbState, calls: readonly InputArgs[]): UsbState {
  return calls.reduce(fromArgs, base);
}

export function describe(id: UsbStateInfo["id"]): string {
  return USB_STATES.find((state) => state.id === id)?.summary ?? "";
}

export { RAIL_ONLY_STATES } from "../skin";

export const INITIAL_STATE = DEFAULT_USB_STATE;
