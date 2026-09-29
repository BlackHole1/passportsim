//! MMIO coverage histogram and the coverage gate: every `(block, offset)` a corpus image touches
//! under an oracle or our own trace needs a `RegSpec` of class other than U or an allowlist entry
//! with a reason, and a new touch fails CI.
//!
//! Producing a histogram needs an oracle (macOS-only), but the gate must run everywhere, so
//! [`Hist`] is the data, committed in a byte-stable text form (sorted, fixed-width hex offsets),
//! and [`new_touches`] is the gate, comparing an observed histogram with the committed one and an
//! allowlist, with no oracle at all.

use std::collections::{BTreeMap, BTreeSet};

use crate::qemu_ingest::{Ingest, Kind};
use crate::spec_toml::{self, Error as SpecError};

/// First line of a histogram file, so a stale format is caught on read.
pub const MAGIC: &str = "# pemu-verify oracle hist v1";

/// Read and write counts of one `(block, offset)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub reads: u64,
    pub writes: u64,
}

impl Counts {
    pub fn add(&mut self, kind: Kind) {
        match kind {
            Kind::Read => self.reads += 1,
            Kind::Write => self.writes += 1,
        }
    }
}

/// A coverage histogram.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hist {
    /// Label of the run that produced it: image, oracle and configuration.
    pub label: String,
    pub entries: BTreeMap<(String, u32), Counts>,
}

impl Hist {
    pub fn new(label: impl Into<String>) -> Hist {
        Hist {
            label: label.into(),
            entries: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, block: &str, offset: u32, kind: Kind) {
        self.entries
            .entry((block.to_string(), offset))
            .or_default()
            .add(kind);
    }

    /// Builds a histogram from an ingested QEMU trace.
    pub fn from_ingest(label: impl Into<String>, ingest: &Ingest) -> Hist {
        let mut hist = Hist::new(label);
        for (block, records) in &ingest.streams {
            for record in records {
                hist.record(block, record.offset, record.kind);
            }
        }
        hist
    }

    /// The `(block, offset)` pairs touched, in order.
    pub fn touches(&self) -> impl Iterator<Item = (&str, u32)> {
        self.entries
            .keys()
            .map(|(block, offset)| (block.as_str(), *offset))
    }

    pub fn blocks(&self) -> BTreeSet<&str> {
        self.entries
            .keys()
            .map(|(block, _)| block.as_str())
            .collect()
    }

    /// The committed text form: a magic line, a label line, then one line per touch.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(MAGIC);
        out.push('\n');
        out.push_str(&format!("# run {}\n", self.label));
        out.push_str(&format!("# touches {}\n", self.entries.len()));
        for ((block, offset), counts) in &self.entries {
            out.push_str(&format!(
                "{block} {offset:#07x} R {} W {}\n",
                counts.reads, counts.writes
            ));
        }
        out
    }

    pub fn parse(text: &str) -> Result<Hist, SpecError> {
        let text = text.replace('\r', "");
        let mut lines = text.split('\n').enumerate();
        let (_, first) = lines
            .next()
            .ok_or_else(|| SpecError::new(1, "empty file"))?;
        if first != MAGIC {
            return Err(SpecError::new(1, format!("expected `{MAGIC}`")));
        }
        let mut hist = Hist::new("");
        for (index, line) in lines {
            let at = index + 1;
            if let Some(label) = line.strip_prefix("# run ") {
                hist.label = label.to_string();
                continue;
            }
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let mut fields = line.split_whitespace();
            let mut next = |what: &str| -> Result<String, SpecError> {
                fields
                    .next()
                    .map(str::to_string)
                    .ok_or_else(|| SpecError::new(at, format!("missing {what}")))
            };
            let block = next("block")?;
            let offset = parse_hex(&next("offset")?, at)?;
            let (mut reads, mut writes) = (0, 0);
            for _ in 0..2 {
                let letter = next("`R` or `W`")?;
                let count = next("count")?
                    .parse::<u64>()
                    .map_err(|_| SpecError::new(at, "count is not a number"))?;
                match letter.as_str() {
                    "R" => reads = count,
                    "W" => writes = count,
                    other => return Err(SpecError::new(at, format!("unknown field `{other}`"))),
                }
            }
            if hist
                .entries
                .insert((block.clone(), offset), Counts { reads, writes })
                .is_some()
            {
                return Err(SpecError::new(at, format!("duplicate touch in `{block}`")));
            }
        }
        Ok(hist)
    }
}

/// A `0x…` offset.
fn parse_hex(text: &str, at: usize) -> Result<u32, SpecError> {
    let body = text
        .strip_prefix("0x")
        .ok_or_else(|| SpecError::new(at, "an offset is written `0x…`"))?;
    u32::from_str_radix(body, 16).map_err(|_| SpecError::new(at, "offset is not hexadecimal"))
}

/// One allowlist entry: a touch accepted without a `RegSpec`, with its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allow {
    pub block: String,
    pub offset: u32,
    /// Exclusive end of the offset range; the whole block when the entry names no offset.
    pub offset_end: u32,
    pub reason: String,
}

impl Allow {
    pub fn covers(&self, block: &str, offset: u32) -> bool {
        self.block == block && (self.offset..self.offset_end).contains(&offset)
    }

    /// Reads the `[[allow]]` array of a spec file (`specs/oracle-known-diffs.toml` carries it).
    /// An entry without a reason is refused: an unexplained allowance is indistinguishable from
    /// an untested register.
    pub fn parse_list(text: &str) -> Result<Vec<Allow>, SpecError> {
        let doc = spec_toml::parse(text)?;
        let mut out = Vec::new();
        for table in doc.array("allow") {
            let at = table.line;
            let reason = table.str_field("reason")?.trim().to_string();
            if reason.is_empty() {
                return Err(SpecError::new(at, "an allowlist entry needs a reason"));
            }
            let has_offset = table.pairs.contains_key("offset");
            let offset = if has_offset {
                u32::try_from(table.u64_field("offset")?)
                    .map_err(|_| SpecError::new(at, "`offset` is not a 32-bit value"))?
            } else {
                0
            };
            let offset_end = match table.pairs.contains_key("offset_end") {
                true => u32::try_from(table.u64_field("offset_end")?)
                    .map_err(|_| SpecError::new(at, "`offset_end` is not a 32-bit value"))?,
                false if has_offset => offset + 1,
                false => u32::MAX,
            };
            if offset_end <= offset {
                return Err(SpecError::new(at, "empty offset range"));
            }
            out.push(Allow {
                block: table.str_field("block")?.to_string(),
                offset,
                offset_end,
                reason,
            });
        }
        Ok(out)
    }
}

/// A touch the committed baseline does not carry and no allowlist entry covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTouch {
    pub block: String,
    pub offset: u32,
    pub counts: Counts,
}

impl core::fmt::Display for NewTouch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} {:#07x} R {} W {}",
            self.block, self.offset, self.counts.reads, self.counts.writes
        )
    }
}

/// The coverage gate: touches in `observed` the baseline and the allowlist do not cover. Empty
/// passes. Counts are not compared, and a *vanished* touch is not a failure but is reported by
/// [`vanished_touches`], so a regeneration can drop it deliberately.
pub fn new_touches(baseline: &Hist, observed: &Hist, allow: &[Allow]) -> Vec<NewTouch> {
    observed
        .entries
        .iter()
        .filter(|((block, offset), _)| {
            !baseline.entries.contains_key(&(block.clone(), *offset))
                && !allow.iter().any(|entry| entry.covers(block, *offset))
        })
        .map(|((block, offset), counts)| NewTouch {
            block: block.clone(),
            offset: *offset,
            counts: *counts,
        })
        .collect()
}

/// Touches the baseline carries and the observed run no longer makes. Informational.
pub fn vanished_touches(baseline: &Hist, observed: &Hist) -> Vec<(String, u32)> {
    baseline
        .entries
        .keys()
        .filter(|(block, offset)| !observed.entries.contains_key(&(block.clone(), *offset)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu_ingest::RegionMap;

    /// A committed QEMU trace excerpt and the histogram it must produce, both ours, written over
    /// the register offsets of the `c3_devices!` blocks.
    const SAMPLE_TRACE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/oracle/fixtures/hist-sample.trace"
    ));
    const SAMPLE_HIST: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/oracle/fixtures/hist-sample.hist"
    ));
    const MAP_TEXT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/oracle-qemu-regions.toml"
    ));
    const KNOWN_DIFFS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/oracle-known-diffs.toml"
    ));

    fn sample() -> Hist {
        let map = RegionMap::parse(MAP_TEXT).expect("map parses");
        let ingest = crate::qemu_ingest::ingest(SAMPLE_TRACE, &map);
        assert!(ingest.malformed.is_empty(), "{:?}", ingest.malformed);
        assert!(ingest.unmapped.is_empty(), "{:?}", ingest.unmapped);
        Hist::from_ingest("hist-sample (synthetic trace)", &ingest)
    }

    #[test]
    fn the_committed_sample_regenerates_byte_for_byte() {
        assert_eq!(
            sample().to_text(),
            SAMPLE_HIST,
            "tools/oracle/fixtures/hist-sample.hist is stale; regenerate it with \
             `cargo xtask oracle hist --regen`"
        );
    }

    #[test]
    fn the_text_form_round_trips() {
        let hist = sample();
        let parsed = Hist::parse(&hist.to_text()).expect("parses");
        assert_eq!(parsed, hist);
        assert_eq!(parsed.label, "hist-sample (synthetic trace)");
    }

    #[test]
    fn the_gate_passes_a_run_that_touches_nothing_new() {
        let baseline = Hist::parse(SAMPLE_HIST).expect("the committed baseline parses");
        let mut observed = sample();
        // More traffic at a register already covered is the same coverage.
        observed.record("sha", 0x80, Kind::Write);
        assert!(new_touches(&baseline, &observed, &[]).is_empty());
        assert!(vanished_touches(&baseline, &observed).is_empty());
    }

    #[test]
    fn the_gate_fails_a_new_touch() {
        let baseline = Hist::parse(SAMPLE_HIST).expect("parses");
        let mut observed = sample();
        observed.record("i2s0", 0x24, Kind::Write);
        let found = new_touches(&baseline, &observed, &[]);
        assert_eq!(
            found,
            vec![NewTouch {
                block: "i2s0".to_string(),
                offset: 0x24,
                counts: Counts {
                    reads: 0,
                    writes: 1
                },
            }]
        );
        assert_eq!(found[0].to_string(), "i2s0 0x00024 R 0 W 1");
    }

    #[test]
    fn an_allowlist_entry_covers_a_new_touch() {
        let baseline = Hist::parse(SAMPLE_HIST).expect("parses");
        let mut observed = sample();
        observed.record("i2s0", 0x24, Kind::Write);
        let allow = [Allow {
            block: "i2s0".to_string(),
            offset: 0,
            offset_end: u32::MAX,
            reason: "I2S is unmodeled".to_string(),
        }];
        assert!(new_touches(&baseline, &observed, &allow).is_empty());
        // The entry is scoped: another block is still a failure.
        observed.record("twai", 0x00, Kind::Read);
        assert_eq!(new_touches(&baseline, &observed, &allow).len(), 1);
    }

    #[test]
    fn a_touch_that_disappeared_is_reported_but_does_not_fail_the_gate() {
        let baseline = Hist::parse(SAMPLE_HIST).expect("parses");
        let observed = Hist::new("empty");
        assert!(new_touches(&baseline, &observed, &[]).is_empty());
        assert_eq!(
            vanished_touches(&baseline, &observed).len(),
            baseline.entries.len()
        );
    }

    #[test]
    fn the_allowlist_of_the_committed_spec_file_parses() {
        let allow = Allow::parse_list(KNOWN_DIFFS).expect("the committed allowlist parses");
        for entry in &allow {
            assert!(!entry.reason.trim().is_empty());
            assert!(entry.offset < entry.offset_end);
            // An unbounded entry would accept every future touch in a 4 KB window, so the
            // committed file carries none.
            assert_ne!(
                entry.offset_end,
                u32::MAX,
                "allowlist entry for `{}` covers the whole block window",
                entry.block
            );
        }
        // The two blocks QEMU leaves unmodeled, bounded to their register files.
        let bounds: Vec<(&str, u32, u32)> = allow
            .iter()
            .map(|entry| (entry.block.as_str(), entry.offset, entry.offset_end))
            .collect();
        assert_eq!(
            bounds,
            vec![("spi2", 0x000, 0x0F4), ("assist_debug", 0x000, 0x200)]
        );
    }

    #[test]
    fn an_allowlist_entry_without_a_reason_is_refused() {
        let text = "schema = 1\n[[allow]]\nblock = \"i2s0\"\nreason = \"\"\n";
        assert!(Allow::parse_list(text).is_err());
    }

    #[test]
    fn a_histogram_without_the_magic_line_is_refused() {
        assert!(Hist::parse("sha 0x00080 R 0 W 1\n").is_err());
        assert!(Hist::parse(&format!("{MAGIC}\nsha 0x80\n")).is_err());
        assert!(Hist::parse(&format!("{MAGIC}\nsha 128 R 0 W 1\n")).is_err());
    }
}
