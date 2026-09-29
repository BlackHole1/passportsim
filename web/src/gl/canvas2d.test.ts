import { describe, expect, test } from "bun:test";

import { FakeCore } from "../worker/fakeCore";
import { WasmCore } from "../worker/core";
import { DEFAULT_BACKLIGHT_SCALE, FRAME_HEIGHT, FRAME_WIDTH } from "../worker/layout";
import { EmulatorSession } from "../worker/session";
import { Canvas2dRenderer } from "./canvas2d";
import { Soft2d, SoftCanvas } from "./fake2d";
import { toRgba, unpack565, type PanelView } from "./rgb565";

const LIT: PanelView = {
  backlight: DEFAULT_BACKLIGHT_SCALE,
  backlightScale: DEFAULT_BACKLIGHT_SCALE,
  glassComplement: false,
  powered: true,
  sleeping: false,
  displayOn: true,
};

function syntheticFrame(): Uint16Array {
  const pixels = new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT);
  for (let y = 0; y < FRAME_HEIGHT; y += 1) {
    for (let x = 0; x < FRAME_WIDTH; x += 1) {
      pixels[y * FRAME_WIDTH + x] = ((x % 32) << 11) | ((y % 64) << 5) | ((x + y) % 32);
    }
  }
  return pixels;
}

function fallbackOn(canvasWidth: number, canvasHeight: number) {
  const visible = new SoftCanvas(canvasWidth, canvasHeight);
  const target = new Soft2d(visible);
  const scratchCanvas = new SoftCanvas(FRAME_WIDTH, FRAME_HEIGHT);
  const scratch = new Soft2d(scratchCanvas);
  const renderer = new Canvas2dRenderer(target, scratch, scratchCanvas, FRAME_WIDTH, FRAME_HEIGHT);
  return { visible, scratch, renderer };
}

function mismatches(visible: SoftCanvas, pixels: Uint16Array, panel: PanelView): number {
  const expected = new Uint8ClampedArray(pixels.length * 4);
  toRgba(pixels, panel, expected);
  const scale = Math.max(1, Math.floor(Math.min(visible.width / FRAME_WIDTH, visible.height / FRAME_HEIGHT)));
  const left = Math.floor((visible.width - FRAME_WIDTH * scale) / 2);
  const top = Math.floor((visible.height - FRAME_HEIGHT * scale) / 2);
  let bad = 0;
  for (let y = 0; y < visible.height; y += 1) {
    for (let x = 0; x < visible.width; x += 1) {
      const px = Math.floor((x - left) / scale);
      const py = Math.floor((y - top) / scale);
      const inside = x >= left && y >= top && px < FRAME_WIDTH && py < FRAME_HEIGHT;
      const at = (py * FRAME_WIDTH + px) * 4;
      const want = inside ? Array.from(expected.subarray(at, at + 4)) : [0, 0, 0, 255];
      if (visible.pixel(x, y).join() !== want.join()) {
        bad += 1;
      }
    }
  }
  return bad;
}

describe("the 2D fallback", () => {
  test("draws a synthetic frame pixel for pixel at scale 1", () => {
    const pixels = syntheticFrame();
    const { visible, renderer } = fallbackOn(FRAME_WIDTH, FRAME_HEIGHT);
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    expect(renderer.draw(LIT, FRAME_WIDTH, FRAME_HEIGHT).scale).toBe(1);
    expect(mismatches(visible, pixels, LIT)).toBe(0);
    const { r, g, b } = unpack565(pixels[5 * FRAME_WIDTH + 7] ?? 0);
    expect(visible.pixel(7, 5)).toEqual([r, g, b, 255]);
    // (31, 63): r5 = 31, g6 = 63, b5 = 94 % 32 = 30, and 30 expands to 240 + 7 = 247.
    expect(visible.pixel(31, 63)).toEqual([255, 255, 247, 255]);
  });

  test("scales by a whole factor, centred, with black around the panel", () => {
    const pixels = syntheticFrame();
    const { visible, renderer } = fallbackOn(500, 700);
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    const view = renderer.draw(LIT, 500, 700);
    expect([view.scale, view.x, view.y]).toEqual([2, 10, 30]);
    expect(mismatches(visible, pixels, LIT)).toBe(0);
  });

  test("applies inversion and the backlight exactly as rgb565.ts documents", () => {
    const pixels = syntheticFrame();
    const panel = { ...LIT, glassComplement: true, backlight: 300 };
    const { visible, renderer } = fallbackOn(FRAME_WIDTH, FRAME_HEIGHT);
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    renderer.draw(panel, FRAME_WIDTH, FRAME_HEIGHT);
    expect(mismatches(visible, pixels, panel)).toBe(0);
  });

  test("puts only the dirty rows, and every row again when the panel state changes", () => {
    const pixels = syntheticFrame();
    const { visible, scratch, renderer } = fallbackOn(FRAME_WIDTH, FRAME_HEIGHT);
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    renderer.draw(LIT, FRAME_WIDTH, FRAME_HEIGHT);
    pixels.fill(0xf800, 10 * FRAME_WIDTH, 13 * FRAME_WIDTH);
    renderer.upload(pixels, 10, 12);
    renderer.draw(LIT, FRAME_WIDTH, FRAME_HEIGHT);
    expect(scratch.puts.at(-1)).toEqual({ first: 10, rows: 3 });
    expect(visible.pixel(0, 11)).toEqual([255, 0, 0, 255]);
    expect(mismatches(visible, pixels, LIT)).toBe(0);

    const dim = { ...LIT, backlight: 512 };
    renderer.draw(dim, FRAME_WIDTH, FRAME_HEIGHT);
    expect(scratch.puts.at(-1)).toEqual({ first: 0, rows: FRAME_HEIGHT });
    expect(mismatches(visible, pixels, dim)).toBe(0);
  });

  // `gain * 2 + complement` is 1.5 for both states, so a combined key would reuse the wrong LUT.
  test("rebuilds the LUT for a state whose gain and complement both differ", () => {
    const pixels = syntheticFrame();
    const { visible, renderer } = fallbackOn(FRAME_WIDTH, FRAME_HEIGHT);
    const quarterComplement = {
      ...LIT,
      backlight: DEFAULT_BACKLIGHT_SCALE / 4,
      glassComplement: true,
    };
    const threeQuartersPlain = { ...LIT, backlight: (DEFAULT_BACKLIGHT_SCALE * 3) / 4 };
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    renderer.draw(quarterComplement, FRAME_WIDTH, FRAME_HEIGHT);
    expect(mismatches(visible, pixels, quarterComplement)).toBe(0);
    renderer.draw(threeQuartersPlain, FRAME_WIDTH, FRAME_HEIGHT);
    expect(mismatches(visible, pixels, threeQuartersPlain)).toBe(0);
  });

  test("shows black while the panel sleeps", () => {
    const { visible, renderer } = fallbackOn(FRAME_WIDTH, FRAME_HEIGHT);
    const pixels = new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT).fill(0xffff);
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    renderer.draw({ ...LIT, sleeping: true }, FRAME_WIDTH, FRAME_HEIGHT);
    expect(visible.pixel(120, 160)).toEqual([0, 0, 0, 255]);
  });

  test("turns the canvas on when the backlight comes up with no pixel written", async () => {
    const core = new FakeCore();
    const MS = 1_000_000_000n;
    core.load([
      {
        durationPs: 8n * MS,
        paint: { firstRow: 0, lastRow: FRAME_HEIGHT - 1, color: 0xffff },
        panel: { backlight: 0 },
      },
      { durationPs: 8n * MS, panel: { backlight: DEFAULT_BACKLIGHT_SCALE } },
    ]);
    const { visible, renderer } = fallbackOn(FRAME_WIDTH, FRAME_HEIGHT);
    let now = 0;
    const clock = { nowMs: () => now, sleep: async (ms: number) => void (now += ms) };
    const session = new EmulatorSession(
      {
        core: WasmCore.build(core, "{}"),
        renderer,
        canvas: { width: FRAME_WIDTH, height: FRAME_HEIGHT },
        nowMs: () => now,
      },
      clock,
      clock,
    );
    session.setMode({ kind: "Max" });
    await session.step();
    expect(visible.pixel(120, 160)).toEqual([0, 0, 0, 255]);
    now += 20;
    expect((await session.step()).report.presented).toBe(true);
    expect(visible.pixel(120, 160)).toEqual([255, 255, 255, 255]);
  });

  test("draws the frames the fake core paints, through the session", async () => {
    const core = new FakeCore();
    const MS = 1_000_000_000n;
    core.load([
      { durationPs: 8n * MS, paint: { firstRow: 0, lastRow: FRAME_HEIGHT - 1, color: 0x145d } },
      { durationPs: 8n * MS, paint: { firstRow: 40, lastRow: 41, color: 0xfec5 } },
    ]);
    const { visible, renderer } = fallbackOn(FRAME_WIDTH * 2, FRAME_HEIGHT * 2);
    let now = 0;
    const clock = { nowMs: () => now, sleep: async (ms: number) => void (now += ms) };
    const session = new EmulatorSession(
      {
        core: WasmCore.build(core, "{}"),
        renderer,
        canvas: { width: FRAME_WIDTH * 2, height: FRAME_HEIGHT * 2 },
        nowMs: () => now,
      },
      clock,
      clock,
    );
    session.setMode({ kind: "Max" });
    expect((await session.step()).report.presented).toBe(true);
    // The official init sends INVON; with the board's `invon_shows_ram` the glass shows memory
    // unmodified, so the sky is 0x145D, not its complement.
    expect(session.views.frame.inverted).toBe(true);
    expect(session.views.frame.glassComplement).toBe(false);
    const sky = unpack565(0x145d);
    expect(visible.pixel(0, 0)).toEqual([sky.r, sky.g, sky.b, 255]);
    now += 20;
    expect((await session.step()).report.presented).toBe(true);
    const yellow = unpack565(0xfec5);
    expect(visible.pixel(479, 81)).toEqual([yellow.r, yellow.g, yellow.b, 255]);
    expect(visible.pixel(479, 79)).toEqual([sky.r, sky.g, sky.b, 255]);
    expect(mismatches(visible, session.views.pixels, session.views.frame)).toBe(0);
  });
});
