// "Copy as CLI", "Copy as scenario step" and "Record scenario" in a real browser. Export needs a
// journal, and a button press needs a running machine, so these run on the served demo and skip
// where there is none.

import { expect, test } from "@playwright/test";
import { readOfficialFiles } from "./demoBundle";
import { browserGaps, holdControl, openPage, showTab, waitForStatus } from "./harness";

// A test whose browser is not installed skips with the install hint rather than failing.
for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

const official = readOfficialFiles();
if ("mismatch" in official) {
  throw new Error(official.mismatch);
}
test.skip("skip" in official, "skip" in official ? official.skip : "");

test("a click on OK copies as the exact CLI lines and the exact steps", async ({ page }) => {
  await openPage(page);
  // A press reaches a machine only once one is up; before that it is refused and nothing is held.
  expect(await waitForStatus(page)).toMatchObject({ ok: true });
  await holdControl(page, "ok", 30);
  await showTab(page, "events");
  // The press and release rows, pinned by `seq`: the boot also journals two `clock` rows
  // (`page.ts` `ready`), which have no scenario step and would blank the export box.
  const inputs = page.locator('#pane-events tbody tr[data-source="ui"]').filter({ has: page.locator('td:text-is("input")') });
  // The release goes out once the guest has held OK for the minimum hold, a moment after the up.
  await expect(inputs).toHaveCount(2);
  const output = page.locator('#pane-events [data-export="output"]');
  await page.locator('#pane-events [data-export="shell"]').selectOption("sh");
  for (const [index, action] of [[0, "press"], [1, "release"]] as const) {
    const seq = await inputs.nth(index).getAttribute("data-seq");
    const row = page.locator(`#pane-events tbody tr[data-seq="${seq}"]`);
    await row.locator('[data-copy="cli"]').click();
    await expect(output).toHaveValue(`passportsim input ok ${action}`);
    await row.locator('[data-copy="step"]').click();
    await expect(output).toHaveValue(`- press: {button: ok, action: ${action}}`);
  }
});

test("Record scenario exports the session as scenario@1 YAML", async ({ page }) => {
  await openPage(page);
  expect(await waitForStatus(page)).toMatchObject({ ok: true });
  await showTab(page, "events");
  const record = page.locator('#pane-events [data-export="record"]');
  await record.click();
  await expect(record).toHaveAttribute("aria-pressed", "true");
  await holdControl(page, "down", 30);
  await expect(page.locator('#pane-events tbody tr[data-source="ui"]')).not.toHaveCount(0);
  await record.click();
  const yaml = await page.locator('#pane-events [data-export="output"]').inputValue();
  expect(yaml).toContain("schema: passportsim/scenario@1\nname: recorded-session\n");
  expect(yaml).toContain("image: official\nsetup: {power: on, usb: open}\n");
});
