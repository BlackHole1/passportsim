//! Journaled inputs reaching the models when their instant comes, and the board effects they
//! and the board's own timers report.

use pemu_board::passport::BoardEffect;
use pemu_board::power::PowerEdge;
use pemu_board::traits::BoardCx;
use pemu_core::hostio::{EventKind, HostEvent};
use pemu_core::input::{EnvChange, InputEvent};
use pemu_core::reset::{ResetCause, ResetKind};
use pemu_core::sched::{EventKey, Owner};
use pemu_core::snap::SnapValue;
use pemu_soc_c3::periph::i2s0::Dir;
use pemu_soc_c3::wiring::usj as usj_wiring;

use super::Machine;
use crate::hle::{HleHciInput, HleNetInput};

/// Whether `PassportBoard::apply_input` consumes this input; the rest go to peripheral models.
fn board_input(ev: &InputEvent) -> bool {
    matches!(
        ev,
        InputEvent::Button { .. }
            | InputEvent::Power { .. }
            | InputEvent::UsbCable { .. }
            | InputEvent::UsbClient { .. }
            | InputEvent::UsbLine { .. }
            | InputEvent::NfcTap { .. }
            | InputEvent::Battery(_)
    )
}

impl Machine {
    /// Applies every due journaled input in `(at, seq)` order. An input nothing routes (the RTC
    /// epoch, a radio input with no bound module) is counted in [`Machine::unapplied_inputs`].
    pub(crate) fn apply_due_journal(&mut self) {
        let now = self.now();
        while let Some(entry) = self.journal.pop_due(now) {
            self.poll.invalidate();
            // Pumped into the OUT endpoint at once; the rest moves on the SOF tick. Bytes a full
            // ring cannot take count as unapplied rather than vanishing.
            if let InputEvent::SerialIn { data, .. } = &entry.ev {
                if self.io.usj_rx.push(data) < data.len() {
                    self.unapplied_inputs += 1;
                }
                usj_wiring::pump(&mut self.soc.devices.usj, &mut self.io, now, &mut self.irq);
                continue;
            }
            // Every `EnvChange` routed here has a named arm, so a new variant is unapplied rather
            // than silently swallowed by a wildcard.
            if let InputEvent::Env(EnvChange::WifiAps(aps)) = &entry.ev {
                let mut payload = Vec::new();
                aps.snap_write(&mut payload);
                if !self.on_radio_input("wifi", &payload) {
                    self.unapplied_inputs += 1;
                }
                continue;
            }
            // Mic samples enter as whole frames of the RX slot count; a full ring drops its
            // oldest frames. Audio arriving while RX is stopped or the capture path is closed is
            // lost, as on silicon, and counted in `PcmRing::dropped`, not as unapplied.
            if let InputEvent::MicChunk { samples, .. } = &entry.ev {
                let i2s = &self.soc.devices.i2s0;
                if self.mic_path().open && i2s.running(Dir::Rx) {
                    let slots = u16::from(i2s.format(Dir::Rx).slots);
                    self.io.audio_in.inject(samples, slots);
                } else {
                    self.io.audio_in.discard();
                    self.io.audio_in.count_dropped(samples.len());
                }
                continue;
            }
            if let InputEvent::Env(EnvChange::BleCentral { script }) = &entry.ev {
                if !self.on_radio_input("ble", script) {
                    self.unapplied_inputs += 1;
                }
                continue;
            }
            // Through the journal, so a replay attaches and detaches at the same instants.
            if let InputEvent::Env(EnvChange::BleHciBridge { attached }) = &entry.ev {
                let ev = match attached {
                    true => HleHciInput::Attach,
                    false => HleHciInput::Detach,
                };
                if !self.on_radio_hci(ev) {
                    self.unapplied_inputs += 1;
                }
                continue;
            }
            // The relay's packets are journaled, so a replay needs no peer.
            if let InputEvent::Env(EnvChange::WifiBridge { attached, routes }) = &entry.ev {
                let ev = match attached {
                    true => HleNetInput::Attach { routes },
                    false => HleNetInput::Detach,
                };
                if !self.on_radio_net(ev) {
                    self.unapplied_inputs += 1;
                }
                continue;
            }
            if let InputEvent::NetFrame { seq, data } = &entry.ev {
                if !self.on_radio_net(HleNetInput::Packet { seq: *seq, data }) {
                    self.unapplied_inputs += 1;
                }
                continue;
            }
            // The only way a host-posted radio event enters the machine: journaled with its
            // origin, so a run that took one replays to the same state.
            if let InputEvent::HciPacket { seq, data } = &entry.ev {
                if !self.on_radio_hci(HleHciInput::Packet { seq: *seq, data }) {
                    self.unapplied_inputs += 1;
                }
                continue;
            }
            if !board_input(&entry.ev) {
                self.unapplied_inputs += 1;
                continue;
            }
            match &entry.ev {
                InputEvent::UsbCable { plugged } => self.io.usj_ctrl.set_cable(*plugged),
                InputEvent::UsbClient { open } => self.io.usj_ctrl.set_client_open(*open),
                InputEvent::UsbLine { dtr, rts } => {
                    self.io.usj_ctrl.set_line_state(*rts, *dtr);
                    // The model keeps its download flag and repeat suppression; the reset is
                    // sequenced from the board's effect, which carries the `GPIO_STRAP` value.
                    let ctrl = self.io.usj_ctrl;
                    self.soc.devices.usj.line_state(ctrl.rts(), ctrl.dtr());
                }
                _ => {}
            }
            let mut cx = BoardCx::new(entry.at);
            let effect = self.board.apply_input(entry.at, &entry.ev, &mut cx);
            self.drain_board_cx(&mut cx);
            if matches!(
                entry.ev,
                InputEvent::UsbCable { .. } | InputEvent::UsbClient { .. }
            ) {
                self.apply_usb_ctrl();
            }
            self.apply_board_effect(effect);
        }
    }

    /// Acts on what a board input or chip event reports: a rail edge to OFF or a brownout powers
    /// the MCU down, a reset latches its strap and is sequenced. A card tap result is counted in
    /// [`Machine::pending_board_effects`] (the `nfc.tap` command reads it).
    pub(crate) fn apply_board_effect(&mut self, effect: BoardEffect) {
        if matches!(effect.rail, Some(PowerEdge::Off | PowerEdge::Brownout)) {
            self.power_down();
        }
        if let Some(line) = effect.reset {
            self.soc.devices.gpio.set_strap(u32::from(line.strap));
            match ResetKind::of(line.cause) {
                Some(kind) if kind.cause == ResetCause::POWERON => {
                    self.mcu_powered = true;
                    self.power_on();
                }
                Some(kind) => {
                    self.chip_reset(kind);
                }
                None => self.pending_board_effects += 1,
            }
        }
        if effect.usb.is_some() {
            self.apply_usb_ctrl();
        }
        if effect.card.is_some() {
            self.pending_board_effects += 1;
        }
        self.check_soc_brownout();
        self.check_gpio_wake();
    }

    /// The MCU rail went down: the hart stops and writable RAM reads back as zeros. UNVERIFIED:
    /// SRAM holds no defined value after a power cycle; zeros are the deterministic choice.
    fn power_down(&mut self) {
        self.mcu_powered = false;
        // The panel shares the board rail: its glass goes dark, its memory is kept.
        self.board.lcd.set_powered(false);
        self.publish_frame();
        self.hart.wfi = false;
        let bytes = self.soc.arena.bytes_mut();
        for region in pemu_soc_c3::mem::REGIONS.iter() {
            if region.flags & pemu_rv32::bus::PF_W == 0 {
                continue;
            }
            let at = region.arena as usize;
            bytes[at..at + region.len as usize].fill(0);
        }
    }

    /// Whether the MCU rail is up, so the hart executes. A machine starts powered.
    pub fn mcu_powered(&self) -> bool {
        self.mcu_powered
    }

    /// Empties the [`BoardCx`] a board call was handed: chip timers go to the scheduler, and named
    /// power events become `EventKind::Power` with `arg` 1 on, 0 off, 2 brownout (UNVERIFIED
    /// encoding); any other name is counted in [`Machine::pending_board_events`].
    pub(super) fn drain_board_cx(&mut self, cx: &mut BoardCx) {
        let now = self.now();
        let (timers, events) = cx.take();
        for t in timers {
            self.sched.schedule(
                now,
                t.at,
                EventKey {
                    owner: Owner::Chip(t.chip),
                    tag: t.tag,
                },
            );
        }
        for ev in events {
            let arg = match ev.name {
                "power.off" => 0,
                "power.on" => 1,
                "power.brownout" => 2,
                _ => {
                    self.pending_board_events += 1;
                    continue;
                }
            };
            self.io.events.emit(HostEvent {
                kind: EventKind::Power,
                vt: ev.at,
                arg,
            });
        }
    }

    /// Journaled inputs that came due and reached no model.
    pub fn unapplied_inputs(&self) -> u64 {
        self.unapplied_inputs
    }

    /// Board effects the machine could not act on: a card tap result, or an unknown reset cause.
    pub fn pending_board_effects(&self) -> u64 {
        self.pending_board_effects
    }

    /// Named board events with no `EventKind` mapping, so they reached no host event ring.
    pub fn pending_board_events(&self) -> u64 {
        self.pending_board_events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_inputs_are_exactly_the_ones_the_board_consumes() {
        use pemu_core::input::{ButtonId, SerialChan};

        assert!(board_input(&InputEvent::Button {
            id: ButtonId::Ok,
            down: true
        }));
        assert!(board_input(&InputEvent::UsbCable { plugged: true }));
        assert!(board_input(&InputEvent::Battery(
            pemu_core::input::BatterySet::default()
        )));
        assert!(!board_input(&InputEvent::SerialIn {
            chan: SerialChan(0),
            data: vec![b'x'],
        }));
        assert!(!board_input(&InputEvent::RtcEpoch { unix_us: 1 }));
    }
}
