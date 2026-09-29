//! ELF and symbols, ESP image and app descriptor, partition table, ROM image assembly with the
//! bundled ROM pin check, eFuse synthesis and dump import, bundles. No run-time path, env or
//! file-system lookup; the bundled ROMs are embedded at compile time behind the default feature
//! `bundled-rom`.

use core::fmt;

pub mod app_desc;
pub mod bundle;
pub mod efuse_image;
pub mod elf;
pub mod esp_image;
pub mod partitions;
pub mod rom;
pub mod symbols;

/// Error of the bytes-in parsers behind `pemu_load`.
/// UNVERIFIED placeholder: no design fixes this error shape yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// The input ends before a structure does.
    Truncated {
        what: &'static str,
        offset: usize,
        need: usize,
        len: usize,
    },
    BadMagic {
        what: &'static str,
        offset: usize,
        found: u32,
    },
    /// The ELF reader rejected the file.
    Elf(String),
    /// A structure is present but inconsistent.
    Malformed { what: &'static str, detail: String },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Truncated {
                what,
                offset,
                need,
                len,
            } => write!(
                f,
                "{what}: need {need} bytes at offset {offset:#x}, input has {len}"
            ),
            LoadError::BadMagic {
                what,
                offset,
                found,
            } => write!(f, "{what}: bad magic {found:#x} at offset {offset:#x}"),
            LoadError::Elf(msg) => write!(f, "ELF: {msg}"),
            LoadError::Malformed { what, detail } => write!(f, "{what}: {detail}"),
        }
    }
}

impl std::error::Error for LoadError {}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0xF)] as char);
    }
    s
}

pub fn parse_sha256_hex(text: &str) -> Option<[u8; 32]> {
    let t = text.as_bytes();
    if t.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in t.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

pub(crate) fn slice<'a>(
    bytes: &'a [u8],
    offset: usize,
    len: usize,
    what: &'static str,
) -> Result<&'a [u8], LoadError> {
    offset
        .checked_add(len)
        .and_then(|end| bytes.get(offset..end))
        .ok_or(LoadError::Truncated {
            what,
            offset,
            need: len,
            len: bytes.len(),
        })
}

pub(crate) fn le_u16(bytes: &[u8], offset: usize, what: &'static str) -> Result<u16, LoadError> {
    let b = slice(bytes, offset, 2, what)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

pub(crate) fn le_u32(bytes: &[u8], offset: usize, what: &'static str) -> Result<u32, LoadError> {
    let b = slice(bytes, offset, 4, what)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Bytes up to the first NUL, decoded lossily.
pub(crate) fn c_str(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_a_sha256() {
        let digest = sha256(b"abc");
        let text = hex(&digest);
        assert_eq!(
            text,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(parse_sha256_hex(&text), Some(digest));
        assert_eq!(parse_sha256_hex("zz"), None);
    }

    #[test]
    fn readers_report_truncation() {
        let b = [1u8, 2, 3, 4, 5];
        assert_eq!(le_u32(&b, 1, "t"), Ok(0x0504_0302));
        assert_eq!(le_u16(&b, 0, "t"), Ok(0x0201));
        assert!(matches!(
            le_u32(&b, 2, "t"),
            Err(LoadError::Truncated {
                need: 4,
                len: 5,
                ..
            })
        ));
        assert!(slice(&b, usize::MAX, 2, "t").is_err());
        assert_eq!(c_str(b"ab\0cd"), "ab");
        assert_eq!(c_str(b"abcd"), "abcd");
    }
}
