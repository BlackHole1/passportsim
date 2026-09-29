//! Wiring application and reset sequencing.
//!
//! A peripheral write or event returns one `Wiring` effect, applied before the next instruction.
//! [`Machine::apply_wiring`] is the one `match` over it; each arm calls the `pemu_soc_c3::wiring`
//! module that owns the effect, so this file decides *when* and the SoC decides *what*.
//!
//! [`Machine::chip_reset`] sequences one [`ResetKind`] in the six steps numbered in its body. The
//! domains a reset leaves alone are left alone by construction: SRAM and RTC RAM are arena bytes
//! no step writes, the flash store is untouched, and the board keeps its rail, panel and codec. No
//! reset powers SRAM down (deep-sleep entry and power loss clear it themselves). Instruction count
//! and virtual time run on: a reset is not a time origin.

use pemu_board::traits::BoardPorts;
use pemu_core::hostio::{EventKind, HostEvent};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner};
use pemu_core::time::VTime;
use pemu_rv32::csr::{CSR_MPCCR, CSR_MPCER, CSR_MPCMR, Csr, CsrOp};
use pemu_rv32::spmon::{SpMonitor, SpSpill};
use pemu_soc_c3::mem::ROM_BASE;
use pemu_soc_c3::periph::{Wiring, ledc};
use pemu_soc_c3::regs::Ports;
use pemu_soc_c3::wiring::protection::Protection;
use pemu_soc_c3::wiring::{
    adc, clock as clock_wiring, flash, gpio as gpio_wiring, i2c, mmu, reset, usj,
};

use pemu_core::clock::{CacheVariant, TimingProfile};
use pemu_rv32::cost::InsnCosts;
use pemu_rv32::engine::FetchWatch;
use pemu_soc_c3::cold::{self, CacheAccount, CacheModel, CacheTiming};
use pemu_soc_c3::mem;
use pemu_soc_c3::periph::flash_xmc::FlashTiming;
use pemu_soc_c3::periph::{Block, block::Extmem};

use crate::config::ConfigError;
use crate::machine::Machine;

/// Virtual time a committed USB Serial/JTAG IN packet waits for the host poll under the `device`
/// profile.
///
/// Class B, fitted: 55.5 us, the `usj_drain_ps` row of `specs/timing-profiles.toml`, whose basis
/// carries the fit against `probe_campaign_timing` on a macOS host. It is an effective poll that
/// absorbs the emulated CPU work between packets, so it moves with `cpi_milli`.
///
/// The 1 ms full-speed USB frame is the wrong unit. The ROM's `usb_uart_tx_one_char` gives up
/// after 5000 polls and drops the character; over the `probe_intc` and `probe_clocks` images the
/// console text matches the `fast` profile up to 200 us and loses lines from 225 us on, because
/// those polls take about 0.2 ms at 160 MHz.
const USJ_DRAIN_PS: VTime = VTime(55_500_000);

/// The largest drain, in microseconds, at which the `probe_intc` and `probe_clocks` console text
/// still matches `fast` (found by a manual sweep).
pub const USJ_DRAIN_TEXT_SAFE_US: u64 = 200;

/// A drain over the ceiling makes the `device` profile print less than `fast`: a broken build.
const _: () = assert!(USJ_DRAIN_PS.0 < USJ_DRAIN_TEXT_SAFE_US * 1_000_000);

/// Refuses a timing profile the guest cannot run under: a table or calibration drain at or over
/// [`USJ_DRAIN_TEXT_SAFE_US`] (the ROM drops console characters), or a CPI of 0.
pub(crate) fn check_timing(profile: &TimingProfile) -> Result<(), ConfigError> {
    let ceiling_ps = USJ_DRAIN_TEXT_SAFE_US * 1_000_000;
    if profile.usj_drain_ps >= ceiling_ps {
        return Err(ConfigError::Timing(format!(
            "usj_drain_ps {} is at or over the {USJ_DRAIN_TEXT_SAFE_US} us at which the ROM \
             drops console characters",
            profile.usj_drain_ps
        )));
    }
    if profile.cpi_milli == 0 {
        return Err(ConfigError::Timing("cpi_milli is 0".to_string()));
    }
    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResetDone {
    pub kind: ResetKind,
    pub after: reset::AfterReset,
    pub gpio_pins_published: u32,
    pub cpu_hz: u32,
}

impl Machine {
    pub(crate) fn apply_wiring(&mut self, w: Wiring) {
        let now = self.now();
        let applied = match &w {
            Wiring::None => return,
            Wiring::ClockChanged => {
                let reset_hz = self.reset_cpu_hz();
                clock_wiring::apply(
                    &self.soc.devices.system,
                    reset_hz,
                    &mut self.clock,
                    self.hart.pos(),
                );
                // The MMIO rows are APB cycles, so their CPU-cycle charge follows the clocks.
                self.apply_insn_costs();
                true
            }
            Wiring::MmuEntry(_) | Wiring::CacheCtrl => {
                mmu::handle(&mut self.soc, &w);
                self.drain_invalidations();
                true
            }
            Wiring::FlashWritten { .. } => {
                flash::handle(&mut self.soc, &w);
                self.drain_invalidations();
                true
            }
            Wiring::ProtectionChanged => {
                self.refold_protection();
                true
            }
            Wiring::GpioChanged => {
                gpio_wiring::apply(&mut self.soc.devices.gpio, &mut self.board, now);
                true
            }
            Wiring::LedcChanged => {
                // The effect names no channel, so every channel is published; a repeated value
                // changes nothing the board shows.
                for ch in 0..ledc::CHANNELS {
                    let out = self.soc.devices.ledc.channel(ch);
                    self.board
                        .ledc(now, out.channel, out.duty, out.duty_res, out.freq_hz);
                }
                self.publish_frame();
                true
            }
            Wiring::UsjIo => {
                usj::pump(&mut self.soc.devices.usj, &mut self.io, now, &mut self.irq);
                true
            }
            Wiring::SpMonitor(monitor) => {
                // The engine re-pulls `Hart::spmon` from the bus before the machine applies this,
                // so the hart's copy is set here too.
                self.soc.set_sp_monitor(*monitor);
                self.hart.spmon = *monitor;
                true
            }
            Wiring::AdcSample { unit, channel } => {
                adc::sample(
                    &mut self.soc.devices.saradc,
                    now,
                    *unit,
                    *channel,
                    &self.board,
                );
                self.soc.devices.saradc.sync_irq(&mut self.irq);
                true
            }
            Wiring::I2cRun => {
                let clocked = self.profile.i2c_clocked;
                let (_, held) =
                    i2c::run_timed(&mut self.soc.devices.i2c0, now, &mut self.board, clocked);
                if clocked {
                    // The bits the driver polls arrive by event at the list's bus time.
                    let bus_ps = held
                        .periods
                        .saturating_mul(self.soc.devices.i2c0.scl_period_ps());
                    self.sched.schedule(
                        now,
                        VTime(now.0.saturating_add(bus_ps)),
                        EventKey {
                            owner: Owner::Periph(<pemu_soc_c3::periph::i2c0::Model as pemu_soc_c3::periph::Peripheral>::ID),
                            tag: held.tag(),
                        },
                    );
                }
                self.soc.devices.i2c0.sync_irq(&mut self.irq);
                true
            }
            Wiring::ChipReset(kind) => {
                self.chip_reset(*kind);
                true
            }
            Wiring::Spi2Transfer => {
                self.apply_spi2_transfer(now);
                true
            }
            Wiring::I2sPeriod(dir) => {
                self.apply_i2s_period(*dir, now);
                true
            }
            Wiring::ShaDma => {
                self.apply_sha_dma(now);
                true
            }
            Wiring::AesDma => {
                self.apply_aes_dma(now);
                true
            }
            Wiring::SleepEnter(kind) => self.enter_sleep(*kind),
        };
        if applied {
            self.applied_wiring.note(&w);
        } else {
            self.unapplied_wiring.note(&w);
        }
    }

    /// Folds the hart's PMP CSRs and the SoC's PMS split into the page table and hands the pages
    /// whose execute permission changed to the engine.
    pub(crate) fn refold_protection(&mut self) {
        let pms = self.soc.pms;
        Protection::from_csr(&self.hart.csr, pms).apply(&mut self.soc);
        self.drain_invalidations();
    }

    pub(crate) fn cpu_hz(&self) -> u32 {
        clock_wiring::cpu_hz(&self.soc.devices.system, self.reset_cpu_hz())
    }

    /// The CPU frequency of the reset state of `SYSCLK_CONF`: XTAL/2 = 20 MHz, which the
    /// `PRE_DIV_CNT` reset value of 1 in `specs/c3-registers.csv` decodes to through
    /// `rtc_clk_cpu_freq_get_config`. The ROM writes neither `SYSCLK_CONF` nor `CPU_PER_CONF`, so
    /// the whole ROM runs at this rate. Some documentation says 40 MHz; device captures decide
    /// against it.
    ///
    /// Under `device`, console drain waits span fixed virtual time, so ROM timestamps depend on the
    /// rate. Anchors on `probe_intc` / `probe_clocks` (ms):
    ///
    /// | image | 40 MHz | XTAL/2 | device |
    /// |---|---|---|---|
    /// | `probe_intc` | 41, 59, 68 | 21, 39, 47 | 24, 58, 67 |
    /// | `probe_clocks` | 41, 60, 68 | 21, 40, 48 | 24, 60, 69 |
    ///
    /// The 40 MHz match at the later anchors is two errors cancelling: add back the unmodelled
    /// bootloader segment verify (about 15 ms) and 40 MHz reads 41, 74, 83, wrong everywhere, while
    /// XTAL/2 reads 21, 54, 62, early by 3 to 5 ms with one sign.
    pub(crate) fn reset_cpu_hz(&self) -> u32 {
        self.cfg.board.xtal_hz / 2
    }

    pub(crate) fn chip_reset(&mut self, kind: ResetKind) -> ResetDone {
        // A reset the RTC domain raises in deep sleep (the RWDT) powers the digital domain up.
        self.leave_deep_sleep_for_reset();
        let now = self.now();
        // 1. The record the next boot prints as `Saved PC`.
        let (pc, sp) = (self.hart.pc, self.hart.x[2]);
        self.soc.devices.assist_debug.record_reset(pc, sp);

        let after = self.with_bus(|bus, _| {
            reset::fan_out(&mut bus.inner.soc.devices, kind, &mut bus.inner.cx.periph)
        });

        // 3. The hart. Every reset cause resets the CPU.
        self.hart.x = [0; 32];
        self.hart.pc = ROM_BASE;
        self.hart.csr = Csr::new();
        // The counter state the SoC keeps beside the CSR store goes back to reset values, so the
        // counter is stopped as after power-on.
        let insns = self.hart.pos();
        for (csr, value) in [
            (CSR_MPCER, self.hart.csr.mpcer),
            (CSR_MPCMR, self.hart.csr.mpcmr),
            (CSR_MPCCR, 0),
        ] {
            self.soc
                .counter_csr(&mut self.clock, csr, CsrOp::Write(value), insns)
                .expect("the performance counter CSRs are modelled");
        }
        self.hart.wfi = false;
        self.hart.spmon = SpMonitor::default();
        self.hart.pipe = Default::default();
        self.engine.flush();
        self.resume_breakpoint = None;
        // A new boot spends the radio MMIO allowance again, and an ELF-less image that keeps
        // panicking the same way stops.
        self.hle.on_reset();
        self.note_reset_panic(pc);

        // 4. What the fan-out obliges the caller to re-apply.
        if after.rebase_clock {
            let reset_hz = self.reset_cpu_hz();
            clock_wiring::apply(
                &self.soc.devices.system,
                reset_hz,
                &mut self.clock,
                self.hart.pos(),
            );
        }
        let gpio_pins_published = if after.republish_gpio {
            gpio_wiring::apply(&mut self.soc.devices.gpio, &mut self.board, now)
        } else {
            0
        };

        // 5. State outside the models: MMU windows, PMP permissions, the stack monitor the reset
        // guard now holds, and the USJ link a block reset dropped.
        mmu::apply_all(&mut self.soc);
        self.refold_protection();
        let monitor = self.soc.devices.assist_debug.monitor();
        self.soc.set_sp_monitor(monitor);
        self.hart.spmon = monitor;
        self.apply_usb_ctrl();
        // The fan-out cleared what the console blocks had in flight (and their scheduler
        // events); re-state the profile so a reset never leaves a half-armed drain or an unpaced
        // console.
        self.apply_timing_profile();
        // The brownout detector is a level, judged again after the register reset.
        self.check_soc_brownout();
        self.drain_invalidations();

        self.io.events.emit(HostEvent {
            kind: EventKind::Reset,
            vt: now,
            arg: u64::from(kind.cause.0),
        });
        self.resets += 1;
        ResetDone {
            kind,
            after,
            gpio_pins_published,
            cpu_hz: self.clock.cpu_hz(),
        }
    }

    /// Applies the profile's console TX pacing. The ROM and bootloader wait for the console to
    /// finish sending, so on silicon every printed line costs its transmission time; the models
    /// keep the transmitter busy for a scheduled interval and the guest's own poll consumes it.
    /// `fast` paces nothing; `device` sends one UART0 byte per baud period and holds a USJ IN
    /// packet [`USJ_DRAIN_PS`].
    pub(crate) fn apply_console_pacing(&mut self) {
        let now = self.now();
        self.soc
            .devices
            .uart0
            .set_tx_pacing(self.profile.uart_paced, &mut self.sched);
        self.soc.devices.usj.set_drain_interval(
            VTime(self.profile.usj_drain_ps),
            now,
            &mut self.sched,
        );
        // Host SOF delays after the link returns (deep-sleep wake, plug) and after a `SYS_` reset.
        // A delay already running keeps its deadline.
        let usj = &mut self.soc.devices.usj;
        usj.set_enumeration_delay(VTime(self.profile.usj_enum_wake_ps));
        usj.set_reset_enumeration_delay(VTime(self.profile.usj_enum_reset_ps));
    }

    /// Applies the profile durations that block models hold as settings. Re-applied after every
    /// reset, because some blocks keep them beside registers a reset restores.
    pub(crate) fn apply_timing_profile(&mut self) {
        self.apply_console_pacing();
        self.apply_slow_clock();
        let p = &self.profile;
        let d = &mut self.soc.devices;
        d.sha.set_block_ps(p.sha_block_ps);
        d.aes.set_op_ps(p.aes_block_ps);
        d.rsa.set_op_ps(p.rsa_op_ps);
        d.spi1.chip_mut().set_timing(FlashTiming {
            pp_ps: p.flash_pp_ps,
            se_ps: p.flash_se_ps,
            be_ps: p.flash_be_ps,
            ce_ps: p.flash_ce_ps,
        });
        self.apply_cache_model();
        self.apply_insn_costs();
    }

    /// Installs the profile's class cost table on the engine: MMIO rows at the current clocks,
    /// EXTMEM as the local MMIO window (off the APB path), and SRAM Block 1 as the block where code
    /// fetches and data accesses contend. `fast`'s rows are all 0, so the clock position is the
    /// instruction count.
    pub(crate) fn apply_insn_costs(&mut self) {
        let p = &self.profile;
        let cpu_hz = self.cpu_hz();
        let apb_hz = self.soc.devices.system.apb_hz();
        let cpu = p.mmio_cpu_cycles;
        self.engine.set_costs(Some(InsnCosts {
            taken_branch: p.taken_branch_cycles,
            jump: p.jump_cycles,
            split_redirect: p.split_redirect_cycles,
            load_use: p.load_use_cycles,
            div_base: p.div_base_cycles,
            mulh: p.mulh_cycles,
            mmio_load: mmio_extra_cycles(cpu, p.mmio_load_apb_cycles, cpu_hz, apb_hz),
            mmio_store: mmio_extra_cycles(cpu, p.mmio_store_apb_cycles, cpu_hz, apb_hz),
            mmio_local: mmio_extra_cycles(cpu, 0, cpu_hz, apb_hz),
            local_mmio_base: <Extmem as Block>::BASE,
            local_mmio_len: <Extmem as Block>::SIZE,
            bank: p.sram_bank_cycles,
            bank_code_base: mem::SRAM1_IRAM_BASE,
            bank_data_base: mem::SRAM1_DRAM_BASE,
            bank_len: mem::SRAM_BLOCK1_LEN,
        }));
    }

    /// Installs the profile's flash-cache model, emptied by a reset as on silicon, and a
    /// [`FetchWatch`] over the IROM window when the variant charges fetches.
    pub(crate) fn apply_cache_model(&mut self) {
        let p = &self.profile;
        let model = match p.cache_model {
            CacheVariant::ColdPage => CacheModel::ColdPage,
            CacheVariant::Lru16k => CacheModel::Lru16k,
            CacheVariant::Fifo16k => CacheModel::Fifo16k,
        };
        self.soc.cache = CacheAccount::with_timing(
            model,
            CacheTiming {
                fill_ps: p.cache_fill_ps,
                first_word_ps: p.cache_first_word_ps,
                miss_cycles: p.cache_miss_cycles,
            },
        );
        let watch = self.soc.cache.charges_fetches().then_some(FetchWatch {
            base: mem::FLASH_IROM_BASE,
            len: mem::FLASH_WINDOW_LEN,
            line_shift: cold::LINE_SHIFT,
        });
        self.engine.set_fetch_watch(watch);
    }

    /// Gives RTC_CNTL and both timer groups the profile's slow clock rate. Also called after a
    /// restore, which rebuilds the models without the rate (it is configuration, not state).
    pub(crate) fn apply_slow_clock(&mut self) {
        let hz = u64::from(self.profile.rtc_slow_hz);
        let d = &mut self.soc.devices;
        d.rtc_cntl.set_slow_hz(hz);
        d.timg0.set_slow_hz(hz);
        d.timg1.set_slow_hz(hz);
    }

    pub(crate) fn apply_usb_ctrl(&mut self) {
        let now = self.now();
        // The USJ PHY is powered down in deep sleep as well as with the rail.
        let rail_on = self.usj_phy_powered();
        usj::apply_ctrl(
            &mut self.soc.devices.usj,
            &mut self.io,
            rail_on,
            now,
            &mut self.sched,
            &mut self.irq,
        );
    }

    /// Latches the stack-guard violation the engine reported into ASSIST_DEBUG (`INTR_RAW`,
    /// `SP_PC`) and raises source 54, which makes the IDF print "Stack protection fault".
    pub(crate) fn note_spill(&mut self, spill: SpSpill) {
        let pc = self.engine.spill_pc();
        let now = self.now();
        let mut ports = Ports {
            now,
            sched: &mut self.sched,
            irq: &mut self.irq,
            ledger: &mut self.ledger,
        };
        self.soc
            .devices
            .assist_debug
            .record_spill(spill, pc, &mut ports);
    }

    /// Always 0 since the engine names the violating pc; kept because the snapshot format carries
    /// it.
    pub fn unrecorded_spills(&self) -> u64 {
        self.unrecorded_spills
    }

    pub fn applied_wiring_by_kind(&self) -> crate::wiring_counts::WiringCounts {
        self.applied_wiring
    }

    pub fn resets(&self) -> u64 {
        self.resets
    }
}

/// The CPU cycles beyond its own one that a peripheral access costs: `cpu_cycles` plus
/// `apb_cycles` rounded up to CPU cycles, less one. The APB part is 0 when a clock reads 0.
pub(crate) fn mmio_extra_cycles(cpu_cycles: u32, apb_cycles: u32, cpu_hz: u32, apb_hz: u32) -> u32 {
    let apb = if apb_cycles == 0 || cpu_hz == 0 || apb_hz == 0 {
        0
    } else {
        (u64::from(apb_cycles) * u64::from(cpu_hz)).div_ceil(u64::from(apb_hz))
    };
    let all = u64::from(cpu_cycles) + apb;
    u32::try_from(all.saturating_sub(1)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::mmio_extra_cycles;

    /// `probe_campaign_timing` measured 6 and 8 CPU cycles for a GPIO read and write at CPU 160 MHz
    /// and APB 80 MHz, 4 and 5 at CPU = APB, and 2 for an EXTMEM read.
    #[test]
    fn an_mmio_access_is_its_cpu_cycles_and_its_apb_cycles_at_the_clocks_ratio() {
        assert_eq!(mmio_extra_cycles(2, 2, 160_000_000, 80_000_000), 5);
        assert_eq!(mmio_extra_cycles(2, 3, 160_000_000, 80_000_000), 7);
        assert_eq!(mmio_extra_cycles(2, 2, 80_000_000, 80_000_000), 3);
        assert_eq!(mmio_extra_cycles(2, 3, 80_000_000, 80_000_000), 4);
        assert_eq!(mmio_extra_cycles(2, 0, 160_000_000, 80_000_000), 1);
        assert_eq!(mmio_extra_cycles(2, 3, 20_000_000, 20_000_000), 4);
        assert_eq!(mmio_extra_cycles(0, 0, 160_000_000, 80_000_000), 0);
        assert_eq!(mmio_extra_cycles(2, 3, 0, 80_000_000), 1);
    }

    use pemu_core::irq_source::irq;
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_rv32::bus::{Bus, HartView};
    use pemu_soc_c3::periph::i2c0::{Cmd, Op};

    use crate::config::{Assets, MachineConfig};
    use crate::machine::Machine;

    const I2C0: u32 = 0x6001_3000;
    const SARADC: u32 = 0x6004_0000;

    fn machine() -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        Machine::new(MachineConfig::default(), assets).expect("the ROM fits the ROM window")
    }

    fn store_and_apply(m: &mut Machine, writes: &[(u32, u32)]) {
        let view = HartView {
            insns: 0,
            extra: 0,
            pc: 0,
        };
        m.with_bus(|bus, _| {
            for &(addr, val) in writes {
                bus.store_slow(addr, 4, val, &view);
            }
        });
        m.apply_pending_wiring();
    }

    #[test]
    fn the_i2c_run_arm_drives_source_29() {
        const INT_NACK: u32 = 1 << 10;
        let mut m = machine();
        let cmds = [
            Cmd {
                byte_num: 0,
                ack_en: false,
                ack_val: false,
                op: Op::Restart,
            },
            Cmd {
                byte_num: 1,
                ack_en: true,
                ack_val: false,
                op: Op::Write,
            },
            Cmd {
                byte_num: 0,
                ack_en: false,
                ack_val: false,
                op: Op::Stop,
            },
        ];
        // A gated I2C0 drops every write, so clock it first (SYSTEM_PERIP_CLK_EN0 bit 7).
        let en0 = m.soc.devices.system.perip_clk_en(0);
        let mut writes = vec![(0x600C_0010, en0 | 1 << 7), (I2C0 + 0x028, INT_NACK)];
        for (i, c) in cmds.iter().enumerate() {
            writes.push((I2C0 + 0x058 + 4 * i as u32, c.encode()));
        }
        // No chip of the board answers 0x20 (codec 0x18, gauge 0x63).
        writes.push((I2C0 + 0x01C, 0x20 << 1));
        store_and_apply(&mut m, &writes);
        assert!(!m.irq.source(irq::I2C_EXT0), "nothing ran yet");
        store_and_apply(&mut m, &[(I2C0 + 0x004, (1 << 11) | (1 << 5))]);
        assert!(m.applied_wiring_by_kind().i2c_run > 0, "the list ran");
        assert!(m.irq.source(irq::I2C_EXT0), "the NACK asserts source 29");
    }

    #[test]
    fn the_profile_selects_the_console_pacing_and_a_reset_keeps_it() {
        use pemu_core::reset::{ResetCause, ResetKind};
        use pemu_core::time::VTime;

        let fast = machine();
        assert!(!fast.soc.devices.uart0.tx_paced());
        assert_eq!(fast.soc.devices.usj.drain_interval(), VTime(0));

        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let cfg = MachineConfig {
            profile: crate::config::TimingProfileId::Device,
            ..MachineConfig::default()
        };
        let mut m = Machine::new(cfg, assets).expect("the ROM fits the ROM window");
        assert!(m.soc.devices.uart0.tx_paced(), "the device profile paces");
        assert_eq!(
            m.soc.devices.usj.drain_interval(),
            super::USJ_DRAIN_PS,
            "and gives the endpoint its host poll"
        );

        m.chip_reset(ResetKind::of(ResetCause::RTC_SW_SYS).expect("a documented reset cause"));
        assert!(m.soc.devices.uart0.tx_paced());
        assert_eq!(m.soc.devices.usj.drain_interval(), super::USJ_DRAIN_PS);
        assert_eq!(
            m.soc.devices.rtc_cntl.slow_hz(),
            u64::from(pemu_core::clock::TimingProfile::device().rtc_slow_hz)
        );
        assert_eq!(
            fast.soc.devices.rtc_cntl.slow_hz(),
            pemu_soc_c3::periph::rtc_cntl::SLOW_HZ
        );
    }

    #[test]
    fn the_table_drain_is_the_chosen_interval_and_the_ceiling_is_checked_at_run_time() {
        use crate::config::ConfigError;
        use pemu_core::clock::TimingProfile;
        let device = TimingProfile::device();
        assert_eq!(device.usj_drain_ps, super::USJ_DRAIN_PS.0);
        assert!(super::check_timing(device).is_ok());
        assert!(super::check_timing(TimingProfile::fast()).is_ok());
        let at_ceiling = TimingProfile {
            usj_drain_ps: super::USJ_DRAIN_TEXT_SAFE_US * 1_000_000,
            ..device.clone()
        };
        assert!(matches!(
            super::check_timing(&at_ceiling),
            Err(ConfigError::Timing(_))
        ));
        let no_cpi = TimingProfile {
            cpi_milli: 0,
            ..device.clone()
        };
        assert!(super::check_timing(&no_cpi).is_err());
    }

    #[test]
    fn the_adc_sample_arm_drives_source_43() {
        const INT_ADC1_DONE: u32 = 1 << 31;
        const ONETIME: u32 = (3 << 23) | (1 << 31);
        let mut m = machine();
        store_and_apply(
            &mut m,
            &[(SARADC + 0x040, INT_ADC1_DONE), (SARADC + 0x020, ONETIME)],
        );
        assert!(!m.irq.source(irq::APB_ADC), "nothing converted yet");
        store_and_apply(&mut m, &[(SARADC + 0x020, ONETIME | (1 << 29))]);
        assert!(
            m.applied_wiring_by_kind().adc_sample > 0,
            "the conversion ran"
        );
        assert!(m.irq.source(irq::APB_ADC), "the done bit asserts source 43");
    }
}
