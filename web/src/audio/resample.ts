// Sample-rate conversion for the worklets, allocation-free on the audio thread. Four-tap cubic
// Hermite (Catmull-Rom); a windowed-sinc polyphase filter would be a drop-in upgrade.
//
// The exact definition (what the tests check). The read position is rational, kept as integers, so
// the output is an exact function of the input:
//
//   step_k   = inRate * (PPM + drift_k) / (outRate * PPM)       drift in whole parts per million
//   pos_0    = 0,  pos_{k+1} = pos_k + step_k
//   out[k]   = CR(x, pos_k - 2)
//
// where `x[n]` is the n-th input since the last `reset` scaled by 1/32768, `x[n < 0] = 0`, and
// `CR(x, n + t)` is the Catmull-Rom cubic through `x[n-1..n+2]` at `t`. The 2-sample delay is the
// least a causal four-tap interpolator needs. With constant drift a run never drifts from its rate:
// 24000 inputs at 24 kHz into 44.1 kHz give exactly 44100 outputs.
//
// Nothing guest-visible depends on this: the guest reads its own I2S DMA, and the journal records
// the converted capture samples, so a replay never re-runs it.

/** The widest drift correction a controller may ask for: plus or minus 0.5 % is inaudible for speech. */
export const MAX_DRIFT = 0.005;

/** Parts per million: the unit drift is quantized to, so the phase stays an exact integer. */
export const PPM = 1_000_000;

export const MAX_DRIFT_PPM = Math.round(MAX_DRIFT * PPM);

/**
 * A rational-phase Catmull-Rom resampler over `i16`. It keeps the four samples around the read
 * position, so `process` gives the same outputs for any split of the input.
 */
export class HermiteResampler {
  private readonly history = new Float64Array(4);
  /** Fractional read position, in units of `1 / (outRate * PPM)`. Always `< outRate * PPM`. */
  private frac = 0;
  private need = 1;
  private inRate = 0;
  private outRate = 0;

  reset(): void {
    this.history.fill(0);
    this.frac = 0;
    this.need = 1;
  }

  /**
   * Converts `input` at `inRate` into `output` at `outRate`; returns outputs produced and inputs
   * consumed, stopping when either runs out (unconsumed input must be offered again). A rate change
   * resets the resampler; `driftPpm` is clamped to {@link MAX_DRIFT_PPM} and may change per call, a
   * positive drift reading the input faster.
   */
  process(
    input: Int16Array,
    output: Float32Array,
    inRate: number,
    outRate: number,
    driftPpm = 0,
  ): { produced: number; consumed: number } {
    if (!validRate(inRate) || !validRate(outRate)) {
      output.fill(0);
      return { produced: output.length, consumed: 0 };
    }
    if (inRate !== this.inRate || outRate !== this.outRate) {
      this.inRate = inRate;
      this.outRate = outRate;
      this.reset();
    }
    const den = outRate * PPM;
    const drift = Math.max(-MAX_DRIFT_PPM, Math.min(MAX_DRIFT_PPM, Math.round(driftPpm) || 0));
    const increment = inRate * (PPM + drift);
    const h = this.history;
    let read = 0;
    let written = 0;
    while (written < output.length) {
      while (this.need > 0 && read < input.length) {
        h[0] = h[1] ?? 0;
        h[1] = h[2] ?? 0;
        h[2] = h[3] ?? 0;
        h[3] = (input[read] ?? 0) / 32768;
        read += 1;
        this.need -= 1;
      }
      if (this.need > 0) {
        break;
      }
      output[written] = catmullRom(h, this.frac / den);
      written += 1;
      // Integers below 2^53 throughout, so the division and the remainder are exact.
      const next = this.frac + increment;
      this.need += Math.floor(next / den);
      this.frac = next % den;
    }
    return { produced: written, consumed: read };
  }

  /**
   * Exactly how many inputs the next `outputs` outputs take. The playback worklet pulls this many
   * and no more, so the consumed counter the `Audio` anchor reads is the count actually played.
   */
  inputsNeeded(outputs: number, inRate: number, outRate: number, driftPpm = 0): number {
    if (outputs <= 0 || !validRate(inRate) || !validRate(outRate)) {
      return 0;
    }
    const fresh = inRate !== this.inRate || outRate !== this.outRate;
    const need = fresh ? 1 : this.need;
    const frac = fresh ? 0 : this.frac;
    const den = outRate * PPM;
    const drift = Math.max(-MAX_DRIFT_PPM, Math.min(MAX_DRIFT_PPM, Math.round(driftPpm) || 0));
    const increment = inRate * (PPM + drift);
    return need + Math.floor((frac + (outputs - 1) * increment) / den);
  }
}

function validRate(rate: number): boolean {
  return Number.isInteger(rate) && rate > 0 && rate <= 384_000;
}

export function catmullRom(h: ArrayLike<number>, t: number): number {
  const y0 = h[0] ?? 0;
  const y1 = h[1] ?? 0;
  const y2 = h[2] ?? 0;
  const y3 = h[3] ?? 0;
  const c0 = y1;
  const c1 = 0.5 * (y2 - y0);
  const c2 = y0 - 2.5 * y1 + 2 * y2 - 0.5 * y3;
  const c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
  return ((c3 * t + c2) * t + c1) * t + c0;
}

export const DECIMATOR_TAPS = 47;

export const DECIMATOR_CUTOFF = 0.4;

/**
 * The capture anti-alias low-pass: a Blackman-windowed sinc of {@link DECIMATOR_TAPS} taps, cutoff
 * {@link DECIMATOR_CUTOFF} times `outRate`, unit DC gain, linear phase. Measured with tones of
 * amplitude 16000 (the bounds in `resample.test.ts`): 48 to 16 kHz passes 4 kHz at -0.03 dB and
 * rejects 9 kHz by 59.7 dB and 12 kHz by 84.1 dB. 31 taps gave only about 29 dB at 9 kHz.
 * Engines may differ in the last bits of `Math.sin`; replays cannot, since mic samples enter the
 * machine only as journaled chunks.
 */
export function lowpassTaps(inRate: number, outRate: number): Float64Array {
  const taps = DECIMATOR_TAPS;
  const h = new Float64Array(taps);
  const middle = (taps - 1) / 2;
  const cutoff = (DECIMATOR_CUTOFF * outRate) / inRate;
  let sum = 0;
  for (let n = 0; n < taps; n += 1) {
    const x = n - middle;
    const sinc = x === 0 ? 2 * cutoff : Math.sin(2 * Math.PI * cutoff * x) / (Math.PI * x);
    const window =
      0.42 - 0.5 * Math.cos((2 * Math.PI * n) / (taps - 1)) + 0.08 * Math.cos((4 * Math.PI * n) / (taps - 1));
    h[n] = sinc * window;
    sum += h[n] ?? 0;
  }
  for (let n = 0; n < taps; n += 1) {
    h[n] = (h[n] ?? 0) / sum;
  }
  return h;
}

/**
 * The capture direction, `i16` in and out. When `inRate > outRate` the input first passes
 * {@link lowpassTaps} so nothing above the guest's Nyquist folds in, then {@link HermiteResampler}.
 * Exactly: `y[n] = round(sum_k h[k] x[n-k])`, then `out[k] = round(32768 * CR(y / 32768, pos_k - 2))`,
 * each rounding half away from zero and clamped to `i16`.
 */
export class Downsampler {
  private readonly resampler = new HermiteResampler();
  private taps: Float64Array | null = null;
  private readonly history = new Float64Array(DECIMATOR_TAPS);
  private historyIndex = 0;
  private inRate = 0;
  private outRate = 0;
  private readonly filtered: Int16Array;
  private readonly converted: Float32Array;

  /** @param maxBlock the largest input block `process` is given. */
  constructor(maxBlock: number) {
    this.filtered = new Int16Array(maxBlock);
    this.converted = new Float32Array(maxBlock);
  }

  reset(): void {
    this.history.fill(0);
    this.historyIndex = 0;
    this.resampler.reset();
  }

  /** Filters and converts `input` (at most `maxBlock`); `output` must hold `ceil(input.length * outRate / inRate) + 1`. */
  process(input: Int16Array, output: Int16Array, inRate: number, outRate: number): number {
    if (inRate !== this.inRate || outRate !== this.outRate) {
      this.inRate = inRate;
      this.outRate = outRate;
      this.taps = inRate > outRate && outRate > 0 ? lowpassTaps(inRate, outRate) : null;
      this.reset();
    }
    const count = Math.min(input.length, this.filtered.length);
    const taps = this.taps;
    const length = DECIMATOR_TAPS;
    for (let index = 0; index < count; index += 1) {
      const sample = input[index] ?? 0;
      if (!taps) {
        this.filtered[index] = sample;
        continue;
      }
      this.historyIndex = this.historyIndex + 1 === length ? 0 : this.historyIndex + 1;
      this.history[this.historyIndex] = sample;
      let sum = 0;
      let slot = this.historyIndex;
      for (let k = 0; k < length; k += 1) {
        sum += (taps[k] ?? 0) * (this.history[slot] ?? 0);
        slot = slot === 0 ? length - 1 : slot - 1;
      }
      this.filtered[index] = toInt16(sum);
    }
    const { produced } = this.resampler.process(
      this.filtered.subarray(0, count),
      this.converted.subarray(0, Math.min(this.converted.length, output.length)),
      inRate,
      outRate,
    );
    for (let index = 0; index < produced; index += 1) {
      output[index] = toInt16((this.converted[index] ?? 0) * 32768);
    }
    return produced;
  }
}

export function roundHalfAway(value: number): number {
  return value < 0 ? -Math.round(-value) : Math.round(value);
}

export function toInt16(value: number): number {
  return Math.max(-32768, Math.min(32767, roundHalfAway(value)));
}

export function floatToInt16(sample: number): number {
  return toInt16((Number.isFinite(sample) ? sample : 0) * 32768);
}

/**
 * A PI drift controller on the ring fill level: the error is low-passed over about a second, the
 * proportional term acts at once, and the integral term (clamped to {@link MAX_DRIFT}) removes the
 * steady offset a constant rate mismatch leaves. It shapes only what the speakers hear, never
 * virtual time, and is held at zero while `Audio` pacing drives.
 */
export class DriftController {
  private smoothed = 0;
  private integral = 0;

  constructor(
    readonly targetSamples: number,
    /** Smoothing factor per update; about one second at one update per render quantum. */
    private readonly alpha = 1 / 375,
    private readonly gain = 0.5,
    /** Integral gain per update: the proportional gain spread over about ten seconds of quanta. */
    private readonly integralGain = 0.5 / 3750,
  ) {}

  update(fillSamples: number): number {
    if (this.targetSamples <= 0) {
      return 0;
    }
    const error = (fillSamples - this.targetSamples) / this.targetSamples;
    this.smoothed += this.alpha * (error - this.smoothed);
    this.integral = clamp(this.integral + this.integralGain * this.smoothed, MAX_DRIFT);
    return this.correctionPpm;
  }

  get correctionPpm(): number {
    return Math.round(clamp(this.smoothed * this.gain + this.integral, MAX_DRIFT) * PPM);
  }
}

function clamp(value: number, limit: number): number {
  return Math.max(-limit, Math.min(limit, value));
}
