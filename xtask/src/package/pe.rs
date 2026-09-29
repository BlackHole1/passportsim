//! A reader for the three facts the Windows package audits in `passportsim.exe`: the machine type,
//! the DLLs its import tables name, and its `RT_MANIFEST` resource.
//!
//! Written from Microsoft Learn, "PE Format". It reads only what those facts need and refuses
//! anything it cannot follow, rather than guessing: an audit that could not read the import table
//! must not report that it found no `vcruntime`.

/// `IMAGE_FILE_MACHINE_AMD64`.
pub const MACHINE_AMD64: u16 = 0x8664;
/// `IMAGE_FILE_MACHINE_ARM64`.
pub const MACHINE_ARM64: u16 = 0xAA64;
/// `RT_MANIFEST`, the resource type of an application manifest.
pub const RT_MANIFEST: u32 = 24;
/// `CREATEPROCESS_MANIFEST_RESOURCE_ID`: the manifest the loader reads for an executable.
pub const MANIFEST_ID: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// COFF `Machine`.
    pub machine: u16,
    /// DLL names of the import directory table, in table order.
    pub imports: Vec<String>,
    /// DLL names of the delay-load directory table, in table order.
    pub delay_imports: Vec<String>,
    /// The bytes of resource `RT_MANIFEST` / [`MANIFEST_ID`], any language, if there is one.
    pub manifest: Option<Vec<u8>>,
}

/// One section header, as much of it as RVA translation needs.
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_size: u32,
    raw_pointer: u32,
}

struct Pe<'a> {
    bytes: &'a [u8],
    sections: Vec<Section>,
}

/// Reads the machine, the imported DLLs and the manifest of a PE image.
pub fn parse(bytes: &[u8]) -> Result<Image, String> {
    if bytes.get(..2) != Some(b"MZ") {
        return Err("not a PE image: no `MZ` signature".into());
    }
    let pe_at = u32_at(bytes, 0x3C)? as usize;
    if bytes.get(pe_at..pe_at + 4) != Some(b"PE\0\0") {
        return Err(format!(
            "not a PE image: no `PE\\0\\0` at e_lfanew 0x{pe_at:x}"
        ));
    }
    let coff = pe_at + 4;
    let machine = u16_at(bytes, coff)?;
    let sections = usize::from(u16_at(bytes, coff + 2)?);
    let optional_size = usize::from(u16_at(bytes, coff + 16)?);
    let optional = coff + 20;
    // PE32 and PE32+ differ in where `NumberOfRvaAndSizes` and the data directories sit.
    let (count_at, directories) = match u16_at(bytes, optional)? {
        0x10B => (optional + 92, optional + 96),
        0x20B => (optional + 108, optional + 112),
        magic => {
            return Err(format!(
                "optional header magic 0x{magic:x} is neither PE32 nor PE32+"
            ));
        }
    };
    let directory_count = u32_at(bytes, count_at)? as usize;
    let directory = |index: usize| -> Result<(u32, u32), String> {
        if index >= directory_count {
            return Ok((0, 0));
        }
        let at = directories + index * 8;
        Ok((u32_at(bytes, at)?, u32_at(bytes, at + 4)?))
    };
    let mut table = Vec::with_capacity(sections);
    let first = optional + optional_size;
    for index in 0..sections {
        let at = first + index * 40;
        table.push(Section {
            virtual_size: u32_at(bytes, at + 8)?,
            virtual_address: u32_at(bytes, at + 12)?,
            raw_size: u32_at(bytes, at + 16)?,
            raw_pointer: u32_at(bytes, at + 20)?,
        });
    }
    let pe = Pe {
        bytes,
        sections: table,
    };
    Ok(Image {
        machine,
        imports: pe.dll_names(directory(1)?, 20, 12)?,
        delay_imports: pe.dll_names(directory(13)?, 32, 4)?,
        manifest: pe.manifest(directory(2)?)?,
    })
}

impl Pe<'_> {
    /// The file offset of `rva`, through the section that maps it.
    fn offset(&self, rva: u32) -> Result<usize, String> {
        for section in &self.sections {
            let size = section.virtual_size.max(section.raw_size);
            if rva >= section.virtual_address && rva - section.virtual_address < size {
                let delta = rva - section.virtual_address;
                if delta >= section.raw_size {
                    return Err(format!(
                        "RVA 0x{rva:x} is in a section's uninitialized tail"
                    ));
                }
                return Ok(section.raw_pointer as usize + delta as usize);
            }
        }
        Err(format!("RVA 0x{rva:x} is in no section"))
    }

    /// The NUL-terminated ASCII string at `rva`.
    fn string(&self, rva: u32) -> Result<String, String> {
        let at = self.offset(rva)?;
        let tail = self
            .bytes
            .get(at..)
            .ok_or_else(|| format!("string RVA 0x{rva:x} is past the end of the file"))?;
        let end = tail
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| format!("string at RVA 0x{rva:x} is not terminated"))?;
        Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
    }

    /// The DLL names of an import-style directory: fixed-size entries, the name RVA at
    /// `name_field`, and an all-zero entry at the end.
    fn dll_names(
        &self,
        (rva, size): (u32, u32),
        entry: usize,
        name_field: usize,
    ) -> Result<Vec<String>, String> {
        if rva == 0 || size == 0 {
            return Ok(Vec::new());
        }
        let start = self.offset(rva)?;
        let mut names = Vec::new();
        for index in 0.. {
            let at = start + index * entry;
            let raw = self
                .bytes
                .get(at..at + entry)
                .ok_or_else(|| "an import directory runs past the end of the file".to_string())?;
            if raw.iter().all(|&b| b == 0) {
                break;
            }
            names.push(self.string(u32_at(self.bytes, at + name_field)?)?);
        }
        Ok(names)
    }

    /// Resource `RT_MANIFEST` / [`MANIFEST_ID`], in the first language that has it. The tree is type,
    /// name, language; offsets are relative to the resource section, and a set high bit marks a
    /// subdirectory.
    fn manifest(&self, (rva, size): (u32, u32)) -> Result<Option<Vec<u8>>, String> {
        if rva == 0 || size == 0 {
            return Ok(None);
        }
        let base = self.offset(rva)?;
        let Some(names) = self.child(base, base, RT_MANIFEST)? else {
            return Ok(None);
        };
        let Some(languages) = self.child(base, names, MANIFEST_ID)? else {
            return Ok(None);
        };
        let entries = usize::from(u16_at(self.bytes, languages + 12)?)
            + usize::from(u16_at(self.bytes, languages + 14)?);
        if entries == 0 {
            return Ok(None);
        }
        let data = u32_at(self.bytes, languages + 16 + 4)?;
        if data & 0x8000_0000 != 0 {
            return Err("the manifest's language level is a directory, not data".into());
        }
        let leaf = base + data as usize;
        let data_rva = u32_at(self.bytes, leaf)?;
        let data_size = u32_at(self.bytes, leaf + 4)? as usize;
        let at = self.offset(data_rva)?;
        self.bytes
            .get(at..at + data_size)
            .map(|bytes| Some(bytes.to_vec()))
            .ok_or_else(|| "the manifest runs past the end of the file".into())
    }

    /// The subdirectory of the resource directory at `dir` whose integer id is `id`.
    fn child(&self, base: usize, dir: usize, id: u32) -> Result<Option<usize>, String> {
        let named = usize::from(u16_at(self.bytes, dir + 12)?);
        let ids = usize::from(u16_at(self.bytes, dir + 14)?);
        for index in named..named + ids {
            let at = dir + 16 + index * 8;
            if u32_at(self.bytes, at)? != id {
                continue;
            }
            let offset = u32_at(self.bytes, at + 4)?;
            if offset & 0x8000_0000 == 0 {
                return Err(format!(
                    "resource id {id} is data where a directory belongs"
                ));
            }
            return Ok(Some(base + (offset & 0x7FFF_FFFF) as usize));
        }
        Ok(None)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| format!("a 16-bit field at 0x{at:x} is past the end of the file"))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("a 32-bit field at 0x{at:x} is past the end of the file"))
}
