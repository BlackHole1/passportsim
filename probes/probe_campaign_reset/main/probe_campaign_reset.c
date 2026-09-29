// probe_campaign_reset: the super-watchdog and deep-sleep-wake facts of the silicon evidence
// campaign, step 1 (specs/notes/silicon-campaign.md). MIT. An ordinary ESP-IDF v5.5.3 app.
//
// Three boots, holding the place in RTC memory and, where a reset may clear that memory, in the
// raw reset cause:
//
//   1. SWD: the RTC super watchdog's auto-feed is switched off and the watchdog is not fed
//      after that (specs/blocks/rtc_cntl.toml RTC_CNTL_SWD*). The write that switches auto-feed
//      off also sets SWD_FEED once, so the count starts at 0 there: with auto-feed on, the
//      watchdog is fed at its feed interrupt, about 100 ms before its timeout (TRM 12.3), so the
//      count's phase at the switch is otherwise unknown. The boot then busy-waits in 1 ms steps,
//      storing the time since that write in RTC memory after every step, for up to 8 s. The
//      boot after the reset prints the last stored time (`SWD|timeout`): the
//      timeout lies between it and it plus the longest gap between two stores, which the boot
//      also stored (a step of 1 ms, and a one-tick yield every 100 steps so the idle task runs).
//      If no reset comes, it prints `SWD|no_reset`, switches auto-feed back on and goes on to
//      step 2 in the same boot.
//   2. The boot after the super-watchdog reset (raw cause 0x12, RESET_REASON_SYS_SUPER_WDT, which
//      "resets the digital core and rtc module", so the step is recognised by the cause even if
//      RTC memory did not survive) prints the cause, whether RTC memory survived and SWD_CONF.
//      Then deep sleep for 3 s, woken by the timer, with GPIO0 (the button ladder) armed as a
//      low-level deep-sleep wake source as well: specs/blocks/rtc_cntl.toml RTC_CNTL_GPIO_WAKEUP.
//      With no button pressed the timer wakes the chip; if the operator presses a button during the
//      3 s the capture shows whether the ladder level wakes it (the row's "which buttons wake the
//      device is UNVERIFIED").
//   3. The boot after the deep sleep prints the wake cause, the GPIO wake mask, the raw
//      RTC_CNTL_GPIO_WAKEUP word, and the retention registers the sleep path writes
//      (RTC_CNTL_DIG_ISO, RTC_CNTL_PWC, RTC_CNTL_DIG_PAD_HOLD), then DONE, and clears its state
//      so a later reset starts the sequence again.
//
// Every boot first waits for the USB host (wait_for_host below) and only then prints its first
// line. The USB Serial/JTAG link drops at the super-watchdog reset and for the deep sleep, and the
// host's capture reopens its port a few hundred ms after the link is back; lines printed before
// that are lost on the device (capture device-probe_campaign_reset-20260924T160245Z-run1: boot1
// printed, then `[capture: link down 283 ms]`, then nothing). The facts a wait could move (the
// reset cause, the RTC time counter) are read at the start of app_main, before the wait.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   WAIT    one line per boot, labelled like its BOOT line: whether the IDF connection monitor
//           reported the host connected, how long this boot waited for it and the settle after it
//   BOOT    one line per boot, labelled boot1, boot2, ...: the step, the IDF reset reason and the
//           raw RTC_CNTL cause, whether this probe's RTC memory held its magic, and the RTC time
//           counter in milliseconds (row rtc_cntl.reset_domains: does a SYS_ reset restart it)
//   SWD     armed | reset | no_reset, with SWD_CONF; timeout, the time from the arming write to
//           the last store before the reset (alive_us) and the longest gap between two stores
//   SLEEP   the wake sources armed before the deep sleep
//   WAKE    the wake cause and the GPIO wake facts after it
//   REG     RTC_CNTL words after the deep-sleep wake
//   TIMEBASE one line per boot and one before the deep sleep: at app_main, the
//           esp_timer time (SYSTIMER, which a reset and the deep sleep restart: the time from the
//           reset or the wake to app_main), the RTC counter converted at the current calibration
//           (the time since the counter last restarted) and IDF's RTC time in microseconds (the
//           rtc_time_ms of BOOT, unrounded). The last two differ by the time from the counter's
//           restart to the first esp_rtc_get_time_us call that found STORE1 at 0 (esp_clk_init)
//           after a SYS_ reset, and are equal if IDF's time counts from the restart. The sleep
//           line gives both RTC times when the SLEEP line is printed, which splits the time
//           from boot2's app_main to boot3's into the part before the deep sleep and the rest.
//   REG     (campaign step 4) the RTC_CNTL words probe_campaign_regs reads (its reg_facts.h),
//           read at app_main of boot2, after the super-watchdog's SYS_ reset, and of boot3, after
//           the deep-sleep wake, before the host wait; kept in RTC memory and printed after every
//           other line of the last boot, labelled .boot2 and .boot3. probe_campaign_regs's capture
//           read them after a core reset that followed this probe's deep sleep, which keeps the RTC
//           domain, so TIMER1, CLK_CONF and GPIO_WAKEUP there are that sleep's residue (the
//           inventory sweep, specs/notes/silicon-campaign.md "Step 4"): boot2 reads them after a
//           reset that restores the RTC domain, boot3 after the sleep that leaves the residue.
//
// Safety, checked for every step (silicon campaign step 1):
//   - no flash write or erase and no NVS; the partition table is the device's own
//     (partitions.csv), so nothing falls in or reads cardid [0x356000, 0x35A000);
//   - no eFuse access beyond what ESP-IDF's normal boot reads;
//   - no radio;
//   - the only deep sleep is woken by the timer after 3 s whatever else happens; GPIO0 is an
//     additional wake source the operator may use by pressing a button, never a required one;
//   - the super-watchdog reset is a chip reset like the RTC watchdog's of probe_reset: the ROM
//     boots the same image again; a bound of 6 boots stops the sequence whatever the reset causes
//     say, and step 1 gives up after 8 s and restores auto-feed if no reset comes;
//   - bounded: at most about 20 s from the first boot to the DONE line with the host attached (the
//     8 s SWD bound, the 3 s sleep, and the host wait of about 1.5 s on each of three boots); with
//     no host, each boot's wait gives up after 5 s and prints anyway, about 30 s in all.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

#include "driver/usb_serial_jtag.h"
#include "esp_attr.h"
#include "esp_private/esp_clk.h"
#include "esp_rom_sys.h"
#include "esp_sleep.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "soc/rtc.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_campaign_reset"

// The key that unlocks the super-watchdog registers (specs/c3-registers.csv RTC_CNTL_SWD_WKEY
// reset value; IDF bootloader_super_wdt_auto_feed writes the same).
#define SWD_KEY 0x8F1D312Au
#define RAW_SUPER_WDT 0x12u
#define RAW_DEEP_SLEEP 0x05u
#define SWD_WAIT_MS 8000
// The SWD wait's store step, and how many steps run between two one-tick yields.
#define SWD_STEP_US 1000
#define SWD_STEPS_PER_YIELD 100
#define DEEP_SLEEP_US 3000000ULL
#define MAX_BOOTS 6

// The host wait of every boot (wait_for_host).
#define HOST_PRIME_MS 30
#define HOST_POLL_MS 10
#define HOST_WAIT_MS 5000
#define HOST_SETTLE_MS 1500

enum step { STEP_NONE = 0, STEP_SWD_ARMED = 1, STEP_SLEEPING = 2 };

#define STATE_MAGIC 0x43525331u // "CRS1"
RTC_NOINIT_ATTR static uint32_t s_magic;
RTC_NOINIT_ATTR static uint32_t s_step;
RTC_NOINIT_ATTR static uint32_t s_boots;
// The SWD wait's stores (step 1): the time from the arming write to the last store, and the
// longest gap between two stores, both in microseconds of esp_timer.
RTC_NOINIT_ATTR static uint32_t s_swd_alive_us;
RTC_NOINIT_ATTR static uint32_t s_swd_max_gap_us;

// Campaign step 4: the RTC_CNTL words of probe_campaign_regs's reg_facts.h, with their rows
// there and the reset-domain row the boot2 read bears on (module comment, REG).
#define RTC_WORDS 19
static const struct {
    const char *fact;
    const char *row;
    uint32_t addr;
} RTC_WORD[RTC_WORDS] = {
    {"rtc_cntl.RTC_CNTL_TIMER1", "rtc_cntl.RTC_CNTL_TIMER*", 0x6000801Cu},
    {"rtc_cntl.RTC_CNTL_TIMER2", "rtc_cntl.RTC_CNTL_TIMER*", 0x60008020u},
    {"rtc_cntl.RTC_CNTL_TIMER3", "rtc_cntl.RTC_CNTL_TIMER*", 0x60008024u},
    {"rtc_cntl.RTC_CNTL_TIMER4", "rtc_cntl.RTC_CNTL_TIMER*", 0x60008028u},
    {"rtc_cntl.RTC_CNTL_TIMER5", "rtc_cntl.RTC_CNTL_TIMER*", 0x6000802Cu},
    {"rtc_cntl.RTC_CNTL_TIMER6", "rtc_cntl.RTC_CNTL_TIMER*", 0x60008030u},
    {"rtc_cntl.RTC_CNTL_ANA_CONF", "rtc_cntl.RTC_CNTL_ANA_CONF", 0x60008034u},
    {"rtc_cntl.RTC_CNTL_CLK_CONF", "rtc_cntl.RTC_CNTL_CLK_CONF", 0x60008070u},
    {"rtc_cntl.RTC_CNTL_SLOW_CLK_CONF", "rtc_cntl.RTC_CNTL_SLOW_CLK_CONF", 0x60008074u},
    {"rtc_cntl.RTC_CNTL", "rtc_cntl.RTC_CNTL", 0x60008080u},
    {"rtc_cntl.RTC_CNTL_PWC", "rtc_cntl.RTC_CNTL_PWC", 0x60008084u},
    {"rtc_cntl.RTC_CNTL_DIG_ISO", "rtc_cntl.RTC_CNTL_DIG_ISO", 0x6000808Cu},
    {"rtc_cntl.RTC_CNTL_SWD_CONF", "rtc_cntl.RTC_CNTL_SWD*", 0x600080ACu},
    {"rtc_cntl.RTC_CNTL_SWD_WPROTECT", "rtc_cntl.RTC_CNTL_SWD*", 0x600080B0u},
    {"rtc_cntl.RTC_CNTL_DIG_PAD_HOLD", "rtc_cntl.RTC_CNTL_DIG_PAD_HOLD", 0x600080D4u},
    {"rtc_cntl.RTC_CNTL_BROWN_OUT", "rtc_cntl.RTC_CNTL_BROWN_OUT", 0x600080D8u},
    {"rtc_cntl.RTC_CNTL_FIB_SEL", "rtc_cntl.RTC_CNTL_FIB_SEL", 0x6000810Cu},
    {"rtc_cntl.RTC_CNTL_GPIO_WAKEUP", "rtc_cntl.RTC_CNTL_GPIO_WAKEUP", 0x60008110u},
    {"rtc_cntl.RTC_CNTL_SENSOR_CTRL", "rtc_cntl.RTC_CNTL_SENSOR_CTRL", 0x6000811Cu},
};
// The words read at boot2 ([0]) and boot3 ([1]), and which of the two were read by this sequence
// (bit 0, bit 1), cleared on the sequence's first boot.
RTC_NOINIT_ATTR static uint32_t s_rtc_words[2][RTC_WORDS];
RTC_NOINIT_ATTR static uint32_t s_rtc_words_read;

// Stores the RTC words at app_main of the boot after the super-watchdog reset or after the deep
// sleep, which `step` names (the other boots store nothing).
static void rtc_words_store(uint32_t step)
{
    int slot = step == STEP_SWD_ARMED ? 0 : step == STEP_SLEEPING ? 1 : -1;
    if (slot < 0) {
        return;
    }
    for (int i = 0; i < RTC_WORDS; i++) {
        s_rtc_words[slot][i] = REG_READ(RTC_WORD[i].addr);
    }
    s_rtc_words_read |= 1u << slot;
}

// Prints the stored words, boot2's then boot3's; a boot whose words were not stored prints
// nothing for them (the lines are then `not printed` in the comparison).
static void rtc_words_print(void)
{
    for (int slot = 0; slot < 2; slot++) {
        if ((s_rtc_words_read & (1u << slot)) == 0) {
            continue;
        }
        for (int i = 0; i < RTC_WORDS; i++) {
            printf("REG|%s.boot%d|row=%s,rtc_cntl.reset_domains|addr=0x%08" PRIx32 "|val=0x%08" PRIx32
                   "\n",
                   RTC_WORD[i].fact, slot + 2, RTC_WORD[i].row, RTC_WORD[i].addr,
                   s_rtc_words[slot][i]);
        }
    }
}

static bool s_ok = true;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

static uint32_t raw_reset_cause(void)
{
    return (REG_READ(RTC_CNTL_RESET_STATE_REG) & RTC_CNTL_RESET_CAUSE_PROCPU_M) >>
           RTC_CNTL_RESET_CAUSE_PROCPU_S;
}

// The RTC counter in microseconds at the current calibration, the product split as IDF's
// esp_rtc_get_time_us splits it (esp_hw_support/esp_clk.c), so it does not overflow.
static uint64_t rtc_counter_us(void)
{
    uint64_t ticks = rtc_time_get();
    uint64_t cal = esp_clk_slowclk_cal_get();
    return (((ticks & UINT32_MAX) * cal) >> RTC_CLK_CAL_FRACT) +
           (((ticks >> 32) * cal) << (32 - RTC_CLK_CAL_FRACT));
}

static void drain_console(void)
{
    fflush(stdout);
    vTaskDelay(pdMS_TO_TICKS(20));
}

static void finish(void)
{
    s_magic = 0;
    s_step = STEP_NONE;
    s_boots = 0;
    PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
}

// Waits, before a boot's first line, until the USB host is connected and has had time to reopen
// its port. Returns whether the host was seen; *waited_ms is the time until then (or until the
// wait gave up) and *settle_ms the settle after it.
//
// usb_serial_jtag_is_connected() is IDF's connection monitor (esp_driver_usb_serial_jtag,
// usb_serial_jtag_connection_monitor.c): a FreeRTOS tick hook that reads and clears the SOF raw
// interrupt bit, so "connected" means the host is sending SOF packets, whether or not a program
// has the serial port open (driver/usb_serial_jtag.h). The monitor starts out reporting connected
// and changes its answer only at a tick, and the first tick may still see an SOF latched before
// the scheduler started, so its answer is read only after HOST_PRIME_MS (three ticks at 100 Hz).
// SOF says the link is enumerated, not that the capture has reopened the port, so a connected
// host is then given HOST_SETTLE_MS. The wait gives up after HOST_WAIT_MS and the boot prints
// anyway, which its WAIT line says.
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

static void swd_auto_feed(bool on)
{
    REG_WRITE(RTC_CNTL_SWD_WPROTECT_REG, SWD_KEY);
    if (on) {
        REG_SET_BIT(RTC_CNTL_SWD_CONF_REG, RTC_CNTL_SWD_AUTO_FEED_EN);
    } else {
        // One write: auto-feed off and one feed, so the count starts at 0 here (step 1).
        uint32_t conf = REG_READ(RTC_CNTL_SWD_CONF_REG);
        REG_WRITE(RTC_CNTL_SWD_CONF_REG, (conf & ~RTC_CNTL_SWD_AUTO_FEED_EN) | RTC_CNTL_SWD_FEED);
    }
    REG_WRITE(RTC_CNTL_SWD_WPROTECT_REG, 0);
}

static void deep_sleep_step(void)
{
    s_magic = STATE_MAGIC;
    s_step = STEP_SLEEPING;
    esp_err_t gpio_rc = esp_deep_sleep_enable_gpio_wakeup(BIT(0), ESP_GPIO_WAKEUP_GPIO_LOW);
    esp_err_t timer_rc = esp_sleep_enable_timer_wakeup(DEEP_SLEEP_US);
    printf("TIMEBASE|sleep|row=rtc_cntl.reset_domains|timer_us=%lld|rtc_counter_us=%llu"
           "|rtc_time_us=%llu\n",
           (long long)esp_timer_get_time(), (unsigned long long)rtc_counter_us(),
           (unsigned long long)esp_clk_rtc_time());
    printf("SLEEP|armed|row=rtc_cntl.RTC_CNTL_GPIO_WAKEUP|gpio_mask=0x1|gpio_level=low|gpio_rc=%d"
           "|timer_us=%llu|timer_rc=%d\n",
           gpio_rc, DEEP_SLEEP_US, timer_rc);
    drain_console();
    esp_deep_sleep_start();
}

static void swd_step(void)
{
    s_magic = STATE_MAGIC;
    s_step = STEP_SWD_ARMED;
    s_swd_alive_us = 0;
    s_swd_max_gap_us = 0;
    swd_auto_feed(false);
    int64_t armed_at = esp_timer_get_time();
    printf("SWD|armed|row=rtc_cntl.RTC_CNTL_SWD*|swd_conf=0x%08" PRIx32 "|wait_ms=%d\n",
           REG_READ(RTC_CNTL_SWD_CONF_REG), SWD_WAIT_MS);
    drain_console();
    // The stores of step 1, until the reset or SWD_WAIT_MS after the arming write.
    int64_t last = esp_timer_get_time();
    s_swd_alive_us = (uint32_t)(last - armed_at);
    for (int steps = 1; last - armed_at < (int64_t)SWD_WAIT_MS * 1000; steps++) {
        esp_rom_delay_us(SWD_STEP_US);
        if (steps % SWD_STEPS_PER_YIELD == 0) {
            vTaskDelay(1);
        }
        int64_t now = esp_timer_get_time();
        if ((uint32_t)(now - last) > s_swd_max_gap_us) {
            s_swd_max_gap_us = (uint32_t)(now - last);
        }
        s_swd_alive_us = (uint32_t)(now - armed_at);
        last = now;
    }
    uint32_t conf = REG_READ(RTC_CNTL_SWD_CONF_REG);
    swd_auto_feed(true);
    printf("SWD|no_reset|row=rtc_cntl.RTC_CNTL_SWD*|waited_ms=%d|swd_conf=0x%08" PRIx32 "\n",
           SWD_WAIT_MS, conf);
    deep_sleep_step();
}

void app_main(void)
{
    // Read before the host wait, so the wait moves none of the facts printed below.
    int64_t timer_us = esp_timer_get_time();
    uint64_t counter_us = rtc_counter_us();
    uint64_t rtc_time_us = esp_clk_rtc_time();
    uint32_t rtc_time_ms = (uint32_t)(rtc_time_us / 1000);
    bool known = s_magic == STATE_MAGIC;
    uint32_t raw = raw_reset_cause();
    if (!known) {
        s_step = STEP_NONE;
        s_boots = 0;
    }
    s_boots++;
    // The super-watchdog reset also resets the RTC module, so its boot is recognised by the
    // cause even when this probe's RTC memory did not survive.
    uint32_t step = raw == RAW_SUPER_WDT ? STEP_SWD_ARMED : s_step;
    // Campaign step 4: the RTC words of this boot, before the host wait (module comment, REG).
    if (!known) {
        s_rtc_words_read = 0;
    }
    rtc_words_store(step);

    int waited_ms = 0;
    int settle_ms = 0;
    bool host = wait_for_host(&waited_ms, &settle_ms);
    PROBE_BEGIN(PROBE_NAME);
    printf("WAIT|boot%" PRIu32 "|row=usj.host_link|host=%s|waited_ms=%d|settle_ms=%d\n", s_boots,
           host ? "connected" : "not_connected_gave_up", waited_ms, settle_ms);
    // The RTC time counter in microseconds (specs/blocks/rtc_cntl.toml reset_domains: whether it
    // restarts at 0 at a SYS_ reset is UNVERIFIED). A value near the time this boot took says it
    // restarted; one that also holds the time before the reset says it did not.
    printf("BOOT|boot%" PRIu32 "|row=rtc_cntl.RTC_CNTL_SWD*,rtc_cntl.RTC_CNTL_GPIO_WAKEUP,"
           "rtc_cntl.reset_domains|step=%" PRIu32 "|reason=%d|raw=0x%02" PRIx32 "|rtc_magic=%d"
           "|rtc_time_ms=%" PRIu32 "\n",
           s_boots, step, (int)esp_reset_reason(), raw, known, rtc_time_ms);
    // Where the boot's RTC time starts (module comment, TIMEBASE).
    printf("TIMEBASE|boot%" PRIu32 "|row=rtc_cntl.reset_domains|timer_us=%lld|rtc_counter_us=%llu"
           "|rtc_time_us=%llu\n",
           s_boots, (long long)timer_us, (unsigned long long)counter_us,
           (unsigned long long)rtc_time_us);
    if (s_boots > MAX_BOOTS) {
        swd_auto_feed(true);
        fail("boots", "the sequence took more boots than it has steps");
        finish();
        return;
    }

    switch (step) {
    case STEP_SWD_ARMED: {
        uint32_t conf = REG_READ(RTC_CNTL_SWD_CONF_REG);
        printf("SWD|reset|row=rtc_cntl.RTC_CNTL_SWD*|raw=0x%02" PRIx32 "|reason=%d|rtc_magic=%d"
               "|swd_conf=0x%08" PRIx32 "\n",
               raw, (int)esp_reset_reason(), known, conf);
        // The timeout is after alive_us and at most max_gap_us after it (step 1).
        printf("SWD|timeout|row=rtc_cntl.RTC_CNTL_SWD*|alive_us=%" PRIu32 "|max_gap_us=%" PRIu32
               "\n",
               known ? s_swd_alive_us : 0, known ? s_swd_max_gap_us : 0);
        // Clear the reset flag the watchdog left, as IDF's own handling does.
        REG_WRITE(RTC_CNTL_SWD_WPROTECT_REG, SWD_KEY);
        REG_SET_BIT(RTC_CNTL_SWD_CONF_REG, RTC_CNTL_SWD_RST_FLAG_CLR);
        REG_WRITE(RTC_CNTL_SWD_WPROTECT_REG, 0);
        if (raw != RAW_SUPER_WDT) {
            fail("swd_reset", "the boot after the SWD step was not a super-watchdog reset");
        }
        deep_sleep_step();
        return;
    }
    case STEP_SLEEPING: {
        esp_sleep_wakeup_cause_t cause = esp_sleep_get_wakeup_cause();
        printf("WAKE|deep_sleep|row=rtc_cntl.RTC_CNTL_GPIO_WAKEUP|raw=0x%02" PRIx32 "|cause=%d"
               "|gpio_status=0x%llx|gpio_wakeup=0x%08" PRIx32 "\n",
               raw, (int)cause, (unsigned long long)esp_sleep_get_gpio_wakeup_status(),
               REG_READ(RTC_CNTL_GPIO_WAKEUP_REG));
        printf("REG|rtc_cntl.RTC_CNTL_DIG_ISO.after_deep_sleep|row=rtc_cntl.RTC_CNTL_DIG_ISO"
               "|addr=0x%08" PRIx32 "|val=0x%08" PRIx32 "\n",
               (uint32_t)RTC_CNTL_DIG_ISO_REG, REG_READ(RTC_CNTL_DIG_ISO_REG));
        printf("REG|rtc_cntl.RTC_CNTL_PWC.after_deep_sleep|row=rtc_cntl.RTC_CNTL_PWC|addr=0x%08" PRIx32
               "|val=0x%08" PRIx32 "\n",
               (uint32_t)RTC_CNTL_PWC_REG, REG_READ(RTC_CNTL_PWC_REG));
        printf("REG|rtc_cntl.RTC_CNTL_DIG_PAD_HOLD.after_deep_sleep|row=rtc_cntl.RTC_CNTL_DIG_PAD_HOLD"
               "|addr=0x%08" PRIx32 "|val=0x%08" PRIx32 "\n",
               (uint32_t)RTC_CNTL_DIG_PAD_HOLD_REG, REG_READ(RTC_CNTL_DIG_PAD_HOLD_REG));
        if (raw != RAW_DEEP_SLEEP) {
            fail("deep_sleep", "the boot after the deep sleep was not a DEEPSLEEP reset");
        }
        // Campaign step 4, after every other line of the sequence.
        rtc_words_print();
        finish();
        return;
    }
    default:
        swd_step();
        return;
    }
}
