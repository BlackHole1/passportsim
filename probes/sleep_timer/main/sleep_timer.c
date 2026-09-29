// sleep_timer: light and deep sleep timer wakes.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Two boots, with the place held in RTC slow memory:
//
//   1. Three light sleeps of 500 ms with a timer wake. Each must return ESP_OK with wake cause
//      TIMER, and esp_timer and the RTC time must both have advanced by at least the programmed
//      time (light sleep returns; RTC time is monotonic). Then a counter in RTC data
//      memory is set and the chip enters deep sleep for 1 s with a timer wake.
//   2. The boot after deep sleep must report reset reason DEEPSLEEP (raw RTC_CNTL cause 0x05),
//      wake cause TIMER, the RTC data counter intact, and an RTC time at least 1 s past the one
//      recorded just before sleeping (`rst:0x5 (DEEPSLEEP_RESET)` and wake cause TIMER
//      after the programmed time).
//
// Prints, as probe lines (probes/common/probe_line.h):
//   BOOT   stage, IDF reset reason, raw RTC_CNTL cause, wake cause
//   LIGHT  one line per light sleep: return code, wake cause, esp_timer and RTC deltas in ms
//   DEEP   boot 2: the retained counter and the RTC time slept, in ms
//
// Deterministic in the emulator: the sleep lengths are programmed. On silicon the deltas carry
// the wake latency, so a reader checks them as lower bounds, as the probe does.
//
// Consumed by: the sleep tests of tests/milestones/m10.rs (the official-image sleep test covers
// the same path at app level), for the reset cause and the RTC RAM retention.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>

#include "esp_attr.h"
#include "esp_rtc_time.h"
#include "esp_sleep.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "sleep_timer"

#define LIGHT_SLEEPS 3
#define LIGHT_SLEEP_US 500000ULL
#define DEEP_SLEEP_US 1000000ULL

// Raw RTC_CNTL reset cause after deep sleep (`rst:0x5`).
#define RAW_RESET_DEEPSLEEP 0x05u

#define STATE_MAGIC 0x534c5031u // "SLP1"
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_stage;
// Set on boot 1 when a light sleep failed, so the footer of boot 2 reports it.
RTC_NOINIT_ATTR static uint32_t s_light_failed;

// RTC data memory survives deep sleep; the startup code clears it only on a power-on reset.
RTC_DATA_ATTR static uint32_t s_deep_counter;
RTC_DATA_ATTR static uint64_t s_rtc_before_deep_us;

enum { STAGE_DEEP = 1, STAGE_DONE = 2 };

static uint32_t raw_reset_cause(void)
{
    return (REG_READ(RTC_CNTL_RESET_STATE_REG) & RTC_CNTL_RESET_CAUSE_PROCPU_M) >>
           RTC_CNTL_RESET_CAUSE_PROCPU_S;
}

static void drain_console(void)
{
    fflush(stdout);
    vTaskDelay(pdMS_TO_TICKS(20));
}

static bool light_sleeps(void)
{
    bool ok = true;
    for (int i = 1; i <= LIGHT_SLEEPS; i++) {
        drain_console();
        esp_sleep_enable_timer_wakeup(LIGHT_SLEEP_US);
        int64_t timer_before = esp_timer_get_time();
        uint64_t rtc_before = esp_rtc_get_time_us();
        esp_err_t rc = esp_light_sleep_start();
        int64_t timer_ms = (esp_timer_get_time() - timer_before) / 1000;
        uint64_t rtc_ms = (esp_rtc_get_time_us() - rtc_before) / 1000u;
        esp_sleep_wakeup_cause_t cause = esp_sleep_get_wakeup_cause();
        printf("LIGHT|%d|rc=%d|cause=%d|timer_ms=%" PRId64 "|rtc_ms=%" PRIu64 "\n", i, rc,
               (int)cause, timer_ms, rtc_ms);
        uint64_t want_ms = LIGHT_SLEEP_US / 1000u;
        if (rc != ESP_OK || cause != ESP_SLEEP_WAKEUP_TIMER || timer_ms < (int64_t)want_ms ||
            rtc_ms < want_ms) {
            PROBE_FAIL("light_sleep", "a light sleep did not return by timer after 500 ms");
            ok = false;
        }
    }
    esp_sleep_disable_wakeup_source(ESP_SLEEP_WAKEUP_TIMER);
    return ok;
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    bool known = s_magic == STATE_MAGIC;
    printf("BOOT|stage=%" PRIu32 "|reason=%d|raw=0x%02" PRIx32 "|wake=%d\n", known ? s_stage : 0,
           (int)esp_reset_reason(), raw_reset_cause(), (int)esp_sleep_get_wakeup_cause());

    if (known && s_stage == STAGE_DEEP) {
        s_stage = STAGE_DONE;
        uint64_t slept_ms = (esp_rtc_get_time_us() - s_rtc_before_deep_us) / 1000u;
        printf("DEEP|counter=%" PRIu32 "|rtc_ms=%" PRIu64 "\n", s_deep_counter, slept_ms);
        bool ok = s_light_failed == 0u;
        if (esp_reset_reason() != ESP_RST_DEEPSLEEP || raw_reset_cause() != RAW_RESET_DEEPSLEEP) {
            PROBE_FAIL("deep_reason", "the boot after deep sleep was not a DEEPSLEEP reset");
            ok = false;
        }
        if (esp_sleep_get_wakeup_cause() != ESP_SLEEP_WAKEUP_TIMER) {
            PROBE_FAIL("deep_cause", "the deep sleep wake cause was not TIMER");
            ok = false;
        }
        if (s_deep_counter != 1u) {
            PROBE_FAIL("rtc_data", "the RTC data counter did not survive deep sleep");
            ok = false;
        }
        if (slept_ms < DEEP_SLEEP_US / 1000u) {
            PROBE_FAIL("deep_time", "the RTC time advanced less than the programmed 1 s");
            ok = false;
        }
        PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
        return;
    }
    if (known && s_stage == STAGE_DONE) {
        PROBE_NOTE("sequence", "already done; power-cycle to run it again");
        return;
    }

    s_magic = STATE_MAGIC;
    s_stage = STAGE_DEEP;
    // The deep sleep stage runs either way: its facts are independent of the light sleeps. The
    // FAIL lines are already printed; the flag carries the verdict to the footer of boot 2.
    s_light_failed = light_sleeps() ? 0u : 1u;
    s_deep_counter = 1;
    drain_console();
    esp_sleep_enable_timer_wakeup(DEEP_SLEEP_US);
    s_rtc_before_deep_us = esp_rtc_get_time_us();
    esp_deep_sleep_start();
}
