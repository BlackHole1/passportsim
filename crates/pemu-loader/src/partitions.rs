//! ESP-IDF partition table in the binary format of `components/partition_table/gen_esp32part.py`
//! (ESP-IDF v5.5.3): 32-byte entries `<2sBBLL16sL` with magic `AA 50`, an optional MD5 row (`EB EB`,
//! 14 bytes `FF`, then the MD5 of every entry before it) and a row of `FF` bytes that ends the
//! table.

use md5::{Digest, Md5};

use crate::{LoadError, c_str, hex, le_u32, slice};

/// Offset in a merged flash image.
pub const TABLE_OFFSET: usize = 0x8000;
/// Largest table the IDF tools accept (`MAX_PARTITION_LENGTH` in `gen_esp32part.py`).
pub const TABLE_MAX_LEN: usize = 0xC00;
const ENTRY_LEN: usize = 32;
const ENTRY_MAGIC: [u8; 2] = [0xAA, 0x50];
const MD5_MAGIC: [u8; 2] = [0xEB, 0xEB];

/// Partition types (`TYPES` in `gen_esp32part.py`).
pub mod ptype {
    pub const APP: u8 = 0x00;
    pub const DATA: u8 = 0x01;
}

/// Partition subtypes used by the corpus (`SUBTYPES` in `gen_esp32part.py`).
pub mod subtype {
    /// App types.
    pub const FACTORY: u8 = 0x00;
    /// Slot n is `OTA_0 + n`.
    pub const OTA_0: u8 = 0x10;
    /// Data types.
    pub const OTA_DATA: u8 = 0x00;
    pub const PHY: u8 = 0x01;
    pub const NVS: u8 = 0x02;
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Partition {
    pub name: String,
    pub ptype: u8,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
}

impl Partition {
    pub fn end(&self) -> u64 {
        u64::from(self.offset) + u64::from(self.size)
    }

    /// Flag bit 0.
    pub fn encrypted(&self) -> bool {
        self.flags & 1 != 0
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PartitionTable {
    pub entries: Vec<Partition>,
    /// True when the table carried an MD5 row, which then matched.
    pub has_md5: bool,
}

impl PartitionTable {
    /// As in `gen_esp32part.py`, a mismatched MD5 row, a bad entry magic, or no end row within
    /// [`TABLE_MAX_LEN`] bytes is an error.
    pub fn parse(bytes: &[u8]) -> Result<PartitionTable, LoadError> {
        let mut entries = Vec::new();
        let mut has_md5 = false;
        let mut offset = 0;
        while offset + ENTRY_LEN <= TABLE_MAX_LEN {
            let row = slice(bytes, offset, ENTRY_LEN, "partition table")?;
            if row.iter().all(|&b| b == 0xFF) {
                return Ok(PartitionTable { entries, has_md5 });
            }
            if row[..2] == MD5_MAGIC {
                if row[2..16].iter().any(|&b| b != 0xFF) {
                    return Err(malformed(format!(
                        "MD5 row at {offset:#x} has no FF padding"
                    )));
                }
                let digest: [u8; 16] = Md5::digest(&bytes[..offset]).into();
                if digest[..] != row[16..] {
                    return Err(malformed(format!(
                        "MD5 mismatch: computed {}, stored {}",
                        hex(&digest),
                        hex(&row[16..])
                    )));
                }
                has_md5 = true;
            } else if row[..2] == ENTRY_MAGIC {
                entries.push(Partition {
                    name: c_str(&row[12..28]),
                    ptype: row[2],
                    subtype: row[3],
                    offset: le_u32(row, 4, "partition entry")?,
                    size: le_u32(row, 8, "partition entry")?,
                    flags: le_u32(row, 28, "partition entry")?,
                });
            } else {
                return Err(LoadError::BadMagic {
                    what: "partition entry",
                    offset,
                    found: u32::from(u16::from_be_bytes([row[0], row[1]])),
                });
            }
            offset += ENTRY_LEN;
        }
        Err(malformed(format!(
            "no end-of-table row within {TABLE_MAX_LEN:#x} bytes"
        )))
    }

    pub fn from_flash(flash: &[u8]) -> Result<PartitionTable, LoadError> {
        let len = flash
            .len()
            .saturating_sub(TABLE_OFFSET)
            .clamp(ENTRY_LEN, TABLE_MAX_LEN);
        PartitionTable::parse(slice(flash, TABLE_OFFSET, len, "partition table")?)
    }

    pub fn find(&self, name: &str) -> Option<&Partition> {
        self.entries.iter().find(|p| p.name == name)
    }

    /// The app the IDF bootloader boots without OTA data: the factory app, else the lowest OTA
    /// slot.
    pub fn boot_app(&self) -> Option<&Partition> {
        let apps = || self.entries.iter().filter(|p| p.ptype == ptype::APP);
        apps().find(|p| p.subtype == subtype::FACTORY).or_else(|| {
            apps()
                .filter(|p| p.subtype >= subtype::OTA_0)
                .min_by_key(|p| p.subtype)
        })
    }
}

fn malformed(detail: String) -> LoadError {
    LoadError::Malformed {
        what: "partition table",
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ptype: u8, subtype: u8, offset: u32, size: u32) -> Vec<u8> {
        let mut row = ENTRY_MAGIC.to_vec();
        row.extend_from_slice(&[ptype, subtype]);
        row.extend_from_slice(&offset.to_le_bytes());
        row.extend_from_slice(&size.to_le_bytes());
        let mut field = [0u8; 16];
        field[..name.len()].copy_from_slice(name.as_bytes());
        row.extend_from_slice(&field);
        row.extend_from_slice(&0u32.to_le_bytes());
        row
    }

    fn table(with_md5: bool) -> Vec<u8> {
        let mut t = entry("nvs", ptype::DATA, subtype::NVS, 0x9000, 0x6000);
        t.extend(entry(
            "factory",
            ptype::APP,
            subtype::FACTORY,
            0x10000,
            0x300000,
        ));
        if with_md5 {
            let digest: [u8; 16] = Md5::digest(&t).into();
            t.extend_from_slice(&MD5_MAGIC);
            t.extend_from_slice(&[0xFF; 14]);
            t.extend_from_slice(&digest);
        }
        t.resize(TABLE_MAX_LEN, 0xFF);
        t
    }

    #[test]
    fn parses_entries_and_md5() {
        let t = PartitionTable::parse(&table(true)).unwrap();
        assert!(t.has_md5);
        assert_eq!(t.entries.len(), 2);
        let nvs = t.find("nvs").unwrap();
        assert_eq!(
            (nvs.ptype, nvs.subtype, nvs.offset, nvs.size),
            (1, 2, 0x9000, 0x6000)
        );
        assert_eq!(t.boot_app().unwrap().name, "factory");
        assert!(!PartitionTable::parse(&table(false)).unwrap().has_md5);
    }

    #[test]
    fn rejects_bad_md5_magic_and_missing_end() {
        let mut bad_md5 = table(true);
        bad_md5[2 * ENTRY_LEN + 20] ^= 1;
        assert!(matches!(
            PartitionTable::parse(&bad_md5),
            Err(LoadError::Malformed { .. })
        ));
        let mut bad_magic = table(false);
        bad_magic[ENTRY_LEN] = 0x12;
        assert!(matches!(
            PartitionTable::parse(&bad_magic),
            Err(LoadError::BadMagic { .. })
        ));
        let no_end: Vec<u8> = table(false)[..ENTRY_LEN].repeat(TABLE_MAX_LEN / ENTRY_LEN);
        assert!(PartitionTable::parse(&no_end).is_err());
        assert!(PartitionTable::parse(&table(false)[..40]).is_err());
    }

    #[test]
    fn reads_the_table_of_a_merged_image() {
        let mut flash = vec![0xFFu8; TABLE_OFFSET];
        flash.extend(table(true));
        assert_eq!(PartitionTable::from_flash(&flash).unwrap().entries.len(), 2);
        assert!(PartitionTable::from_flash(&flash[..TABLE_OFFSET]).is_err());
    }
}
