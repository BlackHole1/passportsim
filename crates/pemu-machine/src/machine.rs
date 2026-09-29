//! The machine facade and `MachineApi`, the same facade as a trait.
//!
//! This file keeps composition, the facade's inherent methods (`input_from` is the one journal
//! append site) and event dispatch. The run loop and its parts live beside it (`run.rs`,
//! `executor.rs`, `apply.rs`, `stops.rs`, `poll_ff.rs`, `hang.rs`, `rom_delay.rs`, `sleep.rs`).
//!
//! Console bytes reach [`HostIo`] only through their models (UART0 transmit ring, USJ capture),
//! so which bytes a host sees is the model's decision.

use std::sync::Arc;

use pemu_board::ladder::AdcCal;
use pemu_board::passport::{BOARD_CHIP, PassportBoard};
use pemu_board::traits::BoardCx;
use pemu_core::clock::{Clock, TimingProfile};
use pemu_core::fidelity::FidelityLedger;
use pemu_core::hostio::HostIo;
use pemu_core::input::{EnvChange, InputEvent, NetRoute, SerialChan};
use pemu_core::journal::{Journal, Origin};
use pemu_core::reset::{ResetCause, ResetKind};
use pemu_core::rng::{DetRng, RngStream};
use pemu_core::sched::{Owner, Scheduler};
use pemu_core::time::VTime;
use pemu_core::trace::TraceSink;
use pemu_rv32::csr::Csr;
use pemu_rv32::engine::{Engine, HookSet};
use pemu_rv32::exec::Hart;
use pemu_rv32::spmon::SpMonitor;
use pemu_soc_c3::dma::DmaView;
use pemu_soc_c3::flash_store::FlashStore;
use pemu_soc_c3::r#gen::regs_efuse::idx as efuse_idx;
use pemu_soc_c3::intc::IrqFabric;
use pemu_soc_c3::mem::ROM_BASE;
use pemu_soc_c3::periph::{Cx, DeviceVisitor, Peripheral, Wiring};
use pemu_soc_c3::{Soc, SocBus, SocCx, SocError};

use crate::config::{Assets, ConfigError, EfuseSource, MachineConfig};
use crate::disable::DisabledModels;
use crate::poll_ff::PollTracker;
use crate::rom_delay::RomDelay;

/// Bit of the two-bit `EFUSE_WDT_DELAY_SEL` in `EFUSE_RD_REPEAT_DATA1`.
const WDT_DELAY_SEL_SHIFT: u32 = 16;
use crate::apply::ResetDone;
use crate::executor::{Executor, engine_for};
use crate::run::{IdleAction, IdlePolicy, RunLimits};
use crate::stops::{ArmedStops, StopReason};
use crate::wiring_counts::WiringCounts;

mod api;
mod bus;
mod input;
mod receipt;
#[cfg(all(test, feature = "bundled-rom"))]
mod rom_tests;

pub use api::{At, GuestMem, InputError, MachineApi};
pub(crate) use bus::ConsoleTap;
pub use bus::{MachineBus, MmioRead};
pub use receipt::{ClassesTouched, FaultCounters, Receipt, UNMODELED_LIMIT};

/// Bytes of every [`pemu_core::hostio::ByteRing`] of a machine. UNVERIFIED design parameter:
/// 64 KiB holds far more than the ROM and bootloader banner text of a bring-up run.
const BYTE_RING_CAPACITY: usize = 64 * 1024;

/// `EFUSE_WDT_DELAY_SEL` out of the eFuse read registers.
pub(crate) fn wdt_delay_sel(
    regs: &pemu_core::regstore::RegStore<{ pemu_soc_c3::r#gen::regs_efuse::REG_COUNT }>,
) -> u8 {
    ((regs.get(efuse_idx::EFUSE_RD_REPEAT_DATA1) >> WDT_DELAY_SEL_SHIFT) & 0x3) as u8
}

/// The ADC calibration the curve-fitting scheme reads, decoded from the eFuse BLK2 read registers:
/// `BLK_VERSION_MAJOR` is word 4 bits 1:0, and `ADC1_CAL_VOL_ATTEN3` is 10 bits split over word 6
/// bits 31:26 (low six) and word 7 bits 3:0 (high four). The synthesized image decodes to
/// `AdcCal::default()`.
pub(crate) fn adc_calibration(
    regs: &pemu_core::regstore::RegStore<{ pemu_soc_c3::r#gen::regs_efuse::REG_COUNT }>,
) -> AdcCal {
    let data4 = regs.get(efuse_idx::EFUSE_RD_SYS_PART1_DATA4);
    let data6 = regs.get(efuse_idx::EFUSE_RD_SYS_PART1_DATA6);
    let data7 = regs.get(efuse_idx::EFUSE_RD_SYS_PART1_DATA7);
    AdcCal {
        blk_version_major: (data4 & 0x3) as u8,
        cal_vol_atten3: (((data6 >> 26) & 0x3F) | ((data7 & 0xF) << 6)) as u16,
    }
}

/// One emulated AI Passport: SoC, board, HLE and radios, composed and driven by the run loop.
pub struct Machine {
    pub(crate) cfg: MachineConfig,
    /// Kept because the ROM symbol table names a stalling pc and the snapshot header hashes the
    /// images. Behind an `Arc` so a fork shares the ROM and flash base images.
    pub(crate) assets: Arc<Assets>,
    pub(crate) hart: Hart,
    pub(crate) soc: Soc,
    pub(crate) board: PassportBoard,
    /// Virtual time, derived from the instruction count.
    pub(crate) clock: Clock,
    pub(crate) sched: Scheduler,
    pub(crate) rng: DetRng,
    pub(crate) profile: TimingProfile,
    pub(crate) ledger: FidelityLedger,
    pub(crate) trace: TraceSink,
    pub(crate) io: HostIo,
    pub(crate) journal: Journal,
    pub(crate) idle: Box<dyn IdlePolicy + Send>,
    /// Cumulative idle picoseconds at the start of the last `run` call, so a `RunOutcome` reports
    /// this call's idle time.
    pub(crate) idle_ps_at_run_start: u64,
    /// Ledger cursor of the last `receipt` call, so the next one drains only what is new.
    pub(crate) receipt_cursor: u64,
    /// On the machine and not on [`Soc`] because a slow path hands it to a peripheral while
    /// [`SocBus`] already holds `&mut Soc`.
    pub(crate) irq: IrqFabric,
    /// What the bus keeps between accesses for [`Machine::last_mmio_read`].
    pub(crate) tap: ConsoleTap,
    /// Journaled inputs that came due and reached no model ([`Machine::apply_due_journal`]).
    pub(crate) unapplied_inputs: u64,
    /// Board effects that came due and were not acted on ([`Machine::apply_due_journal`]).
    pub(crate) pending_board_effects: u64,
    /// Test-only chip supply for the SoC brownout detector: the board rail cuts off before the
    /// supply can drop below 2.51 V, so the level semantics are exercised through this.
    #[cfg(test)]
    pub(crate) test_supply_mv: Option<u32>,
    /// Named board events that reached no host event ring.
    pub(crate) pending_board_events: u64,
    /// Scheduled events that came due and reached no model ([`Machine::dispatch_due_events`]).
    pub(crate) undispatched_events: u64,
    /// Wiring effects produced and not applied (dispatched events plus effects left in the SoC).
    pub(crate) unapplied_wiring: WiringCounts,
    /// The most effects [`Machine::take_unapplied_wiring`] ever found waiting at once.
    pub(crate) peak_pending_wiring: u64,
    /// The stop an unapplied `Wiring::ChipReset` or `Wiring::SleepEnter` owes the run loop.
    pub(crate) wiring_stop: Option<StopReason>,
    pub(crate) engine: Engine,
    /// Hooks the engine turns into `K_HOOK` exits: HLE bindings, observe hooks, the ROM delay
    /// loop head and the current stop set's breakpoints.
    pub(crate) hooks: HookSet,
    pub(crate) hle: crate::hle::MachineHle,
    pub(crate) executor: Executor,
    /// The breakpoint pc a run stopped at, which the next execution runs past exactly once.
    pub(crate) resume_breakpoint: Option<u32>,
    pub(crate) applied_wiring: WiringCounts,
    /// Stack-guard violations the engine reported and the machine could not latch.
    pub(crate) unrecorded_spills: u64,
    /// DMA accesses that named a byte no mapped page backs.
    pub(crate) dma_faults: u64,
    /// I2S services refused for an undecoded sample width.
    pub(crate) pcm_width_faults: u64,
    /// Resets sequenced, power-on included.
    pub(crate) resets: u64,
    pub(crate) mcu_powered: bool,
    /// Events that came due while the MCU was powered down and were dropped.
    pub(crate) unpowered_events: u64,
    pub(crate) armed: ArmedStops,
    pub(crate) poll: PollTracker,
    pub(crate) poll_ff: bool,
    pub(crate) rom_delay: RomDelay,
    /// Instructions credited by poll and ROM delay fast-forward since the machine was built.
    pub(crate) ff_insns: u64,
    pub(crate) ff_insns_at_run_start: u64,
    pub(crate) disabled: DisabledModels,
    /// The most instructions one executor slice runs. A host choice: results never depend on it.
    pub(crate) max_slice: u64,
}

/// Delivers one scheduled event to the block that owns it.
struct OnEvent<'a, 'cx> {
    tag: u16,
    cx: &'a mut Cx<'cx>,
    out: Wiring,
}

impl DeviceVisitor for OnEvent<'_, '_> {
    fn visit<P: Peripheral>(&mut self, dev: &mut P) {
        self.out = dev.on_event(self.tag, self.cx);
    }
}

impl Machine {
    /// Build a machine from its configuration and assets.
    ///
    /// The eFuse image ([`Assets::efuse`]) decides the taint, so a [`MachineConfig::efuse`] that
    /// disagrees is refused as [`ConfigError::EfuseMismatch`] rather than building a `Dump`
    /// machine that reports itself untainted. A flash image longer than the 8 MB part is refused
    /// as `ConfigError::Soc`. Without `app_elf` only the symbol-free HLE stops bind.
    pub fn new(cfg: MachineConfig, assets: Assets) -> Result<Machine, ConfigError> {
        Machine::compose(cfg, Arc::new(assets), None)
    }

    /// [`Machine::new`] over shared assets and an optional prebuilt flash store: what `fork` uses.
    pub(crate) fn compose(
        cfg: MachineConfig,
        assets: Arc<Assets>,
        flash: Option<FlashStore>,
    ) -> Result<Machine, ConfigError> {
        let tainted = assets.efuse.tainted();
        if (cfg.efuse == EfuseSource::Dump) != tainted {
            return Err(ConfigError::EfuseMismatch {
                config: cfg.efuse,
                tainted,
            });
        }

        let profile = cfg.timing();
        crate::apply::check_timing(&profile)?;

        let flash = match flash {
            Some(store) => store,
            None => FlashStore::from_bytes(assets.flash.bytes()).map_err(SocError::from)?,
        };
        let mut soc = Soc::new(flash);
        soc.load_rom(assets.rom.bytes())?;

        let mut rng = DetRng::new(cfg.seed);
        let board = PassportBoard::with_seed(&cfg.board, &mut rng);

        let hart = Hart {
            x: [0; 32],
            // UNVERIFIED: no document states the reset pc; it is the base of the only executable
            // region out of reset, and a wrong value faults on the first fetch.
            pc: ROM_BASE,
            csr: Csr::new(),
            wfi: false,
            insns: 0,
            stores: 0,
            spmon: SpMonitor::default(),
            extra: 0,
            pipe: Default::default(),
        };

        let idle = crate::sleep::idle_policy(&cfg);
        let poll_ff = cfg.poll_ff;
        let hang = cfg.hang;
        let trace = cfg.trace.sink();
        let rom_delay_head = crate::rom_delay::pinned_delay_head(&assets.rom);
        let engine = engine_for(&cfg.engine);
        // Seed rate only: `power_on` rebases the clock onto `Machine::reset_cpu_hz`. Spelled the
        // same (XTAL/2, 20 MHz on this board) so a skipped reset would still start right.
        let reset_hz = cfg.board.xtal_hz / 2;
        let hle = crate::hle::MachineHle::bind(&assets, &cfg.hle, &profile);
        let hooks = hle.bound_hooks().set.clone();

        let mut machine = Machine {
            cfg,
            assets,
            hart,
            soc,
            board,
            // Reset rate is XTAL/2 = 20 MHz, what the `PRE_DIV_CNT` reset value of 1 implies
            // (`specs/c3-registers.csv`). The ROM writes neither `SYSCLK_CONF` nor `CPU_PER_CONF`;
            // the bootloader raises the clock and that reaches us as `Wiring::ClockChanged`.
            clock: Clock::new(reset_hz, profile.cpi_milli),
            sched: Scheduler::new(),
            rng,
            profile,
            ledger: FidelityLedger::default(),
            trace,
            io: HostIo::new(BYTE_RING_CAPACITY),
            journal: Journal::new(),
            idle,
            idle_ps_at_run_start: 0,
            receipt_cursor: 0,
            irq: IrqFabric::new(),
            tap: ConsoleTap::default(),
            unapplied_inputs: 0,
            pending_board_effects: 0,
            #[cfg(test)]
            test_supply_mv: None,
            pending_board_events: 0,
            undispatched_events: 0,
            unapplied_wiring: WiringCounts::default(),
            peak_pending_wiring: 0,
            wiring_stop: None,
            engine,
            hooks,
            hle,
            executor: Executor::Engine,
            resume_breakpoint: None,
            applied_wiring: WiringCounts::default(),
            unrecorded_spills: 0,
            dma_faults: 0,
            pcm_width_faults: 0,
            resets: 0,
            mcu_powered: true,
            unpowered_events: 0,
            armed: ArmedStops::default(),
            poll: PollTracker::with_hang(hang),
            poll_ff,
            rom_delay: RomDelay {
                head: rom_delay_head,
                enabled: false,
            },
            ff_insns: 0,
            ff_insns_at_run_start: 0,
            disabled: DisabledModels::default(),
            max_slice: crate::run::MAX_SLICE_INSNS,
        };
        // On by default, because results do not depend on it.
        machine.set_rom_delay_ff(true);
        // Before the power-on reset, so no event the reset arms reaches a disabled model.
        for name in &machine.cfg.disabled_models {
            if !machine.disabled.disable(name) {
                return Err(ConfigError::UnknownModel(name.clone()));
            }
        }
        machine.power_on();
        Ok(machine)
    }

    /// The power-on reset every block sees before the first instruction.
    ///
    /// A fresh `Devices` already holds reset values, but the reset tells blocks which reset
    /// happened: RTC_CNTL latches the cause the ROM banner prints as `rst:`, and without it the
    /// ROM prints `rst:0x0 (N/A)` where the device prints `rst:0x1 (POWERON)`. It also arms the
    /// RWDT flash-boot hold. Runs the whole [`Machine::chip_reset`] sequence, after the
    /// power-on-only steps the reset reads: the eFuse image, `EFUSE_WDT_DELAY_SEL` into RTC_CNTL,
    /// and the ADC calibration into APB_SARADC.
    pub(crate) fn power_on(&mut self) -> ResetDone {
        self.board.lcd.set_powered(true);
        self.publish_frame();
        let kind = ResetKind::of(ResetCause::POWERON)
            .expect("0x01 CHIP_POWER_ON is a documented reset cause");
        // The reset reloads the `RD_*` shadow from the eFuse array, so the image goes in first.
        let words = self.assets.efuse.dump_words();
        self.soc.devices.efuse.load_image(&words);
        // Scales the RWDT stage 0 hold. RTC_CNTL cannot read another block, so the machine hands
        // it over before the fan-out arms the hold.
        let delay_sel = wdt_delay_sel(self.soc.devices.efuse.regs());
        let now = self.now();
        self.soc
            .devices
            .rtc_cntl
            .set_wdt_delay_sel(delay_sel, now, &mut self.sched);
        let cal = adc_calibration(self.soc.devices.efuse.regs());
        self.soc.devices.saradc.set_calibration(cal);
        self.chip_reset(kind)
    }

    /// Journal an input at `at`; returns its journal sequence number.
    ///
    /// The input is recorded, not applied: the run loop applies due entries in `(at, seq)` order,
    /// so a session and its replay see the same inputs at the same virtual times. An instant
    /// already past is refused rather than applied late.
    pub fn input(&mut self, at: At, ev: InputEvent) -> Result<u64, InputError> {
        // The frozen signature carries no origin, so inputs through here are `Origin::Agent`.
        self.input_from(at, Origin::Agent, ev)
    }

    /// [`Machine::input`] with the [`Origin`] the input came from; the journal raises the run's
    /// class to what the origin implies. The one path that appends to the journal
    /// (`tests/journal_guard.rs`).
    pub fn input_from(
        &mut self,
        at: At,
        origin: Origin,
        ev: InputEvent,
    ) -> Result<u64, InputError> {
        let now = self.now();
        let when = match at {
            At::Now => now,
            At::Vt(t) => t,
        };
        if when < now {
            return Err(InputError {});
        }
        // Only the USJ channel has a host-to-guest ring.
        if let InputEvent::SerialIn { chan, .. } = &ev
            && *chan != SerialChan::USJ
        {
            return Err(InputError {});
        }
        // Checked here and not only in the `env` command, because `pemu_input` deserializes an
        // arbitrary `InputEvent`: a key the `SecretSet` could not hold must never be journaled in
        // the clear.
        if let InputEvent::Env(EnvChange::WifiAps(aps)) = &ev
            && aps.iter().any(|ap| ap.check().is_err())
        {
            return Err(InputError {});
        }
        if let InputEvent::Env(EnvChange::WifiBridge { attached, routes }) = &ev
            && (!NetRoute::check_all(routes) || (!attached && !routes.is_empty()))
        {
            return Err(InputError {});
        }
        Ok(self.journal.append(now, when, origin, ev))
    }

    /// The machine's input journal, read-only. After a restore it holds the snapshot's pending
    /// entries and every input since; [`Journal::next_seq`] says where the snapshot's run stood.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    pub fn io(&mut self) -> &mut HostIo {
        &mut self.io
    }

    /// Current virtual time.
    pub fn now(&self) -> VTime {
        self.clock.now(self.hart.pos())
    }

    /// Read-only view for pemu-introspect.
    pub fn guest_mem(&mut self) -> GuestMem<'_> {
        GuestMem {
            soc: Some(&self.soc),
        }
    }

    /// Whether any input this machine was built from carries device-derived bytes: an imported
    /// eFuse dump does, a synthesized one does not.
    pub fn is_tainted(&self) -> bool {
        self.assets.efuse.tainted()
    }
}

/// Parts only the run loop (`run.rs`) uses; here because they read private fields.
impl Machine {
    /// Runs `f` against the machine's [`MachineBus`] with the hart borrowable beside it. A plain
    /// `bus()` accessor cannot express that split: `Cx::dma` is a `&'a mut DmaView<'a>` whose
    /// referent must be a local of the frame holding the `Cx`. This closure is that frame.
    pub(crate) fn with_bus<R>(&mut self, f: impl FnOnce(&mut MachineBus<'_>, &mut Hart) -> R) -> R {
        self.with_bus_and_engine(|bus, hart, _, _| f(bus, hart))
    }

    /// [`Machine::with_bus`] with the engine and its hooks beside the hart.
    pub(crate) fn with_bus_and_engine<R>(
        &mut self,
        f: impl FnOnce(&mut MachineBus<'_>, &mut Hart, &mut Engine, &HookSet) -> R,
    ) -> R {
        // Detached: `SocBus` takes the whole `&mut Soc`, so a working view cannot borrow the
        // pages and arena too. A detached view reports failure rather than reading zeros; the
        // DMA joins walk guest memory through their own view (`dma.rs`).
        let mut dma = DmaView::detached();
        let now = self.clock.now(self.hart.pos());
        let mut bus = MachineBus {
            inner: SocBus {
                soc: &mut self.soc,
                board: &mut self.board,
                cx: SocCx {
                    now,
                    clock: &mut self.clock,
                    periph: Cx {
                        now,
                        sched: &mut self.sched,
                        irq: &mut self.irq,
                        dma: &mut dma,
                        rng: self.rng.stream(RngStream::GUEST_ENTROPY),
                        profile: &self.profile,
                        ledger: &mut self.ledger,
                        trace: &mut self.trace,
                    },
                },
            },
            io: &mut self.io,
            tap: &mut self.tap,
            stops: &mut self.armed,
            poll: &mut self.poll,
            disabled: &mut self.disabled,
            radio: &mut self.hle.radio,
            radio_trip: &mut self.hle.state.radio_trip,
        };
        f(&mut bus, &mut self.hart, &mut self.engine, &self.hooks)
    }

    /// Dispatches every due scheduled event in `(VTime, seq)` order, applying each one's wiring
    /// before the next. An event no model claims is counted in
    /// [`Machine::undispatched_events`], so a miss cannot pass for a delivery.
    pub(crate) fn dispatch_due_events(&mut self) {
        let now = self.now();
        while let Some(key) = self.sched.pop_due(now) {
            self.poll.invalidate();
            match key.owner {
                // A powered-down MCU has no peripheral clock: events its blocks armed do not fire.
                // RTC_CNTL is the exception while the board rail is up, since the RTC watchdog
                // keeps counting in deep sleep.
                Owner::Periph(p) if !self.periph_clocked(p) => self.unpowered_events += 1,
                Owner::Radio(_) if !self.mcu_powered => self.unpowered_events += 1,
                Owner::Periph(id) if self.disabled.contains(id) => {
                    self.disabled.dropped_events += 1;
                }
                Owner::Periph(id) => {
                    let (found, wiring) = self.with_bus(|bus, _| {
                        let mut visitor = OnEvent {
                            tag: key.tag,
                            cx: &mut bus.inner.cx.periph,
                            out: Wiring::None,
                        };
                        let found = bus.inner.soc.devices.visit(id, &mut visitor);
                        (found, visitor.out)
                    });
                    if found {
                        self.apply_wiring(wiring);
                        // Effects an event handler left on the SoC apply at the same boundary.
                        self.apply_pending_wiring();
                    } else {
                        self.undispatched_events += 1;
                    }
                }
                Owner::Chip(BOARD_CHIP) => {
                    let mut cx = BoardCx::new(now);
                    let effect = self.board.tick(now, &mut cx);
                    self.drain_board_cx(&mut cx);
                    self.apply_board_effect(effect);
                }
                // The wake timer runs in the RTC domain, so it fires while the MCU is halted.
                Owner::Machine(crate::sleep::WAKE_TIMER) => self.on_wake_timer(),
                Owner::Radio(radio) => {
                    self.on_radio_timer(radio, key.tag);
                }
                _ => self.undispatched_events += 1,
            }
        }
    }

    /// The stop a sleep entry left for the run loop, taken.
    pub(crate) fn take_wiring_stop(&mut self) -> Option<StopReason> {
        self.wiring_stop.take()
    }

    /// Applies the cross-block effects peripheral writes left in the SoC, in raise order; returns
    /// how many. Called after every executor exit, and every access that raises one ends the
    /// slice, so at most one access's effects wait.
    pub(crate) fn apply_pending_wiring(&mut self) -> u64 {
        let taken = self.soc.take_wiring();
        let n = taken.len() as u64;
        if n > 0 {
            self.poll.invalidate();
        }
        self.peak_pending_wiring = self.peak_pending_wiring.max(n);
        for w in taken {
            self.apply_wiring(w);
        }
        n
    }

    /// The next instant this run has to stop at: a scheduled event, a journaled input, or the
    /// call's `until` limit. The limit counts because an idle hart with an `until` has somewhere
    /// to idle to, so a sleeping machine paced by `until` calls is not reported as a hang.
    pub(crate) fn next_wake(&self, lim: &RunLimits) -> Option<VTime> {
        [self.sched.next_time(), self.journal.next_time(), lim.until]
            .into_iter()
            .flatten()
            .min()
    }

    pub(crate) fn on_idle(&mut self, now: VTime, next_event: Option<VTime>) -> IdleAction {
        self.idle.on_idle(now, next_event)
    }

    /// Moves virtual time to `t` without retiring an instruction.
    pub(crate) fn idle_until(&mut self, t: VTime) {
        self.poll.idled();
        self.clock.idle_until(self.hart.pos(), t);
    }

    pub(crate) fn clock_ps_per_insn(&self) -> u64 {
        self.clock.ps_per_insn().max(1)
    }

    pub(crate) fn idle_ps(&self) -> u64 {
        self.clock.idle_ps()
    }

    /// Retired instructions, plus instructions credited by fast-forward.
    pub(crate) fn insns(&self) -> u64 {
        self.hart.insns
    }

    pub fn undispatched_events(&self) -> u64 {
        self.undispatched_events
    }

    /// Wiring effects that were produced and not applied.
    pub fn unapplied_wiring(&self) -> u64 {
        self.unapplied_wiring.total()
    }

    pub fn unapplied_wiring_by_kind(&self) -> WiringCounts {
        self.unapplied_wiring
    }

    pub fn hart(&self) -> &Hart {
        &self.hart
    }

    /// The board, read-only.
    pub fn board(&self) -> &PassportBoard {
        &self.board
    }

    /// The canonical MMIO and IRQ trace, read-only. Reading it never changes it.
    pub fn trace(&self) -> &TraceSink {
        &self.trace
    }

    /// Entry `index` of the flash MMU table at 0x600C5000 as the guest last wrote it, read
    /// without an access.
    pub fn mmu_entry(&self, index: u32) -> u32 {
        self.soc.devices.mmu.entry(index)
    }

    /// The fidelity ledger: first touches in the order the run reached them.
    pub fn ledger(&self) -> &FidelityLedger {
        &self.ledger
    }

    pub fn assets(&self) -> &Assets {
        &self.assets
    }

    pub fn config(&self) -> &MachineConfig {
        &self.cfg
    }

    /// The last MMIO read, with its pc and how many times that address was read in a row: a
    /// stall is a repeating register read. An observation only; the loop never branches on it.
    pub fn last_mmio_read(&self) -> Option<MmioRead> {
        self.tap.last_read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0x5C001234 >> 26 is 23 and 0xA << 6 is 640, so the field is 663.
    #[test]
    fn the_adc_calibration_decodes_across_its_two_efuse_words() {
        use pemu_soc_c3::r#gen::regs_efuse;
        let mut regs = pemu_core::regstore::RegStore::new(&regs_efuse::REGS);
        regs.set(efuse_idx::EFUSE_RD_SYS_PART1_DATA4, 0xFFFF_FFFD);
        regs.set(efuse_idx::EFUSE_RD_SYS_PART1_DATA6, 0x5C00_1234);
        regs.set(efuse_idx::EFUSE_RD_SYS_PART1_DATA7, 0xA);
        assert_eq!(
            adc_calibration(&regs),
            AdcCal {
                blk_version_major: 1,
                cal_vol_atten3: 663,
            }
        );
    }
}
