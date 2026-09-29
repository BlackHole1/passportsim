// Both processors are driven over both transports: a page served without COOP and COEP gets no
// SharedArrayBuffer and must play and capture over the MessagePort.

import { describe, expect, test } from "bun:test";

import {
  CAPTURE_PROCESSOR,
  PLAYBACK_PROCESSOR,
  RENDER_QUANTUM,
  CaptureEngine,
  PlaybackEngine,
  captureLinkFor,
  formatChangeOf,
  playbackLinkFor,
  registerProcessors,
  type FormatChange,
} from "./worklet";
import {
  PortSource,
  PortTransport,
  SharedRingTransport,
  allocateSharedRing,
  portLike,
  type PcmSink,
  type PcmSource,
} from "./transport";

function tick(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

class FakePort {
  onmessage: ((event: MessageEvent) => void) | null = null;

  deliver(data: unknown, ports: MessagePort[] = []): void {
    this.onmessage?.({ data, ports } as unknown as MessageEvent);
  }
}

class FakeProcessorBase {
  readonly port = new FakePort();
}

interface Processor {
  readonly port: FakePort;
  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean;
}

function registered(sampleRate = 48_000): Map<string, new () => Processor> {
  const found = new Map<string, new () => Processor>();
  const scope = globalThis as unknown as Record<string, unknown>;
  scope.registerProcessor = (name: string, ctor: unknown) => {
    found.set(name, ctor as new () => Processor);
  };
  scope.AudioWorkletProcessor = FakeProcessorBase;
  scope.sampleRate = sampleRate;
  try {
    registerProcessors();
  } finally {
    delete scope.registerProcessor;
    delete scope.AudioWorkletProcessor;
    delete scope.sampleRate;
  }
  return found;
}

describe("what the module registers", () => {
  test("registers both the playback and the capture processor", () => {
    expect([...registered().keys()].sort()).toEqual([CAPTURE_PROCESSOR, PLAYBACK_PROCESSOR]);
  });

  test("outputs silence until it has been given a transport, instead of throwing", () => {
    const node = new (registered().get(PLAYBACK_PROCESSOR) as new () => Processor)();
    const block = new Float32Array(RENDER_QUANTUM).fill(0.5);
    expect(node.process([], [[block]])).toBe(true);
    expect(block.every((sample) => sample === 0)).toBe(true);
  });
});

describe("playback over the shared ring", () => {
  test("renders what the pump pushed", () => {
    const sab = allocateSharedRing(4096);
    const producer = new SharedRingTransport(sab, 4096);
    producer.push(new Int16Array(2048).fill(8_000));

    const node = new (registered().get(PLAYBACK_PROCESSOR) as new () => Processor)();
    node.port.deliver({ sab, guestRate: 16_000 });
    const block = new Float32Array(RENDER_QUANTUM);
    node.process([], [[block]]);

    expect(block.some((sample) => sample !== 0)).toBe(true);
    expect(producer.consumedSamples()).toBeGreaterThan(0n);
  });
});

describe("playback over the transferred port", () => {
  test("renders what the worker posted and reports back what it consumed", async () => {
    const node = new (registered().get(PLAYBACK_PROCESSOR) as new () => Processor)();
    const channel = new MessageChannel();
    node.port.deliver({ guestRate: 16_000 }, [channel.port2]);

    const producer = new PortTransport(portLike(channel.port1), 8192);
    expect(producer.push(new Int16Array(2048).fill(8_000))).toBe(2048);
    await tick();

    const block = new Float32Array(RENDER_QUANTUM);
    node.process([], [[block]]);
    expect(block.some((sample) => sample !== 0)).toBe(true);

    await tick();
    expect(producer.consumedSamples()).toBeGreaterThan(0n);
    expect(producer.freeSamples()).toBeGreaterThan(8192 - 2048);
    channel.port1.close();
    channel.port2.close();
  });
});

describe("capture", () => {
  test("writes the mic block into the shared ring", () => {
    const sab = allocateSharedRing(4096);
    const node = new (registered().get(CAPTURE_PROCESSOR) as new () => Processor)();
    node.port.deliver({ sab, guestRate: 16_000 });

    node.process([[new Float32Array(RENDER_QUANTUM).fill(0.5)]], []);

    const consumer = new SharedRingTransport(sab, 4096);
    expect(consumer.bufferedSamples()).toBeGreaterThan(0);
  });

  test("posts the mic block over the transferred port when there is no shared ring", async () => {
    const node = new (registered().get(CAPTURE_PROCESSOR) as new () => Processor)();
    const channel = new MessageChannel();
    node.port.deliver({ guestRate: 16_000 }, [channel.port2]);

    const received: Int16Array[] = [];
    channel.port1.onmessage = (event: MessageEvent) => {
      const chunk = (event.data as { pcm?: Int16Array }).pcm;
      if (chunk) {
        received.push(chunk);
      }
    };
    channel.port1.start();

    node.process([[new Float32Array(RENDER_QUANTUM).fill(0.5)]], []);
    await tick();

    expect(received).toHaveLength(1);
    expect(received[0]?.length).toBeGreaterThan(0);
    channel.port1.close();
    channel.port2.close();
  });

  test("drives nothing and throws nothing before it has a transport", () => {
    const node = new (registered().get(CAPTURE_PROCESSOR) as new () => Processor)();
    expect(node.process([[new Float32Array(RENDER_QUANTUM)]], [])).toBe(true);
  });
});

describe("the guest rate", () => {
  test("control messages reach the playback worklet over the port on the shared-ring path too", async () => {
    const channel = new MessageChannel();
    const seen: unknown[] = [];
    const source: { end: PcmSource } | null = playbackLinkFor(
      { sab: allocateSharedRing(64) },
      channel.port2,
      (data) => seen.push(data),
    );
    expect(source).not.toBeNull();

    channel.port1.postMessage({ audioDriven: true });
    await tick();
    expect(seen).toEqual([{ audioDriven: true }]);
    channel.port1.close();
    channel.port2.close();
  });

  // A format posted beside the ring after its samples could be read after them, and they would play
  // at the old rate.
  for (const transport of ["shared ring", "port"] as const) {
    test(`a format mark travels ahead of its samples over the ${transport}`, () => {
      const order: string[] = [];
      let engineSource: PcmSource;
      let producer: { pushFormat: (mark: never) => boolean; push: (s: Int16Array) => number };
      if (transport === "shared ring") {
        const shared = SharedRingTransport.create(4096);
        engineSource = shared;
        producer = shared as never;
      } else {
        let consumer: PortSource | null = null;
        const toWorklet = {
          postMessage: (message: unknown) => {
            order.push(Object.keys(message as object)[0] ?? "");
            consumer?.accept(message);
          },
          onData: () => {},
        };
        consumer = new PortSource({ postMessage() {}, onData() {} });
        engineSource = consumer;
        producer = new PortTransport(toWorklet, 4096) as never;
      }
      const playback = new PlaybackEngine(engineSource, 48_000, { guestRate: 16_000 });
      producer.pushFormat({ guestRate: 24_000, channels: 1, at: 0n } as never);
      producer.push(new Int16Array(1440).fill(1000));
      if (transport === "port") {
        expect(order).toEqual(["format", "pcm"]);
      }
      playback.render(new Float32Array(RENDER_QUANTUM));
      expect(playback.format).toEqual({ guestRate: 24_000, channels: 1 });
      // 128 outputs at 24 kHz into 48 kHz: 1 + floor(127 / 2) inputs.
      expect(engineSource.consumedSamples()).toBe(64n);
    });
  }

  test("reaches the capture worklet the same way", async () => {
    const channel = new MessageChannel();
    const rates: number[] = [];
    const sink: { end: PcmSink } | null = captureLinkFor({}, channel.port2, (change) =>
      rates.push(change.guestRate),
    );
    expect(sink).not.toBeNull();

    channel.port1.postMessage({ guestRate: 24_000 });
    await tick();
    expect(rates).toEqual([24_000]);
    channel.port1.close();
    channel.port2.close();
  });

  test("is not built into a transport when the page gave neither a ring nor a port", () => {
    expect(playbackLinkFor({}, undefined, () => {})).toBeNull();
    expect(captureLinkFor({}, undefined, () => {})).toBeNull();
  });
});

describe("the playback engine", () => {
  function engine(capacity = 16_000, guestRate = 16_000, contextRate = 48_000) {
    const ring = SharedRingTransport.create(capacity);
    return { ring, playback: new PlaybackEngine(ring, contextRate, { guestRate }) };
  }

  test("takes exactly the samples it interpolates, so consumed counts what was played", () => {
    const { ring, playback } = engine();
    // 60 ms at 16 kHz: on the drift target, so the drift stays far too small to move a position.
    ring.push(new Int16Array(960).fill(1000));
    for (let quantum = 0; quantum < 3; quantum += 1) {
      expect(playback.render(new Float32Array(RENDER_QUANTUM))).toBe(true);
    }
    // 384 outputs at a third of a sample each: positions 0 .. 383/3, so x[0..127].
    expect(ring.consumedSamples()).toBe(128n);
  });

  /**
   * Plays `samples` while the producer is still live, a few samples before every quantum, so the
   * fill falls without production stopping. Returns at the first quantum silent for want of samples.
   */
  function drainWhileProducing(ring: SharedRingTransport, playback: PlaybackEngine, samples: number): void {
    ring.push(new Int16Array(samples).fill(1000));
    for (let quantum = 0; quantum < 200; quantum += 1) {
      ring.push(new Int16Array(10).fill(1000));
      if (!playback.render(new Float32Array(RENDER_QUANTUM))) {
        return;
      }
    }
    throw new Error("the ring never ran below the floor");
  }

  test("outputs silence below the 10 ms floor and counts one underrun per dropout", () => {
    const { ring, playback } = engine();
    // 1000 samples, above the 960 of the 60 ms start fill at 16 kHz.
    drainWhileProducing(ring, playback, 1000);
    const left = ring.bufferedSamples();
    // Under the 160 of 10 ms at 16 kHz, and the quantum that found it so was silent.
    expect(left).toBeLessThan(160);
    expect(playback.counters()).toMatchObject({ underruns: 1, starvedQuanta: 1 });
    const block = new Float32Array(RENDER_QUANTUM).fill(1);
    expect(playback.render(block)).toBe(false);
    expect(block.every((sample) => sample === 0)).toBe(true);
    expect(playback.counters()).toMatchObject({ underruns: 1, starvedQuanta: 2 });
    expect(ring.bufferedSamples()).toBe(left);
  });

  test("plays the tail under the floor once production has stopped, and not before", () => {
    const { ring, playback } = engine();
    drainWhileProducing(ring, playback, 1000);
    const left = ring.bufferedSamples();
    // Under the floor: the next quanta wait while production might resume.
    playback.render(new Float32Array(RENDER_QUANTUM));
    expect(ring.bufferedSamples()).toBe(left);
    // 32 ms at 48 kHz is 12 quanta of 128 without a new sample; then the tail plays out.
    for (let quantum = 0; quantum < 16; quantum += 1) {
      playback.render(new Float32Array(RENDER_QUANTUM));
    }
    expect(ring.bufferedSamples()).toBe(0);
    expect(playback.counters().underruns).toBe(1);
  });

  test("a sound shorter than the start fill plays whole once production stops", () => {
    const { ring, playback } = engine();
    ring.push(new Int16Array(200).fill(1000));
    // Held while production might still continue: nothing plays below the start fill.
    for (let quantum = 0; quantum < 11; quantum += 1) {
      expect(playback.render(new Float32Array(RENDER_QUANTUM))).toBe(false);
    }
    expect(ring.bufferedSamples()).toBe(200);
    for (let quantum = 0; quantum < 12; quantum += 1) {
      playback.render(new Float32Array(RENDER_QUANTUM));
    }
    expect(ring.bufferedSamples()).toBe(0);
    expect(playback.counters().underruns).toBe(1);
  });

  // Under `Wall` pacing the guest's I2S TX lands one 15 ms period per slice. A stream started at the
  // 10 ms floor had 12 to 18 ms in hand, and one 24 ms producer stall counted an underrun in a body
  // that plays nothing. The stall is placed at every phase of the period.
  test("starts a stream at the 60 ms drift target, so a 24 ms producer stall is no dropout", () => {
    const PERIOD = 240; // 15 ms of 16 kHz mono
    const quantumMs = (RENDER_QUANTUM * 1000) / 48_000;
    for (let phase = 0; phase < 15; phase += 1) {
      const { ring, playback } = engine();
      const stallFrom = 300 + phase;
      const stallTo = stallFrom + 24;
      let due = 0;
      let firstSound: number | null = null;
      for (let quantum = 0; quantum < 300; quantum += 1) {
        const nowMs = quantum * quantumMs;
        // Periods fall due on time except during the stall, after which they arrive together, as a late
        // `Wall` slice catches up.
        while (due * 15 <= nowMs && !(nowMs >= stallFrom && nowMs < stallTo)) {
          ring.push(new Int16Array(PERIOD).fill(1000));
          due += 1;
        }
        const buffered = ring.bufferedSamples();
        if (playback.render(new Float32Array(RENDER_QUANTUM)) && firstSound === null) {
          firstSound = quantum;
          expect(buffered, "nothing is played before 60 ms of the stream is buffered").toBeGreaterThanOrEqual(960);
        }
      }
      expect(firstSound).not.toBeNull();
      expect(playback.counters().underruns, `a 24 ms stall at ${stallFrom} ms`).toBe(0);
    }
  });

  test("never counts an underrun before anything has played", () => {
    const { playback } = engine();
    for (let quantum = 0; quantum < 10; quantum += 1) {
      playback.render(new Float32Array(RENDER_QUANTUM));
    }
    expect(playback.counters()).toMatchObject({ underruns: 0, starvedQuanta: 10 });
  });

  test("drops to the 60 ms target above 250 ms of fill and counts the overflow", () => {
    const { ring, playback } = engine();
    ring.push(new Int16Array(4800).fill(1000));
    playback.render(new Float32Array(RENDER_QUANTUM));
    // 4800 - 960 = 3840 dropped, then 43 played.
    expect(playback.counters()).toMatchObject({ overflows: 1, overflowSamples: 3840 });
    expect(ring.consumedSamples()).toBe(3840n + 43n);
  });

  test("switches rate at the sample the guest switched at, inside one quantum", () => {
    const { ring, playback } = engine();
    ring.push(new Int16Array(1000).fill(1000));
    playback.setFormat({ guestRate: 24_000, channels: 1, at: 20n });
    playback.render(new Float32Array(RENDER_QUANTUM));
    // 20 samples at 16 kHz give 60 outputs; the other 68 at 24 kHz take 1 + floor(67/2) = 34.
    expect(ring.consumedSamples()).toBe(54n);
    expect(playback.format).toEqual({ guestRate: 24_000, channels: 1 });
  });

  test("a format change off a frame boundary skips the stray sample instead of stalling", () => {
    const source = new PortSource({ postMessage() {}, onData() {} });
    const playback = new PlaybackEngine(source, 48_000, { guestRate: 24_000, channels: 2 });
    source.accept({ pcm: new Int16Array(24_001).fill(1000) });
    playback.setFormat({ guestRate: 16_000, channels: 1, at: 12_001n });
    source.accept({ pcm: new Int16Array(16_000).fill(500) });
    let silent = 0;
    for (let quantum = 0; quantum < 2000; quantum += 1) {
      if (!playback.render(new Float32Array(RENDER_QUANTUM))) {
        silent += 1;
      }
    }
    // 6000 stereo frames, one stray sample, then the mono run; the tail below 10 ms stays.
    expect(playback.format).toEqual({ guestRate: 16_000, channels: 1 });
    expect(playback.counters().straySamples).toBe(1);
    expect(source.consumedSamples()).toBeGreaterThan(12_001n + 16_000n - 12_000n);
    expect(silent).toBeLessThan(2000);
  });

  test("holds the ratio exact while Audio pacing drives, and corrects drift again after", () => {
    const { ring, playback } = engine(64_000);
    // 200 ms at 16 kHz, far over the 60 ms target: the controller would read faster.
    ring.push(new Int16Array(3200).fill(1000));
    playback.setAudioDriven(true);
    for (let quantum = 0; quantum < 30; quantum += 1) {
      playback.render(new Float32Array(RENDER_QUANTUM));
    }
    expect(playback.driftPpm).toBe(0);
    // 3840 outputs at exactly a third of a sample each: 1 + floor(3839 / 3).
    expect(ring.consumedSamples()).toBe(1280n);
    playback.setAudioDriven(false);
    for (let quantum = 0; quantum < 30; quantum += 1) {
      playback.render(new Float32Array(RENDER_QUANTUM));
    }
    expect(playback.driftPpm).toBeGreaterThan(0);
  });

  test("the processor takes the audio-driven flag off its control port", async () => {
    const node = new (registered().get(PLAYBACK_PROCESSOR) as new () => Processor)();
    const channel = new MessageChannel();
    const sab = allocateSharedRing(8192);
    node.port.deliver({ sab, guestRate: 16_000 }, [channel.port2]);
    const producer = new SharedRingTransport(sab, 8192);
    producer.push(new Int16Array(3200).fill(1000));
    channel.port1.postMessage({ audioDriven: true });
    await tick();
    for (let quantum = 0; quantum < 30; quantum += 1) {
      node.process([], [[new Float32Array(RENDER_QUANTUM)]]);
    }
    expect(producer.consumedSamples()).toBe(1280n);
    channel.port1.close();
    channel.port2.close();
  });

  test("plays the left slot of a stereo frame and consumes both slots", () => {
    const { ring, playback } = engine(16_000, 48_000, 48_000);
    playback.setFormat({ guestRate: 48_000, channels: 2, at: null });
    // 60 ms of stereo at 48 kHz: the fill is on the drift target, so the ratio is exactly one.
    const frames = new Int16Array(5760);
    for (let frame = 0; frame < 2880; frame += 1) {
      frames[frame * 2] = 16_384;
      frames[frame * 2 + 1] = -16_384;
    }
    ring.push(frames);
    const block = new Float32Array(RENDER_QUANTUM);
    playback.render(block);
    expect(block[RENDER_QUANTUM - 1]).toBe(0.5);
    expect(ring.consumedSamples()).toBe(BigInt(2 * RENDER_QUANTUM));
  });

  test("reports its counters to the worker over the transferred port", async () => {
    const node = new (registered().get(PLAYBACK_PROCESSOR) as new () => Processor)();
    const channel = new MessageChannel();
    const reports: unknown[] = [];
    channel.port1.onmessage = (event: MessageEvent) => {
      if ((event.data as { playback?: unknown }).playback) {
        reports.push(event.data);
      }
    };
    channel.port1.start();
    node.port.deliver({ sab: allocateSharedRing(1024), guestRate: 16_000 }, [channel.port2]);
    for (let quantum = 0; quantum < 64; quantum += 1) {
      node.process([], [[new Float32Array(RENDER_QUANTUM)]]);
    }
    await tick();
    expect(reports).toEqual([
      {
        playback: {
          quanta: 64,
          underruns: 0,
          starvedQuanta: 64,
          overflows: 0,
          overflowSamples: 0,
          straySamples: 0,
        },
      },
    ]);
    channel.port1.close();
    channel.port2.close();
  });
});

describe("decoding a format message", () => {
  test("accepts the rates the resampler accepts and nothing past 384 kHz", () => {
    expect(formatChangeOf({ guestRate: 384_000 })).toEqual({ guestRate: 384_000, channels: 1, at: null });
    expect(formatChangeOf({ guestRate: 384_001 })).toBeNull();
    expect(formatChangeOf({ guestRate: 16_000.5 })).toBeNull();
    expect(formatChangeOf({ guestRate: 0 })).toBeNull();
    expect(formatChangeOf({ guestRate: 24_000, channels: 2, at: "12" })).toEqual({
      guestRate: 24_000,
      channels: 2,
      at: 12n,
    });
  });

  test("a mark in the ring with an impossible rate is ignored by the engine", () => {
    const ring = SharedRingTransport.create(4096);
    const playback = new PlaybackEngine(ring, 48_000, { guestRate: 16_000 });
    ring.pushFormat({ guestRate: 500_000, channels: 1, at: 0n });
    ring.push(new Int16Array(960));
    playback.render(new Float32Array(RENDER_QUANTUM));
    expect(playback.format).toEqual({ guestRate: 16_000, channels: 1 });
  });
});

describe("the capture engine", () => {
  test("48 kHz mic blocks become 16 kHz samples at the same level, counted", () => {
    const ring = SharedRingTransport.create(4096);
    const capture = new CaptureEngine(ring, 48_000, 16_000);
    // Positions 0, 3, .., 126 of 128 inputs: 43 outputs.
    expect(capture.capture(new Float32Array(RENDER_QUANTUM).fill(0.5))).toBe(43);
    expect(capture.capture(new Float32Array(RENDER_QUANTUM).fill(0.5))).toBe(43);
    const out = new Int16Array(86);
    ring.pull(out);
    // The first output reads the zeros before the stream; the filter delays the step by 23 inputs and
    // rings for a few more, so from output 16 on the level is the input's.
    expect(out[0]).toBe(0);
    expect(out.subarray(16).every((sample) => sample === 16_384)).toBe(true);
    expect(capture.counters()).toEqual({ written: 86, dropped: 0, unratedBlocks: 0 });
  });

  test("writes nothing until the guest rate is known, then converts at that rate", () => {
    const ring = SharedRingTransport.create(4096);
    const capture = new CaptureEngine(ring, 48_000, null);
    expect(capture.capture(new Float32Array(RENDER_QUANTUM).fill(0.5))).toBe(0);
    expect(ring.bufferedSamples()).toBe(0);
    capture.setGuestRate(24_000);
    // 128 inputs at half a sample per output: 64 outputs.
    expect(capture.capture(new Float32Array(RENDER_QUANTUM).fill(0.5))).toBe(64);
    expect(capture.counters()).toEqual({ written: 64, dropped: 0, unratedBlocks: 1 });
  });

  test("counts what a full ring refused instead of blocking the audio thread", () => {
    const ring = SharedRingTransport.create(50);
    const capture = new CaptureEngine(ring, 48_000, 16_000);
    capture.capture(new Float32Array(RENDER_QUANTUM));
    capture.capture(new Float32Array(RENDER_QUANTUM));
    expect(capture.counters()).toEqual({ written: 50, dropped: 36, unratedBlocks: 0 });
  });
});
