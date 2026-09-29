// The Perf tab: what this session measured (busy MIPS, demand per 100 ms window, worst window).
// Browser clocks are coarse (timers clamped to about 16 ms, `performance.now()` whole milliseconds
// in Safari, `specs/notes/g3-behavior.md`), so the tab reports the granularity it observed next
// to every number derived from wall time.

import type { PacingStats } from "../../worker/pacing";

export const WINDOW_MS = 100;

export const WINDOW_HISTORY = 100;

export interface PerfSample {
  readonly hostMs: number;
  readonly vtPs: bigint;
  /** Virtual milliseconds per host millisecond over the window. */
  readonly realTimeFactor: number;
  readonly reanchors: number;
}

export interface PerfReport {
  readonly samples: readonly PerfSample[];
  readonly current: number;
  readonly worst: number;
  readonly median: number;
  readonly virtualMsPerSecond: number;
  /** Re-anchors over the history; a rising count means the loop cannot keep up. */
  readonly reanchors: number;
  /**
   * The smallest non-zero host-time step seen between two samples. On a coarse clock this is about
   * 16 ms and quantises every factor; the tab says so.
   */
  readonly clockGranularityMs: number;
}

/** Folded into windows here, not in the Worker, so measuring does not compete with the slice budget. */
export class PerfHistory {
  private readonly samples: PerfSample[] = [];
  private windowStartMs: number | null = null;
  private granularity = Number.POSITIVE_INFINITY;
  private lastHostMs: number | null = null;

  constructor(
    private readonly windowMs: number = WINDOW_MS,
    private readonly history: number = WINDOW_HISTORY,
  ) {}

  /**
   * Folds one slice report in; returns the sample when it closed a window. Windows close on host
   * time: closed on virtual time, they would shrink as the machine slowed.
   */
  push(hostMs: number, stats: PacingStats): PerfSample | null {
    if (this.lastHostMs !== null) {
      const step = hostMs - this.lastHostMs;
      if (step > 0) {
        this.granularity = Math.min(this.granularity, step);
      }
    }
    this.lastHostMs = hostMs;
    if (this.windowStartMs === null) {
      this.windowStartMs = hostMs;
      return null;
    }
    if (hostMs - this.windowStartMs < this.windowMs) {
      return null;
    }
    this.windowStartMs = hostMs;
    const sample: PerfSample = {
      hostMs,
      vtPs: stats.nowPs,
      realTimeFactor: stats.realTimeFactor,
      reanchors: stats.reanchors,
    };
    this.samples.push(sample);
    while (this.samples.length > this.history) {
      this.samples.shift();
    }
    return sample;
  }

  clear(): void {
    this.samples.length = 0;
    this.windowStartMs = null;
    this.lastHostMs = null;
    this.granularity = Number.POSITIVE_INFINITY;
  }

  report(): PerfReport {
    const factors = this.samples.map((sample) => sample.realTimeFactor);
    const first = this.samples[0];
    const last = this.samples[this.samples.length - 1];
    const spanMs = first && last ? last.hostMs - first.hostMs : 0;
    const advancedMs = first && last ? Number((last.vtPs - first.vtPs) / 1_000_000_000n) : 0;
    return {
      samples: this.samples,
      current: last?.realTimeFactor ?? 0,
      worst: factors.length === 0 ? 0 : Math.min(...factors),
      median: median(factors),
      virtualMsPerSecond: spanMs > 0 ? (advancedMs * 1_000) / spanMs : 0,
      reanchors: last && first ? last.reanchors - first.reanchors : 0,
      clockGranularityMs: Number.isFinite(this.granularity) ? this.granularity : 0,
    };
  }
}

export function median(values: readonly number[]): number {
  if (values.length === 0) {
    return 0;
  }
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  if (sorted.length % 2 === 1) {
    return sorted[middle] ?? 0;
  }
  return ((sorted[middle - 1] ?? 0) + (sorted[middle] ?? 0)) / 2;
}

/** Whether a window spans at least about four clock ticks; finer is quantisation noise. */
export function isMeaningful(windowMs: number, granularityMs: number): boolean {
  return granularityMs > 0 && windowMs >= 4 * granularityMs;
}

export function clockNote(granularityMs: number): string {
  if (granularityMs <= 0) {
    return "clock granularity not yet measured";
  }
  return `host clock steps of about ${granularityMs.toFixed(1)} ms; factors below are quantised by it`;
}
