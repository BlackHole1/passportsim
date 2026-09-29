// What a bare merged `.bin` with no ELF must do in the page: run past BLE init and redraw its
// screen on a press. Shared by `elfless.spec.ts` and `honesty.spec.ts`. The press goes through the
// skin; these firmwares light the key's row for 180 ms of guest time, so the glass is read while
// the button is held.

import { expect, type Page } from "@playwright/test";
import { consoleText, virtualUs, waitForLine, waitForStatus } from "./harness";
import { decodeRgbPng } from "./png";

/** Wall time the guest is given; the page paces at rate 1 and BLE init is at about 0.4 s. */
const GUEST_MS = 40_000;

/** The virtual time a run must pass: well beyond BLE init, where an unbound radio stops it. */
const PAST_BLE_INIT_US = 3_000_000;

/**
 * A press's redraw as a share of the glass's RGB samples, so it holds at any zoom. Idle screens
 * change about 0.06 % on their own; the DOWN highlight about 10 %.
 */
const PRESS_REDRAW = 0.01;

async function glass(page: Page): Promise<Buffer> {
  return page.locator("canvas.glass:not(.rewind-frame)").screenshot();
}

function differing(a: Buffer, b: Buffer): number {
  const [x, y] = [decodeRgbPng(a).rgb, decodeRgbPng(b).rgb];
  let count = 0;
  for (let at = 0; at < x.length; at += 1) {
    count += x[at] === y[at] ? 0 : 1;
  }
  return count / Math.max(1, x.length);
}

/** Whether every pixel of a glass screenshot is one colour, which a drawn screen never is. */
function blank(shot: Buffer): boolean {
  const { rgb } = decodeRgbPng(shot);
  return rgb.every((value, at) => value === rgb[at % 3]);
}

/** The loaded image runs past BLE init with no stop, and a held DOWN on the skin redraws its screen. */
export async function runsPastBleInitAndAnswersDown(page: Page): Promise<void> {
  expect(await waitForStatus(page), "`status` answers once the image runs").toMatchObject({ ok: true });
  await waitForLine(page, /BLE_INIT: Bluetooth MAC: /, GUEST_MS);
  await expect.poll(async () => (await virtualUs(page)) ?? 0, { timeout: GUEST_MS }).toBeGreaterThan(PAST_BLE_INIT_US);
  expect(await consoleText(page), "no panic on the way").not.toMatch(/Guru Meditation|panic/i);
  await expect(page.locator("[data-stop]"), "the machine did not stop").toHaveCount(0);

  const before = await glass(page);
  expect(blank(before), "the glass shows the screen before the press").toBe(false);
  const down = page.locator('[data-control="down"]');
  await down.dispatchEvent("pointerdown");
  let most = 0;
  try {
    // The highlight is up for about 300 ms, so the glass is read back to back rather than polled.
    const deadline = Date.now() + 2_000;
    while (most < PRESS_REDRAW && Date.now() < deadline) {
      most = Math.max(most, differing(before, await glass(page)));
    }
  } finally {
    await down.dispatchEvent("pointerup");
  }
  expect(most, "the press of down redraws the screen").toBeGreaterThanOrEqual(PRESS_REDRAW);
}
