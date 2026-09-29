// The corpus `pk` merged bin, dropped with no ELF, still starts its radio: the machine recovers the
// hooked functions from the image, so the firmware runs past BLE init and answers a button. Skips
// only when `PEMU_E2E_IMAGE_PK` names no corpus directory.

import { expect, test } from "@playwright/test";
import { imageName } from "../src/app/load";
import { runsPastBleInitAndAnswersDown } from "./elfless";
import { browserGaps, consoleText, loadImage, openPage } from "./harness";
import { imageSource, type ImageSource } from "./preconditions";

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

function bareBin(source: ImageSource): ImageSource | null {
  const bins = source.files.filter((file) => file.relative.endsWith(".bin") && !file.relative.includes("/"));
  if (bins.length !== 1) {
    return null;
  }
  const bin = bins[0]!;
  return { ...source, directory: false, root: bin.path, name: imageName(bin.path, false), files: [bin] };
}

test("the corpus pk bin alone runs past BLE init and answers a press of down", async ({ page }) => {
  test.setTimeout(120_000);
  const corpus = imageSource("pk");
  const source = corpus === null ? null : bareBin(corpus);
  test.skip(source === null, "no pk image: PEMU_E2E_IMAGE_PK names no directory with one merged bin");
  await openPage(page);
  await loadImage(page, source!);
  await runsPastBleInitAndAnswersDown(page);
  expect(await consoleText(page), "the application came up after BLE").toMatch(/pk_app: ready/);
});
