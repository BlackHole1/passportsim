// The browser half of `audio.spec.ts`: the real `AudioHost` and worklet module, with the Worker's
// side of both transports played from the page and synthetic PCM standing in for the guest.

import { MicCapture } from "../src/audio/capture";
import { AudioHost, browserDeps, workletUrl } from "../src/audio/host";
import {
  PortSource,
  PortTransport,
  SharedRingTransport,
  portLike,
  type PcmSource,
  type PcmTransport,
} from "../src/audio/transport";
import { PLAYBACK_PROCESSOR, type CaptureCounters, type PlaybackCounters } from "../src/audio/worklet";
import type { InputStamper } from "../src/worker/input";

/** One stretch of guest audio: a sine at `hz` in the left slot; a louder 3 kHz tone in the right. */
export interface ToneSegment {
  readonly rate: number;
  readonly channels: 1 | 2;
  readonly hz: number;
  readonly ms: number;
}

export interface Heard {
  readonly atMs: number;
  /** Frequency from zero crossings over the analyser window. */
  readonly hz: number;
  /** RMS of the window, full scale 1. */
  readonly rms: number;
}

export interface PlaybackResult {
  readonly isolated: boolean;
  readonly contextRate: number;
  readonly pushed: number;
  readonly consumedEnd: string;
  readonly heard: Heard[];
  readonly counters: PlaybackCounters | null;
}

export interface CaptureResult {
  readonly isolated: boolean;
  readonly started: unknown;
  readonly chunks: { readonly seq: string; readonly length: number }[];
  readonly peak: number;
  readonly counters: CaptureCounters | null;
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

const AMPLITUDE = 8_000;

function measure(analyser: AnalyserNode, rate: number, atMs: number): Heard {
  const window = new Float32Array(analyser.fftSize);
  analyser.getFloatTimeDomainData(window);
  let crossings: number[] = [];
  let sum = 0;
  for (let n = 0; n < window.length; n += 1) {
    const value = window[n] ?? 0;
    sum += value * value;
    const previous = window[n - 1];
    if (previous !== undefined && previous < 0 && value >= 0) {
      crossings.push(n - value / (value - previous));
    }
  }
  if (crossings.length < 2) {
    crossings = [0, 0];
  }
  const span = (crossings[crossings.length - 1] ?? 0) - (crossings[0] ?? 0);
  const hz = span > 0 ? ((crossings.length - 1) * rate) / span : 0;
  return { atMs, hz, rms: Math.sqrt(sum / window.length) };
}

/**
 * Plays the segments in 20 ms blocks at a guest's pace (a single push above 250 ms would be cut by
 * the overflow limit), each preceded by its format mark, and measures at the given instants.
 */
async function playback(segments: ToneSegment[], measureAtMs: number[]): Promise<PlaybackResult> {
  const isolated = globalThis.crossOriginIsolated === true;
  const context = new AudioContext({ latencyHint: "interactive" });
  const analyser = context.createAnalyser();
  analyser.fftSize = 4096;
  const deps = browserDeps({
    context,
    onNode: (name, node) => {
      if (name === PLAYBACK_PROCESSOR) {
        node.connect(analyser);
      }
    },
  });
  const host = await AudioHost.open(workletUrl(location.href), deps);
  const ports = host.workerPorts();
  let counters: PlaybackCounters | null = null;
  const onReport = (data: unknown) => {
    const report = (data as { playback?: PlaybackCounters } | null)?.playback;
    if (report) {
      counters = report;
    }
  };
  const control = portLike(ports.audioPort);
  let transport: PcmTransport;
  if (isolated) {
    const shared = SharedRingTransport.create(48_000);
    control.onData(onReport);
    host.attach({ audioSab: shared.sharedBuffer });
    transport = shared;
  } else {
    transport = new PortTransport(control, 48_000, onReport);
    host.attach({});
  }
  await host.resume();

  const start = performance.now();
  const heard: Heard[] = [];
  const pending = [...measureAtMs].sort((a, b) => a - b);
  const listen = () => {
    const now = performance.now() - start;
    while (pending.length > 0 && (pending[0] ?? Infinity) <= now) {
      heard.push(measure(analyser, context.sampleRate, pending.shift() ?? now));
    }
  };
  let pushed = 0;
  // The first 100 ms go in at once, the lead a paced session keeps, so the ring never runs dry.
  let wallMs = -100;
  for (const segment of segments) {
    transport.pushFormat({ guestRate: segment.rate, channels: segment.channels, at: transport.producedSamples() });
    const frames = Math.round((segment.rate * segment.ms) / 1000);
    const block = Math.round(segment.rate / 50);
    for (let first = 0; first < frames; first += block) {
      const count = Math.min(block, frames - first);
      const samples = new Int16Array(count * segment.channels);
      for (let frame = 0; frame < count; frame += 1) {
        const n = first + frame;
        samples[frame * segment.channels] = Math.round(
          AMPLITUDE * Math.sin((2 * Math.PI * segment.hz * n) / segment.rate),
        );
        if (segment.channels === 2) {
          samples[frame * 2 + 1] = Math.round(20_000 * Math.sin((2 * Math.PI * 3_000 * n) / segment.rate));
        }
      }
      pushed += transport.push(samples);
      wallMs += 20;
      // Pace on the wall clock, not on the timer's promise, so late timers do not stretch the song.
      while (performance.now() - start < wallMs) {
        listen();
        await sleep(2);
      }
    }
  }
  while (pending.length > 0) {
    listen();
    await sleep(2);
  }
  await sleep(700);
  const result: PlaybackResult = {
    isolated,
    contextRate: host.contextRate,
    pushed,
    consumedEnd: transport.consumedSamples().toString(),
    heard,
    counters,
  };
  await host.close();
  return result;
}

async function capture(): Promise<CaptureResult> {
  const isolated = globalThis.crossOriginIsolated === true;
  const host = await AudioHost.open(workletUrl(location.href));
  const ports = host.workerPorts();
  let counters: CaptureCounters | null = null;
  const onReport = (data: unknown) => {
    const report = (data as { capture?: CaptureCounters } | null)?.capture;
    if (report) {
      counters = report;
    }
  };
  const control = portLike(ports.capturePort);
  let source: PcmSource;
  if (isolated) {
    const shared = SharedRingTransport.create(12_000);
    control.onData(onReport);
    host.attach({ captureSab: shared.sharedBuffer });
    source = shared;
  } else {
    source = new PortSource(control, onReport);
    host.attach({});
  }
  await host.resume();
  control.postMessage({ guestRate: 16_000 });
  const chunks: { seq: string; length: number }[] = [];
  let peak = 0;
  const stamper = {
    micChunk(seq: bigint, samples: Int16Array) {
      chunks.push({ seq: seq.toString(), length: samples.length });
      for (const sample of samples) {
        peak = Math.max(peak, Math.abs(sample));
      }
    },
  } as unknown as InputStamper;
  const mic = new MicCapture(source, stamper);
  const started = await host.startMicrophone();
  for (let turn = 0; turn < 30; turn += 1) {
    await sleep(50);
    mic.drain();
  }
  host.stopMicrophone();
  await sleep(100);
  mic.flush();
  await host.close();
  return { isolated, started, chunks, peak, counters };
}

(globalThis as { audioProbe?: unknown }).audioProbe = { playback, capture };
