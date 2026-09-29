//! Observe hooks: they run, then the original instruction executes (`Engine::run_hooked_once`).
//!
//! The kinds are the IDF panic path (`esp_panic_handler`, `abort`, `__assert_func`) plus
//! `vTaskDelete` (FreeRTOS `tasks.c`), which drops the deleted task's continuations.

use pemu_core::serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObserveKind {
    PanicHandler,
    Abort,
    AssertFunc,
    /// Drops the deleted task's continuation and bumps its `HleSection::task_generations` entry.
    TaskDelete,
}

impl ObserveKind {
    /// In encoding order: the index is the payload of a `HookId`.
    pub const ALL: [ObserveKind; 4] = [
        ObserveKind::PanicHandler,
        ObserveKind::Abort,
        ObserveKind::AssertFunc,
        ObserveKind::TaskDelete,
    ];

    pub fn from_index(index: u32) -> Option<ObserveKind> {
        ObserveKind::ALL.get(index as usize).copied()
    }

    pub fn symbol(self) -> &'static str {
        match self {
            ObserveKind::PanicHandler => "esp_panic_handler",
            ObserveKind::Abort => "abort",
            ObserveKind::AssertFunc => "__assert_func",
            ObserveKind::TaskDelete => "vTaskDelete",
        }
    }

    /// True when reaching this hook ends the run: the guest is already on its way to a panic, so
    /// the receipt reports the observation rather than letting the image reset.
    pub fn is_fatal(self) -> bool {
        !matches!(self, ObserveKind::TaskDelete)
    }
}

/// A breakpoint or observe hook added through the API. Snapshot state, so plain data only; the
/// `HookSet` is rebuilt from it on restore.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct UserHook {
    /// False for a user observe hook.
    pub breakpoint: bool,
    /// Reported when the hook fires.
    pub label: String,
}

/// What an observe hook did, so the caller can record it and decide whether to stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub kind: ObserveKind,
    /// `pxCurrentTCBs` when it fired.
    pub task: u32,
    /// Continuations dropped; non-zero only for `vTaskDelete`.
    pub dropped: usize,
    /// The generation the task moved to, when the hook bumped one.
    pub generation: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_encoding_index() {
        for (index, kind) in ObserveKind::ALL.iter().enumerate() {
            assert_eq!(ObserveKind::from_index(index as u32), Some(*kind));
        }
        assert_eq!(ObserveKind::from_index(ObserveKind::ALL.len() as u32), None);
    }

    #[test]
    fn the_four_arch_symbols_are_the_four_kinds() {
        let names: Vec<&str> = ObserveKind::ALL.iter().map(|k| k.symbol()).collect();
        assert_eq!(
            names,
            ["esp_panic_handler", "abort", "__assert_func", "vTaskDelete"]
        );
    }

    #[test]
    fn only_task_delete_lets_the_run_continue() {
        assert!(!ObserveKind::TaskDelete.is_fatal());
        assert!(ObserveKind::ALL.iter().filter(|k| k.is_fatal()).count() == 3);
    }
}
