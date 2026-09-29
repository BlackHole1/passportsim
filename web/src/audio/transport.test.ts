import { describe, expect, test } from "bun:test";

import {
  PortSource,
  PortTransport,
  SharedRingTransport,
  TRANSPORT_HEADER_BYTES,
  allocateSharedRing,
  type PortLike,
} from "./transport";

describe("the shared ring", () => {
  test("moves samples from producer to consumer in order", () => {
    const ring = SharedRingTransport.create(16);
    expect(ring.push(Int16Array.from([1, 2, 3]))).toBe(3);
    expect(ring.bufferedSamples()).toBe(3);

    const out = new Int16Array(8);
    expect(ring.pull(out)).toBe(3);
    expect(Array.from(out.slice(0, 3))).toEqual([1, 2, 3]);
    expect(ring.consumedSamples()).toBe(3n);
    expect(ring.bufferedSamples()).toBe(0);
  });

  test("wraps without losing order once more than a capacity has passed through", () => {
    const ring = SharedRingTransport.create(4);
    const out = new Int16Array(4);
    const seen: number[] = [];
    for (let round = 0; round < 5; round += 1) {
      ring.push(Int16Array.from([round * 3 + 1, round * 3 + 2, round * 3 + 3]));
      const taken = ring.pull(out);
      seen.push(...Array.from(out.slice(0, taken)));
    }
    expect(seen).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    expect(ring.consumedSamples()).toBe(15n);
  });

  test("accepts only what fits and reports the rest as refused", () => {
    const ring = SharedRingTransport.create(4);
    expect(ring.push(Int16Array.from([1, 2, 3, 4, 5, 6]))).toBe(4);
    expect(ring.freeSamples()).toBe(0);
    expect(ring.push(Int16Array.from([7]))).toBe(0);
  });

  test("a consumer that asks for more than is buffered gets what there is", () => {
    const ring = SharedRingTransport.create(8);
    ring.push(Int16Array.from([9]));
    const out = new Int16Array(8);
    expect(ring.pull(out)).toBe(1);
    expect(ring.pull(out)).toBe(0);
  });

  test("maps absolute counters past 2^40 onto the right slots, wrapping inside one call", () => {
    const buffer = allocateSharedRing(5);
    const counters = new BigInt64Array(buffer, 0, 2);
    // Both counters at 2^40 + 3; 2^40 = 1 (mod 5), so slot 4 of 5 and a push of 4 wraps at once.
    counters[0] = (1n << 40n) + 3n;
    counters[1] = (1n << 40n) + 3n;
    const ring = new SharedRingTransport(buffer, 5);
    expect(ring.push(Int16Array.from([10, 11, 12, 13]))).toBe(4);
    expect(Array.from(new Int16Array(buffer, TRANSPORT_HEADER_BYTES, 5))).toEqual([11, 12, 13, 0, 10]);
    const out = new Int16Array(4);
    expect(ring.pull(out)).toBe(4);
    expect(Array.from(out)).toEqual([10, 11, 12, 13]);
    expect(ring.consumedSamples()).toBe((1n << 40n) + 7n);
  });

  test("sizes its buffer from the capacity and the two counters", () => {
    const buffer = allocateSharedRing(100);
    expect(buffer.byteLength).toBe(TRANSPORT_HEADER_BYTES + 200);
  });
});

describe("the MessagePort fallback", () => {
  class RecordingPort implements PortLike {
    readonly posted: Int16Array[] = [];
    private handler: ((data: unknown) => void) | null = null;
    closed = false;

    postMessage(message: unknown): void {
      const chunk = (message as { pcm?: Int16Array }).pcm;
      if (chunk) {
        this.posted.push(chunk);
      }
    }

    onData(handler: ((data: unknown) => void) | null): void {
      this.handler = handler;
    }

    close(): void {
      this.closed = true;
    }

    reportConsumed(total: bigint): void {
      this.emit({ consumed: total.toString() });
    }

    emit(data: unknown): void {
      this.handler?.(data);
    }
  }

  test("transfers each chunk and counts it as produced", () => {
    const port = new RecordingPort();
    const transport = new PortTransport(port, 16);
    expect(transport.push(Int16Array.from([1, 2]))).toBe(2);
    expect(port.posted).toHaveLength(1);
    expect(Array.from(port.posted[0] ?? [])).toEqual([1, 2]);
    expect(transport.bufferedSamples()).toBe(2);
  });

  test("frees space again when the worklet reports what it consumed", () => {
    const port = new RecordingPort();
    const transport = new PortTransport(port, 4);
    transport.push(Int16Array.from([1, 2, 3, 4]));
    expect(transport.freeSamples()).toBe(0);

    port.reportConsumed(3n);
    expect(transport.consumedSamples()).toBe(3n);
    expect(transport.freeSamples()).toBe(3);
    expect(transport.push(Int16Array.from([5, 6, 7, 8]))).toBe(3);
  });

  test("drops the handler and closes the port when it is closed", () => {
    const port = new RecordingPort();
    const transport = new PortTransport(port, 4);
    transport.close();
    port.reportConsumed(9n);
    expect(transport.consumedSamples()).toBe(0n);
    expect(port.closed).toBe(true);
  });

  test("ignores a message that is not a consumed count", () => {
    const port = new RecordingPort();
    const transport = new PortTransport(port, 4);
    port.reportConsumed(2n);
    port.onData(null);
    expect(transport.consumedSamples()).toBe(2n);
  });

  test("hands a message that is not a consumed count to the control callback", () => {
    const port = new RecordingPort();
    const seen: unknown[] = [];
    new PortTransport(port, 4, (data) => seen.push(data));
    port.emit({ guestRate: 24_000 });
    expect(seen).toEqual([{ guestRate: 24_000 }]);
  });
});

/** The two halves wired to each other synchronously; a real `MessagePort` pair is one turn later. */
function loopback(): [PortLike, PortLike] {
  let left: ((data: unknown) => void) | null = null;
  let right: ((data: unknown) => void) | null = null;
  return [
    {
      postMessage: (message) => right?.(message),
      onData: (handler) => {
        left = handler;
      },
      close: () => {
        left = null;
      },
    },
    {
      postMessage: (message) => left?.(message),
      onData: (handler) => {
        right = handler;
      },
      close: () => {
        right = null;
      },
    },
  ];
}

describe("the consuming half of the fallback", () => {
  test("hands out the chunks in order and tells the producer what it took", () => {
    const [producerPort, consumerPort] = loopback();
    const source = new PortSource(consumerPort);
    const producer = new PortTransport(producerPort, 8);

    expect(producer.push(Int16Array.from([1, 2, 3]))).toBe(3);
    expect(producer.push(Int16Array.from([4, 5]))).toBe(2);
    expect(source.bufferedSamples()).toBe(5);
    expect(producer.freeSamples()).toBe(3);

    const out = new Int16Array(4);
    expect(source.pull(out)).toBe(4);
    expect(Array.from(out)).toEqual([1, 2, 3, 4]);
    // The `{ consumed }` reply travelled back, so the producer has room again.
    expect(producer.consumedSamples()).toBe(4n);
    expect(producer.freeSamples()).toBe(7);

    expect(source.pull(out)).toBe(1);
    expect(out[0]).toBe(5);
    expect(source.pull(out)).toBe(0);
  });

  test("keeps the place inside a chunk that one pull did not finish", () => {
    const [producerPort, consumerPort] = loopback();
    const source = new PortSource(consumerPort);
    new PortTransport(producerPort, 16).push(Int16Array.from([1, 2, 3, 4]));

    const out = new Int16Array(3);
    expect(source.pull(out)).toBe(3);
    expect(Array.from(out)).toEqual([1, 2, 3]);
    expect(source.pull(out)).toBe(1);
    expect(out[0]).toBe(4);
    expect(source.consumedSamples()).toBe(4n);
  });

  test("hands anything that is not PCM to the control callback", () => {
    const [producerPort, consumerPort] = loopback();
    const seen: unknown[] = [];
    new PortSource(consumerPort, (data) => seen.push(data));
    producerPort.postMessage({ guestRate: 24_000 });
    expect(seen).toEqual([{ guestRate: 24_000 }]);
  });

  test("drops what it holds when it is closed", () => {
    const [producerPort, consumerPort] = loopback();
    const source = new PortSource(consumerPort);
    new PortTransport(producerPort, 16).push(Int16Array.from([1, 2]));
    source.close();
    expect(source.bufferedSamples()).toBe(0);
    expect(source.pull(new Int16Array(4))).toBe(0);
  });
});

describe("both transports behave as one ring with absolute cursors", () => {
  function pairs(capacity: number) {
    const shared = SharedRingTransport.create(capacity);
    const [producerPort, consumerPort] = loopback();
    return [
      { name: "shared ring", producer: shared, consumer: shared },
      {
        name: "transferred port",
        producer: new PortTransport(producerPort, capacity),
        consumer: new PortSource(consumerPort),
      },
    ];
  }

  test("many wraps, partial pulls and skips give the same samples and the same cursors", () => {
    for (const { name, producer, consumer } of pairs(7)) {
      const seen: number[] = [];
      let next = 0;
      const out = new Int16Array(5);
      for (let round = 0; round < 40; round += 1) {
        const chunk = Int16Array.from({ length: 1 + (round % 6) }, () => next++);
        const accepted = producer.push(chunk);
        // What did not fit is dropped by the producer, so the numbering skips it too.
        next -= chunk.length - accepted;
        if (round % 5 === 4) {
          consumer.skip(2);
        }
        const taken = consumer.pull(out.subarray(0, 1 + (round % 5)));
        seen.push(...Array.from(out.subarray(0, taken)));
      }
      const produced = producer.producedSamples();
      expect(produced, name).toBe(BigInt(next));
      expect(consumer.consumedSamples(), name).toBe(producer.consumedSamples());
      expect(BigInt(consumer.bufferedSamples()) + consumer.consumedSamples(), name).toBe(produced);
      // Every sample pulled is in order and none repeats; skips leave gaps of exactly two.
      for (let index = 1; index < seen.length; index += 1) {
        expect((seen[index] ?? 0) > (seen[index - 1] ?? 0), name).toBe(true);
      }
      // The shared ring wrapped many times over its 7 slots.
      expect(produced > 7n * 10n, name).toBe(true);
    }
  });

  test("the scripts agree sample for sample between the two transports", () => {
    const results = pairs(9).map(({ producer, consumer }) => {
      const seen: number[] = [];
      const out = new Int16Array(4);
      for (let round = 0; round < 30; round += 1) {
        producer.push(Int16Array.from({ length: round % 5 }, (_, i) => round * 10 + i));
        if (round % 7 === 6) {
          consumer.skip(3);
        }
        const taken = consumer.pull(out);
        seen.push(...Array.from(out.subarray(0, taken)));
      }
      return { seen, consumed: consumer.consumedSamples(), produced: producer.producedSamples() };
    });
    expect(results[0]?.seen.length ?? 0).toBeGreaterThan(20);
    expect(results[1]).toEqual(results[0]);
  });

  test("a skip never passes what was produced", () => {
    for (const { name, producer, consumer } of pairs(8)) {
      producer.push(Int16Array.from([1, 2, 3]));
      expect(consumer.skip(10), name).toBe(3);
      expect(consumer.consumedSamples(), name).toBe(3n);
      expect(consumer.pull(new Int16Array(2)), name).toBe(0);
    }
  });
});
