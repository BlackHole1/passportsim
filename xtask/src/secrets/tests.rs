//! Unit tests of the secrets-check pattern rules.
//!
//! Identity rule: no literal MAC-shaped string outside the `02:00:00` prefix appears
//! in this file; such strings are built at run time from byte arrays. Every sample file is
//! written to a fresh temporary directory.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use super::scan::{Report, Rule, scan_files};
use super::{nvs, patterns, rom};

/// A unicast, non-placeholder address used only to build planted samples.
const PLANTED: [u8; 6] = [0x10, 0x20, 0x30, 0x4a, 0x5b, 0x6c];

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temporary directory standing in for a repository root, removed on drop.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new() -> TempTree {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("secrets-check-test-{}-{unique}-{nanos}", std::process::id());
        let root = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&root).expect("create temp dir");
        TempTree { root }
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, bytes).expect("write sample");
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.root.join(rel)).expect("remove sample");
    }

    fn scan(&self, rels: &[&str]) -> Report {
        let rels: Vec<String> = rels.iter().map(|rel| rel.to_string()).collect();
        scan_files(&self.root, &rels).expect("scan")
    }

    fn root_arg(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Six two-digit lowercase hex groups of `octets` joined by `sep` (as in `02:00:00:dd:ee:ff`).
fn mac_text(octets: [u8; 6], sep: &str) -> String {
    octets
        .iter()
        .map(|o| format!("{o:02x}"))
        .collect::<Vec<_>>()
        .join(sep)
}

fn rules(report: &Report) -> Vec<Rule> {
    report.hits.iter().map(|hit| hit.rule).collect()
}

// ---------------------------------------------------------------------------------------------
// mac-shape
// ---------------------------------------------------------------------------------------------

#[test]
fn mac_shape_fires_on_planted_text() {
    let tree = TempTree::new();
    let colon = mac_text(PLANTED, ":");
    let dash = mac_text(PLANTED, "-").to_uppercase();
    tree.write("docs/notes.md", format!("station {colon}\n").as_bytes());
    tree.write("docs/other.md", format!("(\"{dash}\")\n").as_bytes());
    let report = tree.scan(&["docs/notes.md", "docs/other.md"]);
    assert_eq!(rules(&report), [Rule::MacShape, Rule::MacShape]);
    assert_eq!(report.hits[0].offset, Some(8));
    assert_eq!(report.hits[1].offset, Some(2));
    let rendered = report.render();
    assert!(rendered.contains("mac-shape docs/notes.md:0x8"));
    assert!(
        !rendered.contains(&colon) && !rendered.to_lowercase().contains(&colon.replace(':', "-"))
    );
    assert!(rendered.contains("2 hit(s)"));
}

#[test]
fn mac_shape_passes_clean_text() {
    let tree = TempTree::new();
    let planted = mac_text(PLANTED, ":");
    let mut text = String::new();
    text.push_str("placeholder 02:00:00:12:34:56 and 02-00-00-AB-CD-EF\n");
    text.push_str(&format!("broadcast {}\n", mac_text([0xFF; 6], ":")));
    text.push_str(&format!(
        "multicast {}\n",
        mac_text([0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb], ":")
    ));
    text.push_str(&format!("zero {}\n", mac_text([0; 6], "-")));
    text.push_str("const ADDR: [u8; 6] = [0x10, 0x20, 0x30, 0x4a, 0x5b, 0x6c];\n");
    text.push_str("00000010: 10 20 30 4a 5b 6c 7d 8e  0x102030 0x4a5b6c\n");
    text.push_str(&format!("fingerprint {planted}:7d:8e\n"));
    text.push_str(&format!("prefix 7d:{planted}\n"));
    text.push_str(&format!("glued x{planted} {planted}_y\n"));
    text.push_str(&format!("mixed {}\n", planted.replacen(':', "-", 1)));
    text.push_str("uuid 550e8400-e29b-41d4-a716-446655440000 time 12:34:56 hash 102030405060\n");
    tree.write("src/clean.rs", text.as_bytes());
    let report = tree.scan(&["src/clean.rs"]);
    assert!(report.hits.is_empty(), "{:?}", report.hits);
    assert_eq!(report.files_scanned, 1);
}

#[test]
fn mac_shape_word_before_separator_is_a_boundary() {
    let planted = mac_text(PLANTED, "-");
    let text = format!("MAC-{planted}");
    assert_eq!(patterns::mac_shape_offsets(text.as_bytes()), [4]);
}

#[test]
fn mac_shape_output_is_capped_per_file() {
    let tree = TempTree::new();
    let line = format!("{}\n", mac_text(PLANTED, ":"));
    tree.write("docs/many.md", line.repeat(25).as_bytes());
    let report = tree.scan(&["docs/many.md"]);
    assert_eq!(report.count(Rule::MacShape), 25);
    let rendered = report.render();
    assert_eq!(rendered.lines().filter(|l| l.contains(":0x")).count(), 20);
    assert!(rendered.contains("docs/many.md (5 more)"));
}

// ---------------------------------------------------------------------------------------------
// efuse-dump
// ---------------------------------------------------------------------------------------------

/// A 24-byte BLK1-like block: the planted MAC in bits 0 to 47, most significant octet at 40.
fn blk1_like() -> Vec<u8> {
    let mut block = vec![0u8; patterns::EFUSE_SHORT_BLOCK_LEN];
    for (k, octet) in PLANTED.iter().rev().enumerate() {
        block[k] = *octet;
    }
    block[8] = 0x03;
    block
}

#[test]
fn efuse_dump_fires_on_planted_blocks() {
    let tree = TempTree::new();
    let mut blk2 = vec![0u8; patterns::EFUSE_BLOCK_LEN];
    for (i, byte) in blk2.iter_mut().enumerate().take(16) {
        *byte = (i as u8).wrapping_mul(37).wrapping_add(0x91);
    }
    blk2[16] = 0x01;
    let mut joint = vec![0u8; patterns::EFUSE_JOINT_LEN];
    joint[24..48].copy_from_slice(&blk1_like());
    joint[48..80].copy_from_slice(&blk2);
    tree.write("fixtures/one.dat", &blk1_like());
    tree.write("fixtures/two.dat", &blk2);
    tree.write("fixtures/all.dat", &joint);
    let report = tree.scan(&["fixtures/one.dat", "fixtures/two.dat", "fixtures/all.dat"]);
    assert_eq!(rules(&report), [Rule::EfuseDump; 3]);
    assert!(
        report
            .render()
            .contains("efuse-dump fixtures/two.dat (32 bytes)")
    );
}

#[test]
fn efuse_dump_passes_clean_files() {
    let tree = TempTree::new();
    let mut elf = vec![0u8; 24];
    elf[..4].copy_from_slice(b"\x7fELF");
    let mut other = blk1_like();
    other.extend_from_slice(&[0, 1, 2, 3]);
    tree.write("fixtures/zero.dat", &[0u8; 32]);
    tree.write("fixtures/erased.dat", &[0xFFu8; 24]);
    tree.write("fixtures/elf.dat", &elf);
    tree.write("fixtures/text.dat", b"abcdefghijklmnopqrstuvwx");
    tree.write("fixtures/other.dat", &other);
    tree.write(
        "fixtures/joint-zero.dat",
        &vec![0u8; patterns::EFUSE_JOINT_LEN],
    );
    let report = tree.scan(&[
        "fixtures/zero.dat",
        "fixtures/erased.dat",
        "fixtures/elf.dat",
        "fixtures/text.dat",
        "fixtures/other.dat",
        "fixtures/joint-zero.dat",
    ]);
    assert!(report.hits.is_empty(), "{:?}", report.hits);
}

// ---------------------------------------------------------------------------------------------
// cardid-window
// ---------------------------------------------------------------------------------------------

#[test]
fn cardid_window_fires_on_planted_image() {
    let tree = TempTree::new();
    let mut image = vec![0xFFu8; patterns::CARDID_WINDOW.end];
    image[0x35_7010] = 0x42;
    image[0x35_7011] = 0x00;
    tree.write("fixtures/large.dat", &image);
    let report = tree.scan(&["fixtures/large.dat"]);
    assert_eq!(rules(&report), [Rule::CardidWindow]);
    assert_eq!(report.hits[0].offset, Some(0x35_7010));
    assert!(
        report
            .render()
            .contains("large.dat:0x357010 (2 non-0xFF byte(s) in the cardid window)")
    );
}

/// 8 MB of 0xFF with `head` at offset 0 and non-0xFF bytes in the cardid window.
pub(crate) fn planted(head: &[u8]) -> Vec<u8> {
    let mut image = vec![0xFFu8; 0x80_0000];
    image[..head.len()].copy_from_slice(head);
    image[0x35_6000] = 0x5A;
    image[0x35_9FFF] = 0x00;
    image
}

/// An 8 MB wasm module: the version 1 header and one custom section whose five-byte LEB128 size
/// covers the rest of the file, so [`planted`]'s cardid-window bytes sit inside a valid module.
pub(crate) fn wasm_module() -> Vec<u8> {
    let payload = 0x80_0000 - 8 - 1 - 5;
    let mut head = b"\0asm\x01\0\0\0\0".to_vec();
    let mut size = payload as u32;
    for index in 0..5 {
        let low = (size & 0x7F) as u8;
        size >>= 7;
        head.push(if index < 4 { low | 0x80 } else { low });
    }
    // The custom section's name: empty.
    head.push(0);
    head
}

/// A 64-bit little-endian Mach-O header with two load commands of 16 and 8 bytes.
pub(crate) fn macho64_le() -> Vec<u8> {
    let mut head = vec![0u8; 32 + 24];
    head[..4].copy_from_slice(&[0xCF, 0xFA, 0xED, 0xFE]);
    head[16..20].copy_from_slice(&2u32.to_le_bytes());
    head[20..24].copy_from_slice(&24u32.to_le_bytes());
    head[32 + 4..32 + 8].copy_from_slice(&16u32.to_le_bytes());
    head[48 + 4..48 + 8].copy_from_slice(&8u32.to_le_bytes());
    head
}

#[test]
fn executable_structure_accepts_the_real_container_shapes() {
    assert!(patterns::executable_structure(&planted(&macho64_le())));
    // The same Mach-O big-endian, 32-bit: header 28 bytes.
    let mut be = vec![0u8; 28 + 8];
    be[..4].copy_from_slice(&[0xFE, 0xED, 0xFA, 0xCE]);
    be[16..20].copy_from_slice(&1u32.to_be_bytes());
    be[20..24].copy_from_slice(&8u32.to_be_bytes());
    be[28 + 4..28 + 8].copy_from_slice(&8u32.to_be_bytes());
    assert!(patterns::executable_structure(&be));
    let mut pe = vec![0u8; 0x100];
    pe[..2].copy_from_slice(b"MZ");
    pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    assert!(patterns::executable_structure(&pe));
    let mut elf = vec![0u8; 0x40];
    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4..7].copy_from_slice(&[2, 1, 1]);
    elf[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
    assert!(patterns::executable_structure(&elf));
    let mut fat = vec![0u8; 0x1000];
    fat[..4].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    fat[4..8].copy_from_slice(&1u32.to_be_bytes());
    fat[16..20].copy_from_slice(&0x100u32.to_be_bytes());
    fat[20..24].copy_from_slice(&0x200u32.to_be_bytes());
    assert!(patterns::executable_structure(&fat));
    let mut wasm = b"\0asm".to_vec();
    wasm.extend_from_slice(&1u32.to_le_bytes());
    assert!(patterns::executable_structure(&wasm));
    // A type section of one empty function type, then the 8 MB module with a custom section.
    let mut typed = wasm.clone();
    typed.extend_from_slice(&[1, 4, 1, 0x60, 0, 0]);
    assert!(patterns::executable_structure(&typed));
    assert!(patterns::executable_structure(&planted(&wasm_module())));
}

#[test]
fn a_forged_magic_is_not_an_executable() {
    // `MZ` with an e_lfanew that points at no `PE\0\0`, and one that points outside the file.
    let mut mz = b"MZ".to_vec();
    mz.resize(0x40, 0);
    mz[0x3C..0x40].copy_from_slice(&0x1000u32.to_le_bytes());
    assert!(!patterns::executable_structure(&planted(&mz)));
    mz[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    assert!(!patterns::executable_structure(&planted(&mz)));
    // `\x7fELF` followed by garbage (0xFF class, data and version).
    assert!(!patterns::executable_structure(&planted(b"\x7fELF")));
    // A Mach-O magic whose load commands do not add up.
    let mut macho = macho64_le();
    macho[20..24].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    assert!(!patterns::executable_structure(&planted(&macho)));
    // Fat with an absurd architecture count, and wasm of another version.
    assert!(!patterns::executable_structure(&planted(&[
        0xCA, 0xFE, 0xBA, 0xBE
    ])));
    assert!(!patterns::executable_structure(&planted(
        b"\0asm\x02\0\0\0"
    )));
    // A wasm header in front of a flash image: 0xFF is no section id, so the walk fails at once.
    assert!(!patterns::executable_structure(&planted(
        b"\0asm\x01\0\0\0"
    )));
    // Sections that do not end at the end of the file: one byte short, one byte over, and a size
    // that runs past five LEB128 bytes.
    let mut short = planted(&wasm_module());
    short.pop();
    assert!(!patterns::executable_structure(&short));
    let mut over = planted(&wasm_module());
    over.push(0);
    assert!(!patterns::executable_structure(&over));
    assert!(!patterns::executable_structure(
        b"\0asm\x01\0\0\0\x00\x80\x80\x80\x80\x80\x01"
    ));
    // A flash image.
    assert!(!patterns::executable_structure(&planted(&[
        0xE9, 0x03, 0x02, 0x20
    ])));
}

#[test]
fn the_tree_scan_still_refuses_a_cardid_window_behind_a_valid_executable() {
    // The exception lives in `xtask package` alone; commit hooks and the tree scan stay strict.
    let tree = TempTree::new();
    tree.write("target/passportsim", &planted(&macho64_le()));
    let report = tree.scan(&["target/passportsim"]);
    assert_eq!(rules(&report), [Rule::CardidWindow]);
}

#[test]
fn cardid_window_passes_erased_window_and_short_files() {
    let tree = TempTree::new();
    let mut erased = vec![0xFFu8; 0x80_0000];
    erased[0] = 0x00;
    erased[patterns::CARDID_WINDOW.end] = 0x00;
    let mut short = vec![0xFFu8; patterns::CARDID_WINDOW.end - 1];
    short[0x35_7000] = 0x00;
    tree.write("fixtures/erased.dat", &erased);
    tree.write("fixtures/short.dat", &short);
    let report = tree.scan(&["fixtures/erased.dat", "fixtures/short.dat"]);
    assert!(report.hits.is_empty(), "{:?}", report.hits);
}

// ---------------------------------------------------------------------------------------------
// nvs-credential
// ---------------------------------------------------------------------------------------------

const DATA: [u8; 8] = [0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28];
const TYPE_STR: u8 = 0x21;
const TYPE_U32: u8 = 0x04;

fn ns_entry(name: &'static str, index: u8) -> (u8, u8, &'static str, [u8; 8]) {
    (
        0,
        nvs::TYPE_U8,
        name,
        [index, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    )
}

/// An uninitialized first page followed by `pages`.
fn nvs_partition(pages: &[Vec<u8>]) -> Vec<u8> {
    let mut image = vec![0xFFu8; nvs::PAGE_SIZE];
    for page in pages {
        image.extend_from_slice(page);
    }
    image
}

#[test]
fn nvs_crc_matches_esp_idf_generator_header() {
    // Header bytes 4 to 27 of a page written by the ESP-IDF v5.5.3 nvs_partition_gen tool
    // (sequence 0, version 0xFE) and the CRC32 the tool stored for them.
    let mut header = vec![0u8; 4];
    header.push(0xFE);
    header.extend_from_slice(&[0xFF; 19]);
    assert_eq!(nvs::crc32_le(&[&header]), 0xB9BA_2D84);
    assert!(nvs::is_page(&nvs::build_page(&[])));
}

#[test]
fn nvs_credential_fires_on_planted_partition() {
    let tree = TempTree::new();
    let wifi = nvs::build_page(&[ns_entry("nvs.net80211", 1), (1, TYPE_STR, "sta.pswd", DATA)]);
    let app = nvs::build_page(&[
        ns_entry("app", 2),
        (2, TYPE_U32, "volume", DATA),
        (2, TYPE_STR, "cloud_token", DATA),
    ]);
    tree.write("fixtures/part.dat", &nvs_partition(&[wifi, app]));
    let report = tree.scan(&["fixtures/part.dat"]);
    assert_eq!(rules(&report), [Rule::NvsCredential; 2]);
    assert_eq!(report.hits[0].offset, Some(nvs::PAGE_SIZE + 64 + 32));
    assert_eq!(
        report.hits[1].offset,
        Some(2 * nvs::PAGE_SIZE + 64 + 2 * 32)
    );
}

#[test]
fn nvs_credential_passes_clean_partitions() {
    let tree = TempTree::new();
    let credential = [ns_entry("nvs.net80211", 1), (1, TYPE_STR, "sta.pswd", DATA)];
    let clean = nvs::build_page(&[
        ns_entry("app", 1),
        (1, TYPE_U32, "volume", DATA),
        (1, TYPE_U32, "boot_count", DATA),
    ]);
    let mut bad_header = nvs::build_page(&credential);
    bad_header[28] ^= 0x01;
    let mut erased = nvs::build_page(&credential);
    erased[32] &= !(0b11 << 2);
    let mut bad_entry = nvs::build_page(&credential);
    bad_entry[64 + 32 + 4] ^= 0x01;
    tree.write("fixtures/clean.dat", &nvs_partition(&[clean]));
    tree.write("fixtures/bad-header.dat", &nvs_partition(&[bad_header]));
    tree.write("fixtures/erased.dat", &nvs_partition(&[erased]));
    tree.write("fixtures/bad-entry.dat", &nvs_partition(&[bad_entry]));
    let report = tree.scan(&[
        "fixtures/clean.dat",
        "fixtures/bad-header.dat",
        "fixtures/erased.dat",
        "fixtures/bad-entry.dat",
    ]);
    assert!(report.hits.is_empty(), "{:?}", report.hits);
    assert_eq!(report.files_scanned, 4);
}

#[test]
fn nvs_credential_heuristic_cases() {
    assert!(nvs::is_credential(Some("nvs.net80211"), "sta.ssid"));
    assert!(nvs::is_credential(Some("nvs.net80211"), "ap.passwd"));
    assert!(nvs::is_credential(Some("nimble_bond"), "our_sec_1"));
    assert!(nvs::is_credential(Some("bt_config.conf"), "bt_cfg_key0"));
    assert!(nvs::is_credential(Some("phy"), "cal_mac"));
    assert!(nvs::is_credential(None, "MQTT_PASSWORD"));
    assert!(!nvs::is_credential(Some("phy"), "cal_data"));
    assert!(!nvs::is_credential(Some("nvs.net80211"), "opmode"));
    assert!(!nvs::is_credential(Some("app"), "volume"));
}

// ---------------------------------------------------------------------------------------------
// backup-name
// ---------------------------------------------------------------------------------------------

#[test]
fn backup_name_fires_on_device_names() {
    let bare = mac_text(PLANTED, "");
    let names = [
        "efuse_blk1.bin".to_string(),
        "data/EFUSE_BLK10.bin".to_string(),
        "dumps/flash_full_8mb.bin".to_string(),
        "notes/passport-backups/readme.txt".to_string(),
        "cardid.hex".to_string(),
        "logs/boot_log_3.txt".to_string(),
        "archive/nvs_dump.bin.gz".to_string(),
        format!("captures/dev_{bare}.txt"),
        format!("captures/{}.log", mac_text(PLANTED, "-")),
        format!("{bare}/notes.txt"),
    ];
    for name in &names {
        assert!(
            patterns::backup_name(name),
            "expected a hit for sample {}",
            names.iter().position(|n| n == name).unwrap_or(0)
        );
    }
}

#[test]
fn backup_name_passes_ordinary_names() {
    let names = [
        "src/main.rs",
        "docs/backup-policy.md",
        "cache/0123456789abcdef0123.json",
        "assets/rom/esp32c3_rev3_rom.elf",
        "tests/fw/riscv-tests/rv32ui-p-add.elf",
        "crates/pemu-soc-c3/src/flash.rs",
        "specs/efuse.toml",
        "firmware.bin",
        "notes/02000012abcd.txt",
        "logs/20260911120000.log",
        "web/src/cardinal.ts",
    ];
    for name in names {
        assert!(!patterns::backup_name(name), "{name}");
    }
}

#[test]
fn backup_name_withholds_the_file_name() {
    let tree = TempTree::new();
    tree.write("backups/flash_full.bin", b"x");
    let report = tree.scan(&["backups/flash_full.bin", "gone/efuse_blk2.bin"]);
    assert_eq!(rules(&report), [Rule::BackupName, Rule::BackupName]);
    assert_eq!(report.files_skipped, 1);
    let rendered = report.render();
    assert!(rendered.contains("backup-name backups/<name withheld>"));
    assert!(!rendered.contains("flash_full") && !rendered.contains("efuse_blk2"));
    let bare = mac_text(PLANTED, "");
    assert_eq!(
        patterns::withheld_path(&format!("{bare}/a.txt")),
        "<path withheld>"
    );
    assert_eq!(patterns::withheld_path("efuse_blk0.bin"), "<name withheld>");
}

// ---------------------------------------------------------------------------------------------
// rom-pin
// ---------------------------------------------------------------------------------------------

fn fake_elf(tag: u8) -> Vec<u8> {
    let mut elf = b"\x7fELF\x01\x01\x01\x00".to_vec();
    elf.extend_from_slice(&[tag; 56]);
    elf
}

/// A tree with a licensed `assets/rom/` whose `pins.toml` pins `pinned` as `a_rom.elf`.
fn rom_tree(pinned: &[u8]) -> TempTree {
    let tree = TempTree::new();
    tree.write(
        "assets/rom/LICENSE",
        b"Apache License 2.0 (test stand-in)\n",
    );
    tree.write("assets/rom/NOTICE", b"Test notice\n");
    let sha = rom::sha256_hex(pinned);
    let pins = format!("release = \"test\"\n\n[[rom]]\nfile = \"a_rom.elf\"\nsha256 = \"{sha}\"\n");
    tree.write("assets/rom/pins.toml", pins.as_bytes());
    tree.write("assets/rom/a_rom.elf", pinned);
    tree
}

#[test]
fn sha256_hex_known_answer() {
    assert_eq!(
        rom::sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn rom_pin_passes_pinned_licensed_elf() {
    let tree = rom_tree(&fake_elf(1));
    let report = tree.scan(&[
        "assets/rom/a_rom.elf",
        "assets/rom/LICENSE",
        "assets/rom/NOTICE",
        "assets/rom/pins.toml",
    ]);
    assert!(report.hits.is_empty(), "{:?}", report.hits);
    assert_eq!(report.roms_pinned, 1);
}

#[test]
fn rom_pin_fails_unpinned_or_unlicensed_binaries() {
    let tree = rom_tree(&fake_elf(1));
    tree.write("assets/rom/b_rom.elf", &fake_elf(2));
    tree.write("assets/rom/blob.dat", &[0, 1, 2, 3]);
    let report = tree.scan(&[
        "assets/rom/a_rom.elf",
        "assets/rom/b_rom.elf",
        "assets/rom/blob.dat",
    ]);
    assert_eq!(rules(&report), [Rule::RomPin, Rule::RomPin]);
    assert_eq!(report.hits[0].path, "assets/rom/b_rom.elf");
    assert_eq!(
        report.hits[0].note.as_deref(),
        Some(rom::RomFailure::Unpinned.note())
    );
    assert_eq!(
        report.hits[1].note.as_deref(),
        Some(rom::RomFailure::NotElf.note())
    );
    assert_eq!(report.roms_pinned, 1);

    tree.remove("assets/rom/NOTICE");
    let report = tree.scan(&["assets/rom/a_rom.elf"]);
    assert_eq!(
        report.hits[0].note.as_deref(),
        Some(rom::RomFailure::MissingLicense.note())
    );

    tree.write("assets/rom/NOTICE", b"Test notice\n");
    tree.write(
        "assets/rom/pins.toml",
        b"[[rom]]\nsha256 = \"not-a-hash\"\n",
    );
    let report = tree.scan(&["assets/rom/a_rom.elf"]);
    assert_eq!(
        report.hits[0].note.as_deref(),
        Some(rom::RomFailure::BadPins.note())
    );
}

// ---------------------------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------------------------

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parse_args_accepts_root_and_paths() {
    let opts =
        super::parse_args(&args(&["--root", "/r", "--paths", "a.txt", "b/c.md"])).expect("parse");
    assert_eq!(opts.root, Some(PathBuf::from("/r")));
    assert_eq!(opts.mode, super::Mode::Paths(args(&["a.txt", "b/c.md"])));
    assert!(super::parse_args(&args(&["--bogus"])).is_err());
    assert!(super::parse_args(&args(&["--root"])).is_err());
}

#[test]
fn run_paths_mode_fails_only_on_hits() {
    let tree = TempTree::new();
    let root = tree.root_arg();
    tree.write("clean.txt", b"placeholder 02:00:00:00:00:01\n");
    tree.write(
        "planted.txt",
        format!("{}\n", mac_text(PLANTED, ":")).as_bytes(),
    );
    assert_eq!(
        super::run(&args(&[
            "--root",
            &root,
            "--paths",
            &format!("{root}/clean.txt")
        ])),
        Ok(())
    );
    let err = super::run(&args(&[
        "--root",
        &root,
        "--paths",
        &format!("{root}/planted.txt"),
    ]))
    .expect_err("hit");
    assert!(err.contains("1 pattern rule hit"), "{err}");
}

#[test]
fn run_paths_mode_resolves_relative_paths_against_root() {
    let tree = TempTree::new();
    let root = tree.root_arg();
    tree.write("docs/clean.txt", b"placeholder 02:00:00:00:00:01\n");
    tree.write(
        "docs/planted.txt",
        format!("{}\n", mac_text(PLANTED, ":")).as_bytes(),
    );
    // The test process runs in the xtask crate directory, which has no `docs/` files, so
    // these relative paths reach the samples only through `--root`.
    assert_eq!(
        super::run(&args(&["--root", &root, "--paths", "docs/clean.txt"])),
        Ok(())
    );
    let err = super::run(&args(&["--root", &root, "--paths", "docs/planted.txt"]))
        .expect_err("hit through the root");
    assert!(err.contains("1 pattern rule hit"), "{err}");
}

#[test]
fn paths_resolve_against_root_or_current_directory() {
    let tree = TempTree::new();
    tree.write("sub/a.txt", b"nothing to see\n");
    let cwd = std::env::current_dir().expect("current directory");
    assert_eq!(super::paths_base(None), Ok(cwd.clone()));
    assert_eq!(super::paths_base(Some(&tree.root)), Ok(tree.root.clone()));

    // With `--root`, the base is the root.
    assert_eq!(
        super::relativize(&tree.root, &tree.root, "sub/a.txt"),
        "sub/a.txt"
    );
    // Without it, the base is the current directory: from `<root>/sub`, `a.txt` is `sub/a.txt`.
    assert_eq!(
        super::relativize(&tree.root, &tree.root.join("sub"), "a.txt"),
        "sub/a.txt"
    );
    // A relative path outside the root stays absolute against the current directory.
    assert_eq!(
        super::relativize(&tree.root, &cwd, "a.txt"),
        super::to_slash(&cwd.join("a.txt"))
    );
    // An absolute path ignores the base.
    let absolute = format!("{}/sub/a.txt", tree.root_arg());
    assert_eq!(super::relativize(&tree.root, &cwd, &absolute), "sub/a.txt");
}

#[test]
fn run_tree_mode_uses_git_file_list() {
    let tree = TempTree::new();
    let root = tree.root_arg();
    let status = std::process::Command::new("git")
        .args(["-C", &root, "init", "-q"])
        .status();
    assert!(
        status.is_ok_and(|s| s.success()),
        "git init in a temporary directory"
    );
    let planted = format!("{}\n", mac_text(PLANTED, ":"));
    tree.write(".gitignore", b"ignored/\n");
    tree.write("ignored/planted.md", planted.as_bytes());
    tree.write("docs/ok.md", b"nothing to see\n");
    assert_eq!(super::run(&args(&["--root", &root])), Ok(()));
    tree.write("docs/planted.md", planted.as_bytes());
    assert!(super::run(&args(&["--root", &root])).is_err());
}
