// RGB565 arithmetic for the panel. The core publishes a row-major `Uint16Array` of RGB565 words in
// wasm memory (it swaps the big-endian SPI order once when storing), so on a little-endian host
// each pixel reads as one number: red in bits 15-11, green 10-5, blue 4-0. Expansion is bit
// replication, used by both renderers and every test:
//
//   r8 = (r5 << 3) | (r5 >> 2)      g8 = (g6 << 2) | (g6 >> 4)      b8 = (b5 << 3) | (b5 >> 2)
//
// It differs from `round(c * 255 / max)` on 14 codes (red and blue 3, 7, 24, 28; green 11 to 15 and
// 48 to 52). Then the glass complement flips the 16-bit word before expansion, and the backlight is
// `floor(c8 * gain + 0.5)`, gain 0 while unpowered, asleep or in DISPOFF. The complement is the
// core's `GlassComplement` bit, never the bare INVON state.

import { DEFAULT_BACKLIGHT_SCALE } from "../worker/layout";

export interface Rgb {
  readonly r: number;
  readonly g: number;
  readonly b: number;
}

/** Unpacks one RGB565 pixel by bit replication, so 0x1F maps to 255 and white reads as white. */
export function unpack565(pixel: number): Rgb {
  const r5 = (pixel >> 11) & 0x1f;
  const g6 = (pixel >> 5) & 0x3f;
  const b5 = pixel & 0x1f;
  return {
    r: (r5 << 3) | (r5 >> 2),
    g: (g6 << 2) | (g6 >> 4),
    b: (b5 << 3) | (b5 >> 2),
  };
}

export function pack565(r: number, g: number, b: number): number {
  const r5 = (clampByte(r) >> 3) & 0x1f;
  const g6 = (clampByte(g) >> 2) & 0x3f;
  const b5 = (clampByte(b) >> 3) & 0x1f;
  return (r5 << 11) | (g6 << 5) | b5;
}

/**
 * The brightness multiplier of `(duty >> 4, 1 << duty_res)`. A scale of 0 means the resolution is
 * not modelled yet, and the panel is lit fully rather than dark.
 */
export function backlightGain(duty: number, scale: number = DEFAULT_BACKLIGHT_SCALE): number {
  if (scale <= 0) {
    return 1;
  }
  return Math.min(1, Math.max(0, duty / scale));
}

export interface PanelView {
  readonly backlight: number;
  readonly backlightScale: number;
  /** The glass shows the complement of panel memory (INVON disagrees with `invon_shows_ram`); not bare INVON. */
  readonly glassComplement: boolean;
  /** The panel rail; off means black whatever the framebuffer holds. */
  readonly powered: boolean;
  readonly sleeping: boolean;
  /** DISPON; DISPOFF blanks the glass and keeps panel memory. */
  readonly displayOn: boolean;
}

/** 0 unless the glass is lit (powered, out of sleep, DISPON), else the backlight gain. */
export function panelGain(panel: PanelView): number {
  return !panel.powered || panel.sleeping || !panel.displayOn
    ? 0
    : backlightGain(panel.backlight, panel.backlightScale);
}

export function shade(c8: number, gain: number): number {
  return Math.floor(c8 * gain + 0.5);
}

/**
 * Whether this host stores a `Uint32Array` little-endian, which decides how a LUT packs RGBA. The
 * pixel view assumes a little-endian host too, which every supported host is; a big-endian host
 * would need the pixel view swapped.
 */
export const LITTLE_ENDIAN = new Uint8Array(new Uint32Array([1]).buffer)[0] === 1;

/**
 * Fills the 65,536-entry `lut` with the RGBA8 word of each pixel under `panel`, packed for a
 * `Uint32Array` over `ImageData`. Complement and gain are baked in; rebuild only when they change.
 */
export function fillRgbaLut(lut: Uint32Array, panel: PanelView, littleEndian = LITTLE_ENDIAN): void {
  const gain = panelGain(panel);
  const invert = panel.glassComplement;
  for (let pixel = 0; pixel < 65536; pixel += 1) {
    const word = invert ? ~pixel & 0xffff : pixel;
    const r5 = (word >> 11) & 0x1f;
    const g6 = (word >> 5) & 0x3f;
    const b5 = word & 0x1f;
    const r = shade((r5 << 3) | (r5 >> 2), gain);
    const g = shade((g6 << 2) | (g6 >> 4), gain);
    const b = shade((b5 << 3) | (b5 >> 2), gain);
    lut[pixel] = littleEndian
      ? ((0xff << 24) | (b << 16) | (g << 8) | r) >>> 0
      : ((r << 24) | (g << 16) | (b << 8) | 0xff) >>> 0;
  }
}

/** Converts `pixels` to RGBA8 under `panel`: the slow, obviously correct form of {@link fillRgbaLut}. */
export function toRgba(pixels: Uint16Array, panel: PanelView, out: Uint8ClampedArray): void {
  const gain = panelGain(panel);
  for (let index = 0; index < pixels.length; index += 1) {
    const raw = pixels[index] ?? 0;
    const pixel = panel.glassComplement ? ~raw & 0xffff : raw;
    const { r, g, b } = unpack565(pixel);
    const at = index * 4;
    out[at] = shade(r, gain);
    out[at + 1] = shade(g, gain);
    out[at + 2] = shade(b, gain);
    out[at + 3] = 255;
  }
}

/** The largest integer scale at which the panel fits the box, never below 1. */
export function integerScale(
  width: number,
  height: number,
  boxWidth: number,
  boxHeight: number,
): number {
  if (width <= 0 || height <= 0) {
    return 1;
  }
  return Math.max(1, Math.floor(Math.min(boxWidth / width, boxHeight / height)));
}

function clampByte(value: number): number {
  return Math.min(255, Math.max(0, Math.round(value)));
}
