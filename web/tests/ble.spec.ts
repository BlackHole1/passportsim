// The BLE card in a real browser on two firmwares, the device's Bluetooth page opened first.
//
// The demo (`official`) advertises from its BLE page as non-connectable (`ADV_SCAN_IND`,
// `demo_ble.c`), and its controller is not started before that page. The card must say both in
// plain words, and "Discover" must say why it cannot.
//
// Passport Keys 1.0.0 is a bare merged `.bin` with no ELF, read from outside the tree: its radio
// binds from the image and it advertises `ADV_IND`. The card scans, connects, discovers the vendor
// service, subscribes to events and gets the firmware's answer to a command line.
//
// Both run on every engine project. Skips name the reason: no wasm core, no demo bundle, or no
// owner firmware.

import { expect, test, type Locator, type Page } from "@playwright/test";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { readOfficialFiles } from "./demoBundle";
import { browserGaps, consoleText, holdControl, openCard, openPage, virtualUs, waitForLine } from "./harness";
import { findCore } from "./preconditions";

/** Where the owner keeps the firmware, outside the tree: the variable, else their Downloads. */
const OWNER_FIRMWARE = process.env.PEMU_E2E_OWNER_BIN ?? join(homedir(), "Downloads", "passport-keys-firmware-1.0.0.bin");

const GUEST_MS = 40_000;

/** Guest time after a click's release: `iot_button` reports a click about 185 ms after it. */
const CLICK_GAP_US = 300_000;

/** The Passport Keys vendor service and its two characteristics (`pk_ble.c`). */
const VENDOR = "12D4FA08-7418-48FA-A95A-B43A2E669E55";
const EVENTS = "12D4FA09-7418-48FA-A95A-B43A2E669E55";
const COMMANDS = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

async function click(page: Page, control: string): Promise<void> {
  await holdControl(page, control, 80);
  const released = (await virtualUs(page)) ?? 0;
  await expect.poll(async () => (await virtualUs(page)) ?? 0, { timeout: GUEST_MS }).toBeGreaterThanOrEqual(released + CLICK_GAP_US);
}

/** The demo's BLE page: DOWN five times from Display, then OK (`main.c` `DEMOS[]`). */
async function openDemoBlePage(page: Page): Promise<void> {
  for (let count = 0; count < 5; count += 1) {
    await click(page, "down");
  }
  await click(page, "ok");
}

function stateLine(card: Locator): Locator {
  return card.locator("[data-ble-state]");
}

test.describe("the BLE card", () => {
  test("on the demo's BLE page the scan finds FoloPassport, and the card says it accepts no connection", async ({ page }) => {
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    const demo = readOfficialFiles();
    if ("mismatch" in demo) {
      throw new Error(demo.mismatch);
    }
    test.skip("skip" in demo, "skip" in demo ? demo.skip : "");
    test.setTimeout(180_000);

    await openPage(page);
    await waitForLine(page, /main: 就绪/, GUEST_MS);
    const ble = await openCard(page, "ble");
    // Bound, and the firmware has not started its controller: waiting, never "no module".
    await expect(stateLine(ble)).toHaveAttribute("data-ble-state", "not_started", { timeout: GUEST_MS });
    await expect(stateLine(ble)).toContainText("Waiting for the firmware to start Bluetooth");
    await expect(ble.locator("[data-radio-unbound]")).toHaveCount(0);

    await openDemoBlePage(page);
    await expect(stateLine(ble)).toHaveAttribute("data-ble-state", "advertising-non-connectable", { timeout: GUEST_MS });
    await expect(stateLine(ble)).toContainText("Advertising FoloPassport as ADV_SCAN_IND, not connectable");

    await ble.getByRole("button", { name: "Scan", exact: true }).click();
    await expect(ble.locator("[data-ble-scan]")).toHaveAttribute("data-ble-scan", "heard", { timeout: GUEST_MS });
    const rows = ble.locator("ul.peer-list li");
    await expect(rows).toHaveCount(1);
    await expect(rows).toContainText("FoloPassport");
    await expect(rows).toContainText("ADV_SCAN_IND");
    await expect(rows).toContainText("not connectable");
    await expect(rows.getByRole("button", { name: "connect" }), "no connect is offered to a non-connectable advertiser").toHaveCount(0);

    await ble.getByRole("button", { name: "Discover", exact: true }).click();
    const why = ble.locator("[data-ble-why]");
    await expect(why).toHaveAttribute("data-ble-why", "non_connectable", { timeout: GUEST_MS });
    await expect(why).toContainText("advertises ADV_SCAN_IND: the firmware does not accept connections");
    await expect(ble.locator(".card-error"), "the reason is the card's, not an error string").toBeHidden();
  });

  test("a scan before the demo starts Bluetooth says so, and a scan as soon as its BLE page opens says what the air carried", async ({ page }) => {
    // The other click order: the card first, scanning the moment the device's page opens.
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    const demo = readOfficialFiles();
    if ("mismatch" in demo) {
      throw new Error(demo.mismatch);
    }
    test.skip("skip" in demo, "skip" in demo ? demo.skip : "");
    test.setTimeout(180_000);

    await openPage(page);
    await waitForLine(page, /main: 就绪/, GUEST_MS);
    const ble = await openCard(page, "ble");
    const scan = ble.getByRole("button", { name: "Scan", exact: true });
    await scan.click();
    await expect(stateLine(ble)).toHaveAttribute("data-ble-state", "not_started", { timeout: GUEST_MS });
    await expect(ble.locator("ul.peer-list li")).toHaveCount(0);
    await expect(ble.locator(".card-error"), "a controller that is not up is a state, not an error").toBeHidden();

    await openDemoBlePage(page);
    await scan.click();
    // The controller may still be coming up; either way the answer names it.
    await expect(ble.locator("[data-ble-scan], [data-ble-state=not_started]").first()).toBeVisible({ timeout: GUEST_MS });
    await expect(stateLine(ble)).toHaveAttribute("data-ble-state", "advertising-non-connectable", { timeout: GUEST_MS });
    await scan.click();
    await expect(ble.locator("[data-ble-scan]")).toHaveAttribute("data-ble-scan", "heard", { timeout: GUEST_MS });
    await expect(ble.locator("ul.peer-list li")).toContainText("FoloPassport");
  });

  test("with the owner's Passport Keys .bin the card scans, connects, discovers, subscribes and gets an answer", async ({ page }) => {
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    test.skip(!existsSync(OWNER_FIRMWARE), `the owner's firmware is not at ${OWNER_FIRMWARE}; set PEMU_E2E_OWNER_BIN to a bare merged .bin`);
    test.setTimeout(180_000);

    await openPage(page);
    await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
    await expect(page.locator("[data-loader]")).toHaveAttribute("data-loader-state", "loaded", { timeout: 60_000 });
    await waitForLine(page, /pk_app: ready/, GUEST_MS);

    const ble = await openCard(page, "ble");
    await expect(stateLine(ble)).toHaveAttribute("data-ble-state", "advertising-connectable", { timeout: GUEST_MS });
    await ble.getByRole("button", { name: "Scan", exact: true }).click();
    await expect(ble.locator("[data-ble-scan]")).toHaveAttribute("data-ble-scan", "heard", { timeout: GUEST_MS });
    const row = ble.locator('ul.peer-list li[data-connectable="true"]');
    await expect(row).toHaveCount(1);
    await expect(row).toContainText("ADV_IND");
    await row.getByRole("button", { name: "connect" }).click();
    await expect(stateLine(ble)).toHaveAttribute("data-ble-state", "connected", { timeout: GUEST_MS });

    await ble.getByRole("button", { name: "Discover", exact: true }).click();
    await expect(ble.locator("ul.gatt-tree")).toContainText(VENDOR, { timeout: GUEST_MS });
    const chooser = ble.getByLabel("Characteristic");
    await chooser.selectOption(EVENTS);
    await ble.getByRole("button", { name: /subscribe/i }).click();
    await expect(ble.locator('[data-ble="link"]')).toContainText(`subscribed ${EVENTS}`, { timeout: GUEST_MS });

    // `pk_protocol.c` frames on the newline, so the command line carries one.
    await chooser.selectOption(COMMANDS);
    await ble.getByLabel(/value/i).fill('{"cmd":"hello"}\n');
    await ble.getByRole("button", { name: /write/i }).click();
    await expect(ble.locator("ul.notification-list")).toContainText('"t":"hello"', { timeout: GUEST_MS });
    await expect(ble.locator(".card-error")).toBeHidden();
    expect(await consoleText(page), "no host reset during the exchange").not.toMatch(/host reset/i);
  });
});
