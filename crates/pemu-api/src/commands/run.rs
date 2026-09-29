//! `passportsim run`: advance virtual time for a fixed duration, or until a matcher fires.
//!
//! The run goes in fixed virtual-time slices and, after each, feeds what the machine released
//! (serial lines, event-ring entries, the deadline) to the compiled matcher in release order. The
//! match instant is exact; the stop instant is the end of that slice, a pure function of the budget
//! ([`slice_of`]), so identical runs stop at the same instant.
//!
//! `ui:changed` settles to the next LVGL safe point after it fires. `var:` reads the global at its
//! DWARF type and arms a `PF_SLOW` write watch on its page for this run only, so a store ends the
//! slice at the writing instruction. Other guest-state matchers are `E_UNMODELED`, never silently
//! unfired.
//!
//! `timeout` is virtual; `wall_budget_ms` is host time, checked once per slice when the process
//! installed a clock ([`WallBudget`]). Running out is a retryable `E_WALL_BUDGET` with the instance
//! paused where it got to.

use std::fmt::Write as _;

use pemu_core::hostio::{EventKind as RingEvent, HostEvent, HostIo, SerialStream};
use pemu_core::time::VTime;
use pemu_introspect::freertos::DeadlockReport;
use pemu_introspect::vars::{VarQuery, VarValue};
use pemu_machine::stops::{StopReason, StopSet, Watch};

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TIMEOUT,
    E_TRIPWIRE, E_UNMODELED, E_USAGE, E_WALL_BUDGET,
};
use crate::matchers::{
    Channel, CmpOp, ConsoleMode, EventKind, From as MatchFrom, LogLevel, Matcher, TextPattern,
    UiMatcher, Value, ValueTest,
};
use crate::output::Output;
use crate::registry::command;
use crate::shape::{ShapeLimits, shape_serial};
use crate::spec::{HandlerCx, Schema};

use crate::args::{
    duration_schema, enum_of, instance_schema, object, only, opt_duration, opt_str, opt_u64, usage,
};
use crate::pool::HostClock;
use crate::session::{Session, fault_of};

/// A shorter budget gets a finer stop instant; a longer one costs the same number of machine calls.
const RUN_SLICES: u64 = 1_000;

/// So a very short budget still makes exactly one call.
const MIN_SLICE: VTime = VTime(1_000_000_000);

pub fn slice_of(budget: VTime) -> VTime {
    VTime((budget.0 / RUN_SLICES).max(MIN_SLICE.0))
}

/// The longest slice while a live bridge is attached: packets move at slice boundaries, and idle
/// skip may lead wall time by at most 10 ms.
pub const BRIDGED_SLICE: VTime = VTime(5_000_000_000);

fn next_slice(session: &mut Session, slice: VTime, first: &mut bool) -> VTime {
    let bridged = super::net_http::live_bridge_tick(session, *first);
    *first = false;
    match bridged {
        true => VTime(slice.0.min(BRIDGED_SLICE.0)),
        false => slice,
    }
}

pub const DEFAULT_TIMEOUT: VTime = VTime(10_000_000_000_000);

pub const DEFAULT_WALL_BUDGET_MS: u64 = 30_000;

pub const SERIAL_TAIL_LINES: usize = 20;

/// One thing the machine released, in release order: the only input a wait is evaluated on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Obs {
    /// Without its newline.
    Line {
        stream: SerialStream,
        cursor: u64,
        vt: VTime,
        /// With a trailing `\r` removed.
        text: String,
    },
    Event {
        seq: u64,
        event: HostEvent,
    },
    /// Read at a slice boundary or right after the store that ended the slice.
    Var {
        /// As the caller wrote it, which is what the leaf matches on.
        query: String,
        value: VarValue,
        vt: VTime,
    },
    /// Virtual time reached with nothing else to report.
    Tick(VTime),
}

impl Obs {
    pub fn vt(&self) -> VTime {
        match self {
            Obs::Line { vt, .. } => *vt,
            Obs::Event { event, .. } => event.vt,
            Obs::Var { vt, .. } | Obs::Tick(vt) => *vt,
        }
    }
}

/// The `match` object of a `run` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub source: &'static str,
    pub text: String,
    /// For a serial or log hit.
    pub cursor: Option<u64>,
    pub vt: VTime,
}

impl Hit {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "source": self.source,
            "text": self.text,
            "cursor": self.cursor,
            "vt_us": self.vt.as_us(),
        })
    }
}

/// A matcher reduced to what this build can evaluate, with the per-leaf state `all(..)` and
/// `seq(..)` need across slices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    node: Node,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Line {
        channel: Channel,
        from: MatchFrom,
        tag: Option<Box<str>>,
        level: Option<LogLevel>,
        pattern: TextPattern,
        source: &'static str,
        fired: bool,
    },
    Event {
        want: RingEvent,
        name: &'static str,
        fired: bool,
    },
    Time {
        at: VTime,
        relative: bool,
        fired: bool,
    },
    /// The settle after a match is [`Wait::settles_ui`]'s, not this leaf's.
    Ui { fired: bool },
    Var {
        /// Parsed at compile time so a malformed name is refused before a run.
        query: VarQuery,
        /// How an [`Obs::Var`] names its value.
        rendered: String,
        test: ValueTest,
        /// What `changed` compares against.
        last: Option<VarValue>,
        /// A first read is not a change, so `changed` never fires on it.
        seen: bool,
        fired: bool,
    },
    /// The first child to fire wins.
    Any(Vec<Node>),
    /// Every child, in any order.
    All(Vec<Node>),
    /// Every child, in this order.
    Seq { children: Vec<Node>, next: usize },
}

impl Wait {
    /// Refuses what this build cannot evaluate.
    pub fn compile(matcher: &Matcher) -> Result<Wait, ApiError> {
        Ok(Wait {
            node: Node::compile(matcher)?,
        })
    }

    pub fn parse(text: &str) -> Result<Wait, ApiError> {
        Wait::compile(&Matcher::parse(text)?)
    }

    pub fn serial_contains(text: &str) -> Wait {
        Wait {
            node: Node::Line {
                channel: Channel::Any,
                from: MatchFrom::Cursor,
                tag: None,
                level: None,
                pattern: TextPattern::literal_contains(text),
                source: "serial",
                fired: false,
            },
        }
    }

    pub fn ui_settled() -> Wait {
        Wait {
            node: Node::Event {
                want: RingEvent::UiSettled,
                name: "ui_settled",
                fired: false,
            },
        }
    }

    /// The widest of its leaves.
    pub fn from(&self) -> MatchFrom {
        self.node.read_from().unwrap_or(MatchFrom::Cursor)
    }

    /// Resolves every `vt:+<dur>` deadline against `started`, the instant the wait is armed.
    /// Idempotent, because an armed leaf stops being relative.
    pub fn arm(&mut self, started: VTime) {
        self.node.arm(started);
    }

    pub fn observe(&mut self, obs: &Obs) -> Option<Hit> {
        self.node.observe(obs)
    }

    /// Without repeats, so a composite over one global reads it once per slice.
    #[must_use]
    pub fn watched_vars(&self) -> Vec<VarQuery> {
        let mut out = Vec::new();
        self.node.collect_vars(&mut out);
        out
    }

    /// A composite with one `ui:` leaf settles too: a following `ui` call must read a consistent
    /// tree whichever leaf ended the run.
    #[must_use]
    pub fn settles_ui(&self) -> bool {
        self.node.has_ui()
    }
}

/// An integer global orders on the integer; a `bool` reads as 0 or 1, so `== 1` and `== true` both
/// work; a `char[]` compares only with `==` and `!=`. Anything with no ordering does not fire,
/// because there is no answer rather than a false one.
fn compare(value: &VarValue, op: CmpOp, want: &Value) -> bool {
    let ordering = match want {
        Value::Int(w) => value.as_int().map(|v| v.cmp(w)),
        Value::Bool(w) => value
            .as_bool()
            .map(|v| i64::from(v).cmp(&i64::from(*w)))
            .filter(|_| matches!(op, CmpOp::Eq | CmpOp::Ne)),
        Value::Text(w) => value
            .as_text()
            .map(|v| v.cmp(w.as_ref()))
            .filter(|_| matches!(op, CmpOp::Eq | CmpOp::Ne)),
    };
    let Some(ordering) = ordering else {
        return false;
    };
    match op {
        CmpOp::Eq => ordering.is_eq(),
        CmpOp::Ne => ordering.is_ne(),
        CmpOp::Le => ordering.is_le(),
        CmpOp::Ge => ordering.is_ge(),
        CmpOp::Lt => ordering.is_lt(),
        CmpOp::Gt => ordering.is_gt(),
    }
}

impl Node {
    fn compile(matcher: &Matcher) -> Result<Node, ApiError> {
        Ok(match matcher {
            Matcher::Serial(m) => {
                if m.mode == ConsoleMode::Stream {
                    return Err(unsupported(
                        "serial:...,stream",
                        "whole-line matching only; a stream matcher needs a byte-level stop \
                         trigger, which the machine's stop set does not have",
                    ));
                }
                Node::Line {
                    channel: m.channel,
                    from: m.from,
                    tag: None,
                    level: None,
                    pattern: m.pattern.clone(),
                    source: "serial",
                    fired: false,
                }
            }
            Matcher::Log(m) => Node::Line {
                channel: Channel::Any,
                from: MatchFrom::Cursor,
                tag: m.tag.clone(),
                level: m.level,
                pattern: m.pattern.clone(),
                source: "log",
                fired: false,
            },
            Matcher::Event(kind) => {
                let (want, name) = ring_event(*kind)?;
                Node::Event {
                    want,
                    name,
                    fired: false,
                }
            }
            Matcher::Time(m) => Node::Time {
                at: m.at,
                relative: m.relative,
                fired: false,
            },
            Matcher::Any(children) => Node::Any(compile_all(children)?),
            Matcher::All(children) => Node::All(compile_all(children)?),
            Matcher::Seq(children) => Node::Seq {
                children: compile_all(children)?,
                next: 0,
            },
            Matcher::Ui(UiMatcher::Changed) => Node::Ui { fired: false },
            Matcher::Ui(UiMatcher::Query { .. }) => {
                // `role` and `selected` need the ui-hint vocabulary, which `ui.expect` refuses for
                // the same reason.
                return Err(unsupported(
                    "ui:<attr>=",
                    "`ui:changed` is evaluated; an attribute query needs the ui-hint vocabulary, \
                     which `ui.expect` refuses for the same reason",
                ));
            }
            Matcher::Symbol(_) => {
                return Err(unsupported("symbol:", "this build has no observe hooks"));
            }
            Matcher::Var(m) => {
                let query = VarQuery::parse(&m.name).map_err(|e| {
                    ApiError::new(E_USAGE, format!("`var:{}` {e}", m.name)).with_hint(
                        "a global is a name, optionally with a compilation unit \
                         (`main.c::s_sel`) or an index (`s_ok[2]`)",
                    )
                })?;
                if let ValueTest::Cmp(op, Value::Text(_)) = &m.test
                    && !matches!(op, CmpOp::Eq | CmpOp::Ne)
                {
                    return Err(ApiError::new(
                        E_USAGE,
                        format!(
                            "`var:{}`: a string value compares with `==` or `!=` only",
                            m.name
                        ),
                    ));
                }
                Node::Var {
                    rendered: query.render(),
                    query,
                    test: m.test.clone(),
                    last: None,
                    seen: false,
                    fired: false,
                }
            }
            Matcher::Addr(_) => {
                return Err(unsupported(
                    "addr:",
                    "a raw data address has no matcher yet (`crate::matchers::AddrMatcher`); \
                     `var:` over a named global is evaluated",
                ));
            }
            Matcher::Reg(_) => {
                return Err(unsupported("reg:", "this build has no MMIO write trigger"));
            }
        })
    }

    /// See [`Wait::arm`].
    fn arm(&mut self, started: VTime) {
        match self {
            Node::Time { at, relative, .. } => {
                if *relative {
                    *at = VTime(started.0.saturating_add(at.0));
                    *relative = false;
                }
            }
            Node::Line { .. } | Node::Event { .. } | Node::Ui { .. } | Node::Var { .. } => {}
            Node::Any(children) | Node::All(children) | Node::Seq { children, .. } => {
                for child in children {
                    child.arm(started);
                }
            }
        }
    }

    /// The earliest of the children, so one composite reads one range.
    fn read_from(&self) -> Option<MatchFrom> {
        match self {
            Node::Line { from, .. } => Some(*from),
            Node::Event { .. } | Node::Time { .. } | Node::Ui { .. } | Node::Var { .. } => None,
            Node::Any(children) | Node::All(children) | Node::Seq { children, .. } => children
                .iter()
                .filter_map(Node::read_from)
                .min_by_key(|from| from_rank(*from)),
        }
    }

    fn observe(&mut self, obs: &Obs) -> Option<Hit> {
        match self {
            Node::Line {
                channel,
                tag,
                level,
                pattern,
                source,
                fired,
                ..
            } => {
                if *fired {
                    return None;
                }
                let Obs::Line {
                    stream,
                    cursor,
                    vt,
                    text,
                } = obs
                else {
                    return None;
                };
                if !channel_has(*channel, *stream) {
                    return None;
                }
                let subject = match (tag.as_deref(), *level) {
                    (None, None) => text.as_str(),
                    (tag, level) => log_body(text, tag, level)?,
                };
                if !pattern.matches(subject) {
                    return None;
                }
                *fired = true;
                Some(Hit {
                    source,
                    text: text.clone(),
                    cursor: Some(*cursor),
                    vt: *vt,
                })
            }
            Node::Event { want, name, fired } => {
                if *fired {
                    return None;
                }
                let Obs::Event { event, .. } = obs else {
                    return None;
                };
                if event.kind != *want {
                    return None;
                }
                *fired = true;
                Some(Hit {
                    source: "event",
                    text: (*name).to_owned(),
                    cursor: None,
                    vt: event.vt,
                })
            }
            Node::Time { at, fired, .. } => {
                if *fired || obs.vt().0 < at.0 {
                    return None;
                }
                *fired = true;
                Some(Hit {
                    source: "vt",
                    text: format!("{}us", at.as_us()),
                    cursor: None,
                    vt: obs.vt(),
                })
            }
            Node::Ui { fired } => {
                if *fired {
                    return None;
                }
                // A display generation change arrives as `RingEvent::Frame`; the safe-point half is
                // the settle after the match.
                let Obs::Event { event, .. } = obs else {
                    return None;
                };
                if event.kind != RingEvent::Frame {
                    return None;
                }
                *fired = true;
                Some(Hit {
                    source: "ui",
                    text: "changed".to_owned(),
                    cursor: None,
                    vt: event.vt,
                })
            }
            Node::Var {
                rendered,
                test,
                last,
                seen,
                fired,
                ..
            } => {
                if *fired {
                    return None;
                }
                let Obs::Var { query, value, vt } = obs else {
                    return None;
                };
                if query != rendered {
                    return None;
                }
                let hit = match test {
                    // A first read is not a change.
                    ValueTest::Changed => *seen && last.as_ref() != Some(value),
                    ValueTest::Cmp(op, want) => compare(value, *op, want),
                };
                *last = Some(value.clone());
                *seen = true;
                if !hit {
                    return None;
                }
                *fired = true;
                Some(Hit {
                    source: "var",
                    text: format!("{rendered} = {}", value.render()),
                    cursor: None,
                    vt: *vt,
                })
            }
            Node::Any(children) => children.iter_mut().find_map(|child| child.observe(obs)),
            Node::All(children) => {
                let mut hit = None;
                for child in children.iter_mut() {
                    if let Some(h) = child.observe(obs) {
                        hit = Some(h);
                    }
                }
                let all = children.iter().all(Node::has_fired);
                if all {
                    hit.or_else(|| Some(last_hit(obs)))
                } else {
                    None
                }
            }
            Node::Seq { children, next } => {
                let index = *next;
                let child = children.get_mut(index)?;
                let hit = child.observe(obs)?;
                *next += 1;
                if *next == children.len() {
                    Some(hit)
                } else {
                    None
                }
            }
        }
    }

    fn collect_vars(&self, out: &mut Vec<VarQuery>) {
        match self {
            Node::Var { query, .. } => {
                if !out.contains(query) {
                    out.push(query.clone());
                }
            }
            Node::Any(children) | Node::All(children) | Node::Seq { children, .. } => {
                for child in children {
                    child.collect_vars(out);
                }
            }
            Node::Line { .. } | Node::Event { .. } | Node::Time { .. } | Node::Ui { .. } => {}
        }
    }

    fn has_ui(&self) -> bool {
        match self {
            Node::Ui { .. } => true,
            Node::Any(children) | Node::All(children) | Node::Seq { children, .. } => {
                children.iter().any(Node::has_ui)
            }
            Node::Line { .. } | Node::Event { .. } | Node::Time { .. } | Node::Var { .. } => false,
        }
    }

    fn has_fired(&self) -> bool {
        match self {
            Node::Line { fired, .. }
            | Node::Event { fired, .. }
            | Node::Time { fired, .. }
            | Node::Ui { fired }
            | Node::Var { fired, .. } => *fired,
            Node::Any(children) => children.iter().any(Node::has_fired),
            Node::All(children) => children.iter().all(Node::has_fired),
            Node::Seq { children, next } => *next == children.len(),
        }
    }
}

/// When the last child of `all(..)` fired on an observation it did not consume.
fn last_hit(obs: &Obs) -> Hit {
    Hit {
        source: "all",
        text: "every child fired".to_owned(),
        cursor: None,
        vt: obs.vt(),
    }
}

/// Widest first.
fn from_rank(from: MatchFrom) -> u8 {
    match from {
        MatchFrom::Start => 0,
        MatchFrom::Cursor => 1,
        MatchFrom::Now => 2,
    }
}

fn compile_all(children: &[Matcher]) -> Result<Vec<Node>, ApiError> {
    children.iter().map(Node::compile).collect()
}

fn unsupported(kind: &str, why: &str) -> ApiError {
    ApiError::new(
        E_UNMODELED,
        format!("`{kind}` matchers are not evaluated by this build: {why}"),
    )
    .with_hint("`serial:`, `log:`, `event:` and `vt:` matchers work today")
}

fn channel_has(channel: Channel, stream: SerialStream) -> bool {
    match channel {
        Channel::Any => true,
        Channel::Usj => stream == SerialStream::UsjTx,
        Channel::Uart0 => stream == SerialStream::Uart0Tx,
    }
}

/// An IDF line is `<L> (<ms>) <tag>: <message>`. A line not in that shape never matches `log:`,
/// which is what makes it differ from `serial:`.
fn log_body<'a>(line: &'a str, tag: Option<&str>, level: Option<LogLevel>) -> Option<&'a str> {
    let letter = line.chars().next()?;
    if !matches!(letter, 'E' | 'W' | 'I' | 'D' | 'V') {
        return None;
    }
    if let Some(level) = level
        && level.as_str() != &line[..letter.len_utf8()]
    {
        return None;
    }
    let rest = line.get(letter.len_utf8()..)?.strip_prefix(" (")?;
    let (_timestamp, rest) = rest.split_once(") ")?;
    let (line_tag, message) = rest.split_once(": ")?;
    if let Some(tag) = tag
        && tag != line_tag
    {
        return None;
    }
    Some(message)
}

/// The ring has seven kinds; the rest of the vocabulary is refused by name rather than never
/// firing. UNVERIFIED: no package documents the `Power` argument yet, so `power_off` matches any
/// `Power` entry.
fn ring_event(kind: EventKind) -> Result<(RingEvent, &'static str), ApiError> {
    let ring = match kind {
        EventKind::Reset => RingEvent::Reset,
        EventKind::Panic => RingEvent::Panic,
        EventKind::Sleep => RingEvent::Sleep,
        EventKind::PowerOff => RingEvent::Power,
        EventKind::Frame | EventKind::UiChanged => RingEvent::Frame,
        other => {
            return Err(unsupported(
                &format!("event:{}", other.as_str()),
                "the event ring has no entry of that kind yet",
            ));
        }
    };
    Ok((ring, event_name(ring)))
}

/// Also how `status` reports the last event.
pub(crate) fn event_name(kind: RingEvent) -> &'static str {
    match kind {
        RingEvent::Reset => "reset",
        RingEvent::Panic => "panic",
        RingEvent::Sleep => "sleep",
        RingEvent::Power => "power_off",
        RingEvent::Frame => "frame",
        RingEvent::UiSettled => "ui_settled",
        RingEvent::FidelityWarning => "fidelity_warning",
    }
}

pub fn read_lines(io: &HostIo, stream: SerialStream, cursor: u64) -> (Vec<Obs>, u64) {
    let view = io.serial_ring(stream).slices(cursor);
    let start = view.start;
    let bytes: Vec<u8> = view.iter().copied().collect();
    let marks = io.lines.slices(stream, io.lines.tail(stream));
    let mut out = Vec::new();
    let mut next = start;
    for mark in marks.iter() {
        if mark.stream != stream || mark.offset < next {
            continue;
        }
        let (Ok(from), Ok(to)) = (
            usize::try_from(next - start),
            usize::try_from(mark.offset - start),
        ) else {
            break;
        };
        if to > bytes.len() || from > to {
            break;
        }
        out.push(Obs::Line {
            stream,
            cursor: mark.offset + 1,
            vt: mark.vt,
            text: String::from_utf8_lossy(&bytes[from..to])
                .trim_end_matches('\r')
                .to_owned(),
        });
        next = mark.offset + 1;
    }
    (out, next)
}

pub fn read_events(io: &HostIo, cursor: u64) -> (Vec<Obs>, u64) {
    let view = io.events.slices(cursor);
    let mut seq = view.start;
    let mut out = Vec::new();
    for event in view.iter() {
        out.push(Obs::Event { seq, event: *event });
        seq += 1;
    }
    (out, view.next)
}

fn start_cursor(session: &mut Session, stream: SerialStream, from: MatchFrom) -> u64 {
    match from {
        MatchFrom::Start => 0,
        MatchFrom::Cursor => session.cursor(stream).0,
        MatchFrom::Now => session.machine().io().serial_ring(stream).head(),
    }
}

pub type SliceObserver = fn(&mut Session);

static SLICE_OBSERVER: std::sync::RwLock<Option<SliceObserver>> = std::sync::RwLock::new(None);

/// A host that streams output (the daemon's WebSocket) looks between the slices of a long `run`,
/// not only after the command. Called on the thread that ran the slice by every command that
/// advances time. It must only read and must not take the process pool lock, which some callers
/// hold.
pub fn set_slice_observer(observer: Option<SliceObserver>) {
    *SLICE_OBSERVER.write().unwrap_or_else(|e| e.into_inner()) = observer;
}

pub(crate) fn observe_slice(session: &mut Session) {
    let observer = *SLICE_OBSERVER.read().unwrap_or_else(|e| e.into_inner());
    if let Some(observer) = observer {
        observer(session);
    }
}

/// Exceeding it stops the run where it got to with the retryable `E_WALL_BUDGET`; a run that
/// finishes is unaffected, so determinism holds.
#[derive(Copy, Clone, Debug)]
pub struct WallBudget {
    clock: Option<HostClock>,
    started_ms: u64,
    budget_ms: u64,
}

impl WallBudget {
    pub fn start(session: &Session, budget_ms: u64) -> WallBudget {
        let clock = session.host_clock;
        WallBudget {
            clock,
            started_ms: clock.map_or(0, |now| now()),
            budget_ms,
        }
    }

    /// For a caller with no host bound (the boot of `start`).
    pub fn unlimited() -> WallBudget {
        WallBudget {
            clock: None,
            started_ms: 0,
            budget_ms: 0,
        }
    }

    pub fn spent(&self) -> bool {
        match self.clock {
            None => false,
            Some(now) => now().saturating_sub(self.started_ms) >= self.budget_ms,
        }
    }

    /// The host's cancel first, then this budget.
    pub fn check(&self, session: &Session) -> Result<(), ApiError> {
        let at = session.now().as_us();
        if session.cancelled() {
            return Err(crate::pool::cancelled(session.id).at_vt_us(at));
        }
        if self.spent() {
            return Err(self.exceeded().at_vt_us(at));
        }
        Ok(())
    }

    pub fn exceeded(&self) -> ApiError {
        wall_budget_exceeded(self.budget_ms)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    Matched(Hit),
    Failed(Hit),
    Deadline,
    /// The guest waits for input, so running again would return at once. Reported as `E_DEADLOCK`
    /// after the loop observed what the guest printed.
    Waiting,
    /// A cycle of tasks each waiting forever for a mutex the next holds. Reported as `E_DEADLOCK`
    /// at the slice it was found in.
    Deadlocked(Box<DeadlockReport>),
}

/// A global the machine cannot watch (a `const` in flash) is left out and still re-read every
/// slice. `None` means no walker or debug information for this firmware, which the caller then
/// stops reading for the run: a host does not remember an ELF miss, and a per-slice retry would
/// re-read the corpus manifest a thousand times a second.
fn var_watch(session: &mut Session, watched: &[VarQuery]) -> Option<StopSet> {
    let mut stops = StopSet::default();
    if watched.is_empty() {
        return Some(stops);
    }
    let fw = session.fw.clone();
    let walkers = crate::commands::inspect::introspectors();
    let snapshot = crate::commands::inspect::with_walk_firmware(&fw, || {
        (walkers.vars)(session.machine(), watched)
    })
    .ok()?;
    for read in &snapshot.reads {
        if let Some(r) = read.reading() {
            let watch = Watch {
                addr: r.addr,
                len: r.len.max(1),
            };
            if watch.watchable() {
                stops.watches.push(watch);
            }
        }
    }
    Some(stops)
}

/// An unreadable name yields no observation rather than an error: an uninitialized global may be
/// exactly what the caller is waiting for. It cannot fire, so the wait ends in `E_TIMEOUT`.
fn read_vars(session: &mut Session, watched: &[VarQuery], now: VTime) -> Vec<Obs> {
    if watched.is_empty() {
        return Vec::new();
    }
    let fw = session.fw.clone();
    let walkers = crate::commands::inspect::introspectors();
    let Ok(snapshot) = crate::commands::inspect::with_walk_firmware(&fw, || {
        (walkers.vars)(session.machine(), watched)
    }) else {
        return Vec::new();
    };
    snapshot
        .reads
        .iter()
        .filter_map(|read| {
            let r = read.reading()?;
            Some(Obs::Var {
                query: r.query.clone(),
                value: r.value.clone(),
                vt: now,
            })
        })
        .collect()
}

/// `wait` is `None` for a run that only has to reach its deadline while `fail_if` watches. A guest
/// fault or a spent host budget ends the loop with `Err`.
pub fn poll(
    session: &mut Session,
    wait: Option<&Wait>,
    fail_if: &[Wait],
    deadline: VTime,
    budget: &WallBudget,
) -> Result<Stop, ApiError> {
    let started = session.now();
    let mut wait = wait.cloned();
    let mut fail_if: Vec<Wait> = fail_if.to_vec();
    for one in wait.iter_mut().chain(fail_if.iter_mut()) {
        one.arm(started);
    }
    let from = wait
        .iter()
        .chain(fail_if.iter())
        .map(Wait::from)
        .min_by_key(|from| from_rank(*from))
        .unwrap_or(MatchFrom::Cursor);
    let mut cursors = [0u64; SerialStream::ALL.len()];
    for stream in SerialStream::ALL {
        cursors[stream.index()] = start_cursor(session, stream, from);
    }
    let mut events = session.event_cursor();
    let slice = slice_of(VTime(deadline.0.saturating_sub(started.0)));
    // The watch only changes when the slice ends, so a global the machine cannot watch still
    // matches, one slice late.
    let watched: Vec<VarQuery> = wait
        .iter()
        .chain(fail_if.iter())
        .flat_map(|w| w.watched_vars())
        .fold(Vec::new(), |mut acc, q| {
            if !acc.contains(&q) {
                acc.push(q);
            }
            acc
        });
    // See [`var_watch`].
    let (stops, watched) = match var_watch(session, &watched) {
        Some(stops) => (stops, watched),
        None => (StopSet::default(), Vec::new()),
    };
    let mut waiting = false;
    let mut first = true;
    loop {
        let mut obs = Vec::new();
        for stream in SerialStream::ALL {
            let (lines, next) = read_lines(session.machine().io(), stream, cursors[stream.index()]);
            cursors[stream.index()] = next;
            obs.extend(lines);
        }
        let (ring, next) = read_events(session.machine().io(), events);
        events = next;
        obs.extend(ring);
        obs.sort_by_key(Obs::vt);
        let now = session.now();
        // After the slice's lines and events, so composite leaves see one order on every host.
        obs.extend(read_vars(session, &watched, now));
        obs.push(Obs::Tick(now));
        session.set_event_cursor(events);
        for one in &obs {
            // `fail_if` first, so an observation satisfying both ends the run as a failure.
            for fail in &mut fail_if {
                if let Some(hit) = fail.observe(one) {
                    return Ok(Stop::Failed(hit));
                }
            }
            if let Some(hit) = wait.as_mut().and_then(|wait| wait.observe(one)) {
                return Ok(Stop::Matched(hit));
            }
        }
        if now.0 >= deadline.0 {
            return Ok(Stop::Deadline);
        }
        if waiting {
            return Ok(Stop::Waiting);
        }
        if let Some(report) = session.task_deadlock.clone() {
            return Ok(Stop::Deadlocked(Box::new(report)));
        }
        budget.check(session)?;
        let step = next_slice(session, slice, &mut first);
        let until = VTime(now.0.saturating_add(step.0).min(deadline.0));
        // A watchpoint ends the slice at the store that hit it, so the next turn reads the new
        // value at that instant. The machine unmarks every page when the run returns.
        let outcome = session.run_until_with(until, &stops);
        // Only after this slice's lines were observed, so a line printed just before parking still
        // matches.
        waiting = outcome.reason == StopReason::Deadlock;
        // This run's own watch firing is a trigger, not the `watch` command's debug stop.
        let armed_here =
            matches!(outcome.reason, StopReason::Watchpoint { .. }) && !stops.watches.is_empty();
        if !waiting
            && !armed_here
            && let Some(error) = fault_of(&outcome.reason)
        {
            return Err(error.at_vt_us(outcome.vt.as_us()));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunArgs {
    pub instance: Option<String>,
    pub for_: Option<VTime>,
    pub until: Option<Wait>,
    pub fail_if: Vec<Wait>,
    pub timeout: VTime,
    pub wall_budget_ms: u64,
    /// Console the excerpt is read from.
    pub stream: SerialStream,
}

impl RunArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<RunArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "for",
                "until",
                "fail_if",
                "timeout",
                "wall_budget_ms",
                "stream",
            ],
        )?;
        let until = match opt_str(args, "until")? {
            None => None,
            Some(text) => Some(Wait::parse(text)?),
        };
        let fail_if = fail_matchers(args)?;
        let for_ = opt_duration(args, "for")?;
        if until.is_none() && for_.is_none() {
            return Err(
                ApiError::new(E_USAGE, "`run` needs `for`, `until` or both").with_hint(
                    "`--for 250ms` advances time; `--until serial:/ready/` waits for a matcher",
                ),
            );
        }
        Ok(RunArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            for_,
            until,
            fail_if,
            timeout: opt_duration(args, "timeout")?.unwrap_or(DEFAULT_TIMEOUT),
            wall_budget_ms: opt_u64(args, "wall_budget_ms")?.unwrap_or(DEFAULT_WALL_BUDGET_MS),
            stream: enum_of(args, "stream", parse_stream, "usj, uart0")?
                .unwrap_or(SerialStream::UsjTx),
        })
    }
}

/// Compiled like `until`, so an unevaluable matcher is refused here rather than never firing.
fn fail_matchers(args: &crate::args::JsonMap) -> Result<Vec<Wait>, ApiError> {
    match args.get("fail_if") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                serde_json::Value::String(text) => Wait::parse(text),
                _ => Err(usage("fail_if", "expected an array of matcher strings")),
            })
            .collect(),
        Some(_) => Err(usage("fail_if", "expected an array of matcher strings")),
    }
}

pub(crate) fn parse_stream(text: &str) -> Option<SerialStream> {
    match text {
        "usj" => Some(SerialStream::UsjTx),
        "uart0" => Some(SerialStream::Uart0Tx),
        _ => None,
    }
}

pub(crate) fn stream_name(stream: SerialStream) -> &'static str {
    match stream {
        SerialStream::UsjTx => "usj",
        SerialStream::Uart0Tx => "uart0",
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Matched,
    /// `for` ran out and no wait was given.
    Elapsed,
    Failed,
}

impl RunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunStatus::Matched => "matched",
            RunStatus::Elapsed => "elapsed",
            RunStatus::Failed => "failed",
        }
    }

    /// A fired `fail_if` is an assertion that did not hold.
    pub const fn result(self) -> &'static str {
        match self {
            RunStatus::Matched | RunStatus::Elapsed => "pass",
            RunStatus::Failed => "fail",
        }
    }
}

/// A guest that parks waiting for input first ends the call as `E_DEADLOCK` at that instant, with
/// the task table, serial tail and the inputs that can wake it.
pub fn run_on(session: &mut Session, args: &RunArgs) -> Result<Output, ApiError> {
    run_on_bare(session, args)
        .map_err(|error| crate::commands::inspect::deadlock_envelope(session, error))
}

fn run_on_bare(session: &mut Session, args: &RunArgs) -> Result<Output, ApiError> {
    let started = session.now();
    let wall = WallBudget::start(session, args.wall_budget_ms);
    let virtual_budget = match args.for_ {
        Some(d) => d,
        None => args.timeout,
    };
    let deadline = VTime(started.0.saturating_add(virtual_budget.0));
    // With nothing to observe, the cheaper loop makes the same machine calls and reads no ring.
    let stop = if args.until.is_none() && args.fail_if.is_empty() {
        advance_to(session, deadline, &wall)?
    } else {
        poll(session, args.until.as_ref(), &args.fail_if, deadline, &wall)?
    };
    // Settle after a `ui:` match, bounded by `ui`'s own settle default rather than this wait's
    // `timeout`, which is about the wait, not the settle.
    if matches!(stop, Stop::Matched(_)) && args.until.as_ref().is_some_and(Wait::settles_ui) {
        crate::commands::ui::settle_to_safe_point(
            session,
            crate::commands::ui::SETTLE_TIMEOUT_DEFAULT,
        )?;
    }
    let (status, hit) = match stop {
        Stop::Matched(hit) => (RunStatus::Matched, Some(hit)),
        Stop::Failed(hit) => (RunStatus::Failed, Some(hit)),
        Stop::Deadline if args.until.is_some() => return Err(timeout(session, args, started)),
        Stop::Deadline => (RunStatus::Elapsed, None),
        Stop::Waiting => {
            let at = session.now().as_us();
            return Err(fault_of(&StopReason::Deadlock)
                .expect("a deadlock is a fault")
                .at_vt_us(at));
        }
        Stop::Deadlocked(report) => {
            let at = session.now().as_us();
            return Err(crate::commands::inspect::task_deadlock_error(&report).at_vt_us(at));
        }
    };
    Ok(result_output(session, args, started, status, hit))
}

/// In slices, so a fault is reported at the slice it happened in.
fn advance_to(
    session: &mut Session,
    deadline: VTime,
    budget: &WallBudget,
) -> Result<Stop, ApiError> {
    let slice = slice_of(VTime(deadline.0.saturating_sub(session.now().0)));
    let mut first = true;
    while session.now().0 < deadline.0 {
        budget.check(session)?;
        let step = next_slice(session, slice, &mut first);
        let until = VTime(session.now().0.saturating_add(step.0).min(deadline.0));
        let outcome = session.run_until(until);
        if outcome.reason == StopReason::Deadlock {
            return Ok(Stop::Waiting);
        }
        if let Some(error) = fault_of(&outcome.reason) {
            return Err(error.at_vt_us(outcome.vt.as_us()));
        }
        if let Some(report) = session.task_deadlock.clone() {
            return Ok(Stop::Deadlocked(Box::new(report)));
        }
    }
    Ok(Stop::Deadline)
}

/// Carries the serial tail as its nearest evidence.
fn timeout(session: &mut Session, args: &RunArgs, started: VTime) -> ApiError {
    let excerpt = excerpt_of(session, args.stream);
    // The last lines of head and tail together; a short run has everything in `head`.
    let rendered: Vec<String> = excerpt
        .shaped
        .head
        .iter()
        .chain(excerpt.shaped.tail.iter())
        .map(crate::shape::Entry::render)
        .collect();
    let tail: Vec<String> = rendered
        .iter()
        .skip(rendered.len().saturating_sub(SERIAL_TAIL_LINES))
        .cloned()
        .collect();
    ApiError::new(
        E_TIMEOUT,
        format!(
            "no match within {}us of virtual time",
            VTime(session.now().0.saturating_sub(started.0)).as_us()
        ),
    )
    .retryable()
    .at_vt_us(session.now().as_us())
    .with_serial_tail(tail)
    .with_detail(serde_json::json!({
        "serial": excerpt.to_json(),
        "wall_budget_ms": args.wall_budget_ms,
    }))
    .with_hint("raise `--timeout`, or check the serial tail for where the guest stopped")
}

/// With the session's cursor moved past them.
pub(crate) fn excerpt_of(
    session: &mut Session,
    stream: SerialStream,
) -> crate::shape::SerialExcerpt {
    let cursor = session.cursor(stream);
    let chunk: Vec<u8> = {
        let ring = session.machine().io().serial_ring(stream);
        ring.slices(cursor.0).iter().copied().collect()
    };
    let excerpt = shape_serial(&chunk, cursor, &[], &ShapeLimits::DEFAULT);
    session.set_cursor(stream, excerpt.next_cursor);
    excerpt
}

fn result_output(
    session: &mut Session,
    args: &RunArgs,
    started: VTime,
    status: RunStatus,
    hit: Option<Hit>,
) -> Output {
    let excerpt = excerpt_of(session, args.stream);
    let receipt = session.receipt();
    let elapsed = VTime(session.now().0.saturating_sub(started.0));
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "status": status.as_str(),
        "result": status.result(),
        "vt_us": receipt.vt_us,
        "elapsed_vt_us": elapsed.as_us(),
        "match": hit.as_ref().map(Hit::to_json),
        "serial": excerpt.to_json(),
        "stream": stream_name(args.stream),
    });
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} {} vt={}us (+{}us)",
        session.id,
        status.as_str(),
        receipt.vt_us,
        elapsed.as_us()
    );
    if let Some(hit) = &hit {
        let label = if status == RunStatus::Failed {
            "fail_if"
        } else {
            "match"
        };
        let _ = writeln!(text, "{label} {}: {}", hit.source, hit.text);
    }
    let body = excerpt.to_text();
    if !body.is_empty() {
        text.push_str(&body);
    }
    Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT)
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`run` arguments.",
        "properties": {
            "instance": instance_schema(),
            "for": duration_schema("Duration to advance."),
            "until": { "type": "string", "description": "Matcher." },
            "fail_if": { "type": "array", "items": { "type": "string" }, "description": "Matchers that fail the run." },
            "timeout": duration_schema("Timeout, 10s."),
            "wall_budget_ms": { "type": "integer", "minimum": 100, "description": "Host budget in ms (30000)." },
            "stream": { "type": "string", "enum": ["usj", "uart0"], "description": "Console (usj)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "status": { "type": "string", "enum": ["matched", "elapsed", "failed"] },
            "result": { "type": "string", "enum": ["pass", "fail"] },
            "vt_us": { "type": "integer" },
            "elapsed_vt_us": { "type": "integer" },
            "match": { "type": ["object", "null"] },
            "serial": { "type": "object" },
            "stream": { "type": "string" }
        }
    })
}

/// Advance virtual time, or wait until a matcher fires.
#[command(
    api_crate = crate,
    name = "run",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["until"]),
    scenario_step = "wait",
    errors(E_USAGE, E_STATE, E_TIMEOUT, E_WALL_BUDGET, E_LEASE, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_UNMODELED, E_HLE, E_INTERNAL),
    example(
        title = "Advance 250 ms so a click is delivered",
        args = r#"{"for":"250ms"}"#,
    ),
    example(
        title = "Wait for a console line",
        args = r#"{"until":"serial:/pk_app: ready/","timeout":"5s"}"#,
    ),
)]
pub fn run(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = RunArgs::from_json(&args)?;
    // The run holds no pool lock, so instances run in parallel.
    crate::pool::with_session(
        |pool| {
            let id = pool.bind(SPEC_RUN.annotations, args.instance.as_deref())?;
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            let holder = crate::lease::LeaseHolder::Agent;
            if let Some(state) = pool.table().get(id) {
                state.lease.check_call(holder, SPEC_RUN.annotations, now)?;
            }
            Ok(id)
        },
        // A guest fault comes back with its envelope, decoded while the session still sits at the
        // stop.
        |session| {
            run_on(session, &args)
                .map_err(|error| crate::commands::inspect::fault_envelope_of(session, error))
        },
    )
}

/// Built here so every surface words it the same way. The machine stays paused, so the call can be
/// repeated.
pub fn wall_budget_exceeded(ms: u64) -> ApiError {
    ApiError::new(
        E_WALL_BUDGET,
        format!("the host budget of {ms} ms ran out before the wait finished"),
    )
    .retryable()
    .with_hint("the instance is paused where it got to; repeat the call to continue")
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_core::hostio::EventKind as RingKind;

    use crate::commands::start::tests::{TestMachine, started};

    fn wait(until: &str) -> RunArgs {
        RunArgs {
            instance: None,
            for_: None,
            until: Some(Wait::parse(until).expect("a matcher of the grammar")),
            fail_if: Vec::new(),
            timeout: VTime::from_ms(5_000),
            wall_budget_ms: DEFAULT_WALL_BUDGET_MS,
            stream: SerialStream::UsjTx,
        }
    }

    #[test]
    fn a_cancelled_session_ends_its_run_at_the_next_slice() {
        let (mut pool, id) = started(TestMachine::new());
        let mut session = pool.checkout(id).expect("in the pool");
        assert!(pool.cancel(id), "the flag is reachable while checked out");
        let args =
            RunArgs::from_json(&serde_json::json!({ "for": "10s", "wall_budget_ms": 1_000_000 }))
                .expect("inside the schema");
        let error = run_on(&mut session, &args).expect_err("cancelled");
        assert_eq!(error.code, E_STATE);
        assert!(error.message.contains("being stopped"), "{}", error.message);
        assert_eq!(
            session.now(),
            VTime(0),
            "not one slice ran after the cancel"
        );
        pool.checkin(session);
        pool.destroy(id).expect("the host ends it");
        assert!(!pool.cancel(id), "a destroyed session has no flag");
    }

    #[test]
    fn a_guest_waiting_for_input_ends_the_run_as_e_deadlock_with_the_task_table() {
        let _world = crate::commands::inspect::tests::world();
        crate::commands::inspect::set_introspectors(crate::commands::inspect::tests::SCRIPTED);
        for until in [None, Some("serial:/never/")] {
            let machine = TestMachine::new()
                .line(10, "I (10) pk_app: idle")
                .waits_for_input_at(30);
            let (mut pool, id) = started(machine);
            let mut json = serde_json::json!({ "for": "2s" });
            if let Some(until) = until {
                json = serde_json::json!({ "until": until, "timeout": "2s" });
            }
            let args = RunArgs::from_json(&json).expect("inside the schema");
            let session = pool.session_mut(id).expect("live");
            let error = run_on(session, &args).expect_err("a deadlock is reported");
            assert_eq!(error.code, E_DEADLOCK, "{until:?}: {error:?}");
            assert_eq!(error.vt_us, 30_000, "where the guest parked, not the limit");
            assert_eq!(error.detail["fault"], "deadlock");
            assert_eq!(
                error.detail["tasks"]["tasks"][1]["name"], "IDLE",
                "{}",
                error.detail
            );
            assert_eq!(error.detail["blocked"][0]["blocked_on"], "lvgl_port mutex");
            let wake = error.detail["wake_inputs"].as_array().expect("wake inputs");
            assert!(
                wake.iter()
                    .any(|w| w.as_str().unwrap().starts_with("input"))
            );
            assert!(
                error.serial_tail.iter().any(|l| l.contains("pk_app: idle")),
                "{:?}",
                error.serial_tail
            );
            assert!(
                error
                    .hint
                    .as_deref()
                    .unwrap_or_default()
                    .contains("wake_inputs"),
                "{:?}",
                error.hint
            );
            assert!(session.waiting_for_input);
            assert_eq!(
                session.now(),
                VTime::from_ms(30),
                "the run did not go on to the limit"
            );
        }
    }

    #[test]
    fn a_line_printed_just_before_the_guest_parks_still_matches() {
        let (mut pool, id) = started(
            TestMachine::new()
                .line(10, "I (10) pk_app: ready")
                .waits_for_input_at(30),
        );
        let session = pool.session_mut(id).expect("live");
        let out = run_on(session, &wait("serial:/pk_app: ready/")).expect("the line matched");
        assert_eq!(out.json["status"], "matched");
    }

    #[test]
    fn a_serial_matcher_fires_at_the_instant_of_its_line() {
        let (mut pool, id) = started(
            TestMachine::new()
                .line(10, "I (10) boot: esp-idf")
                .line(120, "I (120) pk_app: ready"),
        );
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = run_on(session, &wait("serial:/pk_app: ready/")).expect("the line is printed");
        assert_eq!(out.json["status"], "matched");
        assert_eq!(out.json["match"]["source"], "serial");
        assert_eq!(out.json["match"]["text"], "I (120) pk_app: ready");
        assert_eq!(out.json["match"]["vt_us"], 120_000);
        // Both lines are in the excerpt, because the run started at cursor 0.
        assert_eq!(out.json["serial"]["lines_total"], 2);
        assert!(out.text.contains("pk_app: ready"), "{}", out.text);
    }

    #[test]
    fn a_second_run_returns_only_what_is_new() {
        let (mut pool, id) = started(TestMachine::new().line(10, "first").line(200, "second"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let first = run_on(session, &wait("serial:/first/")).expect("the first line");
        let cursor = first.json["serial"]["next_cursor"]
            .as_u64()
            .expect("cursor");
        assert!(cursor > 0);
        let second = run_on(session, &wait("serial:/second/")).expect("the second line");
        assert_eq!(second.json["serial"]["cursor"], cursor);
        assert_eq!(second.json["serial"]["lines_total"], 1);
        assert!(!second.text.contains("first"), "{}", second.text);
    }

    #[test]
    fn a_log_matcher_reads_the_tag_and_the_level() {
        let (mut pool, id) = started(
            TestMachine::new()
                .line(10, "I (10) other: ready")
                .line(20, "W (20) pk_app: ready")
                .line(30, "I (30) pk_app: ready"),
        );
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = run_on(session, &wait("log:pk_app:I:/ready/")).expect("the third line matches");
        assert_eq!(out.json["match"]["source"], "log");
        assert_eq!(out.json["match"]["vt_us"], 30_000);
    }

    #[test]
    fn an_event_matcher_fires_on_the_event_ring() {
        let (mut pool, id) = started(TestMachine::new().event(40, RingKind::Reset));
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = run_on(session, &wait("event:reset")).expect("the ring carries a reset");
        assert_eq!(out.json["match"]["source"], "event");
        assert_eq!(out.json["match"]["text"], "reset");
        assert_eq!(out.json["match"]["vt_us"], 40_000);
    }

    #[test]
    fn a_virtual_time_matcher_fires_at_its_deadline() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = run_on(session, &wait("vt:250ms")).expect("virtual time reaches the deadline");
        assert_eq!(out.json["status"], "matched");
        assert_eq!(out.json["match"]["source"], "vt");
        assert!(out.json["vt_us"].as_u64().expect("vt") >= 250_000);
    }

    /// A booted instance sits at a non-zero virtual time, where reading the duration as absolute
    /// would fire at once.
    #[test]
    fn a_relative_virtual_time_matcher_counts_from_the_instant_the_wait_is_armed() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        session.run_until(VTime::from_ms(2_000));
        let out = run_on(session, &wait("vt:+250ms")).expect("virtual time reaches the deadline");
        assert_eq!(out.json["status"], "matched");
        assert_eq!(out.json["match"]["source"], "vt");
        assert_eq!(out.json["match"]["text"], "2250000us");
        assert_eq!(out.json["match"]["vt_us"], 2_250_000);
        assert_eq!(out.json["elapsed_vt_us"], 250_000);
    }

    #[test]
    fn an_absolute_virtual_time_matcher_that_is_already_past_fires_at_once() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        session.run_until(VTime::from_ms(2_000));
        let out = run_on(session, &wait("vt:250ms")).expect("the instant is already behind us");
        assert_eq!(out.json["match"]["text"], "250000us");
        assert_eq!(
            out.json["elapsed_vt_us"], 0,
            "an absolute instant is absolute"
        );
    }

    #[test]
    fn a_wait_that_never_fires_is_a_retryable_timeout_carrying_the_serial_tail() {
        let (mut pool, id) = started(TestMachine::new().line(10, "I (10) boot: stuck here"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let mut args = wait("serial:/never printed/");
        args.timeout = VTime::from_ms(100);
        let error = run_on(session, &args).expect_err("the line is never printed");
        assert_eq!(error.code, E_TIMEOUT);
        assert!(error.retryable);
        assert_eq!(error.vt_us, 100_000);
        assert!(
            error
                .serial_tail
                .iter()
                .any(|line| line.contains("stuck here")),
            "{:?}",
            error.serial_tail
        );
        assert_eq!(error.detail["wall_budget_ms"], DEFAULT_WALL_BUDGET_MS);
    }

    #[test]
    fn a_run_for_a_duration_elapses_and_advances_exactly_that_far() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        let args = RunArgs {
            instance: None,
            for_: Some(VTime::from_ms(250)),
            until: None,
            fail_if: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            wall_budget_ms: DEFAULT_WALL_BUDGET_MS,
            stream: SerialStream::UsjTx,
        };
        let out = run_on(session, &args).expect("a fixed advance always succeeds");
        assert_eq!(out.json["status"], "elapsed");
        assert_eq!(out.json["result"], "pass");
        assert_eq!(out.json["vt_us"], 250_000);
        assert_eq!(out.json["elapsed_vt_us"], 250_000);
        assert_eq!(out.json["match"], serde_json::Value::Null);
    }

    #[test]
    fn a_run_with_neither_for_nor_until_is_usage() {
        let error = RunArgs::from_json(&serde_json::json!({})).expect_err("a run needs one");
        assert_eq!(error.code, E_USAGE);
        assert!(error.hint.is_some());
    }

    #[test]
    fn a_matcher_this_build_cannot_evaluate_says_what_it_lacks() {
        for (text, lacks) in [
            ("symbol:lv_timer_handler", "observe hooks"),
            ("ui:label=\"Button\"", "ui-hint vocabulary"),
            ("addr:0x3fca1b14 == 1", "no matcher yet"),
        ] {
            let error = Wait::parse(text).expect_err("not evaluated yet");
            assert_eq!(error.code, E_UNMODELED, "{text}");
            assert!(error.message.contains(lacks), "{}", error.message);
        }
    }

    /// `s_sel` is 0 until 40 ms of guest time, 1 after. It reads only the machine, so the test
    /// needs no ELF.
    fn timed_vars(
        machine: &mut dyn pemu_machine::MachineApi,
        queries: &[VarQuery],
    ) -> Result<pemu_introspect::vars::VarSnapshot, pemu_introspect::IntrospectError> {
        use pemu_introspect::vars::{VarRead, VarReading, VarSnapshot, VarValue};
        let value = i64::from(machine.now().as_us() >= 40_000);
        Ok(VarSnapshot {
            reads: queries
                .iter()
                .map(|q| match q.name.as_str() {
                    "s_sel" => VarRead::Ok(VarReading {
                        query: q.render(),
                        name: q.name.clone(),
                        unit: "main.c".to_owned(),
                        // Guest DRAM, so the run arms a real page watch rather than falling back to
                        // polling.
                        addr: 0x3fca_b5cc,
                        len: 4,
                        ty: "int32".to_owned(),
                        value: VarValue::Int(value),
                    }),
                    _ => VarRead::Err {
                        query: q.render(),
                        error: pemu_introspect::IntrospectError::MissingGlobal { name: q.render() },
                    },
                })
                .collect(),
        })
    }

    fn with_timed_vars<R>(f: impl FnOnce() -> R) -> R {
        use crate::commands::inspect::{Introspectors, set_introspectors, tests::SCRIPTED};
        set_introspectors(Introspectors {
            vars: timed_vars,
            ..SCRIPTED
        });
        let out = f();
        set_introspectors(SCRIPTED);
        out
    }

    /// The match lands at the slice the value changed in, not at the run's limit.
    #[test]
    fn a_var_wait_matches_when_the_global_reaches_its_value() {
        let _world = crate::commands::inspect::tests::world();
        with_timed_vars(|| {
            let (mut pool, id) = started(TestMachine::new());
            let session = pool.session_mut(id).expect("the scripted instance");
            let out = run_on(session, &wait("var:s_sel == 1")).expect("s_sel reaches 1");
            assert_eq!(out.json["status"], "matched");
            assert_eq!(out.json["match"]["source"], "var");
            assert_eq!(out.json["match"]["text"], "s_sel = 1");
            assert!(
                out.json["match"]["vt_us"].as_u64().expect("an instant") >= 40_000,
                "{}",
                out.json["match"]
            );
        });
    }

    #[test]
    fn a_var_changed_wait_needs_a_change_and_not_a_first_read() {
        let _world = crate::commands::inspect::tests::world();
        with_timed_vars(|| {
            let (mut pool, id) = started(TestMachine::new());
            let session = pool.session_mut(id).expect("the scripted instance");
            let out = run_on(session, &wait("var:s_sel changed")).expect("it changes at 40ms");
            assert_eq!(out.json["status"], "matched");
            assert_eq!(out.json["match"]["source"], "var");
            assert!(
                out.json["match"]["vt_us"].as_u64().expect("an instant") >= 40_000,
                "not the first read at vt 0: {}",
                out.json["match"]
            );
        });
    }

    #[test]
    fn a_var_wait_that_never_fires_times_out_rather_than_passing() {
        let _world = crate::commands::inspect::tests::world();
        with_timed_vars(|| {
            for until in ["var:s_sel == 7", "var:s_nope == 1"] {
                let (mut pool, id) = started(TestMachine::new());
                let session = pool.session_mut(id).expect("the scripted instance");
                let args = RunArgs::from_json(&serde_json::json!({
                    "until": until,
                    "timeout": "100ms"
                }))
                .expect("inside the schema");
                let error = run_on(session, &args).expect_err("it never fires");
                assert_eq!(error.code, E_TIMEOUT, "{until}: {error:?}");
            }
        });
    }

    #[test]
    fn a_plain_run_reads_no_global_and_arms_no_watch() {
        let _world = crate::commands::inspect::tests::world();
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        assert!(
            var_watch(session, &[])
                .expect("no globals")
                .watches
                .is_empty(),
            "an empty watch list marks no page slow"
        );
        assert!(read_vars(session, &[], VTime(0)).is_empty());
        let args =
            RunArgs::from_json(&serde_json::json!({ "for": "10ms" })).expect("inside the schema");
        let out = run_on(session, &args).expect("a fixed advance");
        assert_eq!(out.json["status"], "elapsed");
    }

    /// The machine really marks the page slow rather than dropping the watch.
    #[test]
    fn a_var_wait_arms_a_write_watch_over_the_global_it_watches() {
        let _world = crate::commands::inspect::tests::world();
        with_timed_vars(|| {
            let (mut pool, id) = started(TestMachine::new());
            let session = pool.session_mut(id).expect("the scripted instance");
            let stops =
                var_watch(session, &[VarQuery::parse("s_sel").expect("a name")]).expect("resolved");
            assert_eq!(stops.watches.len(), 1);
            assert_eq!(stops.watches[0].addr, 0x3fca_b5cc);
            assert_eq!(stops.watches[0].len, 4);
            stops.check().expect("the machine can arm it");
            // A global the firmware lacks arms nothing and is not an error.
            let none = var_watch(session, &[VarQuery::parse("s_nope").expect("a name")])
                .expect("no refusal");
            assert!(none.watches.is_empty());
        });
    }

    /// A host does not remember an ELF miss, so retrying every slice would re-read the corpus
    /// manifest from disk each time.
    #[test]
    fn a_firmware_with_no_walker_stops_being_read_rather_than_retried_every_slice() {
        use crate::commands::inspect::{NO_INTROSPECTORS, set_introspectors, tests::SCRIPTED};
        let _world = crate::commands::inspect::tests::world();
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        set_introspectors(NO_INTROSPECTORS);
        let answer = var_watch(session, &[VarQuery::parse("s_sel").expect("a name")]);
        let read = read_vars(
            session,
            &[VarQuery::parse("s_sel").expect("a name")],
            VTime(0),
        );
        // Still ends, as a timeout rather than a pass.
        let args = RunArgs::from_json(&serde_json::json!({
            "until": "var:s_sel == 1",
            "timeout": "50ms"
        }))
        .expect("inside the schema");
        let error = run_on(session, &args).expect_err("nothing can read the global");
        set_introspectors(SCRIPTED);
        assert!(answer.is_none(), "no walker is reported as no walker");
        assert!(read.is_empty(), "and reads nothing");
        assert_eq!(error.code, E_TIMEOUT, "{error:?}");
    }

    #[test]
    fn the_ui_and_var_rows_compile() {
        let ui = Wait::parse("ui:changed").expect("the ui row is evaluated");
        assert!(ui.settles_ui(), "a `ui:` match settles");
        assert!(ui.watched_vars().is_empty());

        for text in [
            "var:s_sel == 1",
            "var:s_sel changed",
            "var:main.c::s_sel >= 0",
            "var:s_ok[2] != 0",
            "var:s_name == \"Button\"",
            "var:s_ready == true",
        ] {
            Wait::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        }
        let wait = Wait::parse("all(var:s_sel == 1,var:s_active changed,var:s_sel != 9)")
            .expect("a composite");
        let watched = wait.watched_vars();
        assert_eq!(
            watched.len(),
            2,
            "one read per distinct global per slice, not per leaf: {watched:?}"
        );
    }

    /// Before a run, a DWARF parse or a page marked slow.
    #[test]
    fn a_var_matcher_outside_the_grammar_is_refused_at_compile_time() {
        for text in [
            "var:s_ok[x] == 1",
            "var:s_ok[ == 1",
            "var:s_name < \"Button\"",
        ] {
            let error = Wait::parse(text).expect_err("outside the grammar");
            assert_eq!(error.code, E_USAGE, "{text}");
        }
    }

    #[test]
    fn a_comparison_orders_what_it_can_and_refuses_what_it_cannot() {
        use pemu_introspect::vars::VarValue;
        let int = VarValue::Int(1);
        assert!(compare(&int, CmpOp::Eq, &Value::Int(1)));
        assert!(compare(&int, CmpOp::Ne, &Value::Int(0)));
        assert!(compare(&int, CmpOp::Ge, &Value::Int(1)));
        assert!(compare(&int, CmpOp::Lt, &Value::Int(2)));
        assert!(!compare(&int, CmpOp::Gt, &Value::Int(2)));
        let yes = VarValue::Bool(true);
        assert!(compare(&yes, CmpOp::Eq, &Value::Int(1)));
        assert!(compare(&yes, CmpOp::Eq, &Value::Bool(true)));
        assert!(compare(&yes, CmpOp::Ne, &Value::Bool(false)));
        let text = VarValue::Text("Button".into());
        assert!(compare(&text, CmpOp::Eq, &Value::Text("Button".into())));
        assert!(compare(&text, CmpOp::Ne, &Value::Text("Display".into())));
        assert!(!compare(&text, CmpOp::Eq, &Value::Int(0)));
        let opaque = VarValue::Opaque {
            name: "lvgl_port_ctx_t".into(),
            size: 96,
        };
        for op in [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Ge] {
            assert!(!compare(&opaque, op, &Value::Int(0)), "{op:?}");
        }
    }

    #[test]
    fn a_composite_wait_fires_only_when_every_child_has() {
        let (mut pool, id) = started(TestMachine::new().line(10, "alpha").line(50, "beta"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = run_on(session, &wait("all(serial:/alpha/,serial:/beta/)"))
            .expect("both lines are printed");
        assert_eq!(out.json["status"], "matched");
        assert_eq!(out.json["match"]["vt_us"], 50_000);
    }

    #[test]
    fn a_sequence_wait_needs_its_children_in_order() {
        let (mut pool, id) = started(TestMachine::new().line(10, "beta").line(50, "alpha"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let mut args = wait("seq(serial:/alpha/,serial:/beta/)");
        args.timeout = VTime::from_ms(100);
        let error = run_on(session, &args).expect_err("`beta` came first");
        assert_eq!(error.code, E_TIMEOUT);
    }

    #[test]
    fn the_slice_is_a_pure_function_of_the_budget() {
        assert_eq!(slice_of(VTime::from_ms(10_000)), VTime::from_ms(10));
        assert_eq!(slice_of(VTime::from_ms(1)), MIN_SLICE);
        assert_eq!(slice_of(VTime(0)), MIN_SLICE);
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("run").expect("#[command] registered run");
        for example in spec.examples {
            let args = example.args_json().expect("an example is JSON");
            RunArgs::from_json(&args).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.advances_time && spec.annotations.needs_instance);
        assert_eq!(spec.scenario_step, Some("wait"));
        assert!(spec.errors.contains(&E_TIMEOUT) && spec.errors.contains(&E_WALL_BUDGET));
    }

    #[test]
    fn the_host_budget_refusal_is_retryable_and_says_the_machine_is_paused() {
        let error = wall_budget_exceeded(30_000);
        assert_eq!(error.code, E_WALL_BUDGET);
        assert!(error.retryable);
        assert!(error.hint.as_deref().unwrap_or_default().contains("repeat"));
    }

    /// A second per reading, so any loop of more than one slice spends a short budget.
    fn jumpy_clock() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static MS: AtomicU64 = AtomicU64::new(0);
        MS.fetch_add(1_000, Ordering::Relaxed)
    }

    #[test]
    fn a_wait_that_outlives_the_host_budget_is_a_wall_timeout_and_not_a_virtual_one() {
        let (mut pool, id) = started(TestMachine::new().line(10, "I (10) boot: still here"));
        let session = pool.session_mut(id).expect("the scripted instance");
        session.host_clock = Some(jumpy_clock);
        let mut args = wait("serial:/never printed/");
        args.timeout = VTime::from_ms(10_000);
        args.wall_budget_ms = 100;
        let error = run_on(session, &args).expect_err("the host budget runs out first");
        assert_eq!(error.code, E_WALL_BUDGET);
        assert!(error.retryable);
        assert!(
            session.now().0 < VTime::from_ms(10_000).0,
            "the run stopped where it got to, not at the virtual deadline"
        );
    }

    #[test]
    fn a_fixed_advance_is_bounded_by_the_host_budget_too() {
        let (mut pool, id) = started(TestMachine::new());
        let session = pool.session_mut(id).expect("the scripted instance");
        session.host_clock = Some(jumpy_clock);
        let args = RunArgs {
            instance: None,
            for_: Some(VTime::from_ms(10_000)),
            until: None,
            fail_if: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            wall_budget_ms: 100,
            stream: SerialStream::UsjTx,
        };
        let error = run_on(session, &args).expect_err("the host budget runs out first");
        assert_eq!(error.code, E_WALL_BUDGET);
    }

    #[test]
    fn with_no_host_clock_installed_the_budget_is_not_enforced() {
        let (mut pool, id) = started(TestMachine::new().line(10, "I (10) pk_app: ready"));
        let session = pool.session_mut(id).expect("the scripted instance");
        assert!(session.host_clock.is_none(), "a plain pool installs none");
        let mut args = wait("serial:/pk_app: ready/");
        args.wall_budget_ms = 100;
        let out = run_on(session, &args).expect("nothing bounds host time here");
        assert_eq!(out.json["status"], "matched");
    }

    #[test]
    fn a_fail_if_matcher_ends_the_run_with_a_failing_result() {
        let (mut pool, id) = started(
            TestMachine::new()
                .line(10, "I (10) boot: esp-idf")
                .line(40, "Guru Meditation Error")
                .line(90, "I (90) pk_app: ready"),
        );
        let session = pool.session_mut(id).expect("the scripted instance");
        let mut args = wait("serial:/pk_app: ready/");
        args.fail_if = vec![Wait::parse("serial:/Guru Meditation/").expect("a matcher")];
        let out = run_on(session, &args).expect("a failed assertion is a result, not a refusal");
        assert_eq!(out.json["status"], "failed");
        assert_eq!(out.json["result"], "fail");
        assert_eq!(out.json["match"]["vt_us"], 40_000);
        assert!(out.text.contains("fail_if serial:"), "{}", out.text);
    }

    #[test]
    fn a_fail_if_that_never_fires_leaves_the_result_at_pass() {
        let (mut pool, id) = started(TestMachine::new().line(10, "I (10) pk_app: ready"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let mut args = wait("serial:/pk_app: ready/");
        args.fail_if = vec![Wait::parse("serial:/Guru Meditation/").expect("a matcher")];
        let out = run_on(session, &args).expect("the wait fires first");
        assert_eq!(out.json["status"], "matched");
        assert_eq!(out.json["result"], "pass");
    }

    /// `expect_not` maps to this: a window that fails only if the forbidden pattern appears.
    #[test]
    fn a_fail_if_watches_a_fixed_window_with_no_wait_of_its_own() {
        let (mut pool, id) = started(TestMachine::new().line(40, "Guru Meditation Error"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let args = RunArgs {
            instance: None,
            for_: Some(VTime::from_ms(200)),
            until: None,
            fail_if: vec![Wait::parse("serial:/Guru Meditation/").expect("a matcher")],
            timeout: DEFAULT_TIMEOUT,
            wall_budget_ms: DEFAULT_WALL_BUDGET_MS,
            stream: SerialStream::UsjTx,
        };
        let out = run_on(session, &args).expect("a window that failed is still a result");
        assert_eq!(out.json["result"], "fail");
        assert_eq!(out.json["match"]["vt_us"], 40_000);

        let (mut pool, id) = started(TestMachine::new().line(40, "I (40) pk_app: quiet"));
        let session = pool.session_mut(id).expect("the scripted instance");
        let out = run_on(session, &args).expect("nothing forbidden appeared");
        assert_eq!(out.json["status"], "elapsed");
        assert_eq!(out.json["result"], "pass");
        assert_eq!(out.json["vt_us"], 200_000);
    }

    #[test]
    fn a_fail_if_outside_the_grammar_is_refused_when_the_arguments_are_read() {
        for case in [
            serde_json::json!({ "for": "10ms", "fail_if": "serial:/x/" }),
            serde_json::json!({ "for": "10ms", "fail_if": [7] }),
            serde_json::json!({ "for": "10ms", "fail_if": ["symbol:lv_timer_handler"] }),
            serde_json::json!({ "for": "10ms", "fail_if": ["var:s_sel"] }),
        ] {
            let error = RunArgs::from_json(&case).expect_err("outside the schema or the grammar");
            assert!(
                error.code == E_USAGE || error.code == E_UNMODELED,
                "{case}: {error:?}"
            );
        }
    }
}
