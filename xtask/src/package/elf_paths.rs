//! The build paths in the demo's application ELF, blanked in place before it is packaged.
//!
//! The `official` ELF holds 5,261 profile paths of the account that built it, all in `.debug_line`
//! and `.debug_str`. Stripping `.debug_*` would lose what the packaged demo is read through (the
//! DWARF layouts behind `ui` and `inspect`, `.debug_frame`, symbolized frames), so each path's
//! account part is overwritten with `_`, same length: no offset or section moves. A profile path
//! outside a non-loadable `.debug_*` section refuses the demo. Blanking twice changes nothing.
//! Only a little-endian ELF32 is read.

use std::ops::Range;

use super::account;

/// `SHF_ALLOC`: the section occupies memory at run time (System V gABI, "Sections").
const SHF_ALLOC: u32 = 0x2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blanked {
    pub bytes: Vec<u8>,
    /// How many profile paths were blanked.
    pub paths: usize,
}

/// One section of an ELF32: its name and the file bytes it covers.
struct Section {
    name: String,
    flags: u32,
    file: Range<usize>,
}

/// `elf` with the account part of every profile path blanked.
pub fn blank(elf: &[u8]) -> Result<Blanked, String> {
    let spans = account::profile_path_spans(elf);
    if spans.is_empty() {
        return Ok(Blanked {
            bytes: elf.to_vec(),
            paths: 0,
        });
    }
    let sections = sections(elf)?;
    let mut bytes = elf.to_vec();
    for span in &spans {
        let Some(section) = sections
            .iter()
            .find(|s| s.file.start <= span.start && span.end <= s.file.end)
        else {
            return Err(format!(
                "the application ELF holds a profile path at byte {} outside every section, which \
                 cannot be blanked without knowing what reads it",
                span.start
            ));
        };
        if section.flags & SHF_ALLOC != 0 || !section.name.starts_with(".debug") {
            return Err(format!(
                "the application ELF holds a profile path in `{}`, which is not a debug section; \
                 blanking it would change bytes other than build paths",
                section.name
            ));
        }
        bytes[span.clone()].fill(b'_');
    }
    let left = account::profile_path_spans(&bytes);
    if !left.is_empty() {
        return Err(format!(
            "{} profile path(s) are left in the application ELF after blanking",
            left.len()
        ));
    }
    Ok(Blanked {
        bytes,
        paths: spans.len(),
    })
}

/// The sections of a little-endian ELF32, with their names from the section name table.
fn sections(elf: &[u8]) -> Result<Vec<Section>, String> {
    let not = |what: &str| format!("the application ELF is not a little-endian ELF32: {what}");
    if elf.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err(not("no ELF magic"));
    }
    if elf.get(4) != Some(&1) || elf.get(5) != Some(&1) {
        return Err(not("EI_CLASS or EI_DATA"));
    }
    let u16_at = |at: usize| -> Result<usize, String> {
        elf.get(at..at + 2)
            .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
            .ok_or_else(|| not("a header field is past the end"))
    };
    let u32_at = |at: usize| -> Result<u32, String> {
        elf.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| not("a header field is past the end"))
    };
    let shoff = u32_at(32)? as usize;
    let shentsize = u16_at(46)?;
    let shnum = u16_at(48)?;
    let shstrndx = u16_at(50)?;
    if shentsize < 40 {
        return Err(not("e_shentsize is below 40"));
    }
    let mut raw = Vec::with_capacity(shnum);
    for index in 0..shnum {
        let at = shoff + index * shentsize;
        let (name, kind, flags) = (u32_at(at)?, u32_at(at + 4)?, u32_at(at + 8)?);
        let (offset, size) = (u32_at(at + 16)? as usize, u32_at(at + 20)? as usize);
        // SHT_NOBITS (8) and SHT_NULL (0) occupy no file bytes.
        let file = if kind == 8 || kind == 0 {
            offset..offset
        } else {
            offset..offset.saturating_add(size)
        };
        if file.end > elf.len() {
            return Err(not("a section runs past the end of the file"));
        }
        raw.push((name as usize, flags, file));
    }
    let names = raw
        .get(shstrndx)
        .map(|(_, _, file)| &elf[file.clone()])
        .ok_or_else(|| not("e_shstrndx names no section"))?;
    Ok(raw
        .into_iter()
        .map(|(name, flags, file)| {
            let tail = names.get(name..).unwrap_or_default();
            let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
            Section {
                name: String::from_utf8_lossy(&tail[..end]).into_owned(),
                flags,
                file,
            }
        })
        .collect())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A little-endian ELF32 with the sections given as (name, flags, bytes), laid out one after
    /// another after the header, then the section name table and the section header table.
    pub(crate) fn elf32(sections: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = vec![0u8; 52];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 1;
        out[5] = 1;
        let mut names = vec![0u8];
        let mut headers = vec![[0u32; 10]];
        for (name, flags, bytes) in sections {
            let name_at = names.len() as u32;
            names.extend_from_slice(name.as_bytes());
            names.push(0);
            headers.push([
                name_at,
                1,
                *flags,
                0,
                out.len() as u32,
                bytes.len() as u32,
                0,
                0,
                1,
                0,
            ]);
            out.extend_from_slice(bytes);
        }
        let shstr_name = names.len() as u32;
        names.extend_from_slice(b".shstrtab\0");
        headers.push([
            shstr_name,
            3,
            0,
            0,
            out.len() as u32,
            names.len() as u32,
            0,
            0,
            1,
            0,
        ]);
        out.extend_from_slice(&names);
        let shoff = out.len() as u32;
        for header in &headers {
            for field in header {
                out.extend_from_slice(&field.to_le_bytes());
            }
        }
        out[32..36].copy_from_slice(&shoff.to_le_bytes());
        out[46..48].copy_from_slice(&40u16.to_le_bytes());
        out[48..50].copy_from_slice(&(headers.len() as u16).to_le_bytes());
        out[50..52].copy_from_slice(&((headers.len() - 1) as u16).to_le_bytes());
        out
    }

    #[test]
    fn paths_in_debug_sections_are_blanked_in_place_and_nothing_else_moves() {
        let text: &[u8] = b"\x13\x00\x00\x00code";
        let line: &[u8] = b"/Users/someone/esp/idf/components\0main.c\0";
        let strs: &[u8] = b"C:\\Users\\Someone\\build\0int\0/home/ci/x\0";
        let elf = elf32(&[
            (".flash.text", SHF_ALLOC | 0x4, text),
            (".debug_line", 0, line),
            (".debug_str", 0x30, strs),
            (".symtab", 0, b"symbols"),
        ]);
        let blanked = blank(&elf).expect("blanked");
        assert_eq!(blanked.paths, 3);
        assert_eq!(blanked.bytes.len(), elf.len());
        let text_at = 52;
        assert_eq!(&blanked.bytes[text_at..text_at + text.len()], text);
        let shown = String::from_utf8_lossy(&blanked.bytes);
        assert!(
            shown.contains("/_____________/esp/idf/components"),
            "{shown}"
        );
        assert!(shown.contains("C:\\_____________\\build"), "{shown}");
        assert!(shown.contains("/_______/x"), "{shown}");
        assert!(account::profile_path_spans(&blanked.bytes).is_empty());
        // Blanking the blanked ELF changes nothing, which is what `--payload-from` checks.
        assert_eq!(blank(&blanked.bytes).expect("again").bytes, blanked.bytes);
        assert_eq!(blank(&blanked.bytes).expect("again").paths, 0);
        // Every byte that differs is a `_` inside a debug section.
        for (at, (a, b)) in elf.iter().zip(&blanked.bytes).enumerate() {
            if a != b {
                assert_eq!(*b, b'_', "byte {at}");
            }
        }
    }

    #[test]
    fn a_path_outside_a_debug_section_refuses_the_elf() {
        let loadable = elf32(&[(".flash.rodata", SHF_ALLOC, b"/Users/someone/cfg\0")]);
        let refused = blank(&loadable).unwrap_err();
        assert!(refused.contains("`.flash.rodata`"), "{refused}");
        let comment = elf32(&[(".comment", 0x30, b"GCC /home/someone/gcc\0")]);
        assert!(blank(&comment).unwrap_err().contains("`.comment`"));
    }

    #[test]
    fn a_file_without_a_profile_path_is_returned_unparsed() {
        let blanked = blank(b"application elf").expect("no path, nothing to parse");
        assert_eq!(blanked.bytes, b"application elf");
        assert_eq!(blanked.paths, 0);
        assert!(blank(b"not an elf /Users/someone/x").is_err());
    }
}
