//! The Wi-Fi binding profile, compiled in from `specs/hle/idf-5.5.3/wifi.toml`. The shape is
//! `ble.toml`'s with a `[driver]` table in place of `[controller]`, parsed as strictly.

use pemu_loader::bundle::{TomlLite, TomlTable};

use crate::heap_ledger::{CountClass, LedgerRow, Lifetime};
pub use crate::hle_common::{HookRow, Variant};
use crate::hle_common::{ProfileRows, Rows, need, need_u32, unknown_table};

pub const WIFI_TOML: &str = include_str!("../../../../specs/hle/idf-5.5.3/wifi.toml");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRow {
    pub task_name: String,
    /// `configMAX_PRIORITIES - 2`.
    pub priority: u32,
    /// The stack the driver's own formula gives for the corpus sdkconfig.
    pub min_stack: u32,
    /// Poll interval in milliseconds of virtual time, whatever the guest's tick rate.
    pub poll_ms: u32,
    pub max_scratch: u16,
    /// Interrupt-matrix source of the magic ISR (UNVERIFIED for the HLE worker).
    pub isr_source: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverRow {
    pub init_config_bytes: u32,
    pub init_config_magic: u32,
    pub init_config_magic_off: u32,
    /// Offset of `int dynamic_rx_buf_num` in `wifi_init_config_t`: the guest's own
    /// `DYNAMIC_RX_BUFFER_NUM`.
    pub init_config_dynamic_rx_buf_num_off: u32,
    pub ap_record_bytes: u32,
    pub scan_done_bytes: u32,
    /// Channels an active scan visits with the default country.
    pub scan_channels: u32,
    /// Milliseconds.
    pub dwell_ms: u32,
    /// Channels the sweep visits passively after the active ones (12, 13 and 14).
    pub passive_channels: u32,
    /// Milliseconds.
    pub passive_dwell_ms: u32,
    /// Measured scan time minus the sum of the dwells, in milliseconds.
    pub scan_overhead_ms: u32,
    /// How long `esp_wifi_start` takes before its events, in milliseconds.
    pub start_ms: u32,
    /// The event the device posts immediately before `STA_START` (id only).
    pub event_start_first: u32,
    pub event_scan_done: u32,
    pub event_sta_start: u32,
    pub event_sta_stop: u32,
    pub event_sta_connected: u32,
    pub event_sta_disconnected: u32,
    pub disconnected_bytes: u32,
    pub disconnected_ssid_len_off: u32,
    /// The field `probe2`'s handler prints.
    pub disconnected_reason_off: u32,
    pub disconnected_rssi_off: u32,
    /// How long `esp_wifi_connect` at an SSID no access point answers takes before its disconnects,
    /// in milliseconds (class A).
    pub connect_ms: u32,
    /// The reason of the first of the two disconnects.
    pub reason_no_ap_found: u32,
    /// The reason of the second one.
    pub reason_after_no_ap_found: u32,
    pub disconnected_bssid_off: u32,
    /// From the return of `esp_wifi_connect` at a served SSID to `STA_CONNECTED`, in microseconds
    /// (class A).
    pub connected_us: u32,
    pub connected_bytes: u32,
    pub connected_ssid_len_off: u32,
    pub connected_bssid_off: u32,
    pub connected_channel_off: u32,
    /// 4 bytes.
    pub connected_authmode_off: u32,
    /// 2 bytes.
    pub connected_aid_off: u32,
    /// The `aid` a scripted access point gives (class C).
    pub connected_aid: u32,
    /// The reason of the one disconnect `esp_wifi_disconnect` gives an associated station.
    pub reason_assoc_leave: u32,
    pub sta_config_password_off: u32,
    pub sta_config_bssid_set_off: u32,
    pub sta_config_channel_off: u32,
    pub sta_config_threshold_authmode_off: u32,
}

impl DriverRow {
    /// An undisturbed scan in microseconds: active channels at their dwell, passive ones at theirs,
    /// plus the measured overhead (class A).
    pub fn scan_us(&self) -> u64 {
        (u64::from(self.scan_channels) * u64::from(self.dwell_ms)
            + u64::from(self.passive_channels) * u64::from(self.passive_dwell_ms)
            + u64::from(self.scan_overhead_ms))
            * 1_000
    }

    pub fn start_us(&self) -> u64 {
        u64::from(self.start_ms) * 1_000
    }

    pub fn connect_us(&self) -> u64 {
        u64::from(self.connect_ms) * 1_000
    }

    pub fn associate_us(&self) -> u64 {
        u64::from(self.connected_us)
    }

    /// The highest channel the sweep has finished `elapsed_us` into a scan, which is what a partial
    /// result holds; 0 before the first channel ends.
    pub fn swept_channels(&self, elapsed_us: u64) -> u32 {
        let active_us = u64::from(self.dwell_ms) * 1_000;
        let passive_us = u64::from(self.passive_dwell_ms) * 1_000;
        let active = (elapsed_us / active_us.max(1)).min(u64::from(self.scan_channels));
        if active < u64::from(self.scan_channels) {
            return active as u32;
        }
        let rest = elapsed_us - u64::from(self.scan_channels) * active_us;
        let passive = (rest / passive_us.max(1)).min(u64::from(self.passive_channels));
        (u64::from(self.scan_channels) + passive) as u32
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiProfile {
    pub idf: String,
    pub module: String,
    pub hooks: Vec<HookRow>,
    pub calls: Vec<String>,
    pub data: Vec<(String, u32)>,
    pub worker: WorkerRow,
    pub driver: DriverRow,
    pub heap: Vec<LedgerRow>,
}

fn hex_u32(table: &TomlTable, key: &str) -> Result<u32, String> {
    let text = need(table, key)?;
    text.strip_prefix("0x")
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .ok_or_else(|| format!("`{key}` is not 0x and hex digits"))
}

fn driver_row(table: &TomlTable) -> Result<DriverRow, String> {
    Ok(DriverRow {
        init_config_bytes: need_u32(table, "init_config_bytes")?,
        init_config_magic: hex_u32(table, "init_config_magic")?,
        init_config_magic_off: need_u32(table, "init_config_magic_off")?,
        init_config_dynamic_rx_buf_num_off: need_u32(table, "init_config_dynamic_rx_buf_num_off")?,
        ap_record_bytes: need_u32(table, "ap_record_bytes")?,
        scan_done_bytes: need_u32(table, "scan_done_bytes")?,
        scan_channels: need_u32(table, "scan_channels")?,
        dwell_ms: need_u32(table, "dwell_ms")?,
        passive_channels: need_u32(table, "passive_channels")?,
        passive_dwell_ms: need_u32(table, "passive_dwell_ms")?,
        scan_overhead_ms: need_u32(table, "scan_overhead_ms")?,
        start_ms: need_u32(table, "start_ms")?,
        event_start_first: need_u32(table, "event_start_first")?,
        event_scan_done: need_u32(table, "event_scan_done")?,
        event_sta_start: need_u32(table, "event_sta_start")?,
        event_sta_stop: need_u32(table, "event_sta_stop")?,
        event_sta_connected: need_u32(table, "event_sta_connected")?,
        event_sta_disconnected: need_u32(table, "event_sta_disconnected")?,
        disconnected_bytes: need_u32(table, "disconnected_bytes")?,
        disconnected_ssid_len_off: need_u32(table, "disconnected_ssid_len_off")?,
        disconnected_reason_off: need_u32(table, "disconnected_reason_off")?,
        disconnected_rssi_off: need_u32(table, "disconnected_rssi_off")?,
        connect_ms: need_u32(table, "connect_ms")?,
        reason_no_ap_found: need_u32(table, "reason_no_ap_found")?,
        reason_after_no_ap_found: need_u32(table, "reason_after_no_ap_found")?,
        disconnected_bssid_off: need_u32(table, "disconnected_bssid_off")?,
        connected_us: need_u32(table, "connected_us")?,
        connected_bytes: need_u32(table, "connected_bytes")?,
        connected_ssid_len_off: need_u32(table, "connected_ssid_len_off")?,
        connected_bssid_off: need_u32(table, "connected_bssid_off")?,
        connected_channel_off: need_u32(table, "connected_channel_off")?,
        connected_authmode_off: need_u32(table, "connected_authmode_off")?,
        connected_aid_off: need_u32(table, "connected_aid_off")?,
        connected_aid: need_u32(table, "connected_aid")?,
        reason_assoc_leave: need_u32(table, "reason_assoc_leave")?,
        sta_config_password_off: need_u32(table, "sta_config_password_off")?,
        sta_config_bssid_set_off: need_u32(table, "sta_config_bssid_set_off")?,
        sta_config_channel_off: need_u32(table, "sta_config_channel_off")?,
        sta_config_threshold_authmode_off: need_u32(table, "sta_config_threshold_authmode_off")?,
    })
}

impl WifiProfile {
    /// Parsed once. This crate's tests parse the same text, so only an edited spec file with the
    /// tests not run can make this panic.
    pub fn load() -> &'static WifiProfile {
        static PROFILE: std::sync::OnceLock<WifiProfile> = std::sync::OnceLock::new();
        PROFILE.get_or_init(|| {
            WifiProfile::parse(WIFI_TOML)
                .unwrap_or_else(|e| panic!("specs/hle/idf-5.5.3/wifi.toml: {e}"))
        })
    }

    pub fn parse(text: &str) -> Result<WifiProfile, String> {
        let doc = TomlLite::parse(text);
        let mut rows = ProfileRows::default();
        let mut worker = None;
        let mut driver = None;
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
                        poll_ms: need_u32(table, "poll_ms")?,
                        max_scratch: u16::try_from(need_u32(table, "max_scratch")?)
                            .map_err(|_| "a max_scratch above 65535".to_string())?,
                        isr_source: u8::try_from(need_u32(table, "isr_source")?)
                            .map_err(|_| "an interrupt source above 255".to_string())?,
                    })
                }
                ("driver", false) => driver = Some(driver_row(table)?),
                ("heap", true) => {
                    let row = LedgerRow {
                        label: need(table, "label")?.to_string(),
                        count: need_u32(table, "count")?,
                        element: need_u32(table, "element")?,
                        count_class: match need(table, "count_class")? {
                            "capture" => CountClass::Capture,
                            "sdkconfig" => CountClass::Sdkconfig,
                            other => return Err(format!("unknown count_class `{other}`")),
                        },
                        count_source: need(table, "count_source")?.to_string(),
                        element_source: need(table, "element_source")?.to_string(),
                        config: match table.string("config") {
                            None => None,
                            Some("corpus") => Some("corpus".to_string()),
                            // The count is read from each guest's own init config, so the row
                            // applies to every shape.
                            Some("guest") => Some("guest".to_string()),
                            Some(other) => return Err(format!("unknown heap config `{other}`")),
                        },
                        lifetime: Lifetime::Init,
                    };
                    if row.count == 0 || row.element == 0 {
                        return Err(format!("heap row `{}` would allocate nothing", row.label));
                    }
                    if row.count_class == CountClass::Sdkconfig && row.config.is_none() {
                        return Err(format!(
                            "heap row `{}` reads a sdkconfig but names no build shape",
                            row.label
                        ));
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
        Ok(WifiProfile {
            idf,
            module,
            hooks,
            calls,
            data,
            worker: worker.ok_or("no [worker]")?,
            driver: driver.ok_or("no [driver]")?,
            heap,
        })
    }

    /// The ledger rows for an image whose bound `esp_wifi_init` matched a variant with
    /// `log_lines = "verified"` (`verified`), in file order.
    ///
    /// A `config = "corpus"` row's `count` is one build's sdkconfig value, so it applies only to an
    /// image of that shape; a `config = "guest"` row reads its count from the guest at run time and
    /// applies to every image.
    pub fn heap_plan(&self, verified: bool) -> Vec<&LedgerRow> {
        self.heap
            .iter()
            .filter(|row| verified || row.config.as_deref() != Some("corpus"))
            .collect()
    }

    pub fn hook(&self, handler: u16) -> Option<&HookRow> {
        self.hooks.iter().find(|h| h.handler == handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row is `count * element` and nothing else, every row cites both, and a row whose `count`
    /// is one build's sdkconfig value names the shape it applies to.
    #[test]
    fn the_heap_plan_is_one_cited_row_per_declared_capacity() {
        let p = WifiProfile::load();
        assert_eq!(
            p.heap.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            ["dynamic_rx"],
            "Wi-Fi declares exactly the RX buffer capacity"
        );
        for row in &p.heap {
            assert!(!row.count_source.is_empty(), "{} cites no count", row.label);
            assert!(
                !row.element_source.is_empty(),
                "{} cites no element",
                row.label
            );
            assert_eq!(
                row.count_class == CountClass::Sdkconfig,
                row.config.is_some(),
                "{} is a sdkconfig count exactly when it names a build shape",
                row.label
            );
            assert_eq!(row.bytes(), row.count * row.element, "{}", row.label);
        }
        let rx = &p.heap[0];
        // 32 outstanding buffers of one Ethernet II frame at the MTU each: a declared lower bound
        // (class C), not fitted to a measurement.
        assert_eq!(rx.count, 32);
        assert_eq!(rx.element, 1_514);
        assert_eq!(rx.bytes(), 48_448);
        assert_eq!(rx.count_class.class(), "C");
        // Read from the guest's own `dynamic_rx_buf_num`, so the row applies to every shape, the
        // unverified `probe_wifi_http` included.
        assert_eq!(rx.config.as_deref(), Some("guest"));
        assert_eq!(p.heap_plan(true).len(), 1);
        assert_eq!(p.heap_plan(false).len(), 1);
    }

    #[test]
    fn the_compiled_in_profile_is_the_wifi_profile() {
        let p = WifiProfile::load();
        assert_eq!(p.idf, "5.5.3");
        assert_eq!(p.module, "wifi");
        assert_eq!(p.hooks.len(), 21);
        assert_eq!(p.hook(0).map(|h| h.name.as_str()), Some("esp_wifi_init"));
        assert_eq!(
            p.hook(13).map(|h| h.name.as_str()),
            Some("esp_wifi_scan_get_ap_records")
        );
        assert!(p.hooks.iter().all(|h| h.section == ".flash.text"));
        assert!(p.hooks.iter().all(|h| !h.variants.is_empty()));
        assert_eq!(
            p.hook(16).map(|h| h.name.as_str()),
            Some("esp_wifi_internal_free_rx_buffer")
        );
        // Only `probe2` links the association hooks, so the other Wi-Fi corpus builds skip both
        // rows.
        assert_eq!(
            p.hook(17).map(|h| h.name.as_str()),
            Some("esp_wifi_connect_internal")
        );
        assert_eq!(
            p.hook(18).map(|h| h.name.as_str()),
            Some("esp_wifi_disconnect_internal")
        );
        for handler in [17, 18] {
            let hook = p
                .hook(handler)
                .expect("the association hook is in the profile");
            assert_eq!(hook.variants.len(), 1, "{}", hook.name);
            assert_eq!(hook.variants[0].size, 88, "{}", hook.name);
        }
        assert_eq!(
            p.hook(17).unwrap().variants[0].builds,
            "probe2,probe_wifi_http"
        );
        assert_eq!(p.hook(18).unwrap().variants[0].builds, "probe2");
        assert_eq!(
            p.hook(19).map(|h| h.name.as_str()),
            Some("esp_wifi_internal_tx")
        );
        assert_eq!(p.hook(19).unwrap().variants.len(), 1);
        assert_eq!(p.hook(19).unwrap().variants[0].size, 50);
        assert_eq!(
            p.hook(20).map(|h| h.name.as_str()),
            Some("esp_wifi_internal_set_sta_ip")
        );
        // One body in the archive, so one variant however many builds link it.
        assert_eq!(p.hook(20).unwrap().variants.len(), 1);
        assert_eq!(p.hook(20).unwrap().variants[0].size, 52);
        assert_eq!(p.worker.task_name, "wifi");
        assert_eq!(p.worker.priority, 23);
        assert_eq!(p.worker.min_stack, 6656);
        assert_eq!(p.driver.init_config_magic, 0x1F2F_3F4F);
        assert_eq!(p.driver.ap_record_bytes, 92);
        // Class A: 11 x 120 ms active, 3 x 360 ms passive and 20 ms of overhead.
        assert_eq!(p.driver.scan_us(), 2_420_000);
        assert_eq!(p.driver.start_us(), 39_000);
        assert_eq!(p.driver.event_sta_start, 2);
        assert_eq!(p.driver.event_start_first, 43);
        // Two disconnects, 201 then 36, about one scan sweep after the connect.
        assert_eq!(p.driver.connect_us(), 2_420_000);
        assert_eq!(p.driver.connect_us(), p.driver.scan_us());
        assert_eq!(p.driver.reason_no_ap_found, 201);
        assert_eq!(p.driver.reason_after_no_ap_found, 36);
        assert_eq!(p.driver.disconnected_bytes, 41);
        assert_eq!(p.driver.disconnected_reason_off, 39);
        assert_eq!(p.driver.disconnected_rssi_off, 40);
        assert_eq!(p.driver.associate_us(), 212_415);
        assert_eq!(p.driver.connected_bytes, 48);
        assert_eq!(
            [
                p.driver.connected_ssid_len_off,
                p.driver.connected_bssid_off,
                p.driver.connected_channel_off,
                p.driver.connected_authmode_off,
                p.driver.connected_aid_off,
            ],
            [32, 33, 39, 40, 44]
        );
        assert_eq!(p.driver.reason_assoc_leave, 8);
        assert_eq!(p.driver.disconnected_bssid_off, 33);
        assert_eq!(p.driver.sta_config_threshold_authmode_off, 120);
        assert_eq!(p.driver.init_config_dynamic_rx_buf_num_off, 52);
        // Channel 1 ends at 120 ms, channel 11 at 1,320 ms, then three passive channels at 360 ms
        // each.
        assert_eq!(p.driver.swept_channels(0), 0);
        assert_eq!(p.driver.swept_channels(119_999), 0);
        assert_eq!(p.driver.swept_channels(120_000), 1);
        assert_eq!(p.driver.swept_channels(1_320_000), 11);
        assert_eq!(p.driver.swept_channels(1_680_000), 12);
        assert_eq!(p.driver.swept_channels(2_420_000), 14);
        assert!(
            p.hooks[0]
                .variants
                .iter()
                .all(|v| v.log_lines_verified == (v.size == 220)),
            "the corpus esp_wifi_init carries the verified log lines, and only it"
        );
        assert!(
            p.hooks[0]
                .variants
                .iter()
                .any(|v| v.builds == "probe_wifi_http" && !v.log_lines_verified),
            "probe_wifi_http's shape is bound without the corpus lines or capacity"
        );
        assert!(p.calls.iter().any(|c| c == "esp_event_post"));
        assert!(p.calls.iter().any(|c| c == "heap_caps_malloc"));
        assert!(p.calls.iter().any(|c| c == "heap_caps_free"));
        assert_eq!(
            p.data,
            [("s_wifi_inited".to_string(), 1), ("WIFI_EVENT".into(), 4)]
        );
    }

    #[test]
    fn every_corpus_build_that_links_a_hook_has_a_variant_of_its_own() {
        // `builds` names exactly the corpus builds that link Wi-Fi; `pk` and `pkgatt` link none.
        // `probe_wifi_http` is the one device probe measured into the profile.
        let p = WifiProfile::load();
        for hook in &p.hooks {
            for variant in &hook.variants {
                assert!(
                    variant.builds.split(',').all(|b| matches!(
                        b,
                        "scan3" | "probe2" | "demo" | "official" | "probe_wifi_http"
                    )),
                    "hook `{}` names an unknown build in `{}`",
                    hook.name,
                    variant.builds
                );
                assert!(variant.size > 0, "hook `{}` has a zero size", hook.name);
            }
        }
    }

    #[test]
    fn every_guard_is_a_function_the_tripwire_rule_arms() {
        // A guard that is hooked, open source or allowed to run would stop nothing, and the
        // image check would refuse every image that lacks the row it guards.
        let p = WifiProfile::load();
        let blob = pemu_hle::tripwire::blob_defined_set();
        let allow = pemu_hle::tripwire::TripwireSpec::load().coexistence_allow;
        let guarded: Vec<(&str, &str)> = p
            .hooks
            .iter()
            .filter_map(|h| Some((h.name.as_str(), h.guard.as_deref()?)))
            .collect();
        for (hook, guard) in &guarded {
            assert!(blob.contains(*guard), "{hook}: {guard} is not blob-defined");
            assert!(!allow.contains(*guard), "{hook}: {guard} is allowed to run");
            assert!(
                p.hooks.iter().all(|h| h.name != *guard),
                "{hook}: {guard} is hooked"
            );
        }
        let unguarded: Vec<&str> = p
            .hooks
            .iter()
            .filter(|h| h.guard.is_none())
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(
            unguarded,
            [
                "esp_wifi_init",
                "esp_wifi_stop",
                "esp_wifi_internal_reg_netstack_buf_cb",
                "esp_wifi_internal_free_rx_buffer",
                "esp_wifi_internal_tx",
            ]
        );
        let symbols = crate::hle_common::module_symbols(&p.hooks, &p.calls, &p.data, &[]);
        assert_eq!(symbols.guards, guarded);
        assert_eq!(
            symbols
                .guards
                .iter()
                .find(|g| g.0 == "esp_wifi_deinit")
                .map(|g| g.1),
            Some("esp_wifi_get_user_init_flag_internal")
        );
        assert_eq!(
            symbols
                .guards
                .iter()
                .filter(|g| g.1 == "wifi_init_completed")
                .count(),
            15
        );
    }

    #[test]
    fn a_row_a_binding_could_be_guessed_from_is_refused() {
        let base = "[profile]\nidf = \"5.5.3\"\nmodule = \"wifi\"\n";
        let worker = "[worker]\ntask_name = \"wifi\"\npriority = 23\nmin_stack = 6656\n\
                      poll_ms = 20\nmax_scratch = 64\nisr_source = 0\n";
        let driver = "[driver]\ninit_config_bytes = 152\ninit_config_magic = \"0x1F2F3F4F\"\n\
                      init_config_magic_off = 144\ninit_config_dynamic_rx_buf_num_off = 52\nap_record_bytes = 92\nscan_done_bytes = 8\n\
                      scan_channels = 11\ndwell_ms = 120\npassive_channels = 3\n\
                      passive_dwell_ms = 360\nscan_overhead_ms = 20\nstart_ms = 39\n\
                      event_start_first = 43\nevent_scan_done = 1\n\
                      event_sta_start = 2\nevent_sta_stop = 3\nevent_sta_connected = 4\n\
                      event_sta_disconnected = 5\ndisconnected_bytes = 41\n\
                      disconnected_ssid_len_off = 32\ndisconnected_reason_off = 39\n\
                      disconnected_rssi_off = 40\nconnect_ms = 2420\n\
                      reason_no_ap_found = 201\nreason_after_no_ap_found = 36\n\
                      disconnected_bssid_off = 33\nconnected_us = 212415\n\
                      connected_bytes = 48\nconnected_ssid_len_off = 32\n\
                      connected_bssid_off = 33\nconnected_channel_off = 39\n\
                      connected_authmode_off = 40\nconnected_aid_off = 44\n\
                      connected_aid = 1\nreason_assoc_leave = 8\n\
                      sta_config_password_off = 32\nsta_config_bssid_set_off = 100\n\
                      sta_config_channel_off = 107\n\
                      sta_config_threshold_authmode_off = 120\n";
        let ok = format!("{base}{worker}{driver}");
        assert!(WifiProfile::parse(&ok).is_ok());
        assert!(
            WifiProfile::parse(&format!("{worker}{driver}")).is_err(),
            "no [profile]"
        );
        assert!(
            WifiProfile::parse(&format!("{base}{driver}")).is_err(),
            "no [worker]"
        );
        assert!(
            WifiProfile::parse(&format!("{base}{worker}")).is_err(),
            "no [driver]"
        );
        let hook = "[[hook]]\nname = \"esp_wifi_init\"\nhandler = 0\nsection = \".flash.text\"\n";
        assert!(
            WifiProfile::parse(&format!("{ok}{hook}")).is_err(),
            "a hook without a variant"
        );
        let short =
            format!("{ok}{hook}[[variant]]\nbuilds = \"x\"\nsize = 4\ncode_sha256 = \"00\"\n");
        assert!(
            WifiProfile::parse(&short).is_err(),
            "a hash that is not 64 digits"
        );
        let unknown = format!("{ok}[nonsense]\nx = \"y\"\n");
        assert!(WifiProfile::parse(&unknown).is_err(), "an unknown table");
    }
}
