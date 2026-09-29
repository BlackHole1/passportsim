// The emulator session: everything the Worker does between slices. It owns the core, views, pacing
// loop, renderer, audio pump and input journal, with no access to `self`, so `bun test` drives it
// as the Worker does. The ROM is compiled into the core, so a `MachineConfig` is all it needs.

import type { CaptureSource, MicCaptureOptions } from "../audio/capture";
import { MicCapture } from "../audio/capture";
import { PcmPump } from "../audio/playback";
import type { PcmTransport } from "../audio/transport";
import { DirtySpan } from "../gl/dirty";
import type { PanelSink } from "../gl/sink";
import {
  InputStamper,
  encodeBatch,
  journalFromCore,
  type JournalExport,
  type StampedInput,
} from "./input";
import { ABI_VERSION, AT_NOW, ButtonId, FRAME_HEIGHT, InputKind, RingId, StopCode } from "./layout";
import type { EmulatorCore } from "./core";
import type { LoopRecorder } from "./loopTrace";
import { PacingLoop, type PacingMode, type PacingStats, type StepOutcome } from "./pacing";
import { readRelayWindow, relayRequest, type RelayWindow } from "./relay";
import { IoViews, type HostEvent, type LineMark } from "./ring";

export const MAX_SERIAL_PER_SLICE = 16 * 1024;

export const MAX_LINE_MARKS_PER_SLICE = 1024;

export const MAX_EVENTS_PER_SLICE = 256;

/** The console streams, each with its byte ring and its line-mark ring. */
export const SERIAL_STREAMS: readonly { readonly bytes: RingId; readonly lines: RingId }[] = [
  { bytes: RingId.UsjTx, lines: RingId.LinesUsjTx },
  { bytes: RingId.Uart0Tx, lines: RingId.LinesUart0Tx },
];

/**
 * The shortest interval between two panel uploads: the core may dirty rows far more often than a
 * display shows them, and 16 ms is the finest interval any engine's timer can name.
 */
export const MIN_PRESENT_INTERVAL_MS = 16;

export interface SerialSlice {
  readonly stream: RingId;
  readonly bytes: Uint8Array;
  /**
   * Bytes the ring evicted before the session read them; the console draws a gap for them, so a
   * burst that outran {@link MAX_SERIAL_PER_SLICE} never looks seamless.
   */
  readonly dropped: bigint;
  readonly lines: readonly LineMark[];
  readonly linesDropped: bigint;
}

export interface SessionReport {
  readonly serial: readonly SerialSlice[];
  readonly events: readonly HostEvent[];
  readonly presented: boolean;
  /** The core's `FramePort` generation after the slice: frames the guest presented so far. */
  readonly frameGeneration: bigint;
  readonly panel: PanelState;
  readonly pacing: PacingStats;
}

/** The panel metadata of one `FramePort` publish, without the pixels. */
export interface PanelState {
  /** Backlight numerator, `duty >> 4` of the resolved brightness. */
  readonly backlight: number;
  /** Backlight denominator, `1 << duty_res`; 0 while the duty resolution is not modelled. */
  readonly backlightScale: number;
  readonly powered: boolean;
  readonly sleeping: boolean;
  readonly displayOn: boolean;
  /** INVON, the command state; never a drawing rule. */
  readonly inverted: boolean;
  /** The glass shows the complement of memory: `inverted != invon_shows_ram`. */
  readonly glassComplement: boolean;
}

export interface SessionParts {
  readonly core: EmulatorCore;
  readonly renderer?: PanelSink | null;
  readonly canvas?: { width: number; height: number };
  readonly nowMs: () => number;
}

export class EmulatorSession {
  readonly views: IoViews;
  readonly input = new InputStamper();
  readonly pacing: PacingLoop;
  private serialCursors = SERIAL_STREAMS.map(() => 0n);
  private lineCursors = SERIAL_STREAMS.map(() => 0n);
  private eventCursor = 0n;
  private lastPresentMs = Number.NEGATIVE_INFINITY;
  /**
   * The panel state of the last draw, so a change without a pixel write (backlight, DISPON, sleep,
   * glass complement) is still drawn; `-1` before the first draw.
   */
  private drawnBacklight = -1;
  private drawnBacklightScale = -1;
  private drawnPanelBits = -1;
  /** The union of the dirty spans since the last upload; see {@link present}. */
  private readonly pending = new DirtySpan(FRAME_HEIGHT);
  private stopJson: string | null = null;
  private audioPump: PcmPump | null = null;
  private loop: LoopRecorder | null = null;
  private mic: MicCapture | null = null;
  private captureRate: number | null = null;
  /** `seq` the next live chunk carries, kept across detach and re-attach. */
  private nextMicSeq = 0n;
  private liveBridgeUp = false;

  constructor(
    private readonly parts: SessionParts,
    clock: { nowMs: () => number },
    yielder: { sleep: (ms: number) => Promise<void> },
  ) {
    this.views = IoViews.of(parts.core.memory, () => parts.core.ioLayoutPtr());
    // A renderer can outlive a session, so a new machine's first present uploads every row; otherwise
    // the previous frame stays on the glass until the guest repaints each row.
    this.pending.addAll();
    this.pacing = new PacingLoop(
      {
        nowPs: () => parts.core.nowPs(),
        run: (untilPs, maxInsns) => parts.core.run(untilPs, maxInsns),
      },
      clock,
      yielder,
      {
        consumedPs: () => this.audioPump?.consumedPs() ?? null,
        flowing: () => this.audioPump?.flowing() ?? false,
      },
    );
  }

  get abiVersion(): number {
    return ABI_VERSION;
  }

  get lastStopJson(): string | null {
    return this.stopJson;
  }

  /**
   * Starts pumping PCM into `transport`, and makes `Audio` pacing follow what the worklet consumed.
   * Separate from the constructor because the pump reads this session's own views.
   */
  useAudio(transport: PcmTransport): PcmPump {
    this.audioPump = new PcmPump(this.views, transport);
    return this.audioPump;
  }

  get audio(): PcmPump | null {
    return this.audioPump;
  }

  get microphone(): MicCapture | null {
    return this.mic;
  }

  /** Whether a live microphone is attached: a live bridge, which pins pacing to rate 1. */
  get liveMicrophone(): boolean {
    return this.mic !== null;
  }

  /**
   * The carrier reports whether the Wi-Fi bridge is live. A live bridge holds the clock lease: the
   * guest talks to a host peer in wall time, so pacing moves to `Wall { rate: 1 }` while it is up,
   * as for a live microphone and as `relay_wisp::tick` does natively. Returns the pacing in force.
   */
  noteLiveBridge(live: boolean): PacingMode {
    this.liveBridgeUp = live;
    if (live && !liveBridgeMode(this.pacing.currentMode) && this.pacing.currentMode.kind !== "Paused") {
      this.pacing.setMode({ kind: "Wall", rate: 1 });
    }
    return this.pacing.currentMode;
  }

  get liveBridge(): boolean {
    return this.liveBridgeUp;
  }

  /**
   * Attaches the live microphone: the capture ring is drained into the journal before every slice,
   * in the 240-frame `MicChunk`s `mic_set` journals, so a replay hears the same audio at the same
   * virtual times. It is a live bridge, so pacing moves to `Wall { rate: 1 }` unless it is already
   * that or `Audio`. The first `seq` is `options.firstSeq`, else the larger of the journal's next
   * microphone chunk and this session's count; a gap or reuse would make the journal call the run live.
   */
  startMicrophone(source: CaptureSource, options: MicCaptureOptions = {}): PacingMode {
    if (!this.mic) {
      source.skip(source.bufferedSamples());
      const journalNext = this.coreMicNextSeq();
      const own = this.nextMicSeq;
      this.mic = new MicCapture(source, this.input, {
        ...options,
        firstSeq:
          options.firstSeq ?? (journalNext !== null && journalNext > own ? journalNext : own),
      });
    }
    if (!liveBridgeMode(this.pacing.currentMode)) {
      this.pacing.setMode({ kind: "Wall", rate: 1 });
    }
    return this.pacing.currentMode;
  }

  /** The `seq` the journal expects next on the microphone stream, or `null` when the core does not say. */
  private coreMicNextSeq(): bigint | null {
    return this.coreLiveNextSeq("mic_next_seq");
  }

  /** One field of the `@live` answer (`Machine::next_live_chunk`). */
  private coreLiveNextSeq(field: "mic_next_seq" | "net_next_seq"): bigint | null {
    try {
      const answer = JSON.parse(this.parts.core.call('{"cmd":"@live"}')) as Record<string, unknown>;
      const value = answer[field];
      return typeof value === "string" ? BigInt(value) : null;
    } catch {
      return null;
    }
  }

  /**
   * Detaches the live microphone and journals its end of stream: the short last chunk, then a
   * `mic_set` to `silence` through the registry. The core's journal has no end-of-stream record, so
   * the source change is the end.
   */
  endMicrophone(): void {
    const mic = this.mic;
    if (!mic) {
      return;
    }
    mic.flush();
    this.nextMicSeq = mic.stats().nextSeq;
    this.mic = null;
    this.parts.core.call(JSON.stringify({ cmd: "mic_set", args: { kind: "silence" } }));
  }

  /**
   * Takes the guest rate the capture worklet should resample to; returns it when it moved, else `null`.
   * The RX rate is not published through the ABI, and the codec opens one rate for both directions
   * (`bsp_audio_set_format`), so the newest `audio_out` record's TX rate stands in for it (unverified
   * for a firmware that runs them at different rates). Until the first record nothing is captured, so
   * no chunk is journaled at a guessed rate; at a change the open chunk is closed first.
   */
  noteCaptureRate(rate: number | null): number | null {
    if (rate === null || rate === this.captureRate) {
      return null;
    }
    if (this.captureRate !== null) {
      this.mic?.flush();
    }
    this.captureRate = rate;
    return rate;
  }

  /**
   * Changes pacing, or refuses with an `E_LEASE`-shaped answer while a live bridge holds the clock:
   * `Max` and `Wall` rates other than 1 are refused, and so is `Paused` unless `detach` ends the live
   * microphone first. The Wi-Fi bridge is not detached by `detach`: the agent ends it with
   * `net_http --op unbridge`, as the native refusal says.
   */
  setMode(mode: PacingMode, options: { readonly detach?: boolean } = {}): LeaseRefusal | null {
    if (this.liveBridgeUp && !liveBridgeMode(mode)) {
      return leaseRefusal(
        "wifi-bridge",
        mode.kind === "Paused"
          ? "pausing; the bridge ends with `net_http --op unbridge`"
          : `${mode.kind} pacing at this rate`,
      );
    }
    if (this.mic) {
      if (mode.kind === "Paused") {
        if (!options.detach) {
          return leaseRefusal(
            "live-microphone",
            "pausing needs `detach`, which ends the live microphone",
          );
        }
        this.endMicrophone();
      } else if (!liveBridgeMode(mode)) {
        return leaseRefusal("live-microphone", `${mode.kind} pacing at this rate`);
      }
    }
    this.pacing.setMode(mode);
    return null;
  }

  /**
   * Frees the machine (`pemu_drop`); the session is dead afterwards. A wasm instance never returns
   * linear memory, so a reboot that skipped this would leak a machine each time.
   */
  close(): void {
    this.parts.core.drop();
  }

  /** Queues a button edge; it is stamped at the next slice boundary. */
  button(id: ButtonId, down: boolean): void {
    this.input.button(id, down);
  }

  power(down: boolean): void {
    this.input.power(down);
  }

  serialIn(channel: number, bytes: Uint8Array): void {
    this.input.serial(channel, bytes);
  }

  /** Runs a registry command through the ABI's JSON path. */
  call(request: string): string {
    return this.parts.core.call(request);
  }

  /**
   * The Wi-Fi bridge's outbound window at or after `cursor` (`null` on a carrier's first look), for
   * `relay.ts`. Reading takes nothing out of the machine: the cursor is the carrier's.
   */
  relayWindow(cursor: string | null): RelayWindow {
    return readRelayWindow(this.parts.core.call(relayRequest(cursor)));
  }

  /**
   * Journals server packets the carrier brought back as `InputEvent::NetFrame` and returns how many.
   * `seq` continues the journal's network stream, so no gap is noted; the core marks each frame
   * `Origin::Bridge`, making the run `live`. Stamped `AT_NOW`, as the native carrier does.
   */
  bridgeFrames(packets: readonly Uint8Array[]): number {
    if (packets.length === 0) {
      return 0;
    }
    let seq = this.coreLiveNextSeq("net_next_seq") ?? 0n;
    const stamped: StampedInput[] = packets.map((payload) => {
      const at: StampedInput = {
        atPs: AT_NOW,
        kind: InputKind.NetFrame,
        a: Number(seq & 0xffff_ffffn),
        b: Number((seq >> 32n) & 0xffff_ffffn),
        c: 0,
        payload,
      };
      seq += 1n;
      return at;
    });
    this.parts.core.input(encodeBatch(stamped));
    return stamped.length;
  }

  /**
   * The input journal, replayable at `Max`: the machine's own (`@journal`), so it also holds inputs
   * registry commands journaled. Inputs still queued are not in it yet. Live microphone chunks are
   * dropped unless `includeSecrets`. After a restore the core answers format 2, which
   * `journalFromCore` refuses.
   */
  exportJournal(options: { readonly includeSecrets?: boolean } = {}): JournalExport {
    const request = options.includeSecrets
      ? '{"cmd":"@journal","args":{"include_secrets":true}}'
      : '{"cmd":"@journal"}';
    return journalFromCore(this.parts.core.call(request), ABI_VERSION);
  }

  /** One iteration: stamp queued input, run one paced slice, then pump frame, serial, events and PCM. */
  async step(): Promise<{ outcome: StepOutcome; report: SessionReport }> {
    const loop = this.loop;
    if (!loop) {
      this.flushInput();
      const outcome = await this.pacing.step();
      if (outcome.kind === "stopped") {
        this.stopJson = this.parts.core.lastStop();
      }
      return { outcome, report: this.pump(outcome) };
    }
    loop.begin();
    let at = performance.now();
    this.flushInput();
    let now = performance.now();
    loop.add("flushMs", now - at);
    at = now;
    const outcome = await this.pacing.step();
    if (outcome.kind === "stopped") {
      this.stopJson = this.parts.core.lastStop();
    }
    now = performance.now();
    loop.stepped(now - at);
    at = now;
    const report = this.pump(outcome);
    loop.add("pumpMs", performance.now() - at);
    return { outcome, report };
  }

  traceLoop(recorder: LoopRecorder | null): void {
    this.loop = recorder;
  }

  async run(onReport: (report: SessionReport, outcome: StepOutcome) => void): Promise<void> {
    for (;;) {
      const { outcome, report } = await this.step();
      onReport(report, outcome);
      if (outcome.kind === "stopped") {
        return;
      }
      if (this.pacing.currentMode.kind === "Paused" && outcome.kind === "paused") {
        return;
      }
    }
  }

  /**
   * Stamps everything the UI queued with the core's current virtual time and journals it. The next
   * slice starts there, so the input applies at its first instant and a replay reproduces it.
   */
  private flushInput(): void {
    this.mic?.drain();
    if (!this.input.hasPending) {
      return;
    }
    const stamped = this.input.drain(this.parts.core.nowPs());
    if (stamped) {
      this.parts.core.input(encodeBatch(stamped));
    }
  }

  private pump(outcome: StepOutcome): SessionReport {
    this.views.sync();
    const serial = this.readSerial();
    const events = this.views.readEvents(this.eventCursor, MAX_EVENTS_PER_SLICE);
    this.eventCursor = events.next;
    this.audioPump?.pump(this.parts.core.nowPs());
    this.noteDirty();
    // A slice that ends in a stop is the frame the user looks at until they resume, so it is
    // presented at once, past the throttle.
    const presentAt = this.loop ? performance.now() : 0;
    const presented =
      outcome.kind === "ran" || outcome.kind === "stopped" || outcome.kind === "paused"
        ? this.present(outcome.kind !== "ran")
        : false;
    this.loop?.add("presentMs", performance.now() - presentAt);
    return {
      serial,
      events: events.items,
      presented,
      frameGeneration: this.views.frame.generation,
      panel: panelState(this.views.frame),
      pacing: this.pacing.stats(),
    };
  }

  /** Reads both console streams and line-mark rings. uart0 carries the ROM's own output. */
  private readSerial(): SerialSlice[] {
    const slices: SerialSlice[] = [];
    SERIAL_STREAMS.forEach((stream, index) => {
      const read = this.views.readBytes(
        stream.bytes,
        this.serialCursors[index] ?? 0n,
        MAX_SERIAL_PER_SLICE,
      );
      this.serialCursors[index] = read.next;
      const marks = this.views.readLineMarks(
        stream.lines,
        this.lineCursors[index] ?? 0n,
        MAX_LINE_MARKS_PER_SLICE,
      );
      this.lineCursors[index] = marks.next;
      if (read.items.length === 0 && marks.items.length === 0 && read.dropped === 0n) {
        return;
      }
      slices.push({
        stream: stream.bytes,
        bytes: read.items,
        dropped: read.dropped,
        lines: marks.items,
        linesDropped: marks.dropped,
      });
    });
    return slices;
  }

  /**
   * Adds the span the core published to the one waiting for upload. The published span is a delta
   * (`IoPublisher::refresh` takes it out of `FramePort`), so a slice the throttle skipped must keep
   * its rows, or they are never drawn.
   */
  private noteDirty(): void {
    const frame = this.views.frame;
    if (frame.dirtyFirst === null) {
      return;
    }
    this.pending.add(frame.dirtyFirst, frame.dirtyLast);
  }

  /**
   * Uploads dirty rows and draws, at most once per {@link MIN_PRESENT_INTERVAL_MS} unless `force`. A
   * panel state change alone draws without an upload, since backlight, DISPON, sleep and the glass
   * complement change the glass without touching memory. The frame generation is not a gate: a real
   * core repaints rows without moving it.
   */
  private present(force = false): boolean {
    const renderer = this.parts.renderer;
    if (!renderer) {
      return false;
    }
    const frame = this.views.frame;
    const bits =
      (frame.powered ? 1 : 0) |
      (frame.sleeping ? 2 : 0) |
      (frame.displayOn ? 4 : 0) |
      (frame.glassComplement ? 8 : 0);
    const stateChanged =
      bits !== this.drawnPanelBits ||
      frame.backlight !== this.drawnBacklight ||
      frame.backlightScale !== this.drawnBacklightScale;
    if (!this.pending.pending && !stateChanged) {
      return false;
    }
    const now = this.parts.nowMs();
    if (!force && now - this.lastPresentMs < MIN_PRESENT_INTERVAL_MS) {
      return false;
    }
    if (this.pending.pending) {
      renderer.upload(this.views.pixels, this.pending.firstRow, this.pending.lastRow);
      this.pending.clear();
    }
    // Not allocation-free: `IoViews.sync` builds a new `FrameInfo` every slice. The renderers cache
    // their own views.
    const canvas = this.parts.canvas;
    renderer.draw(frame, canvas?.width ?? frame.width, canvas?.height ?? frame.height);
    this.lastPresentMs = now;
    this.drawnPanelBits = bits;
    this.drawnBacklight = frame.backlight;
    this.drawnBacklightScale = frame.backlightScale;
    return true;
  }
}

function panelState(frame: PanelState): PanelState {
  return {
    backlight: frame.backlight,
    backlightScale: frame.backlightScale,
    powered: frame.powered,
    sleeping: frame.sleeping,
    displayOn: frame.displayOn,
    inverted: frame.inverted,
    glassComplement: frame.glassComplement,
  };
}

/** The live-bridge refusal, shaped like the registry's `E_LEASE` error. */
export interface LeaseRefusal {
  readonly code: "E_LEASE";
  readonly holder: LeaseHolder;
  readonly message: string;
}

export type LeaseHolder = "live-microphone" | "wifi-bridge";

const HOLDER_NAMES: Record<LeaseHolder, string> = {
  "live-microphone": "live microphone capture is attached",
  "wifi-bridge": "the Wi-Fi bridge is live",
};

function leaseRefusal(holder: LeaseHolder, what: string): LeaseRefusal {
  return {
    code: "E_LEASE",
    holder,
    message: `E_LEASE: ${HOLDER_NAMES[holder]} and pins pacing to Wall at rate 1; refused ${what}`,
  };
}

/** Pacing a live bridge allows: `Wall { rate: 1 }`, or `Audio` at rate 1. */
function liveBridgeMode(mode: PacingMode): boolean {
  return (mode.kind === "Wall" || mode.kind === "Audio") && mode.rate === 1;
}

export function endsSession(stop: number): boolean {
  return stop !== StopCode.Until && stop !== StopCode.MaxInsns;
}
