//! The riscv-tests `p` suites under `ref_step` (`tests/engine.rs` runs them on the engine). Each
//! ELF listed in `tests/data/riscv-tests/MANIFEST.toml` is loaded into flat RAM and run until it
//! stores to `tohost`: 1 is pass, `(testnum << 1) | 1` names the failing subtest.
//!
//! CSRs the C3 lacks but the `p` prologue writes (`satp`, `medeleg`, ...) reach `Bus::csr_custom`
//! as plain registers. The shim `xtask/src/riscv_tests/c3_env_p.h` redefines the two prologue hooks
//! the C3 cannot run. Misaligned accesses never trap on this hart, so [`TestBus`] serves any
//! alignment.

// `clippy.toml` bans file I/O in core crates; test code may read the committed ELFs.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pemu_rv32::bus::{Access, Bus, CodePage, HartView, PageTable};
use pemu_rv32::csr::{Csr, CsrEffect, CsrOp};
use pemu_rv32::exec::Hart;
use pemu_rv32::refstep::{StepResult, ref_step};
use pemu_rv32::spmon::SpMonitor;
use pemu_rv32::trap::Trap;

const PASS_VALUE: u32 = 1;

/// The whole suite takes under 20 thousand steps, so this is ample headroom per test.
const STEP_LIMIT: u64 = 2_000_000;

struct TestBus {
    base: u32,
    ram: Vec<u8>,
    tohost: u32,
    verdict: Option<u32>,
    custom: BTreeMap<u16, u32>,
}

impl TestBus {
    fn offset(&self, addr: u32, size: u8) -> Option<usize> {
        let off = addr.checked_sub(self.base)? as usize;
        (off + usize::from(size) <= self.ram.len()).then_some(off)
    }
}

impl Bus for TestBus {
    fn pages(&self) -> &PageTable {
        unreachable!("ref_step is the uncached path and never reads the page table")
    }

    fn arena(&mut self) -> *mut u8 {
        unreachable!("ref_step is the uncached path and never reads the arena")
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
        // The verdict is the first write to `tohost`; the trap vector then loops writing it.
        if addr == self.tohost && self.verdict.is_none() {
            self.verdict = Some(val);
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
        let page_end = (off / 4096 + 1) * 4096;
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

/// `Stuck` is a trap the `p` environment never returns from: it spins at the vector.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass { steps: u64 },
    Failed { value: u32, subtest: u32 },
    Hung { pc: u32 },
    Stuck { pc: u32, mcause: u32, mepc: u32 },
}

impl Outcome {
    fn passed(&self) -> bool {
        matches!(self, Outcome::Pass { .. })
    }
}

fn run_elf(bytes: &[u8]) -> Result<Outcome, String> {
    let image = Image::parse(bytes)?;
    let mut bus = TestBus {
        base: image.base,
        ram: image.ram,
        tohost: image.tohost,
        verdict: None,
        custom: BTreeMap::new(),
    };
    let mut hart = Hart {
        x: [0; 32],
        pc: image.entry,
        csr: Csr::new(),
        wfi: false,
        insns: 0,
        stores: 0,
        spmon: SpMonitor::default(),
        extra: 0,
        pipe: Default::default(),
    };
    let mut steps = 0u64;
    let mut last_trap: Option<(u32, u32)> = None;
    while steps < STEP_LIMIT {
        match ref_step(&mut hart, &mut bus) {
            StepResult::Retired => {}
            StepResult::Trapped(_) => last_trap = Some((hart.csr.mcause, hart.csr.mepc)),
            StepResult::Wfi => {
                // No interrupt can wake this bus, so a `wfi` here is a hang.
                return Ok(Outcome::Hung { pc: hart.pc });
            }
        }
        steps += 1;
        if let Some(value) = bus.verdict {
            return Ok(if value == PASS_VALUE {
                Outcome::Pass { steps }
            } else {
                Outcome::Failed {
                    value,
                    subtest: value >> 1,
                }
            });
        }
    }
    Ok(match last_trap {
        Some((mcause, mepc)) => Outcome::Stuck {
            pc: hart.pc,
            mcause,
            mepc,
        },
        None => Outcome::Hung { pc: hart.pc },
    })
}

/// A minimal ELF32 reader: `pemu-rv32` may depend on `pemu-core` only, so `pemu-loader` is out of
/// reach.
struct Image {
    base: u32,
    ram: Vec<u8>,
    entry: u32,
    tohost: u32,
}

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const EM_RISCV: u16 = 243;
/// The RAM span is rounded out to pages so `fetch_code` can hand out whole pages.
const PAGE: u32 = 4096;

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

/// Not a TOML parser (`pemu-rv32` takes no third-party test dependency): reads exactly what
/// `xtask/src/riscv_tests/manifest.rs` renders, one `key = "value"` per line.
fn manifest_names(text: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut in_test = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_test = line == "[[test]]";
            continue;
        }
        if !in_test {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name = \"") {
            let name = rest
                .strip_suffix('"')
                .ok_or_else(|| format!("unterminated name in MANIFEST.toml: {line}"))?;
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return Err("MANIFEST.toml lists no [[test]] name".to_string());
    }
    Ok(names)
}

/// One test, not one per ELF, so the report lists every failure: a bug usually breaks a family.
#[test]
fn every_riscv_test_passes_under_ref_step() {
    let dir = data_dir();
    let manifest = dir.join("MANIFEST.toml");
    let text = std::fs::read_to_string(&manifest).unwrap_or_else(|err| {
        panic!(
            "cannot read {}: {}\nrun `cargo xtask riscv-tests fetch-build`",
            manifest.display(),
            err.kind()
        )
    });
    let names = manifest_names(&text).expect("MANIFEST.toml lists tests");

    let mut failures = Vec::new();
    let mut passed = 0;
    let mut steps_total = 0u64;
    for name in &names {
        let path = dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                failures.push(format!("{name}: cannot read ({})", err.kind()));
                continue;
            }
        };
        match run_elf(&bytes) {
            Ok(outcome) if outcome.passed() => {
                if let Outcome::Pass { steps } = outcome {
                    steps_total += steps;
                }
                passed += 1;
            }
            Ok(outcome) => failures.push(format!("{name}: {outcome:?}")),
            Err(err) => failures.push(format!("{name}: {err}")),
        }
    }
    println!(
        "riscv-tests under ref_step: {passed} of {} passed, {steps_total} steps",
        names.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {} riscv-tests failed under ref_step:\n  {}",
        failures.len(),
        names.len(),
        failures.join("\n  ")
    );
}

#[test]
fn the_manifest_covers_rv32ui_rv32um_and_rv32uc() {
    let dir = data_dir();
    let text = std::fs::read_to_string(dir.join("MANIFEST.toml")).expect("MANIFEST.toml");
    let names = manifest_names(&text).expect("MANIFEST.toml lists tests");
    for suite in ["rv32ui-p-", "rv32um-p-", "rv32uc-p-"] {
        let count = names.iter().filter(|name| name.starts_with(suite)).count();
        assert!(count > 0, "the manifest names no {suite}* test");
    }
    for name in &names {
        assert!(
            dir.join(name).is_file(),
            "{name} is missing from {}",
            dir.display()
        );
    }
}

#[test]
fn manifest_names_reads_test_blocks_only() {
    let text = concat!(
        "schema = \"x\"\nname = \"not-a-test\"\n\n",
        "[[test]]\nname = \"rv32ui-p-add\"\nsha256 = \"00\"\nsize = 1\n\n",
        "[[excluded]]\nname = \"rv32ui-p-nope\"\nreason = \"no\"\n",
    );
    assert_eq!(manifest_names(text).expect("names"), vec!["rv32ui-p-add"]);
    assert!(manifest_names("schema = \"x\"\n").is_err());
}
