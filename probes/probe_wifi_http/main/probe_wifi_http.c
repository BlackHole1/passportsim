// probe_wifi_http: DHCP address and an HTTP GET over Wi-Fi.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Joins the access point named by CONFIG_PROBE_WIFI_SSID, waits for a DHCP lease, then sends one
// HTTP GET to CONFIG_PROBE_HTTP_HOST (the DHCP gateway when empty), CONFIG_PROBE_HTTP_PORT and
// CONFIG_PROBE_HTTP_PATH, hashing the body as it arrives. Then stops and deinitializes Wi-Fi.
//
// Secrets (docs/secrets.md): the SSID and password defaults in main/Kconfig.projbuild
// are placeholders for the scripted open access point of the virtual LAN, not a real network. The
// probe never prints the password, and prints the SSID only as whether it is still the placeholder.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   WIFI  init, start and connect return codes; whether the SSID is the placeholder; whether a
//         lease arrived, and the last disconnect reason if not
//   IP    the leased address, netmask and gateway
//   HTTP  the URL path, the status code, the body length and the SHA-256 of the body
//   RC    return codes of stop and deinit
//
// Deterministic against a scripted LAN: the lease, the status and the body hash are fixed by the
// script. No timestamps are printed.
//
// Consumed by: the virtual LAN tests of tests/milestones/m12.rs.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "esp_event.h"
#include "esp_http_client.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/event_groups.h"
#include "freertos/task.h"
#include "mbedtls/sha256.h"
#include "nvs_flash.h"
#include "sdkconfig.h"

#include "probe_line.h"

#define PROBE_NAME "probe_wifi_http"

#define PLACEHOLDER_SSID "passport-emu-virtual-ap"

#define BIT_GOT_IP BIT0
#define BIT_GAVE_UP BIT1
#define CONNECT_ATTEMPTS 5
#define LEASE_WAIT_MS 30000

static EventGroupHandle_t s_events;
static int s_attempts;
static int s_last_reason = -1;
static esp_netif_ip_info_t s_ip;

static void on_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        wifi_event_sta_disconnected_t *event = data;
        s_last_reason = event->reason;
        if (++s_attempts < CONNECT_ATTEMPTS) {
            esp_wifi_connect();
        } else {
            xEventGroupSetBits(s_events, BIT_GAVE_UP);
        }
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        ip_event_got_ip_t *event = data;
        s_ip = event->ip_info;
        xEventGroupSetBits(s_events, BIT_GOT_IP);
    }
}

typedef struct {
    mbedtls_sha256_context sha;
    size_t length;
} body_t;

static esp_err_t on_http(esp_http_client_event_t *event)
{
    body_t *body = event->user_data;
    if (event->event_id == HTTP_EVENT_ON_DATA && event->data_len > 0) {
        mbedtls_sha256_update(&body->sha, event->data, (size_t)event->data_len);
        body->length += (size_t)event->data_len;
    }
    return ESP_OK;
}

static bool http_get(void)
{
    char host[64];
    if (CONFIG_PROBE_HTTP_HOST[0] != '\0') {
        snprintf(host, sizeof(host), "%s", CONFIG_PROBE_HTTP_HOST);
    } else {
        snprintf(host, sizeof(host), IPSTR, IP2STR(&s_ip.gw));
    }
    static body_t body;
    memset(&body, 0, sizeof(body));
    mbedtls_sha256_init(&body.sha);
    mbedtls_sha256_starts(&body.sha, 0);

    esp_http_client_config_t config = {
        .host = host,
        .port = CONFIG_PROBE_HTTP_PORT,
        .path = CONFIG_PROBE_HTTP_PATH,
        .transport_type = HTTP_TRANSPORT_OVER_TCP,
        .event_handler = on_http,
        .user_data = &body,
        .timeout_ms = 10000,
    };
    esp_http_client_handle_t client = esp_http_client_init(&config);
    esp_err_t rc = client != NULL ? esp_http_client_perform(client) : ESP_FAIL;
    int status = client != NULL ? esp_http_client_get_status_code(client) : -1;
    if (client != NULL) {
        esp_http_client_cleanup(client);
    }

    uint8_t digest[32];
    mbedtls_sha256_finish(&body.sha, digest);
    mbedtls_sha256_free(&body.sha);
    char hex[65];
    for (int i = 0; i < 32; i++) {
        snprintf(&hex[i * 2], 3, "%02x", digest[i]);
    }
    printf("HTTP|path=%s|rc=%d|status=%d|length=%u|sha256=%s\n", CONFIG_PROBE_HTTP_PATH, rc, status,
           (unsigned)body.length, hex);
    if (rc != ESP_OK || status != 200) {
        PROBE_FAIL("http_get", "the GET did not complete with status 200");
        return false;
    }
    return true;
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    bool ok = true;
    esp_err_t rc = nvs_flash_init();
    if (rc == ESP_ERR_NVS_NO_FREE_PAGES || rc == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        nvs_flash_erase();
        rc = nvs_flash_init();
    }
    s_events = xEventGroupCreate();
    esp_netif_init();
    esp_event_loop_create_default();
    esp_netif_create_default_wifi_sta();
    esp_event_handler_register(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, on_event, NULL);
    esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_event, NULL);

    wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
    esp_err_t init_rc = esp_wifi_init(&init);
    wifi_config_t config = {0};
    snprintf((char *)config.sta.ssid, sizeof(config.sta.ssid), "%s", CONFIG_PROBE_WIFI_SSID);
    snprintf((char *)config.sta.password, sizeof(config.sta.password), "%s",
             CONFIG_PROBE_WIFI_PASSWORD);
    config.sta.threshold.authmode =
        CONFIG_PROBE_WIFI_PASSWORD[0] == '\0' ? WIFI_AUTH_OPEN : WIFI_AUTH_WPA2_PSK;
    esp_wifi_set_storage(WIFI_STORAGE_RAM);
    esp_wifi_set_mode(WIFI_MODE_STA);
    esp_wifi_set_config(WIFI_IF_STA, &config);
    esp_err_t start_rc = esp_wifi_start();
    esp_err_t connect_rc = esp_wifi_connect();

    EventBits_t bits = xEventGroupWaitBits(s_events, BIT_GOT_IP | BIT_GAVE_UP, pdFALSE, pdFALSE,
                                           pdMS_TO_TICKS(LEASE_WAIT_MS));
    bool leased = (bits & BIT_GOT_IP) != 0;
    printf("WIFI|nvs=%d|init=%d|start=%d|connect=%d|placeholder_ssid=%d|leased=%d|attempts=%d"
           "|last_reason=%d\n",
           rc, init_rc, start_rc, connect_rc,
           strcmp(CONFIG_PROBE_WIFI_SSID, PLACEHOLDER_SSID) == 0 ? 1 : 0, leased ? 1 : 0,
           s_attempts + 1, s_last_reason);
    if (!leased) {
        PROBE_FAIL("dhcp", "no DHCP lease within 30 s");
        ok = false;
    } else {
        printf("IP|ip=" IPSTR "|netmask=" IPSTR "|gw=" IPSTR "\n", IP2STR(&s_ip.ip),
               IP2STR(&s_ip.netmask), IP2STR(&s_ip.gw));
        ok &= http_get();
    }

    esp_event_handler_unregister(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, on_event);
    esp_err_t stop_rc = esp_wifi_stop();
    esp_err_t deinit_rc = esp_wifi_deinit();
    printf("RC|stop=%d|deinit=%d\n", stop_rc, deinit_rc);
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
