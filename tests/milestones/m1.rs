//! Milestone M1 tests: the ROM boot. Names use the prefix `t<tier>_m1_` so `xtask ci` can count
//! them.
//!
//! The `v0` tests are the machine bring-up gate: the bundled rev101 ROM over an erased flash prints
//! the device's banner on USJ and UART0, then loops in the flash loader on `invalid header:
//! 0xffffffff`, where the run ends and its first-touch ledger is published. They need no corpus.
//! The ledger test is meant to change: a block model that replaces a `StoreOnly` alias reruns it.
//!
//! The run is clocked at `[soc] xtal_hz / 2`, 20 MHz: the ROM phase never raises the clock (the
//! bootloader does), so the `PRE_DIV_CNT` reset value of 1 decodes as XTAL/2 throughout.
//! `Machine::reset_cpu_hz` documents why the device captures pick XTAL/2 over 40 MHz.

// Not every milestone uses every shared helper.
#[allow(dead_code)]
mod common;
use common::image_machine;
// The host legs of the determinism harness, shared with m3.rs.
#[allow(dead_code)]
mod determinism;
// The QEMU oracle side of the T2 comparisons, shared with m3.rs.
mod oracle;

use pemu_core::hostio::SerialStream;
use pemu_core::input::InputEvent;
use pemu_core::sched::PeriphId;
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::determinism::{Variant, report};
use pemu_machine::executor::Executor;
use pemu_machine::machine::At;
use pemu_machine::machine::{Machine, MmioRead};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
use pemu_soc_c3::UNBACKED;
use pemu_soc_c3::periph::BLOCKS;

/// The first line of the ROM banner. The build date proves the run took the ECO7 rev101 image the
/// eFuse selected and not the rev3 one.
const ROM_BANNER: &str = "ESP-ROM:esp32c3-eco7-20230720";

const SLICE_INSNS: u64 = 100_000;

/// Instructions one slice of the ledger run gets. The run ends on console text, so the count it
/// reports is only as precise as a slice.
const RETRY_SLICE_INSNS: u64 = 10_000;

/// Instructions the ledger run gets before giving up on the flash boot retry: a hundred times what
/// the ROM needs.
const RETRY_INSNS: u64 = 3_000_000;

/// The line the rev101 ROM prints each time the image header it reads out of flash is not one it
/// can boot, before it tries the flash boot again.
const BAD_HEADER: &str = "invalid header: ";

/// Instructions the banner gate gives the ROM: far more than the 6 263 to the first `UART_FIFO`
/// write, so a block model that lengthens the path fails on its own change, not on a missing
/// banner.
const BANNER_INSNS: u64 = 12 * SLICE_INSNS;

/// The name of the `c3_devices!` row a first touch names, or a rendering of the two sentinels
/// that are not rows.
fn block_name(id: PeriphId) -> &'static str {
    BLOCKS
        .iter()
        .find(|b| b.id == id)
        .map_or("<not a block>", |b| b.name)
}

/// A machine over the bundled ROM that a synthesized eFuse selects.
fn v0_machine() -> Machine {
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    Machine::new(MachineConfig::default(), assets).expect("the rev101 image fits the ROM window")
}

fn console(m: &mut Machine, stream: SerialStream) -> String {
    let ring = m.io().serial_ring(stream);
    let from = ring.tail();
    let bytes: Vec<u8> = ring.slices(from).iter().copied().collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where the ledger run ended: the second [`BAD_HEADER`], when the flash boot retry is a loop
/// rather than one failed attempt.
struct Retry {
    /// Instructions retired, to the end of the slice that printed the second line.
    insns: u64,
    vt: VTime,
    pc: u32,
    last_read: Option<MmioRead>,
}

/// Runs until the ROM has printed [`BAD_HEADER`] twice, or gives up after [`RETRY_INSNS`]. The
/// console head moves on every retry, so a parking detector cannot end this run.
fn run_to_boot_retry(m: &mut Machine) -> Option<Retry> {
    let mut total = 0;
    while total < RETRY_INSNS {
        let out = m.run(RunLimits::insns(RETRY_SLICE_INSNS));
        total += out.insns;
        if out.insns == 0 {
            return None;
        }
        if console(m, SerialStream::UsjTx).matches(BAD_HEADER).count() >= 2 {
            return Some(Retry {
                insns: total,
                vt: out.vt,
                pc: m.hart().pc,
                last_read: m.last_mmio_read(),
            });
        }
    }
    None
}

/// The ROM prints its first banner line over UART0.
///
/// One register decides it: the ROM's transmit path polls `UART_STATUS.UART_TXFIFO_CNT` (uart0
/// +0x01C) before each character, and 0 satisfies the poll at once (`specs/blocks/uart0.toml`, row
/// `uart0.txfifo_cnt`). `UART_FSM_STATUS.UART_ST_UTX_OUT` (+0x06C) is not part of it: only
/// `uart_tx_wait_idle` and `esp_rom_output_tx_wait_idle` poll it, and this boot reaches neither.
#[test]
fn t1_m1_v0_first_banner_line() {
    let mut m = v0_machine();
    // The gate is about what the machine lets through, not how many instructions the ROM spends, so
    // it runs slices until the banner appears or the budget is spent.
    let mut spent = 0;
    while spent < BANNER_INSNS {
        let out = m.run(RunLimits::insns(SLICE_INSNS));
        spent += out.insns;
        // A slice that retires nothing is a run that ended for a reason of its own, such as a deadlock.
        if out.insns == 0 || console(&mut m, SerialStream::Uart0Tx).contains(ROM_BANNER) {
            break;
        }
    }
    let uart0 = console(&mut m, SerialStream::Uart0Tx);
    assert!(
        uart0.contains(ROM_BANNER),
        "the rev101 ROM did not print {ROM_BANNER:?} on UART0 in the {spent} instructions this \
         gate spent (budget {BANNER_INSNS}); console so far: {uart0:?}"
    );
    assert!(
        uart0.starts_with(ROM_BANNER),
        "the banner is the first thing the ROM prints; console: {uart0:?}"
    );
}

/// The first-touch ledger of the bring-up run, from reset to the ROM's first stall, and where it
/// ends. A block model change reruns it and posts the new ledger.
#[test]
fn t1_m1_v0_first_touch_ledger() {
    let mut m = v0_machine();
    let retry = run_to_boot_retry(&mut m).expect(
        "the ROM did not reach its flash boot retry loop in the budget; a block that left \
         `StoreOnly` changed the run: post the new ledger with that block's pull request",
    );

    // Publish the ledger: block, offset, access and the instant of the first touch.
    let symbols = m.assets().rom.symbols();
    let func = symbols
        .func_at(retry.pc)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "<no ROM symbol>".to_string());
    println!(
        "first-touch ledger, {} rows; second `{BAD_HEADER}` line within {} instructions \
         ({:?}), pc 0x{:08x} in {func}, last read {:?}, {} unapplied wiring effects",
        m.ledger().first_touches().len(),
        retry.insns,
        retry.vt,
        retry.pc,
        retry.last_read,
        m.unapplied_wiring()
    );
    for t in m.ledger().first_touches() {
        println!(
            "  {:<13} +0x{:03x} {:?} size {} at {:?}",
            block_name(t.periph),
            t.off,
            t.access,
            t.size,
            t.now
        );
    }

    // `UsjModel` answers `SERIAL_IN_EP_DATA_FREE` with room in the IN FIFO, so the ROM's USJ transmit
    // path writes every character (`specs/blocks/usj.toml`, row `usj.in_ep_data_free`).
    let usj = console(&mut m, SerialStream::UsjTx);
    assert!(
        usj.starts_with(ROM_BANNER),
        "the banner is the first thing the ROM prints; console: {usj:?}"
    );
    assert!(
        usj.contains("rst:0x1 (POWERON),boot:0xa (SPI_FAST_FLASH_BOOT)\r\n"),
        "`RTC_CNTL_RESET_STATE.RESET_CAUSE_PROCPU` (rtc_cntl +0x038) holds the cause \
         `Machine::power_on` latched and `GPIO_STRAP` (gpio +0x038) reads the 0x0A of the strap \
         model (`[soc] strap`), so the ROM prints the banner line of the device. \
         Console: {usj:?}"
    );
    assert!(
        !usj.contains("invalid reset"),
        "the ROM only prints `invalid reset` for a cause IDF `soc/reset_reasons.h` does not \
         list, which is what a reset cause of 0 was. Console: {usj:?}"
    );
    assert!(
        !usj.contains("Guru Meditation"),
        "the ROM panicked, so the run reached its fatal-exception handler. Console: {usj:?}"
    );

    // Where the run ends: each MMU entry write is applied, so the image header read through the cache
    // window at 0x3C00_0000 reaches the erased flash, reads 0xFFFFFFFF, and the ROM tries again.
    assert!(
        usj.contains("invalid header: 0xffffffff\r\n"),
        "the flash boot read something other than erased flash; console: {usj:?}"
    );
    assert!(
        !m.ledger()
            .first_touches()
            .iter()
            .any(|t| t.periph == UNBACKED && t.off == 0x3C00_0000),
        "the header read landed on an unbacked address, so the MMU entries were not applied"
    );

    // It is a loop: another stretch prints the line again and touches no new register.
    let cursor = m.ledger().cursor();
    let lines = usj.matches(BAD_HEADER).count();
    m.run(RunLimits::insns(20 * RETRY_SLICE_INSNS));
    let after = console(&mut m, SerialStream::UsjTx);
    assert!(
        after.matches(BAD_HEADER).count() > lines,
        "the ROM stopped retrying the flash boot; console: {after:?}"
    );
    assert_eq!(
        m.ledger().cursor(),
        cursor,
        "the retry loop touched a new register, so the ROM got further than the header read"
    );

    // UART0 carries the same ROM output, character by character.
    let uart0 = console(&mut m, SerialStream::Uart0Tx);
    let banner_end = "(SPI_FAST_FLASH_BOOT)\r\n";
    let head = |text: &str| text.find(banner_end).map(|at| text[..at].to_string());
    assert!(
        head(&uart0).is_some() && head(&uart0) == head(&usj),
        "UART0 and USJ disagree on the banner; UART0: {uart0:?}, USJ: {usj:?}"
    );

    // The ledger holds the boot path: reset cause and strapping, the console blocks, then the flash
    // mapping and command.
    let touched = |block: &str, off: u32| {
        m.ledger()
            .first_touches()
            .iter()
            .any(|t| block_name(t.periph) == block && t.off == off)
    };
    assert!(touched("rtc_cntl", 0x038), "RTC_CNTL_RESET_STATE");
    assert!(touched("gpio", 0x038), "GPIO_STRAP");
    assert!(touched("uart0", 0x000), "UART_FIFO");
    assert!(touched("usj", 0x004), "USB_SERIAL_JTAG_EP1_CONF");
    assert!(
        touched("usj", 0x000),
        "USB_SERIAL_JTAG_EP1, the console text"
    );
    assert!(touched("efuse", 0x030), "EFUSE_RD_REPEAT_DATA0");
    // UNVERIFIED: this power-on run also reads ASSIST_DEBUG +0x048 and writes +0x044 (the RCD PC
    // and SP record), which is documented as a watchdog-reset path only; not asserted either way
    // until an oracle row settles it.
    // Past the banner the ROM programs the MMU table, turns the cache on and issues a flash command
    // through SPI1. `Spi1Model` clears `SPI_MEM_CMD` inside the write; a store-only `spi1` leaves
    // the bit set and the ROM polls it for ever in `SPI_WakeUp`.
    assert!(touched("mmu", 0x000), "MMU_TABLE entry 0");
    assert!(touched("extmem", 0x000), "EXTMEM_ICACHE_CTRL");
    assert!(touched("spi1", 0x000), "SPI_MEM_CMD of SPI1");

    // `UART_STATUS.UART_TXFIFO_CNT` is the only register that lets the banner out (see
    // [`t1_m1_v0_first_banner_line`]), so the run has to keep proving +0x06C is untouched.
    assert!(touched("uart0", 0x01C), "UART_STATUS");
    assert!(
        !touched("uart0", 0x06C),
        "the run now reads UART_FSM_STATUS (uart0 +0x06C), so it reaches a transmit path that \
         waits for the shifter to drain; the banner gate's justification names only \
         `uart0.txfifo_cnt` and has to be rewritten with the block that changed this"
    );

    assert_eq!(m.unapplied_inputs(), 0);
    assert_eq!(m.pending_board_effects(), 0);
    assert_eq!(m.pending_board_events(), 0);
    // The RWDT flash-boot hold `Machine::power_on` armed is about 2.94 s away, far past
    // this run, so no scheduled event is due yet.
    assert_eq!(m.undispatched_events(), 0);
    // Every wiring effect was applied: all 128 MMU entries (`Wiring::MmuEntry`) and the console text
    // through the `Wiring::UsjIo` ring pump, the only path USJ bytes take.
    let unapplied = m.unapplied_wiring_by_kind();
    let applied = m.applied_wiring_by_kind();
    println!("wiring by kind: applied {applied:?}, unapplied {unapplied:?}");
    assert_eq!(
        unapplied.total(),
        0,
        "an effect of the ROM phase was left unapplied: {unapplied:?}"
    );
    assert!(
        applied.mmu_entry >= 128,
        "the ROM writes 128 MMU entries (the `mmu` rows of the ledger, +0x000 to +0x1FC) and each \
         raises `Wiring::MmuEntry`; only {} were applied: {applied:?}",
        applied.mmu_entry
    );
    assert!(
        applied.usj_io > 0,
        "the ROM wrote the console to USJ and no `Wiring::UsjIo` was applied: {applied:?}"
    );
}

/// The merged flash image of each `rom-banner` subject, by corpus id and file name.
const E1_IMAGES: [(&str, &str); 3] = [
    (common::PK, "FoloToy-AI-Passport-8MB.bin"),
    (common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin"),
    (common::GOLDMINER, "goldminer-sanitized-8MB.bin"),
];

/// The second ROM banner, dev:L4-L6 and dev:L8-L13, in order.
const SECOND_ROM_BANNER: [&str; 9] = [
    "ESP-ROM:esp32c3-eco7-20230720",
    "Build:Jul 20 2023",
    "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)",
    "SPIWP:0xee",
    "mode:DIO, clock div:1",
    "load:0x3fcd5830,len:0x1584",
    "load:0x403cbf10,len:0xc44",
    "load:0x403ce710,len:0x2ff8",
    "entry 0x403cbf1a",
];

/// Instructions given to reach one `entry` line. The device reaches it at about 24 ms (dev:L14);
/// this machine needs about 150 thousand instructions per boot, far under the budget.
const ENTRY_INSNS: u64 = 50_000_000;

const ENTRY_MATCHER: MatcherId = MatcherId(1);

/// Runs to the next `entry 0x` line on the device console.
fn run_to_entry(m: &mut Machine) -> StopReason {
    m.run(RunLimits {
        until: None,
        max_insns: Some(ENTRY_INSNS),
        stops: StopSet {
            matchers: vec![(
                ENTRY_MATCHER,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("entry 0x".into()),
                },
            )],
            ..StopSet::default()
        },
    })
    .reason
}

/// The device console from absolute cursor `from`, as compared: lines without their `\r\n`, and
/// without the `Saved PC` line, which the M3 boot tests cover.
fn banner_lines(m: &mut Machine, from: u64) -> Vec<String> {
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(from).iter().copied().collect();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.starts_with("Saved PC:"))
        .collect()
}

/// Scenario `rom-banner`: power on, run to the first `entry 0x`, apply `UsbLine {rts: 1, dtr: 0}`
/// at that instant so no bootloader instruction runs, run to the second `entry 0x`, and compare
/// the second banner.
///
/// Returns the machine stopped at the second `entry` line, so the ledger test can read it; `None`
/// after printing the skip.
fn rom_banner(test: &str, id: &str, file: &str) -> Option<Machine> {
    let path = common::corpus_file_or_skip(test, id, file)?;
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let mut m = image_machine(&bytes);

    let first = run_to_entry(&mut m);
    assert_eq!(
        first,
        StopReason::Matcher(ENTRY_MATCHER),
        "`{id}`: the ROM did not reach its first `entry` line; console: {:?}",
        console(&mut m, SerialStream::UsjTx)
    );
    let at_entry = m.now();
    m.input(
        At::Now,
        InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");
    let from = m.io().serial_ring(SerialStream::UsjTx).head();
    let second = run_to_entry(&mut m);
    assert_eq!(
        second,
        StopReason::Matcher(ENTRY_MATCHER),
        "`{id}`: the ROM did not reach its second `entry` line; console: {:?}",
        console(&mut m, SerialStream::UsjTx)
    );
    assert!(m.now() > at_entry);
    let lines = banner_lines(&mut m, from);
    assert_eq!(
        lines, SECOND_ROM_BANNER,
        "`{id}`: the second banner differs from the device's"
    );
    Some(m)
}

#[test]
fn t1_m1_rom_banner() {
    let test = "t1_m1_rom_banner";
    for (id, file) in E1_IMAGES {
        rom_banner(test, id, file);
    }
}

/// The blocks a first touch may land in from power-on to the second `entry` line: the M1 set, with
/// `spi_mem` as `spi0` and `spi1` and `gpio`/`iomux` as both, `sha` in block mode, and `extmem`
/// and `mmu`, because the ROM loader reads flash through the cache
/// ([`t1_m1_loader_reads_flash_through_the_cache`]).
const STRICT_ROM_BLOCKS: [&str; 16] = [
    "efuse",
    "rtc_cntl",
    "regi2c",
    "system",
    "apb_ctrl",
    "uart0",
    "usj",
    "gpio",
    "iomux",
    "spi0",
    "spi1",
    "flash_xmc",
    "intc",
    "sha",
    "extmem",
    "mmu",
];

/// The registers allowed in blocks outside [`STRICT_ROM_BLOCKS`]: the ROM's own boot path.
///
/// - `assist_debug` +0x044 `RCD_EN` and +0x048 `RCD_PDEBUGPC`: the reset record the `Saved PC`
///   line prints (IDF `soc/esp32c3/register/soc/assist_debug_reg.h`).
/// - `sensitive` +0x01C, read once right after the cache enable.
/// - `uart1` +0x010, written once early in the boot.
const STRICT_ROM_REGISTERS: [(&str, &[u32]); 3] = [
    ("assist_debug", &[0x044, 0x048]),
    ("sensitive", &[0x01C]),
    ("uart1", &[0x010]),
];

fn strict_rom_allows(block: &str, off: u32) -> bool {
    STRICT_ROM_BLOCKS.contains(&block)
        || STRICT_ROM_REGISTERS
            .iter()
            .any(|(b, offs)| *b == block && offs.contains(&off))
}

/// Zero first touches outside the allowed set from power-on to the second `entry` line, for all
/// three images, checked on the first-touch ledger of the `rom-banner` run itself.
#[test]
fn t1_m1_strict_rom_blocks() {
    let test = "t1_m1_strict_rom_blocks";
    for (id, file) in E1_IMAGES {
        let Some(m) = rom_banner(test, id, file) else {
            continue;
        };
        let mut outside: Vec<(&str, u32)> = m
            .ledger()
            .first_touches()
            .iter()
            .map(|t| (block_name(t.periph), t.off))
            .filter(|(block, off)| !strict_rom_allows(block, *off))
            .collect();
        outside.sort_unstable();
        outside.dedup();
        assert!(
            outside.is_empty(),
            "`{id}`: first touches outside the expected set before the second `entry`: {outside:x?}"
        );
    }
}

/// `SPI_MEM_W1` to `SPI_MEM_W15` of SPI1, the data buffer words after the first.
const SPI1_DATA_WORDS: std::ops::RangeInclusive<u32> = 0x05C..=0x094;

/// The rev101 ROM reads the bootloader image through the cache.
///
/// Before the first `load:` line the ROM writes all 128 MMU entries and programs `extmem`. SPI1
/// is not the path: a 20 KB image read through SPI1 would touch the data buffer words, and the
/// ledger holds no touch of any word after `W0`.
#[test]
fn t1_m1_loader_reads_flash_through_the_cache() {
    let test = "t1_m1_loader_reads_flash_through_the_cache";
    let (id, file) = E1_IMAGES[0];
    let Some(m) = rom_banner(test, id, file) else {
        return;
    };
    let touches: Vec<(&str, u32)> = m
        .ledger()
        .first_touches()
        .iter()
        .map(|t| (block_name(t.periph), t.off))
        .collect();
    let mmu_entries = touches.iter().filter(|(b, _)| *b == "mmu").count();
    assert!(
        mmu_entries >= 128,
        "`{id}`: the ROM wrote {mmu_entries} MMU entries"
    );
    assert!(touches.iter().any(|(b, _)| *b == "extmem"));
    let buffer_words: Vec<u32> = touches
        .iter()
        .filter(|(b, off)| *b == "spi1" && SPI1_DATA_WORDS.contains(off))
        .map(|(_, off)| *off)
        .collect();
    assert!(
        buffer_words.is_empty(),
        "`{id}`: SPI1 data buffer words were touched: {buffer_words:x?}"
    );
    assert!(
        m.applied_wiring_by_kind().mmu_entry >= 128,
        "`{id}`: the MMU entry writes were not applied to the cache windows"
    );
}

/// The state compared with the fast-forward on and off: `state_hash` (every non-derived section),
/// plus the instruction count, virtual time and both consoles.
#[derive(Debug, PartialEq, Eq)]
struct GuestState {
    state_hash: [u8; 32],
    insns: u64,
    vt: VTime,
    uart0: String,
    usj: String,
}

fn guest_state(m: &mut Machine) -> GuestState {
    GuestState {
        state_hash: m.state_hash(),
        insns: m.hart().insns,
        vt: m.now(),
        uart0: console(m, SerialStream::Uart0Tx),
        usj: console(m, SerialStream::UsjTx),
    }
}

fn image_machine_ff(flash: &[u8], rom_ff: bool) -> Machine {
    let mut m = image_machine(flash);
    m.set_rom_delay_ff(rom_ff);
    m
}

/// The ROM delay fast-forward on and off give equal state at `entry`.
///
/// The ROM's own boot to `entry` makes one `ets_delay_us(0)` call, so nothing is skipped there;
/// for `pk` the comparison goes on to 100 ms of virtual time, through the bootloader's and the
/// application's first delays, where the shortcut skips instructions.
#[test]
fn t1_m1_rom_delay_fast_forward_on_and_off_agree_at_entry() {
    let test = "t1_m1_rom_delay_fast_forward_on_and_off_agree_at_entry";
    for (id, file) in E1_IMAGES {
        let Some(path) = common::corpus_file_or_skip(test, id, file) else {
            continue;
        };
        let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
        let [on, off] = [true, false].map(|ff| {
            let mut m = image_machine_ff(&bytes, ff);
            assert_eq!(
                m.rom_delay_ff(),
                ff,
                "the bundled rev101 ROM has its loop pinned"
            );
            let mut states = Vec::new();
            let mut skipped = 0;
            let first = m.run(RunLimits {
                until: None,
                max_insns: Some(ENTRY_INSNS),
                stops: entry_stop(),
            });
            assert_eq!(first.reason, StopReason::Matcher(ENTRY_MATCHER), "`{id}`");
            skipped += first.ff_insns;
            states.push(guest_state(&mut m));
            m.input(
                At::Now,
                InputEvent::UsbLine {
                    dtr: false,
                    rts: true,
                },
            )
            .expect("now is not in the past");
            let second = run_to_entry(&mut m);
            assert_eq!(second, StopReason::Matcher(ENTRY_MATCHER), "`{id}`");
            states.push(guest_state(&mut m));
            if id == common::PK {
                let later = m.run(RunLimits {
                    until: Some(VTime::from_ms(100)),
                    max_insns: None,
                    stops: StopSet::default(),
                });
                assert_eq!(later.reason, StopReason::Until, "`{id}`");
                skipped += later.ff_insns;
                states.push(guest_state(&mut m));
            }
            (states, skipped, m.hart().insns)
        });
        println!(
            "rom delay `{id}`: shortcut on: {} instructions, {} skipped; shortcut off: {} instructions, \
             {} skipped",
            on.2, on.1, off.2, off.1
        );
        assert_eq!(on.2, off.2, "`{id}`: the logical instruction count differs");
        assert_eq!(off.1, 0, "`{id}`: the shortcut ran while switched off");
        for (i, (a, b)) in on.0.iter().zip(off.0.iter()).enumerate() {
            assert!(
                a == b,
                "`{id}`: state {i} differs with the ROM delay fast-forward on and off"
            );
        }
        if id == common::PK {
            assert!(on.1 > 0, "`{id}`: no delay was fast-forwarded in 100 ms");
        }
    }
}

fn entry_stop() -> StopSet {
    StopSet {
        matchers: vec![(
            ENTRY_MATCHER,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Prefix("entry 0x".into()),
            },
        )],
        ..StopSet::default()
    }
}

fn variant_machine(flash: &[u8], variant: &Variant) -> Machine {
    let flash = FlashImage::from_merged(flash).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let mut m = Machine::new(variant.config(MachineConfig::default()), assets)
        .expect("the image fits the 8 MB flash");
    variant.apply(&mut m);
    m
}

/// The determinism report at the first `entry` line under `variant`
/// (`pemu_machine::determinism::report`).
fn entry_report(flash: &[u8], variant: &Variant) -> String {
    let mut m = variant_machine(flash, variant);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(ENTRY_INSNS),
        stops: entry_stop(),
    });
    report(&out.reason, &m)
}

/// The block and slice matrix: `max_block_insns` in {1, 3, 64} by slices, plus `ref_step`.
fn determinism_variants() -> Vec<Variant> {
    let mut out = Vec::new();
    for block in [1, 3, 64] {
        for slice in [1, 4_096, 1_000_000] {
            out.push(Variant {
                max_block_insns: block,
                max_slice: slice,
                ..Variant::default()
            });
        }
    }
    out.push(Variant {
        executor: Executor::Reference,
        ..Variant::default()
    });
    out
}

/// For each image, the whole determinism report at the first `entry` line is identical for block
/// sizes {1, 3, 64} crossed with slices {1, 4096, 10^6}, and under `ref_step`.
#[test]
fn t1_m1_determinism_at_entry() {
    let test = "t1_m1_determinism_at_entry";
    for (id, file) in E1_IMAGES {
        let Some(path) = common::corpus_file_or_skip(test, id, file) else {
            continue;
        };
        let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
        let base = entry_report(&bytes, &Variant::default());
        assert!(
            base.starts_with("stop=Matcher(MatcherId(1))"),
            "`{id}` reaches `entry`: {base}"
        );
        for variant in determinism_variants() {
            assert_eq!(
                entry_report(&bytes, &variant),
                base,
                "`{id}`: {} differs from the default at `entry`",
                variant.label()
            );
        }
        println!("RAN {test} {id}");
        // The console must hold the ROM banner, so the comparison is not of empty output. No frame or
        // sample exists before `entry`, so those digests are carried along but not claimed.
        let mut m = variant_machine(&bytes, &Variant::default());
        m.run(RunLimits {
            until: None,
            max_insns: Some(ENTRY_INSNS),
            stops: entry_stop(),
        });
        let usj = console(&mut m, SerialStream::UsjTx);
        let uart0 = console(&mut m, SerialStream::Uart0Tx);
        assert!(
            usj.contains(ROM_BANNER) && uart0.contains(ROM_BANNER),
            "`{id}`: the console compared at `entry` holds the ROM banner on USJ and UART0"
        );
        println!("determinism `{id}`: {base}");
    }
}

/// The same report at `entry` from the wasm32 build under Node, and under the macOS `jsc` shell
/// where present (`determinism.rs`).
#[test]
fn t1_m1_native_equals_node_and_jsc() {
    let test = "t1_m1_native_equals_node_and_jsc";
    for (id, file) in E1_IMAGES {
        let Some(path) = common::corpus_file_or_skip(test, id, file) else {
            continue;
        };
        let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
        let variant = Variant::default();
        let native = entry_report(&bytes, &variant);
        let legs = determinism::wasm_legs(&determinism::WasmBoot {
            image: &path,
            pattern: "entry 0x",
            prefix: true,
            max_insns: ENTRY_INSNS,
            max_block_insns: variant.max_block_insns,
            max_slice: variant.max_slice,
            poll_ff: variant.poll_ff,
        });
        determinism::assert_legs(test, &format!("`{id}` at `entry`"), &native, legs);
    }
}

/// The lines of `text` up to and including the first line that starts with `entry 0x`, without
/// their `\r\n`, or `None` when no such line is there.
fn to_first_entry(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').to_string();
        let end = line.starts_with("entry 0x");
        out.push(line);
        if end {
            return Some(out);
        }
    }
    None
}

/// Compares our lines with the oracle's, index by index, skipping an index whose oracle line a
/// `console` entry of `specs/oracle-known-diffs.toml` excuses. A failure names the stream, the
/// line index and both lengths, never oracle text.
fn assert_lines_equal(what: &str, ours: &[String], theirs: &[String]) {
    let known = oracle::known_diffs();
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{what}: {} lines here, {} in the oracle console",
        ours.len(),
        theirs.len()
    );
    for (at, (a, b)) in ours.iter().zip(theirs).enumerate() {
        if a == b {
            continue;
        }
        if let Some(entry) = known.suppresses_console(oracle::QEMU, b) {
            println!("{what}: line {at} excused by `{}`", entry.id);
            continue;
        }
        panic!(
            "{what}: line {at} differs from the oracle console ({} bytes here, {} there)",
            a.len(),
            b.len()
        );
    }
}

/// The power-on banner of `pk` shows `rst:0x1` and `boot:0xa`, and its text equals the oracle's
/// USJ console up to its first `entry 0x` line, where the power-on banner ends. The UART0 console
/// (the ROM banner alone) is compared whole.
#[test]
fn t2_m1_power_on_banner_equals_oracle() {
    let test = "t2_m1_power_on_banner_equals_oracle";
    let Some(usj_path) = oracle::oracle_file_or_skip(test, "pk.usj.console") else {
        return;
    };
    let Some(uart_path) = oracle::oracle_file_or_skip(test, "pk.console") else {
        return;
    };
    let Some(path) = common::corpus_file_or_skip(test, common::PK, E1_IMAGES[0].1) else {
        return;
    };
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let mut m = image_machine(&bytes);
    assert_eq!(
        run_to_entry(&mut m),
        StopReason::Matcher(ENTRY_MATCHER),
        "`pk` reaches its first `entry` line"
    );
    let usj = to_first_entry(&console(&mut m, SerialStream::UsjTx))
        .expect("the USJ console holds the `entry` line the run stopped on");
    let uart0 = to_first_entry(&console(&mut m, SerialStream::Uart0Tx))
        .expect("UART0 carries the same ROM banner");
    let rst = usj
        .iter()
        .find(|l| l.starts_with("rst:"))
        .expect("the power-on banner has a reset line");
    assert!(
        rst.starts_with("rst:0x1 ") && rst.contains("boot:0xa "),
        "the power-on banner shows rst:0x1 and boot:0xa: {rst:?}"
    );

    let oracle_text = |p: &std::path::Path| {
        String::from_utf8_lossy(&std::fs::read(p).expect("the oracle console is readable"))
            .into_owned()
    };
    let theirs_usj = to_first_entry(&oracle_text(&usj_path))
        .expect("the oracle USJ console holds an `entry 0x` line");
    let theirs_uart0 = to_first_entry(&oracle_text(&uart_path))
        .expect("the oracle UART0 console holds an `entry 0x` line");
    assert_lines_equal("USJ", &usj, &theirs_usj);
    assert_lines_equal("UART0", &uart0, &theirs_uart0);
    println!(
        "`pk`: power-on banner of {} USJ and {} UART0 lines equals the oracle",
        usj.len(),
        uart0.len()
    );
}

/// The blocks the ROM-phase write-stream diff covers. USJ needs the oracle binary with the USJ
/// patch; a stock binary would drop it (`usj.stock-stub`).
const ROM_PHASE_BLOCKS: [&str; 4] = ["efuse", "spi1", "rtc_cntl", "usj"];

/// Over the ROM phase of `pk` (power-on to the first `entry 0x`), the per-block write-stream LCS
/// diff against QEMU has zero unexplained divergences for eFuse, SPI1, RTC_CNTL and USJ.
#[test]
fn t2_m1_rom_write_streams_match_oracle() {
    let test = "t2_m1_rom_write_streams_match_oracle";
    let Some(trace_path) = oracle::oracle_file_or_skip(test, "pk.trace") else {
        return;
    };
    let Some(path) = common::corpus_file_or_skip(test, common::PK, E1_IMAGES[0].1) else {
        return;
    };
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let cfg = MachineConfig {
        trace: oracle::access_trace(),
        ..MachineConfig::default()
    };
    let mut m = Machine::new(cfg, assets).expect("the image fits the 8 MB flash");
    let (stop, writes) = oracle::run_collecting_writes(
        &mut m,
        RunLimits {
            until: None,
            max_insns: Some(ENTRY_INSNS),
            stops: entry_stop(),
        },
    );
    assert_eq!(
        stop,
        StopReason::Matcher(ENTRY_MATCHER),
        "`pk` reaches `entry`"
    );

    let map = oracle::regions();
    let (ours, unmapped) = oracle::our_streams(&map, &writes, None, "entry 0x")
        .expect("our USJ write stream spells the `entry 0x` line");
    let trace = std::fs::read_to_string(&trace_path).expect("the oracle trace is readable");
    let theirs = oracle::oracle_streams(&map, &trace, None, "entry 0x")
        .expect("the oracle USJ write stream spells the `entry 0x` line");
    println!(
        "`pk` ROM phase: {} accesses, {unmapped} writes outside every block",
        writes.len()
    );
    let failures = oracle::diff_blocks("`pk` ROM phase", &ROM_PHASE_BLOCKS, &ours, &theirs);
    assert!(
        failures.is_empty(),
        "ROM phase: unexplained divergences:\n{failures}"
    );

    // The ROM phase programs no eFuse, so the write diff compares nothing for it; the eFuse leg is
    // the sequence of offsets both runs read. Values are not compared: the two eFuse images are
    // synthesized separately.
    assert!(
        ours.get("efuse").is_none_or(Vec::is_empty)
            && theirs.get("efuse").is_none_or(Vec::is_empty),
        "the ROM phase now writes the eFuse, so the write diff covers it"
    );
    let our_reads = oracle::our_read_offsets(&map, &writes, "efuse", None, "entry 0x")
        .expect("the span resolves on our side");
    let their_reads = oracle::oracle_read_offsets(&map, &trace, "efuse", None, "entry 0x")
        .expect("the span resolves on the oracle side");
    assert!(!our_reads.is_empty(), "the ROM phase reads the eFuse");
    let first = our_reads.iter().zip(&their_reads).position(|(a, b)| a != b);
    assert!(
        first.is_none() && our_reads.len() == their_reads.len(),
        "ROM phase efuse: the read-offset sequences differ ({} reads here, {} there; first at {:?}: \
         {:?} against {:?})",
        our_reads.len(),
        their_reads.len(),
        first,
        first.map(|i| our_reads[i]),
        first.map(|i| their_reads[i])
    );
    println!(
        "`pk` ROM phase: block efuse: 0 writes on both sides; {} read offsets equal the oracle's",
        our_reads.len()
    );
}
