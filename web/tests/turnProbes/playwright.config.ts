// Two event-loop turn probes, run by hand and never by a tier:
// `bunx playwright test -c tests/turnProbes/playwright.config.ts` from `web/`. They are named `*.probe.ts`
// so the suite config never picks them up.

import { defineConfig } from "@playwright/test";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

export default defineConfig({
  testDir: dirname(fileURLToPath(import.meta.url)),
  testMatch: /.*\.probe\.ts$/,
  workers: 1,
  reporter: [["list"]],
  projects: [
    { name: "chromium", use: { browserName: "chromium" } },
    { name: "webkit", use: { browserName: "webkit" } },
  ],
});
