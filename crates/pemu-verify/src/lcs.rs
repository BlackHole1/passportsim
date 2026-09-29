//! Per-block write-stream LCS diff. The `(offset, size, value)` write streams of our run and the
//! QEMU oracle are aligned per block, ignoring interleaving across blocks, so a scheduler that
//! reorders work between blocks is not a difference. The **first** divergence per block is
//! reported with 20 records of context, our PC and symbol, and the QEMU record index.
//!
//! A full LCS is quadratic and one boot writes about 431,000 SHA records, so the streams are
//! walked forward while they agree and the LCS runs on [`WINDOW`] records each side of the first
//! disagreement: enough to tell an insertion from a deletion from a changed value. A divergence
//! that needs more is reported as unaligned rather than guessed at.

use crate::qemu_ingest::Record;

/// Records examined on each side when classifying a divergence.
pub const WINDOW: usize = 256;

/// Records of context a divergence report carries.
pub const CONTEXT: usize = 20;

/// One write of our run, with the provenance a report needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ours {
    pub index: usize,
    pub offset: u32,
    pub size: u8,
    pub value: u64,
    /// PC of the instruction that wrote.
    pub pc: u32,
    /// Symbol containing that PC, when the loader resolved one.
    pub symbol: Option<String>,
}

impl Ours {
    /// The alignment key.
    pub fn key(&self) -> (u32, u8, u64) {
        (self.offset, self.size, self.value)
    }
}

/// One step of an alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// The records at these positions align.
    Equal {
        /// Position in our stream.
        ours: usize,
        oracle: usize,
    },
    /// Only our stream has this record.
    OnlyOurs(usize),
    /// Only the oracle stream has this record.
    OnlyOracle(usize),
}

/// The longest common subsequence of two key slices, as an edit script. On a tie the walk prefers
/// consuming from `ours`, so the same inputs always give the same report.
pub fn align<K: PartialEq>(ours: &[K], oracle: &[K]) -> Vec<Edit> {
    let (n, m) = (ours.len(), oracle.len());
    // `table[i][j]` is the LCS length of `ours[i..]` and `oracle[j..]`.
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if ours[i] == oracle[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut script = Vec::with_capacity(n.max(m));
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if ours[i] == oracle[j] {
            script.push(Edit::Equal { ours: i, oracle: j });
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            script.push(Edit::OnlyOurs(i));
            i += 1;
        } else {
            script.push(Edit::OnlyOracle(j));
            j += 1;
        }
    }
    script.extend((i..n).map(Edit::OnlyOurs));
    script.extend((j..m).map(Edit::OnlyOracle));
    script
}

/// What the first divergence of a block looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// We wrote a record the oracle does not have at this point.
    OnlyOurs {
        record: Ours,
        /// Index of the next oracle record, for the report.
        oracle_index: Option<usize>,
    },
    /// The oracle wrote a record we do not have at this point.
    OnlyOracle {
        record: Record,
        /// Index of the next record of ours, for the report.
        our_index: Option<usize>,
    },
    /// Both sides wrote here and the records differ.
    Changed {
        /// Our record.
        ours: Ours,
        oracle: Record,
    },
    /// The streams disagree and did not realign inside [`WINDOW`] records.
    Unaligned {
        /// Our record at the disagreement.
        ours: Option<Ours>,
        /// The oracle's record at the disagreement.
        oracle: Option<Record>,
    },
}

/// The result of diffing one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDiff {
    pub block: String,
    pub ours_len: usize,
    pub oracle_len: usize,
    /// Records that aligned before the first divergence.
    pub aligned: usize,
    pub first: Option<Divergence>,
    /// Up to [`CONTEXT`] of our records before the divergence, oldest first.
    pub our_context: Vec<Ours>,
    /// Up to [`CONTEXT`] oracle records before the divergence, oldest first.
    pub oracle_context: Vec<Record>,
}

impl BlockDiff {
    /// True when the block's write streams agree end to end.
    pub fn is_equal(&self) -> bool {
        self.first.is_none()
    }
}

/// Diffs one block's write streams.
pub fn diff_block(block: &str, ours: &[Ours], oracle: &[Record]) -> BlockDiff {
    diff_block_with(block, ours, oracle, &mut |_| false)
}

/// Diffs one block's write streams, skipping the divergences `excused` accepts. The result is the
/// first **unsuppressed** divergence: an excused one advances the stream that carried it (both,
/// for a changed value) and the walk continues, so a listed entry for one register cannot hide an
/// unlisted difference at the next. Each step advances at least one index, so it terminates.
pub fn diff_block_with(
    block: &str,
    ours: &[Ours],
    oracle: &[Record],
    excused: &mut dyn FnMut(&Divergence) -> bool,
) -> BlockDiff {
    let (mut i, mut j) = (0, 0);
    let mut aligned = 0;
    loop {
        let common = ours[i..]
            .iter()
            .zip(oracle[j..].iter())
            .take_while(|(a, b)| a.key() == b.key())
            .count();
        i += common;
        j += common;
        aligned += common;
        let mut diff = BlockDiff {
            block: block.to_string(),
            ours_len: ours.len(),
            oracle_len: oracle.len(),
            aligned,
            first: None,
            our_context: ours[i.saturating_sub(CONTEXT)..i].to_vec(),
            oracle_context: oracle[j.saturating_sub(CONTEXT)..j].to_vec(),
        };
        if i == ours.len() && j == oracle.len() {
            return diff;
        }
        let divergence = classify(&ours[i..], &oracle[j..]);
        if !excused(&divergence) {
            diff.first = Some(divergence);
            return diff;
        }
        match divergence {
            Divergence::OnlyOurs { .. } => i += 1,
            Divergence::OnlyOracle { .. } => j += 1,
            // A changed value consumed a record on each side; an unaligned pair has one on each
            // side too, and stepping past both is the only resume point the window offers.
            Divergence::Changed { .. } | Divergence::Unaligned { .. } => {
                i += 1;
                j += 1;
            }
        }
    }
}

/// Classifies the disagreement at the head of two streams.
fn classify(ours: &[Ours], oracle: &[Record]) -> Divergence {
    match (ours.first(), oracle.first()) {
        (Some(our), None) => {
            return Divergence::OnlyOurs {
                record: our.clone(),
                oracle_index: None,
            };
        }
        (None, Some(their)) => {
            return Divergence::OnlyOracle {
                record: *their,
                our_index: None,
            };
        }
        (None, None) => unreachable!("a divergence has a record on at least one side"),
        (Some(_), Some(_)) => {}
    }
    let our_window: Vec<(u32, u8, u64)> = ours.iter().take(WINDOW).map(Ours::key).collect();
    let oracle_window: Vec<(u32, u8, u64)> = oracle.iter().take(WINDOW).map(Record::key).collect();
    let script = align(&our_window, &oracle_window);
    if !script.iter().any(|edit| matches!(edit, Edit::Equal { .. })) {
        // Nothing in the next `WINDOW` records of either side matches, so calling this an
        // insertion or a changed value would be a guess.
        return Divergence::Unaligned {
            ours: Some(ours[0].clone()),
            oracle: Some(oracle[0]),
        };
    }
    let mut edits = script.iter();
    match (edits.next(), edits.next()) {
        (Some(Edit::OnlyOurs(_)), Some(Edit::OnlyOracle(_)))
        | (Some(Edit::OnlyOracle(_)), Some(Edit::OnlyOurs(_))) => Divergence::Changed {
            ours: ours[0].clone(),
            oracle: oracle[0],
        },
        (Some(Edit::OnlyOurs(_)), _) => Divergence::OnlyOurs {
            record: ours[0].clone(),
            oracle_index: Some(oracle[0].index),
        },
        (Some(Edit::OnlyOracle(_)), _) => Divergence::OnlyOracle {
            record: oracle[0],
            our_index: Some(ours[0].index),
        },
        _ => Divergence::Unaligned {
            ours: Some(ours[0].clone()),
            oracle: Some(oracle[0]),
        },
    }
}

/// Renders a block diff the way a failing oracle test prints it.
pub fn render(diff: &BlockDiff) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "block {}: {} of our writes, {} oracle writes, {} aligned\n",
        diff.block, diff.ours_len, diff.oracle_len, diff.aligned
    ));
    let Some(first) = &diff.first else {
        out.push_str("  no divergence\n");
        return out;
    };
    for record in &diff.our_context {
        out.push_str(&format!(
            "  ours   [{}] {:#06x} w{} = {:#x} pc {:#010x} {}\n",
            record.index,
            record.offset,
            record.size,
            record.value,
            record.pc,
            record.symbol.as_deref().unwrap_or("?")
        ));
    }
    for record in &diff.oracle_context {
        out.push_str(&format!(
            "  oracle [{}] {:#06x} w{} = {:#x}\n",
            record.index, record.offset, record.size, record.value
        ));
    }
    out.push_str(&match first {
        Divergence::OnlyOurs {
            record,
            oracle_index,
        } => format!(
            "  first divergence: only ours [{}] {:#06x} w{} = {:#x} pc {:#010x} {} (next oracle \
             record {:?})\n",
            record.index,
            record.offset,
            record.size,
            record.value,
            record.pc,
            record.symbol.as_deref().unwrap_or("?"),
            oracle_index
        ),
        Divergence::OnlyOracle { record, our_index } => format!(
            "  first divergence: only oracle [{}] {:#06x} w{} = {:#x} (next record of ours {:?})\n",
            record.index, record.offset, record.size, record.value, our_index
        ),
        Divergence::Changed { ours, oracle } => format!(
            "  first divergence: ours [{}] {:#06x} w{} = {:#x} pc {:#010x} {} against oracle [{}] \
             {:#06x} w{} = {:#x}\n",
            ours.index,
            ours.offset,
            ours.size,
            ours.value,
            ours.pc,
            ours.symbol.as_deref().unwrap_or("?"),
            oracle.index,
            oracle.offset,
            oracle.size,
            oracle.value
        ),
        Divergence::Unaligned { ours, oracle } => format!(
            "  first divergence: streams did not realign inside {WINDOW} records (ours {:?}, \
             oracle {:?})\n",
            ours.as_ref().map(|r| r.index),
            oracle.as_ref().map(|r| r.index)
        ),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu_ingest::Kind;

    fn our(index: usize, offset: u32, value: u64) -> Ours {
        Ours {
            index,
            offset,
            size: 4,
            value,
            pc: 0x4038_0000 + index as u32 * 4,
            symbol: Some("esp_sha_block".to_string()),
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

    fn stream(values: &[(u32, u64)]) -> (Vec<Ours>, Vec<Record>) {
        (
            values
                .iter()
                .enumerate()
                .map(|(i, (o, v))| our(i, *o, *v))
                .collect(),
            values
                .iter()
                .enumerate()
                .map(|(i, (o, v))| theirs(i, *o, *v))
                .collect(),
        )
    }

    #[test]
    fn align_finds_the_longest_common_subsequence() {
        let script = align(&['a', 'b', 'c', 'd'], &['a', 'x', 'c', 'd']);
        let equal: Vec<_> = script
            .iter()
            .filter_map(|edit| match edit {
                Edit::Equal { ours, oracle } => Some((*ours, *oracle)),
                _ => None,
            })
            .collect();
        assert_eq!(equal, vec![(0, 0), (2, 2), (3, 3)]);
    }

    #[test]
    fn align_handles_an_empty_side() {
        assert_eq!(
            align::<char>(&[], &['a']),
            vec![Edit::OnlyOracle(0)],
            "everything is the oracle's"
        );
        assert_eq!(align::<char>(&['a'], &[]), vec![Edit::OnlyOurs(0)]);
        assert!(align::<char>(&[], &[]).is_empty());
    }

    #[test]
    fn identical_streams_do_not_diverge() {
        let (ours, oracle) = stream(&[(0x80, 1), (0x84, 2), (0x18, 0)]);
        let diff = diff_block("sha", &ours, &oracle);
        assert!(diff.is_equal());
        assert_eq!(diff.aligned, 3);
        assert!(render(&diff).contains("no divergence"));
    }

    #[test]
    fn a_changed_value_is_reported_with_both_records_and_our_pc() {
        let (ours, _) = stream(&[(0x80, 1), (0x84, 2), (0x88, 3)]);
        let (_, oracle) = stream(&[(0x80, 1), (0x84, 0xbad), (0x88, 3)]);
        let diff = diff_block("sha", &ours, &oracle);
        assert_eq!(diff.aligned, 1);
        assert_eq!(
            diff.first,
            Some(Divergence::Changed {
                ours: our(1, 0x84, 2),
                oracle: theirs(1, 0x84, 0xbad),
            })
        );
        let text = render(&diff);
        assert!(text.contains("pc 0x40380004"), "{text}");
        assert!(text.contains("esp_sha_block"), "{text}");
    }

    #[test]
    fn a_write_only_we_make_is_reported_as_ours() {
        let (ours, _) = stream(&[(0x80, 1), (0x84, 2), (0x88, 3)]);
        let (_, oracle) = stream(&[(0x80, 1), (0x88, 3)]);
        let diff = diff_block("sha", &ours, &oracle);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOurs {
                record: our(1, 0x84, 2),
                oracle_index: Some(1),
            })
        );
    }

    #[test]
    fn a_write_only_the_oracle_makes_is_reported_as_the_oracles() {
        let (ours, _) = stream(&[(0x80, 1), (0x88, 3)]);
        let (_, oracle) = stream(&[(0x80, 1), (0x84, 2), (0x88, 3)]);
        let diff = diff_block("sha", &ours, &oracle);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOracle {
                record: theirs(1, 0x84, 2),
                our_index: Some(1),
            })
        );
    }

    #[test]
    fn a_truncated_stream_diverges_at_its_end() {
        let (ours, _) = stream(&[(0x80, 1), (0x84, 2)]);
        let (_, oracle) = stream(&[(0x80, 1)]);
        let diff = diff_block("sha", &ours, &oracle);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOurs {
                record: our(1, 0x84, 2),
                oracle_index: None,
            })
        );
    }

    #[test]
    fn the_report_carries_twenty_records_of_context() {
        let values: Vec<(u32, u64)> = (0..40).map(|i| (0x80 + i * 4, u64::from(i))).collect();
        let (ours, _) = stream(&values);
        let mut changed = values.clone();
        changed[30].1 = 0xbad;
        let (_, oracle) = stream(&changed);
        let diff = diff_block("sha", &ours, &oracle);
        assert_eq!(diff.aligned, 30);
        assert_eq!(diff.our_context.len(), CONTEXT);
        assert_eq!(diff.oracle_context.len(), CONTEXT);
        assert_eq!(diff.our_context[0].index, 10);
        assert_eq!(diff.our_context[CONTEXT - 1].index, 29);
    }

    #[test]
    fn interleaving_across_blocks_is_invisible_because_blocks_are_diffed_apart() {
        // The same two block streams in a different global order: each per-block diff is equal.
        let (sha_ours, sha_oracle) = stream(&[(0x80, 1), (0x84, 2)]);
        let (timg_ours, timg_oracle) = stream(&[(0x00, 7)]);
        assert!(diff_block("sha", &sha_ours, &sha_oracle).is_equal());
        assert!(diff_block("timg0", &timg_ours, &timg_oracle).is_equal());
    }

    #[test]
    fn a_stream_that_does_not_realign_inside_the_window_is_reported_as_unaligned() {
        let ours: Vec<Ours> = (0..WINDOW + 8)
            .map(|i| our(i, 0x80, u64::from(i as u32)))
            .collect();
        let oracle: Vec<Record> = (0..WINDOW + 8)
            .map(|i| theirs(i, 0x84, u64::from(i as u32) + 1_000_000))
            .collect();
        let diff = diff_block("sha", &ours, &oracle);
        assert!(matches!(diff.first, Some(Divergence::Unaligned { .. })));
        assert!(render(&diff).contains("did not realign"));
    }
}
