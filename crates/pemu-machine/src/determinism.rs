//! The determinism harness: what two runs of one run identity must agree on ([`Observation`]),
//! across the host-side knobs of a [`Variant`]. [`Observation::line`] renders it as one line, so a
//! leg in another process or engine (the wasm32 build) agrees when its line is byte-equal.
//!
//! The guest-to-host rings are host capture, not machine state: a restored machine continues them
//! from their cursors with nothing before. So a restored run is compared over the output after a
//! [`Since`] mark, and the frame and PCM digests are shaped so a restore cannot show through them
//! (no picture before the first present; PCM runs split on rate, channel or time gaps, not on
//! record boundaries). The canonical MMIO and IRQ trace is compared apart
//! ([`Machine::trace_digest`]). Digests use the portable `pemu_loader::sha256`, so native and
//! wasm32 builds agree.

use pemu_core::hostio::{HostIo, SerialStream};
use pemu_core::time::VTime;

use crate::config::{MachineConfig, TimingProfileId, TraceCfg};
use crate::executor::Executor;
use crate::machine::Machine;
use crate::run::MAX_SLICE_INSNS;
use crate::stops::StopReason;

/// One point of the host-side matrix: choices a run may make without changing a result. The
/// timing profile is run identity, here only to run a matrix under each profile.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub executor: Executor,
    /// `EngineCfg::max_block_insns`: 1, 3 or 64 in the matrix.
    pub max_block_insns: u16,
    pub max_slice: u64,
    pub poll_ff: bool,
    pub rom_delay_ff: bool,
    pub trace: bool,
    pub profile: TimingProfileId,
}

impl Default for Variant {
    fn default() -> Variant {
        let cfg = MachineConfig::default();
        Variant {
            executor: Executor::Engine,
            max_block_insns: cfg.engine.max_block_insns,
            max_slice: MAX_SLICE_INSNS,
            poll_ff: cfg.poll_ff,
            rom_delay_ff: true,
            trace: cfg.trace.kinds.is_some(),
            profile: cfg.profile,
        }
    }
}

impl Variant {
    pub fn config(&self, mut base: MachineConfig) -> MachineConfig {
        base.engine.max_block_insns = self.max_block_insns;
        base.poll_ff = self.poll_ff;
        base.trace = if self.trace {
            TraceCfg::all()
        } else {
            TraceCfg::default()
        };
        base.profile = self.profile;
        base
    }

    pub fn apply(&self, m: &mut Machine) {
        m.set_executor(self.executor);
        m.set_max_slice(self.max_slice);
        m.set_rom_delay_ff(self.rom_delay_ff);
    }

    pub fn label(&self) -> String {
        let on = |b: bool| if b { "on" } else { "off" };
        let exec = match self.executor {
            Executor::Engine => format!("engine/{}", self.max_block_insns),
            Executor::Reference => "ref_step".to_string(),
        };
        format!(
            "{exec} slice={} poll_ff={} rom_delay_ff={} trace={} profile={:?}",
            self.max_slice,
            on(self.poll_ff),
            on(self.rom_delay_ff),
            on(self.trace),
            self.profile
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub state_hash: [u8; 32],
    /// Logical instructions retired since power-on, fast-forwarded ones included.
    pub insns: u64,
    pub vt: VTime,
    /// USJ console: SHA-256 over the stream's head cursor, the cursor digested from and the kept
    /// bytes from there.
    pub usj: [u8; 32],
    pub uart0: [u8; 32],
    /// SHA-256 over every kept line mark of both streams: offset and virtual time.
    pub lines: [u8; 32],
    pub frame: [u8; 32],
    pub pcm: [u8; 32],
}

/// Absolute cursors of the guest-to-host output an [`Observation`] digests from.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Since {
    pub usj: u64,
    pub uart0: u64,
    pub pcm: u64,
    pub frame: u64,
}

impl Since {
    pub const START: Since = Since {
        usj: 0,
        uart0: 0,
        pcm: 0,
        frame: 0,
    };

    /// The marks of `m`'s output now. A restored machine takes its own after the restore.
    pub fn now(m: &Machine) -> Since {
        let io = &m.io;
        Since {
            usj: io.usj_tx.head(),
            uart0: io.uart0_tx.head(),
            pcm: io.audio_out.head(),
            frame: io.frame.generation(),
        }
    }
}

impl Observation {
    pub fn capture(m: &Machine) -> Observation {
        Observation::capture_since(m, Since::START)
    }

    pub fn capture_since(m: &Machine, since: Since) -> Observation {
        let io = &m.io;
        Observation {
            state_hash: m.state_hash(),
            insns: m.hart.insns,
            vt: m.now(),
            usj: serial_digest(io, SerialStream::UsjTx, since.usj),
            uart0: serial_digest(io, SerialStream::Uart0Tx, since.uart0),
            lines: lines_digest(io),
            frame: frame_digest(io, since.frame),
            pcm: pcm_digest(io, since),
        }
    }

    pub fn line(&self) -> String {
        format!(
            "state={} insns={} vt={} usj={} uart0={} lines={} frame={} pcm={}",
            hex(&self.state_hash),
            self.insns,
            self.vt.0,
            hex(&self.usj),
            hex(&self.uart0),
            hex(&self.lines),
            hex(&self.frame),
            hex(&self.pcm)
        )
    }

    pub fn differences(&self, other: &Observation) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut check = |same: bool, name| {
            if !same {
                out.push(name);
            }
        };
        check(self.state_hash == other.state_hash, "state_hash");
        check(self.insns == other.insns, "insns");
        check(self.vt == other.vt, "vt");
        check(self.usj == other.usj, "usj console");
        check(self.uart0 == other.uart0, "uart0 console");
        check(self.lines == other.lines, "line timestamps");
        check(self.frame == other.frame, "frame");
        check(self.pcm == other.pcm, "pcm");
        out
    }
}

/// `stop=<reason> ` and [`Observation::line`]. The native tests and the wasm32 leg
/// (`crates/pemu-wasm/js/abi_parity.cjs`) both print it, so equal text is equal state.
pub fn report(reason: &StopReason, m: &Machine) -> String {
    report_since(reason, m, Since::START)
}

pub fn report_since(reason: &StopReason, m: &Machine, since: Since) -> String {
    format!(
        "stop={reason:?} {}",
        Observation::capture_since(m, since).line()
    )
}

/// SHA-256 over the `hang` snapshot section. `state_hash` leaves it out (it differs with poll
/// fast-forward on and off), but restore equivalence compares it: a restored tracker that forgot
/// a candidate would move the instant of a later `Stuck`.
pub fn hang_digest(m: &Machine) -> [u8; 32] {
    let bytes: Vec<u8> = m
        .sections()
        .into_iter()
        .filter(|(id, _)| id.as_str() == pemu_core::snap::SectionId::HANG)
        .flat_map(|(_, section)| section.bytes)
        .collect();
    pemu_loader::sha256(&bytes)
}

impl Machine {
    /// The digest of the canonical MMIO and IRQ trace so far; the empty stream's while tracing is
    /// off.
    pub fn trace_digest(&self) -> [u8; 32] {
        self.trace.digest()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn serial_digest(io: &HostIo, stream: SerialStream, from: u64) -> [u8; 32] {
    let ring = io.serial_ring(stream);
    let mut buf = Vec::with_capacity(16 + ring.len());
    buf.extend_from_slice(&ring.head().to_le_bytes());
    buf.extend_from_slice(&from.to_le_bytes());
    buf.extend(ring.slices(from).iter().copied());
    pemu_loader::sha256(&buf)
}

fn lines_digest(io: &HostIo) -> [u8; 32] {
    let mut buf = Vec::new();
    for stream in SerialStream::ALL {
        buf.extend_from_slice(&io.lines.head(stream).to_le_bytes());
        for mark in io.lines.slices(stream, io.lines.tail(stream)).iter() {
            buf.extend_from_slice(&mark.offset.to_le_bytes());
            buf.extend_from_slice(&mark.vt.0.to_le_bytes());
        }
    }
    pemu_loader::sha256(&buf)
}

fn frame_digest(io: &HostIo, since: u64) -> [u8; 32] {
    let f = &io.frame;
    let presented = f.generation().wrapping_sub(since);
    let mut buf = Vec::with_capacity(16 + f.pixels().len() * 2);
    buf.extend_from_slice(&presented.to_le_bytes());
    if presented > 0 {
        buf.extend_from_slice(&f.backlight().to_le_bytes());
        buf.extend_from_slice(&[
            u8::from(f.powered()),
            u8::from(f.sleeping()),
            u8::from(f.inverted()),
            u8::from(f.display_on()),
        ]);
        for px in f.pixels() {
            buf.extend_from_slice(&px.to_le_bytes());
        }
    }
    pemu_loader::sha256(&buf)
}

const PS_PER_S: u128 = 1_000_000_000_000;

fn pcm_digest(io: &HostIo, since: Since) -> [u8; 32] {
    let pcm = &io.audio_out;
    let start = since.pcm.max(pcm.tail());
    let samples: Vec<i16> = pcm.slices(start).iter().copied().collect();
    let records: Vec<_> = pcm
        .record_slices(pcm.record_tail())
        .iter()
        .copied()
        .collect();
    let mut buf = Vec::new();
    // The run in progress: rate, channels and the instant its next frame is expected at.
    let mut run: Option<(u32, u16, u128)> = None;
    let mut at = start;
    while at < pcm.head() {
        let i = records.iter().rposition(|r| r.first <= at);
        let end = i
            .and_then(|i| records.get(i + 1))
            .map_or(pcm.head(), |r| r.first.min(pcm.head()));
        let Some(r) = i.map(|i| records[i]) else {
            // No header kept for these samples: hash them with no instant.
            buf.extend_from_slice(b"?");
            for s in &samples[(at - start) as usize..(end - start) as usize] {
                buf.extend_from_slice(&s.to_le_bytes());
            }
            run = None;
            at = end;
            continue;
        };
        let ch = u64::from(r.channels.max(1));
        let fs = u128::from(r.fs.max(1));
        let frame0 = u128::from((at - r.first) / ch);
        let vt0 = u128::from(r.vt_start.0) + frame0 * PS_PER_S / fs;
        let half_frame = PS_PER_S / fs / 2;
        let contiguous = run.is_some_and(|(rfs, rch, next)| {
            rfs == r.fs && rch == r.channels && vt0.abs_diff(next) <= half_frame
        });
        if !contiguous {
            buf.extend_from_slice(b"run");
            buf.extend_from_slice(&(vt0 as u64).to_le_bytes());
            buf.extend_from_slice(&r.fs.to_le_bytes());
            buf.extend_from_slice(&r.channels.to_le_bytes());
        }
        for s in &samples[(at - start) as usize..(end - start) as usize] {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        let frames = u128::from((end - r.first) / ch);
        run = Some((
            r.fs,
            r.channels,
            u128::from(r.vt_start.0) + frames * PS_PER_S / fs,
        ));
        at = end;
    }
    pemu_loader::sha256(&buf)
}

#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use super::*;
    use crate::config::Assets;
    use crate::run::RunLimits;
    use pemu_core::snap::{SnapOpts, Snapshot};
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;

    const INSNS: u64 = 300_000;

    fn machine(v: &Variant) -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let mut m = Machine::new(v.config(MachineConfig::default()), assets).expect("composes");
        v.apply(&mut m);
        m
    }

    fn run(v: &Variant) -> (String, Machine) {
        let mut m = machine(v);
        let out = m.run(RunLimits::insns(INSNS));
        (report(&out.reason, &m), m)
    }

    #[test]
    fn block_slice_executor_fast_forward_and_trace_choices_give_one_report() {
        let base = Variant::default();
        let (want, straight) = run(&base);
        assert!(want.starts_with("stop=MaxInsns "), "{want}");
        assert_ne!(
            Observation::capture(&straight).usj,
            Observation::capture(&machine(&base)).usj,
            "the run printed something, so the console digest is not vacuous"
        );
        let variants = [
            Variant {
                max_block_insns: 1,
                max_slice: 1,
                ..base
            },
            Variant {
                max_block_insns: 3,
                max_slice: 37,
                ..base
            },
            Variant {
                executor: Executor::Reference,
                ..base
            },
            Variant {
                poll_ff: !base.poll_ff,
                rom_delay_ff: !base.rom_delay_ff,
                trace: !base.trace,
                ..base
            },
        ];
        for v in variants {
            assert_eq!(run(&v).0, want, "{}", v.label());
        }
        let traced = |poll_ff| {
            run(&Variant {
                poll_ff,
                trace: true,
                ..base
            })
            .1
            .trace_digest()
        };
        assert_eq!(
            traced(true),
            traced(false),
            "poll fast-forward changed the trace"
        );
    }

    #[test]
    fn a_restored_machine_is_compared_over_the_output_after_the_snapshot() {
        let base = Variant::default();
        let mut first = machine(&base);
        first.run(RunLimits::insns(INSNS / 2));
        let since = Since::now(&first);
        assert!(since.usj > 0, "the banner is out before the snapshot");
        let bytes = first.snapshot(SnapOpts::default()).to_bytes().unwrap();
        let mut restored = machine(&base);
        restored
            .restore(&Snapshot::from_bytes(&bytes).unwrap())
            .unwrap();
        let own = Since::now(&restored);
        assert_eq!(
            (own.usj, own.uart0, own.pcm),
            (since.usj, since.uart0, since.pcm),
            "the output cursors continue"
        );
        let a = first.run(RunLimits::insns(INSNS / 2));
        let b = restored.run(RunLimits::insns(INSNS / 2));
        assert_eq!(
            report_since(&a.reason, &first, since),
            report_since(&b.reason, &restored, own)
        );
        // Console output before the snapshot is host capture, not state, so the comparison starts
        // at `since`.
        assert_ne!(report(&a.reason, &first), report(&b.reason, &restored));
        assert_eq!(first.state_hash(), restored.state_hash());
    }

    #[test]
    fn frame_and_pcm_digests_ignore_the_generation_base_and_record_boundaries() {
        let mut a = HostIo::new(1024);
        let mut b = HostIo::new(1024);
        for _ in 0..5 {
            a.frame.present();
        }
        let (ma, mb) = (a.frame.generation(), b.frame.generation());
        assert_eq!(
            frame_digest(&a, ma),
            frame_digest(&b, mb),
            "nothing presented yet"
        );
        a.frame.pixels_mut()[7] = 0x1234;
        b.frame.pixels_mut()[7] = 0x1234;
        assert_eq!(
            frame_digest(&a, ma),
            frame_digest(&b, mb),
            "a picture not presented since the mark is not compared"
        );
        a.frame.present();
        b.frame.present();
        assert_eq!(frame_digest(&a, ma), frame_digest(&b, mb));
        b.frame.pixels_mut()[7] = 0x4321;
        assert_ne!(frame_digest(&a, ma), frame_digest(&b, mb), "pixels count");

        // One 16 kHz record against the same samples split at 100, the second record stamped
        // where the 101st sample falls.
        let samples: Vec<i16> = (0..300).map(|i| i as i16).collect();
        a.audio_out.write(VTime(1_000_000), 16_000, 1, &samples);
        b.audio_out
            .write(VTime(1_000_000), 16_000, 1, &samples[..100]);
        let split = 1_000_000 + 100 * 1_000_000_000_000 / 16_000;
        b.audio_out.write(VTime(split), 16_000, 1, &samples[100..]);
        let mark = Since::START;
        assert_eq!(pcm_digest(&a, mark), pcm_digest(&b, mark));
        let later = Since { pcm: 150, ..mark };
        assert_eq!(pcm_digest(&a, later), pcm_digest(&b, later), "from a mark");
        let mut c = HostIo::new(1024);
        c.audio_out
            .write(VTime(1_000_000), 16_000, 1, &samples[..100]);
        c.audio_out
            .write(VTime(split + 1_000_000_000), 16_000, 1, &samples[100..]);
        assert_ne!(pcm_digest(&a, mark), pcm_digest(&c, mark));
        let mut d = HostIo::new(1024);
        d.audio_out.write(VTime(1_000_000), 8_000, 1, &samples);
        assert_ne!(
            pcm_digest(&a, mark),
            pcm_digest(&d, mark),
            "the rate counts"
        );
    }

    #[test]
    fn a_fork_keeps_the_slice_bound_and_a_zero_bound_is_one() {
        let mut m = machine(&Variant::default());
        m.set_max_slice(0);
        assert_eq!(m.max_slice(), 1);
        m.set_max_slice(37);
        let fork = m.fork(pemu_core::snap::LivePolicy::Refuse).unwrap();
        assert_eq!(fork.max_slice(), 37);
    }
}
