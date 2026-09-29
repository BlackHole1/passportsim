// Under a fake clock, including Safari's whole-millisecond `performance.now()`
// (`specs/notes/g3-behavior.md`) and a timer no finer than about 16 ms.

import { describe, expect, test } from "bun:test";

import { StopCode } from "./layout";
import {
  AUDIO_LEAD_PS,
  AUDIO_STALE_MS,
  IDLE_SLEEP_MS,
  MAX_SLICE_VT_PS,
  MAX_PACING_RATE,
  MIN_PACING_RATE,
  MIN_RUN_LEAD_MS,
  PS_PER_MS,
  PacingLoop,
  REANCHOR_LAG_PS,
  TIMER_FLOOR_MS,
  TURN_PROBE_MS,
  atomicsYielder,
  createInputCell,
  messageChannelYielder,
  notifyInput,
  pacingModeRefusal,
  turnRunsTasks,
  waitAsyncTurn,
  type AudioAnchor,
  type PacedCore,
  type PacingClock,
  type Yielder,
} from "./pacing";

/** A host clock and a sleeper in one. `quantumMs` rounds readings down: 1 is Safari, 16 a clamped timer, 0 ideal. */
class FakeClock implements PacingClock, Yielder {
  ms = 0;
  slept: number[] = [];

  constructor(
    private readonly quantumMs = 0,
    private readonly timerFloorMs = 0,
  ) {}

  nowMs(): number {
    return this.quantumMs > 0 ? Math.floor(this.ms / this.quantumMs) * this.quantumMs : this.ms;
  }

  async sleep(ms: number): Promise<void> {
    this.slept.push(ms);
    // A 0 ms sleep is the per-slice yield, which both real yielders make without a timer.
    this.ms += ms === 0 ? 0 : Math.max(ms, this.timerFloorMs);
  }

  advance(ms: number): void {
    this.ms += ms;
  }
}

class FakeCore implements PacedCore {
  private ps = 0n;
  slices = 0;
  stops: number[] = [];

  constructor(
    private readonly clock: FakeClock,
    private readonly hostCostMs = 1,
  ) {}

  nowPs(): bigint {
    return this.ps;
  }

  run(untilPs: bigint, maxInsns: bigint): number {
    this.slices += 1;
    const bound = this.ps + maxInsns * 6_250n;
    const reached = untilPs < bound ? untilPs : bound;
    if (reached > this.ps) {
      this.ps = reached;
    }
    this.clock.advance(this.hostCostMs);
    return this.stops.shift() ?? (reached < untilPs ? StopCode.MaxInsns : StopCode.Until);
  }
}

class ScaledCore implements PacedCore {
  private ps = 0n;

  constructor(
    private readonly clock: FakeClock,
    private readonly speed: number,
  ) {}

  nowPs(): bigint {
    return this.ps;
  }

  run(untilPs: bigint, maxInsns: bigint): number {
    const bound = this.ps + maxInsns * 6_250n;
    const reached = untilPs < bound ? untilPs : bound;
    if (reached > this.ps) {
      this.clock.advance(Number((reached - this.ps) / 1_000n) / 1_000_000 / this.speed);
      this.ps = reached;
    }
    return reached < untilPs ? StopCode.MaxInsns : StopCode.Until;
  }
}

function anchorAt(ps: bigint | null): AudioAnchor {
  return { consumedPs: () => ps };
}

describe("Wall pacing", () => {
  test("follows the host clock without ever passing it", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 1);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });

    for (let step = 0; step < 40; step += 1) {
      await loop.step();
      const wallPs = BigInt(clock.ms) * PS_PER_MS;
      expect(core.nowPs()).toBeLessThanOrEqual(wallPs);
    }
    expect(core.nowPs()).toBeGreaterThan(0n);
  });

  test("waits instead of running once it has reached the deadline", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });

    clock.advance(4);
    const ran = await loop.step();
    expect(ran.kind).toBe("ran");
    const waited = await loop.step();
    expect(waited.kind).toBe("ahead");
    if (waited.kind === "ahead") {
      expect(waited.sleptMs).toBeLessThanOrEqual(IDLE_SLEEP_MS);
      expect(waited.sleptMs).toBeGreaterThanOrEqual(1);
    }
  });

  test("a rate above one runs the guest faster than the wall", async () => {
    const clock = new FakeClock();
    const fast = new FakeCore(clock, 0);
    const loop = new PacingLoop(fast, clock, clock);
    loop.setMode({ kind: "Wall", rate: 4 });
    clock.advance(100);

    await loop.step();
    expect(fast.nowPs()).toBe(MAX_SLICE_VT_PS);
  });
});

describe("a fine host clock", () => {
  // Isolated Chromium reads `performance.now()` to about 5 us. Without the minimum lead the loop ran
  // a slice of about 10 virtual microseconds per iteration: 803k `pemu_run` calls in a 9 s run, most
  // of the wall time spent in per-call cost.
  test("does not run a slice for every microsecond the wall moves", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0.005);
    const spin: Yielder = { sleep: async () => clock.advance(0.005) };
    const loop = new PacingLoop(core, clock, spin);
    loop.setMode({ kind: "Wall", rate: 1 });

    const ranPs: bigint[] = [];
    while (clock.ms < 1_000) {
      const outcome = await loop.step();
      if (outcome.kind === "ran") {
        ranPs.push(outcome.toPs - outcome.fromPs);
      }
    }
    expect(ranPs.length).toBeLessThanOrEqual(1_000);
    expect(ranPs.length).toBeGreaterThan(900);
    for (const ps of ranPs) {
      expect(ps).toBeGreaterThanOrEqual(BigInt(MIN_RUN_LEAD_MS) * PS_PER_MS);
    }
    expect(core.nowPs()).toBeGreaterThan(998n * PS_PER_MS);
  });

  test("the lead scales with the rate and never exceeds one slice", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 0.5 });
    clock.advance(1);
    expect((await loop.step()).kind).toBe("ran");
    expect(core.nowPs()).toBe(PS_PER_MS / 2n);
    clock.advance(0.5);
    expect((await loop.step()).kind).toBe("ahead");

    loop.setMode({ kind: "Wall", rate: 1_000 });
    clock.advance(0.01);
    expect((await loop.step()).kind).toBe("ran");
  });
});

describe("a very low rate", () => {
  // Rounded through microseconds the lead is 0 below rate 0.0005, so the loop ran empty slices and
  // never slept.
  test("sleeps between slices at rate 1e-4 instead of running empty ones", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0.005);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1e-4 });

    let empty = 0;
    let ran = 0;
    while (clock.ms < 200) {
      const outcome = await loop.step();
      if (outcome.kind === "ran") {
        ran += 1;
        if (outcome.toPs === outcome.fromPs) {
          empty += 1;
        }
      }
    }
    expect(empty).toBe(0);
    expect(ran).toBeLessThanOrEqual(21);
    expect(clock.slept.length).toBeGreaterThan(40);
    expect(core.nowPs()).toBeLessThanOrEqual(20n * 1_000_000n);
  });

  test("the Worker's accepted rate range is checked at its boundary", () => {
    expect(pacingModeRefusal({ kind: "Wall", rate: 1 })).toBeNull();
    expect(pacingModeRefusal({ kind: "Audio", rate: MIN_PACING_RATE })).toBeNull();
    expect(pacingModeRefusal({ kind: "Wall", rate: MAX_PACING_RATE })).toBeNull();
    expect(pacingModeRefusal({ kind: "Max" })).toBeNull();
    expect(pacingModeRefusal({ kind: "Paused" })).toBeNull();
    for (const rate of [0, -1, Number.NaN, Number.POSITIVE_INFINITY, MIN_PACING_RATE / 2, MAX_PACING_RATE * 2]) {
      expect(pacingModeRefusal({ kind: "Wall", rate })).toContain("rate");
    }
    expect(pacingModeRefusal({ kind: "Wall", rate: "1" as unknown as number })).toContain("rate");
  });
});

describe("the isolated yielder", () => {
  test("under a fake clock it wakes on an input before its deadline", async () => {
    const clock = new FakeClock();
    const cell = createInputCell();
    const inputAtMs = 1.5;
    let inputPending = true;
    let yields = 0;
    const yielder = atomicsYielder(cell, {
      wait: (counter, expected, ms) => {
        const deadline = clock.ms + ms;
        if (inputPending && inputAtMs < deadline && Atomics.load(counter, 0) === expected) {
          inputPending = false;
          clock.ms = inputAtMs;
          notifyInput(cell);
          return "ok";
        }
        clock.ms = deadline;
        return "timed-out";
      },
      yieldTask: async () => {
        yields += 1;
      },
    });
    expect(yielder.kind).toBe("atomics-wait");

    await yielder.sleep(IDLE_SLEEP_MS);
    expect(clock.ms).toBe(inputAtMs);
    expect(clock.ms).toBeLessThan(IDLE_SLEEP_MS);
    expect(yields).toBe(1);

    await yielder.sleep(IDLE_SLEEP_MS);
    expect(clock.ms).toBe(inputAtMs + IDLE_SLEEP_MS);
    expect(yields).toBe(2);
  });

  test("does not wait when an input was notified since the previous sleep", async () => {
    const cell = createInputCell();
    let waits = 0;
    const yielder = atomicsYielder(cell, {
      wait: () => {
        waits += 1;
        return "timed-out";
      },
      yieldTask: async () => {},
    });
    await yielder.sleep(IDLE_SLEEP_MS);
    expect(waits).toBe(1);
    notifyInput(cell);
    await yielder.sleep(IDLE_SLEEP_MS);
    expect(waits).toBe(1);
    await yielder.sleep(IDLE_SLEEP_MS);
    expect(waits).toBe(2);
  });

  test("a real Atomics.wait returns when another thread notifies an input", async () => {
    const cell = createInputCell();
    const code =
      "self.onmessage = (event) => { const counter = new Int32Array(event.data); postMessage('armed');" +
      " setTimeout(() => { Atomics.add(counter, 0, 1); Atomics.notify(counter, 0); }, 20); };";
    const page = new Worker(URL.createObjectURL(new Blob([code], { type: "application/javascript" })));
    try {
      // The baseline is taken when the yielder is made, before any input, as the Worker's is. Made after
      // `armed`, a loaded host could let the 20 ms bump land first and the wait would run its full 5 s.
      const yielder = atomicsYielder(cell);
      const armed = new Promise((resolve) => {
        page.onmessage = resolve;
      });
      page.postMessage(cell);
      await armed;
      const start = performance.now();
      await yielder.sleep(5_000);
      expect(performance.now() - start).toBeLessThan(2_000);
      expect(Atomics.load(new Int32Array(cell), 0)).toBe(1);
      yielder.close?.();
    } finally {
      page.terminate();
    }
  });

  test("the fallback names itself", () => {
    const yielder = messageChannelYielder();
    expect(yielder.kind).toBe("message-channel");
    yielder.close?.();
  });

  test("closing a yielder releases its channel and never strands a wait", async () => {
    const fallback = messageChannelYielder();
    const pending = fallback.sleep(0);
    fallback.close?.();
    await pending;
    await fallback.sleep(0);
    fallback.close?.();

    const isolated = atomicsYielder(createInputCell(), { wait: () => "timed-out" });
    isolated.close?.();
    await isolated.sleep(1);
  });
});

describe("the waitAsync turn probe", () => {
  function thread(turnRunsQueue: boolean) {
    let now = 0;
    let turns = 0;
    const queued: (() => void)[] = [];
    return {
      get turns() {
        return turns;
      },
      parts: { queue: (run: () => void) => void queued.push(run), now: () => now },
      turn: async () => {
        turns += 1;
        now += 0.3;
        // Chromium and WebKit run the tasks queued ahead of the turn; Firefox runs none of them.
        if (turnRunsQueue) {
          queued.splice(0).forEach((run) => run());
        }
      },
    };
  }

  test("a turn that runs the queued task is kept after the first turn", async () => {
    const chromium = thread(true);
    expect(await turnRunsTasks(chromium.turn, chromium.parts)).toBe(true);
    expect(chromium.turns).toBe(1);
  });

  test("a turn that starves the queue is refused once the budget has passed, not before", async () => {
    const firefox = thread(false);
    expect(await turnRunsTasks(firefox.turn, firefox.parts)).toBe(false);
    expect(firefox.turns).toBe(Math.ceil(TURN_PROBE_MS / 0.3));
  });

  test("the probe reaches an answer within its budget on this engine's real waitAsync turn", async () => {
    // The answer is the engine's (Bun on Windows starves the timer, as Firefox does); the probe must
    // give one and not hang.
    const turn = waitAsyncTurn();
    expect(turn).not.toBeNull();
    const started = performance.now();
    const answer = await turnRunsTasks(turn ?? (async () => {}), { budgetMs: 200 });
    expect(typeof answer).toBe("boolean");
    expect(performance.now() - started).toBeLessThan(2_000);
  });
});

describe("a coarse host clock", () => {
  test("whole-millisecond readings do not stall the loop (Safari, g3-safari-performance-now)", async () => {
    const clock = new FakeClock(1);
    const core = new FakeCore(clock, 1);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });

    for (let step = 0; step < 200; step += 1) {
      await loop.step();
    }
    expect(core.slices).toBeGreaterThan(20);
    expect(core.nowPs()).toBeGreaterThan(20n * PS_PER_MS);
    expect(Number.isFinite(loop.stats().realTimeFactor)).toBe(true);
  });

  test("a timer clamped to about 16 ms does not make virtual time jump", async () => {
    const clock = new FakeClock(16, TIMER_FLOOR_MS);
    const core = new FakeCore(clock, 1);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });

    let previous = 0n;
    for (let step = 0; step < 100; step += 1) {
      await loop.step();
      const now = core.nowPs();
      expect(now).toBeGreaterThanOrEqual(previous);
      expect(now - previous).toBeLessThanOrEqual(MAX_SLICE_VT_PS);
      previous = now;
    }
  });
});

describe("re-anchoring", () => {
  test("a guest that falls more than 250 ms behind re-anchors instead of catching up", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 40);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });

    let previous = 0n;
    for (let step = 0; step < 40; step += 1) {
      await loop.step();
      const now = core.nowPs();
      expect(now - previous).toBeLessThanOrEqual(MAX_SLICE_VT_PS);
      previous = now;
    }
    expect(loop.stats().reanchors).toBeGreaterThan(0);
    expect(core.nowPs()).toBeLessThan(BigInt(clock.ms) * PS_PER_MS);
  });

  test("the lag needed to re-anchor is a quarter second", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });
    expect(REANCHOR_LAG_PS).toBe(250n * PS_PER_MS);

    clock.advance(200);
    await loop.step();
    expect(loop.stats().reanchors).toBe(0);

    clock.advance(400);
    await loop.step();
    expect(loop.stats().reanchors).toBe(1);
  });

  test("a re-anchored slice still makes progress", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });

    clock.advance(5_000);
    const before = core.nowPs();
    await loop.step();
    expect(loop.stats().reanchors).toBe(1);
    expect(core.nowPs()).toBeGreaterThan(before);
  });
});

describe("Audio pacing", () => {
  test("targets the last consumed sample plus 60 ms", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const consumedPs = 100n * PS_PER_MS;
    const loop = new PacingLoop(core, clock, clock, anchorAt(consumedPs));
    loop.setMode({ kind: "Audio", rate: 1 });

    for (let step = 0; step < 100; step += 1) {
      const outcome = await loop.step();
      if (outcome.kind === "ahead") {
        break;
      }
    }
    expect(core.nowPs()).toBe(consumedPs + AUDIO_LEAD_PS);
    expect((await loop.step()).kind).toBe("ahead");
  });

  test("falls back to the wall anchor before the first sample is consumed", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const loop = new PacingLoop(core, clock, clock, anchorAt(null));
    loop.setMode({ kind: "Audio", rate: 1 });

    clock.advance(20);
    await loop.step();
    expect(core.nowPs()).toBe(MAX_SLICE_VT_PS);
  });

  test("is immune to the host clock's resolution: it counts samples, not milliseconds", async () => {
    const clock = new FakeClock(16, TIMER_FLOOR_MS);
    const core = new FakeCore(clock, 1);
    const loop = new PacingLoop(core, clock, clock, anchorAt(40n * PS_PER_MS));
    loop.setMode({ kind: "Audio", rate: 1 });

    for (let step = 0; step < 100; step += 1) {
      const outcome = await loop.step();
      if (outcome.kind === "ahead") {
        break;
      }
    }
    expect(core.nowPs()).toBe(40n * PS_PER_MS + AUDIO_LEAD_PS);
  });
});

describe("the Audio clock and its handovers", () => {
  class ScriptedAnchor implements AudioAnchor {
    ps: bigint | null = null;
    isFlowing = true;
    consumedPs(): bigint | null {
      return this.ps;
    }
    flowing(): boolean {
      return this.isFlowing;
    }
  }

  async function runUntilAhead(loop: PacingLoop): Promise<void> {
    for (let step = 0; step < 1000; step += 1) {
      if ((await loop.step()).kind === "ahead") {
        return;
      }
    }
    throw new Error("the loop never reached its target");
  }

  test("follows the samples consumed, however fast the host clock runs", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const anchor = new ScriptedAnchor();
    const loop = new PacingLoop(core, clock, clock, anchor);
    loop.setMode({ kind: "Audio", rate: 1 });
    for (let block = 1; block <= 20; block += 1) {
      anchor.ps = BigInt(block * 10) * PS_PER_MS;
      await runUntilAhead(loop);
      expect(core.nowPs()).toBe(anchor.ps + AUDIO_LEAD_PS);
      clock.ms = block * 20;
    }
    expect(loop.stats().clock).toBe("audio");
    expect(loop.stats().reanchors).toBe(0);
  });

  test("hands over to the wall clock at once when nothing is left to consume, without a jump", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const anchor = new ScriptedAnchor();
    anchor.ps = 100n * PS_PER_MS;
    const loop = new PacingLoop(core, clock, clock, anchor);
    loop.setMode({ kind: "Audio", rate: 1 });
    await runUntilAhead(loop);
    expect(core.nowPs()).toBe(160n * PS_PER_MS);

    anchor.isFlowing = false;
    clock.ms = 50;
    const first = await loop.step();
    expect(first.kind).toBe("ran");
    expect(core.nowPs()).toBe(160n * PS_PER_MS + MAX_SLICE_VT_PS);
    expect(loop.stats().clock).toBe("wall");
    clock.ms = 50;
    await runUntilAhead(loop);
    expect(core.nowPs()).toBeGreaterThanOrEqual(210n * PS_PER_MS);
    expect(core.nowPs()).toBeLessThanOrEqual(215n * PS_PER_MS);
    expect(loop.stats()).toMatchObject({ audioHandovers: 1, reanchors: 0 });
  });

  test("treats a consumed count that stops moving as a stopped audio clock", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const anchor = new ScriptedAnchor();
    anchor.ps = 100n * PS_PER_MS;
    const loop = new PacingLoop(core, clock, clock, anchor);
    loop.setMode({ kind: "Audio", rate: 1 });
    await runUntilAhead(loop);

    clock.ms = AUDIO_STALE_MS;
    expect((await loop.step()).kind).toBe("ahead");
    expect(loop.stats().clock).toBe("audio");
    clock.ms = AUDIO_STALE_MS + 1;
    expect((await loop.step()).kind).toBe("ran");
    expect(loop.stats().clock).toBe("wall");

    await runUntilAhead(loop);
    const guest = core.nowPs();
    anchor.ps = 120n * PS_PER_MS;
    expect((await loop.step()).kind).toBe("ahead");
    expect(core.nowPs()).toBe(guest);
    expect(loop.stats()).toMatchObject({ clock: "audio", audioHandovers: 2 });
  });

  test("re-anchors the wall fallback after a quarter second of lag, and virtual time never jumps", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 0);
    const loop = new PacingLoop(core, clock, clock, new ScriptedAnchor());
    loop.setMode({ kind: "Audio", rate: 1 });
    clock.ms = 1_000;
    const outcome = await loop.step();
    expect(outcome).toMatchObject({ kind: "ran", fromPs: 0n, toPs: MAX_SLICE_VT_PS });
    expect(loop.stats().reanchors).toBe(1);
    expect((await loop.step()).kind).toBe("ahead");
  });
});

describe("the other modes", () => {
  test("Max runs a slice every iteration and never waits on the clock", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 1);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Max" });

    for (let step = 0; step < 10; step += 1) {
      expect((await loop.step()).kind).toBe("ran");
    }
    expect(core.nowPs()).toBe(10n * MAX_SLICE_VT_PS);
    expect(clock.slept).toEqual(new Array(10).fill(0));
  });

  test("Max yields to the message loop after every slice, so a message posted meanwhile lands", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 1);
    const slept: number[] = [];
    const yielder: Yielder = {
      sleep: (ms) =>
        new Promise<void>((resolve) => {
          slept.push(ms);
          setTimeout(resolve, 0);
        }),
    };
    const loop = new PacingLoop(core, clock, yielder);
    loop.setMode({ kind: "Max" });

    // A macrotask, like a Worker message: without a yield between slices it would never run.
    setTimeout(() => loop.stop(), 0);
    await loop.run();
    expect(core.slices).toBeGreaterThan(0);
    expect(slept).toHaveLength(core.slices);
    expect(slept.every((ms) => ms === 0)).toBe(true);
  });

  test("Paused never runs the core", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 1);
    const loop = new PacingLoop(core, clock, clock);

    expect((await loop.step()).kind).toBe("paused");
    expect(core.slices).toBe(0);
    expect(clock.slept).toEqual([IDLE_SLEEP_MS]);
  });

  test("a stop that is not a run limit ends the loop and pauses the session", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 1);
    core.stops = [StopCode.Until, StopCode.GuestPanic];
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Max" });

    const outcomes: string[] = [];
    await loop.run((outcome) => outcomes.push(outcome.kind));
    expect(outcomes).toEqual(["ran", "stopped"]);
    expect(loop.currentMode.kind).toBe("Paused");
  });
});

describe("the measured real-time factor", () => {
  test("is only computed over a window long enough for 1 ms to be small", async () => {
    const clock = new FakeClock(1);
    const core = new FakeCore(clock, 4);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Max" });

    await loop.step();
    expect(loop.stats().realTimeFactor).toBe(0);

    for (let step = 0; step < 40; step += 1) {
      await loop.step();
    }
    const factor = loop.stats().realTimeFactor;
    expect(factor).toBeGreaterThan(0);
    expect(Number.isFinite(factor)).toBe(true);
  });

  // A core 20x faster than real time, paced at `Wall` 1x, sleeps most of the time: the factor
  // includes the sleeps and reads about 1, the headroom about 20. Both within 5 % after a second of
  // warm-up: the target is absolute from the anchor, so what remains per window is at most the lead
  // over one quantized reading (about 2 % of a 48 ms window).
  for (const [name, quantumMs, floorMs] of [
    ["an ideal clock", 0, 0],
    ["whole-millisecond readings", 1, 0],
    ["a 16 ms timer floor", 1, 16],
  ] as const) {
    test(`reads about 1 at Wall 1x for a fast core, with ${name}`, async () => {
      const clock = new FakeClock(quantumMs, floorMs);
      const core = new ScaledCore(clock, 20);
      const loop = new PacingLoop(core, clock, clock);
      loop.setMode({ kind: "Wall", rate: 1 });

      const factors: number[] = [];
      const startMs = clock.ms;
      while (clock.ms - startMs < 10_000) {
        await loop.step();
        if (clock.ms - startMs >= 1_000) {
          factors.push(loop.stats().realTimeFactor);
        }
      }
      expect(Math.max(...factors.map((factor) => Math.abs(factor - 1)))).toBeLessThan(0.05);
      expect(Math.abs((loop.stats().headroom ?? 0) - 20)).toBeLessThan(1);
      const overall = Number(core.nowPs() / PS_PER_MS) / (clock.ms - startMs);
      expect(Math.abs(overall - 1)).toBeLessThan(0.05);
    });
  }

  test("reads below 1 at Wall 1x when the core is slower than real time", async () => {
    const clock = new FakeClock();
    const core = new ScaledCore(clock, 0.5);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Wall", rate: 1 });
    for (let step = 0; step < 200; step += 1) {
      await loop.step();
    }
    const stats = loop.stats();
    expect(Math.abs(stats.realTimeFactor - 0.5)).toBeLessThan(0.05);
    expect(Math.abs(stats.realTimeFactor - (stats.headroom ?? 0))).toBeLessThan(0.05);
  });

  test("shortens the slice when the guest costs more wall time than the 8 ms budget", async () => {
    const clock = new FakeClock();
    const core = new FakeCore(clock, 32);
    const loop = new PacingLoop(core, clock, clock);
    loop.setMode({ kind: "Max" });

    for (let step = 0; step < 10; step += 1) {
      await loop.step();
    }
    expect(loop.stats().sliceVtPs).toBeLessThan(MAX_SLICE_VT_PS);
  });
});
