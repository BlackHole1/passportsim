// The browser matrix, as data the config and the report both read. Playwright Chromium and Firefox
// run on every host, Playwright WebKit on macOS only, and the installed Chrome and Edge through the
// `chrome` and `msedge` channels on Windows only (`runnableRows`).

import { existsSync } from "node:fs";
import { join } from "node:path";
import { chromium, firefox, webkit } from "@playwright/test";

export interface BrowserRow {
  readonly id: string;
  readonly engine: "chromium" | "webkit" | "firefox";
  readonly host: "macos" | "windows" | "any";
  /** An installed browser to use instead of the Playwright build; absent for the Playwright builds. */
  readonly channel?: "chrome" | "msedge";
}

export const BROWSER_ROWS: readonly BrowserRow[] = [
  { id: "chromium", engine: "chromium", host: "any" },
  { id: "webkit", engine: "webkit", host: "macos" },
  { id: "firefox", engine: "firefox", host: "any" },
  { id: "windows-chrome", engine: "chromium", host: "windows", channel: "chrome" },
  { id: "windows-msedge", engine: "chromium", host: "windows", channel: "msedge" },
];

export function hostOs(platform: NodeJS.Platform = process.platform): "macos" | "windows" | "other" {
  return platform === "darwin" ? "macos" : platform === "win32" ? "windows" : "other";
}

/** The rows that become projects on this host: each row on its own host, Chromium everywhere. */
export function runnableRows(platform: NodeJS.Platform = process.platform): BrowserRow[] {
  const os = hostOs(platform);
  return BROWSER_ROWS.filter((row) => row.host === "any" || row.host === os);
}

export function rowById(id: string): BrowserRow | undefined {
  return BROWSER_ROWS.find((row) => row.id === id);
}

/** The row a Playwright project runs, from its `browserName` and `channel` fixtures. */
export function rowOf(browserName: string, channel: string | undefined): BrowserRow | undefined {
  return BROWSER_ROWS.find((row) => row.engine === browserName && row.channel === (channel || undefined));
}

/**
 * Where an installed browser of a channel may be, in the order tried: the installers' per-machine
 * and per-user locations, which Playwright also launches from. An unset variable drops its
 * candidate rather than producing a relative path.
 */
export function channelCandidates(
  channel: NonNullable<BrowserRow["channel"]>,
  env: Readonly<Record<string, string | undefined>> = process.env,
): string[] {
  const under = (variable: string, ...parts: string[]): string[] => {
    const base = env[variable];
    return base === undefined || base === "" ? [] : [join(base, ...parts)];
  };
  if (channel === "chrome") {
    const rel = ["Google", "Chrome", "Application", "chrome.exe"];
    return [...under("LOCALAPPDATA", ...rel), ...under("ProgramFiles", ...rel), ...under("ProgramFiles(x86)", ...rel)];
  }
  const rel = ["Microsoft", "Edge", "Application", "msedge.exe"];
  return [...under("ProgramFiles(x86)", ...rel), ...under("ProgramFiles", ...rel), ...under("LOCALAPPDATA", ...rel)];
}

/**
 * Why a row's browser cannot launch here, or `null` when it is installed. The harness never
 * downloads a browser; Playwright builds are found under `PLAYWRIGHT_BROWSERS_PATH`.
 */
export function missingBrowser(
  row: BrowserRow,
  env: Readonly<Record<string, string | undefined>> = process.env,
  exists: (path: string) => boolean = existsSync,
): string | null {
  if (row.channel !== undefined) {
    const candidates = channelCandidates(row.channel, env);
    if (candidates.some(exists)) {
      return null;
    }
    const name = row.channel === "chrome" ? "Google Chrome" : "Microsoft Edge";
    return `${name} (Playwright channel \`${row.channel}\`, row ${row.id}) is not installed at ${candidates.join(" or ") || "any location this host names"}; install the browser itself (not downloaded by the suite, which must run offline)`;
  }
  const type = { chromium, webkit, firefox }[row.engine];
  const path = type.executablePath();
  return exists(path)
    ? null
    : `Playwright ${row.engine} is not installed at ${path}; install it with \`bunx playwright install ${row.engine}\` (not downloaded by the suite, which must run offline)`;
}

/**
 * The skip reason of a row whose browser is missing, or `null` when the row must run. With
 * `PEMU_E2E_REQUIRE_BROWSERS=1` a missing browser is not a skip: the launch fails the row.
 */
export function browserSkip(
  missing: string | null,
  env: Readonly<Record<string, string | undefined>> = process.env,
): string | null {
  if (missing === null) {
    return null;
  }
  return env.PEMU_E2E_REQUIRE_BROWSERS === "1" ? null : missing;
}
