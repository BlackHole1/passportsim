//! The ES8311 audio codec model.

use pemu_board::es8311::{
    ADDRESS, AudioFormat, AudioMode, Es8311, RESET_VALUES, SliceSource, VOLUME_0DB, volume_register,
};
use pemu_board::i2c_transcript::{Transcript, TxOp};
use pemu_board::traits::{BoardDomain, Chip, I2cDevice, I2sCodec, PcmFormat};
use pemu_core::time::VTime;

const SEQUENCES: &str = include_str!("../../../specs/es8311-sequences.toml");
const TRANSCRIPT_A: &str = include_str!("../../../tests/transcripts/i2c/es8311-a-open.txt");
const TRANSCRIPT_B: &str = include_str!("../../../tests/transcripts/i2c/es8311-b-set-format.txt");
const TRANSCRIPT_C: &str = include_str!("../../../tests/transcripts/i2c/es8311-c-set-volume.txt");
const TRANSCRIPT_D: &str = include_str!("../../../tests/transcripts/i2c/es8311-d-close.txt");
const PROBES: &str = include_str!("../../../tests/transcripts/i2c/i2c-boot-probe.txt");

#[derive(PartialEq, Eq, Debug)]
struct SpecOp {
    write: bool,
    reg: u8,
    value: u8,
}

/// The ops of one sequence of the spec file. `pemu-board` may not depend on `toml`, so this reads
/// the one shape the file uses (`[[sequence]]`, `id`, one inline table per line in `ops`) and
/// panics on anything else rather than silently matching nothing.
fn spec_sequence(id: &str) -> Vec<SpecOp> {
    let header = format!("id = \"{id}\"");
    let block = SEQUENCES
        .split("[[sequence]]")
        .find(|block| block.lines().any(|line| line.trim() == header))
        .unwrap_or_else(|| panic!("specs/es8311-sequences.toml has no sequence {id}"));
    let body = block
        .split_once("ops = [")
        .unwrap_or_else(|| panic!("sequence {id} has no ops list"))
        .1;
    let body = body
        .split_once(']')
        .unwrap_or_else(|| panic!("sequence {id} has an unterminated ops list"))
        .0;
    let ops: Vec<SpecOp> = body
        .split("{")
        .skip(1)
        .map(|entry| {
            let entry = entry.split('}').next().unwrap_or_default();
            let field = |name: &str| -> String {
                entry
                    .split(',')
                    .find_map(|part| {
                        let (key, value) = part.split_once('=')?;
                        (key.trim() == name).then(|| value.trim().trim_matches('"').to_string())
                    })
                    .unwrap_or_else(|| panic!("sequence {id} op has no `{name}`: {entry}"))
            };
            let hex = |name: &str| -> u8 {
                let raw = field(name);
                let digits = raw.strip_prefix("0x").unwrap_or(&raw);
                u8::from_str_radix(digits, 16)
                    .unwrap_or_else(|_| panic!("sequence {id} op has a bad `{name}`: {raw}"))
            };
            SpecOp {
                write: field("op") == "write",
                reg: hex("reg"),
                value: hex("value"),
            }
        })
        .collect();
    assert!(!ops.is_empty(), "sequence {id} has no ops");
    ops
}

fn transcript_ops(text: &str) -> Vec<SpecOp> {
    let transcript = Transcript::parse(text).expect("transcript parses");
    transcript
        .ops
        .iter()
        .filter_map(|op| match op {
            TxOp::Write { reg, values, .. } => Some(SpecOp {
                write: true,
                reg: *reg,
                value: values[0],
            }),
            TxOp::Read { reg, expect, .. } => Some(SpecOp {
                write: false,
                reg: *reg,
                value: expect[0],
            }),
            TxOp::Probe { .. } | TxOp::Delay { .. } => None,
        })
        .collect()
}

fn apply_spec(codec: &mut Es8311, ops: &[SpecOp]) {
    for op in ops {
        if op.write {
            codec.write_reg(op.reg, op.value);
        } else {
            assert_eq!(
                codec.reg(op.reg),
                op.value,
                "spec op expects register {:#04X} to read {:#04X}",
                op.reg,
                op.value
            );
        }
    }
}

/// A codec after sequences A and B, the state the firmware reaches before the audio demo sets a
/// volume.
fn opened() -> Es8311 {
    let mut codec = Es8311::new();
    let t = Transcript::parse(TRANSCRIPT_A).expect("A parses");
    t.replay(&mut codec, VTime(0)).expect("A replays");
    let t = Transcript::parse(TRANSCRIPT_B).expect("B parses");
    t.replay(&mut codec, VTime(0)).expect("B replays");
    codec
}

#[test]
fn reset_values_match_the_datasheet_table() {
    let codec = Es8311::new();
    for &(reg, value) in RESET_VALUES {
        assert_eq!(codec.reg(reg), value, "register {reg:#04X}");
    }
    // Every register the table does not name resets to zero, including the ranged runs.
    for reg in 0..=u8::MAX {
        if RESET_VALUES.iter().any(|&(r, _)| r == reg) {
            continue;
        }
        assert_eq!(
            codec.reg(reg),
            0,
            "register {reg:#04X} should reset to zero"
        );
    }
    assert_eq!(codec.registers().len(), 256);
}

#[test]
fn the_codec_answers_the_boot_scan() {
    let mut codec = Es8311::new();
    let probes = Transcript::parse(PROBES).expect("probe transcript parses");
    probes
        .replay(&mut codec, VTime(0))
        .expect("the codec acknowledges its own address and ignores the gauge's");
    assert_eq!(codec.address(), ADDRESS);
}

#[test]
fn sequence_a_replays_against_a_fresh_chip() {
    let mut codec = Es8311::new();
    let a = Transcript::parse(TRANSCRIPT_A).expect("A parses");
    // Sequence A: 24 transactions on a fresh chip.
    assert_eq!(a.ops.len(), 24);
    a.replay(&mut codec, VTime(0))
        .expect("every read of sequence A returns what the transcript expects");
    // Slave mode with every clock on, the analog block still powered down, ADC data on both slots.
    assert_eq!(codec.reg(0x00), 0x80);
    assert_eq!(codec.reg(0x01), 0x3F);
    assert_eq!(codec.reg(0x0D), 0xFA);
    assert_eq!(codec.reg(0x44), 0x08);
    assert!(
        !codec.dac_state().active,
        "the DAC is not started by open alone"
    );
}

#[test]
fn sequence_a_and_b_equality() {
    // The spec's op list and the bus transcript must be the same ops in the same order, and
    // replaying either must leave the same register file.
    let spec_a = spec_sequence("A");
    let spec_b = spec_sequence("B");
    assert_eq!(spec_a, transcript_ops(TRANSCRIPT_A));
    assert_eq!(spec_b, transcript_ops(TRANSCRIPT_B));

    let mut from_spec = Es8311::new();
    apply_spec(&mut from_spec, &spec_a);
    apply_spec(&mut from_spec, &spec_b);

    let from_bus = opened();
    assert_eq!(from_spec.registers(), from_bus.registers());
}

#[test]
fn volume_80_writes_register_0x32_as_0xb2() {
    let mut codec = opened();
    let c = Transcript::parse(TRANSCRIPT_C).expect("C parses");
    c.replay(&mut codec, VTime(0)).expect("C replays");
    assert_eq!(codec.reg(0x32), 0xB2, "volume 80 gives 0xB2");
    assert_eq!(codec.dac_state().volume_reg, 0xB2);
    // 0xB2 is 13 half-decibel steps below 0 dB, that is -6.5 dB.
    assert_eq!(codec.dac_state().volume_half_db, -13);
}

#[test]
fn volume_register_matches_the_driver_table() {
    // Every worked row of the driver's volume table.
    for &(percent, reg) in &[
        (0u8, 0x06u8),
        (10, 0x6C),
        (50, 0x94),
        (80, 0xB2),
        (100, 0xC6),
    ] {
        assert_eq!(volume_register(percent), reg, "volume {percent}");
    }
    for percent in 1..100u8 {
        assert!(volume_register(percent) < volume_register(percent + 1));
    }
}

#[test]
fn derived_state_after_open_and_format() {
    let codec = opened();
    let dac = codec.dac_state();
    assert!(dac.active, "sequence B starts the DAC");
    assert!(!dac.muted);
    assert!(
        dac.left_slot,
        "SDP_IN_SEL is clear, so the DAC takes slot 0"
    );
    // The first open writes volume 0, which is register 0x06.
    assert_eq!(dac.volume_reg, 0x06);

    let adc = codec.adc_state();
    assert!(adc.active);
    // Sequence D: analog PGA +30 dB, digital scale +30 dB, ADC volume 0 dB.
    assert_eq!(adc.pga_db, 30);
    assert_eq!(adc.scale_db, 30);
    assert_eq!(adc.volume_half_db, 0);
    assert_eq!(adc.adcdat_sel, 0, "ADC data goes to both slots");

    assert_eq!(codec.word_length(), Some(16));
    assert!(codec.is_i2s_format());
}

#[test]
fn sequence_d_silences_both_paths() {
    let mut codec = opened();
    Transcript::parse(TRANSCRIPT_C)
        .expect("C parses")
        .replay(&mut codec, VTime(0))
        .expect("C replays");
    Transcript::parse(TRANSCRIPT_D)
        .expect("D parses")
        .replay(&mut codec, VTime(0))
        .expect("D replays");
    assert!(!codec.dac_state().active, "suspend powers the DAC down");
    assert!(!codec.adc_state().active, "suspend powers the ADC down");
    assert_eq!(codec.reg(0x00), 0x1F, "the state machine is off again");
    assert_eq!(codec.reg(0x32), 0x00, "suspend zeroes the volume register");
}

#[test]
fn ini_reg_resets_every_other_register() {
    let mut codec = opened();
    codec.write_reg(0xFA, 0x01);
    assert_eq!(codec.reg(0xFA), 0x01, "0xFA keeps the value just written");
    for &(reg, value) in RESET_VALUES {
        assert_eq!(codec.reg(reg), value, "register {reg:#04X} after INI_REG");
    }
    codec.write_reg(0x32, 0xB2);
    codec.write_reg(0xFA, 0x02);
    assert_eq!(codec.reg(0x32), 0xB2);
}

#[test]
fn read_only_registers_ignore_writes() {
    let mut codec = Es8311::new();
    for reg in [0xFCu8, 0xFD, 0xFE, 0xFF] {
        let before = codec.reg(reg);
        codec.write_reg(reg, 0x5A);
        assert_eq!(codec.reg(reg), before, "register {reg:#04X} is read only");
    }
    assert_eq!(codec.reg(0xFD), 0x83);
    assert_eq!(codec.reg(0xFE), 0x11);
}

#[test]
fn a_rail_drop_returns_the_codec_to_its_reset_values() {
    let mut codec = opened();
    assert_ne!(codec.reg(0x00), 0x1F);
    codec.reset(BoardDomain::Battery);
    assert_ne!(codec.reg(0x00), 0x1F, "the gauge domain does not reach it");
    codec.reset(BoardDomain::McuRail);
    assert_ne!(
        codec.reg(0x00),
        0x1F,
        "an MCU reset with the rail up does not reach it"
    );
    codec.reset(BoardDomain::BoardRail);
    assert_eq!(codec.registers(), Es8311::new().registers());
}

#[test]
fn digital_playback_records_the_transmitted_samples() {
    let mut codec = opened();
    Transcript::parse(TRANSCRIPT_C)
        .expect("C parses")
        .replay(&mut codec, VTime(0))
        .expect("C replays");
    codec.set_format(AudioFormat::DEMO);
    // Two stereo frames per pair: the DAC takes the left slot.
    let frames = [100i16, -1, 200, -2, -300, -3, 32767, -4];
    let written = codec.play(VTime::from_ms(1), &frames);
    assert_eq!(written, 4);
    assert_eq!(codec.log.samples(), &[100, 200, -300, 32767]);
    let record = codec.log.records().first().expect("one record");
    assert_eq!(record.vt_start, VTime::from_ms(1));
    assert_eq!(record.fs_hz, 16_000);
    assert_eq!(record.channels, 1);
    assert_eq!(record.mode, AudioMode::Digital);
    assert_eq!(
        record.volume_reg, 0xB2,
        "the volume travels as metadata, not as gain"
    );
    assert!(!record.silent);
}

#[test]
fn a_muted_dac_records_silence_and_keeps_the_sample_count() {
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    // Muting sets mask 0x60 of register 0x31.
    codec.write_reg(0x31, 0x60);
    let written = codec.play(VTime(0), &[1000, 0, 2000, 0]);
    assert_eq!(written, 2);
    assert_eq!(codec.log.samples(), &[0, 0]);
    assert!(codec.log.records()[0].silent);
}

#[test]
fn an_unconfigured_i2s_clock_records_nothing() {
    let mut codec = opened();
    assert_eq!(codec.format(), AudioFormat::UNCONFIGURED);
    assert_eq!(codec.play(VTime(0), &[1, 2, 3, 4]), 0);
    assert!(codec.log.samples().is_empty());
}

#[test]
fn analog_playback_applies_the_dac_gain() {
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    codec.set_mode(AudioMode::Analog).expect("no open run yet");
    codec.write_reg(0x32, VOLUME_0DB);
    codec.play(VTime(0), &[1000, 0, -1000, 0]);
    assert_eq!(codec.log.samples(), &[1000, -1000]);

    // -6 dB is 12 half-decibel steps down, an amplitude ratio just above one half.
    codec.log.clear();
    codec.write_reg(0x32, VOLUME_0DB - 12);
    codec.play(VTime(0), &[1000, 0]);
    let scaled = codec.log.samples()[0];
    assert!(
        (500..=510).contains(&scaled),
        "-6 dB of 1000 should be about 501, got {scaled}"
    );
}

#[test]
fn a_mode_change_is_refused_while_a_run_is_open() {
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    codec.play(VTime(0), &[10, 0, 20, 0]);
    let refused = codec
        .set_mode(AudioMode::Analog)
        .expect_err("a run is open");
    assert_eq!(refused.requested, AudioMode::Analog);
    assert_eq!(codec.mode(), AudioMode::Digital);
    codec.log.end_stream();
    codec.set_mode(AudioMode::Analog).expect("the run ended");
    assert_eq!(codec.mode(), AudioMode::Analog);
    codec.play(VTime::from_ms(1), &[10, 0]);
    codec.set_mode(AudioMode::Analog).expect("same mode");
}

#[test]
fn capture_returns_the_source_samples_in_both_slots() {
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    let mut source = SliceSource::new(&[7, -7, 21]);
    let mut out = [0i16; 8];
    let got = codec.capture(VTime(0), &mut source, &mut out);
    assert_eq!(got, 3, "the source ran dry after three samples");
    // ADCDAT_SEL 0 puts the same sample in both slots; missing source samples are silence.
    assert_eq!(out, [7, 7, -7, -7, 21, 21, 0, 0]);
    assert_eq!(source.remaining(), 0);
}

#[test]
fn a_powered_down_adc_captures_silence() {
    let mut codec = Es8311::new();
    codec.set_format(AudioFormat::DEMO);
    assert!(!codec.adc_state().active);
    let mut source = SliceSource::new(&[9, 9, 9, 9]);
    let mut out = [1i16; 4];
    assert_eq!(codec.capture(VTime(0), &mut source, &mut out), 0);
    assert_eq!(out, [0, 0, 0, 0]);
    assert_eq!(source.remaining(), 4, "a silent path draws no samples");
}

#[test]
fn the_mclk_check_is_the_256_times_fs_rule() {
    // 16 kHz means MCLK 4.096 MHz.
    assert_eq!(Es8311::expected_mclk(16_000), 4_096_000);
    assert_eq!(Es8311::check_mclk(16_000, 4_096_000), None);
    let warning = Es8311::check_mclk(16_000, 3_072_000).expect("a mismatch is reported");
    assert_eq!(warning.expected_mclk_hz, 4_096_000);
    assert_eq!(warning.mclk_hz, 3_072_000);
}

#[test]
fn the_log_drops_the_oldest_samples_when_it_fills() {
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    let capacity = codec.log.capacity();
    let frames: Vec<i16> = (0..(capacity as i32 + 8) * 2)
        .map(|n| (n / 2) as i16)
        .collect();
    codec.play(VTime(0), &frames);
    assert_eq!(codec.log.samples().len(), capacity);
    assert_eq!(codec.log.dropped(), 8);
    assert_eq!(codec.log.samples()[0], 8);
}

#[test]
fn an_unconfigured_i2s_clock_captures_nothing() {
    // The mirror of `an_unconfigured_i2s_clock_records_nothing`: an I2S slave never consumes host
    // samples at an undefined rate.
    let mut codec = opened();
    assert_eq!(codec.format(), AudioFormat::UNCONFIGURED);
    let mut source = SliceSource::new(&[7, -7]);
    let mut out = [5i16; 4];
    assert_eq!(codec.capture(VTime(0), &mut source, &mut out), 0);
    assert_eq!(out, [5, 5, 5, 5], "the buffer is left as the caller had it");
    assert_eq!(source.remaining(), 2, "no host sample was drained");
}

#[test]
fn a_gap_in_virtual_time_opens_a_new_record() {
    // A record is read back as `vt_start` plus a sample index, so a burst after a gap cannot extend
    // the record before it.
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    // 160 stereo frames at 16 kHz is exactly 10 ms.
    let frames: Vec<i16> = (0..320).map(|n| (n / 2) as i16).collect();

    codec.play(VTime(0), &frames);
    assert_eq!(codec.log.records().len(), 1);
    codec.play(VTime::from_ms(10), &frames);
    assert_eq!(codec.log.records().len(), 1, "contiguous audio is one run");
    assert_eq!(codec.log.records()[0].vt_start, VTime(0));

    codec.play(VTime::from_ms(5_000), &frames);
    let records = codec.log.records();
    assert_eq!(records.len(), 2, "the gap opened a record");
    assert_eq!(records[1].vt_start, VTime::from_ms(5_000));
    assert_eq!(
        records[1].first, 320,
        "the new run starts after 320 samples"
    );
    // Every recorded sample still maps back to the instant it played.
    assert_eq!(codec.log.samples().len(), 480);
}

#[test]
fn the_codec_implements_the_frozen_i2s_trait() {
    // The frame shape arrives both through `set_format` and as the trait's `PcmFormat` argument.
    let mut codec = opened();
    codec.set_format(AudioFormat::DEMO);
    I2sCodec::dac(
        &mut codec,
        VTime::from_ms(1),
        PcmFormat::BSP,
        &[100, -1, 200, -2],
    );
    assert_eq!(codec.log.samples(), &[100, 200]);
    assert_eq!(codec.log.records()[0].vt_start, VTime::from_ms(1));

    // The trait carries no `PcmSource`, so this path delivers silence.
    let mut out = [7i16; 4];
    I2sCodec::adc(&mut codec, VTime::from_ms(2), PcmFormat::BSP, &mut out);
    assert_eq!(out, [0, 0, 0, 0]);
}
