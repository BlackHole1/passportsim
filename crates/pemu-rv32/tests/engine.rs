//! The riscv-tests `p` suites through the block engine at `max_block_insns` 1, 3 and 64, plus the
//! translation-count invariant. `tests/riscv_tests.rs` documents the ELFs and the `tohost`
//! protocol; this file carries its own minimal copy of that reader.
//!
//! Every page is `PF_R | PF_W | PF_X | PF_CODE`: loads take the inlined fast path `ref_step` never
//! takes, and every store takes the slow path, answering `OkStop` so the harness can invalidate
//! the page as the SoC would (`rv32ui-p-fence_i` needs this at every block size).

// `clippy.toml` bans file I/O in core crates; test code may read the committed ELFs.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pemu_rv32::bus::{Access, Bus, CodePage, HartView, PF_CODE, PF_R, PF_W, PF_X, PageTable};
use pemu_rv32::csr::{Csr, CsrEffect, CsrOp};
use pemu_rv32::engine::{Engine, EngineCfg, Exit, HookSet};
use pemu_rv32::exec::Hart;
use pemu_rv32::spmon::SpMonitor;
use pemu_rv32::trap::Trap;

const PASS_VALUE: u32 = 1;

/// The whole suite retires under 20 thousand instructions, so this is ample headroom per test.
const INSN_LIMIT: u64 = 2_000_000;

const BLOCK_SIZES: [u16; 3] = [1, 3, 64];

const PAGE: u32 = 4096;

struct TestBus {
    base: u32,
    ram: Vec<u8>,
    pages: PageTable,
    tohost: u32,
    verdict: Option<u32>,
    custom: BTreeMap<u16, u32>,
    /// Addresses written since the last drain, for the harness to invalidate.
    dirty: Vec<u32>,
}

impl TestBus {
    fn new(image: Image) -> TestBus {
        let mut pages = PageTable::new();
        let flags = PF_R | PF_W | PF_X | PF_CODE;
        for p in 0..(image.ram.len() as u32).div_ceil(PAGE) {
            pages.set_entry((image.base >> 12) + p, (p * PAGE) | flags);
        }
        TestBus {
            base: image.base,
            ram: image.ram,
            pages,
            tohost: image.tohost,
            verdict: None,
            custom: BTreeMap::new(),
            dirty: Vec::new(),
        }
    }

    fn offset(&self, addr: u32, size: u8) -> Option<usize> {
        let off = addr.checked_sub(self.base)? as usize;
        (off + usize::from(size) <= self.ram.len()).then_some(off)
    }

    fn take_dirty(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.dirty)
    }
}

impl Bus for TestBus {
    fn pages(&self) -> &PageTable {
        &self.pages
    }

    fn arena(&mut self) -> *mut u8 {
        self.ram.as_mut_ptr()
    }

    fn load_slow(&mut self, addr: u32, size: u8, _hart: &HartView) -> Access<u32> {
        match self.offset(addr, size) {
            Some(off) => {
                let mut value = 0u32;
                for i in (0..usize::from(size)).rev() {
                    value = (value << 8) | u32::from(self.ram[off + i]);
                }
                Access::Ok(value)
            }
            None => Access::Fault(Trap::load_access_fault(addr)),
        }
    }

    fn store_slow(&mut self, addr: u32, size: u8, val: u32, _hart: &HartView) -> Access<()> {
        let Some(off) = self.offset(addr, size) else {
            return Access::Fault(Trap::store_access_fault(addr));
        };
        for i in 0..usize::from(size) {
            self.ram[off + i] = (val >> (8 * i)) as u8;
        }
        if addr == self.tohost && self.verdict.is_none() {
            self.verdict = Some(val);
        }
        // Record both ends, so a store straddling a page invalidates both pages.
        self.dirty.push(addr);
        self.dirty.push(addr + u32::from(size) - 1);
        Access::OkStop(())
    }

    fn sp_monitor(&self) -> SpMonitor {
        SpMonitor::default()
    }

    fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
        let Some(off) = self.offset(vaddr, 1) else {
            return Err(Trap::instruction_access_fault(vaddr));
        };
        let page_end = (off / PAGE as usize + 1) * PAGE as usize;
        Ok(CodePage {
            bytes: &self.ram[off..page_end.min(self.ram.len())],
        })
    }

    fn csr_custom(&mut self, csr: u16, op: CsrOp, _insns: u64) -> Result<(u32, CsrEffect), Trap> {
        let slot = self.custom.entry(csr).or_default();
        let before = *slot;
        if let CsrOp::Write(value) = op {
            *slot = value;
        }
        Ok((before, CsrEffect::None))
    }

    fn wfi_wake(&mut self) -> bool {
        false
    }

    fn pmp_changed(&mut self, _csr: &Csr) {}
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass {
        insns: u64,
        blocks_built: u64,
        runs: u64,
    },
    Failed {
        value: u32,
        subtest: u32,
    },
    Hung {
        pc: u32,
    },
    Halted {
        pc: u32,
        exit: String,
    },
}

impl Outcome {
    fn passed(&self) -> bool {
        matches!(self, Outcome::Pass { .. })
    }
}

/// Runs in budgets of `slice` instructions, exercising partial blocks and in-block resume.
fn run_elf(bytes: &[u8], max_block_insns: u16, slice: u64) -> Result<Outcome, String> {
    let image = Image::parse(bytes)?;
    let entry = image.entry;
    let mut bus = TestBus::new(image);
    let mut hart = Hart {
        x: [0; 32],
        pc: entry,
        csr: Csr::new(),
        wfi: false,
        insns: 0,
        stores: 0,
        spmon: SpMonitor::default(),
        extra: 0,
        pipe: Default::default(),
    };
    let mut engine = Engine::new(EngineCfg {
        max_block_insns,
        ..EngineCfg::default()
    });
    let hooks = HookSet::default();
    let mut runs = 0u64;
    while hart.insns < INSN_LIMIT {
        let exit = engine.run(&mut hart, &mut bus, &hooks, slice);
        runs += 1;
        for addr in bus.take_dirty() {
            engine.invalidate_vrange(addr, 1);
        }
        if let Some(value) = bus.verdict {
            return Ok(if value == PASS_VALUE {
                Outcome::Pass {
                    insns: hart.insns,
                    blocks_built: engine.stats().blocks_built,
                    runs,
                }
            } else {
                Outcome::Failed {
                    value,
                    subtest: value >> 1,
                }
            });
        }
        match exit {
            // No interrupt can wake this bus, so a `wfi` is a hang.
            Exit::Wfi => return Ok(Outcome::Hung { pc: hart.pc }),
            Exit::Budget | Exit::Stop | Exit::SpSpill(_) => {}
            other => {
                return Ok(Outcome::Halted {
                    pc: hart.pc,
                    exit: format!("{other:?}"),
                });
            }
        }
    }
    Ok(Outcome::Hung { pc: hart.pc })
}

struct Image {
    base: u32,
    ram: Vec<u8>,
    entry: u32,
    tohost: u32,
}

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const EM_RISCV: u16 = 243;

impl Image {
    fn parse(bytes: &[u8]) -> Result<Image, String> {
        if bytes.get(..4) != Some(b"\x7fELF") {
            return Err("not an ELF file".to_string());
        }
        if bytes.get(4) != Some(&1) || bytes.get(5) != Some(&1) {
            return Err("not a little-endian ELF32 file".to_string());
        }
        if u16(bytes, 18)? != EM_RISCV {
            return Err("not an EM_RISCV file".to_string());
        }
        let entry = u32(bytes, 24)?;
        let phoff = u32(bytes, 28)? as usize;
        let phentsize = u16(bytes, 42)? as usize;
        let phnum = u16(bytes, 44)? as usize;

        let mut lo = u32::MAX;
        let mut hi = 0u32;
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if u32(bytes, ph)? != PT_LOAD {
                continue;
            }
            let vaddr = u32(bytes, ph + 8)?;
            let memsz = u32(bytes, ph + 20)?;
            lo = lo.min(vaddr);
            hi = hi.max(
                vaddr
                    .checked_add(memsz)
                    .ok_or("segment wraps the address space")?,
            );
        }
        if lo > hi {
            return Err("no PT_LOAD segment".to_string());
        }
        let base = lo & !(PAGE - 1);
        let end = hi.next_multiple_of(PAGE);
        let mut ram = vec![0u8; (end - base) as usize];
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if u32(bytes, ph)? != PT_LOAD {
                continue;
            }
            let offset = u32(bytes, ph + 4)? as usize;
            let vaddr = u32(bytes, ph + 8)?;
            let filesz = u32(bytes, ph + 16)? as usize;
            let at = (vaddr - base) as usize;
            let from = bytes
                .get(offset..offset + filesz)
                .ok_or("a PT_LOAD segment runs past the end of the file")?;
            ram.get_mut(at..at + filesz)
                .ok_or("a PT_LOAD segment runs past the image span")?
                .copy_from_slice(from);
        }
        let tohost = symbol(bytes, "tohost")?;
        Ok(Image {
            base,
            ram,
            entry,
            tohost,
        })
    }
}

fn symbol(bytes: &[u8], want: &str) -> Result<u32, String> {
    let shoff = u32(bytes, 32)? as usize;
    let shentsize = u16(bytes, 46)? as usize;
    let shnum = u16(bytes, 48)? as usize;
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if u32(bytes, sh + 4)? != SHT_SYMTAB {
            continue;
        }
        let offset = u32(bytes, sh + 16)? as usize;
        let size = u32(bytes, sh + 20)? as usize;
        let entsize = u32(bytes, sh + 36)?.max(16) as usize;
        let strtab = u32(bytes, sh + 24)? as usize;
        let str_off = u32(bytes, shoff + strtab * shentsize + 16)? as usize;
        for sym in (0..size / entsize).map(|n| offset + n * entsize) {
            let name = u32(bytes, sym)? as usize;
            if name != 0 && cstr(bytes, str_off + name) == want {
                return u32(bytes, sym + 4);
            }
        }
    }
    Err(format!("the ELF has no `{want}` symbol"))
}

fn cstr(bytes: &[u8], at: usize) -> &str {
    let rest = bytes.get(at..).unwrap_or_default();
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).unwrap_or_default()
}

fn u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| format!("the ELF ends before offset {at}"))
}

fn u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| format!("the ELF ends before offset {at}"))
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/riscv-tests")
}

/// Not a TOML parser: `pemu-rv32` takes no third-party test dependency.
fn manifest_names() -> Vec<String> {
    let path = data_dir().join("MANIFEST.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "cannot read {}: {}\nrun `cargo xtask riscv-tests fetch-build`",
            path.display(),
            err.kind()
        )
    });
    let mut names = Vec::new();
    let mut in_test = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_test = line == "[[test]]";
            continue;
        }
        if in_test && let Some(rest) = line.strip_prefix("name = \"") {
            names.push(rest.trim_end_matches('"').to_string());
        }
    }
    assert!(!names.is_empty(), "MANIFEST.toml lists no [[test]] name");
    names
}

fn read_elf(name: &str) -> Vec<u8> {
    let path = data_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("cannot read {}: {}", path.display(), err))
}

/// One test, not one per ELF and block size, so the report lists every failure at once.
#[test]
fn every_riscv_test_passes_under_the_engine_at_block_sizes_1_3_and_64() {
    let names = manifest_names();
    let mut failures = Vec::new();
    let mut passed = 0;
    let mut insns_total = 0u64;
    for name in &names {
        let bytes = read_elf(name);
        for max_block_insns in BLOCK_SIZES {
            match run_elf(&bytes, max_block_insns, 1_000) {
                Ok(Outcome::Pass { insns, .. }) => {
                    insns_total += insns;
                    passed += 1;
                }
                Ok(outcome) => failures.push(format!("{name} at {max_block_insns}: {outcome:?}")),
                Err(err) => failures.push(format!("{name} at {max_block_insns}: {err}")),
            }
        }
    }
    println!(
        "riscv-tests on the engine: {passed} of {} runs passed, {insns_total} instructions",
        names.len() * BLOCK_SIZES.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {} engine runs of the riscv-tests failed:\n  {}",
        failures.len(),
        names.len() * BLOCK_SIZES.len(),
        failures.join("\n  ")
    );
}

#[test]
fn every_riscv_test_passes_when_the_engine_is_stopped_after_every_instruction() {
    let names = manifest_names();
    let mut failures = Vec::new();
    for name in &names {
        let bytes = read_elf(name);
        match run_elf(&bytes, 64, 1) {
            Ok(outcome) if outcome.passed() => {}
            Ok(outcome) => failures.push(format!("{name}: {outcome:?}")),
            Err(err) => failures.push(format!("{name}: {err}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} riscv-tests failed at a one-instruction slice:\n  {}",
        failures.len(),
        names.len(),
        failures.join("\n  ")
    );
}

/// Guards the in-block resume token: at a one-instruction slice every stop lands inside a block,
/// and without the token each stop would retranslate.
#[test]
fn the_translation_count_does_not_grow_when_the_engine_is_stopped_often() {
    let names = manifest_names();
    let mut report = Vec::new();
    for name in &names {
        let bytes = read_elf(name);
        let mut counts = Vec::new();
        for slice in [1_000_000u64, 10_000, 1] {
            match run_elf(&bytes, 64, slice) {
                Ok(Outcome::Pass {
                    blocks_built, runs, ..
                }) => counts.push((slice, blocks_built, runs)),
                other => panic!("{name} at slice {slice}: {other:?}"),
            }
        }
        let whole = counts[0].1;
        for (slice, built, runs) in &counts {
            assert_eq!(
                *built, whole,
                "{name}: {built} translations at a {slice}-instruction slice ({runs} runs) \
                 against {whole} in one run: the in-block resume token is not holding"
            );
        }
        report.push((name.clone(), whole, counts[2].2));
    }
    let total: u64 = report.iter().map(|r| r.1).sum();
    let runs: u64 = report.iter().map(|r| r.2).sum();
    println!(
        "translation count over {} tests: {total} blocks, unchanged across {runs} one-instruction \
         runs",
        report.len()
    );
}
