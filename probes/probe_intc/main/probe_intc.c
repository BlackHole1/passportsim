// probe_intc: the interrupt-fabric facts (ESP32-C3 TRM, Interrupt Matrix).
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// The C3 fabric is 62 sources routed by a MAP register each onto 31 CPU lines, with a 4-bit
// priority per line and one threshold register. This probe drives it through the two
// `FROM_CPU` software sources that IDF leaves free (0 and 1 belong to the cross-core and system
// paths), because they are the only sources a firmware can assert on demand with no peripheral.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   INTC    one line per allocated source, and one per state of the MAP re-route: the CPU line,
//           the MAP register read back, the line's PRI, its TYPE bit (0 level, 1 edge) and the
//           ENABLE bit
//   LAT     per-iteration latency from the trigger store to handler entry, in CPU cycles
//   LATSUM  min, max and mean of the LAT samples
//   THRESH  whether a line below THRESH stayed pending and ran once THRESH was restored
//   MAP     which handler ran after the MAP register moved a source to another CPU line
//   EDGE    deliveries per trigger with the line configured edge and level
//   YIELD   FROM_CPU triggers, handler entries and high-priority task wakeups
//
// Compared against the QEMU console text and the model property tests.
//
// Cycle counts are read with `esp_cpu_get_cycle_count` (CSR mcycle). They are latency in CPU
// cycles, not a retired-instruction count: the C3 has no retired-instruction counter a firmware
// can read, so a latency "in instructions" is reported here as cycles. UNVERIFIED
// that the two agree on silicon; under the emulator at CPI 1 they do.

#include <inttypes.h>
#include <stdbool.h>
#include <string.h>

#include "esp_attr.h"
#include "esp_cpu.h"
#include "esp_intr_alloc.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "soc/interrupt_core0_reg.h"
#include "soc/interrupts.h"
#include "soc/soc.h"
#include "soc/system_reg.h"

#include "probe_line.h"

#define PROBE_NAME "probe_intc"

// FROM_CPU 0 and 1 are taken by IDF (cross-core and system). 2 and 3 are free.
#define TRIG_A_SOURCE ETS_FROM_CPU_INTR2_SOURCE
#define TRIG_A_SET_REG SYSTEM_CPU_INTR_FROM_CPU_2_REG
#define TRIG_A_MAP_REG INTERRUPT_CORE0_CPU_INTR_FROM_CPU_2_MAP_REG
#define TRIG_B_SOURCE ETS_FROM_CPU_INTR3_SOURCE
#define TRIG_B_SET_REG SYSTEM_CPU_INTR_FROM_CPU_3_REG
#define TRIG_B_MAP_REG INTERRUPT_CORE0_CPU_INTR_FROM_CPU_3_MAP_REG

// Latency samples. 16 is enough to show the spread without flooding a console capture.
#define LAT_SAMPLES 16
// Triggers per edge/level round.
#define EDGE_TRIGGERS 8
// Triggers in the yield round.
#define YIELD_TRIGGERS 8
// Spin bound while an interrupt is expected to stay blocked, in CPU cycles at 160 MHz
// (about 1 ms). Long enough that a delivery would have happened, short enough for no watchdog.
#define BLOCKED_SPIN_CYCLES 160000u

static volatile uint32_t s_entries_a;
static volatile uint32_t s_entries_b;
static volatile uint32_t s_entry_cycle;
static volatile uint32_t s_trigger_cycle;
static SemaphoreHandle_t s_wake;
static volatile uint32_t s_woken;

// The CPU lines `esp_intr_alloc` chose, so a handler can acknowledge its own line.
static volatile int s_line_a = -1;
static volatile int s_line_b = -1;

// Clears both FROM_CPU sources.
//
// Both handlers clear both, because `run_map_reroute` points one source at the other's CPU line:
// the handler that then runs is not the one that owns the source, and a FROM_CPU source is a
// level, so whoever runs has to take it down or the line re-asserts forever. Clearing an already
// clear register costs one store and is the same cost in every sample.
static inline void IRAM_ATTR clear_sources(void)
{
    REG_WRITE(TRIG_A_SET_REG, 0);
    REG_WRITE(TRIG_B_SET_REG, 0);
}

// Acknowledges the CPU line's edge latch, when the line is configured edge-triggered.
//
// An edge-type CPU interrupt latches in INTERRUPT_CORE0_CPU_INT_CLEAR_REG and stays pending
// until that bit is written; clearing the *source* is not enough. IDF does not do it for us:
// `_global_interrupt_handler` (components/riscv/interrupt.c) calls the handler and nothing else,
// and `intr_alloc.c` acknowledges exactly once at allocation, and only for
// `ESP_INTR_FLAG_EDGE`, which this probe deliberately does not pass because it reconfigures the
// type itself. Without this the first edge delivery re-enters the handler forever and the
// interrupt watchdog resets the chip.
static inline void IRAM_ATTR ack_if_edge(int line)
{
    if (line >= 0 && ((REG_READ(INTERRUPT_CORE0_CPU_INT_TYPE_REG) >> line) & 1u) != 0u) {
        esp_cpu_intr_edge_ack(line);
    }
}

// Handler for FROM_CPU 2: stamps the entry cycle, clears the sources and acknowledges the line.
static void IRAM_ATTR isr_a(void *arg)
{
    (void)arg;
    s_entry_cycle = esp_cpu_get_cycle_count();
    clear_sources();
    ack_if_edge(s_line_a);
    s_entries_a++;
}

// Handler for FROM_CPU 3: clears the sources, gives the semaphore and asks for a yield, which is
// the shape of a real FreeRTOS `FROM_CPU` yield path.
static void IRAM_ATTR isr_b(void *arg)
{
    (void)arg;
    clear_sources();
    ack_if_edge(s_line_b);
    s_entries_b++;
    BaseType_t higher = pdFALSE;
    xSemaphoreGiveFromISR(s_wake, &higher);
    if (higher == pdTRUE) {
        s_woken++;
        portYIELD_FROM_ISR();
    }
}

// Reads the 4-bit priority of CPU line `line`.
static uint32_t line_pri(int line)
{
    return REG_READ(INTC_INT_PRIO_REG(line)) & 0xfu;
}

// Prints the routing of one source on one CPU line.
static void print_route_line(const char *name, int source, uint32_t map_reg, int line)
{
    uint32_t map = REG_READ(map_reg) & INTERRUPT_CORE0_CPU_INTR_FROM_CPU_0_MAP_V;
    uint32_t type = (REG_READ(INTERRUPT_CORE0_CPU_INT_TYPE_REG) >> line) & 1u;
    uint32_t enable = (REG_READ(INTERRUPT_CORE0_CPU_INT_ENABLE_REG) >> line) & 1u;
    printf("INTC|%s|source=%d|line=%d|map=%" PRIu32 "|pri=%" PRIu32 "|type=%" PRIu32
           "|enable=%" PRIu32 "|thresh=%" PRIu32 "\n",
           name, source, line, map, line_pri(line), type, enable,
           REG_READ(INTERRUPT_CORE0_CPU_INT_THRESH_REG) & 0xfu);
}

// Prints the routing of one allocated source and checks the MAP register holds the line the
// allocator chose.
static void print_route(const char *name, int source, uint32_t map_reg, intr_handle_t handle)
{
    int line = esp_intr_get_intno(handle);
    print_route_line(name, source, map_reg, line);
    uint32_t map = REG_READ(map_reg) & INTERRUPT_CORE0_CPU_INTR_FROM_CPU_0_MAP_V;
    if (map != (uint32_t)line) {
        PROBE_FAIL("map_readback", "MAP register does not hold the allocated line");
    }
}

// Total handler entries, either handler. `run_map_reroute` sends source A to the other line, so
// waiting on a specific counter would hang there.
static uint32_t entries_total(void)
{
    return s_entries_a + s_entries_b;
}

// Triggers FROM_CPU 2 once and waits for any handler to run. Returns false on the spin bound,
// leaving the source cleared so a later delivery cannot arrive out of its round.
static bool trigger_a(void)
{
    uint32_t before = entries_total();
    s_entry_cycle = 0;
    s_trigger_cycle = esp_cpu_get_cycle_count();
    REG_WRITE(TRIG_A_SET_REG, 1);
    uint32_t start = s_trigger_cycle;
    while (entries_total() == before) {
        if (esp_cpu_get_cycle_count() - start > BLOCKED_SPIN_CYCLES) {
            REG_WRITE(TRIG_A_SET_REG, 0);
            return false;
        }
    }
    return true;
}

// Triggers FROM_CPU 2 once and returns the cycles from the trigger store to handler entry, or 0
// if the handler did not run within the spin bound.
static uint32_t measure_latency(void)
{
    if (!trigger_a()) {
        return 0;
    }
    return s_entry_cycle - s_trigger_cycle;
}

static void run_latency(void)
{
    uint32_t min = 0xffffffffu, max = 0, sum = 0, taken = 0;
    for (int i = 0; i < LAT_SAMPLES; i++) {
        uint32_t cycles = measure_latency();
        printf("LAT|index=%d|cycles=%" PRIu32 "\n", i, cycles);
        if (cycles == 0) {
            PROBE_FAIL("latency", "handler did not run within the spin bound");
            continue;
        }
        min = cycles < min ? cycles : min;
        max = cycles > max ? cycles : max;
        sum += cycles;
        taken++;
    }
    if (taken == 0) {
        return;
    }
    printf("LATSUM|samples=%" PRIu32 "|min=%" PRIu32 "|max=%" PRIu32 "|mean=%" PRIu32 "\n", taken,
           min, max, sum / taken);
}

// Raises THRESH above the line's priority, triggers, and checks the handler stays pending; then
// restores THRESH and checks it runs. This is the same register FreeRTOS uses for a critical
// section, so the old value is put back before anything else runs.
static void run_thresh(intr_handle_t handle)
{
    int line = esp_intr_get_intno(handle);
    uint32_t pri = line_pri(line);
    uint32_t saved = REG_READ(INTERRUPT_CORE0_CPU_INT_THRESH_REG) & 0xfu;
    uint32_t before = s_entries_a;

    REG_WRITE(INTERRUPT_CORE0_CPU_INT_THRESH_REG, pri + 1);
    REG_WRITE(TRIG_A_SET_REG, 1);
    uint32_t start = esp_cpu_get_cycle_count();
    while (esp_cpu_get_cycle_count() - start < BLOCKED_SPIN_CYCLES) {
    }
    uint32_t during = s_entries_a - before;

    REG_WRITE(INTERRUPT_CORE0_CPU_INT_THRESH_REG, saved);
    start = esp_cpu_get_cycle_count();
    while (s_entries_a - before == during) {
        if (esp_cpu_get_cycle_count() - start > BLOCKED_SPIN_CYCLES) {
            break;
        }
    }
    uint32_t after = s_entries_a - before;

    printf("THRESH|pri=%" PRIu32 "|blocked_at=%" PRIu32 "|restored_to=%" PRIu32
           "|ran_while_blocked=%" PRIu32 "|ran_after_restore=%" PRIu32 "\n",
           pri, pri + 1, saved, during, after - during);
    if (during != 0) {
        PROBE_FAIL("thresh", "handler ran while THRESH was above its priority");
    }
    if (after - during != 1) {
        PROBE_FAIL("thresh", "pending interrupt did not run exactly once after restore");
    }
}

// Moves FROM_CPU 2 to the CPU line FROM_CPU 3 uses by writing its MAP register, shows that the
// *other* handler then runs, and puts the mapping back. This is the "MAP re-route"
// check: the fabric fact that the MAP register alone decides which CPU line a source reaches.
//
// The destination is the line `esp_intr_alloc` gave source B, so it already has a handler, a
// priority and its enable bit set; pointing a source at a line with no handler would leave the
// level asserted with nothing to clear it.
static void run_map_reroute(intr_handle_t handle_a, intr_handle_t handle_b)
{
    int from = esp_intr_get_intno(handle_a);
    int to = esp_intr_get_intno(handle_b);
    uint32_t saved = REG_READ(TRIG_A_MAP_REG);
    if (from == to) {
        PROBE_NOTE("map_reroute", "both sources share one CPU line, nothing to move");
        return;
    }

    print_route_line("from_cpu_2_before", TRIG_A_SOURCE, TRIG_A_MAP_REG, from);
    REG_WRITE(TRIG_A_MAP_REG, (uint32_t)to);
    print_route_line("from_cpu_2_rerouted", TRIG_A_SOURCE, TRIG_A_MAP_REG, to);

    uint32_t a_before = s_entries_a, b_before = s_entries_b;
    bool ran = trigger_a();
    uint32_t a_ran = s_entries_a - a_before, b_ran = s_entries_b - b_before;
    printf("MAP|from_line=%d|to_line=%d|map=%" PRIu32 "|delivered=%d|isr_a=%" PRIu32
           "|isr_b=%" PRIu32 "\n",
           from, to, REG_READ(TRIG_A_MAP_REG) & INTERRUPT_CORE0_CPU_INTR_FROM_CPU_0_MAP_V,
           ran ? 1 : 0, a_ran, b_ran);
    if (!ran) {
        PROBE_FAIL("map_reroute", "no handler ran after the MAP register moved the source");
    } else if (b_ran != 1 || a_ran != 0) {
        PROBE_FAIL("map_reroute", "the source did not reach the line its MAP register names");
    }

    REG_WRITE(TRIG_A_MAP_REG, saved);
    print_route_line("from_cpu_2_restored", TRIG_A_SOURCE, TRIG_A_MAP_REG, from);
    if ((REG_READ(TRIG_A_MAP_REG) & INTERRUPT_CORE0_CPU_INTR_FROM_CPU_0_MAP_V) !=
        (uint32_t)from) {
        PROBE_FAIL("map_reroute", "the MAP register did not take the original line back");
    }
}

// Counts deliveries per trigger with the line set edge, then level. The handler clears the
// source and acknowledges the edge latch either way, so neither configuration can loop.
static void run_edge_level(intr_handle_t handle)
{
    int line = esp_intr_get_intno(handle);
    uint32_t saved = REG_READ(INTERRUPT_CORE0_CPU_INT_TYPE_REG);
    for (int edge = 1; edge >= 0; edge--) {
        uint32_t type = saved;
        if (edge) {
            type |= (1u << line);
        } else {
            type &= ~(1u << line);
        }
        REG_WRITE(INTERRUPT_CORE0_CPU_INT_TYPE_REG, type);
        uint32_t before = s_entries_a;
        uint32_t missed = 0;
        for (int i = 0; i < EDGE_TRIGGERS; i++) {
            if (measure_latency() == 0) {
                missed++;
            }
        }
        printf("EDGE|type=%s|line=%d|triggers=%d|isr=%" PRIu32 "|missed=%" PRIu32 "\n",
               edge ? "edge" : "level", line, EDGE_TRIGGERS, s_entries_a - before, missed);
        if (s_entries_a - before != EDGE_TRIGGERS) {
            PROBE_FAIL("edge_level", "deliveries did not equal triggers");
        }
    }
    REG_WRITE(INTERRUPT_CORE0_CPU_INT_TYPE_REG, saved);
    // The type bit is back to whatever IDF chose, so `ack_if_edge` would no longer clear this
    // line. Any latch the edge round left behind is taken down here instead.
    esp_cpu_intr_edge_ack(line);
}

// The high-priority task the FROM_CPU 3 handler wakes.
static void waiter(void *arg)
{
    uint32_t *taken = (uint32_t *)arg;
    for (;;) {
        if (xSemaphoreTake(s_wake, portMAX_DELAY) == pdTRUE) {
            (*taken)++;
        }
    }
}

// Triggers FROM_CPU 3 and counts handler entries and task wakeups, the yield path.
static void run_yield(void)
{
    static uint32_t taken;
    taken = 0;
    s_woken = 0;
    // `run_map_reroute` made isr_b run once, which gave the semaphore with nobody waiting. The
    // count is drained here so this round's `taken` is its own triggers and nothing else.
    while (xSemaphoreTake(s_wake, 0) == pdTRUE) {
    }
    TaskHandle_t task = NULL;
    if (xTaskCreate(waiter, "waiter", 3072, &taken, configMAX_PRIORITIES - 2, &task) != pdPASS) {
        PROBE_FAIL("yield", "cannot create the waiter task");
        return;
    }
    vTaskDelay(pdMS_TO_TICKS(20));
    uint32_t before = s_entries_b;
    for (int i = 0; i < YIELD_TRIGGERS; i++) {
        REG_WRITE(TRIG_B_SET_REG, 1);
        vTaskDelay(pdMS_TO_TICKS(2));
    }
    vTaskDelay(pdMS_TO_TICKS(20));
    printf("YIELD|triggers=%d|isr=%" PRIu32 "|woken=%" PRIu32 "|taken=%" PRIu32 "\n",
           YIELD_TRIGGERS, s_entries_b - before, s_woken, taken);
    if (s_entries_b - before != YIELD_TRIGGERS || taken != YIELD_TRIGGERS) {
        PROBE_FAIL("yield", "handler entries or task wakeups did not equal triggers");
    }
    vTaskDelete(task);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    s_wake = xSemaphoreCreateCounting(64, 0);
    if (s_wake == NULL) {
        PROBE_FAIL("setup", "cannot create the semaphore");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    REG_WRITE(TRIG_A_SET_REG, 0);
    REG_WRITE(TRIG_B_SET_REG, 0);

    intr_handle_t handle_a = NULL, handle_b = NULL;
    if (esp_intr_alloc(TRIG_A_SOURCE, ESP_INTR_FLAG_LEVEL1, isr_a, NULL, &handle_a) != ESP_OK ||
        esp_intr_alloc(TRIG_B_SOURCE, ESP_INTR_FLAG_LEVEL1, isr_b, NULL, &handle_b) != ESP_OK) {
        PROBE_FAIL("setup", "esp_intr_alloc failed");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    s_line_a = esp_intr_get_intno(handle_a);
    s_line_b = esp_intr_get_intno(handle_b);
    print_route("from_cpu_2", TRIG_A_SOURCE, TRIG_A_MAP_REG, handle_a);
    print_route("from_cpu_3", TRIG_B_SOURCE, TRIG_B_MAP_REG, handle_b);

    run_latency();
    run_thresh(handle_a);
    run_map_reroute(handle_a, handle_b);
    run_edge_level(handle_a);
    run_yield();

    esp_intr_free(handle_a);
    esp_intr_free(handle_b);
    PROBE_END(PROBE_NAME, "ok");
}
