# G3 behavior note

This note restates, as behavior only, facts the design relies on about the firmware images, the
silicon and the oracles. They come from the maintainers' own black-box analysis of the official
firmware, the Passport Keys image and the ESP-IDF radio blobs, and from earlier prototype
experiments: G1 measured host cost (the `G1` build, and `G1+BC` with a block cache), G3 ran the
images on esp32sim with hooks, snapshots and debug stops. Those analyses are not published, so a
section is UNVERIFIED unless it names a test, probe or capture that shows its facts.

- **Author role.** Written under the `CONTRIBUTING.md` "Clean room" rules. It says what the
  firmware, the silicon or an oracle run observably did. It carries no source path, file name, line
  number or code shape of esp32sim or QEMU, and it describes no oracle internals beyond what a
  black-box run shows.
- **How to cite.** `specs/notes/g3-behavior.md` section id, written `g3-behavior <id>`, for example
  `g3-behavior g3-menu-geometry`. Ids are stable; a section that is superseded keeps its id and says
  so.
- **Confidence** is one of: *measured on the device*; *measured on an oracle* (esp32sim, QEMU or
  ESP-EMU run as a black box); *estimated*. Anything not measured is marked UNVERIFIED.

## Conventions used in this note

- **Images.** `pk` is the Passport Keys image and `official` the official FoloToy image (firmware
  corpus ids). Addresses such as `app_main` 0x42009c80 are symbols of the corpus ELFs.
- **Oracle configuration** unless a section says otherwise: esp32sim run as a black box with the ECO7
  mask ROM (`esp32c3_rev101_rom.elf`, banner `ESP-ROM:esp32c3-eco7-20230720`), a synthesized v1.1
  eFuse (g3-synth-efuse), placeholder MAC `02:00:00:c3:00:01`, reset cause 0x15, strap 0xa, an 8 MB
  flash image. Host: Apple M3 Pro, rustc 1.93.0, Node v26.7.0, Chrome 153.
- **Time units.** One CPU cycle is 1/160,000,000 s (160 MHz). The oracle executes one instruction
  per cycle. Its *instruction count* also includes cycles skipped while the core waits in WFI, so on
  idle firmware instruction count equals cycle count; *retired* counts only executed instructions.
  The system timer runs at 16 MHz, 10 CPU cycles per timer tick.
- **Stubs.** Some runs replace a firmware function by an immediate return with a fixed value; the
  section names them. `ESP_ERR_INVALID_STATE` is 0x103, `ESP_ERR_NOT_FOUND` 0x105.

## Index

"Used by" lists the files outside `specs/notes/` that cite the id.

| Id | Title | Used by |
|---|---|---|
| g3-menu-geometry | Menu card geometry | `crates/pemu-board/src/st7789.rs`, `tests/milestones/m5.rs`, `tests/milestones/m6.rs` |
| g3-menu-colours-invon | Menu colours and INVON | `crates/pemu-board/src/st7789.rs`, `crates/pemu-board/tests/st7789.rs`, `tests/milestones/m5.rs` |
| g3-irq-latency | Interrupt latency | `crates/pemu-soc-c3/src/intc.rs`, `crates/pemu-soc-c3/src/periph/systimer.rs` |
| g3-idle-chunk | Idle chunk size does not change guest state | |
| g3-wasm-idle-host-cost | esp32sim idle host cost, native and wasm | |
| g3-hook-api | Function-entry hooks with nested guest calls | `crates/pemu-hle/src/core.rs`, `crates/pemu-hle/src/log_synth.rs` |
| g3-nested-call-stack | Nested-call stack-overflow risk | `crates/pemu-hle/src/guest_call.rs` |
| g3-snapshot-determinism | Whole-machine snapshot determinism | |
| g3-debug-stops | Exact pause, breakpoint and write watch | `crates/pemu-machine/src/` (`stops.rs`, `run.rs`, `snapshot.rs`, `machine.rs`) |
| g3-stall-points | Stall points of the unmodified images | `crates/pemu-soc-c3/src/periph/saradc.rs` |
| g3-app-main-insns | Instruction counts at `app_main` | |
| g3-console-fidelity | Console lines and early timestamps | |
| g3-synth-efuse | Synthesized eFuse words | `crates/pemu-loader/src/efuse_image.rs` |
| g3-rom-data-table | ROM data-table back-fill | |
| g3-gdma-c3-layout | C3 GDMA register layout and the SPI2 DMA stall | |
| g3-adc-read-path | ADC oneshot read path and its cost | `crates/pemu-soc-c3/src/periph/saradc.rs`, `specs/blocks/saradc.toml`, `specs/timing-profiles.toml` |
| g3-narrow-access-widening | Narrow-access widening hazard | |
| g3-oracle-speed | Oracle interpreter speed | |
| g3-g1-cost | G1 cost figures | |
| g3-safari-performance-now | Whole-millisecond `performance.now()` in Safari | `web/src/worker/pacing.ts`, `web/src/audio/playback.ts` |

## g3-menu-geometry: Menu card geometry

**Behavior.**

- The `official` image draws its settled menu on a 240x320 portrait panel memory (MADCTL 0x00,
  COLMOD 0x55). All coordinates below are panel memory pixels, origin top left, as (x, y, w, h).
- Seven menu cards are laid out in two columns in the order Display, Button, Audio, Battery, Wi-Fi,
  BLE, Low Power. Card number i (0 based) sits at x = 11 + 112 per column step (column = i mod 2)
  and y = 52 + 47 per row step (row = i div 2). Every card is 102x40.
- Each card is drawn as an ink shadow block offset by (+5, +6) from the card, then the card with a
  4 px border. The inner fill area of a card is therefore (x+4, y+4, 94, 32).
- The title plate is (5, 8, 151, 33) with a 3 px border, inner fill (8, 11, 145, 27), and shows the
  text "FoloToy".
- Measured positions in panel memory after boot (3 s run, no key presses):

| Element | Tree node (UI dump) | Measured in panel memory |
|---|---|---|
| Display card, selected | (11, 52, 102, 40), bg #ffd928, border #ffffff | white border bounding box exactly (11, 52, 102, 40); yellow inner (15, 56, 94, 32) |
| Button card | (123, 52, 102, 40), bg #f4f4ea | paper inner (127, 56, 94, 32) |
| Audio card | (11, 99, 102, 40), bg #78909c | grey inner (15, 103, 94, 32) |
| Low Power card | (11, 193, 102, 40) | paper inner (15, 197, 94, 32) |
| Title plate | (5, 8, 151, 33), bg #f4f4ea | paper inner (8, 11, 145, 27) |

- Positions derived from the rule for the cards not listed: Battery (123, 99), Wi-Fi (11, 146),
  BLE (123, 146). Every other card follows the (x+4, y+4, 94, 32) inner rule
  and that the ink areas include the shadow blocks.
- Visible content of the settled boot menu: title plate "FoloToy", a cloud, the Display card
  selected, Button, "Audio [FAIL]" and "Battery [FAIL]" greyed with dark red text (Audio and Battery
  fail because no codec and no fuel gauge answer), Wi-Fi, BLE, Low Power, the mascot and grass.
- Panel activity until the settled boot menu (3 s): 40 RAMWR commands, 153,792 pixels written,
  308,099 SPI bytes, 280 GPIO events, 1 SWRESET, the bounding box of all writes is the full
  240x320. After two DOWN presses (3 s run, presses at 1.00 to 1.30 s and 2.00 to 2.30 s): 58 RAMWR,
  218,968 pixels, 438,649 SPI bytes, 388 GPIO events.
- Selection moves with the input: the firmware's selection variable `s_sel` goes 0, 1, 2 over the two
  DOWN presses (g3-debug-stops has the write times). The boot frame marks Display (index 0) and the
  frame after two presses marks Audio (index 2); the frame with Button selected was not imaged.
- The menu is entered (`enter_menu`) at 0.2023 s emulated; the console line
  `main: 就绪:Display=1 Button=1 Audio=0 Battery=0` appears at 343 ms.

**Confidence.** Measured on an oracle (esp32sim with a panel, SPI2 and ADC mounted, `official`
image, stubs `bsp_i2c_scan`, `bsp_audio_init` = 0x103, `bsp_battery_init` = 0x105). The tree nodes
come from an LVGL object-tree dump and agree with the pixels. The three derived card positions
are estimated from the rule. Not compared with the real glass (UNVERIFIED on the device).

## g3-menu-colours-invon: Menu colours and INVON

**Behavior.**

- The board initialisation sends INVON (0x21) to the panel (`BSP_LCD_INVERT_COLOR 1`), and INVON is
  still set after the two DOWN presses. The panel receives RGB565 pixels big-endian on the wire.
- The panel memory holds the LVGL style colours converted to RGB565 with no transform: no inversion
  and no byte swap. Measured in the boot frame:

| Style colour (firmware) | Role | RGB565 in panel memory | Pixels |
|---|---|---|---|
| UI_SKY 0x1689E8 | screen background | 0x145D | 27,077 |
| UI_INK 0x17202A | borders, shadows, text | 0x1105 | 14,712 |
| UI_PAPER 0xF4F4EA | unselected enabled card, title plate | 0xF7BD | 14,517 |
| 0x78909C | disabled card background | 0x7C93 | 5,115 |
| UI_YELLOW 0xFFD928 | selected card background | 0xFEC5 | 2,719 |
| 0xFFFFFF | selected card border | 0xFFFF | 1,489 |
| fail text | "[FAIL]" labels | 0x7904 | 199 |

- Card states (inner fill and border, measured by the most common inner colour and the colour of the
  border's second row):

| State | Inner | Border |
|---|---|---|
| enabled, not selected | 0xF7BD paper | 0x1105 ink |
| enabled, selected | 0xFEC5 yellow | 0xFFFF white |
| disabled (Audio, Battery), not selected | 0x7C93 grey | 0x1105 ink |
| disabled, selected | 0x7C93 grey (the grey wins over the yellow) | 0xFFFF white |

- Consequence for the panel model: with INVON set the glass shows panel memory unmodified; under
  INVOFF the glass shows the bitwise complement of memory. With this rule the
  settled menu shows the style colours above. The rule is kept as a switch, default "INVON shows
  memory".
- Backlight in the menu: LEDC channel 0 output enabled with duty 1023 of 1024 (99.9 %), both at boot
  and after the presses.

**Confidence.** Measured on an oracle (panel memory of esp32sim with a panel model, `official`
image). The INVON-to-glass rule is UNVERIFIED against the real glass; a photo of the INVON
appearance on the device would settle it.

## g3-irq-latency: Interrupt latency

**Behavior.**

- Routing observed in the `official` image: SYSTIMER comparator 0 (interrupt source 37) is the
  FreeRTOS tick on CPU interrupt line 5; SYSTIMER comparator 2 (source 39) is esp_timer on CPU line 3.
  `CONFIG_FREERTOS_HZ` is 1000, so one tick is 1 ms.
- Scenario: `official` image to the menu, no key presses, 10 s emulated, 18,620 interrupts taken in
  total. *Due* is the cycle at which the comparator matched (exact to one timer tick, 10 cycles);
  *raised* is the cycle at which the interrupt line went pending; *taken* is the instruction count at
  interrupt entry.

| Comparator | Fired / taken | due to taken, cycles | raised to taken, cycles | Histogram of due to taken |
|---|---|---|---|---|
| alarm 0, FreeRTOS tick, line 5 | 9,938 / 9,938 | mean 1.1, max 652 | mean 1.1, max 652 | 9,935 below 10; 1 in [64, 128); 2 in [128, 1024) |
| alarm 2, esp_timer, line 3 | 3,920 / 3,920 | mean 1.3, max 31 | mean 1.0, max 1 | 3,880 below 10; 40 in [20, 64) |

- The tick maximum of 652 cycles (4.1 µs) is guest behavior: raised-to-taken equals due-to-taken, so
  the line was pending while the firmware ran with interrupts masked. An exact model shows the same
  masked windows.
- The 40 esp_timer samples at 20 to 31 cycles are an oracle artifact: esp32sim delivers device time
  once per 64-instruction round, so a comparator that falls due while the core executes is raised at
  the end of that round (up to 63 cycles late); once raised, the interrupt is taken within 1 cycle.
  The same round granularity lets a SYSTIMER or TIMG register read during execution return a value
  up to 63 cycles (0.39 µs) old. A core waiting in WFI reads no registers, so idle stretches are
  unaffected.
- After a peripheral register write, esp32sim re-evaluates interrupt lines before the next
  instruction, so write-triggered interrupts (for example FROM_CPU yields) are taken at most one
  instruction later.
- Differential comparisons of interrupt entry counts against esp32sim therefore need a tolerance of
  63 cycles for timer interrupts that fall due while the core executes, and none for idle wake-ups or
  write-triggered interrupts.
- Interrupt matrix behavior as observed through the oracle, which boots both images: a level-type
  CPU line follows its source; an edge-type line latches on a rising source edge until cleared through
  CPU_INT_CLEAR; the delivered line is the highest-priority enabled line whose priority is at or above
  the threshold (a priority equal to the threshold delivers). Register offsets in the matrix block:
  one map register per source from offset 0 (62 sources), CPU_INT_ENABLE 0x104, CPU_INT_TYPE 0x108,
  CPU_INT_CLEAR 0x10C, EIP_STATUS 0x110, line priorities 0x114 to 0x190, threshold 0x194.
- Source numbers follow the order of the IDF `INTERRUPT_CORE0_*_MAP_REG` registers: APB_CTRL 14,
  GPIO 16, SPI2 19, I2S0 20, UART0 21, UART1 22, LEDC 23, EFUSE 24, USB Serial/JTAG 26, RTC_CORE 27,
  I2C_EXT0 29, TG0 T0 32, TG0 WDT 33, TG1 T0 34, TG1 WDT 35, SYSTIMER_TARGET0 to 2 37 to 39,
  APB_ADC 43, DMA channels 0 to 2 44 to 46, RSA 47, AES 48, SHA 49, FROM_CPU_INTR0 to 3 50 to 53.
  Each number is the map register offset divided by 4 in IDF `interrupt_core0_reg.h` (62 map
  registers, offsets 0x000 to 0x0F4). FreeRTOS yields by writing the FROM_CPU_INTR registers
  (SYSTEM + 0x28 to 0x34). The esp32sim C3 notes warn that a numbering taken from `soc/interrupts.h`
  can be shifted against the map register order (UNVERIFIED); the map register order is authoritative.

**Confidence.** Latency table and routing: measured on an oracle (esp32sim, `official`). The round
granularity: measured on an oracle (latency histogram) and consistent with the analysis.
Matrix semantics: measured on an oracle only in the sense that both images boot and take these
interrupts; threshold equality is UNVERIFIED on the device. Source numbers: checked against the IDF
map register offsets; I2S0 20 and APB_ADC 43 are not defined in the oracle, so no run exercised them.

## g3-idle-chunk: Idle chunk size does not change guest state

**Behavior.**

- *Idle chunk* is the number of cycles esp32sim advances per scheduler step while every core waits in
  WFI with no pending interrupt line. Each step is also clamped to the next device deadline and the
  next scripted input, so no step crosses an event.
- Scenario: `official` image to the menu, no key presses, 10 s emulated, chunk sizes 1, 64 and 1024.
  All three end with identical guest state: the same RAM hash, the same USB console bytes, the same
  full machine state hash, 51,761,580 retired instructions and 18,620 interrupts, and identical
  interrupt latency tables (g3-irq-latency).

| Idle chunk, cycles | Wall time for 10 s emulated (native) | Host rate, emulated cycles per second |
|---|---|---|
| 1 | 112.6 to 114.3 s | about 14 M |
| 64 | 2.47 to 2.71 s | about 590 to 650 M |
| 1024 | 0.936 s | about 1,709 M |

- Why the guest cannot tell the sizes apart: the step that precedes a device deadline ends before the
  deadline tick, and the machine crosses the due tick in single-cycle steps, so every timer event is
  delivered at the same cycle whatever the chunk size. The design rule that follows is "idle skip may
  jump any distance as long as it never passes the next event"; skipping straight to the next event
  is the limit of this rule.
- The chunk size does not bound interrupt latency and does not set register staleness; staleness
  during execution comes from the 64-instruction round (g3-irq-latency).
- Scope of the evidence: one idle scenario without audio or radio traffic. The analysis recommends
  keeping 64 in the oracle, or 1024 only after a regression on audio and radio scenarios, and never
  below 64.

**Confidence.** Measured on an oracle (esp32sim, `official` menu, native). Equality for skips larger
than 1024 cycles and for audio and radio scenarios is UNVERIFIED.

## g3-wasm-idle-host-cost: esp32sim idle host cost, native and wasm

**Behavior.**

- With the default idle chunk of 64 cycles, an idle C3 in esp32sim runs 2.5 million scheduler
  iterations per emulated second, each delivering time to every clocked device and re-deriving the
  interrupt lines. This loop, not the interpreter, sets the host cost of idle firmware.
- Native figures (Apple M3 Pro):
  - `official` menu, 10 s idle: 2.47 to 2.71 s wall at chunk 64 (about 4x faster than real time),
    0.936 s at chunk 1024.
  - `official` menu, 3 s with two DOWN presses: 1.042 to 1.060 s run-loop wall (about 2.9x faster than
    real time); effective rates of 443 to 970 million counted instructions per second across the gate
    runs, because idle-skipped cycles count as instructions.
  - IDF `hello_world` for the C3, 26 s emulated, mostly idle: 5.5 s wall (4.7x faster than real time),
    13.4 MB peak memory.
- wasm build of the same oracle: 7,340,382 B module (not post-optimised), 12 to 18 ms to instantiate
  and load an image, 27.5 MB linear memory, no SharedArrayBuffer. On busy code it reaches 0.70 to 0.74
  of native speed (g3-oracle-speed).
- Measured idle cost in G1 (chunk 64, `official` image with the rev3 ROM, scenario O3 of g3-g1-cost:
  boot, idle, 20 DOWN clicks, idle; host seconds per emulated second, median of 2 runs, only time
  spent inside the run call counted):

| Host | Thread | Idle 2 to 3 s | Idle 12 to 14.5 s | Fitted idle cost c |
|---|---|---|---|---|
| native (M3 Pro) | n/a | 0.200 to 0.203 | 0.201 to 0.213 | 0.177 |
| Node 26.7 | main | 0.316 to 0.318 | 0.316 to 0.317 | 0.276 |
| Chrome 153 (headless) | main | 0.307 to 0.316 | 0.308 to 0.314 | 0.271 |
| Chrome 153 (headless) | dedicated worker | 0.306 to 0.307 | 0.307 to 0.333 | 0.280 |
| Safari 27 | main | 0.291 to 0.297 | 0.291 to 0.298 | 0.257 |
| Safari 27 | dedicated worker | 0.292 to 0.297 | 0.295 to 0.318 | 0.269 |

- *c* is the host cost of one fully idle emulated second, fitted per host from the cost model of
  g3-g1-cost; the measured idle phases also contain the 2.7 to 2.8 MIPS the firmware still executes.
  Summary: c is 0.26 to 0.28 in wasm and 0.18 natively. 10 s of idle menu therefore cost about 2.9 to
  3.2 s of host time in wasm and about 2.0 s natively in G1 (the G3 figure of 2.47 to 2.71 s above
  used the ECO7 ROM and a different build; both are native).
- The block-cache prototype of G1 leaves c unchanged (0.26 to 0.28 in wasm, 0.18 natively): it speeds
  up executed instructions, not the idle loop. With the block cache, 86 to 90 % of the menu-idle host
  time in wasm is idle bookkeeping rather than guest instructions.
- Where the idle host time goes (native sampling profile, `official` menu, 60 s emulated in 13.1 s):
  mostly re-deriving interrupt lines after each 64-cycle idle step, then delivering device time,
  computing the next device deadline, and the timer models; instruction decode and execution are a
  small share. The guest meanwhile retires about 2.7 MIPS.
- Main thread and dedicated worker are within 5 % of each other; Safari 27 is about 5 % cheaper than
  Chrome 153, and Node 26 matches Chrome.
- Target stated by G1 (estimated, UNVERIFIED): advancing idle cores straight to the next device
  deadline instead of in 64-cycle steps should bring idle cost close to the cost of the instructions
  actually executed, about 0.03 to 0.05 host s per emulated s in wasm. The design uses c <= 0.05 as
  its planning figure.

**Confidence.** Native G3 figures and wasm module figures: measured on an oracle. G1 idle phases and
the fitted c: measured on an oracle (esp32sim as native binary and as wasm module on one host, Apple
M3 Pro on AC power, foreground Safari window, headless Chrome). Whether a background tab, battery
power or a slower Mac keeps these figures is UNVERIFIED. The event-driven idle target is estimated.

## g3-hook-api: Function-entry hooks with nested guest calls

**Behavior.**

- A function-entry hook fires when execution is about to run the first instruction of a function
  (entry address from the ELF symbols), before that instruction executes, independent of how the
  engine groups instructions.
- A hook runs an ordered list of guest calls. Each call passes up to 8 integer or pointer arguments in
  a0 to a7 (RISC-V ILP32 calling convention); a string argument is copied NUL-terminated onto the
  interrupted stack below its `sp`, and its address is passed. The call starts with `sp` aligned to
  16 bytes. Struct and floating-point arguments were not supported.
- The return address given to a nested call is an address that is not mapped on the C3, so a return
  that is not intercepted faults instead of running on. A return is recognised only when the pc
  reaches that address and `sp` equals the value recorded when the call was set up; the `sp` match
  keeps another task that happens to return to the same address from being taken for the call.
- On return the result in a0 is recorded. The next call in the list then starts; after the last call
  all 32 integer registers and the pc are restored, and the hooked function runs as though it had
  just been called.
- Accounting: setting up and restoring a call costs zero instructions. The instructions of the nested
  calls count normally and advance emulated time, so ticks, interrupts and task switches happen during
  them. A nested call that blocks (for example `vTaskDelay`) lets other tasks run.
- A hook does not fire while another nested call is outstanding.
- The hook table and an outstanding nested call are machine state: they are saved in a snapshot, and
  a fresh process that loads a snapshot taken during the call completes it at the same instruction
  count without the hook being configured again.
- Every execution path must honour hooks. In the G3 experiment the hooks worked on one run loop only
  and were not seen by the per-instruction trace path or the browser run loop; results must not
  depend on which path runs.
- Measured example, `official` image, hook at `app_main` entry with two calls,
  `esp_log(0x13, "G3", "G3 hook: nested esp_log from app_main entry\n")` then `vTaskDelay(1)`:

| Event | Instruction count | Emulated time |
|---|---|---|
| hook fires at `app_main` entry, `sp` 0x3fcaea20 | 9,942,005 | 0.062137 s |
| `esp_log` returns 0x2c (44, the message length) | 9,949,372 | |
| `vTaskDelay(1)` returns, one 1 ms tick later | 10,072,319 | 0.062183 to 0.062952 s |
| `app_main` body entered | 10,072,319 | |

- Effect of the hook over a 3 s run: the USB console grows by 45 bytes (the 44-byte line plus CR),
  retired instructions 37,910,615 instead of 37,902,111, interrupts 5,521 instead of 5,520. A rerun
  is identical in every hook log line and in the final digest. A pause at instruction 10,010,001
  (during `vTaskDelay`) finds the pc in `esp_cpu_wait_for_intr` + 0x18, the idle task running and the
  main task blocked.
- Firmware facts for log synthesis: `esp_log_write` is not linked in either image (dead-stripped);
  the IDF 5.5 `esp_log(esp_log_config_t config, const char *tag, const char *format, ...)` is present
  at 0x40395f02 in `official` and 0x40395bba in `pk`. `config` 0x13 is level INFO (3) in bits 0 to 2
  (`ESP_LOG_LEVEL_LEN` 3) plus `REQUIRE_FORMATTING` in bit 4 (IDF `esp_log_level.h`,
  `esp_log_config.h`). In `official`, `app_main` is 0x42009c80 and `vTaskDelay` 0x403900ca.

**Confidence.** Measured on an oracle (esp32sim with the G3 hook experiment, `official` image, stubs as
in g3-menu-geometry, two DOWN presses). Symbol addresses are facts of the corpus ELFs.

## g3-nested-call-stack: Nested-call stack-overflow risk

**Behavior.**

- A nested guest call takes its scratch space from the interrupted stack: a gap of 32 bytes below
  `sp`, the string argument bytes, and a 16-byte frame with `sp` aligned to 16 bytes. That is at least
  48 bytes plus strings before the callee pushes its own frame; the callee's own stack use comes on
  top (for `esp_log` with formatting, size UNVERIFIED).
- In the G3 experiment nothing compared this space with the limit of the stack in use. A hook placed
  where the stack is nearly full overwrites memory below the stack silently. The firmware's own
  checks do not catch it in that oracle: FreeRTOS stack watermark checks run only later, the IDF
  stack-end watchpoint is inert there (trigger CSRs are stored without effect), and ASSIST_DEBUG,
  which holds the hardware stack guard, is plain register storage there.
- Inside an interrupt handler the interrupted stack is the ISR stack, which is 1536 B in these images
  (`CONFIG_ESP_SYSTEM_ISR_STACK_SIZE`), so the risk is largest for hooks on functions that can run
  in ISR context.
- The overflow was not provoked in G3. The only measured hook ran on the main task at `sp` 0x3fcaea20
  and did not overflow.

**Confidence.** Estimated: a risk read from the scratch-space layout of the G3
experiment; no overflow was provoked (UNVERIFIED). The inert watchpoint and ASSIST_DEBUG storage are
measured on an oracle (first-access logs and boot behavior).

## g3-snapshot-determinism: Whole-machine snapshot determinism

**Behavior.**

- Test shape: a straight run (A), a run that saves a checkpoint and continues in the same process
  (B), and a fresh process that loads the checkpoint and runs on (C). The final digest compared is:
  emulated time, cycles, instruction count, retired instructions, RAM hash, flash hash, USB console
  byte count and hash, full state size and hash, and interrupt count.

| Scenario | Checkpoints | Final digest (A = B = C) |
|---|---|---|
| `pk`, 3 s (busy-waits in the BLE baseband poll from about 0.36 s, hardly idles) | 0.5 s | cycles = instructions 480,000,060; retired 443,760,883; USB 3,463 B; state 9,484,691 B |
| `official` menu with two DOWN presses, 3 s (mostly idle) | 0.5 s; 1.05 s, inside the first press | cycles = instructions 480,000,000; retired 37,902,111; USB 3,445 B; state 9,629,742 B; 5,520 interrupts |

- In every case the USB console bytes of A, B and C are identical byte for byte. Snapshots taken at
  exact instruction counts (g3-debug-stops) and during an outstanding nested call (g3-hook-api) also
  restore to identical final digests.
- State beyond memory and device registers that a snapshot must carry to pass these tests: the
  partially executed scheduling round (the rest of an execution quantum or of an idle step, and which
  cores run or wait), a breakpoint being resumed, the hook table, an outstanding nested call and its
  log, scripted input events and their cursor, console streams and reboot counters. Host
  configuration is not machine state and was not saved: observers, pacing, console routing, stop
  limits, pause targets, breakpoints, watch ranges, idle chunk size and debug flags.
- Size of a whole-machine snapshot: 9.48 to 9.63 MB, of which 8,388,608 B is flash, 393,216 B and
  131,072 B are the IROM and DROM copies, 400 KiB SRAM, 8 KiB RTC slow RAM, 153,600 B panel memory,
  and the rest device registers and console streams. Storing flash as a delta against the loaded
  image and reloading the ROM from its ELF would bring the `official` menu snapshot to about 717 KB
  (computed from buffer sizes, not implemented).
- Speed (native): serialize 0.80 to 1.27 ms, write 1.56 to 2.66 ms, read 1.13 to 2.46 ms, deserialize
  0.34 to 0.52 ms.
- Failure modes: a state field left out of the snapshot restores to its fresh value, and that shows
  only as a digest difference between straight and restored runs. The design therefore makes the
  A/B/C comparison a CI gate on at least three scenarios: `pk` busy boot, `official` menu with key
  presses, and a hooked run paused during the nested call. A snapshot format without a per-model
  version either fails to load after a model change or restores garbage.
- Checkpoints requested by emulated time that end an idle step early matched the straight runs, but
  only empirically; a pause at an exact instruction count is non-perturbing by construction and is the
  primitive to snapshot at.

**Confidence.** Measured on an oracle (esp32sim with the G3 snapshot experiment, native).

## g3-debug-stops: Exact pause, breakpoint and write watch

**Behavior.**

- **Pause at an exact instruction count.** The pause lands exactly on the requested count whether it
  falls in an idle stretch or while the core executes. The count includes idle-skipped cycles. On
  resume the machine first finishes the interrupted round and only then delivers device time for it,
  so device ticks land on the same instruction boundaries as in an uninterrupted run. In-process
  continuations and fresh-process restores of each pause reach final digests identical to the
  straight run.

| Requested | Reached | Where | Bus cycles at the pause |
|---|---|---|---|
| 100,000,001 | 100,000,001 | idle, `esp_cpu_wait_for_intr` + 0x18, t = 0.625 s | 99,999,998 |
| 168,000,037 | 168,000,037 | idle, same pc, t = 1.05 s | 167,999,998 |
| 400,000,063 | 400,000,063 | executing, `lv_style_prop_get_default` + 0x7e, t = 2.5 s | 400,000,026 |

- **Breakpoint** at `lv_timer_handler` (0x4204f2be in `official`): the stop happens before the
  instruction at that pc executes. Resuming executes that instruction exactly once, and the resume
  marker is cleared once passed, so it does not remain in the final state. 119 hits in 3 s: the first
  at instruction 31,145,995 (t = 0.194662 s, ra 0x42035b24), the second at 32,312,646, the third at
  38,019,849. The final digest, full state hash included, equals the run without the breakpoint.
- **Write watch** on `s_sel` (0x3fcab5cc, 4 bytes, `official`): the stop happens right after the
  writing instruction and reports address, value and the writer's pc. Writes through the IRAM view
  0x40380000 to 0x403E0000 reach the same memory as DRAM and must be matched there too. The watch
  covered CPU writes to SRAM only, not flash, register or DMA writes.

| t | Instruction count | Value | Writer |
|---|---|---|---|
| 0.056398 s | 9,023,796 | 0x0 | `memset` + 0x1e (ROM, .bss clear) |
| 1.492153 s | 238,744,520 | 0x1 | `on_key` + 0xf0 |
| 2.492153 s | 398,744,520 | 0x2 | `on_key` + 0xf0 |

- The two `on_key` writes come 0.492 s after each injected press edge (presses at 1.00 to 1.30 s and
  2.00 to 2.30 s); which edge of the ADC level the button driver reports was not examined
  (UNVERIFIED). A `wait` on `s_sel` equal to 1 is implementable as this watch.
- **Cost** (native, busy `pk` stretches, 7 M and 400 M instructions): all stops armed cost at most
  about 5 %; an enabled watch with 87 stops cost 4.6 % on the long stretch; an armed pause and a
  breakpoint were not measurable. On the mostly idle `official` menu, 3 s with 122 stops ran in
  1.046 to 1.060 s against 1.044 to 1.052 s without stops.
- **Pitfall found:** after a debug stop, counting the resumed partial round as a full 64-instruction
  quantum toward an instruction limit made runs stop at 399,999,996 instead of 400,000,060 with
  identical guest state. A resumed partial round counts only its remaining instructions toward stop
  limits.

**Confidence.** Measured on an oracle (esp32sim with the G3 debug-control experiment, `official` and
`pk`, native).

## g3-stall-points: Stall points of the unmodified images

**Behavior.**

- Shape shared by every stall below: the firmware sets or waits for a register bit that the hardware
  changes by itself, in a block the emulator only stores. The polling task spins while the 1 kHz tick
  is still served; unmodelled blocks do not fault. Unless a row says otherwise the other blocks are
  modelled well enough for both images to boot.
- Boot depth with no SPI2, I2C0, APB_SARADC, I2S or BLE baseband model:

| Stage | `pk` | `official` |
|---|---|---|
| Mask ROM (rev3; ECO7 with a v1.1 eFuse) | yes | yes |
| Second-stage bootloader, partition table, image load and verify | yes | yes |
| App start: `cpu_start`, heap init, `spi_flash`, `sleep_gpio`, scheduler | yes | yes |
| `app_main` | yes (g3-app-main-insns) | yes |
| `bsp_i2c_init` | returns, `I2C ready` printed | returns; the I2C scan then takes 5.6 s |
| `bsp_display_init` | stalls (row 1) | stalls (row 1) |

- Stalls in the order the images meet them, each found by removing the previous one (stubbed function
  or added block model):

| # | Stall | Where the firmware spins | Image | Evidence |
|---|---|---|---|---|
| 1 | GP-SPI2 (0x60024000) absent | `bsp_display_init` to `esp_lcd_new_panel_io_spi` to `spi_bus_add_device` to `spi_hal_init`; `spi_ll_apply_config` (IDF `hal/esp32c3/include/hal/spi_ll.h`) sets CMD.update and polls until it clears. `pk` arrives at t = 0.0442 s, instruction 7,072,324; 84.9 % of 48 M instructions on 4 pcs at `spi_hal_init` + 0xb0 to 0xb6. `official` arrives after its I2C scan (`bsp_i2c_scan` returns at 5.662 s) | both, both ROMs | call traces, profile |
| 2 | GP-SPI2 present, GDMA out channel never runs | the first panel command waits forever in `spi_device_polling_end` (polling `xTaskGetTickCount`); SPI2 asks for DMA data, but no out channel is running (g3-gdma-c3-layout) | `pk` | profile, SPI2 debug log |
| 3 | I2C0 (0x60013000) absent | no hang: every transaction times out and the drivers return errors. `official` probes 0x08 to 0x77, 112 probes x 50 ms, warns "no I2C device found" at 5724 ms. `pk` warns that CW2017 did not ACK at 187 ms, and `bsp_battery_init` spends a 100 ms timeout (0.1836 to 0.2829 s) | both | console, call trace |
| 4 | BLE baseband absent | ROM `r_rwip_time_get` sets bit 31 of baseband register 0x6003101C and polls until it clears, which never happens: 81.1 % of instructions (display stubbed) or 75.6 % (display modelled) on `r_rwip_time_get` + 0x1c/0x1e. Reached from `esp_bt_controller_init` at 0.1456 s (display stubbed) or 0.3587 s (display modelled), after the four `BLE_INIT` lines | `pk` | profile |
| 5 | APB_SARADC (0x60040000) absent | `adc_oneshot_hal_convert` to `adc_oneshot_ll_get_event` (IDF `adc_ll.h`) in the esp_timer task polls the ADC1 done bit; the task starves `main`: 86.5 % (display, LVGL stubbed) or 86.2 % (display modelled) of instructions | `official` (`pk` reaches the ADC only after BLE) | profile |
| 6 | I2S0 (0x6002D000) absent | `i2s_channel_enable` to `i2s_tx_channel_start` to `i2s_ll_tx_update` (IDF `i2s_ll.h`) polls `tx_update` until it clears; `main` stops there at 0.0632 s (display, LVGL stubbed) or 0.2027 s (display modelled). ESP-EMU 0.42.0 stops on the same poll (UNVERIFIED: an earlier run, not repeated) | `official` | profile |

- Identity stall: the ECO7 ROM with an eFuse that says wafer v0.4 loads the bootloader, which takes the
  pre-ECO7 ROM path and stops with `Guru Meditation Error: Core 0 panic'ed (Load access fault)` at
  PC 0x4004c16c, MTVAL 0x5274. ESP-EMU shows the same fault. With a v1.1 eFuse the image boots
  (g3-synth-efuse).
- Unmodelled register blocks the firmware touches in the first 0.5 s (distinct registers; `pk` with
  the display stubbed, `official` with display, LVGL and ADC stubbed):

| Block | Base | `pk` | `official` | Consequence when the block only stores |
|---|---|---|---|---|
| IO_MUX | 0x60009000 | 22 | 22 | harmless |
| SENSITIVE (PMS) | 0x600C1000 | 20 | 20 | harmless |
| ASSIST_DEBUG | 0x600CE000 | 7 | 7 | harmless |
| RTC_I2C (regi2c analog) | 0x6000E000 | 4 | 4 | bootloader and ADC calibration writes read back; harmless |
| APB_SARADC | 0x60040000 | 7 | 7 | calibration runs; oneshot conversion never completes (row 5) |
| APB_CTRL beyond the RNG | 0x60026000 | 4 | 4 | Wi-Fi and BT clock and memory power bits; harmless |
| I2C0 | 0x60013000 | 23 | 5 | no transaction completes (row 3) |
| I2S0 | 0x6002D000 | not reached | 12 | row 6 |
| BT baseband | 0x60031000 | 56 | not reached | row 4 |
| Radio and PHY: 0x60011000, 0x6001D000, 0x6001C000, 0x60006000, 0x60005000 | | 2, 1, 1, 1, 1 | 0, 1, 1, 1, 1 | PHY blobs; nothing behind them |
| LEDC | 0x60019000 | | | plain storage is enough for `ledc_timer_config` and the backlight lines |

- With GP-SPI2, a C3-layout GDMA, a panel and a one-shot ADC present, and `bsp_i2c_scan`,
  `bsp_audio_init` (0x103) and `bsp_battery_init` (0x105) stubbed, `official` reaches its settled menu
  and answers key presses (g3-menu-geometry).

**Confidence.** Measured on an oracle (esp32sim run as a black box with function stubs, call traces
and profiles; rev3 and ECO7 ROMs). Arrival times depend on which blocks exist and on zero-cost I/O
(g3-console-fidelity); they are not device times.

## g3-app-main-insns: Instruction counts at `app_main`

**Behavior.**

| Image | Configuration | Instruction count at `app_main` | Emulated time |
|---|---|---|---|
| `pk` | rev3 ROM, eFuse v0.4 | 7.02 M | 0.044 s |
| `official` | rev3 ROM, eFuse v0.4 | 9.94 M | 0.062 s |
| `official` | ECO7 ROM, synthesized v1.1 eFuse | 9,942,005 (hook at entry, before the first instruction of `app_main`) | 0.062137 s |

- The two `official` configurations agree within 0.1 %. The `pk` count under ECO7 was not reported
  separately (UNVERIFIED).
- What is counted: every executed instruction as one cycle, plus cycles skipped while waiting in WFI.
  The reset-to-`app_main` stretch was used as a busy stretch for speed measurements, so idle cycles
  are a small part of it (the exact number is UNVERIFIED).
- In this oracle SPI flash commands, SHA and SPI transfers take zero cycles and the ROM stage has no
  flash-speed cost, so the count measures the CPU work of ROM, bootloader and app start only. It does
  not predict device time: the device prints `Calling app_main()` at 210 ms.
- The console timestamp of `Calling app_main()` in the ECO7 `pk` run is 87 ms, while instruction time
  at `app_main` is 44 ms in the rev3 `pk` run. The runs do not explain the gap (UNVERIFIED whether
  it comes from the ROM revision or from the time base of log timestamps).

**Confidence.** Measured on an oracle (esp32sim, native; the rev3 counts are also pinned by the
reset-to-`app_main` speed runs of 7.0 M and 9.9 M instructions).

## g3-console-fidelity: Console lines and early timestamps

**Behavior.**

- Comparison method: carriage returns removed; every `I (ms)` timestamp replaced by a placeholder;
  build-specific values masked (bootloader and app compile time, app version, ELF SHA,
  `Passport Keys <version>`, `boot=<id>`); a boot runs from one `ESP-ROM:` banner to the next; the
  device's boot is aligned with the emulator's first boot by a line diff. Device reference: a `pk`
  boot capture (IDF v5.5.3). Its first ROM banner is truncated, so the second boot is used: 77 lines,
  including `Saved PC` and the trailing `pk_app: link state -1 -> 0`.

| Run (`pk` unless noted) | Blocks present | Identical lines | Differences |
|---|---|---|---|
| R4: ECO7 ROM, device eFuse | stock oracle, no SPI2, I2C0, SARADC, I2S | all 57 emulator lines; 57 of 77 device lines (74.0 %) | only missing lines: `Saved PC`, and the 19 lines after `bsp_i2c` ready |
| R1: rev3 ROM, eFuse v0.4 | as R4 | 51 of 77 | as R4, plus ROM banner `esp32c3-api1-20210207` and `Build:Feb  7 2021`, `chip revision: v0.4` in bootloader and app, retention heap `len 00002950` (device `0000294C`), and one partition-table line that the oracle's console interleaving split (a run artifact, not a firmware difference) |
| R8: ECO7 ROM, device eFuse | R4 plus GP-SPI2 and a C3-layout GDMA | 65 of 67 emulator lines; 65 of 77 device lines (84.4 %) | see below |
| IDF `hello_world` for the C3, 26 s, three boots | stock | 205 of 208 lines of a real C3 module capture | the three `Saved PC:` lines |

- Missing in R8 (12 device lines): `Saved PC:0x400537d8`; the two CW2017 lines (`VERSION=0x0F` and
  the 520 mAh profile match), replaced by two warnings because no I2C device answers; and after the
  `BLE_INIT` lines: `phy_init`, `pk_app: ready`, three `adc_button` lines, `button`, `bsp_btn`,
  `Returned from app_main()`, the link state line.
- `Saved PC:` is printed by the C3 ROM after a reset that is not a power-on reset, from state the
  oracle does not keep. A device-exact console needs that state across chip resets (reset cause 0x15
  in these runs).
- Device lines identical in R4 after masking:
  - ROM: the banner, `rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)`, `SPIWP:0xee`,
    `mode:DIO, clock div:1`, the three `load:` lines and `entry 0x403cbf1a`.
  - Bootloader: `chip revision: v1.1`, `efuse block revision: v1.3`, SPI 80MHz, DIO, 8MB, all four
    partition-table rows, all six `esp_image` segment lines (same paddr, vaddr and sizes).
  - App start: `Unicore app`, `cpu freq: 160000000 Hz`, the application information, `Min chip rev:
    v0.3`, `Max chip rev: v1.99`, `Chip rev: v1.1`, all four heap regions, `spi_flash: detected chip:
    generic`, `flash io: dio`, both `sleep_gpio` lines, `main_task: Started on CPU0`,
    `Calling app_main()`, `main: Passport Keys`, `bsp_i2c` ready.
  - R8 adds: both `bsp_disp` lines (backlight LEDC on gpio 21; display ready 240x320),
    `LVGL: Starting LVGL task`, `bsp_lvgl` ready, and the four `BLE_INIT` lines (compile version
    `[1bb2f50]`, main XTAL, feature configuration, and the Bluetooth MAC, which IDF derives from the
    eFuse base MAC plus 2).
- `spi_flash: detected chip: generic` follows from the JEDEC ID 0x20 0x40 0x17 (XMC, 8 MB); ESP-EMU,
  which reports a GD part, does not match this line.
- heap_init regions:

| Region | Device, `pk`, ECO7 | `pk` ECO7 (R4) | `pk` rev3 (R1) | `official` ECO7 |
|---|---|---|---|---|
| RAM | `3FCA2AC0 len 0001D540` | identical | identical | `3FCABCC0 len 00014340` (larger .bss) |
| Retention RAM | `3FCC0000 len 0001C710` | identical | identical | identical |
| Retention RAM | `3FCDC710 len 0000294C` | identical | `len 00002950` | `len 0000294C` |
| RTCRAM | `50000020 len 00001FC8` | identical | identical | `50000044 len 00001FA4` |

- Timestamps (ESP_LOG milliseconds; emulated time is cycles / 160 MHz):

| Milestone | Device | Oracle (R4, R8) | Oracle minus device |
|---|---|---|---|
| bootloader first line | 24 | 3 | -21 |
| `segment 1` line (after mapping and hashing the 145 KB DROM) | 50 | 15 | -35 |
| `segment 4` line (after the 730 KB IROM) | 177 | 68 | -109 |
| `Loaded app` | 196 | 77 | -119 |
| `Calling app_main()` | 210 | 87 | -123 |
| `bsp_i2c` ready | 211 | 87 | -124 |
| `bsp_disp` display ready (R8) | 342 | 217 | -125 |
| `bsp_lvgl` ready (R8) | 400 | 226 | -174 |
| `BLE_INIT` compile version (R8) | 758 | 402 | -356 |
| duration `Loaded app` to `app_main` (CPU-bound) | 14 | 10 | -4 |

- Reading: in the oracle SPI flash commands, SHA and GP-SPI transfers take zero emulated cycles and
  the ROM stage has no flash-speed cost, so I/O-bound phases are much shorter than on silicon while
  CPU-bound phases are close. Line content and order are device-accurate; absolute timestamps are
  not. On the device 320 ms pass between the CW2017 profile line (438 ms) and `BLE_INIT`; what fills
  them is UNVERIFIED. In the oracle `bsp_battery_init` spends a 100 ms I2C timeout instead.
- Native and wasm builds of the oracle produce byte-identical console streams, timestamps included
  (`pk`, 0.3 s: 2,872 B on USB Serial/JTAG and 236 B on UART0).

**Confidence.** Line comparison: device capture (measured on the device) against oracle runs
(measured on an oracle). Device timestamps measured on the device; emulated timestamps measured on an
oracle. The cause of the 320 ms device gap is UNVERIFIED.

## g3-synth-efuse: Synthesized eFuse words

**Behavior.**

- The corpus images need an eFuse that says wafer major version 1 when they run on the ECO7 ROM. With
  wafer v0.4 the ECO7 ROM loads the bootloader, which takes the pre-ECO7 ROM path and faults (Load
  access fault at PC 0x4004c16c, MTVAL 0x5274; g3-stall-points). With the rev3 ROM and wafer v0.4
  both images boot to the same stall points.
- A synthetic eFuse with the words below and every other read word zero boots both images on the
  ECO7 ROM to the full depth of every G3 run (menu, hooks, snapshots). Read registers are at
  0x60008800 + offset:

| Offset | Word | Value | Fields (IDF `esp_efuse_table.csv`) |
|---|---|---|---|
| 0x44 | BLK1 word 0 | MAC bytes 5, 4, 3, 2 from bit 0 up; placeholder 02:00:00:c3:00:01 gives 0x00C30001 | MAC_FACTORY bits 0 to 31 |
| 0x48 | BLK1 word 1 | MAC bytes 1, 0 in bits 0 to 15; placeholder gives 0x00000200 | MAC_FACTORY bits 32 to 47 |
| 0x50 | BLK1 word 3 | 0x63040000 | WAFER_VERSION_MINOR_LO 1, PKG_VERSION 0, BLK_VERSION_MINOR 3, FLASH_CAP 4 (8M), FLASH_TEMP 1 (105C) |
| 0x58 | BLK1 word 5 | 0x01000000 | WAFER_VERSION_MINOR_HI 0, WAFER_VERSION_MAJOR 1 |
| 0x5c to 0x68 | BLK2 words 0 to 3 | synthetic unique ID (G3 used random bytes, generated once) | OPTIONAL_UNIQUE_ID |
| 0x6c | BLK2 word 4 | 0x00000001 | BLK_VERSION_MAJOR 1 ("With calibration"); TEMP_CALIB, OCODE and ADC1_INIT_CODE_ATTEN0 0 |

- The G1 image (84 words, every word not listed zero) uses the same words plus these, and boots both
  images on the ECO7 ROM through every G1 scenario (g3-g1-cost):

| Offset | Word | Value | Fields (IDF `esp_efuse_table.csv`) |
|---|---|---|---|
| 0x3c | BLK0 word 4 (RD_REPEAT_DATA3) | 0x80000000 | ERR_RST_ENABLE 1 (bit 159); no security feature set, USB-Serial/JTAG console left enabled |
| 0x54 | BLK1 word 4 | 0x00000001 | FLASH_VENDOR 1 (XMC); K_RTC_LDO, K_DIG_LDO, V_RTC_DBIAS20, V_DIG_DBIAS20 0 |
| 0x58 | BLK1 word 5 | 0x01000000 | as above; DIG_DBIAS_HVT 0, and the app's RTC init then uses its default digital bias 28 |
| 0x70 to 0x78 | BLK2 words 5 to 7 | 0 | ADC1_INIT_CODE_ATTEN1 to 3 and ADC1_CAL_VOL_ATTEN0 to 3 |

- Field values of the synthesized image in one place: chip revision v1.1 (WAFER_VERSION_MAJOR 1,
  WAFER_VERSION_MINOR 1), package 0, block revision v1.3 (BLK_VERSION_MAJOR 1, BLK_VERSION_MINOR 3),
  8 MB flash from XMC rated 105C, every calibration field 0 (BLK1 LDO and DBIAS trims, BLK2
  TEMP_CALIB, OCODE, the four ADC1 init codes and the four ADC1 calibration voltages), MAC
  `02:00:00:c3:00:01` (locally administered placeholder), OPTIONAL_UNIQUE_ID random bytes generated
  with the image and never printed, no security eFuse set. These are synthetic values, not a device
  identity.
- The same words in QEMU, as a 1 KiB eFuse file whose byte offset is the register offset minus 0x2c,
  print `Chip rev: v1.1`.

- Resulting console identity: bootloader `chip revision: v1.1` and `efuse block revision: v1.3`; app
  `Chip rev: v1.1`. The device's own BLK1 decodes to the same version fields (wafer v1.1, package 0,
  block minor 3).
- Read register layout (IDF `efuse_reg.h`): BLK0 from 0x2c (RD_WR_DIS at 0x2c, RD_REPEAT_DATA0 to 4
  at 0x30 to 0x40), BLK1 6 words from 0x44, BLK2 8 words from 0x5c, BLK3 8 words from 0x7c, BLK4 to
  BLK9 8 words each from 0x9c + 0x20 x n, BLK10 8 words from 0x15c: 84 words in total. EFUSE_CMD is
  at 0x1d4.
- In the oracle the read and program commands written to EFUSE_CMD complete at once, and the eFuse
  contents survive chip resets. The oracle's built-in identity (wafer v0.4, package 0,
  BLK_VERSION_MINOR 3, BLK_VERSION_MAJOR 1) boots only on the rev3 ROM.
- With the device's own calibration fields the firmware prints `adc_button: calibration scheme
  version is Curve Fitting` and `Calibration Success`. With calibration fields 0 the `official` menu
  still reports `Button=1`; which calibration lines it prints then was not reported (UNVERIFIED).

**Confidence.** Boot with these words: measured on an oracle (every G3 run). Field decoding and
offsets: checked against IDF v5.5.3 `esp_efuse_table.csv` and `efuse_reg.h` (ERR_RST_ENABLE bit 159,
FLASH_VENDOR bit 128, WAFER_VERSION_MAJOR bit 184). MAC word values: the G1 image holds 0x00C30001 and
0x00000200 for the placeholder, which agrees with the IDF MAC_FACTORY layout (measured on an oracle
for boot; the MAC read-back was not printed in the runs, UNVERIFIED). Console revision lines with
the G1 words: measured on an oracle (esp32sim and QEMU). The fallback to digital bias 28: stated by G1
from the RTC init path, not traced (estimated). Device version fields: measured on the device eFuse
dump.

## g3-rom-data-table: ROM data-table back-fill

**Behavior.**

- On its reset path the C3 mask ROM initialises the data of its own components in DRAM (0x3FCDE710 to
  0x3FCE0000) by copying bytes from a store inside ROM, following a table inside ROM. The ROM ELFs
  carry the table and the RAM-address data sections, but not the store bytes: the ELF `.text`
  section ends exactly where the store begins (`_rom_store`).
- Table: from `_rom_store_table` (equal to `_data_start`) up to `_data_end`, entries of 16 bytes,
  four little-endian words: destination start, destination end, store address, 0.

| ROM ELF | Table | Entries | `_rom_store` | Bytes listed |
|---|---|---|---|---|
| `esp32c3_rev3_rom.elf` (banner `esp32c3-api1-20210207`) | 0x40059200 to 0x40059400 | 32 | 0x40059590 | 0x524 |
| `esp32c3_rev101_rom.elf` (banner `esp32c3-eco7-20230720`) | 0x40059620 to 0x40059830 | 33 | 0x400599cc | 0x524 |

- Many entries are empty (start equals end), and store addresses are not in table order. Each
  non-empty entry names one ROM data section; for example the fifth ECO7 entry is the Bluetooth
  interface data, 0x3FCDF96C to 0x3FCDFA28, stored at 0x400599CC.
- To run the unmodified reset path from the ELF, the store must be filled before reset: for each
  entry, the bytes the ELF holds for [destination start, destination end) are placed at the store
  address. esp32sim does this and boots both the rev3 and the ECO7 ROM, `pk` BLE controller lines
  included. Without it the ROM would copy an empty store over its data (estimated consequence; not
  run).
- A separate 4-byte region `_data_start_btdm_rom` to `_data_end_btdm_rom` sits just before the table
  (0x400591fc rev3, 0x4005961c ECO7). In the ECO7 store a 0x5c-byte gap between 0x40059AA8 and
  0x40059B04 matches the size of the ROM `.data_btdm` section; whether it must be filled too is
  UNVERIFIED (the oracle fills the table entries only and still prints the device's four `BLE_INIT`
  lines).
- The ECO7 ROM ELF has 325 symbols the rev3 ELF lacks and lacks 133 of rev3's.

**Confidence.** Table layout, addresses and entry counts: checked on the bundled ROM ELFs with
objdump. Boot with the back-fill: measured on an oracle. The effect of a missing back-fill and the
role of the 0x5c-byte gap: estimated, UNVERIFIED.

## g3-gdma-c3-layout: C3 GDMA register layout and the SPI2 DMA stall

**Behavior.**

- The C3 GDMA block is at 0x6003F000 with three channels (interrupt sources 44 to 46). Its layout
  (IDF v5.5.3 `esp32c3/register/soc/gdma_reg.h`):

| Register | Channel 0 offset | Per channel |
|---|---|---|
| INT_RAW, INT_ST, INT_ENA, INT_CLR (one combined in and out set per channel) | 0x00, 0x04, 0x08, 0x0C | +0x10 |
| MISC_CONF | 0x44 | |
| DATE | 0x48 | |
| IN_CONF0 | 0x70 | +0xC0 |
| IN_LINK | 0x80 | +0xC0 |
| IN_PERI_SEL | 0xA0 | +0xC0 |
| OUT_CONF0 | 0xD0 | +0xC0 |
| OUT_LINK | 0xE0 | +0xC0 |
| OUT_PERI_SEL | 0x100 | +0xC0 |

- Bits of the combined interrupt set include IN_DONE 0, IN_SUC_EOF 1, OUT_DONE 3, OUT_EOF 4,
  OUT_TOTAL_EOF 8 and OUTFIFO_UDF 12.
- The S3 layout is different: IN_CONF0 at 0x00, separate in and out interrupt sets per channel
  (IN_INT_RAW 0x08, OUT_INT_RAW 0x68), OUT_CONF0 0x60, OUT_LINK 0x80, OUT_PERI_SEL 0xA8, MISC_CONF
  0x3C8. The stock esp32sim C3 decodes the C3 block with the S3 layout. Observable result: GP-SPI2
  asks for DMA data, no out channel is ever running, and the first panel command waits forever in
  `spi_device_polling_end` (g3-stall-points row 2). Any other DMA user on that model would stall the
  same way (UNVERIFIED; I2S and the crypto DMA modes stop earlier for other reasons).
- With the C3 layout decoded instead of the S3 layout, and the same DMA behavior behind it, `pk`
  passes display and LVGL init with console lines identical to the device
  (g3-console-fidelity), `official` draws its menu (g3-menu-geometry), and the IDF `hello_world`
  3 s golden is unchanged (console byte-identical, 480,000,000 instructions).
- SPI2 DMA transfer behavior as observed in the working runs:
  - When SPI2 has a DMA transmit pending and an out channel with OUT_PERI_SEL 0 (SPI2 in
    IDF `esp32c3/include/soc/gdma_channel.h`) is running, the descriptor
    chain in SRAM is walked and up to the data-phase length is taken from it. The channel then shows
    OUT_DONE, OUT_EOF and OUT_TOTAL_EOF, and SPI2 completes the transfer and raises TRANS_DONE
    (source 19).
  - Pending GPIO edges are delivered to the board before the SPI bytes, so the panel samples D/C
    (GPIO20) at its current level for those bytes.
  - Transfers take zero emulated time (g3-console-fidelity timestamps).
  - GPIO edge events queued for the board must be consumed. In the stock oracle the C3 queue was
    never drained and grew with every edge; draining it made a 3 s `pk` snapshot 461 B smaller.

**Confidence.** Layouts: checked against IDF v5.5.3 `gdma_reg.h` for the C3 and the S3. Stall, fix
and transfer ordering: measured on an oracle (panel pixels in g3-menu-geometry). Stalls of other DMA
users: UNVERIFIED.

## g3-adc-read-path: ADC oneshot read path and its cost

**Behavior.**

- The `official` button driver (`button: IoT Button Version: 4.2.0`, ADC1 channel 0 on GPIO0) reads
  the ADC from the esp_timer task, about every 5 ms (200 reads per second): a 3 s menu run made 559
  conversions, 561 with two DOWN presses.
- Read path: `adc_oneshot_read` to `adc_oneshot_hal_convert`, which polls `adc_oneshot_ll_get_event`
  (IDF `adc_ll.h`) until the ADC1 done bit is set. Without an APB_SARADC model the bit never sets,
  the esp_timer task spins and starves `main` (g3-stall-points row 5). Before the first read the
  driver writes calibration through the analog I2C registers (RTC_I2C, 4 distinct registers) and
  touches 7 distinct APB_SARADC registers in the first 0.5 s.
- Minimal completion behavior that runs the menu and answers presses (APB_SARADC at 0x60040000,
  offsets from IDF `apb_saradc_reg.h`):
  - a rising edge of ONETIME_START (ONETIME_SAMPLE 0x20, bit 29) while SARADC1_ONETIME_SAMPLE is
    set (same register, bit 31) sets the ADC1 data field of 1_DATA_STATUS (0x2C) to the raw
    code and ADC1_DONE in INT_RAW (0x44, bit 31) at once;
  - INT_ENA is 0x40 and INT_CLR 0x4C (bit 31 clears ADC1_DONE);
  - the conversion takes zero emulated time.
- Raw codes used: 4095 released, 433 for DOWN (300 mV). The two DOWN presses
  (1.00 to 1.30 s and 2.00 to 2.30 s) change the selection at 1.492 s and 2.492 s (g3-debug-stops).
- Host cost in the oracle: every register write ends the current execution batch and re-derives all
  62 interrupt sources, so a driver that writes registers every few milliseconds (this read path,
  the SPI2 LCD flush, I2S) costs more than its instruction count suggests (size UNVERIFIED).
  Measured whole-run figure: the 3 s `official` menu with this polling took 1.044 to 1.052 s of
  run-loop wall time natively (g3-wasm-idle-host-cost).
- G1 measurement of the same path (`official` menu idle, ECO7 ROM, synthesized eFuse of
  g3-synth-efuse, 10 s):
  - 200.3 `adc_oneshot_read` calls per emulated second.
  - Exactly 2,267 retired instructions from `adc_oneshot_read` entry to the following
    `adc_cali_raw_to_voltage` entry, in 700 of 700 samples. The path is deterministic.
  - 0.454 MIPS in total, 16 % of the 2.75 MIPS menu-idle demand (g3-g1-cost).
  - Largest contributors, instructions per emulated second: `ets_delay_us` 98,600,
    `rom1_chip_i2c_writeReg` 28,800, `rom1_chip_i2c_readReg` 27,600, `rom_i2c_writeReg_Mask` 26,400,
    `regi2c_ctrl_write_reg_mask` 23,400, `rom1_get_i2c_hostid` 22,800, `adc_oneshot_hal_setup` 18,800,
    `adc_oneshot_hal_convert` 16,000, `adc_oneshot_read` 11,800, `adc_hal_arbiter_config` 11,000. The
    ROM microsecond delay and the analog I2C calibration writes dominate, not the conversion poll.
  - Cross-check: QEMU with the ADC read shimmed out (every read returns 4095 at once) runs these
    functions zero times and measures 2.845 MIPS in the same window; esp32sim minus the ADC path
    (2,257 instructions x 200 per second, the value the cross-check subtracted) gives 2.298 MIPS. The
    remaining difference sits in the timer and interrupt entry path of the two oracles.
  - Consequence: the emulator's ROM delay fast-forward removes most of the host cost of this path;
    its instruction count stays as is.
- G1 completion behavior (a superset of the G3 one above): a rising edge of ONETIME_START with bit 31
  (ADC1) set latches the raw code of the selected channel (bits 28 to 25 of ONETIME_SAMPLE) into
  1_DATA_STATUS and sets INT_RAW bit 31; with bit 30 (ADC2) set it writes 0 to 2_DATA_STATUS (0x30)
  and sets INT_RAW bit 30. INT_ST (0x48) reads INT_RAW AND INT_ENA; INT_CLR clears.
- Raw codes that reached the intended keys in every G1 scenario: released 4095, DOWN 433, OK 862,
  UP 107. The button driver converts them to millivolts with the curve-fitting scheme built from
  calibration fields that are all 0 and compares with the firmware's key voltage ranges 0 to 150,
  150 to 447 and 447 to 1900 mV. The millivolt results were not printed (UNVERIFIED); the scripted
  cards were reached, so each code maps to its key.

**Confidence.** Read path, polling and the completion behavior: measured on an oracle (profiles,
conversion counts, key presses reaching `s_sel`). G1 instruction counts: measured on an oracle
(function-entry traces and raw-count profiles, deterministic), cross-checked against QEMU run as a
black box. The 5 ms period is estimated from the conversion counts. The raw-code-to-millivolt values
are UNVERIFIED. Offsets checked against IDF v5.5.3 `apb_saradc_reg.h`.

## g3-narrow-access-widening: Narrow-access widening hazard

**Behavior.**

- Observed in esp32sim's C3 model: an 8-bit or 16-bit access to a peripheral register is carried out
  as a read of the whole 32-bit register, a merge of the accessed bytes, and a write of the whole
  register. A byte write to a register with read side effects therefore triggers them. The same
  project's S3 model has since switched to faulting narrow peripheral accesses; the C3 model has not.
  Widening is both a correctness and a speed hazard (every narrow write costs an
  extra full read).
- What a widened write does to the bits outside the accessed bytes, by TRM access type:

| Access type of a bit in the untouched bytes | Effect of the widened read and write-back |
|---|---|
| W1C (interrupt clear, sticky status) | a bit that reads 1 is written back as 1 and is cleared |
| W1S, W1TS, W1TC | a bit that reads 1 is written back as 1 and sets, or toggles, again |
| SC or WT command bit (self-clearing, for example a pending update or command start) | a bit still reading 1 is written back and restarts the command |
| WO field (reads as 0 or as unrelated state) | the read value, not the firmware's intent, is written into the field |
| RC, or a register whose read pops a FIFO or clears status | the extra read consumes data or status the firmware never read |
| RO | no effect on the register, but the extra read still happens |

- A widened read of a register with read side effects triggers them for bytes the firmware did not
  read.
- None of these hazards was provoked in a G3 run, and which registers the corpus accesses
  narrowly was not measured (UNVERIFIED). The design answer is to pass size and
  byte offset through and apply field semantics to exactly the bytes accessed.

**Confidence.** The widening behavior of the oracle: from the analysis, not observed in a run
(estimated). The hazard table: estimated from the TRM access-type definitions; UNVERIFIED on the
device.

## g3-oracle-speed: Oracle interpreter speed

**Behavior.**

- Method: busy stretches only (no WFI; the oracle's own "Minsn/s" counts idle-skipped cycles and is
  not used). Native: run-loop time only. wasm: Node v26.7.0 driving the C ABI in 2 M-cycle slices,
  and headless Chrome 153 on the main thread. Host: Apple M3 Pro, nothing else running.

| Stretch | Native, Minsn/s | wasm, Minsn/s |
|---|---|---|
| `pk` reset to `app_main` (7.0 M instructions: ROM, bootloader, app start) | 71.0 to 72.9 (10 runs) | Node 48.7 to 53.9 (4 runs) |
| `official` reset to `app_main` (9.9 M) | 71.1 to 72.5 (8 runs) | Node 53.3 |
| `pk`, 400 M instructions (boot plus 393 M in the SPI2 CMD.update poll) | 75.6 to 77.0 (6 runs, 5.2 to 5.3 s) | Node 51.3 to 53.8 (4 runs) |
| as above, ECO7 ROM and v1.1 eFuse | 75.5; peak footprint 21.7 MB, max RSS 22.8 MB | |
| `pk` 2.5 s emulated | | Chrome: reset to `app_main` 51.3 to 51.7; busy poll 52.1 to 54.8; whole run 52.8 and 54.7 |
| `pk` 0.3 s determinism run | 73.9 | Node 51.9 |

- Ratios: native 0.44 to 0.48 of a fully busy 160 MHz C3; wasm 0.30 to 0.34; wasm reaches 0.70 to
  0.74 of native. A busy C3 needs 160 Minsn/s to keep real time at one instruction per cycle.
- The G3 debug-control build measured 72.7 (`pk` 7 M) and 76.9 (`pk` 400 M) Minsn/s for its baseline;
  all debug stops armed cost at most about 5 % (g3-debug-stops).
- The oracle's RISC-V core is a plain interpreter: every executed instruction is fetched and decoded
  again (no decode cache, block cache or JIT on RISC-V). Its documentation claims 200 to 300 Minsn/s;
  busy stretches do not reproduce that. Its counter includes idle-skipped cycles, so idle-heavy runs
  print inflated figures (IDF `hello_world`, 26 s: 757).
- Idle skipping carries the idle menu: 443 to 970 million counted instructions per second in the G3
  menu runs (g3-wasm-idle-host-cost). LVGL animation plus audio is not expected to fit (estimated).
- ESP-EMU v0.42.0 on the same host (UNVERIFIED: an earlier run, not repeated): about 69 Minsn/s
  native CPU-bound, 42 to 50 as wasm in Node 26. esp32sim is about 5 to 10 % faster natively and
  roughly 0 to 30 % faster as wasm (the stretches differ, so the wasm comparison is indicative).
- wasm module: 7,340,382 B without post-optimisation, 12 to 18 ms to instantiate and load an image,
  27.5 MB linear memory, no SharedArrayBuffer.
- G1 repeated these stretches with a fresh build (Safari 27 included) and fitted busy speeds from a
  firmware scenario: 75.8 to 81.4 Minsn/s native on busy code, a fitted busy speed S of 82.6 native,
  59.8 to 65.4 in wasm (60 to 65 in dedicated workers), 85 with a block cache in wasm workers and 129.5
  natively. These are the supply references of the performance targets; the tables, the fit and their
  limits are in g3-g1-cost.
- The G1 figures for the G3 stretches agree with the table above within run-to-run spread (native
  `pk` 400 M: 75.9 to 77.0 with the stock build; with the stock module Chrome 52.5 to 54.1 on the
  main thread and in a worker, Node median 49.8 after a first warm-up run of 20.6).

**Confidence.** Measured on an oracle (esp32sim, native, Node and Chrome on one host; Safari in G1).
The ESP-EMU comparison is across different stretches (indicative). The LVGL and audio expectation is
estimated.

## g3-g1-cost: G1 cost figures

**Behavior.**

*Units and counting.*

- MIPS: millions of guest instructions retired per emulated second (demand). Minsn/s: millions of
  guest instructions per host second (supply). s/s: host seconds per emulated second (below 1 is
  faster than real time). A fully busy C3 at 160 MHz is 160 MIPS (one instruction per cycle).
- *Retired* excludes idle-skipped cycles, WFI steps and trap entries. *Steps* (Msteps/s) include
  idle-skipped cycles.
- Demand is sampled in windows of 10 ms emulated time (1.6 M cycles). A window closes at the first
  scheduler boundary after it ends, at most 64 cycles late, and the next window absorbs the rest:
  totals are exact and single windows are within 0.004 % of 1.6 M cycles. 100 ms figures aggregate
  these windows; percentiles are nearest-rank.

*Setup of the demand runs.*

- Oracle esp32sim run as a black box with the ECO7 mask ROM, the G1 synthesized eFuse
  (g3-synth-efuse), boot from the ROM, 8 MB flash image, placeholder MAC `02:00:00:c3:00:01`, reset
  cause 0x15, strap 0xa, USB-Serial/JTAG console, ELF symbols of the image and its bootloader.
- Stubs: `bsp_i2c_scan` returns 0, `bsp_battery_init` returns 0x105. UI code is not stubbed. `pk`
  also stubs `esp_bt_controller_init` = 0x103 (console `BLE_INIT: controller init failed`). Variants
  O1c and O2c also stub `bsp_audio_init` = 0x103.
- Completions added for these runs: the APB_SARADC behavior of g3-adc-read-path (raw codes: released
  4095, DOWN 433, OK 862, UP 107) and an I2S block whose RX_CONF (0x20) and TX_CONF (0x24) bit 8
  (`rx_update`, `tx_update`) reads 0, so the update bits self-clear.
- A click holds its code for 100 ms. `Calling app_main` appears at 62.3 ms emulated. Menu ready is the
  first console line containing `Battery=` (`main: 就绪:Display=1 Button=1 Audio=0 Battery=0`), at
  1.555 s as specified and at 282.2 ms with `bsp_audio_init` stubbed. `pk_app: ready` appears at
  260.9 ms.

| Id | Script (emulated time) | Verified by |
|---|---|---|
| O1, O2 | none; boot, then menu idle 3 to 13 s | `s_active` = -1 at the end |
| O1c, O2c | as O1, O2 with `bsp_audio_init` stubbed | same |
| O3 | DOWN at 3.0 + 0.4 k s, k = 0 to 19 | `s_sel` = 6 at the end (20 mod 7 cards) |
| O4, O4b | OK at 3.0 s (Display demo); O4b adds OK at 4 to 12 s | `s_active` = 0 |
| O5, O5b | DOWN 3.0, OK 3.4 (Button demo); O5b adds DOWN at 5 to 13 s | `s_active` = 1 |
| O6 | UP 3.0, OK 3.4 (Low Power card) | `s_active` = 6 |
| O6b | O6 plus OK at 5.0 s (LIGHT SLEEP) | excluded, emulator artifact |
| P1 | `pk`, idle from ready + 1 s to ready + 11 s | console `pk_app: ready`, `link state -1 -> 0` |

*Demand* (retired MIPS; idle cycles = share of cycles skipped idle):

| Id | Phase | Seconds | Mean | p95 10 ms | Max 10 ms | p50 100 ms | p95 100 ms | Max 100 ms | Idle cycles | IRQ/s |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| O1 | reset to menu, as specified | 1.55 | 136.69 | 160.00 | 160.00 | 160.00 | 160.00 | 160.00 | 14.6 % | 1706 |
| O1c | reset to menu, audio init stubbed | 0.28 | 86.75 | 160.00 | 160.00 | 14.39 | 100.51 | 100.51 | 45.8 % | 1271 |
| O1x | `app_main` to menu, audio stubbed | 0.21 | 67.94 | 160.00 | 160.00 | 14.39 | 100.51 | 100.51 | 57.5 % | 1638 |
| O2 | menu idle | 10.00 | 2.79 | 2.99 | 23.36 | 2.57 | 4.59 | 6.39 | 98.2 % | 1878 |
| O2c | menu idle, audio stubbed | 10.00 | 2.75 | 2.77 | 23.36 | 2.57 | 4.38 | 6.38 | 98.3 % | 1877 |
| O3 | 20 DOWN clicks | 8.00 | 9.61 | 56.12 | 126.27 | 8.18 | 14.97 | 16.96 | 94.0 % | 1930 |
| O4 | Display demo | 10.00 | 3.67 | 2.77 | 160.00 | 2.57 | 4.52 | 81.41 | 97.7 % | 1879 |
| O4t | Display demo entry (3.0 to 3.5 s) | 0.50 | 21.14 | 160.00 | 160.00 | 4.27 | 81.41 | 81.41 | 86.8 % | 1922 |
| O4b | Display demo, OK every 1 s | 10.00 | 5.84 | 22.48 | 160.00 | 2.57 | 14.92 | 81.41 | 96.3 % | 1899 |
| O5 | Button demo | 10.00 | 5.80 | 23.31 | 160.00 | 4.64 | 6.61 | 83.03 | 96.3 % | 1916 |
| O5b | Button demo, DOWN every 1 s | 10.00 | 7.29 | 23.56 | 160.00 | 4.66 | 14.36 | 83.03 | 95.4 % | 1946 |
| O6 | Low Power card | 10.00 | 3.98 | 2.99 | 160.00 | 2.57 | 6.24 | 98.18 | 97.5 % | 1881 |
| P1 | `pk` idle after ready | 10.00 | 2.76 | 2.95 | 23.04 | 2.57 | 4.32 | 6.36 | 98.2 % | 1881 |
| O6b | LIGHT SLEEP (emulator gap) | 10.00 | 129.15 | 160.00 | 160.00 | 160.00 | 160.00 | 160.00 | 19.3 % | 387 |

- Instruction counts are deterministic: a second build with a block cache reproduced every 10 ms row
  of O3, O4b, O5b, O6 and P1 exactly, and the USB console byte for byte.
- Steady state is set by the 1 kHz FreeRTOS tick, the button driver polling every 5 ms and the LVGL
  timer (33 ms). 10 ms windows are 2.46 to 2.77 MIPS; a 23 MIPS 10 ms spike repeats (periodic UI
  refresh) and raises the 100 ms p95 to 4.3 to 4.6 MIPS.
- One DOWN click redraws the selection for 10 to 20 ms at up to 126 MIPS; with 400 ms spacing no
  100 ms window exceeds 17 MIPS.
- Entering a card is the real peak: the first 100 ms run 81 to 98 MIPS (Display 81.41, Button 83.03,
  Low Power 98.18), with 10 ms windows fully busy at 160 MIPS for 1 to 3 windows.
- Interrupts: about 1,880 per second in steady state. The FreeRTOS tick (SYSTIMER comparator 0,
  source 37) is 1,000 per second; esp_timer (SYSTIMER comparator 2, source 39) is about 390 per
  second; the FROM_CPU yields (sources 50 to 53) are the remaining 490 or so. An earlier budget
  groups the non-tick share as "the FROM_CPU yields (sources 39 and 50)", which mixes two distinct
  sources; this note resolves the grouping in favour of the map-register source order stated in
  g3-irq-latency, whose comparable `official` idle run takes 9,938 tick and 3,920 esp_timer
  interrupts in 10 emulated seconds.
- Largest idle contributors, instructions per emulated second (O2c): `_interrupt_handler` 193 k,
  `__udivdi3` 147 k, `vPortExitCritical` 129 k, `esp_vApplicationIdleHook` 129 k, `ets_delay_us`
  99 k, `vPortEnterCritical` 98 k, `esp_vApplicationTickHook` 87 k, `systimer_hal_get_counter_value`
  78 k. The ADC button read path is 0.454 MIPS of it (16 %; g3-adc-read-path).

*Emulator artifacts, not firmware demand.*

- O1 as specified: from 0.30 to 1.55 s the core is 100 % busy in `s_i2c_master_clear_bus`, polling
  `xTaskGetTickCount`, while opening the ES8311 codec fails through repeated 50 ms I2C timeouts
  (I2C0 not modelled, console `Audio=0`). On silicon the codec answers. O1c is the better boot
  estimate: 86.75 MIPS over 0.28 s.
- O6b: `esp_light_sleep_start` is not supported by the oracle and spins 100 % busy from 5.5 s. Real
  light sleep would be near 0 MIPS. Excluded from the budget.
- Boot is shorter than on silicon because flash, SHA and SPI take zero cycles in the oracle.

*Not measured (UNVERIFIED).*

- Audio demo: a 1 kHz square wave at 16 kHz in 512-sample chunks is about 10 instructions per sample
  plus the codec write, I2S DMA copies and one DMA interrupt per frame; estimated below 1 MIPS, but it
  needs host-paced sample timing. Not runnable (ES8311 and I2C0 not modelled).
- BLE advertising and Wi-Fi scanning: the controller was stubbed, so their demand is unknown. The ROM
  and closed controller code runs in the guest on silicon; it is the main unknown of the budget.

*QEMU cross-check* (QEMU run as a black box with icount 1 instruction per cycle, the ECO7 ROM, the
same eFuse words, the `official` image; SPI transmit, LCD flush, `bsp_i2c_scan`, `bsp_battery_init`
= 0x105 and `bsp_audio_init` = 0x103 shimmed, every ADC read returns 4095; counted per 10 FreeRTOS
ticks; 90,071 ticks, 285.4 M instructions; menu ready at log time 284 ms):

| Window 3 to 13 s | Mean | p50 10 ms | p95 10 ms | Max 10 ms | p50 100 ms | p95 100 ms | Max 100 ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| QEMU, instructions per 10 ticks | 2.845 | 2.582 | 3.014 | 20.59 | 2.689 | 4.226 | 5.979 |
| esp32sim O2c, retired | 2.750 | 2.460 | 2.767 | 23.36 | 2.569 | 4.382 | 6.381 |
| esp32sim O2c minus the ADC read path (2,257 x 200 per s) | 2.298 | 2.008 | 2.316 | 22.91 | 2.118 | 3.931 | 5.930 |

- QEMU is steady over 90 s (5 s blocks 2.807 to 2.884 MIPS).
- Firmware logic has the same per-second counts in both (QEMU / esp32sim): `__udivdi3` 146,810 /
  146,800, `esp_vApplicationTickHook` 87,000 / 87,000, `get_reading_error` 50,600 / 50,600,
  `button_adc_get_key_level` 45,405 / 45,400, `SysTickIsrHandler` 40,003 / 40,000,
  `xTaskIncrementTick` 39,833 / 39,785, `prvAddCurrentTaskToDelayedList` 26,584 / 26,543,
  `get_prop_core` 25,269 / 25,269, `tick_hook` 24,000 / 24,000.
- Higher in QEMU (timer and interrupt entry path): `systimer_hal_get_counter_value` +109,030,
  `rtos_int_enter` +106,672, `restore_stack_pointer` +98,871, `systimer_hal_set_alarm_target`
  +87,425, `wdt_hal_config_stage` +60,000, `_interrupt_handler` +19,385, `context_switch_requested`
  +17,722, `usb_serial_jtag_sof_tick_hook` +10,000. Likely more interrupt entries and SYSTIMER
  counter and alarm retries in QEMU's timer model (UNVERIFIED).
- Conclusion of G1: the demand figures are the firmware's own work within about 0.5 MIPS in steady
  state; the differences come from peripheral models. No correction is applied.

*Supply, native* (sequential runs, nothing else running). PN1 is `pk` reset to 7.0 M cycles and PN3
400 M cycles, rev3 ROM, the oracle's built-in eFuse, no console. The stock build stalls in the SPI2
poll (g3-stall-points); the G1 build passes SPI2 and idles 9.1 % of PN3 cycles (363.8 M retired), so
stock and G1 columns run different guest paths.

| Stretch | Stock build, Minsn/s | G1 build, Msteps/s |
|---|---|---|
| PN1 | 60.4, 72.3, 73.9 (0.1 s runs, coarse) | 70.1, 73.9, 73.9 |
| PN3 | 75.9, 76.9, 77.0 (5.2 to 5.3 s) | 75.8, 76.8, 76.8 |

- Fully busy 10 ms windows of O3 ran at 81.4 Minsn/s natively.
- Native profile, busy (`pk`, 1.2 G cycles, 68.9 Minsn/s): instruction execution dominates; decode
  plus fetch are about 22 %; a per-access lookup for unmodelled peripheral blocks is a visible cost.
  Idle: see g3-wasm-idle-host-cost.

*Supply, wasm* (G1 harness: the same C ABI as the G3 wasm runs, run calls of 2 M cycles, only time
inside the run call counted; main thread yielding every 250 ms, or a dedicated module worker; Node
v26.7.0, Chrome 153 headless, Safari 27.0 in a foreground window). PN, guest steps per host second,
median [min to max] of 3 runs; G1 retired rate is 0.909 x the PN3 figure:

| Host | Thread | Stock PN1 | Stock PN3 | G1 PN1 | G1 PN3 | G1+BC PN1 | G1+BC PN3 |
|---|---|---|---|---|---|---|---|
| native | n/a | 72.3 [60.4 to 73.9] | 76.9 [75.9 to 77.0] | 73.9 [70.1 to 73.9] | 76.8 [75.8 to 76.8] | 97.8 [96.9 to 98.2] | 106.5 [105.6 to 106.6] |
| Node 26.7 | node | 47.4 [13.9 to 50.1] | 49.8 [20.6 to 51.8] | 51.0 [49.3 to 55.5] | 44.2 [43.3 to 45.6] | 63.7 [56.6 to 64.0] | 50.4 [50.3 to 57.9] |
| Chrome 153 | main | 52.3 [46.4 to 53.2] | 52.7 [52.5 to 53.0] | 51.6 [47.9 to 53.3] | 46.9 [46.2 to 47.4] | 62.8 [61.7 to 64.2] | 57.3 [54.4 to 57.5] |
| Chrome 153 | worker | 54.8 [49.9 to 55.4] | 54.1 [52.6 to 54.1] | 51.8 [51.4 to 53.4] | 47.2 [46.5 to 49.0] | 70.1 [68.4 to 70.1] | 55.7 [54.0 to 56.2] |
| Safari 27 | main | 54.3 [51.5 to 56.5] | 57.1 [56.0 to 58.3] | 56.0 [55.6 to 56.5] | 50.6 [35.1 to 50.8] | 63.6 [63.6 to 63.6] | 55.7 [55.4 to 60.2] |
| Safari 27 | worker | 56.5 [51.9 to 56.9] | 56.8 [54.2 to 58.6] | 53.0 [50.0 to 54.3] | 50.7 [50.5 to 51.2] | 64.2 [58.3 to 64.2] | 57.4 [55.4 to 58.9] |

- Node's first stock run (13.9, 20.6) and Safari's first G1 main-thread PN3 run (35.1) are warm-up
  outliers inside the ranges.

O3 on `official` with the rev3 ROM (demand identical to the ECO7 runs within 0.1 %), host s per
emulated s per phase, median [min to max] of 2 runs (native G1+BC 1 run). Worst: largest host time of
one 100 ms emulated window. Late: 100 ms windows above 100 ms host time, of 145.

| Host | Thread | Module | Boot 0 to 1.555 s | Idle 2 to 3 s | Clicks 3 to 11 s | Idle 12 to 14.5 s | 14.5 s run, host s | Worst 100 ms, ms | Late |
|---|---|---|---|---|---|---|---|---|---|
| native | n/a | G1 | 1.680 [1.668 to 1.693] | 0.201 [0.200 to 0.203] | 0.293 [0.291 to 0.296] | 0.207 [0.201 to 0.213] | 5.99 [5.97 to 6.01] | 199 [198 to 199] | 14 |
| native | n/a | G1+BC | 1.082 | 0.192 | 0.245 | 0.197 | 4.62 | 127 | 12 |
| Node 26.7 | node | G1 | 2.325 [2.325 to 2.325] | 0.317 [0.316 to 0.318] | 0.428 [0.427 to 0.428] | 0.316 [0.316 to 0.317] | 8.63 [8.62 to 8.63] | 270 [269 to 270] | 14 |
| Node 26.7 | node | G1+BC | 1.663 [1.653 to 1.673] | 0.307 [0.307 to 0.308] | 0.382 [0.381 to 0.384] | 0.305 [0.304 to 0.305] | 7.18 [7.16 to 7.20] | 200 [188 to 211] | 14 |
| Chrome 153 | main | G1 | 2.326 [2.301 to 2.352] | 0.312 [0.307 to 0.316] | 0.422 [0.418 to 0.426] | 0.311 [0.308 to 0.314] | 8.56 [8.47 to 8.65] | 269 [267 to 272] | 14 |
| Chrome 153 | main | G1+BC | 1.725 [1.620 to 1.829] | 0.315 [0.296 to 0.334] | 0.379 [0.372 to 0.387] | 0.299 [0.298 to 0.301] | 7.24 [6.99 to 7.48] | 201 [187 to 214] | 14 |
| Chrome 153 | worker | G1 | 2.311 [2.295 to 2.328] | 0.307 [0.306 to 0.307] | 0.423 [0.418 to 0.428] | 0.320 [0.307 to 0.333] | 8.56 [8.50 to 8.61] | 267 [266 to 268] | 14 |
| Chrome 153 | worker | G1+BC | 1.640 [1.620 to 1.659] | 0.305 [0.304 to 0.305] | 0.380 [0.370 to 0.390] | 0.299 [0.295 to 0.303] | 7.09 [6.97 to 7.22] | 189 [186 to 192] | 14 |
| Safari 27 | main | G1 | 2.127 [2.115 to 2.139] | 0.294 [0.291 to 0.297] | 0.404 [0.402 to 0.407] | 0.294 [0.291 to 0.298] | 8.03 [7.97 to 8.08] | 248 [246 to 249] | 14 |
| Safari 27 | main | G1+BC | 1.639 [1.637 to 1.641] | 0.287 [0.287 to 0.287] | 0.360 [0.360 to 0.360] | 0.286 [0.286 to 0.287] | 6.86 [6.86 to 6.86] | 186 [186 to 187] | 14 |
| Safari 27 | worker | G1 | 2.165 [2.161 to 2.170] | 0.294 [0.292 to 0.297] | 0.402 [0.402 to 0.403] | 0.307 [0.295 to 0.318] | 8.09 [8.06 to 8.13] | 250 [249 to 251] | 14 |
| Safari 27 | worker | G1+BC | 1.652 [1.648 to 1.656] | 0.288 [0.287 to 0.289] | 0.361 [0.360 to 0.362] | 0.286 [0.286 to 0.286] | 6.89 [6.88 to 6.89] | 188 [187 to 188] | 14 |

- Every late window lies in boot (window ends 0.1 to 1.6 s). After boot the slowest 100 ms window of
  O3 took 51 to 55 host ms with G1 and 43 to 51 ms with the block cache in every wasm host (native 39
  to 45 ms and 30 ms).
- Main thread and worker are within 5 % on every phase. Safari 27 is about 5 % faster than Chrome 153
  on the phases (clicks 0.402 vs 0.423 with G1, 0.361 vs 0.380 with the block cache) and 5 to 8 % on
  stock PN3; Node matches Chrome.
- wasm needs 1.3 to 1.4x the native host time for the O3 run with G1 (8.03 to 8.63 s vs 5.99 s) and
  1.5 to 1.6x with the block cache (6.86 to 7.24 s vs 4.62 s).

*Cost model.* Host s per emulated s = R / S + I x c, with R the phase's retired MIPS, I its idle
cycle share, S the host's busy speed (Minsn/s) and c the host cost of one fully idle emulated second.
S and c are fitted per host from the boot and final idle phases of O3; the clicks phase, not used by
the fit, checks it. A 100 ms window with peak demand R_max keeps real time when
R_max x 0.1 / S + (1 - R_max / 160) x 0.1 x c <= 0.1.

| Host configuration | Runs | S, busy Minsn/s | c, host s per idle emulated s | Clicks measured | Clicks model |
|---|---|---|---|---|---|
| Chrome 153 main G1 | 2 | 59.8 | 0.271 | 0.422 | 0.415 |
| Chrome 153 main G1+BC | 2 | 81.4 | 0.271 | 0.379 | 0.373 |
| Chrome 153 worker G1 | 2 | 60.2 | 0.280 | 0.423 | 0.422 |
| Chrome 153 worker G1+BC | 2 | 85.4 | 0.272 | 0.380 | 0.368 |
| native G1 | 2 | 82.6 | 0.177 | 0.293 | 0.283 |
| native G1+BC | 1 | 129.5 | 0.179 | 0.245 | 0.243 |
| Node 26.7 G1 | 2 | 59.8 | 0.276 | 0.428 | 0.420 |
| Node 26.7 G1+BC | 2 | 84.2 | 0.277 | 0.382 | 0.375 |
| Safari 27 main G1 | 2 | 65.4 | 0.257 | 0.404 | 0.389 |
| Safari 27 main G1+BC | 2 | 85.3 | 0.259 | 0.360 | 0.356 |
| Safari 27 worker G1 | 2 | 64.3 | 0.269 | 0.402 | 0.402 |
| Safari 27 worker G1+BC | 2 | 84.7 | 0.259 | 0.361 | 0.356 |

- The model reproduces the clicks phase within 4 % on every host (largest deviation Safari main G1,
  0.404 vs 0.389). S is the speed on the boot phase, which is dominated by the I2C bus-clear busy
  wait; the `pk` PN3 path is slower, and code with more MMIO accesses per instruction would be slower
  too. c is 0.26 to 0.28 in wasm and 0.18 natively, with or without the block cache.

*Required vs achieved* (model predictions from the fitted S and c; only O3 was run in wasm). Cells:
host s per emulated s over the phase / host ms of its worst 100 ms window; a star marks a phase above
1.0 or a window above 100 ms.

| Scenario | Mean / p95 100 ms / max 100 ms MIPS | Chrome wk G1 | Chrome wk G1+BC | Safari wk G1 | Safari wk G1+BC | native G1 | native G1+BC |
|---|---|---|---|---|---|---|---|
| O1 boot to menu, as specified | 136.69 / 160.00 / 160.00 | 2.31* / 266* | 1.64* / 187* | 2.17* / 249* | 1.65* / 189* | 1.68* / 194* | 1.08* / 124* |
| O1c boot to menu, audio stubbed | 86.75 / 100.51 / 100.51 | 1.57* / 177* | 1.14* / 128* | 1.47* / 166* | 1.14* / 128* | 1.13* / 128* | 0.75 / 84 |
| O1x `app_main` to menu, audio stubbed | 67.94 / 100.51 / 100.51 | 1.29* / 177* | 0.95 / 128* | 1.21* / 166* | 0.95 / 128* | 0.92 / 128* | 0.63 / 84 |
| O2 menu idle 10 s | 2.79 / 4.59 / 6.39 | 0.32 / 37 | 0.30 / 34 | 0.31 / 36 | 0.29 / 32 | 0.21 / 25 | 0.20 / 22 |
| O2c menu idle 10 s, audio stubbed | 2.75 / 4.38 / 6.38 | 0.32 / 37 | 0.30 / 34 | 0.31 / 36 | 0.29 / 32 | 0.21 / 25 | 0.20 / 22 |
| O3 20 DOWN clicks | 9.61 / 14.97 / 16.96 | 0.42 / 53 | 0.37 / 44 | 0.40 / 50 | 0.36 / 43 | 0.28 / 36 | 0.24 / 29 |
| O4 Display demo 10 s | 3.67 / 4.52 / 81.41 | 0.33 / 149* | 0.31 / 109* | 0.32 / 140* | 0.30 / 109* | 0.22 / 107* | 0.20 / 72 |
| O4t Display demo entry 0.5 s | 21.14 / 81.41 / 81.41 | 0.59 / 149* | 0.48 / 109* | 0.56 / 140* | 0.47 / 109* | 0.41 / 107* | 0.32 / 72 |
| O4b Display demo, OK every 1 s | 5.84 / 14.92 / 81.41 | 0.37 / 149* | 0.33 / 109* | 0.35 / 140* | 0.32 / 109* | 0.24 / 107* | 0.22 / 72 |
| O5 Button demo 10 s | 5.80 / 6.61 / 83.03 | 0.37 / 151* | 0.33 / 110* | 0.35 / 142* | 0.32 / 111* | 0.24 / 109* | 0.22 / 73 |
| O5b Button demo, DOWN every 1 s | 7.29 / 14.36 / 83.03 | 0.39 / 151* | 0.34 / 110* | 0.37 / 142* | 0.33 / 111* | 0.26 / 109* | 0.23 / 73 |
| O6 Low Power card 10 s | 3.98 / 6.24 / 98.18 | 0.34 / 174* | 0.31 / 125* | 0.32 / 163* | 0.30 / 126* | 0.22 / 126* | 0.21 / 83 |
| P1 `pk` idle 10 s after ready | 2.76 / 4.32 / 6.36 | 0.32 / 37 | 0.30 / 34 | 0.31 / 36 | 0.29 / 32 | 0.21 / 25 | 0.20 / 22 |

- Busy speed needed so the worst measured window (O6, 98.18 MIPS) stays within 100 ms:
  S >= R_max / (1 - (1 - R_max / 160) x c), about 109 to 110 Minsn/s in the wasm workers
  (c = 0.26 to 0.28) and 105 natively (c = 0.18); the 100.51 MIPS boot window of O1c needs 111 to
  112 in wasm. Achieved: 60 to 65 (G1) and 85 (G1+BC) in wasm workers, 82.6 and 129.5 natively. Only
  native with the block cache meets it; in wasm the block cache narrows the shortfall from 45 to 50
  Minsn/s to about 25.
- In real time with today's interpreter every wasm host runs clicks 2.3 to 2.8x and idle 3.1 to 3.5x
  faster than real time. Card-opening windows run late by up to 74 ms with G1 and 26 ms with the
  block cache (model), and the backlog drains within 2 of the following idle windows.

*Block-cache prototype* (pre-decoded blocks of up to 32 instructions, rebuilt when the guest stores
into their memory page, the flash MMU window is remapped or programmed, or the image is reloaded;
output byte-identical to the plain interpreter). Speedup, median G1+BC over median G1:

| Host | Thread | PN1 | PN3 | O3 run | Fitted S |
|---|---|---|---|---|---|
| native | n/a | 1.32x | 1.39x | 1.30x | 1.57x |
| Node 26.7 | node | 1.25x | 1.14x | 1.20x | 1.41x |
| Chrome 153 | main | 1.22x | 1.22x | 1.18x | 1.36x |
| Chrome 153 | worker | 1.35x | 1.18x | 1.21x | 1.42x |
| Safari 27 | main | 1.14x | 1.10x | 1.17x | 1.30x |
| Safari 27 | worker | 1.21x | 1.13x | 1.17x | 1.32x |

- In wasm the block cache gains less than natively; on the whole O3 run idle bookkeeping (c,
  unchanged) dominates. Why the wasm gain is smaller was not profiled (UNVERIFIED). The block cache
  does not reduce the per-access cost of unmodelled MMIO. Natively with the block cache disabled, the
  page bookkeeping alone costs PN1 57.3 to 66.2 and PN3 66.2 to 74.7 Minsn/s, O3 6.5 s.

*Interpreter ceiling* (rv32emu interpreter with block chaining and macro-op fusion, no JIT; wasm build
with tail calls and -O3, no LTO; CoreMark 80,000 iterations = 23,044,524,102 instructions, Dhrystone
500 M passes = 72,500,011,524 instructions; browser runs in a classic dedicated worker):

| Host | Thread | CoreMark iterations/s (run time) | CoreMark Minsn/s | Dhrystone DMIPS (run time) | Dhrystone Minsn/s |
|---|---|---|---|---|---|
| native | n/a | 1713.3 (46.69 s) | 493.5 | 2859 (99.52 s) | 728.5 |
| Node 26.7 | node | 545.2 (146.7 s) | 157.1 | not run | not run |
| Chrome 153 | worker | 702.3 (113.9 s) | 202.3 | not run | not run |
| Safari 27 | worker | 589.2 (135.8 s) | 169.7 | not run | not run |

- wasm keeps 32 to 41 % of the native CoreMark speed; Chrome's worker is 19 % faster than Safari's
  here, the opposite of the esp32sim ordering. CoreMark has no MMIO and no interrupts, so this is a
  ceiling for instruction execution, 2.6 to 3.4x the esp32sim interpreter's fitted busy speed.

*Conclusions stated by G1.*

- Interactive modes (menu, clicks, demo cards, `pk` idle): interpreter with block cache in a dedicated
  worker, paced by a virtual clock that may fall up to about 100 ms behind after boot and catches up.
  No JIT and no native core are needed for the measured UI scenarios.
- Boot: fix the model (a fast NACK on I2C0 for the absent codec), not the CPU tier.
- Agent fast-forward: block cache plus event-driven idle; in wasm 86 to 90 % of menu-idle host time is
  idle bookkeeping.
- Audio, BLE and Wi-Fi together: measure first; a JIT is the contingency. A worker at about 85
  Minsn/s leaves no room for transitions if steady demand exceeds about 40 MIPS.
- Native core: 1.3 to 1.4x wasm for the same interpreter (82.6 vs 60 to 65) and 1.5 to 1.6x with the
  block cache (129.5 vs 81 to 85), smaller than the gains available inside wasm.
- Ordered work: event-driven idle (target c of about 0.03 to 0.05 in wasm, estimated, UNVERIFIED);
  block cache, then remove per-instruction and per-access costs; a busy speed of about 110 Minsn/s in
  a wasm worker, which a tuned block interpreter can plausibly reach without a JIT (UNVERIFIED for
  MMIO-heavy firmware code); remove the boot I2C timeouts and the light-sleep spin.

**Confidence.** Demand: measured on an oracle, deterministic, cross-checked against QEMU run as a
black box. Native and wasm supply for PN and O3: measured on an oracle, one host (Apple M3 Pro on AC
power, foreground Safari, headless Chrome); a first Safari series disturbed by restored tabs was
discarded and rerun with one-shot run tokens. Browser figures for every scenario except O3: model
predictions (estimated). Audio, BLE, Wi-Fi and light-sleep demand, the silicon boot profile, the
causes of the QEMU timer differences, background tabs, battery power and slower Macs, rv32emu with
LTO, and the block cache under self-modifying code or on the C6: UNVERIFIED.

## g3-safari-performance-now: Whole-millisecond `performance.now()` in Safari

**Behavior.**

*The observation.*

- In the G1 browser measurements (same host and browser builds as g3-g1-cost: Apple M3 Pro, Safari
  27.0 in a foreground window, Chrome 153 headless, Node v26.7.0), every elapsed time Safari reported
  from `performance.now()` was a whole number of milliseconds. The harness computed each figure as the
  difference of two `performance.now()` readings around one emulator run call; in Safari every such
  difference was an integer, on the main thread and in a dedicated module worker alike.
- Chrome 153 and Node v26.7.0 returned fractional values in the same runs, so the coarseness is a
  property of that Safari build and not of the harness.
- The pages were served over plain HTTP from a loopback origin and were not cross-origin isolated (no
  COOP/COEP headers, no SharedArrayBuffer). Whether cross-origin isolation, a different origin or a
  later Safari build restores sub-millisecond resolution was not measured: UNVERIFIED.
- Resolution, not accuracy: the Safari phase totals agreed with the other hosts (g3-g1-cost supply
  tables), and Safari was consistently about 5 % faster than Chrome on the same phases. Nothing
  suggests a clock that drifts or runs at the wrong rate; only that single readings are quantized.

*What quantization does to a measurement.*

- The G1 run calls were 2 M guest cycles, which took roughly 25 to 40 host ms in Safari, so a single
  reading carries a quantization error of up to about 1 ms, that is 2.5 to 4 %. Sums over hundreds of
  run calls average the error out: the phase figures and the 14.5 s run totals of g3-g1-cost are not
  materially affected, and the Safari medians differ from Chrome's by more than the quantization
  bound.
- A per-window figure is not safe. The 10 ms emulated windows of the G1 scenario took 2 to 5 host ms
  each in Safari; at 1 ms resolution a single window's host time is quantized by 20 to 50 % of itself,
  and the "worst 100 ms window" figures in Safari (186 to 250 host ms) are exact to about 1 ms only
  because those windows are long.
- Consequence for our own instrumentation: report host time over a phase, never over one short slice,
  when the reading comes from a browser clock; a slice shorter than about 20 host ms needs many
  repetitions before its mean is meaningful, and a per-slice histogram from Safari shows quantization
  steps that are artifacts, not behavior.

*Consequence for pacing.*

- A pacing loop that asks "how much host time has passed since the last anchor" and derives the next
  emulated deadline from it must tolerate a host clock whose answer moves in 1 ms steps. With a run
  slice near 1 ms, consecutive readings can report 0 ms elapsed, so a loop that divides by the elapsed
  time, or that only advances when the elapsed time is positive, can stall or report an infinite
  rate.
- The safe shape stated by the measurements: keep the emulated clock authoritative and re-anchor
  against the host clock over intervals long enough that 1 ms is small (tens of milliseconds or more),
  rather than converting each short slice to a rate. This is the same conclusion the real-time
  condition of g3-g1-cost reaches for a different reason, so pacing accuracy is limited by the 100 ms
  window budget there, not by the host clock's resolution.
- Audio pacing is unaffected by this: an audio anchor counts samples the output device has consumed,
  not host milliseconds.

**Confidence.** Measured on an oracle (esp32sim built for wasm, run as a black box in the browser),
one host and one Safari build; the integer property held for every Safari reading in those runs. The
cause, whether cross-origin isolation or another origin changes it, and the behavior of other Safari
or WebKit builds: UNVERIFIED. The pacing-loop consequences are stated from the resolution figure, not
from a pacing run: no paced browser run was measured at G1.
