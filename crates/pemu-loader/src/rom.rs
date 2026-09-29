//! ROM image assembly from the esp-rom-elfs ROM ELFs, the bundled ROMs, the pin check against
//! `assets/rom/pins.toml` and the unused-range check behind the magic-PC static proof.
//!
//! One [`ROM_LEN`]-byte image whose IROM view starts at [`IROM_BASE`]; the DROM view at
//! [`DROM_BASE`] aliases image offset [`DROM_IMAGE_OFFSET`].
//! 1. Sections with contents in IROM go to `addr - IROM_BASE`.
//! 2. Sections with contents in DROM go to `addr - DROM_BASE + DROM_IMAGE_OFFSET`. This includes
//!    `.rodata.interface`, which is PROGBITS but not ALLOC and so sits in no LOAD segment.
//! 3. The `.data` initializers, which no LOAD segment carries, follow the executable LOAD
//!    segment: `.data.interface.*` by address, then `.static_dram_start` (whose own LOAD segment
//!    must agree), then `.data_*` by address, each at its own alignment.
//!
//! Every other byte is zero, which the magic-range proof relies on.

use crate::efuse_image::{EfuseImage, Revision};
use crate::elf::{ElfInfo, ElfSection, ElfSegment};
use crate::symbols::SymbolTable;
use crate::{LoadError, parse_sha256_hex, sha256};

pub const IROM_BASE: u32 = 0x4000_0000;
/// Length of the image and of its IROM view.
pub const ROM_LEN: usize = 0x6_0000;
pub const DROM_BASE: u32 = 0x3FF0_0000;
pub const DROM_IMAGE_OFFSET: usize = 0x4_0000;
pub const DROM_LEN: usize = 0x2_0000;
pub const PINS_TOML: &str = include_str!("../../../assets/rom/pins.toml");

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PlacementKind {
    Irom,
    /// Through the DROM alias.
    Drom,
    /// A `.data` initializer at its load address.
    DataInit,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Placement {
    pub name: String,
    pub kind: PlacementKind,
    /// Section address in the ELF (the RAM address for initializers).
    pub vma: u32,
    /// Offset in the image; the IROM address is `IROM_BASE + offset`.
    pub offset: usize,
    pub size: u32,
}

/// The ROM image mapped into the SoC: the bundled ELF or an override.
/// UNVERIFIED placeholder: no design fixes this type's shape.
pub struct RomImage {
    bytes: Vec<u8>,
    elf_sha256: [u8; 32],
    pinned: Option<RomRev>,
    placements: Vec<Placement>,
    symbols: SymbolTable,
}

impl RomImage {
    /// Does not refuse unpinned bytes ([`check_pin`] does); [`RomImage::pinned`] reports a match.
    /// UNVERIFIED signature: no design fixes this function's shape.
    pub fn from_elf(bytes: &[u8]) -> Result<RomImage, LoadError> {
        let info = ElfInfo::parse(bytes)?;
        let mut image = Image {
            bytes: vec![0; ROM_LEN],
            placements: Vec::new(),
        };
        let mut initializers = Vec::new();
        for s in info.sections.iter().filter(|s| s.has_bits()) {
            if let Some((kind, offset)) = window(s)? {
                image.place(s, kind, offset, section_data(bytes, s)?)?;
            } else if is_initializer(&s.name) {
                initializers.push(s);
            }
        }
        place_initializers(&mut image, bytes, &info.segments, initializers)?;
        image.placements.sort_by_key(|p| p.offset);
        Ok(RomImage {
            bytes: image.bytes,
            elf_sha256: info.sha256,
            pinned: pin_for(&info.sha256).map(|p| p.rev),
            placements: image.placements,
            symbols: info.symbols,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The key of `assets/rom/pins.toml` and `specs/rom-pins.toml`.
    pub fn elf_sha256(&self) -> &[u8; 32] {
        &self.elf_sha256
    }

    pub fn image_sha256(&self) -> [u8; 32] {
        sha256(&self.bytes)
    }

    /// `None` for an unpinned override, which gets no ROM-hash-keyed hooks.
    pub fn pinned(&self) -> Option<RomRev> {
        self.pinned
    }

    /// Sorted by image offset.
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Image offset of an address in the IROM or DROM view.
    pub fn offset_of(addr: u32) -> Option<usize> {
        let irom = addr.wrapping_sub(IROM_BASE) as usize;
        let drom = addr.wrapping_sub(DROM_BASE) as usize;
        if irom < ROM_LEN {
            Some(irom)
        } else if drom < DROM_LEN {
            Some(drom + DROM_IMAGE_OFFSET)
        } else {
            None
        }
    }

    pub fn read(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let offset = RomImage::offset_of(addr)?;
        self.bytes.get(offset..offset.checked_add(len)?)
    }
}

struct Image {
    bytes: Vec<u8>,
    placements: Vec<Placement>,
}

impl Image {
    fn place(
        &mut self,
        s: &ElfSection,
        kind: PlacementKind,
        offset: usize,
        data: &[u8],
    ) -> Result<(), LoadError> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset + data.len();
        if end > ROM_LEN {
            return Err(malformed(format!(
                "{} ends at image offset {end:#x}, past {ROM_LEN:#x}",
                s.name
            )));
        }
        if let Some(p) = self
            .placements
            .iter()
            .find(|p| p.offset < end && offset < p.offset + p.size as usize)
        {
            return Err(malformed(format!("{} overlaps {}", s.name, p.name)));
        }
        self.bytes[offset..end].copy_from_slice(data);
        self.placements.push(Placement {
            name: s.name.clone(),
            kind,
            vma: s.addr,
            offset,
            size: data.len() as u32,
        });
        Ok(())
    }
}

fn malformed(detail: String) -> LoadError {
    LoadError::Malformed {
        what: "ROM ELF",
        detail,
    }
}

fn section_data<'a>(elf: &'a [u8], s: &ElfSection) -> Result<&'a [u8], LoadError> {
    s.data(elf).ok_or(LoadError::Truncated {
        what: "ROM ELF section",
        offset: s.offset as usize,
        need: s.size as usize,
        len: elf.len(),
    })
}

/// Steps 1 and 2 of the module layout; `None` outside both views.
fn window(s: &ElfSection) -> Result<Option<(PlacementKind, usize)>, LoadError> {
    let irom = s.addr.wrapping_sub(IROM_BASE) as usize;
    let drom = s.addr.wrapping_sub(DROM_BASE) as usize;
    let (kind, offset) = if irom < ROM_LEN {
        (PlacementKind::Irom, irom)
    } else if drom < DROM_LEN {
        (PlacementKind::Drom, drom + DROM_IMAGE_OFFSET)
    } else {
        return Ok(None);
    };
    if offset + s.size as usize > ROM_LEN {
        return Err(malformed(format!(
            "{} at {:#x} (size {:#x}) leaves the ROM image",
            s.name, s.addr, s.size
        )));
    }
    Ok(Some((kind, offset)))
}

const STATIC_DRAM_START: &str = ".static_dram_start";

/// `align` 0 and 1 mean no padding.
fn align_up(offset: usize, align: u32) -> usize {
    let align = (align as usize).max(1);
    offset.next_multiple_of(align)
}

fn is_initializer(name: &str) -> bool {
    name.starts_with(".data.interface.") || name.starts_with(".data_") || name == STATIC_DRAM_START
}

/// Step 3 of the module layout. Each zero-size LOAD segment inside the span must start one of
/// these sections.
fn place_initializers(
    image: &mut Image,
    elf: &[u8],
    segments: &[ElfSegment],
    mut sections: Vec<&ElfSection>,
) -> Result<(), LoadError> {
    if image.placements.is_empty() {
        return Err(malformed("no section in the IROM or DROM view".into()));
    }
    let text_end = segments
        .iter()
        .filter(|g| g.is_load() && g.is_exec() && g.filesz > 0)
        .filter_map(|g| RomImage::offset_of(g.paddr).map(|o| o + g.filesz as usize))
        .max()
        .ok_or_else(|| malformed("no executable LOAD segment in the ROM".into()))?;
    let group = |s: &ElfSection| match s.name.as_str() {
        n if n.starts_with(".data.interface.") => 0,
        STATIC_DRAM_START => 1,
        _ => 2,
    };
    // Stable: sections at the same address keep their section-table order.
    sections.sort_by_key(|s| (group(s), s.addr));
    let mut cursor = text_end;
    let mut starts = Vec::with_capacity(sections.len());
    for s in sections {
        if s.is_alloc() {
            // Its own LOAD segment states the load address; any gap to the cursor is zero
            // alignment padding (rev0 pads 2 bytes here).
            let lma = segments
                .iter()
                .find(|g| {
                    g.is_load() && g.filesz == s.size && g.vaddr == s.addr && g.offset == s.offset
                })
                .and_then(|g| RomImage::offset_of(g.paddr));
            match lma {
                Some(o) if o >= cursor && o <= align_up(cursor, s.align) => {
                    cursor = o;
                }
                _ => {
                    return Err(malformed(format!(
                        "{} loads at image offset {lma:x?}, the initializers reach {cursor:#x}",
                        s.name
                    )));
                }
            }
        }
        starts.push(cursor);
        image.place(s, PlacementKind::DataInit, cursor, section_data(elf, s)?)?;
        cursor += s.size as usize;
    }
    for g in segments.iter().filter(|g| g.is_load() && g.filesz == 0) {
        if let Some(o) = RomImage::offset_of(g.paddr)
            && o > text_end
            && o < cursor
            && !starts.contains(&o)
        {
            return Err(malformed(format!(
                "empty LOAD segment at {:#x} does not start an initializer",
                g.paddr
            )));
        }
    }
    Ok(())
}

/// Chip v1.x (ECO7, the device) uses rev101; v0.3 and v0.4 use rev3.
/// UNVERIFIED placeholder: no design fixes this type or its variant names.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RomRev {
    Rev101,
    Rev3,
}

impl RomRev {
    pub const ALL: [RomRev; 2] = [RomRev::Rev101, RomRev::Rev3];

    /// The file name under `assets/rom/`, as `assets/rom/pins.toml` lists it.
    pub fn file_name(self) -> &'static str {
        match self {
            RomRev::Rev101 => "esp32c3_rev101_rom.elf",
            RomRev::Rev3 => "esp32c3_rev3_rom.elf",
        }
    }

    /// The id `doctor` prints.
    pub fn corpus_id(self) -> &'static str {
        match self {
            RomRev::Rev101 => "rom101",
            RomRev::Rev3 => "rom3",
        }
    }

    pub fn for_efuse(efuse: &EfuseImage) -> Result<RomRev, PinError> {
        RomRev::for_chip_revision(efuse.chip_revision())
    }

    /// Matched against the `chip_revisions` of `assets/rom/pins.toml` (`"v1.x"` is any minor of
    /// major 1); a revision with no bundled ROM, such as v0.0, is `E_ASSET_MISSING`.
    pub fn for_chip_revision(rev: Revision) -> Result<RomRev, PinError> {
        pins()
            .into_iter()
            .find(|p| p.chip_revisions.iter().any(|c| revision_matches(c, rev)))
            .map(|p| p.rev)
            .ok_or(PinError::NoBundledRom)
    }
}

fn revision_matches(pattern: &str, rev: Revision) -> bool {
    let Some((major, minor)) = pattern.strip_prefix('v').and_then(|r| r.split_once('.')) else {
        return false;
    };
    major.parse::<u8>().is_ok_and(|m| m == rev.major)
        && (minor == "x" || minor.parse::<u8>().is_ok_and(|m| m == rev.minor))
}

/// Error of ROM selection and the pin check; variants follow the asset error codes.
/// UNVERIFIED placeholder: no design fixes this type's shape.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PinError {
    NoBundledRom,
    Unpinned,
    /// The pinned files assemble (golden test), so this points at a loader defect.
    Assembly(LoadError),
    /// Both pinned ROMs pass, so this means the pins and `specs/magic-pcs.toml` disagree.
    MagicRange(UnusedRangeError),
}

impl PinError {
    pub fn code_name(&self) -> &'static str {
        match self {
            PinError::NoBundledRom => "E_ASSET_MISSING",
            PinError::Unpinned => "E_ASSET_HASH",
            PinError::Assembly(_) | PinError::MagicRange(_) => "E_INTERNAL",
        }
    }
}

impl core::fmt::Display for PinError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PinError::NoBundledRom => f.write_str("no bundled ROM for this chip revision"),
            PinError::Unpinned => f.write_str("ROM SHA-256 is not in assets/rom/pins.toml"),
            PinError::Assembly(e) => write!(f, "pinned ROM failed to assemble: {e}"),
            PinError::MagicRange(e) => write!(
                f,
                "the magic PC range of specs/magic-pcs.toml is not unused in this pinned ROM: {e}"
            ),
        }
    }
}

impl std::error::Error for PinError {}

impl core::fmt::Display for UnusedRangeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UnusedRangeError::OutsideRom => f.write_str("the range is empty or outside the ROM"),
            UnusedRangeError::Section(name) => write!(f, "section {name} covers it"),
            UnusedRangeError::Symbol(name) => write!(f, "symbol {name} covers it"),
            UnusedRangeError::NonZero(addr) => write!(f, "the byte at {addr:#010x} is not zero"),
        }
    }
}

impl std::error::Error for UnusedRangeError {}

/// One `[[rom]]` entry of `assets/rom/pins.toml` whose file names a bundled revision.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pin {
    pub file: &'static str,
    pub sha256: [u8; 32],
    pub rev: RomRev,
    /// Such as `"v1.x"` or `"v0.3"`.
    pub chip_revisions: Vec<&'static str>,
}

pub fn pins() -> Vec<Pin> {
    parse_pins(PINS_TOML)
}

fn pin_for(sha: &[u8; 32]) -> Option<Pin> {
    pins().into_iter().find(|p| &p.sha256 == sha)
}

/// Lets `pemu-host::assets` report an override whose revision disagrees with the eFuse chip
/// revision.
pub fn pinned_rev(bytes: &[u8]) -> Option<RomRev> {
    pin_for(&sha256(bytes)).map(|p| p.rev)
}

#[derive(Default)]
struct PinFields {
    file: Option<&'static str>,
    sha256: Option<[u8; 32]>,
    chip_revisions: Vec<&'static str>,
}

impl PinFields {
    fn finish(self) -> Option<Pin> {
        let file = self.file?;
        Some(Pin {
            file,
            sha256: self.sha256?,
            rev: RomRev::ALL.into_iter().find(|r| r.file_name() == file)?,
            chip_revisions: self.chip_revisions,
        })
    }
}

/// One `key = value` per line, `chip_revisions` as a one-line array of strings. An entry missing a
/// field is dropped.
fn parse_pins(text: &'static str) -> Vec<Pin> {
    let mut out = Vec::new();
    let mut entry: Option<PinFields> = None;
    for line in text.lines().map(str::trim).chain(["[end]"]) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            out.extend(entry.take().and_then(PinFields::finish));
            if line == "[[rom]]" {
                entry = Some(PinFields::default());
            }
            continue;
        }
        let (Some(fields), Some((key, value))) = (entry.as_mut(), line.split_once('=')) else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "file" => fields.file = unquote(value),
            "sha256" => fields.sha256 = unquote(value).and_then(parse_sha256_hex),
            "chip_revisions" => {
                fields.chip_revisions = value
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .filter_map(|v| unquote(v.trim()))
                    .collect();
            }
            _ => {}
        }
    }
    out
}

fn unquote(value: &str) -> Option<&str> {
    value.strip_prefix('"')?.strip_suffix('"')
}

pub const MAGIC_PCS_TOML: &str = include_str!("../../../specs/magic-pcs.toml");

/// The inclusive IROM range from the `[range]` table of [`MAGIC_PCS_TOML`]. Without both bounds no
/// ROM can be proved and [`check_pin`] refuses.
pub fn magic_range() -> Option<(u32, u32)> {
    let value = |key: &str| {
        MAGIC_PCS_TOML.lines().map(str::trim).find_map(|line| {
            let (k, v) = line.split_once('=')?;
            if k.trim() != key {
                return None;
            }
            let v = v.split('#').next()?.trim();
            u32::from_str_radix(v.strip_prefix("0x").or(v.strip_prefix("0X"))?, 16).ok()
        })
    };
    Some((value("start")?, value("end")?))
}

/// The SHA-256 must be pinned, then the assembled image must prove the magic-PC range unused. A
/// caller honoring `--allow-unpinned-rom` uses [`RomImage::from_elf`] and gets no proof, which is
/// why that path also turns the ROM-hash hooks off.
pub fn check_pin(bytes: &[u8]) -> Result<RomImage, PinError> {
    if pin_for(&sha256(bytes)).is_none() {
        return Err(PinError::Unpinned);
    }
    let image = RomImage::from_elf(bytes).map_err(PinError::Assembly)?;
    let (start, end) = magic_range().ok_or(PinError::MagicRange(UnusedRangeError::OutsideRom))?;
    image
        .check_unused(start, end)
        .map_err(PinError::MagicRange)?;
    Ok(image)
}

#[cfg(feature = "bundled-rom")]
pub fn bundled(rev: RomRev) -> &'static [u8] {
    match rev {
        RomRev::Rev101 => include_bytes!("../../../assets/rom/esp32c3_rev101_rom.elf"),
        RomRev::Rev3 => include_bytes!("../../../assets/rom/esp32c3_rev3_rom.elf"),
    }
}

/// The real embedding state of this binary: with cargo feature unification, a crate built
/// `--no-default-features` can still link a `pemu-loader` that has `bundled-rom` on.
pub fn bundled_opt(rev: RomRev) -> Option<&'static [u8]> {
    #[cfg(feature = "bundled-rom")]
    {
        Some(bundled(rev))
    }
    #[cfg(not(feature = "bundled-rom"))]
    {
        let _ = rev;
        None
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UnusedRangeError {
    /// The range is empty or leaves the IROM view.
    OutsideRom,
    Section(String),
    /// In the IROM view or its DROM alias.
    Symbol(String),
    /// The IROM address of a non-zero byte.
    NonZero(u32),
}

impl RomImage {
    /// Static proof that the inclusive IROM range is unused: no placed section overlaps it; in the
    /// IROM view and its DROM alias no sized symbol overlaps it and no symbol starts inside it
    /// after `start` (a zero-size marker such as `_rodata_end` may sit on `start`); every byte is
    /// zero.
    pub fn check_unused(&self, start: u32, end: u32) -> Result<(), UnusedRangeError> {
        let irom = |a: u32| Some(a.wrapping_sub(IROM_BASE) as usize).filter(|&o| o < ROM_LEN);
        let (Some(first), Some(last)) = (irom(start), irom(end)) else {
            return Err(UnusedRangeError::OutsideRom);
        };
        if last < first {
            return Err(UnusedRangeError::OutsideRom);
        }
        if let Some(p) = self
            .placements
            .iter()
            .find(|p| p.offset <= last && first < p.offset + p.size as usize)
        {
            return Err(UnusedRangeError::Section(p.name.clone()));
        }
        let mut views = vec![(start, u64::from(end) + 1)];
        if last >= DROM_IMAGE_OFFSET {
            let lo = first.max(DROM_IMAGE_OFFSET) - DROM_IMAGE_OFFSET;
            let hi = last - DROM_IMAGE_OFFSET;
            views.push((DROM_BASE + lo as u32, u64::from(DROM_BASE) + hi as u64 + 1));
        }
        for (lo, hi) in views {
            let overlap = self.symbols.overlapping(lo, hi).into_iter();
            if let Some(s) = overlap.chain(self.symbols.in_range(lo + 1, hi)).next() {
                return Err(UnusedRangeError::Symbol(s.name.clone()));
            }
        }
        match self.bytes[first..=last].iter().position(|&b| b != 0) {
            Some(i) => Err(UnusedRangeError::NonZero(start + i as u32)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "bundled-rom")]
    use crate::elf::SHF_EXECINSTR;
    use crate::hex;

    /// Golden SHA-256 of the assembled images, cross-checked with an independent pyelftools
    /// assembly.
    #[cfg(feature = "bundled-rom")]
    const GOLDEN_REV101: &str = "8b41b9b114e110e373a58188e6e86b044719f3b4e6da0441c6e8b68b72986651";
    #[cfg(feature = "bundled-rom")]
    const GOLDEN_REV3: &str = "0de1e65020e803bea0d7443dca149d61895e01fca3bb9c82d073234eebd73f99";

    #[cfg(feature = "bundled-rom")]
    fn spec_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"'))
        })
    }

    #[cfg(feature = "bundled-rom")]
    fn hex_u32(v: &str) -> u32 {
        u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).unwrap()
    }

    #[test]
    fn pins_parse_both_bundled_roms() {
        let p = pins();
        assert_eq!(p.iter().map(|p| p.rev).collect::<Vec<_>>(), RomRev::ALL);
        assert_eq!(p[0].chip_revisions, ["v1.x"]);
        assert_eq!(p[1].chip_revisions, ["v0.3", "v0.4"]);
        let rev101 = "9495e1453f36f7eec4112714ea1dca22d77808776adffa93dda7dda4393e8336";
        assert_eq!(hex(&p[0].sha256), rev101);
        let zeros = "0000000000000000000000000000000000000000000000000000000000000000";
        assert_eq!(zeros.len(), 64);
        let partial = "[[rom]]\nfile = \"esp32c3_rev3_rom.elf\"\n\n[[rom]]\nfile = \"other.elf\"\n";
        assert!(parse_pins(partial).is_empty());
        let unknown = "[[rom]]\nfile = \"other.elf\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";
        assert!(parse_pins(unknown).is_empty());
    }

    #[test]
    fn selection_follows_chip_revision() {
        let mut efuse = EfuseImage::synth(0);
        assert_eq!(RomRev::for_efuse(&efuse), Ok(RomRev::Rev101));
        let missing = Err(PinError::NoBundledRom);
        for (major, minor, want) in [
            (1, 9, Ok(RomRev::Rev101)),
            (0, 4, Ok(RomRev::Rev3)),
            (0, 3, Ok(RomRev::Rev3)),
            (0, 0, missing.clone()),
            (0, 2, missing.clone()),
            (2, 0, missing),
        ] {
            efuse.set_chip_revision(Revision::new(major, minor));
            assert_eq!(RomRev::for_efuse(&efuse), want, "v{major}.{minor}");
        }
        assert_eq!(PinError::NoBundledRom.code_name(), "E_ASSET_MISSING");
        assert_eq!(PinError::Unpinned.code_name(), "E_ASSET_HASH");
    }

    #[cfg(feature = "bundled-rom")]
    #[test]
    fn embedded_bytes_match_pins_and_assemble_to_goldens() {
        let cases = [
            (RomRev::Rev101, GOLDEN_REV101, 0x5_9AA0, 0x5_9F50),
            (RomRev::Rev3, GOLDEN_REV3, 0x5_9664, 0x5_9AC4),
        ];
        for (rev, golden, static_dram, init_end) in cases {
            let bytes = bundled(rev);
            let pin = pins().into_iter().find(|p| p.rev == rev).unwrap();
            assert_eq!(sha256(bytes), pin.sha256, "{rev:?}");
            let rom = check_pin(bytes).unwrap();
            assert_eq!(rom.pinned(), Some(rev));
            assert_eq!(rom.bytes().len(), ROM_LEN);
            assert_eq!(hex(&rom.image_sha256()), golden, "{rev:?}");
            let find = |name: &str| rom.placements().iter().find(|p| p.name == name).unwrap();
            let i = find(".rodata.interface");
            let want = (PlacementKind::Drom, 0x3FF1_EE3C, 0x5_EE3C, 0x11C4);
            assert_eq!((i.kind, i.vma, i.offset, i.size), want, "{rev:?}");
            assert_eq!(find(STATIC_DRAM_START).offset, static_dram, "{rev:?}");
            let end = rom
                .placements()
                .iter()
                .filter(|p| p.kind == PlacementKind::DataInit)
                .map(|p| p.offset + p.size as usize)
                .max();
            assert_eq!(end, Some(init_end), "{rev:?}");
        }
    }

    #[cfg(feature = "bundled-rom")]
    #[test]
    fn a_changed_byte_is_unpinned() {
        let mut copy = bundled(RomRev::Rev101).to_vec();
        let info = ElfInfo::parse(&copy).unwrap();
        let text = info
            .sections
            .iter()
            .find(|s| s.flags & SHF_EXECINSTR != 0)
            .unwrap();
        copy[text.offset as usize + 0x100] ^= 1;
        assert_eq!(check_pin(&copy).err(), Some(PinError::Unpinned));
        let rom = RomImage::from_elf(&copy).unwrap();
        assert_eq!(rom.pinned(), None);
        assert_ne!(hex(&rom.image_sha256()), GOLDEN_REV101);
    }

    /// The proof runs at load, not only in this test.
    #[test]
    fn the_load_time_proof_reads_the_range_from_the_spec() {
        assert_eq!(magic_range(), Some((0x4005_ECC0, 0x4005_EE3B)));
        let failed = PinError::MagicRange(UnusedRangeError::NonZero(0x4005_ECC0));
        assert_eq!(failed.code_name(), "E_INTERNAL");
        assert!(failed.to_string().contains("specs/magic-pcs.toml"));
        assert!(failed.to_string().contains("0x4005ecc0"));
    }

    #[test]
    fn bundled_opt_reports_this_builds_embedding_state() {
        let embedded = cfg!(feature = "bundled-rom");
        for rev in RomRev::ALL {
            assert_eq!(bundled_opt(rev).is_some(), embedded, "{rev:?}");
            #[cfg(feature = "bundled-rom")]
            assert_eq!(bundled_opt(rev), Some(bundled(rev)), "{rev:?}");
        }
    }

    #[cfg(feature = "bundled-rom")]
    #[test]
    fn magic_range_static_proof() {
        let spec = include_str!("../../../specs/magic-pcs.toml");
        let value = |key| hex_u32(spec_value(spec, key).unwrap());
        let (start, end) = (value("start"), value("end"));
        let (rodata_end, rodata_interface) = (value("rodata_end"), value("rodata_interface"));
        assert_eq!(
            (start, end, end - start + 1),
            (0x4005_ECC0, 0x4005_EE3B, 380)
        );
        let rom = check_pin(bundled(RomRev::Rev101)).unwrap();
        assert_eq!(
            Some(hex(rom.elf_sha256()).as_str()),
            spec_value(spec, "rom_sha256")
        );
        assert_eq!(rom.check_unused(start, end), Ok(()));
        // The range is exactly the gap between `_rodata_end` and `.rodata.interface`.
        let first = RomImage::offset_of(start).unwrap();
        assert_eq!(rom.symbols().addr_of("_rodata_end"), Some(rodata_end));
        assert_eq!(RomImage::offset_of(rodata_end), Some(first));
        let before = rom
            .placements()
            .iter()
            .rev()
            .find(|p| p.offset < first)
            .unwrap();
        assert_eq!(before.offset + before.size as usize, first);
        let after = rom.placements().iter().find(|p| p.offset > first).unwrap();
        assert_eq!(
            (after.name.as_str(), after.vma),
            (".rodata.interface", rodata_interface)
        );
        assert_eq!(
            RomImage::offset_of(rodata_interface),
            RomImage::offset_of(end + 1)
        );
        // Nonzero bytes bound the zero run on both sides.
        assert_ne!(rom.read(start - 1, 1), Some(&[0u8][..]));
        assert_ne!(rom.read(end + 1, 1), Some(&[0u8][..]));
        let magic: Vec<u32> = spec
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pc = "))
            .map(hex_u32)
            .collect();
        assert_eq!(
            magic,
            [
                0x4005_ECC0,
                0x4005_ECC4,
                0x4005_ECC8,
                0x4005_ECCC,
                0x4005_ECD0
            ]
        );
        let rev3 = check_pin(bundled(RomRev::Rev3)).unwrap();
        assert_eq!(rev3.check_unused(start, end), Ok(()));
        assert!(matches!(
            rom.check_unused(start, end + 1),
            Err(UnusedRangeError::Section(_))
        ));
        assert!(matches!(
            rom.check_unused(start - 1, end),
            Err(UnusedRangeError::Section(_))
        ));
        assert_eq!(
            rom.check_unused(end, start),
            Err(UnusedRangeError::OutsideRom)
        );
        assert_eq!(
            rom.check_unused(0x3FC8_0000, 0x3FC8_0010),
            Err(UnusedRangeError::OutsideRom)
        );
    }

    #[cfg(feature = "bundled-rom")]
    #[test]
    fn rom_pins_keys_are_pinned() {
        let spec = include_str!("../../../specs/rom-pins.toml");
        let (mut keys, mut current) = (0, None);
        for line in spec.lines().map(str::trim) {
            if let Some(key) = line.strip_prefix("[rom.").and_then(|l| l.strip_suffix(']')) {
                let pin = pins().into_iter().find(|p| hex(&p.sha256) == key);
                let pin = pin.unwrap_or_else(|| panic!("{key} is not in assets/rom/pins.toml"));
                current = Some((pin.file, check_pin(bundled(pin.rev)).unwrap()));
                keys += 1;
            } else if let Some(file) = line.strip_prefix("file = ") {
                assert_eq!(Some(file.trim_matches('"')), current.as_ref().map(|c| c.0));
            } else if line.starts_with("{ name") {
                let field = |key: &str| {
                    line.split(',').find_map(|kv| {
                        let kv = kv.trim().trim_start_matches('{').trim();
                        kv.strip_prefix(key)?
                            .trim()
                            .strip_prefix('=')
                            .map(str::trim)
                    })
                };
                let (pc, insn) = (
                    hex_u32(field("pc").unwrap()),
                    hex_u32(field("insn").unwrap()),
                );
                let rom = &current.as_ref().unwrap().1;
                let word = rom.read(pc, 4).unwrap();
                assert_eq!(
                    u32::from_le_bytes(word.try_into().unwrap()),
                    insn,
                    "{pc:#x}"
                );
                assert!(
                    rom.symbols().func_at(pc).is_some(),
                    "{pc:#x} is inside a function"
                );
            }
        }
        assert_eq!(keys, pins().len());
    }
}
