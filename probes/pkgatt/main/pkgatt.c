// pkgatt: the Passport Keys GATT probe, rebuilt from repository sources.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific
// code.
//
// Provenance: ported from our earlier prototype firmware. The BLE link
// itself is Passport Keys code: `pk_ble.c`,
// `pk_ble.h`, `pk_protocol.c` and `pk_protocol.h` are byte-identical to the Passport Keys sources
// the `pk` corpus image is built from, MIT licensed, copyright FoloToy (LICENSE.passport-keys).
//
// The probe starts the Passport Keys GATT peripheral (service 12D4FA08-7418-48FA-A95A-B43A2E669E55,
// notify characteristic ...FA09, write characteristic ...FA0A, per the pk_ble.c header) and waits
// up to 180 s for a central to subscribe and send two command lines. It answers `hello`, `ping`,
// `labels` and `config` with the Passport Keys protocol frames, and sends one `ok` button frame
// when a central subscribes.
//
// Changes from the prototype's file, and nothing else:
//   - a `PROBE|name=pkgatt|...` header first and a `DONE|name=pkgatt|status=..` footer after the
//     original `PROBE DONE` line; the status is `ok` when the link started, a central subscribed
//     and at least two lines arrived;
//   - an RX line renders what the central wrote with every byte outside 0x20 to 0x7E, and `|`,
//     replaced by `.` (probes/README.md: a probe never writes host bytes into the console). A
//     well-formed Passport Keys command is plain JSON, so its RX line is the prototype's.
//
// Prints: HEAP|, RC|, EVT|LINK, RX|, TX| (the prototype's formats), then the header and footer.
//
// Consumed by: the GATT re-proof against the in-emulator BLE central (the prototype's run on
// silicon is the reference).

#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include "esp_heap_caps.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "pk_ble.h"
#include "pk_protocol.h"

#include "probe_line.h"

#define PROBE_NAME "pkgatt"

#define BOOT_ID 0x1234abcdu

static volatile int s_lines;
static volatile int s_subscribed;
static volatile int s_tx;

static long long now_ms(void) { return esp_timer_get_time() / 1000; }

static void send_frame(const char *buf, size_t n)
{
    if (n == 0) return;
    pk_ble_send(buf, n);
    s_tx++;
    printf("TX|%d|%.*s", s_tx, (int)n, buf);
}

// Copies at most `cap - 1` bytes of `line`, replacing anything a probe line may not carry.
static void render(char *out, size_t cap, const char *line, size_t len)
{
    size_t n = len < cap - 1 ? len : cap - 1;
    for (size_t i = 0; i < n; i++) {
        unsigned char c = (unsigned char)line[i];
        out[i] = (c >= 0x20 && c <= 0x7e && c != '|') ? (char)c : '.';
    }
    out[n] = '\0';
}

static void on_line(const char *line, size_t len)
{
    s_lines++;
    char shown[PK_MSG_MAX];
    render(shown, sizeof shown, line, len);
    printf("RX|%d|%s|t_ms=%lld\n", s_lines, shown, now_ms());
    pk_cmd_t cmd;
    char buf[PK_MSG_MAX];
    if (!pk_parse_command(line, len, &cmd)) {
        printf("RC|parse|0\n");
        return;
    }
    switch (cmd.type) {
    case PK_CMD_HELLO: send_frame(buf, pk_format_hello(buf, sizeof buf, "g2-spike", BOOT_ID)); break;
    case PK_CMD_PING: send_frame(buf, pk_format_pong(buf, sizeof buf)); break;
    case PK_CMD_LABELS: send_frame(buf, pk_format_ack(buf, sizeof buf, "labels")); break;
    case PK_CMD_CONFIG: send_frame(buf, pk_format_ack(buf, sizeof buf, "config")); break;
    default: break;
    }
}

static void on_link(bool subscribed)
{
    printf("EVT|LINK|subscribed=%d|t_ms=%lld\n", subscribed ? 1 : 0, now_ms());
    if (subscribed) {
        s_subscribed++;
        char buf[PK_MSG_MAX];
        send_frame(buf, pk_format_button(buf, sizeof buf, PK_KEY_OK, 1, BOOT_ID));
    }
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    printf("HEAP|boot.app_main|free=%u|t_ms=%lld\n", (unsigned)heap_caps_get_free_size(MALLOC_CAP_8BIT), now_ms());
    esp_err_t rc = pk_ble_start(on_line, on_link);
    printf("RC|pk_ble_start|%d\n", rc);
    for (int i = 0; i < 360 && s_lines < 2; i++) {
        vTaskDelay(pdMS_TO_TICKS(500));
    }
    vTaskDelay(pdMS_TO_TICKS(1000));
    printf("RC|gatt|subscribed=%d|lines=%d|tx=%d|t_ms=%lld\n", s_subscribed, s_lines, s_tx, now_ms());
    printf("HEAP|end|free=%u|min=%u\n", (unsigned)heap_caps_get_free_size(MALLOC_CAP_8BIT),
           (unsigned)heap_caps_get_minimum_free_size(MALLOC_CAP_8BIT));
    printf("PROBE DONE\n");
    bool ok = rc == ESP_OK && s_subscribed > 0 && s_lines >= 2;
    if (!ok) {
        PROBE_FAIL("gatt", "no central subscribed and wrote two lines within 180 s");
    }
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
