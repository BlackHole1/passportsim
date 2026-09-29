//! The ESP-IDF application descriptor `esp_app_desc_t`
//! (`components/esp_app_format/include/esp_app_desc.h`, ESP-IDF v5.5.3). In an app image it
//! starts the first (DROM) segment; in the app ELF it is the section [`APP_DESC_SECTION`].

use crate::{LoadError, c_str, le_u16, le_u32, slice};

/// `ESP_APP_DESC_MAGIC_WORD`.
pub const APP_DESC_MAGIC: u32 = 0xABCD_5432;
/// `sizeof(esp_app_desc_t)`.
pub const APP_DESC_LEN: usize = 256;
pub const APP_DESC_SECTION: &str = ".flash.appdesc";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AppDesc {
    pub secure_version: u32,
    pub version: String,
    pub project_name: String,
    /// Compile time and date are volatile; golden normalizers mask them.
    pub time: String,
    pub date: String,
    pub idf_ver: String,
    /// SHA-256 of the ELF file the image was built from.
    pub app_elf_sha256: [u8; 32],
    pub min_efuse_blk_rev_full: u16,
    pub max_efuse_blk_rev_full: u16,
    /// Log2 of the MMU page size.
    pub mmu_page_size: u8,
}

impl AppDesc {
    pub fn parse(bytes: &[u8]) -> Result<AppDesc, LoadError> {
        const WHAT: &str = "app descriptor";
        let b = slice(bytes, 0, APP_DESC_LEN, WHAT)?;
        let magic = le_u32(b, 0, WHAT)?;
        if magic != APP_DESC_MAGIC {
            return Err(LoadError::BadMagic {
                what: WHAT,
                offset: 0,
                found: magic,
            });
        }
        let mut app_elf_sha256 = [0u8; 32];
        app_elf_sha256.copy_from_slice(&b[144..176]);
        Ok(AppDesc {
            secure_version: le_u32(b, 4, WHAT)?,
            version: c_str(&b[16..48]),
            project_name: c_str(&b[48..80]),
            time: c_str(&b[80..96]),
            date: c_str(&b[96..112]),
            idf_ver: c_str(&b[112..144]),
            app_elf_sha256,
            min_efuse_blk_rev_full: le_u16(b, 176, WHAT)?,
            max_efuse_blk_rev_full: le_u16(b, 178, WHAT)?,
            mmu_page_size: b[180],
        })
    }

    /// False when the field is all zero.
    pub fn has_elf_sha256(&self) -> bool {
        self.app_elf_sha256 != [0; 32]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(buf: &mut [u8], at: usize, s: &[u8]) {
        buf[at..at + s.len()].copy_from_slice(s);
    }

    #[test]
    fn decodes_every_field() {
        let mut d = [0u8; APP_DESC_LEN];
        put(&mut d, 0, &APP_DESC_MAGIC.to_le_bytes());
        put(&mut d, 4, &7u32.to_le_bytes());
        put(&mut d, 16, b"f75873f");
        put(&mut d, 48, b"FoloToy-AI-Passport");
        put(&mut d, 80, b"12:00:00");
        put(&mut d, 96, b"Jan  1 2026");
        put(&mut d, 112, b"v5.5.3");
        put(&mut d, 144, &[0xAB; 32]);
        put(&mut d, 176, &3u16.to_le_bytes());
        put(&mut d, 178, &199u16.to_le_bytes());
        d[180] = 16;
        let a = AppDesc::parse(&d).unwrap();
        assert_eq!(a.secure_version, 7);
        assert_eq!(a.version, "f75873f");
        assert_eq!(a.project_name, "FoloToy-AI-Passport");
        assert_eq!(a.idf_ver, "v5.5.3");
        assert_eq!(
            (a.time.as_str(), a.date.as_str()),
            ("12:00:00", "Jan  1 2026")
        );
        assert_eq!(a.app_elf_sha256, [0xAB; 32]);
        assert!(a.has_elf_sha256());
        assert_eq!(
            (a.min_efuse_blk_rev_full, a.max_efuse_blk_rev_full),
            (3, 199)
        );
        assert_eq!(a.mmu_page_size, 16);
    }

    #[test]
    fn rejects_bad_magic_and_short_input() {
        let d = [0u8; APP_DESC_LEN];
        assert!(matches!(
            AppDesc::parse(&d),
            Err(LoadError::BadMagic { found: 0, .. })
        ));
        assert!(matches!(
            AppDesc::parse(&d[..10]),
            Err(LoadError::Truncated { .. })
        ));
    }
}
