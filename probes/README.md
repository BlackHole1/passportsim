# Probe firmware

Small ESP-IDF v5.5.3 applications, ours and MIT, built to be run under the emulator, under the
QEMU oracle and (only with explicit user approval, through the planner) on the device, so the
three can be compared line by line.

## Core probes

| Probe | What it drives | Where its facts are checked |
|---|---|---|
| `probe_boot_facts` | chip and eFuse revisions, reset reason, strap, flash id, heap regions, partition table, placeholder MAC | `xtask/src/probes/tests.rs`; the ECO7 heap regions |
| `probe_intc` | the two free `FROM_CPU` sources: routing, MAP re-route to another CPU line, handler latency, THRESH masking, edge versus level, the yield path | `tests/milestones/m3.rs`, `tests/milestones/m11.rs` |
| `probe_clocks` | esp_timer, the FreeRTOS tick, the CPU cycle counter and the RTC counter across five phases, with the one-tick rule asserted on chip | `tests/milestones/m3.rs`, `tests/milestones/m11.rs` |
| `probe_reset` | twenty `esp_restart` calls, deep sleep, task watchdog, interrupt watchdog, RTC watchdog; RTC RAM retention, `STORE4..7`, SENSITIVE lock bits | `tests/milestones/m3.rs`, `tests/milestones/campaign.rs` |
| `usj_echo` | the USB Serial/JTAG port in driver mode: host bytes read and reported, connection state, write-burst timing | `xtask/src/probes/tests.rs` |

## Further probes

These are built, pinned in `tests/fw/manifest.toml` and, where the QEMU oracle models their path,
captured.

| Probe | What it proves | Consumed by |
|---|---|---|
| `probe_limits` | For five capability sets, allocating until an allocation fails drains a total close to the heap regions (`LIMIT`, and `LIMREG` per region with its bounds, used bytes before and after, and bytes drained); `malloc(96000)` succeeds; a request equal to the largest free block succeeds and one byte above it fails (`AUDIO`, `ABOVE`). `t1_m5_limits_heap` compares the regions with a silicon capture of the same image, identified by `IMAGE`; the `pk` numbers are context only, because the first DRAM region depends on the image's static data (larger here than the 117 KiB of `pk`, while the 113, 10 and 7 KiB regions match) | `t1_m5_limits_heap`, `t1_m6_limits_audio` |
| `probe_panic` | A NULL read in the task `panic_task`, through the `noinline` frames `probe_panic_read_null` and `probe_panic_outer`, is a load access fault (mcause 5, MTVAL 0) with IDF's panic text; the next boot reports reason PANIC | `t1_m7_panic_envelope` (frames against `riscv32-esp-elf-gdb bt` on the unstripped ELF) |
| `probe_wdt` | A task holding a mutex that a higher-priority task waits on spins and starves IDLE, and the task watchdog names IDLE and the running owner (`TWDT` names owner, waiter and holder first); a spin inside a critical section then trips the interrupt watchdog; the reasons are TASK_WDT (6) and INT_WDT (5) | `t1_m7_watchdog_envelope` |
| `probe_deadlock` | Two tasks each hold one mutex and wait forever on the other's; the probe prints both states, both holders and the task table, then returns, leaving nothing that can make progress | `t1_m7_deadlock_and_stack_guard` (`E_DEADLOCK`) |
| `probe_stack` | A recursing task with a 2048-byte stack is stopped by the hardware stack guard, not by FreeRTOS's canary (turned off), so the panic names the task; the next boot reports reason PANIC and the depth reached | `t1_m7_deadlock_and_stack_guard` (stack-guard reason) |
| `hle_probe` | Five `noinline` hook points (`hle_probe_hook_delay`, `_malloc`, `_free`, `_post`, `_isr_give`) whose native bodies do the work directly, so the same lines print on silicon, under QEMU and with the HLE bound; two outstanding delay calls cross a context switch; a task deleted inside a hook leaves no tasks or heap behind; an ISR hook call wakes a waiting task. The symbol names are the binding contract with the HLE; no binding hooks them yet, so they are UNVERIFIED as hook points | `t1_hle_probe_passes_with_hle_bound_and_native_bodies` |
| `scan3` | The Wi-Fi scan and BLE advertising probe, ported from our earlier prototype firmware. Its `HEAP`, `TASK`, `EVT`, `RC` and `AP` lines are the prototype's under the virtual air. Added: a probe-line header and footer; a `FAIL` line for every `RC` code that differs from the prototype's recorded run (init, start, stop and deinit codes, including 12289 and 12291, `sta_start_seen`, `sta_stop_seen`, the scan's `start`, `done`, `num_rc` and `rec_rc`, NVS and the NimBLE codes). Not checked: the second scan start's `busy` 12294, which came from our HLE and is UNVERIFIED against the real driver, so a check would only test the HLE against itself; and `num`, `records`, `ms`, heap and task numbers, which depend on the air and the engine. An access point whose BSSID is not the virtual air's `02:00:00:47:32:xx` prints `(neighbour)` for its SSID and only its vendor prefix, so a device run records no neighbour's identity | `t1_m8_scan3_without_wifi_advertises_folopassport`, `t1_m12_scan3_prints_the_48_ordered_lines_of_s3b` |
| `pkgatt` | The Passport Keys GATT probe, ported from the same prototype: `pk_ble.c` and `pk_protocol.c` are the Passport Keys sources, unmodified (MIT, copyright FoloToy, `pkgatt/main/LICENSE.passport-keys`); a central that subscribes and writes two command lines gets the protocol answers | `t1_m8_pkgatt_console_matches_g2_gt3` |
| `probe_crypto` | mbedTLS through the crypto accelerators: AES-CBC 128 and 256 over 4 KB (encrypt, then decrypt back), AES-CTR over 4101 bytes in one call and in two that continue through `nc_off`, AES-GCM over 4 KB with 20 bytes of AAD (IDF's GCM port: software GHASH over CTR runs), an RSA-2048 public operation, MPI products of 1024, 1536 and 2048 x 1024 bits and a 2048-bit exponentiation. Every input comes from a fixed xorshift32 stream, so each `AES`, `RSA` and `MPI` line (FNV-1a 64, first and last 16 bytes, chaining values, tag) is computed on the host with no device: `host_crypto` in `tests/milestones/m8.rs`, cross-checked by `tools/probe_crypto/host_expected.py`. Built with the device's own partition table and writes no flash, so it can run on the device | `t1_m8_probe_crypto_matches_the_host_computed_values`, `t1_m8_probe_crypto_matches_the_device_capture` |
| `sleep_timer` | Three 500 ms light sleeps return by timer with esp_timer and RTC time advanced; a 1 s deep sleep comes back as DEEPSLEEP (raw 0x05) with wake cause TIMER, an RTC data counter intact and the RTC time advanced by the programmed second | `t1_m10_sleep_timer_probe_light_and_deep_sleep` (`t1_m10_official_low_power_light_and_deep_sleep` covers the path on `official`) |
| `flash_stress` | In its own 256 KB `scratch` partition: a full erase reads 0xFF; a pattern reads back; programming without an erase gives the bitwise AND; a one-sector erase touches only that sector; a write straddling a sector boundary reads back; sixteen erase and program cycles; a marker survives a restart. No flash call may fail | the flash model (build and pin: `xtask/src/probes/tests.rs`) |
| `probe_timing` | esp_timer deltas around a 64 KB flash read, 1 MB of SHA-256, a 153,600-byte SPI2 frame, 100 I2C reads from the ES8311 and a 4 KB erase, each with a fact (CRC, digest, count) that shows the work was done | `t1_m11_probe_timing_deltas_within_20_percent`, only with an approved device capture |
| `probe_wifi_conn` | The association half, on silicon: `esp_wifi_init`, `set_mode(STA)`, `esp_wifi_start` and one `esp_wifi_connect()` against an SSID that names no access point, so the outcome is a `WIFI_EVENT_STA_DISCONNECTED` with `reason=201` and **no credential exists anywhere** (there is no password option in its `Kconfig.projbuild`). One `EVT\|WIFI_EVENT\|seq=..\|id=..` line per event, printed from the handler so the console order is the arrival order, which is what settles whether that event arrives as `id=43` or `id=2`; `RC\|<function>\|<code>` for every call, teardown included; a bounded wait that ends in a `NOTE\|` rather than a retry loop. **The one probe built with the Passport's own partition table** (`partitions.csv`), so the identity guard accepts a flash of it | the device captures of the association path; its image is pinned and symbol-checked in `tests/milestones/m8.rs` |
| `probe_wifi_http` | Joins an open access point, gets a DHCP lease, and a GET to the gateway returns 200 with a body hash. The SSID and password come from `main/Kconfig.projbuild`, whose defaults are the obviously fake `passport-emu-virtual-ap` and an empty password: no real network credential is in the tree, and a run against a real network sets them only in a local, uncommitted sdkconfig | `t1_m12_probe_wifi_http_leases_an_address_and_gets_200_with_the_body_hash` |

### The silicon evidence campaign

Four probes built to run on the device, each settling class C and UNVERIFIED rows of
`specs/` that `specs/notes/silicon-campaign.md` lists with their dispositions. Every fact line
names the row it settles in a `row=` field, and the emulator's run of each pinned image is
recorded in `tests/fw/campaign/<probe>.emu.txt`, so a device capture compares mechanically:
`cargo xtask probes compare <capture> tests/fw/campaign/<probe>.emu.txt` lists every row as
equal, different or not printed. All four carry the device's own partition table, write no flash
and no eFuse, transmit nothing a corpus image does not, and end with `DONE|` well under 60 s.

| Probe | What it observes | Rows |
|---|---|---|
| `probe_campaign_regs` | boot values of every register of a class C row a corpus image reaches (`main/reg_facts.h`) and of the IO_MUX pads; SYSCON_RND_DATA behaviour; register writes with a block's clock off or reset held (I2S0, AES, TIMG0, TIMG1); the BT low-power divider's writable bits; one flash read at 0x800000; the button ladder idle and with Up, Down and OK pressed when a `NOTE` asks; the USJ frame-number width | the block rows of sections 1 and 5 of the note |
| `probe_campaign_timing` | cold and warm cache fills, AES-CBC at four sizes, RSA exponentiation with and without constant time, a wrong M', the slow-clock next edge and calibrations, SYSTIMER comparator loads, USJ packet drain, I2C0 at 100 and 400 kHz and UART0 at two bauds with the registers their drivers set | the timing constants and the I2C0, UART0, RSA and RTC rows |
| `probe_campaign_radio` | the BLE controller up and down once with no host stack: call times, free heap around each call, the BT interrupt routing, HCI Reset and Read Local Version round trips over VHCI, the controller's own log lines with the MAC withheld | `timing-profiles.ble_init_ps` and the BLE HLE rows |
| `probe_campaign_reset` | every boot first waiting (5 s at most, then 1.5 s of settle) for the USB host that the reset or the sleep before it disconnected, so its lines reach a reconnecting capture; a super-watchdog reset with auto-feed off and its timeout (`SWD|timeout`: the arming write feeds once, and the time since it is stored in RTC memory every 1 ms until the reset), a 3 s timer deep sleep with GPIO0 armed beside it, the reset cause and RTC time counter on each boot, the RTC retention words after the wake | the SWD, GPIO wake and RTC retention rows |

### Device runs

Every probe writes only through ESP-IDF, and all but two write nothing outside the bootloader, the
partition table and the app. **`flash_stress` and `probe_timing` erase and program their
`scratch` partition, 256 KB at 0x500000**, which is outside all three. A device run of either one
therefore needs explicit approval and, before the run, a backup of 0x500000-0x53FFFF read
from that device. The region is unallocated in the device's own table and read blank in a
full-flash backup, and it is clear of the device's `factory` partition, of cardid 0x356000-0x359FFF
and of the non-blank leftovers at 0x3FA000-0x41A000; none of that makes a run without approval
acceptable. Every write is bounded by the partition: `esp_partition_*` refuses anything past it,
and `flash_stress` checks the partition is large enough before its first write.

**`probe_wifi_conn` is the radio probe that also writes nothing outside those three**, which took
three settings rather than none. ESP-IDF has two routine NVS writers on a Wi-Fi path, the driver's
own NVS store and the PHY calibration store, and both are off in its `sdkconfig.defaults`
(`CONFIG_ESP_WIFI_NVS_ENABLED=n`, `CONFIG_ESP_PHY_CALIBRATION_AND_DATA_STORAGE=n`); the probe adds
`esp_wifi_set_storage(WIFI_STORAGE_RAM)` and, unlike every other probe that calls
`nvs_flash_init`, has **no** `nvs_flash_erase` recovery, so an unmountable partition is reported
rather than erased. That is the configuration of the earlier device Wi-Fi probes (the
`wifi_facts` and `scanblock` captures), and the device's `nvs` holds the installed firmware's own
data (202 non-0xFF bytes). None of it changes the association facts the
probe measures. Nothing else on the part is touched either: `cardid` is declared in the probe's table
and carries no data, so no segment of the merged image falls in [0x356000, 0x35A000).

`probe_wifi_conn` is also the first probe whose partition table is the **device's own**
(`nvs`, `phy_init`, `factory` at 0x10000 size 0x300000, `cardid` at 0x356000 size 0x4000). Its
built `partition_table/partition-table.bin` is byte-identical to `official.pt`
(SHA-256 `a98e0784a39f37da...`), and `plan_flash` accepts its merged image with no
refusal, where every other probe's image is refused for moving `cardid`. `probe_wifi_assoc`,
`probe_crypto` and the four `probe_campaign_*` probes carry the same table (their built tables
have the same SHA-256, recorded in `tests/fw/manifest.toml`). Two tests hold that in place with no
toolchain: `probe_wifi_conn_carries_the_devices_own_partition_table` compares the committed CSVs
of `probe_wifi_conn`, `probe_crypto` and the campaign probes with
`tests/fixtures/device-facts.toml`, and
`no_other_probe_table_declares_cardid` fails if any other probe's table reaches into the identity
region.

The two ported prototype probes keep the prototype's console formats, which are valid probe lines. `pkgatt`
changes one thing in what it prints: the bytes a central wrote are rendered with anything outside
0x20 to 0x7E, and `|`, replaced by `.` (see below). A well-formed Passport Keys command is plain
JSON, so its `RX` line is the prototype's line.

## Console line format

Every probe prints ordinary IDF log lines plus *probe lines*, the machine-readable part:

```text
TAG|positional|positional|key=value|key=value
```

`probes/common/probe_line.h` states the grammar and is the authority for firmware;
`xtask/src/probes/line.rs` parses the same grammar and is the authority for the host. The shape
follows the earlier prototype probes, which already print `HEAP|stage|free=..|largest=..`.

Probe lines never carry a real MAC, unique id, calibration word or cardid byte (`docs/secrets.md`).
`probe_boot_facts` prints the base MAC only when it is a placeholder (leading `02:00:00`).

A probe also never writes anything a host sent into the console. The console is the stream the
reader grades, so host bytes echoed into it would let any host forge a `DONE|status=ok` line or
split a probe line in two; `usj_echo` reports what it read as hex and as a rendering with every
byte outside 0x20 to 0x7E, and `|` itself, replaced by `.`.

## Building

`cargo xtask probes` builds them all with the local ESP-IDF; `cargo xtask probes --check`
rebuilds and compares `tests/fw/manifest.toml`. Both are macOS-only and refuse to run anywhere else; `cargo xtask probes verify`, `list` and `read` need
no toolchain and no build, so they run on any host. `--idf <dir>` names an ESP-IDF other than
`IDF_PATH`. Nothing here ever opens a serial port: no flashing, no monitoring, no
talking to the device.

The **stripped** ELF of each probe is committed at `tests/fw/<name>.elf`, under the 1 MiB limit for
committed test ELFs: 165 KB to 245 KB for most probes, about 300 KB for the two that keep their
symbols, 550 KB for `pkgatt`, 784 KB for `probe_wifi_conn`, 931 KB for `probe_wifi_http` and
1,017,244 bytes for `scan3`
(`elf_stripped_size` in `tests/fw/manifest.toml` has each exact size). It is made by
`riscv32-esp-elf-strip` with `-R .flash_rodata_dummy -R .dram0.dummy` from the build's own ELF, and
is still loadable, disassemblable and runnable. That is why a fresh checkout with no ESP-IDF still has a probe artifact to
run. The unstripped ELF (about 3.3 MB of debug information, 10 MB for a radio probe) and the merged
8 MB image are over the limit and stay outside the repository under `CORPUS/probes/`;
`tests/fw/manifest.toml` records the SHA-256 of all of them, plus the app image, the bootloader
and the partition table, with the strip command as the provenance note.

### Building one probe by hand

`cargo xtask probes` never writes into `probes/`. `idf.py` run by hand does: with no `SDKCONFIG=`
it writes `sdkconfig` and `sdkconfig.old` into the project directory. Both are ignored
(`.gitignore`), but a hand build should still keep the build and its configuration outside the tree,
and must when it sets a real network for `probe_wifi_http`:

```sh
. "$IDF_PATH/export.sh"
OUT="$HOME/Library/Application Support/passportsim/corpus/probes/hand/probe_wifi_http"
D="$PWD/probes/probe_wifi_http"
idf.py -C probes/probe_wifi_http -B "$OUT/build" \
  -D SDKCONFIG="$OUT/sdkconfig" \
  -D "SDKCONFIG_DEFAULTS=$PWD/probes/common/sdkconfig.defaults;$D/sdkconfig.defaults" \
  menuconfig build
```

`menuconfig` is where the real SSID and password go (menu `probe_wifi_http`); they then live only
in `$OUT/sdkconfig`, never in `main/Kconfig.projbuild` or `sdkconfig.defaults` (docs/secrets.md).

Two committed ELFs keep their symbol table (`--strip-debug` instead of `--strip-all`, the
`strip` field of the manifest): `hle_probe`, whose `hle_probe_hook_*` symbols a binding names,
and `probe_panic`, whose `probe_panic_read_null` and `probe_panic_outer` frames `t1_m7_panic_envelope` names. Both
stay near 300 KB.

**Why the two `-R` options.** ESP-IDF's linker script reserves two address ranges with `NOBITS`
placeholder sections, `.flash_rodata_dummy` and `.dram0.dummy`. They hold no bytes, but they lie
inside `PT_LOAD` segments, so a plain strip still writes zero padding for them into the file. In
`scan3` that padding is 0xC0000 + 0x19800 bytes: a plain strip gives 1,906,188 bytes, while the app
image `scan3.bin` is 1,001,920 bytes. Removing the two sections gives 1,017,244 bytes. Building with
`CONFIG_COMPILER_OPTIMIZATION_SIZE` was tried first and is not enough on its own (1,750,540 bytes
for `scan3` and 1,570,316 bytes for `probe_wifi_http`, plain strip). `cargo xtask probes` checks
every build (`xtask/src/probes/loaded.rs`): each allocated section with contents is at the same
address with the same bytes, only sections with no contents are gone, and the entry point is
unchanged. `esptool elf2image` builds the app image from those sections, so it builds the same image
from either ELF, apart from the ELF SHA-256 the image embeds.

A build directory from another checkout is discarded and configured afresh, because CMake refuses
a cache made for a different source directory; the reproducible build maps the source prefix out,
so the artifacts are the same.

## Oracle captures

`tests/fw/captures/<name>.txt` holds one QEMU oracle console capture per probe whose path the
oracle models, with
`tests/fw/captures/README.md` stating how each was taken and exactly where the oracle stops
modelling the path. They are observations, not goldens, and they are what ties
`xtask/src/probes/line.rs` to what the firmware really prints.

## Layout

```text
probes/
  common/probe_line.h         the line format, included by every probe
  common/sdkconfig.defaults   shared configuration, applied before each probe's own
  <name>/CMakeLists.txt       the IDF project
  <name>/sdkconfig.defaults   the probe's overrides, with the reason for each
  <name>/main/<name>.c        the probe itself
  <name>/main/CMakeLists.txt  the component registration
  <name>/partitions.csv       flash_stress and probe_timing: the table with `scratch`;
                            probe_wifi_conn, probe_wifi_assoc, probe_crypto and the
                            probe_campaign_* probes: the Passport's own table, `cardid`
                            included
  <name>/main/Kconfig.projbuild  probe_wifi_http and probe_wifi_conn only: the placeholder
                            network settings (probe_wifi_conn has no password option at all)
```
