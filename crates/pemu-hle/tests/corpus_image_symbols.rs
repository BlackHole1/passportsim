//! The image-only recovery of `pemu_hle::image_symbols` against every corpus build that has an ELF
//! and links a radio: each recovered name sits where the ELF puts it, with the ELF's size, and
//! nothing the ELF does not link is recovered. `print_image_symbol_rules` (ignored) generates
//! `specs/hle/idf-5.5.3/image-symbols.toml` with the same functions the machine runs.
//!
//! Missing corpus files skip with a printed reason; a file whose SHA-256 is not the pinned one
//! fails. No path or hash is printed.

// Test code is exempt from the core-crate API bans; `clippy.toml` bans these for the library.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pemu_hle::binding::{CODE_HASH_BYTES, LoadedSegment, skeleton_hash};
use pemu_hle::image_symbols::{
    GLOBAL_POINTER, ImageRules, Resolution, body_hash, code_section, lead, recover,
};
use pemu_loader::elf::ElfInfo;
use pemu_loader::esp_image::MergedImage;
use pemu_loader::symbols::SymbolTable;
use pemu_loader::{hex, sha256};
use pemu_rv32::decode::decode_at;
use pemu_rv32::op::{
    K_ADDI, K_AUIPC, K_JAL, K_JALR, K_LB, K_LBU, K_LH, K_LHU, K_LUI, K_LW, K_SB, K_SH, K_SW,
};

/// Corpus builds with an app ELF: id, flash image file, app ELF file (`corpus/MANIFEST.json`).
const CORPUS: [(&str, &str, &str); 6] = [
    (
        "pk",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
    ("pkgatt", "merged-binary.bin", "radio_pkgatt.elf"),
    ("scan3", "merged-binary.bin", "radio_scan3probe.elf"),
    ("probe2", "probe2-merged.bin", "radio_heapprobe.elf"),
    (
        "official",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
    ("demo", "demo-merged.bin", "FoloToy-AI-Passport.elf"),
];

/// Device probes that link a radio, with their unstripped ELF (`tests/fw/manifest.toml`).
const PROBES: [&str; 5] = [
    "probe_wifi_http",
    "probe_wifi_conn",
    "probe_wifi_assoc",
    "probe_campaign_radio",
    "hle_probe",
];

/// Every function the rules recover, in file order: the hooks, the nested-call targets, the core
/// observe hooks, the NVS calibration marker, and the functions that host a witness only.
const FUNCTIONS: [&str; 44] = [
    "esp_bt_controller_init",
    "esp_bt_controller_deinit",
    "esp_bt_controller_enable",
    "esp_bt_controller_disable",
    "esp_vhci_host_check_send_available",
    "esp_vhci_host_send_packet",
    "esp_vhci_host_register_callback",
    "esp_wifi_init",
    "esp_wifi_deinit",
    "esp_wifi_set_mode",
    "esp_wifi_get_mode",
    "esp_wifi_set_storage",
    "esp_wifi_set_config",
    "esp_wifi_get_config",
    "esp_wifi_get_mac",
    "esp_wifi_start",
    "esp_wifi_stop",
    "esp_wifi_scan_start",
    "esp_wifi_scan_stop",
    "esp_wifi_scan_get_ap_num",
    "esp_wifi_scan_get_ap_records",
    "esp_wifi_internal_reg_rxcb",
    "esp_wifi_internal_reg_netstack_buf_cb",
    "esp_wifi_internal_free_rx_buffer",
    "esp_wifi_connect_internal",
    "esp_wifi_disconnect_internal",
    "esp_wifi_internal_tx",
    "esp_wifi_internal_set_sta_ip",
    "xTaskCreatePinnedToCore",
    "xQueueGenericCreate",
    "xQueueSemaphoreTake",
    "xQueueGenericSend",
    "xQueueGiveFromISR",
    "vPortYieldFromISR",
    "vTaskDelete",
    "vQueueDelete",
    "esp_event_post",
    "esp_read_mac",
    "esp_intr_alloc",
    "esp_intr_free",
    "esp_log",
    "esp_log_timestamp",
    "heap_caps_malloc",
    "heap_caps_free",
];

/// Functions beyond [`FUNCTIONS`]: observe hooks, a presence marker, witness hosts, and the two
/// guards of `wifi.toml` (`wifi_init_completed`, `esp_wifi_get_user_init_flag_internal`) with
/// `wifi_deinit_internal`, whose first call is the second one and so pins which 18-byte function
/// it is, and `esp_wifi_deinit_internal`, the next blob function on that path.
const MORE_FUNCTIONS: [&str; 12] = [
    "esp_panic_handler",
    "abort",
    "__assert_func",
    "esp_phy_load_cal_data_from_nvs",
    "xPortStartScheduler",
    "call_start_cpu0",
    "wifi_event_post",
    "esp_event_post_wrapper",
    "wifi_init_completed",
    "esp_wifi_deinit_internal",
    "wifi_deinit_internal",
    "esp_wifi_get_user_init_flag_internal",
];

/// Data symbols and the size the ELF path pins for each (the module profiles' `[[data]]` rows and
/// the 4-byte kernel variables).
const DATA: [(&str, u32); 9] = [
    ("btdm_controller_status", 4),
    ("s_wifi_inited", 1),
    ("WIFI_EVENT", 4),
    ("pxCurrentTCBs", 4),
    ("port_uxInterruptNesting", 4),
    ("xSchedulerRunning", 4),
    ("xIsrStackBottom", 4),
    ("xIsrStackTop", 4),
    (GLOBAL_POINTER, 0),
];

/// Functions whose body is not unique in an image without a call to tell it apart: the calls the
/// generator writes into their shapes even when the callee is a ROM symbol.
const TOLD_BY_ROM_CALL: [&str; 1] = ["esp_wifi_internal_free_rx_buffer"];

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

fn field(record: &str, key: &str) -> Option<String> {
    let at = record.find(&format!("\"{key}\""))?;
    let rest = &record[at + key.len() + 2..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].replace("\\\\", "\\"))
}

/// A file read and checked against its pinned SHA-256, or `None` after printing why.
fn pinned(path: &Path, want: &str, what: &str) -> Option<Vec<u8>> {
    let Ok(bytes) = std::fs::read(path) else {
        eprintln!("skip: {what} is absent");
        return None;
    };
    assert_eq!(hex(&sha256(&bytes)), want, "{what} is not the pinned file");
    Some(bytes)
}

/// The flash image and app ELF of a build id, checked, or `None` after printing why.
fn build(root: &Path, id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    if let Some((_, bin, elf)) = CORPUS.iter().find(|c| c.0 == id) {
        let manifest = std::fs::read_to_string(root.join("corpus/MANIFEST.json")).ok()?;
        let record = |file: &str| {
            manifest
                .split('{')
                .find(|r| {
                    field(r, "id").as_deref() == Some(id)
                        && field(r, "file").as_deref() == Some(file)
                })
                .and_then(|r| Some((field(r, "path")?, field(r, "sha256")?)))
        };
        let (bin_path, bin_sha) = record(bin)?;
        let (elf_path, elf_sha) = record(elf)?;
        return Some((
            pinned(
                Path::new(&bin_path),
                &bin_sha,
                &format!("corpus {id} image"),
            )?,
            pinned(Path::new(&elf_path), &elf_sha, &format!("corpus {id} ELF"))?,
        ));
    }
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fw/manifest.toml"),
    )
    .expect("the probe manifest is part of the tree");
    let block = manifest
        .split("[[probe]]")
        .find(|b| b.contains(&format!("name = \"{id}\"")))?;
    let key = |k: &str| {
        block.lines().find_map(|l| {
            l.trim()
                .strip_prefix(&format!("{k} = \""))
                .map(|v| v.trim_end_matches('"').to_string())
        })
    };
    let probes = root.join("corpus/probes");
    Some((
        pinned(
            &probes.join(format!("{id}-8MB.bin")),
            &key("merged_sha256")?,
            &format!("probe {id} image"),
        )?,
        pinned(
            &probes.join(format!("build/{id}/{id}.elf")),
            &key("elf_sha256")?,
            &format!("probe {id} ELF"),
        )?,
    ))
}

fn rom() -> SymbolTable {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/rom/esp32c3_rev101_rom.elf");
    let bytes = std::fs::read(&path).expect("the bundled ROM ELF is part of the tree");
    ElfInfo::parse(&bytes).expect("the ROM ELF parses").symbols
}

/// The boot app of a merged image: its segments, app descriptor and entry.
struct App<'a> {
    segments: Vec<LoadedSegment<'a>>,
    merged: MergedImage,
}

fn app(flash: &[u8]) -> App<'_> {
    let merged = MergedImage::parse(flash).expect("a corpus image parses");
    let (_, image) = merged.app.clone().expect("a boot app");
    let segments = (0..image.segments.len())
        .filter_map(|i| {
            Some(LoadedSegment {
                addr: image.segments[i].load_addr,
                data: image.segment_data(flash, i)?,
            })
        })
        .collect();
    App { segments, merged }
}

impl App<'_> {
    fn bytes(&self, addr: u32, len: u32) -> Option<&[u8]> {
        self.segments.iter().find_map(|seg| {
            let start = addr.checked_sub(seg.addr)? as usize;
            seg.data.get(start..start.checked_add(len as usize)?)
        })
    }

    fn recover(&self, flash: &[u8]) -> pemu_hle::image_symbols::Recovered {
        let (_, image) = self.merged.app.as_ref().expect("a boot app");
        let desc = image.app_desc(flash).expect("the descriptor parses");
        recover(
            ImageRules::load(),
            &self.segments,
            desc,
            image.header.entry_addr,
            Some(&rom()),
        )
    }
}

/// Every build the data root holds, loaded, or an empty list after printing why.
fn builds() -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
    let Some(root) = data_root() else {
        return Vec::new();
    };
    CORPUS
        .iter()
        .map(|c| c.0)
        .chain(PROBES)
        .filter_map(|id| build(&root, id).map(|(flash, elf)| (id, flash, elf)))
        .collect()
}

#[test]
fn every_corpus_build_recovers_what_its_elf_names_and_nothing_it_does_not() {
    let builds = builds();
    for (id, flash, elf_bytes) in &builds {
        let elf = ElfInfo::parse(elf_bytes).expect("the ELF parses");
        let app = app(flash);
        let got = app.recover(flash);
        let rules = ImageRules::load();
        for function in &rules.functions {
            let want = elf.symbols.lookup(&function.name);
            match (&got.resolved[&function.name], want) {
                (Resolution::At { addr, size }, Some(sym)) => {
                    assert_eq!(
                        (*addr, *size),
                        (sym.addr, sym.size),
                        "{id}: {}",
                        function.name
                    );
                }
                (Resolution::Missing(_), None) => {}
                (other, want) => panic!(
                    "{id}: {} recovered as {other:?}, the ELF has {:?}",
                    function.name,
                    want.map(|s| s.addr)
                ),
            }
        }
        for d in &rules.data {
            if let Resolution::At { addr, .. } = got.resolved[&d.name] {
                assert_eq!(
                    Some(addr),
                    elf.symbols.addr_of(&d.name),
                    "{id}: data {}",
                    d.name
                );
            } else {
                // A data symbol is missed only when no function that materializes it is linked.
                let hosts: Vec<&str> = rules
                    .functions
                    .iter()
                    .filter(|f| {
                        f.shapes
                            .iter()
                            .any(|s| s.refs.iter().any(|r| r.symbol == d.name))
                    })
                    .map(|f| f.name.as_str())
                    .collect();
                assert!(
                    hosts.iter().all(|h| elf.symbols.lookup(h).is_none()),
                    "{id}: data {} missed: {:?}",
                    d.name,
                    got.resolved[&d.name]
                );
            }
        }
        // The image's descriptor, not the ELF's: the build writes the ELF's SHA-256 into the
        // image only, so compare the field binding reads.
        assert_eq!(
            got.elf.app_desc.as_ref().map(|d| d.idf_ver.as_str()),
            elf.app_desc.as_ref().map(|d| d.idf_ver.as_str()),
            "{id}: idf_ver"
        );
        println!("RAN {id}: recovery matches the ELF");
    }
}

/// The boot-time cost of the recovery: the median of 5 runs per corpus build, and of any merged
/// image named by `PEMU_RECOVERY_IMAGE`. A measurement, not a check; run it by name with
/// `--ignored --nocapture` in a release build.
#[test]
#[ignore = "a measurement: prints the recovery time per image"]
fn measure_the_recovery_cost() {
    let mut images: Vec<(String, Vec<u8>)> = builds()
        .into_iter()
        .map(|(id, flash, _)| (id.to_string(), flash))
        .collect();
    if let Some(path) = std::env::var_os("PEMU_RECOVERY_IMAGE") {
        let bytes = std::fs::read(&path).expect("PEMU_RECOVERY_IMAGE is readable");
        images.push(("PEMU_RECOVERY_IMAGE".to_string(), bytes));
    }
    for (id, flash) in &images {
        let app = app(flash);
        let executable: usize = app
            .segments
            .iter()
            .filter(|s| code_section(s.addr).is_some())
            .map(|s| s.data.len())
            .sum();
        let mut times: Vec<std::time::Duration> = (0..5)
            .map(|_| {
                let start = std::time::Instant::now();
                std::hint::black_box(app.recover(flash));
                start.elapsed()
            })
            .collect();
        times.sort_unstable();
        let found = app
            .recover(flash)
            .resolved
            .values()
            .filter(|r| matches!(r, Resolution::At { .. }))
            .count();
        println!(
            "{id}: {:.1} ms median over {} KiB of code, {found} names recovered",
            times[2].as_secs_f64() * 1e3,
            executable / 1024
        );
    }
}

// The generator of image-symbols.toml.

/// Offsets inside `code` (starting at `base`) where an address is materialized: (hi, lo, address,
/// width), `lo` `None` for a `gp`-relative access. The same pairing `materialize` checks.
fn materializations(code: &[u8], base: u32, gp: u32) -> Vec<(u32, Option<u32>, u32, Option<u32>)> {
    let mut out = Vec::new();
    let mut producers: BTreeMap<u8, (u32, u32)> = BTreeMap::new();
    let mut at = 0usize;
    while at < code.len() {
        let Some(op) = decode_at(&code[at..], base + at as u32) else {
            break;
        };
        let width = match op.kind {
            K_ADDI => Some(None),
            K_LB | K_LBU | K_SB => Some(Some(1)),
            K_LH | K_LHU | K_SH => Some(Some(2)),
            K_LW | K_SW => Some(Some(4)),
            _ => None,
        };
        if let Some(width) = width {
            // `auipc gp` then `addi gp, gp` sets gp itself: a pair, not a gp-relative access.
            if let Some(&(hi, value)) = producers.get(&op.rs1) {
                out.push((
                    hi,
                    Some(at as u32),
                    value.wrapping_add(op.imm as u32),
                    width,
                ));
            } else if op.rs1 == 3 {
                out.push((at as u32, None, gp.wrapping_add(op.imm as u32), width));
            }
        }
        if op.rd != 0 {
            if op.kind == K_LUI || op.kind == K_AUIPC {
                producers.insert(op.rd, (at as u32, op.imm as u32));
            } else {
                producers.remove(&op.rd);
            }
        }
        at += usize::from(op.len.max(2));
    }
    out
}

/// Offsets inside `code` of every call and its target.
fn calls(code: &[u8], base: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut auipc: Option<(u32, u8, u32)> = None;
    let mut at = 0usize;
    while at < code.len() {
        let Some(op) = decode_at(&code[at..], base + at as u32) else {
            break;
        };
        match op.kind {
            K_JAL => out.push((at as u32, op.imm as u32)),
            K_JALR => {
                if let Some((from, _, value)) = auipc.filter(|a| a.1 == op.rs1) {
                    out.push((from, value.wrapping_add(op.imm as u32) & !1));
                }
            }
            _ => {}
        }
        auipc = (op.kind == K_AUIPC && op.rd != 0).then_some((at as u32, op.rd, op.imm as u32));
        at += usize::from(op.len.max(2));
    }
    out
}

/// Writes the rules measured on every build of the data root to standard output, in the file's
/// format. Run it by name with `--ignored --nocapture` after a corpus change.
#[test]
#[ignore = "a generator: prints image-symbols.toml rows from the corpus"]
fn print_image_symbol_rules() {
    let builds = builds();
    assert!(!builds.is_empty(), "the generator needs the corpus");
    let names: Vec<&str> = FUNCTIONS
        .iter()
        .chain(MORE_FUNCTIONS.iter())
        .copied()
        .collect();
    let data: BTreeMap<&str, u32> = DATA.iter().copied().collect();
    let rom_table = rom();
    // (function, size, lead, body) -> (builds, refs per build, calls per build)
    type Key = (usize, u32, [u8; 8], [u8; 32]);
    type PerBuild = Vec<BTreeSet<String>>;
    let mut shapes: BTreeMap<Key, (Vec<&str>, PerBuild, PerBuild)> = BTreeMap::new();
    for (id, flash, elf_bytes) in &builds {
        let elf = ElfInfo::parse(elf_bytes).expect("the ELF parses");
        let app = app(flash);
        let gp = elf.symbols.addr_of(GLOBAL_POINTER).expect("gp");
        let data_at: BTreeMap<u32, &str> = data
            .keys()
            .filter_map(|n| Some((elf.symbols.addr_of(n)?, *n)))
            .collect();
        for (f, name) in names.iter().enumerate() {
            let Some(sym) = elf.symbols.lookup(name) else {
                continue;
            };
            assert!(code_section(sym.addr).is_some(), "{id}: {name} is not code");
            let code = app
                .bytes(sym.addr, sym.size)
                .expect("the body is in a segment");
            let mut refs = BTreeSet::new();
            let mut named = BTreeSet::new();
            for (hi, lo, addr, width) in materializations(code, sym.addr, gp) {
                let Some(dname) = data_at.get(&addr) else {
                    continue;
                };
                if width.is_some_and(|w| w != data[dname]) || !named.insert(*dname) {
                    continue;
                }
                refs.insert(match lo {
                    Some(lo) => format!("{dname}@{hi:#x}+{lo:#x}"),
                    None => format!("{dname}@{hi:#x}"),
                });
            }
            let mut called = BTreeSet::new();
            let mut callees = BTreeSet::new();
            for (at, target) in calls(code, sym.addr) {
                // Mapping symbols (`$x`) share the address; the rules name the function.
                let at_target: Vec<&str> = elf
                    .symbols
                    .at(target)
                    .map(|s| s.name.as_str())
                    .filter(|n| !n.is_empty() && !n.starts_with('$'))
                    .collect();
                // A ROM callee is named as the ROM ELF names it, which is what the machine
                // resolves it with (the app ELF names a jump-table entry after its target).
                let rom = TOLD_BY_ROM_CALL.contains(name) && target < 0x4200_0000;
                let rom_name = rom_table
                    .at(target)
                    .map(|s| s.name.as_str())
                    .find(|n| !n.is_empty() && !n.starts_with('$'));
                let Some(callee) = at_target
                    .iter()
                    .copied()
                    .find(|n| names.contains(n))
                    .or(rom_name.filter(|_| rom))
                else {
                    continue;
                };
                let in_rules = names.contains(&callee) && callee != *name;
                if (in_rules || rom) && callees.insert(callee.to_string()) {
                    called.insert(format!("{callee}@{at:#x}"));
                }
            }
            let key = (
                f,
                sym.size,
                lead(code).expect("longer than the key"),
                body_hash(code),
            );
            let entry = shapes.entry(key).or_default();
            entry.0.push(id);
            entry.1.push(refs);
            entry.2.push(called);
        }
    }
    let common = |sets: &[BTreeSet<String>]| -> String {
        let first = sets[0].clone();
        sets.iter()
            .fold(first, |acc, s| acc.intersection(s).cloned().collect())
            .into_iter()
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!("[profile]\nidf = \"5.5.3\"\n");
    for (f, name) in names.iter().enumerate() {
        let rows: Vec<_> = shapes.iter().filter(|(k, _)| k.0 == f).collect();
        if rows.is_empty() {
            continue;
        }
        println!("[[function]]\nname = \"{name}\"\n");
        for ((_, size, lead, body), (ids, refs, called)) in rows {
            println!("[[shape]]\nbuilds = \"{}\"\nsize = {size}", ids.join(","));
            println!("lead = \"{}\"\nbody_sha256 = \"{}\"", hex(lead), hex(body));
            let refs = common(refs);
            if !refs.is_empty() {
                println!("refs = \"{refs}\"");
            }
            let called = common(called);
            if !called.is_empty() {
                println!("calls = \"{called}\"");
            }
            println!();
        }
    }
    for (name, size) in DATA {
        println!("[[data]]\nname = \"{name}\"\nsize = {size}\n");
    }
}

/// The hook names of a module profile (`ble.toml`, `wifi.toml`), in file order.
fn profile_hooks(file: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/hle/idf-5.5.3")
        .join(file);
    let text = std::fs::read_to_string(path).expect("the profile is part of the tree");
    let mut names = Vec::new();
    let mut in_hook = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_hook = line == "[[hook]]";
        } else if in_hook && let Some(name) = line.strip_prefix("name = \"") {
            names.push(name.trim_end_matches('"').to_string());
        }
    }
    names
}

/// Writes the (size, head hash) pair of every profile hook on every build of the data root to
/// standard output, one `file hook build size code_sha256` line each: what a `[[variant]]` row of
/// `ble.toml` and `wifi.toml` pins. Run it by name with `--ignored --nocapture` after a corpus
/// change or a change to the skeleton.
#[test]
#[ignore = "a generator: prints the profile variant of every hook on every corpus build"]
fn print_profile_variants() {
    let builds = builds();
    assert!(!builds.is_empty(), "the generator needs the corpus");
    for file in ["ble.toml", "wifi.toml"] {
        let hooks = profile_hooks(file);
        for (id, flash, elf_bytes) in &builds {
            let elf = ElfInfo::parse(elf_bytes).expect("the ELF parses");
            let app = app(flash);
            for hook in &hooks {
                let Some(sym) = elf.symbols.lookup(hook) else {
                    continue;
                };
                let head = app
                    .bytes(sym.addr, CODE_HASH_BYTES as u32)
                    .expect("the head is in a segment");
                println!(
                    "{file} {hook} {id} {} {}",
                    sym.size,
                    hex(&skeleton_hash(head))
                );
            }
        }
    }
}

/// The `(hook, guard)` rows of a module profile.
fn profile_guards(file: &str) -> Vec<(String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/hle/idf-5.5.3")
        .join(file);
    let text = std::fs::read_to_string(path).expect("the profile is part of the tree");
    let mut out = Vec::new();
    let mut hook: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            if line != "[[variant]]" {
                hook = None;
            }
            if line == "[[hook]]" {
                hook = Some(String::new());
            }
        } else if let Some(name) = line.strip_prefix("name = \"")
            && hook.as_deref() == Some("")
        {
            hook = Some(name.trim_end_matches('"').to_string());
        } else if let Some(guard) = line.strip_prefix("guard = \"") {
            let hook = hook.clone().expect("a guard belongs to a hook");
            out.push((hook, guard.trim_end_matches('"').to_string()));
        }
    }
    out
}

/// What a `guard` row of `wifi.toml` claims, on every corpus build that links the row: before
/// the hook's own body calls anything it stores only into its frame, and its first call is the
/// guard, or one open-source function of which the same holds. So a run that enters the body
/// unhooked reaches the guard's tripwire having changed nothing outside a stack frame.
#[test]
fn the_first_call_of_every_guarded_hook_reaches_its_guard() {
    let guards = profile_guards("wifi.toml");
    assert_eq!(guards.len(), 16, "the guarded rows of wifi.toml");
    let blob = pemu_hle::tripwire::blob_defined_set();
    for (id, flash, elf_bytes) in &builds() {
        let elf = ElfInfo::parse(elf_bytes).expect("the ELF parses");
        let app = app(flash);
        let mut checked = 0;
        for (hook, guard) in &guards {
            let Some(sym) = elf.symbols.lookup(hook) else {
                continue;
            };
            let guard_at = elf
                .symbols
                .addr_of(guard)
                .unwrap_or_else(|| panic!("{id}: {hook} is linked and its guard {guard} is not"));
            let (mut addr, mut size, mut name) = (sym.addr, sym.size, hook.clone());
            let mut reached = false;
            for _ in 0..2 {
                let code = app.bytes(addr, size).expect("the body is in a segment");
                let (at, target) = calls(code, addr)
                    .into_iter()
                    .find(|(_, target)| !(addr..addr + size).contains(target))
                    .unwrap_or_else(|| panic!("{id}: {name} calls nothing"));
                let mut off = 0usize;
                while off < at as usize {
                    let op = decode_at(&code[off..], addr + off as u32).expect("decodes");
                    assert!(
                        !matches!(op.kind, K_SB | K_SH | K_SW) || op.rs1 == 2,
                        "{id}: {name}+{off:#x} stores outside its frame before its first call"
                    );
                    off += usize::from(op.len.max(2));
                }
                if target == guard_at {
                    reached = true;
                    break;
                }
                let helper = elf
                    .symbols
                    .func_at(target)
                    .unwrap_or_else(|| panic!("{id}: {name} first calls {target:#010x}"));
                assert!(
                    !blob.contains(&helper.name),
                    "{id}: {name} first calls the blob function {}, not {guard}",
                    helper.name
                );
                (addr, size, name) = (helper.addr, helper.size, helper.name.clone());
            }
            assert!(
                reached,
                "{id}: {hook} does not reach {guard} by its first calls"
            );
            checked += 1;
        }
        println!("RAN {id}: {checked} guarded hooks reach their guard first");
    }
}
