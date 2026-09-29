//! The device boot console: what a line must look like to be kept, and the redaction that drops
//! everything else. Pure text; the reader that opens a port is in `exec`, behind feature `device`.

/// The largest boot console a reader keeps, so a device that logs forever cannot grow host memory.
pub const BOOT_CONSOLE_MAX_BYTES: usize = 64 * 1024;

pub const BOOT_CONSOLE_POLL: core::time::Duration = core::time::Duration::from_millis(20);

/// The IDF line the boot check matches (`CONFIG_APP_RETRIEVE_LEN_ELF_SHA=9`).
pub const ELF_LINE: &str = "ELF file SHA256:";

/// Whether `line` is the ROM's reset banner `rst:0x<hex> (...),boot:0x<hex> (...)`.
///
/// Anchored at the start of the line, so the app being judged cannot print its own pass.
pub fn is_rom_banner(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("rst:0x") else {
        return false;
    };
    if !rest.starts_with(|c: char| c.is_ascii_hexdigit()) {
        return false;
    }
    let Some((_, after)) = rest.split_once(",boot:0x") else {
        return false;
    };
    after.starts_with(|c: char| c.is_ascii_hexdigit())
}

/// Whether `line` is the ROM's `Saved PC:0x<hex>` line of a reboot after a fault. Anchored like
/// [`is_rom_banner`].
pub fn is_saved_pc(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("Saved PC:0x")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_hexdigit()))
}

pub fn has_elf_line(line: &str) -> bool {
    line.contains(ELF_LINE)
}

/// Redacts a device boot console down to the lines the boot check judges.
///
/// A real console may carry values derived from NVS, and there is no `SecretSet` for a real device
/// to redact by value, so this is an allow list of identity-free shapes: the ROM banner,
/// `ESP-ROM:`, `Build:`, `Saved PC:`, and `ELF file SHA256:` cut after its hex prefix. The rest is
/// dropped and counted; MAC-shaped tokens on kept lines are masked as defense in depth.
pub fn redact_boot_console(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut kept = String::new();
    let mut dropped = 0usize;
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\u{0}']).trim_end();
        if line.is_empty() {
            continue;
        }
        let rom = line.starts_with("ESP-ROM:") || line.starts_with("Build:");
        let elf = has_elf_line(line);
        if !(is_rom_banner(line) || rom || is_saved_pc(line) || elf) {
            dropped += 1;
            continue;
        }
        let line = if elf {
            cut_after_elf_prefix(line)
        } else {
            line.to_owned()
        };
        kept.push_str(&mask_mac_shapes(&line));
        kept.push('\n');
    }
    if dropped > 0 {
        kept.push_str(&format!("<{dropped} line(s) of device output omitted>\n"));
    }
    kept
}

pub fn lines_omitted(redacted: &str) -> u64 {
    redacted
        .lines()
        .find_map(|l| {
            l.strip_prefix('<')
                .and_then(|r| r.split_once(" line(s)"))
                .and_then(|(n, _)| n.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Keeps the tag and hex prefix of an ELF line, so nothing an app appended travels with it.
fn cut_after_elf_prefix(line: &str) -> String {
    let (_, rest) = line.split_once(ELF_LINE).unwrap_or(("", ""));
    let prefix: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    format!("{ELF_LINE} {prefix}")
}

/// Replaces every `xx:xx:xx:xx:xx:xx` token with `<MAC>`: no full MAC reaches any output.
///
/// Also used on esptool's own failure output in `rehearse`, which prints `MAC:` on every connect.
pub(crate) fn mask_mac_shapes(line: &str) -> String {
    let bytes: Vec<char> = line.chars().collect();
    let is_mac_at = |i: usize| -> bool {
        if i + 17 > bytes.len() {
            return false;
        }
        if i > 0 && (bytes[i - 1].is_ascii_hexdigit() || bytes[i - 1] == ':') {
            return false;
        }
        if bytes
            .get(i + 17)
            .is_some_and(|c| c.is_ascii_hexdigit() || *c == ':')
        {
            return false;
        }
        (0..6).all(|g| {
            let o = i + g * 3;
            bytes[o].is_ascii_hexdigit()
                && bytes[o + 1].is_ascii_hexdigit()
                && (g == 5 || bytes[o + 2] == ':')
        })
    };
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if is_mac_at(i) {
            out.push_str("<MAC>");
            i += 17;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Whether the console already holds everything the boot check judges, so a reader can stop early.
pub fn boot_console_is_complete(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.lines().any(is_rom_banner) && text.lines().any(has_elf_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device transcript with app lines that must not survive the redaction.
    const TRANSCRIPT: &str = concat!(
        "ESP-ROM:esp32c3-api1-20210207\r\n",
        "Build:Feb  7 2021\r\n",
        "rst:0xc (RTC_SW_CPU_RST),boot:0xa (SPI_FAST_FLASH_BOOT)\r\n",
        "Saved PC:0x4038306c\r\n",
        "I (31) boot: ESP-IDF v5.5.3 2nd stage bootloader\r\n",
        "I (117) app_init: ELF file SHA256:  f5429c0b8...\r\n",
        "I (140) wifi: mac 02:00:00:11:22:33 ready\r\n",
        "I (300) pk_app: device token abcdef0123456789\r\n",
    );

    #[test]
    fn the_boot_console_keeps_only_the_banner_and_the_elf_prefix() {
        let redacted = redact_boot_console(TRANSCRIPT.as_bytes());
        assert_eq!(
            redacted,
            concat!(
                "ESP-ROM:esp32c3-api1-20210207\n",
                "Build:Feb  7 2021\n",
                "rst:0xc (RTC_SW_CPU_RST),boot:0xa (SPI_FAST_FLASH_BOOT)\n",
                "Saved PC:0x4038306c\n",
                "ELF file SHA256: f5429c0b8\n",
                "<3 line(s) of device output omitted>\n",
            )
        );
        assert!(!redacted.contains("pk_app"));
        assert!(!redacted.contains("abcdef0123456789"));
        assert!(!redacted.contains("02:00:00:11:22:33"));
        assert_eq!(lines_omitted(&redacted), 3);
    }

    #[test]
    fn a_mac_on_a_kept_line_is_masked() {
        let text = concat!(
            "rst:0x1 (POWERON),boot:0xa (SPI_FAST_FLASH_BOOT) mac 02:00:00:aa:bb:cc\n",
            "boot: only half a banner\n",
        );
        let redacted = redact_boot_console(text.as_bytes());
        assert_eq!(
            redacted,
            concat!(
                "rst:0x1 (POWERON),boot:0xa (SPI_FAST_FLASH_BOOT) mac <MAC>\n",
                "<1 line(s) of device output omitted>\n",
            )
        );
    }

    #[test]
    fn an_application_line_cannot_forge_the_rom_banner() {
        for forgery in [
            "I (300) pk_app: rst:0x1 (POWERON),boot:0xa (SPI_FAST_FLASH_BOOT)",
            "I (300) pk_app: printing rst:0x and boot:0x for fun",
            "I (300) pk_app: Saved PC:0x4038306c",
            "rst:0xz (NONSENSE),boot:0xa (X)",
            "rst:0x1 (POWERON) boot:0xa (SPI_FAST_FLASH_BOOT)",
        ] {
            assert!(!is_rom_banner(forgery), "{forgery}");
            let redacted = redact_boot_console(forgery.as_bytes());
            assert!(
                !crate::flow::boot_check(&redacted, None).banner,
                "{forgery}: {redacted}"
            );
        }
        // Indented by a stray carriage return or space, the ROM's own banner still counts.
        assert!(is_rom_banner(
            "  rst:0xc (RTC_SW_CPU_RST),boot:0xa (SPI_FAST_FLASH_BOOT)"
        ));
        assert!(is_saved_pc("Saved PC:0x4038306c"));
        assert!(!is_saved_pc("Saved PC: 0x4038306c is what the app says"));
    }

    #[test]
    fn the_window_ends_early_only_on_a_complete_boot() {
        assert!(boot_console_is_complete(TRANSCRIPT.as_bytes()));
        assert!(!boot_console_is_complete(
            b"rst:0x1 (POWERON),boot:0xa (SPI_FAST_FLASH_BOOT)\n"
        ));
        assert!(!boot_console_is_complete(b"ELF file SHA256:  f5429c0b8\n"));
        assert!(!boot_console_is_complete(b""));
    }
}
