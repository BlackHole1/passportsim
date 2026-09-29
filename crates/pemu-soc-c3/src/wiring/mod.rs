//! Cross-block effects: one file per `Wiring` variant a peripheral access returns, the only
//! place two blocks, or a block and the board, meet (`specs/blocks/`).

pub mod adc;
pub mod aes;
pub mod clock;
pub mod flash;
pub mod gates;
pub mod gpio;
pub mod i2c;
pub mod i2s;
pub mod mmu;
pub mod protection;
pub mod reset;
pub mod sha;
pub mod spi2;
pub mod usj;
