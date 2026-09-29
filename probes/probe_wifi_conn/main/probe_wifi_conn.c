// probe_wifi_conn: the Wi-Fi association half, on silicon.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// The failed-association test needs `esp_wifi_connect()` and the `WIFI_EVENT_STA_DISCONNECTED`
// `reason=` path from a class A source: a capture from the chip, not a Python model. This probe
// drives exactly that and nothing else:
//
//   nvs_flash_init, esp_netif_init, esp_event_loop_create_default, the default STA netif,
//   esp_wifi_init, esp_wifi_set_mode(WIFI_MODE_STA), esp_wifi_start, esp_wifi_connect,
//   then esp_wifi_disconnect, esp_wifi_stop, esp_wifi_deinit.
//
// **No credential is involved anywhere.** The SSID is CONFIG_PROBE_WIFI_CONN_SSID, whose default
// names no access point, so the association ends in `WIFI_REASON_NO_AP_FOUND` (201) without any
// password being needed, held or printed. There is no password option in `Kconfig.projbuild`, and
// `esp_wifi_set_storage(WIFI_STORAGE_RAM)` keeps the station configuration out of NVS.
//
// **Bounded.** The probe waits at most CONFIG_PROBE_WIFI_CONN_WAIT_MS for the disconnect event,
// never re-connects on disconnect, and then tears down, so a device run always terminates.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   RC    one line per call, `RC|<function>|<esp_err_t as a number>`, in call order.
//         `esp_netif_create_default_wifi_sta` returns a handle rather than a code; it is reported
//         as 0 for a handle and -1 (ESP_FAIL) for NULL.
//   EVT   one line per `WIFI_EVENT`, printed from the event handler so the console order is the
//         arrival order, `EVT|WIFI_EVENT|seq=<n>|id=<n>`; the disconnect event adds `|reason=<n>`.
//         This is what settles the `id=43` versus `id=2` ordering of the scan erratum, and what
//         gives `reason=` a silicon source.
//   WIFI  the summary: whether the SSID is still the placeholder, how many events arrived, and
//         whether the disconnect arrived or the wait timed out.
//   NOTE  the wait timed out, or the disconnect reason was not 201.
//   FAIL  a step that had to work did not, so the capture is not the intended one.
//
// Secrets (docs/secrets.md): no MAC, no unique id, no BSSID and no SSID text is ever
// printed. The SSID appears only as whether it still equals the compiled-in placeholder.
//
// Flash: the probe calls no flash write API, and **nothing it runs writes the device's NVS**.
// `CONFIG_ESP_WIFI_NVS_ENABLED=n` and the PHY calibration store off (sdkconfig.defaults) remove
// ESP-IDF's two routine NVS writers, `esp_wifi_set_storage(WIFI_STORAGE_RAM)` keeps the station
// configuration in RAM, and `nvs_flash_init` here has no `nvs_flash_erase` recovery. So a device
// run writes only the three segments the planner wrote. `partitions.csv` beside this file is the
// Passport's own layout, so that flash leaves cardid at 0x356000 where it is.
//
// Consumed by: the Wi-Fi driver rows of specs/hle/idf-5.5.3/wifi.toml.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "esp_err.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/event_groups.h"
#include "freertos/task.h"
#include "nvs_flash.h"
#include "sdkconfig.h"

#include "probe_line.h"

#define PROBE_NAME "probe_wifi_conn"

/// The Kconfig default. The run prints whether the configured SSID still equals it, never the
/// SSID itself.
#define PLACEHOLDER_SSID "passport-emu-absent-ap"

/// `WIFI_REASON_NO_AP_FOUND`, the outcome an SSID that does not exist is expected to give. It is
/// written here as a number on purpose: the probe reports what the chip said and only *notes* a
/// difference, because pinning the number is what the capture is for.
#define EXPECTED_REASON 201

#define BIT_DISCONNECTED BIT0

static EventGroupHandle_t s_events;
static int s_event_count;
static int s_sta_start_seen;
static int s_disconnect_count;
static int s_last_reason = -1;

/// Every `WIFI_EVENT`, in arrival order. Printing here rather than in `app_main` is deliberate:
/// the console order is then the order the event loop delivered them in.
static void on_wifi_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    if (base != WIFI_EVENT) {
        return;
    }
    int seq = ++s_event_count;
    if (id == WIFI_EVENT_STA_START) {
        s_sta_start_seen = 1;
    }
    if (id == WIFI_EVENT_STA_DISCONNECTED) {
        const wifi_event_sta_disconnected_t *event = data;
        // Nothing but the reason is read: the event also carries the SSID and the BSSID of the
        // access point, which the secrets policy keeps off the console.
        s_last_reason = event != NULL ? (int)event->reason : -1;
        s_disconnect_count++;
        printf("EVT|WIFI_EVENT|seq=%d|id=%" PRId32 "|reason=%d\n", seq, id, s_last_reason);
        // The probe never re-connects: one attempt, one outcome, a run that ends.
        xEventGroupSetBits(s_events, BIT_DISCONNECTED);
        return;
    }
    printf("EVT|WIFI_EVENT|seq=%d|id=%" PRId32 "\n", seq, id);
}

static void rc(const char *what, esp_err_t code)
{
    printf("RC|%s|%d\n", what, (int)code);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    bool ok = true;

    // Deliberately **without** the `nvs_flash_erase` recovery every other probe has. With
    // CONFIG_ESP_WIFI_NVS_ENABLED=n and the PHY calibration store off (sdkconfig.defaults),
    // nothing in this probe needs NVS, so an unmountable partition is a fact to report rather
    // than a reason to erase the device's own data. The code is printed and the run goes on.
    esp_err_t nvs_rc = nvs_flash_init();
    rc("nvs_flash_init", nvs_rc);

    s_events = xEventGroupCreate();
    rc("esp_netif_init", esp_netif_init());
    rc("esp_event_loop_create_default", esp_event_loop_create_default());
    esp_netif_t *sta = esp_netif_create_default_wifi_sta();
    rc("esp_netif_create_default_wifi_sta", sta != NULL ? ESP_OK : ESP_FAIL);
    rc("esp_event_handler_register",
       esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, on_wifi_event, NULL));

    wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
    esp_err_t init_rc = esp_wifi_init(&init);
    rc("esp_wifi_init", init_rc);
    rc("esp_wifi_set_storage", esp_wifi_set_storage(WIFI_STORAGE_RAM));
    rc("esp_wifi_set_mode", esp_wifi_set_mode(WIFI_MODE_STA));

    wifi_config_t config = {0};
    snprintf((char *)config.sta.ssid, sizeof(config.sta.ssid), "%s", CONFIG_PROBE_WIFI_CONN_SSID);
    // No password field is set and none exists: an open threshold is all an SSID that is not
    // there ever needs.
    config.sta.threshold.authmode = WIFI_AUTH_OPEN;
    rc("esp_wifi_set_config", esp_wifi_set_config(WIFI_IF_STA, &config));

    esp_err_t start_rc = esp_wifi_start();
    rc("esp_wifi_start", start_rc);
    esp_err_t connect_rc = esp_wifi_connect();
    rc("esp_wifi_connect", connect_rc);

    EventBits_t bits = xEventGroupWaitBits(s_events, BIT_DISCONNECTED, pdFALSE, pdFALSE,
                                           pdMS_TO_TICKS(CONFIG_PROBE_WIFI_CONN_WAIT_MS));
    bool disconnected = (bits & BIT_DISCONNECTED) != 0;
    if (!disconnected) {
        PROBE_NOTE("disconnect_wait", "no WIFI_EVENT_STA_DISCONNECTED within the configured wait");
        PROBE_FAIL("disconnect", "the association attempt neither connected nor disconnected");
        ok = false;
    } else if (s_last_reason != EXPECTED_REASON) {
        char why[96];
        snprintf(why, sizeof(why), "reason %d, not the %d this SSID was chosen for",
                 s_last_reason, EXPECTED_REASON);
        PROBE_NOTE("disconnect_reason", why);
    }

    // The handler stays registered through the teardown, so the events it posts (the `id=3` stop
    // among them) are on the console in arrival order too.
    rc("esp_wifi_disconnect", esp_wifi_disconnect());
    rc("esp_wifi_stop", esp_wifi_stop());
    // Let the event loop drain the stop event before the handler goes away with the driver.
    vTaskDelay(pdMS_TO_TICKS(300));
    rc("esp_wifi_deinit", esp_wifi_deinit());
    esp_event_handler_unregister(WIFI_EVENT, ESP_EVENT_ANY_ID, on_wifi_event);

    printf("WIFI|placeholder_ssid=%d|events=%d|sta_start_seen=%d|disconnects=%d|reason=%d"
           "|timed_out=%d\n",
           strcmp(CONFIG_PROBE_WIFI_CONN_SSID, PLACEHOLDER_SSID) == 0 ? 1 : 0, s_event_count,
           s_sta_start_seen, s_disconnect_count, s_last_reason, disconnected ? 0 : 1);

    // `nvs_flash_init` is reported but is not a pass criterion: nothing here uses NVS, so its
    // code is a fact about the part rather than a step that had to work.
    if (init_rc != ESP_OK || start_rc != ESP_OK || connect_rc != ESP_OK) {
        PROBE_FAIL("setup", "esp_wifi_init, esp_wifi_start or esp_wifi_connect did not return ESP_OK");
        ok = false;
    }
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
