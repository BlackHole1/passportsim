// The image loader in a real browser, for what `load.test.ts` cannot answer:
//
// 1. a real drag: a `DataTransfer` built in the page from the bytes `serve.ts` publishes
//    (`/e2e-image/<id>/<file>`), dropped on the shell;
// 2. an `idf.py` build directory whose `flash_files` use `\`, booted through the
//    `webkitdirectory` input;
// 3. a refusal: the page states it, the running machine keeps running, and nothing is thrown.
//
// The image is `pk` from `PEMU_E2E_IMAGE_PK`; it skips on the absences of `preconditions.ts`.

import { expect, test } from "@playwright/test";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { browserGaps, loadImage, loaderState, openPage, waitForLoaded, waitForStatus } from "./harness";
import { currentPrecondition, imageUrl, type ImageSource } from "./preconditions";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function pkSource(): ImageSource {
  const pre = currentPrecondition("pk", {
    name: "the loader",
    waitsOn: "nothing: this file tests the loader, not a firmware result",
  });
  test.skip(pre.kind === "skip", pre.kind === "skip" ? pre.reason : "");
  if (pre.kind !== "run") {
    test.skip();
    throw new Error("unreachable");
  }
  return pre.source;
}

function mergedBinOf(source: ImageSource): { relative: string; path: string } {
  const found = source.files.find((file) => file.relative.endsWith(".bin"));
  expect(found, `\`${source.image}\` holds no merged bin`).toBeDefined();
  return found as { relative: string; path: string };
}

test.describe("the page's image loader", () => {
  test("a dragged firmware is fetched, dropped and booted, and the header names it", async ({ page }) => {
    const source = pkSource();
    await openPage(page);

    const urls = source.files.map((file) => ({ url: imageUrl(source.image, file.relative), name: file.relative }));
    const dropped = await page.evaluate(async ({ urls, root }) => {
      if (typeof DataTransfer !== "function") {
        return "this browser has no DataTransfer constructor";
      }
      const transfer = new DataTransfer();
      for (const entry of urls) {
        const response = await fetch(entry.url);
        if (!response.ok) {
          return `${entry.url} is not served (${response.status})`;
        }
        transfer.items.add(new File([await response.blob()], entry.name));
      }
      const zone = document.querySelector("[data-loader]");
      if (!zone) {
        return "the page has no loader strip";
      }
      zone.dispatchEvent(new DragEvent("dragover", { bubbles: true, cancelable: true, dataTransfer: transfer }));
      zone.dispatchEvent(new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer: transfer }));
      return root;
    }, { urls, root: source.root });
    expect(dropped, "the drop reached the page").toBe(source.root);

    // A drag carries files, not a directory entry, so the drop is named after the flash image.
    const name = mergedBinOf(source).relative.replace(/\.[^.]*$/, "");
    await waitForLoaded(page, name);
    const loaded = await loaderState(page);
    expect(loaded.image).toBe(name);
    const status = await waitForStatus(page);
    expect(JSON.stringify(status.ok ? status.json : null)).toContain(`"fw":"${loaded.image}"`);
  });

  test("an idf.py build directory whose flash_files use backslashes is assembled and booted", async ({ page }) => {
    const source = pkSource();
    const merged = mergedBinOf(source);
    // A build directory with the corpus image as its single flash file at 0x0, its path spelled as a
    // `flasher_args.json` written on Windows spells it.
    const root = mkdtempSync(join(tmpdir(), "pemu-build-"));
    const build = join(root, "build");
    mkdirSync(join(build, "images"), { recursive: true });
    copyFileSync(merged.path, join(build, "images", "merged.bin"));
    writeFileSync(
      join(build, "flasher_args.json"),
      JSON.stringify({ flash_files: { "0x0": "images\\merged.bin" } }),
    );
    try {
      await openPage(page);
      await page.locator('[data-loader-input="directory"]').setInputFiles(build);
      await waitForLoaded(page, "build");
      const loaded = await loaderState(page);
      expect(loaded.image, "a directory is named after itself").toBe("build");
      expect(loaded.message, "the note names the file and the offset it was placed at").toContain(
        "images/merged.bin at 0x0",
      );
      const status = await waitForStatus(page);
      expect(JSON.stringify(status.ok ? status.json : null)).toContain('"fw":"build"');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a file that is no firmware is refused on the page, and the running machine is untouched", async ({ page }) => {
    const source = pkSource();
    await openPage(page);
    await loadImage(page, source);

    const root = mkdtempSync(join(tmpdir(), "pemu-not-firmware-"));
    const notFirmware = join(root, "notes.txt");
    writeFileSync(notFirmware, "this is not a firmware image\n");
    try {
      await page.locator('[data-loader-input="files"]').setInputFiles(notFirmware);
      await expect.poll(async () => (await loaderState(page)).state, { timeout: 30_000 }).toBe("refused");
      const refused = await loaderState(page);
      expect(refused.message).toContain("is not a firmware image");
      expect(refused.message, "the refusal names only the file name the browser gave").toContain("notes.txt");
      expect(refused.message, "and never a host path").not.toContain(root);
      expect(refused.image, "the header keeps the image that is running").toBe(source.name);
      const status = await waitForStatus(page);
      expect(JSON.stringify(status.ok ? status.json : null)).toContain(`"fw":"${source.name}"`);

      // The loader still works after a refusal: a merged bin alone loads on the next try.
      await page.locator('[data-loader-input="files"]').setInputFiles(merged(source));
      await waitForLoaded(page, mergedBinOf(source).relative.replace(/\.[^.]*$/, ""));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

function merged(source: ImageSource): string {
  return mergedBinOf(source).path;
}
