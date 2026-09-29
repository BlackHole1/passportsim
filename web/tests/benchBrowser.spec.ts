// The browser half of `cargo xtask bench-browser`: it measures, and `xtask` judges. One record per
// Playwright project, `bench-browser-<project>.json`, for four checks per engine per host:
//
// 1. workload K: our engine's fw-Og kernel, median of 7 runs, at least 0.90 x the spike's `blockx`
//    median measured in the same job on the same host;
// 2. F5 and F6 busy MIPS at least the browser floors (`xtask/src/bench/browser.rs` `BROWSER_FLOORS`);
// 3. F3 paced at 1x: the machine's own host cost at most 7 % of a core (judged in Rust from this
//    run's counted instructions, S and c) with idle cost c at most 0.05, and the whole browser's
//    paced share recorded against this host's band, not gated;
// 4. `xtask bench --check-model` for the browser F3 rows.
//
// This spec asserts only what is exact on any host (the kernel's checksum and instruction count,
// the windows each suite ran, the virtual time the paced page covered). The metrics come from the
// same `Metrics::from_phases` the native suites use. The xtask builds everything in the same job
// and hands it over in `PEMU_BENCH_BROWSER_PARAMS`; without that variable the test skips.
//
// JavaScriptCore is measured in Playwright WebKit: Safari cannot be driven headless here without
// enabling its remote automation.
//
// The paced leg runs first, on a fresh product page with nothing else in the browser, since its
// figure is the whole browser's CPU. The compute legs follow on a page that loads only the
// measuring Worker (`benchBrowser/worker.js`).

import { expect, test } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { arch, cpus, platform } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { webkit, type Page } from "@playwright/test";
import { browserGaps, call, openPage, waitForLine, waitForStatus } from "./harness";
import { pebundle, readOfficialFiles } from "./demoBundle";
import { findCore } from "./preconditions";
import { browserProcesses, cpuOf, holdFullSpeed, hostLoad, loadKind, processTable, type HighQos, type Proc } from "./processCpu";
import { serveDir } from "./staticServer";

const HERE = dirname(fileURLToPath(import.meta.url));

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

type Button = "up" | "down" | "ok";

/** One F-suite as `xtask/src/bench.rs` `SUITES` defines it, handed over by the xtask. */
interface Scenario {
  readonly bootTo: string;
  readonly bootBudgetMs: number;
  readonly setup: readonly (readonly [number, Button])[];
  readonly setupMs: number;
  readonly clicks: readonly (readonly [number, Button])[];
  readonly bodyMs: number;
}

/** What `cargo xtask bench-browser` hands this spec (`xtask/src/bench/browser.rs` `params`). */
interface Params {
  readonly recordDir: string;
  readonly spikeWasm: string;
  readonly kbenchWasm: string;
  readonly kernel: string;
  readonly k: {
    readonly iters: number;
    readonly slice: number;
    readonly maxBlockInsns: number;
    readonly repeat: number;
    readonly checksum: number;
    /** The spike's retired count; ours is one lower (`pemu_rv32::kbench::EXIT_ECALL`). */
    readonly insns: number;
  };
  readonly suites: readonly { readonly id: string; readonly scenario: Scenario; readonly repeat: number }[];
  readonly windowMs: number;
  readonly calibrationMs: number;
  readonly bootSlackWindows: number;
  readonly clickMs: number;
  readonly paced: {
    /** Console line the page must have printed before the paced minute starts. */
    readonly bootTo: string;
    /** Wall ms the page runs after that line before the reading starts. */
    readonly settleMs: number;
    readonly ms: number;
    /** Virtual ms per slice of the emulator-alone diagnostic (`benchBrowser/worker.js` `runPaced`). */
    readonly bareSliceMs: number;
    /**
     * The emulator-alone sweep: the same firmware, wall minute and instructions every time. `sliceMs`
     * sets how often `pemu_run` is called, `spin` whether the thread keeps the core between calls.
     */
    readonly bareLegs: readonly { readonly id: string; readonly sliceMs: number; readonly spin: boolean }[];
  };
}

function params(): Params | null {
  const text = process.env.PEMU_BENCH_BROWSER_PARAMS;
  return text === undefined || text === "" ? null : (JSON.parse(text) as Params);
}

function sysctl(name: string): string {
  const out = spawnSync("sysctl", ["-n", name], { encoding: "utf8" });
  return out.status === 0 ? out.stdout.trim() : "unknown";
}

/**
 * The CPU model of the host fingerprint, by the mechanism `xtask/src/bench.rs` `cpu_model` uses, so
 * `xtask` can refuse a record from another host: `sysctl`'s brand string on macOS, the inherited
 * `PROCESSOR_IDENTIFIER` on Windows.
 */
function cpuModel(): string {
  return platform() === "darwin" ? sysctl("machdep.cpu.brand_string") : (process.env.PROCESSOR_IDENTIFIER ?? "unknown");
}

function load(): number[] {
  return hostLoad();
}

/** The Playwright WebKit install whose XPC services belong to the browser (`processCpu.ts`). */
function webkitInstall(): string {
  const exe = webkit.executablePath();
  const at = exe.indexOf("/webkit-");
  const end = at < 0 ? -1 : exe.indexOf("/", at + 1);
  return end < 0 ? dirname(exe) : exe.slice(0, end);
}

/**
 * Runs one job in a fresh measuring Worker of `page` and returns what it posted. `files` maps a job
 * field to a URL whose bytes the Worker receives, except `core`: the page compiles the core once
 * and posts the same `WebAssembly.Module` to every Worker, as the product does. Per-run compiles
 * left the first calibration window mostly unoptimized, and sharing raised F5's S by 10 to 19 %.
 */
async function inWorker(page: Page, job: Record<string, unknown>, files: Record<string, string>): Promise<{ value: unknown; resolutionMs: number }> {
  const answer = await page.evaluate(
    async ({ job, files }) => {
      const loaded: Record<string, ArrayBuffer | WebAssembly.Module> = {};
      const transfer: ArrayBuffer[] = [];
      const page = globalThis as unknown as { benchModules?: Map<string, WebAssembly.Module> };
      page.benchModules ??= new Map();
      for (const [key, url] of Object.entries(files)) {
        const cached = key === "core" ? page.benchModules.get(url) : undefined;
        if (cached !== undefined) {
          loaded[key] = cached;
          continue;
        }
        const response = await fetch(url);
        if (!response.ok) {
          throw new Error(`${url}: ${response.status}`);
        }
        const bytes = await response.arrayBuffer();
        if (key === "core") {
          const module = await WebAssembly.compile(bytes);
          page.benchModules.set(url, module);
          loaded[key] = module;
        } else {
          loaded[key] = bytes;
          transfer.push(bytes);
        }
      }
      const worker = new Worker("worker.js");
      try {
        return await new Promise<{ ok: boolean; value?: unknown; resolutionMs?: number; error?: string }>((resolve, reject) => {
          worker.onmessage = (event) => resolve(event.data);
          worker.onerror = (event) => reject(new Error(event.message));
          worker.postMessage({ ...job, ...loaded }, transfer);
        });
      } finally {
        worker.terminate();
      }
    },
    { job, files },
  );
  if (!answer.ok) {
    throw new Error(`the measuring Worker failed: ${answer.error}`);
  }
  return { value: answer.value, resolutionMs: answer.resolutionMs ?? Number.NaN };
}

function reading(engine: "chromium" | "webkit", install: string): { table: Proc[]; pids: Set<number>; shared: string | null } {
  const table = processTable();
  const found = browserProcesses(table, process.pid, engine, install);
  return { table, pids: found.pids, shared: found.shared };
}

test.describe("browser perf measurement for `cargo xtask bench-browser`", () => {
  // K is 14 kernel runs, each suite a handful of runs, and the paced leg a wall minute.
  test.setTimeout(30 * 60_000);

  test("K, F-suite windows and paced host CPU, recorded for xtask", { tag: "@wasm-speed" }, async ({ page, browserName, browser, channel }) => {
    const p = params();
    test.skip(
      p === null,
      "runs from `cargo xtask bench-browser`, which builds the kernel, the spike and both wasm modules in the same job and passes them in PEMU_BENCH_BROWSER_PARAMS",
    );
    if (p === null) return;
    const engine = browserName === "chromium" ? "chromium" : "webkit";
    const core = findCore(process.env);
    if (!("path" in core)) {
      throw new Error(`bench-browser passed no wasm core: ${core.skip}`);
    }
    const official = readOfficialFiles();
    if (!("files" in official)) {
      throw new Error(`bench-browser passed no \`official\` build: ${"skip" in official ? official.skip : official.mismatch}`);
    }

    // The project names the record: the Chromium and Windows Chrome rows are the same engine.
    const project = test.info().project.name;
    const record: Record<string, unknown> = {
      engine,
      project,
      channel: channel ?? null,
      browserVersion: browser.version(),
      host: {
        os: platform(),
        arch: arch(),
        cpu: cpuModel(),
        cpus: cpus().length,
      },
      loadKind: loadKind(),
      loadavgAtStart: load(),
    };

    // 3. F3 host CPU paced at 1x, on the product page.
    await openPage(page);
    expect(await waitForStatus(page, 60_000), "the demo boots").toMatchObject({ ok: true });
    await waitForLine(page, new RegExp(p.paced.bootTo.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")), 60_000);
    await page.waitForTimeout(p.paced.settleMs);
    const install = engine === "webkit" ? webkitInstall() : "";
    // Windows: no EcoQoS for the browser (`processCpu.ts` `holdFullSpeed`); nothing elsewhere.
    const highQos: Record<string, HighQos> = {};
    const fullSpeed = (label: string) => {
      const held = holdFullSpeed(reading(engine, install).pids);
      if (held !== null) {
        highQos[label] = held;
        console.log(`RECORDED bench-browser ${engine} power throttling off before ${label}: ${held.processes} processes${held.failed.length > 0 ? `, refused for ${JSON.stringify(held.failed)}` : ""}`);
      }
    };
    fullSpeed("paced");
    const vtBefore = await vtUs(page);
    // Nothing talks to the page during the reading: every call would be CPU the page did not spend.
    const product = await cpuOver(engine, install, () => page.waitForTimeout(p.paced.ms));
    const vtAfter = await vtUs(page);
    record.paced = { ...product, virtualMs: (vtAfter - vtBefore) / 1_000 };
    console.log(
      `RECORDED bench-browser ${engine} paced: ${((product.cpuS / (product.wallMs / 1_000)) * 100).toFixed(2)} % of a core over ${(product.wallMs / 1_000).toFixed(1)} s wall, ${((vtAfter - vtBefore) / 1e6).toFixed(1)} s virtual, ${product.processes} processes, loadavg ${product.loadavgAtStart.join(" ")}`,
    );
    for (const proc of product.commands) {
      console.log(`   ${proc.pid} +${proc.gainedS.toFixed(2)} s ${proc.command}`);
    }
    expect(product.vanished, "no browser process ended during the reading, so none took its CPU with it").toBe(0);

    // 3, attribution: the same page again, one thing changed at a time. Only the gate above is judged.
    // The tab strip opens on Console, so the gate ran with Perf closed, and opening it measures the panel.
    const legs: Record<string, unknown> = {};
    const leg = async (id: string, prepare: () => Promise<unknown>) => {
      await prepare();
      await page.waitForTimeout(p.paced.settleMs);
      const before = await vtUs(page);
      const reading = await cpuOver(engine, install, () => page.waitForTimeout(p.paced.ms));
      const after = await vtUs(page);
      const value = { ...reading, virtualMs: (after - before) / 1_000 };
      legs[id] = value;
      console.log(
        `RECORDED bench-browser ${engine} paced ${id}: ${((value.cpuS / (value.wallMs / 1_000)) * 100).toFixed(2)} % of a core, rate ${(value.virtualMs / value.wallMs).toFixed(3)}, loadavg ${value.loadavgAtStart.join(" ")}`,
      );
      for (const proc of value.commands) {
        if (proc.gainedS >= 0.05) console.log(`   ${proc.pid} +${proc.gainedS.toFixed(2)} s ${proc.command}`);
      }
    };
    const showScreen = (hidden: boolean) =>
      page.evaluate((hide: boolean) => {
        const screen = document.querySelector(".skin-screen");
        if (!(screen instanceof HTMLElement)) {
          throw new Error("the skin has no `.skin-screen`");
        }
        screen.style.display = hide ? "none" : "";
      }, hidden);
    // The Perf panel open: the one panel that repaints from every `stats` message.
    await leg("perfOpen", () => page.click("#tab-perf"));
    // Nothing presented: the canvas out of the layout, so the Worker's draws reach no compositor.
    await leg("noDisplay", async () => {
      await page.click("#tab-console");
      await showScreen(true);
    });
    // Nothing repainted: `main.ts` calls the global `requestAnimationFrame`, so replacing it stops the
    // page's own painting while the Worker and canvas stay as they are.
    await leg("noRepaint", async () => {
      await showScreen(false);
      await page.evaluate(() => {
        (globalThis as { requestAnimationFrame: (cb: FrameRequestCallback) => number }).requestAnimationFrame = () => 0;
      });
    });
    // Paused with the page's own control, the floor: shell, Worker loop and browser with no guest
    // running. Last, because it stops the clock.
    await leg("paused", () => page.click('[data-action="pause"]'));
    record.pacedLegs = legs;
    await page.close();

    // 1 and 2: the compute legs, in a Worker of a page that loads nothing else.
    const server = await serveDir(join(HERE, "benchBrowser"), true, {
      "index.html": "<!doctype html><meta charset=utf-8><title>bench-browser</title>",
      "spike.wasm": readFileSync(p.spikeWasm),
      "kbench.wasm": readFileSync(p.kbenchWasm),
      "kernel.bin": readFileSync(p.kernel),
      "core.wasm": readFileSync(core.path),
      "official.pebundle": pebundle(official.files),
    });
    try {
      const bench = await browser.newPage();
      await bench.goto(`${server.url}index.html`);
      fullSpeed("compute");
      expect(await bench.evaluate(() => crossOriginIsolated), "the measuring page is cross-origin isolated").toBe(true);

      // The emulator alone at 1x in this empty page: how much of the product's paced figure is the
      // machine. `pacedBare` stays first and unchanged (8 ms slices, nothing counted) for comparability,
      // then the sweep at several call rates and once without releasing the core, each reading
      // `pemu_last_stop` after every slice to turn a duration into a count.
      const bareLeg = async (id: string, sliceMs: number, counted: boolean, spin = false) => {
        let run: { value: unknown; resolutionMs: number } | null = null;
        const reading = await cpuOver(engine, install, async () => {
          run = await inWorker(
            bench,
            { op: "paced", bootTo: p.paced.bootTo, ms: p.paced.ms, sliceMs, counted, spin },
            { core: "core.wasm", bundle: "official.pebundle" },
          );
        });
        const worker = (run as { value: unknown } | null)?.value ?? null;
        console.log(
          `RECORDED bench-browser ${engine} ${id}: ${((reading.cpuS / (reading.wallMs / 1_000)) * 100).toFixed(2)} % of a core, worker ${JSON.stringify(worker)}`,
        );
        return { ...reading, worker };
      };
      record.pacedBare = await bareLeg("paced, emulator alone", p.paced.bareSliceMs, false);
      const sweep: Record<string, unknown> = {};
      for (const spec of p.paced.bareLegs) {
        sweep[spec.id] = await bareLeg(`paced, emulator alone, ${spec.id}`, spec.sliceMs, true, spec.spin);
      }
      record.pacedBareSweep = sweep;

      // 1. Workload K.
      const k = await inWorker(
        bench,
        { op: "k", iters: p.k.iters, slice: p.k.slice, maxBlockInsns: p.k.maxBlockInsns, repeat: p.k.repeat },
        { spike: "spike.wasm", ours: "kbench.wasm", kernel: "kernel.bin" },
      );
      const runs = k.value as { spike: { insns: number; secs: number; checksum: number }[]; ours: { insns: number; secs: number; checksum: number }[] };
      for (const run of runs.spike) {
        expect(run.checksum, "the spike's fw-Og checksum").toBe(p.k.checksum);
        expect(run.insns, "the spike's fw-Og instruction count").toBe(p.k.insns);
      }
      for (const run of runs.ours) {
        expect(run.checksum, "our fw-Og checksum").toBe(p.k.checksum);
        expect(run.insns, "our fw-Og instruction count: the spike's less the exit ecall").toBe(p.k.insns - 1);
      }
      record.k = { ...runs, resolutionMs: k.resolutionMs, loadavgAtEnd: load() };
      const median = (xs: number[]) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)] ?? 0;
      const mips = (list: { insns: number; secs: number }[]) => median(list.map((r) => r.insns / r.secs / 1e6));
      console.log(`RECORDED bench-browser ${engine} K fw-Og: ours ${mips(runs.ours).toFixed(1)}, spike blockx ${mips(runs.spike).toFixed(1)} Minsn/s (medians of ${p.k.repeat})`);

      // 2. The F-suites, each run `repeat` times in a fresh Worker.
      const suites: Record<string, unknown[]> = {};
      for (const suite of p.suites) {
        const list: unknown[] = [];
        for (let r = 0; r < suite.repeat; r += 1) {
          const loadAt = load();
          const run = await inWorker(
            bench,
            {
              op: "suite",
              scenario: suite.scenario,
              windowMs: p.windowMs,
              calibrationMs: p.calibrationMs,
              bootSlackWindows: p.bootSlackWindows,
              clickMs: p.clickMs,
            },
            { core: "core.wasm", bundle: "official.pebundle" },
          );
          const value = run.value as { body: unknown[] };
          expect(value.body.length, `${suite.id}: the body ran ${suite.scenario.bodyMs} ms in ${p.windowMs} ms windows`).toBe(
            Math.ceil(suite.scenario.bodyMs / p.windowMs),
          );
          list.push({ ...value, resolutionMs: run.resolutionMs, loadavg: loadAt });
        }
        suites[suite.id] = list;
        console.log(`RAN bench-browser ${engine} ${suite.id}: ${suite.repeat} runs`);
      }
      record.suites = suites;
      await bench.close();
    } finally {
      await server.close();
    }
    record.loadavgAtEnd = load();
    if (Object.keys(highQos).length > 0) {
      record.powerThrottlingOff = highQos;
    }
    const path = join(p.recordDir, `bench-browser-${project}.json`);
    writeFileSync(path, JSON.stringify(record));
    console.log(`RECORDED bench-browser ${engine} record: ${path}`);
  });
});

async function cpuOver(engine: "chromium" | "webkit", install: string, body: () => Promise<unknown>) {
  const before = reading(engine, install);
  const loadavgAtStart = load();
  const at = performance.now();
  await body();
  const after = reading(engine, install);
  // From the end of one reading to the end of the next: starting the reader takes about a second on
  // Windows.
  const wallMs = performance.now() - at;
  // A process born during the reading brings all of its CPU; one that ended took its CPU with it.
  const kept = new Set([...after.pids].filter((pid) => before.pids.has(pid)));
  const cpuS = cpuOf(after.table, after.pids) - cpuOf(before.table, kept);
  return {
    wallMs,
    cpuS,
    processes: after.pids.size,
    born: [...after.pids].filter((pid) => !before.pids.has(pid)).length,
    vanished: [...before.pids].filter((pid) => !after.pids.has(pid)).length,
    shared: before.shared ?? after.shared,
    loadavgAtStart,
    loadavgAtEnd: load(),
    commands: after.table
      .filter((proc) => after.pids.has(proc.pid))
      .map((proc) => ({
        pid: proc.pid,
        gainedS: proc.cpuS - (before.table.find((b) => b.pid === proc.pid)?.cpuS ?? 0),
        command: proc.command.slice(0, 160),
      })),
  };
}

/** The instance's virtual time in microseconds, from `status`. */
async function vtUs(page: Page): Promise<number> {
  const status = await call(page, "status", {});
  if (!status.ok) {
    throw new Error(`status: ${status.error}`);
  }
  const rows = (status.json as { instances?: { vt_us?: number }[] }).instances;
  const vt = rows?.[0]?.vt_us;
  expect(vt, "`status` reports the instance's virtual time").not.toBeUndefined();
  return Number(vt);
}
