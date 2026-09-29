//! The QEMU oracle side of a T2 comparison, shared by m1.rs, m2.rs and m3.rs.
//!
//! `cargo xtask oracle consoles` writes the oracle outputs below the data root; a test reads them
//! there and skips when they are absent. A failure names indices, offsets and register values,
//! never oracle console text.
//!
//! Both write streams are resolved to `(block, offset)` through `specs/oracle-qemu-regions.toml`,
//! cut at the same instant, and compared per block with `KnownDiffs::diff_block`. A QEMU trace
//! line carries no time and no PC, so the cut is the USJ EP1 write of the `\n` that ends a console
//! line both runs print: the ROM phase ends at `entry 0x`; the app phase runs from the bootloader's
//! `Disabling RNG early entropy source` to the first `bsp_i2c` line.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use pemu_core::trace::{TraceEvent, TraceKinds, TraceRecord};
use pemu_machine::config::TraceCfg;
use pemu_machine::machine::Machine;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::StopReason;
use pemu_verify::known_diffs::KnownDiffs;
use pemu_verify::lcs::{BlockDiff, Ours, render};
use pemu_verify::qemu_ingest::{self, Record, RegionMap};

use crate::common;

/// The oracle name the entries of `specs/oracle-known-diffs.toml` scope to.
pub const QEMU: &str = "qemu";

/// The USJ EP1 data register, which carries every console byte of both runs.
const USJ_EP1: u32 = 0x000;

/// A file below `<data root>/oracles/consoles/`, or `None` after printing why the test skips.
pub fn oracle_file_or_skip(test: &str, name: &str) -> Option<PathBuf> {
    let root = match pemu_testkit::corpus::data_root_from_env() {
        Ok(root) => root,
        Err(e) => {
            common::skip(test, &format!("oracle `{name}`: {e}"));
            return None;
        }
    };
    let path = root.join("oracles").join("consoles").join(name);
    if path.is_file() {
        Some(path)
    } else {
        common::skip(
            test,
            &format!(
                "oracle `{name}` is absent below the data root; `cargo xtask oracle consoles` \
                 regenerates it"
            ),
        );
        None
    }
}

fn spec(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("`{rel}` is readable: {e}"))
}

/// The block table of `specs/oracle-qemu-regions.toml`.
pub fn regions() -> RegionMap {
    RegionMap::parse(&spec("specs/oracle-qemu-regions.toml")).expect("the region map parses")
}

/// The suppression list of `specs/oracle-known-diffs.toml`.
pub fn known_diffs() -> KnownDiffs {
    KnownDiffs::parse(&spec("specs/oracle-known-diffs.toml")).expect("the known diffs parse")
}

/// The trace configuration a write-stream comparison runs with.
pub fn write_trace() -> TraceCfg {
    TraceCfg {
        kinds: Some(TraceKinds::MMIO_WRITE),
        recent: 1 << 16,
    }
}

/// The trace configuration a comparison of write streams and read-offset sequences runs with.
pub fn access_trace() -> TraceCfg {
    TraceCfg {
        kinds: Some(TraceKinds::MMIO_WRITE.union(TraceKinds::MMIO_READ)),
        recent: 1 << 16,
    }
}

/// One MMIO access of our run, in run order: a write, or (under [`access_trace`]) a read, with a
/// folded poll run expanded to its reads.
#[derive(Clone, Copy, Debug)]
pub struct Write {
    pub pc: u32,
    pub addr: u32,
    pub val: u32,
    pub size: u8,
    pub read: bool,
}

/// Runs `m` under `limits` in slices, draining every MMIO write the trace window holds. The window
/// never drops a record between two drains (asserted), so the list is the whole write stream.
pub fn run_collecting_writes(m: &mut Machine, limits: RunLimits) -> (StopReason, Vec<Write>) {
    const SLICE: u64 = 20_000;
    let mut writes = Vec::new();
    let mut cursor = m.trace().head();
    let mut left = limits.max_insns;
    loop {
        let slice = left.map_or(SLICE, |l| l.min(SLICE));
        let out = m.run(RunLimits {
            until: limits.until,
            max_insns: Some(slice),
            stops: limits.stops.clone(),
        });
        let (tail, head) = (m.trace().tail(), m.trace().head());
        assert!(tail <= cursor, "the write trace window dropped records");
        for rec in m.trace().records().skip((cursor - tail) as usize) {
            match rec {
                TraceRecord {
                    ev:
                        TraceEvent::MmioWrite {
                            pc,
                            addr,
                            val,
                            size,
                        },
                    ..
                } => writes.push(Write {
                    pc,
                    addr,
                    val,
                    size,
                    read: false,
                }),
                TraceRecord {
                    ev:
                        TraceEvent::MmioRead {
                            pc,
                            addr,
                            val,
                            size,
                        },
                    ..
                } => writes.push(Write {
                    pc,
                    addr,
                    val,
                    size,
                    read: true,
                }),
                TraceRecord {
                    ev:
                        TraceEvent::PollRun {
                            pc,
                            addr,
                            val,
                            size,
                            count,
                        },
                    ..
                } => {
                    for _ in 0..count {
                        writes.push(Write {
                            pc,
                            addr,
                            val,
                            size,
                            read: true,
                        });
                    }
                }
                _ => {}
            }
        }
        cursor = head;
        if let Some(l) = left.as_mut() {
            *l = l.saturating_sub(out.insns);
        }
        if out.reason != StopReason::MaxInsns || left == Some(0) {
            return (out.reason, writes);
        }
    }
}

/// The positions, in a sequence of `(position, offset, value)` USJ writes, of the EP1 writes of
/// the `\n` that ends the first console line containing `from` (`None`: before the first write)
/// and of the `\n` that ends the first later line containing `to`.
fn span<I: Iterator<Item = (usize, u32, u64)>>(
    usj: I,
    from: Option<&str>,
    to: &str,
) -> Option<(Option<usize>, usize)> {
    let mut start = None;
    let mut want = from;
    let mut line = Vec::new();
    for (at, offset, value) in usj {
        if offset != USJ_EP1 {
            continue;
        }
        let byte = value as u8;
        if byte != b'\n' {
            if byte != b'\r' {
                line.push(byte);
            }
            continue;
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        line.clear();
        match want {
            Some(marker) if text.contains(marker) => {
                start = Some(at);
                want = None;
            }
            Some(_) => {}
            None if text.contains(to) => return Some((start, at)),
            None => {}
        }
    }
    None
}

/// Our writes after the `\n` of the first line containing `from` (from power-on when `None`)
/// up to and including the `\n` of the next line containing `to`, per block of `map`, with the
/// number of writes in that span that resolved to no block.
pub fn our_streams(
    map: &RegionMap,
    writes: &[Write],
    from: Option<&str>,
    to: &str,
) -> Option<(BTreeMap<String, Vec<Ours>>, usize)> {
    let resolved: Vec<Option<(String, u32)>> = writes
        .iter()
        .map(|w| map.resolve(w.addr).map(|(b, off)| (b.name.clone(), off)))
        .collect();
    let usj = resolved
        .iter()
        .zip(writes)
        .enumerate()
        .filter_map(|(at, (r, w))| match r {
            Some((block, off)) if block == "usj" && !w.read => Some((at, *off, u64::from(w.val))),
            _ => None,
        });
    let (start, end) = span(usj, from, to)?;
    let first = start.map_or(0, |s| s + 1);
    let mut streams: BTreeMap<String, Vec<Ours>> = BTreeMap::new();
    let mut unmapped = 0;
    for (r, w) in resolved[first..=end].iter().zip(&writes[first..=end]) {
        if w.read {
            continue;
        }
        let Some((block, offset)) = r else {
            unmapped += 1;
            continue;
        };
        let stream = streams.entry(block.clone()).or_default();
        stream.push(Ours {
            index: stream.len(),
            offset: *offset,
            size: w.size,
            value: u64::from(w.val),
            pc: w.pc,
            symbol: None,
        });
    }
    Some((streams, unmapped))
}

/// The offsets our run read in `block`, in order, over the span of [`our_streams`] (the accesses
/// must come from a run under [`access_trace`]).
pub fn our_read_offsets(
    map: &RegionMap,
    accesses: &[Write],
    block: &str,
    from: Option<&str>,
    to: &str,
) -> Option<Vec<u32>> {
    let resolved: Vec<Option<(&str, u32)>> = accesses
        .iter()
        .map(|w| map.resolve(w.addr).map(|(b, off)| (b.name.as_str(), off)))
        .collect();
    let usj = resolved
        .iter()
        .zip(accesses)
        .enumerate()
        .filter_map(|(at, (r, w))| match r {
            Some(("usj", off)) if !w.read => Some((at, *off, u64::from(w.val))),
            _ => None,
        });
    let (start, end) = span(usj, from, to)?;
    let first = start.map_or(0, |s| s + 1);
    Some(
        resolved[first..=end]
            .iter()
            .zip(&accesses[first..=end])
            .filter_map(|(r, w)| match r {
                Some((b, off)) if *b == block && w.read => Some(*off),
                _ => None,
            })
            .collect(),
    )
}

/// The offsets the oracle read in `block`, in order, over the same span.
pub fn oracle_read_offsets(
    map: &RegionMap,
    trace: &str,
    block: &str,
    from: Option<&str>,
    to: &str,
) -> Option<Vec<u32>> {
    let ingest = qemu_ingest::ingest(trace, map);
    let usj = ingest.writes("usj");
    let (start, end) = span(usj.iter().map(|r| (r.index, r.offset, r.value)), from, to)?;
    Some(
        ingest
            .reads(block)
            .into_iter()
            .filter(|r| start.is_none_or(|s| r.index > s) && r.index <= end)
            .map(|r| r.offset)
            .collect(),
    )
}

/// The oracle's writes over the same span as [`our_streams`], per block.
pub fn oracle_streams(
    map: &RegionMap,
    trace: &str,
    from: Option<&str>,
    to: &str,
) -> Option<BTreeMap<String, Vec<Record>>> {
    let ingest = qemu_ingest::ingest(trace, map);
    let usj = ingest.writes("usj");
    let (start, end) = span(usj.iter().map(|r| (r.index, r.offset, r.value)), from, to)?;
    let inside = |r: &Record| start.is_none_or(|s| r.index > s) && r.index <= end;
    Some(
        ingest
            .streams
            .keys()
            .map(|block| {
                let writes: Vec<Record> = ingest.writes(block).into_iter().filter(inside).collect();
                (block.clone(), writes)
            })
            .collect(),
    )
}

/// Diffs the listed blocks with the known diffs applied and returns the report of every block
/// with an unexplained divergence, printing one summary line per block.
pub fn diff_blocks(
    what: &str,
    blocks: &[&str],
    ours: &BTreeMap<String, Vec<Ours>>,
    oracle: &BTreeMap<String, Vec<Record>>,
) -> String {
    let known = known_diffs();
    let mut failures = String::new();
    for block in blocks {
        let empty_ours = Vec::new();
        let empty_oracle = Vec::new();
        let a = ours.get(*block).unwrap_or(&empty_ours);
        let b = oracle.get(*block).unwrap_or(&empty_oracle);
        let (diff, excused): (BlockDiff, _) = known.diff_block_counted(QEMU, block, a, b);
        println!(
            "{what}: block {block}: {} of our writes, {} oracle writes, {} aligned, {}",
            diff.ours_len,
            diff.oracle_len,
            diff.aligned,
            if diff.is_equal() { "clean" } else { "DIVERGES" }
        );
        for entry in excused {
            println!(
                "{what}: block {block}: `{}` excused {} divergences, rewrote {} of our values",
                entry.id, entry.excused, entry.rewritten
            );
        }
        if !diff.is_equal() {
            failures.push_str(&render(&diff));
        }
    }
    failures
}
