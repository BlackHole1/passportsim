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
    /// `{mac}` is replaced by the guest's BT or station MAC and `{task}` by the Wi-Fi worker's task
    /// handle.
    pub text: String,
    /// True for a line whose text one sdkconfig decides (`config = "pk"` for BLE, `"corpus"` for
    /// Wi-Fi): printed only for a verified shape.
    pub sdkconfig: bool,
    /// True for a line the blob prints with no space after the tag (`wifi:wifi driver task: ...`).
    pub bare: bool,
    pub error: bool,
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
                    lines.push(LogLine {
                        module: table.string("module").unwrap_or("ble").to_string(),
                        stage: match need(table, "stage")? {
                            "init" => Stage::Init,
                            "enable" => Stage::Enable,
                            "start" => Stage::Start,
                            "stop" => Stage::Stop,
                            "deinit" => Stage::Deinit,
                            "deinit_refused" => Stage::DeinitRefused,
                            other => return Err(format!("unknown stage `{other}`")),
                        },
                        tag: need(table, "tag")?.to_string(),
                        text: need(table, "text")?.to_string(),
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
