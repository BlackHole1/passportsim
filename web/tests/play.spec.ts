// The play box in a real browser: the form, the page's own `fetch` to the relay beside it, and the
// boot of what it downloaded. The relay is on the page's own origin (`src/edge/playRelay.js`, the
// deployed site's Worker script); the test server has none, so:
//
// 1. as the test server is, with no relay: the page says this copy has none and links the play's
//    own page, and the machine that was running keeps running;
// 2. with Playwright's router answering as the relay does: the firmware is downloaded, checked and
//    booted, and every request of it is a GET to the page's own origin.
//
// The play site is never reached. The relay itself is tested in `src/edge/playRelay.test.ts`, and
// against the real site under `wrangler dev` by `cloudflare.spec.ts`.
//
// The firmware of the second is `pk` from `PEMU_E2E_IMAGE_PK`; it skips on the absences of
// `preconditions.ts`.

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { expect, test, type BrowserContext } from "@playwright/test";
import { browserGaps, loaderState, openPage, waitForLoaded, waitForStatus } from "./harness";
import { currentPrecondition, findCore } from "./preconditions";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

const PLAY_PAGE = "https://ai-passport.folotoy.cn/plays/1039/";
const API_PATH = "/play-site/api/plays/id/1039";
const DOWNLOAD_PATH = "/play-site/api/download/community/community-b6eb4756";
const RELAY_HEADERS = { "x-play-relay": "1", "cache-control": "no-store" };

/** Every request the context makes while a test runs, `blob:` and `data:` aside. */
function watchRequests(context: BrowserContext): { method: string; url: string }[] {
  const seen: { method: string; url: string }[] = [];
  context.on("request", (request) => {
    if (/^https?:/.test(request.url())) {
      seen.push({ method: request.method(), url: request.url() });
    }
  });
  return seen;
}

/** Answers the relay's two paths the way `playRelay.js` does, from `firmware`. */
async function relay(context: BrowserContext, firmware: Buffer): Promise<void> {
  await context.route("**/play-site/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === API_PATH) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        headers: RELAY_HEADERS,
        body: JSON.stringify({
          ok: true,
          play: {
            id: 1039,
            revisionId: 2347,
            title: { zh: "口袋天气", en: "Pocket Weather" },
            firmware: {
              available: true,
              size: firmware.length,
              sha256: createHash("sha256").update(firmware).digest("hex"),
              url: DOWNLOAD_PATH.replace("/play-site", ""),
              format: "esp-merged-0x0",
            },
          },
        }),
      });
    } else if (path === DOWNLOAD_PATH) {
      await route.fulfill({ status: 200, contentType: "application/octet-stream", headers: RELAY_HEADERS, body: firmware });
    } else {
      await route.fulfill({ status: 404, contentType: "application/json", headers: RELAY_HEADERS, body: JSON.stringify({ detail: "not a path this relay serves" }) });
    }
  });
}

test.describe("the play box", () => {
  test("a server with no relay is said so, with the play's page to open, and nothing is replaced", async ({ page, context, baseURL }) => {
    const core = findCore(process.env);
    test.skip("skip" in core, "skip" in core ? core.skip : "");
    const origin = new URL(baseURL ?? "http://127.0.0.1/").origin;
    await openPage(page, 1280, 900, "simple");
    const before = await loaderState(page);
    const seen = watchRequests(context);

    await page.locator("[data-play-input]").fill(PLAY_PAGE);
    await page.locator("[data-play-input]").press("Enter");
    await expect.poll(async () => (await loaderState(page)).state).toBe("refused");
    const refused = await loaderState(page);
    expect(refused.message).toBe(
      "this server cannot load a play directly from ai-passport.folotoy.cn. Download the firmware from the play's page and drop it here.",
    );
    expect(refused.image, "the image that was running is still the one named").toBe(before.image);
    await expect(page.locator("[data-play-page]")).toHaveAttribute("href", PLAY_PAGE);
    expect(
      seen.filter((one) => one.url.includes("play") || new URL(one.url).origin !== origin),
      "the page's own server was asked once, and the play site never",
    ).toEqual([{ method: "GET", url: `${origin}${API_PATH}` }]);
  });

  test("with a relay: the play's firmware is downloaded, checked and booted, by GETs to the page's own origin", async ({ page, context, baseURL }) => {
    const pre = currentPrecondition("pk", { name: "the play box", waitsOn: "nothing: this file tests the loader, not a firmware result" });
    test.skip(pre.kind === "skip", pre.kind === "skip" ? pre.reason : "");
    if (pre.kind !== "run") {
      throw new Error("unreachable");
    }
    const merged = pre.source.files.find((file) => file.relative.endsWith(".bin"));
    expect(merged, `\`${pre.source.image}\` holds no merged bin`).toBeDefined();
    const firmware = readFileSync((merged as { path: string }).path);
    test.setTimeout(120_000);
    const origin = new URL(baseURL ?? "http://127.0.0.1/").origin;
    await relay(context, firmware);
    await openPage(page, 1280, 900, "simple");
    const seen = watchRequests(context);

    await page.locator("[data-play-input]").fill("1039");
    await page.locator('[data-action="load-play"]').click();
    await waitForLoaded(page, "play-1039-r2347", 90_000);
    const status = await waitForStatus(page);
    expect(JSON.stringify(status.ok ? status.json : null)).toContain('"fw":"play-1039-r2347"');
    await expect(page.locator(".log-panel")).toContainText("Play 1039, Pocket Weather: revision 2347");
    await expect(page.locator(".log-panel")).toContainText("The download has the size and the SHA-256 the site states");
    expect(seen.filter((one) => one.url.includes("/play-site/"))).toEqual([
      { method: "GET", url: `${origin}${API_PATH}` },
      { method: "GET", url: `${origin}${DOWNLOAD_PATH}` },
    ]);
    expect(seen.filter((one) => one.method !== "GET" || new URL(one.url).origin !== origin), "nothing but GETs to the page's own origin").toEqual([]);
  });
});
