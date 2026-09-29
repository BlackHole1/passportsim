import { describe, expect, test } from "bun:test";

import { CoreError, WasmCore } from "./core";
import { FakeCore } from "./fakeCore";
import { LoadKind, ResultHeader } from "./layout";

class LoadingCore extends FakeCore {
  readonly loads: { kind: number; bytes: Uint8Array }[] = [];
  builds = 0;

  constructor(private readonly refuse: readonly number[] = []) {
    super();
  }

  override pemu_load(builder: number, kind: number, ptr: number, len: number): number {
    this.loads.push({ kind, bytes: new Uint8Array(this.memory.buffer).slice(ptr, ptr + len) });
    if (!this.refuse.includes(kind)) {
      return super.pemu_load(builder, kind, ptr, len);
    }
    const body = new TextEncoder().encode('{"code":"E_USAGE","message":"not an image"}');
    const block = this.pemu_alloc(ResultHeader.SIZE + body.length);
    const view = new DataView(this.memory.buffer);
    new Uint8Array(this.memory.buffer).set(body, block + ResultHeader.SIZE);
    view.setUint32(block + ResultHeader.PTR, block + ResultHeader.SIZE, true);
    view.setUint32(block + ResultHeader.LEN, body.length, true);
    view.setUint32(block + ResultHeader.STATUS, 1, true);
    return block;
  }

  override pemu_build(builder: number): number {
    this.builds += 1;
    return super.pemu_build(builder);
  }
}

describe("WasmCore.build", () => {
  test("loads every asset, in order and byte for byte, before it builds", () => {
    const core = new LoadingCore();
    const image = Uint8Array.from([0xe9, 1, 2, 3]);
    const elf = Uint8Array.from([0x7f, 0x45, 0x4c, 0x46]);
    WasmCore.build(core, '{"fw":"official"}', [
      { kind: LoadKind.MergedFlash, bytes: image },
      { kind: LoadKind.AppElf, bytes: elf },
    ]);
    expect(core.loads.map((load) => load.kind)).toEqual([LoadKind.MergedFlash, LoadKind.AppElf]);
    expect(core.loads[0]?.bytes).toEqual(image);
    expect(core.loads[1]?.bytes).toEqual(elf);
    expect(core.builds).toBe(1);
  });

  test("a refused asset throws the core's ApiError and still consumes the builder", () => {
    const core = new LoadingCore([LoadKind.MergedFlash]);
    let thrown: unknown;
    try {
      WasmCore.build(core, "{}", [{ kind: LoadKind.MergedFlash, bytes: Uint8Array.of(1) }]);
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(CoreError);
    expect((thrown as CoreError).status).toBe(1);
    expect(JSON.parse((thrown as CoreError).body).code).toBe("E_USAGE");
    // `pemu_build` is the only call that frees a builder.
    expect(core.builds).toBe(1);
  });
});
