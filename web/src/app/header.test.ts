import { describe, expect, test } from "bun:test";
import {
  agentBanner,
  formatRealTimeFactor,
  formatVirtualTime,
  headerModel,
} from "./header";

const PS_PER_MS = 1_000_000_000n;

describe("formatVirtualTime", () => {
  test("the documented example renders as printed", () => {
    expect(formatVirtualTime(12_402n * PS_PER_MS)).toBe("12.402 s");
  });

  test("one millisecond of guest time is visible", () => {
    expect(formatVirtualTime(1n * PS_PER_MS)).toBe("0.001 s");
    expect(formatVirtualTime(0n)).toBe("0.000 s");
  });

  test("the unit never changes, so the field does not jump while it runs", () => {
    for (const ms of [0n, 9n, 999n, 1000n, 86_400_000n]) {
      expect(formatVirtualTime(ms * PS_PER_MS).endsWith(" s")).toBe(true);
    }
  });

  test("sub-millisecond picoseconds truncate rather than rounding up past the slice", () => {
    expect(formatVirtualTime(PS_PER_MS - 1n)).toBe("0.000 s");
  });

  test("a very long run stays exact, which a double would not", () => {
    const hours = 10_000_000n * PS_PER_MS;
    expect(formatVirtualTime(hours + 7n * PS_PER_MS)).toBe("10000.007 s");
  });
});

describe("formatRealTimeFactor", () => {
  test("it prints two decimals and an x", () => {
    expect(formatRealTimeFactor(1)).toBe("1.00x");
    expect(formatRealTimeFactor(0.375)).toBe("0.38x");
  });

  test("slow motion is shown honestly rather than clamped to 1.00", () => {
    expect(formatRealTimeFactor(0.1)).toBe("0.10x");
  });

  test("an unmeasured factor is not printed as zero", () => {
    expect(formatRealTimeFactor(Number.NaN)).toBe("--x");
    expect(formatRealTimeFactor(-1)).toBe("--x");
  });
});

describe("the agent lease", () => {
  test("an agent lease produces the agent banner", () => {
    expect(agentBanner("agent", 1_520_330n * 1_000_000n)).toBe(
      "controlled by agent, paused at 1520.330 ms",
    );
  });

  test("a ui or endpoint lease shows no banner", () => {
    expect(agentBanner("ui", 5n * PS_PER_MS)).toBeNull();
    expect(agentBanner("endpoint", 5n * PS_PER_MS)).toBeNull();
  });

  test("the input buttons are disabled only under an agent lease", () => {
    const base = {
      image: "official",
      buildId: "a1b2c3d4",
      instance: "p1",
      nowPs: 12_402n * PS_PER_MS,
      realTimeFactor: 1,
      mode: { kind: "Wall", rate: 1 } as const,
    };
    expect(headerModel({ ...base, lease: "agent" }).inputDisabled).toBe(true);
    expect(headerModel({ ...base, lease: "ui" }).inputDisabled).toBe(false);
    expect(headerModel({ ...base, lease: "endpoint" }).inputDisabled).toBe(false);
  });
});

describe("headerModel", () => {
  test("it carries every field, including the build id", () => {
    const model = headerModel({
      image: "official",
      buildId: "a1b2c3d4",
      instance: "p1",
      nowPs: 12_402n * PS_PER_MS,
      realTimeFactor: 1,
      lease: "ui",
      mode: { kind: "Wall", rate: 1 },
    });
    expect(model.product).toBe("PassportSim");
    expect(model.image).toBe("official");
    expect(model.buildId).toBe("a1b2c3d4");
    expect(model.instance).toBe("p1");
    expect(model.virtualTime).toBe("12.402 s");
    expect(model.realTimeFactor).toBe("1.00x");
    expect(model.lease).toBe("ui");
    expect(model.running).toBe(true);
  });

  test("Paused is the only mode that is not running", () => {
    const base = {
      image: "official",
      buildId: "a1b2c3d4",
      instance: "p1",
      nowPs: 0n,
      realTimeFactor: 0,
      lease: "ui" as const,
    };
    expect(headerModel({ ...base, mode: { kind: "Paused" } }).running).toBe(false);
    expect(headerModel({ ...base, mode: { kind: "Max" } }).running).toBe(true);
    expect(headerModel({ ...base, mode: { kind: "Audio", rate: 1 } }).running).toBe(true);
  });
});
