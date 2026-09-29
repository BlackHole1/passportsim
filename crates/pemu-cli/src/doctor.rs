//! The `report` argument of `passportsim doctor`.
//!
//! `pemu-api` reads no file and no environment variable, so `doctor` takes what this host has as an
//! argument, and this binary produces it: `pemu_host::assets::discovery_report` for the ROM pins,
//! any override, a local esp-rom-elfs copy and the corpus map, plus the demo image this binary's
//! own payload carries.
//!
//! The demo is read out of the payload, not searched for: a packaged binary embeds
//! `payload/firmware/official.pebundle` and a plain `cargo build` embeds nothing. The digest
//! reported is the merged flash image inside the bundle, which is what `corpus.toml` pins as
//! `official`'s `bin` and what `xtask package` records as `demo.bin_sha256`.

use pemu_api::commands::doctor::EmbeddedDemo;
use pemu_host::assets::{HostEnv, RomOptions};
use pemu_loader::bundle::{BUNDLE_FLASH, Bundle};
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::rom::RomRev;

use crate::json::Value;
use crate::payload::{FIRMWARE_DIR, Payload};

/// The name `xtask package` writes it under and a `start` with no `fw` asks for.
pub use pemu_api::commands::start::DEMO_FW as DEMO_ID;

/// `verify` hashes every corpus file present; a run that boots one image does not, because
/// `Corpus::read` checks the bytes it reads.
pub fn report_argument(
    paths: &pemu_host::paths::HostPaths,
    payload: &Result<Payload, String>,
) -> Value {
    let env = HostEnv::from_paths(paths);
    // The ROM follows the eFuse chip revision, and the default eFuse is synthesized: no dump is
    // read to answer a read-only question.
    let rev = RomRev::for_efuse(&EfuseImage::synth(0)).unwrap_or(RomRev::Rev101);
    let discovered = pemu_host::assets::discovery_report(&env, &RomOptions::default(), rev, true);
    let mut report = discovered.to_doctor_report();
    report.demo = embedded_demo(payload);
    report.host = Some(pemu_host::assets::host_facts(paths));
    // A policy that writes crash dumps regardless is reported, never a failure.
    report
        .warnings
        .extend(pemu_host::assets::local_dumps_warning());
    report.to_input_json()
}

/// `Bundle::parse` verifies every payload digest, so a damaged embedded demo is reported as absent
/// rather than with a digest nobody checked.
fn embedded_demo(payload: &Result<Payload, String>) -> Option<EmbeddedDemo> {
    let bytes = payload
        .as_ref()
        .ok()?
        .read(&format!("{FIRMWARE_DIR}/{DEMO_ID}.pebundle"))?;
    let bundle = Bundle::parse(&bytes).ok()?;
    let flash = bundle.file(BUNDLE_FLASH)?;
    Some(EmbeddedDemo {
        id: bundle.id().unwrap_or(DEMO_ID).to_owned(),
        sha256: pemu_loader::hex(&flash.sha256),
        bytes: flash.len as u64,
    })
}

/// `passportsim doctor` takes no arguments of its own. A call that already carries a `report` (the
/// command's own example, a test) is left alone.
pub fn fill_report(
    call: &mut Value,
    paths: &pemu_host::paths::HostPaths,
    payload: &Result<Payload, String>,
) {
    let Some(object) = call.as_object_mut() else {
        return;
    };
    if object.contains_key("report") {
        return;
    }
    object.insert("report".to_owned(), report_argument(paths, payload));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{EmbeddedFile, Source};
    use pemu_loader::bundle::{BundleInput, build as build_bundle};

    /// As `xtask package` writes the demo.
    fn demo_bundle(flash: &[u8]) -> Vec<u8> {
        build_bundle(
            Some(DEMO_ID),
            Some("official demo"),
            &[BundleInput {
                role: BUNDLE_FLASH,
                name: "FoloToy-AI-Passport-8MB.bin",
                bytes: flash,
            }],
        )
    }

    fn payload_of(path: &str, bytes: Vec<u8>) -> Result<Payload, String> {
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let file = EmbeddedFile {
            path: Box::leak(path.to_owned().into_boxed_str()),
            offset: 0,
            len: leaked.len(),
            sha256: Box::leak(pemu_loader::hex(&pemu_loader::sha256(leaked)).into_boxed_str()),
        };
        Ok(Payload {
            source: Source::Embedded(crate::payload::Embedded::from_parts(
                "0".repeat(64).leak(),
                leaked,
                Box::leak(vec![file].into_boxed_slice()),
            )),
            digest: "0".repeat(64),
        })
    }

    #[test]
    fn a_build_with_no_payload_reports_no_demo() {
        assert_eq!(embedded_demo(&Err("no payload".to_owned())), None);
    }

    /// By the digest of the merged image inside the bundle, the one `corpus.toml` pins for
    /// `official`.
    #[test]
    fn a_packaged_build_reports_the_demo_image_of_its_own_payload() {
        let flash = vec![0xA5u8; 4096];
        let payload = payload_of(
            &format!("{FIRMWARE_DIR}/{DEMO_ID}.pebundle"),
            demo_bundle(&flash),
        );
        let demo = embedded_demo(&payload).expect("the payload carries the demo");
        assert_eq!(demo.id, DEMO_ID);
        assert_eq!(demo.bytes, 4096);
        assert_eq!(demo.sha256, pemu_loader::hex(&pemu_loader::sha256(&flash)));
    }

    #[test]
    fn a_damaged_bundle_is_no_demo() {
        let mut bundle = demo_bundle(&[7u8; 64]);
        let last = bundle.len() - 1;
        bundle[last] ^= 0xFF;
        let payload = payload_of(&format!("{FIRMWARE_DIR}/{DEMO_ID}.pebundle"), bundle);
        assert_eq!(embedded_demo(&payload), None);
    }

    #[test]
    fn a_call_that_carries_a_report_is_left_alone() {
        let mut call = serde_json::json!({ "report": { "warnings": ["mine"] } });
        let paths = crate::paths::resolver(None);
        fill_report(&mut call, &paths, &Err("no payload".to_owned()));
        assert_eq!(call["report"]["warnings"][0], "mine");
    }
}
