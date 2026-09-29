// The advanced workbench and the logs in a real browser: no pane draws a scrollbar it cannot use,
// the cards pack into even columns, logs follow their newest line only while the reader lets them,
// a log's two blocks say what they are, and Save state and Screenshot report what they did. The
// first three tests need no wasm core; the rest boot the bundled demo and skip without it.

import { readFileSync } from "node:fs";
import { expect, test, type Page } from "@playwright/test";
import { readOfficialFiles } from "./demoBundle";
import { browserGaps, openCard, openPage, showTab, waitForLine, waitForStatus } from "./harness";
import { decodeRgbPng } from "./png";
import { findCore } from "./preconditions";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function requireDemo(): void {
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  const demo = readOfficialFiles(process.env);
  test.skip(!("files" in demo), "files" in demo ? "" : "mismatch" in demo ? demo.mismatch : demo.skip);
}

/**
 * Every element of `root` that scrolls vertically without being one of the page's scroll boxes
 * (`data-scroller`). Firefox draws a scrollbar for each, even for one pixel.
 */
async function strayScrollers(page: Page, root: string): Promise<string[]> {
  return page.evaluate((selector) => {
    const found: string[] = [];
    for (const node of document.querySelectorAll(`${selector}, ${selector} *`)) {
      const element = node as HTMLElement;
      if (element.offsetParent === null || element.hasAttribute("data-scroller") || element.tagName === "TEXTAREA") {
        continue;
      }
      const style = getComputedStyle(element);
      const scrolls = style.overflowY === "auto" || style.overflowY === "scroll";
      if (scrolls && element.scrollHeight > element.clientHeight) {
        found.push(`${element.tagName.toLowerCase()}.${String(element.className).split(" ")[0]}: ${element.scrollHeight} > ${element.clientHeight}`);
      }
    }
    return found;
  }, root);
}

test("no tab of the console area draws a vertical scrollbar outside its own scroll boxes", async ({ page }) => {
  for (const [width, height] of [
    [1440, 900],
    [400, 800],
  ] as const) {
    await openPage(page, width, height, "advanced");
    for (const tab of ["console", "ui-tree", "events", "inspect", "fidelity", "perf"]) {
      await showTab(page, tab);
      expect(await strayScrollers(page, ".panels"), `${tab} at ${width}x${height}`).toEqual([]);
    }
  }
});

/** Each card's box, grouped into the columns they sit in, top to bottom. */
async function cardColumns(page: Page): Promise<{ id: string; top: number; bottom: number }[][]> {
  const boxes = await page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>(".cards > [data-card]")].map((card) => {
      const rect = card.getBoundingClientRect();
      return { id: card.dataset.card ?? "", left: Math.round(rect.left), top: rect.top + window.scrollY, bottom: rect.bottom + window.scrollY };
    }),
  );
  const columns = new Map<number, { id: string; top: number; bottom: number }[]>();
  for (const box of boxes) {
    columns.set(box.left, [...(columns.get(box.left) ?? []), box]);
  }
  return [...columns.entries()].sort((a, b) => a[0] - b[0]).map(([, cards]) => cards.sort((a, b) => a.top - b.top));
}

for (const [width, count] of [
  [1280, 3],
  [1440, 3],
  [1920, 3],
  [800, 2],
  [400, 1],
] as const) {
  test(`at ${width} px the environment cards pack into ${count} column(s) with even gaps`, async ({ page }) => {
    await openPage(page, width, 900, "advanced");
    // Every card open, so the heights differ as much as they can.
    for (const id of ["battery", "usb", "audio", "nfc", "wifi", "ble", "snapshots"]) {
      await openCard(page, id);
    }
    await page.waitForTimeout(300);
    const columns = await cardColumns(page);
    expect(columns.length, JSON.stringify(columns)).toBe(count);
    const firstTops = columns.map((column) => column[0]?.top ?? 0);
    for (const top of firstTops) {
      expect(Math.abs(top - (firstTops[0] ?? 0)), `the columns start level: ${firstTops.join(", ")}`).toBeLessThanOrEqual(1);
    }
    for (const column of columns) {
      for (let i = 1; i < column.length; i += 1) {
        const gap = (column[i]?.top ?? 0) - (column[i - 1]?.bottom ?? 0);
        // The grid's rows are 2 px, so a gap is the 16 px gap plus under one row.
        expect(gap, `${column[i - 1]?.id} to ${column[i]?.id}`).toBeGreaterThanOrEqual(15.5);
        expect(gap, `${column[i - 1]?.id} to ${column[i]?.id}`).toBeLessThan(18.5);
      }
    }
    // The reading order is the cards' own: battery first, snapshots last.
    const order = await page.locator(".cards > [data-card]").evaluateAll((cards) => cards.map((card) => (card as HTMLElement).dataset.card));
    expect(order).toEqual(["battery", "usb", "audio", "nfc", "wifi", "ble", "snapshots"]);
  });
}

test("the follow-latest choice is on by default, in both logs, and is remembered", async ({ page }) => {
  await openPage(page, 1440, 900, "advanced");
  const box = page.locator("#console-follow");
  await expect(box).toBeChecked();
  await box.uncheck();
  await page.reload();
  await expect(page.locator("#console-follow")).not.toBeChecked();
  await page.goto("/?mode=simple&lang=en");
  await expect(page.locator("#log-follow")).not.toBeChecked();
  await page.locator("#log-follow").check();
  await page.goto("/?mode=advanced&lang=en");
  await expect(page.locator("#console-follow")).toBeChecked();
});

const terminal = (page: Page) => page.locator("#pane-console .terminal");

async function scrollState(page: Page): Promise<{ top: number; fromBottom: number; lines: number }> {
  return terminal(page).evaluate((node) => ({
    top: node.scrollTop,
    fromBottom: node.scrollHeight - node.scrollTop - node.clientHeight,
    lines: node.querySelectorAll(".terminal-line").length,
  }));
}

/** One key press as a person makes it, with the pause the demo's button debounce needs after it. */
async function press(page: Page, key: string): Promise<void> {
  await page.keyboard.down(key);
  await page.waitForTimeout(150);
  await page.keyboard.up(key);
  await page.waitForTimeout(600);
}

/** Opens the demo's Wi-Fi page, which prints a burst of lines: DOWN four times, then OK. */
async function burst(page: Page): Promise<void> {
  // The keys go to the page, not to a field that would take them as text.
  await page.locator("#pane-console .terminal").focus();
  for (let i = 0; i < 4; i += 1) {
    await press(page, "ArrowDown");
  }
  await press(page, "Enter");
  await waitForLine(page, /wifi_init/, 30_000);
}

async function bootedDemo(page: Page, width = 1440, height = 700): Promise<void> {
  await openPage(page, width, height, "advanced");
  const status = await waitForStatus(page, 60_000);
  expect(status.ok, "the demo answers `status`").toBe(true);
  await waitForLine(page, /Returned from app_main/, 30_000);
  // The boot's lines overflow the console at this height, so there is somewhere to scroll.
  await expect.poll(async () => (await scrollState(page)).top).toBeGreaterThan(0);
}

test("with follow latest on, new output scrolls the console to its newest line", async ({ page }) => {
  requireDemo();
  test.setTimeout(120_000);
  await bootedDemo(page);
  const before = await scrollState(page);
  expect(before.fromBottom).toBeLessThan(24);
  await burst(page);
  await expect.poll(async () => (await scrollState(page)).lines).toBeGreaterThan(before.lines);
  await expect.poll(async () => (await scrollState(page)).fromBottom).toBeLessThan(24);
  await expect(page.locator("#pane-console .terminal-jump")).toHaveCount(0);
});

test("scrolling up pauses following and says so; new output leaves the reader where they are", async ({ page }) => {
  requireDemo();
  test.setTimeout(120_000);
  await bootedDemo(page);
  await terminal(page).evaluate((node) => {
    node.scrollTop = 0;
  });
  const jump = page.locator("#pane-console .terminal-jump");
  await expect(jump).toHaveAttribute("data-follow-paused", "true");
  await expect(jump).toContainText("Paused while you read");
  // Scrolling is not a vote to turn following off.
  await expect(page.locator("#console-follow")).toBeChecked();
  const lines = (await scrollState(page)).lines;
  await burst(page);
  await expect.poll(async () => (await scrollState(page)).lines).toBeGreaterThan(lines);
  expect((await scrollState(page)).top, "the new lines did not pull the reader down").toBeLessThan(2);
  await jump.click();
  await expect.poll(async () => (await scrollState(page)).fromBottom).toBeLessThan(24);
  await expect(jump).toHaveCount(0);
});

test("with follow latest off, the console never scrolls itself", async ({ page }) => {
  requireDemo();
  test.setTimeout(120_000);
  await bootedDemo(page);
  await page.locator("#console-follow").uncheck();
  const middle = await terminal(page).evaluate((node) => {
    node.scrollTop = Math.floor((node.scrollHeight - node.clientHeight) / 2);
    return node.scrollTop;
  });
  const lines = (await scrollState(page)).lines;
  await burst(page);
  await expect.poll(async () => (await scrollState(page)).lines).toBeGreaterThan(lines);
  expect(Math.abs((await scrollState(page)).top - middle), "the view stayed where the reader left it").toBeLessThanOrEqual(2);
  await expect(page.locator("#pane-console .terminal-jump")).toHaveAttribute("data-follow-paused", "false");
});

test("the page's own steps and the device's output are separate blocks, each captioned inside itself", async ({ page }) => {
  for (const mode of ["simple", "advanced"] as const) {
    await openPage(page, 1440, 900, mode);
    const root = mode === "simple" ? page.locator(".log-panel") : page.locator("#pane-console");
    const steps = root.locator(".log-steps");
    const device = root.locator(".log-device");
    await expect(steps.locator(".block-caption")).toContainText("Page steps");
    await expect(device.locator(".block-caption")).toContainText("Device serial output");
    await expect(steps.locator(".log-page").first()).toBeVisible();
    await expect(device.locator(".log-page")).toHaveCount(0);
    await expect(steps.locator(".terminal-line")).toHaveCount(0);
  }
});

test("Save state says what it saved and where, and Show opens the Snapshots card", async ({ page }) => {
  requireDemo();
  test.setTimeout(120_000);
  await openPage(page, 1440, 900, "advanced");
  expect((await waitForStatus(page, 60_000)).ok).toBe(true);
  await page.locator('[data-action="snap"]').click();
  const notice = page.locator('.toolbar-notice[data-notice="state"]');
  await expect(notice).toContainText(/State saved as rewind-\d+-\d+\.\d{3}s at \d+\.\d{3} s, in the Snapshots card/);
  await expect(notice).toContainText("It is not an image");
  const snapshots = page.locator('section.card[data-card="snapshots"]');
  if (!(await snapshots.locator("#card-snapshots").isVisible())) {
    await snapshots.locator(".card-toggle").click();
  }
  await snapshots.locator(".card-toggle").click();
  await expect(snapshots.locator("#card-snapshots")).toBeHidden();
  await page.locator('[data-action="show-snapshots"]').click();
  await expect(snapshots.locator("#card-snapshots")).toBeVisible();
  await expect(snapshots).toBeInViewport();
  await expect(snapshots.locator("[data-saved]")).toContainText(/last saved: rewind-\d+/);
});

test("Screenshot downloads the screen as a PNG named with the image and the virtual time", async ({ page }) => {
  requireDemo();
  test.setTimeout(120_000);
  for (const mode of ["advanced", "simple"] as const) {
    await openPage(page, 1440, 900, mode);
    expect((await waitForStatus(page, 60_000)).ok).toBe(true);
    await waitForLine(page, /Returned from app_main/, 30_000).catch(() => undefined);
    await page.waitForTimeout(1_000);
    const download = page.waitForEvent("download");
    await page.locator('[data-action="screenshot"]').first().click();
    const file = await download;
    expect(file.suggestedFilename()).toMatch(/^official-vt\d+\.\d{3}s\.png$/);
    const path = await file.path();
    const { width, height, rgb } = decodeRgbPng(readFileSync(path));
    expect([width, height]).toEqual([240, 320]);
    expect(rgb.some((value, at) => value !== rgb[at % 3]), "the screenshot is not one flat colour").toBe(true);
    await expect(page.locator('[data-notice="shot"]').first()).toContainText(file.suggestedFilename());
  }
});
