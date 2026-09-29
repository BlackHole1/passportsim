// The fake core decodes the batch exactly as `pemu_wasm::input_batch::decode` does.

import { describe, expect, test } from "bun:test";

import { FakeCore } from "./fakeCore";
import { InputStamper, JOURNAL_FORMAT, encodeBatch, journalFromCore } from "./input";
import { AT_NOW, ButtonId, INPUT_BATCH_MAGIC, InputBatchHeader, InputKind } from "./layout";

const MS = 1_000_000_000n;

describe("the fixed-layout batch", () => {
  test("starts with the magic, so the core can tell it from the JSON form", () => {
    const bytes = encodeBatch([
      { atPs: AT_NOW, kind: InputKind.Power, a: 0, b: 1, c: 0, payload: null },
    ]);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    expect(view.getUint32(InputBatchHeader.MAGIC, true)).toBe(INPUT_BATCH_MAGIC);
    expect(bytes[0]).not.toBe("[".charCodeAt(0));
    expect(bytes[0]).not.toBe("{".charCodeAt(0));
  });

  test("round-trips every scalar word and the payload through the core", () => {
    const core = new FakeCore();
    const payload = new Uint8Array([0x68, 0x69, 0x0a]);
    const bytes = encodeBatch([
      { atPs: 42n * MS, kind: InputKind.Button, a: ButtonId.Ok, b: 1, c: 0, payload: null },
      { atPs: AT_NOW, kind: InputKind.SerialIn, a: 0, b: 0, c: 0, payload },
    ]);
    const ptr = core.pemu_alloc(bytes.length);
    new Uint8Array(core.memory.buffer).set(bytes, ptr);
    core.pemu_input(1, ptr, bytes.length);

    expect(core.journaled).toHaveLength(2);
    expect(core.journaled[0]).toEqual({
      atPs: 42n * MS,
      kind: InputKind.Button,
      a: ButtonId.Ok,
      b: 1,
      c: 0,
      payload: new Uint8Array(0),
    });
    expect(core.journaled[1]?.payload).toEqual(payload);
  });

  test("packs several payloads into one blob area without overlapping them", () => {
    const core = new FakeCore();
    const bytes = encodeBatch([
      { atPs: AT_NOW, kind: InputKind.SerialIn, a: 0, b: 0, c: 0, payload: new Uint8Array([1]) },
      { atPs: AT_NOW, kind: InputKind.SerialIn, a: 1, b: 0, c: 0, payload: new Uint8Array([2, 3]) },
    ]);
    const ptr = core.pemu_alloc(bytes.length);
    new Uint8Array(core.memory.buffer).set(bytes, ptr);
    core.pemu_input(1, ptr, bytes.length);

    expect(core.journaled[0]?.payload).toEqual(new Uint8Array([1]));
    expect(core.journaled[1]?.payload).toEqual(new Uint8Array([2, 3]));
  });

  test("encodes an empty batch as a header with no records", () => {
    const bytes = encodeBatch([]);
    expect(bytes.length).toBe(InputBatchHeader.SIZE);
    const view = new DataView(bytes.buffer);
    expect(view.getUint32(InputBatchHeader.COUNT, true)).toBe(0);
  });
});

describe("stamping", () => {
  test("gives every queued input the boundary it was drained at", () => {
    const stamper = new InputStamper();
    stamper.button(ButtonId.Up, true);
    stamper.button(ButtonId.Up, false);
    expect(stamper.hasPending).toBe(true);

    const stamped = stamper.drain(7n * MS);
    expect(stamped?.map((input) => input.atPs)).toEqual([7n * MS, 7n * MS]);
    expect(stamper.hasPending).toBe(false);
  });

  test("returns nothing when nothing was queued, so no call is made", () => {
    expect(new InputStamper().drain(0n)).toBeNull();
  });

  test("journals what it stamped, in order, with the payloads", () => {
    const stamper = new InputStamper();
    stamper.power(true);
    stamper.drain(MS);
    stamper.serial(0, new Uint8Array([0x71]));
    stamper.drain(2n * MS);

    expect(stamper.entries.map((entry) => [entry.kind, entry.atPs])).toEqual([
      [InputKind.Power, MS],
      [InputKind.SerialIn, 2n * MS],
    ]);
  });

  test("splits a microphone sequence number over both scalar words", () => {
    const stamper = new InputStamper();
    stamper.micChunk(0x1_0000_0002n, Int16Array.from([1, -1]));
    const stamped = stamper.drain(MS);
    expect(stamped?.[0]?.a).toBe(2);
    expect(stamped?.[0]?.b).toBe(1);
    expect(stamped?.[0]?.payload).toEqual(new Uint8Array([1, 0, 0xff, 0xff]));
  });
});

describe("the journal export", () => {
  const button = (id: string, down: boolean) => ({ Button: { id, down } });

  test("is the core's `@journal` answer with its format and the ABI version, in plain JSON", () => {
    const answer = JSON.stringify({
      format: 1,
      replayable: true,
      dropped: [],
      entries: [
        { at_ps: (5n * MS).toString(), seq: "0", door: "input", event: button("Down", true) },
        { at_ps: (6n * MS).toString(), seq: "1", door: "registry", event: button("Down", false) },
      ],
    });
    const exported = journalFromCore(answer, 3);
    expect(exported).toEqual({
      format: JOURNAL_FORMAT,
      abiVersion: 3,
      replayable: true,
      dropped: [],
      entries: [
        { atPs: (5n * MS).toString(), seq: "0", door: "input", event: button("Down", true) },
        { atPs: (6n * MS).toString(), seq: "1", door: "registry", event: button("Down", false) },
      ],
    });
    expect(JSON.parse(JSON.stringify(exported))).toEqual(exported);
  });

  test("keeps a dropped live chunk's marker and says the export does not replay", () => {
    const answer = JSON.stringify({
      format: 1,
      replayable: false,
      dropped: [{ kind: "MicChunk", seq: "4" }],
      entries: [
        { at_ps: "7", seq: "0", door: "input", event: null, dropped: { kind: "MicChunk", seq: "4", len: 240 } },
      ],
    });
    const exported = journalFromCore(answer, 3);
    expect(exported.replayable).toBe(false);
    expect(exported.dropped).toEqual([{ kind: "MicChunk", seq: "4" }]);
    expect(exported.entries[0]).toEqual({
      atPs: "7",
      seq: "0",
      door: "input",
      event: null,
      dropped: { kind: "MicChunk", seq: "4", len: 240 },
    });
  });

  test("refuses an answer that is not the `@journal` shape or names another format", () => {
    const shaped = (entries: unknown[], format: unknown = 1) =>
      JSON.stringify({ format, replayable: true, dropped: [], entries });
    expect(() => journalFromCore('{"ok":true}', 3)).toThrow("format");
    expect(() => journalFromCore(shaped([], 2), 3)).toThrow("format 2");
    expect(() => journalFromCore('{"format":1,"entries":[]}', 3)).toThrow("replayable");
    expect(() => journalFromCore(shaped([{ at_ps: 1, seq: "0", door: "input", event: {} }]), 3)).toThrow("entry 0");
    expect(() => journalFromCore(shaped([{ at_ps: "1", seq: "0", door: "page", event: {} }]), 3)).toThrow("entry 0");
    expect(() => journalFromCore(shaped([{ at_ps: "1", seq: "0", door: "input", event: null }]), 3)).toThrow(
      "entry 0",
    );
  });
});
