//! Reading the guest's own debug information: structure layouts for the walkers, globals, and
//! an `addr2line` context for inline-aware frames.
//!
//! ESP-IDF adds `-gdwarf-4 -ggdb` to every build type, so a stock build carries DWARF 4. Bitfields
//! are placed by DWARF 4 `DW_AT_bit_offset` (from the most significant bit of the storage unit) or
//! DWARF 5 `DW_AT_data_bit_offset` (from the structure start); both normalize to the same
//! little-endian `byte:bit/width`.

use std::collections::BTreeSet;

use gimli::{AttributeValue, EndianSlice, RunTimeEndian, SectionId, UnitOffset};
use pemu_loader::elf::ElfInfo;

use crate::IntrospectError;
use crate::layout::{Bitfield, LAYOUT_REQUESTS, Layouts, MemberLayout, StructLayout};
use crate::vars::{GlobalVar, Globals, VarType};

pub type Slice<'a> = EndianSlice<'a, RunTimeEndian>;

/// How deep a dotted member path may reach through nested structures and unions.
const MAX_NESTING: u32 = 8;

#[derive(Copy, Clone, Debug)]
struct Nest<'p> {
    /// Dotted path of the enclosing member, empty at the outermost structure.
    prefix: &'p str,
    /// Byte offset of the enclosing member from the start of the outermost structure.
    base: u64,
    /// Bounded by [`MAX_NESTING`] so a cyclic type cannot loop the walk.
    depth: u32,
}

impl Nest<'static> {
    fn root() -> Nest<'static> {
        Nest {
            prefix: "",
            base: 0,
            depth: 0,
        }
    }
}

/// One frame of an `addr2line` lookup. A lookup returns the innermost inlined frame first,
/// then its callers, so one PC can expand to several frames.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InlineFrame {
    /// Undecorated: C firmware needs no demangling.
    pub function: Option<String>,
    /// Exactly as the debug information spells it.
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// True for every frame but the outermost, which is the real call frame.
    pub inlined: bool,
}

/// The debug information of one ELF, borrowed from its bytes. Holds two gimli views of the same
/// sections: one for the layout walk, one owned by the `addr2line` context.
pub struct DebugInfo<'a> {
    dwarf: gimli::Dwarf<Slice<'a>>,
    ctx: addr2line::Context<Slice<'a>>,
    debug_frame: &'a [u8],
    endian: RunTimeEndian,
    units: usize,
    build_root: Option<String>,
}

impl<'a> DebugInfo<'a> {
    /// Parses the debug sections of `elf`. An ELF with no debug information parses into an empty
    /// [`DebugInfo`] (see [`DebugInfo::has_debug_info`]), so a stripped image falls back to symbols
    /// instead of failing.
    pub fn parse(elf: &ElfInfo, bytes: &'a [u8]) -> Result<DebugInfo<'a>, IntrospectError> {
        let endian = RunTimeEndian::Little;
        let section = |name: &str| -> &'a [u8] {
            elf.section(name)
                .and_then(|s| s.data(bytes))
                .unwrap_or(&[][..])
        };
        let load = |id: SectionId| -> Result<Slice<'a>, gimli::Error> {
            Ok(EndianSlice::new(section(id.name()), endian))
        };
        let dwarf = gimli::Dwarf::load(load).map_err(|e| IntrospectError::Dwarf(e.to_string()))?;
        let for_ctx =
            gimli::Dwarf::load(load).map_err(|e| IntrospectError::Dwarf(e.to_string()))?;
        let ctx = addr2line::Context::from_dwarf(for_ctx)
            .map_err(|e| IntrospectError::Dwarf(e.to_string()))?;
        let mut units = 0usize;
        let mut build_root = None;
        let mut headers = dwarf.units();
        while let Some(header) = headers
            .next()
            .map_err(|e| IntrospectError::Dwarf(e.to_string()))?
        {
            units += 1;
            if build_root.is_none()
                && let Ok(unit) = dwarf.unit(header)
                && let Some(dir) = unit.comp_dir
            {
                build_root = Some(dir.to_string_lossy().into_owned());
            }
        }
        Ok(DebugInfo {
            dwarf,
            ctx,
            debug_frame: section(SectionId::DebugFrame.name()),
            endian,
            units,
            build_root,
        })
    }

    pub fn units(&self) -> usize {
        self.units
    }

    pub fn has_debug_info(&self) -> bool {
        self.units > 0
    }

    /// `DW_AT_comp_dir` of the first compilation unit, which frame paths are made relative to.
    pub fn build_root(&self) -> Option<&str> {
        self.build_root.as_deref()
    }

    pub fn debug_frame(&self) -> &'a [u8] {
        self.debug_frame
    }

    pub fn endian(&self) -> RunTimeEndian {
        self.endian
    }

    pub fn layouts(&self) -> Layouts {
        self.resolve(LAYOUT_REQUESTS)
    }

    /// Resolves a request list, as (C struct or typedef name, member paths). The walk stops once
    /// every requested structure has been seen. A structure defined in several units is taken from
    /// the first in section order.
    pub fn resolve(&self, requests: &[(&str, &[&str])]) -> Layouts {
        let mut out = Layouts::new();
        let mut wanted: BTreeSet<&str> = requests.iter().map(|(n, _)| *n).collect();
        let mut headers = self.dwarf.units();
        while let Ok(Some(header)) = headers.next() {
            if wanted.is_empty() {
                break;
            }
            let Ok(unit) = self.dwarf.unit(header) else {
                continue;
            };
            let Ok(mut tree) = unit.entries_tree(None) else {
                continue;
            };
            let Ok(root) = tree.root() else { continue };
            let mut children = root.children();
            let mut hits: Vec<(&str, UnitOffset)> = Vec::new();
            while let Ok(Some(child)) = children.next() {
                let entry = child.entry();
                let tag = entry.tag();
                if tag != gimli::DW_TAG_structure_type
                    && tag != gimli::DW_TAG_union_type
                    && tag != gimli::DW_TAG_typedef
                {
                    continue;
                }
                if entry.attr_value(gimli::DW_AT_declaration).is_some() {
                    continue;
                }
                let Some(name) = self.name_of(&unit, entry) else {
                    continue;
                };
                let Some(&want) = wanted.get(name.as_str()) else {
                    continue;
                };
                // A typedef of an anonymous structure (`lv_style_t`, `control_t`, `block_header_t`,
                // `RvExcFrame`) is followed to the structure it names.
                let target = if tag == gimli::DW_TAG_typedef {
                    match self.follow_type(&unit, entry.offset()) {
                        Some(off) => off,
                        None => continue,
                    }
                } else {
                    entry.offset()
                };
                hits.push((want, target));
            }
            for (name, offset) in hits {
                let members = requests
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, m)| *m)
                    .unwrap_or(&[]);
                if let Some(layout) = self.struct_at(&unit, offset, name, members, &mut out) {
                    wanted.remove(name);
                    out.insert(layout);
                }
            }
        }
        for name in wanted {
            out.note_missing_struct(name);
        }
        out
    }

    /// Resolves the globals of `names` (every global when empty) with their address and decoded
    /// type. Unlike [`DebugInfo::resolve`] every unit is visited, because a file static of the same
    /// name in two units is an ambiguity. Only direct children of each unit root are read, so a
    /// function-local `static` is not a global.
    #[must_use]
    pub fn globals(&self, names: &[&str]) -> Globals {
        let wanted: BTreeSet<&str> = names.iter().copied().collect();
        let mut out = Globals::new();
        let mut headers = self.dwarf.units();
        while let Ok(Some(header)) = headers.next() {
            let Ok(unit) = self.dwarf.unit(header) else {
                continue;
            };
            let Ok(mut tree) = unit.entries_tree(None) else {
                continue;
            };
            let Ok(root) = tree.root() else { continue };
            let unit_name = self.name_of(&unit, root.entry()).unwrap_or_default();
            let mut children = root.children();
            let mut hits: Vec<(String, u32, bool, UnitOffset)> = Vec::new();
            while let Ok(Some(child)) = children.next() {
                let entry = child.entry();
                if entry.tag() != gimli::DW_TAG_variable {
                    continue;
                }
                // A declaration has no storage, and an optimized-out variable has no `DW_OP_addr`
                // location: neither can be watched.
                if entry.attr_value(gimli::DW_AT_declaration).is_some() {
                    continue;
                }
                let Some(name) = self.name_of(&unit, entry) else {
                    continue;
                };
                if !wanted.is_empty() && !wanted.contains(name.as_str()) {
                    continue;
                }
                let Some(addr) = static_address(entry) else {
                    continue;
                };
                let external = matches!(
                    entry.attr_value(gimli::DW_AT_external),
                    Some(AttributeValue::Flag(true))
                );
                let Some(AttributeValue::UnitRef(ty)) = entry.attr_value(gimli::DW_AT_type) else {
                    continue;
                };
                hits.push((name, addr, external, ty));
            }
            for (name, addr, external, ty) in hits {
                out.insert(GlobalVar {
                    name,
                    unit: unit_name.clone(),
                    addr,
                    ty: self.var_type(&unit, ty, 0),
                    external,
                });
            }
        }
        out
    }

    /// Decodes the DIE at `offset` into the [`VarType`] a value is read at, following typedefs and
    /// qualifiers. Anything undecodable within [`MAX_NESTING`] links is [`VarType::Opaque`].
    fn var_type(&self, unit: &gimli::Unit<Slice<'a>>, offset: UnitOffset, depth: u32) -> VarType {
        if depth > MAX_NESTING {
            return VarType::Opaque {
                name: "<type nested too deeply>".into(),
                size: 0,
            };
        }
        let Ok(entry) = unit.entry(offset) else {
            return VarType::Opaque {
                name: "<unreadable type>".into(),
                size: 0,
            };
        };
        let byte_size = |default: u32| -> u32 {
            entry
                .attr_value(gimli::DW_AT_byte_size)
                .and_then(|v: AttributeValue<Slice<'a>>| v.udata_value())
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(default)
        };
        let inner = || match entry.attr_value(gimli::DW_AT_type) {
            Some(AttributeValue::UnitRef(next)) => Some(next),
            _ => None,
        };
        match entry.tag() {
            gimli::DW_TAG_base_type => {
                let bytes = u8::try_from(byte_size(4)).unwrap_or(4).clamp(1, 8);
                match entry.attr_value(gimli::DW_AT_encoding) {
                    Some(AttributeValue::Encoding(gimli::DW_ATE_boolean)) => VarType::Bool(bytes),
                    Some(AttributeValue::Encoding(gimli::DW_ATE_float)) => VarType::Float(bytes),
                    Some(AttributeValue::Encoding(
                        gimli::DW_ATE_unsigned | gimli::DW_ATE_unsigned_char,
                    )) => VarType::Unsigned(bytes),
                    _ => VarType::Signed(bytes),
                }
            }
            gimli::DW_TAG_pointer_type => VarType::Pointer,
            gimli::DW_TAG_enumeration_type => {
                let bytes = u8::try_from(byte_size(4)).unwrap_or(4).clamp(1, 8);
                // DWARF 4 gives an enum a `DW_AT_type` only sometimes; signed is the safe read,
                // since sign extension of a value with its top bit clear changes nothing.
                let signed = !matches!(
                    inner().map(|t| self.var_type(unit, t, depth + 1)),
                    Some(VarType::Unsigned(_))
                );
                VarType::Enum {
                    name: self.name_of(unit, &entry),
                    bytes,
                    signed,
                }
            }
            gimli::DW_TAG_typedef
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type => match inner() {
                Some(next) => self.var_type(unit, next, depth + 1),
                // `const void` and a typedef of nothing are both `void`.
                None => VarType::Opaque {
                    name: "void".into(),
                    size: 0,
                },
            },
            gimli::DW_TAG_array_type => self.array_type(unit, offset, &entry, depth),
            gimli::DW_TAG_structure_type | gimli::DW_TAG_union_type | gimli::DW_TAG_class_type => {
                VarType::Opaque {
                    name: self
                        .name_of(unit, &entry)
                        .map_or_else(|| "<anonymous aggregate>".into(), |n| format!("struct {n}")),
                    size: byte_size(0),
                }
            }
            other => VarType::Opaque {
                name: format!("<{other}>"),
                size: byte_size(0),
            },
        }
    }

    /// Decodes a `DW_TAG_array_type` from its element type, stride and first
    /// `DW_TAG_subrange_type`. A `char[]` becomes [`VarType::Text`].
    fn array_type(
        &self,
        unit: &gimli::Unit<Slice<'a>>,
        offset: UnitOffset,
        entry: &gimli::DebuggingInformationEntry<Slice<'a>>,
        depth: u32,
    ) -> VarType {
        let elem = match entry.attr_value(gimli::DW_AT_type) {
            Some(AttributeValue::UnitRef(next)) => self.var_type(unit, next, depth + 1),
            _ => VarType::Opaque {
                name: "void".into(),
                size: 0,
            },
        };
        let stride = entry
            .attr_value(gimli::DW_AT_byte_stride)
            .and_then(|v: AttributeValue<Slice<'a>>| v.udata_value())
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or_else(|| elem.size())
            .max(1);
        let mut len = None;
        if let Ok(mut tree) = unit.entries_tree(Some(offset))
            && let Ok(root) = tree.root()
        {
            let mut children = root.children();
            while let Ok(Some(child)) = children.next() {
                let sub = child.entry();
                if sub.tag() != gimli::DW_TAG_subrange_type {
                    continue;
                }
                // `DW_AT_upper_bound` is the last index, so the count is one more. A flexible
                // array member has neither and stays `None`.
                len = sub
                    .attr_value(gimli::DW_AT_count)
                    .and_then(|v: AttributeValue<Slice<'a>>| v.udata_value())
                    .or_else(|| {
                        sub.attr_value(gimli::DW_AT_upper_bound)
                            .and_then(|v: AttributeValue<Slice<'a>>| v.udata_value())
                            .map(|u| u + 1)
                    })
                    .and_then(|v| u32::try_from(v).ok());
                break;
            }
        }
        if stride == 1 && matches!(elem, VarType::Signed(1) | VarType::Unsigned(1)) {
            return VarType::Text { len };
        }
        VarType::Array {
            elem: Box::new(elem),
            stride,
            len,
        }
    }

    fn struct_at(
        &self,
        unit: &gimli::Unit<Slice<'a>>,
        offset: UnitOffset,
        name: &str,
        members: &[&str],
        out: &mut Layouts,
    ) -> Option<StructLayout> {
        let mut tree = unit.entries_tree(Some(offset)).ok()?;
        let root = tree.root().ok()?;
        let entry = root.entry();
        let tag = entry.tag();
        if tag != gimli::DW_TAG_structure_type && tag != gimli::DW_TAG_union_type {
            return None;
        }
        if entry.attr_value(gimli::DW_AT_declaration).is_some() {
            return None;
        }
        let size = entry
            .attr_value(gimli::DW_AT_byte_size)
            .and_then(|v: AttributeValue<Slice<'a>>| v.udata_value())
            .unwrap_or(0) as u32;
        let wanted: BTreeSet<&str> = members.iter().copied().collect();
        let mut found: Vec<MemberLayout> = Vec::new();
        self.collect(unit, offset, Nest::root(), &wanted, &mut found);
        // Requested order, so the rendering does not depend on DWARF order.
        let mut ordered = Vec::new();
        for path in members {
            match found.iter().find(|m| m.path == *path) {
                Some(m) => ordered.push(m.clone()),
                None => out.note_missing_member(name, path),
            }
        }
        Some(StructLayout::new(name, size, ordered))
    }

    /// Walks the members of the structure at `offset`, appending every match of `wanted` with
    /// its offset from the outermost structure. An unnamed member is a C11 anonymous structure or
    /// union whose members belong to the enclosing one, so it does not extend the path.
    fn collect(
        &self,
        unit: &gimli::Unit<Slice<'a>>,
        offset: UnitOffset,
        at: Nest<'_>,
        wanted: &BTreeSet<&str>,
        out: &mut Vec<MemberLayout>,
    ) {
        let Nest {
            prefix,
            base,
            depth,
        } = at;
        if depth > MAX_NESTING {
            return;
        }
        let Ok(mut tree) = unit.entries_tree(Some(offset)) else {
            return;
        };
        let Ok(root) = tree.root() else { return };
        let mut children = root.children();
        // The tree cursor cannot be held across the recursive descent, so nested members
        // are collected first and followed afterwards.
        let mut nested: Vec<(String, u64, UnitOffset)> = Vec::new();
        while let Ok(Some(child)) = children.next() {
            let entry = child.entry();
            if entry.tag() != gimli::DW_TAG_member {
                continue;
            }
            let num = |at| {
                entry
                    .attr_value(at)
                    .and_then(|v: AttributeValue<Slice<'a>>| {
                        v.udata_value()
                            .map(|x| x as i64)
                            .or_else(|| v.sdata_value())
                    })
            };
            let loc = num(gimli::DW_AT_data_member_location).unwrap_or(0);
            let here = base.wrapping_add(loc as u64);
            let name = self.name_of(unit, entry);
            let path = match &name {
                Some(n) if prefix.is_empty() => n.clone(),
                Some(n) => format!("{prefix}.{n}"),
                None => prefix.to_string(),
            };
            if name.is_some() && wanted.contains(path.as_str()) {
                let size = num(gimli::DW_AT_byte_size)
                    .or_else(|| self.type_byte_size(unit, entry.offset()))
                    .and_then(|s| u32::try_from(s).ok());
                out.push(MemberLayout {
                    size,
                    ..member_layout(
                        path.clone(),
                        here,
                        num(gimli::DW_AT_bit_size),
                        num(gimli::DW_AT_data_bit_offset),
                        num(gimli::DW_AT_bit_offset),
                        num(gimli::DW_AT_byte_size),
                        loc,
                    )
                });
                continue;
            }
            let descend = name.is_none()
                || wanted
                    .iter()
                    .any(|w| w.starts_with(&path) && w.as_bytes().get(path.len()) == Some(&b'.'));
            if descend && let Some(t) = self.follow_type(unit, entry.offset()) {
                nested.push((path, here, t));
            }
        }
        for (path, here, target) in nested {
            let prefix = if path.is_empty() { prefix } else { &path };
            let at = Nest {
                prefix,
                base: here,
                depth: depth + 1,
            };
            self.collect(unit, target, at, wanted, out);
        }
    }

    /// `DW_AT_byte_size` of the DIE's type, following typedefs and qualifiers to the first type
    /// that states a size.
    fn type_byte_size(&self, unit: &gimli::Unit<Slice<'a>>, offset: UnitOffset) -> Option<i64> {
        let mut at = offset;
        for _ in 0..MAX_NESTING {
            let entry = unit.entry(at).ok()?;
            if at != offset
                && let Some(size) = entry
                    .attr_value(gimli::DW_AT_byte_size)
                    .and_then(|v| v.udata_value())
            {
                return Some(size as i64);
            }
            match entry.attr_value(gimli::DW_AT_type)? {
                AttributeValue::UnitRef(next) => at = next,
                _ => return None,
            }
        }
        None
    }

    fn name_of(
        &self,
        unit: &gimli::Unit<Slice<'a>>,
        entry: &gimli::DebuggingInformationEntry<Slice<'a>>,
    ) -> Option<String> {
        let value = entry.attr_value(gimli::DW_AT_name)?;
        let s = self.dwarf.attr_string(unit, value).ok()?;
        Some(s.to_string_lossy().into_owned())
    }

    /// Follows `DW_AT_type` through typedefs and qualifiers to the structure or union it
    /// names.
    fn follow_type(&self, unit: &gimli::Unit<Slice<'a>>, offset: UnitOffset) -> Option<UnitOffset> {
        let mut at = offset;
        for _ in 0..MAX_NESTING {
            let entry = unit.entry(at).ok()?;
            let tag = entry.tag();
            if at != offset
                && (tag == gimli::DW_TAG_structure_type || tag == gimli::DW_TAG_union_type)
            {
                return Some(at);
            }
            match entry.attr_value(gimli::DW_AT_type)? {
                AttributeValue::UnitRef(next) => at = next,
                _ => return None,
            }
        }
        None
    }

    /// The line and column [`statement_row`] picks for `pc`.
    fn statement_line_at(&self, pc: u32) -> Option<(u32, Option<u32>)> {
        let unit = self
            .ctx
            .find_dwarf_and_unit(u64::from(pc))
            .skip_all_loads()?;
        let program = unit.line_program.clone()?;
        let mut rows = program.rows();
        let mut read = Vec::new();
        while let Ok(Some((_, row))) = rows.next_row() {
            read.push(LineRow {
                address: row.address(),
                end_sequence: row.end_sequence(),
                is_stmt: row.is_stmt(),
                file: row.file_index(),
                line: row.line().map_or(0, |l| l.get()),
                column: match row.column() {
                    gimli::ColumnType::LeftEdge => 0,
                    gimli::ColumnType::Column(c) => c.get(),
                },
            });
        }
        let row = statement_row(&read, u64::from(pc))?;
        let line = u32::try_from(row.line).ok().filter(|&l| l != 0)?;
        Some((line, u32::try_from(row.column).ok().filter(|&c| c != 0)))
    }

    /// The inline-aware frames at `pc`, innermost first. An address with no line-table coverage,
    /// such as a ROM routine, yields none.
    pub fn frames_at(&self, pc: u32) -> Vec<InlineFrame> {
        let mut out: Vec<InlineFrame> = Vec::new();
        let Ok(mut frames) = self.ctx.find_frames(u64::from(pc)).skip_all_loads() else {
            return out;
        };
        while let Ok(Some(f)) = frames.next() {
            let function = f
                .function
                .as_ref()
                .and_then(|n| n.raw_name().ok())
                .map(|n| n.into_owned());
            let (file, line, column) = match &f.location {
                Some(l) => (
                    l.file.map(str::to_string),
                    l.line,
                    l.column.filter(|&c| c != 0),
                ),
                None => (None, None, None),
            };
            out.push(InlineFrame {
                function,
                file,
                line,
                column,
                inlined: true,
            });
        }
        // Inlined callers carry their `DW_AT_call_line`; only the innermost frame is
        // re-read with the statement rule.
        if let Some(first) = out.first_mut()
            && let Some((line, column)) = self.statement_line_at(pc)
        {
            first.line = Some(line);
            first.column = column;
        }
        if let Some(last) = out.last_mut() {
            last.inlined = false;
        }
        out
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct LineRow {
    pub address: u64,
    /// `DW_LNE_end_sequence`; its address is one past the last byte.
    pub end_sequence: bool,
    pub is_stmt: bool,
    pub file: u64,
    /// 0 when the row has no line.
    pub line: u64,
    /// 0 for "left edge".
    pub column: u64,
}

/// The row a debugger (`riscv32-esp-elf-gdb bt`) reports for `pc`: among the rows at the
/// greatest address not above `pc` in its sequence, the last one when it is a statement, else
/// the last statement row there in the same file, else the last row. GCC emits a non-statement
/// row after a statement row when two source lines share the first instruction (a `-Og` load
/// such as `probes/probe_panic`'s `return *ptr`); `addr2line` would take the last row.
#[must_use]
pub fn statement_row(rows: &[LineRow], pc: u64) -> Option<LineRow> {
    let mut group: Vec<LineRow> = Vec::new();
    for row in rows {
        if row.address > pc {
            if !group.is_empty() {
                return pick_statement(&group);
            }
        } else if row.end_sequence {
            group.clear();
        } else if group.last().is_some_and(|last| last.address == row.address) {
            group.push(*row);
        } else {
            group.clear();
            group.push(*row);
        }
    }
    None
}

fn pick_statement(group: &[LineRow]) -> Option<LineRow> {
    let last = *group.last()?;
    if last.is_stmt {
        return Some(last);
    }
    Some(
        group
            .iter()
            .rev()
            .find(|row| row.is_stmt && row.file == last.file)
            .copied()
            .unwrap_or(last),
    )
}

/// Turns the DWARF bitfield attributes into little-endian `byte:bit/width`. DWARF 5
/// `DW_AT_data_bit_offset` counts from the structure start. DWARF 4 `DW_AT_bit_offset` counts
/// from the most significant bit of the storage unit at `DW_AT_data_member_location`, sized by
/// `DW_AT_byte_size`, so the least significant bit is at
/// `location*8 + unit_bits - bit_offset - bit_size`.
fn member_layout(
    path: String,
    base: u64,
    bit_size: Option<i64>,
    data_bit_offset: Option<i64>,
    bit_offset: Option<i64>,
    unit_bytes: Option<i64>,
    location: i64,
) -> MemberLayout {
    let absolute_bit = match (bit_size, data_bit_offset, bit_offset, unit_bytes) {
        // DWARF 5 already includes the member location.
        (Some(_), Some(d), _, _) => Some((base as i64 - location) * 8 + d),
        (Some(w), None, Some(o), Some(s)) => Some(base as i64 * 8 + s * 8 - o - w),
        _ => None,
    };
    match (bit_size, absolute_bit) {
        (Some(width), Some(bit)) if bit >= 0 && width > 0 => MemberLayout {
            path,
            offset: (bit / 8) as u32,
            bits: Some(Bitfield {
                bit: (bit % 8) as u8,
                width: width as u32,
            }),
            size: None,
        },
        _ => MemberLayout {
            path,
            offset: data_bit_offset
                .filter(|_| bit_size.is_none())
                .map(|d| ((base as i64 - location) * 8 + d) / 8)
                .unwrap_or(base as i64) as u32,
            bits: None,
            size: None,
        },
    }
}

/// Forward slashes, relative to the build root when one is known, so macOS and Windows name
/// the same location.
pub fn normalize_path(path: &str, build_root: Option<&str>) -> String {
    let slashed = path.replace('\\', "/");
    let root = build_root.map(|r| r.replace('\\', "/"));
    match root {
        Some(r) if !r.is_empty() => {
            let r = r.trim_end_matches('/');
            match slashed
                .strip_prefix(r)
                .and_then(|rest| rest.strip_prefix('/'))
            {
                Some(rest) => rest.to_string(),
                None => slashed,
            }
        }
        _ => slashed,
    }
}

/// The address of a statically allocated variable. Only a bare `DW_OP_addr <address>` is
/// accepted: a location list or other expression is not one fixed object a page watch could arm
/// over.
fn static_address(entry: &gimli::DebuggingInformationEntry<Slice<'_>>) -> Option<u32> {
    const DW_OP_ADDR: u8 = 0x03;
    let value: AttributeValue<Slice<'_>> = entry.attr_value(gimli::DW_AT_location)?;
    let AttributeValue::Exprloc(expr) = value else {
        return None;
    };
    let bytes: &[u8] = expr.0.slice();
    if bytes.len() != 5 || bytes[0] != DW_OP_ADDR {
        return None;
    }
    Some(u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: u64, is_stmt: bool, line: u64) -> LineRow {
        LineRow {
            address,
            end_sequence: false,
            is_stmt,
            file: 1,
            line,
            column: 0,
        }
    }

    #[test]
    fn a_statement_row_wins_over_a_later_non_statement_row_at_the_same_address() {
        // `probes/probe_panic` `probe_panic_read_null`: lines 49 (statement) and 50 (not)
        // both start at 0x42006f52, and the debugger reports 49.
        let end = LineRow {
            end_sequence: true,
            ..row(0x42006f56, false, 0)
        };
        let rows = [
            row(0x42006f4a, true, 47),
            row(0x42006f4a, true, 48),
            row(0x42006f4a, false, 48),
            row(0x42006f52, true, 49),
            row(0x42006f52, false, 50),
            end,
        ];
        assert_eq!(statement_row(&rows, 0x42006f52).map(|r| r.line), Some(49));
        assert_eq!(statement_row(&rows, 0x42006f54).map(|r| r.line), Some(49));
        assert_eq!(statement_row(&rows, 0x42006f4c).map(|r| r.line), Some(48));
        assert_eq!(statement_row(&rows, 0x42006f56), None);
        // A group of non-statement rows alone keeps the last one.
        let rows = [row(0x10, false, 3), row(0x10, false, 4), row(0x20, true, 5)];
        assert_eq!(statement_row(&rows, 0x18).map(|r| r.line), Some(4));
        // A statement row in another file is not taken.
        let other = LineRow {
            file: 2,
            ..row(0x10, true, 9)
        };
        let rows = [other, row(0x10, false, 4), row(0x20, true, 5)];
        assert_eq!(statement_row(&rows, 0x10).map(|r| r.line), Some(4));
    }

    /// `_lv_obj_t.scr_layout_inv` is `42:2/1`. GCC `-gdwarf-4` writes location 40, a 4-byte unit
    /// and `DW_AT_bit_offset` 13: 40*8 + 32 - 13 - 1 = 338. DWARF 5 writes 338 outright.
    #[test]
    fn dwarf4_msb_relative_and_dwarf5_bit_offsets_agree() {
        let four = member_layout(
            "scr_layout_inv".into(),
            40,
            Some(1),
            None,
            Some(13),
            Some(4),
            40,
        );
        let five = member_layout(
            "scr_layout_inv".into(),
            40,
            Some(1),
            Some(338),
            None,
            None,
            40,
        );
        assert_eq!(four.render(), "42:2/1");
        assert_eq!(five.render(), "42:2/1");
        assert_eq!(four, five);
    }

    #[test]
    fn wide_and_byte_aligned_bitfields_match_the_spike() {
        let instance = member_layout(
            "instance_size".into(),
            32,
            Some(16),
            None,
            Some(12),
            Some(4),
            32,
        );
        assert_eq!(instance.render(), "32:4/16");
        let long_mode = member_layout("long_mode".into(), 96, Some(4), None, Some(28), Some(4), 96);
        assert_eq!(long_mode.render(), "96:0/4");
        let src_type = member_layout("src_type".into(), 92, Some(2), None, Some(30), Some(4), 92);
        assert_eq!(src_type.render(), "92:0/2");
    }

    #[test]
    fn plain_and_nested_members_use_the_accumulated_offset() {
        let plain = member_layout("coords".into(), 20, None, None, None, None, 20);
        assert_eq!(plain.render(), "20");
        assert_eq!(plain.bits, None);
        let nested = member_layout(
            "u.xSemaphore.xMutexHolder".into(),
            8,
            None,
            None,
            None,
            None,
            8,
        );
        assert_eq!(nested.render(), "8");
        // A DWARF 5 producer may give a plain member `DW_AT_data_bit_offset` instead.
        let dwarf5 = member_layout("free_bytes".into(), 4, None, Some(32), None, None, 4);
        assert_eq!(dwarf5.render(), "4");
    }

    #[test]
    fn paths_normalize_to_forward_slashes_relative_to_the_build_root() {
        assert_eq!(
            normalize_path("/build/main/main.c", Some("/build")),
            "main/main.c"
        );
        assert_eq!(
            normalize_path("C:\\build\\main\\main.c", Some("C:\\build")),
            "main/main.c"
        );
        assert_eq!(
            normalize_path("/opt/idf/components/lvgl/lv_obj.c", Some("/build")),
            "/opt/idf/components/lvgl/lv_obj.c"
        );
        assert_eq!(normalize_path("main.c", None), "main.c");
    }
}
