//! Milestone M12 tests: Wi-Fi. Names use the prefix `t<tier>_m12_` so `xtask ci` can count them.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::machine;
// The fresh-process leg of the determinism harness, shared with m1.rs and m3.rs.
#[allow(dead_code)]
mod determinism;

use std::sync::Arc;

use pemu_core::hostio::SerialStream;
use pemu_core::input::{EnvChange, InputEvent, WifiAp};
use pemu_core::snap::{SnapOpts, Snapshot};
use pemu_core::time::VTime;
use pemu_loader::elf::ElfInfo;
use pemu_machine::config::MachineConfig;
use pemu_machine::executor::Executor;
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{StopReason, StopSet};
use pemu_radio::wifi::driver::{DriverState, WifiState};

/// The console of a run, accumulated past the ring's capacity: the whole `scan3` console is larger
/// than the USJ ring, which evicts. A cursor the ring has already evicted is a lost byte, which
/// fails the test.
struct Console {
    bytes: Vec<u8>,
    cursor: u64,
}

impl Console {
    fn from(cursor: u64) -> Console {
        Console {
            bytes: Vec::new(),
            cursor,
        }
    }

    fn pump(&mut self, m: &mut Machine) {
        let ring = m.io().serial_ring(SerialStream::UsjTx);
        assert!(
            self.cursor >= ring.tail(),
            "the console ring evicted bytes before cursor {} (tail {})",
            self.cursor,
            ring.tail()
        );
        self.bytes.extend(ring.slices(self.cursor).iter().copied());
        self.cursor = ring.head();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    fn tail(&self) -> String {
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(16)..].join("\n")
    }
}

/// How far a `run_while` step advances virtual time: short enough that an instant lasting one
/// worker hop (an aborted scan's `SCAN_DONE` delivery) is not stepped over.
const STEP_US: u64 = 100;

/// Runs `m` in [`STEP_US`] steps, pumping the console, until `done` or `limit_ms`.
fn run_while(
    m: &mut Machine,
    con: &mut Console,
    limit_ms: u64,
    test: &str,
    what: &str,
    // `FnMut`, because the scripted relay journals its answer at the instant it reads the window.
    mut done: impl FnMut(&mut Machine, &Console) -> bool,
) {
    loop {
        con.pump(m);
        if done(m, con) {
            return;
        }
        assert!(
            m.now() < VTime::from_ms(limit_ms),
            "{test}: no {what} by {limit_ms} ms; console tail:\n{}",
            con.tail()
        );
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + STEP_US)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test}: {}", con.tail());
    }
}

fn run_to_line(m: &mut Machine, con: &mut Console, line: &'static str, limit_ms: u64, test: &str) {
    run_while(m, con, limit_ms, test, line, |_, con| {
        con.text().contains(line)
    });
}

fn wifi_state(m: &Machine) -> WifiState {
    WifiState::decode(m.radio_module_state("wifi").unwrap_or_default())
        .expect("the wifi module state decodes")
}

/// The three scripted access points of an earlier QEMU reference run, whose SSIDs, channels,
/// RSSIs, auth modes and `02:00:00` BSSIDs the probe prints as its `AP|` lines.
fn g2_world() -> Vec<WifiAp> {
    let ap = |ssid: &str, rssi: i8, channel: u8, authmode: u8, last: u8| WifiAp {
        ssid: ssid.to_string(),
        bssid: [0x02, 0x00, 0x00, 0x47, 0x32, last],
        rssi,
        channel,
        authmode,
        psk: Vec::new(),
    };
    vec![
        ap("G2-Alpha", -42, 1, 0, 0x01),
        ap("G2-Bravo", -60, 6, 0, 0x02),
        ap("G2-Charlie", -75, 11, 0, 0x03),
    ]
}

/// What the probe measures for each cycle, in ms: the wait for the aborted sweep's `SCAN_DONE`.
/// The device prints `ms=2`; under the default U4 wake mode the event posts after the worker's
/// 500 us hop, so the model reads 0 (the driver's abort time is class C, not fitted). Pinned so a
/// change to the delivery path cannot drift unnoticed.
const CYCLE_MS: [u32; 3] = [0, 0, 0];

/// The 48 ordered structured lines of the `scan3` run, as the prefix of each: 15 of the Wi-Fi
/// bring-up, 4 per scan cycle and 21 of the teardown and the BLE advertising tail.
///
/// They are an earlier QEMU reference run's lines (against a Python Wi-Fi model) corrected by the
/// class A capture `device-wifi_facts-20260917T193712Z`: a second `esp_wifi_scan_start` while a
/// sweep runs cancels it, posts `SCAN_DONE` status 1 with no access point and answers 0, not
/// `ESP_ERR_WIFI_STATE`. `scan3` starts its two scans back to back, so every cycle ends on a
/// cancelled sweep with no `AP|` line. The bring-up gains `EVT|WIFI_EVENT|id=43` before
/// `STA_START`, and the teardown the `SCAN_DONE` `esp_wifi_stop` posts for cycle 3's sweep.
fn expected_lines() -> Vec<String> {
    let mut exp: Vec<String> = [
        "HEAP|boot.app_main",
        "HEAP|boot.after_nvs",
        "HEAP|wifi.0_before_netif",
        "HEAP|wifi.1_after_netif_evloop",
        "HEAP|wifi.2_after_default_sta",
        "RC|deinit_before_init|12289",
        "RC|esp_wifi_init|0",
        "HEAP|wifi.3_after_init",
        "RC|esp_wifi_set_storage|0",
        "RC|esp_wifi_set_mode|0",
        // The prio-23 `wifi` worker posts the two start events and the prio-20 `sys_evt` task runs their
        // handlers before the prio-1 caller returns, so they precede the return code.
        "EVT|WIFI_EVENT|id=43",
        "EVT|WIFI_EVENT|id=2",
        "RC|esp_wifi_start|0",
        "RC|sta_start_seen|1",
        "HEAP|wifi.4_after_start",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for k in 1..=3 {
        exp.extend([
            "EVT|WIFI_EVENT|id=1".to_string(),
            "EVT|SCAN_DONE|status=1|number=0".to_string(),
            format!(
                "RC|scan|cycle={k}|start=0|busy=0|done=1|num_rc=0|num=0|rec_rc=0|records=0|ms={}",
                CYCLE_MS[k - 1]
            ),
            format!("HEAP|wifi.5_scan{k}"),
        ]);
    }
    exp.extend(
        [
            "RC|deinit_while_started|12291",
            // `esp_wifi_stop` cancels the sweep cycle 3 left running, and its SCAN_DONE is posted before
            // STA_STOP (`device-scan3-20260917T201219Z`).
            "EVT|WIFI_EVENT|id=1",
            "EVT|SCAN_DONE|status=1|number=0",
            "EVT|WIFI_EVENT|id=3",
            "RC|esp_wifi_stop|0",
            "RC|sta_stop_seen|1",
            "HEAP|wifi.7_after_stop",
            "RC|esp_wifi_deinit|0",
            "HEAP|wifi.8_after_deinit",
            "RC|deinit_again|12289",
            "HEAP|ble.0_before",
            "RC|nimble_port_init|0",
            "HEAP|ble.1_after_port_init",
            "EVT|BLE_SYNC|rc=0",
            "RC|adv_start|0",
            "RC|ble_synced|1",
            "HEAP|ble.2_after_sync_adv",
            "RC|nimble_port_stop|0",
            "HEAP|ble.3_after_deinit",
            "HEAP|end",
            "PROBE DONE",
        ]
        .iter()
        .map(|s| (*s).to_string()),
    );
    exp
}

/// The structured lines of a console: `RC|`, `EVT|`, `AP|`, `HEAP|`, `TASK|` and the end marker.
fn structured(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| {
            line.starts_with("RC|")
                || line.starts_with("EVT|")
                || line.starts_with("AP|")
                || line.starts_with("HEAP|")
                || line.starts_with("TASK|")
                || *line == "PROBE DONE"
        })
        .map(str::to_string)
        .collect()
}

fn ordered(text: &str, exp: &[String]) -> (usize, Option<String>) {
    let mut i = 0;
    for line in structured(text) {
        if i < exp.len() && line.starts_with(&exp[i]) {
            i += 1;
        }
    }
    (i, exp.get(i).cloned())
}

/// The Wi-Fi console lines the blob and `wifi_init.c` print, which the HLE synthesizes through the
/// image's own `esp_log` (`specs/hle/idf-5.5.3/log-lines.toml`). Checked without timestamps.
const WIFI_LOG_LINES: [&str; 6] = [
    "pp: pp rom version: 74f9620",
    "wifi:wifi driver task: ",
    "wifi:wifi firmware version: 4df78f2",
    "wifi_init: WiFi RX IRAM OP enabled",
    "wifi:mode : sta (",
    "wifi:enable tsf",
];

fn assert_wifi_log(text: &str, test: &str, what: &str) {
    let mut lines = text.lines();
    for want in WIFI_LOG_LINES {
        assert!(
            lines.any(|line| line.contains(want)),
            "{test} {what}: the console has no `{want}` line; console:\n{text}"
        );
    }
    // The MAC line comes from the guest's own `esp_read_mac`, and a synthetic image's MAC has the
    // `02:00:00` placeholder prefix. Only the shape is asserted.
    assert!(
        text.contains("wifi:mode : sta (02:00:00:"),
        "{test} {what}: the station MAC line is the guest's own placeholder MAC"
    );
}

/// Asserts the whole `scan3` expectation over `text`, non-vacuously: 48 of 48 in order, the return
/// codes in their order, the synthesized Wi-Fi lines and no `FAIL` line.
fn assert_s3b(text: &str, test: &str, what: &str) {
    let exp = expected_lines();
    assert_eq!(exp.len(), 48, "the reference run has 48 ordered lines");
    let (matched, missing) = ordered(text, &exp);
    assert_eq!(
        matched,
        exp.len(),
        "{test} {what}: first missing `{}`; console:\n{text}",
        missing.unwrap_or_default()
    );
    // The codes in the order the probe reads them: the refused deinit before init, the `busy=` field
    // of each `RC|scan|cycle=...` line (0 on the device), the refused deinit while started and the
    // second refused deinit. A code field is its whole value, so `12289` cannot match inside a number.
    let codes: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("RC|"))
        .flat_map(|line| {
            let fields = line.split('|').map(|f| f.rsplit('=').next().unwrap_or(f));
            if line.contains("|busy=") {
                line.split('|')
                    .filter_map(|f| f.strip_prefix("busy="))
                    .collect::<Vec<_>>()
            } else {
                fields
                    .filter(|field| ["12289", "12290", "12291", "12294"].contains(field))
                    .collect::<Vec<_>>()
            }
        })
        .collect();
    assert_eq!(
        codes,
        ["12289", "0", "0", "0", "12291", "12289"],
        "{test} {what}: the codes of the scan erratum in order, 0 once per cycle"
    );
    assert!(
        !text.contains("12294"),
        "{test} {what}: no call answers ESP_ERR_WIFI_STATE any more: a second scan_start \
         cancels the running sweep and answers 0 (device capture \
         `device-wifi_facts-20260917T193712Z`)"
    );
    assert_wifi_log(text, test, what);
    assert!(
        !text.contains("FAIL"),
        "{test} {what}: the probe printed a FAIL line:\n{text}"
    );
}

/// The corpus `scan3` flash image and app ELF, or `None` after a printed skip.
fn scan3_files(test: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let image = common::corpus_file_or_skip(test, "scan3", "merged-binary.bin")?;
    let elf = common::corpus_file_or_skip(test, "scan3", "radio_scan3probe.elf")?;
    Some((
        std::fs::read(image).expect("the verified image is readable"),
        std::fs::read(elf).expect("the verified ELF is readable"),
    ))
}

/// Both executors, as the m8 BLE rows run them.
const BOTH: [Executor; 2] = [Executor::Engine, Executor::Reference];

/// A `scan3` machine with the three scripted access points of [`g2_world`] already in its air,
/// journaled through `EnvChange::WifiAps` as the `env` command does.
fn scan3_machine(flash: &[u8], elf: &[u8]) -> Machine {
    let mut m = machine(flash, elf, MachineConfig::default());
    m.input(At::Now, InputEvent::Env(EnvChange::WifiAps(g2_world())))
        .expect("the world change is journaled");
    m
}

fn run_scan3(m: &mut Machine, test: &str) -> Console {
    let mut con = Console::from(0);
    run_to_line(m, &mut con, "PROBE DONE", 20_000, test);
    con
}

/// `scan3` runs its whole Wi-Fi half against the Wi-Fi HLE with the scripted access points in its
/// air and prints the 48 ordered structured lines (timestamps excluded): the return codes in order
/// (12289, then 0 for the second `esp_wifi_scan_start` of each cycle, then 12291 and 12289), the
/// Wi-Fi console lines the blob prints and the BLE advertising tail, with no `FAIL` line.
#[test]
fn t1_m12_scan3_prints_the_48_ordered_lines_of_s3b() {
    let test = "t1_m12_scan3_prints_the_48_ordered_lines_of_s3b";
    let Some((flash, elf)) = scan3_files(test) else {
        return;
    };
    // The whole Wi-Fi path runs under both executors and gives the same console.
    let mut consoles = Vec::new();
    for executor in BOTH {
        let mut m = scan3_machine(&flash, &elf);
        m.set_executor(executor);
        let con = run_scan3(&mut m, test);
        assert_s3b(&con.text(), test, &format!("{executor:?}"));
        assert_eq!(
            m.hle_binding()
                .record
                .features
                .get("wifi")
                .map(|f| f.receipt_word()),
            Some("bound"),
            "{test} {executor:?}: the wifi module bound against scan3"
        );
        assert!(
            m.radio_module_state("wifi").is_some(),
            "{test} {executor:?}: the wifi module ran"
        );
        let st = wifi_state(&m);
        assert_eq!(
            st.state,
            DriverState::Uninit,
            "{test} {executor:?}: the probe deinited"
        );
        // The scripted air is the machine's, not the driver's, so a deinit keeps it
        // (`WifiHost::finish_deinit`).
        assert_eq!(
            st.aps.iter().map(|ap| ap.ssid.as_str()).collect::<Vec<_>>(),
            ["G2-Alpha", "G2-Bravo", "G2-Charlie"],
            "{test} {executor:?}: the env route reached the module"
        );
        consoles.push((executor, con.text()));
    }
    assert_eq!(
        consoles[0].1, consoles[1].1,
        "{test}: the two executors print the same console"
    );
}

/// Two mid-run saves of `scan3` restore and finish the probe: the `wifi` worker inside an aborted
/// scan's `SCAN_DONE` delivery with the replacement sweep outstanding, and `main` parked inside
/// `esp_wifi_stop`. Each restores into a fresh machine that has not journaled the scripted air, so
/// the world comes back from the snapshot alone; the concatenated console is byte-identical to the
/// uninterrupted run, and the final `state_hash` is equal.
#[test]
fn t1_m12_a_save_mid_scan_and_mid_stop_restore_to_the_same_run() {
    let test = "t1_m12_a_save_mid_scan_and_mid_stop_restore_to_the_same_run";
    if let Some(file) = determinism::child_snapshot() {
        restore_child(test, &file);
        return;
    }
    let Some((flash, elf)) = scan3_files(test) else {
        return;
    };

    let mut whole = scan3_machine(&flash, &elf);
    let uninterrupted = run_scan3(&mut whole, test).text();
    assert_s3b(&uninterrupted, test, "uninterrupted");
    let final_hash = whole.state_hash();

    type Point = (&'static str, fn(&mut Machine, &Console) -> bool);
    let points: [Point; 2] = [
        ("the aborted scan's SCAN_DONE in flight", |m, _| {
            let st = wifi_state(m);
            // `pending` is what the worker has been given and not yet done, `outbox` what waits for its
            // instant: either is work inside the scan path.
            st.state == DriverState::Scanning && !(st.pending.is_empty() && st.outbox.is_empty())
        }),
        ("main blocked in esp_wifi_stop", |m, con| {
            let text = con.text();
            // The driver has left STARTED for INIT while the caller is still parked: exactly the window
            // `esp_wifi_stop` parks its caller in.
            let st = wifi_state(m);
            st.state == DriverState::Init
                && text.contains("RC|deinit_while_started|12291")
                && !text.contains("RC|esp_wifi_stop|0")
        }),
    ];

    let mut snaps: Vec<(String, Vec<u8>)> = Vec::new();
    let mut wants: Vec<(String, String)> = Vec::new();
    // Each instant is saved and restored under both executors, and the fresh-process leg below
    // repeats every restore in a process of its own.
    for ((index, (what, reached)), executor) in points
        .into_iter()
        .enumerate()
        .flat_map(|point| BOTH.map(|executor| (point, executor)))
    {
        let what = &format!("{what} ({executor:?})");
        let mut m = scan3_machine(&flash, &elf);
        m.set_executor(executor);
        let mut before = Console::from(0);
        run_while(&mut m, &mut before, 20_000, test, what, reached);
        let cursor = m.io().serial_ring(SerialStream::UsjTx).head();
        let state = wifi_state(&m);
        let bytes = m
            .snapshot(SnapOpts::default())
            .to_bytes()
            .expect("a machine snapshot serializes");

        let mut restored = machine(&flash, &elf, MachineConfig::default());
        restored.set_executor(executor);
        restored
            .restore(&Snapshot::from_bytes(&bytes).expect("the snapshot parses"))
            .expect("the snapshot restores");
        assert_eq!(
            restored.state_hash(),
            m.state_hash(),
            "{test} {what}: the restore is the same state"
        );
        assert_eq!(
            wifi_state(&restored),
            state,
            "{test} {what}: the wifi module state came back"
        );
        assert_eq!(
            wifi_state(&restored)
                .aps
                .iter()
                .map(|ap| ap.ssid.as_str())
                .collect::<Vec<_>>(),
            ["G2-Alpha", "G2-Bravo", "G2-Charlie"],
            "{test} {what}: the scripted air came back from the snapshot, not from a journal"
        );

        let mut after = Console::from(cursor);
        run_to_line(&mut restored, &mut after, "PROBE DONE", 20_000, test);
        let both = format!("{}{}", before.text(), after.text());
        assert_eq!(
            both, uninterrupted,
            "{test} {what}: the concatenated console differs from the uninterrupted run"
        );
        assert_s3b(&both, test, what);
        assert_eq!(
            restored.state_hash(),
            final_hash,
            "{test} {what}: the restored run ends in the same state"
        );
        // The save was taken in the middle, so both halves carry lines.
        assert!(
            !before.text().is_empty() && !after.text().is_empty(),
            "{test} {what}: the save must split the console"
        );
        snaps.push((
            format!("{index}-{cursor}-{}", executor_tag(executor)),
            bytes,
        ));
        wants.push((what.clone(), child_answer_text(&after.text(), &restored)));
    }

    // Not a formality: `WifiProfile::load` and `MagicPcs::from_spec` are process-global, so only a
    // fresh process rebuilds them from the spec.
    let answers = determinism::fresh_process(test, &snaps);
    assert_eq!(answers.len(), wants.len());
    for ((what, want), got) in wants.iter().zip(&answers) {
        assert_eq!(
            got, want,
            "{test} {what}: the fresh-process restore differs from the in-process one"
        );
        println!("RAN {test}: {what} restores identically in a fresh process");
    }
}

/// The answer a restored run gives, whichever process ran it: the console after the restore and
/// the final `state_hash`.
fn child_answer_text(after: &str, m: &Machine) -> String {
    format!(
        "{}\n---\n{}",
        pemu_machine::determinism::hex(&m.state_hash()),
        after
    )
}

fn executor_tag(executor: Executor) -> &'static str {
    match executor {
        Executor::Engine => "engine",
        Executor::Reference => "reference",
    }
}

/// In a child of [`determinism::fresh_process`]: restore the snapshot, run the probe to its end and
/// answer as the parent's restored machine did. The tag is
/// `<point index>-<console cursor>-<executor>`.
fn restore_child(test: &str, file: &std::path::Path) {
    let tag = determinism::child_tag(file);
    let mut parts = tag.split('-').skip(1);
    let cursor: u64 = parts
        .next()
        .and_then(|cursor| cursor.parse().ok())
        .expect("the tag carries the console cursor");
    let executor = match parts.next() {
        Some("reference") => Executor::Reference,
        _ => Executor::Engine,
    };
    let Some((flash, elf)) = scan3_files(test) else {
        return;
    };
    let bytes = std::fs::read(file).expect("the parent wrote the snapshot");
    let mut m = machine(&flash, &elf, MachineConfig::default());
    m.set_executor(executor);
    m.restore(&Snapshot::from_bytes(&bytes).expect("the snapshot parses"))
        .expect("the snapshot restores");
    let mut after = Console::from(cursor);
    run_to_line(&mut m, &mut after, "PROBE DONE", 20_000, test);
    determinism::child_answer(file, &child_answer_text(&after.text(), &m));
}

/// The hooks are process state, so the tests that install them take turns.
static HOOKED: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn hooked_demo(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
    let bin = common::corpus_file_or_skip(test, "demo", "demo-merged.bin")?;
    let elf = common::corpus_file_or_skip(test, "demo", "FoloToy-AI-Passport.elf")?;
    let guard = HOOKED.lock().unwrap_or_else(|e| e.into_inner());
    static WORLD: std::sync::OnceLock<(Arc<Vec<u8>>, Arc<pemu_host::hooks::ElfContext>)> =
        std::sync::OnceLock::new();
    let (image, context) = WORLD.get_or_init(|| {
        let elf = std::fs::read(elf).expect("the verified corpus ELF is readable");
        (
            Arc::new(std::fs::read(bin).expect("the verified corpus image is readable")),
            Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses")),
        )
    });
    let (image, context) = (Arc::clone(image), Arc::clone(context));
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            "demo" => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(std::env::temp_dir().join("pemu-m12-no-audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == "demo").then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::new(None, vec![]),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(None);
    Some(guard)
}

fn call(
    name: &str,
    args: serde_json::Value,
) -> Result<pemu_api::output::Output, pemu_api::error::ApiError> {
    let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args)
}

/// Every console line a `serial read` answered, in order. The read is refused when the shaper
/// elided anything, so a check over these lines is never over a fraction of them.
fn console_lines(out: &pemu_api::output::Output) -> Vec<String> {
    let serial = &out.json["serial"];
    assert_eq!(
        serial["elided"], 0,
        "the console read elided lines: {serial}"
    );
    let text = |v: &serde_json::Value| {
        v.as_array()
            .expect("an entry list")
            .iter()
            .map(|e| e.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<String>>()
    };
    let mut lines = text(&serial["head"]);
    lines.extend(text(&serial["tail"]));
    lines
}

/// The three access points of the card, as `demo_wifi.c` draws them: `"<rssi>  <ssid>  ch<n>"`,
/// strongest first.
const DEMO_AP_ROWS: [&str; 3] = [
    "-42  G2-Alpha  ch1",
    "-60  G2-Bravo  ch6",
    "-75  G2-Charlie  ch11",
];

/// The corpus `demo` image with its radios enabled reaches its Wi-Fi card from the menu (four
/// `down` clicks and one `ok`), and the card lists the three scripted access points: the `ui` tree
/// carries `3 APs` and one row per access point with its RSSI, SSID and channel, strongest first.
///
/// `demo_wifi.c` `show_scan_results` logs nothing on success, so the SSIDs are on the screen only.
/// The console carries the synthesized bring-up and no failure line: `demo_wifi.c` prints
/// `E (t) demo_wifi: Wi-Fi ...: <code>` for any failed `esp_wifi_*` call.
#[test]
fn t1_m12_the_demo_wifi_card_lists_the_three_scripted_access_points() {
    let test = "t1_m12_the_demo_wifi_card_lists_the_three_scripted_access_points";
    let id = test.to_string();
    let Some(_hooks) = hooked_demo(test) else {
        return;
    };
    let start = call("start", serde_json::json!({"fw": "demo", "boot": "none"}))
        .expect("the demo image starts");
    let inst = start.json["instance"].as_str().expect("an id").to_owned();
    let bound = start.receipt.hle.bound.clone().unwrap_or_default();
    assert!(
        bound.contains("wifi"),
        "{id}: the demo image binds the wifi module, not `{bound}`"
    );

    // The Wi-Fi card is the fifth of the seven cards (`main.c` `DEMOS[]`).
    let boot = call(
        "run",
        serde_json::json!({"instance": inst, "until": "serial:/main: 就绪/", "timeout": "3s"}),
    )
    .unwrap_or_else(|e| panic!("{id}: the demo settles its menu: {e:?}"));
    assert_eq!(boot.json["status"], "matched", "{id}: {}", boot.json);
    let ready = boot.json["match"]["text"]
        .as_str()
        .expect("the matched line")
        .to_owned();
    let cursor = boot.json["serial"]["next_cursor"]
        .as_u64()
        .expect("a console cursor");
    let aps = g2_world();
    call(
        "env",
        serde_json::json!({"instance": inst, "wifi": {"aps": aps.iter().map(|ap| serde_json::json!({
            "ssid": ap.ssid,
            "bssid": format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                ap.bssid[0], ap.bssid[1], ap.bssid[2], ap.bssid[3], ap.bssid[4], ap.bssid[5]),
            "rssi": ap.rssi,
            "channel": ap.channel,
            "auth": ap.authmode,
        })).collect::<Vec<_>>()}}),
    )
    .unwrap_or_else(|e| panic!("{id}: the air is scripted through `env`: {e:?}"));
    for _ in 0..4 {
        call(
            "input",
            serde_json::json!({"instance": inst, "button": "down", "action": "click"}),
        )
        .unwrap_or_else(|e| panic!("{id}: a `down` click: {e:?}"));
    }
    call(
        "input",
        serde_json::json!({"instance": inst, "button": "ok", "action": "click"}),
    )
    .unwrap_or_else(|e| panic!("{id}: the `ok` click: {e:?}"));

    // The scan dwells 11 channels at 120 ms, so the card waits about 1.3 s of the run.
    let mut tree = String::new();
    for _ in 0..6 {
        call("run", serde_json::json!({"instance": inst, "for": "1s"}))
            .unwrap_or_else(|e| panic!("{id}: the card runs: {e:?}"));
        tree = call("ui", serde_json::json!({"instance": inst}))
            .unwrap_or_else(|e| panic!("{id}: the tree reads: {e:?}"))
            .json["text"]
            .as_str()
            .expect("a rendered tree")
            .to_owned();
        if tree.contains("APs  |  OK: RESCAN") {
            break;
        }
    }

    assert!(
        tree.contains("\"WI-FI SCAN\""),
        "{id}: the five clicks did not open the Wi-Fi card:\n{tree}"
    );
    assert!(
        tree.contains("\"3 APs  |  OK: RESCAN\""),
        "{id}: the card did not count the three scripted access points:\n{tree}"
    );
    let rows = tree
        .lines()
        .find(|l| l.contains("G2-Alpha"))
        .unwrap_or_else(|| panic!("{id}: no access point row on the card:\n{tree}"));
    assert!(
        rows.contains(&DEMO_AP_ROWS.join("\\n")),
        "{id}: the rows are not the three scripted access points strongest first: {rows}"
    );

    assert!(
        ready.contains("main: 就绪:Display=1 Button=1 Audio=1 Battery=1"),
        "{id}: the boot did not reach the menu: {ready}"
    );
    let console = call(
        "serial",
        serde_json::json!({"instance": inst, "op": "read", "cursor": cursor}),
    )
    .unwrap_or_else(|e| panic!("{id}: the console reads: {e:?}"));
    let lines = console_lines(&console);
    let bad: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("demo_wifi") || l.contains("Wi-Fi ") || l.starts_with("E ("))
        .collect();
    assert!(
        bad.is_empty(),
        "{id}: the card printed a Wi-Fi failure: {bad:?}"
    );
    assert_wifi_log(&lines.join("\n"), test, "the demo card");

    // The card could not have invented these SSIDs, so the `env` air really reached the module.
    println!(
        "RAN {test} demo: WI-FI SCAN card, 3 APs, rows {DEMO_AP_ROWS:?}; \
         the card printed {} console line(s), the Wi-Fi bring-up and no failure",
        lines.len()
    );
    call("stop", serde_json::json!({"instance": inst})).ok();
}

/// The corpus builds that link the Wi-Fi driver, with the files each one is made of, and `pk`,
/// which links none of it.
const WIFI_BUILDS: [(&str, &str, &str); 4] = [
    ("scan3", "merged-binary.bin", "radio_scan3probe.elf"),
    ("probe2", "probe2-merged.bin", "radio_heapprobe.elf"),
    ("demo", "demo-merged.bin", "FoloToy-AI-Passport.elf"),
    (
        "official",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
];

fn wifi_word(m: &Machine) -> Option<&'static str> {
    m.hle_binding()
        .record
        .features
        .get("wifi")
        .map(|status| status.receipt_word())
}

/// Applies `edit` to `len` bytes at `offset` into `symbol`, in copies of the merged flash image and
/// the ELF, keeping the app image checksum and SHA-256 valid, so the bootloader still loads it and
/// only the bytes the binding reads differ. As `m8.rs` does for the BLE binding.
fn patch_symbol(
    flash: &mut [u8],
    elf_file: &mut [u8],
    symbol: &str,
    offset: u32,
    len: usize,
    edit: impl Fn(&mut [u8]),
) {
    use pemu_loader::esp_image::MergedImage;
    let elf = ElfInfo::parse(elf_file).expect("the ELF parses");
    let addr = elf.symbols.addr_of(symbol).expect("the symbol is linked") + offset;
    let section = elf
        .sections
        .iter()
        .find(|s| s.is_alloc() && s.has_bits() && addr >= s.addr && u64::from(addr) < s.end())
        .expect("a section holds the bytes");
    let at = (section.offset + (addr - section.addr)) as usize;
    edit(&mut elf_file[at..at + len]);
    let merged = MergedImage::parse(flash).expect("the image parses");
    let (_, app) = merged.app.expect("the image has a boot app");
    let seg = app
        .segments
        .iter()
        .find(|s| addr >= s.load_addr && addr + len as u32 <= s.load_addr + s.len)
        .expect("a segment loads the bytes");
    let at = seg.data_offset + (addr - seg.load_addr) as usize;
    let old = flash[at..at + len].to_vec();
    edit(&mut flash[at..at + len]);
    let xor = old
        .iter()
        .zip(&flash[at..at + len])
        .fold(0u8, |x, (o, n)| x ^ o ^ n);
    let end = app.offset + app.len;
    assert!(app.header.hash_appended, "the image appends its hash");
    flash[end - 33] ^= xor;
    let digest = pemu_loader::sha256(&flash[app.offset..end - 32]);
    flash[end - 32..end].copy_from_slice(&digest);
}

/// Every corpus build that links the Wi-Fi driver binds the module, and `pk`, which links none of
/// the hooks, stays `not linked` with its `esp_wifi_init` tripwire in place.
#[test]
fn t1_m12_the_wifi_module_binds_every_build_that_links_it() {
    let test = "t1_m12_the_wifi_module_binds_every_build_that_links_it";
    for (id, bin, elf) in WIFI_BUILDS {
        let Some(bin) = common::corpus_file_or_skip(test, id, bin) else {
            return;
        };
        let Some(elf) = common::corpus_file_or_skip(test, id, elf) else {
            return;
        };
        let m = machine(
            &std::fs::read(bin).expect("the verified image is readable"),
            &std::fs::read(elf).expect("the verified ELF is readable"),
            MachineConfig::default(),
        );
        assert_eq!(
            wifi_word(&m),
            Some("bound"),
            "{test}: the wifi module binds {id}: {:?}",
            m.hle_binding().mismatches
        );
    }
    let Some(bin) = common::corpus_file_or_skip(test, "pk", "FoloToy-AI-Passport-8MB.bin") else {
        return;
    };
    let Some(elf) = common::corpus_file_or_skip(test, "pk", "FoloToy-AI-Passport.elf") else {
        return;
    };
    let m = machine(
        &std::fs::read(bin).expect("the verified image is readable"),
        &std::fs::read(elf).expect("the verified ELF is readable"),
        MachineConfig::default(),
    );
    assert_eq!(
        wifi_word(&m),
        Some("not linked"),
        "{test}: pk links no Wi-Fi hook"
    );
    println!("RAN {test}: 4 builds bound, pk not linked");
}

/// Fail-closed: an image whose `esp_wifi_init` skeleton or `idf_ver` differs does not bind, and the
/// `DisabledFeature` tripwire at `esp_wifi_init` stays armed, so the run stops there instead of
/// running a guessed handler.
#[test]
fn t1_m12_a_changed_image_refuses_the_module_and_keeps_the_tripwire() {
    let test = "t1_m12_a_changed_image_refuses_the_module_and_keeps_the_tripwire";
    let Some((flash, elf)) = scan3_files(test) else {
        return;
    };
    type Patch = fn(&mut Vec<u8>, &mut Vec<u8>);
    let patches: [(&str, Patch); 2] = [
        ("an inverted first 32 bytes", |f, e| {
            patch_symbol(f, e, "esp_wifi_init", 0, 32, |bytes| {
                for b in bytes {
                    *b ^= 0xFF;
                }
            });
        }),
        ("idf_ver v5.5.2", |f, e| {
            patch_symbol(f, e, "esp_app_desc", 112, 32, |b| {
                b.fill(0);
                b[..6].copy_from_slice(b"v5.5.2");
            });
        }),
    ];
    for (what, patch) in patches {
        let (mut flash, mut elf) = (flash.clone(), elf.clone());
        patch(&mut flash, &mut elf);
        let mut m = machine(&flash, &elf, MachineConfig::default());
        assert_eq!(
            wifi_word(&m),
            Some("unsupported image"),
            "{test} {what}: {:?}",
            m.hle_binding().mismatches
        );
        let entry = m
            .assets()
            .app_elf
            .as_ref()
            .and_then(|e| e.symbols.addr_of("esp_wifi_init"))
            .expect("scan3 links esp_wifi_init");
        assert!(
            m.tripwires().at(entry).is_some(),
            "{test} {what}: the tripwire stays armed"
        );
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(2_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        let StopReason::Tripwire(report) = &out.reason else {
            panic!("{test} {what}: the run ended {:?}", out.reason);
        };
        assert_eq!(
            (report.pc, report.feature),
            (entry, Some("wifi")),
            "{test} {what}"
        );
    }
    println!("RAN {test}: 2 refusals, the tripwire kept");
}

/// The scripted air is one journaled input: two machines given the same `EnvChange::WifiAps` at
/// the same instant reach an equal `state_hash`; one given no air does not.
#[test]
fn t1_m12_a_replay_of_the_journaled_air_reaches_the_same_state() {
    let test = "t1_m12_a_replay_of_the_journaled_air_reaches_the_same_state";
    let Some((flash, elf)) = scan3_files(test) else {
        return;
    };
    let run_to_first_scan = |m: &mut Machine| {
        let mut con = Console::from(0);
        run_to_line(m, &mut con, "RC|scan|cycle=1", 20_000, test);
        m.state_hash()
    };
    let first = run_to_first_scan(&mut scan3_machine(&flash, &elf));
    let replay = run_to_first_scan(&mut scan3_machine(&flash, &elf));
    assert_eq!(first, replay, "{test}: the journal replays to one state");
    let empty = run_to_first_scan(&mut machine(&flash, &elf, MachineConfig::default()));
    assert_ne!(
        first, empty,
        "{test}: not vacuous, an air with no access point is another state"
    );
    println!("RAN {test}: the replay matches and an empty air does not");
}

/// Every `EnvChange` the machine routes has a named arm, so the Wi-Fi air and a scripted BLE
/// central reach their own modules. `scan3` binds both, so both apply and none is unapplied.
#[test]
fn t1_m12_the_wifi_air_and_a_ble_central_script_both_apply() {
    let test = "t1_m12_the_wifi_air_and_a_ble_central_script_both_apply";
    let Some((flash, elf)) = scan3_files(test) else {
        return;
    };
    let mut m = machine(&flash, &elf, MachineConfig::default());
    let mut con = Console::from(0);
    run_to_line(&mut m, &mut con, "RC|esp_wifi_init|0", 20_000, test);
    m.input(At::Now, InputEvent::Env(EnvChange::WifiAps(g2_world())))
        .expect("the world change is journaled");
    m.input(
        At::Now,
        InputEvent::Env(EnvChange::BleCentral {
            script: pemu_radio::ble::central::encode_script(&[
                pemu_radio::ble::central::Step::Scan { ms: 50 },
            ]),
        }),
    )
    .expect("the central script is journaled");
    run_to_line(&mut m, &mut con, "RC|scan|cycle=1", 20_000, test);
    assert_eq!(
        m.unapplied_inputs(),
        0,
        "{test}: both journaled inputs reached their module"
    );
    assert_eq!(
        wifi_state(&m)
            .aps
            .iter()
            .map(|ap| ap.ssid.as_str())
            .collect::<Vec<_>>(),
        ["G2-Alpha", "G2-Bravo", "G2-Charlie"],
        "{test}: the wifi module took the air"
    );
    println!("RAN {test}: both inputs applied, 0 unapplied");
}

// The RX buffer ledger, as a unit test of the bound Wi-Fi module's `ModuleHost`, answering the
// nested calls the way the guest's code would. The module's own file carries the finer cases.

/// The station's RX callback address and the blocks the scripted allocator hands out. Neither is
/// an address the image uses; the handlers only pass them along.
const E6_RXCB: u32 = 0x4203_5000;
const E6_HEAP: u32 = 0x3FC9_8000;
const E6_STRIDE: u32 = 0x800;

/// A guest of registers and sparse memory: the Wi-Fi handlers are step machines over a
/// `GuestView`, so a unit test of one is this plus answers to its nested calls.
#[derive(Default)]
struct E6Guest {
    x: [u32; 32],
    mem: std::collections::BTreeMap<u32, u8>,
    now: VTime,
    sched: pemu_core::sched::Scheduler,
}

impl pemu_hle::guest_call::GuestView for E6Guest {
    fn draw_entropy(&mut self, stream: pemu_core::rng::RngStream, out: &mut [u8]) {
        pemu_core::rng::DetRng::new(7)
            .stream(stream)
            .fill_bytes(out);
    }
    fn reg(&self, r: u8) -> u32 {
        self.x[usize::from(r)]
    }
    fn set_reg(&mut self, r: u8, v: u32) {
        self.x[usize::from(r)] = v;
    }
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), pemu_rv32::trap::Trap> {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = *self.mem.get(&(addr + i as u32)).unwrap_or(&0);
        }
        Ok(())
    }
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), pemu_rv32::trap::Trap> {
        for (i, b) in data.iter().enumerate() {
            self.mem.insert(addr + i as u32, *b);
        }
        Ok(())
    }
    fn raise(&mut self, _s: pemu_core::irq_source::IrqSource, _level: bool) {}
    fn schedule(
        &mut self,
        at: VTime,
        key: pemu_core::sched::EventKey,
    ) -> pemu_core::sched::EventHandle {
        self.sched.schedule(self.now, at, key)
    }
    fn now(&self) -> VTime {
        self.now
    }
    fn symbol(&self, _name: &str) -> Option<u32> {
        None
    }
    fn current_task(&mut self) -> u32 {
        0x3FCB_0000
    }
    fn in_isr(&mut self) -> bool {
        false
    }
    fn scheduler_running(&mut self) -> bool {
        true
    }
}

/// The 33rd outstanding RX buffer is dropped, `DYNAMIC_RX_BUFFER_NUM` being 32.
///
/// The buffers are real guest heap blocks, so the test answers every `heap_caps_malloc` the module
/// makes and asserts that the 33rd frame never produces one. The capacity is the `count` of the
/// `[[heap]]` row of `specs/hle/idf-5.5.3/wifi.toml`, read from the four Wi-Fi corpus builds (class
/// C: no device measurement of a blob allocation exists). `scan3`'s real ELF makes the module the
/// one a real run binds.
#[test]
fn t1_m12_the_thirty_third_outstanding_rx_buffer_is_dropped() {
    use pemu_hle::guest_call::{A0, Arg, GuestView, HleAction};

    let test = "t1_m12_the_thirty_third_outstanding_rx_buffer_is_dropped";
    let Some((_, elf)) = scan3_files(test) else {
        return;
    };
    let info = ElfInfo::parse(&elf).expect("the verified ELF parses");
    let view = pemu_hle::binding::ImageView::new(&info, &elf);
    let module = pemu_radio::wifi::hle::module().expect("the Wi-Fi module is registered");
    assert!(
        module.bind_image(&view).is_ok(),
        "{test}: scan3 binds the Wi-Fi module"
    );
    let mut host = module.host(&view).expect("the bound image has a host");

    let mut g = E6Guest::default();
    let mut st: Vec<u8> = Vec::new();
    let sym = |name: &str| info.symbols.addr_of(name).expect("the ELF links it");

    // `esp_wifi_init`: answer the queues, the task, the MAC and the synthesized lines, so the module
    // is inited exactly as a run inits it.
    let cfg_at = 0x3FC9_1000u32;
    let mut cfg = vec![0u8; 152];
    cfg[144..148].copy_from_slice(&0x1F2F_3F4Fu32.to_le_bytes());
    // `dynamic_rx_buf_num` at offset 52, as `WIFI_INIT_CONFIG_DEFAULT()` fills it from `scan3`'s
    // configuration.
    cfg[52..56].copy_from_slice(&32u32.to_le_bytes());
    g.write(cfg_at, &cfg).expect("writable");
    g.x[usize::from(A0)] = cfg_at;
    let (mut hs, mut a) = host.enter(&mut st, pemu_hle::hooks::HandlerKind(0), &mut g);
    let mut handle = 0x1110u32;
    loop {
        let HleAction::Call { func, .. } = &a else {
            break;
        };
        let func = *func;
        // What the guest's code would answer: a fresh handle per queue, `pdPASS` and a handle for the
        // task, the station MAC, a timestamp for the log, and success for everything else.
        let (a0, scratch) = if func == sym("esp_read_mac") {
            (0, vec![0x02, 0x00, 0x00, 0x11, 0x22, 0x33, 0, 0])
        } else if func == sym("xTaskCreatePinnedToCore") {
            handle += 0x10;
            (1, handle.to_le_bytes().to_vec())
        } else if func == sym("xQueueGenericCreate") {
            handle += 0x10;
            (handle, Vec::new())
        } else if func == sym("esp_log_timestamp") {
            (7, Vec::new())
        } else {
            (0, Vec::new())
        };
        a = host.resume(
            &mut st,
            &mut hs,
            &mut g,
            pemu_hle::continuation::Resume::Returned { a0, a1: 0, scratch },
        );
    }
    assert_eq!(
        a,
        HleAction::Return { a0: 0, a1: 0 },
        "{test}: esp_wifi_init"
    );

    // `esp_wifi_internal_reg_rxcb(WIFI_IF_STA, cb)`: the netstack registers its receiver.
    g.x[usize::from(A0)] = 0;
    g.x[usize::from(A0) + 1] = E6_RXCB;
    let (_, a) = host.enter(&mut st, pemu_hle::hooks::HandlerKind(14), &mut g);
    assert_eq!(a, HleAction::Return { a0: 0, a1: 0 }, "{test}: reg_rxcb");

    let plan = pemu_radio::wifi::profile::WifiProfile::load().heap_plan(true);
    let row = plan
        .iter()
        .find(|r| r.label == pemu_radio::wifi::driver::RX_ROW)
        .expect("the corpus shape has an RX row");
    assert_eq!(
        row.count, 32,
        "{test}: CONFIG_ESP_WIFI_DYNAMIC_RX_BUFFER_NUM"
    );
    assert_eq!(
        WifiState::decode(&st).expect("state").rx_capacity,
        row.count,
        "{test}: the guest's own dynamic_rx_buf_num is the capacity, and the row states it"
    );
    let capacity = row.count as usize;
    let frames: Vec<Vec<u8>> = (0..capacity + 1).map(|i| vec![i as u8; 64 + i]).collect();

    let mut state = WifiState::decode(&st).expect("the module state decodes");
    let wifi = pemu_radio::wifi::driver::WifiHost::new(pemu_radio::wifi::hle::WIFI_MODULE, &view)
        .expect("a host for the frames");
    for frame in &frames {
        wifi.receive_frame(&mut state, &mut g, 0, frame);
    }
    st = state.encode();
    g.now = VTime::from_us(g.now.as_us() + 500);
    let events = host.on_timer(&mut st, 0, &mut g);
    host.deliver(&mut st, pemu_hle::magic::MagicKind::WifiWorker, events);

    let mut lent: Vec<(u32, u32)> = Vec::new();
    let mut next = E6_HEAP;
    let worker = pemu_hle::core::worker_handler(pemu_hle::magic::MagicKind::WifiWorker);
    let (mut hs, mut a) = host.enter(&mut st, worker, &mut g);
    loop {
        match &a {
            HleAction::Park => break,
            HleAction::Call { func, args, .. } if *func == sym("heap_caps_malloc") => {
                let at = next;
                next += E6_STRIDE;
                assert_eq!(
                    args[1],
                    Arg::Val(pemu_radio::heap_ledger::LEDGER_CAPS),
                    "{test}: an RX buffer is internal, byte-addressable RAM"
                );
                a = host.resume(
                    &mut st,
                    &mut hs,
                    &mut g,
                    pemu_hle::continuation::Resume::Returned {
                        a0: at,
                        a1: 0,
                        scratch: Vec::new(),
                    },
                );
            }
            HleAction::Call { func, args, .. } if *func == E6_RXCB => {
                let val = |i: usize| match args[i] {
                    Arg::Val(v) => v,
                    ref other => panic!("{test}: the callback takes values, got {other:?}"),
                };
                assert_eq!(val(0), val(2), "{test}: the buffer is its own `eb` handle");
                lent.push((val(0), val(1)));
                a = host.resume(
                    &mut st,
                    &mut hs,
                    &mut g,
                    pemu_hle::continuation::Resume::Returned {
                        a0: 0,
                        a1: 0,
                        scratch: Vec::new(),
                    },
                );
            }
            other => panic!("{test}: unexpected worker action {other:?}"),
        }
    }

    assert_eq!(
        lent.len(),
        capacity,
        "{test}: 32 frames reach the callback and the 33rd does not"
    );
    let state = WifiState::decode(&st).expect("the module state decodes");
    assert_eq!(state.rx_delivered, capacity as u64, "{test}");
    assert_eq!(state.rx_dropped, 1, "{test}: exactly the 33rd frame");
    assert_eq!(
        state.ledger.blocks.len(),
        capacity,
        "{test}: 32 buffers are outstanding"
    );
    assert_eq!(
        state.ledger.refused, 0,
        "{test}: the allocator was never asked for a 33rd buffer"
    );
    assert_eq!(
        next,
        E6_HEAP + E6_STRIDE * capacity as u32,
        "{test}: exactly 32 blocks came out of the allocator"
    );
    for (i, (addr, len)) in lent.iter().enumerate() {
        assert_eq!(*len as usize, frames[i].len(), "{test}: frame {i}");
        let mut back = vec![0u8; frames[i].len()];
        g.read(*addr, &mut back).expect("readable");
        assert_eq!(back, frames[i], "{test}: frame {i} is in the buffer");
    }

    // The 33rd frame is delivered once the netstack gives one buffer back: the capacity is a lifetime
    // (`esp_wifi_internal_free_rx_buffer`).
    g.x[usize::from(A0)] = lent[0].0;
    let (mut hs, a) = host.enter(&mut st, pemu_hle::hooks::HandlerKind(16), &mut g);
    let HleAction::Call { func, .. } = &a else {
        panic!("{test}: the buffer goes back to the image's own allocator, got {a:?}");
    };
    assert_eq!(*func, sym("heap_caps_free"), "{test}");
    host.resume(
        &mut st,
        &mut hs,
        &mut g,
        pemu_hle::continuation::Resume::Returned {
            a0: 0,
            a1: 0,
            scratch: Vec::new(),
        },
    );
    let state = WifiState::decode(&st).expect("the module state decodes");
    assert_eq!(state.ledger.blocks.len(), capacity - 1, "{test}");
    assert_eq!(state.rx_freed, 1, "{test}");

    // `inspect heap` sees them as guest heap blocks of this module, labelled and classed.
    let report = host.heap_ledger(&st);
    assert_eq!(report.len(), capacity - 1, "{test}");
    assert!(
        report.iter().all(|b| b.module == "wifi"
            && b.label == pemu_radio::wifi::driver::RX_ROW
            && b.class == "C"
            && b.caps == pemu_radio::heap_ledger::LEDGER_CAPS),
        "{test}: {report:?}"
    );
    println!("RAN {test}: {capacity} buffers lent, the 33rd dropped, one returned");
}

/// Every structured line of an earlier QEMU reference run appears in order when `probe2` runs,
/// including `RC|scan_start_block|0|num=3|records=3` and `EVT|DISCONNECTED|reason=201`, with three
/// differences where a device capture beats that run's Python Wi-Fi model:
///
/// 1. `id=43` then `id=2` where the reference run has `WIFI_EVENT id=2` alone
///    (`device-wifi_facts-20260917T193712Z`).
/// 2. Two disconnects, `reason=201` then `reason=36`, for one `esp_wifi_connect` at an SSID nothing
///    answers. The capture's summary reports only the last reason, so the test asserts the event
///    stream: both events, in order, nothing between.
/// 3. `EVT|WIFI_EVENT|id=1` and `EVT|SCAN_DONE|status=0|number=3` after the blocking scan
///    (`device-scanblock-20260917T222906Z`).
///
/// `RC|got_ip|0` prints the event-group bit, so 0 is "no address"; no association is needed.
#[test]
fn t1_m12_probe2_prints_the_lines_of_g2_p1_including_the_association() {
    let test = "t1_m12_probe2_prints_the_lines_of_g2_p1_including_the_association";
    let Some(bin) = common::corpus_file_or_skip(test, "probe2", "probe2-merged.bin") else {
        return;
    };
    let Some(elf) = common::corpus_file_or_skip(test, "probe2", "radio_heapprobe.elf") else {
        return;
    };
    let mut m = machine(
        &std::fs::read(bin).expect("the verified image is readable"),
        &std::fs::read(elf).expect("the verified ELF is readable"),
        MachineConfig::default(),
    );
    assert_eq!(
        wifi_word(&m),
        Some("bound"),
        "{test}: probe2 binds the module"
    );
    m.input_from(
        At::Now,
        pemu_core::journal::Origin::Agent,
        InputEvent::Env(EnvChange::WifiAps(g2_world())),
    )
    .expect("the scripted air applies");
    let mut con = Console::from(0);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(40_000)),
        max_insns: None,
        stops: StopSet::default(),
    });
    con.pump(&mut m);
    let text = con.text();

    let want = [
        "RC|nvs_flash_init|0",
        "RC|esp_wifi_init|0",
        "EVT|WIFI_EVENT|id=43",
        "EVT|WIFI_EVENT|id=2",
        "RC|esp_wifi_start|0",
        "EVT|SCAN_DONE|status=0|number=3",
        "RC|scan_start_block|0|num=3|records=3",
        "AP|G2-Alpha|ch=1|rssi=-42|auth=0",
        "AP|G2-Bravo|ch=6|rssi=-60|auth=0",
        "AP|G2-Charlie|ch=11|rssi=-75|auth=0",
        "RC|esp_wifi_connect|0",
        "EVT|WIFI_EVENT|id=5",
        "EVT|DISCONNECTED|reason=201|rssi=0",
        "EVT|WIFI_EVENT|id=5",
        "EVT|DISCONNECTED|reason=36|rssi=0",
        "RC|got_ip|0",
        "EVT|WIFI_EVENT|id=3",
        // The reference run's BLE tail: `ble_probe` runs after `wifi_probe` returns in the same
        // `app_main`.
        "RC|nimble_port_init|0",
        "EVT|BLE_SYNC|addr_type=0",
        "RC|adv_start|0",
        "RC|ble_synced|1",
        "PROBE DONE",
    ];
    let mut from = 0usize;
    for line in want {
        let at = text[from..]
            .find(line)
            .unwrap_or_else(|| panic!("{test}: `{line}` is missing or out of order:\n{text}"));
        from += at + line.len();
    }

    // Nothing associated, so no `EVT|CONNECTED` and no `EVT|GOT_IP` line can appear.
    for absent in ["EVT|CONNECTED|", "EVT|GOT_IP|"] {
        assert!(
            !text.contains(absent),
            "{test}: nothing answered the SSID, so `{absent}` must not appear:\n{text}"
        );
    }
    assert_eq!(
        text.matches("EVT|DISCONNECTED|").count(),
        2,
        "{test}: one connect at an absent SSID gives two disconnects, not one and not three:\n{text}"
    );
    // The two `_internal` entries are hooked, and a hooked name is not a blob tripwire.
    if let StopReason::Tripwire(report) = &out.reason {
        assert!(
            !report.detail.starts_with("esp_wifi_connect_internal")
                && !report.detail.starts_with("esp_wifi_disconnect_internal"),
            "{test}: the association half is hooked, so it cannot be a tripwire: {report:?}"
        );
    }
    println!(
        "RAN {test}: every structured line of the reference run appears, association included; \
         stop {:?}",
        out.reason
    );
}

/// The open SSID `probe_wifi_http` joins, the placeholder default of its `Kconfig.projbuild`: the
/// scripted access point of the virtual LAN, no real network.
const VIRTUAL_AP: &str = "passport-emu-virtual-ap";

/// The `probe_wifi_http` flash image and its unstripped build ELF, checked against
/// `tests/fw/manifest.toml`. The committed ELF is stripped, so it has no symbol a hook could bind
/// by; the `wifi.toml` rows were measured on the build ELF. `None` after a printed skip.
fn probe_wifi_http_files(test: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let official =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
    let probes = official
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join("probes");
    let read = |path: std::path::PathBuf, what: &str| match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            common::skip(test, &format!("{what} is not built (xtask probes)"));
            None
        }
    };
    let flash = read(
        probes.join("probe_wifi_http-8MB.bin"),
        "corpus/probes/probe_wifi_http-8MB.bin",
    )?;
    let elf = read(
        probes.join("build/probe_wifi_http/probe_wifi_http.elf"),
        "corpus/probes/build/probe_wifi_http/probe_wifi_http.elf",
    )?;
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fw/manifest.toml"),
    )
    .expect("the probe manifest");
    let pinned = |key: &str| {
        manifest
            .split("[[probe]]")
            .find(|block| block.contains("name = \"probe_wifi_http\""))
            .and_then(|block| {
                block.lines().find_map(|l| {
                    l.strip_prefix(&format!("{key} = \""))
                        .map(|v| v.trim_end_matches('"').to_string())
                })
            })
            .unwrap_or_else(|| panic!("tests/fw/manifest.toml pins probe_wifi_http's {key}"))
    };
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&flash),
        pinned("merged_sha256"),
        "{test}: the merged image is not the pinned build"
    );
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&elf),
        pinned("elf_sha256"),
        "{test}: the build ELF is not the pinned build"
    );
    Some((flash, elf))
}

/// One boot of `probe_wifi_http` run to its `STA_CONNECTED`: the machine, its console, and the two
/// instants in virtual microseconds: the return of `esp_wifi_connect` (found to the 100 us step)
/// and the post of `STA_CONNECTED`.
struct HttpRun {
    m: Machine,
    con: Console,
    connect_us: u64,
    connected_us: u64,
}

/// Boots `probe_wifi_http` with its open access point on the air and runs it to `STA_CONNECTED`,
/// or `None` after a printed skip.
fn probe_wifi_http_to_connected(test: &str) -> Option<HttpRun> {
    let (flash, elf) = probe_wifi_http_files(test)?;
    let mut m = machine(&flash, &elf, MachineConfig::default());
    assert_eq!(
        wifi_word(&m),
        Some("bound"),
        "{test}: probe_wifi_http binds the module: {:?}",
        m.hle_binding().mismatches
    );
    let ap = |ssid: &str, rssi: i8, channel: u8| WifiAp {
        ssid: ssid.to_string(),
        bssid: [0x02, 0x00, 0x00, 0x47, 0x32, channel],
        rssi,
        channel,
        authmode: 0,
        psk: Vec::new(),
    };
    m.input_from(
        At::Now,
        pemu_core::journal::Origin::Agent,
        InputEvent::Env(EnvChange::WifiAps(vec![
            ap("passportsim-other", -70, 11),
            ap(VIRTUAL_AP, -40, 6),
        ])),
    )
    .expect("the scripted air applies");
    let mut con = Console::from(0);
    run_while(&mut m, &mut con, 5_000, test, "esp_wifi_connect", |m, _| {
        wifi_state(m).state == DriverState::Connecting
    });
    let connect_us = m.now().as_us();
    run_while(&mut m, &mut con, 5_000, test, "STA_CONNECTED", |m, _| {
        wifi_state(m).posted_events == 3
    });
    let connected_us = m.now().as_us();
    Some(HttpRun {
        m,
        con,
        connect_us,
        connected_us,
    })
}

/// `probe_wifi_http` joins a scripted open access point, and `STA_CONNECTED` is posted with the
/// capture's timing plus the module's delivery path.
#[test]
fn t1_m12_probe_wifi_http_associates_with_the_capture_timing() {
    let test = "t1_m12_probe_wifi_http_associates_with_the_capture_timing";
    let Some(run) = probe_wifi_http_to_connected(test) else {
        return;
    };
    let connected_us = run.connected_us - run.connect_us;

    // Silicon: 212,415 us from the return of `esp_wifi_connect` to the handler. The model arms its
    // timer for exactly that instant (`wifi.toml` `connected_us`) and adds the worker's 500 us hop
    // (class C); under U4 nothing else is added: 212,915 us, found at 212,900 by the 100 us step.
    // Pinned whole, so a change to the delivery path cannot drift unnoticed.
    assert!(
        (212_415..=212_415 + 500 + STEP_US).contains(&connected_us),
        "{test}: STA_CONNECTED was posted {connected_us} us after the connect"
    );
    assert_eq!(
        connected_us, 212_900,
        "{test}: the delivery path of STA_CONNECTED moved"
    );

    let text = run.con.text();
    let want = [
        "PROBE|name=probe_wifi_http",
        "wifi:wifi firmware version: 4df78f2",
        "wifi:mode : sta",
        "wifi:enable tsf",
    ];
    let mut from = 0usize;
    for line in want {
        let at = text[from..]
            .find(line)
            .unwrap_or_else(|| panic!("{test}: `{line}` is missing or out of order:\n{text}"));
        from += at + line.len();
    }
    let state = wifi_state(&run.m);
    assert_eq!(state.state, DriverState::Connected, "{test}: associated");
    assert_eq!(
        state.peer.as_ref().map(|ap| ap.ssid.as_str()),
        Some(VIRTUAL_AP),
        "{test}: with the access point serving the configured SSID"
    );
    assert_eq!(
        state.posted_events, 3,
        "{test}: 43 and 2 from esp_wifi_start, then STA_CONNECTED, and no disconnect"
    );
    println!(
        "RAN {test}: associated, STA_CONNECTED posted {connected_us} us after the connect \
         (silicon 212415 to the handler)"
    );
}

/// `probe_wifi_http` connects to a scripted open access point, obtains an address by DHCP from the
/// virtual LAN, and an HTTP GET to the scripted service returns 200 with the expected body hash.
///
/// Everything in between is the guest's own code (lwIP DHCP, ARP, TCP, `esp_http_client`) over the
/// TX hook and the RX data plane, against the gateway of `pemu_radio::lan`. The `GOT_IP` delay is
/// pinned to this run, not to silicon: the virtual LAN answers within one worker hop.
#[test]
fn t1_m12_probe_wifi_http_leases_an_address_and_gets_200_with_the_body_hash() {
    let test = "t1_m12_probe_wifi_http_leases_an_address_and_gets_200_with_the_body_hash";
    let Some(HttpRun {
        mut m,
        mut con,
        connect_us,
        connected_us,
    }) = probe_wifi_http_to_connected(test)
    else {
        return;
    };
    // The RX capacity is the guest's own `dynamic_rx_buf_num`; read while the driver is up, since
    // the deinit at the end clears it.
    assert_eq!(
        wifi_state(&m).rx_capacity,
        32,
        "{test}: the guest's own dynamic_rx_buf_num"
    );
    // The probe's `esp_wifi_deinit` resets the driver's counters (only the air, the LAN and the
    // capture survive), so the last state seen while the driver was inited is kept for the
    // driver-side assertions.
    let last_up = std::cell::RefCell::new(WifiState::default());
    run_while(&mut m, &mut con, 60_000, test, "DONE|", |m, con| {
        let now = wifi_state(m);
        if now.state.inited() {
            *last_up.borrow_mut() = now;
        }
        con.text().contains("DONE|")
    });
    let up = last_up.into_inner();
    let text = con.text();
    let state = wifi_state(&m);
    assert_eq!(
        state.state,
        DriverState::Uninit,
        "{test}: the probe deinited"
    );
    let body = pemu_radio::lan::services::PROBE_BODY;
    let lines = [
        "WIFI|nvs=0|init=0|start=0|connect=0|placeholder_ssid=1|leased=1|attempts=1|last_reason=-1"
            .to_string(),
        "IP|ip=10.23.0.100|netmask=255.255.255.0|gw=10.23.0.1".to_string(),
        format!(
            "HTTP|path=/probe|rc=0|status=200|length={}|sha256={}",
            body.len(),
            pemu_testkit::corpus::sha256_hex(body)
        ),
        "RC|stop=0|deinit=0".to_string(),
        "DONE|name=probe_wifi_http|status=ok".to_string(),
    ];
    let mut from = 0usize;
    for line in &lines {
        let at = text[from..].find(line.as_str()).unwrap_or_else(|| {
            panic!(
                "{test}: `{line}` is missing or out of order; lan {:?}; console:\n{text}",
                state.lan.counters
            )
        });
        from += at + line.len();
    }
    assert!(!text.contains("FAIL|"), "{test}: no step failed:\n{text}");

    let lan = &state.lan;
    assert_eq!(lan.counters.dhcp_acks, 1, "{test}: {:?}", lan.counters);
    assert_eq!(lan.counters.http_answers, 1, "{test}: {:?}", lan.counters);
    // The client closed and the gateway answered with its own FIN. lwIP delays the client's last ACK
    // and the probe stops Wi-Fi at once, so that frame meets a stopped driver (`tx_refused`), as on
    // silicon. The gateway may therefore still hold the connection in LAST-ACK, and nothing else.
    assert!(
        lan.tcp.iter().all(|c| c.fin_received
            && c.fin_sent
            && c.state == pemu_radio::lan::gateway::tcp_state::LAST_ACK),
        "{test}: the client closed its connection"
    );
    assert_eq!(up.rx_dropped, 0, "{test}: no RX frame was dropped");
    assert_eq!(
        up.rx_alien_free, 0,
        "{test}: every freed RX buffer was the driver's"
    );
    assert_eq!(
        state.capture.records.len() as u64,
        up.tx_frames + up.rx_delivered,
        "{test}: the capture holds every frame sent and every frame delivered"
    );
    let pcap = state.capture.to_pcap();
    assert_eq!(&pcap[..4], &[0xd4, 0xc3, 0xb2, 0xa1], "{test}: a pcap file");

    // GOT_IP: `esp_netif`'s handler calls `esp_wifi_internal_set_sta_ip` when the event is delivered,
    // the instant the capture's probe stamped (1,013,973 us after its STA_CONNECTED).
    assert_ne!(
        up.sta_ip_us, 0,
        "{test}: GOT_IP reached esp_netif's handler"
    );
    let got_ip_us = up.sta_ip_us - connect_us;
    let after_connected_us = up.sta_ip_us - connected_us;
    // Pinned whole to this run: DISCOVER leaves with STA_CONNECTED, OFFER and ACK come back through
    // the worker, then the guest's address conflict check (three ARP probes at +0, +500 and +680 ms
    // and a ~320 ms wait) before lwIP binds. The run is 994,850 us, 1.9 % earlier than silicon: the
    // gateway's answer time is class C and not fitted.
    assert_eq!(
        after_connected_us, 994_850,
        "{test}: GOT_IP moved from its pinned instant ({got_ip_us} us after the connect)"
    );
    println!(
        "RAN {test}: leased 10.23.0.100 and GET /probe answered 200; GOT_IP {got_ip_us} us \
         after the connect and {after_connected_us} us after STA_CONNECTED (silicon 1226388 and \
         1013973); gateway sent its DHCP ACK {} us after the connect; {} frames out, {} refused, \
         {} in, {} captured; lan {:?}",
        lan.first_ack_us - connect_us,
        up.tx_frames,
        up.tx_refused,
        up.rx_delivered,
        state.capture.records.len(),
        lan.counters
    );
}

/// The host-time delay the host server takes before it answers.
const BRIDGE_DELAY_MS: u64 = 300;

/// The body the host server answers with, which proves the answer came from the host and not from
/// the virtual LAN.
const HOST_BODY: &[u8] = b"passportsim: answered by a server on the host\n";

/// Installs the host's real hooks over the `probe_wifi_http` image and its build ELF, under the
/// same lock as [`hooked_demo`].
fn hooked_probe_wifi_http(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
    let (flash, elf) = probe_wifi_http_files(test)?;
    let guard = HOOKED.lock().unwrap_or_else(|e| e.into_inner());
    let image = Arc::new(flash);
    let context = Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses"));
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            "probe_wifi_http" => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(std::env::temp_dir().join("pemu-m12-no-audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == "probe_wifi_http").then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::new(None, vec![]),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(None);
    Some(guard)
}

/// What the host server saw.
#[derive(Debug, Default)]
struct HostServed {
    request: Vec<u8>,
    connections: u32,
}

/// A plain HTTP/1.1 server on 127.0.0.1 that waits [`BRIDGE_DELAY_MS`] of host time after each
/// request head before it answers with [`HOST_BODY`], and serves until the client closes.
fn delayed_host_server() -> (u16, std::thread::JoinHandle<HostServed>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().expect("an address").port();
    let thread = std::thread::spawn(move || {
        let mut served = HostServed::default();
        let (mut sock, _) = listener.accept().expect("the bridge connects");
        served.connections += 1;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(60)))
            .expect("a read timeout");
        let mut buf = [0u8; 2048];
        loop {
            let n = match sock.read(&mut buf) {
                Ok(0) | Err(_) => return served,
                Ok(n) => n,
            };
            served.request.extend_from_slice(&buf[..n]);
            if served.request.windows(4).any(|w| w == b"\r\n\r\n") {
                std::thread::sleep(std::time::Duration::from_millis(BRIDGE_DELAY_MS));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                    HOST_BODY.len()
                );
                if sock.write_all(head.as_bytes()).is_err() || sock.write_all(HOST_BODY).is_err() {
                    return served;
                }
            }
        }
    });
    (port, thread)
}

/// The virtual instant of the first captured frame in `dir` whose bytes contain `needle`.
fn captured_at(state: &WifiState, dir: u8, needle: &[u8]) -> Option<u64> {
    state
        .capture
        .records
        .iter()
        .find(|r| r.dir == dir && r.frame.windows(needle.len()).any(|w| w == needle))
        .map(|r| r.at_us)
}

fn console_and_wifi(inst: &str) -> (String, WifiState) {
    let parsed = pemu_api::instance::InstanceId::parse(inst).expect("an id");
    pemu_api::commands::start::with_pool(|pool| {
        let session = pool.session_mut(parsed).expect("a session");
        let ring = session.machine().io().serial_ring(SerialStream::UsjTx);
        let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let state = pemu_api::commands::net_http::wifi_state(session)
            .expect("the state decodes")
            .expect("the module is bound");
        (text, state)
    })
}

/// `probe_wifi_http`'s GET goes through the Wi-Fi bridge to a real server on the host that takes
/// 300 ms of host time to answer, under an agent's lease, and the guest gets its 200 and the host's
/// body without a timeout. While the bridge is live the pacing is `realtime` 1.000x, and
/// `set_speed max`, `set_mode deterministic` and `endpoint --clock agent` answer `E_LEASE`.
///
/// The run is one `run --until` call paced at wall 1x (`net_http::live_bridge_tick`), so the
/// virtual gap between request and answer frames is the server's 300 ms plus the bridge's hops.
#[test]
fn t1_m12_a_bridged_request_to_a_delayed_host_server_completes_under_a_lease() {
    let test = "t1_m12_a_bridged_request_to_a_delayed_host_server_completes_under_a_lease";
    let exit = test.to_string();
    let Some(_hooks) = hooked_probe_wifi_http(test) else {
        return;
    };
    let start = call(
        "start",
        serde_json::json!({"fw": "probe_wifi_http", "boot": "none"}),
    )
    .unwrap_or_else(|e| panic!("{exit}: the probe starts: {e:?}"));
    let inst = start.json["instance"].as_str().expect("an id").to_owned();
    call(
        "env",
        serde_json::json!({"instance": inst, "wifi": {"aps": [{
            "ssid": VIRTUAL_AP, "bssid": "02:00:00:47:32:06", "rssi": -40, "channel": 6, "auth": 0,
        }]}}),
    )
    .unwrap_or_else(|e| panic!("{exit}: the air is scripted: {e:?}"));

    let (host_port, server) = delayed_host_server();
    let bridged = call(
        "net_http",
        serde_json::json!({"instance": inst, "op": "bridge", "port": 80, "host_port": host_port}),
    )
    .unwrap_or_else(|e| panic!("{exit}: the bridge attaches: {e:?}"));
    assert_eq!(
        bridged.json["lan"]["bridge"]["attached"], true,
        "{}",
        bridged.text
    );

    let status = call(
        "clock",
        serde_json::json!({"instance": inst, "op": "status"}),
    )
    .expect("status");
    assert_eq!(status.json["live_bridges"], 1, "{exit}: {}", status.text);
    assert_eq!(status.json["mode"], "realtime", "{exit}: {}", status.text);
    assert_eq!(status.json["speed"], 1.0, "{exit}: {}", status.text);
    for (name, args) in [
        (
            "clock",
            serde_json::json!({"instance": inst, "op": "set_speed", "speed": "max"}),
        ),
        (
            "clock",
            serde_json::json!({"instance": inst, "op": "set_mode", "mode": "deterministic"}),
        ),
        (
            "endpoint",
            serde_json::json!({"instance": inst, "tcp": true, "clock": "agent"}),
        ),
    ] {
        let err = call(name, args.clone()).expect_err("refused while the bridge is live");
        assert_eq!(err.code.name, "E_LEASE", "{exit}: {name} {args}: {err:?}");
        assert!(
            err.message.contains("bridge"),
            "{exit}: {name} {args} names the bridge: {}",
            err.message
        );
    }

    let wall = std::time::Instant::now();
    let ran = call(
        "run",
        serde_json::json!({
            "instance": inst,
            "until": "serial:\"DONE|name=probe_wifi_http|status=ok\"",
            "timeout": "40s",
            "wall_budget_ms": 90_000,
        }),
    )
    .unwrap_or_else(|e| panic!("{exit}: the probe runs to DONE: {e:?}"));
    let wall_ms = wall.elapsed().as_millis();
    assert_eq!(ran.json["status"], "matched", "{exit}: {}", ran.json);
    let (text, state) = console_and_wifi(&inst);
    let lines = [
        "IP|ip=10.23.0.100|netmask=255.255.255.0|gw=10.23.0.1".to_string(),
        format!(
            "HTTP|path=/probe|rc=0|status=200|length={}|sha256={}",
            HOST_BODY.len(),
            pemu_testkit::corpus::sha256_hex(HOST_BODY)
        ),
        "DONE|name=probe_wifi_http|status=ok".to_string(),
    ];
    let mut from = 0usize;
    for line in &lines {
        let at = text[from..].find(line.as_str()).unwrap_or_else(|| {
            panic!(
                "{exit}: `{line}` is missing or out of order; bridge {:?}; console:\n{text}",
                state.lan.bridge.counters
            )
        });
        from += at + line.len();
    }
    assert!(!text.contains("FAIL|"), "{exit}: no step failed:\n{text}");

    // The host really served it, and the scripted service answered nothing.
    let served = server.join().expect("the host server");
    assert_eq!(served.connections, 1, "{exit}");
    assert!(
        served.request.starts_with(b"GET /probe HTTP/1.1\r\n"),
        "{exit}: the host saw the guest's request: {:?}",
        String::from_utf8_lossy(&served.request)
    );
    let lan = &state.lan;
    assert_eq!(
        lan.counters.http_answers, 0,
        "{exit}: the scripted service was shadowed"
    );
    let b = &lan.bridge;
    assert_eq!(b.counters.connects, 1, "{exit}: {:?}", b.counters);
    assert_eq!(b.counters.refused, 0, "{exit}: {:?}", b.counters);
    assert_eq!(b.lost_in, 0, "{exit}: no gap in the inbound stream");
    assert_eq!(b.dropped_out, 0, "{exit}: the window held");
    assert!(
        b.counters.bytes_in > HOST_BODY.len() as u64,
        "{exit}: {:?}",
        b.counters
    );

    // The answer frame follows the request frame by the server's 300 ms of host time, which is 300
    // ms of virtual time only because the run was paced at wall 1x.
    let request_us = captured_at(&state, pemu_radio::lan::pcap::dir::TX, b"GET /probe")
        .expect("the request frame is in the capture");
    let answer_us = captured_at(&state, pemu_radio::lan::pcap::dir::RX, b"HTTP/1.1 200")
        .expect("the answer frame is in the capture");
    let gap_ms = (answer_us - request_us) / 1_000;
    assert!(
        (BRIDGE_DELAY_MS..BRIDGE_DELAY_MS + 150).contains(&gap_ms),
        "{exit}: the answer came {gap_ms} ms of virtual time after the request"
    );

    let detached = call(
        "net_http",
        serde_json::json!({"instance": inst, "op": "unbridge"}),
    )
    .expect("the bridge detaches");
    assert_eq!(detached.json["lan"]["bridge"]["attached"], false);
    let after = call(
        "clock",
        serde_json::json!({"instance": inst, "op": "status"}),
    )
    .expect("status");
    assert_eq!(after.json["live_bridges"], 0, "{exit}: {}", after.text);
    call(
        "clock",
        serde_json::json!({"instance": inst, "op": "set_speed", "speed": "max"}),
    )
    .unwrap_or_else(|e| panic!("{exit}: the clock is the caller's again: {e:?}"));
    call("stop", serde_json::json!({"instance": inst})).expect("stop");
    println!(
        "RAN {exit}: GET /probe answered 200 by the host server {gap_ms} ms of virtual time \
         after the request (server delay {BRIDGE_DELAY_MS} ms), run to DONE in {wall_ms} ms of \
         wall time at vt {} us; bridge {:?}",
        ran.json["vt_us"], b.counters
    );
}

/// The virtual delay the scripted relay answers with: the host server's 300 ms, in virtual time.
const RELAY_DELAY_US: u64 = 300_000;

/// Drives `m` to `DONE|`, playing the relay's server side against the bridge window: every
/// `CONNECT` is answered with `CONTINUE` at once and every `DATA` with [`RELAY_BODY`] one
/// [`RELAY_DELAY_US`] later, each journaled as a bridged `NetFrame`, so the run replays from its
/// journal alone. Returns how many packets the relay journaled.
fn drive_scripted_relay(m: &mut Machine, con: &mut Console, test: &str) -> u64 {
    use pemu_radio::lan::bridge;
    let mut cursor = 0u64;
    let mut seq = 0u64;
    let journal = |m: &mut Machine, at: u64, seq: &mut u64, packet: Vec<u8>| {
        m.input_from(
            At::Vt(VTime::from_us(at)),
            pemu_core::journal::Origin::Bridge,
            InputEvent::NetFrame {
                seq: *seq,
                data: packet,
            },
        )
        .expect("a bridged packet journals");
        *seq += 1;
    };
    // The relay is ready: the stream-0 credit of WISP v1.
    let now = m.now().as_us();
    journal(m, now, &mut seq, bridge::continue_(0, 64));
    run_while(m, con, 30_000, test, "DONE|", |m, con| {
        let state = wifi_state(m);
        let (packets, next) = state.lan.bridge.since(cursor);
        cursor = next;
        let now = m.now().as_us();
        for packet in &packets {
            let (kind, stream, payload) = bridge::decode(packet).expect("a WISP packet");
            match kind {
                bridge::kind::CONNECT => {
                    journal(m, now, &mut seq, bridge::continue_(stream, 64));
                }
                bridge::kind::DATA => {
                    assert!(
                        payload.starts_with(b"GET /probe "),
                        "the guest's request crossed the bridge: {:?}",
                        String::from_utf8_lossy(payload)
                    );
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                        RELAY_BODY.len()
                    );
                    let mut answer = head.into_bytes();
                    answer.extend_from_slice(RELAY_BODY);
                    journal(
                        m,
                        now + RELAY_DELAY_US,
                        &mut seq,
                        bridge::encode(bridge::kind::DATA, stream, &answer),
                    );
                }
                _ => {}
            }
        }
        con.text().contains("DONE|")
    });
    seq
}

const RELAY_BODY: &[u8] = b"passportsim bridge: answered through the relay\n";

/// A bridged run is `live` while it runs and replays from its journal alone, with no peer, to the
/// same state. The relay is scripted in virtual time, the shape the browser's WISP relay takes.
#[test]
fn t1_m12_a_bridged_run_is_live_and_replays_from_its_journal_without_the_peer() {
    use pemu_core::journal::Determinism;
    let test = "t1_m12_a_bridged_run_is_live_and_replays_from_its_journal_without_the_peer";
    let Some(run) = probe_wifi_http_to_connected(test) else {
        return;
    };
    let HttpRun { mut m, mut con, .. } = run;
    assert_eq!(
        m.journal().class(),
        Determinism::Deterministic,
        "{test}: nothing live yet"
    );
    m.input_from(
        At::Now,
        pemu_core::journal::Origin::Agent,
        InputEvent::Env(EnvChange::WifiBridge {
            attached: true,
            routes: vec![pemu_core::input::NetRoute {
                port: 80,
                host_port: 8_080,
            }],
        }),
    )
    .expect("the bridge attach journals");
    let packets = drive_scripted_relay(&mut m, &mut con, test);
    let text = con.text();
    let lines = [
        format!(
            "HTTP|path=/probe|rc=0|status=200|length={}|sha256={}",
            RELAY_BODY.len(),
            pemu_testkit::corpus::sha256_hex(RELAY_BODY)
        ),
        "DONE|name=probe_wifi_http|status=ok".to_string(),
    ];
    for line in &lines {
        assert!(
            text.contains(line.as_str()),
            "{test}: `{line}` is missing; console:\n{text}"
        );
    }
    let state = wifi_state(&m);
    assert_eq!(state.lan.bridge.counters.connects, 1, "{test}");
    assert_eq!(state.lan.counters.http_answers, 0, "{test}: shadowed");
    assert_eq!(
        m.live_bridges(),
        1,
        "{test}: the machine reports its bridge"
    );
    assert_eq!(
        m.journal().class(),
        Determinism::Live,
        "{test}: a bridged peer makes the run live"
    );
    assert!(
        packets >= 3,
        "{test}: the relay journaled {packets} packet(s)"
    );
    let want_hash = m.state_hash();
    let want_state = wifi_state(&m);
    let want_text = text.clone();
    let end = m.now();

    // The replay: the same journal into a fresh machine, at `Max`, with no relay.
    let (flash, elf) = probe_wifi_http_files(test).expect("the files are here");
    let mut replay = machine(&flash, &elf, MachineConfig::default());
    for entry in m.journal().entries() {
        replay
            .input_from(At::Vt(entry.at), entry.origin, entry.ev.clone())
            .expect("a recorded input journals again");
    }
    let mut replay_con = Console::from(0);
    while replay.now().0 < end.0 {
        let out = replay.run(RunLimits {
            until: Some(end),
            max_insns: None,
            stops: StopSet::default(),
        });
        replay_con.pump(&mut replay);
        assert_eq!(out.reason, StopReason::Until, "{test}: replay");
    }
    assert_eq!(
        replay.state_hash(),
        want_hash,
        "{test}: replay state differs"
    );
    assert_eq!(
        wifi_state(&replay),
        want_state,
        "{test}: replay module state"
    );
    assert!(
        replay_con.text().contains(lines[1].as_str()),
        "{test}: the replay reached the same console:\n{}",
        replay_con.text()
    );
    assert_eq!(replay_con.text(), want_text, "{test}: replay console");
    println!(
        "RAN {test}: {packets} relay packet(s) journaled, replayed with no peer to the same \
         state hash at vt {} us",
        end.as_us()
    );
}
