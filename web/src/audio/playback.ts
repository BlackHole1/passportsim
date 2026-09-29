// The PCM pump and the `Audio` pacing anchor. The pump moves `audio_out` samples from wasm memory
// into the transport after each slice, and keeps enough record headers to answer what virtual time
// the worklet's last consumed sample carried. That anchor counts samples, so it is immune to the
// host clock's resolution.

import { RingId } from "../worker/layout";
import type { IoViews, PcmRecordHeader } from "../worker/ring";
import type { PcmTransport } from "./transport";
import { UNDERRUN_FILL_MS } from "./levels";
import { TRACE_STALL_MS, type TraceBurst, type TraceSink } from "./trace";

/**
 * Two pushed runs are one burst when the second starts within this much virtual time of the
 * first's end: an I2S TX period is 15 ms on the demo's grid.
 */
const BURST_GAP_PS = 40_000_000_000n;

interface OpenBurst {
  transportFirst: bigint;
  ringFirst: bigint;
  samples: number;
  zeros: number;
  peak: number;
  vtFirstPs: bigint;
  vtEndPs: bigint;
  fs: number;
  channels: number;
}

const PS_PER_S = 1_000_000_000_000n;

/** How many samples one pump call moves at most, so a slice never stalls on a huge backlog. */
export const MAX_PUMP_SAMPLES = 8192;

/**
 * The most record headers kept. `pump` already forgets runs never pushed, so this only bounds a
 * pathological stream (a format change per sample), dropping the oldest.
 */
export const MAX_KEPT_RECORDS = 4096;

export interface PlaybackStats {
  readonly pushed: bigint;
  /** Samples the core produced that the transport could not take. */
  readonly dropped: bigint;
  /** Samples the core evicted before the pump read them. */
  readonly lost: bigint;
  readonly buffered: number;
  /** Rate of the newest record read, or `null` before one. The guest switches between 16 and 24 kHz. */
  readonly guestRate: number | null;
  /**
   * Loudest sample pushed since the last {@link PcmPump.stats} call, 0 to 1, for the output meter.
   * Peak rather than RMS, because a short click shows only in the peak.
   */
  readonly peak: number;
}

interface Segment {
  readonly transport: bigint;
  readonly offset: bigint;
}

/**
 * Moves guest PCM into the transport and maps consumed samples back to virtual time. All record
 * headers are kept because the guest changes the I2S rate at runtime. The `audio_out` ring and the
 * transport number samples differently once any is lost or dropped, so a segment list maps one
 * to the other.
 */
export class PcmPump {
  private sampleCursor = 0n;
  private recordCursor = 0n;
  private records: PcmRecordHeader[] = [];
  private segments: Segment[] = [];
  private unplaced: PcmRecordHeader[] = [];
  private lastFormat: { fs: number; channels: number } | null = null;
  private pushedFormat: { fs: number; channels: number } | null = null;
  private pushed = 0n;
  private pushedRingEnd = 0n;
  private dropped = 0n;
  private lost = 0n;
  private peak = 0;
  private trace: TraceSink = null;
  private burst: OpenBurst | null = null;
  private lastPushMs: number | null = null;

  constructor(
    private readonly views: IoViews,
    private readonly transport: PcmTransport,
  ) {}

  get keptRecords(): number {
    return this.records.length;
  }

  /** The rate of the newest record read (`PcmRecordAbi::fs`), or `null` before the first one. */
  get currentRate(): number | null {
    return this.lastFormat?.fs ?? null;
  }

  /** Turns the audio trace on: every record header read and every burst pushed goes to `sink`. */
  setTrace(sink: TraceSink): void {
    this.trace = sink;
  }

  /**
   * Reads what the core produced and pushes what the transport takes. With the trace on, a burst whose
   * last sample is more than {@link BURST_GAP_PS} behind `nowPs` is reported closed.
   */
  pump(nowPs?: bigint): void {
    const headers = this.views.readPcmRecords(
      RingId.AudioOutRecords,
      this.recordCursor,
      MAX_PUMP_SAMPLES,
    );
    this.recordCursor = headers.next;
    for (const record of headers.items) {
      this.records.push(record);
      this.trace?.({
        src: "pump",
        kind: "record",
        vtPs: record.vtStartPs.toString(),
        first: record.first.toString(),
        fs: record.fs,
        channels: record.channels,
      });
      if (record.fs > 0 && (this.lastFormat?.fs !== record.fs || this.lastFormat.channels !== record.channels)) {
        this.lastFormat = { fs: record.fs, channels: Math.max(1, record.channels) };
        this.unplaced.push(record);
      }
    }
    this.forgetConsumedRecords();

    const read = this.views.readSamples(RingId.AudioOutSamples, this.sampleCursor, MAX_PUMP_SAMPLES);
    this.lost += read.dropped;
    this.sampleCursor = read.next;
    const items = read.items;
    if (items.length === 0) {
      if (this.trace && this.burst && nowPs !== undefined && nowPs - this.burst.vtEndPs > BURST_GAP_PS) {
        this.closeBurst();
      }
      return;
    }
    // The peak of this read; `stats()` clears it, so a slow reader sees the loudest of what it missed.
    for (const sample of items) {
      const level = Math.abs(sample) / 32_768;
      if (level > this.peak) {
        this.peak = level;
      }
    }
    const ringStart = read.next - BigInt(items.length);
    let index = 0;
    // An eviction moves the tail by samples, not frames, so a late reader can start mid-frame; those
    // slots are lost with their frame.
    if (read.dropped > 0n) {
      const run = this.recordOf(ringStart);
      const width = BigInt(Math.max(1, run?.channels ?? 1));
      const into = run ? (ringStart - run.first) % width : 0n;
      if (into !== 0n) {
        index = Math.min(items.length, Number(width - into));
        this.lost += BigInt(index);
      }
    }
    // One push per run, cut to whole frames of its own width, so a full transport never splits a frame.
    while (index < items.length) {
      const cursor = ringStart + BigInt(index);
      const { width, end } = this.runAt(cursor, read.next);
      const runLength = Number(end - cursor);
      const room = this.transport.freeSamples();
      const offer = room >= runLength ? runLength : room - (room % width);
      if (offer <= 0) {
        break;
      }
      const transportStart = this.transport.producedSamples();
      // The mark goes in ahead of its samples, so the worklet never reads one at the old rate.
      if (!this.place(cursor, BigInt(offer), transportStart)) {
        break;
      }
      const taken = this.transport.push(items.subarray(index, index + offer));
      if (taken > 0) {
        const offset = cursor - transportStart;
        const last = this.segments[this.segments.length - 1];
        if (!last || last.offset !== offset) {
          this.segments.push({ transport: transportStart, offset });
        }
      }
      if (this.trace && taken > 0) {
        this.traceBurst(items.subarray(index, index + taken), cursor, transportStart);
      }
      index += taken;
      this.pushed += BigInt(taken);
      if (taken > 0) {
        this.pushedRingEnd = cursor + BigInt(taken);
      }
      if (taken < runLength) {
        break;
      }
    }
    this.dropped += BigInt(items.length - index);
    this.forgetUnpushedRecords();
  }

  private traceBurst(run: Int16Array, ringFirst: bigint, transportFirst: bigint): void {
    const nowMs = performance.now();
    if (this.burst && this.lastPushMs !== null && nowMs - this.lastPushMs > TRACE_STALL_MS) {
      this.trace?.({
        src: "pump",
        kind: "stall",
        hostMs: Math.round(nowMs - this.lastPushMs),
        transportFirst: transportFirst.toString(),
        buffered: Math.max(0, this.transport.bufferedSamples() - run.length),
      });
    }
    this.lastPushMs = nowMs;
    const record = this.recordOf(ringFirst);
    const fs = record?.fs ?? 0;
    const channels = Math.max(1, record?.channels ?? 1);
    const vtFirstPs = this.timeOfSample(ringFirst) ?? 0n;
    const vtEndPs =
      fs > 0 ? vtFirstPs + (BigInt(Math.ceil(run.length / channels)) * PS_PER_S) / BigInt(fs) : vtFirstPs;
    let zeros = 0;
    let peak = 0;
    for (const sample of run) {
      if (sample === 0) {
        zeros += 1;
      }
      peak = Math.max(peak, Math.abs(sample));
    }
    const open = this.burst;
    if (
      open &&
      open.fs === fs &&
      open.channels === channels &&
      vtFirstPs >= open.vtEndPs - BURST_GAP_PS &&
      vtFirstPs - open.vtEndPs <= BURST_GAP_PS
    ) {
      open.samples += run.length;
      open.zeros += zeros;
      open.peak = Math.max(open.peak, peak);
      open.vtEndPs = vtEndPs;
      return;
    }
    this.closeBurst();
    this.burst = { transportFirst, ringFirst, samples: run.length, zeros, peak, vtFirstPs, vtEndPs, fs, channels };
    this.trace?.(this.burstEntry(this.burst, "open"));
  }

  private closeBurst(): void {
    if (this.burst) {
      this.trace?.(this.burstEntry(this.burst, "closed"));
      this.burst = null;
    }
  }

  private burstEntry(burst: OpenBurst, state: "open" | "closed"): TraceBurst {
    return {
      src: "pump",
      kind: "burst",
      state,
      transportFirst: burst.transportFirst.toString(),
      ringFirst: burst.ringFirst.toString(),
      samples: burst.samples,
      zeros: burst.zeros,
      peak: burst.peak,
      vtFirstPs: burst.vtFirstPs.toString(),
      vtEndPs: burst.vtEndPs.toString(),
      fs: burst.fs,
      channels: burst.channels,
    };
  }

  /**
   * Forgets headers whose samples were all read and none pushed: no consumed cursor can land in them.
   * This bounds the list while the worklet is suspended and every slice drops its samples.
   */
  private forgetUnpushedRecords(): void {
    const kept: PcmRecordHeader[] = [];
    const last = this.records.length - 1;
    this.records.forEach((record, index) => {
      const end = this.records[index + 1]?.first;
      const neverPushed =
        index < last && end !== undefined && end <= this.sampleCursor && record.first >= this.pushedRingEnd;
      if (!neverPushed) {
        kept.push(record);
      }
    });
    if (kept.length > MAX_KEPT_RECORDS) {
      kept.splice(0, kept.length - MAX_KEPT_RECORDS);
    }
    this.records = kept;
  }

  private runAt(cursor: bigint, limit: bigint): { width: number; end: bigint } {
    const at = this.indexOfRun(cursor);
    const record = at >= 0 ? this.records[at] : undefined;
    const next = this.records[at + 1];
    const end = next && next.first < limit ? next.first : limit;
    return { width: Math.max(1, record?.channels ?? this.lastFormat?.channels ?? 1), end };
  }

  /**
   * Writes a format mark for every unplaced change inside the run about to be pushed. Returns `false`
   * when the transport has no room for a mark yet; the run must then wait, or it would play at the
   * wrong rate.
   */
  private place(ringStart: bigint, count: bigint, transportStart: bigint): boolean {
    while (this.unplaced.length > 0) {
      const record = this.unplaced[0];
      if (!record || record.first >= ringStart + count) {
        break;
      }
      const into = record.first > ringStart ? record.first - ringStart : 0n;
      const format = { fs: record.fs, channels: Math.max(1, record.channels) };
      if (this.pushedFormat?.fs !== format.fs || this.pushedFormat.channels !== format.channels) {
        const mark = { guestRate: format.fs, channels: format.channels, at: transportStart + into };
        if (!this.transport.pushFormat(mark)) {
          return false;
        }
        this.pushedFormat = format;
      }
      this.unplaced.shift();
    }
    return true;
  }

  /** The virtual time of the last sample the worklet consumed, or `null` before the first: the `Audio` anchor. */
  consumedPs(): bigint | null {
    const consumed = this.transport.consumedSamples();
    if (consumed === 0n) {
      return null;
    }
    const ring = this.ringCursorOf(consumed - 1n);
    return ring === null ? null : this.timeOfSample(ring);
  }

  /**
   * Whether the worklet has at least the underrun floor buffered. If not, the consumed count will
   * stall, and `Audio` pacing must hand over to the wall clock rather than wait.
   */
  flowing(): boolean {
    const format = this.pushedFormat ?? this.lastFormat;
    if (!format) {
      return false;
    }
    const floor = Math.round((UNDERRUN_FILL_MS * format.fs) / 1000) * format.channels;
    return this.transport.bufferedSamples() >= Math.max(1, floor);
  }

  /** Sample-exact virtual time of one absolute `audio_out` cursor. */
  timeOfSample(cursor: bigint): bigint | null {
    const record = this.recordOf(cursor);
    if (!record || record.fs === 0 || record.channels === 0) {
      return record ? record.vtStartPs : null;
    }
    const frame = (cursor - record.first) / BigInt(record.channels);
    return record.vtStartPs + (frame * PS_PER_S) / BigInt(record.fs);
  }

  stats(): PlaybackStats {
    const peak = this.peak;
    this.peak = 0;
    return {
      pushed: this.pushed,
      dropped: this.dropped,
      lost: this.lost,
      buffered: this.transport.bufferedSamples(),
      guestRate: this.currentRate,
      peak,
    };
  }

  private ringCursorOf(transportCursor: bigint): bigint | null {
    let found: Segment | null = null;
    for (const segment of this.segments) {
      if (segment.transport <= transportCursor) {
        found = segment;
      } else {
        break;
      }
    }
    return found ? transportCursor + found.offset : null;
  }

  private recordOf(cursor: bigint): PcmRecordHeader | null {
    return this.records[this.indexOfRun(cursor)] ?? null;
  }

  private indexOfRun(cursor: bigint): number {
    let low = 0;
    let high = this.records.length - 1;
    let found = -1;
    while (low <= high) {
      const middle = (low + high) >> 1;
      const record = this.records[middle];
      if (record && record.first <= cursor) {
        found = middle;
        low = middle + 1;
      } else {
        high = middle - 1;
      }
    }
    return found;
  }

  /**
   * Drops headers and segments no consumed sample can refer to; the newest of each is kept. The anchor
   * asks about `consumed - 1`, so a record is forgotten only once the next one owns that sample too;
   * comparing with `consumed` would drop the anchor's record at every exact boundary.
   */
  private forgetConsumedRecords(): void {
    const consumed = this.transport.consumedSamples();
    if (consumed === 0n) {
      return;
    }
    const anchor = consumed - 1n;
    while (this.segments.length > 1 && (this.segments[1]?.transport ?? anchor + 1n) <= anchor) {
      this.segments.shift();
    }
    const ringAnchor = this.ringCursorOf(anchor);
    if (ringAnchor === null) {
      return;
    }
    while (this.records.length > 1) {
      const next = this.records[1];
      if (!next || next.first > ringAnchor) {
        break;
      }
      this.records.shift();
    }
  }
}
