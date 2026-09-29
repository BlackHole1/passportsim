//! What the Windows package adds to the build and checks in its result.
//!
//! [`cargo_args`] sets the Windows-only flags: `+crt-static` as `target.<triple>.rustflags`, so it
//! reaches every crate of the target and nothing built for the host, and the manifest as a
//! compiled `.res` on the final link (`/MANIFEST:NO` keeps the linker from adding its own), which
//! needs neither `mt.exe` nor libxml2.
//!
//! [`MANIFEST`] declares `longPathAware`, `activeCodePage` UTF-8 (Windows 10 1903 and later), the
//! Windows 10/11 `supportedOS` id, and `asInvoker`, without which Windows applies installer
//! detection once the linker's UAC fragment is gone (Microsoft Learn, "Application manifests").
//!
//! [`audit`] fails on a wrong machine, a `vcruntime*.dll` or `msvcp*.dll` import, or a manifest
//! other than [`MANIFEST`].
//!
//! UNVERIFIED: that the loader honors each setting. An unparsable manifest stops the process, so
//! every run shows the XML is accepted, but `longPathAware` also needs the `LongPathsEnabled`
//! policy, and the effective `GetACP()` is not observable from outside the process.

use sha2::{Digest, Sha256};

use super::pe;

pub const X64_TARGET: &str = "x86_64-pc-windows-msvc";
/// The Arm64 Windows target, which no host of this project can build and run.
pub const ARM64_TARGET: &str = "aarch64-pc-windows-msvc";

/// The single `supportedOS` id of Windows 10 and Windows 11.
pub const SUPPORTED_OS_WINDOWS_10_11: &str = "{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}";

pub const MANIFEST: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    "<assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\" manifestVersion=\"1.0\">\n",
    "  <compatibility xmlns=\"urn:schemas-microsoft-com:compatibility.v1\">\n",
    "    <application>\n",
    "      <supportedOS Id=\"{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}\"/>\n",
    "    </application>\n",
    "  </compatibility>\n",
    "  <application xmlns=\"urn:schemas-microsoft-com:asm.v3\">\n",
    "    <windowsSettings>\n",
    "      <longPathAware xmlns=\"http://schemas.microsoft.com/SMI/2016/WindowsSettings\">true</longPathAware>\n",
    "      <activeCodePage xmlns=\"http://schemas.microsoft.com/SMI/2019/WindowsSettings\">UTF-8</activeCodePage>\n",
    "    </windowsSettings>\n",
    "  </application>\n",
    "  <trustInfo xmlns=\"urn:schemas-microsoft-com:asm.v3\">\n",
    "    <security>\n",
    "      <requestedPrivileges>\n",
    "        <requestedExecutionLevel level=\"asInvoker\" uiAccess=\"false\"/>\n",
    "      </requestedPrivileges>\n",
    "    </security>\n",
    "  </trustInfo>\n",
    "</assembly>\n",
);

/// `LANG_ENGLISH`, `SUBLANG_ENGLISH_US`: the language the MSVC tools give an embedded manifest.
const MANIFEST_LANGUAGE: u16 = 0x0409;
/// `MOVEABLE | PURE`, the memory flags `rc.exe` gives a resource ("RESOURCEHEADER structure").
const MEMORY_FLAGS: u16 = 0x0030;

/// A `.res` file holding `manifest` as resource `RT_MANIFEST` / 1 ("RESOURCEHEADER structure").
pub fn res_file(manifest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // The empty resource that opens every 32-bit `.res` file: DataSize 0, HeaderSize 32, type and
    // name both ordinal 0, every other field 0.
    resource_header(&mut out, 0, 0, 0, 0, 0);
    resource_header(
        &mut out,
        manifest.len(),
        pe::RT_MANIFEST as u16,
        pe::MANIFEST_ID as u16,
        MEMORY_FLAGS,
        MANIFEST_LANGUAGE,
    );
    out.extend_from_slice(manifest);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out
}

/// One `RESOURCEHEADER` whose type and name are ordinals: `0xFFFF` then the number.
fn resource_header(
    out: &mut Vec<u8>,
    data_size: usize,
    kind: u16,
    name: u16,
    flags: u16,
    language: u16,
) {
    let data_size = u32::try_from(data_size).expect("a manifest is far below 4 GiB");
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    for ordinal in [kind, name] {
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&ordinal.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    out.extend_from_slice(&language.to_le_bytes()); // LanguageId
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
}

/// The file name of the resource file: its digest, so a changed manifest relinks.
pub fn res_file_name(res: &[u8]) -> String {
    let digest: String = Sha256::digest(res)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("passportsim-manifest-{digest}.res")
}

/// The `cargo` arguments that build the release `passportsim.exe` for `target`, linking `res`.
pub fn cargo_args(target: &str, res: &str, rustflags: &[String]) -> Result<Vec<String>, String> {
    let mut flags = vec!["-C".to_string(), "target-feature=+crt-static".to_string()];
    flags.extend_from_slice(rustflags);
    let config = super::account::rustflags_config(target, &flags)?;
    Ok([
        "rustc",
        "--release",
        "--target",
        target,
        "-p",
        "pemu-cli",
        "--bin",
        "passportsim",
        "--config",
        &config,
        "--",
        "-C",
        "link-arg=/MANIFEST:NO",
        "-C",
        &format!("link-arg={res}"),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audit {
    /// The DLLs the import directory names, sorted, lowercase.
    pub imports: Vec<String>,
    /// The DLLs the delay-load directory names, sorted, lowercase.
    pub delay_imports: Vec<String>,
    /// SHA-256 of the embedded manifest, which equals [`MANIFEST`]'s.
    pub manifest_sha256: String,
}

fn machine(target: &str) -> Result<u16, String> {
    match target {
        X64_TARGET => Ok(pe::MACHINE_AMD64),
        ARM64_TARGET => Ok(pe::MACHINE_ARM64),
        other => Err(format!("`{other}` is not a Windows target")),
    }
}

/// Whether `dll` is a Visual C++ redistributable library: `vcruntime*.dll` or `msvcp*.dll`, in any
/// case, which is how the loader compares module names.
pub fn is_redistributable(dll: &str) -> bool {
    let dll = dll.to_ascii_lowercase();
    dll.ends_with(".dll") && (dll.starts_with("vcruntime") || dll.starts_with("msvcp"))
}

pub fn audit(bytes: &[u8], target: &str) -> Result<Audit, String> {
    let image = pe::parse(bytes).map_err(|e| format!("the Windows import audit: {e}"))?;
    let expected = machine(target)?;
    if image.machine != expected {
        return Err(format!(
            "the executable's COFF machine is 0x{:04x}, not 0x{expected:04x} of `{target}`",
            image.machine
        ));
    }
    let lower = |names: &[String]| {
        let mut names: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        names.sort();
        names.dedup();
        names
    };
    let imports = lower(&image.imports);
    let delay_imports = lower(&image.delay_imports);
    let redistributable: Vec<&String> = imports
        .iter()
        .chain(&delay_imports)
        .filter(|dll| is_redistributable(dll))
        .collect();
    if !redistributable.is_empty() {
        return Err(format!(
            "the executable imports {redistributable:?}, so it needs the Visual C++ \
             redistributable: `+crt-static` did not reach the link. Is \
             `RUSTFLAGS` or a `build.rustflags` of this host overriding the target's?"
        ));
    }
    if imports.is_empty() {
        return Err(
            "the executable imports no DLL at all, which no Rust program linked for Windows does; \
             the import table was not read"
                .into(),
        );
    }
    let manifest = image.manifest.ok_or(
        "the executable carries no RT_MANIFEST resource 1: the manifest was not linked in",
    )?;
    if manifest != MANIFEST.as_bytes() {
        return Err(format!(
            "the executable's manifest ({} bytes) is not the one `xtask package` links \
             ({} bytes): {}",
            manifest.len(),
            MANIFEST.len(),
            String::from_utf8_lossy(&manifest)
        ));
    }
    Ok(Audit {
        imports,
        delay_imports,
        manifest_sha256: Sha256::digest(&manifest)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    })
}

#[cfg(test)]
mod tests;
