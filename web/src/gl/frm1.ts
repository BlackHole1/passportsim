// Reads the daemon's `FRM1` WebSocket frames (`crates/pemu-host/src/ws.rs`) for the renderers.
// Bit 2 `FLAG_INVERTED` is the ST7789 INVON state, a status-line fact, never a drawing rule: the
// official menu sends INVON and its sky 0x145D still shows as 0x145D. The glass complement is bit 4,
// and the glass is black unless bit 5 (powered) and bit 1 (DISPON) are set and bit 3 (sleep) is
// clear. These bits differ from the wasm `FRAME_FLAGS` word, so the mapping is by name.

import type { PanelView } from "./rgb565";

export const FRM1_MAGIC = "FRM1";
export const FRM1_HEADER_BYTES = 28;

export const Frm1Flag = {
  FullFrame: 1 << 0,
  DisplayOn: 1 << 1,
  Inverted: 1 << 2,
  SleepIn: 1 << 3,
  GlassComplement: 1 << 4,
  Powered: 1 << 5,
} as const;

/**
 * The backlight denominator of an `FRM1` header: the daemon sends `duty >> 4` clamped to 10 bits
 * (`crates/pemu-host/src/hub.rs`) and no resolution, so 1024 is assumed (unverified for other LEDC
 * resolutions).
 */
export const FRM1_BACKLIGHT_SCALE = 1024;

export interface Frm1Header {
  readonly frameNo: number;
  readonly vtNs: bigint;
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
  readonly format: 1 | 2;
  readonly flags: number;
  readonly backlight: number;
}

export interface Frm1Frame {
  readonly header: Frm1Header;
  /** Drawing state: the glass complement from bit 4 only. */
  readonly panel: PanelView;
  readonly inverted: boolean;
  readonly rgb565: Uint16Array | null;
  readonly payload: Uint8Array;
}

export function decodeFrm1(bytes: Uint8Array): Frm1Frame {
  if (bytes.length < FRM1_HEADER_BYTES) {
    throw new Error(`a binary frame of ${bytes.length} bytes is shorter than its ${FRM1_HEADER_BYTES}-byte header`);
  }
  const magic = String.fromCharCode(bytes[0] ?? 0, bytes[1] ?? 0, bytes[2] ?? 0, bytes[3] ?? 0);
  if (magic !== FRM1_MAGIC) {
    throw new Error(`\`${magic}\` is not an FRM1 frame`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const format = view.getUint8(24);
  if (format !== 1 && format !== 2) {
    throw new Error(`unknown FRM1 pixel format ${format}`);
  }
  const header: Frm1Header = {
    frameNo: view.getUint32(4, true),
    vtNs: view.getBigUint64(8, true),
    x: view.getUint16(16, true),
    y: view.getUint16(18, true),
    width: view.getUint16(20, true),
    height: view.getUint16(22, true),
    format,
    flags: view.getUint8(25),
    backlight: view.getUint16(26, true),
  };
  const payload = bytes.subarray(FRM1_HEADER_BYTES);
  const want = header.width * header.height * (format === 1 ? 2 : 3);
  if (payload.length !== want) {
    throw new Error(`the header describes ${want} payload bytes and the frame carries ${payload.length}`);
  }
  let rgb565: Uint16Array | null = null;
  if (format === 1) {
    // Little-endian explicitly: the payload is a wire format, not host memory.
    rgb565 = new Uint16Array(header.width * header.height);
    const pixels = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
    for (let index = 0; index < rgb565.length; index += 1) {
      rgb565[index] = pixels.getUint16(index * 2, true);
    }
  }
  return { header, panel: frm1Panel(header), inverted: (header.flags & Frm1Flag.Inverted) !== 0, rgb565, payload };
}

export function frm1Panel(header: Pick<Frm1Header, "flags" | "backlight">): PanelView {
  const has = (bit: number) => (header.flags & bit) !== 0;
  return {
    backlight: header.backlight,
    backlightScale: FRM1_BACKLIGHT_SCALE,
    glassComplement: has(Frm1Flag.GlassComplement),
    powered: has(Frm1Flag.Powered),
    sleeping: has(Frm1Flag.SleepIn),
    displayOn: has(Frm1Flag.DisplayOn),
  };
}
