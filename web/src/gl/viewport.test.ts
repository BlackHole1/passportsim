import { describe, expect, test } from "bun:test";

import { panelViewport, ViewportCache } from "./viewport";

describe("the panel viewport", () => {
  test("fills a canvas of exactly the panel size at scale 1", () => {
    expect(panelViewport(240, 320, 240, 320)).toEqual({
      scale: 1,
      x: 0,
      y: 0,
      width: 240,
      height: 320,
      glY: 0,
    });
  });

  test("takes the largest whole factor and centres the panel", () => {
    const view = panelViewport(240, 320, 800, 700);
    expect(view.scale).toBe(2);
    expect(view.width).toBe(480);
    expect(view.height).toBe(640);
    expect(view.x).toBe(160);
    expect(view.y).toBe(30);
    expect(view.glY).toBe(30);
  });

  test("never scales by a fraction, even one pixel short of the next factor", () => {
    expect(panelViewport(240, 320, 719, 960).scale).toBe(2);
    expect(panelViewport(240, 320, 720, 959).scale).toBe(2);
    expect(panelViewport(240, 320, 720, 960).scale).toBe(3);
  });

  test("puts an odd leftover pixel right and below, and counts glY from the bottom", () => {
    const view = panelViewport(240, 320, 241, 323);
    expect(view.x).toBe(0);
    expect(view.y).toBe(1);
    // 323 - 1 - 320 = 2 rows below the panel.
    expect(view.glY).toBe(2);
  });

  test("keeps scale 1 and crops evenly in a canvas smaller than the panel", () => {
    const view = panelViewport(240, 320, 200, 300);
    expect(view.scale).toBe(1);
    expect(view.x).toBe(-20);
    expect(view.y).toBe(-10);
    expect(view.glY).toBe(-10);
  });

  test("every factor from 1 to 8 lands on whole device pixels", () => {
    for (let factor = 1; factor <= 8; factor += 1) {
      const view = panelViewport(240, 320, 240 * factor + 13, 320 * factor + 7);
      expect(view.scale).toBe(factor);
      expect(Number.isInteger(view.x) && Number.isInteger(view.y)).toBe(true);
      expect(view.width % 240).toBe(0);
      expect(view.height / view.width).toBe(320 / 240);
    }
  });
});

describe("the viewport cache", () => {
  test("returns the same object while the canvas size holds, so a draw allocates nothing", () => {
    const cache = new ViewportCache(240, 320);
    const first = cache.get(480, 640);
    expect(cache.get(480, 640)).toBe(first);
    const resized = cache.get(720, 960);
    expect(resized).not.toBe(first);
    expect(resized.scale).toBe(3);
  });
});
