// The pacing loop. The emulated clock is authoritative: the loop asks the host clock only where
// the next virtual deadline is, never how fast one slice ran, because `performance.now()` is whole
// milliseconds in Safari (`specs/notes/g3-behavior.md`) and timers are clamped to about 16 ms. Every
// rate is measured over tens of milliseconds, and no sleep is assumed shorter than 16 ms.

import { isLimitStop } from "./layout";

/** Picoseconds in one millisecond; virtual time is counted in picoseconds. */
export const PS_PER_MS = 1_000_000_000n;

export const MAX_SLICE_WALL_MS = 8;

export const MAX_SLICE_VT_PS = BigInt(MAX_SLICE_WALL_MS) * PS_PER_MS;

/** Shortest slice in virtual time, so a slow host still makes progress each iteration. */
export const MIN_SLICE_VT_PS = PS_PER_MS / 4n;

/** Falling behind by more than this re-anchors. */
export const REANCHOR_LAG_PS = 250n * PS_PER_MS;

/** `Audio` pacing targets the last consumed sample plus 60 ms. */
export const AUDIO_LEAD_PS = 60n * PS_PER_MS;

/**
 * How long the audio clock may stand still before `Audio` pacing hands over to the wall clock: a
 * suspended context, a hidden tab and a stopped I2S TX all just stop the consumed count. Several
 * clamped timer periods; a tuning choice, not measured against a real suspended context.
 */
export const AUDIO_STALE_MS = 100;

/**
 * The least host time a paced slice covers; a closer target is waited for. It equals the shortest
 * sleep the loop asks of a yielder, so under `Atomics.wait` one wait buys one slice worth its fixed
 * cost. The MessageChannel fallback cannot sleep under {@link TIMER_FLOOR_MS} and spins between
 * slices.
 */
export const MIN_RUN_LEAD_MS = 1;

/**
 * The pacing rates accepted for `Wall` and `Audio`, inclusive. Below the floor the lead of
 * {@link MIN_RUN_LEAD_MS} would be under a picosecond; above the ceiling the wall target's
 * microsecond product leaves the safe-integer range within hours.
 */
export const MIN_PACING_RATE = 1e-6;
export const MAX_PACING_RATE = 1e6;

/** Why `mode` cannot be paced, or `null`. The Worker refuses a bad rate rather than falling back to 1. */
export function pacingModeRefusal(mode: PacingMode): string | null {
  if (mode.kind !== "Wall" && mode.kind !== "Audio") {
    return null;
  }
  const rate = mode.rate;
  if (typeof rate !== "number" || !Number.isFinite(rate) || rate < MIN_PACING_RATE || rate > MAX_PACING_RATE) {
    return `${mode.kind} rate ${String(rate)} is not a finite number in [${MIN_PACING_RATE}, ${MAX_PACING_RATE}]`;
  }
  return null;
}

/** Sleep when the guest is ahead of its deadline: until the next input, or 4 ms. */
export const IDLE_SLEEP_MS = 4;

/**
 * The shortest wall interval a rate is computed over: one `performance.now()` difference carries up
 * to about 1 ms of error in Safari.
 */
export const RATE_WINDOW_MS = 48;

/**
 * The best timer resolution the loop may assume. `setTimeout` and `performance.now` are clamped to
 * about 16 ms without cross-origin isolation.
 */
export const TIMER_FLOOR_MS = 16;

export type PacingMode =
  | { readonly kind: "Paused" }
  | { readonly kind: "Max" }
  | { readonly kind: "Wall"; readonly rate: number }
  | { readonly kind: "Audio"; readonly rate: number };

/** The host clock. Its readings may be quantized; the loop never divides by one difference. */
export interface PacingClock {
  nowMs(): number;
}

export type YielderKind = "atomics-wait" | "message-channel";

/**
 * How the Worker waits: `Atomics.wait` on the input cell when isolated, else a MessageChannel
 * yield. Either may return late.
 */
export interface Yielder {
  sleep(ms: number): Promise<void>;
  readonly kind?: YielderKind;
  /**
   * How an isolated yielder lets the event loop run after its wait: `wait-async`, or
   * `message-channel` where `Atomics.waitAsync` is missing or starves the Worker's tasks.
   */
  readonly turn?: "wait-async" | "message-channel";
  /** Releases what the yielder holds (a MessageChannel's ports); a pending wait resolves at once. */
  close?(): void;
}

export interface PacedCore {
  nowPs(): bigint;
  run(untilPs: bigint, maxInsns: bigint): number;
}

/** The audio anchor: virtual time of the last sample the AudioWorklet consumed. */
export interface AudioAnchor {
  consumedPs(): bigint | null;
  /**
   * Whether the worklet has enough buffered to keep consuming. `false` hands over to the wall clock
   * at once rather than after {@link AUDIO_STALE_MS}. Absent means always `true`.
   */
  flowing?(): boolean;
}

export type PacingClockKind = "none" | "wall" | "audio";

export type StepOutcome =
  | { readonly kind: "paused" }
  | { readonly kind: "ahead"; readonly sleptMs: number }
  | { readonly kind: "ran"; readonly stop: number; readonly fromPs: bigint; readonly toPs: bigint }
  | { readonly kind: "stopped"; readonly stop: number; readonly atPs: bigint };

export interface PacingStats {
  readonly mode: PacingMode["kind"];
  readonly reanchors: number;
  readonly clock?: PacingClockKind;
  readonly audioHandovers?: number;
  /**
   * Virtual milliseconds per host millisecond over at least {@link RATE_WINDOW_MS}, sleeps
   * included: about the pacing rate while the guest keeps up, lower when the host is too slow.
   */
  readonly realTimeFactor: number;
  /**
   * Virtual milliseconds per host millisecond inside `pemu_run` only. Without isolation Chromium's
   * `performance.now()` is 100 us coarse, so most slices read 0 ms and this reads high.
   */
  readonly headroom?: number;
  readonly sliceVtPs: bigint;
  readonly nowPs: bigint;
}

/** Nominal core clock, for the `max_insns` safety bound: 160 MHz at one instruction per cycle. */
const PS_PER_NOMINAL_INSN = 6_250n;

export class PacingLoop {
  private mode: PacingMode = { kind: "Paused" };
  private vtAnchorPs: bigint;
  private wallAnchorMs: number;
  private sliceVt = MAX_SLICE_VT_PS;
  private reanchorCount = 0;
  private measuredFactor = 0;
  private measuredHeadroom = 0;
  private rateStartMs: number;
  private rateStartPs: bigint;
  private windowWallMs = 0;
  private windowVtPs = 0n;
  private windowSlices = 0;
  private running = false;
  private clockKind: PacingClockKind = "none";
  private handovers = 0;
  private lastConsumedPs: bigint | null = null;
  private lastAdvanceMs = 0;

  constructor(
    private readonly core: PacedCore,
    private readonly clock: PacingClock,
    private readonly yielder: Yielder,
    private readonly audio: AudioAnchor = { consumedPs: () => null },
  ) {
    this.vtAnchorPs = core.nowPs();
    this.wallAnchorMs = clock.nowMs();
    this.rateStartPs = this.vtAnchorPs;
    this.rateStartMs = this.wallAnchorMs;
  }

  /** Switches mode and re-anchors, so a resume never replays the time spent paused. */
  setMode(mode: PacingMode): void {
    this.mode = mode;
    this.anchor();
  }

  get currentMode(): PacingMode {
    return this.mode;
  }

  stats(): PacingStats {
    return {
      mode: this.mode.kind,
      reanchors: this.reanchorCount,
      clock: this.clockKind,
      audioHandovers: this.handovers,
      realTimeFactor: this.measuredFactor,
      headroom: this.measuredHeadroom,
      sliceVtPs: this.sliceVt,
      nowPs: this.core.nowPs(),
    };
  }

  async run(onStep: (outcome: StepOutcome) => void = () => {}): Promise<void> {
    this.running = true;
    while (this.running) {
      const outcome = await this.step();
      onStep(outcome);
      if (outcome.kind === "stopped") {
        return;
      }
    }
  }

  stop(): void {
    this.running = false;
  }

  async step(): Promise<StepOutcome> {
    if (this.mode.kind === "Paused") {
      await this.yielder.sleep(IDLE_SLEEP_MS);
      return { kind: "paused" };
    }

    const fromPs = this.core.nowPs();
    let target = this.targetPs(fromPs);

    if (target !== null) {
      if (target - fromPs < this.minRunLeadPs()) {
        const sleptMs = this.aheadSleepMs(fromPs, target);
        await this.yielder.sleep(sleptMs);
        this.sampleRate();
        return { kind: "ahead", sleptMs };
      }
      if (target - fromPs > REANCHOR_LAG_PS) {
        // The guest runs in slow motion and virtual time never jumps, so the anchors move to the present.
        // The next slice is a full one: a target from the fresh anchor would be `fromPs` itself, and the
        // loop would stall exactly when it is furthest behind.
        this.anchor();
        this.reanchorCount += 1;
        target = fromPs + this.sliceVt;
      }
    }

    const untilPs = this.sliceEnd(fromPs, target);
    const wallBefore = this.clock.nowMs();
    const stop = this.core.run(untilPs, this.maxInsns());
    const wallAfter = this.clock.nowMs();
    const toPs = this.core.nowPs();
    this.observe(wallAfter - wallBefore, toPs - fromPs);
    this.sampleRate();

    if (!isLimitStop(stop)) {
      this.mode = { kind: "Paused" };
      this.running = false;
      return { kind: "stopped", stop, atPs: toPs };
    }
    // A slice awaits nothing, so a loop that never gets ahead (`Max`, or `Wall` behind a slow core)
    // would chain slices as microtasks and never read a message. `sleep(0)` gives the message loop a
    // turn without waiting on a clock.
    await this.yielder.sleep(0);
    return { kind: "ran", stop, fromPs, toPs };
  }

  private targetPs(nowPs: bigint): bigint | null {
    switch (this.mode.kind) {
      case "Max":
        this.clockKind = "none";
        return null;
      case "Wall":
        this.clockKind = "wall";
        return this.wallTargetPs(this.mode.rate);
      case "Audio":
        return this.audioTargetPs(this.mode.rate);
      case "Paused":
        return nowPs;
    }
  }

  /**
   * `Audio`: the last consumed sample plus 60 ms while the audio clock is live, the wall clock
   * otherwise. While audio drives, the wall anchor follows it, so a handover has no jump. The target
   * never runs more than {@link AUDIO_LEAD_PS} past the guest, so a guest too slow for real time
   * underruns instead of re-anchoring.
   */
  private audioTargetPs(rate: number): bigint {
    const nowMs = this.clock.nowMs();
    const consumed = this.audio.consumedPs();
    if (consumed !== null && consumed !== this.lastConsumedPs) {
      this.lastConsumedPs = consumed;
      this.lastAdvanceMs = nowMs;
    }
    const live =
      consumed !== null &&
      (this.audio.flowing?.() ?? true) &&
      nowMs - this.lastAdvanceMs <= AUDIO_STALE_MS;
    if (live) {
      const target = consumed + AUDIO_LEAD_PS;
      this.vtAnchorPs = target;
      this.wallAnchorMs = this.lastAdvanceMs;
      this.useClock("audio");
      return target;
    }
    this.useClock("wall");
    return this.wallTargetPs(rate);
  }

  private useClock(kind: PacingClockKind): void {
    if (this.clockKind !== kind && (this.clockKind === "audio" || kind === "audio")) {
      if (this.clockKind !== "none") {
        this.handovers += 1;
      }
    }
    this.clockKind = kind;
  }

  private wallTargetPs(rate: number): bigint {
    const elapsedMs = Math.max(0, this.clock.nowMs() - this.wallAnchorMs);
    const safeRate = Number.isFinite(rate) && rate > 0 ? rate : 1;
    // Microseconds first: the product stays a safe integer for hours, where picoseconds would not.
    const elapsedUs = BigInt(Math.round(elapsedMs * safeRate * 1000));
    return this.vtAnchorPs + elapsedUs * 1_000_000n;
  }

  private aheadSleepMs(nowPs: bigint, target: bigint): number {
    const aheadPs = nowPs + this.minRunLeadPs() - target;
    const aheadMs = Number(aheadPs / 1_000n) / 1_000_000 / this.pacedRate();
    // At most one idle sleep, since an input may arrive and a clamped timer would turn a longer wait
    // into a visible stall. At least 1 ms, since no engine honours less. Overshooting is free: the
    // deadline is absolute from the anchor.
    return Math.min(IDLE_SLEEP_MS, Math.max(1, aheadMs));
  }

  private pacedRate(): number {
    const rate = this.mode.kind === "Wall" || this.mode.kind === "Audio" ? this.mode.rate : 1;
    return Number.isFinite(rate) && rate > 0 ? rate : 1;
  }

  /**
   * How far the target must be past the guest before a slice is worth running: {@link
   * MIN_RUN_LEAD_MS} of host time at the pacing rate, at most one slice. Without it a microsecond
   * clock (Chromium, isolated) finds the target a few microseconds ahead every iteration and runs
   * slices whose fixed per-call cost is most of their wall time: a spin.
   */
  private minRunLeadPs(): bigint {
    // Picoseconds, at least one: through microseconds a rate below 0.0005 gives a lead of 0, and the
    // loop then runs empty slices without sleeping.
    const lead = BigInt(Math.max(1, Math.round(MIN_RUN_LEAD_MS * this.pacedRate() * 1e9)));
    return lead < this.sliceVt ? lead : this.sliceVt;
  }

  private sliceEnd(nowPs: bigint, target: bigint | null): bigint {
    const bounded = nowPs + this.sliceVt;
    if (target === null) {
      return bounded;
    }
    return target < bounded ? target : bounded;
  }

  /** A safety bound so `pemu_run` returns even if the guest never reaches `until_ps`. */
  private maxInsns(): bigint {
    return this.sliceVt / PS_PER_NOMINAL_INSN + 1n;
  }

  private anchor(): void {
    this.vtAnchorPs = this.core.nowPs();
    this.wallAnchorMs = this.clock.nowMs();
    this.lastConsumedPs = null;
    this.windowWallMs = 0;
    this.windowVtPs = 0n;
    this.windowSlices = 0;
    this.rateStartMs = this.wallAnchorMs;
    this.rateStartPs = this.vtAnchorPs;
  }

  /**
   * Updates the real-time factor once {@link RATE_WINDOW_MS} of host time, sleeps included, has
   * passed. An anchor restarts the window, so time spent paused never dilutes it.
   */
  private sampleRate(): void {
    const nowMs = this.clock.nowMs();
    const wallMs = nowMs - this.rateStartMs;
    if (wallMs < RATE_WINDOW_MS) {
      return;
    }
    const nowPs = this.core.nowPs();
    const vtPs = nowPs > this.rateStartPs ? nowPs - this.rateStartPs : 0n;
    this.measuredFactor = Number(vtPs / 1_000n) / 1_000_000 / wallMs;
    this.rateStartMs = nowMs;
    this.rateStartPs = nowPs;
  }

  /** Folds one slice into the run window and, once the window is long enough, updates the headroom. */
  private observe(wallMs: number, vtPs: bigint): void {
    this.windowWallMs += Math.max(0, wallMs);
    this.windowVtPs += vtPs > 0n ? vtPs : 0n;
    this.windowSlices += 1;
    if (this.windowWallMs < RATE_WINDOW_MS || this.windowSlices === 0) {
      return;
    }
    const virtualMs = Number(this.windowVtPs / PS_PER_MS);
    this.measuredHeadroom = virtualMs / this.windowWallMs;
    const perSliceWallMs = this.windowWallMs / this.windowSlices;
    if (perSliceWallMs > 0) {
      const scaled = Number(this.sliceVt) * (MAX_SLICE_WALL_MS / perSliceWallMs);
      this.sliceVt = clampSlice(BigInt(Math.round(scaled)));
    }
    this.windowWallMs = 0;
    this.windowVtPs = 0n;
    this.windowSlices = 0;
  }
}

function clampSlice(value: bigint): bigint {
  if (value < MIN_SLICE_VT_PS) {
    return MIN_SLICE_VT_PS;
  }
  return value > MAX_SLICE_VT_PS ? MAX_SLICE_VT_PS : value;
}

export const performanceClock: PacingClock = {
  nowMs: () => performance.now(),
};

/**
 * The non-isolated yielder on a MessageChannel. A wait shorter than {@link TIMER_FLOOR_MS} yields
 * to the event loop instead of arming a timer that would round up anyway.
 */
export function messageChannelYielder(): Yielder {
  const channel = new MessageChannel();
  const waiters: (() => void)[] = [];
  channel.port1.onmessage = () => {
    waiters.shift()?.();
  };
  channel.port1.start();
  let closed = false;
  return {
    kind: "message-channel",
    sleep(ms: number): Promise<void> {
      if (ms >= TIMER_FLOOR_MS) {
        return new Promise((resolve) => setTimeout(resolve, ms));
      }
      if (closed) {
        return Promise.resolve();
      }
      return new Promise((resolve) => {
        waiters.push(resolve);
        channel.port2.postMessage(0);
      });
    },
    close(): void {
      if (closed) {
        return;
      }
      closed = true;
      channel.port1.onmessage = null;
      channel.port1.close();
      channel.port2.close();
      for (const resolve of waiters.splice(0)) {
        resolve();
      }
    },
  };
}

/**
 * Bytes of the shared input cell: an `Int32` counter the page bumps and notifies after posting an
 * input. It carries no input: inputs stay messages, ordered with every other message.
 */
export const INPUT_CELL_BYTES = 4;

export function createInputCell(): SharedArrayBuffer {
  return new SharedArrayBuffer(INPUT_CELL_BYTES);
}

/** The page's half: call after `worker.postMessage`, so the message is queued when the Worker wakes. */
export function notifyInput(cell: SharedArrayBuffer): void {
  const counter = new Int32Array(cell);
  Atomics.add(counter, 0, 1);
  Atomics.notify(counter, 0);
}

export interface AtomicsYielderParts {
  /** `Atomics.wait`: blocks until the counter moves, a notify arrives, or `ms` pass. */
  readonly wait?: (counter: Int32Array, expected: number, ms: number) => string;
  /** Lets the message loop run once, so a message the wait was woken for arrives before the next slice. */
  readonly yieldTask?: () => Promise<void>;
  /** The loop trace: told each wait (asked, waited, result) and each turn's length. */
  readonly onWait?: (askedMs: number, waitedMs: number, result: string) => void;
  readonly onYield?: (ms: number) => void;
  /** `message-channel` where the `waitAsync` turn starves the Worker's tasks, and as the A/B control. */
  readonly turn?: "wait-async" | "message-channel";
}

/**
 * The spin control for `Atomics.wait`: the same wait, made by spinning on the clock so the thread
 * never gives its core back. A diagnostic that burns a core: it tells whether a loop stall is the
 * host's treatment of a sleeping thread rather than work the loop does.
 */
export function spinWait(counter: Int32Array, expected: number, ms: number): string {
  const end = performance.now() + ms;
  while (performance.now() < end) {
    if (Atomics.load(counter, 0) !== expected) {
      return "spin-ok";
    }
  }
  return "spin-timed-out";
}

/** `Atomics.waitAsync` (ES2024), which the ES2022 `lib` this package compiles against leaves out. */
type WaitAsync = (
  view: Int32Array,
  index: number,
  value: number,
  timeoutMs: number,
) => { readonly async: boolean; readonly value: Promise<string> | string };

/** Bound on a {@link waitAsyncTurn} whose own notify was lost; never expected to be hit. */
const TURN_TIMEOUT_MS = TIMER_FLOOR_MS;

/**
 * One turn of the Worker's event loop: `Atomics.waitAsync` on a private cell, notified at once. In
 * Chromium and WebKit it resolves from a task, so queued messages are delivered first; Firefox does
 * not ({@link turnRunsTasks}). `null` without `waitAsync`. Preferred to a MessageChannel self-post
 * because WebKit routes that through the page's main thread: with the main thread busy for 50 ms, a
 * self-post took up to 50.8 ms in WebKit, and this turn at most 0.16 ms.
 */
export function waitAsyncTurn(
  waitAsync: WaitAsync | null = (Atomics as unknown as { waitAsync?: WaitAsync }).waitAsync ?? null,
): (() => Promise<void>) | null {
  if (typeof waitAsync !== "function") {
    return null;
  }
  const cell = new Int32Array(new SharedArrayBuffer(4));
  return () => {
    const pending = waitAsync.call(Atomics, cell, 0, 0, TURN_TIMEOUT_MS);
    Atomics.notify(cell, 0);
    return typeof pending.value === "string" ? Promise.resolve() : pending.value.then(() => undefined);
  };
}

/**
 * How long {@link turnRunsTasks} turns before deciding the turn starves the task queue. A wrong
 * "starves" only costs the MessageChannel turn, which every engine can run.
 */
export const TURN_PROBE_MS = 100;

export interface TurnProbeParts {
  readonly queue?: (run: () => void) => void;
  readonly now?: () => number;
  readonly budgetMs?: number;
}

/**
 * Whether `turn` lets tasks queued on this thread run (a page message, a timer, an `OffscreenCanvas`
 * commit). Firefox resolves a self-notified `Atomics.waitAsync` without running the Worker's other
 * tasks, so such a loop never receives a message and never shows a frame. The probe tests the engine
 * rather than its name, with a timer rather than a self-post, since WebKit carries a self-post
 * through the busy main thread.
 */
export async function turnRunsTasks(turn: () => Promise<void>, parts: TurnProbeParts = {}): Promise<boolean> {
  const queue = parts.queue ?? ((run: () => void) => void setTimeout(run, 0));
  const now = parts.now ?? (() => performance.now());
  const end = now() + (parts.budgetMs ?? TURN_PROBE_MS);
  let ran = false;
  queue(() => {
    ran = true;
  });
  while (!ran && now() < end) {
    await turn();
  }
  return ran;
}

/**
 * The cross-origin isolated yielder: `Atomics.wait` on the input cell, a real sleep with no 16 ms
 * clamp, woken early by {@link notifyInput}. After the wait it takes one {@link waitAsyncTurn},
 * since a blocked Worker delivers no message. A counter that moved since the last sleep means an
 * input is queued: no wait at all.
 */
export function atomicsYielder(cell: SharedArrayBuffer, parts: AtomicsYielderParts = {}): Yielder {
  const counter = new Int32Array(cell);
  const wait =
    parts.wait ?? ((view: Int32Array, expected: number, ms: number) => Atomics.wait(view, 0, expected, ms));
  const turn = parts.yieldTask || parts.turn === "message-channel" ? null : waitAsyncTurn();
  const channel = parts.yieldTask || turn ? null : messageChannelYielder();
  const yieldTask = parts.yieldTask ?? turn ?? (() => channel?.sleep(0) ?? Promise.resolve());
  let seen = Atomics.load(counter, 0);
  return {
    kind: "atomics-wait",
    turn: parts.yieldTask ? undefined : turn ? "wait-async" : "message-channel",
    close(): void {
      channel?.close?.();
    },
    async sleep(ms: number): Promise<void> {
      const now = Atomics.load(counter, 0);
      if (now === seen && ms > 0) {
        if (parts.onWait) {
          const before = performance.now();
          const result = wait(counter, now, ms);
          parts.onWait(ms, performance.now() - before, result);
        } else {
          wait(counter, now, ms);
        }
      }
      seen = Atomics.load(counter, 0);
      if (parts.onYield) {
        const before = performance.now();
        await yieldTask();
        parts.onYield(performance.now() - before);
      } else {
        await yieldTask();
      }
    },
  };
}
