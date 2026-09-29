//! The producer audit of [`pemu_api::receipt::Receipt`]: one row per field, saying what assigns it
//! other than `default()` ([`Row::producer`]) and who reads it and what the default makes them do
//! ([`Row::consumers`], [`Row::note`]). No producer with a real consumer is a live defect; with no
//! consumer it is a reporting bug. Unit tests assign fields by hand and `..Receipt::default()`
//! hides an absence, so this is kept as data.
//!
//! The guard counts each field named through a `receipt` binding in `pemu-api`, `pemu-host`,
//! `pemu-cli`, `pemu-wasm` and `xtask`; a field with no producer must keep its recorded count, so a
//! new consumer of a permanently defaulted field trips it. It does not catch a field being fed:
//! producers write inside the `Receipt { .. }` literal, which names no binding. Whoever feeds a
//! field must update this table.
//!
//! This test reads the repository's own Rust sources and nothing else.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    /// On a real path; the name is in [`Row::producer_site`].
    Yes,
    /// Nothing assigns it per run, and nothing has to: the field states a fact about the build, and
    /// its `Receipt::default()` value is that fact, from a named constant.
    Constant,
    /// The field holds its `Receipt::default()` value for the life of the build.
    No,
}

struct Row {
    name: &'static str,
    producer: Producer,
    /// Or why nothing does.
    producer_site: &'static str,
    consumers: &'static str,
    note: &'static str,
    /// For a [`Producer::No`] field, how many times the production sources name it. Ignored for a
    /// produced field, whose count moves with ordinary work.
    unfed_mentions: usize,
}

/// In the order `Receipt` declares its fields.
const AUDIT: &[Row] = &[
    Row {
        name: "vt_us",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from `backend.now().as_us()`",
        consumers: "`Output::new` and most commands' bodies",
        note: "fine",
        unfed_mentions: 0,
    },
    Row {
        name: "insns",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from `self.insns`",
        consumers: "`status`, `stop`, `clock`, `endpoint`, `pemu_host::pool`",
        note: "fine",
        unfed_mentions: 0,
    },
    Row {
        name: "profile",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from the machine's `MachineConfig`",
        consumers: "`pemu_host::boot_cache` (cache key), `Receipt::one_line`",
        note: "fine; it was this shape's first find",
        unfed_mentions: 0,
    },
    Row {
        name: "cpi_milli",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from the machine's own `Clock::cpi_milli` carried on \
                        `pemu_machine::machine::Receipt::cpi_milli`",
        consumers: "`Receipt::to_json` only, so an agent reading the receipt",
        note: "fine; carried from the machine's `Clock`, so a `--profile device` receipt states \
               the ratio the run used",
        unfed_mentions: 0,
    },
    Row {
        name: "efuse",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from the machine's `MachineConfig::efuse` carried on \
                        `pemu_machine::machine::Receipt::efuse`",
        consumers: "`Receipt::to_json`, so an agent reading the receipt",
        note: "fine; a machine started on `--efuse-dump` says so",
        unfed_mentions: 0,
    },
    Row {
        name: "tainted",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, the machine's `is_tainted` OR the secret store",
        consumers: "`pemu_host::boot_cache::store_for_session` (in-memory or on-disk store), \
                    `snapshot export`, `Receipt::one_line`",
        note: "fine; the source set is narrower than the full list of secret sources",
        unfed_mentions: 0,
    },
    Row {
        name: "determinism",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from the machine's journal class",
        consumers: "`Receipt::one_line`",
        note: "fine",
        unfed_mentions: 0,
    },
    Row {
        name: "journal_len",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from `Machine::journal().entries().len()` carried on \
                        `pemu_machine::machine::Receipt::journal_len`",
        consumers: "`Receipt::to_json` only",
        note: "fine; journaled input is part of run identity, so the length is too",
        unfed_mentions: 0,
    },
    Row {
        name: "classes_touched",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from `Machine::classes_touched()` over the fidelity \
                        ledger, carried on `pemu_machine::machine::Receipt::classes_touched`",
        consumers: "`inspect fidelity` (text and JSON), and `Receipt::caveats`",
        note: "fine; without it no run could be `pass_with_caveats` for a class-U touch and \
               `--strict` could never exit 7 UNMODELED_HW. The entries name the subject that carries the class (a block, or \
               `block.REGISTER` where the note is a register's), not every register touched, so \
               the list is bounded by the block table",
        unfed_mentions: 3,
    },
    Row {
        name: "unmodeled_first_touch",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from `Machine::unmodeled_first_touch()` over \
                        `FidelityLedger::unmodeled`",
        consumers: "`Receipt::caveats` only",
        note: "fine; same carry as `classes_touched` and the register-level \
               half of it, in first-touch order, capped at \
               `pemu_machine::machine::UNMODELED_LIMIT` with a final `... N more`",
        unfed_mentions: 0,
    },
    Row {
        name: "hle",
        producer: Producer::Yes,
        producer_site: "`binding_into_receipt` writes `hle.bound` and \
                        `hle.synthesized_log_lines`; `Session::receipt` writes \
                        `hle.tripwires_hit` from `Session::tripwires_hit`",
        consumers: "`Receipt::to_json`; `hle.tripwires_hit` also feeds `Receipt::caveats`",
        note: "fully fed. `hle.tripwires_hit` is the session's, not the \
               machine's: a tripwire that fires is a `StopReason::Tripwire` that ends the slice \
               and is reported as `E_TRIPWIRE`, and the machine records only the tripwires it \
               armed, so the `RunOutcome` the session already reads is the one place a hit \
               exists. \
               `hle.synthesized_log_lines` carries the total; \
               `extra[\"binding\"][\"log_lines\"][m]` keeps only the `verified` word, \
               which a total cannot say",
        unfed_mentions: 0,
    },
    Row {
        name: "timing_lint",
        producer: Producer::Yes,
        producer_site: "`Session::receipt`, from `St7789p3::warnings()` carried on \
                        `pemu_machine::machine::Receipt::timing_lint`",
        consumers: "`Receipt::caveats` only",
        note: "fine. Read from the panel rather than drained with \
               `take_warnings`, because a command layer builds a receipt for every response and \
               a drained lint would land in whichever one came next",
        unfed_mentions: 0,
    },
    Row {
        name: "fidelity",
        producer: Producer::No,
        producer_site: "nothing; the per-subsystem table is never built",
        consumers: "`inspect fidelity`, `status`, `Receipt::one_line`",
        note: "recorded gap, not a silent one: `inspect::fidelity_text` and \
               `status::fidelity` report an empty table rather than none, and `one_line` omits \
               the segment entirely rather than printing an empty claim, so nothing false is \
               reported",
        unfed_mentions: 4,
    },
    Row {
        name: "redacted",
        producer: Producer::Yes,
        producer_site: "`Session::receipt` from `self.redacted`, and `snapshot`",
        consumers: "`snapshot`, `Receipt::one_line`",
        note: "fine",
        unfed_mentions: 0,
    },
    Row {
        name: "host_parity",
        producer: Producer::Constant,
        producer_site: "`Receipt::default`, from `pemu_api::receipt::HOST_PARITY`. \
                        Deliberately not a per-run assignment: it is \
                        the parity of the *committed goldens*, which is the same answer \
                        for every receipt this build writes, including one written by the wasm \
                        build in a browser",
        consumers: "none but `Receipt::to_json`",
        note: "fine; the `cross-host-parity` check compares the committed golden on macOS \
               and on Windows, and the constant names both hosts",
        unfed_mentions: 0,
    },
    Row {
        name: "extra",
        producer: Producer::Yes,
        producer_site: "`binding_into_receipt`, `heap_ledger_into_receipt`, \
                        `fault_counters_into_receipt`",
        consumers: "`status` (`fault_counters`), the redaction pass",
        note: "fine",
        unfed_mentions: 6,
    },
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// In path order, relative to `root`.
fn sources(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("the source directory is readable")
        .map(|e| e.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).expect("a source file is UTF-8");
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string()
                .replace('\\', "/");
            out.push((rel, text));
        }
    }
}

/// Line comments removed, so documentation may name a field without counting, then whitespace, so a
/// read split over lines still counts. Unit tests are not cut out: some files carry several
/// `#[cfg(test)]`, and a test asserting on an unproduced field belongs in the audit.
fn code_of(text: &str) -> String {
    let code: String = text
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    code.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `pemu-machine` and below cannot name a receipt field: the receipt is `pemu-api`'s.
const SCANNED: &[&str] = &[
    "crates/pemu-api/src",
    "crates/pemu-host/src",
    "crates/pemu-cli/src",
    "crates/pemu-wasm/src",
    "xtask/src",
];

/// `receipt.rs` itself is left out: its own module renders and derives from the fields, which are
/// not the consumers this audit is about.
fn scanned_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in SCANNED {
        sources(&root, &root.join(dir), &mut files);
    }
    files.retain(|(p, _)| !p.ends_with("pemu-api/src/receipt.rs"));
    assert!(
        files.len() > 100,
        "the scan found the workspace's sources, not {} files",
        files.len()
    );
    files
}

/// Through a binding called `receipt` or a `receipt()` call.
fn mentions(files: &[(String, String)], field: &str) -> Vec<(String, usize)> {
    let direct = format!("receipt.{field}");
    let called = format!("receipt().{field}");
    files
        .iter()
        .map(|(path, text)| {
            let code = code_of(text);
            (
                path.clone(),
                code.matches(&direct).count() + code.matches(&called).count(),
            )
        })
        .filter(|(_, n)| *n > 0)
        .collect()
}

#[test]
fn the_receipt_audit_covers_every_field_in_declaration_order() {
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join("crates/pemu-api/src/receipt.rs"))
        .expect("the receipt module is readable");
    let body = text
        .split_once("pub struct Receipt {")
        .expect("the `Receipt` struct is declared")
        .1
        .split_once("\n}")
        .expect("the `Receipt` struct is closed")
        .0;
    let declared: Vec<&str> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub "))
        .filter_map(|rest| rest.split(':').next())
        .map(str::trim)
        .collect();
    let audited: Vec<&str> = AUDIT.iter().map(|row| row.name).collect();
    assert_eq!(
        declared, audited,
        "every `Receipt` field carries an audit row, in declaration order. A field added without \
         one is a permanently defaulted field waiting to happen: write the row, answering what \
         assigns it other than `default()` and who reads it"
    );
}

#[test]
fn the_receipt_audit_sees_an_unfed_field_gain_a_consumer_or_a_producer() {
    let files = scanned_sources();
    let mut wrong = Vec::new();
    for row in AUDIT.iter().filter(|r| r.producer == Producer::No) {
        let sites = mentions(&files, row.name);
        let found: usize = sites.iter().map(|(_, n)| n).sum();
        if found != row.unfed_mentions {
            wrong.push(format!(
                "`{name}`: the audit recorded {want} mention(s), the sources have {found} \
                 ({sites:?}).\n  producer as audited: {producer}\n  consumers as audited: \
                 {consumers}\n  verdict as audited: {note}\n  Either something now feeds the \
                 field, and the row becomes `Producer::Yes` with the producer named; or it gained \
                 a reader, and a reader of a permanently defaulted field acts on a fact the run \
                 never established. Feed the field first, then update this row",
                name = row.name,
                want = row.unfed_mentions,
                producer = row.producer_site,
                consumers = row.consumers,
                note = row.note,
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
}

#[test]
fn the_receipt_audit_and_the_json_known_keys_name_the_same_fields() {
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join("crates/pemu-api/src/receipt.rs"))
        .expect("the receipt module is readable");
    let keys = text
        .split_once("const KNOWN_KEYS: &[&str] = &[")
        .expect("`KNOWN_KEYS` is declared")
        .1
        .split_once("];")
        .expect("`KNOWN_KEYS` is closed")
        .0;
    let known: Vec<String> = keys
        .split(',')
        .map(|k| k.trim().trim_matches('"').to_string())
        .filter(|k| !k.is_empty())
        .collect();
    // `extra` has no key of its own: its entries are written at the top level.
    let audited: Vec<String> = AUDIT
        .iter()
        .map(|row| row.name)
        .filter(|name| *name != "extra")
        .map(str::to_string)
        .collect();
    assert_eq!(
        known, audited,
        "`KNOWN_KEYS` and the audit name the same fields. A field that is written to the JSON \
         without an audit row, or audited without reaching the JSON, is a receipt that reports \
         something nobody walked"
    );
}
