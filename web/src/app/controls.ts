// Device buttons as physical buttons: a press edge on down and a release edge on up, each an
// `input` registry call, so the guest sees the button held while the user holds it. Two calls with
// no slice between them land at the same virtual instant, and a debounced ADC ladder never sees
// that press, so a release waits until the machine has run {@link MIN_HOLD_US} past its press.

import type { ButtonName, InputArgs } from "../api/commands";

export type DeviceControl = ButtonName | "power";

/** The shortest press the guest is given: the CLI's `click` length (`CLICK_MS` in `pemu_api`). */
export const MIN_HOLD_US = 80_000n;

const PS_PER_US = 1_000_000n;

export function edgeArgs(control: DeviceControl, down: boolean): InputArgs {
  return { button: control, action: down ? "press" : "release" };
}

interface Held {
  /** Virtual time the press applied at; `null` while its call is in flight. */
  pressedAtPs: bigint | null;
  letGo: boolean;
}

/**
 * The controls that are down. One tracker serves pointer and keyboard, because a press may start
 * with one and end with the other, and pressing a control twice would read as stuck.
 */
export class ButtonHolds {
  private readonly held = new Map<DeviceControl, Held>();

  isDown(control: DeviceControl): boolean {
    return this.held.has(control);
  }

  /** Starts a press; `false` when the control is already down, and then nothing is to be sent. */
  press(control: DeviceControl): boolean {
    if (this.held.has(control)) {
      return false;
    }
    this.held.set(control, { pressedAtPs: null, letGo: false });
    return true;
  }

  pressed(control: DeviceControl, atPs: bigint): void {
    const entry = this.held.get(control);
    if (entry !== undefined) {
      entry.pressedAtPs = atPs;
    }
  }

  forget(control: DeviceControl): void {
    this.held.delete(control);
  }

  /** The user let go; `false` when the control was not down (a leave after an up, a stray keyup). */
  letGo(control: DeviceControl): boolean {
    const entry = this.held.get(control);
    if (entry === undefined || entry.letGo) {
      return false;
    }
    entry.letGo = true;
    return true;
  }

  letGoAll(): DeviceControl[] {
    return [...this.held.keys()].filter((control) => this.letGo(control));
  }

  /** The controls let go by the user and held at least {@link MIN_HOLD_US}; they leave the tracker. */
  due(nowPs: bigint): DeviceControl[] {
    const ready: DeviceControl[] = [];
    for (const [control, entry] of this.held) {
      if (entry.letGo && entry.pressedAtPs !== null && nowPs >= entry.pressedAtPs + MIN_HOLD_US * PS_PER_US) {
        ready.push(control);
      }
    }
    for (const control of ready) {
      this.held.delete(control);
    }
    return ready;
  }

  get waiting(): boolean {
    return [...this.held.values()].some((entry) => entry.letGo);
  }

  /** Drops everything, sending nothing: the machine these presses reached is gone. */
  clear(): void {
    this.held.clear();
  }
}

export function pressedAt(json: unknown): bigint | null {
  const answer = json as { input?: { press_vt_us?: unknown }; vt_us?: unknown } | null;
  const us = answer?.input?.press_vt_us ?? answer?.vt_us;
  return typeof us === "number" && Number.isSafeInteger(us) && us >= 0 ? BigInt(us) * PS_PER_US : null;
}
