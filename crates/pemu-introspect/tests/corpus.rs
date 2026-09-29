//! Introspection over the official app ELF of the corpus.
//!
//! The ELF is found through `corpus/MANIFEST.json` below the data root and checked against the
//! SHA-256 it pins. Every test skips with a printed reason when the data root, manifest or file is
//! absent, and takes the `t1_` prefix so it runs only in T1. No test prints file contents.

// Test-only file and env access; the core-crate clippy.toml bans target non-test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;

use pemu_introspect::MemoryImage;
use pemu_introspect::dwarf::DebugInfo;
use pemu_introspect::layout::Layouts;
use pemu_introspect::unwind::{FrameOrigin, MAX_FRAMES, Registers, Symbolizer, Unwinder};
use pemu_loader::elf::ElfInfo;
use pemu_loader::{hex, sha256};

/// The data root, read as `pemu_testkit::corpus` reads it: from `PASSPORTSIM_DATA_ROOT` only,
/// when absolute. This crate may not depend on `pemu-testkit`, hence the local copy.
fn data_root() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("PASSPORTSIM_DATA_ROOT")
        .filter(|d| !d.to_string_lossy().trim().is_empty())
    else {
        eprintln!("skip: no data root: set PASSPORTSIM_DATA_ROOT to an absolute path");
        return None;
    };
    let root = PathBuf::from(dir);
    if !root.is_absolute() {
        eprintln!("skip: PASSPORTSIM_DATA_ROOT is not an absolute path");
        return None;
    }
    Some(root)
}

/// Whether manifest file name `file` is corpus file `key`: `elf` is the app ELF.
fn is_key(file: &str, key: &str) -> bool {
    match key {
        "elf" => file.ends_with(".elf") && !file.starts_with("bootloader"),
        _ => false,
    }
}

fn field(record: &str, key: &str) -> Option<String> {
    let at = record.find(&format!("\"{key}\""))?;
    let rest = &record[at + key.len() + 2..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].replace("\\\\", "\\"))
}

/// Corpus file `key` of entry `id`, checked against its pinned SHA-256, or `None` after
/// printing why the test skips.
pub fn corpus(id: &str, key: &str) -> Option<Vec<u8>> {
    let manifest = data_root()?.join("corpus/MANIFEST.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        eprintln!("skip: the corpus manifest is absent");
        return None;
    };
    let record = text.split('{').find(|r| {
        field(r, "id").as_deref() == Some(id) && field(r, "file").is_some_and(|f| is_key(&f, key))
    });
    let (Some(path), Some(digest)) = record
        .map(|r| (field(r, "path"), field(r, "sha256")))
        .unwrap_or((None, None))
    else {
        eprintln!("skip: the corpus manifest has no {id}.{key}");
        return None;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skip: corpus file {id}.{key} is absent");
        return None;
    };
    assert_eq!(
        hex(&sha256(&bytes)),
        digest,
        "{id}.{key} is not the file MANIFEST.json pins"
    );
    Some(bytes)
}

/// Prints the resolved layouts of the official ELF, to regenerate the table below: run with
/// `--ignored --nocapture`.
#[test]
#[ignore = "prints the layout table rather than asserting; run with --ignored --nocapture"]
fn t1_print_official_layouts() {
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    println!("cus={} build_root={:?}", info.units(), info.build_root());
    let layouts: Layouts = info.layouts();
    print!("{}", layouts.render());
    println!("missing structs: {:?}", layouts.missing_structs());
    println!("missing members: {:?}", layouts.missing_members());
}

/// Every checked structure of the official ELF, as `(struct, size, [(member, layout)])` with the
/// `offset` or `offset:bit/width` form.
type SpikeStruct = (&'static str, u32, &'static [(&'static str, &'static str)]);

const SPIKE_LAYOUTS: &[SpikeStruct] = &[
    (
        "_lv_bar_t",
        120,
        &[
            ("cur_value", "48"),
            ("min_value", "52"),
            ("max_value", "56"),
        ],
    ),
    (
        "_lv_display_t",
        812,
        &[
            ("hor_res", "0"),
            ("ver_res", "4"),
            ("rendering_in_progress", "69:2/1"),
            ("inv_p", "620"),
            ("screens", "708"),
            ("sys_layer", "712"),
            ("top_layer", "716"),
            ("act_scr", "720"),
            ("bottom_layer", "724"),
            ("prev_scr", "728"),
            ("scr_to_load", "732"),
            ("screen_cnt", "736"),
        ],
    ),
    (
        "_lv_global_t",
        504,
        &[
            ("inited", "4"),
            ("disp_ll", "8"),
            ("disp_refresh", "20"),
            ("disp_default", "24"),
            ("indev_ll", "72"),
            ("tick_state", "188"),
            ("tlsf_state", "464"),
        ],
    ),
    (
        "_lv_image_t",
        96,
        &[
            ("src", "48"),
            ("w", "64"),
            ("h", "68"),
            ("src_type", "92:0/2"),
        ],
    ),
    (
        "_lv_label_t",
        108,
        &[("text", "48"), ("long_mode", "96:0/4")],
    ),
    (
        "_lv_obj_class_t",
        36,
        &[
            ("base_class", "0"),
            ("name", "20"),
            ("instance_size", "32:4/16"),
        ],
    ),
    (
        "_lv_obj_spec_attr_t",
        52,
        &[("children", "0"), ("scroll", "32"), ("child_cnt", "48")],
    ),
    (
        "_lv_obj_t",
        48,
        &[
            ("class_p", "0"),
            ("parent", "4"),
            ("spec_attr", "8"),
            ("user_data", "16"),
            ("coords", "20"),
            ("flags", "36"),
            ("state", "40"),
            ("scr_layout_inv", "42:2/1"),
        ],
    ),
    (
        "heap_t_",
        36,
        &[
            ("caps", "0"),
            ("start", "12"),
            ("end", "16"),
            ("heap_mux", "20"),
            ("heap", "28"),
            ("next", "32"),
        ],
    ),
    (
        "multi_heap_info",
        20,
        &[
            ("lock", "0"),
            ("free_bytes", "4"),
            ("minimum_free_bytes", "8"),
            ("pool_size", "12"),
            ("heap_data", "16"),
        ],
    ),
    (
        "tskTaskControlBlock",
        336,
        &[
            ("pxTopOfStack", "0"),
            ("xStateListItem", "4"),
            ("xEventListItem", "24"),
            ("uxPriority", "44"),
            ("pxStack", "48"),
            ("pcTaskName", "52"),
            ("pxEndOfStack", "68"),
            ("uxBasePriority", "72"),
        ],
    ),
    (
        "xLIST",
        20,
        &[
            ("uxNumberOfItems", "0"),
            ("pxIndex", "4"),
            ("xListEnd", "8"),
        ],
    ),
    (
        "xLIST_ITEM",
        20,
        &[
            ("xItemValue", "0"),
            ("pxNext", "4"),
            ("pxPrevious", "8"),
            ("pvOwner", "12"),
            ("pxContainer", "16"),
        ],
    ),
];

#[test]
fn t1_official_layouts_equal_the_spike_outputs() {
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    assert!(info.has_debug_info(), "the official build ships DWARF 4");
    let layouts: Layouts = info.layouts();
    assert_eq!(layouts.missing_structs(), &[] as &[String]);
    assert_eq!(layouts.missing_members(), &[] as &[String]);
    for (name, size, members) in SPIKE_LAYOUTS {
        let s = layouts
            .get(name)
            .unwrap_or_else(|| panic!("no layout for {name}"));
        assert_eq!(s.size, *size, "{name} size");
        for (member, expected) in *members {
            let m = s
                .member(member)
                .unwrap_or_else(|| panic!("{name} has no member {member}"));
            assert_eq!(&m.render(), expected, "{name}.{member}");
        }
    }
    layouts
        .check_lvgl()
        .expect("official LVGL layouts pass their sanity check");
}

/// The nested and typedef paths: the mutex holder inside an anonymous union, the TLSF control
/// block, `lvgl_port_ctx` and the RISC-V exception frame.
#[test]
fn t1_official_nested_and_typedef_layouts_resolve() {
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    let layouts = info.layouts();
    let q = layouts.require("QueueDefinition").expect("QueueDefinition");
    assert_eq!(q.offset("u.xSemaphore.xMutexHolder"), Ok(8));
    assert_eq!(q.offset("u.xSemaphore.uxRecursiveCallCount"), Ok(12));
    assert_eq!(q.offset("xTasksWaitingToReceive"), Ok(36));
    // `control_t` bitfields: sl_index_count at absolute bit 16:14 is byte 17 bit 6,
    // small_block_size 16:23 is byte 18 bit 7.
    let c = layouts.require("control_t").expect("control_t");
    assert_eq!(c.offset("size"), Ok(20));
    assert_eq!(
        c.member("sl_index_count").map(|m| m.render()),
        Some("17:6/6".into())
    );
    assert_eq!(
        c.member("small_block_size").map(|m| m.render()),
        Some("18:7/8".into())
    );
    let b = layouts.require("block_header_t").expect("block_header_t");
    assert_eq!(b.offset("prev_phys_block"), Ok(0));
    assert_eq!(b.offset("size"), Ok(4));
    let ctx = layouts.require("lvgl_port_ctx_t").expect("lvgl_port_ctx_t");
    assert_eq!(ctx.offset("lvgl_task"), Ok(0));
    assert_eq!(ctx.offset("lvgl_mux"), Ok(4));
    let f = layouts.require("RvExcFrame").expect("RvExcFrame");
    assert_eq!(f.offset("mepc"), Ok(0));
    assert_eq!(f.offset("ra"), Ok(4));
    assert_eq!(f.offset("sp"), Ok(8));
}

#[test]
fn t1_official_panic_handler_symbolizes_with_file_and_line() {
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    let addr = elf
        .symbols
        .addr_of("esp_panic_handler")
        .expect("esp_panic_handler is in the symbol table");
    let frames = info.frames_at((addr & !1) + 4);
    let outer = frames.last().expect("a frame at esp_panic_handler+4");
    assert_eq!(outer.function.as_deref(), Some("esp_panic_handler"));
    assert!(!outer.inlined);
    let file = outer.file.as_deref().expect("a source file");
    assert!(file.ends_with("panic.c"), "{file}");
    assert!(outer.line.unwrap_or(0) > 0);
    assert!(info.build_root().is_some(), "the CU records DW_AT_comp_dir");
    assert!(info.debug_frame().len() > 100_000);
}

/// The guest stack is not available here, so the unwind runs over empty memory: the
/// "return address is not readable" warning proves the CFI row and rule were found.
#[test]
fn t1_official_debug_frame_covers_the_panic_handler() {
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    let unwinder = Unwinder::from_debug_info(&info);
    assert!(unwinder.has_cfi());
    let syms = Symbolizer::new(&elf.symbols).with_debug(&info);
    let pc = elf.symbols.addr_of("esp_panic_handler").expect("symbol") + 4;
    let regs = Registers {
        pc,
        sp: 0x3fcb_0000,
        ra: 0,
        fp: 0,
    };
    assert!(
        unwinder.covers(pc),
        "the section has a CFI row for the panic handler"
    );
    let bt = unwinder.unwind(regs, &MemoryImage::new(), &syms, MAX_FRAMES);
    let first = bt.frames.first().expect("the innermost frame");
    assert_eq!(first.function.as_deref(), Some("esp_panic_handler"));
    assert_eq!(first.origin, FrameOrigin::App);
    assert!(
        first
            .file
            .as_deref()
            .unwrap_or_default()
            .ends_with("panic.c")
    );
    assert_eq!(first.sp, 0x3fcb_0000);
    // Mid-function, the prologue has run, so the rule reads the saved return address
    // from the stack, which empty memory cannot serve.
    let mid = elf.symbols.lookup("esp_panic_handler").expect("symbol");
    let regs = Registers {
        pc: mid.addr + mid.size / 2,
        ..regs
    };
    let bt = unwinder.unwind(regs, &MemoryImage::new(), &syms, MAX_FRAMES);
    assert_eq!(bt.warnings.len(), 1, "{:?}", bt.warnings);
    assert_eq!(bt.warnings[0].what, "return address");
}

/// `s_sel` and `s_active` resolved out of the DWARF alone. The ELF symbol table is the
/// independent witness of the address; both are file statics of `main.c`, which is why a query
/// can name the unit.
#[test]
fn t1_official_menu_globals_resolve_at_their_dwarf_type() {
    use pemu_introspect::vars::{VarQuery, VarType};
    use pemu_loader::symbols::SymKind;
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    let globals = info.globals(&["s_sel", "s_active"]);
    for name in ["s_sel", "s_active"] {
        let query = VarQuery::parse(name).expect("a bare name");
        let var = globals
            .resolve(&query)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let symbols: Vec<_> = elf
            .symbols
            .named(name)
            .filter(|s| s.is_defined() && s.kind == SymKind::Object)
            .collect();
        assert_eq!(symbols.len(), 1, "one object named {name}");
        assert_eq!(
            var.addr, symbols[0].addr,
            "{name}: DWARF `DW_OP_addr` equals the symbol table's address"
        );
        assert_eq!(var.ty, VarType::Signed(4), "{name} is a plain C `int`");
        assert_eq!(var.size(), 4);
        assert_eq!(var.unit_file(), "main.c", "{name} is declared in main.c");
        assert!(!var.external, "{name} is a file static");
    }
    assert_eq!(
        globals
            .resolve(&VarQuery::parse("main.c::s_sel").expect("a qualified name"))
            .expect("the unit matches")
            .addr,
        globals
            .resolve(&VarQuery::parse("s_sel").expect("a bare name"))
            .expect("unambiguous")
            .addr,
        "naming the unit picks the same object"
    );
    println!(
        "RAN t1_official_menu_globals_resolve_at_their_dwarf_type official: s_sel {:#010x} \
         int32 main.c, s_active {:#010x} int32 main.c",
        globals
            .resolve(&VarQuery::parse("s_sel").expect("a name"))
            .expect("resolved")
            .addr,
        globals
            .resolve(&VarQuery::parse("s_active").expect("a name"))
            .expect("resolved")
            .addr,
    );
}

#[test]
fn t1_official_refuses_a_global_it_cannot_name() {
    use pemu_introspect::vars::VarQuery;
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    let globals = info.globals(&["s_sel", "no_such_global_anywhere"]);
    for name in ["no_such_global_anywhere", "not_a_unit.c::s_sel"] {
        let query = VarQuery::parse(name).expect("a well-formed query");
        let err = globals
            .resolve(&query)
            .expect_err("nothing declares it under that name");
        assert!(format!("{err}").contains(name), "{name}: {err}");
    }
}

/// The corpus ELF's DWARF says where `s_sel` is, and a synthetic [`MemoryImage`] stands in
/// for the guest's DRAM.
#[test]
fn t1_official_globals_read_out_of_a_synthetic_image() {
    use pemu_introspect::vars::{VarQuery, VarValue, read};
    let Some(bytes) = corpus("official", "elf") else {
        return;
    };
    let elf = ElfInfo::parse(&bytes).expect("official ELF parses");
    let info = DebugInfo::parse(&elf, &bytes).expect("debug info parses");
    let globals = info.globals(&["s_sel", "s_active"]);
    let sel = globals
        .resolve(&VarQuery::parse("s_sel").expect("a name"))
        .expect("resolved")
        .addr;
    let active = globals
        .resolve(&VarQuery::parse("s_active").expect("a name"))
        .expect("resolved")
        .addr;
    let mut mem = MemoryImage::new();
    mem.map_zeroed(sel & !0xFFF, 0x1000);
    mem.map_zeroed(active & !0xFFF, 0x1000);
    // The settled menu: Display selected, no page open.
    mem.put_u32(sel, 0);
    mem.put_u32(active, u32::MAX);
    let at = |mem: &MemoryImage, n: &str| {
        read(&globals, mem, &VarQuery::parse(n).expect("a name"))
            .expect("readable")
            .value
    };
    assert_eq!(at(&mem, "s_sel"), VarValue::Int(0));
    assert_eq!(at(&mem, "s_active"), VarValue::Int(-1));
    // After `click DOWN`.
    mem.put_u32(sel, 1);
    assert_eq!(at(&mem, "s_sel"), VarValue::Int(1));
}
