//! `passportsim snapshot`: `save`, `restore`, `list`, `delete`, `fork`, `export` and `import`.
//!
//! This crate has no machine and opens no file, so it goes through [`SnapshotHooks`] (default
//! [`MACHINE_HOOKS`]), [`crate::artifact_io::ArtifactIo`] and the machine's `SecretSet`. The hooks
//! and export salt are per process ([`Seams`]); snapshots, the `SecretSet` and the taint are per
//! pool ([`Store`]), because an `InstanceId` is unique only within its pool. A missing hook is
//! `E_SNAPSHOT`, never an empty snapshot.
//!
//! Export is the security boundary, enforced in order:
//!
//! 1. a tainted machine refuses without `include_secrets` plus a human code;
//! 2. `SnapOpts { export: true, .. }` has the machine erase the `nvs`, `nvs_keys` and cardid
//!    `[0x356000, 0x35A000)` pages, labelled `Redacted{<salted sha256>}` ([`erased_labels`]);
//! 3. live journal payloads are dropped, chunk numbering kept;
//! 4. cardid windows still found by value become 0xFF, labelled in the export;
//! 5. [`crate::redact`] runs over every section payload and its `varint_words`;
//! 6. the finished bytes are re-scanned, and a member still present refuses the write.
//!
//! `include_secrets` skips the removal steps, never the header stamp. Fork, rewind and boot-cache
//! snapshots are never exported.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pemu_core::input::InputEvent;
use pemu_core::snap::{
    JournalPending, LivePolicy, SnapError, SnapHeader, SnapOpts, SnapSection, Snapshot,
};
use pemu_core::time::VTime;
use pemu_machine::SnapshotMachine;

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_SECRET_REFUSED, E_SNAPSHOT, E_STATE, E_USAGE};
use crate::instance::InstanceId;
use crate::output::Output;
use crate::redact::Redactor;
use crate::registry::command;
use crate::secret_set::{MemberKind, SecretSet};
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::start::{Boot, StartArgs};
use crate::args::{
    JsonMap, instance_schema, object, only, opt_bool, opt_str, opt_u64, req_str, usage,
};
use crate::pool::{Pool, with_pool};
use crate::session::Session;

pub const FORK_MAX: u64 = 16;
/// `^[A-Za-z0-9_.-]{1,64}$`.
pub const NAME_MAX: usize = 64;

/// Smaller than a file name's set: an exported snapshot becomes an artifact path that must mean the
/// same on every host, so separators, drive letters and reserved Windows device names never get
/// that far.
#[must_use]
pub fn name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A boxed facade the pool can attach as a new instance.
pub type ForkHook =
    fn(&mut dyn SnapshotMachine, LivePolicy) -> Result<Box<dyn SnapshotMachine + Send>, SnapError>;

/// The same shape for a `MockMachine`, a browser instance and the real `Machine`.
#[derive(Copy, Clone)]
pub struct SnapshotHooks {
    /// `SnapOpts::export` is passed through: only the machine knows which of its bytes are the
    /// cardid window and the NVS pages.
    pub take: fn(&mut dyn SnapshotMachine, SnapOpts) -> Result<Snapshot, SnapError>,
    pub restore: fn(&mut dyn SnapshotMachine, &Snapshot) -> Result<(), SnapError>,
    pub fork: ForkHook,
}

fn machine_take(machine: &mut dyn SnapshotMachine, opts: SnapOpts) -> Result<Snapshot, SnapError> {
    machine.snapshot(opts)
}

fn machine_restore(
    machine: &mut dyn SnapshotMachine,
    snapshot: &Snapshot,
) -> Result<(), SnapError> {
    machine.restore(snapshot)
}

fn machine_fork(
    machine: &mut dyn SnapshotMachine,
    live: LivePolicy,
) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
    machine.fork(live)
}

pub const MACHINE_HOOKS: SnapshotHooks = SnapshotHooks {
    take: machine_take,
    restore: machine_restore,
    fork: machine_fork,
};

fn no_take(_machine: &mut dyn SnapshotMachine, _opts: SnapOpts) -> Result<Snapshot, SnapError> {
    Err(SnapError::Malformed {
        at: "machine",
        reason: "this build cannot take a snapshot",
    })
}

fn no_restore(_machine: &mut dyn SnapshotMachine, _snapshot: &Snapshot) -> Result<(), SnapError> {
    Err(SnapError::Malformed {
        at: "machine",
        reason: "this build cannot restore a snapshot",
    })
}

fn no_fork(
    _machine: &mut dyn SnapshotMachine,
    _live: LivePolicy,
) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
    Err(SnapError::Malformed {
        at: "machine",
        reason: "this build cannot fork a machine",
    })
}

/// Refuses every operation, for a test of the refusal.
pub const NO_HOOKS: SnapshotHooks = SnapshotHooks {
    take: no_take,
    restore: no_restore,
    fork: no_fork,
};

#[derive(Clone, Debug)]
pub struct Saved {
    pub snapshot: Snapshot,
    pub vt: VTime,
    /// Taint propagates to snapshots.
    pub tainted: bool,
    /// So `list` costs no re-encoding.
    pub size_bytes: usize,
    /// A save taken on redacted state, so restoring it keeps `redacted`. Not
    /// `snapshot.header.redacted`: that makes `export` skip the machine's redaction, and the guest
    /// may have written secrets since.
    pub from_redacted: bool,
}

impl Saved {
    pub fn redacted(&self) -> bool {
        self.snapshot.header.redacted || self.from_redacted
    }
}

/// Named snapshots, the `SecretSet` and the taint of one pool's instances. Not a `Session` field,
/// because the pool reads it when it attaches a session and a fork registers its copies from the
/// pool side.
#[derive(Default)]
pub struct Store {
    named: BTreeMap<InstanceId, BTreeMap<String, Saved>>,
    secrets: BTreeMap<InstanceId, SecretSet>,
    tainted: std::collections::BTreeSet<InstanceId>,
    generations: BTreeMap<InstanceId, u64>,
}

impl Store {
    pub fn named(&self, id: InstanceId) -> impl Iterator<Item = (&String, &Saved)> {
        self.named.get(&id).into_iter().flatten()
    }

    pub fn get(&self, id: InstanceId, name: &str) -> Option<&Saved> {
        self.named.get(&id).and_then(|map| map.get(name))
    }

    /// Replaces one of the same name.
    pub fn put(&mut self, id: InstanceId, name: &str, saved: Saved) {
        self.named
            .entry(id)
            .or_default()
            .insert(name.to_owned(), saved);
    }

    pub fn remove(&mut self, id: InstanceId, name: &str) -> bool {
        self.named
            .get_mut(&id)
            .is_some_and(|map| map.remove(name).is_some())
    }

    /// `Pool::destroy` does not call it: `stop` builds its receipt from the destroyed session,
    /// which reads the taint here. A pool never reuses a stopped id, so the entry is never read for
    /// another instance.
    pub fn forget(&mut self, id: InstanceId) {
        self.named.remove(&id);
        self.secrets.remove(&id);
        self.tainted.remove(&id);
        self.generations.remove(&id);
    }

    /// The empty set for an untainted machine.
    pub fn secret_set(&self, id: InstanceId) -> &SecretSet {
        static EMPTY: OnceLock<SecretSet> = OnceLock::new();
        self.secrets
            .get(&id)
            .unwrap_or_else(|| EMPTY.get_or_init(|| SecretSet::builder().build()))
    }

    /// A non-empty set taints the instance, an empty one clears the taint.
    pub fn set_secret_set(&mut self, id: InstanceId, set: SecretSet) {
        if set.is_empty() {
            self.tainted.remove(&id);
        } else {
            self.tainted.insert(id);
        }
        self.secrets.insert(id, set);
    }

    pub fn is_tainted(&self, id: InstanceId) -> bool {
        self.tainted.contains(&id)
    }

    /// Builds the `SecretSet` of a machine just attached, and taints the instance when its image
    /// yields a member or its eFuse was imported. What a guest wrote over the image is added
    /// without tainting.
    pub fn load_secret_set(&mut self, id: InstanceId, machine: &dyn SnapshotMachine) {
        self.secrets.remove(&id);
        self.tainted.remove(&id);
        self.generations.remove(&id);
        let loaded = machine.secret_sources(pemu_machine::snapshot::FlashView::Image);
        let mut builder = crate::secret_set::SecretSetBuilder::new();
        add_secret_sources(&mut builder, &loaded);
        if loaded.efuse_imported || !builder.build().is_empty() {
            self.tainted.insert(id);
        }
        // A fork can carry an NFC card a caller wrote a PWD or Wi-Fi key to, which taints.
        self.extend_secret_set(id, machine);
    }

    /// Extends the set with what the machine holds now, when its `secret_generation` moved: a
    /// credential the guest wrote to NVS or a cardid window it programmed. Members are never
    /// dropped, since an overwritten value can still sit in RAM. A guest write does not taint.
    pub fn extend_secret_set(&mut self, id: InstanceId, machine: &dyn SnapshotMachine) {
        let generation = machine.secret_generation();
        if self.generations.get(&id) == Some(&generation) {
            return;
        }
        let sources = machine.secret_sources(pemu_machine::snapshot::FlashView::Current);
        let mut builder = crate::secret_set::SecretSetBuilder::from_set(
            self.secrets.remove(&id).unwrap_or_default(),
        );
        add_secret_sources(&mut builder, &sources);
        // A PWD, PACK or Wi-Fi key a caller wrote to the NFC card is a secret input, so it taints.
        // A radio module can be secret-bearing with no member to add: a full-fidelity btsnoop
        // capture keeps this run's own pairing keys.
        if crate::commands::nfc_tap::UserSecrets::from_sources(&sources).is_secret()
            || wifi_keys_are_secret(&sources)
            || sources.radio_state_taints
        {
            self.tainted.insert(id);
        }
        self.secrets.insert(id, builder.build());
        self.generations.insert(id, generation);
    }
}

/// Shared by the pool and its checked-out sessions. [`StoreHandle::default`] is a new, empty table.
#[derive(Clone, Default)]
pub struct StoreHandle(Arc<Mutex<Store>>);

impl StoreHandle {
    /// Recovers a poisoned lock like the pool does. The lock is a leaf: nothing takes the pool lock
    /// or asks for a receipt while holding it.
    pub fn with<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        let mut guard: MutexGuard<'_, Store> = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}

/// What the host installs for every pool: machine hooks, artifact access and the export salt. A
/// test that installs its own seams holds `tests::world()` for its body.
#[derive(Default)]
pub struct Seams {
    salt: Vec<u8>,
    salt_source: Option<fn() -> Option<Vec<u8>>>,
    hooks: Option<SnapshotHooks>,
}

impl Seams {
    /// Host-supplied: a salt kept in the repository would make labels comparable across machines.
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    pub fn set_salt(&mut self, salt: Vec<u8>) {
        self.salt = salt;
    }

    /// Asked at the first export that needs it and kept, so a host that persists a per-install salt
    /// creates it only when a snapshot is exported.
    pub fn set_salt_source(&mut self, source: Option<fn() -> Option<Vec<u8>>>) {
        self.salt_source = source;
    }

    fn export_salt(&mut self) -> Vec<u8> {
        if self.salt.is_empty()
            && let Some(salt) = self.salt_source.and_then(|source| source())
        {
            self.salt = salt;
        }
        self.salt.clone()
    }

    pub fn hooks(&self) -> SnapshotHooks {
        self.hooks.unwrap_or(MACHINE_HOOKS)
    }

    /// A test seam.
    pub fn set_hooks(&mut self, hooks: SnapshotHooks) {
        self.hooks = Some(hooks);
    }
}

fn seams() -> &'static Mutex<Seams> {
    static SEAMS: OnceLock<Mutex<Seams>> = OnceLock::new();
    SEAMS.get_or_init(|| Mutex::new(Seams::default()))
}

pub fn with_seams<R>(f: impl FnOnce(&mut Seams) -> R) -> R {
    let mut guard: MutexGuard<'_, Seams> = match seams().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// A key too short or uniform for the secret set adds nothing and taints nothing, as for NFC keys.
fn wifi_keys_are_secret(sources: &pemu_machine::snapshot::SecretSources) -> bool {
    let mut builder = crate::secret_set::SecretSetBuilder::new();
    for key in &sources.wifi_keys {
        builder.nvs_credential(key);
    }
    !builder.build().is_empty()
}

/// The cardid window in 32-byte chunks, every NVS credential value, BLK1 and BLK2 of an imported
/// eFuse, and the PWD, PACK and Wi-Fi keys a caller put on the NFC card or sent in a tap.
pub fn add_secret_sources(
    builder: &mut crate::secret_set::SecretSetBuilder,
    sources: &pemu_machine::snapshot::SecretSources,
) {
    builder.cardid_window(&sources.cardid_window);
    for partition in &sources.nvs_partitions {
        for value in pemu_introspect::nvs::credential_values(partition) {
            builder.nvs_credential(&value);
        }
    }
    for (index, bytes) in &sources.efuse_blocks {
        // A block of the wrong length is not the machine's layout; the eFuse image fixes it.
        let _ = builder.efuse_block(*index, bytes);
    }
    crate::commands::nfc_tap::UserSecrets::from_sources(sources).add_to(builder);
    // A Wi-Fi key scripted through `env` is a secret input like an NFC password.
    for key in &sources.wifi_keys {
        builder.nvs_credential(key);
    }
}

/// Extended first with what the machine holds now: the set every export, inspect output and fault
/// envelope redacts with.
pub fn secrets_of(session: &mut Session) -> SecretSet {
    let id = session.id;
    let store = session.store();
    let machine: &dyn SnapshotMachine = session.snapshot_machine();
    store.with(|store| {
        store.extend_secret_set(id, machine);
        store.secret_set(id).clone()
    })
}

crate::matchers::str_enum! {
    /// What a `snapshot` call does. `load` is an accepted alias of `restore`
    /// ([`SnapOp::parse_arg`]).
    pub enum SnapOp {
        /// In-memory save under a name.
        Save = "save",
        Restore = "restore",
        List = "list",
        Delete = "delete",
        /// Create instances sharing the parent's read-only state.
        Fork = "fork",
        /// Write a redacted snapshot outside the process.
        Export = "export",
        Import = "import",
    }
}

impl SnapOp {
    pub fn parse_arg(text: &str) -> Option<SnapOp> {
        match text {
            "load" => Some(SnapOp::Restore),
            other => SnapOp::parse(other),
        }
    }

    pub fn is_read_only(self) -> bool {
        matches!(self, SnapOp::List)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotArgs {
    pub instance: Option<String>,
    pub op: SnapOp,
    /// Required by every op but `list`.
    pub name: Option<String>,
    /// 1 to [`FORK_MAX`].
    pub count: u64,
    /// For flakiness hunts.
    pub vary_seed: bool,
    /// Defaults to `snapshots/<name>.snap`.
    pub path: Option<String>,
    /// Needs [`SnapshotArgs::confirm`].
    pub include_secrets: bool,
    /// A human confirmation code, which an agent cannot produce.
    pub confirm: Option<String>,
}

impl Default for SnapshotArgs {
    fn default() -> SnapshotArgs {
        SnapshotArgs {
            instance: None,
            op: SnapOp::List,
            name: None,
            count: 1,
            vary_seed: false,
            path: None,
            include_secrets: false,
            confirm: None,
        }
    }
}

impl SnapshotArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<SnapshotArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "op",
                "name",
                "count",
                "vary_seed",
                "path",
                "include_secrets",
                "confirm",
            ],
        )?;
        let op_text = req_str(args, "op")?;
        let op = SnapOp::parse_arg(op_text).ok_or_else(|| {
            usage(
                "op",
                &format!(
                    "`{op_text}` is not one of save, restore, list, delete, fork, export, import"
                ),
            )
        })?;
        let name = opt_str(args, "name")?.map(str::to_owned);
        if let Some(name) = &name
            && !name_is_valid(name)
        {
            return Err(usage(
                "name",
                "expected 1 to 64 characters of A-Z, a-z, 0-9, `_`, `.` and `-`",
            ));
        }
        if name.is_none() && op != SnapOp::List {
            return Err(usage("name", &format!("is required by `{}`", op.as_str())));
        }
        let count = count_of(args, op)?;
        Ok(SnapshotArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            op,
            name,
            count,
            vary_seed: opt_bool(args, "vary_seed")?.unwrap_or(false),
            path: path_of(args)?,
            include_secrets: opt_bool(args, "include_secrets")?.unwrap_or(false),
            confirm: opt_str(args, "confirm")?.map(str::to_owned),
        })
    }

    pub fn artifact_path(&self) -> String {
        match (&self.path, &self.name) {
            (Some(path), _) => path.clone(),
            (None, Some(name)) => format!("snapshots/{name}.snap"),
            (None, None) => "snapshots/unnamed.snap".to_owned(),
        }
    }
}

/// Refused outside `fork` so a typo is reported instead of ignored.
fn count_of(args: &JsonMap, op: SnapOp) -> Result<u64, ApiError> {
    match opt_u64(args, "count")? {
        None => Ok(1),
        Some(_) if op != SnapOp::Fork => Err(usage("count", "is only used by `fork`")),
        Some(count) if count == 0 || count > FORK_MAX => Err(usage(
            "count",
            &format!("expected 1..={FORK_MAX}, the fork limit"),
        )),
        Some(count) => Ok(count),
    }
}

/// Checked as a relative, forward-slashed artifact path before any host sees it.
fn path_of(args: &JsonMap) -> Result<Option<String>, ApiError> {
    let Some(path) = opt_str(args, "path")? else {
        return Ok(None);
    };
    crate::output::check_artifact_path(path).map_err(|err| usage("path", &format!("{err}")))?;
    Ok(Some(path.to_owned()))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Per kind.
    pub redacted: crate::redact::RedactionReport,
    /// In the order found. The label identifies the removed content only to whoever holds the salt.
    pub redacted_labels: Vec<String>,
    pub live_payloads_dropped: usize,
    pub sections_rewritten: usize,
}

/// Replaces every cardid window of `data` with 0xFF and returns the label of each. Runs before the
/// generic [`Redactor`], which would fill the same bytes and record only a count. Only cardid
/// members are labelled: a label per MAC or credential would make an export a per-value oracle for
/// anyone holding the salt.
fn label_cardid_windows(data: &mut [u8], secrets: &SecretSet, salt: &[u8]) -> Vec<String> {
    let windows: Vec<&[u8]> = secrets
        .members()
        .iter()
        // No cardid member is `text_only` today; one that appeared would fall through to the
        // generic pass and lose its label.
        .filter(|member| {
            member.kind == MemberKind::CardId && !member.text_only && !member.bytes.is_empty()
        })
        .map(|member| member.bytes.as_slice())
        .collect();
    if windows.is_empty() {
        return Vec::new();
    }
    let mut labels = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let hit = windows
            .iter()
            .copied()
            .find(|window| {
                data.len() - at >= window.len() && &data[at..at + window.len()] == *window
            })
            .map(|window| window.len());
        match hit {
            Some(len) => {
                labels.push(crate::redact::redact_cardid_window(
                    &mut data[at..at + len],
                    salt,
                ));
                at += len;
            }
            None => at += 1,
        }
    }
    labels
}

/// The machine erases before this crate sees the sections and holds no salt, so it hands over the
/// prior window content, which is zeroed once labelled. Erased NVS partitions and an all-0xFF
/// window get no label.
pub fn erased_labels(
    redaction: &mut pemu_machine::snapshot::Redaction,
    salt: &[u8],
) -> Vec<String> {
    redaction
        .erased
        .iter_mut()
        .filter_map(|erased| match erased {
            pemu_machine::snapshot::Erased::CardId { content, .. } => {
                // An all-0xFF label would be the same on every machine.
                let label = content
                    .iter()
                    .any(|&b| b != 0xFF)
                    .then(|| crate::redact::redacted_label(salt, content));
                content.fill(0);
                std::hint::black_box(&content);
                *content = Vec::new();
                label
            }
            pemu_machine::snapshot::Erased::Nvs { .. } => None,
        })
        .collect()
}

/// Flash, RAM or byte caches: scanned as bytes, since decoding a RAM section as integers would only
/// multiply the scan.
fn is_byte_section(id: &pemu_core::snap::SectionId) -> bool {
    use pemu_core::snap::SectionId;
    [
        SectionId::RAM,
        SectionId::RTC_RAM,
        SectionId::FLASH_DELTA,
        pemu_machine::snapshot::SOC_FLASH_CACHE,
    ]
    .contains(&id.as_str())
}

/// A register block is a `Vec<u32>` of varints, so a flash word copied into a register is not its
/// bytes in the section. Every varint ends at a byte below 0x80, as does a length prefix, so one
/// stream from the first byte decodes the elements aligned. A varint longer than five bytes is cut
/// and the stream resynchronizes.
fn varint_words(data: &[u8]) -> Vec<(u32, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let (mut value, mut shift, mut start) = (0u64, 0u32, 0usize);
    for (i, &byte) in data.iter().enumerate() {
        value |= u64::from(byte & 0x7F) << shift;
        shift += 7;
        if byte < 0x80 || shift >= 35 {
            out.push((value as u32, start..i + 1));
            (value, shift, start) = (0, 0, i + 1);
        }
    }
    out
}

fn word_stream(words: &[(u32, std::ops::Range<usize>)]) -> Vec<u8> {
    words.iter().flat_map(|(w, _)| w.to_le_bytes()).collect()
}

fn varint(mut value: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// Each word's bytes are redacted as a binary artifact and changed words re-encoded in place. A
/// re-encoded word can change the section length; no member survives either way, and an export is a
/// boundary before it is a restorable file.
fn redact_register_words(
    bytes: &mut Vec<u8>,
    redactor: &Redactor<'_>,
) -> crate::redact::RedactionReport {
    let words = varint_words(bytes);
    let stream = word_stream(&words);
    let mut redacted = stream.clone();
    let report = redactor.redact_bytes_in_place(&mut redacted);
    if report.is_empty() {
        return report;
    }
    let consumed = words.last().map_or(0, |(_, at)| at.end);
    let mut out = Vec::with_capacity(bytes.len());
    for (i, (word, at)) in words.iter().enumerate() {
        let new = u32::from_le_bytes(redacted[4 * i..4 * i + 4].try_into().expect("4 bytes"));
        if new == *word {
            out.extend_from_slice(&bytes[at.clone()]);
        } else {
            out.extend(varint(new));
        }
    }
    out.extend_from_slice(&bytes[consumed..]);
    *bytes = out;
    report
}

fn register_words_hold_a_member(bytes: &[u8], redactor: &Redactor<'_>) -> bool {
    let stream = word_stream(&varint_words(bytes));
    redactor.redact_bytes(&stream) != stream
}

/// The chunk numbering is kept and the samples go, so a replay still shows a live stream was there
/// without carrying what the microphone heard.
fn strip_live_payload(event: &mut InputEvent) -> bool {
    match event {
        InputEvent::MicChunk { samples, .. } if !samples.is_empty() => {
            samples.clear();
            true
        }
        InputEvent::NetFrame { data, .. } | InputEvent::HciPacket { data, .. }
            if !data.is_empty() =>
        {
            data.clear();
            true
        }
        _ => false,
    }
}

/// Drops live payloads, redacts by value, stamps the header, encodes, then re-scans the finished
/// bytes; a surviving member writes nothing and returns `E_SECRET_REFUSED`. `include_secrets` skips
/// both removals but never the header stamp, so a receipt never claims `redacted` for it.
pub fn export_bytes(
    snapshot: &Snapshot,
    secrets: &SecretSet,
    salt: &[u8],
    include_secrets: bool,
) -> Result<(Vec<u8>, ExportReport), ApiError> {
    let mut snapshot = snapshot.clone();
    let mut report = ExportReport::default();

    // 1. Live payloads. A snapshot without the section is the common case.
    if !include_secrets
        && let Ok(section) = snapshot.section(&JournalPending::section_id())
        && let Ok(JournalPending(mut entries)) = JournalPending::decode(section)
    {
        let mut changed = false;
        for entry in &mut entries {
            if strip_live_payload(&mut entry.ev) {
                report.live_payloads_dropped += 1;
                changed = true;
            }
        }
        if changed {
            snapshot
                .put(&JournalPending(entries))
                .map_err(|err| snap_error("re-encoding `journal_pending`", &err))?;
            report.sections_rewritten += 1;
        }
    }

    // 2. Redaction by value. Cardid windows go first: their removal is recorded as a label.
    if !include_secrets {
        let redactor = Redactor::new(secrets);
        for section in snapshot.sections.values_mut() {
            let labels = label_cardid_windows(&mut section.bytes, secrets, salt);
            if !labels.is_empty() {
                report.sections_rewritten += 1;
                report.redacted_labels.extend(labels);
            }
            let one = redactor.redact_bytes_in_place(&mut section.bytes);
            if !one.is_empty() {
                report.sections_rewritten += 1;
            }
            report.redacted.merge(&one);
        }
        // Register words after the byte pass, so a member held as bytes is counted once.
        for (id, section) in snapshot.sections.iter_mut() {
            if is_byte_section(id) {
                continue;
            }
            let one = redact_register_words(&mut section.bytes, &redactor);
            if !one.is_empty() {
                report.sections_rewritten += 1;
            }
            report.redacted.merge(&one);
        }
    }

    // 3. The header.
    snapshot.header = SnapHeader {
        exported: true,
        redacted: !include_secrets,
        // An unsalted eFuse hash identifies a device to whoever holds its dump, and a restore does
        // not check the eFuse of a redacted snapshot.
        efuse_hash: if include_secrets {
            snapshot.header.efuse_hash
        } else {
            [0; 32]
        },
        ..snapshot.header.clone()
    };

    let bytes = snapshot
        .to_bytes()
        .map_err(|err| snap_error("encoding the snapshot", &err))?;

    // 4. The boundary check: a value still here was never matched, so write nothing.
    if !include_secrets && !secrets.is_empty() {
        let redactor = Redactor::new(secrets);
        let leftover = redactor.redact_bytes(&bytes) != bytes
            || Snapshot::from_bytes(&bytes).map_or(true, |written| {
                written.sections.iter().any(|(id, section)| {
                    !is_byte_section(id) && register_words_hold_a_member(&section.bytes, &redactor)
                })
            });
        if leftover {
            return Err(ApiError::new(
                E_SECRET_REFUSED,
                "the export still carried secret-bearing bytes after the redaction pass",
            )
            .with_hint("this is a redaction bug, not a policy decision; nothing was written"));
        }
    }
    Ok((bytes, report))
}

pub fn import_bytes(bytes: &[u8]) -> Result<Snapshot, ApiError> {
    Snapshot::from_bytes(bytes).map_err(|err| snap_error("reading the snapshot", &err))
}

/// Large data is returned by path plus hash.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    pemu_loader::hex(&pemu_loader::sha256(bytes))
}

/// Always `E_SNAPSHOT`; the message says what was being done, so a bad file differs from a bad
/// machine.
fn snap_error(what: &str, err: &SnapError) -> ApiError {
    ApiError::new(E_SNAPSHOT, format!("{what}: {err}"))
        .with_hint(identity_hint(err).unwrap_or("`snapshot list` shows what this instance has"))
}

fn identity_hint(err: &SnapError) -> Option<&'static str> {
    use pemu_core::snap::IdentityField;
    match err {
        SnapError::IdentityMismatch { field } => Some(match field {
            IdentityField::Rom => {
                "start an instance on the same ROM (the bundled one its eFuse selects, or the same `--rom` override) and restore there"
            }
            IdentityField::Image => {
                "start an instance with the same `--fw` image and app ELF the snapshot was saved from and restore there"
            }
            IdentityField::Efuse => {
                "start an instance with the same eFuse source (`--efuse synth` or the same dump) and restore there"
            }
            IdentityField::Config => {
                "start an instance with the same board, profile, seed and HLE configuration and restore there"
            }
        }),
        _ => None,
    }
}

/// Bind and lease check, the machine work on the checked-out session, then the pool side of a fork.
pub fn snapshot_on_pool(pool: &mut Pool, args: &SnapshotArgs) -> Result<Output, ApiError> {
    let id = bind_checked(pool, args)?;
    let mut session = pool.checkout(id)?;
    let step = snapshot_on_session(&mut session, args);
    pool.checkin(session);
    finish(pool, step?)
}

fn bind_checked(pool: &mut Pool, args: &SnapshotArgs) -> Result<InstanceId, ApiError> {
    let id = pool.bind(SPEC_SNAPSHOT.annotations, args.instance.as_deref())?;
    let now = pool
        .session(id)
        .map(Session::now)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    if let Some(state) = pool.table().get(id) {
        state.lease.check_call(
            crate::lease::LeaseHolder::Agent,
            SPEC_SNAPSHOT.annotations,
            now,
        )?;
    }
    Ok(id)
}

enum Step {
    Done(Output),
    /// Registering the copies needs the pool.
    Fork(Forked),
}

struct Forked {
    parent: InstanceId,
    name: String,
    vary_seed: bool,
    receipt: crate::receipt::Receipt,
    copies: Vec<(
        StartArgs,
        Box<dyn SnapshotMachine + Send>,
        super::mic_set::MicState,
    )>,
}

/// Runs on a checked-out session, so it holds no lock a call to another instance needs.
fn snapshot_on_session(session: &mut Session, args: &SnapshotArgs) -> Result<Step, ApiError> {
    Ok(match args.op {
        SnapOp::Save => Step::Done(save(session, args)?),
        SnapOp::Restore => Step::Done(restore(session, args)?),
        SnapOp::List => Step::Done(list(session)?),
        SnapOp::Delete => Step::Done(delete(session, args)?),
        SnapOp::Fork => Step::Fork(fork(session, args)?),
        SnapOp::Export => Step::Done(export(session, args)?),
        SnapOp::Import => Step::Done(import(session, args)?),
    })
}

fn finish(pool: &mut Pool, step: Step) -> Result<Output, ApiError> {
    match step {
        Step::Done(output) => Ok(output),
        Step::Fork(forked) => attach_forks(pool, forked),
    }
}

/// `from_json` already refused its absence.
fn named(args: &SnapshotArgs) -> Result<&str, ApiError> {
    args.name
        .as_deref()
        .ok_or_else(|| usage("name", &format!("is required by `{}`", args.op.as_str())))
}

fn save(session: &mut Session, args: &SnapshotArgs) -> Result<Output, ApiError> {
    let id = session.id;
    let name = named(args)?.to_owned();
    let vt = session.now();
    let hooks = with_seams(|seams| seams.hooks());
    let snapshot = (hooks.take)(session.snapshot_machine(), SnapOpts::default())
        .map_err(|err| machine_hook_error("take a snapshot", &err))?;
    let size_bytes = snapshot
        .to_bytes()
        .map_err(|err| snap_error("sizing the snapshot", &err))?
        .len();
    let tainted = false;
    // The digest two runs compare.
    let state_hash = crate::secret_set::hex_string(&session.snapshot_machine().state_hash());
    session.with_store(|store| {
        store.put(
            id,
            &name,
            Saved {
                snapshot,
                vt,
                tainted,
                size_bytes,
                from_redacted: session.redacted,
            },
        );
    });
    let receipt = session.receipt();
    let json = serde_json::json!({
        "op": "save",
        "instance": id.to_string(),
        "name": name,
        "vt_us": vt.as_us(),
        "size_bytes": size_bytes,
        "state_hash": state_hash,
    });
    let text = format!(
        "{id} snapshot save {name} vt={}us {size_bytes} bytes",
        vt.as_us()
    );
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

fn restore(session: &mut Session, args: &SnapshotArgs) -> Result<Output, ApiError> {
    let id = session.id;
    let name = named(args)?.to_owned();
    let saved = session
        .with_store(|store| store.get(id, &name).cloned())
        .ok_or_else(|| unknown_snapshot(id, &name))?;
    let hooks = with_seams(|seams| seams.hooks());
    (hooks.restore)(session.snapshot_machine(), &saved.snapshot)
        .map_err(|err| machine_hook_error("restore a snapshot", &err))?;
    session.mic = mic_after_restore(&saved.snapshot, saved.vt, session.mic);
    session.redacted = saved.redacted();
    let receipt = session.receipt();
    let json = serde_json::json!({
        "op": "restore",
        "instance": id.to_string(),
        "name": name,
        "vt_us": saved.vt.as_us(),
        "redacted": saved.redacted(),
    });
    let text = format!("{id} snapshot restore {name} vt={}us", saved.vt.as_us());
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

/// A restore rewinds the journal, and with it the chunk number `mic_set` expects next, so a later
/// `mic_set` continues without a gap. UNVERIFIED approximation: a journal entry carries no sample
/// rate, so a new source may be accepted up to one chunk early.
fn mic_after_restore(
    snapshot: &Snapshot,
    at: VTime,
    current: super::mic_set::MicState,
) -> super::mic_set::MicState {
    use pemu_core::journal::LiveStream;
    use pemu_core::snap::JournalCursor;
    let Ok(cursor) = snapshot.get::<JournalCursor>() else {
        return current;
    };
    let last_pending = snapshot
        .get::<JournalPending>()
        .map(|pending| {
            pending
                .0
                .iter()
                .filter(|e| matches!(e.ev, InputEvent::MicChunk { .. }))
                .map(|e| e.at)
                .max()
        })
        .unwrap_or_default();
    super::mic_set::MicState {
        next_seq: cursor.live_next[LiveStream::Mic.index()],
        pending_until: last_pending.map_or(at, |t| t.max(at)),
    }
}

fn list(session: &mut Session) -> Result<Output, ApiError> {
    let id = session.id;
    let rows: Vec<serde_json::Value> = session.with_store(|store| {
        store
            .named(id)
            .map(|(name, saved)| {
                serde_json::json!({
                    "name": name,
                    "vt_us": saved.vt.as_us(),
                    "size_bytes": saved.size_bytes,
                    "tainted": saved.tainted,
                    "redacted": saved.redacted(),
                    "sections": saved.snapshot.sections.len(),
                })
            })
            .collect()
    });
    let receipt = session.receipt();
    let mut text = String::new();
    if rows.is_empty() {
        text.push_str("no snapshot");
    }
    for row in &rows {
        let _ = writeln!(
            text,
            "{} vt={}us {} bytes{}",
            row["name"].as_str().unwrap_or("?"),
            row["vt_us"].as_u64().unwrap_or(0),
            row["size_bytes"].as_u64().unwrap_or(0),
            if row["redacted"] == serde_json::Value::Bool(true) {
                " redacted"
            } else {
                ""
            }
        );
    }
    let json = serde_json::json!({
        "op": "list",
        "instance": id.to_string(),
        "snapshots": rows,
    });
    Ok(Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT))
}

fn delete(session: &mut Session, args: &SnapshotArgs) -> Result<Output, ApiError> {
    let id = session.id;
    let name = named(args)?.to_owned();
    if !session.with_store(|store| store.remove(id, &name)) {
        return Err(unknown_snapshot(id, &name));
    }
    let receipt = session.receipt();
    let json = serde_json::json!({ "op": "delete", "instance": id.to_string(), "name": name });
    Ok(
        Output::new(json, format!("{id} snapshot delete {name}"), receipt)
            .shaped(&ShapeLimits::DEFAULT),
    )
}

/// Forks the instance as it stands; `name` labels the copies, which keeps a report of 16 of them
/// readable.
fn fork(session: &mut Session, args: &SnapshotArgs) -> Result<Forked, ApiError> {
    let id = session.id;
    let name = named(args)?.to_owned();
    let hooks = with_seams(|seams| seams.hooks());
    let (fw, seed, mode, mic) = {
        let (vt, mic) = (session.now(), session.mic);
        // A fork's journal is the parent's, so its `mic_set` numbering continues from it.
        let mic = (hooks.take)(session.snapshot_machine(), SnapOpts::default())
            .map_or(mic, |snapshot| mic_after_restore(&snapshot, vt, mic));
        (session.fw.clone(), session.seed, session.mode, mic)
    };
    let mut copies = Vec::with_capacity(args.count as usize);
    for index in 0..args.count {
        let backend = (hooks.fork)(session.snapshot_machine(), LivePolicy::Refuse)
            .map_err(|err| machine_hook_error("fork the machine", &err))?;
        // The fork index offsets the seed, so one call's seeds are distinct and reproducible.
        let seed = if args.vary_seed {
            seed.wrapping_add(index + 1)
        } else {
            seed
        };
        let start = StartArgs {
            fw: fw.clone(),
            label: format!("{name}#{index}"),
            seed,
            mode,
            boot: Boot::None,
            ..StartArgs::default()
        };
        copies.push((start, backend, mic));
    }
    Ok(Forked {
        parent: id,
        name,
        vary_seed: args.vary_seed,
        receipt: session.receipt(),
        copies,
    })
}

fn attach_forks(pool: &mut Pool, forked: Forked) -> Result<Output, ApiError> {
    let Forked {
        parent: id,
        name,
        vary_seed,
        receipt,
        copies,
    } = forked;
    let mut made = Vec::with_capacity(copies.len());
    for (start, backend, mic) in copies {
        let new_id = pool.attach(&start, backend);
        if let Some(session) = pool.session_mut(new_id) {
            session.mic = mic;
            // A copy of a redacted session runs on the same redacted state.
            session.redacted = receipt.redacted;
        }
        pool.table_mut()
            .get_mut(new_id)
            .ok_or_else(|| ApiError::new(E_INTERNAL, "the fork was not registered"))?
            .transition(crate::instance::Lifecycle::Paused, VTime(0))?;
        made.push(new_id.to_string());
    }
    let json = serde_json::json!({
        "op": "fork",
        "instance": id.to_string(),
        "name": name,
        "instances": made,
        "vary_seed": vary_seed,
    });
    let text = format!("{id} snapshot fork {name} -> {}", made.join(" "));
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

fn export(session: &mut Session, args: &SnapshotArgs) -> Result<Output, ApiError> {
    let id = session.id;
    let name = named(args)?.to_owned();
    let tainted = {
        // A saved snapshot inherits the machine's taint.
        let machine_tainted = machine_is_tainted(session);
        machine_tainted
            || session.with_store(|store| store.get(id, &name).is_some_and(|saved| saved.tainted))
    };
    if tainted && !confirmed_include_secrets(args) {
        return Err(secret_refused(args));
    }
    let saved = session.with_store(|store| store.get(id, &name).cloned());
    let redact = |session: &mut Session, mut snapshot: Snapshot| {
        session
            .snapshot_machine()
            .redact(&mut snapshot)
            .map(|redaction| (snapshot, Some(redaction)))
            .map_err(|err| machine_hook_error("redact the snapshot", &err))
    };
    let (snapshot, mut redaction) = match saved {
        // A named save was taken without the export flag, so the machine's own redaction has not
        // run yet.
        Some(saved) if !args.include_secrets && !saved.snapshot.header.redacted => {
            redact(session, saved.snapshot)?
        }
        // Already redacted (an imported export): the erased content, and what a label would
        // identify, is gone.
        Some(saved) => (saved.snapshot, None),
        None => {
            // Nothing named: export the instance as it stands.
            let include_secrets = args.include_secrets;
            let hooks = with_seams(|seams| seams.hooks());
            let taken = (hooks.take)(
                session.snapshot_machine(),
                SnapOpts {
                    export: include_secrets,
                    include_secrets,
                },
            )
            .map_err(|err| machine_hook_error("take a snapshot", &err))?;
            if include_secrets {
                (taken, None)
            } else {
                redact(session, taken)?
            }
        }
    };
    let salt = with_seams(Seams::export_salt);
    // Labels first, so the window content the machine handed back is gone before the value pass.
    let mut labels = redaction
        .as_mut()
        .map(|redaction| erased_labels(redaction, &salt))
        .unwrap_or_default();
    drop(redaction.take());
    let (bytes, mut report) = session.with_store(|store| {
        export_bytes(&snapshot, store.secret_set(id), &salt, args.include_secrets)
    })?;
    labels.append(&mut report.redacted_labels);
    report.redacted_labels = labels;
    let io = crate::artifact_io::installed().ok_or_else(no_artifact_io)?;
    let path = args.artifact_path();
    let written = (io.write)(&path, &bytes).map_err(|err| {
        ApiError::new(E_STATE, format!("the artifact could not be written: {err}"))
    })?;
    let mut receipt = session.receipt();
    receipt.redacted = !args.include_secrets;
    // Adds the taint of the saved snapshot, which an untainted instance can hold.
    receipt.tainted = receipt.tainted || tainted;
    let json = serde_json::json!({
        "op": "export",
        "instance": id.to_string(),
        "name": name,
        "path": written,
        "sha256": sha256_hex(&bytes),
        "size_bytes": bytes.len(),
        "redacted": !args.include_secrets,
        "redactions": report.redacted.replaced,
        // The label records that a cardid window was there, with only a salted hash.
        "redacted_labels": report.redacted_labels,
        "live_payloads_dropped": report.live_payloads_dropped,
    });
    let text = format!(
        "{id} snapshot export {name} -> {written} ({} bytes, {} redaction(s))",
        bytes.len(),
        report.redacted.replaced
    );
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

/// Kept rather than restored, so `import` then `restore` are two steps and the second moves the
/// guest.
fn import(session: &mut Session, args: &SnapshotArgs) -> Result<Output, ApiError> {
    let id = session.id;
    let name = named(args)?.to_owned();
    let io = crate::artifact_io::installed().ok_or_else(no_artifact_io)?;
    let path = args.artifact_path();
    let bytes = (io.read)(&path)
        .map_err(|err| ApiError::new(E_STATE, format!("the artifact could not be read: {err}")))?;
    let snapshot = import_bytes(&bytes)?;
    let redacted = snapshot.header.redacted;
    let sections = snapshot.sections.len();
    let vt = session.now();
    let mut receipt = session.receipt();
    receipt.redacted = redacted;
    session.with_store(|store| {
        store.put(
            id,
            &name,
            Saved {
                snapshot,
                vt,
                // A redacted snapshot restores with factory NVS and a synthetic cardid, so it
                // carries no taint.
                tainted: !redacted,
                size_bytes: bytes.len(),
                from_redacted: false,
            },
        );
    });
    let json = serde_json::json!({
        "op": "import",
        "instance": id.to_string(),
        "name": name,
        "path": path,
        "size_bytes": bytes.len(),
        "redacted": redacted,
        "sections": sections,
    });
    let text = format!(
        "{id} snapshot import {name} from {path} ({} bytes{})",
        bytes.len(),
        if redacted { ", redacted" } else { "" }
    );
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

/// The machine's own taint, or the store's. The set is extended first, so the export redacts what
/// the guest wrote since.
fn machine_is_tainted(session: &mut Session) -> bool {
    let own = pemu_machine::MachineApi::is_tainted(session.machine());
    let id = session.id;
    let store = session.store();
    let machine: &dyn SnapshotMachine = session.snapshot_machine();
    own || store.with(|store| {
        store.extend_secret_set(id, machine);
        store.is_tainted(id)
    })
}

fn confirmed_include_secrets(args: &SnapshotArgs) -> bool {
    args.include_secrets && args.confirm.as_deref().is_some_and(|code| !code.is_empty())
}

fn secret_refused(args: &SnapshotArgs) -> ApiError {
    let hint = if args.include_secrets {
        "`include_secrets` needs a human confirmation code in `confirm`"
    } else {
        "a tainted machine exports only with `include_secrets` and a human confirmation code"
    };
    ApiError::new(
        E_SECRET_REFUSED,
        "this instance loaded a secret-bearing input, so its state does not leave the process",
    )
    .with_hint(hint)
}

fn no_artifact_io() -> ApiError {
    ApiError::new(
        E_STATE,
        "this build cannot read or write an artifact, so `export` and `import` have nowhere to go",
    )
    .with_hint("a host installs the pair with `artifact_io::set`")
}

/// A live bridge refusing a restore or fork is `E_STATE`: the attached peer is the instance's
/// state, not the snapshot's.
fn machine_hook_error(what: &str, err: &SnapError) -> ApiError {
    if matches!(err, SnapError::LiveBridge { .. }) {
        return ApiError::new(E_STATE, format!("the machine could not {what}: {err}"))
            .with_hint("detach the live bridge, then try again");
    }
    ApiError::new(E_SNAPSHOT, format!("the machine could not {what}: {err}")).with_hint(
        identity_hint(err).unwrap_or(
            "this backend's `SnapshotMachine` refused; `snapshot list` shows \
             what this instance has",
        ),
    )
}

fn unknown_snapshot(id: InstanceId, name: &str) -> ApiError {
    ApiError::new(
        E_SNAPSHOT,
        format!("instance `{id}` has no snapshot `{name}`"),
    )
    .with_hint("`snapshot list` shows what it has")
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["op"],
        "description": "`snapshot` arguments.",
        "properties": {
            "instance": instance_schema(),
            "op": { "type": "string", "enum": ["save", "restore", "load", "list", "delete", "fork", "export", "import"], "description": "What to do." },
            "name": { "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,64}$", "description": "Snapshot name." },
            "count": { "type": "integer", "minimum": 1, "maximum": FORK_MAX, "description": "Forks to create (1)." },
            "vary_seed": { "type": "boolean", "description": "Give each fork its own seed." },
            "path": { "type": "string", "description": "Artifact path, relative and forward-slashed." },
            "include_secrets": { "type": "boolean", "description": "Export unredacted; needs `confirm`." },
            "confirm": { "type": "string", "description": "Human confirmation code." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "op": { "type": "string" },
            "instance": { "type": "string" },
            "name": { "type": "string" },
            "vt_us": { "type": "integer" },
            "size_bytes": { "type": "integer" },
            "snapshots": { "type": "array", "items": { "type": "object" } },
            "instances": { "type": "array", "items": { "type": "string" } },
            "path": { "type": "string" },
            "sha256": { "type": "string" },
            "state_hash": { "type": "string", "description": "`save`: the state hash of the saved state, 64 hex characters." },
            "redacted": { "type": "boolean" },
            "sections": { "type": "integer" }
        }
    })
}

/// Save, restore, list, fork, export and import instance state.
#[command(
    api_crate = crate,
    name = "snapshot",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    cli(positional = ["op", "name"]),
    scenario_step = "snapshot",
    errors(E_USAGE, E_STATE, E_LEASE, E_SNAPSHOT, E_SECRET_REFUSED, E_INTERNAL),
    example(
        title = "Save the current state under a name",
        args = r#"{"op":"save","name":"menu"}"#,
    ),
    example(
        title = "List what this instance has saved",
        args = r#"{"op":"list"}"#,
    ),
    example(
        title = "Fork 16 instances from the settled menu, each with its own seed",
        args = r#"{"op":"fork","name":"menu","count":16,"vary_seed":true}"#,
    ),
    example(
        title = "Export a redacted snapshot as an artifact",
        args = r#"{"op":"export","name":"menu"}"#,
    ),
    example(
        title = "Import a snapshot an export wrote",
        args = r#"{"op":"import","name":"menu"}"#,
    ),
)]
pub fn snapshot(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = SnapshotArgs::from_json(&args)?;
    // The machine work runs on the checked-out session without the pool lock.
    let step = crate::pool::with_session(
        |pool| bind_checked(pool, &args),
        |session| snapshot_on_session(session, &args),
    )?;
    with_pool(|pool| finish(pool, step))
}

#[cfg(test)]
pub(crate) mod tests {

    /// `env` refuses such a key before it is journaled, so these cases are what the set would do
    /// with a key that got past it.
    #[test]
    fn a_scripted_wifi_key_is_a_secret_input_and_a_droppable_one_is_not() {
        let with = |keys: Vec<Vec<u8>>| pemu_machine::snapshot::SecretSources {
            wifi_keys: keys,
            ..pemu_machine::snapshot::SecretSources::default()
        };
        assert!(super::wifi_keys_are_secret(&with(vec![
            b"scripted-key".to_vec()
        ])));
        assert!(!super::wifi_keys_are_secret(&with(Vec::new())));
        assert!(
            !super::wifi_keys_are_secret(&with(vec![b"abc".to_vec()])),
            "shorter than MIN_CREDENTIAL_LEN"
        );
        assert!(
            !super::wifi_keys_are_secret(&with(vec![b"aaaaaaaa".to_vec()])),
            "a uniform value is dropped"
        );
        let mut builder = crate::secret_set::SecretSetBuilder::new();
        super::add_secret_sources(&mut builder, &with(vec![b"scripted-key".to_vec()]));
        let set = builder.build();
        assert!(set.members().iter().any(|m| m.bytes == b"scripted-key"));
    }
    use super::*;

    use std::sync::Mutex as StdMutex;

    use pemu_core::journal::{JournalEntry, Origin};
    use pemu_core::snap::{Section, SectionId};

    use crate::commands::env::tests::journaled;

    /// Synthetic, with the `02:00:00` placeholder prefix the commit hook requires.
    const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x12, 0x34, 0x56];
    const UID: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f,
        0x10,
    ];
    const CALIBRATION: u32 = 0x0012_3456;
    const NVS_PASSWORD: &[u8] = b"correct-horse-battery";
    /// It has to vary: the builder drops a uniform run.
    const CARDID: [u8; 32] = [
        0x5a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    /// The process [`Seams`], the artifact map and the scripted snapshot are one shared world, so a
    /// test that installs into them holds this lock for its whole body.
    static WORLD: StdMutex<()> = StdMutex::new(());

    /// Recovers from a failed test's panic so one failure does not poison later tests.
    pub(crate) fn world() -> std::sync::MutexGuard<'static, ()> {
        match WORLD.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    static FILES: StdMutex<Option<BTreeMap<String, Vec<u8>>>> = StdMutex::new(None);

    fn files<R>(f: impl FnOnce(&mut BTreeMap<String, Vec<u8>>) -> R) -> R {
        let mut guard = FILES.lock().expect("the artifact store is never poisoned");
        f(guard.get_or_insert_with(BTreeMap::new))
    }

    fn test_write(path: &str, bytes: &[u8]) -> Result<String, String> {
        files(|map| map.insert(path.to_owned(), bytes.to_vec()));
        Ok(path.to_owned())
    }

    fn test_read(path: &str) -> Result<Vec<u8>, String> {
        files(|map| map.get(path).cloned()).ok_or_else(|| format!("no artifact `{path}`"))
    }

    /// Answers from one fixed snapshot, so the export and import rules are tested without a
    /// machine.
    static SCRIPTED: StdMutex<Option<Snapshot>> = StdMutex::new(None);

    fn scripted_take(
        _machine: &mut dyn SnapshotMachine,
        _opts: SnapOpts,
    ) -> Result<Snapshot, SnapError> {
        SCRIPTED
            .lock()
            .expect("not poisoned")
            .clone()
            .ok_or(SnapError::BadMagic)
    }

    #[test]
    fn an_identity_mismatch_keeps_e_snapshot_and_names_what_to_change() {
        use pemu_core::snap::IdentityField;
        let err = SnapError::IdentityMismatch {
            field: IdentityField::Image,
        };
        for api in [
            snap_error("restore", &err),
            machine_hook_error("restore", &err),
        ] {
            assert_eq!(api.code, E_SNAPSHOT);
            assert!(api.message.contains("flash image"), "{}", api.message);
            assert!(
                api.hint.as_deref().is_some_and(|h| h.contains("--fw")),
                "{:?}",
                api.hint
            );
        }
    }

    #[test]
    fn a_restore_rewinds_the_mic_numbering_to_the_snapshot_journal() {
        use crate::commands::mic_set::MicState;
        use pemu_core::journal::{Determinism, LiveStream};
        use pemu_core::snap::JournalCursor;

        let later = MicState {
            next_seq: 9,
            pending_until: VTime::from_ms(900),
        };
        let bare = Snapshot::new(SnapHeader::new());
        assert_eq!(mic_after_restore(&bare, VTime::from_ms(5), later), later);

        let mut snapshot = Snapshot::new(SnapHeader::new());
        let mut live_next = [0; LiveStream::ALL.len()];
        live_next[LiveStream::Mic.index()] = 3;
        snapshot
            .put(&JournalCursor {
                cursor: 2,
                next_seq: 4,
                class: Determinism::Deterministic,
                live_notes: Vec::new(),
                live_note_count: 0,
                live_next,
            })
            .expect("encodes");
        let chunk = |at_ms, seq| JournalEntry {
            at: VTime::from_ms(at_ms),
            seq,
            origin: Origin::Agent,
            ev: InputEvent::MicChunk {
                seq,
                samples: vec![0; 4],
            },
        };
        snapshot
            .put(&JournalPending(vec![chunk(20, 2), chunk(30, 3)]))
            .expect("encodes");
        assert_eq!(
            mic_after_restore(&snapshot, VTime::from_ms(10), later),
            MicState {
                next_seq: 3,
                pending_until: VTime::from_ms(30),
            }
        );
        snapshot.put(&JournalPending(Vec::new())).expect("encodes");
        assert_eq!(
            mic_after_restore(&snapshot, VTime::from_ms(10), later).pending_until,
            VTime::from_ms(10)
        );
    }

    fn scripted_restore(
        _machine: &mut dyn SnapshotMachine,
        _s: &Snapshot,
    ) -> Result<(), SnapError> {
        Ok(())
    }

    fn scripted_fork(
        _machine: &mut dyn SnapshotMachine,
        _live: LivePolicy,
    ) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        let (machine, _) = crate::commands::env::tests::JournalMachine::new();
        Ok(Box::new(machine))
    }

    const SCRIPTED_HOOKS: SnapshotHooks = SnapshotHooks {
        take: scripted_take,
        restore: scripted_restore,
        fork: scripted_fork,
    };

    /// Secrets sit inside plausible payloads rather than alone, as the sliding-window pass must
    /// handle.
    fn snapshot_with_secrets() -> Snapshot {
        let mut snapshot = Snapshot::new(SnapHeader::new());
        let mut ram = b"boot log: sta mac ".to_vec();
        ram.extend_from_slice(&MAC);
        ram.extend_from_slice(b" uid ");
        ram.extend_from_slice(&UID);
        ram.extend_from_slice(b" adc ");
        ram.extend_from_slice(&CALIBRATION.to_le_bytes());
        snapshot.put_raw(
            SectionId::new(SectionId::RAM),
            Section {
                version: 1,
                codec: pemu_core::snap::Codec::Postcard,
                bytes: ram,
            },
        );
        let mut flash = b"nvs: wifi.pass=".to_vec();
        flash.extend_from_slice(NVS_PASSWORD);
        flash.extend_from_slice(b" cardid:");
        flash.extend_from_slice(&CARDID);
        snapshot.put_raw(
            SectionId::new(SectionId::FLASH_DELTA),
            Section {
                version: 1,
                codec: pemu_core::snap::Codec::Postcard,
                bytes: flash,
            },
        );
        snapshot
    }

    #[test]
    fn the_salt_source_is_asked_once_and_only_when_an_export_needs_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn source() -> Option<Vec<u8>> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Some(b"per-install".to_vec())
        }
        let mut seams = Seams::default();
        seams.set_salt_source(Some(source));
        assert_eq!(CALLS.load(Ordering::SeqCst), 0, "installing asks nothing");
        assert_eq!(seams.export_salt(), b"per-install");
        assert_eq!(seams.export_salt(), b"per-install");
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    /// Stable so a label is reproducible; a real salt is host-supplied.
    const SALT: &[u8] = b"a-test-salt-of-16";

    fn secrets() -> SecretSet {
        let mut builder = SecretSet::builder();
        builder
            .base_mac(MAC)
            .unique_id(UID)
            .calibration_word(CALIBRATION)
            .nvs_credential(NVS_PASSWORD)
            .cardid_window(&CARDID);
        builder.build()
    }

    #[test]
    fn a_removed_cardid_window_is_recorded_as_a_salted_label() {
        let (_, report) = export_bytes(&snapshot_with_secrets(), &secrets(), SALT, false)
            .expect("the export is written");
        let label = report
            .redacted_labels
            .first()
            .expect("the cardid window was labelled");
        assert!(label.starts_with("Redacted{"), "{label}");
        assert!(label.ends_with('}'), "{label}");
        assert_eq!(
            label.len(),
            "Redacted{}".len() + 64,
            "a sha256 in hex: {label}"
        );
        assert_eq!(
            label,
            &crate::redact::redacted_label(SALT, &CARDID),
            "the label identifies the window that was removed"
        );
        assert!(
            !label.contains(&crate::secret_set::hex_string(&CARDID)),
            "a label must not carry the window it stands for"
        );
    }

    #[test]
    fn the_same_window_labels_differently_under_a_different_salt() {
        let (_, one) = export_bytes(&snapshot_with_secrets(), &secrets(), SALT, false)
            .expect("the export is written");
        let (_, two) = export_bytes(&snapshot_with_secrets(), &secrets(), b"another-salt", false)
            .expect("the export is written");
        assert_ne!(one.redacted_labels, two.redacted_labels);
        assert_eq!(one.redacted_labels.len(), two.redacted_labels.len());
    }

    #[test]
    fn include_secrets_keeps_the_live_payloads_a_replay_needs() {
        let mut snapshot = snapshot_with_secrets();
        snapshot
            .put(&JournalPending(vec![JournalEntry {
                at: VTime::from_ms(1),
                seq: 0,
                origin: Origin::default(),
                ev: InputEvent::MicChunk {
                    seq: 7,
                    samples: vec![1234, -4321, 99],
                },
            }]))
            .expect("the section encodes");

        let redacted = export_bytes(&snapshot, &secrets(), SALT, false)
            .expect("the export is written")
            .1;
        assert_eq!(
            redacted.live_payloads_dropped, 1,
            "the default export drops them"
        );

        let (bytes, full) =
            export_bytes(&snapshot, &secrets(), SALT, true).expect("the export is written");
        assert_eq!(
            full.live_payloads_dropped, 0,
            "`--include-secrets` keeps the journal payloads a replay needs"
        );
        assert!(
            full.redacted_labels.is_empty(),
            "nothing is redacted, so nothing is labelled"
        );
        let read = import_bytes(&bytes).expect("reads back");
        let JournalPending(entries) = read.get::<JournalPending>().expect("the section is there");
        match &entries[0].ev {
            InputEvent::MicChunk { samples, .. } => assert_eq!(
                samples,
                &vec![1234, -4321, 99],
                "the samples are still there, which is what makes the export replayable"
            ),
            other => panic!("the entry should still be the mic chunk, saw {other:?}"),
        }
    }

    fn instance_with_secrets() -> (Pool, InstanceId) {
        let (pool, id, _) = journaled();
        *SCRIPTED.lock().expect("not poisoned") = Some(snapshot_with_secrets());
        files(BTreeMap::clear);
        with_seams(|seams| {
            seams.set_hooks(SCRIPTED_HOOKS);
            crate::artifact_io::set(crate::artifact_io::ArtifactIo {
                write: test_write,
                read: test_read,
            });
            seams.set_salt(SALT.to_vec());
        });
        pool.with_store(|store| {
            store.forget(id);
            store.set_secret_set(id, secrets());
        });
        (pool, id)
    }

    pub(crate) fn args(json: serde_json::Value) -> SnapshotArgs {
        SnapshotArgs::from_json(&json).expect("inside the schema")
    }

    #[test]
    fn an_export_carries_no_member_of_the_secret_set() {
        let secrets = secrets();
        let (bytes, report) = export_bytes(&snapshot_with_secrets(), &secrets, SALT, false)
            .expect("the export is written");
        assert!(
            report.redacted.replaced + report.redacted_labels.len() >= 5,
            "every kind in the set should have been replaced, saw {:?} plus {} label(s)",
            report.redacted.by_kind,
            report.redacted_labels.len()
        );
        assert!(
            !report.redacted_labels.is_empty(),
            "the cardid window is removed by the labelling pass, not counted by kind"
        );
        for (what, needle) in [
            ("the base MAC", &MAC[..]),
            ("the unique id", &UID[..]),
            ("a calibration word", &CALIBRATION.to_le_bytes()[..]),
            ("an NVS credential", NVS_PASSWORD),
            ("the cardid window", &CARDID[..]),
        ] {
            assert!(!contains(&bytes, needle), "{what} survived the export");
        }
    }

    /// The redaction fill is 0xFF, so the window's length is still there and is all fill.
    #[test]
    fn the_cardid_window_leaves_as_0xff() {
        let (bytes, _) =
            export_bytes(&snapshot_with_secrets(), &secrets(), SALT, false).expect("written");
        let fill = vec![crate::redact::BINARY_FILL; CARDID.len()];
        assert!(
            contains(&bytes, &fill),
            "the window should be there as 0xFF, keeping its length"
        );
    }

    #[test]
    fn the_header_never_carries_efuse_bytes() {
        let header = SnapHeader {
            efuse_hash: [7; 32],
            ..SnapHeader::new()
        };
        assert_eq!(header.efuse_kind, pemu_core::snap::EfuseKind::Synthesized);
        let (bytes, _) =
            export_bytes(&Snapshot::new(header.clone()), &secrets(), SALT, false).expect("written");
        let read = import_bytes(&bytes).expect("its own output reads back");
        assert!(read.header.exported && read.header.redacted);
        // Nor an unsalted hash of them.
        assert_eq!(read.header.efuse_hash, [0; 32]);
        let (bytes, _) =
            export_bytes(&Snapshot::new(header), &secrets(), SALT, true).expect("written");
        let read = import_bytes(&bytes).expect("its own output reads back");
        assert_eq!(
            read.header.efuse_hash, [7; 32],
            "include_secrets keeps the identity"
        );
    }

    #[test]
    fn live_journal_payloads_are_dropped_and_their_numbering_is_kept() {
        let mut snapshot = Snapshot::new(SnapHeader::new());
        let entries = vec![
            JournalEntry {
                at: VTime::from_ms(1),
                seq: 0,
                origin: Origin::default(),
                ev: InputEvent::MicChunk {
                    seq: 7,
                    samples: vec![1234, -4321, 99],
                },
            },
            JournalEntry {
                at: VTime::from_ms(2),
                seq: 1,
                origin: Origin::default(),
                ev: InputEvent::Power { down: true },
            },
        ];
        snapshot
            .put(&JournalPending(entries))
            .expect("the section encodes");
        let (bytes, report) = export_bytes(&snapshot, &secrets(), SALT, false).expect("written");
        assert_eq!(report.live_payloads_dropped, 1);
        let read = import_bytes(&bytes).expect("reads back");
        let JournalPending(entries) = read.get::<JournalPending>().expect("the section is there");
        match &entries[0].ev {
            InputEvent::MicChunk { seq, samples } => {
                assert_eq!(*seq, 7, "the numbering survives");
                assert!(samples.is_empty(), "the samples do not");
            }
            other => panic!("the first entry should still be the mic chunk, saw {other:?}"),
        }
        assert_eq!(entries[1].ev, InputEvent::Power { down: true });
    }

    #[test]
    fn include_secrets_keeps_the_bytes_and_says_so_in_the_header() {
        let (bytes, report) =
            export_bytes(&snapshot_with_secrets(), &secrets(), SALT, true).expect("written");
        assert_eq!(report.redacted.replaced, 0);
        assert!(contains(&bytes, &MAC[..]), "nothing was redacted");
        let read = import_bytes(&bytes).expect("reads back");
        assert!(read.header.exported && !read.header.redacted);
    }

    #[test]
    fn a_tainted_instance_refuses_to_export_without_a_confirmed_opt_in() {
        let _world = world();
        let (mut pool, _) = instance_with_secrets();
        let error = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"m"})),
        )
        .expect_err("a tainted machine refuses");
        assert_eq!(error.code, E_SECRET_REFUSED);

        let error = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"m","include_secrets":true})),
        )
        .expect_err("the opt-in alone is not enough");
        assert_eq!(error.code, E_SECRET_REFUSED);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("confirmation code")),
            "{:?}",
            error.hint
        );

        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({
                "op":"export","name":"m","include_secrets":true,"confirm":"123456"
            })),
        )
        .expect("a confirmed opt-in exports");
        assert_eq!(out.json["redacted"], false);
        assert!(out.receipt.tainted);
    }

    #[test]
    fn an_export_of_a_real_machine_with_an_erased_cardid_window_carries_no_label() {
        use crate::commands::start::{Boot, StartArgs};
        use crate::instance::Lifecycle;
        let _world = world();
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            pemu_loader::bundle::FlashImage::erased(),
            None,
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        let machine =
            pemu_machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
                .expect("composes");
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "erased".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&start, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        files(BTreeMap::clear);
        with_seams(|seams| {
            seams.set_hooks(MACHINE_HOOKS);
            crate::artifact_io::set(crate::artifact_io::ArtifactIo {
                write: test_write,
                read: test_read,
            });
            seams.set_salt(SALT.to_vec());
        });
        pool.with_store(|store| {
            store.forget(id);
            store.set_secret_set(id, SecretSet::builder().build());
        });
        let expected = serde_json::json!([]);

        let direct = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"now"})),
        )
        .expect("an untainted machine exports");
        assert_eq!(direct.json["redacted"], true);
        assert_eq!(direct.json["redacted_labels"], expected, "{}", direct.json);

        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"s"})),
        )
        .expect("saves");
        let saved = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"s"})),
        )
        .expect("the named save exports");
        assert_eq!(saved.json["redacted_labels"], expected, "{}", saved.json);

        let full = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"s","include_secrets":true})),
        )
        .expect("an untainted machine exports with secrets too");
        assert_eq!(full.json["redacted_labels"], serde_json::json!([]));
    }

    /// Synthetic, never a device's.
    const GUEST_TOKEN: &[u8] = b"tok-SYNTH-0001";

    const GUEST_LOAD: u32 = 0x403C_E000;

    /// A hand-assembled RV32 guest. It programs [`GUEST_TOKEN`] (with its NUL and one 0xFF, 16
    /// bytes) into the erased NVS `token` entry at 0x9080 over SPI1 (WREN, `W0..W3`, PP, then RDSR
    /// until WIP clears), reads it back with FLASH_READ, and copies the words into RAM at
    /// 0x3FC9_0000, into the HMAC message registers (a store-only block held as varint words), and
    /// 14 bytes into the USB Serial/JTAG EP1 FIFO; then it spins.
    const GUEST_PROGRAM: &[u32] = &[
        // lui t0, SPI1 (0x6000_2000)
        0x600022B7, // CMD = FLASH_WREN
        0x40000337, 0x0062A023, // W0 = value word 0
        0x2D6B7337, 0xF7430313, 0x0462AC23, // W1 = value word 1
        0x544E6337, 0x95330313, 0x0462AE23, // W2 = value word 2
        0x30303337, 0xD4830313, 0x0662A023, // W3 = value word 3
        0xFF003337, 0x13030313, 0x0662A223, // ADDR = 0x9080 | 16 << 24
        0x10009337, 0x08030313, 0x0062A223, // CMD = FLASH_PP
        0x02000337, 0x0062A023, // poll: CMD = FLASH_RDSR; while RD_STATUS & WIP
        0x08000337, 0x0062A023, 0x02C2A303, 0x00137313, 0xFE0318E3, // W0..W3 = 0
        0x0402AC23, 0x0402AE23, 0x0602A023, 0x0602A223, // ADDR = 0x9080 | 16 << 24
        0x10009337, 0x08030313, 0x0062A223, // CMD = FLASH_READ
        0x80000337, 0x0062A023, // a1..a4 = W0..W3
        0x0582A583, 0x05C2A603, 0x0602A683, 0x0642A703, // RAM 0x3FC9_0000 = a1..a4
        0x3FC903B7, 0x00B3A023, 0x00C3A223, 0x00D3A423, 0x00E3A623,
        // HMAC message words = a1..a4
        0x6003EE37, 0x08BE2023, 0x08CE2223, 0x08DE2423, 0x08EE2623,
        // t4 = USJ EP1, t5 = 14, t6 = RAM
        0x60043EB7, 0x00E00F13, 0x00038F93, // loop: EP1 = *t6++ while --t5
        0x000FC403, 0x008EA023, 0x001F8F93, 0xFFFF0F13, 0xFE0F18E3, // j .
        0x0000006F,
    ];

    /// A one-segment bootloader the ROM enters, a partition table with one NVS partition at 0x9000,
    /// and an active page with namespace `app` and a 15-byte string entry `token` whose payload is
    /// still erased. The image holds no credential, so the machine is not tainted.
    fn guest_image() -> Vec<u8> {
        guest_image_writing_at(0x9080)
    }

    /// The two `li t1, target | 16 << 24` pairs of [`GUEST_PROGRAM`] re-encoded.
    fn guest_image_writing_at(target: u32) -> Vec<u8> {
        let li = |value: u32| {
            let lo = ((value & 0xFFF) ^ 0x800).wrapping_sub(0x800);
            let hi = value.wrapping_sub(lo) >> 12;
            [
                (hi << 12) | (6 << 7) | 0x37,
                ((lo & 0xFFF) << 20) | (6 << 15) | (6 << 7) | 0x13,
            ]
        };
        let mut program = GUEST_PROGRAM.to_vec();
        for at in [15, 29] {
            assert_eq!(
                program[at..at + 2],
                li(0x9080 | 16 << 24),
                "the ADDR pair moved"
            );
            program[at..at + 2].copy_from_slice(&li(target | 16 << 24));
        }
        let code: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut flash = vec![0xFFu8; 8 << 20];
        let mut boot = vec![0xE9, 1, 2, 0x3F];
        boot.extend_from_slice(&GUEST_LOAD.to_le_bytes());
        boot.extend_from_slice(&[0xEE, 0, 0, 0]);
        boot.extend_from_slice(&5u16.to_le_bytes());
        boot.push(0);
        boot.extend_from_slice(&0u16.to_le_bytes());
        boot.extend_from_slice(&0xFFFFu16.to_le_bytes());
        boot.extend_from_slice(&[0; 5]);
        boot.extend_from_slice(&GUEST_LOAD.to_le_bytes());
        boot.extend_from_slice(&(code.len() as u32).to_le_bytes());
        boot.extend_from_slice(&code);
        while boot.len() % 16 != 15 {
            boot.push(0);
        }
        boot.push(code.iter().fold(0xEF, |c, b| c ^ b));
        flash[..boot.len()].copy_from_slice(&boot);
        let mut row = vec![0xAA, 0x50, 0x01, 0x02];
        row.extend_from_slice(&0x9000u32.to_le_bytes());
        row.extend_from_slice(&0x4000u32.to_le_bytes());
        let mut label = [0u8; 16];
        label[..3].copy_from_slice(b"nvs");
        row.extend_from_slice(&label);
        row.extend_from_slice(&[0; 4]);
        flash[0x8000..0x8000 + row.len()].copy_from_slice(&row);
        let page = &mut flash[0x9000..0xA000];
        page[0..4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        // Entries 0 to 2 written (0b10 each), the rest empty.
        page[32] = 0b1110_1010;
        let entry = |page: &mut [u8], i: usize, ns: u8, ty: u8, span: u8, key: &[u8]| {
            let at = 64 + 32 * i;
            page[at..at + 24].fill(0);
            page[at] = ns;
            page[at + 1] = ty;
            page[at + 2] = span;
            page[at + 8..at + 8 + key.len()].copy_from_slice(key);
        };
        entry(page, 0, 0, 0x01, 1, b"app");
        page[64 + 24..64 + 32].copy_from_slice(&[1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        entry(page, 1, 1, 0x21, 2, b"token");
        page[96 + 24..96 + 32].copy_from_slice(&[15, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        flash
    }

    /// As bytes, or as postcard varint register words.
    fn holds(bytes: &[u8], value: &[u8]) -> bool {
        let words = word_stream(&varint_words(bytes));
        [bytes, &words[..]]
            .iter()
            .any(|b| b.windows(value.len()).any(|w| w == value))
    }

    /// The guest writes a credential after attach and copies it around. The instance stays
    /// untainted, the set is extended at the export, and every copy is gone from the written bytes.
    #[test]
    fn a_credential_the_guest_writes_and_copies_leaves_no_copy_in_the_export() {
        use crate::commands::start::{Boot, StartArgs};
        use crate::instance::Lifecycle;
        let _world = world();
        let flash = pemu_loader::bundle::FlashImage::from_merged(&guest_image()).expect("merged");
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            flash,
            None,
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        let machine =
            pemu_machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
                .expect("composes");
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "guest".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&start, Box::new(machine));
        assert!(
            pool.with_store(|store| !store.is_tainted(id) && store.secret_set(id).is_empty()),
            "an image without a credential value loads untainted, with nothing to redact yet"
        );
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        let session = pool.session_mut(id).expect("attached");
        session
            .machine()
            .run(pemu_machine::run::RunLimits::insns(5_000_000));
        let before = session
            .snapshot_machine()
            .snapshot(SnapOpts::default())
            .expect("a real machine snapshots");
        for (section, raw) in [("ram", true), ("soc.usj", true), ("soc.hmac", false)] {
            let bytes = &before.sections[&pemu_core::snap::SectionId(section.to_owned())].bytes;
            assert!(
                holds(bytes, GUEST_TOKEN),
                "the guest's copy is in `{section}`"
            );
            assert_eq!(
                bytes.windows(GUEST_TOKEN.len()).any(|w| w == GUEST_TOKEN),
                raw
            );
        }
        files(BTreeMap::clear);
        with_seams(|seams| {
            seams.set_hooks(MACHINE_HOOKS);
            crate::artifact_io::set(crate::artifact_io::ArtifactIo {
                write: test_write,
                read: test_read,
            });
            seams.set_salt(SALT.to_vec());
        });

        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"now"})),
        )
        .expect("an untainted machine exports, and the re-scan found no member");
        assert!(!out.receipt.tainted, "a guest write does not taint");
        assert!(
            out.json["redactions"].as_u64().is_some_and(|n| n >= 3),
            "{}",
            out.json
        );
        let written = files(|map| map.values().next().cloned()).expect("the export was written");
        let exported = Snapshot::from_bytes(&written).expect("the export reads back");
        assert!(!holds(&written, GUEST_TOKEN));
        for (section, payload) in &exported.sections {
            assert!(
                !holds(&payload.bytes, GUEST_TOKEN),
                "a copy survived in `{}`",
                section.as_str()
            );
        }
        assert!(pool.with_store(|store| !store.secret_set(id).is_empty() && !store.is_tainted(id)));
        pool.destroy(id).expect("ends");
    }

    /// The image holds no credential, so the set starts empty; the guest writes one and prints it
    /// in a panic line.
    #[test]
    fn a_credential_the_guest_writes_is_redacted_from_a_fault_envelope() {
        use crate::commands::start::{Boot, StartArgs};
        use crate::instance::Lifecycle;
        use pemu_core::hostio::SerialStream;
        let _world = world();
        let flash = pemu_loader::bundle::FlashImage::from_merged(&guest_image()).expect("merged");
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            flash,
            None,
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        let machine =
            pemu_machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
                .expect("composes");
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "guest".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&start, Box::new(machine));
        pool.with_store(|store| store.forget(id));
        with_seams(|seams| seams.set_hooks(MACHINE_HOOKS));
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        let session = pool.session_mut(id).expect("attached");
        session
            .machine()
            .run(pemu_machine::run::RunLimits::insns(5_000_000));
        assert!(
            session.with_store(|store| store.secret_set(id).is_empty()),
            "nothing extended the set before the fault"
        );
        let token = std::str::from_utf8(GUEST_TOKEN).expect("ASCII");
        let now = session.now();
        let line = format!("Guru Meditation Error: wifi pass={token}\n");
        session
            .machine()
            .io()
            .serial_write(SerialStream::UsjTx, line.as_bytes(), now);
        let error = ApiError::new(crate::error::E_GUEST_PANIC, "the guest panicked");
        let filled = crate::commands::inspect::fault_envelope_of(session, error);
        let tail = filled.serial_tail.join("\n");
        assert!(tail.contains("wifi pass="), "the line is kept: {tail}");
        assert!(
            !tail.contains(token),
            "the guest's credential leaked: {tail}"
        );
        let text = serde_json::to_string(&filled.detail).expect("JSON");
        assert!(!text.contains(token), "{text}");
        pool.destroy(id).expect("ends");
    }

    #[test]
    fn a_cardid_window_the_guest_programmed_is_labelled_by_its_content() {
        use crate::commands::start::{Boot, StartArgs};
        use crate::instance::Lifecycle;
        let _world = world();
        let cardid = pemu_machine::snapshot::CARDID_WINDOW;
        let image = guest_image_writing_at(cardid.start);
        let flash = pemu_loader::bundle::FlashImage::from_merged(&image).expect("merged");
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            flash,
            None,
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        let machine =
            pemu_machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
                .expect("composes");
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "guest".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&start, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        pool.session_mut(id)
            .expect("attached")
            .machine()
            .run(pemu_machine::run::RunLimits::insns(5_000_000));
        files(BTreeMap::clear);
        with_seams(|seams| {
            seams.set_hooks(MACHINE_HOOKS);
            crate::artifact_io::set(crate::artifact_io::ArtifactIo {
                write: test_write,
                read: test_read,
            });
            seams.set_salt(SALT.to_vec());
        });
        let mut window = vec![0xFF; cardid.len()];
        window[..GUEST_TOKEN.len()].copy_from_slice(GUEST_TOKEN);
        window[GUEST_TOKEN.len()] = 0;
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"now"})),
        )
        .expect("a guest write does not taint");
        assert_eq!(
            out.json["redacted_labels"],
            serde_json::json!([crate::redact::redacted_label(SALT, &window)]),
            "{}",
            out.json
        );
        pool.destroy(id).expect("ends");
    }

    #[test]
    fn an_image_that_holds_a_credential_value_loads_tainted() {
        use crate::commands::start::StartArgs;
        let _world = world();
        let mut image = guest_image();
        image[0x9080..0x9080 + GUEST_TOKEN.len()].copy_from_slice(GUEST_TOKEN);
        image[0x9080 + GUEST_TOKEN.len()] = 0;
        let flash = pemu_loader::bundle::FlashImage::from_merged(&image).expect("merged");
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            flash,
            None,
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        let machine =
            pemu_machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
                .expect("composes");
        let mut pool = Pool::new();
        let id = pool.attach(&StartArgs::default(), Box::new(machine));
        assert!(pool.with_store(|store| {
            store.is_tainted(id)
                && store
                    .secret_set(id)
                    .count_by_kind()
                    .contains_key(&MemberKind::NvsCredential)
        }));
        pool.destroy(id).expect("ends");
    }

    #[test]
    fn a_member_held_as_register_words_is_redacted_and_the_vector_still_decodes() {
        let secrets = secrets();
        let member = secrets
            .members()
            .iter()
            .find(|m| m.kind == MemberKind::NvsCredential && m.bytes.len() >= 8)
            .expect("a raw credential member")
            .bytes
            .clone();
        let mut padded = member.clone();
        padded.resize(member.len().div_ceil(4) * 4, 0x11);
        let mut words: Vec<u32> = vec![0x1234_5678, 7];
        words.extend(
            padded
                .chunks(4)
                .map(|c| u32::from_le_bytes(c.try_into().expect("4"))),
        );
        words.push(0xCAFE_F00D);
        let mut bytes = pemu_core::snap::serde_section(&words, 1, "words")
            .expect("encodes")
            .bytes;
        assert!(!bytes.windows(member.len()).any(|w| w == member.as_slice()));
        assert!(holds(&bytes, &member));

        let redactor = Redactor::new(&secrets);
        let report = redact_register_words(&mut bytes, &redactor);
        assert!(!report.is_empty());
        assert!(!holds(&bytes, &member));
        assert!(!register_words_hold_a_member(&bytes, &redactor));
        let section = pemu_core::snap::Section {
            version: 1,
            codec: pemu_core::snap::Codec::Postcard,
            bytes,
        };
        let back: Vec<u32> = pemu_core::snap::serde_from_section(
            &section,
            pemu_core::snap::SectionId("words".to_owned()),
            1,
            "words",
        )
        .expect("still a vector of words");
        assert_eq!(back.len(), words.len());
        assert_eq!(
            (back[0], back[1], back[back.len() - 1]),
            (0x1234_5678, 7, 0xCAFE_F00D)
        );
    }

    #[test]
    fn only_the_erased_cardid_window_is_labelled() {
        use pemu_machine::snapshot::{Erased, Redaction};
        let mut redaction = Redaction {
            erased: vec![
                Erased::CardId {
                    range: 0x35_6000..0x35_A000,
                    content: CARDID.to_vec(),
                },
                Erased::Nvs {
                    range: 0x9000..0xF000,
                },
            ],
            spi_mem_cleared: false,
        };
        let shown = format!("{redaction:?}");
        assert!(
            shown.contains("content_len: 32") && !shown.contains("content:"),
            "Debug prints the window's length, never its content: {shown}"
        );
        assert_eq!(
            erased_labels(&mut redaction, SALT),
            vec![crate::redact::redacted_label(SALT, &CARDID)]
        );
        assert!(
            matches!(&redaction.erased[0], Erased::CardId { content, .. } if content.is_empty()),
            "the content is dropped once its label is taken"
        );
        let mut erased = Redaction {
            erased: vec![Erased::CardId {
                range: 0x35_6000..0x35_A000,
                content: vec![0xFF; 0x4000],
            }],
            spi_mem_cleared: false,
        };
        assert!(
            erased_labels(&mut erased, SALT).is_empty(),
            "an erased window gets no label"
        );
    }

    #[test]
    fn an_untainted_instance_exports_a_redacted_snapshot() {
        let _world = world();
        let (mut pool, id) = instance_with_secrets();
        pool.with_store(|store| store.set_secret_set(id, SecretSet::builder().build()));
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"m"})),
        )
        .expect("an untainted machine exports");
        assert_eq!(out.json["redacted"], true);
        assert!(out.receipt.redacted);
        assert_eq!(out.json["path"], "snapshots/m.snap");
        assert_eq!(
            out.json["sha256"].as_str().expect("a hash").len(),
            64,
            "sha256 in hex"
        );
    }

    /// Two pools mint the same `p1`; the taint, `SecretSet` and snapshots of one never answer for
    /// the other.
    #[test]
    fn two_pools_that_mint_the_same_id_never_share_a_taint() {
        let _world = world();
        let (mut tainted, a) = instance_with_secrets();
        let (mut clean, b) = instance_with_secrets();
        assert_eq!(a, b, "every new pool mints `p1` first");
        clean.with_store(|store| store.set_secret_set(b, SecretSet::builder().build()));
        snapshot_on_pool(
            &mut tainted,
            &args(serde_json::json!({"op":"save","name":"only-in-a"})),
        )
        .expect("save");

        let refused = snapshot_on_pool(
            &mut tainted,
            &args(serde_json::json!({"op":"export","name":"m"})),
        )
        .expect_err("the tainted pool's p1 refuses");
        assert_eq!(refused.code, E_SECRET_REFUSED);
        let out = snapshot_on_pool(
            &mut clean,
            &args(serde_json::json!({"op":"export","name":"m"})),
        )
        .expect("the clean pool's p1 exports");
        assert!(!out.receipt.tainted, "{:?}", out.receipt);
        let listed =
            snapshot_on_pool(&mut clean, &args(serde_json::json!({"op":"list"}))).expect("list");
        assert_eq!(listed.json["snapshots"], serde_json::json!([]), "a's save");

        clean.with_store(|store| store.forget(b));
        assert!(tainted.with_store(|store| store.is_tainted(a)));
        assert!(
            tainted
                .session_mut(a)
                .expect("the session")
                .receipt()
                .tainted
        );
    }

    #[test]
    fn an_imported_redacted_snapshot_carries_redacted_into_the_receipt() {
        let _world = world();
        let (mut pool, id) = instance_with_secrets();
        pool.with_store(|store| store.set_secret_set(id, SecretSet::builder().build()));
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"m"})),
        )
        .expect("export");
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"import","name":"m"})),
        )
        .expect("import");
        assert_eq!(out.json["redacted"], true);
        assert!(out.receipt.redacted);
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"m"})),
        )
        .expect("restore");
        assert!(out.receipt.redacted, "a redacted snapshot stays redacted");

        // A save taken on redacted state restores redacted too, and exporting it still runs the
        // machine's redaction.
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"after"})),
        )
        .expect("save");
        let saved = pool
            .with_store(|store| store.get(id, "after").cloned())
            .expect("saved");
        assert!(saved.from_redacted && !saved.snapshot.header.redacted);
        let listed =
            snapshot_on_pool(&mut pool, &args(serde_json::json!({"op":"list"}))).expect("list");
        let row = listed.json["snapshots"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["name"] == "after"))
            .cloned()
            .expect("listed");
        assert_eq!(row["redacted"], true, "{row}");
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"after"})),
        )
        .expect("restore");
        assert!(
            out.receipt.redacted,
            "a save of redacted state restores redacted"
        );
        assert_eq!(out.json["redacted"], true);
    }

    fn live_restore(_machine: &mut dyn SnapshotMachine, _s: &Snapshot) -> Result<(), SnapError> {
        Err(SnapError::LiveBridge {
            bridge: "the WISP relay".to_owned(),
        })
    }

    fn live_fork(
        _machine: &mut dyn SnapshotMachine,
        _live: LivePolicy,
    ) -> Result<Box<dyn SnapshotMachine + Send>, SnapError> {
        Err(SnapError::LiveBridge {
            bridge: "the WISP relay".to_owned(),
        })
    }

    #[test]
    fn a_live_bridge_refusal_is_e_state_naming_the_bridge() {
        let _world = world();
        let (mut pool, id) = instance_with_secrets();
        pool.with_store(|store| store.set_secret_set(id, SecretSet::builder().build()));
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"menu"})),
        )
        .expect("save");
        with_seams(|seams| {
            seams.set_hooks(SnapshotHooks {
                take: scripted_take,
                restore: live_restore,
                fork: live_fork,
            });
        });
        for op in [
            serde_json::json!({"op":"restore","name":"menu"}),
            serde_json::json!({"op":"fork","name":"menu","count":1}),
        ] {
            let error = snapshot_on_pool(&mut pool, &args(op.clone())).expect_err("refused");
            assert_eq!(error.code, E_STATE, "{op}: {}", error.message);
            assert!(error.message.contains("WISP relay"), "{}", error.message);
            assert!(
                error.hint.as_deref().is_some_and(|h| h.contains("detach")),
                "{:?}",
                error.hint
            );
        }
        with_seams(|seams| seams.set_hooks(SCRIPTED_HOOKS));
        let other = machine_hook_error("restore a snapshot", &SnapError::BadMagic);
        assert_eq!(
            other.code, E_SNAPSHOT,
            "every other refusal keeps E_SNAPSHOT"
        );
    }

    #[test]
    fn save_list_restore_and_delete_are_one_lifecycle() {
        let _world = world();
        let (mut pool, id) = instance_with_secrets();
        pool.with_store(|store| store.set_secret_set(id, SecretSet::builder().build()));
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"menu"})),
        )
        .expect("save");
        let out = snapshot_on_pool(&mut pool, &args(serde_json::json!({"op":"list"})))
            .expect("list always answers");
        assert_eq!(out.json["snapshots"][0]["name"], "menu");
        assert!(out.json["snapshots"][0]["size_bytes"].as_u64().unwrap_or(0) > 0);
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"menu"})),
        )
        .expect("restore");
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"delete","name":"menu"})),
        )
        .expect("delete");
        let error = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"menu"})),
        )
        .expect_err("it is gone");
        assert_eq!(error.code, E_SNAPSHOT);
        let out =
            snapshot_on_pool(&mut pool, &args(serde_json::json!({"op":"list"}))).expect("list");
        assert_eq!(out.text, "no snapshot");
    }

    #[test]
    fn a_snapshot_of_one_instance_does_not_wait_for_another_that_is_busy() {
        let _world = world();
        let (mut pool, p1) = real_instance();
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"f","count":1,"instance":"p1"})),
        )
        .expect("fork");
        let p2 = InstanceId::parse(out.json["instances"][0].as_str().expect("an id"))
            .expect("a minted id");
        let busy = pool.checkout(p1).expect("p1 is in the pool");
        let saved = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"s","instance": p2.to_string()})),
        )
        .expect("p2 is not busy");
        assert_eq!(saved.json["instance"], p2.to_string());
        let refused = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"s","instance":"p1"})),
        )
        .expect_err("p1 is checked out");
        assert_eq!(refused.code, E_STATE);
        assert!(refused.retryable, "{refused:?}");
        pool.checkin(busy);
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"s","instance":"p1"})),
        )
        .expect("back in the pool");
    }

    /// A pool with one instance on the real `Machine` (bundled ROM, erased flash, synthesized
    /// eFuse) and the default hooks.
    pub(crate) fn real_instance() -> (Pool, InstanceId) {
        real_instance_on(pemu_machine::config::TimingProfileId::Fast)
    }

    pub(crate) fn real_instance_on(
        profile: pemu_machine::config::TimingProfileId,
    ) -> (Pool, InstanceId) {
        use pemu_loader::bundle::FlashImage;
        use pemu_loader::efuse_image::EfuseImage;
        use pemu_machine::config::{Assets, MachineConfig};

        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let machine = pemu_machine::Machine::new(
            MachineConfig {
                profile,
                ..MachineConfig::default()
            },
            assets,
        )
        .expect("the ROM fits the ROM window");
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            profile,
            ..StartArgs::default()
        };
        let id = pool.attach(&start, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("the instance was just created")
            .transition(crate::instance::Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        files(BTreeMap::clear);
        with_seams(|seams| {
            seams.set_hooks(MACHINE_HOOKS);
            crate::artifact_io::set(crate::artifact_io::ArtifactIo {
                write: test_write,
                read: test_read,
            });
            seams.set_salt(SALT.to_vec());
        });
        pool.with_store(|store| store.forget(id));
        (pool, id)
    }

    fn run_insns(pool: &mut Pool, id: InstanceId, insns: u64) {
        let session = pool.session_mut(id).expect("the instance");
        session
            .machine()
            .run(pemu_machine::run::RunLimits::insns(insns));
    }

    fn hash_of(pool: &mut Pool, id: InstanceId) -> [u8; 32] {
        pool.session_mut(id)
            .expect("the instance")
            .snapshot_machine()
            .state_hash()
    }

    #[test]
    fn save_restore_fork_and_export_work_on_the_real_machine() {
        let _world = world();
        let (mut pool, id) = real_instance();
        run_insns(&mut pool, id, 20_000);
        let saved = hash_of(&mut pool, id);
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"a"})),
        )
        .expect("save");

        run_insns(&mut pool, id, 20_000);
        assert_ne!(hash_of(&mut pool, id), saved);
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"a"})),
        )
        .expect("restore");
        assert_eq!(hash_of(&mut pool, id), saved);

        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"f","count":1})),
        )
        .expect("fork");
        let fork = InstanceId::parse(out.json["instances"][0].as_str().expect("an id"))
            .expect("a minted id");
        assert_eq!(hash_of(&mut pool, fork), saved);
        run_insns(&mut pool, fork, 5_000);
        assert_ne!(hash_of(&mut pool, fork), saved);
        assert_eq!(hash_of(&mut pool, id), saved, "the parent runs apart");

        // Exporting the named save runs the machine's own redaction on it first.
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"a","instance":"p1"})),
        )
        .expect("export");
        let path = out.json["path"].as_str().expect("a path").to_owned();
        let exported = import_bytes(&test_read(&path).expect("written")).expect("reads");
        let original = pool
            .with_store(|store| store.get(id, "a").cloned())
            .expect("saved");
        assert!(exported.header.redacted && exported.header.exported);
        assert_eq!(exported.header.efuse_hash, [0; 32]);
        let efuse = SectionId::soc("efuse");
        assert_ne!(
            exported.sections.get(&efuse),
            original.snapshot.sections.get(&efuse),
            "the eFuse words were zeroed"
        );

        // The import restores as `redacted`, as does every later receipt; restoring an unredacted
        // save clears it.
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"import","name":"x","path":path,"instance":"p1"})),
        )
        .expect("import");
        let restored = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"x","instance":"p1"})),
        )
        .expect("restore");
        assert!(restored.receipt.redacted);
        run_insns(&mut pool, id, 1_000);
        let session = pool.session_mut(id).expect("the instance");
        assert!(
            session.receipt().redacted,
            "a later receipt still says redacted"
        );
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"a","instance":"p1"})),
        )
        .expect("restore");
        let session = pool.session_mut(id).expect("the instance");
        assert!(!session.receipt().redacted);
    }

    /// Two halves, because either alone could pass on a receipt that guessed.
    #[test]
    fn the_selected_profile_survives_a_snapshot_and_restore() {
        use pemu_machine::config::TimingProfileId;

        let _world = world();
        let (mut pool, id) = real_instance_on(TimingProfileId::Device);
        run_insns(&mut pool, id, 20_000);
        let saved = hash_of(&mut pool, id);
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"d"})),
        )
        .expect("save");
        assert_eq!(out.receipt.profile, "device", "the save's own receipt");

        run_insns(&mut pool, id, 20_000);
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"restore","name":"d"})),
        )
        .expect("restore");
        assert_eq!(hash_of(&mut pool, id), saved, "the state came back");
        assert_eq!(out.receipt.profile, "device", "the restore's receipt");
        let session = pool.session_mut(id).expect("the instance");
        assert_eq!(
            session.receipt().profile,
            "device",
            "and every later receipt of the restored instance"
        );

        // The same save on a `fast` instance is refused on identity, and the hint names the
        // profile.
        let snapshot = pool
            .with_store(|store| store.get(id, "d").cloned())
            .expect("saved");
        let (mut other, other_id) = real_instance_on(TimingProfileId::Fast);
        let session = other.session_mut(other_id).expect("the instance");
        let err = session
            .snapshot_machine()
            .restore(&snapshot.snapshot)
            .expect_err("a `fast` instance is not the machine that saved this");
        assert!(
            matches!(
                err,
                SnapError::IdentityMismatch {
                    field: pemu_core::snap::IdentityField::Config
                }
            ),
            "{err:?}"
        );
        assert!(
            identity_hint(&err)
                .expect("an identity hint")
                .contains("profile"),
            "the hint names the profile"
        );
        assert_eq!(
            session.receipt().profile,
            "fast",
            "and the refused instance still reports its own profile"
        );
    }

    #[test]
    fn a_fork_continues_the_mic_numbering_of_the_journal_it_copied() {
        use crate::commands::mic_set::MicState;
        use pemu_machine::machine::At;

        let _world = world();
        let (mut pool, id) = real_instance();
        let parent = pool.session_mut(id).expect("the instance");
        for seq in 0..3 {
            parent
                .machine()
                .input(
                    At::Vt(VTime::from_ms(5 * (seq + 1))),
                    InputEvent::MicChunk {
                        seq,
                        samples: vec![0; 16],
                    },
                )
                .expect("journaled");
        }
        parent.mic = MicState::default();
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"f","count":1})),
        )
        .expect("fork");
        let fork = InstanceId::parse(out.json["instances"][0].as_str().expect("an id"))
            .expect("a minted id");
        let mic = pool.session(fork).expect("the fork").mic;
        assert_eq!(mic.next_seq, 3);
        assert_eq!(mic.pending_until, VTime::from_ms(15));
    }

    #[test]
    fn fork_creates_count_instances_and_vary_seed_gives_each_its_own() {
        let _world = world();
        let (mut pool, id) = instance_with_secrets();
        let base = pool.session(id).expect("the instance").seed;
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"menu","count":4,"vary_seed":true})),
        )
        .expect("fork");
        let made: Vec<String> = out.json["instances"]
            .as_array()
            .expect("the new ids")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(made, vec!["p2", "p3", "p4", "p5"]);
        let seeds: Vec<u64> = made
            .iter()
            .map(|name| {
                let id = InstanceId::parse(name).expect("a minted id");
                pool.session(id).expect("the fork").seed
            })
            .collect();
        assert_eq!(
            seeds,
            vec![
                base.wrapping_add(1),
                base.wrapping_add(2),
                base.wrapping_add(3),
                base.wrapping_add(4)
            ]
        );
    }

    #[test]
    fn a_fork_without_vary_seed_keeps_the_parent_seed() {
        let _world = world();
        let (mut pool, id) = instance_with_secrets();
        let base = pool.session(id).expect("the instance").seed;
        snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"menu","count":2})),
        )
        .expect("fork");
        for name in ["p2", "p3"] {
            let id = InstanceId::parse(name).expect("a minted id");
            assert_eq!(pool.session(id).expect("the fork").seed, base);
        }
    }

    #[test]
    fn a_missing_machine_hook_is_a_named_refusal_and_not_an_empty_snapshot() {
        let _world = world();
        let (mut pool, id, _) = journaled();
        pool.with_store(|store| store.forget(id));
        with_seams(|seams| seams.set_hooks(NO_HOOKS));
        let error = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"save","name":"m"})),
        )
        .expect_err("there is no machine to snapshot");
        assert_eq!(error.code, E_SNAPSHOT);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("SnapshotMachine")),
            "{:?}",
            error.hint
        );
    }

    #[test]
    fn every_argument_outside_the_schema_is_usage() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "op": "rewind" }),
            serde_json::json!({ "op": "save" }),
            serde_json::json!({ "op": "save", "name": "a/b" }),
            serde_json::json!({ "op": "save", "name": "" }),
            serde_json::json!({ "op": "save", "name": "m", "count": 2 }),
            serde_json::json!({ "op": "fork", "name": "m", "count": 0 }),
            serde_json::json!({ "op": "fork", "name": "m", "count": 17 }),
            serde_json::json!({ "op": "export", "name": "m", "path": "/etc/passwd" }),
            serde_json::json!({ "op": "export", "name": "m", "path": "../up" }),
            serde_json::json!({ "op": "list", "nonsense": 1 }),
        ] {
            assert_eq!(
                SnapshotArgs::from_json(&bad)
                    .expect_err("outside the schema")
                    .code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    /// Existing MCP clients were written to the `load` spelling.
    #[test]
    fn load_is_an_accepted_spelling_of_restore() {
        assert_eq!(SnapOp::parse_arg("load"), Some(SnapOp::Restore));
        assert_eq!(SnapOp::parse_arg("restore"), Some(SnapOp::Restore));
        assert_eq!(SnapOp::parse_arg("rewind"), None);
    }

    #[test]
    fn a_default_artifact_path_is_relative_and_forward_slashed() {
        let args = args(serde_json::json!({"op":"export","name":"menu"}));
        let path = args.artifact_path();
        assert_eq!(path, "snapshots/menu.snap");
        crate::output::check_artifact_path(&path).expect("a valid artifact path");
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("snapshot").expect("#[command] registered snapshot");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            SnapshotArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.needs_instance);
        assert_eq!(spec.scenario_step, Some("snapshot"));
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack.len() >= needle.len()
            && haystack.windows(needle.len()).any(|w| w == needle)
    }
}
