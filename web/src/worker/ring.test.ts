import { describe, expect, test } from "bun:test";

import { FakeCore } from "./fakeCore";
import { CursorSlot, EventKind, RingId, SerialStream, StopCode } from "./layout";
import { IoViews } from "./ring";

function viewsOf(core: FakeCore): IoViews {
  return IoViews.of(core.memory, () => core.pemu_io_layout(1));
}

const MS = 1_000_000_000n;

describe("the byte ring", () => {
  test("hands out what the guest wrote, once, from a growing cursor", () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, serial: "boot\n" }, { durationPs: MS, serial: "ok\n" }]);
    const views = viewsOf(core);

    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    const first = views.readBytes(RingId.UsjTx, 0n, 64);
    expect(new TextDecoder().decode(first.items)).toBe("boot\n");
    expect(first.next).toBe(5n);
    expect(first.dropped).toBe(0n);

    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    const second = views.readBytes(RingId.UsjTx, first.next, 64);
    expect(new TextDecoder().decode(second.items)).toBe("ok\n");
    expect(second.next).toBe(8n);

    expect(views.readBytes(RingId.UsjTx, second.next, 64).items.length).toBe(0);
  });

  test("wraps in place, so a ring longer than its capacity still reads in order", () => {
    const core = new FakeCore({ byteRing: 8 });
    core.load([{ durationPs: MS, serial: "0123456789AB" }]);
    const views = viewsOf(core);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();

    expect(views.head(RingId.UsjTx)).toBe(12n);
    expect(views.tail(RingId.UsjTx)).toBe(4n);
    const read = views.readBytes(RingId.UsjTx, 4n, 64);
    expect(new TextDecoder().decode(read.items)).toBe("456789AB");
  });

  test("reports how many items a late reader lost and continues at the oldest kept one", () => {
    const core = new FakeCore({ byteRing: 8 });
    core.load([{ durationPs: MS, serial: "0123456789AB" }]);
    const views = viewsOf(core);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();

    const read = views.readBytes(RingId.UsjTx, 0n, 64);
    expect(read.dropped).toBe(4n);
    expect(new TextDecoder().decode(read.items)).toBe("456789AB");
    expect(read.next).toBe(12n);
  });

  test("takes at most the maximum it is given", () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, serial: "abcdef" }]);
    const views = viewsOf(core);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();

    const read = views.readBytes(RingId.UsjTx, 0n, 2);
    expect(new TextDecoder().decode(read.items)).toBe("ab");
    expect(read.next).toBe(2n);
  });
});

describe("the record rings", () => {
  test("decode events with their kind, virtual time and argument", () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, events: [{ kind: EventKind.Reset, arg: 0x15n }] },
      { durationPs: MS, events: [{ kind: EventKind.Panic, arg: 1n }] },
    ]);
    const views = viewsOf(core);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();

    const read = views.readEvents(0n, 16);
    expect(read.items).toEqual([
      { kind: EventKind.Reset, vtPs: MS, arg: 0x15n },
      { kind: EventKind.Panic, vtPs: 2n * MS, arg: 1n },
    ]);
    expect(read.next).toBe(2n);
  });

  test("decode PCM headers with the rate the firmware chose at runtime", () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: Int16Array.from([1, 2, 3]) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: Int16Array.from([4, 5]) } },
    ]);
    const views = viewsOf(core);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();

    const headers = views.readPcmRecords(RingId.AudioOutRecords, 0n, 8);
    expect(headers.items.map((record) => record.fs)).toEqual([16_000, 24_000]);
    expect(headers.items.map((record) => record.first)).toEqual([0n, 3n]);
    const samples = views.readSamples(RingId.AudioOutSamples, 0n, 16);
    expect(Array.from(samples.items)).toEqual([1, 2, 3, 4, 5]);
  });

  test("decode a line mark per newline, with the stream it belongs to", () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, serial: "one\ntwo\n" }]);
    const views = viewsOf(core);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();

    const marks = views.readLineMarks(RingId.LinesUsjTx, 0n, 8);
    expect(marks.items.map((mark) => mark.offset)).toEqual([3n, 7n]);
    expect(marks.items.every((mark) => mark.stream === SerialStream.UsjTx)).toBe(true);
    expect(marks.items.every((mark) => mark.vtPs === MS)).toBe(true);
  });
});

describe("the JS view rule", () => {
  test("keeps the views while the generation holds and re-creates them when it moves", () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, serial: "x" }]);
    const views = viewsOf(core);
    const generation = views.generation;

    core.pemu_run(1, MS, 1n << 40n);
    expect(views.sync()).toBe(false);
    expect(views.generation).toBe(generation);

    const snapshot = new Uint8Array(8);
    core.pemu_restore(1, core.pemu_alloc(8), 0);
    expect(snapshot.length).toBe(8);
    expect(views.sync()).toBe(true);
    expect(views.generation).toBeGreaterThan(generation);
  });

  test("publishes the frame span, the panel state and virtual time", () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, paint: { firstRow: 10, lastRow: 12, color: 0xf800 } }]);
    const views = viewsOf(core);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();

    expect(views.frame.dirtyFirst).toBe(10);
    expect(views.frame.dirtyLast).toBe(12);
    expect(views.frame.powered).toBe(true);
    expect(views.frame.generation).toBe(1n);
    expect(views.pixels[10 * 240]).toBe(0xf800);
    expect(views.signedCursor(CursorSlot.NowPs)).toBe(MS);
  });

  test("clears the dirty span on a slice that painted nothing", () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, paint: { firstRow: 0, lastRow: 0, color: 0x07e0 } },
      { durationPs: MS, serial: "quiet\n" },
    ]);
    const views = viewsOf(core);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    expect(views.frame.dirtyFirst).toBe(0);

    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    expect(views.frame.dirtyFirst).toBeNull();
  });
});

describe("the fake core's own ABI", () => {
  test("stops at a scripted stop and reports it as the run's answer", () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, serial: "a\n" },
      { durationPs: MS, stop: StopCode.GuestPanic },
      { durationPs: MS, serial: "never\n" },
    ]);
    expect(core.pemu_run(1, 10n * MS, 1n << 40n)).toBe(StopCode.GuestPanic);
    expect(core.virtualTimePs).toBe(2n * MS);
    expect(core.exhausted).toBe(false);
  });

  test("returns MaxInsns when the instruction bound is reached before the time bound", () => {
    const core = new FakeCore();
    core.load([{ durationPs: 1_000n * MS }]);
    expect(core.pemu_run(1, 1_000n * MS, 16n)).toBe(StopCode.MaxInsns);
    expect(core.virtualTimePs).toBe(16n * 6_250n);
  });
});
