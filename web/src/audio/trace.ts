// A trace of every run of samples through the playback ring, in the transport's own numbering at
// both ends (the Worker's pump and the playback worklet), so a dropout the worklet counts can be
// traced back to the guest records and their virtual time. Off unless the page sets
// `globalThis.__PEMU_AUDIO_TRACE__ = true`; when off it costs one null check per call.

import type { LoopIteration, LoopSummary } from "../worker/loopTrace";

export interface TraceRecord {
  readonly src: "pump";
  readonly kind: "record";
  readonly vtPs: string;
  readonly first: string;
  readonly fs: number;
  readonly channels: number;
}

/**
 * A run of pushed samples contiguous in virtual time. `open` when its first samples go in;
 * `closed` when the next push is not contiguous, or on flush.
 */
export interface TraceBurst {
  readonly src: "pump";
  readonly kind: "burst";
  readonly state: "open" | "closed";
  /** Transport cursor of the burst's first sample: the numbering the worklet counts in. */
  readonly transportFirst: string;
  readonly ringFirst: string;
  readonly samples: number;
  readonly zeros: number;
  readonly peak: number;
  readonly vtFirstPs: string;
  readonly vtEndPs: string;
  readonly fs: number;
  readonly channels: number;
}

/** A gap between two pushes of one burst longer than {@link TRACE_STALL_MS}: the producer was late. */
export interface TraceStall {
  readonly src: "pump";
  readonly kind: "stall";
  readonly hostMs: number;
  readonly transportFirst: string;
  readonly buffered: number;
}

export const TRACE_STALL_MS = 30;

export interface TraceWorklet {
  readonly src: "worklet";
  /**
   * `first-samples`: the first quantum with anything in the ring. `playing`: fill reached
   * `TARGET_FILL_MS`, or a tail. `underrun`: a starved quantum while playing. `tail`: played below
   * the floor after production stopped. `level`: every {@link TRACE_LEVEL_QUANTA} quanta, with the
   * window's lowest `fill`.
   */
  readonly kind: "first-samples" | "playing" | "underrun" | "tail" | "level";
  readonly quantum: number;
  readonly consumed: string;
  readonly fill: number;
  readonly idleQuanta: number;
  readonly underruns: number;
}

/** The loop iterations over one {@link TraceStall}, from the push before it to the push that ended it. */
export interface TraceLoopWindow {
  readonly src: "loop";
  readonly kind: "window";
  readonly fromMs: number;
  readonly stallMs: number;
  /** How the loop waits: `atomics-wait`, `spin` (the control) or `message-channel`. */
  readonly yielder: string;
  readonly iterations: readonly LoopIteration[];
}

export interface TraceLoopSummary extends LoopSummary {
  readonly src: "loop";
  readonly kind: "summary";
  readonly yielder: string;
}

export type AudioTraceEntry = (
  | TraceRecord
  | TraceBurst
  | TraceStall
  | TraceWorklet
  | TraceLoopWindow
  | TraceLoopSummary
) & { atMs?: number };

export const TRACE_FLAG = "__PEMU_AUDIO_TRACE__";
export const TRACE_LOG = "__pemuAudioTrace";

/** Makes the page boot its Worker with the spin control; a diagnostic that burns a core. */
export const SPIN_WAIT_FLAG = "__PEMU_SPIN_WAIT__";

/** Makes the page boot its Worker with the MessageChannel turn; a diagnostic. */
export const CHANNEL_TURN_FLAG = "__PEMU_CHANNEL_TURN__";

export const TRACE_LIMIT = 4096;

export const TRACE_LEVEL_QUANTA = 64;

export type TraceSink = ((entry: AudioTraceEntry) => void) | null;
