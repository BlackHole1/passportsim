// probe_timing: esp_timer deltas around the operations the `device` timing profile is fitted on
// (specs/timing-profiles.toml). MIT. An ordinary ESP-IDF
// v5.5.3 app with no emulator-specific code.
//
// Five operations, each timed with esp_timer and each followed by a fact that proves the work was
// really done:
//
//   flash_read_64k   esp_partition_read of 64 KB from the start of the running app partition;
//                    CRC-32 of what was read
//   sha256_1m        SHA-256 (mbedTLS, hardware accelerated in the default configuration) of 1 MB
//                    of a fixed pattern, fed in 4 KB updates; the digest
//   spi2_153600      one full 240 x 320 RGB565 frame written to the panel on SPI2 (SCLK GPIO8,
//                    MOSI GPIO9, CS GPIO1, DC GPIO20) at 40 MHz,
//                    framed exactly as esp_lcd frames a flush (esp_lcd_panel_st7789.c): CASET
//                    0..239 and RASET 0..319, each a command byte with DC low and CS held, then
//                    its parameters with DC high; RAMWR with DC low and CS held; then the 153,600
//                    pixel bytes with DC high, in chunks of at most 32,768 bytes (the
//                    per-transaction limit of the official firmware), CS held
//                    until the last. The time covers the whole sequence; the fact is the number of
//                    SPI transactions: 5 framing (the CASET command and its parameters, the RASET
//                    command and its parameters, the RAMWR command) and 5 pixel chunks, 10 in all
//                    (FRAME_TRANSACTIONS), which is what silicon did in the 2026-09-23 capture
//   i2c_read_100     100 one-byte register reads from the ES8311 at 0x18 on I2C0 (SDA GPIO10,
//                    SCL GPIO7); how many succeeded and the first value.
//                    The register, 0xFD, is the codec's chip id register (UNVERIFIED: any readable
//                    register serves the timing)
//   erase_4k         esp_partition_erase_range of one 4 KB sector of this probe's `scratch`
//                    partition (partitions.csv)
//
// Prints, as probe lines (probes/common/probe_line.h):
//   TIMING  one line per operation: name, microseconds, and the fact above
//
// Not deterministic on silicon, by design: the microseconds are the measurement. In the emulator
// they are a function of the timing profile. The facts (CRC, digest, counts) are deterministic.
//
// Runs on the device only with explicit approval, through the planner, and after
// a backup of the `scratch` region it erases (probes/README.md "Device runs"). The probe does not
// initialize the panel (no SLPOUT, COLMOD or DISPON), so on silicon the frame is written into a
// sleeping panel and nothing need appear: the transfer is what is timed. The I2C reads only read.
//
// Consumed by: tests/milestones/m11.rs, and only if a device capture was approved.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "driver/gpio.h"
#include "driver/i2c_master.h"
#include "driver/spi_master.h"
#include "esp_heap_caps.h"
#include "esp_partition.h"
#include "esp_rom_crc.h"
#include "esp_timer.h"
#include "mbedtls/sha256.h"

#include "probe_line.h"

#define PROBE_NAME "probe_timing"

#define KB 1024u

#define FLASH_READ_BYTES (64u * KB)
#define SHA_BYTES (1024u * KB)
#define SHA_CHUNK (4u * KB)
#define FRAME_BYTES 153600u
#define SPI_CHUNK 32768u
// Transactions of one framed frame: 5 framing transactions (CASET and RASET as a command and its
// parameters each, then RAMWR) and one per pixel chunk.
#define FRAME_TRANSACTIONS (5 + (int)((FRAME_BYTES + SPI_CHUNK - 1) / SPI_CHUNK))
#define I2C_READS 100
#define ERASE_BYTES (4u * KB)

// Board pins of the Passport.
#define PIN_SPI_SCLK 8
#define PIN_SPI_MOSI 9
#define PIN_LCD_CS 1
#define PIN_LCD_DC 20
#define PIN_I2C_SDA 10
#define PIN_I2C_SCL 7
#define ES8311_ADDR 0x18
#define ES8311_CHIP_ID_REG 0xFD

static bool s_ok = true;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

static void time_flash_read(void)
{
    const esp_partition_t *app =
        esp_partition_find_first(ESP_PARTITION_TYPE_APP, ESP_PARTITION_SUBTYPE_APP_FACTORY, NULL);
    uint8_t *buf = heap_caps_malloc(FLASH_READ_BYTES, MALLOC_CAP_8BIT);
    if (app == NULL || buf == NULL) {
        fail("flash_read_64k", "no factory partition or no 64 KB buffer");
        free(buf);
        return;
    }
    int64_t t0 = esp_timer_get_time();
    esp_err_t rc = esp_partition_read(app, 0, buf, FLASH_READ_BYTES);
    int64_t us = esp_timer_get_time() - t0;
    uint32_t crc = esp_rom_crc32_le(0, buf, FLASH_READ_BYTES);
    printf("TIMING|flash_read_64k|us=%" PRId64 "|rc=%d|crc32=0x%08" PRIx32 "\n", us, rc, crc);
    if (rc != ESP_OK) {
        fail("flash_read_64k", "esp_partition_read failed");
    }
    free(buf);
}

static void time_sha256(void)
{
    uint8_t *chunk = heap_caps_malloc(SHA_CHUNK, MALLOC_CAP_8BIT);
    if (chunk == NULL) {
        fail("sha256_1m", "no 4 KB buffer");
        return;
    }
    uint8_t digest[32];
    mbedtls_sha256_context ctx;
    int64_t t0 = esp_timer_get_time();
    mbedtls_sha256_init(&ctx);
    int rc = mbedtls_sha256_starts(&ctx, 0);
    for (uint32_t done = 0; rc == 0 && done < SHA_BYTES; done += SHA_CHUNK) {
        for (uint32_t i = 0; i < SHA_CHUNK; i++) {
            chunk[i] = (uint8_t)(((done + i) * 131u + 3u) & 0xffu);
        }
        rc = mbedtls_sha256_update(&ctx, chunk, SHA_CHUNK);
    }
    if (rc == 0) {
        rc = mbedtls_sha256_finish(&ctx, digest);
    }
    mbedtls_sha256_free(&ctx);
    int64_t us = esp_timer_get_time() - t0;
    char hex[65];
    for (int i = 0; i < 32; i++) {
        snprintf(&hex[i * 2], 3, "%02x", digest[i]);
    }
    printf("TIMING|sha256_1m|us=%" PRId64 "|rc=%d|digest=%s\n", us, rc, rc == 0 ? hex : "none");
    if (rc != 0) {
        fail("sha256_1m", "mbedtls_sha256 failed");
    }
    free(chunk);
}

static void time_erase(void)
{
    const esp_partition_t *scratch =
        esp_partition_find_first(ESP_PARTITION_TYPE_DATA, 0x40, "scratch");
    if (scratch == NULL) {
        fail("erase_4k", "no `scratch` data partition");
        return;
    }
    int64_t t0 = esp_timer_get_time();
    esp_err_t rc = esp_partition_erase_range(scratch, 0, ERASE_BYTES);
    int64_t us = esp_timer_get_time() - t0;
    printf("TIMING|erase_4k|us=%" PRId64 "|rc=%d\n", us, rc);
    if (rc != ESP_OK) {
        fail("erase_4k", "esp_partition_erase_range failed");
    }
}

// ST7789 commands (ST7789 datasheet).
#define LCD_CASET 0x2A
#define LCD_RASET 0x2B
#define LCD_RAMWR 0x2C

// One SPI transaction with DC at `dc`; `keep_cs` holds CS asserted for the next one, as esp_lcd
// does between a command and its data.
static esp_err_t lcd_send(spi_device_handle_t dev, int dc, const uint8_t *data, size_t len,
                          bool keep_cs, int *transactions)
{
    gpio_set_level(PIN_LCD_DC, dc);
    spi_transaction_t t = {
        .flags = keep_cs ? SPI_TRANS_CS_KEEP_ACTIVE : 0,
        .length = len * 8u,
        .tx_buffer = data,
    };
    (*transactions)++;
    return spi_device_polling_transmit(dev, &t);
}

// A command byte with DC low, then its parameters with DC high, CS held across the two.
static esp_err_t lcd_command(spi_device_handle_t dev, uint8_t cmd, const uint8_t *params,
                             size_t len, int *transactions)
{
    esp_err_t rc = lcd_send(dev, 0, &cmd, 1, len > 0, transactions);
    if (rc == ESP_OK && len > 0) {
        rc = lcd_send(dev, 1, params, len, false, transactions);
    }
    return rc;
}

static void time_spi2(void)
{
    spi_bus_config_t bus = {
        .mosi_io_num = PIN_SPI_MOSI,
        .miso_io_num = -1,
        .sclk_io_num = PIN_SPI_SCLK,
        .quadwp_io_num = -1,
        .quadhd_io_num = -1,
        .max_transfer_sz = SPI_CHUNK,
    };
    spi_device_interface_config_t dev_cfg = {
        .mode = 0,
        .clock_speed_hz = 40 * 1000 * 1000,
        .spics_io_num = PIN_LCD_CS,
        .queue_size = 1,
    };
    gpio_config_t dc_cfg = {
        .pin_bit_mask = 1ULL << PIN_LCD_DC,
        .mode = GPIO_MODE_OUTPUT,
    };
    spi_device_handle_t dev = NULL;
    uint8_t *buf = heap_caps_malloc(SPI_CHUNK, MALLOC_CAP_DMA);
    if (buf == NULL || gpio_config(&dc_cfg) != ESP_OK ||
        spi_bus_initialize(SPI2_HOST, &bus, SPI_DMA_CH_AUTO) != ESP_OK ||
        spi_bus_add_device(SPI2_HOST, &dev_cfg, &dev) != ESP_OK) {
        fail("spi2_153600", "SPI2 setup failed");
        free(buf);
        return;
    }
    // RGB565 stripes, big-endian as the panel takes them.
    for (uint32_t i = 0; i < SPI_CHUNK; i += 2) {
        uint16_t color = ((i / 2) % 240u) < 120u ? 0xF800u : 0x001Fu;
        buf[i] = (uint8_t)(color >> 8);
        buf[i + 1] = (uint8_t)color;
    }
    static const uint8_t columns[] = {0x00, 0x00, 0x00, 0xEF}; // 0..239
    static const uint8_t rows[] = {0x00, 0x00, 0x01, 0x3F};    // 0..319
    const uint8_t ramwr = LCD_RAMWR;
    int transactions = 0;
    // CS may be held across transactions only while the bus is acquired.
    esp_err_t rc = spi_device_acquire_bus(dev, portMAX_DELAY);
    int64_t t0 = esp_timer_get_time();
    if (rc == ESP_OK) {
        rc = lcd_command(dev, LCD_CASET, columns, sizeof(columns), &transactions);
    }
    if (rc == ESP_OK) {
        rc = lcd_command(dev, LCD_RASET, rows, sizeof(rows), &transactions);
    }
    if (rc == ESP_OK) {
        rc = lcd_send(dev, 0, &ramwr, 1, true, &transactions);
    }
    for (uint32_t done = 0; rc == ESP_OK && done < FRAME_BYTES; done += SPI_CHUNK) {
        uint32_t n = FRAME_BYTES - done < SPI_CHUNK ? FRAME_BYTES - done : SPI_CHUNK;
        bool last = done + n == FRAME_BYTES;
        rc = lcd_send(dev, 1, buf, n, !last, &transactions);
    }
    int64_t us = esp_timer_get_time() - t0;
    spi_device_release_bus(dev);
    printf("TIMING|spi2_153600|us=%" PRId64 "|rc=%d|transactions=%d\n", us, rc, transactions);
    if (rc != ESP_OK || transactions != FRAME_TRANSACTIONS) {
        fail("spi2_153600", "the framed SPI2 frame did not complete in FRAME_TRANSACTIONS (10) "
                            "transactions");
    }
    spi_bus_remove_device(dev);
    spi_bus_free(SPI2_HOST);
    free(buf);
}

static void time_i2c(void)
{
    i2c_master_bus_config_t bus_cfg = {
        .i2c_port = 0,
        .sda_io_num = PIN_I2C_SDA,
        .scl_io_num = PIN_I2C_SCL,
        .clk_source = I2C_CLK_SRC_DEFAULT,
        .glitch_ignore_cnt = 7,
        .flags.enable_internal_pullup = 1,
    };
    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = ES8311_ADDR,
        .scl_speed_hz = 100000,
    };
    i2c_master_bus_handle_t bus = NULL;
    i2c_master_dev_handle_t dev = NULL;
    if (i2c_new_master_bus(&bus_cfg, &bus) != ESP_OK ||
        i2c_master_bus_add_device(bus, &dev_cfg, &dev) != ESP_OK) {
        fail("i2c_read_100", "I2C0 setup failed");
        return;
    }
    const uint8_t reg = ES8311_CHIP_ID_REG;
    uint8_t value = 0;
    int first = -1;
    int good = 0;
    int64_t t0 = esp_timer_get_time();
    for (int i = 0; i < I2C_READS; i++) {
        if (i2c_master_transmit_receive(dev, &reg, 1, &value, 1, 100) == ESP_OK) {
            if (first < 0) {
                first = value;
            }
            good++;
        }
    }
    int64_t us = esp_timer_get_time() - t0;
    printf("TIMING|i2c_read_100|us=%" PRId64 "|ok=%d|first=%d\n", us, good, first);
    if (good != I2C_READS) {
        fail("i2c_read_100", "not every I2C read succeeded");
    }
    i2c_master_bus_rm_device(dev);
    i2c_del_master_bus(bus);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    time_flash_read();
    time_sha256();
    time_erase();
    time_spi2();
    time_i2c();
    PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
}
