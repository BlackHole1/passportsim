// The paced session in real Safari, run by hand and wired into no tier. Run from `web/` on a quiet
// Mac with Safari's remote automation enabled (`safaridriver --enable`, once, as an administrator):
//
//     PASSPORTSIM_DATA_ROOT="$HOME/Library/Application Support/passportsim" bun run paced:safari
//
// It builds the wasm core (unless `PEMU_E2E_CORE` names one) and the web bundle, serves them with
// `tests/serve.ts`, starts `safaridriver` and runs `runPacedSession` in one automation window, the
// same page, calls and measurements as `m9b.spec.ts`. Differences from the Playwright driver:
//
// - Audio: Safari has no fake-media flags. The session's first action on each page is a WebDriver
//   click on the Perf tab, a trusted gesture that resumes the AudioContext. The record says so
//   (`gesture`) and whether the worklet rendered (`audioRendered`).
// - No init script: the audio trace cannot be enabled, and `PEMU_PACED_SPIN` and
//   `PEMU_PACED_CHANNEL_TURN` are refused. `PEMU_PACED_ONLY` and `PEMU_PACED_BLOCK_MS` work.
// - A failed gate throws and ends the run; the record keeps every figure measured before it.
//
// On a contended host the run records its figures with the verdict NOT MEASURED.
//
// Output: `<data root>/manual/paced-safari/safari-<date>.json` (a `-<workload>-only` suffix for a run cut
// short with `PEMU_PACED_ONLY`, and `-2`, `-3`, ... rather than overwrite), and one summary line on
// stdout.

import { spawn, spawnSync, type Subprocess } from "bun";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { cpus, loadavg, platform, totalmem } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { readOfficialFiles } from "../demoBundle";
import { runPacedSession, type SessionChecks, type SessionResult } from "../pacedSession";
import { findCore } from "../preconditions";
import { WebDriverPage, WebDriverSession } from "./webdriver";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const REPO = join(WEB, "..");

/** The viewport `harness.ts` `openPage` gives Playwright; the window is sized to hold it. */
const WIDTH = 1280;
const HEIGHT = 900;

class GateFailure extends Error {}

const fmt = (value: unknown) => (typeof value === "number" ? String(value) : JSON.stringify(value));

const checks: SessionChecks = {
  equal: (actual, expected, message) => {
    if (actual !== expected) {
      throw new GateFailure(`${message}: expected ${fmt(expected)}, received ${fmt(actual)}`);
    }
  },
  atLeast: (actual, floor, message) => {
    if (!(actual >= floor)) {
      throw new GateFailure(`${message}: expected >= ${floor}, received ${actual}`);
    }
  },
  atMost: (actual, ceiling, message) => {
    if (!(actual <= ceiling)) {
      throw new GateFailure(`${message}: expected <= ${ceiling}, received ${actual}`);
    }
  },
  greaterThan: (actual, floor, message) => {
    if (!(actual > floor)) {
      throw new GateFailure(`${message}: expected > ${floor}, received ${actual}`);
    }
  },
};

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = typeof address === "object" && address !== null ? address.port : 0;
      server.close(() => resolve(port));
    });
  });
}

async function waitForHttp(url: string, what: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const response = await fetch(url);
      if (response.ok) {
        return;
      }
    } catch {
      // Not listening yet.
    }
    if (Date.now() > deadline) {
      throw new Error(`${what} did not answer at ${url} within ${timeoutMs} ms`);
    }
    await Bun.sleep(100);
  }
}

function output(command: string[], cwd = REPO): string {
  const run = spawnSync(command, { cwd, stdout: "pipe", stderr: "pipe" });
  return run.success ? run.stdout.toString().trim() : "";
}

function build(what: string, command: string[], cwd: string): void {
  console.log(`SAFARI build: ${what}: ${command.join(" ")}`);
  const run = spawnSync(command, { cwd, stdout: "inherit", stderr: "inherit" });
  if (!run.success) {
    throw new Error(`${what} failed: ${command.join(" ")}`);
  }
}

function localDate(now: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

/** The record's path, never overwriting an earlier record of the same day. */
function recordPath(root: string, date: string, only: string): string {
  const dir = join(root, "manual", "paced-safari");
  mkdirSync(dir, { recursive: true });
  const stem = `safari-${date}${only ? `-${only}-only` : ""}`;
  let path = join(dir, `${stem}.json`);
  for (let n = 2; existsSync(path); n += 1) {
    path = join(dir, `${stem}-${n}.json`);
  }
  return path;
}

function summaryLine(verdict: string, safari: string, macos: string, result: SessionResult | null): string {
  const paced = Object.entries(result?.paced ?? {})
    .map(
      ([id, f]) =>
        `${id} rtf ${f.realTimeFactor.toFixed(4)} re-anchors ${f.reanchors} underruns ${f.underruns}/${f.quanta} quanta`,
    )
    .join("; ");
  const windows = Object.entries(result?.worstWindows ?? {})
    .map(([id, w]) => `${id} worst window ${w.hostMs.toFixed(2)} ms`)
    .join("; ");
  const load = result ? `loadavg ${result.worstLoad.toFixed(2)} over ${result.cores} cores` : "no session";
  return `SAFARI ${verdict}: Safari ${safari} on macOS ${macos}; ${[paced, windows, load].filter(Boolean).join("; ")}`;
}

async function main(): Promise<number> {
  if (platform() !== "darwin") {
    console.error("SAFARI: Safari runs on macOS only");
    return 2;
  }
  for (const knob of ["PEMU_PACED_SPIN", "PEMU_PACED_CHANNEL_TURN"]) {
    if (process.env[knob] === "1") {
      console.error(`SAFARI: ${knob} needs an init script, which WebDriver does not have; run it in the Playwright spec`);
      return 2;
    }
  }
  const root = process.env.PASSPORTSIM_DATA_ROOT;
  if (!root) {
    console.error(
      "SAFARI: set PASSPORTSIM_DATA_ROOT on the command line (not exported): the `official` build is read from it and the record is written under <root>/manual/paced-safari/",
    );
    return 2;
  }
  const only = process.env.PEMU_PACED_ONLY ?? "";
  const blockMs = Number(process.env.PEMU_PACED_BLOCK_MS ?? "0");

  if (!process.env.PEMU_E2E_CORE) {
    build("the wasm core", ["cargo", "build", "-p", "pemu-wasm", "--lib", "--target", "wasm32-unknown-unknown", "--profile", "wasm-release"], REPO);
  }
  build("the web bundle", ["bun", "run", "build"], WEB);
  const core = findCore(process.env);
  if ("skip" in core) {
    console.error(`SAFARI: ${core.skip}`);
    return 2;
  }
  const official = readOfficialFiles();
  if (!("files" in official)) {
    console.error(`SAFARI: ${"mismatch" in official ? official.mismatch : official.skip}`);
    return 2;
  }

  const started = new Date();
  const coreBytes = readFileSync(core.path);
  const record: Record<string, unknown> = {
    schema: "passportsim/paced-safari/1",
    leg: "Safari (manual, once per release)",
    session: "web/tests/pacedSession.ts runPacedSession, the code m9b.spec.ts runs",
    startedUtc: started.toISOString(),
    macos: {
      version: output(["sw_vers", "-productVersion"]),
      build: output(["sw_vers", "-buildVersion"]),
    },
    safaridriver: output(["safaridriver", "--version"]),
    hostMachine: {
      model: output(["sysctl", "-n", "hw.model"]),
      cpu: cpus()[0]?.model ?? "",
      cores: cpus().length,
      memoryGiB: Math.round(totalmem() / 2 ** 30),
      loadavgAtStart: loadavg(),
    },
    tree: {
      commit: output(["git", "rev-parse", "HEAD"]),
      dirty: output(["git", "status", "--porcelain"]) !== "",
    },
    core: {
      path: core.path,
      sha256: createHash("sha256").update(coreBytes).digest("hex"),
      modified: statSync(core.path).mtime.toISOString(),
    },
    knobs: { only: only || null, blockMs },
    gesture:
      "WebDriver element click (POST /session/{id}/element/{element}/click) on the Perf tab `#tab-perf`, the first DOM action on every page the session opens; the page resumes its AudioContext on that pointerdown (web/src/app/main.ts)",
    audioTrace: "not available: WebDriver has no init script to set the audio trace flag (`__PEMU_AUDIO_TRACE__`) before the page loads",
  };

  const children: Subprocess[] = [];
  let session: WebDriverSession | null = null;
  // A holder, not a `let`: the session hands its result over from a callback.
  const held: { result: SessionResult | null } = { result: null };
  let verdict: string;
  let failure: string | null = null;
  try {
    const webPort = await freePort();
    const server = spawn(["bun", "tests/serve.ts", String(webPort)], { cwd: WEB, stdout: "inherit", stderr: "inherit", env: process.env });
    children.push(server);
    const base = `http://127.0.0.1:${webPort}/`;
    await waitForHttp(`${base}index.html`, "tests/serve.ts", 30_000);

    const driverPort = await freePort();
    const driver = spawn(["safaridriver", "-p", String(driverPort)], { stdout: "inherit", stderr: "inherit" });
    children.push(driver);
    const driverUrl = `http://127.0.0.1:${driverPort}`;
    await waitForHttp(`${driverUrl}/status`, "safaridriver", 15_000);
    session = await WebDriverSession.open(driverUrl);
    record.safari = {
      browserName: session.capabilities.browserName,
      browserVersion: session.capabilities.browserVersion,
      platformName: session.capabilities.platformName,
    };
    const page = new WebDriverPage(session);
    await page.prepare(WIDTH, HEIGHT);

    let pagesOpened = 0;
    let isolated: boolean | null = null;
    const annotations: { type: string; description: string }[] = [];
    const notRun: string[] = [];
    record.annotations = annotations;
    record.notRun = notRun;
    await runPacedSession(
      page,
      {
        engine: "safari",
        onResult: (filling) => {
          held.result = filling;
        },
        openPage: async () => {
          // Advanced mode, where the session's tabs are; English, which its assertions read.
          await page.goto(`${base}?mode=advanced&lang=en`);
          const deadline = Date.now() + 30_000;
          while (
            !(await page.evaluate(
              () =>
                document.querySelector("#app .app") !== null &&
                typeof (globalThis as { passportEmu?: unknown }).passportEmu === "object",
              null,
            ))
          ) {
            if (Date.now() > deadline) {
              throw new Error("the page shell or `passportEmu` did not come up within 30 s");
            }
            await Bun.sleep(100);
          }
          pagesOpened += 1;
          if (isolated === null) {
            isolated = await page.evaluate(() => globalThis.crossOriginIsolated === true, null);
            console.log(`SAFARI page: crossOriginIsolated=${isolated}`);
          }
        },
        showTab: async (id) => {
          await page.locator(`#tab-${id}`).click();
          const deadline = Date.now() + 5_000;
          while (!(await page.locator(`#pane-${id}`).isVisible())) {
            if (Date.now() > deadline) {
              throw new Error(`the ${id} pane did not show after its tab was clicked`);
            }
            await Bun.sleep(50);
          }
        },
        checks,
        annotate: (type, description) => annotations.push({ type, description }),
        notRun: (row, leg, reason) => {
          console.log(`NOT_RUN ${row} ${leg}: ${reason}`);
          notRun.push(`${row} ${leg}: ${reason}`);
        },
      },
      { only, blockMs },
    );
    record.pagesOpened = pagesOpened;
    record.crossOriginIsolated = isolated;
    const result = held.result;
    if (result === null) {
      throw new Error("the session answered no result");
    }
    verdict =
      result.outcome === "not-measured"
        ? `NOT MEASURED (contended: loadavg ${result.worstLoad.toFixed(2)} over ${result.cores} cores)`
        : result.outcome === "partial"
          ? `PARTIAL (stopped after ${result.stoppedAfter}, PEMU_PACED_ONLY; every gate of the workloads run held)`
          : "PASS";
  } catch (error) {
    failure = error instanceof Error ? error.message : String(error);
    verdict = error instanceof GateFailure ? `FAIL: ${failure}` : `ERROR: ${failure}`;
  } finally {
    if (session !== null) {
      await session.close().catch((error: unknown) => console.error(`SAFARI: closing the session: ${String(error)}`));
    }
    for (const child of children.reverse()) {
      child.kill();
      await child.exited;
    }
  }

  const safari = String((record.safari as { browserVersion?: unknown } | undefined)?.browserVersion ?? "?");
  const macos = (record.macos as { version: string }).version;
  record.finishedUtc = new Date().toISOString();
  record.hostMachine = { ...(record.hostMachine as object), loadavgAtEnd: loadavg() };
  const result = held.result;
  record.result = result;
  record.audioRendered = result?.paced.F4 ? result.paced.F4.quanta > 0 : null;
  record.verdict = verdict;
  record.summary = summaryLine(verdict, safari, macos, result);
  const path = recordPath(root, localDate(started), only);
  writeFileSync(path, `${JSON.stringify(record, null, 2)}\n`);
  console.log(record.summary);
  console.log(`SAFARI record: ${path}`);
  return verdict.startsWith("FAIL") || verdict.startsWith("ERROR") ? 1 : 0;
}

process.exit(await main());
