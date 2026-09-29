//! The receipt's tripwire count for `pk` and `official` equals the blob-defined set intersected
//! with the image symbol table minus hooks and the allowlist, checked on the `HookSet` binding
//! produces; and nothing the coexistence init calls directly is armed.
//!
//! The corpus is not in the repository, so a missing file skips with a printed reason. A present
//! file whose SHA-256 is not the one `MANIFEST.json` pins fails. No path or hash is printed.

// Test code is exempt from the core-crate API bans; `clippy.toml` bans these for the library.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use pemu_core::snap::SectionId;
use pemu_hle::binding::{
    BindingMismatch, BindingProfile, BoundHooks, BoundSymbol, ImageView, MachineConfigFragment,
    RadioModule, bind_all, bind_profile_image,
};
use pemu_hle::hooks::{HandlerKind, HookKind, HookRef, HookSet, ModuleIndex};
use pemu_hle::magic::{MagicKind, MagicPcs};
use pemu_hle::observe::ObserveKind;
use pemu_hle::tripwire::{ROM_RADIO_PREFIXES, TripKind, TripwireSpec, blob_defined_set};
use pemu_loader::elf::ElfInfo;
use pemu_loader::symbols::{SymKind, SymbolTable};
use pemu_loader::{hex, sha256};
use pemu_rv32::decode::decode_at;
use pemu_rv32::op::{K_AUIPC, K_JAL, K_JALR};

/// The data root, read as `pemu_testkit::corpus` reads it: `PASSPORTSIM_DATA_ROOT` only, and only
/// when absolute. This crate may not depend on `pemu-testkit`, hence the local copy.
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

/// The app ELF of corpus entry `id`, checked against `MANIFEST.json`, or `None` after printing
/// why the test skips.
fn app_elf(id: &str) -> Option<Vec<u8>> {
    let root = data_root()?;
    let manifest_path = root.join("corpus/MANIFEST.json");
    let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
        eprintln!("skip: the corpus manifest is absent");
        return None;
    };
    let record = manifest.split('{').find(|r| {
        field(r, "id").as_deref() == Some(id)
            && field(r, "file").is_some_and(|f| f.ends_with(".elf") && !f.starts_with("bootloader"))
    });
    let (Some(path), Some(want)) = record
        .map(|r| (field(r, "path"), field(r, "sha256")))
        .unwrap_or((None, None))
    else {
        eprintln!("skip: corpus {id} has no app ELF record");
        return None;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skip: corpus {id} app ELF is absent");
        return None;
    };
    assert_eq!(
        hex(&sha256(&bytes)),
        want,
        "corpus {id} app ELF is not the file MANIFEST.json pins"
    );
    Some(bytes)
}

fn field(record: &str, key: &str) -> Option<String> {
    let at = record.find(&format!("\"{key}\""))?;
    let rest = &record[at + key.len() + 2..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].replace("\\\\", "\\"))
}

/// The Wi-Fi and BLE entry points the radio modules replace whole. `pemu-hle` registers no radio
/// module, so a stand-in hooks this boundary the way they do.
const REPLACED: &[&str] = &[
    // BLE: the 7 VHCI functions of bt.c.
    "esp_bt_controller_init",
    "esp_bt_controller_deinit",
    "esp_bt_controller_enable",
    "esp_bt_controller_disable",
    "esp_vhci_host_check_send_available",
    "esp_vhci_host_send_packet",
    "esp_vhci_host_register_callback",
    // Wi-Fi: init, deinit and the data plane.
    "esp_wifi_init",
    "esp_wifi_deinit",
    "esp_wifi_internal_tx",
    "esp_wifi_internal_reg_rxcb",
    "esp_wifi_internal_free_rx_buffer",
    "esp_wifi_internal_reg_netstack_buf_cb",
    "esp_wifi_internal_set_sta_ip",
];

/// Hooks [`REPLACED`], skipping what the image does not link.
struct Boundary;

impl RadioModule for Boundary {
    fn name(&self) -> &'static str {
        "boundary"
    }

    fn bind(&self, elf: &ElfInfo) -> Result<HookSet, Vec<BindingMismatch>> {
        let profile = BindingProfile {
            idf: "5.5.3",
            symbols: REPLACED
                .iter()
                .enumerate()
                .map(|(i, name)| BoundSymbol::hook(name, HookKind::Hle(HandlerKind(i as u16))))
                .collect(),
            log_lines: Vec::new(),
        };
        bind_profile_image(&profile, &ImageView::symbols_only(elf), ModuleIndex(1))
            .map(|bound| bound.set)
    }

    fn snapshot_sections(&self) -> &'static [SectionId] {
        &[]
    }

    fn config_fragment(&self) -> MachineConfigFragment {
        MachineConfigFragment::default()
    }
}

/// The rev101 ROM ELF bundled in `assets/rom/`.
fn rom() -> SymbolTable {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/rom/esp32c3_rev101_rom.elf");
    let bytes = std::fs::read(&path).expect("the bundled ROM ELF is part of the tree");
    ElfInfo::parse(&bytes).expect("the ROM ELF parses").symbols
}

/// One corpus image bound the way the machine binds it: the core hooks, the boundary module and
/// the tripwires, with the ROM table in the view.
fn bound_image(id: &str) -> Option<(ElfInfo, Vec<u8>, SymbolTable, BoundHooks)> {
    let bytes = app_elf(id)?;
    let elf = ElfInfo::parse(&bytes).expect("the corpus app ELF parses");
    let rom = rom();
    let bound = bind_all(&[&Boundary], &ImageView::new(&elf, &bytes).with_rom(&rom));
    assert!(bound.mismatches.is_empty(), "{:?}", bound.mismatches);
    Some((elf, bytes, rom, bound))
}

fn tripwire_pcs(bound: &BoundHooks, want: TripKind) -> BTreeSet<u32> {
    bound
        .set
        .iter()
        .filter(|(_, id)| {
            HookRef::from_id(*id).is_some_and(|hook| hook.kind == HookKind::Tripwire(want))
        })
        .map(|(pc, _)| pc)
        .collect()
}

/// The tripwire rule computed from its inputs without `pemu-hle`'s arming code, compared with the
/// hooks in the bound set. Returns the receipt count.
fn check_rule(id: &str) -> Option<usize> {
    let (elf, _, rom, bound) = bound_image(id)?;
    let spec = TripwireSpec::load();
    let blob = blob_defined_set();

    // A set holds one hook per pc, and the rule subtracts hooked symbols.
    let mut hooked: BTreeSet<u32> = REPLACED
        .iter()
        .filter_map(|name| elf.symbols.addr_of(name))
        .collect();
    hooked.extend(
        ObserveKind::ALL
            .iter()
            .filter_map(|k| elf.symbols.addr_of(k.symbol())),
    );
    let pcs = MagicPcs::from_spec().expect("specs/magic-pcs.toml");
    hooked.extend(MagicKind::ALL.iter().map(|k| pcs.pc_of(*k)));

    // These images fold identical functions, so several names can share one pc; a tripwire is
    // one hook per pc, and the receipt count is the number of distinct pcs.
    let blob_pcs: BTreeSet<u32> = blob
        .iter()
        .filter(|name| !spec.coexistence_allow.contains(*name))
        .filter_map(|name| elf.symbols.addr_of(name))
        .filter(|pc| !hooked.contains(pc))
        .collect();
    let rom_pcs: BTreeSet<u32> = rom
        .iter()
        .filter(|sym| {
            sym.is_defined() && ROM_RADIO_PREFIXES.iter().any(|p| sym.name.starts_with(p))
        })
        .map(|sym| sym.addr)
        .filter(|pc| !hooked.contains(pc) && !blob_pcs.contains(pc))
        .collect();

    assert_eq!(
        tripwire_pcs(&bound, TripKind::BlobInternal),
        blob_pcs,
        "corpus {id}: the BlobInternal hooks of the bound set are the blob-defined set intersected \
         with the image symbol table minus hooks and the coexistence allowlist"
    );
    assert_eq!(
        tripwire_pcs(&bound, TripKind::Rwip),
        rom_pcs,
        "corpus {id}: the Rwip hooks are the ROM r_rwip_* and r_btdm_* entry points"
    );
    assert!(
        !rom_pcs.is_empty(),
        "the rev101 ROM has r_rwip_* entry points"
    );
    let count = bound.tripwires.len();
    assert_eq!(count, blob_pcs.len() + rom_pcs.len());
    for (pc, kind, _) in bound.tripwires.iter() {
        assert_eq!(
            bound.set.get(pc).and_then(HookRef::from_id),
            Some(HookRef::core(HookKind::Tripwire(kind)))
        );
    }
    for name in REPLACED {
        if let Some(pc) = elf.symbols.addr_of(name) {
            assert!(
                matches!(
                    bound.set.get(pc).and_then(HookRef::from_id),
                    Some(HookRef {
                        kind: HookKind::Hle(_),
                        ..
                    })
                ),
                "corpus {id}: {name}"
            );
        }
    }
    // A build's `boundary.txt` lists only the blob symbols its open-source objects reference, so
    // public APIs such as `esp_wifi_set_ps` are missing from it. Every linked blob-defined
    // function must be covered, so the count must be well above that seed list's 138 rows.
    assert!(
        blob_pcs.len() > 138,
        "corpus {id}: {} blob tripwires, not more than the 138 seed rows",
        blob_pcs.len()
    );
    eprintln!(
        "corpus {id}: {count} tripwire hook(s), {} blob and {} ROM (receipt count)",
        blob_pcs.len(),
        rom_pcs.len()
    );
    Some(count)
}

#[test]
fn t1_tripwire_hooks_of_pk_follow_the_rule() {
    let _ = check_rule("pk");
}

#[test]
fn t1_tripwire_hooks_of_official_follow_the_rule() {
    let _ = check_rule("official");
}

/// The direct call targets of the function `name`: `jal` and `c.jal` with `rd` `ra`, tail jumps
/// (`rd` 0) that leave the function, and `auipc` + `jalr` pairs through the same register.
/// Decoded with `pemu-rv32`'s own decoder. Indirect calls through a table are not followed.
fn direct_callees(elf: &ElfInfo, file: &[u8], name: &str) -> Vec<u32> {
    let Some(sym) = elf.symbols.lookup(name).filter(|s| s.kind == SymKind::Func) else {
        return Vec::new();
    };
    let Some(section) = elf.sections.iter().find(|s| {
        s.is_alloc() && s.has_bits() && sym.addr >= s.addr && u64::from(sym.addr) < s.end()
    }) else {
        return Vec::new();
    };
    let data = section.data(file).expect("section bytes");
    let (start, end) = (sym.addr, sym.addr.saturating_add(sym.size));
    let mut out = Vec::new();
    let mut pc = start;
    let mut auipc: Option<(u8, u32)> = None;
    while pc < end {
        let Some(op) = data
            .get((pc - section.addr) as usize..)
            .and_then(|bytes| decode_at(bytes, pc))
        else {
            break;
        };
        let target = match op.kind {
            K_JAL if op.rd <= 1 => Some(op.imm as u32),
            K_JALR => auipc
                .filter(|(rd, _)| *rd == op.rs1)
                .map(|(_, base)| base.wrapping_add(op.imm as u32) & !1),
            _ => None,
        };
        auipc = (op.kind == K_AUIPC).then_some((op.rd, op.imm as u32));
        if let Some(target) = target
            && !(start..end).contains(&target)
        {
            out.push(target);
        }
        pc += u32::from(op.len.max(2));
    }
    out
}

/// `esp_coex_adapter_register` and `coex_pre_init` run for real at boot, so nothing they reach
/// through direct calls may be a tripwire. The walk follows app functions (including the newlib
/// and VFS code `coexist_printf` pulls in) and checks ROM targets without walking into the ROM.
fn check_coexistence_closure(id: &str) {
    let Some((elf, bytes, _, bound)) = bound_image(id) else {
        return;
    };
    let spec = TripwireSpec::load();
    let mut todo: Vec<String> = spec.coexistence_allow.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    let mut reached_blob = BTreeSet::new();
    let blob = blob_defined_set();
    while let Some(name) = todo.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let pc = elf.symbols.addr_of(&name);
        if let Some(pc) = pc
            && let Some((kind, armed)) = bound.tripwires.at(pc)
        {
            panic!(
                "corpus {id}: {name} at {pc:#010x} is reached from the coexistence allowlist \
                 by direct calls, and is armed as {kind:?} ({armed})"
            );
        }
        if blob.contains(&name) {
            reached_blob.insert(name.clone());
        }
        for target in direct_callees(&elf, &bytes, &name) {
            assert!(
                bound.tripwires.at(target).is_none(),
                "corpus {id}: {name} calls {target:#010x}, which is armed"
            );
            if let Some(callee) = elf.symbols.func_at(target).filter(|s| s.addr == target) {
                todo.push(callee.name.clone());
            }
        }
    }
    // The walk is not vacuous: coex_pre_init reaches the blob's own init functions.
    assert!(
        reached_blob.contains("coex_core_pre_init"),
        "corpus {id}: {reached_blob:?}"
    );
    eprintln!(
        "corpus {id}: the coexistence allowlist reaches {} function(s) by direct calls, {} of \
         them blob-defined, none armed",
        seen.len(),
        reached_blob.len()
    );
}

#[test]
fn t1_nothing_the_coexistence_allowlist_calls_directly_is_armed_in_pk() {
    check_coexistence_closure("pk");
}

#[test]
fn t1_nothing_the_coexistence_allowlist_calls_directly_is_armed_in_official() {
    check_coexistence_closure("official");
}
