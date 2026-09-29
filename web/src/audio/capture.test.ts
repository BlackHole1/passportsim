import { describe, expect, test } from "bun:test";

import { InputStamper } from "../worker/input";
import { InputKind } from "../worker/layout";
import {
  CHUNK_FRAMES,
  MAX_CAPTURE_SAMPLES,
  MIC_BACKLOG_FRAMES,
  MIC_KEEP_FRAMES,
  MicCapture,
  MIC_CONSTRAINTS,
} from "./capture";
import { CaptureEngine } from "./worklet";
import { PortSource, PortTransport, SharedRingTransport, type PortLike } from "./transport";

describe("the capture constraints", () => {
  test("ask for one channel with no browser DSP", () => {
    expect(MIC_CONSTRAINTS).toEqual({
      channelCount: 1,
      echoCancellation: false,
      noiseSuppression: false,
      autoGainControl: false,
    });
  });
});

describe("draining the capture ring", () => {
  function samplesOf(payload: Uint8Array | null | undefined): number[] {
    const bytes = payload ?? new Uint8Array();
    return Array.from(new Int16Array(bytes.buffer, bytes.byteOffset, bytes.byteLength / 2));
  }

  test("journals whole 240-frame chunks, the chunk `mic_set` journals, numbered from 0", () => {
    const ring = SharedRingTransport.create(1024);
    const stamper = new InputStamper();
    const capture = new MicCapture(ring, stamper);
    expect(CHUNK_FRAMES).toBe(240);

    ring.push(Int16Array.from({ length: 300 }, (_, n) => n));
    expect(capture.drain()).toBe(240);
    ring.push(Int16Array.from({ length: 200 }, (_, n) => 300 + n));
    expect(capture.drain()).toBe(240);
    expect(capture.stats()).toEqual({ chunks: 2, samples: 480n, buffered: 20, nextSeq: 2n, droppedFrames: 0n });

    const stamped = stamper.drain(0n) ?? [];
    expect(stamped.map((input) => input.kind)).toEqual([InputKind.MicChunk, InputKind.MicChunk]);
    expect(stamped.map((input) => input.a)).toEqual([0, 1]);
    expect(samplesOf(stamped[0]?.payload)).toEqual(Array.from({ length: 240 }, (_, n) => n));
    expect(samplesOf(stamped[1]?.payload)).toEqual(Array.from({ length: 240 }, (_, n) => 240 + n));
  });

  test("journals nothing until a chunk is full, and the short last chunk on flush", () => {
    const ring = SharedRingTransport.create(1024);
    const stamper = new InputStamper();
    const capture = new MicCapture(ring, stamper);
    ring.push(Int16Array.from([7, -7, 9]));
    expect(capture.drain()).toBe(0);
    expect(stamper.hasPending).toBe(false);
    expect(capture.flush()).toBe(3);
    const stamped = stamper.drain(0n) ?? [];
    expect(samplesOf(stamped[0]?.payload)).toEqual([7, -7, 9]);
    expect(capture.flush()).toBe(0);
  });

  test("lays a stereo chunk out as mic_set's interleave does and continues a given count", () => {
    const ring = SharedRingTransport.create(1024);
    const stamper = new InputStamper();
    const capture = new MicCapture(ring, stamper, { channels: 2, firstSeq: 5n });
    ring.push(Int16Array.from({ length: 240 }, (_, n) => n - 120));
    expect(capture.drain()).toBe(240);
    const [chunk] = stamper.drain(0n) ?? [];
    expect(chunk?.a).toBe(5);
    const samples = samplesOf(chunk?.payload);
    expect(samples).toHaveLength(480);
    expect(samples.slice(0, 6)).toEqual([-120, -120, -119, -119, -118, -118]);
  });

  test("reads the same chunks over the MessagePort fallback", () => {
    const [producerPort, consumerPort] = loopback();
    const producer = new PortTransport(producerPort, 4096);
    const stamper = new InputStamper();
    const capture = new MicCapture(new PortSource(consumerPort), stamper);
    // Worklet-sized blocks of 43 samples, the 48 kHz to 16 kHz yield of one render quantum.
    for (let block = 0; block < 12; block += 1) {
      producer.push(Int16Array.from({ length: 43 }, (_, n) => block * 43 + n));
    }
    expect(capture.drain()).toBe(480);
    // Everything was taken off the port; the 36 frames past the second chunk wait for the third.
    expect(producer.consumedSamples()).toBe(516n);
    expect(capture.stats().buffered).toBe(36);
  });

  test("moves at most a bounded number of frames per slice", () => {
    const ring = SharedRingTransport.create(16_384);
    const capture = new MicCapture(ring, new InputStamper());
    ring.push(new Int16Array(MIC_BACKLOG_FRAMES));
    expect(capture.drain()).toBe(Math.floor(MAX_CAPTURE_SAMPLES / CHUNK_FRAMES) * CHUNK_FRAMES);
    expect(ring.consumedSamples()).toBe(BigInt(MAX_CAPTURE_SAMPLES));
  });

  test("a backlog loses its oldest frames and journals the newest", () => {
    const ring = SharedRingTransport.create(16_384);
    const stamper = new InputStamper();
    const capture = new MicCapture(ring, stamper);
    ring.push(Int16Array.from({ length: 10_000 }, (_, n) => n % 30_000));
    capture.drain();
    expect(capture.stats().droppedFrames).toBe(BigInt(10_000 - MIC_KEEP_FRAMES));
    const [first] = stamper.drain(0n) ?? [];
    const bytes = first?.payload ?? new Uint8Array(2);
    expect(new Int16Array(bytes.buffer, bytes.byteOffset, 1)[0]).toBe(10_000 - MIC_KEEP_FRAMES);
  });
});

function loopback(): [PortLike, PortLike] {
  const handlers: [((data: unknown) => void) | null, ((data: unknown) => void) | null] = [null, null];
  const port = (self: 0 | 1): PortLike => ({
    postMessage: (message) => handlers[self === 0 ? 1 : 0]?.(message),
    onData: (handler) => {
      handlers[self] = handler;
    },
  });
  return [port(0), port(1)];
}

describe("the capture worklet", () => {
  test("converts the context rate down to the guest rate", () => {
    const ring = SharedRingTransport.create(4096);
    const engine = new CaptureEngine(ring, 48_000, 16_000);
    const block = new Float32Array(128).fill(0.5);

    const written = engine.capture(block);
    // 48 kHz in, 16 kHz out: about a third of the frames survive.
    expect(written).toBeGreaterThanOrEqual(41);
    expect(written).toBeLessThanOrEqual(45);
    expect(ring.bufferedSamples()).toBe(written);
  });

  test("keeps a constant level through the conversion", () => {
    const ring = SharedRingTransport.create(4096);
    const engine = new CaptureEngine(ring, 48_000, 48_000);
    for (let block = 0; block < 8; block += 1) {
      engine.capture(new Float32Array(128).fill(0.25));
    }
    const out = new Int16Array(1024);
    const taken = ring.pull(out);
    expect(out[taken - 1] ?? 0).toBeGreaterThan(8_000);
    expect(out[taken - 1] ?? 0).toBeLessThan(8_400);
  });
});
