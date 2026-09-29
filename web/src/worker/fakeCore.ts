// The scripted fake core: a JavaScript implementation of the ABI over a real `WebAssembly.Memory`,
// laid out exactly as `pemu_wasm::io_view::IoPublisher` lays it out, so the unit tests exercise
// `WasmCore`, the views, the renderer, the audio pump and the pacing loop on the real bytes. It is
// the test double and the `?fake=` development core, never the shipped run path.

import type { CoreExports } from "./core";
import {
  ABI_VERSION,
  CURSOR_SLOTS,
  CursorSlot,
  DEFAULT_BACKLIGHT_SCALE,
  DIRTY_NONE,
  FRAME_HEIGHT,
  FRAME_WIDTH,
  FrameFlag,
  HostEventRecord,
  InputBatchHeader,
  InputKind,
  IoLayout,
  LineMarkRecord,
  PcmRecord,
  RING_COUNT,
  RingId,
  RingLayout,
  ResultHeader,
  STATUS_OK,
  SerialStream,
  StopCode,
  ringHeadSlot,
  ringTailSlot,
} from "./layout";

export interface ScriptStep {
  readonly durationPs: bigint;
  readonly serial?: string;
  /** Bytes it writes to `uart0_tx`, the ROM bootloader's console. */
  readonly uart0?: string;
  readonly events?: readonly { readonly kind: number; readonly arg: bigint }[];
  readonly pcm?: { readonly fs: number; readonly channels: number; readonly samples: Int16Array };
  readonly paint?: { readonly firstRow: number; readonly lastRow: number; readonly color: number };
  readonly stop?: number;
  /** Panel state from here on without a pixel write: the backlight numerator and the `FRAME_FLAGS` word. */
  readonly panel?: { readonly backlight?: number; readonly flags?: number };
}

/** Capacities the fake publishes. Small on purpose, so eviction is reachable in a test. */
export interface FakeCapacities {
  readonly byteRing: number;
  readonly pcmSamples: number;
  readonly pcmRecords: number;
  readonly events: number;
  readonly lineMarks: number;
}

const DEFAULT_CAPACITIES: FakeCapacities = {
  byteRing: 4096,
  pcmSamples: 4096,
  pcmRecords: 16,
  events: 64,
  lineMarks: 256,
};

const PS_PER_INSN = 6_250n;

interface RingState {
  readonly ptr: number;
  readonly capacity: number;
  readonly elemSize: number;
  head: bigint;
  tail: bigint;
}

export class FakeCore implements CoreExports {
  readonly memory: WebAssembly.Memory;
  private readonly view: DataView;
  private readonly bytes: Uint8Array;
  private readonly rings: RingState[] = [];
  private readonly capacities: FakeCapacities;
  private layoutPtr = 0;
  private cursorsPtr = 0;
  private framePtr = 0;
  private generation = 1;
  private nowPs = 0n;
  private heapTop: number;
  private handle = 0;
  private script: ScriptStep[] = [];
  private stepAt = 0;
  private lastStopJson = '{"reason":null}';
  private pendingStop: number | null = null;
  private paintedRows: { first: number; last: number } | null = null;
  private frameGeneration = 0n;
  private backlight = 1023;
  private flags: number | null = null;
  readonly journaled: {
    atPs: bigint;
    kind: number;
    a: number;
    b: number;
    c: number;
    payload: Uint8Array;
  }[] = [];
  /**
   * What the real core's `@journal` would answer: every input in `seq` order, converted to the serde
   * form of `InputEvent`, plus what a test journals through {@link FakeCore.journalRegistry}.
   */
  private readonly journalLog: { at_ps: string; seq: string; door: "input" | "registry"; event: unknown }[] = [];
  readonly calls: string[] = [];
  /** The Wi-Fi bridge `@relay` reports; the default is a machine with no bridge. */
  relay: {
    attached: boolean;
    routes: { port: number; host_port: number }[];
    out: Uint8Array[];
    dropped: bigint;
  } = { attached: false, routes: [], out: [], dropped: 0n };
  readonly relayCursors: (string | null)[] = [];
  drops = 0;

  constructor(capacities: Partial<FakeCapacities> = {}) {
    this.capacities = { ...DEFAULT_CAPACITIES, ...capacities };
    this.memory = new WebAssembly.Memory({ initial: 8 });
    this.view = new DataView(this.memory.buffer);
    this.bytes = new Uint8Array(this.memory.buffer);
    this.heapTop = this.buildLayout();
  }

  load(script: readonly ScriptStep[]): void {
    this.script = [...script];
    this.stepAt = 0;
  }

  get virtualTimePs(): bigint {
    return this.nowPs;
  }

  get exhausted(): boolean {
    return this.stepAt >= this.script.length;
  }

  // ---- the ABI ---------------------------------------------------------------------------

  pemu_abi_version(): number {
    return ABI_VERSION;
  }

  pemu_alloc(len: number): number {
    if (len === 0) {
      return 0;
    }
    const ptr = align8(this.heapTop);
    this.heapTop = ptr + len;
    if (this.heapTop > this.bytes.length) {
      throw new Error("the fake core ran out of linear memory");
    }
    return ptr;
  }

  pemu_free(_ptr: number, _len: number): void {
    // A bump allocator: never reusing a block keeps addresses distinct in a test.
  }

  pemu_result_free(_res: number): void {
  }

  pemu_new(_cfgPtr: number, _cfgLen: number): number {
    return 1;
  }

  pemu_load(_builder: number, _kind: number, _ptr: number, _len: number): number {
    return this.result(STATUS_OK, new Uint8Array(0));
  }

  pemu_build(_builder: number): number {
    this.handle = 1;
    const payload = new Uint8Array(4);
    new DataView(payload.buffer).setUint32(0, this.handle, true);
    return this.result(STATUS_OK, payload);
  }

  pemu_drop(_handle: number): void {
    this.handle = 0;
    this.drops += 1;
  }

  pemu_run(_handle: number, untilPs: bigint, maxInsns: bigint): number {
    const insnLimitPs = maxInsns * PS_PER_INSN;
    const limitPs = this.nowPs + (insnLimitPs > 0n ? insnLimitPs : 0n);
    const ceiling = untilPs < limitPs ? untilPs : limitPs;
    const hitInsnLimit = limitPs < untilPs;

    this.clearDirty();
    while (this.stepAt < this.script.length) {
      const step = this.script[this.stepAt];
      if (step === undefined) {
        break;
      }
      const end = this.nowPs + step.durationPs;
      if (end > ceiling) {
        break;
      }
      this.nowPs = end;
      this.apply(step);
      this.stepAt += 1;
      if (step.stop !== undefined) {
        this.pendingStop = step.stop;
        this.lastStopJson = stopJson(step.stop, this.nowPs);
        this.publish();
        return step.stop;
      }
    }
    if (this.nowPs < ceiling) {
      this.nowPs = ceiling;
    }
    this.publish();
    const stop = hitInsnLimit ? StopCode.MaxInsns : StopCode.Until;
    this.lastStopJson = stopJson(stop, this.nowPs);
    return stop;
  }

  pemu_last_stop(_handle: number): number {
    return this.result(STATUS_OK, new TextEncoder().encode(this.lastStopJson));
  }

  pemu_input(_handle: number, ptr: number, len: number): number {
    if (len >= InputBatchHeader.SIZE) {
      const count = this.view.getUint32(ptr + InputBatchHeader.COUNT, true);
      for (let index = 0; index < count; index += 1) {
        const at = ptr + InputBatchHeader.SIZE + index * 32;
        const blobOff = this.view.getUint32(at + 24, true);
        const blobLen = this.view.getUint32(at + 28, true);
        const record = {
          atPs: this.view.getBigInt64(at + 0, true),
          kind: this.view.getUint32(at + 8, true),
          a: this.view.getUint32(at + 12, true),
          b: this.view.getUint32(at + 16, true),
          c: this.view.getUint32(at + 20, true),
          // Offsets are relative to the batch start, as `input_batch::decode` reads them.
          payload:
            blobLen === 0
              ? new Uint8Array(0)
              : this.bytes.slice(ptr + blobOff, ptr + blobOff + blobLen),
        };
        this.journaled.push(record);
        this.logInput(record.atPs < 0n ? this.nowPs : record.atPs, "input", eventJson(record));
      }
    }
    return this.result(STATUS_OK, new Uint8Array(0));
  }

  journalRegistry(atPs: bigint, event: unknown): void {
    this.logInput(atPs, "registry", event);
  }

  /** The real core's `@journal` answer: live microphone chunks become a marker unless `include_secrets`. */
  private journalAnswer(request: string): unknown {
    const includeSecrets =
      (JSON.parse(request) as { args?: { include_secrets?: boolean } }).args?.include_secrets === true;
    const dropped: { kind: string; seq: string }[] = [];
    const entries = this.journalLog.map((entry) => {
      const chunk = (entry.event as { MicChunk?: { seq: number; samples: number[] } }).MicChunk;
      if (entry.door !== "input" || !chunk || includeSecrets) {
        return entry;
      }
      dropped.push({ kind: "MicChunk", seq: String(chunk.seq) });
      return {
        ...entry,
        event: null,
        dropped: { kind: "MicChunk", seq: String(chunk.seq), len: chunk.samples.length },
      };
    });
    return { format: 1, replayable: dropped.length === 0, dropped, entries };
  }

  private logInput(atPs: bigint, door: "input" | "registry", event: unknown): void {
    this.journalLog.push({ at_ps: atPs.toString(), seq: String(this.journalLog.length), door, event });
  }

  pemu_io_layout(_handle: number): number {
    return this.layoutPtr;
  }

  pemu_now_ps(_handle: number): bigint {
    return this.nowPs;
  }

  pemu_call(_handle: number, jsonPtr: number, jsonLen: number): number {
    const request = new TextDecoder().decode(this.bytes.slice(jsonPtr, jsonPtr + jsonLen));
    // The reserved `@live` request: the next chunk number on the microphone stream. Not a registry
    // command, so it is kept out of `calls`.
    if (request === '{"cmd":"@live"}') {
      let next = 0n;
      for (const entry of this.journaled) {
        if (entry.kind === InputKind.MicChunk) {
          const seq = (BigInt(entry.b) << 32n) | BigInt(entry.a);
          next = seq + 1n > next ? seq + 1n : next;
        }
      }
      let netNext = 0n;
      for (const entry of this.journaled) {
        if (entry.kind === InputKind.NetFrame) {
          const seq = (BigInt(entry.b) << 32n) | BigInt(entry.a);
          netNext = seq + 1n > netNext ? seq + 1n : netNext;
        }
      }
      const body = JSON.stringify({
        mic_next_seq: next.toString(),
        net_next_seq: netNext.toString(),
        hci_next_seq: "0",
      });
      return this.result(STATUS_OK, new TextEncoder().encode(body));
    }
    if (request.startsWith('{"cmd":"@relay"')) {
      const args = (JSON.parse(request) as { args?: { cursor?: string } }).args;
      const cursor = args?.cursor ?? null;
      this.relayCursors.push(cursor);
      const from = cursor === null ? this.relay.out.length : Math.min(Number(cursor), this.relay.out.length);
      const body = JSON.stringify({
        attached: this.relay.attached,
        routes: this.relay.routes,
        packets: this.relay.out.slice(from).map((packet) => base64(packet)),
        cursor: String(this.relay.out.length),
        dropped: this.relay.dropped.toString(),
      });
      return this.result(STATUS_OK, new TextEncoder().encode(body));
    }
    if (request.startsWith('{"cmd":"@journal"')) {
      return this.result(STATUS_OK, new TextEncoder().encode(JSON.stringify(this.journalAnswer(request))));
    }
    this.calls.push(request);
    return this.result(STATUS_OK, new TextEncoder().encode('{"ok":true}'));
  }

  pemu_snapshot(_handle: number, _flags: number): number {
    const payload = new Uint8Array(8);
    new DataView(payload.buffer).setBigUint64(0, this.nowPs, true);
    return this.result(STATUS_OK, payload);
  }

  pemu_restore(_handle: number, ptr: number, len: number): number {
    if (len >= 8) {
      this.nowPs = this.view.getBigUint64(ptr, true);
    }
    // A restore may move a buffer, so the generation moves and the worker re-reads.
    this.generation += 1;
    this.view.setUint32(this.layoutPtr + IoLayout.GENERATION, this.generation, true);
    return this.result(STATUS_OK, new Uint8Array(0));
  }

  // ---- scripted effects ------------------------------------------------------------------

  private apply(step: ScriptStep): void {
    if (step.serial !== undefined) {
      this.writeSerial(RingId.UsjTx, RingId.LinesUsjTx, SerialStream.UsjTx, step.serial);
    }
    if (step.uart0 !== undefined) {
      this.writeSerial(RingId.Uart0Tx, RingId.LinesUart0Tx, SerialStream.Uart0Tx, step.uart0);
    }
    for (const event of step.events ?? []) {
      this.pushRecord(RingId.Events, (write, at) => {
        write.setBigInt64(at + HostEventRecord.VT_PS, this.nowPs, true);
        write.setBigUint64(at + HostEventRecord.ARG, event.arg, true);
        write.setUint32(at + HostEventRecord.KIND, event.kind, true);
        write.setUint32(at + 20, 0, true);
      });
    }
    if (step.pcm) {
      this.writePcm(step.pcm.fs, step.pcm.channels, step.pcm.samples);
    }
    if (step.paint) {
      this.paint(step.paint.firstRow, step.paint.lastRow, step.paint.color);
    }
    if (step.panel?.backlight !== undefined) {
      this.backlight = step.panel.backlight;
    }
    if (step.panel?.flags !== undefined) {
      this.flags = step.panel.flags;
    }
  }

  private writeSerial(bytes: RingId, lines: RingId, stream: number, text: string): void {
    const data = new TextEncoder().encode(text);
    const ring = this.ring(bytes);
    for (const byte of data) {
      const start = ring.head;
      this.bytes[ring.ptr + Number(start % BigInt(ring.capacity))] = byte;
      ring.head += 1n;
      if (ring.head - ring.tail > BigInt(ring.capacity)) {
        ring.tail = ring.head - BigInt(ring.capacity);
      }
      if (byte === 0x0a) {
        this.pushRecord(lines, (write, at) => {
          write.setBigUint64(at + LineMarkRecord.OFFSET, start, true);
          write.setBigInt64(at + LineMarkRecord.VT_PS, this.nowPs, true);
          write.setUint32(at + LineMarkRecord.STREAM, stream, true);
          write.setUint32(at + 20, 0, true);
        });
      }
    }
  }

  private writePcm(fs: number, channels: number, samples: Int16Array): void {
    const ring = this.ring(RingId.AudioOutSamples);
    const first = ring.head;
    const view = new Int16Array(this.memory.buffer, ring.ptr, ring.capacity);
    for (const sample of samples) {
      view[Number(ring.head % BigInt(ring.capacity))] = sample;
      ring.head += 1n;
      if (ring.head - ring.tail > BigInt(ring.capacity)) {
        ring.tail = ring.head - BigInt(ring.capacity);
      }
    }
    this.pushRecord(RingId.AudioOutRecords, (write, at) => {
      write.setBigInt64(at + PcmRecord.VT_START_PS, this.nowPs, true);
      write.setBigUint64(at + PcmRecord.FIRST, first, true);
      write.setUint32(at + PcmRecord.FS, fs, true);
      write.setUint32(at + PcmRecord.CHANNELS, channels, true);
    });
  }

  private paint(firstRow: number, lastRow: number, color: number): void {
    const pixels = new Uint16Array(this.memory.buffer, this.framePtr, FRAME_WIDTH * FRAME_HEIGHT);
    for (let row = firstRow; row <= lastRow; row += 1) {
      pixels.fill(color, row * FRAME_WIDTH, (row + 1) * FRAME_WIDTH);
    }
    const span = this.paintedRows;
    this.paintedRows = span
      ? { first: Math.min(span.first, firstRow), last: Math.max(span.last, lastRow) }
      : { first: firstRow, last: lastRow };
    this.frameGeneration += 1n;
  }

  private pushRecord(ring: RingId, fill: (view: DataView, at: number) => void): void {
    const state = this.ring(ring);
    const slot = Number(state.head % BigInt(state.capacity));
    fill(this.view, state.ptr + slot * state.elemSize);
    state.head += 1n;
    if (state.head - state.tail > BigInt(state.capacity)) {
      state.tail = state.head - BigInt(state.capacity);
    }
  }

  private clearDirty(): void {
    this.paintedRows = null;
  }

  private publish(): void {
    const cursors = new BigUint64Array(this.memory.buffer, this.cursorsPtr, CURSOR_SLOTS);
    for (let ring = 0; ring < RING_COUNT; ring += 1) {
      const state = this.rings[ring];
      if (!state) {
        continue;
      }
      cursors[ringHeadSlot(ring as RingId)] = state.head;
      cursors[ringTailSlot(ring as RingId)] = state.tail;
    }
    cursors[CursorSlot.FrameGeneration] = this.frameGeneration;
    cursors[CursorSlot.AudioOutUnderflows] = 0n;
    cursors[CursorSlot.AudioInUnderflows] = 0n;
    cursors[CursorSlot.NowPs] = BigInt.asUintN(64, this.nowPs);

    const span = this.paintedRows;
    this.view.setUint32(
      this.layoutPtr + IoLayout.FRAME_DIRTY_FIRST,
      span ? span.first : DIRTY_NONE,
      true,
    );
    this.view.setUint32(this.layoutPtr + IoLayout.FRAME_DIRTY_LAST, span ? span.last : 0, true);
    this.view.setUint32(
      this.layoutPtr + IoLayout.FRAME_FLAGS,
      // The official firmware's init: rail up, INVON and DISPON, asleep until the first paint. With the
      // board's `invon_shows_ram`, INVON shows memory unmodified.
      this.flags ??
        FrameFlag.Powered |
          FrameFlag.Inverted |
          FrameFlag.DisplayOn |
          (this.frameGeneration > 0n ? 0 : FrameFlag.Sleeping),
      true,
    );
    this.view.setUint32(this.layoutPtr + IoLayout.FRAME_BACKLIGHT, this.backlight, true);
    // As `IoPublisher::refresh` does; 0 would mean "not modelled" and light the panel fully.
    this.view.setUint32(
      this.layoutPtr + IoLayout.FRAME_BACKLIGHT_SCALE,
      DEFAULT_BACKLIGHT_SCALE,
      true,
    );
  }

  private ring(id: RingId): RingState {
    const state = this.rings[id];
    if (!state) {
      throw new Error(`the fake core has no ring ${id}`);
    }
    return state;
  }

  private result(status: number, payload: Uint8Array): number {
    const block = this.pemu_alloc(ResultHeader.SIZE + payload.length + 8);
    const dataPtr = align8(block + ResultHeader.SIZE);
    this.bytes.set(payload, dataPtr);
    this.view.setUint32(block + ResultHeader.PTR, dataPtr, true);
    this.view.setUint32(block + ResultHeader.LEN, payload.length, true);
    this.view.setUint32(block + ResultHeader.STATUS, status, true);
    return block;
  }

  private buildLayout(): number {
    let at = 16;
    this.layoutPtr = at;
    at = align8(at + IoLayout.SIZE);
    this.cursorsPtr = at;
    at += CURSOR_SLOTS * 8;

    const plan: [RingId, number, number][] = [
      [RingId.UsjTx, this.capacities.byteRing, 1],
      [RingId.UsjRx, this.capacities.byteRing, 1],
      [RingId.Uart0Tx, this.capacities.byteRing, 1],
      [RingId.AudioOutSamples, this.capacities.pcmSamples, 2],
      [RingId.AudioOutRecords, this.capacities.pcmRecords, PcmRecord.SIZE],
      [RingId.AudioInSamples, this.capacities.pcmSamples, 2],
      [RingId.AudioInRecords, this.capacities.pcmRecords, PcmRecord.SIZE],
      [RingId.Events, this.capacities.events, HostEventRecord.SIZE],
      [RingId.LinesUsjTx, this.capacities.lineMarks, LineMarkRecord.SIZE],
      [RingId.LinesUart0Tx, this.capacities.lineMarks, LineMarkRecord.SIZE],
    ];
    for (const [ring, capacity, elemSize] of plan) {
      at = align8(at);
      this.rings[ring] = { ptr: at, capacity, elemSize, head: 0n, tail: 0n };
      at += capacity * elemSize;
    }
    at = align8(at);
    this.framePtr = at;
    at += FRAME_WIDTH * FRAME_HEIGHT * 2;

    this.view.setUint32(this.layoutPtr + IoLayout.ABI_VERSION, ABI_VERSION, true);
    this.view.setUint32(this.layoutPtr + IoLayout.GENERATION, this.generation, true);
    this.view.setUint32(this.layoutPtr + IoLayout.CURSORS_PTR, this.cursorsPtr, true);
    this.view.setUint32(this.layoutPtr + IoLayout.CURSOR_SLOTS, CURSOR_SLOTS, true);
    this.view.setUint32(this.layoutPtr + IoLayout.FRAME_PTR, this.framePtr, true);
    this.view.setUint32(this.layoutPtr + IoLayout.FRAME_WIDTH, FRAME_WIDTH, true);
    this.view.setUint32(this.layoutPtr + IoLayout.FRAME_HEIGHT, FRAME_HEIGHT, true);
    this.view.setUint32(this.layoutPtr + IoLayout.FRAME_DIRTY_FIRST, DIRTY_NONE, true);
    for (const [ring, capacity, elemSize] of plan) {
      const state = this.ring(ring);
      const entry = this.layoutPtr + IoLayout.RINGS + ring * RingLayout.SIZE;
      this.view.setUint32(entry + RingLayout.BUF_PTR, state.ptr, true);
      this.view.setUint32(entry + RingLayout.CAPACITY, capacity, true);
      this.view.setUint32(entry + RingLayout.ELEM_SIZE, elemSize, true);
      this.view.setUint32(entry + RingLayout.CURSOR_SLOT, ringHeadSlot(ring), true);
    }
    this.publish();
    return align8(at);
  }

  get lastStop(): number | null {
    return this.pendingStop;
  }
}

/** The `pemu_last_stop` JSON in the real core's shape (`crates/pemu-wasm/src/instance.rs` `last_stop_json`). */
function stopJson(code: number, nowPs: bigint): string {
  const name = Object.entries(StopCode).find(([, value]) => value === code)?.[0] ?? null;
  return JSON.stringify({ reason: name, code, vt_ps: nowPs.toString() });
}

function align8(at: number): number {
  return (at + 7) & ~7;
}

/**
 * An input record in the serde form of `pemu_core::input::InputEvent`, as the real core journals
 * it (`crates/pemu-wasm/src/input_batch.rs`); only the kinds the Worker sends.
 */
function eventJson(record: { kind: number; a: number; b: number; payload: Uint8Array }): unknown {
  const buttons = ["Up", "Down", "Ok"];
  switch (record.kind) {
    case InputKind.Button:
      return { Button: { id: buttons[record.a] ?? "Up", down: record.b !== 0 } };
    case InputKind.Power:
      return { Power: { down: record.b !== 0 } };
    case InputKind.SerialIn:
      return { SerialIn: { chan: record.a, data: [...record.payload] } };
    case InputKind.MicChunk: {
      const view = new DataView(record.payload.buffer, record.payload.byteOffset, record.payload.byteLength);
      const samples: number[] = [];
      for (let at = 0; at + 1 < record.payload.length; at += 2) {
        samples.push(view.getInt16(at, true));
      }
      const seq = (BigInt(record.b) << 32n) | BigInt(record.a);
      return { MicChunk: { seq: Number(seq), samples } };
    }
    default:
      return { UnknownKind: record.kind };
  }
}

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}
