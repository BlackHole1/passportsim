// Nothing leaves the browser: on the files-only static server (as the Cloudflare deployment is),
// the page makes only GET requests to its own origin while it boots the demo, takes a dropped
// firmware, saves a state, takes a screenshot and uses the firmware history.
//
// Requests are read from the browser context, which sees Worker fetches too; the test checks it
// saw the Worker's fetch of the wasm core. `blob:` and `data:` URLs are not network requests.
//
// The play box is not used here. It too makes only GETs to the page's own origin, to the relay
// beside the page, which `play.spec.ts` checks; what the relay passes on is in `playRelay.test.ts`.

import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { expect, test } from "@playwright/test";
import { readOfficialFiles } from "./demoBundle";
import { browserGaps, openPage, waitForLoaded, waitForStatus } from "./harness";
import { findCore } from "./preconditions";

const OWNER_FIRMWARE = process.env.PEMU_E2E_OWNER_BIN ?? join(homedir(), "Downloads", "passport-keys-firmware-1.0.0.bin");

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

test("the page sends nothing anywhere: only GETs to its own origin through demo, drop, save, screenshot and history", async ({ page, context, baseURL }) => {
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  const demo = readOfficialFiles(process.env);
  test.skip(!("files" in demo), "files" in demo ? "" : "mismatch" in demo ? demo.mismatch : demo.skip);
  test.skip(!existsSync(OWNER_FIRMWARE), `the owner's firmware is not at ${OWNER_FIRMWARE}; set PEMU_E2E_OWNER_BIN to a bare merged .bin`);
  test.setTimeout(240_000);
  const origin = new URL(baseURL ?? "http://127.0.0.1/").origin;
  const seen: { method: string; url: string }[] = [];
  context.on("request", (request) => {
    seen.push({ method: request.method(), url: request.url() });
  });
  const sockets: string[] = [];
  page.on("websocket", (socket) => {
    sockets.push(socket.url());
  });
  page.on("dialog", (dialog) => void dialog.accept());

  await openPage(page, 1440, 900, "advanced");
  expect((await waitForStatus(page, 60_000)).ok, "the demo is up").toBe(true);
  // A firmware is dropped and boots, and is kept in the history.
  await page.locator('[data-loader-input="files"]').setInputFiles(OWNER_FIRMWARE);
  const name = basename(OWNER_FIRMWARE).replace(/\.[^.]*$/, "");
  await waitForLoaded(page, name, 90_000);
  await page.locator('[data-action="snap"]').click();
  await expect(page.locator('.toolbar-notice[data-notice="state"]')).toBeVisible();
  const shot = page.waitForEvent("download");
  await page.locator('[data-action="screenshot"]').first().click();
  await shot;
  // The history: its picture is taken, it survives a reload, it loads again and downloads.
  await page.locator('[data-action="history"]').first().click();
  const entry = page.locator("[data-history] [data-history-entry]");
  await expect(entry.locator("[data-history-thumbnail]")).toBeVisible({ timeout: 60_000 });
  await page.keyboard.press("Escape");
  await page.reload();
  await page.locator('[data-action="history"]').first().click();
  const again = page.waitForEvent("download");
  await entry.locator('[data-history-action="download"]').click();
  await again;
  await entry.locator('[data-history-action="load"]').click();
  await waitForLoaded(page, name, 90_000);
  await page.locator('[data-action="history"]').first().click();
  await page.locator('[data-history-action="clear"]').click();
  await expect(entry).toHaveCount(0);

  const network = seen.filter((one) => /^(https?|wss?):/.test(one.url));
  expect(
    network.some((one) => one.url.endsWith("/pemu_wasm.wasm")),
    `the Worker's fetch of the core was seen: ${network.map((one) => one.url).join(", ")}`,
  ).toBe(true);
  expect(network.filter((one) => one.method !== "GET"), "every request is a GET").toEqual([]);
  expect(network.filter((one) => new URL(one.url).origin !== origin), `every request goes to ${origin}`).toEqual([]);
  expect(sockets, "no socket is opened").toEqual([]);
});
