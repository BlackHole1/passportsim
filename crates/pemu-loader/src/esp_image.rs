//! ESP image format (`components/bootloader_support/include/esp_app_format.h`, ESP-IDF v5.5.3):
//! a 24-byte header, then per segment an 8-byte `{load_addr, data_len}` header and the data, zero
//! padding until `(offset + 1) % 16 == 0`, one checksum byte (XOR of all segment data, seed 0xEF)
//! and, when `hash_appended` is 1, the SHA-256 of everything before it. Merged flash images carry
//! the bootloader at 0x0, the partition table at 0x8000 and apps at their partition offsets.

use crate::app_desc::{APP_DESC_MAGIC, AppDesc};
use crate::partitions::{Partition, PartitionTable};
use crate::{LoadError, le_u16, le_u32, sha256, slice};

/// `ESP_IMAGE_HEADER_MAGIC`.
pub const IMAGE_MAGIC: u8 = 0xE9;
/// `sizeof(esp_image_header_t)`.
pub const HEADER_LEN: usize = 24;
/// `sizeof(esp_image_segment_header_t)`.
pub const SEGMENT_HEADER_LEN: usize = 8;
/// `ESP_IMAGE_MAX_SEGMENTS`.
pub const MAX_SEGMENTS: usize = 16;
pub const CHECKSUM_SEED: u8 = 0xEF;
/// `ESP_CHIP_ID_ESP32C3`.
pub const CHIP_ID_ESP32C3: u16 = 0x0005;
/// The ESP32-C3 second-stage bootloader sits at flash offset 0.
pub const BOOTLOADER_OFFSET: usize = 0x0;

/// `esp_image_header_t` without the magic.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImageHeader {
    pub segment_count: u8,
    pub spi_mode: u8,
    /// Low 4 bits of byte 3.
    pub spi_speed: u8,
    /// High 4 bits of byte 3.
    pub spi_size: u8,
    pub entry_addr: u32,
    pub wp_pin: u8,
    pub spi_pin_drv: [u8; 3],
    pub chip_id: u16,
    pub min_chip_rev: u8,
    pub min_chip_rev_full: u16,
    pub max_chip_rev_full: u16,
    pub hash_appended: bool,
}

/// One image segment; `data_offset` indexes the buffer the image was parsed from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    pub load_addr: u32,
    pub data_offset: usize,
    pub len: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EspImage {
    /// Start of the image in the parsed buffer.
    pub offset: usize,
    pub header: ImageHeader,
    pub segments: Vec<Segment>,
    pub checksum_stored: u8,
    pub checksum_calc: u8,
    /// Stored and computed SHA-256, present when `hash_appended` is set.
    pub sha256_stored: Option<[u8; 32]>,
    pub sha256_calc: Option<[u8; 32]>,
    /// Image length including the checksum and the appended hash.
    pub len: usize,
}

impl EspImage {
    pub fn parse_at(buf: &[u8], offset: usize) -> Result<EspImage, LoadError> {
        let h = slice(buf, offset, HEADER_LEN, "image header")?;
        if h[0] != IMAGE_MAGIC {
            return Err(LoadError::BadMagic {
                what: "image header",
                offset,
                found: u32::from(h[0]),
            });
        }
        if usize::from(h[1]) > MAX_SEGMENTS {
            return Err(LoadError::Malformed {
                what: "image header",
                detail: format!("{} segments, at most {MAX_SEGMENTS}", h[1]),
            });
        }
        let header = ImageHeader {
            segment_count: h[1],
            spi_mode: h[2],
            spi_speed: h[3] & 0xF,
            spi_size: h[3] >> 4,
            entry_addr: le_u32(h, 4, "image header")?,
            wp_pin: h[8],
            spi_pin_drv: [h[9], h[10], h[11]],
            chip_id: le_u16(h, 12, "image header")?,
            min_chip_rev: h[14],
            min_chip_rev_full: le_u16(h, 15, "image header")?,
            max_chip_rev_full: le_u16(h, 17, "image header")?,
            hash_appended: h[23] == 1,
        };
        let mut pos = offset + HEADER_LEN;
        let mut segments = Vec::with_capacity(usize::from(header.segment_count));
        let mut checksum_calc = CHECKSUM_SEED;
        for _ in 0..header.segment_count {
            let load_addr = le_u32(buf, pos, "segment header")?;
            let len = le_u32(buf, pos + 4, "segment header")?;
            let data_offset = pos + SEGMENT_HEADER_LEN;
            let data = slice(buf, data_offset, len as usize, "segment data")?;
            checksum_calc = data.iter().fold(checksum_calc, |c, b| c ^ b);
            segments.push(Segment {
                load_addr,
                data_offset,
                len,
            });
            pos = data_offset + len as usize;
        }
        let checksum_at = offset + ((pos - offset) | 0xF);
        let checksum_stored = slice(buf, checksum_at, 1, "image checksum")?[0];
        let mut len = checksum_at + 1 - offset;
        let (sha256_stored, sha256_calc) = if header.hash_appended {
            let stored = slice(buf, offset + len, 32, "image SHA-256")?;
            let mut s = [0u8; 32];
            s.copy_from_slice(stored);
            let calc = sha256(&buf[offset..offset + len]);
            len += 32;
            (Some(s), Some(calc))
        } else {
            (None, None)
        };
        Ok(EspImage {
            offset,
            header,
            segments,
            checksum_stored,
            checksum_calc,
            sha256_stored,
            sha256_calc,
            len,
        })
    }

    pub fn parse(buf: &[u8]) -> Result<EspImage, LoadError> {
        EspImage::parse_at(buf, 0)
    }

    pub fn checksum_ok(&self) -> bool {
        self.checksum_stored == self.checksum_calc
    }

    /// `None` when no hash is appended.
    pub fn hash_ok(&self) -> Option<bool> {
        Some(self.sha256_stored? == self.sha256_calc?)
    }

    /// `buf` must be the buffer the image was parsed from.
    pub fn segment_data<'a>(&self, buf: &'a [u8], index: usize) -> Option<&'a [u8]> {
        let s = self.segments.get(index)?;
        buf.get(s.data_offset..s.data_offset + s.len as usize)
    }

    /// The app descriptor at the start of the first segment, if that segment begins with its magic.
    pub fn app_desc(&self, buf: &[u8]) -> Result<Option<AppDesc>, LoadError> {
        match self.segment_data(buf, 0) {
            Some(d) if d.len() >= 4 && le_u32(d, 0, "app descriptor")? == APP_DESC_MAGIC => {
                AppDesc::parse(d).map(Some)
            }
            _ => Ok(None),
        }
    }
}

/// Bootloader, partition table and boot app, parsed in place.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MergedImage {
    pub bootloader: EspImage,
    pub partitions: PartitionTable,
    /// The partition [`PartitionTable::boot_app`] selects and its image, if the table has an app.
    pub app: Option<(Partition, EspImage)>,
}

impl MergedImage {
    pub fn parse(flash: &[u8]) -> Result<MergedImage, LoadError> {
        let bootloader = EspImage::parse_at(flash, BOOTLOADER_OFFSET)?;
        let partitions = PartitionTable::from_flash(flash)?;
        let app = match partitions.boot_app() {
            Some(p) => Some((p.clone(), EspImage::parse_at(flash, p.offset as usize)?)),
            None => None,
        };
        Ok(MergedImage {
            bootloader,
            partitions,
            app,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_desc::APP_DESC_LEN;
    use crate::partitions::{TABLE_MAX_LEN, TABLE_OFFSET, ptype, subtype};

    fn build(segments: &[(u32, &[u8])], hash: bool) -> Vec<u8> {
        let mut img = vec![IMAGE_MAGIC, segments.len() as u8, 2, 0x3F];
        img.extend_from_slice(&0x4038_02E8u32.to_le_bytes());
        img.extend_from_slice(&[0xEE, 0, 0, 0]);
        img.extend_from_slice(&CHIP_ID_ESP32C3.to_le_bytes());
        img.push(3);
        img.extend_from_slice(&3u16.to_le_bytes());
        img.extend_from_slice(&199u16.to_le_bytes());
        img.extend_from_slice(&[0; 4]);
        img.push(u8::from(hash));
        assert_eq!(img.len(), HEADER_LEN);
        let mut checksum = CHECKSUM_SEED;
        for (addr, data) in segments {
            img.extend_from_slice(&addr.to_le_bytes());
            img.extend_from_slice(&(data.len() as u32).to_le_bytes());
            img.extend_from_slice(data);
            checksum = data.iter().fold(checksum, |c, b| c ^ b);
        }
        while img.len() % 16 != 15 {
            img.push(0);
        }
        img.push(checksum);
        if hash {
            let digest = sha256(&img);
            img.extend_from_slice(&digest);
        }
        img
    }

    fn app_desc_bytes() -> Vec<u8> {
        let mut d = vec![0u8; APP_DESC_LEN];
        d[..4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        d[48..53].copy_from_slice(b"probe");
        d
    }

    #[test]
    fn parses_header_segments_checksum_and_hash() {
        let desc = app_desc_bytes();
        let img = build(&[(0x3C00_0020, &desc), (0x4200_0020, &[1, 2, 3])], true);
        let e = EspImage::parse(&img).unwrap();
        assert_eq!((e.header.segment_count, e.header.spi_mode), (2, 2));
        assert_eq!((e.header.spi_speed, e.header.spi_size), (0xF, 3));
        assert_eq!((e.header.entry_addr, e.header.wp_pin), (0x4038_02E8, 0xEE));
        assert_eq!(e.header.chip_id, CHIP_ID_ESP32C3);
        assert_eq!(
            (e.header.min_chip_rev_full, e.header.max_chip_rev_full),
            (3, 199)
        );
        assert!(e.header.hash_appended);
        assert_eq!(e.segments[1].load_addr, 0x4200_0020);
        assert_eq!(e.segment_data(&img, 1), Some(&[1u8, 2, 3][..]));
        assert!(e.checksum_ok());
        assert_eq!(e.hash_ok(), Some(true));
        assert_eq!(e.len, img.len());
        assert_eq!(e.app_desc(&img).unwrap().unwrap().project_name, "probe");
    }

    #[test]
    fn detects_corruption_and_bad_input() {
        let img = build(&[(0x4200_0020, &[9; 40])], true);
        let mut data = img.clone();
        data[HEADER_LEN + SEGMENT_HEADER_LEN] ^= 0x10;
        let e = EspImage::parse(&data).unwrap();
        assert!(!e.checksum_ok());
        assert_eq!(e.hash_ok(), Some(false));
        assert_eq!(e.app_desc(&data).unwrap(), None);
        let plain = build(&[(0x4200_0020, &[9; 40])], false);
        assert_eq!(EspImage::parse(&plain).unwrap().hash_ok(), None);
        let mut bad = img.clone();
        bad[0] = 0xE8;
        assert!(matches!(
            EspImage::parse(&bad),
            Err(LoadError::BadMagic { .. })
        ));
        let mut many = img.clone();
        many[1] = 17;
        assert!(matches!(
            EspImage::parse(&many),
            Err(LoadError::Malformed { .. })
        ));
        assert!(matches!(
            EspImage::parse(&img[..img.len() - 1]),
            Err(LoadError::Truncated { .. })
        ));
    }

    #[test]
    fn merged_image_finds_the_factory_app() {
        let boot = build(&[(0x3FCD_5830, &[7; 16])], true);
        let app = build(&[(0x3C00_0020, &app_desc_bytes())], true);
        let mut flash = vec![0xFF; 0x1_0000];
        flash[..boot.len()].copy_from_slice(&boot);
        let mut row = vec![0xAA, 0x50, ptype::APP, subtype::FACTORY];
        row.extend_from_slice(&0x1_0000u32.to_le_bytes());
        row.extend_from_slice(&0x1_0000u32.to_le_bytes());
        row.extend_from_slice(b"factory\0\0\0\0\0\0\0\0\0");
        row.extend_from_slice(&[0; 4]);
        flash[TABLE_OFFSET..TABLE_OFFSET + row.len()].copy_from_slice(&row);
        assert!(
            flash[TABLE_OFFSET + 32..TABLE_OFFSET + TABLE_MAX_LEN]
                .iter()
                .all(|&b| b == 0xFF)
        );
        flash.extend_from_slice(&app);
        let m = MergedImage::parse(&flash).unwrap();
        assert!(m.bootloader.checksum_ok());
        let (part, image) = m.app.unwrap();
        assert_eq!((part.name.as_str(), image.offset), ("factory", 0x1_0000));
        assert_eq!(image.hash_ok(), Some(true));
        assert!(image.app_desc(&flash).unwrap().is_some());
    }
}
