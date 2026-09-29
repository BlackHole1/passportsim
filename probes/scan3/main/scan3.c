// scan3: the Wi-Fi and BLE probe of an earlier prototype, rebuilt from repository sources.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Provenance: ported from our earlier prototype firmware, which was itself derived from our
// earlier heap probe.
// The body below is that file unchanged except for these additions, so every line the
// prototype run printed is printed
// here byte for byte and in the same order:
//   - `#include "probe_line.h"` and a `PROBE|name=scan3|...` header line first;
//   - the return codes the prototype run recorded are checked (`check_rc`); a mismatch prints a
//     `FAIL` line only when something differs from that run: every `RC` line's code, except
//     the scan's `busy` (see the scan loop), `num`, `records` and `ms`;
//   - an access point outside the virtual air prints no SSID and only the vendor prefix of its
//     BSSID (`print_ap`), so a device run records no neighbour's identity;
//   - a `DONE|name=scan3|status=..` footer after the original `PROBE DONE` line.
//
// Wi-Fi: init, start, three non-blocking scan cycles, a wrong-state deinit, stop, deinit.
// BLE: a NimBLE broadcaster advertising with the fields of the official BLE demo: flags 0x06 and
// the complete local name "FoloPassport", non-connectable, general discoverable, forever
// (AdvData `0201060d09466f6c6f50617373706f7274`).
//
// Prints (the prototype's formats, valid probe lines of probes/common/probe_line.h): HEAP|, TASK|,
// EVT|, RC|, AP|. `t_ms` fields and heap numbers are the values that differ between engines;
// The scan test compares the ordered lines with timestamps excluded. An SSID holding `|` or `=` would
// break an AP line; the virtual air of the exits uses neither.
//
// Consumed by: tests/milestones/m8.rs (BLE advertising without Wi-Fi) and m12.rs (Wi-Fi scan).
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/event_groups.h"
#include "freertos/semphr.h"
#include "esp_heap_caps.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "esp_timer.h"
#include "nvs_flash.h"

#include "host/ble_hs.h"
#include "host/util/util.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

#include "probe_line.h"

#define PROBE_NAME "scan3"

// Set by `check_rc` when a return code differs from the recorded prototype run.
static bool s_failed;

// Compares a return code with the one the prototype run recorded, and
// prints a FAIL line on a mismatch.
static void check_rc(const char *what, int got, int want)
{
    if (got != want) {
        char detail[64];
        snprintf(detail, sizeof(detail), "rc %d where G2 s3b recorded %d", got, want);
        PROBE_FAIL(what, detail);
        s_failed = true;
    }
}

#define CAPS (MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT)

static void heap(const char *stage)
{
    printf("HEAP|%s|free=%u|largest=%u|min=%u|tasks=%u|t_ms=%lld\n", stage,
           (unsigned)heap_caps_get_free_size(CAPS),
           (unsigned)heap_caps_get_largest_free_block(CAPS),
           (unsigned)heap_caps_get_minimum_free_size(CAPS),
           (unsigned)uxTaskGetNumberOfTasks(),
           (long long)(esp_timer_get_time() / 1000));
}

static void tasks(const char *stage)
{
    UBaseType_t n = uxTaskGetNumberOfTasks() + 4;
    TaskStatus_t *st = calloc(n, sizeof(TaskStatus_t));
    if (!st) {
        return;
    }
    n = uxTaskGetSystemState(st, n, NULL);
    for (UBaseType_t i = 0; i < n; i++) {
        printf("TASK|%s|%s|prio=%u|hwm=%u\n", stage, st[i].pcTaskName,
               (unsigned)st[i].uxCurrentPriority, (unsigned)st[i].usStackHighWaterMark);
    }
    free(st);
}

#define BIT_GOT_IP    BIT0
#define BIT_SCAN_DONE BIT1
#define BIT_DISC      BIT2
#define BIT_STA_START BIT3
#define BIT_STA_STOP  BIT4
static EventGroupHandle_t s_eg;

static void on_evt(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    if (base == WIFI_EVENT) {
        printf("EVT|WIFI_EVENT|id=%d|t_ms=%lld\n", (int)id, (long long)(esp_timer_get_time() / 1000));
        if (id == WIFI_EVENT_STA_START) {
            xEventGroupSetBits(s_eg, BIT_STA_START);
        } else if (id == WIFI_EVENT_STA_STOP) {
            xEventGroupSetBits(s_eg, BIT_STA_STOP);
        } else if (id == WIFI_EVENT_SCAN_DONE) {
            wifi_event_sta_scan_done_t *d = data;
            printf("EVT|SCAN_DONE|status=%u|number=%u|scan_id=%u\n", (unsigned)d->status, (unsigned)d->number,
                   (unsigned)d->scan_id);
            xEventGroupSetBits(s_eg, BIT_SCAN_DONE);
        } else if (id == WIFI_EVENT_STA_DISCONNECTED) {
            wifi_event_sta_disconnected_t *d = data;
            printf("EVT|DISCONNECTED|reason=%u|rssi=%d\n", d->reason, d->rssi);
            xEventGroupSetBits(s_eg, BIT_DISC);
        }
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        xEventGroupSetBits(s_eg, BIT_GOT_IP);
    }
}

// The virtual air's access points have BSSIDs 02:00:00:47:32:xx.
// Those are printed in full, so the lines stay the prototype's ones. Any other access point
// is a real neighbour on a device run: its SSID is not printed and its BSSID is cut to the
// vendor prefix, so no capture holds the identity of someone else's network.
static const uint8_t VIRTUAL_BSSID_PREFIX[5] = {0x02, 0x00, 0x00, 0x47, 0x32};

static void print_ap(int cycle, const wifi_ap_record_t *rec)
{
    if (memcmp(rec->bssid, VIRTUAL_BSSID_PREFIX, sizeof(VIRTUAL_BSSID_PREFIX)) == 0) {
        printf("AP|%d|%.32s|ch=%u|rssi=%d|auth=%u|bssid=%02x:%02x:%02x:%02x:%02x:%02x\n", cycle,
               (char *)rec->ssid, rec->primary, rec->rssi, rec->authmode, rec->bssid[0],
               rec->bssid[1], rec->bssid[2], rec->bssid[3], rec->bssid[4], rec->bssid[5]);
    } else {
        printf("AP|%d|(neighbour)|ch=%u|rssi=%d|auth=%u|bssid=%02x:%02x:%02x:xx:xx:xx\n", cycle,
               rec->primary, rec->rssi, rec->authmode, rec->bssid[0], rec->bssid[1], rec->bssid[2]);
    }
}

static void wifi_scan3(void)
{
    s_eg = xEventGroupCreate();
    heap("wifi.0_before_netif");
    ESP_ERROR_CHECK(esp_netif_init());
    ESP_ERROR_CHECK(esp_event_loop_create_default());
    heap("wifi.1_after_netif_evloop");
    esp_netif_t *sta = esp_netif_create_default_wifi_sta();
    heap("wifi.2_after_default_sta");
    esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, on_evt, NULL);
    esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_evt, NULL);

    esp_err_t err = esp_wifi_deinit();
    printf("RC|deinit_before_init|%d\n", err);
    check_rc("deinit_before_init", err, ESP_ERR_WIFI_NOT_INIT);

    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    err = esp_wifi_init(&cfg);
    printf("RC|esp_wifi_init|%d\n", err);
    check_rc("esp_wifi_init", err, ESP_OK);
    heap("wifi.3_after_init");
    tasks("wifi.3_after_init");

    err = esp_wifi_set_storage(WIFI_STORAGE_RAM);
    printf("RC|esp_wifi_set_storage|%d\n", err);
    check_rc("esp_wifi_set_storage", err, ESP_OK);
    err = esp_wifi_set_mode(WIFI_MODE_STA);
    printf("RC|esp_wifi_set_mode|%d\n", err);
    check_rc("esp_wifi_set_mode", err, ESP_OK);
    err = esp_wifi_start();
    printf("RC|esp_wifi_start|%d\n", err);
    check_rc("esp_wifi_start", err, ESP_OK);
    EventBits_t b = xEventGroupWaitBits(s_eg, BIT_STA_START, pdFALSE, pdFALSE, pdMS_TO_TICKS(3000));
    printf("RC|sta_start_seen|%d\n", (b & BIT_STA_START) ? 1 : 0);
    check_rc("sta_start_seen", (b & BIT_STA_START) ? 1 : 0, 1);
    heap("wifi.4_after_start");
    tasks("wifi.4_after_start");

    for (int cycle = 1; cycle <= 3; cycle++) {
        xEventGroupClearBits(s_eg, BIT_SCAN_DONE);
        int64_t t0 = esp_timer_get_time();
        esp_err_t e_start = esp_wifi_scan_start(NULL, false);
        esp_err_t e_busy = esp_wifi_scan_start(NULL, false);
        b = xEventGroupWaitBits(s_eg, BIT_SCAN_DONE, pdTRUE, pdFALSE, pdMS_TO_TICKS(10000));
        uint16_t num = 0;
        esp_err_t e_num = esp_wifi_scan_get_ap_num(&num);
        wifi_ap_record_t recs[8];
        uint16_t cnt = 8;
        esp_err_t e_rec = esp_wifi_scan_get_ap_records(&cnt, recs);
        printf("RC|scan|cycle=%d|start=%d|busy=%d|done=%d|num_rc=%d|num=%u|rec_rc=%d|records=%u|ms=%lld\n", cycle,
               e_start, e_busy, (b & BIT_SCAN_DONE) ? 1 : 0, e_num, num, e_rec, cnt,
               (long long)((esp_timer_get_time() - t0) / 1000));
        // Not checked: `busy`, whose 12294 (ESP_ERR_WIFI_STATE) in the prototype run came from our
        // own HLE and is UNVERIFIED against the real driver, so checking it would
        // only test the HLE against itself; and `num`, `records` and `ms`, which depend on the air.
        check_rc("scan_start", e_start, ESP_OK);
        check_rc("scan_done", (b & BIT_SCAN_DONE) ? 1 : 0, 1);
        check_rc("scan_num", e_num, ESP_OK);
        check_rc("scan_records", e_rec, ESP_OK);
        for (int i = 0; i < cnt; i++) {
            print_ap(cycle, &recs[i]);
        }
        char stage[24];
        snprintf(stage, sizeof(stage), "wifi.5_scan%d", cycle);
        heap(stage);
    }

    err = esp_wifi_deinit();
    printf("RC|deinit_while_started|%d\n", err);
    check_rc("deinit_while_started", err, ESP_ERR_WIFI_NOT_STOPPED);

    xEventGroupClearBits(s_eg, BIT_STA_STOP);
    err = esp_wifi_stop();
    printf("RC|esp_wifi_stop|%d\n", err);
    check_rc("esp_wifi_stop", err, ESP_OK);
    b = xEventGroupWaitBits(s_eg, BIT_STA_STOP, pdFALSE, pdFALSE, pdMS_TO_TICKS(3000));
    printf("RC|sta_stop_seen|%d\n", (b & BIT_STA_STOP) ? 1 : 0);
    check_rc("sta_stop_seen", (b & BIT_STA_STOP) ? 1 : 0, 1);
    heap("wifi.7_after_stop");
    err = esp_wifi_deinit();
    printf("RC|esp_wifi_deinit|%d\n", err);
    check_rc("esp_wifi_deinit", err, ESP_OK);
    esp_netif_destroy_default_wifi(sta);
    heap("wifi.8_after_deinit");
    tasks("wifi.8_after_deinit");
    err = esp_wifi_deinit();
    printf("RC|deinit_again|%d\n", err);
    check_rc("deinit_again", err, ESP_ERR_WIFI_NOT_INIT);
}

static SemaphoreHandle_t s_synced;
static SemaphoreHandle_t s_host_done;
static uint8_t s_addr_type;
static const char *DEVICE_NAME = "FoloPassport";

static int gap_cb(struct ble_gap_event *ev, void *arg)
{
    (void)arg;
    printf("EVT|GAP|type=%d\n", ev->type);
    return 0;
}

// Same fields and parameters as the official demo_ble.c advertise().
static int advertise(void)
{
    struct ble_hs_adv_fields fields = { 0 };
    fields.flags = BLE_HS_ADV_F_DISC_GEN | BLE_HS_ADV_F_BREDR_UNSUP;
    fields.name = (const uint8_t *)DEVICE_NAME;
    fields.name_len = strlen(DEVICE_NAME);
    fields.name_is_complete = 1;
    int rc = ble_gap_adv_set_fields(&fields);
    if (rc != 0) {
        return rc;
    }
    struct ble_gap_adv_params params = { 0 };
    params.conn_mode = BLE_GAP_CONN_MODE_NON;
    params.disc_mode = BLE_GAP_DISC_MODE_GEN;
    return ble_gap_adv_start(s_addr_type, NULL, BLE_HS_FOREVER, &params, gap_cb, NULL);
}

static void on_sync(void)
{
    int rc = ble_hs_util_ensure_addr(0);
    if (rc == 0) {
        rc = ble_hs_id_infer_auto(0, &s_addr_type);
    }
    printf("EVT|BLE_SYNC|rc=%d|addr_type=%u|t_ms=%lld\n", rc, s_addr_type, (long long)(esp_timer_get_time() / 1000));
    if (rc == 0) {
        rc = advertise();
    }
    printf("RC|adv_start|%d\n", rc);
    check_rc("adv_start", rc, 0);
    xSemaphoreGive(s_synced);
}

static void host_task(void *arg)
{
    (void)arg;
    nimble_port_run();
    xSemaphoreGive(s_host_done);
    nimble_port_freertos_deinit();
}

static void ble_probe(void)
{
    s_synced = xSemaphoreCreateBinary();
    s_host_done = xSemaphoreCreateBinary();
    heap("ble.0_before");
    esp_err_t err = nimble_port_init();
    printf("RC|nimble_port_init|%d\n", err);
    check_rc("nimble_port_init", err, ESP_OK);
    heap("ble.1_after_port_init");
    tasks("ble.1_after_port_init");
    ble_svc_gap_init();
    ble_svc_gatt_init();
    ble_svc_gap_device_name_set(DEVICE_NAME);
    ble_hs_cfg.sync_cb = on_sync;
    nimble_port_freertos_init(host_task);
    BaseType_t ok = xSemaphoreTake(s_synced, pdMS_TO_TICKS(5000));
    printf("RC|ble_synced|%d\n", ok == pdTRUE);
    check_rc("ble_synced", ok == pdTRUE, 1);
    vTaskDelay(pdMS_TO_TICKS(1000));
    heap("ble.2_after_sync_adv");
    tasks("ble.2_after_sync_adv");
    ble_gap_adv_stop();
    int rc = nimble_port_stop();
    printf("RC|nimble_port_stop|%d\n", rc);
    check_rc("nimble_port_stop", rc, 0);
    if (rc == 0) {
        xSemaphoreTake(s_host_done, pdMS_TO_TICKS(3000));
        nimble_port_deinit();
    }
    vTaskDelay(pdMS_TO_TICKS(200));
    heap("ble.3_after_deinit");
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    heap("boot.app_main");
    tasks("boot.app_main");
    esp_err_t err = nvs_flash_init();
    printf("RC|nvs_flash_init|%d\n", err);
    check_rc("nvs_flash_init", err, ESP_OK);
    heap("boot.after_nvs");
    wifi_scan3();
    ble_probe();
    heap("end");
    printf("PROBE DONE\n");
    PROBE_END(PROBE_NAME, s_failed ? "fail" : "ok");
}
