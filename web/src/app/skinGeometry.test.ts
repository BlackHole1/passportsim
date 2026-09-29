import { describe, expect, test } from "bun:test";
import { PANEL_HEIGHT, PANEL_WIDTH } from "./scale";
import { CSS_PX_PER_MM, DEVICE_MM, EDGE_CONTROLS, PHOTO_PX, SCREEN_MM, boxPx } from "./skinGeometry";

describe("the photo", () => {
  test("spans 60 x 95 mm at one scale, and a CSS millimetre is 96/25.4 CSS pixels", () => {
    expect([DEVICE_MM.width, DEVICE_MM.height]).toEqual([60, 95]);
    expect(PHOTO_PX.width / PHOTO_PX.height).toBeCloseTo(DEVICE_MM.width / DEVICE_MM.height, 3);
    expect(CSS_PX_PER_MM).toBeCloseTo(3.7795, 4);
  });

  test("one scale sizes every length", () => {
    const screen = boxPx(SCREEN_MM, 5);
    expect(screen.left).toBeCloseTo(SCREEN_MM.x * 5, 6);
    expect(screen.height).toBeCloseTo(SCREEN_MM.height * 5, 6);
  });
});

describe("the glass", () => {
  test("has exactly the panel's 3:4 shape and lies inside the photo", () => {
    expect(SCREEN_MM.width / SCREEN_MM.height).toBeCloseTo(PANEL_WIDTH / PANEL_HEIGHT, 10);
    expect(SCREEN_MM.x > 0 && SCREEN_MM.y > 0).toBe(true);
    expect(SCREEN_MM.x + SCREEN_MM.width < DEVICE_MM.width && SCREEN_MM.y + SCREEN_MM.height < DEVICE_MM.height).toBe(true);
  });
});

describe("the side buttons", () => {
  test("UP, OK and DOWN are on the right edge top to bottom; POWER on the left", () => {
    const right = EDGE_CONTROLS.filter((control) => control.edge === "right");
    expect(right.map((control) => control.id)).toEqual(["up", "ok", "down"]);
    expect(EDGE_CONTROLS.filter((control) => control.edge === "left").map((control) => control.id)).toEqual(["power"]);
    const ys = right.map((control) => control.box.y);
    expect([...ys].sort((a, b) => a - b)).toEqual(ys);
  });

  test("each touches its edge of the photo, and none overlaps another", () => {
    for (const control of EDGE_CONTROLS) {
      const { x, y, width, height } = control.box;
      if (control.edge === "left") {
        expect(x).toBe(0);
      } else {
        expect(x + width).toBeCloseTo(DEVICE_MM.width, 1);
      }
      expect(y > 0 && y + height < DEVICE_MM.height).toBe(true);
    }
    const right = EDGE_CONTROLS.filter((control) => control.edge === "right");
    for (const [index, control] of right.entries()) {
      const next = right[index + 1];
      if (next !== undefined) {
        expect(control.box.y + control.box.height).toBeLessThan(next.box.y);
      }
    }
  });
});
