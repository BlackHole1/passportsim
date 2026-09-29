//! Export redaction, and the secret sources a `SecretSet` is built from.

use std::collections::BTreeSet;
use std::ops::Range;

use pemu_core::serde::de::DeserializeOwned;
use pemu_core::snap::{SectionId, SnapError, Snapshot};
use pemu_loader::partitions::{self, PartitionTable};
use pemu_soc_c3::flash_store::{self, DeltaPage, FlashDelta};
use pemu_soc_c3::mem;

use super::{FlashCacheSection, SOC_FLASH_CACHE, decode, decode_section, encode};
use crate::machine::Machine;

/// The flash cardid window an export replaces with 0xFF.
pub const CARDID_WINDOW: Range<u32> = 0x35_6000..0x35_A000;

/// A flash range an export erased.
#[derive(Clone, PartialEq, Eq)]
pub enum Erased {
    /// The cardid window, with its bytes before the erase, which `pemu-api` salts into the
    /// `Redacted{salted_sha256}` label. They never leave the process; `Debug` prints the length.
    CardId { range: Range<u32>, content: Vec<u8> },
    /// An `nvs` or `nvs_keys` partition, erased without a label.
    Nvs { range: Range<u32> },
}

impl std::fmt::Debug for Erased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Erased::CardId { range, content } => f
                .debug_struct("CardId")
                .field("range", range)
                .field("content_len", &content.len())
                .finish(),
            Erased::Nvs { range } => f.debug_struct("Nvs").field("range", range).finish(),
        }
    }
}

/// What [`Machine::redact`] removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Redaction {
    /// Every range erased, the cardid window first, then the NVS partitions in table order.
    pub erased: Vec<Erased>,
    /// The SPI1 `W` buffer or a latched program held erased flash bytes and was cleared.
    pub spi_mem_cleared: bool,
}

/// The bytes a machine's `SecretSet` (`pemu_api::secret_set`) is built from, as its flash and
/// eFuse hold them now. `Debug` prints lengths only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretSources {
    /// Moves whenever these sources may differ from the last ones read.
    pub generation: u64,
    pub cardid_window: Vec<u8>,
    /// Each `nvs` and `nvs_keys` partition of the current partition table and of the base image's,
    /// in the order an export erases them.
    pub nvs_partitions: Vec<Vec<u8>>,
    /// `(index, bytes)` of BLK1 and BLK2 in the espefuse dump layout, for an imported eFuse only:
    /// a synthesized eFuse is not a device's, and its MAC may be printed.
    pub efuse_blocks: Vec<(u8, Vec<u8>)>,
    pub efuse_imported: bool,
    /// The NFC card memory, 45 pages of 4 bytes. A PWD, PACK or Wi-Fi key the caller wrote is a
    /// secret input; the seed-drawn UID in pages 0 to 2 is not.
    pub nfc_card: Vec<u8>,
    /// Command frames of the journaled `NfcTap` inputs: a PWD_AUTH carries a password the card
    /// never stores.
    pub nfc_frames: Vec<Vec<u8>>,
    /// Every pre-shared key of the scripted Wi-Fi world: a caller-supplied key taints the
    /// instance and every output masks it.
    pub wifi_keys: Vec<Vec<u8>>,
    /// A bound radio module's state taints the instance without adding a member, such as a BLE
    /// btsnoop capture whose pairing keys an export cannot mask.
    pub radio_state_taints: bool,
}

impl std::fmt::Debug for SecretSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSources")
            .field("generation", &self.generation)
            .field("cardid_window_len", &self.cardid_window.len())
            .field(
                "nvs_partition_lens",
                &self.nvs_partitions.iter().map(Vec::len).collect::<Vec<_>>(),
            )
            .field(
                "efuse_blocks",
                &self
                    .efuse_blocks
                    .iter()
                    .map(|(i, _)| *i)
                    .collect::<Vec<_>>(),
            )
            .field("efuse_imported", &self.efuse_imported)
            .field("nfc_card_len", &self.nfc_card.len())
            .field("nfc_frames", &self.nfc_frames.len())
            .field("wifi_keys", &self.wifi_keys.len())
            .field("radio_state_taints", &self.radio_state_taints)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashView {
    /// The image the machine was built from: decides whether the loaded input was secret-bearing.
    Image,
    /// The flash as the guest reads it now: what an export must remove.
    Current,
}

/// Partition subtype `nvs_keys` (`SUBTYPES` in `gen_esp32part.py`): erased with the NVS
/// partitions it unlocks.
const SUBTYPE_NVS_KEYS: u8 = 0x04;

impl Machine {
    /// Applies the export rules to `snap`, an earlier snapshot of this run identity, and stamps it
    /// `exported` and `redacted` with no eFuse hash:
    ///
    /// - `soc.efuse` loses its words (image and `RD_*` shadow zeroed), and what power-on derives
    ///   from them is cleared too (APB_SARADC calibration, RTC_CNTL `EFUSE_WDT_DELAY_SEL`);
    /// - every page of the cardid window and of each `nvs` and `nvs_keys` partition becomes an
    ///   erased page in `flash_delta` and in a stale `soc.flash_cache` page;
    /// - flash bytes of those ranges still held by the SPI1 host become 0xFF. Copies in guest RAM
    ///   have no address left and are for the `pemu-api` value pass.
    ///
    /// Returns the ranges erased with the cardid window's prior content; the machine holds no
    /// salt. A snapshot whose sections do not decode is refused, `snap` unchanged.
    pub fn redact(&self, snap: &mut Snapshot) -> Result<Redaction, SnapError> {
        use pemu_soc_c3::periph::{efuse, rtc_cntl, saradc, spi_mem};
        fn model<T: DeserializeOwned>(
            snap: &Snapshot,
            block: &'static str,
        ) -> Result<T, SnapError> {
            let id = SectionId::soc(block);
            decode_section(snap.section(&id)?, id.clone(), block)
        }
        let mut fuses: efuse::Model = model(snap, "efuse")?;
        let mut adc: saradc::Model = model(snap, "saradc")?;
        let mut rtc: rtc_cntl::Model = model(snap, "rtc_cntl")?;
        let mut delta: FlashDelta = decode(snap, SectionId::FLASH_DELTA, "flash_delta")?;
        let mut cache: FlashCacheSection = decode(snap, SOC_FLASH_CACHE, "soc.flash_cache")?;
        let mut spi1: spi_mem::Spi1Model = model(snap, "spi1")?;

        fuses.redact_image();
        adc.set_calibration(Default::default());
        rtc.restore_wdt_delay_sel(0);
        let ranges = self.secret_flash_ranges(&delta);
        let mut redaction = Redaction::default();
        for (i, range) in ranges.iter().enumerate() {
            redaction.erased.push(if i == 0 {
                Erased::CardId {
                    range: range.clone(),
                    content: self.flash_view(&delta, range.clone()),
                }
            } else {
                Erased::Nvs {
                    range: range.clone(),
                }
            });
        }
        redaction.spi_mem_cleared =
            spi1.redact_flash(&|start, end| ranges.iter().any(|r| start < r.end && r.start < end));
        let len = flash_store::PAGE_LEN;
        let secret: BTreeSet<u32> = ranges
            .iter()
            .flat_map(|r| r.start / len..r.end.div_ceil(len).min(flash_store::PAGES))
            .collect();
        let erased = || vec![flash_store::ERASED; flash_store::PAGE_LEN as usize];
        delta.pages.retain(|p| !secret.contains(&p.page));
        delta.pages.extend(secret.iter().map(|&page| DeltaPage {
            page,
            bytes: erased(),
        }));
        delta.pages.sort_by_key(|p| p.page);
        for p in &mut cache.stale {
            if secret.contains(&p.page) {
                p.bytes = erased();
            }
        }

        snap.put_raw(SectionId::soc("efuse"), encode(&fuses));
        snap.put_raw(SectionId::soc("saradc"), encode(&adc));
        snap.put_raw(SectionId::soc("rtc_cntl"), encode(&rtc));
        if redaction.spi_mem_cleared {
            snap.put_raw(SectionId::soc("spi1"), encode(&spi1));
        }
        snap.put_raw(SectionId::new(SectionId::FLASH_DELTA), encode(&delta));
        snap.put_raw(SectionId::new(SOC_FLASH_CACHE), encode(&cache));
        snap.header.exported = true;
        snap.header.redacted = true;
        snap.header.efuse_hash = [0; 32];
        Ok(redaction)
    }

    /// The bytes of `range` in a snapshot's flash view: the base image (0xFF past its end) under
    /// the pages `delta` writes.
    fn flash_view(&self, delta: &FlashDelta, range: Range<u32>) -> Vec<u8> {
        let (start, end) = (range.start as usize, range.end as usize);
        let image = self.soc.flash.image();
        let mut out: Vec<u8> = (start..end)
            .map(|at| image.get(at).copied().unwrap_or(flash_store::ERASED))
            .collect();
        let page_len = flash_store::PAGE_LEN as usize;
        for p in &delta.pages {
            let page_start = p.page as usize * page_len;
            let lo = page_start.max(start);
            let hi = (page_start + page_len).min(end);
            if lo < hi {
                out[lo - start..hi - start]
                    .copy_from_slice(&p.bytes[lo - page_start..hi - page_start]);
            }
        }
        out
    }

    /// The ranges an export erases, from both the base image's partition table and the one `delta`
    /// writes, so a guest that rewrote the table still loses the old partitions.
    fn secret_flash_ranges(&self, delta: &FlashDelta) -> Vec<Range<u32>> {
        let (at, len) = (partitions::TABLE_OFFSET, partitions::TABLE_MAX_LEN);
        let image = self.soc.flash.image();
        let base = image.get(at..at + len).unwrap_or(&[]);
        let mut written = if base.len() == len {
            base.to_vec()
        } else {
            vec![flash_store::ERASED; len]
        };
        let page_len = flash_store::PAGE_LEN as usize;
        for p in &delta.pages {
            let start = p.page as usize * page_len;
            for (i, slot) in written.iter_mut().enumerate() {
                if (start..start + page_len).contains(&(at + i)) {
                    *slot = p.bytes[at + i - start];
                }
            }
        }
        self.secret_ranges_under(&written)
    }

    fn secret_ranges_under(&self, written: &[u8]) -> Vec<Range<u32>> {
        let mut ranges = vec![CARDID_WINDOW];
        let (at, len) = (partitions::TABLE_OFFSET, partitions::TABLE_MAX_LEN);
        let image = self.soc.flash.image();
        let base = image.get(at..at + len).unwrap_or(&[]);
        for table in [written, base] {
            let Ok(table) = PartitionTable::parse(table) else {
                continue;
            };
            ranges.extend(
                table
                    .entries
                    .iter()
                    .filter(|p| {
                        p.ptype == partitions::ptype::DATA
                            && matches!(p.subtype, partitions::subtype::NVS | SUBTYPE_NVS_KEYS)
                    })
                    .map(|p| p.offset..u32::try_from(p.end()).unwrap_or(u32::MAX)),
            );
        }
        let mut unique: Vec<Range<u32>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if !unique.contains(&range) {
                unique.push(range);
            }
        }
        unique
    }

    /// The flash store's change counter combined with the journal's next sequence number, which
    /// every NFC tap moves. Compared for equality only; the eFuse never changes under a machine.
    pub fn secret_generation(&self) -> u64 {
        self.soc
            .flash
            .changes()
            .wrapping_add(self.journal.next_seq().wrapping_mul(1 << 32))
    }

    /// The cardid window and NVS partitions as `view` holds them, and an imported eFuse's BLK1/2.
    pub fn secret_sources(&self, view: FlashView) -> SecretSources {
        let flash = &self.soc.flash;
        let image = flash.image();
        let read = |range: &Range<u32>| {
            let end = range.end.min(mem::FLASH_LEN);
            let mut out = vec![flash_store::ERASED; end.saturating_sub(range.start) as usize];
            match view {
                FlashView::Current => flash.read(range.start, &mut out),
                FlashView::Image => {
                    for (i, slot) in out.iter_mut().enumerate() {
                        *slot = image
                            .get(range.start as usize + i)
                            .copied()
                            .unwrap_or(flash_store::ERASED);
                    }
                }
            }
            out
        };
        let table = read(
            &(partitions::TABLE_OFFSET as u32
                ..(partitions::TABLE_OFFSET + partitions::TABLE_MAX_LEN) as u32),
        );
        let ranges = self.secret_ranges_under(&table);
        let efuse = &self.assets.efuse;
        let efuse_blocks = if efuse.tainted() {
            [1u8, 2]
                .into_iter()
                .map(|block| {
                    let words = pemu_loader::efuse_image::BLOCK_WORDS[usize::from(block)];
                    let bytes = efuse.words()[usize::from(block)][..words]
                        .iter()
                        .flat_map(|w| w.to_le_bytes())
                        .collect();
                    (block, bytes)
                })
                .collect()
        } else {
            Vec::new()
        };
        SecretSources {
            generation: self.secret_generation(),
            cardid_window: read(&ranges[0]),
            nvs_partitions: ranges[1..].iter().map(read).collect(),
            efuse_blocks,
            efuse_imported: efuse.tainted(),
            nfc_card: (0..=pemu_board::ntag213::PAGE_MAX)
                .flat_map(|page| self.board.world.card.page(page).unwrap_or_default())
                .collect(),
            nfc_frames: self
                .journal
                .entries()
                .iter()
                .filter_map(|entry| match &entry.ev {
                    pemu_core::input::InputEvent::NfcTap { ops } => Some(ops),
                    _ => None,
                })
                .flatten()
                .filter_map(|op| match op {
                    pemu_core::input::NfcOp::Cmd(frame) => Some(frame.clone()),
                    _ => None,
                })
                .collect(),
            wifi_keys: self.wifi_keys(),
            radio_state_taints: self.radio_state_taints(),
        }
    }

    fn radio_state_taints(&self) -> bool {
        pemu_radio::modules().iter().any(|module| {
            self.radio_module_state(module.name())
                .is_some_and(|bytes| module.state_taints(bytes))
        })
    }

    /// Every pre-shared key, deduplicated, from both the radio modules' state
    /// (`RadioModule::secret_values`) and the journal's `EnvChange::WifiAps` entries: the journal
    /// is in a snapshot too, so a key journaled with no Wi-Fi module bound is still in an export.
    fn wifi_keys(&self) -> Vec<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut add = |psk: &Vec<u8>| {
            if !psk.is_empty() && !keys.contains(psk) {
                keys.push(psk.clone());
            }
        };
        for module in pemu_radio::modules() {
            let Some(bytes) = self.radio_module_state(module.name()) else {
                continue;
            };
            for value in module.secret_values(bytes) {
                add(&value);
            }
        }
        for entry in self.journal.entries() {
            if let pemu_core::input::InputEvent::Env(pemu_core::input::EnvChange::WifiAps(aps)) =
                &entry.ev
            {
                for ap in aps {
                    add(&ap.psk);
                }
            }
        }
        keys
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use super::*;
    use crate::config::{Assets, MachineConfig};
    use crate::snapshot::tests::{machine, restored_fresh, run_insns};
    use pemu_core::snap::SnapOpts;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;

    /// One synthetic partition table row (`<2sBBLL16sL`, `gen_esp32part.py`).
    fn partition_row(ptype: u8, subtype: u8, offset: u32, size: u32, name: &str) -> Vec<u8> {
        let mut row = vec![0xAA, 0x50, ptype, subtype];
        row.extend_from_slice(&offset.to_le_bytes());
        row.extend_from_slice(&size.to_le_bytes());
        let mut label = [0u8; 16];
        label[..name.len()].copy_from_slice(name.as_bytes());
        row.extend_from_slice(&label);
        row.extend_from_slice(&0u32.to_le_bytes());
        row
    }

    /// A snapshot that is not an export keeps the bytes. Every byte here is synthetic.
    #[test]
    fn an_export_erases_the_nvs_partitions_and_the_cardid_window_and_a_save_keeps_them() {
        let mut m = machine();
        let mut table = partition_row(0x01, 0x02, 0x9000, 0x6000, "nvs");
        table.extend(partition_row(0x01, 0x01, 0xF000, 0x1000, "phy_init"));
        table.extend(partition_row(0x01, 0x04, 0x1_0000, 0x1000, "nvs_keys"));
        m.soc.flash.program(0x8000, &table);
        let marker = [0x5A, 0xA5, 0x3C, 0xC3];
        for addr in [0x9000, 0xE004, 0xF000, 0x1_0000, 0x35_6000, 0x35_9FFC] {
            m.soc.flash.program(addr, &marker);
        }
        // A stale cache page inside the NVS partition.
        let nvs_page = 0x9000 / flash_store::PAGE_LEN;
        let at = (mem::FLASH_ARENA + nvs_page * flash_store::PAGE_LEN) as usize;
        m.soc.arena.bytes_mut()[at..at + 4].copy_from_slice(&marker);

        let flash_of = |snap: &Snapshot| -> (FlashDelta, FlashCacheSection) {
            (
                decode(snap, SectionId::FLASH_DELTA, "flash_delta").expect("delta"),
                decode(snap, SOC_FLASH_CACHE, "soc.flash_cache").expect("cache"),
            )
        };
        let page_of = |delta: &FlashDelta, addr: u32| {
            delta
                .pages
                .iter()
                .find(|p| p.page == addr / flash_store::PAGE_LEN)
                .map(|p| p.bytes.clone())
        };
        let erased = vec![flash_store::ERASED; flash_store::PAGE_LEN as usize];

        let save = m.snapshot(SnapOpts::default());
        let (saved, saved_cache) = flash_of(&save);
        for addr in [0x9000, 0xE004, 0x1_0000, 0x35_6000, 0x35_9FFC] {
            let page = page_of(&saved, addr).expect("the save keeps the written page");
            let off = (addr % flash_store::PAGE_LEN) as usize;
            assert_eq!(page[off..off + 4], marker, "{addr:#x}");
        }
        assert!(
            saved_cache
                .stale
                .iter()
                .any(|p| p.page == nvs_page && p.bytes[..4] == marker)
        );

        let export = m.snapshot(SnapOpts {
            export: true,
            include_secrets: false,
        });
        let (delta, cache) = flash_of(&export);
        // A save redacted later, as `snapshot export` of a named save does, is the same export.
        let mut later = save.clone();
        let redaction = m
            .redact(&mut later)
            .expect("a snapshot of this machine redacts");
        assert!(later == export, "redacting a save equals exporting");
        let mut window = vec![flash_store::ERASED; CARDID_WINDOW.len()];
        window[..4].copy_from_slice(&marker);
        window[0x3FFC..].copy_from_slice(&marker);
        assert_eq!(
            redaction.erased,
            vec![
                Erased::CardId {
                    range: CARDID_WINDOW,
                    content: window,
                },
                Erased::Nvs {
                    range: 0x9000..0xF000,
                },
                Erased::Nvs {
                    range: 0x1_0000..0x1_1000,
                },
            ]
        );
        assert!(!redaction.spi_mem_cleared, "SPI1 held no flash bytes");
        for addr in [
            0x9000, 0xA000, 0xE004, 0x1_0000, 0x35_6000, 0x35_7000, 0x35_9FFC,
        ] {
            assert_eq!(page_of(&delta, addr), Some(erased.clone()), "{addr:#x}");
        }
        assert_eq!(
            page_of(&delta, 0x35_A000),
            None,
            "the window ends at 0x35A000"
        );
        let phy = page_of(&delta, 0xF000).expect("a phy_init page is not a secret");
        assert_eq!(phy[..4], marker);
        assert_eq!(page_of(&delta, 0x8000), page_of(&saved, 0x8000));
        let stale = cache
            .stale
            .iter()
            .find(|p| p.page == nvs_page)
            .expect("stale page");
        assert_eq!(stale.bytes, erased);

        // Restored, the guest reads erased NVS and cardid bytes through the store and the cache.
        let restored = restored_fresh(&export, MachineConfig::default());
        let mut buf = [0u8; 4];
        restored.soc.flash.read(0x35_6000, &mut buf);
        assert_eq!(buf, [0xFF; 4]);
        assert_eq!(restored.soc.arena.bytes()[at..at + 4], [0xFF; 4]);

        let with_secrets = m.snapshot(SnapOpts {
            export: true,
            include_secrets: true,
        });
        assert_eq!(flash_of(&with_secrets).0, saved);
    }

    /// Residue of an earlier secret read can sit above the last transaction.
    #[test]
    fn an_export_clears_the_spi1_buffer_whatever_its_address_register_names() {
        use pemu_core::regstore::Size;
        use pemu_soc_c3::r#gen::regs_spi1::REGS;
        use pemu_soc_c3::r#gen::regs_spi1::idx;
        let mut m = machine();
        let now = m.now();
        for (reg, val) in [
            (idx::SPI_MEM_ADDR, 0x0002_0000),
            (idx::SPI_MEM_W0, 0x1234_5678),
        ] {
            m.soc.devices.spi1.store(
                u32::from(REGS[reg].off),
                Size::B4,
                val,
                now,
                &mut m.ledger,
                &mut m.sched,
            );
        }
        let w0 = |snap: &Snapshot| {
            let spi1: pemu_soc_c3::periph::spi_mem::Spi1Model = decode_section(
                snap.section(&SectionId::soc("spi1")).unwrap(),
                SectionId::soc("spi1"),
                "spi1",
            )
            .expect("decodes");
            spi1.regs().get(idx::SPI_MEM_W0)
        };
        let save = m.snapshot(SnapOpts::default());
        assert_eq!(w0(&save), 0x1234_5678);
        let mut export = save.clone();
        let redaction = m.redact(&mut export).expect("redacts");
        assert!(redaction.spi_mem_cleared);
        assert_eq!(w0(&export), 0xFFFF_FFFF);
    }

    #[test]
    fn an_export_without_secrets_is_stamped_and_restores_on_the_same_assets() {
        let mut m = machine();
        run_insns(&mut m, 20_000);
        let export = m.snapshot(SnapOpts {
            export: true,
            include_secrets: false,
        });
        assert!(export.header.exported && export.header.redacted);
        assert_eq!(export.header.efuse_hash, [0; 32], "no unsalted eFuse hash");
        assert_ne!(m.snapshot(SnapOpts::default()).header.efuse_hash, [0; 32]);
        let mut target = machine();
        target.restore(&export).expect("restores");
        // The export cleared the SPI1 `W` buffer the ROM's flash reads filled.
        m.soc.devices.spi1.redact_flash(&|_, _| false);
        assert_eq!(target.state_hash(), m.state_hash());
    }

    /// A machine on a synthetic eFuse marked as a dump. Every word is nonzero, so a zeroed register
    /// is a redacted one; the words imply a nonzero ADC calibration and `EFUSE_WDT_DELAY_SEL` 3.
    fn tainted_machine(seed: u64) -> Machine {
        let mut synth = EfuseImage::synth(seed);
        synth.set(
            pemu_loader::efuse_image::field::ADC1_CAL_VOL_ATTEN[3],
            0x2A5,
        );
        let words: Vec<u8> = synth
            .dump_words()
            .iter()
            .enumerate()
            .flat_map(|(i, w)| {
                let sel = if i == 2 { 0x3 << 16 } else { 0 };
                (w | sel | (0x0101_0000 << (i % 8))).to_le_bytes()
            })
            .collect();
        let efuse = EfuseImage::from_dump(&words).expect("a dump of 84 words");
        assert!(efuse.tainted());
        let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, efuse)
            .expect("the bundled ROM is pinned");
        Machine::new(tainted_config(), assets).expect("the ROM fits the ROM window")
    }

    fn tainted_config() -> MachineConfig {
        MachineConfig {
            efuse: crate::config::EfuseSource::Dump,
            ..MachineConfig::default()
        }
    }

    #[test]
    fn the_secret_sources_follow_guest_writes_and_restores_and_name_only_imported_efuse() {
        let mut m = machine();
        let fresh = m.snapshot(SnapOpts::default());
        let g0 = m.secret_generation();
        let current = m.secret_sources(FlashView::Current);
        assert_eq!(current.generation, g0);
        assert_eq!(current.cardid_window.len(), CARDID_WINDOW.len());
        assert!(current.cardid_window.iter().all(|&b| b == 0xFF));
        assert!(
            current.nvs_partitions.is_empty(),
            "an erased flash has no partition table"
        );
        assert!(current.efuse_blocks.is_empty() && !current.efuse_imported);

        m.soc.flash.program(CARDID_WINDOW.start + 40, &[0x5A, 0xA5]);
        let g1 = m.secret_generation();
        assert_ne!(g1, g0, "a program moves the generation");
        let written = m.secret_sources(FlashView::Current);
        assert_eq!(&written.cardid_window[40..42], &[0x5A, 0xA5]);
        let image = m.secret_sources(FlashView::Image);
        assert!(
            image.cardid_window.iter().all(|&b| b == 0xFF),
            "the image is not written"
        );

        m.restore(&fresh).expect("restores");
        assert!(m.secret_generation() > g1, "a restore moves the generation");
        assert!(
            m.secret_sources(FlashView::Current)
                .cardid_window
                .iter()
                .all(|&b| b == 0xFF)
        );

        let tainted = tainted_machine(1).secret_sources(FlashView::Image);
        assert!(tainted.efuse_imported);
        let blocks: Vec<(u8, usize)> = tainted
            .efuse_blocks
            .iter()
            .map(|(i, b)| (*i, b.len()))
            .collect();
        assert_eq!(blocks, [(1, 24), (2, 32)]);
        assert!(
            !format!("{tainted:?}").contains("0x"),
            "Debug prints no bytes"
        );
    }

    /// A restore on the same eFuse assets reloads the words and reaches the source's `state_hash`.
    #[test]
    fn a_redacted_export_of_a_tainted_machine_keeps_the_efuse_model_without_its_words() {
        use pemu_soc_c3::r#gen::regs_efuse::idx;
        use pemu_soc_c3::periph::efuse;

        let mut m = tainted_machine(11);
        run_insns(&mut m, 20_000);
        let id = SectionId::soc("efuse");
        let model_of = |snap: &Snapshot| -> efuse::Model {
            decode(snap, id.as_str(), "soc.efuse").expect("the eFuse model is there")
        };
        let rd = idx::EFUSE_RD_WR_DIS..idx::EFUSE_RD_WR_DIS + efuse::RD_WORDS;

        let saved = model_of(&m.snapshot(SnapOpts::default()));
        assert_eq!(saved.image(), m.assets.efuse.dump_words().as_slice());
        assert!(rd.clone().all(|i| saved.regs().get(i) != 0));

        let export = m.snapshot(SnapOpts {
            export: true,
            include_secrets: false,
        });
        let redacted = model_of(&export);
        assert!(redacted.image().iter().all(|w| *w == 0));
        assert!(rd.clone().all(|i| redacted.regs().get(i) == 0));
        assert_eq!(redacted.refused_burns(), saved.refused_burns());

        // What power-on derives from the eFuse words is cleared with them.
        assert_ne!(m.soc.devices.saradc.calibration(), Default::default());
        assert_eq!(m.soc.devices.rtc_cntl.wdt_delay_sel(), 3);
        let saradc: pemu_soc_c3::periph::saradc::Model =
            decode(&export, "soc.saradc", "soc.saradc").expect("saradc");
        assert_eq!(saradc.calibration(), Default::default());
        let rtc: pemu_soc_c3::periph::rtc_cntl::Model =
            decode(&export, "soc.rtc_cntl", "soc.rtc_cntl").expect("rtc_cntl");
        assert_eq!(rtc.wdt_delay_sel(), 0);

        let mut target = tainted_machine(11);
        target
            .restore(&export)
            .expect("restores on the same eFuse assets");
        assert_eq!(
            target.soc.devices.saradc.calibration(),
            m.soc.devices.saradc.calibration()
        );
        assert_eq!(target.soc.devices.rtc_cntl.wdt_delay_sel(), 3);
        // The export cleared the SPI1 `W` buffer; the rest is the machine's state.
        m.soc.devices.spi1.redact_flash(&|_, _| false);
        assert_eq!(target.state_hash(), m.state_hash());
        let mut other = tainted_machine(12);
        other
            .restore(&export)
            .expect("a redacted export skips the eFuse hash");
        assert_eq!(
            other.soc.devices.efuse.image(),
            other.assets.efuse.dump_words().as_slice(),
            "a machine on other eFuse assets takes its own words"
        );
    }
}
