// The PCM transport between the Worker and the AudioWorklet: a SharedArrayBuffer ring when the page
// is cross-origin isolated, else `postMessage` over a transferred MessagePort, both behind
// `PcmTransport`. The SAB form is a single-producer single-consumer ring with absolute 64-bit
// counters (the shape of `padenot/ringbuf.js`); the worklet never allocates or `Atomics.wait`s on it.

/**
 * Format marks the shared ring holds at once. The firmware changes format a handful of times per
 * session; a producer that finds the table full stops pushing until the worklet catches up.
 */
export const FORMAT_SLOTS = 16;

/**
 * Bytes before the samples: four absolute counters, then {@link FORMAT_SLOTS} 16-byte marks, all
 * 8-byte aligned.
 */
export const TRANSPORT_HEADER_BYTES = 32 + FORMAT_SLOTS * 16;
const CONSUMED_SLOT = 0;
const PRODUCED_SLOT = 1;
const FORMATS_PRODUCED_SLOT = 2;
const FORMATS_CONSUMED_SLOT = 3;

/**
 * A guest format change in the stream: from transport cursor `at` on, samples are `channels`-slot
 * frames at `guestRate` Hz. It travels in the transport ahead of its samples, not on a side
 * channel, which could arrive after the worklet read them.
 */
export interface PcmFormatMark {
  readonly guestRate: number;
  readonly channels: number;
  readonly at: bigint;
}

/** The producing end: what the PCM pump and the capture worklet write into. */
export interface PcmSink {
  /** Pushes interleaved `i16` samples; returns how many were accepted. */
  push(samples: Int16Array): number;
}

/** The consuming end. Neither implementation allocates in `pull`: the playback one runs on the audio thread. */
export interface PcmSource {
  pull(out: Int16Array): number;
  bufferedSamples(): number;
  /** Samples pulled or skipped since the source was created, absolute. */
  consumedSamples(): bigint;
  /**
   * Discards at most `count` of the oldest samples without copying; returns how many. They count as
   * consumed, so the `Audio` anchor moves past them.
   */
  skip(count: number): number;
  /**
   * The oldest format mark not yet taken, or `null`. A consumer that reads the produced cursor first
   * and then takes every mark has the format of every sample below that cursor.
   */
  takeFormat(): PcmFormatMark | null;
}

export interface PcmTransport extends PcmSink {
  /** Samples pushed since creation: the cursor the next pushed sample gets. Only the producer moves it. */
  producedSamples(): bigint;
  consumedSamples(): bigint;
  bufferedSamples(): number;
  freeSamples(): number;
  /** Places a format mark ahead of the sample at `mark.at`; `false` means neither it nor those samples may go in yet. */
  pushFormat(mark: PcmFormatMark): boolean;
  close(): void;
}

export function sabAvailable(): boolean {
  return typeof SharedArrayBuffer !== "undefined" && globalThis.crossOriginIsolated === true;
}

export function allocateSharedRing(capacity: number): SharedArrayBuffer {
  return new SharedArrayBuffer(TRANSPORT_HEADER_BYTES + capacity * 2);
}

/**
 * The cross-origin isolated transport: one SPSC ring in shared memory. The counters are absolute
 * and only grow, so full and empty never look alike and a reader that falls behind is detected.
 */
export class SharedRingTransport implements PcmTransport, PcmSource {
  private readonly counters: BigInt64Array;
  /** Mark k is `[2k] = at`, `[2k + 1] = guestRate * 256 + channels`. */
  private readonly marks: BigInt64Array;
  private readonly samples: Int16Array;

  constructor(
    private readonly buffer: SharedArrayBuffer,
    readonly capacity: number,
  ) {
    this.counters = new BigInt64Array(buffer, 0, 4);
    this.marks = new BigInt64Array(buffer, 32, FORMAT_SLOTS * 2);
    this.samples = new Int16Array(buffer, TRANSPORT_HEADER_BYTES, capacity);
  }

  static create(capacity: number): SharedRingTransport {
    return new SharedRingTransport(allocateSharedRing(capacity), capacity);
  }

  get sharedBuffer(): SharedArrayBuffer {
    return this.buffer;
  }

  push(samples: Int16Array): number {
    const produced = Atomics.load(this.counters, PRODUCED_SLOT);
    const consumed = Atomics.load(this.counters, CONSUMED_SLOT);
    const free = this.capacity - Number(produced - consumed);
    const take = Math.max(0, Math.min(free, samples.length));
    // One BigInt division per call, not per sample; the audio thread's twin of this loop allocates nothing.
    let slot = Number(produced % BigInt(this.capacity));
    for (let index = 0; index < take; index += 1) {
      this.samples[slot] = samples[index] ?? 0;
      slot = slot + 1 === this.capacity ? 0 : slot + 1;
    }
    Atomics.store(this.counters, PRODUCED_SLOT, produced + BigInt(take));
    return take;
  }

  producedSamples(): bigint {
    return Atomics.load(this.counters, PRODUCED_SLOT);
  }

  consumedSamples(): bigint {
    return Atomics.load(this.counters, CONSUMED_SLOT);
  }

  bufferedSamples(): number {
    return Number(
      Atomics.load(this.counters, PRODUCED_SLOT) - Atomics.load(this.counters, CONSUMED_SLOT),
    );
  }

  freeSamples(): number {
    return this.capacity - this.bufferedSamples();
  }

  close(): void {
  }

  /** The consumer half, for the worklet: copies samples and advances the consumed counter, allocating nothing. */
  pull(out: Int16Array): number {
    const produced = Atomics.load(this.counters, PRODUCED_SLOT);
    const consumed = Atomics.load(this.counters, CONSUMED_SLOT);
    const take = Math.min(Number(produced - consumed), out.length);
    let slot = take > 0 ? Number(consumed % BigInt(this.capacity)) : 0;
    for (let index = 0; index < take; index += 1) {
      out[index] = this.samples[slot] ?? 0;
      slot = slot + 1 === this.capacity ? 0 : slot + 1;
    }
    Atomics.store(this.counters, CONSUMED_SLOT, consumed + BigInt(take));
    return take;
  }

  pushFormat(mark: PcmFormatMark): boolean {
    const produced = Atomics.load(this.counters, FORMATS_PRODUCED_SLOT);
    const consumed = Atomics.load(this.counters, FORMATS_CONSUMED_SLOT);
    if (produced - consumed >= BigInt(FORMAT_SLOTS)) {
      return false;
    }
    const slot = Number(produced % BigInt(FORMAT_SLOTS));
    Atomics.store(this.marks, slot * 2, mark.at);
    Atomics.store(this.marks, slot * 2 + 1, BigInt(mark.guestRate) * 256n + BigInt(mark.channels));
    Atomics.store(this.counters, FORMATS_PRODUCED_SLOT, produced + 1n);
    return true;
  }

  takeFormat(): PcmFormatMark | null {
    const produced = Atomics.load(this.counters, FORMATS_PRODUCED_SLOT);
    const consumed = Atomics.load(this.counters, FORMATS_CONSUMED_SLOT);
    if (consumed >= produced) {
      return null;
    }
    const slot = Number(consumed % BigInt(FORMAT_SLOTS));
    const at = Atomics.load(this.marks, slot * 2);
    const packed = Atomics.load(this.marks, slot * 2 + 1);
    Atomics.store(this.counters, FORMATS_CONSUMED_SLOT, consumed + 1n);
    return { guestRate: Number(packed / 256n), channels: Number(packed % 256n), at };
  }

  /** Only the consumer calls this, so moving the consumed counter does not race the producer. */
  skip(count: number): number {
    const produced = Atomics.load(this.counters, PRODUCED_SLOT);
    const consumed = Atomics.load(this.counters, CONSUMED_SLOT);
    const take = Math.max(0, Math.min(Number(produced - consumed), Math.floor(count)));
    Atomics.store(this.counters, CONSUMED_SLOT, consumed + BigInt(take));
    return take;
  }
}

/**
 * The half of a `MessagePort` the transport needs. Delivery is a registration rather than an
 * `onmessage` field, which a real port's `MessageEvent` type would not satisfy; see {@link portLike}.
 */
export interface PortLike {
  postMessage(message: unknown, transfer?: Transferable[]): void;
  onData(handler: ((data: unknown) => void) | null): void;
  close?(): void;
}

export function portLike(port: MessagePort): PortLike {
  return {
    postMessage: (message, transfer) => {
      port.postMessage(message, transfer ?? []);
    },
    onData: (handler) => {
      port.onmessage = handler ? (event: MessageEvent) => handler(event.data) : null;
      port.start();
    },
    close: () => {
      port.close();
    },
  };
}

export interface ConsumedMessage {
  readonly consumed: string;
}

export interface PcmMessage {
  readonly pcm: Int16Array;
}

/**
 * The fallback transport: chunks are transferred over a MessagePort and the worklet posts back how
 * many samples it consumed. A message hop and more jitter than the shared ring, hence the fallback.
 */
export class PortTransport implements PcmTransport {
  private produced = 0n;
  private consumed = 0n;

  /**
   * @param port the transferred port.
   * @param capacity samples it may keep in flight before `push` drops.
   * @param onControl anything on the port that is not a `{ consumed }` reply.
   */
  constructor(
    private readonly port: PortLike,
    readonly capacity: number,
    private readonly onControl?: (data: unknown) => void,
  ) {
    port.onData((data) => {
      const message = data as Partial<ConsumedMessage> | null;
      if (message && typeof message.consumed === "string") {
        this.consumed = BigInt(message.consumed);
        return;
      }
      this.onControl?.(data);
    });
  }

  /** Posted before the chunk it describes; a MessagePort keeps the order. */
  pushFormat(mark: PcmFormatMark): boolean {
    this.port.postMessage({
      format: { guestRate: mark.guestRate, channels: mark.channels, at: mark.at.toString() },
    });
    return true;
  }

  push(samples: Int16Array): number {
    const take = Math.max(0, Math.min(this.freeSamples(), samples.length));
    if (take === 0) {
      return 0;
    }
    const chunk = samples.slice(0, take);
    this.port.postMessage({ pcm: chunk }, [chunk.buffer]);
    this.produced += BigInt(take);
    return take;
  }

  producedSamples(): bigint {
    return this.produced;
  }

  consumedSamples(): bigint {
    return this.consumed;
  }

  bufferedSamples(): number {
    return Number(this.produced - this.consumed);
  }

  freeSamples(): number {
    return this.capacity - this.bufferedSamples();
  }

  close(): void {
    this.port.onData(null);
    this.port.close?.();
  }
}

/**
 * The consuming half of {@link PortTransport}: chunks arrive here, `pull` hands them out in order,
 * and every pull posts the absolute consumed count back. The playback worklet uses it when the page
 * is not isolated, and the mic drain likewise. `pull` copies from the producer's chunks by offset,
 * so its only audio-thread allocation is the `{ consumed }` reply.
 */
export class PortSource implements PcmSource {
  private readonly queue: Int16Array[] = [];
  private offset = 0;
  private buffered = 0;
  private consumed = 0n;
  private readonly formats: PcmFormatMark[] = [];

  /**
   * @param port the transferred port.
   * @param onControl anything on the port that is not a PCM chunk, such as `{ guestRate }`.
   */
  constructor(
    private readonly port: PortLike,
    private readonly onControl?: (data: unknown) => void,
  ) {
    port.onData((data) => {
      this.accept(data);
    });
  }

  accept(data: unknown): void {
    const message = data as Partial<PcmMessage> | null;
    const pcm = message?.pcm;
    if (pcm instanceof Int16Array) {
      if (pcm.length > 0) {
        this.queue.push(pcm);
        this.buffered += pcm.length;
      }
      return;
    }
    const format = (data as { format?: { guestRate?: unknown; channels?: unknown; at?: unknown } } | null)
      ?.format;
    if (
      format &&
      typeof format.guestRate === "number" &&
      typeof format.channels === "number" &&
      typeof format.at === "string" &&
      /^\d+$/.test(format.at)
    ) {
      this.formats.push({ guestRate: format.guestRate, channels: format.channels, at: BigInt(format.at) });
      return;
    }
    this.onControl?.(data);
  }

  takeFormat(): PcmFormatMark | null {
    return this.formats.shift() ?? null;
  }

  pull(out: Int16Array): number {
    let written = 0;
    while (written < out.length) {
      const head = this.queue[0];
      if (!head) {
        break;
      }
      const take = Math.min(head.length - this.offset, out.length - written);
      out.set(head.subarray(this.offset, this.offset + take), written);
      written += take;
      this.offset += take;
      if (this.offset >= head.length) {
        this.queue.shift();
        this.offset = 0;
      }
    }
    if (written > 0) {
      this.buffered -= written;
      this.consumed += BigInt(written);
      this.port.postMessage({ consumed: this.consumed.toString() });
    }
    return written;
  }

  skip(count: number): number {
    let skipped = 0;
    const wanted = Math.max(0, Math.floor(count));
    while (skipped < wanted) {
      const head = this.queue[0];
      if (!head) {
        break;
      }
      const take = Math.min(head.length - this.offset, wanted - skipped);
      skipped += take;
      this.offset += take;
      if (this.offset >= head.length) {
        this.queue.shift();
        this.offset = 0;
      }
    }
    if (skipped > 0) {
      this.buffered -= skipped;
      this.consumed += BigInt(skipped);
      this.port.postMessage({ consumed: this.consumed.toString() });
    }
    return skipped;
  }

  bufferedSamples(): number {
    return this.buffered;
  }

  consumedSamples(): bigint {
    return this.consumed;
  }

  close(): void {
    this.port.onData(null);
    this.queue.length = 0;
    this.buffered = 0;
    this.port.close?.();
  }
}
