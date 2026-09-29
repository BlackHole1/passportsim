// The device view in a real browser: the photo filling its room and never under 140 % of nominal
// size, the glass over the photo's screen, the side buttons as controls, a remembered zoom that
// never scrolls the page sideways, and USB states named for what a person sees. Sizes are checked
// against the engine's own millimetre, measured on a probe element.

import { expect, test, type Page } from "@playwright/test";
import { browserGaps, openPage } from "./harness";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

async function engineMm(page: Page, mm: number): Promise<number> {
  return page.evaluate((mm) => {
    const probe = document.createElement("div");
    probe.style.cssText = `position:absolute;visibility:hidden;width:${mm}mm`;
    document.body.append(probe);
    const width = probe.getBoundingClientRect().width;
    probe.remove();
    return width;
  }, mm);
}

async function widths(page: Page): Promise<{ scroll: number; client: number }> {
  return page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
}

test.describe("the device view", () => {
  test("draws the device's photo filling its room, glass near the panel's pixels, with its side buttons as the controls", async ({ page }) => {
    await openPage(page, 1440, 900, "simple");
    const body = page.locator("[data-device-body]");
    await expect(body).toBeVisible();
    const box = await body.boundingBox();
    // Fit is at least 140 % of 60 mm (less the glass's snapping), and on a laptop window the glass is
    // near the panel's own 240 pixels wide.
    const floor = (await engineMm(page, 60)) * 1.4;
    expect(box?.width ?? 0).toBeGreaterThanOrEqual(floor * 0.91);
    expect(box?.height ?? 0).toBeLessThanOrEqual(900);
    const glass = await page.locator("[data-device-body] .skin-screen canvas.glass:not(.rewind-frame)").boundingBox();
    expect(glass?.width ?? 0).toBeGreaterThanOrEqual(216);
    expect((box?.height ?? 0) / (box?.width ?? 1)).toBeCloseTo(95 / 60, 2);
    await expect(page.locator("[data-device-body] .skin-screen canvas.glass:not(.rewind-frame)")).toHaveCount(1);
    await expect(page.locator("canvas.glass:not(.rewind-frame)")).toHaveCount(1);
    // The photo is a data URL from the bundle: no file a package could miss, no cross-origin fetch.
    const photo = await page.locator("[data-device-body] img.device-photo").evaluate((img: HTMLImageElement) => ({
      src: img.src.slice(0, 15),
      complete: img.complete,
      width: img.naturalWidth,
      height: img.naturalHeight,
    }));
    expect(photo).toEqual({ src: "data:image/webp", complete: true, width: 558, height: 883 });
    // The glass lies over the photo's screen, which starts 103 of the photo's 558 pixels across.
    const screen = (await page.locator("[data-device-body] .skin-screen").boundingBox())!;
    const k = (box?.width ?? 0) / 558;
    expect(Math.abs(screen.x - (box?.x ?? 0) - 103 * k)).toBeLessThanOrEqual(1);
    expect(Math.abs(screen.width - 284 * k)).toBeLessThanOrEqual(1);
    // UP, OK and DOWN on the right edge top to bottom, POWER on the left, each reaching out to its name.
    const edge = async (id: string) => (await page.locator(`[data-device-body] [data-control=${id}]`).boundingBox())!;
    const [up, ok, down, power] = [await edge("up"), await edge("ok"), await edge("down"), await edge("power")];
    const left = box?.x ?? 0;
    const right = left + (box?.width ?? 0);
    for (const control of [up, ok, down]) {
      expect(control.x).toBeLessThan(right);
      expect(control.x + control.width).toBeGreaterThan(right);
    }
    expect(up.y).toBeLessThan(ok.y);
    expect(ok.y).toBeLessThan(down.y);
    expect(power.x).toBeLessThan(left);
    expect(power.x + power.width).toBeGreaterThan(left);
    // The badge stays on screen, so a screenshot of the device is never taken for a photo.
    await expect(page.locator(".emulator-badge")).toBeVisible();
  });

  test("a zoom is remembered, and one too wide for the viewport shrinks to fit it", async ({ page }) => {
    await openPage(page, 1440, 900, "simple");
    const body = page.locator("[data-device-body]");
    await page.locator('[data-zoom-choice="140"]').click();
    await expect(page.locator('[data-zoom-choice="140"]')).toHaveAttribute("aria-pressed", "true");
    const at140 = (await body.boundingBox())?.width ?? 0;
    await page.locator('[data-zoom-choice="180"]').click();
    await expect(page.locator('[data-zoom-choice="180"]')).toHaveAttribute("aria-pressed", "true");
    const at180 = (await body.boundingBox())?.width ?? 0;
    expect(at180 / at140).toBeGreaterThan(1.15);
    await expect(page.locator("[data-device-body] .skin-screen canvas.glass:not(.rewind-frame)")).toHaveCount(1);

    // No query: the zoom comes back from storage, at a phone's width, where it cannot fit.
    await page.setViewportSize({ width: 400, height: 800 });
    await page.goto("/");
    await expect(page.locator('[data-zoom-choice="180"]')).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator("[data-zoom-drawn]")).toBeVisible();
    await page.waitForTimeout(300);
    const { scroll, client } = await widths(page);
    expect(scroll, `scrollWidth ${scroll} against clientWidth ${client}`).toBeLessThanOrEqual(client);
  });

  test("the USB states say what they mean, and U1 says why it cannot be chosen", async ({ page }) => {
    await openPage(page, 1440, 900, "advanced");
    const strip = page.locator(".usb-connector");
    await expect(strip.locator('[data-usb="U0"]')).toContainText("Unplugged");
    await expect(strip.locator('[data-usb="U3"]')).toContainText("Port open");
    await expect(strip.locator('[data-usb="U3"]')).toHaveAttribute("aria-pressed", "true");
    const u1 = strip.locator('[data-usb="U1"]');
    await expect(u1).toHaveAttribute("aria-disabled", "true");
    await expect(u1).toHaveAttribute("title", /POWER/);
    await expect(page.locator(".usb-legend")).toBeVisible();
  });
});
