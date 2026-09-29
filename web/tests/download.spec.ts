// A first visit on a slow network: while the core and the demo firmware download, the page says so
// with the bytes received against the size, on the status line, the glass and the log, and never
// reads "Paused" before the machine runs. The network is slowed through the Chrome DevTools
// Protocol, which also slows the Worker's fetches, so the test runs in Chromium only. It needs the
// wasm core and the `official` demo bundle the server builds from the corpus; without either it skips.
// A download cut off shows its error on the glass, whose retry then boots the demo.

import { expect, test } from "@playwright/test";
import { readOfficialFiles } from "./demoBundle";
import { browserGaps, openPage } from "./harness";
import { findCore } from "./preconditions";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

/** Bytes per second: the 25 MB demo takes about 4 s, long enough to see several reports. */
const THROUGHPUT = 6_000_000;

const AMOUNT = /\d+\.\d \/ \d+\.\d MB \(\d+%\)$/;

test.describe("a slow first download", () => {
  test("shows what it downloads and how far, then the machine runs, never paused before @chromium-only", async ({ page, context }) => {
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    const official = readOfficialFiles();
    test.skip(!("files" in official), "skip" in official ? official.skip : "mismatch" in official ? official.mismatch : "");
    test.setTimeout(120_000);

    // Every state the status line takes, from the page's first render.
    await page.addInitScript(() => {
      const seen: string[] = [];
      (globalThis as { __statusStates?: string[] }).__statusStates = seen;
      const note = () => {
        const state = document.querySelector(".sim-status [data-state]")?.getAttribute("data-state");
        if (state && seen.at(-1) !== state) {
          seen.push(state);
        }
      };
      new MutationObserver(note).observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ["data-state"] });
    });
    const cdp = await context.newCDPSession(page);
    await cdp.send("Network.enable");
    await cdp.send("Network.emulateNetworkConditions", {
      offline: false,
      latency: 20,
      downloadThroughput: THROUGHPUT,
      uploadThroughput: THROUGHPUT,
    });

    await openPage(page, 1440, 900, "simple");
    const status = page.locator(".sim-status [data-status-text]");
    await expect(status).toHaveText(new RegExp(`^Downloading the demo firmware ${AMOUNT.source}`), { timeout: 30_000 });
    const glass = page.locator('[data-glass-boot="firmware"]');
    await expect(glass).toContainText("Downloading the demo firmware");
    await expect(glass.locator("[data-glass-amount]")).toHaveText(AMOUNT);
    await expect(glass.locator('[role="progressbar"]')).toBeVisible();
    await expect(page.locator('.log-panel .log-page[data-step="download"]').last()).toContainText(/Downloading the demo firmware \d+\.\d \/ \d+\.\d MB/);
    await page.screenshot({ path: test.info().outputPath("downloading.png") });

    // The numbers move: a later reading has more bytes.
    const bytes = async () => Number((await status.textContent())?.match(/(\d+\.\d) \//)?.[1] ?? Number.NaN);
    const first = await bytes();
    await expect.poll(bytes, { timeout: 15_000 }).toBeGreaterThan(first);

    await expect(page.locator(".sim-status [data-state]")).toHaveAttribute("data-state", "running", { timeout: 60_000 });
    await expect(page.locator("[data-glass-boot]")).toHaveCount(0);
    const lines = page.locator('.log-panel .log-page[data-step="download"]');
    await expect(lines).toHaveCount(2);
    await expect(lines.first()).toContainText(/^.*Downloaded the emulator core \(\d+\.\d MB\)$/);
    await expect(lines.last()).toContainText(/^.*Downloaded the demo firmware \(\d+\.\d MB\)$/);

    const states = await page.evaluate(() => (globalThis as { __statusStates?: string[] }).__statusStates ?? []);
    expect(states[0]).toBe("starting");
    expect(states.slice(0, states.indexOf("running"))).not.toContain("paused");
    expect(states).toContain("running");
  });

  test("a download cut off says why, and its retry boots the demo @chromium-only", async ({ page }) => {
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    const official = readOfficialFiles();
    test.skip(!("files" in official), "skip" in official ? official.skip : "mismatch" in official ? official.mismatch : "");
    test.setTimeout(120_000);

    // The first request is the page's probe of whether the demo is served; the second, the Worker's
    // download, is cut off.
    let requests = 0;
    await page.route("**/official.pebundle", async (route) => {
      requests += 1;
      await (requests === 2 ? route.abort("connectionreset") : route.continue());
    });
    await openPage(page, 1440, 900, "simple");
    const glass = page.locator('[data-glass-boot="failed"]');
    await expect(glass).toContainText("The emulator could not start", { timeout: 30_000 });
    await expect(glass).toContainText("Could not download the demo firmware");
    await expect(page.locator(".sim-status [data-state]")).toHaveAttribute("data-state", "failed");

    await glass.locator('[data-action="retry"]').click();
    await expect(page.locator(".sim-status [data-state]")).toHaveAttribute("data-state", "running", { timeout: 60_000 });
    await expect(page.locator("[data-glass-boot]")).toHaveCount(0);
    expect(requests).toBe(3);
  });
});
