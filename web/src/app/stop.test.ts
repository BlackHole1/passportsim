import { describe, expect, test } from "bun:test";
import { StopCode } from "../worker/layout";
import { CORE_ERROR_STOP, canContinue, coreErrorStop, parseStop, stopClass, stopReasonKey } from "./stop";

/** The shape `pemu_last_stop` answers (`crates/pemu-wasm/src/instance.rs` `last_stop_json`). */
const TRIPWIRE_JSON = JSON.stringify({
  reason: "Tripwire",
  code: StopCode.Tripwire,
  detail: 'Tripwire(TripReport { kind: RadioMmio, pc: 1107296256, detail: "bt controller", caller: 0, feature: None })',
  vt_ps: "411000000000",
  insns: "123",
  ff_insns: "0",
  idle_ps: "0",
});

describe("parseStop", () => {
  test("a tripwire is a fault with the core's detail and the virtual time it stopped at", () => {
    const stop = parseStop(StopCode.Tripwire, TRIPWIRE_JSON);
    expect(stop.name).toBe("Tripwire");
    expect(stop.class).toBe("fault");
    expect(stop.detail).toContain("TripReport");
    expect(stop.vtPs).toBe(411_000_000_000n);
    expect(canContinue(stop)).toBe(false);
  });

  test("no JSON, or JSON that does not parse, still names the stop by its code", () => {
    for (const json of [null, "not json", '{"detail":""}']) {
      const stop = parseStop(StopCode.GuestPanic, json);
      expect(stop.name).toBe("GuestPanic");
      expect(stop.detail).toBeNull();
      expect(stop.vtPs).toBeNull();
    }
  });

  test("a code this bundle does not know is a fault named by its number", () => {
    const stop = parseStop(99, null);
    expect(stop.name).toBe("Stop 99");
    expect(stop.class).toBe("fault");
    expect(stopReasonKey(99)).toBe("stop.reason.unknown");
  });
});

describe("coreErrorStop", () => {
  test("a core error is a fault named by its registry code, with the core's message as detail", () => {
    const stop = coreErrorStop("wasm trap: out of bounds", "E_INTERNAL");
    expect(stop.name).toBe("E_INTERNAL");
    expect(stop.class).toBe("fault");
    expect(stop.detail).toBe("wasm trap: out of bounds");
    expect(canContinue(stop)).toBe(false);
    expect(stopReasonKey(CORE_ERROR_STOP)).toBe("stop.reason.CoreError");
    expect(coreErrorStop("trap").name).toBe("CoreError");
  });
});

describe("stop classes", () => {
  test("armed stops and an unwakeable wait may continue; faults only restart", () => {
    expect(stopClass(StopCode.Breakpoint)).toBe("debug");
    expect(stopClass(StopCode.Watchpoint)).toBe("debug");
    expect(stopClass(StopCode.Matcher)).toBe("debug");
    expect(stopClass(StopCode.Deadlock)).toBe("waiting");
    for (const code of [StopCode.GuestPanic, StopCode.Stuck, StopCode.Tripwire, StopCode.Unmodeled, StopCode.Hle, StopCode.Halted]) {
      expect(stopClass(code)).toBe("fault");
      expect(canContinue(parseStop(code, null))).toBe(false);
    }
    expect(canContinue(parseStop(StopCode.Breakpoint, null))).toBe(true);
    expect(canContinue(parseStop(StopCode.Deadlock, null))).toBe(true);
  });

  test("every stop the core produces has its own reason, and the slice limits have none", () => {
    for (const [name, code] of Object.entries(StopCode)) {
      const key = stopReasonKey(code);
      if (name === "Until" || name === "MaxInsns") {
        expect(key).toBe("stop.reason.unknown");
      } else {
        expect(key).toBe(`stop.reason.${name}` as typeof key);
      }
    }
  });
});
