//! The WAV artifact a capturing suite writes (F6).

use std::path::Path;

use serde_json::{Value, json};

/// Directory of the F-suite artifacts under the data root.
const BENCH_ARTIFACTS: &str = "artifacts/bench";

/// The WAV a capturing suite wrote, referenced by path and hash.
pub(super) struct WavArtifact {
    /// Path relative to the data root, forward-slashed. An absolute path would name this host,
    /// which a recorded artifact reference must not.
    pub(super) path: String,
    pub(super) sha256: String,
    pub(super) fs: u32,
    pub(super) channels: u16,
    pub(super) frames: usize,
    bytes: usize,
}

impl WavArtifact {
    pub(super) fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "relative_to": "data root",
            "sha256": self.sha256,
            "media_type": pemu_api::commands::audio_capture::WAV_MEDIA_TYPE,
            "fs": self.fs,
            "channels": self.channels,
            "frames": self.frames,
            "bytes": self.bytes,
        })
    }
}

/// Writes the guest's playback as a WAV under the data root and returns its path and hash.
///
/// The bytes are `pemu_api::commands::audio_capture::wav_bytes`, the encoder `audio_capture`
/// itself uses, so the artifact a bench writes and the artifact the command writes are the same
/// file for the same samples. It goes under the data root and never into the tree: a capture of a
/// guest run is a measurement artifact.
pub(super) fn write_wav(
    root: &Path,
    workload: &str,
    fs: u32,
    channels: u16,
    samples: &[i16],
) -> Result<WavArtifact, String> {
    let bytes = pemu_api::commands::audio_capture::wav_bytes(fs, channels, samples);
    let relative = format!("{BENCH_ARTIFACTS}/{workload}.wav");
    let file = root.join(&relative);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&file, &bytes).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(WavArtifact {
        path: relative,
        sha256: pemu_testkit::corpus::sha256_hex(&bytes),
        fs,
        channels,
        frames: if channels == 0 {
            0
        } else {
            samples.len() / usize::from(channels)
        },
        bytes: bytes.len(),
    })
}
