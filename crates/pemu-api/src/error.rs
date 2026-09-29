//! Registered error codes and the API error.
//!
//! A code is a name plus a number, not a closed enum. Each caps group reserves a number range, and
//! a command registers new codes in its group's range through `#[command(errors(..))]`.
//! `check_registry` rejects duplicate names or numbers across the registered set.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use pemu_introspect::unwind::Frame;

use crate::registry::commands;
use crate::spec::CapsGroup;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ErrorCode {
    pub name: &'static str,
    pub number: u16,
}

/// The error a command returns; the wasm ABI carries it as the JSON payload of a result whose
/// status is `code.number`. The larger fields are boxed so `Result<Output, ApiError>` stays under
/// `clippy::result_large_err` ([`ApiError::SIZE_LIMIT`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub vt_us: u64,
    pub retryable: bool,
    pub hint: Option<Box<str>>,
    pub detail: Box<serde_json::Value>,
    pub serial_tail: Box<[String]>,
    pub backtrace: Box<[Frame]>,
}

impl ApiError {
    /// `vt_us` stays 0 until the command layer stamps the instance's virtual time.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        debug_assert!(
            code.number != 0,
            "number 0 is the wasm STATUS_OK, not an error code"
        );
        ApiError {
            code,
            message: message.into(),
            vt_us: 0,
            retryable: false,
            hint: None,
            detail: Box::new(serde_json::Value::Null),
            serial_tail: Box::default(),
            backtrace: Box::default(),
        }
    }

    /// The same call can be repeated, such as after `E_TIMEOUT`.
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// The next useful call for an agent.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into().into_boxed_str());
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Box::new(detail);
        self
    }

    pub fn at_vt_us(mut self, vt_us: u64) -> Self {
        self.vt_us = vt_us;
        self
    }

    pub fn with_serial_tail(mut self, lines: Vec<String>) -> Self {
        self.serial_tail = lines.into_boxed_slice();
        self
    }

    pub fn with_backtrace(mut self, frames: Vec<Frame>) -> Self {
        self.backtrace = frames.into_boxed_slice();
        self
    }

    /// The size `clippy::result_large_err` allows by default; a unit test keeps `ApiError` below
    /// it.
    pub const SIZE_LIMIT: usize = 128;

    /// Never 0, because 0 is `STATUS_OK`: registered codes cannot be 0 and `new` debug-asserts it.
    pub fn status(&self) -> u32 {
        u32::from(self.code.number)
    }

    /// The typed error envelope: the wasm result payload, the MCP `structuredContent.error` and the
    /// HTTP or WS `error` object. `hint` is present only when set.
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("code".into(), self.code.name.into());
        map.insert("number".into(), self.code.number.into());
        map.insert("message".into(), self.message.clone().into());
        map.insert("vt_us".into(), self.vt_us.into());
        map.insert("retryable".into(), self.retryable.into());
        map.insert("detail".into(), (*self.detail).clone());
        map.insert(
            "serial_tail".into(),
            self.serial_tail
                .iter()
                .map(|l| serde_json::Value::from(l.as_str()))
                .collect(),
        );
        map.insert(
            "backtrace".into(),
            self.backtrace.iter().map(frame_json).collect(),
        );
        if let Some(hint) = &self.hint {
            map.insert("hint".into(), (&**hint).into());
        }
        serde_json::Value::Object(map)
    }

    /// Reads back what [`ApiError::to_json`] wrote; `None` when `value` is not an envelope of a
    /// registered code. The daemon's browser relay uses it to hand a page's error on unchanged.
    pub fn from_json(value: &serde_json::Value) -> Option<ApiError> {
        let map = value.as_object()?;
        let code = ErrorCode::lookup(map.get("code")?.as_str()?)?;
        let mut error = ApiError::new(code, map.get("message")?.as_str()?);
        error.vt_us = map
            .get("vt_us")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        error.retryable = map
            .get("retryable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        error.hint = map
            .get("hint")
            .and_then(serde_json::Value::as_str)
            .map(Into::into);
        error.detail = Box::new(map.get("detail").cloned().unwrap_or_default());
        error.serial_tail = map
            .get("serial_tail")
            .and_then(serde_json::Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        error.backtrace = map
            .get("backtrace")
            .and_then(serde_json::Value::as_array)
            .map(|frames| frames.iter().filter_map(frame_from_json).collect())
            .unwrap_or_default();
        Some(error)
    }

    pub fn to_json_text(&self) -> String {
        self.to_json().to_string()
    }

    /// The error half of every generated surface; a command's `output_schema` covers success only.
    pub fn schema() -> crate::spec::Schema {
        schemars::json_schema!({
            "type": "object",
            "description": "Typed error envelope of a failed command.",
            "required": ["code", "number", "message", "vt_us", "retryable", "detail", "serial_tail", "backtrace"],
            "properties": {
                "code": { "type": "string", "description": "Stable registered error-code name." },
                "number": { "type": "integer", "minimum": 1, "maximum": MAX_ERROR_NUMBER, "description": "Registered number, also the wasm result status." },
                "message": { "type": "string", "description": "Human-readable message." },
                "vt_us": { "type": "integer", "minimum": 0, "description": "Virtual time in microseconds when the error was raised." },
                "retryable": { "type": "boolean", "description": "Whether repeating the same call can succeed." },
                "detail": { "description": "Machine-readable detail, shape per code." },
                "serial_tail": { "type": "array", "items": { "type": "string" }, "description": "Bounded serial excerpt as evidence." },
                "backtrace": { "type": "array", "items": { "type": "object" }, "description": "Unwound guest frames." },
                "hint": { "type": "string", "description": "The next useful call for an agent." }
            }
        })
    }
}

/// One `backtrace` entry with the fields of `pemu_introspect::unwind::Frame`; keys a frame does not
/// have are left out.
fn frame_json(frame: &Frame) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(function) = &frame.function {
        map.insert("function".into(), function.clone().into());
    }
    if let Some(file) = &frame.file {
        map.insert("file".into(), file.clone().into());
    }
    if let Some(source) = &frame.source {
        map.insert("source".into(), source.clone().into());
    }
    if let Some(line) = frame.line {
        map.insert("line".into(), line.into());
    }
    if let Some(column) = frame.column {
        map.insert("column".into(), column.into());
    }
    map.insert("pc".into(), format!("{:#010x}", frame.pc).into());
    map.insert("sp".into(), format!("{:#010x}", frame.sp).into());
    map.insert("inlined".into(), frame.inlined.into());
    map.insert("origin".into(), frame.origin.tag().into());
    serde_json::Value::Object(map)
}

fn frame_from_json(value: &serde_json::Value) -> Option<Frame> {
    let hex = |key: &str| {
        let text = value.get(key)?.as_str()?;
        u32::from_str_radix(text.strip_prefix("0x")?, 16).ok()
    };
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let number = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    Some(Frame {
        pc: hex("pc")?,
        sp: hex("sp")?,
        function: text("function"),
        source: text("source"),
        file: text("file"),
        line: number("line"),
        column: number("column"),
        inlined: value
            .get("inlined")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        origin: match value.get("origin").and_then(|v| v.as_str()) {
            Some("app") => pemu_introspect::unwind::FrameOrigin::App,
            Some("rom") => pemu_introspect::unwind::FrameOrigin::Rom,
            _ => pemu_introspect::unwind::FrameOrigin::Unknown,
        },
    })
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

/// The `number` maximum of the `error@1` schema, derived from [`CapsGroup::error_range`] so a new
/// group needs no edit here.
pub const MAX_ERROR_NUMBER: u16 = max_error_number();

const fn max_error_number() -> u16 {
    let mut max = 0;
    let mut i = 0;
    while i < CapsGroup::ALL.len() {
        let end = *CapsGroup::ALL[i].error_range().end();
        if end > max {
            max = end;
        }
        i += 1;
    }
    max
}

impl CapsGroup {
    /// Number 0 is never a code, because status 0 means ok in the wasm result header. The bounds
    /// are this crate's choice.
    pub const fn error_range(self) -> RangeInclusive<u16> {
        match self {
            CapsGroup::Core => 1..=999,
            CapsGroup::Audio => 1000..=1999,
            CapsGroup::Radio => 2000..=2999,
            CapsGroup::Nfc => 3000..=3999,
            CapsGroup::Debug => 4000..=4999,
            CapsGroup::Device => 5000..=5999,
            CapsGroup::Power => 6000..=6999,
        }
    }
}

impl ErrorCode {
    pub fn group(self) -> Option<CapsGroup> {
        CapsGroup::ALL
            .into_iter()
            .find(|g| g.error_range().contains(&self.number))
    }

    /// Its number lies in the Core range or in `group`'s own range. `#[command]` asserts it at
    /// compile time.
    pub const fn allowed_in(self, group: CapsGroup) -> bool {
        const fn within(n: u16, g: CapsGroup) -> bool {
            let r = g.error_range();
            n >= *r.start() && n <= *r.end()
        }
        within(self.number, CapsGroup::Core) || within(self.number, group)
    }

    /// `E_` followed by `A-Z`, `0-9` and `_`. `#[command]` asserts it at compile time.
    pub const fn name_is_valid(self) -> bool {
        let b = self.name.as_bytes();
        if b.len() < 3 || b[0] != b'E' || b[1] != b'_' {
            return false;
        }
        let mut i = 2;
        while i < b.len() {
            if !(b[i].is_ascii_uppercase() || b[i].is_ascii_digit() || b[i] == b'_') {
                return false;
            }
            i += 1;
        }
        true
    }

    /// `None` only for a code no entry of the `error_codes!` table names.
    pub fn meaning(self) -> Option<&'static str> {
        MEANINGS
            .iter()
            .find(|(name, _)| *name == self.name)
            .map(|(_, meaning)| *meaning)
    }

    /// Surfaces that receive an envelope as JSON use it to recover the number.
    pub fn lookup(name: &str) -> Option<ErrorCode> {
        registered_error_codes()
            .into_iter()
            .find(|c| c.name == name)
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.number)
    }
}

/// Defines one `ErrorCode` constant per entry, plus `PRE_REGISTERED` and [`MEANINGS`]. Each entry
/// needs a sentence saying what the code means to a caller, which `docs/errors.md` renders.
macro_rules! error_codes {
    ($($(#[doc = $doc:literal])* $name:ident = $number:literal, $meaning:literal;)*) => {
        $(
            $(#[doc = $doc])*
            ///
            #[doc = $meaning]
            pub const $name: ErrorCode = ErrorCode { name: stringify!($name), number: $number };
        )*

        pub const PRE_REGISTERED: &[ErrorCode] = &[$($name),*];

        /// The "Meaning" column of `docs/errors.md`, in registration order.
        pub const MEANINGS: &[(&str, &str)] = &[$((stringify!($name), $meaning)),*];
    };
}

// Device planner codes are in the Device range; every other code is Core.
error_codes! {
    E_USAGE = 1,
        "The arguments are wrong: an unknown argument, a value outside its type or its enumerated \
         set, or a required one missing. Nothing ran; fix the call and repeat it.";
    E_STATE = 2,
        "The call is well formed but the instance cannot serve it in the state it is in, such as \
         no instance at all, one already stopped, or one whose boot has not reached the point the \
         command needs.";
    E_STALE_REF = 3,
        "A reference taken from an earlier revision no longer names anything, such as a `ui` `eN` \
         object ref after the tree moved on. Take a fresh revision and look it up again.";
    E_TIMEOUT = 4,
        "The condition the command was told to wait for did not hold within the virtual-time \
         budget it was given. Retryable: repeat it with a longer budget.";
    E_WALL_BUDGET = 5,
        "The call used more of the host's own wall-clock budget than it is allowed, whatever \
         virtual time it reached. Retryable, and it changes no result.";
    E_LEASE = 6,
        "Someone else holds the clock lease this command needs, or the caller does not hold it. \
         The message names the holder.";
    E_GUEST_PANIC = 7,
        "The guest firmware panicked. The envelope carries the reason, the task, the decoded \
         frames, `mcause` and `mtval`, and the tail of the console.";
    E_DEADLOCK = 8,
        "Every guest task is waiting and nothing left in the machine can wake one, so virtual \
         time cannot advance. The envelope carries the task table and the lock owners.";
    E_STUCK = 9,
        "The guest is spinning on a value nothing in this build will ever change. The envelope \
         names the block, register, program counter and symbol it is waiting on.";
    E_TRIPWIRE = 10,
        "The guest reached something the emulator refuses to run rather than fake, such as a \
         disabled radio feature or an access outside an allowlist. The message names the cause.";
    E_UNMODELED = 11,
        "The guest touched hardware this build does not model. `inspect --json '{\"what\": \
         [\"fidelity\"]}'` lists what was unmodeled in the run.";
    E_HLE_BINDING = 12,
        "High-level emulation could not bind to the image: the symbols it needs are absent, or \
         they do not match the profile's expectation. Raised when the binding is made, not later.";
    E_ASSET_MISSING = 13,
        "A file the run needs is not there: an unknown corpus id, a corpus entry whose file is \
         gone, an unreadable path, or no bundled ROM for the eFuse's chip revision.";
    E_ASSET_HASH = 14,
        "The bytes are there but are not the pinned ones: a corpus file that differs from the \
         SHA-256 `corpus.toml` records, or a ROM override outside `assets/rom/pins.toml`.";
    E_SNAPSHOT = 15,
        "A snapshot could not be saved, restored, exported or imported, most often because it \
         belongs to a different run identity (ROM, image, eFuse or config).";
    E_SECRET_REFUSED = 16,
        "The call would have returned bytes the secret policy does not release, such as a read \
         wholly inside masked memory or an export of secrets without an explicit human step.";
    E_PLAN_REFUSED = 5000,
        "The flash planner refused the plan before writing anything to a real device, and reset \
         the device back to its app. The message says which check refused it.";
    E_DEVICE_BUSY = 5001,
        "The real device the plan names is already held by another operation.";
    E_CARDID_CHANGED = 5002,
        "The device's cardid partition is not the one the plan was made against. It carries \
         instructions for a person, never an automated repair: this tool never writes cardid.";
    E_INTERNAL = 17,
        "The emulator itself failed, or the part of it this command needs has not landed in this \
         build. It says nothing about the firmware under test.";
    /// Distinct from `E_HLE_BINDING`, which reports symbol binding mismatches.
    E_HLE = 18,
        "A bound high-level-emulation handler failed the call it intercepted, or a nested guest \
         call did not have the stack headroom its guard requires. Binding itself was fine.";
    E_HOST_UNSUPPORTED = 19,
        "The command, or one option of it, does not exist on this host. The message names this \
         host and the one that supports it; the host columns of the command pages say so ahead \
         of time.";
    E_DAEMON = 20,
        "The shared daemon could not be reached or started: no port answered with the token, the \
         connection dropped mid-call, or the runtime directory is unusable. Retryable.";
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// (first seen, conflicting).
    DuplicateName(ErrorCode, ErrorCode),
    /// (first seen, conflicting).
    DuplicateNumber(ErrorCode, ErrorCode),
    OutOfRange(ErrorCode),
    OutsideGroup(ErrorCode, CapsGroup),
    InvalidName(ErrorCode),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::DuplicateName(a, b) => {
                write!(f, "error code name used twice: {a} and {b}")
            }
            RegistryError::DuplicateNumber(a, b) => {
                write!(f, "error code number used twice: {a} and {b}")
            }
            RegistryError::OutOfRange(c) => write!(f, "error code {c} lies in no caps group range"),
            RegistryError::OutsideGroup(c, g) => write!(
                f,
                "error code {c} is outside the Core range and the {} range {:?}",
                g.caps_name(),
                g.error_range()
            ),
            RegistryError::InvalidName(c) => {
                write!(
                    f,
                    "error code {c} is not named `E_` plus `A-Z`, `0-9` and `_`"
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// The run-time form of the checks `#[command]` makes at compile time, for lists built by hand.
pub fn check_group_codes(group: CapsGroup, codes: &[ErrorCode]) -> Result<(), RegistryError> {
    for &code in codes {
        if !code.name_is_valid() {
            return Err(RegistryError::InvalidName(code));
        }
        if code.group().is_none() {
            return Err(RegistryError::OutOfRange(code));
        }
        if !code.allowed_in(group) {
            return Err(RegistryError::OutsideGroup(code, group));
        }
    }
    Ok(())
}

/// The same code listed by several commands is accepted.
pub fn check_registry(codes: &[ErrorCode]) -> Result<(), RegistryError> {
    let mut by_name: BTreeMap<&'static str, ErrorCode> = BTreeMap::new();
    let mut by_number: BTreeMap<u16, ErrorCode> = BTreeMap::new();
    for &code in codes {
        if code.group().is_none() {
            return Err(RegistryError::OutOfRange(code));
        }
        match by_name.insert(code.name, code) {
            Some(prev) if prev != code => return Err(RegistryError::DuplicateName(prev, code)),
            _ => {}
        }
        match by_number.insert(code.number, code) {
            Some(prev) if prev != code => return Err(RegistryError::DuplicateNumber(prev, code)),
            _ => {}
        }
    }
    Ok(())
}

/// `PRE_REGISTERED`, then the `errors` of every registered command in registry order.
pub fn registered_error_codes() -> Vec<ErrorCode> {
    PRE_REGISTERED
        .iter()
        .copied()
        .chain(commands().iter().flat_map(|c| c.errors.iter().copied()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_reads_back_into_the_error_it_was_written_from() {
        let frame = Frame {
            pc: 0x4200_1234,
            sp: 0x3fc8_0000,
            function: Some("app_main".into()),
            source: Some("/build/main/app.c".into()),
            file: Some("main/app.c".into()),
            line: Some(12),
            column: Some(3),
            inlined: true,
            origin: pemu_introspect::unwind::FrameOrigin::App,
        };
        let error = ApiError::new(E_TIMEOUT, "no match")
            .retryable()
            .with_hint("call `serial`")
            .with_detail(serde_json::json!({ "until": "x" }))
            .with_serial_tail(vec!["line".into()])
            .with_backtrace(vec![frame])
            .at_vt_us(77);
        let back = ApiError::from_json(&error.to_json()).expect("an envelope");
        assert_eq!(back.to_json(), error.to_json());
        assert!(
            ApiError::from_json(&serde_json::json!({ "code": "E_NOT_A_CODE", "message": "x" }))
                .is_none()
        );
        assert!(ApiError::from_json(&serde_json::json!("E_TIMEOUT")).is_none());
    }

    #[test]
    fn registered_set_has_no_duplicate_names_or_numbers() {
        assert_eq!(check_registry(&registered_error_codes()), Ok(()));
    }

    /// The macro forces a sentence to exist; this pins that it is renderable prose, since
    /// `docs/errors.md` prints it in a table cell.
    #[test]
    fn every_pre_registered_code_has_one_finished_sentence() {
        assert_eq!(MEANINGS.len(), PRE_REGISTERED.len());
        for (code, (name, meaning)) in PRE_REGISTERED.iter().zip(MEANINGS) {
            assert_eq!(code.name, *name);
            assert_eq!(code.meaning(), Some(*meaning));
            assert!(
                meaning.ends_with('.') && meaning.len() > 40,
                "the meaning of {name} is not a finished sentence"
            );
        }
    }

    #[test]
    fn pre_registered_holds_the_arch_codes() {
        assert_eq!(PRE_REGISTERED.len(), 23);
        assert!(PRE_REGISTERED.iter().all(|c| c.name.starts_with("E_")));
        assert_eq!(E_USAGE.name, "E_USAGE");
        assert_eq!(E_HLE.group(), Some(CapsGroup::Core));
        assert_ne!(E_HLE, E_HLE_BINDING);
        assert!(registered_error_codes().starts_with(PRE_REGISTERED));
    }

    #[test]
    fn rejects_a_duplicate_name() {
        let a = ErrorCode {
            name: "E_RADIO_X",
            number: 2000,
        };
        let b = ErrorCode {
            name: "E_RADIO_X",
            number: 2001,
        };
        assert_eq!(
            check_registry(&[a, b]),
            Err(RegistryError::DuplicateName(a, b))
        );
    }

    #[test]
    fn rejects_a_duplicate_number() {
        let other = ErrorCode {
            name: "E_OTHER",
            number: E_USAGE.number,
        };
        assert_eq!(
            check_registry(&[E_USAGE, other]),
            Err(RegistryError::DuplicateNumber(E_USAGE, other))
        );
    }

    #[test]
    fn accepts_the_same_code_listed_twice() {
        assert_eq!(check_registry(&[E_USAGE, E_STATE, E_USAGE]), Ok(()));
    }

    #[test]
    fn rejects_numbers_outside_every_range() {
        for number in [0, 7000, u16::MAX] {
            let code = ErrorCode {
                name: "E_NOWHERE",
                number,
            };
            assert_eq!(
                check_registry(&[code]),
                Err(RegistryError::OutOfRange(code))
            );
        }
    }

    #[test]
    fn group_ranges_are_ordered_disjoint_and_skip_zero() {
        assert_eq!(*CapsGroup::Core.error_range().start(), 1);
        for pair in CapsGroup::ALL.windows(2) {
            assert!(pair[0].error_range().end() < pair[1].error_range().start());
        }
    }

    #[test]
    fn codes_fall_in_their_groups() {
        assert_eq!(E_USAGE.group(), Some(CapsGroup::Core));
        assert_eq!(E_INTERNAL.group(), Some(CapsGroup::Core));
        assert_eq!(E_PLAN_REFUSED.group(), Some(CapsGroup::Device));
        assert_eq!(E_CARDID_CHANGED.group(), Some(CapsGroup::Device));
        assert_eq!(
            ErrorCode {
                name: "E_NFC_X",
                number: 3000
            }
            .group(),
            Some(CapsGroup::Nfc)
        );
    }
    /// Changing a key name or dropping a field breaks every generated client.
    #[test]
    fn envelope_json_golden() {
        let error = ApiError::new(E_TIMEOUT, "no match within 10 s of virtual time")
            .at_vt_us(845_213)
            .retryable()
            .with_hint("call inspect {what:[\"tasks\"]} to see who holds the LVGL mutex")
            .with_detail(serde_json::json!({ "until": "serial:/pk_app: ready/" }))
            .with_serial_tail(vec!["I (812) pk_app: boot".to_string()]);
        assert_eq!(
            error.to_json_text(),
            concat!(
                r#"{"backtrace":[],"code":"E_TIMEOUT","detail":{"until":"serial:/pk_app: ready/"},"#,
                r#""hint":"call inspect {what:[\"tasks\"]} to see who holds the LVGL mutex","#,
                r#""message":"no match within 10 s of virtual time","number":4,"retryable":true,"#,
                r#""serial_tail":["I (812) pk_app: boot"],"vt_us":845213}"#
            )
        );
        assert_eq!(error.status(), 4);
    }

    /// Agents read "no hint" from the key's absence.
    #[test]
    fn envelope_json_golden_without_a_hint() {
        let error = ApiError::new(E_INTERNAL, "unreachable");
        assert_eq!(
            error.to_json_text(),
            concat!(
                r#"{"backtrace":[],"code":"E_INTERNAL","detail":null,"message":"unreachable","#,
                r#""number":17,"retryable":false,"serial_tail":[],"vt_us":0}"#
            )
        );
        assert!(error.hint.is_none());
        assert_eq!(error.to_string(), "E_INTERNAL (17): unreachable");
    }

    #[test]
    fn envelope_schema_describes_the_envelope() {
        let schema = ApiError::schema();
        let object = schema.as_object().expect("the schema is an object");
        let required: Vec<&str> = object["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|v| v.as_str().expect("a key name"))
            .collect();
        let envelope = ApiError::new(E_USAGE, "bad arguments").to_json();
        let envelope = envelope.as_object().expect("the envelope is an object");
        for key in &required {
            assert!(envelope.contains_key(*key), "the envelope lacks {key}");
        }
        let properties = object["properties"].as_object().expect("properties");
        for key in envelope.keys() {
            assert!(properties.contains_key(key), "the schema lacks {key}");
        }
        assert_eq!(properties["number"]["maximum"], MAX_ERROR_NUMBER);
        assert_eq!(
            MAX_ERROR_NUMBER,
            CapsGroup::ALL
                .into_iter()
                .map(|g| *g.error_range().end())
                .max()
                .expect("at least one group"),
            "the ceiling is the end of the last reserved range"
        );
    }

    /// Retryability is per error, not per code.
    #[test]
    fn builders_set_only_what_they_name() {
        let plain = ApiError::new(E_USAGE, "bad arguments");
        assert!(!plain.retryable && plain.hint.is_none() && plain.vt_us == 0);
        assert!(plain.serial_tail.is_empty() && plain.backtrace.is_empty());
        assert_eq!(*plain.detail, serde_json::Value::Null);
        let retryable = ApiError::new(E_TIMEOUT, "timed out").retryable();
        assert!(retryable.retryable);
    }
    /// An error carrying 0 would reach JS as a successful result holding an error payload.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "number 0 is the wasm STATUS_OK")]
    fn a_code_numbered_zero_is_rejected_in_debug_builds() {
        let zero = ErrorCode {
            name: "E_ZERO",
            number: 0,
        };
        let _ = ApiError::new(zero, "impossible");
    }

    #[test]
    fn api_error_stays_under_the_large_err_threshold() {
        assert!(
            size_of::<ApiError>() < ApiError::SIZE_LIMIT,
            "ApiError is {} bytes, the lint allows under {}",
            size_of::<ApiError>(),
            ApiError::SIZE_LIMIT
        );
    }
}
