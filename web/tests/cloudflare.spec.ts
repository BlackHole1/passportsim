// The packaged web bundle served by Cloudflare's own runtime (`wrangler dev`), in a real browser:
// the page is cross-origin isolated, the wasm core comes back as `application/wasm` with the
// daemon's headers, the bundled demo boots and takes a button press, and a dropped firmware boots
// in its place. The files themselves are pinned by the package tests.
//
// Opt-in, since it needs a `wrangler dev` started on a packaged bundle, with its state directory
// outside the bundle (`docs/deploy-cloudflare.md` says why):
//
//   cd target/package/passportsim-<ver>-web
//   bunx wrangler@4 dev --ip 127.0.0.1 --port 8787 --persist-to "$TMPDIR/passportsim-wrangler"
//   PEMU_E2E_CLOUDFLARE_URL=http://127.0.0.1:8787/ PEMU_E2E_CLOUDFLARE_IMAGE=<merged .bin> \
//     bun run e2e --project=chromium tests/cloudflare.spec.ts
//
// Skips, each named: no `PEMU_E2E_CLOUDFLARE_URL`; no `PEMU_E2E_CLOUDFLARE_IMAGE`.

import { existsSync, readFileSync } from "node:fs";
import { basename } from "node:path";
import { expect, test, type Page } from "@playwright/test";
import { browserGaps, holdControl, pinPrefs, waitForLoaded, waitForStatus, watchPage } from "./harness";

const URL_VARIABLE = "PEMU_E2E_CLOUDFLARE_URL";
const IMAGE_VARIABLE = "PEMU_E2E_CLOUDFLARE_IMAGE";

const BOOT_MS = 90_000;

/** The headers every static response of the daemon carries apart from `cache-control` (`webui.rs`). */
const DAEMON_HEADERS: Readonly<Record<string, string>> = {
  "cross-origin-opener-policy": "same-origin",
  "cross-origin-embedder-policy": "require-corp",
  "cross-origin-resource-policy": "same-origin",
  "x-content-type-options": "nosniff",
};

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

async function glass(page: Page): Promise<Buffer> {
  return page.locator(".skin-glass").screenshot();
}

test("the packaged web bundle under wrangler dev is isolated, serves wasm as application/wasm, boots the demo, takes a button and a dropped firmware", async ({
  page,
}) => {
  test.setTimeout(240_000);
  const base = process.env[URL_VARIABLE];
  const image = process.env[IMAGE_VARIABLE];
  test.skip(!base, `no ${URL_VARIABLE}: start \`bunx wrangler dev\` in a packaged web bundle and name its URL`);
  test.skip(!image, `no ${IMAGE_VARIABLE}: name a merged firmware .bin to drop on the page`);
  if (!base || !image) {
    return;
  }
  expect(existsSync(image), `${IMAGE_VARIABLE} names ${image}, which does not exist`).toBe(true);

  const responses = new Map<string, Record<string, string>>();
  page.on("response", (response) => {
    const path = new URL(response.url()).pathname;
    if (response.url().startsWith(base) && !responses.has(path)) {
      responses.set(path, response.headers());
    }
  });
  watchPage(page);
  await pinPrefs(page, "simple");
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(new URL("?mode=simple&lang=en", base).href);
  await expect(page.locator("#app .app"), "the shell mounted").toBeVisible({ timeout: 30_000 });
  await page.waitForFunction(() => typeof (globalThis as { passportEmu?: unknown }).passportEmu === "object");
  expect(await page.evaluate(() => globalThis.crossOriginIsolated), "the page wrangler serves is cross-origin isolated").toBe(true);

  const booted = await waitForStatus(page, BOOT_MS);
  expect(booted.ok, `status answers once the demo is up: ${booted.ok ? "" : booted.error}`).toBe(true);
  expect(JSON.stringify(booted.ok ? booted.json : null), "with nothing dropped the page runs the bundled demo").toContain(
    '"fw":"official"',
  );

  for (const [path, type] of [
    ["/", "text/html"],
    ["/main.js", "text/javascript"],
    ["/worker.js", "text/javascript"],
    ["/pemu_wasm.wasm", "application/wasm"],
    ["/official.pebundle", "application/octet-stream"],
  ] as const) {
    const headers = responses.get(path);
    expect(headers, `the page fetched ${path}: ${[...responses.keys()].join(", ")}`).toBeDefined();
    expect(headers?.["content-type"] ?? "", `${path} has its type`).toContain(type);
    for (const [name, value] of Object.entries(DAEMON_HEADERS)) {
      expect(headers?.[name], `${path} carries ${name}`).toBe(value);
    }
  }

  // Once the menu holds still, `down` changes the glass. Waiting for two equal shots first keeps a
  // boot still drawing from passing as the press.
  let before = await glass(page);
  await expect
    .poll(
      async () => {
        await page.waitForTimeout(1_000);
        const now = await glass(page);
        const steady = now.equals(before);
        before = now;
        return steady;
      },
      { timeout: BOOT_MS, message: "the demo's menu holds still" },
    )
    .toBe(true);
  await holdControl(page, "down", 150);
  await expect
    .poll(async () => !(await glass(page)).equals(before), { timeout: 10_000, message: "the glass changes after `down`" })
    .toBe(true);

  const name = basename(image).replace(/\.[^.]*$/, "");
  const dropped = await page.evaluate(
    ({ bytes, file }) => {
      const data = Uint8Array.from(atob(bytes), (c) => c.charCodeAt(0));
      const transfer = new DataTransfer();
      transfer.items.add(new File([data], file));
      const zone = document.querySelector("[data-loader]");
      if (!zone) {
        return "the page has no loader strip";
      }
      zone.dispatchEvent(new DragEvent("dragover", { bubbles: true, cancelable: true, dataTransfer: transfer }));
      zone.dispatchEvent(new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer: transfer }));
      return "dropped";
    },
    { bytes: readFileSync(image).toString("base64"), file: basename(image) },
  );
  expect(dropped).toBe("dropped");
  await waitForLoaded(page, name, BOOT_MS);
  const status = await waitForStatus(page);
  expect(JSON.stringify(status.ok ? status.json : null), "the page runs the dropped image").toContain(`"fw":"${name}"`);
});
