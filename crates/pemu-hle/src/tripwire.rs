//! Tripwires: hooks on code and addresses the HLE design never expects the guest to reach, built
//! per image at bind time.
//!
//! The set is the blob-defined symbols (every defined symbol of the IDF 5.5.3 blob archives,
//! `specs/hle/idf-5.5.3/blob-symbols.txt`) intersected with the image's symbol table, minus hooked
//! symbols, minus the coexistence allowlist that runs for real; plus the ROM `r_rwip_*` and
//! `r_btdm_*` entry points, plus one access tripwire on the radio MMIO ranges outside the 14 boot
//! `rtc_sleep_pu` accesses. It is never copied from one build's boundary list, which misses public
//! APIs such as `esp_wifi_set_ps`.

use std::collections::{BTreeMap, BTreeSet};

use pemu_loader::elf::ElfInfo;
use pemu_loader::symbols::SymbolTable;

/// Tripwire hook kinds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TripKind {
    /// A function inside a closed radio blob.
    BlobInternal,
    /// An `r_rwip_*` or `r_btdm_*` ROM entry point, a tripwire for every image.
    Rwip,
    /// Translation of a PC in the magic range that is not one of `MagicPcs`.
    MagicRangeFetch,
    /// An access to the radio MMIO ranges outside the 14 boot `rtc_sleep_pu` accesses.
    RadioMmio,
    /// The entry of a radio API whose feature is not bound, where the real blob path would spin
    /// or assert (`esp_wifi_init` without the Wi-Fi HLE loops on `phy_init.c:327`).
    DisabledFeature,
    /// Three consecutive resets of an image without an ELF with identical panic text (the
    /// `phy_init.c:327` assert loop of an image that starts Wi-Fi). Last, so the encoding of the
    /// other kinds is unchanged.
    ResetLoop,
}

impl TripKind {
    /// In encoding order: the index is the payload of a `HookId`.
    pub const ALL: [TripKind; 6] = [
        TripKind::BlobInternal,
        TripKind::Rwip,
        TripKind::MagicRangeFetch,
        TripKind::RadioMmio,
        TripKind::DisabledFeature,
        TripKind::ResetLoop,
    ];

    pub fn from_index(index: u32) -> Option<TripKind> {
        TripKind::ALL.get(index as usize).copied()
    }

    /// The one-word spelling a receipt lists a hit under (`hle.tripwires_hit`). On the type, so an
    /// exhaustive `match` makes a new variant without a word a build error.
    pub const fn word(self) -> &'static str {
        match self {
            TripKind::BlobInternal => "blob_internal",
            TripKind::Rwip => "rwip",
            TripKind::MagicRangeFetch => "magic_range_fetch",
            TripKind::RadioMmio => "radio_mmio",
            TripKind::DisabledFeature => "disabled_feature",
            TripKind::ResetLoop => "reset_loop",
        }
    }
}

/// Prefixes of the ROM radio entry points that are a tripwire for every image.
pub const ROM_RADIO_PREFIXES: [&str; 2] = ["r_rwip_", "r_btdm_"];

/// The tripwires of one image: which symbol each tripped PC names, and the per-kind counts the
/// receipt reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TripwireSet {
    by_pc: BTreeMap<u32, (TripKind, String)>,
}

impl TripwireSet {
    /// A pc already armed keeps its first kind, so the result does not depend on whether the
    /// per-image or the ROM rule ran first.
    pub fn arm(&mut self, pc: u32, kind: TripKind, symbol: &str) {
        self.by_pc
            .entry(pc)
            .or_insert_with(|| (kind, symbol.to_string()));
    }

    pub fn at(&self, pc: u32) -> Option<(TripKind, &str)> {
        self.by_pc.get(&pc).map(|(k, s)| (*k, s.as_str()))
    }

    /// The count the receipt reports.
    pub fn len(&self) -> usize {
        self.by_pc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_pc.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, TripKind, &str)> + '_ {
        self.by_pc
            .iter()
            .map(|(pc, (kind, sym))| (*pc, *kind, sym.as_str()))
    }

    pub fn count_of(&self, kind: TripKind) -> usize {
        self.by_pc.values().filter(|(k, _)| *k == kind).count()
    }

    /// The per-image rule: `blob` (the pinned `blob-symbols.txt`) intersected with the image's
    /// defined function and object symbols, minus `hooked`, minus `allowlist`. An undefined symbol
    /// has no entry pc to arm.
    pub fn arm_image(
        &mut self,
        blob: &BTreeSet<String>,
        image: &SymbolTable,
        hooked: &BTreeSet<String>,
        allowlist: &BTreeSet<String>,
    ) {
        for name in blob {
            if hooked.contains(name) || allowlist.contains(name) {
                continue;
            }
            let Some(sym) = image.lookup(name) else {
                continue;
            };
            self.arm(sym.addr, TripKind::BlobInternal, name);
        }
    }

    /// The ROM half of the rule: every `r_rwip_*` and `r_btdm_*` entry point of the ROM ELF,
    /// available without an app ELF because the ROM ELF is present for every run.
    pub fn arm_rom(&mut self, rom: &SymbolTable) {
        for sym in rom.iter() {
            if sym.is_defined()
                && ROM_RADIO_PREFIXES
                    .iter()
                    .any(|prefix| sym.name.starts_with(prefix))
            {
                self.arm(sym.addr, TripKind::Rwip, &sym.name);
            }
        }
    }
}

/// Builds the tripwire set of one image.
pub fn build(
    blob: &BTreeSet<String>,
    app: Option<&ElfInfo>,
    rom: Option<&SymbolTable>,
    hooked: &BTreeSet<String>,
    allowlist: &BTreeSet<String>,
) -> TripwireSet {
    let mut set = TripwireSet::default();
    if let Some(app) = app {
        set.arm_image(blob, &app.symbols, hooked, allowlist);
    }
    if let Some(rom) = rom {
        set.arm_rom(rom);
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol};

    fn sym(name: &str, addr: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size: 4,
            kind: SymKind::Func,
            bind: SymBind::Global,
            section: SymSection::Index(1),
        }
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn kinds_round_trip_through_their_encoding_index() {
        for (index, kind) in TripKind::ALL.iter().enumerate() {
            assert_eq!(TripKind::from_index(index as u32), Some(*kind));
        }
        assert_eq!(TripKind::from_index(TripKind::ALL.len() as u32), None);
    }

    #[test]
    fn the_image_rule_is_the_intersection_minus_hooks_and_the_allowlist() {
        // `esp_coex_adapter_register` and `coex_pre_init` run for real, `ppTxPkt` is a blob
        // internal, `esp_wifi_init` is hooked, and `pp_not_linked` is not in this image.
        let blob = names(&[
            "ppTxPkt",
            "esp_wifi_init",
            "esp_coex_adapter_register",
            "coex_pre_init",
            "pp_not_linked",
        ]);
        let image = SymbolTable::new(vec![
            sym("ppTxPkt", 0x4200_1000),
            sym("esp_wifi_init", 0x4200_2000),
            sym("esp_coex_adapter_register", 0x4200_3000),
            sym("coex_pre_init", 0x4200_4000),
            sym("app_main", 0x4200_5000),
        ]);
        let set = build(
            &blob,
            None,
            None,
            &names(&["esp_wifi_init"]),
            &names(&["esp_coex_adapter_register", "coex_pre_init"]),
        );
        assert!(
            set.is_empty(),
            "no app ELF arms nothing from the image rule"
        );

        let mut set = TripwireSet::default();
        set.arm_image(
            &blob,
            &image,
            &names(&["esp_wifi_init"]),
            &names(&["esp_coex_adapter_register", "coex_pre_init"]),
        );
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.at(0x4200_1000),
            Some((TripKind::BlobInternal, "ppTxPkt"))
        );
        // A symbol the image does not link is skipped by name, not counted.
        assert!(set.iter().all(|(_, _, name)| name != "pp_not_linked"));
    }

    #[test]
    fn the_rom_rule_arms_every_radio_entry_point() {
        let rom = SymbolTable::new(vec![
            sym("r_rwip_time_get", 0x4000_1000),
            sym("r_btdm_task_post", 0x4000_2000),
            sym("ets_printf", 0x4000_3000),
        ]);
        let mut set = TripwireSet::default();
        set.arm_rom(&rom);
        assert_eq!(set.count_of(TripKind::Rwip), 2);
        assert_eq!(set.at(0x4000_3000), None);
    }

    #[test]
    fn the_first_kind_armed_at_a_pc_wins() {
        let mut set = TripwireSet::default();
        set.arm(0x10, TripKind::Rwip, "r_rwip_time_get");
        set.arm(0x10, TripKind::BlobInternal, "r_rwip_time_get");
        assert_eq!(set.at(0x10), Some((TripKind::Rwip, "r_rwip_time_get")));
        assert_eq!(set.len(), 1);
    }
}

/// `specs/hle/idf-5.5.3/tripwires.toml`: the coexistence allowlist, the ROM radio prefixes and
/// the radio MMIO ranges with their boot allowance.
pub const TRIPWIRES_TOML: &str = include_str!("../../../specs/hle/idf-5.5.3/tripwires.toml");

/// `specs/hle/idf-5.5.3/blob-symbols.txt`, generated by `cargo xtask codegen blob-symbols`.
pub const BLOB_SYMBOLS_TXT: &str = include_str!("../../../specs/hle/idf-5.5.3/blob-symbols.txt");

/// The blob-defined set, read from the pinned list: a header of `key value` lines, a blank line,
/// then one symbol per line. Only the body is read; `cargo xtask codegen --check` verifies the
/// header, the ordering and the digest.
pub fn blob_defined_set() -> BTreeSet<String> {
    let body = BLOB_SYMBOLS_TXT
        .split_once("\n\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    body.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// One radio MMIO window.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RadioRange {
    /// Block name of the `c3_devices!` row.
    pub name: &'static str,
    pub base: u32,
    /// Bytes.
    pub size: u32,
}

impl RadioRange {
    pub fn contains(&self, addr: u32) -> bool {
        addr >= self.base && u64::from(addr) < u64::from(self.base) + u64::from(self.size)
    }
}

/// The hand-written half of the rule, read from [`TRIPWIRES_TOML`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TripwireSpec {
    /// The coexistence functions that run for real and are subtracted from the set.
    pub coexistence_allow: BTreeSet<String>,
    pub rom_prefixes: Vec<String>,
    pub ranges: Vec<RadioRange>,
    pub boot_addresses: Vec<u32>,
    /// Boot accesses allowed in total: 14.
    pub boot_max_accesses: u32,
}

impl TripwireSpec {
    /// `the_compiled_in_spec_parses` proves the spec well formed, so this panics only in a build
    /// whose spec was edited without running the tests.
    pub fn load() -> TripwireSpec {
        match TripwireSpec::parse(TRIPWIRES_TOML) {
            Ok(spec) => spec,
            Err(err) => panic!("specs/hle/idf-5.5.3/tripwires.toml: {err}"),
        }
    }

    /// A malformed number, a range without a name, base or size, a range of size 0 or an unknown
    /// key is refused: a range parsed as base 0 would watch the wrong addresses, and a boot budget
    /// of 0 would trip on the first `rtc_sleep_pu` access.
    pub fn parse(text: &str) -> Result<TripwireSpec, String> {
        let mut spec = TripwireSpec::default();
        let mut section = String::new();
        let mut range: Option<PartialRange> = None;
        let mut max_accesses = None;
        for line in logical_lines(text) {
            let line = line.as_str();
            if let Some(name) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
                if let Some(done) = range.take() {
                    spec.ranges.push(done.finish()?);
                }
                if name != "range" {
                    return Err(format!("unknown array table `[[{name}]]`"));
                }
                section = name.to_string();
                range = Some(PartialRange::default());
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if let Some(done) = range.take() {
                    spec.ranges.push(done.finish()?);
                }
                section = name.to_string();
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("`{line}` is not `key = value`"))?;
            let (key, value) = (key.trim(), value.trim());
            let num = |v: &str| {
                number(v).ok_or_else(|| format!("[{section}] {key}: `{v}` is not a number"))
            };
            match (section.as_str(), key) {
                ("coexistence", "allow") => spec.coexistence_allow = string_list(value),
                ("rom", "prefixes") => spec.rom_prefixes = string_vec(value),
                ("range", field) => {
                    let slot = range.as_mut().ok_or("a range key outside [[range]]")?;
                    match field {
                        "name" => slot.name = Some(range_name(unquote(value))),
                        "base" => slot.base = Some(num(value)?),
                        "size" => slot.size = Some(num(value)?),
                        other => return Err(format!("[[range]]: unknown key `{other}`")),
                    }
                }
                ("boot", "max_accesses") => max_accesses = Some(num(value)?),
                ("boot", "addresses") => {
                    spec.boot_addresses = value
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(num)
                        .collect::<Result<_, _>>()?;
                }
                // Names for a reader and notes carry no rule.
                ("boot", "names") | ("note", _) => {}
                (section, key) => return Err(format!("[{section}]: unknown key `{key}`")),
            }
        }
        if let Some(done) = range.take() {
            spec.ranges.push(done.finish()?);
        }
        spec.boot_max_accesses = max_accesses.ok_or("[boot] max_accesses is missing")?;
        if spec.ranges.is_empty() {
            return Err("no [[range]] is declared".to_string());
        }
        Ok(spec)
    }

    pub fn in_radio_range(&self, addr: u32) -> Option<&RadioRange> {
        self.ranges.iter().find(|r| r.contains(addr))
    }
}

/// A `[[range]]` table while its keys are read.
#[derive(Default)]
struct PartialRange {
    name: Option<&'static str>,
    base: Option<u32>,
    size: Option<u32>,
}

impl PartialRange {
    fn finish(self) -> Result<RadioRange, String> {
        let name = self.name.ok_or("a [[range]] has no name")?;
        let base = self
            .base
            .ok_or_else(|| format!("range {name} has no base"))?;
        let size = self
            .size
            .filter(|size| *size > 0)
            .ok_or_else(|| format!("range {name} has no size, or size 0"))?;
        Ok(RadioRange { name, base, size })
    }
}

/// The `c3_devices!` block names of the radio windows. A leaked `String` would be a slow leak per
/// load, so the five known names map to `&'static str` and anything else is `"radio"`.
fn range_name(name: &str) -> &'static str {
    match name {
        "radio_fe2" => "radio_fe2",
        "radio_fe" => "radio_fe",
        "radio_nrx" => "radio_nrx",
        "radio_bb" => "radio_bb",
        "radio_ble" => "radio_ble",
        _ => "radio",
    }
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"')
}

fn number(value: &str) -> Option<u32> {
    let value = value.split('#').next()?.trim();
    match value.strip_prefix("0x").or(value.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

fn string_vec(value: &str) -> Vec<String> {
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(unquote)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

/// The non-comment lines of a TOML file, with an array that spans several lines (`key = [`, one
/// element per line, `]`) joined into one line and its per-element comments dropped.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut open: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        match open.as_mut() {
            Some(buffer) => {
                buffer.push_str(line);
                if line.ends_with(']') {
                    out.extend(open.take());
                }
            }
            None if line.ends_with('[') => open = Some(line.to_string()),
            None => out.push(line.to_string()),
        }
    }
    out.extend(open);
    out
}

fn string_list(value: &str) -> BTreeSet<String> {
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(unquote)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

/// The radio MMIO tripwire: any access to a radio range outside the 14 boot `rtc_sleep_pu`
/// accesses stops the run with `E_TRIPWIRE`. The allowance is a budget spent wherever the accesses
/// happen, so the check needs no "boot is over" signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadioMmioWatch {
    spec: TripwireSpec,
    used: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadioMmioHit {
    pub range: &'static str,
    pub addr: u32,
    pub detail: String,
}

impl RadioMmioWatch {
    /// The whole boot allowance unspent.
    pub fn new() -> RadioMmioWatch {
        RadioMmioWatch {
            spec: TripwireSpec::load(),
            used: 0,
        }
    }

    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn spec(&self) -> &TripwireSpec {
        &self.spec
    }

    /// `None` when `addr` is outside every radio window or is one of the 14 boot accesses.
    pub fn access(&mut self, addr: u32) -> Option<RadioMmioHit> {
        let range = self.spec.in_radio_range(addr)?.name;
        let word = addr & !3;
        if self.spec.boot_addresses.contains(&word) && self.used < self.spec.boot_max_accesses {
            self.used += 1;
            return None;
        }
        let detail = if self.spec.boot_addresses.contains(&word) {
            format!(
                "{range} {addr:#010x}: the {} boot rtc_sleep_pu accesses are already spent",
                self.spec.boot_max_accesses
            )
        } else {
            format!("{range} {addr:#010x}: not one of the boot rtc_sleep_pu accesses")
        };
        Some(RadioMmioHit {
            range,
            addr,
            detail,
        })
    }
}

impl Default for RadioMmioWatch {
    fn default() -> RadioMmioWatch {
        RadioMmioWatch::new()
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[test]
    fn the_pinned_blob_set_is_read_from_the_generated_file() {
        let blob = blob_defined_set();
        // The file is generated, so the assertion is on its shape, not on a count that moves with
        // the IDF pin: `cargo xtask codegen --check` is what verifies the file itself.
        assert!(blob.len() > 1000, "{} symbols", blob.len());
        // Public APIs a single build's boundary list misses; the rule covers them because it
        // reads the blob archives.
        for name in [
            "esp_wifi_set_ps",
            "esp_wifi_set_country",
            "esp_now_init",
            "esp_wifi_set_promiscuous",
        ] {
            assert!(blob.contains(name), "{name} is blob-defined");
        }
        assert!(blob.contains("esp_wifi_internal_tx_by_ref"));
        assert!(
            blob.iter().any(|s| s.starts_with("phy_")),
            "phy_init internals"
        );
        // `esp_wifi_init` is open-source (`wifi_init.c`), so it is hooked, not blob-defined.
        assert!(!blob.contains("esp_wifi_init"));
        assert!(
            blob.contains("coex_pre_init"),
            "the allowlist names are in the set too"
        );
        assert!(!blob.iter().any(|s| s.is_empty() || s.contains(' ')));
    }

    #[test]
    fn the_compiled_in_spec_parses() {
        assert!(TripwireSpec::parse(TRIPWIRES_TOML).is_ok());
    }

    #[test]
    fn a_malformed_spec_is_refused_rather_than_read_as_zero() {
        let good = TRIPWIRES_TOML;
        for (what, bad) in [
            (
                "base",
                good.replace("base = 0x60005000", "base = 0x6000500G"),
            ),
            ("size", good.replace("size = 0x400", "size = 0")),
            ("missing base", good.replace("base = 0x6001D000\n", "")),
            (
                "budget",
                good.replace("max_accesses = 14", "max_accesses = fourteen"),
            ),
            ("address", good.replace("0x6001CCD4", "0x6001CCZ4")),
            (
                "key",
                good.replace("size = 0x400", "size = 0x400\nsise = 1"),
            ),
        ] {
            assert_ne!(bad, good, "{what}: the edit applied");
            assert!(TripwireSpec::parse(&bad).is_err(), "{what}");
        }
    }

    #[test]
    fn the_spec_file_parses_into_the_three_parts_of_the_rule() {
        let spec = TripwireSpec::load();
        assert_eq!(
            spec.coexistence_allow,
            [
                "esp_coex_adapter_register",
                "coex_pre_init",
                // Their direct-call closure, which the corpus test walks.
                "coex_core_pre_init",
                "coex_schm_init",
                "coex_rom_osi_funcs_init",
                "coex_rom_data_init",
            ]
            .map(str::to_string)
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
        // Every allowlisted name is blob-defined, or subtracting it would do nothing.
        let blob = blob_defined_set();
        assert!(
            spec.coexistence_allow
                .iter()
                .all(|name| blob.contains(name))
        );
        assert_eq!(spec.rom_prefixes, ["r_rwip_", "r_btdm_"]);
        assert_eq!(spec.rom_prefixes, ROM_RADIO_PREFIXES);
        assert_eq!(spec.boot_max_accesses, 14);
        assert_eq!(
            spec.boot_addresses,
            [0x6000_50F0, 0x6000_6090, 0x6001_CCD4, 0x6001_D054]
        );
        // The five c3_devices! radio windows.
        let names: Vec<&str> = spec.ranges.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "radio_fe2",
                "radio_fe",
                "radio_nrx",
                "radio_bb",
                "radio_ble"
            ]
        );
        assert_eq!(spec.ranges[2].base, 0x6001_CC00);
        assert_eq!(spec.ranges[2].size, 0x400);
        // Every boot address lies in a window, and the BLE spin register does too.
        for addr in &spec.boot_addresses {
            assert!(spec.in_radio_range(*addr).is_some(), "{addr:#010x}");
        }
        assert_eq!(
            spec.in_radio_range(0x6003_101C).map(|r| r.name),
            Some("radio_ble")
        );
    }

    #[test]
    fn the_fourteen_boot_accesses_pass_and_anything_else_trips() {
        let mut watch = RadioMmioWatch::new();
        let boot = [0x6000_50F0u32, 0x6000_6090, 0x6001_CCD4, 0x6001_D054];
        // rtc_sleep_pu reads 0 and writes 0 on each of four registers; 14 accesses in all.
        for i in 0..14 {
            assert_eq!(watch.access(boot[i % boot.len()]), None, "boot access {i}");
        }
        assert_eq!(watch.used(), 14);
        // The 15th access to the same register trips.
        let hit = watch.access(boot[0]).expect("the budget is spent");
        assert_eq!(hit.range, "radio_fe2");
        assert!(hit.detail.contains("already spent"), "{}", hit.detail);
    }

    #[test]
    fn a_planted_access_outside_the_boot_set_trips_immediately() {
        // ROM r_rwip_time_get spins on the BLE baseband register whenever controller code runs,
        // so an access there is a tripwire and not a wait.
        let mut watch = RadioMmioWatch::new();
        let hit = watch
            .access(0x6003_101C)
            .expect("the BLE baseband register");
        assert_eq!(hit.range, "radio_ble");
        assert_eq!(hit.addr, 0x6003_101C);
        assert!(hit.detail.contains("not one of the boot"), "{}", hit.detail);
        assert_eq!(watch.used(), 0, "a trip spends no boot access");
    }

    #[test]
    fn an_access_outside_every_radio_window_is_not_a_tripwire() {
        let mut watch = RadioMmioWatch::new();
        for addr in [0x6000_0000u32, 0x6001_C000, 0x6003_2000, 0x3FC8_0000] {
            assert_eq!(watch.access(addr), None, "{addr:#010x}");
        }
    }

    #[test]
    fn the_rule_subtracts_the_coexistence_allowlist_from_the_pinned_set() {
        use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};
        let spec = TripwireSpec::load();
        let blob = blob_defined_set();
        let named = |name: &str, addr: u32| Symbol {
            name: name.to_string(),
            addr,
            size: 4,
            kind: SymKind::Func,
            bind: SymBind::Global,
            section: SymSection::Index(1),
        };
        let image = SymbolTable::new(vec![
            named("coex_pre_init", 0x4200_1000),
            named("esp_coex_adapter_register", 0x4200_1100),
            named("esp_wifi_init", 0x4200_1200),
            named("ppTxPkt", 0x4200_1300),
            named("app_main", 0x4200_1400),
        ]);
        let hooked = BTreeSet::from(["esp_wifi_init".to_string()]);
        let mut set = TripwireSet::default();
        set.arm_image(&blob, &image, &hooked, &spec.coexistence_allow);
        let armed: Vec<&str> = set.iter().map(|(_, _, name)| name).collect();
        assert_eq!(
            armed,
            ["ppTxPkt"],
            "the two coex names and the hook are subtracted"
        );
    }
}
