// The Playwright driver of `pacedSession.ts` (shared with the Safari driver
// `safari/pacedSafari.ts`): the fake-media flags, the preconditions, the knobs that need an init
// script, and `expect` as the way a gate fails.

import { expect, test as base } from "@playwright/test";
import { browserGaps, notRun, openPage, showTab } from "./harness";
import { readOfficialFiles } from "./demoBundle";
import { findCore } from "./preconditions";
import { runPacedSession, type SessionChecks } from "./pacedSession";
import { CHANNEL_TURN_FLAG, SPIN_WAIT_FLAG, TRACE_FLAG } from "../src/audio/trace";

const test = base.extend<{ fakeMedia: undefined }, object>({
  // A headless run has no microphone or speaker, and a suspended AudioContext renders nothing, so
  // without these flags the underrun counter would read zero because nothing played.
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
  fakeMedia: [async ({}, use) => use(undefined), { auto: true }],
});

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function precondition(): string | null {
  const core = findCore(process.env);
  if ("skip" in core) {
    return core.skip;
  }
  const official = readOfficialFiles();
  if ("mismatch" in official) {
    throw new Error(official.mismatch);
  }
  return "skip" in official ? official.skip : null;
}

const checks: SessionChecks = {
  equal: (actual, expected, message) => expect(actual, message).toBe(expected),
  atLeast: (actual, floor, message) => expect(actual, message).toBeGreaterThanOrEqual(floor),
  atMost: (actual, ceiling, message) => expect(actual, message).toBeLessThanOrEqual(ceiling),
  greaterThan: (actual, floor, message) => expect(actual, message).toBeGreaterThan(floor),
};

/** `PEMU_PACED_ONLY=F4` (or `F5`) ends the session after that workload; unset in every tier. */
const ONLY = process.env.PEMU_PACED_ONLY ?? "";

/** `PEMU_PACED_SPIN=1`: the spin control, unset in every tier. */
const SPIN = process.env.PEMU_PACED_SPIN === "1";

/**
 * `PEMU_PACED_BLOCK_MS=<n>`: after every F4 click the page's main thread spins for n ms, showing
 * whether the Worker's loop waits on it. Unset (0) in every tier.
 */
const BLOCK_MS = Number(process.env.PEMU_PACED_BLOCK_MS ?? "0");

test.describe("the paced browser session", () => {
  test.describe.configure({ timeout: 900_000 });

  test("F4 to F6 hold real time with no re-anchor, the worst window is inside its budget and 60 s of audio underruns zero times", { tag: "@wasm-speed" }, async ({
    page,
    browserName,
  }) => {
    const gap = precondition();
    test.skip(gap !== null, gap ?? "");

    await page.addInitScript((flag) => {
      (globalThis as unknown as Record<string, unknown>)[flag] = true;
    }, TRACE_FLAG);
    // Boots the Worker with a loop that spins where it would `Atomics.wait`.
    if (SPIN) {
      await page.addInitScript((flag) => {
        (globalThis as unknown as Record<string, unknown>)[flag] = true;
      }, SPIN_WAIT_FLAG);
      console.log("RECORDED control: the Worker's pacing loop spins instead of waiting (PEMU_PACED_SPIN)");
    }
    // The A/B control: keeps the old MessageChannel turn after a wait.
    if (process.env.PEMU_PACED_CHANNEL_TURN === "1") {
      await page.addInitScript((flag) => {
        (globalThis as unknown as Record<string, unknown>)[flag] = true;
      }, CHANNEL_TURN_FLAG);
      console.log("RECORDED control: the Worker takes its turn through a MessageChannel (PEMU_PACED_CHANNEL_TURN)");
    }

    await runPacedSession(
      page,
      {
        engine: browserName,
        openPage: () => openPage(page),
        showTab: (id) => showTab(page, id),
        checks,
        annotate: (type, description) => {
          test.info().annotations.push({ type, description });
        },
        notRun,
      },
      { only: ONLY, blockMs: BLOCK_MS },
    );
  });
});
