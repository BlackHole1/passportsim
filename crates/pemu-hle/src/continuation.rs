//! Serializable handler state machines, their continuations and the `hle` snapshot section.
//!
//! A handler is a step machine, never a coroutine with host stack state, so all HLE state is
//! plain values and a snapshot taken with calls outstanding restores and finishes them.
//!
//! Continuations are keyed by `(sp1, generation)`. `sp1` alone distinguishes every outstanding
//! call, because each task stack and the ISR stack of the IDF FreeRTOS port have their own
//! addresses; the generation catches a task deleted and a new one created at a reused TCB
//! address. A task continuation carries that task's entry in [`HleSection::task_generations`]; an
//! ISR continuation carries [`HleSection::isr_generation`], bumped at every magic ISR entry.

use std::collections::BTreeMap;

use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::snap::{
    Section, SnapError, SnapReader, SnapSection, SnapValue, snap_struct, value_from_section,
    value_section,
};

use crate::binding::BindingRecord;
use crate::observe::UserHook;
use crate::worker::{WakeReason, WorkerState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    Entry,
    Returned {
        a0: u32,
        a1: u32,
        /// Read back from sp1 after the call.
        scratch: Vec<u8>,
    },
    Woken {
        reason: WakeReason,
    },
}

/// Serialized state of a suspended handler. `bytes` is whatever the handler's own codec
/// produced, so `pemu-hle` never has to know a module's handler types.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HandlerState {
    /// For example `ble.worker`.
    pub handler: String,
    pub bytes: Vec<u8>,
}

/// Key of a continuation: `(sp1, generation)`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContKey {
    /// `sp` during the nested call.
    pub sp: u32,
    /// The task generation, or `isr_generation` for an ISR continuation.
    pub generation: u64,
}

/// A handler suspended in a nested guest call or a park.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Continuation {
    pub handler: HandlerState,
    /// The hook frame restored by `HleAction::Return`.
    pub frame: [u32; 32],
    /// Saved ra; `HleAction::Return` sets pc to it.
    pub ra: u32,
    /// How many times the handler has been resumed.
    pub step: u32,
    /// TCB address that must be current on return.
    pub task: u32,
    /// True when the frame was taken from the ISR stack, in which case the return checks
    /// `isr_generation` instead of the task.
    pub in_isr: bool,
    /// Scratch bytes to read back from `sp1` on return.
    pub scratch_len: u16,
    /// The outstanding callee.
    pub func: u32,
    /// `sp` before the call, restored when the handler returns.
    pub sp0: u32,
    /// True while the outstanding call is the wait the core made for `HleAction::Park`, whose
    /// return resumes the handler with `Resume::Woken` rather than `Resume::Returned`.
    pub wait: bool,
    /// True when the handler was entered at a magic ISR PC: its `Return` is the magic ISR return,
    /// where the core lowers the radio source level and leaves the ISR nesting level.
    pub magic_isr: bool,
}

/// Suspended handlers keyed by (sp1, generation).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Continuations {
    map: BTreeMap<(u32 /* sp1 */, u64 /* generation */), Continuation>,
}

impl Continuations {
    /// True when a continuation already holds `key`; a second call under it is refused.
    pub fn contains(&self, key: ContKey) -> bool {
        self.map.contains_key(&(key.sp, key.generation))
    }

    /// Returns the one it replaced, which the key-collision guard keeps `None`.
    pub fn insert(&mut self, key: ContKey, cont: Continuation) -> Option<Continuation> {
        self.map.insert((key.sp, key.generation), cont)
    }

    pub fn get(&self, key: ContKey) -> Option<&Continuation> {
        self.map.get(&(key.sp, key.generation))
    }

    /// The continuation at `sp` whatever its generation, so a diagnostic for a magic return that
    /// found nothing can name the generation that moved.
    pub fn find_by_sp(&self, sp: u32) -> Option<(ContKey, &Continuation)> {
        self.map
            .range((sp, 0)..=(sp, u64::MAX))
            .next()
            .map(|((sp, generation), cont)| {
                (
                    ContKey {
                        sp: *sp,
                        generation: *generation,
                    },
                    cont,
                )
            })
    }

    /// What a magic return does.
    pub fn take(&mut self, key: ContKey) -> Option<Continuation> {
        self.map.remove(&(key.sp, key.generation))
    }

    /// Drops every continuation of `task` (the `vTaskDelete` observe hook) and returns how many
    /// went.
    pub fn drop_task(&mut self, task: u32) -> usize {
        let before = self.map.len();
        self.map.retain(|_, cont| cont.task != task || cont.in_isr);
        before - self.map.len()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ContKey, &Continuation)> + '_ {
        self.map.iter().map(|((sp, generation), cont)| {
            (
                ContKey {
                    sp: *sp,
                    generation: *generation,
                },
                cont,
            )
        })
    }
}

/// The whole `hle` snapshot section: everything else in pemu-hle, `HookSet` included, is derived
/// from it plus the ELF. Its codec is a hand-written [`SnapValue`] with postcard's layout,
/// because `snap_struct!` has no `BTreeMap` support; the serde derives stay for serde users.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HleSection {
    pub continuations: Continuations,
    /// Bumped by the vTaskDelete observe hook; a new task at a reused TCB address starts with
    /// the bumped value.
    pub task_generations: BTreeMap<u32 /* TCB address */, u64>,
    /// Bumped at every magic ISR entry; ISR continuations key on it.
    pub isr_generation: u64,
    /// Profile id, app ELF SHA-256, per-feature status.
    pub binding: BindingRecord,
    /// Breakpoints and observe hooks added through the API.
    pub user_hooks: BTreeMap<u32, UserHook>,
    /// Generations of the magic ISRs entered and not yet returned, innermost last. An ISR
    /// continuation keys on the innermost one, so a nested radio interrupt does not move the
    /// generation the outer ISR's outstanding call keys on.
    pub isr_nesting: Vec<u64>,
    /// Runtime state of every registered worker, keyed by its magic entry (`MagicKind as u8`).
    pub workers: BTreeMap<u8, WorkerState>,
}

impl HleSection {
    /// `isr_generation` inside an ISR, the task's own generation otherwise.
    pub fn generation_for(&self, task: u32, in_isr: bool) -> u64 {
        if in_isr {
            self.isr_nesting
                .last()
                .copied()
                .unwrap_or(self.isr_generation)
        } else {
            self.task_generations.get(&task).copied().unwrap_or(0)
        }
    }

    /// Bumps a deleted task's generation, so a new task at the same TCB address starts above it,
    /// and drops its continuations. Returns the new generation and how many continuations went.
    pub fn delete_task(&mut self, task: u32) -> (u64, usize) {
        let generation = self.task_generations.entry(task).or_insert(0);
        *generation += 1;
        let generation = *generation;
        (generation, self.continuations.drop_task(task))
    }

    /// Bumps `isr_generation` at a magic ISR entry and makes it the innermost nesting level.
    pub fn enter_isr(&mut self) -> u64 {
        self.isr_generation += 1;
        self.isr_nesting.push(self.isr_generation);
        self.isr_generation
    }

    /// The innermost magic ISR returned: ISR continuations key on the generation of the ISR it
    /// interrupted, if any, again.
    pub fn leave_isr(&mut self) {
        self.isr_nesting.pop();
    }

    /// Refuses a restore whose app ELF is not the one the hooks were bound against.
    pub fn check_app_elf(&self, app_elf_sha256: &[u8; 32]) -> Result<(), SnapError> {
        if self.binding.app_elf_sha256 == *app_elf_sha256 {
            Ok(())
        } else {
            Err(SnapError::Malformed {
                at: HleSection::NAME,
                reason: "the loaded app ELF is not the one the HLE hooks were bound against",
            })
        }
    }
}

/// Writes a `BTreeMap` as a varint length followed by its pairs in key order, which is the
/// canonical encoding `Vec<T>` uses.
fn write_map<K: SnapValue, V: SnapValue>(map: &BTreeMap<K, V>, out: &mut Vec<u8>) {
    (map.len() as u64).snap_write(out);
    for (k, v) in map {
        k.snap_write(out);
        v.snap_write(out);
    }
}

fn read_map<K: SnapValue + Ord, V: SnapValue>(
    r: &mut SnapReader<'_>,
) -> Result<BTreeMap<K, V>, SnapError> {
    let len = u64::snap_read(r)?;
    let mut map = BTreeMap::new();
    for _ in 0..len {
        let k = K::snap_read(r)?;
        let v = V::snap_read(r)?;
        map.insert(k, v);
    }
    Ok(map)
}

snap_struct!(HandlerState { handler, bytes });

snap_struct!(Continuation {
    handler,
    frame,
    ra,
    step,
    task,
    in_isr,
    scratch_len,
    func,
    sp0,
    wait,
    magic_isr,
});

impl SnapValue for Continuations {
    fn snap_write(&self, out: &mut Vec<u8>) {
        (self.map.len() as u64).snap_write(out);
        for ((sp, generation), cont) in &self.map {
            sp.snap_write(out);
            generation.snap_write(out);
            cont.snap_write(out);
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Continuations, SnapError> {
        let len = u64::snap_read(r)?;
        let mut map = BTreeMap::new();
        for _ in 0..len {
            let sp = u32::snap_read(r)?;
            let generation = u64::snap_read(r)?;
            map.insert((sp, generation), Continuation::snap_read(r)?);
        }
        Ok(Continuations { map })
    }
}

snap_struct!(UserHook { breakpoint, label });

impl SnapValue for BindingRecord {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.profile_id.snap_write(out);
        self.app_elf_sha256.snap_write(out);
        (self.features.len() as u64).snap_write(out);
        for (name, status) in &self.features {
            name.snap_write(out);
            (*status as u8).snap_write(out);
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<BindingRecord, SnapError> {
        let profile_id = String::snap_read(r)?;
        let app_elf_sha256 = <[u8; 32]>::snap_read(r)?;
        let len = u64::snap_read(r)?;
        let mut features = BTreeMap::new();
        for _ in 0..len {
            let name = String::snap_read(r)?;
            let status = match u8::snap_read(r)? {
                0 => crate::binding::FeatureStatus::Bound,
                1 => crate::binding::FeatureStatus::UnsupportedImage,
                2 => crate::binding::FeatureStatus::Disabled,
                3 => crate::binding::FeatureStatus::NotLinked,
                _ => {
                    return Err(SnapError::Malformed {
                        at: HleSection::NAME,
                        reason: "FeatureStatus is not one of bound, unsupported image, disabled, not linked",
                    });
                }
            };
            features.insert(name, status);
        }
        Ok(BindingRecord {
            profile_id,
            app_elf_sha256,
            features,
            log_lines: BTreeMap::new(),
        })
    }
}

impl SnapValue for HleSection {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.continuations.snap_write(out);
        write_map(&self.task_generations, out);
        self.isr_generation.snap_write(out);
        self.binding.snap_write(out);
        write_map(&self.user_hooks, out);
        self.isr_nesting.snap_write(out);
        write_map(&self.workers, out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<HleSection, SnapError> {
        Ok(HleSection {
            continuations: Continuations::snap_read(r)?,
            task_generations: read_map(r)?,
            isr_generation: u64::snap_read(r)?,
            binding: BindingRecord::snap_read(r)?,
            user_hooks: read_map(r)?,
            isr_nesting: Vec::<u64>::snap_read(r)?,
            workers: read_map(r)?,
        })
    }
}

impl SnapSection for HleSection {
    const NAME: &'static str = pemu_core::snap::SectionId::HLE;
    const VERSION: u16 = 1;

    fn encode(&self) -> Result<Section, SnapError> {
        value_section(self)
    }

    fn decode(section: &Section) -> Result<HleSection, SnapError> {
        value_from_section(section)
    }
}
