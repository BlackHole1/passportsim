//! esptool resolution, the argument-vector gate, the rehearsal through the same invocation and the
//! emulator port allow list. A fake esptool runs in memory: no process, no port, no file.

mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use pemu_loader::esp_image::EspImage;
use pemu_loader::{hex, sha256};
use pemu_planner::flow::{FlowReport, SessionError, Step, rehearse};
use pemu_planner::plan::{ImageSource, Origin, PlanRequest, encode_partition_table, plan_flash};
use pemu_planner::rehearse::{
    BootConsole, EsptoolCommand, EsptoolSession, EsptoolSources, FileProbe, HostOs, Invocation,
    ProcessOutput, ProcessRunner, RegionMd5, ResolveError, check_invocation, check_version,
    parse_chip, parse_flash_id, resolve_esptool,
};
use pemu_planner::rules::{CARDID_OFFSET, PortUse, Rule, check_emulator_port};
use support::*;

struct Files(BTreeSet<String>);

impl FileProbe for Files {
    fn is_file(&self, path: &str) -> bool {
        self.0.contains(path)
    }
}

fn files(list: &[&str]) -> Files {
    Files(list.iter().map(|s| s.to_string()).collect())
}

#[test]
fn resolution_follows_the_fixed_order() {
    let probe = files(&[
        "/opt/esptool",
        "/env/idf/bin/python",
        "/tools/python_env/idf5.5_py3.10_env/bin/python",
    ]);
    let mut sources = EsptoolSources {
        explicit: Some("/opt/esptool".to_owned()),
        idf_python_env: Some("/env/idf".to_owned()),
        idf_tools_envs: vec!["/tools/python_env/idf5.5_py3.10_env".to_owned()],
    };
    let first = resolve_esptool(&sources, HostOs::MacOs, &probe).expect("explicit");
    assert_eq!(
        (first.program.as_str(), first.prefix.len()),
        ("/opt/esptool", 0)
    );
    sources.explicit = None;
    let second = resolve_esptool(&sources, HostOs::MacOs, &probe).expect("env");
    assert_eq!(second.program, "/env/idf/bin/python");
    assert_eq!(second.prefix, ["-I", "-m", "esptool"]);
    sources.idf_python_env = None;
    let third = resolve_esptool(&sources, HostOs::MacOs, &probe).expect("tools");
    assert_eq!(
        third.program,
        "/tools/python_env/idf5.5_py3.10_env/bin/python"
    );
    sources.idf_tools_envs.clear();
    assert!(matches!(
        resolve_esptool(&sources, HostOs::MacOs, &probe),
        Err(ResolveError::NotFound(_))
    ));
}

#[test]
fn a_windows_environment_uses_scripts_python_exe() {
    let probe = files(&[r"C:\Espressif\python_env\idf5.5_py3.11_env\Scripts\python.exe"]);
    let sources = EsptoolSources {
        idf_python_env: Some(r"C:\Espressif\python_env\idf5.5_py3.11_env".to_owned()),
        ..EsptoolSources::default()
    };
    let cmd = resolve_esptool(&sources, HostOs::Windows, &probe).expect("found");
    assert!(cmd.program.ends_with(r"\Scripts\python.exe"));
    assert_eq!(cmd.prefix, ["-I", "-m", "esptool"]);
}

#[test]
fn rule_esptool_script() {
    for script in ["esptool.bat", r"C:\x\ESPTOOL.CMD", "/x/esptool.ps1"] {
        let sources = EsptoolSources {
            explicit: Some(script.to_owned()),
            ..EsptoolSources::default()
        };
        match resolve_esptool(&sources, HostOs::Windows, &files(&[script])) {
            Err(ResolveError::Refused(r)) => assert_eq!(r.rule, Rule::EsptoolScript),
            other => panic!("{script}: {other:?}"),
        }
    }
}

#[test]
fn version_must_be_at_least_4_12() {
    assert_eq!(check_version("esptool.py v4.12.0"), Ok((4, 12)));
    assert_eq!(check_version("esptool v5.1.0"), Ok((5, 1)));
    assert!(check_version("esptool.py v4.7.0").is_err());
    assert!(check_version("no version here").is_err());
}

#[test]
fn rule_emulator_port() {
    assert!(check_emulator_port("rfc2217://127.0.0.1:5555", PortUse::Flash).is_ok());
    assert!(check_emulator_port("socket://127.0.0.1:5555", PortUse::Monitor).is_ok());
    for (url, usage) in [
        ("socket://127.0.0.1:5555", PortUse::Flash),
        ("rfc2217://localhost:5555", PortUse::Flash),
        ("rfc2217://192.168.1.2:5555", PortUse::Flash),
        ("rfc2217://127.0.0.1:0", PortUse::Flash),
        ("rfc2217://127.0.0.1:99999", PortUse::Flash),
        ("rfc2217://127.0.0.1:", PortUse::Flash),
        ("loop://", PortUse::Monitor),
        ("COM3", PortUse::Monitor),
    ] {
        let refusal = check_emulator_port(url, usage).expect_err(url);
        assert_eq!(refusal.rule, Rule::EmulatorPort, "{url}");
    }
}

const PY: &str = "/env/idf/bin/python";

fn python_command() -> EsptoolCommand {
    EsptoolCommand {
        program: PY.to_owned(),
        prefix: vec!["-m".to_owned(), "esptool".to_owned()],
        major: 4,
    }
}

fn argv(tail: &[&str]) -> Invocation {
    let mut args: Vec<String> = [
        "-m",
        "esptool",
        "--chip",
        "esp32c3",
        "--port",
        "rfc2217://127.0.0.1:4242",
        "--before",
        "usb_reset",
        "--after",
        "no_reset",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(tail.iter().map(|s| s.to_string()));
    Invocation {
        program: PY.to_owned(),
        args,
    }
}

fn raw(program: &str, args: &[&str]) -> Invocation {
    Invocation {
        program: program.to_owned(),
        args: args.iter().map(|s| s.to_string()).collect(),
    }
}

fn unknown_len(_: &str) -> Option<u64> {
    None
}

/// A `file_len` that answers one sector for every file operand, as a runner answers for its own
/// scratch files.
fn scratch_len(_: &str) -> Option<u64> {
    Some(0x1000)
}

#[test]
fn only_allowed_esptool_subcommands_and_options_pass() {
    let py = python_command();
    for ok in [
        argv(&["chip_id"]),
        argv(&["flash_id"]),
        argv(&["read_flash", "0x8000", "0xc00", "scratch/t.bin"]),
        argv(&[
            "write_flash",
            "--flash_size",
            "keep",
            "0x10000",
            "scratch/a.bin",
        ]),
        argv(&[
            "verify_flash",
            "0x0",
            "scratch/b.bin",
            "0x10000",
            "scratch/a.bin",
        ]),
        argv(&["erase_region", "0x9000", "0x6000"]),
        raw(PY, &["-m", "esptool", "version"]),
    ] {
        assert!(
            check_invocation(&py, &ok, &scratch_len).is_ok(),
            "{:?}",
            ok.args
        );
    }
    // The same vectors with a file operand this run does not own are refused.
    for unknown in [
        argv(&["read_flash", "0x8000", "0xc00", "/tmp/elsewhere.bin"]),
        argv(&[
            "write_flash",
            "--flash_size",
            "keep",
            "0x10000",
            "/tmp/elsewhere.bin",
        ]),
        argv(&["verify_flash", "0x10000", "/tmp/elsewhere.bin"]),
    ] {
        let refusal = check_invocation(&py, &unknown, &unknown_len)
            .expect_err(&format!("{:?}", unknown.args));
        assert_eq!(refusal.rule, Rule::EsptoolNotAllowed);
    }
    let exe = EsptoolCommand {
        program: "/opt/esptool".to_owned(),
        prefix: Vec::new(),
        major: 5,
    };
    let dashed = raw(
        "/opt/esptool",
        &[
            "--chip",
            "esp32c3",
            "--port",
            "p",
            "--before",
            "usb-reset",
            "--after",
            "no-reset",
            "write-flash",
            "--flash-size",
            "keep",
            "0x0",
            "b.bin",
        ],
    );
    assert!(check_invocation(&exe, &dashed, &scratch_len).is_ok());
    let refused = [
        argv(&["write_mem", "0x60008000", "0x1", "0xffffffff"]),
        argv(&["load_ram", "x.bin"]),
        argv(&["run"]),
        argv(&["erase_flash"]),
        argv(&["write_flash", "--encrypt", "0x0", "a.bin"]),
        argv(&["write_flash", "0x0", "a.bin"]),
        argv(&["write_flash", "--flash_size", "8MB", "0x0", "a.bin"]),
        argv(&[
            "write_flash",
            "--flash_size",
            "keep",
            "--erase-all",
            "0x0",
            "a.bin",
        ]),
        argv(&["merge_bin", "-o", "x.bin"]),
        argv(&["chip_id", "extra"]),
        argv(&["read_flash", "0x0", "0x800000"]),
        raw(PY, &["-m", "esptool", "--no-stub", "chip_id"]),
        raw(PY, &["-m", "espefuse", "summary"]),
        raw("/env/idf/bin/espefuse.py", &["summary"]),
        raw(PY, &["-m", "esptool", "--chip", "esp32c3", "chip_id"]),
    ];
    for bad in refused {
        let refusal =
            check_invocation(&py, &bad, &unknown_len).expect_err(&format!("{:?}", bad.args));
        assert_eq!(refusal.rule, Rule::EsptoolNotAllowed);
    }
}

#[test]
fn esptool_ranges_programs_and_values_are_enforced() {
    let py = python_command();
    let base = [
        "-m",
        "esptool",
        "--chip",
        "esp32c3",
        "--port",
        "/dev/cu.x",
        "--before",
        "usb_reset",
        "--after",
        "no_reset",
    ];
    let with = |tail: &[&str]| raw(PY, &[&base[..], tail].concat());
    let refused = [
        (
            "port with space",
            raw(
                PY,
                &[&base[..5], &["x --erase-all"], &base[6..], &["chip_id"]].concat(),
            ),
        ),
        (
            "unicode dash port",
            raw(
                PY,
                &[&base[..5], &["\u{2014}erase-all"], &base[6..], &["chip_id"]].concat(),
            ),
        ),
        (
            "write into cardid",
            with(&["write_flash", "--flash_size", "keep", "0x356000", "f"]),
        ),
        (
            "write sector of cardid",
            with(&["write_flash", "--flash_size", "keep", "0x359fff", "f"]),
        ),
        (
            "read the part with one byte less",
            with(&["read_flash", "0x0", "0x7fffff", "f"]),
        ),
        (
            "read from the sector below cardid into it",
            with(&["read_flash", "0x355000", "0x2000", "f"]),
        ),
        (
            "read beyond 8 MB",
            with(&["read_flash", "0x7ff000", "0x2000", "f"]),
        ),
        (
            "erase cardid",
            with(&["erase_region", "0x356000", "0x4000"]),
        ),
        (
            "erase ends in cardid sector",
            with(&["erase_region", "0x350000", "0x6001"]),
        ),
        (
            "uppercase 0X hex",
            with(&["erase_region", "0X9000", "0x6000"]),
        ),
        (
            "huge hex",
            with(&["erase_region", "0x0", "0xFFFFFFFFFFFFFFFFFFFF"]),
        ),
        (
            "nine hex digits",
            with(&["erase_region", "0x000009000", "0x6000"]),
        ),
        ("zero length", with(&["erase_region", "0x9000", "0x0"])),
        ("-m esptool.__main__", {
            let mut v = with(&["chip_id"]);
            v.args[1] = "esptool.__main__".to_owned();
            v
        }),
        ("no -m, python -c", raw(PY, &["-c", "import os"])),
        (
            "v5 dashed without the prefix",
            raw(
                PY,
                &[
                    "--chip",
                    "esp32c3",
                    "--port",
                    "p",
                    "--before",
                    "usb-reset",
                    "--after",
                    "no-reset",
                    "write-flash",
                    "--flash-size",
                    "keep",
                    "0x0",
                    "f",
                ],
            ),
        ),
        (
            "program /bin/sh",
            raw("/bin/sh", &[&base[..], &["chip_id"]].concat()),
        ),
        (
            "another python",
            raw("/usr/bin/python3", &[&base[..], &["chip_id"]].concat()),
        ),
    ];
    for (tag, bad) in refused {
        let refusal = check_invocation(&py, &bad, &unknown_len).expect_err(tag);
        assert_eq!(refusal.rule, Rule::EsptoolNotAllowed, "{tag}");
    }
    // A write whose file runs into cardid is refused once the length is known.
    let long = with(&[
        "write_flash",
        "--flash_size",
        "keep",
        "0x350000",
        "scratch/a.bin",
    ]);
    // An unknown length is refused outright, not range-checked on the offset's own sector.
    assert!(check_invocation(&py, &long, &unknown_len).is_err());
    let len = |path: &str| (path == "scratch/a.bin").then_some(0x6001);
    assert!(check_invocation(&py, &long, &len).is_err());
    let fits = |path: &str| (path == "scratch/a.bin").then_some(0x6000);
    assert!(check_invocation(&py, &long, &fits).is_ok());
    let verify = with(&["verify_flash", "0x350000", "scratch/a.bin"]);
    assert!(check_invocation(&py, &verify, &len).is_err());
    for ok in [
        with(&["erase_region", "0x9000", "0x6000"]),
        with(&["read_flash", "0x8000", "0xc00", "f"]),
        with(&["read_flash", "0x35a000", "0x4a6000", "f"]),
        with(&["erase_region", "0x310000", "0x46000"]),
        with(&["write_flash", "--flash_size", "keep", "0x10000", "f"]),
    ] {
        assert!(
            check_invocation(&py, &ok, &scratch_len).is_ok(),
            "{:?}",
            ok.args
        );
    }
}

#[test]
fn chip_and_flash_id_output_parse() {
    let chip = "Detecting chip type... ESP32-C3\nChip is ESP32-C3 (QFN32) (revision v1.1)\n";
    let (name, rev) = parse_chip(chip).expect("parsed");
    assert_eq!((name.as_str(), rev.major, rev.minor), ("ESP32-C3", 1, 1));
    assert_eq!(
        parse_flash_id("Manufacturer: 20\nDevice: 4017\nDetected flash size: 8MB\n"),
        Some((0x20, 0x4017))
    );
    assert_eq!(parse_chip("Chip is ESP32-S3"), None);
}

/// The emulated flash a fake esptool writes to, shared with the region MD5 and console sources.
#[derive(Default)]
struct Emu {
    flash: Vec<u8>,
    files: std::collections::BTreeMap<String, Vec<u8>>,
}

type Shared = Rc<RefCell<Emu>>;

struct FakeEsptool(Shared);

fn hex_arg(s: &str) -> usize {
    usize::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex argument")
}

impl ProcessRunner for FakeEsptool {
    fn run(&mut self, inv: &Invocation) -> Result<ProcessOutput, String> {
        let mut emu = self.0.borrow_mut();
        // The region-MD5 helper, answered with a real stub transcript.
        if pemu_planner::stub_md5::is_helper_invocation(inv) {
            assert_eq!(
                emu.files.get(&inv.args[1]).map(Vec::len),
                Some(pemu_planner::stub_md5::HELPER_SOURCE.len())
            );
            let (o, n) = (hex_arg(&inv.args[5]), hex_arg(&inv.args[7]));
            let digest = pemu_planner::md5::digest(&emu.flash[o..o + n]);
            return Ok(ProcessOutput {
                success: true,
                output: format!(
                    "esptool.py v4.12.0\nChip is ESP32-C3 (QFN32) (revision v1.1)\nUploading stub...\nRunning stub...\nStub running...\nSPI_FLASH_MD5 {}\n",
                    hex(&digest)
                ),
            });
        }
        let after = inv
            .args
            .iter()
            .position(|a| a == "--after")
            .expect("--after")
            + 2;
        let sub = &inv.args[after..];
        let ok = |output: &str| {
            Ok(ProcessOutput {
                success: true,
                output: output.to_owned(),
            })
        };
        match sub[0].as_str() {
            "chip_id" => ok("Chip is ESP32-C3 (QFN32) (revision v1.1)\nMAC: 02:00:00:00:00:01\n"),
            "flash_id" => ok("Manufacturer: 20\nDevice: 4017\n"),
            "read_flash" => {
                let (o, n) = (hex_arg(&sub[1]), hex_arg(&sub[2]));
                let bytes = emu.flash[o..o + n].to_vec();
                emu.files.insert(sub[3].clone(), bytes);
                ok("")
            }
            "write_flash" => {
                assert_eq!(&sub[1..3], ["--flash_size", "keep"]);
                for pair in sub[3..].chunks(2) {
                    let o = hex_arg(&pair[0]);
                    let data = emu.files[&pair[1]].clone();
                    let s = o / 0x1000 * 0x1000;
                    let e = (o + data.len()).div_ceil(0x1000) * 0x1000;
                    emu.flash[s..e].fill(0xFF);
                    emu.flash[o..o + data.len()].copy_from_slice(&data);
                }
                ok("")
            }
            "verify_flash" => {
                let good = sub[1..].chunks(2).all(|pair| {
                    let o = hex_arg(&pair[0]);
                    let data = &emu.files[&pair[1]];
                    emu.flash[o..o + data.len()] == data[..]
                });
                Ok(ProcessOutput {
                    success: good,
                    output: String::new(),
                })
            }
            other => panic!("unexpected esptool subcommand {other}"),
        }
    }
    fn scratch_path(&mut self, name: &str) -> String {
        format!("scratch/{name}")
    }
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.0
            .borrow_mut()
            .files
            .insert(path.to_owned(), bytes.to_vec());
        Ok(())
    }
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        self.0
            .borrow()
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| "missing".to_owned())
    }
    fn remove_file(&mut self, path: &str) {
        self.0.borrow_mut().files.remove(path);
    }
}

struct EmuMd5(Shared);

impl RegionMd5 for EmuMd5 {
    fn region_md5(&mut self, offset: u32, size: u32) -> Result<[u8; 16], SessionError> {
        let emu = self.0.borrow();
        let d = sha256(&emu.flash[offset as usize..(offset + size) as usize]);
        Ok(d[..16].try_into().expect("16"))
    }
}

struct EmuConsole(Shared);

impl BootConsole for EmuConsole {
    fn boot_log(&mut self) -> Result<String, SessionError> {
        let emu = self.0.borrow();
        let app = &emu.flash[0x1_0000..];
        let desc = EspImage::parse(app)
            .ok()
            .and_then(|i| i.app_desc(app).ok().flatten())
            .ok_or_else(|| SessionError::Failed("no app".to_owned()))?;
        Ok(format!(
            "rst:0x15 (USB_UART_CHIP_RESET),boot:0xa (SPI_FAST_FLASH_BOOT)\nI (117) app_init: ELF file SHA256:  {}...\n",
            &hex(&desc.app_elf_sha256)[..9]
        ))
    }
}

/// A runner that answers like [`FakeEsptool`] until it is asked to write, and then fails the way
/// a stub that fell behind esptool's timeout does.
struct WriteFails(FakeEsptool);

impl ProcessRunner for WriteFails {
    fn run(&mut self, inv: &Invocation) -> Result<ProcessOutput, String> {
        if inv
            .args
            .iter()
            .any(|a| a.replace('-', "_") == "write_flash")
        {
            return Ok(ProcessOutput {
                success: false,
                output: "Connecting...\nWriting at 0x00010000... (3 %)\n\n\
                         A fatal error occurred: The chip stopped responding."
                    .to_owned(),
            });
        }
        self.0.run(inv)
    }

    fn scratch_path(&mut self, name: &str) -> String {
        self.0.scratch_path(name)
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.0.write_file(path, bytes)
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        self.0.read_file(path)
    }

    fn remove_file(&mut self, path: &str) {
        self.0.remove_file(path);
    }
}

/// Asserts on the profile this test is compiled in, so neither half can be lost.
#[test]
fn a_failed_rehearsal_names_the_debug_build_and_a_release_build_does_not() {
    let shared: Shared = Rc::default();
    {
        let mut emu = shared.borrow_mut();
        emu.flash = vec![0xFF; 0x80_0000];
        let table = encode_partition_table(&official_layout());
        emu.flash[0x8000..0x8000 + table.len()].copy_from_slice(&table);
    }
    let image = official_padded();
    let request = PlanRequest::write(ImageSource::Merged(&image), Origin::Tool);
    let plan = plan_flash(&request, None)
        .accepted()
        .expect("accepted")
        .clone();

    let command = EsptoolCommand {
        program: "/env/idf/bin/python".to_owned(),
        prefix: vec!["-m".to_owned(), "esptool".to_owned()],
        major: 4,
    };
    let (mut runner, mut md5, mut console) = (
        WriteFails(FakeEsptool(shared.clone())),
        EmuMd5(shared.clone()),
        EmuConsole(shared.clone()),
    );
    let mut session = EsptoolSession::emulator(
        command,
        "rfc2217://127.0.0.1:4242",
        &mut runner,
        &mut md5,
        &mut console,
    )
    .expect("allowed port");
    let mut report = FlowReport::default();
    let why = format!(
        "{:?}",
        rehearse(&request, &plan, &mut session, &mut report).expect_err("the write failed")
    );

    assert!(why.contains("The chip stopped responding"), "{why}");
    if cfg!(debug_assertions) {
        assert!(why.contains("This is a debug build"), "{why}");
        assert!(why.contains("--release"), "{why}");
    } else {
        assert!(
            !why.contains("debug build"),
            "a release build blames nothing: {why}"
        );
    }
}

#[test]
fn a_rehearsal_through_the_esptool_session_keeps_cardid_bit_identical() {
    let shared: Shared = Rc::default();
    {
        let mut emu = shared.borrow_mut();
        emu.flash = vec![0xFF; 0x80_0000];
        let table = encode_partition_table(&official_layout());
        emu.flash[0x8000..0x8000 + table.len()].copy_from_slice(&table);
        for (i, b) in emu.flash[CARDID_OFFSET as usize..CARDID_OFFSET as usize + 0x4000]
            .iter_mut()
            .enumerate()
        {
            *b = (i % 251) as u8;
        }
    }
    let cardid_before = shared.borrow().flash[0x35_6000..0x35_A000].to_vec();
    let image = official_padded();
    let request = PlanRequest::write(ImageSource::Merged(&image), Origin::Tool);
    let outcome = plan_flash(&request, None);
    let plan = outcome.accepted().expect("accepted").clone();

    let command = EsptoolCommand {
        program: "/env/idf/bin/python".to_owned(),
        prefix: vec!["-m".to_owned(), "esptool".to_owned()],
        major: 4,
    };
    let (mut runner, mut md5, mut console) = (
        FakeEsptool(shared.clone()),
        EmuMd5(shared.clone()),
        EmuConsole(shared.clone()),
    );
    let mut session = EsptoolSession::emulator(
        command.clone(),
        "rfc2217://127.0.0.1:4242",
        &mut runner,
        &mut md5,
        &mut console,
    )
    .expect("allowed port");
    let mut report = FlowReport::default();
    let result = rehearse(&request, &plan, &mut session, &mut report);
    assert_eq!(result, Ok(()));
    assert!(report.device.steps.contains(&(Step::Rehearse, true)));
    assert_eq!(report.rehearsal.cardid_unchanged, Some(true));
    let ran = session.ran.clone();
    drop(session);
    assert_eq!(
        &shared.borrow().flash[0x35_6000..0x35_A000],
        &cardid_before[..]
    );
    assert_eq!(
        &shared.borrow().flash[0x1_0000..0x2_0000],
        &image[0x1_0000..0x2_0000]
    );
    for inv in &ran {
        assert_eq!(inv.program, command.program);
        assert_eq!(
            &inv.args[..10],
            [
                "-m",
                "esptool",
                "--chip",
                "esp32c3",
                "--port",
                "rfc2217://127.0.0.1:4242",
                "--baud",
                "921600",
                "--before",
                "usb_reset"
            ]
        );
        assert!(check_invocation(&command, inv, &scratch_len).is_ok());
    }
    let subs: Vec<&str> = ran.iter().map(|i| i.args[12].as_str()).collect();
    assert_eq!(
        subs,
        [
            "chip_id",
            "flash_id",
            "read_flash",
            "write_flash",
            "verify_flash",
            "chip_id"
        ]
    );
    assert_eq!(ran.last().expect("reset").args[11], "hard_reset");
    assert!(
        shared.borrow().files.is_empty(),
        "scratch files are removed"
    );
}

#[test]
fn an_emulator_session_refuses_a_device_or_socket_port() {
    let shared: Shared = Rc::default();
    let command = EsptoolCommand {
        program: "esptool".to_owned(),
        prefix: Vec::new(),
        major: 5,
    };
    for url in [
        "socket://127.0.0.1:4242",
        "fake-cu-1",
        "rfc2217://10.0.0.1:1",
    ] {
        let (mut runner, mut md5, mut console) = (
            FakeEsptool(shared.clone()),
            EmuMd5(shared.clone()),
            EmuConsole(shared.clone()),
        );
        let refused =
            EsptoolSession::emulator(command.clone(), url, &mut runner, &mut md5, &mut console);
        assert_eq!(
            refused.err().map(|r| r.rule),
            Some(Rule::EmulatorPort),
            "{url}"
        );
    }
}

#[test]
fn the_mac_is_parsed_only_to_be_hashed() {
    use pemu_planner::rehearse::parse_mac;
    assert_eq!(
        parse_mac("Chip is ESP32-C3\nMAC: 02:00:00:00:00:01\n"),
        Some([2, 0, 0, 0, 0, 1])
    );
    assert_eq!(parse_mac("MAC: 02:00:00"), None);
    assert_eq!(parse_mac("no mac"), None);
}

#[test]
fn flash_id_reads_anchored_lines_only() {
    let v4 = "esptool.py v4.12.0\nSerial port rfc2217://127.0.0.1:4000\nConnecting...\n\
              Device PID identification is only supported on COM and /dev/ serial ports.\n\
              Chip is ESP32-C3 (QFN32) (revision v1.1)\nManufacturer: 20\nDevice: 4017\n";
    assert_eq!(parse_flash_id(v4), Some((0x20, 0x4017)));
    let (chip, rev) = parse_chip(v4).expect("chip");
    assert_eq!((chip.as_str(), rev.major, rev.minor), ("ESP32-C3", 1, 1));
    assert_eq!(
        parse_flash_id("Device PID identification\nManufacturer: 20\n"),
        None
    );
    assert_eq!(parse_flash_id("Manufacturer: 20\nDevice: 40zz\n"), None);
    assert_eq!(
        parse_flash_id("Manufacturer: 20\nDevice: 4017\nDevice: 4016\n"),
        None
    );
}

struct VersionRunner(&'static str, Vec<Invocation>);

impl ProcessRunner for VersionRunner {
    fn run(&mut self, inv: &Invocation) -> Result<ProcessOutput, String> {
        self.1.push(inv.clone());
        Ok(ProcessOutput {
            success: true,
            output: self.0.to_owned(),
        })
    }
    fn scratch_path(&mut self, name: &str) -> String {
        name.to_owned()
    }
    fn write_file(&mut self, _: &str, _: &[u8]) -> Result<(), String> {
        Ok(())
    }
    fn read_file(&mut self, _: &str) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn remove_file(&mut self, _: &str) {}
}

#[test]
fn resolution_runs_esptool_version_and_takes_the_spelling_from_it() {
    use pemu_planner::rehearse::resolve_and_check_esptool;
    let probe = files(&["/env/idf/bin/python"]);
    let sources = EsptoolSources {
        idf_python_env: Some("/env/idf".to_owned()),
        ..EsptoolSources::default()
    };
    let mut old = VersionRunner("esptool.py v4.7.0", Vec::new());
    assert!(matches!(
        resolve_and_check_esptool(&sources, HostOs::MacOs, &probe, &mut old),
        Err(ResolveError::Version(_))
    ));
    assert_eq!(old.1[0].args, ["-I", "-m", "esptool", "version"]);

    let shared: Shared = Rc::default();
    for (printed, major, sub, flash_size, before) in [
        (
            "esptool.py v4.12.0",
            4,
            "chip_id",
            "--flash_size",
            "usb_reset",
        ),
        ("esptool v5.1.0", 5, "chip-id", "--flash-size", "usb-reset"),
    ] {
        let mut runner = VersionRunner(printed, Vec::new());
        let command =
            resolve_and_check_esptool(&sources, HostOs::MacOs, &probe, &mut runner).expect(printed);
        assert_eq!(command.major, major);
        let (mut md5, mut console) = (EmuMd5(shared.clone()), EmuConsole(shared.clone()));
        let mut session = EsptoolSession::emulator(
            command,
            "rfc2217://127.0.0.1:4242",
            &mut runner,
            &mut md5,
            &mut console,
        )
        .expect("port");
        let _ = pemu_planner::flow::DeviceSession::hard_reset(&mut session);
        let plan_write = pemu_planner::plan::plan_flash(
            &PlanRequest::write(ImageSource::Merged(&official_unpadded()), Origin::Tool),
            None,
        );
        let _ = pemu_planner::flow::DeviceSession::write(&mut session, &plan_write.plan.writes);
        let ran = session.ran.clone();
        assert_eq!(ran[0].args[10], before, "{printed}");
        assert_eq!(ran[0].args[13], sub, "{printed}");
        assert_eq!(ran[1].args[14], flash_size, "{printed}");
    }
}

/// The `--backup` exception cannot be widened into a cardid read by rounding an offset or a
/// length.
#[test]
fn only_the_whole_part_may_be_read_over_cardid() {
    let py = EsptoolCommand {
        program: "/env/idf/bin/python".to_owned(),
        prefix: vec!["-I".to_owned(), "-m".to_owned(), "esptool".to_owned()],
        major: 4,
    };
    let unknown_len = |_: &str| None;
    let vector = |sub: &[&str]| Invocation {
        program: py.program.clone(),
        args: [
            "-I",
            "-m",
            "esptool",
            "--chip",
            "esp32c3",
            "--port",
            "p",
            "--before",
            "usb_reset",
            "--after",
            "no_reset",
        ]
        .iter()
        .chain(sub.iter())
        .map(|s| (*s).to_owned())
        .collect(),
    };
    check_invocation(
        &py,
        &vector(&["read_flash", "0x0", "0x800000", "full.bin"]),
        &scratch_len,
    )
    .expect("the whole part is the `--backup` read");
    assert!(
        check_invocation(
            &py,
            &vector(&["read_flash", "0x0", "0x800000", "/tmp/full.bin"]),
            &unknown_len,
        )
        .is_err()
    );
    for refused in [
        vec!["read_flash", "0x0", "0x800001", "f"],
        vec!["read_flash", "0x1000", "0x7ff000", "f"],
        vec!["read_flash", "0x356000", "0x4000", "f"],
        vec!["read_flash", "0x0", "0x400000", "f"],
        // The exception is for reads only: the whole part is never erased.
        vec!["erase_region", "0x0", "0x800000"],
    ] {
        let error = check_invocation(&py, &vector(&refused), &scratch_len);
        assert!(error.is_err(), "{refused:?}");
    }
    // Nor written: a whole-part file at 0x0 covers cardid and is refused once its length is known.
    let whole_file = |path: &str| (path == "f").then_some(0x80_0000);
    assert!(
        check_invocation(
            &py,
            &vector(&["write_flash", "--flash_size", "keep", "0x0", "f"]),
            &whole_file,
        )
        .is_err()
    );
}

#[test]
fn every_vector_names_the_usb_jtag_reset_and_default_reset_is_refused() {
    let command = python_command();
    for port in ["rfc2217://127.0.0.1:4242", "COM3", "/dev/cu.usbmodem1101"] {
        let inv = command.invocation(port, "no_reset", &["chip_id".to_owned()]);
        let at = inv
            .args
            .iter()
            .position(|a| a == "--before")
            .expect("--before");
        assert_eq!(inv.args[at + 1], "usb_reset", "{port}");
    }
    let old: Vec<String> = argv(&["chip_id"])
        .args
        .into_iter()
        .map(|a| {
            if a == "usb_reset" {
                "default_reset".to_owned()
            } else {
                a
            }
        })
        .collect();
    let refused = check_invocation(
        &command,
        &Invocation {
            program: PY.to_owned(),
            args: old,
        },
        &|_| None,
    );
    assert!(refused.is_err(), "default_reset is refused");
}
