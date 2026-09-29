//! Tests of the Windows build flags, the manifest resource, the PE reader and the import audit,
//! over synthetic images; only the real-MSVC-link test is `cfg(windows)`.

use super::super::pe;
use super::*;

/// One section, `.rdata` at RVA 0x1000 and file offset 0x200, holding every table.
const SECTION_RVA: u32 = 0x1000;
const SECTION_FILE: usize = 0x200;

/// A PE32+ image for `machine` whose import and delay-load tables name `imports` and `delay`, and
/// which carries `manifest` as `RT_MANIFEST` / 1 / 0x0409, laid out as "PE Format" describes.
fn synthetic_pe(
    machine: u16,
    imports: &[&str],
    delay: &[&str],
    manifest: Option<&[u8]>,
) -> Vec<u8> {
    let rva = |offset: usize| SECTION_RVA + offset as u32;
    let mut section = Vec::new();
    // Import directory table: 20-byte entries and a zero one; the name RVA is at offset 12.
    let import_at = section.len();
    section.resize(import_at + 20 * (imports.len() + 1), 0);
    // Delay-load directory table: 32-byte entries and a zero one; the name RVA is at offset 4.
    let delay_at = section.len();
    section.resize(delay_at + 32 * (delay.len() + 1), 0);
    for (index, name) in imports.iter().enumerate() {
        let at = section.len();
        section.extend_from_slice(name.as_bytes());
        section.push(0);
        let field = import_at + index * 20 + 12;
        section[field..field + 4].copy_from_slice(&rva(at).to_le_bytes());
    }
    for (index, name) in delay.iter().enumerate() {
        let at = section.len();
        section.extend_from_slice(name.as_bytes());
        section.push(0);
        let field = delay_at + index * 32 + 4;
        section[field..field + 4].copy_from_slice(&rva(at).to_le_bytes());
    }
    while section.len() % 4 != 0 {
        section.push(0);
    }
    // Resource tree: type directory, name directory, language directory, data entry, data.
    let resource_at = section.len();
    let mut resource_size = 0;
    if let Some(manifest) = manifest {
        let directory = |id: u32, offset: u32| {
            let mut dir = vec![0u8; 16];
            dir[14..16].copy_from_slice(&1u16.to_le_bytes()); // one id entry
            dir.extend_from_slice(&id.to_le_bytes());
            dir.extend_from_slice(&offset.to_le_bytes());
            dir
        };
        let high = 0x8000_0000u32;
        let mut tree = Vec::new();
        tree.extend(directory(pe::RT_MANIFEST, high | 24));
        tree.extend(directory(pe::MANIFEST_ID, high | 48));
        tree.extend(directory(0x0409, 72));
        let data = resource_at + 72 + 16;
        tree.extend_from_slice(&rva(data).to_le_bytes());
        tree.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        tree.extend_from_slice(&[0u8; 8]);
        tree.extend_from_slice(manifest);
        resource_size = tree.len();
        section.extend(tree);
    }

    let mut image = vec![0u8; SECTION_FILE];
    image[..2].copy_from_slice(b"MZ");
    image[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    image[0x40..0x44].copy_from_slice(b"PE\0\0");
    let coff = 0x44;
    image[coff..coff + 2].copy_from_slice(&machine.to_le_bytes());
    image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // one section
    image[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes()); // PE32+ optional header
    let optional = coff + 20;
    image[optional..optional + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
    image[optional + 108..optional + 112].copy_from_slice(&16u32.to_le_bytes());
    let mut directory = |index: usize, rva: u32, size: usize| {
        let at = optional + 112 + index * 8;
        image[at..at + 4].copy_from_slice(&rva.to_le_bytes());
        image[at + 4..at + 8].copy_from_slice(&(size as u32).to_le_bytes());
    };
    if !imports.is_empty() {
        directory(1, rva(import_at), 20 * (imports.len() + 1));
    }
    if manifest.is_some() {
        directory(2, rva(resource_at), resource_size);
    }
    if !delay.is_empty() {
        directory(13, rva(delay_at), 32 * (delay.len() + 1));
    }
    let header = optional + 240;
    image[header..header + 6].copy_from_slice(b".rdata");
    let size = section.len() as u32;
    image[header + 8..header + 12].copy_from_slice(&size.to_le_bytes());
    image[header + 12..header + 16].copy_from_slice(&SECTION_RVA.to_le_bytes());
    image[header + 16..header + 20].copy_from_slice(&size.to_le_bytes());
    image[header + 20..header + 24].copy_from_slice(&(SECTION_FILE as u32).to_le_bytes());
    image.extend(section);
    image
}

fn good_exe() -> Vec<u8> {
    synthetic_pe(
        pe::MACHINE_AMD64,
        &["KERNEL32.dll", "ntdll.dll", "WS2_32.dll"],
        &["bcryptprimitives.dll"],
        Some(MANIFEST.as_bytes()),
    )
}

#[test]
fn the_pe_reader_reads_machine_imports_delay_imports_and_the_manifest() {
    let image = pe::parse(&good_exe()).expect("parses");
    assert_eq!(image.machine, pe::MACHINE_AMD64);
    assert_eq!(image.imports, ["KERNEL32.dll", "ntdll.dll", "WS2_32.dll"]);
    assert_eq!(image.delay_imports, ["bcryptprimitives.dll"]);
    assert_eq!(image.manifest.as_deref(), Some(MANIFEST.as_bytes()));

    let bare = pe::parse(&synthetic_pe(pe::MACHINE_ARM64, &[], &[], None)).expect("parses");
    assert_eq!(bare.machine, pe::MACHINE_ARM64);
    assert!(bare.imports.is_empty() && bare.delay_imports.is_empty());
    assert!(bare.manifest.is_none());
}

#[test]
fn the_pe_reader_refuses_what_it_cannot_follow() {
    assert!(pe::parse(b"\x7fELF").unwrap_err().contains("MZ"));
    let mut no_pe = good_exe();
    no_pe[0x40] = b'X';
    assert!(pe::parse(&no_pe).unwrap_err().contains("PE"));
    let mut truncated = good_exe();
    truncated.truncate(SECTION_FILE + 8);
    assert!(
        pe::parse(&truncated).is_err(),
        "a table past the end is an error, not `none`"
    );
}

#[test]
fn the_audit_accepts_a_static_crt_executable_with_the_manifest_and_records_its_imports() {
    let audit = audit(&good_exe(), X64_TARGET).expect("clean");
    assert_eq!(audit.imports, ["kernel32.dll", "ntdll.dll", "ws2_32.dll"]);
    assert_eq!(audit.delay_imports, ["bcryptprimitives.dll"]);
    assert_eq!(audit.manifest_sha256.len(), 64);
}

#[test]
fn the_audit_refuses_a_redistributable_import_in_either_table() {
    for (imports, delay) in [
        (&["KERNEL32.dll", "VCRUNTIME140.dll"][..], &[][..]),
        (&["KERNEL32.dll", "vcruntime140_1.dll"][..], &[][..]),
        (&["KERNEL32.dll", "MSVCP140.dll"][..], &[][..]),
        (&["KERNEL32.dll"][..], &["msvcp140_atomic_wait.dll"][..]),
    ] {
        let exe = synthetic_pe(pe::MACHINE_AMD64, imports, delay, Some(MANIFEST.as_bytes()));
        let refused = audit(&exe, X64_TARGET).expect_err("a redistributable import");
        assert!(refused.contains("redistributable"), "{refused}");
    }
    // The Universal CRT is part of Windows 10 and later, so it is not the redistributable.
    assert!(!is_redistributable("api-ms-win-crt-runtime-l1-1-0.dll"));
    assert!(!is_redistributable("ucrtbase.dll"));
    assert!(is_redistributable("VCRUNTIME140.DLL"));
}

#[test]
fn the_audit_refuses_a_missing_or_foreign_manifest_and_a_wrong_machine() {
    let none = synthetic_pe(pe::MACHINE_AMD64, &["KERNEL32.dll"], &[], None);
    assert!(
        audit(&none, X64_TARGET)
            .unwrap_err()
            .contains("RT_MANIFEST")
    );

    let other = MANIFEST.replace("UTF-8</activeCodePage>", "Legacy</activeCodePage>");
    let foreign = synthetic_pe(
        pe::MACHINE_AMD64,
        &["KERNEL32.dll"],
        &[],
        Some(other.as_bytes()),
    );
    assert!(
        audit(&foreign, X64_TARGET)
            .unwrap_err()
            .contains("not the one")
    );

    assert!(
        audit(&good_exe(), ARM64_TARGET)
            .unwrap_err()
            .contains("machine")
    );
    let no_imports = synthetic_pe(pe::MACHINE_AMD64, &[], &[], Some(MANIFEST.as_bytes()));
    assert!(
        audit(&no_imports, X64_TARGET)
            .unwrap_err()
            .contains("no DLL")
    );
}

#[test]
fn the_manifest_declares_long_paths_utf8_and_the_windows_10_11_id() {
    assert!(MANIFEST.contains(
        "<longPathAware xmlns=\"http://schemas.microsoft.com/SMI/2016/WindowsSettings\">true\
         </longPathAware>"
    ));
    assert!(MANIFEST.contains(
        "<activeCodePage xmlns=\"http://schemas.microsoft.com/SMI/2019/WindowsSettings\">UTF-8\
         </activeCodePage>"
    ));
    assert_eq!(
        MANIFEST.matches("<supportedOS ").count(),
        1,
        "the single Windows 10/11 id"
    );
    assert!(MANIFEST.contains(&format!("Id=\"{SUPPORTED_OS_WINDOWS_10_11}\"")));
    assert!(MANIFEST.contains("level=\"asInvoker\""));
    assert!(MANIFEST.is_ascii());
}

#[test]
fn the_resource_file_is_the_empty_header_then_rt_manifest_one() {
    let res = res_file(b"<x/>");
    assert_eq!(res.len(), 32 + 32 + 4);
    let mut empty = [0u8; 32];
    empty[4] = 32; // HeaderSize
    empty[8..10].copy_from_slice(&[0xFF, 0xFF]);
    empty[12..14].copy_from_slice(&[0xFF, 0xFF]);
    assert_eq!(&res[..32], &empty);
    let header = &res[32..64];
    assert_eq!(&header[..4], &4u32.to_le_bytes(), "DataSize");
    assert_eq!(&header[4..8], &32u32.to_le_bytes(), "HeaderSize");
    assert_eq!(&header[8..12], &[0xFF, 0xFF, 24, 0], "type RT_MANIFEST");
    assert_eq!(&header[12..16], &[0xFF, 0xFF, 1, 0], "name 1");
    assert_eq!(&header[22..24], &0x0409u16.to_le_bytes(), "LanguageId");
    assert_eq!(&res[64..], b"<x/>");
    assert_eq!(res_file(b"<x/>!").len() % 4, 0, "padded to a DWORD");
    // A changed manifest is a changed file name, so cargo relinks.
    assert_ne!(res_file_name(&res), res_file_name(&res_file(b"<y/>")));
}

#[test]
fn the_windows_build_sets_crt_static_for_its_target_only_and_links_the_manifest() {
    let args = cargo_args(
        X64_TARGET,
        "C:/t/m.res",
        &["--remap-path-prefix=C:\\w=/pemu".to_string()],
    )
    .expect("args");
    let joined = args.join(" ");
    assert!(joined.starts_with("rustc --release --target x86_64-pc-windows-msvc -p pemu-cli"));
    assert!(joined.contains("--bin passportsim"));
    let config = args
        .iter()
        .position(|a| a == "--config")
        .map(|at| args[at + 1].as_str())
        .expect("--config");
    assert_eq!(
        config,
        "target.x86_64-pc-windows-msvc.rustflags=['-C', 'target-feature=+crt-static', \
         '--remap-path-prefix=C:\\w=/pemu']"
    );
    let extra = &args[args.iter().position(|a| a == "--").expect("--") + 1..];
    assert_eq!(
        extra,
        ["-C", "link-arg=/MANIFEST:NO", "-C", "link-arg=C:/t/m.res"]
    );
    assert!(
        !joined.contains("build.rustflags"),
        "never the global rustflags"
    );
}

/// The audit is not vacuous on a real MSVC link: this test binary is built without `+crt-static`,
/// so it imports the Visual C++ runtime, and the audit refuses it.
#[cfg(windows)]
#[test]
fn the_audit_refuses_this_dynamically_linked_test_binary() {
    let exe = std::env::current_exe().expect("the test binary");
    let bytes = std::fs::read(&exe).expect("readable");
    let image = pe::parse(&bytes).expect("a real PE image parses");
    assert_eq!(image.machine, pe::MACHINE_AMD64);
    assert!(
        image
            .imports
            .iter()
            .any(|dll| dll.eq_ignore_ascii_case("kernel32.dll")),
        "{:?}",
        image.imports
    );
    let refused = audit(&bytes, X64_TARGET).expect_err("a dynamic CRT build");
    assert!(
        refused.to_ascii_lowercase().contains("vcruntime"),
        "{refused}"
    );
}
