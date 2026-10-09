//! The symbols binding reads, recovered from the bytes of an image that comes without an ELF.
//!
//! This module finds each hooked function, nested-call target, data symbol and kernel symbol in
//! the boot app's segments and builds the table the ELF would have given; binding then runs its
//! unchanged ELF checks (size, section, head hash, `idf_ver`, required names) on it.
//!
//! - **A function** is found by its shape, pinned in `specs/hle/idf-5.5.3/image-symbols.toml`:
//!   its size, the [`body_skeleton`] of its first [`LEAD_BYTES`] bytes (the scan key) and the
//!   SHA-256 of the whole body skeleton, which stands in for the ELF's `st_size`.
//! - **A call** (`calls`, `name@offset`) narrows code-identical twins (`esp_event_post` and
//!   `esp_event_handler_instance_register`) to the one the caller reaches, and tells 8-byte ROM
//!   tail calls apart by their target.
//! - **A data symbol** is read from a pinned materialization in a found function (`refs`,
//!   `name@hi+lo` for a `lui`/`auipc` pair, `name@at` for a `gp`-relative access). A load or
//!   store witness must access exactly the pinned size.
//!
//! A symbol is recovered only when exactly one address remains and every witness agrees; anything
//! else is reported, never guessed. Absence is never concluded: a hook that is not found binds
//! nothing, and its module binds without it only when a tripwire guards it
//! ([`Recovered::check`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use pemu_loader::app_desc::AppDesc;
use pemu_loader::bundle::{TomlLite, TomlTable};
use pemu_loader::elf::{ElfInfo, ElfSection, SHF_ALLOC, SHF_EXECINSTR, SHT_PROGBITS};
use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};
use pemu_rv32::decode::decode_at;
use pemu_rv32::op::{
    K_ADDI, K_AUIPC, K_JAL, K_JALR, K_LB, K_LBU, K_LH, K_LHU, K_LUI, K_LW, K_SB, K_SH, K_SW,
};

use crate::binding::{BindingMismatch, LoadedSegment, MismatchField, ModuleSymbols};

/// The pinned recovery rules of IDF 5.5.3.
pub const IMAGE_SYMBOLS_TOML: &str =
    include_str!("../../../specs/hle/idf-5.5.3/image-symbols.toml");

pub const LEAD_BYTES: usize = 8;

pub const GLOBAL_POINTER: &str = "__global_pointer$";

/// What the HLE core reads beside every module's own symbols: the task-deletion observe hook and
/// the kernel variables behind `GuestView::current_task`, `in_isr`, `scheduler_running` and the
/// ISR stack guard.
pub const CORE_REQUIRED: [&str; 6] = [
    "vTaskDelete",
    "pxCurrentTCBs",
    "port_uxInterruptNesting",
    "xSchedulerRunning",
    "xIsrStackBottom",
    "xIsrStackTop",
];

// The body skeleton.

/// The skeleton of a whole function body: [`crate::binding::code_skeleton`], the one binding
/// hashes the head of a hooked function with, over every byte of the function. Anything that
/// differs is a different body.
pub fn body_skeleton(bytes: &[u8]) -> Vec<u8> {
    crate::binding::code_skeleton(bytes)
}

/// The first two bytes of the body skeleton of the code at `bytes[0]`, without decoding more: the
/// first instruction has no high register before it, and zeroing a `gp`-relative `addi` touches
/// only its upper halfword.
fn first_skeleton_half(bytes: &[u8]) -> u16 {
    use crate::binding::{mask_16, mask_32};
    let half = u16::from_le_bytes([bytes[0], bytes.get(1).copied().unwrap_or(0)]);
    if half & 0b11 != 0b11 {
        return mask_16(half);
    }
    match bytes.get(2..4) {
        Some(hi) => {
            mask_32(u32::from(half) | u32::from(u16::from_le_bytes([hi[0], hi[1]])) << 16) as u16
        }
        // Cut by the window: the opcode only.
        None => half & 0x7F,
    }
}

pub fn body_hash(bytes: &[u8]) -> [u8; 32] {
    pemu_loader::sha256(&body_skeleton(bytes))
}

/// The scan key: the body skeleton of the first [`LEAD_BYTES`] bytes, `None` when fewer remain.
pub fn lead(bytes: &[u8]) -> Option<[u8; LEAD_BYTES]> {
    let mut out = Vec::with_capacity(LEAD_BYTES);
    crate::binding::skeleton_into(bytes.get(..LEAD_BYTES)?, &mut out, None);
    out.try_into().ok()
}

// The pinned rules.

/// A data symbol materialized in a function: `hi` is the offset of the `lui`, `auipc` or
/// `gp`-relative instruction, `lo` the offset of the instruction that completes a `lui` or
/// `auipc` (none for `gp`-relative).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRef {
    pub symbol: String,
    /// Offset of the first instruction.
    pub hi: u32,
    /// Offset of the completing instruction, for a `lui` or `auipc` pair.
    pub lo: Option<u32>,
}

/// A call in a function: the instruction at `at` (`jal`, or `auipc` then `jalr`) calls `symbol`, a
/// function of the rules or a ROM symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRef {
    pub symbol: String,
    pub at: u32,
}

/// One accepted body of a function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Informative only.
    pub builds: String,
    /// The ELF's `st_size`.
    pub size: u32,
    /// The scan key ([`lead`]).
    pub lead: [u8; LEAD_BYTES],
    pub body: [u8; 32],
    pub refs: Vec<DataRef>,
    pub calls: Vec<CallRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionRule {
    pub name: String,
    /// At least one.
    pub shapes: Vec<Shape>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRule {
    pub name: String,
    /// The ELF's `st_size`.
    pub size: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRules {
    pub idf: String,
    pub functions: Vec<FunctionRule>,
    pub data: Vec<DataRule>,
}

impl ImageRules {
    /// A file that does not parse is a build defect, caught by the tests.
    pub fn load() -> &'static ImageRules {
        static RULES: OnceLock<ImageRules> = OnceLock::new();
        RULES.get_or_init(|| {
            ImageRules::parse(IMAGE_SYMBOLS_TOML).unwrap_or_else(|_| ImageRules {
                idf: String::new(),
                functions: Vec::new(),
                data: Vec::new(),
            })
        })
    }

    /// Refuses an unknown table, a missing key, a malformed digest or offset, and a reference to an
    /// undeclared data row: a rule that parsed empty would let the recovery guess.
    pub fn parse(text: &str) -> Result<ImageRules, String> {
        let mut idf = None;
        let mut functions: Vec<FunctionRule> = Vec::new();
        let mut data = Vec::new();
        for table in TomlLite::parse(text).tables() {
            match (table.name.as_str(), table.array) {
                ("profile", false) => idf = Some(need(table, "idf")?.to_string()),
                ("function", true) => functions.push(FunctionRule {
                    name: need(table, "name")?.to_string(),
                    shapes: Vec::new(),
                }),
                ("shape", true) => {
                    let function = functions
                        .last_mut()
                        .ok_or("a [[shape]] before any [[function]]")?;
                    let size = table
                        .integer("size")
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or_else(|| format!("a shape of `{}` has no size", function.name))?;
                    if (size as usize) < LEAD_BYTES {
                        return Err(format!("`{}` is shorter than its scan key", function.name));
                    }
                    function.shapes.push(Shape {
                        builds: need(table, "builds")?.to_string(),
                        size,
                        lead: hex_array(need(table, "lead")?)?,
                        body: hex_array(need(table, "body_sha256")?)?,
                        refs: list(table.string("refs"), parse_ref)?,
                        calls: list(table.string("calls"), parse_call)?,
                    });
                }
                ("data", true) => data.push(DataRule {
                    name: need(table, "name")?.to_string(),
                    size: table
                        .integer("size")
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or("a [[data]] row has no size")?,
                }),
                (name, array) => {
                    return Err(format!(
                        "unknown table `{}{name}{}`",
                        if array { "[[" } else { "[" },
                        if array { "]]" } else { "]" }
                    ));
                }
            }
        }
        if let Some(f) = functions.iter().find(|f| f.shapes.is_empty()) {
            return Err(format!("function `{}` has no [[shape]]", f.name));
        }
        let declared: BTreeSet<&str> = data.iter().map(|d| d.name.as_str()).collect();
        for shape in functions.iter().flat_map(|f| &f.shapes) {
            if let Some(r) = shape
                .refs
                .iter()
                .find(|r| !declared.contains(r.symbol.as_str()))
            {
                return Err(format!(
                    "`{}` is referenced but has no [[data]] row",
                    r.symbol
                ));
            }
        }
        Ok(ImageRules {
            idf: idf.ok_or("no [profile]")?,
            functions,
            data,
        })
    }

    fn function(&self, name: &str) -> Option<usize> {
        self.functions.iter().position(|f| f.name == name)
    }
}

fn need<'a>(table: &'a TomlTable, key: &str) -> Result<&'a str, String> {
    table
        .string(key)
        .ok_or_else(|| format!("[{}] has no string `{key}`", table.name))
}

/// `N` bytes written as `2N` hex digits.
fn hex_array<const N: usize>(text: &str) -> Result<[u8; N], String> {
    let bytes = text.as_bytes();
    let refuse = || format!("`{text}` is not {} hex digits", 2 * N);
    if bytes.len() != 2 * N || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(refuse());
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&bytes[2 * i..2 * i + 2]).map_err(|_| refuse())?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| refuse())?;
    }
    Ok(out)
}

/// A space-separated list of `name@...` items.
fn list<T>(text: Option<&str>, item: fn(&str) -> Result<T, String>) -> Result<Vec<T>, String> {
    text.unwrap_or("").split_whitespace().map(item).collect()
}

fn offset(text: &str) -> Result<u32, String> {
    text.strip_prefix("0x")
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .ok_or_else(|| format!("`{text}` is not a 0x offset"))
}

/// `name@0xHI` or `name@0xHI+0xLO`.
fn parse_ref(item: &str) -> Result<DataRef, String> {
    let (symbol, at) = item
        .split_once('@')
        .ok_or_else(|| format!("`{item}` is not name@offset"))?;
    let (hi, lo) = match at.split_once('+') {
        Some((hi, lo)) => (offset(hi)?, Some(offset(lo)?)),
        None => (offset(at)?, None),
    };
    Ok(DataRef {
        symbol: symbol.to_string(),
        hi,
        lo,
    })
}

/// `name@0xAT`.
fn parse_call(item: &str) -> Result<CallRef, String> {
    let (symbol, at) = item
        .split_once('@')
        .ok_or_else(|| format!("`{item}` is not name@offset"))?;
    Ok(CallRef {
        symbol: symbol.to_string(),
        at: offset(at)?,
    })
}

// Recovery.

/// The section name of an executable C3 address (flash instruction window or internal SRAM on the
/// instruction bus), or `None`.
pub fn code_section(addr: u32) -> Option<&'static str> {
    match addr {
        0x4200_0000..=0x427F_FFFF => Some(".flash.text"),
        0x4037_0000..=0x403D_FFFF => Some(".iram0.text"),
        _ => None,
    }
}

/// What the recovery concluded about one name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one address, every witness agreeing.
    At {
        addr: u32,
        size: u32,
    },
    Missing(String),
    /// More than one address remained, or the witnesses disagree.
    Ambiguous(Vec<u32>),
}

/// The outcome of [`recover`]: the symbol table the ELF would have given, and what was concluded
/// about every name of the rules.
#[derive(Clone, Debug)]
pub struct Recovered {
    /// One section per executable segment, the image's app descriptor and entry, and an all-zero
    /// SHA-256: there is no ELF file to identify.
    pub elf: ElfInfo,
    pub resolved: BTreeMap<String, Resolution>,
}

/// How a module may bind against a [`Recovered`] table ([`Recovered::check`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleCheck {
    /// Every hook and every required name was recovered.
    Bind,
    /// No hook's shape is anywhere in the image; nothing is bound, as for any image without an
    /// ELF.
    NotFound,
    /// Some but not all of what the module reads was recovered, or a name is ambiguous.
    Refuse(Vec<BindingMismatch>),
}

impl Recovered {
    /// The image can show that a function is present, never that it is absent, so once any hook
    /// of a module is found, every required name must be, and every hook must be found or
    /// guarded, or the module is refused with each name that was not.
    ///
    /// A hook that is not found is linked in a shape no rule pins, or not linked at all; the
    /// image cannot tell which. Its guard ([`ModuleSymbols::guards`]) makes the two the same to
    /// the run: when the guard is found and the tripwire rule arms it, the hook's own body cannot
    /// get past its first call, so the module binds without that hook, as it does for an ELF that
    /// does not link it. A hook with no guard, or whose guard is not an armed tripwire, refuses
    /// the module as before.
    pub fn check(&self, module: &ModuleSymbols) -> ModuleCheck {
        let status = |name: &str| self.resolved.get(name);
        let any = module
            .hooks
            .iter()
            .any(|h| !matches!(status(h), None | Some(Resolution::Missing(_))));
        if !any {
            return ModuleCheck::NotFound;
        }
        let armed = armed_guards(module);
        let found = |name: &str| matches!(status(name), Some(Resolution::At { .. }));
        let mismatches: Vec<BindingMismatch> = module
            .hooks
            .iter()
            .chain(&module.required)
            .chain(CORE_REQUIRED.iter())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|name| {
                let missing = |why: &str| {
                    let guard = module.guards.iter().find(|(hook, _)| hook == name);
                    let (expected, found) = match guard {
                        Some((_, guard)) if armed.contains(guard) && found(guard) => return None,
                        Some((_, guard)) if !armed.contains(guard) => (
                            format!("found in the image, or guarded by `{guard}`"),
                            format!("{why}; `{guard}` is not a tripwire"),
                        ),
                        Some((_, guard)) => (
                            format!("found in the image, or guarded by `{guard}`"),
                            format!("{why}; `{guard}` was not found either"),
                        ),
                        None => ("found in the image".to_string(), why.to_string()),
                    };
                    Some(BindingMismatch {
                        symbol: name.to_string(),
                        field: MismatchField::Missing,
                        expected,
                        found,
                    })
                };
                match status(name) {
                    Some(Resolution::At { .. }) => None,
                    Some(Resolution::Ambiguous(addrs)) => Some(BindingMismatch {
                        symbol: name.to_string(),
                        field: MismatchField::Ambiguous,
                        expected: "one place in the image".to_string(),
                        found: addrs
                            .iter()
                            .map(|a| format!("{a:#010x}"))
                            .collect::<Vec<_>>()
                            .join(" and "),
                    }),
                    Some(Resolution::Missing(why)) => missing(why),
                    None => missing("no rule in image-symbols.toml recovers it"),
                }
            })
            .collect();
        if mismatches.is_empty() {
            ModuleCheck::Bind
        } else {
            ModuleCheck::Refuse(mismatches)
        }
    }
}

/// The guards of `module` the tripwire rule arms when the image links them: blob-defined, not on
/// the coexistence allowlist, and not hooked by the module itself (a hooked pc holds no tripwire).
fn armed_guards(module: &ModuleSymbols) -> BTreeSet<&'static str> {
    if module.guards.is_empty() {
        return BTreeSet::new();
    }
    let blob = crate::tripwire::blob_defined_set();
    let allow = crate::tripwire::TripwireSpec::load().coexistence_allow;
    module
        .guards
        .iter()
        .map(|(_, guard)| *guard)
        .filter(|guard| {
            blob.contains(*guard)
                && !allow.iter().any(|a| a == guard)
                && !module.hooks.contains(guard)
        })
        .collect()
}

fn op_at(segments: &[LoadedSegment<'_>], addr: u32) -> Option<pemu_rv32::op::Op> {
    segments.iter().find_map(|seg| {
        let start = addr.checked_sub(seg.addr)? as usize;
        decode_at(seg.data.get(start..)?, addr)
    })
}

/// The target of the call at `addr`: a `jal`, or an `auipc` and the `jalr` after it.
fn call_target(segments: &[LoadedSegment<'_>], addr: u32) -> Option<u32> {
    let op = op_at(segments, addr)?;
    match op.kind {
        K_JAL => Some(op.imm as u32),
        K_AUIPC => {
            let next = op_at(segments, addr + u32::from(op.len))?;
            (next.kind == K_JALR && next.rs1 == op.rd && op.rd != 0)
                .then(|| (op.imm as u32).wrapping_add(next.imm as u32) & !1)
        }
        _ => None,
    }
}

/// The address a data reference materializes in the function at `host`, and the width it
/// accesses when it is a load or a store.
fn materialize(
    segments: &[LoadedSegment<'_>],
    host: u32,
    r: &DataRef,
    gp: Option<u32>,
) -> Result<(u32, Option<u32>), String> {
    let first = op_at(segments, host + r.hi).ok_or("the witness is outside the image")?;
    let (base, last) = match r.lo {
        Some(lo) => {
            if !(first.kind == K_LUI || first.kind == K_AUIPC) || first.rd == 0 {
                return Err(format!("+{:#x} is not a lui or auipc", r.hi));
            }
            let last = op_at(segments, host + lo).ok_or("the witness is outside the image")?;
            if last.rs1 != first.rd {
                return Err(format!("+{lo:#x} does not complete +{:#x}", r.hi));
            }
            (first.imm as u32, last)
        }
        None => {
            if first.rs1 != 3 {
                return Err(format!("+{:#x} is not gp-relative", r.hi));
            }
            (gp.ok_or("gp was not recovered")?, first)
        }
    };
    let width = match last.kind {
        K_ADDI => None,
        K_LB | K_LBU | K_SB => Some(1),
        K_LH | K_LHU | K_SH => Some(2),
        K_LW | K_SW => Some(4),
        _ => return Err("the witness neither loads, stores nor adds".to_string()),
    };
    Ok((base.wrapping_add(last.imm as u32), width))
}

/// The longest shape of one lead, and every (function, shape) index pair that has that lead.
type LeadShapes = (u32, Vec<(usize, usize)>);

/// Recovers every name of `rules` from the executable `segments` of a boot app. `rom` resolves
/// calls into the ROM; `app_desc` and `entry` go into the returned `ElfInfo` for the `idf_ver`
/// check.
pub fn recover(
    rules: &ImageRules,
    segments: &[LoadedSegment<'_>],
    app_desc: Option<AppDesc>,
    entry: u32,
    rom: Option<&SymbolTable>,
) -> Recovered {
    // Scan: every even address whose first skeleton halfword starts some lead, then whose lead is
    // some shape's, then whose body is. The first test is a table lookup that turns away almost
    // every address; the body skeleton is computed once per address and each shorter shape hashes
    // a prefix of it.
    let mut index: BTreeMap<[u8; LEAD_BYTES], LeadShapes> = BTreeMap::new();
    let mut first = vec![false; 1 << 16];
    for (f, function) in rules.functions.iter().enumerate() {
        for (s, shape) in function.shapes.iter().enumerate() {
            let entry = index.entry(shape.lead).or_default();
            entry.0 = entry.0.max(shape.size);
            entry.1.push((f, s));
            first[usize::from(u16::from_le_bytes([shape.lead[0], shape.lead[1]]))] = true;
        }
    }
    let mut candidates: Vec<Vec<(u32, usize)>> = vec![Vec::new(); rules.functions.len()];
    let (mut key, mut skeleton, mut ends) = (Vec::new(), Vec::new(), Vec::new());
    for seg in segments
        .iter()
        .filter(|s| code_section(s.addr).is_some() && s.addr % 2 == 0)
    {
        let data = seg.data;
        for off in (0..data.len().saturating_sub(LEAD_BYTES - 1)).step_by(2) {
            if !first[usize::from(first_skeleton_half(&data[off..]))] {
                continue;
            }
            crate::binding::skeleton_into(&data[off..off + LEAD_BYTES], &mut key, None);
            let Some((longest, hits)) = index.get(key.as_slice()) else {
                continue;
            };
            let end = data.len().min(off + *longest as usize);
            crate::binding::skeleton_into(&data[off..end], &mut skeleton, Some(&mut ends));
            let addr = seg.addr + off as u32;
            for &(f, s) in hits {
                let shape = &rules.functions[f].shapes[s];
                let size = shape.size as usize;
                if ends.binary_search(&size).is_ok()
                    && pemu_loader::sha256(&skeleton[..size]) == shape.body
                    && !candidates[f].iter().any(|c| c.0 == addr)
                {
                    candidates[f].push((addr, s));
                }
            }
        }
    }

    // A call into the ROM tells its caller apart at once.
    let rom_target = |name: &str| rom.and_then(|t| t.addr_of(name));
    for (f, function) in rules.functions.iter().enumerate() {
        candidates[f].retain(|&(addr, s)| {
            function.shapes[s].calls.iter().all(|call| {
                rules.function(&call.symbol).is_some()
                    || rom_target(&call.symbol)
                        .is_some_and(|t| call_target(segments, addr + call.at) == Some(t))
            })
        });
    }

    // Calls between functions of the rules, until nothing changes: a found caller keeps only its
    // callee's candidate it calls, and a found callee keeps only its callers' candidates that call it.
    let mut notes: BTreeMap<usize, String> = BTreeMap::new();
    for _ in 0..=rules.functions.len() {
        let mut changed = false;
        for f in 0..rules.functions.len() {
            if let [(addr, s)] = candidates[f][..] {
                for call in &rules.functions[f].shapes[s].calls {
                    let Some(g) = rules.function(&call.symbol).filter(|&g| g != f) else {
                        continue;
                    };
                    let target = call_target(segments, addr + call.at);
                    {
                        let before = candidates[g].len();
                        candidates[g].retain(|c| Some(c.0) == target);
                        if candidates[g].len() != before {
                            changed = true;
                            if candidates[g].is_empty() {
                                notes.insert(
                                    g,
                                    format!(
                                        "{}+{:#x} calls {}, which has no pinned shape",
                                        rules.functions[f].name,
                                        call.at,
                                        target.map_or("nothing".into(), |t| format!("{t:#010x}"))
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            if candidates[f].len() > 1 {
                let before = candidates[f].len();
                let snapshot = candidates.clone();
                candidates[f].retain(|&(addr, s)| {
                    rules.functions[f].shapes[s].calls.iter().all(|call| {
                        match rules.function(&call.symbol).map(|g| &snapshot[g]) {
                            Some(callee) if callee.len() == 1 => {
                                call_target(segments, addr + call.at) == Some(callee[0].0)
                            }
                            _ => true,
                        }
                    })
                });
                changed |= candidates[f].len() != before;
            }
        }
        if !changed {
            break;
        }
    }

    let mut resolved = BTreeMap::new();
    let mut found: Vec<(usize, u32, usize)> = Vec::new();
    for (f, function) in rules.functions.iter().enumerate() {
        let resolution =
            match candidates[f].as_slice() {
                [] => Resolution::Missing(notes.remove(&f).unwrap_or_else(|| {
                    "no place in the image has a pinned shape of it".to_string()
                })),
                [(addr, s)] => {
                    found.push((f, *addr, *s));
                    Resolution::At {
                        addr: *addr,
                        size: function.shapes[*s].size,
                    }
                }
                many => Resolution::Ambiguous(many.iter().map(|c| c.0).collect()),
            };
        resolved.insert(function.name.clone(), resolution);
    }

    // Data: address pairs first, `gp` among them, then the `gp`-relative witnesses.
    let mut seen: BTreeMap<&str, (BTreeSet<u32>, Vec<String>)> = BTreeMap::new();
    for gp_pass in [false, true] {
        let gp = match seen.get(GLOBAL_POINTER) {
            Some((addrs, errs)) if addrs.len() == 1 && errs.is_empty() => addrs.first().copied(),
            _ => None,
        };
        for &(f, host, s) in &found {
            for r in &rules.functions[f].shapes[s].refs {
                if r.lo.is_none() != gp_pass {
                    continue;
                }
                let size = rules
                    .data
                    .iter()
                    .find(|d| d.name == r.symbol)
                    .map_or(0, |d| d.size);
                let entry = seen.entry(r.symbol.as_str()).or_default();
                match materialize(segments, host, r, gp) {
                    Ok((_, Some(width))) if width != size => entry.1.push(format!(
                        "{}+{:#x} accesses {width} bytes, the row pins {size}",
                        rules.functions[f].name, r.hi
                    )),
                    Ok((addr, _)) => {
                        entry.0.insert(addr);
                    }
                    Err(why) => entry
                        .1
                        .push(format!("{}+{:#x}: {why}", rules.functions[f].name, r.hi)),
                }
            }
        }
    }
    for d in &rules.data {
        let resolution = match seen.get(d.name.as_str()) {
            None => Resolution::Missing("no function that materializes it was found".to_string()),
            Some((_, errs)) if !errs.is_empty() => Resolution::Missing(errs.join("; ")),
            Some((addrs, _)) if addrs.len() == 1 => Resolution::At {
                addr: *addrs.first().unwrap_or(&0),
                size: d.size,
            },
            Some((addrs, _)) => Resolution::Ambiguous(addrs.iter().copied().collect()),
        };
        resolved.insert(d.name.clone(), resolution);
    }

    let elf = synthesize(rules, segments, &resolved, app_desc, entry);
    Recovered { elf, resolved }
}

fn synthesize(
    rules: &ImageRules,
    segments: &[LoadedSegment<'_>],
    resolved: &BTreeMap<String, Resolution>,
    app_desc: Option<AppDesc>,
    entry: u32,
) -> ElfInfo {
    let sections: Vec<ElfSection> = segments
        .iter()
        .filter_map(|seg| Some((seg, code_section(seg.addr)?)))
        .enumerate()
        .map(|(i, (seg, name))| ElfSection {
            index: i + 1,
            name: name.to_string(),
            sh_type: SHT_PROGBITS,
            flags: SHF_ALLOC | SHF_EXECINSTR,
            addr: seg.addr,
            offset: 0,
            size: seg.data.len() as u32,
            align: 4,
        })
        .collect();
    let mut symbols = Vec::new();
    for (name, resolution) in resolved {
        let Resolution::At { addr, size } = *resolution else {
            continue;
        };
        let function = rules.function(name).is_some();
        let section = if function {
            sections
                .iter()
                .find(|s| addr >= s.addr && u64::from(addr) < s.end())
                .map_or(SymSection::Absolute, |s| SymSection::Index(s.index))
        } else {
            SymSection::Absolute
        };
        symbols.push(Symbol {
            name: name.clone(),
            addr,
            size,
            kind: if function {
                SymKind::Func
            } else if name == GLOBAL_POINTER {
                SymKind::NoType
            } else {
                SymKind::Object
            },
            bind: SymBind::Global,
            section,
        });
    }
    ElfInfo {
        sha256: [0; 32],
        entry,
        sections,
        segments: Vec::new(),
        symbols: SymbolTable::new(symbols),
        app_desc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{code_skeleton, skeleton_hash};

    #[test]
    fn the_compiled_in_rules_parse() {
        let rules = ImageRules::parse(IMAGE_SYMBOLS_TOML).expect("image-symbols.toml parses");
        assert_eq!(rules.idf, "5.5.3");
        assert_eq!(ImageRules::load(), &rules);
        for name in CORE_REQUIRED {
            assert!(
                rules.function(name).is_some() || rules.data.iter().any(|d| d.name == name),
                "{name} has a rule"
            );
        }
    }

    /// lui a5,0x3fc99; addi a5,a5,-4 (a %lo partner); addi a4,a4,8 (a constant); addi a0,gp,12.
    fn pair(hi: u32, lo: u32, konst: u32, gp: u32) -> Vec<u8> {
        [
            0x0000_07b7 | (hi << 12),
            0x0007_8793 | (lo << 20),
            0x0007_0713 | (konst << 20),
            0x0001_8513 | (gp << 20),
        ]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect()
    }

    #[test]
    fn the_body_skeleton_zeroes_the_low_half_of_an_address_and_keeps_a_constant() {
        let a = pair(0x3fc99, 0xffc, 8, 12);
        let b = pair(0x3fca0, 0x010, 8, 0x200);
        assert_eq!(
            body_hash(&a),
            body_hash(&b),
            "a relinked address is the same body"
        );
        // The head check and the body check are one skeleton, so they never disagree about a
        // relocated field.
        assert_eq!(skeleton_hash(&a), body_hash(&a));
        assert_eq!(code_skeleton(&a), body_skeleton(&a));
        let c = pair(0x3fc99, 0xffc, 9, 12);
        assert_ne!(
            body_hash(&a),
            body_hash(&c),
            "a different constant is another body"
        );
    }

    #[test]
    fn a_register_stops_being_high_when_it_is_written_again() {
        // lui a5; li a5,1 (addi a5,x0,1); addi a5,a5,8: the last addi is a constant, kept.
        let code: Vec<u8> = [0x3fc9_97b7u32, 0x0010_0793, 0x0087_8793]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let mut other = code.clone();
        other[10] ^= 0x10; // addi a5,a5,9
        assert_ne!(body_hash(&code), body_hash(&other));
    }

    #[test]
    fn a_rule_that_could_be_guessed_from_is_refused() {
        let base = "[profile]\nidf = \"5.5.3\"\n[[data]]\nname = \"d\"\nsize = 4\n";
        let shape = |extra: &str| {
            format!(
                "{base}[[function]]\nname = \"f\"\n[[shape]]\nbuilds = \"x\"\nsize = 16\n\
                 lead = \"0011223344556677\"\nbody_sha256 = \"{}\"\n{extra}",
                "ab".repeat(32)
            )
        };
        assert!(ImageRules::parse(&shape("refs = \"d@0x4+0x8\"\n")).is_ok());
        assert!(
            ImageRules::parse(&shape("refs = \"e@0x4\"\n")).is_err(),
            "undeclared"
        );
        assert!(
            ImageRules::parse(&shape("refs = \"d@4\"\n")).is_err(),
            "not 0x"
        );
        assert!(
            ImageRules::parse(&shape("calls = \"g\"\n")).is_err(),
            "no offset"
        );
        assert!(
            ImageRules::parse(&format!("{base}[[function]]\nname = \"f\"\n")).is_err(),
            "no shape"
        );
        assert!(ImageRules::parse(&format!("{base}[[nope]]\n")).is_err());
        let short = shape("").replace("size = 16", "size = 4");
        assert!(ImageRules::parse(&short).is_err(), "shorter than the key");
    }

    /// One function per shape in a segment, found by the scan and resolved.
    #[test]
    fn a_function_and_its_data_are_recovered_and_a_twin_is_ambiguous() {
        // f: lui a5,X; lw a0,4(a5); ret, padded with c.nop to 16 bytes.
        let mut f: Vec<u8> = [0x3fc9_97b7u32, 0x0047_a503]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        f.extend_from_slice(&0x8082u16.to_le_bytes());
        while f.len() < 16 {
            f.extend_from_slice(&0x0001u16.to_le_bytes());
        }
        let rules = ImageRules {
            idf: "5.5.3".into(),
            functions: vec![FunctionRule {
                name: "f".into(),
                shapes: vec![Shape {
                    builds: "t".into(),
                    size: 16,
                    lead: lead(&f).expect("16 bytes"),
                    body: body_hash(&f),
                    refs: vec![DataRef {
                        symbol: "d".into(),
                        hi: 0,
                        lo: Some(4),
                    }],
                    calls: Vec::new(),
                }],
            }],
            data: vec![DataRule {
                name: "d".into(),
                size: 4,
            }],
        };
        let mut text = vec![0u8; 0x20];
        text.extend_from_slice(&f);
        let seg = [LoadedSegment {
            addr: 0x4200_0000,
            data: &text,
        }];
        let got = recover(&rules, &seg, None, 0, None);
        assert_eq!(
            got.resolved["f"],
            Resolution::At {
                addr: 0x4200_0020,
                size: 16
            }
        );
        assert_eq!(
            got.resolved["d"],
            Resolution::At {
                addr: 0x3fc9_9004,
                size: 4
            }
        );
        let sym = got.elf.symbols.lookup("f").expect("in the table");
        assert_eq!(sym.size, 16);
        assert_eq!(
            got.elf.sections[0].name, ".flash.text",
            "the hook section check reads it"
        );

        // A second copy: two places, so neither the function nor its data is claimed.
        text.extend_from_slice(&f);
        let seg = [LoadedSegment {
            addr: 0x4200_0000,
            data: &text,
        }];
        let got = recover(&rules, &seg, None, 0, None);
        assert_eq!(
            got.resolved["f"],
            Resolution::Ambiguous(vec![0x4200_0020, 0x4200_0030])
        );
        assert!(matches!(got.resolved["d"], Resolution::Missing(_)));
        assert!(got.elf.symbols.lookup("f").is_none());

        // A load of the wrong width is not the pinned object.
        let mut narrow = rules.clone();
        narrow.data[0].size = 1;
        let seg = [LoadedSegment {
            addr: 0x4200_0000,
            data: &text[..0x30],
        }];
        let got = recover(&narrow, &seg, None, 0, None);
        assert!(matches!(&got.resolved["d"], Resolution::Missing(why) if why.contains("4 bytes")));
    }

    #[test]
    fn a_module_binds_only_with_every_name_and_never_with_some() {
        let at = Resolution::At { addr: 4, size: 4 };
        let mut resolved: BTreeMap<String, Resolution> = CORE_REQUIRED
            .iter()
            .map(|n| (n.to_string(), at.clone()))
            .collect();
        resolved.insert("h1".into(), at.clone());
        resolved.insert("h2".into(), Resolution::Missing("no shape".into()));
        resolved.insert("c".into(), Resolution::Ambiguous(vec![4, 8]));
        let recovered = Recovered {
            elf: synthesize(
                &ImageRules {
                    idf: String::new(),
                    functions: Vec::new(),
                    data: Vec::new(),
                },
                &[],
                &BTreeMap::new(),
                None,
                0,
            ),
            resolved,
        };
        let module = |hooks: Vec<&'static str>, required: Vec<&'static str>| ModuleSymbols {
            hooks,
            required,
            guards: Vec::new(),
        };
        assert_eq!(
            recovered.check(&module(vec!["h1"], vec![])),
            ModuleCheck::Bind
        );
        assert_eq!(
            recovered.check(&module(vec!["h2", "h3"], vec!["c"])),
            ModuleCheck::NotFound,
            "no hook found: nothing is claimed"
        );
        let ModuleCheck::Refuse(why) = recovered.check(&module(vec!["h1", "h2"], vec!["c", "x"]))
        else {
            panic!("a partial module is refused");
        };
        let named: Vec<(&str, MismatchField)> =
            why.iter().map(|m| (m.symbol.as_str(), m.field)).collect();
        assert_eq!(
            named,
            [
                ("c", MismatchField::Ambiguous),
                ("h2", MismatchField::Missing),
                ("x", MismatchField::Missing)
            ]
        );
    }

    #[test]
    fn a_hook_that_is_not_found_needs_a_found_guard_the_tripwire_rule_arms() {
        let at = Resolution::At { addr: 4, size: 4 };
        let missing = || Resolution::Missing("no shape".into());
        let recovered = |guard: Option<Resolution>| {
            let mut resolved: BTreeMap<String, Resolution> = CORE_REQUIRED
                .iter()
                .map(|n| (n.to_string(), at.clone()))
                .collect();
            resolved.insert("h1".into(), at.clone());
            resolved.insert("h2".into(), missing());
            for name in ["wifi_init_completed", "coex_pre_init", "app_main"] {
                if let Some(guard) = &guard {
                    resolved.insert(name.into(), guard.clone());
                }
            }
            Recovered {
                elf: no_elf(),
                resolved,
            }
        };
        let module = |guard: &'static str| ModuleSymbols {
            hooks: vec!["h1", "h2"],
            required: Vec::new(),
            guards: vec![("h2", guard)],
        };
        let refusal = |recovered: &Recovered, guard: &'static str| {
            let ModuleCheck::Refuse(why) = recovered.check(&module(guard)) else {
                panic!("`{guard}` does not stand in for h2");
            };
            assert_eq!(why.len(), 1);
            assert_eq!(
                (why[0].symbol.as_str(), why[0].field),
                ("h2", MismatchField::Missing)
            );
            why[0].found.clone()
        };
        // Found and blob-defined: the image rule arms it, so `h2` may be absent.
        let found = recovered(Some(at.clone()));
        assert_eq!(
            found.check(&module("wifi_init_completed")),
            ModuleCheck::Bind
        );
        // The guard itself not found, in one place or in two: nothing stops an unhooked `h2`.
        for guard in [
            None,
            Some(missing()),
            Some(Resolution::Ambiguous(vec![4, 8])),
        ] {
            let why = refusal(&recovered(guard), "wifi_init_completed");
            assert!(
                why.contains("`wifi_init_completed` was not found either"),
                "{why}"
            );
        }
        // Found, but never armed: open-source code, and the coexistence code that runs for real.
        for guard in ["app_main", "coex_pre_init"] {
            let why = refusal(&found, guard);
            assert!(why.contains("is not a tripwire"), "{why}");
        }
        // A guard guards the hook it names and no other.
        let ModuleCheck::Refuse(why) = found.check(&ModuleSymbols {
            hooks: vec!["h1", "h2"],
            required: Vec::new(),
            guards: vec![("h1", "wifi_init_completed")],
        }) else {
            panic!("h2 has no guard");
        };
        assert_eq!(why[0].found, "no shape");
    }

    fn no_elf() -> ElfInfo {
        synthesize(
            &ImageRules {
                idf: String::new(),
                functions: Vec::new(),
                data: Vec::new(),
            },
            &[],
            &BTreeMap::new(),
            None,
            0,
        )
    }
}
