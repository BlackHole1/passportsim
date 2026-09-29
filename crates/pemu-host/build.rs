//! Computes the emulator build id of the boot-cache key. `CARGO_PKG_VERSION` does not move between
//! commits, so the key needs a content hash; `src/build_id.rs` defines what it covers.

#[path = "src/build_id.rs"]
mod build_id;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // `doctor` reports the target triple as Cargo names it, not one rebuilt from `cfg` values.
    println!(
        "cargo:rustc-env=PEMU_TARGET_TRIPLE={}",
        std::env::var("TARGET").expect("cargo sets TARGET")
    );
    let manifest =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let workspace = manifest
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/pemu-host sits two levels below the workspace");
    for root in build_id::ROOTS {
        println!("cargo:rerun-if-changed={}", workspace.join(root).display());
    }
    let id = build_id::build_id(workspace, pemu_loader::sha256);
    println!("cargo:rustc-env=PEMU_BUILD_ID={id}");
}
