import { describe, expect, test } from "bun:test";
import type { PacingStats } from "../../worker/pacing";
import * as fidelity from "./fidelity";
import * as inspect from "./inspect";
import * as perf from "./perf";
import {
  REWIND_INTERVAL_PS,
  REWIND_SLOTS,
  RewindRing,
  SNAPSHOT_SIZE_TARGET_BYTES,
  pointBytes,
  pointLabel,
  pointName,
  scrubTo,
  snapshotIdFrom,
  type RewindSource,
} from "./snapshots";

function sourceAt(marker: () => number): RewindSource {
  return {
    snapshot: () => Uint8Array.from([marker() & 0xff]),
    frame: () => Uint16Array.from([marker() & 0xffff, 0xbeef]),
  };
}

describe("the rewind ring", () => {
  test("it takes a point every 2 virtual seconds", () => {
    const ring = new RewindRing();
    let at = 0n;
    const source = sourceAt(() => Number(at / 1_000_000_000n));
    expect(ring.maybeCapture(at, source)).not.toBeNull();
    at += REWIND_INTERVAL_PS - 1n;
    expect(ring.maybeCapture(at, source)).toBeNull();
    at += 1n;
    expect(ring.maybeCapture(at, source)).not.toBeNull();
    expect(ring.list()).toHaveLength(2);
  });

  test("the cadence is virtual, so a machine at Max does not fill the ring in one wall second", () => {
    const ring = new RewindRing();
    const source = sourceAt(() => 0);
    // Twenty slices that advanced almost no virtual time take one point, not twenty.
    for (let index = 0; index < 20; index += 1) {
      ring.maybeCapture(BigInt(index) * 1_000_000n, source);
    }
    expect(ring.list()).toHaveLength(1);
  });

  test("the ring holds 20 points and evicts the oldest", () => {
    const ring = new RewindRing();
    for (let index = 0; index <= REWIND_SLOTS; index += 1) {
      ring.capture(BigInt(index) * REWIND_INTERVAL_PS, sourceAt(() => index));
    }
    expect(ring.list()).toHaveLength(REWIND_SLOTS);
    expect(ring.list()[0]?.seq).toBe(2);
    expect(ring.get(1)).toBeNull();
  });

  test("rewind restores the frame the snapshot carried", () => {
    const ring = new RewindRing();
    let second = 0;
    const source = sourceAt(() => second);
    for (second = 0; second < 4; second += 1) {
      ring.capture(BigInt(second) * REWIND_INTERVAL_PS, source);
    }
    // Scrub back two points: the frame that comes back is the one taken then, not the newest.
    const rewound = scrubTo(ring, 2);
    expect(rewound).not.toBeNull();
    expect(rewound?.point.vtPs).toBe(REWIND_INTERVAL_PS);
    expect(rewound?.frame?.[0]).toBe(1);
    expect(rewound?.point.bytes[0]).toBe(1);
    expect(ring.latest()?.frame?.[0]).toBe(3);
  });

  test("the stored frame is a copy, so the next slice cannot overwrite the past", () => {
    const ring = new RewindRing();
    const live = Uint16Array.from([0x1111, 0x2222]);
    ring.capture(0n, { snapshot: () => new Uint8Array(0), frame: () => live });
    live[0] = 0xffff;
    expect(ring.latest()?.frame?.[0]).toBe(0x1111);
  });

  test("a point taken before the first paint carries no frame rather than a blank one", () => {
    const ring = new RewindRing();
    ring.capture(0n, { snapshot: () => new Uint8Array(4), frame: () => null });
    expect(ring.latest()?.frame).toBeNull();
    expect(scrubTo(ring, 1)?.frame).toBeNull();
  });

  test("scrubbing to an evicted point reports nothing rather than the nearest one", () => {
    const ring = new RewindRing(2);
    for (let index = 0; index < 3; index += 1) {
      ring.capture(BigInt(index) * REWIND_INTERVAL_PS, sourceAt(() => index));
    }
    expect(scrubTo(ring, 1)).toBeNull();
    expect(scrubTo(ring, 3)?.point.seq).toBe(3);
  });

  test("virtual time going backwards re-anchors instead of pushing a point", () => {
    const ring = new RewindRing();
    const source = sourceAt(() => 0);
    ring.maybeCapture(10n * REWIND_INTERVAL_PS, source);
    // A restore moved the clock back; scrubbing must not evict the history being scrubbed.
    expect(ring.maybeCapture(2n * REWIND_INTERVAL_PS, source)).toBeNull();
    expect(ring.list()).toHaveLength(1);
    // From the new anchor the cadence resumes.
    expect(ring.maybeCapture(4n * REWIND_INTERVAL_PS, source)).not.toBeNull();
  });

  test("the stats report the size target", () => {
    const ring = new RewindRing();
    ring.capture(0n, {
      snapshot: () => new Uint8Array(SNAPSHOT_SIZE_TARGET_BYTES + 1),
      frame: () => null,
    });
    ring.capture(REWIND_INTERVAL_PS, { snapshot: () => new Uint8Array(16), frame: () => null });
    const stats = ring.stats();
    expect(stats.count).toBe(2);
    expect(stats.overTarget).toBe(1);
    expect(stats.spanPs).toBe(REWIND_INTERVAL_PS);
    expect(stats.totalBytes).toBe(SNAPSHOT_SIZE_TARGET_BYTES + 17);
  });

  test("a reboot clears the ring, because its snapshots belong to the old machine", () => {
    const ring = new RewindRing();
    ring.capture(0n, sourceAt(() => 0));
    ring.clear();
    expect(ring.list()).toHaveLength(0);
    expect(ring.latest()).toBeNull();
  });

  test("a point's size counts its frame as well as its bytes", () => {
    const ring = new RewindRing();
    const point = ring.capture(0n, {
      snapshot: () => new Uint8Array(10),
      frame: () => new Uint16Array(5),
    });
    expect(pointBytes(point)).toBe(20);
  });

  test("the scrubber labels a point with its virtual time", () => {
    const ring = new RewindRing();
    const point = ring.capture(12_402n * 1_000_000_000n, sourceAt(() => 0));
    expect(pointLabel(point)).toBe("12.402 s");
  });
});

describe("a point's registry snapshot", () => {
  // `snapshot restore` takes the name the registry assigned at `save`. Sending the page's own `seq`
  // instead answers `E_NOT_FOUND` while the overlay shows a past frame: the UI lying about the machine.
  test("a point has no id until `snapshot save` answered for it", () => {
    const ring = new RewindRing();
    const point = ring.capture(0n, sourceAt(() => 0));
    expect(ring.registryId(point.seq)).toBeNull();
    ring.setRegistryId(point.seq, "snap-7");
    expect(ring.registryId(point.seq)).toBe("snap-7");
  });

  test("an evicted point takes its id with it, so the map cannot outgrow the ring", () => {
    const ring = new RewindRing(2);
    const first = ring.capture(0n, sourceAt(() => 0));
    ring.setRegistryId(first.seq, "snap-1");
    ring.capture(1n, sourceAt(() => 1));
    ring.capture(2n, sourceAt(() => 2));
    expect(ring.get(first.seq)).toBeNull();
    expect(ring.registryId(first.seq)).toBeNull();
  });

  test("a reboot drops the ids with the points", () => {
    const ring = new RewindRing();
    const point = ring.capture(0n, sourceAt(() => 0));
    ring.setRegistryId(point.seq, "snap-1");
    ring.clear();
    expect(ring.registryId(point.seq)).toBeNull();
  });

  test("the name is read off the save result, and a result without one is not guessed at", () => {
    expect(snapshotIdFrom({ op: "save", name: "rewind-2-4.000s" })).toBe("rewind-2-4.000s");
    for (const empty of [{}, null, [], 7, "snap-9", { id: "snap-7" }, { name: 7 }, { name: "" }]) {
      expect(snapshotIdFrom(empty)).toBeNull();
    }
  });

  test("a point's name is one `snapshot` accepts", () => {
    const ring = new RewindRing();
    const point = ring.capture(4_000_000_000_000n, sourceAt(() => 0));
    expect(pointName(point)).toBe("rewind-1-4.000s");
    // The command's own name set.
    expect(pointName(point)).toMatch(/^[A-Za-z0-9_.-]{1,64}$/);
  });
});

describe("the fidelity tab", () => {
  test("it reads the rows the command returned and leaves unknown shapes alone", () => {
    const rows = fidelity.parseRows({
      subsystems: [
        { subsystem: "display", class: "B", may_assert: "pixels as commanded" },
        { name: "ble", fidelity: "C" },
        { subsystem: "nonsense" },
        "not an object",
      ],
    });
    expect(rows).toEqual([
      { subsystem: "display", class: "B", mayAssert: "pixels as commanded" },
      { subsystem: "ble", class: "C" },
    ]);
  });

  test("a bare array is accepted too", () => {
    expect(fidelity.parseRows([{ subsystem: "cpu", class: "A" }])).toHaveLength(1);
    expect(fidelity.parseRows(null)).toEqual([]);
    expect(fidelity.parseRows({ other: 1 })).toEqual([]);
  });

  test("class C and U are the ones an assertion has to be annotated for", () => {
    expect(fidelity.needsAnnotation("A")).toBe(false);
    expect(fidelity.needsAnnotation("B")).toBe(false);
    expect(fidelity.needsAnnotation("C")).toBe(true);
    expect(fidelity.needsAnnotation("U")).toBe(true);
    expect(fidelity.annotation("C")).toContain("emulator-only evidence");
    expect(fidelity.annotation("U")).toContain("strict");
  });

  test("a class-U touch blocks a strict PASS and a class-C touch only annotates it", () => {
    expect(fidelity.strictVerdict({ C: ["battery.ocv"], U: [] })).toEqual({
      canPass: true,
      blocking: [],
      annotated: ["battery.ocv"],
    });
    const blocked = fidelity.strictVerdict({ U: ["ledc.duty"] });
    expect(blocked.canPass).toBe(false);
    expect(blocked.blocking).toEqual(["ledc.duty"]);
  });

  test("an empty receipt is a pass, not an unknown", () => {
    expect(fidelity.strictVerdict({}).canPass).toBe(true);
  });

  test("rows sort worst class first", () => {
    const sorted = fidelity.byRisk([
      { subsystem: "cpu", class: "A" },
      { subsystem: "ledc", class: "U" },
      { subsystem: "ble", class: "C" },
      { subsystem: "display", class: "B" },
    ]);
    expect(sorted.map((row) => row.class)).toEqual(["U", "C", "B", "A"]);
  });

  test("the receipt line is the one-line suffix", () => {
    expect(
      fidelity.receiptLine([
        { subsystem: "cpu", class: "B" },
        { subsystem: "ble", class: "C" },
      ]),
    ).toBe("fidelity: cpu B, ble C");
    expect(fidelity.receiptLine([])).toBe("fidelity: unknown");
  });
});

describe("the inspect tab", () => {
  test("every section the command has is offered exactly once", () => {
    const names = inspect.INSPECT_REPORTS.map((report) => report.what);
    expect(names).toEqual(["tasks", "heap", "nvs", "lvgl", "fidelity"]);
    expect(new Set(names).size).toBe(names.length);
  });

  test("the picker round-trips through its `inspect` arguments", () => {
    expect(inspect.fromArgs(inspect.toArgs(["tasks"]))).toEqual({ what: ["tasks"] });
    expect(inspect.fromArgs(inspect.toArgs(["heap", "nvs"]))).toEqual({ what: ["heap", "nvs"] });
  });

  test("an empty pick is refused rather than sent as an empty list", () => {
    expect(() => inspect.toArgs([])).toThrow(inspect.InspectFormError);
  });

  test("a section the command does not have is refused, not passed through", () => {
    // `periph` has no section in the registered command; sending it would answer `E_USAGE`.
    expect(() => inspect.toArgs(["periph" as inspect.InspectReport])).toThrow(
      inspect.InspectFormError,
    );
  });

  test("a section picked twice is asked for once", () => {
    expect(inspect.toArgs(["heap", "heap"])).toEqual({ what: ["heap"] });
  });

  test("an array of objects renders as a table in the command's own field order", () => {
    const rendered = inspect.renderReport(
      [
        { name: "IDLE", state: "ready", hwm: 512 },
        { name: "pk_app", state: "blocked", hwm: 1_024, core: 0 },
      ],
      "text form",
    );
    expect(rendered.headers).toEqual(["name", "state", "hwm", "core"]);
    expect(rendered.rows[0]?.columns).toEqual(["IDLE", "ready", "512", ""]);
    expect(rendered.text).toBeNull();
  });

  test("anything else falls back to the text the command already produced", () => {
    expect(inspect.renderReport({ free: 12 }, "heap: 12").text).toBe("heap: 12");
    expect(inspect.renderReport([], "empty").text).toBe("empty");
    expect(inspect.renderReport([1, 2], "numbers").text).toBe("numbers");
  });

  test("a nested value is shown as JSON rather than as [object Object]", () => {
    expect(inspect.cell({ a: 1 })).toBe('{"a":1}');
    expect(inspect.cell(null)).toBe("");
    expect(inspect.cell(false)).toBe("false");
  });
});

describe("the perf tab", () => {
  const stats = (nowPs: bigint, factor: number, reanchors = 0): PacingStats => ({
    mode: "Wall",
    reanchors,
    realTimeFactor: factor,
    sliceVtPs: 8_000_000_000n,
    nowPs,
  });

  test("a window closes on host time, not virtual time", () => {
    const history = new perf.PerfHistory();
    expect(history.push(0, stats(0n, 1))).toBeNull();
    expect(history.push(perf.WINDOW_MS - 1, stats(50n * 1_000_000_000n, 1))).toBeNull();
    expect(history.push(perf.WINDOW_MS, stats(100n * 1_000_000_000n, 1))).not.toBeNull();
  });

  test("the report names the worst window, not only the latest", () => {
    const history = new perf.PerfHistory();
    history.push(0, stats(0n, 1));
    history.push(100, stats(100n * 1_000_000_000n, 1));
    history.push(200, stats(120n * 1_000_000_000n, 0.2));
    history.push(300, stats(220n * 1_000_000_000n, 1));
    const report = history.report();
    expect(report.current).toBe(1);
    expect(report.worst).toBeCloseTo(0.2, 5);
    expect(report.samples).toHaveLength(3);
  });

  test("the clock granularity is the smallest step it actually saw", () => {
    const history = new perf.PerfHistory();
    history.push(0, stats(0n, 1));
    history.push(16, stats(16n * 1_000_000_000n, 1));
    history.push(116, stats(116n * 1_000_000_000n, 1));
    expect(history.report().clockGranularityMs).toBe(16);
  });

  test("a 100 ms window is meaningful on a 16 ms clock and a 16 ms one is not", () => {
    expect(perf.isMeaningful(perf.WINDOW_MS, 16)).toBe(true);
    expect(perf.isMeaningful(16, 16)).toBe(false);
    expect(perf.isMeaningful(perf.WINDOW_MS, 0)).toBe(false);
  });

  test("the note says the numbers are quantised before anyone reads a trend into them", () => {
    expect(perf.clockNote(16)).toContain("16.0 ms");
    expect(perf.clockNote(0)).toContain("not yet measured");
  });

  test("re-anchors are counted over the history, not since the session began", () => {
    const history = new perf.PerfHistory();
    history.push(0, stats(0n, 1, 5));
    history.push(100, stats(100n * 1_000_000_000n, 1, 5));
    history.push(200, stats(200n * 1_000_000_000n, 1, 9));
    expect(history.report().reanchors).toBe(4);
  });

  test("an empty history reports zeros rather than NaN", () => {
    const report = new perf.PerfHistory().report();
    expect(report.current).toBe(0);
    expect(report.worst).toBe(0);
    expect(report.median).toBe(0);
    expect(report.virtualMsPerSecond).toBe(0);
  });

  test("the median is the middle of the history, and an even count averages the two", () => {
    expect(perf.median([])).toBe(0);
    expect(perf.median([1, 5, 3])).toBe(3);
    expect(perf.median([1, 2, 3, 4])).toBe(2.5);
  });
});
