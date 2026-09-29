//! Instance ids, lifecycle states and the `needs_instance` contract.
//!
//! [`InstanceTable::bind`] enforces, in order:
//!
//! 1. a command without `needs_instance` given an `instance` argument is `E_USAGE`, so a typo is
//!    not silently ignored;
//! 2. an explicit id must name a live instance; one that never existed or was stopped is `E_STATE`;
//! 3. with no id, exactly one live instance is used; none is `E_STATE` and several is `E_USAGE`
//!    naming them;
//! 4. an `advances_time` command also needs a state that can advance time.
//!
//! `advances_time` implies `needs_instance` ([`check_annotations`]).

use std::collections::BTreeMap;
use std::fmt;

use pemu_core::time::VTime;

use crate::error::{ApiError, E_STATE, E_USAGE};
use crate::lease::Lease;
use crate::matchers::str_enum;
use crate::spec::Annotations;

str_enum! {
    /// From the id's leading letter.
    pub enum InstanceKind {
        Process = "p",
        /// `b<n>`: a browser-hosted instance registered through the relay, running as wasm in the
        /// page.
        Browser = "b",
    }
}

/// An instance id such as `p1` or `b1`. Ordered by kind then index, so listings are deterministic.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId {
    kind: InstanceKind,
    index: u32,
}

impl InstanceId {
    /// `None` when `index` is 0: ids count from 1.
    pub const fn new(kind: InstanceKind, index: u32) -> Option<InstanceId> {
        if index == 0 {
            return None;
        }
        Some(InstanceId { kind, index })
    }

    pub const fn kind(self) -> InstanceKind {
        self.kind
    }

    pub const fn index(self) -> u32 {
        self.index
    }

    /// The spelling is canonical (no sign, leading zero or space), so one instance has exactly one
    /// id text to key cursors, receipts and artifact paths by.
    pub fn parse(text: &str) -> Result<InstanceId, ApiError> {
        let bad = || {
            ApiError::new(
                E_USAGE,
                format!(
                    "`{text}` is not an instance id: a kind letter ({}) and a number counting \
                     from 1, as in `p1`",
                    InstanceKind::vocabulary()
                ),
            )
            .with_hint("`status` lists the live instances")
        };
        let (letter, digits) = text.split_at_checked(1).ok_or_else(bad)?;
        let kind = InstanceKind::parse(letter).ok_or_else(bad)?;
        if digits.is_empty()
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let index: u32 = digits.parse().map_err(|_| bad())?;
        InstanceId::new(kind, index).ok_or_else(bad)
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.kind.as_str(), self.index)
    }
}

str_enum! {
    /// Lifecycle state of an instance, the first field `status` reports. Guest states such as
    /// `halted` are reported from the machine, not here.
    pub enum Lifecycle {
        Starting = "starting",
        Paused = "paused",
        Running = "running",
        /// A guest fault stopped the run before the automatic reboot; the next `run` continues into
        /// it.
        Faulted = "faulted",
        /// The power rail is off; time does not advance until the instance powers on.
        PoweredOff = "powered_off",
        Stopped = "stopped",
    }
}

impl Lifecycle {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Lifecycle::Stopped)
    }

    /// `Starting` has no clock yet and `PoweredOff` cannot run; `Faulted` continues into the
    /// reboot.
    pub const fn accepts_time_advance(self) -> bool {
        matches!(
            self,
            Lifecycle::Paused | Lifecycle::Running | Lifecycle::Faulted
        )
    }

    /// Every non-terminal state may stay, or go to `Stopped` or `PoweredOff`: the power rail drops
    /// independently of the CPU, so a brownout or long power-button hold reaches a `Faulted` or
    /// `Starting` instance too. Otherwise:
    ///
    /// | From | To |
    /// |---|---|
    /// | `Starting` | `Paused`, `Faulted`, `PoweredOff`, `Stopped` |
    /// | `Paused` | `Running`, `PoweredOff`, `Stopped` |
    /// | `Running` | `Paused`, `Faulted`, `PoweredOff`, `Stopped` |
    /// | `Faulted` | `Running`, `Paused`, `PoweredOff`, `Stopped` |
    /// | `PoweredOff` | `Paused`, `Stopped` |
    /// | `Stopped` | nothing |
    pub const fn can_transition_to(self, to: Lifecycle) -> bool {
        use Lifecycle::*;
        if self.is_terminal() {
            return false;
        }
        if matches!(to, Stopped | PoweredOff) {
            return true;
        }
        match (self, to) {
            (a, b) if matches_same(a, b) => true,
            (Starting, Paused | Faulted) => true,
            (Paused, Running) => true,
            (Running, Paused | Faulted) => true,
            (Faulted, Running | Paused) => true,
            (PoweredOff, Paused) => true,
            _ => false,
        }
    }

    pub fn transition(self, to: Lifecycle) -> Result<Lifecycle, ApiError> {
        if self.can_transition_to(to) {
            return Ok(to);
        }
        Err(ApiError::new(
            E_STATE,
            format!(
                "an instance cannot go from `{}` to `{}`",
                self.as_str(),
                to.as_str()
            ),
        ))
    }
}

/// `PartialEq` is not usable in a `const fn`.
const fn matches_same(a: Lifecycle, b: Lifecycle) -> bool {
    a as u8 == b as u8
}

/// What `status` reports before it asks the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceState {
    pub lifecycle: Lifecycle,
    pub lease: Lease,
    pub since: VTime,
}

impl InstanceState {
    pub fn new(since: VTime) -> InstanceState {
        InstanceState {
            lifecycle: Lifecycle::Starting,
            lease: Lease::free(),
            since,
        }
    }

    pub fn transition(&mut self, to: Lifecycle, now: VTime) -> Result<(), ApiError> {
        self.lifecycle = self.lifecycle.transition(to)?;
        self.since = now;
        Ok(())
    }
}

/// Every instance of one daemon or wasm page. A stopped instance keeps its entry and its index is
/// never reused, so `p1` never means another device; `forget` prunes an entry.
#[derive(Clone, Debug, Default)]
pub struct InstanceTable {
    entries: BTreeMap<InstanceId, InstanceState>,
    next: BTreeMap<InstanceKind, u32>,
}

impl InstanceTable {
    pub fn new() -> InstanceTable {
        InstanceTable::default()
    }

    /// Indices count from 1 per kind and are never reused.
    pub fn create(&mut self, kind: InstanceKind, now: VTime) -> InstanceId {
        let slot = self.next.entry(kind).or_insert(1);
        let index = *slot;
        *slot = slot.saturating_add(1);
        let id = InstanceId {
            kind,
            index: index.max(1),
        };
        self.entries.insert(id, InstanceState::new(now));
        id
    }

    pub fn get(&self, id: InstanceId) -> Option<&InstanceState> {
        self.entries.get(&id)
    }

    pub fn get_mut(&mut self, id: InstanceId) -> Option<&mut InstanceState> {
        self.entries.get_mut(&id)
    }

    /// A live instance is never dropped; `stop` must come first.
    pub fn forget(&mut self, id: InstanceId) -> bool {
        match self.entries.get(&id) {
            Some(s) if s.lifecycle.is_terminal() => self.entries.remove(&id).is_some(),
            _ => false,
        }
    }

    pub fn live_ids(&self) -> Vec<InstanceId> {
        self.entries
            .iter()
            .filter(|(_, s)| !s.lifecycle.is_terminal())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Resolves the instance a call addresses, by the contract in the module docs. `Ok(None)` means
    /// the command does not address one; a malformed `requested` id is `E_USAGE`.
    pub fn bind(
        &self,
        annotations: Annotations,
        requested: Option<&str>,
    ) -> Result<Option<InstanceId>, ApiError> {
        if !annotations.needs_instance {
            return match requested {
                None => Ok(None),
                Some(text) => Err(ApiError::new(
                    E_USAGE,
                    format!(
                        "this command addresses no instance, so `instance: {text}` is not one of \
                         its arguments"
                    ),
                )),
            };
        }
        let id = match requested {
            Some(text) => {
                let id = InstanceId::parse(text)?;
                match self.entries.get(&id) {
                    Some(s) if !s.lifecycle.is_terminal() => id,
                    Some(_) => {
                        return Err(
                            ApiError::new(E_STATE, format!("instance `{id}` is stopped"))
                                .with_hint("`start` creates a new instance"),
                        );
                    }
                    None => {
                        return Err(ApiError::new(E_STATE, format!("no instance `{id}`"))
                            .with_hint("`status` lists the live instances"));
                    }
                }
            }
            None => {
                let live = self.live_ids();
                match live.len() {
                    1 => live[0],
                    0 => {
                        return Err(ApiError::new(E_STATE, "no instance is running")
                            .with_hint("`start --fw <corpus id or path>` creates one"));
                    }
                    _ => {
                        let names: Vec<String> = live.iter().map(InstanceId::to_string).collect();
                        return Err(ApiError::new(
                            E_USAGE,
                            format!(
                                "`instance` is required while {} instances are running: {}",
                                live.len(),
                                names.join(", ")
                            ),
                        ));
                    }
                }
            }
        };
        if annotations.advances_time {
            let state = self
                .entries
                .get(&id)
                .expect("the id was resolved from this table");
            if !state.lifecycle.accepts_time_advance() {
                return Err(ApiError::new(
                    E_STATE,
                    format!(
                        "instance `{id}` is `{}`, so virtual time cannot advance",
                        state.lifecycle.as_str()
                    ),
                )
                .with_hint(match state.lifecycle {
                    Lifecycle::PoweredOff => "`input power on` powers the instance up",
                    Lifecycle::Starting => "wait for the instance to finish starting",
                    _ => "`start` creates a new instance",
                }));
            }
        }
        Ok(Some(id))
    }
}

pub const fn check_annotations(annotations: Annotations) -> Result<(), &'static str> {
    if annotations.advances_time && !annotations.needs_instance {
        return Err(
            "`advances_time` needs `needs_instance`: there is no clock without an instance",
        );
    }
    if annotations.advances_time && annotations.read_only {
        return Err(
            "`advances_time` and `read_only` exclude each other: advancing time changes the guest",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::E_LEASE;
    use crate::lease::LeaseHolder;

    fn ann(f: impl FnOnce(&mut Annotations)) -> Annotations {
        let mut a = Annotations::EMPTY;
        f(&mut a);
        a
    }

    fn needs_instance() -> Annotations {
        ann(|a| a.needs_instance = true)
    }

    fn advances_time() -> Annotations {
        ann(|a| {
            a.needs_instance = true;
            a.advances_time = true;
        })
    }

    #[test]
    fn instance_ids_parse_only_in_their_canonical_spelling() {
        let good = [
            ("p1", InstanceKind::Process, 1),
            ("b1", InstanceKind::Browser, 1),
            ("p7", InstanceKind::Process, 7),
            ("b16", InstanceKind::Browser, 16),
            ("p4294967295", InstanceKind::Process, u32::MAX),
        ];
        for (text, kind, index) in good {
            let id = InstanceId::parse(text).unwrap_or_else(|e| panic!("{text}: {}", e.message));
            assert_eq!(id.kind(), kind, "{text}");
            assert_eq!(id.index(), index, "{text}");
            assert_eq!(id.to_string(), text, "{text} round-trips");
        }
        let bad = [
            "",            // empty
            "p",           // no index
            "1",           // no kind
            "P1",          // ids are lowercase
            "x1",          // unknown kind
            "p0",          // indices count from 1
            "p01",         // no leading zero
            "p+1",         // no sign
            "p-1",         // no sign
            "p 1",         // no space
            " p1",         // no space
            "p1x",         // trailing text
            "p1.2",        // not a number
            "p4294967296", // past u32
        ];
        for text in bad {
            let e = InstanceId::parse(text)
                .err()
                .unwrap_or_else(|| panic!("`{text}` should be refused"));
            assert_eq!(e.code, E_USAGE, "{text}");
        }
        assert_eq!(InstanceId::new(InstanceKind::Process, 0), None);
    }

    #[test]
    fn ids_order_by_kind_then_index() {
        let mut ids = [
            InstanceId::parse("p10").unwrap(),
            InstanceId::parse("b2").unwrap(),
            InstanceId::parse("p2").unwrap(),
            InstanceId::parse("b1").unwrap(),
        ];
        ids.sort();
        let texts: Vec<String> = ids.iter().map(InstanceId::to_string).collect();
        assert_eq!(texts, ["p2", "p10", "b1", "b2"]);
    }

    #[test]
    fn lifecycle_transitions_follow_the_documented_table() {
        use Lifecycle::*;
        let allowed: &[(Lifecycle, &[Lifecycle])] = &[
            (Starting, &[Starting, Paused, Faulted, PoweredOff, Stopped]),
            (Paused, &[Paused, Running, PoweredOff, Stopped]),
            (Running, &[Running, Paused, Faulted, PoweredOff, Stopped]),
            (Faulted, &[Faulted, Running, Paused, PoweredOff, Stopped]),
            (PoweredOff, &[PoweredOff, Paused, Stopped]),
            (Stopped, &[]),
        ];
        for (from, tos) in allowed {
            for to in Lifecycle::ALL {
                let want = tos.contains(to);
                assert_eq!(
                    from.can_transition_to(*to),
                    want,
                    "{} -> {}",
                    from.as_str(),
                    to.as_str()
                );
                let got = from.transition(*to);
                assert_eq!(got.is_ok(), want, "{} -> {}", from.as_str(), to.as_str());
                if let Err(e) = got {
                    assert_eq!(e.code, E_STATE);
                }
            }
        }
        assert!(Stopped.is_terminal());
        assert!(Lifecycle::ALL.iter().filter(|l| l.is_terminal()).count() == 1);
    }

    #[test]
    fn the_power_rail_drops_from_every_non_terminal_state() {
        use Lifecycle::*;
        for from in [Starting, Paused, Running, Faulted, PoweredOff] {
            assert_eq!(
                from.transition(PoweredOff).unwrap(),
                PoweredOff,
                "{} -> powered_off",
                from.as_str()
            );
        }
        assert_eq!(Stopped.transition(PoweredOff).unwrap_err().code, E_STATE);
    }

    #[test]
    fn only_paused_running_and_faulted_advance_time() {
        use Lifecycle::*;
        for state in Lifecycle::ALL {
            let want = matches!(state, Paused | Running | Faulted);
            assert_eq!(state.accepts_time_advance(), want, "{}", state.as_str());
        }
    }

    #[test]
    fn a_started_instance_walks_its_lifecycle() {
        let mut table = InstanceTable::new();
        let id = table.create(InstanceKind::Process, VTime(0));
        assert_eq!(id.to_string(), "p1");
        let state = table.get(id).expect("just created");
        assert_eq!(state.lifecycle, Lifecycle::Starting);
        assert_eq!(state.lease, Lease::free());

        let s = table.get_mut(id).expect("live");
        s.transition(Lifecycle::Paused, VTime::from_ms(12)).unwrap();
        assert_eq!(s.since, VTime::from_ms(12));
        s.transition(Lifecycle::Running, VTime::from_ms(13))
            .unwrap();
        s.transition(Lifecycle::Faulted, VTime::from_ms(14))
            .unwrap();
        // The next run continues into the reboot.
        s.transition(Lifecycle::Running, VTime::from_ms(15))
            .unwrap();
        assert_eq!(
            s.transition(Lifecycle::Starting, VTime::from_ms(16))
                .unwrap_err()
                .code,
            E_STATE
        );
        s.transition(Lifecycle::Stopped, VTime::from_ms(17))
            .unwrap();
        assert!(table.live_ids().is_empty());
    }

    #[test]
    fn indices_count_from_one_per_kind_and_are_never_reused() {
        let mut table = InstanceTable::new();
        let p1 = table.create(InstanceKind::Process, VTime(0));
        let b1 = table.create(InstanceKind::Browser, VTime(0));
        let p2 = table.create(InstanceKind::Process, VTime(0));
        assert_eq!(
            [p1.to_string(), b1.to_string(), p2.to_string()],
            ["p1", "b1", "p2"]
        );
        table
            .get_mut(p1)
            .unwrap()
            .transition(Lifecycle::Stopped, VTime(0))
            .unwrap();
        assert!(table.forget(p1));
        assert!(!table.forget(p1));
        assert!(!table.forget(b1), "a live instance is never forgotten");
        let p3 = table.create(InstanceKind::Process, VTime(0));
        assert_eq!(
            p3.to_string(),
            "p3",
            "a freed index is not handed out again"
        );
    }

    #[test]
    fn a_command_without_needs_instance_refuses_an_instance_argument() {
        let table = InstanceTable::new();
        assert_eq!(table.bind(Annotations::EMPTY, None).unwrap(), None);
        let e = table.bind(Annotations::EMPTY, Some("p1")).unwrap_err();
        assert_eq!(e.code, E_USAGE);
    }

    #[test]
    fn needs_instance_resolves_by_the_documented_contract() {
        let mut table = InstanceTable::new();
        let e = table.bind(needs_instance(), None).unwrap_err();
        assert_eq!(e.code, E_STATE);
        let e = table.bind(needs_instance(), Some("p1")).unwrap_err();
        assert_eq!(e.code, E_STATE);
        let e = table.bind(needs_instance(), Some("nonsense")).unwrap_err();
        assert_eq!(e.code, E_USAGE);

        let p1 = table.create(InstanceKind::Process, VTime(0));
        assert_eq!(table.bind(needs_instance(), None).unwrap(), Some(p1));
        assert_eq!(table.bind(needs_instance(), Some("p1")).unwrap(), Some(p1));

        let p2 = table.create(InstanceKind::Process, VTime(0));
        let e = table.bind(needs_instance(), None).unwrap_err();
        assert_eq!(e.code, E_USAGE);
        assert!(
            e.message.contains("p1") && e.message.contains("p2"),
            "{}",
            e.message
        );
        assert_eq!(table.bind(needs_instance(), Some("p2")).unwrap(), Some(p2));

        table
            .get_mut(p2)
            .unwrap()
            .transition(Lifecycle::Stopped, VTime(0))
            .unwrap();
        let e = table.bind(needs_instance(), Some("p2")).unwrap_err();
        assert_eq!(e.code, E_STATE);
        assert!(e.message.contains("stopped"), "{}", e.message);
        assert_eq!(table.bind(needs_instance(), None).unwrap(), Some(p1));
    }

    #[test]
    fn advances_time_needs_a_state_that_can_advance_time() {
        let mut table = InstanceTable::new();
        let id = table.create(InstanceKind::Process, VTime(0));
        let e = table.bind(advances_time(), None).unwrap_err();
        assert_eq!(e.code, E_STATE);
        assert_eq!(table.bind(needs_instance(), None).unwrap(), Some(id));

        table
            .get_mut(id)
            .unwrap()
            .transition(Lifecycle::Paused, VTime(0))
            .unwrap();
        assert_eq!(table.bind(advances_time(), None).unwrap(), Some(id));

        table
            .get_mut(id)
            .unwrap()
            .transition(Lifecycle::PoweredOff, VTime(0))
            .unwrap();
        let e = table.bind(advances_time(), None).unwrap_err();
        assert_eq!(e.code, E_STATE);
        assert!(e.hint.is_some());
    }

    #[test]
    fn annotation_rules_hold_for_instances_and_the_clock() {
        assert!(check_annotations(Annotations::EMPTY).is_ok());
        assert!(check_annotations(needs_instance()).is_ok());
        assert!(check_annotations(advances_time()).is_ok());
        assert!(check_annotations(ann(|a| a.advances_time = true)).is_err());
        assert!(
            check_annotations(ann(|a| {
                a.needs_instance = true;
                a.advances_time = true;
                a.read_only = true;
            }))
            .is_err()
        );
    }

    #[test]
    fn an_instance_carries_its_own_lease() {
        let mut table = InstanceTable::new();
        let id = table.create(InstanceKind::Process, VTime(0));
        let state = table.get_mut(id).unwrap();
        state.transition(Lifecycle::Paused, VTime(0)).unwrap();
        let ticket = state
            .lease
            .acquire(LeaseHolder::Agent, VTime(0), Some(Lease::DEFAULT_TTL))
            .unwrap();
        assert_eq!(
            state.lease.holder(VTime(0)),
            Some(LeaseHolder::Agent),
            "the agent call holds the lease for its duration"
        );
        let e = state
            .lease
            .acquire(LeaseHolder::Ui, VTime(0), None)
            .unwrap_err();
        assert_eq!(e.code, E_LEASE);
        state.lease.release(ticket, VTime(0)).unwrap();
        assert_eq!(state.lease.holder(VTime(0)), None);
    }
}
