//! Token-efficient output shaping: bounded text per call, and only what is new on the next. Five
//! rules, in order:
//!
//! 1. Cursors: a channel is one append-only byte stream; a reset appends a marker and never
//!    rewinds. [`shape_serial`] consumes whole lines only, so an unfinished line is never shown
//!    twice.
//! 2. ANSI is stripped and CRLF normalized to LF, so hosts produce the same bytes.
//! 3. Repeat collapse: a run of identical lines becomes one `(xN) <line>` entry.
//! 4. Line cap: a longer line keeps its first [`LINE_CHARS`] characters (not bytes) plus `...(+N
//!    chars)`.
//! 5. Head and tail elision: [`HEAD_LINES`] and [`TAIL_LINES`] entries survive, with one marker
//!    line counting the rest.
//!
//! [`shape_text_within`] also enforces a character budget. Redaction runs before shaping, never
//! after, since a cap can cut a secret into an unmatchable prefix; [`crate::redact::Redactor`] does
//! both in order.

use std::fmt::Write as _;

pub const HEAD_LINES: usize = 10;

pub const TAIL_LINES: usize = 30;

pub const LINE_CHARS: usize = 400;

pub const TEXT_BUDGET_CHARS: usize = 4_000;

/// Not enforced here: `xtask agent-budget` sums real outputs against it.
pub const LOOP_BUDGET_CHARS: usize = 8_000;

/// 13 for the fixed text plus room for a 7-digit count.
const CUT_SUFFIX_ROOM: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeLimits {
    pub head: usize,
    pub tail: usize,
    /// 0 disables the cap.
    pub line_chars: usize,
    pub collapse: bool,
    /// `None` is unbounded.
    pub budget_chars: Option<usize>,
}

impl ShapeLimits {
    pub const DEFAULT: ShapeLimits = ShapeLimits {
        head: HEAD_LINES,
        tail: TAIL_LINES,
        line_chars: LINE_CHARS,
        collapse: true,
        budget_chars: Some(TEXT_BUDGET_CHARS),
    };

    /// Every rule off: what a caller that asked for the full channel gets.
    pub const UNBOUNDED: ShapeLimits = ShapeLimits {
        head: usize::MAX,
        tail: usize::MAX,
        line_chars: 0,
        collapse: false,
        budget_chars: None,
    };

    /// As `--max-lines` gives.
    #[must_use]
    pub const fn head_tail(head: usize, tail: usize) -> ShapeLimits {
        ShapeLimits {
            head,
            tail,
            ..ShapeLimits::DEFAULT
        }
    }
}

impl Default for ShapeLimits {
    fn default() -> Self {
        ShapeLimits::DEFAULT
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Without the `(xN) ` prefix.
    pub line: String,
    pub repeats: usize,
    pub cut_chars: usize,
}

impl Entry {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.line.len() + 16);
        if self.repeats > 1 {
            let _ = write!(out, "(x{}) ", self.repeats);
        }
        out.push_str(&self.line);
        if self.cut_chars > 0 {
            let _ = write!(out, "...(+{} chars)", self.cut_chars);
        }
        out
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shaped {
    pub head: Vec<Entry>,
    pub tail: Vec<Entry>,
    pub lines_total: usize,
    pub entries_total: usize,
    pub lines_shown: usize,
    pub elided: usize,
    /// What the marker line reports.
    pub elided_lines: usize,
    pub budget_truncated: bool,
}

impl Shaped {
    #[must_use]
    pub fn is_elided(&self) -> bool {
        self.elided > 0
    }

    #[must_use]
    pub fn elision_marker(&self) -> String {
        format!("... {} lines elided ...", self.elided_lines)
    }

    /// No trailing newline.
    #[must_use]
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = self.head.iter().map(Entry::render).collect();
        if self.is_elided() {
            lines.push(self.elision_marker());
        }
        lines.extend(self.tail.iter().map(Entry::render));
        lines.join("\n")
    }

    #[must_use]
    pub fn to_json_lines(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.head
                .iter()
                .chain(self.tail.iter())
                .map(|e| serde_json::Value::String(e.render()))
                .collect(),
        )
    }
}

/// Strips CSI, OSC and other two-byte escapes and normalizes CRLF to LF. A trailing lone `ESC`
/// never eats a newline. A lone CR rewrites its line, as a terminal does, so a progress line is the
/// one line a user saw rather than one line per update.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if matches!(c, '@'..='~') {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Eating the newline would join two lines.
                Some('\n' | '\r') => {}
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push('\n');
                } else {
                    let line_start = out.rfind('\n').map_or(0, |at| at + 1);
                    out.truncate(line_start);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `max_chars` of 0 disables the cap.
#[must_use]
pub fn cap_line(line: &str, max_chars: usize) -> (String, usize) {
    if max_chars == 0 {
        return (line.to_string(), 0);
    }
    let mut kept = String::with_capacity(line.len().min(max_chars * 4));
    let mut taken = 0usize;
    let mut dropped = 0usize;
    for c in line.chars() {
        if taken < max_chars {
            kept.push(c);
            taken += 1;
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

/// A trailing newline does not create an empty last line; a partial last line is kept.
fn split_lines(text: &str) -> Vec<&str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() {
        return Vec::new();
    }
    body.split('\n').collect()
}

fn entries(lines: &[&str], limits: &ShapeLimits) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(lines.len());
    for line in lines {
        if limits.collapse
            && let Some(last) = out.last_mut()
            && last.repeats >= 1
            && line_equals(last, line, limits)
        {
            last.repeats += 1;
            continue;
        }
        let (kept, cut) = cap_line(line, limits.line_chars);
        out.push(Entry {
            line: kept,
            repeats: 1,
            cut_chars: cut,
        });
    }
    out
}

/// Two lines that differ only after the cap are different lines and must not collapse.
fn line_equals(entry: &Entry, line: &str, limits: &ShapeLimits) -> bool {
    let (kept, cut) = cap_line(line, limits.line_chars);
    entry.line == kept && entry.cut_chars == cut
}

/// The lines must already be ANSI-stripped and redacted. The split is the same shape either side of
/// the elision boundary, so a consumer reading `tail` for the newest lines always finds them.
#[must_use]
pub fn shape_lines(lines: &[&str], limits: &ShapeLimits) -> Shaped {
    let entries = entries(lines, limits);
    let entries_total = entries.len();
    let mut shaped = Shaped {
        lines_total: lines.len(),
        entries_total,
        ..Shaped::default()
    };
    let head = limits.head.min(entries_total);
    let tail = limits.tail.min(entries_total - head);
    let cut = entries_total - head - tail;
    let mut iter = entries.into_iter();
    shaped.head = iter.by_ref().take(head).collect();
    let dropped: Vec<Entry> = iter.by_ref().take(cut).collect();
    shaped.tail = iter.collect();
    shaped.elided = cut;
    shaped.elided_lines = dropped.iter().map(|e| e.repeats).sum();
    shaped.lines_shown = shaped.head.len() + shaped.tail.len();
    shaped
}

/// The character budget is not applied; use [`shape_text_within`] for that. The text must already
/// be redacted.
#[must_use]
pub fn shape_text(text: &str, limits: &ShapeLimits) -> Shaped {
    let clean = strip_ansi(text);
    let lines = split_lines(&clean);
    shape_lines(&lines, limits)
}

/// Halves the tail, then the head, then truncates, since the head carries the boot banner. The
/// elision marker is the one floor: dropping it would claim the output was complete. The text must
/// already be redacted.
#[must_use]
pub fn shape_text_within(text: &str, limits: &ShapeLimits) -> Shaped {
    let Some(budget) = limits.budget_chars else {
        return shape_text(text, limits);
    };
    let clean = strip_ansi(text);
    let lines = split_lines(&clean);
    let mut limits = *limits;
    loop {
        let shaped = shape_lines(&lines, &limits);
        if shaped.text().chars().count() <= budget {
            return shaped;
        }
        if limits.tail > 1 {
            limits.tail /= 2;
        } else if limits.head > 1 {
            limits.head /= 2;
        } else {
            let mut shaped = shaped;
            shaped.budget_truncated = true;
            truncate_in_place(&mut shaped, budget);
            return shaped;
        }
    }
}

/// Head entries are cut against one running remainder, or two entries of `room` characters would
/// render twice the budget. A cut to zero goes through [`cut_entry_to`], because [`cap_line`]'s 0
/// means "no cap".
fn truncate_in_place(shaped: &mut Shaped, budget: usize) {
    let fixed: usize = shaped
        .tail
        .iter()
        .map(|e| e.render().chars().count() + 1)
        .sum::<usize>()
        + if shaped.is_elided() {
            shaped.elision_marker().chars().count() + 1
        } else {
            0
        };
    let mut remaining = budget.saturating_sub(fixed);
    for entry in &mut shaped.head {
        // Over-counts by one at worst, which keeps the result inside the budget.
        let rendered = entry.render().chars().count() + 1;
        if rendered <= remaining {
            remaining -= rendered;
            continue;
        }
        let overhead = rendered - entry.line.chars().count();
        cut_entry_to(
            entry,
            remaining.saturating_sub(overhead.max(CUT_SUFFIX_ROOM)),
        );
        remaining = remaining.saturating_sub(entry.render().chars().count() + 1);
    }
    // Drop the tail first (the head carries the boot banner), then the head.
    while shaped.text().chars().count() > budget && shaped.lines_shown > 0 {
        let dropped = if shaped.tail.is_empty() {
            shaped.head.pop()
        } else {
            Some(shaped.tail.remove(0))
        };
        let Some(dropped) = dropped else { break };
        shaped.elided += 1;
        shaped.elided_lines += dropped.repeats;
        shaped.lines_shown -= 1;
    }
}

/// `keep` of 0 keeps nothing, unlike [`cap_line`].
fn cut_entry_to(entry: &mut Entry, keep: usize) {
    let dropped = entry.line.chars().count().saturating_sub(keep);
    if dropped == 0 {
        return;
    }
    entry.line = entry.line.chars().take(keep).collect();
    entry.cut_chars += dropped;
}

/// An absolute byte offset into one channel's lifetime stream. A reset appends a [`ResetMark`] and
/// never rewinds, so a cursor kept across a reboot names the same bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor(pub u64);

impl Cursor {
    #[must_use]
    pub const fn advanced(self, bytes: u64) -> Cursor {
        Cursor(self.0.saturating_add(bytes))
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetMark {
    /// Offset of the first byte printed after the reset.
    pub cursor: Cursor,
    pub vt_us: u64,
    /// As the boot ROM reports it (`POWERON_RESET`, `RTC_SW_CPU_RESET`, ...).
    pub reason: String,
}

impl ResetMark {
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cursor": self.cursor.0,
            "vt_us": self.vt_us,
            "reason": self.reason,
        })
    }
}

/// A bounded view of one channel's new bytes plus the cursor to pass next time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SerialExcerpt {
    pub cursor: Cursor,
    pub next_cursor: Cursor,
    /// Whole lines only.
    pub bytes: u64,
    /// Held back because the last line has no newline yet.
    pub held_bytes: u64,
    pub shaped: Shaped,
    pub resets_in_range: Vec<ResetMark>,
}

impl SerialExcerpt {
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cursor": self.cursor.0,
            "next_cursor": self.next_cursor.0,
            "bytes": self.bytes,
            "lines_total": self.shaped.lines_total,
            "lines_shown": self.shaped.lines_shown,
            "elided": self.shaped.elided,
            "elided_lines": self.shaped.elided_lines,
            "head": serde_json::Value::Array(
                self.shaped.head.iter().map(|e| serde_json::Value::String(e.render())).collect(),
            ),
            "tail": serde_json::Value::Array(
                self.shaped.tail.iter().map(|e| serde_json::Value::String(e.render())).collect(),
            ),
            "resets_in_range": serde_json::Value::Array(
                self.resets_in_range.iter().map(ResetMark::to_json).collect(),
            ),
        })
    }

    #[must_use]
    pub fn to_text(&self) -> String {
        self.shaped.text()
    }
}

/// Only whole lines are consumed: bytes after the last newline stay outside `next_cursor`, so a
/// cursor loop is safe. Invalid UTF-8 is replaced. The chunk must already be redacted.
#[must_use]
pub fn shape_serial(
    chunk: &[u8],
    from: Cursor,
    resets: &[ResetMark],
    limits: &ShapeLimits,
) -> SerialExcerpt {
    let (text, mut excerpt) = serial_chunk(chunk, from, resets);
    excerpt.shaped = shape_within(&text, limits);
    excerpt
}

/// Leaves [`SerialExcerpt::shaped`] empty for the caller to fill; the redactor uses it so redaction
/// never moves the cursor.
#[must_use]
pub fn serial_chunk(chunk: &[u8], from: Cursor, resets: &[ResetMark]) -> (String, SerialExcerpt) {
    let consumed = match chunk.iter().rposition(|&b| b == b'\n') {
        Some(last) => last + 1,
        None => 0,
    };
    let text = String::from_utf8_lossy(&chunk[..consumed]).into_owned();
    let next_cursor = from.advanced(consumed as u64);
    let excerpt = SerialExcerpt {
        cursor: from,
        next_cursor,
        bytes: consumed as u64,
        held_bytes: (chunk.len() - consumed) as u64,
        shaped: Shaped::default(),
        resets_in_range: resets
            .iter()
            .filter(|r| r.cursor >= from && r.cursor < next_cursor)
            .cloned()
            .collect(),
    };
    (text, excerpt)
}

#[must_use]
pub fn shape_within(text: &str, limits: &ShapeLimits) -> Shaped {
    match limits.budget_chars {
        Some(_) => shape_text_within(text, limits),
        None => shape_text(text, limits),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("p{i}")).collect()
    }

    fn refs(lines: &[String]) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_defaults_are_the_documented_numbers() {
        assert_eq!(ShapeLimits::DEFAULT.head, 10);
        assert_eq!(ShapeLimits::DEFAULT.tail, 30);
        assert_eq!(ShapeLimits::DEFAULT.line_chars, 400);
        assert_eq!(ShapeLimits::DEFAULT.budget_chars, Some(4_000));
        const { assert!(ShapeLimits::DEFAULT.collapse) };
    }

    #[test]
    fn ansi_is_stripped_and_crlf_normalized() {
        // The CR overwrites `writing` as on a terminal, so the line reads `done`.
        let raw = "\u{1b}[0;32mI (312) main: ready\u{1b}[0m\r\nwriting\r\u{1b}]0;title\u{7}done\n";
        assert_eq!(strip_ansi(raw), "I (312) main: ready\ndone\n");
    }

    #[test]
    fn a_lone_cr_rewrites_its_line_instead_of_adding_one() {
        assert_eq!(strip_ansi("a\rb"), "b");
        let raw = "boot\nwriting 10 %\rwriting 50 %\rwriting 100 %\ndone\n";
        let shaped = shape_text(raw, &ShapeLimits::DEFAULT);
        assert_eq!(shaped.lines_total, 3);
        assert_eq!(shaped.text(), "boot\nwriting 100 %\ndone");
    }

    #[test]
    fn a_truncated_escape_never_swallows_a_newline() {
        assert_eq!(strip_ansi("a\u{1b}\nb"), "a\nb");
        assert_eq!(strip_ansi("a\u{1b}\r\nb"), "a\nb");
        assert_eq!(strip_ansi("a\u{1b}"), "a");
    }

    #[test]
    fn a_line_of_exactly_the_cap_is_not_cut_and_one_more_is() {
        let at_cap = "x".repeat(LINE_CHARS);
        let over = "x".repeat(LINE_CHARS + 1);
        let shaped = shape_lines(&[at_cap.as_str(), over.as_str()], &ShapeLimits::DEFAULT);
        assert_eq!(shaped.head[0].cut_chars, 0);
        assert_eq!(shaped.head[0].render(), at_cap);
        assert_eq!(shaped.head[1].cut_chars, 1);
        assert_eq!(shaped.head[1].render(), format!("{at_cap}...(+1 chars)"));
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        // Four-byte scalars: a byte cap would split one.
        let line = "\u{1f600}".repeat(LINE_CHARS + 2);
        let (kept, cut) = cap_line(&line, LINE_CHARS);
        assert_eq!(kept.chars().count(), LINE_CHARS);
        assert_eq!(cut, 2);
        assert_eq!(kept.len(), LINE_CHARS * 4);
    }

    #[test]
    fn a_run_of_identical_lines_collapses_to_one_entry() {
        let lines = ["boot", "wdt reset", "wdt reset", "wdt reset", "menu"];
        let shaped = shape_lines(&lines, &ShapeLimits::DEFAULT);
        assert_eq!(shaped.text(), "boot\n(x3) wdt reset\nmenu");
        assert_eq!(shaped.lines_total, 5);
        assert_eq!(shaped.entries_total, 3);
        assert!(!shaped.is_elided());
    }

    #[test]
    fn only_adjacent_lines_collapse() {
        let lines = ["a", "a", "b", "a"];
        let shaped = shape_lines(&lines, &ShapeLimits::DEFAULT);
        assert_eq!(shaped.text(), "(x2) a\nb\na");
    }

    #[test]
    fn two_lines_that_differ_only_past_the_cap_do_not_collapse() {
        let base = "y".repeat(LINE_CHARS);
        let one = format!("{base}1");
        let two = format!("{base}22");
        let shaped = shape_lines(&[one.as_str(), two.as_str()], &ShapeLimits::DEFAULT);
        assert_eq!(shaped.entries_total, 2);
    }

    #[test]
    fn collapse_off_keeps_every_line() {
        let limits = ShapeLimits {
            collapse: false,
            ..ShapeLimits::DEFAULT
        };
        let shaped = shape_lines(&["a", "a", "a"], &limits);
        assert_eq!(shaped.text(), "a\na\na");
    }

    #[test]
    fn nothing_is_elided_at_exactly_head_plus_tail_entries() {
        let lines = numbered(HEAD_LINES + TAIL_LINES);
        let shaped = shape_lines(&refs(&lines), &ShapeLimits::DEFAULT);
        assert_eq!(shaped.lines_shown, HEAD_LINES + TAIL_LINES);
        assert_eq!(shaped.elided, 0);
        assert_eq!(shaped.elided_lines, 0);
        assert!(!shaped.text().contains("elided"));
        assert_eq!(shaped.head.len(), HEAD_LINES);
        assert_eq!(shaped.tail.len(), TAIL_LINES);
        assert_eq!(shaped.tail.last().unwrap().line, "p39");
    }

    #[test]
    fn the_tail_fills_before_the_elision_boundary_too() {
        for count in [
            1,
            5,
            HEAD_LINES,
            HEAD_LINES + 1,
            HEAD_LINES + TAIL_LINES - 1,
        ] {
            let lines = numbered(count);
            let shaped = shape_lines(&refs(&lines), &ShapeLimits::DEFAULT);
            assert_eq!(shaped.head.len(), count.min(HEAD_LINES), "{count} entries");
            assert_eq!(
                shaped.tail.len(),
                count.saturating_sub(HEAD_LINES),
                "{count} entries"
            );
            assert_eq!(shaped.elided, 0, "{count} entries");
        }
    }

    #[test]
    fn one_entry_past_the_boundary_elides_exactly_one_line() {
        let lines = numbered(HEAD_LINES + TAIL_LINES + 1);
        let shaped = shape_lines(&refs(&lines), &ShapeLimits::DEFAULT);
        assert_eq!(shaped.head.len(), HEAD_LINES);
        assert_eq!(shaped.tail.len(), TAIL_LINES);
        assert_eq!(shaped.elided, 1);
        assert_eq!(shaped.elided_lines, 1);
        assert_eq!(shaped.head.last().unwrap().line, "p9");
        assert_eq!(shaped.tail[0].line, "p11");
        assert!(shaped.text().contains("... 1 lines elided ..."));
    }

    #[test]
    fn the_marker_counts_source_lines_not_collapsed_entries() {
        let limits = ShapeLimits::head_tail(1, 1);
        let lines = ["start", "same", "same", "same", "same", "same", "end"];
        let shaped = shape_lines(&lines, &limits);
        assert_eq!(shaped.elided, 1);
        assert_eq!(shaped.elided_lines, 5);
        assert_eq!(shaped.text(), "start\n... 5 lines elided ...\nend");
    }

    #[test]
    fn the_golden_shape_of_a_boot_log() {
        let limits = ShapeLimits::head_tail(2, 2);
        let long = format!("I (99) app: {}", "z".repeat(LINE_CHARS));
        let text = format!(
            "ESP-ROM:esp32c3-api1-20210207\nI (24) boot: ESP-IDF v5.5.3\nnoise\nnoise\nnoise\n{long}\nI (912) pk_app: ready\n"
        );
        let shaped = shape_text(&text, &limits);
        let cut = "z".repeat(LINE_CHARS - "I (99) app: ".len());
        assert_eq!(
            shaped.text(),
            format!(
                "ESP-ROM:esp32c3-api1-20210207\n\
                 I (24) boot: ESP-IDF v5.5.3\n\
                 ... 3 lines elided ...\n\
                 I (99) app: {cut}...(+12 chars)\n\
                 I (912) pk_app: ready"
            )
        );
        assert_eq!(shaped.lines_total, 7);
        assert_eq!(shaped.entries_total, 5);
    }

    #[test]
    fn a_default_output_never_exceeds_the_text_budget() {
        let lines: Vec<String> = (0..80)
            .map(|i| format!("{i:03}{}", "q".repeat(500)))
            .collect();
        let shaped = shape_text_within(&lines.join("\n"), &ShapeLimits::DEFAULT);
        assert!(shaped.text().chars().count() <= TEXT_BUDGET_CHARS);
        // The head survives: it identifies the image.
        assert!(shaped.head[0].line.starts_with("000"));
        assert!(!shaped.budget_truncated);
    }

    #[test]
    fn a_budget_smaller_than_one_line_still_holds() {
        let limits = ShapeLimits {
            budget_chars: Some(40),
            ..ShapeLimits::DEFAULT
        };
        let lines = numbered(100);
        let shaped = shape_text_within(&lines.join("\n"), &limits);
        assert!(shaped.text().chars().count() <= 40, "{:?}", shaped.text());
    }

    #[test]
    fn the_budget_holds_with_the_line_cap_off() {
        let limits = ShapeLimits {
            line_chars: 0,
            ..ShapeLimits::DEFAULT
        };
        let text = format!("{}\n{}", "a".repeat(5_000), "b".repeat(5_000));
        let shaped = shape_text_within(&text, &limits);
        assert!(
            shaped.text().chars().count() <= TEXT_BUDGET_CHARS,
            "{} characters",
            shaped.text().chars().count()
        );
        assert!(shaped.budget_truncated);
    }

    #[test]
    fn a_budget_below_one_rendered_line_holds_down_to_the_elision_marker() {
        let text = format!("{}\n{}", "a".repeat(5_000), "b".repeat(5_000));
        for budget in [10, 30, 64, 200] {
            let limits = ShapeLimits {
                budget_chars: Some(budget),
                ..ShapeLimits::DEFAULT
            };
            let shaped = shape_text_within(&text, &limits);
            let rendered = shaped.text();
            let count = rendered.chars().count();
            assert!(
                count <= budget.max(shaped.elision_marker().chars().count()),
                "budget {budget}: {count} characters: {rendered:?}"
            );
        }
    }

    #[test]
    fn a_cursor_advances_only_over_whole_lines() {
        let chunk = b"I (24) boot: start\nI (99) boot: part";
        let excerpt = shape_serial(chunk, Cursor(4_096), &[], &ShapeLimits::DEFAULT);
        assert_eq!(excerpt.cursor, Cursor(4_096));
        assert_eq!(excerpt.bytes, 19);
        assert_eq!(excerpt.next_cursor, Cursor(4_115));
        assert_eq!(excerpt.held_bytes, 17);
        assert_eq!(excerpt.to_text(), "I (24) boot: start");
    }

    #[test]
    fn a_chunk_with_no_newline_consumes_nothing() {
        let excerpt = shape_serial(b"partial", Cursor(7), &[], &ShapeLimits::DEFAULT);
        assert_eq!(excerpt.next_cursor, Cursor(7));
        assert_eq!(excerpt.bytes, 0);
        assert_eq!(excerpt.to_text(), "");
        assert_eq!(excerpt.shaped.lines_total, 0);
    }

    #[test]
    fn a_reset_does_not_rewind_the_cursor_and_is_reported_in_range() {
        let marks = [
            ResetMark {
                cursor: Cursor(100),
                vt_us: 1_000,
                reason: "POWERON_RESET".to_string(),
            },
            ResetMark {
                cursor: Cursor(140),
                vt_us: 9_000,
                reason: "RTC_SW_CPU_RESET".to_string(),
            },
        ];
        let excerpt = shape_serial(
            b"after the reset\n",
            Cursor(100),
            &marks,
            &ShapeLimits::DEFAULT,
        );
        assert_eq!(excerpt.next_cursor, Cursor(116));
        assert_eq!(excerpt.resets_in_range.len(), 1);
        assert_eq!(excerpt.resets_in_range[0].reason, "POWERON_RESET");
    }

    #[test]
    fn invalid_utf8_is_replaced_not_refused() {
        let excerpt = shape_serial(b"ok \xff\xfe end\n", Cursor(0), &[], &ShapeLimits::DEFAULT);
        assert!(excerpt.to_text().starts_with("ok "));
        assert!(excerpt.to_text().ends_with(" end"));
        assert_eq!(excerpt.bytes, 10);
    }

    #[test]
    fn the_excerpt_json_has_the_documented_field_names() {
        let excerpt = shape_serial(b"a\nb\n", Cursor(3), &[], &ShapeLimits::DEFAULT);
        let json = excerpt.to_json();
        assert_eq!(json["cursor"], 3);
        assert_eq!(json["next_cursor"], 7);
        assert_eq!(json["bytes"], 4);
        assert_eq!(json["lines_total"], 2);
        assert_eq!(json["lines_shown"], 2);
        assert_eq!(json["elided"], 0);
        assert_eq!(json["head"], serde_json::json!(["a", "b"]));
        assert_eq!(json["tail"], serde_json::json!([]));
    }

    #[test]
    fn unbounded_limits_shape_nothing_away() {
        let lines = numbered(500);
        let shaped = shape_text(&lines.join("\n"), &ShapeLimits::UNBOUNDED);
        assert_eq!(shaped.lines_shown, 500);
        assert!(!shaped.is_elided());
    }
}
