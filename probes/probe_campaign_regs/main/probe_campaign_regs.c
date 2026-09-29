// probe_campaign_regs: register facts for the silicon evidence campaign, step 1
// (specs/notes/silicon-campaign.md). MIT.
// An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Every fact line names the inventory row it settles in its `row` field, so the device
// capture and the emulator's record compare row by row (`cargo xtask probes compare`).
//
//   REG     the value a register holds as the boot left it, one line per register of reg_facts.h
//           (every class C row a corpus image reaches whose value a read can observe), and the
//           I2S0 PCM2PDM and SARADC values of the steps below, and the IO_MUX words of GPIO0 to
//           GPIO21 (row iomux.reset_values)
//   RND    SYSCON_RND_DATA read 16 times: how many distinct values and how many zeros (the value
//           itself is entropy and is never printed)
//   GATE    what a peripheral register does while the block's clock is off or its reset is held:
//           I2S0 behind SYSTEM_PERIP_CLK_EN0 / RST_EN0, AES behind PERIP_CLK_EN1 / RST_EN1, and
//           TIMG0 and TIMG1 behind their own TIMG_REGCLK CLK_EN
//   MASK    the writable bits of SYSTEM_BT_LPCK_DIV_INT and _FRAC (all ones written, read back,
//           the boot value restored)
//   FLASH   32 bytes read at 0x800000, the first address past the 8 MB part, against 32 bytes
//           read at 0x000000; only a same/different verdict and the first 4 bytes of each are
//           printed (the bootloader's image header, no identity)
//   ADC     four one-shot conversions of ADC1 channel 0 (GPIO0, the button ladder) with no
//           button pressed, and the SARADC registers after the one-shot driver's setup; then,
//           at the end, an 8 s window in which the operator presses Up, then Down, then OK (a NOTE
//           line asks for it) and each press's raw code is printed
//   USJ     the width of USB_SERIAL_JTAG_FRAM_NUM: the largest frame number seen over 4.2 s
//
// Step 4 of the campaign appends, after the button window and every line above (so no earlier line
// moves), the rows the inventory sweep left open (specs/notes/silicon-campaign.md, "Step 4"):
//   GATE    <block>_latch: a clocked read of one pattern, a second pattern written unread, then
//           the clock gated: whether a gated read returns the last value read, the value last
//           written, or the writable bits (step 3 could not tell them apart), for I2S0 and AES;
//           the gate experiment above and its latch line on LEDC, I2C0, SPI2 and SHA, which no
//           capture gated; timg<n>_regclk_count, general timer 0 run at 1 MHz and latched before,
//           during and after 200 us with TIMG_REGCLK's CLK_EN clear (does the counter stop)
//   TIME    adc_oneshot_read: 64 one-shot reads of ADC1 channel 0 timed (the conversion time the
//           model takes as zero; no button is needed, the value is not printed)
//
// Safety, checked for every step (silicon campaign step 1):
//   - no flash write or erase: the only flash access is esp_flash_read, two reads of 32 bytes;
//     no NVS is initialised; the partition table is the device's own (partitions.csv), so no
//     segment of the image falls in cardid [0x356000, 0x35A000), and nothing reads it either;
//   - no eFuse write: EFUSE_RD_REPEAT_ERR0..3 are read, nothing in the eFuse block is written;
//   - no radio: no Wi-Fi or BLE call; APB_CTRL's radio registers are only read;
//   - no sleep of any kind;
//   - every register write is to a block no other code of this app uses at that moment (I2S0,
//     AES, the TIMG general timer 0 alarm word, the BT low-power clock divider; Part B adds LEDC,
//     I2C0, SPI2, SHA's message memory and the TIMG general timer 0 configuration, the timer
//     left stopped), inside a critical section, and the boot value of every register written is
//     restored before it ends; the clock and reset bits of SYSTEM_PERIP_* are restored exactly;
//   - the flash chip's configured size is raised to 16 MB only around the one read at 0x800000
//     and restored at once; esp_flash_read has no write path;
//   - the button window only reads the ADC; pressing nothing is safe and prints `count=0`;
//   - bounded: no wait loop without a count; the run takes about 13 s after boot (4.2 s of it the
//     frame-number sampling, 8 s the button window) and ends with a DONE line. The gate steps
//     set a marker in RTC memory while they run, so if an access to a gated block ever stalled
//     the bus and a watchdog reset the chip, the next boot reports it (`GATE|interrupted`),
//     skips those steps and still ends with DONE. That boot, and only that one, first waits for
//     the USB host as probe_campaign_reset does on every boot (wait_for_host), because a chip
//     reset may drop the USB Serial/JTAG link and a line printed before the host's capture has
//     reopened its port is lost on the device; it prints a WAIT line saying how long it waited,
//     5 s at most and then 1.5 s of settle. A run with no reset never waits and prints no WAIT.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "driver/usb_serial_jtag.h"
#include "esp_adc/adc_oneshot.h"
#include "esp_attr.h"
#include "esp_cpu.h"
#include "esp_flash.h"
#include "esp_rom_sys.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "soc/i2s_reg.h"
#include "soc/reg_base.h"
#include "soc/soc.h"
#include "soc/system_reg.h"
#include "soc/timer_group_reg.h"

#include "probe_line.h"
#include "reg_facts.h"

#define PROBE_NAME "probe_campaign_regs"

// The host wait of a boot after a gate step's reset (wait_for_host).
#define HOST_PRIME_MS 30
#define HOST_POLL_MS 10
#define HOST_WAIT_MS 5000
#define HOST_SETTLE_MS 1500

static bool s_ok = true;
static portMUX_TYPE s_mux = portMUX_INITIALIZER_UNLOCKED;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

static void reg_line(const char *fact, const char *row, uint32_t addr)
{
    printf("REG|%s|row=%s|addr=0x%08" PRIx32 "|val=0x%08" PRIx32 "\n", fact, row, addr,
           REG_READ(addr));
}

static void read_boot_values(void)
{
    for (size_t i = 0; i < sizeof(REG_FACTS) / sizeof(REG_FACTS[0]); i++) {
        reg_line(REG_FACTS[i].fact, REG_FACTS[i].row, REG_FACTS[i].addr);
    }
}

// SYSCON_RND_DATA (specs/blocks/apb_ctrl.toml): the value is entropy, so only its behaviour is
// printed: distinct values and zeros among 16 reads.
static void read_rnd(void)
{
    uint32_t seen[16];
    int distinct = 0;
    int zeros = 0;
    for (int i = 0; i < 16; i++) {
        uint32_t v = REG_READ(DR_REG_APB_CTRL_BASE + 0x0B0);
        if (v == 0) {
            zeros++;
        }
        bool fresh = true;
        for (int j = 0; j < distinct; j++) {
            if (seen[j] == v) {
                fresh = false;
            }
        }
        if (fresh) {
            seen[distinct++] = v;
        }
    }
    printf("RND|apb_ctrl.SYSCON_RND_DATA|row=apb_ctrl.SYSCON_RND_DATA|reads=16|distinct=%d|zeros=%d\n",
           distinct, zeros);
}

// One gate experiment on a plain storage register `reg` of a block whose clock enable is bit
// `bit` of `clk_reg` and whose reset is bit `bit` of `rst_reg`. Prints four lines:
//   <name>_rst_held   clock on, reset held: a pattern written, then read back
//   <name>_released   clock on, reset released: the value the register reads, then a pattern
//                     written and read back (its writable bits)
//   <name>_clk_off    clock off, reset released: a pattern written, then read back, then read
//                     again once the clock is back on (whether the gated write landed)
// The two SYSTEM registers are restored to their boot values afterwards.
static void gate_experiment(const char *name, const char *row, uint32_t clk_reg, uint32_t rst_reg,
                            uint32_t bit, uint32_t reg)
{
    uint32_t clk0 = REG_READ(clk_reg);
    uint32_t rst0 = REG_READ(rst_reg);
    uint32_t held_read;
    uint32_t released;
    uint32_t mask;
    uint32_t off_read;
    uint32_t off_after;

    portENTER_CRITICAL(&s_mux);
    // Clock on, reset held.
    REG_WRITE(clk_reg, clk0 | bit);
    REG_WRITE(rst_reg, rst0 | bit);
    REG_WRITE(reg, 0x5A5A5A5Au);
    held_read = REG_READ(reg);
    // Clock on, reset released.
    REG_WRITE(rst_reg, rst0 & ~bit);
    released = REG_READ(reg);
    REG_WRITE(reg, 0xFFFFFFFFu);
    mask = REG_READ(reg);
    REG_WRITE(reg, released);
    // Clock off, reset released.
    REG_WRITE(clk_reg, clk0 & ~bit);
    REG_WRITE(reg, 0x12345678u);
    off_read = REG_READ(reg);
    REG_WRITE(clk_reg, clk0 | bit);
    off_after = REG_READ(reg);
    REG_WRITE(reg, released);
    // Back to the boot state: reset first, then the clock.
    REG_WRITE(rst_reg, rst0);
    REG_WRITE(clk_reg, clk0);
    portEXIT_CRITICAL(&s_mux);

    printf("GATE|%s_boot|row=%s|clk_bit=%d|rst_bit=%d\n", name, row, (clk0 & bit) != 0,
           (rst0 & bit) != 0);
    printf("GATE|%s_rst_held|row=%s|wrote=0x5a5a5a5a|read=0x%08" PRIx32 "\n", name, row,
           held_read);
    printf("GATE|%s_released|row=%s|read=0x%08" PRIx32 "|mask=0x%08" PRIx32 "\n", name, row,
           released, mask);
    printf("GATE|%s_clk_off|row=%s|wrote=0x12345678|read=0x%08" PRIx32 "|after_clk_on=0x%08" PRIx32
           "\n",
           name, row, off_read, off_after);
    if (REG_READ(clk_reg) != clk0 || REG_READ(rst_reg) != rst0) {
        fail(name, "the SYSTEM clock or reset register did not return to its boot value");
    }
}

// I2S0 behind SYSTEM_PERIP_CLK_EN0 and RST_EN0. The scratch register is I2S_TX_TIMING, which no
// corpus image writes; the PCM2PDM value is read with the block clocked and out of reset, which
// is its reset value (specs/blocks/i2s0.toml I2S_TX_PCM2PDM_CONF).
static void gate_i2s0(void)
{
    gate_experiment("i2s0", "system.SYSTEM_PERIP_CLK_EN0,system.SYSTEM_PERIP_RST_EN0",
                    SYSTEM_PERIP_CLK_EN0_REG, SYSTEM_PERIP_RST_EN0_REG, SYSTEM_I2S0_CLK_EN,
                    I2S_TX_TIMING_REG(0));
    uint32_t clk0 = REG_READ(SYSTEM_PERIP_CLK_EN0_REG);
    uint32_t rst0 = REG_READ(SYSTEM_PERIP_RST_EN0_REG);
    uint32_t pdm;
    portENTER_CRITICAL(&s_mux);
    REG_WRITE(SYSTEM_PERIP_CLK_EN0_REG, clk0 | SYSTEM_I2S0_CLK_EN);
    REG_WRITE(SYSTEM_PERIP_RST_EN0_REG, rst0 | SYSTEM_I2S0_RST);
    REG_WRITE(SYSTEM_PERIP_RST_EN0_REG, rst0 & ~SYSTEM_I2S0_RST);
    pdm = REG_READ(I2S_TX_PCM2PDM_CONF_REG(0));
    REG_WRITE(SYSTEM_PERIP_RST_EN0_REG, rst0);
    REG_WRITE(SYSTEM_PERIP_CLK_EN0_REG, clk0);
    portEXIT_CRITICAL(&s_mux);
    printf("REG|i2s0.I2S_TX_PCM2PDM_CONF.reset|row=i2s0.I2S_TX_PCM2PDM_CONF|addr=0x%08" PRIx32
           "|val=0x%08" PRIx32 "\n",
           (uint32_t)I2S_TX_PCM2PDM_CONF_REG(0), pdm);
}

// AES behind SYSTEM_PERIP_CLK_EN1 and RST_EN1; the scratch register is AES_KEY_0 (offset 0x000,
// specs/blocks/aes.toml), which nothing else uses while this runs.
static void gate_aes(void)
{
    gate_experiment("aes", "system.SYSTEM_PERIP_CLK_EN1,system.SYSTEM_PERIP_RST_EN1",
                    SYSTEM_PERIP_CLK_EN1_REG, SYSTEM_PERIP_RST_EN1_REG, SYSTEM_CRYPTO_AES_CLK_EN,
                    DR_REG_AES_BASE + 0x000);
}

// TIMG_REGCLK CLK_EN (bit 31) of one timer group (specs/blocks/timg0.toml TIMG_REGCLK: "clearing
// it does not stop the group in the model"). A pattern is written to TIMG_T0ALARMLO (general timer
// 0's alarm word; timer 0 is not used by this app, and its alarm is off) and read back once with
// CLK_EN set and once with it clear, the DATE word is read with it clear, and the alarm word is
// read again once CLK_EN is set again. The alarm word and TIMG_REGCLK are restored.
static void gate_timg(int group, const char *row)
{
    uint32_t regclk = REG_READ(TIMG_REGCLK_REG(group));
    uint32_t alarm0 = REG_READ(TIMG_T0ALARMLO_REG(group));
    uint32_t set_read;
    uint32_t clear_read;
    uint32_t date_clear;
    uint32_t after_set;
    portENTER_CRITICAL(&s_mux);
    REG_WRITE(TIMG_REGCLK_REG(group), regclk | TIMG_CLK_EN);
    REG_WRITE(TIMG_T0ALARMLO_REG(group), 0x0000A5A5u);
    set_read = REG_READ(TIMG_T0ALARMLO_REG(group));
    REG_WRITE(TIMG_REGCLK_REG(group), regclk & ~TIMG_CLK_EN);
    REG_WRITE(TIMG_T0ALARMLO_REG(group), 0x00005A5Au);
    clear_read = REG_READ(TIMG_T0ALARMLO_REG(group));
    date_clear = REG_READ(TIMG_NTIMERS_DATE_REG(group));
    REG_WRITE(TIMG_REGCLK_REG(group), regclk | TIMG_CLK_EN);
    after_set = REG_READ(TIMG_T0ALARMLO_REG(group));
    REG_WRITE(TIMG_T0ALARMLO_REG(group), alarm0);
    REG_WRITE(TIMG_REGCLK_REG(group), regclk);
    portEXIT_CRITICAL(&s_mux);
    printf("GATE|timg%d_regclk|row=%s|boot=0x%08" PRIx32 "|set_read=0x%08" PRIx32
           "|clear_read=0x%08" PRIx32 "|after_set=0x%08" PRIx32 "|date_clear=0x%08" PRIx32 "\n",
           group, row, regclk, set_read, clear_read, after_set, date_clear);
    if (REG_READ(TIMG_REGCLK_REG(group)) != regclk) {
        fail("timg_regclk", "TIMG_REGCLK did not return to its boot value");
    }
}

// The writable bits of a register no running code uses (the BT low-power clock divider: this app
// starts no controller), with the boot value restored.
static void mask_of(const char *fact, const char *row, uint32_t addr)
{
    uint32_t boot;
    uint32_t mask;
    portENTER_CRITICAL(&s_mux);
    boot = REG_READ(addr);
    REG_WRITE(addr, 0xFFFFFFFFu);
    mask = REG_READ(addr);
    REG_WRITE(addr, boot);
    portEXIT_CRITICAL(&s_mux);
    printf("MASK|%s|row=%s|boot=0x%08" PRIx32 "|mask=0x%08" PRIx32 "\n", fact, row, boot, mask);
}

// What the part returns for an address past its 8 MB. The
// driver refuses such an address against the configured size, so the size is raised to 16 MB
// around this one read (a read: no write or erase path is reachable) and restored.
static void flash_beyond_the_part(void)
{
    uint8_t low[32];
    uint8_t high[32];
    memset(low, 0, sizeof(low));
    memset(high, 0, sizeof(high));
    esp_flash_t *chip = esp_flash_default_chip;
    esp_err_t rc_low = esp_flash_read(chip, low, 0x000000, sizeof(low));
    uint32_t size = chip->size;
    chip->size = 16u * 1024u * 1024u;
    esp_err_t rc_high = esp_flash_read(chip, high, 0x800000, sizeof(high));
    chip->size = size;
    int ff = 0;
    for (size_t i = 0; i < sizeof(high); i++) {
        ff += high[i] == 0xFF;
    }
    printf("FLASH|read_0x800000|row=flash_xmc.any command at 0x800000 or above|rc_low=%d|rc_high=%d"
           "|same_as_0x000000=%d|ff_bytes=%d|low4=%02x%02x%02x%02x|high4=%02x%02x%02x%02x"
           "|size_restored=%d\n",
           rc_low, rc_high, memcmp(low, high, sizeof(low)) == 0, ff, low[0], low[1], low[2],
           low[3], high[0], high[1], high[2], high[3], chip->size == size);
    if (chip->size != size) {
        fail("flash_size", "the configured flash size was not restored");
    }
}

// Four one-shot conversions of ADC1 channel 0, GPIO0, the button ladder, with no button pressed
// (specs/blocks/saradc.toml stable_read APB_SARADC_1_DATA_STATUS: "released 4095, UNVERIFIED"),
// at the 12 dB attenuation a ladder reaching the rail needs; then the SARADC registers the
// one-shot driver has set up.
static void adc_idle(void)
{
    adc_oneshot_unit_handle_t unit = NULL;
    adc_oneshot_unit_init_cfg_t unit_cfg = {
        .unit_id = ADC_UNIT_1,
    };
    adc_oneshot_chan_cfg_t chan_cfg = {
        .atten = ADC_ATTEN_DB_12,
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    if (adc_oneshot_new_unit(&unit_cfg, &unit) != ESP_OK ||
        adc_oneshot_config_channel(unit, ADC_CHANNEL_0, &chan_cfg) != ESP_OK) {
        fail("adc", "ADC1 one-shot setup failed");
        return;
    }
    int raw[4] = {-1, -1, -1, -1};
    for (int i = 0; i < 4; i++) {
        if (adc_oneshot_read(unit, ADC_CHANNEL_0, &raw[i]) != ESP_OK) {
            raw[i] = -1;
        }
    }
    printf("ADC|gpio0_idle|row=saradc.APB_SARADC_1_DATA_STATUS|raw0=%d|raw1=%d|raw2=%d|raw3=%d\n",
           raw[0], raw[1], raw[2], raw[3]);
    static const struct {
        const char *fact;
        const char *row;
        uint32_t off;
    } after[] = {
        {"saradc.APB_SARADC_CTRL.after_adc", "saradc.APB_SARADC_CTRL", 0x000},
        {"saradc.APB_SARADC_FSM_WAIT.after_adc", "saradc.APB_SARADC_FSM_WAIT", 0x00C},
        {"saradc.APB_SARADC_APB_ADC_ARB_CTRL.after_adc", "saradc.APB_SARADC_APB_ADC_ARB_CTRL",
         0x024},
        {"saradc.APB_SARADC_APB_ADC_CLKM_CONF.after_adc", "saradc.APB_SARADC_APB_ADC_CLKM_CONF",
         0x054},
    };
    for (size_t i = 0; i < sizeof(after) / sizeof(after[0]); i++) {
        reg_line(after[i].fact, after[i].row, DR_REG_APB_SARADC_BASE + after[i].off);
    }
    adc_oneshot_del_unit(unit);
}

// The IO_MUX pad words of GPIO0 to GPIO21 as the boot left them (specs/blocks/iomux.toml
// reset_domains: "the per-pad reset values are UNVERIFIED here"). Every corpus image's first
// touch of pads 1 to 22 of the window is a read, so the value a pad holds before any firmware
// writes it matters; the pads this app's boot never configures still hold it.
static void read_iomux(void)
{
    char fact[32];
    for (int n = 0; n <= 21; n++) {
        snprintf(fact, sizeof(fact), "iomux.IO_MUX_GPIO%d", n);
        reg_line(fact, "iomux.reset_values", DR_REG_IO_MUX_BASE + 0x004 + 4u * (uint32_t)n);
    }
}

// USB_SERIAL_JTAG_FRAM_NUM (specs/blocks/usj.toml: the IDF bitpos comment writes [11:0]
// while the header's _V macro gives 11 bits): the frame number sampled every 10 ms for 4.2 s, more
// than one full turn of a 12-bit count at one SOF per millisecond. The largest value seen, the
// number of times it went down (a wrap) and the number of samples that changed are printed.
static void fram_num_width(void)
{
    uint32_t prev = REG_READ(DR_REG_USB_SERIAL_JTAG_BASE + 0x024);
    uint32_t max = prev;
    int wraps = 0;
    int moved = 0;
    for (int i = 0; i < 420; i++) {
        vTaskDelay(pdMS_TO_TICKS(10));
        uint32_t v = REG_READ(DR_REG_USB_SERIAL_JTAG_BASE + 0x024);
        if (v != prev) {
            moved++;
        }
        if (v < prev) {
            wraps++;
        }
        if (v > max) {
            max = v;
        }
        prev = v;
    }
    printf("USJ|fram_num_width|row=usj.USB_SERIAL_JTAG_FRAM_NUM|samples=420|period_ms=10|max=%" PRIu32
           "|over_2047=%d|wraps=%d|moved=%d\n",
           max, max > 2047, wraps, moved);
}

// The button ladder's pressed codes (specs/blocks/saradc.toml stable_read
// APB_SARADC_1_DATA_STATUS: "up 0, down 433, ok 862, released 4095, all UNVERIFIED"). The probe
// asks for Up, then Down, then OK, one at a time, and samples ADC1 channel 0 every 10 ms for 8 s.
// Every run of at least 3 consecutive samples below 3800 is one press: its median, lowest and
// highest raw value and its length are printed, in the order pressed. The emulator's record
// scripts the same three presses (tests/milestones/campaign.rs).
#define PRESS_SAMPLES 800
#define PRESS_THRESHOLD 3800
#define PRESS_MIN_SAMPLES 3
#define PRESS_MAX 6

static int cmp_int(const void *a, const void *b)
{
    int x = *(const int *)a;
    int y = *(const int *)b;
    return (x > y) - (x < y);
}

static void adc_presses(void)
{
    static int samples[PRESS_SAMPLES];
    adc_oneshot_unit_handle_t unit = NULL;
    adc_oneshot_unit_init_cfg_t unit_cfg = {
        .unit_id = ADC_UNIT_1,
    };
    adc_oneshot_chan_cfg_t chan_cfg = {
        .atten = ADC_ATTEN_DB_12,
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    if (adc_oneshot_new_unit(&unit_cfg, &unit) != ESP_OK ||
        adc_oneshot_config_channel(unit, ADC_CHANNEL_0, &chan_cfg) != ESP_OK) {
        fail("adc", "ADC1 one-shot setup failed");
        return;
    }
    printf("NOTE|press Up, then Down, then OK, one at a time, within 8 s\n");
    fflush(stdout);
    for (int i = 0; i < PRESS_SAMPLES; i++) {
        if (adc_oneshot_read(unit, ADC_CHANNEL_0, &samples[i]) != ESP_OK) {
            samples[i] = -1;
        }
        vTaskDelay(pdMS_TO_TICKS(10));
    }
    adc_oneshot_del_unit(unit);
    int presses = 0;
    int i = 0;
    while (i < PRESS_SAMPLES) {
        if (samples[i] < 0 || samples[i] >= PRESS_THRESHOLD) {
            i++;
            continue;
        }
        int start = i;
        while (i < PRESS_SAMPLES && samples[i] >= 0 && samples[i] < PRESS_THRESHOLD) {
            i++;
        }
        int n = i - start;
        if (n < PRESS_MIN_SAMPLES || presses >= PRESS_MAX) {
            continue;
        }
        presses++;
        qsort(&samples[start], (size_t)n, sizeof(int), cmp_int);
        printf("ADC|press_%d|row=saradc.APB_SARADC_1_DATA_STATUS|median=%d|min=%d|max=%d|samples=%d\n",
               presses, samples[start + n / 2], samples[start], samples[start + n - 1], n);
    }
    printf("ADC|presses|row=saradc.APB_SARADC_1_DATA_STATUS|count=%d|window_ms=%d\n", presses,
           PRESS_SAMPLES * 10);
}

// The step that writes registers of a gated or reset-held block, held in RTC memory across a reset.
// A bus access to a gated block is not expected to stall on this chip, but if one does, the
// interrupt watchdog resets the chip with the marker still set, and the next boot reports that
// and skips the gate steps instead of repeating them: the run stays bounded either way.
#define GATE_MAGIC 0x47415445u // "GATE"
RTC_NOINIT_ATTR static uint32_t s_gate_magic;
RTC_NOINIT_ATTR static uint32_t s_gate_step;

// Set when a gate step reset the chip on the previous boot, so that every gate step of this boot,
// Part B's (gate_steps_b) included, is skipped.
static bool s_gates_skipped;

static void gate_steps(void)
{
    if (s_gate_magic == GATE_MAGIC && s_gate_step != 0) {
        printf("GATE|interrupted|row=system.SYSTEM_PERIP_CLK_EN0|step=%" PRIu32 "|reason=%d\n",
               s_gate_step, (int)esp_reset_reason());
        s_gate_magic = 0;
        s_gates_skipped = true;
        fail("gate", "a gate step reset the chip on the previous boot; gate steps skipped");
        return;
    }
    s_gate_magic = GATE_MAGIC;
    s_gate_step = 1;
    gate_i2s0();
    s_gate_step = 2;
    gate_aes();
    s_gate_step = 3;
    gate_timg(0, "timg0.TIMG_REGCLK");
    s_gate_step = 4;
    gate_timg(1, "timg1.TIMG_REGCLK");
    s_gate_step = 0;
    s_gate_magic = 0;
}

// Campaign step 4. The gated read of step 3 (gate_experiment, `clk_off`) read the value the
// block returned last, which was also its writable bits, so two mechanisms fit it. Here the block
// is pulsed out of reset with its clock on, `p1` is written and read back, `p3` is written and not
// read, the clock is gated and the register read (p1: the last value read; p3: the value written;
// the writable bits: the mask reading), `p2` is written gated and read, and the clock is turned on
// and the register read (whether the gated write landed). The boot value and both SYSTEM bits are
// restored.
static void gate_latch(const char *name, const char *row, uint32_t clk_reg, uint32_t rst_reg,
                       uint32_t bit, uint32_t reg, uint32_t p1, uint32_t p3, uint32_t p2)
{
    uint32_t clk0 = REG_READ(clk_reg);
    uint32_t rst0 = REG_READ(rst_reg);
    uint32_t on_read;
    uint32_t off_read;
    uint32_t off_write_read;
    uint32_t after;
    uint32_t reset_value;
    portENTER_CRITICAL(&s_mux);
    REG_WRITE(clk_reg, clk0 | bit);
    REG_WRITE(rst_reg, rst0 | bit);
    REG_WRITE(rst_reg, rst0 & ~bit);
    reset_value = REG_READ(reg);
    REG_WRITE(reg, p1);
    on_read = REG_READ(reg);
    REG_WRITE(reg, p3);
    REG_WRITE(clk_reg, clk0 & ~bit);
    off_read = REG_READ(reg);
    REG_WRITE(reg, p2);
    off_write_read = REG_READ(reg);
    REG_WRITE(clk_reg, clk0 | bit);
    after = REG_READ(reg);
    REG_WRITE(reg, reset_value);
    REG_WRITE(rst_reg, rst0);
    REG_WRITE(clk_reg, clk0);
    portEXIT_CRITICAL(&s_mux);
    printf("GATE|%s_latch|row=%s|wrote=0x%08" PRIx32 "|on_read=0x%08" PRIx32 "|wrote_unread=0x%08" PRIx32
           "|off_read=0x%08" PRIx32 "|off_wrote=0x%08" PRIx32 "|off_write_read=0x%08" PRIx32
           "|after_clk_on=0x%08" PRIx32 "\n",
           name, row, p1, on_read, p3, off_read, p2, off_write_read, after);
    if (REG_READ(clk_reg) != clk0 || REG_READ(rst_reg) != rst0) {
        fail(name, "the SYSTEM clock or reset register did not return to its boot value");
    }
}

#define ROW_EN0 "system.SYSTEM_PERIP_CLK_EN0,system.SYSTEM_PERIP_RST_EN0"
#define ROW_EN1 "system.SYSTEM_PERIP_CLK_EN1,system.SYSTEM_PERIP_RST_EN1"

// The blocks the gate steps run on, each with a plain storage register nothing else of this app
// uses (IDF v5.5.3 soc/esp32c3/register/soc/*_reg.h): the latch line on I2S0 and AES, whose gate
// experiment step 3 captured, and both on LEDC (LEDC_LSCH0_HPOINT, 14 bits), I2C0
// (I2C_SCL_LOW_PERIOD, 9 bits), SPI2 (SPI_MS_DLEN, 18 bits) and SHA (the first word of its message
// memory, SHA_TEXT_BASE), which no capture gated. Every pattern is inside the register's writable
// bits and is neither 0 nor those bits.
static void gate_blocks(void)
{
    gate_latch("i2s0", ROW_EN0, SYSTEM_PERIP_CLK_EN0_REG, SYSTEM_PERIP_RST_EN0_REG, SYSTEM_I2S0_CLK_EN,
               I2S_TX_TIMING_REG(0), 0x00110011u, 0x00220002u, 0x00010020u);
    gate_latch("aes", ROW_EN1, SYSTEM_PERIP_CLK_EN1_REG, SYSTEM_PERIP_RST_EN1_REG,
               SYSTEM_CRYPTO_AES_CLK_EN, DR_REG_AES_BASE + 0x000, 0x0f0f0f0fu, 0x3c3c3c3cu, 0x00ff00ffu);
    static const struct {
        const char *name;
        const char *row;
        uint32_t clk_reg;
        uint32_t rst_reg;
        uint32_t bit;
        uint32_t reg;
        uint32_t p1;
        uint32_t p3;
        uint32_t p2;
    } blocks[] = {
        {"ledc", ROW_EN0, SYSTEM_PERIP_CLK_EN0_REG, SYSTEM_PERIP_RST_EN0_REG, SYSTEM_LEDC_CLK_EN,
         DR_REG_LEDC_BASE + 0x004, 0x00000155u, 0x000002aau, 0x00000033u},
        {"i2c0", ROW_EN0, SYSTEM_PERIP_CLK_EN0_REG, SYSTEM_PERIP_RST_EN0_REG, SYSTEM_I2C_EXT0_CLK_EN,
         DR_REG_I2C_EXT_BASE + 0x000, 0x00000055u, 0x000000aau, 0x00000033u},
        {"spi2", ROW_EN0, SYSTEM_PERIP_CLK_EN0_REG, SYSTEM_PERIP_RST_EN0_REG, SYSTEM_SPI2_CLK_EN,
         DR_REG_SPI2_BASE + 0x01C, 0x00015555u, 0x0002aaaau, 0x00003333u},
        {"sha", ROW_EN1, SYSTEM_PERIP_CLK_EN1_REG, SYSTEM_PERIP_RST_EN1_REG,
         SYSTEM_CRYPTO_SHA_CLK_EN, DR_REG_SHA_BASE + 0x080, 0x0f0f0f0fu, 0x3c3c3c3cu, 0x00ff00ffu},
    };
    for (size_t i = 0; i < sizeof(blocks) / sizeof(blocks[0]); i++) {
        gate_experiment(blocks[i].name, blocks[i].row, blocks[i].clk_reg, blocks[i].rst_reg,
                        blocks[i].bit, blocks[i].reg);
        gate_latch(blocks[i].name, blocks[i].row, blocks[i].clk_reg, blocks[i].rst_reg, blocks[i].bit,
                   blocks[i].reg, blocks[i].p1, blocks[i].p3, blocks[i].p2);
    }
}

// General timer 0 of `group` latched and read; `ok` is 0 if the latch request did not clear within
// a bounded poll (IDF's timer_ll_trigger_soft_capture waits for it, since the counter runs in
// another clock domain, which a gated register clock might stop).
static uint32_t timg_t0_read(int group, int *ok)
{
    REG_WRITE(TIMG_T0UPDATE_REG(group), TIMG_T0_UPDATE);
    int n = 0;
    while ((REG_READ(TIMG_T0UPDATE_REG(group)) & TIMG_T0_UPDATE) && n < 10000) {
        n++;
    }
    *ok = (REG_READ(TIMG_T0UPDATE_REG(group)) & TIMG_T0_UPDATE) == 0;
    return REG_READ(TIMG_T0LO_REG(group));
}

// TIMG_REGCLK's CLK_EN and the counters (specs/blocks/timg0.toml TIMG_REGCLK: "clearing it does
// not stop the group in the model"). General timer 0, which this app never uses, is set to count
// up from 0 at APB / 80 (1 MHz), as IDF's timer_ll sets a divider (divider, then DIVCNT_RST); it
// is latched after 200 us, CLK_EN is cleared, it is latched after 200 us more, CLK_EN is set, it
// is latched at once and after 200 us more. Each latch says whether its request cleared. Timer 0's
// configuration and TIMG_REGCLK are restored; the timer is left stopped, as it was.
static void gate_timg_count(int group, const char *row)
{
    uint32_t regclk = REG_READ(TIMG_REGCLK_REG(group));
    uint32_t cfg0 = REG_READ(TIMG_T0CONFIG_REG(group));
    uint32_t on;
    uint32_t off;
    uint32_t back;
    uint32_t later;
    int ok_on;
    int ok_off;
    int ok_back;
    int ok_later;
    portENTER_CRITICAL(&s_mux);
    REG_WRITE(TIMG_REGCLK_REG(group), regclk | TIMG_CLK_EN);
    REG_WRITE(TIMG_T0CONFIG_REG(group), TIMG_T0_INCREASE | (80u << TIMG_T0_DIVIDER_S));
    REG_SET_BIT(TIMG_T0CONFIG_REG(group), TIMG_T0_DIVCNT_RST);
    REG_WRITE(TIMG_T0LOADLO_REG(group), 0);
    REG_WRITE(TIMG_T0LOADHI_REG(group), 0);
    REG_WRITE(TIMG_T0LOAD_REG(group), 1);
    REG_SET_BIT(TIMG_T0CONFIG_REG(group), TIMG_T0_EN);
    esp_rom_delay_us(200);
    on = timg_t0_read(group, &ok_on);
    REG_WRITE(TIMG_REGCLK_REG(group), regclk & ~TIMG_CLK_EN);
    esp_rom_delay_us(200);
    off = timg_t0_read(group, &ok_off);
    REG_WRITE(TIMG_REGCLK_REG(group), regclk | TIMG_CLK_EN);
    back = timg_t0_read(group, &ok_back);
    esp_rom_delay_us(200);
    later = timg_t0_read(group, &ok_later);
    REG_WRITE(TIMG_T0CONFIG_REG(group), cfg0);
    REG_WRITE(TIMG_REGCLK_REG(group), regclk);
    portEXIT_CRITICAL(&s_mux);
    printf("GATE|timg%d_regclk_count|row=%s|step_us=200|on=%" PRIu32 "|off=%" PRIu32 "|back=%" PRIu32
           "|later=%" PRIu32 "|latched=%d%d%d%d\n",
           group, row, on, off, back, later, ok_on, ok_off, ok_back, ok_later);
    if (REG_READ(TIMG_REGCLK_REG(group)) != regclk) {
        fail("timg_regclk_count", "TIMG_REGCLK did not return to its boot value");
    }
}

// The one-shot conversion's time (specs/blocks/saradc.toml APB_SARADC_CTRL, APB_ADC_CLKM_CONF and
// FSM_WAIT: "a conversion takes zero virtual time"): 64 reads of ADC1 channel 0 through the
// driver, set up as adc_idle sets it up, timed by the cycle counter and esp_timer. The codes are
// not printed; `ok` counts the reads that returned ESP_OK.
static void adc_conversion_time(void)
{
    adc_oneshot_unit_handle_t unit = NULL;
    adc_oneshot_unit_init_cfg_t unit_cfg = {
        .unit_id = ADC_UNIT_1,
    };
    adc_oneshot_chan_cfg_t chan_cfg = {
        .atten = ADC_ATTEN_DB_12,
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    if (adc_oneshot_new_unit(&unit_cfg, &unit) != ESP_OK ||
        adc_oneshot_config_channel(unit, ADC_CHANNEL_0, &chan_cfg) != ESP_OK) {
        fail("adc", "ADC1 one-shot setup failed");
        return;
    }
    int raw = 0;
    int ok = 0;
    (void)adc_oneshot_read(unit, ADC_CHANNEL_0, &raw);
    int64_t t0 = esp_timer_get_time();
    uint32_t c0 = esp_cpu_get_cycle_count();
    for (int i = 0; i < 64; i++) {
        ok += adc_oneshot_read(unit, ADC_CHANNEL_0, &raw) == ESP_OK;
    }
    uint32_t c1 = esp_cpu_get_cycle_count();
    int64_t t1 = esp_timer_get_time();
    adc_oneshot_del_unit(unit);
    printf("TIME|adc_oneshot_read|row=saradc.APB_SARADC_CTRL,saradc.APB_SARADC_APB_ADC_CLKM_CONF,"
           "saradc.APB_SARADC_FSM_WAIT|reads=64|us=%lld|cycles=%" PRIu32 "|ok=%d\n",
           (long long)(t1 - t0), c1 - c0, ok);
}

// Campaign step 4's gate steps, under the RTC marker of gate_steps (steps 5 to 7), after every
// earlier line: a reset in one of them is reported by the next boot, which skips every gate step.
static void gate_steps_b(void)
{
    if (s_gates_skipped) {
        return;
    }
    s_gate_magic = GATE_MAGIC;
    s_gate_step = 5;
    gate_blocks();
    s_gate_step = 6;
    gate_timg_count(0, "timg0.TIMG_REGCLK");
    s_gate_step = 7;
    gate_timg_count(1, "timg1.TIMG_REGCLK");
    s_gate_step = 0;
    s_gate_magic = 0;
}

// Waits until the USB host is connected and has had time to reopen its port; the same wait as
// probe_campaign_reset's, whose comment gives the reasons: IDF's connection monitor reports
// "connected" while the host sends SOF, starts out reporting it and is read only after three
// ticks; a connected host gets HOST_SETTLE_MS; the wait gives up after HOST_WAIT_MS.
static bool wait_for_host(int *waited_ms, int *settle_ms)
{
    vTaskDelay(pdMS_TO_TICKS(HOST_PRIME_MS));
    int waited = HOST_PRIME_MS;
    while (!usb_serial_jtag_is_connected() && waited < HOST_WAIT_MS) {
        vTaskDelay(pdMS_TO_TICKS(HOST_POLL_MS));
        waited += HOST_POLL_MS;
    }
    bool connected = usb_serial_jtag_is_connected();
    *waited_ms = waited;
    *settle_ms = 0;
    if (connected) {
        vTaskDelay(pdMS_TO_TICKS(HOST_SETTLE_MS));
        *settle_ms = HOST_SETTLE_MS;
    }
    return connected;
}

void app_main(void)
{
    // A boot after a gate step's reset waits for the host before its first line (header).
    bool after_reset = s_gate_magic == GATE_MAGIC && s_gate_step != 0;
    int waited_ms = 0;
    int settle_ms = 0;
    bool host = after_reset && wait_for_host(&waited_ms, &settle_ms);
    PROBE_BEGIN(PROBE_NAME);
    if (after_reset) {
        printf("WAIT|after_gate_reset|row=usj.host_link|host=%s|waited_ms=%d|settle_ms=%d\n",
               host ? "connected" : "not_connected_gave_up", waited_ms, settle_ms);
    }
    read_boot_values();
    read_iomux();
    read_rnd();
    gate_steps();
    mask_of("system.SYSTEM_BT_LPCK_DIV_INT", "system.SYSTEM_BT_LPCK_DIV_INT",
            DR_REG_SYSTEM_BASE + 0x020);
    mask_of("system.SYSTEM_BT_LPCK_DIV_FRAC", "system.SYSTEM_BT_LPCK_DIV_FRAC",
            DR_REG_SYSTEM_BASE + 0x024);
    flash_beyond_the_part();
    adc_idle();
    fram_num_width();
    adc_presses();
    // Campaign step 4, after every line above.
    gate_steps_b();
    adc_conversion_time();
    PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
}
