//! The archives of a package directory: `.tar.gz` (POSIX ustar, RFC 1952 gzip) for the macOS
//! target and `.zip` (deflate, no ZIP64, PKWARE APPNOTE 6.3) for the Windows targets.
//!
//! Both are written in-process, so either host writes either format and nothing depends on a tool
//! on `PATH` (Windows has no `gzip`). Only regular files go in. The archives are byte-reproducible
//! from the tree: entries sorted by path, every time [`EPOCH`], owner 0 with no names, mode `0644`
//! or `0755`, and a gzip header with no name and no time.

use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::DeflateEncoder;
use flate2::{Compression, GzBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `.tar.gz`: the macOS target.
    TarGz,
    /// `.zip`: the Windows targets.
    Zip,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::TarGz => ".tar.gz",
            Format::Zip => ".zip",
        }
    }
}

/// Seconds since the Unix epoch of every entry: 1980-01-01T00:00:00Z. Zip's MS-DOS date field
/// cannot say anything earlier, so both formats use it.
pub const EPOCH: u64 = 315_532_800;

/// Writes `<dir><extension>` beside `dir` and returns its path, every file under
/// `<dir name>/<relative path>`; `executables` get mode `0755` in a tar archive.
pub fn write(dir: &Path, format: Format, executables: &[&str]) -> Result<PathBuf, String> {
    let name = dir
        .file_name()
        .ok_or_else(|| format!("{} has no file name", dir.display()))?
        .to_string_lossy()
        .into_owned();
    let parent = dir
        .parent()
        .ok_or_else(|| format!("{} has no parent", dir.display()))?;
    let archive = parent.join(format!("{name}{}", format.extension()));
    let mut entries = Vec::new();
    collect(dir, dir, &mut entries)?;
    entries.sort();
    let mut files = Vec::with_capacity(entries.len());
    for (relative, path) in entries {
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let executable = executables.contains(&relative.as_str());
        files.push(Entry {
            path: format!("{name}/{relative}"),
            bytes,
            executable,
        });
    }
    let bytes = match format {
        Format::TarGz => targz(&files)?,
        Format::Zip => zip(&files)?,
    };
    std::fs::write(&archive, bytes).map_err(|e| format!("{}: {e}", archive.display()))?;
    Ok(archive)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// `/`-separated path inside the archive.
    pub path: String,
    pub bytes: Vec<u8>,
    /// Mode `0755` rather than `0644` in a tar header.
    pub executable: bool,
}

/// Every file below `dir`, as (`/`-separated path relative to `base`, path on disk).
fn collect(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            collect(base, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .map_err(|_| format!("{} is not below {}", path.display(), base.display()))?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push((relative, path));
    }
    Ok(())
}

/// A ustar block.
const BLOCK: usize = 512;

/// The gzip-compressed ustar archive of `files`, in the given order.
pub fn targz(files: &[Entry]) -> Result<Vec<u8>, String> {
    let mut tar = Vec::new();
    for file in files {
        tar.extend_from_slice(&ustar_header(file)?);
        tar.extend_from_slice(&file.bytes);
        let pad = (BLOCK - file.bytes.len() % BLOCK) % BLOCK;
        tar.resize(tar.len() + pad, 0);
    }
    // The end of the archive is two zero blocks (ustar Interchange Format).
    tar.resize(tar.len() + 2 * BLOCK, 0);
    // `GzBuilder` writes `MTIME` 0 and no `FNAME` unless asked, so the gzip header carries nothing
    // of the build (RFC 1952 section 2.3.1: `MTIME = 0 means no time stamp is available`).
    let mut gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    gz.write_all(&tar).map_err(|e| format!("gzip: {e}"))?;
    gz.finish().map_err(|e| format!("gzip: {e}"))
}

/// `bytes` as one gzip member at the best level, with the same build-free header as [`targz`].
pub fn gzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    gz.write_all(bytes).map_err(|e| format!("gzip: {e}"))?;
    gz.finish().map_err(|e| format!("gzip: {e}"))
}

/// The 512-byte ustar header of one regular file.
fn ustar_header(file: &Entry) -> Result<[u8; BLOCK], String> {
    let mut header = [0u8; BLOCK];
    let (prefix, name) = split_ustar_path(&file.path)?;
    header[..name.len()].copy_from_slice(name.as_bytes());
    let mode = if file.executable { 0o755 } else { 0o644 };
    octal(&mut header[100..108], mode)?;
    octal(&mut header[108..116], 0)?; // uid
    octal(&mut header[116..124], 0)?; // gid
    octal(&mut header[124..136], file.bytes.len() as u64)?;
    octal(&mut header[136..148], EPOCH)?;
    header[156] = b'0'; // typeflag: regular file
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    // The checksum is the byte sum of the header with the checksum field read as eight spaces,
    // written as six octal digits, a NUL and a space.
    header[148..156].copy_from_slice(b"        ");
    let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
    let digits = format!("{sum:06o}");
    header[148..154].copy_from_slice(digits.as_bytes());
    header[154] = 0;
    header[155] = b' ';
    Ok(header)
}

/// `path` as ustar's `prefix` (at most 155 bytes) and `name` (at most 100 bytes), split at a `/`.
fn split_ustar_path(path: &str) -> Result<(&str, &str), String> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    for (at, _) in path.match_indices('/') {
        let (prefix, name) = (&path[..at], &path[at + 1..]);
        if prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            return Ok((prefix, name));
        }
    }
    Err(format!(
        "`{path}` does not fit a ustar header (a name of at most 100 bytes after a prefix of at \
         most 155)"
    ))
}

/// `value` as zero-padded octal digits filling `field` but its last byte, which is NUL.
fn octal(field: &mut [u8], value: u64) -> Result<(), String> {
    let width = field.len() - 1;
    let digits = format!("{value:0width$o}");
    if digits.len() > width {
        return Err(format!("{value} does not fit a {width}-digit ustar field"));
    }
    field[..width].copy_from_slice(digits.as_bytes());
    field[width] = 0;
    Ok(())
}

/// Version needed to extract, and version made by: 2.0 (deflate), MS-DOS attribute host
/// (APPNOTE 4.4.2 and 4.4.3). The package has no mode bits to carry on Windows.
const ZIP_VERSION: u16 = 20;
/// General purpose flag bit 11: the name is UTF-8 (APPNOTE 4.4.4).
const ZIP_UTF8: u16 = 1 << 11;
/// Compression method 8, deflate (APPNOTE 4.4.5).
const ZIP_DEFLATE: u16 = 8;
/// MS-DOS date of [`EPOCH`]: day 1, month 1, year 1980 (`((1980 - 1980) << 9) | (1 << 5) | 1`).
const ZIP_DOS_DATE: u16 = 0x0021;
/// MS-DOS time of [`EPOCH`]: 00:00:00.
const ZIP_DOS_TIME: u16 = 0;

/// The deflate zip archive of `files`, in the given order.
pub fn zip(files: &[Entry]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for file in files {
        let name = file.path.as_bytes();
        let mut crc = flate2::Crc::new();
        crc.update(&file.bytes);
        let mut deflate = DeflateEncoder::new(Vec::new(), Compression::default());
        deflate
            .write_all(&file.bytes)
            .map_err(|e| format!("deflate: {e}"))?;
        let compressed = deflate.finish().map_err(|e| format!("deflate: {e}"))?;
        let offset = u32_field(out.len(), "a local header offset")?;
        let sizes = (
            u32_field(compressed.len(), "a compressed size")?,
            u32_field(file.bytes.len(), "a file size")?,
        );
        let name_len = u16::try_from(name.len())
            .map_err(|_| format!("`{}` is too long for a zip name", file.path))?;

        // Local file header (APPNOTE 4.3.7).
        put32(&mut out, 0x0403_4b50);
        put16(&mut out, ZIP_VERSION);
        put16(&mut out, ZIP_UTF8);
        put16(&mut out, ZIP_DEFLATE);
        put16(&mut out, ZIP_DOS_TIME);
        put16(&mut out, ZIP_DOS_DATE);
        put32(&mut out, crc.sum());
        put32(&mut out, sizes.0);
        put32(&mut out, sizes.1);
        put16(&mut out, name_len);
        put16(&mut out, 0); // extra field length
        out.extend_from_slice(name);
        out.extend_from_slice(&compressed);

        // Central directory file header (APPNOTE 4.3.12).
        put32(&mut central, 0x0201_4b50);
        put16(&mut central, ZIP_VERSION); // made by
        put16(&mut central, ZIP_VERSION); // needed to extract
        put16(&mut central, ZIP_UTF8);
        put16(&mut central, ZIP_DEFLATE);
        put16(&mut central, ZIP_DOS_TIME);
        put16(&mut central, ZIP_DOS_DATE);
        put32(&mut central, crc.sum());
        put32(&mut central, sizes.0);
        put32(&mut central, sizes.1);
        put16(&mut central, name_len);
        put16(&mut central, 0); // extra field length
        put16(&mut central, 0); // file comment length
        put16(&mut central, 0); // disk number start
        put16(&mut central, 0); // internal file attributes
        put32(&mut central, 0); // external file attributes
        put32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let count = u16::try_from(files.len()).map_err(|_| {
        format!(
            "{} files is more than a zip without ZIP64 holds",
            files.len()
        )
    })?;
    let central_offset = u32_field(out.len(), "the central directory offset")?;
    let central_size = u32_field(central.len(), "the central directory size")?;
    out.extend_from_slice(&central);
    // End of central directory record (APPNOTE 4.3.16).
    put32(&mut out, 0x0605_4b50);
    put16(&mut out, 0); // number of this disk
    put16(&mut out, 0); // disk where the central directory starts
    put16(&mut out, count);
    put16(&mut out, count);
    put32(&mut out, central_size);
    put32(&mut out, central_offset);
    put16(&mut out, 0); // comment length
    Ok(out)
}

/// `value` as a 32-bit zip field; this writer has no ZIP64, so a bigger one is refused.
fn u32_field(value: usize, what: &str) -> Result<u32, String> {
    u32::try_from(value)
        .map_err(|_| format!("{what} of {value} bytes needs ZIP64, which this writer does not do"))
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn files() -> Vec<Entry> {
        let long = format!("{}/{}/leaf.txt", "d".repeat(90), "e".repeat(40));
        vec![
            Entry {
                path: "pkg/passportsim".into(),
                bytes: b"\x7fELF binary".to_vec(),
                executable: true,
            },
            Entry {
                path: "pkg/payload/web/index.html".into(),
                bytes: b"<!doctype html>\n".repeat(100),
                executable: false,
            },
            Entry {
                path: "pkg/empty".into(),
                bytes: Vec::new(),
                executable: false,
            },
            Entry {
                path: long,
                bytes: vec![7u8; 1000],
                executable: false,
            },
        ]
    }

    /// Reads a ustar archive back: (path, mode, bytes) per member, checksums verified.
    fn untar(tar: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let field = |bytes: &[u8]| {
            let text = std::str::from_utf8(bytes).unwrap();
            u64::from_str_radix(text.trim_end_matches(['\0', ' ']).trim(), 8).unwrap()
        };
        let text = |bytes: &[u8]| {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            String::from_utf8(bytes[..end].to_vec()).unwrap()
        };
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let header = &tar[at..at + BLOCK];
            if header.iter().all(|&b| b == 0) {
                assert!(tar[at + BLOCK..at + 2 * BLOCK].iter().all(|&b| b == 0));
                assert_eq!(at + 2 * BLOCK, tar.len(), "two zero blocks end the archive");
                break;
            }
            let mut blank = header.to_vec();
            blank[148..156].copy_from_slice(b"        ");
            let sum: u64 = blank.iter().map(|&b| u64::from(b)).sum();
            assert_eq!(field(&header[148..156]), sum, "the header checksum");
            assert_eq!(&header[257..263], b"ustar\0");
            assert_eq!(header[156], b'0');
            assert_eq!(field(&header[136..148]), EPOCH);
            let prefix = text(&header[345..500]);
            let name = text(&header[..100]);
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let size = field(&header[124..136]) as usize;
            let mode = field(&header[100..108]) as u32;
            at += BLOCK;
            out.push((path, mode, tar[at..at + size].to_vec()));
            at += size.div_ceil(BLOCK) * BLOCK;
        }
        out
    }

    /// Reads a zip archive back through its central directory: (path, bytes) per entry, CRCs and
    /// both headers checked against each other.
    fn unzip(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let u16_at = |at: usize| u16::from_le_bytes([zip[at], zip[at + 1]]);
        let u32_at =
            |at: usize| u32::from_le_bytes([zip[at], zip[at + 1], zip[at + 2], zip[at + 3]]);
        let end = zip.len() - 22;
        assert_eq!(u32_at(end), 0x0605_4b50, "end of central directory");
        let count = usize::from(u16_at(end + 10));
        let mut at = u32_at(end + 16) as usize;
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(at), 0x0201_4b50, "central header");
            assert_eq!(u16_at(at + 8), ZIP_UTF8);
            assert_eq!(u16_at(at + 10), ZIP_DEFLATE);
            assert_eq!(
                (u16_at(at + 12), u16_at(at + 14)),
                (ZIP_DOS_TIME, ZIP_DOS_DATE)
            );
            let crc = u32_at(at + 16);
            let compressed = u32_at(at + 20) as usize;
            let size = u32_at(at + 24) as usize;
            let name_len = usize::from(u16_at(at + 28));
            let local = u32_at(at + 42) as usize;
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
            assert_eq!(u32_at(local), 0x0403_4b50, "local header");
            assert_eq!(u32_at(local + 14), crc, "both headers carry the CRC");
            let data = local + 30 + usize::from(u16_at(local + 26));
            let mut bytes = Vec::new();
            flate2::read::DeflateDecoder::new(&zip[data..data + compressed])
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes.len(), size);
            let mut check = flate2::Crc::new();
            check.update(&bytes);
            assert_eq!(check.sum(), crc, "{name}");
            out.push((name, bytes));
            at += 46 + name_len;
        }
        out
    }

    #[test]
    fn the_tar_gz_reads_back_with_modes_long_paths_and_a_blank_gzip_header() {
        let gz = targz(&files()).unwrap();
        // RFC 1952: ID1 ID2 CM FLG MTIME(4). No FNAME flag, no time.
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 8]);
        assert_eq!(gz[3] & 0x08, 0, "no file name in the gzip header");
        assert_eq!(&gz[4..8], &[0, 0, 0, 0], "no time in the gzip header");
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut tar)
            .unwrap();
        let members = untar(&tar);
        let want = files();
        assert_eq!(members.len(), want.len());
        for ((path, mode, bytes), file) in members.iter().zip(&want) {
            assert_eq!(path, &file.path);
            assert_eq!(bytes, &file.bytes);
            assert_eq!(*mode, if file.executable { 0o755 } else { 0o644 });
        }
    }

    #[test]
    fn the_zip_reads_back_through_its_central_directory() {
        let zip = zip(&files()).unwrap();
        let entries = unzip(&zip);
        let want = files();
        assert_eq!(entries.len(), want.len());
        for ((path, bytes), file) in entries.iter().zip(&want) {
            assert_eq!(path, &file.path);
            assert_eq!(bytes, &file.bytes);
        }
    }

    /// Both archives of one tree are the same bytes each time, whatever the file times on disk.
    #[test]
    fn an_archive_of_a_tree_is_reproducible_and_named_by_its_format() {
        let base = std::env::temp_dir().join(format!("pemu-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("passportsim-0.0.0-test");
        std::fs::create_dir_all(dir.join("payload/web")).unwrap();
        std::fs::write(dir.join("passportsim.exe"), b"MZ").unwrap();
        std::fs::write(dir.join("payload/web/index.html"), b"<html>").unwrap();
        for format in [Format::TarGz, Format::Zip] {
            let first = write(&dir, format, &["passportsim.exe"]).unwrap();
            assert_eq!(
                first.file_name().unwrap().to_string_lossy(),
                format!("passportsim-0.0.0-test{}", format.extension())
            );
            let bytes = std::fs::read(&first).unwrap();
            // A later write of the same files, with newer times on disk.
            std::fs::write(dir.join("payload/web/index.html"), b"<html>").unwrap();
            let again = write(&dir, format, &["passportsim.exe"]).unwrap();
            assert_eq!(std::fs::read(&again).unwrap(), bytes, "{format:?}");
        }
        let zip = std::fs::read(base.join("passportsim-0.0.0-test.zip")).unwrap();
        let names: Vec<String> = unzip(&zip).into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "passportsim-0.0.0-test/passportsim.exe",
                "passportsim-0.0.0-test/payload/web/index.html"
            ],
            "sorted, `/`-separated, under the directory's own name"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_path_ustar_cannot_hold_is_refused() {
        assert!(split_ustar_path(&"x".repeat(101)).is_err());
        assert!(split_ustar_path(&format!("{}/n", "p".repeat(156))).is_err());
        assert_eq!(split_ustar_path("a/b").unwrap(), ("", "a/b"));
    }
}
