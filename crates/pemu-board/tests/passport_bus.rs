//! The board routes the SoC's I2C0 and I2S0 ports to its chips. Driven only through
//! [`BoardPorts`], so an unrouted read returns the floating-bus 0xFF (which the firmware prints as
//! `CW2017 VERSION=0xFF`).

use pemu_board::cw2017::{
    BSP_PROFILE, PROFILE_LEN, REG_INT_CONF, REG_PROFILE, REG_SOC_ALERT, REG_VERSION,
    SOC_ALERT_PROVISIONED, VERSION_OVERRIDE,
};
use pemu_board::es8311::{AudioFormat, Es8311, REG_DAC_VOLUME};
use pemu_board::i2c_transcript::{Transcript, TxOp};
use pemu_board::passport::{BoardConfig, PassportBoard};
use pemu_board::traits::{BoardPorts, PcmFormat};
use pemu_core::time::VTime;

const CODEC: u8 = 0x18;
const GAUGE: u8 = 0x63;

const CW2017_INIT_FAST: &str = include_str!("../../../tests/transcripts/i2c/cw2017-init-fast.txt");
const CW2017_INIT_FRESH: &str =
    include_str!("../../../tests/transcripts/i2c/cw2017-init-fresh.txt");
const ES8311_A_OPEN: &str = include_str!("../../../tests/transcripts/i2c/es8311-a-open.txt");
const ES8311_B_SET_FORMAT: &str =
    include_str!("../../../tests/transcripts/i2c/es8311-b-set-format.txt");

fn board() -> PassportBoard {
    PassportBoard::from_toml(&BoardConfig::default())
}

/// `i2c_master_transmit`: start, address with W, register pointer, values, stop. Returns whether
/// every byte was acknowledged.
fn write_reg(board: &mut PassportBoard, t: VTime, addr: u8, reg: u8, values: &[u8]) -> bool {
    let mut ack = board.i2c_start(t, addr, false);
    ack &= board.i2c_write(t, reg);
    for value in values {
        ack &= board.i2c_write(t, *value);
    }
    board.i2c_stop(t);
    ack
}

/// `i2c_master_transmit_receive`: start W, register pointer, repeated start R, `n` reads, stop.
fn read_reg(board: &mut PassportBoard, t: VTime, addr: u8, reg: u8, n: usize) -> Vec<u8> {
    assert!(board.i2c_start(t, addr, false), "{addr:#04x} acknowledges");
    assert!(board.i2c_write(t, reg), "the pointer is acknowledged");
    assert!(board.i2c_start(t, addr, true), "the repeated start is too");
    let bytes = (0..n).map(|_| board.i2c_read(t)).collect();
    board.i2c_stop(t);
    bytes
}

/// Replays a transcript through the board's ports and panics on the first byte that differs.
fn replay_through_board(board: &mut PassportBoard, text: &str) {
    let transcript = Transcript::parse(text).expect("the transcript parses");
    let mut t = VTime(0);
    for (i, op) in transcript.ops.iter().enumerate() {
        match op {
            TxOp::Probe { addr, ack } => {
                assert_eq!(board.i2c_start(t, *addr, false), *ack, "op {i}: probe");
                board.i2c_stop(t);
            }
            TxOp::Write { addr, reg, values } => {
                assert!(write_reg(board, t, *addr, *reg, values), "op {i}: write");
            }
            TxOp::Read { addr, reg, expect } => {
                let got = read_reg(board, t, *addr, *reg, expect.len());
                assert_eq!(&got, expect, "op {i}: read of {reg:#04x} at {addr:#04x}");
            }
            TxOp::Delay { us } => t = VTime(t.0 + us * 1_000_000),
        }
    }
}

/// The CW2017 0xFF regression.
#[test]
fn a_gauge_version_read_through_the_board_returns_the_override() {
    let mut board = board();
    let t = VTime::from_ms(230);
    assert_eq!(
        read_reg(&mut board, t, GAUGE, REG_VERSION, 1),
        [VERSION_OVERRIDE]
    );
    assert_eq!(VERSION_OVERRIDE, 0x0F);
    assert_eq!(
        read_reg(&mut board, t, GAUGE, REG_SOC_ALERT, 1),
        [SOC_ALERT_PROVISIONED]
    );
    for (i, want) in BSP_PROFILE.iter().enumerate() {
        let reg = REG_PROFILE + i as u8;
        assert_eq!(
            read_reg(&mut board, t, GAUGE, reg, 1),
            [*want],
            "profile byte {i}"
        );
    }
    // A multi-byte read auto-increments, which the BSP's VCELL and SOC reads rely on.
    assert_eq!(
        read_reg(&mut board, t, GAUGE, REG_PROFILE, PROFILE_LEN),
        BSP_PROFILE
    );
}

#[test]
fn a_gauge_write_through_the_board_reaches_the_chip() {
    let mut board = board();
    let t = VTime::from_ms(1);
    assert!(write_reg(&mut board, t, GAUGE, REG_INT_CONF, &[0x5A]));
    assert_eq!(board.gauge.reg(REG_INT_CONF), 0x5A);
    assert_eq!(read_reg(&mut board, t, GAUGE, REG_INT_CONF, 1), [0x5A]);
}

/// The fast path on the default provisioned gauge, the slow path (80 writes, 80 read-backs) on a
/// fresh one.
#[test]
fn the_cw2017_init_transcripts_replay_through_the_board() {
    let mut fast = board();
    replay_through_board(&mut fast, CW2017_INIT_FAST);

    let mut fresh = board();
    fresh.gauge = pemu_board::cw2017::Cw2017::fresh();
    replay_through_board(&mut fresh, CW2017_INIT_FRESH);
    assert!(fresh.gauge.profile_matches_bsp());
}

#[test]
fn a_codec_register_round_trips_through_the_board() {
    let mut board = board();
    let t = VTime::from_ms(1);
    let reset = Es8311::new();
    assert_eq!(
        read_reg(&mut board, t, CODEC, 0x0D, 1),
        [reset.reg(0x0D)],
        "an unwritten register reads its reset value"
    );
    assert!(write_reg(&mut board, t, CODEC, REG_DAC_VOLUME, &[0xB2]));
    assert_eq!(board.codec.reg(REG_DAC_VOLUME), 0xB2);
    assert_eq!(read_reg(&mut board, t, CODEC, REG_DAC_VOLUME, 1), [0xB2]);
    let untouched = PassportBoard::from_toml(&BoardConfig::default());
    assert_eq!(
        board.gauge.reg(REG_INT_CONF),
        untouched.gauge.reg(REG_INT_CONF)
    );
}

/// The codec ends in the register state the direct replay produces.
#[test]
fn the_es8311_open_sequences_replay_through_the_board() {
    let mut board = board();
    replay_through_board(&mut board, ES8311_A_OPEN);
    replay_through_board(&mut board, ES8311_B_SET_FORMAT);

    let mut direct = Es8311::new();
    for text in [ES8311_A_OPEN, ES8311_B_SET_FORMAT] {
        Transcript::parse(text)
            .expect("parses")
            .replay(&mut direct, VTime(0))
            .expect("replays");
    }
    assert_eq!(board.codec.registers(), direct.registers());
}

#[test]
fn an_absent_address_nacks_and_reads_the_floating_bus() {
    let mut board = board();
    let before = board.codec.registers().to_vec();
    let t = VTime::from_ms(1);
    assert!(!board.i2c_start(t, 0x50, false));
    assert!(!board.i2c_write(t, REG_DAC_VOLUME));
    assert!(!board.i2c_write(t, 0x00));
    assert_eq!(board.i2c_read(t), 0xFF);
    board.i2c_stop(t);
    assert_eq!(board.codec.registers(), before.as_slice());
    assert_eq!(board.i2c_target(), None);
}

/// The bytes after the restart go to the codec, and the gauge keeps the pointer its own write set,
/// so a later read start to the gauge without a new pointer reads from there.
#[test]
fn a_restart_to_the_other_chip_ends_the_first_transaction() {
    let mut board = board();
    let t = VTime::from_ms(1);
    assert!(board.i2c_start(t, GAUGE, false));
    assert!(board.i2c_write(t, REG_INT_CONF));
    assert!(board.gauge.in_transaction());
    assert_eq!(board.gauge.pointer(), REG_INT_CONF);

    assert!(board.i2c_start(t, CODEC, false));
    assert_eq!(board.i2c_target(), Some(CODEC));
    assert!(
        !board.gauge.in_transaction(),
        "the start addressed to the codec ended the gauge's transaction"
    );
    assert!(board.i2c_write(t, REG_DAC_VOLUME));
    assert!(board.i2c_write(t, 0xB2));
    board.i2c_stop(t);
    assert_eq!(board.codec.reg(REG_DAC_VOLUME), 0xB2);
    let provisioned = pemu_board::cw2017::Cw2017::provisioned();
    assert_eq!(
        board.gauge.reg(REG_INT_CONF),
        provisioned.reg(REG_INT_CONF),
        "no byte after the restart reached the gauge as data"
    );
    assert_ne!(provisioned.reg(REG_INT_CONF), REG_DAC_VOLUME);
    assert_eq!(
        board.gauge.pointer(),
        REG_INT_CONF,
        "the gauge pointer is untouched"
    );

    assert!(board.i2c_start(t, GAUGE, true));
    assert_eq!(board.i2c_read(t), provisioned.reg(REG_INT_CONF));
    assert_eq!(board.i2c_read(t), provisioned.reg(REG_INT_CONF + 1));
    board.i2c_stop(t);
    assert!(!board.gauge.in_transaction());
}

/// A cell moved directly, with no input event, shows in VCELL on the next I2C read.
#[test]
fn a_gauge_read_samples_a_cell_moved_directly() {
    use pemu_board::cw2017::{Cw2017, REG_VCELL_H};

    let mut board = board();
    let t = VTime::from_ms(1);
    let before = read_reg(&mut board, t, GAUGE, REG_VCELL_H, 2);
    board.battery.set_terminal_mv(3_600);
    let after = read_reg(&mut board, VTime::from_ms(2), GAUGE, REG_VCELL_H, 2);
    assert_ne!(before, after);
    assert_eq!(
        u16::from_be_bytes([after[0], after[1]]),
        Cw2017::vcell_raw(board.battery.terminal_mv())
    );
}

/// The frame shape is the one the SoC passed, and the DAC takes the left slot.
#[test]
fn i2s_tx_frames_reach_the_codec_log() {
    let mut board = board();
    replay_through_board(&mut board, ES8311_A_OPEN);
    replay_through_board(&mut board, ES8311_B_SET_FORMAT);
    assert!(
        board.codec.dac_state().active,
        "sequences A and B power the DAC"
    );
    let t = VTime::from_ms(10);
    board.i2s_dac(t, PcmFormat::BSP, &[100, -1, 200, -2, 300, -3]);
    assert_eq!(board.codec.format(), AudioFormat::DEMO);
    assert_eq!(board.codec.log.samples(), &[100, 200, 300]);
    let record = board.codec.log.records()[0];
    assert_eq!(record.vt_start, t);
    assert_eq!(record.fs_hz, 16_000);
    assert!(!record.silent);

    // An unchanged format keeps the open record open.
    let next = VTime(t.0 + pemu_core::time::frame_time(VTime(0), 3, 16_000).0);
    board.i2s_dac(next, PcmFormat::BSP, &[400, -4]);
    assert_eq!(board.codec.log.records().len(), 1);
    assert_eq!(board.codec.log.samples(), &[100, 200, 300, 400]);
}

/// The codec captures silence from the board's default source; before the SoC clocks anything
/// the port still returns silence.
#[test]
fn i2s_rx_frames_come_from_the_codec() {
    let mut board = board();
    let mut out = [7i16; 8];
    board.i2s_adc(VTime::from_ms(1), PcmFormat::BSP, &mut out);
    assert_eq!(out, [0; 8]);
    assert_eq!(
        board.codec.format(),
        AudioFormat::DEMO,
        "the codec learned the shape"
    );

    let unclocked = PcmFormat {
        fs_hz: 0,
        bits: 16,
        slots: 2,
    };
    let mut out = [7i16; 4];
    board.i2s_adc(VTime::from_ms(2), unclocked, &mut out);
    assert_eq!(
        out, [0; 4],
        "an unconfigured capture is silence, not stale data"
    );
}

#[test]
fn power_off_resets_the_codec_registers() {
    use pemu_board::power::RailState;
    use pemu_board::traits::BoardCx;
    use pemu_core::input::InputEvent;

    let mut board = board();
    let t = VTime::from_ms(1);
    assert!(write_reg(&mut board, t, CODEC, REG_DAC_VOLUME, &[0xB2]));
    assert_eq!(board.codec.reg(REG_DAC_VOLUME), 0xB2);

    let mut cx = BoardCx::new(VTime(0));
    board.apply_input(
        VTime::from_ms(10),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    let effect = board.tick(VTime::from_ms(2_010), &mut cx);
    assert_eq!(board.rail(), RailState::Off);
    assert!(
        effect
            .cleared
            .contains(&pemu_board::traits::BoardDomain::BoardRail)
    );
    assert_eq!(board.codec.registers(), Es8311::new().registers());
}
