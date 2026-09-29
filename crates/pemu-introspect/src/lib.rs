//! Guest introspection: DWARF layouts, unwinding, FreeRTOS task, TLSF heap and LVGL
//! tree walkers, safe points, panic decoding and redacted NVS listing.
//!
//! Every walker reads a [`GuestMemory`] through offsets resolved from the guest's own DWARF
//! ([`layout::Layouts`]), never hard-coded ones, so it stays correct for firmware built with other
//! Kconfig options, and its output is a pure function of (memory bytes, layouts, symbols).

pub mod dwarf;
pub mod freertos;
pub mod layout;
pub mod lvgl;
pub mod nvs;
pub mod panic;
pub mod safepoint;
pub mod tlsf;
pub mod unwind;
pub mod vars;

use core::fmt;

pub use layout::{GuestMemory, MemError, MemoryImage};

/// Why an introspection request could not be answered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IntrospectError {
    /// A guest memory read failed.
    Memory(MemError),
    /// The ELF carries no debug information for a structure the walker needs.
    MissingStruct {
        /// C name of the structure, as DWARF spells it.
        name: &'static str,
    },
    /// The ELF carries the structure but not a member the walker needs.
    MissingMember {
        name: &'static str,
        /// Member path, dotted for a nested member (`u.xSemaphore.xMutexHolder`).
        member: &'static str,
    },
    /// The ELF has no symbol of this name.
    MissingSymbol {
        /// Looked up in the app symbol table, then the ROM's.
        name: &'static str,
    },
    /// A resolved layout failed a sanity check (`E_LAYOUT_MISMATCH`).
    LayoutMismatch { what: &'static str, detail: String },
    /// A `ui` ref from an older revision no longer names the same object (`E_STALE_REF`).
    StaleRef {
        reference: String,
        /// Revision it was issued in.
        rev: u64,
        detail: String,
    },
    /// The DWARF or `.debug_frame` sections could not be parsed.
    Dwarf(String),
    /// No compilation unit declares a global of this name (`inspect vars`). Unlike
    /// [`IntrospectError::MissingSymbol`], the name comes from the caller, not the walker.
    MissingGlobal {
        /// The query as written (`main.c::s_sel`, `s_ok[2]`).
        name: String,
    },
    /// More than one compilation unit declares a global of this name (C file statics), so the
    /// caller must qualify it (`main.c::s_sel`).
    AmbiguousGlobal {
        name: String,
        /// File name of every unit that declares it, in section order.
        units: Vec<String>,
    },
}

impl fmt::Display for IntrospectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntrospectError::Memory(e) => write!(f, "{e}"),
            IntrospectError::MissingStruct { name } => {
                write!(f, "no DWARF definition of struct {name}")
            }
            IntrospectError::MissingMember { name, member } => {
                write!(f, "struct {name} has no member {member}")
            }
            IntrospectError::MissingSymbol { name } => write!(f, "no symbol {name}"),
            IntrospectError::LayoutMismatch { what, detail } => {
                write!(f, "E_LAYOUT_MISMATCH: {what}: {detail}")
            }
            IntrospectError::StaleRef {
                reference,
                rev,
                detail,
            } => write!(f, "E_STALE_REF: {reference} of ui_rev {rev}: {detail}"),
            IntrospectError::Dwarf(msg) => write!(f, "DWARF: {msg}"),
            IntrospectError::MissingGlobal { name } => {
                write!(
                    f,
                    "no global named {name} in this firmware's debug information"
                )
            }
            IntrospectError::AmbiguousGlobal { name, units } => write!(
                f,
                "{name} is declared by {} compilation units ({}): name one, as in `{}::{name}`",
                units.len(),
                units.join(", "),
                units.first().map_or("main.c", String::as_str)
            ),
        }
    }
}

impl std::error::Error for IntrospectError {}

impl From<MemError> for IntrospectError {
    fn from(e: MemError) -> IntrospectError {
        IntrospectError::Memory(e)
    }
}

/// A structure a walker found inconsistent, reported instead of followed. Every early stop of a
/// walk appends one, so a partial result is never silent.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Warning {
    /// Structure the walker was following, for example `xLIST` or `tlsf pool`.
    pub what: &'static str,
    /// Guest address the walk stopped at.
    pub at: u32,
    pub detail: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Warning { what, at, detail } = self;
        write!(f, "{what} at {at:#010x}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    //! Pins the rendered bytes. It runs natively only; wasm output matches because every
    //! rendering is a pure function of its input (`xtask layering`, rule `core-std-api`).

    use crate::GuestMemory;
    use crate::freertos::{resolve_blocks, walk_tasks};
    use crate::lvgl::synthetic::{MUX, image, layouts, symbols};
    use crate::lvgl::{Prune, walk_ui};
    use crate::safepoint;

    /// Every walker's rendering over the synthetic image, concatenated.
    fn report(mem: &dyn GuestMemory) -> String {
        let (layouts, syms) = (layouts(), symbols());
        let tree = walk_ui(&layouts, &syms, mem, 1).expect("the UI walk");
        let mut tasks = walk_tasks(&layouts, &syms, mem).expect("the task walk");
        resolve_blocks(
            &mut tasks,
            &layouts,
            mem,
            &[("lvgl_port mutex".to_string(), MUX)],
        );
        let safe = safepoint::evaluate(&layouts, &syms, mem, 0, Some(&tree), Some(&tasks))
            .expect("the safe point");
        let mut out = String::new();
        out.push_str(&layouts.render());
        out.push_str(&tree.render(Prune::Semantic, true));
        out.push_str(&tasks.render());
        out.push_str(&safe.render());
        out.push_str(&crate::nvs::list(&[0xffu8; crate::nvs::PAGE_SIZE]).render());
        out
    }

    #[test]
    fn every_rendering_is_pinned_to_exact_bytes() {
        let text = report(&image());
        assert_eq!(
            text,
            concat!(
                "QueueDefinition size=84 pcHead=0 u.xSemaphore.xMutexHolder=8 ",
                "u.xSemaphore.uxRecursiveCallCount=12 xTasksWaitingToSend=16 ",
                "xTasksWaitingToReceive=36\n",
                "_lv_display_t size=812 hor_res=0 ver_res=4 screens=708 sys_layer=712 ",
                "top_layer=716 act_scr=720 bottom_layer=724 prev_scr=728 scr_to_load=732 ",
                "screen_cnt=736 rendering_in_progress=69:2/1 inv_p=620\n",
                "_lv_global_t size=504 inited=4 disp_ll=8 disp_refresh=20 disp_default=24 ",
                "indev_ll=72 tick_state=188 tlsf_state=464\n",
                "_lv_label_t size=108 text=48 long_mode=96:0/4\n",
                "_lv_obj_class_t size=36 base_class=0 name=20 instance_size=32:4/16\n",
                "_lv_obj_spec_attr_t size=52 children=0 child_cnt=48 scroll=32\n",
                "_lv_obj_style_t size=8 style=0 selector=4:0/24 is_local=7:0/1\n",
                "_lv_obj_t size=48 class_p=0 parent=4 spec_attr=8 styles=12 user_data=16 ",
                "coords=20 flags=36 state=40 layout_inv=42:0/1 scr_layout_inv=42:2/1 ",
                "style_cnt=42:4/6 is_deleting=43:6/1\n",
                "lv_style_t size=12 values_and_props=0 prop_cnt=8\n",
                "lvgl_port_ctx_t size=32 lvgl_task=0 lvgl_mux=4\n",
                "tskTaskControlBlock size=336 pxTopOfStack=0 xStateListItem=4 ",
                "xEventListItem=24 uxPriority=44 pxStack=48 pcTaskName=52 pxEndOfStack=68 ",
                "uxBasePriority=72 ucNotifyState=332\n",
                "xLIST size=20 uxNumberOfItems=0 pxIndex=4 xListEnd=8\n",
                "xLIST_ITEM size=20 xItemValue=0 pxNext=4 pxPrevious=8 pvOwner=12 ",
                "pxContainer=16\n",
                "- obj [0,0 240x320] e1\n",
                "  - obj [5,8 151x33] bg=#f4f4ea border=#17202a e2\n",
                "    - label \"FoloToy\" [42,13 76x22] e3\n",
                "  - obj [11,52 102x40] bg=#ffd928 border=#ffffff e4\n",
                "    - label \"Display\" [36,64 53x16] e5\n",
                "  - obj [123,52 102x40] bg=#f4f4ea border=#17202a e6\n",
                "    - label \"Button\" [148,64 52x16] e7\n",
                "main running prio=22(1) stack=512/1024 tcb=0x3fcb0000\n",
                "esp_timer blocked prio=22(22) stack=900/1024 tcb=0x3fcb0200 blocked on ",
                "lvgl_port mutex (waiting to receive)\n",
                "taskLVGL suspended prio=4(4) stack=128/1024 tcb=0x3fcb0400\n",
                "not safe: the lvgl_port mutex is held by main\n",
                "page 0 uninitialized seq=4294967295 v0xff 0w/0e/126empty\n",
            )
        );
        assert_eq!(report(&image()), text);
    }
}
