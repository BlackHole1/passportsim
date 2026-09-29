//! Milestone M11 tests: timing calibration. Names use the prefix `t<tier>_m11_` so `xtask ci` can
//! count them.
//!
//! The boot tests run the M8 `pk` boot (300 ms of virtual time, the U3 client stated, the esptool
//! line reset, then the `rst:0x15` boot to `pk_app: link state -1 -> 0`) and compare its ESP_LOG
//! timestamps with the device's, under the `device` column of `specs/timing-profiles.toml`, whose
//! `[[anchor]]` rows hold the anchors, their device timestamps and the fit/validation split
//! (`pemu_verify::calibrate`).

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::{console_bytes, machine, pk_files};

use pemu_core::clock::{CacheVariant, TIMING_PROFILES_TOML, TimingProfile};
use pemu_core::hostio::SerialStream;
use pemu_core::input::InputEvent;
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_machine::config::{Assets, MachineConfig, TimingProfileId};
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
use pemu_testkit::golden::{BootSelect, Console};
use pemu_verify::calibrate::{self, Plan, Role};

/// The last line of the boot (dev:L80).
const PK_BOOT_LAST: &str = "pk_app: link state -1 -> 0";

/// The lines of dev:L4-L80.
const PK_BOOT_LINES: usize = 77;

fn device_cfg(timing: TimingProfile) -> MachineConfig {
    MachineConfig {
        profile: TimingProfileId::Device,
        timing: Some(timing),
        ..MachineConfig::default()
    }
}

/// The boot: the raw console bytes of the whole run.
fn pk_boot(flash: &[u8], elf: &[u8], cfg: MachineConfig) -> Vec<u8> {
    let mut m = machine(flash, elf, cfg);
    let reset_at = pk_boot_reset(&mut m);
    loop {
        let bytes = console_bytes(&mut m);
        if String::from_utf8_lossy(&bytes[reset_at..]).contains(PK_BOOT_LAST) {
            return bytes;
        }
        assert!(m.now() < VTime::from_ms(12_000), "no `{PK_BOOT_LAST}`");
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 10_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until);
    }
}

/// The boot up to its reset: 300 ms of virtual time, the U3 client stated, the esptool line reset;
/// returns the console length at the reset.
fn pk_boot_reset(m: &mut Machine) -> usize {
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(300)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(out.reason, StopReason::Until);
    m.input(At::Now, InputEvent::UsbClient { open: true })
        .expect("an input at now is accepted");
    m.input(
        At::Now,
        InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("an input at now is accepted");
    console_bytes(m).len()
}

fn normalized(bytes: &[u8]) -> Console {
    pemu_verify::normalize::normalize(bytes, BootSelect::LastBoot)
}

fn plan() -> Plan {
    Plan::parse(TIMING_PROFILES_TOML).expect("the committed calibration plan parses")
}

/// Each plan anchor's first timestamp in `console`, in plan order.
fn anchor_times(plan: &Plan, console: &Console) -> Vec<Option<u32>> {
    plan.anchors
        .iter()
        .map(|a| console.first_ts(&a.line))
        .collect()
}

fn with_values(plan: &Plan, values: &[i64]) -> TimingProfile {
    let mut p = TimingProfile::device().clone();
    for (param, v) in plan.params.iter().zip(values) {
        p.set_by_name(
            &param.name,
            u64::try_from(*v).expect("a fit value is not negative"),
        )
        .expect("a fit row names a numeric constant");
    }
    p
}

fn committed(plan: &Plan) -> Vec<i64> {
    plan.params
        .iter()
        .map(|p| {
            let v = TimingProfile::device()
                .get_by_name(&p.name)
                .expect("a fit row names a numeric constant");
            i64::try_from(v).expect("the committed value fits")
        })
        .collect()
}

/// The least-squares fit on the fit anchors of dev:L4-L80 reproduces the committed `device`
/// values.
///
/// The model is a step function at the 1 ms ESP_LOG resolution, so an unrelated instruction-count
/// change may move the minimum a few quanta: each fitted value must be within 5 % of the committed
/// one, and the committed values must fit no worse than the fit's own result plus 1 ms RMS.
/// `PEMU_CALIBRATE_WRITE=1` writes the fitted values into `specs/timing-profiles.toml` instead.
#[test]
fn t1_m11_calibrate_fits_the_committed_device_profile() {
    let test = "t1_m11_calibrate_fits_the_committed_device_profile";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let plan = plan();
    let mut run = |values: &[i64]| {
        let console = normalized(&pk_boot(
            &flash,
            &elf,
            device_cfg(with_values(&plan, values)),
        ));
        anchor_times(&plan, &console)
    };
    let fitted = calibrate::fit(&plan, &mut run).expect("the fit runs");
    let report = calibrate::render_report(&plan, &fitted);
    println!("{report}");
    let cold = cold_page_fit(&flash, &elf, &plan);
    let rms = |r: &calibrate::Residuals, role: Role| r.rms_ms(&role).unwrap_or(f64::NAN);
    let variant = TimingProfile::device().cache_model.as_str();
    println!(
        "cache model: {variant} fit RMS {:.2} ms, validation RMS {:.2} ms; cold_page (sha_block_ps \
         fitted in place of cache_fill_ps) fit RMS {:.2} ms, validation RMS {:.2} ms \
         at {:?}. Both are inside the bands on the boot; {variant} is committed on the probe \
         evidence",
        rms(&fitted.residuals, Role::Fit),
        rms(&fitted.residuals, Role::Validation),
        rms(&cold.residuals, Role::Fit),
        rms(&cold.residuals, Role::Validation),
        cold.values
    );

    if std::env::var_os("PEMU_CALIBRATE_WRITE").is_some() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../specs/timing-profiles.toml");
        let text = std::fs::read_to_string(&path).expect("the table is readable");
        let values: Vec<(&str, i64)> = plan
            .params
            .iter()
            .zip(&fitted.values)
            .map(|(p, v)| (p.name.as_str(), *v))
            .collect();
        let out = calibrate::write_device_values(&text, &values).expect("the rows exist");
        std::fs::write(&path, out).expect("the table is writable");
        println!("WROTE specs/timing-profiles.toml: {values:?}");
        return;
    }

    let committed = committed(&plan);
    for ((p, fit), have) in plan.params.iter().zip(&fitted.values).zip(&committed) {
        let tolerance = (have.abs() / 20).max(p.quantum);
        assert!(
            fit.abs_diff(*have) <= tolerance.unsigned_abs(),
            "{id}: the fit gives {} = {fit}, the table commits {have}\n{report}",
            p.name
        );
    }
    let at_committed = calibrate::residuals(&plan, &run(&committed));
    let rms_committed = at_committed
        .rms_ms(&Role::Fit)
        .expect("every fit phase printed");
    let rms_fit = fitted
        .residuals
        .rms_ms(&Role::Fit)
        .expect("every fit phase printed");
    assert!(
        rms_committed <= rms_fit + 1.0,
        "{id}: the committed values fit to {rms_committed:.2} ms RMS, the fit to {rms_fit:.2} ms"
    );
    println!(
        "RAN {test}: {} parameters, {} boots, fit RMS {rms_fit:.2} ms, validation RMS {:.2} ms",
        plan.params.len(),
        fitted.evaluations,
        fitted
            .residuals
            .rms_ms(&Role::Validation)
            .unwrap_or(f64::NAN)
    );
}

/// The `cold_page` variant fitted on the same boot, with `sha_block_ps` in place of
/// `cache_fill_ps`: a 4 KB page fill and a SHA block are both proportional to the bytes the
/// bootloader verifies, so `sha_block_ps` carries both. The `[[fit]]` rows serve `lru16k`.
fn cold_page_fit(flash: &[u8], elf: &[u8], plan: &Plan) -> calibrate::Fitted {
    let mut cold_plan = plan.clone();
    cold_plan.params.retain(|p| p.name != "cache_fill_ps");
    cold_plan.params.insert(
        1.min(cold_plan.params.len()),
        calibrate::FitParam {
            name: "sha_block_ps".into(),
            start: 2_800_000,
            step: 1_000_000,
            min: 0,
            max: 20_000_000,
            quantum: 10_000,
        },
    );
    let mut run = |values: &[i64]| {
        let mut timing = with_values(&cold_plan, values);
        timing.cache_model = CacheVariant::ColdPage;
        timing.cache_fill_ps = 0;
        let console = normalized(&pk_boot(flash, elf, device_cfg(timing)));
        anchor_times(&cold_plan, &console)
    };
    calibrate::fit(&cold_plan, &mut run).expect("the cold_page fit runs")
}

/// The logged silicon boots the `buttons` anchor's evidence band is derived from, relative to the
/// data root. The first is the reference boot itself.
const BUTTONS_BAND_BOOTS: [&str; 13] = [
    "device/boot_log.bin",
    "device/golden/logs/audit-boot-console.log",
    "device/golden/logs/audit-final-boot.log",
    "device/golden/logs/audit-pre-reset-drain.log",
    "device/golden/logs/device-capture-selftest.log",
    "device/golden/logs/device-open-test-first.log",
    "device/golden/logs/device-open-test-second.log",
    "device/golden/logs/device-open-test2-A1.log",
    "device/golden/logs/device-open-test2-baseline_A.log",
    "device/golden/logs/device-open-test2-reset_C.log",
    "device/golden/logs/device-pre.log",
    "device/golden/logs/device-pre.predrain.log",
    "device/golden/logs/passport-keys-restored.log",
];

/// Re-derives every evidence band of the plan from the logged boots it cites, when the data root
/// holds them: a band must be exactly the widest distance of a logged boot's phase from the
/// reference phase, so it can be neither hand-widened nor left stale. Only timestamps are read.
fn check_evidence_bands(test: &str, id: &str, plan: &Plan) {
    let banded: Vec<(usize, u32)> = plan
        .anchors
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.evidence_band.as_ref().map(|b| (i, b.delta_ms)))
        .collect();
    assert_eq!(
        banded
            .iter()
            .map(|(i, _)| plan.anchors[*i].name.as_str())
            .collect::<Vec<_>>(),
        ["buttons"],
        "{id}: the only evidence band is the one derived from BUTTONS_BAND_BOOTS"
    );
    let Some(root) = pemu_testkit::corpus::data_root_from_env().ok() else {
        println!("SKIP-PART {test}: no data root, the evidence band is not re-derived");
        return;
    };
    let (at, band) = banded[0];
    let (from, to) = (&plan.anchors[at - 1], &plan.anchors[at]);
    let reference = to.device_ms - from.device_ms;
    let mut phases = Vec::new();
    for rel in BUTTONS_BAND_BOOTS {
        let Ok(bytes) = std::fs::read(root.join(rel)) else {
            println!("SKIP-PART {test}: {rel} is missing, the evidence band is not re-derived");
            return;
        };
        let boot = normalized(&bytes);
        let (Some(a), Some(b)) = (boot.first_ts(&from.line), boot.first_ts(&to.line)) else {
            panic!(
                "{id}: {rel} does not print both `{}` and `{}`",
                from.line, to.line
            );
        };
        phases.push(b - a);
    }
    assert_eq!(
        phases[0], reference,
        "{id}: the first log is the pk boot reference"
    );
    let widest = phases
        .iter()
        .map(|p| p.abs_diff(reference))
        .max()
        .expect("13 boots");
    println!(
        "  evidence band {} -> {}: {} logged silicon boots read {:?} ms, reference {reference}, \
         band +/-{widest}",
        from.name,
        to.name,
        phases.len(),
        phases
    );
    assert_eq!(
        band, widest,
        "{id}: the `{}` band is not the widest distance of a logged boot from the reference",
        to.name
    );
}

/// Under the `device` profile, every validation anchor lies inside the bands of
/// `pemu_verify::bands::{DELTA, ABSOLUTE}`, deltas first. When the data root has the device
/// capture, each `[[anchor]]` row must name the line and timestamp dev:L<n> really carries.
#[test]
fn t1_m11_validation_anchors_inside_the_bands_under_device() {
    let test = "t1_m11_validation_anchors_inside_the_bands_under_device";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let plan = plan();

    match pemu_testkit::corpus::data_root_from_env()
        .ok()
        .and_then(|root| std::fs::read(root.join("device").join("boot_log.bin")).ok())
    {
        Some(bytes) => {
            let device = normalized(&bytes);
            for a in &plan.anchors {
                let n: usize = a.dev.trim_start_matches('L').parse().expect("an L<n> row");
                // dev:L<n> counts from the file's first line; lines 1 to 3 are the tail of an earlier banner.
                let line = &device.lines[n - 4];
                assert!(
                    line.text.contains(&a.line),
                    "{id}: {} names `{}` but that line reads `{}`",
                    a.dev,
                    a.line,
                    line.text
                );
                assert_eq!(
                    device.first_ts(&a.line),
                    Some(a.device_ms),
                    "{id}: the table's device timestamp of {} ({})",
                    a.name,
                    a.dev
                );
            }
        }
        None => println!("SKIP-PART {test}: no device boot log, the committed timestamps are used"),
    }
    check_evidence_bands(test, &id, &plan);

    let console = normalized(&pk_boot(
        &flash,
        &elf,
        device_cfg(TimingProfile::device().clone()),
    ));
    let r = calibrate::residuals(&plan, &anchor_times(&plan, &console));
    for p in &r.phases {
        println!(
            "  {:<10} device {:>4} emulated {:>4} allowed +/-{:<3} {} -> {}{}",
            if p.role == Role::Fit {
                "fit"
            } else {
                "validation"
            },
            p.device_ms,
            p.emulated_ms.map_or(-1, i64::from),
            p.allowed_ms,
            p.from,
            p.to,
            if p.evidence_band {
                " (band from silicon evidence)"
            } else {
                ""
            }
        );
    }
    let failures = r.band_failures();
    assert!(
        failures.is_empty(),
        "{id}: outside the evidence bands:\n{}",
        failures.join("\n")
    );
    let validated = r
        .phases
        .iter()
        .filter(|p| p.role == Role::Validation)
        .count();
    println!(
        "RAN {test}: {validated} validation phases and {} absolute anchors inside the bands, {} \
         excluded, {} delta band(s) derived from silicon evidence",
        r.absolutes.len(),
        plan.anchors
            .iter()
            .filter(|a| matches!(a.role, Role::Excluded(_)))
            .count(),
        r.phases.iter().filter(|p| p.evidence_band).count()
    );
}

/// Attribution aid for the fit residue: the boot's phases under the committed `device` column and
/// under each variant of `PEMU_PHASE_VARIANTS` (`label:row=value,row=value;label:...`), so a
/// phase's share of one mechanism is the difference a row set to 0 makes.
#[test]
#[ignore = "prints the pk boot phases under the variants of PEMU_PHASE_VARIANTS; run with --ignored"]
fn m11_phase_variants() {
    let test = "m11_phase_variants";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let plan = plan();
    let spec = std::env::var("PEMU_PHASE_VARIANTS").unwrap_or_default();
    let mut variants = vec![("committed".to_string(), TimingProfile::device().clone())];
    for v in spec.split(';').filter(|v| !v.is_empty()) {
        let (label, rows) = v.split_once(':').expect("label:row=value,...");
        let mut p = TimingProfile::device().clone();
        for kv in rows.split(',').filter(|kv| !kv.is_empty()) {
            let (name, value) = kv.split_once('=').expect("row=value");
            p.set_by_name(name, value.parse().expect("a number"))
                .expect("a numeric row");
        }
        variants.push((label.to_string(), p));
    }
    for (label, p) in variants {
        let console = normalized(&pk_boot(&flash, &elf, device_cfg(p)));
        let r = calibrate::residuals(&plan, &anchor_times(&plan, &console));
        let cells: Vec<String> = r
            .phases
            .iter()
            .map(|p| format!("{}={}", p.to, p.emulated_ms.map_or(-1, i64::from)))
            .collect();
        println!("VARIANT {label}: {}", cells.join(" | "));
    }
}

/// Attribution aid for the application phases: the boot run in 5 µs slices, each phase from
/// `main task` on split into the time the hart waited in `wfi`, the time its clock position
/// accounts for at 160 MHz, and the rest (cache stalls), and its time by where the pc was at the
/// end of each slice. The slices do not move the boot; the split can be up to a slice and a
/// line's drain longer than the ESP_LOG phase.
#[test]
#[ignore = "prints the pk boot phases split into execution, stalls and idle; run with --ignored --nocapture"]
fn m11_phase_profile() {
    const SLICE_PS: u64 = 5_000_000;
    const CYCLE_PS: u64 = 6_250;
    #[derive(Default, Clone)]
    struct Split {
        ps: u64,
        idle_ps: u64,
        pos: u64,
        insns: u64,
        at: [u64; 4],
    }
    let test = "m11_phase_profile";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let plan = plan();
    let mut m = machine(&flash, &elf, device_cfg(TimingProfile::device().clone()));
    let reset_at = pk_boot_reset(&mut m);
    let mut split = vec![Split::default(); plan.anchors.len()];
    let mut seen = vec![false; plan.anchors.len()];
    loop {
        let (insns, pos) = (m.hart().insns, m.hart().pos());
        let out = m.run(RunLimits {
            until: Some(VTime(m.now().0 + SLICE_PS)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until);
        if let Some(k) = seen.iter().position(|s| !s) {
            let h = m.hart();
            let at = match h.pc {
                _ if h.wfi => 3,
                0x4200_0000..0x4280_0000 => 0,
                0x4037_c000..0x403e_0000 => 1,
                _ => 2,
            };
            let s = &mut split[k];
            s.ps += SLICE_PS;
            s.idle_ps += out.idle_ps;
            s.pos += h.pos() - pos;
            s.insns += h.insns - insns;
            s.at[at] += SLICE_PS;
        }
        let bytes = console_bytes(&mut m);
        let text = String::from_utf8_lossy(&bytes[reset_at..]).into_owned();
        for (k, a) in plan.anchors.iter().enumerate() {
            seen[k] |= text.contains(a.line.as_str());
        }
        if text.contains(PK_BOOT_LAST) {
            break;
        }
        assert!(m.now() < VTime::from_ms(12_000), "no `{PK_BOOT_LAST}`");
    }
    let console = normalized(&console_bytes(&mut m));
    let times = anchor_times(&plan, &console);
    let ms = |ps: u64| ps as f64 / 1e9;
    let first = plan
        .anchors
        .iter()
        .position(|a| a.name == "main task")
        .expect("the plan names `main task`");
    for k in first + 1..plan.anchors.len() {
        let (a, prev) = (&plan.anchors[k], &plan.anchors[k - 1]);
        let s = &split[k];
        let busy = s.ps - s.idle_ps;
        let exec = s.pos * CYCLE_PS;
        let model = match (times[k], times[k - 1]) {
            (Some(t), Some(p)) => i64::from(t) - i64::from(p),
            _ => -1,
        };
        println!(
            "PHASE {} -> {}: device {} ms, model {model} ms; slices {:.2} ms: idle {:.2}, \
             executing {:.2} ({:.3} M cycles, {:.3} M instructions), stalled {:.2}; pc in flash \
             {:.2}, IRAM {:.2}, ROM {:.2}, wfi {:.2}",
            prev.name,
            a.name,
            i64::from(a.device_ms) - i64::from(prev.device_ms),
            ms(s.ps),
            ms(s.idle_ps),
            ms(exec),
            s.pos as f64 / 1e6,
            s.insns as f64 / 1e6,
            ms(busy.saturating_sub(exec)),
            ms(s.at[0]),
            ms(s.at[1]),
            ms(s.at[2]),
            ms(s.at[3]),
        );
    }
}

/// `fast` and `device` give identical normalized text for every boot golden: the M8 boot against
/// `pk.console.txt` (all 77 lines under both) and the `probe_limits` boot, compared as whole text.
#[test]
fn t1_m11_fast_and_device_give_the_same_boot_text() {
    let test = "t1_m11_fast_and_device_give_the_same_boot_text";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };
    let fast = pk_boot(&flash, &elf, MachineConfig::default());
    let device = pk_boot(
        &flash,
        &elf,
        MachineConfig {
            profile: TimingProfileId::Device,
            ..MachineConfig::default()
        },
    );
    for (name, bytes) in [("fast", &fast), ("device", &device)] {
        let compared =
            common::assert_console_prefix("pk.console.txt", &golden, bytes, Some(PK_BOOT_LINES));
        assert_eq!(compared, PK_BOOT_LINES, "{id}: {name}");
    }
    assert_eq!(
        normalized(&fast).to_text(),
        normalized(&device).to_text(),
        "{id}: the pk boot"
    );

    let mut probes = 0;
    if let Some((image, _)) = probe_image(test, "probe_limits") {
        let run = |profile| probe_console(&image, profile, "DONE|");
        let (f, d) = (run(TimingProfileId::Fast), run(TimingProfileId::Device));
        assert!(f.contains("DONE|"), "{id}: probe_limits under fast");
        assert_eq!(
            normalized(f.as_bytes()).to_text(),
            normalized(d.as_bytes()).to_text(),
            "{id}: probe_limits"
        );
        probes += 1;
    }
    println!(
        "RAN {test}: pk boot ({PK_BOOT_LINES} lines) and {probes} probe boot(s) equal under both profiles"
    );
}

/// The ROM-phase gap: the device prints the bootloader banner (dev:L14) at 24 ms, `fast` at 7. The
/// ROM phase is bound by the console, not by the ROM's own work.
///
/// Four runs, console pacing (`uart_paced`, `usj_drain_ps`) on and off, without and with the
/// per-class instruction costs. Paced, the banner lands in its absolute band and does not move
/// with the costs, so no fitted constant hides in this anchor; unpaced without costs it is within
/// 3 ms of `fast`; unpaced with costs it moves with them and stays earlier than the paced banner.
/// The pacing is the UART0 drain at 115200 baud (class A) plus the USJ host poll (class C, 125 us).
#[test]
fn t1_m11_the_rom_phase_gap_is_the_console_pacing() {
    let test = "t1_m11_the_rom_phase_gap_is_the_console_pacing";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let plan = plan();
    let banner = &plan.anchors[0];
    assert_eq!(
        banner.dev, "L14",
        "{id}: the first anchor is the bootloader banner"
    );
    let at = |paced: bool, costed: bool| {
        let mut timing = TimingProfile::device().clone();
        if !paced {
            timing.uart_paced = false;
            timing.usj_drain_ps = 0;
        }
        if !costed {
            timing.cpi_milli = 1000;
            for row in [
                "taken_branch_cycles",
                "jump_cycles",
                "split_redirect_cycles",
                "load_use_cycles",
                "div_base_cycles",
                "mulh_cycles",
                "mmio_cpu_cycles",
                "mmio_load_apb_cycles",
                "mmio_store_apb_cycles",
                "sram_bank_cycles",
            ] {
                timing.set_by_name(row, 0).expect("a class row");
            }
        }
        normalized(&pk_boot(&flash, &elf, device_cfg(timing)))
            .first_ts(&banner.line)
            .expect("the banner printed")
    };
    let fast = normalized(&pk_boot(&flash, &elf, MachineConfig::default()))
        .first_ts(&banner.line)
        .expect("the banner printed");
    let (paced_1, paced_cost) = (at(true, false), at(true, true));
    let (unpaced_1, unpaced_cost) = (at(false, false), at(false, true));
    println!(
        "banner: device capture {} ms, fast {fast}; paced {paced_1} (no costs) {paced_cost} \
         (device costs); unpaced {unpaced_1} (no costs) {unpaced_cost} (device costs)",
        banner.device_ms
    );
    assert!(
        pemu_verify::bands::ABSOLUTE.allows(banner.device_ms, paced_cost),
        "{id}: the banner at {paced_cost} ms is outside the absolute band of {} ms",
        banner.device_ms
    );
    assert!(
        paced_1.abs_diff(paced_cost) <= 1,
        "{id}: the paced banner moved with the costs ({paced_1} -> {paced_cost} ms), so a \
         fitted term is in the ROM phase"
    );
    assert!(
        unpaced_1.abs_diff(fast) <= 3,
        "{id}: without pacing and costs the banner is at {unpaced_1} ms, not back at fast's {fast}"
    );
    assert!(
        unpaced_cost > unpaced_1 && unpaced_cost < paced_cost,
        "{id}: unpaced, the ROM work should move with the costs and finish before the drain \
         ({unpaced_1} -> {unpaced_cost} ms against {paced_cost})"
    );
    println!(
        "RAN {test}: the console drain holds the banner at {paced_cost} ms with or without the \
         costs; the ROM work alone ends at {unpaced_1} to {unpaced_cost} ms"
    );
}

/// The capture: three runs of the approved `probe_timing` build.
const PROBE_TIMING_CAPTURE: &str = "device-probe_timing-20260923T101040Z";

/// The `TIMING|<what>|us=<n>` values of one console, by name.
fn timing_lines(text: &str) -> std::collections::BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("TIMING|")?;
            let mut fields = rest.split('|');
            let name = fields.next()?.to_string();
            let us = fields.find_map(|f| f.strip_prefix("us="))?.parse().ok()?;
            Some((name, us))
        })
        .collect()
}

/// The `device` constants reproduce the `probe_timing` capture's deltas within 20 %.
///
/// Against the mean of the three device runs: `spi2_153600` (SPI2 clocked), `i2c_read_100` (I2C0
/// clocked, CPU cost), `flash_read_64k` (SPI1 clocked, the esp_flash read loop in IRAM, the XIP
/// fetch charged per 32-byte line miss under `lru16k`) and `sha256_1m`, whose cache-resident loop
/// runs 12.16 M instructions at no more than 1.40 cycles each on silicon. The per-class costs of
/// `probe_campaign_timing` `TIME|cpi_<class>` and a `sha_block_ps` derived from
/// `TIME|sha256_64k_prefilled` make `sha256_1m` an independent check of both. Its digest is the
/// device's (mbedTLS SHA in DMA mode).
///
/// `erase_4k` was not measured (the device table has no `scratch` partition), so `flash_se_ps`
/// stays a class C constant.
#[test]
fn t1_m11_probe_timing_deltas_within_20_percent() {
    let test = "t1_m11_probe_timing_deltas_within_20_percent";
    let id = test.to_string();
    let Some((image, _)) = probe_image(test, "probe_timing") else {
        return;
    };
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let mut runs = Vec::new();
    for n in 1..=3 {
        let path = root
            .join("captures")
            .join(format!("{PROBE_TIMING_CAPTURE}-run{n}.log"));
        match std::fs::read(&path) {
            Ok(bytes) => runs.push(timing_lines(&String::from_utf8_lossy(&bytes))),
            Err(_) => {
                common::skip(
                    test,
                    &format!("no capture {PROBE_TIMING_CAPTURE}-run{n}.log"),
                );
                return;
            }
        }
    }
    let emulated = probe_console(&image, TimingProfileId::Device, "DONE|");
    let emu = timing_lines(&emulated);
    let mut outside = Vec::new();
    let mut summary = Vec::new();
    for what in ["flash_read_64k", "spi2_153600", "i2c_read_100", "sha256_1m"] {
        let dev: u64 = runs.iter().map(|r| r[what]).sum::<u64>() / runs.len() as u64;
        let got = *emu
            .get(what)
            .unwrap_or_else(|| panic!("{id}: no `{what}` line:\n{emulated}"));
        let off = (got as f64 - dev as f64) / dev as f64 * 100.0;
        println!("  {what}: device {dev} us, emulated {got} us, {off:+.1} %");
        summary.push(format!("{what} {off:+.1} %"));
        if got.abs_diff(dev) * 5 > dev {
            outside.push(format!(
                "{what}: emulated {got} us, device {dev} us ({off:+.1} %)"
            ));
        }
    }
    assert!(
        outside.is_empty(),
        "{id}: outside 20 %:\n{}",
        outside.join("\n")
    );
    let device_digest = device_sha256_1m_digest(&root);
    let emulated_digest = sha256_1m_digests(&emulated);
    assert_eq!(
        emulated_digest,
        vec![device_digest.clone()],
        "{id}: the emulator's sha256_1m digest is not the device's (mbedTLS SHA-256 in DMA mode, \
         periph/sha.rs and wiring/sha.rs)"
    );
    println!(
        "RAN {test}: four deltas within 20 % ({}); sha256_1m prints the device digest \
         {device_digest}",
        summary.join(", ")
    );
}

/// Every well-formed `sha256_1m` digest a console printed. The device's run1 log opens with a line
/// cut short by the reset that started the capture, so a digest is only taken whole.
fn sha256_1m_digests(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with("TIMING|sha256_1m|"))
        .filter_map(|l| l.split('|').find_map(|f| f.strip_prefix("digest=")))
        .filter(|d| {
            d.len() == 64
                && d.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(str::to_string)
        .collect()
}

/// The digest the three device runs agree on; panics unless every run printed it whole.
fn device_sha256_1m_digest(root: &std::path::Path) -> String {
    let mut all = Vec::new();
    for n in 1..=3 {
        let path = root
            .join("captures")
            .join(format!("{PROBE_TIMING_CAPTURE}-run{n}.log"));
        let text = String::from_utf8_lossy(&std::fs::read(&path).expect("read by the caller"))
            .into_owned();
        let found = sha256_1m_digests(&text);
        assert_eq!(
            found.len(),
            1,
            "run{n}: one whole sha256_1m digest, got {found:?}"
        );
        all.extend(found);
    }
    assert!(
        all.windows(2).all(|w| w[0] == w[1]),
        "the device runs disagree: {all:?}"
    );
    all.remove(0)
}

fn probe_image(test: &str, name: &str) -> Option<(Vec<u8>, ElfInfo)> {
    let pk = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    let path = pk
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join(format!("probes/{name}-8MB.bin"));
    let Ok(bytes) = std::fs::read(&path) else {
        common::skip(test, &format!("no probe image {name}-8MB.bin"));
        return None;
    };
    let fw = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fw");
    let elf = ElfInfo::parse(
        &std::fs::read(fw.join(format!("{name}.elf"))).expect("the committed probe ELF"),
    )
    .expect("the probe ELF parses");
    Some((bytes, elf))
}

/// Runs a probe image from power-on under `profile` until a console line starts with `done`,
/// and returns the USJ console.
fn probe_console(image: &[u8], profile: TimingProfileId, done: &str) -> String {
    let flash = FlashImage::from_merged(image).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(
        MachineConfig {
            profile,
            ..MachineConfig::default()
        },
        assets,
    )
    .expect("the image fits");
    let stop = MatcherId(0xD0);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(10_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                stop,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix(done.into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let text = String::from_utf8_lossy(&console_bytes(&mut m)).into_owned();
    assert_eq!(out.reason, StopReason::Matcher(stop), "{text}");
    text
}

const PROBE_INTC_CAPTURE: &str = "device-probe_intc-20260917T120257Z.log";

const PROBE_CLOCKS_CAPTURE: &str = "device-probe_clocks-20260917T120350Z.log";

fn field(line: &str, key: &str) -> Option<u64> {
    line.split('|')
        .find_map(|f| f.strip_prefix(key)?.strip_prefix('='))?
        .parse()
        .ok()
}

/// The `cycles` of each line of `text` that starts with `prefix`, keyed by `key`'s value
/// (`LAT|index=`, `CLK|phase=`).
fn cycles_by(text: &str, prefix: &str, key: &str) -> std::collections::BTreeMap<String, u64> {
    text.lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| {
            let k = l
                .split('|')
                .find_map(|f| f.strip_prefix(key)?.strip_prefix('='))?;
            Some((k.to_string(), field(l, "cycles")?))
        })
        .collect()
}

/// The cycle counts of the `probe_intc` and `probe_clocks` device captures under the `device`
/// profile (per-class instruction costs, the restartable line fill).
///
/// - `probe_intc` LAT, samples 1 to 15 (trigger store to handler entry through the IDF vector,
///   IRAM, cache warm): silicon 172 cycles, emulated 194 (+12.8 %). Inside 20 %, asserted.
/// - `probe_intc` LAT, sample 0 (the first run of the flash-resident `trigger_a`): silicon 417,
///   172 plus 245 cycles of one line fill, shorter than the measured 313-cycle fill because the
///   next instructions partly overlap the fetch on silicon. The emulator charges the fill,
///   restarted at the first unconditional transfer in the line, and reads 534 (+28.1 %). Pinned
///   at +15 % to +30 %, so the test fails, and asks to be tightened, the day it moves.
/// - `probe_clocks` WFI phases (`task_delay`, `timer_poll`): silicon 34216 and 61207, emulated
///   -2.5 % and -0.7 %. Inside 20 %, asserted.
#[test]
fn t1_cycle_counts_against_the_device_captures() {
    let test = "t1_cycle_counts_against_the_device_captures";
    let Some((intc, _)) = probe_image(test, "probe_intc") else {
        return;
    };
    let Some((clocks, _)) = probe_image(test, "probe_clocks") else {
        return;
    };
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(test, "no data root");
        return;
    };
    let read = |name: &str| {
        std::fs::read(root.join("captures").join(name))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let (Some(dev_intc), Some(dev_clocks)) = (read(PROBE_INTC_CAPTURE), read(PROBE_CLOCKS_CAPTURE))
    else {
        common::skip(test, "no probe_intc or probe_clocks capture");
        return;
    };
    let off = |got: u64, dev: u64| (got as f64 - dev as f64) / dev as f64 * 100.0;

    let dev = cycles_by(&dev_intc, "LAT|", "index");
    let emu = cycles_by(
        &probe_console(&intc, TimingProfileId::Device, "LATSUM|"),
        "LAT|",
        "index",
    );
    assert_eq!(dev.len(), 16, "{test}: the capture has 16 LAT samples");
    assert_eq!(
        emu.keys().collect::<Vec<_>>(),
        dev.keys().collect::<Vec<_>>(),
        "{test}: LAT samples"
    );
    for (index, &d) in &dev {
        let e = emu[index];
        let o = off(e, d);
        if index == "0" {
            println!("  LAT index 0: device {d}, emulated {e}, {o:+.1} %");
            assert!(
                (15.0..=30.0).contains(&o),
                "{test}: LAT index 0 is {e} cycles against {d} ({o:+.1} %); the pinned residual \
                 (the steady path's CPI residual plus one measured line fill, 313 cycles, against \
                 this fetch's 245) was +15 % to +30 %"
            );
        } else {
            assert!(
                e.abs_diff(d) * 5 <= d,
                "{test}: LAT index {index} is {e} cycles against {d} ({o:+.1} %), outside 20 %"
            );
        }
    }
    let steady = emu["1"];
    println!(
        "  LAT index 1 to 15: device {}, emulated {steady}",
        dev["1"]
    );

    let dev = cycles_by(&dev_clocks, "CLK|", "phase");
    let emu = cycles_by(
        &probe_console(&clocks, TimingProfileId::Device, "DONE|"),
        "CLK|",
        "phase",
    );
    let mut wfi = Vec::new();
    for phase in ["task_delay", "timer_poll"] {
        let (d, e) = (dev[phase], emu[phase]);
        let o = off(e, d);
        println!("  {phase}: device {d} cycles, emulated {e}, {o:+.1} %");
        assert!(
            e.abs_diff(d) * 5 <= d,
            "{test}: {phase} is {e} cycles against {d} ({o:+.1} %), outside 20 %"
        );
        wfi.push(format!("{phase} {o:+.1} %"));
    }
    println!(
        "RAN {test}: LAT steady {steady} cycles against 172 and the WFI phases ({}) inside 20 %; \
         LAT index 0 at its pinned residual",
        wfi.join(", ")
    );
}
