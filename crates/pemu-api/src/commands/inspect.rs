//! `passportsim inspect`: the guest seen from outside.
//!
//! Walkers need the app ELF, which `pemu-api` does not load, so the seam is [`Introspectors`]: one
//! function per section, each taking the machine and returning the walker's snapshot type. The host
//! installs real walkers, a test installs scripted ones. What stays here is testable without an
//! ELF: which sections a call asks for, how each is shaped, what an NVS listing may show, and what
//! a missing walker reports.
//!
//! `vars` takes names; a call that names none is `E_USAGE` rather than a dump of thousands of
//! globals. NVS is redacted twice: the walker keeps credential values out of `NvsValue`, and the
//! rendered text goes through the machine's `SecretSet`, because a value can be a key.

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pemu_core::hostio::SerialStream;
use pemu_introspect::IntrospectError;
use pemu_introspect::freertos::{DeadlockReport, TaskSnapshot};
use pemu_introspect::lvgl::UiTree;
use pemu_introspect::nvs::NvsListing;
use pemu_introspect::panic::PanicRecord;
use pemu_introspect::tlsf::HeapSnapshot;
use pemu_introspect::vars::{VarQuery, VarRead, VarSnapshot};
use pemu_machine::MachineApi;
use pemu_machine::stops::{Watchdog, WatchdogFire};

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_SECRET_REFUSED, E_STATE, E_USAGE};
use crate::output::Output;
use crate::redact::Redactor;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::run::Obs;
use crate::args::{instance_schema, object, only, opt_bool, opt_str, usage};
use crate::pool::{Pool, with_pool};
use crate::session::Session;

/// One function per section, each returning the walker's own snapshot type.
#[derive(Copy, Clone)]
pub struct Introspectors {
    pub tasks: fn(&mut dyn MachineApi) -> Result<TaskSnapshot, IntrospectError>,
    pub heap: fn(&mut dyn MachineApi) -> Result<HeapSnapshot, IntrospectError>,
    pub nvs: fn(&mut dyn MachineApi) -> Result<NvsListing, IntrospectError>,
    /// At the revision the caller passes.
    pub ui: fn(&mut dyn MachineApi, u64) -> Result<UiTree, IntrospectError>,
    /// Decoded from the `panic_info_t` pointer `esp_panic_handler` was handed, and unwound.
    pub panic: fn(&mut dyn MachineApi, u32) -> Result<PanicRecord, IntrospectError>,
    /// The one walker that takes a query. `run` arms its `var:` watch from the same reading, so the
    /// matcher and this section agree about which bytes a name means.
    pub vars: fn(&mut dyn MachineApi, &[VarQuery]) -> Result<VarSnapshot, IntrospectError>,
}

fn no_tasks(_machine: &mut dyn MachineApi) -> Result<TaskSnapshot, IntrospectError> {
    Err(no_walker("tskTaskControlBlock"))
}

fn no_heap(_machine: &mut dyn MachineApi) -> Result<HeapSnapshot, IntrospectError> {
    Err(no_walker("heap_t_"))
}

fn no_nvs(_machine: &mut dyn MachineApi) -> Result<NvsListing, IntrospectError> {
    Err(no_walker("nvs"))
}

fn no_ui(_machine: &mut dyn MachineApi, _rev: u64) -> Result<UiTree, IntrospectError> {
    Err(no_walker("_lv_global_t"))
}

fn no_panic(_machine: &mut dyn MachineApi, _info: u32) -> Result<PanicRecord, IntrospectError> {
    Err(no_walker("panic_info_t"))
}

fn no_vars(
    _machine: &mut dyn MachineApi,
    _queries: &[VarQuery],
) -> Result<VarSnapshot, IntrospectError> {
    Err(no_walker("DW_TAG_variable"))
}

/// Names the structure it would have needed, which tells a reader the ELF, not the guest, is
/// missing.
fn no_walker(name: &'static str) -> IntrospectError {
    IntrospectError::MissingStruct { name }
}

pub const NO_INTROSPECTORS: Introspectors = Introspectors {
    tasks: no_tasks,
    heap: no_heap,
    nvs: no_nvs,
    ui: no_ui,
    panic: no_panic,
    vars: no_vars,
};

/// A host's task-level deadlock detector, run at every slice boundary. A separate seam because it
/// runs after every slice and walks the task lists alone. It only reads guest memory and answers
/// `None` whenever it cannot be sure, which includes every firmware without an ELF.
pub type DeadlockWatch = fn(&mut dyn MachineApi, &str) -> Option<DeadlockReport>;

fn deadlock_watch_slot() -> &'static Mutex<Option<DeadlockWatch>> {
    static WATCH: OnceLock<Mutex<Option<DeadlockWatch>>> = OnceLock::new();
    WATCH.get_or_init(|| Mutex::new(None))
}

pub fn set_deadlock_watch(watch: Option<DeadlockWatch>) {
    *deadlock_watch_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = watch;
}

/// With none installed, no check runs.
#[must_use]
pub fn deadlock_watch() -> Option<DeadlockWatch> {
    *deadlock_watch_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A cycle of FreeRTOS tasks each waiting forever for a mutex the next holds, with nothing outside
/// the cycle left to run. `detail.deadlock` reads `task`, and `detail.wake_inputs` is the reset
/// alone: a mutex is released only by its holder, and every holder is parked in the cycle.
#[must_use]
pub fn task_deadlock_error(report: &DeadlockReport) -> ApiError {
    let cycle: Vec<serde_json::Value> = report
        .cycle
        .iter()
        .map(|link| {
            serde_json::json!({
                "task": link.task,
                "tcb": format!("{:#010x}", link.tcb),
                "waits_for": format!("{:#010x}", link.mutex),
                "held_by": link.holder_name,
            })
        })
        .collect();
    let names: Vec<&str> = report.cycle.iter().map(|link| link.task.as_str()).collect();
    ApiError::new(
        crate::error::E_DEADLOCK,
        format!(
            "{} tasks are deadlocked on FreeRTOS mutexes: {}",
            report.cycle.len(),
            names.join(" -> ")
        ),
    )
    .with_detail(serde_json::json!({ "deadlock": "task", "cycle": cycle }))
    .with_hint(
        "the cycle cannot resolve: each task waits with no timeout for a mutex the next one \
         holds, so only a reset clears it",
    )
}

fn slot() -> &'static Mutex<Option<Introspectors>> {
    static SLOT: OnceLock<Mutex<Option<Introspectors>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn set_introspectors(introspectors: Introspectors) {
    let mut guard: MutexGuard<'_, Option<Introspectors>> = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(introspectors);
}

#[must_use]
pub fn introspectors() -> Introspectors {
    let guard = match slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    (*guard).unwrap_or(NO_INTROSPECTORS)
}

/// So [`walk_firmware`] names the firmware of the walk in progress.
fn walk_gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

fn walk_slot() -> &'static Mutex<Option<String>> {
    static FW: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    FW.get_or_init(|| Mutex::new(None))
}

/// Loads and parses the firmware's app ELF before a walk takes the gate, so a slow parse never
/// blocks other walks.
pub type WalkPrepare = fn(&str);

fn walk_prepare_slot() -> &'static Mutex<Option<WalkPrepare>> {
    static PREPARE: OnceLock<Mutex<Option<WalkPrepare>>> = OnceLock::new();
    PREPARE.get_or_init(|| Mutex::new(None))
}

pub fn set_walk_prepare(prepare: Option<WalkPrepare>) {
    *walk_prepare_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = prepare;
}

/// The machine carries no ELF, so a host walker asks which firmware it is walking and loads that
/// ELF. Walks serialize through one gate because a core crate may not use a thread-local.
pub fn with_walk_firmware<R>(fw: &str, f: impl FnOnce() -> R) -> R {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            *walk_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    let prepare = *walk_prepare_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(prepare) = prepare {
        prepare(fw);
    }
    let _gate = walk_gate().lock().unwrap_or_else(|e| e.into_inner());
    *walk_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(fw.to_owned());
    let _clear = Clear;
    f()
}

#[must_use]
pub fn walk_firmware() -> Option<String> {
    walk_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// `E_STATE`, not `E_INTERNAL`: a walker fails because the guest is not where the walk needs it (no
/// display yet, scheduler not started, no ELF), and the answer is to run further.
pub fn walker_error(section: &str, err: &IntrospectError) -> ApiError {
    ApiError::new(E_STATE, format!("`{section}`: {err}")).with_hint(
        "run the guest further, or check that the instance was started from an image with debug \
         information",
    )
}

crate::matchers::str_enum! {
    pub enum Section {
        Tasks = "tasks",
        Heap = "heap",
        /// Credential values are never shown.
        Nvs = "nvs",
        Lvgl = "lvgl",
        /// Named globals at their DWARF types. Takes `vars`.
        Vars = "vars",
        Fidelity = "fidelity",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectArgs {
    pub instance: Option<String>,
    /// In the order asked for; never empty.
    pub what: Vec<Section>,
    /// Only values the firmware wrote during this session; seeded content is never echoed.
    pub nvs_values: bool,
    /// Non-empty exactly when `vars` was asked for.
    pub vars: Vec<VarQuery>,
}

impl InspectArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<InspectArgs, ApiError> {
        let args = object(value)?;
        only(args, &["instance", "what", "nvs_values", "vars"])?;
        let what = match args.get("what") {
            Some(serde_json::Value::Array(items)) => {
                let mut what = Vec::with_capacity(items.len());
                for item in items {
                    let text = item
                        .as_str()
                        .ok_or_else(|| usage("what", "expected an array of section names"))?;
                    let section = Section::parse(text).ok_or_else(|| {
                        usage(
                            "what",
                            &format!(
                                "`{text}` is not a section this build answers ({})",
                                Section::vocabulary()
                            ),
                        )
                    })?;
                    if !what.contains(&section) {
                        what.push(section);
                    }
                }
                what
            }
            Some(serde_json::Value::String(text)) => {
                vec![Section::parse(text).ok_or_else(|| {
                    usage(
                        "what",
                        &format!(
                            "`{text}` is not a section this build answers ({})",
                            Section::vocabulary()
                        ),
                    )
                })?]
            }
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(_) => return Err(usage("what", "expected a section name or an array of them")),
        };
        if what.is_empty() {
            return Err(usage(
                "what",
                &format!("names at least one section ({})", Section::vocabulary()),
            ));
        }
        let vars = match args.get("vars") {
            Some(serde_json::Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let text = item
                        .as_str()
                        .ok_or_else(|| usage("vars", "expected an array of global names"))?;
                    out.push(parse_var_query(text)?);
                }
                out
            }
            Some(serde_json::Value::String(text)) => vec![parse_var_query(text)?],
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(_) => return Err(usage("vars", "expected a global name or an array of them")),
        };
        if !vars.is_empty() && !what.contains(&Section::Vars) {
            return Err(usage(
                "vars",
                "names globals but `what` does not ask for the `vars` section",
            ));
        }
        if what.contains(&Section::Vars) && vars.is_empty() {
            return Err(usage(
                "vars",
                "`vars` reads the globals this argument names, and a firmware declares too many \
                 to list them all: name at least one, as in `s_sel`, `main.c::s_sel` \
                 or `s_ok[2]`",
            ));
        }
        Ok(InspectArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            what,
            nvs_values: opt_bool(args, "nvs_values")?.unwrap_or(false),
            vars,
        })
    }
}

/// Refused with `E_USAGE` naming the grammar rather than reaching a walker.
fn parse_var_query(text: &str) -> Result<VarQuery, ApiError> {
    VarQuery::parse(text).map_err(|e| usage("vars", &format!("`{text}` {e}")))
}

pub fn inspect_on(
    session: &mut Session,
    args: &InspectArgs,
    redactor: &Redactor<'_>,
) -> Result<Output, ApiError> {
    let walkers = introspectors();
    let mut json = serde_json::Map::new();
    let mut text = String::new();
    json.insert("instance".into(), session.id.to_string().into());
    for section in &args.what {
        let (value, rendered) = match section {
            Section::Tasks => {
                let fw = session.fw.clone();
                let snapshot = with_walk_firmware(&fw, || (walkers.tasks)(session.machine()))
                    .map_err(|e| walker_error("tasks", &e))?;
                (tasks_json(&snapshot), snapshot.render())
            }
            Section::Heap => {
                let fw = session.fw.clone();
                let snapshot = with_walk_firmware(&fw, || (walkers.heap)(session.machine()))
                    .map_err(|e| walker_error("heap", &e))?;
                // Ledger blocks are ordinary heap blocks the walker already counts; the label tells
                // a reader which bytes are the emulator's estimate of a replaced blob's
                // allocations.
                let ledger = session.machine().heap_ledger();
                (
                    heap_json(&snapshot, &ledger),
                    format!("{}{}", snapshot.render(), ledger_text(&ledger)),
                )
            }
            Section::Nvs => {
                let fw = session.fw.clone();
                let listing = with_walk_firmware(&fw, || (walkers.nvs)(session.machine()))
                    .map_err(|e| walker_error("nvs", &e))?;
                (nvs_json(&listing, args.nvs_values), listing.render())
            }
            Section::Lvgl => {
                let fw = session.fw.clone();
                let tree = with_walk_firmware(&fw, || (walkers.ui)(session.machine(), 0))
                    .map_err(|e| walker_error("lvgl", &e))?;
                (lvgl_json(&tree), lvgl_text(&tree))
            }
            Section::Vars => {
                let fw = session.fw.clone();
                let snapshot =
                    with_walk_firmware(&fw, || (walkers.vars)(session.machine(), &args.vars))
                        .map_err(|e| walker_error("vars", &e))?;
                (vars_json(&snapshot), snapshot.render())
            }
            Section::Fidelity => {
                let receipt = session.receipt();
                (fidelity_json(&receipt), fidelity_text(&receipt))
            }
        };
        json.insert(section.as_str().to_owned(), value);
        // Redact before shaping, since a cut secret can no longer be matched.
        let rendered = redactor.redact_text(rendered.trim_end());
        if !rendered.is_empty() {
            let _ = writeln!(text, "[{}]\n{rendered}", section.as_str());
        }
    }
    let receipt = session.receipt();
    let mut json = serde_json::Value::Object(json);
    redactor.redact_json(&mut json);
    Ok(Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT))
}

/// Every field is the guest's own (address, DWARF type, value, compilation unit), so no host path
/// or device identity can reach here.
fn vars_json(snapshot: &VarSnapshot) -> serde_json::Value {
    serde_json::Value::Array(
        snapshot
            .reads
            .iter()
            .map(|read| match read {
                VarRead::Ok(r) => serde_json::json!({
                    "query": r.query,
                    "name": r.name,
                    "unit": r.unit,
                    "addr": format!("{:#010x}", r.addr),
                    "bytes": r.len,
                    "type": r.ty,
                    "value": var_value_json(&r.value),
                }),
                VarRead::Err { query, error } => serde_json::json!({
                    "query": query,
                    "unreadable": error.to_string(),
                }),
            })
            .collect::<Vec<_>>(),
    )
}

/// An aggregate this build does not decode reports its type and size.
fn var_value_json(value: &pemu_introspect::vars::VarValue) -> serde_json::Value {
    use pemu_introspect::vars::VarValue;
    match value {
        VarValue::Int(v) => serde_json::json!(v),
        VarValue::Uint(v) => serde_json::json!(v),
        VarValue::Bool(v) => serde_json::json!(v),
        VarValue::Float(v) => serde_json::json!(v),
        VarValue::Text(s) => serde_json::json!(s),
        VarValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(var_value_json).collect())
        }
        VarValue::Opaque { name, size } => serde_json::json!({ "type": name, "bytes": size }),
    }
}

fn tasks_json(snapshot: &TaskSnapshot) -> serde_json::Value {
    serde_json::json!({
        "tick": snapshot.tick,
        "count": snapshot.count,
        "consistent": snapshot.is_consistent(),
        "tasks": snapshot.tasks.iter().map(|task| serde_json::json!({
            "name": task.name,
            "state": task.state.tag(),
            "priority": task.priority,
            "base_priority": task.base_priority,
            "stack_size": task.stack_bytes,
            // The high-water mark is the untouched fill, in bytes.
            "stack_hwm_bytes": task.stack_free_bytes,
            "blocked_on": task.blocked_on,
        })).collect::<Vec<_>>(),
        "warnings": snapshot.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
    })
}

/// One line per block the radio modules hold, then a total. The word after the class letter says
/// the size is an estimate, not a measurement.
fn ledger_text(ledger: &[pemu_machine::hle::HleHeapBlock]) -> String {
    if ledger.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for block in ledger {
        let _ = writeln!(
            out,
            "ledger {}.{} {:#010x} {}B class={} {}",
            block.module, block.label, block.addr, block.bytes, block.class, block.fidelity
        );
    }
    let bytes: u64 = ledger.iter().map(|b| u64::from(b.bytes)).sum();
    let _ = writeln!(out, "ledger total blocks={} bytes={bytes}", ledger.len());
    out
}

fn heap_json(
    snapshot: &HeapSnapshot,
    ledger: &[pemu_machine::hle::HleHeapBlock],
) -> serde_json::Value {
    let largest = snapshot
        .regions
        .iter()
        .map(|region| region.largest_free_fit)
        .max()
        .unwrap_or(0);
    serde_json::json!({
        "total_free_bytes": snapshot.total_free(),
        "total_minimum_free_bytes": snapshot.total_minimum_free(),
        "largest_free_block": largest,
        "regions": snapshot.regions.iter().map(|region| serde_json::json!({
            "start": format!("{:#010x}", region.start),
            "end": format!("{:#010x}", region.end),
            "pool_size": region.pool_size,
            "free_bytes": region.free_bytes,
            "minimum_free_bytes": region.minimum_free_bytes,
            "used_blocks": region.used_blocks,
            "free_blocks": region.free_blocks,
            "largest_free_block": region.largest_free_fit,
        })).collect::<Vec<_>>(),
        "warnings": snapshot.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "ledger": {
            "fidelity": ledger.first().map(|b| b.fidelity.clone()),
            "bytes": ledger.iter().map(|b| u64::from(b.bytes)).sum::<u64>(),
            "blocks": ledger.iter().map(|block| serde_json::json!({
                "module": block.module,
                "label": block.label,
                "addr": format!("{:#010x}", block.addr),
                "bytes": block.bytes,
                "caps": format!("{:#x}", block.caps),
                "class": block.class,
            })).collect::<Vec<_>>(),
        },
    })
}

fn nvs_json(listing: &NvsListing, nvs_values: bool) -> serde_json::Value {
    serde_json::json!({
        "pages": listing.pages.len(),
        "namespaces": listing.namespaces,
        "credential_keys": listing.credentials().count(),
        "entries": listing.entries.iter().map(|entry| {
            // The walker already withholds credential values; the only decision here is whether
            // other values are echoed.
            let value = if entry.credential || !nvs_values {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(entry.value.to_string())
            };
            serde_json::json!({
                "namespace": entry.namespace,
                "key": entry.key,
                "type": entry.ty.tag(),
                "size": entry.size,
                "value": value,
                "redacted": entry.credential || !nvs_values,
            })
        }).collect::<Vec<_>>(),
    })
}

fn lvgl_json(tree: &UiTree) -> serde_json::Value {
    let mut by_class: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for node in &tree.nodes {
        *by_class.entry(node.class.as_str()).or_insert(0) += 1;
    }
    serde_json::json!({
        "objects": tree.nodes.len(),
        "by_class": by_class,
        "ui_rev": tree.rev,
    })
}

/// One line per class, which makes a leak visible between two calls.
fn lvgl_text(tree: &UiTree) -> String {
    let mut by_class: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for node in &tree.nodes {
        *by_class.entry(node.class.as_str()).or_insert(0) += 1;
    }
    let mut text = format!("{} object(s), rev {}", tree.nodes.len(), tree.rev);
    for (class, count) in by_class {
        let _ = write!(text, "\n{class} {count}");
    }
    text
}

/// Not `Receipt::one_line()`, which every surface already carries; an empty table is stated rather
/// than dropped.
fn fidelity_text(receipt: &crate::receipt::Receipt) -> String {
    let mut text = String::new();
    if receipt.fidelity.is_empty() {
        text.push_str("no per-subsystem fidelity entry");
    } else {
        for entry in &receipt.fidelity {
            let _ = writeln!(text, "{} {}", entry.subsystem, entry.class.name());
        }
        text.truncate(text.trim_end().len());
    }
    let classes = &receipt.classes_touched;
    let named = |names: &[String]| {
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(" ")
        }
    };
    let _ = write!(
        text,
        "\nclass C: {}\nclass U: {}",
        named(&classes.c),
        named(&classes.u)
    );
    text
}

fn fidelity_json(receipt: &crate::receipt::Receipt) -> serde_json::Value {
    serde_json::json!({
        "entries": receipt.fidelity.iter().map(|entry| serde_json::json!({
            "subsystem": entry.subsystem,
            "class": entry.class.name(),
        })).collect::<Vec<_>>(),
        "classes_touched": {
            "C": receipt.classes_touched.c,
            "U": receipt.classes_touched.u,
        },
    })
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["what"],
        "description": "`inspect` arguments.",
        "properties": {
            "instance": instance_schema(),
            "what": {
                "type": "array",
                "minItems": 1,
                "items": { "type": "string", "enum": ["tasks", "heap", "nvs", "lvgl", "vars", "fidelity"] },
                "description": "Sections to report."
            },
            "nvs_values": { "type": "boolean", "description": "Show non-credential NVS values (false)." },
            "vars": {
                "type": "array",
                "minItems": 1,
                "items": { "type": "string" },
                "description": "Globals the `vars` section reads, as `s_sel`, `main.c::s_sel` or `s_ok[2]`. Required by `vars`, refused without it."
            }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "tasks": { "type": "object" },
            "heap": { "type": "object" },
            "nvs": { "type": "object" },
            "lvgl": { "type": "object" },
            "vars": { "type": "array" },
            "fidelity": { "type": "object" }
        }
    })
}

/// Read the guest's tasks, heap, NVS, LVGL objects and fidelity table.
#[command(
    api_crate = crate,
    name = "inspect",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(read_only, idempotent, needs_instance),
    cli(positional = ["what"]),
    scenario_step = "inspect.expect",
    errors(E_USAGE, E_STATE, E_LEASE, E_SECRET_REFUSED, E_INTERNAL),
    example(
        title = "Read the task table",
        args = r#"{"what":["tasks"]}"#,
    ),
    example(
        title = "Read the heap and the LVGL object counts together",
        args = r#"{"what":["heap","lvgl"]}"#,
    ),
    example(
        title = "List the NVS namespaces and keys, never their credential values",
        args = r#"{"what":["nvs"]}"#,
    ),
)]
pub fn inspect(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = InspectArgs::from_json(&args)?;
    with_pool(|pool: &mut Pool| {
        let id = pool.bind(SPEC_INSPECT.annotations, args.instance.as_deref())?;
        let now = pool
            .session(id)
            .map(Session::now)
            .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
        if let Some(state) = pool.table().get(id) {
            state.lease.check_call(
                crate::lease::LeaseHolder::Agent,
                SPEC_INSPECT.annotations,
                now,
            )?;
        }
        let session = pool
            .session_mut(id)
            .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
        let secrets = crate::commands::snapshot::secrets_of(session);
        inspect_on(session, &args, &Redactor::new(&secrets))
    })
}

/// Short, because each line costs output budget and the full console is one `serial` call away.
pub const FAULT_TAIL_LINES: usize = 12;

/// The fault kinds an envelope decodes. `start::fault_of` turns the stop into an [`ApiError`]; this
/// adds what the machine looked like and what to call next.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The guest reached `esp_panic_handler`, `abort` or `__assert_func`.
    Panic,
    /// It arrives as a panic, which is how the IDF reports it.
    Watchdog,
    /// Every task is blocked and nothing can run again.
    Deadlock,
    /// Typically spinning on a register this build does not model.
    Stuck,
}

/// Fallback text for what the TIMG stage interrupt cannot name: the RTC watchdog, a watchdog fed
/// between its report and the panic, a backend with no timer groups. UNVERIFIED beyond the ESP-IDF
/// v5.5.3 texts. Whole phrases, never bare words: boots print `task_wdt: Initialized`, and a bare
/// `"wdt"` would report a null-pointer panic as a watchdog. A renamed report reads as a plain
/// panic, the safe direction.
pub const WATCHDOG_MARKERS: &[&str] = &["task watchdog got triggered", "interrupt wdt timeout"];

impl Fault {
    /// From the console tail alone; commands call [`Fault::of_with_signal`].
    #[must_use]
    pub fn of(error: &ApiError, tail: &[String]) -> Option<Fault> {
        Fault::of_with_signal(error, tail, None)
    }

    /// `signal` is the TIMG watchdog stage that fired; [`WATCHDOG_MARKERS`] in `tail` are the
    /// fallback.
    #[must_use]
    pub fn of_with_signal(
        error: &ApiError,
        tail: &[String],
        signal: Option<Watchdog>,
    ) -> Option<Fault> {
        match error.code.name {
            "E_GUEST_PANIC" => {
                let watchdog = signal.is_some()
                    || tail.iter().any(|line| {
                        let lower = line.to_ascii_lowercase();
                        WATCHDOG_MARKERS.iter().any(|mark| lower.contains(mark))
                    });
                Some(if watchdog {
                    Fault::Watchdog
                } else {
                    Fault::Panic
                })
            }
            "E_DEADLOCK" => Some(Fault::Deadlock),
            "E_STUCK" => Some(Fault::Stuck),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Fault::Panic => "panic",
            Fault::Watchdog => "watchdog",
            Fault::Deadlock => "deadlock",
            Fault::Stuck => "stuck",
        }
    }

    /// In the order that pays off soonest.
    #[must_use]
    pub fn next_commands(self) -> &'static str {
        match self {
            Fault::Panic => {
                "`inspect --what tasks` shows what every task was doing when it stopped; \
                 `serial` reads the whole panic text; `snapshot save panic` keeps the state, which \
                 `--on-panic stop` left intact"
            }
            Fault::Watchdog => {
                "`inspect --what tasks` names the starving task and what it is blocked on, \
                 including the mutex owner; `serial` reads the watchdog's own report"
            }
            Fault::Deadlock => {
                "the guest waits for input, so running on to the same limit returns the \
                 same stop: send one of `detail.wake_inputs`; `inspect --what tasks` lists every \
                 task and what it blocked on; `snapshot save deadlock` keeps the state so the same \
                 block can be replayed"
            }
            Fault::Stuck => {
                "`inspect --what fidelity` lists what this run did not model, which is the usual \
                 cause of a busy-wait that never ends; `serial` shows how far the guest got"
            }
        }
    }
}

/// Every kind of input, since a hart in WFI with a routed interrupt may wake on any. UNVERIFIED per
/// guest, so the resets come first.
pub const WAKE_INPUTS: &[&str] = &[
    "input {button: reset, reset_kind: usb_rts}",
    "input {button: power, action: click}",
    "input {button: usb, action: unplug | plug | open | close}",
    "input {button: ok | up | down, action: click}",
    "serial {op: write}",
    "env",
];

/// Only a guest write changes the routing, and a waiting hart writes nothing.
pub const RESET_WAKE_INPUTS: &[&str] = &["input {button: reset, reset_kind: usb_rts}"];

/// A backend that does not know gets every input.
pub fn wake_inputs(interrupt_can_wake: Option<bool>) -> &'static [&'static str] {
    match interrupt_can_wake {
        Some(false) => RESET_WAKE_INPUTS,
        Some(true) | None => WAKE_INPUTS,
    }
}

/// Redaction is mandatory: a panic prints whatever the task held.
pub fn fault_tail(session: &mut Session, redactor: &Redactor) -> Vec<String> {
    let mut lines: Vec<(u64, String)> = Vec::new();
    for stream in [SerialStream::UsjTx, SerialStream::Uart0Tx] {
        let from = session.machine().io().serial_ring(stream).tail();
        let (observed, _next) =
            crate::commands::run::read_lines(session.machine().io(), stream, from);
        for obs in observed {
            if let Obs::Line { vt, text, .. } = obs {
                lines.push((vt.0, redactor.redact_text(&text)));
            }
        }
    }
    lines.sort_by_key(|(vt, _)| *vt);
    let start = lines.len().saturating_sub(FAULT_TAIL_LINES);
    lines
        .split_off(start)
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

/// The task table, console tail and next commands. The backtrace is filled only when the stop
/// captured the `panic_info_t` ([`PANIC_INFO_KEY`]); a walker that cannot run says so in
/// `backtrace_error`. Errors that report no fault are returned untouched.
pub fn fault_envelope(session: &mut Session, redactor: &Redactor, error: ApiError) -> ApiError {
    // Not filled twice when a caller pipes every refusal through here.
    if error.detail.get("fault").is_some() {
        return error;
    }
    let tail = fault_tail(session, redactor);
    let vt_us = session.receipt().vt_us;
    // Which watchdog fired is a machine fact from the TIMG stage interrupt, not the printed text.
    let signal = watchdog_signal(session.snapshot_machine().watchdog_fired(), vt_us);
    let Some(mut fault) = Fault::of_with_signal(&error, &tail, signal.map(|fire| fire.which))
    else {
        return error;
    };
    let mut detail = serde_json::json!({
        "fault": fault.as_str(),
        "instance": session.id.to_string(),
        "vt_us": vt_us,
    });
    if fault == Fault::Watchdog
        && let Some(fire) = signal
    {
        detail["watchdog"] = fire.which.as_str().into();
        detail["watchdog_signal"] = format!(
            "{} watchdog stage interrupt at {} us, {} us before this stop{}",
            fire.which.timer_group(),
            fire.at.as_us(),
            vt_us.saturating_sub(fire.at.as_us()),
            if fire.unfed { ", not fed since" } else { "" }
        )
        .into();
    }
    // A walker that cannot run says so in the envelope rather than leaving a hole.
    let fw = session.fw.clone();
    match with_walk_firmware(&fw, || (introspectors().tasks)(session.machine())) {
        Ok(snapshot) => {
            let blocked: Vec<serde_json::Value> = snapshot
                .tasks
                .iter()
                .filter(|task| task.blocked_on.is_some())
                .map(|task| serde_json::json!({ "task": task.name, "blocked_on": task.blocked_on }))
                .collect();
            detail["tasks"] = tasks_json(&snapshot);
            detail["blocked"] = serde_json::Value::Array(blocked);
        }
        Err(err) => {
            detail["tasks_error"] = serde_json::Value::String(walker_error("tasks", &err).message);
        }
    }
    let mut backtrace = Vec::new();
    if let Some(info) = error
        .detail
        .get(PANIC_INFO_KEY)
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
    {
        match with_walk_firmware(&fw, || (introspectors().panic)(session.machine(), info)) {
            Ok(record) => {
                detail["reason"] = record.reason.clone().into();
                // The IDF's panic reason names a watchdog the console has not printed yet at the
                // stop.
                if let Some(text) = &record.detail {
                    detail["panic_reason"] = redactor.redact_text(text).into();
                    let lower = text.to_ascii_lowercase();
                    if fault == Fault::Panic
                        && signal.is_none()
                        && WATCHDOG_MARKERS
                            .iter()
                            .chain(PANIC_REASON_WATCHDOG_MARKERS)
                            .any(|mark| lower.contains(mark))
                    {
                        fault = Fault::Watchdog;
                        detail["fault"] = fault.as_str().into();
                    }
                }
                if let Some(frame) = record.frame {
                    detail["mepc"] = format!("{:#010x}", frame.mepc).into();
                    detail["mcause"] = format!("{:#010x}", frame.mcause).into();
                    detail["mtval"] = format!("{:#010x}", frame.mtval).into();
                    // So a debugger can be pointed at the same instant.
                    detail["registers"] = serde_json::json!({
                        "pc": format!("{:#010x}", frame.mepc),
                        "ra": format!("{:#010x}", frame.ra),
                        "sp": format!("{:#010x}", frame.sp),
                        "s0": format!("{:#010x}", frame.fp),
                    });
                }
                if let Some(running) = detail["tasks"]["tasks"].as_array().and_then(|tasks| {
                    tasks
                        .iter()
                        .find(|task| task["state"] == "running")
                        .and_then(|task| task["name"].as_str())
                        .map(str::to_owned)
                }) {
                    detail["task"] = running.into();
                }
                if !record.backtrace.warnings.is_empty() {
                    detail["backtrace_warnings"] = record
                        .backtrace
                        .warnings
                        .iter()
                        .map(|w| serde_json::Value::String(w.to_string()))
                        .collect();
                }
                backtrace = record.backtrace.frames;
            }
            Err(err) => {
                detail["backtrace_error"] =
                    serde_json::Value::String(walker_error("panic", &err).message);
            }
        }
    }
    if fault == Fault::Deadlock {
        // A task-level deadlock carries its cycle; a hart-level one is `hart`.
        let task_level = error.detail.get("deadlock").and_then(|v| v.as_str()) == Some("task");
        for key in ["deadlock", "cycle"] {
            if let Some(value) = error.detail.get(key) {
                detail[key] = value.clone();
            }
        }
        if !task_level {
            detail["deadlock"] = "hart".into();
        }
        let wakes = if task_level {
            // No input releases a mutex.
            RESET_WAKE_INPUTS
        } else {
            wake_inputs(session.snapshot_machine().interrupt_can_wake())
        };
        detail["wake_inputs"] = serde_json::json!(wakes);
    }
    let hint = match error.hint.as_deref() {
        Some(existing) => format!("{existing}; {}", fault.next_commands()),
        None => fault.next_commands().to_owned(),
    };
    error
        .at_vt_us(vt_us)
        .with_detail(detail)
        .with_serial_tail(tail)
        .with_backtrace(backtrace)
        .with_hint(hint)
}

/// How long after a stage interrupt a fault still belongs to a watchdog the guest has fed since.
/// The IDF panic handler feeds both watchdogs before reporting, so only the age counts: measured on
/// `probes/probe_wdt`, 212 us for the task watchdog and 4 us for the interrupt watchdog. The window
/// is far longer, so a later unrelated panic stays a plain panic. An unfed watchdog needs no
/// window.
pub const WATCHDOG_SIGNAL_WINDOW_US: u64 = 50_000;

/// Still unfed, or fired inside [`WATCHDOG_SIGNAL_WINDOW_US`].
#[must_use]
pub fn watchdog_signal(fire: Option<WatchdogFire>, vt_us: u64) -> Option<WatchdogFire> {
    fire.filter(|fire| {
        fire.unfed || vt_us.saturating_sub(fire.at.as_us()) <= WATCHDOG_SIGNAL_WINDOW_US
    })
}

/// UNVERIFIED beyond the ESP-IDF v5.5.3 texts `probes/probe_wdt` prints.
pub const PANIC_REASON_WATCHDOG_MARKERS: &[&str] = &["interrupt wdt timeout", "task_wdt"];

/// Set by `start::fault_of` to the guest address of the `panic_info_t`.
pub const PANIC_INFO_KEY: &str = "panic_info";
/// For `run`, `clock step`, `input` and `audio_capture`. Other errors are returned untouched.
pub fn deadlock_envelope(session: &mut Session, error: ApiError) -> ApiError {
    if error.code != crate::error::E_DEADLOCK {
        return error;
    }
    fault_envelope_of(session, error)
}

/// The set is extended first, so a credential the guest wrote during the run is redacted too.
pub fn fault_envelope_of(session: &mut Session, error: ApiError) -> ApiError {
    let secrets = crate::commands::snapshot::secrets_of(session);
    fault_envelope(session, &Redactor::new(&secrets), error)
}

/// For a caller holding the pool.
pub fn fault_envelope_in_pool(
    pool: &mut Pool,
    id: crate::instance::InstanceId,
    error: ApiError,
) -> ApiError {
    match pool.session_mut(id) {
        Some(session) => fault_envelope_of(session, error),
        None => error,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use pemu_core::time::VTime;
    use pemu_introspect::freertos::{Task, TaskState};
    use pemu_introspect::lvgl::UiNode;
    use pemu_introspect::nvs::{NvsEntry, NvsType, NvsValue};
    use pemu_introspect::tlsf::HeapRegion;

    use crate::commands::start::{Boot, StartArgs};
    use crate::instance::{InstanceId, Lifecycle};
    use crate::secret_set::SecretSet;

    #[test]
    fn a_row_five_deadlock_lists_the_reset_alone() {
        assert_eq!(wake_inputs(Some(false)), RESET_WAKE_INPUTS);
        assert_eq!(wake_inputs(Some(true)), WAKE_INPUTS);
        assert_eq!(wake_inputs(None), WAKE_INPUTS);
        assert!(RESET_WAKE_INPUTS.iter().all(|w| WAKE_INPUTS.contains(w)));
        assert_eq!(WAKE_INPUTS[0], RESET_WAKE_INPUTS[0], "resets first");
    }

    /// A test that installs introspectors holds this lock.
    static WORLD: StdMutex<()> = StdMutex::new(());

    pub(crate) fn world() -> std::sync::MutexGuard<'static, ()> {
        match WORLD.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) const PASSWORD: &str = "correct-horse-battery";

    fn scripted_tasks(_machine: &mut dyn MachineApi) -> Result<TaskSnapshot, IntrospectError> {
        Ok(TaskSnapshot {
            tasks: vec![
                Task {
                    tcb: 0x3fca_0000,
                    name: "main".to_owned(),
                    state: TaskState::Running,
                    priority: 1,
                    base_priority: 1,
                    stack_base: 0x3fcb_0000,
                    stack_end: 0x3fcb_0fff,
                    stack_bytes: 4096,
                    stack_free_bytes: 1536,
                    top_of_stack: 0x3fcb_0800,
                    event_list: 0,
                    blocked_on: None,
                    indefinite: false,
                    mutex_wait: None,
                },
                Task {
                    tcb: 0x3fca_1000,
                    name: "IDLE".to_owned(),
                    state: TaskState::Ready,
                    priority: 0,
                    base_priority: 0,
                    stack_base: 0x3fcb_2000,
                    stack_end: 0x3fcb_25ff,
                    stack_bytes: 1536,
                    stack_free_bytes: 512,
                    top_of_stack: 0x3fcb_2100,
                    event_list: 0,
                    blocked_on: Some("lvgl_port mutex".to_owned()),
                    indefinite: false,
                    mutex_wait: None,
                },
            ],
            tick: Some(1234),
            count: Some(2),
            current: Some(0x3fca_0000),
            idle: vec![0x3fca_1000],
            warnings: Vec::new(),
        })
    }

    fn scripted_heap(_machine: &mut dyn MachineApi) -> Result<HeapSnapshot, IntrospectError> {
        Ok(HeapSnapshot {
            regions: vec![HeapRegion {
                heap_t: 0x3fc9_0000,
                caps: [0, 0, 0],
                start: 0x3fc9_1000,
                end: 0x3fcb_0000,
                info: 0x3fc9_0100,
                free_bytes: 120_000,
                minimum_free_bytes: 90_000,
                pool_size: 130_000,
                control: 0x3fc9_0200,
                control_size: 256,
                sl_index_count: 16,
                small_block_size: 32,
                used_blocks: 40,
                free_blocks: 3,
                largest_free_raw: 65_536,
                largest_free_fit: 65_520,
            }],
            warnings: Vec::new(),
        })
    }

    fn scripted_nvs(_machine: &mut dyn MachineApi) -> Result<NvsListing, IntrospectError> {
        Ok(NvsListing {
            pages: Vec::new(),
            namespaces: vec!["wifi".to_owned()],
            entries: vec![
                NvsEntry {
                    namespace: "wifi".to_owned(),
                    key: "ssid".to_owned(),
                    ty: NvsType::Str,
                    size: 8,
                    value: NvsValue::Text("HomeNet".to_owned()),
                    credential: false,
                },
                NvsEntry {
                    namespace: "wifi".to_owned(),
                    key: "password".to_owned(),
                    ty: NvsType::Str,
                    size: 22,
                    // The walker put this redaction marker in place of the credential value.
                    value: NvsValue::Redacted,
                    credential: true,
                },
            ],
        })
    }

    pub(crate) fn scripted_ui(
        _machine: &mut dyn MachineApi,
        rev: u64,
    ) -> Result<UiTree, IntrospectError> {
        Ok(UiTree {
            rev,
            nodes: vec![
                UiNode {
                    obj: 0x3fcc_0000,
                    reference: "e1".to_owned(),
                    class: "obj".to_owned(),
                    class_chain: vec!["lv_obj".to_owned()],
                    parent: 0,
                    depth: 0,
                    children: vec![0x3fcc_0100],
                    w: 240,
                    h: 320,
                    ..UiNode::default()
                },
                UiNode {
                    obj: 0x3fcc_0100,
                    reference: "e2".to_owned(),
                    class: "label".to_owned(),
                    class_chain: vec!["lv_label".to_owned(), "lv_obj".to_owned()],
                    parent: 0x3fcc_0000,
                    depth: 1,
                    text: Some("Display".to_owned()),
                    w: 102,
                    h: 40,
                    ..UiNode::default()
                },
            ],
            display: 0x3fcd_0000,
            hor_res: 240,
            ver_res: 320,
            screen: 0x3fcc_0000,
            warnings: Vec::new(),
        })
    }

    /// `s_sel` and `s_active` of the `official` build, at the addresses and type its DWARF gives
    /// them, so a shaping test needs no ELF.
    pub(crate) fn scripted_vars(
        _machine: &mut dyn MachineApi,
        queries: &[VarQuery],
    ) -> Result<VarSnapshot, IntrospectError> {
        use pemu_introspect::vars::{VarRead, VarReading, VarValue};
        let mut reads = Vec::with_capacity(queries.len());
        for q in queries {
            let known = match q.name.as_str() {
                "s_sel" => Some((0x3fca_b5ccu32, 1i64)),
                "s_active" => Some((0x3fc9_d670, -1)),
                _ => None,
            };
            reads.push(match known {
                Some((addr, value)) => VarRead::Ok(VarReading {
                    query: q.render(),
                    name: q.name.clone(),
                    unit: "main.c".to_owned(),
                    addr,
                    len: 4,
                    ty: "int32".to_owned(),
                    value: VarValue::Int(value),
                }),
                None => VarRead::Err {
                    query: q.render(),
                    error: IntrospectError::MissingGlobal { name: q.render() },
                },
            });
        }
        Ok(VarSnapshot { reads })
    }

    pub(crate) const SCRIPTED: Introspectors = Introspectors {
        tasks: scripted_tasks,
        heap: scripted_heap,
        nvs: scripted_nvs,
        ui: scripted_ui,
        panic: super::no_panic,
        vars: scripted_vars,
    };

    pub(crate) fn instance() -> (Pool, InstanceId) {
        set_introspectors(SCRIPTED);
        let (machine, _) = crate::commands::env::tests::JournalMachine::new();
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        (pool, id)
    }

    fn args(json: serde_json::Value) -> InspectArgs {
        InspectArgs::from_json(&json).expect("inside the schema")
    }

    fn plain() -> SecretSet {
        SecretSet::builder().build()
    }

    #[test]
    fn the_walk_prepare_runs_before_the_gate_is_taken() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static GATE_FREE: AtomicBool = AtomicBool::new(false);
        fn prepare(fw: &str) {
            if fw == "prepare-probe" {
                GATE_FREE.store(walk_gate().try_lock().is_ok(), Ordering::SeqCst);
            }
        }
        let _world = world();
        set_walk_prepare(Some(prepare));
        let seen = with_walk_firmware("prepare-probe", walk_firmware);
        set_walk_prepare(None);
        assert_eq!(seen.as_deref(), Some("prepare-probe"));
        assert!(
            GATE_FREE.load(Ordering::SeqCst),
            "the gate was free while preparing"
        );
    }

    #[test]
    fn a_walker_sees_the_firmware_of_the_session_it_walks_and_nothing_outside_a_walk() {
        static SEEN: StdMutex<Vec<Option<String>>> = StdMutex::new(Vec::new());
        fn recording_heap(machine: &mut dyn MachineApi) -> Result<HeapSnapshot, IntrospectError> {
            SEEN.lock().expect("never poisoned").push(walk_firmware());
            scripted_heap(machine)
        }
        let _world = world();
        let (mut pool, id) = instance();
        set_introspectors(Introspectors {
            heap: recording_heap,
            ..SCRIPTED
        });
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        inspect_on(
            session,
            &args(serde_json::json!({"what":["heap"]})),
            &Redactor::new(&secrets),
        )
        .expect("the recording walker answers");
        set_introspectors(SCRIPTED);
        assert_eq!(
            *SEEN.lock().expect("never poisoned"),
            [Some("official".to_owned())]
        );
        assert_eq!(walk_firmware(), None, "cleared once the walk returns");
    }

    #[test]
    fn the_task_section_carries_the_stack_high_water_mark_in_bytes() {
        let _world = world();
        let (mut pool, id) = instance();
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        let out = inspect_on(
            session,
            &args(serde_json::json!({"what":["tasks"]})),
            &Redactor::new(&secrets),
        )
        .expect("the scripted walker answers");
        assert_eq!(out.json["tasks"]["count"], 2);
        assert_eq!(out.json["tasks"]["tasks"][0]["name"], "main");
        assert_eq!(
            out.json["tasks"]["tasks"][0]["stack_hwm_bytes"], 1536,
            "the untouched fill, in bytes"
        );
        assert_eq!(
            out.json["tasks"]["tasks"][1]["blocked_on"], "lvgl_port mutex",
            "a watchdog report needs what the task is blocked on"
        );
        assert!(out.text.contains("[tasks]"), "{}", out.text);
    }

    #[test]
    fn the_heap_section_reports_the_totals_and_the_largest_fitting_block() {
        let _world = world();
        let (mut pool, id) = instance();
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        let out = inspect_on(
            session,
            &args(serde_json::json!({"what":["heap"]})),
            &Redactor::new(&secrets),
        )
        .expect("the scripted walker answers");
        assert_eq!(out.json["heap"]["total_free_bytes"], 120_000);
        assert_eq!(out.json["heap"]["total_minimum_free_bytes"], 90_000);
        assert_eq!(
            out.json["heap"]["largest_free_block"], 65_520,
            "the fitting size, not the raw header size"
        );
    }

    #[test]
    fn the_nvs_section_lists_keys_and_never_a_credential_value() {
        let _world = world();
        let (mut pool, id) = instance();
        let mut builder = SecretSet::builder();
        builder.nvs_credential(PASSWORD.as_bytes());
        let secrets = builder.build();
        let session = pool.session_mut(id).expect("the instance");
        let out = inspect_on(
            session,
            &args(serde_json::json!({"what":["nvs"],"nvs_values":true})),
            &Redactor::new(&secrets),
        )
        .expect("the scripted walker answers");
        assert_eq!(out.json["nvs"]["namespaces"][0], "wifi");
        assert_eq!(out.json["nvs"]["credential_keys"], 1);
        assert_eq!(out.json["nvs"]["entries"][0]["key"], "ssid");
        assert_eq!(
            out.json["nvs"]["entries"][0]["value"], "\"HomeNet\"",
            "the walker renders a string value escaped"
        );
        assert_eq!(out.json["nvs"]["entries"][1]["key"], "password");
        assert_eq!(
            out.json["nvs"]["entries"][1]["value"],
            serde_json::Value::Null,
            "a credential value is never echoed, whatever `nvs_values` says"
        );
        assert_eq!(out.json["nvs"]["entries"][1]["redacted"], true);
        assert!(
            !serde_json::to_string(&out.json)
                .expect("the result is JSON")
                .contains(PASSWORD),
            "the password must not be anywhere in the result"
        );
        assert!(!out.text.contains(PASSWORD), "{}", out.text);
    }

    #[test]
    fn nvs_values_default_to_hidden() {
        let _world = world();
        let (mut pool, id) = instance();
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        let out = inspect_on(
            session,
            &args(serde_json::json!({"what":["nvs"]})),
            &Redactor::new(&secrets),
        )
        .expect("answers");
        assert_eq!(
            out.json["nvs"]["entries"][0]["value"],
            serde_json::Value::Null
        );
        assert_eq!(out.json["nvs"]["entries"][0]["redacted"], true);
    }

    #[test]
    fn several_sections_are_answered_in_the_order_they_were_asked_for() {
        let _world = world();
        let (mut pool, id) = instance();
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        let out = inspect_on(
            session,
            &args(serde_json::json!({"what":["lvgl","heap"]})),
            &Redactor::new(&secrets),
        )
        .expect("answers");
        assert_eq!(out.json["lvgl"]["objects"], 2);
        assert_eq!(out.json["lvgl"]["by_class"]["label"], 1);
        assert!(out.json["heap"].is_object());
        let lvgl = out.text.find("[lvgl]").expect("the lvgl block");
        let heap = out.text.find("[heap]").expect("the heap block");
        assert!(lvgl < heap, "{}", out.text);
    }

    #[test]
    fn the_fidelity_section_is_the_receipt_table() {
        let _world = world();
        let (mut pool, id) = instance();
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        let out = inspect_on(
            session,
            &args(serde_json::json!({"what":["fidelity"]})),
            &Redactor::new(&secrets),
        )
        .expect("answers");
        assert_eq!(out.json["fidelity"]["entries"], serde_json::json!([]));
        assert!(out.json["fidelity"]["classes_touched"].is_object());
        // Returning the receipt here would print the one-line receipt twice.
        assert!(out.text.contains("[fidelity]"), "{}", out.text);
        assert!(
            out.text.contains("no per-subsystem fidelity entry"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("profile "), "{}", out.text);
    }

    #[test]
    fn a_missing_walker_is_a_named_refusal_and_not_an_empty_section() {
        let _world = world();
        let (mut pool, id) = instance();
        {
            let mut guard = match slot().lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = None;
        }
        let secrets = plain();
        let session = pool.session_mut(id).expect("the instance");
        let error = inspect_on(
            session,
            &args(serde_json::json!({"what":["tasks"]})),
            &Redactor::new(&secrets),
        )
        .expect_err("no walker is installed");
        assert_eq!(error.code, E_STATE);
        assert!(
            error.message.contains("tskTaskControlBlock"),
            "{}",
            error.message
        );
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("run the guest further")),
            "{:?}",
            error.hint
        );
    }

    #[test]
    fn every_argument_outside_the_schema_is_usage() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "what": [] }),
            serde_json::json!({ "what": ["partitions"] }),
            serde_json::json!({ "what": "vars" }),
            serde_json::json!({ "what": [1] }),
            serde_json::json!({ "what": ["tasks"], "nonsense": 1 }),
        ] {
            assert_eq!(
                InspectArgs::from_json(&bad)
                    .expect_err("outside the schema")
                    .code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn vars_without_a_name_is_refused_rather_than_a_memory_dump() {
        for bad in [
            serde_json::json!({ "what": ["vars"] }),
            serde_json::json!({ "what": ["vars"], "vars": [] }),
        ] {
            let error = InspectArgs::from_json(&bad).expect_err("names no global");
            assert_eq!(error.code, E_USAGE, "{bad}");
            assert!(error.message.contains("s_sel"), "{}", error.message);
        }
        // Naming globals without asking for the section would silently do nothing.
        let error = InspectArgs::from_json(&serde_json::json!({"what":["tasks"],"vars":["s_sel"]}))
            .expect_err("the section was not asked for");
        assert_eq!(error.code, E_USAGE);
    }

    /// Malformed names are refused before a walker or DWARF parse is reached.
    #[test]
    fn a_vars_name_is_parsed_with_the_same_grammar_as_a_var_matcher() {
        let args = args(serde_json::json!({
            "what": ["vars"],
            "vars": ["s_sel", "main.c::s_active", "s_ok[2]"]
        }));
        assert_eq!(args.vars.len(), 3);
        assert_eq!(args.vars[1].unit.as_deref(), Some("main.c"));
        assert_eq!(args.vars[2].index, Some(2));
        for bad in ["s ok", "s_ok[", "s_ok[x]", ""] {
            let error = InspectArgs::from_json(&serde_json::json!({
                "what": ["vars"],
                "vars": [bad]
            }))
            .expect_err("not a global");
            assert_eq!(error.code, E_USAGE, "{bad}");
        }
    }

    #[test]
    fn vars_answers_each_global_with_its_address_type_and_value() {
        let _world = world();
        let (mut pool, id) = instance();
        let session = pool.session_mut(id).expect("the scripted instance");
        let args = args(serde_json::json!({
            "what": ["vars"],
            "vars": ["s_sel", "s_active"]
        }));
        let out = inspect_on(session, &args, &Redactor::new(&Default::default()))
            .expect("the scripted walker answers");
        let rows = out.json["vars"].as_array().expect("an array of globals");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "s_sel");
        assert_eq!(rows[0]["value"], 1);
        assert_eq!(rows[0]["addr"], "0x3fcab5cc");
        assert_eq!(rows[0]["type"], "int32");
        assert_eq!(rows[0]["unit"], "main.c");
        assert_eq!(rows[1]["name"], "s_active");
        assert_eq!(rows[1]["value"], -1);
        assert!(out.text.contains("s_sel"), "{}", out.text);
    }

    #[test]
    fn a_global_this_firmware_has_not_got_is_named_rather_than_hiding_the_others() {
        let _world = world();
        let (mut pool, id) = instance();
        let session = pool.session_mut(id).expect("the scripted instance");
        let args = args(serde_json::json!({
            "what": ["vars"],
            "vars": ["s_sel", "s_nope"]
        }));
        let out = inspect_on(session, &args, &Redactor::new(&Default::default()))
            .expect("the call succeeds; the one name does not");
        let rows = out.json["vars"].as_array().expect("an array");
        assert_eq!(rows[0]["value"], 1);
        assert!(
            rows[1]["unreadable"]
                .as_str()
                .is_some_and(|m| m.contains("s_nope")),
            "{:?}",
            rows[1]
        );
    }

    #[test]
    fn vars_without_a_walker_names_what_it_would_have_needed() {
        let _world = world();
        let (mut pool, id) = instance();
        set_introspectors(NO_INTROSPECTORS);
        let session = pool.session_mut(id).expect("the scripted instance");
        let args = args(serde_json::json!({"what":["vars"],"vars":["s_sel"]}));
        let error = inspect_on(session, &args, &Redactor::new(&Default::default()))
            .expect_err("no walker is installed");
        set_introspectors(SCRIPTED);
        assert_eq!(error.code, E_STATE);
        assert!(error.message.contains("DW_TAG_variable"), "{error:?}");
    }

    #[test]
    fn a_repeated_section_is_answered_once() {
        let args = args(serde_json::json!({"what":["heap","heap","tasks"]}));
        assert_eq!(args.what, vec![Section::Heap, Section::Tasks]);
    }

    fn console(pool: &mut Pool, id: InstanceId, lines: &[&str]) {
        let session = pool.session_mut(id).expect("the instance is live");
        for (index, line) in lines.iter().enumerate() {
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\n');
            let vt = VTime::from_ms(index as u64 + 1);
            session
                .machine()
                .io()
                .serial_write(SerialStream::UsjTx, &bytes, vt);
        }
    }

    #[test]
    fn a_panic_carries_the_task_table_the_serial_tail_and_what_to_call_next() {
        let _world = world();
        let (mut pool, id) = instance();
        console(
            &mut pool,
            id,
            &["I (10) pk_app: menu ready", "Guru Meditation Error"],
        );
        let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.detail["fault"], "panic");
        assert_eq!(filled.detail["tasks"]["count"], 2);
        assert_eq!(filled.detail["blocked"][0]["task"], "IDLE");
        assert_eq!(
            filled.serial_tail.last().map(String::as_str),
            Some("Guru Meditation Error"),
            "the tail ends at the fault"
        );
        let hint = filled.hint.as_deref().unwrap_or_default();
        assert!(hint.contains("inspect --what tasks"), "{hint}");
        assert!(
            filled.backtrace.is_empty(),
            "a panic whose stop carried no `panic_info_t` gets no invented frames"
        );
    }

    #[test]
    fn a_decoded_panic_carries_the_reason_the_registers_and_the_frames() {
        use pemu_introspect::panic::{ExcFrame, PanicKind};
        use pemu_introspect::unwind::{Backtrace, Frame, FrameOrigin};
        fn decoded(_m: &mut dyn MachineApi, info: u32) -> Result<PanicRecord, IntrospectError> {
            assert_eq!(info, 0x3fc8_e7fc, "the address the stop captured");
            let mut record = PanicRecord::from_hook(PanicKind::Exception, "load access fault");
            record.frame = Some(ExcFrame {
                at: 0x3fc9_0b00,
                mepc: 0x4200_6f52,
                ra: 0x4200_6f5e,
                sp: 0x3fc9_0be0,
                fp: 0,
                mstatus: 0x1881,
                mcause: 5,
                mtval: 0,
            });
            record.backtrace = Backtrace {
                frames: vec![Frame {
                    pc: 0x4200_6f52,
                    sp: 0x3fc9_0be0,
                    function: Some("probe_panic_read_null".to_owned()),
                    source: Some("/COMPONENT_MAIN_DIR/probe_panic.c".to_owned()),
                    file: Some("/COMPONENT_MAIN_DIR/probe_panic.c".to_owned()),
                    line: Some(49),
                    column: None,
                    inlined: false,
                    origin: FrameOrigin::App,
                }],
                warnings: Vec::new(),
            };
            Ok(record)
        }
        let _world = world();
        let (mut pool, id) = instance();
        set_introspectors(Introspectors {
            panic: decoded,
            ..SCRIPTED
        });
        let capture = pemu_machine::stops::PanicCapture {
            pc: 0x4201_0266,
            args: [0x3fc8_e7fc, 0, 0, 0],
            first: None,
        };
        let error = crate::session::fault_of(&pemu_machine::stops::StopReason::GuestPanic(capture))
            .expect("a panic is a fault");
        let filled = fault_envelope_in_pool(&mut pool, id, error.clone());
        assert_eq!(filled.detail["reason"], "load access fault");
        assert_eq!(filled.detail["mcause"], "0x00000005");
        assert_eq!(filled.detail["registers"]["ra"], "0x42006f5e");
        assert_eq!(filled.backtrace.len(), 1);
        let json = filled.to_json();
        assert_eq!(json["backtrace"][0]["function"], "probe_panic_read_null");
        assert_eq!(json["backtrace"][0]["line"], 49);
        assert_eq!(json["backtrace"][0]["pc"], "0x42006f52");
        assert_eq!(json["backtrace"][0]["origin"], "app");

        set_introspectors(SCRIPTED);
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert!(filled.backtrace.is_empty());
        assert!(
            filled.detail["backtrace_error"]
                .as_str()
                .is_some_and(|e| e.contains("panic_info_t")),
            "{}",
            filled.detail
        );
    }

    #[test]
    fn a_panic_reason_that_names_the_interrupt_watchdog_is_a_watchdog_envelope() {
        use pemu_introspect::panic::PanicKind;
        fn iwdt(_m: &mut dyn MachineApi, _info: u32) -> Result<PanicRecord, IntrospectError> {
            Ok(PanicRecord::from_hook(PanicKind::Exception, "interrupt 24")
                .with_detail("Interrupt wdt timeout on CPU0", None))
        }
        let _world = world();
        let (mut pool, id) = instance();
        console(&mut pool, id, &["IWDT|twdt_reason=6"]);
        set_introspectors(Introspectors {
            panic: iwdt,
            ..SCRIPTED
        });
        let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked")
            .with_detail(serde_json::json!({ PANIC_INFO_KEY: 0x3fc8_0000u32 }));
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        set_introspectors(SCRIPTED);
        assert_eq!(filled.detail["fault"], "watchdog", "{}", filled.detail);
        assert_eq!(
            filled.detail["panic_reason"],
            "Interrupt wdt timeout on CPU0"
        );
        assert!(
            filled
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("starving task")
        );
    }

    #[test]
    fn a_watchdog_is_told_apart_from_any_other_panic_by_its_own_report() {
        let _world = world();
        let (mut pool, id) = instance();
        console(
            &mut pool,
            id,
            &["E (900) task_wdt: Task watchdog got triggered"],
        );
        let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.detail["fault"], "watchdog");
        assert!(
            filled
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("starving task"),
            "the watchdog hint names what a watchdog envelope is for"
        );
    }

    #[test]
    fn a_boot_line_that_merely_mentions_the_watchdog_is_not_one() {
        let _world = world();
        for line in [
            "I (300) task_wdt: Initialized",
            "D (412) pk_app: esp_task_wdt_add(main)",
            "I (88) int_wdt: Interrupt watchdog enabled",
            "W (91) app: watchdog feeding thread started",
        ] {
            let (mut pool, id) = instance();
            console(
                &mut pool,
                id,
                &[
                    line,
                    "Guru Meditation Error: Core 0 panic'ed (LoadProhibited)",
                ],
            );
            let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
            let filled = fault_envelope_in_pool(&mut pool, id, error);
            assert_eq!(
                filled.detail["fault"], "panic",
                "`{line}` is not a watchdog report"
            );
        }
    }

    #[test]
    fn the_interrupt_watchdog_reason_is_a_watchdog() {
        let _world = world();
        let (mut pool, id) = instance();
        console(
            &mut pool,
            id,
            &["Guru Meditation Error: Core 0 panic'ed (Interrupt wdt timeout on CPU0)"],
        );
        let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.detail["fault"], "watchdog");
    }

    #[test]
    fn every_watchdog_marker_is_a_phrase_not_a_word() {
        for marker in WATCHDOG_MARKERS {
            assert!(
                marker.split_whitespace().count() >= 3,
                "`{marker}` is short enough to match an ordinary boot line"
            );
            assert_eq!(
                *marker,
                marker.to_ascii_lowercase(),
                "`{marker}` is compared against a lowercased line"
            );
        }
    }

    #[test]
    fn the_watchdog_is_classified_from_the_stage_interrupt_before_any_text() {
        let panic = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
        let quiet = ["Guru Meditation Error: Core 0 panic'ed (Load access fault)".to_owned()];
        assert_eq!(
            Fault::of_with_signal(&panic, &quiet, None),
            Some(Fault::Panic)
        );
        for which in [Watchdog::Task, Watchdog::Interrupt] {
            assert_eq!(
                Fault::of_with_signal(&panic, &quiet, Some(which)),
                Some(Fault::Watchdog),
                "{} names the watchdog whatever the console printed",
                which.timer_group()
            );
        }
        // Text still covers a watchdog no TIMG stage names.
        let printed = ["E (1) task_wdt: Task watchdog got triggered.".to_owned()];
        assert_eq!(
            Fault::of_with_signal(&panic, &printed, None),
            Some(Fault::Watchdog)
        );
        let deadlock = ApiError::new(crate::error::E_DEADLOCK, "waiting");
        assert_eq!(
            Fault::of_with_signal(&deadlock, &quiet, Some(Watchdog::Task)),
            Some(Fault::Deadlock)
        );
    }

    #[test]
    fn a_watchdog_signal_is_this_fault_s_only_while_it_is_recent_or_unfed() {
        let fire = |unfed: bool, at_us: u64| WatchdogFire {
            which: Watchdog::Task,
            at: pemu_core::time::VTime::from_us(at_us),
            unfed,
        };
        // The measured path from stage interrupt to panic hook on `probe_wdt`.
        assert!(watchdog_signal(Some(fire(false, 2_038_740)), 2_038_952).is_some());
        assert_eq!(
            watchdog_signal(Some(fire(false, 1_000_000)), 2_038_952),
            None
        );
        let edge = WATCHDOG_SIGNAL_WINDOW_US;
        assert!(watchdog_signal(Some(fire(false, 0)), edge).is_some());
        assert_eq!(watchdog_signal(Some(fire(false, 0)), edge + 1), None);
        assert!(watchdog_signal(Some(fire(true, 0)), 10_000_000).is_some());
        assert_eq!(watchdog_signal(None, 10), None);
    }

    #[test]
    fn a_task_deadlock_names_its_cycle_and_asks_for_a_reset() {
        use pemu_introspect::freertos::{CycleLink, DeadlockReport};
        let link = |task: &str, mutex: u32, holder: &str, tcb: u32, holder_tcb: u32| CycleLink {
            tcb,
            task: task.to_owned(),
            mutex,
            holder: holder_tcb,
            holder_name: holder.to_owned(),
        };
        let report = DeadlockReport {
            cycle: vec![
                link(
                    "dl_task_a",
                    0x3fcb_0d00,
                    "dl_task_b",
                    0x3fcb_0000,
                    0x3fcb_0200,
                ),
                link(
                    "dl_task_b",
                    0x3fcb_0c40,
                    "dl_task_a",
                    0x3fcb_0200,
                    0x3fcb_0000,
                ),
            ],
        };
        let error = task_deadlock_error(&report);
        assert_eq!(error.code, crate::error::E_DEADLOCK);
        assert!(
            error.message.contains("dl_task_a -> dl_task_b"),
            "{error:?}"
        );
        assert_eq!(error.detail["deadlock"], "task");
        assert_eq!(error.detail["cycle"][0]["task"], "dl_task_a");
        assert_eq!(error.detail["cycle"][0]["held_by"], "dl_task_b");
        assert_eq!(error.detail["cycle"][0]["waits_for"], "0x3fcb0d00");
        assert!(
            error.hint.as_deref().is_some_and(|h| h.contains("reset")),
            "{error:?}"
        );
    }

    #[test]
    fn a_deadlock_carries_what_every_task_blocked_on() {
        let _world = world();
        let (mut pool, id) = instance();
        let error = ApiError::new(crate::error::E_DEADLOCK, "every task is blocked");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.detail["fault"], "deadlock");
        assert_eq!(filled.detail["blocked"][0]["blocked_on"], "lvgl_port mutex");
    }

    #[test]
    fn a_stuck_report_points_at_what_was_not_modelled() {
        let _world = world();
        let (mut pool, id) = instance();
        let error = ApiError::new(crate::error::E_STUCK, "the guest made no progress");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.detail["fault"], "stuck");
        assert!(
            filled
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("inspect --what fidelity")
        );
    }

    #[test]
    fn a_refusal_that_is_not_a_guest_fault_is_left_alone() {
        let _world = world();
        let (mut pool, id) = instance();
        let error = ApiError::new(E_USAGE, "no such section").with_hint("read the schema");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.detail.as_ref(), &serde_json::Value::Null);
        assert!(filled.serial_tail.is_empty());
        assert_eq!(filled.hint.as_deref(), Some("read the schema"));
    }

    #[test]
    fn the_serial_tail_of_a_fault_is_redacted() {
        let _world = world();
        let (mut pool, id) = instance();
        console(&mut pool, id, &[&format!("W (9) wifi: pass={PASSWORD}")]);
        let mut builder = SecretSet::builder();
        builder.nvs_credential(PASSWORD.as_bytes());
        pool.with_store(|store| store.set_secret_set(id, builder.build()));
        let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        let tail = filled.serial_tail.join("\n");
        assert!(
            !tail.contains(PASSWORD),
            "the tail still carries it: {tail}"
        );
        assert!(tail.contains("wifi: pass="), "only the value is taken out");
    }

    #[test]
    fn the_serial_tail_keeps_only_the_last_lines() {
        let _world = world();
        let (mut pool, id) = instance();
        let lines: Vec<String> = (0..FAULT_TAIL_LINES * 2)
            .map(|index| format!("I ({index}) app: line {index}"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        console(&mut pool, id, &refs);
        let error = ApiError::new(crate::error::E_STUCK, "the guest made no progress");
        let filled = fault_envelope_in_pool(&mut pool, id, error);
        assert_eq!(filled.serial_tail.len(), FAULT_TAIL_LINES);
        assert!(filled.serial_tail[0].contains(&format!("line {FAULT_TAIL_LINES}")));
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("inspect").expect("#[command] registered inspect");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            InspectArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.read_only && spec.annotations.needs_instance);
        assert_eq!(spec.scenario_step, Some("inspect.expect"));
    }
}
