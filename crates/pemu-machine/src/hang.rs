//! Hang detector: reports a hart that makes no progress as `StopReason::Stuck`, naming the
//! `[[wait]]` row of `specs/blocks/<block>.toml` the guest is polling. [`StuckKind`] says which
//! condition fired. A WFI hart nothing can wake is `StopReason::Deadlock` instead, and a register
//! that changes only through an input is never a hang.
//!
//! Every decision is taken at a tracked read or a confirmation, which happen at the same
//! instruction with poll fast-forward on or off, so a `Stuck` stop lands on the same instruction
//! either way.

use pemu_core::regstore::RegSpec;
use pemu_core::time::VTime;
use pemu_soc_c3::r#gen::waits::{self, WaitSpec};
use pemu_soc_c3::periph::{BLOCKS, lookup};

use crate::machine::Machine;
use crate::poll_ff::HangHit;

/// The block a report names for an address no `c3_devices!` row claims.
pub const UNMAPPED_BLOCK: &str = "unmapped";

pub const DEFAULT_STUCK_MS: u64 = 2_000;

const PS_PER_MS: u64 = 1_000_000_000;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum StuckKind {
    /// A confirmed loop on a register nothing can change: a store-only stub, a disabled model, or a
    /// class-U register of a block with no pending event. Decided at the confirmation.
    #[default]
    Unchangeable,
    /// A confirmed loop on a modeled register unchanged for more than `stuck_ms`, across events.
    Unchanged,
    /// A loop the tracker cannot confirm because it stores: a pc in one 64-byte window re-reading
    /// one register with one value for more than `stuck_ms`, with no idle in between.
    Fallback,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StuckReport {
    pub kind: StuckKind,
    pub block: &'static str,
    pub register: &'static str,
    pub off: u32,
    pub addr: u32,
    pub val: u32,
    pub pc: u32,
    pub symbol: Option<String>,
    /// `id` of the matching `[[wait]]` row, such as `spi2.cmd_update`.
    pub wait_row: Option<&'static str>,
    /// That row's `expect` column: the completion the model should produce.
    pub expect: Option<&'static str>,
    pub since: VTime,
    pub at: VTime,
}

/// Hang detector configuration (`MachineConfig::hang`). It decides where a run stops, so it is
/// part of run identity.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HangCfg {
    pub enabled: bool,
    /// Virtual milliseconds an unchanged read may repeat before `Unchanged` and `Fallback` fire.
    pub stuck_ms: u64,
}

impl Default for HangCfg {
    fn default() -> HangCfg {
        HangCfg {
            enabled: true,
            stuck_ms: DEFAULT_STUCK_MS,
        }
    }
}

impl HangCfg {
    pub fn stuck_ps(&self) -> u64 {
        self.stuck_ms.saturating_mul(PS_PER_MS)
    }
}

pub fn regs_of(block: &str) -> Option<&'static [RegSpec]> {
    use pemu_soc_c3::r#gen as g;
    Some(match block {
        "apb_ctrl" => &g::regs_apb_ctrl::REGS,
        "assist_debug" => &g::regs_assist_debug::REGS,
        "efuse" => &g::regs_efuse::REGS,
        "extmem" => &g::regs_extmem::REGS,
        "gdma" => &g::regs_gdma::REGS,
        "gpio" => &g::regs_gpio::REGS,
        "i2c0" => &g::regs_i2c0::REGS,
        "i2s0" => &g::regs_i2s0::REGS,
        "intc" => &g::regs_intc::REGS,
        "ledc" => &g::regs_ledc::REGS,
        "rmt" => &g::regs_rmt::REGS,
        "rtc_cntl" => &g::regs_rtc_cntl::REGS,
        "saradc" => &g::regs_saradc::REGS,
        "sensitive" => &g::regs_sensitive::REGS,
        "spi0" => &g::regs_spi0::REGS,
        "spi1" => &g::regs_spi1::REGS,
        "spi2" => &g::regs_spi2::REGS,
        "system" => &g::regs_system::REGS,
        "systimer" => &g::regs_systimer::REGS,
        "timg0" => &g::regs_timg0::REGS,
        "timg1" => &g::regs_timg1::REGS,
        "uart0" => &g::regs_uart0::REGS,
        "uart1" => &g::regs_uart1::REGS,
        "uhci0" => &g::regs_uhci0::REGS,
        "usj" => &g::regs_usj::REGS,
        "xts_aes" => &g::regs_xts_aes::REGS,
        _ => return None,
    })
}

pub fn register_at(block: &str, off: u32) -> Option<&'static RegSpec> {
    let off = off & !3;
    regs_of(block)?.iter().find(|r| u32::from(r.off) == off)
}

/// Bit mask of the fields a wait row names in `reg`; `*` means every field.
fn row_mask(row: &WaitSpec, reg: &RegSpec) -> u32 {
    if row.field.trim() == "*" {
        return u32::MAX;
    }
    row.field
        .split(',')
        .map(str::trim)
        .filter_map(|name| reg.fields.iter().find(|f| f.name == name))
        .fold(0u32, |mask, f| {
            let ones = if f.width >= 32 {
                u32::MAX
            } else {
                (1u32 << f.width) - 1
            };
            mask | ones.checked_shl(u32::from(f.shift)).unwrap_or(0)
        })
}

/// The `[[wait]]` row a guest reading `val` from `block` at `off` is polling.
///
/// Two rows can name one register (`spi2.cmd_update` and `spi2.cmd_usr` both poll `SPI_CMD`): the
/// first whose field bits are set in `val` wins, because a guest waiting for a self-clearing bit
/// reads it set; else the first. A block without a generated table matches its only row, if any.
pub fn wait_row(block: &str, off: u32, val: u32) -> Option<&'static WaitSpec> {
    let rows: Vec<&'static WaitSpec> = waits::of_block(block).collect();
    let Some(reg) = register_at(block, off) else {
        return match rows.as_slice() {
            [only] => Some(only),
            _ => None,
        };
    };
    let on_reg: Vec<&'static WaitSpec> = rows
        .into_iter()
        .filter(|r| r.register == reg.name)
        .collect();
    on_reg
        .iter()
        .find(|r| val & row_mask(r, reg) != 0)
        .or_else(|| on_reg.first())
        .copied()
}

impl Machine {
    /// Sets `MachineConfig::hang`. It changes run identity; call it before the first run.
    pub fn set_hang_detector(&mut self, cfg: HangCfg) {
        self.cfg.hang = cfg;
        self.poll.hang = cfg;
    }

    pub fn hang_detector(&self) -> HangCfg {
        self.poll.hang
    }

    pub(crate) fn stuck_report(&self, hit: HangHit) -> StuckReport {
        let (block, off) = match lookup(hit.addr) {
            Some((id, off)) => (BLOCKS[usize::from(id.0)].name, off),
            None => (
                UNMAPPED_BLOCK,
                hit.addr.wrapping_sub(pemu_soc_c3::mem::MMIO_BASE),
            ),
        };
        let register = register_at(block, off).map_or("", |r| r.name);
        let row = wait_row(block, off, hit.val);
        StuckReport {
            kind: hit.kind,
            block,
            register,
            off: off & !3,
            addr: hit.addr,
            val: hit.val,
            pc: hit.pc,
            symbol: self.symbol_at(hit.pc),
            wait_row: row.map(|r| r.id),
            expect: row.map(|r| r.expect),
            since: hit.since,
            at: hit.at,
        }
    }

    fn symbol_at(&self, pc: u32) -> Option<String> {
        let a = &self.assets;
        [
            Some(a.rom.symbols()),
            a.boot_elf.as_deref().map(|e| &e.symbols),
            a.app_elf.as_deref().map(|e| &e.symbols),
        ]
        .into_iter()
        .flatten()
        .find_map(|t| t.func_at(pc).map(|s| s.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_detector_is_on_with_two_seconds() {
        let cfg = HangCfg::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.stuck_ms, 2_000);
        assert_eq!(cfg.stuck_ps(), 2_000_000_000_000);
    }

    #[test]
    fn spi_cmd_with_update_set_names_the_update_row() {
        // SPI_UPDATE is bit 23 and SPI_USR bit 24 of SPI_CMD.
        let row = wait_row("spi2", 0x000, 1 << 23).expect("a row for SPI_CMD");
        assert_eq!(row.id, "spi2.cmd_update");
        let row = wait_row("spi2", 0x000, 1 << 24).expect("a row for SPI_CMD");
        assert_eq!(row.id, "spi2.cmd_usr");
    }

    #[test]
    fn a_register_no_row_names_has_no_row() {
        assert!(wait_row("spi2", 0x004, 0).is_none());
        assert!(wait_row("world_cntl", 0x000, 0).is_none());
    }

    #[test]
    fn every_row_with_a_generated_register_resolves_to_itself() {
        for row in waits::WAITS.iter() {
            let Some(regs) = regs_of(row.block) else {
                continue;
            };
            let Some(reg) = regs.iter().find(|r| r.name == row.register) else {
                continue;
            };
            let mask = row_mask(row, reg);
            let found = wait_row(row.block, u32::from(reg.off), mask).expect(row.id);
            assert_eq!(found.register, row.register, "{}", row.id);
        }
    }
}
