//! The GDMA view of memory (`specs/blocks/gdma.toml`).
//!
//! A GDMA link register holds 20 bits of address; the DMA-visible address is
//! `0x3FC00000 | addr20`, the 1 MB-aligned base of the internal DRAM window. Every descriptor and
//! buffer SPI2 and I2S0 use lands in the SRAM1 DRAM view 0x3FC80000 to 0x3FCDFFFF (no PSRAM on
//! this board).
//!
//! [`DmaView`] rebuilds the address with [`dma_addr`] and resolves it through the same page table
//! the CPU uses, so a page the CPU cannot reach, DMA cannot reach either.
//!
//! It is not a bus master with its own permissions: the SENSITIVE DMA PMS registers are stored
//! but not enforced, so the view grants what the page entry grants. It uses
//! [`crate::pagetable::slow_readable`] and [`crate::pagetable::slow_writable`], because a
//! `PF_SLOW` page of the PMS split is still ordinary memory to a DMA engine. An entry past the end
//! of the arena is refused on both sides, as [`crate::Soc::resolve`] refuses it for the CPU.

use crate::mem::Arena;
use crate::pagetable::{self, PageTable};
use crate::periph::gdma::DmaMem;

/// Base the 20 address bits of a GDMA link register are rebuilt against.
pub const DMA_BASE: u32 = 0x3FC0_0000;

/// Address bits a GDMA link register holds.
pub const DMA_ADDR_BITS: u32 = 20;

/// Mask of the bits a GDMA link register holds.
pub const DMA_ADDR_MASK: u32 = (1 << DMA_ADDR_BITS) - 1;

/// Bytes the DMA view can reach: the 1 MB the 20 bits span.
pub const DMA_WINDOW_LEN: u32 = DMA_ADDR_MASK + 1;

/// The CPU address of the 20-bit DMA address `addr20`. Bits above [`DMA_ADDR_MASK`] have no
/// register field and are dropped.
#[inline]
pub const fn dma_addr(addr20: u32) -> u32 {
    DMA_BASE | (addr20 & DMA_ADDR_MASK)
}

/// Whether the CPU address `addr` is one the DMA view can name at all.
#[inline]
pub const fn in_dma_window(addr: u32) -> bool {
    addr >= DMA_BASE && addr - DMA_BASE < DMA_WINDOW_LEN
}

/// GDMA view reached through `Cx::dma`. It borrows only `&soc.pages` and `&mut soc.arena`: a DMA
/// transfer is not a CPU bus access, so it takes no `HartView`, charges no time and never reaches
/// [`crate::mmio`].
pub struct DmaView<'a> {
    /// The buffers, or `None` for a detached view on which every access fails. `Option` rather
    /// than a dummy pair, because a `PageTable` is 4 MB and an `Arena` 9 MB.
    inner: Option<DmaBuffers<'a>>,
}

/// The two disjoint borrows of a [`crate::Soc`] a working view needs.
struct DmaBuffers<'a> {
    pages: &'a PageTable,
    arena: &'a mut Arena,
}

impl<'a> DmaView<'a> {
    /// A view over `pages` and `arena`.
    pub fn new(pages: &'a PageTable, arena: &'a mut Arena) -> DmaView<'a> {
        DmaView {
            inner: Some(DmaBuffers { pages, arena }),
        }
    }

    /// A view that reaches no buffer. The machine's bus holds the whole `&mut Soc` and cannot
    /// split out the two borrows, so a walk against it reports failure rather than reading zeros.
    pub fn detached() -> DmaView<'static> {
        DmaView { inner: None }
    }

    /// Arena offset of `addr20` when its page is mapped and readable and the offset is inside the
    /// arena.
    #[inline]
    fn readable_at(&self, addr20: u32) -> Option<usize> {
        let addr = dma_addr(addr20);
        let buffers = self.inner.as_ref()?;
        let entry = buffers.pages.entry(addr);
        let off = pagetable::arena_addr(entry, addr);
        (pagetable::slow_readable(pagetable::flags(entry)) && off < Arena::LEN).then_some(off)
    }

    /// Arena offset of `addr20` when its page is mapped and writable and the offset is inside the
    /// arena.
    #[inline]
    fn writable_at(&self, addr20: u32) -> Option<usize> {
        let addr = dma_addr(addr20);
        let buffers = self.inner.as_ref()?;
        let entry = buffers.pages.entry(addr);
        let off = pagetable::arena_addr(entry, addr);
        (pagetable::slow_writable(pagetable::flags(entry)) && off < Arena::LEN).then_some(off)
    }

    /// Fills `out` from `addr20` upwards and reports whether every byte came from a mapped
    /// readable page. Each byte resolves on its own, so a buffer can cross a page boundary. A
    /// refused byte reads 0 and makes the answer false: the transfer completes with zeros rather
    /// than stopping the machine, and the caller logs it.
    pub fn read(&self, addr20: u32, out: &mut [u8]) -> bool {
        let mut ok = true;
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = match self.readable_at(addr20.wrapping_add(i as u32)) {
                // `readable_at` bounded the offset and implies the buffers are there.
                Some(off) => self
                    .inner
                    .as_ref()
                    .expect("readable_at saw them")
                    .arena
                    .bytes()[off],
                None => {
                    ok = false;
                    0
                }
            };
        }
        ok
    }

    /// Writes `bytes` at `addr20` upwards and reports whether every byte reached a mapped writable
    /// page. A refused byte is dropped and the rest are still written: a DMA engine cannot undo
    /// the part of a burst it already placed.
    pub fn write(&mut self, addr20: u32, bytes: &[u8]) -> bool {
        let mut ok = true;
        for (i, byte) in bytes.iter().enumerate() {
            match self.writable_at(addr20.wrapping_add(i as u32)) {
                Some(off) => {
                    // `writable_at` returned `Some`, so the buffers are there.
                    self.inner
                        .as_mut()
                        .expect("writable_at saw them")
                        .arena
                        .bytes_mut()[off] = *byte;
                }
                None => ok = false,
            }
        }
        ok
    }

    /// The little-endian word at `addr20`, or `None` when any byte is unreadable (a descriptor
    /// read).
    pub fn read_u32(&self, addr20: u32) -> Option<u32> {
        let mut bytes = [0u8; 4];
        self.read(addr20, &mut bytes)
            .then(|| u32::from_le_bytes(bytes))
    }

    /// Writes the little-endian word `val` at `addr20` and reports whether all four bytes landed
    /// (a descriptor write-back).
    pub fn write_u32(&mut self, addr20: u32, val: u32) -> bool {
        self.write(addr20, &val.to_le_bytes())
    }
}

impl Default for DmaView<'_> {
    fn default() -> Self {
        DmaView::detached()
    }
}

/// [`DmaMem`] over the arena and the page table through [`DmaView`]. An unreadable byte reads 0
/// and an unwritable one is dropped; each such access is counted into `faults`.
pub struct ArenaDma<'a> {
    view: DmaView<'a>,
    faults: &'a mut u64,
}

impl<'a> ArenaDma<'a> {
    /// The adapter over `view`, counting refused accesses into `faults`.
    pub fn new(view: DmaView<'a>, faults: &'a mut u64) -> ArenaDma<'a> {
        ArenaDma { view, faults }
    }
}

impl DmaMem for ArenaDma<'_> {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        if !self.view.read(addr & DMA_ADDR_MASK, out) {
            *self.faults += 1;
        }
    }

    fn write(&mut self, addr: u32, data: &[u8]) {
        if !self.view.write(addr & DMA_ADDR_MASK, data) {
            *self.faults += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Soc;
    use crate::mem::{SRAM1_DRAM_BASE, SRAM1_IRAM_BASE};
    use crate::pagetable;

    /// A view over a fresh SoC.
    fn view(soc: &mut Soc) -> DmaView<'_> {
        DmaView::new(&soc.pages, &mut soc.arena)
    }

    #[test]
    fn a_link_address_is_rebuilt_as_the_dram_window_address() {
        assert_eq!(dma_addr(0x8_0000), SRAM1_DRAM_BASE);
        assert_eq!(dma_addr(0x8_1234), 0x3FC8_1234);
        assert_eq!(dma_addr(0), DMA_BASE);
        assert_eq!(dma_addr(DMA_ADDR_MASK), DMA_BASE + DMA_ADDR_MASK);
        // Bits above the 20 the register holds are dropped: an address inside the window
        // rebuilds to itself, one outside folds in.
        assert_eq!(dma_addr(0x3FC8_1234), 0x3FC8_1234);
        // The two SRAM1 views differ by 0x700000, a whole number of megabytes, so the IRAM
        // address of a byte folds onto its DRAM address.
        assert_eq!(dma_addr(SRAM1_IRAM_BASE), SRAM1_DRAM_BASE);
        assert_eq!(dma_addr(u32::MAX), DMA_BASE + DMA_ADDR_MASK);
        assert!(in_dma_window(DMA_BASE) && in_dma_window(DMA_BASE + DMA_WINDOW_LEN - 1));
        assert!(!in_dma_window(DMA_BASE - 1) && !in_dma_window(DMA_BASE + DMA_WINDOW_LEN));
        assert!(in_dma_window(SRAM1_DRAM_BASE) && in_dma_window(0x3FCD_FFFF));
        assert!(!in_dma_window(SRAM1_IRAM_BASE));
    }

    #[test]
    fn the_view_moves_bytes_through_the_page_table() {
        let mut soc = Soc::default();
        // A descriptor word and a buffer in the SRAM1 DRAM view, addressed by their 20 bits.
        assert!(view(&mut soc).write_u32(0x8_0000, 0xDEAD_BEEF));
        assert!(view(&mut soc).write(0x8_0010, b"passport"));

        assert_eq!(view(&mut soc).read_u32(0x8_0000), Some(0xDEAD_BEEF));
        let mut buf = [0u8; 8];
        assert!(view(&mut soc).read(0x8_0010, &mut buf));
        assert_eq!(&buf, b"passport");

        // The same bytes through the CPU address: one translation, two names.
        assert_eq!(soc.load_mem(SRAM1_DRAM_BASE, 4), Some(0xDEAD_BEEF));
        // SRAM1 is one physical block, so the IRAM view shows the same byte.
        assert_eq!(soc.load_mem(SRAM1_IRAM_BASE, 4), Some(0xDEAD_BEEF));
    }

    #[test]
    fn a_buffer_that_crosses_a_page_boundary_reaches_both_pages() {
        let mut soc = Soc::default();
        // Six bytes straddling the boundary between the first and the second DRAM page.
        let at = 0x8_0FFD;
        assert!(view(&mut soc).write(at, &[1, 2, 3, 4, 5, 6]));
        let mut buf = [0u8; 6];
        assert!(view(&mut soc).read(at, &mut buf));
        assert_eq!(buf, [1, 2, 3, 4, 5, 6]);
        assert_eq!(soc.load_mem(SRAM1_DRAM_BASE + 0x1000, 4), Some(0x0006_0504));
    }

    #[test]
    fn an_unmapped_or_read_only_page_is_reported_rather_than_silently_dropped() {
        let mut soc = Soc::default();
        // 0x3FC00000 to 0x3FC7FFFF is inside the DMA window but backed by nothing.
        let mut buf = [0xAAu8; 4];
        assert!(!view(&mut soc).read(0, &mut buf));
        assert_eq!(buf, [0, 0, 0, 0], "an unmapped page reads 0");
        assert!(!view(&mut soc).write(0, &[1, 2, 3, 4]));
        assert_eq!(view(&mut soc).read_u32(0), None);

        let top = 0xD_FFFE;
        assert!(!view(&mut soc).write(top, &[7, 7, 7, 7]));
        assert_eq!(soc.load_mem(0x3FCD_FFFE, 2), Some(0x0707));
        assert_eq!(
            soc.load_mem(0x3FCE_0000, 2),
            None,
            "above SRAM1 is unbacked"
        );
    }

    /// A detached view must refuse, not read zeros, or a missing wiring would look like a legal
    /// transfer.
    #[test]
    fn a_detached_view_refuses_every_access_rather_than_reading_zeros() {
        let mut view = DmaView::detached();
        let mut out = [0xAAu8; 4];
        assert!(!view.read(0, &mut out));
        assert_eq!(out, [0; 4], "a refused read still leaves zeros");
        assert!(!view.write(0, &[1, 2, 3, 4]));
        assert_eq!(view.read_u32(0), None);
        assert!(!view.write_u32(0, 0xDEAD_BEEF));
        assert_eq!(view.read_u32(0xF_FFFC), None);
        assert!(DmaView::default().read_u32(0).is_none());
    }

    #[test]
    fn an_entry_pointing_past_the_arena_is_refused_on_both_sides() {
        // The DMA view must refuse an offset past the arena as `Soc::resolve` does, or a
        // mis-scaled MMU entry would give a channel a zero-filled descriptor read that succeeds.
        let mut soc = Soc::default();
        // 0x3FCE0000 to 0x3FCFFFFF is inside the window and covered by no `Region`.
        let page = 0x3FCE_1000;
        soc.pages.set_entry(
            page >> 12,
            pagetable::entry(0x7FFF_F000, pagetable::PF_R | pagetable::PF_W),
        );
        assert!(
            pagetable::arena_off(soc.pages.entry(page)) >= Arena::LEN,
            "the entry names an offset past the end of the arena"
        );

        let addr20 = page & DMA_ADDR_MASK;
        let mut buf = [0xAAu8; 4];
        assert!(!view(&mut soc).read(addr20, &mut buf), "read reports it");
        assert_eq!(buf, [0, 0, 0, 0]);
        assert_eq!(view(&mut soc).read_u32(addr20), None);
        assert!(
            !view(&mut soc).write(addr20, &[1, 2, 3, 4]),
            "so does write"
        );
        assert!(!view(&mut soc).write_u32(addr20, 1));
        // The same answer `Soc::resolve` gives the CPU slow path. The fast path indexes the arena
        // unchecked and reads 0 past its end; the slow path decides whether an access happened.
        assert_eq!(soc.load(page + 0xFFE, 4), Err(crate::Refused::Unbacked));
        assert_eq!(soc.store_mem(page + 0xFFE, 4, 1), crate::Stored::Unbacked);
        assert!(soc.arena.guards_intact());
    }
}
