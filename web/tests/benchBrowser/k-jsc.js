// A narrow driver of workload K for the macOS JavaScriptCore shell, for iterating on why JSC runs
// our engine behind the spike. Not a gate: the gate is `cargo xtask bench-browser` in Playwright
// WebKit, which this mirrors.
//
// It runs what `benchBrowser/worker.js` `runK` runs, in the system `jsc`, where JSC's own options
// (`--useOMGJIT=false`, `--maxB3TailDupBlockSize=N`, `--dumpOMGDisassembly=true
// --omgAllowlist=<function index>`) can be set per run:
//
//   JSC=/System/Library/Frameworks/JavaScriptCore.framework/Versions/Current/Helpers/jsc
//   cargo build -p pemu-rv32 --example kbench_wasm --target wasm32-unknown-unknown --profile wasm-release
//   $JSC [jsc options] web/tests/benchBrowser/k-jsc.js -- <spike.wasm> \
//     target/wasm32-unknown-unknown/wasm-release/examples/kbench_wasm.wasm <fw-Og kernel.bin> [repeat] [which]
//
// `<spike.wasm>` is `<data root>/spikes/rv32-interp/target/wasm32-unknown-unknown/release/rv32_interp.wasm`,
// the kernel `<data root>/spikes/rv32-interp/workload/fw-Og.bin`. `which` is `both` (default),
// `ours` or `spike`. Every run is checked against the native ledger of `xtask/src/bench_k.rs`
// (checksum 0x7A19B27C, 413,918,117 instructions on the spike, one fewer on ours), so a run that
// did not reproduce it never prints a speed.

"use strict";

const [spikePath, oursPath, kernelPath, repeatArg, whichArg] = arguments;
const repeat = Number(repeatArg || 7);
const which = whichArg || "both";

/** `xtask/src/bench_k.rs` `WORKLOADS[0]` and `kbench_wasm`'s defaults (`bench-browser` `params`). */
const ITERS = 10000;
const SLICE = 1000000;
const MAX_BLOCK_INSNS = 64;
const CHECKSUM = 0x7a19b27c;
const SPIKE_INSNS = 413918117;
const SPIKE_BLOCKX = 3;
const SPIKE_FW_OG = 3;

const now = () => preciseTime() * 1000;
const spike = new WebAssembly.Instance(new WebAssembly.Module(readFile(spikePath, "binary")), {}).exports;
const ours = new WebAssembly.Instance(new WebAssembly.Module(readFile(oursPath, "binary")), {}).exports;
const kernel = new Uint8Array(readFile(kernelPath, "binary"));

function check(side, insns, checksum, expected) {
  if (insns !== expected || checksum !== CHECKSUM) {
    throw new Error(`${side}: ${insns} instructions, checksum 0x${checksum.toString(16)}; the native ledger is ` +
      `${expected} and 0x${CHECKSUM.toString(16)}`);
  }
}

const series = { spike: [], ours: [] };

function runSpike() {
  const t0 = now();
  const insns = spike.bench(SPIKE_BLOCKX, SPIKE_FW_OG, ITERS, SLICE);
  const secs = (now() - t0) / 1000;
  check("spike", insns, spike.last_checksum() >>> 0, SPIKE_INSNS);
  series.spike.push(insns / secs / 1e6);
  print(`spike ${(insns / secs / 1e6).toFixed(1)} Minsn/s`);
}

function runOurs() {
  const ptr = ours.kb_alloc(kernel.length);
  new Uint8Array(ours.memory.buffer, ptr, kernel.length).set(kernel);
  const loaded = ours.kb_load(ptr, kernel.length, ITERS, MAX_BLOCK_INSNS);
  ours.kb_free(ptr, kernel.length);
  if (loaded !== 0) throw new Error("kb_load failed");
  const t0 = now();
  const insns = ours.kb_run(SLICE);
  const secs = (now() - t0) / 1000;
  check("ours", insns, ours.kb_checksum() >>> 0, SPIKE_INSNS - 1);
  series.ours.push(insns / secs / 1e6);
  print(`ours  ${(insns / secs / 1e6).toFixed(1)} Minsn/s`);
  ours.kb_unload();
}

// Alternated as `worker.js` alternates them, so neither side always runs first.
for (let r = 0; r < repeat; r++) {
  const spikeFirst = r % 2 === 0;
  if (which !== "ours" && spikeFirst) runSpike();
  if (which !== "spike") runOurs();
  if (which !== "ours" && !spikeFirst) runSpike();
}

const median = (xs) => xs.slice().sort((a, b) => a - b)[xs.length >> 1];
const parts = [];
if (series.ours.length) parts.push(`ours ${median(series.ours).toFixed(1)}`);
if (series.spike.length) parts.push(`spike ${median(series.spike).toFixed(1)}`);
if (series.ours.length && series.spike.length) {
  parts.push(`ratio ${(median(series.ours) / median(series.spike)).toFixed(3)}`);
}
print(`median of ${repeat}: ${parts.join(", ")}`);
