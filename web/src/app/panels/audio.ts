// The Audio card: meter and mic source. The mic source goes through `mic_set`, not `env`: both
// commands of this card are in the opt-in `audio` caps group, so a daemon without `--caps audio`
// refuses the whole card consistently and a copied CLI line names the journaled command. `live`
// (`getUserMedia`) is browser-only; the card says so.

import type { AudioCaptureArgs, AudioCaptureResult, MicSetArgs } from "../../api/commands";

export type MicSource = MicSetArgs["kind"];

export interface AudioState {
  readonly source: MicSource;
  readonly toneHz: number;
  /** Tone peak as a 16-bit sample (0 to 32767), as `mic_set` takes it. */
  readonly amplitude: number;
  /** The WAV the `file` source reads, as a name below the host's audio directory. */
  readonly fileName: string;
  readonly captureMs: number;
  /** `digital` records exact transmitted samples, `analog` applies DAC gain. */
  readonly captureMode: NonNullable<AudioCaptureArgs["mode"]>;
}

/** -6 dBFS as a 16-bit peak. */
export const MINUS_6_DBFS = 16_422;

/** Silence, so no test can see a non-empty capture without the guest doing anything. */
export const DEFAULT_AUDIO: AudioState = {
  source: "silence",
  toneHz: 1_000,
  amplitude: MINUS_6_DBFS,
  fileName: "",
  captureMs: 1_000,
  captureMode: "digital",
};

export const MIC_SOURCES: readonly { readonly id: MicSource; readonly note: string }[] = [
  { id: "silence", note: "no signal" },
  { id: "tone", note: "a synthesized sine at the chosen frequency" },
  { id: "file", note: "a WAV from the host's audio directory" },
  { id: "live", note: "getUserMedia; browser-only in v1, absent on a native host" },
];

export function isBrowserOnly(source: MicSource): boolean {
  return source === "live";
}

/**
 * Tone bounds (`env`'s `MIC_TONE_HZ_MAX`). The guest's lowest I2S rate is 16 kHz, so a tone above
 * 8 kHz would alias.
 */
export const TONE_RANGE = { min: 1, max: 8_000 } as const;

export const AMPLITUDE_RANGE = { min: 0, max: 32_767 } as const;

/** Capture length bounds; the upper one is `audio_capture`'s `DURATION_MS_MAX` (10 minutes). */
export const CAPTURE_MS_RANGE = { min: 1, max: 600_000 } as const;

export class AudioFormError extends Error {
  constructor(
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "AudioFormError";
  }
}

function inRange(value: number, range: { min: number; max: number }): boolean {
  return Number.isInteger(value) && value >= range.min && value <= range.max;
}

/** The `mic_set` arguments, with only the fields the source reads: `mic_set` refuses the rest. */
export function toArgs(state: AudioState): MicSetArgs {
  switch (state.source) {
    case "tone":
      if (!inRange(state.toneHz, TONE_RANGE)) {
        throw new AudioFormError(
          "toneHz",
          `a tone is ${TONE_RANGE.min} to ${TONE_RANGE.max} Hz; above that it aliases at the guest's 16 kHz rate`,
        );
      }
      if (!inRange(state.amplitude, AMPLITUDE_RANGE)) {
        throw new AudioFormError(
          "amplitude",
          `an amplitude is ${AMPLITUDE_RANGE.min} to ${AMPLITUDE_RANGE.max}, a 16-bit peak`,
        );
      }
      return { kind: "tone", hz: state.toneHz, amplitude: state.amplitude };
    case "file": {
      const name = state.fileName.trim();
      if (name.length === 0) {
        throw new AudioFormError("fileName", "a file source needs a file name");
      }
      return { kind: "file", name };
    }
    case "silence":
    case "live":
      return { kind: state.source };
  }
}

export function fromArgs(args: MicSetArgs, base: AudioState = DEFAULT_AUDIO): AudioState {
  return {
    ...base,
    source: args.kind,
    toneHz: args.hz ?? base.toneHz,
    amplitude: args.amplitude ?? base.amplitude,
    fileName: args.name ?? base.fileName,
  };
}

/**
 * The `audio_capture` arguments. It runs the machine for `duration_ms` and returns what it recorded;
 * there is no start/stop pair, because a capture outliving the call would be state the journal does
 * not hold.
 */
export function captureArgs(state: AudioState): AudioCaptureArgs {
  if (!inRange(state.captureMs, CAPTURE_MS_RANGE)) {
    throw new AudioFormError(
      "captureMs",
      `a capture runs ${CAPTURE_MS_RANGE.min} to ${CAPTURE_MS_RANGE.max} ms of virtual time`,
    );
  }
  return { duration_ms: state.captureMs, mode: state.captureMode };
}

export function fromCaptureArgs(args: AudioCaptureArgs, base: AudioState = DEFAULT_AUDIO): AudioState {
  return {
    ...base,
    captureMs: args.duration_ms ?? base.captureMs,
    captureMode: args.mode ?? base.captureMode,
  };
}

/** The card's line for a capture: fundamental and peak, or why there is none. Never throws. */
export function describeCapture(json: unknown): string {
  if (typeof json !== "object" || json === null) {
    return "no samples captured";
  }
  const result = json as AudioCaptureResult;
  const analysis = result.analysis;
  if (!analysis) {
    return "no samples captured";
  }
  const hz = analysis.fundamental_hz === null ? "no fundamental" : `${analysis.fundamental_hz} Hz`;
  const dropped = result.dropped_samples ? `, ${result.dropped_samples} samples dropped` : "";
  return `${hz}, peak ${analysis.peak}${dropped}`;
}

/**
 * The meter level, 0 to 1, from a block of PCM. RMS rather than peak: speech between glottal
 * pulses sits near zero and a peak meter reads it as silence.
 */
export function meterLevel(samples: Int16Array): number {
  if (samples.length === 0) {
    return 0;
  }
  let sum = 0;
  for (const sample of samples) {
    sum += sample * sample;
  }
  return Math.min(1, Math.sqrt(sum / samples.length) / 32_768);
}

export function meterDbfs(level: number): string {
  if (level <= 0) {
    return "-inf";
  }
  return `${Math.round(20 * Math.log10(level))}`;
}

export interface AudioOutput {
  readonly peak: number;
  readonly pushed: string;
  /**
   * Render quanta and underruns of the page's playback worklet, or `null` before it reported. Unlike
   * the pump's counters, these move only when the page actually plays.
   */
  readonly quanta: number | null;
  readonly underruns: number | null;
  /**
   * Quanta that were partly or wholly silence because the ring was empty. An underrun counts only
   * sound turning into silence, so a path that never played reports zero underruns; this does not.
   */
  readonly starvedQuanta: number | null;
}

export const METER_FLOOR_DB = -96;

export function dbfs(peak: number): number {
  return peak <= 0 ? METER_FLOOR_DB : Math.max(METER_FLOOR_DB, 20 * Math.log10(peak));
}

/**
 * The meter's value: dBFS mapped onto 0..1. A half-scale square is -6 dBFS: halfway up a linear
 * meter, 94 % of this one, which is what a person reads as "it is playing".
 */
export function meterValue(peak: number): number {
  return (dbfs(peak) - METER_FLOOR_DB) / -METER_FLOOR_DB;
}
