// Ring reads over wasm linear memory through typed arrays, so nothing crosses the JS boundary per
// item. Rings number items with absolute `u64` cursors: the window is `[tail, head)` and item
// `pos` sits in slot `pos % capacity`. A read from below `tail` reports how many items were lost.

import {
  CURSOR_SLOTS,
  CursorSlot,
  DIRTY_NONE,
  FrameFlag,
  HostEventRecord,
  IoLayout,
  LineMarkRecord,
  PcmRecord,
  RING_COUNT,
  RingId,
  RingLayout,
  ringHeadSlot,
  ringTailSlot,
} from "./layout";

export interface RingDescriptor {
  readonly bufPtr: number;
  readonly capacity: number;
  readonly elemSize: number;
  readonly cursorSlot: number;
}

export interface FrameInfo {
  readonly generation: bigint;
  readonly width: number;
  readonly height: number;
  readonly backlight: number;
  /** Backlight denominator, `1 << duty_res`; 0 means not modelled yet. */
  readonly backlightScale: number;
  readonly powered: boolean;
  readonly sleeping: boolean;
  readonly inverted: boolean;
  readonly displayOn: boolean;
  /** The glass shows the complement of memory: `inverted != invon_shows_ram`. */
  readonly glassComplement: boolean;
  /** First dirty row, or `null` when nothing changed since the core last cleared the span. */
  readonly dirtyFirst: number | null;
  readonly dirtyLast: number;
}

export interface RingRead<T> {
  readonly items: T;
  readonly next: bigint;
  readonly dropped: bigint;
}

export interface HostEvent {
  readonly kind: number;
  readonly vtPs: bigint;
  readonly arg: bigint;
}

export interface PcmRecordHeader {
  readonly vtStartPs: bigint;
  readonly first: bigint;
  readonly fs: number;
  readonly channels: number;
}

export interface LineMark {
  readonly offset: bigint;
  readonly vtPs: bigint;
  readonly stream: number;
}

/**
 * The worker's views of one machine's published layout. A memory growth detaches typed arrays and
 * a restore moves buffers, so {@link IoViews.sync} re-creates every view when the buffer identity or
 * the layout generation changed; it runs before every read burst.
 */
export class IoViews {
  private buffer: ArrayBuffer;
  private generationSeen = 0;
  private rings: RingDescriptor[] = [];
  private cursors: BigUint64Array;
  private ringBytes: (Uint8Array | null)[] = [];
  private framePixels: Uint16Array;
  private frameInfo: FrameInfo;

  private constructor(
    private readonly memory: WebAssembly.Memory,
    private readonly layoutPtr: () => number,
  ) {
    this.buffer = memory.buffer;
    this.cursors = new BigUint64Array(0);
    this.framePixels = new Uint16Array(0);
    this.frameInfo = blankFrame();
    this.rebuild();
  }

  static of(memory: WebAssembly.Memory, layoutPtr: () => number): IoViews {
    return new IoViews(memory, layoutPtr);
  }

  get generation(): number {
    return this.generationSeen;
  }

  /** Re-creates the views if needed and refreshes the frame state; returns whether they were re-created. */
  sync(): boolean {
    const detached = this.buffer !== this.memory.buffer;
    const generation = detached ? -1 : this.readLayout().getUint32(IoLayout.GENERATION, true);
    if (detached || generation !== this.generationSeen) {
      this.buffer = this.memory.buffer;
      this.rebuild();
      return true;
    }
    this.readFrame();
    return false;
  }

  get frame(): FrameInfo {
    return this.frameInfo;
  }

  get pixels(): Uint16Array {
    return this.framePixels;
  }

  descriptor(ring: RingId): RingDescriptor {
    const entry = this.rings[ring];
    if (entry === undefined) {
      throw new Error(`ring ${ring} is not in the published layout`);
    }
    return entry;
  }

  cursor(slot: number): bigint {
    const value = this.cursors[slot];
    if (value === undefined) {
      throw new Error(`cursor cell ${slot} is outside the published block`);
    }
    return value;
  }

  head(ring: RingId): bigint {
    return this.cursor(ringHeadSlot(ring));
  }

  tail(ring: RingId): bigint {
    return this.cursor(ringTailSlot(ring));
  }

  signedCursor(slot: number): bigint {
    return BigInt.asIntN(64, this.cursor(slot));
  }

  /**
   * Reads at most `max` bytes from `cursor` in at most two `set` calls: per-byte `BigInt` math on a
   * 16 KiB slice would cost more than the guest instructions that produced it.
   */
  readBytes(ring: RingId, cursor: bigint, max: number): RingRead<Uint8Array> {
    const { from, count, dropped } = this.window(ring, cursor, max);
    const slots = this.slots(ring);
    const out = new Uint8Array(count);
    copyWrapped(slots, out, this.startSlot(ring, from), count);
    return { items: out, next: from + BigInt(count), dropped };
  }

  readSamples(ring: RingId, cursor: bigint, max: number): RingRead<Int16Array> {
    const { from, count, dropped } = this.window(ring, cursor, max);
    const descriptor = this.descriptor(ring);
    const samples = new Int16Array(this.memory.buffer, descriptor.bufPtr, descriptor.capacity);
    const out = new Int16Array(count);
    copyWrapped(samples, out, this.startSlot(ring, from), count);
    return { items: out, next: from + BigInt(count), dropped };
  }

  private startSlot(ring: RingId, from: bigint): number {
    const capacity = this.descriptor(ring).capacity;
    return capacity === 0 ? 0 : Number(from % BigInt(capacity));
  }

  readEvents(cursor: bigint, max: number): RingRead<HostEvent[]> {
    return this.readRecords(RingId.Events, cursor, max, (view, at) => ({
      vtPs: view.getBigInt64(at + HostEventRecord.VT_PS, true),
      arg: view.getBigUint64(at + HostEventRecord.ARG, true),
      kind: view.getUint32(at + HostEventRecord.KIND, true),
    }));
  }

  readPcmRecords(ring: RingId, cursor: bigint, max: number): RingRead<PcmRecordHeader[]> {
    return this.readRecords(ring, cursor, max, (view, at) => ({
      vtStartPs: view.getBigInt64(at + PcmRecord.VT_START_PS, true),
      first: view.getBigUint64(at + PcmRecord.FIRST, true),
      fs: view.getUint32(at + PcmRecord.FS, true),
      channels: view.getUint32(at + PcmRecord.CHANNELS, true),
    }));
  }

  readLineMarks(ring: RingId, cursor: bigint, max: number): RingRead<LineMark[]> {
    return this.readRecords(ring, cursor, max, (view, at) => ({
      offset: view.getBigUint64(at + LineMarkRecord.OFFSET, true),
      vtPs: view.getBigInt64(at + LineMarkRecord.VT_PS, true),
      stream: view.getUint32(at + LineMarkRecord.STREAM, true),
    }));
  }

  private readRecords<T>(
    ring: RingId,
    cursor: bigint,
    max: number,
    decode: (view: DataView, at: number) => T,
  ): RingRead<T[]> {
    const { from, count, dropped } = this.window(ring, cursor, max);
    const descriptor = this.descriptor(ring);
    const view = new DataView(
      this.memory.buffer,
      descriptor.bufPtr,
      descriptor.capacity * descriptor.elemSize,
    );
    const capacity = BigInt(descriptor.capacity);
    const items: T[] = [];
    for (let index = 0; index < count; index += 1) {
      const slot = Number((from + BigInt(index)) % capacity);
      items.push(decode(view, slot * descriptor.elemSize));
    }
    return { items, next: from + BigInt(count), dropped };
  }

  private window(
    ring: RingId,
    cursor: bigint,
    max: number,
  ): { from: bigint; count: number; dropped: bigint } {
    const tail = this.tail(ring);
    const head = this.head(ring);
    const dropped = cursor < tail ? tail - cursor : 0n;
    const from = cursor < tail ? tail : cursor;
    if (from >= head || max <= 0) {
      return { from: from > head ? head : from, count: 0, dropped };
    }
    const available = head - from;
    const count = Number(available < BigInt(max) ? available : BigInt(max));
    return { from, count, dropped };
  }

  private slots(ring: RingId): Uint8Array {
    const view = this.ringBytes[ring];
    if (!view) {
      throw new Error(`ring ${ring} has no byte view`);
    }
    return view;
  }

  private readLayout(): DataView {
    return new DataView(this.memory.buffer, this.layoutPtr(), IoLayout.SIZE);
  }

  private rebuild(): void {
    const layout = this.readLayout();
    this.generationSeen = layout.getUint32(IoLayout.GENERATION, true);
    const cursorsPtr = layout.getUint32(IoLayout.CURSORS_PTR, true);
    const cursorSlots = layout.getUint32(IoLayout.CURSOR_SLOTS, true);
    if (cursorSlots !== CURSOR_SLOTS) {
      throw new Error(
        `the core publishes ${cursorSlots} cursor cells, this bundle knows ${CURSOR_SLOTS}`,
      );
    }
    this.cursors = new BigUint64Array(this.memory.buffer, cursorsPtr, cursorSlots);

    this.rings = [];
    this.ringBytes = [];
    for (let ring = 0; ring < RING_COUNT; ring += 1) {
      const at = IoLayout.RINGS + ring * RingLayout.SIZE;
      const descriptor: RingDescriptor = {
        bufPtr: layout.getUint32(at + RingLayout.BUF_PTR, true),
        capacity: layout.getUint32(at + RingLayout.CAPACITY, true),
        elemSize: layout.getUint32(at + RingLayout.ELEM_SIZE, true),
        cursorSlot: layout.getUint32(at + RingLayout.CURSOR_SLOT, true),
      };
      this.rings.push(descriptor);
      this.ringBytes.push(
        descriptor.elemSize === 1
          ? new Uint8Array(this.memory.buffer, descriptor.bufPtr, descriptor.capacity)
          : null,
      );
    }

    const framePtr = layout.getUint32(IoLayout.FRAME_PTR, true);
    const width = layout.getUint32(IoLayout.FRAME_WIDTH, true);
    const height = layout.getUint32(IoLayout.FRAME_HEIGHT, true);
    this.framePixels = new Uint16Array(this.memory.buffer, framePtr, width * height);
    this.readFrame();
  }

  private readFrame(): void {
    const layout = this.readLayout();
    const flags = layout.getUint32(IoLayout.FRAME_FLAGS, true);
    const dirtyFirst = layout.getUint32(IoLayout.FRAME_DIRTY_FIRST, true);
    this.frameInfo = {
      generation:
        this.cursors.length > 0 ? this.cursor(CursorSlot.FrameGeneration) : 0n,
      width: layout.getUint32(IoLayout.FRAME_WIDTH, true),
      height: layout.getUint32(IoLayout.FRAME_HEIGHT, true),
      backlight: layout.getUint32(IoLayout.FRAME_BACKLIGHT, true),
      backlightScale: layout.getUint32(IoLayout.FRAME_BACKLIGHT_SCALE, true),
      powered: (flags & FrameFlag.Powered) !== 0,
      sleeping: (flags & FrameFlag.Sleeping) !== 0,
      inverted: (flags & FrameFlag.Inverted) !== 0,
      displayOn: (flags & FrameFlag.DisplayOn) !== 0,
      glassComplement: (flags & FrameFlag.GlassComplement) !== 0,
      dirtyFirst: dirtyFirst === DIRTY_NONE ? null : dirtyFirst,
      dirtyLast: layout.getUint32(IoLayout.FRAME_DIRTY_LAST, true),
    };
  }
}

function copyWrapped<T extends Uint8Array | Int16Array>(slots: T, out: T, start: number, count: number): void {
  if (count === 0) {
    return;
  }
  const head = Math.min(count, slots.length - start);
  out.set(slots.subarray(start, start + head) as T, 0);
  if (count > head) {
    out.set(slots.subarray(0, count - head) as T, head);
  }
}

function blankFrame(): FrameInfo {
  return {
    generation: 0n,
    width: 0,
    height: 0,
    backlight: 0,
    backlightScale: 0,
    powered: false,
    sleeping: true,
    inverted: false,
    displayOn: false,
    glassComplement: false,
    dirtyFirst: null,
    dirtyLast: 0,
  };
}
