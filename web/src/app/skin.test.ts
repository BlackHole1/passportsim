import { describe, expect, test } from "bun:test";
import { ButtonId } from "../worker/layout";
import {
  DEFAULT_USB_STATE,
  GLASS_REGION,
  LADDER_BUTTONS,
  USB_STATES,
  backlightPercent,
} from "./skin";
import { PANEL_HEIGHT, PANEL_WIDTH } from "./scale";

describe("the ladder buttons", () => {
  test("the skin draws UP, OK and DOWN, and each maps to a distinct ButtonId", () => {
    expect(LADDER_BUTTONS.map((b) => b.label)).toEqual(["UP", "OK", "DOWN"]);
    expect(LADDER_BUTTONS.map((b) => b.button)).toEqual([ButtonId.Up, ButtonId.Ok, ButtonId.Down]);
    expect(new Set(LADDER_BUTTONS.map((b) => b.button)).size).toBe(3);
  });

  test("every button has an accessible name", () => {
    for (const spec of LADDER_BUTTONS) {
      expect(spec.ariaLabel.length).toBeGreaterThan(spec.label.length);
    }
  });
});

describe("the USB connector", () => {
  test("the four USB states are in their table order", () => {
    expect(USB_STATES.map((state) => state.id)).toEqual(["U0", "U1", "U2", "U3"]);
    expect(USB_STATES.map((state) => state.name)).toEqual([
      "DETACHED",
      "CHARGE_ONLY",
      "ATTACHED_IDLE",
      "ATTACHED_OPEN",
    ]);
  });

  test("only U3 has a client holding the port open, and only U0 is unplugged", () => {
    const open = USB_STATES.filter((state) => state.clientOpen).map((state) => state.id);
    expect(open).toEqual(["U3"]);
    const unplugged = USB_STATES.filter((state) => !state.cable).map((state) => state.id);
    expect(unplugged).toEqual(["U0"]);
  });

  test("a new machine starts in U3, the default host state", () => {
    expect(DEFAULT_USB_STATE).toBe("U3");
    const state = USB_STATES.find((entry) => entry.id === DEFAULT_USB_STATE);
    expect(state?.cable).toBe(true);
    expect(state?.clientOpen).toBe(true);
  });
});

describe("the skin geometry", () => {
  test("the glass region is exactly the 240x320 panel", () => {
    expect(GLASS_REGION.width).toBe(PANEL_WIDTH);
    expect(GLASS_REGION.height).toBe(PANEL_HEIGHT);
  });
});

describe("backlightPercent", () => {
  test("the backlight integer pair becomes a whole percent", () => {
    // `(duty >> 4, 1 << duty_res)`: full scale with 13-bit resolution is 8192.
    expect(backlightPercent(8192, 8192)).toBe(100);
    expect(backlightPercent(4096, 8192)).toBe(50);
    expect(backlightPercent(0, 8192)).toBe(0);
    expect(backlightPercent(1, 8192)).toBe(0);
  });

  test("a duty seen above its scale reads 100 rather than more", () => {
    expect(backlightPercent(9000, 8192)).toBe(100);
  });

  test("an unusable scale reads 0 rather than NaN", () => {
    expect(backlightPercent(100, 0)).toBe(0);
    expect(backlightPercent(Number.NaN, 8192)).toBe(0);
  });
});
