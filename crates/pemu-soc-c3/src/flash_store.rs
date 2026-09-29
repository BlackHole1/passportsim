//! The 8 MB flash behind the MMU (`specs/blocks/flash_xmc.toml`): the image the machine was
//! built from, plus an overlay of the pages the guest has written.
//!
//! The image is an `Arc` because `Machine::fork` shares it; every guest write lands in the
//! overlay, keyed by the 4 KB sector, the smallest erase unit of the part. [`FlashStore::delta`]
//! holds only pages whose bytes differ from the image, so a program undone by an erase leaves no
//! delta and the run identity does not drift on writes that changed nothing.
//!
//! NOR program clears bits and erase sets them, so [`FlashStore::program`] ands into the stored
//! bytes. Timing belongs to the SPI1 model; this store only holds bytes.
//!
//! A mapped flash window is served from arena copies of these pages, so the store keeps one bit
//! per page saying whether that copy is current ([`FlashStore::mirror_is_current`]). Every
//! mutation goes through one private helper that drops the bit, so reads always see current
//! flash contents without trusting callers to announce writes.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use pemu_core::serde::{Deserialize, Serialize};

use crate::mem::FLASH_LEN;

/// Bytes of one overlay page: the 4 KB sector, the smallest erase unit of the part.
pub const PAGE_LEN: u32 = 0x1000;

pub const PAGES: u32 = FLASH_LEN / PAGE_LEN;

/// Byte an erased flash cell reads as.
pub const ERASED: u8 = 0xFF;

/// Bytes of a 64 KB block erase (0xD8) and of one MMU page.
pub const BLOCK_LEN: u32 = 0x1_0000;

/// What a flash store refuses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FlashError {
    /// The image is not [`FLASH_LEN`] bytes.
    ImageLen(usize),
    /// A delta names a page outside the flash.
    DeltaPage(u32),
    /// A delta page does not hold [`PAGE_LEN`] bytes.
    DeltaLen(u32, usize),
    /// A delta page is not above the one before it: strictly ascending pages are what make two
    /// stores with the same content produce the same bytes and run identity.
    DeltaOrder(u32),
}

impl fmt::Display for FlashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlashError::ImageLen(len) => {
                write!(f, "flash image is {len} bytes, expected {FLASH_LEN}")
            }
            FlashError::DeltaPage(page) => {
                write!(f, "flash delta page {page} is outside 0..{PAGES}")
            }
            FlashError::DeltaLen(page, len) => {
                write!(
                    f,
                    "flash delta page {page} holds {len} bytes, expected {PAGE_LEN}"
                )
            }
            FlashError::DeltaOrder(page) => {
                write!(f, "flash delta page {page} is not above the page before it")
            }
        }
    }
}

impl std::error::Error for FlashError {}

/// One written page of the [`FlashDelta`]: its index and its current bytes.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct DeltaPage {
    pub page: u32,
    pub bytes: Vec<u8>,
}

/// The difference between a flash store and its image, ascending by page: the content of the
/// `flash_delta` snapshot section.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct FlashDelta {
    pub pages: Vec<DeltaPage>,
}

impl FlashDelta {
    pub fn len_bytes(&self) -> usize {
        self.pages.len() * PAGE_LEN as usize
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// The 8 MB flash: a shared image plus the pages the guest has written.
#[derive(Clone)]
pub struct FlashStore {
    image: Arc<[u8]>,
    overlay: BTreeMap<u32, Vec<u8>>,
    /// Bit per page: the arena mirror of the page holds the store's current bytes.
    mirrored: Vec<u64>,
    /// Bumped by every program, erase, delta and reload. Not machine state: no snapshot carries
    /// it and no state hash reads it.
    changes: u64,
}

impl FlashStore {
    /// A store over `image`, which must be [`FLASH_LEN`] bytes.
    pub fn new(image: Arc<[u8]>) -> Result<FlashStore, FlashError> {
        if image.len() != FLASH_LEN as usize {
            return Err(FlashError::ImageLen(image.len()));
        }
        Ok(FlashStore {
            image,
            overlay: BTreeMap::new(),
            mirrored: vec![0; (PAGES as usize).div_ceil(64)],
            changes: 0,
        })
    }

    /// A store over `bytes`, padded with [`ERASED`] to [`FLASH_LEN`] (a partial image, such as a
    /// bootloader plus one app).
    pub fn from_bytes(bytes: &[u8]) -> Result<FlashStore, FlashError> {
        if bytes.len() > FLASH_LEN as usize {
            return Err(FlashError::ImageLen(bytes.len()));
        }
        let mut image = vec![ERASED; FLASH_LEN as usize];
        image[..bytes.len()].copy_from_slice(bytes);
        FlashStore::new(image.into())
    }

    pub fn erased() -> FlashStore {
        FlashStore::from_bytes(&[]).expect("an empty slice fits in FLASH_LEN")
    }

    /// The image the store was built from, which no write changes and a fork shares.
    pub fn image(&self) -> &Arc<[u8]> {
        &self.image
    }

    /// Byte at `addr`. An address past the end of the flash reads as [`ERASED`], as an absent
    /// chip does.
    #[inline]
    pub fn read_byte(&self, addr: u32) -> u8 {
        if addr >= FLASH_LEN {
            return ERASED;
        }
        let (page, off) = (addr / PAGE_LEN, (addr % PAGE_LEN) as usize);
        match self.overlay.get(&page) {
            Some(bytes) => bytes[off],
            None => self.image[addr as usize],
        }
    }

    pub fn read(&self, addr: u32, out: &mut [u8]) {
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.read_byte(addr.wrapping_add(i as u32));
        }
    }

    /// The [`PAGE_LEN`] bytes of page `page`, overlay or image; `None` past the end of the chip.
    pub fn page_bytes(&self, page: u32) -> Option<&[u8]> {
        if page >= PAGES {
            return None;
        }
        let start = (page * PAGE_LEN) as usize;
        Some(match self.overlay.get(&page) {
            Some(bytes) => bytes,
            None => &self.image[start..start + PAGE_LEN as usize],
        })
    }

    /// Programs `data` at `addr`, NOR style: every written bit is anded into the stored byte.
    /// Bytes past the end are dropped. The 256-byte page-program wrap is the SPI1 model's
    /// business.
    pub fn program(&mut self, addr: u32, data: &[u8]) {
        for (i, byte) in data.iter().enumerate() {
            let at = addr.wrapping_add(i as u32);
            if at >= FLASH_LEN {
                continue;
            }
            let page = self.page_mut(at / PAGE_LEN);
            let off = (at % PAGE_LEN) as usize;
            page[off] &= byte;
        }
    }

    /// Erases the [`PAGE_LEN`] sector holding `addr` to [`ERASED`] (0x20).
    pub fn erase_sector(&mut self, addr: u32) {
        if addr < FLASH_LEN {
            self.page_mut(addr / PAGE_LEN).fill(ERASED);
        }
    }

    /// Erases the [`BLOCK_LEN`] block holding `addr` (0xD8).
    pub fn erase_block(&mut self, addr: u32) {
        if addr >= FLASH_LEN {
            return;
        }
        let first = (addr / BLOCK_LEN) * (BLOCK_LEN / PAGE_LEN);
        for page in first..first + BLOCK_LEN / PAGE_LEN {
            self.page_mut(page).fill(ERASED);
        }
    }

    /// Erases the whole chip (0x60 / 0xC7).
    pub fn erase_chip(&mut self) {
        for page in 0..PAGES {
            self.page_mut(page).fill(ERASED);
        }
    }

    /// Whether page `page` has been written. A page written back to its image content stays in
    /// the overlay but contributes no [`FlashDelta`] entry.
    pub fn is_written(&self, page: u32) -> bool {
        self.overlay.contains_key(&page)
    }

    /// Whether the arena mirror of page `page` holds the bytes this store reads now. Only true
    /// when [`FlashStore::mark_mirrored`] ran after the last change to the page, so a window
    /// rewrite can copy only stale pages.
    #[inline]
    pub fn mirror_is_current(&self, page: u32) -> bool {
        (page < PAGES) && self.mirrored[(page / 64) as usize] & (1 << (page % 64)) != 0
    }

    /// Records that the arena mirror of `page` was just copied from this store. Only the two copy
    /// paths of `crate::wiring` call it.
    #[inline]
    pub fn mark_mirrored(&mut self, page: u32) {
        if page < PAGES {
            self.mirrored[(page / 64) as usize] |= 1 << (page % 64);
        }
    }

    fn unmirror_all(&mut self) {
        self.mirrored.fill(0);
        self.changes = self.changes.wrapping_add(1);
    }

    /// A counter that moves whenever the bytes may have changed, and never otherwise: a caller
    /// that derived something from the flash content re-derives it only when it moved.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// Makes this store's change counter follow `previous`'s, for a store that replaces it on
    /// restore, so the counter still moves across the replacement.
    pub fn succeed(&mut self, previous: &FlashStore) {
        self.changes = previous.changes.max(self.changes).wrapping_add(1);
    }

    /// The difference against the image, ascending by page.
    pub fn delta(&self) -> FlashDelta {
        let pages = self
            .overlay
            .iter()
            .filter(|(page, bytes)| self.image_page(**page) != bytes.as_slice())
            .map(|(page, bytes)| DeltaPage {
                page: *page,
                bytes: bytes.clone(),
            })
            .collect();
        FlashDelta { pages }
    }

    /// Replaces the overlay with `delta` on restore. A repeated or unsorted page is refused
    /// rather than collapsed: the last entry would silently win, the restored store would not
    /// reproduce its delta, and two identical runs would hash differently.
    pub fn apply_delta(&mut self, delta: &FlashDelta) -> Result<(), FlashError> {
        let mut prev: Option<u32> = None;
        for page in &delta.pages {
            if page.page >= PAGES {
                return Err(FlashError::DeltaPage(page.page));
            }
            if page.bytes.len() != PAGE_LEN as usize {
                return Err(FlashError::DeltaLen(page.page, page.bytes.len()));
            }
            if prev.is_some_and(|prev| page.page <= prev) {
                return Err(FlashError::DeltaOrder(page.page));
            }
            prev = Some(page.page);
        }
        self.overlay = delta
            .pages
            .iter()
            .map(|p| (p.page, p.bytes.clone()))
            .collect();
        self.unmirror_all();
        Ok(())
    }

    /// Drops the overlay, so the store reads the image again.
    pub fn reload(&mut self) {
        self.overlay.clear();
        self.unmirror_all();
    }

    fn image_page(&self, page: u32) -> &[u8] {
        let start = (page * PAGE_LEN) as usize;
        &self.image[start..start + PAGE_LEN as usize]
    }

    /// The overlay page `page`, copied from the image on first use. Every program and erase goes
    /// through here, which drops the page's mirror record.
    fn page_mut(&mut self, page: u32) -> &mut [u8] {
        self.mirrored[(page / 64) as usize] &= !(1 << (page % 64));
        self.changes = self.changes.wrapping_add(1);
        let start = (page * PAGE_LEN) as usize;
        let image = &self.image;
        self.overlay
            .entry(page)
            .or_insert_with(|| image[start..start + PAGE_LEN as usize].to_vec())
    }
}

impl Default for FlashStore {
    fn default() -> Self {
        FlashStore::erased()
    }
}

impl fmt::Debug for FlashStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashStore")
            .field("len", &self.image.len())
            .field("written_pages", &self.overlay.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image whose every byte names its address, so a read proves which page it came from.
    fn image() -> FlashStore {
        let bytes: Vec<u8> = (0..FLASH_LEN).map(|a| (a >> 4) as u8).collect();
        FlashStore::new(bytes.into()).expect("FLASH_LEN bytes")
    }

    #[test]
    fn an_image_must_be_eight_megabytes() {
        assert_eq!(FLASH_LEN, 8 << 20);
        assert_eq!(PAGES, 2048);
        assert_eq!(
            FlashStore::new(vec![0; 16].into()).unwrap_err(),
            FlashError::ImageLen(16)
        );
        assert_eq!(
            FlashStore::from_bytes(&vec![0; FLASH_LEN as usize + 1]).unwrap_err(),
            FlashError::ImageLen(FLASH_LEN as usize + 1)
        );
        // A short image is padded with erased bytes, so the whole chip is addressable.
        let short = FlashStore::from_bytes(&[1, 2, 3]).expect("shorter than FLASH_LEN");
        assert_eq!(short.read_byte(0), 1);
        assert_eq!(short.read_byte(3), ERASED);
        assert_eq!(short.read_byte(FLASH_LEN - 1), ERASED);
        assert!(short.delta().is_empty());
    }

    #[test]
    fn reads_fall_through_to_the_image_until_a_page_is_written() {
        let mut flash = image();
        assert_eq!(flash.read_byte(0), 0);
        assert_eq!(flash.read_byte(0x10), 1);
        assert!(!flash.is_written(0));
        flash.program(0x20, &[0x00]);
        assert!(flash.is_written(0));
        assert_eq!(flash.read_byte(0x20), 0x00);
        assert_eq!(flash.read_byte(0x21), 2);
        assert_eq!(flash.read_byte(0x10), 1);
        assert!(!flash.is_written(1));
        assert_eq!(flash.read_byte(PAGE_LEN), (PAGE_LEN >> 4) as u8);
        assert_eq!(flash.read_byte(FLASH_LEN), ERASED);
    }

    #[test]
    fn programming_clears_bits_and_erasing_sets_them() {
        let mut flash = FlashStore::erased();
        assert_eq!(flash.read_byte(0), ERASED);
        flash.program(0, &[0xF0, 0x0F]);
        assert_eq!([flash.read_byte(0), flash.read_byte(1)], [0xF0, 0x0F]);
        // A program only clears bits; 0xFF cannot be written back.
        flash.program(0, &[0xFF]);
        assert_eq!(flash.read_byte(0), 0xF0);
        flash.program(0, &[0x3C]);
        assert_eq!(flash.read_byte(0), 0x30);
        flash.program(PAGE_LEN, &[0x00]);
        flash.erase_sector(0x123);
        assert_eq!(flash.read_byte(0), ERASED);
        assert_eq!(flash.read_byte(1), ERASED);
        assert_eq!(flash.read_byte(PAGE_LEN), 0x00);
        flash.erase_block(BLOCK_LEN - 1);
        assert_eq!(flash.read_byte(PAGE_LEN), ERASED);
    }

    #[test]
    fn the_delta_holds_only_pages_that_differ_from_the_image() {
        let mut flash = image();
        assert!(flash.delta().is_empty());

        assert_eq!(flash.read_byte(0x2013), 0x01);
        flash.program(0x2013, &[0x00]);
        let delta = flash.delta();
        assert_eq!(delta.pages.len(), 1);
        assert_eq!(delta.pages[0].page, 2);
        assert_eq!(delta.pages[0].bytes.len(), PAGE_LEN as usize);
        assert_eq!(delta.pages[0].bytes[0x13], 0x00);
        assert_eq!(delta.pages[0].bytes[0x14], flash.read_byte(0x2014));
        assert_eq!(delta.len_bytes(), PAGE_LEN as usize);

        flash.erase_sector(0x1000);
        let delta = flash.delta();
        assert_eq!(
            delta.pages.iter().map(|p| p.page).collect::<Vec<_>>(),
            vec![1, 2]
        );

        // Writing a page back to its image content takes it out of the delta.
        let image_page = flash.image()[0x2000..0x3000].to_vec();
        flash.erase_sector(0x2000);
        flash.program(0x2000, &image_page);
        assert!(flash.is_written(2));
        assert_eq!(
            flash
                .delta()
                .pages
                .iter()
                .map(|p| p.page)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn a_delta_restores_a_store_over_the_same_image() {
        let mut flash = image();
        flash.program(0x4028, &[0x01, 0x02]);
        flash.erase_sector(0x9000);
        let delta = flash.delta();
        assert_eq!(
            delta.pages.iter().map(|p| p.page).collect::<Vec<_>>(),
            vec![4, 9]
        );

        let mut restored = image();
        restored
            .apply_delta(&delta)
            .expect("a delta this store made");
        assert_eq!(restored.delta(), delta);
        for addr in [0x4028, 0x4029, 0x402A, 0x9000, 0x9FFF, 0x1_0000] {
            assert_eq!(restored.read_byte(addr), flash.read_byte(addr), "{addr:#X}");
        }

        let mut bad = image();
        assert_eq!(
            bad.apply_delta(&FlashDelta {
                pages: vec![DeltaPage {
                    page: PAGES,
                    bytes: vec![0; PAGE_LEN as usize]
                }]
            }),
            Err(FlashError::DeltaPage(PAGES))
        );
        assert_eq!(
            bad.apply_delta(&FlashDelta {
                pages: vec![DeltaPage {
                    page: 0,
                    bytes: vec![0; 8]
                }]
            }),
            Err(FlashError::DeltaLen(0, 8))
        );
        assert!(bad.delta().is_empty());
    }

    #[test]
    fn a_delta_whose_pages_do_not_ascend_is_refused() {
        // The overlay is a map, so a repeated page would restore last-write-wins.
        let page = |page: u32, fill: u8| DeltaPage {
            page,
            bytes: vec![fill; PAGE_LEN as usize],
        };
        let mut flash = image();
        assert_eq!(
            flash.apply_delta(&FlashDelta {
                pages: vec![page(7, 0xAA), page(7, 0xBB)],
            }),
            Err(FlashError::DeltaOrder(7))
        );
        assert_eq!(
            flash.apply_delta(&FlashDelta {
                pages: vec![page(9, 0xAA), page(4, 0xBB)],
            }),
            Err(FlashError::DeltaOrder(4))
        );
        assert!(flash.delta().is_empty(), "a refused delta changes nothing");

        let accepted = FlashDelta {
            pages: vec![page(4, 0xAA), page(9, 0xBB)],
        };
        flash.apply_delta(&accepted).expect("ascending pages");
        assert_eq!(flash.delta(), accepted);
    }

    #[test]
    fn a_reload_drops_the_overlay_and_the_image_is_never_written() {
        let mut flash = image();
        let base = Arc::clone(flash.image());
        flash.erase_chip();
        assert_eq!(flash.read_byte(0x1234), ERASED);
        assert_eq!(flash.delta().pages.len(), PAGES as usize);
        // The base is shared, so it must still hold the image bytes.
        assert_eq!(base[0x1234], 0x23);
        flash.reload();
        assert_eq!(flash.read_byte(0x1234), 0x23);
        assert!(flash.delta().is_empty());
    }

    /// Nothing but [`FlashStore::mark_mirrored`] sets the mirror record, and every way of
    /// changing a page clears it.
    #[test]
    fn every_change_to_a_page_drops_its_mirror_record() {
        let mut flash = image();
        assert!(!flash.mirror_is_current(0));
        flash.mark_mirrored(0);
        flash.mark_mirrored(1);
        flash.mark_mirrored(PAGES - 1);
        assert!(flash.mirror_is_current(0) && flash.mirror_is_current(PAGES - 1));
        // A program clears only the page it writes.
        flash.program(0x10, &[0]);
        assert!(!flash.mirror_is_current(0));
        assert!(flash.mirror_is_current(1));
        // A sector erase clears its own page, a block erase all 16 of the block.
        flash.mark_mirrored(0);
        flash.erase_sector(PAGE_LEN);
        assert!(!flash.mirror_is_current(1));
        for page in 0..BLOCK_LEN / PAGE_LEN {
            flash.mark_mirrored(page);
        }
        flash.erase_block(0);
        assert!((0..BLOCK_LEN / PAGE_LEN).all(|page| !flash.mirror_is_current(page)));
        flash.mark_mirrored(PAGES - 1);
        flash.erase_chip();
        assert!(!flash.mirror_is_current(PAGES - 1));
        let delta = flash.delta();
        flash.mark_mirrored(3);
        flash.apply_delta(&delta).expect("its own delta");
        assert!(!flash.mirror_is_current(3));
        flash.mark_mirrored(3);
        flash.reload();
        assert!(!flash.mirror_is_current(3));
        flash.mark_mirrored(PAGES);
        assert!(!flash.mirror_is_current(PAGES));
    }

    #[test]
    fn a_page_view_reads_the_overlay_when_there_is_one() {
        let mut flash = image();
        assert_eq!(flash.page_bytes(0).unwrap()[0x10], 1);
        flash.program(0x10, &[0]);
        assert_eq!(flash.page_bytes(0).unwrap()[0x10], 0);
        assert_eq!(flash.page_bytes(PAGES), None);
    }
}
