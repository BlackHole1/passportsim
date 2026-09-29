//! QEMU trace ingest and the region map. The oracle runs as a black box with `-icount shift=0,
//! align=off,sleep=off` and `-d trace:memory_region_ops_*`, one line per access:
//!
//! ```text
//! memory_region_ops_read cpu 0 mr 0x… addr 0x… value 0x… size 4 name 'esp32c3.iomem'
//! ```
//!
//! No PC and no instruction count, so the ingest yields ordered per-block streams without time.
//! A region name can cover one block, several, or (the catch-all) every block QEMU does not
//! model, so each access is resolved by **address** through `specs/oracle-qemu-regions.toml`; the
//! region name is then checked against the result and a mismatch is reported in
//! [`Ingest::region_block_mismatch`] rather than mapped away.
//!
//! Parsing is tolerant of what surrounds a record (a log prefix) and strict about the record: an
//! unparsable `memory_region_ops` line is counted, never skipped silently.

use std::collections::{BTreeMap, BTreeSet};

use crate::spec_toml::{self, Error as SpecError};

/// Whether an access read or wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Read,
    Write,
}

impl Kind {
    /// The letter used in the histogram and diff reports.
    pub fn letter(self) -> char {
        match self {
            Kind::Read => 'R',
            Kind::Write => 'W',
        }
    }
}

/// One traced access, as the line carried it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub kind: Kind,
    /// Absolute guest address.
    pub addr: u32,
    pub value: u64,
    pub size: u8,
    /// QEMU region name, without its quotes.
    pub region: String,
}

/// One record of a per-block stream: the access with its position in the whole trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Index of the access in the trace, 0-based, for the QEMU record index of a divergence
    /// report.
    pub index: usize,
    pub kind: Kind,
    /// Offset inside the block.
    pub offset: u32,
    pub size: u8,
    pub value: u64,
}

impl Record {
    /// The alignment key of a write-stream diff: `(offset, size, value)`.
    pub fn key(&self) -> (u32, u8, u64) {
        (self.offset, self.size, self.value)
    }
}

/// One block of the map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Block name, as the `c3_devices!` table spells it.
    pub name: String,
    pub base: u32,
    pub size: u32,
    /// True where QEMU's write stream is trusted for this block.
    pub trusted: bool,
}

/// One QEMU region name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub name: String,
    /// Human description of what it covers.
    pub covers: String,
    /// Block names an access through this region is expected to resolve to, in file order.
    /// Empty for the catch-all, which reaches every block QEMU does not model.
    pub blocks: Vec<String>,
    /// True for the region that covers every unmodeled block.
    pub catch_all: bool,
}

impl Region {
    /// Whether an access through this region may resolve to `block`. The catch-all, and a region
    /// naming no block, accept anything: the check is evidence, not a second address table.
    pub fn accepts(&self, block: &str) -> bool {
        self.catch_all || self.blocks.is_empty() || self.blocks.iter().any(|name| name == block)
    }
}

/// The parsed `specs/oracle-qemu-regions.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionMap {
    pub blocks: Vec<Block>,
    pub regions: BTreeMap<String, Region>,
}

impl RegionMap {
    /// Parses the region map and checks that no two block windows overlap.
    pub fn parse(text: &str) -> Result<RegionMap, SpecError> {
        let doc = spec_toml::parse(text)?;
        let mut blocks = Vec::new();
        for table in doc.array("block") {
            let base = u32::try_from(table.u64_field("base")?)
                .map_err(|_| SpecError::new(table.line, "`base` is not a 32-bit address"))?;
            let size = u32::try_from(table.u64_field("size")?)
                .map_err(|_| SpecError::new(table.line, "`size` is not a 32-bit length"))?;
            if size == 0 || base.checked_add(size).is_none() {
                return Err(SpecError::new(table.line, "empty or wrapping block window"));
            }
            blocks.push(Block {
                name: table.str_field("name")?.to_string(),
                base,
                size,
                trusted: table.bool_field("trusted", false)?,
            });
        }
        if blocks.is_empty() {
            return Err(SpecError::new(1, "the region map has no `[[block]]` row"));
        }
        blocks.sort_by_key(|block| block.base);
        for pair in blocks.windows(2) {
            if pair[0].base + pair[0].size > pair[1].base {
                return Err(SpecError::new(
                    1,
                    format!("blocks `{}` and `{}` overlap", pair[0].name, pair[1].name),
                ));
            }
        }
        let mut names = BTreeSet::new();
        for block in &blocks {
            if !names.insert(block.name.clone()) {
                return Err(SpecError::new(
                    1,
                    format!("duplicate block `{}`", block.name),
                ));
            }
        }
        let mut regions = BTreeMap::new();
        for table in doc.array("region") {
            let mut listed = Vec::new();
            if let Some(value) = table.pairs.get("blocks") {
                let array = value.as_array().ok_or_else(|| {
                    SpecError::new(table.line, "`blocks` is a list of block names")
                })?;
                for entry in array {
                    let name = entry.as_str().ok_or_else(|| {
                        SpecError::new(table.line, "`blocks` holds block names as strings")
                    })?;
                    if !names.contains(name) {
                        return Err(SpecError::new(
                            table.line,
                            format!("`blocks` names `{name}`, which is not a [[block]] row"),
                        ));
                    }
                    listed.push(name.to_string());
                }
            }
            let region = Region {
                name: table.str_field("name")?.to_string(),
                covers: table.str_field("covers")?.to_string(),
                blocks: listed,
                catch_all: table.bool_field("catch_all", false)?,
            };
            if regions.insert(region.name.clone(), region).is_some() {
                return Err(SpecError::new(table.line, "duplicate region name"));
            }
        }
        Ok(RegionMap { blocks, regions })
    }

    pub fn resolve(&self, addr: u32) -> Option<(&Block, u32)> {
        let at = self.blocks.partition_point(|block| block.base <= addr);
        let block = self.blocks.get(at.checked_sub(1)?)?;
        let offset = addr - block.base;
        (offset < block.size).then_some((block, offset))
    }

    pub fn block(&self, name: &str) -> Option<&Block> {
        self.blocks.iter().find(|block| block.name == name)
    }
}

/// Everything one trace file yielded.
#[derive(Clone, Debug, Default)]
pub struct Ingest {
    /// Per-block streams, in trace order.
    pub streams: BTreeMap<String, Vec<Record>>,
    /// Accesses whose address is in no block window, with their trace index.
    pub unmapped: Vec<(usize, Access)>,
    /// Region names the map does not list.
    pub unknown_regions: BTreeSet<String>,
    /// Accesses whose listed region resolved to a block it does not cover, as `(region, block)`.
    /// A QEMU build that re-split a region shows up here.
    pub region_block_mismatch: BTreeSet<(String, String)>,
    /// `memory_region_ops` lines that did not parse, by line number.
    pub malformed: Vec<usize>,
    pub accesses: usize,
}

impl Ingest {
    pub fn stream(&self, block: &str) -> &[Record] {
        self.streams.get(block).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The write records of one block, the alignment input of a diff.
    pub fn writes(&self, block: &str) -> Vec<Record> {
        self.stream(block)
            .iter()
            .copied()
            .filter(|record| record.kind == Kind::Write)
            .collect()
    }

    /// The read records of one block, to be compared only for `stable_read` registers. That
    /// comparison is not implemented: the flag reaches code only as a generated `RegSpec` column,
    /// and `pemu-verify` may depend on no core crate. This is its ingest half.
    pub fn reads(&self, block: &str) -> Vec<Record> {
        self.stream(block)
            .iter()
            .copied()
            .filter(|record| record.kind == Kind::Read)
            .collect()
    }
}

/// Ingests a QEMU trace, resolving every access through the map.
pub fn ingest(text: &str, map: &RegionMap) -> Ingest {
    let mut out = Ingest::default();
    for (index, line) in text.replace('\r', "").split('\n').enumerate() {
        if !line.contains("memory_region_ops_") {
            continue;
        }
        let Some(access) = parse_line(line) else {
            out.malformed.push(index + 1);
            continue;
        };
        let at = out.accesses;
        out.accesses += 1;
        let region = map.regions.get(&access.region);
        if region.is_none() {
            out.unknown_regions.insert(access.region.clone());
        }
        match map.resolve(access.addr) {
            Some((block, offset)) => {
                // The address decides the block; the region name is evidence that the QEMU build
                // still splits regions as the map says. A disagreement is reported.
                if let Some(region) = region
                    && !region.accepts(&block.name)
                {
                    out.region_block_mismatch
                        .insert((region.name.clone(), block.name.clone()));
                }
                out.streams
                    .entry(block.name.clone())
                    .or_default()
                    .push(Record {
                        index: at,
                        kind: access.kind,
                        offset,
                        size: access.size,
                        value: access.value,
                    })
            }
            None => out.unmapped.push((at, access)),
        }
    }
    out
}

/// Parses one trace line, or `None` when it is not a well-formed access record.
pub fn parse_line(line: &str) -> Option<Access> {
    let at = line.find("memory_region_ops_")?;
    let rest = &line[at + "memory_region_ops_".len()..];
    let (verb, rest) = rest.split_once(' ')?;
    let kind = match verb {
        "read" => Kind::Read,
        "write" => Kind::Write,
        _ => return None,
    };
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    let mut tokens = rest.split_whitespace();
    let mut name = None;
    while let Some(key) = tokens.next() {
        if key == "name" {
            name = Some(quoted(&rest[rest.find("name ")? + 5..])?);
            break;
        }
        let value = tokens.next()?;
        fields.insert(key, value);
    }
    Some(Access {
        kind,
        addr: number(fields.get("addr")?)? as u32,
        value: number(fields.get("value")?)?,
        size: u8::try_from(number(fields.get("size")?)?).ok()?,
        region: name?.to_string(),
    })
}

/// The content of a `'…'` or `"…"` quoted token.
fn quoted(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let quote = text.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let body = &text[1..];
    let end = body.find(quote)?;
    Some(&body[..end])
}

/// A `0x…` hexadecimal or decimal number.
fn number(text: &str) -> Option<u64> {
    let text = text.trim_end_matches(',');
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse::<u64>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed map, so the tests check the file the oracle actually uses.
    const MAP_TEXT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/oracle-qemu-regions.toml"
    ));

    fn map() -> RegionMap {
        RegionMap::parse(MAP_TEXT).expect("the committed region map parses")
    }

    #[test]
    fn the_committed_map_covers_the_regions_the_spike_saw() {
        let map = map();
        // The region names of our own r5 run.
        for name in [
            "esp_soc.uart",
            "esp.systimer",
            "esp32.gpio",
            "esp32c3.cache",
            "esp32c3.gdma",
            "esp32c3.iomem",
            "esp32c3.soc.clk",
            "misc.esp.sha",
            "misc.esp32c3.intmatrix",
            "misc.esp32c3.rtc_cntl",
            "misc.esp32c3.usb_serial_jtag",
            "nvram.esp.efuse",
            "ssi.esp32c3.spi",
            "timer.esp.timg",
        ] {
            assert!(map.regions.contains_key(name), "{name} is not in the map");
        }
        assert!(map.regions["esp32c3.iomem"].catch_all);
    }

    #[test]
    fn addresses_resolve_to_the_c3_devices_blocks() {
        let map = map();
        // The pages our catch-all histogram reached and the blocks the region histogram names,
        // against the `c3_devices!` bases.
        for (addr, block, offset) in [
            (0x6000_0000u32, "uart0", 0u32),
            (0x6000_8000, "rtc_cntl", 0),
            (0x6000_87FC, "rtc_cntl", 0x7FC),
            (0x6000_8800, "efuse", 0),
            (0x6001_3000, "i2c0", 0),
            (0x6001_CC00, "radio_nrx", 0),
            (0x6002_403C, "spi2", 0x3C),
            (0x6003_B018, "sha", 0x18),
            (0x600C_4000, "extmem", 0),
            (0x600C_E000, "assist_debug", 0),
        ] {
            let (found, at) = map.resolve(addr).expect("address is mapped");
            assert_eq!((found.name.as_str(), at), (block, offset), "at {addr:#x}");
        }
        // The gap between the NRX window start and the page it sits in is in no block.
        assert!(map.resolve(0x6001_C000).is_none());
        assert!(map.resolve(0x6000_1000).is_none());
    }

    #[test]
    fn the_trusted_flag_follows_the_arch_oracle_table() {
        let map = map();
        for name in [
            "sha", "timg0", "timg1", "systimer", "intc", "usj", "efuse", "spi1",
        ] {
            assert!(map.block(name).expect(name).trusted, "{name}");
        }
        for name in ["gdma", "regi2c", "spi2", "i2c0", "saradc", "i2s0"] {
            assert!(!map.block(name).expect(name).trusted, "{name}");
        }
    }

    #[test]
    fn parses_a_trace_line_of_each_kind() {
        let read = parse_line(
            "memory_region_ops_read cpu 0 mr 0x600000012345 addr 0x18 value 0x0 size 4 name 'misc.esp.sha'",
        )
        .expect("read parses");
        assert_eq!(read.kind, Kind::Read);
        assert_eq!(read.addr, 0x18);
        assert_eq!(read.size, 4);
        assert_eq!(read.region, "misc.esp.sha");

        let write = parse_line(
            "12345@0 memory_region_ops_write cpu 0 mr 0x1 addr 0x6003b080 value 0xdeadbeef size 4 name 'misc.esp.sha'",
        )
        .expect("a prefixed write parses");
        assert_eq!(write.kind, Kind::Write);
        assert_eq!(write.addr, 0x6003_B080);
        assert_eq!(write.value, 0xdead_beef);
    }

    #[test]
    fn refuses_a_line_that_is_not_a_record() {
        assert!(parse_line("memory_region_ops_read cpu 0 mr 0x1 addr 0x18").is_none());
        assert!(parse_line("memory_region_ops_flush cpu 0 name 'x'").is_none());
        assert!(parse_line("qemu: some other trace line").is_none());
    }

    #[test]
    fn ingest_builds_ordered_per_block_streams() {
        let map = map();
        let trace = concat!(
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6003b080 value 0x1 size 4 name 'misc.esp.sha'\n",
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6001f000 value 0x2 size 4 name 'timer.esp.timg'\n",
            "memory_region_ops_read cpu 0 mr 0x1 addr 0x6003b018 value 0x0 size 4 name 'misc.esp.sha'\n",
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6003b084 value 0x3 size 4 name 'misc.esp.sha'\n",
            "not a trace line\n",
            "memory_region_ops_read cpu 0 mr 0x1 addr 0x6002403c value 0x0 size 4 name 'esp32c3.iomem'\n",
        );
        let ingest = ingest(trace, &map);
        assert_eq!(ingest.accesses, 5);
        assert!(ingest.malformed.is_empty());
        assert!(ingest.unknown_regions.is_empty());
        assert!(ingest.unmapped.is_empty());

        let sha = ingest.stream("sha");
        assert_eq!(sha.len(), 3);
        assert_eq!(sha[0].index, 0);
        assert_eq!(sha[1].index, 2);
        assert_eq!(ingest.writes("sha").len(), 2);
        assert_eq!(ingest.writes("sha")[1].key(), (0x84, 4, 3));
        assert_eq!(ingest.reads("sha")[0].offset, 0x18);
        // The catch-all region resolves by address alone.
        assert_eq!(ingest.stream("spi2").len(), 1);
        assert_eq!(ingest.stream("timg0").len(), 1);
    }

    #[test]
    fn ingest_reports_a_region_that_reached_a_block_it_does_not_cover() {
        // A QEMU build that re-splits a region so a `misc.esp.sha` access lands at a TIMG address:
        // the address still decides the block, and the mismatch is reported.
        let map = map();
        let trace = concat!(
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6001f000 value 0x2 size 4 name 'misc.esp.sha'\n",
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6003b080 value 0x1 size 4 name 'misc.esp.sha'\n",
            "memory_region_ops_read cpu 0 mr 0x1 addr 0x6002403c value 0x0 size 4 name 'esp32c3.iomem'\n",
        );
        let ingest = ingest(trace, &map);
        assert_eq!(
            ingest.region_block_mismatch.iter().collect::<Vec<_>>(),
            vec![&("misc.esp.sha".to_string(), "timg0".to_string())],
            "only the re-split access is reported"
        );
        // The record is still placed by address, so the diff sees it where it belongs.
        assert_eq!(ingest.stream("timg0").len(), 1);
        assert_eq!(ingest.stream("sha").len(), 1);
    }

    #[test]
    fn every_region_of_the_committed_map_names_the_blocks_it_covers() {
        let map = map();
        for (name, region) in &map.regions {
            assert_eq!(
                region.blocks.is_empty(),
                region.catch_all,
                "{name}: only the catch-all region names no block"
            );
            for block in &region.blocks {
                assert!(map.block(block).is_some(), "{name} names `{block}`");
            }
        }
        assert!(map.regions["timer.esp.timg"].accepts("timg1"));
        assert!(!map.regions["timer.esp.timg"].accepts("sha"));
        assert!(map.regions["esp32c3.iomem"].accepts("spi2"));
    }

    #[test]
    fn a_region_naming_a_block_the_map_does_not_have_is_refused() {
        let text = concat!(
            "schema = 1\n",
            "[[block]]\nname = \"a\"\nbase = 0x6000_0000\nsize = 0x1000\n",
            "[[region]]\nname = \"r\"\ncovers = \"A\"\nblocks = [\"b\"]\n",
        );
        let err = RegionMap::parse(text).expect_err("an unknown block name is refused");
        assert!(err.detail.contains("not a [[block]] row"), "{err}");
    }

    #[test]
    fn ingest_reports_what_it_could_not_place() {
        let map = map();
        let trace = concat!(
            "memory_region_ops_read cpu 0 mr 0x1 addr 0x50000000 value 0x0 size 4 name 'esp32c3.rtcram'\n",
            "memory_region_ops_write cpu 0 mr 0x1 addr 0x6003b080 size 4 name 'misc.esp.sha'\n",
        );
        let ingest = ingest(trace, &map);
        assert_eq!(ingest.malformed, vec![2]);
        assert_eq!(ingest.unmapped.len(), 1);
        assert_eq!(ingest.unmapped[0].1.addr, 0x5000_0000);
        assert!(ingest.unknown_regions.contains("esp32c3.rtcram"));
    }

    #[test]
    fn an_overlapping_map_is_refused() {
        let text = concat!(
            "schema = 1\n",
            "[[block]]\nname = \"a\"\nbase = 0x6000_0000\nsize = 0x2000\n",
            "[[block]]\nname = \"b\"\nbase = 0x6000_1000\nsize = 0x1000\n",
        );
        assert!(RegionMap::parse(text).is_err());
    }
}
