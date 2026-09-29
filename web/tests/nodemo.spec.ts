// The bundle served without `official.pebundle`: the page boots nothing, says what starts it, and
// draws the first firmware it is given (the glass canvas can be transferred only once, so a
// failed demo boot once left it black for every later image). The demo is removed with a route,
// so this runs whether or not the host can build it.

import { expect, test } from "@playwright/test";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { browserGaps, consoleText, openPage, waitForLine, waitForStatus } from "./harness";
import { decodeRgbPng } from "./png";
import { findCore } from "./preconditions";

/** Where the owner keeps the firmware, outside the tree: the variable, else their Downloads. */
const OWNER_FIRMWARE = process.env.PEMU_E2E_OWNER_BIN ?? join(homedir(), "Downloads", "passport-keys-firmware-1.0.0.bin");

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

/** Whether every pixel of a glass screenshot is one colour, which a drawn screen never is. */
function blank(shot: Buffer): boolean {
  const { rgb } = decodeRgbPng(shot);
  return rgb.every((value, at) => value === rgb[at % 3]);
}

test("with no demo served the page waits for a firmware, then draws the first one it is given", async ({ page }) => {
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  test.skip(!existsSync(OWNER_FIRMWARE), `the owner's firmware is not at ${OWNER_FIRMWARE}; set PEMU_E2E_OWNER_BIN to a bare merged .bin`);
  test.setTimeout(120_000);
  const demoRequests: string[] = [];
  await page.route("**/official.pebundle", async (route) => {
    demoRequests.push(route.request().method());
    await route.fulfill({ status: 404, body: "not found" });
  });

  await openPage(page, 1440, 900, "simple");
  const loader = page.locator("[data-loader]");
  await expect(loader).toHaveAttribute("data-loader-state", "empty");
  await expect(page.locator("[data-glass-empty]")).toHaveText("Drop firmware to start");
  await expect(page.locator(".sim-status [data-state]")).toHaveAttribute("data-state", "empty");
  await expect(page.locator("[data-loader-message]")).toContainText("served without the demo firmware");
  await expect(page.locator('[data-action="back-to-demo"]')).toHaveCount(0);
  for (const id of ["up", "ok", "down", "power"]) {
    await expect(page.locator(`[data-control=${id}]`)).toBeDisabled();
  }
  // The page asked whether the demo is there, once, and booted nothing.
  expect(demoRequests).toEqual(["GET"]);
  expect(await waitForStatus(page, 1_000)).toMatchObject({ ok: false, error: expect.stringContaining("no machine is booted") });

  await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
  await expect(loader).toHaveAttribute("data-loader-state", "loaded", { timeout: 60_000 });
  await expect(page.locator("[data-glass-empty]")).toHaveCount(0);
  await waitForLine(page, /pk_app: ready/, 40_000);
  await expect
    .poll(async () => blank(await page.locator("canvas.glass:not(.rewind-frame)").screenshot()), {
      timeout: 10_000,
      message: "the first firmware draws on the glass",
    })
    .toBe(false);
  for (const id of ["up", "ok", "down", "power"]) {
    await expect(page.locator(`[data-control=${id}]`)).toBeEnabled();
  }
  expect(await consoleText(page), "no panic on the way").not.toMatch(/Guru Meditation|panic/i);
});
