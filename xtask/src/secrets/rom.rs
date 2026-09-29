//! The `rom-pin` rule.
//!
//! A binary file under `assets/rom/` passes only when it is an ELF, its SHA-256 is listed as
//! a `[[rom]] sha256` entry of `assets/rom/pins.toml`, and `assets/rom/LICENSE` and
//! `assets/rom/NOTICE` exist. Any other binary there fails. Text files there (the license
//! files, `pins.toml`, notes) are left to the other rules. A pinned ROM ELF is exempt from the
//! content rules, because the pin proves its bytes are the public upstream release.

use std::path::Path;

/// The bundled ROM directory, relative to the repository root.
pub const ROM_DIR: &str = "assets/rom/";

/// Whether a `/`-separated repository path lies under `assets/rom/`.
pub fn is_under_rom_dir(rel: &str) -> bool {
    rel.starts_with(ROM_DIR)
}

/// Why a binary under `assets/rom/` fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RomFailure {
    NotElf,
    MissingLicense,
    BadPins,
    Unpinned,
}

impl RomFailure {
    /// A short reason, printed after the path. It never contains file content or hashes.
    pub fn note(self) -> &'static str {
        match self {
            RomFailure::NotElf => "binary is not an ELF",
            RomFailure::MissingLicense => "assets/rom/LICENSE or assets/rom/NOTICE missing",
            RomFailure::BadPins => "assets/rom/pins.toml missing or invalid",
            RomFailure::Unpinned => "SHA-256 not listed in assets/rom/pins.toml",
        }
    }
}

/// What the rule knows about `assets/rom/` of one repository root.
#[derive(Debug)]
pub struct RomDir {
    pins: Option<Vec<String>>,
    licensed: bool,
}

impl RomDir {
    /// Reads `pins.toml` and checks `LICENSE` and `NOTICE` under `root/assets/rom/`.
    pub fn load(root: &Path) -> RomDir {
        let dir = root.join(ROM_DIR);
        let pins = std::fs::read_to_string(dir.join("pins.toml"))
            .ok()
            .and_then(|text| parse_pins(&text));
        let licensed = dir.join("LICENSE").is_file() && dir.join("NOTICE").is_file();
        RomDir { pins, licensed }
    }

    /// Checks one binary file under `assets/rom/`.
    pub fn check(&self, bytes: &[u8]) -> Result<(), RomFailure> {
        if !bytes.starts_with(b"\x7fELF") {
            return Err(RomFailure::NotElf);
        }
        if !self.licensed {
            return Err(RomFailure::MissingLicense);
        }
        let pins = self.pins.as_ref().ok_or(RomFailure::BadPins)?;
        let digest = sha256_hex(bytes);
        if pins.contains(&digest) {
            Ok(())
        } else {
            Err(RomFailure::Unpinned)
        }
    }
}

/// The `[[rom]] sha256` values of `pins.toml`, lowercased; `None` when the file does not
/// parse, lists no ROM, or holds a value that is not 64 hex digits.
pub fn parse_pins(text: &str) -> Option<Vec<String>> {
    let table: toml::Table = text.parse().ok()?;
    let roms = table.get("rom")?.as_array()?;
    let mut pins = Vec::with_capacity(roms.len());
    for rom in roms {
        let sha = rom.get("sha256")?.as_str()?.to_ascii_lowercase();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        pins.push(sha);
    }
    (!pins.is_empty()).then_some(pins)
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    pemu_loader::hex(&pemu_loader::sha256(bytes))
}
