import { describe, expect, test } from "bun:test";

import { inflateIfGzip, isGzip } from "./gzip";

describe("a gzip-compressed firmware file", () => {
  test("is inflated to the bytes it holds", async () => {
    const plain = new TextEncoder().encode("PEBUNDL1".repeat(1000));
    const packed = Bun.gzipSync(plain);
    expect(isGzip(packed)).toBe(true);
    expect(await inflateIfGzip(packed)).toEqual(plain);
  });

  test("any other file is passed through as it is", async () => {
    const plain = new TextEncoder().encode("PEBUNDL1");
    expect(isGzip(plain)).toBe(false);
    expect(await inflateIfGzip(plain)).toBe(plain);
    expect(isGzip(new Uint8Array([0x1f]))).toBe(false);
  });

  test("a corrupt gzip stream is refused rather than read as a bundle", async () => {
    const packed = Bun.gzipSync(new TextEncoder().encode("PEBUNDL1".repeat(1000)));
    await expect(inflateIfGzip(packed.subarray(0, packed.length / 2))).rejects.toThrow();
  });
});
