//! Hook set and hook kinds for the functions pinned in `specs/hle/idf-5.5.3/`.
//!
//! A `HookId` encodes the [`HookKind`] and the owning module rather than indexing a table, so the
//! `HookSet`s of several modules merge without renumbering and the dispatcher needs no side table.

use pemu_core::serde::{Deserialize, Serialize};

use crate::magic::MagicKind;
use crate::observe::ObserveKind;
use crate::tripwire::TripKind;

/// Defined in `pemu_rv32::engine` because `Engine::run` takes `&HookSet`. Derived state: never
/// serialized, rebuilt by `bind` on restore.
pub use pemu_rv32::engine::{BitSet, HookId, HookSet};

/// What a hook does when execution reaches its pc.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HookKind {
    /// Replaces a function: esp_wifi_*, VHCI functions.
    Hle(HandlerKind),
    /// Runs, then executes the original instruction: esp_panic_handler, abort, __assert_func,
    /// vTaskDelete.
    Observe(ObserveKind),
    /// Return, WifiWorker, BtWorker, WifiIsr, BtIsr.
    Magic(MagicKind),
    FastForward(FfKind),
    /// Blob internals, r_rwip_*, magic-range fetch.
    Tripwire(TripKind),
    Breakpoint,
}

/// Which module owns a hook: 0 is the core, and each `RadioModule` of `pemu_radio::modules()`
/// takes its own index in list order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleIndex(pub u8);

impl ModuleIndex {
    pub const CORE: ModuleIndex = ModuleIndex(0);
    pub const FIRST_MODULE: ModuleIndex = ModuleIndex(1);
}

const CLASS_HLE: u32 = 0;
const CLASS_OBSERVE: u32 = 1;
const CLASS_MAGIC: u32 = 2;
const CLASS_FAST_FORWARD: u32 = 3;
const CLASS_TRIPWIRE: u32 = 4;
const CLASS_BREAKPOINT: u32 = 5;

/// `HookId` layout: class in bits 31..28, [`ModuleIndex`] in 27..20, payload in 19..0.
const CLASS_SHIFT: u32 = 28;
const MODULE_SHIFT: u32 = 20;
const PAYLOAD_MASK: u32 = (1 << MODULE_SHIFT) - 1;

/// The decoded form of a [`HookId`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HookRef {
    pub kind: HookKind,
    pub module: ModuleIndex,
}

impl HookRef {
    pub fn core(kind: HookKind) -> HookRef {
        HookRef {
            kind,
            module: ModuleIndex::CORE,
        }
    }

    /// The encoding is injective, so [`HookRef::from_id`] recovers exactly this value.
    pub fn to_id(self) -> HookId {
        let (class, payload) = match self.kind {
            HookKind::Hle(HandlerKind(n)) => (CLASS_HLE, u32::from(n)),
            HookKind::Observe(k) => (CLASS_OBSERVE, k as u32),
            HookKind::Magic(k) => (CLASS_MAGIC, k as u32),
            HookKind::FastForward(k) => (CLASS_FAST_FORWARD, k as u32),
            HookKind::Tripwire(k) => (CLASS_TRIPWIRE, k as u32),
            HookKind::Breakpoint => (CLASS_BREAKPOINT, 0),
        };
        HookId((class << CLASS_SHIFT) | (u32::from(self.module.0) << MODULE_SHIFT) | payload)
    }

    /// `None` when `id` was not produced by [`HookRef::to_id`].
    pub fn from_id(id: HookId) -> Option<HookRef> {
        let class = id.0 >> CLASS_SHIFT;
        let module = ModuleIndex(((id.0 >> MODULE_SHIFT) & 0xFF) as u8);
        let payload = id.0 & PAYLOAD_MASK;
        let kind = match class {
            CLASS_HLE => HookKind::Hle(HandlerKind(u16::try_from(payload).ok()?)),
            CLASS_OBSERVE => HookKind::Observe(ObserveKind::from_index(payload)?),
            CLASS_MAGIC => HookKind::Magic(match payload {
                0 => MagicKind::Return,
                1 => MagicKind::WifiWorker,
                2 => MagicKind::BtWorker,
                3 => MagicKind::WifiIsr,
                4 => MagicKind::BtIsr,
                _ => return None,
            }),
            CLASS_FAST_FORWARD => HookKind::FastForward(match payload {
                0 => FfKind::RomDelayLoop,
                _ => return None,
            }),
            CLASS_TRIPWIRE => HookKind::Tripwire(TripKind::from_index(payload)?),
            CLASS_BREAKPOINT if payload == 0 => HookKind::Breakpoint,
            _ => return None,
        };
        Some(HookRef { kind, module })
    }
}

/// Which HLE handler replaces a hooked function. A newtype rather than an enum, so each
/// `RadioModule` numbers its own handlers.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HandlerKind(pub u16);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FfKind {
    /// The ROM delay loop.
    RomDelayLoop,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<HookKind> {
        let mut kinds = vec![
            HookKind::Hle(HandlerKind(0)),
            HookKind::Hle(HandlerKind(u16::MAX)),
            HookKind::FastForward(FfKind::RomDelayLoop),
            HookKind::Breakpoint,
        ];
        kinds.extend(ObserveKind::ALL.map(HookKind::Observe));
        kinds.extend(MagicKind::ALL.map(HookKind::Magic));
        kinds.extend(TripKind::ALL.map(HookKind::Tripwire));
        kinds
    }

    #[test]
    fn a_hook_id_decodes_back_into_its_kind_and_module() {
        for kind in every_kind() {
            for module in [ModuleIndex::CORE, ModuleIndex(1), ModuleIndex(u8::MAX)] {
                let hook = HookRef { kind, module };
                assert_eq!(HookRef::from_id(hook.to_id()), Some(hook), "{hook:?}");
            }
        }
    }

    #[test]
    fn two_modules_never_collide_on_the_same_handler_number() {
        let ble = HookRef {
            kind: HookKind::Hle(HandlerKind(3)),
            module: ModuleIndex(1),
        };
        let wifi = HookRef {
            kind: HookKind::Hle(HandlerKind(3)),
            module: ModuleIndex(2),
        };
        assert_ne!(ble.to_id(), wifi.to_id());
    }

    #[test]
    fn an_id_outside_the_encoding_is_refused() {
        // Class 6 is not a HookKind, and payload 9 is not an ObserveKind.
        assert_eq!(HookRef::from_id(HookId(6 << CLASS_SHIFT)), None);
        assert_eq!(
            HookRef::from_id(HookId((CLASS_OBSERVE << CLASS_SHIFT) | 9)),
            None
        );
        assert_eq!(
            HookRef::from_id(HookId((CLASS_BREAKPOINT << CLASS_SHIFT) | 1)),
            None
        );
    }

    #[test]
    fn binding_a_hook_marks_its_page_and_bumps_the_generation() {
        let mut set = HookSet::default();
        let hook = HookRef::core(HookKind::Observe(ObserveKind::TaskDelete));
        let before = set.generation();
        assert_eq!(set.insert(0x4038_0100, hook.to_id()), None);
        assert!(set.generation() != before);
        assert!(set.page_has_hooks(0x4038_0100));
        assert_eq!(set.get(0x4038_0100).and_then(HookRef::from_id), Some(hook));
    }
}
