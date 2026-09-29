//! The device-side region MD5: the flasher stub's `SPI_FLASH_MD5` (opcode 0x13, payload `<IIII`
//! address, size, 0, 0; reply the 16 raw digest bytes). esptool's command line exposes no region
//! digest, so a fixed helper script ([`HELPER_SOURCE`]) uses esptool as a library, run by the same
//! interpreter esptool resolved as. No flash or identity byte passes through a file or stdout.
//!
//! A digest of cardid is allowed where a read is not: it is read-only and one-way, so the bytes
//! never leave the Passport. An esptool that is not a Python entry point has no interpreter to run
//! the helper with, and the session answers [`SessionError::Unsupported`] before any write.

use crate::flow::SessionError;
use crate::rehearse::{EsptoolCommand, Invocation};
use crate::rules::{FLASH_SIZE, Refusal, Rule};

/// The helper's file name in the scratch directory; part of the invocation gate.
pub const HELPER_NAME: &str = "region-md5.py";

/// The line the helper prints, and the only thing the planner reads from its output.
const DIGEST_PREFIX: &str = "SPI_FLASH_MD5 ";

/// The fixed helper script: connect with `usb_reset`, upload the stub, take one region digest,
/// print it. It uses only esptool's public `detect_chip`, `run_stub()` and `flash_md5sum()`, and
/// leaves the chip for the flow to reset.
pub const HELPER_SOURCE: &str = r#"# Written by pemu-planner into an owner-only scratch directory; never committed.
# The region digest is taken by the flasher stub (SPI_FLASH_MD5, opcode 0x13) through esptool
# used as a library, so no flash byte and no identity byte passes through a file or stdout.
import argparse
import sys

import esptool


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--port", required=True)
    parser.add_argument("--addr", required=True)
    parser.add_argument("--size", required=True)
    parser.add_argument("--baud", default="115200")
    args = parser.parse_args()
    addr = int(args.addr, 16)
    size = int(args.size, 16)
    if size <= 0:
        print("SPI_FLASH_MD5 ERROR empty region", file=sys.stderr)
        return 2
    esp = esptool.detect_chip(args.port, int(args.baud), "usb_reset")
    try:
        if esp.CHIP_NAME != "ESP32-C3":
            print("SPI_FLASH_MD5 ERROR not an ESP32-C3", file=sys.stderr)
            return 3
        stub = esp.run_stub()
        digest = str(stub.flash_md5sum(addr, size)).strip().lower()
    finally:
        try:
            esp._port.close()
        except Exception:
            pass
    if len(digest) != 32 or any(c not in "0123456789abcdef" for c in digest):
        print("SPI_FLASH_MD5 ERROR no digest", file=sys.stderr)
        return 4
    print("SPI_FLASH_MD5 " + digest)
    return 0


sys.exit(main())
"#;

/// The argument vector that runs the helper on `port` for `[offset, offset + size)`. The
/// interpreter is `command.program`, so the helper imports the esptool the plan was checked
/// against.
pub fn helper_invocation(
    command: &EsptoolCommand,
    script: &str,
    port: &str,
    offset: u32,
    size: u32,
) -> Invocation {
    Invocation {
        program: command.program.clone(),
        args: vec![
            "-I".to_owned(),
            script.to_owned(),
            "--port".to_owned(),
            port.to_owned(),
            "--addr".to_owned(),
            format!("{offset:#x}"),
            "--size".to_owned(),
            format!("{size:#x}"),
        ],
    }
}

/// Whether `invocation` is a helper run (`-I` then a path named [`HELPER_NAME`]), so the caller
/// knows which gate to apply.
pub fn is_helper_invocation(invocation: &Invocation) -> bool {
    invocation.args.first().is_some_and(|w| w == "-I")
        && invocation.args.get(1).is_some_and(|script| {
            script
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|name| name.ends_with(HELPER_NAME))
        })
}

/// The last gate before the helper is spawned, the counterpart of
/// [`crate::rehearse::check_invocation`]:
///
/// - the program is the resolved Python interpreter (`command.prefix` starts with `-I -m esptool`);
/// - the vector is exactly `-I <script> --port <port> --addr <hex> --size <hex>`;
/// - `<script>` is a file this session wrote, of exactly [`HELPER_SOURCE`]'s length (`file_len`
///   answers only for the session's own scratch directory);
/// - address and length are strict `0x` hex, the length is non-zero, and the region stays inside
///   the 8 MB part. cardid is allowed here only, because the operation is a read-only digest.
pub fn check_md5_invocation(
    command: &EsptoolCommand,
    invocation: &Invocation,
    file_len: &dyn Fn(&str) -> Option<u64>,
) -> Result<(), Refusal> {
    let refuse = |why: String| {
        Err(Refusal::new(
            Rule::EsptoolNotAllowed,
            format!("region-MD5 helper argument vector refused: {why}"),
        ))
    };
    if invocation.program != command.program {
        return refuse("the program is not the resolved esptool".to_owned());
    }
    if command.prefix.first().map(String::as_str) != Some("-I")
        || command.prefix.get(1..) != Some(&["-m".to_owned(), "esptool".to_owned()])
    {
        return refuse("the resolved esptool is not a Python entry point".to_owned());
    }
    let args = &invocation.args;
    if args.len() != 8 {
        return refuse("the vector is not `-I <helper> --port P --addr A --size N`".to_owned());
    }
    if args[0] != "-I" || args[2] != "--port" || args[4] != "--addr" || args[6] != "--size" {
        return refuse("the vector is not `-I <helper> --port P --addr A --size N`".to_owned());
    }
    if !is_helper_invocation(invocation) {
        return refuse(format!("`{}` is not the region-MD5 helper", args[1]));
    }
    if file_len(&args[1]) != Some(HELPER_SOURCE.len() as u64) {
        return refuse("the helper is not a file this session wrote".to_owned());
    }
    if args[3].is_empty()
        || args[3].starts_with('-')
        || !args[3].bytes().all(|b| b.is_ascii_graphic())
    {
        return refuse("the port".to_owned());
    }
    let (Some(offset), Some(size)) = (parse_hex(&args[5]), parse_hex(&args[7])) else {
        return refuse("the address or the length is not `0x` hex".to_owned());
    };
    if size == 0 {
        return refuse("an empty region".to_owned());
    }
    if u64::from(offset) + u64::from(size) > u64::from(FLASH_SIZE) {
        return refuse(format!("[{offset:#x}, {size:#x}) passes 8 MB"));
    }
    Ok(())
}

/// A strict `0x` hex `u32`.
fn parse_hex(word: &str) -> Option<u32> {
    let digits = word.strip_prefix("0x")?;
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// Reads the digest from the [`DIGEST_PREFIX`] line (exactly 32 hex characters); anything else
/// is a failure, never a zero digest.
pub fn parse_digest(output: &str) -> Result<[u8; 16], SessionError> {
    let hex = output
        .lines()
        .find_map(|line| line.trim().strip_prefix(DIGEST_PREFIX))
        .map(str::trim)
        .ok_or_else(|| {
            SessionError::Failed("the region-MD5 helper printed no digest".to_owned())
        })?;
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SessionError::Failed(
            "the region-MD5 helper printed no digest".to_owned(),
        ));
    }
    let mut digest = [0u8; 16];
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let text = core::str::from_utf8(pair).map_err(|_| {
            SessionError::Failed("the region-MD5 helper printed no digest".to_owned())
        })?;
        *byte = u8::from_str_radix(text, 16).map_err(|_| {
            SessionError::Failed("the region-MD5 helper printed no digest".to_owned())
        })?;
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python() -> EsptoolCommand {
        EsptoolCommand {
            program: "/env/idf/bin/python".to_owned(),
            prefix: vec!["-I".to_owned(), "-m".to_owned(), "esptool".to_owned()],
            major: 4,
        }
    }

    const SCRATCH: &str = "/scratch/1-region-md5.py";

    fn len_of(path: &str) -> Option<u64> {
        (path == SCRATCH).then_some(HELPER_SOURCE.len() as u64)
    }

    #[test]
    fn the_cardid_digest_vector_is_allowed() {
        let invocation = helper_invocation(&python(), SCRATCH, "/dev/cu.x", 0x35_6000, 0x4000);
        assert_eq!(
            invocation.args,
            [
                "-I",
                SCRATCH,
                "--port",
                "/dev/cu.x",
                "--addr",
                "0x356000",
                "--size",
                "0x4000"
            ]
        );
        check_md5_invocation(&python(), &invocation, &len_of).expect("the guard's own region");
    }

    #[test]
    fn the_gate_refuses_every_other_shape() {
        let ok = helper_invocation(&python(), SCRATCH, "/dev/cu.x", 0x35_6000, 0x4000);

        let mut other = ok.clone();
        other.program = "/usr/bin/python3".to_owned();
        assert!(check_md5_invocation(&python(), &other, &len_of).is_err());

        let executable = EsptoolCommand {
            program: "/env/idf/bin/esptool".to_owned(),
            prefix: Vec::new(),
            major: 5,
        };
        let mut v5 = ok.clone();
        v5.program = executable.program.clone();
        assert!(check_md5_invocation(&executable, &v5, &len_of).is_err());

        let elsewhere = helper_invocation(&python(), "/tmp/region-md5.py", "/dev/cu.x", 0, 0x1000);
        assert!(check_md5_invocation(&python(), &elsewhere, &len_of).is_err());

        let other_script = helper_invocation(&python(), "/scratch/1-evil.py", "/dev/cu.x", 0, 0x10);
        assert!(check_md5_invocation(&python(), &other_script, &len_of).is_err());

        let past_end = helper_invocation(&python(), SCRATCH, "/dev/cu.x", 0x7F_F000, 0x2000);
        assert!(check_md5_invocation(&python(), &past_end, &len_of).is_err());

        let empty = helper_invocation(&python(), SCRATCH, "/dev/cu.x", 0x1000, 0);
        assert!(check_md5_invocation(&python(), &empty, &len_of).is_err());

        let mut decimal = ok.clone();
        decimal.args[5] = "3497984".to_owned();
        assert!(check_md5_invocation(&python(), &decimal, &len_of).is_err());

        let mut extra = ok.clone();
        extra.args.push("--baud".to_owned());
        assert!(check_md5_invocation(&python(), &extra, &len_of).is_err());

        let mut no_port = ok;
        no_port.args[3] = "--after".to_owned();
        assert!(check_md5_invocation(&python(), &no_port, &len_of).is_err());
    }

    /// A zero digest would make a changed cardid look unchanged.
    #[test]
    fn a_digest_is_read_only_from_a_complete_line() {
        let transcript = "esptool.py v4.12.0\nChip is ESP32-C3 (QFN32) (revision v1.1)\n\
             Uploading stub...\nRunning stub...\nStub running...\n\
             SPI_FLASH_MD5 0123456789abcdef0123456789ABCDEF\n";
        assert_eq!(
            parse_digest(transcript).expect("the digest line"),
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
        for bad in [
            "Stub running...\n",
            "SPI_FLASH_MD5 0123456789abcdef\n",
            "SPI_FLASH_MD5 ERROR no digest\n",
            "SPI_FLASH_MD5 0123456789abcdef0123456789abcdeg\n",
        ] {
            assert!(parse_digest(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_helper_script_only_takes_a_digest() {
        assert!(HELPER_SOURCE.contains("flash_md5sum"));
        assert!(HELPER_SOURCE.contains("detect_chip"));
        for forbidden in [
            "erase",
            "write_flash",
            "read_flash",
            "{}",
            "%s",
            "os.system",
            "subprocess",
        ] {
            assert!(!HELPER_SOURCE.contains(forbidden), "{forbidden}");
        }
    }
}
