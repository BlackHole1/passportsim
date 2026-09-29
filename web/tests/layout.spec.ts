// The two page modes in a browser: no horizontal scroll in either at a phone or laptop width, the
// glass on whole device pixels (at a whole factor where drawn with square pixels), and the mode
// and language switches surviving a reload. None of it needs the wasm core.

import { expect, test, type Page } from "@playwright/test";
import { openPage } from "./harness";

const VIEWPORTS = [
  { width: 400, height: 800 },
  { width: 1440, height: 900 },
] as const;

async function widths(page: Page): Promise<{ scroll: number; client: number }> {
  return page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    client: document.documentElement.clientWidth,
  }));
}

for (const mode of ["simple", "advanced"] as const) {
  for (const viewport of VIEWPORTS) {
    test(`${mode} mode at ${viewport.width}x${viewport.height} has no horizontal page scroll`, async ({ page }) => {
      await openPage(page, viewport.width, viewport.height, mode);
      await expect(page.locator(`[data-mode="${mode}"]`)).toBeVisible();
      // The narrow layout's cards and the boot's console both grow the page; give them a moment.
      await page.waitForTimeout(500);
      const { scroll, client } = await widths(page);
      expect(scroll, `scrollWidth ${scroll} against clientWidth ${client}`).toBeLessThanOrEqual(client);

      // Below two device pixels per guest pixel the glass uses square pixels only at a whole factor
      // (`layout.ts` snaps to it), since an uneven nearest-neighbour upscale that small drops strokes.
      const glass = await page.locator("canvas.glass:not(.rewind-frame)").boundingBox();
      const dpr = await page.evaluate(() => window.devicePixelRatio);
      expect(glass).not.toBeNull();
      const across = (glass?.width ?? 0) * dpr;
      expect(Math.abs(across - Math.round(across)), `the glass is ${glass?.width} CSS px wide at a device pixel ratio of ${dpr}`).toBeLessThan(0.01);
      const rendering = await page.evaluate(() => getComputedStyle(document.querySelector("canvas.glass:not(.rewind-frame)")!).imageRendering);
      const factor = across / 240;
      if (rendering === "pixelated" && factor < 2) {
        expect(factor, "a pixelated glass under 2x is a whole-pixel upscale").toBe(Math.round(factor));
      }
      // And it starts on a whole device pixel, or every guest pixel straddles two screen pixels.
      const origin = await page.evaluate(() => {
        const rect = document.querySelector("canvas.glass:not(.rewind-frame)")!.getBoundingClientRect();
        return { x: (rect.left + window.scrollX) * window.devicePixelRatio, y: (rect.top + window.scrollY) * window.devicePixelRatio };
      });
      expect(Math.abs(origin.x - Math.round(origin.x)), `the glass starts at x ${origin.x}`).toBeLessThan(0.01);
      expect(Math.abs(origin.y - Math.round(origin.y)), `the glass starts at y ${origin.y}`).toBeLessThan(0.01);
    });
  }
}

test("a first visit sees the device fitted, over 140 %, in both modes on a laptop screen", async ({ page }) => {
  await openPage(page, 1440, 900, "simple");
  await expect(page.locator(".device-column")).toHaveAttribute("data-zoom", "fit");
  const simple = Number(await page.locator(".device-column").getAttribute("data-percent"));
  await page.locator('[data-mode-switch="advanced"]').click();
  await expect(page.locator('[data-mode="advanced"]')).toBeVisible();
  const advanced = Number(await page.locator(".device-column").getAttribute("data-percent"));
  // 140 % would draw the glass at two thirds of the panel's pixels; fit on this window is near them.
  for (const percent of [simple, advanced]) {
    expect(percent).toBeGreaterThan(170);
  }
});

test("the header switches mode and language, and both survive a reload", async ({ page }) => {
  // Not `openPage`: its pinned preferences would be written again on the reload.
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/?mode=simple&lang=en");
  await expect(page.locator('[data-mode="simple"]')).toBeVisible();
  await expect(page.locator('[data-action="run"]')).toBeHidden();
  await expect(page.locator(".log-panel")).toBeVisible();

  await page.locator('[data-mode-switch="advanced"]').click();
  await expect(page.locator('[data-action="run"]')).toBeVisible();
  await expect(page.locator(".log-panel")).toHaveCount(0);

  await page.locator('[data-menu="language"]').click();
  await page.locator('[data-locale="fr"]').click();
  await expect(page.locator("html")).toHaveAttribute("lang", "fr");
  await expect(page.locator('[data-control="ok"]')).toHaveAttribute("aria-label", "Bouton OK");

  // No query this time: the choices come back from storage.
  await page.goto("/");
  await expect(page.locator('[data-mode="advanced"]')).toBeVisible();
  await expect(page.locator("html")).toHaveAttribute("lang", "fr");
});
