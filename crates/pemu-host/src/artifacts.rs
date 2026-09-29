//! The per-instance artifact directory.
//!
//! Every path an agent sees is relative and forward-slashed, so goldens, receipts and committed
//! examples are identical on macOS and Windows and carry no user name; [`ArtifactDir::write`] is
//! the only way to create an artifact and returns that relative path. A path is refused if it is
//! absolute, names a drive, or holds `..` or a backslash, and then checked again against the
//! canonical root (case-insensitive, as APFS and NTFS are), because a symlink inside the root is
//! where the name rule and the file system disagree.
//!
//! A daemon instance's pool slot holds its directory; the instance thread appends `serial.log` and
//! `events.ndjson`, and every shutdown path joins the thread and calls [`ArtifactDir::finish`].
//! `pemu-api` cannot name a directory, so the instance thread binds its own for the length of one
//! command ([`bind_current`]) and [`write_current`] is the writer its seams get.

use std::cell::RefCell;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use pemu_api::output::ArtifactRef;

pub const SCREENS_DIR: &str = "screens";
pub const UI_DIR: &str = "ui";
pub const AUDIO_DIR: &str = "audio";
pub const SNAPSHOTS_DIR: &str = "snapshots";
pub const EVENTS_FILE: &str = "events.ndjson";
pub const SERIAL_FILE: &str = "serial.log";
pub const SUMMARY_FILE: &str = "summary.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathRefusal {
    Empty,
    /// The path is absolute, or names a drive or a UNC share.
    Absolute,
    Parent,
    /// The path holds a backslash, a separator on Windows and an ordinary character on macOS.
    Backslash,
    /// A component is not `[a-z0-9][a-z0-9._-]*`.
    Name(String),
}

impl fmt::Display for PathRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathRefusal::Empty => f.write_str("an artifact path cannot be empty"),
            PathRefusal::Absolute => {
                f.write_str("an artifact path is relative to the instance artifact root")
            }
            PathRefusal::Parent => f.write_str("an artifact path cannot hold `..`"),
            PathRefusal::Backslash => f.write_str(
                "an artifact path separates with `/` on both hosts; a backslash is a file-name \
                 character on macOS and a separator on Windows",
            ),
            PathRefusal::Name(part) => write!(
                f,
                "`{part}` is not an artifact name: names are `[a-z0-9][a-z0-9._-]*`"
            ),
        }
    }
}

impl std::error::Error for PathRefusal {}

pub fn name_is_valid(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn check_relative(path: &str) -> Result<(), PathRefusal> {
    if path.is_empty() {
        return Err(PathRefusal::Empty);
    }
    if path.contains('\\') {
        return Err(PathRefusal::Backslash);
    }
    if path.starts_with('/') || path.chars().nth(1) == Some(':') {
        return Err(PathRefusal::Absolute);
    }
    for part in path.split('/') {
        if part == ".." {
            return Err(PathRefusal::Parent);
        }
        if !name_is_valid(part) {
            return Err(PathRefusal::Name(part.to_string()));
        }
    }
    Ok(())
}

/// A native path as a forward-slash relative path. `None` when it is not relative or not UTF-8: a
/// lossy name would not open, so it is refused rather than converted.
pub fn to_forward_slashes(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// One instance's artifact directory: `<artifacts>/<run-id>/<instance>/`.
#[derive(Debug)]
pub struct ArtifactDir {
    root: PathBuf,
    open: Vec<(String, File)>,
}

impl ArtifactDir {
    /// Creates `<artifacts>/<run_id>/<instance>/` and the subdirectories every run uses. Not
    /// owner-only: artifacts are what a person opens and attaches to a report. Private files live
    /// elsewhere, and a tainted machine's captures never reach disk.
    pub fn create(artifacts: &Path, run_id: &str, instance: &str) -> Result<ArtifactDir, Error> {
        for part in [run_id, instance] {
            if !name_is_valid(part) {
                return Err(Error::Path(PathRefusal::Name(part.to_string())));
            }
        }
        let root = artifacts.join(run_id).join(instance);
        std::fs::create_dir_all(&root).map_err(|e| Error::Io(root.clone(), e))?;
        for sub in [SCREENS_DIR, UI_DIR, AUDIO_DIR, SNAPSHOTS_DIR] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).map_err(|e| Error::Io(dir, e))?;
        }
        Ok(ArtifactDir {
            root,
            open: Vec::new(),
        })
    }

    /// The native root. Only `status` reports it, and only home-redacted.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn native(&self, relative: &str) -> Result<PathBuf, Error> {
        check_relative(relative).map_err(Error::Path)?;
        let native = self.root.join(relative);
        // The name rule is about the string; this is about the file system, where a symlink
        // planted inside the root makes them disagree.
        if let Some(parent) = native.parent()
            && parent.exists()
            && !crate::paths::contains(&self.root, parent).map_err(Error::Guard)?
        {
            return Err(Error::Escapes(relative.to_string()));
        }
        // The check above canonicalizes only the parent, so a symlink at the leaf (a valid name
        // like `screens/0001-boot.png`) would be followed by the write. Every file here is one
        // this directory created and none is a link, so a symlink leaf is refused.
        if native
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(Error::Symlink(relative.to_string()));
        }
        Ok(native)
    }

    /// Writes one artifact and returns the reference an `Output` carries. `relative` is what the
    /// agent sees, so no artifact can be reported by an absolute path.
    pub fn write(
        &self,
        relative: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRef, Error> {
        let native = self.native(relative)?;
        if let Some(parent) = native.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.to_path_buf(), e))?;
        }
        std::fs::write(&native, bytes).map_err(|e| Error::Io(native, e))?;
        // The hash comes from the bytes written, never a cached digest.
        let sha256 = pemu_loader::hex(&pemu_loader::sha256(bytes));
        ArtifactRef::new(
            relative,
            sha256,
            media_type,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        )
        .map_err(Error::Reference)
    }

    /// Writes a screenshot from an RGB565 frame (`screens/<seq>-<label>.png`).
    pub fn screenshot(
        &self,
        seq: u32,
        label: &str,
        width: u32,
        height: u32,
        pixels: &[u16],
    ) -> Result<ArtifactRef, Error> {
        let png = crate::png::encode_rgb565(width, height, pixels, 1).map_err(Error::Png)?;
        self.write(
            &format!("{SCREENS_DIR}/{seq:04}-{label}.png"),
            "image/png",
            &png,
        )
    }

    /// Writes a capture from PCM samples (`audio/tx-<seq>.wav`, `audio/rx-<seq>.wav`).
    pub fn capture(
        &self,
        direction: &str,
        seq: u32,
        spec: &crate::wav::WavSpec,
        samples: &[i16],
    ) -> Result<ArtifactRef, Error> {
        let wav = crate::wav::encode(spec, samples);
        self.write(
            &format!("{AUDIO_DIR}/{direction}-{seq:04}.wav"),
            "audio/wav",
            &wav,
        )
    }

    /// Appends to a streamed artifact. The handle stays open across calls, which is why
    /// [`ArtifactDir::flush`] exists.
    pub fn append(&mut self, relative: &str, bytes: &[u8]) -> Result<(), Error> {
        let native = self.native(relative)?;
        if !self.open.iter().any(|(name, _)| name == relative) {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&native)
                .map_err(|e| Error::Io(native, e))?;
            self.open.push((relative.to_string(), file));
        }
        let file = self
            .open
            .iter_mut()
            .find(|(name, _)| name == relative)
            .map(|(_, file)| file)
            .expect("the handle was just opened");
        file.write_all(bytes)
            .map_err(|e| Error::Io(self.root.join(relative), e))
    }

    /// Flushes every open artifact. Idempotent, reporting the first failure, so every shutdown
    /// path and `Drop` may call it.
    pub fn flush(&mut self) -> Result<(), Error> {
        let mut first: Result<(), Error> = Ok(());
        for (name, file) in &mut self.open {
            if let Err(e) = file.flush()
                && first.is_ok()
            {
                first = Err(Error::Io(self.root.join(name), e));
            }
        }
        first
    }

    pub fn open_paths(&self) -> Vec<&str> {
        self.open.iter().map(|(name, _)| name.as_str()).collect()
    }
}

pub type SharedDir = Arc<Mutex<ArtifactDir>>;

impl ArtifactDir {
    /// Writes `summary.json` and flushes: the last write of an instance, on every shutdown path.
    /// The flush runs even if the summary fails, and the first failure is reported.
    pub fn finish(&mut self, summary: &serde_json::Value) -> Result<(), Error> {
        let written = self
            .write(
                SUMMARY_FILE,
                "application/json",
                summary.to_string().as_bytes(),
            )
            .map(|_| ());
        let flushed = self.flush();
        written.and(flushed)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<SharedDir>> = const { RefCell::new(None) };
}

/// Runs `f` with `dir` bound as this thread's current artifact directory, restoring the previous
/// binding afterwards, panic or not.
pub fn bind_current<R>(dir: Option<SharedDir>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<SharedDir>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), dir));
    let _restore = Restore(previous);
    f()
}

/// Writes one artifact into the directory bound to this thread and returns its relative path (the
/// shape of `AudioIo::write`). Errors report the I/O kind alone, never a native path.
pub fn write_current(relative: &str, bytes: &[u8]) -> Result<String, String> {
    let dir = CURRENT.with(|slot| slot.borrow().clone()).ok_or_else(|| {
        "this instance has no artifact directory (the daemon was started without an artifacts \
         root)"
            .to_string()
    })?;
    let guard = dir.lock().unwrap_or_else(|e| e.into_inner());
    let media = if relative.ends_with(".wav") {
        "audio/wav"
    } else if relative.ends_with(".png") {
        "image/png"
    } else {
        "application/octet-stream"
    };
    match guard.write(relative, media, bytes) {
        Ok(reference) => Ok(reference.path.clone()),
        Err(Error::Io(_, e)) => Err(format!("`{relative}`: {}", e.kind())),
        Err(Error::Guard(_)) => Err(format!(
            "`{relative}` could not be checked against the instance artifact root"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Largest artifact [`read_current`] loads: a golden image or snapshot export, never a stream.
pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

/// Reads one artifact from the directory bound to this thread (the shape of `ArtifactIo::read`),
/// through both containment checks and [`crate::host_file::read_regular`]. No refusal names a
/// native path.
pub fn read_current(relative: &str) -> Result<Vec<u8>, String> {
    let dir = CURRENT.with(|slot| slot.borrow().clone()).ok_or_else(|| {
        "this instance has no artifact directory (the daemon was started without an artifacts \
         root)"
            .to_string()
    })?;
    let guard = dir.lock().unwrap_or_else(|e| e.into_inner());
    let native = guard.native(relative).map_err(|e| match e {
        Error::Io(_, e) => format!("`{relative}`: {}", e.kind()),
        Error::Guard(_) => format!("`{relative}` could not be checked against the artifact root"),
        other => other.to_string(),
    })?;
    let canonical = crate::host_file::canonical(&native)
        .map_err(|_| format!("`{relative}` is not a readable artifact"))?;
    if !crate::paths::contains(guard.root(), &canonical).unwrap_or(false) {
        return Err(format!("`{relative}` is not a readable artifact"));
    }
    crate::host_file::read_regular(&canonical, MAX_READ_BYTES)
        .map_err(|_| format!("`{relative}` is not a readable artifact"))
}

impl Drop for ArtifactDir {
    /// Flushes on drop; the shutdown path calls [`ArtifactDir::flush`] explicitly, because `Drop`
    /// cannot report a failure.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[derive(Debug)]
pub enum Error {
    Path(PathRefusal),
    Escapes(String),
    /// The path names an existing symlink, which a write would follow out of the root.
    Symlink(String),
    Io(PathBuf, io::Error),
    Guard(crate::paths::GuardError),
    Png(crate::png::PngError),
    /// `pemu_api` refused the reference's path. The overlap is deliberate: this module decides
    /// what it creates, the API what an `Output` may carry.
    Reference(pemu_api::output::PathError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Path(e) => write!(f, "{e}"),
            Error::Escapes(path) => {
                write!(f, "`{path}` resolves outside the instance artifact root")
            }
            Error::Symlink(path) => write!(
                f,
                "`{path}` is a symlink; an artifact path names a file this directory created"
            ),
            Error::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Error::Guard(e) => write!(f, "{e}"),
            Error::Png(e) => write!(f, "{e}"),
            Error::Reference(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-artifacts-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        dir
    }

    #[test]
    fn finish_writes_the_summary_and_flushes_the_streams() {
        let root = temp_root("finish");
        let mut dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        dir.append(SERIAL_FILE, b"hello\n").expect("append");
        dir.finish(&serde_json::json!({ "instance": "p1", "final_vt_us": 7 }))
            .expect("finish");
        let summary = std::fs::read_to_string(root.join("run-0001/p1").join(SUMMARY_FILE))
            .expect("summary.json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&summary).expect("json")["final_vt_us"],
            7
        );
        assert_eq!(
            std::fs::read(root.join("run-0001/p1").join(SERIAL_FILE)).expect("serial.log"),
            b"hello\n"
        );
    }

    #[test]
    fn the_current_directory_is_bound_per_thread_and_restored_after_the_call() {
        let root = temp_root("current");
        let dir: SharedDir = Arc::new(Mutex::new(
            ArtifactDir::create(&root, "run-0001", "p1").expect("create"),
        ));
        let unbound = write_current("audio/tx.wav", b"x").expect_err("nothing is bound");
        assert!(unbound.contains("no artifact directory"), "{unbound}");

        let path = bind_current(Some(Arc::clone(&dir)), || {
            write_current("audio/tx.wav", b"RIFF").expect("bound")
        });
        assert_eq!(path, "audio/tx.wav");
        assert_eq!(
            std::fs::read(root.join("run-0001/p1/audio/tx.wav")).expect("written"),
            b"RIFF"
        );
        assert!(
            write_current("audio/tx.wav", b"x").is_err(),
            "the binding is restored"
        );

        let refused = bind_current(Some(dir), || write_current("../escape.wav", b"x"))
            .expect_err("a parent segment");
        assert!(!refused.contains(&*root.to_string_lossy()), "{refused}");
    }

    #[test]
    fn an_artifact_reads_back_from_the_bound_directory_and_never_through_an_escaping_link() {
        let root = temp_root("read");
        let dir: SharedDir = Arc::new(Mutex::new(
            ArtifactDir::create(&root, "run-0001", "p1").expect("create"),
        ));
        assert!(read_current("screens/a.png").is_err(), "nothing is bound");
        bind_current(Some(Arc::clone(&dir)), || {
            write_current("screens/golden.png", b"PNG").expect("written");
            assert_eq!(read_current("screens/golden.png"), Ok(b"PNG".to_vec()));
            assert!(read_current("screens/absent.png").is_err());
            assert!(read_current("../outside.png").is_err());
        });
        #[cfg(unix)]
        {
            std::fs::write(root.join("outside.png"), b"secret").expect("write");
            std::os::unix::fs::symlink(
                root.join("outside.png"),
                root.join("run-0001/p1/screens/link.png"),
            )
            .expect("symlink");
            let refused = bind_current(Some(dir), || read_current("screens/link.png"))
                .expect_err("an escaping link");
            assert!(!refused.contains(&*root.to_string_lossy()), "{refused}");
        }
    }

    #[test]
    fn a_written_artifact_reports_a_relative_forward_slash_path() {
        let root = temp_root("relative");
        let dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        let reference = dir
            .screenshot(7, "boot", 2, 1, &[0xf800, 0x001f])
            .expect("screenshot");
        assert_eq!(reference.path, "screens/0007-boot.png");
        assert!(
            !reference.path.contains('\\') && !reference.path.starts_with('/'),
            "no absolute or backslash path ever reaches an agent"
        );
        assert!(
            !reference.path.contains(&root.display().to_string()),
            "the root never appears in the reference"
        );
        pemu_api::output::check_artifact_path(&reference.path).expect("the API accepts it");
        assert!(dir.root().join(&reference.path).exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_capture_lands_in_the_audio_directory_with_a_playable_header() {
        let root = temp_root("audio");
        let dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        let spec = crate::wav::WavSpec::mono(16_000);
        let tx = dir.capture("tx", 1, &spec, &[1, 2, 3, 4]).expect("tx");
        let rx = dir.capture("rx", 1, &spec, &[]).expect("rx");
        assert_eq!(tx.path, "audio/tx-0001.wav");
        assert_eq!(rx.path, "audio/rx-0001.wav");
        assert_eq!(tx.media_type, "audio/wav");
        let bytes = std::fs::read(dir.root().join(&tx.path)).expect("read back");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(bytes.len(), crate::wav::HEADER_BYTES + 8);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_path_that_would_leave_the_root_is_refused_by_name() {
        assert_eq!(check_relative(""), Err(PathRefusal::Empty));
        assert_eq!(check_relative("/etc/passwd"), Err(PathRefusal::Absolute));
        assert_eq!(check_relative("C:/windows"), Err(PathRefusal::Absolute));
        assert_eq!(
            check_relative("screens/../../secret.png"),
            Err(PathRefusal::Parent)
        );
        assert_eq!(
            check_relative("screens\\0001-boot.png"),
            Err(PathRefusal::Backslash),
            "a backslash is a separator on one host and a name character on the other"
        );
        assert_eq!(
            check_relative("Screens/0001.png"),
            Err(PathRefusal::Name("Screens".to_string()))
        );
        assert!(check_relative("screens/0001-boot.png").is_ok());
        assert!(check_relative("ui/0012-menu.json").is_ok());
        assert!(check_relative("summary.json").is_ok());
    }

    #[test]
    fn the_write_path_refuses_the_same_escapes() {
        let root = temp_root("escape");
        let dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        for bad in ["../escaped.txt", "/tmp/escaped.txt", "a\\b.txt"] {
            let err = dir.write(bad, "text/plain", b"no").expect_err("refused");
            assert!(matches!(err, Error::Path(_)), "{bad} gave {err}");
        }
        assert!(!root.join("escaped.txt").exists());
        assert!(
            !root
                .parent()
                .expect("a parent")
                .join("escaped.txt")
                .exists()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Unix only, because planting the symlink needs `std::os::unix`; the check is portable.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_at_a_valid_name_is_refused_rather_than_followed() {
        // `0001-boot.png` is a valid name and only the parent is canonicalized, so without the
        // leaf check the write would follow the link out of the root.
        let root = temp_root("symlink");
        let dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        let outside = root.join("outside.txt");
        std::fs::write(&outside, b"the target must survive").expect("the target file");
        let leaf = dir.root().join(SCREENS_DIR).join("0001-boot.png");
        std::os::unix::fs::symlink(&outside, &leaf).expect("a symlink at a valid artifact name");

        let err = dir
            .write("screens/0001-boot.png", "image/png", b"overwritten")
            .expect_err("a symlinked leaf is refused");
        assert!(matches!(err, Error::Symlink(_)), "got {err}");
        assert_eq!(
            std::fs::read(&outside).expect("the target still reads"),
            b"the target must survive",
            "the file the link pointed at must be untouched"
        );
        // The appending path goes through the same check.
        let mut dir = dir;
        let serial = dir.root().join(SERIAL_FILE);
        std::os::unix::fs::symlink(&outside, &serial).expect("a symlink at the serial log");
        let err = dir
            .append(SERIAL_FILE, b"a line")
            .expect_err("the append path is refused too");
        assert!(matches!(err, Error::Symlink(_)), "got {err}");
        assert_eq!(
            std::fs::read(&outside).expect("the target still reads"),
            b"the target must survive"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_run_id_or_instance_that_is_not_a_name_is_refused() {
        let root = temp_root("names");
        assert!(matches!(
            ArtifactDir::create(&root, "../run", "p1"),
            Err(Error::Path(PathRefusal::Name(_)))
        ));
        assert!(matches!(
            ArtifactDir::create(&root, "run-0001", "P1"),
            Err(Error::Path(PathRefusal::Name(_)))
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_appended_stream_is_only_complete_after_a_flush() {
        let root = temp_root("flush");
        let mut dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        for n in 0..3u32 {
            dir.append(EVENTS_FILE, format!("{{\"n\":{n}}}\n").as_bytes())
                .expect("append");
        }
        assert_eq!(dir.open_paths(), [EVENTS_FILE]);
        dir.flush().expect("flush");
        dir.flush().expect("flushing twice is harmless");
        let text = std::fs::read_to_string(dir.root().join(EVENTS_FILE)).expect("read back");
        assert_eq!(text.lines().count(), 3);
        assert_eq!(text.lines().next(), Some("{\"n\":0}"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dropping_the_directory_flushes_what_is_still_open() {
        let root = temp_root("drop");
        {
            let mut dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
            dir.append(SERIAL_FILE, b"I (1) boot: hello\n")
                .expect("append");
        }
        let text =
            std::fs::read_to_string(root.join("run-0001/p1").join(SERIAL_FILE)).expect("read back");
        assert_eq!(text, "I (1) boot: hello\n");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_native_path_renders_with_forward_slashes_and_refuses_an_absolute_one() {
        assert_eq!(
            to_forward_slashes(Path::new("screens/0001-boot.png")).as_deref(),
            Some("screens/0001-boot.png")
        );
        assert_eq!(
            to_forward_slashes(Path::new("./ui/0001.json")).as_deref(),
            Some("ui/0001.json")
        );
        assert_eq!(to_forward_slashes(Path::new("/tmp/x")), None);
    }

    #[test]
    fn the_containment_check_uses_the_canonical_comparison() {
        let root = temp_root("contains");
        let dir = ArtifactDir::create(&root, "run-0001", "p1").expect("create");
        let native = dir.native("screens/0001-boot.png").expect("allowed");
        assert!(
            crate::paths::contains(dir.root(), native.parent().expect("a parent"))
                .expect("compare"),
            "the screens directory is inside the root"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
