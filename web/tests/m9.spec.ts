// The real wasm core in the built Worker, booting the `official` demo from a `.pebundle`, with the
// audio worklets attached, cross-origin isolated (SAB) and not (MessagePort).
//
// Paced runs (`Wall`, both transports): `ready` at ABI 3, the real firmware's console lines,
// virtual time moving (real-time factor recorded, never gated), frames drawn, PCM rendered, and
// registry calls answered through `pemu_call`. Both compare the settled menu's `raw` frame with
// `tests/golden/official/menu.png`, then `click DOWN` and check the selection moved. The isolated
// run also presses Ok and exports the journal to a record file that `tests/milestones/m9.rs`
// (`t1_m9b_browser_journal_replays_natively`) replays natively.
//
// Unpaced runs: a registry `run` takes the machine to the menu at 3 s of virtual time. The
// page-focus tests open the built page and read two menu pixels from a screenshot of the glass.
// Engine tags keep each test on its project (`@chromium-only`, `@webkit-only`).
//
// Skips, each named: no Playwright browser, no wasm core, no `official` image and app ELF in
// `PEMU_E2E_OFFICIAL_DIR` or under `PASSPORTSIM_DATA_ROOT`. A file that is not the pinned build fails.

import { expect, test as base, type Page } from "@playwright/test";
import { createHash } from "node:crypto";
import { inflateSync } from "node:zlib";
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { hostOs, missingBrowser, rowById } from "./browsers";
import { DEMO_ID, DEMO_NAME, pebundle, readOfficialFiles, type DemoFile } from "./demoBundle";
import { browserGaps, pinPrefs } from "./harness";
import { findCore } from "./preconditions";
import { PANEL_HEIGHT, PANEL_WIDTH } from "../src/app/scale";
import type { BootResult, MidCall, MidPause, ProbeMode } from "./m9Probe";
import { serveDir, type StaticServer } from "./staticServer";
import { TRACE_FLAG, TRACE_LOG, type AudioTraceEntry } from "../src/audio/trace";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(WEB, "..");

/**
 * Wall time each paced run gets, and when the isolated run presses Ok. The press's effect reaches
 * the glass about 3 s of virtual time later, which the replay's negative control relies on.
 */
const RUN_WALL_MS = 9_000;
const PRESS_AT_MS = 4_500;

/** When the runs pause at the settled menu (3 s of virtual time). */
const MENU_AT_MS = 3_500;
const CLICK_DOWN = '{"cmd":"input","args":{"button":"down","action":"click"}}';

const test = base.extend<{ crashProbe: undefined }, object>({
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
  // A run that loses its browser reports `Target page, context or browser has been closed` in
  // teardown, which looks like a renderer crash but is not one. These listeners tell them apart: a
  // dying renderer prints `PAGE CRASH`, a page-level throw prints itself, a healthy run prints nothing.
  crashProbe: [
    async ({ page }, use, testInfo) => {
      const say = (what: string) => console.log(`crash probe: ${what} in ${testInfo.title.slice(0, 60)}`);
      page.on("crash", () => say("PAGE CRASH"));
      page.on("pageerror", (error) => say(`pageerror ${error.message}`));
      await use(undefined);
    },
    { auto: true },
  ],
});

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function corePath(): { path: string } | { skip: string } {
  return findCore(process.env);
}

/** The `official` demo files, or the reason there are none; a file that is not the pinned build fails. */
function officialFiles(): { files: DemoFile[] } | { skip: string } {
  const read = readOfficialFiles();
  if ("mismatch" in read) {
    throw new Error(read.mismatch);
  }
  return read;
}

let staged: Staged | { skip: string } | null = null;

// Each stage copies the core and builds the demo bundle, about 28 MB, so it goes with its worker.
test.afterAll(() => {
  if (staged !== null && "dir" in staged) {
    rmSync(staged.dir, { recursive: true, force: true });
  }
  staged = null;
});

interface Staged {
  readonly dir: string;
  /** SHA-256 of the served wasm core, so a replay can tell the record is current. */
  readonly coreSha256: string;
  readonly bundle: { sha256: string; id: string; name: string; files: { role: string; name: string; sha256: string }[] };
}

function stage(): Staged | { skip: string } {
  const core = corePath();
  if ("skip" in core) return core;
  const demo = officialFiles();
  if ("skip" in demo) return demo;
  const dist = join(WEB, "dist");
  const dir = mkdtempSync(join(tmpdir(), "pemu-m9-"));
  for (const name of readdirSync(dist)) {
    if (statSync(join(dist, name)).isFile()) copyFileSync(join(dist, name), join(dir, name));
  }
  copyFileSync(core.path, join(dir, "pemu_wasm.wasm"));
  const coreSha256 = createHash("sha256").update(readFileSync(core.path)).digest("hex");
  const bundle = pebundle(demo.files);
  writeFileSync(join(dir, "official.pebundle"), bundle);
  const probe = execFileSync("bun", ["build", join(WEB, "tests", "m9Probe.ts"), "--target", "browser"], {
    cwd: WEB,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  writeFileSync(join(dir, "m9probe.js"), probe);
  writeFileSync(
    join(dir, "m9.html"),
    '<!doctype html><meta charset="utf-8"><body><script type="module" src="./m9probe.js"></script></body>',
  );
  return {
    dir,
    coreSha256,
    bundle: {
      sha256: createHash("sha256").update(bundle).digest("hex"),
      id: DEMO_ID,
      name: DEMO_NAME,
      files: demo.files.map((file) => ({ role: file.role, name: file.name, sha256: file.sha256 })),
    },
  };
}

async function bootOn(
  page: Page,
  isolated: boolean,
  press: boolean,
  mid: MidPause,
  mode: ProbeMode = { kind: "Wall", rate: 1 },
  wallMs = RUN_WALL_MS,
): Promise<BootResult> {
  staged ??= stage();
  if ("skip" in staged) {
    test.skip(true, staged.skip);
    throw new Error("unreachable");
  }
  // A real server, whose isolation headers WebKit honours.
  const served = await serveDir(staged.dir, isolated);
  const url = served.url;
  try {
    await page.goto(`${url}m9.html`);
    await page.waitForFunction(() => typeof (globalThis as { m9Probe?: unknown }).m9Probe === "object");
    expect(await page.evaluate(() => globalThis.crossOriginIsolated)).toBe(isolated);
    return (await page.evaluate(
      ([wall, pressAt, midCalls, pacing]) =>
        (
          globalThis as unknown as {
            m9Probe: {
              boot(
                c: string,
                w: number,
                p: number | null,
                r: string[],
                m: MidPause,
                o: ProbeMode,
              ): Promise<unknown>;
            };
          }
        ).m9Probe.boot('{"fw":"official"}', wall, pressAt, ['{"cmd":"status"}', '{"cmd":"@report"}'], midCalls, pacing),
      [wallMs, press ? PRESS_AT_MS : null, mid, mode] as const,
    )) as BootResult;
  } finally {
    await served.close();
  }
}

function notRun(row: string, leg: string, reason: string): void {
  console.log(`NOT_RUN ${row} ${leg}: ${reason}`);
  test.info().annotations.push({ type: "NOT_RUN", description: `${row} ${leg}: ${reason}` });
}

function expectRealBoot(result: BootResult, isolated: boolean): void {
  expect(result.errors, "no Worker error").toEqual([]);
  expect(result.abiVersion).toBe(3);
  expect(result.audioSab, "the shared ring exactly when the page is isolated").toBe(isolated);
  expect(result.inputSab, "the shared input cell exactly when the page is isolated").toBe(isolated);
  // The real firmware, not a scripted core: the ROM banner, the 2nd-stage bootloader and `official`
  // reaching the end of `app_main`.
  const console = result.usj + result.uart0;
  expect(console).toContain("ESP-ROM:esp32c3-eco7-20230720");
  expect(console).toContain("2nd stage bootloader");
  expect(console).toContain("main_task: Returned from app_main()");
  expect(result.statsCount).toBeGreaterThan(10);
  const nowPs = BigInt(result.stats?.nowPs ?? "0");
  expect(nowPs, "Wall pacing moved virtual time past the app start").toBeGreaterThan(1_000_000_000_000n);
  expect(result.stats?.audio, "the audio path is attached").toBeDefined();
  const status = result.calls['{"cmd":"status"}'];
  expect(status?.err).toBeUndefined();
  expect(status?.ok ?? "").toContain('"fw":"official"');
}

/**
 * The boot half of an unpaced run: `ready` on SAB with no Worker error, the registry `run` to the
 * menu answered, the firmware's console lines, and `status` naming `official`.
 */
function expectUnpacedBoot(result: BootResult): void {
  expect(result.errors, "no Worker error").toEqual([]);
  expect(result.abiVersion).toBe(3);
  expect(result.inputSab, "isolated").toBe(true);
  expect(result.midCalls[0]?.err, "the registry run to the menu").toBeUndefined();
  const console = result.usj + result.uart0;
  expect(console).toContain("ESP-ROM:esp32c3-eco7-20230720");
  expect(console).toContain("main_task: Returned from app_main()");
  const status = result.calls['{"cmd":"status"}'];
  expect(status?.err).toBeUndefined();
  expect(status?.ok ?? "").toContain('"fw":"official"');
}

/** The selected card's inner fill, RGB565 (g3-menu-colours-invon). */
const SELECTED = 0xfec5;

/** `click DOWN` through `pemu_call` answered, and the raw frame after it shows Button selected, not Display. */
function expectRegistryClick(transport: string, result: BootResult, clickAt = 1, row = "paced"): void {
  const click = result.midCalls[clickAt];
  expect(click?.err, "the registry click").toBeUndefined();
  const after = result.midCalls[clickAt + 1]?.frame;
  expect(after, "the frame after the click").toBeDefined();
  const at = (x: number, y: number) => after?.pixels[y * (after?.width ?? 0) + x];
  expect(at(127, 56), "the Button card's inner fill is the selected yellow").toBe(SELECTED);
  expect(at(15, 56), "the Display card is no longer selected").not.toBe(SELECTED);
  console.log(`RAN ${row} click-down-${transport}: the registry click moved the selection to Button in the raw frame`);
}

/**
 * The page-focus check reads the two pixels from a screenshot of the built page's glass, since the
 * Worker's `raw` frame would pass on a page that drew nothing. The page is opened at a whole-pixel
 * glass scale (`layout.ts`) with `image-rendering: pixelated`, so each block's centre is one guest
 * pixel.
 */
const SCREEN = "canvas.glass:not(.rewind-frame)";

/** The two guest pixels read: the Button card's inner fill and the Display card's. */
const BUTTON_AT: readonly [number, number] = [127, 56];
const DISPLAY_AT: readonly [number, number] = [15, 56];

/** The `official` console line the settled menu follows (`main.c`). */
const OFFICIAL_READY = "main: 就绪";

const PAGE_BOOT_MS = 60_000;

const CLICK_WALL_MS = 15_000;

/** Wall time to reach the settled menu (3 s virtual at rate 1), with room for a loaded host. */
const MENU_WALL_MS = 30_000;

/**
 * Opens the built page with or without the isolation headers and waits for the demo: `passportEmu`
 * published, the shell mounted, and the guest's ready line in the console pane.
 */
async function bootPage(page: Page, isolated: boolean): Promise<StaticServer> {
  staged ??= stage();
  if ("skip" in staged) {
    test.skip(true, staged.skip);
    throw new Error("unreachable");
  }
  const served = await serveDir(staged.dir, isolated);
  // This viewport snaps the glass to one device pixel per guest pixel (`layout.ts`).
  await page.setViewportSize({ width: 1440, height: 1000 });
  await pinPrefs(page);
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("passportsim.zoom", "fit");
    } catch {
    }
  });
  await page.goto(`${served.url}index.html`);
  await expect(page.locator("#app .app"), "the shell mounted").toBeVisible();
  await page.waitForFunction(() => typeof (globalThis as { passportEmu?: unknown }).passportEmu === "object");
  expect(
    await page.evaluate(() => globalThis.crossOriginIsolated),
    "the page is cross-origin isolated exactly when its server sent the headers",
  ).toBe(isolated);
  await expect
    .poll(async () => (await page.locator("#pane-console").textContent()) ?? "", { timeout: PAGE_BOOT_MS })
    .toContain(OFFICIAL_READY);
  return served;
}

/** One `passportEmu.call` from the page, the door an agent with only a browser has. */
async function pageCall(
  page: Page,
  name: string,
  args: unknown,
): Promise<{ ok: true; json: unknown } | { ok: false; error: string }> {
  return page.evaluate(
    async ({ name, args }) => {
      const api = (globalThis as unknown as { passportEmu: { call(n: string, a: unknown): Promise<{ json: unknown }> } })
        .passportEmu;
      try {
        return { ok: true as const, json: (await api.call(name, args)).json };
      } catch (error) {
        const body = (error as { body?: { code?: string; message?: string } }).body;
        return { ok: false as const, error: body ? `${body.code}: ${body.message}` : String(error) };
      }
    },
    { name, args },
  );
}

/**
 * The RGB888 the page shows at guest pixel `(x, y)`, from a screenshot of the glass. It samples the
 * centre of the block the guest pixel covers, with the scale taken from the screenshot's width
 * against `PANEL_WIDTH`, so a canvas at a non-whole multiple fails here with its size.
 */
async function pageScreen(page: Page): Promise<(x: number, y: number) => [number, number, number]> {
  const shot = decodeRgbPng(await page.locator(SCREEN).screenshot());
  const scale = shot.width / PANEL_WIDTH;
  expect(
    [shot.width, shot.height],
    `the glass is a whole-pixel upscale of ${PANEL_WIDTH}x${PANEL_HEIGHT}, screenshot ${shot.width}x${shot.height}`,
  ).toEqual([PANEL_WIDTH * scale, PANEL_HEIGHT * scale]);
  expect(Number.isInteger(scale) && scale >= 1, `the upscale factor is a whole number, not ${scale}`).toBe(true);
  return (x, y) => {
    const at = (Math.floor((y + 0.5) * scale) * shot.width + Math.floor((x + 0.5) * scale)) * 3;
    return [shot.rgb[at] ?? -1, shot.rgb[at + 1] ?? -1, shot.rgb[at + 2] ?? -1];
  };
}

/**
 * Whether a card's inner fill reads as selected: `UI_YELLOW` 0xFFD928 against `UI_PAPER` 0xF4F4EA
 * (`ui_pixel.c`). The canvas is scaled by the backlight gain (`rgb565.ts` `backlightGain`), which
 * leaves the blue-to-green ratio alone: 0.19 against 0.96, split at 0.5. Black is refused outright.
 */
function selectedFill(rgb: readonly [number, number, number]): boolean {
  const g = rgb[1] ?? 0;
  const b = rgb[2] ?? 0;
  return g > 0 && b * 2 < g;
}

function blank(rgb: readonly [number, number, number]): boolean {
  return rgb.every((v) => v <= 0);
}

/**
 * On the settled menu the glass shows Display selected and Button not, and after one registry
 * `click DOWN` the reverse. Asserting the state before the click catches a page that painted every
 * card yellow or froze; requiring the two fills to swap byte for byte rules out partial redraws.
 */
async function expectPageFocusMovesToButton(page: Page, row: string, leg: string): Promise<void> {
  // The page is `Wall`-paced, so the settled menu arrives about 3 s after the ready line and the
  // glass is black until the first frame: the menu is polled for.
  let displayBefore: [number, number, number] = [-1, -1, -1];
  let buttonBefore: [number, number, number] = [-1, -1, -1];
  await expect
    .poll(
      async () => {
        const screen = await pageScreen(page);
        displayBefore = screen(...DISPLAY_AT);
        buttonBefore = screen(...BUTTON_AT);
        return selectedFill(displayBefore) && !selectedFill(buttonBefore) && !blank(buttonBefore);
      },
      { timeout: MENU_WALL_MS },
    )
    .toBe(true);
  expect(blank(buttonBefore), `the page drew the Button card, it is ${buttonBefore}`).toBe(false);
  expect(selectedFill(displayBefore), `the settled menu shows Display selected, not ${displayBefore}`).toBe(true);
  expect(selectedFill(buttonBefore), `the settled menu does not show Button selected, it shows ${buttonBefore}`).toBe(
    false,
  );
  // `official` latches duty 1023 of 1024 with DISPON sent, so the glass is lit (the yellow's green
  // channel is about 219 at full gain).
  expect(displayBefore[1], `the settled menu is lit, the Display fill is ${displayBefore}`).toBeGreaterThan(200);
  await expect(page.locator(".skin-status")).toHaveText(/^backlight 100% \| panel on \| /);

  const click = await pageCall(page, "input", { button: "down", action: "click" });
  expect(click.ok, `the registry click DOWN through the page: ${click.ok ? "" : click.error}`).toBe(true);

  // The click is answered before the guest redraws, so the page is polled for the move.
  let button: [number, number, number] = [-1, -1, -1];
  let display: [number, number, number] = [-1, -1, -1];
  await expect
    .poll(
      async () => {
        const screen = await pageScreen(page);
        button = screen(...BUTTON_AT);
        display = screen(...DISPLAY_AT);
        return selectedFill(button) && !selectedFill(display);
      },
      { timeout: CLICK_WALL_MS },
    )
    .toBe(true);
  expect(selectedFill(button), `the page shows Button selected at ${BUTTON_AT}, it shows ${button}`).toBe(true);
  expect(selectedFill(display), `the page no longer shows Display selected, it shows ${display}`).toBe(false);
  expect(button, "the Button card now shows the fill the Display card had").toEqual(displayBefore);
  expect(display, "the Display card now shows the fill the Button card had").toEqual(buttonBefore);
  console.log(
    `RAN ${row} ${leg}: the page's glass showed Display ${displayBefore} selected over Button ${buttonBefore}, and after the registry click DOWN it shows Button ${button} selected over Display ${display}`,
  );
}

/**
 * `inspect vars` in the Worker, naming the global: a firmware declares around a thousand and a
 * default answer is budgeted at 4,000 characters, so a `vars` call that names none is refused.
 */
const S_SEL_INSPECT = '{"cmd":"inspect","args":{"what":["vars"],"vars":["s_sel"]}}';

/**
 * `s_sel` is 1 after the click, read from the Worker's guest memory at the type the ELF's DWARF
 * gives it; the row carries the address and C type beside the value.
 */
function expectSSel(step: MidCall | undefined, row: string, transport: string): void {
  expect(step?.err, "`inspect vars` answers").toBeUndefined();
  // `pemu_call` answers the `CommandOutput` envelope, so the command's body is under `json`.
  const rows = JSON.parse(step?.ok ?? "{}").json?.vars as
    | { name?: string; value?: unknown; type?: string; addr?: string; unreadable?: string }[]
    | undefined;
  expect(rows, "`inspect vars` returns a row per global").toHaveLength(1);
  expect(rows?.[0]?.unreadable, `s_sel is readable: ${rows?.[0]?.unreadable ?? ""}`).toBeUndefined();
  expect(rows?.[0]?.name, "the row is s_sel").toBe("s_sel");
  expect(rows?.[0]?.type, "read at the type its DWARF declares").toBe("int32");
  expect(rows?.[0]?.value, "s_sel reads back 1 after the click DOWN").toBe(1);
  console.log(
    `RAN ${row} s_sel-${transport}: inspect vars reads s_sel = 1 (${rows?.[0]?.type} at ${rows?.[0]?.addr}) after the registry click`,
  );
}

/**
 * The RGB888 pixels of an 8-bit, non-interlaced PNG, colour type 2 (the goldens `pemu_host::png`
 * writes) or 6 (a Playwright element screenshot). Alpha is dropped rather than composited: a
 * screenshot of an opaque canvas is fully opaque.
 */
function decodeRgbPng(png: Buffer): { width: number; height: number; rgb: Uint8Array } {
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
      expect([data[8], data[12]], "8 bits per sample, not interlaced").toEqual([8, 0]);
      expect([2, 6], `colour type ${data[9]} is RGB or RGBA`).toContain(data[9]);
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
      const predictor: number =
        filter === 0
          ? 0
          : filter === 1
            ? a
            : filter === 2
              ? b
              : filter === 3
                ? (a + b) >> 1
                : (() => {
                    const p = a + b - c;
                    const pa = Math.abs(p - a);
                    const pb = Math.abs(p - b);
                    const pc = Math.abs(p - c);
                    return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
                  })();
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

function expand565(pixel: number): [number, number, number] {
  const r5 = (pixel >> 11) & 0x1f;
  const g6 = (pixel >> 5) & 0x3f;
  const b5 = pixel & 0x1f;
  return [(r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2)];
}

/**
 * The frame check on the Worker's `raw` frame at the settled menu: every pixel, expanded as the
 * golden was encoded, equals `tests/golden/official/menu.png`.
 */
function expectMenuGolden(transport: string, step: MidCall | undefined, row = "paced"): void {
  const frame = step?.frame;
  expect(frame, `the ${transport} run copied the raw frame: ${step?.err ?? ""}`).toBeDefined();
  if (!frame) return;
  const golden = decodeRgbPng(readFileSync(join(REPO, "tests", "golden", "official", "menu.png")));
  expect([frame.width, frame.height]).toEqual([golden.width, golden.height]);
  let differing = 0;
  let first = -1;
  frame.pixels.forEach((pixel, index) => {
    const [r, g, b] = expand565(pixel);
    const o = index * 3;
    if (golden.rgb[o] !== r || golden.rgb[o + 1] !== g || golden.rgb[o + 2] !== b) {
      differing += 1;
      if (first < 0) first = index;
    }
  });
  expect(
    differing,
    `the ${transport} raw frame differs from menu.png in ${differing} pixels, first at (${first % frame.width}, ${Math.floor(first / frame.width)})`,
  ).toBe(0);
  console.log(`RAN ${row} menu-golden-${transport}: the raw frame at generation ${frame.generation} equals tests/golden/official/menu.png`);
}

/** The guest presented frames and the Worker drew them, and PCM reached the playback worklet. */
function expectFramesAndPcm(transport: string, result: BootResult): void {
  const generation = BigInt(result.stats?.frameGeneration ?? "0");
  expect(generation, "the guest presented frames (FramePort generation)").toBeGreaterThan(0n);
  expect(result.stats?.draws ?? 0, "the Worker drew them to the canvas").toBeGreaterThan(0);
  console.log(`RAN paced frame-${transport}: generation=${generation} draws=${result.stats?.draws}`);
  const audio = result.stats?.audio;
  const pushed = BigInt(audio?.pushed ?? "0");
  expect(pushed, "PCM reached the audio pump").toBeGreaterThan(0n);
  expect(audio?.playback?.quanta ?? 0, "the playback worklet rendered").toBeGreaterThan(0);
  console.log(
    `RAN paced pcm-${transport}: pushed=${pushed} quanta=${audio?.playback?.quanta} underruns=${audio?.playback?.underruns}`,
  );
}

/**
 * Records one run's pacing, never gated: the real-time factor with sleeps included (about 1 at
 * `Wall` 1x), the headroom inside `pemu_run`, and the whole run's virtual time over wall time.
 */
function recordPacing(transport: string, result: BootResult): void {
  const rtf = result.stats?.realTimeFactor ?? 0;
  const headroom = result.stats?.headroom ?? 0;
  const vtMs = Number(BigInt(result.stats?.nowPs ?? "0") / 1_000_000_000n);
  const overall = result.wallMs > 0 ? vtMs / result.wallMs : 0;
  // The isolated Worker waits with `Atomics.wait` on the input cell; the fallback yields through a
  // MessageChannel.
  const yielder = result.stats?.yielder;
  expect(yielder, `the ${transport} run's yielder`).toBe(transport === "sab" ? "atomics-wait" : "message-channel");
  // The isolated loop's turn after a wait never goes through a MessagePort, which WebKit carries
  // through the page's main thread.
  const turn = result.stats?.turn;
  expect(turn, `the ${transport} run's event-loop turn`).toBe(transport === "sab" ? "wait-async" : undefined);
  console.log(`RAN paced yielder-${transport}: ${yielder}${turn ? ` turn ${turn}` : ""}`);
  console.log(
    `RAN paced ${transport}: vt=${result.stats?.nowPs}ps wall=${Math.round(result.wallMs)}ms rtf=${rtf.toFixed(3)} headroom=${headroom.toFixed(1)} vt/wall=${overall.toFixed(3)} reanchors=${result.stats?.reanchors}`,
  );
  test.info().annotations.push(
    { type: `paced ${transport} real-time factor (recorded)`, description: String(rtf) },
    { type: `paced ${transport} headroom (recorded)`, description: String(headroom) },
    { type: `paced ${transport} vt/wall (recorded)`, description: String(overall) },
    { type: `paced ${transport} yielder (recorded)`, description: String(yielder) },
  );
}

/**
 * Why the WebKit leg has no run here: the install hint when Playwright WebKit is missing, the
 * macOS-only rule on another host, else the project that runs it. `xtask ci` drops this leg from a
 * step whose WebKit project ran (`web.rs` `drop_stale_engine_legs`).
 */
function webkitLegGap(): string {
  if (hostOs() !== "macos") {
    return `Playwright WebKit runs on macOS only, this host is ${process.platform}`;
  }
  const row = rowById("webkit");
  return (
    (row === undefined ? null : missingBrowser(row)) ??
    "runs in the webkit project (`bun run e2e --project=webkit`, T2 `playwright-chromium-webkit`), not in a Chromium run"
  );
}

/**
 * The unpaced runs stay `Paused` and a registry `run` takes the machine to the settled menu at 3 s
 * of virtual time. A page-side pause of a `Max` run lands wherever the next `stats` arrives (3.5 s
 * to 4.3 s on Chromium), past a redraw of 48 menu pixels, so virtual time is set exactly here.
 */
const RUN_TO_MENU = '{"cmd":"run","args":{"for":"3s"}}';
const UNPACED_WALL_MS = 1_000;
const UNPACED: ProbeMode = { kind: "Paused" };

// Each test is tagged with its engine and `playwright.config.ts` leaves it out of the other project.
test.describe("the real core in the built Worker", () => {
  test.describe.configure({ timeout: 120_000 });

  test(
    "official boots Wall-paced on SAB with audio attached and no Worker error",
    { tag: "@chromium-only" },
    async ({ page }) => {
      const result = await bootOn(page, true, true, {
        atMs: MENU_AT_MS,
        requests: ["frame", CLICK_DOWN, "frame", S_SEL_INSPECT],
      });
      expectRealBoot(result, true);
      recordPacing("sab", result);

      // The journal holds the registry click's press and release, then the Worker's Ok press and release.
      const journal = JSON.parse(result.journal ?? "null") as {
        format: number;
        replayable: boolean;
        entries: { atPs: string; seq: string; door: string; event: unknown }[];
      };
      // The export names its format, and a session with no live microphone drops nothing.
      expect(journal.format).toBe(1);
      expect(journal.replayable).toBe(true);
      expect(journal.entries.map((entry) => [entry.door, entry.event])).toEqual([
        ["registry", { Button: { id: "Down", down: true } }],
        ["registry", { Button: { id: "Down", down: false } }],
        ["input", { Button: { id: "Ok", down: true } }],
        ["input", { Button: { id: "Ok", down: false } }],
      ]);
      const report = JSON.parse(result.calls['{"cmd":"@report"}']?.ok ?? "{}") as { report?: string; roles?: string[] };
      expect(report.report ?? "").toMatch(/^stop=\S+ state=[0-9a-f]{64} /);
      const { bundle, coreSha256 } = staged as Staged;
      expect(report.roles, "the roles the core loaded").toEqual(bundle.files.map((file) => file.role));

      expectFramesAndPcm("sab", result);
      expectMenuGolden("sab", result.midCalls[0]);
      expectRegistryClick("sab", result);

      // The record is written only once every assertion has passed: `xtask ci` replays whatever record
      // exists.
      const record = process.env.PEMU_BROWSER_RECORD ?? join(tmpdir(), "passportsim-browser-record.json");
      writeFileSync(
        record,
        JSON.stringify({
          image: "official",
          core: { sha256: coreSha256, abiVersion: result.abiVersion },
          config: { fw: "official" },
          bundle,
          roles: report.roles,
          journal,
          report: report.report,
        }),
      );
      console.log(
        `RAN journal-export browser-half: journal of ${journal.entries.length} inputs (2 from the registry click) and its report recorded`,
      );
      expectSSel(result.midCalls[3], "paced", "sab");
      notRun("unpaced-webkit", "webkit", webkitLegGap());
    },
  );

  test(
    "official boots Wall-paced on a transferred MessagePort with no Worker error",
    { tag: "@chromium-only" },
    async ({ page }) => {
      const result = await bootOn(page, false, false, {
        atMs: MENU_AT_MS,
        requests: ["frame", CLICK_DOWN, "frame", S_SEL_INSPECT],
      });
      expectRealBoot(result, false);
      recordPacing("port", result);
      expectFramesAndPcm("port", result);
      expectMenuGolden("port", result.midCalls[0]);
      expectRegistryClick("port", result);
      expectSSel(result.midCalls[3], "paced", "port");
    },
  );

  // Legs that cannot pass here are annotated `NOT_RUN`, and the `test.fail` below names them.
  test(
    "official boots unpaced in headless Chrome to the menu golden and a registry click DOWN moves the selection",
    { tag: "@chromium-only" },
    async ({ page }) => {
      const result = await bootOn(
        page,
        true,
        false,
        { atMs: 0, requests: [RUN_TO_MENU, "frame", CLICK_DOWN, "frame", S_SEL_INSPECT] },
        UNPACED,
        UNPACED_WALL_MS,
      );
      expectUnpacedBoot(result);
      expectMenuGolden("unpaced", result.midCalls[1], "unpaced");
      expectRegistryClick("unpaced", result, 2, "unpaced");
      expectSSel(result.midCalls[4], "unpaced", "unpaced");
    },
  );

  // The page-focus tests open the built page rather than the Worker probe. Each is its own test,
  // because a page boot is a second machine and folding it into a paced run would make one failure
  // report as the other's. Isolated and non-isolated both run: the shared ring and input cell exist
  // only in an isolated page.
  test(
    "unpaced: after the registry click DOWN, the page shows focus on Button",
    { tag: "@chromium-only" },
    async ({ page }) => {
      const served = await bootPage(page, true);
      try {
        await expectPageFocusMovesToButton(page, "unpaced", "page-focus");
      } finally {
        await served.close();
      }
    },
  );

  test(
    "the page shows the focus move to Button after a registry click DOWN, cross-origin isolated",
    { tag: "@chromium-only" },
    async ({ page }) => {
      const served = await bootPage(page, true);
      try {
        expect(
          await page.evaluate(() => typeof SharedArrayBuffer),
          "an isolated page has SharedArrayBuffer, which is the transport this leg is named for",
        ).toBe("function");
        await expectPageFocusMovesToButton(page, "paced", "page-focus-sab");
      } finally {
        await served.close();
      }
    },
  );

  test(
    "the page shows the focus move to Button after a registry click DOWN on a transferred MessagePort",
    { tag: "@chromium-only" },
    async ({ page }) => {
      const served = await bootPage(page, false);
      try {
        await expectPageFocusMovesToButton(page, "paced", "page-focus-port");
      } finally {
        await served.close();
      }
    },
  );

  test(
    "the Worker still answers mode Paused and status while it runs unpaced at Max",
    { tag: "@chromium-only" },
    async ({ page }) => {
      // A `Max` loop once chained slices as microtasks and read no message, so this pause went unanswered.
      const result = await bootOn(page, true, false, { atMs: 1_000, requests: ['{"cmd":"status"}'] }, { kind: "Max" }, 2_000);
      expect(result.errors, "no Worker error").toEqual([]);
      expect(result.midCalls[0]?.err, "status answered at the pause").toBeUndefined();
      expect(result.midCalls[0]?.ok ?? "").toContain('"fw":"official"');
      expect(BigInt(result.midNowPs ?? "0"), "Max ran the guest before the pause").toBeGreaterThan(0n);
    },
  );

  test(
    "Playwright WebKit runs the unpaced flow: official reaches the menu golden and a registry click moves the selection",
    { tag: "@webkit-only" },
    async ({ page }) => {
      const result = await bootOn(
        page,
        true,
        false,
        { atMs: 0, requests: [RUN_TO_MENU, "frame", CLICK_DOWN, "frame", S_SEL_INSPECT] },
        UNPACED,
        UNPACED_WALL_MS,
      );
      expectUnpacedBoot(result);
      expectMenuGolden("webkit-unpaced", result.midCalls[1], "unpaced-webkit");
      expectRegistryClick("webkit-unpaced", result, 2, "unpaced-webkit");
      expectSSel(result.midCalls[4], "unpaced-webkit", "webkit-unpaced");
    },
  );

  test(
    "the page shows the focus move to Button after a registry click DOWN in Playwright WebKit",
    { tag: "@webkit-only" },
    async ({ page }) => {
      const served = await bootPage(page, true);
      try {
        await expectPageFocusMovesToButton(page, "unpaced-webkit", "page-focus-webkit");
      } finally {
        await served.close();
      }
    },
  );
});

// Firefox's glass. Its page once ran a black panel and answered no page message, because the
// `Atomics.waitAsync` turn starved the Worker's other tasks (`pacing.ts` `turnRunsTasks`); only the
// glass shows that.
test.describe("the page's glass in Playwright Firefox", () => {
  test.describe.configure({ timeout: 120_000 });

  for (const isolated of [true, false]) {
    const how = isolated ? "cross-origin isolated" : "on a transferred MessagePort";
    test(
      `Firefox shows the settled menu on the glass and the move after a registry click DOWN, ${how}`,
      { tag: "@firefox-only" },
      async ({ page }) => {
        const served = await bootPage(page, isolated);
        try {
          expect(await page.evaluate(() => crossOriginIsolated), "the page is served the way the title says").toBe(
            isolated,
          );
          await expectPageFocusMovesToButton(page, "Firefox", isolated ? "page-focus-sab" : "page-focus-port");
        } finally {
          await served.close();
        }
      },
    );
  }
});

test.describe("the isolated Worker's pacing loop and the page's main thread", () => {
  test.describe.configure({ timeout: 120_000 });

  const BLOCK_MS = 200;
  const BLOCKS = 5;

  test("a busy page main thread does not hold the Worker's event-loop turn, in every engine", async ({
    page,
    browserName,
  }) => {
    // The loop trace times the turn (`loopTrace.ts`); it is on with the audio trace.
    await page.addInitScript((flag) => {
      (globalThis as unknown as Record<string, unknown>)[flag] = true;
    }, TRACE_FLAG);
    const served = await bootPage(page, true);
    try {
      const firstBlock = Date.now();
      for (let block = 0; block < BLOCKS; block += 1) {
        await page.waitForTimeout(300);
        await page.evaluate((ms) => {
          const end = performance.now() + ms;
          while (performance.now() < end) {
          }
        }, BLOCK_MS);
      }
      // Loop summaries are posted once a second of host time.
      await page.waitForTimeout(1_500);
      const entries = (await page.evaluate(
        (log) => (globalThis as unknown as Record<string, unknown>)[log] ?? [],
        TRACE_LOG,
      )) as AudioTraceEntry[];
      const summaries = entries.filter(
        (entry): entry is Extract<AudioTraceEntry, { kind: "summary" }> =>
          entry.src === "loop" && entry.kind === "summary" && (entry.atMs ?? 0) >= firstBlock,
      );
      expect(summaries.length, "the Worker posted loop summaries over the blocks").toBeGreaterThanOrEqual(2);
      const worstTurn = Math.max(...summaries.map((summary) => summary.max.yieldMs));
      const turn = summaries[0]?.yielder ?? "";
      console.log(
        `RECORDED ${browserName} worst event-loop turn ${worstTurn.toFixed(2)} ms (${turn}) over ${summaries.length} s with the page's main thread blocked ${BLOCKS} x ${BLOCK_MS} ms`,
      );
      // Firefox's `waitAsync` turn runs none of the Worker's other tasks, so its loop takes the
      // MessageChannel turn.
      const expected = browserName === "firefox" ? "atomics-wait/message-channel" : "atomics-wait/wait-async";
      expect(turn, "the isolated loop takes the turn its engine's probe chose").toBe(expected);
      // Through a MessagePort, WebKit held a turn for the whole 50 ms block; a Worker-only turn is tens of
      // microseconds.
      expect(worstTurn, "no turn waited for the page's main thread").toBeLessThan(BLOCK_MS / 2);
    } finally {
      await served.close();
    }
  });
});
