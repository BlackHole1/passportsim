// The page is honest about a machine it cannot fully read. The image is the owner's bare merged
// `.bin` with no ELF (Passport Keys 1.0.0), read from outside the tree; without it or a wasm core
// the file skips. Its radio binds from the image, so it runs past BLE init and answers a button,
// and the page still says what only an ELF gives: the UI tree and settle detection.

import { expect, test } from "@playwright/test";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { runsPastBleInitAndAnswersDown } from "./elfless";
import { browserGaps, openPage } from "./harness";
import { findCore } from "./preconditions";

/** Where the owner keeps the firmware, outside the tree: the variable, else their Downloads. */
const OWNER_FIRMWARE = process.env.PEMU_E2E_OWNER_BIN ?? join(homedir(), "Downloads", "passport-keys-firmware-1.0.0.bin");

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

test.describe("a bare .bin with no ELF", () => {
  test("says which features need the ELF, runs past BLE init and answers a press", async ({ page }) => {
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    test.skip(!existsSync(OWNER_FIRMWARE), `the owner's firmware is not at ${OWNER_FIRMWARE}; set PEMU_E2E_OWNER_BIN to a bare merged .bin`);
    test.setTimeout(120_000);

    await openPage(page, 1440, 900, "simple");
    await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
    await expect(page.locator("[data-loader]")).toHaveAttribute("data-loader-state", "loaded", { timeout: 60_000 });

    const hint = page.locator(".firmware-card [data-elf-hint]");
    await expect(hint).toBeVisible();
    await expect(hint).toContainText("The UI tree and settle detection need the application's ELF");
    await expect(hint).toContainText("idf.py build folder");

    await runsPastBleInitAndAnswersDown(page);
    await expect(page.locator(".sim-status [data-state]")).not.toHaveAttribute("data-state", "stopped");
    for (const id of ["up", "ok", "down", "power"]) {
      await expect(page.locator(`[data-control=${id}]`)).toBeEnabled();
    }
  });
});
