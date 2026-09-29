//! The `Peripheral` trait, its access context `Cx`, the cross-block `Wiring` effects and the
//! single `c3_devices!` table (one `specs/blocks/<block>.toml` per row). Each block file picks its
//! own model through a type alias, so a block owner changes only its own file.

pub mod aes;
pub mod apb_ctrl;
pub mod assist_debug;
pub mod efuse;
pub mod extmem;
pub mod flash_xmc;
pub mod gdma;
pub mod gpio;
pub mod i2c0;
pub mod i2s0;
pub mod iomux;
pub mod ledc;
pub mod radio_store;
pub mod reg_file;
pub mod regi2c;
pub mod rsa;
pub mod rtc_cntl;
pub mod rtc_sleep;
pub mod saradc;
pub mod sensitive;
pub mod sha;
pub mod spi2;
pub mod spi_mem;
pub mod store_only;
pub mod system;
pub mod systimer;
pub mod timg;
pub mod uart0;
pub mod uart1;
pub mod usj;

use pemu_core::clock::TimingProfile;
use pemu_core::fidelity::{Fidelity, FidelityLedger};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::rng::RngView;
use pemu_core::sched::{PeriphId, Scheduler};
use pemu_core::serde::Serialize;
use pemu_core::serde::de::DeserializeOwned;
use pemu_core::time::VTime;
use pemu_core::trace::TraceSink;
use pemu_rv32::spmon::SpMonitor;

use crate::dma::DmaView;
use crate::intc::IrqFabric;
use i2s0::Dir;
use rtc_sleep::SleepKind;
use store_only::StoreOnly;

/// One memory-mapped block. A peripheral depends only on `pemu-core` and its `Cx`; cross-block
/// effects go through `Wiring`.
pub trait Peripheral: Serialize + DeserializeOwned {
    /// Identifier of the block, from its `c3_devices!` row.
    const ID: PeriphId;
    /// Base address of the register window.
    const BASE: u32;
    /// Size of the register window in bytes.
    const SIZE: u32;

    /// Resets the block for `kind`.
    fn reset(&mut self, kind: ResetKind, cx: &mut Cx);

    /// Reads `size` bytes at `off`.
    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead;

    /// Writes `size` bytes of `val` at `off`.
    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite;

    /// A scheduled event of this block fired.
    fn on_event(&mut self, _tag: u16, _cx: &mut Cx) -> Wiring {
        Wiring::None
    }

    /// How long a read of `off` keeps returning the same value if nothing else touches the block.
    fn stable_until(&self, _off: u32, _cx: &Cx) -> Stability {
        Stability::Never
    }

    /// Fidelity class of the register at `off`.
    fn fidelity(&self, off: u32) -> Fidelity;
}

/// Answer of `Peripheral::stable_until`.
pub enum Stability {
    /// The value may change at any time.
    Never,
    /// The value holds until the next scheduled event.
    UntilNextEvent,
    /// The value holds until the given time.
    Until(VTime),
    /// Changes only through an InputEvent (GPIO levels, ADC ladder).
    UntilInput,
}

/// Access context handed to every peripheral call.
pub struct Cx<'a> {
    /// Exact virtual time of the access.
    pub now: VTime,
    /// Event scheduler.
    pub sched: &'a mut Scheduler,
    /// Interrupt fabric.
    pub irq: &'a mut IrqFabric,
    /// GDMA DMA view.
    pub dma: &'a mut DmaView<'a>,
    /// This machine's deterministic random stream.
    pub rng: RngView<'a>,
    /// Timing profile.
    pub profile: &'a TimingProfile,
    /// Fidelity ledger; first touches go here.
    pub ledger: &'a mut FidelityLedger,
    /// Compiled in; branch-predicted off.
    pub trace: &'a mut TraceSink,
}

/// Cross-block and cross-crate effects, applied by `wiring/` after the write or event returns.
pub enum Wiring {
    None,
    /// `Clock::rebase` from SYSCLK_CONF / CPU_PER_CONF.
    ClockChanged,
    /// Page table plus engine invalidation for one MMU entry.
    MmuEntry(u8),
    /// Page table plus engine invalidation after a cache control change.
    CacheCtrl,
    /// A flash page was written.
    FlashWritten {
        /// Physical flash page.
        phys_page: u32,
    },
    /// PMP CSRs or SENSITIVE registers.
    ProtectionChanged,
    /// Collect GDMA TX, sample DC (GPIO_OUT bit 20), call `BoardPorts::spi2`.
    Spi2Transfer,
    /// Execute the I2C0 command list against `BoardPorts::i2c`.
    I2cRun,
    /// One I2S period in direction `Dir`.
    I2sPeriod(Dir),
    /// Compress SHA `BLOCK_NUM` blocks from the GDMA TX channel bound to SHA.
    ShaDma,
    /// Transform AES `BLOCK_NUM` blocks from the GDMA TX channel bound to AES into its RX channel.
    AesDma,
    /// One ADC sample.
    AdcSample {
        /// ADC unit.
        unit: u8,
        /// ADC channel.
        channel: u8,
    },
    /// GPIO outputs changed.
    GpioChanged,
    /// LEDC configuration changed.
    LedcChanged,
    /// HostIo ring pump.
    UsjIo,
    /// ASSIST_DEBUG bounds or enable changed; the write returns OkStop.
    SpMonitor(SpMonitor),
    /// Chip reset.
    ChipReset(ResetKind),
    /// Sleep entry.
    SleepEnter(SleepKind),
}

/// Result of `Peripheral::read`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct RegRead {
    pub val: u32,
    /// Finish this instruction, then leave the block (OkStop).
    pub stop: bool,
}

/// Result of `Peripheral::write` and `IrqFabric::write`.
pub struct RegWrite {
    /// Finish this instruction, then leave the block (OkStop).
    pub stop: bool,
    /// Cross-block effect handled after the write returns.
    pub wiring: Wiring,
}

/// Identity of one `c3_devices!` row, implemented by the generated marker types in `block`, so a
/// generic model such as `StoreOnly<B>` takes its constants from the table.
pub trait Block {
    const NAME: &'static str;
    const ID: PeriphId;
    const BASE: u32;
    const SIZE: u32;
}

/// One row of the `c3_devices!` table as data.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BlockInfo {
    pub name: &'static str,
    pub id: PeriphId,
    pub base: u32,
    pub size: u32,
}

/// Visits block models generically; the reset fan-out and the snapshot sections are visitors.
pub trait DeviceVisitor {
    fn visit<P: Peripheral>(&mut self, dev: &mut P);
}

/// First MMIO address covered by `MMIO_MAP`.
pub const MMIO_BASE: u32 = 0x6000_0000;

/// Number of 4 KB pages in `MMIO_MAP`.
pub const MMIO_PAGES: usize = 0xD1;

/// Generates, from one table: the `PeriphId` constants (`id`), the marker types (`block`), the
/// row data (`BLOCKS`), the model holder with its visitors (`Devices`) and a compile-time check
/// that every model's `Peripheral` constants match its row.
macro_rules! c3_devices {
    ($( $field:ident: $marker:ident, $konst:ident @ $base:expr, $size:expr => $model:ty; )*) => {
        #[repr(u16)]
        enum Row {
            $( $marker, )*
        }

        /// `PeriphId` of every `c3_devices!` row, numbered in table order.
        pub mod id {
            use pemu_core::sched::PeriphId;

            $(
                #[doc = concat!("Block `", stringify!($field), "`.")]
                pub const $konst: PeriphId = PeriphId(super::Row::$marker as u16);
            )*

            /// Entry of `MMIO_MAP` pages that no block claims.
            pub const UNMAPPED: PeriphId = PeriphId(u16::MAX);
        }

        /// Marker type per `c3_devices!` row, carrying the row through `Block`.
        pub mod block {
            use pemu_core::sched::PeriphId;

            $(
                #[doc = concat!("Row `", stringify!($field), "` of the `c3_devices!` table.")]
                pub struct $marker;

                impl super::Block for $marker {
                    const NAME: &'static str = stringify!($field);
                    const ID: PeriphId = super::id::$konst;
                    const BASE: u32 = $base;
                    const SIZE: u32 = $size;
                }
            )*
        }

        /// Number of rows in the `c3_devices!` table.
        pub const BLOCK_COUNT: usize = [$( stringify!($field) ),*].len();

        /// The `c3_devices!` rows in table order; `BLOCKS[i].id == PeriphId(i)`.
        pub const BLOCKS: [BlockInfo; BLOCK_COUNT] = [
            $( BlockInfo { name: stringify!($field), id: id::$konst, base: $base, size: $size }, )*
        ];

        $(
            const _: () = assert!(
                <$model as Peripheral>::ID.0 == id::$konst.0
                    && <$model as Peripheral>::BASE == $base
                    && <$model as Peripheral>::SIZE == $size
            );
        )*

        /// One model instance per `c3_devices!` row, each built by `Default`.
        pub struct Devices {
            $(
                #[doc = concat!("Model of block `", stringify!($field), "`.")]
                pub $field: $model,
            )*
        }

        impl Default for Devices {
            fn default() -> Self {
                Devices { $( $field: <$model as Default>::default(), )* }
            }
        }

        impl Devices {
            /// Visits the model of block `periph`; returns false when no row has that id.
            pub fn visit<V: DeviceVisitor>(&mut self, periph: PeriphId, v: &mut V) -> bool {
                match periph {
                    $( id::$konst => { v.visit(&mut self.$field); true } )*
                    _ => false,
                }
            }

            /// Visits every model in table order.
            pub fn visit_all<V: DeviceVisitor>(&mut self, v: &mut V) {
                $( v.visit(&mut self.$field); )*
            }
        }
    };
}

// The single table, sorted by base. Bases are checked against IDF
// `soc/esp32c3/register/soc/reg_base.h`; sizes are 0x1000 except the RTC_CNTL / eFuse split of
// page 0x60008 and NRX at 0x6001CC00 (UNVERIFIED). Not listed: the sigma-delta registers inside
// the GPIO window, the sleep part of `rtc_cntl` (`rtc_sleep.rs`) and the flash chip behind SPI1
// (`flash_xmc.rs`). Unmodeled rows name `StoreOnly` directly.
c3_devices! {
    uart0: Uart0, UART0 @ 0x6000_0000, 0x1000 => uart0::Model;
    spi1: Spi1, SPI1 @ 0x6000_2000, 0x1000 => spi_mem::Spi1Model;
    spi0: Spi0, SPI0 @ 0x6000_3000, 0x1000 => spi_mem::Spi0Model;
    gpio: Gpio, GPIO @ 0x6000_4000, 0x1000 => gpio::Model;
    radio_fe2: RadioFe2, RADIO_FE2 @ 0x6000_5000, 0x1000 => radio_store::Fe2Model;
    radio_fe: RadioFe, RADIO_FE @ 0x6000_6000, 0x1000 => radio_store::FeModel;
    rtc_cntl: RtcCntl, RTC_CNTL @ 0x6000_8000, 0x800 => rtc_cntl::Model;
    efuse: Efuse, EFUSE @ 0x6000_8800, 0x800 => efuse::Model;
    iomux: Iomux, IOMUX @ 0x6000_9000, 0x1000 => iomux::Model;
    regi2c: Regi2c, REGI2C @ 0x6000_E000, 0x1000 => regi2c::Model;
    uart1: Uart1, UART1 @ 0x6001_0000, 0x1000 => uart1::Model;
    i2c0: I2c0, I2C0 @ 0x6001_3000, 0x1000 => i2c0::Model;
    uhci0: Uhci0, UHCI0 @ 0x6001_4000, 0x1000 => StoreOnly<block::Uhci0>;
    rmt: Rmt, RMT @ 0x6001_6000, 0x1000 => StoreOnly<block::Rmt>;
    ledc: Ledc, LEDC @ 0x6001_9000, 0x1000 => ledc::Model;
    radio_nrx: RadioNrx, RADIO_NRX @ 0x6001_CC00, 0x400 => radio_store::NrxModel;
    radio_bb: RadioBb, RADIO_BB @ 0x6001_D000, 0x1000 => radio_store::BbModel;
    timg0: Timg0, TIMG0 @ 0x6001_F000, 0x1000 => timg::Timg0Model;
    timg1: Timg1, TIMG1 @ 0x6002_0000, 0x1000 => timg::Timg1Model;
    systimer: Systimer, SYSTIMER @ 0x6002_3000, 0x1000 => systimer::Model;
    spi2: Spi2, SPI2 @ 0x6002_4000, 0x1000 => spi2::Model;
    apb_ctrl: ApbCtrl, APB_CTRL @ 0x6002_6000, 0x1000 => apb_ctrl::Model;
    twai: Twai, TWAI @ 0x6002_B000, 0x1000 => StoreOnly<block::Twai>;
    i2s0: I2s0, I2S0 @ 0x6002_D000, 0x1000 => i2s0::Model;
    radio_ble: RadioBle, RADIO_BLE @ 0x6003_1000, 0x1000 => radio_store::BleModel;
    aes: Aes, AES @ 0x6003_A000, 0x1000 => aes::Model;
    sha: Sha, SHA @ 0x6003_B000, 0x1000 => sha::Model;
    rsa: Rsa, RSA @ 0x6003_C000, 0x1000 => rsa::Model;
    ds: Ds, DS @ 0x6003_D000, 0x1000 => StoreOnly<block::Ds>;
    hmac: Hmac, HMAC @ 0x6003_E000, 0x1000 => StoreOnly<block::Hmac>;
    gdma: Gdma, GDMA @ 0x6003_F000, 0x1000 => gdma::Model;
    saradc: Saradc, SARADC @ 0x6004_0000, 0x1000 => saradc::Model;
    usj: Usj, USJ @ 0x6004_3000, 0x1000 => usj::Model;
    system: System, SYSTEM @ 0x600C_0000, 0x1000 => system::Model;
    sensitive: Sensitive, SENSITIVE @ 0x600C_1000, 0x1000 => sensitive::Model;
    intc: Intc, INTC @ 0x600C_2000, 0x1000 => crate::intc::Model;
    extmem: Extmem, EXTMEM @ 0x600C_4000, 0x1000 => extmem::Model;
    mmu: Mmu, MMU @ 0x600C_5000, 0x1000 => crate::mmu::Model;
    xts_aes: XtsAes, XTS_AES @ 0x600C_C000, 0x1000 => StoreOnly<block::XtsAes>;
    assist_debug: AssistDebug, ASSIST_DEBUG @ 0x600C_E000, 0x1000 => assist_debug::Model;
    dedicated_gpio: DedicatedGpio, DEDICATED_GPIO @ 0x600C_F000, 0x1000 => StoreOnly<block::DedicatedGpio>;
    world_cntl: WorldCntl, WORLD_CNTL @ 0x600D_0000, 0x1000 => StoreOnly<block::WorldCntl>;
}

impl Devices {
    /// The fidelity class the model of `periph` claims for the register at `off`, or `None` when
    /// no row has that id. The receipt needs it because `FidelityLedger::class_of` knows only
    /// classes a run noted. `&mut self` only because [`Devices::visit`] hands out `&mut P`.
    pub fn fidelity_of(&mut self, periph: PeriphId, off: u32) -> Option<Fidelity> {
        struct ClassOf {
            off: u32,
            class: Fidelity,
        }
        impl DeviceVisitor for ClassOf {
            fn visit<P: Peripheral>(&mut self, dev: &mut P) {
                self.class = dev.fidelity(self.off);
            }
        }
        let mut v = ClassOf {
            off,
            class: Fidelity::U,
        };
        self.visit(periph, &mut v).then_some(v.class)
    }
}

/// `(addr - 0x6000_0000) >> 12` to block. A page shared by two blocks (the RTC_CNTL / eFuse split)
/// holds the first; `lookup` resolves the second.
pub const MMIO_MAP: [PeriphId; MMIO_PAGES] = build_mmio_map();

const fn build_mmio_map() -> [PeriphId; MMIO_PAGES] {
    let mut map = [id::UNMAPPED; MMIO_PAGES];
    let mut i = 0;
    while i < BLOCK_COUNT {
        let b = BLOCKS[i];
        let mut page = ((b.base - MMIO_BASE) >> 12) as usize;
        let end = ((b.base - MMIO_BASE + b.size).div_ceil(0x1000)) as usize;
        while page < end {
            if map[page].0 == id::UNMAPPED.0 {
                map[page] = b.id;
            }
            page += 1;
        }
        i += 1;
    }
    map
}

/// Address decode through `MMIO_MAP`, including the RTC_CNTL / eFuse split: the block containing
/// `addr` and the offset inside it, or `None`.
pub fn lookup(addr: u32) -> Option<(PeriphId, u32)> {
    let page = (addr.checked_sub(MMIO_BASE)? >> 12) as usize;
    let first = *MMIO_MAP.get(page)?;
    if first == id::UNMAPPED {
        return None;
    }
    let covers = |b: &BlockInfo| addr >= b.base && addr - b.base < b.size;
    let b = &BLOCKS[first.0 as usize];
    if covers(b) {
        return Some((b.id, addr - b.base));
    }
    BLOCKS
        .iter()
        .find(|b| covers(b))
        .map(|b| (b.id, addr - b.base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_sorted_disjoint_numbered_and_inside_the_map() {
        assert_eq!(BLOCKS.len(), BLOCK_COUNT);
        for (i, b) in BLOCKS.iter().enumerate() {
            assert_eq!(b.id, PeriphId(i as u16), "{}", b.name);
            assert!(b.size > 0 && b.size % 4 == 0, "{}", b.name);
            assert!(b.base >= MMIO_BASE, "{}", b.name);
            assert!(
                (((b.base - MMIO_BASE) + b.size - 1) >> 12) < MMIO_PAGES as u32,
                "{}",
                b.name
            );
            if i > 0 {
                let p = &BLOCKS[i - 1];
                assert!(p.base + p.size <= b.base, "{} overlaps {}", p.name, b.name);
            }
        }
    }

    #[test]
    fn bases_follow_the_memory_map() {
        let expected = [
            (id::EFUSE, 0x6000_8800),
            (id::RTC_CNTL, 0x6000_8000),
            (id::REGI2C, 0x6000_E000),
            (id::SYSTEM, 0x600C_0000),
            (id::APB_CTRL, 0x6002_6000),
            (id::UART0, 0x6000_0000),
            (id::USJ, 0x6004_3000),
            (id::GPIO, 0x6000_4000),
            (id::IOMUX, 0x6000_9000),
            (id::SPI0, 0x6000_3000),
            (id::SPI1, 0x6000_2000),
            (id::EXTMEM, 0x600C_4000),
            (id::MMU, 0x600C_5000),
            (id::TIMG0, 0x6001_F000),
            (id::TIMG1, 0x6002_0000),
            (id::SHA, 0x6003_B000),
            (id::INTC, 0x600C_2000),
            (id::SYSTIMER, 0x6002_3000),
            (id::SENSITIVE, 0x600C_1000),
            (id::ASSIST_DEBUG, 0x600C_E000),
            (id::SPI2, 0x6002_4000),
            (id::GDMA, 0x6003_F000),
            (id::LEDC, 0x6001_9000),
            (id::I2C0, 0x6001_3000),
            (id::SARADC, 0x6004_0000),
            (id::I2S0, 0x6002_D000),
            (id::RSA, 0x6003_C000),
            (id::AES, 0x6003_A000),
            (id::RADIO_BLE, 0x6003_1000),
        ];
        for (periph, base) in expected {
            assert_eq!(BLOCKS[periph.0 as usize].base, base, "{periph:?}");
        }
        assert_eq!(<block::Efuse as Block>::NAME, "efuse");
        assert_eq!(<block::Efuse as Block>::ID, id::EFUSE);
        assert_eq!(<block::Efuse as Block>::BASE, 0x6000_8800);
        assert_eq!(<block::Efuse as Block>::SIZE, 0x800);
    }

    #[test]
    fn mmio_map_holds_the_first_block_of_each_page() {
        assert_eq!(MMIO_MAP[0x00], id::UART0);
        assert_eq!(MMIO_MAP[0x01], id::UNMAPPED);
        assert_eq!(MMIO_MAP[0x08], id::RTC_CNTL);
        assert_eq!(MMIO_MAP[0x1C], id::RADIO_NRX);
        assert_eq!(MMIO_MAP[0xC5], id::MMU);
        assert_eq!(MMIO_MAP[0xD0], id::WORLD_CNTL);
        let claimed = MMIO_MAP.iter().filter(|p| **p != id::UNMAPPED).count();
        assert_eq!(
            claimed,
            BLOCK_COUNT - 1,
            "only the split slot shares a page"
        );
    }

    #[test]
    fn lookup_resolves_split_slot_offsets_and_gaps() {
        assert_eq!(lookup(0x6000_0000), Some((id::UART0, 0)));
        assert_eq!(lookup(0x6000_8004), Some((id::RTC_CNTL, 4)));
        assert_eq!(lookup(0x6000_87FC), Some((id::RTC_CNTL, 0x7FC)));
        assert_eq!(lookup(0x6000_8800), Some((id::EFUSE, 0)));
        assert_eq!(lookup(0x6000_8FFF), Some((id::EFUSE, 0x7FF)));
        assert_eq!(lookup(0x600C_E098), Some((id::ASSIST_DEBUG, 0x98)));
        assert_eq!(lookup(0x6001_CCD4), Some((id::RADIO_NRX, 0xD4)));
        assert_eq!(lookup(0x6001_C000), None, "below NRX inside its page");
        assert_eq!(lookup(0x6000_1000), None);
        assert_eq!(lookup(0x5FFF_FFFC), None);
        assert_eq!(lookup(0x600D_1000), None);
        assert_eq!(lookup(u32::MAX), None);
    }

    #[derive(Default)]
    struct Collect(Vec<(PeriphId, u32, u32, Fidelity)>);

    impl DeviceVisitor for Collect {
        fn visit<P: Peripheral>(&mut self, dev: &mut P) {
            self.0.push((P::ID, P::BASE, P::SIZE, dev.fidelity(0)));
        }
    }

    /// What each row answers for `fidelity(0)`, in table order. `StoreOnly` answers U everywhere,
    /// so a blanket U would pass by accident; a row that silently changes model fails here.
    const FIDELITY_AT_0: [(&str, Fidelity); BLOCK_COUNT] = [
        ("uart0", Fidelity::B),
        ("spi1", Fidelity::B),
        ("spi0", Fidelity::U),
        ("gpio", Fidelity::U),
        ("radio_fe2", Fidelity::U),
        ("radio_fe", Fidelity::U),
        ("rtc_cntl", Fidelity::B),
        ("efuse", Fidelity::U),
        ("iomux", Fidelity::U),
        ("regi2c", Fidelity::B),
        ("uart1", Fidelity::U),
        ("i2c0", Fidelity::C),
        ("uhci0", Fidelity::U),
        ("rmt", Fidelity::U),
        ("ledc", Fidelity::B),
        ("radio_nrx", Fidelity::U),
        ("radio_bb", Fidelity::U),
        ("timg0", Fidelity::B),
        ("timg1", Fidelity::B),
        ("systimer", Fidelity::B),
        ("spi2", Fidelity::B),
        ("apb_ctrl", Fidelity::U),
        ("twai", Fidelity::U),
        ("i2s0", Fidelity::U),
        ("radio_ble", Fidelity::U),
        ("aes", Fidelity::A),
        ("sha", Fidelity::A),
        ("rsa", Fidelity::A),
        ("ds", Fidelity::U),
        ("hmac", Fidelity::U),
        ("gdma", Fidelity::B),
        ("saradc", Fidelity::C),
        ("usj", Fidelity::A),
        ("system", Fidelity::C),
        ("sensitive", Fidelity::B),
        ("intc", Fidelity::B),
        ("extmem", Fidelity::B),
        ("mmu", Fidelity::B),
        ("xts_aes", Fidelity::U),
        ("assist_debug", Fidelity::B),
        ("dedicated_gpio", Fidelity::U),
        ("world_cntl", Fidelity::U),
    ];

    #[test]
    fn devices_hold_one_model_per_row() {
        let mut devices = Devices::default();
        let mut all = Collect::default();
        devices.visit_all(&mut all);
        let rows: Vec<_> = BLOCKS
            .iter()
            .zip(FIDELITY_AT_0)
            .map(|(b, (name, class))| {
                assert_eq!(b.name, name, "FIDELITY_AT_0 is not in `c3_devices!` order");
                (b.id, b.base, b.size, class)
            })
            .collect();
        assert_eq!(all.0, rows);

        let mut one = Collect::default();
        assert!(devices.visit(id::EFUSE, &mut one));
        assert_eq!(one.0, vec![(id::EFUSE, 0x6000_8800, 0x800, Fidelity::U)]);
        assert!(!devices.visit(id::UNMAPPED, &mut one));
        assert_eq!(one.0.len(), 1);
    }
}
