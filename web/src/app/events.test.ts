import { describe, expect, test } from "bun:test";
import { EventLog, eventName, summarize } from "./events";
import { EventKind } from "../worker/layout";

describe("event kinds", () => {
  test("every generated kind has a name", () => {
    for (const [name, value] of Object.entries(EventKind)) {
      expect(eventName(value)).toBe(name);
    }
  });

  test("a kind this bundle does not know is named, not dropped", () => {
    expect(eventName(4_242)).toBe("kind 4242");
  });
});

describe("the log", () => {
  test("it interleaves the machine's ring with the page's own calls", () => {
    const log = new EventLog();
    log.pushHost([{ kind: EventKind.Reset, vtPs: 0n, arg: 0n }]);
    log.pushCall("input", { button: "ok", action: "click" }, 1_000_000_000_000n);
    log.pushHost([{ kind: EventKind.Frame, vtPs: 2_000_000_000_000n, arg: 7n }]);

    expect(log.list().map((row) => [row.source, row.name])).toEqual([
      ["machine", "Reset"],
      ["ui", "input"],
      ["machine", "Frame"],
    ]);
    expect(log.list()[1]?.vt).toBe("1.000 s");
    expect(log.list()[2]?.detail).toBe("arg 7");
  });

  test("a call made before a machine exists has no virtual time to show", () => {
    const log = new EventLog();
    log.pushCall("status", {}, null);
    expect(log.list()[0]?.vt).toBe("--");
  });

  test("the list is capped and says how many rows it dropped", () => {
    const log = new EventLog(2);
    for (const kind of [0, 1, 2]) {
      log.pushHost([{ kind, vtPs: 0n, arg: 0n }]);
    }
    expect(log.list()).toHaveLength(2);
    expect(log.dropped).toBe(1);
    expect(log.list().map((row) => row.seq)).toEqual([2, 3]);
  });

  test("a long argument is capped rather than pushing the column off-screen", () => {
    const long = summarize({ uri: "x".repeat(500) });
    expect(long.length).toBe(80);
    expect(long.endsWith("...")).toBe(true);
  });
});
