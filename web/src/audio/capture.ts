// Microphone capture into the guest's I2S RX path. The capture worklet resamples to the guest rate
// and writes `i16` into a ring; this side drains it between slices and journals every chunk as an
// `InputEvent::MicChunk`, so a live session and its replay see the same samples at the same virtual
// times. The mic is requested without browser DSP, which would hide the firmware's own AEC and NS.

import type { InputStamper } from "../worker/input";
import type { PcmSource } from "./transport";

/** What {@link MicCapture} drains: a `SharedRingTransport` when isolated, else a `PortSource`. */
export type CaptureSource = PcmSource;

export const MIC_CONSTRAINTS: MediaTrackConstraints = {
  channelCount: 1,
  echoCancellation: false,
  noiseSuppression: false,
  autoGainControl: false,
};

/**
 * Frames per journaled chunk: one BSP I2S DMA buffer (`dma_frame_num=240`), the chunk size `mic_set`
 * journals (`crates/pemu-api/src/commands/mic_set.rs` `CHUNK_FRAMES`), so live and scripted chunks
 * look the same to the RX path.
 */
export const CHUNK_FRAMES = 240;

export const MAX_CAPTURE_SAMPLES = 4096;

/**
 * The most frames allowed to wait in the capture ring; past it the drain keeps only the newest
 * {@link MIC_KEEP_FRAMES}, since latency matters more than completeness. 256 ms at 16 kHz.
 */
export const MIC_BACKLOG_FRAMES = 4096;

export const MIC_KEEP_FRAMES = 1920;

export interface CaptureStats {
  readonly chunks: number;
  readonly samples: bigint;
  /** Frames waiting: in the capture ring, plus the part of a chunk not yet full. */
  readonly buffered: number;
  readonly nextSeq: bigint;
  readonly droppedFrames: bigint;
}

export interface MicCaptureOptions {
  /** Slots per frame, 1 or 2; like `mic_set`'s `interleave`, every slot carries the same sample. */
  readonly channels?: number;
  /**
   * `seq` of the first chunk. The journal reads `MicChunk::seq` as one per-run count from 0 and a
   * gap as lost data, so live chunks that follow `mic_set` chunks must continue their count.
   */
  readonly firstSeq?: bigint;
}

/**
 * Drains the capture ring and journals whole {@link CHUNK_FRAMES} chunks, each with a `seq`, so a
 * replay can tell a dropped chunk from a silent one. {@link MicCapture.flush} journals a short last
 * chunk when the stream ends, as `mic_set` does.
 */
export class MicCapture {
  private seq: bigint;
  private chunks = 0;
  private samples = 0n;
  private droppedFrames = 0n;
  private readonly channels: number;
  private readonly partial = new Int16Array(CHUNK_FRAMES);
  private partialFill = 0;

  constructor(
    private readonly ring: CaptureSource,
    private readonly stamper: InputStamper,
    options: MicCaptureOptions = {},
  ) {
    this.channels = options.channels === 2 ? 2 : 1;
    this.seq = options.firstSeq ?? 0n;
  }

  /** Moves new frames into the chunk being filled and journals every full chunk; returns frames journaled. */
  drain(): number {
    this.dropBacklog();
    let moved = 0;
    let journaled = 0;
    while (moved < MAX_CAPTURE_SAMPLES) {
      const room = Math.min(CHUNK_FRAMES - this.partialFill, MAX_CAPTURE_SAMPLES - moved);
      const taken = this.ring.pull(this.partial.subarray(this.partialFill, this.partialFill + room));
      if (taken === 0) {
        break;
      }
      moved += taken;
      this.partialFill += taken;
      if (this.partialFill === CHUNK_FRAMES) {
        journaled += this.journal();
      }
    }
    return journaled;
  }

  /** The stream ended: drains the ring and journals the partial chunk; returns frames journaled. */
  flush(): number {
    const drained = this.drain();
    return drained + (this.partialFill > 0 ? this.journal() : 0);
  }

  stats(): CaptureStats {
    return {
      chunks: this.chunks,
      samples: this.samples,
      buffered: this.ring.bufferedSamples() + this.partialFill,
      nextSeq: this.seq,
      droppedFrames: this.droppedFrames,
    };
  }

  /**
   * Drops the oldest frames of a backlog. The drain is the ring's consumer, the one party that may
   * move the consumed cursor without racing the worklet. The journal shows no gap: a skipped `seq`
   * would mark the run live, so the count goes to `droppedFrames` instead.
   */
  private dropBacklog(): void {
    const buffered = this.ring.bufferedSamples();
    if (buffered <= MIC_BACKLOG_FRAMES) {
      return;
    }
    this.droppedFrames += BigInt(this.ring.skip(buffered - MIC_KEEP_FRAMES));
  }

  private journal(): number {
    const frames = this.partialFill;
    const payload = new Int16Array(frames * this.channels);
    for (let frame = 0; frame < frames; frame += 1) {
      const sample = this.partial[frame] ?? 0;
      for (let slot = 0; slot < this.channels; slot += 1) {
        payload[frame * this.channels + slot] = sample;
      }
    }
    this.stamper.micChunk(this.seq, payload);
    this.seq += 1n;
    this.chunks += 1;
    this.samples += BigInt(frames);
    this.partialFill = 0;
    return frames;
  }
}
