// probe_wdt: a starved IDLE task and an interrupt-watchdog variant.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Three boots, with the place held in RTC slow memory:
//
//   1. `wdt_owner` (priority 3) takes the mutex `s_lock` and starts `wdt_waiter` (priority 4),
//      which blocks on the same mutex; the owner inherits priority 4 and then spins without
//      yielding. IDLE never runs, so the task watchdog fires, prints the tasks that did not feed
//      it and the task running, and panics. The watchdog test expects the emulator's TWDT event to name
//      the blocked task (`wdt_waiter`) and the mutex owner (`wdt_owner`), which the `TWDT` line
//      states before the spin.
//   2. The boot after it records the reset reason, then spins inside a critical section with
//      interrupts off, so the tick stops feeding the interrupt watchdog and it resets the chip.
//   3. The last boot records that reason and ends the run.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   TWDT     stage 1: owner, waiter, the holder FreeRTOS reports for the mutex, and the owner's
//            priority after inheritance
//   IWDT     stage 2: the reason the TWDT stage ended with, just before the spin
//   SUMMARY  stage 3: the reason after each stage
//
// Deterministic: fixed tasks, priorities and order of events; the watchdog timeouts come from
// sdkconfig.defaults.
//
// Consumed by: the watchdog test of tests/milestones/m7.rs.

#include <inttypes.h>
#include <stdint.h>

#include "esp_attr.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_wdt"

#define STATE_MAGIC 0x57445431u // "WDT1"
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_stage;
RTC_NOINIT_ATTR static uint32_t s_twdt_reason;
RTC_NOINIT_ATTR static uint32_t s_iwdt_reason;

enum { STAGE_TWDT = 1, STAGE_IWDT = 2, STAGE_DONE = 3 };

static SemaphoreHandle_t s_lock;
static volatile uint32_t s_spins;

static uint32_t raw_reset_cause(void)
{
    return (REG_READ(RTC_CNTL_RESET_STATE_REG) & RTC_CNTL_RESET_CAUSE_PROCPU_M) >>
           RTC_CNTL_RESET_CAUSE_PROCPU_S;
}

static void waiter_task(void *arg)
{
    (void)arg;
    // Blocks for good: the owner never gives the mutex back.
    xSemaphoreTake(s_lock, portMAX_DELAY);
    PROBE_FAIL("twdt_waiter", "the waiter obtained the mutex");
    vTaskDelete(NULL);
}

static void owner_task(void *arg)
{
    (void)arg;
    xSemaphoreTake(s_lock, portMAX_DELAY);
    TaskHandle_t waiter = NULL;
    xTaskCreate(waiter_task, "wdt_waiter", 2048, NULL, 4, &waiter);
    // The waiter has run and blocked by now: it has the higher priority.
    TaskHandle_t holder = xSemaphoreGetMutexHolder(s_lock);
    printf("TWDT|owner=%s|waiter=%s|holder=%s|waiter_state=%d|owner_prio=%u\n",
           pcTaskGetName(NULL), pcTaskGetName(waiter),
           holder != NULL ? pcTaskGetName(holder) : "none", (int)eTaskGetState(waiter),
           (unsigned)uxTaskPriorityGet(NULL));
    fflush(stdout);
    // Give the console a moment to drain; IDLE runs here once more, which is fine.
    vTaskDelay(pdMS_TO_TICKS(20));
    for (;;) {
        s_spins++;
    }
}

static void spin_critical(void)
{
    static portMUX_TYPE mux = portMUX_INITIALIZER_UNLOCKED;
    portENTER_CRITICAL(&mux);
    for (;;) {
        s_spins++;
    }
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    bool known = s_magic == STATE_MAGIC;
    printf("BOOT|stage=%" PRIu32 "|reason=%d|raw=0x%02" PRIx32 "\n", known ? s_stage : 0,
           (int)esp_reset_reason(), raw_reset_cause());

    if (known && s_stage == STAGE_TWDT) {
        s_twdt_reason = (uint32_t)esp_reset_reason();
        s_stage = STAGE_IWDT;
        printf("IWDT|twdt_reason=%" PRIu32 "\n", s_twdt_reason);
        fflush(stdout);
        vTaskDelay(pdMS_TO_TICKS(20));
        spin_critical();
        return;
    }
    if (known && s_stage == STAGE_IWDT) {
        s_iwdt_reason = (uint32_t)esp_reset_reason();
        s_stage = STAGE_DONE;
        printf("SUMMARY|twdt_reason=%" PRIu32 "|iwdt_reason=%" PRIu32 "\n", s_twdt_reason,
               s_iwdt_reason);
        bool ok = true;
        if (s_twdt_reason != ESP_RST_TASK_WDT) {
            PROBE_FAIL("twdt_reason", "the boot after the IDLE starvation was not a TWDT reset");
            ok = false;
        }
        if (s_iwdt_reason != ESP_RST_INT_WDT) {
            PROBE_FAIL("iwdt_reason", "the boot after the critical-section spin was not an IWDT reset");
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
    s_stage = STAGE_TWDT;
    s_lock = xSemaphoreCreateMutex();
    xTaskCreate(owner_task, "wdt_owner", 3072, NULL, 3, NULL);
}
