//! Pure flash-plan builder and refusal rules; native execution only with feature `device`.

/// The device boot console: the allow-listed line shapes and the redaction.
pub mod console;
pub mod flow;
/// MD5 (RFC 1321). Public because the rehearsal target computes the region digest over its own
/// emulated flash.
pub mod md5;
pub mod plan;
pub mod rehearse;
pub mod rules;
pub mod stub_md5;

// Feature `device` builds on macOS and Windows only; other hosts compile the pure planner.
#[cfg(all(feature = "device", not(any(target_os = "macos", windows))))]
compile_error!(
    "feature `device` builds on macOS and Windows only; build \
     without it and use the pure planner, which returns E_HOST_UNSUPPORTED for a real-device flash"
);

// `cargo xtask layering` exempts the `device` feature from the core-crate API rules, but
// clippy.toml cannot be scoped to a feature, so the module allows its two lints here.
#[cfg(feature = "device")]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub mod exec;
