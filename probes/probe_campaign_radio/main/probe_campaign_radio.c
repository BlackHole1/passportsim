// probe_campaign_radio: the BLE controller facts of the silicon evidence campaign, step 1
// (specs/notes/silicon-campaign.md). MIT.
// An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// The controller is brought up and taken down once, through the same calls every BLE corpus image
// makes, with no host stack on top: HCI goes over VHCI directly, and only commands that make the
// controller transmit nothing are sent.
//
//   TIME|ble_init, TIME|ble_enable, TIME|ble_disable, TIME|ble_deinit   esp_timer microseconds
//            around each controller call, with its return code. Rows timing-profiles.ble_init_ps
//            (class C: "a capture with a line or a GPIO edge at esp_bt_controller_init's entry
//            would settle it") and timing-profiles.ble_enable_ps (class B, a check)
//   HEAP|<stage>   heap_caps_get_free_size(MALLOC_CAP_INTERNAL) and the largest free block before
//            init, after init, after enable, after disable and after deinit. Row
//            hle.ble.heap_ledger (specs/hle/idf-5.5.3/ble.toml: "What would settle it: a
//            radio_heapprobe capture of heap_caps_get_free_size(MALLOC_CAP_INTERNAL) around init,
//            enable and deinit on silicon"; running it is a deliberate device decision, since
//            it enables the radio)
//   ISR|<stage>   the interrupt-matrix map of the seven BT sources (BT_MAC 4 to RWBLE_NMI 10) after
//            init and after enable: which CPU line each is routed to, 0 for none. Row
//            hle.ble.isr_source (ble.toml: "which source BLE uses is UNVERIFIED and read from the
//            profile")
//   HCI|reset, HCI|read_local_version   the time from esp_vhci_host_send_packet to the
//            controller's Command Complete reaching the VHCI callback, and the status (and, for
//            the version, the HCI and LMP versions and the company id the controller reports, the
//            class A facts of ble.toml, which make the line checkable). Row hle.ble.reply_us
//   LOG|ble_init_<n>, LOG|phy_init_<n>   the controller's own BLE_INIT and phy_init log lines,
//            in order, as probe lines: the text after the tag, with the timestamp dropped and
//            the address of the `Bluetooth MAC:` line withheld. Row hle.ble.log_lines (ble.toml:
//            the sdkconfig-dependent lines were checked against pk's boot log only, and are
//            UNVERIFIED for pkgatt's shape, whose BT settings this probe copies)
//
// Safety, checked for every step (silicon campaign step 1):
//   - no radio TX beyond what the corpus firmware already does: the controller is initialised and
//     enabled as every BLE corpus image does at boot (pk, official, pkgatt), and the only HCI
//     commands are HCI_Reset and HCI_Read_Local_Version_Information, neither of which transmits;
//     no advertising, scanning or connection command is sent;
//   - no flash write or erase and no NVS: PHY calibration data storage is off
//     (sdkconfig.defaults), the probe never calls nvs_flash_init, and the partition table is the
//     device's own (partitions.csv), so nothing falls in or reads cardid [0x356000, 0x35A000);
//   - no eFuse write; the controller reads the BT MAC from eFuse as every BLE image does, and the
//     log hook below prints the MAC line with the address withheld, so no MAC reaches the console;
//   - no sleep;
//   - bounded: every wait on the controller has a timeout; the run takes about a second after
//     boot and ends with a DONE line.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <stdarg.h>

#include "esp_bt.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "soc/interrupt_core0_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_campaign_radio"

static bool s_ok = true;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

// The log hook: ESP-IDF's log v1 hands every line to vprintf whole ("I (<ms>) <tag>: <text>\n").
// A line of the BLE_INIT or phy_init tag is printed as a probe line instead; every other line
// passes through unchanged. The emulator's HLE prints these lines through the same esp_log call
// (specs/hle/idf-5.5.3/log-lines.toml), so both sides go through this hook.
static vprintf_like_t s_vprintf;
static char s_log_buf[256];
static int s_log_count[2];

static int log_hook(const char *fmt, va_list args)
{
    static const char *const TAGS[2] = {"BLE_INIT", "phy_init"};
    static const char *const LABELS[2] = {"ble_init", "phy_init"};
    va_list copy;
    va_copy(copy, args);
    int n = vsnprintf(s_log_buf, sizeof(s_log_buf), fmt, copy);
    va_end(copy);
    if (n <= 0) {
        return s_vprintf(fmt, args);
    }
    for (int t = 0; t < 2; t++) {
        char key[16];
        snprintf(key, sizeof(key), ") %s: ", TAGS[t]);
        char *at = strstr(s_log_buf, key);
        if (at == NULL) {
            continue;
        }
        char *text = at + strlen(key);
        // Printable ASCII only, no separator, no line end.
        for (char *c = text; *c != '\0'; c++) {
            if (*c == '\n' || *c == '\r') {
                *c = '\0';
                break;
            }
            if (*c == '|' || *c < 0x20 || *c > 0x7e) {
                *c = '/';
            }
        }
        char *mac = strstr(text, "MAC:");
        if (mac != NULL) {
            strcpy(mac, "MAC: (withheld)");
        }
        s_log_count[t]++;
        printf("LOG|%s_%d|row=hle.ble.log_lines|level=%c|text=%s\n", LABELS[t], s_log_count[t],
               s_log_buf[0], text);
        return n;
    }
    return s_vprintf(fmt, args);
}

static void heap_line(const char *stage)
{
    printf("HEAP|%s|row=hle.ble.heap_ledger|free=%u|largest=%u\n", stage,
           (unsigned)heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
           (unsigned)heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL));
}

static void isr_line(const char *stage)
{
    printf("ISR|%s|row=hle.ble.isr_source|bt_mac=%" PRIu32 "|bt_bb=%" PRIu32 "|bt_bb_nmi=%" PRIu32
           "|rwbt=%" PRIu32 "|rwble=%" PRIu32 "|rwbt_nmi=%" PRIu32 "|rwble_nmi=%" PRIu32 "\n",
           stage, REG_READ(INTERRUPT_CORE0_BT_MAC_INT_MAP_REG) & 0x1F,
           REG_READ(INTERRUPT_CORE0_BT_BB_INT_MAP_REG) & 0x1F,
           REG_READ(INTERRUPT_CORE0_BT_BB_NMI_MAP_REG) & 0x1F,
           REG_READ(INTERRUPT_CORE0_RWBT_IRQ_MAP_REG) & 0x1F,
           REG_READ(INTERRUPT_CORE0_RWBLE_IRQ_MAP_REG) & 0x1F,
           REG_READ(INTERRUPT_CORE0_RWBT_NMI_MAP_REG) & 0x1F,
           REG_READ(INTERRUPT_CORE0_RWBLE_NMI_MAP_REG) & 0x1F);
}

// ---------------------------------------------------------------------------------------------
// VHCI: one command at a time, the Command Complete handed to the waiting task.
// ---------------------------------------------------------------------------------------------

static SemaphoreHandle_t s_event;
static volatile int64_t s_event_us;
static uint8_t s_event_buf[64];
static volatile uint16_t s_event_len;

static void vhci_send_available(void)
{
}

static int vhci_recv(uint8_t *data, uint16_t len)
{
    // Only the first event after a command is kept: a Command Complete (0x04 0x0E ...).
    if (s_event_len == 0) {
        s_event_us = esp_timer_get_time();
        uint16_t n = len < sizeof(s_event_buf) ? len : sizeof(s_event_buf);
        memcpy(s_event_buf, data, n);
        s_event_len = n;
        xSemaphoreGive(s_event);
    }
    return 0;
}

static const esp_vhci_host_callback_t VHCI_CB = {
    .notify_host_send_available = vhci_send_available,
    .notify_host_recv = vhci_recv,
};

// Sends one HCI command packet and waits (at most 500 ms) for the first event; returns the
// microseconds from the send to the event, or -1.
static int64_t hci_command(const uint8_t *packet, uint16_t len)
{
    for (int i = 0; i < 50 && !esp_vhci_host_check_send_available(); i++) {
        vTaskDelay(pdMS_TO_TICKS(10));
    }
    if (!esp_vhci_host_check_send_available()) {
        return -1;
    }
    s_event_len = 0;
    uint8_t copy[16];
    memcpy(copy, packet, len);
    int64_t t0 = esp_timer_get_time();
    esp_vhci_host_send_packet(copy, len);
    if (xSemaphoreTake(s_event, pdMS_TO_TICKS(500)) != pdTRUE) {
        return -1;
    }
    return s_event_us - t0;
}

static void hci_steps(void)
{
    s_event = xSemaphoreCreateBinary();
    if (s_event == NULL || esp_vhci_host_register_callback(&VHCI_CB) != ESP_OK) {
        fail("vhci", "no semaphore or the VHCI callback was refused");
        return;
    }
    // H4 command packets: type 0x01, opcode little-endian, parameter length 0.
    static const uint8_t reset[] = {0x01, 0x03, 0x0C, 0x00};
    static const uint8_t version[] = {0x01, 0x01, 0x10, 0x00};

    int64_t us = hci_command(reset, sizeof(reset));
    // Command Complete: 04 0E len ncmd opcode_lo opcode_hi status ...
    int status = s_event_len >= 7 && s_event_buf[0] == 0x04 && s_event_buf[1] == 0x0E
                     ? s_event_buf[6]
                     : -1;
    printf("HCI|reset|row=hle.ble.reply_us|us=%" PRId64 "|event=0x%02x|status=%d\n", us,
           s_event_len >= 2 ? s_event_buf[1] : 0, status);
    if (us < 0 || status != 0) {
        fail("hci_reset", "no successful Command Complete for HCI_Reset");
    }

    us = hci_command(version, sizeof(version));
    bool complete = s_event_len >= 15 && s_event_buf[0] == 0x04 && s_event_buf[1] == 0x0E;
    // Return parameters from byte 6: status, HCI_Version, HCI_Subversion (2), LMP_Version,
    // Company_Identifier (2), LMP_Subversion (2).
    printf("HCI|read_local_version|row=hle.ble.reply_us|us=%" PRId64 "|status=%d|hci_version=%d"
           "|lmp_version=%d|company=0x%04x\n",
           us, complete ? s_event_buf[6] : -1, complete ? s_event_buf[7] : -1,
           complete ? s_event_buf[10] : -1,
           complete ? (unsigned)(s_event_buf[11] | (s_event_buf[12] << 8)) : 0u);
    if (us < 0 || !complete) {
        fail("hci_version", "no Command Complete for HCI_Read_Local_Version_Information");
    }
}

static void timed_call(const char *name, const char *row, esp_err_t (*call)(void *), void *arg)
{
    int64_t t0 = esp_timer_get_time();
    esp_err_t rc = call(arg);
    int64_t us = esp_timer_get_time() - t0;
    printf("TIME|%s|row=%s|us=%" PRId64 "|rc=%d\n", name, row, us, rc);
    if (rc != ESP_OK) {
        fail(name, "the controller call failed");
    }
}

static esp_err_t call_init(void *arg)
{
    return esp_bt_controller_init((esp_bt_controller_config_t *)arg);
}

static esp_err_t call_enable(void *arg)
{
    (void)arg;
    return esp_bt_controller_enable(ESP_BT_MODE_BLE);
}

static esp_err_t call_disable(void *arg)
{
    (void)arg;
    return esp_bt_controller_disable();
}

static esp_err_t call_deinit(void *arg)
{
    (void)arg;
    return esp_bt_controller_deinit();
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    // The controller's log lines become probe lines, with the BT MAC withheld.
    s_vprintf = esp_log_set_vprintf(log_hook);
    esp_bt_controller_config_t cfg = BT_CONTROLLER_INIT_CONFIG_DEFAULT();
    heap_line("before_init");
    isr_line("before_init");
    timed_call("ble_init", "timing-profiles.ble_init_ps", call_init, &cfg);
    heap_line("after_init");
    isr_line("after_init");
    timed_call("ble_enable", "timing-profiles.ble_enable_ps", call_enable, NULL);
    heap_line("after_enable");
    isr_line("after_enable");
    if (s_ok) {
        hci_steps();
    }
    timed_call("ble_disable", "timing-profiles.ble_enable_ps", call_disable, NULL);
    heap_line("after_disable");
    timed_call("ble_deinit", "timing-profiles.ble_init_ps", call_deinit, NULL);
    heap_line("after_deinit");
    esp_log_set_vprintf(s_vprintf);
    PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
}
