# QEMU oracle console captures

One console capture per probe whose path the QEMU oracle models, taken by running the probe's
merged 8 MB image under the Espressif QEMU oracle. The M3 captures are described first;
the later probes, taken with the pinned oracle through `xtask oracle consoles`, follow in
"Captures of the later probes".

These are **observations, not goldens**. They record what the oracle actually printed, including
where it stopped, so the emulator has something real to be compared against and so the gaps in
the oracle are written down instead of being rediscovered. The M3 files were captured by hand
with the command below.

## How they were taken

```sh
QEMU_BIN=<qemu>/build/qemu-system-riscv32 qemu-c3 -M esp32c3 -display none -monitor none \
  -serial file:<name>.uart0.log -serial null -serial file:<name>.usj.log \
  -drive file=<name>-8MB.bin,if=mtd,format=raw
```

- QEMU 9.2.2 (`esp_develop_9.2.2_20260417`) with the local USB Serial/JTAG patch, which forwards
  endpoint 1 to a chardev. Serial 0 is UART0 (the ROM banner), serial 2 is the USJ console, which
  is where the probe lines are: every probe sets `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG`.
- The merged image is the one `cargo xtask probes` wrote under `CORPUS/probes/`, built from the
  sources `tests/fw/manifest.toml` records.
- CR bytes are stripped, so the files are LF text like every other tracked text file. The device
  and the oracle both send CRLF; the reader accepts either (`xtask/src/probes/line.rs`).

## What the M3 oracle build does and does not model

This section is about the hand-patched QEMU 9.2.2 the five M3 captures were taken with. The later
captures use the pinned oracle, which differs in this respect (next section).

**Its USJ has no SOF interrupt**, and that is the single fact that shapes the five M3 captures.
IDF's connection monitor (`usb_serial_jtag_connection_monitor.c`) is a FreeRTOS tick hook that
watches the SOF interrupt raw bit and declares the host gone after 3 ms without one; from then on
`usb_serial_jtag_write` returns -1 and **every `printf` is dropped**. That patched QEMU device
never raises SOF, so an M3 capture holds only what the probe printed in its first few milliseconds
of guest time. The emulator models SOF and this monitor, so its own consoles are not cut this
way.

Also missing: the CPU interrupt matrix ignores the edge/level type bit and the edge latch; the
RTC_CNTL time registers read 0, so `esp_clk_rtc_time` returns 0; light sleep never signals a wake,
so `rtc_sleep_start` spins until the interrupt watchdog resets the chip; the USJ driver's
interrupt-driven TX ring is never drained; and the raw RTC_CNTL reset cause is `0x0c` for every
reset the oracle produces, deep sleep and the watchdogs included, where silicon gives
`0x05` after deep sleep and a watchdog cause after each watchdog. `probe_reset.txt` shows this
directly: `reason` follows the IDF hint correctly (5 after deep sleep, 6 after each watchdog
panic) while `raw` stays `0x0c`. The twenty `esp_restart` boots, which must give `0x0C` each
time, do give it.

| Capture | How far it gets | Why it stops |
|---|---|---|
| `probe_boot_facts.txt` | complete, `DONE\|status=ok` | the whole probe runs inside the SOF tolerance |
| `probe_reset.txt` | complete, `DONE\|status=ok`, 25 boots, `restart_reasons_ok=20` | each boot prints for only a few ms, so every boot is inside the tolerance |
| `probe_intc.txt` | through both `EDGE` lines | `run_yield` waits on `vTaskDelay`, past the SOF tolerance; the probe keeps running, the console does not |
| `probe_clocks.txt` | `CLKCFG`, then the light-sleep panic | the 20 ms `busy_loop` is past the tolerance, so the `CLK` lines are dropped; the run then hangs in light sleep until the IWDT resets the chip, and repeats. Only the first boot is kept here |
| `usj_echo.txt` | through the first `BURST` | the 1 s heartbeat is far past the tolerance |

`probe_intc.txt` is still worth having: it carries the `MAP` re-route line, the fabric fact that
the MAP register alone decides which CPU line a source reaches
(`MAP|from_line=7|to_line=8|...|isr_a=0|isr_b=1`).

## Captures of the later probes

Taken with the pinned `qemu-oracle` configuration (binary
`eb28ffc878f8b2d2`, ROM `8b41b9b114e110e3`, synthesized eFuse `555952c9daccdf75`, strap `0x0A`
requested and `boot:0xa` read back from every console), through `xtask oracle`, the oracle
treated as a black box:

```sh
cargo xtask oracle consoles --print --images pk      # the resolved pins, as an env file
# then, per probe, that env file with:
#   PEMU_ORACLE_IMAGES="<name>=<scratch copy of CORPUS/probes/<name>-8MB.bin>"
#   PEMU_ORACLE_OUT=<scratch directory>   PEMU_ORACLE_TIMEOUT=30 (90 for probe_wdt)
#   PEMU_QEMU_TRACE_FLAGS=""              (no memory-region trace: not needed, and tens of MB/s)
cargo xtask oracle consoles --config <that env file>
```

- The image is a **copy** of the merged image `cargo xtask probes` wrote. QEMU writes flash back
  into its `-drive` file, and `flash_stress` programs its scratch partition, so running the
  original would change the file whose SHA-256 `tests/fw/manifest.toml` records.
- The file is the USJ console (`<name>.usj.console`) with CR bytes stripped. Unlike the M3 build,
  this oracle keeps the USJ console alive past the 3 ms SOF tolerance: every run below prints for
  seconds of guest time (90 s for `probe_wdt`) and reaches its `DONE` footer, which the SOF cut of
  the previous section would have made impossible. The run header the driver writes carries local
  paths and is not committed; the pins above are the part of it that matters.
- No capture holds a MAC, a unique id or a calibration word: the eFuse image is synthesized and
  none of these probes prints identity data.

| Capture | How far it gets | Notes |
|---|---|---|
| `probe_limits.txt` | complete, `DONE\|status=ok` | heap regions, allocate-until-failure per capability, `malloc(96000)`, the largest-block boundary |
| `probe_panic.txt` | complete, two boots | the load access fault panic text of the NULL read, then `AFTER\|reason=4\|raw=0x0c` |
| `probe_wdt.txt` | complete, three boots | the task watchdog names IDLE and `wdt_owner`; the interrupt watchdog fires in the critical section; reasons 6 and 5. QEMU's panic register dumps after a watchdog print no register lines, which is an oracle gap, not a probe fact |
| `probe_deadlock.txt` | complete | both tasks blocked, each holding one mutex, and the task table |
| `hle_probe.txt` | complete | the native bodies of the hook points, which is the reference an HLE-bound run is compared with |
| `flash_stress.txt` | complete, two boots | QEMU's flash model erases, programs (AND semantics included) and persists across a reset. Its `TIME` line is QEMU icount time and means nothing for the timing profile |
| `probe_crypto.txt` | complete, `DONE\|status=ok` | taken the same way (timeout 60 s). mbedTLS AES in DMA mode, GCM, RSA-2048 and MPI: all ten `AES`, `RSA` and `MPI` lines equal the host's computation and the emulator's, `stream=` of `aes_ctr128` included, whose first five bytes are ciphertext because IDF copies the padded tail block's whole output there. The oracle is a third, independent computation here, not a silicon observation |

Probes with **no** capture, because QEMU does not model their path; each was run once the same way
to confirm it, and nothing was kept:

| Probe | What the run showed |
|---|---|
| `probe_stack` | `ARMED`, then silence: no hardware stack guard, so the recursion runs off the stack without a fault |
| `sleep_timer` | the first light sleep never wakes and the interrupt watchdog resets the chip (the same gap as `probe_clocks` above) |
| `probe_timing` | the flash read, SHA-256 and erase lines, then a hang in the SPI2 transfer: QEMU maps no SPI2 or I2C0 (07 L16), and icount time is no timing reference anyway |
| `scan3`, `probe_wifi_http` | `esp_phy_enable` asserts on the missing modem clock bits: no Wi-Fi |
| `probe_wifi_conn` | the same Wi-Fi path as the two above, and the same gap. Not run separately: it calls `esp_wifi_init` and `esp_wifi_start` before anything it prints, which is exactly where `scan3` and `probe_wifi_http` stop, so a run would record the oracle's missing modem clock a third time |
| `probe_wifi_assoc` | not run. The committed build has an empty SSID, which compiles its whole radio path out, so a capture would pin none of the association it exists for; a credentialled build takes the Wi-Fi path above and meets the same missing modem clock |
| `pkgatt` | `btdm_low_power_mode_init` asserts: no BLE controller |

`xtask/src/probes/tests.rs` lists these eight with the same reasons and fails if a capture for one
of them appears, or if a probe without a reason has no capture.

## What the tests do with them

`xtask/src/probes/tests.rs` parses every file with the same reader `cargo xtask probes read` uses,
checks that each capture's probe lines carry the tags its firmware prints, and checks that the
nine captures that reach their footer (`probe_boot_facts` and `probe_reset` of M3, and the seven
later ones) grade as passing runs. Some captures get content checks too: the panic's fault address
against the `probe_panic_read_null` symbol, the `flash_stress` stages, the `probe_limits` region
numbers and the `hle_probe` delay bounds. That is what ties the reader to what the firmware really
prints, rather than to a hand-written example.
