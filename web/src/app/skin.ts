// The data behind the device skin: controls, U-states and status, with no DOM, so the tests run
// under `bun test`. `view/Device.tsx` draws it.

import { ButtonId } from "../worker/layout";

export type SkinControlId = "up" | "ok" | "down" | "power" | "nfc-zone" | "usb-connector";

export interface LadderButton {
  readonly id: "up" | "ok" | "down";
  readonly button: ButtonId;
  readonly label: string;
  readonly ariaLabel: string;
}

/** The ladder buttons in the device's order, top to bottom (`skinGeometry.ts` `EDGE_CONTROLS`). */
export const LADDER_BUTTONS: readonly LadderButton[] = [
  { id: "up", button: ButtonId.Up, label: "UP", ariaLabel: "Up button" },
  { id: "down", button: ButtonId.Down, label: "DOWN", ariaLabel: "Down button" },
  { id: "ok", button: ButtonId.Ok, label: "OK", ariaLabel: "OK button" },
];

/**
 * One USB host state. The skin shows all four because the U-state bounds what the console can
 * show: in U0 and U2 the IN FIFO never drains, so a silent console is correct, not a hang.
 */
export interface UsbStateInfo {
  readonly id: "U0" | "U1" | "U2" | "U3";
  readonly name: string;
  readonly cable: boolean;
  /** Whether a CDC-ACM client holds the port open, so IN drains. */
  readonly clientOpen: boolean;
  readonly summary: string;
}

export const USB_STATES: readonly UsbStateInfo[] = [
  {
    id: "U0",
    name: "DETACHED",
    cable: false,
    clientOpen: false,
    summary: "unplugged; no SOF, the IN FIFO never drains",
  },
  {
    id: "U1",
    name: "CHARGE_ONLY",
    cable: true,
    clientOpen: false,
    summary: "plugged with the rail off; nothing enumerates",
  },
  {
    id: "U2",
    name: "ATTACHED_IDLE",
    cable: true,
    clientOpen: false,
    summary: "enumerated with no client; IN drains only while a client reads",
  },
  {
    id: "U3",
    name: "ATTACHED_OPEN",
    cable: true,
    clientOpen: true,
    summary: "a client holds the port open; IN drains per the timing profile",
  },
];

/** A new machine starts with the cable plugged, the rail on and the port open. */
export const DEFAULT_USB_STATE: UsbStateInfo["id"] = "U3";

/**
 * U-states a USB control cannot reach, because reaching them moves the MCU rail, which is the
 * power button's.
 */
export const RAIL_ONLY_STATES: readonly UsbStateInfo["id"][] = ["U1"];

export const GLASS_REGION = { width: 240, height: 320 } as const;

export interface SkinStatus {
  /**
   * Backlight duty as a percentage; `null` until a machine has reported one, or while the core
   * models no duty resolution, so the line never states a brightness nobody measured.
   */
  readonly backlightPercent: number | null;
  /**
   * Panel state; `null` until reported. `inverted` means the glass shows the complement of panel
   * memory, not that INVON was sent: the `official` menu sends INVON and its glass shows memory as
   * drawn.
   */
  readonly panel: "on" | "off" | "sleeping" | "display off" | "inverted" | null;
  readonly usb: UsbStateInfo["id"];
}

export type { PanelState } from "../worker/session";

/**
 * Backlight percent from `(duty, 1 << duty_res)`, rounded and clamped, so a duty that briefly
 * exceeds its scale between two register writes reads 100 rather than 103.
 */
export function backlightPercent(duty: number, scale: number): number {
  if (!Number.isFinite(duty) || !Number.isFinite(scale) || scale <= 0) {
    return 0;
  }
  return Math.max(0, Math.min(100, Math.round((duty / scale) * 100)));
}

export interface DisplayNotice {
  readonly level: "ok" | "inexact" | "fallback" | "lost" | "none";
  readonly text: string;
}

/**
 * The line the skin shows for a `display` message. A fallback is stated, never silent; a lost
 * context outranks the backend, because the glass is blank until it is restored.
 */
export function displayNotice(state: {
  readonly backend: "webgl1" | "canvas2d" | "none";
  readonly reason: string | null;
  readonly contextLost: boolean;
}): DisplayNotice {
  const why = state.reason === null ? "" : `: ${state.reason}`;
  if (state.contextLost) {
    return { level: "lost", text: `renderer ${state.backend}: WebGL context lost, the panel is blank until it is restored` };
  }
  switch (state.backend) {
    case "webgl1":
      return state.reason === null
        ? { level: "ok", text: "renderer WebGL1" }
        : { level: "inexact", text: `renderer WebGL1${why}` };
    case "canvas2d":
      return { level: "fallback", text: `renderer 2D fallback${why}` };
    case "none":
      return { level: "none", text: `no renderer${why}` };
  }
}
