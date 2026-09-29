/* ESP32-C3 shim over the riscv-tests `p` environment.
 *
 * `cargo xtask riscv-tests fetch-build` copies this file into the build tree with
 * @UPSTREAM_RISCV_TEST_H@ replaced by the absolute path of the pinned upstream
 * `env/p/riscv_test.h`, and puts its directory ahead of `env/p` on the include path. So the test
 * bodies, the macros of `isa/macros/scalar`, the linker script, the trap vector and the
 * pass/fail protocol are upstream's unchanged. Two hooks of the machine-mode prologue are
 * redefined, both for documented C3 properties, and `MANIFEST.toml` records the SHA-256 of this
 * file so the adjustment is part of the provenance.
 *
 * The two C3 properties:
 *
 *   1. `mtvec` is BASE in bits 31:8 with MODE hardwired to vectored, so a write keeps only a
 *      256-byte-aligned base and an exception always enters at BASE + 0 (TRM Register 1.7,
 *      CSR 0x305). Upstream puts `trap_vector` four bytes after `_start`, which masks down
 *      to `_start` itself, so every trap would re-enter `j reset_vector` and restart the test.
 *
 *   2. `mie` (0x304) is not in the TRM CSR table: the memory-mapped INTC replaces it, so a
 *      write raises illegal instruction (0x304 and 0x344). Upstream's prologue survives
 *      writes to CSRs a target lacks by pointing `mtvec` at the instruction after each one, but
 *      property 1 defeats that trick, so the write has to go instead of being caught.
 *
 * Everything else the prologue touches is fine as written: `satp` (0x180), `medeleg` (0x302),
 * `mideleg` (0x303) and the RNMI `mnstatus` (0x744) are unknown numbers, and in the lenient
 * bring-up mode (`EngineCfg::strict_csr` clear, which is what `ref_step` uses)
 * they reach `Bus::csr_custom` without trapping; `pmpaddr0` and `pmpcfg0` are implemented; and
 * `mhartid` reads its reset value 0, which is what `RISCV_MULTICORE_DISABLE` waits for.
 *
 * Nothing in any test body depends on either hook. These are user-level integer, multiply and
 * compressed tests: they run with interrupts off, take no interrupt, and reach the host only
 * through `RVTEST_PASS` and `RVTEST_FAIL`, whose `ecall` the upstream `trap_vector` turns into
 * the `tohost` store the harness watches.
 */

#include "@UPSTREAM_RISCV_TEST_H@"

/* Property 2: drop `csrwi mie, 0` and keep the rest verbatim. */
#undef DELEGATE_NO_TRAPS
#define DELEGATE_NO_TRAPS                                               \
  la t0, 1f;                                                            \
  csrw mtvec, t0;                                                       \
  csrwi medeleg, 0;                                                     \
  csrwi mideleg, 0;                                                     \
  .align 2;                                                            \
1:

/* Property 1: re-point `mtvec` at a 256-byte-aligned trampoline into upstream's `trap_vector`.
 * `EXTRA_INIT` is the upstream hook that runs at the end of `RVTEST_CODE_BEGIN`, after it has
 * installed `trap_vector` itself, so this is the last word on `mtvec` and the only entry the C3
 * can reach. Slot 0 of a vectored `mtvec` is where every exception enters, which is all these
 * tests raise. Local label 9 is unused upstream. */
#undef EXTRA_INIT
#define EXTRA_INIT                                                      \
  la t0, c3_vector_base;                                                \
  csrw mtvec, t0;                                                       \
  j 9f;                                                                 \
  .align 8;                                                             \
c3_vector_base:                                                         \
  j trap_vector;                                                        \
  .align 2;                                                            \
9:
