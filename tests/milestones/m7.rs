//! Milestone M7 tests: the agent commands (`snapshot`, `env`, `scenario`, the fault envelopes)
//! and the daemon behind them. Names use the prefix `t<tier>_m7_` so `xtask ci` can count them.
//!
//! Each test drives the real `passportsim` binary against a private daemon (`Home`) on a corpus
//! image or a probe, and compares with an in-process machine or `riscv32-esp-elf-gdb`.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::{passportsim, workspace};

/// The snapshot-fork (10 s) and boot-cache (50 ms) wall-clock bounds are release figures for an
/// idle host. Every test holds this lock shared for its body ([`busy`]); a measured section holds
/// it exclusively ([`quiet`]), so it runs while no other M7 test in this process is doing work.
static WALL_CLOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn busy() -> std::sync::RwLockReadGuard<'static, ()> {
    WALL_CLOCK
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The exclusive hold of [`WALL_CLOCK`]. The caller drops its own [`busy`] guard first.
fn quiet() -> std::sync::RwLockWriteGuard<'static, ()> {
    WALL_CLOCK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Host budget every CLI `run` of this file carries ([`Home::json`]), instead of the 30 s default,
/// which fails on a loaded host although no M7 test asserts it. [`CLI_DEADLINE`] stays above it,
/// so an overrun is still the envelope.
const HOST_BUDGET_MS: &str = "900000";

/// Wall time one `passportsim` child gets before [`wait_by`] kills it: nothing else bounds these
/// calls (`cargo test` and `xtask ci` have no per-test timeout).
const CLI_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1200);

/// Waits for `child`, reading both pipes on their own threads, and kills it once `deadline` has
/// passed. `Err` carries what was collected before the kill.
fn try_wait_by(
    mut child: std::process::Child,
    deadline: std::time::Duration,
) -> Result<(i32, String, String), (String, String)> {
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut s);
        s
    });
    let started = std::time::Instant::now();
    let mut killed = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child is waitable") {
            break status;
        }
        let waited = started.elapsed();
        if !killed && waited > deadline {
            killed = true;
            let _ = child.kill();
        }
        // The boot-cache test measures a 50 ms call through this wait, so the first tenth of a second is
        // polled finely.
        std::thread::sleep(if waited < std::time::Duration::from_millis(100) {
            std::time::Duration::from_micros(200)
        } else {
            std::time::Duration::from_millis(5)
        });
    };
    // The deadline bounds the process, not this join: a reader ends when the last writer closes its
    // pipe. The daemon `serve` spawns outlives its parent, but `spawn_detached` gives it null handles
    // instead of these pipes; a command leaving a grandchild on them would turn a kill into a hang.
    let (out, err) = (
        out.join().expect("stdout reader"),
        err.join().expect("stderr reader"),
    );
    if killed {
        // A killed child's pipes can end mid-character; this only becomes a panic message.
        return Err((
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ));
    }
    Ok((
        status.code().unwrap_or(-1),
        String::from_utf8(out).expect("UTF-8 stdout"),
        String::from_utf8(err).expect("UTF-8 stderr"),
    ))
}

#[track_caller]
fn wait_by(
    child: std::process::Child,
    args: &[&str],
    deadline: std::time::Duration,
) -> (i32, String, String) {
    try_wait_by(child, deadline).unwrap_or_else(|(stdout, stderr)| {
        panic!(
            "passportsim {args:?} did not finish within {} s and was killed: {stdout}{stderr}",
            deadline.as_secs()
        )
    })
}

/// A redacted `snapshot export` carries no secret, and its import boots marked `redacted`.
///
/// The file decodes with no eFuse hash or word, every cardid-window and `nvs` page 0xFF, and
/// `cargo xtask secrets-check --paths` passes; an `--include-secrets` export is the control.
/// `official` writes no NVS page before its menu and a daemon refuses an image with real secrets
/// (`refuse_secrets`), so the erasure of real content is asserted over a synthetic guest
/// ([`guest_written_legs`]).
#[test]
fn t1_m7_snapshot_export_is_redacted() {
    let test = "t1_m7_snapshot_export_is_redacted";
    let _busy = busy();
    guest_written_legs(test);
    let Some((entry, _, _)) = official_entry(test) else {
        return;
    };
    let home = Home::new("export", std::slice::from_ref(&entry));
    let first = home.json(&["start", common::OFFICIAL], 0);
    assert_eq!(first["boot"]["status"], "matched", "{first}");
    let first_id = first["instance"].as_str().expect("an id").to_owned();
    let export = home.json(
        &["snapshot", "export", "export", "--instance", &first_id],
        0,
    );
    assert_eq!(export["redacted"], true, "{export}");
    let path = export["path"].as_str().expect("a path").to_owned();
    let file = find_file(&home.artifacts(), &path).expect("the export is on disk");
    let bytes = std::fs::read(&file).expect("the export reads");
    let snapshot = pemu_core::snap::Snapshot::from_bytes(&bytes).expect("the export decodes");
    assert!(
        snapshot.header.exported && snapshot.header.redacted,
        "{:?}",
        snapshot.header
    );
    assert_eq!(
        snapshot.header.efuse_hash, [0; 32],
        "no eFuse hash in the header"
    );
    let efuse = efuse_words(&snapshot);
    assert!(
        efuse.iter().all(|w| *w == 0),
        "{test}: the export carries no eFuse word"
    );
    let delta = flash_delta(&snapshot);
    for (what, range) in [
        ("cardid window", CARDID_WINDOW),
        ("nvs partition", OFFICIAL_NVS),
    ] {
        for page in range.start / 0x1000..range.end / 0x1000 {
            let bytes = delta
                .get(&page)
                .unwrap_or_else(|| panic!("{test}: the {what} page {page:#x} is carried erased"));
            assert!(
                bytes.iter().all(|b| *b == 0xFF),
                "{test}: the {what} page {page:#x} leaves as 0xFF"
            );
        }
    }
    let check =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["run", "-q", "-p", "xtask", "--", "secrets-check", "--paths"])
            .arg(&file)
            .current_dir(workspace())
            .env_remove("PASSPORTSIM_DATA_ROOT")
            .output()
            .expect("xtask runs");
    assert!(
        check.status.success(),
        "{test}: `xtask secrets-check` passes on the export: {}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    println!("RAN {test} export-redacted-and-secrets-check-passes");

    // The control: without redaction the export carries what the redacted one dropped.
    let raw = home.json(
        &[
            "snapshot",
            "export",
            "export-raw",
            "--instance",
            &first_id,
            "--include-secrets",
            "--confirm",
            "export",
        ],
        0,
    );
    assert_eq!(raw["redacted"], false, "{raw}");
    let raw_bytes = std::fs::read(
        find_file(&home.artifacts(), raw["path"].as_str().expect("a path")).expect("on disk"),
    )
    .expect("reads");
    let raw_snapshot = pemu_core::snap::Snapshot::from_bytes(&raw_bytes).expect("decodes");
    assert!(
        efuse_words(&raw_snapshot).iter().any(|w| *w != 0),
        "{test}: an unredacted export carries the synthesized eFuse words"
    );
    let raw_delta = flash_delta(&raw_snapshot);
    let written_nvs = (OFFICIAL_NVS.start / 0x1000..OFFICIAL_NVS.end / 0x1000)
        .filter(|page| {
            raw_delta
                .get(page)
                .is_some_and(|b| b.iter().any(|x| *x != 0xFF))
        })
        .count();
    println!(
        "RAN {test} control: the unredacted export carries eFuse words and {written_nvs} written NVS page(s)"
    );

    let second = home.json(&["start", common::OFFICIAL, "--boot", "none"], 0);
    let second_id = second["instance"].as_str().expect("an id").to_owned();
    let second_dir =
        find_dir(&home.artifacts(), &second_id).expect("the second instance's artifacts");
    std::fs::create_dir_all(second_dir.join("snapshots")).expect("a snapshots directory");
    std::fs::copy(&file, second_dir.join("snapshots/export.snap"))
        .expect("the export is handed over");
    let imported = home.json(
        &["snapshot", "import", "export", "--instance", &second_id],
        0,
    );
    assert_eq!(imported["redacted"], true, "{imported}");
    let restored = home.json(
        &["snapshot", "restore", "export", "--instance", &second_id],
        0,
    );
    assert_eq!(restored["receipt"]["redacted"], true, "{restored}");
    let ran = home.json(&["run", "--for", "1s", "--instance", &second_id], 0);
    assert_eq!(
        ran["receipt"]["redacted"], true,
        "{test}: the booted import is marked redacted: {ran}"
    );
    assert_eq!(
        ran["vt_us"].as_u64(),
        restored["vt_us"].as_u64().map(|vt| vt + 1_000_000),
        "{test}: the imported state runs on: {ran}"
    );
    println!("RAN {test} import-boots-redacted");
    let _ = home.json(&["stop", &first_id], 0);
    let _ = home.json(&["stop", &second_id], 0);
}

/// The value the synthetic guest of [`GUEST_PROGRAM`] writes into flash: never a device's.
const GUEST_VALUE: &[u8] = b"tok-SYNTH-0001";

const GUEST_LOAD: u32 = 0x403C_E000;

/// A hand-assembled RV32 guest, as in `pemu_api::commands::snapshot::tests`. It programs
/// [`GUEST_VALUE`] (with its NUL and one 0xFF, 16 bytes) at 0x9080 over SPI1 (WREN, `W0..W3`, PP,
/// then RDSR until WIP clears), reads it back with a legacy FLASH_READ, copies the four words into
/// RAM, the HMAC message registers and the USB Serial/JTAG EP1 staging FIFO, and spins.
const GUEST_PROGRAM: &[u32] = &[
    // lui t0, SPI1 (0x6000_2000)
    0x600022B7, // CMD = FLASH_WREN
    0x40000337, 0x0062A023, // W0 = value word 0
    0x2D6B7337, 0xF7430313, 0x0462AC23, // W1 = value word 1
    0x544E6337, 0x95330313, 0x0462AE23, // W2 = value word 2
    0x30303337, 0xD4830313, 0x0662A023, // W3 = value word 3
    0xFF003337, 0x13030313, 0x0662A223, // ADDR = 0x9080 | 16 << 24
    0x10009337, 0x08030313, 0x0062A223, // CMD = FLASH_PP
    0x02000337, 0x0062A023, // poll: CMD = FLASH_RDSR; while RD_STATUS & WIP
    0x08000337, 0x0062A023, 0x02C2A303, 0x00137313, 0xFE0318E3, // W0..W3 = 0
    0x0402AC23, 0x0402AE23, 0x0602A023, 0x0602A223, // ADDR = 0x9080 | 16 << 24
    0x10009337, 0x08030313, 0x0062A223, // CMD = FLASH_READ
    0x80000337, 0x0062A023, // a1..a4 = W0..W3
    0x0582A583, 0x05C2A603, 0x0602A683, 0x0642A703, // RAM 0x3FC9_0000 = a1..a4
    0x3FC903B7, 0x00B3A023, 0x00C3A223, 0x00D3A423, 0x00E3A623,
    // HMAC message words = a1..a4
    0x6003EE37, 0x08BE2023, 0x08CE2223, 0x08DE2423, 0x08EE2623,
    // t4 = USJ EP1, t5 = 14, t6 = RAM
    0x60043EB7, 0x00E00F13, 0x00038F93, // loop: EP1 = *t6++ while --t5
    0x000FC403, 0x008EA023, 0x001F8F93, 0xFFFF0F13, 0xFE0F18E3, // j .
    0x0000006F,
];

/// A synthetic merged image for [`GUEST_PROGRAM`] with the flash program and read-back aimed at
/// `target`: a one-segment bootloader at 0, a partition table with one NVS partition at 0x9000,
/// and in it an active page declaring namespace `app` and a 15-byte string entry `label` whose
/// payload is still erased. The key is no credential marker, so a daemon starts the image.
fn guest_image_writing_at(target: u32) -> Vec<u8> {
    let li = |value: u32| {
        let lo = ((value & 0xFFF) ^ 0x800).wrapping_sub(0x800);
        let hi = value.wrapping_sub(lo) >> 12;
        [
            (hi << 12) | (6 << 7) | 0x37,
            ((lo & 0xFFF) << 20) | (6 << 15) | (6 << 7) | 0x13,
        ]
    };
    let mut program = GUEST_PROGRAM.to_vec();
    for at in [15, 29] {
        assert_eq!(
            program[at..at + 2],
            li(0x9080 | 16 << 24),
            "the ADDR pair moved"
        );
        program[at..at + 2].copy_from_slice(&li(target | 16 << 24));
    }
    let code: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut flash = vec![0xFFu8; 8 << 20];
    let mut boot = vec![0xE9, 1, 2, 0x3F];
    boot.extend_from_slice(&GUEST_LOAD.to_le_bytes());
    boot.extend_from_slice(&[0xEE, 0, 0, 0]);
    boot.extend_from_slice(&5u16.to_le_bytes());
    boot.push(0);
    boot.extend_from_slice(&0u16.to_le_bytes());
    boot.extend_from_slice(&0xFFFFu16.to_le_bytes());
    boot.extend_from_slice(&[0; 5]);
    boot.extend_from_slice(&GUEST_LOAD.to_le_bytes());
    boot.extend_from_slice(&(code.len() as u32).to_le_bytes());
    boot.extend_from_slice(&code);
    while boot.len() % 16 != 15 {
        boot.push(0);
    }
    boot.push(code.iter().fold(0xEF, |c, b| c ^ b));
    flash[..boot.len()].copy_from_slice(&boot);
    let mut row = vec![0xAA, 0x50, 0x01, 0x02];
    row.extend_from_slice(&0x9000u32.to_le_bytes());
    row.extend_from_slice(&0x4000u32.to_le_bytes());
    let mut label = [0u8; 16];
    label[..3].copy_from_slice(b"nvs");
    row.extend_from_slice(&label);
    row.extend_from_slice(&[0; 4]);
    flash[0x8000..0x8000 + row.len()].copy_from_slice(&row);
    let page = &mut flash[0x9000..0xA000];
    page[0..4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
    // Entries 0 to 2 written (0b10 each), the rest empty.
    page[32] = 0b1110_1010;
    let entry = |page: &mut [u8], i: usize, ns: u8, ty: u8, span: u8, key: &[u8]| {
        let at = 64 + 32 * i;
        page[at..at + 24].fill(0);
        page[at] = ns;
        page[at + 1] = ty;
        page[at + 2] = span;
        page[at + 8..at + 8 + key.len()].copy_from_slice(key);
    };
    entry(page, 0, 0, 0x01, 1, b"app");
    page[64 + 24..64 + 32].copy_from_slice(&[1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    entry(page, 1, 1, 0x21, 2, b"label");
    page[96 + 24..96 + 32].copy_from_slice(&[15, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    flash
}

const GUEST_NVS: std::ops::Range<u32> = 0x9000..0xD000;

/// The NVS and cardid legs over content a guest wrote (at 0x9080, and at 0x35_6080 inside the
/// cardid window): the unredacted export carries [`GUEST_VALUE`] in that page of `flash_delta`,
/// the redacted one carries the page erased and the value in no flash page at all.
fn guest_written_legs(test: &str) {
    let home = Home::new("export-guest", &[]);
    let images = home.home.join("images");
    std::fs::create_dir_all(&images).expect("an image directory");
    for (leg, target, window) in [
        ("nvs", 0x9080u32, GUEST_NVS),
        ("cardid", 0x35_6080, CARDID_WINDOW),
    ] {
        let image = images.join(format!("guest-{leg}.bin"));
        std::fs::write(&image, guest_image_writing_at(target)).expect("the image is written");
        let started = home.json(
            &[
                "start",
                image.to_str().expect("a UTF-8 path"),
                "--boot",
                "none",
            ],
            0,
        );
        let id = started["instance"].as_str().expect("an id").to_owned();
        home.json(&["run", "--for", "2s", "--instance", &id], 0);
        let page = target / 0x1000;
        let carried = |name: &str, extra: &[&str]| {
            let mut argv = vec!["snapshot", "export", name, "--instance", &id];
            argv.extend_from_slice(extra);
            let out = home.json(&argv, 0);
            let bytes = std::fs::read(
                find_file(&home.artifacts(), out["path"].as_str().expect("a path"))
                    .expect("the export is on disk"),
            )
            .expect("the export reads");
            let snapshot = pemu_core::snap::Snapshot::from_bytes(&bytes).expect("decodes");
            (out, flash_delta(&snapshot))
        };
        let holds = |bytes: &[u8]| bytes.windows(GUEST_VALUE.len()).any(|w| w == GUEST_VALUE);
        let plain_name = format!("export-{leg}-plain");
        let (plain, plain_delta) = carried(
            &plain_name,
            &["--include-secrets", "--confirm", &plain_name],
        );
        assert_eq!(plain["redacted"], false, "{plain}");
        assert!(
            plain_delta.get(&page).is_some_and(|bytes| holds(bytes)),
            "{test}: {leg}: the unredacted export carries what the guest wrote at {target:#x}, \
             so the redacted leg compares something"
        );
        let (redacted, delta) = carried(&format!("export-{leg}"), &[]);
        assert_eq!(redacted["redacted"], true, "{redacted}");
        for index in window.start / 0x1000..window.end / 0x1000 {
            assert!(
                delta
                    .get(&index)
                    .is_some_and(|bytes| bytes.iter().all(|b| *b == 0xFF)),
                "{test}: {leg}: page {index:#x} leaves erased"
            );
        }
        assert!(
            !delta.values().any(|bytes| holds(bytes)),
            "{test}: {leg}: the guest's value is in no flash page of the redacted export"
        );
        println!(
            "RAN {test} guest-written-{leg}: the plain export carries the value at {target:#x}, the redacted one erases it"
        );
        let _ = home.json(&["stop", &id], 0);
    }
}

/// The flash cardid window an export erases (`pemu_machine::snapshot::CARDID_WINDOW`).
const CARDID_WINDOW: std::ops::Range<u32> = 0x35_6000..0x35_A000;

/// The `nvs` partition of the `official` image, as its bootloader prints the table.
const OFFICIAL_NVS: std::ops::Range<u32> = 0x9000..0xF000;

fn flash_delta(snapshot: &pemu_core::snap::Snapshot) -> std::collections::BTreeMap<u32, Vec<u8>> {
    let id = pemu_core::snap::SectionId::new(pemu_core::snap::SectionId::FLASH_DELTA);
    let section = snapshot.section(&id).expect("a flash_delta section");
    let delta: pemu_soc_c3::flash_store::FlashDelta =
        pemu_core::snap::serde_from_section(section, id, section.version, "flash_delta")
            .expect("flash_delta decodes");
    delta.pages.into_iter().map(|p| (p.page, p.bytes)).collect()
}

fn efuse_words(snapshot: &pemu_core::snap::Snapshot) -> Vec<u32> {
    let id = pemu_core::snap::SectionId::soc("efuse");
    let section = snapshot.section(&id).expect("a soc.efuse section");
    let model: pemu_soc_c3::periph::efuse::Model =
        pemu_core::snap::serde_from_section(section, id, section.version, "soc.efuse")
            .expect("soc.efuse decodes");
    model.image().to_vec()
}

fn find_dir(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == name) {
                    return Some(path);
                }
                dirs.push(path);
            }
        }
    }
    None
}

/// Sixteen forks from the settled menu, each with its own clicks, in parallel.
///
/// Each fork is driven over the daemon's HTTP route by its own thread with the fork index in
/// binary (one bit a second, DOWN for 1, UP for 0), so every fork runs exactly 5 s of virtual
/// time. The parent's state hash is unchanged, the forks do not all end in one state, and a
/// release build finishes within 10 s of wall time (a debug build prints NOT_RUN).
#[test]
fn t1_m7_snapshot_fork_sixteen_ways() {
    let test = "t1_m7_snapshot_fork_sixteen_ways";
    let _busy = busy();
    let Some((entry, _, _)) = official_entry(test) else {
        return;
    };
    let home = Home::new("fork", std::slice::from_ref(&entry));
    // Seventeen instances exceed the default limit on a small host.
    home.serve_with_limit(17);
    let parent = home.json(&["start", common::OFFICIAL], 0);
    assert_eq!(
        parent["boot"]["status"], "matched",
        "the menu settles: {parent}"
    );
    let parent_id = parent["instance"].as_str().expect("an id").to_owned();
    let saved = home.json(&["snapshot", "save", "menu"], 0);
    let parent_hash = saved["state_hash"]
        .as_str()
        .expect("a state hash")
        .to_owned();
    let base_us = saved["vt_us"].as_u64().expect("the fork instant");
    let forked = home.json(&["snapshot", "fork", "menu", "--count", "16"], 0);
    let forks: Vec<String> = forked["instances"]
        .as_array()
        .expect("the fork ids")
        .iter()
        .map(|v| v.as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(forks.len(), 16, "{forked}");
    let discovery = home.discovery();
    let port = u16::try_from(discovery["port"].as_u64().expect("a port")).expect("a port");
    let token = discovery["token"].as_str().expect("a token").to_owned();

    drop(_busy);
    let quiet_hold = quiet();
    let started = std::time::Instant::now();
    let threads: Vec<_> = forks
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let (id, token) = (id.clone(), token.clone());
            std::thread::spawn(move || {
                let call = |name: &str, args: serde_json::Value| {
                    let headers = [
                        ("Host", format!("127.0.0.1:{port}")),
                        ("Authorization", format!("Bearer {token}")),
                        ("Content-Type", "application/json".to_owned()),
                    ];
                    let path = format!("/v1/instances/{id}/commands/{name}");
                    let (status, _, body) = http(port, "POST", &path, &headers, &args.to_string());
                    let body: serde_json::Value = serde_json::from_str(&body).expect("JSON");
                    assert_eq!(
                        (status, &body["ok"]),
                        (200, &serde_json::Value::Bool(true)),
                        "{id} {name}: {body}"
                    );
                    body["result"].clone()
                };
                let mut clicks = Vec::new();
                for second in 0..5u64 {
                    let button = if index >> second & 1 == 1 {
                        "down"
                    } else {
                        "up"
                    };
                    clicks.push(button);
                    call(
                        "input",
                        serde_json::json!({"button": button, "action": "click"}),
                    );
                    let until = format!("vt:{}ms", base_us / 1_000 + (second + 1) * 1_000);
                    call("run", serde_json::json!({"until": until, "timeout": "2s"}));
                }
                let end = call("snapshot", serde_json::json!({"op": "save", "name": "end"}));
                (clicks, end)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|t| t.join().expect("a fork's driver thread"))
        .collect();
    let wall = started.elapsed();
    drop(quiet_hold);
    let _busy = busy();

    let sequences: std::collections::BTreeSet<_> = results.iter().map(|(c, _)| c.clone()).collect();
    assert_eq!(
        sequences.len(),
        16,
        "{test}: every fork got its own click sequence"
    );
    for (_, end) in &results {
        assert_eq!(
            end["vt_us"].as_u64(),
            Some(base_us / 1_000 * 1_000 + 5_000_000),
            "{test}: every fork ran its 5 s script: {end}"
        );
    }
    let hashes: std::collections::BTreeSet<&str> = results
        .iter()
        .map(|(_, end)| end["state_hash"].as_str().expect("a hash"))
        .collect();
    assert!(
        hashes.len() > 1 && !hashes.contains(parent_hash.as_str()),
        "{test}: the forks moved away from the parent and apart: {hashes:?}"
    );
    let after = home.json(&["snapshot", "save", "after", "--instance", &parent_id], 0);
    assert_eq!(
        after["state_hash"].as_str(),
        Some(parent_hash.as_str()),
        "{test}: the parent digest is unchanged"
    );
    println!(
        "RAN {test} forks: 16 sequences, {} distinct end states, parent unchanged, wall {wall:?}",
        hashes.len()
    );
    if cfg!(debug_assertions) {
        println!(
            "NOT_RUN {test} wall-10s: a debug build measures the unoptimized core, and the \
             10 s budget is a native release figure (ran {wall:?}; run with --release)"
        );
    } else {
        assert!(
            wall <= std::time::Duration::from_secs(10),
            "{test}: sixteen 5 s scripts finish in at most 10 s wall natively, took {wall:?}"
        );
        println!("RAN {test} wall-10s: {wall:?}");
    }
    for id in forks.iter().chain(std::iter::once(&parent_id)) {
        let _ = home.json(&["stop", id], 0);
    }
}

fn official(test: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let bin = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")?;
    Some((
        std::fs::read(bin).expect("the verified corpus image is readable"),
        std::fs::read(elf).expect("the verified corpus ELF is readable"),
    ))
}

/// The last byte of the `official` NVS partition, which a fresh NVS leaves erased.
const NVS_VARIANT_BYTE: usize = 0x9000 + 0x6000 - 1;

fn start(args: serde_json::Value) -> (serde_json::Value, std::time::Duration) {
    let (output, elapsed) = start_output(args);
    (output.json, elapsed)
}

fn start_output(args: serde_json::Value) -> (pemu_api::output::Output, std::time::Duration) {
    let spec = pemu_api::registry::find("start").expect("start is registered");
    let started = std::time::Instant::now();
    let output = (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone())
        .unwrap_or_else(|e| panic!("start {args} refused: {e:?}"));
    (output, started.elapsed())
}

/// `screenshot raw` through the registry with an artifact directory bound as the daemon binds
/// one: `(frame_gen, path)`.
fn shot(json: &serde_json::Value, artifacts: &std::path::Path) -> (u64, String) {
    let instance = json["instance"].as_str().expect("an id");
    let dir = pemu_host::artifacts::ArtifactDir::create(artifacts, "run-boot-cache", instance)
        .expect("an artifact directory");
    let spec = pemu_api::registry::find("screenshot").expect("screenshot is registered");
    let output = pemu_host::artifacts::bind_current(
        Some(std::sync::Arc::new(std::sync::Mutex::new(dir))),
        || {
            (spec.handler)(
                &mut pemu_api::spec::HandlerCx {},
                serde_json::json!({"instance": instance, "view": "raw"}),
            )
        },
    )
    .unwrap_or_else(|e| panic!("screenshot refused: {e:?}"));
    (
        output.json["frame_gen"].as_u64().expect("a frame_gen"),
        output.json["path"].as_str().expect("a path").to_owned(),
    )
}

fn hash_and_stop(json: &serde_json::Value) -> ([u8; 32], u64) {
    let id = pemu_api::instance::InstanceId::parse(json["instance"].as_str().expect("an id"))
        .expect("a minted id");
    pemu_api::commands::start::with_pool(|pool| {
        let session = pool.session_mut(id).expect("the started session");
        let out = (
            session.snapshot_machine().state_hash(),
            session.receipt().vt_us,
        );
        pool.destroy(id).expect("stops");
        out
    })
}

/// How many warm starts are timed; the 50 ms bound applies to the fastest.
const WARM_HITS: usize = 5;

/// The boot cache hits on a second start and misses on a changed seed or U-state.
///
/// A cold start fills the cache, a second start restores it with the cold boot's state hash, and
/// a variant whose NVS differs by one byte and one with another initial USB world both miss. The
/// key's image part erases NVS (`image_sha_without_nvs`), so the NVS variant differs in the
/// key's NVS seed part alone.
#[test]
fn t1_m7_boot_cache_hits_and_misses() {
    use std::sync::Arc;
    let test = "t1_m7_boot_cache_hits_and_misses";
    let _busy = busy();
    let Some((image, elf)) = official(test) else {
        return;
    };
    let mut variant = image.clone();
    variant[NVS_VARIANT_BYTE] ^= 0x01;
    let (image, variant) = (Arc::new(image), Arc::new(variant));
    let cache_role =
        std::env::temp_dir().join(format!("pemu-m7-boot-cache-{}", std::process::id()));
    std::fs::create_dir_all(&cache_role).expect("a temp cache role");
    let context = Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses"));
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            "official" => pemu_host::backend::merged_image(&image),
            "official-nvs" => pemu_host::backend::merged_image(&variant),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(cache_role.join("audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| fw.starts_with("official").then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::default(),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(Some(cache_role.clone()));
    let args = |fw: &str| serde_json::json!({"fw": fw, "boot_cache": "ui-settled"});

    let (cold, cold_wall) = start(args("official"));
    let cache = &cold["boot"]["cache"];
    assert_eq!(
        cold["boot"]["status"], "matched",
        "the cold boot settles: {cold}"
    );
    assert_eq!(
        (cache["hit"].as_bool(), cache["store"].as_str()),
        (Some(false), Some("disk"))
    );
    let key = cache["key"].as_str().expect("a key").to_owned();
    let entry = cache_role.join(pemu_host::boot_cache::CACHE_DIR).join(&key);
    assert!(
        entry.is_file(),
        "an untainted entry is on disk under its key"
    );
    let cold_shot = shot(&cold, &cache_role.join("artifacts"));
    let (cold_hash, cold_vt) = hash_and_stop(&cold);
    println!("RAN {test} cold: vt {cold_vt} us, wall {cold_wall:?}");

    // Checked against the fastest warm start: one start that lost the core is noise, every start
    // missing the bound is the finding.
    drop(_busy);
    let quiet_hold = quiet();
    let (warm, mut warm_wall) = start(args("official"));
    assert_eq!(warm["boot"]["cache"]["hit"], true, "{warm}");
    assert_eq!(warm["boot"]["cache"]["key"], key.as_str());
    let warm_shot = shot(&warm, &cache_role.join("artifacts-warm"));
    let (warm_hash, warm_vt) = hash_and_stop(&warm);
    assert!(cold_shot.0 > 0, "the settled menu was drawn: {cold_shot:?}");
    assert_eq!(
        warm_shot, cold_shot,
        "a restored instance's screenshot has the cold boot's frame_gen \
         and artifact path (snapshot format 3 `frame` section)"
    );
    assert_eq!(
        warm_vt, cold_vt,
        "the restored instance sits at the cold boot's instant"
    );
    assert_eq!(
        warm_hash, cold_hash,
        "the state hash equals the cold boot's"
    );
    let mut warm_walls = vec![warm_wall];
    for _ in 1..WARM_HITS {
        let (again, wall) = start(args("official"));
        assert_eq!(again["boot"]["cache"]["hit"], true, "{again}");
        assert_eq!(hash_and_stop(&again).0, cold_hash);
        warm_walls.push(wall);
        warm_wall = warm_wall.min(wall);
    }
    drop(quiet_hold);
    let _busy = busy();
    println!("RAN {test} warm: {WARM_HITS} hits, fastest {warm_wall:?} of {warm_walls:?}");
    if cfg!(debug_assertions) {
        println!(
            "NOT_RUN {test} wall-50ms: a debug build measures the unoptimized restore, and PLAN \
             the 50 ms restore is a release figure (ran {warm_wall:?}; run with --release)"
        );
    } else {
        assert!(
            warm_wall <= std::time::Duration::from_millis(50),
            "a cache hit reaches the settled menu in at most 50 ms, the fastest of \
             {WARM_HITS} took {warm_wall:?} ({warm_walls:?})"
        );
        println!("RAN {test} wall-50ms: {warm_wall:?}");
    }

    // A damaged entry is a miss whose cold boot replaces it, not a permanent miss.
    std::fs::write(&entry, b"not a snapshot").expect("the entry is writable by its owner");
    let (damaged, _) = start(args("official"));
    assert_eq!(
        (
            damaged["boot"]["cache"]["hit"].as_bool(),
            damaged["boot"]["cache"]["key"].as_str()
        ),
        (Some(false), Some(key.as_str())),
        "{damaged}"
    );
    assert_eq!(hash_and_stop(&damaged).0, cold_hash);
    let (repaired, _) = start(args("official"));
    assert_eq!(
        repaired["boot"]["cache"]["hit"], true,
        "the replaced entry restores: {repaired}"
    );
    assert_eq!(hash_and_stop(&repaired).0, cold_hash);
    println!("RAN {test} damaged-entry-replaced");

    let (plain, plain_wall) = start(serde_json::json!({"fw": "official"}));
    assert_eq!(plain["boot"]["status"], "matched", "{plain}");
    assert!(plain["boot"].get("cache").is_none(), "{plain}");
    assert_eq!(
        hash_and_stop(&plain),
        (cold_hash, cold_vt),
        "`start` and `start --boot-cache` land at the same instant and state hash"
    );
    println!("RAN {test} plain-start-settles: wall {plain_wall:?}");

    let (nvs, _) = start(args("official-nvs"));
    assert_eq!(
        nvs["boot"]["cache"]["hit"], false,
        "a different NVS seed misses: {nvs}"
    );
    assert_ne!(nvs["boot"]["cache"]["key"], key.as_str());
    hash_and_stop(&nvs);
    println!("RAN {test} nvs-seed-only-variant-misses");

    let mut unplugged = args("official");
    unplugged["usb"] = "unplugged".into();
    let (usb, _) = start(unplugged);
    assert_eq!(
        usb["boot"]["cache"]["hit"], false,
        "another USB U-state misses: {usb}"
    );
    assert_ne!(usb["boot"]["cache"]["key"], key.as_str());
    hash_and_stop(&usb);

    // The tainted leg: on an eFuse dump the instance is tainted in its receipt, the boot cache gives
    // it the memory store, and the cache role gains no file for its key.
    //
    // The dump is `EfuseImage::synth`'s own words: taint comes from the image's origin, not a word's
    // value, so no device dump is read. This process stands in for the native CLI with
    // `allow_tainted_loads` and takes the allowance back afterwards.
    let dump = cache_role.join("efuse-dump");
    std::fs::create_dir_all(&dump).expect("a dump directory");
    let synth = pemu_loader::efuse_image::EfuseImage::synth(11);
    for (block, &words) in pemu_loader::efuse_image::BLOCK_WORDS.iter().enumerate() {
        let bytes: Vec<u8> = synth.words()[block][..words]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        std::fs::write(dump.join(format!("efuse_blk{block}.bin")), bytes).expect("a block file");
    }
    let mut tainted_args = args("official");
    tainted_args["efuse_dump"] = dump.to_string_lossy().into_owned().into();
    tainted_args["confirm"] = "the person running this test".into();
    pemu_api::commands::start::allow_tainted_loads();
    let (tainted, tainted_wall) = start_output(tainted_args);
    pemu_api::commands::start::refuse_tainted_loads();
    assert!(
        tainted.receipt.tainted,
        "the receipt of a machine on an eFuse dump says tainted: {}",
        tainted.text
    );
    let tainted_cache = &tainted.json["boot"]["cache"];
    assert_eq!(
        (
            tainted_cache["store"].as_str(),
            tainted_cache["hit"].as_bool()
        ),
        (Some("memory"), Some(false)),
        "a tainted machine's boot-cache entries stay in memory only: {}",
        tainted.json
    );
    let tainted_key = tainted_cache["key"].as_str().expect("a key");
    assert_ne!(tainted_key, key, "another eFuse is another key");
    assert!(
        !cache_role
            .join(pemu_host::boot_cache::CACHE_DIR)
            .join(tainted_key)
            .exists(),
        "a tainted machine's entry reached the cache role on disk"
    );
    hash_and_stop(&tainted.json);
    println!("RAN {test} tainted-in-memory: wall {tainted_wall:?}");
    std::fs::remove_dir_all(&cache_role).ok();
}

/// A guest panic comes back as `E_GUEST_PANIC` with symbolized frames equal to gdb's.
///
/// `probe_panic` reads address 0 through two `noinline` functions; the run must end with the
/// envelope naming the load access fault, the task and the frames. An in-process machine built as
/// the daemon builds it reaches the same instant, its data RAM and registers are written as an
/// ELF core, and `gdb -batch -ex bt` must name the same functions, files and lines.
#[test]
fn t1_m7_panic_envelope() {
    let test = "t1_m7_panic_envelope";
    let _busy = busy();
    let exit = test.to_string();
    let Some(entry) = probe_entry(test, &exit, "probe_panic") else {
        return;
    };
    let Some(gdb) = riscv_gdb() else {
        common::skip(
            test,
            &format!(
                "{exit} riscv32-esp-elf-gdb is not installed (set PEMU_RISCV_GDB or install the \
                 ESP-IDF gdb under ~/.espressif/tools), and the exit compares with its `bt`"
            ),
        );
        return;
    };
    let home = Home::new("panic", std::slice::from_ref(&entry));
    let start = home.json(&["start", "probe_panic", "--boot", "none"], 0);
    let id = start["instance"].as_str().expect("an id").to_owned();
    // Exit 4 is a guest fault.
    let out = home.json(&["run", "serial:/AFTER/", "--timeout", "5s"], 4);
    let error = &out;
    assert_eq!(error["code"], "E_GUEST_PANIC", "{out}");
    let detail = &error["detail"];
    assert_eq!(detail["fault"], "panic", "{detail}");
    assert_eq!(detail["reason"], "load access fault", "{detail}");
    assert_eq!(detail["mcause"], "0x00000005", "{detail}");
    assert_eq!(
        detail["mtval"], "0x00000000",
        "the NULL read faults at 0: {detail}"
    );
    assert_eq!(detail["task"], "panic_task", "{detail}");
    let frames = error["backtrace"].as_array().expect("a backtrace").clone();
    let names: Vec<&str> = frames
        .iter()
        .map(|f| f["function"].as_str().unwrap_or("??"))
        .collect();
    assert_eq!(
        names[..3],
        ["probe_panic_read_null", "probe_panic_outer", "panic_task"],
        "{error}"
    );
    assert!(
        error["serial_tail"].as_array().is_some_and(|tail| tail
            .iter()
            .any(|l| l.as_str().is_some_and(|l| l.starts_with("ARMED|")))),
        "the serial tail carries the probe's last line: {error}"
    );
    let vt_us = error["vt_us"].as_u64().expect("the fault instant");
    println!(
        "RAN {test} cli-envelope: {} frames at vt {vt_us} us",
        frames.len()
    );

    let registers = &detail["registers"];
    let reg = |name: &str| {
        u32::from_str_radix(
            registers[name]
                .as_str()
                .and_then(|t| t.strip_prefix("0x"))
                .unwrap_or_else(|| panic!("register {name}: {detail}")),
            16,
        )
        .expect("hex")
    };
    let elf_path = entry.elf.clone().expect("the probe ELF");
    let elf_bytes = std::fs::read(&elf_path).expect("the probe ELF reads");
    let elf = pemu_loader::elf::ElfInfo::parse(&elf_bytes).expect("the probe ELF parses");
    let flash = pemu_host::backend::merged_image(&std::fs::read(&entry.bin).expect("image"))
        .expect("a merged image");
    let mut twin = pemu_host::backend::build_machine_with_elf(
        flash,
        Some(std::sync::Arc::new(elf)),
        1,
        pemu_machine::config::TimingProfileId::Fast,
    )
    .expect("the machine composes");
    let stop = loop {
        let out = twin.run(pemu_machine::run::RunLimits {
            until: Some(pemu_core::time::VTime::from_ms(5_000)),
            max_insns: None,
            stops: pemu_machine::stops::StopSet::default(),
        });
        match out.reason {
            pemu_machine::stops::StopReason::GuestPanic(_) => break out,
            pemu_machine::stops::StopReason::Until => panic!("{test}: the twin never panicked"),
            _ => {}
        }
    };
    assert_eq!(
        stop.vt.as_us(),
        vt_us,
        "{test}: the in-process machine stops at the daemon's instant"
    );
    let core = std::env::temp_dir().join(format!("pemu-m7-panic-{}.core", std::process::id()));
    let mut dram = Vec::with_capacity(DRAM_LEN as usize);
    {
        let mem = twin.guest_mem();
        for addr in (DRAM_BASE..DRAM_BASE + DRAM_LEN).step_by(4) {
            dram.extend_from_slice(&mem.load(addr, 4).unwrap_or(0).to_le_bytes());
        }
    }
    let mut regs = [0u32; 32];
    regs[0] = reg("pc");
    regs[1] = reg("ra");
    regs[2] = reg("sp");
    regs[8] = reg("s0");
    std::fs::write(&core, elf_core(&regs, DRAM_BASE, &dram)).expect("the core is written");
    let bt = gdb_bt(&gdb, &elf_path, &core);
    std::fs::remove_file(&core).ok();
    assert!(!bt.is_empty(), "{test}: gdb printed no frame");
    let ours: Vec<(String, String, u64)> = frames
        .iter()
        .take(bt.len())
        .map(|f| {
            (
                f["function"].as_str().unwrap_or("??").to_owned(),
                f["source"].as_str().unwrap_or("").to_owned(),
                f["line"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(
        ours, bt,
        "{test}: the envelope's frames equal `riscv32-esp-elf-gdb bt` at the same pc"
    );
    println!("RAN {test} gdb-bt: {} frames equal", bt.len());
    let _ = home.json(&["stop", &id], 0);
}

/// SRAM1's data view, which holds every task stack of an IDF app (IDF
/// `soc/esp32c3/include/soc/soc.h`): the core's memory.
const DRAM_BASE: u32 = 0x3FC8_0000;
const DRAM_LEN: u32 = 0x6_0000;

/// `riscv32-esp-elf-gdb`: `PEMU_RISCV_GDB`, else the newest ESP-IDF tool install under
/// `$IDF_TOOLS_PATH` or `~/.espressif`. Run on an ELF and a core file only.
fn riscv_gdb() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("PEMU_RISCV_GDB") {
        return Some(path.into()).filter(|p: &std::path::PathBuf| p.is_file());
    }
    let tools = std::env::var_os("IDF_TOOLS_PATH")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".espressif"))
        })?
        .join("tools")
        .join("riscv32-esp-elf-gdb");
    let mut versions: Vec<std::path::PathBuf> = std::fs::read_dir(tools)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|v| v.join("riscv32-esp-elf-gdb/bin/riscv32-esp-elf-gdb"))
        .find(|p| p.is_file())
}

/// An ELF32 RISC-V core file: one `NT_PRSTATUS` note carrying `regs` (pc, then x1 to x31, the
/// Linux `elf_gregset_t` order) and one loadable segment of `mem` at `base`.
fn elf_core(regs: &[u32; 32], base: u32, mem: &[u8]) -> Vec<u8> {
    fn pad4(bytes: &mut Vec<u8>) {
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
    }
    // `struct elf_prstatus` for a 32-bit target: 72 bytes before `pr_reg`, 32 words of it, then
    // `pr_fpvalid`. `pr_cursig` (offset 12) is SIGSEGV and `pr_pid` (offset 24) is 1.
    let mut prstatus = vec![0u8; 204];
    prstatus[12..14].copy_from_slice(&11u16.to_le_bytes());
    prstatus[24..28].copy_from_slice(&1u32.to_le_bytes());
    for (i, value) in regs.iter().enumerate() {
        prstatus[72 + 4 * i..76 + 4 * i].copy_from_slice(&value.to_le_bytes());
    }
    let mut note = Vec::new();
    note.extend_from_slice(&5u32.to_le_bytes());
    note.extend_from_slice(&(prstatus.len() as u32).to_le_bytes());
    note.extend_from_slice(&1u32.to_le_bytes());
    note.extend_from_slice(b"CORE\0");
    pad4(&mut note);
    note.extend_from_slice(&prstatus);
    pad4(&mut note);
    let (ehsize, phentsize, phnum) = (52u32, 32u32, 2u32);
    let notes_at = ehsize + phentsize * phnum;
    let load_at = notes_at + note.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    for half in [4u16, 243] {
        out.extend_from_slice(&half.to_le_bytes()); // ET_CORE, EM_RISCV
    }
    for word in [1u32, 0, ehsize, 0, 0] {
        out.extend_from_slice(&word.to_le_bytes()); // version, entry, phoff, shoff, flags
    }
    for half in [ehsize as u16, phentsize as u16, phnum as u16, 0, 0, 0] {
        out.extend_from_slice(&half.to_le_bytes());
    }
    let mem_len = mem.len() as u32;
    for header in [
        [4u32, notes_at, 0, 0, note.len() as u32, 0, 0, 0],
        [1u32, load_at, base, 0, mem_len, mem_len, 6, 0],
    ] {
        for word in header {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    out.extend_from_slice(&note);
    out.extend_from_slice(mem);
    out
}

/// `bt` over `elf` and `core`: `(function, file as DWARF spells it, line)` per frame.
fn gdb_bt(
    gdb: &std::path::Path,
    elf: &std::path::Path,
    core: &std::path::Path,
) -> Vec<(String, String, u64)> {
    let out = std::process::Command::new(gdb)
        .args([
            "-nx",
            "-batch",
            "-ex",
            "set pagination off",
            "-ex",
            "echo BT-BEGIN\\n",
            "-ex",
            "bt",
        ])
        .arg(elf)
        .arg(core)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("gdb runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // Loading a core prints its innermost frame once before `bt` runs; only `bt`'s count.
    let listing = text.split_once("BT-BEGIN").map_or("", |(_, bt)| bt);
    parse_bt(listing)
}

fn parse_bt(text: &str) -> Vec<(String, String, u64)> {
    text.lines()
        .filter(|line| line.starts_with('#'))
        .filter_map(|line| {
            let rest = line.split_once(char::is_whitespace)?.1.trim_start();
            let rest = match rest.split_once(" in ") {
                Some((addr, tail)) if addr.starts_with("0x") => tail,
                _ => rest,
            };
            let function = rest.split_once(" (")?.0.to_owned();
            let (file, line) = rest.rsplit_once(" at ")?.1.rsplit_once(':')?;
            Some((function, file.to_owned(), line.trim().parse().ok()?))
        })
        .collect()
}

#[test]
fn t0_m7_gdb_bt_lines_and_the_core_file_parse() {
    let text = "#0  probe_panic_read_null () at /COMPONENT_MAIN_DIR/probe_panic.c:49\n\
                warning: 49\t/COMPONENT_MAIN_DIR/probe_panic.c: No such file or directory\n\
                #1  0x42006f5e in probe_panic_outer () at /COMPONENT_MAIN_DIR/probe_panic.c:54\n\
                #2  0x42006fa6 in panic_task (arg=<error reading variable: value has been optimized out>) at /COMPONENT_MAIN_DIR/probe_panic.c:63\n";
    assert_eq!(
        parse_bt(text),
        [
            (
                "probe_panic_read_null".to_owned(),
                "/COMPONENT_MAIN_DIR/probe_panic.c".to_owned(),
                49
            ),
            (
                "probe_panic_outer".to_owned(),
                "/COMPONENT_MAIN_DIR/probe_panic.c".to_owned(),
                54
            ),
            (
                "panic_task".to_owned(),
                "/COMPONENT_MAIN_DIR/probe_panic.c".to_owned(),
                63
            ),
        ]
    );
    let core = elf_core(&[7; 32], 0x3FC8_0000, &[1, 2, 3, 4]);
    assert_eq!(&core[..4], b"\x7fELF");
    assert_eq!(u16::from_le_bytes([core[16], core[17]]), 4, "ET_CORE");
    assert_eq!(&core[core.len() - 4..], &[1, 2, 3, 4]);
}

/// Both watchdogs on `probe_wdt`:
///
/// 1. `wdt_owner` holds a mutex and spins at the priority 4 it inherited from `wdt_waiter`, so
///    IDLE starves and the TWDT fires: the envelope names `wdt_waiter` blocked on that mutex.
/// 2. With interrupts off, the interrupt watchdog fires.
/// 3. `SUMMARY` reports `ESP_RST_TASK_WDT` (6) and `ESP_RST_INT_WDT` (5).
///
/// `detail.watchdog` is classified from the TIMG stage interrupt that fired, not the console.
#[test]
fn t1_m7_watchdog_envelope() {
    let test = "t1_m7_watchdog_envelope";
    let _busy = busy();
    let exit = test.to_string();
    let Some(entry) = probe_entry(test, &exit, "probe_wdt") else {
        return;
    };
    let home = Home::new("watchdog", std::slice::from_ref(&entry));
    let start = home.json(&["start", "probe_wdt", "--boot", "none"], 0);
    let id = start["instance"].as_str().expect("an id").to_owned();

    let twdt = home.json(&["run", "serial:/SUMMARY/", "--timeout", "20s"], 4);
    assert_eq!(twdt["code"], "E_GUEST_PANIC", "{twdt}");
    let detail = &twdt["detail"];
    assert_eq!(detail["fault"], "watchdog", "{detail}");
    assert!(
        twdt["serial_tail"]
            .as_array()
            .is_some_and(|tail| tail.iter().any(|l| l
                .as_str()
                .is_some_and(|l| l.contains("Task watchdog got triggered")))),
        "the TWDT report is in the tail: {twdt}"
    );
    assert_eq!(
        detail["watchdog"], "task",
        "{test}: the TWDT is classified from TIMG0's stage interrupt: {detail}"
    );
    assert!(
        detail["watchdog_signal"]
            .as_str()
            .is_some_and(|s| s.starts_with("timg0 watchdog stage interrupt")),
        "{test}: the envelope names the machine signal it classified from: {detail}"
    );
    let tasks = detail["tasks"]["tasks"].as_array().expect("the task table");
    let task = |name: &str| {
        tasks
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{test}: task {name} in {detail}"))
    };
    let waiter = task("wdt_waiter");
    assert_eq!(waiter["state"], "blocked", "{waiter}");
    let blocked_on = waiter["blocked_on"].as_str().unwrap_or_default();
    assert!(
        blocked_on.contains("held by wdt_owner"),
        "{test}: the blocked task and the mutex owner: {waiter}"
    );
    let owner = task("wdt_owner");
    assert_eq!(
        (owner["priority"].as_u64(), owner["base_priority"].as_u64()),
        (Some(4), Some(3)),
        "{test}: the owner runs at the priority it inherited: {owner}"
    );
    assert!(
        detail["blocked"]
            .as_array()
            .is_some_and(|b| b.iter().any(|e| e["task"] == "wdt_waiter")),
        "{detail}"
    );
    assert!(
        twdt["hint"]
            .as_str()
            .is_some_and(|h| h.contains("starving task")),
        "{twdt}"
    );
    println!(
        "RAN {test} twdt: wdt_waiter {blocked_on}; signal {}",
        detail["watchdog_signal"]
    );

    let iwdt = home.json(&["run", "serial:/SUMMARY/", "--timeout", "20s"], 4);
    assert_eq!(iwdt["code"], "E_GUEST_PANIC", "{iwdt}");
    assert_eq!(iwdt["detail"]["fault"], "watchdog", "{iwdt}");
    assert_eq!(
        iwdt["detail"]["watchdog"], "interrupt",
        "{test}: the IWDT is classified from TIMG1's stage interrupt: {iwdt}"
    );
    assert!(
        iwdt["detail"]["watchdog_signal"]
            .as_str()
            .is_some_and(|s| s.starts_with("timg1 watchdog stage interrupt")),
        "{iwdt}"
    );
    assert!(
        iwdt["detail"]["panic_reason"]
            .as_str()
            .is_some_and(|r| r.contains("Interrupt wdt timeout")),
        "{test}: the IWDT panic names itself: {iwdt}"
    );
    assert!(
        iwdt["serial_tail"]
            .as_array()
            .is_some_and(|tail| tail.iter().any(|l| l
                .as_str()
                .is_some_and(|l| l.starts_with("BOOT|stage=1|reason=6")))),
        "{test}: the stage the IWDT ends began after the TWDT reset: {iwdt}"
    );
    println!(
        "RAN {test} iwdt-envelope: {}",
        iwdt["detail"]["watchdog_signal"]
    );

    let done = home.json(&["run", "serial:/SUMMARY/", "--timeout", "20s"], 0);
    assert_eq!(done["status"], "matched", "{done}");
    let line = done["match"]["text"].as_str().unwrap_or_default();
    assert!(
        line.starts_with("SUMMARY|twdt_reason=6|iwdt_reason=5"),
        "{test}: the IWDT variant yields the IWDT reset reason (ESP_RST_INT_WDT = 5) \
         and the TWDT one ESP_RST_TASK_WDT (6): {done}"
    );
    println!("RAN {test} iwdt-reset-reason: ESP_RST_INT_WDT");
    let _ = home.json(&["stop", &id], 0);
}

/// A deadlock returns `E_DEADLOCK` with the task table; a stack guard names the task.
///
/// `probe_stack`: `stack_task` overflows its 2048-byte stack, the ASSIST_DEBUG guard fires, and
/// the run ends `E_GUEST_PANIC` naming `stack_task` with `probe_stack_recurse` on top.
///
/// `probe_deadlock`: two tasks wait on each other's mutex while IDLE and the tick run on, so the
/// verdict is task-level. The next `run` after `DONE` ends `E_DEADLOCK` naming the cycle, both
/// tasks blocked, and the reset as the only wake input.
#[test]
fn t1_m7_deadlock_and_stack_guard() {
    let test = "t1_m7_deadlock_and_stack_guard";
    let _busy = busy();
    let exit = test.to_string();
    let (Some(stack), Some(deadlock)) = (
        probe_entry(test, &exit, "probe_stack"),
        probe_entry(test, &exit, "probe_deadlock"),
    ) else {
        return;
    };
    let stack_elf = stack.elf.clone().expect("the probe ELF");
    let home = Home::new("deadlock", &[stack, deadlock]);

    let start = home.json(&["start", "probe_stack", "--boot", "none"], 0);
    let id = start["instance"].as_str().expect("an id").to_owned();
    let fault = home.json(
        &[
            "run",
            "serial:/DONE/",
            "--timeout",
            "10s",
            "--instance",
            &id,
        ],
        4,
    );
    assert_eq!(fault["code"], "E_GUEST_PANIC", "{fault}");
    let detail = &fault["detail"];
    assert_eq!(
        detail["panic_reason"], "Stack protection fault",
        "{test}: the stack-guard reason: {detail}"
    );
    assert_eq!(
        detail["task"], "stack_task",
        "{test}: the reason names the task: {detail}"
    );
    assert!(
        detail["tasks"]["tasks"]
            .as_array()
            .is_some_and(|tasks| tasks
                .iter()
                .any(|t| t["name"] == "stack_task" && t["state"] == "running")),
        "{detail}"
    );
    let top = &fault["backtrace"][0];
    assert_eq!(top["function"], "probe_stack_recurse", "{fault}");
    assert!(
        top["file"]
            .as_str()
            .is_some_and(|f| f.ends_with("probe_stack.c")),
        "{top}"
    );
    println!(
        "RAN {test} stack-guard: {} in {} at {}:{}",
        detail["panic_reason"], detail["task"], top["file"], top["line"]
    );
    // The next boot: no stale violation fires, and the probe reads the panic reset reason.
    let after = home.json(
        &[
            "run",
            "serial:/DONE/",
            "--timeout",
            "10s",
            "--instance",
            &id,
        ],
        0,
    );
    let tail = after["serial"]["tail"].to_string();
    assert!(
        tail.contains("AFTER|reason=4|") && tail.contains("DONE|name=probe_stack|status=ok"),
        "{test}: the boot after the overflow is a panic reset (ESP_RST_PANIC = 4): {after}"
    );
    println!("RAN {test} stack-guard-reset: the next boot reports ESP_RST_PANIC");
    // Class A, `captures/device-probe_stack-20260917T154321Z.log` (data root): the reboot banner is
    // `rst:0xc (RTC_SW_CPU_RST),boot:0xa (SPI_FAST_FLASH_BOOT)` then `Saved PC:0x4038306c`, inside
    // `esp_restart_noos`.
    let console = console_lines(&home, &id);
    let at = console
        .iter()
        .position(|l| l.starts_with("rst:0xc (RTC_SW_CPU_RST),boot:0xa (SPI_FAST_FLASH_BOOT)"))
        .unwrap_or_else(|| panic!("{test}: the panic reset banner: {console:?}"));
    let saved = console
        .get(at + 1)
        .and_then(|l| l.strip_prefix("Saved PC:0x"))
        .and_then(|hex| u32::from_str_radix(hex.trim(), 16).ok())
        .unwrap_or_else(|| {
            panic!(
                "{test}: `Saved PC:` follows the banner: {:?}",
                console.get(at + 1)
            )
        });
    let elf = pemu_loader::elf::ElfInfo::parse(&std::fs::read(&stack_elf).expect("the ELF reads"))
        .expect("the probe ELF parses");
    let function = elf.symbols.func_at(saved).map(|sym| sym.name.clone());
    assert_eq!(
        function.as_deref(),
        Some("esp_restart_noos"),
        "{test}: the saved PC {saved:#010x} is the esp_restart_noos reset point, as on the device"
    );
    println!("RAN {test} saved-pc: rst:0xc then Saved PC:{saved:#010x} in esp_restart_noos");
    let _ = home.json(&["stop", &id], 0);

    let start = home.json(&["start", "probe_deadlock", "--boot", "none"], 0);
    let id = start["instance"].as_str().expect("an id").to_owned();
    let done = home.json(
        &[
            "run",
            "serial:/DONE/",
            "--timeout",
            "10s",
            "--instance",
            &id,
        ],
        0,
    );
    assert_eq!(
        done["status"], "matched",
        "{test}: the cycle closes while `app_main` still runs, so its own report is still printed          and the wait on it still matches: {done}"
    );
    let deadlock_line = done["serial"]["tail"].to_string();
    assert!(
        deadlock_line.contains("|a_done=0|b_done=0"),
        "{test}: neither task got both mutexes: {done}"
    );
    let tasks = home.json(&["inspect", "tasks", "--instance", &id], 0);
    let rows = tasks["tasks"]["tasks"]
        .as_array()
        .or_else(|| tasks["tasks"].as_array())
        .unwrap_or_else(|| panic!("{test}: a task table: {tasks}"))
        .clone();
    let blocked_on = |name: &str| {
        let row = rows
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{test}: {name} in {tasks}"));
        assert_eq!(row["state"], "blocked", "{row}");
        row["blocked_on"].as_str().unwrap_or_default().to_owned()
    };
    let a = blocked_on("dl_task_a");
    let b = blocked_on("dl_task_b");
    assert!(
        a.contains("held by dl_task_b") && b.contains("held by dl_task_a"),
        "{test}: each task waits on the mutex the other holds: a {a}; b {b}"
    );
    println!("RAN {test} deadlock-table: dl_task_a {a}; dl_task_b {b}");

    // `app_main` has returned, so nothing outside the cycle is left to run.
    let fault = home.json(&["run", "--for", "1s", "--instance", &id], 4);
    assert_eq!(
        fault["code"], "E_DEADLOCK",
        "{test}: a mutex cycle is `E_DEADLOCK`: {fault}"
    );
    let detail = &fault["detail"];
    assert_eq!(detail["fault"], "deadlock", "{detail}");
    assert_eq!(
        detail["deadlock"], "task",
        "{test}: a task-level deadlock, not the hart-level one: {detail}"
    );
    let cycle = detail["cycle"].as_array().expect("the cycle");
    let named = |name: &str| {
        cycle
            .iter()
            .find(|link| link["task"] == name)
            .unwrap_or_else(|| panic!("{test}: {name} in the cycle: {detail}"))
    };
    assert_eq!(
        cycle.len(),
        2,
        "{test}: the two tasks of the cycle: {detail}"
    );
    assert_eq!(named("dl_task_a")["held_by"], "dl_task_b", "{detail}");
    assert_eq!(named("dl_task_b")["held_by"], "dl_task_a", "{detail}");
    assert_ne!(
        named("dl_task_a")["waits_for"],
        named("dl_task_b")["waits_for"],
        "{test}: each waits for a different mutex: {detail}"
    );
    assert!(
        detail["tasks"]["tasks"].as_array().is_some_and(|rows| {
            ["dl_task_a", "dl_task_b"].iter().all(|name| {
                rows.iter()
                    .any(|row| row["name"] == *name && row["state"] == "blocked")
            })
        }),
        "{test}: the task table: {detail}"
    );
    assert!(
        detail["blocked"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["task"] == "dl_task_a")),
        "{detail}"
    );
    assert_eq!(
        detail["wake_inputs"].as_array().map(Vec::len),
        Some(1),
        "{test}: only a reset clears a mutex cycle: {detail}"
    );
    assert!(
        detail["wake_inputs"][0]
            .as_str()
            .is_some_and(|w| w.contains("reset")),
        "{detail}"
    );
    println!(
        "RAN {test} deadlock: E_DEADLOCK, cycle {} -> {}",
        named("dl_task_a")["task"],
        named("dl_task_b")["task"]
    );
    // Running on returns the same stop, never a pass.
    let again = home.json(&["run", "--for", "1s", "--instance", &id], 4);
    assert_eq!(again["code"], "E_DEADLOCK", "{again}");
    let _ = home.json(&["stop", &id], 0);
}

/// Every `env` input is journaled, and a replay of the journal gives an equal digest.
///
/// USB and battery change at 0, an NFC card and a mic tone at 300 ms, and `snapshot save` at
/// 600 ms reports the state hash. The journal is read from the `journal_pending` section of an
/// export taken before each run applies it. A fresh machine replaying those entries must match
/// the hash; the same replay without the `env` entries must not.
#[test]
fn t1_m7_env_inputs_are_journaled() {
    use pemu_core::input::{EnvChange, InputEvent};

    let test = "t1_m7_env_inputs_are_journaled";
    let _busy = busy();
    let Some((entry, image, elf)) = official_entry(test) else {
        return;
    };
    let home = Home::new("env", std::slice::from_ref(&entry));
    let start = home.json(&["start", common::OFFICIAL, "--boot", "none"], 0);
    let id = start["instance"].as_str().expect("an id").to_owned();
    let env = |json: serde_json::Value| {
        let out = home.json_doc(&["env"], &json, 0);
        assert!(out["applied"].is_object(), "env {json}: {out}");
    };
    env(serde_json::json!({"usb": "host"}));
    env(serde_json::json!({"battery": {"mv": 3700, "soc": 42}}));
    let mut journal = pending_journal(&home, "env-a");
    let _ = home.json(&["run", "--for", "300ms"], 0);
    env(serde_json::json!({"nfc": {"card_present": true}}));
    env(serde_json::json!({"mic": {"kind": "tone", "hz": 440, "amplitude": 12000}}));
    journal.extend(pending_journal(&home, "env-b"));
    let _ = home.json(&["run", "--for", "300ms"], 0);
    let saved = home.json(&["snapshot", "save", "env-end"], 0);
    let vt = saved["vt_us"].as_u64().expect("the save instant");
    let digest = saved["state_hash"]
        .as_str()
        .expect("a state hash")
        .to_owned();
    assert_eq!(vt, 600_000, "{saved}");
    let _ = home.json(&["stop", &id], 0);

    let env_kinds: std::collections::BTreeSet<&str> = journal
        .iter()
        .filter_map(|entry| match &entry.ev {
            InputEvent::Battery(_) => Some("battery"),
            InputEvent::NfcTap { .. } => Some("nfc"),
            InputEvent::Env(EnvChange::MicSource(_)) => Some("mic"),
            InputEvent::UsbCable { .. } | InputEvent::UsbClient { .. } => Some("usb"),
            _ => None,
        })
        .collect();
    assert_eq!(
        env_kinds,
        ["battery", "mic", "nfc", "usb"].into_iter().collect(),
        "{test}: the four `env` kinds are journaled: {journal:?}"
    );
    assert!(
        journal.iter().any(|e| e.at.as_us() == 300_000),
        "{test}: the second group was journaled at its own instant: {journal:?}"
    );
    println!("RAN {test} journaled: {} entries", journal.len());

    let replay = |entries: &[pemu_core::journal::JournalEntry]| {
        let flash = pemu_host::backend::merged_image(&image).expect("a merged image");
        let mut m = pemu_host::backend::build_machine_with_elf(
            flash,
            Some(std::sync::Arc::clone(&elf)),
            1,
            pemu_machine::config::TimingProfileId::Fast,
        )
        .expect("the machine composes");
        for entry in entries {
            m.input(pemu_machine::machine::At::Vt(entry.at), entry.ev.clone())
                .expect("a journaled input is not in the past of a fresh machine");
        }
        m.run(pemu_machine::run::RunLimits {
            until: Some(pemu_core::time::VTime::from_us(vt)),
            max_insns: None,
            stops: pemu_machine::stops::StopSet::default(),
        });
        assert_eq!(
            m.now().as_us(),
            vt,
            "{test}: the replay reaches the save instant"
        );
        pemu_machine::determinism::hex(&pemu_machine::SnapshotMachine::state_hash(&m))
    };
    assert_eq!(
        replay(&journal),
        digest,
        "{test}: replaying the journal gives the daemon's digest"
    );
    println!("RAN {test} replay-digest-equal");
    // `start` journals the initial USB world at the same instant, so the USB entries stay.
    let without_env: Vec<_> = journal
        .iter()
        .filter(|e| {
            !matches!(
                e.ev,
                InputEvent::Battery(_) | InputEvent::NfcTap { .. } | InputEvent::Env(_)
            )
        })
        .cloned()
        .collect();
    assert_ne!(
        replay(&without_env),
        digest,
        "{test}: the replay without the `env` entries ends elsewhere, so they compared something"
    );
    println!("RAN {test} control-without-env-differs");
}

fn pk_entry(test: &str) -> Option<CorpusEntry> {
    let bin = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport.elf")?;
    Some(CorpusEntry {
        id: common::PK.to_owned(),
        bin,
        elf: Some(elf),
    })
}

/// The verified `official` image and app ELF as a private corpus entry, with the image bytes and
/// the parsed ELF for an in-process twin.
fn official_entry(
    test: &str,
) -> Option<(
    CorpusEntry,
    Vec<u8>,
    std::sync::Arc<pemu_loader::elf::ElfInfo>,
)> {
    let bin = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")?;
    let image = std::fs::read(&bin).expect("the verified corpus image is readable");
    let parsed = pemu_loader::elf::ElfInfo::parse(&std::fs::read(&elf).expect("the ELF reads"))
        .expect("the official ELF parses");
    Some((
        CorpusEntry {
            id: common::OFFICIAL.to_owned(),
            bin,
            elf: Some(elf),
        },
        image,
        std::sync::Arc::new(parsed),
    ))
}

/// The journal entries the live instance has not applied yet, from a `snapshot export` of it.
fn pending_journal(home: &Home, name: &str) -> Vec<pemu_core::journal::JournalEntry> {
    let bytes = exported(home, name);
    let snapshot = pemu_core::snap::Snapshot::from_bytes(&bytes).expect("an export decodes");
    snapshot
        .get::<pemu_core::snap::JournalPending>()
        .expect("an export carries the pending journal")
        .0
}

fn exported(home: &Home, name: &str) -> Vec<u8> {
    let out = home.json(&["snapshot", "export", name], 0);
    let path = out["path"].as_str().expect("an artifact path").to_owned();
    assert!(!path.starts_with('/'), "artifact paths are relative: {out}");
    let file = find_file(&home.artifacts(), &path)
        .unwrap_or_else(|| panic!("{path} is under the daemon's artifacts root"));
    std::fs::read(file).expect("the export reads")
}

/// Every console line `id` printed so far, read through `serial read` in chunks small enough that
/// the shaped result shows each chunk whole.
fn console_lines(home: &Home, id: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cursor = 0u64;
    loop {
        let read = home.json(
            &[
                "serial",
                "read",
                "--cursor",
                &cursor.to_string(),
                "--max-bytes",
                "1024",
                "--instance",
                id,
            ],
            0,
        );
        let serial = &read["serial"];
        assert_eq!(serial["elided_lines"], 0, "a chunk shows whole: {read}");
        let next = read["next_cursor"].as_u64().unwrap_or(cursor);
        for key in ["head", "tail"] {
            for line in serial[key].as_array().into_iter().flatten() {
                lines.push(line.as_str().unwrap_or_default().to_owned());
            }
        }
        if next == cursor {
            break;
        }
        cursor = next;
    }
    lines
}

fn find_file(root: &std::path::Path, tail: &str) -> Option<std::path::PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.to_string_lossy().replace('\\', "/").ends_with(tail) {
                return Some(path);
            }
        }
    }
    None
}

/// One quoted glob runs the whole scenario suite and writes one JUnit file.
///
/// The pattern is one argument, so the command's own glob expansion finds the files. Every
/// committed scenario must run on a fork of a `boot_cache: ui-settled` template (one per boot),
/// leave no instance running, write one suite per scenario, and hit the boot cache on a second
/// batch. The core MCP `tools/list` result is held to its 24 KB guard.
#[test]
fn t1_m7_scenario_suite_writes_one_junit_file() {
    let test = "t1_m7_scenario_suite_writes_one_junit_file";
    let _busy = busy();
    let Some((entry, _, _)) = official_entry(test) else {
        return;
    };
    let Some(pk) = pk_entry(test) else {
        return;
    };
    let home = Home::new("suite", &[entry, pk]);
    // The batch is the daemon's; a CLI with no daemon answers in process.
    home.serve_with_limit(12);
    let mut files: Vec<String> = std::fs::read_dir(workspace().join("tests/scenarios"))
        .expect("tests/scenarios lists")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".yaml"))
        .map(|name| format!("tests/scenarios/{name}"))
        .collect();
    files.sort();
    assert!(files.len() >= 2, "a suite to run: {files:?}");

    let batch = |round: usize| {
        let started = std::time::Instant::now();
        let out = home
            .command(&[
                "scenario",
                "tests/scenarios/*.yaml",
                "--op",
                "run",
                "--jobs",
                "8",
                "--junit",
                "junit.xml",
                "--output",
                "json",
            ])
            .current_dir(workspace())
            .stdin(std::process::Stdio::null())
            .output()
            .expect("passportsim runs");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let json: serde_json::Value = serde_json::from_str(stdout.trim_end()).unwrap_or_else(|e| {
            panic!(
                "{test}: batch {round} printed no JSON: {e}: {stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code(), json, started.elapsed())
    };

    let (code, first, wall) = batch(1);
    let scenarios = first["scenarios"]
        .as_array()
        .unwrap_or_else(|| panic!("{test}: scenario reports: {code:?} {first}"));
    let sources: Vec<&str> = scenarios
        .iter()
        .filter_map(|r| r["source"].as_str())
        .collect();
    // The source is the workspace-relative path, not the absolute one the CLI forwarded.
    assert_eq!(
        sources, files,
        "{test}: the glob ran every file, each named relative to the workspace: {first}"
    );
    assert_eq!(scenarios.len(), files.len(), "{first}");
    for report in scenarios {
        assert!(
            matches!(
                report["status"].as_str(),
                Some("pass" | "pass_with_caveats")
            ),
            "{test}: {} did not pass: {report}",
            report["source"]
        );
    }
    assert!(
        matches!(code, Some(0 | 10)),
        "{test}: a passing batch exits 0, or 10 with caveats: {code:?} {first}"
    );
    // Templates group by boot, not by image: two boots (`official` and `pk`), two templates.
    let templates = first["batch"]["forked_from"]
        .as_array()
        .unwrap_or_else(|| panic!("{test}: the templates: {first}"));
    let mut images: Vec<&str> = templates
        .iter()
        .filter_map(|t| t["image"].as_str())
        .collect();
    images.sort_unstable();
    assert_eq!(
        images,
        ["official", common::PK],
        "{test}: one template per boot: {first}"
    );
    let template_ids: Vec<&str> = templates
        .iter()
        .map(|from| {
            assert_eq!(from["boot_cache"]["point"], "ui-settled", "{first}");
            from["instance"].as_str().expect("a template instance")
        })
        .collect();
    let template = template_ids.join(", ");
    let from = &templates[0];
    let mut instances: Vec<&str> = scenarios
        .iter()
        .filter_map(|r| r["instance"].as_str())
        .collect();
    assert!(
        instances.iter().all(|i| !template_ids.contains(i)),
        "{test}: every scenario runs on a fork, not on a cached instance: {first}"
    );
    instances.sort_unstable();
    instances.dedup();
    assert_eq!(
        instances.len(),
        scenarios.len(),
        "{test}: one instance per scenario: {first}"
    );
    let parallel = first["batch"]["parallel"].as_u64().unwrap_or(0);
    assert!(parallel > 1, "{test}: the batch ran in parallel: {first}");

    let junit_path = first["junit_path"].as_str().expect("a JUnit path");
    let junit = std::fs::read_to_string(
        find_file(&home.artifacts(), junit_path).expect("the JUnit file is under the artifacts"),
    )
    .expect("the JUnit file reads");
    assert_eq!(
        junit.matches("<testsuite ").count(),
        files.len(),
        "{test}: one JUnit file, one suite per scenario: {junit}"
    );
    let status = home.json(&["status"], 0);
    assert!(
        status["instances"]
            .as_array()
            .expect("a status list")
            .iter()
            .all(|i| i["state"] == "stopped"),
        "{test}: the batch stopped every instance it made: {status}"
    );
    println!(
        "RAN {test} batch: {} scenario(s) on forks of {template} (boot cache hit {}), {parallel} at once, one JUnit file, {} ms",
        scenarios.len(),
        from["boot_cache"]["hit"],
        wall.as_millis()
    );

    let (_, second, _) = batch(2);
    let again = second["batch"]["forked_from"]
        .as_array()
        .unwrap_or_else(|| panic!("{test}: the templates: {second}"));
    assert_eq!(again.len(), templates.len(), "{test}: {second}");
    assert!(
        again.iter().all(|from| from["boot_cache"]["hit"] == true),
        "{test}: the second batch restores every cached boot: {second}"
    );
    println!("RAN {test} boot-cache: the second batch forks from a cache hit");

    let discovery = home.discovery();
    let port = u16::try_from(discovery["port"].as_u64().expect("a port")).expect("a port");
    let token = discovery["token"].as_str().expect("a token");
    let host = format!("127.0.0.1:{port}");
    let headers = vec![
        ("Host", host.clone()),
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/json".to_owned()),
        ("Accept", "application/json, text/event-stream".to_owned()),
    ];
    let list = http(
        port,
        "POST",
        "/mcp",
        &headers,
        &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})
            .to_string(),
    );
    assert_eq!(list.0, 200, "{list:?}");
    let body: serde_json::Value = serde_json::from_str(&list.2).expect("JSON-RPC");
    let bytes = serde_json::to_string(&body["result"]).expect("JSON").len();
    let tools = body["result"]["tools"].as_array().map_or(0, Vec::len);
    assert!(tools > 0, "{body}");
    assert!(
        bytes <= 24 * 1024,
        "{test}: the core tool list is at most 24 KB, measured {bytes} bytes"
    );
    println!("RAN {test} tools-list: {tools} core tool(s) in {bytes} bytes (budget 24576)");
}

type Refusal<'a> = (&'a str, Vec<(&'a str, String)>, u16);

/// The HTTP, WS and MCP streamable HTTP protocols of the real daemon, and every rejection.
///
/// Over plain loopback with the discovery file's port and token: `GET /v1/health` and a command
/// over HTTP, a `passportsim.v1` WebSocket JSON-RPC call, MCP `initialize`, `tools/list`,
/// `tools/call` and `GET /mcp` 405. Each refuses a missing or wrong token (401) and a foreign
/// `Host` or `Origin` (403) before it does anything.
#[test]
fn t1_m7_server_protocol_tests() {
    let test = "t1_m7_server_protocol_tests";
    let _busy = busy();
    let Some(bin) =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let home = Home::new(
        "server",
        &[CorpusEntry {
            id: common::OFFICIAL.to_owned(),
            bin,
            elf: None,
        }],
    );
    let start = home.json(&["start", common::OFFICIAL, "--boot", "none"], 0);
    let id = start["instance"].as_str().expect("an id").to_owned();
    let discovery = home.discovery();
    let port = u16::try_from(discovery["port"].as_u64().expect("a port")).expect("a port");
    let token = discovery["token"].as_str().expect("a token").to_owned();
    let host = format!("127.0.0.1:{port}");
    let bearer = format!("Bearer {token}");
    let wrong = format!("Bearer {}", "0".repeat(token.len()));
    let origin = format!("http://{host}");
    let refusals: [Refusal; 5] = [
        ("missing-token", vec![("Host", host.clone())], 401),
        (
            "wrong-token",
            vec![("Host", host.clone()), ("Authorization", wrong.clone())],
            401,
        ),
        (
            "foreign-host",
            vec![
                ("Host", format!("evil.example:{port}")),
                ("Authorization", bearer.clone()),
            ],
            403,
        ),
        (
            "rebound-host",
            vec![
                ("Host", format!("127.0.0.1:{}", port.wrapping_add(1))),
                ("Authorization", bearer.clone()),
            ],
            403,
        ),
        (
            "foreign-origin",
            vec![
                ("Host", host.clone()),
                ("Authorization", bearer.clone()),
                ("Origin", "http://evil.example".to_owned()),
            ],
            403,
        ),
    ];
    let good = vec![
        ("Host", host.clone()),
        ("Authorization", bearer.clone()),
        ("Origin", origin.clone()),
    ];

    let health = http(port, "GET", "/v1/health", &good, "");
    assert_eq!(health.0, 200, "{health:?}");
    let body: serde_json::Value = serde_json::from_str(&health.2).expect("JSON");
    assert_eq!(body["protocol"], 1, "{body}");
    let status_path = format!("/v1/instances/{id}/commands/status");
    let status = http(port, "POST", &status_path, &good, "{}");
    assert_eq!(status.0, 200, "{status:?}");
    let body: serde_json::Value = serde_json::from_str(&status.2).expect("JSON");
    assert_eq!(body["ok"], true, "{body}");
    for (name, headers, want) in &refusals {
        for (method, path, payload) in [
            ("GET", "/v1/health", ""),
            ("POST", status_path.as_str(), "{}"),
        ] {
            let got = http(port, method, path, headers, payload);
            assert_eq!(got.0, *want, "HTTP {method} {path} {name}: {got:?}");
            assert!(!got.2.contains(&token), "a refusal never echoes the token");
        }
    }
    println!("RAN {test} http: health, a command and five refusals");

    let ws_path = format!("/v1/instances/{id}/ws");
    let (code, head, mut socket) = ws_open(port, &ws_path, &good);
    assert_eq!(code, 101, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-protocol: passportsim.v1"),
        "{head}"
    );
    let socket = socket.as_mut().expect("an upgraded socket");
    ws_send_text(
        socket,
        &serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "passport_status", "params": {"instance": id}})
            .to_string(),
    );
    let answer: serde_json::Value = serde_json::from_str(&ws_read_text(socket)).expect("JSON-RPC");
    assert_eq!(answer["id"], 9, "{answer}");
    assert!(answer.get("result").is_some(), "{answer}");
    for (name, headers, want) in &refusals {
        let (code, head, socket) = ws_open(port, &ws_path, headers);
        assert_eq!(code, *want, "WS {name}: {head}");
        assert!(socket.is_none());
    }
    println!("RAN {test} ws: upgrade, subprotocol, a JSON-RPC call and five refusals");

    let mut mcp_headers = good.clone();
    mcp_headers.push(("Content-Type", "application/json".to_owned()));
    mcp_headers.push(("Accept", "application/json, text/event-stream".to_owned()));
    let rpc = |id: u64, method: &str, params: serde_json::Value| {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
    };
    let init = http(
        port,
        "POST",
        "/mcp",
        &mcp_headers,
        &rpc(
            1,
            "initialize",
            serde_json::json!({"protocolVersion": "2025-11-25"}),
        ),
    );
    assert_eq!(init.0, 200, "{init:?}");
    let body: serde_json::Value = serde_json::from_str(&init.2).expect("JSON-RPC");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");
    let list = http(
        port,
        "POST",
        "/mcp",
        &mcp_headers,
        &rpc(2, "tools/list", serde_json::json!({})),
    );
    assert_eq!(list.0, 200, "{list:?}");
    let body: serde_json::Value = serde_json::from_str(&list.2).expect("JSON-RPC");
    let tools = body["result"]["tools"].as_array().expect("a tool list");
    assert!(
        tools.iter().any(|t| t["name"] == "passport_status"),
        "{body}"
    );
    let call = http(
        port,
        "POST",
        "/mcp",
        &mcp_headers,
        &rpc(
            3,
            "tools/call",
            serde_json::json!({"name": "passport_status", "arguments": {"instance": id}}),
        ),
    );
    assert_eq!(call.0, 200, "{call:?}");
    let body: serde_json::Value = serde_json::from_str(&call.2).expect("JSON-RPC");
    assert_eq!(body["result"]["isError"], false, "{body}");
    let get = http(port, "GET", "/mcp", &good, "");
    assert_eq!(get.0, 405, "{get:?}");
    for (name, headers, want) in &refusals {
        let mut headers = headers.clone();
        headers.push(("Content-Type", "application/json".to_owned()));
        let got = http(
            port,
            "POST",
            "/mcp",
            &headers,
            &rpc(4, "tools/list", serde_json::json!({})),
        );
        assert_eq!(got.0, *want, "MCP {name}: {got:?}");
        assert!(
            !got.2.contains("passport_status"),
            "a refused MCP call lists no tool"
        );
    }
    println!("RAN {test} mcp: initialize, tools/list, tools/call, GET 405 and five refusals");
    let _ = home.json(&["stop", &id], 0);
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &str,
) -> (u16, String, String) {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("the daemon listens");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .expect("a timeout");
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    stream
        .write_all(request.as_bytes())
        .expect("the request is sent");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the answer is read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(rest)
    } else {
        rest.to_owned()
    };
    (status, head.to_owned(), body)
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 || tail.len() < size {
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].trim_start_matches("\r\n");
    }
    out
}

/// A WebSocket opening handshake (RFC 6455 §4.1) offering `passportsim.v1`: the status, the
/// response head, and the socket when it upgraded.
fn ws_open(
    port: u16,
    path: &str,
    headers: &[(&str, String)],
) -> (u16, String, Option<std::net::TcpStream>) {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("the daemon listens");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .expect("a timeout");
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: passportsim.v1\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the handshake is sent");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head, (status == 101).then_some(stream))
}

/// Sends one masked text frame (RFC 6455 §5.2; a client always masks).
fn ws_send_text(stream: &mut std::net::TcpStream, text: &str) {
    use std::io::Write as _;
    let mask = [0x37u8, 0xFA, 0x21, 0x3D];
    let mut frame = vec![0x81u8];
    let len = text.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&u16::try_from(len).expect("a short frame").to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(text.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).expect("the frame is sent");
}

fn ws_read_text(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;
    loop {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).expect("a frame header");
        let mut len = u64::from(head[1] & 0x7F);
        if len == 126 {
            let mut ext = [0u8; 2];
            stream.read_exact(&mut ext).expect("a length");
            len = u64::from(u16::from_be_bytes(ext));
        } else if len == 127 {
            let mut ext = [0u8; 8];
            stream.read_exact(&mut ext).expect("a length");
            len = u64::from_be_bytes(ext);
        }
        let mut payload = vec![0u8; usize::try_from(len).expect("a frame that fits")];
        stream.read_exact(&mut payload).expect("the payload");
        if head[0] & 0x0F == 0x1 {
            return String::from_utf8(payload).expect("a UTF-8 text frame");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The binary harness: a private home, the real daemon, the CLI and the MCP adapter
// ---------------------------------------------------------------------------------------------
//
// `start` auto-spawns `serve --headless` in a private `PASSPORTSIM_HOME`, and every later call
// forwards to it. The home's corpus map names only the ids a test boots. No
// `PASSPORTSIM_DATA_ROOT`, `PASSPORTSIM_CONFIG_DIR` or `PASSPORTSIM_CORPUS_<ID>` reaches a child,
// and the daemon is stopped with `serve --stop` however the test ends.

struct CorpusEntry {
    id: String,
    bin: std::path::PathBuf,
    elf: Option<std::path::PathBuf>,
}

/// A private `PASSPORTSIM_HOME` with a daemon of its own, stopped and removed on drop.
struct Home {
    bin: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Home {
    fn new(tag: &str, entries: &[CorpusEntry]) -> Home {
        let home = std::env::temp_dir().join(format!(
            "pemu-m7-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        ));
        let config = home.join("config");
        std::fs::create_dir_all(&config).expect("a temporary home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for dir in [&home, &config] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .expect("owner-only");
            }
        }
        let quoted = |path: &std::path::Path| {
            let text = path.to_str().expect("a UTF-8 corpus path");
            assert!(
                !text.contains(['"', ',']),
                "the corpus map reader takes no quote or comma"
            );
            format!("\"{text}\"")
        };
        let mut map = String::new();
        for entry in entries {
            map.push_str(&format!("[{}]\nbin = {}\n", entry.id, quoted(&entry.bin)));
            if let Some(elf) = &entry.elf {
                map.push_str(&format!("elf = {}\n", quoted(elf)));
            }
        }
        std::fs::write(config.join("corpus.toml"), map).expect("the corpus map");
        Home {
            bin: passportsim(),
            home,
        }
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(&self.bin);
        command.args(args).env("PASSPORTSIM_HOME", &self.home);
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| {
                key == "PASSPORTSIM_DATA_ROOT"
                    || key == "PASSPORTSIM_CONFIG_DIR"
                    || key.starts_with("PASSPORTSIM_CORPUS_")
            }) {
                command.env_remove(&key);
            }
        }
        command
    }

    #[track_caller]
    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let child = self
            .command(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("passportsim runs");
        wait_by(child, args, CLI_DEADLINE)
    }

    /// One `--output json` call whose nested arguments go in as `--json -` on stdin, which must exit
    /// `code`; its stdout as JSON.
    #[track_caller]
    fn json_doc(&self, args: &[&str], doc: &serde_json::Value, code: i32) -> serde_json::Value {
        use std::io::Write as _;
        let mut argv = args.to_vec();
        argv.extend_from_slice(&["--json", "-", "--output", "json"]);
        let mut child = self
            .command(&argv)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("passportsim runs");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(doc.to_string().as_bytes())
            .expect("the document is written");
        let (exit, stdout, stderr) = wait_by(child, &argv, CLI_DEADLINE);
        common::assert_cli_exit(
            exit,
            code,
            &stdout,
            &format!("passportsim {argv:?} {doc}: {stderr}"),
        );
        serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("passportsim {argv:?} printed no JSON: {e}: {stdout}"))
    }

    /// One `--output json` call that must exit `code`. A `run` that names no host budget gets
    /// [`HOST_BUDGET_MS`].
    #[track_caller]
    fn json(&self, args: &[&str], code: i32) -> serde_json::Value {
        let mut argv = args.to_vec();
        if args.first() == Some(&"run") && !args.contains(&"--wall-budget-ms") {
            argv.extend_from_slice(&["--wall-budget-ms", HOST_BUDGET_MS]);
        }
        argv.extend_from_slice(&["--output", "json"]);
        let (exit, stdout, stderr) = self.cli(&argv);
        common::assert_cli_exit(
            exit,
            code,
            &stdout,
            &format!("passportsim {argv:?}: {stderr}"),
        );
        serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("passportsim {argv:?} printed no JSON: {e}: {stdout}"))
    }

    /// Starts this home's daemon with `--max-instances max`, detached from the test's stdio, and waits
    /// until it answers `status`. [`Drop`] stops it with `serve --stop`.
    fn serve_with_limit(&self, max: usize) {
        let mut child = self
            .command(&["serve", "--headless", "--max-instances", &max.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("passportsim serve runs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let path = self
                .home
                .join("run")
                .join(pemu_host::daemon::DISCOVERY_FILE);
            if path.is_file() && self.cli(&["status", "--output", "json"]).0 == 0 {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!("`serve --headless` exited {status} before it answered");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon answered within 30 s"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::mem::drop(child);
    }

    fn discovery(&self) -> serde_json::Value {
        let path = self
            .home
            .join("run")
            .join(pemu_host::daemon::DISCOVERY_FILE);
        let text = std::fs::read_to_string(&path).expect("the daemon published its discovery file");
        serde_json::from_str(&text).expect("the discovery file is JSON")
    }

    fn artifacts(&self) -> std::path::PathBuf {
        self.home.join("data").join("artifacts")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // A drop can run while a test is unwinding, so a stop that hangs is killed and ignored rather
        // than panicked on.
        if let Ok(child) = self
            .command(&["serve", "--stop"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            let _ = try_wait_by(child, CLI_DEADLINE);
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

// ---------------------------------------------------------------------------------------------
// The M7 probes
// ---------------------------------------------------------------------------------------------

fn probe_pin(name: &str, field: &str) -> String {
    let manifest = std::fs::read_to_string(workspace().join("tests/fw/manifest.toml"))
        .expect("the probe manifest");
    let prefix = format!("{field} = \"");
    manifest
        .split("[[probe]]")
        .find(|block| block.contains(&format!("name = \"{name}\"")))
        .and_then(|block| {
            block
                .lines()
                .find_map(|l| l.strip_prefix(prefix.as_str()))
                .map(|v| v.trim_end_matches('"').to_owned())
        })
        .unwrap_or_else(|| panic!("tests/fw/manifest.toml pins {field} of {name}"))
}

/// A probe as a private corpus entry: its merged image and its unstripped ELF from
/// `corpus/probes/`, each checked against the hash `tests/fw/manifest.toml` pins. The unstripped
/// ELF carries the DWARF the file:line and task-name checks need.
fn probe_entry(test: &str, exit: &str, name: &str) -> Option<CorpusEntry> {
    let official =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
    let probes = official
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join("probes");
    let bin = probes.join(format!("{name}-8MB.bin"));
    let elf = probes.join("build").join(name).join(format!("{name}.elf"));
    let (Ok(bin_bytes), Ok(elf_bytes)) = (std::fs::read(&bin), std::fs::read(&elf)) else {
        common::skip(
            test,
            &format!(
                "{exit} corpus/probes/{name}-8MB.bin or its build ELF is not built (cargo xtask probes)"
            ),
        );
        return None;
    };
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&bin_bytes),
        probe_pin(name, "merged_sha256"),
        "{test}: corpus/probes/{name}-8MB.bin is not the pinned build"
    );
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&elf_bytes),
        probe_pin(name, "elf_sha256"),
        "{test}: the {name} build ELF is not the pinned one"
    );
    Some(CorpusEntry {
        id: name.to_owned(),
        bin,
        elf: Some(elf),
    })
}
