// probe_campaign_timing: timed facts for the silicon evidence campaign, step 1
// (specs/notes/silicon-campaign.md). MIT.
// An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Every fact line names the inventory row it settles in its `row` field (`cargo xtask probes
// compare` groups by it). Times are the measurement, so on silicon they vary from run to run; the
// emulator's record is taken under the `device` timing profile. Each timed line also carries a
// fact that shows the work was done (a sum, a digest, a count), which must be equal on both sides.
//
//   TIME|cache_code_cold, cache_code_warm   64 calls of 64 distinct flash functions, each aligned
//            to its own 64-byte block and never run before (cold), then the same 64 calls again
//            (warm): CPU cycles (the cycle counter) and SYSTIMER ticks (16 MHz) around the loop.
//            Rows cache_fill_ps (a cold line fetch) and cache_stall_counts_cycles (whether the
//            cycle counter advances during the stall: cycles against ticks x 10 at 160 MHz)
//   TIME|cache_data_cold, cache_data_warm   one word read from each of 256 consecutive 32-byte
//            lines of a 64 KB constant array in flash that nothing read before (cold), then the
//            same 256 lines again (warm); the same two counters
//   TIME|aes_cbc_<bytes>   mbedTLS AES-128-CBC encryption of 16, 1024, 4096 and 16384 bytes of a
//            fixed pattern (hardware AES, the DMA mode): microseconds and cycles, and FNV-1a 64 of
//            the ciphertext. Row aes_block_ps (the slope per 16-byte block)
//   TIME|rsa_modexp_<case>   one 2048-bit modular exponentiation on the RSA block, driven through
//            ESP-IDF's MPI HAL with the all-ones modulus (M' = 1, r = 1, as IDF's own failover
//            uses): a sparse exponent with RSA_CONSTANT_TIME 1 and 0, and a dense exponent with 0;
//            cycles from start to completion and FNV-1a 64 of the result. Rows rsa_op_ps and
//            rsa.RSA_CONSTANT_TIME
//   MPI|modmult_mprime_<right|wrong>   one 32-bit modular multiplication X x Y mod M with the
//            correct M' and with M' = 0 (every other input the same): the result against the
//            host's. Row rsa.RSA_M_PRIME ("a caller's wrong M' changes silicon's answer and not
//            this model's")
//   TIME|slow_clk_next_edge   cycles until RTC_CNTL_SLOW_CLK_NEXT_EDGE self-clears after it is set,
//            four samples. Row rtc_cntl.RTC_CNTL_SLOW_CLK_CONF
//   CAL|rtc_mux, CAL|rc_fast_d256   rtc_clk_cal over 1024 cycles of the RTC slow clock and of the
//            RC_FAST/256 clock. Row rtc_cntl.RTC_CNTL_CLK_CONF (one fixed slow-clock rate in the
//            model)
//   TIME|systimer_idf_order, systimer_mode_without_load, systimer_zero_period   SYSTIMER
//            comparator 1 (no user on this chip, interrupt disabled) in period mode: the ticks from
//            the PERIOD_MODE write and from COMP1_LOAD to the first raw interrupt and the cadence
//            after it, in ESP-IDF's load-then-mode order; the cadence after a new period written
//            with no load; whether a period of 0 fires. Rows systimer.SYSTIMER_TARGET0_CONF and
//            TARGET1_CONF (the three UNVERIFIED items of the former)
//   USJ|drain_line_<nn>, TIME|usj_drain_16x64   16 console lines of exactly 64 bytes, one IN
//            packet each, flushed one by one: microseconds from the first to the last flush. Row
//            timing-profiles.usj_drain_ps
//   REG|i2c0.<register>.<speed>   the I2C0 timing registers after ESP-IDF's master driver set the
//            bus up for 100 kHz and for 400 kHz; TIME|i2c_read_20_<speed> 20 one-byte reads of the
//            ES8311 chip id register (0xFD at 0x18) at each speed. Rows i2c0.*
//   REG|uart0.<register>.<baud>, TIME|uart0_tx_128_<baud>   128 bytes written to UART0 with the
//            ESP-IDF driver at 115200 and 921600 baud, from the first write to uart_wait_tx_done.
//            Rows uart0.UART_CLKDIV, UART_CONF0, UART_CONF1, UART_CLK_CONF
//   TIME|cpi_<class>   (printed after the lines above so they keep their numbers) one
//            IRAM loop per instruction class, 256 iterations of eight instructions of the class:
//            ALU, a branch not taken and taken, a jump, a call and return, loads from DRAM and
//            from a warm flash line with and without a use right after, a store, mul, div, a CSR
//            read, and a read and a write of a GPIO register (GPIO_IN, and GPIO_OUT_W1TC with 0,
//            which changes no pin); cpi_empty is the loop alone. Row cpi_milli: the one effective
//            CPI leaves cache-resident loops 18 % to 26 % slow, and the existing captures cannot
//            say which class carries it
//   TIME|sha256_64k_prefilled   mbedTLS SHA-256 of 64 KB already in RAM (1024 blocks, hardware
//            SHA in DMA mode): microseconds and cycles for the updates alone. Row sha_block_ps
//            (probe_timing sha256_1m mixes it with a 1 MB fill loop)
//   (placement group, printed after every line above; each names the ELF-readable code address it ran
//   and, for a DRAM kernel, the operand's address, so the SRAM placement is read off the line)
//   TIME|cpi_at_<kernel>   the four DRAM kernels left at class C (load_dram, load_dram_use,
//            load_dram_use_gap1, store_dram; the same IRAM code as cpi_<kernel>) with the operand
//            at the probe's static data and at the low end, middle and high end of the largest free
//            internal block: whether the IRAM-fetch / DRAM-data residue depends on the SRAM block
//   TIME|cpi_gap_<load|store>_gap<k>   eight DRAM loads or stores each followed by k = 0 to 3
//            unrelated ALU instructions, loop head aligned, operand static and far: the residue as a
//            function of the gap
//   TIME|cpi_flash_<kernel>   the loop alone, ALU and the four DRAM kernels from flash (a warm
//            cache line), operand static and far: whether the residue needs IRAM code
//   TIME|cpi_x_<kernel>   a CSR write (mscratch, restored), div with a zero quotient and by 1, mulhu,
//            and a store of the value the load before it read: rows the timing model charges untimed
//   TIME|cpi_mmio_read_<block>   the GPIO_IN read kernel on SYSTIMER_CONF and EXTMEM_ICACHE_CTRL
//            as well: whether the MMIO read cost is GPIO's alone
//   TIME|cpi80_<kernel>   the loop, ALU, a DRAM load and store and the two GPIO kernels with the
//            CPU at 80 MHz (APB 80 MHz): the model's APB-cycle rows predict 3 and 4 cycles an access
//   REG|extmem.<register>.app, TIME|fill_work[80]_<use|nouse>_k<k>   the cache's autoload state,
//            and one word read from each of 128 cold flash lines with an inner loop of k iterations
//            after each, then the same lines warm, at CPU 160 MHz and (fill_work80) 80 MHz: whether
//            a line fill overlaps the work after it, and what it costs at the bootloader's clock
//            (the bootloader's segment phases)
//   TIME|rom_crc32_4k[_80]   the ROM's crc32_le over 4 KB of internal RAM at CPU 160 and 80 MHz:
//            the speed of ROM-resident code, which runs a third of the bootloader's verify
//   (fetch group, printed after every line above)
//   TIME|fetch_<end|mid>_k<k>   the fetch version of fill_work: 128 cold 32-byte flash code lines,
//            each entered at its first word by a call from IRAM and left by a `c.jr ra` at the
//            line's last halfword (end, the straight run ends in word 7) or in word 3 (mid, an
//            unconditional jump mid-line), with a straight run of k `c.addi` in IRAM after each
//            (k = 0, 8, 16, 32, 64, and 128 and 256 to reach past the line's transfer), then the
//            same lines warm: which word of a cold code line the CPU waits for (the run end,
//            the entry word or the whole line) and whether it runs the line's words as they arrive
//   TIME|fetchf_mid_k<k>, TIME|fill_workf_use_k<k>   the mid fetch lines and the fill_work load
//            lines with the driver and its work in a warm flash line instead of IRAM: whether a hit
//            on another cache line waits for a line fill in progress
//   TIME|dres_<load|store>_c<cc>   eight DRAM loads (stores) a loop from IRAM, the loop head
//            stepped 0 to 64 bytes in 8-byte steps (cc) and, on each line, the operand stepped the
//            same way (fields d00 to d64), code in IRAM and data in DRAM, both in SRAM Block 1:
//            the address rule of the Block 1 DRAM residue
//   TIME|wline_<seq|jump|jump_in>[_iram]   64 calls of warm flash code (and its IRAM twin) that
//            crosses 15 lines straight through, 15 lines by jumps, or one line: what entering a
//            warm cache line costs (the application phases of the boot)
//   TIME|cpi_flash_x_<lbu|lhu|sb|sh|sw_lw_same|sw_lw_next>   byte and halfword loads and stores
//            and a load right after a store, from a warm flash line: access kinds the
//            application's flash code runs and no kernel above times
//   (replacement group, printed after every line above)
//   TIME|ways_<cyclic|pseudo|mixed>_n<n>, TIME|ways_retouch_n10   the cache's replacement policy:
//            n = 4, 6, 7, 8, 9, 10, 12 and 16 flash lines of one cache set (each holds one `ret`,
//            the lines 2048 bytes apart: the 16 KB, 8 ways and 32-byte lines of IDF's
//            esp32c3/rom/cache.h and the TRM give 64 sets), called from IRAM with interrupts off,
//            one pass cold and then 64 passes: in index order (cyclic), in a fresh xorshift32
//            Fisher-Yates order each pass (pseudo), in index order with every odd index a word
//            read of a flash constant line of the same set instead of a call (mixed: whether code
//            and data share the ways), and lines 0 to 7, 0, 1, 8, 9 each pass (retouch: a hit
//            before a miss, which true LRU, tree pseudo-LRU, FIFO and random replacement answer
//            with four distinct miss counts). Cycles of the cold pass and of the 64 passes, the
//            EXTMEM IBUS and DBUS access and miss counters over each, the set, the stride, an
//            FNV-1a 64 of the access order and the line addresses. Row cache_model
//
// Safety, checked for every step (silicon campaign step 1):
//   - no flash write or erase, no NVS; the partition table is the device's own (partitions.csv),
//     so nothing falls in or reads cardid [0x356000, 0x35A000);
//   - no eFuse access of any kind beyond what ESP-IDF's normal boot reads;
//   - no radio;
//   - no sleep;
//   - the I2C reads only read (the ES8311 chip id register, as probe_timing does); the UART0 bytes
//     leave on UART0's default TX pad, GPIO21, which the ROM already drives with its banner on
//     every boot (on the Passport it is the backlight line, so the backlight may flicker);
//   - the RSA and AES blocks are used through ESP-IDF's own locks; the RSA operations are bounded
//     by a cycle budget, and a timeout is printed rather than waited out;
//   - the cache steps only read; the timed loops run from IRAM inside a critical section of a few
//     milliseconds at most; so do the CPI loops, whose only peripheral accesses are a read of
//     GPIO_IN and writes of 0 to GPIO_OUT_W1TC;
//   - the SYSTIMER step uses comparator 1 only, with its interrupt disabled, and restores
//     SYSTIMER_CONF's work-enable bit, TARGET1_CONF and INT_ENA; units, comparator 0 (the tick) and
//     comparator 2 (esp_timer) are only read;
//   - the placement steps read only: GPIO_IN, SYSTIMER_CONF and the EXTMEM cache registers; the one
//     CSR they write is mscratch, saved before the loop and restored after it inside the critical
//     section; the CPU runs at 80 MHz for a few hundred microseconds through IDF's own
//     rtc_clk_cpu_freq_set_config and is set back to the saved configuration before the critical
//     section ends; the far DRAM operands are words of a heap block allocated and freed around
//     the step;
//   - the fetch steps only fetch their own flash code lines, read their own flash constants and
//     read and write their own DRAM words, each run inside a critical section of about a
//     millisecond at most;
//   - the replacement step only calls its own flash `ret` lines, reads its own flash constants, reads
//     the EXTMEM access and miss counters (read-only registers, never cleared) and reads a heap
//     block allocated and freed around it; each run is one critical section of about 2.5 ms at
//     most;
//   - bounded: every wait has a count or a timeout; the run takes about a second after boot and
//     ends with a DONE line.

#include <inttypes.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "driver/i2c_master.h"
#include "driver/uart.h"
#include "esp_attr.h"
#include "esp_cpu.h"
#include "esp_crypto_lock.h"
#include "esp_crypto_periph_clk.h"
#include "esp_heap_caps.h"
#include "esp_rom_crc.h"
#include "esp_timer.h"
#include "esp32c3/rom/cache.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "hal/mpi_hal.h"
#include "hal/mpi_ll.h"
#include "mbedtls/aes.h"
#include "mbedtls/sha256.h"
#include "soc/extmem_reg.h"
#include "soc/gpio_reg.h"
#include "soc/reg_base.h"
#include "soc/rtc.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"
#include "soc/systimer_reg.h"

#include "probe_line.h"

#define PROBE_NAME "probe_campaign_timing"

static bool s_ok = true;
static portMUX_TYPE s_mux = portMUX_INITIALIZER_UNLOCKED;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

static uint64_t fnv1a64(const uint8_t *p, size_t n)
{
    uint64_t h = 0xcbf29ce484222325ull;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 0x100000001b3ull;
    }
    return h;
}

// SYSTIMER unit 0, low word, 16 MHz: an update request, then the snapshot once it is valid.
// Called only inside a critical section, so esp_timer's own use of unit 0 cannot interleave.
static IRAM_ATTR uint32_t systimer_lo(void)
{
    REG_WRITE(SYSTIMER_UNIT0_OP_REG, SYSTIMER_TIMER_UNIT0_UPDATE);
    for (int i = 0; i < 1000 && !(REG_READ(SYSTIMER_UNIT0_OP_REG) & SYSTIMER_TIMER_UNIT0_VALUE_VALID);
         i++) {
    }
    return REG_READ(SYSTIMER_UNIT0_VALUE_LO_REG);
}

// ---------------------------------------------------------------------------------------------
// Cache: cold and warm line fetches of code and of constant data in flash.
// ---------------------------------------------------------------------------------------------

#define COLD_FNS 64
#define COLD_FN(n)                                                                                 \
    __attribute__((noinline, aligned(64))) static uint32_t cold_fn_##n(uint32_t x)                 \
    {                                                                                              \
        return x * (2u * (n) + 1u) + (n);                                                          \
    }
#define COLD_FN8(a)                                                                                \
    COLD_FN(a##0) COLD_FN(a##1) COLD_FN(a##2) COLD_FN(a##3) COLD_FN(a##4) COLD_FN(a##5)            \
    COLD_FN(a##6) COLD_FN(a##7)
COLD_FN8(1)
COLD_FN8(2)
COLD_FN8(3)
COLD_FN8(4)
COLD_FN8(5)
COLD_FN8(6)
COLD_FN8(7)
COLD_FN8(8)
#define COLD_REF8(a)                                                                               \
    cold_fn_##a##0, cold_fn_##a##1, cold_fn_##a##2, cold_fn_##a##3, cold_fn_##a##4,                \
        cold_fn_##a##5, cold_fn_##a##6, cold_fn_##a##7
// The table itself is in DRAM, so reading it costs no flash line (without DRAM_ATTR the compiler
// places a table nothing writes in flash rodata).
DRAM_ATTR static uint32_t (*s_cold_fns[COLD_FNS])(uint32_t) = {
    COLD_REF8(1), COLD_REF8(2), COLD_REF8(3), COLD_REF8(4),
    COLD_REF8(5), COLD_REF8(6), COLD_REF8(7), COLD_REF8(8),
};

#define DATA_LINES 256
// 64 KB of constant data in flash; only the first word is nonzero, so the sum of one word per
// line is 1 whichever lines are read.
static const uint32_t s_blob[16384] __attribute__((aligned(32))) = {1};

typedef struct {
    uint32_t cycles;
    uint32_t ticks;
    uint32_t result;
} timed_t;

static IRAM_ATTR timed_t run_calls(void)
{
    timed_t t;
    uint32_t x = 1;
    uint32_t c0 = esp_cpu_get_cycle_count();
    uint32_t s0 = systimer_lo();
    for (int i = 0; i < COLD_FNS; i++) {
        x = s_cold_fns[i](x);
    }
    t.ticks = systimer_lo() - s0;
    t.cycles = esp_cpu_get_cycle_count() - c0;
    t.result = x;
    return t;
}

static IRAM_ATTR timed_t read_lines(void)
{
    timed_t t;
    const volatile uint32_t *p = s_blob;
    uint32_t sum = 0;
    uint32_t c0 = esp_cpu_get_cycle_count();
    uint32_t s0 = systimer_lo();
    for (int i = 0; i < DATA_LINES; i++) {
        sum += p[i * 8];
    }
    t.ticks = systimer_lo() - s0;
    t.cycles = esp_cpu_get_cycle_count() - c0;
    t.result = sum;
    return t;
}

static void time_cache(void)
{
    timed_t r[4];
    portENTER_CRITICAL(&s_mux);
    r[0] = run_calls();
    r[1] = run_calls();
    r[2] = read_lines();
    r[3] = read_lines();
    portEXIT_CRITICAL(&s_mux);
    static const char *const names[] = {"cache_code_cold", "cache_code_warm", "cache_data_cold",
                                        "cache_data_warm"};
    for (int i = 0; i < 4; i++) {
        printf("TIME|%s|row=timing-profiles.cache_fill_ps,timing-profiles.cache_stall_counts_cycles"
               "|%s=%d|cycles=%" PRIu32 "|ticks=%" PRIu32 "|result=%" PRIu32 "\n",
               names[i], i < 2 ? "calls" : "lines", i < 2 ? COLD_FNS : DATA_LINES, r[i].cycles,
               r[i].ticks, r[i].result);
    }
}

// ---------------------------------------------------------------------------------------------
// AES-128-CBC through mbedTLS (hardware AES, DMA mode).
// ---------------------------------------------------------------------------------------------

static void time_aes(void)
{
    static const size_t sizes[] = {16, 1024, 4096, 16384};
    uint8_t *in = heap_caps_malloc(16384, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    uint8_t *out = heap_caps_malloc(16384, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    if (in == NULL || out == NULL) {
        fail("aes", "no 16 KB buffers");
        free(in);
        free(out);
        return;
    }
    for (size_t i = 0; i < 16384; i++) {
        in[i] = (uint8_t)(i * 7u + 3u);
    }
    static const uint8_t key[16] = {0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6,
                                    0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c};
    for (size_t k = 0; k < sizeof(sizes) / sizeof(sizes[0]); k++) {
        uint8_t iv[16];
        for (int i = 0; i < 16; i++) {
            iv[i] = (uint8_t)i;
        }
        mbedtls_aes_context ctx;
        mbedtls_aes_init(&ctx);
        int rc = mbedtls_aes_setkey_enc(&ctx, key, 128);
        int64_t t0 = esp_timer_get_time();
        uint32_t c0 = esp_cpu_get_cycle_count();
        if (rc == 0) {
            rc = mbedtls_aes_crypt_cbc(&ctx, MBEDTLS_AES_ENCRYPT, sizes[k], iv, in, out);
        }
        uint32_t cycles = esp_cpu_get_cycle_count() - c0;
        int64_t us = esp_timer_get_time() - t0;
        mbedtls_aes_free(&ctx);
        printf("TIME|aes_cbc_%u|row=timing-profiles.aes_block_ps|bytes=%u|blocks=%u|us=%" PRId64
               "|cycles=%" PRIu32 "|rc=%d|fnv=%016" PRIx64 "\n",
               (unsigned)sizes[k], (unsigned)sizes[k], (unsigned)(sizes[k] / 16), us, cycles, rc,
               fnv1a64(out, sizes[k]));
        if (rc != 0) {
            fail("aes", "mbedtls_aes_crypt_cbc failed");
        }
    }
    free(in);
    free(out);
}

// ---------------------------------------------------------------------------------------------
// RSA block: modular exponentiation timing and the M' approximation.
// ---------------------------------------------------------------------------------------------

#define RSA_WORDS 64
// A generous bound on one 2048-bit exponentiation (silicon takes tens of milliseconds at most).
#define RSA_BUDGET_CYCLES (160u * 1000u * 1000u)

// Waits for the operation to complete, bounded; returns the cycles it took or UINT32_MAX.
static uint32_t rsa_wait(uint32_t c0)
{
    while (mpi_ll_get_int_status()) {
        if (esp_cpu_get_cycle_count() - c0 > RSA_BUDGET_CYCLES) {
            return UINT32_MAX;
        }
    }
    uint32_t cycles = esp_cpu_get_cycle_count() - c0;
    mpi_hal_clear_interrupt();
    return cycles;
}

static uint32_t xorshift32(uint32_t *s)
{
    uint32_t x = *s;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *s = x;
    return x;
}

static void rsa_modexp_case(const char *name, const uint32_t *x, const uint32_t *y, bool ct)
{
    static uint32_t m[RSA_WORDS];
    static uint32_t one[RSA_WORDS];
    static uint32_t z[RSA_WORDS];
    for (int i = 0; i < RSA_WORDS; i++) {
        m[i] = 0xFFFFFFFFu;
        one[i] = i == 0 ? 1u : 0u;
    }
    mpi_hal_set_mode(RSA_WORDS - 1);
    mpi_hal_write_to_mem_block(MPI_PARAM_X, 0, x, RSA_WORDS, RSA_WORDS);
    mpi_hal_write_to_mem_block(MPI_PARAM_Y, 0, y, RSA_WORDS, RSA_WORDS);
    mpi_hal_write_to_mem_block(MPI_PARAM_M, 0, m, RSA_WORDS, RSA_WORDS);
    mpi_hal_write_to_mem_block(MPI_PARAM_Z, 0, one, RSA_WORDS, RSA_WORDS);
    mpi_hal_write_m_prime(1u);
    mpi_hal_enable_constant_time(ct);
    mpi_hal_enable_search(false);
    uint32_t c0 = esp_cpu_get_cycle_count();
    mpi_hal_start_op(MPI_MODEXP);
    uint32_t cycles = rsa_wait(c0);
    memset(z, 0, sizeof(z));
    if (cycles != UINT32_MAX) {
        mpi_ll_read_from_mem_block(z, RSA_WORDS, RSA_WORDS);
    }
    printf("TIME|rsa_modexp_%s|row=timing-profiles.rsa_op_ps,rsa.RSA_CONSTANT_TIME|words=%d"
           "|constant_time=%d|cycles=%" PRIu32 "|timeout=%d|fnv=%016" PRIx64 "\n",
           name, RSA_WORDS, ct, cycles == UINT32_MAX ? 0 : cycles, cycles == UINT32_MAX,
           fnv1a64((const uint8_t *)z, sizeof(z)));
    if (cycles == UINT32_MAX) {
        fail("rsa_modexp", "the RSA block did not complete within the cycle budget");
    }
}

// X x Y mod M for one 32-bit word, with r = R^2 mod M (R = 2^32) and the given M'.
static uint32_t rsa_modmult_1(uint32_t x, uint32_t y, uint32_t m, uint32_t r, uint32_t mprime,
                              bool *timeout)
{
    mpi_hal_set_mode(0);
    mpi_hal_write_to_mem_block(MPI_PARAM_X, 0, &x, 1, 1);
    mpi_hal_write_to_mem_block(MPI_PARAM_Y, 0, &y, 1, 1);
    mpi_hal_write_to_mem_block(MPI_PARAM_M, 0, &m, 1, 1);
    mpi_hal_write_to_mem_block(MPI_PARAM_Z, 0, &r, 1, 1);
    mpi_hal_write_m_prime(mprime);
    uint32_t c0 = esp_cpu_get_cycle_count();
    mpi_hal_start_op(MPI_MODMULT);
    *timeout = rsa_wait(c0) == UINT32_MAX;
    uint32_t z = 0;
    if (!*timeout) {
        mpi_ll_read_from_mem_block(&z, 1, 1);
    }
    return z;
}

static void time_rsa(void)
{
    static uint32_t x[RSA_WORDS];
    static uint32_t sparse[RSA_WORDS];
    static uint32_t dense[RSA_WORDS];
    uint32_t seed = 0x2545F491u;
    for (int i = 0; i < RSA_WORDS; i++) {
        x[i] = xorshift32(&seed);
        sparse[i] = 0;
        dense[i] = 0xFFFFFFFFu;
    }
    x[RSA_WORDS - 1] &= 0x7FFFFFFFu; // below the modulus
    sparse[0] = 1u;
    sparse[RSA_WORDS - 1] = 0x80000000u; // 2^2047 + 1

    esp_crypto_mpi_lock_acquire();
    esp_crypto_mpi_enable_periph_clk(true);
    mpi_hal_enable_hardware_hw_op();
    mpi_hal_interrupt_enable(false);

    rsa_modexp_case("sparse_ct1", x, sparse, true);
    rsa_modexp_case("sparse_ct0", x, sparse, false);
    rsa_modexp_case("dense_ct0", x, dense, false);

    // M = 2^32 - 5 (a prime), R = 2^32, so R mod M = 5 and r = R^2 mod M = 25.
    const uint32_t m = 0xFFFFFFFBu;
    const uint32_t xm = 0x12345678u;
    const uint32_t ym = 0x9ABCDEF1u;
    uint32_t inv = m; // m^-1 mod 2^32 by Newton's iteration
    for (int i = 0; i < 5; i++) {
        inv *= 2u - m * inv;
    }
    const uint32_t mprime = 0u - inv;
    const uint32_t want = (uint32_t)(((uint64_t)xm * ym) % m);
    bool right_timeout = false;
    bool wrong_timeout = false;
    uint32_t right = rsa_modmult_1(xm, ym, m, 25u, mprime, &right_timeout);
    uint32_t wrong = rsa_modmult_1(xm, ym, m, 25u, 0u, &wrong_timeout);

    mpi_hal_disable_hardware_hw_op();
    esp_crypto_mpi_enable_periph_clk(false);
    esp_crypto_mpi_lock_release();

    printf("MPI|modmult_mprime_right|row=rsa.RSA_M_PRIME|mprime=0x%08" PRIx32 "|want=0x%08" PRIx32
           "|got=0x%08" PRIx32 "|timeout=%d\n",
           mprime, want, right, right_timeout);
    printf("MPI|modmult_mprime_wrong|row=rsa.RSA_M_PRIME|mprime=0x00000000|want=0x%08" PRIx32
           "|got=0x%08" PRIx32 "|timeout=%d\n",
           want, wrong, wrong_timeout);
    if (right_timeout || wrong_timeout) {
        fail("rsa_modmult", "the RSA block did not complete within the cycle budget");
    }
}

// ---------------------------------------------------------------------------------------------
// RTC slow clock: the next-edge request and the calibrations.
// ---------------------------------------------------------------------------------------------

static void time_slow_edge(void)
{
    uint32_t cycles[4];
    int cleared[4];
    for (int k = 0; k < 4; k++) {
        portENTER_CRITICAL(&s_mux);
        uint32_t c0 = esp_cpu_get_cycle_count();
        REG_SET_BIT(RTC_CNTL_SLOW_CLK_CONF_REG, RTC_CNTL_SLOW_CLK_NEXT_EDGE);
        int n = 0;
        while ((REG_READ(RTC_CNTL_SLOW_CLK_CONF_REG) & RTC_CNTL_SLOW_CLK_NEXT_EDGE) && n < 100000) {
            n++;
        }
        cycles[k] = esp_cpu_get_cycle_count() - c0;
        cleared[k] = !(REG_READ(RTC_CNTL_SLOW_CLK_CONF_REG) & RTC_CNTL_SLOW_CLK_NEXT_EDGE);
        portEXIT_CRITICAL(&s_mux);
    }
    printf("TIME|slow_clk_next_edge|row=rtc_cntl.RTC_CNTL_SLOW_CLK_CONF|cycles0=%" PRIu32
           "|cycles1=%" PRIu32 "|cycles2=%" PRIu32 "|cycles3=%" PRIu32 "|cleared=%d%d%d%d\n",
           cycles[0], cycles[1], cycles[2], cycles[3], cleared[0], cleared[1], cleared[2],
           cleared[3]);
}

static void rtc_cal(void)
{
    uint32_t mux = rtc_clk_cal(RTC_CAL_RTC_MUX, 1024);
    uint32_t fast = rtc_clk_cal(RTC_CAL_8MD256, 1024);
    printf("CAL|rtc_mux|row=rtc_cntl.RTC_CNTL_CLK_CONF|cycles=1024|period_q13_19=%" PRIu32 "\n", mux);
    printf("CAL|rc_fast_d256|row=rtc_cntl.RTC_CNTL_CLK_CONF|cycles=1024|period_q13_19=%" PRIu32 "\n",
           fast);
}

// ---------------------------------------------------------------------------------------------
// SYSTIMER comparator 1: when a period-mode comparator loads (specs/blocks/systimer.toml
// SYSTIMER_TARGET0_CONF, whose three UNVERIFIED items TARGET1_CONF shares by its "same rule").
// Comparator 0 is FreeRTOS's tick and comparator 2 esp_timer's alarm; comparator 1 has no user on
// a single-core chip, and its interrupt stays disabled (only INT_RAW is polled). All times are
// SYSTIMER unit 0 ticks at 16 MHz.
// ---------------------------------------------------------------------------------------------

#define ST_PERIOD 16000u     // 1 ms
#define ST_LOAD_GAP 8000u    // 0.5 ms between COMP1_LOAD and the PERIOD_MODE write
#define ST_POLL_LIMIT 64000u // 4 ms: no wait of this step is longer

// Polls TARGET1's raw interrupt until it sets or ST_POLL_LIMIT ticks pass from `from`; returns
// the tick it was seen at, or 0 on a timeout, and clears it.
static IRAM_ATTR uint32_t st_wait_raw(uint32_t from)
{
    for (;;) {
        uint32_t now = systimer_lo();
        if (REG_READ(SYSTIMER_INT_RAW_REG) & SYSTIMER_TARGET1_INT_RAW) {
            REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
            return now == 0 ? 1 : now;
        }
        if (now - from > ST_POLL_LIMIT) {
            return 0;
        }
    }
}

static IRAM_ATTR void st_spin_until(uint32_t t)
{
    for (int n = 0; n < 1000000 && (int32_t)(systimer_lo() - t) < 0; n++) {
    }
}

typedef struct {
    uint32_t first;   // ticks from the reference write to the first raw interrupt, 0 = none
    uint32_t cadence; // ticks from the first raw interrupt to the second, 0 = none
    uint32_t from_load;
} st_result_t;

static IRAM_ATTR st_result_t st_case_idf_order(void)
{
    // ESP-IDF's order (vSystimerSetup): the period with PERIOD_MODE 0, COMP1_LOAD, then the
    // PERIOD_MODE write, here 0.5 ms after the load so the two candidate starts differ.
    st_result_t r = {0, 0, 0};
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, ST_PERIOD);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    uint32_t t_load = systimer_lo();
    REG_WRITE(SYSTIMER_COMP1_LOAD_REG, SYSTIMER_TIMER_COMP1_LOAD);
    st_spin_until(t_load + ST_LOAD_GAP);
    uint32_t t_mode = systimer_lo();
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, ST_PERIOD | SYSTIMER_TARGET1_PERIOD_MODE);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    uint32_t t1 = st_wait_raw(t_mode);
    uint32_t t2 = t1 ? st_wait_raw(t1) : 0;
    r.first = t1 ? t1 - t_mode : 0;
    r.from_load = t1 ? t1 - t_load : 0;
    r.cadence = t2 ? t2 - t1 : 0;
    return r;
}

static IRAM_ATTR st_result_t st_case_mode_without_load(void)
{
    // A new period (2 ms) written with PERIOD_MODE 0 and then 1, with no COMP1_LOAD: a cadence
    // of 2 ms says the PERIOD_MODE write loaded the comparator, 1 ms (the period of the case
    // before) says it did not.
    st_result_t r = {0, 0, 0};
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, 2u * ST_PERIOD);
    uint32_t t_mode = systimer_lo();
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, (2u * ST_PERIOD) | SYSTIMER_TARGET1_PERIOD_MODE);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    uint32_t t1 = st_wait_raw(t_mode);
    uint32_t t2 = t1 ? st_wait_raw(t1) : 0;
    r.first = t1 ? t1 - t_mode : 0;
    r.cadence = t2 ? t2 - t1 : 0;
    return r;
}

static IRAM_ATTR st_result_t st_case_zero_period(void)
{
    // Period 0 in period mode, loaded: whether the raw interrupt sets at all within 4 ms.
    st_result_t r = {0, 0, 0};
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, SYSTIMER_TARGET1_PERIOD_MODE);
    uint32_t t_load = systimer_lo();
    REG_WRITE(SYSTIMER_COMP1_LOAD_REG, SYSTIMER_TIMER_COMP1_LOAD);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    uint32_t t1 = st_wait_raw(t_load);
    uint32_t t2 = t1 ? st_wait_raw(t1) : 0;
    r.first = t1 ? t1 - t_load : 0;
    r.cadence = t2 ? t2 - t1 : 0;
    return r;
}

static void systimer_comparator(void)
{
    st_result_t a;
    st_result_t b;
    st_result_t c;
    // Each case runs in its own critical section (at most about 9 ms), so the tick and the
    // interrupt watchdog see the gaps between them.
    portENTER_CRITICAL(&s_mux);
    uint32_t conf0 = REG_READ(SYSTIMER_CONF_REG);
    uint32_t ena0 = REG_READ(SYSTIMER_INT_ENA_REG);
    REG_WRITE(SYSTIMER_INT_ENA_REG, ena0 & ~SYSTIMER_TARGET1_INT_ENA);
    REG_WRITE(SYSTIMER_CONF_REG, conf0 | SYSTIMER_TARGET1_WORK_EN);
    a = st_case_idf_order();
    portEXIT_CRITICAL(&s_mux);
    vTaskDelay(1);
    portENTER_CRITICAL(&s_mux);
    b = st_case_mode_without_load();
    portEXIT_CRITICAL(&s_mux);
    vTaskDelay(1);
    portENTER_CRITICAL(&s_mux);
    c = st_case_zero_period();
    // Back to the boot state: comparator 1 off and unconfigured, its raw interrupt cleared.
    REG_WRITE(SYSTIMER_CONF_REG, (REG_READ(SYSTIMER_CONF_REG) & ~SYSTIMER_TARGET1_WORK_EN) |
                                     (conf0 & SYSTIMER_TARGET1_WORK_EN));
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, 0);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    REG_WRITE(SYSTIMER_INT_ENA_REG, ena0);
    portEXIT_CRITICAL(&s_mux);
    printf("TIME|systimer_idf_order|row=systimer.SYSTIMER_TARGET0_CONF,systimer.SYSTIMER_TARGET1_CONF"
           "|period=%u|load_gap=%u|first_after_mode=%" PRIu32 "|first_after_load=%" PRIu32
           "|cadence=%" PRIu32 "\n",
           ST_PERIOD, ST_LOAD_GAP, a.first, a.from_load, a.cadence);
    printf("TIME|systimer_mode_without_load|row=systimer.SYSTIMER_TARGET0_CONF,"
           "systimer.SYSTIMER_TARGET1_CONF|new_period=%u|first_after_mode=%" PRIu32
           "|cadence=%" PRIu32 "\n",
           2u * ST_PERIOD, b.first, b.cadence);
    printf("TIME|systimer_zero_period|row=systimer.SYSTIMER_TARGET0_CONF,systimer.SYSTIMER_TARGET1_CONF"
           "|fired=%d|first_after_load=%" PRIu32 "|second_after_first=%" PRIu32 "\n",
           c.first != 0, c.first, c.cadence);
}

// ---------------------------------------------------------------------------------------------
// USB Serial/JTAG: how fast the host drains the 64-byte IN endpoint (timing-profiles.toml
// usj_drain_ps, one IN packet). The console is the USJ port, so the payload is itself 16 probe
// lines of exactly 64 bytes, each handed to the port with its own flush; the time from the
// first to the return of the last flush is about 15 drains of the endpoint with the
// capture host attached and reading.
// ---------------------------------------------------------------------------------------------

#define USJ_LINES 16
#define USJ_LINE_BYTES 64

static void time_usj_drain(void)
{
    char line[USJ_LINE_BYTES + 1];
    fflush(stdout);
    vTaskDelay(pdMS_TO_TICKS(50)); // what came before has left the FIFO
    int64_t t0 = esp_timer_get_time();
    for (int i = 0; i < USJ_LINES; i++) {
        int n = snprintf(line, sizeof(line), "USJ|drain_line_%02d|row=timing-profiles.usj_drain_ps|pad=",
                         i);
        memset(line + n, 'x', USJ_LINE_BYTES - 1 - n);
        line[USJ_LINE_BYTES - 1] = '\n';
        line[USJ_LINE_BYTES] = '\0';
        fwrite(line, 1, USJ_LINE_BYTES, stdout);
        fflush(stdout);
    }
    int64_t us = esp_timer_get_time() - t0;
    printf("TIME|usj_drain_16x64|row=timing-profiles.usj_drain_ps|lines=%d|bytes=%d|us=%" PRId64 "\n",
           USJ_LINES, USJ_LINES * USJ_LINE_BYTES, us);
}

// ---------------------------------------------------------------------------------------------
// I2C0: the timing registers the driver writes, and 20 reads at two bus speeds.
// ---------------------------------------------------------------------------------------------

#define PIN_I2C_SDA 10
#define PIN_I2C_SCL 7
#define ES8311_ADDR 0x18
#define ES8311_CHIP_ID_REG 0xFD
#define I2C_READS 20

static const struct {
    const char *name;
    uint32_t off;
} I2C_REGS[] = {
    {"I2C_SCL_LOW_PERIOD", 0x000},   {"I2C_TO", 0x00C},
    {"I2C_SDA_HOLD", 0x030},         {"I2C_SDA_SAMPLE", 0x034},
    {"I2C_SCL_HIGH_PERIOD", 0x038},  {"I2C_SCL_START_HOLD", 0x040},
    {"I2C_SCL_RSTART_SETUP", 0x044}, {"I2C_SCL_STOP_HOLD", 0x048},
    {"I2C_SCL_STOP_SETUP", 0x04C},   {"I2C_FILTER_CFG", 0x050},
    {"I2C_CLK_CONF", 0x054},
};

static void i2c_at(i2c_master_bus_handle_t bus, uint32_t hz, const char *speed)
{
    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = ES8311_ADDR,
        .scl_speed_hz = hz,
    };
    i2c_master_dev_handle_t dev = NULL;
    if (i2c_master_bus_add_device(bus, &dev_cfg, &dev) != ESP_OK) {
        fail("i2c", "i2c_master_bus_add_device failed");
        return;
    }
    const uint8_t reg = ES8311_CHIP_ID_REG;
    uint8_t value = 0;
    int good = 0;
    int first = -1;
    int64_t t0 = esp_timer_get_time();
    for (int i = 0; i < I2C_READS; i++) {
        if (i2c_master_transmit_receive(dev, &reg, 1, &value, 1, 100) == ESP_OK) {
            if (first < 0) {
                first = value;
            }
            good++;
        }
    }
    int64_t us = esp_timer_get_time() - t0;
    printf("TIME|i2c_read_20_%s|row=i2c0.I2C_SCL_LOW_PERIOD,i2c0.I2C_SCL_HIGH_PERIOD,"
           "i2c0.I2C_CLK_CONF|hz=%" PRIu32 "|us=%" PRId64 "|ok=%d|first=%d\n",
           speed, hz, us, good, first);
    for (size_t i = 0; i < sizeof(I2C_REGS) / sizeof(I2C_REGS[0]); i++) {
        uint32_t addr = DR_REG_I2C_EXT_BASE + I2C_REGS[i].off;
        printf("REG|i2c0.%s.%s|row=i2c0.%s|addr=0x%08" PRIx32 "|val=0x%08" PRIx32 "\n",
               I2C_REGS[i].name, speed, I2C_REGS[i].name, addr, REG_READ(addr));
    }
    if (good != I2C_READS) {
        fail("i2c", "not every I2C read succeeded");
    }
    i2c_master_bus_rm_device(dev);
}

static void time_i2c(void)
{
    i2c_master_bus_config_t bus_cfg = {
        .i2c_port = 0,
        .sda_io_num = PIN_I2C_SDA,
        .scl_io_num = PIN_I2C_SCL,
        .clk_source = I2C_CLK_SRC_DEFAULT,
        .glitch_ignore_cnt = 7,
        .flags.enable_internal_pullup = 1,
    };
    i2c_master_bus_handle_t bus = NULL;
    if (i2c_new_master_bus(&bus_cfg, &bus) != ESP_OK) {
        fail("i2c", "I2C0 setup failed");
        return;
    }
    i2c_at(bus, 100000, "100k");
    i2c_at(bus, 400000, "400k");
    i2c_del_master_bus(bus);
}

// ---------------------------------------------------------------------------------------------
// UART0: 128 bytes at two baud rates.
// ---------------------------------------------------------------------------------------------

static const struct {
    const char *name;
    uint32_t off;
} UART_REGS[] = {
    {"UART_CLKDIV", 0x014},
    {"UART_CONF0", 0x020},
    {"UART_CONF1", 0x024},
    {"UART_CLK_CONF", 0x078},
};

static void uart_at(uint32_t baud, const uint8_t *bytes, size_t n)
{
    if (uart_set_baudrate(UART_NUM_0, baud) != ESP_OK) {
        fail("uart0", "uart_set_baudrate failed");
        return;
    }
    int64_t t0 = esp_timer_get_time();
    int written = uart_write_bytes(UART_NUM_0, bytes, n);
    esp_err_t rc = uart_wait_tx_done(UART_NUM_0, pdMS_TO_TICKS(200));
    int64_t us = esp_timer_get_time() - t0;
    printf("TIME|uart0_tx_128_%" PRIu32 "|row=uart0.UART_CLKDIV|baud=%" PRIu32 "|us=%" PRId64
           "|written=%d|rc=%d\n",
           baud, baud, us, written, rc);
    for (size_t i = 0; i < sizeof(UART_REGS) / sizeof(UART_REGS[0]); i++) {
        uint32_t addr = DR_REG_UART_BASE + UART_REGS[i].off;
        printf("REG|uart0.%s.%" PRIu32 "|row=uart0.%s|addr=0x%08" PRIx32 "|val=0x%08" PRIx32 "\n",
               UART_REGS[i].name, baud, UART_REGS[i].name, addr, REG_READ(addr));
    }
}

static void time_uart0(void)
{
    uart_config_t cfg = {
        .baud_rate = 115200,
        .data_bits = UART_DATA_8_BITS,
        .parity = UART_PARITY_DISABLE,
        .stop_bits = UART_STOP_BITS_1,
        .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
        .source_clk = UART_SCLK_DEFAULT,
    };
    if (uart_driver_install(UART_NUM_0, 256, 0, 0, NULL, 0) != ESP_OK ||
        uart_param_config(UART_NUM_0, &cfg) != ESP_OK) {
        fail("uart0", "UART0 driver setup failed");
        return;
    }
    uint8_t bytes[128];
    for (size_t i = 0; i < sizeof(bytes); i++) {
        bytes[i] = (uint8_t)('0' + i % 10u);
    }
    uart_at(115200, bytes, sizeof(bytes));
    uart_at(921600, bytes, sizeof(bytes));
    uart_driver_delete(UART_NUM_0);
}

// ---------------------------------------------------------------------------------------------
// CPI per instruction class: one IRAM loop per class.
// ---------------------------------------------------------------------------------------------

// Every kernel runs its body CPI_ITERS times in a counted loop from IRAM and returns the cycles
// from before the loop to after it. The body is eight instructions of one class (sixteen or
// twenty-four where a class needs a partner, such as a load and the add that uses it); the loop
// is `addi` and a `bnez` taken every time but the last. `cpi_empty` is the loop alone, so
// (cycles - empty) / (CPI_ITERS x 8) is the cost of one instruction of the class. Operands: %3 an
// aligned DRAM word, %4 a flash constant (DROM, warmed by a first run), %5 GPIO_IN_REG (read
// only), %6 GPIO_OUT_W1TC_REG (written with 0, which changes no pin).
#define CPI_ITERS 256
#define CPI_REP8(x) x x x x x x x x

__asm__(".section .iram1.cpi_leaf,\"ax\",@progbits\n"
        ".global cpi_leaf\n"
        ".type cpi_leaf,@function\n"
        "cpi_leaf:\n"
        "  ret\n"
        ".size cpi_leaf, .-cpi_leaf\n"
        ".previous\n");

#define CPI_KERNEL(name, pre, body)                                                                \
    static IRAM_ATTR __attribute__((noinline)) uint32_t cpi_##name(                              \
        volatile uint32_t *ram, const volatile uint32_t *rom, const volatile uint32_t *mmio_in,    \
        volatile uint32_t *mmio_out)                                                               \
    {                                                                                              \
        uint32_t c0, c1, n = CPI_ITERS;                                                            \
        __asm__ volatile(pre "csrr %0, 0x7e2\n"                                                    \
                             "1:\n" body "addi %2, %2, -1\n"                                       \
                             "bnez %2, 1b\n"                                                       \
                             "2:\n"                                                                \
                             "csrr %1, 0x7e2\n"                                                    \
                         : "=&r"(c0), "=&r"(c1), "+r"(n)                                           \
                         : "r"(ram), "r"(rom), "r"(mmio_in), "r"(mmio_out)                         \
                         : "t0", "t1", "t2", "ra", "memory");                                      \
        return c1 - c0;                                                                            \
    }

CPI_KERNEL(empty, "", "")
CPI_KERNEL(alu, "", CPI_REP8("addi t0, t0, 1\n"))
CPI_KERNEL(branch_not_taken, "", CPI_REP8("bne t0, t0, 2f\n"))
CPI_KERNEL(branch_taken, "", CPI_REP8("beq t0, t0, 3f\n3:\n"))
CPI_KERNEL(jump, "", CPI_REP8("j 3f\n3:\n"))
CPI_KERNEL(call_ret, "", CPI_REP8("jal ra, cpi_leaf\n"))
CPI_KERNEL(load_dram, "", CPI_REP8("lw t0, 0(%3)\n"))
CPI_KERNEL(load_dram_use, "", CPI_REP8("lw t0, 0(%3)\nadd t1, t1, t0\n"))
CPI_KERNEL(load_dram_use_gap1, "", CPI_REP8("lw t0, 0(%3)\naddi t2, t2, 1\nadd t1, t1, t0\n"))
CPI_KERNEL(load_flash, "", CPI_REP8("lw t0, 0(%4)\n"))
CPI_KERNEL(load_flash_use, "", CPI_REP8("lw t0, 0(%4)\nadd t1, t1, t0\n"))
CPI_KERNEL(store_dram, "", CPI_REP8("sw t0, 0(%3)\n"))
CPI_KERNEL(mul, "li t1, 3\n", CPI_REP8("mul t0, t0, t1\n"))
CPI_KERNEL(div, "li t1, 1000000\nli t2, 7\n", CPI_REP8("div t0, t1, t2\n"))
CPI_KERNEL(csr_read, "", CPI_REP8("csrr t0, 0x7e2\n"))
CPI_KERNEL(mmio_read, "", CPI_REP8("lw t0, 0(%5)\n"))
CPI_KERNEL(mmio_write, "", CPI_REP8("sw zero, 0(%6)\n"))

typedef uint32_t (*cpi_kernel_t)(volatile uint32_t *, const volatile uint32_t *,
                                 const volatile uint32_t *, volatile uint32_t *);

// A flash constant for the DROM loads (only its first word is read).
static const uint32_t s_cpi_rom[8] __attribute__((aligned(32))) = {7};

static void time_cpi(void)
{
    static const struct {
        const char *name;
        cpi_kernel_t fn;
        int insns; // per iteration, the loop's two included
    } kernels[] = {
        {"empty", cpi_empty, 2},
        {"alu", cpi_alu, 10},
        {"branch_not_taken", cpi_branch_not_taken, 10},
        {"branch_taken", cpi_branch_taken, 10},
        {"jump", cpi_jump, 10},
        {"call_ret", cpi_call_ret, 18},
        {"load_dram", cpi_load_dram, 10},
        {"load_dram_use", cpi_load_dram_use, 18},
        {"load_dram_use_gap1", cpi_load_dram_use_gap1, 26},
        {"load_flash", cpi_load_flash, 10},
        {"load_flash_use", cpi_load_flash_use, 18},
        {"store_dram", cpi_store_dram, 10},
        {"mul", cpi_mul, 10},
        {"div", cpi_div, 10},
        {"csr_read", cpi_csr_read, 10},
        {"mmio_read", cpi_mmio_read, 10},
        {"mmio_write", cpi_mmio_write, 10},
    };
    enum { N = sizeof(kernels) / sizeof(kernels[0]) };
    static DRAM_ATTR uint32_t ram_word __attribute__((aligned(4)));
    uint32_t cycles[N];
    portENTER_CRITICAL(&s_mux);
    for (int i = 0; i < N; i++) {
        // The first run warms the flash line of s_cpi_rom; the second is the one printed.
        kernels[i].fn(&ram_word, s_cpi_rom, (const volatile uint32_t *)GPIO_IN_REG,
                      (volatile uint32_t *)GPIO_OUT_W1TC_REG);
        cycles[i] = kernels[i].fn(&ram_word, s_cpi_rom, (const volatile uint32_t *)GPIO_IN_REG,
                                  (volatile uint32_t *)GPIO_OUT_W1TC_REG);
    }
    portEXIT_CRITICAL(&s_mux);
    for (int i = 0; i < N; i++) {
        printf("TIME|cpi_%s|row=timing-profiles.cpi_milli|iters=%d|insns=%d|cycles=%" PRIu32 "\n",
               kernels[i].name, CPI_ITERS, kernels[i].insns, cycles[i]);
    }
}

// SHA-256 of 64 KB already in RAM: 1024 blocks through mbedTLS (the hardware SHA,
// DMA mode), timed without the fill loop that dominates probe_timing sha256_1m, so the engine's
// time per block (row sha_block_ps) is read apart from the CPU's. FNV-1a 64 of the digest shows
// the work was done.
#define SHA_PREFILLED_CHUNK 4096
#define SHA_PREFILLED_CHUNKS 16

static void time_sha_prefilled(void)
{
    uint8_t *chunk = heap_caps_malloc(SHA_PREFILLED_CHUNK, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    if (chunk == NULL) {
        fail("sha256_64k_prefilled", "no 4 KB buffer");
        return;
    }
    for (int i = 0; i < SHA_PREFILLED_CHUNK; i++) {
        chunk[i] = (uint8_t)(i * 131u + 3u);
    }
    uint8_t digest[32];
    mbedtls_sha256_context ctx;
    mbedtls_sha256_init(&ctx);
    int rc = mbedtls_sha256_starts(&ctx, 0);
    uint32_t c0 = esp_cpu_get_cycle_count();
    int64_t t0 = esp_timer_get_time();
    for (int i = 0; rc == 0 && i < SHA_PREFILLED_CHUNKS; i++) {
        rc = mbedtls_sha256_update(&ctx, chunk, SHA_PREFILLED_CHUNK);
    }
    int64_t us = esp_timer_get_time() - t0;
    uint32_t cycles = esp_cpu_get_cycle_count() - c0;
    if (rc == 0) {
        rc = mbedtls_sha256_finish(&ctx, digest);
    }
    mbedtls_sha256_free(&ctx);
    printf("TIME|sha256_64k_prefilled|row=timing-profiles.sha_block_ps|blocks=%d|us=%lld"
           "|cycles=%" PRIu32 "|rc=%d|fnv=%016llx\n",
           SHA_PREFILLED_CHUNK * SHA_PREFILLED_CHUNKS / 64, (long long)us, cycles, rc,
           rc == 0 ? (unsigned long long)fnv1a64(digest, sizeof digest) : 0ull);
    if (rc != 0) {
        fail("sha256_64k_prefilled", "mbedtls_sha256 failed");
    }
    free(chunk);
}

// ---------------------------------------------------------------------------------------------
// Placement group: the kernels left at class C, printed after every line above.
// ---------------------------------------------------------------------------------------------

// The new kernels put their loop head on a 4-byte boundary (`.balign 4`, a `c.nop` executed once
// when the pad is needed), so the timing model's redirect rule is the same for every one of them, and
// take a `post` that runs after the second counter read. Operands as CPI_KERNEL's.
#define CPI_LOOP_ASM(pre, body, post)                                                              \
    __asm__ volatile(pre "csrr %0, 0x7e2\n"                                                        \
                         ".balign 4\n"                                                             \
                         "1:\n" body "addi %2, %2, -1\n"                                           \
                         "bnez %2, 1b\n"                                                           \
                         "2:\n"                                                                    \
                         "csrr %1, 0x7e2\n" post                                                   \
                     : "=&r"(c0), "=&r"(c1), "+r"(n)                                               \
                     : "r"(ram), "r"(rom), "r"(mmio_in), "r"(mmio_out)                             \
                     : "t0", "t1", "t2", "ra", "memory")

// One kernel in IRAM (cpi_<name>) and one in flash (cpif_<name>, run from a warm cache line).
#define CPI_KERNEL_AL(name, pre, body, post)                                                       \
    static IRAM_ATTR __attribute__((noinline)) uint32_t cpi_##name(                              \
        volatile uint32_t *ram, const volatile uint32_t *rom, const volatile uint32_t *mmio_in,    \
        volatile uint32_t *mmio_out)                                                               \
    {                                                                                              \
        uint32_t c0, c1, n = CPI_ITERS;                                                            \
        CPI_LOOP_ASM(pre, body, post);                                                             \
        return c1 - c0;                                                                            \
    }
#define CPI_KERNEL_FLASH(name, pre, body)                                                          \
    static __attribute__((noinline)) uint32_t cpif_##name(                                       \
        volatile uint32_t *ram, const volatile uint32_t *rom, const volatile uint32_t *mmio_in,    \
        volatile uint32_t *mmio_out)                                                               \
    {                                                                                              \
        uint32_t c0, c1, n = CPI_ITERS;                                                            \
        CPI_LOOP_ASM(pre, body, "");                                                               \
        return c1 - c0;                                                                            \
    }

#define LW "lw t0, 0(%3)\n"
#define SW "sw t0, 0(%3)\n"
#define GAP "addi t2, t2, 1\n"

// Item 3: a DRAM load or store and 0 to 3 unrelated ALU instructions after it, eight times.
CPI_KERNEL_AL(load_gap0, "", CPI_REP8(LW), "")
CPI_KERNEL_AL(load_gap1, "", CPI_REP8(LW GAP), "")
CPI_KERNEL_AL(load_gap2, "", CPI_REP8(LW GAP GAP), "")
CPI_KERNEL_AL(load_gap3, "", CPI_REP8(LW GAP GAP GAP), "")
CPI_KERNEL_AL(store_gap0, "", CPI_REP8(SW), "")
CPI_KERNEL_AL(store_gap1, "", CPI_REP8(SW GAP), "")
CPI_KERNEL_AL(store_gap2, "", CPI_REP8(SW GAP GAP), "")
CPI_KERNEL_AL(store_gap3, "", CPI_REP8(SW GAP GAP GAP), "")

// Item 2: the loop alone, ALU and the four class C DRAM kernels, from flash.
CPI_KERNEL_FLASH(empty, "", "")
CPI_KERNEL_FLASH(alu, "", CPI_REP8("addi t0, t0, 1\n"))
CPI_KERNEL_FLASH(load_dram, "", CPI_REP8(LW))
CPI_KERNEL_FLASH(load_dram_use, "", CPI_REP8(LW "add t1, t1, t0\n"))
CPI_KERNEL_FLASH(load_dram_use_gap1, "", CPI_REP8(LW GAP "add t1, t1, t0\n"))
CPI_KERNEL_FLASH(store_dram, "", CPI_REP8(SW))

// Item 5: the rows the timing model charges without a kernel. A CSR write (mscratch, saved before and
// restored after); `div` with a zero quotient and by 1 (the divider's latency was one operand
// pair's); `mulhu` (only `mul` was timed); a store of the value the load before it read (taken to
// be a use).
CPI_KERNEL_AL(csr_write, "csrr t2, mscratch\n", CPI_REP8("csrw mscratch, t0\n"),
              "csrw mscratch, t2\n")
CPI_KERNEL_AL(div_q0, "li t1, 7\nli t2, 1000000\n", CPI_REP8("div t0, t1, t2\n"), "")
CPI_KERNEL_AL(div_by1, "li t1, 1000000\nli t2, 1\n", CPI_REP8("div t0, t1, t2\n"), "")
CPI_KERNEL_AL(mulhu, "li t1, 3\n", CPI_REP8("mulhu t0, t0, t1\n"), "")
CPI_KERNEL_AL(load_store_data, "", CPI_REP8(LW "sw t0, 4(%3)\n"), "")

// A cold flash line with work after it (the bootloader's segment
// phases, which read the image through the cache with about 600 cycles of work a 64-byte block):
// one word from each of FW_LINES consecutive 32-byte lines nothing read before, an inner loop of
// `k` iterations after each (about 4k cycles), then the same lines again (warm). A blocking fill
// costs cold - warm = FW_LINES x the fill whatever `k` is; a fill that overlaps the work (the
// cache loading the next line, or a load that does not wait) costs less as `k` grows.
#define FW_LINES 128
#define FW_RUNS 6
static const uint32_t s_fill_work[FW_RUNS][FW_LINES * 8] __attribute__((aligned(32))) = {{1}};

#define FILL_WORK(name, use) FILL_WORK_AT(IRAM_ATTR, fill_work_##name, use)
#define FILL_WORK_AT(attr, fn, use)                                                                \
    static attr __attribute__((noinline)) uint32_t fn(                                             \
        const volatile uint32_t *p, uint32_t k, uint32_t *sum_out)                                 \
    {                                                                                              \
        uint32_t c0, c1, sum = 0, lines = FW_LINES;                                                \
        __asm__ volatile("csrr %0, 0x7e2\n"                                                        \
                         ".balign 4\n"                                                             \
                         "1:\n"                                                                    \
                         "lw t0, 0(%3)\n" use "addi %3, %3, 32\n"                                  \
                         "mv t1, %5\n"                                                             \
                         "beqz t1, 3f\n"                                                           \
                         "2:\n"                                                                    \
                         "addi t1, t1, -1\n"                                                       \
                         "bnez t1, 2b\n"                                                           \
                         "3:\n"                                                                    \
                         "addi %4, %4, -1\n"                                                       \
                         "bnez %4, 1b\n"                                                           \
                         "csrr %1, 0x7e2\n"                                                        \
                         : "=&r"(c0), "=&r"(c1), "+r"(sum), "+r"(p), "+r"(lines)                   \
                         : "r"(k)                                                                  \
                         : "t0", "t1", "memory");                                                  \
        *sum_out = sum;                                                                            \
        return c1 - c0;                                                                            \
    }
FILL_WORK(use, "add %2, %2, t0\n")
FILL_WORK(nouse, "")

// Runs a kernel twice (the first warms a flash kernel's lines and the DROM operand) and returns
// the second count. Called inside s_mux.
static IRAM_ATTR uint32_t run2(cpi_kernel_t fn, volatile uint32_t *ram, const volatile uint32_t *mmio_in)
{
    fn(ram, s_cpi_rom, mmio_in, (volatile uint32_t *)GPIO_OUT_W1TC_REG);
    return fn(ram, s_cpi_rom, mmio_in, (volatile uint32_t *)GPIO_OUT_W1TC_REG);
}

typedef struct {
    const char *name;
    cpi_kernel_t fn;
    int insns; // per iteration, the loop's two included
} kernel_t;

// A DRAM operand beside the probe's other static data (the placement of ram_word in time_cpi),
// with a second word for load_store_data.
static DRAM_ATTR uint32_t s_ram_near[2] __attribute__((aligned(8)));

static void print_kernel(const char *tag, const kernel_t *k, const char *where, uint32_t cycles,
                         const volatile void *data)
{
    printf("TIME|%s_%s|row=timing-profiles.cpi_milli|iters=%d|insns=%d|data=%s|data_addr=0x%08" PRIx32
           "|code_addr=0x%08" PRIx32 "|cycles=%" PRIu32 "\n",
           tag, k->name, CPI_ITERS, k->insns, where, (uint32_t)(uintptr_t)data,
           (uint32_t)(uintptr_t)k->fn, cycles);
}

// Items 1 to 3 and 5: the DRAM kernels with their operand in SRAM away from the code, the
// ALU-gap kernels, the flash-resident kernels and the uncharged rows, at CPU 160 MHz.
static void time_cpi_placement(void)
{
    static const kernel_t class_c[] = {
        {"load_dram", cpi_load_dram, 10},
        {"load_dram_use", cpi_load_dram_use, 18},
        {"load_dram_use_gap1", cpi_load_dram_use_gap1, 26},
        {"store_dram", cpi_store_dram, 10},
    };
    static const kernel_t gaps[] = {
        {"load_gap0", cpi_load_gap0, 10},   {"load_gap1", cpi_load_gap1, 18},
        {"load_gap2", cpi_load_gap2, 26},   {"load_gap3", cpi_load_gap3, 34},
        {"store_gap0", cpi_store_gap0, 10}, {"store_gap1", cpi_store_gap1, 18},
        {"store_gap2", cpi_store_gap2, 26}, {"store_gap3", cpi_store_gap3, 34},
    };
    static const kernel_t flash[] = {
        {"empty", cpif_empty, 2},
        {"alu", cpif_alu, 10},
        {"load_dram", cpif_load_dram, 10},
        {"load_dram_use", cpif_load_dram_use, 18},
        {"load_dram_use_gap1", cpif_load_dram_use_gap1, 26},
        {"store_dram", cpif_store_dram, 10},
    };
    static const kernel_t extra[] = {
        {"csr_write", cpi_csr_write, 10},
        {"div_q0", cpi_div_q0, 10},
        {"div_by1", cpi_div_by1, 10},
        {"mulhu", cpi_mulhu, 10},
        {"load_store_data", cpi_load_store_data, 18},
    };
    enum { NC = 4, NG = 8, NF = 6, NX = 5, NP = 4 };

    // Item 1: the operand at the probe's static data and at three points of the largest free
    // internal block, which spans a large part of SRAM1 away from the IRAM code.
    size_t largest = heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    size_t size = largest > 256 ? (largest - 128) & ~(size_t)63 : 0;
    uint8_t *block = size ? heap_caps_malloc(size, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT) : NULL;
    if (block == NULL) {
        fail("cpi_placement", "no internal block for the far operands");
        return;
    }
    struct {
        const char *name;
        volatile uint32_t *ram;
    } places[NP] = {
        {"static", s_ram_near},
        {"heap_low", (volatile uint32_t *)(((uintptr_t)block + 63) & ~(uintptr_t)63)},
        {"heap_mid", (volatile uint32_t *)(((uintptr_t)block + size / 2) & ~(uintptr_t)63)},
        {"heap_high", (volatile uint32_t *)(((uintptr_t)block + size - 64) & ~(uintptr_t)63)},
    };
    for (int p = 0; p < NP; p++) {
        places[p].ram[0] = 0;
        places[p].ram[1] = 0;
    }

    uint32_t c_class[NP][NC];
    uint32_t c_gaps[2][NG];
    uint32_t c_flash[2][NF];
    uint32_t c_extra[NX];
    const volatile uint32_t *gpio_in = (const volatile uint32_t *)GPIO_IN_REG;
    portENTER_CRITICAL(&s_mux);
    for (int p = 0; p < NP; p++) {
        for (int i = 0; i < NC; i++) {
            c_class[p][i] = run2(class_c[i].fn, places[p].ram, gpio_in);
        }
    }
    for (int w = 0; w < 2; w++) {
        volatile uint32_t *ram = places[w == 0 ? 0 : NP - 1].ram;
        for (int i = 0; i < NG; i++) {
            c_gaps[w][i] = run2(gaps[i].fn, ram, gpio_in);
        }
        for (int i = 0; i < NF; i++) {
            c_flash[w][i] = run2(flash[i].fn, ram, gpio_in);
        }
    }
    for (int i = 0; i < NX; i++) {
        c_extra[i] = run2(extra[i].fn, s_ram_near, gpio_in);
    }
    portEXIT_CRITICAL(&s_mux);

    for (int p = 0; p < NP; p++) {
        for (int i = 0; i < NC; i++) {
            print_kernel("cpi_at", &class_c[i], places[p].name, c_class[p][i], places[p].ram);
        }
    }
    for (int w = 0; w < 2; w++) {
        int p = w == 0 ? 0 : NP - 1;
        for (int i = 0; i < NG; i++) {
            print_kernel("cpi_gap", &gaps[i], places[p].name, c_gaps[w][i], places[p].ram);
        }
    }
    for (int w = 0; w < 2; w++) {
        int p = w == 0 ? 0 : NP - 1;
        for (int i = 0; i < NF; i++) {
            print_kernel("cpi_flash", &flash[i], places[p].name, c_flash[w][i], places[p].ram);
        }
    }
    for (int i = 0; i < NX; i++) {
        print_kernel("cpi_x", &extra[i], "static", c_extra[i], s_ram_near);
    }
    free(block);
}

// Item 5, the peripheral cost: the GPIO_IN read kernel (the same code as cpi_mmio_read) on a
// register of two other blocks, SYSTIMER_CONF (APB) and EXTMEM_ICACHE_CTRL (the cache
// controller's block), both read only.
static void time_cpi_mmio_blocks(void)
{
    static const struct {
        const char *name;
        uint32_t addr;
    } regs[] = {
        {"gpio_in", GPIO_IN_REG},
        {"systimer_conf", SYSTIMER_CONF_REG},
        {"extmem_icache_ctrl", EXTMEM_ICACHE_CTRL_REG},
    };
    enum { N = sizeof(regs) / sizeof(regs[0]) };
    uint32_t cycles[N];
    portENTER_CRITICAL(&s_mux);
    for (int i = 0; i < N; i++) {
        cycles[i] = run2(cpi_mmio_read, s_ram_near, (const volatile uint32_t *)regs[i].addr);
    }
    portEXIT_CRITICAL(&s_mux);
    for (int i = 0; i < N; i++) {
        printf("TIME|cpi_mmio_read_%s|row=timing-profiles.mmio_load_apb_cycles|iters=%d|insns=10"
               "|addr=0x%08" PRIx32 "|cycles=%" PRIu32 "\n",
               regs[i].name, CPI_ITERS, regs[i].addr, cycles[i]);
    }
}

// Item 4: the same IRAM kernels with the CPU at 80 MHz (APB stays 80 MHz), switched with IDF's
// own call inside the critical section and switched back before it ends. The timing model predicts the
// MMIO read and write at 3 and 4 CPU cycles an access here, against 6 and 8 at 160 MHz.
static void time_cpi_80mhz(void)
{
    static const kernel_t k80[] = {
        {"empty", cpi_empty, 2},           {"alu", cpi_alu, 10},
        {"load_dram", cpi_load_dram, 10},  {"store_dram", cpi_store_dram, 10},
        {"mmio_read", cpi_mmio_read, 10},  {"mmio_write", cpi_mmio_write, 10},
    };
    enum { N = sizeof(k80) / sizeof(k80[0]) };
    rtc_cpu_freq_config_t old;
    rtc_cpu_freq_config_t slow;
    rtc_cpu_freq_config_t during;
    rtc_clk_cpu_freq_get_config(&old);
    if (!rtc_clk_cpu_freq_mhz_to_config(80, &slow)) {
        fail("cpi_80mhz", "no 80 MHz CPU configuration");
        return;
    }
    uint32_t cycles[N];
    uint32_t apb_hz;
    portENTER_CRITICAL(&s_mux);
    rtc_clk_cpu_freq_set_config(&slow);
    rtc_clk_cpu_freq_get_config(&during);
    apb_hz = rtc_clk_apb_freq_get();
    for (int i = 0; i < N; i++) {
        cycles[i] = run2(k80[i].fn, s_ram_near, (const volatile uint32_t *)GPIO_IN_REG);
    }
    rtc_clk_cpu_freq_set_config(&old);
    portEXIT_CRITICAL(&s_mux);
    for (int i = 0; i < N; i++) {
        printf("TIME|cpi80_%s|row=timing-profiles.mmio_load_apb_cycles,"
               "timing-profiles.mmio_store_apb_cycles|cpu_mhz=%" PRIu32 "|apb_hz=%" PRIu32
               "|iters=%d|insns=%d|cycles=%" PRIu32 "\n",
               k80[i].name, during.freq_mhz, apb_hz, CPI_ITERS, k80[i].insns, cycles[i]);
    }
    if (during.freq_mhz != 80) {
        fail("cpi_80mhz", "the CPU did not run at 80 MHz");
    }
}

// Part C: the cache's autoload state, then the cold line reads with work between them.
static void time_fill_work(void)
{
    static const struct {
        const char *name;
        uint32_t addr;
    } regs[] = {
        {"EXTMEM_ICACHE_CTRL", EXTMEM_ICACHE_CTRL_REG},
        {"EXTMEM_ICACHE_PRELOAD_CTRL", EXTMEM_ICACHE_PRELOAD_CTRL_REG},
        {"EXTMEM_ICACHE_AUTOLOAD_CTRL", EXTMEM_ICACHE_AUTOLOAD_CTRL_REG},
        {"EXTMEM_ICACHE_AUTOLOAD_SCT0_ADDR", EXTMEM_ICACHE_AUTOLOAD_SCT0_ADDR_REG},
        {"EXTMEM_ICACHE_AUTOLOAD_SCT0_SIZE", EXTMEM_ICACHE_AUTOLOAD_SCT0_SIZE_REG},
        {"EXTMEM_ICACHE_AUTOLOAD_SCT1_ADDR", EXTMEM_ICACHE_AUTOLOAD_SCT1_ADDR_REG},
        {"EXTMEM_ICACHE_AUTOLOAD_SCT1_SIZE", EXTMEM_ICACHE_AUTOLOAD_SCT1_SIZE_REG},
    };
    for (size_t i = 0; i < sizeof(regs) / sizeof(regs[0]); i++) {
        printf("REG|extmem.%s.app|row=extmem.%s|addr=0x%08" PRIx32 "|val=0x%08" PRIx32 "\n",
               regs[i].name, regs[i].name, regs[i].addr, REG_READ(regs[i].addr));
    }
    // The last two runs at CPU 80 MHz, the second-stage bootloader's clock, where the same fill
    // is half as many CPU cycles if the flash alone paces it.
    static const struct {
        uint32_t k;
        bool use;
        bool slow;
    } runs[FW_RUNS] = {{0, true, false},  {32, true, false}, {96, true, false},
                       {96, false, false}, {0, true, true},   {48, true, true}};
    uint32_t cold[FW_RUNS];
    uint32_t warm[FW_RUNS];
    uint32_t sum[FW_RUNS];
    uint32_t mhz[FW_RUNS];
    uint32_t sum_warm = 0;
    rtc_cpu_freq_config_t old;
    rtc_cpu_freq_config_t slow;
    rtc_cpu_freq_config_t during;
    rtc_clk_cpu_freq_get_config(&old);
    bool have_slow = rtc_clk_cpu_freq_mhz_to_config(80, &slow);
    if (!have_slow) {
        fail("fill_work", "no 80 MHz CPU configuration");
    }
    portENTER_CRITICAL(&s_mux);
    for (int r = 0; r < FW_RUNS; r++) {
        if (runs[r].slow && !have_slow) {
            cold[r] = warm[r] = sum[r] = mhz[r] = 0;
            continue;
        }
        if (runs[r].slow) {
            rtc_clk_cpu_freq_set_config(&slow);
        }
        rtc_clk_cpu_freq_get_config(&during);
        mhz[r] = during.freq_mhz;
        const volatile uint32_t *p = s_fill_work[r];
        if (runs[r].use) {
            cold[r] = fill_work_use(p, runs[r].k, &sum[r]);
            warm[r] = fill_work_use(p, runs[r].k, &sum_warm);
        } else {
            cold[r] = fill_work_nouse(p, runs[r].k, &sum[r]);
            warm[r] = fill_work_nouse(p, runs[r].k, &sum_warm);
        }
        if (runs[r].slow) {
            rtc_clk_cpu_freq_set_config(&old);
        }
    }
    portEXIT_CRITICAL(&s_mux);
    for (int r = 0; r < FW_RUNS; r++) {
        printf("TIME|fill_work%s_%s_k%" PRIu32 "|row=timing-profiles.cache_fill_ps|cpu_mhz=%" PRIu32
               "|lines=%d|inner=%" PRIu32 "|use=%d|addr=0x%08" PRIx32 "|cold=%" PRIu32 "|warm=%" PRIu32
               "|sum=%" PRIu32 "\n",
               runs[r].slow ? "80" : "", runs[r].use ? "use" : "nouse", runs[r].k, mhz[r], FW_LINES,
               runs[r].k, runs[r].use, (uint32_t)(uintptr_t)s_fill_work[r], cold[r], warm[r], sum[r]);
    }
}

// Part C, ROM code speed: the bootloader verifies an image through the ROM's SHA routine, so a
// third of its instructions run from ROM, a fetch path no kernel above times. The ROM's own
// crc32_le (esp_rom_crc32_le) over 4 KB in internal RAM, at CPU 160 and 80 MHz: the model runs the
// same ROM code, so the difference is the ROM's.
#define ROM_CRC_BYTES 4096

static void time_rom_code(void)
{
    uint8_t *buf = heap_caps_malloc(ROM_CRC_BYTES, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    if (buf == NULL) {
        fail("rom_crc32", "no 4 KB buffer");
        return;
    }
    for (int i = 0; i < ROM_CRC_BYTES; i++) {
        buf[i] = (uint8_t)(i * 29u + 7u);
    }
    rtc_cpu_freq_config_t old;
    rtc_cpu_freq_config_t slow;
    rtc_cpu_freq_config_t during;
    rtc_clk_cpu_freq_get_config(&old);
    bool have_slow = rtc_clk_cpu_freq_mhz_to_config(80, &slow);
    uint32_t cycles[2] = {0, 0};
    uint32_t crc[2] = {0, 0};
    uint32_t mhz[2] = {0, 0};
    portENTER_CRITICAL(&s_mux);
    for (int r = 0; r < 2; r++) {
        if (r == 1) {
            if (!have_slow) {
                break;
            }
            rtc_clk_cpu_freq_set_config(&slow);
        }
        rtc_clk_cpu_freq_get_config(&during);
        mhz[r] = during.freq_mhz;
        (void)esp_rom_crc32_le(0, buf, ROM_CRC_BYTES); // a first run, as for every kernel above
        uint32_t c0 = esp_cpu_get_cycle_count();
        crc[r] = esp_rom_crc32_le(0, buf, ROM_CRC_BYTES);
        cycles[r] = esp_cpu_get_cycle_count() - c0;
        if (r == 1) {
            rtc_clk_cpu_freq_set_config(&old);
        }
    }
    portEXIT_CRITICAL(&s_mux);
    for (int r = 0; r < 2; r++) {
        printf("TIME|rom_crc32_4k%s|row=timing-profiles.cpi_milli|cpu_mhz=%" PRIu32 "|bytes=%d"
               "|buf_addr=0x%08" PRIx32 "|cycles=%" PRIu32 "|crc=0x%08" PRIx32 "\n",
               r == 1 ? "_80" : "", mhz[r], ROM_CRC_BYTES, (uint32_t)(uintptr_t)buf, cycles[r],
               crc[r]);
    }
    if (!have_slow) {
        fail("rom_crc32", "no 80 MHz CPU configuration");
    }
    free(buf);
}

// ---------------------------------------------------------------------------------------------
// Fetch group: the fetch version of fill_work, a hit during a fill, and the Block 1 DRAM
// residue's address rule. Printed after every line above; every function and datum here is
// defined after the ones above, so their addresses do not move.
// ---------------------------------------------------------------------------------------------

#define STR_(x) #x
#define STR(x) STR_(x)

// Item 1. A block is FETCH_LINES consecutive 32-byte flash code lines, each a stub entered at its
// first word and left by `c.jr ra`: `end` runs 15 `c.addi t0, 1` and leaves from the line's last
// halfword (its straight run ends in word 7), `mid` runs 6 and leaves from word 3 (an
// unconditional jump mid-line; words 4 to 7 hold `c.nop`s never run). Each block is fetched by
// one timed run alone, so every line of it is cold on that run; the first run of the lines
// counts t0 up by 15 or 6 a line, which the line prints as `ran`. A block is a naked function
// (never called as one), so the compiler emits it in definition order, after the code above.
#define FETCH_LINES 128
#define FETCH_STUB_END ".rept 15\nc.addi t0, 1\n.endr\nc.jr ra\n"
#define FETCH_STUB_MID ".rept 6\nc.addi t0, 1\n.endr\nc.jr ra\n.rept 9\nc.nop\n.endr\n"
#define FETCH_BLOCK(label, n, stub)                                                                \
    static __attribute__((naked, noinline, aligned(32))) void fetch_lines_##label(void)            \
    {                                                                                              \
        __asm__ volatile(".option push\n"                                                          \
                         ".option rvc\n"                                                           \
                         ".rept " STR(n) "\n" stub ".endr\n"                                       \
                         ".option pop\n");                                                         \
    }

FETCH_BLOCK(warm, 1, FETCH_STUB_MID)
FETCH_BLOCK(end_k0, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(end_k8, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(end_k16, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(end_k32, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(end_k64, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(end_k128, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(end_k256, FETCH_LINES, FETCH_STUB_END)
FETCH_BLOCK(mid_k0, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(mid_k8, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(mid_k16, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(mid_k32, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(mid_k64, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(mid_k128, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(mid_k256, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(fmid_k0, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(fmid_k64, FETCH_LINES, FETCH_STUB_MID)
FETCH_BLOCK(fmid_k256, FETCH_LINES, FETCH_STUB_MID)

// The driver calls each line of a block in turn and runs a straight run of `K` `c.addi t1, 1`
// after each return (no loop, so the work has no branch), then the loop's `addi` and `bnez`;
// cycles from before the first call to after the last iteration. The `fetch_iram_*` drivers run
// from IRAM, so the stub line is the only cache access of an iteration; the `fetch_flash_*`
// drivers are the same code in flash, run once on the warm-up line first, so every return lands
// in a warm flash line while the stub's line may still be arriving.
#define FETCH_DRIVER(attr, name, K)                                                                \
    static attr __attribute__((noinline)) uint32_t name(uintptr_t p, uint32_t lines,              \
                                                         uint32_t *ran)                            \
    {                                                                                              \
        uint32_t c0, c1, t0v;                                                                      \
        __asm__ volatile("li t0, 0\n"                                                              \
                         "csrr %0, 0x7e2\n"                                                        \
                         ".balign 4\n"                                                             \
                         "1:\n"                                                                    \
                         "jalr ra, 0(%3)\n"                                                        \
                         "addi %3, %3, 32\n"                                                       \
                         ".rept " #K "\nc.addi t1, 1\n.endr\n"                                     \
                         "addi %4, %4, -1\n"                                                       \
                         "bnez %4, 1b\n"                                                           \
                         "csrr %1, 0x7e2\n"                                                        \
                         "mv %2, t0\n"                                                             \
                         : "=&r"(c0), "=&r"(c1), "=&r"(t0v), "+r"(p), "+r"(lines)                  \
                         :                                                                         \
                         : "t0", "t1", "ra", "memory");                                            \
        *ran = t0v;                                                                                \
        return c1 - c0;                                                                            \
    }

FETCH_DRIVER(IRAM_ATTR, fetch_iram_k0, 0)
FETCH_DRIVER(IRAM_ATTR, fetch_iram_k8, 8)
FETCH_DRIVER(IRAM_ATTR, fetch_iram_k16, 16)
FETCH_DRIVER(IRAM_ATTR, fetch_iram_k32, 32)
FETCH_DRIVER(IRAM_ATTR, fetch_iram_k64, 64)
FETCH_DRIVER(IRAM_ATTR, fetch_iram_k128, 128)
FETCH_DRIVER(IRAM_ATTR, fetch_iram_k256, 256)
FETCH_DRIVER(, fetch_flash_k0, 0)
FETCH_DRIVER(, fetch_flash_k64, 64)
FETCH_DRIVER(, fetch_flash_k256, 256)

// fill_work_use's loop in flash: cold data lines of their own and the same inner loop, the
// driver and its work in a warm flash line (warmed by an untimed run over s_fill_work[0] first).
FILL_WORK_AT(, fill_workf_use, "add %2, %2, t0\n")
#define FWF_RUNS 2
static const uint32_t s_fill_workf[FWF_RUNS][FW_LINES * 8] __attribute__((aligned(32))) = {{1}};

typedef uint32_t (*fetch_driver_t)(uintptr_t, uint32_t, uint32_t *);

static void time_fetch_work(void)
{
    static const struct {
        const char *name;
        void (*lines)(void);
        fetch_driver_t drv;
        uint32_t k;
        uint32_t run_end_word;
        bool flash;
    } runs[] = {
        {"fetch_end_k0", fetch_lines_end_k0, fetch_iram_k0, 0, 7, false},
        {"fetch_end_k8", fetch_lines_end_k8, fetch_iram_k8, 8, 7, false},
        {"fetch_end_k16", fetch_lines_end_k16, fetch_iram_k16, 16, 7, false},
        {"fetch_end_k32", fetch_lines_end_k32, fetch_iram_k32, 32, 7, false},
        {"fetch_end_k64", fetch_lines_end_k64, fetch_iram_k64, 64, 7, false},
        {"fetch_end_k128", fetch_lines_end_k128, fetch_iram_k128, 128, 7, false},
        {"fetch_end_k256", fetch_lines_end_k256, fetch_iram_k256, 256, 7, false},
        {"fetch_mid_k0", fetch_lines_mid_k0, fetch_iram_k0, 0, 3, false},
        {"fetch_mid_k8", fetch_lines_mid_k8, fetch_iram_k8, 8, 3, false},
        {"fetch_mid_k16", fetch_lines_mid_k16, fetch_iram_k16, 16, 3, false},
        {"fetch_mid_k32", fetch_lines_mid_k32, fetch_iram_k32, 32, 3, false},
        {"fetch_mid_k64", fetch_lines_mid_k64, fetch_iram_k64, 64, 3, false},
        {"fetch_mid_k128", fetch_lines_mid_k128, fetch_iram_k128, 128, 3, false},
        {"fetch_mid_k256", fetch_lines_mid_k256, fetch_iram_k256, 256, 3, false},
        {"fetchf_mid_k0", fetch_lines_fmid_k0, fetch_flash_k0, 0, 3, true},
        {"fetchf_mid_k64", fetch_lines_fmid_k64, fetch_flash_k64, 64, 3, true},
        {"fetchf_mid_k256", fetch_lines_fmid_k256, fetch_flash_k256, 256, 3, true},
    };
    enum { N = sizeof(runs) / sizeof(runs[0]) };
    uint32_t cold[N];
    uint32_t warm[N];
    uint32_t ran[N];
    for (int r = 0; r < N; r++) {
        uint32_t ran_other;
        portENTER_CRITICAL(&s_mux);
        (void)runs[r].drv((uintptr_t)fetch_lines_warm, 1, &ran_other);
        cold[r] = runs[r].drv((uintptr_t)runs[r].lines, FETCH_LINES, &ran[r]);
        warm[r] = runs[r].drv((uintptr_t)runs[r].lines, FETCH_LINES, &ran_other);
        portEXIT_CRITICAL(&s_mux);
    }
    for (int r = 0; r < N; r++) {
        printf("TIME|%s|row=timing-profiles.cache_first_word_ps,timing-profiles.cache_fill_ps"
               "|lines=%d|run_end_word=%" PRIu32 "|work=%" PRIu32 "|driver=%s|code_addr=0x%08" PRIx32
               "|driver_addr=0x%08" PRIx32 "|cold=%" PRIu32 "|warm=%" PRIu32 "|ran=%" PRIu32 "\n",
               runs[r].name, FETCH_LINES, runs[r].run_end_word, runs[r].k,
               runs[r].flash ? "flash" : "iram", (uint32_t)(uintptr_t)runs[r].lines,
               (uint32_t)(uintptr_t)runs[r].drv, cold[r], warm[r], ran[r]);
    }

    static const uint32_t kf[FWF_RUNS] = {32, 96};
    uint32_t fcold[FWF_RUNS];
    uint32_t fwarm[FWF_RUNS];
    uint32_t fsum[FWF_RUNS];
    for (int r = 0; r < FWF_RUNS; r++) {
        uint32_t sum_other;
        portENTER_CRITICAL(&s_mux);
        (void)fill_workf_use(s_fill_work[0], kf[r], &sum_other);
        fcold[r] = fill_workf_use(s_fill_workf[r], kf[r], &fsum[r]);
        fwarm[r] = fill_workf_use(s_fill_workf[r], kf[r], &sum_other);
        portEXIT_CRITICAL(&s_mux);
    }
    for (int r = 0; r < FWF_RUNS; r++) {
        printf("TIME|fill_workf_use_k%" PRIu32 "|row=timing-profiles.cache_fill_ps|lines=%d"
               "|inner=%" PRIu32 "|driver=flash|addr=0x%08" PRIx32 "|driver_addr=0x%08" PRIx32
               "|cold=%" PRIu32 "|warm=%" PRIu32 "|sum=%" PRIu32 "\n",
               kf[r], FW_LINES, kf[r], (uint32_t)(uintptr_t)s_fill_workf[r],
               (uint32_t)(uintptr_t)fill_workf_use, fcold[r], fwarm[r], fsum[r]);
    }
}

// Item 2. Eight `lw t0, 0(a0)` (or `sw t0, 0(a0)`) a loop, 256 iterations, from IRAM; every
// instruction is 32-bit (`.option norvc`). Function `c<cc>` starts on a 64-byte boundary of its
// own and runs cc / 4 `nop`s, `li` and `csrr` before the loop (cc = 0 to 64 in steps of 8), so its
// loop head is at its address plus cc plus 8 and steps 8 bytes a function. It returns the cycles
// from before the loop to after it, as every CPI kernel above does. The operand is a word of
// s_dres, stepped the same way from a 64-byte boundary inside it. Naked functions, emitted in
// definition order after the code above, so the kernels above keep their addresses.
#define DRES_FN(kind, c, insn)                                                                     \
    static IRAM_ATTR __attribute__((naked, noinline, aligned(64))) uint32_t dres_##kind##_c##c(    \
        volatile uint32_t *op)                                                                     \
    {                                                                                              \
        __asm__ volatile(".option push\n"                                                          \
                         ".option norvc\n"                                                         \
                         ".rept " #c " / 4\nnop\n.endr\n"                                          \
                         "li t2, 256\n"                                                            \
                         "csrr a1, 0x7e2\n"                                                        \
                         "1:\n"                                                                    \
                         ".rept 8\n" insn "\n.endr\n"                                              \
                         "addi t2, t2, -1\n"                                                       \
                         "bnez t2, 1b\n"                                                           \
                         "csrr a2, 0x7e2\n"                                                        \
                         "sub a0, a2, a1\n"                                                        \
                         "ret\n"                                                                   \
                         ".option pop\n");                                                         \
    }
#define DRES_KIND(kind, insn)                                                                      \
    DRES_FN(kind, 0, insn)                                                                         \
    DRES_FN(kind, 8, insn)                                                                         \
    DRES_FN(kind, 16, insn)                                                                        \
    DRES_FN(kind, 24, insn)                                                                        \
    DRES_FN(kind, 32, insn)                                                                        \
    DRES_FN(kind, 40, insn)                                                                        \
    DRES_FN(kind, 48, insn)                                                                        \
    DRES_FN(kind, 56, insn)                                                                        \
    DRES_FN(kind, 64, insn)

DRES_KIND(load, "lw t0, 0(a0)")
DRES_KIND(store, "sw t0, 0(a0)")

#define DRES_STEPS 9
// 192 bytes at 8-byte alignment, so adding it moves the static data after it by a multiple of 64
// and the offsets within 64 bytes that the placement lines print stay as they were.
static DRAM_ATTR uint32_t s_dres[48] __attribute__((aligned(8)));

typedef uint32_t (*dres_fn_t)(volatile uint32_t *);

static void time_dram_residue(void)
{
    static const struct {
        const char *name;
        dres_fn_t fn[DRES_STEPS];
    } kinds[] = {
        {"load",
         {dres_load_c0, dres_load_c8, dres_load_c16, dres_load_c24, dres_load_c32, dres_load_c40,
          dres_load_c48, dres_load_c56, dres_load_c64}},
        {"store",
         {dres_store_c0, dres_store_c8, dres_store_c16, dres_store_c24, dres_store_c32,
          dres_store_c40, dres_store_c48, dres_store_c56, dres_store_c64}},
    };
    volatile uint32_t *const words =
        (volatile uint32_t *)(((uintptr_t)s_dres + 63u) & ~(uintptr_t)63u);
    const uint32_t base = (uint32_t)(uintptr_t)words;
    // SRAM Block 1 in its DRAM view (TRM table 16.3-1); the IRAM view is 0x0070_0000 above it.
    const bool data_in_block1 = base >= 0x3FC80000u && base + 64u + 4u <= 0x3FCA0000u;
    for (int k = 0; k < 2; k++) {
        for (int c = 0; c < DRES_STEPS; c++) {
            uint32_t cycles[DRES_STEPS];
            dres_fn_t fn = kinds[k].fn[c];
            portENTER_CRITICAL(&s_mux);
            for (int d = 0; d < DRES_STEPS; d++) {
                volatile uint32_t *op = &words[d * 2];
                (void)fn(op);
                cycles[d] = fn(op);
            }
            portEXIT_CRITICAL(&s_mux);
            const uint32_t head = (uint32_t)(uintptr_t)fn + (uint32_t)c * 8u + 8u;
            const bool code_in_block1 = head >= 0x40380000u && head + 64u <= 0x403A0000u;
            printf("TIME|dres_%s_c%02d|row=timing-profiles.cpi_milli|iters=%d|insns=10"
                   "|code_addr=0x%08" PRIx32 "|data_addr=0x%08" PRIx32 "|step=8|block1=%d"
                   "|d00=%" PRIu32 "|d08=%" PRIu32 "|d16=%" PRIu32 "|d24=%" PRIu32 "|d32=%" PRIu32
                   "|d40=%" PRIu32 "|d48=%" PRIu32 "|d56=%" PRIu32 "|d64=%" PRIu32 "\n",
                   kinds[k].name, c * 8, CPI_ITERS, head, base, data_in_block1 && code_in_block1,
                   cycles[0], cycles[1], cycles[2], cycles[3], cycles[4], cycles[5], cycles[6],
                   cycles[7], cycles[8]);
        }
    }
}

// Item 3 (the application phases of the boot): what a warm flash line costs to enter.
// Each kernel is called 64 times from an IRAM loop after one warm-up call, so every line is a
// hit; t0 counts the `c.addi`s run (`ran`). `seq`: 255 `c.addi` and `c.jr ra` straight through
// 16 lines (15 sequential line crossings); `jump`: 16 lines, each `c.addi` and a `c.j` to the next
// line's first word (15 crossings by a jump); `jump_in`: the same 16 `c.addi` and 15 `c.j` packed
// into 2 lines (one crossing by a jump). The `_iram` twins run the same code from IRAM, where
// there is no cache. seq - seq_iram is one entry and 15 sequential crossings, jump_in -
// jump_in_iram one entry and one jump crossing, jump - jump_in 14 jump crossings.
#define WLINE_SEQ ".rept 255\nc.addi t0, 1\n.endr\nc.jr ra\n"
#define WLINE_JUMP ".rept 15\nc.addi t0, 1\nc.j 1f\n.balign 32\n1:\n.endr\nc.addi t0, 1\nc.jr ra\n"
#define WLINE_JUMP_IN ".rept 15\nc.addi t0, 1\nc.j 1f\n1:\n.endr\nc.addi t0, 1\nc.jr ra\n"
#define WLINE_FN(attr, name, body)                                                                 \
    static attr __attribute__((naked, noinline, aligned(32))) void name(void)                     \
    {                                                                                              \
        __asm__ volatile(".option push\n"                                                          \
                         ".option rvc\n" body ".option pop\n");                                    \
    }

WLINE_FN(, wline_seq, WLINE_SEQ)
WLINE_FN(, wline_jump, WLINE_JUMP)
WLINE_FN(, wline_jump_in, WLINE_JUMP_IN)
WLINE_FN(IRAM_ATTR, wline_seq_iram, WLINE_SEQ)
WLINE_FN(IRAM_ATTR, wline_jump_in_iram, WLINE_JUMP_IN)

#define WLINE_CALLS 64

static IRAM_ATTR __attribute__((noinline)) uint32_t wline_run(void (*fn)(void), uint32_t calls,
                                                              uint32_t *ran)
{
    uint32_t c0, c1, t0v;
    __asm__ volatile("li t0, 0\n"
                     "csrr %0, 0x7e2\n"
                     ".balign 4\n"
                     "1:\n"
                     "jalr ra, 0(%3)\n"
                     "addi %4, %4, -1\n"
                     "bnez %4, 1b\n"
                     "csrr %1, 0x7e2\n"
                     "mv %2, t0\n"
                     : "=&r"(c0), "=&r"(c1), "=&r"(t0v), "+r"(fn), "+r"(calls)
                     :
                     : "t0", "ra", "memory");
    *ran = t0v;
    return c1 - c0;
}

// Item 4 (the same): the access kinds the application's flash code runs that no kernel above
// times, from a warm flash line with the operand in DRAM (s_dres; flash code pays no Block 1
// residue, cpi_flash_*): a byte and a halfword load and store, and a load right after a store to
// the same word and to the next one.
CPI_KERNEL_FLASH(x_lbu, "", CPI_REP8("lbu t0, 1(%3)\n"))
CPI_KERNEL_FLASH(x_lhu, "", CPI_REP8("lhu t0, 2(%3)\n"))
CPI_KERNEL_FLASH(x_sb, "", CPI_REP8("sb t0, 1(%3)\n"))
CPI_KERNEL_FLASH(x_sh, "", CPI_REP8("sh t0, 2(%3)\n"))
CPI_KERNEL_FLASH(x_sw_lw_same, "", CPI_REP8("sw t0, 0(%3)\nlw t1, 0(%3)\n"))
CPI_KERNEL_FLASH(x_sw_lw_next, "", CPI_REP8("sw t0, 0(%3)\nlw t1, 4(%3)\n"))

static void time_warm_lines(void)
{
    static const struct {
        const char *name;
        void (*fn)(void);
        int lines;
        int seq;
        int jumps;
        bool flash;
    } wl[] = {
        {"seq", wline_seq, 16, 15, 0, true},
        {"seq_iram", wline_seq_iram, 16, 15, 0, false},
        {"jump", wline_jump, 16, 0, 15, true},
        {"jump_in", wline_jump_in, 2, 0, 1, true},
        {"jump_in_iram", wline_jump_in_iram, 2, 0, 1, false},
    };
    enum { NW = sizeof(wl) / sizeof(wl[0]) };
    uint32_t cycles[NW];
    uint32_t ran[NW];
    portENTER_CRITICAL(&s_mux);
    for (int i = 0; i < NW; i++) {
        uint32_t ran_other;
        (void)wline_run(wl[i].fn, 1, &ran_other);
        cycles[i] = wline_run(wl[i].fn, WLINE_CALLS, &ran[i]);
    }
    portEXIT_CRITICAL(&s_mux);
    for (int i = 0; i < NW; i++) {
        printf("TIME|wline_%s|row=timing-profiles.cpi_milli|calls=%d|lines=%d|seq_crossings=%d"
               "|jump_crossings=%d|code=%s|code_addr=0x%08" PRIx32 "|cycles=%" PRIu32
               "|ran=%" PRIu32 "\n",
               wl[i].name, WLINE_CALLS, wl[i].lines, wl[i].seq, wl[i].jumps,
               wl[i].flash ? "flash" : "iram", (uint32_t)(uintptr_t)wl[i].fn, cycles[i], ran[i]);
    }

    static const kernel_t xk[] = {
        {"x_lbu", cpif_x_lbu, 10},
        {"x_lhu", cpif_x_lhu, 10},
        {"x_sb", cpif_x_sb, 10},
        {"x_sh", cpif_x_sh, 10},
        {"x_sw_lw_same", cpif_x_sw_lw_same, 18},
        {"x_sw_lw_next", cpif_x_sw_lw_next, 18},
    };
    enum { NX = sizeof(xk) / sizeof(xk[0]) };
    volatile uint32_t *const ram =
        (volatile uint32_t *)(((uintptr_t)s_dres + 63u) & ~(uintptr_t)63u);
    uint32_t xc[NX];
    portENTER_CRITICAL(&s_mux);
    for (int i = 0; i < NX; i++) {
        xc[i] = run2(xk[i].fn, ram, (const volatile uint32_t *)GPIO_IN_REG);
    }
    portEXIT_CRITICAL(&s_mux);
    for (int i = 0; i < NX; i++) {
        print_kernel("cpi_flash", &xk[i], "static", xc[i], ram);
    }
}

// ---------------------------------------------------------------------------------------------
// Replacement group: the cache's replacement policy. Printed after every line above. Nothing above
// moves: the new flash code is in `.irom0.text` and the new constants in `.rodata1`, which the
// ESP-IDF linker script places after all other flash text and rodata; the access order is a heap
// block; the one IRAM function (ways_run) moves only the ESP-IDF IRAM code linked after it, by a
// multiple of 16 bytes. The call in app_main moves the ESP-IDF flash code after it by 4 bytes.
// ---------------------------------------------------------------------------------------------

#define WAYS_TEXT __attribute__((section(".irom0.text")))
#define WAYS_RODATA __attribute__((section(".rodata1")))

// The geometry: 16 KB of 8 ways of 32-byte lines (IDF esp32c3/rom/cache.h MAX_ICACHE_SIZE,
// MAX_ICACHE_WAYS, MIN_CACHE_LINE_SIZE; the TRM's ICache, one cache behind IBUS and DBUS). A way
// is 2048 bytes, so there are 64 sets, a line's set is address bits 10:5, and addresses 2048
// bytes apart share a set; the MMU's 64 KB pages keep those bits from virtual to physical. The
// C3's EXTMEM has no register that reports the geometry (only enables, counters and the sync,
// lock, preload and autoload controls), so the ROM header's constants are the source.
#define WAYS_STRIDE (MAX_ICACHE_SIZE / MAX_ICACHE_WAYS)
#define WAYS_SETS (WAYS_STRIDE / MIN_CACHE_LINE_SIZE)
#define WAYS_CODE_ROWS 16
#define WAYS_DATA_ROWS 8
#define WAYS_PASSES 64
#define WAYS_PASS_MAX 16
_Static_assert(WAYS_STRIDE == 2048 && WAYS_SETS == 64, "the C3 cache is 64 sets of 8 ways");
_Static_assert(WAYS_CODE_ROWS * WAYS_SETS == 1024, "the .rept count of ways_code");

// WAYS_CODE_ROWS ways' worth of consecutive 32-byte lines, each a `ret` and seven `nop`s never
// run (`.option norvc`, so every line is exactly 32 bytes), so each set holds WAYS_CODE_ROWS of
// them 2048 bytes apart (ways_line).
static WAYS_TEXT __attribute__((naked, noinline, aligned(32))) void ways_code(void)
{
    __asm__ volatile(".option push\n"
                     ".option norvc\n"
                     ".rept 1024\nret\n.rept 7\nnop\n.endr\n.endr\n"
                     ".option pop\n");
}

// WAYS_DATA_ROWS ways' worth of constant lines, every word 1, so a run's `sum` is its data reads.
static const uint32_t s_ways_data[WAYS_DATA_ROWS * WAYS_STRIDE / 4] WAYS_RODATA
    __attribute__((aligned(32))) = {[0 ...(WAYS_DATA_ROWS * WAYS_STRIDE / 4) - 1] = 1};

// The run's results, filled by ways_run at the offsets its assembly names.
typedef struct {
    uint32_t cold;     // 0: cycles of the cold pass
    uint32_t warm;     // 4: cycles of the WAYS_PASSES passes after it
    uint32_t sum_cold; // 8: data words read in the cold pass
    uint32_t sum;      // 12: data words read in the passes after it
    // 16, 32, 48: EXTMEM IBUS miss, IBUS access, DBUS miss and DBUS access counts before the cold
    // pass, between the two and after the last pass
    uint32_t cnt[3][4];
} ways_out_t;
_Static_assert(offsetof(ways_out_t, sum) == 12 && offsetof(ways_out_t, cnt) == 16 &&
                   sizeof(ways_out_t) == 64,
               "the offsets ways_run stores at");
_Static_assert(DR_REG_EXTMEM_BASE == 0x600C4000 &&
                   EXTMEM_IBUS_ACS_MISS_CNT_REG == DR_REG_EXTMEM_BASE + 0x68 &&
                   EXTMEM_IBUS_ACS_CNT_REG == DR_REG_EXTMEM_BASE + 0x6C &&
                   EXTMEM_DBUS_ACS_FLASH_MISS_CNT_REG == DR_REG_EXTMEM_BASE + 0x70 &&
                   EXTMEM_DBUS_ACS_CNT_REG == DR_REG_EXTMEM_BASE + 0x74,
               "the EXTMEM counter offsets ways_run reads");

// The four EXTMEM counters (read-only, never cleared here) into out + `off`.
#define WAYS_CNT(off)                                                                              \
    "lw t0, 0x68(t4)\nsw t0, " #off "(a3)\n"                                                       \
    "lw t0, 0x6c(t4)\nsw t0, " #off "+4(a3)\n"                                                     \
    "lw t0, 0x70(t4)\nsw t0, " #off "+8(a3)\n"                                                     \
    "lw t0, 0x74(t4)\nsw t0, " #off "+12(a3)\n"
// Runs `len` entries from a0 on: an entry with bit 0 clear is the address of a `ret` line,
// called; one with bit 0 set is a flash constant word's address plus 1, read. Stores the cycles
// from before the first entry to after the last at out + `cyc` and the words read at out + `sum`.
#define WAYS_LOOP(len, cyc, sum, l1, l2, l3)                                                       \
    "li t5, 0\n"                                                                                   \
    "csrr t6, 0x7e2\n" #l1 ":\n"                                                                   \
    "lw t0, 0(a0)\n"                                                                               \
    "andi t1, t0, 1\n"                                                                             \
    "bnez t1, " #l2 "f\n"                                                                          \
    "jalr ra, 0(t0)\n"                                                                             \
    "j " #l3 "f\n" #l2 ":\n"                                                                       \
    "lw t1, -1(t0)\n"                                                                              \
    "add t5, t5, t1\n" #l3 ":\n"                                                                   \
    "addi a0, a0, 4\n"                                                                             \
    "addi " #len ", " #len ", -1\n"                                                                \
    "bnez " #len ", " #l1 "b\n"                                                                    \
    "csrr t0, 0x7e2\n"                                                                             \
    "sub t0, t0, t6\n"                                                                             \
    "sw t0, " #cyc "(a3)\n"                                                                        \
    "sw t5, " #sum "(a3)\n"
// ways_run(seq, cold_len, warm_len, out): the cold pass (cold_len entries), then the WAYS_PASSES
// passes after it (warm_len entries), from IRAM with nothing in flash between them, and the
// EXTMEM counters around each. Naked and all 32-bit instructions: 60 of them, 240 bytes at the
// 2-byte alignment of the code before it, so the IRAM code after it moves by 240 bytes (its 8-byte
// fetch chunks keep their SRAM bank) and the end of IRAM stays in its 512-byte block, so
// no DRAM datum moves (checked on the ELF).
static IRAM_ATTR __attribute__((naked, noinline, aligned(2))) void
ways_run(const uint32_t *seq, uint32_t cold_len, uint32_t warm_len, ways_out_t *out)
{
    __asm__ volatile(".option push\n"
                     ".option norvc\n"
                     "mv t3, ra\n"
                     "li t4, 0x600C4000\n"
                     WAYS_CNT(16)
                     WAYS_LOOP(a1, 0, 8, 1, 2, 3)
                     WAYS_CNT(32)
                     WAYS_LOOP(a2, 4, 12, 4, 5, 6)
                     WAYS_CNT(48)
                     "mv ra, t3\n"
                     "ret\n"
                     ".option pop\n");
}

enum { WAYS_CYCLIC, WAYS_PSEUDO, WAYS_MIXED, WAYS_RETOUCH, WAYS_KINDS };

static const char s_ways_kind[WAYS_KINDS][8] WAYS_RODATA = {"cyclic", "pseudo", "mixed", "retouch"};
static const uint8_t s_ways_n[] WAYS_RODATA = {4, 6, 7, 8, 9, 10, 12, 16};
// One pass of the retouch run: lines 0 to 7, 0 and 1 again, then 8 and 9.
static const uint8_t s_ways_retouch[] WAYS_RODATA = {0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 8, 9};
static const char s_ways_what[] WAYS_RODATA = "ways";
static const char s_ways_no_block[] WAYS_RODATA = "no internal block for the access order";
static const char s_ways_not_one_set[] WAYS_RODATA = "a run's lines are not in one set";
static const char s_ways_fmt[] WAYS_RODATA =
    "TIME|ways_%s_n%" PRIu32 "|row=timing-profiles.cache_model|n=%" PRIu32 "|pass=%" PRIu32
    "|passes=%d|cache=%d|ways=%d|line=%d|stride=%d|set=%" PRIu32 "|order=%016" PRIx64
    "|cold=%" PRIu32 "|cycles=%" PRIu32 "|cold_ibus_miss=%" PRIu32 "|cold_ibus_acs=%" PRIu32
    "|cold_dbus_miss=%" PRIu32 "|cold_dbus_acs=%" PRIu32 "|ibus_miss=%" PRIu32 "|ibus_acs=%" PRIu32
    "|dbus_miss=%" PRIu32 "|dbus_acs=%" PRIu32 "|sum=%" PRIu32 "|lines=%s\n";

// Line `row` of set `set` in a block of consecutive 32-byte lines starting at `base`: the block's
// first line of that set, then every 2048 bytes (64 divides 2^32, so the unsigned wrap is exact).
static WAYS_TEXT uint32_t ways_line(uint32_t base, uint32_t set, uint32_t row)
{
    uint32_t first = (set - base / 32u) % WAYS_SETS;
    return base + (first + row * WAYS_SETS) * 32u;
}

// The address of line `i` of a run in set `set`, plus 1 when it is read rather than called: code
// row i, or for the mixed runs code row i / 2 at an even i and data row i / 2 at an odd one.
static WAYS_TEXT uint32_t ways_entry(int kind, uint32_t set, uint32_t i)
{
    if (kind == WAYS_MIXED && (i & 1u)) {
        return ways_line((uint32_t)(uintptr_t)s_ways_data, set, i / 2u) + 1u;
    }
    return ways_line((uint32_t)(uintptr_t)ways_code, set, kind == WAYS_MIXED ? i / 2u : i);
}

// The line indices of all 1 + WAYS_PASSES passes of a run into `idx`; returns one pass's length.
// Pseudo: each pass a Fisher-Yates shuffle of 0 to n-1 from the identity, drawn from one
// xorshift32 stream seeded with 0x3A5E0000 + n.
static WAYS_TEXT uint32_t ways_order(int kind, uint32_t n, uint8_t *idx)
{
    uint32_t pass = kind == WAYS_RETOUCH ? (uint32_t)sizeof(s_ways_retouch) : n;
    uint32_t rng = 0x3A5E0000u + n;
    for (uint32_t p = 0; p <= WAYS_PASSES; p++) {
        uint8_t *o = idx + p * pass;
        for (uint32_t i = 0; i < pass; i++) {
            o[i] = kind == WAYS_RETOUCH ? s_ways_retouch[i] : (uint8_t)i;
        }
        if (kind == WAYS_PSEUDO) {
            for (uint32_t i = n - 1; i > 0; i--) {
                uint32_t j = xorshift32(&rng) % (i + 1u);
                uint8_t t = o[i];
                o[i] = o[j];
                o[j] = t;
            }
        }
    }
    return pass;
}

// `v` as 0x and eight lowercase hex digits at `s` (no string constant, so nothing is added to the
// object's string section).
static WAYS_TEXT char *ways_hex(char *s, uint32_t v)
{
    *s++ = '0';
    *s++ = 'x';
    for (int k = 28; k >= 0; k -= 4) {
        uint32_t d = (v >> k) & 0xFu;
        *s++ = (char)(d < 10u ? '0' + d : 'a' + d - 10u);
    }
    return s;
}

static WAYS_TEXT void time_ways(void)
{
    enum { NN = sizeof(s_ways_n), NRUN = 3 * NN + 1 };
    _Static_assert(NRUN <= WAYS_SETS, "one set a run");
    enum { MAXLEN = (WAYS_PASSES + 1) * WAYS_PASS_MAX };
    uint32_t *seq = heap_caps_malloc(MAXLEN * (sizeof(uint32_t) + 1),
                                     MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    if (seq == NULL) {
        fail(s_ways_what, s_ways_no_block);
        return;
    }
    uint8_t *idx = (uint8_t *)(seq + MAXLEN);
    for (uint32_t r = 0; r < NRUN; r++) {
        int kind = r == 3 * NN ? WAYS_RETOUCH : (int)(r / NN);
        uint32_t n = kind == WAYS_RETOUCH ? 10u : s_ways_n[r % NN];
        uint32_t set = r;
        uint32_t pass = ways_order(kind, n, idx);
        uint32_t len = pass * (WAYS_PASSES + 1);
        for (uint32_t k = 0; k < len; k++) {
            seq[k] = ways_entry(kind, set, idx[k]);
        }
        char lines[WAYS_PASS_MAX * 11];
        char *at = lines;
        bool one_set = true;
        for (uint32_t i = 0; i < n; i++) {
            uint32_t a = ways_entry(kind, set, i) & ~1u;
            one_set = one_set && ((a >> 5) % WAYS_SETS) == set;
            if (i) {
                *at++ = ',';
            }
            at = ways_hex(at, a);
        }
        *at = '\0';
        if (!one_set) {
            fail(s_ways_what, s_ways_not_one_set);
        }
        ways_out_t o;
        portENTER_CRITICAL(&s_mux);
        ways_run(seq, pass, pass * WAYS_PASSES, &o);
        portEXIT_CRITICAL(&s_mux);
        printf(s_ways_fmt, s_ways_kind[kind], n, n, pass, WAYS_PASSES, MAX_ICACHE_SIZE,
               MAX_ICACHE_WAYS, MIN_CACHE_LINE_SIZE, WAYS_STRIDE, set, fnv1a64(idx, len), o.cold,
               o.warm, o.cnt[1][0] - o.cnt[0][0], o.cnt[1][1] - o.cnt[0][1],
               o.cnt[1][2] - o.cnt[0][2], o.cnt[1][3] - o.cnt[0][3], o.cnt[2][0] - o.cnt[1][0],
               o.cnt[2][1] - o.cnt[1][1], o.cnt[2][2] - o.cnt[1][2], o.cnt[2][3] - o.cnt[1][3],
               o.sum_cold + o.sum, lines);
    }
    free(seq);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    time_cache();
    time_aes();
    time_rsa();
    time_slow_edge();
    rtc_cal();
    systimer_comparator();
    time_usj_drain();
    time_i2c();
    time_uart0();
    time_cpi();
    time_sha_prefilled();
    time_cpi_placement();
    time_cpi_mmio_blocks();
    time_cpi_80mhz();
    time_fill_work();
    time_rom_code();
    time_fetch_work();
    time_dram_residue();
    time_warm_lines();
    time_ways();
    PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
}
