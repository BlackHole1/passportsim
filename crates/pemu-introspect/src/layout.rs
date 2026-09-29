//! The guest memory view and the structure layouts read through it.
//!
//! [`GuestMemory`] is the read-only view: the running core's arena, or a [`MemoryImage`] in tests.
//! Reads are little-endian, as the ESP32-C3 is. A layout is a member's byte offset plus, for a
//! bitfield, the position of its least significant bit in that byte and its width, rendered
//! `byte:bit/width`. Offsets come from the guest's DWARF ([`crate::dwarf`]) because LVGL Kconfig
//! options change them per build.

use core::fmt;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::IntrospectError;

/// A guest read that could not be served.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemError {
    pub addr: u32,
    pub len: u32,
}

impl fmt::Display for MemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let MemError { addr, len } = self;
        write!(f, "guest memory {addr:#010x}..+{len} is not readable")
    }
}

impl std::error::Error for MemError {}

/// Read-only access to guest memory at one stopped point in time. Reads must be side-effect
/// free, so a rendered walk is reproducible.
pub trait GuestMemory {
    /// Fills `out` from guest address `addr`, or fails without writing a partial result.
    fn read(&self, addr: u32, out: &mut [u8]) -> Result<(), MemError>;

    fn u8(&self, addr: u32) -> Result<u8, MemError> {
        let mut b = [0u8; 1];
        self.read(addr, &mut b)?;
        Ok(b[0])
    }

    fn u16(&self, addr: u32) -> Result<u16, MemError> {
        let mut b = [0u8; 2];
        self.read(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    /// Also how every guest pointer is read.
    fn u32(&self, addr: u32) -> Result<u32, MemError> {
        let mut b = [0u8; 4];
        self.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn i32(&self, addr: u32) -> Result<i32, MemError> {
        Ok(self.u32(addr)? as i32)
    }

    fn bytes(&self, addr: u32, len: usize) -> Result<Vec<u8>, MemError> {
        let mut out = vec![0u8; len];
        self.read(addr, &mut out)?;
        Ok(out)
    }

    /// A NUL-terminated C string of at most `max` bytes, decoded lossily. Hitting `max` or
    /// unmapped memory truncates rather than fails: a pointer into a half-written structure is what
    /// the walkers' corruption reports are for.
    fn cstr(&self, addr: u32, max: usize) -> String {
        let mut out = Vec::new();
        for i in 0..max {
            match self.u8(addr.wrapping_add(i as u32)) {
                Ok(0) | Err(_) => break,
                Ok(b) => out.push(b),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn is_mapped(&self, addr: u32) -> bool {
        self.u8(addr).is_ok()
    }
}

impl<T: GuestMemory + ?Sized> GuestMemory for &T {
    fn read(&self, addr: u32, out: &mut [u8]) -> Result<(), MemError> {
        (**self).read(addr, out)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Region {
    base: u32,
    bytes: Vec<u8>,
}

/// A guest memory image assembled from disjoint spans, for tests and ELF-section reads.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MemoryImage {
    /// Sorted by base address.
    regions: Vec<Region>,
}

impl MemoryImage {
    pub fn new() -> MemoryImage {
        MemoryImage::default()
    }

    /// Maps `bytes` at `base`, replacing any overlap. Spans stay sorted, so two images built by the
    /// same calls compare equal.
    pub fn map(&mut self, base: u32, bytes: impl Into<Vec<u8>>) -> &mut MemoryImage {
        let bytes = bytes.into();
        let end = u64::from(base) + bytes.len() as u64;
        self.regions.retain(|r| {
            u64::from(r.base) + r.bytes.len() as u64 <= u64::from(base) || u64::from(r.base) >= end
        });
        let at = self.regions.partition_point(|r| r.base < base);
        self.regions.insert(at, Region { base, bytes });
        self
    }

    pub fn map_zeroed(&mut self, base: u32, len: usize) -> &mut MemoryImage {
        self.map(base, vec![0u8; len])
    }

    /// Writes a little-endian word into an already mapped span, or does nothing.
    pub fn put_u32(&mut self, addr: u32, value: u32) -> &mut MemoryImage {
        self.put(addr, &value.to_le_bytes())
    }

    /// Writes `bytes` into already mapped spans; bytes outside every span are dropped.
    pub fn put(&mut self, addr: u32, bytes: &[u8]) -> &mut MemoryImage {
        for (i, b) in bytes.iter().enumerate() {
            let a = addr.wrapping_add(i as u32);
            for r in &mut self.regions {
                let off = u64::from(a).wrapping_sub(u64::from(r.base));
                if off < r.bytes.len() as u64 {
                    r.bytes[off as usize] = *b;
                }
            }
        }
        self
    }

    pub fn mapped_len(&self) -> usize {
        self.regions.iter().map(|r| r.bytes.len()).sum()
    }
}

impl GuestMemory for MemoryImage {
    fn read(&self, addr: u32, out: &mut [u8]) -> Result<(), MemError> {
        let err = MemError {
            addr,
            len: out.len() as u32,
        };
        let end = u64::from(addr) + out.len() as u64;
        let at = self.regions.partition_point(|r| r.base <= addr);
        let r = self
            .regions
            .get(at.checked_sub(1).ok_or_else(|| err.clone())?)
            .ok_or_else(|| err.clone())?;
        let start = (addr - r.base) as usize;
        if u64::from(r.base) + r.bytes.len() as u64 >= end {
            out.copy_from_slice(&r.bytes[start..start + out.len()]);
            Ok(())
        } else {
            Err(err)
        }
    }
}

/// Structures and members the walkers resolve, as (C struct name, member paths). A dotted path
/// reaches through a nested structure or union. All are optional at run time: [`Layouts`] records
/// what was missing, so a firmware without LVGL still yields FreeRTOS layouts.
pub const LAYOUT_REQUESTS: &[(&str, &[&str])] = &[
    // LVGL 9.5.0.
    (
        "_lv_obj_t",
        &[
            "class_p",
            "parent",
            "spec_attr",
            "styles",
            "user_data",
            "coords",
            "flags",
            "state",
            "layout_inv",
            "scr_layout_inv",
            "style_cnt",
            "is_deleting",
        ],
    ),
    ("_lv_obj_spec_attr_t", &["children", "child_cnt", "scroll"]),
    ("_lv_obj_class_t", &["base_class", "name", "instance_size"]),
    ("_lv_obj_style_t", &["style", "selector", "is_local"]),
    ("lv_style_t", &["values_and_props", "prop_cnt"]),
    (
        "_lv_display_t",
        &[
            "hor_res",
            "ver_res",
            "screens",
            "sys_layer",
            "top_layer",
            "act_scr",
            "bottom_layer",
            "prev_scr",
            "scr_to_load",
            "screen_cnt",
            "rendering_in_progress",
            "inv_p",
        ],
    ),
    (
        "_lv_global_t",
        &[
            "inited",
            "disp_ll",
            "disp_refresh",
            "disp_default",
            "indev_ll",
            "tick_state",
            "tlsf_state",
        ],
    ),
    ("_lv_label_t", &["text", "long_mode"]),
    ("_lv_image_t", &["src", "w", "h", "src_type"]),
    ("_lv_bar_t", &["cur_value", "min_value", "max_value"]),
    // FreeRTOS.
    (
        "tskTaskControlBlock",
        &[
            "pxTopOfStack",
            "xStateListItem",
            "xEventListItem",
            "uxPriority",
            "pxStack",
            "pcTaskName",
            "pxEndOfStack",
            "uxBasePriority",
            // `eTaskGetState` reads it to report a task waiting on a notification as blocked.
            "ucNotifyState",
        ],
    ),
    ("xLIST", &["uxNumberOfItems", "pxIndex", "xListEnd"]),
    (
        "xLIST_ITEM",
        &[
            "xItemValue",
            "pxNext",
            "pxPrevious",
            "pvOwner",
            "pxContainer",
        ],
    ),
    (
        "QueueDefinition",
        &[
            // `pcHead` (`uxQueueType`) is NULL in a mutex and nowhere else, which tells a mutex
            // from a queue whose union holds something else.
            "pcHead",
            "u.xSemaphore.xMutexHolder",
            "u.xSemaphore.uxRecursiveCallCount",
            "xTasksWaitingToSend",
            "xTasksWaitingToReceive",
        ],
    ),
    // Heap.
    (
        "heap_t_",
        &["caps", "start", "end", "heap_mux", "heap", "next"],
    ),
    (
        "multi_heap_info",
        &[
            "lock",
            "free_bytes",
            "minimum_free_bytes",
            "pool_size",
            "heap_data",
        ],
    ),
    (
        "control_t",
        &[
            "size",
            "sl_index_count",
            "small_block_size",
            "fl_index_count",
        ],
    ),
    ("block_header_t", &["prev_phys_block", "size"]),
    // esp_lvgl_port safe points.
    ("lvgl_port_ctx_t", &["lvgl_task", "lvgl_mux"]),
    // Panic decoding: the RISC-V exception frame ESP-IDF hands `esp_panic_handler`.
    (
        "RvExcFrame",
        &["mepc", "ra", "sp", "s0", "mstatus", "mcause", "mtval"],
    ),
    // `esp_panic_handler`'s argument: `frame` points at the `RvExcFrame` above, and
    // `exception` and `reason` say which panic path it was.
    ("panic_info_t", &["exception", "reason", "addr", "frame"]),
];

/// Bit position and width of a bitfield member: `bit` is that of the field's least significant
/// bit inside the byte at the member's offset.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Bitfield {
    /// 0 to 7.
    pub bit: u8,
    pub width: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemberLayout {
    /// Dotted for a nested member.
    pub path: String,
    pub offset: u32,
    pub bits: Option<Bitfield>,
    /// `DW_AT_byte_size` of the member, or of its type through typedefs and qualifiers, when
    /// DWARF gives one.
    pub size: Option<u32>,
}

impl MemberLayout {
    /// `offset` or `offset:bit/width`.
    pub fn render(&self) -> String {
        match self.bits {
            Some(Bitfield { bit, width }) => format!("{}:{bit}/{width}", self.offset),
            None => self.offset.to_string(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StructLayout {
    pub name: String,
    pub size: u32,
    /// In request order, so the rendering is stable.
    members: Vec<MemberLayout>,
}

impl StructLayout {
    /// Builds a layout directly, for tests and for known offsets when an ELF has no debug
    /// information.
    pub fn new(name: &str, size: u32, members: Vec<MemberLayout>) -> StructLayout {
        StructLayout {
            name: name.to_string(),
            size,
            members,
        }
    }

    pub fn members(&self) -> &[MemberLayout] {
        &self.members
    }

    pub fn member(&self, path: &str) -> Option<&MemberLayout> {
        self.members.iter().find(|m| m.path == path)
    }

    /// Byte offset of a member the caller cannot proceed without.
    pub fn offset(&self, path: &'static str) -> Result<u32, IntrospectError> {
        self.member(path)
            .map(|m| m.offset)
            .ok_or(IntrospectError::MissingMember {
                name: leak(&self.name),
                member: path,
            })
    }

    pub fn bitfield(&self, path: &'static str) -> Result<Bitfield, IntrospectError> {
        self.member(path)
            .and_then(|m| m.bits)
            .ok_or(IntrospectError::MissingMember {
                name: leak(&self.name),
                member: path,
            })
    }

    pub fn u32(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<u32, IntrospectError> {
        Ok(mem.u32(base.wrapping_add(self.offset(path)?))?)
    }

    /// Reads an unsigned integer member at the width DWARF declares: a bitfield by its bits, a
    /// plain member by its byte size (1, 2 or 4). Any other or missing size is a layout mismatch.
    pub fn uint(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<u32, IntrospectError> {
        let member = self.member(path).ok_or(IntrospectError::MissingMember {
            name: leak(&self.name),
            member: path,
        })?;
        if member.bits.is_some() {
            return self.bits(mem, base, path);
        }
        match member.size {
            Some(1) => Ok(u32::from(self.u8(mem, base, path)?)),
            Some(2) => Ok(u32::from(self.u16(mem, base, path)?)),
            Some(4) => self.u32(mem, base, path),
            other => Err(IntrospectError::LayoutMismatch {
                what: "integer member",
                detail: format!("{}.{path} has byte size {other:?}", self.name),
            }),
        }
    }

    pub fn u16(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<u16, IntrospectError> {
        Ok(mem.u16(base.wrapping_add(self.offset(path)?))?)
    }

    pub fn u8(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<u8, IntrospectError> {
        Ok(mem.u8(base.wrapping_add(self.offset(path)?))?)
    }

    pub fn i32(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<i32, IntrospectError> {
        Ok(self.u32(mem, base, path)? as i32)
    }

    /// Reads a bitfield member as an unsigned value. A field wider than its byte, such as the
    /// 16-bit `instance_size` of `_lv_obj_class_t` at `32:4/16`, is read from the enclosing half
    /// word or word.
    pub fn bits(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<u32, IntrospectError> {
        let at = base.wrapping_add(self.offset(path)?);
        let Bitfield { bit, width } = self.bitfield(path)?;
        let total = u32::from(bit) + width;
        let raw = if total <= 8 {
            u32::from(mem.u8(at)?)
        } else if total <= 16 {
            u32::from(mem.u16(at)?)
        } else if total <= 32 {
            mem.u32(at)?
        } else {
            return Err(IntrospectError::LayoutMismatch {
                what: "bitfield",
                detail: format!("{}.{path} spans {total} bits from its byte", self.name),
            });
        };
        let mask = if width >= 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        Ok((raw >> bit) & mask)
    }

    pub fn flag(
        &self,
        mem: &dyn GuestMemory,
        base: u32,
        path: &'static str,
    ) -> Result<bool, IntrospectError> {
        Ok(self.bits(mem, base, path)? != 0)
    }

    /// `size=<n> <member>=<layout> ...`.
    pub fn render(&self) -> String {
        let mut out = format!("size={}", self.size);
        for m in &self.members {
            out.push(' ');
            out.push_str(&m.path);
            out.push('=');
            out.push_str(&m.render());
        }
        out
    }
}

/// Every structure resolved from one ELF, with the requests that could not be answered.
/// Missing parts are recorded, not an error: firmware need not link LVGL, FreeRTOS and the heap
/// at once, and `inspect` still answers for what is present.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Layouts {
    structs: BTreeMap<String, StructLayout>,
    missing_structs: Vec<String>,
    missing_members: Vec<String>,
}

impl Layouts {
    pub fn new() -> Layouts {
        Layouts::default()
    }

    /// Adds a structure. A second definition of the same name is ignored: a C structure defined
    /// in many compilation units yields many identical DWARF entries.
    pub fn insert(&mut self, layout: StructLayout) -> &mut Layouts {
        if let Entry::Vacant(e) = self.structs.entry(layout.name.clone()) {
            e.insert(layout);
        }
        self
    }

    pub(crate) fn note_missing_struct(&mut self, name: &str) {
        self.missing_structs.push(name.to_string());
    }

    pub(crate) fn note_missing_member(&mut self, name: &str, path: &str) {
        self.missing_members.push(format!("{name}.{path}"));
    }

    pub fn iter(&self) -> impl Iterator<Item = &StructLayout> {
        self.structs.values()
    }

    pub fn get(&self, name: &str) -> Option<&StructLayout> {
        self.structs.get(name)
    }

    pub fn require(&self, name: &'static str) -> Result<&StructLayout, IntrospectError> {
        self.structs
            .get(name)
            .ok_or(IntrospectError::MissingStruct { name })
    }

    pub fn is_complete(&self) -> bool {
        self.missing_structs.is_empty() && self.missing_members.is_empty()
    }

    /// In request order.
    pub fn missing_structs(&self) -> &[String] {
        &self.missing_structs
    }

    /// In request order.
    pub fn missing_members(&self) -> &[String] {
        &self.missing_members
    }

    /// One `<struct> size=... member=...` line per resolved structure, in name order.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for s in self.structs.values() {
            out.push_str(&s.name);
            out.push(' ');
            out.push_str(&s.render());
            out.push('\n');
        }
        out
    }

    /// The layout-only LVGL sanity checks: a plausible `_lv_obj_t` size and the bitfields the
    /// safe-point predicate reads. The checks that need guest memory run in [`crate::lvgl`].
    pub fn check_lvgl(&self) -> Result<(), IntrospectError> {
        let obj = self.require("_lv_obj_t")?;
        if !(40..=96).contains(&obj.size) {
            return Err(IntrospectError::LayoutMismatch {
                what: "_lv_obj_t size",
                detail: format!("{} is outside 40..=96", obj.size),
            });
        }
        obj.bitfield("scr_layout_inv")?;
        obj.bitfield("layout_inv")?;
        self.require("_lv_display_t")?
            .bitfield("rendering_in_progress")?;
        Ok(())
    }
}

/// The `&'static str` the error variants carry. Every name a walker asks for is a literal of
/// [`LAYOUT_REQUESTS`]; any other name degrades to a constant rather than leaking memory.
fn leak(name: &str) -> &'static str {
    LAYOUT_REQUESTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, _)| *n)
        .unwrap_or("struct")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryImage;

    fn member(path: &str, offset: u32, bits: Option<Bitfield>) -> MemberLayout {
        MemberLayout {
            path: path.to_string(),
            offset,
            bits,
            size: None,
        }
    }

    #[test]
    fn uint_reads_a_member_at_its_dwarf_width() {
        let mut mem = MemoryImage::new();
        mem.map(0x1000, 0x03c3_0027u32.to_le_bytes().to_vec());
        let sized = |path: &str, size: Option<u32>| MemberLayout {
            size,
            ..member(path, 0, None)
        };
        let layout = StructLayout::new(
            "s",
            4,
            vec![
                sized("one", Some(1)),
                sized("two", Some(2)),
                sized("four", Some(4)),
                sized("unknown", None),
                member("bits", 0, Some(Bitfield { bit: 0, width: 16 })),
            ],
        );
        assert_eq!(layout.uint(&mem, 0x1000, "one"), Ok(0x27));
        assert_eq!(layout.uint(&mem, 0x1000, "two"), Ok(0x0027));
        assert_eq!(layout.uint(&mem, 0x1000, "four"), Ok(0x03c3_0027));
        assert_eq!(layout.uint(&mem, 0x1000, "bits"), Ok(0x0027));
        assert!(layout.uint(&mem, 0x1000, "unknown").is_err());
    }

    #[test]
    fn render_uses_the_byte_bit_width_form() {
        let obj = StructLayout::new(
            "_lv_obj_t",
            48,
            vec![
                member("class_p", 0, None),
                member("state", 40, None),
                member("scr_layout_inv", 42, Some(Bitfield { bit: 2, width: 1 })),
            ],
        );
        assert_eq!(
            obj.render(),
            "size=48 class_p=0 state=40 scr_layout_inv=42:2/1"
        );
        let class = StructLayout::new(
            "_lv_obj_class_t",
            36,
            vec![member(
                "instance_size",
                32,
                Some(Bitfield { bit: 4, width: 16 }),
            )],
        );
        assert_eq!(class.render(), "size=36 instance_size=32:4/16");
    }

    #[test]
    fn bitfield_reads_narrow_and_wide_fields() {
        let mut m = MemoryImage::new();
        m.map(0x1000, vec![0u8; 48]);
        // byte 42 = 0b0101_0100: layout_inv 0, scr_layout_inv 1, style_cnt 5.
        m.put(0x102a, &[0b0101_0100]);
        // instance_size at 32:4/16 = 48 -> word 0x0000_0300 at offset 32.
        m.put_u32(0x1020, 48 << 4);
        let obj = StructLayout::new(
            "_lv_obj_t",
            48,
            vec![
                member("layout_inv", 42, Some(Bitfield { bit: 0, width: 1 })),
                member("scr_layout_inv", 42, Some(Bitfield { bit: 2, width: 1 })),
                member("style_cnt", 42, Some(Bitfield { bit: 4, width: 6 })),
                member("instance_size", 32, Some(Bitfield { bit: 4, width: 16 })),
            ],
        );
        assert_eq!(obj.flag(&m, 0x1000, "layout_inv"), Ok(false));
        assert_eq!(obj.flag(&m, 0x1000, "scr_layout_inv"), Ok(true));
        assert_eq!(obj.bits(&m, 0x1000, "style_cnt"), Ok(5));
        assert_eq!(obj.bits(&m, 0x1000, "instance_size"), Ok(48));
    }

    #[test]
    fn missing_members_and_structs_are_named_not_guessed() {
        let mut l = Layouts::new();
        l.insert(StructLayout::new(
            "_lv_obj_t",
            48,
            vec![member("class_p", 0, None)],
        ));
        l.note_missing_struct("_lv_bar_t");
        l.note_missing_member("_lv_obj_t", "spec_attr");
        assert!(!l.is_complete());
        assert_eq!(l.missing_structs(), ["_lv_bar_t"]);
        assert_eq!(l.missing_members(), ["_lv_obj_t.spec_attr"]);
        assert_eq!(
            l.require("_lv_display_t"),
            Err(IntrospectError::MissingStruct {
                name: "_lv_display_t"
            })
        );
        assert_eq!(
            l.require("_lv_obj_t").unwrap().offset("spec_attr"),
            Err(IntrospectError::MissingMember {
                name: "_lv_obj_t",
                member: "spec_attr"
            })
        );
        assert_eq!(l.render(), "_lv_obj_t size=48 class_p=0\n");
        let mut bad = Layouts::new();
        bad.insert(StructLayout::new("_lv_obj_t", 8, vec![]));
        assert!(matches!(
            bad.check_lvgl(),
            Err(IntrospectError::LayoutMismatch {
                what: "_lv_obj_t size",
                ..
            })
        ));
    }

    #[test]
    fn image_reads_words_strings_and_reports_unmapped() {
        let mut m = MemoryImage::new();
        m.map(0x3fc8_0000, vec![0u8; 16]);
        m.map(0x3fc9_0000, b"task\0extra".to_vec());
        m.put_u32(0x3fc8_0004, 0xdead_beef);
        assert_eq!(m.u32(0x3fc8_0004), Ok(0xdead_beef));
        assert_eq!(m.u16(0x3fc8_0004), Ok(0xbeef));
        assert_eq!(m.u8(0x3fc8_0007), Ok(0xde));
        assert_eq!(m.i32(0x3fc8_0004), Ok(-559_038_737));
        assert_eq!(m.cstr(0x3fc9_0000, 16), "task");
        assert_eq!(m.mapped_len(), 26);
        assert_eq!(
            m.read(0x3fc8_000e, &mut [0u8; 4]),
            Err(MemError {
                addr: 0x3fc8_000e,
                len: 4
            })
        );
        assert!(!m.is_mapped(0x3fc8_0010));
        assert!(!m.is_mapped(0x0000_0000));
    }

    #[test]
    fn overlapping_maps_replace_and_stay_sorted() {
        let mut m = MemoryImage::new();
        m.map(0x100, vec![1u8; 8]);
        m.map(0x200, vec![2u8; 8]);
        m.map(0x100, vec![3u8; 8]);
        assert_eq!(m.mapped_len(), 16);
        assert_eq!(m.u8(0x100), Ok(3));
        assert_eq!(m.u8(0x200), Ok(2));
        let bases: Vec<u32> = m.regions.iter().map(|r| r.base).collect();
        assert_eq!(bases, vec![0x100, 0x200]);
        m.put(0x400, &[9]);
        assert_eq!(m.mapped_len(), 16);
    }
}
