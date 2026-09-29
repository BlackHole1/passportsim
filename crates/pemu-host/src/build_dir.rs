//! An `idf.py build` directory as a firmware argument: `passportsim start <build dir>`.
//!
//! The layout comes only from the `flasher_args.json` that `idf.py build` writes; a directory
//! without it is not a build directory. The result is the merged 8 MB image `esptool merge_bin`
//! would write, with NVS and the cardid window left erased, so it carries nothing secret.
//!
//! Every part is read through [`crate::host_file::read_regular`] and must stay inside the build
//! directory. No refusal carries a host path.

use std::path::{Component, Path, PathBuf};

use pemu_api::error::{ApiError, E_ASSET_MISSING, E_USAGE};

/// The file `idf.py build` writes, and the only description of the layout this module reads.
pub const FLASHER_ARGS: &str = "flasher_args.json";

const ARGS_BYTES: u64 = 1 << 20;

/// Whether `path` is an `idf.py` build directory: a directory holding [`FLASHER_ARGS`]. Otherwise
/// the caller reads the path as a file, so a mistyped path gets a refusal about that path.
pub fn is_build_dir(path: &Path) -> bool {
    path.is_dir() && path.join(FLASHER_ARGS).is_file()
}

/// The merged 8 MB image of the `idf.py` build directory `dir`, named `fw` by the caller.
///
/// # Errors
///
/// `E_USAGE` when `flasher_args.json` is malformed, `E_ASSET_MISSING` when a file it names cannot
/// be read.
pub fn merged_image_bytes(dir: &Path, fw: &str, flash_bytes: u64) -> Result<Vec<u8>, ApiError> {
    let root = crate::host_file::canonical(dir).map_err(|_| not_found(fw))?;
    let args = crate::host_file::read_regular(&root.join(FLASHER_ARGS), ARGS_BYTES)
        .map_err(|_| not_found(fw))?;
    let files = flash_files(&args, fw)?;
    let size = usize::try_from(flash_bytes).unwrap_or(usize::MAX);
    let mut flash = vec![0xFFu8; size];
    let mut written: Vec<(usize, usize, String)> = Vec::new();
    for (offset, name) in files {
        let path = inside(&root, &name, fw)?;
        let bytes = crate::host_file::read_regular(&path, flash_bytes)
            .map_err(|_| missing_part(fw, &name))?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| too_big(fw, &name))?;
        if end > size {
            return Err(too_big(fw, &name));
        }
        if let Some((_, _, other)) = written
            .iter()
            .find(|(start, stop, _)| offset < *stop && *start < end)
        {
            return Err(ApiError::new(
                E_USAGE,
                format!(
                    "`{fw}`: `{FLASHER_ARGS}` puts `{name}` and `{other}` at overlapping flash \
                     offsets"
                ),
            )
            .with_hint("run `idf.py build` again; a merged image cannot hold both"));
        }
        flash[offset..end].copy_from_slice(&bytes);
        written.push((offset, end, name));
    }
    if written.is_empty() {
        return Err(ApiError::new(
            E_USAGE,
            format!("`{fw}`: `{FLASHER_ARGS}` lists no `flash_files`"),
        )
        .with_hint(
            "`idf.py build` writes the bootloader, the partition table and the app into it",
        ));
    }
    Ok(flash)
}

/// The `flash_files` map as `(offset, name)` in ascending offset order. Parsed by hand because
/// `idf.py` writes the offsets as hex string keys (`"0x10000"`).
fn flash_files(args: &[u8], fw: &str) -> Result<Vec<(usize, String)>, ApiError> {
    let document: serde_json::Value = serde_json::from_slice(args).map_err(|_| malformed(fw))?;
    let map = document
        .get("flash_files")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| malformed(fw))?;
    let mut files = Vec::with_capacity(map.len());
    for (offset, name) in map {
        let offset = offset
            .strip_prefix("0x")
            .or_else(|| offset.strip_prefix("0X"))
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .or_else(|| offset.parse::<usize>().ok())
            .ok_or_else(|| malformed(fw))?;
        let name = name.as_str().ok_or_else(|| malformed(fw))?;
        files.push((offset, name.to_owned()));
    }
    files.sort_by_key(|(offset, _)| *offset);
    Ok(files)
}

/// `root.join(name)`, canonical, only when `name` is relative with no `..` and the result is still
/// inside `root`. Escaping is `E_USAGE`; a missing file is `E_ASSET_MISSING` (an unfinished build).
fn inside(root: &Path, name: &str, fw: &str) -> Result<PathBuf, ApiError> {
    let relative = Path::new(name);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(outside(fw, name));
    }
    let path =
        crate::host_file::canonical(&root.join(relative)).map_err(|_| missing_part(fw, name))?;
    if !path.starts_with(root) {
        return Err(outside(fw, name));
    }
    Ok(path)
}

fn not_found(fw: &str) -> ApiError {
    pemu_api::commands::start::firmware_not_found(fw)
}

fn malformed(fw: &str) -> ApiError {
    ApiError::new(
        E_USAGE,
        format!("`{fw}`: `{FLASHER_ARGS}` is not the document `idf.py build` writes"),
    )
    .with_hint("it needs a `flash_files` object of flash offset to file name")
}

fn missing_part(fw: &str, name: &str) -> ApiError {
    ApiError::new(
        E_ASSET_MISSING,
        format!(
            "`{fw}`: `{FLASHER_ARGS}` names `{name}`, which this build directory does not have"
        ),
    )
    .with_hint("run `idf.py build` again, or pass the merged image instead")
}

fn outside(fw: &str, name: &str) -> ApiError {
    ApiError::new(
        E_USAGE,
        format!("`{fw}`: `{FLASHER_ARGS}` names `{name}`, which is not inside the build directory"),
    )
    .with_hint("a build directory is read as one directory; nothing outside it is opened")
}

fn too_big(fw: &str, name: &str) -> ApiError {
    ApiError::new(
        E_USAGE,
        format!("`{fw}`: `{name}` does not fit the flash at the offset `{FLASHER_ARGS}` gives it"),
    )
    .with_hint("the part is larger than the 8 MB flash of this board")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLASH: u64 = 8 * 1024 * 1024;

    fn build_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pemu-build-dir-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bootloader")).expect("a build directory");
        std::fs::create_dir_all(dir.join("partition_table")).expect("a partition table directory");
        std::fs::write(dir.join("bootloader/bootloader.bin"), [0x11u8; 32]).expect("bootloader");
        std::fs::write(
            dir.join("partition_table/partition-table.bin"),
            [0x22u8; 32],
        )
        .expect("partition table");
        std::fs::write(dir.join("app.bin"), [0x33u8; 32]).expect("app");
        std::fs::write(
            dir.join(FLASHER_ARGS),
            br#"{
                "flash_files": {
                    "0x0": "bootloader/bootloader.bin",
                    "0x10000": "app.bin",
                    "0x8000": "partition_table/partition-table.bin"
                }
            }"#,
        )
        .expect(FLASHER_ARGS);
        dir
    }

    #[test]
    fn the_parts_are_merged_at_the_offsets_the_build_recorded() {
        let dir = build_dir("merges");
        assert!(is_build_dir(&dir));
        let flash = merged_image_bytes(&dir, "<build>", FLASH).expect("the parts merge");
        assert_eq!(flash.len() as u64, FLASH);
        assert_eq!(&flash[0..32], &[0x11u8; 32]);
        assert_eq!(&flash[0x8000..0x8000 + 32], &[0x22u8; 32]);
        assert_eq!(&flash[0x10000..0x10000 + 32], &[0x33u8; 32]);
        assert!(
            flash[32..0x8000].iter().all(|&b| b == 0xFF),
            "the gaps between the parts are erased flash"
        );
        assert!(
            flash[0x10000 + 32..].iter().all(|&b| b == 0xFF),
            "NVS and the cardid window are erased"
        );
    }

    #[test]
    fn a_directory_without_flasher_args_is_not_a_build_directory() {
        let dir = build_dir("bare");
        std::fs::remove_file(dir.join(FLASHER_ARGS)).expect("removed");
        assert!(!is_build_dir(&dir));
        let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("nothing to read");
        assert_eq!(error.code, E_ASSET_MISSING);
    }

    #[test]
    fn a_part_the_build_directory_does_not_have_is_asset_missing() {
        let dir = build_dir("gone");
        std::fs::remove_file(dir.join("app.bin")).expect("removed");
        let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("a part is missing");
        assert_eq!(error.code, E_ASSET_MISSING);
        assert!(error.message.contains("app.bin"), "{}", error.message);
        assert!(
            !error.message.contains(dir.to_str().expect("a UTF-8 path")),
            "no host path is in the refusal: {}",
            error.message
        );
    }

    #[test]
    fn a_part_outside_the_build_directory_is_refused() {
        for name in ["../secret.bin", "/etc/hosts"] {
            let dir = build_dir("escape");
            std::fs::write(
                dir.join(FLASHER_ARGS),
                format!(r#"{{ "flash_files": {{ "0x0": "{name}" }} }}"#),
            )
            .expect(FLASHER_ARGS);
            let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("outside");
            assert_eq!(error.code, E_USAGE, "{name}");
        }
    }

    /// Unix only because the test creates a symbolic link; the canonical-path check it covers
    /// holds on both hosts.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_part_that_leaves_the_directory_is_refused() {
        let dir = build_dir("symlink");
        let outside = dir
            .join("..")
            .join(format!("pemu-build-dir-outside-{}.bin", std::process::id()));
        std::fs::write(&outside, [0x44u8; 32]).expect("a file outside");
        std::os::unix::fs::symlink(&outside, dir.join("app.bin.link")).expect("a symlink");
        std::fs::write(
            dir.join(FLASHER_ARGS),
            br#"{ "flash_files": { "0x0": "app.bin.link" } }"#,
        )
        .expect(FLASHER_ARGS);
        let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("outside");
        assert_eq!(error.code, E_USAGE);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_malformed_flasher_args_is_usage() {
        let dir = build_dir("malformed");
        for text in [
            &b"not json"[..],
            b"{}",
            br#"{"flash_files": {"nope": "a.bin"}}"#,
        ] {
            std::fs::write(dir.join(FLASHER_ARGS), text).expect(FLASHER_ARGS);
            let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("malformed");
            assert_eq!(error.code, E_USAGE, "{}", String::from_utf8_lossy(text));
        }
    }

    #[test]
    fn overlapping_parts_are_refused_rather_than_silently_layered() {
        let dir = build_dir("overlap");
        std::fs::write(
            dir.join(FLASHER_ARGS),
            br#"{ "flash_files": { "0x0": "app.bin", "0x10": "app.bin" } }"#,
        )
        .expect(FLASHER_ARGS);
        let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("overlap");
        assert_eq!(error.code, E_USAGE);
        assert!(error.message.contains("overlapping"), "{}", error.message);
    }

    #[test]
    fn a_part_that_does_not_fit_the_flash_is_refused() {
        let dir = build_dir("toobig");
        std::fs::write(
            dir.join(FLASHER_ARGS),
            format!(
                r#"{{ "flash_files": {{ "0x{:x}": "app.bin" }} }}"#,
                FLASH - 4
            ),
        )
        .expect(FLASHER_ARGS);
        let error = merged_image_bytes(&dir, "<build>", FLASH).expect_err("past the end");
        assert_eq!(error.code, E_USAGE);
    }
}
