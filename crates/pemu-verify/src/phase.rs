//! The boot-phase record and its diff: one ordered stream per run interleaving entries into a
//! watched set of functions with the MMIO accesses, cut to a phase by two of those entries, then
//! compared per block and as a call trace.
//!
//! A QEMU memory-region trace line carries no PC and no time, so a boundary must be an event both
//! runs produce. The bootloader phase has no console line at either end (the ROM's `entry 0x`
//! prints before the ROM finishes, and the bootloader's last line comes before
//! `set_cache_and_start_app` programs EXTMEM and the MMU), so the boundaries are the bootloader's
//! and the app's `call_start_cpu0`, and the same entries are the call trace.
//!
//! **Record format** (`#!pemu-phase-record v1`), one event per line, anything else ignored:
//!
//! - `CALL <name>`: an entry into a watched function. App ELF names carry [`APP_PREFIX`].
//! - a `memory_region_ops_{read,write}` line. The oracle's are verbatim; ours add a `pc` field
//!   before `addr` and the region name `pemu`, and [`crate::qemu_ingest::parse_line`] reads both.
//!
//! The oracle's side comes from its log output, never its source, through [`ExecFilter`]; ours
//! from breakpoints on the same addresses plus the MMIO trace (`pemu_testkit::oracle_run`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pemu_loader::elf::ElfInfo;
use pemu_loader::symbols::{SymKind, SymSection};

use crate::calltrace::{self, CallDiff, CallTrace};
use crate::known_diffs::{EntryUse, KnownDiffs};
use crate::lcs::{BlockDiff, Ours, render};
use crate::qemu_ingest::{Kind, Record, RegionMap, parse_line};

pub const MAGIC: &str = "#!pemu-phase-record v1";

/// The region name our accesses carry in a record file.
pub const OUR_REGION: &str = "pemu";

/// The prefix of the names a record takes from the app ELF.
pub const APP_PREFIX: &str = "app:";

/// One event of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Call(String),
    Access(Access),
}

/// One MMIO access of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub kind: Kind,
    pub addr: u32,
    pub value: u64,
    pub size: u8,
    /// The PC of the access, which only our side knows.
    pub pc: Option<u32>,
}

/// An ordered stream of function entries and MMIO accesses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseRecord {
    /// Where it came from, for reports: `qemu`, `pemu`.
    pub source: String,
    pub events: Vec<Event>,
}

/// Why a record file cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The first line is not [`MAGIC`].
    NotARecord,
    /// A `memory_region_ops_` line that does not parse, by 1-based line number.
    Malformed(usize),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::NotARecord => write!(f, "the file does not start with `{MAGIC}`"),
            RecordError::Malformed(line) => {
                write!(f, "line {line} is a memory-region line that does not parse")
            }
        }
    }
}

impl PhaseRecord {
    pub fn new(source: impl Into<String>) -> PhaseRecord {
        PhaseRecord {
            source: source.into(),
            events: Vec::new(),
        }
    }

    /// Reads a record file. Lines that are neither a `CALL` nor a memory-region line are ignored;
    /// a memory-region line that does not parse is an error, so a garbled file never reads as a
    /// shorter stream.
    pub fn parse(source: impl Into<String>, text: &str) -> Result<PhaseRecord, RecordError> {
        let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
        if lines.next() != Some(MAGIC) {
            return Err(RecordError::NotARecord);
        }
        let mut record = PhaseRecord::new(source);
        for (at, line) in lines.enumerate() {
            if let Some(name) = line.strip_prefix("CALL ") {
                let name = name.trim();
                if !name.is_empty() {
                    record.events.push(Event::Call(name.to_string()));
                }
                continue;
            }
            // A header line may quote the command line, which names the trace events.
            if line.starts_with('#') || !line.contains("memory_region_ops_") {
                continue;
            }
            let access = parse_line(line).ok_or(RecordError::Malformed(at + 2))?;
            record.events.push(Event::Access(Access {
                kind: access.kind,
                addr: access.addr,
                value: access.value,
                size: access.size,
                pc: pc_field(line),
            }));
        }
        Ok(record)
    }

    /// The record as a file, headed by [`MAGIC`] and `header` as `#!` lines, in the line shape
    /// [`Self::parse`] reads.
    pub fn render(&self, header: &[(&str, String)]) -> String {
        let mut out = String::with_capacity(64 * self.events.len() + 256);
        out.push_str(MAGIC);
        out.push('\n');
        for (key, value) in header {
            let _ = writeln!(out, "#!{key}: {value}");
        }
        for event in &self.events {
            match event {
                Event::Call(name) => {
                    let _ = writeln!(out, "CALL {name}");
                }
                Event::Access(a) => {
                    let verb = match a.kind {
                        Kind::Read => "read",
                        Kind::Write => "write",
                    };
                    let pc = a.pc.map(|pc| format!(" pc {pc:#x}")).unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "memory_region_ops_{verb} cpu 0{pc} addr {:#x} value {:#x} size {} name '{OUR_REGION}'",
                        a.addr, a.value, a.size
                    );
                }
            }
        }
        out
    }

    /// The phase from the first entry into `from` through the first later entry into `to`, both
    /// kept. `None` when either entry is missing, which a caller reports rather than comparing.
    pub fn phase(&self, from: &str, to: &str) -> Option<PhaseRecord> {
        let is_call = |event: &Event, name: &str| matches!(event, Event::Call(n) if n == name);
        let start = self.events.iter().position(|e| is_call(e, from))?;
        let end = self.events[start + 1..]
            .iter()
            .position(|e| is_call(e, to))?
            + start
            + 1;
        Some(PhaseRecord {
            source: self.source.clone(),
            events: self.events[start..=end].to_vec(),
        })
    }

    pub fn calls(&self) -> CallTrace {
        CallTrace::from_names(
            self.source.clone(),
            self.events.iter().filter_map(|event| match event {
                Event::Call(name) => Some(name.clone()),
                Event::Access(_) => None,
            }),
        )
    }

    pub fn accesses(&self) -> impl Iterator<Item = &Access> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::Access(a) => Some(a),
            Event::Call(_) => None,
        })
    }

    /// The writes of each block, as the oracle side of a block diff: `index` is the position of
    /// the access among all accesses of the record (the `qemu_ingest` convention).
    pub fn oracle_writes(&self, map: &RegionMap) -> BTreeMap<String, Vec<Record>> {
        self.oracle_writes_with_reads(map)
            .into_iter()
            .map(|(block, writes)| (block, writes.into_iter().map(|(w, _)| w).collect()))
            .collect()
    }

    /// [`Self::oracle_writes`], each with its read-back: the value of the last read of the same
    /// address after the previous write to it, `None` when there was none
    /// (`KnownDiffs::apply_readback`).
    pub fn oracle_writes_with_reads(
        &self,
        map: &RegionMap,
    ) -> BTreeMap<String, Vec<(Record, Option<u64>)>> {
        let mut streams: BTreeMap<String, Vec<(Record, Option<u64>)>> = BTreeMap::new();
        let mut last_read: BTreeMap<u32, u64> = BTreeMap::new();
        for (index, a) in self.accesses().enumerate() {
            if a.kind == Kind::Read {
                last_read.insert(a.addr, a.value);
                continue;
            }
            let read = last_read.remove(&a.addr);
            if let Some((block, offset)) = map.resolve(a.addr) {
                streams.entry(block.name.clone()).or_default().push((
                    Record {
                        index,
                        kind: Kind::Write,
                        offset,
                        size: a.size,
                        value: a.value,
                    },
                    read,
                ));
            }
        }
        streams
    }

    /// The writes of each block, as our side of a block diff: `index` is the position inside the
    /// block's stream (the `lcs::Ours` convention), and `symbol` names the function the PC lies
    /// in when `symbolize` knows it.
    pub fn our_writes(
        &self,
        map: &RegionMap,
        symbolize: &dyn Fn(u32) -> Option<String>,
    ) -> BTreeMap<String, Vec<Ours>> {
        self.our_writes_with_reads(map, symbolize)
            .into_iter()
            .map(|(block, writes)| (block, writes.into_iter().map(|(w, _)| w).collect()))
            .collect()
    }

    /// [`Self::our_writes`], each with its read-back, as [`Self::oracle_writes_with_reads`].
    pub fn our_writes_with_reads(
        &self,
        map: &RegionMap,
        symbolize: &dyn Fn(u32) -> Option<String>,
    ) -> BTreeMap<String, Vec<(Ours, Option<u64>)>> {
        let mut streams: BTreeMap<String, Vec<(Ours, Option<u64>)>> = BTreeMap::new();
        let mut last_read: BTreeMap<u32, u64> = BTreeMap::new();
        for a in self.accesses() {
            if a.kind == Kind::Read {
                last_read.insert(a.addr, a.value);
                continue;
            }
            let read = last_read.remove(&a.addr);
            if let Some((block, offset)) = map.resolve(a.addr) {
                let stream = streams.entry(block.name.clone()).or_default();
                let pc = a.pc.unwrap_or(0);
                stream.push((
                    Ours {
                        index: stream.len(),
                        offset,
                        size: a.size,
                        value: a.value,
                        pc,
                        symbol: a.pc.and_then(symbolize),
                    },
                    read,
                ));
            }
        }
        streams
    }

    pub fn read_offsets(&self, map: &RegionMap, block: &str) -> Vec<u32> {
        self.accesses()
            .filter(|a| a.kind == Kind::Read)
            .filter_map(|a| map.resolve(a.addr))
            .filter(|(b, _)| b.name == block)
            .map(|(_, offset)| offset)
            .collect()
    }

    pub fn unmapped_writes(&self, map: &RegionMap) -> usize {
        self.accesses()
            .filter(|a| a.kind == Kind::Write && map.resolve(a.addr).is_none())
            .count()
    }
}

fn pc_field(line: &str) -> Option<u32> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "pc" {
            let value = tokens.next()?;
            return u32::from_str_radix(value.strip_prefix("0x")?, 16).ok();
        }
    }
    None
}

/// The watched set of a phase: function start address to name.
pub type Watch = BTreeMap<u32, String>;

/// Every function symbol of `elf` with a size and a section, by start address, names prefixed
/// with `prefix`. Where two names share an address the alphabetically first is kept, so the set
/// does not depend on symbol-table order.
pub fn function_starts(elf: &ElfInfo, prefix: &str) -> Watch {
    let mut watch = Watch::new();
    for symbol in elf.symbols.iter() {
        if symbol.kind != SymKind::Func
            || symbol.size == 0
            || !matches!(symbol.section, SymSection::Index(_))
        {
            continue;
        }
        let name = format!("{prefix}{}", symbol.name);
        watch
            .entry(symbol.addr)
            .and_modify(|kept| {
                if name < *kept {
                    kept.clone_from(&name);
                }
            })
            .or_insert(name);
    }
    watch
}

/// Turns a QEMU `-d exec,nochain,trace:memory_region_ops_*` log into record lines: a translation
/// block starting at a watched function's first instruction is an entry into it, since compiled
/// code never falls through into one. Under `-icount` an I/O instruction makes QEMU rewind and
/// re-run its block (`cpu_io_recompile: rewound execution of TB to <pc>`, then a `Trace` at that
/// PC): a continuation, not a new entry.
///
/// Checked against gdb breakpoints on the 118 sized `pk` bootloader functions: the same 1,371
/// entries in the same order.
#[derive(Clone, Debug)]
pub struct ExecFilter {
    watch: Watch,
    end: u32,
    rewound: Option<u32>,
    done: bool,
    blocks: u64,
}

impl ExecFilter {
    /// A filter that names the entries of `watch` and ends at the first entry into `end`, which
    /// must be one of its addresses.
    pub fn new(watch: Watch, end: u32) -> ExecFilter {
        ExecFilter {
            watch,
            end,
            rewound: None,
            done: false,
            blocks: 0,
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn feed(&mut self, line: &str) -> Option<String> {
        if self.done {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = line.strip_prefix("cpu_io_recompile: rewound execution of TB to ") {
            self.rewound = u64::from_str_radix(rest.trim(), 16)
                .ok()
                .and_then(|pc| u32::try_from(pc).ok());
            return None;
        }
        if line.starts_with("Trace ") {
            self.blocks += 1;
            let pc = exec_pc(line)?;
            if self.rewound.take() == Some(pc) {
                return None;
            }
            if pc == self.end {
                self.done = true;
            }
            return self.watch.get(&pc).map(|name| format!("CALL {name}"));
        }
        // Only a `Trace` line consumes a rewind: the access that caused it is printed after the
        // re-run starts, and other log chatter may sit between the two.
        line.contains("memory_region_ops_")
            .then(|| line.to_string())
    }
}

/// The guest PC of a `Trace` line: `Trace <cpu>: <host ptr> [<a>/<pc>/<flags>/<cflags>] ...`.
fn exec_pc(line: &str) -> Option<u32> {
    let inside = line.split_once('[')?.1.split_once(']')?.0;
    let pc = inside.split('/').nth(1)?;
    u32::try_from(u64::from_str_radix(pc, 16).ok()?).ok()
}

#[derive(Clone, Debug)]
pub struct BlockReport {
    pub diff: BlockDiff,
    /// What each known-diffs entry excused or rewrote in this block.
    pub excused: Vec<EntryUse>,
}

/// A whole phase diff: every named block and the call trace.
#[derive(Clone, Debug)]
pub struct PhaseDiff {
    pub blocks: Vec<BlockReport>,
    /// The call-trace comparison, with the `call` entries applied.
    pub calls: CallDiff,
    pub call_excused: Vec<EntryUse>,
    pub traces: (CallTrace, CallTrace),
}

impl PhaseDiff {
    pub fn is_clean(&self) -> bool {
        self.calls.is_equal() && self.blocks.iter().all(|b| b.diff.is_equal())
    }

    /// The report: one summary line per block and for the call trace, then the rendered first
    /// divergence of everything that is not clean.
    pub fn render(&self, what: &str) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            let d = &block.diff;
            let _ = writeln!(
                out,
                "{what}: block {}: {} of our writes, {} oracle writes, {} aligned, {}",
                d.block,
                d.ours_len,
                d.oracle_len,
                d.aligned,
                if d.is_equal() { "clean" } else { "DIVERGES" }
            );
            for entry in &block.excused {
                let _ = writeln!(
                    out,
                    "{what}: block {}: `{}` excused {} divergences, rewrote {} of our values",
                    d.block, entry.id, entry.excused, entry.rewritten
                );
            }
        }
        let _ = writeln!(
            out,
            "{what}: call trace: {} entries of ours, {} of the oracle's, {} aligned, {}",
            self.calls.ours_len,
            self.calls.oracle_len,
            self.calls.aligned,
            if self.calls.is_equal() {
                "equal"
            } else {
                "DIVERGES"
            }
        );
        for entry in &self.call_excused {
            let _ = writeln!(
                out,
                "{what}: call trace: `{}` excused {} entries",
                entry.id, entry.excused
            );
        }
        for block in self.blocks.iter().filter(|b| !b.diff.is_equal()) {
            out.push_str(&render(&block.diff));
        }
        if !self.calls.is_equal() {
            out.push_str(&calltrace::render(
                &self.traces.0,
                &self.traces.1,
                &self.calls,
            ));
        }
        out
    }
}

/// Diffs one phase: the write stream of each of `blocks` with `known`'s entries for `oracle`
/// applied (`readback` entries first, then value rewrites and excused extra writes), and the
/// function-entry order with its `call` entries applied. Both records must already be cut
/// ([`PhaseRecord::phase`]).
pub fn diff_phase(
    ours: &PhaseRecord,
    theirs: &PhaseRecord,
    map: &RegionMap,
    known: &KnownDiffs,
    oracle: &str,
    blocks: &[&str],
    symbolize: &dyn Fn(u32) -> Option<String>,
) -> PhaseDiff {
    let mut our_writes = ours.our_writes_with_reads(map, symbolize);
    let mut their_writes = theirs.oracle_writes_with_reads(map);
    let reports = blocks
        .iter()
        .map(|block| {
            let (mut a, a_reads): (Vec<Ours>, Vec<Option<u64>>) = our_writes
                .remove(*block)
                .unwrap_or_default()
                .into_iter()
                .unzip();
            let (mut b, b_reads): (Vec<Record>, Vec<Option<u64>>) = their_writes
                .remove(*block)
                .unwrap_or_default()
                .into_iter()
                .unzip();
            let mut excused =
                known.apply_readback(oracle, block, &mut a, &a_reads, &mut b, &b_reads);
            let (diff, counted) = known.diff_block_counted(oracle, block, &a, &b);
            excused.extend(counted);
            BlockReport { diff, excused }
        })
        .collect();
    let traces = (ours.calls(), theirs.calls());
    let (calls, call_excused) = known.diff_calls(oracle, &traces.0, &traces.1);
    PhaseDiff {
        blocks: reports,
        calls,
        call_excused,
        traces,
    }
}

/// The entry that opens the bootloader phase: the bootloader's own `call_start_cpu0`, where the
/// ROM jumps once the image is loaded.
pub const BOOTLOADER_ENTRY: &str = "call_start_cpu0";

/// The entry that closes it: the app's `call_start_cpu0`, where `set_cache_and_start_app` jumps.
pub const APP_ENTRY: &str = "app:call_start_cpu0";

/// The blocks of the bootloader-phase comparison, by their `specs/oracle-qemu-regions.toml`
/// names: SHA, both TIMG groups, EXTMEM and the MMU table, SPI1, eFuse and RTC_CNTL.
pub const BOOTLOADER_PHASE_BLOCKS: [&str; 8] = [
    "sha", "timg0", "timg1", "extmem", "mmu", "spi1", "efuse", "rtc_cntl",
];

/// The watched set of the bootloader phase and its end address: every function of the bootloader
/// ELF (about a hundred with a body), plus the app's `call_start_cpu0` as [`APP_ENTRY`].
pub fn boot_watch(boot: &ElfInfo, app: &ElfInfo) -> Result<(Watch, u32), String> {
    let mut watch = function_starts(boot, "");
    if !watch.values().any(|name| name == BOOTLOADER_ENTRY) {
        return Err(format!("the bootloader ELF has no `{BOOTLOADER_ENTRY}`"));
    }
    let end = app
        .symbols
        .addr_of("call_start_cpu0")
        .ok_or("the app ELF has no `call_start_cpu0`")?;
    if let Some(clash) = watch.insert(end, APP_ENTRY.to_string()) {
        return Err(format!(
            "the app entry {end:#010x} is also the bootloader's `{clash}`"
        ));
    }
    Ok((watch, end))
}

/// The bootloader-phase comparison: the [`BOOTLOADER_PHASE_BLOCKS`] write streams, the call trace,
/// and the eFuse read-offset sequence while neither side writes the eFuse.
#[derive(Clone, Debug)]
pub struct BootPhaseReport {
    pub diff: PhaseDiff,
    /// Accesses of each side inside the phase, and our writes that resolve to no block.
    pub accesses: (usize, usize, usize),
    /// The eFuse read-offset leg: `Ok(n)` when both sides read the same `n` offsets in order,
    /// `Err` with the first difference, `None` when the eFuse write stream is not empty and the
    /// write diff covers it.
    pub efuse_reads: Option<Result<usize, String>>,
}

impl BootPhaseReport {
    pub fn is_clean(&self) -> bool {
        self.diff.is_clean() && !matches!(self.efuse_reads, Some(Err(_)))
    }

    pub fn render(&self, what: &str) -> String {
        let (ours, theirs, unmapped) = self.accesses;
        let mut out = format!(
            "{what}: {ours} accesses of ours, {theirs} of the oracle's, {unmapped} of our writes \
             outside every block\n"
        );
        match &self.efuse_reads {
            None => {}
            Some(Ok(n)) => {
                let _ = writeln!(
                    out,
                    "{what}: block efuse: 0 writes on both sides; {n} read offsets equal the \
                     oracle's"
                );
            }
            Some(Err(why)) => {
                let _ = writeln!(out, "{what}: block efuse: read offsets DIVERGE: {why}");
            }
        }
        out.push_str(&self.diff.render(what));
        out
    }
}

/// Cuts both whole-boot records to the bootloader phase and compares them. `Err` when either
/// record does not hold the phase: a broken record, not a divergence.
pub fn diff_boot_phase(
    ours: &PhaseRecord,
    theirs: &PhaseRecord,
    map: &RegionMap,
    known: &KnownDiffs,
    oracle: &str,
    symbolize: &dyn Fn(u32) -> Option<String>,
) -> Result<BootPhaseReport, String> {
    let cut = |record: &PhaseRecord| {
        record.phase(BOOTLOADER_ENTRY, APP_ENTRY).ok_or_else(|| {
            format!(
                "the {} record holds no `{BOOTLOADER_ENTRY}` entry followed by `{APP_ENTRY}`",
                record.source
            )
        })
    };
    let (ours, theirs) = (cut(ours)?, cut(theirs)?);
    let diff = diff_phase(
        &ours,
        &theirs,
        map,
        known,
        oracle,
        &BOOTLOADER_PHASE_BLOCKS,
        symbolize,
    );
    let efuse_written = diff
        .blocks
        .iter()
        .any(|b| b.diff.block == "efuse" && (b.diff.ours_len > 0 || b.diff.oracle_len > 0));
    let efuse_reads = (!efuse_written).then(|| {
        let (a, b) = (
            ours.read_offsets(map, "efuse"),
            theirs.read_offsets(map, "efuse"),
        );
        match a.iter().zip(&b).position(|(x, y)| x != y) {
            None if a.len() == b.len() => Ok(a.len()),
            first => Err(format!(
                "{} reads here, {} there; first difference at {:?}: {:?} against {:?}",
                a.len(),
                b.len(),
                first,
                first.map(|i| a[i]),
                first.map(|i| b[i])
            )),
        }
    });
    Ok(BootPhaseReport {
        accesses: (
            ours.accesses().count(),
            theirs.accesses().count(),
            ours.unmapped_writes(map),
        ),
        diff,
        efuse_reads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP_TEXT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/oracle-qemu-regions.toml"
    ));

    fn map() -> RegionMap {
        RegionMap::parse(MAP_TEXT).expect("the committed region map parses")
    }

    fn write(addr: u32, value: u64, pc: Option<u32>) -> Event {
        Event::Access(Access {
            kind: Kind::Write,
            addr,
            value,
            size: 4,
            pc,
        })
    }

    fn call(name: &str) -> Event {
        Event::Call(name.to_string())
    }

    /// A boot-shaped record: ROM writes, the bootloader entry, a SHA and an MMU write with a
    /// call between them, the app entry, and an app write after it.
    fn boot(source: &str, sha_value: u64, pc: Option<u32>) -> PhaseRecord {
        PhaseRecord {
            source: source.to_string(),
            events: vec![
                write(0x6000_8000, 1, pc),
                call("call_start_cpu0"),
                call("bootloader_init"),
                write(0x6003_B000, sha_value, pc),
                call("bootloader_sha256_data"),
                write(0x600C_5000, 0x40, pc),
                call("app:call_start_cpu0"),
                write(0x6003_B000, 7, pc),
            ],
        }
    }

    const BLOCKS: [&str; 3] = ["sha", "mmu", "rtc_cntl"];

    fn diff_of(ours: &PhaseRecord, theirs: &PhaseRecord, known: &KnownDiffs) -> PhaseDiff {
        let cut = |r: &PhaseRecord| {
            r.phase("call_start_cpu0", "app:call_start_cpu0")
                .expect("both boundaries are there")
        };
        diff_phase(
            &cut(ours),
            &cut(theirs),
            &map(),
            known,
            "qemu",
            &BLOCKS,
            &|_| None,
        )
    }

    #[test]
    fn a_record_survives_render_and_parse() {
        let record = boot("pemu", 3, Some(0x403c_e000));
        let text = record.render(&[("source", "pemu".to_string())]);
        assert!(text.starts_with(MAGIC));
        let back = PhaseRecord::parse("pemu", &text).expect("our own file parses");
        assert_eq!(back, record);
    }

    #[test]
    fn an_oracle_log_line_parses_without_a_pc() {
        let text = format!(
            "{MAGIC}\n#!command: qemu -d trace:memory_region_ops_write\nCALL call_start_cpu0\nmemory_region_ops_write cpu 0 mr \
             0x7cb6804c10 addr 0x600c2104 value 0x0 size 4 name 'misc.esp32c3.intmatrix'\n"
        );
        let record = PhaseRecord::parse("qemu", &text).expect("parses");
        assert_eq!(
            record.events,
            vec![
                call("call_start_cpu0"),
                Event::Access(Access {
                    kind: Kind::Write,
                    addr: 0x600c_2104,
                    value: 0,
                    size: 4,
                    pc: None
                })
            ]
        );
    }

    #[test]
    fn a_file_without_the_magic_or_with_a_garbled_access_is_refused() {
        assert_eq!(
            PhaseRecord::parse("x", "CALL a\n"),
            Err(RecordError::NotARecord)
        );
        let text = format!("{MAGIC}\nCALL a\nmemory_region_ops_write cpu 0 addr zz\n");
        assert_eq!(
            PhaseRecord::parse("x", &text),
            Err(RecordError::Malformed(3))
        );
    }

    #[test]
    fn the_phase_keeps_both_boundaries_and_drops_what_is_outside() {
        let phase = boot("qemu", 3, None)
            .phase("call_start_cpu0", "app:call_start_cpu0")
            .expect("cut");
        assert_eq!(
            phase.calls().names(),
            [
                "call_start_cpu0",
                "bootloader_init",
                "bootloader_sha256_data",
                "app:call_start_cpu0"
            ]
        );
        let writes = phase.oracle_writes(&map());
        assert_eq!(writes["sha"].len(), 1, "the app's SHA write is outside");
        assert!(
            !writes.contains_key("rtc_cntl"),
            "the ROM's write is outside"
        );
        assert_eq!(writes["mmu"][0].offset, 0);
        let mut no_end = boot("qemu", 3, None);
        no_end.events.retain(|e| e != &call("app:call_start_cpu0"));
        assert_eq!(no_end.phase("call_start_cpu0", "app:call_start_cpu0"), None);
    }

    #[test]
    fn equal_records_are_clean() {
        let d = diff_of(
            &boot("pemu", 3, Some(0x403c_0000)),
            &boot("qemu", 3, None),
            &KnownDiffs::default(),
        );
        assert!(d.is_clean(), "{}", d.render("t"));
        assert_eq!(d.blocks.len(), 3);
        assert!(
            d.render("t")
                .contains("block sha: 1 of our writes, 1 oracle writes")
        );
    }

    #[test]
    fn a_planted_write_difference_is_caught_in_its_block_only() {
        let d = diff_of(
            &boot("pemu", 3, Some(0x403c_0000)),
            &boot("qemu", 4, None),
            &KnownDiffs::default(),
        );
        assert!(!d.is_clean());
        let sha = &d.blocks[0].diff;
        assert_eq!(sha.block, "sha");
        assert!(!sha.is_equal(), "the SHA value differs");
        assert!(d.blocks[1].diff.is_equal() && d.blocks[2].diff.is_equal());
        assert!(d.calls.is_equal());
        assert!(d.render("t").contains("DIVERGES"));
    }

    #[test]
    fn a_planted_call_difference_is_caught() {
        let mut theirs = boot("qemu", 3, None);
        theirs.events.insert(3, call("bootloader_flash_read"));
        let d = diff_of(&boot("pemu", 3, Some(0)), &theirs, &KnownDiffs::default());
        assert!(!d.is_clean());
        assert!(d.blocks.iter().all(|b| b.diff.is_equal()));
        assert!(matches!(
            d.calls.first,
            Some(calltrace::Divergence::OnlyOracle(ref e)) if e.function == "bootloader_flash_read"
        ));
    }

    #[test]
    fn a_known_diff_excuses_only_the_register_it_names() {
        let entry = |offset_end: u32| {
            KnownDiffs::parse(&format!(
                "schema = 1\n[[diff]]\nid = \"sha.t\"\nkind = \"mmio\"\noracle = \"qemu\"\n\
                 block = \"sha\"\noffset = 0x000\noffset_end = {offset_end:#x}\naccess = \"write\"\n\
                 values = [[3, 4]]\nreason = \"test\"\n"
            ))
            .expect("parses")
        };
        let ours = boot("pemu", 3, Some(0));
        let theirs = boot("qemu", 4, None);
        let d = diff_of(&ours, &theirs, &entry(4));
        assert!(d.is_clean(), "{}", d.render("t"));
        assert_eq!(d.blocks[0].excused[0].id, "sha.t");
        assert_eq!(d.blocks[0].excused[0].rewritten, 1);
        // The same entry for another oracle excuses nothing.
        let other = diff_phase(
            &ours
                .phase("call_start_cpu0", "app:call_start_cpu0")
                .unwrap(),
            &theirs
                .phase("call_start_cpu0", "app:call_start_cpu0")
                .unwrap(),
            &map(),
            &entry(4),
            "esp32sim",
            &BLOCKS,
            &|_| None,
        );
        assert!(!other.is_clean());
        // Another value at the same register is not the listed pair.
        let d = diff_of(&ours, &boot("qemu", 5, None), &entry(4));
        assert!(!d.is_clean());
    }

    #[test]
    fn the_exec_filter_names_block_starts_and_skips_a_rewound_rerun() {
        let watch: Watch = [
            (0x403c_bf1a, "call_start_cpu0".to_string()),
            (0x403c_c000, "bootloader_init".to_string()),
            (0x4038_02e8, "app:call_start_cpu0".to_string()),
        ]
        .into();
        let mut f = ExecFilter::new(watch, 0x4038_02e8);
        let log = [
            "Adding SPI flash device",
            "Trace 0: 0x7000000180 [00000000/0000000040000000/07014003/ff022200] ",
            "Trace 0: 0x7000000300 [00000000/00000000403cbf1a/07014003/ff022200] ",
            "Trace 0: 0x7000000400 [00000000/00000000403cc000/07014003/ff022200] ",
            "cpu_io_recompile: rewound execution of TB to 00000000403cc000",
            "Trace 0: 0x7000000800 [00000000/00000000403cc000/07014003/ff023201] ",
            "memory_region_ops_write cpu 0 mr 0x7c addr 0x600c2104 value 0x0 size 4 name 'x'",
            "Trace 0: 0x7000000a00 [00000000/00000000403cc000/07014003/ff022200] ",
            "Trace 0: 0x7000000b00 [00000000/00000000403802e8/07014003/ff022200] ",
            "memory_region_ops_write cpu 0 mr 0x7c addr 0x600c2108 value 0x0 size 4 name 'x'",
        ];
        let out: Vec<String> = log.iter().filter_map(|l| f.feed(l)).collect();
        assert_eq!(
            out,
            [
                "CALL call_start_cpu0",
                "CALL bootloader_init",
                "memory_region_ops_write cpu 0 mr 0x7c addr 0x600c2104 value 0x0 size 4 name 'x'",
                "CALL bootloader_init",
                "CALL app:call_start_cpu0",
            ]
        );
        assert!(f.done());
        assert_eq!(f.blocks(), 6);
    }

    #[test]
    fn a_rewind_to_another_pc_does_not_hide_the_next_entry() {
        let watch: Watch = [(0x100, "f".to_string()), (0x200, "end".to_string())].into();
        let mut f = ExecFilter::new(watch, 0x200);
        let out: Vec<String> = [
            "cpu_io_recompile: rewound execution of TB to 0000000000000104",
            "Trace 0: 0x1 [00000000/0000000000000100/0/0] ",
            "Trace 0: 0x1 [00000000/0000000000000200/0/0] ",
        ]
        .iter()
        .filter_map(|l| f.feed(l))
        .collect();
        assert_eq!(out, ["CALL f", "CALL end"]);
    }

    fn read(addr: u32, value: u64) -> Event {
        Event::Access(Access {
            kind: Kind::Read,
            addr,
            value,
            size: 4,
            pc: None,
        })
    }

    /// A read-modify-write of RTC_CNTL_WDTCONFIG0 inside the phase: `read_back` read, `written`
    /// written.
    fn rmw(source: &str, read_back: u64, written: u64) -> PhaseRecord {
        PhaseRecord {
            source: source.to_string(),
            events: vec![
                call("call_start_cpu0"),
                read(0x6000_8090, read_back),
                write(0x6000_8090, written, None),
                call("app:call_start_cpu0"),
            ],
        }
    }

    fn readback_entry() -> KnownDiffs {
        KnownDiffs::parse(
            "schema = 1\n[[diff]]\nid = \"rtc.rb\"\nkind = \"mmio\"\noracle = \"qemu\"\n\
             block = \"rtc_cntl\"\noffset = 0x090\noffset_end = 0x094\naccess = \"write\"\n\
             readback = true\nreason = \"test\"\n",
        )
        .expect("parses")
    }

    #[test]
    fn a_carried_read_back_value_is_excused_only_by_a_readback_entry() {
        let ours = rmw("pemu", 0x4_8000, 0x44_8000);
        let theirs = rmw("qemu", 0, 0x40_0000);
        assert!(
            !diff_of(&ours, &theirs, &KnownDiffs::default()).is_clean(),
            "without an entry the carried bits are a divergence"
        );
        let d = diff_of(&ours, &theirs, &readback_entry());
        assert!(d.is_clean(), "{}", d.render("t"));
        assert_eq!(d.blocks[2].excused[0].id, "rtc.rb");
        // A bit the code wrote differently is still reported.
        let changed = rmw("pemu", 0x4_8000, 0x44_8001);
        assert!(!diff_of(&changed, &theirs, &readback_entry()).is_clean());
    }

    #[test]
    fn the_read_back_of_a_write_is_the_last_read_since_the_previous_write() {
        let record = PhaseRecord {
            source: "pemu".into(),
            events: vec![
                read(0x6000_8090, 1),
                read(0x6000_8090, 2),
                write(0x6000_8090, 3, Some(0)),
                write(0x6000_8090, 4, Some(0)),
            ],
        };
        let writes = record.our_writes_with_reads(&map(), &|_| None);
        let reads: Vec<Option<u64>> = writes["rtc_cntl"].iter().map(|(_, r)| *r).collect();
        assert_eq!(reads, [Some(2), None]);
        let theirs = record.oracle_writes_with_reads(&map());
        let reads: Vec<Option<u64>> = theirs["rtc_cntl"].iter().map(|(_, r)| *r).collect();
        assert_eq!(reads, [Some(2), None]);
    }

    /// A whole-boot record for [`diff_boot_phase`]: an eFuse read before the phase, then eFuse
    /// reads at `offsets` and one SHA write inside it.
    fn efuse_boot(source: &str, offsets: &[u32]) -> PhaseRecord {
        let mut events = vec![read(0x6000_8800, 0), call(BOOTLOADER_ENTRY)];
        events.extend(offsets.iter().map(|o| read(0x6000_8800 + o, 0)));
        events.push(write(0x6003_B000, 1, Some(0)));
        events.push(call(APP_ENTRY));
        PhaseRecord {
            source: source.to_string(),
            events,
        }
    }

    fn boot_diff(ours: &PhaseRecord, theirs: &PhaseRecord) -> Result<BootPhaseReport, String> {
        diff_boot_phase(
            ours,
            theirs,
            &map(),
            &KnownDiffs::default(),
            "qemu",
            &|_| None,
        )
    }

    #[test]
    fn the_efuse_leg_compares_read_offsets_while_neither_side_writes_it() {
        let ours = efuse_boot("pemu", &[0x44, 0x5c]);
        let report = boot_diff(&ours, &efuse_boot("qemu", &[0x44, 0x5c])).expect("both cut");
        assert!(report.is_clean(), "{}", report.render("t"));
        assert_eq!(
            report.efuse_reads,
            Some(Ok(2)),
            "the read before the phase is outside"
        );
        let reordered = boot_diff(&ours, &efuse_boot("qemu", &[0x5c, 0x44])).expect("both cut");
        assert!(!report_is_clean_efuse(&reordered));
        let shorter = boot_diff(&ours, &efuse_boot("qemu", &[0x44])).expect("both cut");
        assert!(!report_is_clean_efuse(&shorter));
        assert!(shorter.render("t").contains("read offsets DIVERGE"));
    }

    /// Whether a report is clean, asserting that its blocks and calls are, so only the eFuse
    /// leg decides.
    fn report_is_clean_efuse(report: &BootPhaseReport) -> bool {
        assert!(report.diff.is_clean(), "{}", report.render("t"));
        report.is_clean()
    }

    #[test]
    fn an_efuse_write_moves_the_efuse_leg_to_the_write_diff() {
        let mut ours = efuse_boot("pemu", &[0x44]);
        ours.events.insert(2, write(0x6000_8800 + 0x1c, 1, Some(0)));
        let mut theirs = efuse_boot("qemu", &[0x5c]);
        theirs.events.insert(2, write(0x6000_8800 + 0x1c, 1, None));
        let report = boot_diff(&ours, &theirs).expect("both cut");
        assert_eq!(report.efuse_reads, None);
        assert!(report.is_clean(), "{}", report.render("t"));
    }

    #[test]
    fn a_record_without_the_phase_is_an_error_not_a_clean_diff() {
        let ours = efuse_boot("pemu", &[0x44]);
        let mut theirs = efuse_boot("qemu", &[0x44]);
        theirs.events.retain(|e| e != &call(APP_ENTRY));
        let err = boot_diff(&ours, &theirs).expect_err("no end entry");
        assert!(err.contains("qemu"), "{err}");
    }
}
