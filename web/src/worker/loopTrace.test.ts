import { describe, expect, test } from "bun:test";

import { LoopRecorder, summarize } from "./loopTrace";
import { atomicsYielder, spinWait, waitAsyncTurn } from "./pacing";

function clock() {
  let now = 1_000;
  return { now: () => now, advance: (ms: number) => (now += ms) };
}

describe("LoopRecorder", () => {
  test("splits a step into its wait, its yield and the run left over, and times the gap between iterations", () => {
    const c = clock();
    const recorder = new LoopRecorder(c.now);
    recorder.begin();
    recorder.add("flushMs", 0.5);
    recorder.waited(4, 43, "timed-out");
    recorder.add("yieldMs", 2);
    recorder.stepped(46);
    recorder.add("pumpMs", 1);
    c.advance(50);
    const first = recorder.end("ahead", 0);
    expect(first).toMatchObject({ kind: "ahead", askedMs: 4, waitMs: 43, waitResult: "timed-out", yieldMs: 2, runMs: 1, gapMs: 0 });
    c.advance(7);
    recorder.begin();
    recorder.stepped(3);
    const second = recorder.end("ran", 2_000);
    expect(second).toMatchObject({ kind: "ran", gapMs: 7, runMs: 3, vtUs: 2_000, waitResult: "skip" });
  });

  test("counts a message handler into the iteration it ran in", () => {
    const recorder = new LoopRecorder(clock().now);
    recorder.begin();
    recorder.handled("call", 12);
    recorder.handled("button", 0.25);
    expect(recorder.end("ran", 0)).toMatchObject({ handlers: 2, handlerMs: 12.25, handled: "call,button" });
  });

  test("returns the iterations since a stall began, and summarises a window with its late waits", () => {
    const c = clock();
    const recorder = new LoopRecorder(c.now);
    for (const waitMs of [4, 30, 4]) {
      recorder.begin();
      recorder.waited(4, waitMs, "timed-out");
      recorder.stepped(waitMs);
      c.advance(waitMs);
      recorder.end("ahead", 0);
    }
    expect(recorder.since(1_005).map((iteration) => iteration.waitMs)).toEqual([30, 4]);
    const summary = recorder.summary(0);
    expect(summary).toMatchObject({ iterations: 3, ahead: 3, lateWaits5: 1, lateWaits20: 1, worstOvershootMs: 26 });
    expect(summary?.max.waitMs).toBe(30);
    expect(summarize([]).iterations).toBe(0);
  });
});

describe("the yielder's trace hooks", () => {
  test("report the wait asked, its answer and the message-loop turn after it", async () => {
    const cell = new SharedArrayBuffer(4);
    const waits: [number, string][] = [];
    const yields: number[] = [];
    const yielder = atomicsYielder(cell, {
      wait: () => "timed-out",
      yieldTask: () => Promise.resolve(),
      onWait: (asked, _waited, result) => waits.push([asked, result]),
      onYield: (ms) => yields.push(ms),
    });
    await yielder.sleep(3);
    await yielder.sleep(0);
    expect(waits).toEqual([[3, "timed-out"]]);
    expect(yields).toHaveLength(2);
  });

  test("the isolated yielder's turn is a waitAsync on the Worker's own thread, not a MessagePort", async () => {
    const yielder = atomicsYielder(new SharedArrayBuffer(4));
    expect(yielder.turn).toBe("wait-async");
    const at = performance.now();
    await yielder.sleep(0);
    expect(performance.now() - at).toBeLessThan(50);
    yielder.close?.();
  });

  test("an engine without waitAsync falls back to the MessageChannel turn", () => {
    expect(waitAsyncTurn(null)).toBeNull();
    const calls: string[] = [];
    const turn = waitAsyncTurn((_view, _index, _value, ms) => {
      calls.push(`wait ${ms}`);
      return { async: true, value: Promise.resolve("ok") };
    });
    expect(turn).not.toBeNull();
    void turn?.();
    expect(calls).toEqual(["wait 16"]);
  });

  test("the spin control returns at once when the counter moved and spins out its time otherwise", () => {
    const counter = new Int32Array(new SharedArrayBuffer(4));
    expect(spinWait(counter, 1, 50)).toBe("spin-ok");
    const at = performance.now();
    expect(spinWait(counter, 0, 2)).toBe("spin-timed-out");
    expect(performance.now() - at).toBeGreaterThanOrEqual(2);
  });
});
