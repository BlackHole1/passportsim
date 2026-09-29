//! Golden console tests: kinds, headers, the text rule, golden derivation and the
//! instruction-count comparison.
//!
//! A golden file is **bytes**: a header of `#!` lines no console line can collide with, then the
//! normalized console verbatim. The normalizer strips CR first, so a checkout on either host
//! compares equal.
//!
//! Deriving a `device` golden from the reference boot is a local, macOS-only step: the raw
//! capture never enters the tree, only the masked text does, after `xtask secrets-check` passes
//! on it. [`derive`] reports what it masked, so the operator sees the evidence rather than
//! trusting the mask list.

use std::collections::BTreeMap;

use crate::normalize::{self, BootSelect, Console, LineCoverage, MASK_MAC, TAIL_MASKS};
use crate::spec_toml::Error as SpecError;

/// Where a golden comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The Passport Keys reference boot; fidelity class A, authoritative.
    Device,
    /// A QEMU or esp32sim console for a probe image; class B, authoritative for text on
    /// modeled paths.
    Oracle,
    /// Reviewed emulator output; class B, `provisional` until a device capture exists, and it
    /// cannot promote a block to class A.
    SelfReviewed,
}

impl Kind {
    /// The fidelity class this kind can support.
    pub fn fidelity_class(self) -> char {
        match self {
            Kind::Device => 'A',
            Kind::Oracle | Kind::SelfReviewed => 'B',
        }
    }

    /// True for the kind that is provisional until a device capture exists.
    pub fn is_provisional(self) -> bool {
        self == Kind::SelfReviewed
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Device => "device",
            Kind::Oracle => "oracle",
            Kind::SelfReviewed => "self",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        match text {
            "device" => Some(Kind::Device),
            "oracle" => Some(Kind::Oracle),
            "self" => Some(Kind::SelfReviewed),
            _ => None,
        }
    }
}

/// Prefix of every golden header line. A console line cannot start with it, so the split
/// between header and body needs no escaping and no line count.
pub const HEADER_PREFIX: &str = "#!";

pub const HEADER_MAGIC: &str = "#!pemu-golden v1";

/// A golden header: the command line, the binary, ROM and eFuse hashes, and the strap value
/// read back from the console.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub kind: Option<Kind>,
    /// Which oracle or emulator produced it: `device`, `qemu`, `esp32sim`, `pemu`.
    pub source: String,
    /// Image id, the directory name under `tests/golden/`.
    pub image: String,
    /// The exact command line the regeneration ran.
    pub command: String,
    pub binary_sha256: String,
    pub rom_sha256: String,
    pub efuse_sha256: String,
    /// The strap value read back from the console, as printed.
    pub strap: String,
    /// True while the golden is `self` and no device capture exists.
    pub provisional: bool,
    /// Anything else the run recorded, in key order.
    pub extra: BTreeMap<String, String>,
}

impl Header {
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER_MAGIC);
        out.push('\n');
        let mut put = |key: &str, value: &str| {
            out.push_str(&format!("{HEADER_PREFIX}{key}: {value}\n"));
        };
        if let Some(kind) = self.kind {
            put("kind", kind.name());
        }
        put("source", &self.source);
        put("image", &self.image);
        put("command", &self.command);
        put("binary-sha256", &self.binary_sha256);
        put("rom-sha256", &self.rom_sha256);
        put("efuse-sha256", &self.efuse_sha256);
        put("strap", &self.strap);
        if self.provisional {
            put("provisional", "true");
        }
        for (key, value) in &self.extra {
            put(key, value);
        }
        out
    }

    /// Splits a golden file into its header and its console body.
    pub fn parse(text: &str) -> Result<(Header, &str), SpecError> {
        let mut header = Header::default();
        let mut rest = text;
        let mut line_no = 0;
        let first = text.split('\n').next().unwrap_or_default();
        if first != HEADER_MAGIC {
            return Err(SpecError::new(1, format!("expected `{HEADER_MAGIC}`")));
        }
        while let Some((line, tail)) = rest.split_once('\n') {
            line_no += 1;
            if line_no == 1 {
                rest = tail;
                continue;
            }
            let Some(body) = line.strip_prefix(HEADER_PREFIX) else {
                break;
            };
            rest = tail;
            let (key, value) = body
                .split_once(": ")
                .ok_or_else(|| SpecError::new(line_no, "a header line is `#!key: value`"))?;
            match key {
                "kind" => {
                    header.kind = Some(
                        Kind::parse(value)
                            .ok_or_else(|| SpecError::new(line_no, "unknown golden kind"))?,
                    );
                }
                "source" => header.source = value.to_string(),
                "image" => header.image = value.to_string(),
                "command" => header.command = value.to_string(),
                "binary-sha256" => header.binary_sha256 = value.to_string(),
                "rom-sha256" => header.rom_sha256 = value.to_string(),
                "efuse-sha256" => header.efuse_sha256 = value.to_string(),
                "strap" => header.strap = value.to_string(),
                "provisional" => header.provisional = value == "true",
                other => {
                    header.extra.insert(other.to_string(), value.to_string());
                }
            }
        }
        Ok((header, rest))
    }

    /// Fields a regenerated oracle golden must carry before it is committed.
    pub fn missing_fields(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        for (name, value) in [
            ("source", &self.source),
            ("image", &self.image),
            ("command", &self.command),
            ("binary-sha256", &self.binary_sha256),
            ("rom-sha256", &self.rom_sha256),
            ("efuse-sha256", &self.efuse_sha256),
            ("strap", &self.strap),
        ] {
            if value.trim().is_empty() {
                missing.push(name);
            }
        }
        if self.kind.is_none() {
            missing.push("kind");
        }
        missing
    }
}

/// A golden file: its header and the normalized console it pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Golden {
    pub header: Header,
    /// The console body, exactly as committed.
    pub body: String,
}

impl Golden {
    pub fn parse(bytes: &[u8]) -> Result<Golden, SpecError> {
        let text =
            core::str::from_utf8(bytes).map_err(|_| SpecError::new(1, "a golden is UTF-8 text"))?;
        let (header, body) = Header::parse(text)?;
        Ok(Golden {
            header,
            body: body.to_string(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.header.render();
        out.push_str(&self.body);
        out.into_bytes()
    }

    /// The console lines the golden pins, split exactly as [`Console::to_text`] builds them, so
    /// only the empty tail after the final newline is dropped. An **interior** blank line is kept
    /// (an ESP-IDF boot console has them), or a golden could not match the console it came from.
    pub fn lines(&self) -> Vec<&str> {
        let body = self.body.strip_suffix('\n').unwrap_or(&self.body);
        if body.is_empty() {
            return Vec::new();
        }
        body.split('\n').collect()
    }
}

/// How a prefix comparison failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextMismatch {
    /// The run produced fewer lines than the milestone claims.
    TooShort {
        claimed: usize,
        /// Lines produced.
        produced: usize,
    },
    Line {
        /// 0-based line index.
        at: usize,
        expected: String,
        actual: String,
    },
    /// The golden has fewer lines than the milestone claims.
    OverClaimed {
        claimed: usize,
        /// Lines the golden holds.
        golden: usize,
    },
}

/// Checks the prefix a milestone claims: the normalized lines must be equal in sequence, and any
/// insertion, deletion or change fails. `claimed` is a line count; `None` claims the whole golden.
pub fn check_prefix(
    golden: &Golden,
    produced: &Console,
    claimed: Option<usize>,
) -> Result<usize, TextMismatch> {
    let expected = golden.lines();
    let claimed = claimed.unwrap_or(expected.len());
    if claimed > expected.len() {
        return Err(TextMismatch::OverClaimed {
            claimed,
            golden: expected.len(),
        });
    }
    let actual: Vec<&str> = produced
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    if actual.len() < claimed {
        return Err(TextMismatch::TooShort {
            claimed,
            produced: actual.len(),
        });
    }
    for at in 0..claimed {
        if expected[at] != actual[at] {
            return Err(TextMismatch::Line {
                at,
                expected: expected[at].to_string(),
                actual: actual[at].to_string(),
            });
        }
    }
    Ok(claimed)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DerivationReport {
    pub input_bytes: usize,
    /// Lines kept after the boot selection.
    pub lines_kept: usize,
    /// Of those, lines carrying an ESP_LOG timestamp.
    pub timestamped: usize,
    /// MAC addresses masked.
    pub macs_masked: usize,
    /// Tail masks applied, by mask name.
    pub tails_masked: BTreeMap<&'static str, usize>,
    /// MAC-shaped strings still present after masking. Must be zero; a non-zero value means the
    /// derived text must not be written anywhere, let alone committed.
    pub residual_macs: usize,
}

impl DerivationReport {
    pub fn is_clean(&self) -> bool {
        self.residual_macs == 0
    }
}

/// A derived golden, ready to be written to `$ROOT/goldens/` and, once `xtask secrets-check`
/// passes on it, committed to `tests/golden/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derived {
    pub golden: Golden,
    pub report: DerivationReport,
}

/// Derives a golden from a raw console capture. Pure, so tests use synthetic input and the
/// operator runs it through `cargo xtask oracle goldens --derive`. Only
/// `derived.golden.to_bytes()` enters the tree, after `xtask secrets-check` passes on it.
pub fn derive(raw: &[u8], header: Header, select: BootSelect) -> Derived {
    let before = normalize::normalize(raw, select);
    let mut report = DerivationReport {
        input_bytes: raw.len(),
        lines_kept: before.lines.len(),
        timestamped: before.timestamped().count(),
        ..DerivationReport::default()
    };
    let body = before.to_text();
    report.macs_masked = body.matches(MASK_MAC).count();
    for mask in TAIL_MASKS {
        let hits = body.matches(mask.marker).count();
        if hits > 0 {
            report.tails_masked.insert(mask.name, hits);
        }
    }
    // The mask is not trusted: the derived text is scanned again for the MAC shape, and a
    // non-zero count stops the operator before anything is written.
    report.residual_macs = count_mac_shapes(&body);
    Derived {
        golden: Golden { header, body },
        report,
    }
}

/// Counts strings of the shape `xx:xx:xx:xx:xx:xx`, the derivation's own check of the mask.
fn count_mac_shapes(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i + 17 <= bytes.len() {
        let looks = (0..6).all(|group| {
            let at = i + group * 3;
            bytes[at].is_ascii_hexdigit()
                && bytes[at + 1].is_ascii_hexdigit()
                && (group == 5 || bytes[at + 2] == b':')
        });
        if looks {
            count += 1;
            i += 17;
        } else {
            i += 1;
        }
    }
    count
}

/// Instruction counts at `app_main` compared under `fast` at CPI 1, plus or minus 2 %,
/// informational only: the esp32sim figures for the Passport Keys and official images.
pub const ESP32SIM_APP_MAIN: &[(&str, u64)] = &[("pk", 7_020_000), ("official", 9_940_000)];

pub const COUNT_TOLERANCE_PERCENT: u64 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountComparison {
    pub image: String,
    /// The stored esp32sim count.
    pub reference: u64,
    /// Our count under `fast` at CPI 1.
    pub ours: u64,
    /// The band, in instructions.
    pub allowed: u64,
    pub within: bool,
}

impl core::fmt::Display for CountComparison {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}: app_main at {} instructions, esp32sim {} (+/-{}), {} [informational]",
            self.image,
            self.ours,
            self.reference,
            self.allowed,
            if self.within { "within" } else { "outside" }
        )
    }
}

/// Compares one instruction count. Informational only: a caller reports it and never fails a tier
/// on it.
pub fn compare_instruction_count(image: &str, reference: u64, ours: u64) -> CountComparison {
    let allowed = reference * COUNT_TOLERANCE_PERCENT / 100;
    CountComparison {
        image: image.to_string(),
        reference,
        ours,
        allowed,
        within: reference.abs_diff(ours) <= allowed,
    }
}

pub const COUNTS_MAGIC: &str = "# pemu-verify instruction counts v1";

/// Reads a stored instruction-count file: the magic line, then `<image> <function> <count>` per
/// line. The oracle is a black box, so its counts are recorded by `cargo xtask oracle counts
/// --record` on the oracle host and read back here on any host.
pub fn parse_counts(text: &str) -> Result<BTreeMap<(String, String), u64>, SpecError> {
    let text = text.replace('\r', "");
    let mut lines = text.split('\n').enumerate();
    let (_, first) = lines
        .next()
        .ok_or_else(|| SpecError::new(1, "empty file"))?;
    if first != COUNTS_MAGIC {
        return Err(SpecError::new(1, format!("expected `{COUNTS_MAGIC}`")));
    }
    let mut out = BTreeMap::new();
    for (index, line) in lines {
        let at = index + 1;
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
        let image = next("image")?;
        let function = next("function")?;
        let count = next("count")?
            .replace('_', "")
            .parse::<u64>()
            .map_err(|_| SpecError::new(at, "count is not a number"))?;
        if out.insert((image, function), count).is_some() {
            return Err(SpecError::new(at, "duplicate image and function"));
        }
    }
    Ok(out)
}

/// The timestamped-line comparison of the `timing_calib` spike: both raw captures normalized with
/// [`BootSelect::LastBoot`] and compared by [`normalize::line_coverage`].
///
/// Against the real pair (the preserved reference boot and the esp32sim R8 run log, neither in the
/// tree), `cargo xtask oracle timing-calib` prints
///
/// ```text
/// oracle timing-calib: reference timestamped lines 67, emulator 58, matched 56 of 67
/// ```
///
/// exactly the spike's figures. The test runs a synthetic pair of the same shape.
pub fn timestamped_coverage(reference: &[u8], emulated: &[u8]) -> LineCoverage {
    normalize::line_coverage(
        &normalize::normalize(reference, BootSelect::LastBoot),
        &normalize::normalize(emulated, BootSelect::LastBoot),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic consoles with the shape of the spike's pair: 67 timestamped reference lines,
    /// 58 emulated ones, 56 in common.
    const REFERENCE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/oracle/fixtures/coverage-reference.console"
    ));
    const EMULATED: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/oracle/fixtures/coverage-emulated.console"
    ));

    fn header() -> Header {
        Header {
            kind: Some(Kind::Oracle),
            source: "qemu".into(),
            image: "probe-boot-facts".into(),
            command: "qemu-c3 -M esp32c3 -icount shift=0,align=off,sleep=off".into(),
            binary_sha256: "0".repeat(64),
            rom_sha256: "1".repeat(64),
            efuse_sha256: "2".repeat(64),
            strap: "0x0a".into(),
            provisional: false,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn golden_kinds_carry_the_arch_classes() {
        assert_eq!(Kind::Device.fidelity_class(), 'A');
        assert_eq!(Kind::Oracle.fidelity_class(), 'B');
        assert_eq!(Kind::SelfReviewed.fidelity_class(), 'B');
        assert!(Kind::SelfReviewed.is_provisional());
        assert!(!Kind::Device.is_provisional());
    }

    #[test]
    fn a_header_round_trips_and_never_swallows_a_console_line() {
        let golden = Golden {
            header: header(),
            body: "ESP-ROM:esp32c3-api1-20210207\nI (T) boot: ESP-IDF v5.5.3\n".to_string(),
        };
        let bytes = golden.to_bytes();
        let read = Golden::parse(&bytes).expect("round trips");
        assert_eq!(read, golden);
        assert_eq!(read.lines().len(), 2);
        assert!(read.header.missing_fields().is_empty());
    }

    #[test]
    fn a_header_missing_a_regeneration_field_is_named() {
        let mut header = header();
        header.strap = String::new();
        header.rom_sha256 = String::new();
        assert_eq!(header.missing_fields(), vec!["rom-sha256", "strap"]);
    }

    #[test]
    fn a_file_without_the_magic_line_is_refused() {
        assert!(Golden::parse(b"I (T) boot: ESP-IDF\n").is_err());
        assert!(Header::parse("#!pemu-golden v1\n#!broken\n").is_err());
    }

    #[test]
    fn the_text_rule_accepts_the_claimed_prefix_and_ignores_what_follows() {
        let golden = Golden {
            header: header(),
            body: "a\nb\nc\n".to_string(),
        };
        let produced = normalize::normalize(b"a\nb\nZ\n", BootSelect::AllBoots);
        assert_eq!(check_prefix(&golden, &produced, Some(2)), Ok(2));
        assert_eq!(
            check_prefix(&golden, &produced, Some(3)),
            Err(TextMismatch::Line {
                at: 2,
                expected: "c".into(),
                actual: "Z".into()
            })
        );
    }

    #[test]
    fn the_text_rule_fails_an_insertion_and_a_deletion() {
        let golden = Golden {
            header: header(),
            body: "a\nb\nc\n".to_string(),
        };
        let inserted = normalize::normalize(b"a\nX\nb\nc\n", BootSelect::AllBoots);
        assert!(matches!(
            check_prefix(&golden, &inserted, None),
            Err(TextMismatch::Line { at: 1, .. })
        ));
        let deleted = normalize::normalize(b"a\nc\nd\n", BootSelect::AllBoots);
        assert!(matches!(
            check_prefix(&golden, &deleted, None),
            Err(TextMismatch::Line { at: 1, .. })
        ));
        let short = normalize::normalize(b"a\nb\n", BootSelect::AllBoots);
        assert_eq!(
            check_prefix(&golden, &short, None),
            Err(TextMismatch::TooShort {
                claimed: 3,
                produced: 2
            })
        );
    }

    #[test]
    fn a_golden_with_a_blank_console_line_matches_the_console_it_came_from() {
        // An *equal* run must pass. A boot console contains blank lines (the ROM prints one before
        // the bootloader banner), so the golden has to keep them.
        let raw: &[u8] = b"ESP-ROM:esp32c3-api1-20210207\nI (1) a: one\n\nI (2) b: two\n";
        let console = normalize::normalize(raw, BootSelect::LastBoot);
        let derived = derive(raw, header(), BootSelect::LastBoot);
        assert_eq!(
            derived.golden.lines(),
            vec![
                "ESP-ROM:esp32c3-api1-20210207",
                "I (T) a: one",
                "",
                "I (T) b: two"
            ],
            "the blank line is a console line, not a separator"
        );
        assert_eq!(check_prefix(&derived.golden, &console, None), Ok(4));
        let read = Golden::parse(&derived.golden.to_bytes()).expect("parses");
        assert_eq!(check_prefix(&read, &console, None), Ok(4));
        // A run that drops the blank line is still a deletion and still fails.
        let without = normalize::normalize(
            b"ESP-ROM:esp32c3-api1-20210207\nI (1) a: one\nI (2) b: two\n",
            BootSelect::LastBoot,
        );
        assert!(matches!(
            check_prefix(&read, &without, None),
            Err(TextMismatch::TooShort { .. }) | Err(TextMismatch::Line { at: 2, .. })
        ));
    }

    #[test]
    fn a_milestone_cannot_claim_more_lines_than_the_golden_holds() {
        let golden = Golden {
            header: header(),
            body: "a\nb\n".to_string(),
        };
        let produced = normalize::normalize(b"a\nb\nc\n", BootSelect::AllBoots);
        assert_eq!(
            check_prefix(&golden, &produced, Some(3)),
            Err(TextMismatch::OverClaimed {
                claimed: 3,
                golden: 2
            })
        );
    }

    #[test]
    fn t_timing_calib_line_coverage_has_the_spike_shape() {
        // The real run (67 / 58 / 56) is in the `timestamped_coverage` doc; the device capture
        // never enters the tree, so this pins the arithmetic on a synthetic pair.
        let coverage = timestamped_coverage(REFERENCE, EMULATED);
        assert_eq!(coverage.total, 67, "reference timestamped lines");
        assert_eq!(coverage.emulated, 58, "emulated timestamped lines");
        assert_eq!(
            coverage.matched, 56,
            "reference lines present in the emulator"
        );
    }

    #[test]
    fn line_coverage_ignores_the_boots_before_the_last_one() {
        let mut twice = REFERENCE.to_vec();
        twice.extend_from_slice(REFERENCE);
        assert_eq!(timestamped_coverage(&twice, EMULATED).total, 67);
    }

    #[test]
    fn derivation_masks_every_identity_and_reports_what_it_did() {
        let derived = derive(REFERENCE, header(), BootSelect::LastBoot);
        assert!(derived.report.is_clean(), "{:?}", derived.report);
        assert_eq!(derived.report.residual_macs, 0);
        assert_eq!(derived.report.macs_masked, 1);
        assert_eq!(derived.report.timestamped, 67);
        assert!(derived.report.input_bytes > 0);
        assert!(derived.report.tails_masked.contains_key("app-version"));
        assert!(derived.report.tails_masked.contains_key("pk-version"));
        let body = &derived.golden.body;
        assert!(
            !body.contains("02:00:00:c3:00:01"),
            "a MAC survived masking"
        );
        assert!(
            !body.contains("1.0.0-synthetic"),
            "a version survived masking"
        );
        assert!(body.contains("Saved PC:<PC>"), "the Saved PC line is kept");
        assert!(body.contains("I (T) boot: compile time <masked>"));
    }

    #[test]
    fn a_derived_golden_is_a_readable_golden_file() {
        let derived = derive(REFERENCE, header(), BootSelect::LastBoot);
        let bytes = derived.golden.to_bytes();
        let read = Golden::parse(&bytes).expect("a derived golden parses");
        assert_eq!(read.header.strap, "0x0a");
        assert_eq!(read.body, derived.golden.body);
        assert!(!read.body.contains('\r'), "CR never reaches a golden");
    }

    #[test]
    fn instruction_counts_use_the_arch_values_and_band() {
        let map: BTreeMap<&str, u64> = ESP32SIM_APP_MAIN.iter().copied().collect();
        assert_eq!(map["pk"], 7_020_000);
        assert_eq!(map["official"], 9_940_000);
        // 2 % of 7.02 M is 140,400 instructions.
        let inside = compare_instruction_count("pk", 7_020_000, 7_150_000);
        assert!(inside.within, "{inside}");
        assert_eq!(inside.allowed, 140_400);
        let outside = compare_instruction_count("pk", 7_020_000, 7_200_000);
        assert!(!outside.within, "{outside}");
        assert!(outside.to_string().contains("[informational]"));
    }

    #[test]
    fn a_stored_count_file_is_read_back() {
        let text = concat!(
            "# pemu-verify instruction counts v1\n",
            "# recorded by `cargo xtask oracle counts --record` on the oracle host\n",
            "pk app_main 7_020_000\n",
            "official app_main 9940000\n",
        );
        let counts = parse_counts(text).expect("parses");
        assert_eq!(
            counts[&("pk".to_string(), "app_main".to_string())],
            7_020_000
        );
        assert_eq!(
            counts[&("official".to_string(), "app_main".to_string())],
            9_940_000
        );
        let comparison = compare_instruction_count(
            "pk",
            counts[&("pk".to_string(), "app_main".to_string())],
            7_000_000,
        );
        assert!(comparison.within);
        assert!(parse_counts("pk app_main 1\n").is_err());
    }
}
