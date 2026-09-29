//! The heap ledger: the guest-heap blocks a replaced radio blob would have held.
//!
//! Under HLE the blob's `heap_caps_malloc` calls vanish, so the guest would see a heap several
//! kilobytes larger than the device's and a firmware that barely fits on silicon would pass here.
//! The ledger takes that memory back with real nested `heap_caps_malloc` calls into the image's
//! allocator, so the blocks fragment the heap and show in `inspect heap` and
//! `esp_get_free_heap_size`.
//!
//! - Element rows: a capacity the controller reports, or a count one sdkconfig fixes, times the
//!   octets the Bluetooth Core specification gives one element. A lower bound.
//! - Measured rows ([`CountClass::Measured`]): the device's
//!   `heap_caps_get_free_size(MALLOC_CAP_INTERNAL)` delta around a controller call, less what the
//!   emulator's run already takes at that step. One block, contents unknown.
//!
//! Nothing is fitted. The split between blocks is the emulator's, hence [`HEAP_FIDELITY`]. A
//! refused block fails that init with `ESP_ERR_NO_MEM`, as silicon does.

use pemu_core::snap::snap_struct;
use pemu_hle::binding::HeapBlock;

/// The receipt's fidelity class for a radio heap that comes from a ledger.
pub const HEAP_FIDELITY: &str = "blob_allocations_estimated";

pub const MALLOC_CAP_8BIT: u32 = 1 << 2;
pub const MALLOC_CAP_INTERNAL: u32 = 1 << 11;

/// Radio buffers are touched from an interrupt and by the radio hardware, so they must be
/// internal, byte-addressable RAM; the ESP32-C3 has no external RAM, so one constant serves.
pub const LEDGER_CAPS: u32 = MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT;

/// Where a row's `count` comes from, the only part of a row whose fidelity varies.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CountClass {
    /// A capacity the device reported in an HCI capture (class A).
    Capture,
    /// A count read from the build's `sdkconfig` (class C).
    Sdkconfig,
    /// A free-heap difference the device printed, less what the emulator's run takes at the same
    /// step (class B).
    Measured,
}

impl CountClass {
    pub const fn class(self) -> &'static str {
        match self {
            CountClass::Capture => "A",
            CountClass::Measured => "B",
            CountClass::Sdkconfig => "C",
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Lifetime {
    /// Allocated at the end of a successful init, freed at deinit.
    #[default]
    Init,
    /// Allocated by a successful enable, freed by disable.
    Enable,
    /// Allocated by the first successful init of a boot and never freed: the device's free heap
    /// after deinit stays below its value before init.
    Boot,
}

/// One planned block. The whole row is spec data; nothing is computed from a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerRow {
    /// The label `inspect heap` prints after the module name.
    pub label: String,
    pub count: u32,
    /// Octets one element carries, from the layout named in `element_source`.
    pub element: u32,
    pub count_class: CountClass,
    /// The capacity or Kconfig symbol `count` was read from.
    pub count_source: String,
    pub element_source: String,
    /// Set when `count` is one build's sdkconfig value: the row is allocated only for an image of
    /// that shape.
    pub config: Option<String>,
    pub lifetime: Lifetime,
}

impl LedgerRow {
    /// `count * element`, saturating.
    pub fn bytes(&self) -> u32 {
        self.count.saturating_mul(self.element)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerBlock {
    /// An index rather than the label keeps a snapshot small and free of text.
    pub row: u16,
    pub addr: u32,
    pub bytes: u32,
}

snap_struct!(LedgerBlock { row, addr, bytes });

/// A module's live ledger. It rides in the `hle.machine` section and is part of `state_hash`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// In allocation order.
    pub blocks: Vec<LedgerBlock>,
    /// Refusals since the machine started. A refusal fails its init, so this is nonzero only when
    /// the controller never came up.
    pub refused: u32,
}

snap_struct!(Ledger { blocks, refused });

impl Ledger {
    pub fn held(&self) -> u64 {
        self.blocks.iter().map(|b| u64::from(b.bytes)).sum()
    }

    pub fn record(&mut self, row: u16, addr: u32, bytes: u32) {
        self.blocks.push(LedgerBlock { row, addr, bytes });
    }

    pub fn holds(&self, row: u16) -> bool {
        self.blocks.iter().any(|b| b.row == row)
    }

    /// Removes and returns the newest block whose row `plan` gives `lifetime`. A block whose row is
    /// not in `plan` is never taken: its lifetime is unknown, so it stays held.
    pub fn take_newest(&mut self, plan: &[&LedgerRow], lifetime: Lifetime) -> Option<LedgerBlock> {
        let at = self.blocks.iter().rposition(|b| {
            plan.get(usize::from(b.row))
                .is_some_and(|r| r.lifetime == lifetime)
        })?;
        Some(self.blocks.remove(at))
    }

    /// The blocks as the receipt and `inspect heap` report them. A block whose row is not in `plan`
    /// (restored from a build with another plan) is reported with an empty label and class `U`,
    /// not dropped.
    pub fn report(&self, module: &'static str, plan: &[&LedgerRow]) -> Vec<HeapBlock> {
        self.blocks
            .iter()
            .map(|b| {
                let row = plan.get(usize::from(b.row));
                HeapBlock {
                    module: module.to_string(),
                    label: row.map(|r| r.label.clone()).unwrap_or_default(),
                    addr: b.addr,
                    bytes: b.bytes,
                    caps: LEDGER_CAPS,
                    fidelity: HEAP_FIDELITY.to_string(),
                    class: row
                        .map(|r| r.count_class.class())
                        .unwrap_or("U")
                        .to_string(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::snap::{SnapReader, SnapValue};

    fn row(label: &str, count: u32, element: u32) -> LedgerRow {
        LedgerRow {
            label: label.to_string(),
            count,
            element,
            count_class: CountClass::Capture,
            count_source: "test".to_string(),
            element_source: "test".to_string(),
            config: None,
            lifetime: Lifetime::Init,
        }
    }

    #[test]
    fn a_lifetime_takes_only_its_own_blocks_newest_first() {
        let init = row("init", 1, 16);
        let mut enable = row("enable", 1, 32);
        enable.lifetime = Lifetime::Enable;
        let mut boot = row("boot", 1, 8);
        boot.lifetime = Lifetime::Boot;
        let plan = vec![&init, &enable, &boot];
        let mut ledger = Ledger::default();
        ledger.record(0, 0x100, 16);
        ledger.record(2, 0x200, 8);
        ledger.record(0, 0x300, 16);
        ledger.record(1, 0x400, 32);
        ledger.record(7, 0x500, 4);
        assert_eq!(
            ledger.take_newest(&plan, Lifetime::Init).map(|b| b.addr),
            Some(0x300)
        );
        assert_eq!(
            ledger.take_newest(&plan, Lifetime::Init).map(|b| b.addr),
            Some(0x100)
        );
        assert_eq!(ledger.take_newest(&plan, Lifetime::Init), None);
        assert_eq!(
            ledger.take_newest(&plan, Lifetime::Enable).map(|b| b.addr),
            Some(0x400)
        );
        assert_eq!(ledger.blocks.len(), 2);
        assert!(ledger.holds(2) && ledger.holds(7) && !ledger.holds(0));
        assert_eq!(CountClass::Measured.class(), "B");
    }

    #[test]
    fn a_row_is_its_count_times_its_element_and_nothing_else() {
        assert_eq!(row("acl", 12, 255).bytes(), 3_060);
        assert_eq!(row("none", 0, 255).bytes(), 0);
        assert_eq!(row("huge", u32::MAX, 2).bytes(), u32::MAX);
    }

    #[test]
    fn a_ledger_round_trips_through_a_snapshot() {
        let mut ledger = Ledger::default();
        ledger.record(0, 0x3fc9_0000, 3_060);
        ledger.record(1, 0x3fc9_1000, 372);
        ledger.refused = 2;
        let mut bytes = Vec::new();
        ledger.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "hle.machine");
        assert_eq!(Ledger::snap_read(&mut r).expect("reads back"), ledger);
        assert_eq!(ledger.held(), 3_432);
    }

    #[test]
    fn a_block_of_a_row_this_build_has_no_plan_for_is_reported_unlabelled() {
        let mut ledger = Ledger::default();
        ledger.record(9, 0x3fc9_0000, 16);
        let acl = row("acl", 12, 255);
        let plan = vec![&acl];
        let report = ledger.report("ble", &plan);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].label, "");
        assert_eq!(report[0].class, "U");
        assert_eq!(report[0].fidelity, HEAP_FIDELITY);
    }

    #[test]
    fn a_reported_block_carries_the_class_of_its_count_and_the_ledger_fidelity() {
        let mut ledger = Ledger::default();
        ledger.record(0, 0x3fc9_0000, 3_060);
        let mut acl = row("acl_rx", 12, 255);
        acl.count_class = CountClass::Sdkconfig;
        let plan = vec![&acl];
        let report = ledger.report("ble", &plan);
        assert_eq!(report[0].module, "ble");
        assert_eq!(report[0].label, "acl_rx");
        assert_eq!(report[0].class, "C");
        assert_eq!(report[0].caps, LEDGER_CAPS);
        assert_eq!(report[0].fidelity, HEAP_FIDELITY);
    }
}
