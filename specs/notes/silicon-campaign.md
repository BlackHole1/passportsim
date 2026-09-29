# Silicon evidence campaign: inventory (step 1, swept in step 4, Part B rows settled in step 5)

Before the campaign the model left `specs/` with 115 class C and 14 class U block rows (A 29,
B 314), class C constants in `specs/timing-profiles.toml`, UNVERIFIED behaviours in
`specs/oracle-known-diffs.toml`, class C and UNVERIFIED values in the HLE bindings, and UNVERIFIED
items inside class A and B rows.
This note lists every one of them with what it assumes, which corpus firmware reaches it and a
disposition, and it maps each **probe** row to the probe line that settles it.

Step 1 changes no model and no class. Step 2 is the device captures of the four campaign probes;
step 3 fixes models and promotes classes from them. The boot fit (the boot phase fit), the anchor
check (the validation anchors) and the delta check (the `probe_timing` deltas) are checks of
`tests/milestones/m11.rs`; the `pk` boot is the device-golden boot of `m8.rs`.

## Dispositions and counts

- **probe**: a device probe observes it safely (a register value read, a read-modify-write
  behaviour, a bit's effect, a timed operation). The probe and its line are named.
- **cannot**: silicon cannot show it safely or at all (a write to the battery gauge or the flash,
  cardid, anything irreversible, anything needing lab equipment or the opened board, or not a
  property of silicon at all); the reason is given.
- **untouched**: no corpus firmware reaches it, with how that was established.

Step 4 ("Step 4: the inventory sweep" below) replaced every **probe** with one of:

- **settled**: a device capture line answers the row's probe question and the committed emulator
  record prints the device's value; the cell cites the line (`<capture>:<line> <TAG|fact>`) and
  gives the row's class after it: **A** or **B** when the row's claim is the silicon fact, **C
  kept** when the capture settles what the guest reads and the row's C is an effect the model omits
  by declaration, which the cell names (with **cannot** or "not a corpus path" for the effect).
- **open**: a claim of the row a probe can still show is in no capture; the cell names the Part B
  line that asks for it.

Step 1:

| Section | Items | probe | cannot | untouched |
|---|---|---|---|---|
| 1. Class C and U block rows (`specs/blocks/*.toml`) | 129 | 99 | 5 | 25 |
| 2. Class C timing constants (`specs/timing-profiles.toml`) | 10 | 6 | 4 | 0 |
| 3. UNVERIFIED known diffs (`specs/oracle-known-diffs.toml`) | 2 | 0 | 2 | 0 |
| 4. Class C and UNVERIFIED HLE values (`specs/hle/idf-5.5.3/`) | 9 | 5 | 4 | 0 |
| 5. UNVERIFIED items inside class A and B block rows | 59 | 6 | 36 | 17 |
| 6. Other spec files (`st7789-boot.toml`, `oracle-qemu-regions.toml`) | 3 | 0 | 3 | 0 |
| **Total** | **212** | **116** | **54** | **42** |

Step 4 (section 2 counts T11, the row step 3 added):

| Section | Items | settled A | settled B | settled, C kept | open | cannot | untouched |
|---|---|---|---|---|---|---|---|
| 1. Class C and U block rows | 129 | 4 | 2 | 83 | 10 | 5 | 25 |
| 2. Class C timing constants | 11 | 5 | 2 | 0 | 0 | 4 | 0 |
| 3. UNVERIFIED known diffs | 2 | 0 | 0 | 0 | 0 | 2 | 0 |
| 4. Class C and UNVERIFIED HLE values | 9 | 3 | 1 | 0 | 1 | 4 | 0 |
| 5. UNVERIFIED items inside class A and B block rows | 59 | 2 | 4 | 0 | 0 | 36 | 17 |
| 6. Other spec files | 3 | 0 | 0 | 0 | 0 | 3 | 0 |
| **Total** | **213** | **14** | **9** | **83** | **11** | **54** | **42** |

Step 5 ("Step 5: the Part B rows" below; the last count table, which
`t1_campaign_every_settled_row_names_a_capture_line` checks):

| Section | Items | settled A | settled B | settled, C kept | open | cannot | untouched |
|---|---|---|---|---|---|---|---|
| 1. Class C and U block rows | 129 | 6 | 5 | 88 | 0 | 5 | 25 |
| 2. Class C timing constants | 11 | 5 | 2 | 0 | 0 | 4 | 0 |
| 3. UNVERIFIED known diffs | 2 | 0 | 0 | 0 | 0 | 2 | 0 |
| 4. Class C and UNVERIFIED HLE values | 9 | 3 | 1 | 0 | 1 | 4 | 0 |
| 5. UNVERIFIED items inside class A and B block rows | 59 | 2 | 4 | 0 | 0 | 36 | 17 |
| 6. Other spec files | 3 | 0 | 0 | 0 | 0 | 3 | 0 |
| **Total** | **213** | **16** | **12** | **88** | **1** | **54** | **42** |

The appendix groups the 321 field access types of `specs/c3-registers.csv` whose `access_basis`
is UNVERIFIED; they are not counted above.

## Method: how "touched by" was read

"Touched by" is the first-touch ledger of emulator runs, never a guess.
`campaign_first_touch_ledger` in `tests/milestones/campaign.rs` (ignored, run by hand) runs every
image from power-on and prints one `TOUCH|<image>|<block>|<register>|<offset>|<R or W>` line per
register the run reached, the access being the first one:

```text
PASSPORTSIM_DATA_ROOT=<data root> cargo test --profile ci-test -p pemu-milestones \
  --test campaign -- --ignored --nocapture campaign_first_touch_ledger
```

- **Images, 28**: the corpus images `pk`, `official`, `demo`, `goldminer`, `probe-long`, `probe2`,
  `pkgatt` and `scan3`, and the merged image of every probe under the data root's
  `corpus/probes/` as it stood before the campaign probes (`flash_stress`, `hle_probe`, `pkgatt`,
  `probe_boot_facts`, `probe_clocks`, `probe_crypto`, `probe_deadlock`, `probe_intc`,
  `probe_limits`, `probe_panic`, `probe_reset`, `probe_stack`, `probe_timing`, `probe_wdt`,
  `probe_wifi_assoc`, `probe_wifi_conn`, `probe_wifi_http`, `scan3`, `sleep_timer`,
  `usj_echo`). "all 28" in the tables means every one of them.
- **Budget**: 15 s of virtual time under the `fast` profile with the HLE bound as the corpus
  suites bind it. A probe stops at its `DONE|` line; a guest panic is run through (four probes
  panic and reboot on purpose).
- **Input**: the four product images get the button walk Down, Down, Ok, Ok, Up (from 1.5 s),
  which in `official` opens the Audio demo, plays the tone, records and plays back, so the I2S,
  GDMA and codec paths are in its ledger.
- **Limits.** `probe-long` stops at 2.3 s at a radio tripwire (`radio_fe 0x60006110`), and
  `probes/probe_wifi_conn` and `probes/scan3` stop at the Wi-Fi `DisabledFeature` tripwire, so
  their later paths are not in the ledger. The ledger covers memory-mapped registers only: flash
  commands, the CW2017 gauge's I2C registers and HLE values have no ledger record, and their
  rows say where their evidence comes from instead. A path none of the 28 runs reaches in 15 s
  (a longer button walk, pairing, an OTA) is outside it.

## The campaign probes

Four ESP-IDF v5.5.3 apps under `probes/`, built by `cargo xtask probes` and pinned in
`tests/fw/manifest.toml`. Each is built with the device's own partition table
(`partitions.csv`, a copy of `probes/probe_wifi_conn/partitions.csv`), so `plan_flash` accepts the
merged image, and each header states the safety rules below, checked step by step.

| Probe | Rows | Device run time | What it does |
|---|---|---|---|
| `probe_campaign_regs` | B1 to B4, B8 to B19, B21, B24 to B26, B45, B53, B57 to B70, B72 to B82, B85 to B118, B120 to B123, B125 (86 block rows); V21, V37, V57 | about 14 s after boot (4.2 s of frame-number sampling, an 8 s button window) | `REG` boot value of every register of every class C row a corpus image reaches (300 words, and the two TIMG0 calibration words of section 3: `main/reg_facts.h`), the IO_MUX pad words; `RND`; `GATE` (I2S0, AES, TIMG0, TIMG1 behind their clock and reset bits); `MASK` of the BT low-power divider; `FLASH` one read at 0x800000; `ADC` idle and three presses; `USJ` frame-number width |
| `probe_campaign_timing` | B28 to B38, B55, B56, B60, B61, B120 to B123 (19 block rows); T1 to T4, T9; V49 | about 2 s after boot | `TIME` of cold and warm cache line fills, AES-CBC at four sizes, three RSA exponentiations, the slow-clock next edge, SYSTIMER comparator loads, 16 USJ packets, I2C reads at 100 and 400 kHz, UART0 at two bauds; `MPI` with a right and a wrong M'; `CAL`; `REG` of the I2C0 and UART0 timing registers the drivers set |
| `probe_campaign_radio` | T10; H6 to H9 | about 1 s after boot | the BLE controller brought up and down once with no host stack: `TIME` of init, enable, disable, deinit; `HEAP` around each; `ISR` routing of the seven BT sources; `HCI` Reset and Read Local Version over VHCI; `LOG` the controller's own log lines, the MAC withheld |
| `probe_campaign_reset` | B57, B63 to B66 (5 block rows); V35, V59 | at most about 20 s from the first boot (an 8 s bound on the SWD wait, a 3 s sleep, about 1.5 s of host wait on each boot), three boots | every boot first waits for the USB host (`WAIT`, below); super watchdog with auto-feed off and not fed (a SYS_ reset expected, 8 s bound); deep sleep of 3 s with GPIO0 low armed beside the timer; `BOOT` with the reset cause and the RTC time counter on every boot; `WAKE` and the RTC retention words after the wake |

Safety, the same six rules in every header: no flash write or erase and no NVS (only
`esp_flash_read`; `CONFIG_ESP_PHY_CALIBRATION_AND_DATA_STORAGE=n` in the radio probe); no eFuse
write of any kind; nothing in or reading cardid [0x356000, 0x35A000) (the device's table declares
it and nothing lands there); no sleep but the reset probe's 3 s deep sleep, which the timer ends
whatever else happens (GPIO0 is an additional wake source the operator may use, never a needed one);
no radio TX (the controller is enabled as every BLE corpus image enables it at boot, and only
HCI_Reset and HCI_Read_Local_Version, which transmit nothing, are sent; no Wi-Fi call); every run
bounded well under 60 s and ending with a `DONE|` line. Every register write outside a driver is to
a block no other code of the app uses at that moment, inside a critical section, with the boot
value restored; a gate step that ever stalled the bus would be reported on the next boot by an RTC
marker and skipped.

Every fact line is `TAG|<fact>|row=<row id>[,<row id>]|key=value...`. The row id is the block's
file and the register (`rtc_cntl.RTC_CNTL_SWD*`), `timing-profiles.<name>`, `hle.ble.<name>`, or
for section 5 the id given there (`iomux.reset_values`, `rtc_cntl.reset_domains`).

### Capture procedure

1. Flash each merged image (the data root's `corpus/probes/<probe>-8MB.bin`) through the planner
   and capture the USJ console from reset to the `DONE|` line.
2. `probe_campaign_regs` prints `NOTE|press Up, then Down, then OK, one at a time, within 8 s` near
   its end: press Up, then Down, then OK, each for about a second, one after the other. The
   emulator's record scripts the same three presses.
3. `probe_campaign_reset` resets the chip twice by itself (the super watchdog, then the deep-sleep
   wake), and each reset drops the USB link for a moment. Every boot therefore waits before its
   first line until IDF's connection monitor reports the host (`usb_serial_jtag_is_connected()`,
   which means the host is sending SOF, not that a port is open), then 1.5 s more for the capture
   to reopen its port; it gives up after 5 s and prints anyway. Its `WAIT|boot<n>` line says which
   (`host=connected` or `host=not_connected_gave_up`) and how long it waited. `probe_campaign_regs`
   does the same, with a `WAIT|after_gate_reset` line, only on a boot after a gate step reset the
   chip, which is not expected to happen. Capture with the reconnecting capture
   (`tools/capture_console.py` of the data root) and keep it running across both resets to the
   `DONE|` line; no button is needed.
4. Compare, row by row:

```text
cargo xtask probes compare <device capture> tests/fw/campaign/<probe>.emu.txt
```

The report lists every row as `equal`, `different` (each differing field with both values, and
the device-to-emulator ratio for a decimal number) or `not printed` (which side printed it), and
any `FAIL` line of either run. `t1_campaign_probes_print_the_recorded_emulator_lines` keeps the
records honest: it reruns each pinned image in the emulator under the `device` profile and fails
if the probe lines differ from `tests/fw/campaign/<probe>.emu.txt` (`CAMPAIGN_RECORD=1` renews
them when a model change is meant to move them).

### What the emulator prints

The full records are `tests/fw/campaign/<probe>.emu.txt`. What they say, so the operator knows
where the differences are expected (none of it is changed in this step):

- **`probe_campaign_regs`**: every `REG` word; `RND` 16 distinct values and no zero; I2S0 keeps a
  write made while its reset is held and one made with its clock off (`i2s0_rst_held`
  `read=0x12120012`, `i2s0_clk_off` `read=0x12300030`), AES and both TIMG groups store every write
  whatever their clock and reset bits say; the BT divider masks are `0x00000fff` and `0x1fffffff`;
  the read at 0x800000 returns 32 bytes of 0xFF (`same_as_0x000000=0`); ADC1 channel 0 reads 4095
  idle and exactly 0, 433 and 862 for the scripted Up, Down and OK; the USJ frame number wraps at
  2048 (`max=2041`, 11 bits).
- **`probe_campaign_timing`**: cache cold/warm code 29,646/1,282 cycles, data 116,435/2,975
  cycles, cycles equal to ticks x 10 (the stall is counted); AES-CBC 16/1024/4096/16384 bytes in
  552/54/128/414 us (the first run is the slowest); RSA exponentiation
  1,812 cycles with CONSTANT_TIME 1 and 39 with 0 (rsa_op_ps is 0); a wrong M' gives the
  right answer (`got` equal to `want` for both); SYSTIMER: the first alarm comes one period after
  the PERIOD_MODE write, not after COMP1_LOAD, a PERIOD_MODE write with no load loads the new
  period, and a period of 0 fires (4,554 ticks after the load, then 5 ticks later); 16 USJ lines drain in 4,917 us; 20 I2C reads in
  8,955 us at 100 kHz and 2,788 us at 400 kHz; `uart_wait_tx_done` times out
  (`rc=263`) at both bauds after about 194 ms: no TX_DONE reaches the driver.
- **`probe_campaign_radio`**: init 1,713 us, enable 80,323 us, disable 6 us, deinit 65 us; free
  heap 292,284 before, 282,636 after init (9,648 bytes taken by init), 292,188 after deinit; every
  BT source maps to line 0 at every stage; HCI Reset 15,754
  us and Read Local Version 19,821 us; four `BLE_INIT` lines and one `phy_init` line, pk's text.
  After the BLE rows followed the device capture (step 3): init 3,758
  us, enable 39,338 us, disable 665 us, deinit 1,034 us; free heap and largest block equal to the
  device at every stage (259,088 and 122,880 after init, 259,052 after enable, 258,992 after
  disable, 291,976 and 122,880 after deinit); RWBLE on line 8 after init and after enable, the
  other six at 0; HCI Reset 16,451 us and Read Local Version 19,818 us under U5, then the
  default, which the worker's 20 ms poll decides (520 and 79 us under U4); the log lines
  unchanged. Against both device runs, heap, ISR and log lines are equal; init, disable and
  deinit are within 3 us of both device runs, and enable lies between the two runs (37,542 and
  41,135); the HCI lines differ under U5 only. **U4 then became the default**, and
  the renewed record `tests/fw/campaign/probe_campaign_radio.emu.txt` reads HCI Reset 520 us and
  Read Local Version 79 us, inside the device runs (523 and 516, 76 and 82); init, enable, disable
  and deinit moved by 1 us each (3,757, 39,339, 666 and 1,033), the tick phase of the wake mode,
  and every other line is unchanged. So under the default every line of the radio probe now
  agrees with the device, the four times within 3 us and enable between the runs.
- **`probe_campaign_reset`**: the super watchdog never resets (`SWD|no_reset` after 8 s), so the
  emulator skips the device's second boot: its `boot2` is the deep-sleep wake (raw 0x05, cause 4
  = timer) and the device's `boot3` is `not printed` there; the RTC time counter reads 12,652 ms
  after the sleep, so it kept counting. Both boots print `WAIT|boot<n>` with `host=connected`,
  `waited_ms=30` (the floor: the connection monitor reports the host at its first reading, since
  the model raises SOF every emulated millisecond and brings the link back at the wake instant)
  and `settle_ms=1500`; boot1's wait is why boot2's RTC time is 1,530 ms later than before the
  wait was added. (Step 1; "Step 3: reset and deep sleep" below changed all of it.)

### Step 3: the register rows

Captures: `device-probe_campaign_regs-20260924T164141Z-run1.clean.log` (regs, "line" below) and
`device-probe_campaign_timing-20260924T155139Z-run{1,2}.log` (timing). `cargo xtask probes compare`
on the regs capture goes from 67 equal and 24 different to 86 equal and 5 different, and on each
timing run from 9 equal and 17 different to 12 equal and 14 different (`rsa.RSA_M_PRIME`, and
UART0 `CONF0`/`CONF1`, which the reset rule below fixed although they belong to the timing rows).
`t1_campaign_regs_rows_match_the_device_capture` checks the 182 fixed lines against the
captures.

| Row | Device | Emulator before | Emulator after | Class | Evidence |
|---|---|---|---|---|---|
| `flash_xmc` any command at 0x800000 or above | reads the cell 8 MB below (`same_as_0x000000=1`) | 32 bytes of 0xFF | the cell 8 MB below | C to A | line 394 |
| `rsa.RSA_M_PRIME` | M' 0 gives 0x00000001 | 0x6d660a7e (M' ignored) | 0x00000001 | C to A | timing lines 67, 68, both runs |
| `regi2c` 0x040, 0x044, 0x048 | 0x2100e408, 0x00fbffff, 0x0001fe04 | 0x00000008, 0xfffbffff, 0x0001fe00 | equal | C (row), values A | lines 251 to 253 |
| `gpio.GPIO_FUNC*_IN_SEL_CFG` | 48 selectors 0x1f (0x1e for 53, 54, 74) | 0 | equal | C, per-selector rows | lines 71 to 198 |
| `iomux` pad reset values | GPIO0 to GPIO21 0x802 ... 0x1b02 | 20 pads differ | equal | new row A | lines 358 to 379 |
| `spi0.SPI_MEM_USER` | 0x200000c0 | 0xf00000c0 | equal | C | line 320 |
| `spi0.SPI_MEM_MOSI_DLEN`, `MISO_DLEN` | 0, 0 | 0xff, 0xff | equal | C | lines 323, 324 |
| `spi0.SPI_MEM_MISC` | 0x28 | 0x2 | equal | C | line 325 |
| `spi1.SPI_MEM_CTRL2` | 0 | 0x3e0 | equal | C | line 330 |
| `system.SYSTEM_PERIP_CLK_EN1` | 0x200 | 0 | equal | C | line 341, TRM register 16.4 |
| `system` gate lines, I2S0 | held 0, released 0, clock off 0x33330033, after 0 | 0x12120012, 0x12120012, 0x12300030, 0x12300030 | equal | CLK_EN0 C, RST_EN0 C | lines 381 to 384 |
| `system` gate lines, AES | held 0, released 0, clock off 0xffffffff, after 0 | 0x5a5a5a5a, 0x5a5a5a5a, 0x12345678, 0x12345678 | equal | CLK_EN1 C, RST_EN1 C to A | lines 386 to 389 |
| `uart0` CLKDIV, CONF0, CONF1, CLK_CONF | 0 (held in reset) | 0x0030015b, 0x0400001c, 0x1, 0x03700000 | equal | unchanged (timing rows) | lines 351 to 354 |
| known-diffs `timg0.TIMG_RTCCALICFG` | 0x00013000 | 0x04008000 | equal | known diff 068 unsettled | line 356 |
| known-diffs `timg0.TIMG_RTCCALICFG2` | 0xffffff98 | 0x08000018 | equal | known diff 080: our side confirmed | line 357 |
| ADC ladder medians | 3, 394, 782 | 0, 433, 862 | 3, 393, 782 | B, Down pinned one below | lines 402 to 404 |
| `usj` FRAM_NUM width | max 2046, over_2047 0, wraps 2 | max 2041, the rest equal | unchanged | B, width verified | line 400 |
| `rtc_cntl` CLK_CONF, GPIO_WAKEUP, TIMER1 | 0x20c80298, 0x02000081, 0x14190143 | 0x30c80298, 0, 0x14140143 | unchanged | C, explained | lines 262, 254, 255 |

The gating rule, from the gate lines and the rows read after the app's start
(`crates/pemu-soc-c3/src/wiring/gates.rs`):

- A block whose `PERIP_RST_EN` bit is 1 reads 0 and ignores writes, clock on or off (I2S0 and AES
  held: `rst_held` 0; UART0, held by the app, reads 0 in every register).
- The write that raises the bit resets the block (`released` reads the reset value; TIMG0's
  calibration registers read their reset values after `enable_timer_group0_for_calibration`
  pulses `TIMERGROUP_RST`; UART0's `CONF0`/`CONF1` in the timing capture are the driver's
  read-modify-writes of the reset values after its `UART_RST` pulse).
- I2S0 and AES with the clock off and the reset released ignore writes and read the last value
  they returned while clocked (`clk_off` 0x33330033 and 0xffffffff, `after_clk_on` 0). UNVERIFIED
  as a mechanism: the probe read the writable-bits pattern just before it gated the clock, so
  "the writable bits read 1" fits too. A probe that reads another value before gating would
  separate the two.
- It is not a rule of every block. SARADC answers with `APB_SARADC_CLK_EN` clear (lines 273 to
  276), so no other clock bit gates anything, and SARADC keeps `CTRL`, `FSM_WAIT` and
  `APB_ADC_CLKM_CONF` at the bootloader's values across the app's two `APB_SARADC_RST` pulses
  (`adc_apb_periph_claim`), so it is outside the reset rules too. SYSTIMER and USJ are left out
  as approximations (the SYSTIMER restart belongs to the timing rows; a USJ reset drops the USB link),
  UNVERIFIED. SYSTIMER is under the rule too, on the reset probe's TIMEBASE lines. The two I2S0 and AES read latches are snapshot state: FORMAT_VERSION 21.

Found on the way: the regs row `timg0.RTCCALICFG` also corrected an earlier reading, "RDY is 1 at reset". A
cycling calibration sets no RDY (IDF `rtc_time.c` says RDY is the one-off mode's), and the first
`rtc_clk_cal_internal` leaves it by writing a small `TIMEOUT_THRES` and waiting for `TIMEOUT`
(new wait row `timg0.rtccali_cycling_timeout`).

The three `rtc_cntl` rows are not boot values. The capture's boot is a core reset (`rst:0x15`),
which keeps the RTC domain, and `probe_campaign_reset` put the device in a timer deep sleep
earlier that day. XTL_BUF_WAIT 100 and XTAL_GLOBAL_FORCE_NOGATING 0 are what `rtc_sleep.c`
writes for a deep sleep, and PIN_CLK_GATE with GPIO0's low-level type is what
`gpio_ll_deepsleep_wakeup_enable` writes. The rows keep class C. The ADC sample counts measure
how long the owner held each button, and the USJ maximum depends on the host's SOF phase. Neither
is a model fact.

### Step 3: reset and deep sleep

Capture: `device-probe_campaign_reset-20260924T164812Z-run1.log` (host-wait build, merged SHA 5084c2bd..., three boots).
`cargo xtask probes compare` on it goes from 3 equal and 4 different to 4 equal and 3 different;
the three rows left are the `BOOT` lines' `rtc_time_ms` of boot2 and boot3 (below) and, on
`RTC_CNTL_SWD*`, the new `SWD|timeout` line the capture's build does not print. The boot1 reset
cause and RTC time are a capture-method difference and are listed as `METHOD` lines, compared by
neither verdict (`xtask/src/probes/compare.rs` `METHOD_FIELDS`).
`t1_campaign_reset_rows_match_the_device_capture` checks the ten settled lines and the
boot2 and boot3 causes against the capture.

| Row | Device | Emulator before | Emulator after | Class | Evidence |
|---|---|---|---|---|---|
| `rtc_cntl.RTC_CNTL_SWD*` (B65) | auto-feed off and no feed: reset, raw 0x12, reason 7, RTC memory kept, `SWD_CONF` 0x84b00000 after it | never resets (`SWD\|no_reset` after 8 s) | resets 950 ms after arming, every field equal | C to B; timeout C | boot2 `BOOT`, `SWD\|reset` |
| `rtc_cntl.reset_domains` (V35), the RTC counter at a `SYS_` reset | restarts: boot2 reads 51 ms, boot1 read 2,852,258 | kept (no such reset) | restarts (14 ms) | settled, the `rom/rtc.h` reading | boot2 `rtc_time_ms` |
| `rtc_cntl.RTC_CNTL_GPIO_WAKEUP` (B57), GPIO0 low beside a 3 s timer | wakes at once by GPIO: cause 7, status 0x1, word 0x02000081 | timer wake after 3 s: cause 4, 0x02000080 | equal | C to B | boot3 `WAKE` |
| board: GPIO0 in deep sleep | reads low with no key pressed | the ladder level (high when released) | low whatever the keys | new board fact, B | boot3 `WAKE`; `boards/ai-passport.toml` `pullup_held_in_deep_sleep = false` |
| `usj.host_link` (V59), the link at a `SYS_` reset | drops (`[capture: link down 296 ms]`); boot2 `waited_ms=140` | kept, 30 | drops; 140 | B, host macOS | boot2 `WAIT`, `timing-profiles.usj_enum_reset_ps` 217 ms |
| `usj.host_link` (V59), after a deep-sleep wake | boot3 `waited_ms=60` | 30 (delay 0) | 60 | B, host macOS | boot3 `WAIT`, `timing-profiles.usj_enum_wake_ps` 137 ms |
| `RTC_CNTL_DIG_ISO`, `PWC`, `DIG_PAD_HOLD` after the wake (B63, B64, B66) | 0x10400080, 0, 0 | equal | equal | unchanged, C (stored words) | boot3 `REG` |

What the rows rest on:

- **The super watchdog** (`crates/pemu-soc-c3/src/periph/rtc_cntl.rs`). Armed means its reset is
  enabled and nothing feeds it: `FIB_SEL.FIB_SUPER_WDT_RST`, `SWD_BYPASS_RST`, `SWD_DISABLE` and
  `SWD_AUTO_FEED_EN` all clear. IDF's bootloader clears the first two
  (`bootloader_ana_super_wdt_reset_config(true)`) just before it sets auto-feed; the ROM never
  touches the watchdog (its only accesses are the reset-flag reads of
  `analog_super_wdt_reset_happened` and `clear_super_wdt_reset_flag`, ROM ELF rev101), so the
  `FIB_SEL` reset value is read as bypassing the reset, which keeps every ROM stage unchanged
  (UNVERIFIED mechanism). `SWD_FEED` (no ROM or IDF path writes it) and the feed interrupt are
  not modeled. The reset is `SYS_` class: registers back to their reset values, RTC memory and
  SRAM kept. `swd_conf=0x84b00000` is not a reset flag in bit 31, as the step 2 note read it:
  bit 31 is `SWD_AUTO_FEED_EN`, which the bootloader sets again after the reset returned the word
  to 0x04b00000; the flag is bit 0, set by the reset (TRM 12.3.2.2) and cleared by the ROM before
  any app code runs, so whether it is set is UNVERIFIED (the ROM prints `analog super wdt reset`
  when it finds it, a banner line the capture lost to the link drop).
- **The timeout** is class B, 3355 ms (`SWD_TIMEOUT_PS`), not the TRM's "slightly less than one
  second": `device-probe_campaign_reset-20260924T201202Z` run1 to run3 print `alive_us` 3372243,
  3341213, 3339203 with `max_gap_us` 10888, midpoints averaging 3.356 s, spread 1 % (the RC slow
  clock's drift). With it the emulator prints `alive_us` 3354297 (the
  compare is exact, so the row still reads different, now within 0.5 % of run1). The compare stays
  4 equal and 3 different: all three carry the `BOOT` lines' `rtc_time_ms`, 51 and 1826 on the
  device in every run against 17 and 1763, a boot-time residue after a `SYS_` reset and a
  deep-sleep wake that belongs to the timing rows. The 950 ms
  placeholder before it was the TRM's wording. The rebuilt
  `probe_campaign_reset` measures it: the arming write also sets `SWD_FEED` once (with auto-feed
  on, the count's phase is unknown: the watchdog is fed at its feed interrupt about 100 ms before
  the timeout), the boot then stores the time since that write in RTC memory every 1 ms, and the
  boot after the reset prints `SWD|timeout|alive_us=...|max_gap_us=...`: the timeout lies in
  `(alive_us, alive_us + max_gap_us]`. The emulator prints `alive_us=949000|max_gap_us=10931`.
- **GPIO0 in deep sleep** is the board's (`crates/pemu-board/src/ladder.rs`
  `LadderConfig::pullup_held_in_deep_sleep`). IDF's deep-sleep preparation turns the chip's own
  pull-up (about 45 kOhm) on for a pad armed on a low level and holds it, and the start-up
  isolation floats the other pins; so the pad reads low only if the board pulls it to ground
  harder, which the ladder's 10 kOhm pull-up does with its supply off (about 0.6 V, under V_IL).
  Which supply, UNVERIFIED without the schematic. With the pull-up not held, GPIO0 reads low for
  the whole sleep whatever the keys, so any GPIO0-low arming wakes the device at once.
- **The USB link** (`periph/usj.rs` `UsjModel::reset_to`, `specs/timing-profiles.toml`). A
  `SYS_` class reset (0x0F, 0x10, 0x12, 0x13) drops the link although the host side did not
  change, and the host enumerates the chip again after `usj_enum_reset_ps`; a `CORE_` reset keeps
  it (the capture's RTS reset, 0x15: boot1 `waited_ms=30`, the monitor's floor). UNVERIFIED for
  the three causes other than 0x12. The deep-sleep wake re-attaches the link at the wake instant
  (`sleep.rs`) and waits `usj_enum_wake_ps`, now wired from the profile
  (`Machine::apply_console_pacing`), which also serves the plug-while-on delay. A light-sleep
  wake still re-attaches at once: with the delay there, `probe_clocks`'s light-sleep phase never
  finishes under `device` (its console task stays blocked, UNVERIFIED why), and no capture times
  that case (`sleep.rs` "The link during light sleep"). Both values are
  one sample against a macOS host and are fitted so the emulator's own boots print the device's
  waits: the emulator reaches app_main about 83 ms after the reset or the wake, and the device's
  chip time to app_main is not measured. `boards/ai-passport.toml` `[usb] enumerate_ms` is read by
  nothing and stays only because `config_hash` covers it.
- **The RTC domain across a core reset** (found with "Step 3: the register rows"): `rtc_cntl`
  keeps every register but `RESET_STATE` across `CORE_` and `CPU0_` resets and the deep-sleep
  wake, and restores them at a `SYS_` reset (`periph::rtc_cntl::tests::the_rtc_domain_survives_a_core_reset_and_not_a_sys_reset`,
  with the three words the regs capture read after its RTS reset). The SYSTEM gating rule of
  `wiring/gates.rs` is untouched; the `SYS_` reset reaches the gated blocks through the ordinary
  fan-out.

Left open for the timing rows (step 3f below found the mechanism): `rtc_time_ms` at app_main.
The device reads 51 ms after the super-watchdog reset and 1,817 after the wake; the emulator 14
and 1,757. The emulator's value is
about 69 ms less than the virtual time since the reset (app_main at 83 ms; 6 against about 74 at
power-on), so IDF's `esp_rtc_get_time_us` starts counting late here; the device's counter
restarting at the reset (this section's fact) holds on both sides.

Snapshot format 24 (the pending super-watchdog timeout and the USJ reset enumeration delay).

### Step 3f: the timing residue

Four items the earlier sections left: the `rtc_time_ms` residue of the reset probe, the nominal
RC_FAST rate outside TIMG, TIMG's slice rule, and the one-CPI residual of the delta check's `sha256_1m`. The
probe extensions the first and the last need were written as source first and captured with a
later rebuild.

| Item | Device | Before | After | Class | Evidence |
|---|---|---|---|---|---|
| `rtc_cntl.RTC_CNTL_STORE0`..`7` at a `SYS_` reset | IDF's RTC time restarts at the super-watchdog reset: boot2 51 ms after a boot1 of 12122679 | kept; with the capture's boot1 (an RTS reset of a running chip) boot2 prints 1855425889 ms | cleared (`rom/rtc.h` `SUPER_WDT_RESET`); boot2 3 ms, boot3 1749 | STORE1 at 0x12 B; the other words and causes UNVERIFIED by the row | `device-probe_campaign_reset-20260924T201202Z` run1 to run3 boot2; `t1_campaign_rtc_time_restarts_after_a_super_watchdog_reset` |
| `rtc_time_ms` boot2, the rest | 51 | 17 (a power-on boot1 hid the wrap) | 3 | C, +48 ms | as above |
| `rtc_time_ms` boot3 | 1826 | 1763 | 1749 | C, +29 ms over the model's elapsed time | as above |
| RC_FAST in SYSTEM, LEDC, I2C0, UART0 | RC_FAST/256 69279 Hz | 17.5 MHz nominal in three blocks, a private copy in UART0 | `timg::RC_FAST_HZ` (17735424 Hz) in all four | A (the rate); the blocks' use of it unchanged | `device-probe_campaign_timing-20260924T155139Z` line 71; no corpus image selects RC_FAST there, so no record moves |
| TIMG slice rule | | stops only on an interrupt-level change | a write that arms a T0 alarm or a watchdog stage sooner than every pending event also ends the slice, as SYSTIMER's | model rule | `timg::tests::a_write_that_arms_a_sooner_event_ends_the_slice`; with it a watchdog hold written below the count trips at the next tick, not at the write (IDF's IWDT tick hook writes the holds, then feeds; `t1_m3_probe_reset_restarts_20_times` caught the stage firing between the two); no record moves |
| `probe_timing` `sha256_1m` | 106.74 ms | +20 % to +35 % | unchanged, pinned | C | below |

**Why STORE1 is 0 after the super-watchdog reset.** IDF's `esp_rtc_get_time_us`
(`esp_hw_support/esp_clk.c`) keeps the time and the last counter value in RTC memory and adds
`(ticks - rtc_last_ticks) x STORE1` at each call; it starts again from 0 only when STORE1 reads 0
(or the RTC-memory record fails its checksum). The probe shows RTC memory kept (`rtc_magic=1`),
and the reset rows showed the counter restarting. With STORE1 kept, the first call after the
reset subtracts boot1's counter (12122679 ms of ticks) from a counter near zero, and the 64-bit
wrap prints cut to 32 bits: the emulator, run the way the device is captured (a
power-on, then an RTS reset at 300 ms, so boot1 is `raw=0x15` with the counter running), printed
1855425889. The device prints 51 in all four runs, so it took the restart branch. The committed
record could not show it: its boot1 is a power-on, and a counter at 73 ms is still below boot2's
at 86. The ROM's own header gives the same reading (`rom/rtc.h`: `SUPER_WDT_RESET`, "reset digital
core and rtc module"), which puts STORE0-7 with the counter the reset rows already settled.

**What the rest of the residue is, and is not.** After the fix IDF's time counts from
`esp_clk_init`'s slow-clock calibration (the first call, which finds STORE1 at 0), about 3.9 ms
of virtual time before app_main's read. The device reads 51, so its span from that call to
app_main is about 48 ms longer after the super-watchdog reset, with the USB link down (it is down
for about 290 ms, the capture's `link down` markers). The emulator's trace of the same boot (ROM,
bootloader and app, `DEVICE` profile): the reset at 4958.553 ms, the ROM's banner committed at
+0.8 ms, its one 5000-poll wait for the undrained packet at +1 to +16 ms, after which every
console byte polls once and drops, `esp_clk_init` at +82.3 ms, app_main's read at +86.3. Not the
host wait: the probe reads the time before it, and the waits are tick-exact and equal on both
sides (`waited_ms` 140 and 60). A candidate with the right size is the IDF USB console's 50 ms
`TX_FLUSH_TIMEOUT_US` (`usb_serial_jtag_vfs.c`), which a writer spends once when a byte finds the
FIFO full within 50 ms of a byte that went out; in the model no console byte goes out between the
ROM banner and app_main, so it never waits, and nothing captured says whether one did on the
device. boot3 carries boot2's time across the deep sleep: the device's elapsed time from boot2's
read to boot3's is 1775 ms against the model's 1745.7 (boot2's wait, prints and drain 1659.8, the
sleep 0.3, the wake to app_main 85.6); the 29 ms more sits somewhere in the deep-sleep entry, the
sleep and wake state machine, and boot3's boot with the link down. The extended
`probe_campaign_reset` prints `TIMEBASE` lines that split both: at each app_main the esp_timer
time (reset or wake to app_main), the RTC counter at the current calibration (time since the
counter restarted) and IDF's RTC time; and both RTC times at the `SLEEP` line.

**The delta check's `sha256_1m`: why one CPI cannot be split with these captures.** The instruction mix of
the three cache-resident loops, from the probe ELFs and checked against the emulator (its cycles
at CPI 1.64 over the counted instructions):

| Loop | Instructions | Device cycles | Device cycles per instruction | Per iteration |
|---|---|---|---|---|
| `probe_campaign_timing` cache_code_warm (line 57) | 1008 (64 x 10 in IRAM, 330 in the 64 flash functions, 21 of them `mul`, 38 around them and the two SYSTIMER reads) | 1405 | 1.39 | a taken `bge`, a `jalr`, a `ret`, a DRAM load |
| cache_data_warm (line 59) | 2342 (256 x 9 in IRAM, 38 around) | 3188 | 1.36 | a taken `bge`, a flash-line load the next instruction uses |
| `probe_timing` sha256_1m fill | 11.53 M of 12.16 M (1 M x 11, flash-resident) | 17.08 M in all, the SHA engine's 16384 blocks inside | at most 1.48 | a taken `bltu`, a DRAM byte store |

Take one cycle a instruction plus `t` for each taken control transfer, `u` for a load whose
result the next instruction uses, and `X` for what the two SYSTIMER reads cost beyond a cycle an instruction (six peripheral accesses). The
two warm loops give 192 t + X = 397 and 256 t + 256 u + X = 846 (no `mul` cost), one equation
short: `t` 2 reads `X` 13 and `u` 1.25, `t` 1 reads `X` 205 (34 cycles an access) and `u` 1.5,
and both fit. `sha256_1m` adds no equation, because its SHA time is not measured apart from the
fill: `t` 1 to 2 leaves 3.5 to 4.5 M cycles (1.3 to 1.7 us a block with the driver) where
`sha_block_ps` says 0.6 (class C). The IRAM paths that read close at 1.64 (`probe_intc` LAT +11.6
%, `probe_clocks` WFI phases -6.1 % and +0.1 %) are trap, context-save and scheduler code whose
mix is not counted. So no per-class cost is identified, and one would not be cheap to try: time
is `instructions x ps_per_insn` plus stalls (`Clock::now(insns)`), charged the same way
by the interpreter and the wasm JIT, so a per-class charge changes both hot paths and the frozen
clock contract, and moving the base CPI below 1.64 moves every boot fit phase unless
the boot's mix carries the difference, which would have to be counted per phase first. The
residual stays pinned, class C. The extended `probe_campaign_timing` appends the capture that
would identify it: `TIME|cpi_<class>`, 256 iterations of eight instructions of one class from
IRAM (ALU; branch not taken and taken; jump; call and return; DRAM and flash loads, alone, with a
use right after and one instruction later; store; `mul`; `div`; a CSR read; a GPIO_IN read and a
GPIO_OUT_W1TC write of 0) against `cpi_empty`, the loop alone; and `TIME|sha256_64k_prefilled`, the
SHA of 64 KB already in RAM, which reads `sha_block_ps` without the fill loop. Built in a scratch
directory and run in the emulator, it prints every line and DONE, and the existing lines keep
their values and their line numbers.

### Step 3g: per-class cycle cost

The one effective CPI is replaced by a cost per instruction class, read from
`device-probe_campaign_timing-20260924T234712Z-run1` lines 126 to 142 with the probe's ELF (every
kernel is `csrr; 256 x (8 of the class; c.addi; c.bnez); csrr`). Rows of
`specs/timing-profiles.toml`: `cpi_milli` 1000 (the one-cycle base, B to A), and seven new rows
beyond it. "Before" is the record under the one CPI of 1.64, "after" the renewed
`tests/fw/campaign/probe_campaign_timing.emu.txt`; `t1_campaign_cpi_rows_match_the_device_capture`
pins every "after".

| Kernel | Line | Device | Before | After | Row, class |
|---|---|---|---|---|---|
| empty (c.addi, c.bnez taken) | 126 | 1023 | 842 | 1023 | `cpi_milli` 1000, `taken_branch_cycles` 2, A |
| alu | 127 | 3071 | 4201 | 3071 | base, A |
| branch_not_taken | 128 | 3071 | 4200 | 3071 | base, A |
| branch_taken (8 `beq` to the next, 32-bit at 2 mod 4) | 129 | 9214 | 4200 | 9214 | `taken_branch_cycles` 2, `split_redirect_cycles` 1, A |
| jump (8 `c.j`) | 130 | 5119 | 4200 | 5119 | `jump_cycles` 1, A |
| call_ret (8 `jal`, `c.ret`) | 131 | 11262 | 7559 | 11262 | `jump_cycles`, `split_redirect_cycles`, A |
| load_dram | 132 | 3583 | 4200 | 3071 | C, -512 (2 an iteration) |
| load_dram_use | 133 | 7679 | 7558 | 7422 | `load_use_cycles` 1; C, -257 (1 an iteration) |
| load_dram_use_gap1 | 134 | 8191 | 10917 | 7167 | C, -1024 (4 an iteration) |
| load_flash (warm DROM line) | 135 | 3326 | 4200 | 3326 | base, A |
| load_flash_use | 136 | 7167 | 7559 | 7167 | `load_use_cycles` 1, A |
| store_dram | 137 | 4094 | 4201 | 3326 | C, -768 (3 an iteration) |
| mul | 138 | 3326 | 4200 | 3326 | base, A |
| div (1000000 / 7) | 139 | 66814 | 4200 | 66814 | `div_cycles` 31, B (one operand pair) |
| csr_read | 140 | 3071 | 4200 | 3071 | base, A |
| mmio_read (GPIO_IN) | 141 | 13821 | 4200 | 13566 | `mmio_load_apb_cycles` 3, B; C, -255 (1 per 8 reads) |
| mmio_write (GPIO_OUT_W1TC) | 142 | 17407 | 4200 | 17407 | `mmio_store_apb_cycles` 4, B |

The class C residues are the four DRAM kernels, whose code (IRAM) and data (DRAM) share the
internal SRAM: the flash-data kernels, the same loops with the data behind the cache, read none,
and the DRAM residue per access is 0.125 to 0.5 cycles depending on what sits between the
accesses, a fetch-contention pattern eight identical accesses a loop do not identify. The MMIO
rows are GPIO's, counted in APB cycles (the kernels ran at CPU 160 and APB 80 MHz; the
bootloader at CPU 80 MHz pays 3 and 4 CPU cycles, unmeasured, class C at that ratio).

Other lines of the same record that moved with the costs (device, before, after): cache_code_warm
1472, 1654, 1377; cache_data_warm 3446, 3842, 3415; cache_code_cold 20902, 21725, 21447;
cache_data_cold 83506, 84124, 83697; aes_cbc_16384 8225, 8923, 8066 cycles; i2c_read_20_100k 9171,
9088, 9055 us; systimer cadence 15999, 15999, 16002 ticks (the probe's poll loop, now charged its
MMIO reads, samples 3 ticks coarser); `TIME|sha256_64k_prefilled` 171079, 185215, 171087 cycles,
with `sha_block_ps` derived from it at the new costs (0.515 us a block, C to B; the emulated work
around the engine is 86607 cycles).

Outside the record: the delta check's `sha256_1m` +26.3 % to +0.1 % (the pinned residual of step 3f is gone,
the test asserts the 20 % band), `flash_read_64k` -2.1 %, `i2c_read_100` -1.3 %; `probe_intc` LAT
steady 194 against 172 (+12.8 %), sample 0 +21.8 %; `probe_clocks` WFI phases +2.1 % and +1.8 %.
The boot fit no longer fits `cpi_milli`: fit RMS 12.82 ms, validation RMS 1.97 ms, every phase inside its
band, the bootloader's segment phases long (+3, +11 ms) and the application's IRAM-heavy phases
short (-4, -26 ms), the DRAM residue above.

What would settle the class C rows (a capture not built in this section): the DRAM kernels with
the data in an SRAM block the code is not fetched from, and again from flash-resident code; with
0 to 3 independent instructions between the accesses; and the two MMIO kernels with the CPU at
80 MHz (`rtc_clk_cpu_freq_set_config`), which the APB-cycle rows predict at 3 and 4 cycles an access.

### Step 3h: the boot time base and the boot fit residue

**Part A, the capture that separates step 3g's class C rows** (probe source; captured after a
rebuild). `probe_campaign_timing` appends, after every existing line: `TIME|cpi_at_<kernel>`
(the four class C DRAM kernels, the same IRAM code, operand at the static data and at the low
end, middle and high end of the largest free internal block), `TIME|cpi_gap_<load|store>_gap<k>`
(k = 0 to 3 ALU instructions after each access, loop head aligned, operand static and far),
`TIME|cpi_flash_<kernel>` (the loop, ALU and the four DRAM kernels from a warm flash line),
`TIME|cpi_x_<kernel>` (a CSR write, `div` with a zero quotient and by 1, `mulhu`, a store of a
loaded value), `TIME|cpi_mmio_read_<block>` (the GPIO read kernel on SYSTIMER_CONF and
EXTMEM_ICACHE_CTRL), `TIME|cpi80_<kernel>` (loop, ALU, DRAM load and store, GPIO read and write
at CPU 80 MHz), `REG|extmem.<autoload registers>.app`, `TIME|fill_work[80]_<use|nouse>_k<k>`
(128 cold flash lines with 0 to 96 inner iterations of work after each, then warm, at 160 and
80 MHz) and `TIME|rom_crc32_4k[_80]` (the ROM's crc32_le over 4 KB at 160 and 80 MHz). Every line
carries its code address and, for DRAM, its operand's. Built in a scratch directory and run in the
emulator it prints every line and DONE with status ok; the record needs the rebuild.

**Part B, the boot time base.** `device-probe_campaign_reset-20260924T232751Z` run1 to run3,
against the record before and after this section (µs unless named):

| Field | Line | Device run1 (run2, run3) | Before | After | Class |
|---|---|---|---|---|---|
| boot1 `timer_us` | 59 | 2632 (2645, 2631) | 73760 | 2542 | A: esp_timer counts from its init |
| boot2 `timer_us` | 66 | 50400 (50400, 50400) | 87293 | 50356 | A: the same, plus IDF's 50 ms console flush timeout |
| boot2 `rtc_time_us`, `BOOT` `rtc_time_ms` | 66, 65 | 51911 (51875, 51889), 51 | 3931, 3 | 51948, 51 | A (step 3f's +48 ms residue) |
| boot2 `rtc_counter_us` | 66 | 128169 (128109, 128151) | 87291 | 135308 | C, +7.1 ms: ROM and bootloader after the reset |
| SLEEP `timer_us` | 69 | 1683182 (1683223, 1683254) | 1728060 | 1683118 | A |
| SLEEP `rtc_counter_us`, `rtc_time_us` | 69 | 1762680, 1686408 | 1728057, 1644689 | 1768070, 1684702 | C: +7.1 ms carried, -1.7 ms RC rate |
| boot3 `timer_us` | 76 | 44079 (41897, 43099) | 86602 | 50356 | C, +6.3 to +8.5 ms |
| boot3 `rtc_counter_us` | 76 | 1900588 (1898751, 1899338) | 1833661 | 1921684 | C, +21.1 ms: the rows above and +9.4 ms |
| `WAIT` boot2, boot3 | 64, 74 | 140, 60 | 140, 60 | 140, 60 | B: `usj_enum_reset_ps` 217 to 263 ms, `usj_enum_wake_ps` 137 to 178 ms, refitted |

What each is:

1. **esp_timer counts from its own init, not from the chip reset** (the fix). IDF's
   `esp_timer_impl_early_init` pulses `SYSTIMER_RST` (`PERIPH_RCC_ACQUIRE_ATOMIC`, first user of the
   module), which restarts both counters; `wiring::gates` now applies the SYSTEM reset rule to
   SYSTIMER, which "Step 3: the register rows" had left to the timing rows. Line 59 shows it on
   its own: 2632 µs at app_main of a boot whose ROM and bootloader alone take about 70 ms. The model's own split of that
   boot (breakpoints on the ELFs' symbols, `campaign_time_points`): `esp_timer_impl_early_init` at
   71.2 ms, app_main 2.5 ms later.
2. **The 50 ms after the super-watchdog reset is IDF's console timeout.** With the USB link down the
   IN FIFO does not drain, and `usb_serial_jtag_tx_char_no_driver` (`usb_serial_jtag_vfs.c`) spins
   while `esp_timer_get_time() - last_tx_ts < 50000`, with `last_tx_ts` 0 because no byte has gone
   out: it waits until esp_timer reads 50 ms, then drops the rest. Before the fix esp_timer was
   already past 70 ms there, so the wait was never taken (step 3f's "every console byte polls once
   and drops"). The model now reads 50356 against 50400 on all three runs (the 44 µs is the work
   from the wait's end to app_main), and IDF's RTC time, which starts 1.5 ms before esp_timer, 51 ms
   on both sides.
3. **ROM and bootloader after a `SYS_` reset with the link down, +7.1 ms, class C.** What the RTC
   counter holds before esp_timer starts: 77.8 ms on the device (`rtc_counter_us - timer_us`), 84.95
   in the model: ROM 30.3 ms, of which about 16 ms is the ROM's `usb_uart_tx_one_char` giving up on
   the undrained packet (5000 polls, each an `ets_delay_us(1)` at the reset CPU rate); bootloader 44.8;
   app start to esp_timer init 9.9. With the link up (boot1, power-on in the model, esptool reset on
   the device) the same span is 71.2 ms in the model and about 70.4 on the device (its log reads
   `main_task: Calling app_main()` at 73 ms with esp_timer at 2.6), so the link-down extra is about
   7.4 ms on the device and 13.7 in the model, the ROM wait the likely carrier. Not changed: no
   capture times the ROM phase after the reset (the link is down, so its console is lost). A
   bootloader hook (`bootloader_before_init`) that stores `rtc_time_get()` in an RTC_NOINIT word for
   the app to print would split ROM from bootloader.
4. **The RC slow clock runs 0.1 % fast against its boot calibration, class C.** From boot2's read
   to the SLEEP line the device's counter advances 1634.5 ms against esp_timer's 1632.8; the model's
   RC runs at exactly the calibrated rate, so the SLEEP line's RTC fields read 1.7 ms less than the
   carried +7.1 would give. Not a model fault: one deterministic rate (`rtc_slow_hz`'s basis).
5. **Boot3: the device reaches app_main before the 50 ms timeout, class C, +6.3 ms (run1), +7.3,
   +8.5.** The model waits the whole timeout after the deep-sleep wake as after the reset; the device
   reads 41.9 to 44.1 ms and varies by 2 ms between runs, so something ends its wait early. Its
   `WAIT` line puts the first SOF 30 to 60 ms after app_main, so it is not the host reading the
   packet. Not identified; the USJ `INT_RAW` bits (`USB_BUS_RESET`, `SOF`) read at app_main of boot3
   would say whether the host's bus reset preceded it.
6. **Deep-sleep entry, sleep and wake to boot3's esp_timer init, +9.4 ms, class C.** The SLEEP
   line to boot3's esp_timer init is 93.8 ms on the device (RTC counter less esp_timer) and 103.3 in
   the model: 18.5 ms of drain and `esp_deep_sleep_start`, 0.5 ms to the wake, then ROM 29.6 (the
   same link-down ROM wait as item 3), bootloader 44.8 and app start 9.9. With item 3's +7.1 taken
   as the ROM wait again, 2.3 ms sits in the entry, the sleep state machine and the wake, which no
   line separates.

**Part C, the boot fit residue.** Phases under the committed profile (unchanged by this section: the
SYSTIMER restart moves no `pk` boot anchor), with the model's own counts per phase (a scratch counter in
the costed loop, not committed) and each row's share (`m11_phase_variants`, a row set to its
bound):

| Phase | Device | Model | What the model spends | Attribution |
|---|---|---|---|---|
| segment 0 -> 1 (145552 B, map) | 23 | 26 | 2274 blocks; fills 9 ms | as below, same per block |
| segment 3 -> 4 (730488 B, map) | 117 | 128.4 | 11414 blocks of 64 B at CPU 80 MHz, 898 cycles each against the device's 820: bootloader IRAM 209 instructions + 144 extra (32 SRAM loads, 16 SRAM stores, 16 flash loads), ROM SHA 130 + 102 extra (16.4 MMIO stores, 5.6 MMIO loads, 16 flash loads), 2 line fills (313 cycles) | C, -78 cycles a block to find. Bounds: MMIO at CPU = APB at 1 cycle, -7 ms; the branch extras (21 ms of the phase) if the 80 MHz penalty were smaller; line fills that overlap the work, up to -44 ms; ROM fetch speed (a third of the instructions). DRAM contention at step 3g's IRAM rate would add +2 to +3 ms, the wrong sign, so it is not this phase's mechanism |
| lvgl task -> lvgl ready | 57 | 53.0 | 3.30 M instructions, 94 % flash code; 1.08 M SRAM loads and 0.22 M stores from flash code, 0.05 M and 0.03 M from IRAM; fills 12 ms | C, -4 ms: DRAM contention at step 3g's IRAM rate (0.125 to 0.5 cycle a load, 0.375 a store) gives 1.4 to 3.9 ms if flash code pays it too, 0.03 ms if only IRAM code does |
| gauge profile -> ble init | 320 | 294.9 | 14.5 M instructions, 96 % flash code; 3.23 M SRAM loads and 1.55 M stores from flash code, 0.22 M and 0.12 M from IRAM; fills 63 ms | C, -26 ms: the same bound gives 6.2 to 13.7 ms from flash code and 0.4 to 0.9 ms from IRAM code, so at least 11 ms is not contention |

Implemented: nothing in Part C. No mechanism is evidenced: the one class C row with a measured rate
(DRAM contention from IRAM code) moves the bootloader the wrong way and the application phases by
under 1 ms, and its extension to flash code is what `cpi_flash_*` measures. What settles each row,
all in Part A's capture: `cpi80_empty` and `cpi80_alu` (branch cost at the bootloader's clock),
`cpi80_mmio_read` and `cpi80_mmio_write` (MMIO at CPU = APB), `fill_work80_*` against `fill_work_*`
and the `EXTMEM_ICACHE_AUTOLOAD_CTRL` line (fill cost at 80 MHz, and whether a fill overlaps work),
`rom_crc32_4k[_80]` (ROM code speed), `cpi_flash_*` (contention from flash code), `cpi_at_*` and
`cpi_gap_*` (its dependence on the SRAM block and on the gap).

### Step 3i: fill overlap, divide, mulh and MMIO by block

Read from `device-probe_campaign_timing-20260925T042926Z` (the step 3h build; run1
and run2 identical on every cycle row) with the probe's ELF; line numbers are run1's. "Before" is
the record before this section (step 3g's rows, blocking fill), "after" the renewed
`tests/fw/campaign/probe_campaign_timing.emu.txt`;
`t1_campaign_fill_overlap_divide_and_mmio_rows_match_the_device_capture` pins every
"after" (the `fill_work` lines within 150 cycles, the class C rows at their residue).

| Kernel | Line | Device | Before | After | Row, class |
|---|---|---|---|---|---|
| fill_work_use_k0 cold (warm 1536 equal) | 209 | 41604 | 41677 | 41686 | `cache_miss_cycles` 10, `cache_first_word_ps` 0.59 µs, `cache_fill_ps` 1.9775 µs, A |
| fill_work_use_k32 cold | 210 | 41642 | 57548 | 41693 | the same, A |
| fill_work_use_k96 cold | 211 | 63488 | 90317 | 63539 | the same, A |
| fill_work_nouse_k96 cold | 212 | 63382 | 90061 | 63466 | the same, A |
| fill_work80_use_k0 cold | 213 | 21444 | 21607 | 21431 | the same at 80 MHz, A |
| fill_work80_use_k48 cold | 214 | 32896 | 45670 | 32921 | the same at 80 MHz, A |
| `EXTMEM_ICACHE_AUTOLOAD_CTRL` | 204 | 0x8 | | | no prefetch (AUTOLOAD_DONE, ENA clear), A |
| cache_code_cold | 56 | 21042 | 21447 | 20868 | the fetch run rule (below), C inside the row |
| cache_data_cold | 58 | 83426 | 83696 | 83530 | fill rows, A |
| aes_cbc_16 (cold flash code) | 60 | 68913 | 66763 | 64936 | C, -5.8 %: cold flash code short |
| cpi_x_div_q0 | 189 | 31744 | 66560 | 31744 | `div_base_cycles` 13 plus quotient bits, B |
| cpi_x_div_by1 | 190 | 70656 | 66560 | 70656 | the same, B |
| cpi_div (1000000 / 7) | 139 | 66814 | 66814 | 66814 | the same, B |
| cpi_x_mulhu | 191 | 11264 | 3072 | 11264 | `mulh_cycles` 4, B |
| cpi_mmio_read_extmem_icache_ctrl | 195 | 5374 | 13566 | 5374 | `mmio_cpu_cycles` 2, EXTMEM off APB, A |
| cpi_mmio_read_gpio_in, _systimer_conf | 193, 194 | 13821 | 13566 | 13566 | `mmio_cpu_cycles` 2 + `mmio_load_apb_cycles` 2; C, -255 (APB clock edge) |
| cpi_mmio_write (GPIO_OUT_W1TC) | 142 | 17408 | 17407 | 17407 | `mmio_cpu_cycles` 2 + `mmio_store_apb_cycles` 3, B |
| cpi80_mmio_read | 200 | 9470 | 7422 | 9470 | the same rows at CPU 80 MHz, B |
| cpi80_mmio_write | 201 | 11263 | 9215 | 11263 | the same, B |
| sha256_64k_prefilled | 143 | 169843 | 170176 | 169688 | `sha_block_ps` kept, B |

What each is:

1. **Fill overlap.** A miss costs 10 CPU cycles to start the transfer, then the line's eight words
   arrive in order, word 0 at 0.59 µs and word 7 at 1.9775 µs (198 ns apart, 16 SPI clocks of a DIO
   read at 80 MHz); a load waits for its word, the next miss for the transfer. Fitted by least
   squares on the six `fill_work` lines: the loops that work longer than a transfer between misses
   pay 104 and 57 cycles a line at 160 and 80 MHz (10 cycles plus 0.59 µs), the back-to-back ones
   326 and 168 (10 plus the whole line). The blocking fill read these up to +26829 cycles. In-order
   arrival from word 0 is read from IDF, not measured: AUTOLOAD off (line 204), and IDF v5.5.3
   enables `EXTMEM_CACHE_FLASH_WRAP_AROUND` only on S2, S3 and C2 (`spi_flash/cache_utils.c`).
2. **Fetches.** A fetch that enters a line waits for the word ending its straight-line run there
   (the first `jal`, `jalr`, `ecall`, `ebreak`, `mret`, `wfi`, compressed forms included, or the
   line's end). Of three rules measured, the entry word alone leaves cold code 8.7 % short
   (`aes_cbc_16`) and the boot fit at fit RMS 18.09 ms; the whole line puts `cache_code_cold` +5.8 %
   (64 three-word functions, each alone in its line: silicon does not wait for their later words);
   this rule reads -0.8 % and -5.8 % and 13.61 ms. Class C: cold flash code about 6 % short
   (`aes_cbc_16`, the reset probe's boot1 app start -6.1 %). A `fill_work` twin for fetches (cold
   code lines with timed work between them) would settle it.
3. **Divide.** `div_base_cycles` 13 plus `max(clz(|b|) - clz(|a|), 0) + 1`, the quotient's bit
   count bound, over the magnitudes for the signed forms. Extra cycles a divide over the ALU
   kernel's 3071 (2048 divides): zero quotient 14 (13 + 1), 1000000 / 1 33 (13 + 31 - 12 + 1),
   1000000 / 7 31 (13 + 29 - 12 + 1), the three to the cycle. Deterministic and data-dependent;
   class B (three operand pairs fit one shape of the rule; no negative operand measured).
4. **mulh.** `mulhu` pays 4 cycles over the one-cycle base (11264 = 3072 + 2048 x 4, to the
   cycle), `mul` none (`cpi_mul` 3326, line 138, unchanged); `mulh` and `mulhsu` take the same row
   unmeasured, class B.
5. **MMIO by block.** An APB access is 2 CPU cycles plus 2 (load) or 3 (store) APB cycles rounded up
   to CPU cycles: at 160 MHz 6 and 8 cycles, at 80 MHz 4 and 5, which reads all four GPIO kernels
   at both clocks. EXTMEM (`0x600C_4000`, 4 KB) is on the CPU's own bus, not behind APB: 2 cycles
   (line 195). The 160 MHz reads keep step 3g's -255, one cycle every 8 reads, a clock-edge
   alignment the 80 MHz kernels (CPU = APB) do not show; class C.
6. **The DRAM residue follows the SRAM block, class C, not modelled.** In this build the original
   kernels read `cpi_load_dram` 3838 and `cpi_store_dram` 3838 (lines 132, 137; 3583 and 4094 in
   the 3g build) and the relocated `cpi_at_*` read step 3g's residue with the operand at `static`
   and `heap_low` (lines 144 to 151, `0x3fc8e420` and `0x3fc9b440`, SRAM Block 1,
   `0x3FC8_0000` to `0x3FC9_FFFF`, TRM table 16.3-1) and none at `heap_mid` and `heap_high` (lines
   152 to 159, Block 2). The code (`0x4038_17c0` on, the IRAM alias of Block 1) is in Block 1 too,
   so the residue is an IRAM fetch and a DRAM access in the same block. Its size moves with the
   code and data addresses at 8-byte granularity (the original kernels' operand is `ram_word` at
   `0x3fc8e428`, the relocated ones' `s_ram_near` at `0x3fc8e420`; `cpi_gap_*@static` read -767,
   0, 0 and -256 over gaps 0 to 3) and no flash-code kernel pays it (`cpi_flash_*`). One block rule
   without the address pattern would move the Block 1 rows by a constant the captures contradict,
   so none is taken. Not "every `cpi_at_*` equal": the Block 1 rows
   differ, the Block 2 rows match.

Outside the record: the boot fit RMS 12.82 to 13.61 ms, validation 1.97 to 1.89 ms, every phase in
its band; the bootloader's segment phases +3 and +11 to +1 and +0 ms (step 3h's fill-overlap
candidate), the application's lvgl and gauge-to-BLE phases -4 and -26 to -5 and -30 ms (cold flash
code, item 2, and the DRAM residue). The delta check: `flash_read_64k` -2.2 %, `spi2_153600` -0.0 %,
`i2c_read_100` -1.4 %, `sha256_1m` +0.1 %. Cycle-count pins
(`t1_cycle_counts_against_the_device_captures`): `probe_intc` LAT sample 0 +28.1 % (inside its
+15 to +30 % pin), steady +12.8 %; `probe_clocks` WFI -2.5 % and -0.7 %. `probe_campaign_reset`:
boot2's ROM and bootloader +7.1 to +4.3 ms, `usj_enum_reset_ps` 263 to 258 ms to keep `WAIT`
boot2 at 140.

### Step 3j: cold fetch probes, the Block 1 residue by address, the application phases

**Part A, probe source** (captured after a rebuild). `probe_campaign_timing` appends, after
every existing line: `TIME|fetch_<end|mid>_k<k>` (128 cold 32-byte flash code lines, each entered
at word 0 by a call from IRAM and left by `c.jr ra` in word 7 or in word 3, then a straight run of
k `c.addi` in IRAM, k = 0, 8, 16, 32, 64 and, to reach past a line's transfer, 128 and 256; then
the same lines warm), `TIME|fetchf_mid_k<k>` and `TIME|fill_workf_use_k<k>` (the mid lines and
`fill_work`'s load lines with the driver in a warm flash line: a hit on another line during a
fill), and `TIME|dres_<load|store>_c<cc>` (eight DRAM loads or stores a loop from IRAM, loop head
stepped 0 to 64 bytes in 8-byte steps, and on each line the operand stepped the same way, fields
`d00` to `d64`; code and data in SRAM Block 1, `block1=1` checked, both addresses printed). The new
code is naked or ordinary functions defined after the old, so every existing IRAM and flash
kernel keeps its code address (checked on the ELF: `cpi_load_dram` 0x403817c0, `run2`,
`cold_fn_*`, `cpif_*` unchanged), and the static operands move by 0x1200 plus a 192-byte buffer,
keeping their offset within 64 bytes (`s_ram_near` 0x3fc8f8e0). A scratch build run in the
emulator reaches DONE with status ok.

What the rows separate, with the model's reading (the record the rebuild writes) and what each
rival rule would print:

| Rows | Model (step 3i run rule) | Whole-line rule | Entry-word rule | Hits wait for a fill in progress |
|---|---|---|---|---|
| `fetch_end_k*` cold - warm | 41779 to 41887, flat | the same | falls with k (the CPU restarts at word 0) | the same as the model |
| `fetch_mid_k*` cold - warm | 39888 (k 0) falling to 25541 flat from k 128 (10 + word 3's 189.6 cycles a line) | flat at about 41.8 k | falls further, flat near 13.4 k (10 + 94.4 a line) | the same as the model |
| `fetchf_mid_k256` cold - warm | 25712 (as the IRAM driver) | about 41.8 k | about 13.4 k | about 41.8 k |
| `fill_workf_use_k96` cold | 63539 (as `fill_work_use_k96`) | | | higher by up to the transfer's rest a line |
| `fetch_end_k*` against the streaming CPU | the 15 `c.addi` run after word 7 arrives | | | a CPU that runs words as they arrive reads about 15 cycles a line less |
| `dres_*` | 3071 in every cell (no residue modelled) | | | the device's pattern gives the address rule |

**Part B, the application phases** (the boot fit's `lvgl task -> lvgl ready` 52 against 57 ms and
`gauge profile -> ble init` 290 against 320 ms). Attributed with `m11_phase_profile` (new,
ignored; the `pk` boot in 5 µs slices, public machine API only, so the boot is the boot fit's) and with
temporary counters in the cache account and the costed loop (not committed):

| Phase | Model | What the time is | Carrier |
|---|---|---|---|
| lvgl task -> lvgl ready | 52.1 ms | 5 ms LVGL init, then 16 flushes of the 240 x 20 single buffer (9600 bytes, 1.92 ms at 40 MHz each): 1.15 ms render then 1.85 ms spinning in `wait_for_flushing` (27.3 ms in all); 3.30 M instructions, 94 % flash code; 234 k fetch line changes, 6.5 k fetch misses (9.9 ms of stall) | single buffer, so the CPU and the SPI2 transfer add; SPI2 is `spi2_153600` -0.0 % (`probe_timing`), so the 5 ms is the CPU part, about 20 % |
| gauge profile -> ble init | 290.3 ms | 99.5 ms idle (the battery driver's timed wait, then one I2C transaction at 99.1 ms, `s_i2c_start_end_command`), 87.2 ms UI build (`pk_ui_init`, LVGL object and style code, `get_prop_core`, `lv_event_send`), 95 ms rendering with 28.8 ms of it in `wait_for_flushing`, 7.8 ms NVS reads (`spi_flash_hal_poll_cmd_done`); 14.5 M instructions, 96 % flash code; 2.23 M fetch line changes (887 k to the next line), 30.5 k fetch misses (47 ms of stall) | the idle, the I2C (`i2c_read_100` -1.4 %), SPI2 and flash read (`flash_read_64k` -2.2 %) parts are measured elsewhere, so the 30 ms falls on the 190 ms of CPU work, about 16 % |

The model's share of each candidate, each run through the `pk` boot as a temporary switch (lvgl,
gauge to BLE, buttons; device 57, 320, 6; committed 52, 290, 11):

| Candidate | lvgl | gauge -> ble | buttons | Evidence |
|---|---|---|---|---|
| a fetch entering a warm line costs 1.0 / 1.73 / 2.6 cycles | 53 / 54 / 55 | 303 / 310 / 326 | 13 | `cache_code_warm` +111 cycles over 64 entries (1488 against 1377), but its loop loads the table from Block 1 DRAM in IRAM code, the residue step 3g left C, so the 1.73 is an upper bound; no other capture enters a warm line |
| the same, jumps only (2.0 cycles) | 53 | 307 | 12 | as above |
| whole-line fetch rule | 53 | 297 | 13 | rejected on `cache_code_cold` (step 3i) |
| a fetch hit waits for a fill in progress | 53 | 292 | 7 | none; the next miss already waits for the transfer, so it adds little |
| a data hit waits likewise | 52 | 291 | 11 | none |
| DRAM contention from flash code | | | | rejected: `cpi_flash_*` read the model to the cycle |
| SPI2 flush time | | | | `spi2_153600` -0.0 %; the flush is 9600 bytes at the same clock |

Implemented: nothing. The one mechanism that moves both phases the right way by about the right
amount, a cost for entering a warm cache line (about 2.3 cycles closes both within 3 ms), has no
capture that isolates it: `cache_code_warm` mixes it with a Block 1 DRAM load, and every
`cpi_flash_*` loop stays in one line. Class C, with the settling rows appended to the probe:
`TIME|wline_<seq|jump|jump_in>[_iram]` (64 calls of warm flash code crossing 15 lines straight
through, 15 lines by jumps, or one line, and IRAM twins; the model reads 16832, 16832, 3456, 3456,
3456), the warm columns of `fetch_*` (an IRAM call into a warm line and back, no DRAM table) and of
`fetchf_*` (flash to flash), and `TIME|cpi_flash_x_<lbu|lhu|sb|sh|sw_lw_same|sw_lw_next>` (byte
and halfword accesses, 3.6 % of the phase's flash instructions, and a load right after a store,
none timed before; the model reads 3071 and 5119). `seq - seq_iram` is one entry and 15
sequential crossings, `jump_in - jump_in_iram` one entry and one jump, `jump - jump_in` 14 jumps.

### Step 3k: the fetch reads ahead, SRAM Block 1's banks, the application phases

From `device-probe_campaign_timing-20260925T100555Z-run{1,2}` (identical on every row below; the
recorded build, ELF `222fdc5f...`), read with the probe's source and ELF.

**Task 1, the fetch reads one word ahead.** `fetch_end` cold is flat at 42.1 to 42.2 k cycles for
k = 0 to 128 and rises only at 256: the next line is already in transfer when the call reaches
it. It is not the cache's autoload: the app's `EXTMEM_ICACHE_AUTOLOAD_CTRL` reads 0x8 (`ENA`
clear) and both sections' address and size read 0 (the capture's `REG|extmem.*AUTOLOAD*.app`
lines). The rule: a fetch that enters a line runs its straight-line run there as the words
arrive, and the run's last instruction does not complete before the word after the run's last
word has arrived; after a jump in word 7 that word is word 0 of the next line, whose read misses
and starts its fill (asked when execution reaches word 7). Trigger: the fetch unit's next-word
read; distance: one word; data reads: none (`fill_work_use_k96` pays the fill's rest, 104
cycles a line). `fetch_mid`'s rise at 128 and 256 is the same rule: its read-ahead is word 4 of
the same line, so each line costs its own miss once the work outlasts the fill period.

**Task 2, the Block 1 banks.** `dres_*` read 3583 or 3839 by the parity of (code offset + data
offset) / 8. SRAM Block 1 is two banks on address bit 3; code is fetched in aligned 8-byte chunks;
a chunk entered in sequence costs `sram_bank_cycles` (1) when every op since the previous chunk
entry was a one-cycle Block 1 data access in its bank, and a redirect target costs 1 when it is
itself such an access and a back-to-back pair's second access is among the three ops before it.
The 40-byte `dres` loop has five chunks alternating from its head's bank: 3 or 2 pay. The original
kernels: in this build `ram_word` is 0x3fc8f8e8 (bit 3 set) and both read 3838, the model's; in
step 3g's capture (`20260924T234712Z`) they read 3583 and 4094, and a scratch rebuild of that
build (IDF 5.5.3, never flashed) puts `ram_word` at 0x3fc8de20,
bit 3 clear, where the rule charges +2 and +3 an iteration: 3583 and 4094. A fetch-buffer model
(depth, latency, priority) was searched first and over-charged the sparse loops wherever it
matched the dense ones.

| Rows | Device | Before | After | Class |
|---|---|---|---|---|
| `fetch_end_k0..k128` cold | 42110 to 42238 (flat) | 44932 to 61236 | 41906 to 42033 (-203 to -205, -0.5 %) | A |
| `fetch_end_k256` cold | 48702 | 77619 | 49388 (+1.4 %) | A |
| `fetch_mid_k0..k64` cold | 41658 to 41784 | -32 to +26 | -6 to +51 (+0.12 %) | A |
| `fetch_mid_k128` / `_k256` cold | 47103 / 63488 | 43846 / 60229 | 47137 / 63521 (+34, +33) | A |
| `fetchf_mid_k0` / `_k64` / `_k256` cold | 41719 / 41705 / 63629 | 41772 / 41735 / 60400 | 41770 / 41758 / 63662 | A |
| `fill_workf_use_k32` / `_k96` cold | 42056 / 63744 | 41809 / 63539 | 41782 / 63540 (-0.65 %, -0.32 %) | B |
| `fill_work*` cold (six lines) | 21428 to 63488 | +2 to +83 | +2 to +55 | A |
| `cache_code_cold` / `cache_data_cold` | 21010 / 83364 | 20869 / 83495 | 20897 / 83468 (-0.5 %, +0.1 %) | A |
| `aes_cbc_16` / `_1024` / `_4096` / `_16384` | 68652 / 9309 / 13137 / 9279 | -2.9 % / -9.5 % / -10.7 % / -13.0 % | -2.7 % / -6.0 % / -8.7 % / -12.7 % | C |
| `dres_<load\|store>_c*` (162 cells) | 3583 or 3839 by parity | 3071 | 3583 or 3838 | A (-1 on the 3839 cells) |
| `cpi_load_dram`, `cpi_store_dram`, `cpi_gap_<load\|store>_gap0` | 3838 each | 3071 | 3838 each | A |
| `cpi_at_<load\|store>_dram` static and heap_low, `cpi80_<load\|store>_dram` | 3583 / 4094 | 3071 / 3326 | 3582 / 4093 | A (-1) |
| `cpi_at_load_dram_use` Block 1 | 7679 | 7422 | 7422 | C (-257) |
| `cpi_at_load_dram_use_gap1` Block 1 | 8191 | 7167 | 7167 | C (-1024) |
| `cpi_gap_<load\|store>_gap3` | 9471 | 9215 | 9215 | C (-256) |
| `cpi_x_load_store_data` | 7423 | 7167 | 7167 | C (-256) |
| `wline_seq` / `_iram` / `jump` / `jump_in` / `jump_in_iram` | 16832 / 16832 / 3459 / 3460 / 3456 | 16832 / 16832 / 3456 / 3456 / 3456 | the same | A (line entry refuted) |
| `sha256_64k_prefilled` | 171806 | 170044 (-1.0 %) | 170910 (-0.5 %) | B |

`t1_campaign_fetch_ahead_and_block_1_bank_rows_match_the_device_capture` pins these. The
step 3g test does not compare `cpi_load_dram` and `cpi_store_dram` across builds, and its
`sha256_64k_prefilled` check reads the capture of the recorded build.

**Task 3, line entry and the application phases.** Line entry is refuted: the `wline_*` rows read
the model's within 4 cycles over 64 calls, so step 3j's candidate is closed. `m11_phase_profile`
before and after (device, model; stall is time neither executing nor idle):

| Phase | Device | Before | After | Stalled before / after |
|---|---|---|---|---|
| lvgl task -> lvgl ready | 57 | 52 | 53 | 9.91 / 10.56 ms |
| gauge profile -> ble init | 320 | 290 | 293 | 47.25 / 49.85 ms |

The read-ahead adds stall where the next word or line is still in flight; the bank rule moves no
phase by a millisecond (the application's Block 1 IRAM code rarely runs dense Block 1 accesses).
The remaining -4 and -27 ms still fall on flash-resident CPU work, and every warm-code and
cold-fill mechanism the probes measure now reads within 1 %. What no capture has measured is the
cache's replacement: the model is an exact 8-way LRU. A scratch run of the boot with the LRU cut
to 6 ways reads lvgl 60 and gauge to BLE 363 ms, and to 4 ways 69 and 519 ms: the phases are
highly sensitive to it, and a policy that keeps fewer of the recently used lines than LRU would
close them from the side the residue points to.

Next capture (probe source, captured in step 3l): `TIME|ways_<cyclic|mixed>_n<n>`, n = 4,
8, 9, 10, 12 and 16 flash functions of one `ret` each, each alone in its 32-byte line and 2 KB
apart (the same set of the 64), called from IRAM with interrupts off, 64 passes in a cyclic order
and in a fixed pseudo-random order, cold then warm, cycles per pass; and the same with half the
lines read as data (`lw` of a word in the line) to show whether code and data share the ways. An
8-way LRU misses nothing at n <= 8 and everything at n = 9 cyclic; tree pseudo-LRU, FIFO and random
replacement give distinct counts at 9 and 10. The pk path is where the answer lands: its
`gauge profile -> ble init` makes 2.23 M line changes and 30.5 k misses.

**Task 4, refit.** The boot fit (`t1_m11_calibrate_fits_the_committed_device_profile`): fit RMS 13.61 to 12.21 ms, validation RMS 1.89 to 1.89 ms,
every phase inside its band (bootloader segment phases unchanged). The anchor check re-met (16 validation
phases, 22 absolute anchors). The delta check: `flash_read_64k` -2.2 %, `spi2_153600` -0.0 %,
`i2c_read_100` -1.4 to -1.3 %, `sha256_1m` +0.1 %. Cycle-count pins: LAT steady 194 against 172
(unchanged), LAT index 0 at its pin, WFI `task_delay` -2.5 to -2.0 % and `timer_poll` -0.7 to -0.2 %. Campaign
records renewed: `probe_campaign_reset` boot1 `timer_us` 2471 to 2478 (device 2631 to 2645),
the other TIMEBASE fields within 7 us, the step 3h bands unchanged; `probe_campaign_radio`
`ble_init` 3682 to 3707 us, HCI Reset 518 us. Host cost: `cargo xtask bench` F1 to F7, `pk-lvgl`
and `rom-boot`, `--repeat 5 --no-record`, 6 runs a side interleaved ABBA against the build before
(load 3.3 to 4.0), all 108 runs 100 % on the performance cluster: busy S median +1.3, +0.8, +1.8,
+1.9, +1.6, +2.3, +1.7, -0.2 and +1.7 % (after against before), worst 100 ms windows -1.1 to +0.5 ms; the F-suites run `fast`, which never enters the costed loop, so this is layout noise.
Every gate meets.

### Step 3l: the cache's replacement policy

**Part A, probe source** (captured after a rebuild). `probe_campaign_timing` appends, after
every existing line, 25 `TIME|ways_*` lines (row `cache_model`): `ways_<cyclic|pseudo|mixed>_n<n>`
for n = 4, 6, 7, 8, 9, 10, 12 and 16, and `ways_retouch_n10`. Each run takes n flash lines of one
cache set, each holding one `ret`: 2048 bytes apart, the way size of the 16 KB, 8-way, 32-byte-line
cache (IDF `esp32c3/rom/cache.h` `MAX_ICACHE_SIZE`, `MAX_ICACHE_WAYS`, `MIN_CACHE_LINE_SIZE`; the
C3's EXTMEM has no register that reports the geometry), so 64 sets, one set a run (sets 0 to 24),
no line touched before its run. A naked IRAM loop, interrupts off, runs a cold pass and then 64
passes: in index order (cyclic); in a fresh Fisher-Yates shuffle each pass from one xorshift32
stream seeded 0x3A5E0000 + n (pseudo); in index order with every odd index a `lw` of a flash
constant line of the same set instead of a call (mixed: whether data shares the ways); and lines
0 to 7, 0, 1, 8, 9 each pass (retouch, the pattern that best separated the four policies of a
search over "fill, re-touch k, add j new lines" and "re-touch line 0 every r accesses"). Each line
prints the cold pass's and the 64 passes' cycles, the EXTMEM IBUS and DBUS access and miss counters
(`EXTMEM_*_ACS_*CNT`, read-only, read before, between and after; plain storage reading 0 in the
emulator), the set, the stride, an FNV-1a 64 of the access order (`order`) and the line addresses.

Nothing already in the probe moves: the new flash code is in `.irom0.text` and the new constants
in `.rodata1`, which the IDF linker script places after all other flash text and rodata (a
2048-byte-aligned block would have moved `_stext` itself); the order buffer is a heap block; the
one IRAM function is 240 bytes, so the IDF IRAM code after it moves by a multiple of 16 and IRAM's
end stays in its 512-byte block (no DRAM datum moves, `ram_word` stays 0x3fc8f8e8). The call in
`app_main` moves the IDF flash code after it by 4 bytes. Checked on a scratch build's ELF against
the pinned one (`222fdc5f...`): every probe kernel, table and datum at its address. The scratch
build reaches DONE with status ok in the emulator; its existing lines read the record's within 3
cycles or 3 us (the RSA lines within 0.001 %), except the layout-sensitive `aes_cbc_*` (-2.1 % to +4.8 %, class C),
`sha256_64k_prefilled` (+0.2 %) and `slow_clk_next_edge` (cycles0 and cycles1).

What each policy predicts, from `t0_campaign_replacement_policies_predict_the_ways_lines`
run on the scratch build's record: warm misses of the 64 passes, and in brackets the cycles the
line would print (a hit at the cost of the `n4` lines, a miss by the `device` fill timing as
`cold::CacheAccount` charges it; the LRU column gives the emulator's own cycles within 0.04 %).
Tree pseudo-LRU is the same from all 128 initial states on every line; random is the mean of 1000
seeds, its range in the test's output. Every policy misses each line once in the cold pass.

| Line | Accesses | True LRU (model) | Tree PLRU | FIFO | Random (mean) |
|---|---|---|---|---|---|
| `cyclic_n4` / `n6` / `n7` / `n8` | 256 / 384 / 448 / 512 | 0 (3839 / 5758 / 6718 / 7678) | 0 | 0 | 1.1 / 3.7 / 6.7 / 13.5 (4161 / 6854 / 8651 / 11486) |
| `cyclic_n9` | 576 | 576 (188006) | 576 | 576 | 135.2 (44153) |
| `cyclic_n10` | 640 | 640 (208896) | 640 | 640 | 249.8 (81542) |
| `cyclic_n12` | 768 | 768 (250675) | 768 | 768 | 461.3 (150577) |
| `cyclic_n16` | 1024 | 1024 (334234) | 1024 | 1024 | 829.1 (270613) |
| `pseudo_n4` to `n8` | as cyclic | 0 | 0 | 0 | as cyclic |
| `pseudo_n9` | 576 | 182 (59405) | 155 (50592) | 100 (32649) | 115.8 (37850) |
| `pseudo_n10` | 640 | 295 (96288) | 278 (90739) | 201 (65606) | 212.2 (69288) |
| `pseudo_n12` | 768 | 533 (173971) | 523 (170707) | 439 (143305) | 405.6 (132392) |
| `pseudo_n16` | 1024 | 851 (277766) | 845 (275808) | 812 (265037) | 737.7 (240780) |
| `mixed_n<n>` | as cyclic | as cyclic (3711, 5566, 6526, 7422 at n <= 8; 188006 to 334234 above) | as cyclic | as cyclic | as cyclic |
| `retouch_n10` | 768 | 512 (167117) | 384 (125338) | 640 (208896) | 272.4, 245 to 298 (88936) |

The retouch line alone names the policy: 512, 384, 640 and 245 to 298 misses a run, 40 to 80 k
cycles apart. If the data lines had ways of their own, every mixed line would hit throughout (at
most 8 code and 8 data lines), about 14.5 cycles an access against the table's 326 a miss; if the
EXTMEM counters run on silicon, `ibus_miss` and `dbus_miss` read the counts directly.

**Part B, the capture** (`device-probe_campaign_timing-20260925T133649Z-run{1,2}.log`, lines 265 to
289, run1 and run2 identical). The EXTMEM counters count on silicon, and
the warm IBUS misses are the FIFO column exactly: cyclic n9 to n16 576, 640, 768, 1024; pseudo n9,
n10, n12, n16 100, 201, 439, 812; retouch 640; none at n <= 8. `ways_mixed_n9` reads 320 IBUS and
256 DBUS misses, FIFO's split of one shared set, so data and code share the ways. A cold pass
misses each line once; the IBUS access counter reads 2 per call into a line, the DBUS one 1 per
read. `t1_campaign_fifo_replacement_matches_the_device_capture` checks all of this against
the capture (the orders equal the test's, the counters equal FIFO's fetch and data misses, and on
`retouch_n10` true LRU, every tree PLRU state and every random seed disagree).

The emulator adds `fifo16k` (`CacheVariant::Fifo16k`): `lru16k`'s geometry, fill timing and fetch
rule, with a hit changing nothing and a miss dropping the way filled longest ago. The per-set way
order already held is the FIFO order, so `CacheState` and `FORMAT_VERSION` (27) are unchanged.
`specs/timing-profiles.toml` `cache_model` `device` is `fifo16k`, class A. The EXTMEM counters stay
storage reading 0: under `fast` there is no line account, the IBUS access count is of fetch-unit
word requests the line account never sees, and EXTMEM has no path to the SoC's account through `Cx`.

| Line | Device | `lru16k` | `fifo16k` |
|---|---|---|---|
| `ways_cyclic_n9` / `n10` / `n12` / `n16` | 187755 / 208619 / 250347 / 333803 | +0.12 % | +0.12 % |
| `ways_pseudo_n9` | 32587 | 59384 (+82.2 %) | 32629 (+0.13 %) |
| `ways_pseudo_n10` | 65505 | 96267 (+47.0 %) | 65585 (+0.12 %) |
| `ways_pseudo_n12` | 143108 | 173950 (+21.6 %) | 143284 (+0.12 %) |
| `ways_pseudo_n16` | 264691 | 277745 (+4.9 %) | 265016 (+0.12 %) |
| `ways_mixed_n9` to `n16` | as cyclic | +0.12 % | +0.12 % |
| `ways_retouch_n10` | 208619 | 167096 (-19.9 %) | 208875 (+0.12 %) |
| `ways_*_n4` to `n8` | 3840 to 7680 | -0.03 to -0.01 % | -0.03 to -0.01 % |
| `aes_cbc_1024` cycles | 8911 | 8470 (-4.95 %) | 8889 (-0.25 %) |
| `aes_cbc_16384` cycles | 9139 | 8101 (-11.4 %) | 8667 (-5.2 %) |
| `fill_workf_use_k32` cold | 42056 | 41782 (-0.65 %) | 42109 (+0.13 %) |

The +0.12 % of every missing line is the miss period (326.4 cycles against 326.0). The cold passes
read within 0.8 % but for the section's first, `cyclic_n4` (-13 %, the same under `lru16k`). Every
other row of the three campaign records stays within 0.15 % of its `lru16k` value.

The refit, `device` profile with `fifo16k`:

- The boot fit RMS 12.21 to 2.32 ms, validation RMS 1.89 to 1.87 ms; `lvgl task -> lvgl ready` 53 to
  56 ms and `gauge profile -> ble init` 293 to 315 ms (device 57 and 320). `ble_enable_nvs_cal_ps`
  fits at 41 ms; 40.5 kept.
- The anchor check: 16 validation phases and 22 anchors inside, none excluded, no band widened; largest
  residuals +6 ms (`app ready -> buttons`, band +/-8) and -4 ms. The delta check: -2.2 / -0.0 / -1.3 / +0.1 %.
  Cycle-count pins inside (LAT 194 against 172, WFI -2.0 % and -0.2 %).
- Records (`CAMPAIGN_RECORD=1`): `probe_campaign_timing` as above; `probe_campaign_reset` boot1
  `timer_us` 2478 to 2526 (device 2631 to 2645); `probe_campaign_radio` `ble_init` 3707 to 3858 us.
- Host cost, ABBA against the build before, 6 runs a side on the performance cluster (105 of 108 runs
  at 100 %, none below 99.9 %): busy S median -1.4 to +1.7 % across F1 to F7, `pk-lvgl` and
  `rom-boot`, worst 100 ms windows -0.50 to +0.18 ms; noise (F-suites run `fast`).

### Step 4: the inventory sweep

**Part A, the sweep.** Every row of sections 1 to 6 was read again against the captures under the
data root's `captures/` and the probe source that printed each line (what the line measures, not
the row its `row=` field names), and each cited fact against the committed emulator record. The
disposition cells now say **settled**, **open**, **cannot** or **untouched** (the vocabulary and
the counts are at the top), and a settled cell cites its lines as
`<alias>:<line> <TAG|fact>`, with `~<field>:<pct>%` for a measured field the record matches
within that share of the device and `~<field>:*` for a field that measures the capture rather
than the chip (a press's length, a sample's phase), which the cell names. The aliases:

| Alias | Capture | Build |
|---|---|---|
| `regs` | `device-probe_campaign_regs-20260924T164141Z-run1.clean.log` | the pinned build (step 2) |
| `timing` | `device-probe_campaign_timing-20260925T133649Z-run1.log` | the pinned build (step 3l) |
| `reset` | `device-probe_campaign_reset-20260924T232751Z-run1.log` | the pinned build (step 3f) |
| `radio` | `device-probe_campaign_radio-20260924T155729Z-run1.log` | the pinned build (step 2) |

`t1_campaign_every_settled_row_names_a_capture_line` keeps the link: every row has a step
4 disposition, every open row names its Part B line, the step 4 count table's Total row
is what the rows say, every settled row cites at least one line, and, with the data root, each
cited line is that fact at that line of that capture and the record prints it with every field
equal but the `~` fields, within their tolerance (167 lines on 106 rows). `cargo xtask probes
compare` keys a fact by its `data=` placement too, since the `cpi_at_*`, `cpi_gap_*` and
`cpi_flash_*` kernels print one fact per operand placement (`probes::compare::tests::a_placed_fact_is_one_fact_per_placement`).

What moved: no class, in this step. Every row whose whole claim a capture shows was promoted by
step 3 already (B21, B55, B56 and B111 to A; B57 and B65 to B; T1 to T4 and T10 to A, T9
to B; V21's new pad row A, V49 A; H6, H8, H9 A, H7 B), and the sweep found each of them still
matched by the record. The 83 rows settled with **C kept** read the device's value on every word
the corpus reads, cited, and their C is an effect the model omits by declaration that no read
shows: the effect lands on hardware the model replaces (radio, power domains, analog, cache
power), shows only on a path no corpus image takes (a cache fault, a memory-protection violation,
a routed crossbar signal, a program-erase suspend), needs a write the rules forbid or happens
before app code (an eFuse image, a flash program, FLASHBOOT_MOD_EN, FIB_SEL bit 2), or is the
`fast` profile's. Promoting them would claim what no capture tested; each cell says
which. The **cannot** and **untouched** rows were read again as well: no capture line reaches any
of them, so all 96 stay (the appendix's access types gain a partial check, not counted: the
`MASK` and `GATE ... released` lines read the writable bits of SYSTEM_BT_LPCK_DIV_INT and _FRAC,
I2S_TX_TIMING and AES_KEY_0, all equal).

The 11 rows left open, each reachable by a probe:

| Rows | What no capture shows | Part B line |
|---|---|---|
| B58, B60 | the RTC_CNTL words after a reset that restores the RTC domain: the regs capture read TIMER1 and CLK_CONF after a core reset that followed the reset probe's deep sleep, so they are that sleep's residue ("Step 3: the register rows") | `probe_campaign_reset` `REG\|rtc_cntl.<word>.boot2` and `.boot3` |
| B70, B72, B74 | the one-shot conversion time the model takes as zero | `probe_campaign_regs` `TIME\|adc_oneshot_read` |
| B108, B109, B110 | the clock-off mechanism (the last value read, the value written, or the writable bits) and the gate on any block but I2S0 and AES | `probe_campaign_regs` `GATE\|<block>_latch`, and the gate experiment on LEDC, I2C0, SPI2 and SHA |
| B116, B118 | whether TIMG_REGCLK's CLK_EN clear stops the counters (register access with it clear is equal) | `probe_campaign_regs` `GATE\|timg<n>_regclk_count` |
| H4, `hle.wifi.connected_us` | the association time with an open access point | `probe_wifi_assoc`, no new source |

**Part B, the probe lines** (probe source; captured in step 5). Both probes append after every
existing line of their output; neither moves a kernel (the timing probe, the only one with timed
kernels, is unchanged). Built in a scratch directory (IDF v5.5.3, the device's partition table) and run in the emulator (`campaign_run_image`, `device` profile), both
reach `DONE|` with status ok, and every existing line reads the committed record's: the regs probe
to the bit, the reset probe but its microsecond fields (TIMEBASE within 15 us, `SWD|timeout`
`alive_us` +15 us: the app's code moved and the store runs before the host wait).

- **`probe_campaign_regs`**, after the button window: `GATE|i2s0_latch` and `GATE|aes_latch` (the
  block pulsed out of reset, `p1` written and read, `p3` written and not read, the clock gated and
  the register read, `p2` written gated and read, the clock back on and read); the four-line gate
  experiment and the latch line on LEDC (`LEDC_LSCH0_HPOINT`), I2C0 (`I2C_SCL_LOW_PERIOD`), SPI2
  (`SPI_MS_DLEN`) and SHA (the first word of its message memory), which the app does not use;
  `GATE|timg<n>_regclk_count` (general timer 0 counting at 1 MHz, latched after 200 us, then 200 us
  with CLK_EN clear, then at once and 200 us after CLK_EN is set again, each latch's request
  bounded); `TIME|adc_oneshot_read` (64 driver reads, cycles and us). The emulator prints: the
  I2S0 and AES gated reads return `p1`, the last value read (the model's latch), and the gated
  write is dropped; LEDC, I2C0, SPI2 and SHA hold reset as the rule says and store every gated
  write (no latch modelled); the TIMG counters run on with CLK_EN clear (202, 405, 407, 607 and
  200, 400, 401, 601); 64 reads take 1352 us, all driver work.
- **`probe_campaign_reset`**: the 19 RTC_CNTL words of `reg_facts.h`, read at app_main of boot2
  (after the super-watchdog `SYS_` reset) and boot3 (after the wake), before the host wait, kept in
  RTC memory and printed at the end of boot3 as `REG|rtc_cntl.<word>.boot2` and `.boot3` (rows
  the word's own and `rtc_cntl.reset_domains`). The emulator prints boot2's TIMER1 0x14140143,
  CLK_CONF 0x30c80298 and GPIO_WAKEUP 0, the power-on values its `SYS_` reset restores, and boot3's
  0x14190143, 0x20c80298 and 0x02000081, the regs capture's: the residue reading of "Step 3: the register rows",
  predicted.

Capture procedure:

1. `cargo xtask probes` for `probe_campaign_regs` and `probe_campaign_reset`, then
   `CAMPAIGN_RECORD=1` for their records. Until that rebuild only the source hashes of the three
   changed files move in `tests/fw/manifest.toml`; the ELF, app and merged hashes pin the builds
   the records and the captures above are of. The timing
   and radio images are unchanged.
2. Flash `probe_campaign_regs` and capture it as before. **A person at the device** presses Up,
   then Down, then OK, about a second each, when the `NOTE|press` line appears (8 s window), as in
   step 2; the Part B lines follow the window with nothing to do.
3. Flash `probe_campaign_reset` and capture it with the reconnecting capture across both resets
   to `DONE|`, as before; no button (pressing none is right: GPIO0 wakes the chip at once anyway).
4. H4, when an open access point is at hand: **a person** sets up an access point with no
   password near the device; the operator builds `probe_wifi_assoc` with that SSID and an empty
   password in the overlay kept outside the checkout, flashes it under the device rules,
   masks the SSID, MAC and IPv4 in the capture, and restores Passport Keys (cardid MD5 compared).
5. `cargo xtask probes compare` each capture against the renewed record. Each open row then
   settles on its line: B58 and B60 on the boot2 words (and boot3 against the regs capture);
   B70, B72, B74 on the read time less the model's 1352 us; B108 to B110 on the latch and block
   lines; B116 and B118 on `off - on` against 200.

Found on the way:

- **T10 `ble_init_ps` reads +2.7 % after step 3l.** The record's `TIME|ble_init` went from 3707 to
  3858 us under `fifo16k` (device 3757 and 3760): the constant is the device time less the HLE's
  guest work, and the guest work grew with the cache policy. Refitting it (about 2139 us) moves a
  timing value, so it is left to step 5; the row stays A with the cell's 3 %.
- **B17's row text is stale**: "the emulated cache fills a whole 64 KB page through the MMU" predates
  the line account; the value and the class are right.
- The inventory has 116 probe rows (99, 6, 5 and 6 in sections 1, 2, 4 and 5), not 118, and T11
  was settled when step 3 added it.

### Step 5: the Part B rows

Both Part B probes were captured on the Part B build
(`captures/campaign-step2-2026-09-24.notes.md`). Their aliases:

| Alias | Capture | Build |
|---|---|---|
| `regsb` | `device-probe_campaign_regs-20260926T024930Z-run1.clean.log` | the Part B build |
| `resetb` | `device-probe_campaign_reset-20260925T174549Z-run1.log` | the Part B build |

Ten of the eleven open rows settle on them; H4 (the association time with an open access point)
stays open. `t1_campaign_gate_and_adc_rows_match_the_device_capture` pins the lines:

- **B108 to B110, the clock gate per block** (`crates/pemu-soc-c3/src/wiring/gates.rs`). The
  latch lines read, with the clock gated after `p1` was read and `p3` written: I2S0, AES and SPI2
  `p1`, the last value read (regsb 405, 406, 421); LEDC and I2C0 `p3`, the stored value (411,
  416); SHA 0 (426); every gated write dropped whole and `p3` held once the clock is back. The gate
  experiment agrees (410, 415, 420, 425): SPI2's gated read returns 0x3ffff, the writable bits it
  read last, while it holds 0, which settles step 3's latch reading against "the writable bits
  read 1". The model now answers a gated read by the block's rule (`OffRead::Latch`, `Stored`,
  `Zero`) and drops the write; SPI2 has a latch slot of its own (`FORMAT_VERSION` 28). The blocks
  no capture gates keep answering with their clock off, UNVERIFIED, which makes the three rows B
  rather than A; the held-reset lines of LEDC, I2C0 and SPI2 (408, 413, 418) read 0 on both sides,
  so the reset rule is the device's on 7 of the 15 blocks it covers.
- **B70, B72, B74, the one-shot time** (`adc_conversion_ps`, class B). 64 reads take 3029 us
  and 484360 cycles on the device (regsb 429) and took 1352 us and 216122 cycles with the
  conversion at zero. IDF v5.5.3's `adc_oneshot_read` does, per read: the lock, the clock and the
  unit setup, `adc_hal_calibration_init` and `adc_set_hw_calibration_code` (three regi2c
  read-modify-writes), the 3 us `esp_rom_delay_us` of `adc_hal_onetime_start` (the controller
  clock is APB / 16, 5 MHz, below APB / 8), the done poll, and no delay after it
  (`ADC_LL_DELAY_CYCLE_AFTER_DONE_SIGNAL` 0). The emulator runs all of it at the committed cycle
  costs, so the 26.2 us a read left is charged from the rising edge of `onetime_start` to the done
  bit: 26.24 us reads 3026 us and 484026 cycles (the driver's 13-cycle poll moves the total in
  steps of 832 cycles; the next step reads 3031 and 484858). At 5 MHz that is 131 controller
  cycles, of which `FSM_WAIT`'s standby, reset and power-up waits (100, 8, 5) would be 113,
  UNVERIFIED. The constant also takes in anything else on the path the model times as zero (the
  regi2c `BUSY` bit reads 0 at once); no capture separates them. The `fast` profile keeps the
  same-access completion; the three rows stay C kept, since their fields are stored and not
  applied (the time is one number at the driver's setting). The press medians are unchanged
  (3, 393, 782; the device 3, 394, 782).
- **B116, B118, TIMG_REGCLK** (A). General timer 0 counts on with CLK_EN clear: `off - on` is
  205 and 200 us of the 200 on the device (regsb 427, 428) and 203 and 200 in the model, every
  latch request clearing. The rows' claim is the silicon fact; `specs/blocks/timg0.toml` and
  `timg1.toml` say so.
- **B58, B60, the RTC_CNTL words** (C kept). All 19 words after the super-watchdog `SYS_` reset
  and after the wake equal the model's (resetb lines 81 to 99 after the reset, 100 to 118 after
  the wake): the regs capture's TIMER1 and CLK_CONF were the deep-sleep residue "Step 3: the register
  rows" named. What the rows declare (wait counts that pace nothing, unhonored clock selectors) stays C.

Two leftovers of step 4, done here:

- **T10 `ble_init_ps`** refitted from 2240 to 2140 us: the HLE's guest work in the call is 1618 us
  since the `fifo16k` cache (1518 when the row was fitted), and the probe reads 3758 us against
  the device's 3757 and 3760. `ble_deinit` reads 1024 us against the device's 1034 (-1.0 %) for
  the same reason; its row is left.
- **B17**: `specs/blocks/extmem.toml`'s text is step 3i's in-order line fill, not the 64 KB page.

After the step the boot fit fits the committed `device` profile at fit RMS 2.32 ms and validation RMS 1.89 ms (`fifo16k`), and the anchor check finds its 16 validation phases and 22 absolute anchors inside the bands (`app ready -> buttons` 12 to 13 ms against the silicon band 6 +/- 8).

## 1. Class C and U block rows

Every `[[overrides]]` row of class C and every block header of class U in `specs/blocks/*.toml`,
in file order. "Assumed" is the row's value (for a header, its provenance), cut at about 170
characters. "Touched by" is the ledger: the images whose run reached a register of the row, and
whether the first access was a read or a write.

| # | Row | Class | Assumed | Touched by | Disposition |
|---|---|---|---|---|---|
| B1 | `apb_ctrl.SYSCON_RND_DATA` | C | a word from the DetRng stream of this block, not a host random value | all 28 (first R) | **settled** (C kept): 16 reads, 16 distinct and no zero on both sides, `regs:380 RND\|apb_ctrl.SYSCON_RND_DATA`. C by design: a seeded stream instead of entropy (DetRng); no read can move it |
| B2 | `apb_ctrl.SYSCON_WIFI_RST_EN` | C | stored; the write reaches nothing, because the emulator models no Wi-Fi or BT MAC to hold in reset | all 28 (first R) | **settled** (C kept): the value equal, `regs:56 REG\|apb_ctrl.SYSCON_WIFI_RST_EN`; the effect is on the radio the HLE replaces, not a register fact |
| B3 | `apb_ctrl.SYSCON_FRONT_END_MEM_PD` | C | stored; the radio front-end memories it powers down are not modeled, so the guest sees no difference | all 28 (first R) | **settled** (C kept): the value equal, `regs:57 REG\|apb_ctrl.SYSCON_FRONT_END_MEM_PD`; the effect is on the radio the HLE replaces, not a register fact |
| B4 | `apb_ctrl.SYSCON_MEM_POWER_UP` | C | stored; every emulated memory is always powered, so the power-up request has nothing to do | all 28 (first R) | **settled** (C kept): the value equal, `regs:58 REG\|apb_ctrl.SYSCON_MEM_POWER_UP`; the model has no memory power domain, and powering one down under running code is not a safe probe step (cannot) |
| B5 | `cw2017.SOC` | C | 0xFF for 1 s of virtual time after a guest write of CONFIG 0x00 starts a computation, then the battery model through a first-order lag; the provisioned start state has ... | no ledger (an I2C device); the row cites the products' bsp_battery_init | **cannot**: the window opens only after a guest write of CONFIG 0x00 to the gauge, a write to the battery-domain part the device rules do not allow in this step |
| B6 | `dedicated_gpio` | U | a deferred block (dedicated_gpio, no boot histogram entry); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED ... | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B7 | `ds` | U | a deferred block (ds; the sha/aes DMA modes, hmac, ds and xts_aes group); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED ... | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B8 | `efuse.EFUSE_RD_REPEAT_ERR0` | C | reads 0; the model performs no repeat-consistency decode of the image | all 28 (first R) | **settled** (C kept): 0 on both sides, `regs:59 REG\|efuse.EFUSE_RD_REPEAT_ERR0`; the missing decode shows only for an inconsistent eFuse image, which needs an eFuse write (cannot) |
| B9 | `efuse.EFUSE_RD_REPEAT_ERR1` | C | reads 0; the model performs no repeat-consistency decode of the image | all 28 (first R) | **settled** (C kept): 0 on both sides, `regs:60 REG\|efuse.EFUSE_RD_REPEAT_ERR1`; the missing decode shows only for an inconsistent eFuse image, which needs an eFuse write (cannot) |
| B10 | `efuse.EFUSE_RD_REPEAT_ERR2` | C | reads 0; the model performs no repeat-consistency decode of the image | all 28 (first R) | **settled** (C kept): 0 on both sides, `regs:61 REG\|efuse.EFUSE_RD_REPEAT_ERR2`; the missing decode shows only for an inconsistent eFuse image, which needs an eFuse write (cannot) |
| B11 | `efuse.EFUSE_RD_REPEAT_ERR3` | C | reads 0; the model performs no repeat-consistency decode of the image | all 28 (first R) | **settled** (C kept): 0 on both sides, `regs:62 REG\|efuse.EFUSE_RD_REPEAT_ERR3`; the missing decode shows only for an inconsistent eFuse image, which needs an eFuse write (cannot) |
| B12 | `extmem.EXTMEM_ICACHE_TAG_POWER_CTRL` | C | stored; the emulated tag memory is always powered, so the power and clock-gate bits reach nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:63 REG\|extmem.EXTMEM_ICACHE_TAG_POWER_CTRL`; gating it under code that runs from the cache is not a safe probe step (cannot) |
| B13 | `extmem.EXTMEM_CACHE_ILG_INT_ENA` | C | stored; the illegal-access interrupt it enables is never raised, so enabling it changes nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:64 REG\|extmem.EXTMEM_CACHE_ILG_INT_ENA`; the interrupt needs a faulting cache access, which IDF's cache error handler turns into a panic; no corpus image makes one (not a corpus path) |
| B14 | `extmem.EXTMEM_CACHE_ILG_INT_CLR` | C | write-only, so it reads 0 as the device does; the clear reaches no raw bit, because none is ever set | all 28 (first W) | **settled** (C kept): reads 0 on both sides, `regs:65 REG\|extmem.EXTMEM_CACHE_ILG_INT_CLR`; as B13 |
| B15 | `extmem.EXTMEM_CORE0_ACS_CACHE_INT_ENA` | C | stored; the core-access interrupt it enables is never raised | all 28 (first R) | **settled** (C kept): the value equal, `regs:66 REG\|extmem.EXTMEM_CORE0_ACS_CACHE_INT_ENA`; as B13 |
| B16 | `extmem.EXTMEM_CORE0_ACS_CACHE_INT_CLR` | C | write-only, so it reads 0 as the device does; the clear reaches no raw bit | all 28 (first R) | **settled** (C kept): reads 0 on both sides, `regs:67 REG\|extmem.EXTMEM_CORE0_ACS_CACHE_INT_CLR`; as B13 |
| B17 | `extmem.EXTMEM_CACHE_WRAP_AROUND_CTRL` | C | stored; the emulated cache fills a whole 64 KB page through the MMU, so the burst wrap mode is invisible | all 28 (first R) | **settled** (C kept): 0 on both sides, `regs:68 REG\|extmem.EXTMEM_CACHE_WRAP_AROUND_CTRL`: no wrap-around, the in-order fill from word 0 read from IDF; the model ignores a guest that sets it, which no corpus image does. The row's "fills a whole 64 KB page", stale since the line account (step 4, found), was rewritten by step 5 to the in-order line fill (`specs/blocks/extmem.toml`) |
| B18 | `extmem.EXTMEM_CACHE_MMU_POWER_CTRL` | C | stored; the MMU entry table is always powered, so the power and clock-gate bits reach nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:69 REG\|extmem.EXTMEM_CACHE_MMU_POWER_CTRL`; gating it under code that runs from the cache is not a safe probe step (cannot) |
| B19 | `extmem.EXTMEM_CACHE_MMU_OWNER` | C | stored; one owner drives the single emulated MMU table, so the ownership mask selects nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:70 REG\|extmem.EXTMEM_CACHE_MMU_OWNER`; one bus master in the model, and the corpus never hands the table to another owner (not a corpus path) |
| B20 | `flash_xmc.0x5A RDSFDP` | C | not reached | no ledger (a flash command, not a register) | **untouched**: the call trace in the row: is_xmc_chip_strict accepts this part, so the bootloader skips the SFDP path; not an MMIO register, so the ledger cannot show it |
| B21 | `flash_xmc.any command at 0x800000 or above` | C, A since step 3 | served like the 8 MB part | no ledger (a flash command, not a register) | **settled** (A, step 3): the cell 8 MB below on both sides, `regs:394 FLASH\|read_0x800000` |
| B22 | `flash_xmc.0x02 PP across a page boundary` | C | wraps inside the 256-byte page | no ledger (a flash command, not a register) | **untouched**: the row: reachable only from a raw SPI1 command with addr % 256 > 192; ROM and IDF chunk at the page boundary. Observing it would need a flash write (cannot) |
| B23 | `flash_xmc.any command while WIP is set` | C | only 0x05, 0x35, 0x66 and 0x99 are answered | no ledger (a flash command, not a register) | **untouched**: the row: every ROM and IDF path polls WIP before each command. Observing it would need a program or erase (cannot) |
| B24 | `gpio.GPIO_FUNC*_IN_SEL_CFG` | C | stored; the input crossbar is not decoded, because every emulated peripheral input comes from its board model rather than from a routed pin | all 28 (first W) | **settled** (C kept): all 128 selectors equal (lines 71 to 198, `t1_campaign_regs_rows_match_the_device_capture`), `regs:71 REG\|gpio.GPIO_FUNC0_IN_SEL_CFG`; the crossbar routing itself is what the board model replaces; a routed signal would retire the row, and no corpus image routes one the board model does not carry |
| B25 | `gpio.GPIO_FUNC*_OUT_SEL_CFG` | C | stored; the output crossbar is not decoded, because every emulated peripheral output reaches its board model directly | demo, goldminer, official, pk, probe-long, probes/probe_timing (first W) | **settled** (C kept): all 26 selectors equal (lines 199 to 224), `regs:199 REG\|gpio.GPIO_FUNC0_OUT_SEL_CFG`; the crossbar routing itself is what the board model replaces; a routed signal would retire the row, and no corpus image routes one the board model does not carry |
| B26 | `gpio.GPIO_PIN*` | C | stored; the per-pin interrupt type, the wakeup enable and the pad hold are not acted on | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): all 26 words 0 on both sides (lines 225 to 250), `regs:225 REG\|gpio.GPIO_PIN0`; the per-pin interrupt and hold matter only to an image with a GPIO ISR or a held pad, which the corpus has not (the row's retirement condition) |
| B27 | `hmac` | U | a deferred block (hmac; the sha/aes DMA modes, hmac, ds and xts_aes group); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED ... | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B28 | `i2c0.I2C_SCL_LOW_PERIOD` | C | stored and read back; honored only under a profile with i2c_clocked (specs/timing-profiles.toml), where it sets the SCL period of the list's bus time; under fast a ... | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:93 REG\|i2c0.I2C_SCL_LOW_PERIOD.100k` `timing:105 REG\|i2c0.I2C_SCL_LOW_PERIOD.400k`; the SCL period paces the bus under `i2c_clocked` (A) and 20 reads land within 2 %, `timing:92 TIME\|i2c_read_20_100k ~us:2%` `timing:104 TIME\|i2c_read_20_400k ~us:2%`; C because `fast` does not clock |
| B29 | `i2c0.I2C_SCL_HIGH_PERIOD` | C | stored and read back; honored only under a profile with i2c_clocked (specs/timing-profiles.toml), where it sets the SCL period of the list's bus time; under fast a ... | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:97 REG\|i2c0.I2C_SCL_HIGH_PERIOD.100k` `timing:109 REG\|i2c0.I2C_SCL_HIGH_PERIOD.400k`; the SCL period paces the bus under `i2c_clocked` (A) and 20 reads land within 2 %, `timing:92 TIME\|i2c_read_20_100k ~us:2%` `timing:104 TIME\|i2c_read_20_400k ~us:2%`; C because `fast` does not clock |
| B30 | `i2c0.I2C_SDA_HOLD` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:95 REG\|i2c0.I2C_SDA_HOLD.100k` `timing:107 REG\|i2c0.I2C_SDA_HOLD.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B31 | `i2c0.I2C_SDA_SAMPLE` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:96 REG\|i2c0.I2C_SDA_SAMPLE.100k` `timing:108 REG\|i2c0.I2C_SDA_SAMPLE.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B32 | `i2c0.I2C_SCL_START_HOLD` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:98 REG\|i2c0.I2C_SCL_START_HOLD.100k` `timing:110 REG\|i2c0.I2C_SCL_START_HOLD.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B33 | `i2c0.I2C_SCL_RSTART_SETUP` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:99 REG\|i2c0.I2C_SCL_RSTART_SETUP.100k` `timing:111 REG\|i2c0.I2C_SCL_RSTART_SETUP.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B34 | `i2c0.I2C_SCL_STOP_HOLD` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:100 REG\|i2c0.I2C_SCL_STOP_HOLD.100k` `timing:112 REG\|i2c0.I2C_SCL_STOP_HOLD.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B35 | `i2c0.I2C_SCL_STOP_SETUP` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:101 REG\|i2c0.I2C_SCL_STOP_SETUP.100k` `timing:113 REG\|i2c0.I2C_SCL_STOP_SETUP.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B36 | `i2c0.I2C_TO` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:94 REG\|i2c0.I2C_TO.100k` `timing:106 REG\|i2c0.I2C_TO.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B37 | `i2c0.I2C_FILTER_CFG` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:102 REG\|i2c0.I2C_FILTER_CFG.100k` `timing:114 REG\|i2c0.I2C_FILTER_CFG.400k`; the hold, setup and filter times are inside the 1 to 2 % the two timed runs leave (step 3b: CPU); not modeled apart |
| B38 | `i2c0.I2C_CLK_CONF` | C | stored and read back; honored only under a profile with i2c_clocked (specs/timing-profiles.toml), where it sets the SCL period of the list's bus time; under fast a ... | demo, goldminer, official, pk, probe-long, probes/probe_timing (first R) | **settled** (C kept): the driver's words at both rates equal, `timing:103 REG\|i2c0.I2C_CLK_CONF.100k` `timing:115 REG\|i2c0.I2C_CLK_CONF.400k`; the SCL period paces the bus under `i2c_clocked` (A) and 20 reads land within 2 %, `timing:92 TIME\|i2c_read_20_100k ~us:2%` `timing:104 TIME\|i2c_read_20_400k ~us:2%`; C because `fast` does not clock |
| B39 | `i2c0.I2C_SCL_ST_TIME_OUT` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B40 | `i2c0.I2C_SCL_MAIN_ST_TIME_OUT` | C | stored and read back; the value is not honored, because a command list runs in zero virtual time | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B41 | `i2s0.I2S_INT_RAW` | C | never raised: stream completion reaches the driver through the GDMA EOF interrupt, not through I2S source 20 | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B42 | `i2s0.I2S_TX_TIMING` | C | stored and read back; pad timing has no effect on a modeled bus | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B43 | `i2s0.I2S_RX_TIMING` | C | stored and read back; pad timing has no effect on a modeled bus | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B44 | `i2s0.I2S_LC_HUNG_CONF` | C | stored and read back; the hung detector never fires, because a period either moves a whole descriptor or none | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B45 | `i2s0.I2S_TX_PCM2PDM_CONF` | C | stored and read back; PDM is off on this board | demo, goldminer, official, probe-long (first R) | **settled** (C kept): the reset value equal, `regs:385 REG\|i2s0.I2S_TX_PCM2PDM_CONF.reset`; PDM is not modeled, and the products leave it off |
| B46 | `i2s0.I2S_CONF_SIGLE_DATA` | C | stored and read back; the constant-data path is not used | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B47 | `intc.INTERRUPT_CORE0_CLOCK_GATE` | C | CLK_EN, bit 0: stored and read back, reset 1; clearing it does not stop the matrix in the model | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B48 | `radio_bb` | U | radio register store (a store plus tripwires, class U); base IDF soc/esp32c3/register/soc/reg_base.h; BBPD_CTRL 0x6001_D054 is the only boot access | all 28 (first R) | **cannot**: undocumented radio registers behind the modem clock, replaced by the HLE; the only corpus access is the boot's rtc_sleep_pu, and a read with the modem clock gated is not known to be safe |
| B49 | `radio_ble` | U | radio register store (BLE baseband around 0x6003_1000, a store plus tripwires, class U); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED, and the BLE ... | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B50 | `radio_fe` | U | radio register store (a store plus tripwires, class U); base IDF soc/esp32c3/register/soc/reg_base.h; FE_GEN_CTRL 0x6000_6090 is the only boot access | all 28 (first R) | **cannot**: undocumented radio registers behind the modem clock, replaced by the HLE; the only corpus access is the boot's rtc_sleep_pu, and a read with the modem clock gated is not known to be safe |
| B51 | `radio_fe2` | U | radio register store (a store plus tripwires, class U); base IDF soc/esp32c3/register/soc/reg_base.h; FE2_TX_INTERP_CTRL 0x6000_50F0 is the only boot access | all 28 (first R) | **cannot**: undocumented radio registers behind the modem clock, replaced by the HLE; the only corpus access is the boot's rtc_sleep_pu, and a read with the modem clock gated is not known to be safe |
| B52 | `radio_nrx` | U | radio register store (a store plus tripwires, class U); base IDF soc/esp32c3/register/soc/reg_base.h; NRXPD_CTRL 0x6001_CCD4 is the only boot access; the 0x400 ... | all 28 (first R) | **cannot**: undocumented radio registers behind the modem clock, replaced by the HLE; the only corpus access is the boot's rtc_sleep_pu, and a read with the modem clock gated is not known to be safe |
| B53 | `regi2c.REGI2C analog configuration registers` | C | stored; the master is always available, so forcing its clock or holding it in reset through these registers changes nothing | all 28 (first R) | **settled** (C kept): the three words equal (step 3), `regs:251 REG\|regi2c.0x040` `regs:252 REG\|regi2c.0x044` `regs:253 REG\|regi2c.0x048`; C for the master-enable approximation, which a guest that disables the master would show and no corpus image does |
| B54 | `rmt` | U | a deferred block (rmt, no boot histogram entry); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B55 | `rsa.RSA_M_PRIME` | C, A since step 3 | stores M'; the model derives M' and r from M itself instead of reading them (periph/rsa.rs declared approximation), which agrees with silicon whenever the caller's ... | probes/probe_crypto (first W) | **settled** (A, step 3): a wrong M' gives the device's answer, `timing:67 MPI\|modmult_mprime_right` `timing:68 MPI\|modmult_mprime_wrong` |
| B56 | `rsa.RSA_CONSTANT_TIME` | C, A since step 3 | bit 0, reset 1; stored: it changes the silicon's time only (TRM table 20.3-1), and rsa_op_ps charges one operation whatever it says | probes/probe_crypto (first W) | **settled** (A, step 3, step 3b): the operation time per CONSTANT_TIME within 0.02 %, `timing:64 TIME\|rsa_modexp_sparse_ct1 ~cycles:0.02%` `timing:65 TIME\|rsa_modexp_sparse_ct0 ~cycles:0.02%` |
| B57 | `rtc_cntl.RTC_CNTL_GPIO_WAKEUP` | C, B since step 3 | the deep-sleep GPIO wake: GPIO_PIN<n>_WAKEUP_ENABLE and GPIO_PIN<n>_INT_TYPE arm RTC pad n on a level, GPIO_WAKEUP_STATUS reads back the pad that woke the chip, and a ... | probes/probe_reset, probes/sleep_timer (first R) | **settled** (B, step 3): the GPIO0 wake at once, `reset:70 SLEEP\|armed` `reset:77 WAKE\|deep_sleep`. The regs capture's line 254 is not a boot value (the reset probe's deep sleep left it, "Step 3: the register rows"); the Part B reset probe prints the word after a `SYS_` reset |
| B58 | `rtc_cntl.RTC_CNTL_TIMER*` | C | stored; the power-up, power-down and calibration wait counts do not pace anything, because the emulator has no analog domain to wait for | all 28 (first R) | **settled** (C kept, step 5): all 19 words after the super-watchdog `SYS_` reset and after the wake equal, `resetb:81 REG\|rtc_cntl.RTC_CNTL_TIMER1.boot2` `resetb:100 REG\|rtc_cntl.RTC_CNTL_TIMER1.boot3` (TIMER2 to TIMER6 lines 82 to 86 and 101 to 105), so the regs capture's TIMER1 was the deep-sleep residue "Step 3: the register rows" named. The pacing the counts ask for stays C: the model has no analog domain to wait for, and the counts time power steps no probe can observe |
| B59 | `rtc_cntl.RTC_CNTL_ANA_CONF` | C | stored; the analog power and PLL force bits reach no emulated analog block | all 28 (first R) | **settled** (C kept): the value equal, `regs:261 REG\|rtc_cntl.RTC_CNTL_ANA_CONF`; no analog block is modeled |
| B60 | `rtc_cntl.RTC_CNTL_CLK_CONF` | C | stored; the fast and slow clock source selectors are not honored, because the emulated RTC slow clock runs at one fixed rate | all 28 (first R) | **settled** (C kept, step 5): the word after the super-watchdog reset and after the wake equal, `resetb:88 REG\|rtc_cntl.RTC_CNTL_CLK_CONF.boot2` `resetb:107 REG\|rtc_cntl.RTC_CNTL_CLK_CONF.boot3`; the RC_FAST rate is A (`timing:71 CAL\|rc_fast_d256 ~period_q13_19:0.1%`, step 3b and 3f) and the slow rate a decision (`rtc_slow_hz`, `CAL\|rtc_mux`, timing line 70). The unhonored selectors stay C: no corpus image switches the slow clock |
| B61 | `rtc_cntl.RTC_CNTL_SLOW_CLK_CONF` | C | the next-edge request self-clears so rtc_clk_wait_for_slow_cycle always sees it done; the divider and the source selector are stored and not honored | all 28 (first R) | **settled** (C kept): the word equal, `regs:263 REG\|rtc_cntl.RTC_CNTL_SLOW_CLK_CONF`; the next-edge request clears within one slow period on both sides (step 3b), `timing:69 TIME\|slow_clk_next_edge ~cycles0:*,cycles1:*,cycles2:*,cycles3:*` (the cycle counts are the slow clock's phase at each request); the divider and selector stay C, no corpus image changes them |
| B62 | `rtc_cntl.RTC_CNTL` | C | stored; the analog regulator and sleep-current trim bits reach no emulated analog block | all 28 (first R) | **settled** (C kept): the value equal, `regs:264 REG\|rtc_cntl.RTC_CNTL`; no analog block is modeled |
| B63 | `rtc_cntl.RTC_CNTL_PWC` | C | stored; the RTC power-domain controls reach no emulated power domain | all 28 (first R) | **settled** (C kept): equal at app_main and after the wake, `regs:265 REG\|rtc_cntl.RTC_CNTL_PWC` `reset:79 REG\|rtc_cntl.RTC_CNTL_PWC.after_deep_sleep`; no power domain is modeled, and the reset section's retention facts are its reset_domains rows |
| B64 | `rtc_cntl.RTC_CNTL_DIG_ISO` | C | stored; there is nothing to isolate, because the emulated digital domain is never powered down | all 28 (first R) | **settled** (C kept): equal at app_main and after the wake, `regs:266 REG\|rtc_cntl.RTC_CNTL_DIG_ISO` `reset:78 REG\|rtc_cntl.RTC_CNTL_DIG_ISO.after_deep_sleep`; no power domain is modeled, and the reset section's retention facts are its reset_domains rows |
| B65 | `rtc_cntl.RTC_CNTL_SWD*` | C, B since step 3 | the write key gates the group and the feed and flag-clear bits read back 0, but the super watchdog counts nothing and never resets the chip | all 28 (first W) | **settled** (B, step 3): the reset and its fields, `reset:60 SWD\|armed` `reset:67 SWD\|reset`; the timeout (T11) `reset:68 SWD\|timeout ~alive_us:1%,max_gap_us:1%` |
| B66 | `rtc_cntl.RTC_CNTL_DIG_PAD_HOLD` | C | stored; pad levels are not held across a sleep, because the board model keeps its pin state anyway | all 28 (first R) | **settled** (C kept): equal at app_main and after the wake, `regs:269 REG\|rtc_cntl.RTC_CNTL_DIG_PAD_HOLD` `reset:80 REG\|rtc_cntl.RTC_CNTL_DIG_PAD_HOLD.after_deep_sleep`; the board model keeps pin state anyway |
| B67 | `rtc_cntl.RTC_CNTL_BROWN_OUT` | C | the detector bit and the brownout interrupt follow the battery model while the enable is set; the reset enable, the reset wait and the analog reset enable are stored and ... | all 28 (first R) | **settled** (C kept): the value equal, `regs:270 REG\|rtc_cntl.RTC_CNTL_BROWN_OUT`; the brownout reset needs a bench supply (cannot) |
| B68 | `rtc_cntl.RTC_CNTL_FIB_SEL` | C | stored; the fuse-bypass selector reaches no emulated analog block | all 28 (first R) | **settled** (C kept): the value equal, `regs:271 REG\|rtc_cntl.RTC_CNTL_FIB_SEL`; bit 2's reset value is cleared by the bootloader before any app code (cannot) |
| B69 | `rtc_cntl.RTC_CNTL_SENSOR_CTRL` | C | stored; it is on the entropy-enable path, which is store-only across every block it touches | all 28 (first R) | **settled** (C kept): the value equal, `regs:272 REG\|rtc_cntl.RTC_CNTL_SENSOR_CTRL`; store-only (specs/blocks/rtc_cntl.toml) |
| B70 | `saradc.APB_SARADC_CTRL` | C | stored and read back; the converter needs no power or clock sequencing here, because a conversion takes zero virtual time, and the continuous-mode START bits start ... | all 28 (first R) | **settled** (C kept, step 5): the values equal before and after the ADC driver, `regs:273 REG\|saradc.APB_SARADC_CTRL` `regs:396 REG\|saradc.APB_SARADC_CTRL.after_adc`, and the one-shot read's time since step 5, `regsb:429 TIME\|adc_oneshot_read ~us:0.5%,cycles:0.5%` (`adc_conversion_ps` 26.24 us, class B). C kept: the fields' power and clock sequencing is inside that one fitted time and not applied field by field; no corpus image changes them |
| B71 | `saradc.APB_SARADC_2_DATA_STATUS` | C | an ADC2 conversion latches 0, because nothing on this board is wired to ADC2 | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B72 | `saradc.APB_SARADC_APB_ADC_CLKM_CONF` | C | stored and read back; the divider is not honored, because a conversion takes zero virtual time | all 28 (first R) | **settled** (C kept, step 5): the values equal, `regs:274 REG\|saradc.APB_SARADC_APB_ADC_CLKM_CONF` `regs:399 REG\|saradc.APB_SARADC_APB_ADC_CLKM_CONF.after_adc`, and the one-shot time at this divider, `regsb:429 TIME\|adc_oneshot_read ~us:0.5%,cycles:0.5%`. C kept: the divider is not applied, the time is fitted at the driver's divider (APB / 16), which is the only one any corpus image sets |
| B73 | `saradc.APB_SARADC_APB_ADC_ARB_CTRL` | C | stored and read back; the ADC2 arbiter has nothing to arbitrate on this board | demo, goldminer, official, pk, probe-long (first R) | **settled** (C kept): equal before and after the ADC driver, `regs:275 REG\|saradc.APB_SARADC_APB_ADC_ARB_CTRL` `regs:398 REG\|saradc.APB_SARADC_APB_ADC_ARB_CTRL.after_adc`; nothing on ADC2 to arbitrate |
| B74 | `saradc.APB_SARADC_FSM_WAIT` | C | stored and read back; the FSM wait states are not honored, because a conversion takes zero virtual time | all 28 (first R) | **settled** (C kept, step 5): the values equal, `regs:276 REG\|saradc.APB_SARADC_FSM_WAIT` `regs:397 REG\|saradc.APB_SARADC_FSM_WAIT.after_adc`, and the one-shot time at these counts, `regsb:429 TIME\|adc_oneshot_read ~us:0.5%,cycles:0.5%` (the standby, reset and power-up waits would be 113 of its 131 controller cycles, UNVERIFIED). C kept: the counts are not applied; no corpus image changes them |
| B75 | `sensitive.SENSITIVE_INTERNAL_SRAM_USAGE_*` | C | stored behind the group lock; the IRAM and DRAM split of the internal SRAM is not re-folded, because the emulated address map is fixed | all 28 (first R) | **settled** (C kept): every word equal (regs lines 277 to 280), `regs:277 REG\|sensitive.SENSITIVE_INTERNAL_SRAM_USAGE_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B76 | `sensitive.SENSITIVE_CORE_X_IRAM0_DRAM0_DMA_SPLIT_LINE_CONSTRAIN_*` | C | stored behind the group lock; the split lines are not decoded into the permission fold | all 28 (first W) | **settled** (C kept): every word equal (regs lines 281 to 286), `regs:281 REG\|sensitive.SENSITIVE_CORE_X_IRAM0_DRAM0_DMA_SPLIT_LINE_CONSTRAIN_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B77 | `sensitive.SENSITIVE_CORE_X_IRAM0_PMS_CONSTRAIN_*` | C | stored behind the group lock; the IRAM0 permission areas are not enforced | all 28 (first R) | **settled** (C kept): every word equal (regs lines 287 to 289), `regs:287 REG\|sensitive.SENSITIVE_CORE_X_IRAM0_PMS_CONSTRAIN_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B78 | `sensitive.SENSITIVE_CORE_0_IRAM0_PMS_MONITOR_*` | C | stored behind the group lock; no IRAM0 violation is ever latched, because none is detected | all 28 (first R) | **settled** (C kept): every word equal (regs lines 290 to 292), `regs:290 REG\|sensitive.SENSITIVE_CORE_0_IRAM0_PMS_MONITOR_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B79 | `sensitive.SENSITIVE_CORE_X_DRAM0_PMS_CONSTRAIN_*` | C | stored behind the group lock; the DRAM0 permission areas are not enforced | all 28 (first R) | **settled** (C kept): every word equal (regs lines 293 to 294), `regs:293 REG\|sensitive.SENSITIVE_CORE_X_DRAM0_PMS_CONSTRAIN_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B80 | `sensitive.SENSITIVE_CORE_0_DRAM0_PMS_MONITOR_*` | C | stored behind the group lock; no DRAM0 violation is ever latched | all 28 (first R) | **settled** (C kept): every word equal (regs lines 295 to 298), `regs:295 REG\|sensitive.SENSITIVE_CORE_0_DRAM0_PMS_MONITOR_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B81 | `sensitive.SENSITIVE_CORE_0_PIF_PMS_CONSTRAIN_*` | C | stored behind the group lock; the peripheral-bus permission areas are not enforced | all 28 (first W) | **settled** (C kept): every word equal (regs lines 299 to 309), `regs:299 REG\|sensitive.SENSITIVE_CORE_0_PIF_PMS_CONSTRAIN_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B82 | `sensitive.SENSITIVE_CORE_0_PIF_PMS_MONITOR_*` | C | stored behind the group lock; no peripheral-bus violation is ever latched | all 28 (first R) | **settled** (C kept): every word equal (regs lines 310 to 316), `regs:310 REG\|sensitive.SENSITIVE_CORE_0_PIF_PMS_MONITOR_0`; enforcement shows only on a violation, which IDF's memory protection turns into a panic; no corpus image violates (not a corpus path) |
| B83 | `sha.SHA_CLEAR_IRQ` | C | write-trigger: a write of 1 clears the latched DMA completion interrupt (source 49); reads back 0 | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B84 | `sha.SHA_INT_ENA` | C | bit 0 gates the level of source 49; the DMA completion latches whether or not it is set (UNVERIFIED), and block mode never raises it | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B85 | `spi0.SPI_MEM_CTRL` | C | stored; SPI0 drives no transaction, so the read mode and dummy-cycle bits select nothing | all 28 (first W) | **settled** (C kept): the value equal, `regs:317 REG\|spi0.SPI_MEM_CTRL`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B86 | `spi0.SPI_MEM_CTRL2` | C | stored; the timing trim has no effect and SYNC_RESET reads back 0 as the device does | all 28 (first R) | **settled** (C kept): the value equal, `regs:318 REG\|spi0.SPI_MEM_CTRL2`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B87 | `spi0.SPI_MEM_CLOCK` | C | stored; the emulated flash answers inside the access, so the clock divider paces nothing | all 28 (first W) | **settled** (C kept): the value equal, `regs:319 REG\|spi0.SPI_MEM_CLOCK`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B88 | `spi0.SPI_MEM_USER` | C | stored; SPI0 decodes no user transaction | all 28 (first R) | **settled** (C kept): the value equal, `regs:320 REG\|spi0.SPI_MEM_USER`; equal since step 3 (the unstored bits); the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B89 | `spi0.SPI_MEM_USER1` | C | stored; SPI0 decodes no user transaction | all 28 (first R) | **settled** (C kept): the value equal, `regs:321 REG\|spi0.SPI_MEM_USER1`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B90 | `spi0.SPI_MEM_USER2` | C | stored; SPI0 decodes no user transaction, so the opcode field selects nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:322 REG\|spi0.SPI_MEM_USER2`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B91 | `spi0.SPI_MEM_MOSI_DLEN` | C | stored; SPI0 moves no data, so the transfer length bounds nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:323 REG\|spi0.SPI_MEM_MOSI_DLEN`; equal since step 3; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B92 | `spi0.SPI_MEM_MISO_DLEN` | C | stored; SPI0 moves no data, so the transfer length bounds nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:324 REG\|spi0.SPI_MEM_MISO_DLEN`; equal since step 3; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B93 | `spi0.SPI_MEM_MISC` | C | stored; the chip-select and hold controls reach no emulated pin | all 28 (first R) | **settled** (C kept): the value equal, `regs:325 REG\|spi0.SPI_MEM_MISC`; equal since step 3; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B94 | `spi0.SPI_MEM_CACHE_FCTRL` | C | stored; a cache fill never goes through SPI0, so enabling cache access to it changes nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:326 REG\|spi0.SPI_MEM_CACHE_FCTRL`; a cache fill never goes through SPI0 in the model |
| B95 | `spi0.SPI_MEM_CLOCK_GATE` | C | stored; the block is never clock-gated, so the gate reaches nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:327 REG\|spi0.SPI_MEM_CLOCK_GATE`; gating the flash controller under code that runs from flash is not a safe probe step (cannot) |
| B96 | `spi0.SPI_MEM_CORE_CLK_SEL` | C | stored; the emulated flash answers inside the access, so the core clock source selects nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:328 REG\|spi0.SPI_MEM_CORE_CLK_SEL`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B97 | `spi1.SPI_MEM_CTRL1` | C | stored; the bus timing and the RX FIFO reset reach nothing, because the transaction is synchronous | all 28 (first R) | **settled** (C kept): the value equal, `regs:329 REG\|spi1.SPI_MEM_CTRL1`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B98 | `spi1.SPI_MEM_CTRL2` | C | stored; the timing trim has no effect and SYNC_RESET reads back 0 as the device does | all 28 (first R) | **settled** (C kept): the value equal, `regs:330 REG\|spi1.SPI_MEM_CTRL2`; equal since step 3; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B99 | `spi1.SPI_MEM_CLOCK` | C | stored; the emulated part answers inside the access, so the clock divider paces nothing | all 28 (first W) | **settled** (C kept): the value equal, `regs:331 REG\|spi1.SPI_MEM_CLOCK`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B100 | `spi1.SPI_MEM_MISC` | C | stored; the chip-select and hold controls reach no emulated pin | all 28 (first R) | **settled** (C kept): the value equal, `regs:332 REG\|spi1.SPI_MEM_MISC`; the model's flash answers inside the access, so no pin timing or transaction engine exists to show |
| B101 | `spi1.SPI_MEM_FLASH_WAITI_CTRL` | C | stored; the model charges its own write-in-progress time, so the wait-idle command and its dummy cycles are not issued | all 28 (first R) | **settled** (C kept): the value equal, `regs:333 REG\|spi1.SPI_MEM_FLASH_WAITI_CTRL`; the wait-idle command runs only around a program or erase (cannot) |
| B102 | `spi1.SPI_MEM_CLOCK_GATE` | C | stored; the block is never clock-gated, so the gate reaches nothing | all 28 (first R) | **settled** (C kept): the value equal, `regs:334 REG\|spi1.SPI_MEM_CLOCK_GATE`; as B95 |
| B103 | `spi1.SPI_MEM_FLASH_SUS_CTRL` | C | FLASH_PER and FLASH_PES self-clear inside the write as the device clears them; the program-erase suspend they ask for never runs, and the wait enables, end mask and ... | all 28 (first R) | **settled** (C kept): the value equal, `regs:335 REG\|spi1.SPI_MEM_FLASH_SUS_CTRL`; a suspend needs a program or erase in progress (cannot) |
| B104 | `spi1.SPI_MEM_SUS_STATUS` | C | FLASH_SUS self-clears inside the write as the device clears it; the model holds no suspended program or erase for it to report, and the delay and lock fields beside it ... | all 28 (first R) | **settled** (C kept): the value equal, `regs:336 REG\|spi1.SPI_MEM_SUS_STATUS`; as B103 |
| B105 | `system.SYSTEM_CPU_PERI_CLK_EN` | C | stored; no emulated CPU peripheral checks its clock gate | all 28 (first R) | **settled** (C kept): the value equal, `regs:337 REG\|system.SYSTEM_CPU_PERI_CLK_EN`; the gated CPU peripherals (assist_debug) are the corpus's only users and never run gated |
| B106 | `system.SYSTEM_CPU_PERI_RST_EN` | C | bit 6 resets the assist_debug block and republishes the stack monitor; the other reset enables are stored and hold no block in reset | all 28 (first R) | **settled** (C kept): the value equal, `regs:338 REG\|system.SYSTEM_CPU_PERI_RST_EN`; bit 6 is modeled, the other bits hold no block the corpus uses |
| B107 | `system.SYSTEM_MEM_PD_MASK` | C | stored; the emulator has no memory power domains to keep alive across a sleep | all 28 (first R) | **settled** (C kept): the value equal, `regs:339 REG\|system.SYSTEM_MEM_PD_MASK`; no memory power domain is modeled |
| B108 | `system.SYSTEM_PERIP_CLK_EN0` | C, B since step 5 | stored; no emulated peripheral checks its clock gate, so a block answers whether or not the guest enabled it | all 28 (first R) | **settled** (B, step 5): the word equal, `regs:340 REG\|system.SYSTEM_PERIP_CLK_EN0`, and the clock-off rule per block equal on the four EN0 blocks the captures gate: I2S0 and SPI2 read the last value read, `regsb:405 GATE\|i2s0_latch` `regsb:420 GATE\|spi2_clk_off` `regsb:421 GATE\|spi2_latch` (SPI2's gate experiment reads the writable bits it read last while it holds 0, which settles the latch against "the writable bits read 1"), LEDC and I2C0 their stored value, `regsb:410 GATE\|ledc_clk_off` `regsb:411 GATE\|ledc_latch` `regsb:415 GATE\|i2c0_clk_off` `regsb:416 GATE\|i2c0_latch`; every gated write dropped. B: the ten other EN0 bits gate nothing, UNVERIFIED (no capture gates them), and per block against per register latch is UNVERIFIED |
| B109 | `system.SYSTEM_PERIP_CLK_EN1` | C, B since step 5 | stored; no emulated peripheral checks its clock gate | all 28 (first R) | **settled** (B, step 5): the word (bit 9) equal, `regs:341 REG\|system.SYSTEM_PERIP_CLK_EN1`, AES reads the last value read, `regsb:406 GATE\|aes_latch`, and SHA reads 0 gated and keeps its word, `regsb:425 GATE\|sha_clk_off` `regsb:426 GATE\|sha_latch`. B: RSA, DS, HMAC and GDMA gate nothing, UNVERIFIED |
| B110 | `system.SYSTEM_PERIP_RST_EN0` | C, B since step 5 | stored; holding a peripheral in reset is not modeled, so a block answers while the guest asserts its reset | all 28 (first R) | **settled** (B, step 5): the word equal, `regs:342 REG\|system.SYSTEM_PERIP_RST_EN0`, and the held-reset rule on three more EN0 blocks, `regsb:408 GATE\|ledc_rst_held` `regsb:409 GATE\|ledc_released` `regsb:413 GATE\|i2c0_rst_held` `regsb:414 GATE\|i2c0_released` `regsb:418 GATE\|spi2_rst_held` `regsb:419 GATE\|spi2_released`: the rule is the device's on 7 of the 15 blocks it is applied to (I2S0, UART0, LEDC, I2C0, SPI2, TIMG0, SYSTIMER). B: the other eight follow it UNVERIFIED |
| B111 | `system.SYSTEM_PERIP_RST_EN1` | C, A since step 3 | stored; holding a peripheral in reset is not modeled | all 28 (first R) | **settled** (A, step 3): the word and the AES held-reset lines equal, `regs:343 REG\|system.SYSTEM_PERIP_RST_EN1` `regs:387 GATE\|aes_rst_held` `regs:388 GATE\|aes_released` |
| B112 | `system.SYSTEM_BT_LPCK_DIV_INT` | C | stored; there is no emulated BT controller for the low-power clock divider to feed | all 28 (first R) | **settled** (C kept): the word and its writable bits equal, `regs:344 REG\|system.SYSTEM_BT_LPCK_DIV_INT` `regs:392 MASK\|system.SYSTEM_BT_LPCK_DIV_INT`; the controller it feeds is the HLE's |
| B113 | `system.SYSTEM_BT_LPCK_DIV_FRAC` | C | stored; there is no emulated BT controller for the low-power clock divider to feed | all 28 (first R) | **settled** (C kept): the word and its writable bits equal, `regs:345 REG\|system.SYSTEM_BT_LPCK_DIV_FRAC` `regs:393 MASK\|system.SYSTEM_BT_LPCK_DIV_FRAC`; as B112 |
| B114 | `system.SYSTEM_CACHE_CONTROL` | C | stored; the cache clock enable and the cache reset reach nothing, because the emulated cache is the address-space fold and is always available | all 28 (first R) | **settled** (C kept): the value equal, `regs:346 REG\|system.SYSTEM_CACHE_CONTROL`; gating it under code that runs from the cache is not a safe probe step (cannot) |
| B115 | `timg0.TIMG_WDTCONFIG0` | C | watchdog enable at bit 31, the four stage actions at bits 29 - 2n, USE_XTAL and FLASHBOOT_MOD_EN; stored exactly, and every field but FLASHBOOT_MOD_EN acts | all 28 (first R) | **settled** (C kept): the value equal, `regs:347 REG\|timg0.TIMG_WDTCONFIG0`; FLASHBOOT_MOD_EN acts only between the ROM and the bootloader, before any app code (cannot) |
| B116 | `timg0.TIMG_REGCLK` | C, A since step 5 | block clock gate: stored and read back; clearing it does not stop the group in the model | all 28 (first R) | **settled** (A, step 5): the word and register access with CLK_EN clear equal, `regs:348 REG\|timg0.TIMG_REGCLK` `regs:390 GATE\|timg0_regclk`, and the counter runs on with it clear, 205 us of 200 on the device, `regsb:427 GATE\|timg0_regclk_count ~on:1%,off:1%,later:1%` (201, 406, 608 against the model's 202, 405, 607: the CPU's time around the latches). The row's claim, that clearing it stops nothing, is the silicon fact |
| B117 | `timg1.TIMG_WDTCONFIG0` | C | watchdog enable at bit 31, the four stage actions at bits 29 - 2n, USE_XTAL and FLASHBOOT_MOD_EN; stored exactly, and every field but FLASHBOOT_MOD_EN acts | all 28 (first R) | **settled** (C kept): the value equal, `regs:349 REG\|timg1.TIMG_WDTCONFIG0`; FLASHBOOT_MOD_EN acts only between the ROM and the bootloader, before any app code (cannot) |
| B118 | `timg1.TIMG_REGCLK` | C, A since step 5 | block clock gate: stored and read back; clearing it does not stop the group in the model | all 28 (first R) | **settled** (A, step 5): as B116, `regs:350 REG\|timg1.TIMG_REGCLK` `regs:391 GATE\|timg1_regclk` `regsb:428 GATE\|timg1_regclk_count` (200, 400, 401, 601 on both sides) |
| B119 | `twai` | U | a deferred block (twai, no boot histogram entry); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B120 | `uart0.UART_CLKDIV` | C | stored; the baud rate is not honored, because the model transmits inside the write and the host stream carries bytes | all 28 (first W) | **settled** (C kept): held in reset at app_main (0 on both sides, `regs:351 REG\|uart0.UART_CLKDIV`), the driver's dividers equal (`timing:117 REG\|uart0.UART_CLKDIV.115200` `timing:122 REG\|uart0.UART_CLKDIV.921600`) and paced under `device` (`timing:116 TIME\|uart0_tx_128_115200 ~us:1%` `timing:121 TIME\|uart0_tx_128_921600`); the `fast` profile does not pace, so the register stays C as a whole |
| B121 | `uart0.UART_CONF0` | C | stored; the enable, parity, stop-bit and reset controls reach nothing, because every byte written to UART_FIFO is transmitted | all 28 (first R) | **settled** (C kept): 0 in reset and the driver's word equal, `regs:352 REG\|uart0.UART_CONF0` `timing:118 REG\|uart0.UART_CONF0.115200`; framing reaches no host byte stream |
| B122 | `uart0.UART_CONF1` | C | stored; the FIFO thresholds gate no interrupt, because the transmitter is always empty and the receive queue is drained by the read | all 28 (first W) | **settled** (C kept): 0 in reset and the driver's word equal, `regs:353 REG\|uart0.UART_CONF1` `timing:119 REG\|uart0.UART_CONF1.115200`; the thresholds gate no interrupt the corpus takes |
| B123 | `uart0.UART_CLK_CONF` | C | stored; the clock source and divider select nothing, for the same reason as UART_CLKDIV | all 28 (first W) | **settled** (C kept): 0 in reset and the driver's word equal, `regs:354 REG\|uart0.UART_CLK_CONF` `timing:120 REG\|uart0.UART_CLK_CONF.115200`; the `fast` profile does not pace, so the register stays C as a whole |
| B124 | `uart1` | U | a deferred block (uart1, no boot histogram entry); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED | all 28 (first W) | **untouched**: ledger: the only touch of the block in all 28 images is UART_INT_CLR, row uart1.UART_INT_CLR (probed) |
| B125 | `uart1.UART_INT_CLR` | C | every field is WT: a write triggers the clear of the matching UART_INT_RAW bit and the register reads back 0, as the device does. The raw bits it clears are never set, ... | all 28 (first W) | **settled** (C kept): reads 0 on both sides, `regs:355 REG\|uart1.UART_INT_CLR`; UART1 raises nothing in the corpus (the row's retirement condition) |
| B126 | `uhci0` | U | a deferred block (uhci, no boot histogram entry); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B127 | `usj.USB_SERIAL_JTAG_IN_EP1_ST` | C | IN_EP1_STATE reads 1 (idle); WR_ADDR and RD_ADDR stay 0 rather than tracking the FIFO fill | none of 28 | **untouched**: ledger: no image of the 28 reads or writes the register |
| B128 | `world_cntl` | U | a deferred block (world_cntl, no boot histogram entry); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |
| B129 | `xts_aes` | U | a deferred block (xts_aes; the sha/aes DMA modes, hmac, ds and xts_aes group); base IDF soc/esp32c3/register/soc/reg_base.h; window size 0x1000 UNVERIFIED ... | none of 28 | **untouched**: ledger: no image of the 28 touches any register of the block |

## 2. Class C timing constants

`specs/timing-profiles.toml`, the `device` profile's class C `[[constant]]` rows.

| # | Row | Assumed | Touched by | Disposition |
|---|---|---|---|---|
| T1 | `timing-profiles.cache_fill_ps` | 2.77 us per 32-byte line miss, a fit that carries boot cost beyond the fill (first principles 1.9 us; probe_intc LAT one miss 1.53 us) | every image: all code and constant data run from flash through the cache | **settled** (A, step 3b, refined by 3i to 3l): cold fills within 1 %, `timing:56 TIME\|cache_code_cold ~cycles:1%,ticks:1%` `timing:58 TIME\|cache_data_cold ~cycles:1%,ticks:1%`; the fill rows are pinned by `t1_campaign_fill_overlap_divide_and_mmio_rows_match_the_device_capture` |
| T2 | `timing-profiles.cache_stall_counts_cycles` | the cycle counter counts the stall of a fill | as T1 | **settled** (A, step 3b): cycles equal ticks x 10 over the cold loops on both sides, `timing:58 TIME\|cache_data_cold ~cycles:1%,ticks:1%` |
| T3 | `timing-profiles.aes_block_ps` | 0.372 us per 16-byte block, an upper bound from IDF's minimum throughput | ledger: only `probes/probe_crypto` reaches the AES block | **settled** (A, step 3b): the slope over the long runs, `timing:61 TIME\|aes_cbc_1024 ~cycles:1%` `timing:63 TIME\|aes_cbc_16384 ~us:2%,cycles:*` (`cycles` is the last call's, cold flash code, the class C residue of step 3k) |
| T4 | `timing-profiles.rsa_op_ps` | 0: no capture times an RSA operation | ledger: only `probes/probe_crypto` reaches the RSA block | **settled** (A, step 3b): within 0.02 %, `timing:64 TIME\|rsa_modexp_sparse_ct1 ~cycles:0.02%` `timing:66 TIME\|rsa_modexp_dense_ct0 ~cycles:0.02%` |
| T5 | `timing-profiles.flash_pp_ps` | 0.7 ms per page program, NOR typical | flash commands have no ledger record; every image drives SPI1, the command host (all 28, first write) | **cannot**: timing a program means writing the flash, which this step does not do; the device's table has no scratch partition to write |
| T6 | `timing-profiles.flash_se_ps` | 45 ms per 4 KB sector erase, NOR typical | as T5 | **cannot**: an erase; the row's own path is probe_timing erase_4k on a table with a scratch partition, with the device owner's approval |
| T7 | `timing-profiles.flash_be_ps` | 150 ms per block erase, NOR typical | as T5 | **cannot**: an erase, as T6 |
| T8 | `timing-profiles.flash_ce_ps` | 20 s per chip erase, NOR typical | as T5 | **cannot**: a chip erase destroys cardid, which is irreversible |
| T9 | `timing-profiles.usj_drain_ps` | 125 us per 64-byte IN packet, chosen under a 200 us ceiling | ledger: all 28 write the USJ IN endpoint (USB_SERIAL_JTAG_EP1) | **settled** (B, step 3b, fitted): 16 lines within 3 %, `timing:91 TIME\|usj_drain_16x64 ~us:3%` (the host reads; the fit is one sample) |
| T10 | `timing-profiles.ble_init_ps` | 0: no separate controller init time is identifiable | the products' boot (dev:L67-L68 in the row) and the BLE probes; the HLE replaces the call, so the ledger has no register of it | **settled** (A, step 3): `radio:62 TIME\|ble_init ~us:1%`; +2.7 % after step 3l, the guest work around the charged constant grew under FIFO (step 4, found), refitted by step 5 to 2140 us, which reads 3758 us against the device's 3757 and 3760 |

`probe_campaign_radio` also prints TIME ble_enable and ble_disable against
`timing-profiles.ble_enable_ps` (class B), as a check. **Step 3**: the check found the fit wrong.
The probe's enable is 37,542 and 41,135 us (L66), not the fitted 80 ms, so `ble_enable_ps` is
39,064,000,000 ps (class A); the rest of pk's phy_init-to-ready span (dev:L72-L73) is a new class
C row, `ble_enable_nvs_cal_ps` (40.5 ms, applied only to an image that links
`esp_phy_load_cal_data_from_nvs`, which pk does and the probe, built with
`CONFIG_ESP_PHY_CALIBRATION_AND_DATA_STORAGE=n`, does not). Disable (L71, 668 and 662 us) and
deinit (L73, 1,034 us) get rows of their own, `ble_disable_ps` and `ble_deinit_ps` (class A).

### Step 3b: the timing rows

Every `probe_campaign_timing` row the compare listed as different, except `rsa.RSA_M_PRIME` and
the `uart0` CONF0/CONF1 and `rtc_cntl` CLK_CONF register values ("Step 3: the register rows"). Capture
`device-probe_campaign_timing-20260924T155139Z`, run1 and run2 (the same line numbers in both);
"before" is the step-1 record, "after" the renewed `tests/fw/campaign/probe_campaign_timing.emu.txt`.
`cargo xtask probes compare` compares exactly, so a timed fact stays "different" unless it lands on
the device's number; what is left is named per row.

| Item | Line | Device (run1 / run2) | Before | After | Class and what changed |
|---|---|---|---|---|---|
| V49, `systimer.SYSTIMER_TARGET0_CONF`, `TARGET1_CONF` | 72 | IDF order: first 16010 after the mode write, 24016 after the load, cadence 15999 / 15997 | 16000, 24005, 16002 | 16001, 24007, 15999 | B to A. The PERIOD_MODE 0-to-1 write re-bases the latched period one period after itself |
| | 73 | new period 32000, mode off and on, no load: first 16003 / 16011, cadence 16004 / 15999 | 32001, 31999 | 16001, 15999 | the latched 16000 is kept, the written 32000 waits for a load (the step-2 notes read it the other way round) |
| | 74 | period 0: fires once, 22 ticks after the load, not again within 4 ms | 4554, then again 5 later | 4, once | fires once; the 22 against 4 and the ten-tick offsets above are the probe's APB polls, which the model does not charge |
| B61, `rtc_cntl.RTC_CNTL_SLOW_CLK_CONF` | 69 | next-edge request set for 508, 727, 917, 979 / 1208, 915, 915, 915 cycles | 457, 14, 14, 14 | 458, 672, 994, 982 | stays C (divider and selector not honored); the request now clears at the slow clock's next edge, a scheduled event |
| B60, `rtc_cntl.RTC_CNTL_CLK_CONF`, CAL facts | 70 | rtc_mux 3588800 / 3586637 | 3617574 | 3617574 | unchanged by decision: `rtc_slow_hz` keeps the boot-time calibration of the wifi_facts capture; the 0.8 % day-to-day spread is recorded in its basis |
| | 71 | rc_fast_d256 7569178 / 7566413 | 7669632 | 7567782 | `timg::RC_FAST_D256_HZ` 69279 Hz, class A (was the nominal 17.5 MHz) |
| B28, B29, B38, `i2c0` SCL period and CLK_CONF | 92, 104 | 20 reads: 9174 / 9173 us at 100 kHz, 3044 us at 400 kHz | 8955, 2788 | 9094, 3016 | block rows stay C (`fast` does not clock); `i2c_clocked` B to A: 40.9 SCL periods plus 50.0 us a read on silicon, 40.5 plus 49.5 here; the 0.9 % left is CPU |
| B120, B123, `uart0.UART_CLKDIV`, `UART_CLK_CONF` | 116 to 125 | 128 bytes: 11187 / 11186 us at 115200, 1406 us at 921600, rc 0 | 193820, 198363, rc 263 | 11188, 1406, rc 0 | stay C (`fast` does not pace). TX_DONE now latches when the last byte leaves, and the core clock follows SCLK_SEL and its divider (the driver selects APB, 80 MHz) |
| T1, `timing-profiles.cache_fill_ps` | 56 to 59 | cold code 20931, cold data 83506, warm 1405 and 3188 cycles | 29646, 116435, 1282, 2975 | 21725, 84124, 1655, 3842 | C to A: 1.96 us, measured (313.2 cycles a miss); the warm loops' +18 % and +21 % are the one-CPI residual of the refit |
| T2, `timing-profiles.cache_stall_counts_cycles` | 56, 58 | ticks 2086 and 8344, 10.03 and 10.01 cycles a tick | 2963, 11641 | 2169, 8409 | C to A |
| T3, `timing-profiles.aes_block_ps` | 60 to 63 | 437, 57, 117 / 116, 326 us | 552, 54, 128, 414 | 442, 53, 111, 321 | C to A: 0.2728 us a block, the slope over 768 blocks |
| T4, `timing-profiles.rsa_op_ps`; B56, `rsa.RSA_CONSTANT_TIME` | 64 to 66 | 35173669, 17597366, 35156148 cycles | 1812, 39, 39 | 35173648, 17595266, 35156170 | both C to A: 53.644 us a 2048-bit Montgomery multiplication, counted per CONSTANT_TIME (within 0.012 %) |
| T9, `timing-profiles.usj_drain_ps` | 91 | 2975 / 2898 us | 4917 | 2940 | C to B: 55.5 us, fitted; each 64-byte probe line is 65 bytes on the wire (CR LF), two IN packets, not the one the probe's comment assumes |

Beside the rows: the boot fit refit around the measured fill moves `cpi_milli` from 1.27 to 1.64
(fit RMS 3.71 to 3.74 ms, validation RMS 2.00 to 0.94 ms, 1.12 with the fitted USJ poll), and `sha_block_ps` goes from B to C,
because at the refit CPI `probe_timing` sha256_1m no longer identifies it.
Added by "Step 3: reset and deep sleep", a class C model constant outside the table (the timeout is a property of the
chip, not of a timing profile):

| # | Row | Assumed | Touched by | Disposition |
|---|---|---|---|---|
| T11 | `rtc_cntl.SWD_TIMEOUT_PS` | 3355 ms from arming to the reset (class B; the TRM's "slightly less than one second" does not hold on this chip) | only a firmware that switches the super watchdog's auto-feed off: `probe_campaign_reset` | **settled** (B, step 3): `reset:68 SWD\|timeout ~alive_us:1%,max_gap_us:1%` |

## 3. UNVERIFIED known diffs

`specs/oracle-known-diffs.toml` entries carry reasons, not classes; two reasons rest on an
UNVERIFIED value.

| # | Entry | Assumed | Touched by | Disposition |
|---|---|---|---|---|
| K1 | `systimer.unit0-op-snapshots` | SYSTIMER_UNIT0_OP VALUE_VALID reads 0 before the first update (c3-registers.csv reset 0, basis UNVERIFIED) | ledger: all 28 read SYSTIMER_UNIT0_OP first | **cannot**: the value exists only before esp_timer's first update, which the bootloader makes before any app code runs |
| K2 | `console.boot-strap` | the oracle's strap_mode 0x0A override | the ROM banner of every boot | **cannot**: a QEMU configuration, not a silicon fact; the device's value is already class A (`gpio.GPIO_STRAP`, boot:0xa) |

`probe_campaign_regs` also reads `TIMG_RTCCALICFG` and `TIMG_RTCCALICFG2` of TIMG0 (row ids
`known-diffs.timg0.TIMG_RTCCALICFG` and `...CFG2`): the entries `timg0.reset-value-068` and
`timg0.reset-value-080` say which bits this model and the oracle disagree on, and the device's
word after the bootloader's calibration says which side silicon is on.

## 4. Class C and UNVERIFIED HLE values

`specs/hle/idf-5.5.3/wifi.toml`, `ble.toml` and `log-lines.toml`. The HLE replaces the radio
blobs, so the ledger never shows these; "touched by" names the images that bind the module.

| # | Row | Assumed | Touched by | Disposition |
|---|---|---|---|---|
| H1 | `hle.wifi.isr_source` | 0, the source the U4 magic ISR is allocated on; UNVERIFIED because no U4 Wi-Fi run was made. **Exercised:** U4 is the default and every Wi-Fi milestone test runs the Wi-Fi worker on source 0 | the builds `wifi.toml`'s variants were measured on (`official`, `demo`, `probe2`, `scan3`, `probe_wifi_http`) | **cannot**: a choice of the emulator's U4 mode (no blob runs, so no silicon source exists for it); settled by a U4 run, not by a device |
| H2 | `hle.wifi` worker hop | the worker's hop to esp_event_post (POST_US plus the U5 poll) added on top of every event time, class C; under U4, the default, POST_US alone (`STA_CONNECTED` 212,900 us against silicon's 212,415) | as H1 | **cannot**: emulator-internal; the device-visible total (`connected_us`, class A) already includes the real driver's delivery |
| H3 | `hle.wifi.connected_aid` | 1, the lowest AID IEEE 802.11 allows | images that associate: `probe_wifi_http` (open AP) and `probe_wifi_assoc` | **cannot**: the access point chooses it; a capture shows one AP's choice, not a property of the part |
| H4 | `hle.wifi.connected_us` for an open network | the WPA2-PSK association time (212,415 us) used for an open AP too | `probe_wifi_http` | **open**: needs an access point with no password, which no capture had. Part B, no new source: the operator builds `probe_wifi_assoc` (device table) with an open SSID and an empty password in its overlay; a person sets up the open access point near the device. Its `STA_CONNECTED` stamp is the fact |
| H5 | `hle.wifi` heap row `dynamic_rx` | count from one build's sdkconfig (`count_class = "sdkconfig"`, class C) times the specification's element size | Wi-Fi images, per received frame | **cannot**: the count is a build's configuration value, not a property of silicon; the calibrated total needs RX traffic from an access point, a later capture |
| H6 | `hle.ble.isr_source` | 8 (`ETS_RWBLE_INTR_SOURCE`), UNVERIFIED | the builds `ble.toml`'s variants were measured on (`pk`, `pkgatt`, `official`, `probe2`, `scan3`) | **settled** (A, step 3): RWBLE on line 8, the other six at 0, at every stage, `radio:57 ISR\|before_init` `radio:64 ISR\|after_init` `radio:68 ISR\|after_enable` |
| H7 | `hle.ble.heap_ledger` | a class C lower bound, count x element; the rows `link_env`, `scan_dupl`, `adv_dup_filt` count from pk's sdkconfig | as H6 | **settled** (B, step 3): free heap and largest block equal at every stage, `radio:56 HEAP\|before_init` `radio:63 HEAP\|after_init` `radio:67 HEAP\|after_enable` `radio:72 HEAP\|after_disable` `radio:74 HEAP\|after_deinit`; the split into blocks stays an estimate |
| H8 | `hle.ble.reply_us` | the virtual time between a host packet and the controller's events, class C (no timestamped capture) | as H6 | **settled** (A for the two timed commands, step 3): inside the device runs, `radio:69 HCI\|reset ~us:2%` `radio:70 HCI\|read_local_version ~us:5%` (`t1_campaign_radio_under_u4_answers_hci_inside_the_device_runs`); every other command stays C |
| H9 | `hle.ble.log_lines` | the `config = "pk"` lines of `log-lines.toml` (XTAL clock source, Feature Config) are pk's, UNVERIFIED for another BT sdkconfig; `pkgatt`'s shape binds `log_lines = "verified"` without its boot log compared (D7) | `pk`, `pkgatt` | **settled** (A, step 3): the lines equal, `radio:58 LOG\|ble_init_1` `radio:59 LOG\|ble_init_2` `radio:60 LOG\|ble_init_3` `radio:61 LOG\|ble_init_4` `radio:65 LOG\|phy_init_1` |

## 5. UNVERIFIED items inside class A and B block rows

Rows whose class is A or B but whose provenance, value or note still calls something UNVERIFIED.
The row id of a probe line for these is given in the disposition.

| # | Row | Class | The UNVERIFIED item | Disposition |
|---|---|---|---|---|
| V1 | `aes` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V2 | `aes.AES_MODE` (overrides) | A | 0 (aes_gcm128 tag=82d72d88d22d9f40b5540d1fb53146f0). The odd reserved values are not exercised and stay UNVERIFIED  bits 2 to 0: 0 AES-128 encrypt, 2 AES-256 encryp | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V3 | `aes.AES_TRIGGER` (overrides) | A | 1 a DMA run of AES_BLOCK_NUM blocks (Wiring::AesDma), STATE 1 until it completes; reads 0. UNVERIFIED and class C inside the row: a trigger while STATE | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V4 | `aes.AES_STATE` (overrides) | B | 2 once a DMA run has completed and until AES_DMA_EXIT; the typical mode returns to 0 at its completion. UNVERIFIED and class C inside the row: a DMA run whose TX or | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V5 | `aes.AES_BLOCK_MODE` (overrides) | A | (5) are not run by the probe and rest on the SP 800-38A unit vectors, class B inside the row; 6 and 7 stay UNVERIFIED  bits 2 to 0: 0 ECB, 1 CBC, 2 OFB, 3 CTR, 4 CFB8, | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V6 | `aes.AES_BLOCK_NUM` (overrides) | A | is not exercised (the padding mutant survives the probe), class B inside the row; BLOCK_NUM 0 stays UNVERIFIED  the block count of the next DMA run, all 32 bits | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V7 | `aes.AES_INT_ENA` (overrides) | B | depends on it  bit 0 gates the level of source 48; the DMA completion latches whether or not it is set (UNVERIFIED), and the typical mode never raises it | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V8 | `aes.AES_DMA_EXIT` (overrides) | B | DMA_EXIT mutant survives the probe)  write-trigger: any write returns STATE from 2 (done) to 0; reads 0. UNVERIFIED and class C inside the row: a write while STATE i | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V9 | `apb_ctrl` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V10 | `apb_ctrl.SYSCON_RND_DATA` (reset_domains) | B | it belongs to the rng snapshot section, not to this block's reset, so a reset does not rewind it. UNVERIFIED against hardware, where the entropy source keeps  | **cannot**: silicon has no sequence to rewind: the entropy source runs on, so a device run cannot show a reset rewinding it |
| V11 | `cw2017.*` (reset_domains) | B | the battery domain and survive MCU resets and power-off, so no MCU reset scope reaches them (UNVERIFIED on the schematic). battery.fresh() and a disconne | **cannot**: the gauge's supply on the schematic: needs the board, which means opening the device |
| V12 | `cw2017.VERSION` (overrides) | A | reading VERSION on a second unit as an open device question, so whether it is a part or a unit property is UNVERIFIED  15 | **cannot**: needs a second unit |
| V13 | `flash_xmc.wait` (wait) | B | with the per-op timeouts at 65-68) (WIP timing, device figures UNVERIFIED for this XMC part); boot-path seed row | **cannot**: WIP times of this part come from programs and erases, which this step does not do |
| V14 | `gdma` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V15 | `gpio` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V16 | `gpio.GPIO_STRAP` (overrides) | A | other values in other emulators. Class A because a device capture fixes it. The meaning of bit 1 is UNVERIFIED (b0 GPIO2, b2 GPIO8, b3 GPIO9 per the TR | **cannot**: the meaning of bit 1 needs the strapping pins driven otherwise, which means the board |
| V17 | `i2c0` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V18 | `i2s0` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V19 | `intc.INTERRUPT_CORE0_CPU_INT_CLEAR` (overrides) | B | IDF soc/esp32c3/register/soc/interrupt_core0_reg.h (+0x10C); the held-at-1 case is UNVERIFIED and IDF never clears the bits rv_uti | **untouched**: ledger: only probes/probe_intc reads it, and IDF never clears the bits (the row) |
| V20 | `iomux` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V21 | `iomux.*` (reset_domains) | B | matrix: IO_MUX is in the digital domain, which every reset scope restores. The per-pad reset values are UNVERIFIED here (no CSV rows); the block owner imports them | **settled** (A, step 3, the new pad-value row): all 22 pads equal (lines 358 to 379), `regs:358 REG\|iomux.IO_MUX_GPIO0` `regs:379 REG\|iomux.IO_MUX_GPIO21` |
| V22 | `ledc` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V23 | `mmu` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V24 | `mmu.*` (reset_domains) | B | matrix: the MMU table is in the digital domain, which every reset scope restores. The entry reset value is UNVERIFIED (no public source gives one); the bootloader write | **untouched**: ledger: all 3584 first touches of the MMU table (128 entries x 28 images) are writes, so no image reads an entry's reset value |
| V25 | `radio_ble.wait` (wait) | B | row: ROM r_rwip_time_get spins on 0x6003_101C whenever controller code runs; `E_STUCK`. Register name UNVERIFIED (no IDF header and no specs/c3-registers.csv row  | **cannot**: a register name with no IDF header; the HLE replaces the controller that spins on it |
| V26 | `regi2c` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V27 | `regi2c.*` (reset_domains) | B | matrix: the regi2c master is in the digital domain. Whether the analog slave bytes survive a core reset is UNVERIFIED; the emulator res | **cannot**: the analog slave bytes are rewritten by rtc_init on every boot before any app code can read them |
| V28 | `regi2c.wait` (wait) | B | ROM BBPLL / O-code calibration; boot-path seed row. Register name UNVERIFIED: decoded from ROM disassembly, IDF has no define  | **cannot**: a register name decoded from ROM, not a behaviour; no read names a register |
| V29 | `regi2c.slave register space` (overrides) | B | is known to pass both the bootloader and ADC calibration. Hardware defaults are themselves UNVERIFIED  last written byte, 0x00 when never wri | **cannot**: the slave defaults are overwritten by the bootloader before the app runs |
| V30 | `regi2c.REGI2C host command registers` (overrides) | B | offsets are named here so the class is spec data rather than a constant in periph/regi2c.rs. Register name UNVERIFIED, as the header of this file records  the command  | **cannot**: a register name, not a behaviour |
| V31 | `rsa` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V32 | `rsa.RSA_X_MEM` (overrides) | A | at 66). That X is left unchanged is not observed (IDF writes X before every operation), so it stays UNVERIFIED  the base or first factor X; left unchanged by ev | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V33 | `rsa.RSA_MULT_START` (overrides) | A | N (167 MULT_START at 2 words and 64 MOD_MULT_START at 66). Every LENGTH + 1 is even, so the odd case stays UNVERIFIED  write-trigger: Z = X x Y, X the first n words of | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V34 | `rsa.RSA_INT_ENA` (overrides) | B | on it  bit 0, reset 1, gates the level of source 47; the completion latches whether or not it is set (UNVERIFIED) | **untouched**: ledger: only probes/probe_crypto reaches the block, and the audit recorded in the row that no corpus run exercises this case |
| V35 | `rtc_cntl.*` (reset_domains) | B | the register table: it restarts at 0 wherever these scopes reset, so at a power-on and at a SYS_ reset. UNVERIFIED, and the one place the reset tables contradict each other ... | **settled** (B, step 3): the RTC counter restarts at the `SYS_` reset, `reset:65 BOOT\|boot2` |
| V36 | `saradc` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V37 | `saradc.APB_SARADC_1_DATA_STATUS` (stable_read) | B | row: the raw code is a function of the modeled button state (up 0, down 433, ok 862, released 4095, all UNVERIFIED), not of timing | **settled** (B, step 3): 4095 idle and the pressed medians, `regs:395 ADC\|gpio0_idle` `regs:402 ADC\|press_1 ~min:*,max:*,samples:*` `regs:403 ADC\|press_2 ~median:1%,min:*,max:*,samples:*` `regs:404 ADC\|press_3 ~min:*,max:*,samples:*` (down one code below, the synthesized eFuse curve; the rest is how long the owner held each key) |
| V38 | `sensitive` (header) | B | sensitive block (MUST, class B, reset domain UNVERIFIED, IRQ sources 55 to 60); base IDF soc/esp32c3/register/soc/reg_base.h (SENSITIVE,  | **cannot**: the lock bits are set again by every boot before app code runs; the only observable is the absence of a reboot loop, which every device boot already shows |
| V39 | `sensitive.*` (reset_domains) | B | The exact hardware reset domain is UNVERIFIED: the behavior is inferred from the fact that devi | **cannot**: as the header row above |
| V40 | `sha` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V41 | `sha.SHA_DMA_BLOCK_NUM` (overrides) | A | (bits 31 to 6 dropped) is not exercised and rests on the TRM, class B inside the row; BLOCK_NUM 0 stays UNVERIFIED  the block count of the next DMA run, bits 5 to 0 | **untouched**: BLOCK_NUM 0: no corpus run starts a zero-block run (the row) |
| V42 | `sha.SHA_DMA_START` (overrides) | A | then DMA_CONTINUE runs of 62 and 2 blocks) with the device capture device-probe_timing-20260923T101040Z. The UNVERIFIED items of the value have no source here. Unit test | **untouched**: the row: the UNVERIFIED items of the value have no corpus path |
| V43 | `spi0` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V44 | `spi1` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V45 | `spi2` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V46 | `system` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V47 | `system.wait` (wait) | B | and the FreeRTOS port's use of it is UNVERIFIED; boot-path seed | **cannot**: a question about the FreeRTOS port's source, not silicon; the ledger shows every image writing SYSTEM_CPU_INTR_FROM_CPU_0 first |
| V48 | `systimer` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V49 | `systimer.SYSTIMER_TARGET0_CONF` (overrides) | B | the order pk writes at run time, and the device ticks; the silicon mechanism is UNVERIFIED. UNVERIFIED for a silicon check: (1) whether a PE | **settled** (A, step 3b): the IDF order, the unloaded period and the zero period, `timing:72 TIME\|systimer_idf_order ~first_after_mode:0.2%,first_after_load:0.2%,cadence:0.2%` `timing:73 TIME\|systimer_mode_without_load ~first_after_mode:0.2%,cadence:0.2%` `timing:74 TIME\|systimer_zero_period ~first_after_load:*` (the zero period's offset is the probe's own APB polls) |
| V50 | `timg0` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V51 | `timg0.TIMG_WDTCONFIG5` (overrides) | B | IDF soc/esp32c3/register/soc/timer_group_reg.h (+0x05C): same rule as WDTCONFIG2; after stage 3 the behavior is UNVERIFIED and the model stops the counter  hold of watchdog | **untouched**: ledger: no image touches TIMG_WDTCONFIG5 |
| V52 | `timg1` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V53 | `timg1.TIMG_WDTCONFIG5` (overrides) | B | IDF soc/esp32c3/register/soc/timer_group_reg.h (+0x05C): same rule as WDTCONFIG2; after stage 3 the behavior is UNVERIFIED and the model stops the counter  hold of watchdog | **untouched**: ledger: no image touches TIMG_WDTCONFIG5 |
| V54 | `uart0` (header) | B | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V55 | `usj` (header) | A | (block header) window size 0x1000 | **cannot**: the next block starts at base + 0x1000 (IDF soc/esp32c3/register/soc/reg_base.h), so only aliasing inside the window could show a smaller decode, and reading the reserved offsets of a live block (FIFO windows among them) is not a read-only probe |
| V56 | `usj.*` (reset_domains) | B | them across every reset except the Chip-scope one of a power cycle. Clearing the line latch with the flag is UNVERIFIED and follows the re-enumeration default ( | **cannot**: the line latch is host state across a re-enumeration, which the device cannot report |
| V57 | `usj.USB_SERIAL_JTAG_FRAM_NUM` (overrides) | B | FRAM_NUM (+1 per emulated SOF; FRAME_NUM frozen in U0). Width UNVERIFIED: the IDF bitpos comment writes [11:0] while IDF v5.5.3 soc/esp32 | **settled** (B, step 3): 11 bits, `regs:400 USJ\|fram_num_width ~max:1%` (`max` is the host's SOF phase against the samples) |
| V58 | `usj.USB_SERIAL_JTAG_OUT_EP1_ST` (overrides) | B | OUT_EP0..2_ST (OUT_EP1_REC_DATA_CNT = OUT FIFO byte count). Width UNVERIFIED, the same comment-versus-macro conflict in the IDF header as USB_SERIA | **untouched**: ledger: no image touches the register |
| V59 | `usj` host link (`USB_SERIAL_JTAG_INT_RAW` SOF, `*` reset_domains; `periph/usj.rs`, `pemu-machine` `sleep.rs`) | B | what `usb_serial_jtag_is_connected()` answers after a reset and after a deep-sleep wake. The answer itself is modelled, class B: IDF's monitor reads the SOF raw bit, and the INT_RAW row raises SOF once per emulated millisecond while the link is enumerated. What is not: the reset_domains row keeps the host connection across every MCU reset, so the model never drops the link at a SYS_ reset, while the capture `device-probe_campaign_reset-20260924T160245Z-run1` shows `[capture: link down 283 ms]` after `SWD | **settled** (B, step 3): the waits after the reset and after the wake, `reset:64 WAIT\|boot2` `reset:74 WAIT\|boot3` |

The `rsa` header row (V31) also says "MUST for Passport Keys pairing if used, path UNVERIFIED":
**untouched** in this ledger, because no pairing runs in the 28 runs and `pk`'s 15 s with the
button walk never reaches the RSA block (only `probes/probe_crypto` does).

## 6. Other spec files

| # | Row | Assumed | Disposition |
|---|---|---|---|
| S1 | `st7789-boot.toml` vendor table row 13 | 60 Hz frame rate in the ST7789V table, UNVERIFIED for the P3 | **cannot**: the panel's MISO is not wired on this board, so nothing reads the panel back |
| S2 | `st7789-boot.toml` vendor table row 16 | P3-specific command, meaning UNVERIFIED | **cannot**: as S1 |
| S3 | `oracle-qemu-regions.toml` | region sizes 0x1000 UNVERIFIED | **cannot**: as the window sizes of section 5 |

## Appendix: field access types in `specs/c3-registers.csv`

321 fields carry `access_basis` UNVERIFIED: efuse 63, spi2 41, gdma 39, i2c0 26, spi0 25, spi1 25,
uart0 22, uart1 22, extmem 15, rmt 14, usj 12, uhci0 7, systimer 5, timg0 2, timg1 2, apb_ctrl 1.
**cannot** in this step, as a group: an access type is shown only by writing the field and reading
it back, a write sweep over live peripheral registers (and, for efuse, over the eFuse controller)
that the rules of this step exclude. Where a class C row covers one of these registers,
`probe_campaign_regs` reads its boot value, which checks the reset value, not the access type.

## Found while taking the inventory

- `specs/hle/idf-5.5.3/wifi.toml` says "the scan timing is class C" in the paragraph that opens
  `[driver]`, while the bullet on the scan sweep in the same section makes it class A from
  `device-wifi_facts-20260917T193712Z`. The sweep is not in section 4; the stale sentence is for
  step 3 (no class changes here).
- The USJ enumeration delay has no producer. `UsjModel::set_enumeration_delay` is called only by
  the model's own unit tests, so under the `device` profile the delay is the `fast` value 0, not
  the 100 ms its field documentation and `sleep.rs` describe: the link, and SOF with it, is back
  at the wake instant. Found while checking what the emulator answers to `usb_serial_jtag_is_connected()` (V59); no model is changed here, it is for step 3.
  **Done in "Step 3: reset and deep sleep"**: the profile rows `usj_enum_wake_ps` and `usj_enum_reset_ps` feed it.
- `probe_wifi_http`, `pkgatt` and `usj_echo` are built on the default table, which `plan_flash`
  refuses on the device (probes/README.md). That is why the open-network association (H4), the
  pkgatt log lines (H9) and the USJ drain (T9) are taken by `probe_wifi_assoc` and the campaign
  probes rather than by the existing probes that first measured them.
