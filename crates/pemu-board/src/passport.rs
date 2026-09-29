//! The AI Passport board: holds every chip, routes the SoC's buses to them, and owns the rail, the
//! cable and the persistence domains. Reset causes follow the ESP32-C3 TRM (Reset and Clock).
//!
//! `wiring::i2s` hands every I2S0 TX period both to [`BoardPorts::i2s_dac`] (the codec's `PcmLog`)
//! and to `HostIo::audio_out`. Nothing drains `PcmLog` into `HostIo::audio_out`, so a consumer
//! reads one, never both; whoever adds that drain must remove the direct write in `wiring::i2s`.

use pemu_core::input::{BatterySet, InputEvent, NfcOp};
use pemu_core::reset::ResetCause;
use pemu_core::rng::DetRng;
use pemu_core::sched::ChipId;
use pemu_core::time::VTime;

use crate::backlight::Backlight;
use crate::battery::{Battery, BatteryConfig};
use crate::cw2017::Cw2017;
use crate::es8311::{AudioFormat, Es8311, SliceSource};
use crate::ladder::{ButtonLadder, LadderConfig};
use crate::ntag213::BoardWorld;
use crate::power::{PowerConfig, PowerEdge, PowerRail, RailState};
use crate::st7789::{PanelConfig, St7789p3};
use crate::traits::{BoardCx, BoardDomain, BoardPorts, Chip, I2cDevice, PcmFormat};
use crate::usb_plug::{LineAction, UsbConfig, UsbHostState, UsbPlug};

/// The [`ChipId`] of the board's own wake-ups (UNVERIFIED numbering: board 0, chips nonzero).
pub const BOARD_CHIP: ChipId = ChipId(0);

/// The tag of the power-hold wake-up at [`PowerRail::deadline`].
pub const POWER_DEADLINE_TAG: u16 = 0;

pub struct PassportBoard {
    pub lcd: St7789p3,
    pub backlight: Backlight,
    pub codec: Es8311,
    pub gauge: Cw2017,
    pub battery: Battery,
    pub ladder: ButtonLadder,
    pub power: PowerRail,
    pub usb: UsbPlug,
    pub world: BoardWorld,
    /// The configuration the board was built from, for `config_hash` and models that read a pin.
    cfg: BoardConfig,
    /// The I2C address of the current transfer, `None` between a stop and the next addressed start.
    i2c_addr: Option<u8>,
}

/// State of charge the unseeded board starts at, in thousandths of a percent: a full cell.
const FULL_SOC_MILLI: u32 = 100_000;

impl PassportBoard {
    /// Builds the unseeded board (seed-0 card); see [`PassportBoard::with_seed`].
    pub fn from_toml(cfg: &BoardConfig) -> Self {
        let battery = Battery::new(
            BatteryConfig {
                capacity_mah: cfg.battery_capacity_mah,
                charge_ma: cfg.battery_charge_ma,
                term_ma: cfg.battery_term_ma,
                cv_mv: cfg.battery_cv_mv,
                ..BatteryConfig::default()
            },
            FULL_SOC_MILLI,
        );
        PassportBoard {
            lcd: St7789p3::new(PanelConfig {
                invon_shows_ram: cfg.invon_shows_ram,
            }),
            backlight: Backlight::new(cfg.backlight_active_high),
            codec: Es8311::new(),
            gauge: Cw2017::provisioned(),
            battery,
            ladder: ButtonLadder::new(cfg.buttons),
            power: PowerRail::new(cfg.power),
            usb: UsbPlug::new(cfg.usb),
            world: BoardWorld::default(),
            cfg: cfg.clone(),
            i2c_addr: None,
        }
    }

    /// The board with its card's UID and signature drawn from the machine seed.
    pub fn with_seed(cfg: &BoardConfig, rng: &mut DetRng) -> Self {
        let mut board = PassportBoard::from_toml(cfg);
        board.world = BoardWorld::new(rng);
        board
    }

    pub fn config(&self) -> &BoardConfig {
        &self.cfg
    }

    /// The I2C address of the transfer in progress, carried by the `board.bus` snapshot section.
    pub fn i2c_target(&self) -> Option<u8> {
        self.i2c_addr
    }

    pub fn set_i2c_target(&mut self, addr: Option<u8>) {
        self.i2c_addr = addr;
    }

    /// Applies one input, already journaled: the only place board state changes for a reason the
    /// guest did not cause.
    pub fn apply_input(&mut self, t: VTime, ev: &InputEvent, cx: &mut BoardCx) -> BoardEffect {
        cx.advance_to(t);
        let before = self.usb();
        let mut effect = BoardEffect::default();
        match ev {
            InputEvent::Button { id, down } => {
                self.ladder.set(*id, *down);
            }
            InputEvent::Power { down } => {
                if let Some(edge) = self.power.hold(t, *down) {
                    self.rail_edge(edge, cx, &mut effect);
                }
                self.arm_power_deadline(cx);
            }
            InputEvent::UsbCable { plugged } => self.usb.set_cable(*plugged),
            InputEvent::UsbClient { open } => self.usb.set_client(*open),
            InputEvent::UsbLine { dtr, rts } => {
                if let LineAction::Reset { strap } = self.usb.set_line(*dtr, *rts, self.cfg.strap) {
                    effect.reset = Some(LineReset {
                        cause: ResetCause::USB_UART_CHIP,
                        strap,
                    });
                }
            }
            InputEvent::NfcTap { ops } => effect.card = Some(self.tap(ops)),
            InputEvent::Battery(set) => self.set_battery(t, set, cx, &mut effect),
            // Not the board's: USJ, audio, Wi-Fi, BLE or machine inputs.
            _ => {}
        }
        let after = self.usb();
        if after != before {
            effect.usb = Some(after);
        }
        effect
    }

    /// Fires a power hold whose deadline has passed; the machine calls this at the scheduled
    /// wake-up, so the edge lands at the mark, not at the release.
    pub fn tick(&mut self, t: VTime, cx: &mut BoardCx) -> BoardEffect {
        cx.advance_to(t);
        let mut effect = BoardEffect::default();
        let before = self.usb();
        if let Some(edge) = self.power.tick(t) {
            self.rail_edge(edge, cx, &mut effect);
        }
        self.arm_power_deadline(cx);
        let after = self.usb();
        if after != before {
            effect.usb = Some(after);
        }
        effect
    }

    pub fn check_brownout(&mut self, cell_mv: u32, cx: &mut BoardCx) -> BoardEffect {
        let mut effect = BoardEffect::default();
        let usb_5v = self.usb.cable();
        if let Some(edge) = self.power.check_brownout(cell_mv, usb_5v) {
            self.rail_edge(edge, cx, &mut effect);
        }
        effect
    }

    /// Asks the machine, through [`BoardCx`], for the wake-up [`PowerRail::tick`] depends on.
    fn arm_power_deadline(&mut self, cx: &mut BoardCx) {
        if let Some(deadline) = self.power.deadline() {
            cx.wake(BOARD_CHIP, deadline, POWER_DEADLINE_TAG);
        }
    }

    fn rail_edge(&mut self, edge: PowerEdge, cx: &mut BoardCx, effect: &mut BoardEffect) {
        for &domain in edge.clears() {
            self.reset_domain(domain);
            effect.cleared.push(domain);
        }
        if edge == PowerEdge::On {
            // The rail coming up is a power-on reset for the SoC.
            effect.reset = Some(LineReset {
                cause: ResetCause::POWERON,
                strap: self.cfg.strap,
            });
        }
        cx.emit(edge.name());
        effect.rail = Some(edge);
    }

    /// Clears one persistence domain; `card` is forwarded to the world. The ladder is in no domain:
    /// a finger on OK while the device powers on is a gesture the model must express.
    pub fn reset_domain(&mut self, domain: BoardDomain) {
        match domain {
            BoardDomain::McuRail => {
                // The SoC's memory is the machine's; nothing board-side is in `mcu_rail`.
            }
            BoardDomain::BoardRail => {
                // The backlight is LEDC-driven and not in this domain.
                self.lcd.reset(domain);
                self.codec.reset(domain);
            }
            BoardDomain::Battery => {
                // Cable, rail, temperature and integration time are not the cell's and stay.
                self.gauge.reset(domain);
                self.battery = self.battery.reconnected(FULL_SOC_MILLI);
            }
            BoardDomain::Card => self.world.reset(domain),
            BoardDomain::Rtc | BoardDomain::Flash => {
                // Both live in the SoC and the loader, above this crate.
            }
            BoardDomain::Host => {
                self.usb.set_cable(false);
                self.usb.reset_line();
            }
        }
    }

    /// Applies `battery.set` and `battery.disconnect()`. `soc`, `mv` and `temp_c_deci` apply in
    /// that order. Whether a disconnected cell drops the rail without USB is not modeled
    /// (UNVERIFIED).
    fn set_battery(
        &mut self,
        t: VTime,
        set: &BatterySet,
        cx: &mut BoardCx,
        effect: &mut BoardEffect,
    ) {
        // Sample against the old cell first, so the set cannot pull the reported SOC by an amount
        // that depends on the time since the last read.
        self.gauge.sample(t, &mut self.battery);
        if set.present == Some(false) {
            self.reset_domain(BoardDomain::Battery);
            effect.cleared.push(BoardDomain::Battery);
        }
        if let Some(soc) = set.soc {
            self.battery.set_soc_milli(u32::from(soc.min(100)) * 1_000);
        }
        if let Some(mv) = set.mv {
            self.battery.set_terminal_mv(u32::from(mv));
        }
        if let Some(temp) = set.temp_c_deci {
            self.battery.set_temp_deci_c(temp);
        }
        if set.soc.is_some() {
            // An explicit charge override reaches the gauge SOC at once.
            self.gauge.override_soc(t, &mut self.battery);
        } else {
            self.gauge.sample(t, &mut self.battery);
        }
        let usb_5v = self.usb.cable();
        if let Some(edge) = self
            .power
            .check_brownout(self.battery.terminal_mv(), usb_5v)
        {
            self.rail_edge(edge, cx, effect);
        }
    }

    fn tap(&mut self, ops: &[NfcOp]) -> TapResult {
        let mut result = TapResult::default();
        for op in ops {
            match op {
                NfcOp::FieldOn => self.world.card.field_on(),
                NfcOp::FieldOff => self.world.card.field_off(),
                NfcOp::Cmd(frame) => {
                    result
                        .responses
                        .push(self.world.card.command(frame).to_bytes());
                }
            }
        }
        result.uid = self.world.card.uid();
        result.counter = self.world.card.counter();
        result
    }
}

impl PassportBoard {
    /// Hands the codec the I2S frame shape only when it changes.
    fn sync_codec_format(&mut self, fmt: PcmFormat) {
        let format = audio_format(fmt);
        if self.codec.format() != format {
            self.codec.set_format(format);
        }
    }

    fn i2c_device_stop(&mut self, t: VTime, addr: u8) {
        if addr == self.cfg.gauge_addr {
            self.gauge.stop(t);
        } else if addr == self.cfg.codec_addr {
            self.codec.stop(t);
        }
    }
}

/// The codec's view of the I2S frame shape, with the slot count as the channel count.
fn audio_format(fmt: PcmFormat) -> AudioFormat {
    AudioFormat {
        fs_hz: fmt.fs_hz,
        bits: fmt.bits,
        channels: u16::from(fmt.slots),
    }
}

impl BoardPorts for PassportBoard {
    /// SPI2 carries only the panel (chip select GPIO1, D/C on GPIO20).
    fn spi2(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool) {
        self.lcd.transfer(t, dc, data, cs_release);
    }

    /// Every address but 0x18 and 0x63 NACKs, which `bsp_i2c_scan` depends on. A repeated start to
    /// the other chip ends the first chip's transaction as a stop would (UNVERIFIED: the BSP never
    /// does it).
    fn i2c_start(&mut self, t: VTime, addr: u8, read: bool) -> bool {
        if let Some(prev) = self.i2c_addr.filter(|prev| *prev != addr) {
            self.i2c_device_stop(t, prev);
        }
        if addr == self.cfg.gauge_addr {
            self.gauge.sample(t, &mut self.battery);
            self.i2c_addr = Some(addr);
            self.gauge.start(t, read)
        } else if addr == self.cfg.codec_addr {
            self.i2c_addr = Some(addr);
            self.codec.start(t, read)
        } else {
            self.i2c_addr = None;
            false
        }
    }

    /// One written byte to the addressed chip; with none addressed nothing ACKs.
    fn i2c_write(&mut self, t: VTime, byte: u8) -> bool {
        match self.i2c_addr {
            Some(addr) if addr == self.cfg.gauge_addr => self.gauge.write(t, byte),
            Some(addr) if addr == self.cfg.codec_addr => self.codec.write(t, byte),
            _ => false,
        }
    }

    /// One read byte from the addressed chip; with none addressed the pulled-up bus reads 0xFF.
    fn i2c_read(&mut self, t: VTime) -> u8 {
        match self.i2c_addr {
            Some(addr) if addr == self.cfg.gauge_addr => self.gauge.read(t),
            Some(addr) if addr == self.cfg.codec_addr => self.codec.read(t),
            _ => 0xFF,
        }
    }

    fn i2c_stop(&mut self, t: VTime) {
        if let Some(addr) = self.i2c_addr.take() {
            self.i2c_device_stop(t, addr);
        }
    }

    /// A changed `fmt` is handed to the codec first, closing the open `PcmLog` run.
    fn i2s_dac(&mut self, t: VTime, fmt: PcmFormat, frames: &[i16]) {
        self.sync_codec_format(fmt);
        self.codec.play(t, frames);
    }

    /// The codec captures silence (the board holds no host source; host audio reaches the guest in
    /// `wiring::i2s`). `out` is zeroed first because an unconfigured codec leaves it alone.
    fn i2s_adc(&mut self, t: VTime, fmt: PcmFormat, out: &mut [i16]) {
        out.fill(0);
        self.sync_codec_format(fmt);
        self.codec.capture(t, &mut SliceSource::default(), out);
    }

    /// Only the ladder is wired; other channels float at the rail, not 0 mV, which would read as
    /// UP.
    fn adc_mv(&self, t: VTime, unit: u8, channel: u8) -> u32 {
        let _ = t;
        let cfg = self.ladder.config();
        if unit == cfg.adc_unit && channel == cfg.adc_channel {
            self.ladder.adc_mv()
        } else {
            cfg.released_mv
        }
    }

    /// A GPIO output level. Only GPIO20 (panel D/C) matters, and the SPI2 transfer samples it.
    fn gpio_out(&mut self, t: VTime, pin: u8, level: bool, oe: bool) {
        let _ = (t, pin, level, oe);
    }

    /// A GPIO input level while the chip runs: GPIO0 is the ladder; nothing else drives an input.
    fn gpio_in(&self, t: VTime, pin: u8) -> bool {
        let _ = t;
        if pin == self.ladder.config().gpio {
            !self.ladder.reads_low()
        } else {
            false
        }
    }

    fn ledc(&mut self, t: VTime, channel: u8, duty: u32, duty_res: u8, freq_hz: u32) {
        self.backlight.ledc(t, channel, duty, duty_res, freq_hz);
        self.lcd.set_backlight(&self.backlight);
    }

    fn rail(&self) -> RailState {
        self.power.state()
    }

    fn usb(&self) -> UsbHostState {
        self.usb.state(self.power.state())
    }
}

/// Board configuration parsed from `boards/ai-passport.toml`; also `MachineConfig::board`. Chip
/// rows are plain numbers, not chip structs, so adding a chip field never changes the
/// `config_hash` of an unchanged board. No serde on this type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BoardConfig {
    pub xtal_hz: u32,
    pub slow_clk_hz: u32,
    pub flash_size_mb: u32,
    /// `[soc] flash.jedec`, the three JEDEC id bytes.
    pub flash_jedec: [u8; 3],
    pub strap: u8,
    pub display_width: u16,
    pub display_height: u16,
    pub display_cs_gpio: u8,
    pub display_dc_gpio: u8,
    /// `[display] hz`, the SPI clock.
    pub display_hz: u32,
    pub invon_shows_ram: bool,
    pub backlight_gpio: u8,
    pub backlight_channel: u8,
    pub backlight_active_high: bool,
    pub i2c_sda_gpio: u8,
    pub i2c_scl_gpio: u8,
    /// The ES8311's 7-bit address, from the `[i2c0] devices` list.
    pub codec_addr: u8,
    /// The CW2017's 7-bit address, from the `[i2c0] devices` list.
    pub gauge_addr: u8,
    /// `[i2s0]` pins: MCLK, BCLK, WS, DOUT, DIN.
    pub i2s_gpio: [u8; 5],
    pub buttons: LadderConfig,
    pub power: PowerConfig,
    pub battery_capacity_mah: u32,
    /// `[battery] charge_ma`; UNVERIFIED.
    pub battery_charge_ma: u32,
    /// `[battery] term_ma`; UNVERIFIED.
    pub battery_term_ma: u32,
    pub battery_cv_mv: u32,
    /// `[gauge] version_reg`; the device reads 0x0F where the datasheet default is 0xA0.
    pub gauge_version_reg: u8,
    pub usb: UsbConfig,
}

impl Default for BoardConfig {
    /// `boards/ai-passport.toml` exactly; `tests/domains.rs` checks the two agree.
    fn default() -> Self {
        BoardConfig {
            xtal_hz: 40_000_000,
            slow_clk_hz: 136_000,
            flash_size_mb: 8,
            flash_jedec: [0x20, 0x40, 0x17],
            strap: 0x0A,
            display_width: 240,
            display_height: 320,
            display_cs_gpio: 1,
            display_dc_gpio: 20,
            display_hz: 40_000_000,
            invon_shows_ram: true,
            backlight_gpio: 21,
            backlight_channel: 0,
            backlight_active_high: true,
            i2c_sda_gpio: 10,
            i2c_scl_gpio: 7,
            codec_addr: 0x18,
            gauge_addr: 0x63,
            i2s_gpio: [6, 5, 3, 2, 4],
            buttons: LadderConfig::default(),
            power: PowerConfig::default(),
            battery_capacity_mah: 520,
            battery_charge_ma: 200,
            battery_term_ma: 20,
            battery_cv_mv: 4_200,
            gauge_version_reg: 0x0F,
            usb: UsbConfig::default(),
        }
    }
}

/// A reset the board asked the SoC for: the cause the ROM reports and the strap latched with it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct LineReset {
    /// The reset cause; `POWERON` (0x01) for a rail edge, `USB_UART_CHIP` (0x15) for a line reset.
    pub cause: ResetCause,
    /// The value `GPIO_STRAP` latches.
    pub strap: u8,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct TapResult {
    /// One response per [`NfcOp::Cmd`], in order.
    pub responses: Vec<Vec<u8>>,
    /// The card's UID, which every real tap reads during anticollision.
    pub uid: [u8; 7],
    pub counter: u32,
}

/// Effect of `PassportBoard::apply_input` for the machine to act on.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct BoardEffect {
    pub rail: Option<PowerEdge>,
    pub reset: Option<LineReset>,
    pub usb: Option<UsbHostState>,
    pub cleared: Vec<BoardDomain>,
    pub card: Option<TapResult>,
}

#[cfg(test)]
mod snapshot_fields {
    use super::*;

    /// No `..` in the pattern: a new board field fails to compile until it gets a snapshot section
    /// (`snapshot.rs` `BOARD_CHIPS`) or is named here as configuration.
    #[test]
    fn every_board_field_has_a_snapshot_section() {
        let board = PassportBoard::from_toml(&BoardConfig::default());
        let PassportBoard {
            lcd: _,
            backlight: _,
            codec: _,
            gauge: _,
            battery: _,
            ladder: _,
            power: _,
            usb: _,
            world: _,
            // `board.bus`.
            i2c_addr: _,
            // Configuration: part of the identity hash, never a section.
            cfg: _,
        } = &board;
    }
}
