# Benchmark kernels of workload K

Guest sources of workload K, the kernel performance gate. `cargo run --release -p xtask -- bench-k` builds
them here, in the same job as the measurement, and runs both the block engine and the preserved
spike over the result.

| File | What it is |
|---|---|
| `fw.c` | Call-heavy, firmware-like kernel: non-inlined helpers, draw and event function-pointer tables over 40 objects on a 64x48 framebuffer, queue send and receive inside critical sections, a sorted software-timer list, a mini printf, FNV hash. **The design reference and the workload the gate is taken on.** |
| `bench.c` | Loop-heavy CPU kernel: bitwise CRC32 over 2 KB, a 20x20 matrix multiply, a 240x32 RGB565 alpha blend, an insertion sort of 160, a switch-based bytecode VM. The upper-bound workload; recorded, not gated. |
| `start.S` | Entry stub: sets `sp`, zeroes `.bss`, calls `bench_main(a0)` and leaves through `li a7, 93; ecall` with the checksum in `a0`. |
| `link.ld` | 1 MB of RAM at 0x40380000, which is SRAM1 through the instruction bus. `.text`, `.rodata` and `.data` land in one segment, so `.data` and `.bss` share the last text page: that mixed page is what puts global stores on the slow path in both engines (design-facts perf-A A.5). |

## Provenance

Copied verbatim from `workload/` of our own interpreter spike, preserved at
`<data root>/spikes/rv32-interp`. They are our sources, written for that spike; nothing
here comes from a third party, so `THIRD_PARTY.md` gains no entry. The spike's own
`workload/build.sh` is the build `xtask bench-k` reproduces:

```sh
riscv32-esp-elf-gcc -march=rv32imc_zicsr_zifencei -mabi=ilp32 -ffreestanding -nostdlib \
    -nostartfiles -fno-builtin -fno-tree-loop-distribute-patterns -Og \
    -T link.ld start.S <kernel>.c -o <kernel>-Og.elf -lgcc
riscv32-esp-elf-objcopy -O binary <kernel>-Og.elf <kernel>-Og.bin
```

with `riscv32-esp-elf-gcc` 14.2.0 of `esp-14.2.0_20251107`, the toolchain of `xtask riscv-tests`. Only the `-Og` images are used: the official AI Passport firmware builds
with `-Og`, which is why those two workloads are measured at `-Og`.

No image is committed. `xtask bench-k` writes its build to `target/bench-kernels/`, which keeps the
measured bytes tied to the toolchain that produced them in the same job.

## Correctness anchors

The kernels return a checksum in `a0`, and `xtask bench-k` refuses a run whose checksum **or whose
retired-instruction count** is wrong, so a run that is fast because it executed the wrong thing
fails instead of passing (design-facts perf-A, "Correctness anchors"):

| Workload | Iterations | Checksum | Instructions retired (spike) | Instructions retired (engine) |
|---|---|---|---|---|
| `fw-Og` | 10,000 | `0x7a19b27c` | 413,918,117 | 413,918,116 |
| `bench-Og` | 350 | `0x50f7a0a8` | 391,184,244 | 391,184,243 |

The engine runs exactly one instruction fewer than the spike on each, and `bench-k` checks that
difference rather than tolerating it: the exit `ecall` is a hook terminator here
(`xtask/src/bench_k.rs` binds `EXIT_HOOK` at it), so the engine reports `Exit::Hook` before
executing it, while the spike executes it and halts on it. Both sides run the same kernel to the
same end; only the harness's exit differs.

The counts are a property of the build, so they change when the kernel sources or the pinned
toolchain change. A `bench-k` that fails on the count is saying that the images are no longer the
ones this table anchors; regenerate the table from a run whose checksums are right, and say in the
commit why the build moved.

## Why the sources and not the images

The gate is measured in the same job on the same host as the spike `blockx` median. Building the guest from source in that job keeps both sides on one toolchain and makes the
kernel a reviewable input rather than an opaque blob. A host with no ESP toolchain cannot run the
gate; `xtask bench-k` says so instead of measuring something else.
