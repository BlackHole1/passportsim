// probe_deadlock: a two-mutex deadlock. MIT.
// An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// `dl_task_a` takes `m1`, waits 10 ms, then asks for `m2`; `dl_task_b` takes `m2`, waits 10 ms,
// then asks for `m1`. Both have the same priority and both wait forever. app_main checks the
// state after 200 ms, prints it and the task table, and returns; from then on nothing in the
// image can make progress, which is the condition the emulator reports as E_DEADLOCK.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   DEADLOCK  the state of both tasks and the holder of each mutex
//   TASK      one line per task: name, FreeRTOS state (0 running, 1 ready, 2 blocked,
//             3 suspended, 4 deleted) and current priority, ordered by task number
//
// Deterministic: fixed tasks, priorities and delays.
//
// Consumed by: the deadlock test of tests/milestones/m7.rs.

#include <inttypes.h>
#include <stdlib.h>

#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

#include "probe_line.h"

#define PROBE_NAME "probe_deadlock"

#define MAX_TASKS 16

static SemaphoreHandle_t s_m1;
static SemaphoreHandle_t s_m2;
static volatile int s_a_got_both;
static volatile int s_b_got_both;

static void task_a(void *arg)
{
    (void)arg;
    xSemaphoreTake(s_m1, portMAX_DELAY);
    vTaskDelay(pdMS_TO_TICKS(10));
    xSemaphoreTake(s_m2, portMAX_DELAY);
    s_a_got_both = 1;
    vTaskDelete(NULL);
}

static void task_b(void *arg)
{
    (void)arg;
    xSemaphoreTake(s_m2, portMAX_DELAY);
    vTaskDelay(pdMS_TO_TICKS(10));
    xSemaphoreTake(s_m1, portMAX_DELAY);
    s_b_got_both = 1;
    vTaskDelete(NULL);
}

static const char *holder_name(SemaphoreHandle_t mutex)
{
    TaskHandle_t holder = xSemaphoreGetMutexHolder(mutex);
    return holder != NULL ? pcTaskGetName(holder) : "none";
}

static int by_number(const void *left, const void *right)
{
    const TaskStatus_t *l = (const TaskStatus_t *)left;
    const TaskStatus_t *r = (const TaskStatus_t *)right;
    return (l->xTaskNumber > r->xTaskNumber) - (l->xTaskNumber < r->xTaskNumber);
}

static void print_tasks(void)
{
    static TaskStatus_t tasks[MAX_TASKS];
    UBaseType_t count = uxTaskGetSystemState(tasks, MAX_TASKS, NULL);
    qsort(tasks, count, sizeof(tasks[0]), by_number);
    for (UBaseType_t i = 0; i < count; i++) {
        printf("TASK|%s|state=%d|prio=%u\n", tasks[i].pcTaskName, (int)tasks[i].eCurrentState,
               (unsigned)tasks[i].uxCurrentPriority);
    }
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    s_m1 = xSemaphoreCreateMutex();
    s_m2 = xSemaphoreCreateMutex();
    TaskHandle_t a = NULL;
    TaskHandle_t b = NULL;
    xTaskCreate(task_a, "dl_task_a", 2048, NULL, 5, &a);
    xTaskCreate(task_b, "dl_task_b", 2048, NULL, 5, &b);
    vTaskDelay(pdMS_TO_TICKS(200));

    int a_state = (int)eTaskGetState(a);
    int b_state = (int)eTaskGetState(b);
    printf("DEADLOCK|a_state=%d|b_state=%d|m1_holder=%s|m2_holder=%s|a_done=%d|b_done=%d\n",
           a_state, b_state, holder_name(s_m1), holder_name(s_m2), s_a_got_both, s_b_got_both);
    print_tasks();

    bool ok = a_state == eBlocked && b_state == eBlocked && s_a_got_both == 0 &&
              s_b_got_both == 0 && xSemaphoreGetMutexHolder(s_m1) == a &&
              xSemaphoreGetMutexHolder(s_m2) == b;
    if (!ok) {
        PROBE_FAIL("deadlock", "the two tasks are not each blocked on the other's mutex");
    }
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
