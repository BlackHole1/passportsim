//! The known-diffs file and its loader. `specs/oracle-known-diffs.toml` lists the intended
//! differences between our run and an oracle; everything else fails. Two invariants keep it
//! evidence rather than a mask: every entry carries a reason ([`KnownDiffs::parse`] refuses an
//! empty one), and every entry names the oracle it excuses, or writes out `any`.
//!
//! An mmio entry matches a block and optionally an offset range and access direction, so a diff at
//! another register of the same block still fails. Its optional [`Constraints`] narrow it:
//! `xor_mask`, `values` and `readback` rewrite values before alignment, `extra_ours` and
//! `extra_oracle` bound the extra writes it excuses per block diff, and `pcs` limits both to
//! writes from those PCs. With no constraint an entry excuses any divergence at its registers;
//! with any, only what its `extra_*` fields allow.
//!
//! Call entries (`kind = "call"`) name one watched `function` and at least one `extra_*`. Nothing
//! else excuses a call-trace difference, so no entry can blank the call trace.

use crate::qemu_ingest::Kind;
use crate::spec_toml::{self, Error as SpecError};

/// Which accesses an mmio entry excuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Any,
}

impl Access {
    pub fn covers(self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Access::Any, _) | (Access::Read, Kind::Read) | (Access::Write, Kind::Write)
        )
    }

    fn parse(text: &str, line: usize) -> Result<Access, SpecError> {
        match text {
            "read" => Ok(Access::Read),
            "write" => Ok(Access::Write),
            "any" => Ok(Access::Any),
            other => Err(SpecError::new(line, format!("unknown access `{other}`"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Mmio {
        /// Block name of `specs/oracle-qemu-regions.toml`.
        block: String,
        offset: u32,
        offset_end: u32,
        access: Access,
    },
    Console {
        /// Substring of the normalized line the entry excuses.
        contains: String,
    },
    /// Entries into one function of a call trace.
    Call {
        /// The function's name as the trace records it.
        function: String,
    },
}

/// The optional constraints of an mmio entry (module documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Constraints {
    /// At most this many writes of ours the oracle does not have.
    pub extra_ours: Option<usize>,
    /// At most this many oracle writes we do not have.
    pub extra_oracle: Option<usize>,
    /// Bits ignored in the values written to the entry's registers, on both sides.
    pub xor_mask: Option<u64>,
    /// `(ours, oracle)`: our write of `ours` is compared as `oracle`.
    pub values: Vec<(u64, u64)>,
    /// An extra write of ours must come from one of these PCs, and a `xor_mask` or `values`
    /// rewrite applies only to our writes from them.
    pub pcs: Vec<u32>,
    /// Bits in which the two runs' read-backs of the register differed are ignored in the write
    /// that follows ([`KnownDiffs::apply_readback`]).
    pub readback: bool,
}

impl Constraints {
    fn is_empty(&self) -> bool {
        *self == Constraints::default()
    }

    /// Whether these constraints excuse `divergence` (seen after the value rewrites), given the
    /// extra writes of each side the entry has already excused in this block diff.
    pub fn accepts(
        &self,
        divergence: &crate::lcs::Divergence,
        used_ours: usize,
        used_oracle: usize,
    ) -> bool {
        use crate::lcs::Divergence;
        if self.is_empty() {
            return true;
        }
        match divergence {
            Divergence::OnlyOurs { record, .. } => {
                self.extra_ours.is_some_and(|n| used_ours < n)
                    && (self.pcs.is_empty() || self.pcs.contains(&record.pc))
            }
            Divergence::OnlyOracle { .. } => {
                self.pcs.is_empty() && self.extra_oracle.is_some_and(|n| used_oracle < n)
            }
            Divergence::Changed { .. } | Divergence::Unaligned { .. } => false,
        }
    }

    fn our_value(&self, value: u64) -> u64 {
        let mapped = self
            .values
            .iter()
            .find(|(ours, _)| *ours == value)
            .map_or(value, |(_, oracle)| *oracle);
        self.xor_mask.map_or(mapped, |mask| mapped & !mask)
    }

    fn oracle_value(&self, value: u64) -> u64 {
        self.xor_mask.map_or(value, |mask| value & !mask)
    }

    fn parse(table: &spec_toml::Table) -> Result<Constraints, SpecError> {
        use spec_toml::Value;
        let at = table.line;
        let int = |value: &Value, key: &str| -> Result<u64, SpecError> {
            match value.as_int() {
                Some(i) if i >= 0 => Ok(i as u64),
                _ => Err(SpecError::new(
                    at,
                    format!("`{key}` holds a value that is not a non-negative integer"),
                )),
            }
        };
        let count = |key: &str| -> Result<Option<usize>, SpecError> {
            match table.pairs.get(key) {
                None => Ok(None),
                Some(_) => Ok(Some(table.u64_field(key)? as usize)),
            }
        };
        let mut out = Constraints {
            extra_ours: count("extra_ours")?,
            extra_oracle: count("extra_oracle")?,
            xor_mask: match table.pairs.get("xor_mask") {
                None => None,
                Some(_) => Some(table.u64_field("xor_mask")?),
            },
            ..Constraints::default()
        };
        if out.xor_mask == Some(0) {
            return Err(SpecError::new(at, "`xor_mask = 0` excuses nothing"));
        }
        out.readback = table.bool_field("readback", false)?;
        if let Some(value) = table.pairs.get("values") {
            let items = value
                .as_array()
                .ok_or_else(|| SpecError::new(at, "`values` is not an array"))?;
            for item in items {
                match item.as_array() {
                    Some([ours, oracle]) => {
                        out.values
                            .push((int(ours, "values")?, int(oracle, "values")?));
                    }
                    _ => {
                        return Err(SpecError::new(
                            at,
                            "`values` items are `[ours, oracle]` pairs",
                        ));
                    }
                }
            }
        }
        if let Some(value) = table.pairs.get("pcs") {
            let items = value
                .as_array()
                .ok_or_else(|| SpecError::new(at, "`pcs` is not an array"))?;
            for item in items {
                out.pcs.push(
                    u32::try_from(int(item, "pcs")?)
                        .map_err(|_| SpecError::new(at, "`pcs` holds a value above 32 bits"))?,
                );
            }
        }
        Ok(out)
    }
}

/// What one entry did in one block diff ([`KnownDiffs::diff_block_counted`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryUse {
    pub id: String,
    /// Divergences it excused (`extra_ours`, `extra_oracle`, or an unconstrained entry).
    pub excused: usize,
    /// Our write values its `xor_mask` or `values` rewrite changed.
    pub rewritten: usize,
}

/// One listed difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownDiff {
    pub id: String,
    pub oracle: String,
    pub scope: Scope,
    /// Why the difference is intended, with its citation.
    pub reason: String,
    /// Milestone by which the entry is expected to disappear.
    pub milestone: Option<String>,
    /// Which divergences an mmio entry excuses; empty for a console entry.
    pub constraints: Constraints,
}

impl KnownDiff {
    pub fn applies_to(&self, oracle: &str) -> bool {
        self.oracle == "any" || self.oracle == oracle
    }
}

/// The parsed `specs/oracle-known-diffs.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownDiffs {
    pub entries: Vec<KnownDiff>,
}

impl KnownDiffs {
    /// Parses the file, refusing an entry without a reason or with a duplicate id.
    pub fn parse(text: &str) -> Result<KnownDiffs, SpecError> {
        let doc = spec_toml::parse(text)?;
        let mut entries: Vec<KnownDiff> = Vec::new();
        for table in doc.array("diff") {
            let at = table.line;
            let id = table.str_field("id")?.to_string();
            let reason = table.str_field("reason")?.trim().to_string();
            if reason.is_empty() {
                return Err(SpecError::new(
                    at,
                    format!("known diff `{id}` has an empty reason"),
                ));
            }
            let oracle = table.str_field("oracle")?.to_string();
            let scope = match table.str_field("kind")? {
                "mmio" => {
                    let offset = u32::try_from(
                        table
                            .pairs
                            .get("offset")
                            .map_or(Ok(0), |_| table.u64_field("offset"))?,
                    )
                    .map_err(|_| SpecError::new(at, "`offset` is not a 32-bit value"))?;
                    let offset_end = match table.pairs.get("offset_end") {
                        Some(_) => u32::try_from(table.u64_field("offset_end")?).map_err(|_| {
                            SpecError::new(at, "`offset_end` is not a 32-bit value")
                        })?,
                        None if table.pairs.contains_key("offset") => offset + 1,
                        None => u32::MAX,
                    };
                    if offset_end <= offset {
                        return Err(SpecError::new(at, "empty offset range"));
                    }
                    Scope::Mmio {
                        block: table.str_field("block")?.to_string(),
                        offset,
                        offset_end,
                        access: match table.opt_str("access")? {
                            Some(text) => Access::parse(text, at)?,
                            None => Access::Any,
                        },
                    }
                }
                "console" => Scope::Console {
                    contains: table.str_field("contains")?.to_string(),
                },
                "call" => Scope::Call {
                    function: table.str_field("function")?.to_string(),
                },
                other => return Err(SpecError::new(at, format!("unknown kind `{other}`"))),
            };
            let constraints = match scope {
                Scope::Mmio { .. } => Constraints::parse(table)?,
                Scope::Console { .. } => Constraints::default(),
                Scope::Call { .. } => {
                    let c = Constraints::parse(table)?;
                    if c.extra_ours.is_none() && c.extra_oracle.is_none() {
                        return Err(SpecError::new(
                            at,
                            format!(
                                "call entry `{id}` needs `extra_ours` or `extra_oracle`: nothing \
                                 else excuses a call-trace difference"
                            ),
                        ));
                    }
                    if c.xor_mask.is_some()
                        || !c.values.is_empty()
                        || !c.pcs.is_empty()
                        || c.readback
                    {
                        return Err(SpecError::new(
                            at,
                            format!("call entry `{id}` takes only `extra_ours` and `extra_oracle`"),
                        ));
                    }
                    c
                }
            };
            if entries.iter().any(|entry| entry.id == id) {
                return Err(SpecError::new(at, format!("duplicate known diff `{id}`")));
            }
            entries.push(KnownDiff {
                id,
                oracle,
                scope,
                reason,
                milestone: table.opt_str("milestone")?.map(str::to_string),
                constraints,
            });
        }
        Ok(KnownDiffs { entries })
    }

    pub fn suppresses_mmio(
        &self,
        oracle: &str,
        block: &str,
        offset: u32,
        kind: Kind,
    ) -> Option<&KnownDiff> {
        self.entries.iter().find(|entry| {
            entry.applies_to(oracle)
                && match &entry.scope {
                    Scope::Mmio {
                        block: name,
                        offset: from,
                        offset_end: to,
                        access,
                    } => name == block && (*from..*to).contains(&offset) && access.covers(kind),
                    Scope::Console { .. } | Scope::Call { .. } => false,
                }
        })
    }

    pub fn suppresses_console(&self, oracle: &str, line: &str) -> Option<&KnownDiff> {
        self.entries.iter().find(|entry| {
            entry.applies_to(oracle)
                && match &entry.scope {
                    Scope::Console { contains } => line.contains(contains.as_str()),
                    Scope::Mmio { .. } | Scope::Call { .. } => false,
                }
        })
    }

    /// The entry that excuses a block diff's divergence, if any. Whether a block passes is
    /// [`Self::diff_block`]'s question: it walks past a listed divergence.
    pub fn suppresses_divergence(
        &self,
        oracle: &str,
        diff: &crate::lcs::BlockDiff,
    ) -> Option<&KnownDiff> {
        self.suppresses_divergence_in(oracle, &diff.block, diff.first.as_ref()?)
    }

    /// Diffs one block's write streams with the suppressions applied. Use this, not
    /// [`crate::lcs::diff_block`] plus [`Self::suppresses_divergence`]: the walk continues past an
    /// excused divergence, so a difference at another register still fails.
    pub fn diff_block(
        &self,
        oracle: &str,
        block: &str,
        ours: &[crate::lcs::Ours],
        theirs: &[crate::qemu_ingest::Record],
    ) -> crate::lcs::BlockDiff {
        self.diff_block_counted(oracle, block, ours, theirs).0
    }

    /// [`Self::diff_block`], also returning what each entry excused and rewrote, so a caller can
    /// show every entry was spent on what it was written for.
    pub fn diff_block_counted(
        &self,
        oracle: &str,
        block: &str,
        ours: &[crate::lcs::Ours],
        theirs: &[crate::qemu_ingest::Record],
    ) -> (crate::lcs::BlockDiff, Vec<EntryUse>) {
        use crate::lcs::Divergence;
        // Per entry: extra ours excused, extra oracle excused, divergences excused, our values
        // rewritten.
        let mut used = vec![(0usize, 0usize, 0usize, 0usize); self.entries.len()];
        let covers = |entry: &KnownDiff, offset: u32| {
            entry.applies_to(oracle)
                && matches!(&entry.scope, Scope::Mmio { block: name, offset: from, offset_end: to, access }
                    if name == block && (*from..*to).contains(&offset) && access.covers(Kind::Write))
        };
        let rewriting: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let c = &self.entries[i].constraints;
                c.xor_mask.is_some() || !c.values.is_empty()
            })
            .collect();
        let mut seen_ours = ours.to_vec();
        for record in &mut seen_ours {
            let from_pc = |i: usize| {
                let pcs = &self.entries[i].constraints.pcs;
                pcs.is_empty() || pcs.contains(&record.pc)
            };
            if let Some(&i) = rewriting
                .iter()
                .find(|&&i| covers(&self.entries[i], record.offset) && from_pc(i))
            {
                let value = self.entries[i].constraints.our_value(record.value);
                if value != record.value {
                    used[i].3 += 1;
                }
                record.value = value;
            }
        }
        let mut seen_theirs = theirs.to_vec();
        for record in &mut seen_theirs {
            if record.kind != Kind::Write {
                continue;
            }
            if let Some(&i) = rewriting
                .iter()
                .find(|&&i| covers(&self.entries[i], record.offset))
            {
                record.value = self.entries[i].constraints.oracle_value(record.value);
            }
        }
        let mut diff =
            crate::lcs::diff_block_with(block, &seen_ours, &seen_theirs, &mut |divergence| {
                let Some(at) = self.matching(oracle, block, divergence, |i| (used[i].0, used[i].1))
                else {
                    return false;
                };
                match divergence {
                    Divergence::OnlyOurs { .. } => used[at].0 += 1,
                    Divergence::OnlyOracle { .. } => used[at].1 += 1,
                    _ => {}
                }
                used[at].2 += 1;
                true
            });
        // A report shows what each side really wrote, not the rewritten values.
        let raw_ours = |record: &mut crate::lcs::Ours| {
            if let Some(raw) = ours.get(record.index).filter(|r| r.offset == record.offset) {
                record.value = raw.value;
            }
        };
        let raw_theirs = |record: &mut crate::qemu_ingest::Record| {
            if let Some(raw) = theirs.iter().find(|r| r.index == record.index) {
                record.value = raw.value;
            }
        };
        diff.our_context.iter_mut().for_each(raw_ours);
        diff.oracle_context.iter_mut().for_each(raw_theirs);
        match diff.first.as_mut() {
            Some(Divergence::OnlyOurs { record, .. }) => raw_ours(record),
            Some(Divergence::OnlyOracle { record, .. }) => raw_theirs(record),
            Some(Divergence::Changed { ours, oracle }) => {
                raw_ours(ours);
                raw_theirs(oracle);
            }
            Some(Divergence::Unaligned { ours, oracle }) => {
                ours.iter_mut().for_each(raw_ours);
                oracle.iter_mut().for_each(raw_theirs);
            }
            None => {}
        }
        let counts = self
            .entries
            .iter()
            .zip(&used)
            .filter(|(_, u)| u.2 + u.3 > 0)
            .map(|(entry, u)| EntryUse {
                id: entry.id.clone(),
                excused: u.2,
                rewritten: u.3,
            })
            .collect();
        (diff, counts)
    }

    /// Applies the `readback` entries of `block` for `oracle` to both write streams before the
    /// diff, and returns what each rewrote.
    ///
    /// `our_reads[i]` is the last value our run read from the register of `ours[i]` since its
    /// previous write there; `their_reads` likewise. The k-th writes to an offset pair up only
    /// where both sides write it equally often. A pair is rewritten, ours to the oracle's, only
    /// when both followed a read and every differing bit is one the reads differed in.
    pub fn apply_readback(
        &self,
        oracle: &str,
        block: &str,
        ours: &mut [crate::lcs::Ours],
        our_reads: &[Option<u64>],
        theirs: &mut [crate::qemu_ingest::Record],
        their_reads: &[Option<u64>],
    ) -> Vec<EntryUse> {
        let mut uses = Vec::new();
        for entry in &self.entries {
            let Scope::Mmio {
                block: name,
                offset: from,
                offset_end: to,
                access,
            } = &entry.scope
            else {
                continue;
            };
            if !entry.constraints.readback
                || !entry.applies_to(oracle)
                || name != block
                || !access.covers(Kind::Write)
            {
                continue;
            }
            let mut by_offset: std::collections::BTreeMap<u32, (Vec<usize>, Vec<usize>)> =
                std::collections::BTreeMap::new();
            for (i, record) in ours.iter().enumerate() {
                if (*from..*to).contains(&record.offset) {
                    by_offset.entry(record.offset).or_default().0.push(i);
                }
            }
            for (i, record) in theirs.iter().enumerate() {
                if record.kind == Kind::Write && (*from..*to).contains(&record.offset) {
                    by_offset.entry(record.offset).or_default().1.push(i);
                }
            }
            let mut rewritten = 0;
            for (a, b) in by_offset.values() {
                if a.len() != b.len() {
                    continue;
                }
                for (&i, &j) in a.iter().zip(b) {
                    let (Some(Some(ra)), Some(Some(rb))) = (our_reads.get(i), their_reads.get(j))
                    else {
                        continue;
                    };
                    let (va, vb) = (ours[i].value, theirs[j].value);
                    if va != vb && (va ^ vb) & !(ra ^ rb) == 0 {
                        ours[i].value = vb;
                        rewritten += 1;
                    }
                }
            }
            if rewritten > 0 {
                uses.push(EntryUse {
                    id: entry.id.clone(),
                    excused: 0,
                    rewritten,
                });
            }
        }
        uses
    }

    /// Diffs two call traces with the `call` entries for `oracle` applied, returning what each
    /// entry excused.
    pub fn diff_calls(
        &self,
        oracle: &str,
        ours: &crate::calltrace::CallTrace,
        theirs: &crate::calltrace::CallTrace,
    ) -> (crate::calltrace::CallDiff, Vec<EntryUse>) {
        use crate::calltrace::Divergence;
        // Per entry: extra ours excused, extra oracle excused.
        let mut used = vec![(0usize, 0usize); self.entries.len()];
        let find = |used: &[(usize, usize)], function: &str, ours_side: bool| {
            self.entries.iter().enumerate().position(|(i, entry)| {
                entry.applies_to(oracle)
                    && matches!(&entry.scope, Scope::Call { function: f } if f == function)
                    && if ours_side {
                        entry.constraints.extra_ours.is_some_and(|n| used[i].0 < n)
                    } else {
                        entry
                            .constraints
                            .extra_oracle
                            .is_some_and(|n| used[i].1 < n)
                    }
            })
        };
        let diff = crate::calltrace::diff_with(ours, theirs, &mut |divergence| match divergence {
            Divergence::OnlyOurs(entry) => match find(&used, &entry.function, true) {
                Some(i) => {
                    used[i].0 += 1;
                    true
                }
                None => false,
            },
            Divergence::OnlyOracle(entry) => match find(&used, &entry.function, false) {
                Some(i) => {
                    used[i].1 += 1;
                    true
                }
                None => false,
            },
            Divergence::Reordered { ours, oracle } => {
                match (
                    find(&used, &ours.function, true),
                    find(&used, &oracle.function, false),
                ) {
                    (Some(a), Some(b)) => {
                        used[a].0 += 1;
                        used[b].1 += 1;
                        true
                    }
                    _ => false,
                }
            }
        });
        let counts = self
            .entries
            .iter()
            .zip(&used)
            .filter(|(_, u)| u.0 + u.1 > 0)
            .map(|(entry, u)| EntryUse {
                id: entry.id.clone(),
                excused: u.0 + u.1,
                rewritten: 0,
            })
            .collect();
        (diff, counts)
    }

    /// The entry that excuses one divergence of a named block, if any: matched by our record's
    /// offset, or the oracle's when only it has one, and accepted by the entry's
    /// [`Constraints`] with nothing yet excused.
    pub fn suppresses_divergence_in(
        &self,
        oracle: &str,
        block: &str,
        divergence: &crate::lcs::Divergence,
    ) -> Option<&KnownDiff> {
        self.matching(oracle, block, divergence, |_| (0, 0))
            .map(|at| &self.entries[at])
    }

    /// The first entry whose scope covers `divergence` and whose constraints accept it, given
    /// what each entry has already excused (`used(index)`).
    fn matching(
        &self,
        oracle: &str,
        block: &str,
        divergence: &crate::lcs::Divergence,
        used: impl Fn(usize) -> (usize, usize),
    ) -> Option<usize> {
        use crate::lcs::Divergence;
        let (offset, kind) = match divergence {
            Divergence::OnlyOurs { record, .. } => (record.offset, Kind::Write),
            Divergence::OnlyOracle { record, .. } => (record.offset, record.kind),
            Divergence::Changed { ours, .. } => (ours.offset, Kind::Write),
            Divergence::Unaligned { ours, oracle: them } => match (ours, them) {
                (Some(ours), _) => (ours.offset, Kind::Write),
                (None, Some(them)) => (them.offset, them.kind),
                (None, None) => return None,
            },
        };
        self.entries.iter().enumerate().position(|(i, entry)| {
            let (ours_used, oracle_used) = used(i);
            entry.applies_to(oracle)
                && match &entry.scope {
                    Scope::Mmio {
                        block: name,
                        offset: from,
                        offset_end: to,
                        access,
                    } => name == block && (*from..*to).contains(&offset) && access.covers(kind),
                    Scope::Console { .. } | Scope::Call { .. } => false,
                }
                && entry
                    .constraints
                    .accepts(divergence, ours_used, oracle_used)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lcs::{Divergence, Ours};
    use crate::qemu_ingest::Record;

    /// The committed file, so the tests check what the oracle runs actually read.
    const TEXT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/oracle-known-diffs.toml"
    ));

    fn committed() -> KnownDiffs {
        KnownDiffs::parse(TEXT).expect("the committed known-diffs file parses")
    }

    #[test]
    fn the_committed_file_lists_the_arch_examples_with_reasons() {
        let diffs = committed();
        assert!(!diffs.entries.is_empty());
        for entry in &diffs.entries {
            assert!(
                !entry.reason.trim().is_empty(),
                "{} has no reason",
                entry.id
            );
            assert!(
                entry.reason.len() > 40,
                "{} has a reason too short to be evidence",
                entry.id
            );
        }
        // The regi2c store model against QEMU's 0xFFFFFF.
        assert!(
            diffs
                .suppresses_mmio("qemu", "regi2c", 0x40, Kind::Read)
                .is_some()
        );
        // The boot and chip-revision lines are listed, not masked.
        assert_eq!(
            diffs
                .suppresses_console("qemu", "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa")
                .map(|entry| entry.id.as_str()),
            Some("console.boot-strap")
        );
        assert!(
            diffs
                .suppresses_console("qemu", "I (T) boot: chip revision: v1.1")
                .is_some()
        );
    }

    #[test]
    fn a_suppression_is_scoped_to_its_oracle_and_block() {
        let diffs = committed();
        assert!(
            diffs
                .suppresses_mmio("esp32sim", "regi2c", 0x40, Kind::Read)
                .is_none(),
            "an entry written for QEMU must not excuse another oracle"
        );
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x40, Kind::Write)
                .is_none(),
            "a block with no entry is never suppressed"
        );
        assert!(
            diffs
                .suppresses_mmio("qemu", "usj", 0, Kind::Write)
                .is_none(),
            "the USJ entry is scoped to the stock binary"
        );
        assert!(
            diffs
                .suppresses_mmio("qemu-stock", "usj", 0, Kind::Write)
                .is_some()
        );
    }

    #[test]
    fn an_offset_range_bounds_the_suppression() {
        let text = concat!(
            "schema = 1\n",
            "[[diff]]\n",
            "id = \"t.window\"\n",
            "kind = \"mmio\"\n",
            "oracle = \"any\"\n",
            "block = \"sha\"\n",
            "offset = 0x80\n",
            "offset_end = 0x90\n",
            "access = \"write\"\n",
            "reason = \"a test entry whose reason is long enough to be a real one\"\n",
        );
        let diffs = KnownDiffs::parse(text).expect("parses");
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x80, Kind::Write)
                .is_some()
        );
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x8f, Kind::Write)
                .is_some()
        );
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x90, Kind::Write)
                .is_none(),
            "the end is exclusive"
        );
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x80, Kind::Read)
                .is_none(),
            "a write entry does not excuse a read"
        );
    }

    #[test]
    fn a_lone_offset_covers_exactly_that_register() {
        let text = concat!(
            "schema = 1\n",
            "[[diff]]\nid = \"t.one\"\nkind = \"mmio\"\noracle = \"any\"\nblock = \"sha\"\n",
            "offset = 0x18\n",
            "reason = \"a test entry whose reason is long enough to be a real one\"\n",
        );
        let diffs = KnownDiffs::parse(text).expect("parses");
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x18, Kind::Read)
                .is_some()
        );
        assert!(
            diffs
                .suppresses_mmio("qemu", "sha", 0x1c, Kind::Read)
                .is_none()
        );
    }

    #[test]
    fn an_entry_without_a_reason_is_refused() {
        let text = concat!(
            "schema = 1\n",
            "[[diff]]\nid = \"t.x\"\nkind = \"mmio\"\noracle = \"any\"\nblock = \"sha\"\n",
            "reason = \"  \"\n",
        );
        let err = KnownDiffs::parse(text).expect_err("an empty reason is refused");
        assert!(err.detail.contains("empty reason"), "{err}");
    }

    #[test]
    fn a_duplicate_id_is_refused() {
        let entry = concat!(
            "[[diff]]\nid = \"t.x\"\nkind = \"console\"\noracle = \"any\"\ncontains = \"x\"\n",
            "reason = \"a test entry whose reason is long enough to be a real one\"\n",
        );
        let text = format!("schema = 1\n{entry}{entry}");
        assert!(KnownDiffs::parse(&text).is_err());
    }

    #[test]
    fn a_divergence_is_matched_by_the_offset_of_its_record() {
        let diffs = committed();
        let diff = crate::lcs::BlockDiff {
            block: "regi2c".to_string(),
            ours_len: 1,
            oracle_len: 1,
            aligned: 0,
            first: Some(Divergence::Changed {
                ours: Ours {
                    index: 0,
                    offset: 0x10,
                    size: 4,
                    value: 0x1234,
                    pc: 0x4038_0000,
                    symbol: None,
                },
                oracle: Record {
                    index: 0,
                    kind: Kind::Write,
                    offset: 0x10,
                    size: 4,
                    value: 0xff_ffff,
                },
            }),
            our_context: Vec::new(),
            oracle_context: Vec::new(),
        };
        assert_eq!(
            diffs
                .suppresses_divergence("qemu", &diff)
                .map(|entry| entry.id.as_str()),
            Some("regi2c.analog-store")
        );
        let mut elsewhere = diff.clone();
        elsewhere.block = "sha".to_string();
        assert!(diffs.suppresses_divergence("qemu", &elsewhere).is_none());
    }

    /// The entries a narrow-entry test needs: one write at `sha` 0x80 and nothing else.
    fn narrow() -> KnownDiffs {
        let text = concat!(
            "schema = 1\n",
            "[[diff]]\nid = \"t.one\"\nkind = \"mmio\"\noracle = \"qemu\"\nblock = \"sha\"\n",
            "offset = 0x80\naccess = \"write\"\n",
            "reason = \"a test entry whose reason is long enough to be a real one\"\n",
        );
        KnownDiffs::parse(text).expect("parses")
    }

    fn ours(index: usize, offset: u32, value: u64) -> crate::lcs::Ours {
        Ours {
            index,
            offset,
            size: 4,
            value,
            pc: 0x4038_0000 + index as u32 * 4,
            symbol: None,
        }
    }

    fn theirs(index: usize, offset: u32, value: u64) -> Record {
        Record {
            index,
            kind: Kind::Write,
            offset,
            size: 4,
            value,
        }
    }

    /// One constrained entry at `sha` 0x80, with `extra` as its constraint lines.
    fn constrained(extra: &str) -> KnownDiffs {
        let text = format!(
            "schema = 1\n[[diff]]\nid = \"t.c\"\nkind = \"mmio\"\noracle = \"qemu\"\n\
             block = \"sha\"\noffset = 0x80\noffset_end = 0x84\naccess = \"write\"\n{extra}\n\
             reason = \"a test entry whose reason is long enough to be a real one\"\n"
        );
        KnownDiffs::parse(&text).expect("parses")
    }

    #[test]
    fn an_xor_mask_ignores_its_bits_and_reports_any_other_bit() {
        let diffs = constrained("xor_mask = 0x48000");
        let oracle = [theirs(0, 0x80, 0x40_0000), theirs(1, 0x88, 9)];
        let inside = [ours(0, 0x80, 0x44_8000), ours(1, 0x88, 9)];
        let (diff, used) = diffs.diff_block_counted("qemu", "sha", &inside, &oracle);
        assert!(diff.is_equal(), "{}", crate::lcs::render(&diff));
        assert_eq!((used[0].excused, used[0].rewritten), (0, 1));
        let outside = [ours(0, 0x80, 0x44_8001), ours(1, 0x88, 9)];
        let diff = diffs.diff_block("qemu", "sha", &outside, &oracle);
        assert!(!diff.is_equal(), "bit 0 is outside the mask");
        // The report shows the values each side really wrote.
        let text = crate::lcs::render(&diff);
        assert!(
            text.contains("0x448001") && text.contains("0x400000"),
            "{text}"
        );
    }

    #[test]
    fn a_value_pair_matches_only_that_pair() {
        let diffs = constrained("values = [[0x26252625, 0x4c4b4c4b]]");
        let oracle = [theirs(0, 0x80, 0x4c4b_4c4b)];
        assert!(
            diffs
                .diff_block("qemu", "sha", &[ours(0, 0x80, 0x2625_2625)], &oracle)
                .is_equal()
        );
        assert!(
            !diffs
                .diff_block("qemu", "sha", &[ours(0, 0x80, 0x1312_1312)], &oracle)
                .is_equal(),
            "an unlisted value is reported"
        );
    }

    #[test]
    fn extra_writes_are_excused_up_to_the_budget_and_only_from_the_listed_pcs() {
        // `ours(i, ..)` writes from pc 0x40380000 + 4 i.
        let oracle = [theirs(0, 0x88, 1), theirs(1, 0x88, 2)];
        let two_extra = [
            ours(0, 0x88, 1),
            ours(1, 0x80, 7),
            ours(2, 0x80, 7),
            ours(3, 0x88, 2),
        ];
        let fits = constrained("extra_ours = 2\npcs = [0x40380004, 0x40380008]");
        let (diff, used) = fits.diff_block_counted("qemu", "sha", &two_extra, &oracle);
        assert!(diff.is_equal(), "{}", crate::lcs::render(&diff));
        assert_eq!(used[0].excused, 2);
        let short = constrained("extra_ours = 1\npcs = [0x40380004, 0x40380008]");
        assert!(
            !short
                .diff_block("qemu", "sha", &two_extra, &oracle)
                .is_equal(),
            "a third extra write over a budget of one is reported"
        );
        let other_pc = constrained("extra_ours = 2\npcs = [0x40380004]");
        assert!(
            !other_pc
                .diff_block("qemu", "sha", &two_extra, &oracle)
                .is_equal(),
            "an extra write from an unlisted pc is reported"
        );
        // A constrained entry never excuses a changed value its rewrites do not produce.
        let changed = [ours(0, 0x80, 5)];
        assert!(
            !fits
                .diff_block("qemu", "sha", &changed, &[theirs(0, 0x80, 6)])
                .is_equal()
        );
    }

    #[test]
    fn malformed_constraints_are_refused() {
        for bad in [
            "xor_mask = 0",
            "values = [0x1, 0x2]",
            "values = [[0x1]]",
            "pcs = [\"x\"]",
            "extra_ours = -1",
        ] {
            let text = format!(
                "schema = 1\n[[diff]]\nid = \"t.c\"\nkind = \"mmio\"\noracle = \"qemu\"\n\
                 block = \"sha\"\n{bad}\n\
                 reason = \"a test entry whose reason is long enough to be a real one\"\n"
            );
            assert!(KnownDiffs::parse(&text).is_err(), "`{bad}` is refused");
        }
    }

    #[test]
    fn a_narrow_entry_does_not_hide_a_second_divergence_in_the_same_block() {
        // 0x80 is listed, 0x84 is not, and the 0x84 mismatch is the one that must be reported.
        let diffs = narrow();
        let ours = [
            ours(0, 0x80, 1),
            ours(1, 0x88, 9),
            ours(2, 0x84, 2),
            ours(3, 0x8c, 4),
        ];
        let oracle = [
            theirs(0, 0x80, 0xff_ffff),
            theirs(1, 0x88, 9),
            theirs(2, 0x84, 0xdead),
            theirs(3, 0x8c, 4),
        ];
        let unfiltered = crate::lcs::diff_block("sha", &ours, &oracle);
        assert!(
            diffs.suppresses_divergence("qemu", &unfiltered).is_some(),
            "the listed 0x80 divergence is the first one, so it hides the rest today"
        );
        let diff = diffs.diff_block("qemu", "sha", &ours, &oracle);
        assert_eq!(
            diff.first,
            Some(Divergence::Changed {
                ours: ours[2].clone(),
                oracle: oracle[2],
            }),
            "the unlisted 0x84 divergence must survive the suppression of 0x80"
        );
        assert!(
            diffs.suppresses_divergence("qemu", &diff).is_none(),
            "and it must not itself be excused"
        );
        assert_eq!(diff.aligned, 1, "the 0x88 record between them aligned");
    }

    #[test]
    fn a_block_whose_every_divergence_is_listed_passes() {
        let diffs = narrow();
        let ours = [ours(0, 0x80, 1), ours(1, 0x84, 2)];
        let oracle = [theirs(0, 0x80, 0xff_ffff), theirs(1, 0x84, 2)];
        assert!(diffs.diff_block("qemu", "sha", &ours, &oracle).is_equal());
        // The same entry excuses nothing for another oracle, so the block still fails there.
        assert!(
            !diffs
                .diff_block("esp32sim", "sha", &ours, &oracle)
                .is_equal()
        );
    }

    #[test]
    fn a_listed_write_only_we_make_does_not_shift_the_rest_of_the_stream() {
        // An excused `OnlyOurs` advances our stream alone: the records after it line up again
        // and the block passes, rather than every later record being reported as moved.
        let diffs = narrow();
        let ours = [ours(0, 0x80, 1), ours(1, 0x84, 2), ours(2, 0x88, 3)];
        let oracle = [theirs(0, 0x84, 2), theirs(1, 0x88, 3)];
        let diff = diffs.diff_block("qemu", "sha", &ours, &oracle);
        assert!(diff.is_equal(), "{:?}", diff.first);
        assert_eq!(diff.aligned, 2);
    }

    #[test]
    fn a_value_rewrite_with_pcs_applies_only_to_writes_from_them() {
        // `ours(0, ..)` writes from pc 0x40380000, `ours(1, ..)` from 0x40380004.
        let diffs = constrained("values = [[0x26252625, 0x4c4b4c4b]]\npcs = [0x40380000]");
        let listed = [ours(0, 0x80, 0x2625_2625)];
        assert!(
            diffs
                .diff_block("qemu", "sha", &listed, &[theirs(0, 0x80, 0x4c4b_4c4b)])
                .is_equal()
        );
        // The same value from another pc is compared as it is: equal where the oracle agrees,
        // reported where it does not.
        let other = [ours(0, 0x80, 1), ours(1, 0x80, 0x2625_2625)];
        assert!(
            diffs
                .diff_block(
                    "qemu",
                    "sha",
                    &other,
                    &[theirs(0, 0x80, 1), theirs(1, 0x80, 0x2625_2625)]
                )
                .is_equal()
        );
        assert!(
            !diffs
                .diff_block(
                    "qemu",
                    "sha",
                    &other,
                    &[theirs(0, 0x80, 1), theirs(1, 0x80, 0x4c4b_4c4b)]
                )
                .is_equal()
        );
    }

    #[test]
    fn a_readback_entry_excuses_only_the_bits_the_reads_differed_in() {
        let diffs = constrained("readback = true");
        // Our read 0x48000, the oracle's 0: the write carried those bits and set 0x400000.
        let mut a = [ours(0, 0x80, 0x44_8000)];
        let mut b = [theirs(0, 0x80, 0x40_0000)];
        let used =
            diffs.apply_readback("qemu", "sha", &mut a, &[Some(0x4_8000)], &mut b, &[Some(0)]);
        assert_eq!((used[0].id.as_str(), used[0].rewritten), ("t.c", 1));
        assert!(diffs.diff_block("qemu", "sha", &a, &b).is_equal());
        // A bit the code set differently (bit 0) is not a read-back bit: left as it is.
        let mut a = [ours(0, 0x80, 0x44_8001)];
        let mut b = [theirs(0, 0x80, 0x40_0000)];
        assert!(
            diffs
                .apply_readback("qemu", "sha", &mut a, &[Some(0x4_8000)], &mut b, &[Some(0)])
                .is_empty()
        );
        assert_eq!(
            a[0].value, 0x44_8001,
            "a write that is not excused keeps its raw value"
        );
        assert!(!diffs.diff_block("qemu", "sha", &a, &b).is_equal());
        // A write with no read before it is compared as it is.
        let mut a = [ours(0, 0x80, 0x44_8000)];
        let mut b = [theirs(0, 0x80, 0x40_0000)];
        assert!(
            diffs
                .apply_readback("qemu", "sha", &mut a, &[None], &mut b, &[Some(0)])
                .is_empty()
        );
        // Another oracle, or a register outside the entry's range, is untouched.
        let mut a = [ours(0, 0x80, 0x44_8000)];
        let mut b = [theirs(0, 0x80, 0x40_0000)];
        assert!(
            diffs
                .apply_readback(
                    "esp32sim",
                    "sha",
                    &mut a,
                    &[Some(0x4_8000)],
                    &mut b,
                    &[Some(0)]
                )
                .is_empty()
        );
        let mut a = [ours(0, 0x84, 0x44_8000)];
        let mut b = [theirs(0, 0x84, 0x40_0000)];
        assert!(
            diffs
                .apply_readback("qemu", "sha", &mut a, &[Some(0x4_8000)], &mut b, &[Some(0)])
                .is_empty()
        );
    }

    #[test]
    fn a_readback_entry_never_pairs_across_an_extra_write() {
        let diffs = constrained("readback = true");
        let mut a = [ours(0, 0x80, 0x44_8000), ours(1, 0x80, 0x44_8000)];
        let mut b = [theirs(0, 0x80, 0x40_0000)];
        assert!(
            diffs
                .apply_readback(
                    "qemu",
                    "sha",
                    &mut a,
                    &[Some(0x4_8000), Some(0x4_8000)],
                    &mut b,
                    &[Some(0)]
                )
                .is_empty(),
            "two writes against one are not paired"
        );
        // And a readback entry excuses no divergence by itself.
        assert!(!diffs.diff_block("qemu", "sha", &a, &b).is_equal());
    }

    /// One call entry for `bootloader_flash_read_sfdp`, with `extra` as its constraint lines.
    fn call_entry(extra: &str) -> Result<KnownDiffs, SpecError> {
        KnownDiffs::parse(&format!(
            "schema = 1\n[[diff]]\nid = \"t.call\"\nkind = \"call\"\noracle = \"qemu\"\n\
             function = \"bootloader_flash_read_sfdp\"\n{extra}\n\
             reason = \"a test entry whose reason is long enough to be a real one\"\n"
        ))
    }

    #[test]
    fn a_call_entry_needs_a_count_and_takes_nothing_else() {
        assert!(call_entry("extra_oracle = 1").is_ok());
        assert!(
            call_entry("").is_err(),
            "a call entry with no count would blank the trace"
        );
        for bad in [
            "extra_oracle = 1\nxor_mask = 1",
            "extra_oracle = 1\npcs = [4]",
            "extra_oracle = 1\nreadback = true",
        ] {
            assert!(call_entry(bad).is_err(), "{bad}");
        }
        let diffs = call_entry("extra_oracle = 1").unwrap();
        assert!(
            diffs
                .suppresses_mmio("qemu", "spi1", 0, Kind::Write)
                .is_none(),
            "a call entry excuses no register"
        );
    }

    #[test]
    fn a_call_entry_excuses_its_function_up_to_its_count_and_nothing_else() {
        use crate::calltrace::{CallTrace, Divergence as CallDivergence};
        let diffs = call_entry("extra_oracle = 1").unwrap();
        let ours = CallTrace::from_names("ours", ["a", "b", "c"]);
        let one = CallTrace::from_names("qemu", ["a", "bootloader_flash_read_sfdp", "b", "c"]);
        let (diff, used) = diffs.diff_calls("qemu", &ours, &one);
        assert!(diff.is_equal(), "{diff:?}");
        assert_eq!(diff.aligned, 3);
        assert_eq!((used[0].id.as_str(), used[0].excused), ("t.call", 1));
        let two = CallTrace::from_names(
            "qemu",
            [
                "a",
                "bootloader_flash_read_sfdp",
                "b",
                "bootloader_flash_read_sfdp",
                "c",
            ],
        );
        let (diff, _) = diffs.diff_calls("qemu", &ours, &two);
        assert!(
            matches!(diff.first, Some(CallDivergence::OnlyOracle(ref e)) if e.index == 3),
            "a second extra entry over a count of one is reported: {diff:?}"
        );
        // Another function, the other side, or another oracle is not excused.
        let other = CallTrace::from_names("qemu", ["a", "x", "b", "c"]);
        assert!(!diffs.diff_calls("qemu", &ours, &other).0.is_equal());
        let mine = CallTrace::from_names("ours", ["a", "bootloader_flash_read_sfdp", "b", "c"]);
        let plain = CallTrace::from_names("qemu", ["a", "b", "c"]);
        assert!(!diffs.diff_calls("qemu", &mine, &plain).0.is_equal());
        assert!(!diffs.diff_calls("esp32sim", &ours, &one).0.is_equal());
    }
}
