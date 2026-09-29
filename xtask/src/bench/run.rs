//! The F-suite runner.

use std::path::{Path, PathBuf};

use pemu_machine::stops::StopReason;
use serde_json::{Value, json};

use super::cli::Options;
use super::history::REGRESSION;
use super::metrics::{Metrics, Window};
use super::model::WINDOW_MS;
use super::suites::{
    BleLeg, CLICK_MS, Clicks, EndState, Image, Scenario, end_state, run_phase, run_slices,
};

/// A suite's subject, or why there is none.
pub(super) enum Subject {
    /// The corpus image, read and verified once and minted into a machine per run.
    Ready(Box<Loaded>),
    /// The corpus id is not on this host: the suite is NOT_RUN with this reason, never a failure.
    Absent(String),
}

/// One corpus image, parsed once so every repeat and every calibration pass of a suite mints its
/// machine without re-reading 8 MB from disk.
pub(super) struct Loaded {
    flash: pemu_loader::bundle::FlashImage,
    elf: std::sync::Arc<pemu_loader::elf::ElfInfo>,
    /// Critical-section fusion sizing of this image (lever 3).
    pub(super) sites: pemu_rv32::fuse::SiteCount,
}

impl Loaded {
    /// A machine over this image: the merged flash and its application ELF, the bundled ROM and
    /// the synthesized eFuse, under the default configuration with `o`'s lever knobs
    /// applied. The knobs are part of the record's config key, so a swept run never baselines a
    /// default one.
    pub(super) fn machine(&self, o: &Options) -> Result<pemu_machine::Machine, String> {
        use pemu_loader::efuse_image::EfuseImage;
        use pemu_machine::config::{Assets, MachineConfig};

        let assets = Assets::with_bundled_rom(
            self.flash.clone(),
            Some(self.elf.clone()),
            None,
            EfuseImage::synth(0),
        )
        .map_err(|e| format!("the bundled ROM is not pinned: {e:?}"))?;
        let mut cfg = MachineConfig::default();
        if let Some(max) = o.max_block_insns {
            cfg.engine.max_block_insns = max;
        }
        cfg.poll_ff = o.poll_ff;
        pemu_machine::Machine::new(cfg, assets).map_err(|e| e.to_string())
    }
}

/// Reads and verifies one corpus image.
///
/// The corpus verdict is `pemu_testkit::corpus`'s: an id that is absent is a reason to record the
/// suite NOT_RUN, and a file that is present and is not the pinned one is an error, so a corrupted
/// corpus can never quietly become a measurement.
pub(super) fn corpus_image(root: &Path, image: Image) -> Result<Subject, String> {
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::elf::ElfInfo;

    let located = pemu_testkit::corpus::locate_id_at(root, image.id);
    if let Some(failure) = located.failure_reason() {
        return Err(failure);
    }
    if let Some(reason) = located.skip_reason() {
        return Ok(Subject::Absent(reason));
    }
    let path = |file: &str| -> Result<PathBuf, String> {
        located
            .file(file)
            .map(|f| f.path.clone())
            .ok_or_else(|| format!("corpus id `{}` has no file `{file}`", image.id))
    };
    let bytes = std::fs::read(path(image.bin)?)
        .map_err(|e| format!("corpus id `{}`: the image is unreadable: {e}", image.id))?;
    let flash = FlashImage::from_merged(&bytes)
        .map_err(|e| format!("corpus id `{}`: the image does not parse: {e:?}", image.id))?;
    let elf_bytes = std::fs::read(path(image.elf)?)
        .map_err(|e| format!("corpus id `{}`: the ELF is unreadable: {e}", image.id))?;
    let elf = ElfInfo::parse(&elf_bytes)
        .map_err(|e| format!("corpus id `{}`: the ELF does not parse: {e:?}", image.id))?;
    let sites = elf_fusion_sites(&elf_bytes, &elf);
    Ok(Subject::Ready(Box::new(Loaded {
        flash,
        elf: std::sync::Arc::new(elf),
        sites,
    })))
}

/// One measured run of one suite.
pub(super) struct SuiteRun {
    /// Windows of the boot phase: the measured ones for F1 and F2, and not reported otherwise.
    boot: Vec<Window>,
    /// Windows of the body phase; empty when the boot is the workload.
    pub(super) body: Vec<Window>,
    /// Slices of the calibration pass, which resolve S alone
    /// ([`CALIBRATION_PS`](super::suites::CALIBRATION_PS)).
    calibration: Vec<Window>,
    /// Virtual ps the boot took to reach its console line.
    pub(super) boot_ps: u64,
    pub(super) end: EndState,
    /// The playback of a capturing suite (F6), as `(fs, channels, samples)`.
    pub(super) pcm: Option<(u32, u16, Vec<i16>)>,
    /// What the BLE leg of a suite that has one did (F7).
    pub(super) ble: Option<BleSummary>,
    /// The USJ console of the run, for the diagnosis `--console` prints.
    pub(super) console: String,
}

impl SuiteRun {
    /// The windows the suite's numbers are read from: the body, or the boot for F1 and F2.
    fn measured(&self) -> &[Window] {
        if self.body.is_empty() {
            &self.boot
        } else {
            &self.body
        }
    }

    pub(super) fn metrics(&self) -> Metrics {
        Metrics::from_phases(self.measured(), &self.calibration)
    }
}

/// The USJ console of `m` so far.
fn console(m: &mut pemu_machine::Machine) -> String {
    let ring = m.io().serial_ring(pemu_core::hostio::SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Fewest runs of a workload before its fastest is taken as the measurement.
const MIN_REPEATS: usize = 3;

/// Consecutive runs that must agree before repeating stops ([`settled`]).
const PATIENCE: usize = 3;

/// How far above the fastest run those runs may be and still count as agreeing: the same 10 %
/// the trend gate uses, so the repeat loop stops exactly when another run could no
/// longer change the verdict. UNVERIFIED: a design choice.
const SETTLE_BAND: f64 = REGRESSION;

/// Whether the repeat loop has learned what it is going to: `walls` are the runs so far, newest
/// last, at least [`MIN_REPEATS`] of them, and the last [`PATIENCE`] all sit within
/// [`SETTLE_BAND`] of the fastest.
///
/// The criterion is agreement, not the absence of an improvement: a contended host scatters
/// rather than plateaus, and the fastest of many runs converges on the uncontended cost. Measured
/// on F3 at a load average of 39: the fastest of 5 runs was 5.39 s, of 9 was 2.43 s and of 15 was
/// 1.54 s, against 1.16 s on a quiet host. A quiet host stops at [`MIN_REPEATS`] plus a couple.
pub(super) fn settled(walls: &[f64]) -> bool {
    if walls.len() < MIN_REPEATS.max(PATIENCE) {
        return false;
    }
    let best = walls.iter().copied().fold(f64::INFINITY, f64::min);
    walls[walls.len() - PATIENCE..]
        .iter()
        .all(|w| *w <= best * (1.0 + SETTLE_BAND))
}

/// Whether a repeat loop can stop: the runs so far agree ([`settled`]) and at least one of them
/// was measured on the cores the targets are for ([`Metrics::on_measured_cores`]), so
/// [`reported_run`] has a run to report that is not a mix of two core speeds. Until then it
/// repeats up to its ceiling, exactly as it does while the runs disagree.
pub(super) fn repeats_done(runs: &[&Metrics]) -> bool {
    settled(&runs.iter().map(|m| m.wall_s).collect::<Vec<_>>())
        && runs.iter().any(|m| m.on_measured_cores())
}

/// Index of the run a repeat loop reports: the fastest by wall among the runs measured on the
/// cores the targets are for, else, when no run was, the fastest of all (which then carries
/// `mixed` or `efficiency`, and [`finish`](super::cli::finish) enforces nothing on it). `runs`
/// must not be empty.
///
/// The fastest, not the median: every repeat did identical work ([`EndState`]) and contention
/// only adds host time, so a median would make the 10 % gate a load detector. A run spread over
/// both clusters is contaminated whatever its wall: its S is a mix of two core speeds.
pub(super) fn reported_run(runs: &[&Metrics]) -> usize {
    let fastest = |measured: bool| {
        runs.iter()
            .enumerate()
            .filter(|(_, m)| !measured || m.on_measured_cores())
            .min_by(|a, b| a.1.wall_s.total_cmp(&b.1.wall_s))
            .map(|(i, _)| i)
    };
    fastest(true)
        .or_else(|| fastest(false))
        .expect("a repeat loop reports at least one run")
}

/// Fewest windows a boot phase is given beyond its own budget, so a boot that ends exactly on a
/// window boundary still has a window to end in.
pub(super) const BOOT_SLACK_WINDOWS: u64 = 1;

/// What a BLE leg did, read out of the bound module at the end of a run
/// (`MachineApi::radio_module_state`).
///
/// Every field is a guest-side count, so it is exact whatever else the host is doing, and a
/// record that carries it says what the measured windows were measuring rather than leaving
/// "BLE demand" to be taken on trust.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct BleSummary {
    /// Advertising events the controller put on the virtual air.
    pub(super) adv_events: u64,
    /// True while the central still holds the connection at the end of the body.
    pub(super) connected: bool,
    /// Connection events on the air.
    pub(super) connection_events: u64,
    /// Notifications and indications the central received since it was created.
    pub(super) notified: u64,
    /// Poll scripts the body journaled.
    pub(super) polls_journaled: u64,
    /// Steps the central was given since it was created.
    pub(super) steps: u32,
    /// Of the results it still holds (the last `central::MAX_RESULTS`), the ones that did not
    /// end `Ok`.
    pub(super) recent_failed_steps: usize,
    /// Steps refused because the queue was full.
    pub(super) refused_steps: u64,
    /// H4 packets the guest host sent, and packets delivered back to it.
    pub(super) tx_packets: u64,
    pub(super) rx_packets: u64,
}

impl BleSummary {
    pub(super) fn to_json(&self) -> Value {
        json!({
            "adv_events": self.adv_events,
            "connected": self.connected,
            "connection_events": self.connection_events,
            "notified": self.notified,
            "polls_journaled": self.polls_journaled,
            "steps": self.steps,
            "recent_failed_steps": self.recent_failed_steps,
            "refused_steps": self.refused_steps,
            "tx_packets": self.tx_packets,
            "rx_packets": self.rx_packets,
        })
    }
}

/// The UUID `text` names; the strings are constants of `suites.rs`, so a bad one is a bug here.
fn ble_uuid(text: &str) -> pemu_radio::ble::central::Uuid {
    pemu_radio::ble::central::Uuid::parse(text).expect("a UUID constant of this module")
}

/// The connect script: find the advertising, connect to the vendor service, exchange the
/// MTU 247, discover the service and its characteristics, subscribe to the events characteristic.
pub(super) fn ble_connect_script(leg: &BleLeg) -> Vec<pemu_radio::ble::central::Step> {
    use pemu_radio::ble::central::{Step, Target};
    vec![
        Step::Scan { ms: leg.scan_ms },
        Step::Connect {
            target: Target::Service(ble_uuid(leg.service)),
            // 30 ms, no latency, 4 s: the class C parameters of the `m8.rs` central.
            interval: 24,
            latency: 0,
            timeout: 400,
            within_ms: leg.connect_within_ms,
        },
        Step::ExchangeMtu { mtu: 247 },
        Step::DiscoverServices,
        Step::DiscoverCharacteristics {
            service: ble_uuid(leg.service),
        },
        Step::Subscribe {
            characteristic: ble_uuid(leg.events),
            indicate: false,
        },
    ]
}

/// One poll of the notify stream: write the command line, then wait for the answer it draws.
pub(super) fn ble_poll_script(leg: &BleLeg) -> Vec<pemu_radio::ble::central::Step> {
    use pemu_radio::ble::central::Step;
    vec![
        Step::Write {
            characteristic: ble_uuid(leg.commands),
            value: format!("{}\n", leg.poll_line).into_bytes(),
            with_response: true,
        },
        Step::WaitNotification {
            characteristic: ble_uuid(leg.events),
            contains: leg.poll_answer.as_bytes().to_vec(),
            within_ms: leg.poll_within_ms,
        },
    ]
}

/// Journals `steps` for the scripted central of `m` now.
fn journal_ble(
    m: &mut pemu_machine::Machine,
    steps: &[pemu_radio::ble::central::Step],
) -> Result<(), String> {
    use pemu_core::input::{EnvChange, InputEvent};
    use pemu_machine::machine::At;

    m.input(
        At::Now,
        InputEvent::Env(EnvChange::BleCentral {
            script: pemu_radio::ble::central::encode_script(steps),
        }),
    )
    .map_err(|e| format!("the BLE central script was refused: {e:?}"))?;
    Ok(())
}

/// The BLE leg's summary from the bound module's state, or an error naming what is not there.
fn ble_summary(m: &pemu_machine::Machine, polls_journaled: u64) -> Result<BleSummary, String> {
    use pemu_radio::ble::central::StepStatus;
    use pemu_radio::ble::vhci::BleState;

    let bytes = m
        .radio_module_state("ble")
        .ok_or("the `ble` module is not bound, so this suite measured no BLE")?;
    let st = BleState::decode(bytes).map_err(|e| format!("the `ble` module state: {e:?}"))?;
    Ok(BleSummary {
        adv_events: st.air.adv_events,
        connected: st.central.link.is_some(),
        connection_events: st.air.link.as_ref().map_or(0, |l| l.events),
        notified: st.central.notified,
        polls_journaled,
        steps: st.central.next_index,
        recent_failed_steps: st
            .central
            .results
            .iter()
            .filter(|r| r.status != StepStatus::Ok)
            .count(),
        refused_steps: st.central.refused_steps,
        tx_packets: st.tx_packets,
        rx_packets: st.rx_packets,
    })
}

/// Runs one scenario on `m`.
///
/// The boot phase runs to the scenario's console line; a boot that does not print it by its
/// budget is an error, because every later number would be measured on a machine that is not in
/// the state the workload names. The body then journals its clicks at absolute instants and runs
/// its own virtual length with no matcher armed.
pub(super) fn run_scenario(
    m: &mut pemu_machine::Machine,
    calibration: Vec<Window>,
    s: &Scenario,
    window_ms: u64,
) -> Result<SuiteRun, String> {
    let boot_windows = s.boot_budget_ms.div_ceil(WINDOW_MS) + BOOT_SLACK_WINDOWS;
    let (boot, reason) = run_phase(m, boot_windows, Some(s.boot_to))?;
    if !matches!(reason, StopReason::Matcher(_)) {
        return Err(format!(
            "`{}` did not print {:?} within {} ms virtual (stopped for {reason:?} at {:?})",
            s.image.id,
            s.boot_to,
            s.boot_budget_ms,
            m.now()
        ));
    }
    let boot_ps = m.now().0;
    if s.boot_is_the_workload {
        return Ok(SuiteRun {
            console: console(m),
            end: end_state(m, &boot),
            boot,
            body: Vec::new(),
            calibration,
            boot_ps,
            pcm: None,
            ble: None,
        });
    }

    /// Journals every click of `clicks` at its instant from `from`.
    fn journal(
        m: &mut pemu_machine::Machine,
        from: pemu_core::time::VTime,
        clicks: Clicks,
    ) -> Result<(), String> {
        use pemu_core::input::InputEvent;
        use pemu_core::time::VTime;
        use pemu_machine::machine::At;

        for (ms, button) in clicks.schedule() {
            let press = VTime(from.0 + VTime::from_ms(ms).0);
            let release = VTime(press.0 + VTime::from_ms(CLICK_MS).0);
            for (at, down) in [(press, true), (release, false)] {
                m.input(At::Vt(at), InputEvent::Button { id: button, down })
                    .map_err(|e| format!("the click at {ms} ms was refused: {e:?}"))?;
            }
        }
        Ok(())
    }

    // The setup: the clicks that put the machine in the state the workload names, and the time it
    // takes to get there. Its windows join the boot's, which nothing reports for a body workload.
    let mut boot = boot;
    let setup_start = m.now();
    journal(m, setup_start, s.setup)?;
    // The BLE connect script goes in with the setup clicks, so the scan, the connection, the
    // MTU exchange, the discovery and the CCCD write are behind the body rather than in it.
    if let Some(leg) = s.ble {
        journal_ble(m, &ble_connect_script(leg))?;
    }
    if s.setup_ms > 0 {
        let (mut setup, reason) = run_phase(m, s.setup_ms.div_ceil(WINDOW_MS), None)?;
        boot.append(&mut setup);
        if !matches!(reason, StopReason::Until) {
            return Err(format!(
                "the setup of `{}` stopped for {reason:?}",
                s.image.id
            ));
        }
    }

    let body_start = m.now();
    journal(m, body_start, s.clicks)?;
    let mut capture = s
        .capture_audio
        .then(|| pemu_api::commands::audio_capture::Capture::starting_at(m.io().audio_out.head()));
    let mut body = Vec::with_capacity((s.body_ms / WINDOW_MS) as usize);
    let mut polls = 0u64;
    // The capture drains between windows, because the ring evicts its oldest samples when it is
    // full and a 10 s playback does not fit in it whole. A BLE leg journals its next
    // poll on the same boundaries: the period is counted in virtual ms from the start of the
    // body, so a poll that took longer than its period is caught up with rather than skipped,
    // and one script of two steps at a time keeps the central's queue far below `MAX_STEPS`.
    for window in 0..s.body_ms.div_ceil(window_ms.max(1)) {
        if let Some(leg) = s.ble {
            let due = window * window_ms.max(1) / leg.poll_period_ms.max(1) + 1;
            while polls < due {
                journal_ble(m, &ble_poll_script(leg))?;
                polls += 1;
            }
        }
        let (mut one, reason) = run_slices(m, window_ms.max(1) * 1_000_000_000, 1, None)?;
        body.append(&mut one);
        if let Some(capture) = capture.as_mut() {
            capture.drain(&m.io().audio_out);
        }
        if !matches!(reason, StopReason::Until) {
            break;
        }
    }
    let pcm = match capture {
        None => None,
        Some(c) => {
            let (fs, channels) = c.format().ok_or_else(|| {
                format!(
                    "the capture of `{}` holds {} playback runs and no single format, so it is \
                     not one WAV; the workload must play inside one format",
                    s.image.id,
                    c.runs.len()
                )
            })?;
            if c.samples.is_empty() {
                return Err(format!(
                    "the capture of `{}` is empty: the workload played no audio",
                    s.image.id
                ));
            }
            Some((fs, channels, c.samples))
        }
    };
    let ble = match s.ble {
        None => None,
        Some(_) => Some(ble_summary(m, polls)?),
    };
    Ok(SuiteRun {
        console: console(m),
        end: end_state(m, &body),
        boot,
        body,
        calibration,
        boot_ps,
        pcm,
        ble,
    })
}

/// Critical-section candidate sites in the IROM sections of the bundled ROM (lever 3, sized before
/// it is applied).
pub(super) fn rom_fusion_sites(m: &pemu_machine::Machine) -> pemu_rv32::fuse::SiteCount {
    use pemu_loader::rom::PlacementKind;

    let rom = &m.assets().rom;
    let mut total = pemu_rv32::fuse::SiteCount::default();
    for p in rom
        .placements()
        .iter()
        .filter(|p| p.kind == PlacementKind::Irom)
    {
        let Some(bytes) = rom.bytes().get(p.offset..p.offset + p.size as usize) else {
            continue;
        };
        let count = pemu_rv32::fuse::scan_image(bytes, p.vma);
        total.units += count.units;
        total.threshold_units += count.threshold_units;
        total.candidates += count.candidates;
    }
    total
}

/// Critical-section candidate sites in an application ELF (lever 3, sized before it is applied).
///
/// The application's own executable sections, not the ROM's IROM: the ROM's `ets_intr_lock` and
/// `ets_intr_unlock` clear and restore MIE in separate functions, so the unit does not exist
/// there, while the IDF port's critical sections are the fused sequence.
/// A static count over an image is lever sizing, never a correctness input; how often
/// the executed code reaches those sites is what a lever is worth, and is not this.
fn elf_fusion_sites(bytes: &[u8], elf: &pemu_loader::elf::ElfInfo) -> pemu_rv32::fuse::SiteCount {
    use pemu_loader::elf::SHF_EXECINSTR;

    let mut total = pemu_rv32::fuse::SiteCount::default();
    for s in &elf.sections {
        if !s.is_alloc() || !s.has_bits() || s.flags & SHF_EXECINSTR == 0 {
            continue;
        }
        let from = s.offset as usize;
        let Some(code) = bytes.get(from..from.saturating_add(s.size as usize)) else {
            continue;
        };
        let count = pemu_rv32::fuse::scan_image(code, s.addr);
        total.units += count.units;
        total.threshold_units += count.threshold_units;
        total.candidates += count.candidates;
    }
    total
}
