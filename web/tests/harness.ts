// What every Playwright test needs from the page. `requireImage` puts the firmware in through the
// loader's file inputs (the door a drag uses, drivable on every engine), waits for the page to say
// it runs that image and checks `status` agrees, so a page that kept the bundled demo fails. It
// skips only for the two named absences of `preconditions.ts`.

import { expect, test, type Page } from "@playwright/test";
import { browserSkip, missingBrowser, rowOf, runnableRows } from "./browsers";
import { currentPrecondition, type ImageSource, type RowNeeds } from "./preconditions";
import { descendants, holdFullSpeed, processTable } from "./processCpu";

/**
 * Pins the page's remembered mode and language before any document runs, so a test that navigates
 * by itself (a daemon's launch URL drops any query) still gets the mode it drives, in English.
 */
export async function pinPrefs(page: Page, mode: "simple" | "advanced" = "advanced"): Promise<void> {
  await page.addInitScript((chosen) => {
    try {
      window.localStorage.setItem("passportsim.mode", chosen);
      window.localStorage.setItem("passportsim.locale", "en");
    } catch {
      // A context with storage blocked runs on the URL and `navigator.languages` instead.
    }
  }, mode);
}

/**
 * Opens the page and waits for the shell. On Windows it asks the system not to throttle the
 * browser (`processCpu.ts` `holdFullSpeed`): Windows 11 runs a browser with no visible window
 * under EcoQoS, and host-time figures would read a background process.
 */
export async function openPage(page: Page, width = 1280, height = 900, mode: "simple" | "advanced" = "advanced"): Promise<void> {
  recordConsole(page);
  await pinPrefs(page, mode);
  await page.setViewportSize({ width, height });
  await page.goto(`/?mode=${mode}&lang=en`);
  await expect(page.locator("#app .app")).toBeVisible();
  await page.waitForFunction(() => typeof (globalThis as { passportEmu?: unknown }).passportEmu === "object");
  if (process.platform === "win32") {
    const held = holdFullSpeed(descendants(processTable(), process.pid));
    if (held !== null && held.failed.length > 0) {
      console.log(`power throttling stays on for ${JSON.stringify(held.failed)} (pid, Win32 error)`);
    }
  }
}

/** Every console line and page error of one page, so a timeout can say what the page was doing. */
const consoles = new WeakMap<Page, string[]>();

/**
 * Every request of one page that failed or answered 400 or more, by URL, since the console's 404
 * names no resource. Kept apart from the console tail, which a navigation failure would scroll.
 */
const failedRequests = new WeakMap<Page, string[]>();

/**
 * Starts recording what `page` says and which requests fail. Call it before the first navigation,
 * as [`openPage`] does, or the load's own requests are missed.
 */
export function watchPage(page: Page): void {
  recordConsole(page);
}

function recordConsole(page: Page): void {
  if (consoles.has(page)) {
    return;
  }
  const lines: string[] = [];
  consoles.set(page, lines);
  const keep = (line: string) => {
    // Bounded, keeping the last lines: they say what stopped.
    lines.push(line);
    if (lines.length > 200) {
      lines.splice(0, lines.length - 200);
    }
  };
  const failed: string[] = [];
  failedRequests.set(page, failed);
  const fail = (line: string) => {
    // Bounded too, but keeping the first ones: the request that broke a load is an early one.
    if (failed.length < 50) {
      failed.push(line);
    }
    keep(line);
  };
  page.on("console", (message) => keep(`${message.type()}: ${message.text()}`));
  page.on("pageerror", (error) =>
    // With the stack: the message alone names neither the port nor the site that transferred it twice.
    keep(`pageerror: ${error.message}\n      ${(error.stack ?? "(no stack)").split("\n").slice(0, 8).join("\n      ")}`),
  );
  page.on("crash", () => keep("crash: the page process went away"));
  page.on("worker", (worker) => keep(`worker: ${worker.url()}`));
  page.on("requestfailed", (request) =>
    fail(`request failed: ${request.method()} ${request.url()}: ${request.failure()?.errorText ?? "(no reason given)"}`),
  );
  page.on("response", (response) => {
    if (response.status() >= 400) {
      fail(`http ${response.status()}: ${response.request().method()} ${response.url()}`);
    }
  });
}

function consoleTail(page: Page): string {
  const lines = consoles.get(page) ?? [];
  return lines.length === 0 ? "(nothing)" : lines.slice(-40).join("\n    ");
}

function failedTail(page: Page): string {
  const failed = failedRequests.get(page);
  if (failed === undefined) {
    return "(not recorded: the page was not watched before it loaded; see `watchPage`)";
  }
  return failed.length === 0 ? "(none)" : failed.join("\n    ");
}

/**
 * The rows of this host whose browser is not installed, each with its skip reason; empty when all
 * are here or `PEMU_E2E_REQUIRE_BROWSERS=1`. Chrome and Edge share Chromium's engine, so projects
 * are told apart by `browserName` and `channel`:
 *
 * ```ts
 * for (const { applies, gap } of browserGaps()) {
 *   test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
 * }
 * ```
 */
export function browserGaps(): { readonly applies: (browserName: string, channel: string | undefined) => boolean; readonly gap: string }[] {
  return runnableRows().flatMap((row) => {
    const gap = browserSkip(missingBrowser(row));
    return gap === null ? [] : [{ applies: (browserName: string, channel: string | undefined) => rowOf(browserName, channel) === row, gap }];
  });
}

/** One `passportEmu.call` from the page, with the error text instead of a throw. */
export async function call(page: Page, name: string, args: unknown): Promise<{ ok: true; json: unknown } | { ok: false; error: string }> {
  return page.evaluate(
    async ({ name, args }) => {
      const api = (globalThis as unknown as { passportEmu: { call(n: string, a: unknown): Promise<{ json: unknown }> } }).passportEmu;
      try {
        const out = await Promise.race([
          api.call(name, args),
          new Promise<never>((_, reject) => setTimeout(() => reject(new Error("no answer within 15 s")), 15_000)),
        ]);
        return { ok: true as const, json: out.json };
      } catch (error) {
        const body = (error as { body?: { code?: string; message?: string } }).body;
        return { ok: false as const, error: body ? `${body.code}: ${body.message}` : String(error) };
      }
    },
    { name, args },
  );
}

export async function waitForStatus(page: Page, timeoutMs = 30_000): Promise<{ ok: true; json: unknown } | { ok: false; error: string }> {
  // Until the Worker's machine is up `status` answers `E_STATE: no machine is booted`; only that
  // answer is waited out.
  let status = await call(page, "status", {});
  const deadline = Date.now() + timeoutMs;
  while (!status.ok && status.error.startsWith("E_STATE: no machine is booted") && Date.now() < deadline) {
    await page.waitForTimeout(100);
    status = await call(page, "status", {});
  }
  return status;
}

/**
 * How long one read of the page may take before it counts as not answering. The config sets no
 * `actionTimeout`, and a WebKit page whose main thread stopped would otherwise hold a locator call
 * until the test's own timeout.
 */
export const ANSWER_MS = 5_000;

function firstLine(error: unknown): string {
  return (error instanceof Error ? error.message : String(error)).split("\n")[0] ?? "";
}

async function answered<T>(what: string, work: Promise<T>, ms = ANSWER_MS): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      work,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${what}: no answer within ${ms} ms`)), ms);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/** The loader's state, image and message (`Firmware.tsx`), or a rejection after `ms` without an answer. */
export async function loaderState(page: Page, ms = ANSWER_MS): Promise<{ state: string; image: string; message: string }> {
  const strip = page.locator("[data-loader]");
  const read = async () => ({
    state: (await strip.getAttribute("data-loader-state", { timeout: ms })) ?? "",
    image: (await strip.getAttribute("data-loader-image", { timeout: ms })) ?? "",
    message: (await strip.locator("[data-loader-message]").textContent({ timeout: ms })) ?? "",
  });
  return answered("reading the loader strip", read(), ms);
}

/**
 * Whether the page's main thread and each Worker still answer, one line each, asked in parallel
 * so a silent page costs one [`ANSWER_MS`].
 */
async function answering(page: Page): Promise<string> {
  const ask = (what: string, work: Promise<unknown>) =>
    answered(what, work).then(
      (value) => `${what}: answers (${String(value)})`,
      (error: unknown) => firstLine(error),
    );
  const lines = await Promise.all([
    ask("the page's main thread", page.evaluate(() => document.readyState)),
    ...page.workers().map((worker) => ask(`the worker ${worker.url()}`, worker.evaluate(() => "alive"))),
  ]);
  return lines.join("\n    ");
}

/**
 * Puts `source` into the page through the loader and waits for it to be running. A directory goes
 * to the `webkitdirectory` input, so the page sees the paths an `idf.py` build's `flash_files`
 * resolve against; anything else to the files input. A refusal fails with the page's own message.
 */
export async function loadImage(page: Page, source: ImageSource): Promise<void> {
  const selector = source.directory ? '[data-loader-input="directory"]' : '[data-loader-input="files"]';
  const files = source.directory ? [source.root] : source.files.map((file) => file.path);
  await page.locator(selector).setInputFiles(files);
  await waitForLoaded(page, source.name);
}

/**
 * Waits for the loader strip to say `name` is running. It waits by name because the strip also
 * states machine-level refusals (a run without the bundled demo opens on its 404), which must not
 * read as the answer to this load. A refusal of this load fails at once.
 */
export async function waitForLoaded(page: Page, name: string, timeoutMs = 60_000): Promise<void> {
  const started = Date.now();
  await expect
    .poll(
      async () => {
        // An unanswered read is one more state, polled again until the deadline.
        const strip = await loaderState(page).catch((error: unknown) => new Error(firstLine(error)));
        if (strip instanceof Error) {
          return `not answering: ${strip.message}`;
        }
        if (strip.state === "loaded" && strip.image === name) {
          return "loaded";
        }
        if (strip.state === "refused" || (strip.state === "error" && strip.message.includes(name))) {
          return `the page refused \`${name}\`: ${strip.message}`;
        }
        return `${strip.state}: ${strip.message}`;
      },
      { timeout: timeoutMs },
    )
    .toBe("loaded")
    .catch(async (failure: unknown) => {
      // The strip's text alone says only that the boot never settled, so the page's console, the failed
      // requests and whether it still answers go with it. Every read is bounded.
      const [strip, threads] = await Promise.all([
        loaderState(page).then(
          (read) => JSON.stringify(read),
          (error: unknown) => `(unread) ${firstLine(error)}`,
        ),
        answering(page),
      ]);
      throw new Error(
        `${String(failure)}\n\n` +
          `waitForLoaded(${JSON.stringify(name)}) gave up after ${Date.now() - started} ms (limit ${timeoutMs} ms).\n` +
          `  strip: ${strip}\n` +
          `  answering:\n    ${threads}\n` +
          `  failed requests:\n    ${failedTail(page)}\n` +
          `  page console, last 40 lines:\n    ${consoleTail(page)}`,
      );
    });
}

/**
 * Skips for a named absence, or loads `image` through the loader and asserts `status` names it. A
 * refusal, a `status` error or a page still on the bundled demo fails.
 */
export async function requireImage(page: Page, image: string, row: RowNeeds): Promise<void> {
  const pre = currentPrecondition(image, row);
  test.skip(pre.kind === "skip", pre.kind === "skip" ? pre.reason : "");
  if (pre.kind !== "run") {
    return;
  }
  expect(pre.source.files.length, `${pre.source.variable} names nothing the loader can read`).toBeGreaterThan(0);
  // The bundled demo is not waited for: a run may have this image and not the `official` build.
  await loadImage(page, pre.source);
  const status = await waitForStatus(page);
  expect(status, "`status` must answer once the loaded image is running").toMatchObject({ ok: true });
  expect(
    JSON.stringify(status.ok ? status.json : null),
    `the page must run \`${pre.source.name}\` from ${pre.source.variable}, not the bundled demo`,
    // `"fw":"<name>"` exactly, so the name cannot match elsewhere in the answer.
  ).toContain(`"fw":"${pre.source.name}"`);
}

/** Selects a panel tab; at a narrow width the strip scrolls, and the click scrolls it into view. */
export async function showTab(page: Page, id: string): Promise<void> {
  await page.locator(`#tab-${id}`).click();
  await expect(page.locator(`#pane-${id}`)).toBeVisible();
}

/**
 * Opens a card, whatever state the narrow layout left it in, and returns its section. The page
 * grows under the toggle, so a click can land as `mousedown` on the button and `mouseup` on the
 * section below, and WebKit then fires no `click`. So it clicks only while the card is shut, and
 * gives up after a few presses with a message.
 */
export async function openCard(page: Page, id: string) {
  const section = page.locator(`section.card[data-card="${id}"]`);
  const toggle = section.locator("button.card-toggle");
  const body = section.locator(`#card-${id}`);
  const PRESSES = 4;
  for (let press = 0; press < PRESSES; press += 1) {
    if (await body.isVisible()) {
      return section;
    }
    await toggle.click();
  }
  await expect(body, `the ${id} card did not open after ${PRESSES} presses of its toggle`).toBeVisible();
  return section;
}

export async function consoleText(page: Page): Promise<string> {
  return (await page.locator("#pane-console").textContent()) ?? "";
}

/**
 * Prints and annotates a leg that cannot run on this tree as `NOT_RUN <row> <leg>: <reason>`, which
 * `xtask ci` reads (`web.rs` `not_run_of`).
 */
export function notRun(row: string, leg: string, reason: string): void {
  console.log(`NOT_RUN ${row} ${leg}: ${reason}`);
  test.info().annotations.push({ type: "NOT_RUN", description: `${row} ${leg}: ${reason}` });
}

export async function waitForLine(page: Page, pattern: RegExp, timeout: number): Promise<void> {
  await expect.poll(() => consoleText(page), { timeout }).toMatch(pattern);
}

/**
 * Types `line` into the console input and presses Enter (`serial write`); the console pane must be
 * shown. The console view adds the newline, so write the line as a person types it.
 */
export async function sendConsoleLine(page: Page, line: string): Promise<void> {
  const entry = page.getByLabel("Console input");
  await entry.fill(line);
  await entry.press("Enter");
  await expect(entry, "the console input clears once the line is sent").toHaveValue("");
}

/** The guest's virtual time from `status`, in microseconds, or `null` while no machine answers. */
export async function virtualUs(page: Page): Promise<number | null> {
  const status = await call(page, "status", {});
  if (!status.ok) {
    return null;
  }
  const instances = (status.json as { instances?: { vt_us?: unknown }[] }).instances ?? [];
  const vt = instances[0]?.vt_us;
  return typeof vt === "number" ? vt : null;
}

/**
 * Presses a skin control and releases it once the guest has run `ms` past the press, since a
 * wall-time hold is shorter in guest time on a slow machine. The `status` read queues after the
 * press. With no machine answering, the hold is `ms` of wall time.
 */
export async function holdControl(page: Page, control: string, ms: number): Promise<void> {
  const node = page.locator(`[data-control="${control}"]`);
  await node.dispatchEvent("pointerdown");
  const pressed = await virtualUs(page);
  await page.waitForTimeout(ms);
  if (pressed !== null) {
    await expect
      .poll(async () => (await virtualUs(page)) ?? pressed, { timeout: 30_000 })
      .toBeGreaterThanOrEqual(pressed + ms * 1_000);
  }
  await node.dispatchEvent("pointerup");
}
