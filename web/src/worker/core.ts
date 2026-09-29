// The wasm core as the Worker sees it. Everything above this seam talks to `EmulatorCore`, never a
// WebAssembly export, so the Worker runs the same against `pemu-wasm` and `fakeCore.ts`.

import { ABI_VERSION, assertAbiVersion, LoadKind, ResultHeader, STATUS_OK } from "./layout";

export interface CoreExports {
  readonly memory: WebAssembly.Memory;
  pemu_abi_version(): number;
  pemu_alloc(len: number): number;
  pemu_free(ptr: number, len: number): void;
  pemu_result_free(res: number): void;
  pemu_new(cfgPtr: number, cfgLen: number): number;
  pemu_load(builder: number, kind: number, ptr: number, len: number): number;
  pemu_build(builder: number): number;
  pemu_drop(handle: number): void;
  pemu_run(handle: number, untilPs: bigint, maxInsns: bigint): number;
  pemu_last_stop(handle: number): number;
  pemu_input(handle: number, ptr: number, len: number): number;
  pemu_io_layout(handle: number): number;
  pemu_now_ps(handle: number): bigint;
  pemu_call(handle: number, jsonPtr: number, jsonLen: number): number;
  pemu_snapshot(handle: number, flags: number): number;
  pemu_restore(handle: number, ptr: number, len: number): number;
}

export interface CoreAsset {
  readonly kind: LoadKind;
  readonly bytes: Uint8Array;
}

/** A call that failed: `status` is the ErrorCode number and `body` the ApiError JSON. */
export class CoreError extends Error {
  constructor(
    readonly status: number,
    readonly body: string,
  ) {
    super(`pemu call failed with status ${status}: ${body}`);
    this.name = "CoreError";
  }
}

/** One built machine. Every fallible method throws `CoreError`, so no caller frees a result header. */
export interface EmulatorCore {
  /** Raw linear memory; its `buffer` identity changes when wasm memory grows. */
  readonly memory: WebAssembly.Memory;
  ioLayoutPtr(): number;
  nowPs(): bigint;
  run(untilPs: bigint, maxInsns: bigint): number;
  lastStop(): string;
  /** Journals inputs from an already-encoded buffer (JSON or fixed-layout records). */
  input(bytes: Uint8Array): void;
  call(request: string): string;
  snapshot(flags: number): Uint8Array;
  /** Restores snapshot bytes; the layout generation changes afterwards. */
  restore(bytes: Uint8Array): void;
  drop(): void;
}

export function bytesOf(memory: WebAssembly.Memory): Uint8Array {
  return new Uint8Array(memory.buffer);
}

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export class WasmCore implements EmulatorCore {
  private constructor(
    private readonly exports: CoreExports,
    private readonly handle: number,
  ) {}

  get memory(): WebAssembly.Memory {
    return this.exports.memory;
  }

  /**
   * Builds a machine from a JSON configuration and firmware assets (`pemu_new`, `pemu_load`,
   * `pemu_build`). Kind 1 takes a merged flash image or a `.pebundle`. The real core refuses to
   * build with no firmware (`E_ASSET_MISSING`); the test fake needs none.
   */
  static build(exports: CoreExports, config: string, assets: readonly CoreAsset[] = []): WasmCore {
    assertAbiVersion(exports.pemu_abi_version());
    const cfg = textEncoder.encode(config);
    const cfgPtr = copyIn(exports, cfg);
    let builder = 0;
    try {
      builder = exports.pemu_new(cfgPtr, cfg.length);
      if (builder === 0) {
        throw new Error("pemu_new returned no builder");
      }
    } finally {
      exports.pemu_free(cfgPtr, cfg.length);
    }
    for (const asset of assets) {
      const ptr = copyIn(exports, asset.bytes);
      try {
        readResult(exports, exports.pemu_load(builder, asset.kind, ptr, asset.bytes.length));
      } catch (error) {
        // Only `pemu_build` frees a builder, so a refused asset still passes through it.
        exports.pemu_result_free(exports.pemu_build(builder));
        throw error;
      } finally {
        exports.pemu_free(ptr, asset.bytes.length);
      }
    }
    const handle = readResult(exports, exports.pemu_build(builder));
    if (handle.length < 4) {
      throw new Error("pemu_build returned no handle");
    }
    const view = new DataView(handle.buffer, handle.byteOffset, handle.byteLength);
    return new WasmCore(exports, view.getUint32(0, true));
  }

  ioLayoutPtr(): number {
    return this.exports.pemu_io_layout(this.handle);
  }

  nowPs(): bigint {
    return this.exports.pemu_now_ps(this.handle);
  }

  run(untilPs: bigint, maxInsns: bigint): number {
    return this.exports.pemu_run(this.handle, untilPs, maxInsns);
  }

  lastStop(): string {
    return textDecoder.decode(readResult(this.exports, this.exports.pemu_last_stop(this.handle)));
  }

  input(bytes: Uint8Array): void {
    this.withBuffer(bytes, (ptr, len) =>
      readResult(this.exports, this.exports.pemu_input(this.handle, ptr, len)),
    );
  }

  call(request: string): string {
    const body = textEncoder.encode(request);
    return textDecoder.decode(
      this.withBuffer(body, (ptr, len) =>
        readResult(this.exports, this.exports.pemu_call(this.handle, ptr, len)),
      ),
    );
  }

  snapshot(flags: number): Uint8Array {
    return readResult(this.exports, this.exports.pemu_snapshot(this.handle, flags));
  }

  restore(bytes: Uint8Array): void {
    this.withBuffer(bytes, (ptr, len) =>
      readResult(this.exports, this.exports.pemu_restore(this.handle, ptr, len)),
    );
  }

  drop(): void {
    this.exports.pemu_drop(this.handle);
  }

  private withBuffer<T>(bytes: Uint8Array, body: (ptr: number, len: number) => T): T {
    const ptr = copyIn(this.exports, bytes);
    try {
      return body(ptr, bytes.length);
    } finally {
      this.exports.pemu_free(ptr, bytes.length);
    }
  }
}

function copyIn(exports: CoreExports, bytes: Uint8Array): number {
  if (bytes.length === 0) {
    return 0;
  }
  const ptr = exports.pemu_alloc(bytes.length);
  if (ptr === 0) {
    throw new Error(`pemu_alloc(${bytes.length}) failed`);
  }
  bytesOf(exports.memory).set(bytes, ptr);
  return ptr;
}

/**
 * Copies a result's payload out, frees the header, and throws `CoreError` unless `STATUS_OK`. The
 * copy is deliberate: the next call may grow memory and detach the payload's view.
 */
export function readResult(exports: CoreExports, res: number): Uint8Array {
  if (res === 0) {
    throw new Error("an ABI call returned no result header");
  }
  try {
    const header = new DataView(exports.memory.buffer, res, ResultHeader.SIZE);
    const ptr = header.getUint32(ResultHeader.PTR, true);
    const len = header.getUint32(ResultHeader.LEN, true);
    const status = header.getUint32(ResultHeader.STATUS, true);
    const payload = len === 0 ? new Uint8Array(0) : bytesOf(exports.memory).slice(ptr, ptr + len);
    if (status !== STATUS_OK) {
      throw new CoreError(status, textDecoder.decode(payload));
    }
    return payload;
  } finally {
    exports.pemu_result_free(res);
  }
}

export const BUNDLE_ABI_VERSION = ABI_VERSION;
