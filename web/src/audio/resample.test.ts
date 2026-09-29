import { describe, expect, test } from "bun:test";

import {
  DriftController,
  Downsampler,
  HermiteResampler,
  MAX_DRIFT_PPM,
  catmullRom,
  DECIMATOR_TAPS,
  floatToInt16,
  lowpassTaps,
  toInt16,
} from "./resample";

const F32_DIGITS = 6;

function run(input: Int16Array, outputs: number, inRate: number, outRate: number) {
  const resampler = new HermiteResampler();
  const output = new Float32Array(outputs);
  const result = resampler.process(input, output, inRate, outRate);
  return { output, ...result };
}

describe("the Catmull-Rom kernel", () => {
  test("passes through the two middle points and reproduces a straight line", () => {
    expect(catmullRom([1, 2, 3, 4], 0)).toBe(2);
    expect(catmullRom([1, 2, 3, 4], 0.5)).toBe(2.5);
    expect(catmullRom([7, -3, 5, 9], 0)).toBe(-3);
  });
});

describe("16 kHz into a 48 kHz context", () => {
  test("an impulse gives the Catmull-Rom impulse response at thirds, two samples late", () => {
    // x[0] = 0.5; out[k] = CR(x, k/3 - 2). Hand-derived: the weights of (y0, y1, y2, y3) are
    // (-2/27, 7/9, 1/3, -1/27) at t = 1/3 and (-1/27, 1/3, 7/9, -2/27) at t = 2/3.
    const input = new Int16Array(8);
    input[0] = 16384;
    const { output, produced, consumed } = run(input, 15, 16_000, 48_000);
    const half = [0, -1 / 27, -2 / 27, 0, 1 / 3, 7 / 9, 1, 7 / 9, 1 / 3, 0, -2 / 27, -1 / 27, 0, 0, 0];
    expect(produced).toBe(15);
    // Output 14 sits at position 14/3 - 2, which needs x[0..4].
    expect(consumed).toBe(5);
    half.forEach((value, k) => {
      expect(output[k] ?? Number.NaN).toBeCloseTo(value / 2, F32_DIGITS);
    });
  });

  test("a chunk split anywhere gives the same samples as one call", () => {
    const input = Int16Array.from({ length: 64 }, (_, n) => ((n * 7919) % 20_000) - 10_000);
    const whole = run(input, 180, 16_000, 48_000).output;
    const split = new HermiteResampler();
    const pieces = new Float32Array(180);
    let read = 0;
    let written = 0;
    for (const size of [5, 1, 17, 3, 38]) {
      const end = Math.min(input.length, read + size);
      while (read < end && written < pieces.length) {
        const { produced, consumed } = split.process(
          input.subarray(read, end),
          pieces.subarray(written),
          16_000,
          48_000,
        );
        read += consumed;
        written += produced;
        if (consumed === 0) {
          break;
        }
      }
    }
    expect(Array.from(pieces.subarray(0, written))).toEqual(Array.from(whole.subarray(0, written)));
    expect(written).toBeGreaterThan(170);
  });

  test("48000 outputs take exactly 16000 inputs", () => {
    const { produced, consumed } = run(new Int16Array(20_000), 48_000, 16_000, 48_000);
    expect(produced).toBe(48_000);
    // Output 47999 sits at 15999.67 - 2, so x[0..15999] have been taken.
    expect(consumed).toBe(16_000);
  });
});

describe("24 kHz into a 44.1 kHz context", () => {
  test("a ramp comes out as the same ramp at every rational position", () => {
    // Catmull-Rom is exact on a straight line, so from position 1 on out[k] = 8 (k*80/147 - 2),
    // with 24000/44100 = 80/147 exactly and no accumulated phase error.
    const input = Int16Array.from({ length: 4000 }, (_, n) => n * 8);
    const { output } = run(input, 7000, 24_000, 44_100);
    for (const k of [6, 7, 147, 148, 1000, 4410, 6999]) {
      const position = (k * 80) / 147 - 2;
      expect(output[k] ?? Number.NaN).toBeCloseTo((position * 8) / 32768, F32_DIGITS);
    }
  });

  test("one second of input is exactly one second of output, with no drift", () => {
    const resampler = new HermiteResampler();
    const input = new Int16Array(24_000).fill(1000);
    const output = new Float32Array(50_000);
    const { produced, consumed } = resampler.process(input, output, 24_000, 44_100);
    expect(consumed).toBe(24_000);
    // Outputs k with floor(k*80/147) <= 23999: k <= 44099.
    expect(produced).toBe(44_100);
  });
});

describe("rates and drift", () => {
  test("a rate change restarts the stream instead of gliding across the seam", () => {
    const input = Int16Array.from({ length: 8 }, (_, n) => 4000 * (n + 1));
    const resampler = new HermiteResampler();
    resampler.process(new Int16Array(8).fill(32_000), new Float32Array(3), 16_000, 48_000);
    const after = new Float32Array(6);
    resampler.process(input, after, 24_000, 48_000);
    const fresh = new Float32Array(6);
    new HermiteResampler().process(input, fresh, 24_000, 48_000);
    expect(Array.from(after)).toEqual(Array.from(fresh));
    expect(after[0]).toBe(0);
  });

  test("a positive drift reads faster, and the correction is clamped to half a percent", () => {
    const input = new Int16Array(200_000);
    const plain = new HermiteResampler().process(input, new Float32Array(100_000), 48_000, 48_000);
    const fast = new HermiteResampler().process(input, new Float32Array(100_000), 48_000, 48_000, 5_000);
    const clamped = new HermiteResampler().process(
      input,
      new Float32Array(100_000),
      48_000,
      48_000,
      50_000,
    );
    expect(plain.consumed).toBe(100_000);
    // Output 99999 sits at 99999 * 1.005 = 100498.995, so x[0..100498] have been taken.
    expect(fast.consumed).toBe(100_499);
    expect(clamped.consumed).toBe(fast.consumed);
    expect(MAX_DRIFT_PPM).toBe(5_000);
  });

  test("a nonsensical rate outputs silence rather than looping forever", () => {
    const output = new Float32Array(8).fill(1);
    const result = new HermiteResampler().process(new Int16Array(8), output, Number.NaN, 48_000);
    expect(result).toEqual({ produced: 8, consumed: 0 });
    expect(Array.from(output)).toEqual(new Array(8).fill(0));
  });
});

describe("the capture downsampler", () => {
  function toneGainDb(inRate: number, outRate: number, hz: number): number {
    const sampler = new Downsampler(128);
    const block = new Int16Array(128);
    const out = new Int16Array(64);
    const collected: number[] = [];
    for (let n = 0, b = 0; b < 800; b += 1) {
      for (let k = 0; k < 128; k += 1, n += 1) {
        block[k] = Math.round(16_000 * Math.sin((2 * Math.PI * hz * n) / inRate));
      }
      const produced = sampler.process(block, out, inRate, outRate);
      collected.push(...Array.from(out.subarray(0, produced)));
    }
    const settled = collected.slice(2_000);
    const rms = Math.sqrt(settled.reduce((sum, v) => sum + v * v, 0) / settled.length);
    return 20 * Math.log10(Math.max(rms, 1e-9) / (16_000 / Math.SQRT2));
  }

  test("rejects what would alias into the guest band and keeps the voice band", () => {
    const rows = [
      [48_000, 9_000],
      [48_000, 12_000],
      [44_100, 9_000],
      [44_100, 12_000],
    ] as const;
    const measured = rows.map(([rate, hz]) => toneGainDb(rate, 16_000, hz));
    // Measured: -59.7, -84.1, -72.0 and -83.2 dB; the 4 kHz rows -0.03 and -0.02 dB.
    for (const gain of measured) {
      expect(gain).toBeLessThan(-50);
    }
    expect(Math.abs(toneGainDb(48_000, 16_000, 4_000))).toBeLessThan(0.2);
    expect(Math.abs(toneGainDb(44_100, 16_000, 4_000))).toBeLessThan(0.2);
    // 44.1 kHz to 24 kHz once had no filter at all (floor(44100 / 24000) = 1).
    expect(toneGainDb(44_100, 24_000, 14_000)).toBeLessThan(-50);
  });

  test("the filter is a symmetric, unit-gain, linear-phase low-pass", () => {
    const h = lowpassTaps(48_000, 16_000);
    expect(h.length).toBe(DECIMATOR_TAPS);
    expect(h.reduce((sum, v) => sum + v, 0)).toBeCloseTo(1, 12);
    for (let k = 0; k < h.length; k += 1) {
      expect(h[k]).toBeCloseTo(h[h.length - 1 - k] ?? Number.NaN, 15);
    }
  });

  test("48 kHz to 16 kHz is the filtered stream sampled every third input, two filtered samples late", () => {
    const h = lowpassTaps(48_000, 16_000);
    const input = new Int16Array(120);
    input[0] = 16_384;
    const output = new Int16Array(41);
    const produced = new Downsampler(128).process(input, output, 48_000, 16_000);
    // y[n] = round(16384 h[n]); out[k] = y[3k - 2].
    const y = (n: number) => (n < 0 || n >= h.length ? 0 : toInt16(16_384 * (h[n] ?? 0)));
    expect(produced).toBe(40);
    for (let k = 0; k < produced; k += 1) {
      expect(output[k]).toBe(y(3 * k - 2) + 0);
    }
  });

  test("44.1 kHz to 16 kHz keeps a constant level and produces exactly the rate's outputs", () => {
    const sampler = new Downsampler(128);
    const out = new Int16Array(64);
    let produced = 0;
    let last = 0;
    for (let block = 0; block < 441; block += 1) {
      const n = sampler.process(new Int16Array(100).fill(-12_000), out, 44_100, 16_000);
      produced += n;
      last = out[n - 1] ?? last;
    }
    // 44100 inputs: outputs k with floor(k * 44100/16000) <= 44099, that is k <= 15999.
    expect(produced).toBe(16_000);
    expect(last).toBe(-12_000);
  });

  test("a context at or below the guest rate is not filtered", () => {
    const input = Int16Array.from([100, -200, 300, -400, 500, -600, 700]);
    const output = new Int16Array(8);
    const produced = new Downsampler(16).process(input, output, 16_000, 16_000);
    expect(Array.from(output.subarray(0, produced))).toEqual([0, 0, 100, -200, 300, -400, 500]);
  });

  test("float mic samples map to i16 symmetrically and clamp", () => {
    expect(floatToInt16(0.5)).toBe(16_384);
    expect(floatToInt16(-0.5)).toBe(-16_384);
    expect(floatToInt16(1)).toBe(32_767);
    expect(floatToInt16(-1.5)).toBe(-32_768);
    expect(floatToInt16(Number.NaN)).toBe(0);
  });
});

describe("the drift controller", () => {
  test("asks for no correction while the fill is on target", () => {
    expect(new DriftController(960).update(960)).toBe(0);
  });

  test("slows the ratio down when the ring drains and speeds it up when it fills", () => {
    const full = new DriftController(960);
    const empty = new DriftController(960);
    for (let quantum = 0; quantum < 2000; quantum += 1) {
      full.update(1920);
      empty.update(0);
    }
    expect(full.correctionPpm).toBeGreaterThan(0);
    expect(empty.correctionPpm).toBeLessThan(0);
    expect(Number.isInteger(full.correctionPpm)).toBe(true);
  });

  // A proportional term alone settles off target; the integral term removes the offset.
  test("keeps correcting a steady offset that a proportional term alone would settle on", () => {
    const controller = new DriftController(10_000);
    // 0.2 % over target: proportional alone would settle at 0.5 * 0.002 = 1000 ppm.
    for (let quantum = 0; quantum < 20_000; quantum += 1) {
      controller.update(10_020);
    }
    expect(controller.correctionPpm).toBeGreaterThan(1_500);
    expect(controller.correctionPpm).toBeLessThanOrEqual(MAX_DRIFT_PPM);
  });

  test("never asks for more than half a percent, which is inaudible for speech", () => {
    const controller = new DriftController(960);
    for (let quantum = 0; quantum < 10_000; quantum += 1) {
      controller.update(1_000_000);
    }
    expect(controller.correctionPpm).toBe(MAX_DRIFT_PPM);
  });
});
