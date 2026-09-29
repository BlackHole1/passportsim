//! The LVGL safe point: when a UI read describes a consistent frame.
//!
//! `lv_obj_create` and `lv_obj_set_pos` only mark the layout invalid; `lv_display_refr_timer`
//! computes coordinates later. A tree read in between shows the right objects with zero-size
//! coordinates, wrong in a way no error would reveal. All three conditions must hold:
//!
//! 1. **Lock.** The `lvgl_port` mutex is free, or held by the LVGL task with the PC at
//!    `lv_timer_handler` entry. Without `esp_lvgl_port` there is no known mutex, so the PC
//!    test alone applies, in any task.
//! 2. **Layout.** The active screen's `scr_layout_inv` is 0 and no visited object has
//!    `layout_inv` set.
//! 3. **Rendering.** The display's `rendering_in_progress` is 0. A non-zero `inv_p` also means
//!    a redraw is pending, so a screenshot would not match the tree.

use crate::GuestMemory;
use crate::freertos::{MutexInfo, TaskSnapshot};
use crate::layout::Layouts;
use crate::lvgl::UiTree;
use crate::{IntrospectError, Warning};
use pemu_loader::symbols::SymbolTable;

/// The three conditions, evaluated.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SafePoint {
    pub lock_ok: bool,
    /// `None` when no walked tree was supplied.
    pub layout_ok: Option<bool>,
    pub rendering_ok: bool,
    /// `inv_p != 0`.
    pub redraw_pending: bool,
    /// TCB holding the `lvgl_port` mutex.
    pub holder: Option<u32>,
    /// Name of the holding task, when a task walk supplied one.
    pub holder_name: Option<String>,
    /// The firmware has no `lvgl_port_ctx`, so condition 1 fell back to the PC.
    pub port_absent: bool,
    /// One line per condition that does not hold, in condition order.
    pub reasons: Vec<String>,
    pub warnings: Vec<Warning>,
}

impl SafePoint {
    /// An unevaluated condition (no tree) is not a pass.
    pub fn is_safe(&self) -> bool {
        self.lock_ok && self.layout_ok == Some(true) && self.rendering_ok
    }

    pub fn render(&self) -> String {
        let mut out = if self.is_safe() {
            "safe".to_string()
        } else {
            format!("not safe: {}", self.reasons.join("; "))
        };
        if self.redraw_pending {
            out.push_str(" (redraw pending)");
        }
        out.push('\n');
        for w in &self.warnings {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }
}

/// Evaluates the safe-point predicate. `tasks` is used only to name the lock holder.
pub fn evaluate(
    layouts: &Layouts,
    syms: &SymbolTable,
    mem: &dyn GuestMemory,
    pc: u32,
    tree: Option<&UiTree>,
    tasks: Option<&TaskSnapshot>,
) -> Result<SafePoint, IntrospectError> {
    let mut out = SafePoint::default();
    let at_timer_entry = syms
        .addr_of("lv_timer_handler")
        .is_some_and(|entry| pc == entry & !1);

    match (
        syms.addr_of("lvgl_port_ctx"),
        layouts.get("lvgl_port_ctx_t"),
    ) {
        (Some(ctx), Some(port)) => {
            let task = port.u32(mem, ctx, "lvgl_task")?;
            let mux = port.u32(mem, ctx, "lvgl_mux")?;
            let lock = MutexInfo::read(layouts, mem, mux, tasks)?;
            out.holder = lock.holder;
            out.holder_name = lock.holder_name;
            out.lock_ok = match lock.holder {
                None => true,
                Some(holder) => holder == task && at_timer_entry,
            };
            if !out.lock_ok {
                let who = out
                    .holder_name
                    .clone()
                    .unwrap_or_else(|| format!("{:#010x}", lock.holder.unwrap_or(0)));
                out.reasons
                    .push(format!("the lvgl_port mutex is held by {who}"));
            }
        }
        _ => {
            out.port_absent = true;
            out.lock_ok = at_timer_entry;
            if !out.lock_ok {
                out.reasons.push(
                    "the firmware has no lvgl_port_ctx and the PC is not at lv_timer_handler \
                     entry"
                        .into(),
                );
            }
        }
    }

    if let Some(tree) = tree {
        let obj = layouts.require("_lv_obj_t")?;
        let screen_inv = obj.flag(mem, tree.screen, "scr_layout_inv")?;
        let stale = tree.nodes.iter().find(|n| n.layout_inv);
        out.layout_ok = Some(!screen_inv && stale.is_none());
        if screen_inv {
            out.reasons
                .push("the active screen has scr_layout_inv set".into());
        }
        if let Some(node) = stale {
            out.reasons.push(format!(
                "object {} ({:#010x}) has layout_inv set",
                node.reference, node.obj
            ));
        }
    } else {
        out.reasons
            .push("no UI tree was walked, so the layout condition is unevaluated".into());
    }

    let display = match tree.map(|t| t.display) {
        Some(d) if d != 0 => d,
        _ => {
            let global = crate::freertos::symbol(syms, "lv_global")?;
            layouts
                .require("_lv_global_t")?
                .u32(mem, global, "disp_default")?
        }
    };
    let d = layouts.require("_lv_display_t")?;
    out.rendering_ok = !d.flag(mem, display, "rendering_in_progress")?;
    out.redraw_pending = d.u32(mem, display, "inv_p")? != 0;
    if !out.rendering_ok {
        out.reasons
            .push("the display has rendering_in_progress set".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryImage;
    use crate::freertos::walk_tasks;
    use crate::lvgl::synthetic::{
        DISPLAY, LVGL_TASK, MUX, SCREEN, TIMER_HANDLER, image, layouts, symbols,
    };
    use crate::lvgl::walk_ui;
    use pemu_loader::symbols::SymbolTable;

    /// PC of an ordinary instruction, away from `lv_timer_handler` entry.
    const ELSEWHERE: u32 = 0x4200_9c84;

    fn evaluate_at(mem: &MemoryImage, pc: u32) -> SafePoint {
        let (layouts, syms) = (layouts(), symbols());
        let tree = walk_ui(&layouts, &syms, mem, 1).expect("the walk succeeds");
        let tasks = walk_tasks(&layouts, &syms, mem).expect("the task walk succeeds");
        evaluate(&layouts, &syms, mem, pc, Some(&tree), Some(&tasks))
            .expect("the predicate evaluates")
    }

    #[test]
    fn the_lock_condition_follows_the_mutex_holder_and_the_pc() {
        let mem = image();
        let sp = evaluate_at(&mem, ELSEWHERE);
        assert!(!sp.is_safe());
        assert!(!sp.lock_ok);
        assert_eq!(sp.holder_name.as_deref(), Some("main"));
        assert_eq!(
            sp.render(),
            "not safe: the lvgl_port mutex is held by main\n"
        );

        let mut mem = image();
        mem.put_u32(MUX + 8, 0);
        let sp = evaluate_at(&mem, ELSEWHERE);
        assert!(sp.is_safe(), "{:?}", sp.reasons);
        assert_eq!(sp.render(), "safe\n");

        let mut mem = image();
        mem.put_u32(MUX + 8, LVGL_TASK);
        assert!(!evaluate_at(&mem, ELSEWHERE).is_safe());
        let sp = evaluate_at(&mem, TIMER_HANDLER);
        assert!(sp.is_safe(), "{:?}", sp.reasons);
        assert_eq!(sp.holder, Some(LVGL_TASK));
    }

    #[test]
    fn the_layout_and_rendering_conditions_name_what_is_not_settled() {
        // `scr_layout_inv` of the active screen, byte 42 bit 2.
        let mut mem = image();
        mem.put_u32(MUX + 8, 0);
        mem.put(SCREEN + 42, &[1 << 2]);
        let sp = evaluate_at(&mem, ELSEWHERE);
        assert_eq!(sp.layout_ok, Some(false));
        assert!(
            sp.render()
                .contains("the active screen has scr_layout_inv set")
        );

        // `layout_inv` on a visited object, byte 42 bit 0.
        let mut mem = image();
        mem.put_u32(MUX + 8, 0);
        mem.put(crate::lvgl::synthetic::CARD1 + 42, &[1 | (1 << 4)]);
        let sp = evaluate_at(&mem, ELSEWHERE);
        assert_eq!(sp.layout_ok, Some(false));
        assert!(sp.render().contains("has layout_inv set"));

        // `rendering_in_progress`, byte 69 bit 2 of the display.
        let mut mem = image();
        mem.put_u32(MUX + 8, 0);
        mem.put(DISPLAY + 69, &[1 << 2]);
        let sp = evaluate_at(&mem, ELSEWHERE);
        assert!(!sp.rendering_ok);
        assert!(
            sp.render()
                .contains("the display has rendering_in_progress set")
        );

        // `inv_p` non-zero means a redraw is pending even at a safe point.
        let mut mem = image();
        mem.put_u32(MUX + 8, 0);
        mem.put_u32(DISPLAY + 620, 1);
        let sp = evaluate_at(&mem, ELSEWHERE);
        assert!(sp.is_safe());
        assert!(sp.redraw_pending);
        assert_eq!(sp.render(), "safe (redraw pending)\n");
    }

    #[test]
    fn an_unevaluated_layout_condition_is_not_a_pass() {
        let (layouts, syms) = (layouts(), symbols());
        let mut mem = image();
        mem.put_u32(MUX + 8, 0);
        let sp = evaluate(&layouts, &syms, &mem, ELSEWHERE, None, None).expect("it evaluates");
        assert!(sp.lock_ok && sp.rendering_ok);
        assert_eq!(sp.layout_ok, None);
        assert!(!sp.is_safe());
        assert!(sp.render().contains("unevaluated"));
    }

    #[test]
    fn firmware_without_lvgl_port_falls_back_to_the_program_counter() {
        let layouts = layouts();
        let syms: SymbolTable = SymbolTable::new(
            symbols()
                .iter()
                .filter(|s| s.name != "lvgl_port_ctx")
                .cloned()
                .collect(),
        );
        let mem = image();
        let tree = walk_ui(&layouts, &syms, &mem, 1).expect("the walk succeeds");
        let sp = evaluate(&layouts, &syms, &mem, ELSEWHERE, Some(&tree), None)
            .expect("the predicate evaluates");
        assert!(sp.port_absent);
        assert!(!sp.lock_ok);
        assert!(sp.render().contains("no lvgl_port_ctx"));
        let sp = evaluate(&layouts, &syms, &mem, TIMER_HANDLER, Some(&tree), None)
            .expect("the predicate evaluates");
        assert!(sp.is_safe(), "{:?}", sp.reasons);
    }
}
