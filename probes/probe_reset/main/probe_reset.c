// probe_reset: the reset-domain facts (ESP32-C3 TRM, Reset and Clock).
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// One firmware walks a sequence of resets, holding its place in RTC slow memory, which no reset
// in the sequence clears. Each boot prints what it found and then causes the next reset:
//
//   1. twenty `esp_restart()` calls. Every one of the twenty following boots must report reason
//      SW_CPU (raw 0x0C) and no `Memprot feature locked` line;
//   2. deep sleep for one second, expected to come back as DEEPSLEEP (raw 0x05);
//   3. a task-watchdog panic;
//   4. an interrupt-watchdog timeout, caused by spinning with interrupts off;
//   5. an RTC-watchdog system reset (stage 0, RESET_SYSTEM, so the RTC domain and this state
//      machine survive).
//
// Prints, as probe lines (probes/common/probe_line.h):
//   BOOT    stage, boot index, IDF reset reason, raw RTC_CNTL cause, wake cause
//   RTCRAM  the retained counters, so a reset that cleared RTC memory is visible
//   STORE   RTC_CNTL_STORE4..STORE7, which carry the ROM and bootloader hints (STORE6 is the
//           RTC entry hint)
//   SENS    a fixed set of SENSITIVE lock and PMS registers after each reset kind. Whether the
//           locks survive a software CPU reset is UNVERIFIED on silicon;
//           this is the capture that settles it
//   RESTART one line per restart-loop boot with its reason
//   SUMMARY the reason seen after each stage, once the sequence is done
//
// Compared against the merged `reset-domains` table.

#include <inttypes.h>
#include <string.h>

#include "esp_cpu.h"
#include "esp_private/esp_clk.h"
#include "esp_sleep.h"
#include "esp_system.h"
#include "esp_task_wdt.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/sensitive_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_reset"

// Twenty `esp_restart()` calls.
#define RESTART_TARGET 20

// Deep-sleep duration, one second.
#define DEEP_SLEEP_US 1000000ULL

// Hard bound on boots, so a stage that never resets cannot loop a device forever.
#define MAX_BOOTS (RESTART_TARGET + 16)

// Raw RTC_CNTL reset cause of a software CPU reset (the `restart-reason` limit).
#define RAW_RESET_SW_CPU 0x0cu

enum stage {
    STAGE_INIT = 0,
    STAGE_RESTART,
    STAGE_DEEP_SLEEP,
    STAGE_AFTER_DEEP_SLEEP,
    STAGE_TWDT,
    STAGE_AFTER_TWDT,
    STAGE_IWDT,
    STAGE_AFTER_IWDT,
    STAGE_RWDT,
    STAGE_AFTER_RWDT,
    STAGE_DONE,
};

// The state machine, in RTC slow memory. `RTC_NOINIT_ATTR` keeps it out of the startup clear, so
// the first boot recognises itself by the magic not matching.
#define STATE_MAGIC 0x50524231u // "PRB1"
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_stage;
RTC_NOINIT_ATTR static uint32_t s_boots;
RTC_NOINIT_ATTR static uint32_t s_restarts;
RTC_NOINIT_ATTR static uint32_t s_restart_reasons_ok;
RTC_NOINIT_ATTR static uint32_t s_stage_reason[STAGE_DONE + 1];

// A counter in ordinary RTC data memory, zeroed by the startup code only on a power-on reset.
// A difference between this and `s_boots` shows which resets cleared RTC data memory.
RTC_DATA_ATTR static uint32_t s_rtc_data_boots;

// SENSITIVE registers dumped after every reset. The first two are the lock registers whose
// retention is in question; the PMS constrain registers show whether a lock left a domain closed.
static const struct {
    const char *name;
    uint32_t addr;
} SENS_REGS[] = {
    {"rom_table_lock", SENSITIVE_ROM_TABLE_LOCK_REG},
    {"privilege_mode_sel_lock", SENSITIVE_PRIVILEGE_MODE_SEL_LOCK_REG},
    {"core_x_iram0_pms_constrain_0", SENSITIVE_CORE_X_IRAM0_PMS_CONSTRAIN_0_REG},
    {"core_x_dram0_pms_constrain_0", SENSITIVE_CORE_X_DRAM0_PMS_CONSTRAIN_0_REG},
    {"core_0_pif_pms_constrain_1", SENSITIVE_CORE_0_PIF_PMS_CONSTRAIN_1_REG},
};

static const char *stage_name(uint32_t stage)
{
    switch (stage) {
    case STAGE_INIT: return "init";
    case STAGE_RESTART: return "restart";
    case STAGE_DEEP_SLEEP: return "deep_sleep";
    case STAGE_AFTER_DEEP_SLEEP: return "after_deep_sleep";
    case STAGE_TWDT: return "twdt";
    case STAGE_AFTER_TWDT: return "after_twdt";
    case STAGE_IWDT: return "iwdt";
    case STAGE_AFTER_IWDT: return "after_iwdt";
    case STAGE_RWDT: return "rwdt";
    case STAGE_AFTER_RWDT: return "after_rwdt";
    default: return "done";
    }
}

static uint32_t raw_reset_cause(void)
{
    uint32_t raw = REG_READ(RTC_CNTL_RESET_STATE_REG) & RTC_CNTL_RESET_CAUSE_PROCPU_M;
    return raw >> RTC_CNTL_RESET_CAUSE_PROCPU_S;
}

static void print_boot(void)
{
    printf("BOOT|stage=%s|boot=%" PRIu32 "|reason=%d|raw=0x%02" PRIx32 "|wake=%d\n",
           stage_name(s_stage), s_boots, (int)esp_reset_reason(), raw_reset_cause(),
           (int)esp_sleep_get_wakeup_cause());
    printf("RTCRAM|noinit_boots=%" PRIu32 "|data_boots=%" PRIu32 "|restarts=%" PRIu32 "\n",
           s_boots, s_rtc_data_boots, s_restarts);
    printf("STORE|store4=0x%08" PRIx32 "|store5=0x%08" PRIx32 "|store6=0x%08" PRIx32
           "|store7=0x%08" PRIx32 "\n",
           REG_READ(RTC_CNTL_STORE4_REG), REG_READ(RTC_CNTL_STORE5_REG),
           REG_READ(RTC_CNTL_STORE6_REG), REG_READ(RTC_CNTL_STORE7_REG));
    for (size_t i = 0; i < sizeof(SENS_REGS) / sizeof(SENS_REGS[0]); i++) {
        printf("SENS|stage=%s|reg=%s|value=0x%08" PRIx32 "\n", stage_name(s_stage),
               SENS_REGS[i].name, REG_READ(SENS_REGS[i].addr));
    }
}

// Write key of the RTC watchdog protection register. ESP-IDF exposes it only from a HAL private
// header (`hal/rwdt_ll.h` RTC_CNTL_WDT_WKEY_VALUE), so the value is restated here.
#define RWDT_WRITE_KEY 0x50D83AA1u

// Arms the RTC watchdog so that stage 0 resets the system (not the RTC domain) after
// `timeout_ms`, and never feeds it again.
//
// ESP-IDF's `rtc_wdt.h` helpers are compiled for the ESP32 and ESP32-S2 only, so the C3 sequence
// is written out: unlock with the key, feed once, set the stage-0 hold count, write the
// configuration, lock again. The hold count is in RTC_SLOW_CLK cycles; the calibration word is
// the slow-clock period in microseconds in Q13.19 (STORE1, IDF `esp32c3/rom/rtc.h`), so
// cycles = us * 2^19 / cal. UNVERIFIED: whether `WDT_DELAY_SEL` scales stage 0 on silicon.
// An overshoot is harmless here, because the probe then spins forever.
static void arm_rtc_wdt(uint32_t timeout_ms)
{
    uint32_t cal = esp_clk_slowclk_cal_get();
    uint32_t cycles = 0xffffffffu;
    if (cal != 0) {
        uint64_t ticks = (((uint64_t)timeout_ms * 1000u) << 19) / cal;
        cycles = ticks > 0xffffffffu ? 0xffffffffu : (uint32_t)ticks;
    }
    // Stage 0 resets the system, stages 1 to 3 stay off, reset signal lengths at their maximum,
    // flash-boot mode off, no pause in sleep, enabled.
    uint32_t config0 = ((uint32_t)RTC_WDT_STG_SEL_RESET_SYSTEM << RTC_CNTL_WDT_STG0_S) |
                       (7u << RTC_CNTL_WDT_SYS_RESET_LENGTH_S) |
                       (7u << RTC_CNTL_WDT_CPU_RESET_LENGTH_S) | RTC_CNTL_WDT_EN_M;
    printf("RWDT|timeout_ms=%" PRIu32 "|slowclk_cal=%" PRIu32 "|cycles=%" PRIu32
           "|config0=0x%08" PRIx32 "\n",
           timeout_ms, cal, cycles, config0);
    REG_WRITE(RTC_CNTL_WDTWPROTECT_REG, RWDT_WRITE_KEY);
    REG_WRITE(RTC_CNTL_WDTFEED_REG, RTC_CNTL_WDT_FEED_M);
    REG_WRITE(RTC_CNTL_WDTCONFIG1_REG, cycles);
    REG_WRITE(RTC_CNTL_WDTCONFIG0_REG, config0);
    REG_WRITE(RTC_CNTL_WDTWPROTECT_REG, 0);
}

// Spins with interrupts off for `ms` milliseconds of CPU time, which is what makes the interrupt
// watchdog fire (the IWDT is fed from the tick interrupt).
static void spin_interrupts_off(uint32_t ms)
{
    uint32_t target = 160000u * ms; // 160 MHz; an overshoot only makes the watchdog surer.
    portDISABLE_INTERRUPTS();
    uint32_t start = esp_cpu_get_cycle_count();
    while (esp_cpu_get_cycle_count() - start < target) {
    }
    portENABLE_INTERRUPTS();
}

static void print_summary(void)
{
    printf("SUMMARY|restarts=%" PRIu32 "|restart_reasons_ok=%" PRIu32 "|target=%d\n", s_restarts,
           s_restart_reasons_ok, RESTART_TARGET);
    for (uint32_t stage = STAGE_AFTER_DEEP_SLEEP; stage <= STAGE_AFTER_RWDT; stage += 2) {
        printf("SUMMARY|stage=%s|reason=%" PRIu32 "\n", stage_name(stage), s_stage_reason[stage]);
    }
    if (s_restart_reasons_ok != RESTART_TARGET) {
        PROBE_FAIL("restart_loop", "not every restart reported the software CPU reset reason");
    }
}

void app_main(void)
{
    if (s_magic != STATE_MAGIC) {
        s_magic = STATE_MAGIC;
        s_stage = STAGE_INIT;
        s_boots = 0;
        s_restarts = 0;
        s_restart_reasons_ok = 0;
        memset((void *)s_stage_reason, 0, sizeof(s_stage_reason));
    }
    s_boots++;
    s_rtc_data_boots++;

    PROBE_BEGIN(PROBE_NAME);
    print_boot();

    if (s_boots > MAX_BOOTS) {
        PROBE_FAIL("sequence", "boot bound exceeded, the sequence is not advancing");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }

    switch (s_stage) {
    case STAGE_INIT:
        s_stage = STAGE_RESTART;
        break;

    case STAGE_RESTART:
        // The boot we are in is the result of restart number `s_restarts`.
        printf("RESTART|index=%" PRIu32 "|reason=%d|raw=0x%02" PRIx32 "\n", s_restarts,
               (int)esp_reset_reason(), raw_reset_cause());
        // Both the IDF enum and the raw RTC_CNTL cause are asserted. The `restart-reason` limit
        // names the raw value ("`esp_restart` 20 times gives 0x0C each time"), and the enum alone would not
        // catch it: `esp_reset_reason` maps several raw causes onto ESP_RST_SW, so a model that
        // reported 0x0B would pass twenty times while printing raw=0x0b.
        if (esp_reset_reason() == ESP_RST_SW && raw_reset_cause() == RAW_RESET_SW_CPU) {
            s_restart_reasons_ok++;
        } else if (esp_reset_reason() != ESP_RST_SW) {
            PROBE_FAIL("restart_loop", "reason after esp_restart was not ESP_RST_SW");
        } else {
            PROBE_FAIL("restart_loop", "raw reset cause after esp_restart was not 0x0c");
        }
        if (s_restarts >= RESTART_TARGET) {
            s_stage = STAGE_DEEP_SLEEP;
        }
        break;

    case STAGE_AFTER_DEEP_SLEEP:
    case STAGE_AFTER_TWDT:
    case STAGE_AFTER_IWDT:
    case STAGE_AFTER_RWDT:
        s_stage_reason[s_stage] = (uint32_t)esp_reset_reason();
        s_stage++;
        break;

    default:
        break;
    }

    // Cause the reset the current stage asks for. Every branch below ends in a reset, except
    // STAGE_DONE: a branch whose own reset could not be set up records the failure, moves to the
    // next stage and calls `esp_restart`, because falling out of the switch would return from
    // `app_main` with no reset, no DONE and no SUMMARY, and MAX_BOOTS could not help because no
    // further boot would happen.
    switch (s_stage) {
    case STAGE_RESTART:
        s_restarts++;
        fflush(stdout);
        esp_restart();
        break;

    case STAGE_DEEP_SLEEP:
        s_stage = STAGE_AFTER_DEEP_SLEEP;
        if (esp_sleep_enable_timer_wakeup(DEEP_SLEEP_US) != ESP_OK) {
            PROBE_FAIL("deep_sleep", "esp_sleep_enable_timer_wakeup failed");
            // A skipped stage still has to reset, or the sequence stalls here with no DONE and
            // no SUMMARY and MAX_BOOTS never fires, because no further boot happens.
            s_stage = STAGE_TWDT;
            fflush(stdout);
            esp_restart();
        }
        fflush(stdout);
        esp_deep_sleep_start();
        break;

    case STAGE_TWDT: {
        s_stage = STAGE_AFTER_TWDT;
        esp_task_wdt_config_t config = {
            .timeout_ms = 1000,
            .idle_core_mask = 0,
            .trigger_panic = true,
        };
        // The watchdog is already initialised by the startup code, so reconfigure it.
        // `esp_task_wdt_add(NULL)` returns ESP_ERR_INVALID_ARG when the main task is already
        // subscribed, which some CONFIG_ESP_TASK_WDT_CHECK_IDLE_TASK* configurations make
        // possible, so this branch is reachable and has to end in a reset like every other one.
        if (esp_task_wdt_reconfigure(&config) != ESP_OK ||
            esp_task_wdt_add(NULL) != ESP_OK) {
            PROBE_FAIL("twdt", "cannot configure the task watchdog");
            s_stage = STAGE_IWDT;
            fflush(stdout);
            esp_restart();
        }
        fflush(stdout);
        for (;;) {
            // Busy, never feeding: the watchdog panics and the panic handler reboots.
        }
    }

    case STAGE_IWDT:
        s_stage = STAGE_AFTER_IWDT;
        fflush(stdout);
        spin_interrupts_off(2000);
        PROBE_FAIL("iwdt", "spinning with interrupts off did not reset the chip");
        s_stage = STAGE_RWDT;
        fflush(stdout);
        esp_restart();
        break;

    case STAGE_RWDT:
        s_stage = STAGE_AFTER_RWDT;
        arm_rtc_wdt(1000);
        fflush(stdout);
        for (;;) {
            // Never fed: stage 0 resets the system but not the RTC domain, so this state
            // machine survives into the next boot.
        }

    case STAGE_DONE:
    default:
        print_summary();
        PROBE_END(PROBE_NAME, "ok");
        break;
    }
}
