//! Runs the pattern rules over a list of repository files and renders the report.
//!
//! The report names the rule, the file and, where a rule locates its match, the byte offset.
//! It never contains matched content, and a file whose name tripped `backup-name` appears only
//! with its name withheld on every line.

use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::Path;

use super::{nvs, patterns, rom};

/// One pattern rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    MacShape,
    EfuseDump,
    NvsCredential,
    CardidWindow,
    BackupName,
    RomPin,
}

impl Rule {
    /// Every rule, in report order.
    pub const ALL: [Rule; 6] = [
        Rule::MacShape,
        Rule::EfuseDump,
        Rule::NvsCredential,
        Rule::CardidWindow,
        Rule::BackupName,
        Rule::RomPin,
    ];

    /// The rule name printed in reports.
    pub fn name(self) -> &'static str {
        match self {
            Rule::MacShape => "mac-shape",
            Rule::EfuseDump => "efuse-dump",
            Rule::NvsCredential => "nvs-credential",
            Rule::CardidWindow => "cardid-window",
            Rule::BackupName => "backup-name",
            Rule::RomPin => "rom-pin",
        }
    }
}

/// One rule firing on one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: Rule,
    /// The printable path: repository-relative, or withheld (see module docs).
    pub path: String,
    /// Byte offset of the match, for rules that locate one.
    pub offset: Option<usize>,
    /// A fixed reason or a count; never file content.
    pub note: Option<String>,
}

/// Outcome of one scan.
#[derive(Debug, Default)]
pub struct Report {
    /// Regular files read and checked.
    pub files_scanned: usize,
    /// Listed paths that are missing, symlinks or not regular files.
    pub files_skipped: usize,
    /// Binaries under `assets/rom/` accepted by the pin.
    pub roms_pinned: usize,
    pub hits: Vec<Hit>,
}

/// Printed hit lines per file and rule; the rest is summarized as a count.
const MAX_LINES_PER_FILE_RULE: usize = 20;

impl Report {
    /// Number of hits of one rule.
    pub fn count(&self, rule: Rule) -> usize {
        self.hits.iter().filter(|hit| hit.rule == rule).count()
    }

    /// Hit lines followed by one summary line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < self.hits.len() {
            let first = &self.hits[i];
            let run = self.hits[i..]
                .iter()
                .take_while(|hit| hit.rule == first.rule && hit.path == first.path)
                .count();
            for hit in &self.hits[i..i + run.min(MAX_LINES_PER_FILE_RULE)] {
                let _ = write!(out, "secrets-check: {} {}", hit.rule.name(), hit.path);
                if let Some(offset) = hit.offset {
                    let _ = write!(out, ":0x{offset:x}");
                }
                if let Some(note) = &hit.note {
                    let _ = write!(out, " ({note})");
                }
                out.push('\n');
            }
            if run > MAX_LINES_PER_FILE_RULE {
                let more = run - MAX_LINES_PER_FILE_RULE;
                let _ = writeln!(
                    out,
                    "secrets-check: {} {} ({more} more)",
                    first.rule.name(),
                    first.path
                );
            }
            i += run;
        }
        let counts: Vec<String> = Rule::ALL
            .iter()
            .map(|&rule| format!("{} {}", rule.name(), self.count(rule)))
            .collect();
        let _ = writeln!(
            out,
            "secrets-check: pattern rules: {} file(s) scanned, {} skipped, {} pinned ROM ELF(s); {} hit(s): {}",
            self.files_scanned,
            self.files_skipped,
            self.roms_pinned,
            self.hits.len(),
            counts.join(", ")
        );
        out
    }
}

/// Scans `rel_paths` (repository-relative and `/`-separated, or absolute) under `root`.
///
/// Missing paths (deleted in the work tree but still listed by git), symlinks and other
/// non-regular files are skipped for content but still checked by name. Any other I/O error
/// fails the scan, so the guard fails closed.
pub fn scan_files(root: &Path, rel_paths: &[String]) -> Result<Report, String> {
    let rom_dir = rom::RomDir::load(root);
    let mut report = Report::default();
    for rel in rel_paths {
        scan_one(root, rel, &rom_dir, &mut report)?;
    }
    Ok(report)
}

fn scan_one(
    root: &Path,
    rel: &str,
    rom_dir: &rom::RomDir,
    report: &mut Report,
) -> Result<(), String> {
    let name_hit = patterns::backup_name(rel);
    let shown = if name_hit {
        patterns::withheld_path(rel)
    } else {
        rel.to_string()
    };
    if name_hit {
        add(report, Rule::BackupName, &shown, None, None);
    }
    let full = root.join(rel);
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            report.files_skipped += 1;
            return Ok(());
        }
        Err(err) => return Err(format!("cannot inspect {shown}: {}", err.kind())),
    };
    if !meta.is_file() {
        report.files_skipped += 1;
        return Ok(());
    }
    let bytes =
        std::fs::read(&full).map_err(|err| format!("cannot read {shown}: {}", err.kind()))?;
    report.files_scanned += 1;
    let text = patterns::is_text(&bytes);

    if rom::is_under_rom_dir(rel) && !text {
        match rom_dir.check(&bytes) {
            Ok(()) => {
                report.roms_pinned += 1;
                return Ok(());
            }
            Err(failure) => add(
                report,
                Rule::RomPin,
                &shown,
                None,
                Some(failure.note().to_string()),
            ),
        }
    }

    for offset in patterns::mac_shape_offsets(&bytes) {
        add(report, Rule::MacShape, &shown, Some(offset), None);
    }
    if !text {
        if patterns::efuse_dump_shape(&bytes) {
            add(
                report,
                Rule::EfuseDump,
                &shown,
                None,
                Some(format!("{} bytes", bytes.len())),
            );
        }
        for offset in nvs::credential_entries(&bytes) {
            add(report, Rule::NvsCredential, &shown, Some(offset), None);
        }
        if let Some((offset, count)) = patterns::cardid_window(&bytes) {
            let note = format!("{count} non-0xFF byte(s) in the cardid window");
            add(report, Rule::CardidWindow, &shown, Some(offset), Some(note));
        }
    }
    Ok(())
}

fn add(report: &mut Report, rule: Rule, path: &str, offset: Option<usize>, note: Option<String>) {
    report.hits.push(Hit {
        rule,
        path: path.to_string(),
        offset,
        note,
    });
}
