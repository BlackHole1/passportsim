import { describe, expect, test } from "bun:test";

import { FakeCore } from "../worker/fakeCore";
import { IoViews } from "../worker/ring";
import { PcmPump } from "./playback";
import type { AudioTraceEntry } from "./trace";
import { SharedRingTransport } from "./transport";
import { PlaybackEngine, RENDER_QUANTUM } from "./worklet";

const MS = 1_000_000_000n;

function pumpOf(steps: { durationPs: bigint; pcm?: { fs: number; channels: number; samples: Int16Array } }[]) {
  const core = new FakeCore();
  const views = IoViews.of(core.memory, () => core.pemu_io_layout(1));
  const ring = SharedRingTransport.create(16_000);
  core.load(steps);
  return { core, views, ring, pump: new PcmPump(views, ring) };
}

describe("the pump's trace", () => {
  test("names the record and the burst of zeros it pushed, and closes the burst when the guest goes quiet", () => {
    const { core, views, pump } = pumpOf([
      { durationPs: MS, pcm: { fs: 16_000, channels: 2, samples: new Int16Array(32) } },
      { durationPs: 100n * MS },
    ]);
    const seen: AudioTraceEntry[] = [];
    pump.setTrace((entry) => seen.push(entry));
    core.pemu_run(1, MS, 1n << 40n);
    views.sync();
    pump.pump(core.virtualTimePs);
    core.pemu_run(1, 100n * MS, 1n << 40n);
    views.sync();
    pump.pump(core.virtualTimePs);

    expect(seen.map((entry) => `${entry.src}:${entry.kind}`)).toEqual(["pump:record", "pump:burst", "pump:burst"]);
    expect(seen[1]).toMatchObject({ kind: "burst", state: "open", transportFirst: "0", samples: 32, zeros: 32, peak: 0, fs: 16_000, channels: 2 });
    expect(seen[2]).toMatchObject({ kind: "burst", state: "closed", samples: 32, zeros: 32 });
  });
});

describe("the worklet's trace", () => {
  test("reports the start of playing and the underrun it counts, in transport cursors", () => {
    const ring = SharedRingTransport.create(16_000);
    const seen: AudioTraceEntry[] = [];
    const playback = new PlaybackEngine(ring, 48_000, { guestRate: 16_000, trace: (entry) => seen.push(entry) });
    ring.push(new Int16Array(1200).fill(1000));
    for (let quantum = 0; quantum < 200 && playback.counters().underruns === 0; quantum += 1) {
      ring.push(new Int16Array(10).fill(1000));
      playback.render(new Float32Array(RENDER_QUANTUM));
    }
    const kinds = seen.filter((entry) => entry.src === "worklet" && entry.kind !== "level").map((entry) => entry.kind);
    expect(kinds).toEqual(["first-samples", "playing", "underrun"]);
    const underrun = seen.find((entry) => entry.kind === "underrun");
    expect(underrun).toMatchObject({ src: "worklet", underruns: 1 });
    expect(BigInt((underrun as { consumed: string }).consumed)).toBe(ring.consumedSamples());
  });
});
