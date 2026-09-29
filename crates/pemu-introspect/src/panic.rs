//! Decoding a guest panic: the RISC-V exception frame ESP-IDF hands `esp_panic_handler`, the
//! trap cause, the faulting task, and the unwound backtrace.
//!
//! The frame is read through the DWARF layout of `RvExcFrame`, not a fixed offset table. The
//! `E_GUEST_PANIC` response (serial tail, suggested commands, budgets, redaction) is shaped by
//! `pemu-api` from these facts.

use crate::GuestMemory;
use crate::layout::Layouts;
use crate::unwind::{Backtrace, Registers, Symbolizer, Unwinder};
use crate::{IntrospectError, Warning};

/// The saved RISC-V machine-mode exception frame (`RvExcFrame` in ESP-IDF). Only the members
/// the decoder and the unwinder need are read.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct ExcFrame {
    /// Guest address of the frame itself.
    pub at: u32,
    pub mepc: u32,
    /// `x1`.
    pub ra: u32,
    /// `x2`.
    pub sp: u32,
    /// `x8`, the frame pointer.
    pub fp: u32,
    pub mstatus: u32,
    /// Bit 31 marks an interrupt, the rest is the cause code.
    pub mcause: u32,
    /// The faulting address or instruction word.
    pub mtval: u32,
}

impl ExcFrame {
    pub fn read(
        layouts: &Layouts,
        mem: &dyn GuestMemory,
        at: u32,
    ) -> Result<ExcFrame, IntrospectError> {
        let f = layouts.require("RvExcFrame")?;
        Ok(ExcFrame {
            at,
            mepc: f.u32(mem, at, "mepc")?,
            ra: f.u32(mem, at, "ra")?,
            sp: f.u32(mem, at, "sp")?,
            fp: f.u32(mem, at, "s0")?,
            mstatus: f.u32(mem, at, "mstatus")?,
            mcause: f.u32(mem, at, "mcause")?,
            mtval: f.u32(mem, at, "mtval")?,
        })
    }

    pub fn is_interrupt(self) -> bool {
        self.mcause & 0x8000_0000 != 0
    }

    pub fn cause_code(self) -> u32 {
        self.mcause & 0x7fff_ffff
    }

    pub fn registers(self) -> Registers {
        Registers {
            pc: self.mepc,
            sp: self.sp,
            ra: self.ra,
            fp: self.fp,
        }
    }
}

/// The RISC-V privileged-architecture name of a trap cause. An unknown code is reported by
/// number rather than guessed.
pub fn trap_reason(mcause: u32) -> String {
    let code = mcause & 0x7fff_ffff;
    if mcause & 0x8000_0000 != 0 {
        return match code {
            3 => "machine software interrupt".into(),
            7 => "machine timer interrupt".into(),
            11 => "machine external interrupt".into(),
            _ => format!("interrupt {code}"),
        };
    }
    match code {
        0 => "instruction address misaligned".into(),
        1 => "instruction access fault".into(),
        2 => "illegal instruction".into(),
        3 => "breakpoint".into(),
        4 => "load address misaligned".into(),
        5 => "load access fault".into(),
        6 => "store or AMO address misaligned".into(),
        7 => "store or AMO access fault".into(),
        8 => "environment call from U-mode".into(),
        9 => "environment call from S-mode".into(),
        11 => "environment call from M-mode".into(),
        12 => "instruction page fault".into(),
        13 => "load page fault".into(),
        15 => "store or AMO page fault".into(),
        _ => format!("exception {code}"),
    }
}

/// Which stop path produced the record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PanicKind {
    Exception,
    /// `abort` was called; the hook captured the message first.
    Abort,
    Assert,
    /// The hardware stack guard fired, or `vApplicationStackOverflowHook` ran.
    StackOverflow,
    /// The interrupt or task watchdog fired.
    Watchdog,
}

impl PanicKind {
    pub fn tag(&self) -> &'static str {
        match self {
            PanicKind::Exception => "exception",
            PanicKind::Abort => "abort",
            PanicKind::Assert => "assert",
            PanicKind::StackOverflow => "stack overflow",
            PanicKind::Watchdog => "watchdog",
        }
    }
}

/// One decoded guest panic. The serial tail and suggested commands are added by the command
/// layer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PanicRecord {
    pub kind: PanicKind,
    /// From `mcause` for an exception, from the hook otherwise.
    pub reason: String,
    pub frame: Option<ExcFrame>,
    /// From the FreeRTOS walker.
    pub task: Option<String>,
    /// Assertion expression or abort message captured by the hook.
    pub detail: Option<String>,
    /// `file:line` captured by the `__assert_func` hook.
    pub location: Option<String>,
    /// Empty until [`PanicRecord::unwind`] runs.
    pub backtrace: Backtrace,
    pub warnings: Vec<Warning>,
}

impl PanicRecord {
    /// Decodes a CPU trap from the exception frame `esp_panic_handler` was given.
    pub fn from_exception_frame(
        layouts: &Layouts,
        mem: &dyn GuestMemory,
        frame_ptr: u32,
    ) -> Result<PanicRecord, IntrospectError> {
        let frame = ExcFrame::read(layouts, mem, frame_ptr)?;
        Ok(PanicRecord {
            kind: PanicKind::Exception,
            reason: trap_reason(frame.mcause),
            frame: Some(frame),
            task: None,
            detail: None,
            location: None,
            backtrace: Backtrace::default(),
            warnings: Vec::new(),
        })
    }

    /// A record for a stop path with no exception frame: `abort`, `__assert_func`, a
    /// stack-overflow hook or a watchdog.
    pub fn from_hook(kind: PanicKind, reason: impl Into<String>) -> PanicRecord {
        PanicRecord {
            kind,
            reason: reason.into(),
            frame: None,
            task: None,
            detail: None,
            location: None,
            backtrace: Backtrace::default(),
            warnings: Vec::new(),
        }
    }

    pub fn with_task(mut self, task: impl Into<String>) -> PanicRecord {
        self.task = Some(task.into());
        self
    }

    /// Adds the expression or message the hook captured, and its source location.
    pub fn with_detail(
        mut self,
        detail: impl Into<String>,
        location: Option<String>,
    ) -> PanicRecord {
        self.detail = Some(detail.into());
        self.location = location;
        self
    }

    /// Unwinds from the exception frame, or from `regs` when there is none. With neither, the
    /// record gets a warning, because an empty backtrace would read like a shallow stack.
    pub fn unwind(
        mut self,
        unwinder: &Unwinder<'_>,
        mem: &dyn GuestMemory,
        syms: &Symbolizer<'_>,
        regs: Option<Registers>,
    ) -> PanicRecord {
        match self.frame.map(ExcFrame::registers).or(regs) {
            Some(regs) => {
                self.backtrace = unwinder.unwind(regs, mem, syms, crate::unwind::MAX_FRAMES);
            }
            None => self.warnings.push(Warning {
                what: "backtrace",
                at: 0,
                detail: "the stop path carried no register state to unwind from".into(),
            }),
        }
        self
    }

    /// A header line, the `mcause`/`mtval` line for a trap, then the backtrace.
    pub fn render(&self) -> String {
        let mut out = format!("panic {}: {}", self.kind.tag(), self.reason);
        if let Some(task) = &self.task {
            out.push_str(&format!(" in task {task}"));
        }
        out.push('\n');
        if let Some(detail) = &self.detail {
            out.push_str(&format!("detail: {detail}"));
            if let Some(loc) = &self.location {
                out.push_str(&format!(" at {loc}"));
            }
            out.push('\n');
        }
        if let Some(f) = &self.frame {
            out.push_str(&format!(
                "mcause={:#010x} mtval={:#010x} mepc={:#010x} sp={:#010x}\n",
                f.mcause, f.mtval, f.mepc, f.sp
            ));
        }
        out.push_str(&self.backtrace.render());
        for w in &self.warnings {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryImage;
    use crate::layout::{MemberLayout, StructLayout};

    /// The `RvExcFrame` layout the official ELF resolves to (corpus test
    /// `t1_official_nested_and_typedef_layouts_resolve`).
    fn layouts() -> Layouts {
        let member = |path: &str, offset: u32| MemberLayout {
            path: path.to_string(),
            offset,
            bits: None,
            size: None,
        };
        let mut l = Layouts::new();
        l.insert(StructLayout::new(
            "RvExcFrame",
            148,
            vec![
                member("mepc", 0),
                member("ra", 4),
                member("sp", 8),
                member("s0", 32),
                member("mstatus", 128),
                member("mcause", 136),
                member("mtval", 140),
            ],
        ));
        l
    }

    #[test]
    fn a_load_fault_decodes_through_the_dwarf_exception_frame_layout() {
        let mut mem = MemoryImage::new();
        mem.map_zeroed(0x3fc8_2000, 148);
        mem.put_u32(0x3fc8_2000, 0x4200_0010); // mepc
        mem.put_u32(0x3fc8_2004, 0x4200_1020); // ra
        mem.put_u32(0x3fc8_2008, 0x3fc8_1000); // sp
        mem.put_u32(0x3fc8_2020, 0x3fc8_1010); // s0
        mem.put_u32(0x3fc8_2080, 0x0000_1880); // mstatus at +128
        mem.put_u32(0x3fc8_2088, 5); // mcause at +136: load access fault
        mem.put_u32(0x3fc8_208c, 0x0000_0004); // mtval at +140: the faulting address
        let record = PanicRecord::from_exception_frame(&layouts(), &mem, 0x3fc8_2000)
            .expect("the frame decodes")
            .with_task("taskLVGL");
        let frame = record.frame.expect("a frame");
        assert!(!frame.is_interrupt());
        assert_eq!(frame.cause_code(), 5);
        assert_eq!(record.reason, "load access fault");
        assert_eq!(
            frame.registers(),
            Registers {
                pc: 0x4200_0010,
                sp: 0x3fc8_1000,
                ra: 0x4200_1020,
                fp: 0x3fc8_1010,
            }
        );
        assert_eq!(
            record.render(),
            "panic exception: load access fault in task taskLVGL\n\
             mcause=0x00000005 mtval=0x00000004 mepc=0x42000010 sp=0x3fc81000\n"
        );
    }

    #[test]
    fn trap_reasons_cover_interrupts_and_unknown_codes() {
        assert_eq!(trap_reason(2), "illegal instruction");
        assert_eq!(trap_reason(6), "store or AMO address misaligned");
        assert_eq!(trap_reason(7), "store or AMO access fault");
        assert_eq!(trap_reason(0x8000_0007), "machine timer interrupt");
        assert_eq!(trap_reason(0x8000_0013), "interrupt 19");
        assert_eq!(trap_reason(30), "exception 30");
    }

    #[test]
    fn a_hook_record_carries_its_detail_and_reports_a_missing_stack() {
        let record = PanicRecord::from_hook(PanicKind::Assert, "assertion failed")
            .with_task("main")
            .with_detail("obj != NULL", Some("lv_obj.c:412".into()));
        let syms = pemu_loader::symbols::SymbolTable::default();
        let sym = Symbolizer::new(&syms);
        let unwinder = Unwinder::new(&[], gimli::RunTimeEndian::Little);
        let record = record.unwind(&unwinder, &MemoryImage::new(), &sym, None);
        assert_eq!(
            record.render(),
            "panic assert: assertion failed in task main\n\
             detail: obj != NULL at lv_obj.c:412\n\
             warning: backtrace at 0x00000000: the stop path carried no register state to \
             unwind from\n"
        );
    }

    #[test]
    fn a_missing_exception_frame_layout_is_an_error_not_a_guess() {
        let mut mem = MemoryImage::new();
        mem.map_zeroed(0x3fc8_2000, 148);
        assert_eq!(
            PanicRecord::from_exception_frame(&Layouts::new(), &mem, 0x3fc8_2000),
            Err(IntrospectError::MissingStruct { name: "RvExcFrame" })
        );
    }
}
