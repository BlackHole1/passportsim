//! Reads one host file an agent named (a firmware image for `start`, a `mic_set` audio file):
//!
//! 1. the path is canonicalized, resolving every symlink;
//! 2. the canonical path passes [`crate::paths::refuse_device`];
//! 3. `symlink_metadata` must say regular file of at most `max_bytes`, checked before the open, so
//!    an oversized file is never read and a FIFO or directory is never opened;
//! 4. it is opened through [`crate::platform::open_regular_no_follow`], so a path swapped between
//!    steps 3 and 4 is refused;
//! 5. at most `max_bytes` are read, and a file that grew past that is refused.
//!
//! Every refusal is the same [`Refused`], with no reason and no path, so an agent cannot probe the
//! host's file system through the error text.

use std::io::Read;
use std::path::{Path, PathBuf};

/// A host file was not read. Deliberately carries nothing (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

pub fn canonical(path: &Path) -> Result<PathBuf, Refused> {
    std::fs::canonicalize(path).map_err(|_| Refused)
}

/// Reads the regular file at the canonical path `path` under steps 2 to 5 of the module doc.
pub fn read_regular(path: &Path, max_bytes: u64) -> Result<Vec<u8>, Refused> {
    crate::paths::refuse_device(path).map_err(|_| Refused)?;
    let meta = std::fs::symlink_metadata(path).map_err(|_| Refused)?;
    if !meta.file_type().is_file() || meta.len() > max_bytes {
        return Err(Refused);
    }
    let file = crate::platform::open_regular_no_follow(path, &meta).map_err(|_| Refused)?;
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| Refused)?;
    if bytes.len() as u64 > max_bytes {
        return Err(Refused);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-host-file-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        canonical(&dir).expect("canonical")
    }

    #[test]
    fn a_regular_file_within_the_cap_is_read_and_everything_else_is_one_refusal() {
        let dir = temp("cap");
        std::fs::write(dir.join("ok.bin"), [1u8; 16]).expect("write");
        assert_eq!(read_regular(&dir.join("ok.bin"), 16), Ok(vec![1u8; 16]));
        assert_eq!(
            read_regular(&dir.join("ok.bin"), 15),
            Err(Refused),
            "over the cap"
        );
        assert_eq!(read_regular(&dir.join("absent.bin"), 16), Err(Refused));
        assert_eq!(read_regular(&dir, 16), Err(Refused), "a directory");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_symlink_at_the_checked_path_and_a_fifo_are_refused_without_blocking() {
        let dir = temp("nofollow");
        std::fs::write(dir.join("target.bin"), [7u8; 4]).expect("write");
        std::os::unix::fs::symlink(dir.join("target.bin"), dir.join("link.bin")).expect("link");
        assert_eq!(
            read_regular(&dir.join("link.bin"), 16),
            Err(Refused),
            "not canonical"
        );
        let fifo = dir.join("pipe.bin");
        crate::platform::macos::make_fifo(&fifo).expect("mkfifo");
        assert_eq!(read_regular(&fifo, 16), Err(Refused));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_path_swapped_after_the_check_is_refused() {
        let dir = temp("swap");
        let path = dir.join("fw.bin");
        std::fs::write(&path, [1u8; 8]).expect("write");
        let checked = std::fs::symlink_metadata(&path).expect("metadata");

        std::fs::write(dir.join("other.bin"), [2u8; 8]).expect("write");
        std::fs::rename(dir.join("other.bin"), &path).expect("swap for another file");
        assert!(crate::platform::open_regular_no_follow(&path, &checked).is_err());

        std::fs::remove_file(&path).expect("remove");
        std::fs::write(dir.join("elsewhere.bin"), [1u8; 8]).expect("write");
        std::os::unix::fs::symlink(dir.join("elsewhere.bin"), &path).expect("swap for a link");
        assert!(crate::platform::open_regular_no_follow(&path, &checked).is_err());
    }
}
