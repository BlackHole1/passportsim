//! Byte and name patterns of the pattern rules `mac-shape`, `efuse-dump`, `cardid-window` and
//! `backup-name`. The NVS rule lives in `nvs.rs`.
//!
//! Every function is pure over bytes or a path string, so the rules are unit-testable without
//! a file system and shared by every scan mode.

use std::ops::Range;

/// A file is text when it has no NUL byte and is valid UTF-8 (the usual git heuristic,
/// tightened by the UTF-8 check). Binary-only rules skip text files.
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

// ---------------------------------------------------------------------------------------------
// mac-shape
// ---------------------------------------------------------------------------------------------

/// Length of the colon form `02:00:00:dd:ee:ff`.
const MAC_TEXT_LEN: usize = 17;

/// Byte offsets of MAC-shaped strings that are not allowed placeholders.
///
/// A MAC shape is exactly six two-digit hex groups joined by one separator, either all `:` or
/// all `-`, in either letter case. The rule scans every file as bytes (text files are the
/// target; in binary files the 17-byte shape occurs by chance far less than once per
/// gigabyte), and handles hex dumps of code as follows so ordinary constants do not hit:
///
/// - Boundaries: the shape must not touch a letter, digit or `_` on either side, and it must
///   not continue a longer run of the same separator, so fingerprints such as
///   `aa:bb:cc:dd:ee:ff:00:11`, IPv6 text and `xxd` offsets (`00000010:`) are not MACs.
/// - Only the separated forms are shapes. Byte arrays (`[0x24, 0x0a, ...]`), space-separated
///   dumps (`24 0a c4 ...`) and bare 12-digit hex are ordinary hex constants for this rule;
///   device values in those forms are caught by value by the hashed rules, which cover bare
///   hex and byte-reversed forms of the secret set.
///
/// Allowed shapes: the `02:00:00` placeholder prefix, the all-zero address, and group
/// (multicast or broadcast) addresses, whose first octet has bit 0 set: a device station,
/// soft-AP, BT or Ethernet address is always unicast.
pub fn mac_shape_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + MAC_TEXT_LEN <= bytes.len() {
        match mac_at(bytes, i) {
            Some(octets) => {
                if !is_allowed_mac(octets) {
                    out.push(i);
                }
                i += MAC_TEXT_LEN;
            }
            None => i += 1,
        }
    }
    out
}

/// The MAC octets when a bounded MAC shape starts at `i`.
fn mac_at(bytes: &[u8], i: usize) -> Option<[u8; 6]> {
    let sep = *bytes.get(i + 2)?;
    if sep != b':' && sep != b'-' {
        return None;
    }
    let mut octets = [0u8; 6];
    for (k, octet) in octets.iter_mut().enumerate() {
        let at = i + 3 * k;
        *octet = hex_pair(bytes, at)?;
        if k < 5 && bytes.get(at + 2) != Some(&sep) {
            return None;
        }
    }
    // Left boundary: no word character, and no complete group of the same run before it.
    if i > 0 {
        let prev = bytes[i - 1];
        if is_word(prev) {
            return None;
        }
        if prev == sep
            && i >= 3
            && hex_pair(bytes, i - 3).is_some()
            && (i == 3 || !is_word(bytes[i - 4]))
        {
            return None;
        }
    }
    // Right boundary: the same checks after the sixth group.
    let end = i + MAC_TEXT_LEN;
    if let Some(&next) = bytes.get(end) {
        if is_word(next) {
            return None;
        }
        if next == sep
            && hex_pair(bytes, end + 1).is_some()
            && bytes.get(end + 3).is_none_or(|&b| !is_word(b))
        {
            return None;
        }
    }
    Some(octets)
}

/// Placeholder prefix, all-zero, or group address (see [`mac_shape_offsets`]).
pub fn is_allowed_mac(octets: [u8; 6]) -> bool {
    octets[..3] == [0x02, 0x00, 0x00] || octets == [0; 6] || octets[0] & 0x01 == 0x01
}

fn hex_pair(bytes: &[u8], at: usize) -> Option<u8> {
    let hi = hex_digit(*bytes.get(at)?)?;
    let lo = hex_digit(*bytes.get(at + 1)?)?;
    Some(hi << 4 | lo)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---------------------------------------------------------------------------------------------
// efuse-dump
// ---------------------------------------------------------------------------------------------

/// Size of a raw BLK0 or BLK1 dump (6 little-endian words, as espefuse writes them).
pub const EFUSE_SHORT_BLOCK_LEN: usize = 24;
/// Size of a raw BLK2 to BLK10 dump (8 little-endian words).
pub const EFUSE_BLOCK_LEN: usize = 32;
/// Size of all eleven blocks written as one file: 2 short blocks and 9 full blocks.
pub const EFUSE_JOINT_LEN: usize = 2 * EFUSE_SHORT_BLOCK_LEN + 9 * EFUSE_BLOCK_LEN;

/// Whether a binary file has the size and structure of a raw eFuse dump.
///
/// Heuristics (size first, because a dump carries no magic):
///
/// - a 24-byte or 32-byte file (one block) that is not a uniform fill (all 0x00 reads as
///   unprogrammed, all 0xFF as erased: neither carries identity) and has no known container
///   magic (ELF, PNG, GIF, JPEG, gzip, zip, zstd, wasm, PDF);
/// - a 336-byte file (all blocks joined) without container magic whose BLK1 MAC field
///   (bytes 24 to 29; BLK1 bits 0 to 47, most significant octet at bit 40, per the ESP-IDF
///   v5.5.3 `esp_efuse_table.csv`) is a non-zero unicast address.
///
/// The caller applies this only to binary files; eFuse dumps are never valid NUL-free UTF-8
/// in practice. Tiny binary fixtures of exactly these sizes must be generated at test time
/// instead of committed.
pub fn efuse_dump_shape(bytes: &[u8]) -> bool {
    if has_container_magic(bytes) {
        return false;
    }
    match bytes.len() {
        EFUSE_SHORT_BLOCK_LEN | EFUSE_BLOCK_LEN => !is_uniform(bytes),
        EFUSE_JOINT_LEN => {
            let mac = &bytes[EFUSE_SHORT_BLOCK_LEN..EFUSE_SHORT_BLOCK_LEN + 6];
            let first_octet = mac[5];
            mac.iter().any(|&b| b != 0) && first_octet & 0x01 == 0
        }
        _ => false,
    }
}

fn is_uniform(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0x00) || bytes.iter().all(|&b| b == 0xFF)
}

fn has_container_magic(bytes: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"\x7fELF",
        b"\x89PNG",
        b"GIF8",
        b"\xff\xd8\xff",
        b"\x1f\x8b",
        b"PK\x03\x04",
        b"\x28\xb5\x2f\xfd",
        b"\0asm",
        b"%PDF",
    ];
    MAGICS.iter().any(|magic| bytes.starts_with(magic))
}

// ---------------------------------------------------------------------------------------------
// cardid-window
// ---------------------------------------------------------------------------------------------

/// The cardid window of the 8 MB flash image.
pub const CARDID_WINDOW: Range<usize> = 0x35_6000..0x35_A000;

/// For a binary at least `CARDID_WINDOW.end` bytes long: the file offset of the first
/// non-0xFF byte in the cardid window and the number of non-0xFF bytes there. An erased
/// window, or a file too short to hold the window, passes (`None`).
pub fn cardid_window(bytes: &[u8]) -> Option<(usize, usize)> {
    let window = bytes.get(CARDID_WINDOW)?;
    let first = window.iter().position(|&b| b != 0xFF)?;
    let count = window.iter().filter(|&&b| b != 0xFF).count();
    Some((CARDID_WINDOW.start + first, count))
}

// ---------------------------------------------------------------------------------------------
// executable structure (the `xtask package` exception to cardid-window)
// ---------------------------------------------------------------------------------------------

/// Whether `bytes` are a structurally valid native executable or wasm module, not merely a file
/// that starts with a magic.
///
/// Used by exactly one caller, the scan of the binary `xtask package` has just built
/// (`crate::package::guard`), to exempt that one file from `cardid-window`: offset 0x356000 of a
/// linked program longer than 0x35A000 bytes holds code, not a flash partition. The commit hooks
/// and the tree scan never call it, so they stay strict. The checks are the ones a loader makes
/// before it reads anything else, so a flash image with a forged first few bytes fails them:
///
/// | Container | Checked |
/// |---|---|
/// | PE | `MZ`, `e_lfanew` at 0x3C inside the file, `PE\0\0` there |
/// | ELF | `\x7fELF`, `EI_CLASS` 1 or 2, `EI_DATA` 1 or 2, `EI_VERSION` 1, `e_version` 1 |
/// | thin Mach-O | 32/64-bit magic in either byte order, `ncmds` load commands of at least 8 bytes each that add up to `sizeofcmds` inside the file |
/// | fat Mach-O | `CAFEBABE`/`CAFEBABF`, 1 to 64 architectures, every slice inside the file |
/// | wasm | `\0asm`, version 1, sections (id 0 to 13, LEB128 size) that end exactly at the end of the file |
pub fn executable_structure(bytes: &[u8]) -> bool {
    pe_structure(bytes)
        || elf_structure(bytes)
        || macho_structure(bytes)
        || fat_structure(bytes)
        || wasm_structure(bytes)
}

fn u32_at(bytes: &[u8], at: usize, big: bool) -> Option<u32> {
    let word: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(if big {
        u32::from_be_bytes(word)
    } else {
        u32::from_le_bytes(word)
    })
}

fn u64_at(bytes: &[u8], at: usize, big: bool) -> Option<u64> {
    let word: [u8; 8] = bytes.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(if big {
        u64::from_be_bytes(word)
    } else {
        u64::from_le_bytes(word)
    })
}

fn pe_structure(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"MZ") {
        return false;
    }
    let Some(lfanew) = u32_at(bytes, 0x3C, false) else {
        return false;
    };
    let lfanew = lfanew as usize;
    lfanew >= 0x40 && bytes.get(lfanew..lfanew.saturating_add(4)) == Some(b"PE\0\0")
}

fn elf_structure(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"\x7fELF") || bytes.len() < 0x18 {
        return false;
    }
    let (class, data, version) = (bytes[4], bytes[5], bytes[6]);
    if !(class == 1 || class == 2) || !(data == 1 || data == 2) || version != 1 {
        return false;
    }
    u32_at(bytes, 0x14, data == 2) == Some(1)
}

fn macho_structure(bytes: &[u8]) -> bool {
    let (big, header) = match bytes.get(..4) {
        Some([0xFE, 0xED, 0xFA, 0xCE]) => (true, 28),
        Some([0xCE, 0xFA, 0xED, 0xFE]) => (false, 28),
        Some([0xFE, 0xED, 0xFA, 0xCF]) => (true, 32),
        Some([0xCF, 0xFA, 0xED, 0xFE]) => (false, 32),
        _ => return false,
    };
    let (Some(ncmds), Some(sizeofcmds)) = (u32_at(bytes, 16, big), u32_at(bytes, 20, big)) else {
        return false;
    };
    let (ncmds, sizeofcmds) = (ncmds as usize, sizeofcmds as usize);
    if ncmds == 0 || header + sizeofcmds > bytes.len() || sizeofcmds < ncmds * 8 {
        return false;
    }
    let (mut at, mut seen) = (header, 0usize);
    while seen < ncmds {
        let Some(size) = u32_at(bytes, at + 4, big) else {
            return false;
        };
        let size = size as usize;
        if size < 8 || at + size > header + sizeofcmds {
            return false;
        }
        at += size;
        seen += 1;
    }
    at == header + sizeofcmds
}

fn fat_structure(bytes: &[u8]) -> bool {
    let wide = match bytes.get(..4) {
        Some([0xCA, 0xFE, 0xBA, 0xBE]) => false,
        Some([0xCA, 0xFE, 0xBA, 0xBF]) => true,
        _ => return false,
    };
    let Some(count) = u32_at(bytes, 4, true) else {
        return false;
    };
    let count = count as usize;
    let entry = if wide { 32 } else { 20 };
    if count == 0 || count > 64 || 8 + count * entry > bytes.len() {
        return false;
    }
    (0..count).all(|i| {
        let at = 8 + i * entry;
        let (offset, size) = if wide {
            (u64_at(bytes, at + 8, true), u64_at(bytes, at + 16, true))
        } else {
            (
                u32_at(bytes, at + 8, true).map(u64::from),
                u32_at(bytes, at + 12, true).map(u64::from),
            )
        };
        matches!((offset, size), (Some(offset), Some(size))
            if size > 0 && offset.checked_add(size).is_some_and(|end| end <= bytes.len() as u64))
    })
}

/// `\0asm` version 1 followed by sections that tile the rest of the file exactly: each a known
/// section id (0 custom to 13 tag) and a LEB128 `u32` size of at most five bytes whose payload
/// ends inside the file, the last one ending at its last byte. A module is only these, so a flash
/// image or any other file behind a forged header fails the walk.
fn wasm_structure(bytes: &[u8]) -> bool {
    if !(bytes.starts_with(b"\0asm") && u32_at(bytes, 4, false) == Some(1)) {
        return false;
    }
    let mut at = 8usize;
    while at < bytes.len() {
        if bytes[at] > 13 {
            return false;
        }
        at += 1;
        let mut size = 0u64;
        let mut shift = 0;
        loop {
            let Some(&byte) = bytes.get(at) else {
                return false;
            };
            at += 1;
            size |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return false;
            }
        }
        if size > u64::from(u32::MAX) {
            return false;
        }
        match at.checked_add(size as usize) {
            Some(end) if end <= bytes.len() => at = end,
            _ => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// backup-name
// ---------------------------------------------------------------------------------------------

/// Directory name of the local device backup store.
const BACKUP_DIR: &str = "passport-backups";
/// File name prefixes of device data, matching the device-data entries of `.gitignore`.
const DEVICE_PREFIXES: &[&str] = &["efuse_blk", "cardid", "boot_log", "ground_truth"];
/// Extensions of raw dumps.
const DUMP_EXTENSIONS: &[&str] = &["bin", "img", "dump", "dmp", "raw"];
/// Compression or archive extensions stripped once before the dump check.
const WRAPPER_EXTENSIONS: &[&str] = &["gz", "xz", "zst", "bz2", "zip", "7z"];
/// Stem words that mark a raw dump as a device backup.
const DUMP_WORDS: &[&str] = &[
    "backup", "dump", "flash", "full", "efuse", "nvs", "cardid", "readback", "passport", "4m",
    "8m", "16m",
];

/// Whether a `/`-separated path looks like device data by name (case-insensitive):
///
/// - a directory component is `passport-backups`;
/// - the file name starts with `efuse_blk`, `cardid`, `boot_log` or `ground_truth`;
/// - the file is a raw dump (`.bin`, `.img`, `.dump`, `.dmp`, `.raw`, optionally followed by
///   one compression extension) whose stem contains `backup`, `dump`, `flash`, `full`,
///   `efuse`, `nvs`, `cardid`, `readback`, `passport`, `4m`, `8m` or `16m`;
/// - any path component carries a MAC ([`name_has_mac`]).
///
/// Exact device backup stems are matched by value only by the hashed rules.
pub fn backup_name(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    let (dir, name) = lower.rsplit_once('/').unwrap_or(("", lower.as_str()));
    if dir.split('/').any(|c| c == BACKUP_DIR) {
        return true;
    }
    if DEVICE_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    let mut base = name;
    if let Some((stem, ext)) = split_ext(base)
        && WRAPPER_EXTENSIONS.contains(&ext)
    {
        base = stem;
    }
    if let Some((stem, ext)) = split_ext(base)
        && DUMP_EXTENSIONS.contains(&ext)
        && DUMP_WORDS.iter().any(|w| stem.contains(w))
    {
        return true;
    }
    lower.split('/').any(name_has_mac)
}

fn split_ext(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty())
}

/// Whether one path component carries a MAC: a separated MAC shape as in
/// [`mac_shape_offsets`] (with `_` also accepted as a boundary), or a token of exactly 12 hex
/// digits holding at least one decimal digit and one letter. Tokens split at every character
/// that is not an ASCII letter or digit. Allowed addresses ([`is_allowed_mac`]) do not count.
pub fn name_has_mac(component: &str) -> bool {
    if !mac_shape_offsets(component.replace('_', " ").as_bytes()).is_empty() {
        return true;
    }
    component
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| {
            let t = token.as_bytes();
            if t.len() != 12 || !t.iter().all(u8::is_ascii_hexdigit) {
                return false;
            }
            if !t.iter().any(u8::is_ascii_digit) || !t.iter().any(u8::is_ascii_alphabetic) {
                return false;
            }
            let mut octets = [0u8; 6];
            for (k, octet) in octets.iter_mut().enumerate() {
                *octet = hex_pair(t, 2 * k).unwrap_or(0);
            }
            !is_allowed_mac(octets)
        })
}

/// The path printed for a file whose name tripped `backup-name`: its directory with the name
/// replaced, or no part of the path when a directory component itself carries a MAC.
pub fn withheld_path(rel: &str) -> String {
    match rel.rsplit_once('/') {
        None => "<name withheld>".to_string(),
        Some((dir, _)) if dir.split('/').any(name_has_mac) => "<path withheld>".to_string(),
        Some((dir, _)) => format!("{dir}/<name withheld>"),
    }
}
