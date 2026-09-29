//! [`MachineBus`], the bus the executor runs against: [`SocBus`] plus what the run loop observes
//! at each access.

use pemu_core::hostio::{HostIo, SerialStream};
use pemu_core::sched::PeriphId;
use pemu_rv32::bus::{Access, Bus, CodePage, HartView, PageTable};
use pemu_rv32::csr::{Csr, CsrEffect, CsrOp};
use pemu_rv32::spmon::SpMonitor;
use pemu_rv32::trap::Trap;
use pemu_soc_c3::SocBus;
use pemu_soc_c3::mem;
use pemu_soc_c3::periph::{self, Peripheral};

use crate::disable::DisabledModels;
use crate::poll_ff::PollTracker;
use crate::stops::ArmedStops;

/// The UART0 register window, whose `UART_FIFO` writes reach the model's transmit ring.
const UART0_BASE: u32 = <pemu_soc_c3::periph::uart0::Model as Peripheral>::BASE;
const UART0_SIZE: u32 = <pemu_soc_c3::periph::uart0::Model as Peripheral>::SIZE;

/// [`SocBus`] plus what the run loop observes at the bus, which never decides a value the guest
/// reads. Any access that moved `IrqFabric::epoch` ends the slice (`Ok` becomes `OkStop`), so the
/// interrupt it made deliverable is taken at the next instruction.
pub struct MachineBus<'a> {
    pub(crate) inner: SocBus<'a>,
    pub(super) io: &'a mut HostIo,
    pub(super) tap: &'a mut ConsoleTap,
    pub(super) stops: &'a mut ArmedStops,
    pub(super) poll: &'a mut PollTracker,
    pub(super) disabled: &'a mut DisabledModels,
    pub(super) radio: &'a mut pemu_hle::tripwire::RadioMmioWatch,
    pub(super) radio_trip: &'a mut Option<crate::hle::PendingRadioTrip>,
}

/// The radio rows of the `c3_devices!` table. `pemu-hle` reads the same windows from
/// `specs/hle/idf-5.5.3/tripwires.toml`; `the_radio_windows_agree_across_their_definitions` ties
/// them together.
const RADIO_BLOCKS: [PeriphId; 5] = [
    periph::id::RADIO_FE2,
    periph::id::RADIO_FE,
    periph::id::RADIO_NRX,
    periph::id::RADIO_BB,
    periph::id::RADIO_BLE,
];

const RADIO_SPAN: (u32, u32) = radio_span();

const fn radio_span() -> (u32, u32) {
    let (mut lo, mut hi) = (u32::MAX, 0);
    let mut i = 0;
    while i < RADIO_BLOCKS.len() {
        let b = periph::BLOCKS[RADIO_BLOCKS[i].0 as usize];
        if b.base < lo {
            lo = b.base;
        }
        if b.base + b.size > hi {
            hi = b.base + b.size;
        }
        i += 1;
    }
    (lo, hi)
}

impl MachineBus<'_> {
    /// Checks one CPU access against the radio MMIO tripwire; `true` when it tripped.
    fn radio_access(&mut self, addr: u32, pc: u32) -> bool {
        if !(RADIO_SPAN.0..RADIO_SPAN.1).contains(&addr) {
            return false;
        }
        let Some(hit) = self.radio.access(addr) else {
            return false;
        };
        self.radio_trip.get_or_insert(crate::hle::PendingRadioTrip {
            pc,
            detail: hit.detail,
        });
        true
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MmioRead {
    pub addr: u32,
    pub pc: u32,
    /// Consecutive reads of this address by this pc, this one included.
    pub repeats: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConsoleTap {
    pub(super) last_read: Option<MmioRead>,
}

impl ConsoleTap {
    /// Adds `k` repeats the poll fast-forward skipped, so the report is the same with it on or off.
    pub(crate) fn credit_repeats(&mut self, k: u64) {
        if let Some(r) = self.last_read.as_mut() {
            r.repeats = r.repeats.saturating_add(k);
        }
    }

    fn note_read(&mut self, addr: u32, pc: u32) {
        self.last_read = Some(match self.last_read {
            Some(prev) if prev.addr == addr && prev.pc == pc => MmioRead {
                repeats: prev.repeats.saturating_add(1),
                ..prev
            },
            _ => MmioRead {
                addr,
                pc,
                repeats: 1,
            },
        });
    }
}

impl Bus for MachineBus<'_> {
    fn pages(&self) -> &PageTable {
        self.inner.pages()
    }

    fn arena(&mut self) -> *mut u8 {
        self.inner.arena()
    }

    fn load_slow(&mut self, addr: u32, size: u8, hart: &HartView) -> Access<u32> {
        let epoch = self.inner.cx.periph.irq.epoch();
        let out = match self.disabled.load(addr, size, hart, &mut self.inner.cx) {
            Some(v) => Access::Ok(v),
            None => self.inner.load_slow(addr, size, hart),
        };
        let mut stop = self.inner.cx.periph.irq.epoch() != epoch;
        stop |= self.radio_access(addr, hart.pc);
        // Any stop the access asked for ends a poll chain, whatever the address.
        if stop || matches!(out, Access::OkStop(_)) {
            self.poll.invalidate();
        }
        if mem::is_mmio(addr) {
            self.tap.note_read(addr, hart.pc);
            if let Access::Ok(v) | Access::OkStop(v) = out {
                let cx = &mut self.inner.cx;
                cx.periph
                    .trace
                    .mmio_read(hart.insns, hart.pc, addr, v, size);
                // A read the model asked to stop on changed something, so it cannot repeat the next one.
                stop |= self.poll.on_read(
                    hart.pc,
                    addr,
                    v,
                    size,
                    hart.insns,
                    hart.extra,
                    cx.now,
                    cx.periph.irq.epoch(),
                    cx.clock.stall_ps(),
                );
            }
        }
        match out {
            Access::Ok(v) if stop => Access::OkStop(v),
            other => other,
        }
    }

    fn store_slow(&mut self, addr: u32, size: u8, val: u32, hart: &HartView) -> Access<()> {
        let epoch = self.inner.cx.periph.irq.epoch();
        let out = if self
            .disabled
            .store(addr, size, val, hart, &mut self.inner.cx)
        {
            Access::Ok(())
        } else {
            self.inner.store_slow(addr, size, val, hart)
        };
        if mem::is_mmio(addr) {
            self.poll.invalidate();
            self.inner
                .cx
                .periph
                .trace
                .mmio_write(hart.insns, hart.pc, addr, val, size);
        }
        let mut stop = self.inner.cx.periph.irq.epoch() != epoch;
        stop |= self.radio_access(addr, hart.pc);
        if addr.wrapping_sub(UART0_BASE) < UART0_SIZE {
            let tx = self.inner.soc.devices.uart0.take_tx();
            if !tx.is_empty() {
                let now = self.inner.cx.now;
                self.io.serial_write(SerialStream::Uart0Tx, &tx, now);
                stop |= self.stops.wants_uart0_lines() && tx.contains(&b'\n');
            }
        } else if matches!(out, Access::Ok(()) | Access::OkStop(()))
            && self.stops.watched(addr, size)
        {
            self.stops.watch_hit.get_or_insert((addr, hart.pc));
            stop = true;
        }
        if stop || matches!(out, Access::OkStop(())) {
            self.poll.invalidate();
        }
        match out {
            Access::Ok(()) if stop => Access::OkStop(()),
            other => other,
        }
    }

    fn sp_monitor(&self) -> SpMonitor {
        self.inner.sp_monitor()
    }

    fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
        self.inner.fetch_code(vaddr)
    }

    fn csr_custom(&mut self, csr: u16, op: CsrOp, insns: u64) -> Result<(u32, CsrEffect), Trap> {
        // Custom CSRs are the performance counter and its controls, derived from the clock.
        self.poll.invalidate();
        self.inner.csr_custom(csr, op, insns)
    }

    /// Any pending INTC line, ignoring MIE.
    fn wfi_wake(&mut self) -> bool {
        self.inner.wfi_wake()
    }

    fn pmp_changed(&mut self, csr: &Csr) {
        self.inner.pmp_changed(csr)
    }

    fn fetch_enter(&mut self, insns: u64, pc: u32) -> bool {
        self.inner.fetch_enter(insns, pc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_outside_the_peripheral_window_leaves_the_tap_alone() {
        let mut tap = ConsoleTap::default();
        tap.note_read(0x6000_001C, 0x4004_9b98);
        tap.note_read(0x6000_001C, 0x4004_9b98);
        assert_eq!(
            tap.last_read,
            Some(MmioRead {
                addr: 0x6000_001C,
                pc: 0x4004_9b98,
                repeats: 2,
            })
        );
        tap.note_read(0x6000_001C, 0x4004_9b9c);
        assert_eq!(tap.last_read.expect("a read was noted").repeats, 1);
    }

    /// A window moved in only one of the three definitions would leave radio accesses unwatched.
    #[test]
    fn the_radio_windows_agree_across_their_definitions() {
        let mut table: Vec<(&str, u32, u32)> = RADIO_BLOCKS
            .iter()
            .map(|id| {
                let b = periph::BLOCKS[id.0 as usize];
                (b.name, b.base, b.size)
            })
            .collect();
        table.sort_by_key(|row| row.1);
        let mut named: Vec<(&str, u32, u32)> = periph::BLOCKS
            .iter()
            .filter(|b| b.name.starts_with("radio_"))
            .map(|b| (b.name, b.base, b.size))
            .collect();
        named.sort_by_key(|row| row.1);
        assert_eq!(
            table, named,
            "RADIO_BLOCKS lists every radio row of the table"
        );

        let mut spec: Vec<(&str, u32, u32)> = pemu_hle::tripwire::TripwireSpec::load()
            .ranges
            .iter()
            .map(|r| (r.name, r.base, r.size))
            .collect();
        spec.sort_by_key(|row| row.1);
        assert_eq!(spec, table, "tripwires.toml mirrors the c3_devices! rows");

        for (name, base, size) in &table {
            assert!(
                RADIO_SPAN.0 <= *base && base + size <= RADIO_SPAN.1,
                "{name} outside the bus span"
            );
        }
        assert_eq!(RADIO_SPAN, (0x6000_5000, 0x6003_2000));
    }
}
