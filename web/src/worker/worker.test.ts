// The Worker's message plumbing: every message reaches the session and nothing throws out of the
// message callback.

import { describe, expect, test } from "bun:test";

import { CoreError, WasmCore } from "./core";
import { FakeCore } from "./fakeCore";
import { ABI_VERSION, InputKind, RingId, StopCode } from "./layout";
import { PortTransport, portLike } from "../audio/transport";
import { OVERFLOW_FILL_MS } from "../audio/levels";
import type { DisplayState, PanelSink } from "../gl/sink";
import {
  PLAYBACK_RING_SAMPLES,
  bundledFirmwareUrl,
  startWorker,
  type DisplayOpener,
  type FromWorker,
  type ToWorker,
  type WorkerScope,
} from "./worker";

const MS = 1_000_000_000n;

class TestScope implements WorkerScope {
  readonly posted: FromWorker[] = [];
  onmessage: ((event: { data: ToWorker }) => void) | null = null;

  postMessage(message: FromWorker): void {
    this.posted.push(message);
  }

  send(message: ToWorker): void {
    this.onmessage?.({ data: message });
  }

  of<K extends FromWorker["type"]>(type: K): Extract<FromWorker, { type: K }>[] {
    return this.posted.filter((message) => message.type === type) as Extract<
      FromWorker,
      { type: K }
    >[];
  }
}

async function settle(): Promise<void> {
  for (let turn = 0; turn < 16; turn += 1) {
    await Promise.resolve();
  }
}

function bootOn(core: FakeCore): TestScope {
  const scope = new TestScope();
  startWorker(scope, (config) => WasmCore.build(core, config));
  scope.send({ type: "boot", config: "{}" });
  return scope;
}

describe("ring sizes", () => {
  test("the playback ring can exceed the worklet's overflow limit at 48 kHz stereo", () => {
    const limit = Math.round((OVERFLOW_FILL_MS * 48_000) / 1000) * 2;
    expect(PLAYBACK_RING_SAMPLES).toBeGreaterThan(limit);
  });
});

describe("booting", () => {
  test("answers with the ABI version the bundle was generated against", async () => {
    const scope = bootOn(new FakeCore());
    await settle();
    expect(scope.of("ready")[0]?.abiVersion).toBe(ABI_VERSION);
  });

  // A wasm instance never returns linear memory, so a reboot that skips `pemu_drop` leaks a machine.
  test("drops the previous machine before it builds the next one", async () => {
    const first = new FakeCore();
    const second = new FakeCore();
    const cores = [first, second];
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(cores.shift() ?? second, config));

    scope.send({ type: "boot", config: "{}" });
    await settle();
    expect(first.drops).toBe(0);

    scope.send({ type: "boot", config: "{}" });
    await settle();
    expect(first.drops).toBe(1);
    expect(second.drops).toBe(0);
    expect(scope.of("ready")).toHaveLength(2);
  });

  // A dropped image may replace a machine still coming up. There is one `session`, so the second
  // boot waits for the first, and each answer names its boot so the page knows which machine runs.
  test("builds one boot at a time and answers each under the name the page gave it", async () => {
    const first = new FakeCore();
    const second = new FakeCore();
    const cores = [first, second];
    const scope = new TestScope();
    const built: string[] = [];
    startWorker(scope, (config) => {
      built.push(config);
      return WasmCore.build(cores.shift() ?? second, config);
    });

    scope.send({ type: "boot", config: '{"fw":"official"}', token: "demo" });
    scope.send({ type: "boot", config: '{"fw":"pk"}', token: "load-1" });
    await settle();

    expect(built).toEqual(['{"fw":"official"}', '{"fw":"pk"}']);
    expect(scope.of("ready").map((message) => message.token)).toEqual(["demo", "load-1"]);
    expect(first.drops).toBe(1);
    expect(second.drops).toBe(0);
  });

  test("names the boot a refusal came out of, so a page waiting on a later one is not told its load failed", async () => {
    const scope = new TestScope();
    startWorker(scope, () => {
      throw new Error("the firmware bundle for `official` is not served (404)");
    });
    scope.send({ type: "boot", config: "{}", token: "demo" });
    await settle();

    expect(scope.of("error").map((message) => [message.token, message.message])).toEqual([
      ["demo", "the firmware bundle for `official` is not served (404)"],
    ]);
  });

  test("sends the playback worklet each format mark ahead of its samples", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(160) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 2, samples: new Int16Array(240) } },
      { durationPs: MS, stop: StopCode.Deadlock },
    ]);
    const channel = new MessageChannel();
    const capture = new MessageChannel();
    const seen: unknown[] = [];
    const captureSeen: unknown[] = [];
    channel.port2.onmessage = (event: MessageEvent) => {
      const data = event.data as { format?: unknown; pcm?: Int16Array };
      if (data.format) {
        seen.push({ format: data.format });
      } else if (data.pcm) {
        seen.push({ pcm: data.pcm.length });
      }
    };
    capture.port2.onmessage = (event: MessageEvent) => {
      captureSeen.push(event.data);
    };
    channel.port2.start();
    capture.port2.start();

    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(core, config));
    scope.send({ type: "boot", config: "{}", audioPort: channel.port1, capturePort: capture.port1 });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await settle();
    await new Promise((resolve) => setTimeout(resolve, 0));

    // Without isolation the format marks ride the PCM port, each ahead of the chunk it describes.
    expect(seen).toEqual([
      { format: { guestRate: 16_000, channels: 1, at: "0" } },
      { pcm: 160 },
      { format: { guestRate: 24_000, channels: 2, at: "160" } },
      { pcm: 240 },
    ]);
    expect(captureSeen).toEqual([{ guestRate: 24_000 }]);
    channel.port1.close();
    channel.port2.close();
    capture.port1.close();
    capture.port2.close();
  });

  test("journals no capture before micStart, and refuses a pause while live unless it detaches", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: MS }, { durationPs: MS, stop: StopCode.Deadlock }]);
    const capture = new MessageChannel();
    const worklet = new PortTransport(portLike(capture.port2), 4096);
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(core, config));
    scope.send({ type: "boot", config: "{}", capturePort: capture.port1 });
    await settle();
    worklet.push(new Int16Array(300).fill(1));
    await new Promise((resolve) => setTimeout(resolve, 0));
    scope.send({ type: "micStart" });
    await settle();
    scope.send({ type: "journal" });
    await settle();
    expect(JSON.parse(scope.of("journal").at(-1)?.json ?? "null").entries).toEqual([]);

    scope.send({ type: "mode", mode: { kind: "Paused" } });
    await settle();
    expect(scope.of("error").at(-1)).toMatchObject({ code: "E_LEASE" });
    scope.send({ type: "mode", mode: { kind: "Paused" }, detach: true });
    await settle();
    expect(scope.of("error")).toHaveLength(1);
    expect(core.calls).toContain('{"cmd":"mic_set","args":{"kind":"silence"}}');
    capture.port1.close();
    capture.port2.close();
  });

  test("hands the page the counters the worklets report, with the pump's own", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(16) } },
      { durationPs: MS },
      { durationPs: MS, stop: StopCode.Deadlock },
    ]);
    const channel = new MessageChannel();
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(core, config));
    scope.send({ type: "boot", config: "{}", audioPort: channel.port1 });
    await settle();
    const playback = { quanta: 64, underruns: 2, starvedQuanta: 9, overflows: 0, overflowSamples: 0 };
    channel.port2.postMessage({ playback });
    await new Promise((resolve) => setTimeout(resolve, 0));
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await settle();

    const last = scope.of("stats").at(-1);
    expect(last?.audio).toMatchObject({ pushed: "16", dropped: "0", guestRate: 16_000, playback });
    expect(last?.clock).toBe("none");
    expect(last?.frameGeneration).toMatch(/^\d+$/);
    expect(last?.draws).toBe(0);
    expect(last?.panel).toMatchObject({ backlightScale: 1024, powered: true, displayOn: true });
    channel.port1.close();
    channel.port2.close();
  });

  test("reports a failure instead of throwing out of the message callback", async () => {
    const scope = new TestScope();
    startWorker(scope, () => {
      throw new Error("no core");
    });
    scope.send({ type: "boot", config: "{}" });
    await settle();
    expect(scope.of("error")[0]?.message).toBe("no core");
  });
});

describe("a pacing loop that throws", () => {
  class ThrowingCore extends FakeCore {
    constructor(private readonly error: Error) {
      super();
    }
    override pemu_run(): number {
      throw this.error;
    }
  }

  async function fatalOf(error: Error): Promise<{ fatal: FromWorker[]; errors: FromWorker[] }> {
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(new ThrowingCore(error), config));
    scope.send({ type: "boot", config: "{}" });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await settle();
    return { fatal: scope.of("fatal"), errors: scope.of("error") };
  }

  // The page shows `fatal` as a stop; a relay or microphone refusal on `error` is not one.
  test("posts `fatal`, not `error`, with the trap's message", async () => {
    expect(await fatalOf(new Error("unreachable executed"))).toEqual({
      fatal: [{ type: "fatal", message: "unreachable executed" }],
      errors: [],
    });
  });

  test("names the registry code of a core refusal", async () => {
    const body = JSON.stringify({ code: "E_INTERNAL", number: 1, message: "the machine faulted" });
    const { fatal } = await fatalOf(new CoreError(1, body));
    expect(fatal).toEqual([{ type: "fatal", code: "E_INTERNAL", message: `pemu call failed with status 1: ${body}` }]);
  });
});

describe("the isolated loop's turn", () => {
  async function turnsWith(runsTasks: boolean): Promise<{ turns: (string | undefined)[]; asked: number }> {
    const global = globalThis as { crossOriginIsolated?: boolean };
    const was = global.crossOriginIsolated;
    global.crossOriginIsolated = true;
    try {
      let asked = 0;
      const scope = new TestScope();
      startWorker(scope, (config) => {
        const core = new FakeCore();
        core.load([{ durationPs: MS }, { durationPs: MS, stop: StopCode.Deadlock }]);
        return WasmCore.build(core, config);
      }, {
        waitAsyncTurnRunsTasks: async () => {
          asked += 1;
          return runsTasks;
        },
      });
      const turns: (string | undefined)[] = [];
      for (let boot = 0; boot < 2; boot += 1) {
        scope.send({ type: "boot", config: "{}" });
        await settle();
        scope.send({ type: "mode", mode: { kind: "Max" } });
        for (let turn = 0; turn < 8 && scope.of("stopped").length <= boot; turn += 1) {
          await new Promise((resolve) => setTimeout(resolve, 0));
          await settle();
        }
        turns.push(scope.of("stats").at(-1)?.turn);
      }
      return { turns, asked };
    } finally {
      global.crossOriginIsolated = was;
    }
  }

  // Firefox: a loop that turns only through `Atomics.waitAsync` never receives a page message.
  test("takes the MessageChannel turn where the waitAsync turn starves the Worker's tasks", async () => {
    expect(await turnsWith(false)).toEqual({ turns: ["message-channel", "message-channel"], asked: 1 });
  });

  test("keeps the waitAsync turn where it lets the Worker's tasks run", async () => {
    expect(await turnsWith(true)).toEqual({ turns: ["wait-async", "wait-async"], asked: 1 });
  });
});

describe("driving a booted machine", () => {
  test("runs to a stop and reports the serial, the stats and the stop", async () => {
    const core = new FakeCore();
    core.load([
      { durationPs: MS, serial: "hello\n", uart0: "rst\n" },
      { durationPs: MS, stop: StopCode.Deadlock },
    ]);
    const scope = bootOn(core);
    await settle();

    scope.send({ type: "mode", mode: { kind: "Max" } });
    await settle();

    const streams = scope.of("serial");
    expect(new TextDecoder().decode(streams[0]?.bytes)).toBe("hello\n");
    expect(streams[0]?.stream).toBe(RingId.UsjTx);
    expect(streams[0]?.lines).toEqual([{ offset: "5", vtPs: MS.toString() }]);
    expect(streams[0]?.dropped).toBe("0");
    expect(new TextDecoder().decode(streams[1]?.bytes)).toBe("rst\n");
    expect(streams[1]?.stream).toBe(RingId.Uart0Tx);
    expect(scope.of("stats").length).toBeGreaterThan(0);
    expect(scope.of("stopped")[0]?.stop).toBe(StopCode.Deadlock);
    expect(scope.of("stopped")[0]?.json).toContain(String(StopCode.Deadlock));
  });

  test("queues input and journals it with the next slice", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: 8n * MS }, { durationPs: 8n * MS, stop: StopCode.Deadlock }]);
    const scope = bootOn(core);
    await settle();

    scope.send({ type: "power", down: true });
    scope.send({ type: "serial", channel: 0, text: "q" });
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await settle();

    expect(core.journaled.map((entry) => entry.kind)).toEqual([
      InputKind.Power,
      InputKind.SerialIn,
    ]);
  });

  test("refuses a pacing rate outside the accepted range and leaves the machine paused", async () => {
    const core = new FakeCore();
    core.load([{ durationPs: 1_000_000_000n }]);
    const scope = bootOn(core);
    await settle();

    for (const rate of [0, 1e-9, Number.NaN]) {
      scope.send({ type: "mode", mode: { kind: "Wall", rate } });
      await settle();
      expect(scope.of("error").at(-1)).toMatchObject({ code: "E_USAGE" });
    }
    expect(scope.of("stats")).toHaveLength(0);
  });

  test("answers a registry command and exports the journal", async () => {
    const core = new FakeCore();
    const scope = bootOn(core);
    await settle();

    scope.send({ type: "call", id: 4, request: '{"cmd":"status"}' });
    scope.send({ type: "journal" });
    await settle();

    expect(scope.of("call")[0]).toEqual({ type: "call", id: 4, ok: '{"ok":true}' });
    expect(JSON.parse(scope.of("journal")[0]?.json ?? "null")).toMatchObject({
      abiVersion: ABI_VERSION,
      entries: [],
    });
  });

  test("answers a refused journal export with a journal reply carrying the ApiError", async () => {
    const fake = new FakeCore();
    const scope = new TestScope();
    startWorker(scope, (config) => {
      const core = WasmCore.build(fake, config);
      const refusing = Object.create(core) as typeof core;
      refusing.call = () => {
        throw new CoreError(2, '{"code":"E_STATE","message":"a restore rewound the journal"}');
      };
      return refusing;
    });
    scope.send({ type: "boot", config: "{}" });
    await settle();
    scope.send({ type: "journal" });
    await settle();
    expect(scope.of("journal")[0]).toEqual({
      type: "journal",
      json: "null",
      err: '{"code":"E_STATE","message":"a restore rewound the journal"}',
    });
    expect(scope.of("error")).toHaveLength(0);
  });

  test("answers a registry refusal as the call's error, with the ApiError body", async () => {
    const fake = new FakeCore();
    const scope = new TestScope();
    startWorker(scope, (config) => {
      const core = WasmCore.build(fake, config);
      const refusing = Object.create(core) as typeof core;
      refusing.call = () => {
        throw new CoreError(2, '{"code":"E_STATE","message":"no instance"}');
      };
      return refusing;
    });
    scope.send({ type: "boot", config: "{}" });
    await settle();
    scope.send({ type: "call", id: 9, request: '{"cmd":"status"}' });
    await settle();
    expect(scope.of("call")[0]).toEqual({
      type: "call",
      id: 9,
      err: '{"code":"E_STATE","message":"no instance"}',
    });
    expect(scope.of("error")).toEqual([]);
  });

  test("hands the loader the dropped image, and names a bundled firmware by corpus id only", async () => {
    const seen: (Uint8Array | undefined)[] = [];
    const scope = new TestScope();
    startWorker(scope, (config, image) => {
      seen.push(image);
      return WasmCore.build(new FakeCore(), config);
    });
    const image = Uint8Array.of(0xe9, 0);
    scope.send({ type: "boot", config: '{"fw":"mine"}', image });
    await settle();
    expect(seen).toEqual([image]);

    const base = "https://example.test/emu/worker.js";
    expect(bundledFirmwareUrl("official", base)).toBe("https://example.test/emu/official.pebundle");
    expect(bundledFirmwareUrl("../secret", base)).toBeNull();
    expect(bundledFirmwareUrl("C:\\fw.bin", base)).toBeNull();
    expect(bundledFirmwareUrl("", base)).toBeNull();
  });

  test("refuses a command before a machine exists rather than failing silently", async () => {
    const scope = new TestScope();
    startWorker(scope, () => WasmCore.build(new FakeCore(), "{}"));
    scope.send({ type: "call", id: 1, request: "{}" });
    await settle();
    const body = JSON.parse(scope.of("call")[0]?.err ?? "null");
    expect(body).toEqual({
      backtrace: [],
      code: "E_STATE",
      detail: null,
      hint: "boot a firmware before calling a command",
      message: "no machine is booted",
      number: 2,
      retryable: false,
      serial_tail: [],
      vt_us: 0,
    });
  });

  test("refuses to attach before a machine exists", async () => {
    const scope = new TestScope();
    startWorker(scope, () => WasmCore.build(new FakeCore(), "{}"));
    scope.send({ type: "attach", port: 8765 });
    await settle();
    expect(scope.of("error")[0]?.message).toBe("attach needs a booted machine");
  });

  // The page attaches after its first machine is up; a firmware dropped meanwhile reboots the
  // Worker, and a refusal of the attach carries no token, so it would read as the drop's failure.
  test("an attach that arrives while a boot replaces the machine waits for the new one", async () => {
    const scope = new TestScope();
    let release: () => void = () => {};
    const slow = new Promise<void>((resolve) => {
      release = resolve;
    });
    let boots = 0;
    startWorker(scope, async (config) => {
      boots += 1;
      if (boots === 2) {
        await slow;
      }
      return WasmCore.build(new FakeCore(), config);
    });
    scope.send({ type: "boot", config: "{}", token: "demo" });
    await settle();
    scope.send({ type: "boot", config: "{}", token: "load-1" });
    await settle();
    scope.send({ type: "attach", port: 1, label: "web ui" });
    await settle();
    release();
    await settle();
    expect(scope.of("error")).toEqual([]);
    expect(scope.of("ready").map((message) => message.token)).toEqual(["demo", "load-1"]);
  });
});

class FakeDisplays {
  readonly opened: {
    canvas: unknown;
    uploads: { first: number; last: number }[];
    draws: number;
    sizes: [number, number][];
    onState: (state: DisplayState) => void;
  }[] = [];

  readonly open: DisplayOpener = (canvas, _width, _height, onState) => {
    const record = {
      canvas: canvas as unknown,
      uploads: [] as { first: number; last: number }[],
      draws: 0,
      sizes: [] as [number, number][],
      onState,
    };
    this.opened.push(record);
    const sink: PanelSink = {
      backend: "webgl1",
      upload: (_pixels, first, last) => {
        record.uploads.push({ first, last });
      },
      draw: (_panel, canvasWidth, canvasHeight) => {
        record.draws += 1;
        record.sizes.push([canvasWidth, canvasHeight]);
        return { scale: 1, x: 0, y: 0, glY: 0, width: 240, height: 320 };
      },
    };
    return { sink, state: { backend: "webgl1", reason: null, contextLost: false } };
  };
}

function paintThenStop(core: FakeCore): void {
  core.load([
    { durationPs: 8n * MS, paint: { firstRow: 3, lastRow: 4, color: 0xf800 } },
    { durationPs: 8n * MS, stop: StopCode.Deadlock },
  ]);
}

describe("the raw frame", () => {
  test("answers a copy of the frame buffer with its generation, and null before a boot", async () => {
    const empty = new TestScope();
    startWorker(empty, (config) => WasmCore.build(new FakeCore(), config));
    empty.send({ type: "frame" });
    expect(empty.of("frame")[0]).toEqual({ type: "frame", width: 0, height: 0, generation: "0", pixels: null });

    const core = new FakeCore();
    paintThenStop(core);
    const scope = bootOn(core);
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    for (let turn = 0; turn < 4 && scope.of("stopped").length === 0; turn += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
      await settle();
    }
    scope.send({ type: "frame" });
    const frame = scope.of("frame")[0];
    expect(frame?.generation).toBe("1");
    expect(frame?.pixels?.length).toBe((frame?.width ?? 0) * (frame?.height ?? 0));
    const row = frame?.width ?? 0;
    expect(frame?.pixels?.[3 * row]).toBe(0xf800);
    expect(frame?.pixels?.[5 * row]).not.toBe(0xf800);
  });
});

describe("the display", () => {
  const canvasA = { id: "a" } as unknown as OffscreenCanvas;
  const canvasB = { id: "b" } as unknown as OffscreenCanvas;

  test("posts the display state after ready, and names a boot without a canvas", async () => {
    const displays = new FakeDisplays();
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(new FakeCore(), config), {
      openDisplay: displays.open,
    });
    scope.send({ type: "boot", config: "{}", canvas: canvasA });
    await settle();
    expect(scope.posted.map((message) => message.type)).toEqual(["ready", "display"]);
    expect(scope.of("display")[0]).toEqual({
      type: "display",
      backend: "webgl1",
      reason: null,
      contextLost: false,
    });

    displays.opened[0]?.onState({ backend: "webgl1", reason: null, contextLost: true });
    expect(scope.of("display").at(-1)?.contextLost).toBe(true);

    const bare = new TestScope();
    startWorker(bare, (config) => WasmCore.build(new FakeCore(), config), {
      openDisplay: displays.open,
    });
    bare.send({ type: "boot", config: "{}" });
    await settle();
    expect(bare.of("display")[0]?.backend).toBe("none");
    expect(bare.of("display")[0]?.reason).toContain("no OffscreenCanvas");
  });

  test("a reboot without a canvas keeps the display and repaints it whole", async () => {
    const displays = new FakeDisplays();
    const cores = [new FakeCore(), new FakeCore()];
    for (const core of cores) paintThenStop(core);
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(cores.shift() ?? new FakeCore(), config), {
      openDisplay: displays.open,
    });
    scope.send({ type: "boot", config: "{}", canvas: canvasA });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await new Promise((resolve) => setTimeout(resolve, 10));

    scope.send({ type: "boot", config: "{}" });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(displays.opened).toHaveLength(1);
    expect(scope.of("display").at(-1)?.backend).toBe("webgl1");
    expect(displays.opened[0]?.uploads).toEqual([
      { first: 0, last: 319 },
      { first: 0, last: 319 },
    ]);
  });

  // The canvas can be transferred only once, so a failed boot must not take it down.
  test("a boot that fails keeps its canvas for the next boot", async () => {
    const displays = new FakeDisplays();
    const core = new FakeCore();
    paintThenStop(core);
    let loads = 0;
    const scope = new TestScope();
    startWorker(
      scope,
      (config) => {
        loads += 1;
        return loads === 1
          ? Promise.reject(new Error("the firmware bundle for `official` is not served (404)"))
          : WasmCore.build(core, config);
      },
      { openDisplay: displays.open },
    );
    scope.send({ type: "boot", config: "{}", canvas: canvasA, token: "demo" });
    await settle();
    expect(scope.of("ready")).toEqual([]);
    expect(scope.of("error")[0]?.message).toContain("not served");

    scope.send({ type: "boot", config: "{}", token: "load-1" });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(scope.of("ready").map((message) => message.token)).toEqual(["load-1"]);
    expect(displays.opened.map((record) => record.canvas)).toEqual([canvasA]);
    expect(displays.opened[0]?.uploads[0]).toEqual({ first: 0, last: 319 });
    expect(scope.of("display").at(-1)?.backend).toBe("webgl1");
  });

  test("draws at the transferred canvas's own size, not the canvasSize the page sent", async () => {
    const displays = new FakeDisplays();
    const core = new FakeCore();
    paintThenStop(core);
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(core, config), { openDisplay: displays.open });
    const canvas = { width: 480, height: 640 } as unknown as OffscreenCanvas;
    scope.send({ type: "boot", config: "{}", canvas, canvasSize: { width: 240, height: 320 } });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(displays.opened[0]?.sizes[0]).toEqual([480, 640]);
  });

  test("a boot with a second canvas reopens the display on it and ignores the old one", async () => {
    const displays = new FakeDisplays();
    const core = new FakeCore();
    paintThenStop(core);
    const scope = new TestScope();
    startWorker(scope, (config) => WasmCore.build(core, config), { openDisplay: displays.open });
    scope.send({ type: "boot", config: "{}", canvas: canvasA });
    await settle();
    scope.send({ type: "boot", config: "{}", canvas: canvasB });
    await settle();
    scope.send({ type: "mode", mode: { kind: "Max" } });
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(displays.opened.map((record) => record.canvas)).toEqual([canvasA, canvasB]);
    expect(displays.opened[0]?.uploads).toEqual([]);
    expect(displays.opened[1]?.uploads[0]).toEqual({ first: 0, last: 319 });

    const before = scope.of("display").length;
    displays.opened[0]?.onState({ backend: "webgl1", reason: null, contextLost: true });
    expect(scope.of("display")).toHaveLength(before);
  });
});
