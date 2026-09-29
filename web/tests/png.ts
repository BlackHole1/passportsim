// Pixel helpers for comparing a frame with a committed golden.

import { inflateSync } from "node:zlib";

/**
 * The RGB888 pixels of an 8-bit, non-interlaced RGB (colour type 2) or RGBA (6) PNG, unfiltered
 * (filters 0 to 4); alpha is dropped. `pemu_host::png` writes the goldens as RGB.
 */
export function decodeRgbPng(png: Buffer): { width: number; height: number; rgb: Uint8Array } {
  let at = 8;
  let width = 0;
  let height = 0;
  let samples = 3;
  const idat: Buffer[] = [];
  while (at < png.length) {
    const len = png.readUInt32BE(at);
    const type = png.toString("ascii", at + 4, at + 8);
    const data = png.subarray(at + 8, at + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      if (data[8] !== 8 || data[12] !== 0) {
        throw new Error(`a PNG of ${data[8]} bits per sample, interlace ${data[12]}: only 8 and none are read`);
      }
      if (data[9] !== 2 && data[9] !== 6) {
        throw new Error(`a PNG of colour type ${data[9]}: only RGB and RGBA are read`);
      }
      samples = data[9] === 6 ? 4 : 3;
    } else if (type === "IDAT") {
      idat.push(data);
    }
    at += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * samples;
  const all = new Uint8Array(stride * height);
  for (let y = 0; y < height; y += 1) {
    const filter = raw[y * (stride + 1)] ?? 0;
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    for (let x = 0; x < stride; x += 1) {
      const a = x >= samples ? (all[y * stride + x - samples] ?? 0) : 0;
      const b = y > 0 ? (all[(y - 1) * stride + x] ?? 0) : 0;
      const c = x >= samples && y > 0 ? (all[(y - 1) * stride + x - samples] ?? 0) : 0;
      let predictor = 0;
      if (filter === 1) {
        predictor = a;
      } else if (filter === 2) {
        predictor = b;
      } else if (filter === 3) {
        predictor = (a + b) >> 1;
      } else if (filter === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a);
        const pb = Math.abs(p - b);
        const pc = Math.abs(p - c);
        predictor = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      all[y * stride + x] = ((line[x] ?? 0) + predictor) & 0xff;
    }
  }
  if (samples === 3) {
    return { width, height, rgb: all };
  }
  const rgb = new Uint8Array(width * height * 3);
  for (let pixel = 0; pixel < width * height; pixel += 1) {
    rgb[pixel * 3] = all[pixel * 4] ?? 0;
    rgb[pixel * 3 + 1] = all[pixel * 4 + 1] ?? 0;
    rgb[pixel * 3 + 2] = all[pixel * 4 + 2] ?? 0;
  }
  return { width, height, rgb };
}

/** `pemu_host::png::rgb565_to_rgb888`: bit replication, so white is 0xFFFFFF. */
export function expand565(pixel: number): [number, number, number] {
  const r5 = (pixel >> 11) & 0x1f;
  const g6 = (pixel >> 5) & 0x3f;
  const b5 = pixel & 0x1f;
  return [(r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2)];
}

export function differingPixels(
  frame: { readonly width: number; readonly height: number; readonly pixels: ArrayLike<number> },
  golden: { readonly width: number; readonly height: number; readonly rgb: Uint8Array },
): { differing: number; first: [number, number] | null } {
  if (frame.width !== golden.width || frame.height !== golden.height) {
    return { differing: golden.width * golden.height, first: [0, 0] };
  }
  let differing = 0;
  let first: [number, number] | null = null;
  for (let index = 0; index < frame.width * frame.height; index += 1) {
    const [r, g, b] = expand565(frame.pixels[index] ?? 0);
    const o = index * 3;
    if (golden.rgb[o] !== r || golden.rgb[o + 1] !== g || golden.rgb[o + 2] !== b) {
      differing += 1;
      first ??= [index % frame.width, Math.floor(index / frame.width)];
    }
  }
  return { differing, first };
}
