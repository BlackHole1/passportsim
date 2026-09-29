//! The matcher grammar, compiled to trigger classes. Parsing only: `commands/run.rs` evaluates over
//! `pemu_machine::stops`, and nothing here reads guest state.
//!
//! Regex-free by construction (the workspace has no regex engine): a `/.../` body compiles to a
//! [`TextClass`], each bounded-time on 400-character lines: `Literal` `/ready/`, `Prefix` `/^I (/`,
//! `Suffix` `/ done$/`, `Exact` `/^menu ready$/` or `"menu ready"`, `Glob` `/^?? ready$/` or
//! `/*ready/`. Real regex syntax (`+ | [ ] { }`, an interior `^` or `$`, `\d`, `.*`, `.?`) is
//! `E_USAGE`, as is a wildcard after a literal character, since `ready*` means something else in
//! RE2; `\*` and `\?` are literal. A lone `.` and `(`/`)` stay literal, because ESP log lines are
//! full of them. UNVERIFIED: whether the slash notation should mean RE2 is open.
//!
//! [`TriggerClass`] is a host-side classification, unrelated to the hart's eight debug triggers: an
//! address matcher is a `PF_SLOW` page watch, so a run may arm more than eight.

use std::collections::BTreeSet;

use pemu_core::time::VTime;

use crate::error::{ApiError, E_USAGE};

/// UNVERIFIED limit. The parser is recursive, so one is needed to keep a hostile pattern from
/// overflowing the 1 MiB stack of a Windows `.exe`.
pub const MAX_DEPTH: usize = 8;

/// UNVERIFIED limit, chosen so a composite still fits the output budget.
pub const MAX_CHILDREN: usize = 16;

/// The regex-free class a console or UI pattern compiles to (see the module docs).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TextClass {
    Literal,
    Exact,
    Prefix,
    Suffix,
    /// `*`, `?` and literal runs.
    Glob,
}

impl TextClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            TextClass::Literal => "literal",
            TextClass::Exact => "exact",
            TextClass::Prefix => "prefix",
            TextClass::Suffix => "suffix",
            TextClass::Glob => "glob",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextPattern {
    Literal(Box<str>),
    Exact(Box<str>),
    Prefix(Box<str>),
    Suffix(Box<str>),
    Glob(Glob),
}

impl TextPattern {
    /// Reported by `run` in its match object, so an agent can see how its pattern was read.
    pub fn class(&self) -> TextClass {
        match self {
            TextPattern::Literal(_) => TextClass::Literal,
            TextPattern::Exact(_) => TextClass::Exact,
            TextPattern::Prefix(_) => TextClass::Prefix,
            TextPattern::Suffix(_) => TextClass::Suffix,
            TextPattern::Glob(_) => TextClass::Glob,
        }
    }

    /// Pure and allocation-free, so the run loop can call it on every console line.
    pub fn matches(&self, subject: &str) -> bool {
        match self {
            TextPattern::Literal(t) => subject.contains(&**t),
            TextPattern::Exact(t) => subject == &**t,
            TextPattern::Prefix(t) => subject.starts_with(&**t),
            TextPattern::Suffix(t) => subject.ends_with(&**t),
            TextPattern::Glob(g) => g.matches(subject),
        }
    }

    /// Compiles the body between the slashes by the class rules of the module docs.
    pub fn compile(body: &str) -> Result<TextPattern, ApiError> {
        let toks = lex_pattern(body)?;
        let (anchor_start, anchor_end, body_toks) = split_anchors(&toks);
        if body_toks.is_empty() {
            return Err(usage(
                "empty pattern: write the text to look for between the slashes",
            ));
        }
        let wild = body_toks.iter().any(|t| matches!(t, Tok::Any | Tok::One));
        if wild {
            let mut toks: Vec<Tok> = Vec::with_capacity(body_toks.len() + 2);
            if !anchor_start {
                toks.push(Tok::Any);
            }
            toks.extend(body_toks.iter().copied());
            if !anchor_end {
                toks.push(Tok::Any);
            }
            return Ok(TextPattern::Glob(Glob {
                source: body.into(),
                toks: normalize(toks).into_boxed_slice(),
            }));
        }
        let text: String = body_toks
            .iter()
            .map(|t| match t {
                Tok::Ch(c) => *c,
                // `wild` is false, so no `Any` or `One` is left in `body_toks`.
                Tok::Any | Tok::One => unreachable!("wildcards were handled above"),
            })
            .collect();
        Ok(match (anchor_start, anchor_end) {
            (true, true) => TextPattern::Exact(text.into_boxed_str()),
            (true, false) => TextPattern::Prefix(text.into_boxed_str()),
            (false, true) => TextPattern::Suffix(text.into_boxed_str()),
            (false, false) => TextPattern::Literal(text.into_boxed_str()),
        })
    }

    /// The escape-free spelling of `/^...$/`, as in `ui:label="Button"`.
    pub fn literal_exact(text: &str) -> TextPattern {
        TextPattern::Exact(text.into())
    }

    /// The `~` operator of `ui:focused~"Display"`.
    pub fn literal_contains(text: &str) -> TextPattern {
        TextPattern::Literal(text.into())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Tok {
    Ch(char),
    /// Any run of characters, including none.
    Any,
    /// Exactly one character.
    One,
}

/// Anchoring is folded in at compile time: an unanchored pattern gets a leading and trailing `*`,
/// so matching is always a whole-subject match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
    source: Box<str>,
    toks: Box<[Tok]>,
}

impl Glob {
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Two-pointer wildcard walk with one backtrack point per `*`, over characters, not bytes. It
    /// decodes one character at a time and never copies the line, because a stream matcher runs on
    /// every byte appended to a console.
    pub fn matches(&self, subject: &str) -> bool {
        let t = &self.toks;
        // `i` and `mark` are byte offsets on character boundaries; `j` indexes `t`.
        let (mut i, mut j) = (0usize, 0usize);
        let mut star: Option<usize> = None;
        let mut mark = 0usize;
        while let Some(c) = subject[i..].chars().next() {
            match t.get(j) {
                Some(Tok::One) => {
                    i += c.len_utf8();
                    j += 1;
                }
                Some(Tok::Ch(k)) if *k == c => {
                    i += c.len_utf8();
                    j += 1;
                }
                Some(Tok::Any) => {
                    star = Some(j);
                    mark = i;
                    j += 1;
                }
                _ => match star {
                    Some(sj) => {
                        j = sj + 1;
                        // `mark` is at most `i`, inside the subject here, so the step is never 0
                        // and the walk advances.
                        mark += subject[mark..].chars().next().map_or(0, char::len_utf8);
                        i = mark;
                    }
                    None => return false,
                },
            }
        }
        t[j..].iter().all(|k| *k == Tok::Any)
    }
}

fn normalize(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    for t in toks {
        if t == Tok::Any && out.last() == Some(&Tok::Any) {
            continue;
        }
        out.push(t);
    }
    out
}

fn split_anchors(toks: &[Tok]) -> (bool, bool, &[Tok]) {
    // `lex_pattern` emits the sentinels only in an anchor position and refuses every other `^` or
    // `$`.
    let start = toks.first() == Some(&Tok::Ch(ANCHOR_START));
    let end = toks.len() > usize::from(start) && toks.last() == Some(&Tok::Ch(ANCHOR_END));
    let from = usize::from(start);
    let to = toks.len() - usize::from(end);
    (start, end, &toks[from..to])
}

/// `lex_pattern` refuses ASCII control characters, so a sentinel never collides with literal text.
const ANCHOR_START: char = '\u{1}';
const ANCHOR_END: char = '\u{2}';

/// `^` and `$` in an anchor position become the sentinel characters.
fn lex_pattern(body: &str) -> Result<Vec<Tok>, ApiError> {
    let chars: Vec<char> = body.chars().collect();
    let mut out: Vec<Tok> = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let next = chars.get(i + 1).copied().ok_or_else(|| {
                    usage("pattern ends with a lone `\\`: write `\\\\` for a backslash")
                })?;
                // Every metacharacter, plus every refused character, so refusals can offer
                // `\<char>` as the literal spelling.
                if !matches!(
                    next,
                    '*' | '?'
                        | '^'
                        | '$'
                        | '\\'
                        | '/'
                        | '.'
                        | '+'
                        | '|'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '('
                        | ')'
                ) {
                    return Err(usage(format!(
                        "unsupported escape `\\{next}`: matchers are regex-free, so an escape \
                         only spells a metacharacter literally (`\\*`, `\\?`, `\\^`, `\\$`, \
                         `\\\\`, `\\/`, `\\.`) or one of the refused characters (`\\+`, `\\|`, \
                         `\\[`, `\\]`, `\\{{`, `\\}}`, `\\(`, `\\)`)"
                    )));
                }
                out.push(Tok::Ch(next));
                i += 2;
                continue;
            }
            '*' | '?' => {
                reject_repetition_position(c, out.last())?;
                out.push(if c == '*' { Tok::Any } else { Tok::One });
            }
            '^' => {
                if i != 0 {
                    return Err(usage(
                        "`^` anchors the start of the line and is allowed only as the first \
                         character; write `\\^` for a literal `^`",
                    ));
                }
                out.push(Tok::Ch(ANCHOR_START));
            }
            '$' => {
                if i != chars.len() - 1 {
                    return Err(usage(
                        "`$` anchors the end of the line and is allowed only as the last \
                         character; write `\\$` for a literal `$`",
                    ));
                }
                out.push(Tok::Ch(ANCHOR_END));
            }
            '.' => {
                if matches!(chars.get(i + 1), Some('*') | Some('?') | Some('+')) {
                    return Err(usage(
                        "`.*`, `.?` and `.+` are regular-expression syntax: matchers are \
                         regex-free, so write `*` for any run and `?` for one character",
                    ));
                }
                out.push(Tok::Ch('.'));
            }
            '+' | '|' | '[' | ']' | '{' | '}' => {
                return Err(usage(format!(
                    "`{c}` is regular-expression syntax: matchers are regex-free, so use `*`, \
                     `?`, `^` and `$`, or `\\{c}` for a literal `{c}`"
                )));
            }
            _ if c.is_control() => {
                return Err(usage(
                    "a control character cannot appear in a pattern: console lines are split on \
                     the newline and ANSI is stripped before matching",
                ));
            }
            _ => out.push(Tok::Ch(c)),
        }
        i += 1;
    }
    Ok(out)
}

/// Refuses a wildcard where a regex would read it as a repetition of the previous character, the
/// one place the two notations disagree silently. At the start of the body or after a wildcard, `*`
/// and `?` are not valid RE2 at all.
fn reject_repetition_position(c: char, previous: Option<&Tok>) -> Result<(), ApiError> {
    let after_literal = match previous {
        Some(Tok::Ch(ANCHOR_START)) | Some(Tok::Any) | Some(Tok::One) | None => false,
        Some(Tok::Ch(_)) => true,
    };
    if !after_literal {
        return Ok(());
    }
    let regex_reading = if c == '*' {
        "zero or more of the character before it"
    } else {
        "the character before it, optionally"
    };
    Err(usage(format!(
        "`{c}` after a literal character is refused: a regular expression reads it as \
         {regex_reading}, a glob as that character then a wildcard, and matchers are regex-free, \
         so the two readings would silently disagree. Write `/text/` (contains), `/^text/` \
         (starts), `/text$/` (ends) or `/^text$/` (equals), put the wildcard at the start of the \
         body (`/{c}text/`), or write `\\{c}` for a literal `{c}`"
    )))
}

fn usage(message: impl Into<String>) -> ApiError {
    ApiError::new(E_USAGE, message).with_hint("see `docs/commands/run.md` for the matcher grammar")
}

/// The run loop arms one hook per distinct class, so a matcher costs nothing on paths it cannot
/// fire on.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TriggerClass {
    /// A newline appended to a console channel: `serial` in line mode, `log`.
    ConsoleLine,
    /// Every byte appended to a console channel: `serial` with `stream`.
    ConsoleStream,
    /// A display generation change, then the next LVGL safe point.
    UiGeneration,
    EventRing,
    /// An observe hook: `symbol`.
    ObserveHook,
    /// A store to the watched `PF_SLOW` page: `var`, `addr`.
    DataWrite,
    /// A write to the named MMIO register: `reg`.
    MmioWrite,
    /// The deadline: `vt`.
    Deadline,
}

impl TriggerClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            TriggerClass::ConsoleLine => "console_line",
            TriggerClass::ConsoleStream => "console_stream",
            TriggerClass::UiGeneration => "ui_generation",
            TriggerClass::EventRing => "event_ring",
            TriggerClass::ObserveHook => "observe_hook",
            TriggerClass::DataWrite => "data_write",
            TriggerClass::MmioWrite => "mmio_write",
            TriggerClass::Deadline => "deadline",
        }
    }
}

/// A string-valued enum with `as_str`, `parse` and `ALL`, for the closed matcher vocabularies.
macro_rules! str_enum {
    (
        $(#[$meta:meta])* $vis:vis enum $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis enum $name { $($(#[$vmeta])* $variant),* }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),* }
            }

            pub fn parse(text: &str) -> Option<$name> {
                match text { $($text => Some($name::$variant),)* _ => None }
            }

            /// Comma separated, for a refusal message.
            pub fn vocabulary() -> String {
                let names: Vec<&str> = $name::ALL.iter().map(|v| v.as_str()).collect();
                names.join(", ")
            }
        }
    };
}

pub(crate) use str_enum;

str_enum! {
    pub enum Channel {
        /// The default channel of the board.
        Usj = "usj",
        Uart0 = "uart0",
        /// Fires on whichever appends the line first.
        Any = "any",
    }
}

str_enum! {
    pub enum ConsoleMode {
        Line = "line",
        /// So a prompt without a trailing newline can be matched.
        Stream = "stream",
    }
}

str_enum! {
    pub enum From {
        /// The instance's absolute serial cursor, which survives resets.
        Cursor = "cursor",
        /// Ignores everything already buffered.
        Now = "now",
        Start = "start",
    }
}

str_enum! {
    pub enum LogLevel {
        Error = "E",
        Warn = "W",
        Info = "I",
        Debug = "D",
        Verbose = "V",
    }
}

str_enum! {
    /// Event ring kind an `event:` matcher waits for. UNVERIFIED: whether `wifi_connected` and
    /// `wifi_got_ip` are one event or two; both spellings parse.
    pub enum EventKind {
        Reset = "reset",
        Panic = "panic",
        Wdt = "wdt",
        Brownout = "brownout",
        /// Three resets inside 10 s.
        BootLoop = "boot_loop",
        Sleep = "sleep",
        Wake = "wake",
        PowerOff = "power_off",
        UiChanged = "ui_changed",
        Frame = "frame",
        /// A host opened the USB Serial/JTAG console.
        UsbOpen = "usb_open",
        WifiConnected = "wifi_connected",
        WifiGotIp = "wifi_got_ip",
        WifiDisconnected = "wifi_disconnected",
        BleAdv = "ble_adv",
        BleConnected = "ble_connected",
        NfcRead = "nfc_read",
    }
}

str_enum! {
    pub enum UiAttr {
        Label = "label",
        Text = "text",
        /// LVGL class without the `lv_` prefix.
        Class = "class",
        /// Role assigned by ui-hints.
        Role = "role",
        Focused = "focused",
        Selected = "selected",
    }
}

str_enum! {
    /// UNVERIFIED, like [`Matcher::Addr`].
    pub enum Width {
        U8 = "u8",
        /// Little-endian.
        U16 = "u16",
        /// Little-endian. The default.
        U32 = "u32",
    }
}

str_enum! {
    pub enum CmpOp {
        Eq = "==",
        Ne = "!=",
        Le = "<=",
        Ge = ">=",
        Lt = "<",
        Gt = ">",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bool(bool),
    /// For a `char[]` global.
    Text(Box<str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueTest {
    /// `changed`: fires on any store that changes the value.
    Changed,
    Cmp(CmpOp, Value),
}

/// `serial:/pk_app: ready/`, with `stream` and `from=cursor|now|start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialMatcher {
    /// `usj` when the matcher names none.
    pub channel: Channel,
    pub mode: ConsoleMode,
    pub from: From,
    pub pattern: TextPattern,
}

/// `log:pk_app:I:/ready/`. A `*` tag or level matches any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMatcher {
    pub tag: Option<Box<str>>,
    pub level: Option<LogLevel>,
    /// Matched against the message after the tag and level.
    pub pattern: TextPattern,
}

/// Settles to the LVGL safe point by default ([`Matcher::default_settle`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiMatcher {
    /// Any display generation change.
    Changed,
    /// `=` is exact, `~` contains.
    Query { attr: UiAttr, pattern: TextPattern },
}

/// `symbol:lv_timer_handler:hits=3`; a `0x` address may replace the name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolMatcher {
    pub target: SymbolTarget,
    /// 1 unless `hits=` says otherwise.
    pub hits: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolTarget {
    /// Resolved from DWARF at arm time.
    Name(Box<str>),
    Addr(u32),
}

/// `var:s_sel == 1` over a DWARF-typed global. The name may carry a compilation unit and an index,
/// as in `main.c::s_sel` and `s_ok[2]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VarMatcher {
    pub name: Box<str>,
    pub test: ValueTest,
}

/// `addr:0x3fca1b14 == 1` or `addr:0x3fca1b14:u8 != 0`: the `var` trigger class and page watch
/// without the DWARF lookup. UNVERIFIED: whether it stays or folds into `var:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddrMatcher {
    pub addr: u32,
    /// `u32` when the matcher names none.
    pub width: Width,
    pub test: ValueTest,
}

/// `reg:UART0.STATUS == 0`, or `reg:UART0.STATUS.txfifo_cnt == 0` for one field. UNVERIFIED: this
/// module only compiles and classifies the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegMatcher {
    /// The `specs/blocks/<block>.toml` name.
    pub block: Box<str>,
    pub reg: Box<str>,
    /// `None` tests the whole register.
    pub field: Option<Box<str>>,
    pub test: ValueTest,
}

/// `vt:+500ms` relative to the start of the run, or `vt:1.5s` absolute.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimeMatcher {
    pub relative: bool,
    pub at: VTime,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Settle {
    None,
    /// Continue to the next LVGL safe point, so a following `ui` call is consistent.
    Ui,
}

/// Build one with [`Matcher::parse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Matcher {
    Serial(SerialMatcher),
    Log(LogMatcher),
    Ui(UiMatcher),
    Event(EventKind),
    Symbol(SymbolMatcher),
    Var(VarMatcher),
    /// UNVERIFIED; see [`AddrMatcher`].
    Addr(AddrMatcher),
    /// UNVERIFIED; see [`RegMatcher`].
    Reg(RegMatcher),
    Time(TimeMatcher),
    /// The first child to fire.
    Any(Box<[Matcher]>),
    /// Every child has fired, in any order.
    All(Box<[Matcher]>),
    /// Every child has fired, in this order.
    Seq(Box<[Matcher]>),
}

impl Matcher {
    /// Compiles one matcher:
    ///
    /// ```text
    /// matcher   := "any" "(" list ")" | "all" "(" list ")" | "seq" "(" list ")" | leaf
    /// leaf      := "serial:" pattern opts | "log:" tag ":" level ":" pattern
    ///            | "ui:changed" | "ui:" attr ("=" | "~") string
    ///            | "event:" kind | "symbol:" (name | hex) [":hits=" n]
    ///            | "var:" name (cmp value | "changed")
    ///            | "addr:" hex [":" width] (cmp value | "changed")
    ///            | "reg:" block "." reg ["." field] (cmp value | "changed")
    ///            | "vt:" ["+"] duration
    /// pattern   := "/" body "/" | "\"" text "\"" | "~\"" text "\""
    /// opts      := ("," ("stream" | "line" | "usj" | "uart0" | "any"
    ///                   | "from=" ("cursor" | "now" | "start") | "channel=" channel))*
    /// ```
    ///
    /// Each `opts` axis (mode, channel, `from`) appears at most once. Anything else is `E_USAGE`.
    pub fn parse(text: &str) -> Result<Matcher, ApiError> {
        parse_matcher(text, 0)
    }

    /// `None` for a composite, which fires on any child's trigger.
    pub fn trigger_class(&self) -> Option<TriggerClass> {
        Some(match self {
            Matcher::Serial(m) => match m.mode {
                ConsoleMode::Line => TriggerClass::ConsoleLine,
                ConsoleMode::Stream => TriggerClass::ConsoleStream,
            },
            Matcher::Log(_) => TriggerClass::ConsoleLine,
            Matcher::Ui(_) => TriggerClass::UiGeneration,
            Matcher::Event(_) => TriggerClass::EventRing,
            Matcher::Symbol(_) => TriggerClass::ObserveHook,
            Matcher::Var(_) | Matcher::Addr(_) => TriggerClass::DataWrite,
            Matcher::Reg(_) => TriggerClass::MmioWrite,
            Matcher::Time(_) => TriggerClass::Deadline,
            Matcher::Any(_) | Matcher::All(_) | Matcher::Seq(_) => return None,
        })
    }

    /// So the run loop arms exactly those hooks.
    pub fn trigger_classes(&self) -> BTreeSet<TriggerClass> {
        let mut out = BTreeSet::new();
        self.collect_classes(&mut out);
        out
    }

    fn collect_classes(&self, out: &mut BTreeSet<TriggerClass>) {
        match self {
            Matcher::Any(c) | Matcher::All(c) | Matcher::Seq(c) => {
                for child in c.iter() {
                    child.collect_classes(out);
                }
            }
            leaf => {
                if let Some(class) = leaf.trigger_class() {
                    out.insert(class);
                }
            }
        }
    }

    /// `ui` for a `ui:*` matcher, so a following `ui` call is consistent. A composite settles when
    /// any child is a UI matcher.
    pub fn default_settle(&self) -> Settle {
        if self.trigger_classes().contains(&TriggerClass::UiGeneration) {
            Settle::Ui
        } else {
            Settle::None
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            Matcher::Any(c) | Matcher::All(c) | Matcher::Seq(c) => {
                1 + c.iter().map(Matcher::depth).max().unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// The count the run loop arms.
    pub fn leaf_count(&self) -> usize {
        match self {
            Matcher::Any(c) | Matcher::All(c) | Matcher::Seq(c) => {
                c.iter().map(Matcher::leaf_count).sum()
            }
            _ => 1,
        }
    }
}

const LEAF_KINDS: &[&str] = &[
    "serial", "log", "ui", "event", "symbol", "var", "addr", "reg", "vt",
];

fn parse_matcher(text: &str, depth: usize) -> Result<Matcher, ApiError> {
    let s = text.trim();
    if s.is_empty() {
        return Err(usage("empty matcher"));
    }
    if depth > MAX_DEPTH {
        return Err(usage(format!(
            "matcher nests `any`, `all` or `seq` deeper than {MAX_DEPTH} levels"
        )));
    }
    for kw in ["any", "all", "seq"] {
        let Some(rest) = s.strip_prefix(kw) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(inner) = rest.strip_prefix('(') else {
            continue;
        };
        let inner = inner
            .strip_suffix(')')
            .ok_or_else(|| usage(format!("`{kw}(` is not closed by a final `)`")))?;
        if inner.trim().is_empty() {
            return Err(usage(format!("`{kw}()` needs at least one child matcher")));
        }
        let parts = split_children(inner)?;
        if parts.len() > MAX_CHILDREN {
            return Err(usage(format!(
                "`{kw}` takes at most {MAX_CHILDREN} children, got {}",
                parts.len()
            )));
        }
        let mut children = Vec::with_capacity(parts.len());
        for part in parts {
            children.push(parse_matcher(part, depth + 1)?);
        }
        let children = children.into_boxed_slice();
        return Ok(match kw {
            "any" => Matcher::Any(children),
            "all" => Matcher::All(children),
            _ => Matcher::Seq(children),
        });
    }
    let (kind, rest) = s.split_once(':').ok_or_else(|| {
        usage(format!(
            "`{s}` is not a matcher: a leaf starts with `{}:` and a composite is \
             `any(...)`, `all(...)` or `seq(...)`",
            LEAF_KINDS.join(":`, `")
        ))
    })?;
    match kind.trim() {
        "serial" => parse_serial(rest),
        "log" => parse_log(rest),
        "ui" => parse_ui(rest),
        "event" => parse_event(rest),
        "symbol" => parse_symbol(rest),
        "var" => parse_var(rest),
        "addr" => parse_addr(rest),
        "reg" => parse_reg(rest),
        "vt" => parse_time(rest),
        other => Err(usage(format!(
            "unknown matcher kind `{other}`: the kinds are `{}`",
            LEAF_KINDS.join("`, `")
        ))),
    }
}

/// Splits on top-level commas, stepping over `(...)`, `/.../` and `"..."`.
fn split_children(inner: &str) -> Result<Vec<&str>, ApiError> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_slash = false;
    let mut in_quote = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (idx, c) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_slash || in_quote => escaped = true,
            '/' if !in_quote => in_slash = !in_slash,
            '"' if !in_slash => in_quote = !in_quote,
            '(' if !in_slash && !in_quote => depth += 1,
            ')' if !in_slash && !in_quote => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| usage("unbalanced `)` in a composite matcher"))?;
            }
            ',' if depth == 0 && !in_slash && !in_quote => {
                out.push(&inner[start..idx]);
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(usage("unbalanced `(` in a composite matcher"));
    }
    if in_slash || in_quote {
        return Err(usage(
            "a pattern is not closed: `/` needs a `/` and `\"` needs a `\"`",
        ));
    }
    out.push(&inner[start..]);
    Ok(out)
}

fn take_pattern(s: &str) -> Result<(TextPattern, &str), ApiError> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('/') {
        let (body, rest) = take_raw_until(rest, '/')?;
        return Ok((TextPattern::compile(&body)?, rest));
    }
    if let Some(rest) = s.strip_prefix('~') {
        let rest = rest.trim_start().strip_prefix('"').ok_or_else(|| {
            usage("`~` compares against a double-quoted literal, as in `ui:focused~\"Display\"`")
        })?;
        let (body, rest) = take_quoted(rest)?;
        return Ok((TextPattern::literal_contains(&body), rest));
    }
    if let Some(rest) = s.strip_prefix('"') {
        let (body, rest) = take_quoted(rest)?;
        return Ok((TextPattern::literal_exact(&body), rest));
    }
    Err(usage(
        "a pattern is written between slashes (`/ready/`), or in double quotes for an exact \
         literal (`\"menu ready\"`)",
    ))
}

/// Keeps backslash escapes for the pattern lexer.
fn take_raw_until(s: &str, delim: char) -> Result<(String, &str), ApiError> {
    let mut body = String::new();
    let mut chars = s.char_indices();
    while let Some((idx, c)) = chars.next() {
        if c == '\\' {
            let (_, next) = chars
                .next()
                .ok_or_else(|| usage("pattern ends with a lone `\\`"))?;
            body.push('\\');
            body.push(next);
            continue;
        }
        if c == delim {
            return Ok((body, &s[idx + c.len_utf8()..]));
        }
        body.push(c);
    }
    Err(usage(format!("pattern is not closed by `{delim}`")))
}

/// Decodes `\"` and `\\`.
fn take_quoted(s: &str) -> Result<(String, &str), ApiError> {
    let mut body = String::new();
    let mut chars = s.char_indices();
    while let Some((idx, c)) = chars.next() {
        if c == '\\' {
            let (_, next) = chars
                .next()
                .ok_or_else(|| usage("quoted text ends with a lone `\\`"))?;
            if !matches!(next, '"' | '\\') {
                return Err(usage(format!(
                    "unsupported escape `\\{next}` in quoted text: only `\\\"` and `\\\\` are escapes"
                )));
            }
            body.push(next);
            continue;
        }
        if c == '"' {
            return Ok((body, &s[idx + c.len_utf8()..]));
        }
        body.push(c);
    }
    Err(usage("quoted text is not closed by `\"`"))
}

fn split_options(rest: &str) -> Result<Vec<&str>, ApiError> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let rest = rest.strip_prefix(',').ok_or_else(|| {
        usage(format!(
            "unexpected text after the pattern: `{rest}`; each option follows a comma"
        ))
    })?;
    Ok(rest.split(',').map(str::trim).collect())
}

fn parse_serial(rest: &str) -> Result<Matcher, ApiError> {
    let (pattern, tail) = take_pattern(rest)?;
    // The USB Serial/JTAG console is the one a call means when it names none.
    let mut m = SerialMatcher {
        channel: Channel::Usj,
        mode: ConsoleMode::Line,
        from: From::Cursor,
        pattern,
    };
    // The mode picks the trigger class, so taking the last one would silently arm a different hook.
    let (mut mode_set, mut channel_set, mut from_set) = (false, false, false);
    for opt in split_options(tail)? {
        if let Some(v) = opt.strip_prefix("from=") {
            set_once(&mut from_set, "from")?;
            m.from = From::parse(v).ok_or_else(|| {
                usage(format!("`from={v}` is not one of: {}", From::vocabulary()))
            })?;
        } else if let Some(v) = opt.strip_prefix("channel=") {
            set_once(&mut channel_set, "channel")?;
            m.channel = Channel::parse(v).ok_or_else(|| {
                usage(format!(
                    "`channel={v}` is not one of: {}",
                    Channel::vocabulary()
                ))
            })?;
        } else if let Some(mode) = ConsoleMode::parse(opt) {
            set_once(&mut mode_set, "mode")?;
            m.mode = mode;
        } else if let Some(channel) = Channel::parse(opt) {
            set_once(&mut channel_set, "channel")?;
            m.channel = channel;
        } else {
            return Err(usage(format!(
                "unknown serial option `{opt}`: use `stream`, `line`, a channel ({}), \
                 `from=<{}>` or `channel=<channel>`",
                Channel::vocabulary(),
                From::vocabulary()
            )));
        }
    }
    Ok(Matcher::Serial(m))
}

fn set_once(seen: &mut bool, axis: &str) -> Result<(), ApiError> {
    if *seen {
        return Err(usage(format!(
            "the {axis} of a serial matcher is named twice: `mode` (`line` or `stream`), \
             `channel` and `from` each appear at most once"
        )));
    }
    *seen = true;
    Ok(())
}

fn parse_log(rest: &str) -> Result<Matcher, ApiError> {
    let (tag, rest) = rest.trim_start().split_once(':').ok_or_else(|| {
        usage(
            "a log matcher is `log:<tag>:<level>:<pattern>`, as in `log:pk_app:I:/ready/`; \
               write `*` for any tag or level",
        )
    })?;
    let (level, rest) = rest.split_once(':').ok_or_else(|| {
        usage("a log matcher is `log:<tag>:<level>:<pattern>`, as in `log:pk_app:I:/ready/`")
    })?;
    let tag = tag.trim();
    let level = level.trim();
    let tag = match tag {
        "*" | "" => None,
        t if t.bytes().all(|b| b.is_ascii_graphic() && b != b':') => Some(Box::from(t)),
        _ => {
            return Err(usage(format!(
                "`{tag}` is not an ESP log tag: printable ASCII without `:`, or `*` for any"
            )));
        }
    };
    let level = match level {
        "*" | "" => None,
        l => Some(LogLevel::parse(l).ok_or_else(|| {
            usage(format!(
                "`{l}` is not an ESP log level: {}, or `*` for any",
                LogLevel::vocabulary()
            ))
        })?),
    };
    let (pattern, tail) = take_pattern(rest)?;
    reject_tail(tail, "log")?;
    Ok(Matcher::Log(LogMatcher {
        tag,
        level,
        pattern,
    }))
}

fn parse_ui(rest: &str) -> Result<Matcher, ApiError> {
    let rest = rest.trim();
    if rest == "changed" {
        return Ok(Matcher::Ui(UiMatcher::Changed));
    }
    let op = rest.find(['=', '~']).ok_or_else(|| {
        usage(
            "a ui matcher is `ui:changed`, `ui:<attr>=\"text\"` (exact) or `ui:<attr>~\"text\"` \
             (contains), as in `ui:label=\"Button\"`",
        )
    })?;
    let attr = rest[..op].trim();
    let attr = UiAttr::parse(attr).ok_or_else(|| {
        usage(format!(
            "`{attr}` is not a ui attribute: {}",
            UiAttr::vocabulary()
        ))
    })?;
    let value = &rest[op..];
    let (pattern, tail) = match value.as_bytes()[0] {
        b'=' => {
            let inner = value[1..].trim_start().strip_prefix('"').ok_or_else(|| {
                usage("`=` compares against a double-quoted literal, as in `ui:label=\"Button\"`")
            })?;
            let (text, tail) = take_quoted(inner)?;
            (TextPattern::literal_exact(&text), tail)
        }
        _ => take_pattern(value)?,
    };
    reject_tail(tail, "ui")?;
    Ok(Matcher::Ui(UiMatcher::Query { attr, pattern }))
}

fn parse_event(rest: &str) -> Result<Matcher, ApiError> {
    let name = rest.trim();
    EventKind::parse(name).map(Matcher::Event).ok_or_else(|| {
        usage(format!(
            "`{name}` is not an event: {}",
            EventKind::vocabulary()
        ))
    })
}

fn parse_symbol(rest: &str) -> Result<Matcher, ApiError> {
    let rest = rest.trim();
    let (head, hits) = match rest.split_once(':') {
        Some((head, tail)) => {
            let n = tail.trim().strip_prefix("hits=").ok_or_else(|| {
                usage(
                    "the only symbol option is `hits=<n>`, as in `symbol:lv_timer_handler:hits=3`",
                )
            })?;
            let n: u32 = n
                .trim()
                .parse()
                .map_err(|_| usage(format!("`hits={n}` is not a positive whole number")))?;
            if n == 0 {
                return Err(usage("`hits=0` never fires; the smallest hit count is 1"));
            }
            (head, n)
        }
        None => (rest, 1),
    };
    let head = head.trim();
    let target = if head.starts_with("0x") || head.starts_with("0X") {
        SymbolTarget::Addr(parse_hex_u32(head)?)
    } else {
        if head.is_empty() || !head.bytes().all(is_symbol_byte) {
            return Err(usage(format!(
                "`{head}` is not a symbol name: ASCII letters, digits, `_`, `.` and `$`, or a \
                 `0x` address"
            )));
        }
        SymbolTarget::Name(head.into())
    };
    Ok(Matcher::Symbol(SymbolMatcher { target, hits }))
}

fn is_symbol_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$')
}

fn parse_var(rest: &str) -> Result<Matcher, ApiError> {
    let (head, test) = split_test(rest)?;
    if head.is_empty()
        || !head
            .bytes()
            .all(|b| is_symbol_byte(b) || matches!(b, b':' | b'[' | b']'))
    {
        return Err(usage(format!(
            "`{head}` is not a global: a name, optionally with a compilation unit \
             (`main.c::s_sel`) or an index (`s_ok[2]`)"
        )));
    }
    Ok(Matcher::Var(VarMatcher {
        name: head.into(),
        test,
    }))
}

fn parse_addr(rest: &str) -> Result<Matcher, ApiError> {
    let (head, test) = split_test(rest)?;
    let (addr, width) = match head.split_once(':') {
        Some((a, w)) => (
            a.trim(),
            Width::parse(w.trim()).ok_or_else(|| {
                usage(format!(
                    "`{w}` is not an address width: {}",
                    Width::vocabulary()
                ))
            })?,
        ),
        None => (head, Width::U32),
    };
    Ok(Matcher::Addr(AddrMatcher {
        addr: parse_hex_u32(addr)?,
        width,
        test,
    }))
}

fn parse_reg(rest: &str) -> Result<Matcher, ApiError> {
    let (head, test) = split_test(rest)?;
    let parts: Vec<&str> = head.split('.').collect();
    let ok = matches!(parts.len(), 2 | 3)
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    if !ok {
        return Err(usage(format!(
            "`{head}` is not a register path: `<BLOCK>.<REG>` or `<BLOCK>.<REG>.<FIELD>`, as in \
             `reg:UART0.STATUS.txfifo_cnt`"
        )));
    }
    Ok(Matcher::Reg(RegMatcher {
        block: parts[0].into(),
        reg: parts[1].into(),
        field: parts.get(2).map(|f| Box::from(*f)),
        test,
    }))
}

fn parse_time(rest: &str) -> Result<Matcher, ApiError> {
    let rest = rest.trim();
    let (relative, body) = match rest.strip_prefix('+') {
        Some(body) => (true, body),
        None => (false, rest),
    };
    Ok(Matcher::Time(TimeMatcher {
        relative,
        at: parse_duration(body)?,
    }))
}

/// The head may hold `::`, and may end in the letters `changed`, so the keyword counts only after
/// whitespace.
fn split_test(rest: &str) -> Result<(&str, ValueTest), ApiError> {
    let s = rest.trim();
    for (i, _) in s.char_indices() {
        let tail = &s[i..];
        if tail.starts_with("::") {
            continue;
        }
        for op in CmpOp::ALL {
            if tail.starts_with(op.as_str()) {
                let head = s[..i].trim();
                let value = tail[op.as_str().len()..].trim();
                if value.is_empty() {
                    return Err(usage(format!(
                        "`{op_str}` has no value on its right",
                        op_str = op.as_str()
                    )));
                }
                return Ok((head, ValueTest::Cmp(*op, parse_value(value)?)));
            }
        }
    }
    // Without the boundary, `var:s_changed` would silently watch a global named `s_`.
    if let Some(head) = s.strip_suffix("changed")
        && head.ends_with([' ', '\t'])
    {
        let head = head.trim_end();
        if !head.is_empty() {
            return Ok((head, ValueTest::Changed));
        }
    }
    Err(usage(format!(
        "`{s}` needs a test: a comparison ({}) and a value, or the keyword `changed` separated \
         from the name by a space, as in `var:s_sel changed`",
        CmpOp::vocabulary()
    )))
}

fn parse_value(text: &str) -> Result<Value, ApiError> {
    match text {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if let Some(inner) = text.strip_prefix('"') {
        let (body, tail) = take_quoted(inner)?;
        reject_tail(tail, "value")?;
        return Ok(Value::Text(body.into_boxed_str()));
    }
    let (neg, digits) = match text.strip_prefix('-') {
        Some(d) => (true, d.trim_start()),
        None => (false, text),
    };
    let magnitude: i64 = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i64::from(parse_hex_u32_digits(hex)?)
    } else {
        digits
            .parse::<i64>()
            .map_err(|_| usage(format!("`{text}` is not a value: a whole number, `0x` hexadecimal, `true`, `false` or a double-quoted string")))?
    };
    Ok(Value::Int(if neg { -magnitude } else { magnitude }))
}

/// `0x` followed by 1 to 8 hexadecimal digits.
fn parse_hex_u32(text: &str) -> Result<u32, ApiError> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .ok_or_else(|| {
            usage(format!(
                "`{text}` is not an address: write it as `0x` hexadecimal"
            ))
        })?;
    parse_hex_u32_digits(digits)
}

fn parse_hex_u32_digits(digits: &str) -> Result<u32, ApiError> {
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(usage(format!(
            "`0x{digits}` is not a 32-bit address: 1 to 8 hexadecimal digits"
        )));
    }
    u32::from_str_radix(digits, 16)
        .map_err(|_| usage(format!("`0x{digits}` is not a 32-bit address")))
}

/// `800us`, `250ms`, `1.5s`, `2m`, or a bare integer in milliseconds, truncated toward zero at
/// picoseconds. Exactly the `Duration` schema spellings, so the CLI accepts nothing MCP would
/// refuse. A duration past [`VTime`] (about 213 days) is `E_USAGE`, so two matchers never silently
/// share a deadline.
pub fn parse_duration(text: &str) -> Result<VTime, ApiError> {
    const PS_PER_US: u128 = 1_000_000;
    let s = text.trim();
    if s.is_empty() {
        return Err(usage(
            "empty duration: write `250ms`, `1.5s`, `2m` or `800us`",
        ));
    }
    let (number, ps_per_unit) = if let Some(n) = s.strip_suffix("us") {
        (n, PS_PER_US)
    } else if let Some(n) = s.strip_suffix("ms") {
        (n, PS_PER_US * 1_000)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, PS_PER_US * 1_000_000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, PS_PER_US * 60_000_000)
    } else {
        (s, PS_PER_US * 1_000)
    };
    let number = number.trim();
    let (int_text, frac_text, dotted) = match number.split_once('.') {
        Some((i, f)) => (i, f, true),
        None => (number, "", false),
    };
    let bad = || {
        usage(format!(
            "`{text}` is not a duration: a whole or decimal number with `us`, `ms`, `s` or `m`, \
             or a bare integer in milliseconds"
        ))
    };
    // A digit is required on each side of the `.`, as in the schema.
    if int_text.is_empty()
        || (dotted && frac_text.is_empty())
        || int_text.len() > 20
        || frac_text.len() > 12
        || !int_text.bytes().all(|b| b.is_ascii_digit())
        || !frac_text.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let whole: u128 = int_text.parse().map_err(|_| bad())?;
    let mut ps = whole.saturating_mul(ps_per_unit);
    if !frac_text.is_empty() {
        let frac: u128 = frac_text.parse().map_err(|_| bad())?;
        let scale = 10u128.pow(frac_text.len() as u32);
        ps = ps.saturating_add(frac.saturating_mul(ps_per_unit) / scale);
    }
    // Saturating would collapse every long duration onto the same deadline.
    let ps = u64::try_from(ps).map_err(|_| {
        usage(format!(
            "`{text}` is longer than the virtual clock can represent: virtual time is 64-bit \
             picoseconds, so the largest duration is {MAX_DURATION_US} us, about 213 days"
        ))
    })?;
    Ok(VTime(ps))
}

/// `u64::MAX` picoseconds is about 213 days.
const MAX_DURATION_US: u64 = u64::MAX / 1_000_000;

fn reject_tail(tail: &str, what: &str) -> Result<(), ApiError> {
    if tail.trim().is_empty() {
        Ok(())
    } else {
        Err(usage(format!(
            "unexpected text after a {what} matcher: `{}`",
            tail.trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Matcher {
        Matcher::parse(text).unwrap_or_else(|e| panic!("`{text}` should compile: {}", e.message))
    }

    fn refused(text: &str) -> ApiError {
        match Matcher::parse(text) {
            Ok(m) => panic!("`{text}` should be refused, compiled to {m:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn every_grammar_form_compiles_to_its_documented_trigger_class() {
        let rows: &[(&str, TriggerClass)] = &[
            ("serial:/pk_app: ready/", TriggerClass::ConsoleLine),
            ("serial:/pk_app: ready/,stream", TriggerClass::ConsoleStream),
            ("log:pk_app:I:/ready/", TriggerClass::ConsoleLine),
            ("ui:label=\"Button\"", TriggerClass::UiGeneration),
            ("ui:focused~\"Display\"", TriggerClass::UiGeneration),
            ("ui:changed", TriggerClass::UiGeneration),
            ("event:panic", TriggerClass::EventRing),
            ("event:usb_open", TriggerClass::EventRing),
            ("event:wifi_got_ip", TriggerClass::EventRing),
            ("symbol:lv_timer_handler:hits=3", TriggerClass::ObserveHook),
            ("symbol:0x40047e9e", TriggerClass::ObserveHook),
            ("var:s_sel == 1", TriggerClass::DataWrite),
            ("addr:0x3fca1b14:u8 != 0", TriggerClass::DataWrite),
            ("reg:UART0.STATUS.txfifo_cnt == 0", TriggerClass::MmioWrite),
            ("vt:+500ms", TriggerClass::Deadline),
        ];
        for (text, class) in rows {
            assert_eq!(ok(text).trigger_class(), Some(*class), "{text}");
        }
        let composite = ok("any(serial:/a/,event:reset)");
        assert_eq!(composite.trigger_class(), None);
        assert_eq!(
            composite.trigger_classes(),
            BTreeSet::from([TriggerClass::ConsoleLine, TriggerClass::EventRing])
        );
        assert_eq!(ok("all(vt:+1s,ui:changed)").leaf_count(), 2);
        assert_eq!(ok("seq(event:reset,event:panic)").depth(), 1);
    }

    #[test]
    fn pattern_bodies_compile_to_the_documented_text_class() {
        let rows: &[(&str, TextClass)] = &[
            ("/ready/", TextClass::Literal),
            ("/^I (/", TextClass::Prefix),
            ("/ done$/", TextClass::Suffix),
            ("/^menu ready$/", TextClass::Exact),
            ("\"menu ready\"", TextClass::Exact),
            ("~\"menu\"", TextClass::Literal),
            ("/^?? ready$/", TextClass::Glob),
            ("/*boot/", TextClass::Glob),
            ("/2.6 ready/", TextClass::Literal),
            ("/100\\* done/", TextClass::Literal),
        ];
        for (text, class) in rows {
            let Matcher::Serial(m) = ok(&format!("serial:{text}")) else {
                panic!("`serial:{text}` should be a serial matcher");
            };
            assert_eq!(m.pattern.class(), *class, "serial:{text}");
        }
    }

    #[test]
    fn text_classes_match_what_they_document() {
        let literal = TextPattern::compile("ready").unwrap();
        assert!(literal.matches("I (312) app: ready now"));
        assert!(!literal.matches("I (312) app: read"));

        let prefix = TextPattern::compile("^I (").unwrap();
        assert!(prefix.matches("I (312) app"));
        assert!(!prefix.matches(" I (312) app"));

        let suffix = TextPattern::compile(" done$").unwrap();
        assert!(suffix.matches("boot done"));
        assert!(!suffix.matches("boot done "));

        let exact = TextPattern::compile("^menu ready$").unwrap();
        assert!(exact.matches("menu ready"));
        assert!(!exact.matches("menu ready!"));

        let escaped = TextPattern::compile("100\\* done").unwrap();
        assert_eq!(escaped.class(), TextClass::Literal);
        assert!(escaped.matches("at 100* done"));
        assert!(!escaped.matches("at 1000 done"));
    }

    #[test]
    fn globs_match_without_a_regular_expression_engine() {
        // The only position RE2 refuses outright.
        let g = TextPattern::compile("^*pk_app: ready$").unwrap();
        assert_eq!(g.class(), TextClass::Glob);
        assert!(g.matches("pk_app: ready"));
        assert!(g.matches("I (312) pk_app: ready"));
        assert!(!g.matches("pk_app: ready now"));

        let g = TextPattern::compile("?boot").unwrap();
        assert!(g.matches("I (99) xboot: all done here"));
        assert!(!g.matches("boot first"));

        let g = TextPattern::compile("^??b$").unwrap();
        assert!(g.matches("aüb"));
        assert!(!g.matches("ab"));
        assert!(!g.matches("abcb"));

        let g = TextPattern::compile("^*?*ab$").unwrap();
        assert!(g.matches("zzabzzab"));
        assert!(g.matches("xab"));
        assert!(!g.matches("ab"), "at least one character has to precede");
        assert!(!g.matches("zzabzz"));
    }

    #[test]
    fn the_glob_walk_backtracks_over_multi_byte_characters() {
        let g = TextPattern::compile("*ünd").unwrap();
        assert!(g.matches("ü ü ünd"));
        assert!(g.matches("ünd"));
        assert!(!g.matches("ü ü un d"));

        let g = TextPattern::compile("^*?ünd$").unwrap();
        assert!(g.matches("xünd"));
        assert!(g.matches("üüünd"));
        assert!(!g.matches("ünd"));

        // Exhaustive against the same globs read character by character.
        let unanchored = TextPattern::compile("??ü").unwrap();
        let anchored = TextPattern::compile("^*??ü$").unwrap();
        for n in 0..=6u32 {
            for bits in 0..(1u32 << n) {
                let subject: String = (0..n)
                    .map(|k| if bits >> k & 1 == 0 { 'a' } else { 'ü' })
                    .collect();
                let chars: Vec<char> = subject.chars().collect();
                let want = (2..chars.len()).any(|s| chars[s] == 'ü');
                assert_eq!(unanchored.matches(&subject), want, "`??ü` on {subject:?}");
                let want = chars.len() >= 3 && chars[chars.len() - 1] == 'ü';
                assert_eq!(anchored.matches(&subject), want, "`^*??ü$` on {subject:?}");
            }
        }
    }

    /// `ready*` is "read" plus zero or more `y` to RE2 and "ready" plus anything to a glob.
    #[test]
    fn a_wildcard_a_regular_expression_would_read_differently_is_refused() {
        for text in [
            "serial:/ready*/",
            "serial:/ab?c/",
            "serial:/^ready*$/",
            "serial:/100\\.*/",
            "log:pk_app:I:/ready?/",
            "any(serial:/ready*/,event:reset)",
        ] {
            let e = refused(text);
            assert_eq!(e.code, E_USAGE, "{text}");
            assert!(e.hint.is_some(), "`{text}` should carry a hint");
        }
        assert_eq!(
            TextPattern::compile("ready\\*").unwrap(),
            TextPattern::Literal("ready*".into())
        );
        assert_eq!(
            TextPattern::compile("^ready").unwrap(),
            TextPattern::Prefix("ready".into())
        );
        assert_eq!(
            TextPattern::compile("*ready").unwrap().class(),
            TextClass::Glob
        );
    }

    #[test]
    fn an_unsupported_pattern_is_refused_with_e_usage() {
        let bad = [
            "serial:/(a|b)/",         // alternation
            "serial:/a+/",            // repetition
            "serial:/[0-9]/",         // character class
            "serial:/a{2}/",          // counted repetition
            "serial:/.*ready/",       // regex any-run
            "serial:/x.?/",           // regex optional
            "serial:/\\d+/",          // class escape
            "serial:/\\w/",           // class escape
            "serial:/a\\zb/",         // escape of an ordinary character
            "serial:/a^b/",           // interior anchor
            "serial:/a$b/",           // interior anchor
            "serial:/ready",          // unterminated
            "serial://",              // empty body
            "serial:ready",           // undelimited
            "serial:/a/,mode=stream", // unknown option
            "serial:/a/,from=later",  // unknown `from`
            "log:pk_app:X:/a/",       // unknown level
            "log:pk_app:/a/",         // missing level field
            "ui:title=\"x\"",         // unknown attribute
            "event:exploded",         // unknown event
            "symbol:lv_timer:hits=0", // a hit count of 0 never fires
            "symbol:has space",       // not a symbol name
            "var:s_sel",              // no test
            "var:s_sel ~= 1",         // not a comparison
            "var:s_changed",
            "addr:0x10changed",
            "reg:UART0.STATUSchanged",
            "addr:3fca1b14 == 1",     // address without `0x`
            "addr:0x1234567890 == 1", // more than 32 bits
            "reg:UART0 == 1",         // path too short
            "vt:+later",              // not a duration
            "vt:+1.5x",               // unknown unit
            "nope:1",                 // unknown kind
            "serial",                 // no `:`
            "any()",                  // empty composite
            "any(event:reset",        // unclosed composite
            "",                       // empty
        ];
        for text in bad {
            let e = refused(text);
            assert_eq!(e.code, E_USAGE, "`{text}` refused with {:?}", e.code);
            assert!(e.hint.is_some(), "`{text}` should carry a hint");
        }
    }

    #[test]
    fn composites_are_bounded_in_depth_and_width() {
        let deep =
            (0..=MAX_DEPTH).fold(String::from("event:reset"), |acc, _| format!("any({acc})"));
        assert_eq!(refused(&deep).code, E_USAGE);
        let shallow =
            (0..MAX_DEPTH).fold(String::from("event:reset"), |acc, _| format!("any({acc})"));
        assert_eq!(ok(&shallow).depth(), MAX_DEPTH);

        let children: Vec<String> = (0..=MAX_CHILDREN)
            .map(|_| "event:reset".to_string())
            .collect();
        assert_eq!(
            refused(&format!("all({})", children.join(","))).code,
            E_USAGE
        );
    }

    #[test]
    fn a_pattern_may_hold_a_comma_or_a_parenthesis_inside_a_composite() {
        let m = ok("any(serial:/ready, set, go/,event:reset)");
        let Matcher::Any(children) = &m else {
            panic!("expected any()");
        };
        assert_eq!(children.len(), 2);
        let Matcher::Serial(s) = &children[0] else {
            panic!("expected a serial matcher");
        };
        assert_eq!(s.pattern, TextPattern::Literal("ready, set, go".into()));
        assert_eq!(children[1], Matcher::Event(EventKind::Reset));
    }

    #[test]
    fn serial_options_parse_channel_and_mode() {
        let Matcher::Serial(m) = ok("serial:/a/") else {
            panic!("expected a serial matcher");
        };
        assert_eq!(m.channel, Channel::Usj);
        assert_eq!(m.mode, ConsoleMode::Line);
        assert_eq!(m.from, From::Cursor);

        let Matcher::Serial(m) = ok("serial:/a/,stream,uart0,from=start") else {
            panic!("expected a serial matcher");
        };
        assert_eq!(m.channel, Channel::Uart0);
        assert_eq!(m.mode, ConsoleMode::Stream);
        assert_eq!(m.from, From::Start);
        assert_eq!(
            Matcher::Serial(m).trigger_class(),
            Some(TriggerClass::ConsoleStream)
        );
    }

    #[test]
    fn a_serial_option_axis_named_twice_is_refused() {
        for text in [
            "serial:/a/,line,stream",
            "serial:/a/,stream,line",
            "serial:/a/,usj,uart0",
            "serial:/a/,uart0,channel=usj",
            "serial:/a/,channel=usj,usj",
            "serial:/a/,from=now,from=start",
            "serial:/a/,line,line",
        ] {
            let e = refused(text);
            assert_eq!(e.code, E_USAGE, "{text}");
            assert!(e.message.contains("twice"), "{}", e.message);
        }
        ok("serial:/a/,from=start,uart0,stream");
    }

    #[test]
    fn log_tag_and_level_accept_the_any_wildcard() {
        let Matcher::Log(m) = ok("log:pk_app:I:/ready/") else {
            panic!("expected a log matcher");
        };
        assert_eq!(m.tag.as_deref(), Some("pk_app"));
        assert_eq!(m.level, Some(LogLevel::Info));

        let Matcher::Log(m) = ok("log:*:*:/ready/") else {
            panic!("expected a log matcher");
        };
        assert_eq!(m.tag, None);
        assert_eq!(m.level, None);
    }

    #[test]
    fn value_tests_cover_the_var_row() {
        assert_eq!(
            ok("var:s_sel == 1"),
            Matcher::Var(VarMatcher {
                name: "s_sel".into(),
                test: ValueTest::Cmp(CmpOp::Eq, Value::Int(1)),
            })
        );
        assert_eq!(
            ok("var:main.c::s_sel >= 0x10"),
            Matcher::Var(VarMatcher {
                name: "main.c::s_sel".into(),
                test: ValueTest::Cmp(CmpOp::Ge, Value::Int(16)),
            })
        );
        assert_eq!(
            ok("var:s_ok[2] changed"),
            Matcher::Var(VarMatcher {
                name: "s_ok[2]".into(),
                test: ValueTest::Changed,
            })
        );
        assert_eq!(
            ok("var:s_flag != true"),
            Matcher::Var(VarMatcher {
                name: "s_flag".into(),
                test: ValueTest::Cmp(CmpOp::Ne, Value::Bool(true)),
            })
        );
        assert_eq!(
            ok("var:s_name == \"menu\""),
            Matcher::Var(VarMatcher {
                name: "s_name".into(),
                test: ValueTest::Cmp(CmpOp::Eq, Value::Text("menu".into())),
            })
        );
        let Matcher::Var(m) = ok("var:s_sel <= 3") else {
            panic!("expected a var matcher");
        };
        assert_eq!(m.test, ValueTest::Cmp(CmpOp::Le, Value::Int(3)));
    }

    #[test]
    fn the_changed_keyword_needs_a_word_boundary() {
        assert_eq!(
            ok("var:s_changed changed"),
            Matcher::Var(VarMatcher {
                name: "s_changed".into(),
                test: ValueTest::Changed,
            })
        );
        assert_eq!(
            ok("var:s_changed == 1"),
            Matcher::Var(VarMatcher {
                name: "s_changed".into(),
                test: ValueTest::Cmp(CmpOp::Eq, Value::Int(1)),
            })
        );
        let Matcher::Reg(m) = ok("reg:UART0.STATUSchanged changed") else {
            panic!("expected a reg matcher");
        };
        assert_eq!(&*m.reg, "STATUSchanged");
        for text in [
            "var:s_changed",
            "addr:0x10changed",
            "reg:UART0.STATUSchanged",
        ] {
            let e = refused(text);
            assert_eq!(e.code, E_USAGE, "{text}");
            assert!(e.message.contains("needs a test"), "{}", e.message);
        }
    }

    #[test]
    fn durations_parse_to_the_documented_virtual_time() {
        assert_eq!(parse_duration("800us").unwrap(), VTime::from_us(800));
        assert_eq!(parse_duration("250ms").unwrap(), VTime::from_ms(250));
        assert_eq!(parse_duration("1.5s").unwrap(), VTime::from_ms(1_500));
        assert_eq!(parse_duration("2m").unwrap(), VTime::from_ms(120_000));
        assert_eq!(parse_duration("500").unwrap(), VTime::from_ms(500));
        assert_eq!(parse_duration("0.000001us").unwrap(), VTime(1));
        // Truncation toward zero, never a float rounding.
        assert_eq!(parse_duration("0.0000001us").unwrap(), VTime(0));
        assert_eq!(
            ok("vt:+500ms"),
            Matcher::Time(TimeMatcher {
                relative: true,
                at: VTime::from_ms(500),
            })
        );
        assert_eq!(
            ok("vt:1.5s"),
            Matcher::Time(TimeMatcher {
                relative: false,
                at: VTime::from_ms(1_500),
            })
        );
    }

    /// Text the CLI accepts has to pass the generated schema too, or a command would work on the
    /// CLI and fail over MCP.
    #[test]
    fn durations_accept_exactly_the_schema_spellings() {
        for text in [".5s", "5.s", "5.", "5..5s", ".s", "."] {
            let e = parse_duration(text).unwrap_err();
            assert_eq!(e.code, E_USAGE, "`{text}` should be refused");
            assert!(e.hint.is_some(), "`{text}` should carry a hint");
        }
        for text in ["0.5s", "5s", "5", "0.000001us"] {
            parse_duration(text).unwrap_or_else(|e| panic!("`{text}`: {}", e.message));
        }
    }

    #[test]
    fn a_duration_the_virtual_clock_cannot_hold_is_refused() {
        assert_eq!(
            parse_duration("18446744073709us").unwrap(),
            VTime(18_446_744_073_709_000_000)
        );
        for text in ["99999999999999999999m", "99999999s", "18446745000000us"] {
            let e = parse_duration(text).unwrap_err();
            assert_eq!(e.code, E_USAGE, "`{text}` should be refused");
            assert!(e.message.contains("213 days"), "`{text}`: {}", e.message);
        }
        assert_eq!(refused("vt:+99999999s").code, E_USAGE);
    }

    #[test]
    fn ui_matchers_settle_by_default_and_others_do_not() {
        assert_eq!(ok("ui:changed").default_settle(), Settle::Ui);
        assert_eq!(ok("ui:label=\"Button\"").default_settle(), Settle::Ui);
        assert_eq!(ok("serial:/ready/").default_settle(), Settle::None);
        assert_eq!(
            ok("any(serial:/ready/,ui:changed)").default_settle(),
            Settle::Ui
        );
        assert_eq!(
            ok("any(serial:/ready/,event:reset)").default_settle(),
            Settle::None
        );
    }

    #[test]
    fn ui_operators_pick_the_text_class() {
        let Matcher::Ui(UiMatcher::Query { attr, pattern }) = ok("ui:label=\"Button\"") else {
            panic!("expected a ui query");
        };
        assert_eq!(attr, UiAttr::Label);
        assert_eq!(pattern, TextPattern::Exact("Button".into()));

        let Matcher::Ui(UiMatcher::Query { attr, pattern }) = ok("ui:focused~\"Display\"") else {
            panic!("expected a ui query");
        };
        assert_eq!(attr, UiAttr::Focused);
        assert_eq!(pattern, TextPattern::Literal("Display".into()));
    }

    #[test]
    fn symbol_hits_default_to_one() {
        assert_eq!(
            ok("symbol:lv_timer_handler"),
            Matcher::Symbol(SymbolMatcher {
                target: SymbolTarget::Name("lv_timer_handler".into()),
                hits: 1,
            })
        );
        assert_eq!(
            ok("symbol:0x40047e9e:hits=3"),
            Matcher::Symbol(SymbolMatcher {
                target: SymbolTarget::Addr(0x4004_7e9e),
                hits: 3,
            })
        );
    }

    #[test]
    fn address_matchers_are_not_limited_to_the_eight_hart_triggers() {
        let children: Vec<String> = (0..MAX_CHILDREN)
            .map(|i| format!("addr:0x3fca{:04x} == 1", 0x1000 + i))
            .collect();
        let m = ok(&format!("any({})", children.join(",")));
        assert_eq!(m.leaf_count(), MAX_CHILDREN);
        const _: () = assert!(MAX_CHILDREN > 8);
        assert_eq!(
            m.trigger_classes(),
            BTreeSet::from([TriggerClass::DataWrite])
        );
    }
}
