//! Milestone M5 tests: the I2C0, APB_SARADC and I2S0 blocks, and `official` through the agent
//! surface. Names use the prefix `t<tier>_m5_` so `xtask ci` can count them.
//!
//! The `t0_m5_*` tests check the block behavior with no corpus and no machine: the I2C0 bus scan,
//! the chip transcripts through the executor, the ladder codes, and the I2S0 pacing with the PCM
//! path behind it. The `t1_m5_*` tests run on the corpus: on the machine through the MMIO trace,
//! through the registry on the host's real hooks, and against device captures.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::{passportsim, workspace, xtask_bench};

use pemu_board::cw2017::Cw2017;
use pemu_board::es8311::Es8311;
use pemu_board::i2c_transcript::{Transcript, TxOp};
use pemu_board::ladder::{ButtonLadder, LadderConfig};
use pemu_board::traits::{I2cDevice, PcmFormat};
use pemu_core::fidelity::{Fidelity, FidelityLedger, LedgerSubject};
use pemu_core::hostio::HostIo;
use pemu_core::input::ButtonId;
use pemu_core::regstore::Size;
use pemu_core::sched::Scheduler;
use pemu_core::time::VTime;
use pemu_soc_c3::periph::i2c0::{self, Cmd, I2cBus, Op, RunEnd};
use pemu_soc_c3::periph::i2s0::{self, Dir, I2sDma};
use pemu_soc_c3::periph::saradc;
use pemu_soc_c3::periph::{Peripheral, Stability, Wiring};
use pemu_soc_c3::wiring;
use pemu_testkit::mock_board::{BoardCall, MockBoard};
use pemu_testkit::reg_harness::RegHarness;

/// The ES8311 codec and the CW2017 gauge.
const CODEC: u8 = 0x18;
const GAUGE: u8 = 0x63;

/// `bsp_i2c_scan` probes 0x08 through 0x77.
const SCAN_FIRST: u8 = 0x08;
const SCAN_LAST: u8 = 0x77;

/// The I2C transcripts of the ES8311 and CW2017 paths, plus the boot probe list, embedded so these
/// tests read no file.
const ES8311_A_OPEN: &str = include_str!("../transcripts/i2c/es8311-a-open.txt");
const ES8311_B_SET_FORMAT: &str = include_str!("../transcripts/i2c/es8311-b-set-format.txt");
const ES8311_C_SET_VOLUME: &str = include_str!("../transcripts/i2c/es8311-c-set-volume.txt");
const ES8311_D_CLOSE: &str = include_str!("../transcripts/i2c/es8311-d-close.txt");
const CW2017_INIT_FAST: &str = include_str!("../transcripts/i2c/cw2017-init-fast.txt");
const CW2017_INIT_FRESH: &str = include_str!("../transcripts/i2c/cw2017-init-fresh.txt");
const I2C_BOOT_PROBE: &str = include_str!("../transcripts/i2c/i2c-boot-probe.txt");

/// Block offsets of the I2C0 registers these tests drive (IDF
/// `soc/esp32c3/register/soc/i2c_reg.h`).
const I2C_CTR: u32 = 0x004;
const I2C_FIFO_CONF: u32 = 0x018;
const I2C_DATA: u32 = 0x01C;
const I2C_INT_RAW: u32 = 0x020;
const I2C_INT_CLR: u32 = 0x024;
const I2C_COMD0: u32 = 0x058;
/// `CTR.conf_upgate` and `CTR.trans_start`, the two bits that start a list.
const I2C_START_WRITE: u32 = (1 << 11) | (1 << 5);
/// `FIFO_CONF.rx_fifo_rst | tx_fifo_rst`, which every transaction pulses.
const I2C_FIFO_RST: u32 = (1 << 12) | (1 << 13);
/// Every `INT_RAW` bit the driver clears before a transaction.
const I2C_INT_ALL: u32 = 0x3FFFF;

/// The two chips on one bus, as an [`I2cBus`] the I2C0 executor can drive. It also records the
/// `(register, value)` write trace the sequence tests compare, which `PassportBoard` does not keep.
struct ChipBus {
    codec: Es8311,
    gauge: Cw2017,
    addressed: Option<u8>,
    t: VTime,
    /// Every `(register, value)` pair the wire carried, compared against
    /// `specs/es8311-sequences.toml`.
    writes: Vec<(u8, u8)>,
    /// The first byte a write put on the wire after the address: the register pointer.
    pointer: Option<u8>,
}

impl ChipBus {
    fn new() -> ChipBus {
        ChipBus {
            codec: Es8311::new(),
            gauge: Cw2017::provisioned(),
            addressed: None,
            t: VTime(0),
            writes: Vec::new(),
            pointer: None,
        }
    }
}

impl I2cBus for ChipBus {
    fn start(&mut self, addr: u8, read: bool) -> bool {
        let t = self.t;
        self.pointer = None;
        let ack = match addr {
            CODEC => self.codec.start(t, read),
            GAUGE => self.gauge.start(t, read),
            _ => false,
        };
        self.addressed = ack.then_some(addr);
        ack
    }

    fn write(&mut self, byte: u8) -> bool {
        let (t, addr) = (self.t, self.addressed);
        let Some(addr) = addr else {
            return false;
        };
        match self.pointer {
            None => self.pointer = Some(byte),
            Some(reg) => {
                self.writes.push((reg, byte));
                self.pointer = Some(reg.wrapping_add(1));
            }
        }
        match addr {
            CODEC => self.codec.write(t, byte),
            GAUGE => self.gauge.write(t, byte),
            _ => false,
        }
    }

    fn read(&mut self) -> u8 {
        let (t, addr) = (self.t, self.addressed);
        match addr {
            Some(CODEC) => self.codec.read(t),
            Some(GAUGE) => self.gauge.read(t),
            // An undriven bus floats high: an absent address NACKs and nothing drives SDA.
            _ => 0xFF,
        }
    }

    fn stop(&mut self) {
        let (t, addr) = (self.t, self.addressed);
        match addr {
            Some(CODEC) => self.codec.stop(t),
            Some(GAUGE) => self.gauge.stop(t),
            _ => {}
        }
        self.addressed = None;
        self.pointer = None;
    }
}

/// One transaction driven as `s_i2c_transaction_start` and `i2c_hal_master_trans_start` do: reset
/// both FIFOs, clear the interrupts, write the command list, push the bytes, then `conf_upgate`
/// and `trans_start`.
fn run_list(
    i2c: &mut i2c0::Model,
    ledger: &mut FidelityLedger,
    bus: &mut dyn I2cBus,
    t: VTime,
    cmds: &[Cmd],
    tx: &[u8],
) -> RunEnd {
    i2c.store(I2C_FIFO_CONF, Size::B4, I2C_FIFO_RST, t, ledger);
    i2c.store(I2C_FIFO_CONF, Size::B4, 0, t, ledger);
    i2c.store(I2C_INT_CLR, Size::B4, I2C_INT_ALL, t, ledger);
    for (i, cmd) in cmds.iter().enumerate() {
        i2c.store(I2C_COMD0 + 4 * i as u32, Size::B4, cmd.encode(), t, ledger);
    }
    for byte in tx {
        i2c.store(I2C_DATA, Size::B4, u32::from(*byte), t, ledger);
    }
    let wiring = i2c.store(I2C_CTR, Size::B4, I2C_START_WRITE, t, ledger);
    assert!(
        matches!(wiring, pemu_soc_c3::periph::Wiring::I2cRun),
        "a trans_start write asks for the command list to run",
    );
    i2c.run(bus)
}

fn cmd(op: Op, byte_num: u8, ack_en: bool) -> Cmd {
    Cmd {
        byte_num,
        ack_en,
        ack_val: false,
        op,
    }
}

/// `i2c_master_probe`: `RESTART; WRITE 1 ack_en; STOP` with `addr<<1`.
fn probe(
    i2c: &mut i2c0::Model,
    ledger: &mut FidelityLedger,
    bus: &mut dyn I2cBus,
    addr: u8,
) -> bool {
    let list = [
        cmd(Op::Restart, 0, false),
        cmd(Op::Write, 1, true),
        cmd(Op::Stop, 0, false),
    ];
    run_list(i2c, ledger, bus, VTime(0), &list, &[addr << 1]) == RunEnd::Complete
}

/// Replays one transcript through the I2C0 executor with the IDF `i2c_master` command list of each
/// transaction. A transaction to an address no chip claims must NACK, which is a `nack` line.
fn replay(
    i2c: &mut i2c0::Model,
    ledger: &mut FidelityLedger,
    bus: &mut ChipBus,
    transcript: &Transcript,
) {
    for (index, op) in transcript.ops.iter().enumerate() {
        match op {
            TxOp::Delay { us } => {
                bus.t = VTime(bus.t.0.saturating_add(VTime::from_us(*us).0));
            }
            TxOp::Probe { addr, ack } => {
                let list = [
                    cmd(Op::Restart, 0, false),
                    cmd(Op::Write, 1, true),
                    cmd(Op::Stop, 0, false),
                ];
                let now = bus.t;
                let end = run_list(i2c, ledger, bus, now, &list, &[addr << 1]);
                let want = if *ack { RunEnd::Complete } else { RunEnd::Nack };
                assert_eq!(end, want, "op {index}: probe {addr:#04X}");
            }
            TxOp::Write { addr, reg, values } => {
                // `i2c_master_transmit(dev, [reg, val], 2, 100)`: one WRITE step of `1 + 1 +
                // values.len()` bytes.
                let byte_num = u8::try_from(2 + values.len()).expect("BSP writes are short");
                let list = [
                    cmd(Op::Restart, 0, false),
                    cmd(Op::Write, byte_num, true),
                    cmd(Op::Stop, 0, false),
                ];
                let mut tx = vec![addr << 1, *reg];
                tx.extend_from_slice(values);
                let now = bus.t;
                let end = run_list(i2c, ledger, bus, now, &list, &tx);
                assert_eq!(end, RunEnd::Complete, "op {index}: write {addr:#04X}");
            }
            TxOp::Read { addr, reg, expect } => {
                // `i2c_master_transmit_receive(dev, [reg], 1, buf, n, 100)`: the `READ n-1`
                // step exists only when more than one byte is read.
                let n = u8::try_from(expect.len()).expect("BSP reads are short");
                let mut list = vec![
                    cmd(Op::Restart, 0, false),
                    cmd(Op::Write, 2, true),
                    cmd(Op::Restart, 0, false),
                    cmd(Op::Write, 1, true),
                ];
                if n > 1 {
                    list.push(cmd(Op::Read, n - 1, false));
                }
                list.push(Cmd {
                    byte_num: 1,
                    ack_en: false,
                    // The master NACKs the last byte it reads.
                    ack_val: true,
                    op: Op::Read,
                });
                list.push(cmd(Op::Stop, 0, false));
                let tx = [addr << 1, *reg, (addr << 1) | 1];
                let now = bus.t;
                let end = run_list(i2c, ledger, bus, now, &list, &tx);
                assert_eq!(end, RunEnd::Complete, "op {index}: read {addr:#04X}");
                let got: Vec<u8> = (0..expect.len())
                    .map(|_| i2c.load(I2C_DATA, Size::B4, now, ledger) as u8)
                    .collect();
                assert_eq!(&got, expect, "op {index}: read {addr:#04X} reg {reg:#04X}");
            }
        }
    }
}

/// `bsp_i2c_scan` probes 112 addresses and only 0x18 and 0x63 answer. Every other
/// probe must NACK inside the access that started it: a probe that times out costs 50 ms plus a bus
/// reset, and the scan alone would take about 5.6 s. Runs through `wiring::i2c`.
#[test]
fn t0_m5_the_boot_bus_scan_acknowledges_only_the_codec_and_the_gauge() {
    let mut i2c = i2c0::Model::default();
    let mut ledger = FidelityLedger::default();
    let mut board = MockBoard::new()
        .with_i2c_device(CODEC)
        .with_i2c_device(GAUGE);
    let t = VTime(0);

    let mut acked = Vec::new();
    for addr in SCAN_FIRST..=SCAN_LAST {
        let list = [
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 1, true),
            cmd(Op::Stop, 0, false),
        ];
        i2c.store(I2C_INT_CLR, Size::B4, I2C_INT_ALL, t, &mut ledger);
        for (i, c) in list.iter().enumerate() {
            i2c.store(
                I2C_COMD0 + 4 * i as u32,
                Size::B4,
                c.encode(),
                t,
                &mut ledger,
            );
        }
        i2c.store(I2C_DATA, Size::B4, u32::from(addr << 1), t, &mut ledger);
        i2c.store(I2C_CTR, Size::B4, I2C_START_WRITE, t, &mut ledger);
        let end = wiring::i2c::run(&mut i2c, t, &mut board);

        let raw = i2c.load(I2C_INT_RAW, Size::B4, t, &mut ledger);
        match end {
            RunEnd::Complete => {
                assert_eq!(raw & (1 << 7), 1 << 7, "{addr:#04X}: TRANS_COMPLETE");
                acked.push(addr);
            }
            RunEnd::Nack => assert_eq!(raw & (1 << 10), 1 << 10, "{addr:#04X}: NACK"),
            other => panic!("{addr:#04X}: a probe ended {other:?}"),
        }
        assert!(
            !i2c.bus_busy(),
            "{addr:#04X}: the bus is released either way"
        );
    }

    assert_eq!(acked, [CODEC, GAUGE]);
    assert_eq!(
        usize::from(SCAN_LAST - SCAN_FIRST + 1),
        112,
        "the scan is 112 probes",
    );

    // 112 starts and 112 stops, and one written byte per chip that answered.
    let starts = board.calls_to("i2c_start").len();
    let stops = board.calls_to("i2c_stop").len();
    assert_eq!((starts, stops), (112, 112));
    assert!(
        board
            .log()
            .iter()
            .all(|call| !matches!(call, BoardCall::I2cRead { .. })),
        "a probe reads nothing",
    );
}

/// The `i2c-boot-probe.txt` transcript (the two acknowledging addresses) replays through the
/// executor against the real chips.
#[test]
fn t0_m5_the_boot_probe_transcript_replays_against_the_two_chips() {
    let transcript = Transcript::parse(I2C_BOOT_PROBE).expect("the transcript parses");
    let mut i2c = i2c0::Model::default();
    let mut ledger = FidelityLedger::default();
    let mut bus = ChipBus::new();
    replay(&mut i2c, &mut ledger, &mut bus, &transcript);

    // The addresses the file leaves out are a property of the bus, so they are checked here.
    for addr in [0x08, 0x17, 0x19, 0x62, 0x64, 0x77] {
        assert!(
            !probe(&mut i2c, &mut ledger, &mut bus, addr),
            "{addr:#04X} must NACK",
        );
    }
}

/// The ES8311 write trace of `bsp_audio_init` through the I2C0 executor is the transcripts' trace
/// byte for byte; the chip tests compare the transcripts with `specs/es8311-sequences.toml`, which
/// closes the chain from the spec file to the wire.
#[test]
fn t0_m5_the_es8311_sequences_replay_through_the_executor() {
    for (name, text) in [
        ("A open", ES8311_A_OPEN),
        ("B set_format", ES8311_B_SET_FORMAT),
        ("C set_volume", ES8311_C_SET_VOLUME),
        ("D close", ES8311_D_CLOSE),
    ] {
        let transcript = Transcript::parse(text).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let mut i2c = i2c0::Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = ChipBus::new();
        replay(&mut i2c, &mut ledger, &mut bus, &transcript);
        assert_eq!(bus.writes, transcript.writes(), "{name}");
    }

    // Volume 80 gives register 0x32 = 0xB2, which sequence C is the write trace of.
    let transcript = Transcript::parse(ES8311_C_SET_VOLUME).expect("sequence C parses");
    assert!(
        transcript.writes().contains(&(0x32, 0xB2)),
        "sequence C sets the volume register",
    );
}

/// `bsp_battery_init` on a provisioned gauge is 1 probe plus 1 + 1 + 80 reads and no write, each
/// read returning the transcript's byte through the executor. The fresh-gauge path replays too.
#[test]
fn t0_m5_the_cw2017_init_transcripts_replay_through_the_executor() {
    let fast = Transcript::parse(CW2017_INIT_FAST).expect("the fast path parses");
    let mut i2c = i2c0::Model::default();
    let mut ledger = FidelityLedger::default();
    let mut bus = ChipBus::new();
    replay(&mut i2c, &mut ledger, &mut bus, &fast);
    assert!(
        bus.writes.is_empty(),
        "the provisioned gauge takes the fast path: no writes",
    );

    let fresh = Transcript::parse(CW2017_INIT_FRESH).expect("the fresh path parses");
    let mut i2c = i2c0::Model::default();
    let mut bus = ChipBus::new();
    bus.gauge = Cw2017::fresh();
    replay(&mut i2c, &mut ledger, &mut bus, &fresh);
    assert_eq!(
        bus.writes,
        fresh.writes(),
        "the fresh gauge takes the slow path: the profile is written",
    );
}

/// A press on each button puts its ladder voltage on GPIO0, and one APB_SARADC conversion gives
/// the raw code 3 for UP, 393 for DOWN, 782 for OK and 4095 released (the device's codes, DOWN one
/// below the device's 394, `boards/ai-passport.toml`). Runs through `wiring::adc`.
#[test]
fn t0_m5_a_button_press_converts_to_its_ladder_code() {
    let cfg = LadderConfig::default();
    let mut ladder = ButtonLadder::new(cfg);
    let mut adc = saradc::Model::default();
    let mut ledger = FidelityLedger::default();
    let t = VTime(0);
    // `adc_oneshot_hal_setup`: attenuation 3 (12 dB), ADC1 channel 0.
    let setup = (3 << 23) | (1 << 31);

    let cases = [
        (Some(ButtonId::Up), cfg.button_raw(ButtonId::Up)),
        (Some(ButtonId::Down), cfg.button_raw(ButtonId::Down)),
        (Some(ButtonId::Ok), cfg.button_raw(ButtonId::Ok)),
        (None, cfg.released_raw()),
    ];
    for (button, want) in cases {
        ladder.release_all();
        if let Some(id) = button {
            ladder.set(id, true);
        }
        let board = MockBoard::new().with_adc_mv(cfg.adc_unit, cfg.adc_channel, ladder.adc_mv());

        // `adc_oneshot_hal_convert`: clear the event, drop start, raise start.
        adc.store(0x04C, Size::B4, 1 << 31, t, &mut ledger);
        adc.store(0x020, Size::B4, setup, t, &mut ledger);
        adc.store(0x020, Size::B4, setup | (1 << 29), t, &mut ledger);
        wiring::adc::sample(&mut adc, t, cfg.adc_unit, cfg.adc_channel, &board);

        assert_eq!(
            adc.load(0x044, Size::B4, t, &mut ledger) & (1 << 31),
            1 << 31,
            "{button:?}: the done bit the driver polls",
        );
        assert_eq!(
            adc.load(0x02C, Size::B4, t, &mut ledger) & 0xFFF,
            u32::from(want),
            "{button:?}: raw code",
        );
        assert_eq!(
            ladder.raw_code(adc.calibration()),
            want,
            "{button:?}: the board and the converter agree",
        );
    }
    assert_eq!(cfg.raw_code, [3, 393, 782, 4_095], "`raw_code`");
}

/// A descriptor ring of buffers of `bytes` each, standing in for the GDMA walk so the I2S side is
/// tested alone (6 descriptors of `frame_num * bytes_per_sample * active_slots`).
struct StubRing {
    bytes: u32,
    started: bool,
    tx: Vec<Vec<u8>>,
    rx: Vec<Vec<u8>>,
}

impl I2sDma for StubRing {
    fn period_bytes(&mut self, _dir: Dir) -> Option<u32> {
        self.started.then_some(self.bytes)
    }

    fn take_tx(&mut self, out: &mut Vec<u8>) {
        if !self.tx.is_empty() {
            *out = self.tx.remove(0);
        }
    }

    fn put_rx(&mut self, bytes: &[u8]) {
        self.rx.push(bytes.to_vec());
    }
}

/// With the BSP's 16 kHz stereo configuration and 240-frame buffers, I2S0 pacing events are spaced
/// exactly `frames x 1e12 / fs` ps, each period moves one buffer, and the PCM lands in
/// `HostIo::audio_out` with the rate and channel count the clock registers imply.
#[test]
fn t0_m5_one_i2s_period_per_buffer_paces_the_pcm_into_host_io() {
    let mut i2s = i2s0::Model::default();
    let mut ledger = FidelityLedger::default();
    let mut sched = Scheduler::default();
    let mut board = MockBoard::new();
    let mut io = HostIo::new(1 << 16);

    // The clock `bsp_audio.c` configures: PLL_F160M, `div_num` 39 with 15/0/1/0, `bck_div_num` 7,
    // 16-bit, stereo with both slots enabled.
    let clk_active = 1 << 26;
    let clk_sel_pll = 2 << 27;
    let conf1 = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
    let tdm_stereo = 0b11 | (1 << 16);
    let conf = (1 << 19) | (1 << 15);
    i2s.store(
        0x034,
        Size::B4,
        clk_sel_pll | clk_active | 39,
        VTime(0),
        &mut ledger,
    );
    i2s.store(0x03C, Size::B4, (15 << 18) | 1, VTime(0), &mut ledger);
    i2s.store(0x02C, Size::B4, conf1, VTime(0), &mut ledger);
    i2s.store(0x054, Size::B4, tdm_stereo, VTime(0), &mut ledger);
    i2s.store(0x024, Size::B4, conf | (1 << 8), VTime(0), &mut ledger);
    assert_eq!(i2s.fs_hz(Dir::Tx), 16_000);
    assert_eq!(i2s.format(Dir::Tx), PcmFormat::BSP);

    // 240 frames of stereo 16-bit is 960 bytes.
    let frames = 240usize;
    let buffer_bytes = frames * 2 * 2;
    let periods = 4;
    let ring_buffers: Vec<Vec<u8>> = (0..periods)
        .map(|n| {
            (0..frames * 2)
                .flat_map(|i| i16::to_le_bytes((n * 1_000 + i) as i16))
                .collect()
        })
        .collect();
    let mut dma = StubRing {
        bytes: buffer_bytes as u32,
        started: true,
        tx: ring_buffers.clone(),
        rx: Vec::new(),
    };

    // `i2s_channel_enable` sets tx_start; the wiring arms the first period.
    let mut now = VTime(0);
    i2s.store(0x024, Size::B4, conf | (1 << 2), now, &mut ledger);
    wiring::i2s::service(
        &mut i2s,
        Dir::Tx,
        now,
        &mut sched,
        &mut dma,
        &mut board,
        &mut io,
    );

    let period_ps = frames as u64 * 1_000_000_000_000 / 16_000;
    assert_eq!(period_ps, 15_000_000_000, "240 frames at 16 kHz is 15 ms");
    let mut fired = Vec::new();
    for _ in 0..periods {
        let at = sched.next_time().expect("a period is scheduled");
        now = at;
        let key = sched.pop_due(now).expect("the period comes due");
        assert_eq!(key.tag, 0, "the TX pacing tag");
        fired.push(at);
        i2s.period_due(Dir::Tx);
        wiring::i2s::service(
            &mut i2s,
            Dir::Tx,
            now,
            &mut sched,
            &mut dma,
            &mut board,
            &mut io,
        );
    }

    for (n, at) in fired.iter().enumerate() {
        assert_eq!(
            at.0,
            (n as u64 + 1) * period_ps,
            "period {n} is exactly one buffer after the last",
        );
    }

    // Every buffer reached the codec once, in order, with the layout the clock registers imply.
    let dac: Vec<BoardCall> = board.calls_to("i2s_dac");
    assert_eq!(dac.len(), periods);
    for (n, call) in dac.iter().enumerate() {
        let BoardCall::I2sDac {
            fmt, frames: pcm, ..
        } = call
        else {
            panic!("call {n} is not an i2s_dac");
        };
        assert_eq!(*fmt, PcmFormat::BSP);
        assert_eq!(pcm.len(), frames * 2);
        assert_eq!(pcm[0], (n * 1_000) as i16, "buffer {n} in ring order");
    }

    // The same frames are in the host ring, as one record at 16 kHz, two channels.
    assert_eq!(io.audio_out.len(), periods * frames * 2);
    let record = io
        .audio_out
        .record_at(io.audio_out.record_tail())
        .expect("a record");
    assert_eq!(
        (record.fs, record.channels, record.vt_start),
        (16_000, 2, VTime(0))
    );
}

/// Under `Strictness::Strict` a milestone fails on the first touch of an unclassed register, so
/// every register the i2c0, saradc and i2s0 models touch on the M5 paths needs a class row in its
/// `specs/blocks/<block>.toml`. The ledger is shared across the three blocks, as a machine's is.
#[test]
fn t0_m5_the_registers_the_m5_paths_touch_all_carry_a_class() {
    let mut ledger = FidelityLedger::default();
    let t = VTime(0);

    let mut i2c = i2c0::Model::default();
    let mut bus = ChipBus::new();
    assert!(probe(&mut i2c, &mut ledger, &mut bus, CODEC));
    let read = Transcript::parse("R 63 00 0F").expect("one read transaction parses");
    replay(&mut i2c, &mut ledger, &mut bus, &read);

    let cfg = LadderConfig::default();
    let mut adc = saradc::Model::default();
    let board = MockBoard::new().with_adc_mv(cfg.adc_unit, cfg.adc_channel, 300);
    let setup = (3 << 23) | (1 << 31);
    adc.store(0x04C, Size::B4, 1 << 31, t, &mut ledger);
    adc.store(0x040, Size::B4, 1 << 31, t, &mut ledger);
    adc.store(0x020, Size::B4, setup, t, &mut ledger);
    adc.store(0x020, Size::B4, setup | (1 << 29), t, &mut ledger);
    wiring::adc::sample(&mut adc, t, cfg.adc_unit, cfg.adc_channel, &board);
    adc.load(0x044, Size::B4, t, &mut ledger);
    adc.load(0x048, Size::B4, t, &mut ledger);
    adc.load(0x02C, Size::B4, t, &mut ledger);

    // I2S0: the `bsp_audio_init` configuration of both directions plus the two update polls.
    let mut i2s = i2s0::Model::default();
    let conf1 = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
    for (clkm, div, c1, tdm, conf) in [
        (0x034u32, 0x03Cu32, 0x02Cu32, 0x054u32, 0x024u32),
        (0x030, 0x038, 0x028, 0x050, 0x020),
    ] {
        i2s.store(clkm, Size::B4, (2 << 27) | (1 << 26) | 39, t, &mut ledger);
        i2s.store(div, Size::B4, (15 << 18) | 1, t, &mut ledger);
        i2s.store(c1, Size::B4, conf1, t, &mut ledger);
        i2s.store(tdm, Size::B4, 0b11 | (1 << 16), t, &mut ledger);
        i2s.store(
            conf,
            Size::B4,
            (1 << 19) | (1 << 15) | (1 << 8),
            t,
            &mut ledger,
        );
        i2s.load(conf, Size::B4, t, &mut ledger);
    }
    i2s.store(0x064, Size::B4, 960, t, &mut ledger);
    i2s.store(0x014, Size::B4, 0xF, t, &mut ledger);
    i2s.load(0x06C, Size::B4, t, &mut ledger);

    let unmodeled: Vec<String> = ledger
        .unmodeled()
        .map(|touch| format!("{:?} +{:#05X}", touch.periph, touch.off))
        .collect();
    assert!(
        unmodeled.is_empty(),
        "M5 claims these blocks, so every register their paths touch needs a class row: {unmodeled:?}",
    );
    assert!(
        ledger.first_touches().len() >= 25,
        "the paths touch the blocks"
    );
}

/// The M5 strict gate has no offset-0 hole: `i2c0` +0x000 (`I2C_SCL_LOW_PERIOD`) and `saradc`
/// +0x000 (`APB_SARADC_CTRL`) are class C rows: stored and read back, not honored.
#[test]
fn t0_m5_the_offset_zero_registers_carry_their_class() {
    let mut ledger = FidelityLedger::default();
    let t = VTime(0);
    assert_eq!(
        i2c0::Model::default().fidelity(0),
        Fidelity::C,
        "i2c0 I2C_SCL_LOW_PERIOD"
    );
    assert_eq!(
        saradc::Model::default().fidelity(0),
        Fidelity::C,
        "saradc APB_SARADC_CTRL"
    );

    // The setup's values read back, which is all class C promises: SCL_LOW 199 for 100 kHz
    // and CTRL with `sar_clk_gated` set.
    let mut i2c = i2c0::Model::default();
    i2c.store(0x000, Size::B4, 199, t, &mut ledger);
    assert_eq!(i2c.load(0x000, Size::B4, t, &mut ledger), 199);
    let mut adc = saradc::Model::default();
    let ctrl = adc.load(0x000, Size::B4, t, &mut ledger) | (1 << 6);
    adc.store(0x000, Size::B4, ctrl, t, &mut ledger);
    assert_eq!(adc.load(0x000, Size::B4, t, &mut ledger), ctrl);
    let unmodeled: Vec<_> = ledger.unmodeled().collect();
    assert!(
        unmodeled.is_empty(),
        "a first touch of either offset-0 register is classed: {unmodeled:?}"
    );

    assert_eq!(
        i2s0::Model::default().fidelity(0),
        Fidelity::U,
        "i2s0 has no register at +0x000 at all (its table starts at INT_RAW +0x00C), so this row \
         is a hole rather than an unclassified register",
    );
}

/// An offset inside a block window with no register row is reported unclassified, so a strict
/// milestone stops on it, even with the block's own class noted: `FidelityLedger::class_of` falls
/// back to the `LedgerSubject::Block` note, and all three block files declare `class = "B"`, so a
/// hole that noted no class of its own would otherwise read as modeled.
#[test]
fn t0_m5_a_hole_in_a_block_window_stays_unmodeled_under_a_block_class_note() {
    let mut ledger = FidelityLedger::default();
    let t = VTime(0);
    let mut i2c = i2c0::Model::default();
    let mut adc = saradc::Model::default();
    let mut i2s = i2s0::Model::default();

    for id in [
        <i2c0::Model as Peripheral>::ID,
        <saradc::Model as Peripheral>::ID,
        <i2s0::Model as Peripheral>::ID,
    ] {
        ledger.note(LedgerSubject::Block(id), Fidelity::B);
    }

    // One classed register per block first, to show the note is the register's own.
    i2c.load(I2C_INT_RAW, Size::B4, t, &mut ledger);
    adc.load(0x044, Size::B4, t, &mut ledger);
    i2s.load(0x024, Size::B4, t, &mut ledger);
    assert_eq!(ledger.unmodeled().count(), 0, "classed rows are claimed");

    let holes = [
        (<i2c0::Model as Peripheral>::ID, 0x900u32),
        (<saradc::Model as Peripheral>::ID, 0x900),
        (<i2s0::Model as Peripheral>::ID, 0x900),
    ];
    assert_eq!(i2c.load(0x900, Size::B4, t, &mut ledger), 0);
    i2c.store(0x900, Size::B4, 0xFF, t, &mut ledger);
    assert_eq!(adc.load(0x900, Size::B4, t, &mut ledger), 0);
    assert_eq!(i2s.load(0x900, Size::B4, t, &mut ledger), 0);

    for (periph, off) in holes {
        assert_eq!(
            ledger.class_of(LedgerSubject::Register { periph, off }),
            Fidelity::U,
            "{periph:?} +{off:#05X} carries its own U note, not the block's B",
        );
    }
    let unmodeled: Vec<(u32,)> = ledger.unmodeled().map(|t| (t.off,)).collect();
    assert_eq!(
        unmodeled.len(),
        holes.len(),
        "every hole is reported to the strict gate: {unmodeled:?}",
    );
}

/// Block offsets of the I2S0 registers the harness tests drive (IDF
/// `soc/esp32c3/register/soc/i2s_reg.h`).
const I2S_RX_CONF: u32 = 0x020;
const I2S_TX_CONF: u32 = 0x024;
const I2S_RX_CONF1: u32 = 0x028;
const I2S_TX_CONF1: u32 = 0x02C;
const I2S_RX_CLKM_CONF: u32 = 0x030;
const I2S_TX_CLKM_CONF: u32 = 0x034;
const I2S_RX_CLKM_DIV_CONF: u32 = 0x038;
const I2S_TX_CLKM_DIV_CONF: u32 = 0x03C;
const I2S_RX_TDM_CTRL: u32 = 0x050;
const I2S_TX_TDM_CTRL: u32 = 0x054;
const I2S_RXEOF_NUM: u32 = 0x064;

/// The clock `bsp_audio.c` configures, for one direction: PLL_F160M with `div_num` 39 and the
/// fractional 15/0/1/0, `bck_div_num` 7, 16-bit, stereo with both slots enabled. The direction is
/// left configured but not started.
fn configure_i2s(i2s: &mut i2s0::Model, ledger: &mut FidelityLedger, dir: Dir, t: VTime) {
    let (conf, conf1, clkm, div, tdm) = match dir {
        Dir::Tx => (
            I2S_TX_CONF,
            I2S_TX_CONF1,
            I2S_TX_CLKM_CONF,
            I2S_TX_CLKM_DIV_CONF,
            I2S_TX_TDM_CTRL,
        ),
        Dir::Rx => (
            I2S_RX_CONF,
            I2S_RX_CONF1,
            I2S_RX_CLKM_CONF,
            I2S_RX_CLKM_DIV_CONF,
            I2S_RX_TDM_CTRL,
        ),
    };
    let conf1_bsp = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
    i2s.store(clkm, Size::B4, (2 << 27) | (1 << 26) | 39, t, ledger);
    i2s.store(div, Size::B4, (15 << 18) | 1, t, ledger);
    i2s.store(conf1, Size::B4, conf1_bsp, t, ledger);
    i2s.store(tdm, Size::B4, 0b11 | (1 << 16), t, ledger);
    i2s.store(conf, Size::B4, (1 << 19) | (1 << 15), t, ledger);
}

/// `*_CONF` with the BSP flags and `start` set (IDF `i2s_channel_enable`).
const I2S_CONF_START: u32 = (1 << 19) | (1 << 15) | (1 << 2);
const I2S_CONF_STOP: u32 = (1 << 19) | (1 << 15);

/// On `RegHarness` (a `Cx` plus a `MockBoard`, no machine): the busy-wait rows of
/// `specs/blocks/{i2c0,saradc,i2s0}.toml` rest on `Peripheral::stable_until` answering that a poll
/// of `i2c0.trans_complete_or_nack`, `saradc.adc1_done`, `i2s0.tx_update` and `i2s0.rx_update`
/// holds. Fields are looked up by name in the `RegSpec`, so a renamed or moved field fails here.
#[test]
fn t0_m5_the_busy_wait_rows_answer_the_hang_detector_with_a_stable_read() {
    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();

    let mut i2c = i2c0::Model::default();
    for off in [I2C_INT_RAW, 0x008] {
        assert!(
            matches!(i2c.stability(off), Stability::UntilNextEvent),
            "i2c0 +{off:#05X} carries the trans_complete_or_nack row",
        );
    }
    assert!(matches!(i2c.stability(I2C_CTR), Stability::Never));
    probe(&mut i2c, &mut ledger, &mut ChipBus::new(), CODEC);
    h.assert_field("i2c0", i2c.regs(), "I2C_SR", "I2C_BUS_BUSY", 0);
    h.assert_field(
        "i2c0",
        i2c.regs(),
        "I2C_INT_RAW",
        "I2C_TRANS_COMPLETE_INT_RAW",
        1,
    );
    h.assert_field("i2c0", i2c.regs(), "I2C_CTR", "I2C_TRANS_START", 0);

    let mut adc = saradc::Model::default();
    assert!(matches!(adc.stability(0x044), Stability::UntilNextEvent));
    assert!(
        matches!(adc.stability(0x02C), Stability::UntilInput),
        "the raw code follows the modeled button state, not time",
    );
    let cfg = LadderConfig::default();
    h.board = MockBoard::new().with_adc_mv(cfg.adc_unit, cfg.adc_channel, 300);
    let setup = (3 << 23) | (1 << 31);
    adc.store(0x04C, Size::B4, 1 << 31, h.now, &mut ledger);
    adc.store(0x020, Size::B4, setup, h.now, &mut ledger);
    adc.store(0x020, Size::B4, setup | (1 << 29), h.now, &mut ledger);
    wiring::adc::sample(&mut adc, h.now, cfg.adc_unit, cfg.adc_channel, &h.board);
    h.assert_field(
        "saradc",
        adc.regs(),
        "APB_SARADC_INT_RAW",
        "APB_SARADC_ADC1_DONE_INT_RAW",
        1,
    );

    let mut i2s = i2s0::Model::default();
    for off in [I2S_TX_CONF, I2S_RX_CONF] {
        assert!(
            matches!(i2s.stability(off), Stability::UntilNextEvent),
            "i2s0 +{off:#05X} carries a *_update row",
        );
    }
    configure_i2s(&mut i2s, &mut ledger, Dir::Tx, h.now);
    i2s.store(
        I2S_TX_CONF,
        Size::B4,
        I2S_CONF_STOP | (1 << 8),
        h.now,
        &mut ledger,
    );
    h.assert_field("i2s0", i2s.regs(), "I2S_TX_CONF", "I2S_TX_UPDATE", 0);
    h.assert_field("i2s0", i2s.regs(), "I2S_TX_CONF", "I2S_TX_START", 0);
}

/// Decodes a descriptor the RX path wrote back into interleaved samples (little-endian
/// signed 16-bit).
fn samples_of(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// The RX half of the PCM path drains `HostIo::audio_in` (where `InputEvent::MicChunk` samples
/// land) into the descriptor the guest reads. A dry ring pads with zeros and counts the shortfall,
/// the only producer of the wasm layout's `SLOT_AUDIO_IN_UNDERFLOWS`. Driven on `RegHarness`.
#[test]
fn t0_m5_an_rx_period_writes_the_host_injected_microphone_audio_into_the_descriptor() {
    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);

    configure_i2s(&mut i2s, &mut ledger, Dir::Rx, h.now);
    // `i2s_channel_init_std_mode` writes the descriptor size into `RXEOF_NUM`.
    let frames = 240usize;
    let buffer_bytes = frames * 2 * 2;
    i2s.store(
        I2S_RXEOF_NUM,
        Size::B4,
        buffer_bytes as u32,
        h.now,
        &mut ledger,
    );
    assert_eq!(i2s.fs_hz(Dir::Rx), 16_000);
    assert_eq!(i2s.format(Dir::Rx), PcmFormat::BSP);

    let injected: Vec<i16> = (0..frames * 2).map(|i| (i as i16) - 100).collect();
    assert_eq!(io.audio_in.push(&injected), injected.len());

    let mut dma = StubRing {
        bytes: buffer_bytes as u32,
        started: true,
        tx: Vec::new(),
        rx: Vec::new(),
    };
    i2s.store(I2S_RX_CONF, Size::B4, I2S_CONF_START, h.now, &mut ledger);
    wiring::i2s::service(
        &mut i2s,
        Dir::Rx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );

    // Two periods: the first drains the chunk, the second finds the ring dry.
    for period in 0..2 {
        let due = h.advance_to_next_event().expect("a period is scheduled");
        assert_eq!(due.len(), 1, "period {period}");
        assert_eq!(due[0].tag, Dir::Rx.tag());
        assert!(matches!(i2s.event(due[0].tag), Wiring::I2sPeriod(Dir::Rx)));
        wiring::i2s::service(
            &mut i2s,
            Dir::Rx,
            h.now,
            &mut h.sched,
            &mut dma,
            &mut h.board,
            &mut io,
        );
    }

    assert_eq!(dma.rx.len(), 2, "one descriptor per period");
    assert_eq!(
        samples_of(&dma.rx[0]),
        injected,
        "the guest reads the samples the host injected, not the codec's silence",
    );
    assert_eq!(io.audio_in.len(), 0, "the transport was drained");
    assert_eq!(
        samples_of(&dma.rx[1]),
        vec![0i16; frames * 2],
        "a dry ring yields zeros",
    );
    assert_eq!(
        io.audio_in.underflows(),
        (frames * 2) as u64,
        "and the zeros are counted",
    );

    // The codec was asked for every period all the same: it is the analog capture path.
    assert_eq!(h.board.calls_to("i2s_adc").len(), 2);
}

/// A run the host never fed leaves `HostIo::audio_in` alone and reports no underflow: the codec
/// stays the capture source. The other half of the precedence rule `wiring::i2s` documents.
#[test]
fn t0_m5_an_rx_period_without_host_audio_keeps_the_codec_samples() {
    let mut h = RegHarness::with_board(MockBoard::new().with_mic(&[11, 12, 13, 14]));
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);

    configure_i2s(&mut i2s, &mut ledger, Dir::Rx, h.now);
    i2s.store(I2S_RXEOF_NUM, Size::B4, 8, h.now, &mut ledger);
    let mut dma = StubRing {
        bytes: 8,
        started: true,
        tx: Vec::new(),
        rx: Vec::new(),
    };
    i2s.store(I2S_RX_CONF, Size::B4, I2S_CONF_START, h.now, &mut ledger);
    wiring::i2s::service(
        &mut i2s,
        Dir::Rx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    let due = h.advance_to_next_event().expect("a period is scheduled");
    i2s.event(due[0].tag);
    wiring::i2s::service(
        &mut i2s,
        Dir::Rx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );

    assert_eq!(samples_of(&dma.rx[0]), vec![11, 12, 13, 14]);
    assert_eq!(io.audio_in.underflows(), 0, "nothing was asked of the ring");
}

/// A snapshot taken while a direction runs carries the pending period and the handle that cancels
/// it, so a stop after a restore cancels the event as an uninterrupted run does. A period event
/// that reaches a stopped direction anyway moves nothing: `*_CONF.start` is clear.
#[test]
fn t0_m5_a_period_event_after_a_stop_plays_no_extra_descriptor() {
    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);

    configure_i2s(&mut i2s, &mut ledger, Dir::Tx, h.now);
    let frames = 240usize;
    let buffer_bytes = frames * 2 * 2;
    let mut dma = StubRing {
        bytes: buffer_bytes as u32,
        started: true,
        tx: (0..2)
            .map(|n| {
                (0..frames * 2)
                    .flat_map(|i| i16::to_le_bytes((n * 1_000 + i) as i16))
                    .collect()
            })
            .collect(),
        rx: Vec::new(),
    };

    i2s.store(I2S_TX_CONF, Size::B4, I2S_CONF_START, h.now, &mut ledger);
    wiring::i2s::service(
        &mut i2s,
        Dir::Tx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    let pending = i2s.pending(Dir::Tx).expect("the first period is armed");
    assert!(h.sched.is_pending(pending));

    // The handle is block state a snapshot carries; a build that dropped it would leave this pending.
    let wiring = i2s.store(I2S_TX_CONF, Size::B4, I2S_CONF_STOP, h.now, &mut ledger);
    assert!(matches!(wiring, Wiring::I2sPeriod(Dir::Tx)));
    assert_eq!(i2s.pending(Dir::Tx), Some(pending));
    wiring::i2s::service(
        &mut i2s,
        Dir::Tx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    assert_eq!(i2s.pending(Dir::Tx), None);
    assert!(!h.sched.is_pending(pending));
    assert_eq!(h.sched.len(), 0, "the stop left no pending period");

    assert!(matches!(i2s.event(Dir::Tx.tag()), Wiring::None));
    wiring::i2s::service(
        &mut i2s,
        Dir::Tx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    assert!(h.board.calls_to("i2s_dac").is_empty());
    assert_eq!(io.audio_out.len(), 0);
    assert_eq!(dma.tx.len(), 2, "both descriptors are still in the ring");
}

/// `I2S_RXEOF_NUM` resets to 0x40 (IDF `i2s_reg.h:1005`), so there is no "unprogrammed" value. An
/// RX stream started before the driver writes it moves 64-byte periods, as the hardware counter
/// does: 16 stereo 16-bit frames, 1 ms apart at 16 kHz.
#[test]
fn t0_m5_an_rx_stream_started_before_rxeof_num_is_written_paces_64_byte_periods() {
    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);

    configure_i2s(&mut i2s, &mut ledger, Dir::Rx, h.now);
    assert_eq!(i2s.rx_eof_bytes(), 0x40, "the reset value, never 0");

    let mut dma = StubRing {
        bytes: 960,
        started: true,
        tx: Vec::new(),
        rx: Vec::new(),
    };
    i2s.store(I2S_RX_CONF, Size::B4, I2S_CONF_START, h.now, &mut ledger);
    wiring::i2s::service(
        &mut i2s,
        Dir::Rx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    let due = h.advance_to_next_event().expect("a period is scheduled");
    assert_eq!(
        h.now,
        VTime(16 * 1_000_000_000_000 / 16_000),
        "16 frames at 16 kHz is 1 ms, not the 240 frames of a full descriptor",
    );
    i2s.event(due[0].tag);
    wiring::i2s::service(
        &mut i2s,
        Dir::Rx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    assert_eq!(dma.rx[0].len(), 64, "RXEOF_NUM bytes, not the descriptor");
}

/// Only the 16-bit layout the board configures is decoded (`DATA_16BIT`; the byte order is
/// known for 16-bit only), and `Model::frame_bytes` decides so: a stream at another width
/// disarms instead of running period events that move no PCM.
#[test]
fn t0_m5_a_stream_at_a_width_the_pcm_path_cannot_decode_is_not_paced() {
    let mut h = RegHarness::new();
    let mut ledger = FidelityLedger::default();
    let mut i2s = i2s0::Model::default();
    let mut io = HostIo::new(1 << 16);

    configure_i2s(&mut i2s, &mut ledger, Dir::Tx, h.now);
    let mut dma = StubRing {
        bytes: 960,
        started: true,
        tx: vec![vec![0u8; 960]],
        rx: Vec::new(),
    };
    i2s.store(I2S_TX_CONF, Size::B4, I2S_CONF_START, h.now, &mut ledger);
    let paced = wiring::i2s::service(
        &mut i2s,
        Dir::Tx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    assert_eq!(h.sched.len(), 1, "16-bit paces");
    assert!(!paced.width_refused);

    // `bits_mod` 23 is a 24-bit slot, whose DMA layout no in-tree source gives.
    let conf1_bsp = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
    let conf1_24 = (conf1_bsp & !(0x1F << 13)) | (23 << 13);
    i2s.store(I2S_TX_CONF1, Size::B4, conf1_24, h.now, &mut ledger);
    assert_eq!(i2s.format(Dir::Tx).bits, 24);
    let serviced = wiring::i2s::service(
        &mut i2s,
        Dir::Tx,
        h.now,
        &mut h.sched,
        &mut dma,
        &mut h.board,
        &mut io,
    );
    assert_eq!(h.sched.len(), 0, "and an undecodable width disarms");
    assert!(
        serviced.width_refused,
        "and says so, for the machine to count"
    );
    assert!(h.board.calls_to("i2s_dac").is_empty());
    assert_eq!(dma.tx.len(), 1, "the descriptor is untouched");
}

// ---------------------------------------------------------------------------------------------
// The I2C0 executor against the real board (the CW2017 0xFF regression)
// ---------------------------------------------------------------------------------------------

/// `i2c_master_transmit_receive` as the IDF 5.x `i2c_master` driver builds it for a register read:
/// `RESTART; WRITE 2 (addr|W, reg) ack_en; RESTART; WRITE 1 (addr|R) ack_en;
/// READ n-1 ack_val 0; READ 1 ack_val 1; STOP`.
fn read_list(n: u8) -> Vec<Cmd> {
    let mut list = vec![
        cmd(Op::Restart, 0, false),
        cmd(Op::Write, 2, true),
        cmd(Op::Restart, 0, false),
        cmd(Op::Write, 1, true),
    ];
    if n > 1 {
        list.push(cmd(Op::Read, n - 1, false));
    }
    list.push(Cmd {
        ack_val: true,
        ..cmd(Op::Read, 1, false)
    });
    list.push(cmd(Op::Stop, 0, false));
    list
}

/// Loads one command list and its TX bytes as `s_i2c_transaction_start` does, starts
/// it, and runs it through `wiring::i2c` against `board`, the machine's path for `Wiring::I2cRun`.
fn run_on_board(
    i2c: &mut i2c0::Model,
    ledger: &mut FidelityLedger,
    board: &mut pemu_board::passport::PassportBoard,
    t: VTime,
    cmds: &[Cmd],
    tx: &[u8],
) -> RunEnd {
    i2c.store(I2C_FIFO_CONF, Size::B4, I2C_FIFO_RST, t, ledger);
    i2c.store(I2C_FIFO_CONF, Size::B4, 0, t, ledger);
    i2c.store(I2C_INT_CLR, Size::B4, I2C_INT_ALL, t, ledger);
    for (i, c) in cmds.iter().enumerate() {
        i2c.store(I2C_COMD0 + 4 * i as u32, Size::B4, c.encode(), t, ledger);
    }
    for byte in tx {
        i2c.store(I2C_DATA, Size::B4, u32::from(*byte), t, ledger);
    }
    let wiring = i2c.store(I2C_CTR, Size::B4, I2C_START_WRITE, t, ledger);
    assert!(
        matches!(wiring, Wiring::I2cRun),
        "trans_start runs the list"
    );
    wiring::i2c::run(i2c, t, board)
}

/// Reads `n` bytes from register `reg` of `addr` through the block and the real board, popping them
/// from `I2C_DATA` as `i2c_ll_read_rxfifo` does.
fn board_read(
    i2c: &mut i2c0::Model,
    ledger: &mut FidelityLedger,
    board: &mut pemu_board::passport::PassportBoard,
    addr: u8,
    reg: u8,
    n: u8,
) -> Vec<u8> {
    let t = VTime::from_ms(230);
    let end = run_on_board(
        i2c,
        ledger,
        board,
        t,
        &read_list(n),
        &[addr << 1, reg, addr << 1 | 1],
    );
    assert_eq!(end, RunEnd::Complete, "{addr:#04X} reg {reg:#04X}");
    assert!(!i2c.bus_busy(), "the stop released the bus");
    (0..n)
        .map(|_| i2c.load(I2C_DATA, Size::B4, t, ledger) as u8)
        .collect()
}

/// A register read through the block, `wiring::i2c` and the real `PassportBoard` reaches the
/// chips: VERSION, SOC_ALERT and the first profile byte return the provisioned gauge's 0x0F, 0x94
/// and 0x64, not the floating bus's 0xFF, and the ES8311 answers a write-then-read of the volume
/// register.
#[test]
fn t0_m5_a_register_read_through_the_executor_reaches_the_board_chips() {
    use pemu_board::cw2017::{BSP_PROFILE, SOC_ALERT_PROVISIONED, VERSION_OVERRIDE};
    use pemu_board::passport::{BoardConfig, PassportBoard};

    let mut i2c = i2c0::Model::default();
    let mut ledger = FidelityLedger::default();
    let mut board = PassportBoard::from_toml(&BoardConfig::default());

    assert_eq!(
        board_read(&mut i2c, &mut ledger, &mut board, GAUGE, 0x00, 1),
        [VERSION_OVERRIDE],
        "CW2017 VERSION (the device reads 0x0F)",
    );
    assert_eq!(
        board_read(&mut i2c, &mut ledger, &mut board, GAUGE, 0x0B, 1),
        [SOC_ALERT_PROVISIONED],
    );
    assert_eq!(
        board_read(&mut i2c, &mut ledger, &mut board, GAUGE, 0x10, 1),
        [BSP_PROFILE[0]],
        "the byte `bsp_battery_init` compared as 0x64 and got 0xFF",
    );
    assert_eq!(BSP_PROFILE[0], 0x64);
    // A read of more than one byte in one list auto-increments the pointer.
    assert_eq!(
        board_read(&mut i2c, &mut ledger, &mut board, GAUGE, 0x10, 32),
        BSP_PROFILE[..32]
    );

    let write = [
        cmd(Op::Restart, 0, false),
        cmd(Op::Write, 3, true),
        cmd(Op::Stop, 0, false),
    ];
    let end = run_on_board(
        &mut i2c,
        &mut ledger,
        &mut board,
        VTime::from_ms(270),
        &write,
        &[CODEC << 1, 0x32, 0xB2],
    );
    assert_eq!(end, RunEnd::Complete);
    assert_eq!(board.codec.reg(0x32), 0xB2, "the write reached the codec");
    assert_eq!(
        board_read(&mut i2c, &mut ledger, &mut board, CODEC, 0x32, 1),
        [0xB2]
    );
    assert_eq!(
        board_read(&mut i2c, &mut ledger, &mut board, CODEC, 0x0D, 1),
        [Es8311::new().reg(0x0D)],
        "an unwritten codec register reads its reset value",
    );

    // An absent address still NACKs fast, and nothing is read.
    let end = run_on_board(
        &mut i2c,
        &mut ledger,
        &mut board,
        VTime::from_ms(271),
        &read_list(1),
        &[0x50 << 1, 0x00, 0x50 << 1 | 1],
    );
    assert_eq!(end, RunEnd::Nack);
    assert!(i2c.rx_bytes().is_empty());
}

// ---------------------------------------------------------------------------------------------
// The CW2017 gauge on the corpus (T1)
// ---------------------------------------------------------------------------------------------

/// The line `bsp_battery_init` prints once it has read VERSION.
const CW2017_VERSION_LINE: &str = "bsp_batt: 检测到 CW2017 VERSION=0x0F";

/// A log line without its `I (<ms>) ` prefix.
fn untimed(line: &str) -> &str {
    let line = line.trim_end_matches('\r');
    match line.find(") ") {
        Some(at) if line.starts_with(['I', 'W', 'E']) && line[1..].starts_with(" (") => {
            &line[at + 2..]
        }
        _ => line,
    }
}

/// Boots one corpus image on a default machine until `needle` is printed on the USB Serial/JTAG
/// console or `budget` passes; the console and the stop time, or `None` after the SKIP line.
fn boot_until(
    test: &str,
    id: &str,
    file: &str,
    needle: &str,
    budget: VTime,
) -> Option<(String, VTime)> {
    use pemu_core::hostio::SerialStream;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::Machine;
    use pemu_machine::run::RunLimits;
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopSet};

    let path = common::corpus_file_or_skip(test, id, file)?;
    let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses as a merged image");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits the flash");
    m.run(RunLimits {
        until: Some(budget),
        max_insns: Some(4_000_000_000),
        stops: StopSet {
            matchers: vec![(
                MatcherId(1),
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Contains(needle.into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let now = m.now();
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    Some((String::from_utf8_lossy(&bytes).into_owned(), now))
}

/// A log line as its level letter followed by the untimed text, for example `Ibsp_batt: ...`.
fn leveled(line: &str) -> String {
    let level = line.chars().next().map_or_else(String::new, String::from);
    format!("{level}{}", untimed(line))
}

fn battery_lines(console: &str) -> Vec<String> {
    console
        .lines()
        .filter(|l| l.contains("bsp_batt:"))
        .map(leveled)
        .collect()
}

/// `pk` reads the gauge the device reads: `bsp_battery_init` prints `VERSION=0x0F` and takes the
/// provisioned fast path (no error line). With the derived device golden on this host, its
/// `bsp_batt:` lines are the expectation, so the profile line is compared without being copied
/// into the tree.
#[test]
fn t1_m5_v0_pk_reads_the_cw2017_the_device_reads() {
    let test = "t1_m5_v0_pk_reads_the_cw2017_the_device_reads";
    // The first line `pk` prints after the battery window.
    let Some((console, _)) = boot_until(
        test,
        common::PK,
        "FoloToy-AI-Passport-8MB.bin",
        "BLE_INIT",
        VTime::from_ms(2_000),
    ) else {
        return;
    };
    let got = battery_lines(&console);
    assert!(
        got.iter().any(|l| l == &format!("I{CW2017_VERSION_LINE}")),
        "`{CW2017_VERSION_LINE}` missing; battery lines {got:?}",
    );
    assert!(
        got.iter().all(|l| !l.starts_with('E')),
        "the provisioned gauge takes the fast path with no error: {got:?}",
    );
    assert_eq!(got.len(), 2, "VERSION and the profile line: {got:?}");

    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };
    let want: Vec<String> = golden
        .lines()
        .into_iter()
        .filter(|l| l.contains("bsp_batt:"))
        .map(leveled)
        .collect();
    assert_eq!(got, want, "the battery window equals the device golden's");
}

/// Lines of `goldens/pk.console.txt` the last-boot test claims: dev:L4-L67.
const LAST_BOOT_LINES: usize = 64;

/// `pk` as the device reference boot ran it: power on, run to the ROM's first `entry 0x`, apply
/// `UsbLine {rts: 1, dtr: 0}` there (the capture's RTS hard reset), then run to `until`.
/// `ble_disabled` leaves the BLE module unbound. `None` after the SKIP line.
fn pk_last_boot(
    test: &str,
    ble_disabled: bool,
    until: VTime,
) -> Option<(
    pemu_machine::machine::Machine,
    pemu_machine::stops::StopReason,
    String,
)> {
    use pemu_core::hostio::SerialStream;
    use pemu_core::input::InputEvent;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_loader::elf::ElfInfo;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::{At, Machine};
    use pemu_machine::run::RunLimits;
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};

    let bin = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport.elf")?;
    let flash = FlashImage::from_merged(&std::fs::read(bin).expect("the verified image"))
        .expect("a corpus image parses");
    let elf = ElfInfo::parse(&std::fs::read(elf).expect("the verified ELF")).expect("it parses");
    let assets = Assets::with_bundled_rom(
        flash,
        Some(std::sync::Arc::new(elf)),
        None,
        EfuseImage::synth(0),
    )
    .expect("the bundled ROM is pinned");
    let mut cfg = MachineConfig::default();
    if ble_disabled {
        cfg.hle.disabled = vec!["ble".to_owned()];
    }
    let mut m = Machine::new(cfg, assets).expect("the image fits");
    let entry = MatcherId(0xE0);
    let first = m.run(RunLimits {
        until: Some(VTime::from_ms(1_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                entry,
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("entry 0x".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    assert_eq!(
        first.reason,
        StopReason::Matcher(entry),
        "the ROM reaches `entry`"
    );
    m.input(
        At::Now,
        InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");
    let out = m.run(RunLimits {
        until: Some(until),
        max_insns: None,
        stops: StopSet::default(),
    });
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    let console = String::from_utf8_lossy(&bytes).into_owned();
    Some((m, out.reason, console))
}

/// "No assert": neither the `abort` nor the `__assert_func` observe hook fired
/// (`HleState::observed[1..3]`) and no `assert failed` line was printed.
fn assert_no_assert(m: &pemu_machine::machine::Machine, console: &str, what: &str) {
    assert_eq!(
        m.hle_state().observed[1..3],
        [0, 0],
        "{what}: abort or __assert_func ran"
    );
    assert!(!console.contains("assert failed"), "{what}: an assert line");
}

/// `pk`'s normalized last boot equals dev:L4-L67 of the derived device golden; with
/// `hle.disabled = [ble]` the run then stops with `E_TRIPWIRE` at `esp_bt_controller_init`; by
/// default the BLE module binds and the run continues past BLE init.
#[test]
fn t1_m5_pk_last_boot_and_ble_init() {
    use pemu_machine::hle::{HleFeatureStatus, HleTripKind};
    use pemu_machine::stops::StopReason;

    let test = "t1_m5_pk_last_boot_and_ble_init";
    let id = test.to_string();
    let Some((mut m, reason, console)) = pk_last_boot(test, true, VTime::from_ms(2_000)) else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };
    common::assert_console_prefix(
        "pk.console.txt",
        &golden,
        console.as_bytes(),
        Some(LAST_BOOT_LINES),
    );
    let StopReason::Tripwire(report) = &reason else {
        panic!(
            "{id}: with ble disabled pk ended {reason:?} at pc {:#x}",
            m.hart().pc
        );
    };
    let entry = m
        .assets()
        .app_elf
        .as_ref()
        .and_then(|elf| elf.symbols.addr_of("esp_bt_controller_init"))
        .expect("pk links esp_bt_controller_init");
    assert_eq!(
        report.kind,
        HleTripKind::DisabledFeature,
        "{id}: {report:?}"
    );
    assert_eq!(report.feature, Some("ble"), "{id}: {report:?}");
    assert_eq!(report.pc, entry, "{id}: {report:?}");
    assert_eq!(
        m.hle_binding().record.features.get("ble"),
        Some(&HleFeatureStatus::Disabled)
    );
    assert!(
        !console.contains("BLE_INIT"),
        "{id}: stopped before the controller"
    );
    assert_no_assert(&m, &console, &format!("{id}: ble disabled"));
    let again = m.run(pemu_machine::run::RunLimits::insns(1_000));
    assert_eq!(again.reason, reason, "{id}");
    println!("RAN {test} ble-disabled: dev:L4-L67 equal, E_TRIPWIRE ble at esp_bt_controller_init");

    let Some((m, reason, console)) = pk_last_boot(test, false, VTime::from_ms(2_000)) else {
        return;
    };
    common::assert_console_prefix(
        "pk.console.txt",
        &golden,
        console.as_bytes(),
        Some(LAST_BOOT_LINES),
    );
    // No tripwire, and no Stuck, Deadlock or guest fault either.
    assert_eq!(
        reason,
        StopReason::Until,
        "{id}: with the default configuration the run reaches its 2 s budget at pc {:#x}",
        m.hart().pc
    );
    assert_no_assert(&m, &console, &format!("{id}: ble default"));
    assert_eq!(
        m.hle_binding().record.features.get("ble"),
        Some(&HleFeatureStatus::Bound),
        "{id}: the ble module binds by default"
    );
    assert!(
        console.contains("BLE_INIT: BT controller compile version"),
        "{id}: the run continues past BLE init"
    );
    println!("RAN {test} ble-default: dev:L4-L67 equal, ble bound, BLE_INIT printed, {reason:?}");
}

/// `official` finishes `app_main` with every subsystem up within 2 s virtual, and `Battery=1`
/// depends on the gauge answering through I2C0.
#[test]
fn t1_m5_v0_official_reports_the_battery_up() {
    let test = "t1_m5_v0_official_reports_the_battery_up";
    let ready = "main: 就绪:";
    let Some((console, now)) = boot_until(
        test,
        common::OFFICIAL,
        "FoloToy-AI-Passport-8MB.bin",
        ready,
        VTime::from_ms(2_000),
    ) else {
        return;
    };
    let lines: Vec<&str> = console.lines().map(untimed).collect();
    assert!(
        lines.contains(&CW2017_VERSION_LINE),
        "`{CW2017_VERSION_LINE}` missing; battery lines {:?}",
        battery_lines(&console),
    );
    let line = lines
        .iter()
        .find(|l| l.starts_with(ready))
        .unwrap_or_else(|| {
            panic!(
                "`{ready}` not printed within 2 s virtual (stopped at {now:?}); tail {:?}",
                &lines[lines.len().saturating_sub(6)..]
            )
        });
    assert_eq!(*line, "main: 就绪:Display=1 Button=1 Audio=1 Battery=1");
    assert!(now <= VTime::from_ms(2_000));
}

/// `goldminer` (the sanitized factory image) reads the same gauge and reports `battery=ESP_OK`.
#[test]
fn t1_m5_v0_goldminer_reports_the_battery_up() {
    let test = "t1_m5_v0_goldminer_reports_the_battery_up";
    let ready = "main: ready:";
    let Some((console, now)) = boot_until(
        test,
        common::GOLDMINER,
        "goldminer-sanitized-8MB.bin",
        ready,
        VTime::from_ms(2_000),
    ) else {
        return;
    };
    let lines: Vec<&str> = console.lines().map(untimed).collect();
    assert!(
        lines.contains(&CW2017_VERSION_LINE),
        "`{CW2017_VERSION_LINE}` missing; battery lines {:?}",
        battery_lines(&console),
    );
    let line = lines
        .iter()
        .find(|l| l.starts_with(ready))
        .unwrap_or_else(|| panic!("`{ready}` not printed (stopped at {now:?})"));
    assert!(line.ends_with("battery=ESP_OK"), "{line}");
}

// ---------------------------------------------------------------------------------------------
// The command hooks on a real `official` instance
// ---------------------------------------------------------------------------------------------

static HOOKED: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Installs the host's real hooks over the corpus `official` image and app ELF, with a boot cache
/// in memory and artifacts nowhere, or `None` after the SKIP line.
fn hooked_official(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
    use std::sync::Arc;
    let bin = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")?;
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
            common::OFFICIAL => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(std::env::temp_dir().join("pemu-m5-no-audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == common::OFFICIAL).then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::new(Some(workspace()), vec![workspace()]),
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

fn settled_official() -> String {
    let out = call(
        "start",
        serde_json::json!({"fw": common::OFFICIAL, "boot_cache": "ui-settled"}),
    )
    .expect("official starts");
    assert_eq!(out.json["boot"]["status"], "matched", "{}", out.json);
    out.json["instance"].as_str().expect("an id").to_owned()
}

fn stop(id: &str) {
    call("stop", serde_json::json!({"instance": id})).expect("the instance stops");
}

/// What one run of the menu scenario left behind, for the ten-run comparison.
#[derive(Debug, PartialEq, Eq)]
struct SmokeRun {
    steps: Vec<(String, String, u64)>,
    tree: String,
    state_hash: [u8; 32],
    frame_gen: u64,
    artifact_sha256: [u8; 32],
}

/// The settled `official` menu as `tests/golden/official/menu.png` holds it, decoded to RGB888.
fn menu_golden_rgb() -> Vec<u8> {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden/official/menu.png");
    let png = std::fs::read(&path).expect("the approved menu golden is committed");
    let (w, h, rgb) = pemu_host::png::decode_rgb888(&png).expect("the golden is a PNG");
    assert_eq!((w, h), (240, 320), "the golden is a panel frame");
    rgb
}

/// Asserts that `rgb` (RGB888, 240x320) is the settled menu: the golden pixel for pixel, and the
/// card geometry and colours spot-checked on their own, so a wrong golden could not pass.
fn assert_menu_pixels(test: &str, via: &str, rgb: &[u8]) {
    let golden = menu_golden_rgb();
    assert_eq!(rgb.len(), golden.len(), "{via}: a 240x320 frame");
    let differing = rgb
        .chunks(3)
        .zip(golden.chunks(3))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "{via}: {differing} pixel(s) differ from tests/golden/official/menu.png"
    );
    let px = |x: usize, y: usize| -> [u8; 3] {
        let at = (y * 240 + x) * 3;
        [rgb[at], rgb[at + 1], rgb[at + 2]]
    };
    let c = pemu_api::commands::screenshot::rgb565_to_rgb888;
    const SKY: u16 = 0x145D;
    const INK: u16 = 0x1105;
    const PAPER: u16 = 0xF7BD;
    const YELLOW: u16 = 0xFEC5;
    const WHITE: u16 = 0xFFFF;
    assert_eq!(px(0, 45), c(SKY), "{via}: screen background");
    assert_eq!(
        px(11, 52),
        c(WHITE),
        "{via}: the selected Display card border"
    );
    assert_eq!(
        px(10, 52),
        c(SKY),
        "{via}: the Display border starts at x = 11"
    );
    assert_eq!(
        px(15, 56),
        c(YELLOW),
        "{via}: the selected Display card inner"
    );
    for (name, x, y) in [
        ("Button", 123, 52),
        ("Wi-Fi", 11, 146),
        ("Low Power", 11, 193),
    ] {
        assert_eq!(px(x, y), c(INK), "{via}: {name} border corner");
        assert_eq!(px(x + 4, y + 4), c(PAPER), "{via}: {name} inner corner");
    }
    println!("RAN {test} {via}-screenshot-pixels: equal to tests/golden/official/menu.png");
}

/// `screenshot raw` of instance `id` in this process, with an artifact directory bound under
/// `artifacts` as the daemon binds one: the PNG bytes and the answer.
fn shot_in_process(id: &str, artifacts: &std::path::Path) -> (Vec<u8>, serde_json::Value) {
    let dir = pemu_host::artifacts::ArtifactDir::create(artifacts, "run-menu-smoke", id)
        .expect("an artifact directory");
    let root = artifacts.join("run-menu-smoke").join(id);
    let out = pemu_host::artifacts::bind_current(
        Some(std::sync::Arc::new(std::sync::Mutex::new(dir))),
        || {
            call(
                "screenshot",
                serde_json::json!({"instance": id, "view": "raw"}),
            )
        },
    )
    .expect("the screenshot is taken");
    let path = out.json["path"].as_str().expect("a path");
    let png = std::fs::read(root.join(path)).expect("the screenshot is written");
    (png, out.json)
}

/// The `int` static `name` of `official`'s `main.c` in instance `inst`, read from guest memory at
/// its symbol address (`s_sel` and `s_active`).
fn menu_variable(inst: &str, name: &str) -> i32 {
    use pemu_loader::symbols::SymKind;
    let context = pemu_host::hooks::elf_of(common::OFFICIAL).expect("the official ELF is hooked");
    let symbols: Vec<_> = context
        .elf
        .symbols
        .named(name)
        .filter(|s| s.is_defined() && s.kind == SymKind::Object)
        .collect();
    assert_eq!(symbols.len(), 1, "one object named `{name}`: {symbols:?}");
    assert_eq!(symbols[0].size, 4, "`{name}` is an int");
    let addr = symbols[0].addr;
    let parsed = pemu_api::instance::InstanceId::parse(inst).expect("an id");
    let word = pemu_api::commands::start::with_pool(|pool| {
        let session = pool.session_mut(parsed).expect("the session");
        session.machine().guest_mem().load(addr, 4)
    })
    .unwrap_or_else(|| panic!("`{name}` at {addr:#x} is readable guest memory"));
    word as i32
}

/// On a settled menu, `click DOWN` gives `s_sel == 1`, `click OK` opens the Button page
/// (`s_active == 1`), and a long OK returns to the menu (`s_active == -1`) with `s_sel == 1`.
fn menu_variables_follow_the_presses(test: &str) {
    let inst = settled_official();
    let vars = |inst: &str| {
        (
            menu_variable(inst, "s_sel"),
            menu_variable(inst, "s_active"),
        )
    };
    assert_eq!(vars(&inst), (0, -1), "the settled menu selects Display");
    let press = |button: &str, action: &str| {
        call(
            "input",
            serde_json::json!({"instance": inst, "button": button, "action": action}),
        )
        .expect("the press is applied");
    };
    press("down", "click");
    assert_eq!(vars(&inst).0, 1, "click DOWN gives s_sel == 1");
    press("ok", "click");
    assert_eq!(vars(&inst), (1, 1), "click OK opens the Button page");
    press("ok", "hold");
    assert_eq!(
        vars(&inst),
        (1, -1),
        "long OK returns to the menu with s_sel == 1"
    );
    stop(&inst);
    println!(
        "RAN {test} s_sel-s_active: DOWN s_sel 1, OK s_active 1, long OK s_active -1 with s_sel 1"
    );
}

/// `tests/scenarios/official-menu-smoke.yaml` passes ten runs out of ten with identical results:
/// every step and its virtual time, the final tree, and the state hash. Each run starts from the
/// golden menu and its screenshot hashes the same ten times.
#[test]
fn t1_m5_official_menu_smoke() {
    let test = "t1_m5_official_menu_smoke";
    let Some(_hooks) = hooked_official(test) else {
        return;
    };
    let mut runs: Vec<SmokeRun> = Vec::new();
    let artifacts = temp("menu-smoke");
    for run in 0..10 {
        let id = settled_official();
        if run == 0 {
            let (png, _) = shot_in_process(&id, &artifacts.join("settled"));
            let (_, _, rgb) = pemu_host::png::decode_rgb888(&png).expect("a PNG");
            assert_menu_pixels(test, "settled-menu", &rgb);
        }
        let out = call(
            "scenario",
            serde_json::json!({
                "file": "tests/scenarios/official-menu-smoke.yaml",
                "instance": id,
                "strict": false,
            }),
        )
        .expect("the scenario runs");
        let report = &out.json["scenarios"][0];
        // A lenient run that reached class U hardware passes as `pass_with_caveats`; the step statuses
        // below are the check, and `fail` is never accepted.
        let status = report["status"].as_str().unwrap_or_default();
        assert!(
            status == "pass" || status == "pass_with_caveats",
            "run {run}: {status}\n{}\n{}",
            out.text,
            out.json
        );
        let steps = report["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .map(|s| {
                (
                    s["key"].as_str().unwrap_or_default().to_owned(),
                    s["status"].as_str().unwrap_or_default().to_owned(),
                    s["vt_us"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        let tree = call("ui", serde_json::json!({"instance": id}))
            .expect("the tree reads")
            .json["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let parsed = pemu_api::instance::InstanceId::parse(&id).expect("an id");
        let (png, shot) = shot_in_process(&id, &artifacts.join(format!("run{run}")));
        let artifact_sha256 = pemu_loader::sha256(&png);
        assert_eq!(
            shot["sha256"].as_str(),
            Some(pemu_loader::hex(&artifact_sha256).as_str())
        );
        let frame_gen = shot["frame_gen"].as_u64().expect("a frame_gen");
        let state_hash = pemu_api::commands::start::with_pool(|pool| {
            let session = pool.session_mut(parsed).expect("the session");
            session.snapshot_machine().state_hash()
        });
        runs.push(SmokeRun {
            steps,
            tree,
            state_hash,
            frame_gen,
            artifact_sha256,
        });
        stop(&id);
    }
    println!(
        "RAN {test} official: 10 runs, {} steps each",
        runs[0].steps.len()
    );

    // Not vacuous: on the settled menu, Button is not the selected card.
    let id = settled_official();
    let wrong = call(
        "scenario",
        serde_json::json!({
            "inline": "schema: passportsim/scenario@1\nname: wrong\nsteps:\n  - ui.expect: {contains: [{text: Button, within: {bg: \"#ffd928\"}}]}\n",
            "instance": id,
        }),
    )
    .expect("the scenario runs");
    assert_eq!(
        wrong.json["scenarios"][0]["status"], "fail",
        "{}",
        wrong.text
    );
    stop(&id);
    for (i, run) in runs.iter().enumerate().skip(1) {
        assert_eq!(run, &runs[0], "run {i} differs from run 0");
    }
    menu_variables_follow_the_presses(test);
    assert!(runs[0].frame_gen > 0, "the panel was drawn");
    println!(
        "RAN {test} artifact-hashes: 10 equal screenshot artifacts, sha256 {}, frame_gen {}",
        pemu_loader::hex(&runs[0].artifact_sha256),
        runs[0].frame_gen
    );
    std::fs::remove_dir_all(&artifacts).ok();
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pemu-m5-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

/// Checks what `inspect`, `ui` and `screenshot` answered on the settled `official` menu, however
/// the call travelled. `screen` is the PNG the screenshot wrote.
fn check_answers(
    test: &str,
    via: &str,
    ui: &serde_json::Value,
    inspect: &serde_json::Value,
    shot: &serde_json::Value,
    screen: &[u8],
) {
    // The official menu is 63 LVGL objects; the tree names its cards.
    assert_eq!(ui["counts"]["objects"], 63, "{via} ui: {ui}");
    let text = ui["text"].as_str().expect("the rendered tree");
    for label in ["\"FoloToy\"", "\"Display\"", "\"Button\"", "\"Low Power\""] {
        assert!(text.contains(label), "{via} ui lacks {label}: {text}");
    }
    assert_eq!(inspect["lvgl"]["objects"], 63, "{via} inspect: {inspect}");
    assert!(
        inspect["tasks"]["count"].as_u64().unwrap_or(0) >= 2,
        "{via} inspect tasks: {inspect}"
    );
    let names: Vec<&str> = inspect["tasks"]["tasks"]
        .as_array()
        .expect("a task list")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"IDLE"),
        "{via}: the idle task is walked: {names:?}"
    );
    assert!(
        inspect["heap"]["total_free_bytes"].as_u64().unwrap_or(0) > 0,
        "{via} inspect heap: {inspect}"
    );
    assert_eq!(
        (shot["w"].as_u64(), shot["h"].as_u64()),
        (Some(240), Some(320)),
        "{via}: {shot}"
    );
    assert_eq!(
        shot["sha256"].as_str(),
        Some(pemu_loader::hex(&pemu_loader::sha256(screen)).as_str()),
        "{via}: the reported hash is the written file's"
    );
    let (w, h, rgb) = pemu_host::png::decode_rgb888(screen).expect("the artifact is a PNG");
    assert_eq!((w, h, rgb.len()), (240, 320, 240 * 320 * 3), "{via}");
    println!("RAN {test} {via}");
    assert!(shot["frame_gen"].as_u64().unwrap_or(0) > 0, "{via}: {shot}");
    assert_menu_pixels(test, via, &rgb);
}

/// `inspect`, `ui`, `screenshot` and `scenario` answer on a real `official` instance through the
/// daemon's HTTP routes.
#[test]
fn t1_m5_official_hooks_answer_through_the_daemon() {
    use pemu_host::auth::{Auth, Token};
    use pemu_host::daemon::{self, Shutdown};
    let test = "t1_m5_official_hooks_answer_through_the_daemon";
    let Some(_hooks) = hooked_official(test) else {
        return;
    };
    let artifacts = temp("daemon");
    let token = Token::from_bytes([0x5A; 32]);
    let bound = daemon::bind(0).expect("port 0 always binds");
    let port = bound.port;
    let server = std::sync::Arc::new(
        pemu_host::http::Server::new(
            Auth::new(token.clone(), port),
            std::sync::Arc::new(pemu_host::pool::Pool::new(4)),
            Shutdown::new(),
            std::collections::BTreeSet::from([pemu_api::spec::CapsGroup::Core]),
        )
        .with_artifacts(&artifacts),
    );
    let served = std::sync::Arc::clone(&server);
    let listener = bound.listener;
    std::thread::spawn(move || pemu_host::http::serve_blocking(listener, served));
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let http = |method: &str, path: &str, body: serde_json::Value| -> serde_json::Value {
        let response = daemon::request_with_timeout(
            addr,
            method,
            path,
            &token,
            Some(body.to_string().as_bytes()),
            std::time::Duration::from_secs(1_800),
        )
        .expect("the daemon answers");
        serde_json::from_slice(&response.body).expect("a JSON envelope")
    };

    let start = http(
        "POST",
        "/v1/instances",
        serde_json::json!({"fw": common::OFFICIAL, "boot_cache": "ui-settled"}),
    );
    assert_eq!(start["ok"], true, "{start}");
    let id = start["result"]["instance"]
        .as_str()
        .expect("an id")
        .to_owned();
    let command = |name: &str, body: serde_json::Value| {
        let answer = http("POST", &format!("/v1/instances/{id}/commands/{name}"), body);
        assert_eq!(answer["ok"], true, "{name}: {answer}");
        answer["result"].clone()
    };
    let ui = command("ui", serde_json::json!({}));
    let inspect = command(
        "inspect",
        serde_json::json!({"what": ["tasks", "heap", "lvgl"]}),
    );
    let shot = command(
        "screenshot",
        serde_json::json!({"view": "raw", "save_as": "menu"}),
    );
    assert_eq!(
        shot["path"], "screens/menu.png",
        "relative, forward-slashed"
    );
    let file = artifacts
        .join(server.run_id())
        .join(&id)
        .join("screens/menu.png");
    let screen = std::fs::read(&file).expect("the screenshot is in the instance's artifacts");
    check_answers(test, "daemon", &ui, &inspect, &shot, &screen);

    let same = command(
        "screenshot",
        serde_json::json!({"view": "raw", "save_as": "again", "compare_with": "screens/menu.png"}),
    );
    assert_eq!(same["compare"]["equal"], true, "{same}");

    let export = command(
        "snapshot",
        serde_json::json!({"op": "export", "name": "menu"}),
    );
    assert_eq!(export["redacted"], true, "{export}");
    // A daemon machine's cardid window is erased (a secret-bearing image is refused), so the export
    // records no `Redacted{...}` label.
    let labels = export["redacted_labels"]
        .as_array()
        .expect("`redacted_labels` is an array");
    assert!(
        labels.is_empty(),
        "an erased cardid window is not labelled: {export}"
    );
    let exported = artifacts
        .join(server.run_id())
        .join(&id)
        .join(export["path"].as_str().expect("a relative path"));
    let bytes = std::fs::read(&exported).expect("the export is in the instance's artifacts");
    assert_eq!(
        bytes.len() as u64,
        export["size_bytes"].as_u64().expect("a size")
    );
    let import = command(
        "snapshot",
        serde_json::json!({"op": "import", "name": "menu"}),
    );
    assert_eq!(import["op"], "import", "{import}");
    println!("RAN {test} daemon-snapshot-export-import");

    // `inspect nvs` is the one section left unanswered, and it says which structure it needs.
    let nvs = http(
        "POST",
        &format!("/v1/instances/{id}/commands/inspect"),
        serde_json::json!({"what": ["nvs"]}),
    );
    assert_eq!(nvs["ok"], false, "{nvs}");
    assert_eq!(nvs["error"]["code"], "E_STATE", "{nvs}");

    let scenario = workspace().join("tests/scenarios/official-menu-smoke.yaml");
    let run = command(
        "scenario",
        serde_json::json!({"file": scenario.to_str().expect("UTF-8"), "junit": "junit.xml"}),
    );
    assert_eq!(run["result"], "pass", "{run}");
    assert!(
        artifacts
            .join(server.run_id())
            .join(&id)
            .join("junit.xml")
            .is_file(),
        "the JUnit report is written into the instance's artifacts"
    );
    let stop = http(
        "DELETE",
        &format!("/v1/instances/{id}"),
        serde_json::json!({}),
    );
    assert_eq!(stop["ok"], true, "{stop}");
    server.shutdown().request();
    std::fs::remove_dir_all(&artifacts).ok();
}

/// A private `PASSPORTSIM_HOME` whose corpus map names `official`, with a daemon of its own that
/// is stopped however the test ends.
struct OfficialHome {
    bin: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl OfficialHome {
    fn new(bin_file: &std::path::Path, elf_file: &std::path::Path) -> OfficialHome {
        let home = temp("cli-home");
        let config = home.join("config");
        std::fs::create_dir_all(&config).expect("a config role");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for dir in [&home, &config] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .expect("owner-only");
            }
        }
        let (bin_file, elf_file) = (
            bin_file.to_str().expect("a UTF-8 corpus path"),
            elf_file.to_str().expect("a UTF-8 corpus path"),
        );
        std::fs::write(
            config.join("corpus.toml"),
            format!(
                "[{}]\nbin = \"{bin_file}\"\nelf = \"{elf_file}\"\n",
                common::OFFICIAL
            ),
        )
        .expect("the corpus map");
        OfficialHome {
            bin: passportsim(),
            home,
        }
    }

    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let mut command = std::process::Command::new(&self.bin);
        command
            .args(args)
            .current_dir(workspace())
            .env("PASSPORTSIM_HOME", &self.home)
            .stdin(std::process::Stdio::null());
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| {
                key == "PASSPORTSIM_DATA_ROOT"
                    || key == "PASSPORTSIM_CONFIG_DIR"
                    || key.starts_with("PASSPORTSIM_CORPUS_")
            }) {
                command.env_remove(&key);
            }
        }
        let out = command.output().expect("passportsim runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8(out.stdout).expect("UTF-8 stdout"),
            String::from_utf8(out.stderr).expect("UTF-8 stderr"),
        )
    }

    /// One `--output json` call that succeeds: 0, or the 10 PASS_WITH_CAVEATS of a lenient run.
    #[track_caller]
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut argv = args.to_vec();
        argv.extend_from_slice(&["--output", "json"]);
        let (code, stdout, stderr) = self.cli(&argv);
        common::assert_cli_exit(code, 0, &stdout, &format!("passportsim {argv:?}: {stderr}"));
        serde_json::from_str(stdout.trim_end())
            .unwrap_or_else(|e| panic!("passportsim {argv:?} printed no JSON: {e}: {stdout}"))
    }
}

impl Drop for OfficialHome {
    fn drop(&mut self) {
        let _ = self.cli(&["serve", "--stop"]);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The same through the `passportsim` binary and the daemon it spawns, and `start --boot-cache`
/// hits the on-disk entry a previous daemon wrote.
#[test]
fn t1_m5_official_hooks_answer_through_the_cli() {
    let test = "t1_m5_official_hooks_answer_through_the_cli";
    let Some(bin) =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let Some(elf) = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")
    else {
        return;
    };
    let home = OfficialHome::new(&bin, &elf);
    let start = home.json(&["start", common::OFFICIAL, "--boot-cache", "ui-settled"]);
    assert_eq!(start["boot"]["status"], "matched", "{start}");
    assert_eq!(start["boot"]["cache"]["hit"], false, "{start}");
    assert_eq!(start["boot"]["cache"]["store"], "disk", "{start}");
    let id = start["instance"].as_str().expect("an id").to_owned();

    let ui = home.json(&["ui"]);
    let what = home.home.join("what.json");
    std::fs::write(&what, r#"{"what":["tasks","heap","lvgl"]}"#).expect("the argument file");
    let inspect = home.json(&[
        "inspect",
        "--json",
        &format!("@{}", what.to_str().expect("UTF-8")),
    ]);
    let shot = home.json(&["screenshot", "raw", "--save-as", "menu"]);
    let artifacts = home.home.join("data").join("artifacts");
    let screen = std::fs::read_dir(&artifacts)
        .expect("the daemon's artifacts root")
        .filter_map(Result::ok)
        .map(|run| run.path().join(&id).join("screens/menu.png"))
        .find(|path| path.is_file())
        .expect("the screenshot is under the instance's artifacts");
    let screen = std::fs::read(screen).expect("readable");
    check_answers(test, "cli", &ui, &inspect, &shot, &screen);

    // A relative pattern typed in the workspace reaches the daemon absolute.
    let run = home.json(&["scenario", "tests/scenarios/official-menu-*.yaml"]);
    assert_eq!(run["result"], "pass", "{run}");
    home.json(&["stop", &id]);

    // A new daemon process reads the entry the first one wrote.
    let (code, out, err) = home.cli(&["serve", "--stop"]);
    assert_eq!(code, 0, "{out}{err}");
    let again = home.json(&["start", common::OFFICIAL, "--boot-cache", "ui-settled"]);
    assert_eq!(again["boot"]["cache"]["hit"], true, "{again}");
    assert_eq!(again["boot"]["cache"]["key"], start["boot"]["cache"]["key"]);
    println!("RAN {test} cli-boot-cache-across-daemons");
}

/// The settled `official` menu `raw` frame equals `tests/golden/official/menu.png`, its card
/// geometry matches `specs/notes/g3-behavior.md` g3-menu-geometry, and its colours are the LVGL
/// style colours in RGB565 with no inversion (g3-menu-colours-invon: screen 0x145D, selected
/// 0xFEC5). Taken at 3 s with no key presses, on both executors. Audio and Battery are enabled
/// cards, since the ES8311 and the CW2017 answer here. Without a committed golden a `SKIP` line
/// names the candidate (written only with `PEMU_WRITE_CANDIDATES=1`).
#[test]
fn t1_m5_official_menu_frame() {
    use pemu_board::st7789::{FrameView, PANEL_HEIGHT, PANEL_WIDTH};
    use pemu_machine::run::RunLimits;

    let test = "t1_m5_official_menu_frame";
    let id = test.to_string();
    let Some(path) =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let Some(elf_path) =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")
    else {
        return;
    };
    let image = std::fs::read(&path).expect("the verified corpus file is readable");
    let elf = std::sync::Arc::new(
        pemu_loader::elf::ElfInfo::parse(&std::fs::read(&elf_path).expect("readable"))
            .expect("the pinned ELF parses"),
    );
    let mut frames = Vec::new();
    for executor in [
        pemu_machine::Executor::Engine,
        pemu_machine::Executor::Reference,
    ] {
        let flash = pemu_loader::bundle::FlashImage::from_merged(&image).expect("parses");
        let assets = pemu_machine::config::Assets::with_bundled_rom(
            flash,
            Some(elf.clone()),
            None,
            pemu_loader::efuse_image::EfuseImage::synth(0),
        )
        .expect("the bundled ROM is pinned");
        let mut m = pemu_machine::machine::Machine::new(
            pemu_machine::config::MachineConfig::default(),
            assets,
        )
        .expect("the image fits");
        m.set_executor(executor);
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(3_000)),
            max_insns: None,
            stops: pemu_machine::stops::StopSet::default(),
        });
        assert_eq!(
            out.reason,
            pemu_machine::stops::StopReason::Until,
            "{id} {executor:?}: the menu boot ended early"
        );
        let raw = m.board().lcd.frame(FrameView::Raw);
        assert_eq!(
            m.io().frame.pixels(),
            &raw[..],
            "{id}: FramePort is panel memory"
        );
        frames.push(raw);
    }
    assert_eq!(
        frames[0], frames[1],
        "{id}: the engine and the reference differ"
    );
    let raw = &frames[0];
    let w = usize::from(PANEL_WIDTH);
    let px = |x: usize, y: usize| raw[y * w + x];
    // Every pixel of the rectangle (x, y, width, height) is `colour`.
    let fill = |x: usize, y: usize, cw: usize, ch: usize, colour: u16, what: &str| {
        for yy in y..y + ch {
            for xx in x..x + cw {
                assert_eq!(
                    px(xx, yy),
                    colour,
                    "{id}: {what} at ({xx}, {yy}) is {:#06x}, not {colour:#06x}",
                    px(xx, yy)
                );
            }
        }
    };
    const SKY: u16 = 0x145D;
    const INK: u16 = 0x1105;
    const PAPER: u16 = 0xF7BD;
    const YELLOW: u16 = 0xFEC5;
    const WHITE: u16 = 0xFFFF;
    assert_eq!(px(0, 45), SKY, "{id}: screen background");
    assert!(raw.iter().filter(|p| **p == SKY).count() > 20_000, "{id}");
    // Title plate (5, 8, 151, 33), inner paper (8, 11, 145, 27): its corners and edges.
    assert_eq!(px(8, 11), PAPER, "{id}: title plate inner");
    assert_eq!(px(5, 8), INK, "{id}: title plate border");
    // Display card, selected: white border exactly (11, 52, 102, 40), yellow inner (15, 56, 94, 32).
    fill(11, 52, 102, 4, WHITE, "Display border top");
    fill(11, 88, 102, 4, WHITE, "Display border bottom");
    fill(11, 52, 4, 40, WHITE, "Display border left");
    fill(109, 52, 4, 40, WHITE, "Display border right");
    assert_eq!(px(10, 52), SKY, "{id}: the Display border starts at x = 11");
    for (x, y) in [(15, 56), (108, 56), (15, 87), (108, 87)] {
        assert_eq!(px(x, y), YELLOW, "{id}: Display inner corner ({x}, {y})");
    }
    // The other cards: x = 11 + 112 per column, y = 52 + 47 per row, 102x40, ink border and an inner
    // fill whose colour is the card's state.
    let cards = [
        ("Button", 123, 52, PAPER),
        ("Audio", 11, 99, PAPER),
        ("Battery", 123, 99, PAPER),
        ("Wi-Fi", 11, 146, PAPER),
        ("BLE", 123, 146, PAPER),
        ("Low Power", 11, 193, PAPER),
    ];
    for (name, x, y, inner) in cards {
        assert_eq!(px(x, y), INK, "{id}: {name} border corner ({x}, {y})");
        assert_eq!(px(x + 101, y + 39), INK, "{id}: {name} far border corner");
        assert_eq!(px(x - 1, y), SKY, "{id}: {name} starts at x = {x}");
        for (dx, dy) in [(4, 4), (97, 4), (4, 35), (97, 35)] {
            assert_eq!(
                px(x + dx, y + dy),
                inner,
                "{id}: {name} inner corner ({}, {})",
                x + dx,
                y + dy
            );
        }
    }
    let png =
        pemu_host::png::encode_rgb565(u32::from(PANEL_WIDTH), u32::from(PANEL_HEIGHT), raw, 1)
            .expect("a panel frame encodes");
    let committed =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden/official/menu.png");
    if let Ok(golden) = std::fs::read(&committed) {
        assert!(
            golden == png,
            "{id}: the menu frame differs from its golden"
        );
        return;
    }
    let root = path
        .ancestors()
        .nth(3)
        .expect("a corpus file sits under the data root");
    let candidate = "scratch/candidates/official/menu.png";
    let written = if std::env::var("PEMU_WRITE_CANDIDATES").as_deref() == Ok("1") {
        let file = root.join(candidate);
        std::fs::create_dir_all(file.parent().expect("a file has a parent"))
            .expect("the data root is writable");
        std::fs::write(&file, &png).expect("the candidate is written");
        "written"
    } else {
        "set PEMU_WRITE_CANDIDATES=1 to write it"
    };
    common::skip(
        test,
        &format!(
            "{id} golden tests/golden/official/menu.png is not committed and needs a person's \
             approval; candidate {candidate} under the data root ({written}), sha256 {}",
            pemu_testkit::corpus::sha256_hex(&png)
        ),
    );
}

// ---------------------------------------------------------------------------------------------
// Machine-level checks on the corpus `official` image
// ---------------------------------------------------------------------------------------------

const I2C0_BASE: u32 = 0x6001_3000;
const SARADC_BASE: u32 = 0x6004_0000;
const I2S0_BASE: u32 = 0x6002_D000;
const WINDOW: u32 = 0x1000;

/// Virtual time one traced slice covers; [`Traced::drain`] fails rather than lose a record.
const TRACE_SLICE: VTime = VTime::from_ms(2);
const TRACE_WINDOW: usize = 1 << 18;

/// A corpus `official` machine recording its MMIO reads and writes, and the drained records of the
/// three M5 blocks, oldest first.
struct Traced {
    m: pemu_machine::machine::Machine,
    cursor: u64,
    records: Vec<pemu_core::trace::TraceRecord>,
}

fn m5_block(addr: u32) -> bool {
    [I2C0_BASE, SARADC_BASE, I2S0_BASE]
        .iter()
        .any(|base| addr.wrapping_sub(*base) < WINDOW)
}

impl Traced {
    /// The default machine over `official` with the MMIO trace on and `disabled` models switched off;
    /// `None` after the SKIP line.
    fn official(test: &str, trace: bool) -> Option<Traced> {
        use pemu_core::trace::TraceKinds;
        use pemu_loader::bundle::FlashImage;
        use pemu_loader::efuse_image::EfuseImage;
        use pemu_machine::config::{Assets, MachineConfig, TraceCfg};
        use pemu_machine::machine::Machine;

        let path =
            common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
        let bytes = std::fs::read(&path).expect("the verified corpus file is readable");
        let flash = FlashImage::from_merged(&bytes).expect("a corpus image parses");
        let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
            .expect("the bundled ROM ELF is pinned by assets/rom/pins.toml");
        let mut cfg = MachineConfig::default();
        if trace {
            cfg.trace = TraceCfg {
                kinds: Some(TraceKinds::MMIO_READ.union(TraceKinds::MMIO_WRITE)),
                recent: TRACE_WINDOW,
            };
        }
        let m = Machine::new(cfg, assets).expect("the image fits the flash");
        Some(Traced {
            m,
            cursor: 0,
            records: Vec::new(),
        })
    }

    /// Moves the records the last run closed into [`Traced::records`], keeping the M5 blocks.
    fn drain(&mut self) {
        let trace = self.m.trace();
        assert!(
            trace.tail() <= self.cursor,
            "the trace window lost records {}..{}; shorten TRACE_SLICE",
            self.cursor,
            trace.tail()
        );
        let skip = usize::try_from(self.cursor - trace.tail()).expect("a window index");
        self.records.extend(trace.records().skip(skip).filter(|r| {
            use pemu_core::trace::TraceEvent::{MmioRead, MmioWrite, PollRun};
            matches!(r.ev, MmioRead { addr, .. } | MmioWrite { addr, .. } | PollRun { addr, .. }
                if m5_block(addr))
        }));
        self.cursor = trace.head();
    }

    /// Runs in [`TRACE_SLICE`] pieces until a console line contains `needle` or `until` is reached.
    /// Returns whether the line was printed; any other stop fails.
    fn run_until(&mut self, needle: Option<&str>, until: VTime) -> bool {
        use pemu_core::hostio::SerialStream;
        use pemu_machine::run::RunLimits;
        use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};

        let stops = StopSet {
            matchers: needle
                .map(|n| {
                    vec![(
                        MatcherId(1),
                        Matcher::Serial {
                            stream: SerialStream::UsjTx,
                            pattern: LinePattern::Contains(n.into()),
                        },
                    )]
                })
                .unwrap_or_default(),
            ..StopSet::default()
        };
        while self.m.now() < until {
            let slice = VTime((self.m.now().0 + TRACE_SLICE.0).min(until.0));
            let out = self.m.run(RunLimits {
                until: Some(slice),
                max_insns: None,
                stops: stops.clone(),
            });
            self.drain();
            match out.reason {
                StopReason::Until => {}
                StopReason::Matcher(_) => return true,
                other => panic!(
                    "the run stopped at {:?} on {other:?}, pc {:#x}",
                    self.m.now(),
                    self.m.hart().pc
                ),
            }
        }
        false
    }

    fn run_to_line(&mut self, needle: &str, until: VTime) -> VTime {
        assert!(
            self.run_until(Some(needle), until),
            "`{needle}` not printed by {until:?} (stopped at {:?})",
            self.m.now()
        );
        self.m.now()
    }

    /// `input <button> click` as `passportsim input` expands it: pressed for `CLICK_MS`, released,
    /// then `THEN_RUN_MS` more. Returns the indices into [`Traced::records`] where the press
    /// and the release took effect.
    fn click(&mut self, id: ButtonId) -> (usize, usize) {
        use pemu_api::commands::input::{CLICK_MS, THEN_RUN_MS};
        use pemu_core::input::InputEvent;
        use pemu_machine::machine::At;

        let press = self.m.now();
        let release = VTime(press.0 + VTime::from_ms(CLICK_MS).0);
        self.m
            .input(At::Vt(press), InputEvent::Button { id, down: true })
            .expect("a press now is accepted");
        self.m
            .input(At::Vt(release), InputEvent::Button { id, down: false })
            .expect("a later release is accepted");
        let pressed = self.records.len();
        self.run_until(None, release);
        let released = self.records.len();
        self.run_until(None, VTime(release.0 + VTime::from_ms(THEN_RUN_MS).0));
        (pressed, released)
    }

    fn console(&mut self) -> String {
        use pemu_core::hostio::SerialStream;
        let ring = self.m.io().serial_ring(SerialStream::UsjTx);
        let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// One I2C0 transaction as the trace shows it: the TX FIFO bytes before `CTR.trans_start`, the
/// first `INT_ST` read afterwards, and the bytes read out of the RX FIFO.
#[derive(Debug, Default)]
struct BusTxn {
    tx: Vec<u8>,
    int_st: Option<u32>,
    rx: Vec<u8>,
}

impl BusTxn {
    /// Whether the transaction ended complete and acknowledged.
    fn acked(&self) -> bool {
        self.int_st
            .is_some_and(|st| st & i2c0::INT_TRANS_COMPLETE != 0 && st & i2c0::INT_NACK == 0)
    }

    /// The transaction as the transcript op it is (`i2c_master_probe`, `transmit` or
    /// `transmit_receive`), or `None` for any other shape.
    fn op(&self) -> Option<TxOp> {
        let (&first, rest) = self.tx.split_first()?;
        if first & 1 != 0 {
            return None;
        }
        let addr = first >> 1;
        match rest {
            [] => Some(TxOp::Probe {
                addr,
                ack: self.acked(),
            }),
            [reg, again] if *again == first | 1 => Some(TxOp::Read {
                addr,
                reg: *reg,
                expect: self.rx.clone(),
            }),
            [reg, values @ ..] if self.rx.is_empty() => Some(TxOp::Write {
                addr,
                reg: *reg,
                values: values.to_vec(),
            }),
            _ => None,
        }
    }
}

/// Splits I2C0 register traffic into transactions: `FIFO_CONF.tx_fifo_rst` starts a TX buffer,
/// each `DATA` write adds a byte, a `CTR` write with `trans_start` sends it, and the `INT_ST` and
/// `DATA` reads after that belong to it.
fn bus_transactions(records: &[pemu_core::trace::TraceRecord]) -> Vec<BusTxn> {
    use pemu_core::trace::TraceEvent::{MmioRead, MmioWrite, PollRun};
    let (fifo_conf, data, ctr, int_st) = (
        I2C0_BASE + I2C_FIFO_CONF,
        I2C0_BASE + I2C_DATA,
        I2C0_BASE + I2C_CTR,
        I2C0_BASE + 0x02C,
    );
    let mut txns: Vec<BusTxn> = Vec::new();
    let mut tx = Vec::new();
    for rec in records {
        match rec.ev {
            MmioWrite { addr, val, .. } if addr == fifo_conf && val & (1 << 13) != 0 => {
                tx.clear();
            }
            MmioWrite { addr, val, .. } if addr == data => tx.push(val as u8),
            MmioWrite { addr, val, .. } if addr == ctr && val & (1 << 5) != 0 => {
                txns.push(BusTxn {
                    tx: std::mem::take(&mut tx),
                    ..BusTxn::default()
                });
            }
            MmioRead { addr, val, .. } | PollRun { addr, val, .. } if addr == int_st => {
                if let Some(last) = txns.last_mut() {
                    last.int_st.get_or_insert(val);
                }
            }
            MmioRead { addr, val, .. } if addr == data => {
                if let Some(last) = txns.last_mut() {
                    last.rx.push(val as u8);
                }
            }
            _ => {}
        }
    }
    txns
}

/// The transactions to one 7-bit address as transcript ops, failing on a shape no BSP path uses.
fn ops_to(txns: &[BusTxn], addr: u8) -> Vec<TxOp> {
    txns.iter()
        .filter(|t| t.tx.first().is_some_and(|b| b >> 1 == addr))
        .map(|t| {
            t.op()
                .unwrap_or_else(|| panic!("an I2C transaction of no BSP shape: {t:?}"))
        })
        .filter(|op| !matches!(op, TxOp::Probe { .. }))
        .collect()
}

fn transcript_ops(name: &str, text: &str) -> Vec<TxOp> {
    Transcript::parse(text)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
        .ops
        .into_iter()
        .filter(|op| !matches!(op, TxOp::Delay { .. }))
        .collect()
}

/// The settled `official` menu is drawn on a lit panel: the backlight at duty 1023 of 1024 and the
/// panel powered, out of sleep and in DISPON.
///
/// `backlight_init` sets LEDC low-speed timer 0 to 10 bits at 5000 Hz and channel 0 on GPIO21 with
/// `output_invert` 0; `bsp_display_backlight(100)` calls `ledc_set_duty(LOW_SPEED, 0, 1023)`, and
/// `ledc_hal_set_duty_int_part` stores it shifted left by four fractional bits (the corpus ELF's
/// disassembly). The LEDC hands the board `DUTY_R >> 4`, which the backlight must not shift again.
#[test]
fn t1_m5_official_menu_backlight_and_panel_are_lit() {
    use pemu_machine::run::RunLimits;

    let test = "t1_m5_official_menu_backlight_and_panel_are_lit";
    let Some(path) =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let image = std::fs::read(&path).expect("the verified corpus file is readable");
    let flash = pemu_loader::bundle::FlashImage::from_merged(&image).expect("parses");
    let assets = pemu_machine::config::Assets::with_bundled_rom(
        flash,
        None,
        None,
        pemu_loader::efuse_image::EfuseImage::synth(0),
    )
    .expect("the bundled ROM is pinned");
    let mut m =
        pemu_machine::machine::Machine::new(pemu_machine::config::MachineConfig::default(), assets)
            .expect("the image fits");
    // The settled menu: 3 s of virtual time with no key presses.
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(3_000)),
        max_insns: None,
        stops: pemu_machine::stops::StopSet::default(),
    });
    assert_eq!(
        out.reason,
        pemu_machine::stops::StopReason::Until,
        "{test}: the menu boot ended early"
    );

    let backlight = m.board().backlight.clone();
    assert!(backlight.enabled(), "{test}: channel 0 drives GPIO21");
    assert_eq!(
        (backlight.duty(), backlight.duty_res(), backlight.freq_hz()),
        (1023, 10, 5000),
        "{test}: bsp_display_backlight(100) latches duty 1023 at 10 bits and 5000 Hz"
    );
    let brightness = backlight.brightness();
    assert_eq!(
        (brightness.level(), brightness.scale()),
        (1023, 1024),
        "{test}: the panel pair"
    );

    let frame = &m.io().frame;
    assert_eq!(
        frame.backlight(),
        1023 << 4,
        "{test}: the frame port carries 1023 of 1024 with the four fractional bits"
    );
    assert!(frame.powered(), "{test}: the panel rail is on");
    assert!(!frame.sleeping(), "{test}: SLPOUT was sent");
    assert!(frame.display_on(), "{test}: DISPON was sent");
    assert!(frame.inverted(), "{test}: INVON was sent");
    assert!(
        !frame.glass_complement(),
        "{test}: INVON shows panel memory as it is on this board"
    );
    println!(
        "RAN {test}: duty {} of {} at {} Hz, frame backlight {}, panel powered, awake, DISPON",
        brightness.level(),
        brightness.scale(),
        backlight.freq_hz(),
        frame.backlight()
    );
}

/// Scenario `i2c-scan`: `official`'s `bsp_i2c_scan` finds exactly 0x18 and 0x63 within 50 ms
/// virtual.
///
/// Taken from the I2C0 trace between `I2C 就绪` and `I2C 扫描完成` (`bsp_i2c.c`): 112 probes, one
/// per address from 0x08 to 0x77 in order, exactly two acknowledged. The console lines
/// are checked against the same answer.
#[test]
fn t1_m5_official_boot_bus_scan() {
    let test = "t1_m5_official_boot_bus_scan";
    let id = test.to_string();
    let Some(mut t) = Traced::official(test, true) else {
        return;
    };
    let budget = VTime::from_ms(2_000);
    let before = t.run_to_line("bsp_i2c: I2C 就绪", budget);
    let from = t.records.len();
    let after = t.run_to_line("I2C 扫描完成", budget);

    let txns = bus_transactions(&t.records[from..]);
    let probes: Vec<(u8, bool)> = txns
        .iter()
        .map(|txn| match txn.op() {
            Some(TxOp::Probe { addr, ack }) => (addr, ack),
            other => panic!("{id}: the scan sends only probes, got {other:?} from {txn:?}"),
        })
        .collect();
    let addrs: Vec<u8> = probes.iter().map(|p| p.0).collect();
    assert_eq!(
        addrs,
        (SCAN_FIRST..=SCAN_LAST).collect::<Vec<u8>>(),
        "{id}: one probe per address"
    );
    let found: Vec<u8> = probes.iter().filter(|p| p.1).map(|p| p.0).collect();
    assert_eq!(
        found,
        [CODEC, GAUGE],
        "{id}: exactly the codec and the gauge"
    );

    let console = t.console();
    let lines: Vec<&str> = console
        .lines()
        .map(untimed)
        .filter(|l| l.starts_with("bsp_i2c:"))
        .collect();
    let reported: Vec<&str> = lines
        .iter()
        .filter(|l| l.contains("发现设备"))
        .copied()
        .collect();
    assert_eq!(
        reported,
        [
            "bsp_i2c:   发现设备 @ 0x18  <- ES8311 音频 codec",
            "bsp_i2c:   发现设备 @ 0x63  <- CW2017 电量计",
        ],
        "{id}: {lines:?}"
    );
    assert!(
        lines.contains(&"bsp_i2c: I2C 扫描完成,共 2 个设备"),
        "{lines:?}"
    );

    let took = after.0 - before.0;
    assert!(
        took <= VTime::from_ms(50).0,
        "{id}: the scan took {took} ps virtual, over 50 ms"
    );
    println!(
        "RAN {test} official: 112 probes, ACK 0x18 and 0x63, {} us virtual",
        took / 1_000_000
    );
}

/// Scenario `hang-negative`, `official` variant: with `--disable-model spi2` the run stops with
/// `E_STUCK` naming `spi2.cmd_update` within 3 s. It holds only because the fast NACK path answers
/// the 110 absent addresses at once, so the scan must finish before the stop.
#[test]
fn t1_m5_official_hang_negative() {
    use pemu_machine::hang::StuckKind;
    use pemu_machine::run::RunLimits;
    use pemu_machine::stops::{StopReason, StopSet};

    let test = "t1_m5_official_hang_negative";
    let id = test.to_string();
    let Some(mut t) = Traced::official(test, false) else {
        return;
    };
    assert!(t.m.disable_model("spi2"), "spi2 can be disabled");
    let limit = VTime::from_ms(3_000);
    let out = t.m.run(RunLimits {
        until: Some(limit),
        max_insns: None,
        stops: StopSet::default(),
    });
    let StopReason::Stuck(report) = out.reason else {
        panic!(
            "{id}: `official` without spi2 did not stop as stuck within 3 s: {:?} at {:?}, pc {:#x}",
            out.reason,
            out.vt,
            t.m.hart().pc
        );
    };
    assert_eq!(report.wait_row, Some("spi2.cmd_update"), "{id}: {report:?}");
    assert_eq!(report.block, "spi2");
    assert_ne!(report.kind, StuckKind::Fallback, "{report:?}");
    assert!(out.vt <= limit, "{id}: stopped at {:?}", out.vt);
    let console = t.console();
    assert!(
        console.contains("I2C 扫描完成,共 2 个设备"),
        "{id}: the scan ran to its end before display init"
    );
    println!(
        "RAN {test} official: E_STUCK spi2.cmd_update at {:?}",
        out.vt
    );
}

/// The shared `update` bit 8 of I2S0 `TX_CONF` and `RX_CONF` (IDF
/// `soc/esp32c3/register/soc/i2s_reg.h`).
const I2S_CONF_UPDATE: u32 = 1 << 8;

/// For `TX_CONF` and `RX_CONF`, the number of `update` writes whose next read came back clear;
/// fails on one that read it set (`i2s_ll_tx_update` spins until it reads 0).
fn update_self_clears(records: &[pemu_core::trace::TraceRecord]) -> [usize; 2] {
    use pemu_core::trace::TraceEvent::{MmioRead, MmioWrite, PollRun};
    let mut cleared = [0; 2];
    let mut waiting = [false; 2];
    for rec in records {
        let (addr, val, write) = match rec.ev {
            MmioWrite { addr, val, .. } => (addr, val, true),
            MmioRead { addr, val, .. } | PollRun { addr, val, .. } => (addr, val, false),
            _ => continue,
        };
        let Some(k) = [I2S_TX_CONF, I2S_RX_CONF]
            .iter()
            .position(|r| I2S0_BASE + *r == addr)
        else {
            continue;
        };
        if write {
            waiting[k] = val & I2S_CONF_UPDATE != 0;
        } else if waiting[k] {
            assert_eq!(
                val & I2S_CONF_UPDATE,
                0,
                "{addr:#x}: the update bit reads back set after the write that set it"
            );
            cleared[k] += 1;
            waiting[k] = false;
        }
    }
    cleared
}

/// The ES8311 register traffic of `official` equals sequences A and B of
/// `specs/es8311-sequences.toml`, and the I2S0 `tx_update`/`rx_update` self-clears hold.
///
/// A is `bsp_audio_init` up to `bsp_audio: ES8311 就绪`; B is `bsp_audio_set_format(16000, 16, 1)`
/// when the Audio card plays its tone, followed by C, `bsp_audio_set_volume(80)`. The codec's
/// register file is checked after A and after B and C. Every `update` write reads back 0.
#[test]
fn t1_m5_es8311_sequences_on_official() {
    let test = "t1_m5_es8311_sequences_on_official";
    let id = test.to_string();
    let Some(mut t) = Traced::official(test, true) else {
        return;
    };
    let budget = VTime::from_ms(2_000);
    t.run_to_line("bsp_audio: ES8311 就绪", budget);
    let init = ops_to(&bus_transactions(&t.records), CODEC);
    assert_eq!(
        init,
        transcript_ops("A", ES8311_A_OPEN),
        "{id}: bsp_audio_init is sequence A"
    );
    // Chip side: what the codec received, not only what the driver queued.
    let mut expected = codec_file_after(Es8311::new().registers(), &init);
    assert_codec_file(&t.m, &expected, &format!("{id}: after A"));
    t.run_to_line("main: 就绪", budget);

    for button in [ButtonId::Down, ButtonId::Down, ButtonId::Ok] {
        t.click(button);
    }
    let menu = ops_to(&bus_transactions(&t.records), CODEC);
    assert_eq!(
        menu.len(),
        init.len(),
        "{id}: opening the Audio page talks to no codec"
    );
    let from = t.records.len();
    t.click(ButtonId::Ok);
    let opened = "bsp_audio: codec 打开 16000Hz/16bit/1ch";
    if !t.console().contains(opened) {
        t.run_to_line(opened, VTime::from_ms(5_000));
    }
    // The volume write follows within the same task step; 100 ms more reaches it.
    let settle = VTime(t.m.now().0 + VTime::from_ms(100).0);
    t.run_until(None, settle);
    let ops = ops_to(&bus_transactions(&t.records[from..]), CODEC);
    let format = transcript_ops("B", ES8311_B_SET_FORMAT);
    let volume = transcript_ops("C", ES8311_C_SET_VOLUME);
    assert!(
        ops.len() >= format.len() + volume.len(),
        "{id}: the tone request sends B then C, got {} ops: {ops:?}",
        ops.len()
    );
    assert_eq!(
        ops[..format.len()],
        format[..],
        "{id}: bsp_audio_set_format(16000, 16, 1) is sequence B"
    );
    assert_eq!(
        ops[format.len()..format.len() + volume.len()],
        volume[..],
        "{id}: bsp_audio_set_volume(80) follows as sequence C"
    );
    // Every register B and C wrote holds its last value, and the volume register holds 0xB2.
    expected = codec_file_after(&expected, &ops);
    assert_codec_file(&t.m, &expected, &format!("{id}: after B and C"));
    for op in &volume {
        if let TxOp::Write { reg, values, .. } = op {
            assert_eq!(
                t.m.board().codec.reg(*reg),
                *values.last().expect("one byte"),
                "{id}: codec register {reg:#04x} after C"
            );
        }
    }
    assert_eq!(
        t.m.board().codec.reg(0x32),
        0xB2,
        "{id}: bsp_audio_set_volume(80) reached the codec's DAC volume register"
    );
    println!(
        "RAN {test} official: A {} ops at bsp_audio_init, B {} ops at the Audio page's set_format, \
         then C",
        init.len(),
        format.len()
    );

    let [tx, rx] = update_self_clears(&t.records);
    assert!(tx > 0 && rx > 0, "{id}: tx_update {tx}, rx_update {rx}");
    println!("RAN {test} i2s0-update-self-clear: tx_update {tx}, rx_update {rx}, all read back 0");
}

/// The ES8311 register file expected after `ops` land on a codec holding `from`: each written
/// register takes its last byte, the read-only 0xFC to 0xFF keep theirs. Derived without
/// `Es8311::write_reg`, so a codec that drops a write disagrees. No sequence writes INI_REG 0xFA,
/// whose reset this would have to model; the assertion guards that.
fn codec_file_after(from: &[u8], ops: &[TxOp]) -> Vec<u8> {
    let mut file = from.to_vec();
    for op in ops {
        if let TxOp::Write { reg, values, .. } = op {
            assert_ne!(*reg, 0xFA, "an INI_REG write in an ES8311 sequence");
            if let Some(v) = values.last()
                && !pemu_board::es8311::READ_ONLY.contains(reg)
            {
                file[*reg as usize] = *v;
            }
        }
    }
    file
}

fn assert_codec_file(m: &pemu_machine::machine::Machine, expected: &[u8], what: &str) {
    let got = m.board().codec.registers();
    if let Some(reg) = (0..expected.len()).find(|&r| got[r] != expected[r]) {
        panic!(
            "{what}: codec register {reg:#04x} holds {:#04x}, the writes leave {:#04x}",
            got[reg], expected[reg]
        );
    }
}

/// `APB_SARADC_1_DATA_STATUS` (IDF `soc/esp32c3/register/soc/apb_saradc_reg.h`), which
/// `adc_oneshot_ll_get_raw_result` reads.
const SARADC_DATA: u32 = SARADC_BASE + 0x02C;

/// The raw codes (`& 0xFFF`) the driver read out of APB_SARADC in `records`.
fn adc_codes(records: &[pemu_core::trace::TraceRecord]) -> Vec<u16> {
    use pemu_core::trace::TraceEvent::{MmioRead, PollRun};
    records
        .iter()
        .filter_map(|r| match r.ev {
            MmioRead { addr, val, .. } | PollRun { addr, val, .. } if addr == SARADC_DATA => {
                Some((val & saradc::DATA_MASK) as u16)
            }
            _ => None,
        })
        .collect()
}

/// `input click up`, `down` and `ok` on `official` put the raw codes 3, 393 and 782 into the ADC
/// trace while pressed, and 4095 once released (the device's codes, DOWN one below its 394). They
/// are the `1_DATA_STATUS` reads the `iot_button` driver itself made.
#[test]
fn t1_m5_ladder_codes_in_the_adc_trace() {
    let test = "t1_m5_ladder_codes_in_the_adc_trace";
    let id = test.to_string();
    let Some(mut t) = Traced::official(test, true) else {
        return;
    };
    t.run_to_line("main: 就绪", VTime::from_ms(2_000));
    let idle = adc_codes(&t.records);
    assert!(
        !idle.is_empty(),
        "{id}: the button driver samples during boot"
    );
    assert!(
        idle.iter().all(|c| *c == 4_095),
        "{id}: released at boot: {idle:?}"
    );

    for (button, want) in [
        (ButtonId::Up, 3),
        (ButtonId::Down, 393),
        (ButtonId::Ok, 782),
    ] {
        let (pressed, released) = t.click(button);
        let held = adc_codes(&t.records[pressed..released]);
        let after = adc_codes(&t.records[released..]);
        assert!(
            !held.is_empty() && held.iter().all(|c| *c == want),
            "{id}: {button:?} pressed reads {want}: {held:?}"
        );
        assert!(
            !after.is_empty() && after.iter().all(|c| *c == 4_095),
            "{id}: {button:?} released reads 4095: {after:?}"
        );
        println!(
            "RAN {test} {button:?}: {} reads of {want} pressed, {} of 4095 released",
            held.len(),
            after.len()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Registry checks on a hooked `official` instance
// ---------------------------------------------------------------------------------------------

/// The seven menu cards of `official`, in `DEMOS[]` order (`main.c`).
const MENU_CARDS: [&str; 7] = [
    "Display",
    "Button",
    "Audio",
    "Battery",
    "Wi-Fi",
    "BLE",
    "Low Power",
];

fn ui_labels(text: &str) -> Vec<(String, [u32; 4])> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- label \"")?;
            let (label, rest) = rest.split_once("\" [")?;
            let (geom, _) = rest.split_once(']')?;
            let (xy, wh) = geom.split_once(' ')?;
            let (x, y) = xy.split_once(',')?;
            let (w, h) = wh.split_once('x')?;
            let n = |v: &str| v.parse::<u32>().ok();
            Some((label.to_owned(), [n(x)?, n(y)?, n(w)?, n(h)?]))
        })
        .collect()
}

/// `official` prints `main: 就绪:Display=1 Button=1 Audio=1 Battery=1` within 2 s virtual of
/// reset, and the `ui` tree lists the seven cards with no `[FAIL]` (`main.c` appends `  [FAIL]` to
/// a card whose init failed, so an exact label comparison is the check).
#[test]
fn t1_m5_official_ready_and_menu_tree() {
    let test = "t1_m5_official_ready_and_menu_tree";
    let id = test.to_string();
    let Some(_hooks) = hooked_official(test) else {
        return;
    };
    let start = call(
        "start",
        serde_json::json!({"fw": common::OFFICIAL, "boot": "none"}),
    )
    .expect("official starts");
    assert_eq!(start.json["vt_us"], 0, "{id}: at reset: {}", start.json);
    let inst = start.json["instance"].as_str().expect("an id").to_owned();

    let run = call(
        "run",
        serde_json::json!({"instance": inst, "until": "serial:/main: 就绪/", "timeout": "2s"}),
    )
    .unwrap_or_else(|e| panic!("{id}: the ready line within 2 s virtual: {e:?}"));
    assert_eq!(run.json["status"], "matched", "{id}: {}", run.json);
    let at = run.json["match"]["vt_us"]
        .as_u64()
        .expect("a match instant");
    assert!(at <= 2_000_000, "{id}: printed at {at} us virtual");
    let line = run.json["match"]["text"].as_str().expect("the line");
    assert_eq!(
        untimed(line),
        "main: 就绪:Display=1 Button=1 Audio=1 Battery=1",
        "{id}"
    );

    let ui = call("ui", serde_json::json!({"instance": inst})).expect("the tree reads");
    let text = ui.json["text"].as_str().expect("a rendered tree");
    assert!(!text.contains("[FAIL]"), "{id}: {text}");
    let cards: Vec<String> = ui_labels(text)
        .into_iter()
        .map(|(l, _)| l)
        .filter(|l| l != "FoloToy")
        .collect();
    assert_eq!(cards, MENU_CARDS, "{id}: the menu cards");
    println!(
        "RAN {test} official: ready line at {at} us virtual; 7 cards, no [FAIL] (ui_rev {})",
        ui.json["ui_rev"]
    );
    stop(&inst);
}

/// Semantic pruning on the real menu: 63 LVGL objects prune to 17 lines, with every
/// label kept and the selected card still carrying its colours. LVGL sets `CLICKABLE` on every
/// object (`lv_obj.c:584`), so the flag alone is not a role; dropped are the status icon pieces,
/// the bottom bar and grass tiles, the card shadows and the pixel-art mascot.
#[test]
fn t1_m5_official_menu_tree_prunes_to_17_lines() {
    let test = "t1_m5_official_menu_tree_prunes_to_17_lines";
    let Some(_hooks) = hooked_official(test) else {
        return;
    };
    let start = call(
        "start",
        serde_json::json!({"fw": common::OFFICIAL, "boot": "none"}),
    )
    .expect("official starts");
    let inst = start.json["instance"].as_str().expect("an id").to_owned();
    let run = call(
        "run",
        serde_json::json!({"instance": inst, "until": "serial:/main: 就绪/", "timeout": "2s"}),
    )
    .expect("the ready line");
    assert_eq!(run.json["status"], "matched", "{test}: {}", run.json);

    let full = call(
        "ui",
        serde_json::json!({"instance": inst, "prune": "none", "include_style": true}),
    )
    .expect("the full tree reads");
    let pruned = call(
        "ui",
        serde_json::json!({"instance": inst, "include_style": true}),
    )
    .expect("the pruned tree reads");
    let full_text = full.json["text"].as_str().expect("a rendered tree");
    let text = pruned.json["text"].as_str().expect("a rendered tree");
    assert_eq!(full.json["counts"]["objects"], 63, "{test}: {full_text}");
    assert_eq!(full.json["counts"]["labels"], 8, "{test}: {full_text}");
    assert_eq!(text.lines().count(), 17, "{test}: the pruned tree\n{text}");
    assert_eq!(
        pruned.json["counts"]["shown"], 17,
        "{test}: {}",
        pruned.json
    );

    // Every label of the unpruned tree survives, in the same order and at the same box.
    let labels = ui_labels(full_text);
    assert_eq!(labels.len(), 8, "{test}: {full_text}");
    assert_eq!(ui_labels(text), labels, "{test}: the labels\n{text}");
    // The selected card keeps its colours, directly above its label.
    let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
    let display = lines
        .iter()
        .position(|l| l.starts_with("- label \"Display\" "))
        .unwrap_or_else(|| panic!("{test}: no Display label\n{text}"));
    assert!(
        display > 0
            && lines[display - 1].starts_with("- obj [")
            && lines[display - 1].contains(" bg=#ffd928 border=#ffffff "),
        "{test}: the selected card above Display\n{text}"
    );
    // Every card container survives with its style, and nothing plain is left beside it.
    let styled = lines
        .iter()
        .filter(|l| l.starts_with("- obj [") && l.contains(" border="))
        .count();
    assert_eq!(styled, 8, "{test}: the header and the seven cards\n{text}");
    println!(
        "RAN {test} official: {} objects prune to {} lines, 8 labels and the selected card kept",
        full.json["counts"]["objects"],
        text.lines().count()
    );
    stop(&inst);
}

/// `lv_color_hex(0xFF5A5A)` and `lv_color_hex(0x39FF88)` in RGB565, the two SOC label colours of
/// `demo_battery.c` (LVGL truncates each channel to 5, 6 and 5 bits).
const SOC_LOW_RGB565: u16 = 0xFACB;
const SOC_OK_RGB565: u16 = 0x3FF1;

fn pixels_of(rgb: &[u8], bbox: [u32; 4], colour: u16) -> usize {
    let want = pemu_api::commands::screenshot::rgb565_to_rgb888(colour);
    let [x0, y0, w, h] = bbox;
    (y0..y0 + h)
        .flat_map(|y| (x0..x0 + w).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let at = ((y * 240 + x) * 3) as usize;
            rgb[at..at + 3] == want
        })
        .count()
}

fn soc_label(ui: &serde_json::Value) -> (String, [u32; 4]) {
    let text = ui["text"].as_str().expect("a rendered tree");
    ui_labels(text)
        .into_iter()
        .find(|(l, _)| l.ends_with(" %"))
        .unwrap_or_else(|| panic!("no SOC label on the page: {text}"))
}

/// Scenario `battery-low`: `env battery --soc 10` produces the firmware's low-battery state within
/// 5 s virtual.
///
/// The Battery page reads the gauge once a second and draws the SOC label red below 20 % and green
/// otherwise (`demo_battery.c` `tick`). The test opens the page, sees `100 %` in green, applies the
/// override, and runs until the tree shows `10 %`; the screenshot must then draw it in red only.
#[test]
fn t1_m5_battery_low() {
    let test = "t1_m5_battery_low";
    let id = test.to_string();
    let Some(_hooks) = hooked_official(test) else {
        return;
    };
    let inst = settled_official();
    for button in ["down", "down", "down", "ok"] {
        call(
            "input",
            serde_json::json!({"instance": inst, "button": button, "action": "click"}),
        )
        .expect("the click is applied");
    }
    call("run", serde_json::json!({"instance": inst, "for": "1s"})).expect("the page settles");
    let artifacts = temp("battery-low");
    let ui = call("ui", serde_json::json!({"instance": inst})).expect("the tree reads");
    let (label, bbox) = soc_label(&ui.json);
    assert_eq!(
        label, "100 %",
        "{id}: a full battery first: {}",
        ui.json["text"]
    );
    let (png, _) = shot_in_process(&inst, &artifacts.join("before"));
    let (_, _, rgb) = pemu_host::png::decode_rgb888(&png).expect("a PNG");
    let (ok_before, low_before) = (
        pixels_of(&rgb, bbox, SOC_OK_RGB565),
        pixels_of(&rgb, bbox, SOC_LOW_RGB565),
    );
    assert!(
        ok_before > 0 && low_before == 0,
        "{id}: `100 %` is drawn green ({ok_before} px) and not red ({low_before} px)"
    );

    let env = call(
        "env",
        serde_json::json!({"instance": inst, "battery": {"soc": 10}}),
    )
    .expect("env battery applies");
    let applied = env.json["vt_us"].as_u64().expect("an instant");
    let mut shown = None;
    while shown.is_none() {
        let run = call("run", serde_json::json!({"instance": inst, "for": "250ms"}))
            .expect("the run advances");
        let now = run.json["vt_us"].as_u64().expect("an instant");
        let ui = call("ui", serde_json::json!({"instance": inst})).expect("the tree reads");
        let (label, bbox) = soc_label(&ui.json);
        if label == "10 %" {
            shown = Some((now, bbox));
        } else {
            assert!(
                now - applied < 5_000_000,
                "{id}: still `{label}` 5 s virtual after env battery --soc 10"
            );
        }
    }
    let (now, bbox) = shown.expect("shown");
    assert!(
        now - applied <= 5_000_000,
        "{id}: `10 %` shown {} us virtual after env battery --soc 10, over 5 s",
        now - applied
    );
    let (png, _) = shot_in_process(&inst, &artifacts.join("after"));
    let (_, _, rgb) = pemu_host::png::decode_rgb888(&png).expect("a PNG");
    let (ok_after, low_after) = (
        pixels_of(&rgb, bbox, SOC_OK_RGB565),
        pixels_of(&rgb, bbox, SOC_LOW_RGB565),
    );
    assert!(
        low_after > 0 && ok_after == 0,
        "{id}: `10 %` is drawn red ({low_after} px) and not green ({ok_after} px)"
    );
    println!(
        "RAN {test} official: `10 %` in the low colour {} us virtual after env battery --soc 10 \
         ({low_after} px of 0xFF5A5A)",
        now - applied
    );
    stop(&inst);
    std::fs::remove_dir_all(&artifacts).ok();
}

/// The reference agent loop on `official` totals under 8,000 characters of text output, each
/// output at most 4,000: `start official`, run until `main: 就绪`, `ui`, `input click down`,
/// `run --until ui:changed`, `ui --diff` (which answers `diff: []` here). The measure is
/// `Output::text_chars`, as `xtask agent-budget` measures.
#[test]
fn t1_m5_agent_budget_on_official() {
    const PER_OUTPUT_CHARS: usize = 4_000;
    const LOOP_CHARS: usize = 8_000;

    let test = "t1_m5_agent_budget_on_official";
    let id = test.to_string();
    let Some(_hooks) = hooked_official(test) else {
        return;
    };
    let start = call(
        "start",
        serde_json::json!({"fw": common::OFFICIAL, "boot": "none"}),
    )
    .expect("official starts");
    let inst = start.json["instance"].as_str().expect("an id").to_owned();
    let mut steps = vec![("start official", start.text_chars())];
    let mut diff = serde_json::Value::Null;
    let rest = [
        (
            "run --until serial:/main: 就绪/",
            "run",
            serde_json::json!({"instance": inst, "until": "serial:/main: 就绪/", "timeout": "5s"}),
        ),
        ("ui", "ui", serde_json::json!({"instance": inst})),
        (
            "input click down",
            "input",
            serde_json::json!({"instance": inst, "button": "down", "action": "click"}),
        ),
        (
            "run --until ui:changed",
            "run",
            serde_json::json!({"instance": inst, "until": "ui:changed", "timeout": "5s"}),
        ),
        (
            "ui --diff 1",
            "ui",
            serde_json::json!({"instance": inst, "diff": 1}),
        ),
    ];
    for (label, name, args) in rest {
        let out = call(name, args).unwrap_or_else(|e| panic!("{id}: `{label}` refused: {e:?}"));
        if name == "run" {
            // A run that elapsed or failed would measure a loop that never reached its step.
            assert_eq!(
                out.json["status"], "matched",
                "{id}: `{label}` did not match: {}",
                out.json
            );
        }
        if name == "ui" && label.starts_with("ui --diff") {
            diff = out.json["diff"].clone();
        }
        steps.push((label, out.text_chars()));
    }
    stop(&inst);
    let total: usize = steps.iter().map(|s| s.1).sum();
    for (label, chars) in &steps {
        assert!(
            *chars <= PER_OUTPUT_CHARS,
            "{id}: `{label}` renders {chars} characters, over {PER_OUTPUT_CHARS}"
        );
    }
    assert!(
        total < LOOP_CHARS,
        "{id}: the reference loop renders {total} characters, not under {LOOP_CHARS}: {steps:?}"
    );
    println!(
        "RAN {test} official: reference loop {total} characters {steps:?}, ui --diff 1 {diff}"
    );
}

fn bench_records(stdout: &str) -> std::collections::BTreeMap<String, serde_json::Value> {
    let from = stdout.find("[\n").expect("a --json array");
    let end = stdout[from..].rfind("\n]").expect("a --json array") + from + 2;
    let array: Vec<serde_json::Value> =
        serde_json::from_str(&stdout[from..end]).expect("the --json array parses");
    array
        .into_iter()
        .map(|r| {
            (
                r["workload"].as_str().expect("a workload id").to_string(),
                r,
            )
        })
        .collect()
}

/// Ceiling on the repeats, not a count: `xtask bench` stops once three in a row agree within
/// 10 %. It cannot remove host noise: F3's wall is 1.16 s on a quiet host against its 2 s target,
/// and the fastest of five runs at load average 39 was 5.39 s.
const PERF_REPEAT: &str = "25";

/// Native perf: F1 at most 0.2 s wall; F3, the 60 s menu idle, at most 2 s at `Max` with idle cost
/// c at most 0.02; the worst 100 ms virtual window on F4 and F5 card entry at most 30 ms; F1, F3
/// and F5 busy MIPS and c recorded and trend-gated at 10 %; `xtask bench --check-model` passing.
///
/// Runs `xtask bench` with its own history file. On a contended host or a run spread over both
/// core clusters it asserts only the guest-side quantities and SKIPs, as `xtask bench` reports
/// NOT MEASURED.
#[test]
fn t1_m5_native_perf() {
    let test = "t1_m5_native_perf";
    let id = test.to_string();
    if common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin").is_none()
    {
        return;
    }
    let dir = std::env::temp_dir().join(format!("pemu-native-perf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    let history = dir.join("history.json");
    let history_arg = history.to_str().expect("a UTF-8 temporary path");

    let stdout = xtask_bench(&[
        "--workload",
        "F1,F3,F4,F5",
        "--repeat",
        PERF_REPEAT,
        "--history",
        history_arg,
        "--gate-exits",
        "--json",
    ])
    .unwrap_or_else(|e| panic!("{id}: {e}"));
    let records = bench_records(&stdout);
    assert_eq!(
        records.keys().collect::<Vec<_>>(),
        ["F1", "F3", "F4", "F5"],
        "{id}: the run measured other workloads than the row names"
    );

    let metric = |id: &str, name: &str| -> Option<f64> { records[id]["metrics"][name].as_f64() };
    let need = |workload: &str, name: &str| -> f64 {
        metric(workload, name)
            .unwrap_or_else(|| panic!("{id}: {workload} did not record {name}: {stdout}"))
    };

    // The guest-side quantities are exact on any host. No window may exceed the 160 MHz core, and
    // every workload must have run.
    for workload in ["F1", "F3", "F4", "F5"] {
        assert!(
            need(workload, "busy_insns") > 0.0,
            "{id}: {workload} executed nothing"
        );
        assert!(
            need(workload, "virtual_s") > 0.0 && need(workload, "windows") >= 1.0,
            "{id}: {workload} covered no virtual time"
        );
        let max = need(workload, "demand_max_mips");
        assert!(
            max > 0.0 && max <= 160.0,
            "{id}: {workload} demands {max:.2} MIPS in a window, over the 160 MHz core"
        );
        assert!(
            need(workload, "demand_p95_mips") <= max,
            "{id}: {workload} p95 demand is over its maximum"
        );
    }

    // A host running other work measured none of the budgets below.
    if let Some(load) = records
        .values()
        .find(|r| r["contended"] == serde_json::json!(true))
        .and_then(|r| r["load_avg"].as_f64())
    {
        common::skip(
            test,
            &format!(
                "host busy: one-minute load average {load:.1} on {} cores; every M5 perf target is \
                 an absolute host-time budget, so this host did not measure one (the guest-side \
                 demand and instruction counts above did run and passed)",
                std::thread::available_parallelism().map_or(1, |n| n.get())
            ),
        );
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    // Nor did a run the OS spread over both core clusters: the record says `mixed` (or
    // `efficiency`) and the gates are NOT MEASURED.
    if let Some((workload, r)) = records
        .iter()
        .find(|(_, r)| matches!(r["cores"]["cluster"].as_str(), Some("mixed" | "efficiency")))
    {
        common::skip(
            test,
            &format!(
                "{workload} ran on both core clusters: {}; every M5 perf target is an absolute \
                 host-time budget of the performance cores, so this host did not measure one (the \
                 guest-side demand and instruction counts above did run and passed)",
                r["cores"]
            ),
        );
        std::fs::remove_dir_all(&dir).ok();
        return;
    }

    // `--gate-exits` already failed the run on a miss; these name the numbers.
    let f1_wall = need("F1", "wall_s");
    assert!(f1_wall <= 0.2, "{id}: F1 wall {f1_wall:.4} s is over 0.2 s");
    let f3_wall = need("F3", "wall_s");
    let f3_c = need("F3", "idle_cost");
    assert!(f3_wall <= 2.0, "{id}: F3 wall {f3_wall:.4} s is over 2 s");
    assert!(f3_c <= 0.02, "{id}: F3 idle cost {f3_c:.5} is over 0.02");
    let f4_worst = need("F4", "worst_window_ms");
    let f5_worst = need("F5", "worst_window_ms");
    for (workload, worst) in [("F4", f4_worst), ("F5", f5_worst)] {
        assert!(
            worst <= 30.0,
            "{id}: the worst 100 ms window of {workload} took {worst:.2} ms host, over the 30 ms \
             hard gate"
        );
    }

    // F1 has no idle cost: the boot to `Calling app_main()` never idles.
    for workload in ["F1", "F3", "F5"] {
        let s = need(workload, "busy_mips");
        assert!(
            s > 0.0 && s.is_finite(),
            "{id}: {workload} recorded busy MIPS {s}"
        );
    }
    assert!(metric("F5", "idle_cost").is_some(), "{id}: F5 records c");
    assert_eq!(metric("F1", "idle_cost"), None, "{id}: F1 never idles");
    let notes = records["F1"]["metrics"]["notes"].to_string();
    assert!(
        notes.contains("c is not defined for this workload"),
        "{id}: F1's record must say why it has no c: {notes}"
    );

    // The trend gate, proved by a regression it has to catch: a planted baseline 25 % faster.
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&history).expect("the history was written"))
            .expect("the history is JSON");
    let faster = {
        let mut r = records["F1"].clone();
        r["metrics"]["busy_mips"] = serde_json::json!(need("F1", "busy_mips") * 1.25);
        r["unix_s"] = serde_json::json!(0);
        r
    };
    doc["records"] = serde_json::json!([faster]);
    let planted = dir.join("planted.json");
    std::fs::write(&planted, doc.to_string()).expect("the temporary directory is writable");
    // If the load crosses one runnable thread per core meanwhile, `xtask bench` reports the
    // regression NOT MEASURED instead of failing; it must still name it.
    let refused = match xtask_bench(&[
        "--workload",
        "F1",
        "--repeat",
        "1",
        "--history",
        planted.to_str().expect("a UTF-8 temporary path"),
        "--no-record",
    ]) {
        Err(refused) => refused,
        Ok(out) => {
            assert!(
                out.contains("NOT MEASURED, load average")
                    || out.contains("NOT MEASURED, core cluster"),
                "{id}: a 25 % loss against the baseline must fail the 10 % gate, or be reported \
                 as NOT MEASURED on a contended host or a run spread over both core clusters; \
                 it did neither:\n{out}"
            );
            out
        }
    };
    assert!(
        refused.contains("busy_mips") && refused.contains("F1"),
        "{id}: the trend gate must name the metric and the workload: {refused}"
    );

    let model = xtask_bench(&[
        "--check-model",
        "--gate",
        "--gate-engine",
        "native",
        "--history",
        history_arg,
    ])
    .unwrap_or_else(|e| panic!("{id}: {e}"));
    assert!(
        model.contains("F3 native wall at Max") && model.contains("reachable"),
        "{id}: --check-model did not check the native F3 row: {model}"
    );
    assert!(
        model.contains("history record F3"),
        "{id}: --check-model must use the measured F3 S, not a stand-in: {model}"
    );
    std::fs::remove_dir_all(&dir).ok();

    println!(
        "RAN {test} official: F1 wall {f1_wall:.4} s (<= 0.2), F3 wall {f3_wall:.4} s (<= 2) \
         with c {f3_c:.5} (<= 0.02), worst 100 ms window F4 {f4_worst:.2} ms and F5 \
         {f5_worst:.2} ms (<= 30); busy MIPS F1 {:.2}, F3 {:.2}, F5 {:.2}; c F3 {f3_c:.5}, F5 \
         {:.5}; trend gate refuses a 25 % loss; --check-model passes for native",
        need("F1", "busy_mips"),
        need("F3", "busy_mips"),
        need("F5", "busy_mips"),
        need("F5", "idle_cost"),
    );
}

/// The probe lines of a `probe_limits` console, `PROBE|` to `DONE|`, each as its tag (the fields
/// before the first `key=value`, for example `LIMREG|default`), plus `index` when it has one, and
/// its `key=value` fields.
fn probe_records(lines: &[&str]) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    lines
        .iter()
        .map(|l| l.trim_end_matches('\r'))
        .skip_while(|l| !l.starts_with("PROBE|"))
        .take_while(|l| !l.starts_with("I ("))
        .filter(|l| l.contains('|'))
        .map(|line| {
            let mut tag = Vec::new();
            let mut fields = std::collections::BTreeMap::new();
            for part in line.split('|') {
                match part.split_once('=') {
                    Some((k, v)) => {
                        fields.insert(k.to_owned(), v.to_owned());
                    }
                    None => tag.push(part),
                }
            }
            let mut key = tag.join("|");
            if let Some(index) = fields.get("index") {
                key = format!("{key}|{index}");
            }
            (key, fields)
        })
        .collect()
}

/// The merged image of a probe from `corpus/probes/`, pinned by `tests/fw/manifest.toml`; `None`
/// after the SKIP line.
fn probe_image(test: &str, name: &str) -> Option<Vec<u8>> {
    let pk = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    let path = pk
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join(format!("probes/{name}-8MB.bin"));
    let Ok(bytes) = std::fs::read(&path) else {
        common::skip(
            test,
            &format!("corpus/probes/{name}-8MB.bin is not built (xtask probes)"),
        );
        return None;
    };
    let manifest = std::fs::read_to_string(workspace().join("tests/fw/manifest.toml"))
        .expect("the probe manifest");
    let pinned = manifest
        .split("[[probe]]")
        .find(|block| block.contains(&format!("name = \"{name}\"")))
        .and_then(|block| {
            block
                .lines()
                .find_map(|l| l.strip_prefix("merged_sha256 = \""))
                .map(|v| v.trim_end_matches('"').to_owned())
        })
        .unwrap_or_else(|| panic!("tests/fw/manifest.toml pins the {name} merged image"));
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&bytes),
        pinned,
        "corpus/probes/{name}-8MB.bin is not the pinned build"
    );
    Some(bytes)
}

/// The bookkeeping one TLSF block adds to the used bytes: the size word and the
/// previous-physical-block pointer in front of every block (10 L2335). The unit of the allocator
/// overhead allowance (UNVERIFIED as a bound).
const TLSF_BLOCK_OVERHEAD: u64 = 8;

/// `limits-heap`: the emulator's run of `probe_limits` matches a silicon capture of the same image
/// region by region, within the allocator overhead.
///
/// The reference is `goldens/probe_limits.console.txt`, derived into the data root by `cargo xtask
/// oracle goldens --derive` (never committed: its header names the device's eFuse hash). `HEAPREG`
/// bounds and sizes are equal; each `LIMREG` region's bounds are equal and its used and drained
/// figures differ by at most [`TLSF_BLOCK_OVERHEAD`] per allocation its device `LIMIT` line counts.
#[test]
fn t1_m5_limits_heap() {
    let test = "t1_m5_limits_heap";
    let id = test.to_string();
    let Some(bytes) = probe_image(test, "probe_limits") else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "probe_limits.console.txt") else {
        return;
    };
    let device = probe_records(&golden.lines());

    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_machine::config::{Assets, MachineConfig};
    use pemu_machine::machine::Machine;
    use pemu_machine::run::RunLimits;
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet};
    let flash = FlashImage::from_merged(&bytes).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("the image fits");
    let done = MatcherId(0xD0);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(10_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                done,
                Matcher::Serial {
                    stream: pemu_core::hostio::SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("DONE|".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let ring = m.io().serial_ring(pemu_core::hostio::SerialStream::UsjTx);
    let console = String::from_utf8_lossy(
        &ring
            .slices(ring.tail())
            .iter()
            .copied()
            .collect::<Vec<u8>>(),
    )
    .into_owned();
    assert_eq!(out.reason, StopReason::Matcher(done), "{id}:\n{console}");
    let emulated = probe_records(&console.lines().collect::<Vec<_>>());

    let find = |records: &[(String, std::collections::BTreeMap<String, String>)], key: &str| {
        records
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, f)| f.clone())
    };
    let image = |r: &[(String, std::collections::BTreeMap<String, String>)]| {
        find(r, "IMAGE").and_then(|f| f.get("elf_sha256").cloned())
    };
    assert!(
        image(&device).is_some(),
        "{id}: the capture has an IMAGE line"
    );
    assert_eq!(image(&emulated), image(&device), "{id}: the same image");
    for records in [&device, &emulated] {
        assert_eq!(
            find(records, "DONE").and_then(|f| f.get("status").cloned()),
            Some("ok".to_owned()),
            "{id}: the probe finished"
        );
    }

    let keys = |r: &[(String, std::collections::BTreeMap<String, String>)]| -> Vec<String> {
        r.iter()
            .map(|(k, _)| k.clone())
            .filter(|k| k.starts_with("HEAPREG|") || k.starts_with("LIMREG|"))
            .collect()
    };
    assert_eq!(
        keys(&emulated),
        keys(&device),
        "{id}: the same regions per capability"
    );
    let mut worst = 0u64;
    for key in keys(&device) {
        let (dev, emu) = (
            find(&device, &key).expect("present"),
            find(&emulated, &key).expect("present"),
        );
        for bound in ["start", "end", "size"] {
            assert_eq!(emu.get(bound), dev.get(bound), "{id}: {key} {bound}");
        }
        if let Some(cap) = key
            .strip_prefix("LIMREG|")
            .and_then(|k| k.split('|').next())
        {
            let allocs: u64 = find(&device, &format!("LIMIT|{cap}"))
                .and_then(|f| f.get("allocs").and_then(|v| v.parse().ok()))
                .unwrap_or_else(|| panic!("{id}: LIMIT|{cap} counts its allocations"));
            let allowance = allocs * TLSF_BLOCK_OVERHEAD;
            for field in ["used_before", "used_drained", "drained"] {
                let n = |f: &std::collections::BTreeMap<String, String>| -> u64 {
                    f.get(field)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| panic!("{key} {field} is a number"))
                };
                let diff = n(&emu).abs_diff(n(&dev));
                worst = worst.max(diff);
                assert!(
                    diff <= allowance,
                    "{id}: {key} {field} emulated {} device {}, apart by more than the \
                     allocator overhead of {allowance} bytes",
                    n(&emu),
                    n(&dev)
                );
            }
        }
    }
    println!(
        "RAN {test} probe_limits: {} regions and capability rows equal to the device capture, \
         largest used-bytes difference {worst} bytes",
        keys(&device).len()
    );
}
