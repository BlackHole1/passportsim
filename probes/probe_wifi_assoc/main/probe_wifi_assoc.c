// probe_wifi_assoc: the *successful* association half, on silicon.
// MIT. An ordinary ESP-IDF v5.5.3 app with no
// emulator-specific code.
//
// Its sibling `probe_wifi_conn` captured the association attempt that fails: an SSID that names
// no access point, `WIFI_REASON_NO_AP_FOUND`, no credential anywhere. That probe
// cannot be extended to the success case, because "carries no credential" is written into its own
// Kconfig and the failed-association test rests on it. This probe is the other half: it associates with a
// real access point and gets an address, which is the one thing no absent SSID can ever produce.
//
// What it drives, and nothing else:
//
//   nvs_flash_init, esp_netif_init, esp_event_loop_create_default, the default STA netif,
//   esp_wifi_init, esp_wifi_set_storage(RAM), esp_wifi_set_mode(STA), esp_wifi_set_config,
//   esp_wifi_start, esp_wifi_connect, wait for IP_EVENT_STA_GOT_IP,
//   then esp_wifi_disconnect, esp_wifi_stop, esp_wifi_deinit.
//
// **The credential.** CONFIG_PROBE_WIFI_ASSOC_SSID and CONFIG_PROBE_WIFI_ASSOC_PASSWORD are both
// **empty in the repository** and must stay empty (docs/secrets.md, and the long note in
// main/Kconfig.projbuild). With an empty SSID this probe prints a `NOTE|` and ends **before
// `esp_wifi_init`**, so the committed tree builds an app that reaches no radio and holds no
// secret; that is the build `xtask probes` and CI produce. A capture supplies real values through
// a defaults overlay kept outside the checkout, and they then exist only in that file and in the
// app image built from it, which the post-capture restore overwrites.
//
// **Measured, and stronger than that.** `CONFIG_PROBE_WIFI_ASSOC_SSID` is a macro, so with the
// committed empty default the guard folds at compile time and the linker drops everything behind
// it. The credential-free app is 152,864 bytes where `probe_wifi_conn`, which makes the same Wi-Fi
// calls for real, is 772,128 (`tests/fw/manifest.toml`). In the committed build the Wi-Fi stack is
// not merely unreached: it is **not linked in at all**.
//
// **Bounded.** At most CONFIG_PROBE_WIFI_ASSOC_WAIT_MS for the address, one connect attempt, no
// re-connect on disconnect, then teardown. A device run always ends.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   RC    one line per call, `RC|<function>|<esp_err_t as a number>`, in call order.
//         `esp_netif_create_default_wifi_sta` returns a handle, reported as 0 or -1 (ESP_FAIL).
//   EVT   one line per `WIFI_EVENT` **and** per `IP_EVENT`, printed from the handler so the
//         console order is the arrival order, and stamped with the microseconds since
//         `esp_wifi_connect` returned: `EVT|<base>|seq=<n>|id=<n>|us=<n>`. The disconnect event
//         adds `|reason=<n>`. This ordering and these timings are what the Wi-Fi model needs:
//         which of CONNECTED and GOT_IP arrives first, and how far apart.
//   WIFI  the summary: whether an address arrived, the event counts, and the two timings.
//   NOTE  no SSID was configured, the wait timed out, or a disconnect arrived instead.
//   FAIL  a step that had to work did not, so the capture is not the intended one.
//
// Secrets (docs/secrets.md): **this probe's own lines** never print the SSID, the
// password, the BSSID or the obtained IP address. The address appears only as `got_ip=1`, and the
// handler reads nothing out of `ip_event_got_ip_t` but the fact that it arrived. **ESP-IDF's own
// logs do, and the probe leaves them on (measured on the device, 2026-09-22):** the `wifi:` driver
// log prints `connected with <SSID>` and the BSSID and station MAC, and `esp_netif_handlers` prints
// the leased address, mask and gateway. They are left on because the driver's state lines
// (`init -> auth -> assoc -> run` with timestamps) are exactly the timing this capture is for, so
// a capture masks the SSID, MAC-shaped and IPv4-shaped strings when it is written, as the Wi-Fi
// evidence file does.
//
// Flash: the probe calls no flash write API and nothing it runs writes the device's NVS.
// `CONFIG_ESP_WIFI_NVS_ENABLED=n`, the PHY calibration store off (sdkconfig.defaults),
// `esp_wifi_set_storage(WIFI_STORAGE_RAM)`, and no `nvs_flash_erase` recovery. So the credential
// a capture build carries never reaches the part outside the app image, and a device run writes
// only the three segments the planner wrote. `partitions.csv` beside this file is the Passport's
// own layout, so cardid at 0x356000 is left where it is.
//
// **Why this is not `probe_wifi_http`.** That probe is the other half and runs against the
// emulator's *scripted* virtual LAN, where the lease and the body are fixed by the script. It
// cannot be the device capture: it has no `partitions.csv`, so the planner's identity guard
// refuses to flash it at all; it keeps ESP-IDF's `nvs_flash_erase` recovery, which on the
// Passport would erase the device's own NVS; and it prints the leased address, the netmask and
// the gateway, which is right for a scripted LAN and wrong for a person's network. This probe is
// the silicon side: what the chip's event loop actually does between `esp_wifi_connect` and an
// address, so the emulator's DHCP and association timing have a class A source to be
// built against. Neither probe replaces the other.
//
// Consumed by: the association timing rows of specs/hle/idf-5.5.3/wifi.toml.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "esp_err.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/event_groups.h"
#include "freertos/task.h"
#include "nvs_flash.h"
#include "sdkconfig.h"

#include "probe_line.h"

#define PROBE_NAME "probe_wifi_assoc"

#define BIT_GOT_IP BIT0
#define BIT_DISCONNECTED BIT1

static EventGroupHandle_t s_events;
static int64_t s_connect_us;
static int s_event_count;
static int s_wifi_events;
static int s_ip_events;
static int s_disconnect_count;
static int s_last_reason = -1;
static int64_t s_connected_us = -1;
static int64_t s_got_ip_us = -1;

/// Microseconds since `esp_wifi_connect` returned, or -1 before it has.
static int64_t since_connect(void)
{
    return s_connect_us > 0 ? esp_timer_get_time() - s_connect_us : -1;
}

/// Every `WIFI_EVENT` and every `IP_EVENT`, in arrival order. Printing here rather than in
/// `app_main` is deliberate: the console order is then the order the event loop delivered them
/// in, which is the fact this capture is for.
static void on_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    int seq = ++s_event_count;
    int64_t us = since_connect();

    if (base == WIFI_EVENT) {
        s_wifi_events++;
        if (id == WIFI_EVENT_STA_CONNECTED) {
            // The event also carries the SSID, the BSSID and the channel; the secrets policy keeps the
            // first two off the console, so nothing is read out of it here.
            s_connected_us = us;
        }
        if (id == WIFI_EVENT_STA_DISCONNECTED) {
            const wifi_event_sta_disconnected_t *event = data;
            s_last_reason = event != NULL ? (int)event->reason : -1;
            s_disconnect_count++;
            printf("EVT|WIFI_EVENT|seq=%d|id=%" PRId32 "|us=%" PRId64 "|reason=%d\n", seq, id, us,
                   s_last_reason);
            // The probe never re-connects: one attempt, one outcome, a run that ends. A
            // disconnect before the address is what a wrong credential looks like, and it has to
            // end the wait rather than hang it out to the timeout.
            xEventGroupSetBits(s_events, BIT_DISCONNECTED);
            return;
        }
        printf("EVT|WIFI_EVENT|seq=%d|id=%" PRId32 "|us=%" PRId64 "\n", seq, id, us);
        return;
    }

    if (base == IP_EVENT) {
        s_ip_events++;
        if (id == IP_EVENT_STA_GOT_IP) {
            // `ip_event_got_ip_t` carries the address, the netmask and the gateway. **None of
            // them is read.** That the event arrived is the whole fact this probe reports.
            s_got_ip_us = us;
            printf("EVT|IP_EVENT|seq=%d|id=%" PRId32 "|us=%" PRId64 "\n", seq, id, us);
            xEventGroupSetBits(s_events, BIT_GOT_IP);
            return;
        }
        printf("EVT|IP_EVENT|seq=%d|id=%" PRId32 "|us=%" PRId64 "\n", seq, id, us);
        return;
    }
}

static void rc(const char *what, esp_err_t code)
{
    printf("RC|%s|%d\n", what, (int)code);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);

    // The committed tree's own path. An empty SSID is not a misconfiguration to report as a
    // failure: it is what this probe is checked into the repository as, and reaching the radio
    // without one would be the defect. Nothing below this point runs.
    if (CONFIG_PROBE_WIFI_ASSOC_SSID[0] == '\0') {
        PROBE_NOTE("ssid", "no SSID configured, so the radio is not touched; see Kconfig.projbuild");
        printf("WIFI|configured=0|got_ip=0|wifi_events=0|ip_events=0|disconnects=0|reason=-1"
               "|connected_us=-1|got_ip_us=-1|timed_out=0\n");
        PROBE_END(PROBE_NAME, "ok");
        return;
    }

    bool ok = true;

    // Deliberately **without** the `nvs_flash_erase` recovery every other probe has, for the same
    // reason as `probe_wifi_conn`: with the Wi-Fi NVS store and the PHY calibration store off,
    // nothing here needs NVS, so an unmountable partition is a fact to print rather than a reason
    // to erase the device's own data.
    rc("nvs_flash_init", nvs_flash_init());

    s_events = xEventGroupCreate();
    rc("esp_netif_init", esp_netif_init());
    rc("esp_event_loop_create_default", esp_event_loop_create_default());
    esp_netif_t *sta = esp_netif_create_default_wifi_sta();
    rc("esp_netif_create_default_wifi_sta", sta != NULL ? ESP_OK : ESP_FAIL);
    rc("esp_event_handler_register_wifi",
       esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, on_event, NULL));
    rc("esp_event_handler_register_ip",
       esp_event_handler_register(IP_EVENT, ESP_EVENT_ANY_ID, on_event, NULL));

    wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
    esp_err_t init_rc = esp_wifi_init(&init);
    rc("esp_wifi_init", init_rc);
    // RAM storage, so the credential a capture build carries never reaches the device's NVS.
    rc("esp_wifi_set_storage", esp_wifi_set_storage(WIFI_STORAGE_RAM));
    rc("esp_wifi_set_mode", esp_wifi_set_mode(WIFI_MODE_STA));

    wifi_config_t config = {0};
    snprintf((char *)config.sta.ssid, sizeof(config.sta.ssid), "%s", CONFIG_PROBE_WIFI_ASSOC_SSID);
    snprintf((char *)config.sta.password, sizeof(config.sta.password), "%s",
             CONFIG_PROBE_WIFI_ASSOC_PASSWORD);
    // An empty password means an open access point and an empty threshold would reject nothing;
    // with one, WPA2-PSK is the floor. Neither branch prints which it took, because that is a
    // fact about the user's network.
    config.sta.threshold.authmode =
        CONFIG_PROBE_WIFI_ASSOC_PASSWORD[0] == '\0' ? WIFI_AUTH_OPEN : WIFI_AUTH_WPA2_PSK;
    rc("esp_wifi_set_config", esp_wifi_set_config(WIFI_IF_STA, &config));

    esp_err_t start_rc = esp_wifi_start();
    rc("esp_wifi_start", start_rc);
    esp_err_t connect_rc = esp_wifi_connect();
    // Stamped after the call returns, so every `us=` on the console is measured from the same
    // instant and the first event's stamp is not negative.
    s_connect_us = esp_timer_get_time();
    rc("esp_wifi_connect", connect_rc);

    // Either outcome ends the wait: the address, or the disconnect that a refused credential
    // gives instead.
    EventBits_t bits =
        xEventGroupWaitBits(s_events, BIT_GOT_IP | BIT_DISCONNECTED, pdFALSE, pdFALSE,
                            pdMS_TO_TICKS(CONFIG_PROBE_WIFI_ASSOC_WAIT_MS));
    bool got_ip = (bits & BIT_GOT_IP) != 0;
    bool timed_out = (bits & (BIT_GOT_IP | BIT_DISCONNECTED)) == 0;

    if (timed_out) {
        PROBE_NOTE("address_wait", "neither an address nor a disconnect within the configured wait");
    } else if (!got_ip) {
        char why[96];
        snprintf(why, sizeof(why), "disconnected with reason %d before any address arrived",
                 s_last_reason);
        PROBE_NOTE("address", why);
    }

    // The handler stays registered through the teardown, so the events it posts are on the
    // console in arrival order too.
    // With CONFIG_PROBE_WIFI_ASSOC_STOP_WHILE_ASSOCIATED the station is stopped while still
    // associated, so the console shows what the stop itself posts.
#if !CONFIG_PROBE_WIFI_ASSOC_STOP_WHILE_ASSOCIATED
    rc("esp_wifi_disconnect", esp_wifi_disconnect());
#endif
    rc("esp_wifi_stop", esp_wifi_stop());
    // Let the event loop drain the stop event before the handler goes away with the driver.
    vTaskDelay(pdMS_TO_TICKS(300));
    rc("esp_wifi_deinit", esp_wifi_deinit());
    esp_event_handler_unregister(WIFI_EVENT, ESP_EVENT_ANY_ID, on_event);
    esp_event_handler_unregister(IP_EVENT, ESP_EVENT_ANY_ID, on_event);

    printf("WIFI|configured=1|got_ip=%d|wifi_events=%d|ip_events=%d|disconnects=%d|reason=%d"
           "|connected_us=%" PRId64 "|got_ip_us=%" PRId64 "|timed_out=%d\n",
           got_ip ? 1 : 0, s_wifi_events, s_ip_events, s_disconnect_count, s_last_reason,
           s_connected_us, s_got_ip_us, timed_out ? 1 : 0);

    // `nvs_flash_init` is reported but is not a pass criterion: nothing here uses NVS.
    if (init_rc != ESP_OK || start_rc != ESP_OK || connect_rc != ESP_OK) {
        PROBE_FAIL("setup",
                   "esp_wifi_init, esp_wifi_start or esp_wifi_connect did not return ESP_OK");
        ok = false;
    }
    if (!got_ip) {
        PROBE_FAIL("got_ip", "the station never obtained an address, so there is no capture");
        ok = false;
    }
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
