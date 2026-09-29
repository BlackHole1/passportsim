//! `Machine::snapshot` and `Machine::restore`, over `pemu_core::snap`. A snapshot is taken
//! between two `run` calls, an exact instruction boundary; derived state is rebuilt on restore,
//! never saved. `state_hash.rs` says which sections the hash leaves out; export redaction is in
//! `snapshot/redact.rs`. Every section is postcard, version 1:
//!
//! | Section | Content |
//! |---|---|
//! | `hart` | integer registers, pc, every CSR, `wfi`, `insns`, `stores`, the stack monitor mirror |
//! | `ram`, `rtc_ram` | SRAM0 then SRAM1; RTC FAST memory |
//! | `flash_delta` | the guest's flash writes over the base image |
//! | `soc.<block>` | one per `c3_devices!` row: the model's serde form |
//! | `soc.bus` | the performance-counter pair, the stack monitor, the PMS split, the cache account |
//! | `soc.irq_fabric` | the interrupt fabric without its derived routing table and cache |
//! | `soc.flash_cache` | which flash pages the cache windows hold current, and the stale ones' bytes |
//! | `board.<chip>` | one per board chip, and `board.bus` for the addressed I2C device |
//! | `sched`, `rng`, `clock` | the `pemu_core` serde forms |
//! | `journal_cursor`, `journal_pending` | the journal, split as `pemu_core::snap::split_journal` does |
//! | `hostio` | undrained host-to-guest data, `usj_ctrl`, the line index and read cursors |
//! | `ledger` | first touches and fidelity notes |
//! | `machine` | whether the MCU rail is up, and the number of resets sequenced |
//! | `host` | run bookkeeping that is not guest state (receipt cursor, counters, ring heads) |
//! | `soc.disabled` | the register store of every disabled model, and its dropped events |
//! | `frame` | the `FramePort` generation and dirty span; a restore leaves the whole screen dirty |
//! | `hang` | the poll tracker and the hang detector's clocks, restored after hart, INTC and clock |
//! | `hle`, `hle.machine` | `pemu_hle::continuation::HleSection` (refused when bound against another app ELF), and `HleMachineSection` |

use std::sync::Arc;

use pemu_core::clock::Clock;
use pemu_core::fidelity::FidelityLedger;
use pemu_core::journal::Journal;
use pemu_core::rng::DetRng;
use pemu_core::sched::Scheduler;
use pemu_core::serde::de::DeserializeOwned;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::snap::{
    EfuseKind, FORMAT_VERSION, HostIoSection, JournalCursor, JournalPending, LivePolicy, Section,
    SectionId, SnapError, SnapHeader, SnapOpts, Snapshot, join_journal, serde_from_section,
    serde_section, split_journal,
};
use pemu_rv32::bus::PageTable;
use pemu_rv32::csr::{Csr, TRIGGERS};
use pemu_rv32::exec::Hart;
use pemu_rv32::spmon::SpMonitor;
use pemu_soc_c3::cold::CacheState;
use pemu_soc_c3::flash_store::{self, DeltaPage, FlashDelta, FlashStore};
use pemu_soc_c3::intc::{IrqFabric, IrqFabricState};
use pemu_soc_c3::mem;
use pemu_soc_c3::periph::{BLOCKS, DeviceVisitor, Devices, Peripheral};
use pemu_soc_c3::wiring::mmu as mmu_wiring;
use pemu_soc_c3::wiring::protection::Pms;

use pemu_soc_c3::periph::rtc_sleep::SleepKind;

use crate::machine::{Machine, MachineApi};
use crate::stops::{StopReason, WatchdogFire};
use crate::wiring_counts::WiringCounts;

mod redact;

pub use redact::{CARDID_WINDOW, Erased, FlashView, Redaction, SecretSources};

/// Version of every section here; a bump needs a migration or `SnapError::Incompatible`.
pub const SECTION_VERSION: u16 = 1;

pub const SOC_BUS: &str = "soc.bus";
pub const SOC_IRQ_FABRIC: &str = "soc.irq_fabric";
pub const SOC_DISABLED: &str = "soc.disabled";
pub const SOC_FLASH_CACHE: &str = "soc.flash_cache";
pub const BOARD_BUS: &str = "board.bus";
pub const MACHINE: &str = "machine";
/// Bookkeeping `state_hash` leaves out.
pub const HOST: &str = "host";

/// The board chips, as `board.<chip>` names.
pub const BOARD_CHIPS: [&str; 9] = [
    "lcd",
    "backlight",
    "codec",
    "gauge",
    "battery",
    "ladder",
    "power",
    "usb",
    "world",
];

/// The `hart` section, field by field, because `pemu-rv32` keeps serde off its hart.
#[derive(Clone, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HartSection {
    pub x: [u32; 32],
    pub pc: u32,
    /// `mstatus`, `mtvec`, `mepc`, `mcause`, `mtval`, `mscratch`.
    pub trap: [u32; 6],
    pub pmpcfg: [u8; 16],
    pub pmpaddr: [u32; 16],
    /// `tselect`, `tcontrol`, `mpcer`, `mpcmr`, CSR 0x000.
    pub misc: [u32; 5],
    pub tdata1: [u32; TRIGGERS],
    pub tdata2: [u32; TRIGGERS],
    pub wfi: bool,
    pub insns: u64,
    pub stores: u64,
    pub spmon: SpMonitor,
    /// Class-cost cycles beyond one per instruction; with `insns`, keys the `clock` section.
    pub extra: u64,
    /// `Hart::pipe` as `Pipe::to_byte`.
    pub pipe: u8,
    /// `Hart::pipe.bank` as `Bank::to_bits`.
    pub bank: u64,
}

/// The `soc.bus` section: SoC state outside the models.
#[derive(Clone, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct SocBusSection {
    pub pcer: u32,
    pub pcmr: u32,
    pub sp_monitor: SpMonitor,
    /// The PMS split offset inside SRAM1, `None` while the monitors are off.
    pub pms_split: Option<u32>,
    /// The flash-cache account (`pemu_soc_c3::cold::CacheState`): the guest cannot read it, but
    /// the next fill's timing depends on it.
    pub cache: CacheState,
}

/// The `soc.flash_cache` section. A current page holds exactly the store's bytes, so only the mark
/// is saved. A stale page (flash written, no cache flush yet) is what the guest still reads and
/// cannot be rebuilt from the store, so its bytes are saved unless all zero.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct FlashCacheSection {
    pub current: Vec<u64>,
    pub stale: Vec<DeltaPage>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct MachineSection {
    pub mcu_powered: bool,
    pub resets: u64,
    /// The sleep stop owed to the run loop (`Some(0)` light, `Some(1)` deep); `None` between runs.
    pub sleep_stop: Option<u8>,
}

/// The `frame` section: how far the host-visible frame stream got. The pixels are not carried: a
/// restore repaints them from the panel memory.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct FrameSection {
    pub generation: u64,
    pub dirty: Option<(u16, u16)>,
}

/// The `host` section: what a host did to this machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HostSection {
    pub receipt_cursor: u64,
    pub idle_ps_at_run_start: u64,
    pub resume_breakpoint: Option<u32>,
    /// Absolute heads of `usj_tx`, `uart0_tx` and `events`, so host cursors keep their meaning.
    pub output_heads: [u64; 3],
    pub audio_out_heads: [u64; 2],
    /// Unapplied inputs, pending board effects, pending board events, undispatched events, peak
    /// pending wiring, unrecorded spills, unpowered events, DMA faults, PCM width faults.
    pub counters: [u64; 9],
    pub applied_wiring: WiringCounts,
    pub unapplied_wiring: WiringCounts,
}

/// The snapshot half of the facade, for a host holding a machine as a trait object.
pub trait SnapshotMachine: MachineApi {
    /// Take a snapshot. A backend that cannot snapshot refuses.
    fn snapshot(&self, opts: SnapOpts) -> Result<Snapshot, SnapError>;
    fn snapshot_header(&self, opts: SnapOpts) -> Result<SnapHeader, SnapError> {
        self.snapshot(opts).map(|snapshot| snapshot.header)
    }
    /// Apply the export redaction to an earlier snapshot of this machine. Returns what was
    /// erased, so the caller holding the export salt can label the cardid window.
    fn redact(&self, snapshot: &mut Snapshot) -> Result<Redaction, SnapError>;
    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapError>;
    fn fork(&self, live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError>;
    fn state_hash(&self) -> [u8; 32];
    /// A counter that moves whenever [`SnapshotMachine::secret_sources`] may answer differently,
    /// so a caller rebuilds the `SecretSet` only then.
    fn secret_generation(&self) -> u64 {
        0
    }
    fn secret_sources(&self, view: FlashView) -> SecretSources {
        let _ = view;
        SecretSources::default()
    }
    /// Whether an interrupt could still wake a WFI hart, so a `Deadlock` report can tell "an input
    /// may wake it" from "only a reset does". `None` when the backend does not know.
    fn interrupt_can_wake(&self) -> Option<bool> {
        None
    }
    /// Which watchdog last drove its interrupt stage, and when, so a fault envelope classifies a
    /// watchdog from the TIMG stage that fired rather than from console text.
    fn watchdog_fired(&self) -> Option<WatchdogFire> {
        None
    }
}

impl SnapshotMachine for Machine {
    fn snapshot(&self, opts: SnapOpts) -> Result<Snapshot, SnapError> {
        Ok(Machine::snapshot(self, opts))
    }

    fn snapshot_header(&self, opts: SnapOpts) -> Result<SnapHeader, SnapError> {
        Ok(Machine::snapshot_header(self, opts))
    }

    fn redact(&self, snapshot: &mut Snapshot) -> Result<Redaction, SnapError> {
        Machine::redact(self, snapshot)
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapError> {
        Machine::restore(self, snapshot)
    }

    fn fork(&self, live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        Ok(Box::new(Machine::fork(self, live)?))
    }

    fn state_hash(&self) -> [u8; 32] {
        Machine::state_hash(self)
    }

    fn secret_generation(&self) -> u64 {
        Machine::secret_generation(self)
    }

    fn secret_sources(&self, view: FlashView) -> SecretSources {
        Machine::secret_sources(self, view)
    }

    fn interrupt_can_wake(&self) -> Option<bool> {
        Some(Machine::interrupt_can_wake(self))
    }

    fn watchdog_fired(&self) -> Option<WatchdogFire> {
        Machine::watchdog_fired(self)
    }
}

/// Calls `$m!` with every field of `pemu_soc_c3::periph::Devices`, in `c3_devices!` table order.
/// `Devices::visit_all` needs `&mut` and `snapshot` takes `&self`, so the encoder names the fields
/// itself; `tests::the_block_list_is_the_c3_devices_table` keeps this list equal to `BLOCKS`.
macro_rules! with_blocks {
    ($m:ident, $($arg:tt)*) => {
        $m!($($arg)*; uart0, spi1, spi0, gpio, radio_fe2, radio_fe, rtc_cntl, efuse, iomux, regi2c,
            uart1, i2c0, uhci0, rmt, ledc, radio_nrx, radio_bb, timg0, timg1, systimer, spi2,
            apb_ctrl, twai, i2s0, radio_ble, aes, sha, rsa, ds, hmac, gdma, saradc, usj, system,
            sensitive, intc, extmem, mmu, xts_aes, assist_debug, dedicated_gpio, world_cntl)
    };
}

macro_rules! encode_blocks {
    ($devices:expr; $($field:ident),*) => {
        vec![$( (stringify!($field), encode(&$devices.$field)) ),*]
    };
}

#[cfg(test)]
macro_rules! block_names {
    (; $($field:ident),*) => {
        vec![$( stringify!($field) ),*]
    };
}

/// `value` as a postcard section at [`SECTION_VERSION`]. Every type here is plain serde data, so
/// a failure is a defect in that type, not a machine condition.
fn encode<T: Serialize>(value: &T) -> Section {
    serde_section(value, SECTION_VERSION, "machine section")
        .expect("machine state always encodes as postcard")
}

/// The value of section `name`, refusing a missing section, another version or codec, bytes
/// that are not the value, and bytes left over.
fn decode<T: DeserializeOwned>(
    snap: &Snapshot,
    name: &str,
    at: &'static str,
) -> Result<T, SnapError> {
    let id = SectionId::new(name);
    let section = snap.section(&id)?;
    decode_section(section, id, at)
}

fn decode_section<T: DeserializeOwned>(
    section: &Section,
    id: SectionId,
    at: &'static str,
) -> Result<T, SnapError> {
    serde_from_section(section, id, SECTION_VERSION, at)
}

fn arena_slice(m: &Machine, at: u32, len: u32) -> Vec<u8> {
    m.soc.arena.bytes()[at as usize..(at + len) as usize].to_vec()
}

fn current_pages(flash: &FlashStore) -> Vec<u64> {
    let mut bits = vec![0u64; (flash_store::PAGES as usize).div_ceil(64)];
    for page in 0..flash_store::PAGES {
        if flash.mirror_is_current(page) {
            bits[(page / 64) as usize] |= 1 << (page % 64);
        }
    }
    bits
}

impl Machine {
    /// Take a snapshot. An export is stamped `exported`, and without `include_secrets` it is also
    /// [`Machine::redact`]ed; the command layer runs byte-match redaction over what remains.
    pub fn snapshot(&self, opts: SnapOpts) -> Snapshot {
        let mut snap = Snapshot::new(self.snapshot_header(SnapOpts::default()));
        for (id, section) in self.sections() {
            snap.put_raw(id, section);
        }
        if opts.export && !opts.include_secrets {
            self.redact(&mut snap)
                .expect("the sections this machine just encoded decode");
        }
        snap.header.exported = opts.export;
        snap
    }

    /// The snapshot header. `image_sha256` covers the app ELF too, since the HLE binding is
    /// derived from it; the eFuse words are never in it.
    pub fn snapshot_header(&self, opts: SnapOpts) -> SnapHeader {
        let ids = self.assets.identity();
        SnapHeader {
            format: FORMAT_VERSION,
            rom_sha256: ids.rom_sha256,
            image_sha256: std::iter::once(ids.image_sha256)
                .chain(ids.app_elf_sha256)
                .collect(),
            config_hash: self.cfg.identity_hash(),
            efuse_kind: if self.is_tainted() {
                EfuseKind::Imported
            } else {
                EfuseKind::Synthesized
            },
            efuse_hash: if opts.export && !opts.include_secrets {
                [0; 32]
            } else {
                ids.efuse_sha256
            },
            exported: opts.export,
            redacted: opts.export && !opts.include_secrets,
            ..SnapHeader::new()
        }
    }

    pub(crate) fn sections(&self) -> Vec<(SectionId, Section)> {
        let mut out: Vec<(SectionId, Section)> = Vec::new();
        let mut put = |name: &str, section: Section| out.push((SectionId::new(name), section));

        // No `..`, so a field added to `Hart` or `Csr` fails to compile here instead of silently
        // staying out of every snapshot.
        let Hart {
            x,
            pc,
            csr,
            wfi,
            insns,
            stores,
            spmon,
            extra,
            pipe,
        } = &self.hart;
        let Csr {
            mstatus,
            mtvec,
            mepc,
            mcause,
            mtval,
            mscratch,
            pmpcfg,
            pmpaddr,
            tselect,
            tdata1,
            tdata2,
            tcontrol,
            mpcer,
            mpcmr,
            csr000,
        } = csr;
        put(
            SectionId::HART,
            encode(&HartSection {
                x: *x,
                pc: *pc,
                trap: [*mstatus, *mtvec, *mepc, *mcause, *mtval, *mscratch],
                pmpcfg: *pmpcfg,
                pmpaddr: *pmpaddr,
                misc: [*tselect, *tcontrol, *mpcer, *mpcmr, *csr000],
                tdata1: *tdata1,
                tdata2: *tdata2,
                wfi: *wfi,
                insns: *insns,
                stores: *stores,
                spmon: *spmon,
                extra: *extra,
                pipe: pipe.to_byte(),
                bank: pipe.bank.to_bits(),
            }),
        );
        let mut ram = arena_slice(self, mem::SRAM0_ARENA, mem::SRAM0_LEN);
        ram.extend_from_slice(&arena_slice(self, mem::SRAM1_ARENA, mem::SRAM1_LEN));
        put(SectionId::RAM, encode(&ram));
        put(
            SectionId::RTC_RAM,
            encode(&arena_slice(self, mem::RTC_FAST_ARENA, mem::RTC_FAST_LEN)),
        );
        put(SectionId::FLASH_DELTA, encode(&self.soc.flash.delta()));

        let blocks: Vec<(&'static str, Section)> = with_blocks!(encode_blocks, self.soc.devices);
        for (name, section) in blocks {
            put(&format!("{}{name}", SectionId::SOC_PREFIX), section);
        }
        let (pcer, pcmr) = self.soc.counter_registers();
        put(
            SOC_BUS,
            encode(&SocBusSection {
                pcer,
                pcmr,
                sp_monitor: self.soc.sp_monitor(),
                pms_split: self.soc.pms.split_offset(),
                cache: self.soc.cache.state(),
            }),
        );
        put(SOC_IRQ_FABRIC, encode(&self.irq.state()));
        put(SOC_FLASH_CACHE, encode(&self.flash_cache()));
        put(SOC_DISABLED, encode(&self.disabled.section()));

        let b = &self.board;
        let chips = [
            encode(&b.lcd),
            encode(&b.backlight),
            encode(&b.codec),
            encode(&b.gauge),
            encode(&b.battery),
            encode(&b.ladder),
            encode(&b.power),
            encode(&b.usb),
            encode(&b.world),
        ];
        for (chip, section) in BOARD_CHIPS.iter().zip(chips) {
            put(&format!("{}{chip}", SectionId::BOARD_PREFIX), section);
        }
        put(BOARD_BUS, encode(&b.i2c_target()));

        put(SectionId::SCHED, encode(&self.sched));
        put(SectionId::RNG, encode(&self.rng));
        put(SectionId::CLOCK, encode(&self.clock));
        let (cursor, pending) = split_journal(self.journal.save());
        put(SectionId::JOURNAL_CURSOR, encode(&cursor));
        put(SectionId::JOURNAL_PENDING, encode(&pending));
        put(SectionId::HOSTIO, encode(&HostIoSection::capture(&self.io)));
        put(SectionId::LEDGER, encode(&self.ledger));
        put(
            SectionId::FRAME,
            encode(&FrameSection {
                generation: self.io.frame.generation(),
                dirty: self.io.frame.dirty_rows(),
            }),
        );
        put(
            SectionId::HANG,
            pemu_core::snap::SnapSection::encode(&self.hang_section())
                .expect("the hang section always encodes"),
        );
        put(
            SectionId::HLE,
            pemu_core::snap::SnapSection::encode(&self.hle.core.section)
                .expect("the hle section always encodes"),
        );
        put(
            crate::hle::HLE_MACHINE,
            encode(&crate::hle::HleMachineSection {
                radio_used: self.hle.radio.used(),
                ..self.hle.state.clone()
            }),
        );
        put(
            MACHINE,
            encode(&MachineSection {
                mcu_powered: self.mcu_powered,
                resets: self.resets,
                sleep_stop: match self.wiring_stop {
                    Some(StopReason::Sleep(SleepKind::Light)) => Some(0),
                    Some(StopReason::Sleep(SleepKind::Deep)) => Some(1),
                    _ => None,
                },
            }),
        );
        put(
            HOST,
            encode(&HostSection {
                receipt_cursor: self.receipt_cursor,
                idle_ps_at_run_start: self.idle_ps_at_run_start,
                resume_breakpoint: self.resume_breakpoint,
                output_heads: [
                    self.io.usj_tx.head(),
                    self.io.uart0_tx.head(),
                    self.io.events.head(),
                ],
                audio_out_heads: [self.io.audio_out.head(), self.io.audio_out.record_head()],
                counters: [
                    self.unapplied_inputs,
                    self.pending_board_effects,
                    self.pending_board_events,
                    self.undispatched_events,
                    self.peak_pending_wiring,
                    self.unrecorded_spills,
                    self.unpowered_events,
                    self.dma_faults,
                    self.pcm_width_faults,
                ],
                applied_wiring: self.applied_wiring,
                unapplied_wiring: self.unapplied_wiring,
            }),
        );
        out
    }

    fn flash_cache(&self) -> FlashCacheSection {
        let flash = &self.soc.flash;
        let current = current_pages(flash);
        let bytes = self.soc.arena.bytes();
        let stale = (0..flash_store::PAGES)
            .filter(|page| !flash.mirror_is_current(*page))
            .filter_map(|page| {
                let at = (mem::FLASH_ARENA + page * flash_store::PAGE_LEN) as usize;
                let window = &bytes[at..at + flash_store::PAGE_LEN as usize];
                window.iter().any(|b| *b != 0).then(|| DeltaPage {
                    page,
                    bytes: window.to_vec(),
                })
            })
            .collect();
        FlashCacheSection { current, stale }
    }
}

/// Every `soc.<block>` section decodes into a fresh `Devices`, which replaces the machine's only
/// once the whole snapshot decoded, so a refused snapshot leaves the machine alone.
struct DecodeBlocks<'a> {
    snap: &'a Snapshot,
    err: Option<SnapError>,
}

impl DeviceVisitor for DecodeBlocks<'_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        if self.err.is_some() {
            return;
        }
        let name = BLOCKS[usize::from(P::ID.0)].name;
        let id = SectionId::soc(name);
        match self.snap.sections.get(&id) {
            None => self.err = Some(SnapError::MissingSection(id)),
            Some(section) => match decode_section(section, id, name) {
                Ok(model) => *dev = model,
                Err(e) => self.err = Some(e),
            },
        }
    }
}

/// Every section name this build applies; an unknown required section is refused.
fn known_section(id: &SectionId) -> bool {
    let name = id.as_str();
    if let Some(block) = name.strip_prefix(SectionId::SOC_PREFIX) {
        return BLOCKS.iter().any(|b| b.name == block)
            || matches!(
                name,
                SOC_BUS | SOC_IRQ_FABRIC | SOC_FLASH_CACHE | SOC_DISABLED
            );
    }
    if let Some(chip) = name.strip_prefix(SectionId::BOARD_PREFIX) {
        return BOARD_CHIPS.contains(&chip) || name == BOARD_BUS;
    }
    matches!(
        name,
        SectionId::HART
            | SectionId::RAM
            | SectionId::RTC_RAM
            | SectionId::FLASH_DELTA
            | SectionId::SCHED
            | SectionId::RNG
            | SectionId::CLOCK
            | SectionId::JOURNAL_CURSOR
            | SectionId::JOURNAL_PENDING
            | SectionId::HOSTIO
            | SectionId::LEDGER
            | SectionId::HANG
            | SectionId::FRAME
            | SectionId::HLE
            | crate::hle::HLE_MACHINE
            | MACHINE
            | HOST
    )
}

struct BoardSections {
    lcd: pemu_board::st7789::St7789p3,
    backlight: pemu_board::backlight::Backlight,
    codec: pemu_board::es8311::Es8311,
    gauge: pemu_board::cw2017::Cw2017,
    battery: pemu_board::battery::Battery,
    ladder: pemu_board::ladder::ButtonLadder,
    power: pemu_board::power::PowerRail,
    usb: pemu_board::usb_plug::UsbPlug,
    world: pemu_board::ntag213::BoardWorld,
    i2c_target: Option<u8>,
}

impl BoardSections {
    fn decode(snap: &Snapshot) -> Result<BoardSections, SnapError> {
        let chip = |i: usize| format!("{}{}", SectionId::BOARD_PREFIX, BOARD_CHIPS[i]);
        Ok(BoardSections {
            lcd: decode(snap, &chip(0), "board.lcd")?,
            backlight: decode(snap, &chip(1), "board.backlight")?,
            codec: decode(snap, &chip(2), "board.codec")?,
            gauge: decode(snap, &chip(3), "board.gauge")?,
            battery: decode(snap, &chip(4), "board.battery")?,
            ladder: decode(snap, &chip(5), "board.ladder")?,
            power: decode(snap, &chip(6), "board.power")?,
            usb: decode(snap, &chip(7), "board.usb")?,
            world: decode(snap, &chip(8), "board.world")?,
            i2c_target: decode(snap, BOARD_BUS, "board.bus")?,
        })
    }
}

fn unusable(at: &'static str, reason: &'static str) -> SnapError {
    SnapError::Malformed { at, reason }
}

/// Where a guest-to-host ring from `tail` to `now` goes when rewound to a snapshot's `head`: it
/// keeps what it holds before `head`, or nothing with the tail at `head` when `head` is out of its
/// window. `window` lists the whole window from `tail`.
fn rewound<T>(tail: u64, now: u64, head: u64, window: impl FnOnce() -> Vec<T>) -> (u64, Vec<T>) {
    if head > now || head <= tail {
        return (head, Vec::new());
    }
    let mut kept = window();
    kept.truncate((head - tail) as usize);
    (tail, kept)
}

impl Machine {
    /// Restore a snapshot; any refusal leaves the machine exactly as it was. A live bridge is
    /// refused because its peer is in no snapshot. A redacted snapshot's eFuse hash is not
    /// checked: this machine's own eFuse image is used instead.
    pub fn restore(&mut self, s: &Snapshot) -> Result<(), SnapError> {
        self.refuse_live_bridge()?;
        if s.header.format != FORMAT_VERSION {
            return Err(SnapError::UnsupportedFormat {
                found: s.header.format,
                supported: FORMAT_VERSION,
            });
        }
        let own = self.snapshot_header(SnapOpts::default());
        if let Some(field) = own.identity_mismatch(&s.header, !s.header.redacted) {
            return Err(SnapError::IdentityMismatch { field });
        }
        s.plan_restore(|id| id.fate(known_section(id)))?;

        let hart: HartSection = decode(s, SectionId::HART, "hart")?;
        let ram: Vec<u8> = decode(s, SectionId::RAM, "ram")?;
        let rtc_ram: Vec<u8> = decode(s, SectionId::RTC_RAM, "rtc_ram")?;
        if ram.len() != (mem::SRAM0_LEN + mem::SRAM1_LEN) as usize
            || rtc_ram.len() != mem::RTC_FAST_LEN as usize
        {
            return Err(unusable("ram", "the memory size is not this chip's"));
        }
        let delta: FlashDelta = decode(s, SectionId::FLASH_DELTA, "flash_delta")?;
        let mut flash = FlashStore::new(Arc::clone(self.soc.flash.image()))
            .map_err(|_| unusable("flash_delta", "the base image is not a flash image"))?;
        flash
            .apply_delta(&delta)
            .map_err(|_| unusable("flash_delta", "the delta does not fit the flash"))?;
        let cache: FlashCacheSection = decode(s, SOC_FLASH_CACHE, "soc.flash_cache")?;
        if cache.current.len() != (flash_store::PAGES as usize).div_ceil(64)
            || cache.stale.iter().any(|p| {
                p.page >= flash_store::PAGES || p.bytes.len() != flash_store::PAGE_LEN as usize
            })
        {
            return Err(unusable("soc.flash_cache", "a page is not a flash page"));
        }
        let bus: SocBusSection = decode(s, SOC_BUS, "soc.bus")?;
        let pms = match bus.pms_split {
            None => Pms::OPEN,
            Some(off) => Pms::split_at(mem::SRAM1_IRAM_BASE.wrapping_add(off))
                .ok_or(unusable("soc.bus", "the PMS split is outside SRAM1"))?,
        };
        let irq_state: IrqFabricState = decode(s, SOC_IRQ_FABRIC, "soc.irq_fabric")?;
        let irq = IrqFabric::from_state(&irq_state).ok_or(unusable(
            "soc.irq_fabric",
            "a routing or priority value is out of range",
        ))?;
        let mut devices = Box::<Devices>::default();
        let mut blocks = DecodeBlocks { snap: s, err: None };
        devices.visit_all(&mut blocks);
        if let Some(e) = blocks.err {
            return Err(e);
        }
        let board = BoardSections::decode(s)?;
        let sched: Scheduler = s.get()?;
        let rng: DetRng = s.get()?;
        let clock: Clock = s.get()?;
        let cursor: JournalCursor = s.get()?;
        let pending: JournalPending = s.get()?;
        let journal = Journal::restore(join_journal(cursor, pending))
            .map_err(|_| unusable("journal_pending", "the entries are not in (at, seq) order"))?;
        let hostio: HostIoSection = s.get()?;
        let ledger: FidelityLedger = s.get()?;
        let machine: MachineSection = decode(s, MACHINE, "machine")?;
        let sleep_stop = match machine.sleep_stop {
            None => None,
            Some(0) => Some(StopReason::Sleep(SleepKind::Light)),
            Some(1) => Some(StopReason::Sleep(SleepKind::Deep)),
            Some(_) => return Err(unusable("machine", "the sleep stop is not a sleep kind")),
        };
        let host: HostSection = decode(s, HOST, "host")?;
        let hang: crate::poll_ff::HangSection = s.get()?;
        let frame: FrameSection = decode(s, SectionId::FRAME, SectionId::FRAME)?;
        let shape: crate::disable::DisabledShape = decode(s, SOC_DISABLED, "soc.disabled")?;
        if !self.disabled.fits(&shape) {
            return Err(unusable(
                "soc.disabled",
                "the stores are not this configuration's disabled models",
            ));
        }
        let disabled: crate::disable::DisabledSection = decode(s, SOC_DISABLED, "soc.disabled")?;
        let hle: pemu_hle::continuation::HleSection = s.get()?;
        // The hooks are recomputed from this machine's app ELF.
        hle.check_app_elf(&self.hle.core.section.binding.app_elf_sha256)?;
        let hle_state: crate::hle::HleMachineSection =
            decode(s, crate::hle::HLE_MACHINE, crate::hle::HLE_MACHINE)?;

        // The one fallible write, all or nothing on its own.
        hostio
            .restore_into(&mut self.io)
            .map_err(|_| unusable("hostio", "a ring window does not fit this machine's rings"))?;

        // From here nothing can fail.
        self.soc.devices = *devices;
        if s.header.redacted {
            self.rederive_efuse();
        }
        self.apply_hart(&hart);
        let bytes = self.soc.arena.bytes_mut();
        let sram0 = mem::SRAM0_ARENA as usize;
        let sram1 = mem::SRAM1_ARENA as usize;
        let rtc = mem::RTC_FAST_ARENA as usize;
        let (ram0, ram1) = ram.split_at(mem::SRAM0_LEN as usize);
        bytes[sram0..sram0 + ram0.len()].copy_from_slice(ram0);
        bytes[sram1..sram1 + ram1.len()].copy_from_slice(ram1);
        bytes[rtc..rtc + rtc_ram.len()].copy_from_slice(&rtc_ram);
        flash.succeed(&self.soc.flash);
        self.soc.flash = flash;
        self.soc.set_counter_registers(bus.pcer, bus.pcmr);
        self.soc.set_sp_monitor(bus.sp_monitor);
        self.soc.pms = pms;
        self.irq = irq;
        self.apply_board(board);
        self.sched = sched;
        self.rng = rng;
        self.clock = clock;
        self.journal = journal;
        self.ledger = ledger;
        self.mcu_powered = machine.mcu_powered;
        self.resets = machine.resets;
        self.wiring_stop = sleep_stop;
        self.disabled.restore(disabled);
        self.hle.core.section = hle;
        self.hle.restore_radio_used(hle_state.radio_used);
        self.hle.state = hle_state;
        self.rebuild_hooks();
        self.board.lcd.mark_all_dirty();
        self.publish_frame();
        self.io
            .frame
            .restore_progress(frame.generation, frame.dirty);
        self.apply_host(&host);
        self.rebuild_derived(&cache);
        // MMIO costs are APB cycles, charged at the restored clocks' ratio.
        self.apply_insn_costs();
        // After the rebuild, whose `apply_all` marks the DROM pages the account wants.
        self.soc.cache.restore(&bus.cache);
        // After the hart, the INTC and the clock: the tracker rebases on them.
        self.restore_hang_section(&hang);
        // Only an old, refused format could hold an owed sleep stop; the call stays as a guard.
        self.perform_restored_sleep_stop();
        Ok(())
    }

    /// Puts this machine's eFuse words back after a redacted snapshot and rederives from them,
    /// without rearming the RWDT event the snapshot scheduled.
    fn rederive_efuse(&mut self) {
        let words = self.assets.efuse.dump_words();
        let devices = &mut self.soc.devices;
        devices.efuse.load_image(&words);
        let regs = devices.efuse.regs();
        devices
            .saradc
            .set_calibration(crate::machine::adc_calibration(regs));
        devices
            .rtc_cntl
            .restore_wdt_delay_sel(crate::machine::wdt_delay_sel(regs));
    }

    fn apply_hart(&mut self, h: &HartSection) {
        let [mstatus, mtvec, mepc, mcause, mtval, mscratch] = h.trap;
        let [tselect, tcontrol, mpcer, mpcmr, csr000] = h.misc;
        self.hart.x = h.x;
        self.hart.pc = h.pc;
        self.hart.csr = Csr {
            mstatus,
            mtvec,
            mepc,
            mcause,
            mtval,
            mscratch,
            pmpcfg: h.pmpcfg,
            pmpaddr: h.pmpaddr,
            tselect,
            tdata1: h.tdata1,
            tdata2: h.tdata2,
            tcontrol,
            mpcer,
            mpcmr,
            csr000,
        };
        self.hart.wfi = h.wfi;
        self.hart.insns = h.insns;
        self.hart.stores = h.stores;
        self.hart.spmon = h.spmon;
        self.hart.extra = h.extra;
        self.hart.pipe =
            pemu_rv32::cost::Pipe::from_byte(h.pipe, pemu_rv32::cost::Bank::from_bits(h.bank));
    }

    fn apply_board(&mut self, b: BoardSections) {
        self.board.lcd = b.lcd;
        self.board.backlight = b.backlight;
        self.board.codec = b.codec;
        self.board.gauge = b.gauge;
        self.board.battery = b.battery;
        self.board.ladder = b.ladder;
        self.board.power = b.power;
        self.board.usb = b.usb;
        self.board.world = b.world;
        self.board.set_i2c_target(b.i2c_target);
    }

    /// Puts the `host` section back, rewinding the guest-to-host rings to its heads.
    fn apply_host(&mut self, h: &HostSection) {
        self.receipt_cursor = h.receipt_cursor;
        self.idle_ps_at_run_start = h.idle_ps_at_run_start;
        self.resume_breakpoint = h.resume_breakpoint;
        [
            self.unapplied_inputs,
            self.pending_board_effects,
            self.pending_board_events,
            self.undispatched_events,
            self.peak_pending_wiring,
            self.unrecorded_spills,
            self.unpowered_events,
            self.dma_faults,
            self.pcm_width_faults,
        ] = h.counters;
        self.applied_wiring = h.applied_wiring;
        self.unapplied_wiring = h.unapplied_wiring;

        let [usj, uart, events] = h.output_heads;
        let io = &mut self.io;
        for (ring, head) in [(&mut io.usj_tx, usj), (&mut io.uart0_tx, uart)] {
            let (tail, kept) = rewound(ring.tail(), ring.head(), head, || {
                ring.slices(ring.tail()).iter().copied().collect()
            });
            ring.restore(tail, &kept)
                .expect("a window taken from the same ring fits it");
        }
        let ring = &mut io.events;
        let (tail, kept) = rewound(ring.tail(), ring.head(), events, || {
            ring.slices(ring.tail()).iter().copied().collect()
        });
        ring.restore(tail, &kept)
            .expect("a window taken from the same ring fits it");
        let [samples, records] = h.audio_out_heads;
        let pcm = &mut io.audio_out;
        let (tail, kept) = rewound(pcm.tail(), pcm.head(), samples, || {
            pcm.slices(pcm.tail()).iter().copied().collect()
        });
        let (record_tail, kept_records) =
            rewound(pcm.record_tail(), pcm.record_head(), records, || {
                pcm.record_slices(pcm.record_tail())
                    .iter()
                    .copied()
                    .collect()
            });
        let underflows = pcm.underflows();
        pcm.restore(tail, &kept, record_tail, &kept_records, underflows)
            .expect("a window taken from the same ring fits it");
        // No section carries `FramePort`: the next publish repaints it from panel memory.
        self.board.lcd.mark_all_dirty();
    }

    /// Rebuilds derived state: page table, flash cache arena, translation cache, stop and tap
    /// state, idle policy.
    fn rebuild_derived(&mut self, cache: &FlashCacheSection) {
        self.soc.pages = PageTable::new();
        mem::write_identity_pages(&mut self.soc.pages);
        // `apply_all` re-reads each mapped page and marks it current; the snapshot's cache
        // content replaces that: current pages from the store, stale ones from the section.
        let store = self.soc.flash.clone();
        mmu_wiring::apply_all(&mut self.soc);
        self.refold_protection();
        self.soc.flash = store;
        let flash_area = mem::FLASH_ARENA as usize..(mem::FLASH_ARENA + mem::FLASH_LEN) as usize;
        self.soc.arena.bytes_mut()[flash_area].fill(0);
        for page in 0..flash_store::PAGES {
            if cache.current[(page / 64) as usize] & (1 << (page % 64)) == 0 {
                continue;
            }
            let at = (mem::FLASH_ARENA + page * flash_store::PAGE_LEN) as usize;
            let soc = &mut self.soc;
            if let Some(src) = soc.flash.page_bytes(page) {
                soc.arena.bytes_mut()[at..at + src.len()].copy_from_slice(src);
                soc.flash.mark_mirrored(page);
            }
        }
        for p in &cache.stale {
            let at = (mem::FLASH_ARENA + p.page * flash_store::PAGE_LEN) as usize;
            self.soc.arena.bytes_mut()[at..at + p.bytes.len()].copy_from_slice(&p.bytes);
        }
        self.soc.take_invalidated();
        self.soc.take_wiring();
        self.engine.flush();
        self.armed = Default::default();
        self.tap = Default::default();
        self.idle = crate::sleep::idle_policy(&self.cfg);
        self.apply_slow_clock();
    }
}

impl Machine {
    /// The chunk number the journal expects next on live stream `stream`, rewound by a restore.
    pub fn next_live_chunk(&self, stream: pemu_core::journal::LiveStream) -> u64 {
        self.journal.live_next(stream)
    }

    pub fn determinism(&self) -> pemu_core::journal::Determinism {
        self.journal.class()
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use super::*;
    use crate::config::{Assets, MachineConfig};
    use crate::executor::Executor;
    use crate::run::RunLimits;
    use crate::stops::{LinePattern, Matcher, MatcherId, StopSet};
    use pemu_core::hostio::SerialStream;
    use pemu_core::snap::{Codec, LivePolicy};
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_rv32::bus::PF_CODE;

    /// A machine over the bundled ROM and an erased flash: the ROM retries until the RWDT resets.
    fn machine_with(cfg: MachineConfig) -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        Machine::new(cfg, assets).expect("the ROM fits the ROM window")
    }

    pub(super) fn machine() -> Machine {
        machine_with(MachineConfig::default())
    }

    fn console_from(m: &Machine, stream: SerialStream, from: u64) -> Vec<u8> {
        m.io.serial_ring(stream)
            .slices(from)
            .iter()
            .copied()
            .collect()
    }

    pub(super) fn run_insns(m: &mut Machine, insns: u64) {
        let out = m.run(RunLimits::insns(insns));
        assert!(
            matches!(out.reason, StopReason::MaxInsns | StopReason::Deadlock),
            "{:?}",
            out.reason
        );
    }

    /// Every section except `host` (output heads a fresh machine cannot share) and `frame` (a
    /// restore widens its dirty span on purpose).
    fn guest_sections(m: &Machine) -> Vec<(SectionId, Section)> {
        m.sections()
            .into_iter()
            .filter(|(id, _)| id.as_str() != HOST && id.as_str() != SectionId::FRAME)
            .collect()
    }

    fn assert_same_sections(a: &Machine, b: &Machine) {
        let (a, b) = (guest_sections(a), guest_sections(b));
        assert_eq!(a.len(), b.len());
        for ((ia, sa), (ib, sb)) in a.iter().zip(&b) {
            assert_eq!(ia, ib);
            assert!(sa == sb, "section {ia} differs");
        }
    }

    pub(super) fn restored_fresh(snap: &Snapshot, cfg: MachineConfig) -> Machine {
        let bytes = snap.to_bytes().expect("a machine snapshot serializes");
        let back = Snapshot::from_bytes(&bytes).expect("and parses");
        assert_eq!(&back, snap);
        let mut m = machine_with(cfg);
        m.restore(&back)
            .expect("a snapshot of the same run identity restores");
        m
    }

    #[test]
    fn the_block_list_is_the_c3_devices_table() {
        let names: Vec<&str> = with_blocks!(block_names,);
        let table: Vec<&str> = BLOCKS.iter().map(|b| b.name).collect();
        assert_eq!(names, table);
        let snap = machine().snapshot(SnapOpts::default());
        let soc_blocks = snap
            .sections
            .keys()
            .filter(|id| {
                id.as_str()
                    .strip_prefix(SectionId::SOC_PREFIX)
                    .is_some_and(|b| table.contains(&b))
            })
            .count();
        assert_eq!(soc_blocks, BLOCKS.len());
        assert!(snap.sections.keys().all(known_section));
    }

    /// Every `Machine` field is saved, rebuilt on restore, or host-side by design. No `..`, so a
    /// new field fails to compile here until it is classified.
    #[test]
    fn every_machine_field_is_saved_rebuilt_or_host_side() {
        let m = machine();
        let Machine {
            // Saved in the sections of the module documentation.
            hart: _,
            soc: _,
            board: _,
            clock: _,
            sched: _,
            rng: _,
            ledger: _,
            io: _,
            journal: _,
            irq: _,
            mcu_powered: _,
            resets: _,
            wiring_stop: _,
            // Saved in `host`, outside `state_hash`.
            idle_ps_at_run_start: _,
            receipt_cursor: _,
            resume_breakpoint: _,
            unapplied_inputs: _,
            pending_board_effects: _,
            #[cfg(test)]
                test_supply_mv: _,
            pending_board_events: _,
            undispatched_events: _,
            peak_pending_wiring: _,
            unrecorded_spills: _,
            unpowered_events: _,
            dma_faults: _,
            pcm_width_faults: _,
            applied_wiring: _,
            unapplied_wiring: _,
            // Rebuilt on restore (`rebuild_derived`).
            engine: _,
            armed: _,
            tap: _,
            idle: _,
            // Run identity, checked against the header, never restored.
            cfg: _,
            assets: _,
            // Derived from the configuration at construction.
            profile: _,
            // Host choices a restore keeps: trace sink, stop-set hooks, executor.
            trace: _,
            hooks: _,
            executor: _,
            // Saved in `hang`, outside `state_hash`.
            poll: _,
            // Host choices a restore keeps and a fork carries: the fast-forward switches and the
            // skipped-instruction count.
            poll_ff: _,
            rom_delay: _,
            ff_insns: _,
            ff_insns_at_run_start: _,
            // A host latency choice a restore keeps and a fork carries.
            max_slice: _,
            // Saved in `soc.disabled`; which blocks are disabled is configuration.
            disabled: _,
            // Saved in `hle` and `hle.machine`; the core is derived from the assets, and the
            // pending radio trip is empty between runs.
            hle: _,
        } = &m;
    }

    #[test]
    fn a_boxed_snapshot_machine_saves_forks_and_restores_through_the_trait() {
        let mut boxed: Box<dyn SnapshotMachine + Send> = Box::new(machine());
        let api: &mut dyn MachineApi = &mut *boxed;
        api.run(RunLimits::insns(20_000));
        let snap = boxed
            .snapshot(SnapOpts::default())
            .expect("a machine snapshots");
        let hash = boxed.state_hash();
        let mut fork = boxed.fork(LivePolicy::Refuse).expect("a machine forks");
        assert_eq!(fork.state_hash(), hash);
        let api: &mut dyn MachineApi = &mut *fork;
        api.run(RunLimits::insns(5_000));
        assert_ne!(fork.state_hash(), hash);
        fork.restore(&snap).expect("restores");
        assert_eq!(fork.state_hash(), hash);
        let mut later = snap.clone();
        boxed.redact(&mut later).expect("redacts");
        assert!(later.header.redacted && later.header.exported);
    }

    #[test]
    fn every_section_round_trips_through_the_byte_stream_into_a_fresh_machine() {
        let mut m = machine();
        run_insns(&mut m, 250_000);
        let snap = m.snapshot(SnapOpts::default());
        assert!(snap.sections.len() > 60, "{} sections", snap.sections.len());
        let restored = restored_fresh(&snap, MachineConfig::default());
        let again = restored.snapshot(SnapOpts::default());
        for (id, section) in &snap.sections {
            if id.as_str() == SectionId::FRAME {
                // A restore leaves the whole screen dirty on purpose; the generation carries.
                let before: FrameSection = decode(&snap, SectionId::FRAME, "frame").expect("frame");
                let after: FrameSection = decode(&again, SectionId::FRAME, "frame").expect("frame");
                assert_eq!(before.generation, after.generation);
                assert_eq!(after.dirty, Some((0, 319)));
                continue;
            }
            assert!(
                again.sections.get(id) == Some(section),
                "section {id} changed across the round trip"
            );
        }
        assert_eq!(again.sections.len(), snap.sections.len());
        assert_eq!(snap.header, again.header);
    }

    #[test]
    fn restore_rebuilds_the_page_table_and_the_flash_cache_it_replaced() {
        // `PF_CODE` marks what the flushed engine had translated, which is not rebuilt.
        let mut m = machine();
        run_insns(&mut m, 300_000);
        assert!(
            m.applied_wiring_by_kind().mmu_entry > 0,
            "the ROM mapped the cache"
        );
        let restored = restored_fresh(&m.snapshot(SnapOpts::default()), MachineConfig::default());
        let (live, rebuilt) = (m.soc.pages.entries(), restored.soc.pages.entries());
        for vpn in 0..live.len() {
            assert_eq!(
                live[vpn] & !PF_CODE,
                rebuilt[vpn] & !PF_CODE,
                "page entry {:#x}",
                vpn << 12
            );
        }
        assert!(
            m.soc.arena.bytes() == restored.soc.arena.bytes(),
            "arena bytes"
        );
        for page in 0..flash_store::PAGES {
            assert_eq!(
                m.soc.flash.mirror_is_current(page),
                restored.soc.flash.mirror_is_current(page),
                "flash page {page}"
            );
        }
        let lines = |m: &Machine| {
            (0..0x800)
                .step_by(4)
                .map(|off| m.irq.read(off))
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(&m), lines(&restored));
    }

    #[test]
    fn a_stale_flash_cache_page_keeps_what_the_guest_still_reads() {
        // Written behind the cache's back: a restore must not refresh the old bytes the guest
        // still reads.
        let mut m = machine();
        let page = 3;
        let at = (mem::FLASH_ARENA + page * flash_store::PAGE_LEN) as usize;
        m.soc.arena.bytes_mut()[at..at + 4].copy_from_slice(&[1, 2, 3, 4]);
        m.soc
            .flash
            .program(page * flash_store::PAGE_LEN, &[0x00; 8]);
        assert!(!m.soc.flash.mirror_is_current(page));
        let restored = restored_fresh(&m.snapshot(SnapOpts::default()), MachineConfig::default());
        assert_eq!(
            &restored.soc.arena.bytes()[at..at + 8],
            &[1, 2, 3, 4, 0, 0, 0, 0]
        );
        assert_eq!(restored.soc.flash.delta(), m.soc.flash.delta());
        assert_eq!(restored.state_hash(), m.state_hash());
    }

    #[test]
    fn state_hash_is_equal_after_restore_and_after_fork_and_moves_with_the_guest() {
        let mut m = machine();
        run_insns(&mut m, 120_000);
        let hash = m.state_hash();
        assert_eq!(hash, m.state_hash(), "a hash reads, it does not change");
        let snap = m.snapshot(SnapOpts::default());
        assert_eq!(
            restored_fresh(&snap, MachineConfig::default()).state_hash(),
            hash
        );
        let fork = m
            .fork(LivePolicy::Refuse)
            .expect("no live bridge is attached");
        assert_eq!(fork.state_hash(), hash);

        // A host reading output or asking for a receipt is not guest state.
        m.receipt();
        let cursor = m.io.lines.head(SerialStream::UsjTx);
        m.io.lines.set_read_cursor(SerialStream::UsjTx, cursor);
        assert_eq!(m.state_hash(), hash);

        let ledger = std::mem::take(&mut m.ledger);
        assert!(ledger.cursor() > 0, "the ROM run touched registers");
        assert_ne!(m.state_hash(), hash, "the ledger is part of the hash");
        m.ledger = ledger;
        assert_eq!(m.state_hash(), hash);

        run_insns(&mut m, 1);
        assert_ne!(m.state_hash(), hash, "one instruction moves the hash");
        m.restore(&snap)
            .expect("the machine restores its own snapshot");
        assert_eq!(m.state_hash(), hash);
    }

    #[test]
    fn a_fork_shares_the_base_images_and_runs_independently_of_its_parent() {
        let mut parent = machine();
        run_insns(&mut parent, 100_000);
        let mut fork = parent.fork(LivePolicy::default()).expect("forks");
        assert!(
            Arc::ptr_eq(&parent.assets, &fork.assets),
            "ROM and assets shared"
        );
        assert!(
            Arc::ptr_eq(parent.soc.flash.image(), fork.soc.flash.image()),
            "flash base shared"
        );
        let before = parent.state_hash();
        run_insns(&mut fork, 50_000);
        assert_eq!(
            parent.state_hash(),
            before,
            "running the fork leaves the parent alone"
        );
        run_insns(&mut parent, 50_000);
        assert_eq!(
            parent.state_hash(),
            fork.state_hash(),
            "and both reach the same state"
        );
        assert_same_sections(&parent, &fork);
    }

    #[test]
    fn n_instructions_from_one_snapshot_agree_under_the_engine_and_the_reference() {
        // Both restored machines must also match the source machine after the same instructions.
        let mut m = machine();
        run_insns(&mut m, 150_000);
        let snap = m.snapshot(SnapOpts::default());
        let heads = |m: &Machine| [m.io.usj_tx.head(), m.io.uart0_tx.head()];
        let at = heads(&m);
        let run = |executor| {
            let mut f = restored_fresh(&snap, MachineConfig::default());
            assert_eq!(
                heads(&f),
                at,
                "the output rings continue from the snapshot's heads"
            );
            f.set_executor(executor);
            run_insns(&mut f, 150_000);
            f
        };
        let engine = run(Executor::Engine);
        let reference = run(Executor::Reference);
        run_insns(&mut m, 150_000);
        for other in [&reference, &m] {
            assert_same_sections(&engine, other);
            assert_eq!(engine.state_hash(), other.state_hash());
            for (i, stream) in [SerialStream::UsjTx, SerialStream::Uart0Tx]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    console_from(&engine, stream, at[i]),
                    console_from(other, stream, at[i]),
                    "{stream:?}"
                );
            }
        }
        assert!(
            String::from_utf8_lossy(&console_from(&m, SerialStream::UsjTx, 0)).contains("ESP-ROM"),
            "the run is the ROM boot"
        );
    }

    #[test]
    fn a_version_format_or_identity_mismatch_is_refused_and_leaves_the_machine_alone() {
        let mut m = machine();
        run_insns(&mut m, 50_000);
        let snap = m.snapshot(SnapOpts::default());
        let mut target = machine();
        let before = target.state_hash();
        let hart = SectionId::new(SectionId::HART);

        let mut bumped = snap.clone();
        bumped.sections.get_mut(&hart).expect("hart").version = SECTION_VERSION + 1;
        assert_eq!(
            target.restore(&bumped),
            Err(SnapError::Incompatible {
                section: hart.clone(),
                found: SECTION_VERSION + 1,
                expected: SECTION_VERSION,
            })
        );
        let mut clock = snap.clone();
        let clock_id = SectionId::new(SectionId::CLOCK);
        clock.sections.get_mut(&clock_id).expect("clock").version = 9;
        assert!(matches!(
            target.restore(&clock),
            Err(SnapError::Incompatible { section, found: 9, .. }) if section == clock_id
        ));

        let mut format = snap.clone();
        format.header.format = FORMAT_VERSION + 1;
        assert_eq!(
            target.restore(&format),
            Err(SnapError::UnsupportedFormat {
                found: FORMAT_VERSION + 1,
                supported: FORMAT_VERSION,
            })
        );
        // Format 5 is refused as a whole: its `soc` section would misdecode.
        let mut five = snap.clone();
        five.header.format = 5;
        assert_eq!(
            target.restore(&five),
            Err(SnapError::UnsupportedFormat {
                found: 5,
                supported: FORMAT_VERSION,
            })
        );
        let mut old = snap.clone();
        old.header.format = 1;
        assert_eq!(
            target.restore(&old),
            Err(SnapError::UnsupportedFormat {
                found: 1,
                supported: FORMAT_VERSION,
            })
        );
        let mut bytes = snap.to_bytes().expect("serializes");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&bytes),
            Err(SnapError::UnsupportedFormat {
                found: 1,
                supported: FORMAT_VERSION,
            })
        );
        let mut bytes = snap.to_bytes().expect("serializes");
        bytes[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&bytes),
            Err(SnapError::UnsupportedFormat {
                found: FORMAT_VERSION + 1,
                supported: FORMAT_VERSION,
            })
        );

        let empty = Section {
            version: SECTION_VERSION,
            codec: Codec::Postcard,
            bytes: Vec::new(),
        };
        let mut unknown = snap.clone();
        unknown.put_raw(SectionId::soc("nosuch"), empty.clone());
        assert_eq!(
            target.restore(&unknown),
            Err(SnapError::UnknownSection(SectionId::soc("nosuch")))
        );
        let mut missing = snap.clone();
        missing.sections.remove(&SectionId::soc("usj"));
        assert_eq!(
            target.restore(&missing),
            Err(SnapError::MissingSection(SectionId::soc("usj")))
        );
        let mut trailing = snap.clone();
        trailing
            .sections
            .get_mut(&SectionId::new(MACHINE))
            .expect("machine")
            .bytes
            .push(0);
        assert_eq!(
            target.restore(&trailing),
            Err(SnapError::TrailingBytes {
                section: SectionId::new(MACHINE)
            })
        );
        let other = machine_with(MachineConfig {
            seed: 7,
            ..MachineConfig::default()
        });
        assert_eq!(
            target.restore(&other.snapshot(SnapOpts::default())),
            Err(SnapError::IdentityMismatch {
                field: pemu_core::snap::IdentityField::Config
            })
        );
        assert_eq!(
            target.state_hash(),
            before,
            "every refusal left the machine alone"
        );

        let mut optional = snap.clone();
        optional.put_raw(SectionId::new(SectionId::BLE), empty);
        target
            .restore(&optional)
            .expect("an optional section is skipped");
        assert_eq!(target.state_hash(), m.state_hash());
    }

    /// Refused on restore, not installed for the next register access to index out of bounds.
    #[test]
    fn a_model_section_with_a_window_of_another_length_is_refused() {
        let mut m = machine();
        run_insns(&mut m, 20_000);
        let snap = m.snapshot(SnapOpts::default());
        let mut target = machine();
        let before = target.state_hash();
        let id = SectionId::soc("aes");
        let mut corrupt = snap.clone();
        let bytes = &mut corrupt.sections.get_mut(&id).expect("aes").bytes;
        // `key` is a varint length of 32, then 32 bytes; one byte fewer is still a well-formed
        // postcard `Vec`, of the wrong length.
        assert_eq!(bytes[0], 32);
        bytes[0] = 31;
        bytes.remove(1);
        assert_eq!(
            target.restore(&corrupt),
            Err(SnapError::Malformed {
                at: "aes",
                reason: "the bytes are not this section's value",
            })
        );
        assert_eq!(
            target.state_hash(),
            before,
            "the refusal left the machine alone"
        );
    }

    /// Instruction counts from a fixed LCG, so the points are the same on every host.
    fn points(n: usize, below: u64) -> Vec<u64> {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                1 + (x >> 11) % (below - 1)
            })
            .collect()
    }

    /// The ROM boot with a future-stamped journaled input and undrained `usj_rx` bytes.
    fn boot_with_inputs() -> Machine {
        let mut m = machine();
        m.input(
            crate::machine::At::Vt(pemu_core::time::VTime::from_ms(6)),
            pemu_core::input::InputEvent::UsbClient { open: false },
        )
        .expect("a future instant is accepted");
        m.io.usj_rx.push(b"undrained");
        m
    }

    #[test]
    fn a_snapshot_taken_at_any_instruction_restores_to_the_same_future() {
        // At every point: save, go through the byte stream into a fresh machine (and once back
        // into the same machine), run on, and compare with the uninterrupted run.
        const TOTAL: u64 = 400_000;
        let mut straight = boot_with_inputs();
        run_insns(&mut straight, TOTAL);
        let hash = straight.state_hash();
        for (i, k) in points(8, TOTAL).into_iter().enumerate() {
            let mut m = boot_with_inputs();
            run_insns(&mut m, k);
            let snap = m.snapshot(SnapOpts::default());
            let heads = [m.io.usj_tx.head(), m.io.uart0_tx.head()];
            let mut fresh = restored_fresh(&snap, MachineConfig::default());
            run_insns(&mut fresh, TOTAL - k);
            assert_eq!(fresh.hart.insns, straight.hart.insns);
            assert_eq!(fresh.state_hash(), hash, "restored at instruction {k}");
            assert_same_sections(&fresh, &straight);
            for (j, stream) in [SerialStream::UsjTx, SerialStream::Uart0Tx]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    console_from(&fresh, stream, heads[j]),
                    console_from(&straight, stream, heads[j]),
                    "{stream:?} after instruction {k}"
                );
            }
            if i == 0 {
                run_insns(&mut m, TOTAL - k);
                m.restore(&snap).expect("in-process restore");
                run_insns(&mut m, TOTAL - k);
                assert_eq!(m.state_hash(), hash, "in-process restore at {k}");
            }
        }
    }

    #[test]
    fn a_snapshot_before_a_watchdog_reset_restores_to_the_same_reset_and_reboot() {
        // The snapshot carries the armed RWDT event (about 2.94 s) with its generation.
        use pemu_core::time::VTime;
        let until = |ms| RunLimits {
            until: Some(VTime::from_ms(ms)),
            max_insns: None,
            stops: StopSet::default(),
        };
        let start = || {
            let mut m = machine();
            m.hart.wfi = true;
            m
        };
        let mut straight = start();
        straight.run(until(2_900));
        let snap = straight.snapshot(SnapOpts::default());
        let resets = straight.resets();
        let mut fresh = restored_fresh(&snap, MachineConfig::default());
        for m in [&mut straight, &mut fresh] {
            m.run(until(2_990));
            run_insns(m, 60_000);
        }
        assert_eq!(straight.resets(), resets + 1, "the watchdog reset the chip");
        assert_eq!(fresh.resets(), straight.resets());
        assert_eq!(fresh.now(), straight.now());
        assert_eq!(fresh.state_hash(), straight.state_hash());
        assert_same_sections(&fresh, &straight);
    }

    fn retry_line() -> StopSet {
        StopSet {
            matchers: vec![(
                MatcherId(1),
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains("invalid header".into()),
                },
            )],
            ..StopSet::default()
        }
    }

    #[test]
    fn a_snapshot_at_a_breakpoint_mid_boot_continues_like_an_uninterrupted_run() {
        // A breakpoint stop is mid-slice (the engine's block ends at the hooked pc); 90,000
        // instructions in is mid-boot.
        let mut probe = machine();
        run_insns(&mut probe, 90_000);
        let bp = probe.hart.pc;
        let lim = |stops| RunLimits {
            until: None,
            max_insns: Some(5_000_000),
            stops,
        };

        let mut straight = machine();
        let done = straight.run(lim(retry_line()));
        assert_eq!(done.reason, StopReason::Matcher(MatcherId(1)));

        let mut m = machine();
        let stop = m.run(lim(StopSet {
            breakpoints: vec![bp],
            ..StopSet::default()
        }));
        assert_eq!(stop.reason, StopReason::Breakpoint(bp));
        assert!(stop.insns > 0 && stop.insns < done.insns);
        let snap = m.snapshot(SnapOpts::default());
        let head = m.io.usj_tx.head();
        // The restored machine resumes past the breakpoint exactly once.
        let mut again = restored_fresh(&snap, MachineConfig::default());
        let past = again.run(RunLimits {
            until: None,
            max_insns: Some(1),
            stops: StopSet {
                breakpoints: vec![bp],
                ..StopSet::default()
            },
        });
        assert_eq!(past.insns, 1, "{:?}", past.reason);
        for mut resumed in [restored_fresh(&snap, MachineConfig::default()), m] {
            let out = resumed.run(lim(retry_line()));
            assert_eq!(out.reason, done.reason);
            assert_eq!(out.vt, done.vt);
            assert_eq!(resumed.hart.insns, straight.hart.insns);
            assert_eq!(resumed.state_hash(), straight.state_hash());
            assert_eq!(
                console_from(&resumed, SerialStream::UsjTx, head),
                console_from(&straight, SerialStream::UsjTx, head)
            );
        }
    }

    #[test]
    fn an_in_process_restore_drops_blocks_translated_from_code_the_snapshot_did_not_have() {
        // The translation cache is not state: restoring writes the old bytes back behind the
        // engine's back, so a cache kept across the restore would still add 2.
        const PROG: u32 = mem::SRAM1_IRAM_BASE;
        const ADD1: u32 = 0x0015_0513;
        const ADD2: u32 = 0x0025_0513;
        const JUMP_BACK: u32 = 0xFFDF_F06F;
        let load = |m: &mut Machine, first: u32| {
            for (i, w) in [first, JUMP_BACK].into_iter().enumerate() {
                let stored = m.soc.store_mem(PROG + 4 * i as u32, 4, w);
                assert!(matches!(stored, pemu_soc_c3::Stored::Wrote { .. }));
            }
            m.drain_invalidations();
            m.hart.pc = PROG;
            m.hart.x[10] = 0;
        };
        let mut m = machine();
        m.set_executor(Executor::Engine);
        load(&mut m, ADD1);
        let snap = m.snapshot(SnapOpts::default());
        load(&mut m, ADD2);
        run_insns(&mut m, 1_000);
        assert_eq!(m.hart.x[10], 1_000, "the engine ran the second program");
        m.restore(&snap).expect("restores");
        run_insns(&mut m, 1_000);
        assert_eq!(m.hart.x[10], 500, "the restored program adds 1 per turn");
    }

    #[test]
    fn a_pending_sleep_stop_is_performed_on_restore() {
        // The machine performs sleep, so an owed sleep stop restores into the sleep itself.
        for kind in [SleepKind::Light, SleepKind::Deep] {
            let mut m = machine();
            m.wiring_stop = Some(StopReason::Sleep(kind));
            let restored =
                restored_fresh(&m.snapshot(SnapOpts::default()), MachineConfig::default());
            assert_eq!(restored.wiring_stop, None, "{kind:?}");
            match kind {
                SleepKind::Light => assert!(restored.mcu_powered && restored.hart.wfi),
                SleepKind::Deep => assert!(!restored.mcu_powered),
            }
            let sleeps: Vec<u64> = restored
                .io
                .events
                .slices(0)
                .iter()
                .filter(|e| e.kind == pemu_core::hostio::EventKind::Sleep)
                .map(|e| e.arg)
                .collect();
            let want = match kind {
                SleepKind::Light => crate::sleep::SLEEP_EVENT_LIGHT,
                SleepKind::Deep => crate::sleep::SLEEP_EVENT_DEEP,
            };
            assert_eq!(sleeps.last(), Some(&want));
        }
        let mut bad = machine().snapshot(SnapOpts::default());
        let id = SectionId::new(MACHINE);
        let mut section: MachineSection = decode(&bad, MACHINE, "machine").expect("decodes");
        section.sleep_stop = Some(7);
        bad.put_raw(id, encode(&section));
        assert!(matches!(
            machine().restore(&bad),
            Err(SnapError::Malformed { at: "machine", .. })
        ));
    }

    #[test]
    fn a_restore_rewinds_the_live_chunk_count_so_mic_numbering_continues_from_it() {
        use crate::machine::At;
        use pemu_core::input::InputEvent;
        use pemu_core::journal::LiveStream;

        let chunk = |seq| InputEvent::MicChunk {
            seq,
            samples: vec![0; 16],
        };
        let mut m = machine();
        m.input(At::Now, chunk(0)).expect("journaled");
        let snap = m.snapshot(SnapOpts::default());
        let class = m.determinism();
        assert_eq!(m.next_live_chunk(LiveStream::Mic), 1);
        m.input(At::Now, chunk(1)).expect("journaled");
        m.input(At::Now, chunk(2)).expect("journaled");
        assert_eq!(m.next_live_chunk(LiveStream::Mic), 3);

        m.restore(&snap).expect("restores");
        assert_eq!(m.next_live_chunk(LiveStream::Mic), 1, "the count went back");
        m.input(At::Now, chunk(1)).expect("journaled");
        assert_eq!(m.next_live_chunk(LiveStream::Mic), 2);
        assert_eq!(
            m.determinism(),
            class,
            "chunk 1 after the restore is not a loss"
        );
    }

    /// The `FramePort` generation travels outside `state_hash`; the whole screen is left dirty so
    /// a host repaints even when the guest never draws again.
    #[test]
    fn a_restore_carries_the_frame_generation_and_leaves_the_whole_screen_dirty() {
        let mut m = machine();
        let hash = m.state_hash();
        for _ in 0..32 {
            m.io.frame.present();
        }
        m.io.frame.mark_dirty(10, 20);
        assert_eq!(m.state_hash(), hash, "frame numbering is host-visible only");
        let snap = m.snapshot(SnapOpts::default());

        let fresh = restored_fresh(&snap, MachineConfig::default());
        assert_eq!(
            (fresh.io.frame.generation(), fresh.io.frame.dirty_rows()),
            (32, Some((0, 319)))
        );
        assert_eq!(fresh.state_hash(), hash);

        for _ in 0..7 {
            m.io.frame.present();
        }
        m.io.frame.take_dirty();
        m.restore(&snap).expect("restores");
        assert_eq!(
            (m.io.frame.generation(), m.io.frame.dirty_rows()),
            (32, Some((0, 319)))
        );
    }

    #[test]
    fn a_restore_after_the_host_took_every_dirty_row_leaves_the_whole_screen_dirty() {
        let mut m = machine();
        m.io.frame.present();
        m.io.frame.take_dirty();
        let snap = m.snapshot(SnapOpts::default());
        let generation = m.io.frame.generation();
        m.io.frame.present();
        m.io.frame.take_dirty();
        m.restore(&snap).expect("restores");
        assert_eq!(
            m.io.frame.dirty_rows(),
            Some((0, 319)),
            "a host must repaint after a restore"
        );
        assert_eq!(m.io.frame.generation(), generation);
    }

    /// Which rows a host has published is not part of `state_hash`.
    #[test]
    fn a_restore_rewinds_audio_out_and_repaints_the_panel_whose_dirty_rows_are_not_hashed() {
        use pemu_core::time::VTime;

        let mut m = machine();
        assert_eq!(m.board.lcd.dirty_rows(), None);
        let hash = m.state_hash();
        m.board.lcd.mark_all_dirty();
        assert_eq!(
            m.state_hash(),
            hash,
            "the panel's dirty rows are host pacing"
        );
        let mut m = machine();

        m.io.audio_out.write(VTime(0), 16_000, 1, &[1; 100]);
        let snap = m.snapshot(SnapOpts::default());
        m.io.audio_out
            .write(VTime::from_ms(1000), 48_000, 2, &[2; 50]);
        assert_eq!(
            (m.io.audio_out.head(), m.io.audio_out.record_head()),
            (150, 2)
        );

        m.restore(&snap).expect("restores");
        let pcm = &m.io.audio_out;
        assert_eq!((pcm.tail(), pcm.head()), (0, 100));
        assert_eq!((pcm.record_tail(), pcm.record_head()), (0, 1));
        assert!(pcm.slices(0).iter().all(|s| *s == 1));
        assert_eq!(pcm.record_at(99).map(|r| r.fs), Some(16_000));
        assert_eq!(m.board.lcd.dirty_rows(), Some((0, 319)));

        // The rings of a machine that never produced those samples start at the recorded heads.
        let fresh = restored_fresh(&snap, MachineConfig::default());
        let pcm = &fresh.io.audio_out;
        assert_eq!((pcm.tail(), pcm.head(), pcm.record_head()), (100, 100, 1));
    }
}
