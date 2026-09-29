// probe_clocks: the one-clock invariants of the emulator's time model, checked on silicon.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Four time sources must stay consistent because they are derived from one emulated clock:
// esp_timer (SYSTIMER, 16 ticks/us), the FreeRTOS tick, the CPU
// cycle counter (CSR mcycle) and the RTC slow counter. This probe samples all four around each
// of five phases, prints the deltas, and **applies the agreement rule itself**, so a capture
// that is graded by its DONE status and its FAIL lines cannot pass with inconsistent clocks:
//
//   - busy loop, `esp_rom_delay_us`, `vTaskDelay` and the esp_timer poll: esp_timer, the tick
//     count and the RTC time agree pairwise within one tick;
//   - busy loop and `esp_rom_delay_us` only: the cycle count agrees with them within one tick;
//   - WFI and light sleep: the cycle count advances by cpu_mhz x (wall - idle - stall), so it is
//     expected to fall behind and is printed as a fact, not judged.
//
// The light-sleep phase judges esp_timer against the RTC only. FreeRTOS does not step its tick
// across an explicit `esp_light_sleep_start`: only the tickless-idle hook calls `vTaskStepTick`,
// and this probe sleeps from a task with tickless idle off, so the tick legitimately stands
// still for the whole sleep. Judging it there would fail a correct chip.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   CLKCFG  CPU, APB and XTAL frequency, the RTC slow-clock calibration word and the tick rate
//   CLK     one line per phase: nominal duration and the delta of each of the four sources
//   SLEEP   the light-sleep phase's wake cause
//   FAIL    `what=clock_consistency` when a judged pair is more than one tick apart
//
// The RTC delta is read with `esp_clk_rtc_time` (esp_private/esp_clk.h). That header is part of
// ESP-IDF and is what `esp_sleep` itself uses; a probe may use a private IDF header because it
// is firmware, not emulator code.

#include <inttypes.h>

#include "esp_cpu.h"
#include "esp_private/esp_clk.h"
#include "esp_rom_sys.h"
#include "esp_sleep.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#include "probe_line.h"

#define PROBE_NAME "probe_clocks"

// Nominal duration of every timed phase, in microseconds. 20 ms is long enough that a one-tick
// (100 us at the default 10 ms tick... see below) rounding does not dominate, and short enough
// that a console capture stays small.
#define PHASE_US 20000

// One FreeRTOS tick in microseconds: the tolerance of the agreement rule.
#define TICK_US ((int64_t)(1000000 / configTICK_RATE_HZ))

// Which sources a phase is judged on.
typedef enum {
    // esp_timer, tick and RTC agree pairwise within one tick.
    RULE_WALL,
    // ... and the cycle count agrees with esp_timer too.
    RULE_WALL_AND_CYCLES,
    // esp_timer and RTC only: the tick stands still across an explicit light sleep.
    RULE_TIMER_AND_RTC,
} clk_rule_t;

// Sample of the four time sources.
typedef struct {
    int64_t timer_us;
    uint32_t ticks;
    uint32_t cycles;
    uint64_t rtc_us;
} clk_sample_t;

static void sample(clk_sample_t *out)
{
    // Order matters only in that the four reads should be adjacent; the cycle counter is read
    // between the two 64-bit reads so no single source is systematically last.
    out->timer_us = esp_timer_get_time();
    out->cycles = esp_cpu_get_cycle_count();
    out->ticks = (uint32_t)xTaskGetTickCount();
    out->rtc_us = esp_clk_rtc_time();
}

// One judged pair. `detail` names the phase and the pair, so a capture says what broke.
static void judge(const char *phase, const char *pair, int64_t left_us, int64_t right_us)
{
    int64_t apart = left_us > right_us ? left_us - right_us : right_us - left_us;
    if (apart > TICK_US) {
        printf("FAIL|what=clock_consistency|detail=%s %s are %" PRId64
               " us apart, over the %" PRId64 " us tick\n",
               phase, pair, apart, TICK_US);
    }
}

static void report(const char *phase, uint32_t nominal_us, const clk_sample_t *a,
                   const clk_sample_t *b, clk_rule_t rule)
{
    int64_t timer_us = b->timer_us - a->timer_us;
    uint32_t ticks = b->ticks - a->ticks;
    uint32_t cycles = b->cycles - a->cycles;
    uint64_t rtc_us = b->rtc_us - a->rtc_us;
    printf("CLK|phase=%s|nominal_us=%" PRIu32 "|timer_us=%" PRId64 "|ticks=%" PRIu32
           "|cycles=%" PRIu32 "|rtc_us=%" PRIu64 "\n",
           phase, nominal_us, timer_us, ticks, cycles, rtc_us);
    fflush(stdout);

    int64_t tick_span_us = (int64_t)ticks * TICK_US;
    int64_t rtc_span_us = (int64_t)rtc_us;
    judge(phase, "esp_timer and rtc", timer_us, rtc_span_us);
    if (rule != RULE_TIMER_AND_RTC) {
        judge(phase, "esp_timer and tick", timer_us, tick_span_us);
        judge(phase, "tick and rtc", tick_span_us, rtc_span_us);
    }
    if (rule == RULE_WALL_AND_CYCLES) {
        int64_t mhz = esp_clk_cpu_freq() / 1000000;
        judge(phase, "esp_timer and cycles", timer_us, mhz > 0 ? (int64_t)cycles / mhz : 0);
    }
}

// Spins on the cycle counter for `us` microseconds of CPU time. Nothing else runs: this is the
// phase where the cycle count and the wall clock must agree exactly.
static void busy_loop(uint32_t us)
{
    uint32_t hz = (uint32_t)esp_clk_cpu_freq();
    uint32_t target = (uint32_t)((uint64_t)hz / 1000000u * us);
    uint32_t start = esp_cpu_get_cycle_count();
    while (esp_cpu_get_cycle_count() - start < target) {
    }
}

static void print_config(void)
{
    printf("CLKCFG|cpu_hz=%d|apb_hz=%d|xtal_hz=%d|rtc_slow_cal=%" PRIu32 "|tick_hz=%d\n",
           esp_clk_cpu_freq(), esp_clk_apb_freq(), esp_clk_xtal_freq(), esp_clk_slowclk_cal_get(),
           (int)configTICK_RATE_HZ);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    print_config();

    clk_sample_t before, after;

    // 1. Busy loop on the cycle counter.
    sample(&before);
    busy_loop(PHASE_US);
    sample(&after);
    report("busy_loop", PHASE_US, &before, &after, RULE_WALL_AND_CYCLES);

    // 2. ROM delay. `esp_rom_delay_us` is the ROM `ets_delay_us` path.
    sample(&before);
    esp_rom_delay_us(PHASE_US);
    sample(&after);
    report("rom_delay_us", PHASE_US, &before, &after, RULE_WALL_AND_CYCLES);

    // 3. `vTaskDelay`: the task blocks, IDLE runs and the CPU may enter WFI, so the cycle count
    //    is expected to fall behind the other three.
    sample(&before);
    vTaskDelay(pdMS_TO_TICKS(PHASE_US / 1000));
    sample(&after);
    report("task_delay", PHASE_US, &before, &after, RULE_WALL);

    // 4. `esp_timer` one-shot wait: the same wall time, driven by SYSTIMER alarms instead of the
    //    tick, which is the path a poll fast-forward has to keep honest.
    sample(&before);
    int64_t deadline = esp_timer_get_time() + PHASE_US;
    while (esp_timer_get_time() < deadline) {
        vTaskDelay(1);
    }
    sample(&after);
    report("timer_poll", PHASE_US, &before, &after, RULE_WALL);

    // 5. Light sleep with a timer wake.
    if (esp_sleep_enable_timer_wakeup(PHASE_US) != ESP_OK) {
        PROBE_FAIL("light_sleep", "esp_sleep_enable_timer_wakeup failed");
    } else {
        // Everything above has to be on the wire before the chip sleeps: a part that never wakes
        // takes the console with it, and the earlier phases are still facts worth having.
        fflush(stdout);
        sample(&before);
        esp_err_t err = esp_light_sleep_start();
        sample(&after);
        report("light_sleep", PHASE_US, &before, &after, RULE_TIMER_AND_RTC);
        printf("SLEEP|phase=light_sleep|err=%d|cause=%d\n", (int)err,
               (int)esp_sleep_get_wakeup_cause());
        if (err != ESP_OK) {
            PROBE_FAIL("light_sleep", "esp_light_sleep_start did not return ESP_OK");
        }
    }

    PROBE_END(PROBE_NAME, "ok");
}
