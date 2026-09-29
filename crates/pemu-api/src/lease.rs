//! The clock lease: exactly one party (`agent`, `ui`, `endpoint` or `scenario`) drives an
//! instance's virtual time, and a conflict is `E_LEASE` naming the holder.
//!
//! A lease lives entirely in virtual time: expiry is a pure comparison against the caller's `now`,
//! never a host clock or background timer. A lease acquired at `t` with TTL `d` is held while `now
//! < t + d`; `None` is held until released or taken over. A `now` before the acquisition is not
//! expiry: snapshot restore, rewind and fork move virtual time back while the lease state does not.
//!
//! [`Lease::check_call`] gates a handler by its annotations: `read_only` commands run while another
//! party holds the lease; `advances_time` or input injection gets a retryable `E_LEASE`. While a
//! live bridge is attached the instance runs in wall time and a pause is refused unless the caller
//! detaches.

use std::fmt;

use pemu_core::time::VTime;

use crate::error::{ApiError, E_LEASE};
use crate::matchers::str_enum;
use crate::spec::Annotations;

str_enum! {
    pub enum LeaseHolder {
        /// The default for an instance an agent created.
        Agent = "agent",
        Ui = "ui",
        /// A host tool on a serial endpoint, such as `idf.py monitor` or `esptool`.
        Endpoint = "endpoint",
        Scenario = "scenario",
    }
}

/// Required to renew or release a lease. The generation makes a handle stale after a forced
/// take-over, so the old holder's next call fails rather than quietly renewing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseTicket {
    holder: LeaseHolder,
    generation: u64,
}

impl LeaseTicket {
    pub const fn holder(self) -> LeaseHolder {
        self.holder
    }

    /// Every acquisition, forced or not, gets a new one.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

impl fmt::Display for LeaseTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.holder.as_str(), self.generation)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    holder: Option<LeaseHolder>,
    acquired_at: VTime,
    /// `None` is a lease without a deadline, which only a release or a take-over ends.
    expires_at: Option<VTime>,
    generation: u64,
    live_bridges: u32,
}

impl Default for Lease {
    fn default() -> Self {
        Lease::free()
    }
}

impl Lease {
    /// Virtual time an agent call holds the lease for when it names no TTL: the default `run`
    /// timeout, since `run` is the longest-lived call. A longer timeout passes its own TTL.
    pub const DEFAULT_TTL: VTime = VTime(10_000_000_000_000);

    pub const fn free() -> Lease {
        Lease {
            holder: None,
            acquired_at: VTime(0),
            expires_at: None,
            generation: 0,
            live_bridges: 0,
        }
    }

    /// Who holds the lease at `now`, or `None` when it is free or its deadline has passed. Pure:
    /// only [`Lease::expire`] collects an expired lease. A `now` before [`Lease::acquired_at`] is a
    /// rewound clock, not an expiry.
    pub fn holder(&self, now: VTime) -> Option<LeaseHolder> {
        let holder = self.holder?;
        match self.expires_at {
            Some(deadline) if now.0 >= deadline.0 => None,
            _ => Some(holder),
        }
    }

    pub fn is_held(&self, now: VTime) -> bool {
        self.holder(now).is_some()
    }

    pub const fn expires_at(&self) -> Option<VTime> {
        self.expires_at
    }

    pub const fn acquired_at(&self) -> VTime {
        self.acquired_at
    }

    /// Bumped by every acquisition, so a [`LeaseTicket`] from an earlier one is detectably stale.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Drops an expired lease, returning the holder it had. The only place time changes a lease:
    /// the command layer calls it at the start of a command so the transition lands where the
    /// journal records it. A rewound `now` never drops a lease.
    pub fn expire(&mut self, now: VTime) -> Option<LeaseHolder> {
        let had = self.holder?;
        let deadline = self.expires_at?;
        if now.0 < deadline.0 {
            return None;
        }
        self.holder = None;
        self.expires_at = None;
        Some(had)
    }

    /// Takes the lease for `holder` until `now + ttl`, or with no deadline when `ttl` is `None`.
    /// Returns a retryable `E_LEASE` naming the holder when someone else holds it. Re-acquiring as
    /// the same holder starts a new generation.
    pub fn acquire(
        &mut self,
        holder: LeaseHolder,
        now: VTime,
        ttl: Option<VTime>,
    ) -> Result<LeaseTicket, ApiError> {
        match self.holder(now) {
            Some(other) if other != holder => Err(conflict(other, self.expires_at)),
            _ => Ok(self.take(holder, now, ttl)),
        }
    }

    /// Takes the lease whoever holds it (the web UI's `force: true`); the previous ticket becomes
    /// stale.
    pub fn force_acquire(
        &mut self,
        holder: LeaseHolder,
        now: VTime,
        ttl: Option<VTime>,
    ) -> LeaseTicket {
        self.take(holder, now, ttl)
    }

    /// Extends a held lease to `now + ttl`. A stale ticket or an already expired lease is
    /// `E_LEASE`, so a lapse is always visible: the holder acquires again rather than renewing.
    pub fn renew(
        &mut self,
        ticket: LeaseTicket,
        now: VTime,
        ttl: Option<VTime>,
    ) -> Result<LeaseTicket, ApiError> {
        self.check_ticket(ticket, now)?;
        self.expires_at = ttl.map(|d| saturating_add(now, d));
        Ok(ticket)
    }

    pub fn release(&mut self, ticket: LeaseTicket, now: VTime) -> Result<(), ApiError> {
        self.check_ticket(ticket, now)?;
        self.holder = None;
        self.expires_at = None;
        Ok(())
    }

    pub fn check_call(
        &self,
        caller: LeaseHolder,
        annotations: Annotations,
        now: VTime,
    ) -> Result<(), ApiError> {
        if annotations.read_only {
            return Ok(());
        }
        match self.holder(now) {
            Some(other) if other != caller => Err(conflict(other, self.expires_at)),
            _ => Ok(()),
        }
    }

    /// A bridge-mode LAN socket, the WISP relay, an external HCI socket, the host BLE mirror or
    /// live microphone capture.
    pub fn attach_bridge(&mut self) {
        self.live_bridges = self.live_bridges.saturating_add(1);
    }

    pub fn detach_bridge(&mut self) {
        self.live_bridges = self.live_bridges.saturating_sub(1);
    }

    /// While this is not 0 the instance runs at wall rate 1 and determinism is `live`.
    pub const fn live_bridges(&self) -> u32 {
        self.live_bridges
    }

    /// While a bridge is attached a pause is `E_LEASE` naming the bridge, unless the caller
    /// detaches, which journals a link-down first.
    pub fn check_pause(&self, detach: bool) -> Result<(), ApiError> {
        if self.live_bridges == 0 || detach {
            return Ok(());
        }
        Err(ApiError::new(
            E_LEASE,
            format!(
                "{} live bridge(s) hold the clock: a bridged peer answers in host time, so the \
                 instance runs in wall time and cannot pause",
                self.live_bridges
            ),
        )
        .retryable()
        .with_hint("ask to detach, which journals a link-down and then pauses"))
    }

    fn take(&mut self, holder: LeaseHolder, now: VTime, ttl: Option<VTime>) -> LeaseTicket {
        self.holder = Some(holder);
        self.acquired_at = now;
        self.expires_at = ttl.map(|d| saturating_add(now, d));
        self.generation = self.generation.saturating_add(1);
        LeaseTicket {
            holder,
            generation: self.generation,
        }
    }

    fn check_ticket(&self, ticket: LeaseTicket, now: VTime) -> Result<(), ApiError> {
        if self.generation == ticket.generation && self.holder(now) == Some(ticket.holder) {
            return Ok(());
        }
        Err(match self.holder(now) {
            Some(other) => conflict(other, self.expires_at),
            None => ApiError::new(
                E_LEASE,
                format!(
                    "the {} lease expired or was released; take it again before advancing time",
                    ticket.holder.as_str()
                ),
            )
            .retryable()
            .with_hint("call `clock` to take the lease"),
        })
    }
}

fn conflict(holder: LeaseHolder, expires_at: Option<VTime>) -> ApiError {
    let deadline = match expires_at {
        Some(t) => format!(" until vt {} us", t.as_us()),
        None => String::new(),
    };
    ApiError::new(
        E_LEASE,
        format!("the clock lease is held by `{}`{deadline}", holder.as_str()),
    )
    .retryable()
    .with_hint("wait for the holder to release it, or take it over from the holding surface")
}

/// Saturates so a long TTL never wraps.
fn saturating_add(a: VTime, b: VTime) -> VTime {
    VTime(a.0.saturating_add(b.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl(ms: u64) -> Option<VTime> {
        Some(VTime::from_ms(ms))
    }

    fn read_only() -> Annotations {
        Annotations {
            read_only: true,
            ..Annotations::EMPTY
        }
    }

    fn advances_time() -> Annotations {
        Annotations {
            advances_time: true,
            needs_instance: true,
            ..Annotations::EMPTY
        }
    }

    fn injects_input() -> Annotations {
        Annotations {
            needs_instance: true,
            ..Annotations::EMPTY
        }
    }

    #[test]
    fn acquire_renew_and_release_walk_one_holder_through() {
        let mut lease = Lease::free();
        assert_eq!(lease.holder(VTime(0)), None);
        assert_eq!(lease.generation(), 0);

        let t = lease
            .acquire(LeaseHolder::Agent, VTime::from_ms(100), ttl(50))
            .unwrap();
        assert_eq!(t.holder(), LeaseHolder::Agent);
        assert_eq!(lease.generation(), 1);
        assert_eq!(lease.acquired_at(), VTime::from_ms(100));
        assert_eq!(lease.expires_at(), Some(VTime::from_ms(150)));

        let t = lease.renew(t, VTime::from_ms(140), ttl(50)).unwrap();
        assert_eq!(lease.expires_at(), Some(VTime::from_ms(190)));
        assert_eq!(lease.generation(), 1, "a renewal is not a new acquisition");

        lease.release(t, VTime::from_ms(150)).unwrap();
        assert_eq!(lease.holder(VTime::from_ms(150)), None);
        assert_eq!(
            lease
                .renew(t, VTime::from_ms(151), ttl(50))
                .unwrap_err()
                .code,
            E_LEASE
        );
    }

    #[test]
    fn expiry_is_deterministic_in_virtual_time() {
        let mut lease = Lease::free();
        lease
            .acquire(LeaseHolder::Agent, VTime::from_ms(1_000), ttl(250))
            .unwrap();
        assert_eq!(
            lease.holder(VTime::from_ms(1_000)),
            Some(LeaseHolder::Agent)
        );
        assert_eq!(
            lease.holder(VTime(VTime::from_ms(1_250).0 - 1)),
            Some(LeaseHolder::Agent)
        );
        assert_eq!(
            lease.holder(VTime::from_ms(1_250)),
            None,
            "free at the deadline"
        );
        assert_eq!(lease.holder(VTime::from_ms(9_999)), None);

        // Asking does not change the lease; only `expire` collects it, and only once.
        assert_eq!(lease.expire(VTime::from_ms(1_249)), None);
        assert_eq!(
            lease.expire(VTime::from_ms(1_250)),
            Some(LeaseHolder::Agent)
        );
        assert_eq!(lease.expire(VTime::from_ms(1_250)), None);

        let mut a = Lease::free();
        a.acquire(LeaseHolder::Ui, VTime::from_ms(7), ttl(3))
            .unwrap();
        let forward: Vec<bool> = (0..20).map(|ms| a.is_held(VTime::from_ms(ms))).collect();
        let backward: Vec<bool> = (0..20)
            .rev()
            .map(|ms| a.is_held(VTime::from_ms(ms)))
            .collect();
        assert_eq!(
            forward,
            backward.into_iter().rev().collect::<Vec<bool>>(),
            "reading the lease must not change it"
        );
        assert_eq!(
            forward,
            (0..20).map(|ms| ms < 10).collect::<Vec<bool>>(),
            "only the deadline frees a lease; a `now` before the acquisition is a rewind"
        );
    }

    /// Restore, rewind and fork move virtual time back while the lease state does not; the lease
    /// must still be held.
    #[test]
    fn a_rewound_clock_does_not_free_a_lease() {
        let mut forever = Lease::free();
        forever
            .acquire(LeaseHolder::Endpoint, VTime::from_ms(100), None)
            .unwrap();
        assert_eq!(forever.expire(VTime::from_ms(50)), None);
        assert_eq!(
            forever.holder(VTime::from_ms(50)),
            Some(LeaseHolder::Endpoint)
        );
        assert_eq!(
            forever.holder(VTime::from_ms(200)),
            Some(LeaseHolder::Endpoint)
        );
        let e = forever
            .acquire(LeaseHolder::Agent, VTime::from_ms(50), ttl(10))
            .unwrap_err();
        assert_eq!(e.code, E_LEASE);
        assert!(e.message.contains("endpoint"), "{}", e.message);

        let mut timed = Lease::free();
        let ticket = timed
            .acquire(LeaseHolder::Agent, VTime::from_ms(100), ttl(50))
            .unwrap();
        assert_eq!(timed.expire(VTime::from_ms(10)), None);
        assert_eq!(timed.holder(VTime::from_ms(10)), Some(LeaseHolder::Agent));
        timed.renew(ticket, VTime::from_ms(10), ttl(50)).unwrap();
        assert_eq!(timed.expire(VTime::from_ms(60)), Some(LeaseHolder::Agent));
        assert_eq!(timed.expire(VTime::from_ms(60)), None);
    }

    #[test]
    fn a_lease_without_a_deadline_never_expires() {
        let mut lease = Lease::free();
        lease
            .acquire(LeaseHolder::Endpoint, VTime(0), None)
            .unwrap();
        assert_eq!(lease.expires_at(), None);
        assert_eq!(lease.holder(VTime(u64::MAX)), Some(LeaseHolder::Endpoint));
        assert_eq!(lease.expire(VTime(u64::MAX)), None);
    }

    #[test]
    fn a_very_long_ttl_saturates_instead_of_wrapping() {
        let mut lease = Lease::free();
        lease
            .acquire(
                LeaseHolder::Scenario,
                VTime(u64::MAX - 1),
                Some(VTime(1_000)),
            )
            .unwrap();
        assert_eq!(lease.expires_at(), Some(VTime(u64::MAX)));
        assert_eq!(
            lease.holder(VTime(u64::MAX - 1)),
            Some(LeaseHolder::Scenario)
        );
    }

    #[test]
    fn a_conflict_names_the_holder() {
        let mut lease = Lease::free();
        lease
            .acquire(LeaseHolder::Agent, VTime(0), ttl(100))
            .unwrap();
        for other in [
            LeaseHolder::Ui,
            LeaseHolder::Endpoint,
            LeaseHolder::Scenario,
        ] {
            let e = lease.acquire(other, VTime(0), ttl(10)).unwrap_err();
            assert_eq!(e.code, E_LEASE);
            assert!(e.retryable, "a held lease is retryable");
            assert!(e.message.contains("agent"), "{}", e.message);
        }
        lease
            .acquire(LeaseHolder::Ui, VTime::from_ms(100), ttl(10))
            .unwrap();
        assert_eq!(lease.holder(VTime::from_ms(100)), Some(LeaseHolder::Ui));
    }

    #[test]
    fn a_forced_take_over_makes_the_old_ticket_stale() {
        let mut lease = Lease::free();
        let agent = lease
            .acquire(LeaseHolder::Agent, VTime(0), ttl(1_000))
            .unwrap();
        let ui = lease.force_acquire(LeaseHolder::Ui, VTime::from_ms(10), ttl(1_000));
        assert_ne!(agent.generation(), ui.generation());
        let e = lease.renew(agent, VTime::from_ms(20), ttl(10)).unwrap_err();
        assert_eq!(e.code, E_LEASE);
        assert!(e.message.contains("ui"), "{}", e.message);
        let e = lease.release(agent, VTime::from_ms(20)).unwrap_err();
        assert_eq!(e.code, E_LEASE);
        lease.renew(ui, VTime::from_ms(20), ttl(10)).unwrap();
    }

    #[test]
    fn read_only_calls_pass_a_held_lease_and_time_advancing_calls_do_not() {
        let mut lease = Lease::free();
        lease
            .acquire(LeaseHolder::Ui, VTime(0), ttl(1_000))
            .unwrap();

        lease
            .check_call(LeaseHolder::Agent, read_only(), VTime(0))
            .unwrap();
        for annotations in [advances_time(), injects_input()] {
            let e = lease
                .check_call(LeaseHolder::Agent, annotations, VTime(0))
                .unwrap_err();
            assert_eq!(e.code, E_LEASE);
            assert!(e.message.contains("ui"), "{}", e.message);
        }
        lease
            .check_call(LeaseHolder::Ui, advances_time(), VTime(0))
            .unwrap();
        lease
            .check_call(LeaseHolder::Agent, advances_time(), VTime::from_ms(1_000))
            .unwrap();
        let free = Lease::free();
        free.check_call(LeaseHolder::Scenario, advances_time(), VTime(0))
            .unwrap();
    }

    #[test]
    fn a_live_bridge_refuses_a_pause_until_the_caller_detaches() {
        let mut lease = Lease::free();
        lease.check_pause(false).unwrap();
        lease.attach_bridge();
        lease.attach_bridge();
        assert_eq!(lease.live_bridges(), 2);
        let e = lease.check_pause(false).unwrap_err();
        assert_eq!(e.code, E_LEASE);
        assert!(e.message.contains("bridge"), "{}", e.message);
        lease.check_pause(true).unwrap();
        lease.detach_bridge();
        assert!(lease.check_pause(false).is_err(), "one bridge is still up");
        lease.detach_bridge();
        lease.check_pause(false).unwrap();
        lease.detach_bridge();
        assert_eq!(lease.live_bridges(), 0, "the count saturates at 0");
    }

    #[test]
    fn the_default_ttl_is_the_run_timeout() {
        assert_eq!(Lease::DEFAULT_TTL, VTime::from_ms(10_000));
        let mut lease = Lease::free();
        lease
            .acquire(LeaseHolder::Agent, VTime(0), Some(Lease::DEFAULT_TTL))
            .unwrap();
        assert_eq!(lease.expires_at(), Some(VTime::from_ms(10_000)));
    }

    #[test]
    fn the_four_holders_are_the_whole_vocabulary() {
        assert_eq!(LeaseHolder::ALL.len(), 4);
        assert_eq!(LeaseHolder::vocabulary(), "agent, ui, endpoint, scenario");
        for holder in LeaseHolder::ALL {
            assert_eq!(LeaseHolder::parse(holder.as_str()), Some(*holder));
        }
        assert_eq!(LeaseHolder::parse("bridge"), None);
    }
}
