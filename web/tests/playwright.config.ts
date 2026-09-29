// Run from `web/`: `bun run e2e`. The projects come from `browsers.ts`.

import { defineConfig } from "@playwright/test";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { runnableRows } from "./browsers";

const HERE = dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.PEMU_WEB_PORT ?? 4173);
const WEB = join(HERE, "..");

/**
 * The tags each engine's project leaves out: other engines' own tests, and in Firefox the tests
 * whose figures are the core's speed (`@wasm-speed`). Playwright's Firefox runs wasm on its
 * baseline compiler only, about 6x slower than Firefox Developer Edition, so its figure says
 * nothing about Firefox.
 */
const ONLY_ELSEWHERE = {
  chromium: /@(webkit|firefox)-only/,
  webkit: /@(chromium|firefox)-only/,
  firefox: /@(chromium|webkit)-only|@wasm-speed/,
} as const;

export default defineConfig({
  testDir: HERE,
  testMatch: /.*\.spec\.ts$/,
  // Outside the tree: traces and screenshots are run litter, and nothing in `web/` ignores them.
  outputDir: join(tmpdir(), "passportsim-playwright"),
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  timeout: 60_000,
  use: {
    baseURL: `http://127.0.0.1:${PORT}/`,
    // The page picks its language from the browser; the tests read its English.
    locale: "en-US",
  },
  // A test tagged `@chromium-only`, `@webkit-only` or `@firefox-only` is left out of the other
  // projects rather than skipped there.
  projects: runnableRows().map((row) => ({
    name: row.id,
    grepInvert: ONLY_ELSEWHERE[row.engine],
    use: { browserName: row.engine, ...(row.channel ? { channel: row.channel } : {}) },
  })),
  webServer: {
    command: `bun run build && bun tests/serve.ts ${PORT}`,
    cwd: WEB,
    url: `http://127.0.0.1:${PORT}/index.html`,
    reuseExistingServer: false,
    timeout: 120_000,
  },
});
