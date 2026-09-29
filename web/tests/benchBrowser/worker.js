// The measuring Worker of `cargo xtask bench-browser`, loaded by `benchBrowser.spec.ts`. Plain JavaScript
// with no imports, so Chromium and WebKit run the same bytes untranspiled. One message per job:
//
// - `k`: workload K. The spike's `blockx` and our engine (`kbench_wasm.rs`) run the fw-Og kernel in
//   turn in this Worker, each timed around the run alone.
// - `suite`: one F-suite over the production core's raw ABI, cut into windows as
//   `xtask/src/bench.rs` cuts one natively: a calibration pass on a twin machine in 20 ms slices,
//   the boot in 100 ms windows to the console line, the setup, and the body, each window timed
//   around `pemu_run` with what `pemu_last_stop` reports.
// - `paced`: the emulator alone paced at 1x, beside the product's paced reading.
//
// Nothing here computes a metric: the windows go back to `xtask`, which uses the native
// `Metrics::from_phases`.

"use strict";

/** The spike's engine and workload indices (`spikes/rv32-interp/src/lib.rs`, `js/run-chrome.mjs`). */
const SPIKE_BLOCKX = 3;
const SPIKE_FW_OG = 3;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Smallest nonzero step of `performance.now()` seen here, in ms: the resolution every figure has. */
function timerResolution() {
  let best = Infinity;
  for (let i = 0; i < 200; i++) {
    const a = performance.now();
    let b = a;
    while (b === a) b = performance.now();
    best = Math.min(best, b - a);
  }
  return best;
}

// Workload K

function runK(msg) {
  const spike = new WebAssembly.Instance(new WebAssembly.Module(msg.spike), {}).exports;
  const ours = new WebAssembly.Instance(new WebAssembly.Module(msg.ours), {}).exports;
  const kernel = new Uint8Array(msg.kernel);
  const out = { spike: [], ours: [] };

  const runSpike = () => {
    const t0 = performance.now();
    const insns = spike.bench(SPIKE_BLOCKX, SPIKE_FW_OG, msg.iters, msg.slice);
    const secs = (performance.now() - t0) / 1000;
    out.spike.push({ insns, secs, checksum: spike.last_checksum() >>> 0 });
  };
  const runOurs = () => {
    const ptr = ours.kb_alloc(kernel.length);
    new Uint8Array(ours.memory.buffer, ptr, kernel.length).set(kernel);
    const loaded = ours.kb_load(ptr, kernel.length, msg.iters, msg.maxBlockInsns);
    ours.kb_free(ptr, kernel.length);
    if (loaded !== 0) {
      throw new Error("kb_load: " + error(ours));
    }
    const t0 = performance.now();
    const insns = ours.kb_run(msg.slice);
    const secs = (performance.now() - t0) / 1000;
    if (insns < 0) {
      throw new Error("kb_run: " + error(ours));
    }
    out.ours.push({
      insns,
      secs,
      checksum: ours.kb_checksum() >>> 0,
      slowStores: ours.kb_slow_stores(),
      translations: ours.kb_translations(),
    });
    ours.kb_unload();
  };
  // Alternating which side goes first, so neither always runs on a cooler or warmer core.
  for (let r = 0; r < msg.repeat; r++) {
    if (r % 2 === 0) {
      runSpike();
      runOurs();
    } else {
      runOurs();
      runSpike();
    }
  }
  return out;
}

function error(ours) {
  return decoder.decode(new Uint8Array(ours.memory.buffer, ours.kb_error(), ours.kb_error_len()).slice());
}

// The production core over its raw ABI

/**
 * The production core as the job carries it: the `WebAssembly.Module` the page compiled once, or
 * its bytes, which this Worker then compiles.
 */
function coreModule(core) {
  return core instanceof WebAssembly.Module ? core : new WebAssembly.Module(core);
}

class Core {
  constructor(module) {
    this.x = new WebAssembly.Instance(module, {}).exports;
  }

  withBytes(bytes, body) {
    const x = this.x;
    const ptr = bytes.length === 0 ? 0 : x.pemu_alloc(bytes.length);
    if (bytes.length !== 0 && ptr === 0) {
      throw new Error("pemu_alloc(" + bytes.length + ") failed");
    }
    new Uint8Array(x.memory.buffer).set(bytes, ptr);
    try {
      return body(ptr, bytes.length);
    } finally {
      if (bytes.length !== 0) x.pemu_free(ptr, bytes.length);
    }
  }

  result(res) {
    const x = this.x;
    const header = new DataView(x.memory.buffer, res, 12);
    const ptr = header.getUint32(0, true);
    const len = header.getUint32(4, true);
    const status = header.getUint32(8, true);
    const bytes = new Uint8Array(x.memory.buffer).slice(ptr, ptr + len);
    x.pemu_result_free(res);
    if (status !== 0) {
      throw new Error("status " + status + ": " + decoder.decode(bytes));
    }
    return bytes;
  }

  json(res) {
    return JSON.parse(decoder.decode(this.result(res)));
  }

  /** Builds a machine over a `.pebundle` (kind 1) with the default configuration, as the page does. */
  build(bundle) {
    const x = this.x;
    const builder = this.withBytes(encoder.encode("{}"), (ptr, len) => x.pemu_new(ptr, len));
    this.withBytes(bundle, (ptr, len) => this.result(x.pemu_load(builder, 1, ptr, len)));
    const built = this.result(x.pemu_build(builder));
    return new DataView(built.buffer).getUint32(0, true);
  }

  call(handle, request) {
    return this.json(this.withBytes(encoder.encode(JSON.stringify(request)), (ptr, len) => this.x.pemu_call(handle, ptr, len)));
  }

  /** Arms one USJ console matcher, or none when `line` is null (`xtask/src/bench.rs` `run_slices`). */
  marker(handle, line) {
    const matchers = line === null ? [] : [{ id: 0xf32, serial: { stream: "usj", contains: line } }];
    this.call(handle, { cmd: "@stops", args: { matchers } });
  }

  input(handle, entries) {
    this.json(this.withBytes(encoder.encode(JSON.stringify(entries)), (ptr, len) => this.x.pemu_input(handle, ptr, len)));
  }

  nowPs(handle) {
    return BigInt(this.x.pemu_now_ps(handle));
  }

  /**
   * Runs `count` slices of `slicePs`, timing each around `pemu_run` alone and stopping early on the
   * armed matcher or a deadlock, as `run_slices` does. Returns the windows and the last stop's name.
   */
  slices(handle, slicePs, count) {
    const out = [];
    let reason = "Until";
    for (let i = 0; i < count; i++) {
      const start = this.nowPs(handle);
      const t0 = performance.now();
      const code = this.x.pemu_run(handle, start + slicePs, -1n) >>> 0;
      const hostMs = performance.now() - t0;
      if (code === 0xffffffff) {
        throw new Error("pemu_run: handle " + handle + " names no machine");
      }
      const stop = this.json(this.x.pemu_last_stop(handle));
      out.push({
        busy_insns: Number(BigInt(stop.insns) - BigInt(stop.ff_insns)),
        idle_ps: Number(stop.idle_ps),
        span_ps: Number(BigInt(stop.vt_ps) - start),
        host_ns: Math.round(hostMs * 1e6),
      });
      reason = stop.reason;
      if (reason === "Until") continue;
      if (reason === "Matcher" || reason === "Deadlock") break;
      throw new Error("the run stopped for " + stop.detail + " at " + stop.vt_ps + " ps");
    }
    return { windows: out, reason };
  }
}

const PS_PER_MS = 1_000_000_000n;
const BUTTONS = { up: "Up", down: "Down", ok: "Ok" };

/** Journals every click at its instant from `fromPs`: press, then release `clickMs` later. */
function journal(core, handle, fromPs, clicks, clickMs) {
  const entries = [];
  for (const [ms, button] of clicks) {
    const press = fromPs + BigInt(ms) * PS_PER_MS;
    const release = press + BigInt(clickMs) * PS_PER_MS;
    for (const [at, down] of [
      [press, true],
      [release, false],
    ]) {
      entries.push({ at: at.toString(), event: { Button: { id: BUTTONS[button], down } } });
    }
  }
  if (entries.length > 0) core.input(handle, entries);
}

function runSuite(msg) {
  const module = coreModule(msg.core);
  const bundle = new Uint8Array(msg.bundle);
  const s = msg.scenario;
  const windowPs = BigInt(msg.windowMs) * PS_PER_MS;

  // The calibration pass, on a twin machine (`xtask/src/bench.rs` "How S and c are measured").
  const twin = new Core(module);
  const twinHandle = twin.build(bundle);
  twin.marker(twinHandle, s.bootTo);
  const calibrationSlices = Math.ceil(s.bootBudgetMs / msg.calibrationMs) + msg.bootSlackWindows;
  const calibration = twin.slices(twinHandle, BigInt(msg.calibrationMs) * PS_PER_MS, calibrationSlices).windows;
  twin.x.pemu_drop(twinHandle);

  const core = new Core(module);
  const handle = core.build(bundle);
  core.marker(handle, s.bootTo);
  const bootWindows = Math.ceil(s.bootBudgetMs / msg.windowMs) + msg.bootSlackWindows;
  const booted = core.slices(handle, windowPs, bootWindows);
  if (booted.reason !== "Matcher") {
    throw new Error("the boot did not print " + JSON.stringify(s.bootTo) + " within " + s.bootBudgetMs + " ms virtual");
  }
  core.marker(handle, null);
  const bootPs = core.nowPs(handle);
  const boot = booted.windows;

  journal(core, handle, core.nowPs(handle), s.setup, msg.clickMs);
  if (s.setupMs > 0) {
    const setup = core.slices(handle, windowPs, Math.ceil(s.setupMs / msg.windowMs));
    if (setup.reason !== "Until") {
      throw new Error("the setup stopped for " + setup.reason);
    }
    boot.push(...setup.windows);
  }

  journal(core, handle, core.nowPs(handle), s.clicks, msg.clickMs);
  const body = core.slices(handle, windowPs, Math.ceil(s.bodyMs / msg.windowMs)).windows;
  const stop = core.json(core.x.pemu_last_stop(handle));
  core.x.pemu_drop(handle);
  return {
    calibration,
    boot,
    body,
    bootPs: bootPs.toString(),
    end: { vtPs: stop.vt_ps, windows: body.length },
  };
}

/**
 * The emulator alone at 1x: boots to `bootTo` unpaced, then runs `ms` of wall time at rate 1 in
 * `sliceMs` slices with an `Atomics.wait` until each is due. `busyMs` is host time inside
 * `pemu_run`. With `counted` it reads `pemu_last_stop` per slice, since a host second buys
 * different work per core: `busyInsns / busyMs` is host MIPS while paced, `busyInsns / virtualMs`
 * the guest's demand. That read is timed into `stopMs`, never `busyMs`. `spin` replaces the wait
 * with a busy loop: the control for whether the host got the core back between slices.
 */
function runPaced(msg) {
  const core = new Core(coreModule(msg.core));
  const handle = core.build(new Uint8Array(msg.bundle));
  core.marker(handle, msg.bootTo);
  const booted = core.slices(handle, 100n * PS_PER_MS, 30);
  if (booted.reason !== "Matcher") {
    throw new Error("the boot did not print " + JSON.stringify(msg.bootTo));
  }
  core.marker(handle, null);
  const counted = msg.counted === true;
  const cell = new Int32Array(new SharedArrayBuffer(4));
  const startPs = core.nowPs(handle);
  const t0 = performance.now();
  let busyMs = 0;
  let stopMs = 0;
  let waitMs = 0;
  let slices = 0;
  let iterations = 0;
  let busyInsns = 0;
  let ffInsns = 0;
  let idlePs = 0;
  let longestSliceMs = 0;
  for (;;) {
    const wall = performance.now() - t0;
    if (wall >= msg.ms) break;
    iterations += 1;
    const target = startPs + BigInt(Math.floor(wall * 1_000)) * 1_000_000n;
    if (target > core.nowPs(handle)) {
      const a = performance.now();
      core.x.pemu_run(handle, target, -1n);
      const took = performance.now() - a;
      busyMs += took;
      if (took > longestSliceMs) longestSliceMs = took;
      slices += 1;
      if (counted) {
        const b = performance.now();
        const stop = core.json(core.x.pemu_last_stop(handle));
        busyInsns += Number(BigInt(stop.insns) - BigInt(stop.ff_insns));
        ffInsns += Number(stop.ff_insns);
        idlePs += Number(stop.idle_ps);
        stopMs += performance.now() - b;
      }
    }
    const w = performance.now();
    if (msg.spin === true) {
      while (performance.now() - w < msg.sliceMs) {
        // The control leg: the same pause, with the core held rather than given back.
      }
    } else {
      Atomics.wait(cell, 0, 0, msg.sliceMs);
    }
    waitMs += performance.now() - w;
  }
  const wallMs = performance.now() - t0;
  const virtualMs = Number((core.nowPs(handle) - startPs) / 1_000_000n) / 1_000;
  core.x.pemu_drop(handle);
  return {
    wallMs,
    virtualMs,
    busyMs,
    slices,
    sliceMs: msg.sliceMs,
    counted,
    spin: msg.spin === true,
    iterations,
    waitMs,
    stopMs,
    longestSliceMs,
    busyInsns,
    ffInsns,
    idlePs,
  };
}

self.onmessage = (event) => {
  const msg = event.data;
  try {
    const resolutionMs = timerResolution();
    const value = msg.op === "k" ? runK(msg) : msg.op === "suite" ? runSuite(msg) : msg.op === "paced" ? runPaced(msg) : null;
    if (value === null) {
      throw new Error("unknown op " + msg.op);
    }
    self.postMessage({ ok: true, value, resolutionMs });
  } catch (error) {
    self.postMessage({ ok: false, error: String(error && error.stack ? error.stack : error) });
  }
};
