// At a 400 px viewport the UI has no horizontal page scroll, and the skin's buttons, the battery
// card with its charger, audio output, the microphone source and the console all work. The radio,
// NFC, USB and power cards are `cards.spec.ts`'s.
//
// Every control is driven the way a person drives it, then the machine is asked what it did:
//
// | Control | Driven | Checked by |
// |---|---|---|
// | buttons | pointerdown and pointerup on the UP, DOWN and OK keys of the skin | three `input` rows in the Events tab, and the selection moving on the glass |
// | battery and charger | the SOC slider and the Charger checkbox | an `env` row, two cable `input` rows, the USB strip following, and the card showing no refusal |
// | microphone source | the Mic source select | a `mic_set` row, and the card showing no refusal |
// | console | the console input box | a `serial` row carrying the line, and the card showing no refusal |
// | audio output | the demo's own Audio card, opened with the skin's buttons | the page's output meter leaving its floor while the guest plays |
//
// A listed call with no refusal in the card's `ErrorLine` is the machine having taken the command.
//
// It runs on the bundled demo the page boots with no input, published as `/official.pebundle` when
// this host has the pinned `official` build. Without it or a wasm core the test skips with that
// absence; a machine that refused to boot fails.

import { expect, test as base, type Locator, type Page } from "@playwright/test";
import { tmpdir } from "node:os";
import { fingerprint } from "./pacedSession";
import {
  browserGaps,
  call,
  openCard,
  openPage,
  requireImage,
  sendConsoleLine,
  showTab,
  waitForLine,
  waitForStatus,
} from "./harness";
import { inflateSync } from "node:zlib";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { pebundle, readOfficialFiles, type DemoFile } from "./demoBundle";
import { findCore } from "./preconditions";

const test = base.extend<{ fakeMedia: undefined }, object>({
  // A headless run has no microphone, so the fake device makes the `live` source reachable. An
  // AudioContext stays suspended until a user gesture and renders nothing, so autoplay is allowed too.
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

/** The narrow viewport, the "about 400 px" breakpoint. */
const NARROW = { width: 400, height: 900 } as const;

/** Wall time the page gets for something the guest has to do; it paces at rate 1. */
const GUEST_MS = 40_000;

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

/**
 * Fails when the page scrolls horizontally (`scrollWidth` over `clientWidth`), naming the widest
 * offender by tag, classes and right edge.
 */
async function expectNoHorizontalScroll(page: Page, when: string): Promise<void> {
  const found = await page.evaluate(() => {
    const root = document.documentElement;
    if (root.scrollWidth <= root.clientWidth) {
      return null;
    }
    let worst: { name: string; right: number } | null = null;
    for (const node of document.querySelectorAll<HTMLElement>("*")) {
      const right = node.getBoundingClientRect().right;
      if (right > root.clientWidth && (worst === null || right > worst.right)) {
        const name = `${node.tagName.toLowerCase()}${node.className ? `.${String(node.className).split(/\s+/).join(".")}` : ""}`;
        worst = { name, right };
      }
    }
    return { scrollWidth: root.scrollWidth, clientWidth: root.clientWidth, worst };
  });
  expect(
    found,
    found === null
      ? ""
      : `the page scrolls horizontally ${when}: scrollWidth ${found.scrollWidth} against clientWidth ${found.clientWidth}` +
        `, widest element over the edge ${found.worst?.name ?? "none"} at x=${Math.round(found.worst?.right ?? 0)}`,
  ).toBeNull();
}

/**
 * The command names the Events tab lists as the page's own, oldest first. A control that moved its
 * own widget and sent nothing leaves no row here.
 */
async function uiCalls(page: Page): Promise<string[]> {
  await showTab(page, "events");
  return page.locator('#pane-events tbody tr[data-source="ui"] td:nth-child(3)').allTextContents();
}

/**
 * Fails when a card shows a refusal. `controllers.ts` `attempt` puts the registry's error text in
 * the card's error line and leaves it hidden on success, so a hidden, empty line beside a listed
 * call means the machine took the command.
 */
async function expectNoCardRefusal(card: Locator, which: string): Promise<void> {
  const error = card.locator("p.card-error");
  expect(await error.textContent(), `the ${which} card reported a refusal`).toBe("");
  await expect(error, `the ${which} card's error line stays hidden`).toBeHidden();
}

test.describe("the UI at 400 px", () => {
  test.describe.configure({ timeout: 180_000 });

  test("at a 400 px viewport the UI has no horizontal page scroll and every control works", async ({
    page,
  }) => {
    const gap = precondition();
    test.skip(gap !== null, gap ?? "");

    await openPage(page, NARROW.width, NARROW.height);
    await expectNoHorizontalScroll(page, "as it opens");
    expect(
      await waitForStatus(page, GUEST_MS),
      "the bundled demo is up, so every control below has a machine to reach",
    ).toMatchObject({ ok: true });
    await expectNoHorizontalScroll(page, "once the demo is running");

    // Buttons: each edge is its own `input`, the release once the guest has held the key for the
    // minimum hold (`controls.ts`). DOWN then UP, watching the glass: the selection moves from Display
    // to Button and back, leaving the menu where it started. OK is exercised by the audio leg, which
    // needs three of them to reach the tone.
    await expect.poll(() => selectedCard(page), { timeout: GUEST_MS }).toBe("display");
    await pressKey(page, "down");
    await expect.poll(() => selectedCard(page), { timeout: GUEST_MS }).toBe("button");
    await pressKey(page, "up");
    await expect.poll(() => selectedCard(page), { timeout: GUEST_MS }).toBe("display");
    await expect
      .poll(async () => (await uiCalls(page)).filter((name) => name === "input").length, {
        message: "a press and a release `input` per key",
      })
      .toBe(4);
    await expectNoHorizontalScroll(page, "after the skin buttons");

    // Battery and charger.
    const battery = await openCard(page, "battery");
    const soc = battery.locator("#f-battery-soc");
    await soc.scrollIntoViewIfNeeded();
    // `fill` fires `input` and `change`; the slider journals on `change` only.
    await soc.fill("42");
    // The charger is the USB cable, plugged on a new machine: out and back in again.
    const charger = battery.locator("#f-battery-charger");
    await charger.scrollIntoViewIfNeeded();
    await expect(charger, "a new machine starts plugged in").toBeChecked();
    const inputsBefore = (await uiCalls(page)).filter((name) => name === "input").length;
    await battery.locator("#f-battery-charger").uncheck();
    await expect(page.locator('[data-usb="U0"]'), "the connector strip follows the charger").toHaveAttribute("aria-pressed", "true");
    await battery.locator("#f-battery-charger").check();
    await expect(page.locator('[data-usb="U3"]')).toHaveAttribute("aria-pressed", "true");
    await expect(battery.locator("span.readout"), "the card shows the cell it set").toContainText("42%");
    await expectNoCardRefusal(battery, "battery");
    const calls = await uiCalls(page);
    expect(
      calls.filter((name) => name === "env"),
      "the charge is one `env` call carrying only the charge",
    ).toHaveLength(1);
    expect(
      calls.filter((name) => name === "input").length - inputsBefore,
      "the charger unplugs and plugs the cable, one `input` each",
    ).toBe(2);
    await expectNoHorizontalScroll(page, "after the battery card");

    // Microphone source.
    const audio = await openCard(page, "audio");
    const source = audio.locator("#f-audio-mic-source");
    await source.scrollIntoViewIfNeeded();
    await source.selectOption("tone");
    await expectNoCardRefusal(audio, "audio");
    expect((await uiCalls(page)).filter((name) => name === "mic_set"), "the source is a `mic_set`").toHaveLength(1);
    await expectNoHorizontalScroll(page, "after the mic source");

    // Console.
    await showTab(page, "console");
    await sendConsoleLine(page, CONSOLE_PROBE);
    const serial = (await uiCalls(page)).filter((name) => name === "serial");
    expect(serial, "the console input is a `serial` write").toHaveLength(1);
    await showTab(page, "events");
    expect(
      await page.locator('#pane-events tbody tr[data-source="ui"]:has(td:nth-child(3):text-is("serial"))').last().textContent(),
      "the row carries the line that was typed",
    ).toContain(CONSOLE_PROBE);
    await expectNoHorizontalScroll(page, "after the console");

    // Audio output, end to end: the demo's Audio card plays a 1 kHz square at +-6000
    // (`main/demo_audio.c`), the Worker's pump reports the peak it pushed, and the card's meter shows
    // it. A page with no `AudioHost` reads "no audio path" and fails.
    await enterAudioDemo(page);
    const audioCard = await openCard(page, "audio");
    const meter = audioCard.locator('meter[aria-label="Output level"]');
    const level = audioCard.locator(".field:has(meter) .readout");
    await meter.scrollIntoViewIfNeeded();
    expect(await level.textContent(), "the page has an audio path to this machine").not.toBe("no audio path");
    // The pump fills its ring whether or not anyone listens, so the quanta the page's
    // `AudioWorkletNode` rendered are checked too.
    await expect
      .poll(async () => (await level.textContent()) ?? "", { timeout: GUEST_MS })
      // The silent count sits beside the underruns: an underrun is only counted where sound turned into
      // silence.
      .toMatch(/ \| [1-9]\d* quanta, \d+ underrun\(s\), \d+ silent$/);

    // The tone lasts one guest second and the meter follows the newest report, so the card is read
    // while OK plays more, until one reading is unambiguously the demo's amplitude.
    let loudest = 0;
    let pushed = "";
    for (let attempt = 0; attempt < 8 && loudest < TONE_METER; attempt += 1) {
      await pressKey(page, "ok");
      for (let read = 0; read < 16 && loudest < TONE_METER; read += 1) {
        loudest = Math.max(loudest, Number(await meter.getAttribute("value")));
        pushed = (await level.textContent()) ?? "";
        await page.waitForTimeout(80);
      }
    }
    expect(
      loudest,
      `the output meter never left its floor: the page showed ${pushed}, and the demo's tone is ${TONE_PEAK} of full scale`,
    ).toBeGreaterThanOrEqual(TONE_METER);
    console.log(`RAN audio-output: the page's output meter reached ${loudest.toFixed(4)} showing ${pushed}`);
    await expectNoHorizontalScroll(page, "after the audio demo");

    console.log(
      "RAN narrow-viewport: at 400 px the page has no horizontal page scroll, and its buttons, battery, charger, mic source, console and audio output all reached the machine",
    );
  });
});

/** The button component's click window (`CONFIG_BUTTON_SHORT_PRESS_TIME_MS` in the demo's sdkconfig). */
const CLICK_WINDOW_US = 180_000;

const CONSOLE_PROBE = "viewport-console-probe";

/** The demo's tone amplitude as a fraction of full scale (`demo_audio.c`: +-6000 of 32768). */
const TONE_PEAK = 6_000 / 32_768;

/** The meter reading that tone produces, less a margin for resampling from the guest's 16 kHz. */
const TONE_METER = ((20 * Math.log10(TONE_PEAK) + 96) / 96) * 0.9;

/** Presses and releases one skin key, long enough for the firmware's debounce. */
async function pressKey(page: Page, id: "up" | "ok" | "down"): Promise<void> {
  const key = page.locator(`[data-control="${id}"]`);
  await key.scrollIntoViewIfNeeded();
  await key.dispatchEvent("pointerdown");
  await page.waitForTimeout(80);
  await key.dispatchEvent("pointerup");
  await page.waitForTimeout(220);
}

/**
 * Which of the menu's first two cards the glass shows as selected: `display`, `button`, `both` or
 * `neither`, read as `m9.spec.ts`'s page-focus tests read it.
 */
async function selectedCard(page: Page): Promise<string> {
  const shot = decodeRgba(await page.locator("canvas.glass:not(.rewind-frame)").screenshot());
  const scale = shot.width / 240;
  // At 400 px the glass is drawn smaller than the panel (about half a device pixel per guest pixel),
  // so a few points into the fill are read and any yellow counts.
  const yellow = (x: number, y: number) =>
    [0, 4, 8, 12].some((dx) =>
      [0, 4, 8].some((dy) => {
        const at = (Math.floor((y + dy + 0.5) * scale) * shot.width + Math.floor((x + dx + 0.5) * scale)) * 3;
        const g = shot.rgb[at + 1] ?? 0;
        const b = shot.rgb[at + 2] ?? 0;
        return g > 0 && b * 2 < g;
      }),
    );
  const display = yellow(15, 56);
  const button = yellow(127, 56);
  return display && button ? "both" : display ? "display" : button ? "button" : "neither";
}

/**
 * Walks the menu to the Audio card and opens its codec with the skin's buttons: DOWN, DOWN, OK,
 * then an OK whose `bsp_audio_set_format` takes about 1.8 s of virtual time, so its tone may be
 * lost and the caller plays more. A second press inside the 180 ms click window is a double click
 * the menu ignores, so each key waits out that window in guest time.
 */
async function enterAudioDemo(page: Page): Promise<void> {
  for (const key of ["down", "down", "ok"] as const) {
    await pressKey(page, key);
    const from = await instanceVtUs(page);
    await expect.poll(() => instanceVtUs(page), { timeout: GUEST_MS }).toBeGreaterThan(from + 2 * CLICK_WINDOW_US);
  }
  await page.waitForTimeout(500);
  await pressKey(page, "ok");
  await page.waitForTimeout(3_000);
}

/** The RGB888 pixels of an 8-bit RGB or RGBA, non-interlaced PNG (`m9.spec.ts` `decodeRgbPng`). */
function decodeRgba(png: Buffer): { width: number; height: number; rgb: Uint8Array } {
  let at = 8;
  let width = 0;
  let height = 0;
  let samples = 3;
  const idat: Buffer[] = [];
  while (at < png.length) {
    const len = png.readUInt32BE(at);
    const type = png.toString("ascii", at + 4, at + 8);
    const data = png.subarray(at + 8, at + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      samples = data[9] === 6 ? 4 : 3;
    } else if (type === "IDAT") {
      idat.push(data);
    }
    at += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * samples;
  const all = new Uint8Array(stride * height);
  for (let y = 0; y < height; y += 1) {
    const filter = raw[y * (stride + 1)] ?? 0;
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    for (let x = 0; x < stride; x += 1) {
      const a = x >= samples ? (all[y * stride + x - samples] ?? 0) : 0;
      const b = y > 0 ? (all[(y - 1) * stride + x] ?? 0) : 0;
      const c = x >= samples && y > 0 ? (all[(y - 1) * stride + x - samples] ?? 0) : 0;
      let predictor = 0;
      if (filter === 1) {
        predictor = a;
      } else if (filter === 2) {
        predictor = b;
      } else if (filter === 3) {
        predictor = (a + b) >> 1;
      } else if (filter === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a);
        const pb = Math.abs(p - b);
        const pc = Math.abs(p - c);
        predictor = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      all[y * stride + x] = ((line[x] ?? 0) + predictor) & 0xff;
    }
  }
  if (samples === 3) {
    return { width, height, rgb: all };
  }
  const rgb = new Uint8Array(width * height * 3);
  for (let pixel = 0; pixel < width * height; pixel += 1) {
    rgb[pixel * 3] = all[pixel * 4] ?? 0;
    rgb[pixel * 3 + 1] = all[pixel * 4 + 1] ?? 0;
    rgb[pixel * 3 + 2] = all[pixel * 4 + 2] ?? 0;
  }
  return { width, height, rgb };
}

// Browser measurements for the interpreter-speed decision, recorded and not gated: the host time of
// the worst 100 ms virtual window and the audio underruns on F4 to F7, with a host fingerprint.
//
// Only the guest-side ledger is asserted (windows run, virtual time covered, button edges
// journaled, the console line the boot ended at), since it is exact on any host.
//
// A window is cut as `xtask bench` cuts one: the page paused with its own Pause button and the body
// run as N calls of `run --for 100ms`, each timed around the call. Press and release are raw edges
// with no implicit run, so a click is one 100 ms window (80 ms natively).
//
// Each workload runs twice. The worst window is a question about an unpaced machine; an underrun
// can only happen while the guest is paced at rate 1, since an unpaced body overfills the ring. So
// the body is also run `Wall`-paced with the page's Run button, and the underruns counted there.

/** One workload as the browser runs it (`xtask/src/bench.rs` `SUITES`). */
interface Workload {
  readonly id: string;
  readonly definition: string;
  readonly bootTo: string;
  readonly bootBudgetMs: number;
  /** Clicks that put the machine in the workload's state, at ms from the start of setup. */
  readonly setup: readonly (readonly [number, SkinKey])[];
  readonly setupMs: number;
  readonly clicks: readonly (readonly [number, SkinKey])[];
  readonly bodyMs: number;
}

type SkinKey = "up" | "ok" | "down";

const OFFICIAL_MENU = "main: 就绪:Display=1 Button=1 Audio=1 Battery=1";

const WINDOW_MS = 100;

/** F4 to F6, as `xtask/src/bench.rs` runs them natively. */
const WORKLOADS: readonly Workload[] = [
  {
    id: "F4",
    definition: "`official`: 40 menu clicks at 400 ms",
    bootTo: OFFICIAL_MENU,
    bootBudgetMs: 2_000,
    setup: [],
    setupMs: 0,
    clicks: Array.from({ length: 40 }, (_, i) => [i * 400, "down"] as const),
    bodyMs: 16_000,
  },
  {
    id: "F5",
    definition: "`official`: Display demo for 10 s",
    bootTo: OFFICIAL_MENU,
    bootBudgetMs: 2_000,
    setup: [],
    setupMs: 0,
    clicks: [[0, "ok"]],
    bodyMs: 10_000,
  },
  {
    id: "F6",
    definition: "`official`: Audio tone demo for 10 s",
    bootTo: OFFICIAL_MENU,
    bootBudgetMs: 2_000,
    setup: [
      [0, "down"],
      [400, "down"],
      [800, "ok"],
      [1_200, "ok"],
    ],
    setupMs: 4_000,
    clicks: [[0, "ok"]],
    bodyMs: 10_000,
  },
];

interface Measured {
  readonly windows: number;
  /** Virtual milliseconds the windows covered, from the machine's own `elapsed_vt_us`. */
  readonly virtualMs: number;
  readonly worstHostMs: number;
  readonly worstAt: number;
  readonly bodyHostMs: number;
  readonly underruns: number;
  readonly quanta: number;
  readonly edges: number;
}

async function pausePage(page: Page): Promise<void> {
  const pause = page.locator('button.transport[data-action="pause"]');
  await expect(pause, "the page is running, so Pause is live").toBeEnabled({ timeout: GUEST_MS });
  await pause.click();
  await expect(page.locator('button.transport[data-action="run"]'), "the page is paused").toBeEnabled();
}

async function must(page: Page, name: string, args: unknown): Promise<Record<string, unknown>> {
  const answer = await call(page, name, args);
  if (!answer.ok) {
    throw new Error(`${name} ${JSON.stringify(args)}: ${answer.error}`);
  }
  return (answer.json ?? {}) as Record<string, unknown>;
}

async function runFor(page: Page, ms: number): Promise<number> {
  const out = await must(page, "run", { for: `${ms}ms` });
  return Number(out.elapsed_vt_us ?? 0);
}

async function edge(page: Page, key: SkinKey, down: boolean): Promise<void> {
  await must(page, "input", { button: key, action: down ? "press" : "release" });
}

async function playback(page: Page): Promise<{ quanta: number; underruns: number }> {
  const text = (await page.locator('.field:has(meter[aria-label="Output level"]) .readout').textContent()) ?? "";
  const found = /(\d+) quanta, (\d+) underrun/.exec(text);
  return { quanta: Number(found?.[1] ?? 0), underruns: Number(found?.[2] ?? 0) };
}

async function reachTheBody(page: Page, workload: Workload): Promise<void> {
  const matched = await must(page, "run", {
    until: `serial:/${workload.bootTo}/`,
    timeout: `${workload.bootBudgetMs}ms`,
  });
  expect(matched.status, `${workload.id}: the boot reached \`${workload.bootTo}\``).toBe("matched");
  let at = 0;
  for (const [ms, key] of workload.setup) {
    if (ms > at) {
      await runFor(page, ms - at);
      at = ms;
    }
    await edge(page, key, true);
    await runFor(page, WINDOW_MS);
    at += WINDOW_MS;
    await edge(page, key, false);
  }
  if (workload.setupMs > at) {
    await runFor(page, workload.setupMs - at);
  }
}

async function measureBody(page: Page, workload: Workload): Promise<Measured> {
  const before = await playback(page);
  const windows = Math.round(workload.bodyMs / WINDOW_MS);
  const press = new Map<number, SkinKey[]>();
  const release = new Map<number, SkinKey[]>();
  for (const [ms, key] of workload.clicks) {
    const index = Math.round(ms / WINDOW_MS);
    press.set(index, [...(press.get(index) ?? []), key]);
    release.set(index + 1, [...(release.get(index + 1) ?? []), key]);
  }
  let worstHostMs = 0;
  let worstAt = 0;
  let virtualUs = 0;
  let edges = 0;
  const bodyAt = performance.now();
  for (let window = 0; window < windows; window += 1) {
    for (const key of release.get(window) ?? []) {
      await edge(page, key, false);
      edges += 1;
    }
    for (const key of press.get(window) ?? []) {
      await edge(page, key, true);
      edges += 1;
    }
    const at = performance.now();
    virtualUs += await runFor(page, WINDOW_MS);
    const hostMs = performance.now() - at;
    if (hostMs > worstHostMs) {
      worstHostMs = hostMs;
      worstAt = window;
    }
  }
  const bodyHostMs = performance.now() - bodyAt;
  for (const key of release.get(windows) ?? []) {
    await edge(page, key, false);
    edges += 1;
  }
  const after = await playback(page);
  return {
    windows,
    virtualMs: virtualUs / 1_000,
    worstHostMs,
    worstAt,
    bodyHostMs,
    underruns: after.underruns - before.underruns,
    quanta: after.quanta - before.quanta,
    edges,
  };
}

/**
 * Runs the same body `Wall`-paced at rate 1 with the page's Run button, for the underruns. Clicks
 * wait out wall time between them; the window count and virtual time are read back from the
 * machine, so the ledger is the guest's own however late the `await`s were.
 */
async function pacedBody(page: Page, workload: Workload): Promise<Measured> {
  const before = await playback(page);
  const startUs = await instanceVtUs(page);
  const run = page.locator('button.transport[data-action="run"]');
  await expect(run, `${workload.id}: the page can be resumed`).toBeEnabled();
  await run.click();
  const at = performance.now();
  let edges = 0;
  let waited = 0;
  for (const [ms, key] of workload.clicks) {
    if (ms > waited) {
      await page.waitForTimeout(ms - waited);
      waited = ms;
    }
    await edge(page, key, true);
    await page.waitForTimeout(WINDOW_MS);
    waited += WINDOW_MS;
    await edge(page, key, false);
    edges += 2;
  }
  if (workload.bodyMs > waited) {
    await page.waitForTimeout(workload.bodyMs - waited);
  }
  await pausePage(page);
  const bodyHostMs = performance.now() - at;
  const after = await playback(page);
  const endUs = await instanceVtUs(page);
  const virtualMs = (endUs - startUs) / 1_000;
  return {
    windows: workload.bodyMs / WINDOW_MS,
    virtualMs,
    worstHostMs: 0,
    worstAt: 0,
    bodyHostMs,
    underruns: after.underruns - before.underruns,
    quanta: after.quanta - before.quanta,
    edges,
  };
}

/** The instance's virtual time in microseconds, from `status`. */
async function instanceVtUs(page: Page): Promise<number> {
  const rows = (await must(page, "status", {})).instances as { vt_us?: number }[] | undefined;
  const first = rows?.[0]?.vt_us;
  expect(first, "`status` reports the instance's virtual time").not.toBeUndefined();
  return Number(first ?? 0);
}


function record(id: string, engine: string, what: string, value: string): void {
  console.log(`RECORDED ${id} ${engine} ${what}: ${value}`);
  test.info().annotations.push({ type: `${id} ${what} (recorded)`, description: value });
}

test.describe("interpreter speed in the browser (recorded, not gated)", () => {
  test.describe.configure({ timeout: 900_000 });

  test("F4, F5 and F6 worst 100 ms virtual window and audio underruns, recorded", { tag: "@wasm-speed" }, async ({
    page,
    browserName,
  }) => {
    const gap = precondition();
    test.skip(gap !== null, gap ?? "");

    const host = fingerprint(browserName);
    console.log(`RECORDED host: ${host}`);
    test.info().annotations.push({ type: "host (recorded)", description: host });

    for (const workload of WORKLOADS) {
      // A page per workload, so each starts from the reset the native suite starts from.
      await openPage(page);
      expect(await waitForStatus(page, GUEST_MS), `${workload.id}: the demo is up`).toMatchObject({ ok: true });
      await pausePage(page);
      await reachTheBody(page, workload);
      const measured = await measureBody(page, workload);

      // The paced pass, on its own machine, for the underruns.
      await openPage(page);
      expect(await waitForStatus(page, GUEST_MS), `${workload.id}: the demo is up for the paced pass`).toMatchObject({
        ok: true,
      });
      await pausePage(page);
      await reachTheBody(page, workload);
      const paced = await pacedBody(page, workload);

      expect(measured.windows, `${workload.id}: ${workload.bodyMs} ms of body in ${WINDOW_MS} ms windows`).toBe(
        workload.bodyMs / WINDOW_MS,
      );
      expect(
        Math.round(measured.virtualMs),
        `${workload.id}: every window advanced exactly ${WINDOW_MS} ms of virtual time`,
      ).toBe(workload.bodyMs);
      expect(measured.edges, `${workload.id}: one press and one release per click`).toBe(workload.clicks.length * 2);

      record(workload.id, browserName, "worst 100 ms window", `${measured.worstHostMs.toFixed(2)} ms host (window ${measured.worstAt} of ${measured.windows})`);
      expect(paced.windows, `${workload.id}: the paced pass ran the same body`).toBe(workload.bodyMs / WINDOW_MS);
      expect(
        Math.round(paced.virtualMs),
        `${workload.id}: the paced pass covered the same virtual time`,
      ).toBeGreaterThanOrEqual(workload.bodyMs);
      record(
        workload.id,
        browserName,
        "audio underruns",
        `${paced.underruns} over ${paced.quanta} playback quanta, Wall-paced at rate 1 for ${(paced.virtualMs / 1_000).toFixed(3)} s virtual in ${(paced.bodyHostMs / 1_000).toFixed(3)} s host`,
      );
      record(
        workload.id,
        browserName,
        "unpaced body",
        `${(measured.bodyHostMs / 1_000).toFixed(3)} s host for ${(measured.virtualMs / 1_000).toFixed(3)} s virtual, ${measured.quanta} playback quanta and ${measured.underruns} underruns (an unpaced body starves no worklet; the paced pass is the one to read)`,
      );
      console.log(
        `RAN ${workload.id} (${workload.definition}): ${measured.windows} windows, ${measured.virtualMs} ms virtual, ${measured.edges} journaled edges`,
      );
    }
  });

  // F7 needs `pk`, which is never committed, so a run without `PEMU_E2E_IMAGE_PK` skips it.
  test("F7 in the browser: BLE advertising, one connection and a notify stream, recorded", { tag: "@wasm-speed" }, async ({
    page,
    browserName,
  }) => {
    const gap = precondition();
    test.skip(gap !== null, gap ?? "");
    const host = fingerprint(browserName);
    console.log(`RECORDED host: ${host}`);

    await openPage(page);
    await requireImage(page, "pk", { name: "the F7 worst window", waitsOn: "nothing; F7 is recorded, not gated" });
    await showTab(page, "console");
    await waitForLine(page, /pk_app: ready/, GUEST_MS);
    await pausePage(page);

    // The connect script, outside the measured body as in the native F7: scan, connect, discover,
    // subscribe to the events characteristic.
    const scan = await must(page, "ble_scan", { duration_ms: PK_SCAN_MS });
    const peers = (scan.found ?? []) as { addr?: string }[];
    const addr = peers[0]?.addr;
    expect(addr, `the scan found \`pk\` advertising: ${JSON.stringify(scan)}`).toBeDefined();
    await must(page, "ble_connect", { addr });
    await must(page, "ble_gatt", { op: "discover" });
    await must(page, "ble_gatt", { op: "subscribe", uuid: PK_EVENTS });

    // 30 s of virtual time in 100 ms windows, writing one command line every 200 ms, because `pk`
    // notifies only in answer to one. Each write's virtual time is counted apart from the windows.
    const windows = F7_BODY_MS / WINDOW_MS;
    let worstHostMs = 0;
    let worstAt = 0;
    let windowUs = 0;
    let pollUs = 0;
    let polls = 0;
    let notifications = 0;
    const before = await playback(page);
    const bodyAt = performance.now();
    for (let window = 0; window < windows; window += 1) {
      if (window % (F7_POLL_MS / WINDOW_MS) === 0) {
        const startUs = await instanceVtUs(page);
        await must(page, "ble_gatt", { op: "write", uuid: PK_COMMANDS, text: PK_PING });
        polls += 1;
        // Waited for as the native F7 waits (`poll_within_ms` 2000): a poll that did not wait would read
        // an empty list.
        const seen = await must(page, "ble_gatt", {
          op: "notifications",
          uuid: PK_EVENTS,
          text: PK_PONG,
          within_ms: F7_POLL_WITHIN_MS,
        });
        // `new` counts only what arrived inside this call, and the pong usually lands before it starts, so
        // `total` is the figure that matters.
        notifications = Number(seen.total ?? 0);
        pollUs += (await instanceVtUs(page)) - startUs;
      }
      const at = performance.now();
      windowUs += await runFor(page, WINDOW_MS);
      const hostMs = performance.now() - at;
      if (hostMs > worstHostMs) {
        worstHostMs = hostMs;
        worstAt = window;
      }
    }
    const bodyHostMs = performance.now() - bodyAt;
    const after = await playback(page);

    expect(windows, `${F7_BODY_MS} ms of body in ${WINDOW_MS} ms windows`).toBe(300);
    expect(Math.round(windowUs / 1_000), "every window advanced exactly 100 ms of virtual time").toBe(F7_BODY_MS);
    expect(polls, "one command line every 200 ms of window time").toBe(F7_BODY_MS / F7_POLL_MS);
    notifications = Number(
      (await must(page, "ble_gatt", { op: "notifications", uuid: PK_EVENTS })).total ?? 0,
    );
    expect(notifications, "one notification per command line, which is what a notify stream is").toBe(polls);

    record("F7", browserName, "worst 100 ms window", `${worstHostMs.toFixed(2)} ms host (window ${worstAt} of ${windows})`);
    record("F7", browserName, "audio underruns", `${after.underruns - before.underruns} over ${after.quanta - before.quanta} playback quanta (\`pk\` plays nothing; the figure is here because the row asks for it)`);
    record(
      "F7",
      browserName,
      "body",
      `${(bodyHostMs / 1_000).toFixed(3)} s host for ${(windowUs / 1_000_000).toFixed(3)} s of windows plus ${(pollUs / 1_000_000).toFixed(3)} s inside the ${polls} writes`,
    );
    console.log(
      `RAN F7 (\`pk\`: BLE advertising, one connection, notify stream for 30 s): ${windows} windows, ${polls} polls, ${notifications} notifications`,
    );
  });
});

/** The `pk` vendor service characteristics F7 drives (`pk_ble.c`). */
const PK_EVENTS = "12D4FA09-7418-48FA-A95A-B43A2E669E55";
const PK_COMMANDS = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";

/** The command line one poll writes; `pk_line_feed` frames on the newline. */
const PK_PING = '{"cmd":"ping"}\n';

const PK_PONG = '{"t":"pong"}';

/** The native F7's own figures (`xtask/src/bench.rs` `F7_BLE`). */
const PK_SCAN_MS = 500;
const F7_POLL_MS = 200;
const F7_BODY_MS = 30_000;
const F7_POLL_WITHIN_MS = 2_000;

// The Wi-Fi scan workload, recorded and not gated like F4 to F7:
//
// | Id | Image | What the guest does in the body |
// |---|---|---|
// | `wifi-scan3` | the corpus `scan3` | three `esp_wifi_scan_start` cycles against a three-AP air, each cancelling the sweep before it, plus the whole bring-up and teardown |
// | `wifi-http` | `probe_wifi_http` | association, lwIP's DHCP lease and a GET answered by the virtual LAN |
//
// The ledger asserts the air was journaled, every window advanced exactly 100 ms, and the console
// reached the line that proves the radio work happened. Neither image plays audio, so no paced
// pass is run.
//
// Neither image is committed. Both come from the corpus under `PASSPORTSIM_DATA_ROOT` and are
// checked against their pins (`MANIFEST.json`, `tests/fw/manifest.toml`): an absent file skips, a
// file that is not the pinned build fails.

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

const RADIO_BODY_MS = 10_000;

/** Loads a test may spend looking for a start the page has not already run past (`reachTheRadioBody`). */
const START_ATTEMPTS = 5;

/**
 * The line the unmeasured boot ends at, printed by IDF's `main_task` just before `app_main`. The
 * ROM, bootloader and segment copies are the cost of starting a machine, not of a radio workload
 * (a body from reset reports that first window, 57 ms, as the worst).
 */
const RADIO_BOOT_TO = "Calling app_main";

const RADIO_BOOT_BUDGET_MS = 2_000;

/** The three scripted access points of `tests/milestones/m12.rs` `g2_world`. */
const G2_AIR = [
  { ssid: "G2-Alpha", bssid: "02:00:00:47:32:01", channel: 1, rssi: -42 },
  { ssid: "G2-Bravo", bssid: "02:00:00:47:32:02", channel: 6, rssi: -60 },
  { ssid: "G2-Charlie", bssid: "02:00:00:47:32:03", channel: 11, rssi: -75 },
] as const;

/** The open SSID `probe_wifi_http` joins (its `Kconfig.projbuild` placeholder default). */
const VIRTUAL_AP = "passport-emu-virtual-ap";

interface RadioWorkload {
  readonly id: string;
  readonly definition: string;
  /** The bundle id, which is also the name the loader shows for `<id>.pebundle`. */
  readonly image: string;
  readonly files: () => { files: DemoFile[] } | { skip: string } | { mismatch: string };
  readonly air: readonly { readonly ssid: string; readonly bssid?: string; readonly channel: number; readonly rssi: number }[];
  /** The guest's first line of its own, which must not be on the console when the body starts. */
  readonly startsAt: RegExp;
  /** The console line that says the guest really did the work. */
  readonly endsAt: RegExp;
}

/**
 * One corpus id's files, checked against the corpus `MANIFEST.json` as `pemu-testkit`'s
 * `corpus::locate_id` does. An absent file skips; a file that is not the pinned one is a
 * `mismatch` and fails.
 */
function readCorpusFiles(
  id: string,
  wanted: readonly { readonly file: string; readonly role: string }[],
  env: Readonly<Record<string, string | undefined>> = process.env,
): { files: DemoFile[] } | { skip: string } | { mismatch: string } {
  const root = env.PASSPORTSIM_DATA_ROOT;
  if (root === undefined || root === "") {
    return {
      skip: `no data root: set PASSPORTSIM_DATA_ROOT; the \`${id}\` corpus image is not committed`,
    };
  }
  const manifestPath = join(root, "corpus", "MANIFEST.json");
  if (!existsSync(manifestPath)) {
    return { skip: `no corpus manifest at ${manifestPath}` };
  }
  const records = JSON.parse(readFileSync(manifestPath, "utf8")) as {
    id: string;
    file: string;
    path: string;
    size: number;
    sha256: string;
  }[];
  const files: DemoFile[] = [];
  for (const want of wanted) {
    const record = records.find((row) => row.id === id && row.file === want.file);
    if (record === undefined) {
      return { skip: `the corpus manifest lists no \`${id}\` \`${want.file}\`` };
    }
    // A relative path is relative to the corpus directory (`corpus::locate_at`), so a manifest carried
    // to another host still resolves.
    const path = isAbsolute(record.path) ? record.path : join(root, "corpus", record.path);
    if (!existsSync(path)) {
      return { skip: `no \`${id}\` ${want.role} at ${path}: corpus images are not committed` };
    }
    const bytes = readFileSync(path);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    if (bytes.length !== record.size || sha256 !== record.sha256) {
      return { mismatch: `the \`${id}\` ${want.role} at ${path} is not the pinned file (SHA-256 ${sha256})` };
    }
    files.push({ role: want.role, name: want.file, bytes, sha256 });
  }
  return { files };
}

/**
 * The `probe_wifi_http` build from the corpus `probes` directory, checked against
 * `tests/fw/manifest.toml`. The ELF is the unstripped one: the committed `tests/fw` copy carries no
 * symbol a Wi-Fi hook could bind by.
 */
function readProbeWifiHttpFiles(
  env: Readonly<Record<string, string | undefined>> = process.env,
): { files: DemoFile[] } | { skip: string } | { mismatch: string } {
  const root = env.PASSPORTSIM_DATA_ROOT;
  if (root === undefined || root === "") {
    return { skip: "no data root: set PASSPORTSIM_DATA_ROOT; probe builds are not committed" };
  }
  const probes = join(root, "corpus", "probes");
  const wanted = [
    {
      role: "flash",
      name: "probe_wifi_http-8MB.bin",
      path: join(probes, "probe_wifi_http-8MB.bin"),
      pin: "merged_sha256",
    },
    {
      role: "app_elf",
      name: "probe_wifi_http.elf",
      path: join(probes, "build", "probe_wifi_http", "probe_wifi_http.elf"),
      pin: "elf_sha256",
    },
  ] as const;
  const manifest = readFileSync(join(REPO, "tests", "fw", "manifest.toml"), "utf8");
  const block = manifest.split("[[probe]]").find((part) => part.includes('name = "probe_wifi_http"'));
  if (block === undefined) {
    throw new Error("tests/fw/manifest.toml pins no probe_wifi_http");
  }
  const files: DemoFile[] = [];
  for (const file of wanted) {
    if (!existsSync(file.path)) {
      return {
        skip: `no \`probe_wifi_http\` ${file.role} at ${file.path}: the probe is not built (\`cargo xtask probes\`)`,
      };
    }
    const bytes = readFileSync(file.path);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    const pinned = block.split("\n").find((line) => line.startsWith(`${file.pin} = "`));
    const want = pinned?.slice(`${file.pin} = "`.length).replace(/"$/, "");
    if (want === undefined) {
      throw new Error(`tests/fw/manifest.toml pins no ${file.pin} for probe_wifi_http`);
    }
    if (sha256 !== want) {
      return { mismatch: `the ${file.role} at ${file.path} is not the pinned probe build (SHA-256 ${sha256})` };
    }
    files.push({ role: file.role, name: file.name, bytes, sha256 });
  }
  return { files };
}

const RADIO_WORKLOADS: readonly RadioWorkload[] = [
  {
    id: "wifi-scan3",
    definition: "`scan3`: three scan cycles against a three-AP air, bring-up and teardown",
    image: "scan3",
    files: () =>
      readCorpusFiles("scan3", [
        { file: "merged-binary.bin", role: "flash" },
        { file: "radio_scan3probe.elf", role: "app_elf" },
      ]),
    air: G2_AIR,
    startsAt: /HEAP\|boot\.app_main/,
    endsAt: /PROBE DONE/,
  },
  {
    id: "wifi-http",
    definition: "`probe_wifi_http`: association, a DHCP lease and a GET on the virtual LAN",
    image: "probe_wifi_http",
    files: readProbeWifiHttpFiles,
    air: [{ ssid: VIRTUAL_AP, channel: 6, rssi: -40 }],
    startsAt: /PROBE\|name=probe_wifi_http/,
    endsAt: /DONE\|name=probe_wifi_http\|status=ok/,
  },
];

interface RadioMeasured {
  readonly windows: number;
  readonly virtualMs: number;
  readonly worstHostMs: number;
  readonly worstAt: number;
  /** The five most expensive windows, worst first, as `window index: host ms`. */
  readonly worstFive: string;
  readonly bodyHostMs: number;
  readonly underruns: number;
  readonly quanta: number;
}

interface RadioStart {
  readonly onTheAir: number;
  readonly startUs: number;
  /** The guest console at that instant: empty of the workload's first line when the start is good. */
  readonly before: string;
}

/**
 * Loads one radio image, stops it before `app_main` and puts its air on the air. The page resumes
 * `Wall`-paced at `ready`, and both probes reach `app_main` about 85 ms of guest time after reset,
 * so Pause is sent at once and a start already past the workload's first line is redone.
 */
async function reachTheRadioBody(page: Page, workload: RadioWorkload, bundle: string): Promise<RadioStart> {
  // `waitForLoaded` polls from this process and lets the guest run 250 to 280 ms before Pause lands,
  // past both probes' whole Wi-Fi section. So the loader strip is watched from inside the page at
  // 5 ms, and Pause is pressed repeatedly because the page's own resume lands around the same time.
  await page.locator('[data-loader-input="files"]').setInputFiles([bundle]);
  const stopped = await page.evaluate(
    async ({ name, budgetMs }) => {
      const strip = () => document.querySelector("[data-loader]");
      const began = performance.now();
      let clicks = 0;
      let state = "";
      while (performance.now() - began < budgetMs) {
        const node = strip();
        state = node?.getAttribute("data-loader-state") ?? "";
        if (state === "refused" || state === "error") {
          return { clicks, state, message: node?.querySelector("[data-loader-message]")?.textContent ?? "" };
        }
        if (state === "loaded" && node?.getAttribute("data-loader-image") === name) {
          (document.querySelector('button.transport[data-action="pause"]') as HTMLButtonElement | null)?.click();
          clicks += 1;
          if (clicks >= 24) {
            break;
          }
        }
        await new Promise((resume) => setTimeout(resume, 5));
      }
      return { clicks, state, message: "" };
    },
    { name: workload.image, budgetMs: 90_000 },
  );
  expect(
    stopped.clicks,
    `${workload.id}: the page never loaded \`${workload.image}\` (strip ${stopped.state}: ${stopped.message})`,
  ).toBeGreaterThan(0);
  await expect(page.locator('button.transport[data-action="run"]'), `${workload.id}: the page is paused`).toBeEnabled();
  const status = await waitForStatus(page, GUEST_MS);
  expect(status, `${workload.id}: \`status\` answers once the image is running`).toMatchObject({ ok: true });
  expect(
    JSON.stringify(status.ok ? status.json : null),
    `${workload.id}: the page runs \`${workload.image}\``,
  ).toContain(`"fw":"${workload.image}"`);
  const before = await guestConsole(page);
  let onTheAir = 0;
  for (const ap of workload.air) {
    const answer = await must(page, "wifi_ap", ap);
    onTheAir = ((answer.aps as unknown[] | undefined) ?? []).length;
  }
  // The unmeasured boot, up to the guest's `app_main` ({@link RADIO_BOOT_TO}).
  const matched = await must(page, "run", {
    until: `serial:/${RADIO_BOOT_TO}/`,
    timeout: `${RADIO_BOOT_BUDGET_MS}ms`,
  });
  expect(matched.status, `${workload.id}: the boot reached \`${RADIO_BOOT_TO}\``).toBe("matched");
  const startUs = await instanceVtUs(page);
  return { onTheAir, startUs, before };
}

/**
 * The guest's own console, read from the machine with an explicit cursor. The console pane is not
 * used: it stops at the last line published before the pause, and an explicit cursor leaves the
 * page's own cursor alone.
 */
async function guestConsole(page: Page): Promise<string> {
  const lines: string[] = [];
  let cursor = 0;
  for (let read = 0; read < 4_000; read += 1) {
    // 1 KiB a read: `serial` shapes every answer inside a 4,000 character budget
    // (`crates/pemu-api/src/shape.rs`), so a larger chunk could elide the line looked for.
    const out = await must(page, "serial", { op: "read", stream: "usj", cursor, max_bytes: 1_024 });
    const excerpt = (out.serial ?? {}) as { head?: string[]; tail?: string[] };
    lines.push(...(excerpt.head ?? []), ...(excerpt.tail ?? []));
    const next = Number(out.next_cursor ?? cursor);
    if (next <= cursor) {
      break;
    }
    cursor = next;
  }
  return lines.join("\n");
}

async function measureRadioBody(page: Page, windows: number): Promise<RadioMeasured> {
  const before = await playback(page);
  const cost: number[] = [];
  let virtualUs = 0;
  const bodyAt = performance.now();
  for (let window = 0; window < windows; window += 1) {
    const at = performance.now();
    virtualUs += await runFor(page, WINDOW_MS);
    cost.push(performance.now() - at);
  }
  const bodyHostMs = performance.now() - bodyAt;
  const after = await playback(page);
  // The five worst windows with their index show whether the worst is in the radio work or the idle
  // tail.
  const ranked = cost
    .map((hostMs, window) => ({ hostMs, window }))
    .sort((left, right) => right.hostMs - left.hostMs);
  return {
    windows,
    virtualMs: virtualUs / 1_000,
    worstHostMs: ranked[0]?.hostMs ?? 0,
    worstAt: ranked[0]?.window ?? 0,
    worstFive: ranked
      .slice(0, 5)
      .map((row) => `${row.window}: ${row.hostMs.toFixed(2)} ms`)
      .join(", "),
    bodyHostMs,
    underruns: after.underruns - before.underruns,
    quanta: after.quanta - before.quanta,
  };
}

test.describe("the Wi-Fi scan workload (recorded, not gated)", () => {
  test.describe.configure({ timeout: 900_000 });

  for (const workload of RADIO_WORKLOADS) {
    test(`the Wi-Fi scan workload on ${workload.id}: worst 100 ms virtual window and audio underruns, recorded`, { tag: "@wasm-speed" }, async ({
      page,
      browserName,
    }) => {
      const gap = precondition();
      test.skip(gap !== null, gap ?? "");
      const found = workload.files();
      if ("mismatch" in found) {
        throw new Error(found.mismatch);
      }
      test.skip("skip" in found, "skip" in found ? found.skip : "");
      if (!("files" in found)) {
        return;
      }

      const host = fingerprint(browserName);
      console.log(`RECORDED host: ${host}`);
      test.info().annotations.push({ type: "host (recorded)", description: host });

      const scratch = mkdtempSync(join(tmpdir(), "pemu-wifi-card-"));
      const bundle = join(scratch, `${workload.image}.pebundle`);
      writeFileSync(
        bundle,
        pebundle(found.files, { id: workload.image, name: `${workload.image} (Wi-Fi scan)` }),
      );
      try {
        // A start the page ran past is redone on a fresh page, and each attempt is logged.
        let start: RadioStart | null = null;
        for (let attempt = 1; attempt <= START_ATTEMPTS && start === null; attempt += 1) {
          await openPage(page);
          expect(await waitForStatus(page, GUEST_MS), `${workload.id}: the page's machine is up`).toMatchObject({
            ok: true,
          });
          const reached = await reachTheRadioBody(page, workload, bundle);
          if (workload.startsAt.test(reached.before)) {
            console.log(
              `RAN ${workload.id} attempt ${attempt}: the page ran ${(reached.startUs / 1_000).toFixed(1)} ms ` +
                `past the reset before the pause landed, which is past the guest's own first line; starting again`,
            );
            continue;
          }
          start = reached;
          console.log(
            `RAN ${workload.id} start: attempt ${attempt}, the body starts ${(reached.startUs / 1_000).toFixed(1)} ms ` +
              `of virtual time after the reset, at the guest's \`app_main\``,
          );
        }
        expect(
          start,
          `${workload.id}: ${START_ATTEMPTS} loads all ran past the guest's own first line before the pause landed, ` +
            `so the workload would have been behind the measured windows; nothing is recorded from such a run`,
        ).not.toBeNull();
        if (start === null) {
          return;
        }
        const onTheAir = start.onTheAir;
        const measured = await measureRadioBody(page, RADIO_BODY_MS / WINDOW_MS);
        const text = await guestConsole(page);

        expect(onTheAir, `${workload.id}: the scripted air was journaled before the body`).toBe(workload.air.length);
        expect(
          workload.startsAt.test(start.before),
          `${workload.id}: the whole workload is inside the measured windows`,
        ).toBe(false);
        expect(text, `${workload.id}: and its first line is in them`).toMatch(workload.startsAt);
        expect(measured.windows, `${workload.id}: ${RADIO_BODY_MS} ms of body in ${WINDOW_MS} ms windows`).toBe(
          RADIO_BODY_MS / WINDOW_MS,
        );
        expect(
          Math.round(measured.virtualMs),
          `${workload.id}: every window advanced exactly ${WINDOW_MS} ms of virtual time`,
        ).toBe(RADIO_BODY_MS);
        expect(text, `${workload.id}: the guest really did the radio work`).toMatch(workload.endsAt);
        expect(text, `${workload.id}: no step of the probe failed`).not.toContain("FAIL|");

        record(
          workload.id,
          browserName,
          "worst 100 ms window",
          `${measured.worstHostMs.toFixed(2)} ms host (window ${measured.worstAt} of ${measured.windows})`,
        );
        record(
          workload.id,
          browserName,
          "audio underruns",
          `${measured.underruns} over ${measured.quanta} playback quanta (\`${workload.image}\` plays nothing; the figure is here because the gate names it)`,
        );
        record(workload.id, browserName, "five worst windows", measured.worstFive);
        record(
          workload.id,
          browserName,
          "body",
          `${(measured.bodyHostMs / 1_000).toFixed(3)} s host for ${(measured.virtualMs / 1_000).toFixed(3)} s virtual, ` +
            `the boot run outside it to \`${RADIO_BOOT_TO}\``,
        );
        console.log(
          `RAN ${workload.id} (${workload.definition}): ${measured.windows} windows, ` +
            `${measured.virtualMs} ms virtual from ${(start.startUs / 1_000).toFixed(1)} ms after the reset, ` +
            `${onTheAir} access point(s) journaled`,
        );
      } finally {
        rmSync(scratch, { recursive: true, force: true });
      }
    });
  }
});
