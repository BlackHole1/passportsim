// flash_stress: erase and program patterns over a scratch partition.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific
// code.
//
// Works only inside the `scratch` data partition of this probe's own partition table
// (partitions.csv: 256 KB at 0x500000). Two boots, with the place held in RTC slow memory:
//
//   1. On the first boot:
//      a. erase the whole partition and check every byte reads 0xFF;
//      b. program 64 KB of a position-dependent pattern and read it back;
//      c. program 0x55 over the pattern without erasing: NOR flash only clears bits, so every
//         byte must read `pattern & 0x55`;
//      d. erase the one 4 KB sector in the middle of the 64 KB and check that only that sector
//         reads 0xFF while both neighbours keep their contents;
//      e. program a 6 KB block straddling a sector boundary and read it back;
//      f. sixteen erase and program cycles on one sector with a different seed each time;
//      g. program a marker record in the last sector and restart.
//   2. The boot after the restart reads the marker back (flash contents survive a reset) and ends
//      the run.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   PART     the scratch partition's offset and size
//   ERASE    step a: bytes checked, bytes not 0xFF
//   PATTERN  step b: bytes, mismatches, CRC-32 of what was read back
//   AND      step c: mismatches against `pattern & 0x55`, CRC-32
//   SECTOR   step d: mismatches inside the erased sector and in each neighbour
//   STRADDLE step e: mismatches, CRC-32
//   CYCLES   step f: cycles, total mismatches, CRC-32 of the last cycle
//   TIME     esp_timer microseconds of the full erase and of the 64 KB program: informational, not
//            deterministic on silicon, and never part of the pass criterion
//   PERSIST  boot 2: whether the marker read back intact
//
// Every error code of the flash calls is checked; ESP_ERR_FLASH_OP_TIMEOUT or any other failure
// prints a FAIL line.
//
// Consumed by: the flash model tests.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "esp_attr.h"
#include "esp_partition.h"
#include "esp_rom_crc.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#include "probe_line.h"

#define PROBE_NAME "flash_stress"

#define SECTOR 4096u
#define PATTERN_BYTES (16u * SECTOR)       // 64 KB
#define MIDDLE_SECTOR_OFFSET (8u * SECTOR) // inside the 64 KB
#define STRADDLE_OFFSET (0x20000u - 2048u) // 2 KB before a sector boundary
#define STRADDLE_BYTES 6144u
#define CYCLE_OFFSET (0x30000u)
#define CYCLES 16u
#define MARKER_MAGIC 0x464c5331u // "FLS1"

// The highest byte any step writes is below this: the pattern (64 KB), the straddle (to 0x21000),
// the cycles (0x30000 + 4 KB) and the marker in the last sector all fit in 256 KB.
#define SCRATCH_MIN_BYTES (CYCLE_OFFSET + 2u * SECTOR)

#define STATE_MAGIC 0x464c5331u
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_stage;
enum { STAGE_RESTARTED = 1, STAGE_DONE = 2 };

static const esp_partition_t *s_part;
static uint8_t s_buf[SECTOR];
static bool s_ok = true;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

static bool check_rc(const char *what, esp_err_t rc)
{
    if (rc != ESP_OK) {
        char detail[48];
        snprintf(detail, sizeof(detail), "esp_err 0x%x", (unsigned)rc);
        fail(what, detail);
        return false;
    }
    return true;
}

static uint8_t pattern_byte(uint32_t offset, uint32_t seed)
{
    return (uint8_t)((offset * 31u + seed * 17u + 7u) & 0xffu);
}

// Programs `len` bytes of the pattern for `seed` at `offset`, one sector-sized chunk at a time.
static bool program_pattern(const char *what, uint32_t offset, uint32_t len, uint32_t seed)
{
    for (uint32_t done = 0; done < len;) {
        uint32_t n = len - done < SECTOR ? len - done : SECTOR;
        for (uint32_t i = 0; i < n; i++) {
            s_buf[i] = pattern_byte(offset + done + i, seed);
        }
        if (!check_rc(what, esp_partition_write(s_part, offset + done, s_buf, n))) {
            return false;
        }
        done += n;
    }
    return true;
}

// Reads `len` bytes at `offset` and counts bytes that differ from `expect(offset)`; folds what was
// read into `*crc`.
typedef uint8_t (*expect_fn)(uint32_t offset, uint32_t seed);

static uint32_t verify(const char *what, uint32_t offset, uint32_t len, expect_fn expect,
                       uint32_t seed, uint32_t *crc)
{
    uint32_t mismatches = 0;
    for (uint32_t done = 0; done < len;) {
        uint32_t n = len - done < SECTOR ? len - done : SECTOR;
        if (!check_rc(what, esp_partition_read(s_part, offset + done, s_buf, n))) {
            return len;
        }
        for (uint32_t i = 0; i < n; i++) {
            if (s_buf[i] != expect(offset + done + i, seed)) {
                mismatches++;
            }
        }
        if (crc != NULL) {
            *crc = esp_rom_crc32_le(*crc, s_buf, n);
        }
        done += n;
    }
    return mismatches;
}

static uint8_t erased(uint32_t offset, uint32_t seed)
{
    (void)offset;
    (void)seed;
    return 0xff;
}

static uint8_t anded(uint32_t offset, uint32_t seed)
{
    return pattern_byte(offset, seed) & 0x55u;
}

static void step_erase_all(void)
{
    int64_t t0 = esp_timer_get_time();
    check_rc("erase_all", esp_partition_erase_range(s_part, 0, s_part->size));
    int64_t erase_us = esp_timer_get_time() - t0;
    uint32_t bad = verify("erase_all_read", 0, s_part->size, erased, 0, NULL);
    printf("ERASE|bytes=%" PRIu32 "|not_ff=%" PRIu32 "\n", s_part->size, bad);
    if (bad != 0) {
        fail("erase_all", "bytes not 0xFF after erasing the partition");
    }

    t0 = esp_timer_get_time();
    program_pattern("pattern_write", 0, PATTERN_BYTES, 1);
    int64_t program_us = esp_timer_get_time() - t0;
    printf("TIME|erase_256k_us=%" PRId64 "|program_64k_us=%" PRId64 "\n", erase_us, program_us);
}

static void step_pattern(void)
{
    uint32_t crc = 0;
    uint32_t bad = verify("pattern_read", 0, PATTERN_BYTES, pattern_byte, 1, &crc);
    printf("PATTERN|bytes=%u|mismatches=%" PRIu32 "|crc32=0x%08" PRIx32 "\n", PATTERN_BYTES, bad,
           crc);
    if (bad != 0) {
        fail("pattern", "the programmed pattern did not read back");
    }
}

static void step_and(void)
{
    memset(s_buf, 0x55, sizeof(s_buf));
    for (uint32_t off = 0; off < PATTERN_BYTES; off += SECTOR) {
        memset(s_buf, 0x55, sizeof(s_buf));
        if (!check_rc("and_write", esp_partition_write(s_part, off, s_buf, SECTOR))) {
            return;
        }
    }
    uint32_t crc = 0;
    uint32_t bad = verify("and_read", 0, PATTERN_BYTES, anded, 1, &crc);
    printf("AND|bytes=%u|mismatches=%" PRIu32 "|crc32=0x%08" PRIx32 "\n", PATTERN_BYTES, bad, crc);
    if (bad != 0) {
        fail("and", "programming without an erase did not give pattern AND 0x55");
    }
}

static void step_sector(void)
{
    check_rc("sector_erase", esp_partition_erase_range(s_part, MIDDLE_SECTOR_OFFSET, SECTOR));
    uint32_t inside = verify("sector_read", MIDDLE_SECTOR_OFFSET, SECTOR, erased, 0, NULL);
    uint32_t before =
        verify("sector_read", MIDDLE_SECTOR_OFFSET - SECTOR, SECTOR, anded, 1, NULL);
    uint32_t after = verify("sector_read", MIDDLE_SECTOR_OFFSET + SECTOR, SECTOR, anded, 1, NULL);
    printf("SECTOR|offset=0x%05x|inside_not_ff=%" PRIu32 "|before_mismatches=%" PRIu32
           "|after_mismatches=%" PRIu32 "\n",
           MIDDLE_SECTOR_OFFSET, inside, before, after);
    if (inside != 0 || before != 0 || after != 0) {
        fail("sector", "a one-sector erase did not erase exactly that sector");
    }
}

static void step_straddle(void)
{
    // Both sectors are erased by step a and untouched since, so no erase is needed first.
    program_pattern("straddle_write", STRADDLE_OFFSET, STRADDLE_BYTES, 2);
    uint32_t crc = 0;
    uint32_t bad = verify("straddle_read", STRADDLE_OFFSET, STRADDLE_BYTES, pattern_byte, 2, &crc);
    printf("STRADDLE|offset=0x%05x|bytes=%u|mismatches=%" PRIu32 "|crc32=0x%08" PRIx32 "\n",
           STRADDLE_OFFSET, STRADDLE_BYTES, bad, crc);
    if (bad != 0) {
        fail("straddle", "a write across a sector boundary did not read back");
    }
}

static void step_cycles(void)
{
    uint32_t total = 0;
    uint32_t crc = 0;
    for (uint32_t cycle = 0; cycle < CYCLES; cycle++) {
        if (!check_rc("cycle_erase", esp_partition_erase_range(s_part, CYCLE_OFFSET, SECTOR))) {
            break;
        }
        program_pattern("cycle_write", CYCLE_OFFSET, SECTOR, 10u + cycle);
        crc = 0;
        total += verify("cycle_read", CYCLE_OFFSET, SECTOR, pattern_byte, 10u + cycle, &crc);
    }
    printf("CYCLES|cycles=%u|mismatches=%" PRIu32 "|last_crc32=0x%08" PRIx32 "\n", CYCLES, total,
           crc);
    if (total != 0) {
        fail("cycles", "an erase and program cycle did not read back");
    }
}

static uint32_t marker_offset(void)
{
    return s_part->size - SECTOR;
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    s_part = esp_partition_find_first(ESP_PARTITION_TYPE_DATA, 0x40, "scratch");
    if (s_part == NULL) {
        fail("partition", "no `scratch` data partition");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    printf("PART|offset=0x%06" PRIx32 "|size=0x%06" PRIx32 "\n", s_part->address, s_part->size);
    // Every fixed region the steps touch must lie inside the partition before anything is
    // written. esp_partition_* refuses an out-of-range call too; this makes a table that shrank
    // fail loudly up front instead of half-way through the steps.
    if (s_part->size < SCRATCH_MIN_BYTES) {
        fail("partition_size", "`scratch` is smaller than the regions the steps write");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }

    if (s_magic == STATE_MAGIC && s_stage == STAGE_RESTARTED) {
        s_stage = STAGE_DONE;
        uint32_t marker[2] = {0, 0};
        check_rc("marker_read", esp_partition_read(s_part, marker_offset(), marker, sizeof(marker)));
        bool intact = marker[0] == MARKER_MAGIC && marker[1] == ~MARKER_MAGIC;
        printf("PERSIST|marker=%d\n", intact ? 1 : 0);
        if (!intact) {
            fail("persist", "the marker programmed before the restart did not read back");
        }
        PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
        return;
    }
    if (s_magic == STATE_MAGIC && s_stage == STAGE_DONE) {
        PROBE_NOTE("sequence", "already done; power-cycle to run it again");
        return;
    }

    step_erase_all();
    step_pattern();
    step_and();
    step_sector();
    step_straddle();
    step_cycles();
    if (!s_ok) {
        PROBE_END(PROBE_NAME, "fail");
        return;
    }

    uint32_t marker[2] = {MARKER_MAGIC, ~MARKER_MAGIC};
    if (!check_rc("marker_write",
                  esp_partition_write(s_part, marker_offset(), marker, sizeof(marker)))) {
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    s_magic = STATE_MAGIC;
    s_stage = STAGE_RESTARTED;
    fflush(stdout);
    vTaskDelay(pdMS_TO_TICKS(20));
    esp_restart();
}
