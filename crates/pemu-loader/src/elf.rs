//! ELF files of the app, the bootloader and the ROM: section and program headers, the whole symbol
//! table (local symbols included), the file SHA-256 and, for an app, the `esp_app_desc_t` in
//! `.flash.appdesc`. Only 32-bit little-endian RISC-V files are accepted.

use object::read::elf::{ElfFile32, ProgramHeader, SectionHeader, Sym};
use object::{Architecture, LittleEndian, Object, ObjectSection, ObjectSymbol, elf};

use crate::app_desc::{APP_DESC_SECTION, AppDesc};
use crate::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};
use crate::{LoadError, sha256};

pub const SHT_PROGBITS: u32 = 1;
pub const SHT_NOBITS: u32 = 8;
pub const SHF_WRITE: u64 = 1;
pub const SHF_ALLOC: u64 = 2;
pub const SHF_EXECINSTR: u64 = 4;
pub const PT_LOAD: u32 = 1;
pub const PF_X: u32 = 1;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ElfSection {
    pub index: usize,
    pub name: String,
    pub sh_type: u32,
    pub flags: u64,
    pub addr: u32,
    /// File offset of the contents (meaningless for `SHT_NOBITS`).
    pub offset: u32,
    pub size: u32,
    pub align: u32,
}

impl ElfSection {
    pub fn is_alloc(&self) -> bool {
        self.flags & SHF_ALLOC != 0
    }

    /// True when the file holds contents for this section.
    pub fn has_bits(&self) -> bool {
        self.sh_type != SHT_NOBITS && self.sh_type != 0
    }

    /// Exclusive end address.
    pub fn end(&self) -> u64 {
        u64::from(self.addr) + u64::from(self.size)
    }

    /// `file` must be the bytes this section was parsed from.
    pub fn data<'a>(&self, file: &'a [u8]) -> Option<&'a [u8]> {
        if !self.has_bits() {
            return None;
        }
        let start = self.offset as usize;
        file.get(start..start.checked_add(self.size as usize)?)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ElfSegment {
    pub p_type: u32,
    pub flags: u32,
    pub offset: u32,
    pub vaddr: u32,
    pub paddr: u32,
    pub filesz: u32,
    pub memsz: u32,
}

impl ElfSegment {
    pub fn is_load(&self) -> bool {
        self.p_type == PT_LOAD
    }

    pub fn is_exec(&self) -> bool {
        self.flags & PF_X != 0
    }
}

/// A parsed app, bootloader or ROM ELF. It keeps no reference to the file bytes.
/// UNVERIFIED placeholder: no design fixes this type's shape.
#[derive(Clone, Debug)]
pub struct ElfInfo {
    /// SHA-256 of the whole file; an app image's descriptor carries the same value.
    pub sha256: [u8; 32],
    pub entry: u32,
    pub sections: Vec<ElfSection>,
    pub segments: Vec<ElfSegment>,
    pub symbols: SymbolTable,
    pub app_desc: Option<AppDesc>,
}

impl ElfInfo {
    /// UNVERIFIED signature: no design fixes this function's shape.
    pub fn parse(bytes: &[u8]) -> Result<ElfInfo, LoadError> {
        let file =
            ElfFile32::<LittleEndian>::parse(bytes).map_err(|e| LoadError::Elf(e.to_string()))?;
        if file.architecture() != Architecture::Riscv32 {
            return Err(LoadError::Malformed {
                what: "ELF",
                detail: format!("architecture {:?}, expected RISC-V 32", file.architecture()),
            });
        }
        let endian = file.endian();
        let elf_err = |e: object::Error| LoadError::Elf(e.to_string());

        let mut sections = Vec::new();
        for s in file.sections() {
            let h = s.elf_section_header();
            sections.push(ElfSection {
                index: s.index().0,
                name: s.name().map_err(elf_err)?.to_string(),
                sh_type: h.sh_type(endian).0,
                flags: h.sh_flags(endian).0,
                addr: h.sh_addr(endian),
                offset: h.sh_offset(endian),
                size: h.sh_size(endian),
                align: h.sh_addralign(endian),
            });
        }

        let segments = file
            .elf_program_headers()
            .iter()
            .map(|p| ElfSegment {
                p_type: p.p_type(endian).0,
                flags: p.p_flags(endian).0,
                offset: p.p_offset(endian),
                vaddr: p.p_vaddr(endian),
                paddr: p.p_paddr(endian),
                filesz: p.p_filesz(endian),
                memsz: p.p_memsz(endian),
            })
            .collect();

        let mut syms = Vec::new();
        for s in file.symbols() {
            let raw = s.elf_symbol();
            let t = raw.st_type();
            let kind = if t == elf::STT_FUNC {
                SymKind::Func
            } else if t == elf::STT_OBJECT {
                SymKind::Object
            } else if t == elf::STT_SECTION {
                SymKind::Section
            } else if t == elf::STT_FILE {
                SymKind::File
            } else if t == elf::STT_TLS {
                SymKind::Tls
            } else {
                SymKind::NoType
            };
            let b = raw.st_bind();
            let bind = if b == elf::STB_LOCAL {
                SymBind::Local
            } else if b == elf::STB_WEAK {
                SymBind::Weak
            } else {
                SymBind::Global
            };
            let section = match s.section() {
                object::SymbolSection::Undefined => SymSection::Undefined,
                object::SymbolSection::Absolute => SymSection::Absolute,
                object::SymbolSection::Common => SymSection::Common,
                object::SymbolSection::Section(i) => SymSection::Index(i.0),
                _ => SymSection::Other,
            };
            syms.push(Symbol {
                name: s.name().map_err(elf_err)?.to_string(),
                addr: raw.st_value(endian),
                size: raw.st_size(endian),
                kind,
                bind,
                section,
            });
        }

        let app_desc = sections
            .iter()
            .find(|s| s.name == APP_DESC_SECTION)
            .and_then(|s| s.data(bytes))
            .and_then(|d| AppDesc::parse(d).ok());

        Ok(ElfInfo {
            sha256: sha256(bytes),
            entry: file.entry() as u32,
            sections,
            segments,
            symbols: SymbolTable::new(syms),
            app_desc,
        })
    }

    pub fn section(&self, name: &str) -> Option<&ElfSection> {
        self.sections.iter().find(|s| s.name == name)
    }
}
