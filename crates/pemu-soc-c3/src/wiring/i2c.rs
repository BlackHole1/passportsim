//! `Wiring::I2cRun`: run the I2C0 command list against the board (`specs/blocks/i2c0.toml`).
//!
//! The whole list runs inside the access that started it (row `i2c0.trans_complete_or_nack`,
//! `within = same_access`), so every board call carries that access's virtual time.

use pemu_board::traits::BoardPorts;
use pemu_core::time::VTime;

use crate::periph::i2c0::{Deferred, I2cBus, Model, RunEnd};

/// Runs the loaded command list of `i2c` against `board` at `now`, and returns how it ended.
pub fn run(i2c: &mut Model, now: VTime, board: &mut dyn BoardPorts) -> RunEnd {
    let mut bus = Ports { now, board };
    i2c.run(&mut bus)
}

/// [`run`] under a timing profile: with `clocked`, the end of the list is held back and returned
/// as a [`Deferred`] for the machine to deliver at `now` plus the list's bus time
/// (`Model::run_timed`).
pub fn run_timed(
    i2c: &mut Model,
    now: VTime,
    board: &mut dyn BoardPorts,
    clocked: bool,
) -> (RunEnd, Deferred) {
    let mut bus = Ports { now, board };
    i2c.run_timed(&mut bus, clocked)
}

/// The board behind [`I2cBus`], with the virtual time of the access bound to it.
struct Ports<'a> {
    now: VTime,
    board: &'a mut dyn BoardPorts,
}

impl I2cBus for Ports<'_> {
    fn start(&mut self, addr: u8, read: bool) -> bool {
        self.board.i2c_start(self.now, addr, read)
    }

    fn write(&mut self, byte: u8) -> bool {
        self.board.i2c_write(self.now, byte)
    }

    fn read(&mut self) -> u8 {
        self.board.i2c_read(self.now)
    }

    fn stop(&mut self) {
        self.board.i2c_stop(self.now);
    }
}
