//! The machine-readable probe console line format.
//!
//! The firmware side of this contract is `probes/common/probe_line.h`, which states the grammar
//! in prose; this module is the reader. Both must agree on [`SCHEMA`], which every probe declares
//! on its first line.
//!
//! A probe console is ordinary IDF output with probe lines mixed in. A line is a probe line when
//! it begins with a tag (`[A-Z][A-Z0-9_]{0,15}`) followed immediately by `|`; anything else, ROM
//! banner and `I (123) tag: ...` included, is not one and is skipped. Once a line is recognised
//! as a probe line it must parse completely, so a typo in a firmware format string is an error
//! rather than a silently dropped fact.
//!
//! **Except when the chip reset mid-line.** `probe_reset` causes 24 resets on purpose, three of
//! which (the TWDT panic, the IWDT spin and the RWDT spin) land while `printf` output is still
//! draining out of the USB Serial/JTAG FIFO, so a boot's last line routinely stops in the middle
//! of a segment. A line that the capture itself shows was cut short, because the text ends
//! without a newline or because a ROM banner follows it immediately, is counted as truncated and
//! skipped; a complete but malformed line is still an error.

/// Schema every probe declares on its `PROBE` line. Equals `PROBE_LINE_SCHEMA` of
/// `probes/common/probe_line.h`.
pub const SCHEMA: &str = "passport-emu/probe-line/1";

/// Longest tag the grammar allows.
const MAX_TAG: usize = 16;

/// Longest field key the grammar allows.
const MAX_KEY: usize = 32;

/// One parsed probe line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeLine<'a> {
    /// The tag, without the separator (`HEAP`, `BOOT`, `DONE`).
    pub tag: &'a str,
    /// Segments before the first `key=value`, in order (`HEAP|internal_8bit|free=..`).
    pub positional: Vec<&'a str>,
    /// The `key=value` segments, in order. Keys may repeat; readers take the first.
    pub fields: Vec<(&'a str, &'a str)>,
}

impl<'a> ProbeLine<'a> {
    /// Parses one console line.
    ///
    /// `Ok(None)` means the line is not a probe line at all. `Err` means it looked like one (a
    /// valid tag and a separator) but broke the grammar.
    pub fn parse(line: &'a str) -> Result<Option<ProbeLine<'a>>, String> {
        // Console captures arrive with CRLF from the device and the oracle alike.
        let line = line.trim_end_matches(['\r', '\n']);
        let Some((tag, rest)) = line.split_once('|') else {
            return Ok(None);
        };
        if !is_tag(tag) {
            return Ok(None);
        }
        if rest.is_empty() {
            return Err(format!("probe line `{tag}` has no segments"));
        }
        if rest.ends_with('|') {
            return Err(format!("probe line `{tag}` ends with a separator"));
        }
        let mut positional = Vec::new();
        let mut fields = Vec::new();
        for segment in rest.split('|') {
            match segment.split_once('=') {
                Some((key, value)) => {
                    if !is_key(key) {
                        return Err(format!("probe line `{tag}` has the bad field key `{key}`"));
                    }
                    if !is_value(value) {
                        return Err(format!(
                            "probe line `{tag}` field `{key}` has a value outside printable ASCII"
                        ));
                    }
                    fields.push((key, value));
                }
                None => {
                    if !fields.is_empty() {
                        return Err(format!(
                            "probe line `{tag}` has the positional segment `{segment}` after a \
                             field"
                        ));
                    }
                    if !is_value(segment) {
                        return Err(format!(
                            "probe line `{tag}` has a segment outside printable ASCII"
                        ));
                    }
                    positional.push(segment);
                }
            }
        }
        Ok(Some(ProbeLine {
            tag,
            positional,
            fields,
        }))
    }

    /// The first positional segment, which by convention labels the line (`HEAP|after_init|..`,
    /// `SENS|stage=..` has none). It is what the earlier prototype probes put in the stage slot.
    pub fn label(&self) -> Option<&'a str> {
        self.positional.first().copied()
    }

    /// The first value of `key`, if the line carries it.
    pub fn field(&self, key: &str) -> Option<&'a str> {
        self.fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    }
}

/// What a ROM banner line starts with. A probe line immediately followed by one was cut short by
/// a reset, because the ROM prints this first thing on every boot.
const ROM_BANNER: &str = "ESP-ROM:";

/// The probe lines of a console capture, and what the capture could not hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Console<'a> {
    /// Every probe line, in order. Non-probe lines are skipped.
    pub lines: Vec<ProbeLine<'a>>,
    /// Probe lines a reset cut short, which are skipped rather than parsed.
    pub truncated: usize,
}

/// Reads a console capture.
///
/// A probe line that broke the grammar is an error, unless the capture shows it was cut short by
/// a reset (see the module comment), in which case it is counted in [`Console::truncated`].
pub fn parse_console(text: &str) -> Result<Console<'_>, String> {
    let mut out = Console::default();
    // `split_inclusive` keeps the line terminator, so a final line with no newline of its own is
    // visible as such: that is one of the two marks of a line a reset interrupted.
    let raw: Vec<&str> = text.split_inclusive('\n').collect();
    for (index, source) in raw.iter().enumerate() {
        let line = source.trim_end_matches(['\r', '\n']);
        match ProbeLine::parse(line) {
            Ok(Some(parsed)) => out.lines.push(parsed),
            Ok(None) => {}
            Err(err) => {
                let unterminated = !source.ends_with('\n');
                let banner_follows = raw
                    .get(index + 1)
                    .is_some_and(|next| next.starts_with(ROM_BANNER));
                if unterminated || banner_follows {
                    out.truncated += 1;
                    continue;
                }
                return Err(format!("line {}: {err}", index + 1));
            }
        }
    }
    Ok(out)
}

/// What a completed probe run looks like from the outside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// `name` of the `PROBE` header line.
    pub name: String,
    /// `status` of the `DONE` footer line.
    pub status: String,
    /// `what` of every `FAIL` line, in order.
    pub failures: Vec<String>,
    /// Probe lines that are neither header, footer, `NOTE` nor `FAIL`.
    pub facts: usize,
    /// Distinct labels of the fact lines, in the order they first appeared: the stages the probe
    /// reached. A capture that stops early is visible as a missing stage.
    pub stages: Vec<String>,
    /// Probe lines a reset cut short. Not a failure; see the module comment.
    pub truncated: usize,
}

impl Run {
    /// Reads a console capture as one probe run: a `PROBE` header declaring [`SCHEMA`], some
    /// facts, and a `DONE` footer.
    ///
    /// `probe_reset` restarts on purpose and prints one header per boot, so a capture may hold
    /// several headers; the first names the run and the last `DONE` closes it.
    pub fn read(text: &str) -> Result<Run, String> {
        let console = parse_console(text)?;
        let lines = &console.lines;
        let header = lines
            .iter()
            .find(|line| line.tag == "PROBE")
            .ok_or_else(|| "the capture has no PROBE header line".to_string())?;
        let name = header
            .field("name")
            .ok_or_else(|| "the PROBE header line has no `name`".to_string())?;
        let schema = header
            .field("schema")
            .ok_or_else(|| "the PROBE header line has no `schema`".to_string())?;
        if schema != SCHEMA {
            return Err(format!(
                "the capture declares schema `{schema}`, not `{SCHEMA}`"
            ));
        }
        let footer = lines
            .iter()
            .rfind(|line| line.tag == "DONE")
            .ok_or_else(|| "the capture has no DONE footer line".to_string())?;
        if footer.field("name") != Some(name) {
            return Err(
                "the DONE footer names a different probe than the PROBE header".to_string(),
            );
        }
        let status = footer
            .field("status")
            .ok_or_else(|| "the DONE footer line has no `status`".to_string())?;
        let failures: Vec<String> = lines
            .iter()
            .filter(|line| line.tag == "FAIL")
            .map(|line| line.field("what").unwrap_or_default().to_string())
            .collect();
        let fact_lines = lines
            .iter()
            .filter(|line| !matches!(line.tag, "PROBE" | "DONE" | "NOTE" | "FAIL"));
        let mut facts = 0;
        let mut stages: Vec<String> = Vec::new();
        for line in fact_lines {
            facts += 1;
            // A stage is named either by the first positional segment or by a `stage` field;
            // probes use whichever reads better on the line, so both count here.
            if let Some(stage) = line.label().or_else(|| line.field("stage"))
                && !stages.iter().any(|seen| seen == stage)
            {
                stages.push(stage.to_string());
            }
        }
        Ok(Run {
            name: name.to_string(),
            status: status.to_string(),
            failures,
            facts,
            stages,
            truncated: console.truncated,
        })
    }

    /// Whether the run finished with every step passing.
    pub fn passed(&self) -> bool {
        self.status == "ok" && self.failures.is_empty()
    }
}

fn is_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_uppercase())
        && tag.len() <= MAX_TAG
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && key.len() <= MAX_KEY
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_value(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii() && (' '..='~').contains(&c) && c != '|')
}
