// A trace of the pacing loop's iterations, to explain a stall of the playback producer. One
// iteration: `flush` input, then either `wait` and `yield` (the message turn, with its `handler`s)
// or `run` a slice and `yield`, then `pump` (with `present`) and `report`; `gap` is host time
// between iterations. All host milliseconds on the Worker's `performance.now()`. Off unless the
// page asked for the audio trace.

export interface LoopIteration {
  readonly atMs: number;
  readonly kind: "ahead" | "ran" | "paused" | "stopped";
  readonly gapMs: number;
  readonly flushMs: number;
  /** The sleep the loop asked the yielder for, 0 for a slice's `sleep(0)`. */
  readonly askedMs: number;
  readonly waitMs: number;
  readonly waitResult: string;
  readonly yieldMs: number;
  readonly runMs: number;
  readonly vtUs: number;
  readonly pumpMs: number;
  readonly presentMs: number;
  readonly reportMs: number;
  readonly handlerMs: number;
  readonly handlers: number;
  readonly handled: string;
}

export interface LoopSummary {
  readonly iterations: number;
  readonly ran: number;
  readonly ahead: number;
  readonly vtUs: number;
  readonly spanMs: number;
  readonly sum: Readonly<Record<LoopTerm, number>>;
  readonly max: Readonly<Record<LoopTerm, number>>;
  readonly lateWaits5: number;
  readonly lateWaits20: number;
  readonly worstOvershootMs: number;
}

export const LOOP_TERMS = [
  "gapMs",
  "flushMs",
  "waitMs",
  "yieldMs",
  "runMs",
  "pumpMs",
  "presentMs",
  "reportMs",
  "handlerMs",
] as const;
export type LoopTerm = (typeof LOOP_TERMS)[number];

type Mutable<T> = { -readonly [K in keyof T]: T[K] };

export const LOOP_RING = 512;

export class LoopRecorder {
  private readonly ring: LoopIteration[] = [];
  private current: Mutable<LoopIteration> | null = null;
  private lastEndMs: number | null = null;
  private summaryFrom = 0;
  private window: LoopIteration[] = [];

  constructor(private readonly nowMs: () => number = () => performance.timeOrigin + performance.now()) {}

  /** Opens an iteration, closing one still open as `paused`. */
  begin(): void {
    if (this.current) {
      this.end("paused", 0);
    }
    const at = this.nowMs();
    this.current = {
      atMs: at,
      kind: "paused",
      gapMs: this.lastEndMs === null ? 0 : at - this.lastEndMs,
      flushMs: 0,
      askedMs: 0,
      waitMs: 0,
      waitResult: "skip",
      yieldMs: 0,
      runMs: 0,
      vtUs: 0,
      pumpMs: 0,
      presentMs: 0,
      reportMs: 0,
      handlerMs: 0,
      handlers: 0,
      handled: "",
    };
  }

  add(term: "flushMs" | "yieldMs" | "pumpMs" | "presentMs" | "reportMs", ms: number): void {
    if (this.current) {
      this.current[term] += ms;
    }
  }

  /** The pacing step took `ms` in all; {@link end} takes the wait and yield out of the run term. */
  stepped(ms: number): void {
    if (this.current) {
      this.current.runMs += ms;
    }
  }

  waited(askedMs: number, waitMs: number, result: string): void {
    if (this.current) {
      this.current.askedMs += askedMs;
      this.current.waitMs += waitMs;
      this.current.waitResult = result;
    }
  }

  /** One message handler ran for `ms`. Outside an iteration it is kept for the next one. */
  handled(type: string, ms: number): void {
    if (!this.current) {
      this.begin();
    }
    const current = this.current as Mutable<LoopIteration>;
    current.handlerMs += ms;
    current.handlers += 1;
    current.handled = current.handled === "" ? type : `${current.handled},${type}`;
  }

  end(kind: LoopIteration["kind"], vtUs: number): LoopIteration | null {
    const current = this.current;
    if (!current) {
      return null;
    }
    this.current = null;
    current.kind = kind;
    current.vtUs = vtUs;
    current.runMs = Math.max(0, current.runMs - current.waitMs - current.yieldMs);
    const closed: LoopIteration = { ...current };
    this.lastEndMs = this.nowMs();
    this.ring.push(closed);
    if (this.ring.length > LOOP_RING) {
      this.ring.splice(0, this.ring.length - LOOP_RING);
    }
    this.window.push(closed);
    return closed;
  }

  since(fromMs: number): LoopIteration[] {
    return this.ring.filter((iteration, index) => {
      const next = this.ring[index + 1];
      const end = next ? next.atMs - next.gapMs : (this.lastEndMs ?? iteration.atMs);
      return end >= fromMs;
    });
  }

  /** The summary of iterations closed since the last call, or `null` before `everyMs` has passed. */
  summary(everyMs: number): LoopSummary | null {
    const now = this.nowMs();
    if (this.window.length === 0 || now - this.summaryFrom < everyMs) {
      return null;
    }
    this.summaryFrom = now;
    const window = this.window;
    this.window = [];
    return summarize(window);
  }
}

export function summarize(iterations: readonly LoopIteration[]): LoopSummary {
  const sum = Object.fromEntries(LOOP_TERMS.map((term) => [term, 0])) as Record<LoopTerm, number>;
  const max = Object.fromEntries(LOOP_TERMS.map((term) => [term, 0])) as Record<LoopTerm, number>;
  let ran = 0;
  let ahead = 0;
  let vtUs = 0;
  let lateWaits5 = 0;
  let lateWaits20 = 0;
  let worstOvershootMs = 0;
  for (const iteration of iterations) {
    for (const term of LOOP_TERMS) {
      sum[term] += iteration[term];
      max[term] = Math.max(max[term], iteration[term]);
    }
    if (iteration.kind === "ran") {
      ran += 1;
    } else if (iteration.kind === "ahead") {
      ahead += 1;
    }
    vtUs += iteration.vtUs;
    if (iteration.waitResult !== "skip") {
      const over = iteration.waitMs - iteration.askedMs;
      worstOvershootMs = Math.max(worstOvershootMs, over);
      if (over > 5) {
        lateWaits5 += 1;
      }
      if (over > 20) {
        lateWaits20 += 1;
      }
    }
  }
  const first = iterations[0];
  const last = iterations[iterations.length - 1];
  const lastMs =
    last === undefined
      ? 0
      : last.atMs +
        last.flushMs +
        last.waitMs +
        last.yieldMs +
        last.runMs +
        last.pumpMs +
        last.reportMs;
  return {
    iterations: iterations.length,
    ran,
    ahead,
    vtUs,
    spanMs: first === undefined ? 0 : lastMs - first.atMs,
    sum,
    max,
    lateWaits5,
    lateWaits20,
    worstOvershootMs,
  };
}
