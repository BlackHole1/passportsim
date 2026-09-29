//! `Wiring::UsjIo`: the only place bytes move between the USB Serial/JTAG model and the
//! `HostIo` rings (`specs/blocks/usj.toml`).
//!
//! [`apply_ctrl`] runs after a journaled cable, client or rail change and starts or stops SOF;
//! [`pump`] runs on every `Wiring::UsjIo`. The SOF tick returns `Wiring::UsjIo` once per emulated
//! millisecond, which keeps host-to-guest bytes moving while the guest only reads.

use pemu_core::hostio::{HostIo, SerialStream, UsjCtrl};
use pemu_core::sched::Scheduler;
use pemu_core::time::VTime;

use crate::intc::IrqFabric;
use crate::periph::usj::{FIFO_BYTES, HostLink, UsjModel};

/// The [`HostLink`] for a control state and an MCU rail: U0 to U3 plus enumeration progress.
pub fn host_link(ctrl: &UsjCtrl, rail_on: bool) -> HostLink {
    HostLink {
        state: ctrl.host_state(rail_on),
        enumeration: ctrl.enumeration(),
    }
}

/// Applies the host control state to the model, moves the rings and re-drives source 26.
///
/// Starting or stopping the 1 kHz SOF tick is the whole connection signal the IDF monitor has.
/// It ends in [`pump`] because opening the port (U2 to U3) releases the waiting packet, whose
/// line marks must carry `now`.
pub fn apply_ctrl(
    model: &mut UsjModel,
    io: &mut HostIo,
    rail_on: bool,
    now: VTime,
    sched: &mut Scheduler,
    irq: &mut IrqFabric,
) {
    let link = host_link(&io.usj_ctrl, rail_on);
    model.set_link(link, now, sched);
    pump(model, io, now, irq);
}

/// The `Wiring::UsjIo` ring pump.
///
/// Host bytes move into the OUT endpoint only while the model would take a packet, so a byte is
/// never popped and then dropped; the rest waits in the ring as backpressure.
pub fn pump(model: &mut UsjModel, io: &mut HostIo, now: VTime, irq: &mut IrqFabric) {
    if !model.capture().is_empty() {
        io.serial_write(SerialStream::UsjTx, model.capture(), now);
        model.clear_capture();
    }
    let mut packet = [0u8; FIFO_BYTES];
    while model.accepts_out() && !io.usj_rx.is_empty() {
        let n = io.usj_rx.pop(&mut packet);
        if n == 0 {
            break;
        }
        model.push_out_packet(&packet[..n]);
    }
    model.sync_irq(irq);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::periph::Wiring;
    use crate::periph::usj::LineAction;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::hostio::UsbHostState;
    use pemu_core::regstore::Size;
    use pemu_core::reset::ResetCause;
    use pemu_core::sched::EventKey;

    const EP1: u32 = 0x000;
    const EP1_CONF: u32 = 0x004;
    const WR_DONE: u32 = 1 << 0;

    /// `INT_ENA` stays 0, so [`UsjModel::sync_irq`] never touches the fabric.
    struct Rig {
        m: UsjModel,
        io: HostIo,
        sched: Scheduler,
        irq: IrqFabric,
        ledger: FidelityLedger,
        now: VTime,
    }

    impl Rig {
        fn new() -> Rig {
            let mut rig = Rig {
                m: UsjModel::default(),
                io: HostIo::new(1024),
                sched: Scheduler::new(),
                irq: IrqFabric::new(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
            };
            let kind = pemu_core::reset::ResetKind::of(ResetCause::POWERON)
                .expect("cause 0x01 is documented");
            rig.m.reset_to(kind, rig.now, &mut rig.sched);
            rig
        }

        fn ctrl(&mut self, rail_on: bool) {
            apply_ctrl(
                &mut self.m,
                &mut self.io,
                rail_on,
                self.now,
                &mut self.sched,
                &mut self.irq,
            );
        }

        /// Delivers the next scheduled event and applies the `Wiring` it returns, as the machine
        /// run loop does; this is how the SOF tick reaches [`pump`].
        fn run_one_event(&mut self) {
            let Some(t) = self.sched.next_time() else {
                return;
            };
            self.now = t;
            while let Some(EventKey { tag, .. }) = self.sched.pop_due(self.now) {
                if matches!(self.m.tick(tag, self.now, &mut self.sched), Wiring::UsjIo) {
                    self.pump();
                }
            }
        }

        fn pump(&mut self) {
            pump(&mut self.m, &mut self.io, self.now, &mut self.irq);
        }

        fn puts(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.m.reg_write(
                    EP1,
                    Size::B4,
                    u32::from(*b),
                    self.now,
                    &mut self.sched,
                    &mut self.ledger,
                );
            }
            self.m.reg_write(
                EP1_CONF,
                Size::B4,
                WR_DONE,
                self.now,
                &mut self.sched,
                &mut self.ledger,
            );
        }
    }

    #[test]
    fn the_pump_moves_bytes_both_ways() {
        let mut rig = Rig::new();
        rig.ctrl(true);
        assert!(rig.m.sof_running(), "the default host state is U3");

        rig.puts(b"hi\n");
        rig.pump();
        let mut out = [0u8; 16];
        let read = rig.io.usj_tx.read(0, &mut out);
        assert_eq!(&out[..read.n], b"hi\n");
        assert_eq!(rig.io.lines.lines(SerialStream::UsjTx), 1);
        assert!(
            rig.m.capture().is_empty(),
            "the capture is handed over once"
        );
        rig.pump();
        assert_eq!(rig.io.usj_tx.len(), 3, "a second pump writes nothing again");

        rig.io.usj_rx.push(b"ping");
        rig.pump();
        assert!(rig.io.usj_rx.is_empty());
        assert_eq!(rig.m.out_len(), 4);
        assert_eq!(
            rig.m.reg_read(EP1, Size::B4, rig.now, &mut rig.ledger),
            u32::from(b'p'),
        );

        // The endpoint holds one packet; the rest waits in the ring.
        rig.io.usj_rx.push(b"pong");
        rig.pump();
        assert_eq!(
            rig.io.usj_rx.len(),
            4,
            "the ring keeps what the endpoint cannot take"
        );
    }

    #[test]
    fn the_u_states_decide_whether_sof_runs() {
        let mut rig = Rig::new();
        rig.ctrl(true);
        assert_eq!(
            host_link(&rig.io.usj_ctrl, true).state,
            UsbHostState::AttachedOpen,
        );
        assert!(rig.m.sof_running());

        // U2: the client closed the port. SOF keeps running, the IN FIFO stops draining.
        rig.io.usj_ctrl.set_client_open(false);
        rig.ctrl(true);
        assert_eq!(rig.m.link().state, UsbHostState::AttachedIdle);
        assert!(rig.m.sof_running());
        rig.puts(b"idle");
        rig.pump();
        assert_eq!(rig.io.usj_tx.len(), 0);

        // U1: the rail is off, so nothing is enumerated.
        rig.io.usj_ctrl.set_client_open(true);
        rig.ctrl(false);
        assert_eq!(rig.m.link().state, UsbHostState::ChargeOnly);
        assert!(!rig.m.sof_running());

        // U0: the cable is gone.
        rig.io.usj_ctrl.set_cable(false);
        rig.ctrl(true);
        assert_eq!(rig.m.link().state, UsbHostState::Detached);
        assert!(!rig.m.sof_running());

        // Back to U3: the waiting packet is in the ring when `apply_ctrl` returns, stamped now.
        rig.io.usj_ctrl.set_cable(true);
        rig.ctrl(true);
        assert!(rig.m.sof_running());
        let mut out = [0u8; 16];
        let read = rig.io.usj_tx.read(0, &mut out);
        assert_eq!(&out[..read.n], b"idle");
        assert!(rig.m.capture().is_empty(), "apply_ctrl handed them over");
    }

    /// The ROM download path: `usb_uart_rx_one_char_block` only reads, and a read returns no
    /// `Wiring`, so the refill after each 64-byte packet must come from the SOF tick.
    #[test]
    fn the_out_stream_keeps_moving_while_the_guest_only_reads() {
        let mut rig = Rig::new();
        rig.ctrl(true);
        // Six packets, the shape of an esptool FLASH_DATA body.
        let body: Vec<u8> = (0..6 * FIFO_BYTES).map(|i| i as u8).collect();
        assert_eq!(rig.io.usj_rx.push(&body), body.len());
        rig.pump();

        let mut got = Vec::new();
        for packet in 0..6 {
            assert_eq!(rig.m.out_len(), FIFO_BYTES, "packet {packet} arrived whole");
            while rig.m.out_len() > 0 {
                let avail = rig.m.reg_read(EP1_CONF, Size::B4, rig.now, &mut rig.ledger);
                assert_ne!(avail & (1 << 2), 0, "SERIAL_OUT_EP_DATA_AVAIL");
                let byte = rig.m.reg_read(EP1, Size::B4, rig.now, &mut rig.ledger);
                got.push(byte as u8);
            }
            rig.run_one_event();
        }
        assert_eq!(got, body, "every byte reached the guest, in order");
        assert!(rig.io.usj_rx.is_empty(), "and none stayed in the ring");
    }

    #[test]
    fn the_line_state_comes_from_the_journaled_control_state() {
        let mut rig = Rig::new();
        rig.ctrl(true);
        rig.io.usj_ctrl.set_line_state(false, true);
        assert_eq!(
            rig.m
                .line_state(rig.io.usj_ctrl.rts(), rig.io.usj_ctrl.dtr()),
            LineAction::DownloadFlag(true)
        );
        rig.io.usj_ctrl.set_line_state(true, false);
        let LineAction::ChipReset { kind, download } = rig
            .m
            .line_state(rig.io.usj_ctrl.rts(), rig.io.usj_ctrl.dtr())
        else {
            panic!("(RTS, DTR) = (1, 0) resets the chip");
        };
        assert_eq!(kind.cause, ResetCause::USB_UART_CHIP);
        assert!(download);
    }
}
