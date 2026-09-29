//! `cargo xtask agent-budget`: the token budgets of the agent-facing text output:
//!
//! - **each default text output is at most 4,000 characters**;
//! - **the reference agent loop totals under 8,000 characters**.
//!
//! A command past 4,000 costs a whole turn's context on one call, and a loop past 8,000 no longer
//! fits beside a task. What is measured is `Output::to_text`, the body plus the one-line receipt
//! suffix, exactly what the CLI prints and an MCP text content block carries; not the JSON, which
//! an agent asks for knowingly. Two populations are measured:
//!
//! 1. **every registered command's documented examples** (`CommandSpec::examples`), each against
//!    a freshly started scripted instance; an example that cannot run is a skip carrying its
//!    refusal, so shrinking coverage is visible instead of silent.
//! 2. **the reference agent loop** (`run --until ui:changed` included): start `official`, wait
//!    for the menu, read the UI, click DOWN, wait for the UI to change, read the difference. The
//!    same loop runs on the real `official` image in `tests/milestones/m5.rs`
//!    `t1_m5_agent_budget_on_official`.
//!
//! The scripted machine releases its outputs at fixed virtual instants and reads no host clock,
//! and every measurement starts from a reset pool ([`install_world`]), so the counts are the same
//! on every host, in CI and in any order.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard};

use pemu_api::commands::snapshot::{MACHINE_HOOKS, Store};
use pemu_api::commands::start::{self, Boot, Pool, StartArgs};
use pemu_api::commands::{inspect, scenario, screenshot, snapshot, ui};
use pemu_api::error::ApiError;
use pemu_api::instance::InstanceTable;
use pemu_api::spec::HandlerCx;

use pemu_core::hostio::{EventKind, HostEvent, HostIo, SerialStream};
use pemu_core::input::InputEvent;
use pemu_core::snap::{LivePolicy, SnapError, SnapHeader, SnapOpts, Snapshot};
use pemu_core::time::VTime;
use pemu_introspect::IntrospectError;
use pemu_introspect::freertos::{Task, TaskSnapshot, TaskState};
use pemu_introspect::lvgl::{UiNode, UiTree};
use pemu_introspect::nvs::{NvsEntry, NvsListing, NvsType, NvsValue};
use pemu_introspect::tlsf::{HeapRegion, HeapSnapshot};
use pemu_machine::machine::{At, GuestMem, InputError, Receipt as LedgerReceipt};
use pemu_machine::run::{RunLimits, RunOutcome};
use pemu_machine::stops::StopReason;
use pemu_machine::{MachineApi, SnapshotMachine};

const USAGE: &str = "usage: cargo xtask agent-budget [--verbose]";

/// Largest text output one command may produce by default.
pub const PER_OUTPUT_CHARS: usize = 4_000;

/// Largest total the reference agent loop may produce.
pub const LOOP_CHARS: usize = 8_000;

/// One measured output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measure {
    /// What produced it: `<command> example <n>`, or the loop step's label.
    pub what: String,
    /// Characters its text rendering cost.
    pub chars: usize,
}

/// What one run of the check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Every example that ran, in registry order.
    pub examples: Vec<Measure>,
    /// Every step of the reference loop, in order.
    pub reference_loop: Vec<Measure>,
    /// Examples that could not run here, each with the refusal that stopped it.
    pub skipped: Vec<(String, String)>,
    /// Budget violations, each one printable line.
    pub findings: Vec<String>,
}

impl Report {
    /// Characters the whole reference loop cost.
    #[must_use]
    pub fn loop_chars(&self) -> usize {
        self.reference_loop.iter().map(|step| step.chars).sum()
    }

    /// The largest single output of either population.
    #[must_use]
    pub fn worst(&self) -> Option<&Measure> {
        self.examples
            .iter()
            .chain(self.reference_loop.iter())
            .max_by_key(|measure| measure.chars)
    }

    /// The one line `agent-budget` prints, over budget or not.
    #[must_use]
    pub fn line(&self) -> String {
        let worst = match self.worst() {
            Some(measure) => format!("`{}` at {} characters", measure.what, measure.chars),
            None => "none".to_owned(),
        };
        format!(
            "agent-budget: {} example output(s), {} skipped, reference loop {} of {LOOP_CHARS} \
             characters, largest single output {worst} of {PER_OUTPUT_CHARS}, {} finding(s)",
            self.examples.len(),
            self.skipped.len(),
            self.loop_chars(),
            self.findings.len()
        )
    }
}

/// Entry point of `cargo xtask agent-budget`.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut verbose = false;
    for arg in args {
        match arg.as_str() {
            "--verbose" | "-v" => verbose = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let report = check();
    if verbose {
        print!("{}", render(&report));
        for (what, why) in &report.skipped {
            println!("  skip  {what}: {why}");
        }
    }
    for finding in &report.findings {
        println!("{finding}");
    }
    println!("{}", report.line());
    if report.findings.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} output(s) over the output budget",
            report.findings.len()
        ))
    }
}

/// Measures both populations and collects the violations.
#[must_use]
pub fn check() -> Report {
    let _world = world();
    let mut report = Report::default();
    measure_examples(&mut report);
    measure_reference_loop(&mut report);
    for measure in &report.examples {
        if measure.chars > PER_OUTPUT_CHARS {
            report.findings.push(format!(
                "{} renders {} characters, over the {PER_OUTPUT_CHARS}-character default output \
                 budget by {}; shape it with `Output::shaped`",
                measure.what,
                measure.chars,
                measure.chars - PER_OUTPUT_CHARS
            ));
        }
    }
    for measure in &report.reference_loop {
        if measure.chars > PER_OUTPUT_CHARS {
            report.findings.push(format!(
                "reference loop step `{}` renders {} characters, over the \
                 {PER_OUTPUT_CHARS}-character default output budget",
                measure.what, measure.chars
            ));
        }
    }
    let total = report.loop_chars();
    if total > LOOP_CHARS {
        report.findings.push(format!(
            "the reference agent loop renders {total} characters, over its \
             {LOOP_CHARS}-character budget by {}",
            total - LOOP_CHARS
        ));
    }
    report
}

/// Renders the measurements one per line, count first.
#[must_use]
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    for measure in report.examples.iter().chain(report.reference_loop.iter()) {
        let _ = writeln!(out, "{:>6}  {}", measure.chars, measure.what);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Population 1: the documented examples
// ---------------------------------------------------------------------------------------------

/// Runs every registered command's examples against a freshly started scripted instance.
fn measure_examples(report: &mut Report) {
    for spec in pemu_api::registry::commands() {
        for (index, example) in spec.examples.iter().enumerate() {
            let what = format!("{} example {index} ({})", spec.name, example.title);
            let Ok(args) = example.args_json() else {
                report
                    .skipped
                    .push((what, "the example arguments are not JSON".to_owned()));
                continue;
            };
            if let Err(why) = fresh_instance() {
                report.skipped.push((what, why));
                continue;
            }
            let mut cx = HandlerCx {};
            match (spec.handler)(&mut cx, args) {
                Ok(output) => report.examples.push(Measure {
                    what,
                    chars: output.text_chars(),
                }),
                Err(error) => report.skipped.push((what, refusal(&error))),
            }
        }
    }
}

/// The one-line reason an example did not run here.
fn refusal(error: &ApiError) -> String {
    format!("{} {}", error.code.name, error.message)
}

// ---------------------------------------------------------------------------------------------
// Population 2: the reference agent loop
// ---------------------------------------------------------------------------------------------

/// The reference agent loop (module header).
fn reference_loop() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    vec![
        (
            "start official",
            "start",
            serde_json::json!({ "fw": "official" }),
        ),
        (
            "run --until serial:/menu ready/",
            "run",
            serde_json::json!({ "until": "serial:/menu ready/", "timeout": "5s" }),
        ),
        ("ui", "ui", serde_json::json!({})),
        (
            "input down click",
            "input",
            serde_json::json!({ "button": "down", "action": "click" }),
        ),
        (
            "run --until ui:changed",
            "run",
            serde_json::json!({ "until": "ui:changed", "timeout": "5s" }),
        ),
        ("ui --diff 1", "ui", serde_json::json!({ "diff": 1 })),
    ]
}

/// Runs the reference loop and measures each step.
fn measure_reference_loop(report: &mut Report) {
    install_world();
    for (label, name, args) in reference_loop() {
        let Some(spec) = pemu_api::registry::find(name) else {
            report.findings.push(format!(
                "the reference loop names `{name}`, which is not a registered command"
            ));
            return;
        };
        let mut cx = HandlerCx {};
        match (spec.handler)(&mut cx, args) {
            Ok(output) => report.reference_loop.push(Measure {
                what: label.to_owned(),
                chars: output.text_chars(),
            }),
            Err(error) => {
                report.findings.push(format!(
                    "the reference agent loop stopped at `{label}`: {}",
                    refusal(&error)
                ));
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted world
// ---------------------------------------------------------------------------------------------

/// The pool, the command seams and the artifact store are process state, so one measurement runs
/// at a time.
fn world() -> MutexGuard<'static, ()> {
    static WORLD: Mutex<()> = Mutex::new(());
    match WORLD.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Empties the pool, installs the scripted backend factory and every command seam, and clears the
/// artifact store and the UI revision counter.
///
/// The instance table is replaced, not just emptied: [`pemu_api::instance::InstanceTable::create`]
/// never reuses an index, so the next `start` would mint a wider id, every line naming it would
/// grow, and the measured numbers would depend on how many instances the process had created.
fn install_world() {
    start::with_pool(|pool: &mut Pool| {
        for id in pool.live_ids() {
            let _ = pool.destroy(id);
        }
        *pool.table_mut() = InstanceTable::new();
        // The new table mints `p1` again, so the named snapshots and taint the previous
        // measurement's `p1` left in the pool's store go with the old table.
        pool.with_store(|store| *store = Store::default());
        // The same for the UI revisions: a re-minted `p1` starts at `ui_rev` 1 again.
        pool.with_table(|trees: &mut ui::Trees| *trees = ui::Trees::default());
        pool.set_factory(|_args| Ok(Box::new(BudgetMachine::new())));
        pool.set_artifacts_root(Some("runs".to_owned()));
    });
    inspect::set_introspectors(inspect::Introspectors {
        tasks: scripted_tasks,
        heap: scripted_heap,
        nvs: scripted_nvs,
        ui: scripted_ui,
        ..inspect::NO_INTROSPECTORS
    });
    screenshot::set_io(screenshot::ScreenshotCodec { encode, decode });
    pemu_api::artifact_io::set(pemu_api::artifact_io::ArtifactIo {
        write: write_artifact,
        read: read_artifact,
    });
    snapshot::with_seams(|seams| seams.set_hooks(MACHINE_HOOKS));
    scenario::set_io(scenario::ScenarioIo {
        read_text,
        list_files,
        write_text,
        display_path: str::to_owned,
    });
    files(BTreeMap::clear);
}

/// Resets the world and starts exactly one instance, so an example that needs one finds one and
/// an example that resolves its instance implicitly is never ambiguous.
fn fresh_instance() -> Result<(), String> {
    install_world();
    let args = StartArgs {
        fw: "official".to_owned(),
        boot: Boot::UntilUiSettled,
        ..StartArgs::default()
    };
    start::with_pool(|pool| start::start_on(pool, &args))
        .map(|_| ())
        .map_err(|error| format!("the fixture instance did not start: {}", refusal(&error)))
}

/// The artifact store the scripted host seams write into: a path to bytes map, never a file.
fn files<R>(f: impl FnOnce(&mut BTreeMap<String, Vec<u8>>) -> R) -> R {
    static FILES: Mutex<Option<BTreeMap<String, Vec<u8>>>> = Mutex::new(None);
    let mut guard = match FILES.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(guard.get_or_insert_with(BTreeMap::new))
}

fn write_artifact(path: &str, bytes: &[u8]) -> Result<String, String> {
    files(|map| map.insert(path.to_owned(), bytes.to_vec()));
    Ok(path.to_owned())
}

fn read_artifact(path: &str) -> Result<Vec<u8>, String> {
    files(|map| map.get(path).cloned()).ok_or_else(|| format!("no artifact `{path}`"))
}

fn write_text(path: &str, text: &str) -> Result<String, String> {
    write_artifact(path, text.as_bytes())
}

/// Scenario files come from the tree, so the `scenario` examples measured here are the committed
/// fixtures rather than a fixture invented for the measurement. Anything the run itself wrote is
/// served from the in-memory store first.
fn read_text(path: &str) -> Result<String, String> {
    if let Ok(bytes) = read_artifact(path) {
        return String::from_utf8(bytes).map_err(|_| format!("`{path}` is not UTF-8"));
    }
    std::fs::read_to_string(crate::util::workspace_root().join(path))
        .map_err(|err| format!("`{path}` cannot be read: {err}"))
}

fn list_files(root: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = files(|map| map.keys().cloned().collect());
    let dir = crate::util::workspace_root().join(root);
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if entry.path().is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            out.push(format!("{}/{name}", root.trim_end_matches('/')));
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// A codec that is not PNG: what is measured here is characters of text, so the artifact format
/// only has to round-trip. `png` is a host crate `pemu-api` may not name.
fn encode(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(8 + rgb.len());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(rgb);
    Ok(out)
}

fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if bytes.len() < 8 {
        return Err("an image artifact is at least 8 bytes".to_owned());
    }
    let width = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let height = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    Ok((width, height, bytes[8..].to_vec()))
}

/// The scripted machine's snapshot half: an empty snapshot, a restore that changes
/// nothing and a fork that starts the script again, which is all the budget examples need.
impl SnapshotMachine for BudgetMachine {
    fn snapshot(&self, _opts: SnapOpts) -> Result<Snapshot, SnapError> {
        Ok(Snapshot::new(SnapHeader::new()))
    }

    fn redact(
        &self,
        snapshot: &mut Snapshot,
    ) -> Result<pemu_machine::snapshot::Redaction, SnapError> {
        snapshot.header.exported = true;
        snapshot.header.redacted = true;
        Ok(pemu_machine::snapshot::Redaction::default())
    }

    fn restore(&mut self, _snapshot: &Snapshot) -> Result<(), SnapError> {
        Ok(())
    }

    fn fork(&self, _live: LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        Ok(Box::new(BudgetMachine::new()))
    }

    fn state_hash(&self) -> [u8; 32] {
        [0; 32]
    }
}

/// The official menu as the LVGL walker would report it: a screen and its eight rows, which is the
/// tree size the output budget was written against.
fn scripted_ui(_machine: &mut dyn MachineApi, rev: u64) -> Result<UiTree, IntrospectError> {
    const ROWS: [&str; 8] = [
        "FoloToy",
        "Display",
        "Button",
        "Wi-Fi",
        "Bluetooth",
        "Audio",
        "Battery",
        "Low Power",
    ];
    let mut nodes = vec![UiNode {
        obj: 0x3fcc_0000,
        reference: "e1".to_owned(),
        class: "obj".to_owned(),
        class_chain: vec!["lv_obj".to_owned()],
        parent: 0,
        depth: 0,
        children: Vec::new(),
        w: 240,
        h: 320,
        ..UiNode::default()
    }];
    for (index, label) in ROWS.iter().enumerate() {
        let obj = 0x3fcc_0100 + (index as u32) * 0x40;
        nodes[0].children.push(obj);
        nodes.push(UiNode {
            obj,
            reference: format!("e{}", index + 2),
            class: "label".to_owned(),
            class_chain: vec!["lv_label".to_owned(), "lv_obj".to_owned()],
            parent: 0x3fcc_0000,
            depth: 1,
            text: Some((*label).to_owned()),
            x: 10,
            y: (index as i32) * 34,
            w: 220,
            h: 30,
            ..UiNode::default()
        });
    }
    Ok(UiTree {
        rev,
        nodes,
        display: 0x3fcd_0000,
        hor_res: 240,
        ver_res: 320,
        screen: 0x3fcc_0000,
        warnings: Vec::new(),
    })
}

fn scripted_tasks(_machine: &mut dyn MachineApi) -> Result<TaskSnapshot, IntrospectError> {
    const NAMES: [&str; 5] = ["main", "IDLE", "esp_timer", "lvgl_port", "tiT"];
    Ok(TaskSnapshot {
        tasks: NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| Task {
                tcb: 0x3fca_0000 + (index as u32) * 0x100,
                name: (*name).to_owned(),
                state: if index == 0 {
                    TaskState::Running
                } else {
                    TaskState::Ready
                },
                priority: index as u32,
                base_priority: index as u32,
                stack_base: 0x3fcb_0000 + (index as u32) * 0x1000,
                stack_end: 0x3fcb_0fff + (index as u32) * 0x1000,
                stack_bytes: 4096,
                stack_free_bytes: 1536,
                top_of_stack: 0x3fcb_0800 + (index as u32) * 0x1000,
                event_list: 0,
                blocked_on: None,
                indefinite: false,
                mutex_wait: None,
            })
            .collect(),
        tick: Some(1234),
        count: Some(NAMES.len() as u32),
        current: Some(0x3fca_0000),
        idle: vec![0x3fca_0100],
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

/// One namespace with one plain value and one credential the walker already redacted:
/// a listing this check can measure without a secret ever entering the process.
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
                value: NvsValue::Redacted,
                credential: true,
            },
        ],
    })
}

/// A scripted machine that boots to a settled menu and redraws after a click, at fixed virtual
/// instants.
///
/// **Why not `pemu_testkit::MockMachine`**: the mock keeps its scripted output on the
/// side and hands out an empty `HostIo`, and every `run --until` matcher reads the real rings. This
/// one releases into `HostIo`, the way `pemu-api`'s own command tests do.
struct BudgetMachine {
    vt: VTime,
    io: HostIo,
    pending: Vec<(VTime, Scripted)>,
    journal: Vec<InputEvent>,
}

/// One scripted output.
enum Scripted {
    /// A console line; the newline is added.
    Line(&'static str),
    /// An event-ring entry.
    Event(EventKind),
}

impl BudgetMachine {
    fn new() -> BudgetMachine {
        let mut pending = Vec::new();
        for (ms, line) in [
            (5u64, "I (5) boot: ESP-IDF 2nd stage bootloader"),
            (40, "I (40) main_task: Calling app_main()"),
            (90, "I (90) bsp_display: panel init done"),
            (260, "I (260) pk_app: ready"),
        ] {
            pending.push((VTime::from_ms(ms), Scripted::Line(line)));
        }
        // `start` boots until the display settles; the menu line lands after it, so the first
        // `run` of the reference loop has something ahead of its cursor to wait for.
        pending.push((VTime::from_ms(100), Scripted::Event(EventKind::UiSettled)));
        pending.push((
            VTime::from_ms(200),
            Scripted::Line("I (200) pk_app: menu ready"),
        ));
        BudgetMachine {
            vt: VTime(0),
            io: HostIo::new(8192),
            pending,
            journal: Vec::new(),
        }
    }

    /// Releases everything due at or before `at`, in instant then insertion order.
    fn release(&mut self, at: VTime) {
        let mut due = Vec::new();
        let mut keep = Vec::new();
        for (vt, out) in self.pending.drain(..) {
            if vt.0 <= at.0 {
                due.push((vt, out));
            } else {
                keep.push((vt, out));
            }
        }
        self.pending = keep;
        due.sort_by_key(|(vt, _)| *vt);
        for (vt, out) in due {
            match out {
                Scripted::Line(text) => {
                    let mut bytes = text.as_bytes().to_vec();
                    bytes.push(b'\n');
                    self.io.serial_write(SerialStream::UsjTx, &bytes, vt);
                }
                Scripted::Event(kind) => self.io.events.emit(HostEvent { kind, vt, arg: 0 }),
            }
        }
    }
}

impl MachineApi for BudgetMachine {
    fn run(&mut self, lim: RunLimits) -> RunOutcome {
        let until = match (lim.until, lim.max_insns) {
            (Some(t), _) => VTime(t.0.max(self.vt.0)),
            (None, Some(n)) => VTime(self.vt.0.saturating_add(VTime::from_us(n.div_ceil(160)).0)),
            (None, None) => self.vt,
        };
        // Releases first, so an output scheduled at the deadline is visible when `run` returns.
        self.release(until);
        let insns = (until.0.saturating_sub(self.vt.0) / VTime::from_us(1).0).saturating_mul(160);
        self.vt = until;
        RunOutcome {
            reason: if lim.until.is_some() {
                StopReason::Until
            } else {
                StopReason::MaxInsns
            },
            vt: self.vt,
            insns,
            ff_insns: 0,
            idle_ps: 0,
        }
    }

    fn input(&mut self, _at: At, ev: InputEvent) -> Result<u64, InputError> {
        // Releasing a button redraws the menu a little later, which is what the fifth step of the
        // reference loop waits for.
        if matches!(ev, InputEvent::Button { down: false, .. }) {
            let at = VTime(self.vt.0.saturating_add(VTime::from_ms(20).0));
            self.pending.push((at, Scripted::Event(EventKind::Frame)));
        }
        match &ev {
            InputEvent::UsbCable { plugged } => self.io.usj_ctrl.set_cable(*plugged),
            InputEvent::UsbClient { open } => self.io.usj_ctrl.set_client_open(*open),
            InputEvent::UsbLine { dtr, rts } => self.io.usj_ctrl.set_line_state(*rts, *dtr),
            InputEvent::SerialIn { data, .. } => {
                self.io.usj_rx.push(data);
            }
            _ => {}
        }
        self.journal.push(ev);
        Ok(self.journal.len() as u64 - 1)
    }

    fn io(&mut self) -> &mut HostIo {
        &mut self.io
    }

    fn now(&self) -> VTime {
        self.vt
    }

    fn guest_mem(&mut self) -> GuestMem<'_> {
        unreachable!("the budget check reads the guest through the introspector seam")
    }

    /// A budget double over no input: untainted.
    fn is_tainted(&self) -> bool {
        false
    }
    fn receipt(&mut self) -> LedgerReceipt {
        LedgerReceipt::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference agent loop runs end to end and fits its 8,000-character budget.
    #[test]
    fn the_reference_loop_runs_and_fits_its_budget() {
        let report = check();
        assert_eq!(
            report.reference_loop.len(),
            reference_loop().len(),
            "every step of the reference loop ran; findings: {:?}",
            report.findings
        );
        assert!(
            report.loop_chars() > 0,
            "a loop that produced no text measured nothing"
        );
        assert!(
            report.loop_chars() <= LOOP_CHARS,
            "the reference loop is {} characters:\n{}",
            report.loop_chars(),
            render(&report)
        );
    }

    /// Every default text output is at most 4,000 characters.
    #[test]
    fn every_measured_output_fits_the_per_output_budget() {
        let report = check();
        assert!(
            report.findings.is_empty(),
            "{}\n{}",
            report.findings.join("\n"),
            render(&report)
        );
        assert!(
            !report.examples.is_empty(),
            "no documented example ran; skips: {:?}",
            report.skipped
        );
    }

    /// A skip is a reason, not a silence: every example the check could not measure names the
    /// refusal that stopped it.
    #[test]
    fn an_example_that_cannot_run_here_is_reported_with_its_reason() {
        let report = check();
        for (what, why) in &report.skipped {
            assert!(!why.is_empty(), "`{what}` was skipped without a reason");
        }
    }

    /// The official menu as the smoke scenario walks it on the real firmware, one state per
    /// `ui` revision of the fixture instance: revision 1 is the settled
    /// menu with Display selected, 2 follows DOWN (Button selected), 3 follows OK (the Button
    /// page), and 4 follows the long OK (the menu again, Button still selected). The selection is
    /// the local background `#ffd928` of a card.
    fn smoke_ui(_machine: &mut dyn MachineApi, rev: u64) -> Result<UiTree, IntrospectError> {
        const SCREEN: u32 = 0x3fcc_0000;
        let yellow = pemu_introspect::lvgl::Color {
            r: 0xff,
            g: 0xd9,
            b: 0x28,
        };
        let mut nodes = vec![UiNode {
            obj: SCREEN,
            reference: "e1".to_owned(),
            class: "obj".to_owned(),
            w: 240,
            h: 320,
            ..UiNode::default()
        }];
        let push = |nodes: &mut Vec<UiNode>, node: UiNode| {
            let obj = SCREEN + 0x40 * nodes.len() as u32;
            let reference = format!("e{}", nodes.len() + 1);
            nodes.push(UiNode {
                obj,
                reference,
                ..node
            });
            obj
        };
        let label = |parent: u32, depth: u32, text: &str, y: i32| UiNode {
            class: "label".to_owned(),
            parent,
            depth,
            text: Some(text.to_owned()),
            x: 20,
            y,
            w: 80,
            h: 20,
            ..UiNode::default()
        };
        if rev == 3 {
            push(&mut nodes, label(SCREEN, 1, "Button", 10));
            push(&mut nodes, label(SCREEN, 1, "Press a key", 60));
        } else {
            let selected = if rev == 1 { "Display" } else { "Button" };
            push(&mut nodes, label(SCREEN, 1, "FoloToy", 10));
            for (index, card) in ["Display", "Button", "Wi-Fi", "BLE", "Low Power"]
                .iter()
                .enumerate()
            {
                let y = 52 + 47 * index as i32;
                let obj = push(
                    &mut nodes,
                    UiNode {
                        class: "obj".to_owned(),
                        parent: SCREEN,
                        depth: 1,
                        x: 11,
                        y,
                        w: 102,
                        h: 40,
                        bg: (*card == selected).then_some(yellow),
                        ..UiNode::default()
                    },
                );
                push(&mut nodes, label(obj, 2, card, y + 10));
            }
        }
        Ok(UiTree {
            rev,
            nodes,
            display: 0x3fcd_0000,
            hor_res: 240,
            ver_res: 320,
            screen: SCREEN,
            warnings: Vec::new(),
        })
    }

    /// Replaces the budget's static tree with [`smoke_ui`] for one smoke test.
    fn install_smoke_ui() {
        inspect::set_introspectors(inspect::Introspectors {
            tasks: scripted_tasks,
            heap: scripted_heap,
            nvs: scripted_nvs,
            ui: smoke_ui,
            ..inspect::NO_INTROSPECTORS
        });
    }

    /// As far as a scripted machine reaches it: the committed menu smoke scenario runs end to end,
    /// its presses delivered and every `ui.expect` asserted on the tree the firmware shows after
    /// them ([`smoke_ui`]), and every step passes.
    ///
    /// What it does not assert is `s_sel == 1` and `s_active == 1` (no `var:` reads in
    /// this build) and the ten identical artifact hashes, which need the real firmware:
    /// `tests/milestones/m5.rs` `t1_m5_official_menu_smoke` asserts those on the corpus.
    #[test]
    fn the_committed_menu_smoke_scenario_runs_end_to_end() {
        let _world = world();
        fresh_instance().expect("the fixture instance starts");
        install_smoke_ui();
        let spec = pemu_api::registry::find("scenario").expect("`scenario` is registered");
        let mut cx = HandlerCx {};
        let args = serde_json::json!({ "file": "tests/scenarios/official-menu-smoke.yaml" });
        let output = (spec.handler)(&mut cx, args).expect("the scenario runs");
        assert_eq!(
            output.json["status"],
            "pass",
            "{}\n{}",
            output.text,
            serde_json::to_string_pretty(&output.json).unwrap_or_default()
        );
    }

    /// As far as a scripted machine reaches it: the quoted glob expands to every committed
    /// fixture, the batch runs them in one call and one JUnit file is written.
    #[test]
    fn the_committed_suite_runs_from_one_glob_and_writes_one_junit_file() {
        let _world = world();
        fresh_instance().expect("the fixture instance starts");
        install_smoke_ui();
        let spec = pemu_api::registry::find("scenario").expect("`scenario` is registered");
        let mut cx = HandlerCx {};
        let args = serde_json::json!({
            "file": "tests/scenarios/*.yaml",
            "jobs": 8,
            "junit": "junit.xml",
        });
        let output = (spec.handler)(&mut cx, args).expect("the batch runs");
        let scenarios = output.json["scenarios"]
            .as_array()
            .expect("a batch reports one entry per scenario");
        assert!(
            scenarios.len() >= 3,
            "the glob found {} scenario(s): {}",
            scenarios.len(),
            output.text
        );
        assert_eq!(output.json["junit_path"], "junit.xml");
        let junit = read_text("junit.xml").expect("the JUnit file was written");
        assert!(junit.starts_with("<?xml"), "{junit}");
        assert!(junit.contains("<testsuite "), "{junit}");
        // Every `official` scenario passes on the fixture. `ble-gatt` names `image:
        // pk` and drives the `ble` radio module, which a scripted `official` instance does not
        // have, so the runner refuses it at `setup` by its image before any step runs; that is
        // the precondition check working, and `tests/milestones/m8.rs` runs the file on `pk`.
        for scenario in scenarios {
            if scenario["name"] == "ble-gatt" {
                assert_eq!(scenario["status"], "error", "{scenario}");
                let setup = &scenario["steps"][0];
                assert_eq!(setup["key"], "setup", "{scenario}");
                assert!(
                    setup["error"]["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("`image: pk`")),
                    "{scenario}"
                );
            } else {
                assert_eq!(scenario["status"], "pass", "{}\n{junit}", output.text);
            }
        }
    }

    /// The measurement does not depend on what ran before it.
    #[test]
    fn the_measurement_is_deterministic() {
        let first = check();
        let second = check();
        assert_eq!(first.reference_loop, second.reference_loop);
        assert_eq!(first.examples.len(), second.examples.len());
        assert_eq!(first.skipped, second.skipped);
    }

    /// Every documented example of the six Core commands in `OWNED` **runs** here against the
    /// scripted instance, and none is quietly skipped.
    ///
    /// `commands/start.rs::every_core_example_runs_against_a_scripted_instance` skips these six:
    /// its `TestMachine` installs none of the introspection, artifact, snapshot-hook or
    /// scenario-IO seams they read through, and `install_world` installs all four. Without this
    /// test the rule that examples run in CI against a fixture instance would silently lose them.
    /// Examples that cannot run are pinned with their refusal, so a new one going dark fails.
    #[test]
    fn every_seam_command_example_runs_against_the_scripted_instance() {
        const OWNED: [&str; 6] = ["env", "snapshot", "ui", "screenshot", "inspect", "scenario"];
        /// `command example <n>`, and why that example cannot reach a verdict in this build.
        const EXPECTED_SKIPS: [(&str, &str); 31] = [
            // `--diff` needs a kept revision, and a fresh instance has exactly one.
            ("ui example 1", "E_USAGE"),
            // Both read an artifact an earlier example would have had to write; each example
            // gets its own reset world, so neither file exists.
            ("snapshot example 4", "E_STATE"),
            ("screenshot example 2", "E_STATE"),
            // The USB Serial/JTAG endpoints are sockets, threads and a pty that only
            // `pemu_host::endpoints::install` provides; the scripted world installs no
            // native host, so both examples are the documented native-host refusal.
            ("endpoint example 0", "E_HOST_UNSUPPORTED"),
            ("endpoint example 1", "E_HOST_UNSUPPORTED"),
            // The NFC commands read the card out of the machine's `board.world` snapshot
            // section, and the scripted instance is a double that takes no snapshot, so every
            // `nfc_tag` and `nfc_tap` example is its snapshot refusal. They run against a real
            // machine in `pemu-api` (`nfc_tap::tests`, `nfc_tag::tests`) and in
            // `tests/milestones/m10.rs`.
            ("nfc_tag example 0", "E_SNAPSHOT"),
            ("nfc_tag example 1", "E_SNAPSHOT"),
            ("nfc_tap example 0", "E_SNAPSHOT"),
            ("nfc_tap example 1", "E_SNAPSHOT"),
            ("nfc_tap example 2", "E_SNAPSHOT"),
            // The Device group answers only when a host installed a planner, which
            // happens only with `--allow-device`; the scripted world never passes it, so all three
            // examples are the documented refusal that names the flag.
            ("plan_flash example 0", "E_PLAN_REFUSED"),
            ("flash_device example 0", "E_PLAN_REFUSED"),
            ("device_boot_check example 0", "E_PLAN_REFUSED"),
            // The three BLE commands read the scripted central out of the bound
            // `ble` radio module (`MachineApi::radio_module_state`), and the scripted
            // instance is a double that binds no module, so every example is the documented
            // "no bound BLE module" refusal. They run against a real machine in `pemu-api`
            // (`ble_scan::tests` and friends) and on `pk` in `tests/milestones/m8.rs`.
            ("ble_scan example 0", "E_STATE"),
            ("ble_scan example 1", "E_STATE"),
            ("ble_scan example 2", "E_STATE"),
            ("ble_connect example 0", "E_STATE"),
            ("ble_connect example 1", "E_STATE"),
            ("ble_connect example 2", "E_STATE"),
            ("ble_gatt example 0", "E_STATE"),
            ("ble_gatt example 1", "E_STATE"),
            ("ble_gatt example 2", "E_STATE"),
            ("ble_gatt example 3", "E_STATE"),
            ("ble_gatt example 4", "E_STATE"),
            // The three Wi-Fi commands read the scripted air, the virtual LAN and its capture
            // out of the bound `wifi` radio module, which the double does not bind, so
            // each example is the "no bound Wi-Fi module" refusal (`wifi_ap example 2` sees it
            // through the `remove` path). They run against a real machine in `pemu-api`
            // (`wifi_ap::tests`, `net_http::tests`, `net_capture::tests`) and on the
            // `probe_wifi_http` guest in `tests/milestones/m12.rs`.
            ("wifi_ap example 0", "E_STATE"),
            ("wifi_ap example 2", "E_STATE"),
            ("net_http example 0", "E_STATE"),
            ("net_http example 1", "E_STATE"),
            ("net_http example 2", "E_STATE"),
            ("net_capture example 0", "E_STATE"),
            ("net_capture example 1", "E_STATE"),
        ];

        let report = check();
        let skipped: Vec<&str> = report
            .skipped
            .iter()
            .map(|(what, _)| what.as_str())
            .collect();
        for (what, code) in EXPECTED_SKIPS {
            let (_, why) = report
                .skipped
                .iter()
                .find(|(skip, _)| skip.starts_with(what))
                .unwrap_or_else(|| {
                    panic!("`{what}` no longer skips; drop it from EXPECTED_SKIPS: {skipped:?}")
                });
            assert!(
                why.contains(code),
                "`{what}` now skips for a different reason than {code}: {why}"
            );
        }
        assert_eq!(
            report.skipped.len(),
            EXPECTED_SKIPS.len(),
            "an example stopped running; every skip must be pinned above: {skipped:?}"
        );

        for name in OWNED {
            let spec = pemu_api::registry::find(name)
                .unwrap_or_else(|| panic!("`{name}` is a registered command"));
            assert!(
                !spec.examples.is_empty(),
                "`{name}` documents no example, so nothing was measured"
            );
            for index in 0..spec.examples.len() {
                let what = format!("{name} example {index}");
                let ran = report
                    .examples
                    .iter()
                    .any(|measure| measure.what.starts_with(&what));
                let pinned = EXPECTED_SKIPS.iter().any(|(skip, _)| *skip == what);
                assert!(
                    ran || pinned,
                    "`{what}` neither ran nor is a pinned skip; the shared guard in \
                     `commands/start.rs` relies on this test to execute it: {skipped:?}"
                );
            }
        }
    }

    /// Every documented example of the opt-in `power` group (`power`, `usb`) runs end to end
    /// against the scripted instance and fits the per-output budget, and the default Core tool list
    /// stays without them.
    #[test]
    fn every_power_group_example_runs_against_the_scripted_instance() {
        use pemu_api::spec::CapsGroup;

        let report = check();
        for name in ["power", "usb"] {
            let spec = pemu_api::registry::find(name)
                .unwrap_or_else(|| panic!("`{name}` is a registered command"));
            assert_eq!(
                spec.group,
                CapsGroup::Power,
                "`{name}` is in the power group"
            );
            assert!(!spec.examples.is_empty(), "`{name}` documents no example");
            for index in 0..spec.examples.len() {
                let what = format!("{name} example {index}");
                let measure = report
                    .examples
                    .iter()
                    .find(|measure| measure.what.starts_with(&what))
                    .unwrap_or_else(|| panic!("`{what}` did not run: {:?}", report.skipped));
                assert!(
                    measure.chars > 0 && measure.chars <= PER_OUTPUT_CHARS,
                    "`{what}` printed {} characters",
                    measure.chars
                );
            }
        }
        let core = crate::docs::registry_commands().expect("the command registry");
        assert!(
            core.iter()
                .filter(|command| command.spec.group == CapsGroup::Core)
                .all(|command| !["power", "usb"].contains(&command.spec.name)),
            "`power` and `usb` stay out of the default tool list"
        );
    }

    /// The Core tool list fits inside the tool-list budget, and every one of the thirteen
    /// always-on Core commands is actually in `CapsGroup::Core`, which is what makes the measured
    /// number the complete one.
    #[test]
    fn the_complete_core_set_fits_the_tool_list_budget() {
        use pemu_api::spec::CapsGroup;

        let commands = crate::docs::registry_commands().expect("the command registry");
        let core: Vec<_> = commands
            .iter()
            .copied()
            .filter(|command| command.spec.group == CapsGroup::Core)
            .collect();
        let names: Vec<&str> = core.iter().map(|command| command.spec.name).collect();
        for declared in [
            "start",
            "stop",
            "status",
            "input",
            "run",
            "serial",
            "ui",
            "screenshot",
            "inspect",
            "snapshot",
            "env",
            "clock",
            "scenario",
        ] {
            assert!(
                names.contains(&declared),
                "`{declared}` is an always-on Core command, but it is not in CapsGroup::Core: \
                 {names:?}"
            );
        }
        let bytes = crate::docs::list_bytes(&core);
        assert!(
            bytes <= crate::docs::CORE_BUDGET_BYTES,
            "the complete Core tool list is {bytes} bytes, over the {}-byte budget; \
             raising it needs a measured reason",
            crate::docs::CORE_BUDGET_BYTES
        );
    }

    /// `install_world` replaces the instance table, so every reset world mints the same first id
    /// and the measured text cannot widen.
    #[test]
    fn install_world_resets_the_never_reused_instance_counter() {
        let _world = world();
        let mut ids = Vec::new();
        for _ in 0..3 {
            fresh_instance().expect("the fixture instance started");
            let live = start::with_pool(|pool| pool.live_ids());
            assert_eq!(live.len(), 1, "one instance per reset world");
            ids.push(live[0].to_string());
        }
        assert_eq!(
            ids,
            vec![ids[0].clone(), ids[0].clone(), ids[0].clone()],
            "a reset world always mints the same first id, so the measured text cannot widen"
        );
    }
}
