// The emulator Worker entry point: decodes the page's messages, drives an `EmulatorSession` and
// posts back what the UI needs. The decisions live in `session.ts`, `pacing.ts`, `ring.ts`,
// `renderer.ts` and `playback.ts`, which have no `self` and are tested directly.

import type { PcmPump } from "../audio/playback";
import {
  PortSource,
  PortTransport,
  SharedRingTransport,
  portLike,
  sabAvailable,
  type PcmTransport,
  type PortLike,
} from "../audio/transport";
import type { CaptureSource } from "../audio/capture";
import type { AudioTraceEntry } from "../audio/trace";
import type { CaptureCounters, PlaybackCounters } from "../audio/worklet";
import { openDisplay, type Display } from "../gl/display";
import type { DisplayState } from "../gl/sink";
import { AttachClient, type AttachEvent } from "./attach";
import { CoreError, WasmCore, type CoreAsset, type CoreExports, type EmulatorCore } from "./core";
import { ABI_VERSION, ButtonId, FRAME_HEIGHT, FRAME_WIDTH, LoadKind } from "./layout";
import {
  atomicsYielder,
  createInputCell,
  messageChannelYielder,
  pacingModeRefusal,
  performanceClock,
  spinWait,
  turnRunsTasks,
  waitAsyncTurn,
  type StepOutcome,
  type PacingMode,
  type Yielder,
  type YielderKind,
} from "./pacing";
import { LoopRecorder } from "./loopTrace";
import { RelayClient } from "./relay";
import { EmulatorSession, type PanelState, type SessionReport } from "./session";

/**
 * Samples the playback ring holds: 500 ms of 48 kHz stereo. It must exceed the worklet's 250 ms
 * overflow limit at the widest guest format, or that limit could never trigger.
 */
export const PLAYBACK_RING_SAMPLES = 48_000;

/**
 * Samples the capture ring holds: 500 ms at 24 kHz, well above the drain's `MIC_BACKLOG_FRAMES`,
 * so the drain trims a backlog before the worklet ever refuses new samples.
 */
export const CAPTURE_RING_SAMPLES = 12_000;

/**
 * Host milliseconds between two `stats` posts from iterations that ran no slice: one display
 * frame. Slices, stops and the final pause are never throttled.
 */
export const WAIT_STATS_PERIOD_MS = 16;

/**
 * How often, in host milliseconds, the Wi-Fi bridge carrier looks for a bridge while none is live
 * (each look costs a `pemu_call`). A live bridge is pumped every slice.
 */
export const RELAY_IDLE_POLL_MS = 250;

export const LOOP_SUMMARY_MS = 1_000;

export type ToWorker =
  | {
      readonly type: "boot";
      readonly config: string;
      /**
       * The firmware to boot: a merged flash image or a `.pebundle`. Absent, the config's `fw` names a
       * bundle shipped beside the Worker, which is how the demo boots with no input.
       */
      readonly image?: Uint8Array;
      /**
       * Further `pemu_load` assets of the same load, after `image` and in this order. An ELF dropped
       * alone arrives here with no `image`.
       */
      readonly assets?: readonly CoreAsset[];
      /**
       * The page's name for this boot, echoed in its `ready` and errors, so a page that boots a second
       * image while the first is still coming up can tell which boot answered.
       */
      readonly token?: string;
      readonly canvas?: OffscreenCanvas;
      /** Ignored: the Worker draws at the transferred canvas's own size. Kept so old pages type-check. */
      readonly canvasSize?: { readonly width: number; readonly height: number };
      /**
       * The Worker's end of the channel to the playback worklet. It carries PCM when the page is not
       * cross-origin isolated, and the `{ guestRate }` the worklet needs either way.
       */
      readonly audioPort?: MessagePort;
      readonly capturePort?: MessagePort;
      /** Post an `audioTrace` for every record header, burst and worklet state change; stays on once asked. */
      readonly audioTrace?: boolean;
      /** Diagnostic: the isolated loop spins where it would `Atomics.wait`, burning a core. */
      readonly spinWait?: boolean;
      /** Diagnostic: the isolated loop takes its turn through a MessageChannel, whatever `waitAsync` does. */
      readonly channelTurn?: boolean;
    }
  /** `detach` lets a pause end the live microphone first; without it such a pause is refused with `E_LEASE`. */
  | { readonly type: "mode"; readonly mode: PacingMode; readonly detach?: boolean }
  /** The live microphone connected: capture is journaled and pacing is pinned to `Wall { rate: 1 }`. */
  | { readonly type: "micStart" }
  | { readonly type: "micEnd" }
  | { readonly type: "button"; readonly id: ButtonId; readonly down: boolean }
  | { readonly type: "power"; readonly down: boolean }
  | { readonly type: "serial"; readonly channel: number; readonly text: string }
  | { readonly type: "call"; readonly id: number; readonly request: string }
  /** The journal export; live microphone chunks are dropped unless `includeSecrets` asks for them. */
  | { readonly type: "journal"; readonly includeSecrets?: boolean }
  /** A copy of the `raw` frame as the guest wrote it. */
  | { readonly type: "frame" }
  /**
   * Attach this machine to the daemon on `port` as a `b<n>` instance. The page's session cookie
   * authorizes the socket at the upgrade, so no token travels here.
   */
  | { readonly type: "attach"; readonly port: number; readonly label?: string }
  | { readonly type: "detach" };

export type FromWorker =
  | {
      readonly type: "ready";
      readonly abiVersion: number;
      readonly token?: string;
      /** The playback ring, when the page is cross-origin isolated. */
      readonly audioSab?: SharedArrayBuffer;
      readonly captureSab?: SharedArrayBuffer;
      /** The shared input cell, on the same condition: the page's `notifyInput` on it wakes `Atomics.wait`. */
      readonly inputSab?: SharedArrayBuffer;
    }
  | {
      readonly type: "serial";
      /** The `RingId` of the byte ring, so the UI keeps usj and uart0 apart. */
      readonly stream: number;
      readonly bytes: Uint8Array;
      readonly dropped: string;
      readonly lines: readonly { readonly offset: string; readonly vtPs: string }[];
      readonly linesDropped: string;
    }
  | { readonly type: "events"; readonly events: readonly { kind: number; vtPs: string; arg: string }[] }
  | {
      readonly type: "stats";
      readonly nowPs: string;
      /** Virtual over wall time including the loop's sleeps: about the rate at `Wall`. */
      readonly realTimeFactor: number;
      /** Virtual over wall time inside `pemu_run` only: how far the core could outrun the rate. */
      readonly headroom: number;
      readonly reanchors: number;
      /** The core's `FramePort` generation: frames the guest has presented. */
      readonly frameGeneration: string;
      readonly panel: PanelState;
      readonly draws: number;
      /** The clock the last slice was paced on: `audio` while the worklet drives. */
      readonly clock?: string;
      readonly yielder?: YielderKind;
      /** How the isolated loop lets its event loop run after a wait; absent when not isolated. */
      readonly turn?: "wait-async" | "message-channel";
      readonly audio?: AudioStatsMessage;
    }
  | { readonly type: "stopped"; readonly stop: number; readonly json: string | null }
  | { readonly type: "call"; readonly id: number; readonly ok?: string; readonly err?: string }
  /** The exported journal as JSON (`"null"` with no machine), or `err`, a refusal's `ApiError` body. */
  | { readonly type: "journal"; readonly json: string; readonly err?: string }
  /** The `raw` frame: panel memory in RGB565, row-major, with its `FramePort` generation. */
  | {
      readonly type: "frame";
      readonly width: number;
      readonly height: number;
      readonly generation: string;
      readonly pixels: Uint16Array | null;
    }
  /** Which renderer draws the panel and why, posted at boot and on every context loss and restore. */
  | ({ readonly type: "display" } & DisplayState)
  | { readonly type: "audioTrace"; readonly entry: AudioTraceEntry }
  | { readonly type: "attach"; readonly event: AttachEvent }
  /**
   * The pacing loop threw: the machine runs no further until the page boots one again. Unlike
   * `error`, which is a refusal the machine outlives.
   */
  | {
      readonly type: "fatal";
      readonly message: string;
      readonly code?: string;
    }
  | {
      readonly type: "error";
      readonly message: string;
      readonly code?: string;
      readonly token?: string;
    };

/** The audio counters the page shows, as plain JSON (bigints as decimal strings). */
export interface AudioStatsMessage {
  readonly pushed: string;
  readonly dropped: string;
  readonly lost: string;
  readonly buffered: number;
  readonly guestRate: number | null;
  readonly peak: number;
  readonly playback: PlaybackCounters | null;
  readonly capture: CaptureCounters | null;
  readonly micChunks: number;
}

/**
 * An `ApiError` JSON body with every field `pemu_api::error::ApiError` serializes, for a refusal
 * the Worker makes itself, so the page decodes one shape. `number` is the `ErrorCode` number.
 */
export function apiErrorJson(code: string, number: number, message: string, vtUs: number, hint?: string): string {
  return JSON.stringify({
    backtrace: [],
    code,
    detail: null,
    ...(hint === undefined ? {} : { hint }),
    message,
    number,
    retryable: false,
    serial_tail: [],
    vt_us: vtUs,
  });
}

export interface WorkerScope {
  postMessage(message: FromWorker, transfer?: Transferable[]): void;
  onmessage: ((event: { data: ToWorker }) => void) | null;
}

export type CoreLoader = (
  config: string,
  image?: Uint8Array,
  assets?: readonly CoreAsset[],
) => EmulatorCore | Promise<EmulatorCore>;

export type DisplayOpener = (
  canvas: OffscreenCanvas,
  width: number,
  height: number,
  onState: (state: DisplayState) => void,
) => Display;

export interface WorkerOptions {
  readonly openDisplay?: DisplayOpener;
  /**
   * Whether the `Atomics.waitAsync` turn lets this Worker's tasks run; `false` makes the isolated
   * loop take the MessageChannel turn. Asked once, at the first isolated boot; absent, probed.
   */
  readonly waitAsyncTurnRunsTasks?: () => Promise<boolean>;
}

function apiErrorCode(body: string): string | null {
  try {
    const code = (JSON.parse(body) as { code?: unknown }).code;
    return typeof code === "string" ? code : null;
  } catch {
    return null;
  }
}

function probeWaitAsyncTurn(): Promise<boolean> {
  const turn = waitAsyncTurn();
  return turn ? turnRunsTasks(turn) : Promise.resolve(false);
}

/** Wires a scope to a session; returns a handle so a test can drive the browser's code. */
export function startWorker(
  scope: WorkerScope,
  loadCore: CoreLoader,
  options: WorkerOptions = {},
): void {
  const open: DisplayOpener =
    options.openDisplay ?? ((canvas, width, height, onState) => openDisplay(canvas, width, height, onState));
  const probeTurn = options.waitAsyncTurnRunsTasks ?? probeWaitAsyncTurn;
  let turnRunsTasksHere: Promise<boolean> | null = null;
  let session: EmulatorSession | null = null;
  let attach: AttachClient | null = null;
  /**
   * The daemon port this page was served from. It survives a reboot: the daemon hosts the page, not
   * the machine, and the next machine's Wi-Fi bridge uses the same carrier.
   */
  let daemonPort: number | null = null;
  let relay: RelayClient | null = null;
  /** The carrier's position in the machine's outbound window; `null` before its first look. */
  let relayCursor: string | null = null;
  let relayPolledMs = Number.NEGATIVE_INFINITY;
  let audio: PcmTransport | null = null;
  let audioPump: PcmPump | null = null;
  /** The control channels to the two worklets, which carry `{ guestRate }` on both transports. */
  let audioControl: PortLike | null = null;
  let captureControl: PortLike | null = null;
  let postedAudioDriven = false;
  let playbackCounters: PlaybackCounters | null = null;
  let audioTrace = false;
  let captureCounters: CaptureCounters | null = null;
  let loop: LoopRecorder | null = null;
  /** The stall the pump reported in the open iteration, posted with its window at its end. */
  let pendingStall: { fromMs: number; stallMs: number } | null = null;
  let spinning = false;
  let captureSource: (CaptureSource & { close?: () => void }) | null = null;
  let looping = false;
  let yielder: Yielder | null = null;
  /**
   * The panel renderer. The canvas is transferred once, with the first boot, so the display
   * outlives a reboot; a `boot` with a different canvas reopens it there.
   */
  let display: Display | null = null;
  let displayCanvas: OffscreenCanvas | null = null;
  let displayState: DisplayState | null = null;

  const post = (message: FromWorker, transfer?: Transferable[]) => {
    scope.postMessage(message, transfer);
  };

  /**
   * Tells the capture worklet the guest rate. Playback needs no message: the pump writes each format
   * change into the transport ahead of its samples.
   */
  const publishFormats = () => {
    const rate = session?.noteCaptureRate(audioPump?.currentRate ?? null) ?? null;
    if (rate !== null) {
      captureControl?.postMessage({ guestRate: rate });
    }
  };

  const postTrace = (entry: AudioTraceEntry) => {
    const atMs = performance.timeOrigin + performance.now();
    post({ type: "audioTrace", entry: { ...entry, atMs } });
    if (loop && entry.src === "pump" && entry.kind === "stall") {
      pendingStall = { fromMs: atMs - entry.hostMs, stallMs: entry.hostMs };
    }
  };

  const yielderName = () =>
    `${spinning ? "spin" : (yielder?.kind ?? "none")}${yielder?.turn ? `/${yielder.turn}` : ""}`;

  const closeIteration = (outcome: StepOutcome, reportMs: number) => {
    if (!loop) {
      return;
    }
    loop.add("reportMs", reportMs);
    const vtUs = outcome.kind === "ran" ? Number((outcome.toPs - outcome.fromPs) / 1_000_000n) : 0;
    loop.end(outcome.kind, vtUs);
    if (pendingStall) {
      const stall = pendingStall;
      pendingStall = null;
      postTrace({
        src: "loop",
        kind: "window",
        fromMs: stall.fromMs,
        stallMs: stall.stallMs,
        yielder: yielderName(),
        iterations: loop.since(stall.fromMs - 1),
      });
    }
    const summary = loop.summary(LOOP_SUMMARY_MS);
    if (summary) {
      postTrace({ src: "loop", kind: "summary", yielder: yielderName(), ...summary });
    }
  };

  const onWorkletReport = (data: unknown) => {
    const report = data as {
      playback?: PlaybackCounters;
      capture?: CaptureCounters;
      trace?: AudioTraceEntry;
    } | null;
    if (report?.trace && audioTrace) {
      postTrace(report.trace);
    }
    if (report?.playback) {
      playbackCounters = report.playback;
    }
    if (report?.capture) {
      captureCounters = report.capture;
    }
  };

  const audioStats = (): AudioStatsMessage | undefined => {
    if (!audioPump && !session?.microphone) {
      return undefined;
    }
    const pump = audioPump?.stats();
    return {
      pushed: (pump?.pushed ?? 0n).toString(),
      dropped: (pump?.dropped ?? 0n).toString(),
      lost: (pump?.lost ?? 0n).toString(),
      buffered: pump?.buffered ?? 0,
      guestRate: pump?.guestRate ?? null,
      peak: pump?.peak ?? 0,
      playback: playbackCounters,
      capture: captureCounters,
      micChunks: session?.microphone?.stats().chunks ?? 0,
    };
  };

  /**
   * Moves the Wi-Fi bridge's packets both ways once per slice, the browser analogue of
   * `relay_wisp::tick` (`relay.ts`). Only for a daemon-served page, since the relay is a daemon route;
   * otherwise the guest's connection is never answered. With no live bridge it looks at most every
   * {@link RELAY_IDLE_POLL_MS}.
   */
  const pumpRelay = () => {
    if (!session || daemonPort === null) {
      return;
    }
    const hostMs = performance.now();
    if (relay === null && hostMs - relayPolledMs < RELAY_IDLE_POLL_MS) {
      return;
    }
    relayPolledMs = hostMs;
    // A relay that ended carries nothing more. The window is left unread on purpose: the machine then
    // evicts and counts those packets as `dropped_out`, rather than losing them silently here.
    if (relay !== null && !relay.ready && relay.state !== "connecting") {
      return;
    }
    let bridge;
    try {
      bridge = session.relayWindow(relayCursor);
    } catch (error) {
      // A core that does not answer `@relay` has no bridge to carry; say so once and stop asking.
      daemonPort = null;
      post({
        type: "error",
        message: `the Wi-Fi bridge carrier is off: ${error instanceof Error ? error.message : String(error)}`,
      });
      return;
    }
    if (!bridge.attached) {
      if (relay) {
        relay.close();
        relay = null;
        relayCursor = null;
        session.noteLiveBridge(false);
      }
      return;
    }
    if (relay === null) {
      // The first look: packets already in the window predate this carrier and belong to no server,
      // so the cursor starts at the window's end (`relay_wisp::tick`).
      relay = new RelayClient({
        port: daemonPort,
        routes: bridge.routes,
        onError: (reason) => post({ type: "error", message: `the Wi-Fi relay ended: ${reason}` }),
      });
      relay.connect();
      relayCursor = bridge.cursor;
      session.noteLiveBridge(true);
      return;
    }
    session.bridgeFrames(relay.drain());
    relay.send(bridge.packets);
    relayCursor = bridge.cursor;
  };

  /** Frees everything the previous machine held, so a reboot does not leak one. */
  const teardown = () => {
    attach?.close();
    attach = null;
    relay?.close();
    relay = null;
    relayCursor = null;
    relayPolledMs = Number.NEGATIVE_INFINITY;
    captureSource?.close?.();
    captureSource = null;
    audio?.close();
    audio = null;
    audioPump = null;
    audioControl?.close?.();
    captureControl?.close?.();
    audioControl = null;
    captureControl = null;
    postedAudioDriven = false;
    playbackCounters = null;
    captureCounters = null;
    session?.close();
    session = null;
    yielder?.close?.();
    yielder = null;
  };

  let draws = 0;
  /** The run in flight, so a `boot` can wait for the loop to let go of the machine it frees. */
  let loopDone: Promise<void> | null = null;
  let lastStatsMs = Number.NEGATIVE_INFINITY;
  const pumpLoop = () => {
    if (looping || !session) {
      return;
    }
    looping = true;
    const active = session;
    loopDone = active
      .run((report, outcome) => {
        if (!loop) {
          onReport(report, outcome);
          return;
        }
        const at = performance.now();
        try {
          onReport(report, outcome);
        } finally {
          closeIteration(outcome, performance.now() - at);
        }
      })
      // A throwing loop ends the machine; `loopDone` stays a promise a `boot` can wait on.
      .catch((error: unknown) => {
        const code = error instanceof CoreError ? apiErrorCode(error.body) : null;
        post({
          type: "fatal",
          message: error instanceof Error ? error.message : String(error),
          ...(code === null ? {} : { code }),
        });
      })
      .finally(() => {
        looping = false;
      });
  };

  const onReport = (report: SessionReport, outcome: StepOutcome) => {
    if (report.presented) {
      draws += 1;
    }
    pumpRelay();
    for (const slice of report.serial) {
      post({
        type: "serial",
        stream: slice.stream,
        bytes: slice.bytes,
        dropped: slice.dropped.toString(),
        lines: slice.lines.map((mark) => ({
          offset: mark.offset.toString(),
          vtPs: mark.vtPs.toString(),
        })),
        linesDropped: slice.linesDropped.toString(),
      });
    }
    publishFormats();
    // No drift correction while audio drives the clock.
    const audioDriven = report.pacing.clock === "audio";
    if (audioDriven !== postedAudioDriven) {
      postedAudioDriven = audioDriven;
      audioControl?.postMessage({ audioDriven });
    }
    if (report.events.length > 0) {
      post({
        type: "events",
        events: report.events.map((event) => ({
          kind: event.kind,
          vtPs: event.vtPs.toString(),
          arg: event.arg.toString(),
        })),
      });
    }
    // An iteration that ran no slice posts `stats` at most once per `WAIT_STATS_PERIOD_MS`. Without
    // isolation such iterations spin, and a post per spin was about 680,000 `stats` in 3 s: the page
    // stopped answering and the Worker died with `Data cannot be cloned, out of memory`.
    const waited = outcome.kind === "ahead";
    const hostMs = performance.now();
    if (waited && hostMs - lastStatsMs < WAIT_STATS_PERIOD_MS) {
      return;
    }
    lastStatsMs = hostMs;
    post({
      type: "stats",
      nowPs: report.pacing.nowPs.toString(),
      realTimeFactor: report.pacing.realTimeFactor,
      headroom: report.pacing.headroom ?? 0,
      reanchors: report.pacing.reanchors,
      frameGeneration: report.frameGeneration.toString(),
      panel: report.panel,
      draws,
      clock: report.pacing.clock,
      yielder: yielder?.kind,
      ...(yielder?.turn ? { turn: yielder.turn } : {}),
      audio: audioStats(),
    });
    if (outcome.kind === "stopped") {
      post({ type: "stopped", stop: outcome.stop, json: session?.lastStopJson ?? null });
    }
  };

  /**
   * Builds the machine a `boot` asks for: waits out the loop that owns the old one, frees it, loads
   * the core, opens the display and audio transports, and answers `ready` with the boot's `token`.
   */
  const bootMachine = async (message: Extract<ToWorker, { type: "boot" }>): Promise<void> => {
    if (looping) {
      // The loop owns the machine `teardown` frees, so it is paused and waited out first; a live
      // microphone, the one thing that would refuse the pause, is detached with it.
      const refusal = session?.setMode({ kind: "Paused" }, { detach: true }) ?? null;
      if (refusal) {
        post({
          type: "error",
          code: refusal.code,
          message: refusal.message,
          ...(message.token === undefined ? {} : { token: message.token }),
        });
        return;
      }
      await loopDone;
    }
    // Only `pemu_drop` returns what the old machine held.
    teardown();
    // Asked here, while no loop of this Worker is turning.
    const channelTurn =
      message.channelTurn === true || (sabAvailable() && !(await (turnRunsTasksHere ??= probeTurn())));
    // Opened before the core loads: a canvas is transferred only once, so a failed boot must still
    // leave it to the next.
    if (message.canvas && message.canvas !== displayCanvas) {
      const opened: { display: Display | null } = { display: null };
      opened.display = open(message.canvas, FRAME_WIDTH, FRAME_HEIGHT, (state) => {
        if (opened.display !== display) {
          return;
        }
        displayState = state;
        post({ type: "display", ...state });
      });
      display = opened.display;
      displayCanvas = message.canvas;
      displayState = display.state;
    }
    const core = await loadCore(message.config, message.image, message.assets);
    let sab: SharedArrayBuffer | undefined;
    let captureSab: SharedArrayBuffer | undefined;
    let inputSab: SharedArrayBuffer | undefined;
    audioControl = message.audioPort ? portLike(message.audioPort) : null;
    captureControl = message.capturePort ? portLike(message.capturePort) : null;
    if (sabAvailable()) {
      const shared = SharedRingTransport.create(PLAYBACK_RING_SAMPLES);
      audio = shared;
      sab = shared.sharedBuffer;
      const capture = SharedRingTransport.create(CAPTURE_RING_SAMPLES);
      captureSource = capture;
      captureSab = capture.sharedBuffer;
      // Isolated, the loop waits with `Atomics.wait`, woken by an input.
      inputSab = createInputCell();
      // The ports carry only control and counters on this path.
      audioControl?.onData(onWorkletReport);
      captureControl?.onData(onWorkletReport);
    } else {
      // Not isolated: both directions fall back to a transferred MessagePort.
      if (audioControl) {
        audio = new PortTransport(audioControl, PLAYBACK_RING_SAMPLES, onWorkletReport);
      }
      if (captureControl) {
        captureSource = new PortSource(captureControl, onWorkletReport);
      }
    }
    draws = 0;
    if (message.audioTrace) {
      audioTrace = true;
    }
    if (audioTrace && !loop) {
      loop = new LoopRecorder();
    }
    const traced = loop;
    spinning = inputSab !== undefined && message.spinWait === true;
    session = new EmulatorSession(
      {
        core,
        renderer: display?.sink ?? null,
        // Its `width` and `height` are read at every draw, so they are the drawing buffer's real size.
        canvas: displayCanvas ?? undefined,
        nowMs: () => performance.now(),
      },
      performanceClock,
      (yielder = inputSab
        ? atomicsYielder(inputSab, {
            ...(spinning ? { wait: spinWait } : {}),
            ...(channelTurn ? { turn: "message-channel" as const } : {}),
            ...(traced
              ? {
                  onWait: (askedMs: number, waitedMs: number, result: string) =>
                    traced.waited(askedMs, waitedMs, result),
                  onYield: (ms: number) => traced.add("yieldMs", ms),
                }
              : {}),
          })
        : messageChannelYielder()),
    );
    session.traceLoop(traced);
    if (audio) {
      audioPump = session.useAudio(audio);
      if (audioTrace) {
        audioPump.setTrace(postTrace);
      }
    }
    post({
      type: "ready",
      abiVersion: ABI_VERSION,
      audioSab: sab,
      captureSab,
      inputSab,
      // A `ready` with no token answers whatever the page is waiting for.
      ...(message.token === undefined ? {} : { token: message.token }),
    });
    // After `ready`, which stays the first answer to a boot.
    post({
      type: "display",
      ...(displayState ?? {
        backend: "none",
        reason: "the page transferred no OffscreenCanvas to the Worker",
        contextLost: false,
      }),
    });
  };

  // One boot at a time: two concurrent boots would race for the one `session`. Every other message
  // is handled as it arrives.
  let booting: Promise<void> = Promise.resolve();
  scope.onmessage = (event) => {
    const message = event.data;
    const handledAt = loop ? performance.now() : 0;
    void (async () => {
      try {
        switch (message.type) {
          case "boot": {
            const queued = booting.then(() => bootMachine(message));
            // Stays a promise a later boot can wait on; the catch below states this boot's failure.
            booting = queued.catch(() => {});
            await queued;
            break;
          }
          case "mode": {
            const badRate = pacingModeRefusal(message.mode);
            if (badRate !== null) {
              post({ type: "error", code: "E_USAGE", message: `E_USAGE: ${badRate}` });
              break;
            }
            const refusal = session?.setMode(message.mode, { detach: message.detach }) ?? null;
            if (refusal) {
              post({ type: "error", code: refusal.code, message: refusal.message });
              break;
            }
            pumpLoop();
            break;
          }
          case "micStart":
            if (!session || !captureSource) {
              post({ type: "error", message: "micStart needs a booted machine with a capture port" });
              break;
            }
            session.startMicrophone(captureSource);
            pumpLoop();
            break;
          case "micEnd":
            session?.endMicrophone();
            break;
          case "button":
            session?.button(message.id, message.down);
            break;
          case "power":
            session?.power(message.down);
            break;
          case "serial":
            session?.serialIn(message.channel, new TextEncoder().encode(message.text));
            break;
          case "call": {
            if (!session) {
              post({
                type: "call",
                id: message.id,
                err: apiErrorJson("E_STATE", 2, "no machine is booted", 0, "boot a firmware before calling a command"),
              });
              break;
            }
            // A refusal is the call's answer, not a Worker error: the client decodes `err` as an ApiError.
            try {
              post({ type: "call", id: message.id, ok: session.call(message.request) });
            } catch (error) {
              post({
                type: "call",
                id: message.id,
                err:
                  error instanceof CoreError
                    ? error.body
                    : apiErrorJson(
                        "E_INTERNAL",
                        17,
                        error instanceof Error ? error.message : String(error),
                        Number(session.pacing.stats().nowPs / 1_000_000n),
                      ),
              });
            }
            break;
          }
          case "journal":
            try {
              post({
                type: "journal",
                json: JSON.stringify(session?.exportJournal({ includeSecrets: message.includeSecrets }) ?? null),
              });
            } catch (error) {
              post({
                type: "journal",
                json: "null",
                err:
                  error instanceof CoreError
                    ? error.body
                    : apiErrorJson("E_INTERNAL", 17, error instanceof Error ? error.message : String(error), 0),
              });
            }
            break;
          case "frame": {
            if (!session) {
              post({ type: "frame", width: 0, height: 0, generation: "0", pixels: null });
              break;
            }
            // A registry call may have restored or rebuilt the view since the last slice.
            session.views.sync();
            const frame = session.views.frame;
            const pixels = session.views.pixels.slice();
            post(
              {
                type: "frame",
                width: frame.width,
                height: frame.height,
                generation: frame.generation.toString(),
                pixels,
              },
              [pixels.buffer],
            );
            break;
          }
          case "attach": {
            attach?.close();
            // Kept whatever happens to the attach: the Wi-Fi bridge's relay lives there too.
            daemonPort = message.port;
            await booting;
            if (!session) {
              post({ type: "error", message: "attach needs a booted machine" });
              break;
            }
            // The daemon reaches whichever machine this Worker runs, so a load keeps the same `b<n>`.
            attach = new AttachClient(
              {
                port: message.port,
                ...(message.label !== undefined ? { label: message.label } : {}),
                onEvent: (event) => post({ type: "attach", event }),
              },
              (call) => {
                if (!session) {
                  throw new CoreError(
                    2,
                    apiErrorJson("E_STATE", 2, "no machine is booted", 0, "boot a firmware before calling a command"),
                  );
                }
                return session.call(call);
              },
            );
            attach.connect();
            break;
          }
          case "detach":
            attach?.close();
            attach = null;
            break;
        }
      } catch (error) {
        post({
          type: "error",
          message: error instanceof Error ? error.message : String(error),
          // A boot that throws names itself, so a page waiting on a later one is not told its load failed.
          ...(message.type === "boot" && message.token !== undefined ? { token: message.token } : {}),
        });
      }
    })();
    loop?.handled(message.type, performance.now() - handledAt);
  };
}

/**
 * The default loader: instantiate the bundled `pemu_wasm` module (the ROM is compiled in) and build
 * the machine. The firmware is `image` when given, else the bundle `fw` names, else none, which the
 * core refuses with `E_ASSET_MISSING`.
 */
export async function loadBundledCore(
  url: string,
  config: string,
  image?: Uint8Array,
  firmwareUrl: (fw: string) => string | null = () => null,
  extra: readonly CoreAsset[] = [],
): Promise<EmulatorCore> {
  const module = await WebAssembly.instantiateStreaming(fetch(url), {});
  const assets: CoreAsset[] = [];
  // A page that loaded an image sends its own assets; `fw` is then only the name it shows.
  const firmware = image ?? (extra.length > 0 ? null : await fetchFirmware(config, firmwareUrl));
  if (firmware) {
    assets.push({ kind: LoadKind.MergedFlash, bytes: firmware });
  }
  assets.push(...extra);
  return WasmCore.build(module.instance.exports as unknown as CoreExports, config, assets);
}

/** The corpus-id shape a bundled firmware name must have, so no path can be smuggled through it. */
const FIRMWARE_ID = /^[a-z0-9][a-z0-9_-]{0,63}$/;

/** The bundle of firmware `fw` beside the Worker script (`official` is `./official.pebundle`), or `null`. */
export function bundledFirmwareUrl(fw: string, workerScriptUrl: string): string | null {
  return FIRMWARE_ID.test(fw) ? new URL(`./${fw}.pebundle`, workerScriptUrl).href : null;
}

async function fetchFirmware(
  config: string,
  firmwareUrl: (fw: string) => string | null,
): Promise<Uint8Array | null> {
  let fw: unknown;
  try {
    fw = (JSON.parse(config) as { fw?: unknown }).fw;
  } catch {
    return null;
  }
  if (typeof fw !== "string") {
    return null;
  }
  const url = firmwareUrl(fw);
  if (url === null) {
    return null;
  }
  const response = await fetch(url);
  if (!response.ok) {
    // A development bundle ships no demo image.
    throw new Error(
      `the firmware bundle for \`${fw}\` is not served (${response.status}); a development build ships no demo image, so drop a merged bin or .pebundle`,
    );
  }
  return new Uint8Array(await response.arrayBuffer());
}

export const CORE_FILE = "pemu_wasm.wasm";

/**
 * The core's URL: a sibling of the built `worker.js`, so a bundle served at `/emu/` needs no
 * configuration. `file://` is unsupported, since Chromium refuses `file:` fetches.
 */
export function bundledCoreUrl(workerScriptUrl: string): string {
  return new URL(`./${CORE_FILE}`, workerScriptUrl).href;
}

// Starts the Worker only when this module is the Worker's entry; a test imports it without side effects.
const scope = globalThis as unknown as Partial<WorkerScope> & { importScripts?: unknown };
if (typeof scope.postMessage === "function" && "onmessage" in scope) {
  startWorker(scope as WorkerScope, (config, image, assets) =>
    loadBundledCore(
      bundledCoreUrl(import.meta.url),
      config,
      image,
      (fw) => bundledFirmwareUrl(fw, import.meta.url),
      assets,
    ),
  );
}
