//! The LVGL object-tree walker: the tree, semantic state read from local styles, semantic
//! pruning, and the `eN` refs.
//!
//! The walk starts at `lv_global`, takes `disp_default`, then `act_scr`, and descends
//! `spec_attr->children`. The official menu marks the selected card only by its background and
//! border colors, so the walker reads `LV_STYLE_BG_COLOR` and `LV_STYLE_BORDER_COLOR` from each
//! object's local styles.
//!
//! Pruning keeps a node with text, an image source, a bar value, a semantic state, or a clickable
//! widget; drops hidden, off-display and decorative leaves; keeps ancestors of kept nodes; and
//! collapses a chain of single-child plain containers (no local background or border color).
//!
//! `LV_OBJ_FLAG_CLICKABLE` alone means nothing: `lv_obj_constructor` sets it on every object
//! (LVGL 9.5.0 `src/core/lv_obj.c:584`) and only non-interactive widgets clear it (`lv_label.c:762`,
//! `lv_image.c:693`). It counts only on an object whose innermost class is a widget built on
//! `lv_obj` (`lv_button`, `lv_checkbox`, `lv_slider`).

use crate::GuestMemory;
use crate::layout::Layouts;
use crate::{IntrospectError, Warning};
use pemu_loader::symbols::SymbolTable;

/// Objects the walker will visit before declaring the tree unbounded.
pub const MAX_OBJECTS: usize = 4096;

pub const MAX_DEPTH: u32 = 32;

const MAX_TEXT: usize = 256;

/// `LV_OBJ_FLAG_HIDDEN` (`lv_obj.h`).
pub const FLAG_HIDDEN: u32 = 1 << 0;
/// `LV_OBJ_FLAG_CLICKABLE`; see the module docs.
pub const FLAG_CLICKABLE: u32 = 1 << 1;
pub const FLAG_SCROLLABLE: u32 = 1 << 4;
pub const FLAG_FLOATING: u32 = 1 << 18;

pub const STYLE_BG_COLOR: u8 = 73;
pub const STYLE_BORDER_COLOR: u8 = 57;
/// A `prop_cnt` of 255 marks a const style, whose property array is not walked.
const CONST_STYLE: u8 = 255;

/// The LVGL state bits that read as semantic state (`lv_obj_style.h`), in rendering order.
pub const STATES: [(u16, &str); 5] = [
    (1 << 2, "checked"),
    (1 << 3, "focused"),
    (1 << 5, "edited"),
    (1 << 7, "pressed"),
    (1 << 9, "disabled"),
];

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Prune {
    /// The default of `passport_ui`.
    #[default]
    Semantic,
    None,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub fn render(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UiNode {
    pub obj: u32,
    /// `e1`, `e2`, ..., assigned in pre-order.
    pub reference: String,
    /// With the `lv_` prefix removed, for example `obj` or `label`.
    pub class: String,
    /// Innermost first, as DWARF spells it (`lv_label`, `lv_obj`).
    pub class_chain: Vec<String>,
    /// 0 for a screen.
    pub parent: u32,
    pub depth: u32,
    pub children: Vec<u32>,
    pub x: i32,
    pub y: i32,
    /// LVGL stores `x2 - x1 + 1`.
    pub w: i32,
    pub h: i32,
    pub flags: u32,
    pub state: u16,
    /// Part of the safe-point predicate.
    pub layout_inv: bool,
    /// When the class chain reaches `lv_label`.
    pub text: Option<String>,
    /// Source pointer and type, when the chain reaches `lv_image`.
    pub image: Option<(u32, u32)>,
    /// `(cur, min, max)` when the chain reaches `lv_bar`.
    pub value: Option<(i32, i32, i32)>,
    pub bg: Option<Color>,
    pub border: Option<Color>,
}

impl UiNode {
    pub fn states(&self) -> Vec<&'static str> {
        STATES
            .iter()
            .filter(|(bit, _)| self.state & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }

    pub fn is_hidden(&self) -> bool {
        self.flags & FLAG_HIDDEN != 0
    }

    /// Text, an image, a value, a semantic state, or clickability with a role.
    pub fn is_semantic(&self) -> bool {
        self.text.as_ref().is_some_and(|t| !t.is_empty())
            || self.image.is_some()
            || self.value.is_some()
            || !self.states().is_empty()
            || self.is_interactive()
    }

    /// `CLICKABLE` on an object whose innermost class is not the plain `lv_obj` (module docs).
    pub fn is_interactive(&self) -> bool {
        self.flags & FLAG_CLICKABLE != 0
            && self
                .class_chain
                .first()
                .is_some_and(|class| class != "lv_obj")
    }

    /// `- <class> ["<text>"] [<x>,<y> <w>x<h>] [<state>,...] [bg=] [border=] [value=] e<N>`,
    /// without the indent.
    pub fn render(&self, include_style: bool) -> String {
        let mut out = format!("- {}", self.class);
        if let Some(text) = &self.text {
            out.push_str(&format!(" {:?}", text));
        }
        out.push_str(&format!(" [{},{} {}x{}]", self.x, self.y, self.w, self.h));
        for state in self.states() {
            out.push(' ');
            out.push_str(state);
        }
        if include_style {
            if let Some(bg) = self.bg {
                out.push_str(&format!(" bg={}", bg.render()));
            }
            if let Some(border) = self.border {
                out.push_str(&format!(" border={}", border.render()));
            }
        }
        if let Some((cur, min, max)) = self.value {
            out.push_str(&format!(" value={cur}/{min}..{max}"));
        }
        out.push(' ');
        out.push_str(&self.reference);
        out
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UiTree {
    /// The caller bumps it whenever the tree is re-walked.
    pub rev: u64,
    /// Pre-order, the order the refs were assigned in.
    pub nodes: Vec<UiNode>,
    pub display: u32,
    pub hor_res: i32,
    pub ver_res: i32,
    pub screen: u32,
    pub warnings: Vec<Warning>,
}

impl UiTree {
    pub fn node(&self, reference: &str) -> Option<&UiNode> {
        self.nodes.iter().find(|n| n.reference == reference)
    }

    pub fn at(&self, obj: u32) -> Option<&UiNode> {
        self.nodes.iter().find(|n| n.obj == obj)
    }

    /// Resolves a ref from an older revision: it still resolves while the address holds an object
    /// with the same class and parent; otherwise `E_STALE_REF`.
    pub fn resolve_stale(
        &self,
        reference: &str,
        older: &UiTree,
    ) -> Result<&UiNode, IntrospectError> {
        let stale = |detail: String| IntrospectError::StaleRef {
            reference: reference.to_string(),
            rev: older.rev,
            detail,
        };
        if older.rev == self.rev {
            return self
                .node(reference)
                .ok_or_else(|| stale("no such ref in this revision".into()));
        }
        let was = older
            .node(reference)
            .ok_or_else(|| stale("no such ref in that revision".into()))?;
        let now = self
            .at(was.obj)
            .ok_or_else(|| stale("the object is gone".into()))?;
        if now.class != was.class || now.parent != was.parent {
            return Err(stale(format!(
                "the address now holds a {} under {:#010x}, not a {} under {:#010x}",
                now.class, now.parent, was.class, was.parent
            )));
        }
        Ok(now)
    }

    /// One node per line, two spaces of indent per level, fields in a fixed order.
    pub fn render(&self, prune: Prune, include_style: bool) -> String {
        let keep = self.kept(prune);
        let mut out = String::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            // Indent by kept ancestors, so a collapsed container leaves no gap.
            let indent = self.kept_ancestors(i, &keep);
            out.push_str(&"  ".repeat(indent));
            out.push_str(&node.render(include_style));
            out.push('\n');
        }
        for w in &self.warnings {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }

    fn kept(&self, prune: Prune) -> Vec<bool> {
        let mut keep = vec![matches!(prune, Prune::None); self.nodes.len()];
        if matches!(prune, Prune::None) {
            return keep;
        }
        let index = |obj: u32| self.nodes.iter().position(|n| n.obj == obj);
        // Keep visible, on-screen semantic nodes and every ancestor of one.
        for (i, node) in self.nodes.iter().enumerate() {
            if node.is_hidden() || self.is_offscreen(node) || !node.is_semantic() {
                continue;
            }
            keep[i] = true;
            let mut parent = node.parent;
            while parent != 0 {
                match index(parent) {
                    Some(p) if !keep[p] => {
                        keep[p] = true;
                        parent = self.nodes[p].parent;
                    }
                    _ => break,
                }
            }
        }
        // The root is always shown, so a tree with nothing semantic is still an answer.
        if let Some(root) = self.nodes.first() {
            keep[0] = !root.is_hidden();
        }
        // Collapse single-child plain containers. A container with a local background or
        // border color is not plain: the official menu's selection is style-only.
        for i in 0..self.nodes.len() {
            if !keep[i] || i == 0 {
                continue;
            }
            let node = &self.nodes[i];
            if node.is_semantic() || node.bg.is_some() || node.border.is_some() {
                continue;
            }
            let kept_children = node
                .children
                .iter()
                .filter_map(|c| index(*c))
                .filter(|&c| keep[c])
                .count();
            if kept_children == 1 {
                keep[i] = false;
            }
        }
        keep
    }

    fn is_offscreen(&self, node: &UiNode) -> bool {
        if self.hor_res <= 0 || self.ver_res <= 0 {
            return false;
        }
        node.w <= 0
            || node.h <= 0
            || node.x >= self.hor_res
            || node.y >= self.ver_res
            || node.x + node.w <= 0
            || node.y + node.h <= 0
    }

    /// Kept ancestors of the node at `i`, which is its indent level.
    fn kept_ancestors(&self, i: usize, keep: &[bool]) -> usize {
        let mut n = 0;
        let mut parent = self.nodes[i].parent;
        while parent != 0 {
            let Some(p) = self.nodes.iter().position(|x| x.obj == parent) else {
                break;
            };
            if keep[p] {
                n += 1;
            }
            parent = self.nodes[p].parent;
        }
        n
    }
}

/// Walks the active screen of the default display. `rev` is the revision the refs belong to.
///
/// The layout is checked before the tree is trusted: [`Layouts::check_lvgl`], `screen_cnt` at
/// most 16, the active screen in `screens[]`, and a `class_p->name` that is a printable C string
/// beginning `lv_`. A failure is `E_LAYOUT_MISMATCH`, never a tree read through wrong offsets.
pub fn walk_ui(
    layouts: &Layouts,
    syms: &SymbolTable,
    mem: &dyn GuestMemory,
    rev: u64,
) -> Result<UiTree, IntrospectError> {
    layouts.check_lvgl()?;
    let global = crate::freertos::symbol(syms, "lv_global")?;
    let g = layouts.require("_lv_global_t")?;
    let d = layouts.require("_lv_display_t")?;
    let display = g.u32(mem, global, "disp_default")?;
    if display == 0 {
        return Err(IntrospectError::LayoutMismatch {
            what: "lv_global.disp_default",
            detail: "is null, so LVGL has no default display yet".into(),
        });
    }
    let screen_cnt = d.u32(mem, display, "screen_cnt")?;
    if screen_cnt > 16 {
        return Err(IntrospectError::LayoutMismatch {
            what: "_lv_display_t.screen_cnt",
            detail: format!("{screen_cnt} is past the limit of 16 screens"),
        });
    }
    let screens = d.u32(mem, display, "screens")?;
    let screen = d.u32(mem, display, "act_scr")?;
    let mut in_screens = false;
    for i in 0..screen_cnt {
        if mem.u32(screens.wrapping_add(i * 4))? == screen {
            in_screens = true;
        }
    }
    if !in_screens {
        return Err(IntrospectError::LayoutMismatch {
            what: "_lv_display_t.act_scr",
            detail: format!("{screen:#010x} is not one of the {screen_cnt} screens"),
        });
    }
    let mut tree = UiTree {
        rev,
        display,
        hor_res: d.i32(mem, display, "hor_res")?,
        ver_res: d.i32(mem, display, "ver_res")?,
        screen,
        ..UiTree::default()
    };
    // Pre-order, iterative, so a deep tree cannot overflow the host stack.
    let mut stack = vec![(screen, 0u32, 0u32)];
    while let Some((obj, parent, depth)) = stack.pop() {
        if tree.nodes.len() >= MAX_OBJECTS {
            tree.warnings.push(Warning {
                what: "lv_obj tree",
                at: obj,
                detail: format!("the tree holds more than {MAX_OBJECTS} objects"),
            });
            break;
        }
        let node = match read_node(layouts, mem, obj, parent, depth) {
            Ok(node) => node,
            // An unreadable active screen means the offsets are wrong, so nothing below it
            // can be trusted.
            Err(e) if depth == 0 => return Err(e),
            Err(e) => {
                tree.warnings.push(Warning {
                    what: "lv_obj",
                    at: obj,
                    detail: e.to_string(),
                });
                continue;
            }
        };
        if depth == 0 && !node.class_chain.iter().any(|c| c == "lv_obj") {
            return Err(IntrospectError::LayoutMismatch {
                what: "_lv_obj_t.class_p",
                detail: format!(
                    "the active screen's class chain is {:?}, which does not reach lv_obj",
                    node.class_chain
                ),
            });
        }
        let children = node.children.clone();
        tree.nodes.push(node);
        if depth >= MAX_DEPTH {
            tree.warnings.push(Warning {
                what: "lv_obj tree",
                at: obj,
                detail: format!("the tree is deeper than {MAX_DEPTH} levels"),
            });
            continue;
        }
        // Reversed so the pre-order walk visits children in LVGL order.
        for child in children.iter().rev() {
            match layouts
                .require("_lv_obj_t")
                .and_then(|o| o.u32(mem, *child, "parent"))
            {
                Ok(back) if back == obj => stack.push((*child, obj, depth + 1)),
                Ok(back) => tree.warnings.push(Warning {
                    what: "lv_obj",
                    at: *child,
                    detail: format!(
                        "its parent is {back:#010x}, not {obj:#010x}; the child is not walked"
                    ),
                }),
                Err(e) => tree.warnings.push(Warning {
                    what: "lv_obj",
                    at: *child,
                    detail: e.to_string(),
                }),
            }
        }
    }
    for (i, node) in tree.nodes.iter_mut().enumerate() {
        node.reference = format!("e{}", i + 1);
    }
    Ok(tree)
}

fn read_node(
    layouts: &Layouts,
    mem: &dyn GuestMemory,
    obj: u32,
    parent: u32,
    depth: u32,
) -> Result<UiNode, IntrospectError> {
    let o = layouts.require("_lv_obj_t")?;
    let coords = obj.wrapping_add(o.offset("coords")?);
    let (x1, y1) = (mem.i32(coords)?, mem.i32(coords.wrapping_add(4))?);
    let (x2, y2) = (
        mem.i32(coords.wrapping_add(8))?,
        mem.i32(coords.wrapping_add(12))?,
    );
    let class_chain = read_class_chain(layouts, mem, o.u32(mem, obj, "class_p")?)?;
    let class = class_chain
        .first()
        .map(|c| c.trim_start_matches("lv_").to_string())
        .unwrap_or_else(|| "?".into());
    let mut node = UiNode {
        obj,
        reference: String::new(),
        class,
        parent,
        depth,
        children: read_children(layouts, mem, obj)?,
        x: x1,
        y: y1,
        w: x2.saturating_sub(x1).saturating_add(1),
        h: y2.saturating_sub(y1).saturating_add(1),
        flags: o.u32(mem, obj, "flags")?,
        state: o.u16(mem, obj, "state")?,
        layout_inv: o.flag(mem, obj, "layout_inv")?,
        ..UiNode::default()
    };
    if class_chain.iter().any(|c| c == "lv_label")
        && let Ok(label) = layouts.require("_lv_label_t")
        && let Ok(text) = label.u32(mem, obj, "text")
        && text != 0
    {
        node.text = Some(mem.cstr(text, MAX_TEXT));
    }
    if class_chain.iter().any(|c| c == "lv_image")
        && let Ok(image) = layouts.require("_lv_image_t")
        && let (Ok(src), Ok(kind)) = (image.u32(mem, obj, "src"), image.bits(mem, obj, "src_type"))
    {
        node.image = Some((src, kind));
    }
    if class_chain.iter().any(|c| c == "lv_bar")
        && let Ok(bar) = layouts.require("_lv_bar_t")
        && let (Ok(cur), Ok(min), Ok(max)) = (
            bar.i32(mem, obj, "cur_value"),
            bar.i32(mem, obj, "min_value"),
            bar.i32(mem, obj, "max_value"),
        )
    {
        node.value = Some((cur, min, max));
    }
    node.class_chain = class_chain;
    read_local_styles(layouts, mem, &mut node);
    Ok(node)
}

/// Follows `class_p` and its `base_class` chain, innermost first. A name that is not a
/// printable C string beginning `lv_` means the offsets are wrong.
fn read_class_chain(
    layouts: &Layouts,
    mem: &dyn GuestMemory,
    class_p: u32,
) -> Result<Vec<String>, IntrospectError> {
    let c = layouts.require("_lv_obj_class_t")?;
    let mut out = Vec::new();
    let mut at = class_p;
    while at != 0 && out.len() < 8 {
        let name_ptr = c.u32(mem, at, "name")?;
        if name_ptr == 0 {
            break;
        }
        let name = mem.cstr(name_ptr, 32);
        if !name.starts_with("lv_") || !name.chars().all(|ch| ch.is_ascii_graphic()) {
            return Err(IntrospectError::LayoutMismatch {
                what: "_lv_obj_class_t.name",
                detail: format!("{name:?} is not a printable C string beginning lv_"),
            });
        }
        out.push(name);
        at = c.u32(mem, at, "base_class")?;
    }
    Ok(out)
}

/// `spec_attr->children`, null for a leaf.
fn read_children(
    layouts: &Layouts,
    mem: &dyn GuestMemory,
    obj: u32,
) -> Result<Vec<u32>, IntrospectError> {
    let o = layouts.require("_lv_obj_t")?;
    let spec = o.u32(mem, obj, "spec_attr")?;
    if spec == 0 {
        return Ok(Vec::new());
    }
    let s = layouts.require("_lv_obj_spec_attr_t")?;
    let array = s.u32(mem, spec, "children")?;
    // `child_cnt` is read at its DWARF width: on the official image it is a `uint16_t`
    // sharing a word with the scrollbar, snap, scroll-direction and layer bitfields.
    let count = s.uint(mem, spec, "child_cnt")?;
    if array == 0 || count == 0 {
        return Ok(Vec::new());
    }
    if count as usize > MAX_OBJECTS {
        return Err(IntrospectError::LayoutMismatch {
            what: "_lv_obj_spec_attr_t.child_cnt",
            detail: format!("{count} is past the {MAX_OBJECTS} object budget"),
        });
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        out.push(mem.u32(array.wrapping_add(i * 4))?);
    }
    Ok(out)
}

/// Reads the background and border colors from the object's local styles. An unreadable style
/// only costs the node its `bg=`/`border=` fields, so it is skipped, not reported.
fn read_local_styles(layouts: &Layouts, mem: &dyn GuestMemory, node: &mut UiNode) {
    let (Ok(obj), Ok(entry), Ok(style)) = (
        layouts.require("_lv_obj_t"),
        layouts.require("_lv_obj_style_t"),
        layouts.require("lv_style_t"),
    ) else {
        return;
    };
    let (Ok(array), Ok(count)) = (
        obj.u32(mem, node.obj, "styles"),
        obj.bits(mem, node.obj, "style_cnt"),
    ) else {
        return;
    };
    if array == 0 {
        return;
    }
    for i in 0..count {
        let at = array.wrapping_add(i * entry.size);
        let (Ok(is_local), Ok(style_ptr)) =
            (entry.flag(mem, at, "is_local"), entry.u32(mem, at, "style"))
        else {
            continue;
        };
        if !is_local || style_ptr == 0 {
            continue;
        }
        let (Ok(values), Ok(prop_cnt)) = (
            style.u32(mem, style_ptr, "values_and_props"),
            style.u8(mem, style_ptr, "prop_cnt"),
        ) else {
            continue;
        };
        if values == 0 || prop_cnt == 0 || prop_cnt == CONST_STYLE {
            continue;
        }
        // The values array comes first, followed by the u8 property ids.
        let props = values.wrapping_add(u32::from(prop_cnt) * 4);
        for j in 0..u32::from(prop_cnt) {
            let (Ok(id), Ok(raw)) = (
                mem.u8(props.wrapping_add(j)),
                mem.u32(values.wrapping_add(j * 4)),
            ) else {
                continue;
            };
            // `lv_color_t` byte order is {blue, green, red}.
            let color = Color {
                b: raw as u8,
                g: (raw >> 8) as u8,
                r: (raw >> 16) as u8,
            };
            match id {
                STYLE_BG_COLOR => node.bg = Some(color),
                STYLE_BORDER_COLOR => node.border = Some(color),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod synthetic {
    //! A synthetic LVGL 9.5.0 image with the layouts the official ELF resolves to and the shape
    //! of the official menu.

    use super::*;
    use crate::MemoryImage;
    use crate::layout::{Bitfield, MemberLayout, StructLayout};
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};

    pub const GLOBAL: u32 = 0x3fca_1b34;
    pub const PORT_CTX: u32 = 0x3fca_1b14;
    pub const MUX: u32 = 0x3fcb_0c40;
    pub const LVGL_TASK: u32 = 0x3fcb_0400;
    pub const TIMER_HANDLER: u32 = 0x4204_f2be;
    pub const DISPLAY: u32 = 0x3fca_2000;
    pub const SCREENS: u32 = 0x3fca_2400;
    pub const SCREEN: u32 = 0x3fca_3b18;
    /// The header container, styled paper on ink, which pruning therefore keeps.
    pub const HEADER: u32 = 0x3fca_4b94;
    /// Clickable, with the selected colors.
    pub const CARD1: u32 = 0x3fca_4e5c;
    pub const CARD2: u32 = 0x3fca_4ec4;
    /// Decorative, dropped by pruning.
    pub const DECO: u32 = 0x3fca_6000;
    /// Hidden, dropped by pruning.
    pub const HIDDEN: u32 = 0x3fca_6200;
    /// `flags` of a plain `lv_obj` under a parent, as the official menu reads it.
    pub const OBJ_FLAGS: u32 = 0xbb66;
    /// `flags` of the active screen, which has none of the parent-only flags.
    pub const SCREEN_FLAGS: u32 = 0x1866;
    /// The label constructor clears `CLICKABLE`.
    pub const LABEL_FLAGS: u32 = 0xbb74;
    pub const OBJ_CLASS: u32 = 0x3c13_d9a0;
    const LABEL_CLASS: u32 = 0x3c14_6fd4;

    fn member(path: &str, offset: u32) -> MemberLayout {
        MemberLayout {
            path: path.to_string(),
            offset,
            bits: None,
            size: None,
        }
    }

    fn bitfield(path: &str, offset: u32, bit: u8, width: u32) -> MemberLayout {
        MemberLayout {
            path: path.to_string(),
            offset,
            bits: Some(Bitfield { bit, width }),
            size: None,
        }
    }

    /// The LVGL layouts as the official ELF resolves them.
    pub fn layouts() -> Layouts {
        let mut l = crate::freertos::synthetic::layouts();
        l.insert(StructLayout::new(
            "_lv_obj_t",
            48,
            vec![
                member("class_p", 0),
                member("parent", 4),
                member("spec_attr", 8),
                member("styles", 12),
                member("user_data", 16),
                member("coords", 20),
                member("flags", 36),
                member("state", 40),
                bitfield("layout_inv", 42, 0, 1),
                bitfield("scr_layout_inv", 42, 2, 1),
                bitfield("style_cnt", 42, 4, 6),
                bitfield("is_deleting", 43, 6, 1),
            ],
        ));
        l.insert(StructLayout::new(
            "_lv_obj_spec_attr_t",
            52,
            vec![
                member("children", 0),
                MemberLayout {
                    size: Some(2),
                    ..member("child_cnt", 48)
                },
                member("scroll", 32),
            ],
        ));
        l.insert(StructLayout::new(
            "_lv_obj_class_t",
            36,
            vec![
                member("base_class", 0),
                member("name", 20),
                bitfield("instance_size", 32, 4, 16),
            ],
        ));
        l.insert(StructLayout::new(
            "_lv_obj_style_t",
            8,
            vec![
                member("style", 0),
                bitfield("selector", 4, 0, 24),
                bitfield("is_local", 7, 0, 1),
            ],
        ));
        l.insert(StructLayout::new(
            "lv_style_t",
            12,
            vec![member("values_and_props", 0), member("prop_cnt", 8)],
        ));
        l.insert(StructLayout::new(
            "_lv_display_t",
            812,
            vec![
                member("hor_res", 0),
                member("ver_res", 4),
                member("screens", 708),
                member("sys_layer", 712),
                member("top_layer", 716),
                member("act_scr", 720),
                member("bottom_layer", 724),
                member("prev_scr", 728),
                member("scr_to_load", 732),
                member("screen_cnt", 736),
                bitfield("rendering_in_progress", 69, 2, 1),
                member("inv_p", 620),
            ],
        ));
        l.insert(StructLayout::new(
            "_lv_global_t",
            504,
            vec![
                member("inited", 4),
                member("disp_ll", 8),
                member("disp_refresh", 20),
                member("disp_default", 24),
                member("indev_ll", 72),
                member("tick_state", 188),
                member("tlsf_state", 464),
            ],
        ));
        l.insert(StructLayout::new(
            "_lv_label_t",
            108,
            vec![member("text", 48), bitfield("long_mode", 96, 0, 4)],
        ));
        l.insert(StructLayout::new(
            "lvgl_port_ctx_t",
            32,
            vec![member("lvgl_task", 0), member("lvgl_mux", 4)],
        ));
        l
    }

    fn object(name: &str, addr: u32, size: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size,
            kind: SymKind::Object,
            bind: SymBind::Global,
            section: SymSection::Index(1),
        }
    }

    pub fn symbols() -> SymbolTable {
        let mut syms: Vec<Symbol> = crate::freertos::synthetic::symbols()
            .iter()
            .cloned()
            .collect();
        syms.push(object("lv_global", GLOBAL, 504));
        syms.push(object("lvgl_port_ctx", PORT_CTX, 32));
        syms.push(Symbol {
            kind: SymKind::Func,
            ..object("lv_timer_handler", TIMER_HANDLER, 398)
        });
        SymbolTable::new(syms)
    }

    struct Obj {
        at: u32,
        class: u32,
        parent: u32,
        rect: (i32, i32, i32, i32),
        flags: u32,
        children: Vec<u32>,
    }

    fn write_obj(mem: &mut MemoryImage, o: &Obj, spec: u32, children_array: u32) {
        mem.map_zeroed(o.at, 48);
        mem.put_u32(o.at, o.class);
        mem.put_u32(o.at + 4, o.parent);
        mem.put_u32(o.at + 36, o.flags);
        let (x, y, w, h) = o.rect;
        mem.put_u32(o.at + 20, x as u32);
        mem.put_u32(o.at + 24, y as u32);
        mem.put_u32(o.at + 28, (x + w - 1) as u32);
        mem.put_u32(o.at + 32, (y + h - 1) as u32);
        if o.children.is_empty() {
            return;
        }
        mem.map_zeroed(spec, 52);
        mem.map_zeroed(children_array, o.children.len() * 4);
        mem.put_u32(o.at + 8, spec);
        mem.put_u32(spec, children_array);
        mem.put_u32(spec + 48, o.children.len() as u32);
        for (i, c) in o.children.iter().enumerate() {
            mem.put_u32(children_array + (i as u32) * 4, *c);
        }
    }

    fn write_label(
        mem: &mut MemoryImage,
        at: u32,
        parent: u32,
        rect: (i32, i32, i32, i32),
        text: u32,
    ) {
        write_obj(
            mem,
            &Obj {
                at,
                class: LABEL_CLASS,
                parent,
                rect,
                flags: LABEL_FLAGS,
                children: Vec::new(),
            },
            0,
            0,
        );
        // `_lv_label_t` extends `_lv_obj_t`, so the text pointer sits past the base object.
        mem.map_zeroed(at + 48, 108 - 48);
        mem.put_u32(at + 48, text);
    }

    fn write_style(mem: &mut MemoryImage, obj: u32, base: u32, bg: u32, border: u32) {
        mem.map_zeroed(base, 8);
        mem.map_zeroed(base + 0x100, 12);
        mem.map_zeroed(base + 0x200, 16);
        mem.put_u32(obj + 12, base);
        // style_cnt is bits 4..9 of byte 42.
        mem.put(obj + 42, &[1 << 4]);
        mem.put_u32(base, base + 0x100);
        mem.put(base + 7, &[1]); // is_local
        mem.put_u32(base + 0x100, base + 0x200);
        mem.put(base + 0x100 + 8, &[2]); // prop_cnt
        mem.put_u32(base + 0x200, bg);
        mem.put_u32(base + 0x204, border);
        mem.put(base + 0x208, &[STYLE_BG_COLOR, STYLE_BORDER_COLOR]);
    }

    /// The official menu in miniature: a screen, a styled header holding a label, two styled
    /// cards each holding a label, a decorative leaf and a hidden label, with the official flags.
    pub fn image() -> MemoryImage {
        let mut mem = crate::freertos::synthetic::image();
        mem.map_zeroed(GLOBAL, 504);
        mem.map_zeroed(DISPLAY, 812);
        mem.map_zeroed(SCREENS, 4);
        mem.map_zeroed(PORT_CTX, 32);
        mem.map_zeroed(OBJ_CLASS, 36);
        mem.map_zeroed(LABEL_CLASS, 36);
        mem.map(0x3c13_0000, b"lv_obj\0lv_label\0".to_vec());
        mem.map(0x3c13_1000, b"FoloToy\0Display\0Button\0secret\0".to_vec());

        mem.put_u32(GLOBAL + 24, DISPLAY);
        mem.put_u32(DISPLAY, 240);
        mem.put_u32(DISPLAY + 4, 320);
        mem.put_u32(DISPLAY + 708, SCREENS);
        mem.put_u32(DISPLAY + 720, SCREEN);
        mem.put_u32(DISPLAY + 736, 1);
        mem.put_u32(SCREENS, SCREEN);
        mem.put_u32(PORT_CTX, LVGL_TASK);
        mem.put_u32(PORT_CTX + 4, MUX);
        mem.put_u32(OBJ_CLASS + 20, 0x3c13_0000);
        mem.put_u32(LABEL_CLASS, OBJ_CLASS);
        mem.put_u32(LABEL_CLASS + 20, 0x3c13_0007);

        write_obj(
            &mut mem,
            &Obj {
                at: SCREEN,
                class: OBJ_CLASS,
                parent: 0,
                rect: (0, 0, 240, 320),
                flags: SCREEN_FLAGS,
                children: vec![HEADER, CARD1, CARD2, DECO, HIDDEN],
            },
            0x3fca_7000,
            0x3fca_7100,
        );
        write_obj(
            &mut mem,
            &Obj {
                at: HEADER,
                class: OBJ_CLASS,
                parent: SCREEN,
                rect: (5, 8, 151, 33),
                flags: OBJ_FLAGS,
                children: vec![0x3fca_4c2c],
            },
            0x3fca_7200,
            0x3fca_7300,
        );
        write_label(&mut mem, 0x3fca_4c2c, HEADER, (42, 13, 76, 22), 0x3c13_1000);
        write_obj(
            &mut mem,
            &Obj {
                at: CARD1,
                class: OBJ_CLASS,
                parent: SCREEN,
                rect: (11, 52, 102, 40),
                flags: OBJ_FLAGS,
                children: vec![0x3fca_4efc],
            },
            0x3fca_7400,
            0x3fca_7500,
        );
        write_label(&mut mem, 0x3fca_4efc, CARD1, (36, 64, 53, 16), 0x3c13_1008);
        write_obj(
            &mut mem,
            &Obj {
                at: CARD2,
                class: OBJ_CLASS,
                parent: SCREEN,
                rect: (123, 52, 102, 40),
                flags: OBJ_FLAGS,
                children: vec![0x3fca_51d0],
            },
            0x3fca_7600,
            0x3fca_7700,
        );
        write_label(&mut mem, 0x3fca_51d0, CARD2, (148, 64, 52, 16), 0x3c13_1010);
        write_obj(
            &mut mem,
            &Obj {
                at: DECO,
                class: OBJ_CLASS,
                parent: SCREEN,
                rect: (189, 15, 43, 10),
                flags: OBJ_FLAGS,
                children: Vec::new(),
            },
            0,
            0,
        );
        write_label(&mut mem, HIDDEN, SCREEN, (0, 286, 240, 34), 0x3c13_1017);
        mem.put_u32(HIDDEN + 36, LABEL_FLAGS | FLAG_HIDDEN);
        // The selected card is yellow on white, the other and the header paper on ink.
        write_style(&mut mem, CARD1, 0x3fcb_1000, 0x00ff_d928, 0x00ff_ffff);
        write_style(&mut mem, CARD2, 0x3fcb_2000, 0x00f4_f4ea, 0x0017_202a);
        write_style(&mut mem, HEADER, 0x3fcb_3000, 0x00f4_f4ea, 0x0017_202a);
        mem
    }
}

#[cfg(test)]
mod tests {
    use super::synthetic::*;
    use super::*;

    #[test]
    fn the_ui_walk_prunes_semantically_and_numbers_refs_in_pre_order() {
        let tree = walk_ui(&layouts(), &symbols(), &image(), 7).expect("the walk succeeds");
        assert!(tree.warnings.is_empty(), "{:?}", tree.warnings);
        assert_eq!((tree.hor_res, tree.ver_res), (240, 320));
        assert_eq!(tree.screen, SCREEN);
        // Refs are assigned in pre-order over every walked object, pruned or not.
        assert_eq!(tree.nodes.len(), 9);
        assert_eq!(tree.node("e1").map(|n| n.obj), Some(SCREEN));
        assert_eq!(tree.node("e4").map(|n| n.obj), Some(CARD1));
        assert_eq!(tree.node("e9").map(|n| n.obj), Some(HIDDEN));
        assert_eq!(
            tree.render(Prune::Semantic, false),
            "- obj [0,0 240x320] e1\n  \
             - obj [5,8 151x33] e2\n    \
             - label \"FoloToy\" [42,13 76x22] e3\n  \
             - obj [11,52 102x40] e4\n    \
             - label \"Display\" [36,64 53x16] e5\n  \
             - obj [123,52 102x40] e6\n    \
             - label \"Button\" [148,64 52x16] e7\n"
        );
        let full = tree.render(Prune::None, false);
        assert_eq!(full.lines().count(), 9);
        assert!(full.contains("- obj [189,15 43x10] e8"));
    }

    /// The official screen reads 0x03c3_0027 in the `child_cnt` word; reading the whole word would
    /// refuse the real menu as past the object budget.
    #[test]
    fn the_child_count_ignores_the_bitfields_above_it_in_the_same_word() {
        let mut mem = image();
        let spec = 0x3fca_7000;
        mem.put(spec + 50, &[0xc3, 0x03]);
        let tree = walk_ui(&layouts(), &symbols(), &mem, 1).expect("the walk succeeds");
        assert_eq!(tree.nodes.len(), 9, "{:?}", tree.warnings);
    }

    #[test]
    fn local_styles_give_the_background_and_border_colors() {
        let tree = walk_ui(&layouts(), &symbols(), &image(), 1).expect("the walk succeeds");
        let card1 = tree.at(CARD1).expect("the first card");
        assert_eq!(card1.bg.map(Color::render).as_deref(), Some("#ffd928"));
        assert_eq!(card1.border.map(Color::render).as_deref(), Some("#ffffff"));
        let card2 = tree.at(CARD2).expect("the second card");
        assert_eq!(card2.bg.map(Color::render).as_deref(), Some("#f4f4ea"));
        assert_eq!(card2.border.map(Color::render).as_deref(), Some("#17202a"));
        assert!(
            tree.render(Prune::Semantic, true)
                .contains("- obj [11,52 102x40] bg=#ffd928 border=#ffffff e4")
        );
    }

    #[test]
    fn a_styled_single_child_container_survives_the_collapse() {
        let (layouts, syms) = (layouts(), symbols());
        let styled = walk_ui(&layouts, &syms, &image(), 1).expect("the walk succeeds");
        assert!(
            styled
                .render(Prune::Semantic, true)
                .contains("  - obj [5,8 151x33] bg=#f4f4ea border=#17202a e2\n"),
            "{}",
            styled.render(Prune::Semantic, true)
        );
        // Without the header's local style, the plain container collapses into its label.
        let mut mem = image();
        mem.put_u32(HEADER + 12, 0);
        mem.put(HEADER + 42, &[0]);
        let plain = walk_ui(&layouts, &syms, &mem, 1).expect("the walk succeeds");
        let header = plain.at(HEADER).expect("the header is still walked");
        assert_eq!((header.bg, header.border), (None, None));
        assert!(!header.is_semantic());
        assert_eq!(
            plain.render(Prune::Semantic, true),
            "- obj [0,0 240x320] e1\n  \
             - label \"FoloToy\" [42,13 76x22] e3\n  \
             - obj [11,52 102x40] bg=#ffd928 border=#ffffff e4\n    \
             - label \"Display\" [36,64 53x16] e5\n  \
             - obj [123,52 102x40] bg=#f4f4ea border=#17202a e6\n    \
             - label \"Button\" [148,64 52x16] e7\n"
        );
    }

    #[test]
    fn clickable_is_a_role_only_on_a_widget_class() {
        let (layouts, syms) = (layouts(), symbols());
        let tree = walk_ui(&layouts, &syms, &image(), 1).expect("the walk succeeds");
        for obj in [HEADER, CARD1, CARD2, DECO] {
            let node = tree.at(obj).expect("walked");
            assert_eq!(
                node.flags & FLAG_CLICKABLE,
                FLAG_CLICKABLE,
                "{}",
                node.reference
            );
            assert!(!node.is_interactive(), "{}", node.reference);
            assert!(!node.is_semantic(), "{}", node.reference);
        }
        assert!(
            !tree
                .render(Prune::Semantic, false)
                .contains("[189,15 43x10]")
        );

        let button_class = 0x3c13_da00;
        let mut mem = image();
        mem.map(0x3c13_2000, b"lv_button\0".to_vec());
        mem.map_zeroed(button_class, 36);
        mem.put_u32(button_class, OBJ_CLASS);
        mem.put_u32(button_class + 20, 0x3c13_2000);
        mem.put_u32(DECO, button_class);
        let tree = walk_ui(&layouts, &syms, &mem, 1).expect("the walk succeeds");
        let button = tree.at(DECO).expect("walked");
        assert_eq!(button.class_chain, ["lv_button", "lv_obj"]);
        assert!(button.is_interactive() && button.is_semantic());
        assert!(
            tree.render(Prune::Semantic, false)
                .contains("  - button [189,15 43x10] e8\n"),
            "{}",
            tree.render(Prune::Semantic, false)
        );
        mem.put_u32(DECO + 36, OBJ_FLAGS & !FLAG_CLICKABLE);
        let tree = walk_ui(&layouts, &syms, &mem, 1).expect("the walk succeeds");
        assert!(!tree.at(DECO).expect("walked").is_semantic());
    }

    #[test]
    fn a_ref_from_an_older_revision_resolves_or_reports_e_stale_ref() {
        let (layouts, syms) = (layouts(), symbols());
        let old = walk_ui(&layouts, &syms, &image(), 1).expect("the first walk");
        let same = walk_ui(&layouts, &syms, &image(), 2).expect("the second walk");
        assert_eq!(same.resolve_stale("e4", &old).map(|n| n.obj), Ok(CARD1));
        // The menu is rebuilt: the address now holds a label, not a card.
        let mut mem = image();
        mem.put_u32(CARD1, 0x3c14_6fd4);
        let rebuilt = walk_ui(&layouts, &syms, &mem, 3).expect("the third walk");
        let err = rebuilt.resolve_stale("e4", &old).expect_err("E_STALE_REF");
        assert!(matches!(err, IntrospectError::StaleRef { .. }));
        assert!(err.to_string().starts_with("E_STALE_REF: e4 of ui_rev 1:"));
        assert!(err.to_string().contains("now holds a label"));
    }

    #[test]
    fn the_sanity_checks_refuse_a_wrong_layout() {
        let (layouts, syms) = (layouts(), symbols());
        let mut mem = image();
        mem.put_u32(DISPLAY + 736, 17);
        assert!(matches!(
            walk_ui(&layouts, &syms, &mem, 1),
            Err(IntrospectError::LayoutMismatch {
                what: "_lv_display_t.screen_cnt",
                ..
            })
        ));
        let mut mem = image();
        mem.put_u32(SCREENS, 0x1234_5678);
        assert!(matches!(
            walk_ui(&layouts, &syms, &mem, 1),
            Err(IntrospectError::LayoutMismatch {
                what: "_lv_display_t.act_scr",
                ..
            })
        ));
        let mut mem = image();
        mem.put(0x3c13_0000, b"\x01\x02\x03\0");
        assert!(matches!(
            walk_ui(&layouts, &syms, &mem, 1),
            Err(IntrospectError::LayoutMismatch {
                what: "_lv_obj_class_t.name",
                ..
            })
        ));
        let mut mem = image();
        mem.put_u32(CARD2 + 4, 0xdead_beef);
        let tree = walk_ui(&layouts, &syms, &mem, 1).expect("the walk still returns");
        let w = tree
            .warnings
            .iter()
            .find(|w| w.at == CARD2)
            .expect("the child is reported");
        assert!(w.detail.contains("is not walked"));
        assert!(tree.at(CARD2).is_none());
    }
}
