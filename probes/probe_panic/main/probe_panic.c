// probe_panic: a NULL read in a task. MIT.
// An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Two boots, with the place held in RTC slow memory:
//
//   1. app_main starts the task `panic_task`, which calls `probe_panic_outer`, which calls
//      `probe_panic_read_null`, which loads a word from address 0. The fault is a load access
//      fault (mcause 5), IDF prints its Guru Meditation text (`esp_system/panic.c`) and reboots.
//   2. The next boot reports the reset reason, which must be ESP_RST_PANIC, and ends the run.
//
// The two probe functions are `noinline`, so the frames the panic test compares with
// `riscv32-esp-elf-gdb bt` are `probe_panic_read_null`, `probe_panic_outer`, `panic_task`, with
// file and line from the unstripped ELF kept outside the repository (tests/fw/manifest.toml).
//
// Prints, as probe lines (probes/common/probe_line.h):
//   ARMED   boot 1, just before the fault: the task name and the address about to be read
//   AFTER   boot 2: the IDF reset reason and the raw RTC_CNTL cause
//
// Deterministic: the fault address, the task and the call chain are fixed by the source.
//
// Consumed by: the panic test of tests/milestones/m7.rs, where the emulator returns E_GUEST_PANIC at the fault.

#include <inttypes.h>
#include <stdint.h>

#include "esp_attr.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_panic"

#define STATE_MAGIC 0x50414e31u // "PAN1"
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_stage;

enum { STAGE_ARMED = 1, STAGE_DONE = 2 };

// Address the task reads. Zero is unmapped on the C3, so the read faults.
static volatile uintptr_t s_fault_addr = 0;

__attribute__((noinline)) static uint32_t probe_panic_read_null(void)
{
    const uint32_t *ptr = (const uint32_t *)s_fault_addr;
    return *ptr;
}

__attribute__((noinline)) static uint32_t probe_panic_outer(void)
{
    return probe_panic_read_null() + 1u;
}

static void panic_task(void *arg)
{
    (void)arg;
    printf("ARMED|task=%s|addr=0x%08" PRIxPTR "\n", pcTaskGetName(NULL), s_fault_addr);
    fflush(stdout);
    vTaskDelay(pdMS_TO_TICKS(20)); // let the console drain before the fault
    uint32_t value = probe_panic_outer();
    // Reached only if the read did not fault.
    printf("NOFAULT|value=0x%08" PRIx32 "\n", value);
    PROBE_FAIL("null_read", "a read of address 0 did not fault");
    PROBE_END(PROBE_NAME, "fail");
    vTaskDelete(NULL);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    if (s_magic == STATE_MAGIC && s_stage == STAGE_ARMED) {
        s_stage = STAGE_DONE;
        esp_reset_reason_t reason = esp_reset_reason();
        uint32_t raw = (REG_READ(RTC_CNTL_RESET_STATE_REG) & RTC_CNTL_RESET_CAUSE_PROCPU_M) >>
                       RTC_CNTL_RESET_CAUSE_PROCPU_S;
        printf("AFTER|reason=%d|raw=0x%02" PRIx32 "\n", (int)reason, raw);
        if (reason != ESP_RST_PANIC) {
            PROBE_FAIL("reset_reason", "the boot after the fault was not a panic reset");
            PROBE_END(PROBE_NAME, "fail");
            return;
        }
        PROBE_END(PROBE_NAME, "ok");
        return;
    }
    if (s_magic == STATE_MAGIC && s_stage == STAGE_DONE) {
        PROBE_NOTE("sequence", "already done; power-cycle to run it again");
        return;
    }
    s_magic = STATE_MAGIC;
    s_stage = STAGE_ARMED;
    xTaskCreate(panic_task, "panic_task", 3072, NULL, 5, NULL);
}
