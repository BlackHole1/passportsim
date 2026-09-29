# Design facts

Facts behind the design of the core, the agent interface and the fidelity rules, restated as
behavior statements. It records behavior, not code: names and shapes come from
`docs/ARCHITECTURE.md` and the frozen interfaces.

- **Citing.** Cite a section as "design-facts <id>", for example "design-facts perf-A" or
  "design-facts fid-8.4". The prefix names the area (`perf` performance, `agent` agent interface,
  `fid` fidelity); the rest is the section number (`A` is the performance appendix).
- **Section layout.** *Design* gives facts the architecture relies on. *Details* gives the finer
  facts behind them.
- **Markers.** UNVERIFIED marks a fact not confirmed on silicon or against a primary source.
  "Checked against IDF v5.5.3" marks a value confirmed by compiling the IDF headers with the
  esp-14.2.0 toolchain.

## Index

| Id | Topic |
|---|---|
| perf-A | Interpreter spike and the predecoded op |
| perf-2.1 | Time and value types |
| perf-2.9 | Snapshot, restore and fork |
| perf-2.10 | wasm ABI |
| perf-3.1 | Engine |
| perf-3.2 | How time advances |
| perf-3.4 | Run loop |
| perf-3.5 | Deterministic poll-loop fast-forward |
| perf-3.6 | Browser threading, pacing and audio |
| perf-8.3 | Determinism suite |
| perf-8.5 | Performance benchmarks and targets |
| agent-1.4 | Spec tables as data |
| agent-2.1 | Virtual time |
| agent-2.6 | Clock and scheduler |
| agent-2.9 | Host I/O channels |
| agent-2.10 | Snapshot and restore |
| agent-7.1 | Command registry |
| agent-7.2 | Command set |
| agent-7.3 | Deterministic waits and matchers |
| agent-7.4 | Token-efficient outputs |
| agent-7.5 | Instances, snapshots, forks and parallel runs |
| agent-7.6 | Panic, watchdog, deadlock and hang decoding |
| agent-7.7 | Servers, security and browser attach |
| agent-7.8 | Web UI layout |
| agent-8.1 | Test layers and the test firmware list |
| fid-3.4 | Zero-cost versus modeled-duration I/O |
| fid-3.5 | Device event scheduling, run loop and determinism contract |
| fid-8.2 | Canonical trace format and MMIO differential |
| fid-8.3 | Golden console tests |
| fid-8.4 | Register-model tests and the IRQ numbering pitfall |
| fid-8.6 | Probe firmware list |
| fid-8.7 | CPU conformance |
| fid-8.9 | Self-consistency and determinism tests |

## perf-A: interpreter spike and the predecoded op

- **Design:**
  - `Op` is a 16-byte `repr(C)` record with fields kind, rd, rs1, rs2, imm, imm2, len, flags, pc_off.
  - Blocks hold at most 64 instructions, end at terminators and 4 KB page boundaries, and are keyed by virtual PC.
  - Lookup order is chain slot (94 to 96 % of transitions in the spike), then a 65,536-entry direct-mapped jump cache, then a never-iterated map.
  - `match` dispatch on `Op.kind` measured faster than function pointers in V8 and JSC.
  - Exact budgets use a separate never-inlined partial path plus an in-block resume token (block id, op index, cache generation).
  - Page table of 2^20 entries (arena offset plus flags); fast load needs R and in-page; fast store needs W without CODE or SLOW.
  - Store into a translated byte range invalidates that page's blocks and returns `OkStop`.
  - MMIO dispatch is a `match` on `PeriphId`.
  - fw-Og is the design reference, bench-Og an upper bound; spike `blockx` fw-Og ranges native 433.8-470.2, Chrome Worker 379.8-390.6, JSC 430.1-444.1, Node 366.9-398.9 MIPS.
  - Spike speed 380 to 445 Minsn/s on fw-Og in Chrome, Node and JSC.
  - Rust 1.93.0 is the measured toolchain; `jsc` shell equality with Safari is UNVERIFIED; `wasm-opt` optional; `wasm32-wasip1` optional only.
  - Real firmware runs below kernel MIPS (MMIO slow paths, CSR terminators in critical sections, 1 kHz tick).
  - Macro-op fusion is not in v1 and sits behind the same `Op` format.
- **Details:**
  - **Op contents.** When a value is known at decode time, imm and imm2 carry the absolute value (auipc result, branch target, link value), so ordinary ops never need the PC. Any op whose only effect is a write to x0 decodes to NOP. pc_off is the op's byte offset from its block start PC, so a trap PC is block PC plus pc_off.
  - **Method (A.1).** Host: Apple M3 Pro, 12 cores, 36 GB, macOS 27.0. Native build: release, opt-level 3, fat LTO, one codegen unit, panic abort, default target features. Guest kernels: riscv32-esp-elf-gcc 14.2.0, `-march=rv32imc_zicsr_zifencei -mabi=ilp32`, freestanding, no builtins, no loop-distribution into library calls; each built at -O2 and -Og (the official firmware builds with -Og); linked at 0x40380000 with 1 MB RAM; the iteration count arrives in a0 and the kernel exits through `ecall` with a7 = 93.
  - **Kernels.** bench (loop-heavy, image 1.5 KB): bitwise CRC32 over 2 KB, 20x20 matrix multiply, 240x32 RGB565 alpha blend, insertion sort of 160, switch-based bytecode VM. fw (call-heavy, firmware-like, image 2.5-2.8 KB): non-inlined helpers, class tables of draw and event function pointers over 40 objects on a 64x48 framebuffer, queue send and receive inside critical sections, a sorted software-timer list, a mini printf, FNV hash.
  - **Correctness anchors.** Expected checksums: bench 500 iterations 0x4272acaf; bench 350 iterations 0x50f7a0a8; fw 10,000 iterations 0x7a19b27c. All 255 recorded runs matched. Every engine retired the same count per workload: bench-O2 414,181,733; bench-Og 391,184,244; fw-O2 380,675,932; fw-Og 413,918,117.
  - **Engines compared.** naive (fetch and decode every instruction into `Op`, then execute); block (predecoded blocks, `match`, two chain slots per block, jump cache in front of a map; may overshoot a deadline by up to one block); blockfn (same blocks, per-op handler function pointer, `call_indirect` in wasm); blockx (block plus instruction-exact deadlines: a block that would cross the deadline runs only its prefix).
  - **Measure.** MIPS = retired instructions / wall seconds of the whole run, including cold translation; cells are min-max of 3 runs; slice (instructions per engine call) 1,000,000 by default and 10,000 as the dense case (a 1 kHz tick at 160 MHz is 160,000 instructions). Host not isolated; spread inside a cell mostly below 3 %.
  - **Representativeness.** Static instructions per control transfer: official app 4.53, passport-keys 4.58, ROM rev101 5.38. Measured block length: bench 10.8-11.0, fw 6.6.
  - **Artifacts.** wasm32-unknown-unknown module 61 KB (4 engines, 4 kernels, no wasm-bindgen); native binary 411 KB.
  - **Results, fw-Og and bench-Og (A.2), MIPS min-max:**

    | Workload | Engine | Native | Node 26 | Chrome Worker | Chrome main | JSC shell |
    |---|---|---|---|---|---|---|
    | fw-Og | naive | 166.7-175.5 | 120.0-121.8 | 114.0-115.2 | 114.3-115.9 | 110.3-112.5 |
    | fw-Og | block | 465.1-468.4 | 376.9-388.4 | 409.1-410.0 | 409.4-411.7 | 443.2-444.3 |
    | fw-Og | blockfn | 423.1-427.5 | 262.8-265.5 | 256.6-257.9 | 255.7-259.2 | 318.3-333.3 |
    | fw-Og | blockx | 433.8-464.2 | 396.7-398.9 | 389.4-390.6 | 380.8-388.5 | 430.1-434.2 |
    | fw-Og, slice 10K | block | 459.1-459.8 | 384.5-388.4 | 411.1-418.2 | 416.8-419.3 | 442.0-446.8 |
    | fw-Og, slice 10K | blockx | 460.6-470.2 | 366.9-393.1 | 379.8-386.7 | 382.7-387.0 | 439.5-444.1 |
    | bench-Og | block | 531.2-533.3 | 470.2-473.1 | 484.7-488.9 | 476.7-489.8 | 508.0-518.8 |
    | bench-Og | blockx | 540.5-546.2 | 420.3-422.9 | 471.8-474.6 | 451.1-456.3 | 506.0-509.3 |

  - **Engine statistics (native runner):**

    | Workload | Translations block / blockx (1M) | blockx (10K) | Block execs | Insns per block | Chain-slot hits | Slow-path stores |
    |---|---|---|---|---|---|---|
    | bench-O2 | 64 / 164 | not run | 38.3 M | 10.81 | 95.0 % | 1,802 |
    | bench-Og | 98 / 220 | 320 | 35.7 M | 10.96 | 96.2 % | 68,188 |
    | fw-O2 | 163 / 333 | not run | 57.2 M | 6.66 | 93.9 % | 2,442,015 |
    | fw-Og | 180 / 352 | 731 | 62.7 M | 6.60 | 94.4 % | 3,501,414 |

  - **Dispatch (A.3).** blockfn against block, midpoints: native -9 % to -15 %; Chrome Worker -37 % (fw-Og) and -39 % (bench-O2); Node -31 % to -37 %; JSC shell -26 % to -35 %; Bun -32 %. Consequences: one monomorphized loop matching a dense u8 kind (a `br_table` in wasm), no per-op indirect calls; peripheral dispatch by `match` for the same reason (not measured separately).
  - **Exact deadlines (A.4).** blockx against block, midpoints (bench-Og / fw-Og / fw-Og slice 10K): native +2 / -4 (noisy, best -1) / +1 %; JSC -1 / -3 / -1 %; Chrome Worker -3 / -5 / -8 %; Chrome main -6 / -6 / -8 %; Node -11 / +4 / -2 %. The added work is one compare per block. Suspected cause of the Chrome cost is code layout and tier-up around a larger function, because the spike inlined two copies of the op loop (UNVERIFIED); the compare itself is not the suspect. Without in-block resume, translations doubled at 1M slices (fw-Og 180 to 352) and quadrupled at 10K (731); real firmware stops far more often, so the cache would grow.
  - **Linking and memory (A.5).** Two chain slots plus a direct-mapped cache suffice; no inline caches for indirect jumps in v1 (misses reaching the map were not counted separately). The spike's data and bss share the last 4 KB text page, so global stores there take the slow path: 3.5 M on fw-Og (0.85 % of instructions) with fw-Og still at 465 MIPS native. IDF images have such a mixed page at the IRAM/DRAM boundary in SRAM1; how often firmware stores there is UNVERIFIED. The naive engine reaches 110-128 MIPS in wasm, against 42-55 MIPS reported for prior emulators (different workloads; indicative only).
  - **Placement and runtimes (A.6).** Chrome Worker and main thread agree within 2 %. wasm/native ratio for block: JavaScriptCore 0.95-0.97; Chrome 0.79-0.92 (0.88-0.92 on the -Og kernels); Node 0.82-0.89. Safari is expected to be the fastest browser for this core (UNVERIFIED until run in Safari). 100x more engine calls (10K instead of 1M slices) changes block throughput by -2 % to +1 %; the machine-side cost of dispatching events per slice is not part of that measurement. wasm32-wasip1 and wasm32-unknown-unknown are within noise in Node; the web build uses unknown-unknown with no WASI shim.
  - **Limits (A.7).** The spike has no MMIO, CSRs, interrupts or HLE hooks. Projection (UNVERIFIED): busy real firmware at 50-75 % of the fw-Og numbers, about 200-300 MIPS in Chrome and JSC and 230-350 native, which in the browser is 1.3-1.9x a fully busy 160 MHz core. Not measured: Safari itself, Firefox, Linux or x86 hosts, memory use and startup time, several competing instances, `wasm-opt`.
  - **Reproduction (A.8, our spike under `ROOT/spikes/rv32-interp`).** Build the guest kernels with the spike's workload build script (the esp-14.2 toolchain), build the crate in release for native and for wasm32-unknown-unknown, then run the native bench and the Node, Bun, `jsc` and Chrome (CDP, headless) runners on workload index 3 (fw-Og), 10,000 iterations, 3 repeats, all four engines, slice 1,000,000, expecting 0x7a19b27c. Raw outputs are the `*-matrix.txt` files (matrix round) and the per-runtime files of the first round in `results/`, with checksums in `results/expect.txt`.

## perf-2.1: time and value types

- **Design:**
  - Virtual time in picoseconds since power-on; 2^64 ps is about 213 days.
  - Audio frame instants are exact, n x 1e12 / fs in u128.
  - The clock maps executed instructions to time from a base taken at the last rebase; rebase on frequency change; idle moves time but not instructions; instructions-until-deadline rounds up and is at least 1; 6,250 ps per instruction at 160 MHz and CPI 1.
  - The scheduler orders by (time, seq), seq is insertion order and snapshotted, cancel is O(1) by generation bump.
  - Recorded host streams (mic chunks, network frames, HCI) are journaled inputs.
- **Details:**
  - Every MHz-derived clock period is an integer number of picoseconds (6,250 at 160 MHz, 12,500 at 80 MHz, 25,000 at 40 MHz); the full list of periods is in agent-2.1.
  - Cancellation is lazy: a cancelled entry stays in the heap and is discarded when it reaches the top with a stale generation, so cancel never searches the heap.

## perf-2.9: snapshot, restore and fork

- **Design:**
  - Header carries format version, core version, ROM SHA, image SHAs and configuration; eFuse contents never leave in an export.
  - Sections are postcard, optionally lz4-compressed.
  - Fork shares ROM and flash base (`Arc`) and copies RAM and overlay.
  - Engine cache, jump cache and page table are derived state, rebuilt and never saved, which is safe because block boundaries are unobservable.
  - Fork and rewind snapshots stay in memory.
  - The cardid window [0x356000, 0x35A000) is never exported.
- **Details:**
  - Guest RAM in a snapshot is SRAM0 plus SRAM1 plus RTC FAST RAM: 400 KB plus 8 KB (datasheet sizes for the C3).
  - The flash delta is kept per 4 KB page: only pages that differ from the base image are stored.
  - A forked machine rebuilds its engine cache lazily, by retranslating on first execution; nothing from the parent's cache is copied.

## perf-2.10: wasm ABI

- **Design:** allocation and free, machine creation from a configuration, asset loading by kind, run, batched input, ring layout, current time, JSON command call, snapshot and restore. Wall budgets are checked by the host between calls. Inputs without an explicit time are stamped by the core and journaled (`At::Now`).

## perf-3.1: engine

- **Design:**
  - One decoder for RV32IMC, Zicsr and Zifencei.
  - Every FENCE encoding is a NOP; `fence.i` flushes the cache.
  - CSR instructions, `ecall`, `ebreak`, `mret`, `wfi`, jumps and branches are terminators.
  - Decoding is checked against `riscv32-esp-elf-objdump` over every instruction of the ROM ELFs and corpus ELFs.
  - Block limits, keying by virtual PC, lookup order and the 94 to 96 % chain hit rate.
  - `match` dispatch and register indexing masked with 31.
  - The partial-block path is a separate never-inlined function.
  - Invalidation triggers and actions (translated-range store, MMU entry write over 64 KB of IBUS VA, flash program or erase under translated code, `fence.i`/ROM reload/reset flush, hook or breakpoint change).
  - Fusion is not in v1.
- **Details:**
  - Decode: all MISC-MEM encodings with funct3 = 000, whatever their other fields, are NOP (RISC-V Unprivileged ISA, FENCE: reserved configurations are treated as normal fences, and one hart has nothing to order).
  - Inside a block the ops before the terminator run in one tight loop; the terminator then runs and selects which of the block's chain slots to follow.
  - Memory ops call the page-table fast paths inline; their slow paths are never inlined into the hot loop. The hot loop holds exactly one copy of the op loop (the spike's two copies are the UNVERIFIED suspect for its 3-8 % Chrome cost, perf-A).
  - Invalidated blocks are unlinked by marking their start PC invalid; a chain jump checks the target block's PC before entering, so a stale link is never followed. Their op ranges return to a free list, and a full flush compacts op storage.
  - Flash invalidation finds translated code through a per-physical-page code bitmap, then invalidates every VA range mapping that page.
  - Lookup order through the map never affects state, because nothing iterates it.

## perf-3.2: how time advances

- **Design:**
  - Each instruction adds 1 and one instruction's picoseconds at the current CPU frequency.
  - WFI and light sleep add no instructions and jump to the next event (deterministic) or pacing target (paced).
  - Deep sleep resets the CPU on wake, jumps to the wake event and keeps the RTC domain.
  - Frequency switches (`SYSCLK_CONF`, `CPU_PER_CONF`) return `Wiring::ClockChanged` and rebase the clock, so the cycle counter advances `cpu_mhz` per emulated µs of executed time.
  - SYSTIMER, TIMG, RTC timer, watchdogs and slow-clock calibration compute values lazily from virtual time and schedule alarms; nothing ticks.
  - One RTC slow-clock constant serves every consumer (RWDT, sleep timer, RTC counter).

## perf-3.4: run loop

- **Design:** per iteration, due events are dispatched and their wiring applied, stops are checked at an exact instruction boundary, a WFI hart wakes or idles to the next deadline, an interrupt is taken when MIE is set and a line is deliverable, the budget is computed from the clock to the next deadline, the engine runs and its exit is handled; wall budgets and pacing live outside `run`.

## perf-3.5: deterministic poll-loop fast-forward

- **Design:**
  - Firmware busy-waits on hardware state, SPI2 polling being the hottest case.
  - Fast-forward never changes guest-visible state; results are identical with it on or off.
  - k is the floor of the time to the stop over one period's picoseconds; stability classes include "until next event" (SPI2 `trans_done`).
  - Loops that fail get an exponential backoff.
  - Loops that read time-derived CSRs are not fast-forwarded.
  - Fast-forward on and off is a determinism test.
- **Details:**
  - Busy-wait targets the design expects: SPI2 polling transactions; `vPortYield` waiting on FROM_CPU (IDF freertos/FreeRTOS-Kernel/portable/riscv/port.c); flash WIP; I2S `tx_update`, which is self-clearing and so exits at once; ADC conversion done.
  - Scale: with modeled durations a 9,600-byte LCD flush at 40 MHz spins about 1.92 ms, about 307,000 instructions at 160 MHz.
  - esp_timer reads inside tight loops are rare in the corpus, so time-reading loops that are not the ROM delay simply execute (estimate, UNVERIFIED).

## perf-3.6: browser threading, pacing and audio

- **Design:**
  - Main thread (UI), emulator Worker (wasm core), AudioWorklet topology; wasm memory not shared; core single-threaded.
  - SharedArrayBuffers only between JS parties and only when cross-origin isolated; otherwise `postMessage` plus a MessagePort transferred to the worklet; headers optional.
  - OffscreenCanvas WebGL1 `UNSIGNED_SHORT_5_6_5` upload of dirty rows straight from wasm memory.
  - Audio-master target is the last consumed sample plus 60 ms, used while I2S TX runs and an AudioContext is live.
  - Falling behind by more than 250 ms re-anchors; the guest runs in slow motion and virtual time never jumps.
  - Hidden tabs show paused or slow motion; Worker throttling in background tabs is UNVERIFIED.
  - A paced session equals its journal replayed at `Max` (frame and PCM hashes).
  - Live mic samples are journaled as consumed.
- **Details:**
  - Stable Rust cannot build wasm32-unknown-unknown with atomics without rebuilding std on nightly (true as of 2025; UNVERIFIED for 1.93); this is one reason wasm memory is not shared.
  - Cross-origin isolation needs COOP `same-origin` and COEP `require-corp`; Safari supports both (from Safari 15.2). The non-isolated path adds a hop and latency (numbers UNVERIFIED).
  - The canvas is transferred to the Worker once. The Worker uploads after a slice that produced a dirty frame, at most once per 16 ms, from a `Uint16Array` view of wasm memory; there is no main-thread copy. Backlight duty and panel inversion are applied as shader uniforms, not by rewriting pixels.
  - Speed choices: 0.05x to 64x, max, paused, single step. After each slice the Worker pumps frame, PCM and serial. When virtual time has reached the target, the Worker sleeps until the next input or 4 ms, whichever is first (`Atomics.wait` on the input SAB when available, else a timer).
  - The worklet only converts sample rate (guest fs to context fs). When I2S stops, pacing hands back to the wall anchor without a virtual-time jump.
  - The UI reacts to `visibilitychange`.

## perf-8.3: determinism suite

- **Design:** variants block size {1, 3, 64}, fast-forward on and off, snapshot at a random instruction and restore in a fresh machine, native versus Node versus `jsc`; equal serial, MMIO and IRQ traces, frame and PCM hashes and final state; a paced browser journal replayed natively matches frame and PCM hashes.
- **Details:**
  - MMIO and IRQ trace record fields: fid-8.2 item 1.

## perf-8.5: performance benchmarks and targets

- **Design:** suites K and F1 to F7; busy MIPS excluding idle skip and fast-forward credit, real-time factor, guest demand per 100 ms (p95, max), paced host CPU, snapshot size and time; F1 wall <= 0.2 s native and <= 0.5 s in browsers; F4 to F6 paced real-time factor >= 1.0 with no re-anchors.

## agent-1.4: spec tables as data

- **Design:**
  - Register facts live as data with citations, one file per block; behavior after a write is hand-written next to the generated storage.
  - Field access kinds RW, RO, WO, W1C, W1S, WT (reads 0), SC, RC.
  - Codegen emits per-block storage and tests for reset values per reset scope, access types, address decode and overrides.
  - Busy-wait rows (trigger, expected read, bound, polling function, images, source, milestone); each row becomes a model test and a hang-detector expectation.
  - `RegHarness` is a `Cx` plus `MockBoard` with no machine; contributor loop order.
  - SYSTIMER VALUE_VALID is set after an UPDATE write.
- **Details:**
  - Diagnostics render an MMIO address as `BLOCK.REGISTER` (for example 0x60023004 renders as `SYSTIMER.UNIT0_OP`, SYSTIMER base 0x60023000, block size 0x100; address checked against IDF v5.5.3). Codegen therefore provides a per-block register name table usable by traces, `E_STUCK` and `inspect periph`.
  - A busy-wait row test drives the harness through the row's trigger and asserts the row's expected read, one test per row.
  - SYSTIMER uses interrupt sources 37, 38 and 39 (targets 0 to 2). Numbering by name and the reference numbers: fid-8.4 items 1 and 2.
  - Generated register code is committed so diffs are reviewable, and a check fails CI when it is stale: `pemu-soc-c3/src/gen/regs_<block>.rs` and `pemu-core/src/irq_source.rs` are per-item generated files, committed, and `xtask codegen --check` checks them.

## agent-2.1: virtual time

- **Design:**
  - `VTime` is a u64 count from the first power-on, with `from_us`, `from_ms` and a flooring `as_us`.
  - Exact audio frame times with no drift, computed in u128.
  - SYSTIMER counts 16 ticks per µs, so one tick is 62,500 ps, derived on read.
  - USJ FRAME_NUM advances once per 1 ms of attached time (1 kHz SOF).
- **Details:**
  - Design goal: every SoC clock period is an exact integer number of time units, so no conversion accumulates error. In picoseconds: 160 MHz CPU cycle 6,250; 80 MHz cycle 12,500; 40 MHz XTAL cycle 25,000; SYSTIMER tick 62,500; 1 µs 1,000,000; USB SOF period 10^9; a 16 kHz audio frame 62,500,000. Rates with a non-integer period (for example 44.1 kHz) go through `frame_time`.
  - Conversions to coarser units floor; the SYSTIMER counter is floor((vt - epoch) / 62,500) plus the loaded value.

## agent-2.6: clock and scheduler

- **Design:**
  - The clock maps retired instructions to virtual time from a base point and rebases on a CPU frequency change.
  - 6,250 ps per instruction at 160 MHz and CPI 1.
  - Idle time (WFI, light sleep) moves virtual time without instructions or cycle counts; the cycle counter (CSR 0x7E2, alias 0x802) excludes WFI.
  - Scheduler order is (time, seq), seq is insertion order and part of the snapshot, so delivery order is a pure function of guest behavior.
  - Cancel is O(1) by a generation bump.
  - Next deadline and pop-due queries.
  - Clock lease holders and conflicts.
- **Details:**
  - `inspect sched` lists pending events in delivery order with time and owner. The frozen `Scheduler` has no enumeration method, so this needs a non-frozen reader.

## agent-2.9: host I/O channels

- **Design:**
  - Guest-to-host serial bytes carry absolute u64 cursors that survive resets.
  - Host-originated inputs are never applied directly; they are journaled with virtual time and seq, applied by the run loop, saved in snapshots, and replay gives the same run.
  - Frame port: raw RGB565 240x320, dirty rows, generation, backlight duty, panel power, sleep and inversion.
  - PCM output records carry start time, rate, channels and i16 samples; input underflow yields zeros and is counted.
  - Microphone source kinds silence, tone, file and live.
  - Rings live in core memory; wasm exposes them as typed-array views with fixed capacity; no per-byte JS crossing.
  - Event ring kinds (reset, panic, sleep, power, frame, ui-settled, fidelity warnings); network frame pipe and HCI packet pipe.
- **Details:**
  - A serial read from a cursor returns the bytes, the next cursor and a dropped count: the number of bytes between the requested cursor and the oldest byte still retained. The read then starts at the oldest retained byte (UNVERIFIED detail; only the dropped count is specified).
  - The line index records, for every newline, its absolute offset, virtual time and stream. Serial and log matchers and a per-run serial index artifact read it.
  - Ring push and pop transfer as many items as fit or exist and return the count; a length query returns the buffered count.
  - The frame generation increments on each completed RAM write that changed at least one pixel; a write of identical pixels does not bump it, so `ui:changed` does not fire on redraws of the same image (design choice).
  - The input journal is also written as an NDJSON file next to the run artifacts (file name UNVERIFIED; only the `events.ndjson` alias is named).

## agent-2.10: snapshot and restore

- **Design:**
  - Header with format version, core version, ROM SHA, image SHAs, config hash and eFuse kind (never bytes when exported).
  - Every section is versioned; a mismatch without a migration is `SnapError::Incompatible`, reported as `E_SNAPSHOT` (15).
  - Section list, derived state rebuilt rather than saved, skip fields need a rebuild.
  - Fork shares ROM and flash base, copies RAM and overlay.
  - In-memory save, target under 10 ms.
  - Export redaction: cardid window [0x356000, 0x35A000) becomes 0xFF with a salted hash, NVS becomes erased pages, eFuse bytes are omitted, live journal payloads are dropped; a redacted snapshot boots with factory NVS and a synthetic cardid and says so. Taint refuses export.
- **Details:**
  - The header also exposes virtual time, instruction count and machine seed at the snapshot instant, so `snapshot list` shows them without decoding sections.
  - Exported snapshots carry a redaction list: each removed item and the reason.
  - After restoring a redacted export, the first guest read of removed data (NVS, cardid, identity) raises a fidelity warning in the receipt.
  - An incompatible-section error names the section and the core version that wrote it.
  - RAM contents and the per-page flash delta are as stated in perf-2.9 (the memory map is authoritative for sizes); the base image is referenced by hash, not stored.
  - The file starts with a fixed 8-byte magic and a format number; each section entry has name, version, offset, length and a checksum (magic value and checksum UNVERIFIED).

## agent-7.1: command registry

- **Design:**
  - `CommandSpec` fields (name, caps group, summary, input and output schema, annotations, CLI shape, scenario step, examples, errors, handler).
  - `Output` with JSON, text, artifact references; `ApiError` envelope; registered error codes and ranges.
  - Generated surfaces and their guards (clap tree with `--json` for nested objects, MCP `passport_<cmd>` tools filtered by caps groups with four hint annotations, HTTP route and GET aliases, WS request shape, scenario step aliases, browser JS types, docs and skill references); MCP core list at most 12 KB (UNVERIFIED budget).
  - Registration by attribute macro into a distributed slice natively and a generated list in wasm.
  - A command author writes argument and output types, handler, text renderer and one example; examples run in CI against a fixture instance.
- **Details:**
  - The one-line doc comment of a command is its summary, reused verbatim by CLI help, the MCP tool description and the generated docs.
  - Which annotation sets the MCP open-world hint is not stated (UNVERIFIED; commands reaching a bridged network or the device are the natural candidates).

## agent-7.2: command set

- **Design:**
  - The 13 core commands and their purposes; caps groups audio, radio, nfc, debug and device with their commands.
  - `input` is journaled; `clock step N` steps instructions; `screenshot --inline` is the only inline pixel path.
  - Device group native only with human confirmation.
- **Details:**
  - Native instance ids are `p1`, `p2`, ... in creation order; `start` returns the new id; forks take the next free ids; browser-attached instances are `b1`, `b2`.

## agent-7.3: deterministic waits and matchers

- **Design:**
  - `run` arguments `until`, `timeout` (10 s default, required when `until` is absent), `wall_budget_ms` (30,000), `settle` (`ui` default for `ui:*`).
  - Matcher kinds serial, log, ui, event, symbol, var, time and composite, each evaluated only on its trigger class.
  - LVGL safe points from DWARF per image.
  - Timeout is `E_TIMEOUT`, retryable, with a hint of last UI summary, serial tail and stuck diagnosis; wall budget is `E_WALL_BUDGET`.
- **Details:**
  - Matchers compile into registrations on trigger classes; no matcher adds work per instruction.
  - The `run` text result states: the matcher that fired and its virtual time; virtual time elapsed in this call, instructions retired and wall time; per serial channel the new line count, the new cursor and the last line; events seen with their virtual times.
  - Composite semantics: `any` fires on the first child match, `all` when every child has matched at least once in any order, `seq` when children match in the given order (UNVERIFIED; inferred from the names).
  - The default `from` of a serial matcher is not given (UNVERIFIED; `cursor` fits the absolute-cursor model).

## agent-7.4: token-efficient outputs

- **Design:**
  - Serial deltas: head 10 and tail 30 lines with an elided count, `x12` repeat collapse, 400-character lines, ANSI stripped, redaction.
  - UI tree grammar and pruning (63 objects to 17 lines, 976 characters), `ui --diff`, `eN` refs valid for one `ui_rev`, `E_STALE_REF`.
  - Screenshot result fields path, sha256, frame_gen, ui_rev; `--compare` returns a mismatch count and a diff PNG path.
  - Large data goes to the artifacts directory by path plus hash; error envelope fields; exit codes; one-line receipt suffix; output budgets 4,000 and 8,000 characters.
- **Details:**
  - `--compare` accepts a tolerance value; pixels within it do not count as mismatches (unit of the tolerance UNVERIFIED).

## agent-7.5: instances, snapshots, forks and parallel runs

- **Design:**
  - Daemon auto-spawn, discovery file (0600, port and token), exit after 10 idle minutes with no instances, paused between calls, `--ephemeral`.
  - One OS thread and mailbox per instance; instances in parallel, calls to one instance serialized; one shared pool for MCP stdio, MCP HTTP, CLI and web UI.
  - In-memory `snapshot save` under 10 ms target, `fork` sharing ROM and flash base, `--vary-seed`.
  - Boot cache at the first LVGL safe point after `app_main` returns, on disk under the user cache directory (0700), never exported; batch `--jobs 8` with one JUnit file.
- **Details:**
  - `restore NAME` restores a named in-memory snapshot into the same instance.
  - `fork NAME --count N` creates N instances; from `p1`, a count of 4 yields `p2` to `p5`.
  - Expected effect: a suite of 30 scenarios on one build boots once.

## agent-7.6: panic, watchdog, deadlock and hang decoding

- **Design:**
  - Observe hook at `esp_panic_handler` entry resolved per ELF (0x4211c3d6 in `official`); the run stops before the IDF reboots.
  - Panic info through DWARF layouts, unwinding with `.debug_frame` CFI plus inline frames, ROM frames from the ROM ELF; `E_GUEST_PANIC`; `--on-panic stop|reboot|continue`, default `stop`; hooks on `abort` and `__assert_func`.
  - Watchdog reports name the starving task, what it is blocked on, mutex owners and inherited priorities.
  - Stack overflow from the hardware guard (source 54) plus `vApplicationStackOverflowHook`; deadlock and stuck conditions with task table and lock owners.
  - addr2line context build 4.2 ms native and 6.1 ms wasm, no sidecar files.
- **Details:**
  - The panic report lists: reason text, virtual time, task name, task priority and stack high-water mark in bytes; numbered frames with address, symbol plus offset and file:line; `mcause` and `mtval` with a plain reading (for example `mcause` 5 with `mtval` 0 is a NULL read); serial tail; next commands (`inspect tasks`, `snapshot save <name>`, `run --on-panic reboot`).
  - `reboot` lets the guest panic path reset as on the device; `stop` keeps state inspectable and snapshot-able; the exact behavior of `continue` is UNVERIFIED.
  - A watchdog report reads as: which watchdog, which task was not fed and for how long, and the chain of blocked task, blocking object and owner with inherited priority.
  - The panic-info struct names differ per ELF and are resolved from DWARF; which names is UNVERIFIED.
  - Resolving 4 PCs costs 0.2 ms native and 1.2 ms wasm (MEASURED in the spike).
  - IDF routes the stack guard source 54 (`ETS_ASSIST_DEBUG_INTR_SOURCE`) to CPU interrupt line 27 (`ETS_ASSIST_DEBUG_INUM`) as a level interrupt at medium priority (checked against IDF v5.5.3 `soc.h` and `hw_stack_guard.c`).

## agent-7.7: servers, security and browser attach

- **Design:**
  - `serve` hosts HTTP, WS, MCP streamable HTTP and the web UI on 127.0.0.1:8765; bearer token file 0600; `Host` and `Origin` checks, no CORS; launch code for a session cookie (UNVERIFIED mechanism); COOP and COEP headers.
  - `mcp` stdio adapter spawning the daemon, `--caps`; browser instances attach over `/v1/attach` as `b1`, `b2`, commands proxied through the registry; in-page `window.passportEmu.call` without a daemon.
  - WS binary frames `FRM1` (28-byte header, RGB565 dirty rectangle), `AUD1` and `MIC1` (24-byte header, PCM).
- **Details:**
  - WS text frames carry JSON responses and events; binary frames carry only `FRM1`, `AUD1` and `MIC1`.

## agent-7.8: web UI layout

- **Design:**
  - Two-column layout, device skin with 240x320 canvas and integer scaling, buttons, USB U0 to U3, tabs, environment cards, narrow-screen stacking.
  - Every control calls a registry command; "Copy as CLI" and "Copy as scenario step"; recorder with derived `run --until` waits; rewind ring of 20 snapshots every 2 virtual s (under 1 MB each, target); drag and drop of build directory, merged bin, ELF or `.pebundle`; synthesized eFuse default.
- **Details:**
  - The header shows a short identifier of the loaded build next to the image name (which hash is UNVERIFIED).
  - The Events tab lists resets, panics, frames, leases and inputs. The console tab has a line filter.
  - Clicking a node in the UI tree tab copies its ref or selector.
  - Accessibility: every device button has a keyboard binding, and the UI tree tab doubles as a screen-reader view of the device screen.
  - The NFC card sets a tap dwell time; the Wi-Fi card has a bridge toggle, subject to the live-bridge pacing rules.

## agent-8.1: test layers and the test firmware list

- **Design:**
  - Layers: generated register and property tests, `RegHarness` model tests and I2C transcripts, CPU conformance, goldens, scenarios, oracle diffs, determinism suite, browser and performance.
  - `hle_probe` (nested `vTaskDelay`, `malloc`, `esp_event_post`; blocking nested calls refused inside an ISR).
  - 20 `esp_restart` calls give reason 0x0C each with no memprot reboot loop.
  - Task and interrupt watchdogs.
  - `usj_echo`, `sleep_timer`; `mbedtls_selftest` later.
  - Small reviewed test ELFs under 1 MB may be committed.
- **Details:**
  - The 20-restart loop is part of `probe_reset`; the watchdog tests map to `probe_wdt` plus the watchdog cases of `probe_reset`.
  - `usj_echo` runs the USJ in driver mode (IDF USB Serial/JTAG driver installed), not console mode.
  - Test firmware sources are MIT, built with the IDF v5.5.3 toolchain, and binaries are cached rather than rebuilt per run.

## fid-3.4: zero-cost versus modeled-duration I/O (rules, profiles, calibration)

- **Design:**
  1. Rules P1 to P4 (a polled duration is register state completed by an event; stalls only for flash cache fill; "immediate" is an event at now after the current instruction; profiles are data, hashed into `config_hash`, named in receipts).
  2. Profile table rows for cache fill, SPI1, flash WIP (0.7 ms / 45 ms / 150 ms / 20 s), SHA (initial 2.8 µs), RSA and AES, SPI2 at 40 MHz, I2C0, I2S0, SARADC, USJ drain, RTCCALI (7.53 ms for 1024 slow cycles), eFuse and regi2c.
  3. Segment-verify fit: 163.6 µs per KB on the device against 69.0 µs per KB zero-cost, gap 94.7 µs per KB.
  4. The delay-dominated backlight phase matches (131 ms against 130 ms); the 21 ms ROM-phase gap is open.
  5. Calibration unknowns `{cpi_milli, sha_block_ps, cache_fill_scale, usj_poll_ps, spi2_overhead_ps}`, least squares with 1 ms quantization as uniform noise, each phase either fit or validation.
  6. A cache-fill stall advances virtual time and, by profile flag, the cycle counter.
  7. Cache fill first principles: 205 µs per 4 KB page at 80 MHz DIO.
  8. Device anchor times (196, 210, 400, 402 and 438, 758 ms).
- **Details:**
  1. P1 addendum: no model puts the hart to sleep to emulate a duration. Only the guest's own WFI idles the CPU; the guest's polling consumes the virtual time.
  2. A calibration comparison aligned ESP_LOG timestamps of the device reference boot (last boot, MAC masked) with esp32sim run R8 (ECO7 ROM, v1.1 eFuse, SPI2 and GDMA support, 1 instruction = 1 cycle, zero-cost I/O).
  3. Bootloader segment verification, time between consecutive `esp_image: segment` lines:

     | Seg | Kind | Bytes | Device ms | Emu ms | Device µs/KB | Emu µs/KB | Gap µs/KB |
     |---|---|---|---|---|---|---|---|
     | 0 | map | 145552 | 23 | 10 | 161.8 | 70.4 | 91.5 |
     | 1 | load | 10604 | 3 | 1 | 289.7 | 96.6 | 193.1 |
     | 2 | load | 40428 | 7 | 3 | 177.3 | 76.0 | 101.3 |
     | 3 | map | 730488 | 117 | 49 | 164.0 | 68.7 | 95.3 |
     | 4 | load | 59164 | 11 | 5 | 190.4 | 86.5 | 103.8 |

     Only the large segments 0 and 3 are used for the fit; the small segments carry 1 ms quantization error.
  4. How the IDF bootloader verifies a segment: it maps flash through the flash cache (`bootloader_mmap`), runs a per-word checksum loop, and hashes with the ROM hardware SHA (`ets_sha_update`). Cache fill and SHA latency therefore both scale with segment size.
  5. UNVERIFIED split of the 94.7 µs per KB gap: one 64 KB cache page over SPI0 at 80 MHz DIO is 524,288 bits / 160 Mbit/s = 3.28 ms, i.e. 51 µs per KB; the remaining about 44 µs per KB would be SHA accelerator latency (16 SHA blocks of 64 bytes per KB of 1,024 bytes) plus CPI above 1.
  6. Milestone lines from `timing_calib.R8.out` (ms; step = delta from the previous row):

     | Phase | Device | Emu | Device step | Emu step | Reading |
     |---|---|---|---|---|---|
     | Bootloader banner | 24 | 3 | - | - | 21 ms of ROM-phase time missing |
     | Partition table end | 27 | 5 | 3 | 2 | |
     | App loaded | 196 | 77 | 169 | 72 | flash cache plus SHA |
     | cpu_start user code | 205 | 86 | 9 | 9 | CPU-bound init matches at 1 ms resolution |
     | app_main called | 210 | 87 | 5 | 1 | |
     | i2c ready | 211 | 87 | 1 | 0 | |
     | Backlight LEDC ready | 342 | 217 | 131 | 130 | dominated by `vTaskDelay` in panel init |
     | LVGL ready | 400 | 226 | 58 | 9 | first full-screen render and SPI2 flush |
     | BLE_INIT first line | 758 | 402 | 358 | 176 | not comparable (item 10) |

  7. Coverage: 56 of 67 timestamped device lines appear verbatim after masking in R8, 48 of 67 in R4. The normalizer test reproduces the 56 of 67.
  8. Candidates for the 21 ms ROM-phase gap, all UNVERIFIED: USJ drain waits, RTC clock calibration, flash wake.
  9. A full-screen flush of 240 x 320 x 2 = 153,600 bytes at 40 MHz is 30.7 ms of wire time alone, which explains most of the 58 ms against 9 ms LVGL step.
  10. The BLE_INIT step is not comparable: the R8 run had no CW2017, and the device spends 402 to 438 ms on the gauge profile.
  11. Estimates, UNVERIFIED: a `device` profile of 4 to 6 constants brings the boot timeline inside the golden timestamp bands; its wall cost is only extra poll iterations, below 20 % of boot instructions.
  12. The SPI2 CLOCK register value the firmware writes, 0x00001001, decodes to 40 MHz; the `device` SPI2 transaction time is bits / 40 MHz.
  13. The I2C0 `device` time is about 90 µs per byte at 100 kHz (9 bits per byte at the SCL period), plus START and STOP.
  14. Calibration observations are the per-phase deltas between device anchor lines, from the reference boot and from any later device capture. The profile records the fitted values together with residuals, the anchor list and the capture id.

## fid-3.5: device event scheduling, run loop and determinism contract

- **Design:**
  1. Loop order: due journal inputs, due events, stop check, WFI handling, interrupt take, budgeted engine run.
  2. Event order `(VTime, seq)`; events due at an instruction boundary run before the next instruction.
  3. An MMIO write that schedules an event before the budget end ends the slice, so no event fires late.
  4. The slice bound limits host latency only; results never depend on it.
  5. The IDF idle hook reaches WFI through `esp_cpu_wait_for_intr`, so idle skip needs no idle-loop pattern matching.
  6. Idle skip jumps virtual time to the next event or limit and records the idle time.
  7. Determinism contract D-1 to D-6 (identity, bit-identical outputs, independence list, forbidden sources including platform `libm`, seeded guest entropy with a fixed default seed, journaled bridges).
  8. `DetRng` is ChaCha20 keyed by the config seed.
- **Details:**
  1. `RNG_DATA` is the APB_CTRL register at 0x600260B0 (base 0x60026000 plus 0xB0, IDF `apb_ctrl_reg.h`, which IDF v5.5.3 marks deprecated in favor of the identical `syscon_reg.h`; checked against IDF v5.5.3), served from `DetRng`.
  2. The QEMU oracle's host-random behavior for that register is deliberately not reproduced.
  3. The D-3 host list is concrete: aarch64 macOS and x86_64 Linux natively, and wasm32 in Chrome, Safari and Node.

## fid-8.2: canonical trace format and MMIO differential

- **Design:**
  1. The QEMU trace line format has no PC and no instruction count; ingest yields per-block ordered streams without time.
  2. Region names map to blocks through `specs/oracle-qemu-regions.toml`; catch-all accesses map by address.
  3. Per-block LCS alignment of write streams `(offset, size, value)`, ignoring interleaving across blocks.
  4. Reads are compared only for `stable_read` registers.
  5. The first divergence per block is reported with context, our PC and symbol, and the QEMU record index.
  6. Intended divergences live in `specs/oracle-known-diffs.toml` with a reason (regi2c example).
  7. Coverage gate `xtask oracle hist`, new touches fail CI.
  8. Call-trace diff over about 60 named boot functions, observe hooks against gdb breakpoints.
  9. Consecutive identical tracked reads fold into one `PollRun` record; records carry logical instruction counts.
  10. Trace record types live in `pemu-core`.
  11. Canonical MMIO and IRQ traces are bit-identical for equal run identity.
- **Details:**
  1. Canonical trace record kinds and their fields (names not frozen):

     | Kind | Fields |
     |---|---|
     | MMIO access | virtual time, instruction count, pc, address, size, write flag, value, block |
     | IRQ source level change | virtual time, instruction count, source, level |
     | Interrupt taken | virtual time, instruction count, CPU line, pc |
     | Reset | virtual time, reset kind |
     | Console bytes | virtual time, stream, bytes |
     | Hook entry | virtual time, instruction count, symbol, hook kind |
     | Input applied | virtual time, journal sequence number |
     | Panel transport summary (board level) | virtual time, DC level, bytes digest, length |
     | Fidelity ledger event | virtual time, event |

  2. Encoding: postcard binary with a JSONL mirror.
  3. LCS per block is chosen so the diff stays robust to timing differences between oracle and emulator.
  4. The context is 20 records on each side of the first divergence.
  5. `stable_read` marks identification and configuration read-back registers only; counters and status bits are never compared.
  6. Each known-diff entry carries a citation as well as a reason. In the regi2c example QEMU reads 0xFFFFFF, which changes the values of later masked (read-modify-write) writes, so those writes differ on purpose.
  7. The call-trace diff runs before the register-stream diff and localizes where a boot phase diverges.

## fid-8.3: golden console tests

- **Design:**
  1. Normalizer: strip CR; timestamps to `(T)` with numbers kept for bands; mask compile time and date, app version, ELF SHA, Passport Keys version, `boot=` ids, MACs and the `Saved PC:` value (line kept); select the last boot after the final `ESP-ROM:` banner unless a test selects all boots.
  2. Text rule: a milestone claims a golden prefix, whose normalized lines must be equal in sequence.
  3. Band formulas for deltas (5 ms floor, 20 %) and absolute times (10 ms floor, 20 %), deltas checked first.
  4. Golden kinds `device` (77 lines, class A), `oracle` (class B), `self` (class B, provisional, cannot promote to A).
  5. Storage `tests/golden/<image-id>/<name>.console.txt` with `bands.toml`.
  6. Baseline esp32sim R8 with 65 of 77 device lines.
  7. Goldens hold normalized, identity-masked text and may be committed after the secrets check.
- **Details:**
  1. The `Saved PC:` value is masked because it depends on the instant of the USB reset.
  2. The 5 ms delta floor already absorbs the 1 ms log quantization.
  3. The 20 % relative term is a ±20 % scenario tolerance (UNVERIFIED: no public source gives it).
  4. Bands use only anchor lines present in both logs, and deltas are taken between consecutive such anchors.
  5. The double-banner test selects all boots explicitly.
  6. Of the 77 `device` golden lines, 67 carry ESP_LOG timestamps (fid-3.4 item 7).
  7. The baseline is 84.4 % (65 of 77).

## fid-8.4: register-model tests from spec tables, and the IRQ numbering pitfall

- **Design:**
  1. `xtask codegen regs` reads the 994 CSV rows and emits `RegStore` layouts plus tests of reset values per reset scope, access types (RO, W1C, W1S, WT, SC, RC), address decode including the RTC_CNTL / eFuse split slot, and overrides.
  2. Structural cross-checks: IRQ numbering by name against `interrupts.h`, GDMA C3 layout, MMU index formula and invalid bit 8.
  3. INTC property test against a 20-line reference.
  4. A MAP write re-routes at once; FROM_CPU_INTR0 is source 50 on SYSTEM 0x600C0028 bit 0.
  5. SPI2 samples DC from `GPIO_OUT` bit 20 when `CMD.usr` sets.
  6. SARADC INT_RAW bit 31 sets on the rising edge of `onetime_start`.
  7. LEDC duty is latched on `para_up`.
  8. RTCCALI reads 301176 for 1024 slow cycles.
  9. SYSTIMER counts 16 ticks per µs, VALUE_VALID after UPDATE.
  10. XMC JEDEC `20 40 17`; eFuse RS_ERR must be 0; regi2c needs a store model with BUSY polled.
  11. USJ 64-byte packets and SOF derivation; a line-state reset gives cause 0x15.
- **Details:**
  1. Numbering pitfall, checked against IDF v5.5.3: the `interrupts.h` source enum holds three alias entries (`SYSTIMER_TARGETn_EDGE` equal to `SYSTIMER_TARGETn`, n = 0 to 2) followed by an explicit `= 40` on `SPI_MEM_REJECT_CACHE`. Numbering entries by position is therefore wrong by 3 for every source after 39. Codegen evaluates enum values (aliases and explicit assignments) and cross-checks each against its MAP register, which for source s sits at 0x600C2000 + 4 x s (SYSTIMER_TARGET0 at offset 0x094 is 37; SPI_MEM_REJECT at 0x0A0 is 40). A test fails when a source number differs from its MAP offset / 4.
  2. Reference numbers: USJ 26 (MAP 0x068), RTC_CORE 27, I2C_EXT0 29, SYSTIMER_TARGET0 to 2 = 37 to 39, APB_ADC 43, DMA_CH0 to 2 = 44 to 46, RSA 47, AES 48, SHA 49, FROM_CPU_INTR0 to 3 = 50 to 53 (MAP 0x0C8 to 0x0D4), CACHE_CORE0_ACS 61 (MAP 0x0F4); 62 sources in total. Checked against IDF v5.5.3: sources 26, 27, 29, 37, 40, 43, 44, 47, 49, 50, 54 (ASSIST_DEBUG) and 61, and the total of 62.
  3. The INTC property test draws random PRI, THRESH, ENABLE, MAP, TYPE and source levels, and covers MAP-rewrite immediacy.
  4. Behavior tests written by hand from the spec tables (not generated):

     | Block | Behavior |
     |---|---|
     | `i2c0` | command-list executor with opcodes WRITE 1, STOP 2, READ 3, END 4, RESTART 6; NACK on absent addresses; `bus_busy`; the 07 scan sequence |
     | `spi2` | `CMD.update` reads 0 after the same write; `CMD.usr` walks the GDMA descriptors; `trans_done` also accepts software writes |
     | `i2s0` | `tx_update` and `rx_update` self-clear; EOF period `240/fs` (UNVERIFIED reading: 240 frames per descriptor); 6-descriptor ring |
     | `efuse` | CMD self-clears; RS_ERR reads 0; synthesized eFuse image layout |
     | `regi2c` | write word: bits 0-7 block, 8-15 register, 16-23 data, top byte 0x05; reads return stored values; BUSY reads 0 |
     | `timg` | MWDT write key 0x50D83AA1, per IDF `soc/include/soc/wdt_periph.h` (`TIMG_WDT_WKEY_VALUE`) and `hal/esp32c3/include/hal/mwdt_ll.h`; stage actions |
     | `rtc_cntl` | RWDT write key 0x50D83AA1 and super watchdog (SWD) write key 0x8F1D312A, per IDF `hal/esp32c3/include/hal/rwdt_ll.h`; RWDT and SWD belong to `rtc_cntl`, not `timg` |
     | `systimer` | an alarm set in the past fires at once; VALUE_VALID |
     | `usj` | 64-byte FIFO; WR_DONE commits; `SERIAL_IN_EMPTY` level behavior; SOF derivation; line-state reset 0x15 |
     | `flash_xmc` | JEDEC `20 40 17` returned on two reads; SR1 and SR2 read 0; legacy PP command word with address in the low 24 bits and length in the top byte; WIP timing per profile |

## fid-8.6: probe firmware list

- **Design:**
  1. Probes and what each prints and is compared against (`probe_boot_facts`, `probe_intc`, `probe_clocks`, `probe_reset`, `probe_timing`, `probe_reg_reset_<block>`, existing `probe-long` and `heapprobe`).
  2. `probe_clocks` is judged against the one-clock invariants.
  3. An approved device `probe_timing` capture gives calibration constants directly and replaces the fit.
  4. A probe runs on the device only with explicit user approval, through the planner, restoring the original app.
- **Details:**
  1. A device run of `probe_reset` also settles the SYSTIMER reset-domain UNVERIFIED, not only the SENSITIVE one.
  2. `probe_timing` is the key calibration input; without it the `device` constants rest on the single reference boot.

## fid-8.7: CPU conformance

- **Design:**
  1. `mstatus` write mask 0x00201889, UNVERIFIED beyond IDF usage.
  2. riscv-tests `rv32ui`, `rv32um`, `rv32uc` `-p` (BSD) and riscv-arch-test I, M, C, Zicsr, Zifencei, built once by `xtask riscv-tests` with the local toolchain.
  3. ESP CSR tests: mtvec forced vectored, mstatus mask, FENCE encodings as NOP, perf counters 0x7E0 to 0x7E2 and aliases 0x800 to 0x802 pausing in WFI, CSR 0x000 as a plain register, PMP TOR with lock, trigger CSRs 0x7A0 to 0x7A5.
  4. Instruction counts at `app_main` under `fast` at CPI 1 against 7.02 M (Passport Keys) and 9.94 M (official), ±2 %, informational.
- **Details:**
  1. 0x00201889 sets bits 0, 3, 7, 11, 12 and 21. In the RISC-V privileged layout these are UIE (reserved in newer spec versions), MIE, MPIE, MPP (bits 11 and 12) and TW. A write changes only these bits; the others keep their reset value. Derived from the value, UNVERIFIED on silicon.
  2. The riscv-arch-test license is BSD-3-Clause, UNVERIFIED. Small built test ELFs go into `tests/riscv/` only if the licenses allow.
  3. An instruction count outside ±2 % opens an investigation, not a failure, because poll iterations depend on model latency.

## fid-8.9: self-consistency and determinism tests

- **Design:**
  1. Run twice: equal trace digest and `state_hash`, every push 0.5 s virtual, nightly 30 s.
  2. Slice invariance over {1, 37, 4096, 10^6}.
  3. Trace on/off: equal `state_hash`.
  4. Snapshot anywhere at 20 random instruction counts, including outstanding HLE continuations and mid-DMA transfers, restored in a fresh process.
  5. Native versus wasm: equal digest and console bytes.
  6. Pause and resume at random run limits.
  7. HLE A/B prefix: hooks installed versus a run stopping at the first hook entry, identical `state_hash` there.
  8. Profile text invariance, `fast` versus `device`.
  9. Fork independence: parent digest unchanged.
- **Details:**
  1. HLE A/B prefix addendum: a tripwire-only run (radio functions not hooked, only tripwires armed) must match the HLE run up to the first radio call.
