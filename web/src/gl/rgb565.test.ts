import { describe, expect, test } from "bun:test";

import { abiCore, FRAME_BYTES } from "../worker/abiCoreFixture";
import { DEFAULT_BACKLIGHT_SCALE, FRAME_HEIGHT, FRAME_WIDTH } from "../worker/layout";
import { IoViews } from "../worker/ring";
import {
  LITTLE_ENDIAN,
  backlightGain,
  fillRgbaLut,
  integerScale,
  pack565,
  panelGain,
  shade,
  toRgba,
  unpack565,
  type PanelView,
} from "./rgb565";

/**
 * The documented expansion, written the way the fragment shader writes it (`code * 8 +
 * floor(code / 4)`) rather than with shifts, so the test is not the implementation read twice.
 */
function reference(pixel: number): [number, number, number] {
  const r5 = Math.floor(pixel / 2048) % 32;
  const g6 = Math.floor(pixel / 32) % 64;
  const b5 = pixel % 32;
  return [r5 * 8 + Math.floor(r5 / 4), g6 * 4 + Math.floor(g6 / 16), b5 * 8 + Math.floor(b5 / 4)];
}

const LIT: PanelView = {
  backlight: DEFAULT_BACKLIGHT_SCALE,
  backlightScale: DEFAULT_BACKLIGHT_SCALE,
  glassComplement: false,
  powered: true,
  sleeping: false,
  displayOn: true,
};

describe("unpacking RGB565", () => {
  test("matches the documented bit replication for all 65,536 values", () => {
    let mismatches = 0;
    for (let pixel = 0; pixel < 65536; pixel += 1) {
      const { r, g, b } = unpack565(pixel);
      const [er, eg, eb] = reference(pixel);
      if (r !== er || g !== eg || b !== eb) {
        mismatches += 1;
      }
    }
    expect(mismatches).toBe(0);
  });

  test("differs from round(c * 255 / max) on exactly the 14 codes the renderer documents", () => {
    const red = [];
    for (let c = 0; c < 32; c += 1) {
      if (reference(c << 11)[0] !== Math.round((c * 255) / 31)) red.push(c);
    }
    const green = [];
    for (let c = 0; c < 64; c += 1) {
      if (reference(c << 5)[1] !== Math.round((c * 255) / 63)) green.push(c);
    }
    expect(red).toEqual([3, 7, 24, 28]);
    expect(green).toEqual([11, 12, 13, 14, 15, 48, 49, 50, 51, 52]);
  });

  test("takes black to black and white to white", () => {
    expect(unpack565(0x0000)).toEqual({ r: 0, g: 0, b: 0 });
    expect(unpack565(0xffff)).toEqual({ r: 255, g: 255, b: 255 });
  });

  test("puts each channel in its own field", () => {
    expect(unpack565(0xf800)).toEqual({ r: 255, g: 0, b: 0 });
    expect(unpack565(0x07e0)).toEqual({ r: 0, g: 255, b: 0 });
    expect(unpack565(0x001f)).toEqual({ r: 0, g: 0, b: 255 });
  });

  test("fills the low bits by repeating the high ones, so mid grey stays mid grey", () => {
    const { r, g, b } = unpack565(pack565(128, 128, 128));
    for (const channel of [r, g, b]) {
      expect(Math.abs(channel - 128)).toBeLessThanOrEqual(4);
    }
  });

  test("round-trips every value a 5-bit channel can hold", () => {
    for (let value = 0; value < 32; value += 1) {
      const pixel = (value << 11) | value;
      const { r, b } = unpack565(pixel);
      expect((r >> 3) & 0x1f).toBe(value);
      expect((b >> 3) & 0x1f).toBe(value);
    }
  });
});

describe("the backlight", () => {
  test("is the documented integer pair", () => {
    expect(backlightGain(0, 1024)).toBe(0);
    expect(backlightGain(512, 1024)).toBe(0.5);
    expect(backlightGain(1024, 1024)).toBe(1);
  });

  test("clamps a duty above its scale rather than over-brightening", () => {
    expect(backlightGain(2048, 1024)).toBe(1);
    expect(backlightGain(-5, 1024)).toBe(0);
  });

  test("treats an unmodelled resolution as fully lit, never as dark", () => {
    expect(backlightGain(0, 0)).toBe(1);
    expect(backlightGain(700, 0)).toBe(1);
  });
});

describe("the backlight formula", () => {
  test("rounds half up, the same way the shader's floor(x + 0.5) does", () => {
    expect(shade(255, 0.5)).toBe(128);
    expect(shade(1, 0.5)).toBe(1);
    expect(shade(254, 0.5)).toBe(127);
    expect(shade(200, 0)).toBe(0);
    expect(shade(200, 1)).toBe(200);
  });

  test("is dark whenever the panel is unpowered or asleep, whatever the duty", () => {
    expect(panelGain({ ...LIT, powered: false })).toBe(0);
    expect(panelGain({ ...LIT, sleeping: true })).toBe(0);
    expect(panelGain({ ...LIT, displayOn: false })).toBe(0);
    expect(panelGain({ ...LIT, backlight: 256 })).toBe(0.25);
  });
});

describe("the 65,536-entry LUT", () => {
  const states: [string, PanelView][] = [
    ["lit", LIT],
    ["inverted", { ...LIT, glassComplement: true }],
    ["dimmed to 3/1024", { ...LIT, backlight: 3 }],
    ["inverted at half", { ...LIT, glassComplement: true, backlight: 512 }],
    ["asleep", { ...LIT, sleeping: true }],
  ];
  for (const [name, panel] of states) {
    test(`agrees with toRgba for every pixel (${name})`, () => {
      const pixels = new Uint16Array(65536);
      for (let index = 0; index < 65536; index += 1) pixels[index] = index;
      const expected = new Uint8ClampedArray(65536 * 4);
      toRgba(pixels, panel, expected);
      const lut = new Uint32Array(65536);
      fillRgbaLut(lut, panel, true);
      const actual = new Uint8ClampedArray(lut.buffer);
      let mismatches = 0;
      for (let index = 0; index < expected.length; index += 1) {
        if (actual[index] !== expected[index]) mismatches += 1;
      }
      expect(mismatches).toBe(0);
    });
  }

  test("packs big-endian words with red in the top byte", () => {
    const lut = new Uint32Array(65536);
    fillRgbaLut(lut, LIT, false);
    expect(lut[0xf800]).toBe(0xff0000ff);
    expect(lut[0x001f]).toBe(0x0000ffff);
  });
});

// A `Uint16Array` over wasm memory must read the core's stored bytes as the documented word.
describe("pixels in real wasm memory", () => {
  test("reads the little-endian bytes 00 F8 as 0xF800 through the Worker's views", async () => {
    const { instance } = await WebAssembly.instantiate(abiCore());
    const exports = instance.exports as unknown as {
      memory: WebAssembly.Memory;
      pemu_io_layout: (handle: number) => number;
    };
    const views = IoViews.of(exports.memory, () => exports.pemu_io_layout(7));
    expect(Array.from(new Uint8Array(exports.memory.buffer, views.pixels.byteOffset, 4))).toEqual(
      Array.from(FRAME_BYTES),
    );
    expect(views.pixels[0]).toBe(0xf800);
    expect(views.pixels[1]).toBe(0x145d);
    expect(unpack565(views.pixels[0] ?? 0)).toEqual({ r: 255, g: 0, b: 0 });
    expect(LITTLE_ENDIAN).toBe(true);
  });
});

describe("converting a frame to RGBA", () => {
  test("applies the backlight as a multiplier", () => {
    const pixels = Uint16Array.from([0xffff]);
    const out = new Uint8ClampedArray(4);
    toRgba(pixels, { ...LIT, backlight: DEFAULT_BACKLIGHT_SCALE / 2 }, out);
    expect(out[0]).toBe(128);
    expect(out[3]).toBe(255);
  });

  test("shows nothing while the panel is unpowered or asleep", () => {
    const pixels = Uint16Array.from([0xffff]);
    const out = new Uint8ClampedArray(4);
    toRgba(pixels, { ...LIT, powered: false }, out);
    expect(Array.from(out)).toEqual([0, 0, 0, 255]);
    toRgba(pixels, { ...LIT, sleeping: true }, out);
    expect(Array.from(out)).toEqual([0, 0, 0, 255]);
  });

  test("shows black in DISPOFF, whatever memory and the backlight hold", () => {
    const out = new Uint8ClampedArray(4);
    toRgba(Uint16Array.from([0xffff]), { ...LIT, displayOn: false }, out);
    expect(Array.from(out)).toEqual([0, 0, 0, 255]);
  });

  test("shows the sky 0x145D as itself when the glass shows memory (INVON, invon_shows_ram)", () => {
    const out = new Uint8ClampedArray(4);
    toRgba(Uint16Array.from([0x145d]), { ...LIT, glassComplement: false }, out);
    expect(Array.from(out)).toEqual([16, 138, 239, 255]);
  });

  test("inverts every channel when the glass shows the complement", () => {
    const out = new Uint8ClampedArray(4);
    toRgba(Uint16Array.from([0x0000]), { ...LIT, glassComplement: true }, out);
    expect(Array.from(out).slice(0, 3)).toEqual([255, 255, 255]);
  });

  test("converts a whole panel without leaving a pixel untouched", () => {
    const pixels = new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT).fill(0x07e0);
    const out = new Uint8ClampedArray(pixels.length * 4);
    toRgba(pixels, LIT, out);
    expect(out[0]).toBe(0);
    expect(out[1]).toBe(255);
    expect(out[out.length - 1]).toBe(255);
  });
});

describe("integer scaling", () => {
  test("picks the largest whole multiple that fits", () => {
    expect(integerScale(240, 320, 480, 640)).toBe(2);
    expect(integerScale(240, 320, 470, 640)).toBe(1);
    expect(integerScale(240, 320, 1000, 1000)).toBe(3);
  });

  test("never goes below one, however small the box", () => {
    expect(integerScale(240, 320, 10, 10)).toBe(1);
    expect(integerScale(240, 320, 0, 0)).toBe(1);
  });
});
