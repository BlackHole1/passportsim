//! A synthetic guest program for this crate's unit tests.
//!
//! `pemu-hle` cannot depend on `pemu-machine`, so unit tests that need a guest drive this one. It
//! lays out what the guards read (a TCB's `pxStack`, the `xIsrStackBottom`/`xIsrStackTop`
//! variables of the IDF RISC-V FreeRTOS port), so headroom tests exercise the real read path.

use std::collections::BTreeMap;

use pemu_core::irq_source::IrqSource;
use pemu_core::sched::{EventHandle, EventKey, Scheduler};
use pemu_core::time::VTime;
use pemu_rv32::trap::Trap;

use crate::guest_call::{GuardProfile, GuestView, StackSymbols};

pub const TEXT: u32 = 0x4200_0000;
pub const DATA: u32 = 0x3FC8_0000;
/// IDF `CONFIG_ESP_SYSTEM_ISR_STACK_SIZE` default.
pub const ISR_STACK_BYTES: u32 = 1536;

/// Registers, sparse memory, symbols and the FreeRTOS predicates of `GuestView`.
#[derive(Clone, Debug, Default)]
pub struct SynthGuest {
    regs: [u32; 32],
    mem: BTreeMap<u32, u8>,
    syms: BTreeMap<String, u32>,
    /// `port_uxInterruptNesting != 0`.
    pub in_isr: bool,
    pub scheduler_running: bool,
    pub current_task: u32,
    /// Levels `GuestView::raise` set, newest last: `(source number, level)`.
    pub raised: Vec<(u32, bool)>,
    pub now: VTime,
    /// Addresses no read or write may touch, so a test can provoke a guest fault.
    pub unmapped: Vec<u32>,
    pub sched: Scheduler,
    pub scheduled: Vec<(VTime, EventKey)>,
    /// Seed 0.
    pub rng: SynthRng,
}

/// A [`pemu_core::rng::DetRng`] with a default, for [`SynthGuest`]'s derived `Default`.
#[derive(Clone, Debug)]
pub struct SynthRng(pub pemu_core::rng::DetRng);

impl Default for SynthRng {
    fn default() -> SynthRng {
        SynthRng(pemu_core::rng::DetRng::new(0))
    }
}

impl SynthGuest {
    pub fn new() -> SynthGuest {
        SynthGuest {
            scheduler_running: true,
            ..SynthGuest::default()
        }
    }

    pub fn define(&mut self, name: &str, addr: u32) -> &mut SynthGuest {
        self.syms.insert(name.to_string(), addr);
        self
    }

    /// Writes a little-endian word, bypassing the fault list.
    pub fn poke(&mut self, addr: u32, value: u32) -> &mut SynthGuest {
        for (i, byte) in value.to_le_bytes().iter().enumerate() {
            self.mem.insert(addr + i as u32, *byte);
        }
        self
    }

    /// Reads a little-endian word, bypassing the fault list.
    pub fn peek(&self, addr: u32) -> u32 {
        let mut word = [0u8; 4];
        for (i, slot) in word.iter_mut().enumerate() {
            *slot = self.mem.get(&(addr + i as u32)).copied().unwrap_or(0);
        }
        u32::from_le_bytes(word)
    }

    pub fn bytes(&self, addr: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                self.mem
                    .get(&(addr + i as u32))
                    .copied()
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Lays out one task: a TCB at `tcb` whose `pxStack` (at the profile's offset) is `stack`,
    /// with `sp` set to `sp`. Makes it the current task.
    pub fn with_task(
        &mut self,
        guards: &GuardProfile,
        tcb: u32,
        stack: u32,
        sp: u32,
    ) -> &mut SynthGuest {
        self.poke(tcb + guards.tcb_px_stack, stack);
        self.current_task = tcb;
        self.set_reg(2, sp);
        self
    }

    /// Lays out the ISR stack of the RISC-V port: the two pointer variables and `sp` on it.
    pub fn with_isr_stack(&mut self, bottom: u32, sp: u32) -> &mut SynthGuest {
        let bottom_at = DATA + 0x100;
        let top_at = DATA + 0x104;
        self.define("xIsrStackBottom", bottom_at);
        self.define("xIsrStackTop", top_at);
        self.poke(bottom_at, bottom);
        self.poke(top_at, bottom + ISR_STACK_BYTES);
        self.in_isr = true;
        self.set_reg(2, sp);
        self
    }

    pub fn stack_symbols(&self) -> StackSymbols {
        StackSymbols::resolve(self)
    }
}

impl GuestView for SynthGuest {
    fn draw_entropy(&mut self, stream: pemu_core::rng::RngStream, out: &mut [u8]) {
        self.rng.0.stream(stream).fill_bytes(out);
    }

    fn reg(&self, r: u8) -> u32 {
        if r == 0 { 0 } else { self.regs[usize::from(r)] }
    }

    fn set_reg(&mut self, r: u8, v: u32) {
        if r != 0 {
            self.regs[usize::from(r)] = v;
        }
    }

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Trap> {
        self.fault_check(addr, buf.len())?;
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = self
                .mem
                .get(&addr.wrapping_add(i as u32))
                .copied()
                .unwrap_or_default();
        }
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Trap> {
        self.fault_check(addr, data.len())?;
        for (i, byte) in data.iter().enumerate() {
            self.mem.insert(addr.wrapping_add(i as u32), *byte);
        }
        Ok(())
    }

    fn raise(&mut self, s: IrqSource, level: bool) {
        self.raised.push((u32::from(s.0), level));
    }

    fn schedule(&mut self, at: VTime, key: EventKey) -> EventHandle {
        self.scheduled.push((at, key));
        let now = self.now;
        self.sched.schedule(now, at, key)
    }

    fn now(&self) -> VTime {
        self.now
    }

    fn symbol(&self, name: &str) -> Option<u32> {
        self.syms.get(name).copied()
    }

    fn current_task(&mut self) -> u32 {
        self.current_task
    }

    fn in_isr(&mut self) -> bool {
        self.in_isr
    }

    fn scheduler_running(&mut self) -> bool {
        self.scheduler_running
    }
}

impl SynthGuest {
    fn fault_check(&self, addr: u32, len: usize) -> Result<(), Trap> {
        let hit = self
            .unmapped
            .iter()
            .any(|bad| *bad >= addr && u64::from(*bad) < u64::from(addr) + len as u64);
        if hit {
            Err(Trap::load_access_fault(addr))
        } else {
            Ok(())
        }
    }
}
