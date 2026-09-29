//! The JSON `pemu_new` reads. `MachineConfig` has no serde form, so this module sets the fields a
//! browser session and the determinism harness use, by name, onto `MachineConfig::default()`. An
//! unknown key is refused (`E_USAGE` at `pemu_build`), so a typo is not a silent default boot.
//!
//! | Key | Type | Sets |
//! |---|---|---|
//! | `fw` | string | the firmware display name; the image arrives through `pemu_load` |
//! | `label` | string | the session label `status` shows |
//! | `seed` | integer | `MachineConfig::seed` |
//! | `profile` | `"fast"` or `"device"` | `MachineConfig::profile` |
//! | `poll_ff` | bool | `MachineConfig::poll_ff` |
//! | `max_block_insns` | integer 1..=65535 | `EngineCfg::max_block_insns` |
//! | `trace` | bool | `TraceCfg::all()` or none |
//! | `hang` | `{enabled, stuck_ms}` | `MachineConfig::hang` |
//! | `disabled_models` | array of strings | `MachineConfig::disabled_models` |
//! | `executor` | `"engine"` or `"reference"` | `Machine::set_executor` |
//! | `max_slice` | integer >= 1 | `Machine::set_max_slice` |
//! | `rom_delay_ff` | bool | `Machine::set_rom_delay_ff` |
//!
//! The last three are machine setters that change no result, which is why the Node leg of the
//! determinism harness varies them.

use pemu_machine::Executor;
use pemu_machine::config::{MachineConfig, TimingProfileId, TraceCfg};
use pemu_machine::hang::HangCfg;
use serde_json::{Map, Value};

#[derive(Clone, Default)]
pub struct WasmConfig {
    pub machine: MachineConfig,
    /// `Machine::set_executor`, when given.
    pub executor: Option<Executor>,
    /// `Machine::set_max_slice`, when given.
    pub max_slice: Option<u64>,
    /// `Machine::set_rom_delay_ff`, when given.
    pub rom_delay_ff: Option<bool>,
    pub fw: String,
    pub label: String,
}

/// Every key [`parse`] accepts.
pub const KEYS: [&str; 12] = [
    "fw",
    "label",
    "seed",
    "profile",
    "poll_ff",
    "max_block_insns",
    "trace",
    "hang",
    "disabled_models",
    "executor",
    "max_slice",
    "rom_delay_ff",
];

fn want_bool(key: &str, value: &Value) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("`{key}` must be a boolean"))
}

fn want_u64(key: &str, value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("`{key}` must be a non-negative integer"))
}

fn want_str<'a>(key: &str, value: &'a Value) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("`{key}` must be a string"))
}

/// Reads a `pemu_new` configuration: empty bytes are the default, anything else a JSON object of
/// the keys in the module table.
pub fn parse(bytes: &[u8]) -> Result<WasmConfig, String> {
    let mut out = WasmConfig::default();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(out);
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("the config is not JSON: {e}"))?;
    let map: &Map<String, Value> = value
        .as_object()
        .ok_or_else(|| "the config must be a JSON object".to_string())?;
    for (key, value) in map {
        match key.as_str() {
            "fw" => out.fw = want_str(key, value)?.to_string(),
            "label" => out.label = want_str(key, value)?.to_string(),
            "seed" => out.machine.seed = want_u64(key, value)?,
            "profile" => {
                // The same parser as the `profile` argument of `start`.
                let text = want_str(key, value)?;
                out.machine.profile = TimingProfileId::parse(text).ok_or_else(|| {
                    format!(
                        "`profile` is `{text}`, not one of {}",
                        TimingProfileId::vocabulary()
                    )
                })?;
            }
            "poll_ff" => out.machine.poll_ff = want_bool(key, value)?,
            "max_block_insns" => {
                out.machine.engine.max_block_insns = u16::try_from(want_u64(key, value)?)
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| "`max_block_insns` must be 1 to 65535".to_string())?;
            }
            "trace" => {
                out.machine.trace = if want_bool(key, value)? {
                    TraceCfg::all()
                } else {
                    TraceCfg::default()
                }
            }
            "hang" => out.machine.hang = hang(value)?,
            "disabled_models" => {
                let items = value
                    .as_array()
                    .ok_or_else(|| "`disabled_models` must be an array of strings".to_string())?;
                out.machine.disabled_models = items
                    .iter()
                    .map(|item| want_str("disabled_models", item).map(str::to_string))
                    .collect::<Result<_, _>>()?;
            }
            "executor" => {
                out.executor = Some(match want_str(key, value)? {
                    "engine" => Executor::Engine,
                    "reference" => Executor::Reference,
                    other => {
                        return Err(format!(
                            "`executor` is `{other}`, not `engine` or `reference`"
                        ));
                    }
                })
            }
            "max_slice" => {
                let n = want_u64(key, value)?;
                if n == 0 {
                    return Err("`max_slice` must be at least 1".to_string());
                }
                out.max_slice = Some(n);
            }
            "rom_delay_ff" => out.rom_delay_ff = Some(want_bool(key, value)?),
            other => {
                return Err(format!(
                    "`{other}` is not a config key; expected one of {KEYS:?}"
                ));
            }
        }
    }
    Ok(out)
}

fn hang(value: &Value) -> Result<HangCfg, String> {
    let map = value
        .as_object()
        .ok_or_else(|| "`hang` must be an object `{enabled, stuck_ms}`".to_string())?;
    let mut cfg = HangCfg::default();
    for (key, value) in map {
        match key.as_str() {
            "enabled" => cfg.enabled = want_bool("hang.enabled", value)?,
            "stuck_ms" => cfg.stuck_ms = want_u64("hang.stuck_ms", value)?,
            other => {
                return Err(format!(
                    "`hang.{other}` is not a key; expected `enabled`, `stuck_ms`"
                ));
            }
        }
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bytes_are_the_default_configuration() {
        let cfg = parse(b"").unwrap();
        let def = MachineConfig::default();
        assert_eq!(cfg.machine.seed, def.seed);
        assert_eq!(cfg.machine.poll_ff, def.poll_ff);
        assert!(cfg.executor.is_none() && cfg.max_slice.is_none() && cfg.rom_delay_ff.is_none());
    }

    #[test]
    fn every_key_lands_on_its_field() {
        let cfg = parse(
            br#"{"fw":"official","label":"b","seed":7,"profile":"device","poll_ff":false,
                 "max_block_insns":3,"trace":true,"hang":{"enabled":false,"stuck_ms":9},
                 "disabled_models":["twai"],"executor":"reference","max_slice":1000,
                 "rom_delay_ff":false}"#,
        )
        .unwrap();
        assert_eq!(cfg.fw, "official");
        assert_eq!(cfg.label, "b");
        assert_eq!(cfg.machine.seed, 7);
        assert_eq!(cfg.machine.profile, TimingProfileId::Device);
        assert!(!cfg.machine.poll_ff);
        assert_eq!(cfg.machine.engine.max_block_insns, 3);
        assert!(cfg.machine.trace.kinds.is_some());
        assert!(!cfg.machine.hang.enabled);
        assert_eq!(cfg.machine.hang.stuck_ms, 9);
        assert_eq!(cfg.machine.disabled_models, vec!["twai".to_string()]);
        assert_eq!(cfg.executor, Some(Executor::Reference));
        assert_eq!(cfg.max_slice, Some(1000));
        assert_eq!(cfg.rom_delay_ff, Some(false));
    }

    #[test]
    fn a_typo_or_a_bad_value_is_refused_with_its_name() {
        assert!(
            parse(br#"{"sed":1}"#)
                .err()
                .expect("refused")
                .contains("`sed`")
        );
        assert!(
            parse(br#"{"profile":"slow"}"#)
                .err()
                .expect("refused")
                .contains("slow")
        );
        assert!(parse(br#"{"max_block_insns":0}"#).is_err());
        assert!(parse(br#"{"max_slice":0}"#).is_err());
        assert!(parse(br#"[1]"#).is_err());
        assert!(parse(b"{").is_err());
    }
}
