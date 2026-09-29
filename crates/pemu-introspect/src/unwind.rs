//! Guest stack unwinding from `.debug_frame` CFI, with ROM frames symbolized from the ROM ELF.
//!
//! ESP-IDF builds carry `.debug_frame`, not `.eh_frame`. Each step evaluates the CFI row for the
//! PC: the CFA from the row's CFA rule, the caller's return address from the rule for `x1`, and
//! the caller's stack pointer is the CFA. A restored return address of zero is the only clean
//! bottom of a stack; every other stop is a [`Warning`], so a panic inside ROM does not read as a
//! complete one-frame walk. A caller's lookup uses `pc - 1`, because a return address points after
//! the call, which may be in the next FDE.

use gimli::{
    BaseAddresses, CfaRule, DebugFrame, EndianSlice, Register, RegisterRule, RunTimeEndian,
    UnwindContext, UnwindSection,
};
use pemu_loader::symbols::SymbolTable;

use crate::GuestMemory;
use crate::Warning;
use crate::dwarf::{DebugInfo, normalize_path};

/// DWARF register number of `x1` (`ra`).
pub const REG_RA: Register = Register(1);
/// DWARF register number of `x2` (`sp`).
pub const REG_SP: Register = Register(2);
/// DWARF register number of `x8` (`s0`/`fp`).
pub const REG_FP: Register = Register(8);

/// Frames an unwind will produce before it declares the chain unbounded.
pub const MAX_FRAMES: usize = 64;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum FrameOrigin {
    App,
    /// The bundled ROM ELF, which has symbols but no debug information.
    Rom,
    #[default]
    Unknown,
}

impl FrameOrigin {
    pub fn tag(self) -> &'static str {
        match self {
            FrameOrigin::App => "app",
            FrameOrigin::Rom => "rom",
            FrameOrigin::Unknown => "?",
        }
    }
}

/// One unwound guest stack frame. It carries the source path twice: as the debug information
/// writes it, and forward-slashed relative to the build root so macOS and Windows agree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// For a caller this is the return address, which points after the call instruction.
    pub pc: u32,
    /// The canonical frame address of the callee.
    pub sp: u32,
    /// From the debug information, else from a symbol table.
    pub function: Option<String>,
    /// Exactly as the debug information spells it.
    pub source: Option<String>,
    /// Forward-slashed, relative to the build root.
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// Inlined into the frame after it.
    pub inlined: bool,
    pub origin: FrameOrigin,
}

impl Frame {
    /// `<function> at <file>:<line> [<origin>] pc=<pc> sp=<sp>`.
    pub fn render(&self) -> String {
        let func = self.function.as_deref().unwrap_or("??");
        let mut out = String::new();
        if self.inlined {
            out.push_str("inlined ");
        }
        out.push_str(func);
        if let Some(file) = &self.file {
            out.push_str(" at ");
            out.push_str(file);
            if let Some(line) = self.line {
                out.push(':');
                out.push_str(&line.to_string());
            }
        }
        out.push_str(&format!(
            " [{}] pc={:#010x} sp={:#010x}",
            self.origin.tag(),
            self.pc,
            self.sp
        ));
        out
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Backtrace {
    /// Innermost first, inline frames included.
    pub frames: Vec<Frame>,
    pub warnings: Vec<Warning>,
}

impl Backtrace {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for f in &self.frames {
            out.push_str(&f.render());
            out.push('\n');
        }
        for w in &self.warnings {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }

    pub fn is_complete(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// The registers an unwind starts from: the four the CFI rules of a `-Og` ESP-IDF build ever
/// name.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Registers {
    pub pc: u32,
    pub sp: u32,
    pub ra: u32,
    pub fp: u32,
}

/// Names an address: the app ELF's debug information first, then the app and ROM symbol
/// tables.
pub struct Symbolizer<'a> {
    app: &'a SymbolTable,
    rom: Option<&'a SymbolTable>,
    debug: Option<&'a DebugInfo<'a>>,
}

impl<'a> Symbolizer<'a> {
    /// A symbolizer over an app symbol table alone, for a stripped ELF.
    pub fn new(app: &'a SymbolTable) -> Symbolizer<'a> {
        Symbolizer {
            app,
            rom: None,
            debug: None,
        }
    }

    /// Adds the debug information, which supplies file, line and inline frames.
    pub fn with_debug(mut self, debug: &'a DebugInfo<'a>) -> Symbolizer<'a> {
        self.debug = Some(debug);
        self
    }

    pub fn with_rom(mut self, rom: &'a SymbolTable) -> Symbolizer<'a> {
        self.rom = Some(rom);
        self
    }

    /// The frames at `pc`, innermost inline frame first; never empty. `lookup` is the address to
    /// resolve (`pc - 1` for a return address).
    pub fn frames(&self, pc: u32, sp: u32, lookup: u32) -> Vec<Frame> {
        let build_root = self.debug.and_then(DebugInfo::build_root);
        let mut out: Vec<Frame> = Vec::new();
        if let Some(debug) = self.debug {
            for f in debug.frames_at(lookup) {
                out.push(Frame {
                    pc,
                    sp,
                    function: f.function,
                    file: f.file.as_deref().map(|p| normalize_path(p, build_root)),
                    source: f.file,
                    line: f.line,
                    column: f.column,
                    inlined: f.inlined,
                    origin: FrameOrigin::App,
                });
            }
        }
        if let Some(last) = out.last_mut() {
            if last.function.is_none() {
                last.function = self.app.func_at(lookup).map(|s| s.name.clone());
            }
            return out;
        }
        let (function, origin) = match self.app.func_at(lookup) {
            Some(s) => (Some(s.name.clone()), FrameOrigin::App),
            None => match self.rom.and_then(|r| r.func_at(lookup)) {
                Some(s) => (Some(s.name.clone()), FrameOrigin::Rom),
                None => (None, FrameOrigin::Unknown),
            },
        };
        vec![Frame {
            pc,
            sp,
            function,
            origin,
            ..Frame::default()
        }]
    }
}

/// The `.debug_frame` CFI unwinder.
pub struct Unwinder<'a> {
    frame: DebugFrame<EndianSlice<'a, RunTimeEndian>>,
    bases: BaseAddresses,
    /// Length of `.debug_frame`, so [`Unwinder::has_cfi`] needs no gimli private trait.
    len: usize,
}

impl<'a> Unwinder<'a> {
    /// Builds an unwinder over a raw `.debug_frame` section. Version 1 and 3 entries carry no
    /// address size, so it is set to 4 for this 32-bit target.
    pub fn new(debug_frame: &'a [u8], endian: RunTimeEndian) -> Unwinder<'a> {
        let mut frame = DebugFrame::new(debug_frame, endian);
        frame.set_address_size(4);
        Unwinder {
            frame,
            bases: BaseAddresses::default(),
            len: debug_frame.len(),
        }
    }

    pub fn from_debug_info(debug: &'a DebugInfo<'a>) -> Unwinder<'a> {
        Unwinder::new(debug.debug_frame(), debug.endian())
    }

    pub fn has_cfi(&self) -> bool {
        self.len > 0
    }

    /// True when `.debug_frame` has a CFI row for `pc`.
    pub fn covers(&self, pc: u32) -> bool {
        let mut ctx = UnwindContext::new();
        self.frame
            .unwind_info_for_address(
                &self.bases,
                &mut ctx,
                u64::from(pc),
                DebugFrame::cie_from_offset,
            )
            .is_ok()
    }

    pub fn unwind(
        &self,
        regs: Registers,
        mem: &dyn GuestMemory,
        syms: &Symbolizer<'_>,
        max_frames: usize,
    ) -> Backtrace {
        let mut out = Backtrace::default();
        let max = max_frames.min(MAX_FRAMES);
        if !self.has_cfi() {
            out.frames.extend(syms.frames(regs.pc, regs.sp, regs.pc));
            out.warnings.push(Warning {
                what: ".debug_frame",
                at: regs.pc,
                detail: "the ELF has no CFI, so only the innermost frame is known".into(),
            });
            return out;
        }
        let mut ctx = UnwindContext::new();
        let mut regs = regs;
        for depth in 0..max {
            let lookup = if depth == 0 {
                regs.pc
            } else {
                regs.pc.wrapping_sub(1)
            };
            out.frames.extend(syms.frames(regs.pc, regs.sp, lookup));
            let row = match self.frame.unwind_info_for_address(
                &self.bases,
                &mut ctx,
                u64::from(lookup),
                DebugFrame::cie_from_offset,
            ) {
                Ok(row) => row,
                Err(e) => {
                    if e != gimli::Error::NoUnwindInfoForAddress {
                        out.warnings.push(Warning {
                            what: ".debug_frame",
                            at: lookup,
                            detail: e.to_string(),
                        });
                        return out;
                    }
                    // No CFI row. Code the app ELF does not describe (a panic inside ROM) is a stop the
                    // caller must tell apart from a complete backtrace, so it is reported.
                    let origin = out.frames.last().map_or(FrameOrigin::Unknown, |f| f.origin);
                    if depth == 0 || origin != FrameOrigin::App {
                        let kind = match origin {
                            FrameOrigin::App => "app",
                            FrameOrigin::Rom => "ROM",
                            FrameOrigin::Unknown => "unsymbolized",
                        };
                        out.warnings.push(Warning {
                            what: ".debug_frame",
                            at: lookup,
                            detail: format!(
                                "has no CFI row for this {kind} address, \
                                 so the walk stops short of the bottom of the stack"
                            ),
                        });
                    }
                    return out;
                }
            };
            let cfa = match row.cfa() {
                CfaRule::RegisterAndOffset { register, offset } => {
                    match register_value(&regs, *register) {
                        Some(base) => (u64::from(base) as i64).wrapping_add(*offset) as u32,
                        None => {
                            out.warnings.push(Warning {
                                what: "CFA rule",
                                at: regs.pc,
                                detail: format!(
                                    "names register x{}, which is not tracked",
                                    register.0
                                ),
                            });
                            return out;
                        }
                    }
                }
                CfaRule::Expression(_) => {
                    out.warnings.push(Warning {
                        what: "CFA rule",
                        at: regs.pc,
                        detail: "is a DWARF expression, which this unwinder does not evaluate"
                            .into(),
                    });
                    return out;
                }
            };
            let next_ra = match restore(row.register(REG_RA), regs.ra, cfa, mem) {
                Ok(v) => v,
                Err(detail) => {
                    out.warnings.push(Warning {
                        what: "return address",
                        at: cfa,
                        detail,
                    });
                    return out;
                }
            };
            let next_fp = restore(row.register(REG_FP), regs.fp, cfa, mem).unwrap_or(regs.fp);
            if next_ra == 0 {
                return out;
            }
            if next_ra == regs.pc && cfa == regs.sp {
                out.warnings.push(Warning {
                    what: "backtrace",
                    at: regs.pc,
                    detail: "the frame restores itself, so the chain does not progress".into(),
                });
                return out;
            }
            regs = Registers {
                pc: next_ra,
                sp: cfa,
                ra: next_ra,
                fp: next_fp,
            };
        }
        out.warnings.push(Warning {
            what: "backtrace",
            at: regs.pc,
            detail: format!("stopped after {max} frames"),
        });
        out
    }
}

fn register_value(regs: &Registers, register: Register) -> Option<u32> {
    match register {
        REG_RA => Some(regs.ra),
        REG_SP => Some(regs.sp),
        REG_FP => Some(regs.fp),
        _ => None,
    }
}

fn restore(
    rule: Option<RegisterRule<usize>>,
    current: u32,
    cfa: u32,
    mem: &dyn GuestMemory,
) -> Result<u32, String> {
    match rule {
        Some(RegisterRule::Offset(offset)) => {
            let at = (u64::from(cfa) as i64).wrapping_add(offset) as u32;
            mem.u32(at).map_err(|e| e.to_string())
        }
        Some(RegisterRule::ValOffset(offset)) => {
            Ok((u64::from(cfa) as i64).wrapping_add(offset) as u32)
        }
        Some(RegisterRule::Constant(v)) => Ok(v as u32),
        Some(RegisterRule::SameValue) | None => Ok(current),
        Some(RegisterRule::Undefined) => {
            Err("the CFI row marks it undefined, which ends the chain".into())
        }
        Some(other) => Err(format!("has the unsupported rule {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryImage;
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol};

    const DW_CFA_DEF_CFA: u8 = 0x0c;
    const DW_CFA_DEF_CFA_OFFSET: u8 = 0x0e;
    /// `DW_CFA_offset(r)` is the high two bits `01` with the register in the low six.
    fn dw_cfa_offset(register: u8, factored: u64) -> Vec<u8> {
        let mut out = vec![0x80 | register];
        uleb(&mut out, factored);
        out
    }

    fn uleb(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                return;
            }
        }
    }

    fn sleb(out: &mut Vec<u8>, mut v: i64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            let sign = byte & 0x40 != 0;
            if (v == 0 && !sign) || (v == -1 && sign) {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// Pads an entry body to a 4-byte multiple with `DW_CFA_nop` and prefixes its length.
    fn entry(mut body: Vec<u8>) -> Vec<u8> {
        while !body.len().is_multiple_of(4) {
            body.push(0x00);
        }
        let mut out = (body.len() as u32).to_le_bytes().to_vec();
        out.extend(body);
        out
    }

    /// A DWARF 4 CIE: 32-bit RISC-V, `ra` is `x1`, CFA starts at `sp`.
    fn cie() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(0xffff_ffffu32.to_le_bytes()); // CIE_id
        body.push(4); // version
        body.push(0); // augmentation ""
        body.push(4); // address_size
        body.push(0); // segment_selector_size
        uleb(&mut body, 2); // code alignment factor
        sleb(&mut body, -4); // data alignment factor
        uleb(&mut body, 1); // return address register x1
        body.push(DW_CFA_DEF_CFA);
        uleb(&mut body, 2); // register x2 (sp)
        uleb(&mut body, 0); // offset 0
        entry(body)
    }

    /// An FDE covering `[start, start + len)` whose prologue saves `ra` and `fp` at the top of a
    /// `frame`-byte frame.
    fn fde(cie_offset: u32, start: u32, len: u32, frame: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(cie_offset.to_le_bytes());
        body.extend(start.to_le_bytes());
        body.extend(len.to_le_bytes());
        body.push(DW_CFA_DEF_CFA_OFFSET);
        uleb(&mut body, frame);
        // Data alignment is -4, so a factored offset of 1 is CFA-4 and 2 is CFA-8.
        body.extend(dw_cfa_offset(1, 1));
        body.extend(dw_cfa_offset(8, 2));
        entry(body)
    }

    fn func(name: &str, addr: u32, size: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size,
            kind: SymKind::Func,
            bind: SymBind::Global,
            section: SymSection::Index(1),
        }
    }

    /// `leaf` called from `middle` called from `entry`, whose saved return address is zero, as at
    /// the bottom of a FreeRTOS task stack.
    fn section() -> Vec<u8> {
        let mut out = cie();
        out.extend(fde(0, 0x4200_0000, 0x40, 16));
        out.extend(fde(0, 0x4200_1000, 0x40, 32));
        out.extend(fde(0, 0x4200_2000, 0x40, 16));
        out
    }

    fn stack() -> MemoryImage {
        let mut m = MemoryImage::new();
        m.map_zeroed(0x3fc8_1000, 0x100);
        m.put_u32(0x3fc8_100c, 0x4200_1020); // leaf frame: return into middle
        m.put_u32(0x3fc8_102c, 0x4200_2030); // middle frame: return into entry
        m.put_u32(0x3fc8_103c, 0); // entry frame: bottom of the task stack
        m
    }

    fn symbols() -> SymbolTable {
        SymbolTable::new(vec![
            func("leaf", 0x4200_0000, 0x40),
            func("middle", 0x4200_1000, 0x40),
            func("entry", 0x4200_2000, 0x40),
        ])
    }

    #[test]
    fn cfi_unwind_walks_a_synthetic_frame_chain_to_the_bottom() {
        let bytes = section();
        let u = Unwinder::new(&bytes, RunTimeEndian::Little);
        assert!(u.has_cfi());
        let syms = symbols();
        let sym = Symbolizer::new(&syms);
        let regs = Registers {
            pc: 0x4200_0010,
            sp: 0x3fc8_1000,
            ra: 0,
            fp: 0,
        };
        let bt = u.unwind(regs, &stack(), &sym, MAX_FRAMES);
        assert!(bt.is_complete(), "{:?}", bt.warnings);
        assert_eq!(
            bt.render(),
            "leaf [app] pc=0x42000010 sp=0x3fc81000\n\
             middle [app] pc=0x42001020 sp=0x3fc81010\n\
             entry [app] pc=0x42002030 sp=0x3fc81030\n"
        );
    }

    #[test]
    fn an_innermost_pc_with_no_cfi_row_is_reported() {
        let bytes = section();
        let u = Unwinder::new(&bytes, RunTimeEndian::Little);
        assert!(u.has_cfi());
        assert!(!u.covers(0x4200_9010));
        let syms = SymbolTable::new(vec![func("no_cfi", 0x4200_9000, 0x40)]);
        let sym = Symbolizer::new(&syms);
        let regs = Registers {
            pc: 0x4200_9010,
            sp: 0x3fc8_1000,
            ra: 0,
            fp: 0,
        };
        let bt = u.unwind(regs, &stack(), &sym, MAX_FRAMES);
        assert_eq!(bt.frames.len(), 1);
        assert!(!bt.is_complete());
        assert_eq!(
            bt.render(),
            "no_cfi [app] pc=0x42009010 sp=0x3fc81000\n\
             warning: .debug_frame at 0x42009010: has no CFI row for this app address, \
             so the walk stops short of the bottom of the stack\n"
        );
    }

    #[test]
    fn rom_frames_are_symbolized_from_the_rom_table() {
        let bytes = section();
        let u = Unwinder::new(&bytes, RunTimeEndian::Little);
        let app = symbols();
        let rom = SymbolTable::new(vec![func("ets_printf", 0x4000_0000, 0x100)]);
        let sym = Symbolizer::new(&app).with_rom(&rom);
        let mut mem = stack();
        // The middle frame returns into the ROM instead.
        mem.put_u32(0x3fc8_102c, 0x4000_0040);
        let regs = Registers {
            pc: 0x4200_0010,
            sp: 0x3fc8_1000,
            ra: 0,
            fp: 0,
        };
        let bt = u.unwind(regs, &mem, &sym, MAX_FRAMES);
        assert!(!bt.is_complete());
        assert!(
            bt.render().ends_with(
                "warning: .debug_frame at 0x4000003f: has no CFI row for this ROM address, \
                 so the walk stops short of the bottom of the stack\n"
            ),
            "{}",
            bt.render()
        );
        let last = bt.frames.last().expect("a ROM frame");
        assert_eq!(last.function.as_deref(), Some("ets_printf"));
        assert_eq!(last.origin, FrameOrigin::Rom);
        assert_eq!(last.pc, 0x4000_0040);
        let bare = Symbolizer::new(&app);
        let bt = u.unwind(regs, &mem, &bare, MAX_FRAMES);
        let last = bt.frames.last().expect("a frame");
        assert_eq!(last.function, None);
        assert_eq!(last.origin, FrameOrigin::Unknown);
        assert!(last.render().starts_with("?? [?] pc=0x40000040"));
        assert!(
            bt.render()
                .contains("has no CFI row for this unsymbolized address"),
            "{}",
            bt.render()
        );
    }

    #[test]
    fn an_unreadable_saved_return_address_is_reported() {
        let bytes = section();
        let u = Unwinder::new(&bytes, RunTimeEndian::Little);
        let syms = symbols();
        let sym = Symbolizer::new(&syms);
        let mut mem = MemoryImage::new();
        // The leaf frame's CFA-4 lands outside every mapped span.
        mem.map_zeroed(0x3fc8_1000, 8);
        let regs = Registers {
            pc: 0x4200_0010,
            sp: 0x3fc8_1000,
            ra: 0,
            fp: 0,
        };
        let bt = u.unwind(regs, &mem, &sym, MAX_FRAMES);
        assert_eq!(bt.frames.len(), 1);
        assert_eq!(bt.warnings.len(), 1);
        assert_eq!(bt.warnings[0].what, "return address");
        assert_eq!(bt.warnings[0].at, 0x3fc8_1010);
        assert!(bt.render().ends_with(
            "warning: return address at 0x3fc81010: guest memory 0x3fc8100c..+4 is not readable\n"
        ));
    }

    #[test]
    fn a_missing_debug_frame_section_yields_one_frame_and_a_warning() {
        let u = Unwinder::new(&[], RunTimeEndian::Little);
        assert!(!u.has_cfi());
        let syms = symbols();
        let sym = Symbolizer::new(&syms);
        let regs = Registers {
            pc: 0x4200_0010,
            sp: 0x3fc8_1000,
            ra: 0,
            fp: 0,
        };
        let bt = u.unwind(regs, &stack(), &sym, MAX_FRAMES);
        assert_eq!(bt.frames.len(), 1);
        assert_eq!(bt.warnings.len(), 1);
        assert_eq!(bt.warnings[0].what, ".debug_frame");
    }

    #[test]
    fn an_endless_chain_stops_at_the_frame_budget_or_at_a_frame_that_restores_itself() {
        let syms = symbols();
        let sym = Symbolizer::new(&syms);
        let regs = Registers {
            pc: 0x4200_0010,
            sp: 0x3fc8_1000,
            ra: 0,
            fp: 0,
        };
        // Every stack word is a return address back into the FDE: the walk progresses but never
        // ends.
        let mut bytes = cie();
        bytes.extend(fde(0, 0x4200_0000, 0x1_0000, 16));
        let u = Unwinder::new(&bytes, RunTimeEndian::Little);
        let mut mem = MemoryImage::new();
        mem.map(
            0x3fc8_1000,
            0x4200_0010u32.to_le_bytes().repeat(MAX_FRAMES * 8),
        );
        let bt = u.unwind(regs, &mem, &sym, MAX_FRAMES);
        assert_eq!(bt.frames.len(), MAX_FRAMES);
        assert_eq!(bt.warnings.len(), 1);
        assert_eq!(bt.warnings[0].what, "backtrace");
        assert!(bt.warnings[0].detail.contains("stopped after 64 frames"));
        // A zero-width frame leaves the CFA where it was, so the next step would repeat.
        let mut bytes = cie();
        bytes.extend(fde(0, 0x4200_0000, 0x1_0000, 0));
        let u = Unwinder::new(&bytes, RunTimeEndian::Little);
        let mut mem = MemoryImage::new();
        mem.map_zeroed(0x3fc8_0000, 0x2000);
        mem.put_u32(0x3fc8_0ffc, 0x4200_0010);
        let bt = u.unwind(regs, &mem, &sym, MAX_FRAMES);
        assert_eq!(bt.frames.len(), 1);
        assert_eq!(bt.warnings.len(), 1);
        assert!(bt.warnings[0].detail.contains("does not progress"));
    }
}
