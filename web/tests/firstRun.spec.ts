// The browser half of the first run on a clean macOS environment.
// `t1_m9a_first_run_on_a_clean_environment` (`xtask/src/package/tests.rs`) covers the rest
// natively; `cargo xtask ci t1` builds the package and names it in `PEMU_E2E_PACKAGE`.
//
// The packaged `passportsim serve` gets exactly `HOME` (a fresh empty directory), `PATH`
// (`/usr/bin:/bin` plus the package) and `TMPDIR` inside that `HOME`: no `IDF_*`, no
// `PASSPORTSIM_*`, no `~/.espressif`, no `~/.config/passportsim`.
//
// Asserted:
// 1. The daemon serves its embedded payload and prints the launch URL.
// 2. With no file dropped the page boots the bundled demo to the settled menu: `official`'s ready
//    line, and a raw frame equal to `tests/golden/official/menu.png`.
// 3. Dropping the `official` merged bin reboots into it, with the same ready line and golden.
// 4. Every receipt lists only bundled assets or the dropped image (`@report` roles before and after
//    the drop), and every request went to the daemon's own origin.
// 5. The `HOME` holds no `.espressif` and no `.config` afterwards.
// The raw frame is taken through Playwright's handle on the Worker, so page traffic is untouched.
//
// Skips, each named: no Playwright Chromium; no `PEMU_E2E_PACKAGE`; no `official` merged bin.
// A named package directory that is not a package fails.

import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { officialDir } from "./demoBundle";
import { browserGaps, call, loaderState, pinPrefs, waitForLoaded, waitForStatus, watchPage } from "./harness";
import { decodeRgbPng, differingPixels } from "./png";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(WEB, "..");

/** The variable `xtask ci t1` names the package directory with (`xtask/src/ci/tiers.rs`). */
const PACKAGE_VARIABLE = "PEMU_E2E_PACKAGE";

/** The `official` console line the settled menu follows (`main.c`). */
const OFFICIAL_READY = "main: 就绪";

/** The merged image an `idf.py` build directory of `official` holds. */
const MERGED_IMAGE = "FoloToy-AI-Passport-8MB.bin";

/** The name the page's loader gives a dropped loose file: the file name without its extension. */
const DROPPED_NAME = "FoloToy-AI-Passport-8MB";

const BOOT_MS = 90_000;

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

interface Served {
  readonly child: ChildProcess;
  readonly home: string;
  readonly ui: string;
  readonly origin: string;
  readonly output: () => string;
}

/** Starts `<package>/passportsim serve` in the clean environment and waits for its `ui:` line. */
async function serve(pkg: string): Promise<Served> {
  const home = mkdtempSync(join(tmpdir(), "pemu-package-home-"));
  expect(readdirSync(home), "the HOME is a new empty directory").toEqual([]);
  const child = spawn(join(pkg, "passportsim"), ["serve"], {
    cwd: pkg,
    // The whole environment: `spawn` with `env` inherits nothing else.
    env: { HOME: home, PATH: `/usr/bin:/bin:${pkg}`, TMPDIR: home },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const chunks: string[] = [];
  child.stdout?.setEncoding("utf8");
  child.stderr?.setEncoding("utf8");
  child.stdout?.on("data", (chunk: string) => chunks.push(chunk));
  child.stderr?.on("data", (chunk: string) => chunks.push(chunk));
  const output = () => chunks.join("");
  const deadline = Date.now() + 60_000;
  let ui: string | undefined;
  while (ui === undefined && Date.now() < deadline && child.exitCode === null) {
    ui = output()
      .split("\n")
      .map((line) => line.trim())
      .find((line) => line.startsWith("ui:"))
      ?.slice("ui:".length)
      .trim();
    if (ui === undefined) {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  if (ui === undefined) {
    child.kill("SIGKILL");
    throw new Error(`the packaged daemon printed no \`ui:\` line (exit ${child.exitCode}):\n${output()}`);
  }
  expect(output(), "the packaged daemon serves its own embedded payload").toContain("payload: embedded");
  expect(ui, "the UI line is a loopback URL carrying a launch code").toMatch(/^http:\/\/127\.0\.0\.1:\d+\/#lc=\S+$/);
  return { child, home, ui, origin: new URL(ui).origin, output };
}

/** Stops the daemon as `part_four` does, `serve --stop` in the same environment, then waits. */
async function stop(pkg: string, served: Served): Promise<void> {
  const stopper = spawn(join(pkg, "passportsim"), ["serve", "--stop"], {
    cwd: pkg,
    env: { HOME: served.home, PATH: `/usr/bin:/bin:${pkg}`, TMPDIR: served.home },
    stdio: "ignore",
  });
  await new Promise<void>((resolve) => stopper.once("exit", () => resolve()));
  if (served.child.exitCode === null) {
    const exited = new Promise<void>((resolve) => served.child.once("exit", () => resolve()));
    const timer = setTimeout(() => served.child.kill("SIGKILL"), 10_000);
    await exited;
    clearTimeout(timer);
  }
}

interface RawFrame {
  readonly width: number;
  readonly height: number;
  readonly generation: string;
  readonly pixels: number[];
}

/** Asks the page's Worker for its `raw` frame, taking the answer before it is posted to the page. */
async function workerFrame(page: Page): Promise<RawFrame> {
  const worker = page.workers().find((w) => new URL(w.url()).pathname.endsWith("/worker.js"));
  expect(worker, `the page runs its Worker: ${page.workers().map((w) => w.url()).join(", ")}`).toBeDefined();
  return (worker as NonNullable<typeof worker>).evaluate(
    () =>
      new Promise<RawFrame>((resolve) => {
        type Scope = {
          postMessage: (message: unknown, transfer?: unknown) => void;
          onmessage: ((event: { data: unknown }) => void) | null;
        };
        const scope = globalThis as unknown as Scope;
        const original = scope.postMessage;
        scope.postMessage = (message: unknown, transfer?: unknown) => {
          const m = message as { type?: string; width?: number; height?: number; generation?: string; pixels?: Uint16Array | null };
          if (m?.type === "frame") {
            scope.postMessage = original;
            resolve({
              width: m.width ?? 0,
              height: m.height ?? 0,
              generation: m.generation ?? "0",
              pixels: m.pixels ? Array.from(m.pixels) : [],
            });
            return;
          }
          original.call(scope, message, transfer);
        };
        scope.onmessage?.({ data: { type: "frame" } });
      }),
  );
}

/** Waits for the raw frame to equal `menu.png`; a frame that never does fails with its last difference. */
async function expectSettledMenu(page: Page, leg: string): Promise<RawFrame> {
  const golden = decodeRgbPng(readFileSync(join(REPO, "tests", "golden", "official", "menu.png")));
  const deadline = Date.now() + BOOT_MS;
  let last: { frame: RawFrame; differing: number; first: [number, number] | null } | null = null;
  while (Date.now() < deadline) {
    const frame = await workerFrame(page);
    const { differing, first } = differingPixels(frame, golden);
    last = { frame, differing, first };
    if (frame.width > 0 && differing === 0) {
      console.log(
        `RAN ${leg}: the raw frame at generation ${frame.generation} equals tests/golden/official/menu.png`,
      );
      return frame;
    }
    await page.waitForTimeout(250);
  }
  throw new Error(
    `${leg}: the raw frame never equalled menu.png in ${BOOT_MS} ms; the last one ` +
      `(${last?.frame.width}x${last?.frame.height}, generation ${last?.frame.generation}) differs in ` +
      `${last?.differing} pixels, first at ${JSON.stringify(last?.first)}`,
  );
}

async function expectReadyLine(page: Page, leg: string): Promise<void> {
  await expect
    .poll(
      async () => {
        const read = await call(page, "serial", { op: "read" });
        return read.ok ? JSON.stringify(read.json) : `serial: ${read.error}`;
      },
      { timeout: BOOT_MS, message: `${leg}: the machine's console reaches ${OFFICIAL_READY}` },
    )
    .toContain(OFFICIAL_READY);
}

/** The roles the core loaded, from its determinism report (`@report`). */
async function loadedRoles(page: Page): Promise<string[]> {
  const report = await call(page, "@report", {});
  expect(report.ok, `the core answers @report: ${report.ok ? "" : report.error}`).toBe(true);
  return ((report.ok ? report.json : {}) as { roles?: string[] }).roles ?? [];
}

async function runningFw(page: Page): Promise<unknown> {
  const status = await call(page, "status", {});
  expect(status.ok, `status answers: ${status.ok ? "" : status.error}`).toBe(true);
  const rows = ((status.ok ? status.json : {}) as { instances?: { fw?: unknown }[] }).instances ?? [];
  return rows[0]?.fw;
}

test(
  "headless Chrome opens the web bundle the packaged binary serves in a clean environment, boots the demo to menu.png and reboots into a dropped official image",
  { tag: "@chromium-only" },
  async ({ page }) => {
    test.setTimeout(300_000);
    const pkg = process.env[PACKAGE_VARIABLE];
    test.skip(
      pkg === undefined || pkg === "",
      `no ${PACKAGE_VARIABLE}: the package is built by \`cargo xtask ci t1\` (step package-smoke)`,
    );
    const dir = officialDir();
    const merged = dir ? join(dir, MERGED_IMAGE) : undefined;
    test.skip(
      merged === undefined || !existsSync(merged),
      `no \`official\` ${MERGED_IMAGE}: set PEMU_E2E_OFFICIAL_DIR or PASSPORTSIM_DATA_ROOT (FoloToy builds are never committed)`,
    );
    if (!pkg || !merged) {
      return;
    }
    expect(
      existsSync(join(pkg, "passportsim")) && statSync(join(pkg, "passportsim")).isFile(),
      `${PACKAGE_VARIABLE} names ${pkg}, which holds no \`passportsim\`: the run asked for that package`,
    ).toBe(true);

    const served = await serve(pkg);
    console.log(
      `RAN serve: ${join(pkg, "passportsim")} in an empty HOME, browser ${page.context().browser()?.browserType().name()} ${page.context().browser()?.version()}`,
    );
    const requests: string[] = [];
    page.on("request", (request) => requests.push(request.url()));
    try {
      // (2) The launch URL with no file dropped. Watched before navigating, so a failed `waitForLoaded`
      // can name the failed requests.
      watchPage(page);
      await page.setViewportSize({ width: 1280, height: 900 });
      await pinPrefs(page);
      await page.goto(served.ui);
      await expect(page.locator("#app .app"), "the shell mounted").toBeVisible({ timeout: 30_000 });
      await page.waitForFunction(() => typeof (globalThis as { passportEmu?: unknown }).passportEmu === "object");
      expect(await page.evaluate(() => globalThis.crossOriginIsolated), "the daemon's page is cross-origin isolated").toBe(
        true,
      );
      // The bundled demo is no load of the strip's, so the machine itself is asked what it runs.
      const booted = await waitForStatus(page, BOOT_MS);
      expect(booted.ok, `status answers once the demo is up: ${booted.ok ? "" : booted.error}`).toBe(true);
      expect(await runningFw(page), "with no file dropped the page runs the bundled demo").toBe("official");
      await expectReadyLine(page, "demo");
      const demoFrame = await expectSettledMenu(page, "demo-menu-golden");
      // The UI tree tab on the settled menu: the pruned tree of 17 lines
      // (`t1_m5_official_menu_tree_prunes_to_17_lines`), the Display card's label among its rows, and
      // hovering a row outlines it on the glass. Back to the console before the drop replaces the machine.
      await page.locator("#tab-ui-tree").click();
      const displayRow = page.locator('#pane-ui-tree .ui-row[data-class="label"]', { hasText: 'label "Display" [' });
      await expect(displayRow, "the UI tree tab shows the menu's Display label").toBeVisible({ timeout: 30_000 });
      const treeRows = await page.locator("#pane-ui-tree .ui-row").count();
      expect(treeRows, "the official menu prunes to 17 lines").toBe(17);
      await displayRow.hover();
      await expect(page.locator(".skin-screen .ui-highlight"), "hovering the row outlines it on the glass").toBeVisible();
      console.log(
        `RAN ui-tree: ${treeRows} rows on the UI tree tab, ${await page.locator("[data-ui-summary]").textContent()}`,
      );
      await page.locator("#tab-console").click();
      // (4) The demo's receipt: the roles of the packaged demo bundle and nothing else.
      const demoRoles = await loadedRoles(page);
      expect(demoRoles, "the demo boots from the packaged bundle's flash image and app ELF").toEqual(["flash", "app_elf"]);
      console.log(`RAN demo: the bundled demo booted with roles ${JSON.stringify(demoRoles)}`);

      // (3) Dropping the `official` merged bin reboots into it.
      await page.locator('[data-loader-input="files"]').setInputFiles(merged);
      await waitForLoaded(page, DROPPED_NAME, BOOT_MS);
      expect((await loaderState(page)).image).toBe(DROPPED_NAME);
      expect(await runningFw(page), "the page runs the dropped image").toBe(DROPPED_NAME);
      await expectReadyLine(page, "dropped");
      const droppedFrame = await expectSettledMenu(page, "dropped-menu-golden");
      const droppedRoles = await loadedRoles(page);
      expect(droppedRoles, "the dropped image is the only role the rebooted machine loaded").toEqual(["flash"]);
      console.log(
        `RAN dropped: ${MERGED_IMAGE} rebooted the page from generation ${demoFrame.generation} ` +
          `to a new machine at generation ${droppedFrame.generation}, roles ${JSON.stringify(droppedRoles)}`,
      );

      // (4) Every request went to the packaged daemon.
      const foreign = requests.filter((url) => !url.startsWith(`${served.origin}/`) && !url.startsWith("blob:"));
      expect(foreign, `every request is to ${served.origin}`).toEqual([]);
      console.log(`RAN origin: ${requests.length} requests, all to the packaged daemon at ${served.origin}`);
    } finally {
      await page.close();
      await stop(pkg, served);
    }
    // (5) A first run grows no toolchain or configuration directory in the empty HOME.
    for (const absent of [".espressif", ".config", "esp-idf"]) {
      expect(existsSync(join(served.home, absent)), `the first run created ~/${absent}`).toBe(false);
    }
    rmSync(served.home, { recursive: true, force: true });
  },
);
