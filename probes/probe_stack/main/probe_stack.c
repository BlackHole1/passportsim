// probe_stack: a stack overflow caught by the hardware guard.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Two boots, with the place held in RTC slow memory:
//
//   1. The task `stack_task`, with a 2048-byte stack, recurses through a function holding a
//      256-byte local array until the stack pointer leaves the stack. The hardware stack guard
//      raises the panic IDF prints as "Stack protection fault" with `Detected in task
//      "stack_task"` (IDF `esp_system/port/arch/riscv/panic_arch.c`), and the chip reboots.
//   2. The next boot reports the reset reason, which must be ESP_RST_PANIC, and the recursion
//      depth the task reached before the fault.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   ARMED   boot 1, from the main task before `stack_task` starts: the task name and its stack size
//   AFTER   boot 2: IDF reset reason, raw RTC_CNTL cause, recursion depth reached
//
// Deterministic: the depth is a function of the stack size, the frame size and the compiler.
//
// Consumed by: tests/milestones/m7.rs, where the emulator reports a stack-guard reason naming the task.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "esp_attr.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_stack"

#define STACK_BYTES 2048
#define FRAME_BYTES 256
#define MAX_DEPTH 100000u

#define STATE_MAGIC 0x53544b31u // "STK1"
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_stage;
RTC_NOINIT_ATTR static uint32_t s_depth;

enum { STAGE_ARMED = 1, STAGE_DONE = 2 };

__attribute__((noinline)) static uint32_t probe_stack_recurse(uint32_t depth)
{
    uint8_t frame[FRAME_BYTES];
    memset(frame, (int)(depth & 0xffu), sizeof(frame));
    s_depth = depth;
    // The bound is far past what a 2048-byte stack holds; it exists so the compiler does not
    // reject the function as infinite recursion. The sum after the call keeps it from becoming a
    // tail call the compiler could turn into a loop.
    if (depth >= MAX_DEPTH) {
        return 0;
    }
    return probe_stack_recurse(depth + 1u) + frame[depth % FRAME_BYTES];
}

// Set by `stack_task` only if the recursion returns, which it must not.
static volatile bool s_returned;
static volatile uint32_t s_value;

// Does nothing but recurse. It never calls printf, whose own frames need more than a kilobyte:
// a print here could overflow the 2048-byte stack before the recursion starts, and the guard
// would then fire in the console path instead of in `probe_stack_recurse`.
static void stack_task(void *arg)
{
    (void)arg;
    s_value = probe_stack_recurse(1u);
    s_returned = true;
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
        printf("AFTER|reason=%d|raw=0x%02" PRIx32 "|depth=%" PRIu32 "\n", (int)reason, raw,
               s_depth);
        bool ok = reason == ESP_RST_PANIC;
        if (!ok) {
            PROBE_FAIL("reset_reason", "the boot after the overflow was not a panic reset");
        }
        PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
        return;
    }
    if (s_magic == STATE_MAGIC && s_stage == STAGE_DONE) {
        PROBE_NOTE("sequence", "already done; power-cycle to run it again");
        return;
    }
    s_magic = STATE_MAGIC;
    s_stage = STAGE_ARMED;
    s_depth = 0;
    // Printed from the main task, whose stack is large, before the small task exists.
    printf("ARMED|task=stack_task|stack=%d\n", STACK_BYTES);
    fflush(stdout);
    vTaskDelay(pdMS_TO_TICKS(20));
    xTaskCreate(stack_task, "stack_task", STACK_BYTES, NULL, 5, NULL);
    // The fault reboots the chip long before this wait ends; reaching the end is the failure.
    vTaskDelay(pdMS_TO_TICKS(2000));
    if (s_returned) {
        printf("NOFAULT|value=%" PRIu32 "\n", s_value);
        PROBE_FAIL("stack_guard", "the recursion returned");
    } else {
        PROBE_FAIL("stack_guard", "no fault within 2 s of starting the recursion");
    }
    PROBE_END(PROBE_NAME, "fail");
}
