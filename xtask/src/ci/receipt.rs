//! Tier receipts of `xtask ci`.
//!
//! A receipt is the evidence of one tier run: commit, dirty flag, toolchain versions, host OS,
//! architecture and target triple, and each step's name, status, reason and duration. It is JSON
//! written by hand with a fixed key order, to `<data root>/receipts/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::model::{self, StepResult};

/// Receipt format identifier, bumped when a field or status changes.
pub const SCHEMA: &str = "passportsim/ci-receipt/5";

/// One tier run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub tier: String,
    /// The T0 groups the run was limited to; empty when it ran the whole tier.
    pub groups: Vec<String>,
    /// Where the steps ran: `<os>-<arch>` from `std::env::consts`, such as `macos-aarch64` or
    /// `windows-x86_64`.
    pub leg: String,
    /// `std::env::consts::OS` of the leg.
    pub os: String,
    /// `std::env::consts::ARCH` of the leg.
    pub arch: String,
    /// Target triple the leg built for, from `rustc -vV`.
    pub target: String,
    pub commit: String,
    pub short_commit: String,
    /// Whether `git status --porcelain` listed changes; `None` when git failed.
    pub dirty: Option<bool>,
    pub rustc: String,
    pub cargo: String,
    pub host: String,
    /// Start of the run, seconds since the Unix epoch.
    pub started_unix: u64,
    pub duration: Duration,
    pub steps: Vec<StepResult>,
}

impl Receipt {
    /// The receipt as pretty-printed JSON.
    pub fn to_json(&self) -> String {
        let c = model::counts(&self.steps);
        let fields: Vec<(&str, String)> = vec![
            ("schema", json_string(SCHEMA)),
            ("tier", json_string(&self.tier)),
            (
                "groups",
                format!(
                    "[{}]",
                    self.groups
                        .iter()
                        .map(|g| json_string(g))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            ("leg", json_string(&self.leg)),
            ("os", json_string(&self.os)),
            ("arch", json_string(&self.arch)),
            ("target", json_string(&self.target)),
            ("commit", json_string(&self.commit)),
            ("short_commit", json_string(&self.short_commit)),
            (
                "dirty",
                self.dirty.map_or("null".to_string(), |d| d.to_string()),
            ),
            ("rustc", json_string(&self.rustc)),
            ("cargo", json_string(&self.cargo)),
            ("host", json_string(&self.host)),
            ("started_utc", json_string(&iso_utc(self.started_unix))),
            ("duration_ms", self.duration.as_millis().to_string()),
            ("result", json_string(model::overall(&self.steps).as_str())),
            (
                "counts",
                format!(
                    "{{\"pass\": {}, \"fail\": {}, \"skipped\": {}, \"not_run\": {}, \
                     \"blocked\": {}, \"skipped_corpus\": {}}}",
                    c.pass, c.fail, c.skipped, c.not_run, c.blocked, c.skipped_corpus
                ),
            ),
        ];
        let mut out = String::from("{\n");
        for (key, value) in fields {
            out.push_str(&format!("  {}: {value},\n", json_string(key)));
        }
        out.push_str("  \"steps\": [");
        for (index, step) in self.steps.iter().enumerate() {
            out.push_str(if index == 0 { "\n" } else { ",\n" });
            out.push_str(&format!(
                "    {{\"name\": {}, \"status\": {}, \"reason\": {}, \"duration_ms\": {}}}",
                json_string(&step.name),
                json_string(step.status.as_str()),
                json_string(&step.reason),
                step.duration.as_millis()
            ));
        }
        out.push_str(if self.steps.is_empty() {
            "]\n}\n"
        } else {
            "\n  ]\n}\n"
        });
        out
    }

    /// `<tier>-<short commit>-<UTC stamp>.json` on macOS and
    /// `<tier>-<short commit>-<UTC stamp>-<leg>.json` elsewhere, with `-<group>+<group>` last for a
    /// run of some T0 groups, so receipts gathered from several hosts and jobs into one directory
    /// never collide.
    pub fn file_name(&self) -> String {
        let leg = if self.os == "macos" {
            String::new()
        } else {
            format!("-{}", self.leg)
        };
        let groups = if self.groups.is_empty() {
            String::new()
        } else {
            format!("-{}", self.groups.join("+"))
        };
        format!(
            "{}-{}-{}{leg}{groups}.json",
            self.tier,
            self.short_commit,
            stamp_utc(self.started_unix)
        )
    }

    /// Writes the receipt into `dir`, creating it, and returns the file path.
    pub fn write(&self, dir: &Path) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("cannot create {}: {}", dir.display(), err.kind()))?;
        let path = dir.join(self.file_name());
        std::fs::write(&path, self.to_json())
            .map_err(|err| format!("cannot write {}: {}", path.display(), err.kind()))?;
        Ok(path)
    }
}

/// `text` as a JSON string literal: quotes, backslashes and control characters escaped,
/// everything else (non-ASCII included) kept as UTF-8.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// UTC `(year, month, day, hour, minute, second)` of a Unix time (days-to-civil algorithm).
pub fn utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (
        (rem / 3600) as u32,
        (rem / 60 % 60) as u32,
        (rem % 60) as u32,
    );
    (year, month, day, h, m, s)
}

/// `2026-09-11T12:34:56Z`.
pub fn iso_utc(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `20260911T123456Z`, the file-name form.
pub fn stamp_utc(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}
