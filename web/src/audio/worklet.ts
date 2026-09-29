// The AudioWorklet processors `pemu-playback` and `pemu-capture`, on the audio thread. Neither
// allocates per quantum on the shared-ring path nor calls `Atomics.wait`, and each runs over either
// a SharedArrayBuffer or a transferred MessagePort. The context stays at the hardware rate; the
// guest format arrives from the `audio_out` headers and becomes the resampler's input rate at the
// exact sample where the guest changed it.

import { OVERFLOW_FILL_MS, RENDER_QUANTUM, TARGET_FILL_MS, UNDERRUN_FILL_MS } from "./levels";
import { Downsampler, DriftController, HermiteResampler, floatToInt16 } from "./resample";
import { TRACE_LEVEL_QUANTA, type TraceSink, type TraceWorklet } from "./trace";
import {
  PortSource,
  PortTransport,
  SharedRingTransport,
  TRANSPORT_HEADER_BYTES,
  portLike,
  type PcmSink,
  type PcmSource,
} from "./transport";

export { OVERFLOW_FILL_MS, RENDER_QUANTUM, TARGET_FILL_MS, UNDERRUN_FILL_MS };

/**
 * How long production must have stopped before a tail below the underrun floor plays: slices are at
 * most 8 ms and timers may be 16 ms late, so 32 ms without a sample means the burst ended.
 */
export const TAIL_IDLE_MS = 32;

/** Guest rate assumed until a record says otherwise: the official demo's 16 kHz, used only briefly. */
export const DEFAULT_GUEST_RATE = 16_000;

export const REPORT_EVERY_QUANTA = 64;

export const PLAYBACK_PROCESSOR = "pemu-playback";
export const CAPTURE_PROCESSOR = "pemu-capture";

/** The widest guest frame the engines accept: the I2S TX layouts are mono or stereo. */
const MAX_CHANNELS = 2;

/**
 * What a processor is told when it starts. With `sab` it maps the shared ring; otherwise the PCM
 * travels on the MessagePort transferred with this message. The port is transferred either way,
 * for format changes and counters.
 */
export interface WorkletOptions {
  readonly sab?: SharedArrayBuffer;
  /** Samples the fallback may keep in flight; ignored on the `sab` path. */
  readonly capacity?: number;
  readonly guestRate?: number;
  readonly trace?: boolean;
}

/**
 * A guest format change, as the worker posts it: `{ guestRate, channels?, at? }`. `at` is the
 * transport cursor where it starts, as a decimal string since a `bigint` does not survive every
 * structured clone path. Without `at` it applies at once, which is what capture gets.
 */
export interface FormatChange {
  readonly guestRate: number;
  readonly channels: number;
  readonly at: bigint | null;
}

export function validGuestRate(rate: unknown): rate is number {
  return typeof rate === "number" && Number.isInteger(rate) && rate > 0 && rate <= 384_000;
}

export function formatChangeOf(data: unknown): FormatChange | null {
  const message = data as { guestRate?: unknown; channels?: unknown; at?: unknown } | null;
  const rate = message?.guestRate;
  if (!validGuestRate(rate)) {
    return null;
  }
  const channels =
    typeof message?.channels === "number" && message.channels >= 1 && message.channels <= MAX_CHANNELS
      ? message.channels
      : 1;
  let at: bigint | null = null;
  if (typeof message?.at === "string" && /^\d+$/.test(message.at)) {
    at = BigInt(message.at);
  }
  return { guestRate: rate, channels, at };
}

export interface PlaybackCounters {
  readonly quanta: number;
  /** Times playback went from sound to silence because the ring ran below the floor. */
  readonly underruns: number;
  /** Quanta that were silent, entirely or in part, for want of samples. */
  readonly starvedQuanta: number;
  readonly overflows: number;
  readonly overflowSamples: number;
  /** Samples skipped because a run ended part-way through a frame before a format change. */
  readonly straySamples: number;
}

export interface CaptureCounters {
  readonly written: number;
  /** Guest-rate samples the ring refused because the worker had not drained it. */
  readonly dropped: number;
  /** Mic blocks discarded because the guest rate was not known yet. */
  readonly unratedBlocks: number;
}

/**
 * The playback side, a plain class so it tests off the audio thread. `render` pulls exactly the
 * samples the resampler interpolates into this quantum (so the consumed counter is what played),
 * takes the left slot of a stereo frame (the DAC uses the left slot), applies format changes at
 * their sample, and outputs silence rather than waiting when the ring runs dry.
 */
export class PlaybackEngine {
  private readonly resampler = new HermiteResampler();
  private drift: DriftController;
  /** Interleaved frames as pulled; sized for the widest ratio and frame, allocated once. */
  private readonly pulled = new Int16Array(RENDER_QUANTUM * 8 * MAX_CHANNELS);
  private readonly mono = new Int16Array(RENDER_QUANTUM * 8);
  private guestRate: number;
  private channels = 1;
  /** Format changes not reached yet, oldest first. Filled by the message handler only. */
  private readonly pending: FormatChange[] = [];
  private playing = false;
  private audioDriven = false;
  /** Transport cursor one past the last sample whose format mark this quantum has read. */
  private visibleEnd = 0n;
  private idleQuanta = 0;
  private readonly trace: TraceSink;
  private sawSamples = false;
  private inTail = false;
  private lowestFill = Number.MAX_SAFE_INTEGER;
  private counts = {
    quanta: 0,
    underruns: 0,
    starvedQuanta: 0,
    overflows: 0,
    overflowSamples: 0,
    straySamples: 0,
  };

  constructor(
    private readonly ring: PcmSource,
    private readonly contextRate: number,
    options: { guestRate: number; channels?: number; trace?: TraceSink },
  ) {
    this.trace = options.trace ?? null;
    this.guestRate = options.guestRate;
    this.channels = Math.min(MAX_CHANNELS, Math.max(1, options.channels ?? 1));
    this.drift = this.controller();
  }

  counters(): PlaybackCounters {
    return { ...this.counts };
  }

  get format(): { guestRate: number; channels: number } {
    return { guestRate: this.guestRate, channels: this.channels };
  }

  /** Schedules a format change at `at`, or before the next sample without one or once `at` has passed. */
  setFormat(change: FormatChange): void {
    if (change.at === null) {
      this.pending.length = 0;
      this.apply(change);
      return;
    }
    this.pending.push(change);
  }

  /**
   * Whether `Audio` pacing follows this worklet. While it does, the emulator runs on the device clock,
   * so drift correction would only fight the pacing loop: the ratio is held exact.
   */
  setAudioDriven(driven: boolean): void {
    if (driven !== this.audioDriven) {
      this.audioDriven = driven;
      this.drift = this.controller();
    }
  }

  get driftPpm(): number {
    return this.audioDriven ? 0 : this.drift.correctionPpm;
  }

  /** Fills one render quantum. Returns false when any of it was silence because the ring ran dry. */
  render(output: Float32Array): boolean {
    this.counts.quanta += 1;
    // Produced cursor first, marks second: every sample below the snapshot has its mark already.
    const visibleEnd = this.ring.consumedSamples() + BigInt(this.ring.bufferedSamples());
    this.idleQuanta = visibleEnd === this.visibleEnd ? this.idleQuanta + 1 : 0;
    this.visibleEnd = visibleEnd;
    this.takeMarks();
    this.applyDue();
    this.dropOverflow();
    const fill = this.readable();
    const floor = this.samplesFor(UNDERRUN_FILL_MS);
    // Below the floor the ring is waiting for the next slice, unless production stopped: then the last
    // few milliseconds of a sound are played rather than stranded.
    const tail = this.idleQuanta * output.length * 1000 >= TAIL_IDLE_MS * this.contextRate;
    // A stream starts, and restarts after a dropout, only once the drift target is buffered. Started at
    // the floor, a 24 ms production gap counted an underrun; from the target the fill before a push stays
    // at or above 45 ms, which outlasts a slice 8 ms late plus a 16 ms timer clamp.
    const start = this.playing ? floor : this.samplesFor(TARGET_FILL_MS);
    if (this.trace) {
      if (!this.sawSamples && fill > 0) {
        this.sawSamples = true;
        this.note("first-samples", fill);
      }
      this.lowestFill = Math.min(this.lowestFill, fill);
      if (this.sawSamples && this.counts.quanta % TRACE_LEVEL_QUANTA === 0) {
        this.note("level", this.lowestFill);
        this.lowestFill = Number.MAX_SAFE_INTEGER;
      }
    }
    if ((fill < start && !tail) || fill < this.channels) {
      output.fill(0);
      this.starve(fill);
      return false;
    }
    if (this.trace) {
      const inTail = fill < floor;
      if (inTail && !this.inTail) {
        this.note("tail", fill);
      }
      this.inTail = inTail;
    }
    const driftPpm = this.audioDriven ? 0 : this.drift.update(fill);
    let written = 0;
    while (written < output.length) {
      this.applyDue();
      const wanted = this.resampler.inputsNeeded(
        output.length - written,
        this.guestRate,
        this.contextRate,
        driftPpm,
      );
      const frames = Math.min(wanted, this.mono.length, this.framesBeforeBoundary());
      const got = this.pullFrames(frames);
      if (got === 0) {
        break;
      }
      const { produced } = this.resampler.process(
        this.mono.subarray(0, got),
        output.subarray(written),
        this.guestRate,
        this.contextRate,
        driftPpm,
      );
      written += produced;
      if (got < frames) {
        break;
      }
    }
    if (written < output.length) {
      output.fill(0, written);
      this.starve(fill);
      return false;
    }
    // A tail is what is left after the dropout was counted; its end is not a second one.
    if (fill >= floor) {
      if (this.trace && !this.playing) {
        this.note("playing", fill);
      }
      this.playing = true;
    }
    return true;
  }

  private note(kind: TraceWorklet["kind"], fill: number): void {
    this.trace?.({
      src: "worklet",
      kind,
      quantum: this.counts.quanta,
      consumed: this.ring.consumedSamples().toString(),
      fill,
      idleQuanta: this.idleQuanta,
      underruns: this.counts.underruns,
    });
  }

  private readable(): number {
    return Math.max(0, Number(this.visibleEnd - this.ring.consumedSamples()));
  }

  private takeMarks(): void {
    for (let mark = this.ring.takeFormat(); mark !== null; mark = this.ring.takeFormat()) {
      if (validGuestRate(mark.guestRate) && mark.channels >= 1 && mark.channels <= MAX_CHANNELS) {
        this.pending.push(mark);
      }
    }
  }

  private starve(fill: number): void {
    this.counts.starvedQuanta += 1;
    if (this.playing) {
      this.counts.underruns += 1;
      this.note("underrun", fill);
    }
    this.playing = false;
  }

  private pullFrames(frames: number): number {
    const width = this.channels;
    const whole = Math.floor(this.readable() / width);
    const count = Math.max(0, Math.min(frames, whole));
    if (count === 0) {
      return 0;
    }
    if (width === 1) {
      return this.ring.pull(this.mono.subarray(0, count));
    }
    const got = Math.floor(this.ring.pull(this.pulled.subarray(0, count * width)) / width);
    for (let frame = 0; frame < got; frame += 1) {
      this.mono[frame] = this.pulled[frame * width] ?? 0;
    }
    return got;
  }

  /**
   * Whole frames readable before the next scheduled format change. Never 0 while one is pending:
   * {@link PlaybackEngine.applyDue} applies a change once less than a frame of the old format is left.
   */
  private framesBeforeBoundary(): number {
    const next = this.pending[0];
    if (!next || next.at === null) {
      return Number.MAX_SAFE_INTEGER;
    }
    const left = next.at - this.ring.consumedSamples();
    return Math.max(1, Math.floor(Number(left) / this.channels));
  }

  /**
   * Applies every change whose boundary is reached, meaning less than one whole old-format frame lies
   * before it. Those stray samples cannot play as a frame, so they are skipped; if they have not
   * arrived yet the change waits.
   */
  private applyDue(): void {
    while (this.pending.length > 0) {
      const next = this.pending[0];
      if (!next) {
        break;
      }
      if (next.at !== null) {
        const left = next.at - this.ring.consumedSamples();
        if (left >= BigInt(this.channels)) {
          break;
        }
        if (left > 0n) {
          const strays = Number(left);
          if (this.ring.skip(strays) < strays) {
            break;
          }
          this.counts.straySamples += strays;
        }
      }
      this.pending.shift();
      this.apply(next);
    }
  }

  /** Above 250 ms of fill, drop to the 60 ms target. */
  private dropOverflow(): void {
    const fill = this.readable();
    if (fill <= this.samplesFor(OVERFLOW_FILL_MS)) {
      return;
    }
    const excess = fill - this.samplesFor(TARGET_FILL_MS);
    const frames = Math.min(Math.floor(excess / this.channels), this.framesBeforeBoundary());
    const dropped = this.ring.skip(frames * this.channels);
    if (dropped > 0) {
      this.counts.overflows += 1;
      this.counts.overflowSamples += dropped;
      this.resampler.reset();
    }
  }

  private apply(change: FormatChange): void {
    const channels = Math.min(MAX_CHANNELS, Math.max(1, change.channels));
    if (change.guestRate === this.guestRate && channels === this.channels) {
      return;
    }
    this.guestRate = change.guestRate;
    this.channels = channels;
    this.resampler.reset();
    this.drift = this.controller();
  }

  private samplesFor(ms: number): number {
    return Math.round((ms * this.guestRate) / 1000) * this.channels;
  }

  private controller(): DriftController {
    return new DriftController(this.samplesFor(TARGET_FILL_MS));
  }
}

/**
 * The capture side: mic floats to `i16` at the guest rate, into either sink. The worker's drain
 * drops the oldest frames of a backlog, since only the SPSC consumer may move the consumed cursor;
 * this worklet refuses samples (counted in `dropped`) only when draining has stopped altogether.
 */
export class CaptureEngine {
  private readonly downsampler = new Downsampler(RENDER_QUANTUM * 4);
  private readonly asInt = new Int16Array(RENDER_QUANTUM * 4);
  private readonly block = new Int16Array(RENDER_QUANTUM * 4 + 2);
  private counts = { written: 0, dropped: 0, unratedBlocks: 0 };

  /**
   * @param guestRate the guest's capture rate, or `null` while unknown. The ABI publishes no RX rate,
   *   so the worker sends the first `audio_out` record's TX rate; until then nothing is written, rather
   *   than journaling chunks at a guessed rate.
   */
  constructor(
    private readonly ring: PcmSink,
    private readonly contextRate: number,
    private guestRate: number | null,
  ) {}

  setGuestRate(rate: number): void {
    if (validGuestRate(rate) && rate !== this.guestRate) {
      this.guestRate = rate;
      this.downsampler.reset();
    }
  }

  counters(): CaptureCounters {
    return { ...this.counts };
  }

  /** Converts one input block (the mic's first channel) and writes it; returns how many samples the ring took. */
  capture(input: Float32Array): number {
    if (this.guestRate === null) {
      this.counts.unratedBlocks += 1;
      return 0;
    }
    const taken = Math.min(this.asInt.length, input.length);
    for (let index = 0; index < taken; index += 1) {
      this.asInt[index] = floatToInt16(input[index] ?? 0);
    }
    const produced = this.downsampler.process(
      this.asInt.subarray(0, taken),
      this.block,
      this.contextRate,
      this.guestRate,
    );
    const accepted = this.ring.push(this.block.subarray(0, produced));
    this.counts.written += accepted;
    this.counts.dropped += produced - accepted;
    return accepted;
  }
}

export function ringCapacityOf(sab: SharedArrayBuffer): number {
  return Math.max(0, Math.floor((sab.byteLength - TRANSPORT_HEADER_BYTES) / 2));
}

export interface WorkletLink<End> {
  readonly end: End;
  /** Posts a counters report to the worker; a no-op when the page transferred no port. */
  report(message: unknown): void;
}

function reporter(port: MessagePort | undefined): (message: unknown) => void {
  return (message) => {
    port?.postMessage(message);
  };
}

/**
 * The playback consumer a start message asks for: the shared ring when isolated, else the port.
 * Format changes travel inside the transport as marks; the port carries control messages either way.
 */
export function playbackLinkFor(
  message: WorkletOptions,
  port: MessagePort | undefined,
  control: (data: unknown) => void,
): WorkletLink<PcmSource> | null {
  if (message.sab) {
    if (port) {
      portLike(port).onData(control);
    }
    return { end: new SharedRingTransport(message.sab, ringCapacityOf(message.sab)), report: reporter(port) };
  }
  if (port) {
    return { end: new PortSource(portLike(port), control), report: reporter(port) };
  }
  return null;
}

/** The capture producer a start message asks for; on the fallback its port end is a {@link PortTransport}. */
export function captureLinkFor(
  message: WorkletOptions,
  port: MessagePort | undefined,
  onFormat: (change: FormatChange) => void,
): WorkletLink<PcmSink> | null {
  const control = (data: unknown) => {
    const change = formatChangeOf(data);
    if (change !== null) {
      onFormat(change);
    }
  };
  if (message.sab) {
    if (port) {
      portLike(port).onData(control);
    }
    return { end: new SharedRingTransport(message.sab, ringCapacityOf(message.sab)), report: reporter(port) };
  }
  if (port) {
    const end = new PortTransport(portLike(port), message.capacity ?? RENDER_QUANTUM * 64, control);
    return { end, report: reporter(port) };
  }
  return null;
}

/**
 * Registers both processors when loaded by `AudioWorklet.addModule`; elsewhere the module only
 * provides the engines. Both speak both transports, since a page without COOP and COEP gets no SAB.
 */
export function registerProcessors(): void {
  const scope = globalThis as {
    registerProcessor?: (name: string, ctor: unknown) => void;
    AudioWorkletProcessor?: new () => { port: MessagePort };
    sampleRate?: number;
  };
  const Base = scope.AudioWorkletProcessor;
  if (!scope.registerProcessor || !Base) {
    return;
  }
  const contextRate = () => scope.sampleRate ?? 48_000;

  class PlaybackProcessor extends Base {
    private engine: PlaybackEngine | null = null;
    private link: WorkletLink<PcmSource> | null = null;
    private sinceReport = 0;

    constructor() {
      super();
      this.port.onmessage = (event: MessageEvent) => {
        const message = event.data as WorkletOptions;
        // Format changes are marks in the transport; the control channel only says whether `Audio` drives.
        const link = playbackLinkFor(message, event.ports[0], (data) => {
          const driven = (data as { audioDriven?: unknown } | null)?.audioDriven;
          if (typeof driven === "boolean") {
            this.engine?.setAudioDriven(driven);
          }
        });
        if (link) {
          this.link = link;
          this.engine = new PlaybackEngine(link.end, contextRate(), {
            guestRate: message.guestRate ?? DEFAULT_GUEST_RATE,
            trace: message.trace === true ? (entry) => link.report({ trace: entry }) : null,
          });
        }
      };
    }

    process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
      const block = outputs[0]?.[0];
      if (block) {
        if (this.engine) {
          this.engine.render(block);
          // Every other output channel repeats the first: the device has one speaker.
          for (const other of outputs[0]?.slice(1) ?? []) {
            other.set(block);
          }
        } else {
          block.fill(0);
        }
      }
      this.sinceReport += 1;
      if (this.engine && this.link && this.sinceReport >= REPORT_EVERY_QUANTA) {
        this.sinceReport = 0;
        this.link.report({ playback: this.engine.counters() });
      }
      return true;
    }
  }

  class CaptureProcessor extends Base {
    private engine: CaptureEngine | null = null;
    private link: WorkletLink<PcmSink> | null = null;
    private sinceReport = 0;

    constructor() {
      super();
      this.port.onmessage = (event: MessageEvent) => {
        const message = event.data as WorkletOptions;
        const link = captureLinkFor(message, event.ports[0], (change) => {
          this.engine?.setGuestRate(change.guestRate);
        });
        if (link) {
          this.link = link;
          this.engine = new CaptureEngine(
            link.end,
            contextRate(),
            validGuestRate(message.guestRate) ? message.guestRate : null,
          );
        }
      };
    }

    process(inputs: Float32Array[][]): boolean {
      const block = inputs[0]?.[0];
      if (block && this.engine) {
        this.engine.capture(block);
      }
      this.sinceReport += 1;
      if (this.engine && this.link && this.sinceReport >= REPORT_EVERY_QUANTA) {
        this.sinceReport = 0;
        this.link.report({ capture: this.engine.counters() });
      }
      return true;
    }
  }

  scope.registerProcessor(PLAYBACK_PROCESSOR, PlaybackProcessor);
  scope.registerProcessor(CAPTURE_PROCESSOR, CaptureProcessor);
}

registerProcessors();
