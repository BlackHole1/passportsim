//! I2C bus transcripts and their replayer.
//!
//! Every BSP I2C access is one of three IDF `components/esp_driver_i2c/i2c_master.c` command
//! lists: a probe (`RESTART; WRITE addr; STOP`), a one-register write (`addr<<1, reg, val`) and a
//! read (pointer write, restart, `n` read bytes). A transcript is one line per transaction and
//! replays against a chip model here or through the SoC I2C0 command executor.
//!
//! ```text
//! # comment
//! P 18 ack           probe address 0x18, expect an ACK (`nack` for an absent address)
//! W 18 0D FA         write value 0xFA to register 0x0D
//! R 18 0D FC         read register 0x0D, expect 0xFC (more bytes mean a multi-byte read)
//! D 20000            let 20000 us of virtual time pass
//! ```

use pemu_core::time::VTime;

use crate::traits::I2cDevice;

/// One transaction of a transcript.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TxOp {
    /// `i2c_master_probe`: the address byte alone, with the acknowledgement it must get.
    Probe {
        addr: u8,
        /// Whether the address must be acknowledged.
        ack: bool,
    },
    /// `i2c_master_transmit`: a register pointer followed by the bytes written to it.
    Write {
        addr: u8,
        reg: u8,
        /// Bytes written after the pointer; the BSP always writes exactly one.
        values: Vec<u8>,
    },
    /// `i2c_master_transmit_receive`: a register pointer, a restart, and the bytes read back.
    Read {
        addr: u8,
        reg: u8,
        /// Bytes the transcript expects the chip to return.
        expect: Vec<u8>,
    },
    /// Virtual time passing between transactions (the BSP's `vTaskDelay`), in microseconds.
    Delay { us: u64 },
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Transcript {
    pub ops: Vec<TxOp>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseError {
    pub line: usize,
    pub detail: &'static str,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReplayError {
    /// Index of the failing transaction in [`Transcript::ops`].
    pub op: usize,
    pub detail: &'static str,
    pub got: u8,
    pub want: u8,
}

impl Transcript {
    /// Parses a transcript file. Blank lines and `#` comments are ignored.
    pub fn parse(text: &str) -> Result<Transcript, ParseError> {
        let mut ops = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let body = raw.split('#').next().unwrap_or("").trim();
            if body.is_empty() {
                continue;
            }
            let mut words = body.split_whitespace();
            let kind = words.next().unwrap_or_default();
            let mut byte = |detail: &'static str| -> Result<u8, ParseError> {
                let word = words.next().ok_or(ParseError { line, detail })?;
                u8::from_str_radix(word, 16).map_err(|_| ParseError { line, detail })
            };
            let op = match kind {
                "P" => {
                    let addr = byte("probe needs an address")?;
                    let ack = match words.next() {
                        Some("ack") => true,
                        Some("nack") => false,
                        _ => {
                            return Err(ParseError {
                                line,
                                detail: "probe needs `ack` or `nack`",
                            });
                        }
                    };
                    TxOp::Probe { addr, ack }
                }
                "W" | "R" => {
                    let addr = byte("transaction needs an address")?;
                    let reg = byte("transaction needs a register")?;
                    let mut rest = Vec::new();
                    for word in words {
                        rest.push(u8::from_str_radix(word, 16).map_err(|_| ParseError {
                            line,
                            detail: "byte is not two hex digits",
                        })?);
                    }
                    if rest.is_empty() {
                        return Err(ParseError {
                            line,
                            detail: "transaction needs at least one data byte",
                        });
                    }
                    if kind == "W" {
                        TxOp::Write {
                            addr,
                            reg,
                            values: rest,
                        }
                    } else {
                        TxOp::Read {
                            addr,
                            reg,
                            expect: rest,
                        }
                    }
                }
                "D" => {
                    let word = words.next().ok_or(ParseError {
                        line,
                        detail: "delay needs a microsecond count",
                    })?;
                    let us = word.parse::<u64>().map_err(|_| ParseError {
                        line,
                        detail: "delay is not a decimal microsecond count",
                    })?;
                    TxOp::Delay { us }
                }
                _ => {
                    return Err(ParseError {
                        line,
                        detail: "line does not start with P, W, R or D",
                    });
                }
            };
            ops.push(op);
        }
        Ok(Transcript { ops })
    }

    /// The `(register, value)` pairs of every write, in bus order, for comparison against
    /// `specs/es8311-sequences.toml`.
    pub fn writes(&self) -> Vec<(u8, u8)> {
        let mut trace = Vec::new();
        for op in &self.ops {
            if let TxOp::Write { reg, values, .. } = op {
                for (offset, value) in values.iter().enumerate() {
                    trace.push((reg.wrapping_add(offset as u8), *value));
                }
            }
        }
        trace
    }

    /// Replays the transcript against one chip from `t0`, skipping other addresses; returns the end
    /// time or the first mismatch.
    pub fn replay<D: I2cDevice>(&self, chip: &mut D, t0: VTime) -> Result<VTime, ReplayError> {
        let mut t = t0;
        for (index, op) in self.ops.iter().enumerate() {
            let fail = |detail, got, want| ReplayError {
                op: index,
                detail,
                got,
                want,
            };
            match op {
                TxOp::Delay { us } => t = VTime(t.0.saturating_add(VTime::from_us(*us).0)),
                TxOp::Probe { addr, ack } => {
                    if *addr != chip.address() {
                        // An absent address is the bus's answer, not this chip's.
                        continue;
                    }
                    let got = chip.start(t, false);
                    chip.stop(t);
                    if got != *ack {
                        return Err(fail("probe acknowledgement", u8::from(got), u8::from(*ack)));
                    }
                }
                TxOp::Write { addr, reg, values } => {
                    if *addr != chip.address() {
                        continue;
                    }
                    if !chip.start(t, false) {
                        return Err(fail("write address not acknowledged", 0, 1));
                    }
                    if !chip.write(t, *reg) {
                        return Err(fail("register pointer not acknowledged", 0, 1));
                    }
                    for value in values {
                        if !chip.write(t, *value) {
                            return Err(fail("data byte not acknowledged", 0, 1));
                        }
                    }
                    chip.stop(t);
                }
                TxOp::Read { addr, reg, expect } => {
                    if *addr != chip.address() {
                        continue;
                    }
                    if !chip.start(t, false) {
                        return Err(fail("read address not acknowledged", 0, 1));
                    }
                    if !chip.write(t, *reg) {
                        return Err(fail("register pointer not acknowledged", 0, 1));
                    }
                    // No stop between the pointer write and the reads.
                    if !chip.start(t, true) {
                        return Err(fail("restart not acknowledged", 0, 1));
                    }
                    for want in expect {
                        let got = chip.read(t);
                        if got != *want {
                            return Err(fail("read byte", got, *want));
                        }
                    }
                    chip.stop(t);
                }
            }
        }
        Ok(t)
    }
}
