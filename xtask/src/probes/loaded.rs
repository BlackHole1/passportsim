//! Proof that the committed ELF of a probe loads exactly what the build's own ELF loads.
//!
//! ESP-IDF's linker script reserves two address ranges with `NOBITS` placeholder sections,
//! `.flash_rodata_dummy` (the rodata view of the flash pages the code occupies) and `.dram0.dummy`
//! (the DRAM view of IRAM). They hold no bytes, but they sit inside `PT_LOAD` segments, so the file
//! has to carry zero padding for them: about 900 KB in a radio probe, which is what put the stripped
//! `scan3` ELF at 1.9 MB while its app image is 1,001,920 bytes. `riscv32-esp-elf-strip -R` drops
//! the two sections and the padding with them.
//!
//! That is only acceptable if nothing that is loaded changes, and this module checks it on every
//! build rather than trusting the command: every allocated section that has contents in the
//! original ELF must be in the stripped ELF at the same address with the same bytes, no allocated
//! section with contents may appear, a section may disappear only if it holds no bytes, and the
//! entry point must be the same. `esptool elf2image` builds the app image from exactly those
//! sections, so the image built from either ELF is the same apart from the ELF SHA-256 it embeds.

use pemu_loader::elf::ElfInfo;

/// The placeholder sections the strip step removes.
pub const DUMMY_SECTIONS: [&str; 2] = [".flash_rodata_dummy", ".dram0.dummy"];

/// Checks that `stripped` loads the same bytes at the same addresses as `original`.
pub fn same_loaded_image(original: &[u8], stripped: &[u8]) -> Result<(), String> {
    let before = ElfInfo::parse(original).map_err(|err| format!("original ELF: {err}"))?;
    let after = ElfInfo::parse(stripped).map_err(|err| format!("stripped ELF: {err}"))?;
    let mut problems = Vec::new();
    if before.entry != after.entry {
        problems.push(format!(
            "entry 0x{:08x} became 0x{:08x}",
            before.entry, after.entry
        ));
    }
    for section in before.sections.iter().filter(|s| s.is_alloc()) {
        let kept = after.sections.iter().find(|s| s.name == section.name);
        match (section.has_bits() && section.size > 0, kept) {
            (true, None) => problems.push(format!("{} (loaded) was removed", section.name)),
            (false, None) => {}
            (_, Some(kept)) => {
                if kept.addr != section.addr
                    || kept.size != section.size
                    || kept.sh_type != section.sh_type
                {
                    problems.push(format!(
                        "{} moved: 0x{:08x}+{} became 0x{:08x}+{}",
                        section.name, section.addr, section.size, kept.addr, kept.size
                    ));
                } else if section.data(original) != kept.data(stripped) {
                    problems.push(format!("{} changed contents", section.name));
                }
            }
        }
    }
    for section in after.sections.iter().filter(|s| s.is_alloc()) {
        if !before.sections.iter().any(|s| s.name == section.name) {
            problems.push(format!("{} appeared", section.name));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the stripped ELF does not load the same image: {}",
            problems.join("; ")
        ))
    }
}
