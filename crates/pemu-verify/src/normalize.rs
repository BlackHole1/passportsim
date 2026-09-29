//! Console normalizer and identity masks. A console is read as **bytes**, never through a text
//! layer that could rewrite line endings. Four steps, in order:
//!
//! 1. strip CR;
//! 2. rewrite ESP_LOG timestamps to `(T)`, keeping the numbers for the bands of [`crate::bands`];
//! 3. mask compile time and date, app version, ELF SHA, the Passport Keys version, `boot=` ids,
//!    MAC addresses and the `Saved PC:` value, keeping the line;
//! 4. keep the last boot, the text after the final `ESP-ROM:` banner, unless asked for all.
//!
//! Same mask set, placeholders and "level plus message" key as our timing spike's comparison, so
//! [`line_coverage`] reproduces its ratio line. No regex engine: this crate may depend on no
//! third-party crate, and the patterns are fixed enough to scan by hand.

use std::collections::BTreeSet;

/// Which boots of a console the normalizer keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootSelect {
    /// Everything after the final `ESP-ROM:` banner; the whole text when there is no banner.
    LastBoot,
    AllBoots,
}

/// ESP_LOG level letters, in the order `esp_log_level_t` defines them (IDF
/// `esp_log_level.h`). `N` (none) never reaches a line.
pub const LEVELS: &[u8] = b"EWIDV";

/// Placeholder that replaces a masked value, the spike's own.
pub const MASKED: &str = "<masked>";

/// Placeholder that replaces a MAC address.
pub const MASK_MAC: &str = "<MAC>";

/// Placeholder that replaces the `Saved PC:` value.
pub const MASK_PC: &str = "<PC>";

/// A mask that blanks the rest of a line once its marker is seen.
#[derive(Clone, Copy, Debug)]
pub struct TailMask {
    pub name: &'static str,
    /// Marker; everything after it on the line becomes [`MASKED`].
    pub marker: &'static str,
}

/// The tail masks, one per identity- or build-bearing field. Markers are ESP_LOG message text
/// matched against the rewritten line: the bootloader banner, the three `app_init:` app
/// descriptor lines and the firmware version line.
pub const TAIL_MASKS: &[TailMask] = &[
    TailMask {
        name: "boot-compile-time",
        marker: "boot: compile time ",
    },
    TailMask {
        name: "app-version",
        marker: "app_init: App version:",
    },
    TailMask {
        name: "app-compile-time",
        marker: "app_init: Compile time:",
    },
    TailMask {
        name: "app-elf-sha",
        marker: "app_init: ELF file SHA256:",
    },
    TailMask {
        name: "pk-version",
        marker: "main: Passport Keys ",
    },
];

/// One normalized console line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub level: Option<u8>,
    /// ESP_LOG timestamp in milliseconds, kept for the bands.
    pub ts_ms: Option<u32>,
    /// The normalized line: timestamp rewritten to `(T)`, identity values masked.
    pub text: String,
}

impl Line {
    /// Comparison key of the text rule: level and masked message with the timestamp rewritten, so
    /// two runs at different speeds compare equal.
    pub fn key(&self) -> &str {
        &self.text
    }
}

/// A normalized console.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Console {
    pub lines: Vec<Line>,
}

impl Console {
    pub fn timestamped(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter().filter(|line| line.ts_ms.is_some())
    }

    /// The normalized text, one line per entry, with a trailing newline when not empty: the byte
    /// form goldens are compared in.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.text);
            out.push('\n');
        }
        out
    }

    /// First timestamp of a line whose text contains `needle`, for a band anchor or a phase
    /// marker.
    pub fn first_ts(&self, needle: &str) -> Option<u32> {
        self.lines
            .iter()
            .find(|line| line.ts_ms.is_some() && line.text.contains(needle))
            .and_then(|line| line.ts_ms)
    }
}

/// Normalizes a console capture. Invalid UTF-8 is replaced rather than rejected: an oracle console
/// can be cut mid-character when a run is stopped.
pub fn normalize(bytes: &[u8], select: BootSelect) -> Console {
    let text = String::from_utf8_lossy(bytes).replace('\r', "");
    let mut lines: Vec<Line> = text.split('\n').map(normalize_line).collect();
    // `split` on a trailing newline yields one empty tail line; it carries no console content.
    if lines.last().is_some_and(|line| line.text.is_empty()) {
        lines.pop();
    }
    if select == BootSelect::LastBoot
        && let Some(start) = lines
            .iter()
            .rposition(|line| line.text.starts_with("ESP-ROM:"))
    {
        lines.drain(..start);
    }
    Console { lines }
}

/// Steps 2 and 3 for one CR-free line.
fn normalize_line(raw: &str) -> Line {
    let (level, ts_ms, rest) = match split_timestamp(raw) {
        Some((level, ts, rest)) => (Some(level), Some(ts), rest.to_string()),
        None => (None, None, raw.to_string()),
    };
    let masked = mask_values(&rest);
    let text = match level {
        Some(level) => format!("{} (T) {masked}", level as char),
        None => masked,
    };
    Line { level, ts_ms, text }
}

/// Splits an ESP_LOG line `X (NNNN) rest` into its level letter, its millisecond timestamp and
/// the message. Returns `None` for a line the ROM or a raw `printf` wrote.
fn split_timestamp(line: &str) -> Option<(u8, u32, &str)> {
    let bytes = line.as_bytes();
    let level = *bytes.first()?;
    if !LEVELS.contains(&level) || bytes.get(1) != Some(&b' ') || bytes.get(2) != Some(&b'(') {
        return None;
    }
    let close = line[3..].find(')')? + 3;
    let digits = &line[3..close];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ts = digits.parse::<u32>().ok()?;
    let rest = line.get(close + 2..)?;
    if line.as_bytes().get(close + 1) != Some(&b' ') {
        return None;
    }
    Some((level, ts, rest))
}

/// Step 3: the value masks, applied in a fixed order so the result is deterministic.
fn mask_values(line: &str) -> String {
    let line = mask_macs(line);
    let line = mask_after(&line, "Saved PC:", MASK_PC, is_pc_char);
    let line = mask_after(&line, "boot=", MASKED, |b| b.is_ascii_hexdigit());
    for mask in TAIL_MASKS {
        if let Some(head) = line.find(mask.marker) {
            let end = head + mask.marker.len();
            return format!("{}{MASKED}", &line[..end]);
        }
    }
    line
}

/// True for a character of a `Saved PC:0x4000....` value.
fn is_pc_char(b: u8) -> bool {
    b.is_ascii_hexdigit() || b == b'x' || b == b'X'
}

/// Replaces the run of `accept` characters that follows each occurrence of `marker`.
fn mask_after(line: &str, marker: &str, placeholder: &str, accept: fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(marker) {
        let after = at + marker.len();
        out.push_str(&rest[..after]);
        let run = rest.as_bytes()[after..]
            .iter()
            .position(|b| !accept(*b))
            .unwrap_or(rest.len() - after);
        if run == 0 {
            rest = &rest[after..];
            continue;
        }
        out.push_str(placeholder);
        rest = &rest[after + run..];
    }
    out.push_str(rest);
    out
}

/// Replaces every `xx:xx:xx:xx:xx:xx` hexadecimal MAC with [`MASK_MAC`]. Deliberately wide, so
/// no real MAC survives a golden derivation; it also covers the emulator's `02:00:00` MACs.
fn mask_macs(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if is_mac_at(bytes, i) {
            out.push_str(MASK_MAC);
            i += 17;
        } else {
            // A MAC is ASCII, so a byte that starts a multi-byte character never starts one and
            // pushing it back as part of the next `char` keeps the string valid.
            let ch = line[i..].chars().next().expect("index is a char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// True when a 17-byte MAC starts at `i` and is not part of a longer colon-separated run.
fn is_mac_at(bytes: &[u8], i: usize) -> bool {
    if i + 17 > bytes.len() {
        return false;
    }
    for group in 0..6 {
        let at = i + group * 3;
        if !bytes[at].is_ascii_hexdigit() || !bytes[at + 1].is_ascii_hexdigit() {
            return false;
        }
        if group < 5 && bytes[at + 2] != b':' {
            return false;
        }
    }
    let before_ok =
        i == 0 || !matches!(bytes[i - 1], b':' | b'a'..=b'f' | b'A'..=b'F' | b'0'..=b'9');
    let after_ok = bytes
        .get(i + 17)
        .is_none_or(|b| !matches!(b, b':' | b'a'..=b'f' | b'A'..=b'F' | b'0'..=b'9'));
    before_ok && after_ok
}

/// How many normalized timestamped reference lines appear in an emulated console, order ignored
/// (the ordered comparison is [`crate::goldens`]). The spike reports 56 of 67 for esp32sim R8
/// against the device boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCoverage {
    /// Reference lines present in the emulated console.
    pub matched: usize,
    /// Timestamped reference lines.
    pub total: usize,
    /// Timestamped emulated lines, reported beside the ratio.
    pub emulated: usize,
}

/// Computes [`LineCoverage`] for a reference and an emulated console.
pub fn line_coverage(reference: &Console, emulated: &Console) -> LineCoverage {
    let emu: BTreeSet<&str> = emulated.timestamped().map(Line::key).collect();
    let mut matched = 0;
    let mut total = 0;
    for line in reference.timestamped() {
        total += 1;
        if emu.contains(line.key()) {
            matched += 1;
        }
    }
    LineCoverage {
        matched,
        total,
        emulated: emulated.timestamped().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every MAC here is a placeholder (`02:00:00` prefix) and every other value is invented, so
    /// no device fact enters the tree.
    const SYNTHETIC: &str = concat!(
        "ESP-ROM:esp32c3-api1-20210207\r\n",
        "Build:Feb  7 2021\r\n",
        "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (DOWNLOAD(USB/UART0))\r\n",
        "Saved PC:0x4038a0f2\r\n",
        "I (24) boot: ESP-IDF v5.5.3 2nd stage bootloader\r\n",
        "I (24) boot: compile time Jan  1 1970 00:00:00\r\n",
        "I (196) app_init: App version:                   1.2.3-test\r\n",
        "I (197) app_init: Compile time:                  Jan  1 1970 00:00:00\r\n",
        "I (198) app_init: ELF file SHA256:               0011223344556677...\r\n",
        "I (210) main: Passport Keys 9.9.9 starting\r\n",
        "I (211) wifi: sta mac 02:00:00:c3:00:01\r\n",
        "I (212) pk_app: ready: boot=0123abcd\r\n",
        "W (213) bsp_batt: gauge did not ACK\r\n",
    );

    fn norm(text: &str) -> Console {
        normalize(text.as_bytes(), BootSelect::LastBoot)
    }

    #[test]
    fn strips_cr_and_rewrites_timestamps() {
        let console = norm(SYNTHETIC);
        assert!(
            !console.to_text().contains('\r'),
            "step 1 must strip every CR"
        );
        let boot_line = &console.lines[4];
        assert_eq!(boot_line.ts_ms, Some(24));
        assert_eq!(boot_line.level, Some(b'I'));
        assert_eq!(
            boot_line.text,
            "I (T) boot: ESP-IDF v5.5.3 2nd stage bootloader"
        );
    }

    #[test]
    fn masks_a_placeholder_mac() {
        let console = norm(SYNTHETIC);
        let line = console
            .lines
            .iter()
            .find(|l| l.text.contains("sta mac"))
            .expect("mac line kept");
        assert_eq!(line.text, "I (T) wifi: sta mac <MAC>");
    }

    #[test]
    fn masks_a_mac_anywhere_in_the_line() {
        let console = normalize(
            b"I (1) t: a=02:00:00:aa:bb:cc b=02:00:00:DD:EE:FF end\n",
            BootSelect::AllBoots,
        );
        assert_eq!(console.lines[0].text, "I (T) t: a=<MAC> b=<MAC> end");
    }

    #[test]
    fn does_not_mask_a_longer_colon_run_as_a_mac() {
        // Seven hexadecimal pairs are not a MAC; masking the first six would corrupt the line.
        let console = normalize(b"I (1) t: 00:11:22:33:44:55:66\n", BootSelect::AllBoots);
        assert_eq!(console.lines[0].text, "I (T) t: 00:11:22:33:44:55:66");
    }

    #[test]
    fn masks_every_tail_field() {
        let console = norm(SYNTHETIC);
        let texts: Vec<&str> = console.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"I (T) boot: compile time <masked>"));
        assert!(texts.contains(&"I (T) app_init: App version:<masked>"));
        assert!(texts.contains(&"I (T) app_init: Compile time:<masked>"));
        assert!(texts.contains(&"I (T) app_init: ELF file SHA256:<masked>"));
        assert!(texts.contains(&"I (T) main: Passport Keys <masked>"));
    }

    #[test]
    fn masks_the_boot_id_and_the_saved_pc_but_keeps_the_lines() {
        let console = norm(SYNTHETIC);
        let texts: Vec<&str> = console.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"I (T) pk_app: ready: boot=<masked>"));
        assert!(
            texts.contains(&"Saved PC:<PC>"),
            "the Saved PC line stays, only its value is masked: {texts:?}"
        );
    }

    #[test]
    fn selects_the_last_boot() {
        let two = format!("{SYNTHETIC}ESP-ROM:esp32c3-api1-20210207\nI (7) boot: second\n");
        let console = normalize(two.as_bytes(), BootSelect::LastBoot);
        assert_eq!(console.lines.len(), 2);
        assert!(console.lines[0].text.starts_with("ESP-ROM:"));
        assert_eq!(console.lines[1].text, "I (T) boot: second");

        let all = normalize(two.as_bytes(), BootSelect::AllBoots);
        assert_eq!(all.lines.len(), 15);
    }

    #[test]
    fn keeps_a_line_without_a_timestamp_untouched_but_unmarked() {
        let console = normalize(
            b"rst:0x15 (USB_UART_CHIP_RESET),boot:0xa\n",
            BootSelect::AllBoots,
        );
        assert_eq!(console.lines[0].ts_ms, None);
        assert_eq!(console.lines[0].level, None);
        assert_eq!(
            console.lines[0].text,
            "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa"
        );
    }

    #[test]
    fn line_coverage_counts_reference_lines_present_in_the_emulator() {
        let reference = normalize(
            b"I (1) a: one\nI (2) b: two\nI (3) c: three\nnot timestamped\n",
            BootSelect::AllBoots,
        );
        let emulated = normalize(
            b"I (9) c: three\nI (9) a: one\nI (9) d: four\n",
            BootSelect::AllBoots,
        );
        let coverage = line_coverage(&reference, &emulated);
        assert_eq!(coverage.total, 3, "the untimestamped line is not counted");
        assert_eq!(coverage.matched, 2);
        assert_eq!(coverage.emulated, 3);
    }

    #[test]
    fn line_coverage_ignores_timestamps_and_masked_values() {
        let reference = normalize(
            b"I (1) pk_app: ready: boot=deadbeef\n",
            BootSelect::AllBoots,
        );
        let emulated = normalize(
            b"I (999) pk_app: ready: boot=00000001\n",
            BootSelect::AllBoots,
        );
        assert_eq!(line_coverage(&reference, &emulated).matched, 1);
    }

    #[test]
    fn first_ts_finds_a_phase_marker() {
        let console = norm(SYNTHETIC);
        assert_eq!(console.first_ts("boot: ESP-IDF"), Some(24));
        assert_eq!(console.first_ts("no such line"), None);
    }
}
