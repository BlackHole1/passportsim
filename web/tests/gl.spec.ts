// The panel renderers in a real browser: a synthetic RGB565 frame drawn through WebGL1 and read
// back with `readPixels`, the same after a forced context loss and restore, and through the 2D
// fallback read with `getImageData`. Every pixel is compared with `toRgba` at the integer scale of
// `viewport.ts`, with black around the panel.
//
// Skips, each named: a browser not installed; no WebGL1 on a fresh `OffscreenCanvas`
// (`webgl-unavailable`); no `WEBGL_lose_context` (`no-lose-context-extension`). A context that draws
// wrong pixels fails.

import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { browserGaps } from "./harness";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

let probe: string | null = null;

function probeSource(): string {
  probe ??= execFileSync("bun", ["build", join(WEB, "tests", "glProbe.ts"), "--target", "browser", "--format", "iife"], {
    cwd: WEB,
    encoding: "utf8",
  });
  return probe;
}

async function blankWithProbe(page: Page): Promise<void> {
  await page.setContent("<!doctype html><title>gl probe</title><body></body>");
  await page.addScriptTag({ content: probeSource() });
}

interface Readback {
  readonly skip?: string;
  readonly backend?: string;
  readonly reason?: string | null;
  readonly scale?: number;
  readonly mismatches?: number;
  readonly first?: string | null;
  readonly sky?: number[];
}

/** Runs in the page, serialised by Playwright, so it declares everything it uses. */
const pageHelpers = `
  globalThis.syntheticFrame = (width, height) => {
    const pixels = new Uint16Array(width * height);
    for (let y = 0; y < height; y += 1)
      for (let x = 0; x < width; x += 1)
        pixels[y * width + x] = ((x % 32) << 11) | (((y * 7) % 64) << 5) | ((x + y) % 32);
    return pixels;
  };
  globalThis.compare = (rgba, bottomUp, canvasWidth, canvasHeight, pixels, panel, view) => {
    const expected = new Uint8ClampedArray(pixels.length * 4);
    globalThis.pemuGl.toRgba(pixels, panel, expected);
    let mismatches = 0;
    let first = null;
    for (let y = 0; y < canvasHeight; y += 1) {
      const row = bottomUp ? canvasHeight - 1 - y : y;
      for (let x = 0; x < canvasWidth; x += 1) {
        const px = Math.floor((x - view.x) / view.scale);
        const py = Math.floor((y - view.y) / view.scale);
        const inside = x >= view.x && y >= view.y && px < 240 && py < 320;
        const at = (row * canvasWidth + x) * 4;
        const want = inside ? (py * 240 + px) * 4 : -1;
        for (let c = 0; c < 4; c += 1) {
          const expect = want < 0 ? (c === 3 ? 255 : 0) : expected[want + c];
          if (rgba[at + c] !== expect) {
            mismatches += 1;
            if (first === null) first = "(" + x + ", " + y + ") channel " + c + ": got " + rgba[at + c] + ", want " + expect;
            break;
          }
        }
      }
    }
    return { mismatches, first };
  };
`;

const PANELS = [
  { backlight: 1024, backlightScale: 1024, glassComplement: false, powered: true, sleeping: false, displayOn: true },
  { backlight: 1024, backlightScale: 1024, glassComplement: true, powered: true, sleeping: false, displayOn: true },
  { backlight: 300, backlightScale: 1024, glassComplement: true, powered: true, sleeping: false, displayOn: true },
];

test.describe("panel renderers in the browser", () => {
  test("WebGL1 draws a synthetic RGB565 frame exactly, at an integer scale", async ({ page }) => {
    await blankWithProbe(page);
    await page.addScriptTag({ content: pageHelpers });
    const results = (await page.evaluate((panels) => {
      const g = globalThis as unknown as Record<string, (...args: unknown[]) => unknown> & {
        pemuGl: { openDisplay: (...args: unknown[]) => { sink: { draw: Function; upload: Function } | null; state: { backend: string; reason: string | null } } };
      };
      if (!new OffscreenCanvas(1, 1).getContext("webgl")) {
        return [{ skip: "webgl-unavailable: a fresh OffscreenCanvas gave no WebGL1 context" }];
      }
      const canvas = new OffscreenCanvas(500, 700);
      const display = g.pemuGl.openDisplay(canvas, 240, 320);
      if (!display.sink) {
        return [{ backend: display.state.backend, reason: display.state.reason }];
      }
      const pixels = g.syntheticFrame!(240, 320) as Uint16Array;
      const gl = canvas.getContext("webgl") as WebGLRenderingContext;
      const rgba = new Uint8Array(500 * 700 * 4);
      const results: Readback[] = panels.map((panel) => {
        display.sink!.upload(pixels, 0, 319);
        const view = display.sink!.draw(panel, 500, 700) as { scale: number };
        gl.readPixels(0, 0, 500, 700, gl.RGBA, gl.UNSIGNED_BYTE, rgba);
        const outcome = g.compare!(rgba, true, 500, 700, pixels, panel, view) as { mismatches: number; first: string | null };
        return { backend: display.state.backend, scale: view.scale, ...outcome };
      });
      // The official menu's sky under INVON with `invon_shows_ram`: no glass complement.
      display.sink.upload(new Uint16Array(240 * 320).fill(0x145d), 0, 319);
      display.sink.draw(panels[0], 500, 700);
      const sky = new Uint8Array(4);
      gl.readPixels(250, 350, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, sky);
      results[0] = { ...results[0], sky: Array.from(sky) };
      return results;
    }, PANELS)) as Readback[];
    test.skip(results[0]?.skip !== undefined, results[0]?.skip ?? "");
    expect(results[0]?.sky, "0x145D with INVON and invon_shows_ram").toEqual([16, 138, 239, 255]);
    for (const result of results) {
      expect(result.backend, result.reason ?? "").toBe("webgl1");
      expect(result.scale).toBe(2);
      expect(result.first ?? null).toBeNull();
      expect(result.mismatches).toBe(0);
    }
  });

  test("WebGL1 redraws the last frame after a context loss and restore", async ({ page }) => {
    await blankWithProbe(page);
    await page.addScriptTag({ content: pageHelpers });
    const result = (await page.evaluate(async (panel) => {
      const g = globalThis as unknown as Record<string, (...args: unknown[]) => unknown> & {
        pemuGl: { openDisplay: (...args: unknown[]) => { sink: { draw: Function; upload: Function } | null; state: { backend: string } } };
      };
      if (!new OffscreenCanvas(1, 1).getContext("webgl")) {
        return { skip: "webgl-unavailable: a fresh OffscreenCanvas gave no WebGL1 context" };
      }
      const canvas = new OffscreenCanvas(240, 320);
      const states: unknown[] = [];
      const display = g.pemuGl.openDisplay(canvas, 240, 320, (state: unknown) => states.push(state));
      const gl = canvas.getContext("webgl") as WebGLRenderingContext;
      const lose = gl.getExtension("WEBGL_lose_context");
      if (!lose || !display.sink) {
        return { skip: "no-lose-context-extension: WEBGL_lose_context is not exposed", backend: display.state.backend };
      }
      const pixels = g.syntheticFrame!(240, 320) as Uint16Array;
      display.sink.upload(pixels, 0, 319);
      const view = display.sink.draw(panel, 240, 320);
      const readback = new Promise<{ mismatches: number; first: string | null }>((resolve) => {
        // Registered after the renderer's own listener, so it runs after the redraw, in the same task.
        canvas.addEventListener("webglcontextrestored", () => {
          const rgba = new Uint8Array(240 * 320 * 4);
          gl.readPixels(0, 0, 240, 320, gl.RGBA, gl.UNSIGNED_BYTE, rgba);
          resolve(g.compare!(rgba, true, 240, 320, pixels, panel, view) as { mismatches: number; first: string | null });
        });
      });
      await new Promise<void>((resolve) => {
        canvas.addEventListener("webglcontextlost", () => resolve());
        lose.loseContext();
      });
      // Out of the loss event's dispatch first: Chromium ignores a restore requested from inside it.
      await new Promise((resolve) => setTimeout(resolve, 0));
      lose.restoreContext();
      const outcome = await Promise.race([
        readback,
        new Promise<null>((resolve) => setTimeout(() => resolve(null), 10_000)),
      ]);
      return { states, ...(outcome ?? { mismatches: -1, first: "no webglcontextrestored within 10 s" }) };
    }, PANELS[1])) as Readback & { states?: unknown[] };
    test.skip(result.skip !== undefined, result.skip ?? "");
    expect(result.states).toEqual([
      { backend: "webgl1", reason: null, contextLost: true },
      { backend: "webgl1", reason: null, contextLost: false },
    ]);
    expect(result.first ?? null).toBeNull();
    expect(result.mismatches).toBe(0);
  });

  test("the 2D fallback draws the same frame exactly, and says it is the fallback", async ({ page }) => {
    await blankWithProbe(page);
    await page.addScriptTag({ content: pageHelpers });
    const results = (await page.evaluate((panels) => {
      const g = globalThis as unknown as Record<string, (...args: unknown[]) => unknown> & {
        pemuGl: { openDisplay: (...args: unknown[]) => { sink: { draw: Function; upload: Function } | null; state: { backend: string; reason: string | null } } };
      };
      const real = new OffscreenCanvas(500, 700);
      // A canvas that refuses WebGL, as a blocklisted GPU does, and hands out its real 2D context.
      const noWebgl = {
        addEventListener: () => {},
        getContext: (kind: string, attributes: unknown) =>
          kind === "webgl" ? null : real.getContext("2d", attributes as CanvasRenderingContext2DSettings),
      };
      const display = g.pemuGl.openDisplay(noWebgl, 240, 320);
      const ctx = real.getContext("2d") as OffscreenCanvasRenderingContext2D;
      const pixels = g.syntheticFrame!(240, 320) as Uint16Array;
      return panels.map((panel) => {
        display.sink!.upload(pixels, 0, 319);
        const view = display.sink!.draw(panel, 500, 700);
        const rgba = ctx.getImageData(0, 0, 500, 700).data;
        const outcome = g.compare!(rgba, false, 500, 700, pixels, panel, view) as { mismatches: number; first: string | null };
        return { backend: display.state.backend, reason: display.state.reason, ...outcome };
      });
    }, PANELS)) as Readback[];
    for (const result of results) {
      expect(result.backend).toBe("canvas2d");
      expect(result.reason).toContain("2D fallback");
      expect(result.first ?? null).toBeNull();
      expect(result.mismatches).toBe(0);
    }
  });

  test("a real guest frame (the official boot menu) reaches the canvas", async () => {
    test.fixme(true, "this harness cannot read the transferred canvas back; m9.spec.ts checks the real menu on the glass through a screenshot");
  });
});
