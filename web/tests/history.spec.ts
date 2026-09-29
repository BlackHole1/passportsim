// The firmware history in a real browser (`history.ts`): a booted firmware is kept in IndexedDB with
// its build id and a picture of its settled screen, survives a reload, loads again, downloads its
// bytes, and can be deleted or cleared. The bundled demo and a refused drop are not kept, and
// blocked storage is stated rather than breaking the page. The firmware is the owner's Passport
// Keys build outside the tree (`PEMU_E2E_OWNER_BIN`, else `~/Downloads`); tests needing it skip.

import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { expect, test, type Page } from "@playwright/test";
import { readOfficialFiles } from "./demoBundle";
import { browserGaps, openPage, virtualUs, waitForLoaded, waitForStatus } from "./harness";
import { findCore } from "./preconditions";

const OWNER_FIRMWARE = process.env.PEMU_E2E_OWNER_BIN ?? join(homedir(), "Downloads", "passport-keys-firmware-1.0.0.bin");
const NAME = basename(OWNER_FIRMWARE).replace(/\.[^.]*$/, "");

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function requireFirmware(): void {
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  test.skip(!existsSync(OWNER_FIRMWARE), `the owner's firmware is not at ${OWNER_FIRMWARE}; set PEMU_E2E_OWNER_BIN to a bare merged .bin`);
}

async function openHistory(page: Page) {
  await page.locator('[data-action="history"]').first().click();
  const dialog = page.locator("[data-history]");
  await expect(dialog).toBeVisible();
  return dialog;
}

async function closeHistory(page: Page): Promise<void> {
  await page.keyboard.press("Escape");
  await expect(page.locator("[data-history]")).toHaveCount(0);
}

test("a booted firmware is kept in this browser with its build and picture, and survives a reload", async ({ page }) => {
  requireFirmware();
  test.setTimeout(180_000);
  page.on("dialog", (dialog) => void dialog.accept());
  await openPage(page, 1440, 900, "advanced");
  let dialog = await openHistory(page);
  await expect(dialog).toHaveAttribute("data-history-state", "ready");
  await expect(dialog.locator("[data-history-entry]")).toHaveCount(0);
  await closeHistory(page);

  // A refused drop is not kept.
  await page.locator('[data-loader-input="files"]').setInputFiles({ name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from("not firmware") });
  await expect(page.locator("[data-loader]")).toHaveAttribute("data-loader-state", "refused");

  await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
  await waitForLoaded(page, NAME, 90_000);
  dialog = await openHistory(page);
  const entry = dialog.locator("[data-history-entry]");
  await expect(entry).toHaveCount(1);
  await expect(entry).toContainText(NAME);
  await expect(entry).toContainText("1.0 MiB");
  // The build fields come from `status`, the picture once the boot has run three virtual seconds.
  await expect(entry).toContainText(/build [0-9a-f]{9}/, { timeout: 30_000 });
  await expect(entry).toContainText("FoloToy-AI-Passport 1.0.0");
  await expect(entry.locator("[data-history-thumbnail]")).toBeVisible({ timeout: 60_000 });
  const build = (await entry.textContent())?.match(/build ([0-9a-f]{9})/)?.[1];
  expect(await page.locator('[data-field="build"]').textContent(), "the build id is the one the header shows").toBe(build);
  await closeHistory(page);

  await page.reload();
  await expect(page.locator("#app .app")).toBeVisible();
  dialog = await openHistory(page);
  await expect(entry).toHaveCount(1);
  await expect(entry).toContainText(`build ${build}`);
  const picture = entry.locator("[data-history-thumbnail]");
  await expect(picture).toBeVisible();
  expect(await picture.evaluate((img) => (img as HTMLImageElement).naturalWidth)).toBe(120);

  const download = page.waitForEvent("download");
  await entry.locator('[data-history-action="download"]').click();
  const file = await download;
  expect(file.suggestedFilename()).toBe(basename(OWNER_FIRMWARE));
  const digest = (bytes: Buffer) => createHash("sha256").update(bytes).digest("hex");
  expect(digest(readFileSync(await file.path()))).toBe(digest(readFileSync(OWNER_FIRMWARE)));

  await entry.locator('[data-history-action="load"]').click();
  await expect(page.locator("[data-history]")).toHaveCount(0);
  await waitForLoaded(page, NAME, 90_000);
  await expect(page.locator('.log-page[data-step="history"]').first()).toBeAttached();
  dialog = await openHistory(page);
  await expect(entry, "loading it again keeps one entry").toHaveCount(1);

  // The demo is not added, however long it runs.
  if ("files" in readOfficialFiles(process.env)) {
    await closeHistory(page);
    await page.locator('[data-action="back-to-demo"]').click();
    await waitForLoaded(page, "official", 90_000);
    await expect.poll(async () => (await virtualUs(page)) ?? 0, { timeout: 30_000 }).toBeGreaterThan(3_500_000);
    dialog = await openHistory(page);
    await expect(entry).toHaveCount(1);
    await expect(entry).toContainText(NAME);
  }

  await entry.locator('[data-history-action="delete"]').click();
  await expect(entry).toHaveCount(0);
  await closeHistory(page);

  // Clear all empties it, after asking.
  await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
  await waitForLoaded(page, NAME, 90_000);
  dialog = await openHistory(page);
  await expect(entry).toHaveCount(1);
  await dialog.locator('[data-history-action="clear"]').click();
  await expect(entry).toHaveCount(0);
  await page.reload();
  dialog = await openHistory(page);
  await expect(dialog.locator("[data-history-entry]")).toHaveCount(0);
});

test("the bundled demo is not kept in the history", async ({ page }) => {
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  const demo = readOfficialFiles(process.env);
  test.skip(!("files" in demo), "files" in demo ? "" : "mismatch" in demo ? demo.mismatch : demo.skip);
  await openPage(page, 1440, 900, "simple");
  expect((await waitForStatus(page, 60_000)).ok, "the demo is up").toBe(true);
  // Past the virtual instant at which a kept firmware's picture is taken.
  await expect.poll(async () => (await virtualUs(page)) ?? 0, { timeout: 30_000 }).toBeGreaterThan(3_500_000);
  const dialog = await openHistory(page);
  await expect(dialog).toHaveAttribute("data-history-state", "ready");
  await expect(dialog.locator("[data-history-entry]")).toHaveCount(0);
});

test("with storage blocked the history says it is unavailable and the page still runs", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(window, "indexedDB", {
      configurable: true,
      get() {
        throw new DOMException("The operation is insecure.", "SecurityError");
      },
    });
  });
  await openPage(page, 1440, 900, "simple");
  const dialog = await openHistory(page);
  await expect(dialog).toHaveAttribute("data-history-state", "unavailable");
  await expect(dialog.locator("[data-history-unavailable]")).toContainText("The history is unavailable in this browser");
  await expect(dialog.locator("[data-history-unavailable]")).toContainText("insecure");
  await closeHistory(page);
  await expect(page.locator("[data-loader]")).toBeVisible();
  await expect(page.locator(".log-panel")).toBeVisible();
});
