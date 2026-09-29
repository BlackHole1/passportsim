import { describe, expect, test } from "bun:test";

import {
  ABI_VERSION,
  assertAbiVersion,
  CURSOR_SLOTS,
  CursorSlot,
  FRAME_HEIGHT,
  FRAME_WIDTH,
  InputKind,
  IoLayout,
  LoadKind,
  RING_COUNT,
  RingId,
  RingLayout,
  StopCode,
  isLimitStop,
  ringHeadSlot,
  ringTailSlot,
} from "./layout";

describe("the ABI version check", () => {
  test("accepts the version this bundle was generated against", () => {
    expect(() => assertAbiVersion(ABI_VERSION)).not.toThrow();
  });

  test("refuses any other version and says how to fix it", () => {
    expect(() => assertAbiVersion(ABI_VERSION + 1)).toThrow(/does not match/);
    expect(() => assertAbiVersion(ABI_VERSION + 1)).toThrow(/cargo xtask wasm/);
    expect(() => assertAbiVersion(0)).toThrow();
  });
});

describe("the generated layout", () => {
  test("gives every ring two cursor cells of its own, below the extra cells", () => {
    const used = new Set<number>();
    for (const ring of Object.values(RingId)) {
      const head = ringHeadSlot(ring);
      const tail = ringTailSlot(ring);
      expect(head).toBe(ring * 2);
      expect(tail).toBe(ring * 2 + 1);
      expect(used.has(head)).toBe(false);
      expect(used.has(tail)).toBe(false);
      used.add(head);
      used.add(tail);
      expect(tail).toBeLessThan(CursorSlot.FrameGeneration);
    }
    expect(used.size).toBe(RING_COUNT * 2);
    expect(CURSOR_SLOTS).toBe(RING_COUNT * 2 + 4);
    expect(CursorSlot.NowPs as number).toBe(CURSOR_SLOTS - 1);
  });

  test("sizes the layout block from the ring count", () => {
    expect(IoLayout.SIZE as number).toBe(IoLayout.RINGS + RING_COUNT * RingLayout.SIZE);
    expect(IoLayout.RINGS).toBeGreaterThan(IoLayout.FRAME_DIRTY_LAST);
  });

  test("keeps the panel at the ST7789P3 size", () => {
    expect([FRAME_WIDTH, FRAME_HEIGHT]).toEqual([240, 320]);
  });

  test("numbers the two run limits apart from every other stop", () => {
    expect(isLimitStop(StopCode.Until)).toBe(true);
    expect(isLimitStop(StopCode.MaxInsns)).toBe(true);
    for (const stop of Object.values(StopCode)) {
      if (stop !== StopCode.Until && stop !== StopCode.MaxInsns) {
        expect(isLimitStop(stop)).toBe(false);
      }
    }
  });

  test("numbers the load kinds as the core ABI does", () => {
    expect(LoadKind.RomElf).toBe(0);
    expect(LoadKind.MergedFlash).toBe(1);
    expect(LoadKind.AppElf).toBe(2);
    expect(LoadKind.BootloaderElf).toBe(3);
    expect(LoadKind.Efuse).toBe(4);
  });

  test("numbers the input kinds densely from zero", () => {
    const numbers: number[] = Object.values(InputKind).sort((a, b) => a - b);
    expect(numbers).toEqual(numbers.map((_, index) => index));
  });
});
