// The audio path in a real browser: the built `worklet.js` in a real AudioWorklet, over the shared
// ring (isolated page) and the transferred MessagePort (not isolated). Playback is synthetic PCM
// measured through an AnalyserNode; capture is Chromium's fake microphone. The guest is not in the
// loop.
//
// Skips: `fake-media-chromium-only` for the capture tests on WebKit (it has no fake capture
// device); the install hint of `browsers.ts` for a missing browser; `bun-not-on-path` when the
// probe cannot be bundled; `no-audio-output` for the playback tests when the browser starts no
// audio clock (Firefox on a hosted Windows runner, which has no audio device); `host-not-real-time`
// for the underrun and overflow counts and the pitch of the rate-change test when the host's audio
// clock or timers did not keep to the wall clock (a hosted CI runner with no audio device), measured
// by `realTimeGap`.

import { expect, test as base, type Page } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { OVERFLOW_FILL_MS, UNDERRUN_FILL_MS } from "../src/audio/levels";
import { browserGaps } from "./harness";
import { descendants, holdFullSpeed, processTable } from "./processCpu";
import { serveDir, type StaticServer } from "./staticServer";
import type { CaptureResult, PlaybackResult, PlaybackSkip, ToneSegment } from "./audioProbe";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");

/** The fake-media switches go to Chromium only; WebKit would refuse to launch with them. */
const test = base.extend<object, object>({
  launchOptions: [
    async ({ browserName }, use) => {
      await use(
        browserName === "chromium"
          ? {
              args: [
                "--use-fake-device-for-media-stream",
                "--use-fake-ui-for-media-stream",
                "--autoplay-policy=no-user-gesture-required",
              ],
            }
          : {},
      );
    },
    { scope: "worker" },
  ],
});

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function bundleProbe(): { code: string } | { skip: string } {
  const out = mkdtempSync(join(tmpdir(), "pemu-audio-probe-"));
  const built = spawnSync("bun", ["build", "tests/audioProbe.ts", "--outdir", out, "--target", "browser"], {
    cwd: WEB,
    encoding: "utf8",
  });
  if (built.error) {
    return { skip: `bun-not-on-path: ${built.error.message}` };
  }
  if (built.status !== 0) {
    throw new Error(`bundling the audio probe failed: ${built.stderr}`);
  }
  return { code: readFileSync(join(out, "audioProbe.js"), "utf8") };
}

let probe: { code: string } | { skip: string } | null = null;

let served: StaticServer | null = null;

test.afterEach(async () => {
  await served?.close();
  served = null;
});

/**
 * Serves a blank page with the probe, isolated or not, from a loopback server over `dist/`. Not
 * `page.route` fulfilment: Playwright WebKit ignores a fulfilled response's isolation headers.
 */
async function openProbe(page: Page, isolated: boolean): Promise<void> {
  probe ??= bundleProbe();
  if ("skip" in probe) {
    test.skip(true, probe.skip);
    return;
  }
  // The probe page sits beside the built `worklet.js`, so `workletUrl` resolves to it.
  served = await serveDir(join(WEB, "dist"), isolated, {
    "__audio_probe.html":
      '<!doctype html><meta charset="utf-8"><button id="go">go</button><script type="module" src="/__audio_probe.js"></script>',
    "__audio_probe.js": probe.code,
  });
  await page.goto(`${served.url}__audio_probe.html`);
  await page.waitForFunction(() => typeof (globalThis as { audioProbe?: unknown }).audioProbe === "object");
  // A user gesture, for engines whose AudioContext starts suspended until one.
  await page.click("#go");
  expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(isolated);
  // As `harness.ts` `openPage`: Windows 11 runs a browser with no visible window under EcoQoS,
  // whose late timers would starve the ring.
  if (process.platform === "win32") {
    holdFullSpeed(descendants(processTable(), process.pid));
  }
}

/** Plays `segments` in the probe and measures at `at`; skips the test when no audio clock runs. */
async function play(page: Page, segments: ToneSegment[], at: number[]): Promise<PlaybackResult> {
  const result = (await page.evaluate(
    ([s, a]) =>
      (
        globalThis as unknown as {
          audioProbe: { playback(s: unknown, a: unknown): Promise<unknown> };
        }
      ).audioProbe.playback(s, a),
    [segments, at] as const,
  )) as PlaybackResult | PlaybackSkip;
  if ("skip" in result) {
    test.skip(true, result.skip);
  }
  return result as PlaybackResult;
}

const TONE_RMS = 8_000 / 32_768 / Math.SQRT2;

function expectTone(heard: PlaybackResult["heard"][number] | undefined, hz: number): void {
  expect(heard, "a measurement at this instant").toBeDefined();
  // Zero crossings over a 4096-sample window resolve a few hertz; 3 % covers it.
  expect(Math.abs((heard?.hz ?? 0) - hz) / hz, `heard ${heard?.hz} Hz, wanted ${hz}`).toBeLessThan(0.03);
  expect(Math.abs((heard?.rms ?? 0) - TONE_RMS) / TONE_RMS, `rms ${heard?.rms}`).toBeLessThan(0.1);
}

/** What the fill check allows for sampling the clock between device callbacks. */
const FILL_MARGIN_MS = 10;

/**
 * Why the underrun and overflow counts of `result` measure the host rather than the worklet, or
 * null. They hold only while the ring stays between the worklet's floor and ceiling, and the fill
 * that the pushes and the audio clock imply leaves that band only when the page's timers or the
 * audio clock ran late or in bursts, as on a hosted runner with no audio device and shared CPUs.
 */
function realTimeGap(result: PlaybackResult): string | null {
  const { lowMs, highMs } = result.impliedFill;
  const floor = UNDERRUN_FILL_MS + FILL_MARGIN_MS;
  const ceiling = OVERFLOW_FILL_MS - FILL_MARGIN_MS;
  if (lowMs >= floor && highMs <= ceiling) {
    return null;
  }
  const ms = (value: number) => `${Math.round(value)} ms`;
  return (
    `host-not-real-time: the pushes and the audio clock put the ring between ${ms(lowMs)} and ${ms(highMs)}, ` +
    `outside ${ms(floor)} to ${ms(ceiling)} (the pacing keeps 100 to 120 ms), ` +
    `so the counters ${JSON.stringify(result.counters)} measure the host, not the worklet`
  );
}

for (const isolated of [true, false]) {
  const transport = isolated ? "the shared ring" : "the transferred MessagePort";

  test(`playback over ${transport} plays a 24 kHz tone at its pitch and level, in real time`, async ({ page }) => {
    await openProbe(page, isolated);
    const result = await play(page, [{ rate: 24_000, channels: 1, hz: 440, ms: 600 }], [400]);

    expect(result.isolated).toBe(isolated);
    expect(result.pushed).toBe(14_400);
    expectTone(result.heard[0], 440);
    // Everything was played, the tail under the 10 ms floor included.
    expect(result.consumedEnd).toBe("14400");
    const gap = realTimeGap(result);
    test.skip(gap !== null, gap ?? "");
    expect(result.counters?.underruns).toBe(1);
    expect(result.counters?.overflows).toBe(0);
  });

  test(`playback over ${transport} plays the left slot of a stereo stream`, async ({ page }) => {
    await openProbe(page, isolated);
    const result = await play(page, [{ rate: 24_000, channels: 2, hz: 660, ms: 600 }], [400]);
    // The right slot is a 3 kHz tone at more than twice the level; none of it may be heard.
    expectTone(result.heard[0], 660);
    expect(result.consumedEnd).toBe(String(2 * 14_400));
  });

  test(`playback over ${transport} follows a rate change inside one stream`, async ({ page }) => {
    await openProbe(page, isolated);
    const result = await play(
      page,
      [
        { rate: 16_000, channels: 1, hz: 440, ms: 700 },
        { rate: 24_000, channels: 1, hz: 880, ms: 700 },
      ],
      [450, 1_150],
    );
    expect(result.counters?.straySamples).toBe(0);
    // An underrun or overflow in a window puts a gap or a jump in its zero crossings.
    const gap = realTimeGap(result);
    test.skip(gap !== null, gap ?? "");
    // Played at the wrong rate, the second segment would sound at 587 Hz (880 * 16 / 24).
    expectTone(result.heard[0], 440);
    expectTone(result.heard[1], 880);
  });

  test(`capture over ${transport} journals 240-frame chunks of the fake microphone at 16 kHz`, async ({
    page,
    browserName,
  }) => {
    test.skip(browserName !== "chromium", "fake-media-chromium-only");
    await openProbe(page, isolated);
    const result = (await page.evaluate(() =>
      (globalThis as unknown as { audioProbe: { capture(): Promise<unknown> } }).audioProbe.capture(),
    )) as CaptureResult;

    expect(result.started).toMatchObject({ ok: true });
    // 1.5 s at 16 kHz is 100 chunks; allow for the graph starting late.
    expect(result.chunks.length).toBeGreaterThan(40);
    result.chunks.forEach((chunk, index) => {
      expect(chunk.seq).toBe(String(index));
      if (index < result.chunks.length - 1) {
        expect(chunk.length).toBe(240);
      }
    });
    expect(result.peak).toBeGreaterThan(0);
    expect(result.counters?.dropped).toBe(0);
  });
}
