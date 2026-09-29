// The card's curve, full cell and room temperature are read out of the Rust sources, so a change
// there fails here.

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { FULL_SOC, OCV_MV, ROOM_TEMP_C, ocvMv, socMilliAtOcv } from "./battery";

const BOARD = join(import.meta.dir, "..", "..", "..", "..", "crates", "pemu-board", "src");

function rustConst(file: string, name: string): string {
  const source = readFileSync(join(BOARD, file), "utf8");
  const match = new RegExp(`const ${name}: [^=]+= ([^;]+);`).exec(source);
  if (match === null) {
    throw new Error(`${file} has no const ${name}`);
  }
  return match[1]!.replace(/_/g, "").replace(/\s+/g, " ").trim();
}

describe("the battery card's cell is the board's", () => {
  test("the open-circuit curve is battery.rs OCV_MV", () => {
    const rust = rustConst("battery.rs", "OCV_MV")
      .replace(/^\[|,?\s*\]$/g, "")
      .split(",")
      .map((value) => Number(value.trim()));
    expect(OCV_MV).toEqual(rust);
  });

  test("a new board's cell is full and at room temperature", () => {
    expect(Number(rustConst("passport.rs", "FULL_SOC_MILLI"))).toBe(FULL_SOC * 1_000);
    expect(Number(rustConst("battery.rs", "ROOM_TEMP_DECI_C"))).toBe(ROOM_TEMP_C * 10);
  });

  test("the curve and its inverse agree on every grid point and between them", () => {
    for (let index = 0; index < OCV_MV.length; index += 1) {
      expect(ocvMv(index * 10_000)).toBe(OCV_MV[index]!);
    }
    // Inverting then evaluating lands on the voltage, to the curve's millivolt resolution.
    for (let mv = OCV_MV[0]! + 1; mv <= OCV_MV[OCV_MV.length - 1]!; mv += 7) {
      expect(Math.abs(ocvMv(socMilliAtOcv(mv)) - mv)).toBeLessThanOrEqual(1);
    }
  });
});
