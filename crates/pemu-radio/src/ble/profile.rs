//! The BLE binding profile, compiled in from `specs/hle/idf-5.5.3/ble.toml`.
//!
//! `TomlLite` skips what it does not recognize, so this module adds the strictness a binding
//! needs: every table is known, every row has its keys, every hash is 64 hex digits. A row that
//! parsed empty would make the binding guess.

use pemu_loader::bundle::{TomlLite, TomlTable};

use crate::heap_ledger::{CountClass, LedgerRow, Lifetime};
pub use crate::hle_common::{HookRow, Variant};
use crate::hle_common::{ProfileRows, Rows, need, need_u32, unknown_table};

pub const BLE_TOML: &str = include_str!("../../../../specs/hle/idf-5.5.3/ble.toml");

/// The `btController` worker parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRow {
    pub task_name: String,
    /// The priority `bt.c` requires of the controller config.
    pub priority: u32,
    /// The smallest stack `bt.c` accepts.
    pub min_stack: u32,
    /// U5 poll interval in ticks.
    pub poll_ticks: u32,
    /// Largest scratch block of a nested call from the worker task.
    pub max_scratch: u16,
    /// Interrupt-matrix source of the U4 magic ISR (UNVERIFIED).
    pub isr_source: u8,
}

/// The controller identity and capacity of `ble.toml` `[controller]`, class A from the probe
/// capture `device-probe_vhci` except where the `ble.toml` header says otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerRow {
    /// `HCI_Version` (Core assigned number; 9 is 5.0).
    pub hci_version: u8,
    /// `HCI_Subversion`.
    pub hci_subversion: u16,
    /// `LMP_Version`.
    pub lmp_version: u8,
    /// `Company_Identifier`.
    pub company: u16,
    /// `LMP_Subversion`.
    pub lmp_subversion: u16,
    /// `Supported_Commands` of `HCI_Read_Local_Supported_Commands`, 64 octets.
    pub supported_commands: [u8; 64],
    /// `LMP_Features` of `HCI_Read_Local_Supported_Features`.
    pub lmp_features: [u8; 8],
    /// `LE_Features` as a bitmap of Link Layer feature bits (Core Vol 6 Part B §4.6).
    pub le_features: u64,
    /// `LE_States` of `HCI_LE_Read_Supported_States`.
    pub supported_states: [u8; 8],
    /// `LE_ACL_Data_Packet_Length`.
    pub acl_len: u16,
    /// `Total_Num_LE_ACL_Data_Packets`.
    pub acl_count: u8,
    /// `Filter_Accept_List_Size`.
    pub accept_list_size: u8,
    /// `Resolving_List_Size`.
    pub resolving_list_size: u8,
    /// `TX_Power_Level` of the advertising physical channel, in dBm.
    pub adv_tx_power: i8,
    /// `Min_TX_Power` of `HCI_LE_Read_Transmit_Power`, in dBm.
    pub tx_power_min: i8,
    /// `Max_TX_Power` of `HCI_LE_Read_Transmit_Power`, in dBm.
    pub tx_power_max: i8,
    /// `Supported_Max_TX_Octets` and `_RX_Octets`.
    pub max_tx_octets: u16,
    /// `Supported_Max_TX_Time` and `_RX_Time`, in microseconds.
    pub max_tx_time: u16,
    /// `Max_Advertising_Data_Length`.
    pub max_adv_data_len: u16,
    /// `Num_Supported_Advertising_Sets`.
    pub adv_sets: u8,
    /// `Num_HCI_Command_Packets` of every acknowledgement.
    pub num_command_packets: u8,
    /// Virtual time from a host packet to the controller's events, in microseconds, for every
    /// packet but an `HCI_Reset`.
    pub reply_us: u32,
    /// The same for an `HCI_Reset`, which the controller answers after resetting itself.
    pub reset_reply_us: u32,
}

/// How the controller answers a `[[command]]` row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Answering {
    Modeled,
    /// Reported by the device, not modeled: refused with Unsupported Feature or Parameter Value.
    Unmodeled,
    /// Answered with Unknown HCI Command, as the device does.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRow {
    pub opcode: u16,
    pub name: String,
    /// `(octet, bit)` in `Supported_Commands` (Core Vol 4 Part E §6.27); `None` for the one command
    /// without a bit.
    pub supported: Option<(u8, u8)>,
    pub answer: Answering,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleProfile {
    pub idf: String,
    pub module: String,
    /// The 7 hooks, in handler order.
    pub hooks: Vec<HookRow>,
    pub calls: Vec<String>,
    /// Required data symbols with their size.
    pub data: Vec<(String, u32)>,
    pub worker: WorkerRow,
    pub controller: ControllerRow,
    pub commands: Vec<CommandRow>,
    pub heap: Vec<LedgerRow>,
}

impl BleProfile {
    /// The ledger rows that apply to an image whose `esp_bt_controller_init` matched a `verified`
    /// variant. A row with a `config` was read from one build's sdkconfig, so only an image of that
    /// shape gets it; every other image gets the capture-derived rows and a smaller bound.
    pub fn heap_plan(&self, verified: bool) -> Vec<&LedgerRow> {
        self.heap
            .iter()
            .filter(|row| verified || row.config.is_none())
            .collect()
    }
}

fn need_num<T: TryFrom<u64>>(table: &TomlTable, key: &str) -> Result<T, String> {
    table
        .integer(key)
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| format!("[{}] has no `{key}` in range", table.name))
}

fn opcode(text: &str) -> Result<u16, String> {
    text.strip_prefix("0x")
        .filter(|hex| hex.len() == 4)
        .and_then(|hex| u16::from_str_radix(hex, 16).ok())
        .ok_or_else(|| format!("opcode `{text}` is not 0x and 4 hex digits"))
}

fn supported_bit(text: &str) -> Result<Option<(u8, u8)>, String> {
    if text == "none" {
        return Ok(None);
    }
    let refuse = || format!("supported `{text}` is not octet.bit");
    let (octet, bit) = text.split_once('.').ok_or_else(refuse)?;
    let octet: u8 = octet.parse().map_err(|_| refuse())?;
    let bit: u8 = bit.parse().map_err(|_| refuse())?;
    if octet >= 64 || bit >= 8 {
        return Err(refuse());
    }
    Ok(Some((octet, bit)))
}

fn hex_bytes<const N: usize>(table: &TomlTable, key: &str) -> Result<[u8; N], String> {
    let text = need(table, key)?;
    let refuse = || format!("`{key}` is not {} hex digits", 2 * N);
    let bytes = text.as_bytes();
    if bytes.len() != 2 * N || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(refuse());
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|_| refuse())?;
    }
    Ok(out)
}

fn signed(table: &TomlTable, key: &str) -> Result<i8, String> {
    need(table, key)?
        .parse()
        .map_err(|_| format!("`{key}` is not a signed 8-bit number"))
}

fn controller_row(table: &TomlTable) -> Result<ControllerRow, String> {
    let le_features = need(table, "le_features")?;
    let le_features = le_features
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .ok_or_else(|| format!("le_features `{le_features}` is not 0x and hex digits"))?;
    Ok(ControllerRow {
        hci_version: need_num(table, "hci_version")?,
        hci_subversion: need_num(table, "hci_subversion")?,
        lmp_version: need_num(table, "lmp_version")?,
        company: need_num(table, "company")?,
        lmp_subversion: need_num(table, "lmp_subversion")?,
        supported_commands: hex_bytes(table, "supported_commands")?,
        lmp_features: hex_bytes(table, "lmp_features")?,
        le_features,
        supported_states: hex_bytes(table, "supported_states")?,
        acl_len: need_num(table, "acl_len")?,
        acl_count: need_num(table, "acl_count")?,
        accept_list_size: need_num(table, "accept_list_size")?,
        resolving_list_size: need_num(table, "resolving_list_size")?,
        adv_tx_power: signed(table, "adv_tx_power")?,
        tx_power_min: signed(table, "tx_power_min")?,
        tx_power_max: signed(table, "tx_power_max")?,
        max_tx_octets: need_num(table, "max_tx_octets")?,
        max_tx_time: need_num(table, "max_tx_time")?,
        max_adv_data_len: need_num(table, "max_adv_data_len")?,
        adv_sets: need_num(table, "adv_sets")?,
        num_command_packets: need_num(table, "num_command_packets")?,
        reply_us: need_num(table, "reply_us")?,
        reset_reply_us: need_num(table, "reset_reply_us")?,
    })
}

impl BleProfile {
    /// The compiled-in profile, parsed once. This crate's tests parse it, so it panics only in a
    /// build whose spec file was edited without running them.
    pub fn load() -> &'static BleProfile {
        static PROFILE: std::sync::OnceLock<BleProfile> = std::sync::OnceLock::new();
        PROFILE.get_or_init(|| {
            BleProfile::parse(BLE_TOML)
                .unwrap_or_else(|e| panic!("specs/hle/idf-5.5.3/ble.toml: {e}"))
        })
    }

    /// Parses a profile, refusing anything a binding could be guessed from.
    pub fn parse(text: &str) -> Result<BleProfile, String> {
        let doc = TomlLite::parse(text);
        let mut rows = ProfileRows::default();
        let mut worker = None;
        let mut controller = None;
        let mut commands: Vec<CommandRow> = Vec::new();
        let mut heap: Vec<LedgerRow> = Vec::new();
        for table in doc.tables() {
            if rows.take(table)? {
                continue;
            }
            match (table.name.as_str(), table.array) {
                ("worker", false) => {
                    worker = Some(WorkerRow {
                        task_name: need(table, "task_name")?.to_string(),
                        priority: need_u32(table, "priority")?,
                        min_stack: need_u32(table, "min_stack")?,
                        poll_ticks: need_u32(table, "poll_ticks")?,
                        max_scratch: u16::try_from(need_u32(table, "max_scratch")?)
                            .map_err(|_| "a max_scratch above 65535".to_string())?,
                        isr_source: u8::try_from(need_u32(table, "isr_source")?)
                            .map_err(|_| "an interrupt source above 255".to_string())?,
                    })
                }
                ("controller", false) => controller = Some(controller_row(table)?),
                ("command", true) => {
                    let row = CommandRow {
                        opcode: opcode(need(table, "opcode")?)?,
                        name: need(table, "name")?.to_string(),
                        supported: supported_bit(need(table, "supported")?)?,
                        answer: match need(table, "answer")? {
                            "modeled" => Answering::Modeled,
                            "unmodeled" => Answering::Unmodeled,
                            "unknown" => Answering::Unknown,
                            other => return Err(format!("unknown answer `{other}`")),
                        },
                    };
                    if commands.iter().any(|c| {
                        c.opcode == row.opcode
                            || (row.supported.is_some() && c.supported == row.supported)
                    }) {
                        return Err(format!("command `{}` repeats an opcode or a bit", row.name));
                    }
                    commands.push(row);
                }
                ("heap", true) => {
                    let row = LedgerRow {
                        label: need(table, "label")?.to_string(),
                        count: need_u32(table, "count")?,
                        element: need_u32(table, "element")?,
                        count_class: match need(table, "count_class")? {
                            "capture" => CountClass::Capture,
                            "sdkconfig" => CountClass::Sdkconfig,
                            "measured" => CountClass::Measured,
                            other => return Err(format!("unknown count_class `{other}`")),
                        },
                        count_source: need(table, "count_source")?.to_string(),
                        element_source: need(table, "element_source")?.to_string(),
                        config: match table.string("config") {
                            None => None,
                            Some("pk") => Some("pk".to_string()),
                            Some(other) => return Err(format!("unknown heap config `{other}`")),
                        },
                        lifetime: match table.string("lifetime") {
                            None | Some("init") => Lifetime::Init,
                            Some("enable") => Lifetime::Enable,
                            Some("boot") => Lifetime::Boot,
                            Some(other) => return Err(format!("unknown heap lifetime `{other}`")),
                        },
                    };
                    if row.count == 0 || row.element == 0 {
                        return Err(format!("heap row `{}` would allocate nothing", row.label));
                    }
                    if heap.iter().any(|r| r.label == row.label) {
                        return Err(format!("heap row `{}` repeats a label", row.label));
                    }
                    if heap.len() >= usize::from(u16::MAX) {
                        return Err("more heap rows than a block index can name".to_string());
                    }
                    heap.push(row);
                }
                (name, array) => return Err(unknown_table(name, array)),
            }
        }
        let Rows {
            idf,
            module,
            hooks,
            calls,
            data,
        } = rows.finish()?;
        Ok(BleProfile {
            idf,
            module,
            hooks,
            calls,
            data,
            worker: worker.ok_or("no [worker]")?,
            controller: controller.ok_or("no [controller]")?,
            commands,
            heap,
        })
    }

    pub fn hook(&self, handler: u16) -> Option<&HookRow> {
        self.hooks.iter().find(|h| h.handler == handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle_common::digest;
    use crate::log_lines::{LogLine, LogLines, Stage};

    /// Every capture-derived `count` repeats a `[controller]` field of the same capture, so a
    /// ledger row cannot drift from the capacity it bounds.
    #[test]
    fn every_capture_derived_heap_count_is_a_controller_capacity_of_the_same_capture() {
        let p = BleProfile::load();
        let c = &p.controller;
        for row in &p.heap {
            match row.count_class {
                CountClass::Capture => {
                    let capacity = match row.label.as_str() {
                        "acl_rx" => u32::from(c.acl_count),
                        "adv_data" => u32::from(c.adv_sets),
                        "accept_list" => u32::from(c.accept_list_size),
                        "resolving_list" => u32::from(c.resolving_list_size),
                        other => panic!("heap row `{other}` names no [controller] capacity"),
                    };
                    assert_eq!(
                        row.count, capacity,
                        "heap row `{}` does not repeat its capture capacity",
                        row.label
                    );
                    assert!(row.config.is_none(), "a capture row needs no sdkconfig");
                }
                // A row read from one build's sdkconfig must say which build, or it would be
                // applied to an image whose configuration was never read.
                CountClass::Sdkconfig => assert_eq!(
                    row.config.as_deref(),
                    Some("pk"),
                    "heap row `{}` reads a sdkconfig but names no build",
                    row.label
                ),
                // A measured row is one block of a total the campaign capture read on the
                // `pk`-shaped controller, so it too applies to that shape only.
                CountClass::Measured => {
                    assert_eq!(row.count, 1, "measured row `{}` is one block", row.label);
                    assert_eq!(
                        row.config.as_deref(),
                        Some("pk"),
                        "measured row `{}` names no build",
                        row.label
                    );
                }
            }
            assert!(
                !row.count_source.is_empty() && !row.element_source.is_empty(),
                "heap row `{}` cites nothing",
                row.label
            );
        }
    }

    /// The totals are literals so that an edit to a row has to change this test on purpose.
    #[test]
    fn the_heap_plan_totals_are_the_declared_ones() {
        let p = BleProfile::load();
        let labels: Vec<&str> = p.heap.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "acl_rx",
                "adv_data",
                "accept_list",
                "resolving_list",
                "link_env",
                "scan_dupl",
                "adv_dup_filt",
                "blob_init",
                "blob_init_2",
                "blob_init_3",
                "blob_retained",
                "blob_retained_tail",
                "blob_enable",
            ]
        );
        let bytes = |rows: Vec<&LedgerRow>| rows.iter().map(|r| r.bytes()).sum::<u32>();
        // An image whose sdkconfig was never read gets the four capture rows only.
        assert_eq!(bytes(p.heap_plan(false)), 3_916);
        assert_eq!(p.heap_plan(false).len(), 4);
        // `pk` and `pkgatt` add the three sdkconfig rows (1114 bytes) and the six measured ones.
        assert_eq!(bytes(p.heap_plan(true)), 28_554);
        assert_eq!(p.heap_plan(true).len(), 13);
        let by = |lifetime: Lifetime| {
            p.heap_plan(true)
                .into_iter()
                .filter(|r| r.lifetime == lifetime)
                .map(|r| r.bytes())
                .sum::<u32>()
        };
        assert_eq!(
            (by(Lifetime::Init), by(Lifetime::Boot), by(Lifetime::Enable)),
            (5_030 + 23_308, 184, 32)
        );
    }

    #[test]
    fn a_heap_row_that_would_allocate_nothing_or_repeat_a_label_is_refused() {
        let base = BLE_TOML;
        let empty = format!(
            "{base}\n[[heap]]\nlabel = \"z\"\ncount = 0\nelement = 4\ncount_class = \"capture\"\ncount_source = \"s\"\nelement_source = \"s\"\n"
        );
        assert!(
            BleProfile::parse(&empty)
                .expect_err("a zero row")
                .contains("allocate nothing")
        );
        let repeat = format!(
            "{base}\n[[heap]]\nlabel = \"acl_rx\"\ncount = 1\nelement = 4\ncount_class = \"capture\"\ncount_source = \"s\"\nelement_source = \"s\"\n"
        );
        assert!(
            BleProfile::parse(&repeat)
                .expect_err("a repeated label")
                .contains("repeats a label")
        );
    }

    #[test]
    fn the_profile_names_the_seven_vhci_functions() {
        let p = BleProfile::load();
        assert_eq!((p.idf.as_str(), p.module.as_str()), ("5.5.3", "ble"));
        let names: Vec<&str> = p.hooks.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "esp_bt_controller_init",
                "esp_bt_controller_deinit",
                "esp_bt_controller_enable",
                "esp_bt_controller_disable",
                "esp_vhci_host_check_send_available",
                "esp_vhci_host_send_packet",
                "esp_vhci_host_register_callback",
            ]
        );
        for (i, hook) in p.hooks.iter().enumerate() {
            assert_eq!(usize::from(hook.handler), i);
            assert_eq!(hook.section, ".flash.text");
            assert!(hook.variants.iter().any(|v| v.builds.contains("pk")));
        }
        assert!(p.calls.iter().any(|c| c == "xQueueGiveFromISR"));
        assert!(p.calls.iter().any(|c| c == "vPortYieldFromISR"));
        assert_eq!(p.data, [("btdm_controller_status".to_string(), 4)]);
        assert_eq!((p.worker.priority, p.worker.min_stack), (23, 4096));
        assert_eq!(p.worker.max_scratch, 264);
        assert!(super::super::vhci::MAX_H4.next_multiple_of(4) <= u32::from(p.worker.max_scratch));
    }

    #[test]
    fn the_interrupt_source_is_ets_rwble_intr_source_as_the_campaign_capture_reads() {
        // Source 8 is `ETS_RWBLE_INTR_SOURCE`, the one BT source the real controller routes (class
        // A); the default U4 wake mode raises exactly this source.
        let p = BleProfile::load();
        assert_eq!(
            pemu_core::irq_source::IrqSource(p.worker.isr_source),
            pemu_core::irq_source::irq::RWBLE
        );
        assert!(
            BLE_TOML.contains(
                "`device-probe_campaign_radio-20260924T155729Z-run1.log` L64 (after init)"
            )
        );
        assert_eq!(
            pemu_hle::worker::WakeMode::default(),
            pemu_hle::worker::WakeMode::U4MagicIsr
        );
    }

    #[test]
    fn a_profile_row_that_would_bind_by_guessing_is_refused() {
        let zeros = "0".repeat(128);
        let base = format!(
            "[profile]\nidf = \"5.5.3\"\nmodule = \"ble\"\n[worker]\ntask_name = \"t\"\n\
             priority = 23\nmin_stack = 4096\npoll_ticks = 20\nmax_scratch = 264\n\
             isr_source = 8\n[controller]\nhci_version = 9\nhci_subversion = 0\n\
             lmp_version = 9\ncompany = 741\nlmp_subversion = 0\n\
             supported_commands = \"{zeros}\"\nlmp_features = \"0000000060000000\"\n\
             le_features = \"0x1\"\nsupported_states = \"0000000000000000\"\n\
             acl_len = 27\nacl_count = 1\naccept_list_size = 1\nresolving_list_size = 1\n\
             adv_tx_power = \"-3\"\ntx_power_min = \"-24\"\ntx_power_max = \"21\"\n\
             max_tx_octets = 27\nmax_tx_time = 328\nmax_adv_data_len = 31\nadv_sets = 1\n\
             num_command_packets = 5\nreply_us = 1\nreset_reply_us = 2\n"
        );
        let base = base.as_str();
        let short_map = base.replace(&zeros, "00");
        assert!(
            BleProfile::parse(&short_map)
                .unwrap_err()
                .contains("hex digits")
        );
        let bad_answer = format!(
            "{base}[[command]]\nopcode = \"0x0C03\"\nname = \"x\"\nsupported = \"5.7\"\n\
             answer = \"maybe\"\n"
        );
        assert!(
            BleProfile::parse(&bad_answer)
                .unwrap_err()
                .contains("answer")
        );
        assert_eq!(BleProfile::parse(base).unwrap().controller.adv_tx_power, -3);
        assert!(BleProfile::parse(base).is_ok());
        let no_controller = base.split("[controller]").next().unwrap_or_default();
        assert!(
            BleProfile::parse(no_controller)
                .unwrap_err()
                .contains("[controller]")
        );
        let bad_op = format!(
            "{base}[[command]]\nopcode = \"0x1\"\nname = \"x\"\nsupported = \"none\"\nanswer = \"modeled\"\n"
        );
        assert!(BleProfile::parse(&bad_op).unwrap_err().contains("hex"));
        let bad_bit = format!(
            "{base}[[command]]\nopcode = \"0x0C03\"\nname = \"x\"\nsupported = \"5.8\"\nanswer = \"modeled\"\n"
        );
        assert!(
            BleProfile::parse(&bad_bit)
                .unwrap_err()
                .contains("octet.bit")
        );
        let twice = format!(
            "{base}[[command]]\nopcode = \"0x0C03\"\nname = \"a\"\nsupported = \"5.7\"\nanswer = \"modeled\"\n\
             [[command]]\nopcode = \"0x0C01\"\nname = \"b\"\nsupported = \"5.7\"\nanswer = \"modeled\"\n"
        );
        assert!(BleProfile::parse(&twice).unwrap_err().contains("repeats"));
        let no_variant = format!("{base}[[hook]]\nname = \"f\"\nhandler = 0\nsection = \".t\"\n");
        assert!(
            BleProfile::parse(&no_variant)
                .unwrap_err()
                .contains("no [[variant]]")
        );
        let short =
            format!("{no_variant}[[variant]]\nbuilds = \"x\"\nsize = 4\ncode_sha256 = \"ab\"\n");
        assert!(BleProfile::parse(&short).unwrap_err().contains("64 hex"));
        // 64 bytes of text that are not 64 hex digits: a multi-byte character must not panic.
        for bad in [format!("é{}", "0".repeat(62)), "g".repeat(64)] {
            assert!(digest(&bad).unwrap_err().contains("64 hex"), "{bad}");
        }
        assert_eq!(digest(&"aB".repeat(32)), Ok([0xAB; 32]));
        let unknown = format!("{base}[[hooks]]\nname = \"f\"\n");
        assert!(
            BleProfile::parse(&unknown)
                .unwrap_err()
                .contains("unknown table")
        );
        let no_size = format!(
            "{no_variant}[[variant]]\nbuilds = \"x\"\ncode_sha256 = \"{}\"\n",
            "0".repeat(64)
        );
        assert!(BleProfile::parse(&no_size).unwrap_err().contains("size"));
    }

    #[test]
    fn the_log_lines_are_the_four_ble_init_lines_and_the_phy_line() {
        let lines = LogLines::load();
        assert_eq!(lines.info_format, "I (%lu) %s: %s\n");
        let init: Vec<&str> = lines
            .stage(Stage::Init, true)
            .map(|l| l.tag.as_str())
            .collect();
        assert_eq!(init, ["BLE_INIT"; 4]);
        let unverified: Vec<&str> = lines
            .stage(Stage::Init, false)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            unverified,
            [
                "BT controller compile version [1bb2f50]",
                "Bluetooth MAC: {mac}"
            ]
        );
        let p = BleProfile::load();
        let verified: Vec<&str> = p.hooks[0]
            .variants
            .iter()
            .filter(|v| v.log_lines_verified)
            .map(|v| v.builds.as_str())
            .collect();
        assert_eq!(verified, ["pk,pkgatt"], "only pk's shape is verified");
        assert!(
            p.hooks[1..]
                .iter()
                .all(|h| h.variants.iter().all(|v| !v.log_lines_verified))
        );
        let enable: Vec<&LogLine> = lines.stage(Stage::Enable, false).collect();
        assert_eq!(enable.len(), 1);
        assert_eq!(enable[0].tag, "phy_init");
        assert!(enable[0].text.starts_with("phy_version 1232,"));
        assert!(LogLines::parse("[format]\ninfo = \"%s %lu %s\"\n").is_err());
    }
}
