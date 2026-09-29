//! The eFuse image as raw block words, BLK0 to BLK10. Field positions are block-relative bit
//! numbers from ESP-IDF v5.5.3 `components/efuse/esp32c3/esp_efuse_table.csv`. The synthesized
//! default follows `specs/notes/g3-behavior.md` (g3-synth-efuse); an imported dump carries device
//! identity and taints the machine.

use core::fmt;

use crate::{LoadError, sha256};

pub const EFUSE_BLOCKS: usize = 11;
pub const WORDS_PER_BLOCK: usize = 8;
/// Words the read registers expose per block: BLK0 `RD_WR_DIS` plus `RD_REPEAT_DATA0` to 4, BLK1
/// `RD_MAC_SPI_SYS_0` to 5, then 8 each (`efuse_reg.h`).
pub const BLOCK_WORDS: [usize; EFUSE_BLOCKS] = [6, 6, 8, 8, 8, 8, 8, 8, 8, 8, 8];
/// Every block in read-register order.
pub const DUMP_WORDS: usize = 84;
/// Offset of `EFUSE_RD_WR_DIS_REG` in the eFuse register block, where a whole-image dump starts.
pub const RD_REG_OFFSET: usize = 0x2c;

/// An `esp_efuse_table.csv` row: block, bit start, bit count.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct EfuseField {
    pub block: u8,
    pub bit: u16,
    pub width: u8,
}

pub mod field {
    use super::EfuseField;

    /// `MAC_FACTORY`, one row per byte: `MAC[0]` at bit 40 down to `MAC[5]` at bit 0
    /// (`esp_efuse_table.csv` rows 147 to 152).
    pub const MAC: [EfuseField; 6] = [
        EfuseField {
            block: 1,
            bit: 40,
            width: 8,
        },
        EfuseField {
            block: 1,
            bit: 32,
            width: 8,
        },
        EfuseField {
            block: 1,
            bit: 24,
            width: 8,
        },
        EfuseField {
            block: 1,
            bit: 16,
            width: 8,
        },
        EfuseField {
            block: 1,
            bit: 8,
            width: 8,
        },
        EfuseField {
            block: 1,
            bit: 0,
            width: 8,
        },
    ];
    /// `OPTIONAL_UNIQUE_ID` is wider than one word: read it through
    /// [`super::EfuseImage::unique_id`].
    pub const OPTIONAL_UNIQUE_ID_BLOCK: u8 = 2;
    pub const OPTIONAL_UNIQUE_ID_WORDS: core::ops::Range<usize> = 0..4;
    pub const ERR_RST_ENABLE: EfuseField = EfuseField {
        block: 0,
        bit: 159,
        width: 1,
    };
    pub const PKG_VERSION: EfuseField = EfuseField {
        block: 1,
        bit: 117,
        width: 3,
    };
    pub const FLASH_CAP: EfuseField = EfuseField {
        block: 1,
        bit: 123,
        width: 3,
    };
    pub const FLASH_TEMP: EfuseField = EfuseField {
        block: 1,
        bit: 126,
        width: 2,
    };
    pub const FLASH_VENDOR: EfuseField = EfuseField {
        block: 1,
        bit: 128,
        width: 3,
    };
    pub const WAFER_VERSION_MINOR_LO: EfuseField = EfuseField {
        block: 1,
        bit: 114,
        width: 3,
    };
    pub const WAFER_VERSION_MINOR_HI: EfuseField = EfuseField {
        block: 1,
        bit: 183,
        width: 1,
    };
    pub const WAFER_VERSION_MAJOR: EfuseField = EfuseField {
        block: 1,
        bit: 184,
        width: 2,
    };
    pub const BLK_VERSION_MINOR: EfuseField = EfuseField {
        block: 1,
        bit: 120,
        width: 3,
    };
    pub const BLK_VERSION_MAJOR: EfuseField = EfuseField {
        block: 2,
        bit: 128,
        width: 2,
    };

    pub const fn at(block: u8, bit: u16, width: u8) -> EfuseField {
        EfuseField { block, bit, width }
    }

    pub const K_RTC_LDO: EfuseField = at(1, 135, 7);
    pub const K_DIG_LDO: EfuseField = at(1, 142, 7);
    pub const V_RTC_DBIAS20: EfuseField = at(1, 149, 8);
    pub const V_DIG_DBIAS20: EfuseField = at(1, 157, 8);
    pub const DIG_DBIAS_HVT: EfuseField = at(1, 165, 5);
    pub const THRES_HVT: EfuseField = at(1, 170, 10);
    pub const TEMP_CALIB: EfuseField = at(2, 131, 9);
    pub const OCODE: EfuseField = at(2, 140, 8);
    pub const ADC1_INIT_CODE_ATTEN: [EfuseField; 4] = [
        at(2, 148, 10),
        at(2, 158, 10),
        at(2, 168, 10),
        at(2, 178, 10),
    ];
    pub const ADC1_CAL_VOL_ATTEN: [EfuseField; 4] = [
        at(2, 188, 10),
        at(2, 198, 10),
        at(2, 208, 10),
        at(2, 218, 10),
    ];

    /// Every calibration field the synthesized image keeps 0.
    pub const CALIBRATION: [EfuseField; 16] = [
        K_RTC_LDO,
        K_DIG_LDO,
        V_RTC_DBIAS20,
        V_DIG_DBIAS20,
        DIG_DBIAS_HVT,
        THRES_HVT,
        TEMP_CALIB,
        OCODE,
        ADC1_INIT_CODE_ATTEN[0],
        ADC1_INIT_CODE_ATTEN[1],
        ADC1_INIT_CODE_ATTEN[2],
        ADC1_INIT_CODE_ATTEN[3],
        ADC1_CAL_VOL_ATTEN[0],
        ADC1_CAL_VOL_ATTEN[1],
        ADC1_CAL_VOL_ATTEN[2],
        ADC1_CAL_VOL_ATTEN[3],
    ];
}

/// A `major.minor` revision; `full()` is IDF's `major * 100 + minor`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Revision {
    pub major: u8,
    pub minor: u8,
}

impl Revision {
    pub const fn new(major: u8, minor: u8) -> Revision {
        Revision { major, minor }
    }

    pub fn full(self) -> u16 {
        u16::from(self.major) * 100 + u16::from(self.minor)
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
}

pub const SYNTH_CHIP_REVISION: Revision = Revision::new(1, 1);
pub const SYNTH_BLOCK_REVISION: Revision = Revision::new(1, 3);
/// Locally administered unicast prefix of the placeholder MAC `02:00:00:xx:xx:xx`.
pub const SYNTH_MAC_PREFIX: [u8; 3] = [0x02, 0x00, 0x00];
/// The board's 8 MB embedded flash.
pub const SYNTH_FLASH_CAP_8M: u32 = 4;
pub const SYNTH_FLASH_TEMP_105C: u32 = 1;
pub const SYNTH_FLASH_VENDOR_XMC: u32 = 1;
/// The placeholder MAC of the reference image in `specs/notes/g3-behavior.md`; a run that must
/// reproduce those words sets it through [`EfuseImage::set_mac`].
pub const G3_SYNTH_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0xc3, 0x00, 0x01];

/// A dump carries device identity, so it taints the machine.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EfuseOrigin {
    Synth,
    Dump,
}

/// UNVERIFIED placeholder: no design fixes this type's shape.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EfuseImage {
    words: [[u32; WORDS_PER_BLOCK]; EFUSE_BLOCKS],
    origin: EfuseOrigin,
}

impl EfuseImage {
    /// Every bit 0, counted as synthesized.
    pub fn blank() -> EfuseImage {
        EfuseImage {
            words: [[0; WORDS_PER_BLOCK]; EFUSE_BLOCKS],
            origin: EfuseOrigin::Synth,
        }
    }

    /// The reference image of `specs/notes/g3-behavior.md` (g3-synth-efuse), which boots every
    /// corpus image on the ECO7 ROM: wafer v1.1, block v1.3, 8 MB XMC flash, calibration fields 0.
    /// Only the MAC's last three bytes and `OPTIONAL_UNIQUE_ID` depend on the seed; both are
    /// synthetic, never a device identity.
    /// UNVERIFIED signature: no design fixes this function's shape.
    pub fn synth(seed: u64) -> EfuseImage {
        let mut img = EfuseImage::blank();
        img.set(field::ERR_RST_ENABLE, 1);
        img.set_mac(synth_mac(seed));
        img.set_chip_revision(SYNTH_CHIP_REVISION);
        img.set(field::PKG_VERSION, 0);
        img.set_block_revision(SYNTH_BLOCK_REVISION);
        img.set(field::FLASH_CAP, SYNTH_FLASH_CAP_8M);
        img.set(field::FLASH_TEMP, SYNTH_FLASH_TEMP_105C);
        img.set(field::FLASH_VENDOR, SYNTH_FLASH_VENDOR_XMC);
        img.set_unique_id(synth_unique_id(seed));
        img
    }

    /// Imports the read registers of every block in order, little-endian words, as espefuse
    /// writes them. Accepted: [`DUMP_WORDS`] words, the same zero-padded to 1 KiB (byte offset =
    /// register offset minus [`RD_REG_OFFSET`]), or a flat [`EFUSE_BLOCKS`] x [`WORDS_PER_BLOCK`]
    /// image.
    /// UNVERIFIED signature: no design fixes this function's shape.
    pub fn from_dump(bytes: &[u8]) -> Result<EfuseImage, LoadError> {
        let flat = match bytes.len() {
            n if n == DUMP_WORDS * 4 || n == 1024 => false,
            n if n == EFUSE_BLOCKS * WORDS_PER_BLOCK * 4 => true,
            _ => {
                return Err(LoadError::Malformed {
                    what: "eFuse dump",
                    detail: format!(
                        "{} bytes: expected {} (read registers), 1024 (padded) or {} (flat blocks)",
                        bytes.len(),
                        DUMP_WORDS * 4,
                        EFUSE_BLOCKS * WORDS_PER_BLOCK * 4
                    ),
                });
            }
        };
        let mut img = EfuseImage::blank();
        let mut offset = 0;
        for (block, &words) in BLOCK_WORDS.iter().enumerate() {
            let words = if flat { WORDS_PER_BLOCK } else { words };
            for word in 0..words {
                img.words[block][word] = crate::le_u32(bytes, offset, "eFuse dump")?;
                offset += 4;
            }
        }
        img.origin = EfuseOrigin::Dump;
        Ok(img)
    }

    /// Imports per-block files (`efuse_blk0.bin` to `efuse_blk10.bin`) as block index and
    /// little-endian words. An unlisted block stays 0; a short one fills its first words.
    pub fn from_blocks(blocks: &[(u8, &[u8])]) -> Result<EfuseImage, LoadError> {
        let mut img = EfuseImage::blank();
        for &(block, bytes) in blocks {
            let index = usize::from(block);
            let words = *BLOCK_WORDS.get(index).ok_or(LoadError::Malformed {
                what: "eFuse block file",
                detail: format!("block {block} does not exist"),
            })?;
            if bytes.len() > words * 4 || bytes.len() % 4 != 0 {
                return Err(LoadError::Malformed {
                    what: "eFuse block file",
                    detail: format!(
                        "block {block}: {} bytes, expected a multiple of 4 up to {}",
                        bytes.len(),
                        words * 4
                    ),
                });
            }
            for (word, chunk) in bytes.chunks_exact(4).enumerate() {
                img.words[index][word] =
                    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            }
        }
        img.origin = EfuseOrigin::Dump;
        Ok(img)
    }

    pub fn origin(&self) -> EfuseOrigin {
        self.origin
    }

    pub fn tainted(&self) -> bool {
        self.origin == EfuseOrigin::Dump
    }

    pub fn words(&self) -> &[[u32; WORDS_PER_BLOCK]; EFUSE_BLOCKS] {
        &self.words
    }

    /// The layout [`EfuseImage::from_dump`] reads and the peripheral exposes from
    /// [`RD_REG_OFFSET`].
    pub fn dump_words(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(DUMP_WORDS);
        for (block, &words) in BLOCK_WORDS.iter().enumerate() {
            out.extend_from_slice(&self.words[block][..words]);
        }
        out
    }

    /// `MAC_FACTORY` as the 6 bytes `esp_read_mac` returns (`MAC[0]` first).
    pub fn mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        for (byte, &f) in mac.iter_mut().zip(field::MAC.iter()) {
            *byte = self.get(f) as u8;
        }
        mac
    }

    pub fn set_mac(&mut self, mac: [u8; 6]) {
        for (&byte, &f) in mac.iter().zip(field::MAC.iter()) {
            self.set(f, u32::from(byte));
        }
    }

    /// `OPTIONAL_UNIQUE_ID`, 128 bits, least significant word first.
    pub fn unique_id(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        let block = &self.words[usize::from(field::OPTIONAL_UNIQUE_ID_BLOCK)];
        for (word, chunk) in field::OPTIONAL_UNIQUE_ID_WORDS.zip(out.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&block[word].to_le_bytes());
        }
        out
    }

    pub fn set_unique_id(&mut self, id: [u8; 16]) {
        let block = &mut self.words[usize::from(field::OPTIONAL_UNIQUE_ID_BLOCK)];
        for (word, chunk) in field::OPTIONAL_UNIQUE_ID_WORDS.zip(id.chunks_exact(4)) {
            block[word] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
    }

    pub fn get(&self, f: EfuseField) -> u32 {
        (0..u16::from(f.width)).fold(0, |acc, i| {
            let bit = usize::from(f.bit + i);
            let w = self.words[usize::from(f.block)][bit / 32];
            acc | (((w >> (bit % 32)) & 1) << i)
        })
    }

    /// Bits of `value` above the field width are dropped.
    pub fn set(&mut self, f: EfuseField, value: u32) {
        for i in 0..u16::from(f.width) {
            let bit = usize::from(f.bit + i);
            let w = &mut self.words[usize::from(f.block)][bit / 32];
            let mask = 1u32 << (bit % 32);
            if (value >> i) & 1 != 0 {
                *w |= mask;
            } else {
                *w &= !mask;
            }
        }
    }

    /// Minor is `WAFER_VERSION_MINOR_HI` (bit 3) over `WAFER_VERSION_MINOR_LO` (bits 2:0).
    pub fn chip_revision(&self) -> Revision {
        let minor = (self.get(field::WAFER_VERSION_MINOR_HI) << 3)
            | self.get(field::WAFER_VERSION_MINOR_LO);
        Revision::new(self.get(field::WAFER_VERSION_MAJOR) as u8, minor as u8)
    }

    /// Sets the wafer revision; major keeps 2 bits and minor 4 bits.
    pub fn set_chip_revision(&mut self, rev: Revision) {
        self.set(field::WAFER_VERSION_MAJOR, u32::from(rev.major));
        self.set(field::WAFER_VERSION_MINOR_LO, u32::from(rev.minor));
        self.set(field::WAFER_VERSION_MINOR_HI, u32::from(rev.minor) >> 3);
    }

    pub fn block_revision(&self) -> Revision {
        Revision::new(
            self.get(field::BLK_VERSION_MAJOR) as u8,
            self.get(field::BLK_VERSION_MINOR) as u8,
        )
    }

    /// Sets the block revision; major keeps 2 bits and minor 3 bits.
    pub fn set_block_revision(&mut self, rev: Revision) {
        self.set(field::BLK_VERSION_MAJOR, u32::from(rev.major));
        self.set(field::BLK_VERSION_MINOR, u32::from(rev.minor));
    }
}

/// [`SYNTH_MAC_PREFIX`] plus three bytes of `SHA-256("passport-emu efuse mac" || seed)`.
pub fn synth_mac(seed: u64) -> [u8; 6] {
    let digest = seeded_digest(b"passport-emu efuse mac", seed);
    let mut mac = [0u8; 6];
    mac[..3].copy_from_slice(&SYNTH_MAC_PREFIX);
    mac[3..].copy_from_slice(&digest[..3]);
    mac
}

/// 16 bytes of `SHA-256("passport-emu efuse unique id" || seed)`.
pub fn synth_unique_id(seed: u64) -> [u8; 16] {
    let digest = seeded_digest(b"passport-emu efuse unique id", seed);
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

fn seeded_digest(domain: &[u8], seed: u64) -> [u8; 32] {
    let mut input = Vec::with_capacity(domain.len() + 8);
    input.extend_from_slice(domain);
    input.extend_from_slice(&seed.to_le_bytes());
    sha256(&input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_land_on_the_table_bits() {
        let mut img = EfuseImage::blank();
        img.set_chip_revision(Revision::new(1, 9));
        // BLK1 bit 114 is word 3 bit 18; bit 183 is word 5 bit 23; bits 184-185 are word 5 bits 24-25.
        assert_eq!(img.words()[1][3], 1 << 18);
        assert_eq!(img.words()[1][5], (1 << 23) | (1 << 24));
        assert_eq!(img.chip_revision(), Revision::new(1, 9));
        img.set_block_revision(Revision::new(1, 3));
        assert_eq!(img.words()[1][3], (1 << 18) | (3 << 24));
        assert_eq!(img.words()[2][4], 1);
        assert_eq!(img.block_revision().full(), 103);
    }

    #[test]
    fn synth_is_chip_v1_1_and_block_v1_3() {
        let img = EfuseImage::synth(7);
        assert_eq!(img.chip_revision().to_string(), "v1.1");
        assert_eq!(img.chip_revision().full(), 101);
        assert_eq!(img.block_revision().to_string(), "v1.3");
        assert_eq!(img.block_revision().full(), 103);
        assert_eq!(img.get(field::PKG_VERSION), 0);
        assert_eq!(img.get(field::FLASH_CAP), SYNTH_FLASH_CAP_8M);
        assert_eq!(img.get(field::FLASH_TEMP), SYNTH_FLASH_TEMP_105C);
        assert_eq!(img.get(field::FLASH_VENDOR), SYNTH_FLASH_VENDOR_XMC);
        assert_eq!(img.get(field::ERR_RST_ENABLE), 1);
        for f in field::CALIBRATION {
            assert_eq!(img.get(f), 0, "{f:?} must stay 0");
        }
        assert_eq!(img.origin(), EfuseOrigin::Synth);
        assert!(!img.tainted());
        let mut v = img.clone();
        v.set(field::WAFER_VERSION_MAJOR, 0xFF);
        assert_eq!(v.get(field::WAFER_VERSION_MAJOR), 3);
    }

    #[test]
    fn synth_mac_and_unique_id_follow_the_seed() {
        let a = EfuseImage::synth(1);
        let b = EfuseImage::synth(2);
        assert_eq!(a.mac()[..3], SYNTH_MAC_PREFIX);
        assert_eq!(a, EfuseImage::synth(1));
        assert_ne!(a.mac(), b.mac());
        assert_ne!(a.unique_id(), b.unique_id());
        assert_ne!(a.unique_id(), [0u8; 16]);
        let mut img = EfuseImage::blank();
        img.set_mac(G3_SYNTH_MAC);
        assert_eq!(img.words()[1][0], 0x00C3_0001);
        assert_eq!(img.words()[1][1], 0x0000_0200);
        assert_eq!(img.mac(), G3_SYNTH_MAC);
    }

    /// The word set of `specs/notes/g3-behavior.md` (g3-synth-efuse).
    #[test]
    fn synth_words_match_the_g3_reference_image() {
        let mut img = EfuseImage::synth(0);
        img.set_mac(G3_SYNTH_MAC);
        img.set_unique_id([0; 16]);
        let mut want = [[0u32; WORDS_PER_BLOCK]; EFUSE_BLOCKS];
        want[0][4] = 0x8000_0000;
        want[1][0] = 0x00C3_0001;
        want[1][1] = 0x0000_0200;
        want[1][3] = 0x6304_0000;
        want[1][4] = 0x0000_0001;
        want[1][5] = 0x0100_0000;
        want[2][4] = 0x0000_0001;
        assert_eq!(img.words(), &want);
        assert_eq!(img.dump_words().len(), DUMP_WORDS);
        assert_eq!(img.dump_words()[4], 0x8000_0000);
        assert_eq!(img.dump_words()[6], 0x00C3_0001);
    }

    #[test]
    fn a_dump_round_trips_and_taints() {
        let synth = EfuseImage::synth(3);
        let mut bytes = Vec::new();
        for word in synth.dump_words() {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        let imported = EfuseImage::from_dump(&bytes).expect("84-word dump");
        assert_eq!(imported.words(), synth.words());
        assert_eq!(imported.origin(), EfuseOrigin::Dump);
        assert!(imported.tainted());
        assert_eq!(imported.chip_revision(), SYNTH_CHIP_REVISION);

        bytes.resize(1024, 0);
        assert_eq!(
            EfuseImage::from_dump(&bytes).expect("padded dump").words(),
            synth.words()
        );
        let mut flat = Vec::new();
        for block in synth.words() {
            for word in block {
                flat.extend_from_slice(&word.to_le_bytes());
            }
        }
        assert_eq!(
            EfuseImage::from_dump(&flat).expect("flat dump").words(),
            synth.words()
        );
        assert!(matches!(
            EfuseImage::from_dump(&[0u8; 8]),
            Err(LoadError::Malformed { .. })
        ));
    }

    #[test]
    fn block_files_fill_their_block_and_taint() {
        let blk1: Vec<u8> = EfuseImage::synth(0).words()[1]
            .iter()
            .take(BLOCK_WORDS[1])
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(blk1.len(), 24);
        let img = EfuseImage::from_blocks(&[(1, &blk1)]).expect("blk1 file");
        assert_eq!(img.chip_revision(), SYNTH_CHIP_REVISION);
        assert_eq!(img.get(field::BLK_VERSION_MINOR), 3);
        // BLK2 was not supplied, so BLK_VERSION_MAJOR stays 0.
        assert_eq!(img.block_revision(), Revision::new(0, 3));
        assert!(img.tainted());
        assert!(EfuseImage::from_blocks(&[(1, &[0u8; 28])]).is_err());
        assert!(EfuseImage::from_blocks(&[(11, &[0u8; 4])]).is_err());
        assert!(EfuseImage::from_blocks(&[(2, &[0u8; 3])]).is_err());
    }
}
