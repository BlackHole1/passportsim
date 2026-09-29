// usj_echo: the USB Serial/JTAG endpoint probe.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// The USJ is the device's only console and the port `idf.py` and `esptool` talk to.
// This probe runs it in *driver* mode, not console mode (design-facts agent-8.1):
// the IDF USB Serial/JTAG driver is installed and host bytes are read through it.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   USJ     driver installation, and every change of the host connection state with the time it
//           happened, which is the line-state log
//   BURST   the time a fixed write burst took. The expectation is that a detached host would
//           cost nothing, an idle attached host about one 50 ms stall per burst, and an open
//           host to take every byte
//   ECHO    bytes the host sent, as hex and as a sanitized rendering (see below)
//   IDLE    a heartbeat, so a capture taken with no host writing is still a sequence of lines
//
// **Host bytes are never written to the console.** The console is the same stream the probe-line
// parser grades, so echoing host input into it would let any host forge a `DONE|status=ok` line
// or split a probe line in two. What the host sent is reported instead, as hex, on the ECHO
// line, together with how many bytes the driver read; the ECHO line is the loopback report and
// carries the bytes safely. `sanitize` keeps the printable rendering of those bytes free of `|`
// and of anything outside 0x20 to 0x7E, so even the rendering cannot pose as a probe line.
//
// The console deliberately stays on the default (non-driver) path while the driver is installed.
// That two-writer arrangement is the configuration this probe exists to pin: it is driver mode
// alongside a console on the same port, which is what the Passport Keys firmware does
// (design-facts agent-8.1). `usb_serial_jtag_vfs_use_driver` would move the console into
// the driver's interrupt-driven TX ring and pin a different configuration.

#include <inttypes.h>
#include <string.h>

#include "driver/usb_serial_jtag.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#include "probe_line.h"

#define PROBE_NAME "usj_echo"

#define RX_BUFFER 1024
#define TX_BUFFER 1024
// Read chunk. The USJ endpoint is 64 bytes, so this is one packet.
#define CHUNK 64
// Bytes of a received chunk rendered as hex on the ECHO line.
#define ECHO_HEX_BYTES 16
// Writes per burst and bytes per write in the V10 timing check.
#define BURST_WRITES 8
#define BURST_BYTES 64
// Heartbeat period.
#define IDLE_MS 1000
// How long the probe runs before it stops, in heartbeats. A probe capture is a bounded file.
#define IDLE_LIMIT 30

// Byte an unprintable or separator byte is rendered as.
#define ECHO_SUBSTITUTE '.'

// Bytes of a received chunk rendered as text on the ECHO line.
#define ECHO_TEXT_BYTES 16

// Renders host bytes as a probe-line value: every byte a probe line may not carry becomes `.`.
//
// The grammar of probes/common/probe_line.h allows 0x20 to 0x7E without `|`; anything else, a
// newline above all, would let host input pose as a probe line on the console the parser reads.
static void sanitize(const uint8_t *bytes, int count, char *out)
{
    for (int i = 0; i < count; i++) {
        uint8_t byte = bytes[i];
        out[i] = (byte < 0x20 || byte > 0x7e || byte == '|') ? ECHO_SUBSTITUTE : (char)byte;
    }
    out[count] = '\0';
}

static void report_burst(int index)
{
    static const char pattern[BURST_BYTES + 1] =
        "usj_echo burst pattern 0123456789abcdef 0123456789abcdef 0123456";
    int64_t start = esp_timer_get_time();
    int written = 0;
    for (int i = 0; i < BURST_WRITES; i++) {
        written += usb_serial_jtag_write_bytes(pattern, BURST_BYTES, pdMS_TO_TICKS(100));
    }
    int64_t elapsed = esp_timer_get_time() - start;
    printf("BURST|index=%d|writes=%d|bytes=%d|written=%d|us=%" PRId64 "\n", index, BURST_WRITES,
           BURST_WRITES * BURST_BYTES, written, elapsed);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);

    usb_serial_jtag_driver_config_t config = USB_SERIAL_JTAG_DRIVER_CONFIG_DEFAULT();
    config.rx_buffer_size = RX_BUFFER;
    config.tx_buffer_size = TX_BUFFER;
    if (usb_serial_jtag_driver_install(&config) != ESP_OK) {
        PROBE_FAIL("install", "usb_serial_jtag_driver_install failed");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    printf("USJ|event=installed|rx_buf=%d|tx_buf=%d|console=vfs_no_driver\n", RX_BUFFER,
           TX_BUFFER);

    bool connected = usb_serial_jtag_is_connected();
    printf("USJ|event=line_state|connected=%d|t_ms=%" PRId64 "\n", connected ? 1 : 0,
           esp_timer_get_time() / 1000);
    report_burst(0);

    static uint8_t buffer[CHUNK];
    uint32_t total = 0;
    int idle = 0;
    int64_t next_idle = esp_timer_get_time() + IDLE_MS * 1000;

    while (idle < IDLE_LIMIT) {
        int read = usb_serial_jtag_read_bytes(buffer, sizeof(buffer), pdMS_TO_TICKS(100));
        if (read > 0) {
            total += (uint32_t)read;
            // The ECHO line is the loopback: it reports what the host sent, as hex and as a
            // sanitized rendering. The bytes themselves never reach the console.
            char hex[ECHO_HEX_BYTES * 2 + 1];
            int shown = read < ECHO_HEX_BYTES ? read : ECHO_HEX_BYTES;
            for (int i = 0; i < shown; i++) {
                static const char digits[] = "0123456789abcdef";
                hex[i * 2] = digits[buffer[i] >> 4];
                hex[i * 2 + 1] = digits[buffer[i] & 0xf];
            }
            hex[shown * 2] = '\0';
            char text[ECHO_TEXT_BYTES + 1];
            sanitize(buffer, read < ECHO_TEXT_BYTES ? read : ECHO_TEXT_BYTES, text);
            printf("ECHO|bytes=%d|total=%" PRIu32 "|hex=%s|text=%s\n", read, total, hex, text);
        }

        bool now = usb_serial_jtag_is_connected();
        if (now != connected) {
            connected = now;
            printf("USJ|event=line_state|connected=%d|t_ms=%" PRId64 "\n", connected ? 1 : 0,
                   esp_timer_get_time() / 1000);
        }

        if (esp_timer_get_time() >= next_idle) {
            next_idle += IDLE_MS * 1000;
            idle++;
            printf("IDLE|index=%d|connected=%d|total=%" PRIu32 "|t_ms=%" PRId64 "\n", idle,
                   connected ? 1 : 0, total, esp_timer_get_time() / 1000);
            if (idle % 10 == 0) {
                report_burst(idle / 10);
            }
        }
    }

    printf("ECHO|final=1|total=%" PRIu32 "\n", total);
    PROBE_END(PROBE_NAME, "ok");
}
