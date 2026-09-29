// The device buttons are physical buttons, on the owner's Passport Keys 1.0.0 bare merged `.bin`: a
// press reaches the guest when the pointer or key goes down, so the firmware's response is on the
// glass while it is held (`controls.ts`). The firmware turns its backlight off after 10 s idle,
// and the page states and shows that.
//
// The firmware lights the pressed key's row for 180 ms and then clears it whether or not the key is
// still down, so "while held" is checked before the release is sent. The image is read from
// outside the tree; without it or a wasm core each test skips.

import { expect, test, type Locator, type Page } from "@playwright/test";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { browserGaps, holdControl, openPage, showTab, virtualUs, waitForLine, waitForStatus } from "./harness";
import { decodeRgbPng } from "./png";
import { findCore } from "./preconditions";

/** Where the owner keeps the firmware, outside the tree: the variable, else their Downloads. */
const OWNER_FIRMWARE = process.env.PEMU_E2E_OWNER_BIN ?? join(homedir(), "Downloads", "passport-keys-firmware-1.0.0.bin");

const GUEST_MS = 40_000;

/** The firmware's idle time before the backlight goes off (`s_screen_off_ms`, 10 000 ms). */
const SCREEN_OFF_US = 10_000_000;

/** The share of the glass a lit key row covers in highlight yellow: about 9 %, against 0 % idle. */
const LIT_ROW = 0.04;

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

async function glass(page: Page): Promise<Buffer> {
  return page.locator("canvas.glass:not(.rewind-frame)").screenshot();
}

/** The share of a glass screenshot in the highlight's yellow, dimmed or not by the backlight. */
function yellowShare(shot: Buffer): number {
  const { rgb } = decodeRgbPng(shot);
  let yellow = 0;
  for (let at = 0; at < rgb.length; at += 3) {
    const [r, g, b] = [rgb[at]!, rgb[at + 1]!, rgb[at + 2]!];
    yellow += r > 120 && g > 100 && b < 60 && r - b > 100 ? 1 : 0;
  }
  return yellow / (rgb.length / 3);
}

function brightness(shot: Buffer): number {
  const { rgb } = decodeRgbPng(shot);
  let sum = 0;
  for (const value of rgb) {
    sum += value;
  }
  return sum / rgb.length;
}

/** Reads the glass back to back until `lit` holds or `ms` passes; the row stays lit only 180 ms. */
async function glassShows(page: Page, lit: (shot: Buffer) => boolean, ms: number): Promise<boolean> {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (lit(await glass(page))) {
      return true;
    }
  }
  return false;
}

const rowLit = (shot: Buffer) => yellowShare(shot) >= LIT_ROW;

async function inputActions(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const rows = [...document.querySelectorAll('#pane-events tbody tr[data-source="ui"]')];
    return rows.filter((row) => row.querySelectorAll("td")[2]?.textContent === "input").map((row) => row.textContent ?? "");
  });
}

/** Opens the page, drops the owner's firmware and waits until its menu is up and still. */
async function bootOwnerFirmware(page: Page, mode: "simple" | "advanced"): Promise<void> {
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  test.skip(!existsSync(OWNER_FIRMWARE), `the owner's firmware is not at ${OWNER_FIRMWARE}; set PEMU_E2E_OWNER_BIN to a bare merged .bin`);
  await openPage(page, 1440, 900, mode);
  await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
  await expect(page.locator("[data-loader]")).toHaveAttribute("data-loader-state", "loaded", { timeout: 60_000 });
  expect(await waitForStatus(page), "`status` answers once the image runs").toMatchObject({ ok: true });
  // The app's ready line is printed once its UI is drawn.
  await waitForLine(page, /pk_app: ready/, GUEST_MS);
  expect(yellowShare(await glass(page)), "no row is lit before a press").toBeLessThan(LIT_ROW);
}

async function centre(control: Locator): Promise<{ x: number; y: number }> {
  const box = await control.boundingBox();
  expect(box, "the control is on screen").not.toBeNull();
  return { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2 };
}

test.describe("the owner's firmware", () => {
  test("a held DOWN lights its row before it is let go; after release the row is clear", async ({ page }) => {
    test.setTimeout(120_000);
    await bootOwnerFirmware(page, "advanced");
    await showTab(page, "events");
    const down = page.locator('[data-control="down"]');

    // A real mouse: down, and the row lights while the button is still held.
    const at = await centre(down);
    await page.mouse.move(at.x, at.y);
    await page.mouse.down();
    const whileHeld = await glassShows(page, rowLit, 3_000);
    const heldActions = await inputActions(page);
    await page.mouse.up();
    expect(whileHeld, "the row lit while DOWN was held").toBe(true);
    expect(heldActions.some((row) => row.includes("release")), "no release had been sent while it lit").toBe(false);

    // Well after the release the firmware has cleared the row.
    await expect.poll(async () => yellowShare(await glass(page)), { timeout: 5_000 }).toBeLessThan(LIT_ROW);

    // A click within one frame still reaches the guest as a press with a hold behind it.
    await page.mouse.click(at.x, at.y);
    expect(await glassShows(page, rowLit, 3_000), "a quick click lights the row").toBe(true);
    await expect.poll(async () => yellowShare(await glass(page)), { timeout: 5_000 }).toBeLessThan(LIT_ROW);

    // The keyboard is the same button: the arrow down presses, up releases.
    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.down("ArrowDown");
    expect(await glassShows(page, rowLit, 3_000), "a held ArrowDown lights the row").toBe(true);
    await page.keyboard.up("ArrowDown");

    // The journal holds the three presses and their releases, as an agent would send them.
    await expect.poll(async () => (await inputActions(page)).length, { timeout: 5_000 }).toBe(6);
    const actions = (await inputActions(page)).map((row) => (row.includes("release") ? "release" : row.includes("press") ? "press" : row));
    expect(actions).toEqual(["press", "release", "press", "release", "press", "release"]);
  });

  test("the backlight the page states is the firmware's: off after 10 s idle, on again at a press", async ({ page }) => {
    test.setTimeout(120_000);
    await bootOwnerFirmware(page, "advanced");
    const status = page.locator(".skin-status");
    await expect(status).toContainText("backlight 80%");
    const lit = brightness(await glass(page));

    // No input since boot: the firmware's idle timer turns the backlight off at 10 s.
    await expect.poll(async () => (await virtualUs(page)) ?? 0, { timeout: GUEST_MS }).toBeGreaterThan(SCREEN_OFF_US + 1_000_000);
    await expect(status).toContainText("backlight 0%");
    const dark = brightness(await glass(page));
    expect(dark, "the glass is dark with the backlight off").toBeLessThan(lit / 8);

    // A press is activity: the firmware lights the panel again, and the page follows.
    await holdControl(page, "up", 100);
    await expect(status).toContainText("backlight 80%");
    await expect.poll(async () => brightness(await glass(page)), { timeout: 5_000 }).toBeGreaterThan(lit / 2);
  });
});
