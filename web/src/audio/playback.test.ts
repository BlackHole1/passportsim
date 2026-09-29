import { describe, expect, test } from "bun:test";

import { FakeCore } from "../worker/fakeCore";
import { IoViews } from "../worker/ring";
import { PcmPump } from "./playback";
import { FORMAT_SLOTS, SharedRingTransport } from "./transport";

const MS = 1_000_000_000n;

function marks(ring: SharedRingTransport): { guestRate: number; channels: number; at: bigint }[] {
  const out = [];
  for (let mark = ring.takeFormat(); mark !== null; mark = ring.takeFormat()) {
    out.push(mark);
  }
  return out;
}
const S = 1_000_000_000_000n;

function parts(capacity = 4096): { core: FakeCore; views: IoViews; ring: SharedRingTransport } {
  const core = new FakeCore();
  const views = IoViews.of(core.memory, () => core.pemu_io_layout(1));
  return { core, views, ring: SharedRingTransport.create(capacity) };
}

describe("the pump", () => {
  test("moves everything the guest produced, once", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: Int16Array.from([1, 2, 3]) } },
    ]);
    const pump = new PcmPump(views, ring);

    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(pump.stats().pushed).toBe(3n);
    expect(ring.bufferedSamples()).toBe(3);

    pump.pump();
    expect(pump.stats().pushed).toBe(3n);
  });

  test("counts what the transport refused instead of blocking the guest", () => {
    const { core, views, ring } = parts(2);
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: Int16Array.from([1, 2, 3, 4]) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();

    expect(pump.stats().pushed).toBe(2n);
    expect(pump.stats().dropped).toBe(2n);
  });
});

describe("the audio anchor", () => {
  test("is null until the worklet has consumed a sample", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(16) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(pump.consumedPs()).toBeNull();
  });

  test("is the sample-exact virtual time of the last consumed sample", () => {
    const { core, views, ring } = parts();
    // A run of 16 samples at 16 kHz starting at 1 ms: sample n is at 1 ms + n / 16000 s.
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(16) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();

    const out = new Int16Array(8);
    ring.pull(out);
    expect(pump.consumedPs()).toBe(MS + (7n * S) / 16_000n);
  });

  test("follows a rate change, so the anchor is right across an I2S reconfiguration", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(4) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();

    const out = new Int16Array(6);
    ring.pull(out);
    // Sample 5 is the second of the 24 kHz run, which started at 2 ms.
    expect(pump.consumedPs()).toBe(2n * MS + S / 24_000n);
  });

  // `consumedPs` asks about sample `consumed - 1`, so the record that owns it must survive the exact
  // boundary, or `Audio` pacing silently falls back to the host clock.
  test("survives an exact record boundary, where the anchor is the older run's last sample", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(4) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();

    // Exactly the first run: the anchor is sample 3, the last of the 16 kHz run.
    ring.pull(new Int16Array(4));
    expect(pump.consumedPs()).toBe(MS + (3n * S) / 16_000n);

    pump.pump();
    expect(pump.consumedPs()).toBe(MS + (3n * S) / 16_000n);
  });
});

describe("the guest rate", () => {
  test("is null before the first record and then follows the newest one", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(4) } },
    ]);
    const pump = new PcmPump(views, ring);
    expect(pump.currentRate).toBeNull();

    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(pump.currentRate).toBe(16_000);
    expect(pump.stats().guestRate).toBe(16_000);

    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(pump.currentRate).toBe(24_000);
  });

  test("holds the last rate through a slice that produced no record", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    pump.pump();
    expect(pump.currentRate).toBe(24_000);
  });
});

describe("the two numberings", () => {
  test("the anchor stays sample-exact after the transport dropped samples it had no room for", () => {
    const { core, views } = parts();
    const ring = SharedRingTransport.create(4);
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(6) } },
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    // Ring samples 0..3 went in, 4 and 5 were dropped.
    expect(pump.stats().dropped).toBe(2n);
    ring.pull(new Int16Array(4));

    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    // Transport samples 4..7 are ring samples 6..9, the second run at 2 ms.
    ring.pull(new Int16Array(2));
    // Transport sample 5 is ring sample 7, the second of the run.
    expect(pump.consumedPs()).toBe(2n * MS + S / 16_000n);
  });

  test("a stereo run is timed per frame, not per sample", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 24_000, channels: 2, samples: new Int16Array(8) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    ring.pull(new Int16Array(6));
    // Sample 5 is the right slot of frame 2.
    expect(pump.consumedPs()).toBe(MS + (2n * S) / 24_000n);
  });
});

describe("format changes for the worklet", () => {
  test("are placed on the transport cursor of the first sample of the new format", () => {
    const { core, views, ring } = parts();
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 2, samples: new Int16Array(6) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 3n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    // The second 16 kHz record repeats the format and is not a change.
    expect(marks(ring)).toEqual([
      { guestRate: 16_000, channels: 1, at: 0n },
      { guestRate: 24_000, channels: 2, at: 8n },
    ]);
    expect(marks(ring)).toEqual([]);
  });

  test("a change whose first samples were dropped starts at the next sample pushed", () => {
    const { core, views } = parts();
    const ring = SharedRingTransport.create(4);
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(2) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(marks(ring)).toEqual([{ guestRate: 16_000, channels: 1, at: 0n }]);
    ring.pull(new Int16Array(4));
    core.load([{ durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(2) } }]);
    core.pemu_run(1, 3n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(marks(ring)).toEqual([{ guestRate: 24_000, channels: 1, at: 4n }]);
  });
});

describe("whether audio is flowing", () => {
  test("needs at least the worklet's 10 ms underrun floor buffered", () => {
    const { core, views, ring } = parts(8192);
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(200) } },
    ]);
    const pump = new PcmPump(views, ring);
    expect(pump.flowing()).toBe(false);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(pump.flowing()).toBe(true);
    // 160 samples is 10 ms at 16 kHz; 41 left is below it.
    ring.pull(new Int16Array(159));
    expect(pump.flowing()).toBe(false);
  });
});

describe("whole frames per run", () => {
  test("a full transport cuts each run to its own frame width: 3 mono then 6 stereo into 8 free", () => {
    const { core, views } = parts();
    const ring = SharedRingTransport.create(8);
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: Int16Array.from([1, 2, 3]) } },
      {
        durationPs: MS,
        pcm: { fs: 24_000, channels: 2, samples: Int16Array.from([10, -10, 11, -11, 12, -12]) },
      },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    // The mono run fits; 5 slots remain, which is two stereo frames and not two and a half.
    expect(pump.stats()).toMatchObject({ pushed: 7n, dropped: 2n });
    const out = new Int16Array(8);
    expect(ring.pull(out)).toBe(7);
    expect(Array.from(out.subarray(0, 7))).toEqual([1, 2, 3, 10, -10, 11, -11]);
    expect(marks(ring)).toEqual([
      { guestRate: 16_000, channels: 1, at: 0n },
      { guestRate: 24_000, channels: 2, at: 3n },
    ]);
  });

  test("a reader the core overran part-way through a stereo frame re-aligns on the next frame", () => {
    const core = new FakeCore({ pcmSamples: 8 });
    const views = IoViews.of(core.memory, () => core.pemu_io_layout(1));
    const ring = SharedRingTransport.create(64);
    core.load([
      {
        durationPs: MS,
        pcm: { fs: 24_000, channels: 2, samples: Int16Array.from([1, -1, 2, -2, 3, -3, 4, -4, 5, -5]) },
      },
      { durationPs: MS, pcm: { fs: 24_000, channels: 2, samples: Int16Array.from([6, -6, 7]) } },
    ]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    // 13 samples into an 8-slot ring: the tail is sample 5, the right slot of frame 2 (-3).
    const out = new Int16Array(16);
    const taken = ring.pull(out);
    expect(Array.from(out.subarray(0, taken))).toEqual([4, -4, 5, -5, 6, -6, 7]);
    expect(pump.stats().lost).toBe(6n);
  });
});

describe("bounded headers", () => {
  test("a suspended worklet does not make the pump keep a header per slice", () => {
    const { core, views } = parts();
    const ring = SharedRingTransport.create(4);
    const slices = Array.from({ length: 200 }, () => ({
      durationPs: MS,
      pcm: { fs: 16_000, channels: 1, samples: new Int16Array(4) },
    }));
    core.load(slices);
    const pump = new PcmPump(views, ring);
    for (let slice = 1; slice <= 200; slice += 1) {
      core.pemu_run(1, BigInt(slice) * MS, 1n << 40n);
      views.sync();
      pump.pump();
    }
    // Only the first run was pushed; nothing is ever consumed.
    expect(pump.stats()).toMatchObject({ pushed: 4n, dropped: 796n });
    expect(pump.keptRecords).toBeLessThanOrEqual(2);
    // The anchor still resolves once the worklet wakes up.
    ring.pull(new Int16Array(4));
    expect(pump.timeOfSample(3n)).not.toBeNull();
    expect(pump.consumedPs()).toBe(MS + (3n * S) / 16_000n);
  });
});

describe("format marks go in before their samples", () => {
  test("the pump calls pushFormat before it pushes the run the mark describes", () => {
    const { core, views } = parts();
    const ring = SharedRingTransport.create(1024);
    const calls: string[] = [];
    const recording = new Proxy(ring, {
      get(target, key, receiver) {
        const value = Reflect.get(target, key, receiver);
        if ((key === "push" || key === "pushFormat") && typeof value === "function") {
          return (arg: never) => {
            calls.push(key === "push" ? `push ${(arg as Int16Array).length}` : `mark ${(arg as { at: bigint }).at}`);
            return (value as (a: never) => unknown).call(target, arg);
          };
        }
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    core.load([
      { durationPs: MS, pcm: { fs: 16_000, channels: 1, samples: new Int16Array(3) } },
      { durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(5) } },
    ]);
    const pump = new PcmPump(views, recording);
    core.pemu_run(1, 2n * MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(calls).toEqual(["mark 0", "push 3", "mark 3", "push 5"]);
  });

  test("a run waits when the transport has no room for its mark", () => {
    const { core, views } = parts();
    const ring = SharedRingTransport.create(1024);
    for (let slot = 0; slot < FORMAT_SLOTS; slot += 1) {
      ring.pushFormat({ guestRate: 16_000, channels: 1, at: 0n });
    }
    core.load([{ durationPs: MS, pcm: { fs: 24_000, channels: 1, samples: new Int16Array(5) } }]);
    const pump = new PcmPump(views, ring);
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump();
    expect(pump.stats()).toMatchObject({ pushed: 0n, dropped: 5n });
  });
});
