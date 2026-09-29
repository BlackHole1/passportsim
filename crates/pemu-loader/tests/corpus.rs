//! The loader's parsers against the firmware corpus, found through `corpus/MANIFEST.json` below
//! the data root and checked against its SHA-256. Each test skips with a message when the data
//! root, the manifest or a file is absent. No test prints file contents or paths.

// Test-only file and env access; the clippy.toml bans of core crates target non-test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;

use pemu_loader::elf::ElfInfo;
use pemu_loader::esp_image::{CHIP_ID_ESP32C3, EspImage, MergedImage};
use pemu_loader::partitions::{PartitionTable, ptype, subtype};
use pemu_loader::rom::{PinError, ROM_LEN, RomImage, check_pin};
use pemu_loader::symbols::{SymBind, SymKind};
use pemu_loader::{hex, sha256};

const IMAGES: [&str; 8] = [
    "pk",
    "official",
    "goldminer",
    "demo",
    "probe2",
    "scan3",
    "pkgatt",
    "probe-long",
];

/// The rule of `pemu_testkit::corpus` (absolute `PASSPORTSIM_DATA_ROOT` only), copied because this
/// crate may not depend on `pemu-testkit`.
fn data_root() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("PASSPORTSIM_DATA_ROOT")
        .filter(|d| !d.to_string_lossy().trim().is_empty())
    else {
        eprintln!("skip: no data root: set PASSPORTSIM_DATA_ROOT to an absolute path");
        return None;
    };
    let root = PathBuf::from(dir);
    if !root.is_absolute() {
        eprintln!("skip: PASSPORTSIM_DATA_ROOT is not an absolute path");
        return None;
    }
    Some(root)
}

fn is_key(file: &str, key: &str) -> bool {
    match key {
        "pt" => file == "partition-table.bin",
        "bin" => file.ends_with(".bin") && file != "partition-table.bin",
        "boot_elf" => file == "bootloader.elf",
        "elf" => file.ends_with(".elf") && !file.starts_with("bootloader"),
        _ => false,
    }
}

/// The value of `"<key>": "<value>"` inside one manifest record.
fn field(record: &str, key: &str) -> Option<String> {
    let at = record.find(&format!("\"{key}\""))?;
    let rest = &record[at + key.len() + 2..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].replace("\\\\", "\\"))
}

/// `None` after printing why the test skips.
fn corpus(id: &str, key: &str) -> Option<Vec<u8>> {
    let manifest = data_root()?.join("corpus/MANIFEST.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        eprintln!("skip: the corpus manifest is absent");
        return None;
    };
    let record = text.split('{').find(|r| {
        field(r, "id").as_deref() == Some(id) && field(r, "file").is_some_and(|f| is_key(&f, key))
    });
    let (Some(path), Some(digest)) = record
        .map(|r| (field(r, "path"), field(r, "sha256")))
        .unwrap_or((None, None))
    else {
        eprintln!("skip: the corpus manifest has no {id}.{key}");
        return None;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skip: corpus file {id}.{key} is absent");
        return None;
    };
    assert_eq!(
        hex(&sha256(&bytes)),
        digest,
        "{id}.{key} is not the file MANIFEST.json pins"
    );
    Some(bytes)
}

#[test]
fn t1_official_partition_table_rows() {
    let Some(pt) = corpus("official", "pt") else {
        return;
    };
    let table = PartitionTable::parse(&pt).unwrap();
    assert!(table.has_md5);
    let rows: Vec<_> = table
        .entries
        .iter()
        .map(|p| (p.name.as_str(), p.ptype, p.subtype, p.offset, p.size))
        .collect();
    // The rows `gen_esp32part.py` of ESP-IDF v5.5.3 prints for this file.
    assert_eq!(
        rows,
        [
            ("nvs", ptype::DATA, subtype::NVS, 0x9000, 0x6000),
            ("phy_init", ptype::DATA, subtype::PHY, 0xF000, 0x1000),
            ("factory", ptype::APP, subtype::FACTORY, 0x1_0000, 0x30_0000),
            ("cardid", ptype::DATA, subtype::NVS, 0x35_6000, 0x4000),
        ]
    );
    assert_eq!(table.boot_app().map(|p| p.name.as_str()), Some("factory"));
    if let Some(bin) = corpus("official", "bin") {
        assert_eq!(PartitionTable::from_flash(&bin).unwrap(), table);
    }
}

#[test]
fn t1_merged_images_verify() {
    for id in IMAGES {
        let Some(bin) = corpus(id, "bin") else {
            continue;
        };
        let m = MergedImage::parse(&bin).unwrap_or_else(|e| panic!("{id}: {e}"));
        let (part, app) = m
            .app
            .as_ref()
            .unwrap_or_else(|| panic!("{id}: no boot app"));
        for (what, image) in [("bootloader", &m.bootloader), ("app", app)] {
            assert_eq!(image.header.chip_id, CHIP_ID_ESP32C3, "{id} {what}");
            assert!(image.checksum_ok(), "{id} {what} checksum");
            assert_eq!(image.hash_ok(), Some(true), "{id} {what} SHA-256");
        }
        assert!(app.len as u64 <= u64::from(part.size), "{id} app fits");
        let desc = app.app_desc(&bin).unwrap();
        assert!(
            desc.is_some_and(|d| d.has_elf_sha256()),
            "{id} app descriptor"
        );
    }
}

#[test]
fn t1_bootloader_header_is_shared() {
    for id in ["pk", "official", "goldminer"] {
        let Some(bin) = corpus(id, "bin") else {
            continue;
        };
        let b = EspImage::parse(&bin).unwrap();
        let segments: Vec<_> = b.segments.iter().map(|s| (s.load_addr, s.len)).collect();
        let expected = [
            (0x3FCD_5830, 0x1584),
            (0x403C_BF10, 0xC44),
            (0x403C_E710, 0x2FF8),
        ];
        assert_eq!(segments, expected, "{id}");
        assert_eq!(b.header.entry_addr, 0x403C_BF1A, "{id}");
        // esptool header codes: mode 2 is DIO, size 3 is 8 MB, speed 0xF is 80 MHz.
        let h = &b.header;
        assert_eq!((h.spi_mode, h.spi_size, h.spi_speed), (2, 3, 0xF), "{id}");
    }
}

#[test]
fn t1_pk_elf_symbols_descriptor_and_sha256() {
    let (Some(elf_bytes), Some(bin), Some(boot_bytes)) = (
        corpus("pk", "elf"),
        corpus("pk", "bin"),
        corpus("pk", "boot_elf"),
    ) else {
        return;
    };
    let elf = ElfInfo::parse(&elf_bytes).unwrap();
    assert_eq!(elf.sha256, sha256(&elf_bytes));
    let fact = |name: &str| {
        let s = elf.symbols.lookup(name).unwrap();
        (s.addr, s.size, s.kind, s.bind)
    };
    let func = SymKind::Func;
    assert_eq!(
        fact("call_start_cpu0"),
        (elf.entry, 252, func, SymBind::Global)
    );
    assert_eq!(
        fact("s_sleep_hook_register"),
        (0x4200_4EB0, 122, func, SymBind::Local)
    );
    assert_eq!(
        fact("esp_app_desc"),
        (0x3C0C_0020, 256, SymKind::Object, SymBind::Weak)
    );
    let inside = elf.symbols.func_at(0x4200_4EB0 + 60);
    assert_eq!(
        inside.map(|s| s.name.as_str()),
        Some("s_sleep_hook_register")
    );

    let m = MergedImage::parse(&bin).unwrap();
    let (_, app) = m.app.as_ref().unwrap();
    let desc = app.app_desc(&bin).unwrap().unwrap();
    assert_eq!(desc.app_elf_sha256, elf.sha256);
    assert_eq!(desc.project_name, "FoloToy-AI-Passport");
    assert_eq!(desc.idf_ver, "v5.5.3");
    assert_eq!(
        elf.app_desc.as_ref().unwrap().project_name,
        desc.project_name
    );
    assert_eq!(
        (app.header.entry_addr, elf.entry),
        (0x4038_02E8, 0x4038_02E8)
    );

    let boot = ElfInfo::parse(&boot_bytes).unwrap();
    assert_eq!(boot.entry, m.bootloader.header.entry_addr);
    assert!(boot.app_desc.is_none());
}

#[test]
fn t1_corpus_elves_match_their_images() {
    for id in ["pk", "official", "demo", "probe2", "scan3", "pkgatt"] {
        let (Some(elf_bytes), Some(bin)) = (corpus(id, "elf"), corpus(id, "bin")) else {
            continue;
        };
        let elf = ElfInfo::parse(&elf_bytes).unwrap_or_else(|e| panic!("{id}: {e}"));
        let m = MergedImage::parse(&bin).unwrap();
        let (_, app) = m.app.as_ref().unwrap();
        let desc = app.app_desc(&bin).unwrap().unwrap();
        assert_eq!(desc.app_elf_sha256, elf.sha256, "{id}");
        assert_eq!(app.header.entry_addr, elf.entry, "{id}");
    }
}

#[test]
fn t1_rev0_rom_is_unpinned_but_assembles() {
    let Some(bytes) = corpus("rom0", "elf") else {
        return;
    };
    assert!(hex(&sha256(&bytes)).starts_with("11d565922e0a405b"));
    assert_eq!(check_pin(&bytes).err(), Some(PinError::Unpinned));
    let rom = RomImage::from_elf(&bytes).unwrap();
    assert_eq!(rom.pinned(), None);
    assert_eq!(rom.bytes().len(), ROM_LEN);
}
