//! Sleep, and the idle policy the run loop runs with.
//!
//! Sleep is not an [`IdlePolicy`]: it halts the hart after the `SLEEP_EN` write, powers down or
//! gates the digital domain, reads wake sources from RTC_CNTL and, for deep sleep, sequences a
//! reset. It keeps no state of its own: the kind is `STATE0.SLEEP_EN` and `DIG_PWC.DG_WRAP_PD_EN`,
//! a deep sleep has cleared [`Machine::mcu_powered`], and the wake is an `Owner::Machine` event.
//!
//! Not modeled (UNVERIFIED): a USB line state during deep sleep still resets the chip; wake
//! sources other than the timer and RTC GPIO pads; a GPIO wake out of light sleep.

use pemu_board::traits::BoardPorts;
use pemu_core::hostio::{EventKind, HostEvent};
use pemu_core::reset::{ResetCause, ResetKind, Retention};
use pemu_core::sched::{EventKey, MachineTimer, Owner};
use pemu_core::time::VTime;
use pemu_soc_c3::periph::rtc_cntl;
use pemu_soc_c3::periph::rtc_sleep::{self, SleepKind, trig};

use crate::config::MachineConfig;
use crate::machine::Machine;
use crate::run::{IdlePolicy, SkipToNextEvent};

/// [`SkipToNextEvent`] for every configuration: the wake timer is an event.
pub fn idle_policy(cfg: &MachineConfig) -> Box<dyn IdlePolicy + Send> {
    let _ = cfg;
    Box::new(SkipToNextEvent)
}

pub const WAKE_TIMER: MachineTimer = MachineTimer(0x30);

/// How long after its timer instant a light sleep returns.
///
/// On the device, 500 ms timer light sleeps return about 125 us late on esp_timer. IDF programs
/// `SLP_VAL` about 500 us short of the request, expecting the silicon's wake path to spend it, so
/// the wake is modeled 625 us after the timer instant. The split is class C (UNVERIFIED): only
/// the sum is measured, and only for 500 ms sleeps.
pub const LIGHT_SLEEP_WAKE_LATENCY_PS: u64 = 625_000_000;

/// `arg` of the `EventKind::Sleep` records; entries also detach USJ and wakes attach it.
pub const SLEEP_EVENT_LIGHT: u64 = 0;
pub const WAKE_EVENT_LIGHT: u64 = 1;
pub const SLEEP_EVENT_DEEP: u64 = 2;
pub const WAKE_EVENT_DEEP: u64 = 3;

impl Machine {
    pub(crate) fn enter_sleep(&mut self, kind: SleepKind) -> bool {
        match kind {
            SleepKind::Deep => self.enter_deep_sleep(),
            SleepKind::Light => self.enter_light_sleep(),
        }
        true
    }

    /// Light sleep entry: the hart halts in place and the digital clocks are gated, once, for the
    /// interval to the timer wake. SYSTIMER does not count the sleep (IDF adds it to esp_timer
    /// itself). At the wake the hart resumes at IDF's `INT_RAW` poll and finds `SLP_WAKEUP` set.
    ///
    /// Not modeled (UNVERIFIED): a host read during the sleep sees counters already advanced by the
    /// whole interval; USJ `FRAME_NUM`, LEDC and I2S counters are not gated; `Owner::Radio` events
    /// keep their time; an early wake would leave the RWDT paused for the whole planned interval.
    fn enter_light_sleep(&mut self) {
        let now = self.now();
        self.poll.invalidate();
        let gated = self.light_wake_at(now).map_or(u64::MAX, |at| at.0 - now.0);
        self.gate_digital_clocks(gated);
        // The RWDT is RTC domain and keeps counting unless `PAUSE_IN_SLP` holds it. IDF sets the
        // bit on the 1 s safety-net stage it arms around every light sleep; without the pause the
        // official firmware's 2 s sleep ends in an RTC reset.
        self.soc
            .devices
            .rtc_cntl
            .wdt_sleep_for(gated, now, &mut self.sched);
        // Halts as in WFI: nothing retires and the cycle counter stands still.
        self.hart.wfi = true;
        self.apply_usb_ctrl();
        self.emit_sleep_event(SLEEP_EVENT_LIGHT);
        if let Some(at) = self.light_wake_at(now) {
            self.schedule_wake_at(now, at);
        }
    }

    fn light_wake_at(&self, now: VTime) -> Option<VTime> {
        rtc_sleep::wake_plan(&self.soc.devices.rtc_cntl, now)
            .timer_at
            .map(|at| VTime(at.0.saturating_add(LIGHT_SLEEP_WAKE_LATENCY_PS)))
    }

    /// Gates the digital clocks for `ps`: pending events of SoC blocks other than RTC_CNTL fire
    /// `ps` later, and SYSTIMER and TIMG do not count those `ps`.
    fn gate_digital_clocks(&mut self, ps: u64) {
        let rtc = pemu_soc_c3::periph::id::RTC_CNTL;
        self.sched
            .postpone(ps, |key| matches!(key.owner, Owner::Periph(p) if p != rtc));
        let devices = &mut self.soc.devices;
        devices.systimer.clock_gated_for(ps);
        devices.timg0.clock_gated_for(ps);
        devices.timg1.clock_gated_for(ps);
    }

    /// Deep sleep entry. SRAM is lost (zeroed, the deterministic choice; UNVERIFIED) and RTC fast
    /// RAM kept; the wake resets with cause 0x05, which leaves the RTC domain alone.
    fn enter_deep_sleep(&mut self) {
        self.poll.invalidate();
        self.mcu_powered = false;
        self.hart.wfi = false;
        self.clear_ram_lost_by(ResetCause::DEEPSLEEP);
        self.apply_usb_ctrl();
        self.emit_sleep_event(SLEEP_EVENT_DEEP);
        // 4. The RWDT keeps counting, or holds with `PAUSE_IN_SLP`, for the time to the wake.
        let now = self.now();
        let wake_at = self.deep_wake_at(now);
        let slept = wake_at.map_or(u64::MAX, |at| at.0 - now.0);
        self.soc
            .devices
            .rtc_cntl
            .wdt_sleep_for(slept, now, &mut self.sched);
        if let Some(at) = wake_at {
            self.schedule_wake_at(now, at);
        }
    }

    /// The timer instant, or `now` when a GPIO wake pad already reads its wake level, whichever is
    /// first. `None` when nothing modeled can end the sleep.
    fn deep_wake_at(&self, now: VTime) -> Option<VTime> {
        let timer_at = rtc_sleep::wake_plan(&self.soc.devices.rtc_cntl, now).timer_at;
        if self.gpio_wake_pads_asserted(now) != 0 {
            return Some(timer_at.map_or(now, |at| at.min(now)));
        }
        timer_at
    }

    /// The RTC GPIO pads armed on a level the board reads now, a bit per pad. Only the
    /// button-ladder pad is asked: it is the one input the board drives. Any other pin's `gpio_in`
    /// is a default, not a measurement, so answering for it would invent a wake.
    fn gpio_wake_pads_asserted(&self, now: VTime) -> u32 {
        let rtc = &self.soc.devices.rtc_cntl;
        let armed = rtc_sleep::wake_plan(rtc, now).gpio_pads;
        if armed == 0 {
            return 0;
        }
        let ladder_pin = self.board.ladder.config().gpio;
        if u32::from(ladder_pin) >= u32::from(rtc_sleep::RTC_GPIO_PINS)
            || armed & (1 << ladder_pin) == 0
        {
            return 0;
        }
        let Some(wake_level) = rtc_sleep::gpio_wake_level(rtc, ladder_pin) else {
            return 0;
        };
        // The deep-sleep level, not the awake `gpio_in`: this board's pull-up does not hold GPIO0
        // high then.
        if self.board.ladder.low_in_deep_sleep() != wake_level {
            1 << ladder_pin
        } else {
            0
        }
    }

    pub(crate) fn check_gpio_wake(&mut self) {
        if self.mcu_powered || !self.board.rail().is_on() {
            return;
        }
        if rtc_sleep::sleeping(&self.soc.devices.rtc_cntl) != Some(SleepKind::Deep) {
            return;
        }
        let now = self.now();
        let pads = self.gpio_wake_pads_asserted(now);
        if pads != 0 {
            self.wake_from_deep_sleep(trig::GPIO, pads);
        }
    }

    /// Performs the sleep a restored snapshot still owed as `StopReason::Sleep`. Only a format
    /// `FORMAT_VERSION` already refuses could hold one; kept as a guard.
    pub(crate) fn perform_restored_sleep_stop(&mut self) {
        if let Some(crate::stops::StopReason::Sleep(kind)) = self.wiring_stop {
            self.wiring_stop = None;
            self.enter_sleep(kind);
        }
    }

    /// Called at the start of every chip reset: a reset during deep sleep on a live rail can only
    /// come from the RTC domain (the RWDT). It powers up and ends the sleep without a wake cause.
    pub(crate) fn leave_deep_sleep_for_reset(&mut self) {
        if self.mcu_powered || !self.board.rail().is_on() {
            return;
        }
        if rtc_sleep::sleeping(&self.soc.devices.rtc_cntl) != Some(SleepKind::Deep) {
            return;
        }
        rtc_sleep::abort_sleep(&mut self.soc.devices.rtc_cntl);
        self.mcu_powered = true;
        self.emit_sleep_event(WAKE_EVENT_DEEP);
    }

    fn schedule_wake_at(&mut self, now: VTime, at: VTime) {
        self.sched.schedule(
            now,
            at,
            EventKey {
                owner: Owner::Machine(WAKE_TIMER),
                tag: 0,
            },
        );
    }

    /// Wakes the chip only while RTC_CNTL still sleeps and the counter has reached `SLP_VAL`, so a
    /// timer left from a sleep a power cycle or watchdog reset ended wakes nothing.
    pub(crate) fn on_wake_timer(&mut self) {
        let now = self.now();
        if !self.board.rail().is_on() {
            return;
        }
        let rtc = &self.soc.devices.rtc_cntl;
        let Some(kind) = rtc_sleep::sleeping(rtc) else {
            return;
        };
        let timer_due = rtc_sleep::timer_due(rtc, now);
        match kind {
            SleepKind::Deep if !self.mcu_powered => {
                let pads = self.gpio_wake_pads_asserted(now);
                if timer_due {
                    self.wake_from_deep_sleep(trig::TIMER, 0);
                } else if pads != 0 {
                    self.wake_from_deep_sleep(trig::GPIO, pads);
                }
            }
            SleepKind::Light if self.mcu_powered && timer_due => self.wake_from_light_sleep(),
            _ => {}
        }
    }

    fn wake_from_light_sleep(&mut self) {
        // The latch clears `SLEEP_EN`, so `usj_phy_powered` answers again.
        self.latch_wake(trig::TIMER);
        self.hart.wfi = false;
        self.poll.invalidate();
        // The enumeration delay is measured only for a deep-sleep wake and a `SYS_` reset.
        let delay = self.soc.devices.usj.enumeration_delay();
        self.soc.devices.usj.set_enumeration_delay(VTime(0));
        self.apply_usb_ctrl();
        self.soc.devices.usj.set_enumeration_delay(delay);
        self.emit_sleep_event(WAKE_EVENT_LIGHT);
    }

    /// Deep sleep wake: latch `cause` (and `pads`, for a GPIO wake), power up and reset with 0x05.
    fn wake_from_deep_sleep(&mut self, cause: u32, pads: u32) {
        if pads != 0 {
            rtc_sleep::latch_gpio_status(&mut self.soc.devices.rtc_cntl, pads);
        }
        self.latch_wake(cause);
        self.mcu_powered = true;
        self.emit_sleep_event(WAKE_EVENT_DEEP);
        let kind = ResetKind::of(ResetCause::DEEPSLEEP).expect("cause 0x05 is documented");
        self.chip_reset(kind);
    }

    fn latch_wake(&mut self, cause: u32) {
        let rtc = &mut self.soc.devices.rtc_cntl;
        rtc_sleep::latch_wake(rtc, cause);
        let level = rtc.irq_level();
        self.irq.set_source(rtc_cntl::IRQ_SOURCE, level);
    }

    /// Runs the SoC brownout detector against the chip supply the board gives now. Called after
    /// every board input and every chip reset, so a firmware restarting below the threshold sees it
    /// again. The battery advances at 1 s board events, so a sagging supply reaches the detector
    /// within 1 s (UNVERIFIED; the silicon comparator acts at once).
    pub(crate) fn check_soc_brownout(&mut self) {
        if !self.board.rail().is_on() {
            return;
        }
        #[allow(unused_mut)]
        let mut supply =
            rtc_sleep::chip_supply_mv(self.board.battery.terminal_mv(), self.board.usb.cable());
        #[cfg(test)]
        if let Some(mv) = self.test_supply_mv {
            supply = mv;
        }
        let rtc = &mut self.soc.devices.rtc_cntl;
        rtc_sleep::detect_brownout(rtc, supply);
        let level = rtc.irq_level();
        self.irq.set_source(rtc_cntl::IRQ_SOURCE, level);
    }

    /// Whether the digital domain is in light sleep. The run loop's WFI wake is suppressed then:
    /// a pending line does not resume a clock-gated CPU, only an enabled wake source does.
    pub(crate) fn light_sleeping(&self) -> bool {
        self.mcu_powered
            && rtc_sleep::sleeping(&self.soc.devices.rtc_cntl) == Some(SleepKind::Light)
    }

    /// Every block has a clock while the MCU is powered; RTC_CNTL alone keeps one in deep sleep.
    pub(crate) fn periph_clocked(&self, p: pemu_core::sched::PeriphId) -> bool {
        self.mcu_powered || (p == pemu_soc_c3::periph::id::RTC_CNTL && self.board.rail().is_on())
    }

    /// Whether the USJ PHY has power: the rail is up and the chip is in neither deep nor light
    /// sleep (the device drops the link for a light sleep too). The IN FIFO survives, so queued
    /// lines reach the host once the link is back.
    ///
    /// UNVERIFIED: re-enumeration time after a light-sleep wake; the link returns at once. With
    /// `usj_enum_wake_ps` applied here, `probe_clocks`'s light-sleep phase never finishes under
    /// `device` (its console task stays blocked, cause unknown).
    pub(crate) fn usj_phy_powered(&self) -> bool {
        self.board.rail().is_on() && self.mcu_powered && !self.light_sleeping()
    }

    fn clear_ram_lost_by(&mut self, cause: ResetCause) {
        let Some(spec) = cause.spec() else {
            return;
        };
        let bytes = self.soc.arena.bytes_mut();
        for region in pemu_soc_c3::mem::REGIONS.iter() {
            if region.flags & pemu_rv32::bus::PF_W == 0 {
                continue;
            }
            let retention = if region.vbase == pemu_soc_c3::mem::RTC_FAST_BASE {
                spec.rtc_ram
            } else {
                spec.sram
            };
            if retention == Retention::Cleared {
                let at = region.arena as usize;
                bytes[at..at + region.len as usize].fill(0);
            }
        }
    }

    fn emit_sleep_event(&mut self, arg: u64) {
        let vt = self.now();
        self.io.events.emit(HostEvent {
            kind: EventKind::Sleep,
            vt,
            arg,
        });
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use super::*;

    use pemu_core::hostio::SerialStream;
    use pemu_core::input::{ButtonId, InputEvent};
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_rv32::bus::{Bus, HartView};
    use pemu_soc_c3::r#gen::regs_rtc_cntl::idx;
    use pemu_soc_c3::mem::{RTC_FAST_BASE, SRAM1_DRAM_BASE, SRAM1_IRAM_BASE};
    use pemu_soc_c3::periph::rtc_cntl::SLOW_HZ;

    use crate::config::Assets;
    use crate::machine::At;
    use crate::run::RunLimits;
    use crate::stops::{Matcher, MatcherId, StopReason, StopSet};
    use pemu_core::hostio::UsbHostState;
    use pemu_core::snap::SnapOpts;

    const RTC_CNTL: u32 = 0x6000_8000;
    const OFF_SLP_TIMER0: u32 = 0x004;
    const OFF_SLP_TIMER1: u32 = 0x008;
    const OFF_WAKEUP_STATE: u32 = 0x03C;
    const OFF_DIG_PWC: u32 = 0x088;
    const OFF_GPIO_WAKEUP: u32 = 0x110;
    const DIG_PWC_RESET: u32 = 0x0055_5010;

    /// `lui a0, 0x60008; lui a1, 0x80000; sw a1, 24(a0); j .`: the `STATE0.SLEEP_EN` write, then a
    /// loop the silicon never reaches.
    const SLEEP_EN_WRITE: [u32; 4] = [0x6000_8537, 0x8000_05B7, 0x00B5_2C23, 0x0000_006F];

    const SRAM_MARK: u32 = SRAM1_DRAM_BASE + 0x1_0000;
    const RTC_MARK: u32 = RTC_FAST_BASE + 0x100;
    const MARK: u32 = 0x464F_4C4F;

    const RESET: MatcherId = MatcherId(0x05);

    fn machine() -> Machine {
        machine_with(MachineConfig::default())
    }

    fn machine_with(cfg: MachineConfig) -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        Machine::new(cfg, assets).expect("the ROM fits the ROM window")
    }

    fn machine_with_held_pullup() -> Machine {
        let mut cfg = MachineConfig::default();
        cfg.board.buttons.pullup_held_in_deep_sleep = true;
        machine_with(cfg)
    }

    fn store(m: &mut Machine, addr: u32, val: u32) {
        let view = HartView {
            insns: 0,
            extra: 0,
            pc: 0,
        };
        m.with_bus(|bus, _| {
            bus.store_slow(addr, 4, val, &view);
        });
        m.apply_pending_wiring();
    }

    fn load(m: &mut Machine, addr: u32) -> u32 {
        m.soc.load_mem(addr, 4).expect("a mapped RAM address")
    }

    /// IDF's deep-sleep setup with a timer wake `ms` from now, the RWDT flash-boot hold cleared.
    fn prepare_deep_sleep(m: &mut Machine, timer_ms: Option<u64>) {
        store(m, RTC_CNTL + 0xA8, rtc_cntl::WDT_WKEY);
        store(m, RTC_CNTL + 0x90, 0);
        store(m, RTC_CNTL + 0xA8, 0);
        store(m, SRAM_MARK, MARK);
        store(m, RTC_MARK, MARK);
        if let Some(ms) = timer_ms {
            let target = m.soc.devices.rtc_cntl.rtc_ticks(m.now()) + ms * SLOW_HZ / 1_000;
            store(m, RTC_CNTL + OFF_WAKEUP_STATE, trig::TIMER << 15);
            store(m, RTC_CNTL + OFF_SLP_TIMER0, target as u32);
            store(
                m,
                RTC_CNTL + OFF_SLP_TIMER1,
                ((target >> 32) as u32) | rtc_sleep::SLP_TIMER1_ALARM_EN,
            );
        } else {
            store(m, RTC_CNTL + OFF_WAKEUP_STATE, 0);
        }
        store(
            m,
            RTC_CNTL + OFF_DIG_PWC,
            DIG_PWC_RESET | rtc_sleep::DIG_PWC_DG_WRAP_PD_EN,
        );
        for (i, w) in SLEEP_EN_WRITE.iter().enumerate() {
            m.soc.store_mem(SRAM1_IRAM_BASE + 4 * i as u32, 4, *w);
        }
        m.hart.pc = SRAM1_IRAM_BASE;
    }

    fn until(t: VTime, stops: StopSet) -> RunLimits {
        RunLimits {
            until: Some(t),
            max_insns: None,
            stops,
        }
    }

    fn reset_stop() -> StopSet {
        StopSet {
            matchers: vec![(RESET, Matcher::Event(EventKind::Reset))],
            ..StopSet::default()
        }
    }

    fn sleep_events(m: &Machine) -> Vec<(u64, VTime)> {
        m.io.events
            .slices(0)
            .iter()
            .filter(|e| e.kind == EventKind::Sleep)
            .map(|e| (e.arg, e.vt))
            .collect()
    }

    fn console(m: &mut Machine) -> String {
        let ring = m.io.serial_ring(SerialStream::UsjTx);
        String::from_utf8_lossy(&ring.slices(0).iter().copied().collect::<Vec<u8>>()).into_owned()
    }

    #[test]
    fn a_deep_sleep_timer_wake_clears_sram_keeps_rtc_ram_and_resets_with_cause_5() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(1_000));
        let entered = m.run(until(VTime::from_ms(500), StopSet::default()));
        assert_eq!(
            entered.reason,
            StopReason::Until,
            "deep sleep is not a stop"
        );
        assert!(!m.mcu_powered(), "the digital domain is down");
        let asleep_at = sleep_events(&m)[0].1;
        assert_eq!(sleep_events(&m), vec![(SLEEP_EVENT_DEEP, asleep_at)]);
        assert_eq!(entered.insns, 3, "nothing runs after the SLEEP_EN store");
        assert_eq!(load(&mut m, SRAM_MARK), 0, "SRAM is lost at the power-down");
        assert_eq!(load(&mut m, RTC_MARK), MARK);
        assert!(
            m.soc.devices.usj.link().state == UsbHostState::ChargeOnly,
            "the host sees the detach"
        );

        let woke = m.run(until(VTime::from_ms(3_000), reset_stop()));
        assert_eq!(woke.reason, StopReason::Matcher(RESET));
        // `SLP_VAL` was one second of ticks past the counter at time 0, so the wake is at 1 s give
        // or take one slow-clock tick.
        let tick = 1_000_000_000_000 / SLOW_HZ + 1;
        assert!(asleep_at.0 > 0 && asleep_at.0 < tick);
        assert!(
            woke.vt.0.abs_diff(1_000_000_000_000) <= tick,
            "woke at {:?}",
            woke.vt
        );
        assert_eq!(woke.insns, 0);
        let rtc = &m.soc.devices.rtc_cntl;
        assert_eq!(rtc.reset_cause(), ResetCause::DEEPSLEEP);
        assert_eq!(rtc.regs().get(idx::RTC_CNTL_SLP_WAKEUP_CAUSE), trig::TIMER);
        assert_eq!(
            rtc.regs().get(idx::RTC_CNTL_STATE0),
            0x2000_0000,
            "device-sleep_lat-20260917T141559Z.notes.md: STATE0 at boot 2 app_main"
        );
        assert_eq!(rtc_sleep::sleeping(rtc), None);
        assert!(m.mcu_powered());
        assert_eq!(load(&mut m, SRAM_MARK), 0);
        assert_eq!(load(&mut m, RTC_MARK), MARK, "RTC fast RAM survives");
        assert_eq!(
            m.soc.devices.usj.link().state,
            UsbHostState::AttachedOpen,
            "the attach"
        );
        let events = sleep_events(&m);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].0, WAKE_EVENT_DEEP);

        for _ in 0..40 {
            m.run(RunLimits::insns(10_000));
            if console(&mut m).contains("boot:") {
                break;
            }
        }
        let text = console(&mut m);
        assert!(
            text.contains("rst:0x5 (DSLEEP),boot:0xa (SPI_FAST_FLASH_BOOT)"),
            "{text:?}"
        );
    }

    /// Arms a deep-sleep GPIO wake on pad 0, `LOW_LEVEL`: what
    /// `esp_deep_sleep_enable_gpio_wakeup(BIT(0), ESP_GPIO_WAKEUP_GPIO_LOW)` writes.
    fn arm_gpio_wake(m: &mut Machine) {
        assert_eq!(m.board.ladder.config().gpio, 0, "GPIO0 is the ladder pad");
        let ena = m
            .soc
            .devices
            .rtc_cntl
            .regs()
            .get(idx::RTC_CNTL_WAKEUP_STATE);
        store(m, RTC_CNTL + OFF_WAKEUP_STATE, ena | (trig::GPIO << 15));
        store(
            m,
            RTC_CNTL + OFF_GPIO_WAKEUP,
            (1 << 31) | (rtc_sleep::int_type::LOW_LEVEL << 23),
        );
    }

    fn gpio_wakeup_status(m: &Machine) -> u32 {
        m.soc.devices.rtc_cntl.regs().get(idx::RTC_CNTL_GPIO_WAKEUP) & 0x3F
    }

    #[test]
    fn a_gpio0_press_wakes_a_deep_sleep_with_cause_5_and_wake_cause_gpio() {
        let mut m = machine_with_held_pullup();
        prepare_deep_sleep(&mut m, None);
        arm_gpio_wake(&mut m);
        let entered = m.run(until(VTime::from_ms(500), StopSet::default()));
        assert_eq!(entered.reason, StopReason::Until, "no button is down yet");
        assert!(!m.mcu_powered(), "the chip is asleep");
        assert_eq!(m.resets(), 1, "only the power-on");
        assert_eq!(gpio_wakeup_status(&m), 0);

        let t = m.now();
        m.input(
            At::Vt(t),
            InputEvent::Button {
                id: ButtonId::Ok,
                down: true,
            },
        )
        .unwrap();
        let woke = m.run(until(VTime::from_ms(1_500), reset_stop()));
        assert_eq!(woke.reason, StopReason::Matcher(RESET));
        let rtc = &m.soc.devices.rtc_cntl;
        assert_eq!(rtc.reset_cause(), ResetCause::DEEPSLEEP);
        assert_eq!(rtc.regs().get(idx::RTC_CNTL_SLP_WAKEUP_CAUSE), trig::GPIO);
        assert_eq!(rtc.regs().get(idx::RTC_CNTL_STATE0), 0x2000_0000);
        assert_eq!(rtc_sleep::sleeping(rtc), None);
        assert_eq!(gpio_wakeup_status(&m), 1, "pad 0 woke the chip");
        assert!(m.mcu_powered());
        assert_eq!(load(&mut m, SRAM_MARK), 0, "SRAM was lost at the entry");
        assert_eq!(load(&mut m, RTC_MARK), MARK, "RTC fast RAM survives");
        let events = sleep_events(&m);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].0, WAKE_EVENT_DEEP);
    }

    /// On this board GPIO0 reads low in deep sleep with no key pressed, so a GPIO0-low deep sleep
    /// ends at once, even with a timer beside it.
    #[test]
    fn on_this_board_a_gpio0_low_deep_sleep_wakes_at_once() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(3_000));
        arm_gpio_wake(&mut m);
        let woke = m.run(until(VTime::from_ms(500), reset_stop()));
        assert_eq!(woke.reason, StopReason::Matcher(RESET));
        let entered_at = sleep_events(&m)[0].1;
        assert_eq!(
            sleep_events(&m)[1].1,
            entered_at,
            "the wake is at the entry instant"
        );
        let rtc = &m.soc.devices.rtc_cntl;
        assert_eq!(rtc.reset_cause(), ResetCause::DEEPSLEEP);
        assert_eq!(rtc.regs().get(idx::RTC_CNTL_SLP_WAKEUP_CAUSE), trig::GPIO);
        assert_eq!(gpio_wakeup_status(&m), 1);
    }

    #[test]
    fn a_gpio_wake_armed_on_the_level_the_pad_does_not_read_does_not_wake() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(1_000));
        let ena = m
            .soc
            .devices
            .rtc_cntl
            .regs()
            .get(idx::RTC_CNTL_WAKEUP_STATE);
        store(
            &mut m,
            RTC_CNTL + OFF_WAKEUP_STATE,
            ena | (trig::GPIO << 15),
        );
        store(
            &mut m,
            RTC_CNTL + OFF_GPIO_WAKEUP,
            (1 << 31) | (rtc_sleep::int_type::HIGH_LEVEL << 23),
        );
        let woke = m.run(until(VTime::from_ms(3_000), reset_stop()));
        assert_eq!(woke.reason, StopReason::Matcher(RESET));
        assert_eq!(
            m.soc
                .devices
                .rtc_cntl
                .regs()
                .get(idx::RTC_CNTL_SLP_WAKEUP_CAUSE),
            trig::TIMER,
            "the timer woke it, not the pad"
        );
        assert_eq!(gpio_wakeup_status(&m), 0);
        let tick = 1_000_000_000_000 / SLOW_HZ + 1;
        assert!(
            woke.vt.0.abs_diff(1_000_000_000_000) <= tick,
            "{:?}",
            woke.vt
        );
    }

    #[test]
    fn a_deep_sleep_without_a_wake_source_lasts_until_a_power_cycle() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, None);
        let out = m.run(until(VTime::from_ms(10_000), StopSet::default()));
        assert_eq!(out.reason, StopReason::Until);
        assert!(!m.mcu_powered());
        assert_eq!(m.resets(), 1, "only the power-on");
        let t = m.now();
        m.input(At::Vt(t), InputEvent::Power { down: true })
            .unwrap();
        m.input(
            At::Vt(VTime(t.0 + VTime::from_ms(2_200).0)),
            InputEvent::Power { down: false },
        )
        .unwrap();
        let on = VTime(t.0 + VTime::from_ms(3_000).0);
        m.input(At::Vt(on), InputEvent::Power { down: true })
            .unwrap();
        m.input(
            At::Vt(VTime(on.0 + VTime::from_ms(600).0)),
            InputEvent::Power { down: false },
        )
        .unwrap();
        let out = m.run(until(VTime(on.0 + VTime::from_ms(700).0), reset_stop()));
        assert_eq!(out.reason, StopReason::Matcher(RESET));
        assert_eq!(m.soc.devices.rtc_cntl.reset_cause(), ResetCause::POWERON);
        assert!(m.mcu_powered());
        assert_eq!(
            load(&mut m, RTC_MARK),
            0,
            "a power cycle clears RTC fast RAM"
        );
    }

    #[test]
    fn a_snapshot_taken_mid_sleep_wakes_at_the_same_instant() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(1_000));
        m.run(until(VTime::from_ms(400), StopSet::default()));
        let snap = m.snapshot(SnapOpts::default());
        let first = m.run(until(VTime::from_ms(3_000), reset_stop()));
        let mut other = machine();
        other.restore(&snap).expect("same identity");
        let second = other.run(until(VTime::from_ms(3_000), reset_stop()));
        assert_eq!(first.reason, StopReason::Matcher(RESET));
        assert_eq!((first.vt, first.reason), (second.vt, second.reason));
        assert_eq!(m.state_hash(), other.state_hash());
    }

    #[test]
    fn a_stale_wake_timer_is_ignored() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(1_000));
        m.run(until(VTime::from_ms(100), StopSet::default()));
        m.mcu_powered = true;
        let kind = ResetKind::of(ResetCause::RTC_SW_SYS).expect("documented");
        m.chip_reset(kind);
        let resets = m.resets();
        let rtc = &mut m.soc.devices.rtc_cntl;
        let state0 = rtc.regs().get(idx::RTC_CNTL_STATE0) & !rtc_sleep::STATE0_SLEEP_EN;
        rtc.hw_set(idx::RTC_CNTL_STATE0, state0);
        m.hart.wfi = true;
        m.run(until(VTime::from_ms(1_500), StopSet::default()));
        assert_eq!(m.resets(), resets, "the stale timer sequenced no reset");
        assert_eq!(sleep_events(&m).len(), 1);
    }

    /// `lui a0, 0x60008; lui a1, 0x80000; sw a1, 24(a0); addi a3, zero, 1; j .`: the `SLEEP_EN`
    /// write, then a marker the hart sets only once it resumes.
    const LIGHT_SLEEP_THEN_MARK: [u32; 5] = [
        0x6000_8537,
        0x8000_05B7,
        0x00B5_2C23,
        0x0010_0693,
        0x0000_006F,
    ];

    #[test]
    fn a_light_sleep_gates_systimer_counts_rtc_time_and_resumes_after_the_store() {
        let mut m = machine();
        m.idle_until(VTime::from_ms(1_234));
        prepare_deep_sleep(&mut m, Some(2_000));
        store(&mut m, RTC_CNTL + OFF_DIG_PWC, DIG_PWC_RESET);
        for (i, w) in LIGHT_SLEEP_THEN_MARK.iter().enumerate() {
            m.soc.store_mem(SRAM1_IRAM_BASE + 4 * i as u32, 4, *w);
        }
        let out = m.run(until(VTime::from_ms(2_000), StopSet::default()));
        assert_eq!(out.reason, StopReason::Until, "light sleep is not a stop");
        assert_eq!(out.insns, 3, "the hart halted after the SLEEP_EN store");
        assert!(m.hart.wfi && m.mcu_powered());
        let events = sleep_events(&m);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, SLEEP_EVENT_LIGHT);
        let entry = events[0].1;
        let counter_at_entry = entry.0 / pemu_core::clock::SYSTIMER_TICK_PS;
        assert!(counter_at_entry > 1_234_000 * 16);
        let ticks_at_entry = m.soc.devices.rtc_cntl.rtc_ticks(entry);
        assert_eq!(load(&mut m, SRAM_MARK), MARK, "light sleep keeps SRAM");
        assert_eq!(
            m.soc.devices.usj.link().state,
            UsbHostState::ChargeOnly,
            "the host sees the detach for the slept interval"
        );

        let out = m.run(until(VTime::from_ms(4_000), StopSet::default()));
        assert_eq!(out.reason, StopReason::Until);
        let events = sleep_events(&m);
        assert_eq!(events.len(), 2);
        let (arg, wake) = events[1];
        assert_eq!(arg, WAKE_EVENT_LIGHT);
        let tick = 1_000_000_000_000 / SLOW_HZ + 1;
        assert!(
            wake.0
                .abs_diff(VTime::from_ms(3_234).0 + LIGHT_SLEEP_WAKE_LATENCY_PS)
                <= tick,
            "woke at {wake:?}"
        );
        let rtc = &m.soc.devices.rtc_cntl;
        assert!(rtc.rtc_ticks(wake) - ticks_at_entry >= 2 * SLOW_HZ - 1);
        assert_eq!(
            m.soc.devices.systimer.counter(0, wake),
            counter_at_entry,
            "SYSTIMER stands still across the sleep"
        );
        assert_eq!(rtc.regs().get(idx::RTC_CNTL_SLP_WAKEUP_CAUSE), trig::TIMER);
        assert_ne!(
            rtc.regs().get(idx::RTC_CNTL_INT_RAW) & rtc_sleep::INT_SLP_WAKEUP,
            0
        );
        assert_eq!(
            rtc.regs().get(idx::RTC_CNTL_STATE0),
            0x2000_0000,
            "device-sleep_lat-20260917T141559Z.notes.md: STATE0 after a light-sleep wake"
        );
        assert_eq!(rtc_sleep::sleeping(rtc), None);
        assert!(!m.hart.wfi);
        assert_eq!(m.hart.x[13], 1, "the hart resumed after the store");
        assert!(m.idle_ps() >= wake.0 - entry.0, "the sleep is idle time");
        assert_eq!(m.resets(), 1, "a light sleep resets nothing");
        assert_eq!(
            m.soc.devices.usj.link().state,
            UsbHostState::AttachedOpen,
            "the attach"
        );
    }

    /// On battery the chip supply stays above the 2.51 V threshold down to the 3000 mV rail cutoff,
    /// so a sagging cell ends as the rail brownout, never setting `DET`.
    #[test]
    fn a_sagging_cell_ends_in_the_rail_cutoff_before_the_soc_detector() {
        use pemu_core::input::BatterySet;
        let mut m = machine();
        let apply = |m: &mut Machine, ev: InputEvent| {
            m.input(At::Now, ev).expect("now");
            let now = m.now();
            m.run(until(now, StopSet::default()));
        };
        let det = |m: &Machine| {
            m.soc.devices.rtc_cntl.regs().get(idx::RTC_CNTL_BROWN_OUT) & rtc_sleep::BROWN_OUT_DET
        };
        let battery = |mv: u16| {
            InputEvent::Battery(BatterySet {
                mv: Some(mv),
                ..BatterySet::default()
            })
        };
        apply(&mut m, InputEvent::UsbCable { plugged: false });
        for mv in [3_900, 3_300, 3_010] {
            apply(&mut m, battery(mv));
            assert_eq!(det(&m), 0, "{mv} mV on the cell");
            assert!(m.mcu_powered());
        }
        apply(&mut m, battery(2_900));
        assert!(!m.mcu_powered(), "below the 3000 mV cutoff the rail drops");
        assert_eq!(det(&m), 0);
        let power: Vec<u64> =
            m.io.events
                .slices(0)
                .iter()
                .filter(|e| e.kind == EventKind::Power)
                .map(|e| e.arg)
                .collect();
        assert_eq!(power.last(), Some(&2), "{power:?}");
    }

    #[test]
    fn a_firmware_restarting_below_the_threshold_sees_the_brownout_again() {
        let mut m = machine();
        let raw = |m: &Machine| m.soc.devices.rtc_cntl.regs().get(idx::RTC_CNTL_INT_RAW);
        let det = |m: &Machine| {
            m.soc.devices.rtc_cntl.regs().get(idx::RTC_CNTL_BROWN_OUT) & rtc_sleep::BROWN_OUT_DET
        };
        m.test_supply_mv = Some(rtc_sleep::BROWNOUT_LVL7_MV - 10);
        m.check_soc_brownout();
        assert_ne!(det(&m), 0);
        store(&mut m, RTC_CNTL + 0x04C, rtc_sleep::INT_BROWN_OUT);
        assert_ne!(
            raw(&m) & rtc_sleep::INT_BROWN_OUT,
            0,
            "INT_CLR cannot clear a level"
        );

        let kind = ResetKind::of(ResetCause::RTC_SW_SYS).expect("cause 0x03 is documented");
        m.chip_reset(kind);
        assert_ne!(det(&m), 0, "re-evaluated after the reset");
        assert_ne!(raw(&m) & rtc_sleep::INT_BROWN_OUT, 0);

        m.test_supply_mv = Some(rtc_sleep::CHIP_SUPPLY_MV);
        m.chip_reset(kind);
        assert_eq!(det(&m), 0, "a restart with a good supply clears DET");
    }

    /// Arms the RWDT stage 0 as an RTC reset after `hold_ms` (hold is `WDTCONFIG1 << 1` with
    /// `WDT_DELAY_SEL` 0).
    fn arm_rwdt(m: &mut Machine, hold_ms: u64, pause: bool) {
        const WDT_EN: u32 = 1 << 31;
        const PAUSE_IN_SLP: u32 = 1 << 9;
        const STG0_RESET_RTC: u32 = 4 << 28;
        store(m, RTC_CNTL + 0xA8, rtc_cntl::WDT_WKEY);
        store(m, RTC_CNTL + 0x94, (hold_ms * SLOW_HZ / 1_000 / 2) as u32);
        let pause = if pause { PAUSE_IN_SLP } else { 0 };
        store(m, RTC_CNTL + 0x90, WDT_EN | STG0_RESET_RTC | pause);
        store(m, RTC_CNTL + 0xA8, 0);
    }

    fn reset_events(m: &Machine) -> Vec<(u64, VTime)> {
        m.io.events
            .slices(0)
            .iter()
            .filter(|e| e.kind == EventKind::Reset)
            .map(|e| (e.arg, e.vt))
            .collect()
    }

    #[test]
    fn a_deep_sleep_rwdt_without_pause_resets_with_cause_0x10() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(2_000));
        arm_rwdt(&mut m, 500, false);
        let out = m.run(until(VTime::from_ms(3_000), StopSet::default()));
        assert_eq!(out.reason, StopReason::Until);
        let resets = reset_events(&m);
        let rtc = resets.iter().find(|r| r.0 == 0x10).expect("an RWDT reset");
        assert!(rtc.1 < VTime::from_ms(600), "{resets:?}");
        assert!(
            !resets.iter().any(|r| r.0 == 0x05),
            "the watchdog ended the sleep, the timer wake finds none: {resets:?}"
        );
        assert!(m.mcu_powered());
        assert_eq!(rtc_sleep::sleeping(&m.soc.devices.rtc_cntl), None);
        let sleeps: Vec<u64> = sleep_events(&m).iter().map(|e| e.0).collect();
        assert_eq!(sleeps, vec![SLEEP_EVENT_DEEP, WAKE_EVENT_DEEP]);
    }

    #[test]
    fn a_deep_sleep_rwdt_with_pause_holds_and_is_scheduled_after_the_wake() {
        let mut m = machine();
        prepare_deep_sleep(&mut m, Some(1_000));
        arm_rwdt(&mut m, 500, true);
        let out = m.run(until(VTime::from_ms(1_200), reset_stop()));
        assert_eq!(out.reason, StopReason::Matcher(RESET));
        assert_eq!(m.soc.devices.rtc_cntl.reset_cause(), ResetCause::DEEPSLEEP);
        assert!(
            m.soc.devices.rtc_cntl.wdt_stage().is_some(),
            "the RWDT is scheduled after the 0x05 wake"
        );
        let wake = m.now();
        let _ = m.run(until(VTime::from_ms(3_000), StopSet::default()));
        let rtc = reset_events(&m)
            .into_iter()
            .find(|r| r.0 == 0x10)
            .expect("the held count runs out after the wake");
        assert!(rtc.1 > wake, "not during the sleep");
        assert!(rtc.1 < VTime(wake.0 + VTime::from_ms(600).0), "{rtc:?}");
    }
}
