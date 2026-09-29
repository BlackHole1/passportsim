import { describe, expect, test } from "bun:test";
import {
  FIT_MIN_PERCENT,
  LABEL_GUTTER_PX,
  NARROW_MAX_WIDTH_PX,
  PAGE_GUTTER_PX,
  SNAP_TOLERANCE,
  STACK_BELOW_PX,
  deviceLayout,
  fitDevice,
  overflowsHorizontally,
} from "./layout";
import { MODES, ZOOMS } from "./prefs";
import { PANEL_HEIGHT, PANEL_WIDTH } from "./scale";
import { CSS_PX_PER_MM, DEVICE_MM, SCREEN_MM } from "./skinGeometry";
import { TabState, TABS } from "./tabs";

const NARROW = { width: 400, height: 800 };
const DESKTOP = { width: 1440, height: 900 };

describe("the layout", () => {
  test("400 px is narrow and stacked in both modes", () => {
    for (const mode of MODES) {
      const layout = deviceLayout(mode, NARROW);
      expect(layout.narrow).toBe(true);
      expect(layout.stacked).toBe(true);
    }
  });

  test("the breakpoints are inclusive on the narrow side", () => {
    expect(deviceLayout("simple", { width: NARROW_MAX_WIDTH_PX + 1, height: 800 }).narrow).toBe(false);
    expect(deviceLayout("simple", { width: STACK_BELOW_PX - 1, height: 800 }).stacked).toBe(true);
    expect(deviceLayout("simple", { width: STACK_BELOW_PX, height: 800 }).stacked).toBe(false);
  });

  test("the page never scrolls sideways, in either mode, at any zoom and any width from 320 to 1920", () => {
    for (const mode of MODES) {
      for (const zoom of ZOOMS) {
        for (const dpr of [1, 1.25, 1.5, 2, 3]) {
          for (let width = 320; width <= 1920; width += 7) {
            const viewport = { width, height: 800 };
            expect(overflowsHorizontally(mode, deviceLayout(mode, viewport, dpr, zoom), viewport)).toBe(false);
          }
        }
      }
    }
  });

  test("a first visit on a laptop sees the device fitted, over 140 %, the glass near the panel's own pixels", () => {
    for (const mode of MODES) {
      for (const dpr of [1, 1.25, 1.5, 2]) {
        const device = deviceLayout(mode, DESKTOP, dpr).device;
        expect(device.capped).toBe(false);
        expect(device.percent).toBeGreaterThan(140);
        // 140 % alone would draw it at two thirds of them.
        expect(device.glass.cssWidth).toBeGreaterThanOrEqual(240 * 0.9);
      }
    }
  });

  test("fit never shrinks under 140 % while the width allows it; the page scrolls down instead", () => {
    const device = fitDevice("fit", 2000, 200, 1);
    expect(Math.abs(device.percent - FIT_MIN_PERCENT)).toBeLessThanOrEqual(FIT_MIN_PERCENT * SNAP_TOLERANCE + 1);
    expect(device.bodyHeight).toBeGreaterThan(200);
  });

  test("at 400 px a zoom wider than the column shrinks to it, and says so", () => {
    for (const mode of MODES) {
      const layout = deviceLayout(mode, NARROW, 1, "180");
      expect(layout.device.capped).toBe(true);
      expect(layout.device.frameWidth).toBeLessThanOrEqual(NARROW.width - 2 * PAGE_GUTTER_PX);
      expect(layout.device.percent).toBeLessThan(180);
    }
  });

  test("fit fills the room: wider or taller would not fit", () => {
    const device = fitDevice("fit", 900, 700, 1);
    expect(device.bodyHeight).toBeLessThanOrEqual(700);
    expect(device.frameWidth).toBeLessThanOrEqual(900);
    // Within one step of the three-pixel grid the glass is sized on.
    expect(device.bodyHeight).toBeGreaterThan(700 - (3 * DEVICE_MM.height) / SCREEN_MM.width);
  });

  test("the zooms are ordered, and 100 % is the CSS millimetre", () => {
    const at = (zoom: "100" | "140" | "180") => fitDevice(zoom, 4000, 4000, 1.1);
    expect(at("100").bodyWidth).toBeLessThan(at("140").bodyWidth);
    expect(at("140").bodyWidth).toBeLessThan(at("180").bodyWidth);
    expect(Math.abs(at("100").pxPerMm - CSS_PX_PER_MM) / CSS_PX_PER_MM).toBeLessThanOrEqual(0.02);
  });

  test("the glass is a whole number of device pixels, exactly 3:4, and the body follows from it", () => {
    for (const zoom of ZOOMS) {
      for (const dpr of [1, 1.25, 1.5, 2, 3]) {
        const device = deviceLayout("simple", DESKTOP, dpr, zoom).device;
        const { glass } = device;
        expect(Number.isInteger(glass.deviceWidth)).toBe(true);
        expect(glass.cssWidth * dpr).toBeCloseTo(glass.deviceWidth, 9);
        expect(glass.cssHeight * dpr).toBeCloseTo((glass.deviceWidth * PANEL_HEIGHT) / PANEL_WIDTH, 9);
        expect(Number.isInteger((glass.deviceWidth * PANEL_HEIGHT) / PANEL_WIDTH)).toBe(true);
        expect(device.bodyWidth / glass.cssWidth).toBeCloseTo(DEVICE_MM.width / SCREEN_MM.width, 9);
      }
    }
  });

  test("a glass close to a whole scale is snapped to it and drawn with square pixels", () => {
    // 140 % at a device pixel ratio of 1.5 is 242.5 device pixels across: one per guest pixel.
    const exact = fitDevice("140", 4000, 4000, 1.5).glass;
    expect(exact.deviceWidth).toBe(PANEL_WIDTH);
    expect(exact.scale).toBe(1);
    expect(exact.pixelated).toBe(true);
    // 100 % at 2 is 231.0: snapped up to 240, 4 % larger than asked.
    expect(fitDevice("100", 4000, 4000, 2).glass.deviceWidth).toBe(PANEL_WIDTH);
    // 140 % at 1 is 161.7: far from any whole scale, so it is resampled smoothly.
    const between = fitDevice("140", 4000, 4000, 1).glass;
    expect(Number.isInteger(between.scale)).toBe(false);
    expect(between.pixelated).toBe(false);
  });

  test("a snap never pushes the device wider than its column", () => {
    // 240 device pixels would need a column this wide; one pixel less keeps the glass unsnapped.
    const needed = (PANEL_WIDTH / 1.5 / SCREEN_MM.width) * DEVICE_MM.width + 2 * LABEL_GUTTER_PX;
    const tight = fitDevice("140", Math.floor(needed) - 1, 4000, 1.5);
    expect(tight.frameWidth).toBeLessThanOrEqual(Math.floor(needed) - 1);
    expect(tight.glass.deviceWidth).toBeLessThan(PANEL_WIDTH);
  });
});

describe("card collapse", () => {
  test("narrow collapses every card and widening restores what the user had open", () => {
    const state = new TabState(TABS);
    state.toggleCard("battery");
    expect(state.isCollapsed("battery", false)).toBe(true);
    expect(state.isCollapsed("audio", false)).toBe(false);
    expect(state.isCollapsed("audio", true)).toBe(true);
    // Back to wide: only the card the user closed is still closed.
    expect(state.isCollapsed("audio", false)).toBe(false);
    expect(state.isCollapsed("battery", false)).toBe(true);
  });

  test("a card opened at the narrow width opens, and the wide layout is unchanged", () => {
    const state = new TabState(TABS);
    state.toggleCard("battery", true);
    expect(state.isCollapsed("battery", true)).toBe(false);
    expect(state.isCollapsed("audio", true)).toBe(true);
    expect(state.isCollapsed("battery", false)).toBe(false);
    state.toggleCard("battery", true);
    expect(state.isCollapsed("battery", true)).toBe(true);
  });
});

describe("tab selection", () => {
  test("the default tab is the console and an unknown id is ignored", () => {
    const state = new TabState(TABS);
    expect(state.active).toBe("console");
    expect(state.select("nope")).toBe(false);
    expect(state.active).toBe("console");
    expect(state.select("perf")).toBe(true);
    expect(state.active).toBe("perf");
  });

  test("stepping wraps in both directions", () => {
    const state = new TabState(TABS);
    expect(state.step(-1)).toBe("perf");
    expect(state.step(1)).toBe("console");
  });

  test("every tab and card is present exactly once", () => {
    const ids = TABS.map((tab) => tab.id);
    expect(ids).toEqual(["console", "ui-tree", "events", "inspect", "fidelity", "perf"]);
    expect(new Set(ids).size).toBe(ids.length);
  });
});
