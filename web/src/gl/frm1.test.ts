// Frames built byte for byte as `crates/pemu-host/src/ws.rs` `FrameHeader::to_bytes` writes them.

import { describe, expect, test } from "bun:test";
import { FRM1_HEADER_BYTES, Frm1Flag, decodeFrm1 } from "./frm1";
import { toRgba } from "./rgb565";

function frame(flags: number, pixels: number[], w: number, h: number, backlight = 1023, x = 3, y = 5): Uint8Array {
  const out = new Uint8Array(FRM1_HEADER_BYTES + pixels.length * 2);
  out.set([0x46, 0x52, 0x4d, 0x31], 0);
  const view = new DataView(out.buffer);
  view.setUint32(4, 7, true);
  view.setBigUint64(8, 3_000_000_000n, true);
  view.setUint16(16, x, true);
  view.setUint16(18, y, true);
  view.setUint16(20, w, true);
  view.setUint16(22, h, true);
  view.setUint8(24, 1);
  view.setUint8(25, flags);
  view.setUint16(26, backlight, true);
  pixels.forEach((pixel, index) => view.setUint16(FRM1_HEADER_BYTES + index * 2, pixel, true));
  return out;
}

const LIT = Frm1Flag.Powered | Frm1Flag.DisplayOn;
const SKY = [16, 138, 239, 255];

function drawn(bytes: Uint8Array): number[] {
  const decoded = decodeFrm1(bytes);
  const out = new Uint8ClampedArray((decoded.rgb565?.length ?? 0) * 4);
  toRgba(decoded.rgb565 ?? new Uint16Array(0), { ...decoded.panel, backlight: 1024 }, out);
  return Array.from(out.slice(0, 4));
}

describe("decodeFrm1", () => {
  test("an INVON frame of the sky 0x145D reaches the consumer as sky, not its complement", () => {
    // `ws.rs` an_invon_frame_of_the_sky_carries_no_glass_complement_under_invon_shows_ram.
    const decoded = decodeFrm1(frame(LIT | Frm1Flag.Inverted, [0x145d, 0x145d], 2, 1));
    expect(decoded.inverted).toBe(true);
    expect(decoded.panel.glassComplement).toBe(false);
    expect(Array.from(decoded.rgb565 ?? [])).toEqual([0x145d, 0x145d]);
    expect(drawn(frame(LIT | Frm1Flag.Inverted, [0x145d, 0x145d], 2, 1))).toEqual(SKY);
  });

  test("the complement is drawn exactly when bit 4 is set, whatever INVON says", () => {
    expect(drawn(frame(LIT | Frm1Flag.GlassComplement, [0x0000], 1, 1)).slice(0, 3)).toEqual([255, 255, 255]);
    expect(drawn(frame(LIT | Frm1Flag.Inverted | Frm1Flag.GlassComplement, [0x0000], 1, 1)).slice(0, 3)).toEqual([255, 255, 255]);
    expect(drawn(frame(LIT, [0x145d], 1, 1))).toEqual(SKY);
  });

  test("the glass is black unless powered, DISPON and out of sleep; bits map by name, not by the wasm word", () => {
    expect(drawn(frame(Frm1Flag.DisplayOn, [0xffff], 1, 1)).slice(0, 3)).toEqual([0, 0, 0]);
    expect(drawn(frame(Frm1Flag.Powered, [0xffff], 1, 1)).slice(0, 3)).toEqual([0, 0, 0]);
    expect(drawn(frame(LIT | Frm1Flag.SleepIn, [0xffff], 1, 1)).slice(0, 3)).toEqual([0, 0, 0]);
    // FRM1 bit 0 is the full-frame bit; in the wasm FRAME_FLAGS word bit 0 is powered.
    const full = decodeFrm1(frame(Frm1Flag.FullFrame, [0xffff], 1, 1));
    expect(full.panel.powered).toBe(false);
    expect(full.panel.displayOn).toBe(false);
  });

  test("reads the header fields little-endian and the backlight over its 10-bit scale", () => {
    const decoded = decodeFrm1(frame(LIT, [0x1234, 0x5678, 0x9abc, 0xdef0], 2, 2, 512, 10, 20));
    expect(decoded.header).toEqual({
      frameNo: 7,
      vtNs: 3_000_000_000n,
      x: 10,
      y: 20,
      width: 2,
      height: 2,
      format: 1,
      flags: LIT,
      backlight: 512,
    });
    expect(decoded.panel.backlight / decoded.panel.backlightScale).toBe(0.5);
    expect(Array.from(decoded.rgb565 ?? [])).toEqual([0x1234, 0x5678, 0x9abc, 0xdef0]);
  });

  test("refuses a short frame, another magic and a payload of the wrong length", () => {
    expect(() => decodeFrm1(new Uint8Array(10))).toThrow("shorter than its 28-byte header");
    const aud = frame(LIT, [0], 1, 1);
    aud.set([0x41, 0x55, 0x44, 0x31], 0);
    expect(() => decodeFrm1(aud)).toThrow("not an FRM1 frame");
    expect(() => decodeFrm1(frame(LIT, [0, 0, 0], 2, 1))).toThrow("describes 4 payload bytes");
  });
});
