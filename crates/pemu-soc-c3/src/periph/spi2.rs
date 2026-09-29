//! GP-SPI2 (0x60024000, interrupt source 19), the LCD transport, on the shared [`RegFile`]
//! (`specs/blocks/spi2.toml`).
//!
//! `spi_ll_apply_config` loops on `CMD.update` and `spi_device_polling_end` on
//! `DMA_INT_RAW.trans_done`, neither with a yield, so without this model the guest stalls in
//! `bsp_display_init`. Under the default timing both self-clearing bits and `trans_done` are
//! answered inside the access that starts the transaction ([`TransDone::Immediate`]).
//!
//! `CMD.usr` with `USER.usr_mosi` and `DMA_CONF.dma_tx_ena` sends `MS_DLEN + 1` bits from the
//! GDMA TX channel bound to SPI2. The model records the request in [`Model::pending`]; collecting
//! the bytes and calling the board is `wiring/spi2.rs`, behind [`Wiring::Spi2Transfer`].

use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::irq_source::irq;
use pemu_core::regstore::{RegSpec, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner, PeriphId};
use pemu_core::time::VTime;

use crate::r#gen::regs_spi2::{BLOCK_SIZE, REG_COUNT, REGS, idx};

use super::reg_file::{RegFile, RegTable};
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block};

impl RegTable<REG_COUNT> for block::Spi2 {
    const SPECS: &'static [RegSpec; REG_COUNT] = &REGS;
}

/// `SPI_CMD.SPI_UPDATE` (bit 23, WT): latch the configuration; reads 0 in the same access
/// (IDF `hal/esp32c3/include/hal/spi_ll.h`).
pub const CMD_UPDATE: u32 = 1 << 23;
/// `SPI_CMD.SPI_USR` (bit 24, SC): start a user transaction.
pub const CMD_USR: u32 = 1 << 24;
/// `SPI_USER.SPI_USR_MOSI` (bit 27): the transaction has a write phase.
pub const USER_USR_MOSI: u32 = 1 << 27;
/// `SPI_MS_DLEN.SPI_MS_DATA_BITLEN` (bits 17 to 0): transfer length in bits minus 1.
pub const MS_DATA_BITLEN: u32 = 0x3_FFFF;
/// `SPI_MISC.SPI_CS_KEEP_ACTIVE` (bit 30): CS stays asserted after the transaction.
pub const MISC_CS_KEEP_ACTIVE: u32 = 1 << 30;
/// `SPI_DMA_CONF.SPI_DMA_TX_ENA` (bit 28): the write phase is fed by GDMA.
pub const DMA_CONF_TX_ENA: u32 = 1 << 28;
/// `SPI_DMA_INT_*.SPI_TRANS_DONE_INT_*` (bit 12): the only interrupt the master uses.
pub const INT_TRANS_DONE: u32 = 1 << 12;

/// Event tag of the deferred `trans_done` under [`TransDone::Clocked`].
pub const EV_TRANS_DONE: u16 = 0;

/// When `DMA_INT_RAW.trans_done` rises after `CMD.usr`.
///
/// The polling loop has no yield, so a later completion only burns host time. The timing profile
/// picks the variant on every guest write, from the `SPI_CLOCK` the guest programmed; the
/// `probe_timing` capture times a 153,600-byte frame at 30.93 ms against 30.72 ms of wire time.
#[derive(
    Copy,
    Clone,
    PartialEq,
    Eq,
    Debug,
    Default,
    pemu_core::serde::Serialize,
    pemu_core::serde::Deserialize,
)]
#[serde(crate = "pemu_core::serde")]
pub enum TransDone {
    /// The `fast` profile: `trans_done` rises and `CMD.usr` clears inside the `CMD.usr` write.
    #[default]
    Immediate,
    /// The `device` profile: `trans_done` rises `bits * ps_per_bit` picoseconds later, through
    /// [`EV_TRANS_DONE`]; 25000 ps per bit at the panel's 40 MHz clock.
    Clocked {
        /// Picoseconds per transferred bit.
        ps_per_bit: u32,
    },
}

/// The user transaction a `CMD.usr` write started, waiting for `wiring/spi2.rs`.
#[derive(
    Copy, Clone, PartialEq, Eq, Debug, pemu_core::serde::Serialize, pemu_core::serde::Deserialize,
)]
#[serde(crate = "pemu_core::serde")]
pub struct UserTransfer {
    /// `MS_DLEN.ms_data_bitlen + 1`.
    pub bits: u32,
    /// Whole bytes of `bits`; every transfer on this path is whole bytes.
    pub bytes: u32,
    /// `MISC.cs_keep_active`; the panel sees `cs_release = !cs_keep_active`.
    pub cs_keep_active: bool,
    /// `USER.usr_mosi`: there is a write phase to collect bytes for.
    pub mosi: bool,
    /// `DMA_CONF.dma_tx_ena`: the write phase is fed by the GDMA TX channel bound to SPI2.
    pub dma_tx: bool,
}

impl UserTransfer {
    /// Whether `wiring/spi2.rs` has bytes to collect: a DMA-fed write phase of non-zero length.
    pub fn feeds_the_panel(&self) -> bool {
        self.mosi && self.dma_tx && self.bytes > 0
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Step {
    /// A user transaction started: apply [`Wiring::Spi2Transfer`].
    pub transfer: bool,
    pub irq: Option<bool>,
    /// Under [`TransDone::Clocked`], when [`EV_TRANS_DONE`] is due.
    pub done_at: Option<VTime>,
    pub stop: bool,
}

#[derive(Default, pemu_core::serde::Serialize, pemu_core::serde::Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Master {
    regs: RegFile<block::Spi2, REG_COUNT>,
    /// The transaction `wiring/spi2.rs` still owes bytes for.
    pending: Option<UserTransfer>,
    /// Completion timing of `trans_done`.
    done: TransDone,
    /// Level this model last drove on source 19, so a change is reported once.
    irq: bool,
}

impl Master {
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.regs.read(off, size, now, ledger)
    }

    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Step {
        let mut step = Step::default();
        for (i, delta) in self.regs.write(off, size, val, now, ledger).iter() {
            match i {
                idx::SPI_CMD => {
                    // UPDATE is WT, so it already reads 0; the configuration is read when the
                    // transaction starts, so there is nothing to latch here.
                    if delta.triggers & CMD_USR != 0 {
                        self.start(now, &mut step);
                    }
                }
                idx::SPI_DMA_INT_CLR => {
                    let raw = self.regs.get(idx::SPI_DMA_INT_RAW);
                    self.regs.set(idx::SPI_DMA_INT_RAW, raw & !delta.triggers);
                }
                idx::SPI_DMA_INT_RAW | idx::SPI_DMA_INT_ENA => {}
                _ => continue,
            }
            step.irq = self.sync_irq().or(step.irq);
        }
        step
    }

    /// Raises `trans_done`, clears `CMD.usr` and returns the new level of source 19 when it
    /// changed, whether inside the `CMD.usr` write or at [`EV_TRANS_DONE`].
    pub fn finish(&mut self) -> Option<bool> {
        self.regs.clear_sc(idx::SPI_CMD, CMD_USR);
        self.regs.raise(idx::SPI_DMA_INT_RAW, INT_TRANS_DONE);
        self.sync_irq()
    }

    pub fn pending(&self) -> Option<&UserTransfer> {
        self.pending.as_ref()
    }

    /// Takes the pending transaction, once per [`Wiring::Spi2Transfer`].
    pub fn take_pending(&mut self) -> Option<UserTransfer> {
        self.pending.take()
    }

    /// Level the model drives on interrupt source 19: `INT_ST != 0`, with `INT_ST = RAW & ENA`.
    pub fn irq_level(&self) -> bool {
        self.regs.get(idx::SPI_DMA_INT_ST) != 0
    }

    /// Sets the completion timing of `trans_done`. A guest write through [`Peripheral::write`]
    /// replaces it with the profile's choice; a test driving [`Master::store`] keeps it.
    pub fn set_trans_done(&mut self, done: TransDone) {
        self.done = done;
    }

    /// Picoseconds per bit at the clock `SPI_CLOCK` selects: the 80 MHz APB clock divided by
    /// `(clkdiv_pre + 1) x (clkcnt_n + 1)`, or undivided with `clk_equ_sysclk`. The panel
    /// driver's 0x00001001 gives 25,000 ps (40 MHz).
    pub fn clock_ps_per_bit(&self) -> u32 {
        const APB_PS: u32 = 12_500;
        let clock = self.regs.get(idx::SPI_CLOCK);
        if clock >> 31 & 1 == 1 {
            return APB_PS;
        }
        let n = (clock >> 12 & 0x3F) + 1;
        let pre = (clock >> 18 & 0xF) + 1;
        APB_PS * n * pre
    }

    pub fn regs(&self) -> &RegFile<block::Spi2, REG_COUNT> {
        &self.regs
    }

    /// `CMD.usr` rose: read the latched configuration and complete now or schedule completion.
    fn start(&mut self, now: VTime, step: &mut Step) {
        let bits = (self.regs.get(idx::SPI_MS_DLEN) & MS_DATA_BITLEN) + 1;
        let transfer = UserTransfer {
            bits,
            bytes: bits / 8,
            cs_keep_active: self.regs.get(idx::SPI_MISC) & MISC_CS_KEEP_ACTIVE != 0,
            mosi: self.regs.get(idx::SPI_USER) & USER_USR_MOSI != 0,
            dma_tx: self.regs.get(idx::SPI_DMA_CONF) & DMA_CONF_TX_ENA != 0,
        };
        self.pending = Some(transfer);
        step.transfer = true;
        step.stop = true;
        match self.done {
            // `finish` syncs the level itself, so the later sync in `store` would see no change:
            // the rising edge must be carried out of here or the queued-chunk ISR never runs.
            TransDone::Immediate => {
                step.irq = self.finish().or(step.irq);
            }
            TransDone::Clocked { ps_per_bit } => {
                let span = u64::from(bits).saturating_mul(u64::from(ps_per_bit));
                step.done_at = Some(VTime(now.0.saturating_add(span)));
            }
        }
    }

    /// Recomputes `INT_ST = RAW & ENA` and returns the new level of source 19 when it changed.
    fn sync_irq(&mut self) -> Option<bool> {
        let st = self.regs.get(idx::SPI_DMA_INT_RAW) & self.regs.get(idx::SPI_DMA_INT_ENA);
        self.regs.set(idx::SPI_DMA_INT_ST, st);
        let level = st != 0;
        (level != self.irq).then(|| {
            self.irq = level;
            level
        })
    }
}

impl Peripheral for Master {
    const ID: PeriphId = <block::Spi2 as Block>::ID;
    const BASE: u32 = <block::Spi2 as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.regs.reset(kind);
        self.pending = None;
        if let Some(level) = self.sync_irq() {
            cx.irq.set_source(irq::SPI2, level);
        }
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        // The profile picks when `trans_done` rises: at once under `fast`, after the bits at the
        // programmed clock plus the per-transaction overhead under `device`.
        self.done = if cx.profile.spi2_clocked {
            TransDone::Clocked {
                ps_per_bit: self.clock_ps_per_bit(),
            }
        } else {
            TransDone::Immediate
        };
        let mut step = self.store(off, size, val, cx.now, cx.ledger);
        if let Some(at) = step.done_at.as_mut() {
            *at = VTime(at.0.saturating_add(cx.profile.spi2_overhead_ps));
        }
        if let Some(level) = step.irq {
            cx.irq.set_source(irq::SPI2, level);
        }
        if let Some(at) = step.done_at {
            cx.sched.schedule(
                cx.now,
                at,
                EventKey {
                    owner: Owner::Periph(Self::ID),
                    tag: EV_TRANS_DONE,
                },
            );
        }
        RegWrite {
            stop: step.stop,
            wiring: if step.transfer {
                Wiring::Spi2Transfer
            } else {
                Wiring::None
            },
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        if tag == EV_TRANS_DONE
            && let Some(level) = self.finish()
        {
            cx.irq.set_source(irq::SPI2, level);
        }
        Wiring::None
    }

    /// `SPI_CMD` and the interrupt registers change only through a transaction this model
    /// performs: under [`TransDone::Clocked`] at the completion event. Everything else is plain
    /// storage, so the conservative [`Stability::Never`] costs nothing.
    fn stable_until(&self, off: u32, _cx: &Cx) -> Stability {
        match Self::reg_index(off) {
            Some(idx::SPI_CMD | idx::SPI_DMA_INT_RAW | idx::SPI_DMA_INT_ST) => {
                Stability::UntilNextEvent
            }
            _ => Stability::Never,
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs.class(off)
    }
}

impl Master {
    fn reg_index(off: u32) -> Option<usize> {
        RegFile::<block::Spi2, REG_COUNT>::index_of(off & !3)
    }
}

pub type Model = Master;

#[cfg(test)]
mod tests {
    use pemu_core::fidelity::TouchAccess;
    use pemu_core::reset::ResetCause;

    use crate::r#gen::waits::{self, Within};

    use super::*;

    const T: VTime = VTime(1_000);

    /// Mask of field `field` of register `reg`, so a wait-row test names the spec's field.
    fn field_mask(specs: &[RegSpec], reg: &str, field: &str) -> u32 {
        let spec = specs
            .iter()
            .find(|spec| spec.name == reg)
            .unwrap_or_else(|| panic!("spi2 has no register {reg}"));
        let f = spec
            .fields
            .iter()
            .find(|f| f.name == field)
            .unwrap_or_else(|| panic!("{reg} has no field {field}"));
        ((1u32 << f.width) - 1) << f.shift
    }

    fn reg_off(reg: &str) -> u32 {
        u32::from(
            REGS.iter()
                .find(|spec| spec.name == reg)
                .unwrap_or_else(|| panic!("spi2 has no register {reg}"))
                .off,
        )
    }

    /// The register writes `spi_hal_setup_trans` and `s_spi_dma_prepare_data` make before
    /// `spi_hal_user_start` for one `esp_lcd` transaction of `bytes` bytes.
    fn setup(m: &mut Master, l: &mut FidelityLedger, bytes: u32, cs_keep_active: bool) {
        m.store(0x1C, Size::B4, bytes * 8 - 1, T, l);
        m.store(0x10, Size::B4, USER_USR_MOSI, T, l);
        m.store(0x30, Size::B4, DMA_CONF_TX_ENA, T, l);
        let misc = if cs_keep_active {
            MISC_CS_KEEP_ACTIVE
        } else {
            0
        };
        m.store(0x20, Size::B4, misc, T, l);
    }

    #[test]
    fn the_block_identity_is_the_c3_devices_row() {
        assert_eq!(<Master as Peripheral>::ID, super::super::id::SPI2);
        assert_eq!(<Master as Peripheral>::BASE, 0x6002_4000);
        assert_eq!(<Master as Peripheral>::SIZE, 0x1000);
        assert_eq!(REG_COUNT, REGS.len());
    }

    #[test]
    fn cmd_update_reads_back_zero_in_the_same_access() {
        let row = waits::of_block("spi2")
            .find(|w| w.id == "spi2.cmd_update")
            .expect("the block file carries the row");
        assert_eq!(row.within, Within::All("same_access"));
        let mask = field_mask(&REGS, row.register, row.field);
        assert_eq!(mask, CMD_UPDATE);

        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        let off = reg_off(row.register);
        let step = m.store(off, Size::B4, mask, T, &mut l);
        assert_eq!(m.load(off, Size::B4, T, &mut l) & mask, 0, "{}", row.expect);
        assert!(!step.transfer, "UPDATE latches config, it starts nothing");
        assert_eq!(step.irq, None);
    }

    #[test]
    fn cmd_usr_clears_and_trans_done_rises_in_the_same_access() {
        let usr = waits::of_block("spi2")
            .find(|w| w.id == "spi2.cmd_usr")
            .expect("the block file carries the row");
        let done = waits::of_block("spi2")
            .find(|w| w.id == "spi2.trans_done")
            .expect("the block file carries the row");
        let usr_mask = field_mask(&REGS, usr.register, usr.field);
        let done_mask = field_mask(&REGS, done.register, done.field);

        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        setup(&mut m, &mut l, 9600, true);
        let step = m.store(reg_off(usr.register), Size::B4, usr_mask, T, &mut l);

        assert!(step.transfer, "the wiring step has bytes to move");
        assert!(step.stop, "OkStop, so the machine applies the wiring");
        assert_eq!(step.done_at, None, "the fast profile completes in place");
        assert_eq!(
            m.load(reg_off(usr.register), Size::B4, T, &mut l) & usr_mask,
            0,
            "{}",
            usr.expect
        );
        assert_eq!(
            m.load(reg_off(done.register), Size::B4, T, &mut l) & done_mask,
            done_mask,
            "{}",
            done.expect
        );
        assert_eq!(
            m.take_pending(),
            Some(UserTransfer {
                bits: 76_800,
                bytes: 9_600,
                cs_keep_active: true,
                mosi: true,
                dma_tx: true,
            })
        );
        assert_eq!(m.take_pending(), None, "the wiring takes it once");
    }

    /// The rising edge is load-bearing: the source-19 ISR completes the queued pixel chunks and
    /// eventually calls LVGL's `flush_ready`.
    #[test]
    fn a_completed_transaction_reports_the_rising_edge_of_source_19() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        let (raw, ena) = (reg_off("SPI_DMA_INT_RAW"), reg_off("SPI_DMA_INT_ENA"));

        // `spi_hal_init` sets the raw bit, `spi_hal_setup_trans` clears it, the driver unmasks.
        m.store(raw, Size::B4, INT_TRANS_DONE, T, &mut l);
        m.store(raw, Size::B4, 0, T, &mut l);
        let step = m.store(ena, Size::B4, INT_TRANS_DONE, T, &mut l);
        assert_eq!(step.irq, None, "RAW is 0, so the level did not change");

        setup(&mut m, &mut l, 9600, false);
        let step = m.store(0x00, Size::B4, CMD_USR, T, &mut l);
        assert_eq!(
            step.irq,
            Some(true),
            "the completion inside the CMD.usr write raises source 19"
        );
        assert!(m.irq_level(), "and INT_ST agrees with the reported level");
        assert_eq!(
            m.load(reg_off("SPI_DMA_INT_ST"), Size::B4, T, &mut l),
            INT_TRANS_DONE
        );

        // The ISR clears the raw bit and the next transaction raises it again.
        let step = m.store(
            reg_off("SPI_DMA_INT_CLR"),
            Size::B4,
            INT_TRANS_DONE,
            T,
            &mut l,
        );
        assert_eq!(step.irq, Some(false));
        let step = m.store(0x00, Size::B4, CMD_USR, T, &mut l);
        assert_eq!(
            step.irq,
            Some(true),
            "the next queued chunk raises it again"
        );
    }

    /// `spi_hal_init` sets the bit and `spi_hal_setup_trans` clears it, both by software.
    #[test]
    fn trans_done_accepts_software_writes_of_zero_and_one() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        let raw = reg_off("SPI_DMA_INT_RAW");
        m.store(raw, Size::B4, INT_TRANS_DONE, T, &mut l);
        assert_eq!(m.load(raw, Size::B4, T, &mut l), INT_TRANS_DONE);
        m.store(raw, Size::B4, 0, T, &mut l);
        assert_eq!(m.load(raw, Size::B4, T, &mut l), 0);
    }

    #[test]
    fn int_st_is_raw_and_ena_and_drives_source_19() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        let (raw, ena, clr, st) = (
            reg_off("SPI_DMA_INT_RAW"),
            reg_off("SPI_DMA_INT_ENA"),
            reg_off("SPI_DMA_INT_CLR"),
            reg_off("SPI_DMA_INT_ST"),
        );

        let step = m.store(raw, Size::B4, INT_TRANS_DONE, T, &mut l);
        assert_eq!(step.irq, None, "masked off while ENA is 0");
        assert_eq!(m.load(st, Size::B4, T, &mut l), 0);
        assert!(!m.irq_level());

        let step = m.store(ena, Size::B4, INT_TRANS_DONE, T, &mut l);
        assert_eq!(step.irq, Some(true), "the level changed once");
        assert_eq!(m.load(st, Size::B4, T, &mut l), INT_TRANS_DONE);
        assert!(m.irq_level());

        let step = m.store(ena, Size::B4, INT_TRANS_DONE, T, &mut l);
        assert_eq!(step.irq, None, "an unchanged level is not reported again");

        let step = m.store(clr, Size::B4, INT_TRANS_DONE, T, &mut l);
        assert_eq!(step.irq, Some(false));
        assert_eq!(
            m.load(raw, Size::B4, T, &mut l),
            0,
            "INT_CLR clears the raw bit"
        );
        assert_eq!(
            m.load(clr, Size::B4, T, &mut l),
            0,
            "INT_CLR is write-trigger"
        );
    }

    #[test]
    fn the_clocked_profile_defers_trans_done_to_its_event() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        m.set_trans_done(TransDone::Clocked { ps_per_bit: 25_000 });
        setup(&mut m, &mut l, 8, false);
        let step = m.store(0x00, Size::B4, CMD_USR, T, &mut l);

        assert_eq!(step.done_at, Some(VTime(T.0 + 64 * 25_000)));
        assert_eq!(m.load(0x00, Size::B4, T, &mut l) & CMD_USR, CMD_USR);
        assert_eq!(m.load(0x3C, Size::B4, T, &mut l) & INT_TRANS_DONE, 0);
        assert!(step.transfer, "the bytes still go out at the write");

        assert_eq!(m.finish(), None, "ENA is 0, so no level change");
        assert_eq!(m.load(0x00, Size::B4, T, &mut l) & CMD_USR, 0);
        assert_eq!(
            m.load(0x3C, Size::B4, T, &mut l) & INT_TRANS_DONE,
            INT_TRANS_DONE
        );
    }

    #[test]
    fn a_transaction_without_mosi_or_dma_feeds_no_panel_bytes() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        m.store(0x1C, Size::B4, 8 * 8 - 1, T, &mut l);
        m.store(0x00, Size::B4, CMD_USR, T, &mut l);
        let t = m.take_pending().expect("a transaction started");
        assert_eq!((t.bits, t.bytes), (64, 8));
        assert!(!t.feeds_the_panel(), "usr_mosi and dma_tx_ena are both 0");

        setup(&mut m, &mut l, 8, false);
        m.store(0x00, Size::B4, CMD_USR, T, &mut l);
        assert!(m.take_pending().expect("started").feeds_the_panel());
    }

    #[test]
    fn every_row_of_the_block_file_names_a_register_and_field_of_the_table() {
        let rows: Vec<_> = waits::of_block("spi2").collect();
        assert_eq!(rows.len(), 3, "cmd_update, trans_done and cmd_usr");
        for row in rows {
            assert_ne!(field_mask(&REGS, row.register, row.field), 0, "{}", row.id);
            assert_eq!(row.milestone, "M4", "{}", row.id);
        }
    }

    #[test]
    fn the_register_file_reports_each_register_once_and_splits_narrow_accesses() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        m.store(0x1C, Size::B4, 0x3_FFFF, T, &mut l);
        assert_eq!(m.load(0x1C, Size::B1, T, &mut l), 0xFF);
        assert_eq!(m.load(0x1E, Size::B2, T, &mut l), 0x0003);
        m.store(0x1D, Size::B1, 0, T, &mut l);
        assert_eq!(m.load(0x1C, Size::B4, T, &mut l), 0x3_00FF);

        // Crossing two registers: the high half of MS_DLEN, then the low half of MISC, which
        // resets to 0x3E (cs1 to cs5 disabled).
        assert_eq!(m.load(0x1E, Size::B4, T, &mut l), 0x003E_0003);
        assert_eq!(m.load(0xFF0, Size::B4, T, &mut l), 0, "no register there");

        let touches: Vec<_> = l
            .first_touches()
            .iter()
            .map(|t| (t.off, t.access))
            .collect();
        assert_eq!(
            touches,
            vec![
                (0x1C, TouchAccess::Write),
                (0x20, TouchAccess::Read),
                (0xFF0, TouchAccess::Read),
            ],
            "one entry per register, first access wins"
        );
    }

    /// The lookup is a binary search, correct only while the table is in offset order.
    #[test]
    fn the_register_lookup_finds_every_row_of_the_generated_table() {
        for (i, spec) in REGS.iter().enumerate() {
            assert_eq!(
                Master::reg_index(u32::from(spec.off)),
                Some(i),
                "{}",
                spec.name
            );
            assert_eq!(
                Master::reg_index(u32::from(spec.off) + 3),
                Some(i),
                "{} through its last byte",
                spec.name
            );
        }
        assert_eq!(Master::reg_index(0xFF0), None, "no register there");
        assert_eq!(Master::reg_index(u32::MAX), None);
    }

    #[test]
    fn a_chip_reset_restores_the_table_and_keeps_the_first_touches() {
        let (mut m, mut l) = (Master::default(), FidelityLedger::default());
        m.store(0x20, Size::B4, MISC_CS_KEEP_ACTIVE, T, &mut l);
        assert_eq!(
            m.load(0x20, Size::B4, T, &mut l),
            MISC_CS_KEEP_ACTIVE,
            "a whole-register write also clears the cs1 to cs5 disable bits"
        );
        let before = l.first_touches().len();

        m.regs
            .reset(ResetKind::of(ResetCause::POWERON).expect("the power-on reset is documented"));
        assert_eq!(
            m.load(0x20, Size::B4, T, &mut l),
            0x3E,
            "MISC is back to its reset value, cs1 to cs5 disabled"
        );
        assert_eq!(l.first_touches().len(), before, "touches are per machine");
    }
}
