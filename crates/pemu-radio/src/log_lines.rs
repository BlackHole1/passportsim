//! The synthesized log lines both radio modules print, compiled in from
//! `specs/hle/idf-5.5.3/log-lines.toml`.

use pemu_loader::bundle::TomlLite;

use crate::hle_common::need;

pub const LOG_LINES_TOML: &str = include_str!("../../../specs/hle/idf-5.5.3/log-lines.toml");

/// When a synthesized line is printed (`log-lines.toml` `stage`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// At the end of a successful `esp_bt_controller_init`, or of a successful `esp_wifi_init`.
    Init,
    /// Once per boot, at the first successful `esp_bt_controller_enable`.
    Enable,
    /// At the end of a successful `esp_wifi_start`, before its events.
    Start,
    /// Inside `esp_wifi_stop`, before its event.
    Stop,
    /// Inside a successful `esp_wifi_deinit`, before it returns.
    Deinit,
    /// Inside an `esp_wifi_deinit` the driver refuses because it is still started.
    DeinitRefused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// `ble` for a row with no `module` key.
    pub module: String,
    pub stage: Stage,
    pub tag: String,
    /// `{mac}` is replaced by the guest's BT or station MAC, `{ap_mac}` by its SoftAP MAC and
    /// `{task}` by the Wi-Fi worker's task handle.
    pub text: String,
    /// The `wifi_mode_t` values the line is printed in, one bit each: the `mode` of a Wi-Fi `start`
    /// row, every mode for any other row.
    pub modes: u8,
    /// True for a line whose text one sdkconfig decides (`config = "pk"` for BLE, `"corpus"` for
    /// Wi-Fi): printed only for a verified shape.
    pub sdkconfig: bool,
    /// True for a line the blob prints with no space after the tag (`wifi:wifi driver task: ...`).
    pub bare: bool,
    pub error: bool,
}

impl LogLine {
    pub fn printed_in(&self, mode: u32) -> bool {
        mode < 8 && self.modes & (1 << mode) != 0
    }
}

/// `WIFI_MODE_NULL` to `WIFI_MODE_APSTA`.
const EVERY_MODE: u8 = 0b1111;
/// `WIFI_MODE_AP` and `WIFI_MODE_APSTA`, the modes with a SoftAP MAC to print.
const AP_MODES: u8 = 0b1100;

/// `mode = "sta, apsta"`: the modes a Wi-Fi `start` row is printed in.
fn modes_of(names: &str) -> Result<u8, String> {
    let mut modes = 0;
    for name in names.split(',') {
        modes |= 1
            << match name.trim() {
                "sta" => 1,
                "ap" => 2,
                "apsta" => 3,
                other => return Err(format!("unknown line mode `{other}`")),
            };
    }
    Ok(modes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLines {
    /// The log v1 INFO format, with `%lu`, `%s`, `%s`.
    pub info_format: String,
    /// The INFO format with no space after the tag, as the Wi-Fi blob prints.
    pub info_bare_format: String,
    pub error_format: String,
    pub lines: Vec<LogLine>,
}

impl LogLines {
    pub fn stage(&self, stage: Stage, verified: bool) -> impl Iterator<Item = &LogLine> + '_ {
        self.module_stage("ble", stage, verified)
    }

    /// The sdkconfig-dependent lines appear only when `verified`.
    pub fn module_stage(
        &self,
        module: &str,
        stage: Stage,
        verified: bool,
    ) -> impl Iterator<Item = &LogLine> + '_ {
        let module = module.to_string();
        self.lines
            .iter()
            .filter(move |l| l.module == module && l.stage == stage && (verified || !l.sdkconfig))
    }

    pub fn format_of(&self, line: &LogLine) -> &str {
        match (line.error, line.bare) {
            (true, _) => &self.error_format,
            (false, true) => &self.info_bare_format,
            (false, false) => &self.info_format,
        }
    }
}

impl LogLines {
    /// Parsed once. This crate's tests parse the same text, so only an edited spec file with the
    /// tests not run can make this panic.
    pub fn load() -> &'static LogLines {
        static LINES: std::sync::OnceLock<LogLines> = std::sync::OnceLock::new();
        LINES.get_or_init(|| {
            LogLines::parse(LOG_LINES_TOML)
                .unwrap_or_else(|e| panic!("specs/hle/idf-5.5.3/log-lines.toml: {e}"))
        })
    }

    /// The only escape a string may hold is `\n`.
    pub fn parse(text: &str) -> Result<LogLines, String> {
        let doc = TomlLite::parse(text);
        let mut info_format = None;
        let mut bare_format = None;
        let mut error_format = None;
        let mut lines = Vec::new();
        for table in doc.tables() {
            match (table.name.as_str(), table.array) {
                ("format", false) => {
                    for (key, slot) in [
                        ("info", &mut info_format),
                        ("info_bare", &mut bare_format),
                        ("error", &mut error_format),
                    ] {
                        if let Some(text) = table.string(key) {
                            let format = text.replace("\\n", "\n");
                            pemu_hle::log_synth::check_format(&format)?;
                            *slot = Some(format);
                        }
                    }
                }
                ("line", true) => {
                    let level = need(table, "level")?;
                    if level != "info" && level != "error" {
                        return Err("only info and error lines are synthesized".to_string());
                    }
                    need(table, "class")?;
                    let module = table.string("module").unwrap_or("ble");
                    let stage = match need(table, "stage")? {
                        "init" => Stage::Init,
                        "enable" => Stage::Enable,
                        "start" => Stage::Start,
                        "stop" => Stage::Stop,
                        "deinit" => Stage::Deinit,
                        "deinit_refused" => Stage::DeinitRefused,
                        other => return Err(format!("unknown stage `{other}`")),
                    };
                    let text = need(table, "text")?;
                    // A `start` row with no `mode` would be printed whatever the guest set, which
                    // is how a station line came to be printed for a SoftAP.
                    let modes = match (
                        module == "wifi" && stage == Stage::Start,
                        table.string("mode"),
                    ) {
                        (true, Some(names)) => modes_of(names)?,
                        (true, None) => {
                            return Err(format!("the wifi start line `{text}` names no mode"));
                        }
                        (false, Some(_)) => {
                            return Err(format!("only a wifi start line has a mode, not `{text}`"));
                        }
                        (false, None) => EVERY_MODE,
                    };
                    if text.contains("{ap_mac}") && modes & !AP_MODES != 0 {
                        return Err(format!(
                            "`{text}` prints a SoftAP MAC in a mode with no SoftAP"
                        ));
                    }
                    lines.push(LogLine {
                        module: module.to_string(),
                        stage,
                        tag: need(table, "tag")?.to_string(),
                        text: text.to_string(),
                        modes,
                        sdkconfig: match table.string("config") {
                            None => false,
                            Some("pk" | "corpus") => true,
                            Some(other) => return Err(format!("unknown line config `{other}`")),
                        },
                        bare: match table.string("format") {
                            None | Some("info") => false,
                            Some("info_bare") => true,
                            Some(other) => return Err(format!("unknown line format `{other}`")),
                        },
                        error: level == "error",
                    });
                }
                (name, _) => return Err(format!("unknown table `{name}`")),
            }
        }
        Ok(LogLines {
            info_format: info_format.ok_or("no [format] info")?,
            info_bare_format: bare_format.ok_or("no [format] info_bare")?,
            error_format: error_format.ok_or("no [format] error")?,
            lines,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATS: &str = "[format]\ninfo = \"I (%lu) %s: %s\\n\"\ninfo_bare = \"I (%lu) %s:%s\\n\"\n\
                           error = \"E (%lu) %s: %s\\n\"\n";

    fn row(extra: &str, text: &str) -> String {
        format!(
            "{FORMATS}[[line]]\nmodule = \"wifi\"\nstage = \"start\"\n{extra}level = \"info\"\n\
             tag = \"wifi\"\nclass = \"blob\"\ntext = \"{text}\"\n"
        )
    }

    #[test]
    fn every_wifi_start_line_of_the_spec_names_its_modes() {
        let lines = LogLines::parse(LOG_LINES_TOML).expect("the spec parses");
        let start = |mode: u32| -> Vec<&str> {
            lines
                .module_stage("wifi", Stage::Start, true)
                .filter(|l| l.printed_in(mode))
                .map(|l| l.text.as_str())
                .collect()
        };
        let softap = [
            "Total power save buffer number: 16",
            "Init max length of beacon: 752/752",
            "Init max length of beacon: 752/752",
        ];
        assert_eq!(start(0), [""; 0], "a mode of 0 is NULL or never set");
        assert_eq!(start(1), ["mode : sta ({mac})", "enable tsf"]);
        assert_eq!(
            start(2),
            [&["mode : softAP ({ap_mac})"][..], &softap].concat()
        );
        assert_eq!(
            start(3),
            [
                &["mode : sta ({mac}) + softAP ({ap_mac})", "enable tsf"][..],
                &softap
            ]
            .concat()
        );
        assert!(
            lines
                .lines
                .iter()
                .filter(|l| l.stage != Stage::Start)
                .all(|l| (0..4).all(|mode| l.printed_in(mode))),
            "no other stage depends on the mode"
        );
    }

    #[test]
    fn a_wifi_start_line_with_no_mode_is_refused() {
        let err = LogLines::parse(&row("", "mode : sta ({mac})")).unwrap_err();
        assert!(err.contains("names no mode"), "{err}");
        let err = LogLines::parse(&row("mode = \"softap\"\n", "x")).unwrap_err();
        assert!(err.contains("unknown line mode `softap`"), "{err}");
    }

    #[test]
    fn a_softap_mac_in_a_station_only_line_is_refused() {
        let err =
            LogLines::parse(&row("mode = \"sta, ap\"\n", "mode : softAP ({ap_mac})")).unwrap_err();
        assert!(err.contains("no SoftAP"), "{err}");
        assert!(
            LogLines::parse(&row("mode = \"ap, apsta\"\n", "mode : softAP ({ap_mac})")).is_ok()
        );
    }
}
