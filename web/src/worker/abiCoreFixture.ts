// A hand-assembled wasm core for tests that need a real module behind the ABI without the real
// machine. It answers only what booting touches.

import { ABI_VERSION, CURSOR_SLOTS, FRAME_HEIGHT, FRAME_WIDTH, IoLayout, ResultHeader } from "./layout";

/** Unsigned LEB128. */
function uleb(value: number): number[] {
  const out: number[] = [];
  do {
    let byte = value & 0x7f;
    value >>>= 7;
    if (value !== 0) byte |= 0x80;
    out.push(byte);
  } while (value !== 0);
  return out;
}

/** Signed LEB128, for `i32.const` and `i64.const` operands. */
function sleb(value: bigint): number[] {
  const out: number[] = [];
  for (;;) {
    const byte = Number(value & 0x7fn);
    value >>= 7n;
    const done = (value === 0n && (byte & 0x40) === 0) || (value === -1n && (byte & 0x40) !== 0);
    out.push(done ? byte : byte | 0x80);
    if (done) return out;
  }
}

const vec = (items: number[][]): number[] => [...uleb(items.length), ...items.flat()];
const section = (id: number, body: number[]): number[] => [id, ...uleb(body.length), ...body];
const name = (text: string): number[] => [...uleb(text.length), ...new TextEncoder().encode(text)];

const I32 = 0x7f;
const I64 = 0x7e;
const HEADER_AT = 256;
const HANDLE = 7;
const LAYOUT_AT = 4096;
const CURSORS_AT = 8192;
/** Where the frame buffer lives: past the first page, which is too small for 240x320 words. */
const FRAME_AT = 65536;
/** The first frame bytes as a little-endian core stores them: `00 F8` is pixel 0, `5D 14` pixel 1. */
export const FRAME_BYTES = Uint8Array.from([0x00, 0xf8, 0x5d, 0x14]);
export const NOW_PS = 42n;

/**
 * A minimal core: the version check, alloc and free, `pemu_new`, a `pemu_build` whose result
 * header carries a handle, `pemu_now_ps`, and a `pemu_io_layout` with empty rings. Hand-assembled
 * because the real core needs cargo, which does not belong in `bun test`.
 */
export function abiCore(): Uint8Array<ArrayBuffer> {
  // [export name, params, results, body without the trailing `end`]
  const funcs: [string, number[], number[], number[]][] = [
    ["pemu_abi_version", [], [I32], [0x41, ...sleb(BigInt(ABI_VERSION))]],
    ["pemu_alloc", [I32], [I32], [0x41, ...sleb(1024n)]],
    ["pemu_free", [I32, I32], [], []],
    ["pemu_result_free", [I32], [], []],
    ["pemu_new", [I32, I32], [I32], [0x41, ...sleb(1n)]],
    ["pemu_build", [I32], [I32], [0x41, ...sleb(BigInt(HEADER_AT))]],
    ["pemu_now_ps", [I32], [I64], [0x42, ...sleb(NOW_PS)]],
    ["pemu_io_layout", [I32], [I32], [0x41, ...sleb(BigInt(LAYOUT_AT))]],
    ["pemu_drop", [I32], [], []],
  ];
  const payloadAt = HEADER_AT + ResultHeader.SIZE;
  const data = new Uint8Array(ResultHeader.SIZE + 4);
  const view = new DataView(data.buffer);
  view.setUint32(ResultHeader.PTR, payloadAt, true);
  view.setUint32(ResultHeader.LEN, 4, true);
  view.setUint32(ResultHeader.STATUS, 0, true);
  view.setUint32(ResultHeader.SIZE, HANDLE, true);

  const layout = new Uint8Array(IoLayout.SIZE);
  const layoutView = new DataView(layout.buffer);
  layoutView.setUint32(IoLayout.ABI_VERSION, ABI_VERSION, true);
  layoutView.setUint32(IoLayout.CURSORS_PTR, CURSORS_AT, true);
  layoutView.setUint32(IoLayout.CURSOR_SLOTS, CURSOR_SLOTS, true);
  layoutView.setUint32(IoLayout.FRAME_PTR, FRAME_AT, true);
  layoutView.setUint32(IoLayout.FRAME_WIDTH, FRAME_WIDTH, true);
  layoutView.setUint32(IoLayout.FRAME_HEIGHT, FRAME_HEIGHT, true);
  const segment = (at: number, bytes: Uint8Array): number[] => [
    0x00,
    0x41,
    ...sleb(BigInt(at)),
    0x0b,
    ...uleb(bytes.length),
    ...bytes,
  ];

  const bytes = [
    ...[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00],
    ...section(
      1,
      vec(funcs.map(([, p, r]) => [0x60, ...vec(p.map((t) => [t])), ...vec(r.map((t) => [t]))])),
    ),
    ...section(3, vec(funcs.map((_, i) => uleb(i)))),
    // Four pages: the layout blocks in the first, the 153,600-byte frame from the second.
    ...section(5, vec([[0x00, 0x04]])),
    ...section(
      7,
      vec([
        [...name("memory"), 0x02, 0x00],
        ...funcs.map(([n], i) => [...name(n), 0x00, ...uleb(i)]),
      ]),
    ),
    ...section(
      10,
      vec(funcs.map(([, , , body]) => [...uleb(body.length + 2), 0x00, ...body, 0x0b])),
    ),
    ...section(
      11,
      vec([segment(HEADER_AT, data), segment(LAYOUT_AT, layout), segment(FRAME_AT, FRAME_BYTES)]),
    ),
  ];
  return new Uint8Array(bytes);
}
