//! Reset causes, scopes and the scope matrix peripherals use. The IDF reset-reason prefix
//! (`soc/reset_reasons.h`) sets the scope: `CHIP_` is [`ResetScope::Chip`], `SYS_` is
//! [`ResetScope::System`] (digital blocks and RTC_CNTL configuration), `CORE_` is
//! [`ResetScope::Core`] (RTC domain kept), and `CPU0_` is `Core` narrowed to the hart and PMS
//! ([`ResetFanout::CpuAndPms`]). SRAM and RTC RAM retention belong to the cause, not the scope.
//! RTC `STORE` words follow the `SYS_` row (`specs/blocks/rtc_cntl.toml`); the device confirms
//! STORE1 at the super-watchdog reset, the other words and `SYS_` causes are UNVERIFIED.

use serde::{Deserialize, Serialize};

use crate::regstore::resets_in;

/// The value `RTC_CNTL_RESET_STATE.RESET_CAUSE_PROCPU[5:0]` reads and the ROM banner prints as
/// `rst:`. The named constants are the rows of [`RESET_CAUSES`].
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResetCause(pub u8);

impl ResetCause {
    /// Emulator start or "unplug".
    pub const POWERON: ResetCause = ResetCause(0x01);
    /// A write of `RTC_CNTL_OPTIONS0.SW_SYS_RST`.
    pub const RTC_SW_SYS: ResetCause = ResetCause(0x03);
    pub const DEEPSLEEP: ResetCause = ResetCause(0x05);
    pub const TG0WDT_SYS: ResetCause = ResetCause(0x07);
    pub const TG1WDT_SYS: ResetCause = ResetCause(0x08);
    pub const RTCWDT_SYS: ResetCause = ResetCause(0x09);
    /// No IDF `soc` name and no emulator trigger.
    pub const INTRUSION: ResetCause = ResetCause(0x0A);
    pub const TG0WDT_CPU: ResetCause = ResetCause(0x0B);
    /// A write of `RTC_CNTL_OPTIONS0.SW_PROCPU_RST`: the `esp_restart` path.
    pub const RTC_SW_CPU: ResetCause = ResetCause(0x0C);
    pub const RTCWDT_CPU: ResetCause = ResetCause(0x0D);
    /// Brownout with `RST_ENA`.
    pub const RTCWDT_BROWN_OUT: ResetCause = ResetCause(0x0F);
    /// RWDT action RESET_RTC, also the esptool RWDT reset.
    pub const RTCWDT_RTC: ResetCause = ResetCause(0x10);
    pub const TG1WDT_CPU: ResetCause = ResetCause(0x11);
    /// Super-watchdog timeout. RTC slow memory is kept and the RTC time counter restarts (probe
    /// `probes/probe_campaign_reset`).
    pub const SUPER_WDT: ResetCause = ResetCause(0x12);
    pub const GLITCH_RTC: ResetCause = ResetCause(0x13);
    pub const EFUSE: ResetCause = ResetCause(0x14);
    /// The USJ CDC line-state reset an esptool or agent reset produces (`rst:0x15` on the device).
    pub const USB_UART_CHIP: ResetCause = ResetCause(0x15);
    pub const USB_JTAG_CHIP: ResetCause = ResetCause(0x16);
    pub const POWER_GLITCH: ResetCause = ResetCause(0x17);

    /// The row of this cause, or `None` for an undocumented code.
    pub fn spec(self) -> Option<&'static ResetSpec> {
        RESET_CAUSES.iter().find(|s| s.cause.0 == self.0)
    }
}

/// How far a reset reaches. A register a `Core` reset restores is also restored by `System` and
/// `Chip`.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ResetScope {
    Chip,
    System,
    Core,
}

impl ResetScope {
    /// Widest first.
    pub const ALL: [ResetScope; 3] = [ResetScope::Chip, ResetScope::System, ResetScope::Core];
}

/// A reset: `scope` says how deeply a block that is reset goes back to its reset values, `fanout`
/// which blocks are reset at all. [`ResetKind::clears`] combines them, so a model never has to.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResetKind {
    pub cause: ResetCause,
    pub scope: ResetScope,
    pub fanout: ResetFanout,
}

impl ResetKind {
    pub fn of(cause: ResetCause) -> Option<ResetKind> {
        cause.spec().map(|s| s.kind)
    }

    /// Whether this reset restores a register of `domain`. Always `false` under
    /// [`ResetFanout::CpuAndPms`], or `esp_restart` would wipe the guest console's UART0 and TIMG
    /// setup; the SENSITIVE model clears its own lock bits.
    pub const fn clears(self, domain: ResetDomain) -> bool {
        self.fanout.reaches_all_blocks() && resets_in(domain, self.scope)
    }

    pub fn spec(self) -> Option<&'static ResetSpec> {
        self.cause.spec()
    }
}

/// Which blocks a reset reaches.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ResetFanout {
    /// Every block of the `c3_devices!` table.
    AllBlocks,
    /// The `CPU0_` classes: the hart, plus the SENSITIVE (PMS) block whose lock bits read 0 after
    /// every reset. Every other block keeps its registers; IDF resets by hand what it needs.
    CpuAndPms,
}

impl ResetFanout {
    pub const fn reaches_all_blocks(self) -> bool {
        matches!(self, ResetFanout::AllBlocks)
    }
}

/// Whether a memory keeps its contents across a reset.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Retention {
    Cleared,
    Kept,
}

/// One documented C3 reset reason: how far it reaches and what it clears.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ResetSpec {
    pub cause: ResetCause,
    /// ROM name, from `esp_rom/include/esp32c3/rom/rtc.h`.
    pub rom_name: &'static str,
    /// IDF `soc/reset_reasons.h` name; its prefix picks the scope.
    pub idf_name: &'static str,
    pub kind: ResetKind,
    /// SRAM: cleared at power-on and on a deep-sleep wake, kept otherwise (`__NOINIT_ATTR` relies
    /// on it). UNVERIFIED for the deep-sleep wake: the sources disagree; a capture would settle it.
    pub sram: Retention,
    /// RTC FAST/SLOW RAM: cleared only at power-on.
    pub rtc_ram: Retention,
    /// Whether an emulator path produces this cause; unmodeled rows let a captured `rst:` decode.
    pub modeled: bool,
}

impl ResetSpec {
    pub const fn fanout(&self) -> ResetFanout {
        self.kind.fanout
    }
}

/// Every documented reset reason, ascending by cause value.
pub static RESET_CAUSES: &[ResetSpec] = &[
    spec(
        ResetCause::POWERON,
        "POWERON_RESET",
        "CHIP_POWER_ON",
        ResetScope::Chip,
        ResetFanout::AllBlocks,
        Retention::Cleared,
        Retention::Cleared,
        true,
    ),
    spec(
        ResetCause::RTC_SW_SYS,
        "RTC_SW_SYS_RESET",
        "CORE_SW",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::DEEPSLEEP,
        "DEEPSLEEP_RESET",
        "CORE_DEEP_SLEEP",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Cleared,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::TG0WDT_SYS,
        "TG0WDT_SYS_RESET",
        "CORE_MWDT0",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::TG1WDT_SYS,
        "TG1WDT_SYS_RESET",
        "CORE_MWDT1",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::RTCWDT_SYS,
        "RTCWDT_SYS_RESET",
        "CORE_RTC_WDT",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::INTRUSION,
        "INTRUSION_RESET",
        "",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        false,
    ),
    spec(
        ResetCause::TG0WDT_CPU,
        "TG0WDT_CPU_RESET",
        "CPU0_MWDT0",
        ResetScope::Core,
        ResetFanout::CpuAndPms,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::RTC_SW_CPU,
        "RTC_SW_CPU_RESET",
        "CPU0_SW",
        ResetScope::Core,
        ResetFanout::CpuAndPms,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::RTCWDT_CPU,
        "RTCWDT_CPU_RESET",
        "CPU0_RTC_WDT",
        ResetScope::Core,
        ResetFanout::CpuAndPms,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::RTCWDT_BROWN_OUT,
        "RTCWDT_BROWN_OUT_RESET",
        "SYS_BROWN_OUT",
        ResetScope::System,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        false,
    ),
    spec(
        ResetCause::RTCWDT_RTC,
        "RTCWDT_RTC_RESET",
        "SYS_RTC_WDT",
        ResetScope::System,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::TG1WDT_CPU,
        "TG1WDT_CPU_RESET",
        "CPU0_MWDT1",
        ResetScope::Core,
        ResetFanout::CpuAndPms,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::SUPER_WDT,
        "SUPER_WDT_RESET",
        "SYS_SUPER_WDT",
        ResetScope::System,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::GLITCH_RTC,
        "GLITCH_RTC_RESET",
        "SYS_CLK_GLITCH",
        ResetScope::System,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        false,
    ),
    spec(
        ResetCause::EFUSE,
        "EFUSE_RESET",
        "CORE_EFUSE_CRC",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        false,
    ),
    spec(
        ResetCause::USB_UART_CHIP,
        "USB_UART_CHIP_RESET",
        "CORE_USB_UART",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        true,
    ),
    spec(
        ResetCause::USB_JTAG_CHIP,
        "USB_JTAG_CHIP_RESET",
        "CORE_USB_JTAG",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        false,
    ),
    spec(
        ResetCause::POWER_GLITCH,
        "POWER_GLITCH_RESET",
        "CORE_PWR_GLITCH",
        ResetScope::Core,
        ResetFanout::AllBlocks,
        Retention::Kept,
        Retention::Kept,
        false,
    ),
];

/// Private, so the table stays the only way to build a [`ResetSpec`].
#[allow(clippy::too_many_arguments)]
const fn spec(
    cause: ResetCause,
    rom_name: &'static str,
    idf_name: &'static str,
    scope: ResetScope,
    fanout: ResetFanout,
    sram: Retention,
    rtc_ram: Retention,
    modeled: bool,
) -> ResetSpec {
    ResetSpec {
        cause,
        rom_name,
        idf_name,
        kind: ResetKind {
            cause,
            scope,
            fanout,
        },
        sram,
        rtc_ram,
        modeled,
    }
}

/// Reset domain of a register (`reset_domains` rows of `specs/blocks/<block>.toml`): a mask of
/// the [`ResetScope`] values that restore it. Empty means none does, as for
/// `RTC_CNTL_RESET_STATE`, which the reset itself writes.
/// UNVERIFIED: the encoding is this crate's own, not a documented chip property.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResetDomain(pub u8);

impl ResetDomain {
    pub const fn contains(self, scope: ResetScope) -> bool {
        resets_in(self, scope)
    }

    /// Whether the mask is a prefix of the `Chip`, `System`, `Core` ladder, as every mask the
    /// block files produce must be.
    pub const fn is_nested(self) -> bool {
        matches!(self.0, 0x0 | 0x1 | 0x3 | 0x7)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Digital blocks (UART, TIMG, SPI, ...), specs/blocks/uart0.toml.
    const DIGITAL: ResetDomain = ResetDomain(0x7);
    /// RTC_CNTL configuration, specs/blocks/rtc_cntl.toml.
    const RTC_CONFIG: ResetDomain = ResetDomain(0x3);
    /// A chip-only reading of the RTC STORE words, which the spec file does not take.
    const RTC_STORE: ResetDomain = ResetDomain(0x1);
    /// `RTC_CNTL_RESET_STATE`: the reset writes the cause, so no scope restores it.
    const NONE: ResetDomain = ResetDomain(0x0);

    fn kind(cause: ResetCause) -> ResetKind {
        ResetKind::of(cause).expect("documented cause")
    }

    #[test]
    fn the_table_lists_every_documented_cause_once_in_order() {
        // 0x02, 0x04, 0x06 and 0x0E are not reset reasons.
        assert_eq!(RESET_CAUSES.len(), 19);
        for pair in RESET_CAUSES.windows(2) {
            assert!(
                pair[0].cause.0 < pair[1].cause.0,
                "{:#04x} then {:#04x}",
                pair[0].cause.0,
                pair[1].cause.0
            );
        }
        assert_eq!(RESET_CAUSES[0].cause, ResetCause::POWERON);
        assert_eq!(
            RESET_CAUSES[RESET_CAUSES.len() - 1].cause,
            ResetCause::POWER_GLITCH
        );
        assert!(ResetCause(0x02).spec().is_none());
        assert!(ResetCause(0x0E).spec().is_none());
        assert!(ResetCause(0x3F).spec().is_none());
    }

    #[test]
    fn the_idf_prefix_decides_the_scope_and_the_fan_out() {
        for s in RESET_CAUSES {
            let (want_scope, want_fanout) = match s.idf_name {
                n if n.starts_with("CHIP_") => (ResetScope::Chip, ResetFanout::AllBlocks),
                n if n.starts_with("SYS_") => (ResetScope::System, ResetFanout::AllBlocks),
                n if n.starts_with("CPU0_") => (ResetScope::Core, ResetFanout::CpuAndPms),
                // CORE_*, and INTRUSION_RESET, which has no IDF name and is not modeled.
                _ => (ResetScope::Core, ResetFanout::AllBlocks),
            };
            assert_eq!(s.kind.scope, want_scope, "{}", s.rom_name);
            assert_eq!(s.fanout(), want_fanout, "{}", s.rom_name);
            assert_eq!(s.kind.fanout, want_fanout, "{}", s.rom_name);
            assert_eq!(s.kind.cause, s.cause, "{}", s.rom_name);
        }
    }

    #[test]
    fn the_scope_matrix_matches_the_documented_domains() {
        for s in RESET_CAUSES {
            let reaches = s.fanout() == ResetFanout::AllBlocks;
            assert_eq!(s.kind.clears(DIGITAL), reaches, "{}", s.rom_name);
            assert!(!s.kind.clears(NONE), "{}", s.rom_name);
        }
        assert!(kind(ResetCause::POWERON).clears(RTC_CONFIG));
        assert!(kind(ResetCause::RTCWDT_RTC).clears(RTC_CONFIG));
        assert!(kind(ResetCause::SUPER_WDT).clears(RTC_CONFIG));
        assert!(!kind(ResetCause::RTC_SW_SYS).clears(RTC_CONFIG));
        assert!(!kind(ResetCause::DEEPSLEEP).clears(RTC_CONFIG));
        assert!(!kind(ResetCause::USB_UART_CHIP).clears(RTC_CONFIG));
        assert!(!kind(ResetCause::RTC_SW_CPU).clears(RTC_CONFIG));
        assert!(kind(ResetCause::POWERON).clears(RTC_STORE));
        for s in RESET_CAUSES {
            if s.cause != ResetCause::POWERON {
                assert!(!s.kind.clears(RTC_STORE), "{}", s.rom_name);
            }
        }
    }

    #[test]
    fn only_a_power_on_reset_clears_the_memories() {
        for s in RESET_CAUSES {
            let want_sram = match s.cause {
                ResetCause::POWERON | ResetCause::DEEPSLEEP => Retention::Cleared,
                _ => Retention::Kept,
            };
            let want_rtc = if s.cause == ResetCause::POWERON {
                Retention::Cleared
            } else {
                Retention::Kept
            };
            assert_eq!(s.sram, want_sram, "{}", s.rom_name);
            assert_eq!(s.rtc_ram, want_rtc, "{}", s.rom_name);
        }
    }

    #[test]
    fn the_device_boot_log_cause_decodes() {
        // After an esptool RTS hard reset the device prints `rst:0x15 (USB_UART_CHIP_RESET)`.
        let s = ResetCause(0x15).spec().expect("0x15 is documented");
        assert_eq!(s.rom_name, "USB_UART_CHIP_RESET");
        assert_eq!(s.idf_name, "CORE_USB_UART");
        assert_eq!(s.kind.scope, ResetScope::Core);
        assert_eq!(s.fanout(), ResetFanout::AllBlocks);
        assert!(s.modeled);
        let restart = ResetCause(0x0C).spec().expect("0x0C is documented");
        assert_eq!(restart.idf_name, "CPU0_SW");
        assert_eq!(restart.fanout(), ResetFanout::CpuAndPms);
    }

    /// A model that trusts `ResetKind` alone must keep the guest console across `esp_restart`.
    #[test]
    fn a_cpu_reset_restores_no_peripheral_register() {
        let cpu = [
            ResetCause::TG0WDT_CPU,
            ResetCause::RTC_SW_CPU,
            ResetCause::RTCWDT_CPU,
            ResetCause::TG1WDT_CPU,
        ];
        for cause in cpu {
            let k = kind(cause);
            assert_eq!(k.scope, ResetScope::Core, "{:#04x}", cause.0);
            assert_eq!(k.fanout, ResetFanout::CpuAndPms, "{:#04x}", cause.0);
            for d in [NONE, RTC_STORE, RTC_CONFIG, DIGITAL] {
                assert!(!k.clears(d), "{:#04x} {d:?}", cause.0);
            }
        }
        // The same scope with the whole-chip fan-out does restore the digital blocks.
        let core = kind(ResetCause::USB_UART_CHIP);
        assert_eq!(core.scope, ResetScope::Core);
        assert_eq!(core.fanout, ResetFanout::AllBlocks);
        assert!(core.clears(DIGITAL));
    }

    #[test]
    fn domain_masks_are_prefixes_of_the_scope_ladder() {
        for d in [NONE, RTC_STORE, RTC_CONFIG, DIGITAL] {
            assert!(d.is_nested(), "{d:?}");
            let mut seen_kept = false;
            for scope in ResetScope::ALL {
                let clears = d.contains(scope);
                assert!(
                    !(clears && seen_kept),
                    "{d:?}: {scope:?} restores what a wider scope keeps"
                );
                seen_kept |= !clears;
            }
        }
        for bad in [0x2, 0x4, 0x5, 0x6] {
            assert!(!ResetDomain(bad).is_nested(), "{bad:#x}");
        }
    }

    /// Asserting `contains` against `resets_in`, which it calls, would hold for any encoding.
    #[test]
    fn the_domain_mask_encodes_one_bit_per_scope() {
        use ResetScope::{Chip, Core, System};
        let seen = |d: ResetDomain| {
            ResetScope::ALL
                .iter()
                .filter(|&&s| d.contains(s))
                .copied()
                .collect::<Vec<_>>()
        };
        assert_eq!(seen(NONE), Vec::<ResetScope>::new());
        assert_eq!(seen(RTC_STORE), vec![Chip]);
        assert_eq!(seen(RTC_CONFIG), vec![Chip, System]);
        assert_eq!(seen(DIGITAL), vec![Chip, System, Core]);
        assert_eq!(seen(ResetDomain(0x2)), vec![System]);
        assert_eq!(seen(ResetDomain(0x4)), vec![Core]);
        for mask in 0..=0x7u8 {
            let d = ResetDomain(mask);
            assert_eq!(d.contains(Chip), mask & 0x1 != 0, "{mask:#x}");
            assert_eq!(d.contains(System), mask & 0x2 != 0, "{mask:#x}");
            assert_eq!(d.contains(Core), mask & 0x4 != 0, "{mask:#x}");
        }
    }
}
