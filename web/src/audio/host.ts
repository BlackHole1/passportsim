// The page's audio graph: one AudioContext, the two worklet nodes, their channels to the Worker,
// and the browser-only `live` microphone. It never touches the core. It runs on the main thread,
// where `AudioContext` and `getUserMedia` live; every browser object comes through
// `AudioHostDeps`, so `bun test` drives the whole flow.

import { MIC_CONSTRAINTS } from "./capture";
import { CAPTURE_PROCESSOR, DEFAULT_GUEST_RATE, PLAYBACK_PROCESSOR } from "./worklet";

export const WORKLET_FILE = "worklet.js";

export function workletUrl(pageScriptUrl: string): string {
  return new URL(`./${WORKLET_FILE}`, pageScriptUrl).href;
}

export interface WorkletNodeLike {
  readonly port: { postMessage(message: unknown, transfer?: Transferable[]): void };
  connect(destination: unknown): unknown;
  disconnect(): void;
}

export interface SourceNodeLike {
  connect(destination: unknown): unknown;
  disconnect(): void;
}

export interface TrackLike {
  readonly label: string;
  stop(): void;
  addEventListener(type: "ended", listener: () => void): void;
}

export interface StreamLike {
  getAudioTracks(): TrackLike[];
}

export interface ContextLike {
  readonly sampleRate: number;
  readonly state: string;
  readonly destination: unknown;
  addModule(url: string): Promise<void>;
  createWorkletNode(name: string, options: AudioWorkletNodeOptions): WorkletNodeLike;
  createMediaStreamSource(stream: StreamLike): SourceNodeLike;
  resume(): Promise<void>;
  close(): Promise<void>;
}

export interface AudioHostDeps {
  createContext(): ContextLike;
  createChannel(): { port1: MessagePort; port2: MessagePort };
  readonly getUserMedia: ((constraints: MediaStreamConstraints) => Promise<StreamLike>) | null;
}

/**
 * The real browser. The context runs at the hardware rate: a fixed rate would need recreating on
 * every guest I2S rate change, and Firefox refuses to connect a microphone to a context whose rate
 * differs from the track's.
 */
export function browserDeps(
  options: {
    readonly context?: AudioContext;
    readonly onNode?: (name: string, node: AudioWorkletNode) => void;
  } = {},
): AudioHostDeps {
  const media = (globalThis.navigator as Navigator | undefined)?.mediaDevices;
  return {
    createContext: () => {
      const context = options.context ?? new AudioContext({ latencyHint: "interactive" });
      return {
        get sampleRate() {
          return context.sampleRate;
        },
        get state() {
          return context.state;
        },
        destination: context.destination,
        addModule: (url) => context.audioWorklet.addModule(url),
        createWorkletNode: (name, nodeOptions) => {
          const node = new AudioWorkletNode(context, name, nodeOptions);
          options.onNode?.(name, node);
          return node;
        },
        createMediaStreamSource: (stream) =>
          context.createMediaStreamSource(stream as unknown as MediaStream),
        resume: () => context.resume(),
        close: () => context.close(),
      };
    },
    createChannel: () => new MessageChannel(),
    getUserMedia:
      media && typeof media.getUserMedia === "function"
        ? (constraints) => media.getUserMedia(constraints) as Promise<StreamLike>
        : null,
  };
}

/** Why the browser gave no microphone. Reported to the user, never swallowed. */
export type MicRefusal =
  | "permission-denied"
  | "insecure-context"
  | "no-device"
  | "device-busy"
  | "unsupported"
  | "not-attached"
  /** Anything else, `AbortError` included: it names no cause. `message` says what the browser said. */
  | "failed";

export type MicStart =
  | { readonly ok: true; readonly label: string; readonly contextRate: number }
  | { readonly ok: false; readonly reason: MicRefusal; readonly message: string };

export function refusalOf(error: unknown): { reason: MicRefusal; message: string } {
  const name = (error as { name?: unknown } | null)?.name;
  const message = error instanceof Error ? error.message : String(error);
  switch (name) {
    case "NotAllowedError":
      return { reason: "permission-denied", message };
    case "SecurityError":
      return { reason: "insecure-context", message };
    case "NotFoundError":
    case "OverconstrainedError":
      return { reason: "no-device", message };
    case "NotReadableError":
      return { reason: "device-busy", message };
    default:
      return { reason: "failed", message };
  }
}

export interface WorkerRings {
  readonly audioSab?: SharedArrayBuffer;
  readonly captureSab?: SharedArrayBuffer;
}

/**
 * One AudioContext with the playback node on the speakers and a capture node for a microphone.
 * Order: {@link AudioHost.open}, {@link AudioHost.workerPorts} into the Worker's `boot`, then
 * {@link AudioHost.attach} with the `ready` rings.
 */
export class AudioHost {
  private playback: WorkletNodeLike | null = null;
  private capture: WorkletNodeLike | null = null;
  /**
   * Channel pairs minted and not yet attached, oldest first. A queue, not one slot: a drop can boot
   * while the demo's boot is in flight, and one slot would attach the same pair twice. FIFO holds
   * because the Worker serialises boots.
   */
  private pending: { audio: MessageChannel; capture: MessageChannel }[] = [];
  private mic: { source: SourceNodeLike; tracks: TrackLike[] } | null = null;

  private constructor(
    private readonly context: ContextLike,
    private readonly deps: AudioHostDeps,
    private readonly onMicEnded: () => void,
  ) {}

  /**
   * Creates the context and loads the worklet module. The context starts suspended; call
   * {@link AudioHost.resume} from a user gesture.
   *
   * @param onMicEnded called when the browser ends the microphone track itself (unplugged, permission
   *   revoked); the page then sends the Worker `micEnd`.
   */
  static async open(
    url: string,
    deps: AudioHostDeps = browserDeps(),
    onMicEnded: () => void = () => {},
  ): Promise<AudioHost> {
    const context = deps.createContext();
    await context.addModule(url);
    return new AudioHost(context, deps, onMicEnded);
  }

  get contextRate(): number {
    return this.context.sampleRate;
  }

  get microphoneLive(): boolean {
    return this.mic !== null;
  }

  /** The Worker's ends of the two channels, to transfer in its `boot` message. */
  workerPorts(): { audioPort: MessagePort; capturePort: MessagePort } {
    const audio = this.deps.createChannel() as MessageChannel;
    const capture = this.deps.createChannel() as MessageChannel;
    this.pending.push({ audio, capture });
    return { audioPort: audio.port1, capturePort: capture.port1 };
  }

  /**
   * Builds both nodes on the Worker's transport: its shared rings when `ready` carried them, the
   * ports otherwise. The port goes to the node either way: format changes and counters travel on it.
   */
  attach(rings: WorkerRings, options: { readonly trace?: boolean } = {}): void {
    // Taken, not read: a pair belongs to the one machine whose `boot` carried its other ends.
    const channels = this.pending.shift();
    if (!channels) {
      throw new Error("AudioHost.attach before workerPorts: the Worker has no channel to the worklets");
    }
    // A reboot attaches anew; the replaced machine's nodes are disconnected, or each dropped image
    // would leave one silent worklet in the render graph.
    this.playback?.disconnect();
    this.capture?.disconnect();
    this.playback = this.context.createWorkletNode(PLAYBACK_PROCESSOR, {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [1],
    });
    this.playback.connect(this.context.destination);
    this.playback.port.postMessage(
      { sab: rings.audioSab, guestRate: DEFAULT_GUEST_RATE, ...(options.trace ? { trace: true } : {}) },
      [channels.audio.port2],
    );
    this.capture = this.context.createWorkletNode(CAPTURE_PROCESSOR, {
      numberOfInputs: 1,
      numberOfOutputs: 0,
      channelCount: 1,
      channelCountMode: "explicit",
    });
    this.capture.port.postMessage(
      { sab: rings.captureSab },
      [channels.capture.port2],
    );
  }

  async resume(): Promise<void> {
    if (this.context.state !== "running") {
      await this.context.resume();
    }
  }

  /**
   * Asks for the microphone with browser DSP off and connects it to the capture node. A refusal is a
   * value naming the reason; it never throws and never silently captures nothing. On `ok` the page
   * sends `micStart`, and the Worker pins pacing to `Wall { rate: 1 }` until `micEnd`.
   */
  async startMicrophone(): Promise<MicStart> {
    if (this.mic) {
      const label = this.mic.tracks[0]?.label ?? "";
      return { ok: true, label, contextRate: this.context.sampleRate };
    }
    if (!this.capture) {
      return { ok: false, reason: "not-attached", message: "attach the host to a booted Worker first" };
    }
    if (!this.deps.getUserMedia) {
      return {
        ok: false,
        reason: "unsupported",
        message: "this browser has no navigator.mediaDevices.getUserMedia (is the page a secure context?)",
      };
    }
    let stream: StreamLike;
    try {
      stream = await this.deps.getUserMedia({ audio: MIC_CONSTRAINTS, video: false });
    } catch (error) {
      return { ok: false, ...refusalOf(error) };
    }
    const tracks = stream.getAudioTracks();
    if (tracks.length === 0) {
      return { ok: false, reason: "no-device", message: "the granted stream has no audio track" };
    }
    const source = this.context.createMediaStreamSource(stream);
    source.connect(this.capture);
    this.mic = { source, tracks };
    for (const track of tracks) {
      track.addEventListener("ended", () => {
        if (this.mic?.tracks.includes(track)) {
          this.releaseMicrophone();
          this.onMicEnded();
        }
      });
    }
    return { ok: true, label: tracks[0]?.label ?? "", contextRate: this.context.sampleRate };
  }

  /** Disconnects and stops the microphone; the page then sends `micEnd`, which releases the pacing lease. */
  stopMicrophone(): void {
    this.releaseMicrophone();
  }

  async close(): Promise<void> {
    this.releaseMicrophone();
    this.playback?.disconnect();
    this.capture?.disconnect();
    this.playback = null;
    this.capture = null;
    await this.context.close();
  }

  private releaseMicrophone(): void {
    const mic = this.mic;
    if (!mic) {
      return;
    }
    this.mic = null;
    mic.source.disconnect();
    for (const track of mic.tracks) {
      track.stop();
    }
  }
}
