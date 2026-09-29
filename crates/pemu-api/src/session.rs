//! One running instance: its machine, output cursors, receipt folding and the mapping of a stop to
//! a fault. Nothing in a session output is an absolute path.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pemu_core::hostio::SerialStream;
use pemu_core::time::VTime;
use pemu_machine::run::{RunLimits, RunOutcome};
use pemu_machine::stops::{StopReason, StopSet};
use pemu_machine::{MachineApi, SnapshotMachine};

use crate::args::usage;
use crate::error::{ApiError, E_STATE};
use crate::instance::InstanceId;
use crate::lease::LeaseTicket;
use crate::matchers::str_enum;
use crate::pool::{HostClock, PoolReach, PoolTables, ReachSlot, ThreadQosReader};
use crate::receipt::{Determinism, EfuseSource, Receipt};
use crate::shape::Cursor;

use crate::commands::snapshot::{Store, StoreHandle};

str_enum! {
    pub enum ClockMode {
        /// The default for agents and CI.
        Deterministic = "deterministic",
        /// The default for the web UI.
        Realtime = "realtime",
    }
}

/// 0.05x to 64x, or `max`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Speed {
    Max,
    /// Thousandths, so the value stays exact across a JSON round trip.
    Milli(u32),
}

impl Speed {
    pub const MIN_MILLI: u32 = 50;
    pub const MAX_MILLI: u32 = 64_000;

    pub fn parse(value: &serde_json::Value) -> Result<Speed, ApiError> {
        match value {
            serde_json::Value::String(s) if s == "max" => Ok(Speed::Max),
            serde_json::Value::Number(n) => {
                let milli = n
                    .as_f64()
                    .map(|f| (f * 1000.0) as i64)
                    .filter(|m| {
                        *m >= i64::from(Speed::MIN_MILLI) && *m <= i64::from(Speed::MAX_MILLI)
                    })
                    .ok_or_else(|| usage("speed", "expected a factor from 0.05 to 64, or `max`"))?;
                Ok(Speed::Milli(milli as u32))
            }
            _ => Err(usage(
                "speed",
                "expected a factor from 0.05 to 64, or `max`",
            )),
        }
    }

    pub fn to_json(self) -> serde_json::Value {
        match self {
            Speed::Max => serde_json::Value::String("max".to_owned()),
            Speed::Milli(milli) => match serde_json::Number::from_f64(f64::from(milli) / 1000.0) {
                Some(n) => serde_json::Value::Number(n),
                None => serde_json::Value::String("max".to_owned()),
            },
        }
    }

    pub fn as_text(self) -> String {
        match self {
            Speed::Max => "max".to_owned(),
            Speed::Milli(milli) => format!("{}.{:03}x", milli / 1000, milli % 1000),
        }
    }
}

/// One live instance: its machine and the registry-side state the core commands keep beside it.
pub struct Session {
    pub id: InstanceId,
    pub label: String,
    /// Display form (`fw_display`): a corpus id as written, a path by file name only.
    pub fw: String,
    /// Part of run identity.
    pub seed: u64,
    pub mode: ClockMode,
    pub speed: Speed,
    pub idle_skip: bool,
    pub deterministic_so_far: bool,
    pub ticket: Option<LeaseTicket>,
    pub insns: u64,
    pub host_clock: Option<HostClock>,
    pub(crate) thread_qos: Option<ThreadQosReader>,
    pub(crate) host_qos: Option<&'static str>,
    /// Cursors survive resets.
    pub(crate) cursors: [Cursor; SerialStream::ALL.len()],
    pub(crate) event_cursor: u64,
    /// Console bytes journaled for `usj_rx` and the instant, as the ring head they bring it to.
    pub(crate) usj_journaled: (VTime, u64),
    pub mic: crate::commands::mic_set::MicState,
    /// The last run ended in `StopReason::Deadlock`: the hart is in WFI and only input can wake it.
    /// The next run that moves on clears it.
    pub waiting_for_input: bool,
    /// The task-level deadlock the last slice found: FreeRTOS tasks each waiting forever for a
    /// mutex the next holds. Time-advancing commands end as `E_DEADLOCK` on it.
    pub task_deadlock: Option<pemu_introspect::freertos::DeadlockReport>,
    /// Tripwires this instance's runs stopped at, once each, in firing order. Kept here because a
    /// hit exists only in the `RunOutcome`; the machine records only the tripwires it armed.
    pub(crate) tripwires_hit: Vec<String>,
    /// The state came from a redacted export, so the guest runs on factory NVS and a synthetic
    /// cardid and every receipt says `redacted`, until an unredacted restore.
    pub redacted: bool,
    /// A handle, because the session runs checked out of the pool and its receipt still reads the
    /// taint.
    pub(crate) store: StoreHandle,
    pub(crate) tables: PoolTables,
    /// Set by [`cancel`] when the host ends the instance. Slice loops read it between slices so a
    /// long call returns promptly; the pool keeps a clone to reach it while the session is checked
    /// out.
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) backend: Box<dyn SnapshotMachine + Send>,
}

impl Session {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn machine(&mut self) -> &mut dyn MachineApi {
        self.backend.as_mut()
    }

    pub fn snapshot_machine(&mut self) -> &mut dyn SnapshotMachine {
        self.backend.as_mut()
    }

    pub fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        self.store.with(f)
    }

    /// A separate handle, so holding it does not borrow the session.
    pub fn store(&self) -> StoreHandle {
        self.store.clone()
    }

    pub fn with_table<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: Default + Send + 'static,
    {
        self.tables.with(f)
    }

    pub fn tables(&self) -> PoolTables {
        self.tables.clone()
    }

    pub fn reach(&self) -> Option<PoolReach> {
        self.tables.with(|slot: &mut ReachSlot| slot.0.clone())
    }

    /// The same session around a machine `wrap` built from this one's, so a host can interpose on
    /// every run (the `endpoint --clock agent` transport).
    pub fn with_backend(
        self,
        wrap: impl FnOnce(Box<dyn SnapshotMachine + Send>) -> Box<dyn SnapshotMachine + Send>,
    ) -> Session {
        Session {
            backend: wrap(self.backend),
            ..self
        }
    }

    /// `None` before the first slice or when the process installed no reader.
    pub fn host_qos(&self) -> Option<&'static str> {
        self.host_qos
    }

    /// For a thread that runs this machine outside [`Session::run_until`]; `class` must be read on
    /// that thread.
    pub fn note_host_qos(&mut self, class: &'static str) {
        self.host_qos = Some(class);
    }

    fn read_host_qos(&mut self) {
        if let Some(read) = self.thread_qos {
            self.host_qos = Some(read());
        }
    }

    pub fn now(&self) -> VTime {
        self.backend.now()
    }

    pub fn cursor(&self, stream: SerialStream) -> Cursor {
        self.cursors[stream.index()]
    }

    pub fn set_cursor(&mut self, stream: SerialStream, cursor: Cursor) {
        self.cursors[stream.index()] = cursor;
    }

    /// The ring's free space less what this session journaled at this instant and the machine has
    /// not applied yet (journaled input applies at the next slice).
    pub fn usj_room(&mut self) -> usize {
        let now = self.backend.now();
        let ring = &self.backend.io().usj_rx;
        let (at, target) = self.usj_journaled;
        let pending = if at == now {
            usize::try_from(target.saturating_sub(ring.head())).unwrap_or(usize::MAX)
        } else {
            0
        };
        ring.free().saturating_sub(pending)
    }

    pub fn note_usj_journaled(&mut self, n: usize) {
        let now = self.backend.now();
        let head = self.backend.io().usj_rx.head();
        let (at, target) = self.usj_journaled;
        let base = if at == now { target.max(head) } else { head };
        self.usj_journaled = (now, base + n as u64);
    }

    pub fn event_cursor(&self) -> u64 {
        self.event_cursor
    }

    pub fn set_event_cursor(&mut self, cursor: u64) {
        self.event_cursor = cursor;
    }

    /// The receipt as it stands. `MachineApi::receipt` drains the machine's ledger into it; the
    /// types stay apart so the ledger can change without changing the response.
    pub fn receipt(&mut self) -> Receipt {
        let vt_us = self.backend.now().as_us();
        let drained = self.backend.receipt();
        let mut receipt = Receipt {
            vt_us,
            insns: self.insns,
            // The machine's journal class, or `Live` when a realtime clock mode already made it
            // live.
            determinism: drained
                .determinism
                .map_or(Determinism::Deterministic, |class| match class {
                    pemu_core::journal::Determinism::Deterministic => Determinism::Deterministic,
                    pemu_core::journal::Determinism::Replayable => Determinism::Replayable,
                    pemu_core::journal::Determinism::Live => Determinism::Live,
                })
                .max(if self.deterministic_so_far {
                    Determinism::Deterministic
                } else {
                    Determinism::Live
                }),
            redacted: self.redacted,
            // A backend under no `MachineConfig` (a mock) reports no profile, eFuse source, caveat
            // classes or CPI and keeps the defaults.
            profile: drained
                .profile
                .map_or_else(|| Receipt::default().profile, |id| id.as_str().to_string()),
            efuse: drained
                .efuse
                .map_or_else(|| Receipt::default().efuse, EfuseSource::from),
            // The one place taint is answered. `pemu_host::boot_cache` picks its store from it, so
            // a wrong `false` writes a tainted entry to disk.
            tainted: drained.tainted || self.with_store(|store| store.is_tainted(self.id)),
            // Defaulted, the caveat chain would only ever answer `pass`.
            classes_touched: crate::receipt::ClassesTouched {
                c: drained.classes_touched.c,
                u: drained.classes_touched.u,
            },
            unmodeled_first_touch: drained.unmodeled_first_touch,
            timing_lint: drained.timing_lint,
            cpi_milli: drained
                .cpi_milli
                .unwrap_or_else(|| Receipt::default().cpi_milli),
            // Saturates rather than falling back to the default 0.
            journal_len: drained.journal_len.map_or_else(
                || Receipt::default().journal_len,
                |n| u32::try_from(n).unwrap_or(u32::MAX),
            ),
            // A tripwire always stops the run, so a hit exists only as a `StopReason` this session
            // saw.
            hle: crate::receipt::HleReceipt {
                tripwires_hit: self.tripwires_hit.clone(),
                ..crate::receipt::HleReceipt::default()
            },
            ..Receipt::default()
        };
        if let Some(binding) = drained.binding {
            binding_into_receipt(&binding, &mut receipt);
        }
        if let Some(faults) = drained.fault_counters {
            fault_counters_into_receipt(faults, &mut receipt);
        }
        heap_ledger_into_receipt(self.backend.heap_ledger(), &mut receipt);
        if let Some(class) = self.host_qos {
            receipt.extra.insert("host_qos".to_string(), class.into());
        }
        receipt
    }

    pub fn run_until(&mut self, until: VTime) -> RunOutcome {
        self.run_until_with(until, &StopSet::default())
    }

    /// With a stop set armed for this slice alone (the write watches of a `var:` matcher). The
    /// machine unmarks every page when the call returns, so a watch cannot outlive the slice.
    pub fn run_until_with(&mut self, until: VTime, stops: &StopSet) -> RunOutcome {
        let outcome = self.backend.run(RunLimits {
            until: Some(until),
            max_insns: None,
            stops: stops.clone(),
        });
        self.insns = self.insns.saturating_add(outcome.insns);
        self.waiting_for_input = outcome.reason == StopReason::Deadlock;
        self.note_tripwire(&outcome.reason);
        self.read_host_qos();
        crate::commands::run::observe_slice(self);
        self.watch_deadlock();
        outcome
    }

    /// The `clock step N` form.
    pub fn run_insns(&mut self, max_insns: u64) -> RunOutcome {
        let outcome = self.backend.run(RunLimits {
            until: None,
            max_insns: Some(max_insns),
            stops: StopSet::default(),
        });
        self.insns = self.insns.saturating_add(outcome.insns);
        self.waiting_for_input = outcome.reason == StopReason::Deadlock;
        self.note_tripwire(&outcome.reason);
        self.read_host_qos();
        crate::commands::run::observe_slice(self);
        self.watch_deadlock();
        outcome
    }

    /// Named `<kind>:<symbol or register>` from the stop's `TripReport`. The caller address is in
    /// the `E_TRIPWIRE` error, not the caveat.
    pub(crate) fn note_tripwire(&mut self, reason: &StopReason) {
        let StopReason::Tripwire(report) = reason else {
            return;
        };
        let name = format!("{}:{}", report.kind.word(), report.detail);
        if !self.tripwires_hit.contains(&name) {
            self.tripwires_hit.push(name);
        }
    }

    /// Runs at the slice boundary every command already stops at, so it is the same instant in
    /// every replay, and only reads guest memory. Costs one FreeRTOS list walk.
    fn watch_deadlock(&mut self) {
        let Some(watch) = crate::commands::inspect::deadlock_watch() else {
            self.task_deadlock = None;
            return;
        };
        let fw = self.fw.clone();
        self.task_deadlock = watch(self.backend.as_mut(), &fw);
    }
}

/// Writes the HLE binding record into the receipt: `hle.bound` as `<profile>/<feature>`, and
/// `binding` with the status words. An all-zero app ELF SHA-256 reads `null`.
pub(crate) fn binding_into_receipt(
    binding: &pemu_machine::hle::HleBindingRecord,
    receipt: &mut Receipt,
) {
    use pemu_machine::hle::HleFeatureStatus;
    let bound: Vec<String> = binding
        .features
        .iter()
        .filter(|(_, status)| **status == HleFeatureStatus::Bound)
        .map(|(name, _)| {
            if binding.profile_id.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", binding.profile_id)
            }
        })
        .collect();
    receipt.hle.bound = (!bound.is_empty()).then(|| bound.join(","));
    // Saturates rather than wraps: a count this large is already a defect.
    receipt.hle.synthesized_log_lines = binding
        .log_lines
        .values()
        .map(|lines| u32::try_from(lines.synthesized).unwrap_or(u32::MAX))
        .fold(0u32, u32::saturating_add);
    let sha = (binding.app_elf_sha256 != [0; 32]).then(|| {
        binding
            .app_elf_sha256
            .iter()
            .fold(String::new(), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            })
    });
    let features: serde_json::Map<String, serde_json::Value> = binding
        .features
        .iter()
        .map(|(name, status)| (name.clone(), status.receipt_word().into()))
        .collect();
    if sha.is_none() {
        // Without an ELF a module binds only against symbols recovered from the image.
        let radio = if bound.is_empty() {
            "unbound (no ELF)"
        } else {
            "bound from the image (no ELF)"
        };
        receipt.extra.insert("radio".to_string(), radio.into());
    }
    receipt.extra.insert(
        "binding".to_string(),
        serde_json::json!({
            "profile_id": binding.profile_id,
            "app_elf_sha256": sha,
            "features": features,
            "log_lines": binding
                .log_lines
                .iter()
                .map(|(name, lines)| {
                    (
                        name.clone(),
                        serde_json::Value::from(if lines.verified {
                            "verified"
                        } else {
                            "unverified"
                        }),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        }),
    );
}

/// `heap_fidelity` is a declaration, not a measurement: the blocks are a Kconfig and
/// capture-derived lower bound. A run with no ledger block writes neither key.
fn heap_ledger_into_receipt(blocks: Vec<pemu_machine::hle::HleHeapBlock>, receipt: &mut Receipt) {
    if blocks.is_empty() {
        return;
    }
    let mut modules: std::collections::BTreeMap<&str, (u64, u64)> =
        std::collections::BTreeMap::new();
    for block in &blocks {
        let row = modules.entry(block.module.as_str()).or_default();
        row.0 += 1;
        row.1 += u64::from(block.bytes);
    }
    // The word travels on the blocks because `pemu-api` may not depend on `pemu-radio`.
    let fidelity = blocks[0].fidelity.clone();
    receipt
        .extra
        .insert("heap_fidelity".to_string(), fidelity.into());
    receipt.extra.insert(
        "heap_ledger".to_string(),
        modules
            .into_iter()
            .map(|(name, (count, bytes))| {
                (
                    name.to_string(),
                    serde_json::json!({ "blocks": count, "bytes": bytes }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into(),
    );
}

pub(crate) fn fault_counters_into_receipt(
    faults: pemu_machine::machine::FaultCounters,
    receipt: &mut Receipt,
) {
    receipt.extra.insert(
        "fault_counters".to_string(),
        serde_json::json!({
            "dma": faults.dma_faults,
            "pcm_width": faults.pcm_width_faults,
        }),
    );
}

/// `Until`, `MaxInsns`, `Matcher` and `ChipReset` are requested stops, not faults. `Deadlock` is
/// `E_DEADLOCK` under any limit, reported at the instant the hart became unwakeable.
pub(crate) fn fault_of(reason: &StopReason) -> Option<ApiError> {
    let (code, message) = match reason {
        StopReason::Until
        | StopReason::MaxInsns
        | StopReason::Matcher(_)
        | StopReason::ChipReset(_) => return None,
        StopReason::Deadlock => (
            crate::error::E_DEADLOCK,
            "the guest is waiting for input: the hart is in WFI and nothing but an input can wake it",
        ),
        // `esp_panic_handler`'s first argument is its `panic_info_t`, which the fault envelope
        // decodes.
        StopReason::GuestPanic(capture) => {
            return Some(
                ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked").with_detail(
                    serde_json::json!({
                        crate::commands::inspect::PANIC_INFO_KEY: capture.args[0],
                    }),
                ),
            );
        }
        StopReason::Stuck(_) => (crate::error::E_STUCK, "the guest made no progress"),
        StopReason::Tripwire(_) => (crate::error::E_TRIPWIRE, "a tripwire fired"),
        StopReason::Unmodeled(_) => (
            crate::error::E_UNMODELED,
            "the guest touched an unmodeled register under `--strict`",
        ),
        StopReason::Hle(_) => (crate::error::E_HLE, "an HLE handler failed"),
        StopReason::Breakpoint(_) | StopReason::Watchpoint { .. } | StopReason::Halted(_) => (
            E_STATE,
            "the machine stopped at a debug stop, so virtual time is frozen",
        ),
        // Until sleep is modeled, the machine stops where the guest slept.
        StopReason::Sleep(_) => (
            E_STATE,
            "the guest entered sleep, which this machine does not perform yet",
        ),
    };
    Some(ApiError::new(code, message))
}

/// Reported like the hart-level `Deadlock`; the caller adds the envelope.
pub(crate) fn task_deadlock_fault(session: &Session) -> Option<ApiError> {
    session.task_deadlock.as_ref().map(|report| {
        crate::commands::inspect::task_deadlock_error(report).at_vt_us(session.now().as_us())
    })
}

pub(crate) const NOW: pemu_machine::machine::At = pemu_machine::machine::At::Now;
