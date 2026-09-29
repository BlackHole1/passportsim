// The paced browser session shared by `m9b.spec.ts` (Playwright) and `safari/pacedSafari.ts` (real
// Safari over WebDriver). One paced session per workload, driven the way an agent drives it:
//
// | Figure | Where it comes from |
// |---|---|
// | real-time factor at least 1.0 on F4 to F6 | the machine's own `vt_us` against the wall time the body took |
// | zero re-anchors | the Perf tab, which shows the pacing loop's own counter |
// | worst 100 ms virtual window after boot at most 60 ms (F4, F5 card entry) | `run --for 100ms` windows on a paused machine, as `xtask bench` cuts them |
// | audio ring underruns 0 over 60 s (fake media) | the playback worklet's own counter on the Audio card |
//
// Everything runs paced at rate 1 except the worst-window pass. Menu keys go through registry
// `input --action press` and `release`, so the wall-time hold becomes the virtual-time hold the
// firmware debounces. F6 runs 60 s with OK pressed every {@link TONE_MS} to keep the codec
// producing.
//
// A busy host can make all three host budgets lie: above one runnable thread per core the session
// records `contended` and prints NOT MEASURED. The guest-side ledger is asserted either way.

import { arch, cpus, loadavg, platform } from "node:os";
import type { AudioTraceEntry } from "../src/audio/trace";
import { hostLoad, loadKind } from "./processCpu";
import { TRACE_LOG } from "../src/audio/trace";
import type { LoopIteration } from "../src/worker/loopTrace";

export interface SessionLocator {
  textContent(): Promise<string | null>;
  getAttribute(name: string): Promise<string | null>;
  isVisible(): Promise<boolean>;
  isEnabled(): Promise<boolean>;
  click(): Promise<void>;
}

export interface SessionPage {
  /** Runs `fn(arg)` in the page and answers its awaited, JSON-shaped result. */
  evaluate<R, A>(fn: (arg: A) => R | Promise<R>, arg: A): Promise<R>;
  waitForTimeout(ms: number): Promise<void>;
  locator(selector: string): SessionLocator;
}

export interface SessionChecks {
  equal(actual: unknown, expected: unknown, message: string): void;
  atLeast(actual: number, floor: number, message: string): void;
  atMost(actual: number, ceiling: number, message: string): void;
  greaterThan(actual: number, floor: number, message: string): void;
}

export interface SessionHost {
  readonly engine: string;
  openPage(): Promise<void>;
  showTab(id: string): Promise<void>;
  readonly checks: SessionChecks;
  annotate(type: string, description: string): void;
  notRun(row: string, leg: string, reason: string): void;
  /** Handed the result as soon as it exists, so a driver keeps the figures measured before a gate threw. */
  onResult?(result: SessionResult): void;
}

/** Knobs of a session run by hand; no tier sets any of them. */
export interface SessionOptions {
  readonly only: string;
  readonly blockMs: number;
}

export interface SessionResult {
  host: string;
  cores: number;
  /** The worst one-minute load average any body saw, the figure the contention rule reads. */
  worstLoad: number;
  contended: boolean;
  paced: Record<string, PacedFigures>;
  worstWindows: Record<string, { hostMs: number; virtualMs: number; windows: number; budgetMs: number }>;
  stoppedAfter: string | null;
  outcome: "measured" | "not-measured" | "partial";
}

export interface PacedFigures {
  realTimeFactor: number;
  virtualMs: number;
  hostMs: number;
  reanchors: number;
  underruns: number;
  quanta: number;
  silent: number;
  loudest: number;
  firstUnderrunMs: number | null;
  edges: number;
}

export const OFFICIAL_MENU = "main: 就绪:Display=1 Button=1 Audio=1 Battery=1";

/** Wall time the page gets for something the guest has to do; it paces at rate 1. */
export const GUEST_MS = 40_000;

const WINDOW_MS = 100;

/** The browser budget for the worst 100 ms virtual window. */
export const WORST_WINDOW_BUDGET_MS = 60;

export const UNDERRUN_SPAN_MS = 60_000;

/** How often F6 presses OK again; the demo's tone is about one guest second long. */
const TONE_MS = 1_200;

const HOLD_MS = 80;

/** Wall time a click gets to land before the next; two keys back to back are one key to the firmware. */
const SETTLE_MS = 220;

const CLICK_MS = 400;

/**
 * What the pacing loop's own grain can leave behind over one body: one slice
 * (`pacing.ts` `MAX_SLICE_WALL_MS`) plus one timer clamp. At `Wall` pacing virtual time never runs
 * ahead of wall time, so "real-time factor at least 1.0" means falling behind by no more than this.
 */
const PACING_SLACK_MS = 8 + 16;

/**
 * Silent render quanta F6's 60 s body may contain: one report period of the worklet's counters
 * (`worklet.ts` `REPORT_EVERY_QUANTA`, about 170 ms at 48 kHz), the grain the page reads them at.
 */
const SILENT_QUANTA_ALLOWED = 64;

/**
 * The demo's tone (`main/demo_audio.c`: a 1 kHz square at +-6000 of 32768) on the card's dBFS
 * meter, less a tenth for resampling from the guest's 16 kHz.
 */
const TONE_METER = ((20 * Math.log10(6_000 / 32_768) + 96) / 96) * 0.9;

type SkinKey = "up" | "ok" | "down";

/** The host fingerprint every recorded figure is read against. */
export function fingerprint(engine: string): string {
  // Windows has no load average (`processCpu.ts` `hostLoad`): it says how many processors were busy.
  const kind = loadKind() === "loadavg" ? "loadavg" : "busy";
  const load = hostLoad()
    .map((n) => n.toFixed(2))
    .join(" ");
  return `engine=${engine} host=${platform()}-${arch()} cpus=${cpus().length} ${kind}=[${load}]`;
}

/**
 * Whether this host can make an absolute host number lie: more than one runnable thread per core.
 * The one-minute load average lags, so it is re-read while each body runs and the worst counts.
 */
export class Contention {
  private worst = loadavg()[0] ?? 0;
  readonly cores = cpus().length;

  sample(): void {
    this.worst = Math.max(this.worst, loadavg()[0] ?? 0);
  }

  get load(): number {
    return this.worst;
  }

  get contended(): boolean {
    return this.worst > this.cores;
  }

  readonly recordedOnly: string | null = recordedOnlyReason();

  /** The host budgets are recorded, not judged: a contended host, or one that is not gated. */
  get ungated(): boolean {
    return this.contended || this.recordedOnly !== null;
  }
}

/**
 * Why the host budgets are recorded and not gated on `platform`, or `null` where they gate (macOS).
 * Windows is recorded only, and has no load average (`os.loadavg()` is `[0, 0, 0]`), so the
 * contention rule could not tell a busy Windows host from a quiet one.
 */
export function recordedOnlyReason(platform: NodeJS.Platform = process.platform): string | null {
  return platform === "win32"
    ? "recorded, not gated: the host budgets gate on macOS only, and Windows has no load average for the contention rule to read"
    : null;
}

/** One `passportEmu.call` from the page, with the error text instead of a throw. */
export async function sessionCall(
  page: SessionPage,
  name: string,
  args: unknown,
): Promise<{ ok: true; json: unknown } | { ok: false; error: string }> {
  return page.evaluate(
    async ({ name, args }: { name: string; args: unknown }) => {
      const api = (globalThis as unknown as { passportEmu: { call(n: string, a: unknown): Promise<{ json: unknown }> } }).passportEmu;
      try {
        const out = await Promise.race([
          api.call(name, args),
          new Promise<never>((_, reject) => setTimeout(() => reject(new Error("no answer within 15 s")), 15_000)),
        ]);
        return { ok: true as const, json: out.json };
      } catch (error) {
        const body = (error as { body?: { code?: string; message?: string } }).body;
        return { ok: false as const, error: body ? `${body.code}: ${body.message}` : String(error) };
      }
    },
    { name, args },
  );
}

/** `status`, waiting out the one answer a machine that is still booting gives. */
async function waitForStatus(page: SessionPage, timeoutMs: number): Promise<{ ok: boolean }> {
  let status = await sessionCall(page, "status", {});
  const deadline = Date.now() + timeoutMs;
  while (!status.ok && status.error.startsWith("E_STATE: no machine is booted") && Date.now() < deadline) {
    await page.waitForTimeout(100);
    status = await sessionCall(page, "status", {});
  }
  if (!status.ok) {
    throw new Error(`\`status\` did not answer: ${status.error}`);
  }
  return status;
}

async function until(page: SessionPage, probe: () => Promise<boolean>, timeoutMs: number, message: string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!(await probe())) {
    if (Date.now() > deadline) {
      throw new Error(`${message} (waited ${timeoutMs} ms)`);
    }
    await page.waitForTimeout(100);
  }
}

async function must(page: SessionPage, name: string, args: unknown): Promise<Record<string, unknown>> {
  const answer = await sessionCall(page, name, args);
  if (!answer.ok) {
    throw new Error(`${name} ${JSON.stringify(args)}: ${answer.error}`);
  }
  return (answer.json ?? {}) as Record<string, unknown>;
}

/** The instance's virtual time in microseconds, from `status`. */
async function instanceVtUs(page: SessionPage, host: SessionHost): Promise<number> {
  const rows = (await must(page, "status", {})).instances as { vt_us?: number }[] | undefined;
  const first = rows?.[0]?.vt_us;
  host.checks.equal(first === undefined, false, "`status` reports the instance's virtual time");
  return Number(first ?? 0);
}

async function edge(page: SessionPage, key: SkinKey, down: boolean): Promise<void> {
  await must(page, "input", { button: key, action: down ? "press" : "release" });
}

/** One menu click: the two registry edges, with the hold spent in wall time (guest time at rate 1). */
async function click(page: SessionPage, key: SkinKey): Promise<void> {
  await edge(page, key, true);
  await page.waitForTimeout(HOLD_MS);
  await edge(page, key, false);
  await page.waitForTimeout(SETTLE_MS);
}

/**
 * The playback counters the Audio card shows, or zeroes before the worklet's first report. The
 * silent count matters too: an underrun is only counted where sound turned into silence, so a page
 * that never played reports zero underruns.
 */
async function playback(page: SessionPage): Promise<{ quanta: number; underruns: number; silent: number }> {
  const text = (await page.locator('.field:has(meter[aria-label="Output level"]) .readout').textContent()) ?? "";
  const found = /(\d+) quanta, (\d+) underrun\(s\), (\d+) silent/.exec(text);
  return {
    quanta: Number(found?.[1] ?? 0),
    underruns: Number(found?.[2] ?? 0),
    silent: Number(found?.[3] ?? 0),
  };
}

/**
 * A pacing counter the Perf tab shows (`Perf.tsx`), found by its header's exact text. The tab keeps
 * 10 s of history, so it is read repeatedly during a body. A missing row throws rather than
 * reading as zero.
 */
async function perfRow(page: SessionPage, label: string): Promise<number> {
  const text = await page.evaluate((wanted: string) => {
    for (const row of Array.from(document.querySelectorAll("#pane-perf tr"))) {
      if (row.querySelector("th")?.textContent?.trim() === wanted) {
        return row.querySelector("td")?.textContent ?? null;
      }
    }
    return null;
  }, label);
  if (text === null) {
    throw new Error(`the Perf tab has no row \`${label}\``);
  }
  return Number(text);
}

async function pacedPage(page: SessionPage, host: SessionHost): Promise<void> {
  await host.openPage();
  await waitForStatus(page, GUEST_MS);
  const menu = new RegExp(OFFICIAL_MENU);
  await until(
    page,
    async () => menu.test((await page.locator("#pane-console").textContent()) ?? ""),
    GUEST_MS,
    `the console never printed ${OFFICIAL_MENU}`,
  );
  const pause = page.locator('button.transport[data-action="pause"]');
  await until(
    page,
    () => pause.isEnabled(),
    GUEST_MS,
    "the page runs its machine `Wall`-paced at rate 1 from the boot: Pause never became live",
  );
}

/** Stops the Worker's `Wall` loop as well as the clock, with the page's own Pause button. */
async function pausePage(page: SessionPage): Promise<void> {
  const pause = page.locator('button.transport[data-action="pause"]');
  await until(page, () => pause.isEnabled(), GUEST_MS, "the page is running, so Pause is live");
  await pause.click();
  const run = page.locator('button.transport[data-action="run"]');
  await until(page, () => run.isEnabled(), 5_000, "the page is paused: Run is live");
}

interface Paced {
  readonly virtualMs: number;
  /** Wall milliseconds the body took, measured inside the same two readings. */
  readonly hostMs: number;
  readonly underruns: number;
  readonly silent: number;
  readonly quanta: number;
  /** The most re-anchors the Perf tab reported at any sample during the body. */
  readonly reanchors: number;
  readonly edges: number;
  /** The loudest the page's own output meter read during the body, 0 to 1. */
  readonly loudest: number;
  readonly startEpochMs: number;
  readonly endEpochMs: number;
  readonly startUs: number;
  readonly endUs: number;
  /**
   * Milliseconds into the body at which the underrun count first rose, or `null`: early is the ring
   * still filling after boot, mid-body is the path failing to hold.
   */
  readonly firstUnderrunMs: number | null;
}

function record(host: SessionHost, id: string, what: string, value: string): void {
  console.log(`RECORDED ${id} ${host.engine} ${what}: ${value}`);
  host.annotate(`${id} ${what} (recorded)`, value);
}

/**
 * Runs the session on `page`. A failed gate throws through `host.checks`; a contended host records
 * its figures and judges none.
 */
export async function runPacedSession(page: SessionPage, host: SessionHost, options: SessionOptions): Promise<SessionResult> {
  const hostLine = fingerprint(host.engine);
  const load = new Contention();
  const blocks: number[] = [];
  console.log(`RECORDED host: ${hostLine}`);
  host.annotate("host (recorded)", hostLine);
  const result: SessionResult = {
    host: hostLine,
    cores: load.cores,
    worstLoad: load.load,
    contended: false,
    paced: {},
    worstWindows: {},
    stoppedAfter: null,
    outcome: "measured",
  };
  host.onResult?.(result);
  const settle = (outcome: SessionResult["outcome"]): SessionResult => {
    result.worstLoad = load.load;
    result.contended = load.contended;
    result.outcome = outcome;
    return result;
  };

  // F4 and F5: paced, for the real-time factor and the re-anchors.
  for (const workload of [
    { id: "F4", definition: "`official`: 40 menu clicks at 400 ms", clicks: 40, key: "down" as SkinKey, bodyMs: 16_000 },
    { id: "F5", definition: "`official`: Display demo for 10 s", clicks: 1, key: "ok" as SkinKey, bodyMs: 10_000 },
  ]) {
    await pacedPage(page, host);
    const measured = await pacedBody(page, host, load, async (sample) => {
      let spent = 0;
      for (let at = 0; at < workload.clicks; at += 1) {
        await click(page, workload.key);
        let wait = Math.min(CLICK_MS, workload.bodyMs - spent) - HOLD_MS - SETTLE_MS;
        if (options.blockMs > 0) {
          // The main-thread control: the page's own thread is busy for BLOCK_MS.
          blocks.push(Date.now());
          await page.evaluate((ms: number) => {
            const end = performance.now() + ms;
            while (performance.now() < end) {
            }
          }, options.blockMs);
          wait -= options.blockMs;
        }
        if (wait > 0) {
          await page.waitForTimeout(wait);
        }
        spent += CLICK_MS;
        await sample();
      }
      if (workload.bodyMs > spent) {
        await page.waitForTimeout(workload.bodyMs - spent);
      }
      return workload.clicks * 2;
    });
    host.checks.equal(
      measured.edges,
      workload.clicks * 2,
      `${workload.id}: one press and one release per menu click, journaled through the registry`,
    );
    await recordTrace(page, workload.id, host.engine, measured, blocks, options.blockMs);
    judge(host, workload.id, measured, load, result);
    if (options.only === workload.id) {
      console.log(`RAN ${workload.id} only (PEMU_PACED_ONLY): ${hostLine}`);
      result.stoppedAfter = workload.id;
      return settle(load.ungated ? "not-measured" : "partial");
    }
  }

  // F6: 60 s of audio, for the underrun count.
  await pacedPage(page, host);
  // DOWN, DOWN, OK reaches the Audio card and the second OK opens its codec at 16 kHz mono, which
  // takes about 1.8 s of virtual time (`xtask/src/bench.rs` F6 waits 4 s).
  await click(page, "down");
  await click(page, "down");
  await click(page, "ok");
  await page.waitForTimeout(500);
  await click(page, "ok");
  await page.waitForTimeout(3_000);
  const tones = Math.floor(UNDERRUN_SPAN_MS / TONE_MS);
  const f6 = await pacedBody(page, host, load, async (sample) => {
    for (let tone = 0; tone < tones; tone += 1) {
      await click(page, "ok");
      await page.waitForTimeout(TONE_MS - HOLD_MS - SETTLE_MS);
      await sample();
    }
    return tones * 2;
  });
  host.checks.equal(f6.edges, tones * 2, "F6: one press and one release per tone");
  host.checks.greaterThan(
    f6.quanta,
    UNDERRUN_SPAN_MS / 4,
    "F6: the page's own worklet rendered the minute, so its underrun count is about something",
  );
  judge(host, "F6", f6, load, result);

  // The worst 100 ms virtual window: F4 and F5 card entry.
  for (const workload of [
    { id: "F4", definition: "40 menu clicks at 400 ms", key: "down" as SkinKey, every: 4, windows: 160 },
    { id: "F5", definition: "Display card entry, then 10 s", key: "ok" as SkinKey, every: 0, windows: 100 },
  ]) {
    // Not `pacedPage`: the machine is paused before the boot line, since `run --until` waits on it.
    await host.openPage();
    await waitForStatus(page, GUEST_MS);
    await pausePage(page);
    const worst = await worstWindow(page, host, workload.key, workload.every, workload.windows);
    host.checks.equal(
      Math.round(worst.virtualMs),
      workload.windows * WINDOW_MS,
      `${workload.id}: every window advanced exactly ${WINDOW_MS} ms of virtual time`,
    );
    result.worstWindows[workload.id] = {
      hostMs: worst.hostMs,
      virtualMs: worst.virtualMs,
      windows: workload.windows,
      budgetMs: WORST_WINDOW_BUDGET_MS,
    };
    record(
      host,
      workload.id,
      "worst 100 ms window",
      `${worst.hostMs.toFixed(2)} ms host over ${workload.windows} windows (${workload.definition}), budget ${WORST_WINDOW_BUDGET_MS} ms, ${hostLine}`,
    );
    if (!load.ungated) {
      host.checks.atMost(
        worst.hostMs,
        WORST_WINDOW_BUDGET_MS,
        `${workload.id}: the worst 100 ms virtual window costs at most ${WORST_WINDOW_BUDGET_MS} ms of host time`,
      );
    }
  }

  // The contention verdict uses the worst load any body saw, not the load at the start: a build that
  // started halfway through the audio would otherwise skip the gates silently while the run passed.
  if (load.ungated) {
    host.notRun(
      "paced-session",
      "host budgets",
      load.contended
        ? `contended: loadavg reached ${load.load.toFixed(2)} over ${load.cores} cores, more than one runnable thread per core, so the real-time factor, the worst window and the underrun count would measure this host and not the emulator`
        : (load.recordedOnly ?? ""),
    );
    console.log(`NOT MEASURED: ${load.contended ? "contended host" : "recorded, not gated on this host"}`);
    return settle("not-measured");
  }
  console.log(
    `RAN paced-session: F4, F5 and F6 paced at rate 1 with no re-anchor, ${UNDERRUN_SPAN_MS / 1_000} s of audio and the worst window inside ${WORST_WINDOW_BUDGET_MS} ms (${hostLine})`,
  );
  return settle("measured");
}

/**
 * Runs `body` and measures it. Virtual time is `status` on both sides and wall time is read just
 * inside those readings, so both span the same interval and the `status` calls' cost is not
 * charged to the pacing loop.
 */
async function pacedBody(
  page: SessionPage,
  host: SessionHost,
  load: Contention,
  body: (sample: () => Promise<void>) => Promise<number>,
): Promise<Paced> {
  await host.showTab("perf");
  const before = await playback(page);
  const startUs = await instanceVtUs(page, host);
  const startMs = performance.now();
  const startEpochMs = Date.now();
  let reanchors = 0;
  // The Perf tab keeps 10 s of history, so a longer body is sampled while it runs.
  let loudest = 0;
  let firstUnderrunMs: number | null = null;
  const sample = async () => {
    load.sample();
    reanchors = Math.max(reanchors, await perfRow(page, "re-anchors over the history"));
    loudest = Math.max(loudest, Number(await page.locator('meter[aria-label="Output level"]').getAttribute("value")));
    if (firstUnderrunMs === null && (await playback(page)).underruns > before.underruns) {
      firstUnderrunMs = performance.now() - startMs;
    }
  };
  const edges = await body(sample);
  const hostMs = performance.now() - startMs;
  const endEpochMs = Date.now();
  const endUs = await instanceVtUs(page, host);
  await sample();
  const after = await playback(page);
  return {
    virtualMs: (endUs - startUs) / 1_000,
    hostMs,
    underruns: after.underruns - before.underruns,
    silent: after.silent - before.silent,
    quanta: after.quanta - before.quanta,
    reanchors,
    edges,
    loudest,
    firstUnderrunMs,
    startEpochMs,
    endEpochMs,
    startUs,
    endUs,
  };
}

function iterationLine(iteration: LoopIteration, originMs: number): string {
  const f = (ms: number) => ms.toFixed(1);
  const wait =
    iteration.waitResult === "skip"
      ? ""
      : ` wait=${f(iteration.waitMs)}/${f(iteration.askedMs)}(${iteration.waitResult})`;
  const handlers = iteration.handlers > 0 ? ` handlers=${f(iteration.handlerMs)}[${iteration.handled}]` : "";
  return `+${f(iteration.atMs - originMs)} ${iteration.kind} gap=${f(iteration.gapMs)} flush=${f(iteration.flushMs)}${wait} yield=${f(iteration.yieldMs)} run=${f(iteration.runMs)}/${(iteration.vtUs / 1_000).toFixed(2)}vt pump=${f(iteration.pumpMs)} present=${f(iteration.presentMs)} report=${f(iteration.reportMs)}${handlers}`;
}

function vtMs(ps: string): string {
  return (Number(BigInt(ps) / 1_000_000n) / 1_000).toFixed(3);
}

/**
 * Prints the audio path's trace since the page loaded (record headers, pushed bursts with their
 * zero counts, worklet state changes), stamped in host and virtual ms. A driver that could not set
 * the trace flag before load (WebDriver has no init script) kept no trace, and says so.
 */
async function recordTrace(
  page: SessionPage,
  id: string,
  engine: string,
  measured: Paced,
  blocks: readonly number[],
  blockMs: number,
): Promise<void> {
  const entries = (await page.evaluate(
    (log: string) => (globalThis as unknown as Record<string, unknown>)[log] ?? null,
    TRACE_LOG,
  )) as AudioTraceEntry[] | null;
  if (entries === null) {
    console.log(`AUDIO TRACE ${id} ${engine}: the page kept no trace`);
    return;
  }
  const at = (entry: AudioTraceEntry) =>
    entry.atMs === undefined ? "?" : `${(entry.atMs - measured.startEpochMs).toFixed(0)}ms`;
  console.log(
    `AUDIO TRACE ${id} ${engine}: ${entries.length} entries; body host 0..${(measured.endEpochMs - measured.startEpochMs).toFixed(0)}ms, body vt ${(measured.startUs / 1_000).toFixed(3)}..${(measured.endUs / 1_000).toFixed(3)}ms, underruns ${measured.underruns}`,
  );
  if (blocks.length > 0) {
    console.log(
      `AUDIO TRACE ${id} main-thread blocks of ${blockMs}ms at: ${blocks.map((ms) => `${(ms - measured.startEpochMs).toFixed(0)}ms`).join(" ")}`,
    );
  }
  // The margin the ring kept against a late producer, over the body's windows, in guest ms.
  const windows = entries.filter(
    (entry): entry is AudioTraceEntry & { src: "worklet"; fill: number } =>
      entry.src === "worklet" &&
      entry.kind === "level" &&
      entry.atMs !== undefined &&
      entry.atMs >= measured.startEpochMs &&
      entry.atMs <= measured.endEpochMs,
  );
  const lows = windows.map((entry) => entry.fill).sort((a, b) => a - b);
  if (lows.length > 0) {
    const pick = (q: number) => lows[Math.min(lows.length - 1, Math.floor(q * lows.length))] ?? 0;
    console.log(
      `AUDIO TRACE ${id} ring margin over ${lows.length} windows of the body, lowest fill in samples: min=${pick(0)} p10=${pick(0.1)} median=${pick(0.5)} max=${pick(1)}; the first 2 s: ${windows
        .slice(0, 12)
        .map((entry) => `${at(entry)}:${entry.fill}`)
        .join(" ")}`,
    );
  }
  for (const entry of entries) {
    if (entry.src === "pump" && entry.kind === "record") {
      console.log(`AUDIO TRACE ${id} t=${at(entry)} record vt=${vtMs(entry.vtPs)}ms first=${entry.first} fs=${entry.fs} ch=${entry.channels}`);
    } else if (entry.src === "pump" && entry.kind === "burst") {
      console.log(
        `AUDIO TRACE ${id} t=${at(entry)} burst ${entry.state} transport=${entry.transportFirst} ring=${entry.ringFirst} samples=${entry.samples} zeros=${entry.zeros} peak=${entry.peak} vt=${vtMs(entry.vtFirstPs)}..${vtMs(entry.vtEndPs)}ms fs=${entry.fs} ch=${entry.channels}`,
      );
    } else if (entry.src === "pump" && entry.kind === "stall") {
      console.log(
        `AUDIO TRACE ${id} t=${at(entry)} stall ${entry.hostMs}ms without a push, transport=${entry.transportFirst} buffered=${entry.buffered}`,
      );
    } else if (entry.src === "loop" && entry.kind === "window") {
      console.log(
        `AUDIO TRACE ${id} t=${at(entry)} loop window over a ${entry.stallMs}ms stall (${entry.yielder}), ${entry.iterations.length} iterations from the push before it:`,
      );
      for (const iteration of entry.iterations) {
        console.log(`AUDIO TRACE ${id}   ${iterationLine(iteration, entry.fromMs)}`);
      }
    } else if (entry.src === "loop" && entry.kind === "summary") {
      const terms = (which: Record<string, number>) =>
        Object.entries(which)
          .map(([term, ms]) => `${term.replace(/Ms$/, "")}=${ms.toFixed(1)}`)
          .join(" ");
      console.log(
        `AUDIO TRACE ${id} t=${at(entry)} loop summary (${entry.yielder}) ${entry.iterations} iterations (${entry.ran} ran, ${entry.ahead} ahead) over ${entry.spanMs.toFixed(0)}ms host, ${(entry.vtUs / 1_000).toFixed(1)}ms vt; late waits >5ms ${entry.lateWaits5}, >20ms ${entry.lateWaits20}, worst overshoot ${entry.worstOvershootMs.toFixed(1)}ms; sum ${terms(entry.sum)}; max ${terms(entry.max)}`,
      );
    } else if (entry.src === "worklet" && entry.kind === "level") {
      continue;
    } else if (entry.src === "worklet") {
      console.log(
        `AUDIO TRACE ${id} t=${at(entry)} worklet ${entry.kind} quantum=${entry.quantum} consumed=${entry.consumed} fill=${entry.fill} idle=${entry.idleQuanta} underruns=${entry.underruns}`,
      );
    }
  }
}

function judge(host: SessionHost, id: string, measured: Paced, load: Contention, result: SessionResult): void {
  const factor = measured.virtualMs / measured.hostMs;
  result.worstLoad = load.load;
  result.contended = load.contended;
  result.paced[id] = {
    realTimeFactor: factor,
    virtualMs: measured.virtualMs,
    hostMs: measured.hostMs,
    reanchors: measured.reanchors,
    underruns: measured.underruns,
    quanta: measured.quanta,
    silent: measured.silent,
    loudest: measured.loudest,
    firstUnderrunMs: measured.firstUnderrunMs,
    edges: measured.edges,
  };
  record(
    host,
    id,
    "real-time factor",
    `${factor.toFixed(4)} (${measured.virtualMs.toFixed(0)} ms virtual in ${measured.hostMs.toFixed(0)} ms host), ${measured.reanchors} re-anchors, loadavg ${load.load.toFixed(2)} over ${load.cores} cores`,
  );
  record(
    host,
    id,
    "audio underruns",
    `${measured.underruns} over ${measured.quanta} playback quanta, ${measured.silent} of them silent for want of samples, loudest meter reading ${measured.loudest.toFixed(4)}, first dropout ${measured.firstUnderrunMs === null ? "none" : `${measured.firstUnderrunMs.toFixed(0)} ms into the body`}`,
  );
  if (load.ungated) {
    console.log(`NOT MEASURED ${id}: ${load.contended ? "contended host" : "recorded, not gated on this host"}`);
    return;
  }
  host.checks.atLeast(
    measured.virtualMs,
    measured.hostMs - PACING_SLACK_MS,
    `${id}: the paced body held real time; it advanced ${measured.virtualMs.toFixed(0)} ms of virtual time in ${measured.hostMs.toFixed(0)} ms of host time`,
  );
  host.checks.equal(measured.reanchors, 0, `${id}: the pacing loop never re-anchored`);
  host.checks.equal(measured.underruns, 0, `${id}: the playback ring never ran dry over ${measured.quanta} render quanta`);
  // Zero underruns over silence would be zero by vacancy; F4 and F5 play nothing, so this is F6's alone.
  if (id === "F6") {
    // The tone on the page's own meter: a menu walk that landed on the wrong card plays nothing.
    host.checks.atLeast(
      measured.loudest,
      TONE_METER,
      `${id}: the guest's tone reached the page's output meter; the demo plays a 1 kHz square at 6000 of full scale`,
    );
    host.checks.atMost(
      measured.silent,
      SILENT_QUANTA_ALLOWED,
      `${id}: the worklet had samples for every one of its ${measured.quanta} render quanta`,
    );
  }
}

/**
 * The worst 100 ms virtual window of a body, cut as `xtask bench` cuts one: the machine paused and
 * the body run as N calls of `run --for 100ms`, each timed around the call.
 */
async function worstWindow(
  page: SessionPage,
  host: SessionHost,
  key: SkinKey,
  every: number,
  windows: number,
): Promise<{ hostMs: number; virtualMs: number }> {
  const matched = await must(page, "run", {
    until: `serial:/${OFFICIAL_MENU}/`,
    timeout: "2000ms",
  });
  host.checks.equal(matched.status, "matched", "the boot reached the settled menu before the windows start");
  let hostMs = 0;
  let virtualUs = 0;
  // Press and release are raw edges with no implicit run, journaled on a window boundary, so a click
  // is one 100 ms window. `every` 0 is one click at the start (F5's card entry); F4 clicks every four.
  for (let window = 0; window < windows; window += 1) {
    const clicking = every === 0 ? window === 0 : window % every === 0;
    if (clicking) {
      await edge(page, key, true);
    }
    const at = performance.now();
    virtualUs += Number((await must(page, "run", { for: `${WINDOW_MS}ms` })).elapsed_vt_us ?? 0);
    hostMs = Math.max(hostMs, performance.now() - at);
    if (clicking) {
      await edge(page, key, false);
    }
  }
  return { hostMs, virtualMs: virtualUs / 1_000 };
}
