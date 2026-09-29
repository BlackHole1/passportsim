// hle_probe: nested guest calls from HLE hooks.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific
// code.
//
// The probe calls five small `noinline` functions, the *hook points*. Each has a native body that
// does the work directly, so the probe runs and passes on silicon and under QEMU as it is. The
// emulator's HLE layer binds these symbols by name and replaces each body with a handler that
// does the same work through nested guest calls (`HleAction::Call`), so the same lines
// are printed with the work done by continuations instead:
//
//   hle_probe_hook_delay(ms)       vTaskDelay: a nested call that blocks and switches tasks
//   hle_probe_hook_malloc(size)    heap_caps_malloc: a nested call that returns a value
//   hle_probe_hook_post(id, word)  esp_event_post to the default loop: a nested call that wakes
//                                  another task
//   hle_probe_hook_isr_give(q)     xQueueSendFromISR with `&woken` from interrupt context: the
//                                  scratch out-parameter case
//   hle_probe_hook_free(ptr)       heap_caps_free, so the heap returns to where it started
//
// The hook symbol names are the binding contract with the HLE layer. They are chosen here and
// are UNVERIFIED against a hook table until an HLE package binds them.
//
// Sequence:
//   1. main calls each hook once, printing task and heap state before and after.
//   2. Two tasks of different priority call the delay hook at the same time, so two outstanding
//      continuations cross a context switch.
//   3. A task blocked inside the delay hook is deleted, which drops its continuation; the heap and
//      the task count must come back once IDLE has cleaned up.
//   4. A FROM_CPU software interrupt calls the ISR hook, which wakes a waiting task.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   STATE  stage, current task, its priority, task count, internal free and largest block
//   CALL   one line per hook call: hook, inputs, result, and what changed
//   ORDER  the order in which the two delay callers of step 2 returned
//   DELETE step 3: whether the task count and the free heap came back
//   ISR    step 4: the hook's `woken` result and whether the waiter received the item
//
// Deterministic: no clock values are printed, only tick deltas and heap numbers, which are
// functions of the image and the scheduler.
//
// Consumed by: the `hle_probe` milestone test.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>

#include "esp_attr.h"
#include "esp_event.h"
#include "esp_heap_caps.h"
#include "esp_intr_alloc.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "soc/interrupts.h"
#include "soc/soc.h"
#include "soc/system_reg.h"

#include "probe_line.h"

#define PROBE_NAME "hle_probe"

#define CAPS (MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT)

// The delay of step 1, in milliseconds.
#define DELAY_MS 100u

ESP_EVENT_DEFINE_BASE(HLE_PROBE_EVENT);

// ---------------------------------------------------------------------------------------------
// Hook points. `noinline` and `used` keep each a real symbol with its own entry PC. `noipa` also
// stops GCC from specializing a caller on what it can see of the body (constant propagation,
// dropped arguments, a known return value): the HLE layer replaces the body, so every caller must
// pass the full arguments and use the returned value exactly as the prototype says.

__attribute__((noinline, noipa, used)) int32_t hle_probe_hook_delay(uint32_t ms)
{
    vTaskDelay(pdMS_TO_TICKS(ms));
    return 0;
}

__attribute__((noinline, noipa, used)) void *hle_probe_hook_malloc(size_t size)
{
    return heap_caps_malloc(size, CAPS);
}

__attribute__((noinline, noipa, used)) void hle_probe_hook_free(void *ptr)
{
    heap_caps_free(ptr);
}

__attribute__((noinline, noipa, used)) int32_t hle_probe_hook_post(int32_t id, uint32_t word)
{
    return esp_event_post(HLE_PROBE_EVENT, id, &word, sizeof(word), portMAX_DELAY);
}

__attribute__((noinline, noipa, used)) IRAM_ATTR int32_t hle_probe_hook_isr_give(QueueHandle_t queue,
                                                                        uint32_t item)
{
    BaseType_t woken = pdFALSE;
    xQueueSendFromISR(queue, &item, &woken);
    return woken == pdTRUE ? 1 : 0;
}

// ---------------------------------------------------------------------------------------------

static void state(const char *stage)
{
    printf("STATE|%s|task=%s|prio=%u|tasks=%u|free=%u|largest=%u\n", stage, pcTaskGetName(NULL),
           (unsigned)uxTaskPriorityGet(NULL), (unsigned)uxTaskGetNumberOfTasks(),
           (unsigned)heap_caps_get_free_size(CAPS),
           (unsigned)heap_caps_get_largest_free_block(CAPS));
}

static SemaphoreHandle_t s_event_seen;
static volatile uint32_t s_event_word;
static volatile int32_t s_event_id = -1;

static void on_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    (void)base;
    s_event_id = id;
    s_event_word = *(uint32_t *)data;
    xSemaphoreGive(s_event_seen);
}

static bool step_single_calls(void)
{
    bool ok = true;

    // 100 ms spans several ticks at any tick rate the probes use (10 at the 100 Hz default), so an
    // early return, a return one tick late and a call that never blocked are all visible. The
    // delay starts on a tick boundary, so it measures exactly `want` ticks; one more is allowed
    // for a higher-priority task that runs first when the delay ends.
    state("delay.before");
    vTaskDelay(1);
    TickType_t want = pdMS_TO_TICKS(DELAY_MS);
    TickType_t t0 = xTaskGetTickCount();
    int32_t rc = hle_probe_hook_delay(DELAY_MS);
    TickType_t ticks = xTaskGetTickCount() - t0;
    state("delay.after");
    printf("CALL|delay|ms=%u|rc=%" PRId32 "|ticks=%u|want=%u\n", DELAY_MS, rc, (unsigned)ticks,
           (unsigned)want);
    if (rc != 0 || want < 2 || ticks < want || ticks > want + 1) {
        PROBE_FAIL("delay", "the delay hook did not block for the requested ticks");
        ok = false;
    }

    size_t free_before = heap_caps_get_free_size(CAPS);
    state("malloc.before");
    void *block = hle_probe_hook_malloc(64);
    size_t free_after = heap_caps_get_free_size(CAPS);
    state("malloc.after");
    printf("CALL|malloc|size=64|ok=%d|usable=%u|taken=%u\n", block != NULL ? 1 : 0,
           block != NULL ? (unsigned)heap_caps_get_allocated_size(block) : 0u,
           (unsigned)(free_before - free_after));
    hle_probe_hook_free(block);
    size_t free_back = heap_caps_get_free_size(CAPS);
    printf("CALL|free|restored=%d\n", free_back == free_before ? 1 : 0);
    if (block == NULL || free_back != free_before) {
        PROBE_FAIL("malloc", "the malloc hook failed or the free hook did not restore the heap");
        ok = false;
    }

    state("post.before");
    rc = hle_probe_hook_post(7, 0x1234abcdu);
    bool seen = xSemaphoreTake(s_event_seen, pdMS_TO_TICKS(1000)) == pdTRUE;
    state("post.after");
    printf("CALL|post|id=7|rc=%" PRId32 "|seen=%d|seen_id=%" PRId32 "|word=0x%08" PRIx32 "\n", rc,
           seen ? 1 : 0, s_event_id, s_event_word);
    if (rc != 0 || !seen || s_event_id != 7 || s_event_word != 0x1234abcdu) {
        PROBE_FAIL("post", "the posted event did not reach its handler intact");
        ok = false;
    }
    return ok;
}

// Step 2: two outstanding delay calls across a context switch.
static QueueHandle_t s_order;

static void delay_caller(void *arg)
{
    uint32_t ms = (uint32_t)(uintptr_t)arg;
    hle_probe_hook_delay(ms);
    char tag = pcTaskGetName(NULL)[4]; // "hle_a" or "hle_b"
    xQueueSend(s_order, &tag, portMAX_DELAY);
    vTaskDelete(NULL);
}

static bool step_two_callers(void)
{
    s_order = xQueueCreate(2, sizeof(char));
    // `hle_a` has the higher priority and the longer delay, so it starts first and returns last.
    xTaskCreate(delay_caller, "hle_a", 2048, (void *)(uintptr_t)30u, 6, NULL);
    xTaskCreate(delay_caller, "hle_b", 2048, (void *)(uintptr_t)10u, 5, NULL);
    char first = '?';
    char second = '?';
    bool got = xQueueReceive(s_order, &first, pdMS_TO_TICKS(1000)) == pdTRUE &&
               xQueueReceive(s_order, &second, pdMS_TO_TICKS(1000)) == pdTRUE;
    printf("ORDER|first=%c|second=%c\n", first, second);
    vQueueDelete(s_order);
    if (!got || first != 'b' || second != 'a') {
        PROBE_FAIL("two_callers", "the two delay calls did not return in delay order");
        return false;
    }
    return true;
}

// Step 3: a task deleted while its delay call is outstanding.
static void victim(void *arg)
{
    (void)arg;
    hle_probe_hook_delay(1000);
    PROBE_FAIL("victim", "the deleted task returned from its delay");
    vTaskDelete(NULL);
}

static bool step_delete(void)
{
    vTaskDelay(pdMS_TO_TICKS(20)); // let IDLE free the step 2 tasks first
    UBaseType_t tasks_before = uxTaskGetNumberOfTasks();
    size_t free_before = heap_caps_get_free_size(CAPS);
    TaskHandle_t handle = NULL;
    xTaskCreate(victim, "hle_victim", 2048, NULL, 5, &handle);
    vTaskDelay(pdMS_TO_TICKS(50));
    vTaskDelete(handle);
    vTaskDelay(pdMS_TO_TICKS(20)); // IDLE frees the TCB and stack
    bool tasks_back = uxTaskGetNumberOfTasks() == tasks_before;
    bool heap_back = heap_caps_get_free_size(CAPS) == free_before;
    printf("DELETE|task=hle_victim|tasks_restored=%d|free_restored=%d\n", tasks_back ? 1 : 0,
           heap_back ? 1 : 0);
    if (!tasks_back || !heap_back) {
        PROBE_FAIL("delete", "deleting a task inside a hook left tasks or heap behind");
        return false;
    }
    return true;
}

// Step 4: the ISR hook, from a FROM_CPU software interrupt. FROM_CPU 2 is free (probe_intc).
static QueueHandle_t s_isr_queue;
static volatile int32_t s_isr_woken = -1;

static void IRAM_ATTR isr(void *arg)
{
    (void)arg;
    REG_WRITE(SYSTEM_CPU_INTR_FROM_CPU_2_REG, 0);
    s_isr_woken = hle_probe_hook_isr_give(s_isr_queue, 0x5a5a0001u);
    if (s_isr_woken == 1) {
        portYIELD_FROM_ISR();
    }
}

static volatile uint32_t s_isr_item;
static SemaphoreHandle_t s_isr_waiter_done;

static void isr_waiter(void *arg)
{
    (void)arg;
    uint32_t item = 0;
    if (xQueueReceive(s_isr_queue, &item, pdMS_TO_TICKS(1000)) == pdTRUE) {
        s_isr_item = item;
    }
    xSemaphoreGive(s_isr_waiter_done);
    vTaskDelete(NULL);
}

static bool step_isr(void)
{
    s_isr_queue = xQueueCreate(1, sizeof(uint32_t));
    s_isr_waiter_done = xSemaphoreCreateBinary();
    intr_handle_t handle = NULL;
    if (esp_intr_alloc(ETS_FROM_CPU_INTR2_SOURCE, 0, isr, NULL, &handle) != ESP_OK) {
        PROBE_FAIL("isr_alloc", "esp_intr_alloc of FROM_CPU 2 failed");
        return false;
    }
    // The waiter has the higher priority, so it is blocked on the queue before the trigger.
    xTaskCreate(isr_waiter, "hle_waiter", 2048, NULL, 6, NULL);
    REG_WRITE(SYSTEM_CPU_INTR_FROM_CPU_2_REG, SYSTEM_CPU_INTR_FROM_CPU_2);
    bool done = xSemaphoreTake(s_isr_waiter_done, pdMS_TO_TICKS(1000)) == pdTRUE;
    esp_intr_free(handle);
    printf("ISR|woken=%" PRId32 "|received=%d|item=0x%08" PRIx32 "\n", s_isr_woken,
           done && s_isr_item == 0x5a5a0001u ? 1 : 0, s_isr_item);
    if (!done || s_isr_woken != 1 || s_isr_item != 0x5a5a0001u) {
        PROBE_FAIL("isr", "the ISR hook did not wake the waiting task with the item");
        return false;
    }
    return true;
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    bool ok = true;
    s_event_seen = xSemaphoreCreateBinary();
    if (esp_event_loop_create_default() != ESP_OK ||
        esp_event_handler_register(HLE_PROBE_EVENT, ESP_EVENT_ANY_ID, on_event, NULL) != ESP_OK) {
        PROBE_FAIL("event_loop", "the default event loop could not be set up");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    state("start");
    ok &= step_single_calls();
    ok &= step_two_callers();
    ok &= step_delete();
    ok &= step_isr();
    state("end");
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
