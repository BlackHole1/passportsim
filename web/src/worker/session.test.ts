import { describe, expect, test } from "bun:test";

import type { PanelView } from "../gl/rgb565";
import { CHUNK_FRAMES } from "../audio/capture";
import { SharedRingTransport } from "../audio/transport";
import { WasmCore, type EmulatorCore } from "./core";
import { FakeCore } from "./fakeCore";
import { ButtonId, EventKind, FrameFlag, InputKind, RingId, StopCode } from "./layout";
import type { PacingClock, Yielder } from "./pacing";
import { EmulatorSession } from "./session";

const MS = 1_000_000_000n;
const SLICE = 8n * MS;

class TestClock implements PacingClock, Yielder {
  ms = 0;

  nowMs(): number {
    return this.ms;
  }

  async sleep(ms: number): Promise<void> {
    this.ms += ms;
  }
}

class RecordingRenderer {
  readonly uploads: { first: number; last: number; firstPixel: number }[] = [];
  readonly draws: PanelView[] = [];

  upload(pixels: Uint16Array, first: number, last: number): void {
    this.uploads.push({ first, last, firstPixel: pixels[first * 240] ?? 0 });
  }

  draw(panel: PanelView): void {
    this.draws.push(panel);
  }
}

function sessionOn(
  core: FakeCore,
  renderer?: RecordingRenderer,
): { session: EmulatorSession; clock: TestClock; wrapped: EmulatorCore } {
  const clock = new TestClock();
  const wrapped = WasmCore.build(core, "{}");
  const session = new EmulatorSession(
    {
      core: wrapped,
      renderer: renderer as unknown as null,
      canvas: { width: 480, height: 640 },
      nowMs: () => clock.ms,
    },
    clock,
    clock,
  );
  return { session, clock, wrapped };
}

describe("booting", () => {
  test("builds through the ABI and agrees on the version", () => {
    const core = new FakeCore();
    const { session } = sessionOn(core);
    expect(session.abiVersion).toBe(core.pemu_abi_version());
  });

  test("refuses a core that speaks another ABI version", () => {
    const core = new FakeCore();
    const wrong = Object.create(Object.getPrototypeOf(core) as object) as FakeCore;
    Object.assign(wrong, core, { pemu_abi_version: () => 999 });
    expect(() => WasmCore.build(wrong, "{}")).toThrow(/does not match/);
  });
});

describe("a paced session", () => {
  test("hands the UI the serial bytes and the events of every slice", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, serial: "boot\n", events: [{ kind: EventKind.Reset, arg: 0x15n }] },
      { durationPs: MS, serial: "ready\n" },
    ]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    const first = await session.step();
    expect(new TextDecoder().decode(first.report.serial[0]?.bytes)).toBe("boot\nready\n");
    expect(first.report.serial[0]?.stream).toBe(RingId.UsjTx);
    expect(first.report.events.map((event) => event.kind)).toEqual([EventKind.Reset]);

    const second = await session.step();
    expect(second.report.serial).toHaveLength(0);
    expect(second.report.events).toHaveLength(0);
  });

  test("carries the line marks and the eviction count of each stream", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, serial: "one\ntwo\n" }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    const { report } = await session.step();
    const usj = report.serial.find((slice) => slice.stream === RingId.UsjTx);
    expect(usj?.lines.map((mark) => mark.offset)).toEqual([3n, 7n]);
    expect(usj?.dropped).toBe(0n);
  });

  test("reads uart0 as well as usj, so the ROM's own output is not dropped", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, uart0: "rst:0x1\n" }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    const { report } = await session.step();
    const uart0 = report.serial.find((slice) => slice.stream === RingId.Uart0Tx);
    expect(new TextDecoder().decode(uart0?.bytes)).toBe("rst:0x1\n");
    expect(uart0?.lines.map((mark) => mark.offset)).toEqual([7n]);
  });

  test("reports what the ring evicted before the session read it", async () => {
    // A byte ring of 8 with 24 bytes written: the first 16 are gone before the first read.
    const core = new FakeCore({ byteRing: 8 });
    core.load([{ durationPs: MS, serial: "0123456789abcdefghijklmn" }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    const { report } = await session.step();
    const usj = report.serial.find((slice) => slice.stream === RingId.UsjTx);
    expect(usj?.dropped).toBe(16n);
    expect(new TextDecoder().decode(usj?.bytes)).toBe("ghijklmn");
  });

  test("stops on a guest panic and keeps the stop's JSON", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS, stop: StopCode.GuestPanic }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    const { outcome } = await session.step();
    expect(outcome.kind).toBe("stopped");
    if (outcome.kind === "stopped") {
      expect(outcome.stop).toBe(StopCode.GuestPanic);
    }
    expect(session.lastStopJson).toContain(String(StopCode.GuestPanic));
  });
});

describe("input", () => {
  test("is stamped with the slice boundary and journaled", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }, { durationPs: SLICE }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    await session.step();
    const boundary = core.virtualTimePs;
    session.button(ButtonId.Ok, true);
    await session.step();

    expect(core.journaled).toHaveLength(1);
    expect(core.journaled[0]?.kind).toBe(InputKind.Button);
    expect(core.journaled[0]?.a).toBe(ButtonId.Ok);
    expect(core.journaled[0]?.atPs).toBe(boundary);
  });

  test("is not sent at all when nothing was queued", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });
    await session.step();
    expect(core.journaled).toHaveLength(0);
  });

  test("exports a journal that names every stamped input", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS }, { durationPs: MS }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    session.power(true);
    await session.step();
    session.serialIn(0, new Uint8Array([0x0d]));
    await session.step();

    const exported = session.exportJournal();
    expect(exported.entries.map((entry) => entry.event)).toEqual([
      { Power: { down: true } },
      { SerialIn: { chan: 0, data: [0x0d] } },
    ]);
    expect(exported.entries.map((entry) => entry.door)).toEqual(["input", "input"]);
  });

  test("exports the inputs a registry command journaled, in the machine's order", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS }, { durationPs: MS }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });

    session.button(ButtonId.Ok, true);
    await session.step();
    core.journalRegistry(MS, { Button: { id: "Down", down: true } });
    session.button(ButtonId.Ok, false);
    await session.step();

    const exported = session.exportJournal();
    expect(exported.entries.map((entry) => [entry.door, entry.event])).toEqual([
      ["input", { Button: { id: "Ok", down: true } }],
      ["registry", { Button: { id: "Down", down: true } }],
      ["input", { Button: { id: "Ok", down: false } }],
    ]);
    expect(exported.entries.map((entry) => entry.seq)).toEqual(["0", "1", "2"]);
  });

  test("drops live microphone chunks from the export unless asked", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS }]);
    const { session } = sessionOn(core);
    session.setMode({ kind: "Max" });
    session.input.micChunk(0n, Int16Array.from([1, 2]));
    await session.step();

    const shared = session.exportJournal();
    expect(shared.replayable).toBe(false);
    expect(shared.dropped).toEqual([{ kind: "MicChunk", seq: "0" }]);
    expect(shared.entries[0]).toMatchObject({ event: null, dropped: { kind: "MicChunk", seq: "0", len: 2 } });

    const full = session.exportJournal({ includeSecrets: true });
    expect(full.replayable).toBe(true);
    expect(full.entries[0]?.event).toEqual({ MicChunk: { seq: 0, samples: [1, 2] } });
  });
});

describe("presenting", () => {
  test("reports the panel state the core published, for the page's status line", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 }, panel: { backlight: 1023 } },
      {
        durationPs: SLICE,
        panel: { flags: FrameFlag.Powered | FrameFlag.GlassComplement, backlight: 512 },
      },
    ]);
    const { session, clock } = sessionOn(core, new RecordingRenderer());
    session.setMode({ kind: "Max" });

    const first = (await session.step()).report;
    expect(first.panel).toEqual({
      backlight: 1023,
      backlightScale: 1024,
      powered: true,
      sleeping: false,
      displayOn: true,
      inverted: true,
      glassComplement: false,
    });
    clock.ms += 20;
    const second = (await session.step()).report;
    expect(second.panel).toEqual({
      backlight: 512,
      backlightScale: 1024,
      powered: true,
      sleeping: false,
      displayOn: false,
      inverted: false,
      glassComplement: true,
    });
  });

  test("uploads only the rows the core marked dirty, after the first full upload", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 } },
      { durationPs: SLICE, paint: { firstRow: 5, lastRow: 7, color: 0x001f } },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    clock.ms += 20;
    const { report } = await session.step();
    expect(report.presented).toBe(true);
    expect(report.frameGeneration).toBe(2n);
    expect(renderer.uploads[1]).toEqual({ first: 5, last: 7, firstPixel: 0x001f });
    expect(renderer.draws[1]?.powered).toBe(true);
  });

  test("uploads every row and draws on a new session's first present, even with nothing dirty", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }]);
    const renderer = new RecordingRenderer();
    const { session } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    expect((await session.step()).report.presented).toBe(true);
    expect(renderer.uploads).toEqual([{ first: 0, last: 319, firstPixel: 0 }]);
    expect(renderer.draws).toHaveLength(1);
  });

  test("does not present again for a slice that changed no row and no panel state", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 } },
      { durationPs: SLICE },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    clock.ms += 100;
    await session.step();
    expect(renderer.uploads).toHaveLength(1);
  });

  test("draws a panel-state change that wrote no pixel, without uploading", async () => {
    const core = new FakeCore();
    const lit = FrameFlag.Powered | FrameFlag.DisplayOn | FrameFlag.Inverted;
    core.load([
      {
        durationPs: SLICE,
        paint: { firstRow: 0, lastRow: 319, color: 0xffff },
        panel: { backlight: 0, flags: lit },
      },
      { durationPs: SLICE, panel: { backlight: 1023 } },
      { durationPs: SLICE, panel: { flags: lit & ~FrameFlag.DisplayOn } },
      { durationPs: SLICE },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    expect(renderer.draws.at(-1)?.backlight).toBe(0);
    clock.ms += 20;
    expect((await session.step()).report.presented).toBe(true);
    expect(renderer.draws.at(-1)?.backlight).toBe(1023);
    clock.ms += 20;
    expect((await session.step()).report.presented).toBe(true);
    expect(renderer.draws.at(-1)?.displayOn).toBe(false);
    clock.ms += 20;
    expect((await session.step()).report.presented).toBe(false);
    expect(renderer.uploads).toHaveLength(1);
    expect(renderer.draws).toHaveLength(3);
  });

  test("presents the rows of a slice that ends in a stop, inside the 16 ms throttle", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 } },
      {
        durationPs: SLICE,
        paint: { firstRow: 10, lastRow: 10, color: 2 },
        stop: StopCode.Deadlock,
      },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    clock.ms += 1;
    const { outcome, report } = await session.step();
    expect(outcome.kind).toBe("stopped");
    expect(report.presented).toBe(true);
    expect(renderer.uploads.at(-1)).toMatchObject({ first: 10, last: 10 });
  });

  test("presents rows the throttle held back when the machine is paused", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 } },
      { durationPs: SLICE, paint: { firstRow: 20, lastRow: 21, color: 2 } },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    clock.ms += 1;
    expect((await session.step()).report.presented).toBe(false);
    session.setMode({ kind: "Paused" });
    const { outcome, report } = await session.step();
    expect(outcome.kind).toBe("paused");
    expect(report.presented).toBe(true);
    expect(renderer.uploads.at(-1)).toMatchObject({ first: 20, last: 21 });
  });

  test("presents at most once per 16 ms of host time", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 } },
      { durationPs: SLICE, paint: { firstRow: 1, lastRow: 1, color: 2 } },
      { durationPs: SLICE, paint: { firstRow: 2, lastRow: 2, color: 3 } },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    expect(renderer.uploads).toHaveLength(1);
    clock.ms += 1;
    await session.step();
    expect(renderer.uploads).toHaveLength(1);
    clock.ms += 20;
    await session.step();
    expect(renderer.uploads).toHaveLength(2);
  });

  // The published span is a delta, so rows of a throttled slice must survive until the next upload.
  test("uploads the union of the spans the throttle skipped, not just the last one", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, paint: { firstRow: 0, lastRow: 0, color: 1 } },
      { durationPs: SLICE, paint: { firstRow: 100, lastRow: 101, color: 2 } },
      { durationPs: SLICE, paint: { firstRow: 200, lastRow: 200, color: 3 } },
    ]);
    const renderer = new RecordingRenderer();
    const { session, clock } = sessionOn(core, renderer);
    session.setMode({ kind: "Max" });

    await session.step();
    expect(renderer.uploads[0]).toMatchObject({ first: 0, last: 319 });

    clock.ms += 1;
    await session.step();
    expect(renderer.uploads).toHaveLength(1);

    clock.ms += 20;
    await session.step();
    expect(renderer.uploads[1]).toMatchObject({ first: 100, last: 200 });

    clock.ms += 20;
    core.load([{ durationPs: SLICE, paint: { firstRow: 7, lastRow: 7, color: 4 } }]);
    await session.step();
    expect(renderer.uploads[2]).toMatchObject({ first: 7, last: 7 });
  });
});

describe("audio", () => {
  test("pumps PCM into the transport and anchors `Audio` pacing on it", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(160) } },
      { durationPs: MS },
    ]);
    const { session } = sessionOn(core);
    const ring = SharedRingTransport.create(1024);
    session.useAudio(ring);
    session.setMode({ kind: "Max" });

    await session.step();
    expect(ring.bufferedSamples()).toBe(160);

    const out = new Int16Array(80);
    ring.pull(out);
    session.setMode({ kind: "Audio", rate: 1 });
    const outcome = await session.step();
    expect(["ran", "ahead"]).toContain(outcome.outcome.kind);
  });
});

describe("Audio pacing through the pump", () => {
  test("paces on the worklet while it has audio to consume, and on the wall once the guest goes quiet", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: SLICE, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(960) } },
      { durationPs: SLICE },
      { durationPs: SLICE },
      { durationPs: SLICE },
    ]);
    const { session, clock } = sessionOn(core);
    const ring = SharedRingTransport.create(4096);
    session.useAudio(ring);
    session.setMode({ kind: "Max" });
    await session.step();

    // The worklet plays 480 of the 960 samples: the anchor is sample 479.
    ring.pull(new Int16Array(480));
    const anchor = session.audio?.consumedPs() ?? null;
    const run = session.audio?.timeOfSample(0n) ?? null;
    expect(run).not.toBeNull();
    expect(anchor).toBe((run ?? 0n) + (479n * 1_000_000_000_000n) / 16_000n);
    session.setMode({ kind: "Audio", rate: 1 });
    const { outcome } = await session.step();
    expect(session.pacing.stats().clock).toBe("audio");
    expect(outcome.kind).toBe("ran");
    if (outcome.kind === "ran") {
      expect(outcome.toPs).toBeLessThanOrEqual((anchor ?? 0n) + 60n * MS);
    }

    // It plays the rest down to the 10 ms floor; with no more PCM the wall clock takes over.
    ring.pull(new Int16Array(400));
    clock.ms += 5;
    await session.step();
    expect(session.pacing.stats()).toMatchObject({ clock: "wall", audioHandovers: 1, reanchors: 0 });
  });
});

describe("the microphone", () => {
  test("journals full chunks at the slice boundary, and the short tail and end of stream when it ends", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }, { durationPs: SLICE }, { durationPs: SLICE }]);
    const { session, clock } = sessionOn(core);
    const capture = SharedRingTransport.create(1024);
    session.startMicrophone(capture);

    clock.ms += 8;
    await session.step();
    const boundary = core.virtualTimePs;
    capture.push(Int16Array.from({ length: CHUNK_FRAMES + 2 }, (_, n) => (n % 2 === 0 ? 7 : -7)));
    clock.ms += 8;
    await session.step();

    expect(core.journaled).toHaveLength(1);
    expect(core.journaled[0]?.kind).toBe(InputKind.MicChunk);
    expect(core.journaled[0]?.atPs).toBe(boundary);
    expect(core.journaled[0]?.payload.subarray(0, 4)).toEqual(new Uint8Array([7, 0, 0xf9, 0xff]));
    expect(core.journaled[0]?.payload.length).toBe(CHUNK_FRAMES * 2);

    session.endMicrophone();
    clock.ms += 8;
    await session.step();
    expect(core.journaled).toHaveLength(2);
    expect(core.journaled[1]?.payload).toEqual(new Uint8Array([7, 0, 0xf9, 0xff]));
    expect(core.calls).toEqual(['{"cmd":"mic_set","args":{"kind":"silence"}}']);
    expect(session.liveMicrophone).toBe(false);
  });

  test("closes the part-filled chunk when the capture rate moves, and posts only real changes", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }, { durationPs: SLICE }]);
    const { session, clock } = sessionOn(core);
    const capture = SharedRingTransport.create(1024);
    session.startMicrophone(capture);
    expect(session.noteCaptureRate(null)).toBeNull();
    expect(session.noteCaptureRate(16_000)).toBe(16_000);
    capture.push(new Int16Array(100).fill(9));
    session.microphone?.drain();
    expect(session.noteCaptureRate(16_000)).toBeNull();
    expect(session.input.hasPending).toBe(false);
    expect(session.noteCaptureRate(24_000)).toBe(24_000);
    clock.ms += 8;
    await session.step();
    expect(core.journaled).toHaveLength(1);
    expect(core.journaled[0]?.payload.length).toBe(200);
  });

  test("the first live chunk continues the machine's journal count when that is ahead", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }]);
    core.journaled.push({ atPs: 0n, kind: InputKind.MicChunk, a: 6, b: 0, c: 0, payload: new Uint8Array(0) });
    const { session, clock } = sessionOn(core);
    const capture = SharedRingTransport.create(1024);
    session.startMicrophone(capture);
    capture.push(new Int16Array(CHUNK_FRAMES));
    clock.ms += 8;
    await session.step();
    expect(core.journaled.at(-1)?.a).toBe(7);
    expect(core.calls).toEqual([]);
  });

  test("chunk numbers continue across a detach and a new attach", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }, { durationPs: SLICE }]);
    const { session, clock } = sessionOn(core);
    const capture = SharedRingTransport.create(1024);
    session.startMicrophone(capture);
    capture.push(new Int16Array(CHUNK_FRAMES));
    session.endMicrophone();
    session.startMicrophone(capture);
    capture.push(new Int16Array(CHUNK_FRAMES));
    clock.ms += 8;
    await session.step();
    expect(core.journaled.map((entry) => entry.a)).toEqual([0, 1]);
    expect(session.microphone?.stats().nextSeq).toBe(2n);
  });
});

describe("the live-microphone lease", () => {
  function liveSession() {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }]);
    const parts = sessionOn(core);
    parts.session.setMode({ kind: "Max" });
    return { core, ...parts, capture: SharedRingTransport.create(1024) };
  }

  test("attaching moves Max pacing to Wall at rate 1, and skips what the ring held from before", () => {
    const { session, capture } = liveSession();
    capture.push(new Int16Array(50));
    expect(session.startMicrophone(capture)).toEqual({ kind: "Wall", rate: 1 });
    expect(capture.bufferedSamples()).toBe(0);
  });

  test("refuses Max and other Wall rates with E_LEASE, and allows Wall 1 and Audio 1", () => {
    const { session, capture } = liveSession();
    session.startMicrophone(capture);
    expect(session.setMode({ kind: "Max" })).toMatchObject({ code: "E_LEASE", holder: "live-microphone" });
    expect(session.setMode({ kind: "Wall", rate: 2 })?.code).toBe("E_LEASE");
    expect(session.pacing.currentMode).toEqual({ kind: "Wall", rate: 1 });
    expect(session.setMode({ kind: "Audio", rate: 1 })).toBeNull();
    expect(session.setMode({ kind: "Wall", rate: 1 })).toBeNull();
  });

  test("refuses a pause unless it detaches, which journals the end of stream first", () => {
    const { core, session, capture } = liveSession();
    session.startMicrophone(capture);
    capture.push(new Int16Array(10).fill(3));
    session.microphone?.drain();
    const refused = session.setMode({ kind: "Paused" });
    expect(refused?.code).toBe("E_LEASE");
    expect(refused?.message).toContain("detach");
    expect(session.liveMicrophone).toBe(true);

    expect(session.setMode({ kind: "Paused" }, { detach: true })).toBeNull();
    expect(session.liveMicrophone).toBe(false);
    expect(session.pacing.currentMode.kind).toBe("Paused");
    expect(session.input.hasPending).toBe(true);
    expect(core.calls).toEqual(['{"cmd":"mic_set","args":{"kind":"silence"}}']);
    expect(session.setMode({ kind: "Max" })).toBeNull();
  });
});

describe("registry commands", () => {
  test("go straight through the ABI's JSON cold path", () => {
    const core = new FakeCore();
    const { session } = sessionOn(core);
    expect(session.call('{"cmd":"status"}')).toBe('{"ok":true}');
    expect(core.calls).toEqual(['{"cmd":"status"}']);
  });
});

// What the session reads out of the core and journals back; the socket half is `relay.test.ts`.
describe("the Wi-Fi bridge carrier", () => {
  function bridged(): { core: FakeCore; session: EmulatorSession } {
    const core = new FakeCore();
    core.load([{ durationPs: SLICE }]);
    core.relay = {
      attached: true,
      routes: [{ port: 80, host_port: 18080 }],
      out: [new Uint8Array([1, 2, 0, 0, 0]), new Uint8Array([2, 1, 0, 0, 0])],
      dropped: 0n,
    };
    return { core, session: sessionOn(core).session };
  }

  test("a first look answers the window's end, and a cursor answers the packets after it", () => {
    const { core, session } = bridged();
    const first = session.relayWindow(null);
    expect(first.attached).toBe(true);
    expect(first.routes).toEqual([{ port: 80, hostPort: 18080 }]);
    expect(first.packets).toEqual([]);
    expect(first.cursor).toBe("2");

    core.relay.out.push(new Uint8Array([3, 1, 0, 0, 0]));
    const next = session.relayWindow(first.cursor);
    expect(next.packets.map((p) => [...p])).toEqual([[3, 1, 0, 0, 0]]);
    expect(next.cursor).toBe("3");
    expect(core.relayCursors).toEqual([null, "2"]);
    expect(session.relayWindow(first.cursor).packets.map((p) => [...p])).toEqual([[3, 1, 0, 0, 0]]);
  });

  test("server packets are journaled as NetFrame records numbered from the machine's own seq", () => {
    const { core, session } = bridged();
    expect(session.bridgeFrames([])).toBe(0);
    expect(core.journaled).toEqual([]);

    expect(session.bridgeFrames([new Uint8Array([9]), new Uint8Array([8, 7])])).toBe(2);
    expect(
      core.journaled.map((entry) => ({ kind: entry.kind, a: entry.a, b: entry.b, payload: [...entry.payload] })),
    ).toEqual([
      { kind: InputKind.NetFrame, a: 0, b: 0, payload: [9] },
      { kind: InputKind.NetFrame, a: 1, b: 0, payload: [8, 7] },
    ]);
    // The next batch continues the machine's numbering (`@live` `net_next_seq`), so no gap is noted.
    session.bridgeFrames([new Uint8Array([6])]);
    expect(core.journaled.at(-1)?.a).toBe(2);
  });

  test("a live bridge takes the clock lease, and gives it back when it ends", () => {
    const { session } = bridged();
    session.setMode({ kind: "Max" });
    expect(session.noteLiveBridge(true)).toEqual({ kind: "Wall", rate: 1 });
    expect(session.liveBridge).toBe(true);
    expect(session.setMode({ kind: "Max" })).toMatchObject({ code: "E_LEASE", holder: "wifi-bridge" });
    expect(session.setMode({ kind: "Wall", rate: 4 })?.code).toBe("E_LEASE");
    // Pausing is refused too, and `detach` does not end a bridge: `net_http --op unbridge` does.
    const paused = session.setMode({ kind: "Paused" }, { detach: true });
    expect(paused?.code).toBe("E_LEASE");
    expect(paused?.message).toContain("unbridge");
    expect(session.setMode({ kind: "Audio", rate: 1 })).toBeNull();

    session.noteLiveBridge(false);
    expect(session.liveBridge).toBe(false);
    expect(session.setMode({ kind: "Max" })).toBeNull();
  });
});
