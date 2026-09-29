//! Reading a named global's value at its DWARF type, for `inspect vars` and `var:` matchers.
//!
//! A global is a `DW_TAG_variable` that is a direct child of a compilation unit with a bare
//! `DW_OP_addr` location. A file static such as `s_sel` may exist in two units, so a name matching
//! more than one is [`IntrospectError::AmbiguousGlobal`], never a silent first pick. [`VarQuery`]
//! is the one parser of `main.c::s_sel` and `s_ok[2]`, so `run` and `inspect vars` cannot
//! disagree about a name.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::sync::OnceLock;

use crate::IntrospectError;
use crate::dwarf::DebugInfo;
use crate::layout::{GuestMemory, MemError};

/// Bytes of a `char[]` global decoded as text: a value is one field of an output row, so it is
/// read at the size of a log line, not a buffer.
pub const MAX_TEXT: usize = 256;

/// Elements of an array global decoded when the query names no index; `name[900]` reads one.
pub const MAX_ELEMS: usize = 32;

/// A global's DWARF type, reduced to how its bytes are decoded. Typedefs and qualifiers are
/// followed through, so `const uint8_t` arrives as its base type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VarType {
    /// Of this many bytes (1, 2, 4 or 8).
    Signed(u8),
    Unsigned(u8),
    /// Nonzero is `true`.
    Bool(u8),
    /// 4 or 8 bytes.
    Float(u8),
    /// A 4-byte guest address, reported and not dereferenced.
    Pointer,
    /// Read at its underlying width and signedness.
    Enum {
        name: Option<String>,
        bytes: u8,
        signed: bool,
    },
    /// A `char[N]` or `unsigned char[N]`, decoded as NUL-terminated text.
    Text {
        /// `None` for an incomplete `char[]`.
        len: Option<u32>,
    },
    Array {
        elem: Box<VarType>,
        stride: u32,
        /// `None` for an incomplete array.
        len: Option<u32>,
    },
    /// A structure, union or anything else not decoded: its size only, rather than a number that
    /// means nothing.
    Opaque {
        /// The C spelling, as DWARF names it.
        name: String,
        /// 0 when the type carries none.
        size: u32,
    },
}

impl VarType {
    /// 0 when DWARF gave none.
    #[must_use]
    pub fn size(&self) -> u32 {
        match self {
            VarType::Signed(b)
            | VarType::Unsigned(b)
            | VarType::Bool(b)
            | VarType::Float(b)
            | VarType::Enum { bytes: b, .. } => u32::from(*b),
            VarType::Pointer => 4,
            VarType::Text { len } => len.unwrap_or(0),
            VarType::Array { stride, len, .. } => stride.saturating_mul(len.unwrap_or(0)),
            VarType::Opaque { size, .. } => *size,
        }
    }

    /// The C-ish spelling reported beside a value.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            VarType::Signed(1) => "int8".into(),
            VarType::Signed(2) => "int16".into(),
            VarType::Signed(4) => "int32".into(),
            VarType::Signed(8) => "int64".into(),
            VarType::Signed(b) => format!("int{}", u32::from(*b) * 8),
            VarType::Unsigned(b) => format!("uint{}", u32::from(*b) * 8),
            VarType::Bool(_) => "bool".into(),
            VarType::Float(4) => "float".into(),
            VarType::Float(_) => "double".into(),
            VarType::Pointer => "pointer".into(),
            VarType::Enum { name: Some(n), .. } => format!("enum {n}"),
            VarType::Enum { name: None, .. } => "enum".into(),
            VarType::Text { len: Some(n) } => format!("char[{n}]"),
            VarType::Text { len: None } => "char[]".into(),
            VarType::Array { elem, len, .. } => match len {
                Some(n) => format!("{}[{n}]", elem.render()),
                None => format!("{}[]", elem.render()),
            },
            VarType::Opaque { name, .. } => name.clone(),
        }
    }

    #[must_use]
    pub fn element(&self) -> Option<(VarType, u32)> {
        match self {
            VarType::Text { .. } => Some((VarType::Signed(1), 1)),
            VarType::Array { elem, stride, .. } => Some(((**elem).clone(), *stride)),
            _ => None,
        }
    }

    /// Declared element count of an array or text global.
    #[must_use]
    pub fn elems(&self) -> Option<u32> {
        match self {
            VarType::Text { len } | VarType::Array { len, .. } => *len,
            _ => None,
        }
    }
}

/// One statically allocated global the debug information describes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GlobalVar {
    pub name: String,
    /// `DW_AT_name` of the declaring unit: a guest build path. [`GlobalVar::unit_file`] is what
    /// a query matches.
    pub unit: String,
    pub addr: u32,
    pub ty: VarType,
    /// `DW_AT_external`: linker-visible rather than a static.
    pub external: bool,
}

impl GlobalVar {
    /// The file name of [`GlobalVar::unit`], which is what `main.c::s_sel` names.
    #[must_use]
    pub fn unit_file(&self) -> &str {
        self.unit
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(self.unit.as_str())
    }

    #[must_use]
    pub fn size(&self) -> u32 {
        self.ty.size()
    }
}

/// Every resolved global, by name. A name maps to every unit that declared one, in section
/// order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Globals {
    by_name: BTreeMap<String, Vec<GlobalVar>>,
}

impl Globals {
    #[must_use]
    pub fn new() -> Globals {
        Globals::default()
    }

    pub fn insert(&mut self, var: GlobalVar) -> &mut Globals {
        let slot = self.by_name.entry(var.name.clone()).or_default();
        // The same unit linked twice (a header pulled into two units) is not an
        // ambiguity.
        if !slot
            .iter()
            .any(|v| v.unit == var.unit && v.addr == var.addr)
        {
            slot.push(var);
        }
        self
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// The global a [`VarQuery`] names.
    ///
    /// # Errors
    ///
    /// [`IntrospectError::MissingGlobal`] when no unit (or not the named one) declares it, and
    /// [`IntrospectError::AmbiguousGlobal`] when several do and the query named none.
    pub fn resolve(&self, query: &VarQuery) -> Result<&GlobalVar, IntrospectError> {
        let all = self
            .by_name
            .get(query.name.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let matching: Vec<&GlobalVar> = match query.unit.as_deref() {
            Some(unit) => all.iter().filter(|v| v.unit_file() == unit).collect(),
            None => all.iter().collect(),
        };
        match matching.as_slice() {
            [one] => Ok(one),
            [] => Err(IntrospectError::MissingGlobal {
                name: query.render(),
            }),
            several => Err(IntrospectError::AmbiguousGlobal {
                name: query.render(),
                units: several.iter().map(|v| v.unit_file().to_owned()).collect(),
            }),
        }
    }
}

/// A global as a caller writes it: `s_sel`, `main.c::s_sel`, `s_ok[2]`. The only parser of
/// that grammar.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VarQuery {
    /// The compilation unit's file name, when the caller wrote one.
    pub unit: Option<String>,
    pub name: String,
    pub index: Option<u32>,
}

impl VarQuery {
    /// # Errors
    ///
    /// The text is empty, has an unbalanced or non-numeric `[]`, or has an empty name or unit.
    pub fn parse(text: &str) -> Result<VarQuery, VarQueryError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(VarQueryError::Empty);
        }
        let (head, index) = match text.strip_suffix(']') {
            Some(rest) => {
                let (head, idx) = rest.rsplit_once('[').ok_or(VarQueryError::Brackets)?;
                let idx: u32 = idx.trim().parse().map_err(|_| VarQueryError::Index)?;
                (head.trim(), Some(idx))
            }
            None => {
                if text.contains('[') {
                    return Err(VarQueryError::Brackets);
                }
                (text, None)
            }
        };
        let (unit, name) = match head.rsplit_once("::") {
            Some((unit, name)) => (Some(unit.trim().to_owned()), name.trim()),
            None => (None, head),
        };
        if name.is_empty() || unit.as_deref().is_some_and(str::is_empty) {
            return Err(VarQueryError::Empty);
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$'))
        {
            return Err(VarQueryError::Name);
        }
        Ok(VarQuery {
            unit,
            name: name.to_owned(),
            index,
        })
    }

    /// The query written back out, as an error names it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(unit) = &self.unit {
            out.push_str(unit);
            out.push_str("::");
        }
        out.push_str(&self.name);
        if let Some(i) = self.index {
            out.push('[');
            out.push_str(&i.to_string());
            out.push(']');
        }
        out
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum VarQueryError {
    /// Nothing, or nothing left after a `::`.
    Empty,
    Brackets,
    Index,
    Name,
}

impl fmt::Display for VarQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VarQueryError::Empty => f.write_str("names no global"),
            VarQueryError::Brackets => f.write_str("has an unbalanced `[]`"),
            VarQueryError::Index => f.write_str("has an index that is not a decimal number"),
            VarQueryError::Name => f.write_str(
                "is not a global: a name, optionally with a compilation unit (`main.c::s_sel`) \
                 or an index (`s_ok[2]`)",
            ),
        }
    }
}

impl std::error::Error for VarQueryError {}

/// A global's value. Equality is **bitwise** for a float, which `var:x changed` needs: the same
/// bits are no change, and a NaN does not read as changing every slice.
#[derive(Clone, Debug)]
pub enum VarValue {
    Int(i64),
    /// Also a pointer, as an address.
    Uint(u64),
    Bool(bool),
    Float(f64),
    /// Decoded to the first NUL, at most [`MAX_TEXT`] bytes.
    Text(String),
    /// At most [`MAX_ELEMS`] elements.
    Array(Vec<VarValue>),
    /// A structure or union: its size only.
    Opaque {
        name: String,
        size: u32,
    },
}

impl PartialEq for VarValue {
    fn eq(&self, other: &VarValue) -> bool {
        match (self, other) {
            (VarValue::Int(a), VarValue::Int(b)) => a == b,
            (VarValue::Uint(a), VarValue::Uint(b)) => a == b,
            (VarValue::Bool(a), VarValue::Bool(b)) => a == b,
            (VarValue::Float(a), VarValue::Float(b)) => a.to_bits() == b.to_bits(),
            (VarValue::Text(a), VarValue::Text(b)) => a == b,
            (VarValue::Array(a), VarValue::Array(b)) => a == b,
            (VarValue::Opaque { name: a, size: x }, VarValue::Opaque { name: b, size: y }) => {
                a == b && x == y
            }
            _ => false,
        }
    }
}

impl Eq for VarValue {}

impl VarValue {
    /// The value as an orderable integer. A `bool` reads as 0 or 1 so `var:s_ready == 1` works;
    /// text, arrays and aggregates answer `None`.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            VarValue::Int(v) => Some(*v),
            VarValue::Uint(v) => i64::try_from(*v).ok(),
            VarValue::Bool(v) => Some(i64::from(*v)),
            VarValue::Float(_)
            | VarValue::Text(_)
            | VarValue::Array(_)
            | VarValue::Opaque { .. } => None,
        }
    }

    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            VarValue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            VarValue::Bool(v) => Some(*v),
            VarValue::Int(v) => Some(*v != 0),
            VarValue::Uint(v) => Some(*v != 0),
            _ => None,
        }
    }

    #[must_use]
    pub fn render(&self) -> String {
        match self {
            VarValue::Int(v) => v.to_string(),
            VarValue::Uint(v) => v.to_string(),
            VarValue::Bool(v) => v.to_string(),
            VarValue::Float(v) => format!("{v}"),
            VarValue::Text(s) => format!("{s:?}"),
            VarValue::Array(items) => {
                let inner: Vec<String> = items.iter().map(VarValue::render).collect();
                format!("[{}]", inner.join(", "))
            }
            VarValue::Opaque { name, size } => format!("<{name}, {size} bytes>"),
        }
    }
}

/// One global's name, type and value at the instant `mem` was read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VarReading {
    pub query: String,
    pub name: String,
    /// The declaring unit's file name.
    pub unit: String,
    /// Of the object, or of the element when the query had an index.
    pub addr: u32,
    /// Bytes the read covered, which is what a `var:` watch arms over.
    pub len: u32,
    pub ty: String,
    pub value: VarValue,
}

/// Reads one global at its DWARF type. An indexed query reads that element only.
///
/// # Errors
///
/// From [`Globals::resolve`]; `Memory` for unreadable bytes; `LayoutMismatch` for an index out of
/// bounds or on a non-array.
pub fn read(
    globals: &Globals,
    mem: &dyn GuestMemory,
    query: &VarQuery,
) -> Result<VarReading, IntrospectError> {
    let var = globals.resolve(query)?;
    let (addr, ty) = match query.index {
        None => (var.addr, var.ty.clone()),
        Some(i) => {
            let (elem, stride) =
                var.ty
                    .element()
                    .ok_or_else(|| IntrospectError::LayoutMismatch {
                        what: "an indexed `var:` query",
                        detail: format!("`{}` is {}, not an array", var.name, var.ty.render()),
                    })?;
            if let Some(len) = var.ty.elems()
                && i >= len
            {
                return Err(IntrospectError::LayoutMismatch {
                    what: "an indexed `var:` query",
                    detail: format!(
                        "`{}` has {len} elements, so [{i}] is out of bounds",
                        var.name
                    ),
                });
            }
            (var.addr.wrapping_add(i.saturating_mul(stride)), elem)
        }
    };
    let value = read_at(mem, addr, &ty)?;
    Ok(VarReading {
        query: query.render(),
        name: var.name.clone(),
        unit: var.unit_file().to_owned(),
        addr,
        len: read_len(&ty, &value),
        ty: ty.render(),
        value,
    })
}

/// The span a `var:` watch arms over.
fn read_len(ty: &VarType, value: &VarValue) -> u32 {
    match (ty, value) {
        // A `char[]` watch covers the decoded text plus its NUL: a store past it cannot
        // change the value.
        (VarType::Text { .. }, VarValue::Text(s)) => {
            u32::try_from(s.len().saturating_add(1)).unwrap_or(u32::MAX)
        }
        (VarType::Array { stride, .. }, VarValue::Array(items)) => {
            stride.saturating_mul(u32::try_from(items.len()).unwrap_or(u32::MAX))
        }
        _ => ty.size().max(1),
    }
}

fn read_at(mem: &dyn GuestMemory, addr: u32, ty: &VarType) -> Result<VarValue, IntrospectError> {
    Ok(match ty {
        VarType::Signed(b) => VarValue::Int(signed(mem, addr, *b)?),
        VarType::Unsigned(b) => VarValue::Uint(unsigned(mem, addr, *b)?),
        VarType::Bool(b) => VarValue::Bool(unsigned(mem, addr, *b)? != 0),
        VarType::Float(4) => VarValue::Float(f64::from(f32::from_bits(mem.u32(addr)?))),
        VarType::Float(_) => {
            let mut bytes = [0u8; 8];
            mem.read(addr, &mut bytes)?;
            VarValue::Float(f64::from_le_bytes(bytes))
        }
        VarType::Pointer => VarValue::Uint(u64::from(mem.u32(addr)?)),
        VarType::Enum {
            bytes, signed: s, ..
        } => {
            if *s {
                VarValue::Int(signed(mem, addr, *bytes)?)
            } else {
                VarValue::Uint(unsigned(mem, addr, *bytes)?)
            }
        }
        VarType::Text { len } => {
            let max = len.map_or(MAX_TEXT, |n| (n as usize).min(MAX_TEXT));
            // Read the first byte strictly, so an unmapped global is a memory error, not
            // an empty string.
            mem.u8(addr)?;
            VarValue::Text(mem.cstr(addr, max))
        }
        VarType::Array { elem, stride, len } => {
            let count = len.map_or(MAX_ELEMS, |n| (n as usize).min(MAX_ELEMS));
            let mut items = Vec::with_capacity(count);
            for i in 0..count {
                let at = addr.wrapping_add(u32::try_from(i).unwrap_or(0).saturating_mul(*stride));
                items.push(read_at(mem, at, elem)?);
            }
            VarValue::Array(items)
        }
        VarType::Opaque { name, size } => VarValue::Opaque {
            name: name.clone(),
            size: *size,
        },
    })
}

/// Little-endian, zero-extended.
fn unsigned(mem: &dyn GuestMemory, addr: u32, bytes: u8) -> Result<u64, MemError> {
    let n = usize::from(bytes).clamp(1, 8);
    let mut buf = [0u8; 8];
    mem.read(addr, &mut buf[..n])?;
    Ok(u64::from_le_bytes(buf))
}

/// Little-endian, sign-extended.
fn signed(mem: &dyn GuestMemory, addr: u32, bytes: u8) -> Result<i64, MemError> {
    let n = usize::from(bytes).clamp(1, 8);
    let raw = unsigned(mem, addr, bytes)?;
    if n == 8 {
        return Ok(raw as i64);
    }
    let shift = 64 - n * 8;
    Ok(((raw << shift) as i64) >> shift)
}

/// One query's answer. A failure is per name, so a misspelled name does not hide the others'
/// readings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VarRead {
    Ok(VarReading),
    Err {
        query: String,
        error: IntrospectError,
    },
}

impl VarRead {
    #[must_use]
    pub fn query(&self) -> &str {
        match self {
            VarRead::Ok(r) => r.query.as_str(),
            VarRead::Err { query, .. } => query.as_str(),
        }
    }

    #[must_use]
    pub fn reading(&self) -> Option<&VarReading> {
        match self {
            VarRead::Ok(r) => Some(r),
            VarRead::Err { .. } => None,
        }
    }
}

/// One answer per query, in the order asked for.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct VarSnapshot {
    pub reads: Vec<VarRead>,
}

impl VarSnapshot {
    /// The one reading of a single-query snapshot.
    ///
    /// # Errors
    ///
    /// The query could not be answered; an empty snapshot is `MissingGlobal` with an empty name.
    pub fn only(&self) -> Result<&VarReading, IntrospectError> {
        match self.reads.first() {
            Some(VarRead::Ok(r)) => Ok(r),
            Some(VarRead::Err { error, .. }) => Err(error.clone()),
            None => Err(IntrospectError::MissingGlobal {
                name: String::new(),
            }),
        }
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for read in &self.reads {
            match read {
                VarRead::Ok(r) => {
                    let _ = writeln!(
                        out,
                        "{} {:#010x} {} = {}",
                        r.query,
                        r.addr,
                        r.ty,
                        r.value.render()
                    );
                }
                VarRead::Err { query, error } => {
                    let _ = writeln!(out, "{query} unreadable: {error}");
                }
            }
        }
        out
    }
}

/// The [`Globals`] of one app ELF, resolved at the first call. Shared by every host so
/// `inspect vars` answers the same in a browser and natively.
#[derive(Debug, Default)]
pub struct GlobalsCache(OnceLock<Result<Globals, IntrospectError>>);

impl GlobalsCache {
    #[must_use]
    pub fn new() -> GlobalsCache {
        GlobalsCache::default()
    }

    /// The globals of `elf`, resolved on the first call.
    ///
    /// # Errors
    ///
    /// The DWARF does not parse; the error is remembered, so it is not re-parsed per call.
    pub fn get(
        &self,
        elf: &pemu_loader::elf::ElfInfo,
        bytes: &[u8],
    ) -> Result<&Globals, IntrospectError> {
        self.0
            .get_or_init(|| DebugInfo::parse(elf, bytes).map(|debug| debug.globals(&[])))
            .as_ref()
            .map_err(Clone::clone)
    }

    #[must_use]
    pub fn resolved(&self) -> bool {
        self.0.get().is_some()
    }
}

#[must_use]
pub fn read_all(globals: &Globals, mem: &dyn GuestMemory, queries: &[VarQuery]) -> VarSnapshot {
    VarSnapshot {
        reads: queries
            .iter()
            .map(|q| match read(globals, mem, q) {
                Ok(r) => VarRead::Ok(r),
                Err(error) => VarRead::Err {
                    query: q.render(),
                    error,
                },
            })
            .collect(),
    }
}

/// Resolves only the globals `queries` names. Every unit is still visited, because an
/// ambiguity cannot be seen from the first hit.
#[must_use]
pub fn resolve_globals(debug: &DebugInfo<'_>, queries: &[VarQuery]) -> Globals {
    let names: Vec<&str> = queries.iter().map(|q| q.name.as_str()).collect();
    debug.globals(&names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::MemoryImage;

    /// One page at `0x3fca_0000`, the DRAM the corpus statics live in.
    fn image() -> MemoryImage {
        let mut m = MemoryImage::new();
        m.map_zeroed(0x3fca_0000, 0x1000);
        m
    }

    fn var(name: &str, addr: u32, ty: VarType) -> GlobalVar {
        GlobalVar {
            name: name.to_owned(),
            unit: format!("/build/main/{name}.c"),
            addr,
            ty,
            external: false,
        }
    }

    #[test]
    fn a_query_carries_a_unit_and_an_index() {
        assert_eq!(
            VarQuery::parse("s_sel").expect("a bare name"),
            VarQuery {
                unit: None,
                name: "s_sel".into(),
                index: None
            }
        );
        assert_eq!(
            VarQuery::parse("main.c::s_sel").expect("a qualified name"),
            VarQuery {
                unit: Some("main.c".into()),
                name: "s_sel".into(),
                index: None
            }
        );
        let indexed = VarQuery::parse("s_ok[2]").expect("an indexed name");
        assert_eq!(indexed.index, Some(2));
        assert_eq!(indexed.name, "s_ok");
        assert_eq!(
            VarQuery::parse("main.c::s_ok[2]").expect("both").render(),
            "main.c::s_ok[2]"
        );
    }

    #[test]
    fn a_query_that_is_not_a_global_is_refused() {
        assert_eq!(VarQuery::parse(""), Err(VarQueryError::Empty));
        assert_eq!(VarQuery::parse("s_ok["), Err(VarQueryError::Brackets));
        assert_eq!(VarQuery::parse("s_ok[x]"), Err(VarQueryError::Index));
        assert_eq!(VarQuery::parse("s ok"), Err(VarQueryError::Name));
        assert_eq!(VarQuery::parse("::s_sel"), Err(VarQueryError::Empty));
    }

    #[test]
    fn an_int_global_reads_at_its_width_and_sign() {
        let mut m = image();
        m.put_u32(0x3fca_0000, 1);
        m.put_u32(0x3fca_0004, 0xffff_ffff);
        m.put(0x3fca_0008, &[0xfe]);
        m.put(0x3fca_000c, &[0xfe]);
        let mut g = Globals::new();
        g.insert(var("s_sel", 0x3fca_0000, VarType::Signed(4)));
        g.insert(var("s_active", 0x3fca_0004, VarType::Signed(4)));
        g.insert(var("s_byte", 0x3fca_0008, VarType::Signed(1)));
        g.insert(var("s_ubyte", 0x3fca_000c, VarType::Unsigned(1)));
        let read_one = |name: &str| {
            read(&g, &m, &VarQuery::parse(name).expect("a name")).expect("a readable global")
        };
        assert_eq!(read_one("s_sel").value, VarValue::Int(1));
        assert_eq!(read_one("s_active").value, VarValue::Int(-1));
        assert_eq!(read_one("s_byte").value, VarValue::Int(-2));
        assert_eq!(read_one("s_ubyte").value, VarValue::Uint(254));
        assert_eq!(read_one("s_sel").ty, "int32");
        assert_eq!(read_one("s_sel").len, 4);
    }

    /// The menu globals as the corpus declares them: settled `(0, -1)`, `(1, -1)` after a
    /// `click DOWN`.
    #[test]
    fn the_menu_globals_read_the_values_the_menu_smoke_asserts() {
        let mut m = image();
        let mut g = Globals::new();
        g.insert(var("s_sel", 0x3fca_0000, VarType::Signed(4)));
        g.insert(var("s_active", 0x3fca_0004, VarType::Signed(4)));
        m.put_u32(0x3fca_0000, 0);
        m.put_u32(0x3fca_0004, u32::MAX);
        let at = |m: &MemoryImage, n: &str| {
            read(&g, m, &VarQuery::parse(n).expect("a name"))
                .expect("a readable global")
                .value
                .as_int()
                .expect("an int")
        };
        assert_eq!((at(&m, "s_sel"), at(&m, "s_active")), (0, -1));
        m.put_u32(0x3fca_0000, 1);
        assert_eq!((at(&m, "s_sel"), at(&m, "s_active")), (1, -1));
    }

    #[test]
    fn a_bool_a_pointer_and_an_enum_read_at_their_own_widths() {
        let mut m = image();
        m.put(0x3fca_0000, &[1]);
        m.put_u32(0x3fca_0004, 0x3fca_0800);
        m.put(0x3fca_0008, &[0xff, 0xff]);
        let mut g = Globals::new();
        g.insert(var("s_ready", 0x3fca_0000, VarType::Bool(1)));
        g.insert(var("s_ctx", 0x3fca_0004, VarType::Pointer));
        g.insert(var(
            "s_mode",
            0x3fca_0008,
            VarType::Enum {
                name: Some("mode_t".into()),
                bytes: 2,
                signed: true,
            },
        ));
        let one = |n: &str| read(&g, &m, &VarQuery::parse(n).expect("a name")).expect("readable");
        assert_eq!(one("s_ready").value, VarValue::Bool(true));
        assert_eq!(one("s_ready").value.as_int(), Some(1));
        assert_eq!(one("s_ctx").value, VarValue::Uint(0x3fca_0800));
        assert_eq!(one("s_mode").value, VarValue::Int(-1));
        assert_eq!(one("s_mode").ty, "enum mode_t");
    }

    #[test]
    fn a_char_array_reads_to_its_nul_and_arms_over_what_it_read() {
        let mut m = image();
        m.put(0x3fca_0000, b"Button\0and then some rubbish");
        let mut g = Globals::new();
        g.insert(var("s_label", 0x3fca_0000, VarType::Text { len: Some(32) }));
        let out = read(&g, &m, &VarQuery::parse("s_label").expect("a name")).expect("readable");
        assert_eq!(out.value.as_text(), Some("Button"));
        assert_eq!(out.len, 7, "the text and its NUL, not the whole buffer");
    }

    #[test]
    fn an_array_global_reads_whole_or_by_element() {
        let mut m = image();
        for i in 0..8u32 {
            m.put_u32(0x3fca_0000 + i * 4, i * 10);
        }
        let mut g = Globals::new();
        g.insert(var(
            "s_ok",
            0x3fca_0000,
            VarType::Array {
                elem: Box::new(VarType::Signed(4)),
                stride: 4,
                len: Some(8),
            },
        ));
        let whole = read(&g, &m, &VarQuery::parse("s_ok").expect("a name")).expect("readable");
        assert_eq!(
            whole.value,
            VarValue::Array((0..8).map(|i| VarValue::Int(i * 10)).collect())
        );
        let one = read(&g, &m, &VarQuery::parse("s_ok[2]").expect("a name")).expect("readable");
        assert_eq!(one.value, VarValue::Int(20));
        assert_eq!(one.addr, 0x3fca_0008, "the element, not the array");
        assert_eq!(one.len, 4, "a watch on `s_ok[2]` covers one element");
    }

    #[test]
    fn an_index_past_the_declared_bounds_is_refused_rather_than_read() {
        let mut m = image();
        m.put_u32(0x3fca_0020, 99);
        let mut g = Globals::new();
        g.insert(var(
            "s_ok",
            0x3fca_0000,
            VarType::Array {
                elem: Box::new(VarType::Signed(4)),
                stride: 4,
                len: Some(8),
            },
        ));
        let err =
            read(&g, &m, &VarQuery::parse("s_ok[8]").expect("a name")).expect_err("out of bounds");
        assert!(
            matches!(err, IntrospectError::LayoutMismatch { .. }),
            "{err}"
        );
        assert!(
            format!("{err}").contains("8 elements"),
            "the error says what the bound is: {err}"
        );
    }

    #[test]
    fn indexing_something_that_is_not_an_array_is_refused() {
        let m = image();
        let mut g = Globals::new();
        g.insert(var("s_sel", 0x3fca_0000, VarType::Signed(4)));
        let err =
            read(&g, &m, &VarQuery::parse("s_sel[0]").expect("a name")).expect_err("not an array");
        assert!(format!("{err}").contains("not an array"), "{err}");
    }

    #[test]
    fn a_struct_global_reports_its_size_rather_than_a_number_that_means_nothing() {
        let m = image();
        let mut g = Globals::new();
        g.insert(var(
            "lvgl_port_ctx",
            0x3fca_0000,
            VarType::Opaque {
                name: "lvgl_port_ctx_t".into(),
                size: 96,
            },
        ));
        let out = read(&g, &m, &VarQuery::parse("lvgl_port_ctx").expect("a name")).expect("read");
        assert_eq!(
            out.value,
            VarValue::Opaque {
                name: "lvgl_port_ctx_t".into(),
                size: 96
            }
        );
        assert_eq!(out.value.as_int(), None, "an aggregate has no ordering");
    }

    #[test]
    fn a_name_two_units_declare_is_ambiguous_rather_than_the_first() {
        let m = image();
        let mut g = Globals::new();
        g.insert(GlobalVar {
            name: "s_sel".into(),
            unit: "/b/main/main.c".into(),
            addr: 0x3fca_0000,
            ty: VarType::Signed(4),
            external: false,
        });
        g.insert(GlobalVar {
            name: "s_sel".into(),
            unit: "/b/ui/menu.c".into(),
            addr: 0x3fca_0010,
            ty: VarType::Signed(4),
            external: false,
        });
        let err = read(&g, &m, &VarQuery::parse("s_sel").expect("a name"))
            .expect_err("two units declare it");
        let IntrospectError::AmbiguousGlobal { units, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(units, &["main.c".to_owned(), "menu.c".to_owned()]);
        let picked = read(&g, &m, &VarQuery::parse("menu.c::s_sel").expect("a name"))
            .expect("the unit picks one");
        assert_eq!(picked.addr, 0x3fca_0010);
    }

    #[test]
    fn a_name_no_unit_declares_names_itself_in_the_error() {
        let m = image();
        let g = Globals::new();
        let err = read(&g, &m, &VarQuery::parse("s_nope").expect("a name"))
            .expect_err("nothing declares it");
        assert!(format!("{err}").contains("s_nope"), "{err}");
    }

    #[test]
    fn an_unmapped_global_is_a_memory_error_and_not_a_zero() {
        let m = image();
        let mut g = Globals::new();
        g.insert(var("s_far", 0x5000_0000, VarType::Signed(4)));
        g.insert(var("s_text", 0x5000_0000, VarType::Text { len: Some(8) }));
        for name in ["s_far", "s_text"] {
            let err = read(&g, &m, &VarQuery::parse(name).expect("a name"))
                .expect_err("nothing is mapped there");
            assert!(matches!(err, IntrospectError::Memory(_)), "{name}: {err}");
        }
    }

    #[test]
    fn the_same_global_in_two_copies_of_one_unit_is_not_an_ambiguity() {
        let m = image();
        let mut g = Globals::new();
        for _ in 0..2 {
            g.insert(var("s_sel", 0x3fca_0000, VarType::Signed(4)));
        }
        assert!(read(&g, &m, &VarQuery::parse("s_sel").expect("a name")).is_ok());
    }
}
