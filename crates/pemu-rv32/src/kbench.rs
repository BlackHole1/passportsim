//! The machine benchmark workload K runs on: the kernels of `bench/kernels/` on this crate's hart
//! (ESP32-C3 TRM chapter 1) in 1 MB of flat RAM, driven through [`Engine`] in slices as the run
//! loop drives it.
//!
//! It lives here so that `cargo xtask bench-k` natively and `examples/kbench_wasm.rs` in the
//! browser run the same machine. Nothing here reads a clock: the caller times
//! [`KernelMachine::run`] alone.

use crate::bus::{Access, Bus, CodePage, HartView, PF_CODE, PF_R, PF_W, PF_X, PageTable};
use crate::csr::{Csr, CsrEffect, CsrOp};
use crate::engine::{Engine, EngineCfg, EngineStats, Exit, HookId, HookSet};
use crate::exec::Hart;
use crate::spmon::SpMonitor;
use crate::trap::Trap;

/// Guest RAM of the kernels' linker script (`bench/kernels/link.ld`).
pub const RAM_BASE: u32 = 0x4038_0000;
pub const RAM_SIZE: u32 = 0x10_0000;
const PAGE: u32 = 4096;

/// `li a7, 93` followed by `ecall`, the exit sequence of `bench/kernels/start.S`. The runner binds
/// a hook at the `ecall` so the kernel's return reaches it as [`Exit::Hook`] instead of a trap.
pub const EXIT_SEQUENCE: [u8; 8] = [0x93, 0x08, 0xD0, 0x05, 0x73, 0x00, 0x00, 0x00];

const EXIT_HOOK: HookId = HookId(1);

/// The engine retires one instruction fewer than the spike: its hook exits before the exit
/// `ecall`, which the spike executes.
pub const EXIT_ECALL: u64 = 1;

/// 1 MB of RAM with a page table. Image pages lack `PF_W`, so every store into them takes the
/// slow path, as in the spike, whose `.data`/`.bss` share the last text page. The slow path does
/// not invalidate: the kernels never write their own text, which the checksum anchors verify.
struct KernelBus {
    arena: Vec<u8>,
    pages: PageTable,
    slow_loads: u64,
    slow_stores: u64,
}

impl KernelBus {
    fn new(image: &[u8]) -> KernelBus {
        let mut arena = vec![0u8; RAM_SIZE as usize];
        arena[..image.len()].copy_from_slice(image);
        let mut pages = PageTable::new();
        let image_pages = (image.len() as u32).div_ceil(PAGE);
        for p in 0..RAM_SIZE / PAGE {
            let flags = if p < image_pages {
                PF_R | PF_X | PF_CODE
            } else {
                PF_R | PF_W
            };
            pages.set_entry((RAM_BASE >> 12) + p, (p * PAGE) | flags);
        }
        KernelBus {
            arena,
            pages,
            slow_loads: 0,
            slow_stores: 0,
        }
    }

    #[inline]
    fn offset(&self, addr: u32, size: u8) -> Option<usize> {
        let off = addr.checked_sub(RAM_BASE)? as usize;
        (off + usize::from(size) <= self.arena.len()).then_some(off)
    }
}

impl Bus for KernelBus {
    fn pages(&self) -> &PageTable {
        &self.pages
    }

    fn arena(&mut self) -> *mut u8 {
        self.arena.as_mut_ptr()
    }

    fn load_slow(&mut self, addr: u32, size: u8, _hart: &HartView) -> Access<u32> {
        self.slow_loads += 1;
        match self.offset(addr, size) {
            Some(off) => {
                let mut v = 0u32;
                for i in (0..usize::from(size)).rev() {
                    v = (v << 8) | u32::from(self.arena[off + i]);
                }
                Access::Ok(v)
            }
            None => Access::Fault(Trap::load_access_fault(addr)),
        }
    }

    fn store_slow(&mut self, addr: u32, size: u8, val: u32, _hart: &HartView) -> Access<()> {
        self.slow_stores += 1;
        let Some(off) = self.offset(addr, size) else {
            return Access::Fault(Trap::store_access_fault(addr));
        };
        for i in 0..usize::from(size) {
            self.arena[off + i] = (val >> (8 * i)) as u8;
        }
        Access::Ok(())
    }

    fn sp_monitor(&self) -> SpMonitor {
        SpMonitor::default()
    }

    fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
        let Some(off) = self.offset(vaddr, 1) else {
            return Err(Trap::instruction_access_fault(vaddr));
        };
        let end = (off / PAGE as usize + 1) * PAGE as usize;
        Ok(CodePage {
            bytes: &self.arena[off..end],
        })
    }

    fn csr_custom(&mut self, _csr: u16, _op: CsrOp, _insns: u64) -> Result<(u32, CsrEffect), Trap> {
        Ok((0, CsrEffect::None))
    }

    fn wfi_wake(&mut self) -> bool {
        false
    }

    fn pmp_changed(&mut self, _csr: &Csr) {}
}

pub fn exit_pc(image: &[u8]) -> Result<u32, String> {
    image
        .windows(EXIT_SEQUENCE.len())
        .position(|w| w == EXIT_SEQUENCE)
        .map(|at| RAM_BASE + at as u32 + 4)
        .ok_or_else(|| {
            "the kernel image has no `li a7, 93; ecall` exit sequence; \
             bench/kernels/start.S and this file must agree"
                .to_string()
        })
}

/// One kernel, loaded and ready to run once; building it is not part of a measured run.
pub struct KernelMachine {
    bus: KernelBus,
    hart: Hart,
    engine: Engine,
    hooks: HookSet,
}

impl KernelMachine {
    /// The kernel takes its iteration count in `a0` and returns its checksum there.
    pub fn new(image: &[u8], iters: u32, max_block_insns: u16) -> Result<KernelMachine, String> {
        if image.len() > RAM_SIZE as usize {
            return Err(format!(
                "the kernel image is {} bytes, over the {RAM_SIZE}-byte RAM of link.ld",
                image.len()
            ));
        }
        let mut hooks = HookSet::default();
        hooks.insert(exit_pc(image)?, EXIT_HOOK);
        let mut hart = Hart {
            x: [0; 32],
            pc: RAM_BASE,
            csr: Csr::new(),
            wfi: false,
            insns: 0,
            stores: 0,
            spmon: SpMonitor::default(),
            extra: 0,
            pipe: Default::default(),
        };
        hart.x[10] = iters;
        Ok(KernelMachine {
            bus: KernelBus::new(image),
            hart,
            engine: Engine::new(EngineCfg {
                max_block_insns,
                ..EngineCfg::default()
            }),
            hooks,
        })
    }

    /// Runs the kernel to its exit in engine calls of at most `slice` instructions, so the
    /// exact-budget path and the in-block resume token are on the measured path.
    pub fn run(&mut self, slice: u64) -> Result<(), String> {
        loop {
            match self
                .engine
                .run(&mut self.hart, &mut self.bus, &self.hooks, slice)
            {
                Exit::Hook { id } if id == EXIT_HOOK => return Ok(()),
                Exit::Budget | Exit::Stop => {}
                other => {
                    return Err(format!(
                        "the kernel ended with {other:?} at pc 0x{:08x} after {} instructions",
                        self.hart.pc, self.hart.insns
                    ));
                }
            }
        }
    }

    pub fn insns(&self) -> u64 {
        self.hart.insns
    }

    /// Meaningful once [`KernelMachine::run`] is done.
    pub fn checksum(&self) -> u32 {
        self.hart.x[10]
    }

    pub fn slow_stores(&self) -> u64 {
        self.bus.slow_stores
    }

    pub fn stats(&self) -> EngineStats {
        self.engine.stats()
    }
}
