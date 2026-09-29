//! Shared builders of the planner tests: synthetic images with the `official.pt` layout and the
//! committed device facts. No corpus bytes, no device values.

#![allow(dead_code)]

use pemu_loader::partitions::{Partition, ptype, subtype};
use pemu_planner::plan::encode_partition_table;
use pemu_planner::rules::DeviceFacts;

pub const FACTS_TOML: &str = include_str!("../../../../tests/fixtures/device-facts.toml");

pub fn facts() -> DeviceFacts {
    DeviceFacts::from_toml(FACTS_TOML).expect("the committed device facts parse")
}

pub fn part(name: &str, ptype: u8, subtype: u8, offset: u32, size: u32) -> Partition {
    Partition {
        name: name.to_owned(),
        ptype,
        subtype,
        offset,
        size,
        flags: 0,
    }
}

pub fn official_layout() -> Vec<Partition> {
    vec![
        part("nvs", ptype::DATA, subtype::NVS, 0x9000, 0x6000),
        part("phy_init", ptype::DATA, subtype::PHY, 0xF000, 0x1000),
        part("factory", ptype::APP, subtype::FACTORY, 0x1_0000, 0x30_0000),
        part("cardid", ptype::DATA, subtype::NVS, 0x35_6000, 0x4000),
    ]
}

/// A synthetic placeholder ELF digest, unrelated to any build.
pub const ELF_SHA: [u8; 32] = [
    0x0a, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01,
    0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80,
];

/// A minimal app image: one segment holding an `esp_app_desc_t` whose ELF SHA-256 is `elf_sha`,
/// followed by `extra` bytes of 0x5A so the trimmed length can be set.
pub fn app_image(elf_sha: [u8; 32], extra: usize) -> Vec<u8> {
    let mut img = vec![0u8; 24];
    img[0] = 0xE9;
    img[1] = 1;
    img[2] = 2;
    img[12..14].copy_from_slice(&5u16.to_le_bytes());
    img.extend_from_slice(&0x3C00_0020u32.to_le_bytes());
    img.extend_from_slice(&256u32.to_le_bytes());
    let mut desc = vec![0u8; 256];
    desc[..4].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
    desc[144..176].copy_from_slice(&elf_sha);
    img.extend_from_slice(&desc);
    img.resize(304, 0);
    img.extend(std::iter::repeat_n(0x5A, extra));
    img
}

pub fn bootloader() -> Vec<u8> {
    let mut b = vec![0x11u8; 0x5220];
    b[0] = 0xE9;
    b
}

/// A merged image with `layout`, `app` at 0x10000, 0xFF elsewhere, `len` bytes long.
pub fn merged(layout: &[Partition], app: &[u8], len: usize) -> Vec<u8> {
    let mut img = vec![0xFFu8; len];
    let boot = bootloader();
    img[..boot.len()].copy_from_slice(&boot);
    let table = encode_partition_table(layout);
    img[0x8000..0x8000 + table.len()].copy_from_slice(&table);
    img[0x1_0000..0x1_0000 + app.len()].copy_from_slice(app);
    img
}

/// The unpadded merged image of the official layout: it ends right after the app.
pub fn official_unpadded() -> Vec<u8> {
    let app = app_image(ELF_SHA, 0x1000);
    merged(&official_layout(), &app, 0x1_0000 + app.len())
}

/// The padded merged image of the official layout: 8,388,608 B, cardid window 0xFF.
pub fn official_padded() -> Vec<u8> {
    let app = app_image(ELF_SHA, 0x1000);
    merged(&official_layout(), &app, 0x80_0000)
}
