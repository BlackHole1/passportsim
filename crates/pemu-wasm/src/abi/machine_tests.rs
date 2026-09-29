//! The machine-bound entry points on the real machine, over the bundled ROM and an erased flash,
//! which boots to the ROM's retry line with no corpus.

use pemu_api::error::{E_ASSET_MISSING, E_SNAPSHOT, E_STATE, E_USAGE};
use pemu_api::instance::InstanceId;
use pemu_core::hostio::SerialStream;
use pemu_core::input::InputEvent;
use pemu_loader::bundle::{BundleInput, FlashImage, build as build_bundle};
use pemu_machine::Machine;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::determinism::report;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopSet};

use super::*;
use crate::layout::{INPUT_BATCH_MAGIC, InputKind, RingId, SLOT_NOW_PS, StopCode};

/// Instructions past the banner and into the boot retry loop (as `pemu_machine::determinism`).
const INSNS: i64 = 300_000;

fn take(res: *mut ResultHeader) -> (u32, String) {
    assert!(!res.is_null());
    // SAFETY: `res` is a live header an ABI call just returned; its payload follows it.
    unsafe {
        let status = (*res).status;
        let len = (*res).len as usize;
        let data = res.cast::<u8>().add(RESULT_PAYLOAD_OFFSET);
        let bytes = core::slice::from_raw_parts(data, len).to_vec();
        pemu_result_free(res);
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn bytes_of(res: *mut ResultHeader) -> (u32, Vec<u8>) {
    // SAFETY: as `take`.
    unsafe {
        let status = (*res).status;
        let len = (*res).len as usize;
        let data = res.cast::<u8>().add(RESULT_PAYLOAD_OFFSET);
        let bytes = core::slice::from_raw_parts(data, len).to_vec();
        pemu_result_free(res);
        (status, bytes)
    }
}

fn ok_json(res: *mut ResultHeader) -> serde_json::Value {
    let (status, text) = take(res);
    assert_eq!(status, STATUS_OK, "{text}");
    serde_json::from_str(&text).expect("a JSON payload")
}

fn err_code(res: *mut ResultHeader) -> (u32, serde_json::Value) {
    let (status, text) = take(res);
    assert_ne!(status, STATUS_OK, "expected a refusal, got {text}");
    (
        status,
        serde_json::from_str(&text).expect("an ApiError JSON"),
    )
}

fn erased_handle(cfg: &str) -> u32 {
    // SAFETY: the configuration bytes are a live slice.
    let builder = unsafe { pemu_new(cfg.as_ptr(), cfg.len() as u32) };
    // SAFETY: `builder` is live and not yet built.
    unsafe { (*builder).flash = Some(FlashImage::erased()) };
    // SAFETY: `builder` came from `pemu_new`.
    let (status, payload) = bytes_of(unsafe { pemu_build(builder) });
    assert_eq!(status, STATUS_OK, "{}", String::from_utf8_lossy(&payload));
    u32::from_le_bytes(payload.try_into().expect("a u32 handle"))
}

fn call(handle: u32, request: &str) -> *mut ResultHeader {
    // SAFETY: the request bytes are a live slice.
    unsafe { pemu_call(handle, request.as_ptr(), request.len() as u32) }
}

fn native(cfg: MachineConfig) -> Machine {
    let assets = Assets::with_bundled_rom(
        FlashImage::erased(),
        None,
        None,
        pemu_loader::efuse_image::EfuseImage::synth(cfg.seed),
    )
    .expect("the bundled ROM is pinned");
    Machine::new(cfg, assets).expect("composes")
}

const RETRY_STOPS: &str = r#"{"cmd":"@stops","args":{"matchers":[{"id":1,"serial":{"stream":"usj","contains":"invalid header"}}]}}"#;

fn retry_line() -> StopSet {
    StopSet {
        matchers: vec![(
            MatcherId(1),
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Contains("invalid header".into()),
            },
        )],
        ..StopSet::default()
    }
}

#[test]
fn a_built_machine_publishes_its_layout_and_starts_at_time_zero() {
    let handle = erased_handle("");
    assert_eq!(pemu_now_ps(handle), 0);
    let layout = pemu_io_layout(handle);
    assert!(!layout.is_null());
    // SAFETY: the layout lives as long as the handle.
    let (version, generation) = unsafe { ((*layout).abi_version, (*layout).generation) };
    assert_eq!(version, ABI_VERSION);
    assert_eq!(generation, 1);
    assert_eq!(
        ok_json(pemu_last_stop(handle)),
        serde_json::json!({ "reason": null })
    );
    pemu_drop(handle);
}

#[test]
fn run_limits_stop_where_they_say_and_the_cursor_block_follows() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, INSNS), StopCode::MaxInsns as u32);
    let stop = ok_json(pemu_last_stop(handle));
    assert_eq!(stop["reason"], "MaxInsns");
    assert_eq!(stop["code"], StopCode::MaxInsns as u32);
    assert_eq!(stop["insns"], INSNS.to_string());
    let now = pemu_now_ps(handle);
    assert!(now > 0);
    assert_eq!(stop["vt_ps"], now.to_string());
    // The published cursor block, read by the worker through `cursors_ptr`.
    let cursors = instance::with_table(|t| t.get(handle).unwrap().publisher().cursors().to_vec());
    let (cursor_now, usj_head) = (
        cursors[SLOT_NOW_PS as usize],
        cursors[RingId::UsjTx.head_slot() as usize],
    );
    assert_eq!(cursor_now, now as u64);
    assert!(
        usj_head > 0,
        "the ROM banner reached the published usj cursor"
    );

    let until = now + 1_000_000_000;
    assert_eq!(pemu_run(handle, until, -1), StopCode::Until as u32);
    assert_eq!(pemu_now_ps(handle), until);
    pemu_drop(handle);
}

#[test]
fn stops_are_armed_through_call_and_the_report_equals_the_native_machine() {
    let cfg = r#"{"max_block_insns":3,"max_slice":1000,"poll_ff":false}"#;
    let handle = erased_handle(cfg);
    let armed = ok_json(call(handle, RETRY_STOPS));
    assert_eq!(armed["armed"]["matchers"], 1);
    assert_eq!(pemu_run(handle, -1, 5_000_000), StopCode::Matcher as u32);
    assert_eq!(ok_json(pemu_last_stop(handle))["matcher"], 1);
    let abi = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"]
        .as_str()
        .expect("a report line")
        .to_string();

    let mut m = native(crate::config::parse(cfg.as_bytes()).ok().unwrap().machine);
    m.set_max_slice(1000);
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(5_000_000),
        stops: retry_line(),
    });
    assert_eq!(
        abi,
        report(&out.reason, &m),
        "the ABI machine is the native machine"
    );
    assert!(abi.starts_with("stop=Matcher(MatcherId(1)) "), "{abi}");

    let (status, body) = err_code(call(
        handle,
        r#"{"cmd":"@stops","args":{"watches":[{"addr":1073741824,"len":4}]}}"#,
    ));
    assert_eq!(status, u32::from(E_USAGE.number), "{body}");
    pemu_drop(handle);
}

#[test]
fn inputs_are_journaled_from_json_and_from_fixed_records() {
    let handle = erased_handle("");
    assert_eq!(
        ok_json(call(handle, r#"{"cmd":"@live"}"#))["mic_next_seq"],
        "0"
    );
    let json = r#"[{"at":"now","event":{"MicChunk":{"seq":0,"samples":[1,2,3]}}},
                   {"at":"5000","event":{"Button":{"id":"Ok","down":true}}}]"#;
    // SAFETY: live slice.
    let answer = ok_json(unsafe { pemu_input(handle, json.as_ptr(), json.len() as u32) });
    assert_eq!(answer["journaled"], 2);
    assert_eq!(
        ok_json(call(handle, r#"{"cmd":"@live"}"#))["mic_next_seq"],
        "1"
    );

    let mut batch = Vec::new();
    for word in [INPUT_BATCH_MAGIC, 1, 48, 0] {
        batch.extend_from_slice(&word.to_le_bytes());
    }
    batch.extend_from_slice(&crate::layout::AT_NOW.to_le_bytes());
    for word in [InputKind::Button as u32, 2, 0, 0, 0, 0] {
        batch.extend_from_slice(&word.to_le_bytes());
    }
    // SAFETY: live slice.
    let answer = ok_json(unsafe { pemu_input(handle, batch.as_ptr(), batch.len() as u32) });
    assert_eq!(answer["journaled"], 1);

    let bad = br#"[{"at":"now","event":{"NoSuchEvent":1}}]"#;
    // SAFETY: live slice.
    let (status, _) = err_code(unsafe { pemu_input(handle, bad.as_ptr(), bad.len() as u32) });
    assert_eq!(status, u32::from(E_USAGE.number));
    pemu_drop(handle);
}

#[test]
fn a_snapshot_restores_in_place_and_bumps_the_layout_generation() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, INSNS), StopCode::MaxInsns as u32);
    let (status, snap) = bytes_of(pemu_snapshot(handle, 0));
    assert_eq!(status, STATUS_OK);
    let at = pemu_now_ps(handle);
    // SAFETY: the layout lives as long as the handle.
    let (ptr, generation) = unsafe {
        (
            (*pemu_io_layout(handle)).frame_ptr,
            (*pemu_io_layout(handle)).generation,
        )
    };
    ok_json(call(handle, RETRY_STOPS));
    assert_eq!(pemu_run(handle, -1, 5_000_000), StopCode::Matcher as u32);
    let straight = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"].clone();

    let (status, _) = take(unsafe { pemu_restore(handle, snap.as_ptr(), snap.len() as u32) });
    assert_eq!(status, STATUS_OK);
    assert_eq!(pemu_now_ps(handle), at);
    // SAFETY: as above.
    let (ptr_after, generation_after) = unsafe {
        (
            (*pemu_io_layout(handle)).frame_ptr,
            (*pemu_io_layout(handle)).generation,
        )
    };
    assert_eq!(ptr_after, ptr, "a restore reallocates nothing");
    assert_eq!(
        generation_after,
        generation + 1,
        "the worker is told to re-create its views"
    );
    assert_eq!(pemu_run(handle, -1, 5_000_000), StopCode::Matcher as u32);
    let again = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"].clone();
    // The console digests cover output since power-on, which a restored ring does not hold, so
    // state and position are compared instead.
    let field = |r: &serde_json::Value, name: &str| {
        r.as_str()
            .unwrap()
            .split(' ')
            .find(|f| f.starts_with(name))
            .map(str::to_string)
    };
    for name in ["stop=", "state=", "insns=", "vt="] {
        assert_eq!(field(&again, name), field(&straight, name), "{name}");
    }

    let (status, _) = err_code(pemu_snapshot(handle, 1));
    assert_eq!(status, u32::from(E_USAGE.number));
    let junk = b"not a snapshot";
    // SAFETY: live slice.
    let (status, _) = err_code(unsafe { pemu_restore(handle, junk.as_ptr(), junk.len() as u32) });
    assert_eq!(status, u32::from(E_SNAPSHOT.number));
    pemu_drop(handle);
}

#[test]
fn registry_commands_run_on_the_handle_s_pool_session() {
    let handle = erased_handle(r#"{"fw":"erased","label":"abi"}"#);
    let id: InstanceId = instance::with_table(|t| t.get(handle).unwrap().id);
    let status = ok_json(call(handle, r#"{"cmd":"status","args":{}}"#));
    assert!(
        status["json"].to_string().contains(&id.to_string()),
        "{status}"
    );
    assert!(status["receipt"].is_object());
    // An erased flash boots no app, so there is no build to name.
    assert_eq!(
        status["json"]["instances"][0].get("build"),
        Some(&serde_json::Value::Null),
        "{status}"
    );

    let before = pemu_now_ps(handle);
    let ran = ok_json(call(handle, r#"{"cmd":"run","args":{"for":"2ms"}}"#));
    assert!(ran["json"].is_object(), "{ran}");
    assert!(
        pemu_now_ps(handle) >= before + 2_000_000_000,
        "the registry run advanced this machine"
    );

    let (status, body) = err_code(call(handle, r#"{"cmd":"no_such_command"}"#));
    assert_eq!(status, u32::from(E_USAGE.number), "{body}");
    let (status, body) = err_code(call(
        handle,
        r#"{"cmd":"status","args":{"instance":"p999999"}}"#,
    ));
    assert_eq!(status, u32::from(E_USAGE.number), "{body}");
    let (status, _) = err_code(call(handle, "{"));
    assert_eq!(status, u32::from(E_USAGE.number));
    pemu_drop(handle);
}

#[test]
fn a_dropped_handle_ends_its_session_and_names_no_machine() {
    let handle = erased_handle("");
    let id = instance::with_table(|t| t.get(handle).unwrap().id);
    assert!(pemu_api::commands::start::with_pool(|p| p
        .session(id)
        .is_some()));
    pemu_drop(handle);
    assert!(pemu_api::commands::start::with_pool(|p| p
        .session(id)
        .is_none()));
    assert_eq!(pemu_run(handle, -1, 1), NO_MACHINE);
    assert_eq!(pemu_now_ps(handle), -1);
    assert!(pemu_io_layout(handle).is_null());
    let (status, _) = err_code(pemu_last_stop(handle));
    assert_eq!(status, u32::from(E_STATE.number));
    let (status, _) = err_code(call(handle, r#"{"cmd":"status"}"#));
    assert_eq!(status, u32::from(E_STATE.number));
    pemu_drop(handle);
}

#[test]
fn a_bad_config_or_asset_is_refused_with_its_code() {
    let cfg = br#"{"sed":1}"#;
    // SAFETY: live slices; each builder is consumed by pemu_build.
    unsafe {
        let builder = pemu_new(cfg.as_ptr(), cfg.len() as u32);
        (*builder).flash = Some(FlashImage::erased());
        let (status, body) = err_code(pemu_build(builder));
        assert_eq!(status, u32::from(E_USAGE.number), "{body}");

        let builder = pemu_new(core::ptr::null(), 0);
        let (status, _) = err_code(pemu_load(builder, 9, b"x".as_ptr(), 1));
        assert_eq!(status, u32::from(E_USAGE.number));
        let (status, _) = err_code(pemu_load(builder, 1, b"xyz".as_ptr(), 3));
        assert_eq!(status, u32::from(E_USAGE.number));
        let (status, _) = err_code(pemu_load(builder, 2, b"xyz".as_ptr(), 3));
        assert_eq!(status, u32::from(E_USAGE.number));

        let bundle = build_bundle(
            Some("x"),
            None,
            &[BundleInput {
                role: "app_elf",
                name: "a.elf",
                bytes: b"elf",
            }],
        );
        let (status, body) = err_code(pemu_load(builder, 1, bundle.as_ptr(), bundle.len() as u32));
        assert_eq!(status, u32::from(E_ASSET_MISSING.number), "{body}");
        let bundle = build_bundle(
            None,
            None,
            &[BundleInput {
                role: "flash",
                name: "f.bin",
                bytes: b"not flash",
            }],
        );
        let (status, body) = err_code(pemu_load(builder, 1, bundle.as_ptr(), bundle.len() as u32));
        assert_eq!(status, u32::from(E_USAGE.number), "{body}");

        let (status, body) = err_code(pemu_build(builder));
        assert_eq!(status, u32::from(E_ASSET_MISSING.number), "{body}");
        assert!(body["hint"].as_str().unwrap().contains("pemu_load"));
    }
}

/// The end of a live microphone stream is a `mic_set` to `silence` through `pemu_call`; it is
/// journaled, so the same run with and without it ends in different states.
#[test]
fn the_worker_s_mic_end_of_stream_call_is_journaled_on_the_real_machine() {
    let with_end = erased_handle("");
    let without = erased_handle("");
    assert_eq!(pemu_run(with_end, -1, INSNS), StopCode::MaxInsns as u32);
    assert_eq!(pemu_run(without, -1, INSNS), StopCode::MaxInsns as u32);
    let answer = ok_json(call(
        with_end,
        r#"{"cmd":"mic_set","args":{"kind":"silence"}}"#,
    ));
    assert!(answer["json"].is_object(), "{answer}");
    for handle in [with_end, without] {
        assert_eq!(pemu_run(handle, -1, 1_000), StopCode::MaxInsns as u32);
    }
    let state = |handle| {
        let line = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"]
            .as_str()
            .unwrap()
            .to_string();
        line.split(' ')
            .find(|f| f.starts_with("state="))
            .unwrap()
            .to_string()
    };
    assert_ne!(
        state(with_end),
        state(without),
        "the source change is in the journal"
    );
    pemu_drop(with_end);
    pemu_drop(without);
}

/// A refused load names nothing, and a role loaded twice is named once. The flash is set on the
/// builder directly: no merged image parses without a corpus file (`tests/milestones/m9.rs`
/// covers the `.pebundle` roles).
#[test]
fn the_report_names_the_asset_roles_that_were_loaded() {
    let dump: Vec<u8> = pemu_loader::efuse_image::EfuseImage::synth(0)
        .dump_words()
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    // SAFETY: live slices; the builder is consumed by pemu_build.
    let handle = unsafe {
        let builder = pemu_new(core::ptr::null(), 0);
        let (status, body) = take(pemu_load(builder, 4, dump.as_ptr(), dump.len() as u32));
        assert_eq!(status, STATUS_OK, "{body}");
        let (status, _) = take(pemu_load(builder, 4, dump.as_ptr(), dump.len() as u32));
        assert_eq!(status, STATUS_OK);
        let (status, _) = take(pemu_load(builder, 2, b"xyz".as_ptr(), 3));
        assert_ne!(status, STATUS_OK, "not an ELF");
        let bundle = build_bundle(
            None,
            None,
            &[BundleInput {
                role: "app_elf",
                name: "a.elf",
                bytes: b"elf",
            }],
        );
        let (status, _) = take(pemu_load(builder, 1, bundle.as_ptr(), bundle.len() as u32));
        assert_ne!(status, STATUS_OK, "a bundle with no flash");
        assert_eq!((*builder).roles, ["efuse"]);
        (*builder).flash = Some(FlashImage::erased());
        let (status, payload) = bytes_of(pemu_build(builder));
        assert_eq!(status, STATUS_OK, "{}", String::from_utf8_lossy(&payload));
        u32::from_le_bytes(payload.try_into().expect("a u32 handle"))
    };
    assert_eq!(pemu_run(handle, -1, 1_000), StopCode::MaxInsns as u32);
    let answer = ok_json(call(handle, r#"{"cmd":"@report"}"#));
    assert_eq!(answer["roles"], serde_json::json!(["efuse"]));
    assert!(
        answer["report"]
            .as_str()
            .is_some_and(|r| r.starts_with("stop="))
    );
    pemu_drop(handle);
}

/// The pool session's count is raised to the machine's before the command runs, so the tone's
/// chunks continue after the live ones instead of reusing their numbers.
#[test]
fn a_mic_set_after_live_chunks_continues_their_seq() {
    let handle = erased_handle("");
    let live = r#"[{"at":"now","event":{"MicChunk":{"seq":0,"samples":[1,2,3]}}},
                   {"at":"now","event":{"MicChunk":{"seq":1,"samples":[4,5,6]}}}]"#;
    // SAFETY: live slice.
    let answer = ok_json(unsafe { pemu_input(handle, live.as_ptr(), live.len() as u32) });
    assert_eq!(answer["journaled"], 2);
    let next = |handle| {
        ok_json(call(handle, r#"{"cmd":"@live"}"#))["mic_next_seq"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    assert_eq!(next(handle), 2);
    let answer = ok_json(call(
        handle,
        r#"{"cmd":"mic_set","args":{"kind":"tone","hz":440,"amplitude":16384,"duration_ms":20}}"#,
    ));
    assert!(answer["json"].is_object(), "{answer}");
    let after = next(handle);
    assert!(
        after > 2,
        "the tone's chunks were journaled after the live ones (next seq {after})"
    );
    let id = instance::with_table(|t| t.get(handle).unwrap().id);
    let first = pemu_api::commands::start::with_pool(|p| p.session(id).map(|s| s.mic.next_seq));
    assert_eq!(
        first,
        Some(after),
        "the session and the machine agree on the next seq"
    );
    pemu_drop(handle);
}

/// A Worker's stamped press keeps the run `deterministic`; the journal replayed natively through
/// `Machine::input` is `deterministic` again.
#[test]
fn a_live_mic_chunk_makes_the_receipt_replayable_and_its_replay_deterministic() {
    let handle = erased_handle("");
    let receipt_class = |handle| {
        ok_json(call(handle, r#"{"cmd":"status","args":{}}"#))["receipt"]["determinism"].clone()
    };
    let press = br#"[{"at":"now","event":{"Button":{"id":"Ok","down":true}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, press.as_ptr(), press.len() as u32) });
    assert_eq!(receipt_class(handle), "deterministic");
    let live = br#"[{"at":"now","event":{"MicChunk":{"seq":0,"samples":[1,2,3]}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, live.as_ptr(), live.len() as u32) });
    assert_eq!(receipt_class(handle), "replayable");

    let answer = ok_json(call(
        handle,
        r#"{"cmd":"@journal","args":{"include_secrets":true}}"#,
    ));
    let mut m = native(MachineConfig::default());
    for entry in answer["entries"].as_array().expect("entries") {
        let at: u64 = entry["at_ps"].as_str().unwrap().parse().unwrap();
        let event: InputEvent = serde_json::from_value(entry["event"].clone()).expect("an event");
        m.input(
            pemu_machine::machine::At::Vt(pemu_core::time::VTime(at)),
            event,
        )
        .expect("journaled");
    }
    assert_eq!(
        m.receipt().determinism,
        Some(pemu_core::journal::Determinism::Deterministic),
        "a replay is scripted input"
    );
    pemu_drop(handle);
}

#[test]
fn a_bridged_frame_through_pemu_input_is_live_and_dropped_from_the_journal() {
    let handle = erased_handle("");
    let frame = br#"[{"at":"now","event":{"NetFrame":{"seq":0,"data":[1,2,3,4]}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, frame.as_ptr(), frame.len() as u32) });
    let status = ok_json(call(handle, r#"{"cmd":"status","args":{}}"#));
    assert_eq!(status["receipt"]["determinism"], "live", "{status}");
    let answer = ok_json(call(handle, r#"{"cmd":"@journal","args":{}}"#));
    let entries = answer["entries"].as_array().expect("entries");
    let last = entries.last().expect("the frame is journaled");
    assert!(last["event"].is_null(), "{answer}");
    assert_eq!(answer["dropped"][0]["kind"], "NetFrame", "{answer}");
    pemu_drop(handle);
}

/// This machine links no Wi-Fi driver, so the window is empty however often it is read; what is
/// checked is the shape, the refusals and that a read moves nothing.
#[test]
fn the_relay_window_is_read_only_and_empty_without_a_bridge() {
    let handle = erased_handle("");
    let first = ok_json(call(handle, r#"{"cmd":"@relay"}"#));
    assert_eq!(first["attached"], false, "{first}");
    assert_eq!(first["routes"], serde_json::json!([]), "{first}");
    assert_eq!(first["packets"], serde_json::json!([]), "{first}");
    assert_eq!(first["cursor"], "0", "{first}");
    assert_eq!(first["dropped"], "0", "{first}");
    // Reading again answers the same thing: a read takes nothing out of the machine.
    let again = ok_json(call(handle, r#"{"cmd":"@relay","args":{"cursor":"0"}}"#));
    assert_eq!(again, first, "a read moves no cursor of the machine's");
    assert_eq!(
        ok_json(call(handle, r#"{"cmd":"@relay","args":{"cursor":0}}"#)),
        first,
        "a cursor may be a number as well as a decimal string"
    );
    for bad in [
        r#"{"cmd":"@relay","args":{"cursor":"seven"}}"#,
        r#"{"cmd":"@relay","args":{"cursor":true}}"#,
        r#"{"cmd":"@relay","args":{"cursor":-1}}"#,
    ] {
        let (status, body) = err_code(call(handle, bad));
        assert_eq!(status, u32::from(E_USAGE.number), "{bad}: {body}");
    }
    pemu_drop(handle);
}

#[test]
fn raw_runs_count_into_the_session_status_and_receipt() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, 5_000), StopCode::MaxInsns as u32);
    let ran = ok_json(call(handle, r#"{"cmd":"run","args":{"for":"1ms"}}"#));
    assert_eq!(pemu_run(handle, -1, 7_000), StopCode::MaxInsns as u32);
    let machine_insns: u64 = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"]
        .as_str()
        .unwrap()
        .split(' ')
        .find_map(|f| f.strip_prefix("insns="))
        .unwrap()
        .parse()
        .unwrap();
    let status = ok_json(call(handle, r#"{"cmd":"status","args":{}}"#));
    let row = &status["json"]["instances"][0];
    assert_eq!(row["insns"], machine_insns, "status: {status}");
    assert_eq!(
        status["receipt"]["insns"], machine_insns,
        "receipt: {status}"
    );
    assert!(ran["receipt"]["insns"].as_u64().unwrap() >= 5_000, "{ran}");
    pemu_drop(handle);
}

#[test]
fn a_registry_restore_bumps_the_generation_even_when_no_cursor_moves_back() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let generation = || {
        // SAFETY: the layout lives as long as the handle.
        unsafe { (*pemu_io_layout(handle)).generation }
    };
    let saved = ok_json(call(
        handle,
        r#"{"cmd":"snapshot","args":{"op":"save","name":"here"}}"#,
    ));
    assert!(saved["json"].is_object(), "{saved}");
    let before = generation();
    let listed = ok_json(call(handle, r#"{"cmd":"snapshot","args":{"op":"list"}}"#));
    assert!(listed["json"].is_object(), "{listed}");
    assert_eq!(
        generation(),
        before,
        "a read-only snapshot op rebuilds nothing"
    );
    let restored = ok_json(call(
        handle,
        r#"{"cmd":"snapshot","args":{"op":"restore","name":"here"}}"#,
    ));
    assert!(restored["json"].is_object(), "{restored}");
    assert_eq!(generation(), before + 1, "the restore rebuilt the view");
    let (status, _) = err_code(call(
        handle,
        r#"{"cmd":"snapshot","args":{"op":"load","name":"nowhere"}}"#,
    ));
    assert_ne!(status, STATUS_OK);
    assert_eq!(
        generation(),
        before + 1,
        "a refused restore rebuilds nothing"
    );
    pemu_drop(handle);
}

/// The inputs `@journal` answers, as `(at, event)` a native machine journals.
fn journal_of(handle: u32) -> Vec<(serde_json::Value, pemu_machine::machine::At, InputEvent)> {
    let answer = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(answer["format"], 1);
    assert_eq!(answer["replayable"], true, "{answer}");
    answer["entries"]
        .as_array()
        .expect("`entries` is an array")
        .iter()
        .map(|entry| {
            let at: u64 = entry["at_ps"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .expect("`at_ps` is a decimal string");
            let event: InputEvent =
                serde_json::from_value(entry["event"].clone()).expect("`event` is an InputEvent");
            (
                entry.clone(),
                pemu_machine::machine::At::Vt(pemu_core::time::VTime(at)),
                event,
            )
        })
        .collect()
}

/// Without the registry click the native replay ends elsewhere, so the click is what the export
/// had to carry.
#[test]
fn a_worker_press_and_a_registry_click_replay_natively_from_journal() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let down = format!(
        r#"[{{"at":"{}","event":{{"Button":{{"id":"Ok","down":true}}}}}}]"#,
        pemu_now_ps(handle)
    );
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, down.as_ptr(), down.len() as u32) });
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let up = br#"[{"at":"now","event":{"Button":{"id":"Ok","down":false}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, up.as_ptr(), up.len() as u32) });
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let clicked = ok_json(call(
        handle,
        r#"{"cmd":"input","args":{"button":"down","action":"click"}}"#,
    ));
    assert!(clicked["json"].is_object(), "{clicked}");
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let browser = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"]
        .as_str()
        .expect("a report line")
        .to_string();
    let vt = pemu_now_ps(handle) as u64;

    let journal = journal_of(handle);
    let doors: Vec<_> = journal.iter().map(|(e, ..)| e["door"].clone()).collect();
    assert_eq!(
        doors,
        ["input", "input", "registry", "registry"],
        "both doors, in the order they journaled"
    );
    let seqs: Vec<u64> = journal
        .iter()
        .map(|(e, ..)| e["seq"].as_str().unwrap().parse().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq order: {seqs:?}");
    assert_eq!(
        journal[2].2,
        InputEvent::Button {
            id: pemu_core::input::ButtonId::Down,
            down: true
        }
    );

    let replay = |entries: &[(serde_json::Value, pemu_machine::machine::At, InputEvent)]| {
        let mut m = native(MachineConfig::default());
        for (_, at, event) in entries {
            m.input(*at, event.clone())
                .expect("a journaled instant is not in a fresh machine's past");
        }
        let out = m.run(RunLimits {
            until: Some(pemu_core::time::VTime(vt)),
            max_insns: None,
            stops: StopSet::default(),
        });
        report(&out.reason, &m)
    };
    let field = |line: &str, name: &str| {
        line.split(' ')
            .find(|f| f.starts_with(name))
            .map(str::to_string)
    };
    let native_report = replay(&journal);
    for name in ["state=", "insns=", "vt=", "usj=", "uart0=", "lines="] {
        assert_eq!(
            field(&native_report, name),
            field(&browser, name),
            "`{name}` of the replay differs\nnative: {native_report}\nabi:    {browser}"
        );
    }
    let without_click = replay(&journal[..2]);
    assert_ne!(
        field(&without_click, "state="),
        field(&browser, "state="),
        "the replay without the registry click ends in the same state, so the click compared nothing"
    );
    pemu_drop(handle);
}

/// The entries are the snapshot's pending input and every input since; those journaled after the
/// snapshot but before the restore are gone with their timeline. Raw and registry restores both
/// count.
#[test]
fn after_a_restore_the_journal_answers_from_the_snapshot_s_instant() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    // A registry click, then a press stamped ahead of the snapshot, still pending when it is taken.
    ok_json(call(
        handle,
        r#"{"cmd":"input","args":{"button":"down","action":"click"}}"#,
    ));
    let now = pemu_now_ps(handle) as u64;
    let ahead = format!(
        r#"[{{"at":"{}","event":{{"Button":{{"id":"Ok","down":true}}}}}}]"#,
        now + 50_000_000_000
    );
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, ahead.as_ptr(), ahead.len() as u32) });
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let (status, bytes) = bytes_of(pemu_snapshot(handle, 0));
    assert_eq!(status, STATUS_OK);
    let before = journal_of(handle);
    assert_eq!(
        before.len(),
        3,
        "a click is a press and a release, plus the Worker press"
    );

    // A timeline the restore discards.
    let gone = br#"[{"at":"now","event":{"Button":{"id":"Up","down":true}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, gone.as_ptr(), gone.len() as u32) });
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);

    // SAFETY: live slice.
    let (status, _) = take(unsafe { pemu_restore(handle, bytes.as_ptr(), bytes.len() as u32) });
    assert_eq!(status, STATUS_OK);
    let answer = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(answer["format"], 2, "{answer}");
    assert_eq!(answer["from"]["vt_ps"], pemu_now_ps(handle).to_string());
    assert_eq!(answer["from"]["next_seq"], "3", "{answer}");
    let entries = answer["entries"].as_array().expect("entries");
    assert_eq!(
        entries.len(),
        1,
        "only the pending press came back: {answer}"
    );
    assert_eq!(entries[0]["seq"], "2");
    assert_eq!(entries[0]["door"], "input");
    assert_eq!(entries[0]["at_ps"], (now + 50_000_000_000).to_string());

    // Inputs after the restore reuse the discarded timeline's numbers, with their own doors.
    ok_json(call(
        handle,
        r#"{"cmd":"input","args":{"button":"down","action":"click"}}"#,
    ));
    let answer = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    let doors: Vec<(String, String)> = answer["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| {
            (
                e["seq"].as_str().unwrap().to_string(),
                e["door"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        doors,
        [
            ("2".to_string(), "input".to_string()),
            ("3".to_string(), "registry".to_string()),
            ("4".to_string(), "registry".to_string()),
        ]
    );
    pemu_drop(handle);

    // A registry restore answers format 2 as well.
    let handle = erased_handle("");
    assert_eq!(journal_of(handle).len(), 0, "format 1 before any restore");
    let saved = ok_json(call(
        handle,
        r#"{"cmd":"snapshot","args":{"op":"save","name":"here"}}"#,
    ));
    assert!(saved["json"].is_object(), "{saved}");
    ok_json(call(
        handle,
        r#"{"cmd":"snapshot","args":{"op":"restore","name":"here"}}"#,
    ));
    let answer = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(answer["format"], 2, "{answer}");
    assert_eq!(answer["from"]["next_seq"], "0", "{answer}");
    assert_eq!(
        answer["from"]["snapshot"].as_str().map(str::len),
        Some(64),
        "the registry restore names its snapshot too: {answer}"
    );
    pemu_drop(handle);
}

/// Liveness is the journal entry's origin, not the door, so a live chunk restored under a `seq`
/// the door table calls `registry` still leaves the export. Synthetic samples.
#[test]
fn a_live_chunk_restored_under_a_reused_seq_is_still_dropped_from_the_journal() {
    let handle = erased_handle("");
    let (status, empty) = bytes_of(pemu_snapshot(handle, 0));
    assert_eq!(status, STATUS_OK);
    let live = br#"[{"at":"now","event":{"MicChunk":{"seq":0,"samples":[11,22,33]}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, live.as_ptr(), live.len() as u32) });
    let (status, with_live) = bytes_of(pemu_snapshot(handle, 0));
    assert_eq!(status, STATUS_OK);
    // SAFETY: live slice.
    let (status, _) = take(unsafe { pemu_restore(handle, empty.as_ptr(), empty.len() as u32) });
    assert_eq!(status, STATUS_OK);
    let tone = ok_json(call(
        handle,
        r#"{"cmd":"mic_set","args":{"kind":"tone","hz":440,"amplitude":100,"duration_ms":20}}"#,
    ));
    assert!(tone["json"].is_object(), "{tone}");
    // SAFETY: live slice.
    let (status, _) =
        take(unsafe { pemu_restore(handle, with_live.as_ptr(), with_live.len() as u32) });
    assert_eq!(status, STATUS_OK);
    let answer = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    let entry = answer["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["seq"] == "0")
        .expect("the live chunk came back with the snapshot")
        .clone();
    assert!(
        entry["event"].is_null(),
        "the live samples leaked: {answer}"
    );
    assert_eq!(entry["dropped"]["kind"], "MicChunk");
    assert_eq!(answer["replayable"], false);
    pemu_drop(handle);
}

/// The replay journals only entries at or above `from.next_seq`, as the others came back with the
/// snapshot; without the inputs since the restore it ends elsewhere.
#[test]
fn a_restored_handle_s_journal_replays_from_its_snapshot() {
    let handle = erased_handle("");
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let now = pemu_now_ps(handle) as u64;
    let ahead = format!(
        r#"[{{"at":"{}","event":{{"Button":{{"id":"Ok","down":true}}}}}}]"#,
        now + 1_000_000
    );
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, ahead.as_ptr(), ahead.len() as u32) });
    let (status, bytes) = bytes_of(pemu_snapshot(handle, 0));
    assert_eq!(status, STATUS_OK);
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    // SAFETY: live slice.
    let (status, _) = take(unsafe { pemu_restore(handle, bytes.as_ptr(), bytes.len() as u32) });
    assert_eq!(status, STATUS_OK);

    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let up = br#"[{"at":"now","event":{"Button":{"id":"Ok","down":false}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, up.as_ptr(), up.len() as u32) });
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    ok_json(call(
        handle,
        r#"{"cmd":"input","args":{"button":"down","action":"click"}}"#,
    ));
    assert_eq!(pemu_run(handle, -1, 20_000), StopCode::MaxInsns as u32);
    let restored = ok_json(call(handle, r#"{"cmd":"@report"}"#))["report"]
        .as_str()
        .expect("a report line")
        .to_string();
    let vt = pemu_now_ps(handle) as u64;

    let answer = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(answer["format"], 2, "{answer}");
    assert_eq!(answer["replayable"], true, "{answer}");
    let next_seq: u64 = answer["from"]["next_seq"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("`from.next_seq` is a decimal string");
    let since: Vec<(pemu_machine::machine::At, InputEvent)> = answer["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| e["seq"].as_str().unwrap().parse::<u64>().unwrap() >= next_seq)
        .map(|e| {
            let at: u64 = e["at_ps"].as_str().unwrap().parse().unwrap();
            (
                pemu_machine::machine::At::Vt(pemu_core::time::VTime(at)),
                serde_json::from_value(e["event"].clone()).expect("an InputEvent"),
            )
        })
        .collect();
    assert_eq!(
        since.len(),
        3,
        "the release and the click's two edges: {answer}"
    );

    let snapshot = pemu_core::snap::Snapshot::from_bytes(&bytes).expect("the snapshot decodes");
    // A replay restores only the snapshot `from` names.
    let named = answer["from"]["snapshot"]
        .as_str()
        .expect("`from.snapshot` is a hex hash");
    let hex = |hash: [u8; 32]| hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(named, hex(snapshot.canonical_hash()), "{answer}");
    let (status, later) = bytes_of(pemu_snapshot(handle, 0));
    assert_eq!(status, STATUS_OK);
    let other = pemu_core::snap::Snapshot::from_bytes(&later).expect("another snapshot decodes");
    assert_ne!(
        named,
        hex(other.canonical_hash()),
        "a later snapshot is refused"
    );
    let replay = |inputs: &[(pemu_machine::machine::At, InputEvent)]| {
        let mut m = native(MachineConfig::default());
        m.restore(&snapshot)
            .expect("the handle's snapshot restores natively");
        assert_eq!(m.journal().next_seq(), next_seq);
        for (at, event) in inputs {
            m.input(*at, event.clone())
                .expect("an input since the restore is not in the snapshot's past");
        }
        let out = m.run(RunLimits {
            until: Some(pemu_core::time::VTime(vt)),
            max_insns: None,
            stops: StopSet::default(),
        });
        (report(&out.reason, &m), m)
    };
    let field = |line: &str, name: &str| {
        line.split(' ')
            .find(|f| f.starts_with(name))
            .map(str::to_string)
    };
    let (native_report, mut replayed) = replay(&since);
    for name in ["state=", "insns=", "vt=", "lines="] {
        assert_eq!(
            field(&native_report, name),
            field(&restored, name),
            "`{name}` of the replay differs\nnative: {native_report}\nabi:    {restored}"
        );
    }

    // Compared here rather than through `usj=` and `uart0=` of the reports: a snapshot carries
    // only the ring heads, so the native replay's window opens empty there while the handle still
    // holds what it printed. The heads agree, which is the determinism claim.
    instance::with_table(|table| {
        let abi = table
            .get(handle)
            .expect("the handle is live")
            .machine()
            .io();
        let mut expected = Vec::new();
        for stream in [SerialStream::UsjTx, SerialStream::Uart0Tx] {
            let ring = abi.serial_ring(stream);
            expected.push((
                ring.head(),
                ring.tail(),
                ring.slices(ring.tail())
                    .iter()
                    .copied()
                    .collect::<Vec<u8>>(),
            ));
        }
        let io = replayed.io();
        for (stream, (head, tail, bytes)) in [SerialStream::UsjTx, SerialStream::Uart0Tx]
            .into_iter()
            .zip(expected)
        {
            let ring = io.serial_ring(stream);
            assert_eq!(
                ring.head(),
                head,
                "{stream:?}: the replay printed a different number of bytes"
            );
            // From the first byte both still hold.
            let from = ring.tail().max(tail);
            let ours: Vec<u8> = ring.slices(from).iter().copied().collect();
            let theirs: Vec<u8> = bytes[(from - tail) as usize..].to_vec();
            assert_eq!(
                String::from_utf8_lossy(&ours),
                String::from_utf8_lossy(&theirs),
                "{stream:?}: the replay printed different bytes from cursor {from}"
            );
        }
    });
    assert_ne!(
        field(&replay(&[]).0, "state="),
        field(&restored, "state="),
        "the replay without the inputs since the restore ends in the same state"
    );
    pemu_drop(handle);
}

/// The key rule lives in `WifiAp::check`, which `Machine::input_from` runs, so this door refuses
/// the key the `env` command does; a rule in the `env` handler alone would let the event reach the
/// module state untainted, and `snapshot export` would write the key out.
#[test]
fn the_raw_input_door_refuses_a_key_the_secret_set_cannot_hold() {
    let handle = erased_handle("");
    let weak = |psk: &str| {
        format!(
            r#"[{{"at":"now","event":{{"Env":{{"WifiAps":[{{"ssid":"G2-Secure","bssid":[2,0,0,71,50,6],"rssi":-55,"channel":6,"authmode":3,"psk":{:?}}}]}}}}}}]"#,
            psk.as_bytes()
        )
    };
    for psk in ["abc", "12345", "aaaaaaaa"] {
        let json = weak(psk);
        // SAFETY: live slice.
        let (status, body) =
            err_code(unsafe { pemu_input(handle, json.as_ptr(), json.len() as u32) });
        assert_eq!(
            status,
            u32::from(E_USAGE.number),
            "`{psk}` must be refused at the raw door: {body}"
        );
    }
    let empty = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(
        empty["entries"].as_array().expect("entries").len(),
        0,
        "a refused input is never journaled: {empty}"
    );

    // A key the set keeps is accepted through the same door.
    let good = weak("scripted-key");
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, good.as_ptr(), good.len() as u32) });
    let after = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(after["entries"].as_array().expect("entries").len(), 1);
    pemu_drop(handle);
}

/// A scripted Wi-Fi key and an NFC reader frame render as number arrays no string masker can see,
/// and a reserved `@` request runs no redaction, so the entries are dropped whole. An access point
/// list with no key stays.
#[test]
fn a_scripted_wifi_key_never_reaches_a_journal_export() {
    let handle = erased_handle("");
    let ok = ok_json(call(
        handle,
        r#"{"cmd":"env","args":{"wifi":{"aps":[{"ssid":"G2-Secure","bssid":"02:00:00:47:32:06","rssi":-55,"channel":6,"auth":3,"psk":"scripted-key"}]}}}"#,
    ));
    assert!(ok["json"].is_object(), "{ok}");
    let taps =
        r#"[{"at":"now","event":{"NfcTap":{"ops":["FieldOn",{"Cmd":[27,1,2,3,4]},"FieldOff"]}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, taps.as_ptr(), taps.len() as u32) });

    let default = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    let text = default.to_string();
    assert!(
        !text.contains("scripted-key") && !text.contains("115,99,114"),
        "the key is not in the answer in any form: {default}"
    );
    assert!(
        !text.contains("27,1,2,3,4"),
        "nor is the reader frame: {default}"
    );
    assert_eq!(default["replayable"], false);
    let kinds: Vec<&str> = default["dropped"]
        .as_array()
        .expect("dropped")
        .iter()
        .map(|d| d["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(kinds, ["WifiKeys", "NfcFrames"]);
    for entry in default["entries"].as_array().expect("entries") {
        assert!(entry["event"].is_null(), "{entry}");
    }

    // The explicit opt-in still answers with them, like a live payload.
    let full = ok_json(call(
        handle,
        r#"{"cmd":"@journal","args":{"include_secrets":true}}"#,
    ));
    assert_eq!(full["replayable"], true);
    assert!(full.to_string().contains("115,99,114"), "{full}");

    // An open network carries no secret, so its entry is exported whole.
    let handle2 = erased_handle("");
    let ok = ok_json(call(
        handle2,
        r#"{"cmd":"env","args":{"wifi":{"aps":[{"ssid":"G2-Alpha","bssid":"02:00:00:47:32:01","rssi":-42,"channel":1,"auth":0}]}}}"#,
    ));
    assert!(ok["json"].is_object(), "{ok}");
    let open = ok_json(call(handle2, r#"{"cmd":"@journal"}"#));
    assert_eq!(open["replayable"], true, "{open}");
    assert!(open.to_string().contains("G2-Alpha"), "{open}");
    pemu_drop(handle2);
    pemu_drop(handle);
}

/// Dropped unless `include_secrets`, with a marker naming them and `replayable: false`; a
/// registry `mic_set` tone is scripted and stays whole either way.
#[test]
fn live_microphone_chunks_are_dropped_from_the_journal_unless_asked() {
    let handle = erased_handle("");
    let live = r#"[{"at":"now","event":{"MicChunk":{"seq":0,"samples":[1,2,3]}}},
                   {"at":"now","event":{"MicChunk":{"seq":1,"samples":[4,5,6]}}}]"#;
    // SAFETY: live slice.
    ok_json(unsafe { pemu_input(handle, live.as_ptr(), live.len() as u32) });
    let tone = ok_json(call(
        handle,
        r#"{"cmd":"mic_set","args":{"kind":"tone","hz":440,"amplitude":16384,"duration_ms":20}}"#,
    ));
    assert!(tone["json"].is_object(), "{tone}");

    let default = ok_json(call(handle, r#"{"cmd":"@journal"}"#));
    assert_eq!(default["format"], 1);
    assert_eq!(default["replayable"], false);
    assert_eq!(
        default["dropped"],
        serde_json::json!([{"kind":"MicChunk","seq":"0"},{"kind":"MicChunk","seq":"1"}])
    );
    let entries = default["entries"].as_array().expect("entries");
    let live_entries: Vec<_> = entries.iter().filter(|e| e["door"] == "input").collect();
    assert_eq!(live_entries.len(), 2);
    for entry in &live_entries {
        assert!(entry["event"].is_null(), "{entry}");
        assert_eq!(entry["dropped"]["kind"], "MicChunk");
        assert_eq!(entry["dropped"]["len"], 3);
    }
    let registry_chunks = entries
        .iter()
        .filter(|e| e["door"] == "registry" && e["event"]["MicChunk"]["samples"].is_array())
        .count();
    assert!(
        registry_chunks > 0,
        "the registry tone's chunks stay whole: {default}"
    );
    assert!(!default.to_string().contains("[1,2,3]"), "{default}");

    let full = ok_json(call(
        handle,
        r#"{"cmd":"@journal","args":{"include_secrets":true}}"#,
    ));
    assert_eq!(full["replayable"], true);
    assert_eq!(full["dropped"], serde_json::json!([]));
    assert_eq!(
        full["entries"][0]["event"],
        serde_json::json!({"MicChunk":{"seq":0,"samples":[1,2,3]}})
    );

    let (status, body) = err_code(call(
        handle,
        r#"{"cmd":"@journal","args":{"include_secrets":"yes"}}"#,
    ));
    assert_eq!(status, u32::from(E_USAGE.number), "{body}");
    let (status, body) = err_code(call(handle, r#"{"cmd":"@journal","args":{"all":true}}"#));
    assert_eq!(status, u32::from(E_USAGE.number), "{body}");
    pemu_drop(handle);
}
