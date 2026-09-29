//! `plan_flash`: the offline half of the flash-to-device planner, and the seam the whole Device
//! group answers through.
//!
//! `pemu-api` may not name `pemu-planner`, so the Device commands declare a [`DevicePlanner`] of
//! plain JSON in and out, a host fills it ([`install`]), and this crate keeps what it can check
//! alone: the argument shape, the offline label, the text, the receipt and every refusal that needs
//! no planner.
//!
//! No planner is installed unless the host was started with `--allow-device`; a call without one is
//! `E_PLAN_REFUSED` naming the flag. A plan made without device facts has not run the identity,
//! device-table and `nvs` rules, so it is labelled "offline, device checks pending" and carries
//! `device_checks: "pending"`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::error::{
    ApiError, E_CARDID_CHANGED, E_DEVICE_BUSY, E_HOST_UNSUPPORTED, E_PLAN_REFUSED, E_USAGE,
};
use crate::output::Output;
use crate::receipt::Receipt;
use crate::registry::command;
use crate::spec::{HandlerCx, Schema};

/// Names no host path beyond the image and the port a person typed; the planner resolves the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceRequest {
    /// A merged `.bin`, or a build directory with `flasher_args.json`.
    pub image: String,
    /// Must equal a discovered 303A:1001 port.
    pub port: Option<String>,
    /// CLI only, never reachable over MCP.
    pub erase_nvs: bool,
    /// Plan, rehearse and report, and open no device.
    pub dry_run: bool,
    /// The full 8 MB backup, CLI only.
    pub backup: bool,
    /// Only `device_boot_check` declares and acts on it; to the flash commands it is an unknown
    /// field. Not CLI-only: a person confirms the restart in words either way.
    pub reset: bool,
}

/// The host half: `pemu-planner`, the esptool session, the backup store and the confirmation
/// transports. Every method returns JSON; this crate renders the text and stamps the receipt, so
/// what an agent reads is decided and tested here.
pub trait DevicePlanner: Send {
    /// Offline: the write list, the refusals and the plan digest. Without device facts the JSON
    /// says `device_checks: "pending"`.
    fn plan(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError>;
    /// The whole flow, or with `dry_run` everything up to the open.
    fn flash(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError>;
    /// The redacted boot console judged against the image.
    fn boot_check(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError>;
}

/// `--erase-nvs` and `--backup` are for a person on the CLI, never an agent, and a `HandlerCx`
/// carries no origin, so it is decided where the planner is installed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceCaller {
    Cli,
    /// MCP or HTTP.
    Agent,
}

/// `None`: the process was not started with `--allow-device`.
type Installed = Option<Box<dyn DevicePlanner>>;

fn slot() -> MutexGuard<'static, Installed> {
    static SLOT: OnceLock<Mutex<Installed>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Outside the planner's lock: [`with_planner`] holds that mutex while the planner runs, and the
/// host planner asks who called while it runs. A `std::sync::Mutex` is not reentrant, so reading
/// this under it would deadlock.
static CALLER: AtomicU8 = AtomicU8::new(CALLER_NONE);
const CALLER_NONE: u8 = 0;
const CALLER_CLI: u8 = 1;
const CALLER_AGENT: u8 = 2;

/// Called once, only by a host started with `--allow-device`; until then every Device command
/// refuses.
pub fn install(caller: DeviceCaller, planner: Box<dyn DevicePlanner>) {
    *slot() = Some(planner);
    CALLER.store(
        match caller {
            DeviceCaller::Cli => CALLER_CLI,
            DeviceCaller::Agent => CALLER_AGENT,
        },
        Ordering::SeqCst,
    );
}

/// For a host that shuts down, and for tests.
pub fn uninstall() {
    *slot() = None;
    CALLER.store(CALLER_NONE, Ordering::SeqCst);
}

/// Serializes every test that installs into the process-wide planner slot. It lives beside the slot
/// because two private gates would guard one global with two locks and interleave.
#[cfg(test)]
pub(crate) fn test_gate() -> MutexGuard<'static, ()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Exactly "this process was started with `--allow-device`". Reads `CALLER`, not the slot, so a
/// planner may ask while it runs.
pub fn device_allowed() -> bool {
    CALLER.load(Ordering::SeqCst) != CALLER_NONE
}

pub fn not_allowed() -> ApiError {
    ApiError::new(
        E_PLAN_REFUSED,
        "the device group is off: start the server or the CLI with `--allow-device`",
    )
}

pub fn with_planner(
    call: impl FnOnce(&mut dyn DevicePlanner) -> Result<serde_json::Value, ApiError>,
) -> Result<serde_json::Value, ApiError> {
    let mut guard = slot();
    match guard.as_mut() {
        Some(planner) => call(planner.as_mut()),
        None => Err(not_allowed()),
    }
}

/// `no_backup` is the CLI's `--no-backup`, the person's other answer to the backup question, and in
/// no input schema, so the accepted set is a list rather than read off a schema.
const SHARED_KEYS: &[&str] = &[
    "image",
    "port",
    "erase_nvs",
    "dry_run",
    "backup",
    "no_backup",
];

/// `--erase-nvs` and `--backup` (which reads cardid and NVS) are refused unless the caller is the
/// CLI.
pub fn request_of(args: &serde_json::Value, from_cli: bool) -> Result<DeviceRequest, ApiError> {
    request_of_with(args, from_cli, &[])
}

/// `extra` is the command's own key list; a key outside the union is an unknown field. A command
/// must not accept an argument it does not act on: someone who passes `reset` to `flash_device`
/// expects a reset, and silence is the worst answer. So only `device_boot_check` takes `reset`.
pub fn request_of_with(
    args: &serde_json::Value,
    from_cli: bool,
    extra: &[&str],
) -> Result<DeviceRequest, ApiError> {
    let object = args
        .as_object()
        .ok_or_else(|| ApiError::new(E_USAGE, "arguments: expected an object"))?;
    for name in object.keys() {
        if !SHARED_KEYS.contains(&name.as_str()) && !extra.contains(&name.as_str()) {
            return Err(ApiError::new(
                E_USAGE,
                format!("arguments: unknown field `{name}`"),
            ));
        }
    }
    let string = |name: &str| -> Result<Option<String>, ApiError> {
        match object.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(_) => Err(ApiError::new(
                E_USAGE,
                format!("arguments: `{name}` is a non-empty string"),
            )),
        }
    };
    let flag = |name: &str| -> Result<bool, ApiError> {
        match object.get(name) {
            None | Some(serde_json::Value::Null) => Ok(false),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(_) => Err(ApiError::new(
                E_USAGE,
                format!("arguments: `{name}` is a boolean"),
            )),
        }
    };
    let request = DeviceRequest {
        image: string("image")?
            .ok_or_else(|| ApiError::new(E_USAGE, "arguments: missing `image`"))?,
        port: string("port")?,
        erase_nvs: flag("erase_nvs")?,
        dry_run: flag("dry_run")?,
        backup: flag("backup")?,
        // Only `device_boot_check` passes `"reset"` in `extra`.
        reset: flag("reset")?,
    };
    if !from_cli && request.erase_nvs {
        return Err(ApiError::new(
            E_PLAN_REFUSED,
            "`erase_nvs` is a CLI flag a person types; it is never available over MCP or HTTP",
        ));
    }
    if !from_cli && request.backup {
        return Err(ApiError::new(
            E_PLAN_REFUSED,
            "`backup` reads cardid and NVS and is CLI only",
        ));
    }
    // The person's other answer to the same question, so CLI only as well; saying both is not an
    // answer.
    if !from_cli && flag("no_backup")? {
        return Err(ApiError::new(
            E_PLAN_REFUSED,
            "`no_backup` answers a question only a person on the CLI is asked",
        ));
    }
    if request.backup && flag("no_backup")? {
        return Err(ApiError::new(
            E_USAGE,
            "`--backup` and `--no-backup` contradict each other; type one of them",
        ));
    }
    Ok(request)
}

/// [`request_of`] has already refused it from anything but the CLI.
pub fn declined_backup(args: &serde_json::Value) -> bool {
    args.get("no_backup")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// That person has two flags no tool call may set.
pub fn from_cli() -> bool {
    CALLER.load(Ordering::SeqCst) == CALLER_CLI
}

pub const OFFLINE_LABEL: &str = "offline, device checks pending";
pub const CHECKS_PENDING: &str = "pending";

/// Never prints a port path, a MAC, a backup file name or a cardid byte: the planner's JSON carries
/// none of them.
pub fn render_plan(plan: &serde_json::Value) -> String {
    let mut text = String::new();
    if let Some(digest) = plan.get("plan_sha256").and_then(serde_json::Value::as_str) {
        text.push_str(&format!("plan {digest}\n"));
    }
    if let Some(writes) = plan.get("writes").and_then(serde_json::Value::as_array) {
        for write in writes {
            let name = write
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let offset = write
                .get("offset")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let bytes = write
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            text.push_str(&format!("  write {name} at {offset:#x}, {bytes} bytes\n"));
        }
        if writes.is_empty() {
            text.push_str("  no write\n");
        }
    }
    if let Some(refusals) = plan.get("refusals").and_then(serde_json::Value::as_array) {
        for refusal in refusals {
            let rule = refusal
                .get("rule")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let detail = refusal
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            text.push_str(&format!("  refused {rule}: {detail}\n"));
        }
    }
    if plan
        .get("device_checks")
        .and_then(serde_json::Value::as_str)
        == Some(CHECKS_PENDING)
    {
        text.push_str(&format!(
            "{OFFLINE_LABEL}: the identity, device-table and nvs rules run again at Identify\n"
        ));
    }
    text
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "Arguments of `plan_flash`: the image to plan and the optional port. Nothing here opens a device.",
        "required": ["image"],
        "properties": {
            "image": {
                "type": "string",
                "description": "A merged `.bin` of the whole 8 MB part. A build directory is refused in v1."
            },
            "port": {
                "type": ["string", "null"],
                "description": "A discovered 303A:1001 port; required when several are attached."
            },
            "erase_nvs": {
                "type": "boolean",
                "description": "CLI only, never over MCP: erase the `nvs` partition as well."
            }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "The write list, the refusals and the plan digest. No device value, no port path.",
        "required": ["plan_sha256", "writes", "refusals", "accepted", "device_checks"],
        "properties": {
            "plan_sha256": { "type": "string", "description": "SHA-256 over the whole plan." },
            "writes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "offset", "bytes", "sha256"],
                    "properties": {
                        "name": { "type": "string" },
                        "offset": { "type": "integer", "minimum": 0 },
                        "bytes": { "type": "integer", "minimum": 0 },
                        "sha256": { "type": "string" }
                    }
                }
            },
            "refusals": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["rule", "detail"],
                    "properties": {
                        "rule": { "type": "string" },
                        "detail": { "type": "string" }
                    }
                }
            },
            "accepted": { "type": "boolean", "description": "No rule fired." },
            "device_checks": {
                "type": "string",
                "enum": ["pending", "done"],
                "description": "`pending` when the plan was made without a device."
            }
        }
    })
}

/// Plan what would be written to a real AI Passport, without opening it.
#[command(
    api_crate = crate,
    name = "plan_flash",
    group = device,
    input_schema = input_schema,
    output_schema = output_schema,
    cli(positional = ["image"], cli_only = ["erase_nvs"]),
    annotations(read_only, idempotent, native_only),
    errors(E_USAGE, E_PLAN_REFUSED, E_HOST_UNSUPPORTED),
    example(
        title = "Plan the official image without a device",
        args = r#"{"image":"FoloToy-AI-Passport-8MB.bin"}"#,
    ),
)]
pub fn plan_flash(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let request = request_of(&args, from_cli())?;
    let plan = with_planner(|planner| planner.plan(&request))?;
    let text = render_plan(&plan);
    Ok(Output::new(plan, text, Receipt::default()))
}

/// Named here so every Device command lists the same set.
pub const DEVICE_CODES: [crate::error::ErrorCode; 3] =
    [E_PLAN_REFUSED, E_DEVICE_BUSY, E_CARDID_CHANGED];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::find;
    use crate::spec::CapsGroup;

    /// The planner slot is process-wide; [`super::test_gate`] is shared with every module that
    /// installs.
    use super::test_gate as gate;

    struct Fake {
        asked: Vec<DeviceRequest>,
        plan: serde_json::Value,
    }

    impl DevicePlanner for Fake {
        fn plan(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
            self.asked.push(request.clone());
            Ok(self.plan.clone())
        }
        fn flash(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
            self.asked.push(request.clone());
            Ok(self.plan.clone())
        }
        fn boot_check(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
            self.asked.push(request.clone());
            Ok(self.plan.clone())
        }
    }

    fn offline_plan() -> serde_json::Value {
        serde_json::json!({
            "plan_sha256": "a".repeat(64),
            "writes": [{"name": "factory", "offset": 0x10000, "bytes": 1024, "sha256": "b".repeat(64)}],
            "refusals": [],
            "accepted": true,
            "device_checks": CHECKS_PENDING,
        })
    }

    /// Asks who called while it runs, as `pemu_host::device::caller_origin` does for every plan.
    struct Reentrant;

    impl DevicePlanner for Reentrant {
        fn plan(&mut self, _: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
            Ok(serde_json::json!({ "from_cli": from_cli(), "allowed": device_allowed() }))
        }
        fn flash(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
            self.plan(request)
        }
        fn boot_check(&mut self, request: &DeviceRequest) -> Result<serde_json::Value, ApiError> {
            self.plan(request)
        }
    }

    /// A planner that asks [`from_cli`] must not re-enter the planner's mutex, which would hang
    /// with no deadline. The test asserts it by finishing.
    #[test]
    fn a_planner_may_ask_who_called_while_it_runs() {
        let _gate = gate();
        uninstall();
        for (caller, want) in [(DeviceCaller::Cli, true), (DeviceCaller::Agent, false)] {
            install(caller, Box::new(Reentrant));
            assert!(device_allowed());
            let out = with_planner(|planner| planner.plan(&DeviceRequest::default()))
                .expect("the planner answers instead of hanging");
            assert_eq!(out["from_cli"], want, "{caller:?}");
            assert_eq!(out["allowed"], true, "{caller:?}");
        }
        uninstall();
        assert!(!device_allowed() && !from_cli());
    }

    #[test]
    fn the_command_is_registered_native_only_in_the_device_group() {
        let spec = find("plan_flash").expect("registered");
        assert_eq!(spec.group, CapsGroup::Device);
        assert!(spec.annotations.native_only && spec.annotations.read_only);
        assert!(!spec.annotations.needs_instance);
    }

    #[test]
    fn without_allow_device_the_command_refuses() {
        let _gate = gate();
        uninstall();
        assert!(!device_allowed());
        let error = with_planner(|_| Ok(serde_json::Value::Null)).expect_err("no planner");
        assert_eq!(error.code, E_PLAN_REFUSED);
        assert!(
            error.message.contains("--allow-device"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_offline_plan_is_labelled_and_a_checked_one_is_not() {
        let offline = render_plan(&offline_plan());
        assert!(offline.contains(OFFLINE_LABEL), "{offline}");
        assert!(
            offline.contains("write factory at 0x10000, 1024 bytes"),
            "{offline}"
        );
        let mut checked = offline_plan();
        checked["device_checks"] = serde_json::json!("done");
        let text = render_plan(&checked);
        assert!(!text.contains(OFFLINE_LABEL), "{text}");
    }

    #[test]
    fn erase_nvs_and_backup_are_cli_only() {
        let args = serde_json::json!({"image": "x.bin", "erase_nvs": true});
        let refused = request_of(&args, false).expect_err("not over MCP");
        assert_eq!(refused.code, E_PLAN_REFUSED);
        assert!(request_of(&args, true).is_ok());
        let args = serde_json::json!({"image": "x.bin", "backup": true});
        assert_eq!(
            request_of(&args, false).expect_err("not over MCP").code,
            E_PLAN_REFUSED
        );
        assert!(request_of(&args, true).is_ok());
    }

    #[test]
    fn the_argument_shape_is_checked_first() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"image": ""}),
            serde_json::json!({"image": "x.bin", "port": 7}),
            serde_json::json!({"image": "x.bin", "unknown": true}),
            serde_json::json!([]),
        ] {
            assert_eq!(
                request_of(&bad, true).expect_err("refused").code,
                E_USAGE,
                "{bad}"
            );
        }
        let request = request_of(
            &serde_json::json!({"image": "x.bin", "port": "p", "dry_run": true}),
            true,
        )
        .expect("well formed");
        assert_eq!(
            request,
            DeviceRequest {
                image: "x.bin".to_owned(),
                port: Some("p".to_owned()),
                erase_nvs: false,
                dry_run: true,
                backup: false,
                reset: false,
            }
        );
        // `reset` is `device_boot_check`'s own argument: an unknown field to a caller that does not
        // name it.
        let with_reset = serde_json::json!({"image": "x.bin", "reset": true});
        let refused = request_of(&with_reset, true).expect_err("not a shared argument");
        assert_eq!(refused.code, E_USAGE);
        assert!(
            refused.message.contains("unknown field `reset`"),
            "{}",
            refused.message
        );
        assert!(
            request_of_with(&with_reset, true, &["reset"])
                .expect("well formed for the command that declares it")
                .reset
        );
        assert_eq!(
            request_of_with(
                &serde_json::json!({"image": "x.bin", "reset": 1}),
                true,
                &["reset"]
            )
            .expect_err("a flag is a boolean")
            .code,
            E_USAGE
        );
        // An `extra` key does not widen the set for anything else.
        assert_eq!(
            request_of_with(
                &serde_json::json!({"image": "x.bin", "nope": true}),
                true,
                &["reset"]
            )
            .expect_err("still an unknown field")
            .code,
            E_USAGE
        );
    }

    #[test]
    fn an_installed_planner_answers_the_command() {
        let _gate = gate();
        install(
            DeviceCaller::Cli,
            Box::new(Fake {
                asked: Vec::new(),
                plan: offline_plan(),
            }),
        );
        assert!(from_cli());
        let out = with_planner(|planner| {
            planner.plan(&DeviceRequest {
                image: "x.bin".to_owned(),
                ..DeviceRequest::default()
            })
        })
        .expect("the planner answered");
        assert_eq!(out["device_checks"], CHECKS_PENDING);
        uninstall();
    }
}
