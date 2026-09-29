//! The confinement policy of the `file` microphone source, and the host half of the audio
//! commands' I/O seams (`mic_set::set_io`, `audio_capture::set_io`).
//!
//! `mic_set {kind: file, name}` names a file relative to `<data root>/audio/`, which no command
//! can move. [`AudioRoot::bytes`] rechecks the name shape, requires the canonical file to stay
//! inside the canonical root (so an outward symlink is refused), and reads it through
//! [`crate::host_file::read_regular`]. Errors never carry a host path.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pemu_api::commands::audio_capture::{self, AudioIo};
use pemu_api::commands::mic_set::{self, MicFile, MicFileError, MicIo};

use crate::host_file;
use crate::paths::{HostPaths, PathError};

/// The audio root below the data root.
pub const AUDIO_DIR: &str = "audio";

/// Largest audio file the reader loads: over half an hour of 16 kHz stereo, and a bound that
/// keeps a mistaken name from pulling a disk image into memory.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRoot {
    dir: PathBuf,
}

impl AudioRoot {
    /// A root at `dir`, which need not exist yet: a read while it is absent is `Unreadable`.
    pub fn new(dir: impl Into<PathBuf>) -> AudioRoot {
        AudioRoot { dir: dir.into() }
    }

    pub fn from_paths(paths: &HostPaths) -> Result<AudioRoot, PathError> {
        Ok(AudioRoot::new(paths.data_root()?.join(AUDIO_DIR)))
    }

    /// The directory, for the host's own log. Never put it in an output.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn bytes(&self, name: &str) -> Result<Vec<u8>, MicFileError> {
        mic_set::check_file_name(name).map_err(|_| MicFileError::Unreadable)?;
        let root = host_file::canonical(&self.dir).map_err(|_| MicFileError::Unreadable)?;
        let path = host_file::canonical(&root.join(name)).map_err(|_| MicFileError::Unreadable)?;
        if !crate::paths::contains(&root, &path).unwrap_or(false) || path == root {
            return Err(MicFileError::Unreadable);
        }
        host_file::read_regular(&path, MAX_FILE_BYTES).map_err(|_| MicFileError::Unreadable)
    }

    pub fn read(&self, name: &str) -> Result<MicFile, MicFileError> {
        let bytes = self.bytes(name)?;
        let (spec, samples) = crate::wav::decode(&bytes).map_err(|e| MicFileError::Invalid(e.0))?;
        Ok(MicFile {
            fs: spec.sample_rate,
            channels: spec.channels,
            samples,
        })
    }
}

fn installed() -> &'static Mutex<Option<AudioRoot>> {
    static ROOT: OnceLock<Mutex<Option<AudioRoot>>> = OnceLock::new();
    ROOT.get_or_init(|| Mutex::new(None))
}

fn read_installed(name: &str) -> Result<MicFile, MicFileError> {
    let root = installed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or(MicFileError::Unreadable)?;
    root.read(name)
}

/// Installs the audio I/O of this process: `mic_set` reads under `root`, and `audio_capture` writes
/// into the running instance's artifact directory. A second call replaces the root.
pub fn install(root: AudioRoot) {
    *installed().lock().unwrap_or_else(|e| e.into_inner()) = Some(root);
    mic_set::set_io(MicIo {
        read: read_installed,
    });
    audio_capture::set_io(AudioIo {
        write: crate::artifacts::write_current,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-audio-root-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        dir
    }

    fn wav() -> Vec<u8> {
        crate::wav::encode(&crate::wav::WavSpec::mono(16_000), &[1, -2, 3])
    }

    fn layout(name: &str) -> (PathBuf, AudioRoot) {
        let base = temp(name);
        let root = base.join("audio");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("ok.wav"), wav()).expect("write");
        std::fs::write(root.join("sub/ok.wav"), wav()).expect("write");
        std::fs::write(base.join("secret.wav"), wav()).expect("write");
        (base, AudioRoot::new(root))
    }

    #[test]
    fn a_file_under_the_root_is_read_and_decoded() {
        let (_, root) = layout("ok");
        for name in ["ok.wav", "sub/ok.wav"] {
            let file = root.read(name).expect(name);
            assert_eq!((file.fs, file.channels), (16_000, 1));
            assert_eq!(file.samples, [1, -2, 3]);
        }
    }

    #[test]
    fn a_parent_segment_or_an_absolute_name_is_refused_even_when_the_target_exists() {
        let (base, root) = layout("dotdot");
        let absolute = base.join("secret.wav").to_string_lossy().into_owned();
        for name in [
            "../secret.wav",
            "sub/../../secret.wav",
            "./ok.wav",
            &absolute,
        ] {
            assert_eq!(root.read(name), Err(MicFileError::Unreadable), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_escapes_the_root_is_refused_and_one_that_stays_inside_is_read() {
        let (base, root) = layout("symlink");
        std::os::unix::fs::symlink(base.join("secret.wav"), root.dir().join("escape.wav"))
            .expect("symlink");
        std::os::unix::fs::symlink(base.clone(), root.dir().join("outside")).expect("dir symlink");
        std::os::unix::fs::symlink(root.dir().join("ok.wav"), root.dir().join("inside.wav"))
            .expect("symlink");
        assert_eq!(root.read("escape.wav"), Err(MicFileError::Unreadable));
        assert_eq!(
            root.read("outside/secret.wav"),
            Err(MicFileError::Unreadable)
        );
        assert_eq!(root.read("inside.wav").expect("inside").samples, [1, -2, 3]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_directory_or_a_fifo_is_not_a_regular_file() {
        let (_, root) = layout("nonregular");
        assert_eq!(root.read("sub"), Err(MicFileError::Unreadable));
        let fifo = root.dir().join("pipe.wav");
        crate::platform::macos::make_fifo(&fifo).expect("mkfifo");
        // Opening a FIFO would block until a writer appears, so it must be refused on metadata.
        assert_eq!(root.read("pipe.wav"), Err(MicFileError::Unreadable));
    }

    #[test]
    fn a_missing_root_or_file_and_a_bad_wav_name_no_host_path() {
        let (base, root) = layout("errors");
        assert_eq!(root.read("absent.wav"), Err(MicFileError::Unreadable));
        assert_eq!(
            AudioRoot::new(base.join("nowhere")).read("ok.wav"),
            Err(MicFileError::Unreadable)
        );
        std::fs::write(root.dir().join("bad.wav"), b"not a wav").expect("write");
        match root.read("bad.wav") {
            Err(MicFileError::Invalid(text)) => {
                assert!(!text.contains(&*base.to_string_lossy()), "{text}");
                assert!(text.contains("RIFF"), "{text}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
