//! Resource readings: what this node measures of itself, and what it has
//! heard of its peers.
//!
//! # Why none of this is persisted
//!
//! The same argument [`crate::followers`] makes in its own header. A
//! utilization figure is a fact about a live process with a shelf life of
//! seconds; a stored reading would outlive the process that took it, and the
//! next process would publish a claim nobody can check. So the self sample is
//! empty until the sampler's first pass, and the peer registry is empty until
//! a peer serves one. A restarted node reports absence rather than last
//! night's numbers.
//!
//! # What is measured, and what is not
//!
//! Only what the machine knows of itself, process-locally: logical cores, CPU
//! busy, memory total and used, the disk volume the store lives on, and
//! network throughput in and out across every interface. Every
//! figure is optional, and a figure the platform cannot give is absent rather
//! than an invented number — a reading has to be labelled by what kind of
//! truth it is, and *no answer* is one of the kinds. The engine's own serving
//! load has its own surface (`INFO FOR NODE`'s lease, campaigns and follower
//! rows) and is deliberately not re-measured here.
//!
//! # Two halves, for ADR-0018's reason
//!
//! The self sample is one slot, replaced whole per pass — this machine's
//! truth, asked of the process now. The peer registry is keyed by the node id
//! the handshake proved, one entry per peer, replaced whole per hearing: what
//! a peer said about itself, plus how long ago it said it, compounded the way
//! the directory's currency rule compounds.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use scc::TreeIndex;
use tessari_encoding::NODE_ID_LEN;

/// One sample of this machine's resources, taken by the node's own sampler.
///
/// Every field is optional on purpose: a platform that cannot give a figure
/// leaves it absent, and a reader renders the absence rather than a default.
/// The fields are counts and fractions in fixed units — never floats on the
/// wire — so two nodes compare one number the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Readings {
    /// Logical CPU cores the machine presents.
    pub cpu_cores: Option<u64>,
    /// CPU busy over the sampling window, in basis points of one percent
    /// (0..=10_000). The first window after a process starts has no previous
    /// sample to measure the delta against, so it is absent — reporting a
    /// zero would read as *idle*, which is a claim, and the sampler refuses
    /// claims it cannot back.
    pub cpu_busy: Option<u64>,
    /// Physical memory the machine holds, in bytes.
    pub memory_total: Option<u64>,
    /// Memory in use, in bytes — the figure a *what is left* question answers
    /// against `memory_total`, both from the same sample so the two cannot
    /// drift apart mid-read.
    pub memory_used: Option<u64>,
    /// The disk volume the store lives on, in bytes.
    pub disk_total: Option<u64>,
    /// Bytes free on that volume, from the same sample as `disk_total`.
    pub disk_free: Option<u64>,
    /// Network throughput received, in bytes per second over the sampling
    /// window, summed across every interface the machine holds — a node's
    /// load is the sum of what its surfaces carry, and splitting it by
    /// interface here would be a second, narrower answer nobody asked for.
    /// Like `cpu_busy`, the first window after a process starts has no
    /// delta to stand on, so it is absent rather than a zero that would
    /// read as *quiet*, which is a claim.
    pub network_received: Option<u64>,
    /// Network throughput transmitted, in bytes per second over the same
    /// window and across the same interfaces as `network_received`, so the
    /// two cannot drift apart mid-read.
    pub network_transmitted: Option<u64>,
}

/// What one peer said about its resources, and how old that claim is here.
///
/// The age is compounded, not stored whole: `age_at_send` is how old the
/// sample already was when it arrived (the peer's own honest figure, taken on
/// the peer's own clock at send), and it accrues the time since this process
/// received it — *what it said plus how long ago it said it*, the directory's
/// rule, so a reader never trusts a peer's wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeardReadings {
    /// The sample the peer served.
    pub sample: Readings,
    /// How old the sample already was when it arrived.
    pub age_at_send: Duration,
    /// When this process received it.
    pub received_at: Instant,
}

impl HeardReadings {
    /// How old the sample is at `now`: what the peer said plus how long ago.
    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        self.age_at_send
            .saturating_add(now.saturating_duration_since(self.received_at))
    }
}

/// The self sample and the instant it was taken, so a reader renders the age
/// against its own clock rather than trusting a stored one.
#[derive(Debug, Clone, Copy)]
struct Held {
    sample: Readings,
    at: Instant,
}

/// What this process knows of resource readings: its own last sample, and
/// every peer's last served one.
///
/// Shared by every handle to one store for the reason the followers registry
/// is: two handles are not two nodes, and a hearing recorded against one of
/// them is a hearing the other cannot report.
#[derive(Debug, Default)]
pub struct Registry {
    /// This machine's last sample, replaced whole per pass. Empty until the
    /// sampler's first pass — a node that has never measured says so.
    own: Mutex<Option<Held>>,
    /// What each peer last served, keyed by the id the handshake proved, in id
    /// order for reporting. A `TreeIndex`, like the followers registry: reads
    /// take no lock at all, and a hearing is replaced whole, which is the one
    /// kind of write it takes.
    peers: TreeIndex<[u8; NODE_ID_LEN], HeardReadings>,
}

impl Registry {
    /// Replace this machine's sample with `sample`, taken now.
    pub(crate) fn sample(&self, sample: Readings) {
        // A poisoned holder means a sampler thread died mid-swap; the slot is
        // one value, so recovering the inner one is sound, and refusing to
        // read it would take the readings down over an unrelated failure.
        *self
            .own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Held {
            sample,
            at: Instant::now(),
        });
    }

    /// This machine's last sample and its age at `now`, or `None` before the
    /// first pass.
    #[must_use]
    pub(crate) fn own(&self, now: Instant) -> Option<(Readings, Duration)> {
        self.own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|held| (held.sample, now.saturating_duration_since(held.at)))
    }

    /// Record what `node` served, received now.
    pub(crate) fn heard(&self, node: [u8; NODE_ID_LEN], heard: HeardReadings) {
        // `_sync` like the followers registry this mirrors: the call is one
        // whole-value upsert, never held across a suspension point.
        self.peers.upsert_sync(node, heard);
    }

    /// What `node` last served, if this process has heard from it — copied
    /// out, so no guard leaves this call.
    #[must_use]
    pub(crate) fn of(&self, node: &[u8; NODE_ID_LEN]) -> Option<HeardReadings> {
        self.peers.peek_with(node, |_, held| *held)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn node(byte: u8) -> [u8; NODE_ID_LEN] {
        [byte; NODE_ID_LEN]
    }

    fn sample(cpu_busy: Option<u64>) -> Readings {
        Readings {
            cpu_cores: Some(8),
            cpu_busy,
            memory_total: Some(16_000_000_000),
            memory_used: Some(4_000_000_000),
            disk_total: Some(500_000_000_000),
            disk_free: Some(300_000_000_000),
            network_received: Some(1_000_000),
            network_transmitted: Some(500_000),
        }
    }

    #[test]
    fn the_first_sample_replaces_none_and_reports_its_age() {
        let held = Registry::default();
        assert!(held.own(Instant::now()).is_none());
        held.sample(sample(Some(4_200)));
        let (taken, age) = held
            .own(Instant::now())
            .expect("a sample after the first pass");
        assert_eq!(taken.cpu_busy, Some(4_200));
        assert!(age < Duration::from_secs(1));
    }

    #[test]
    fn a_peer_hearing_compounds_the_peers_age_with_time_here() {
        let held = Registry::default();
        held.heard(
            node(7),
            HeardReadings {
                sample: sample(None),
                age_at_send: Duration::from_secs(3),
                received_at: Instant::now(),
            },
        );
        let found = held.of(&node(7)).expect("the hearing is kept");
        assert!(found.age(Instant::now()) >= Duration::from_secs(3));
        assert!(held.of(&node(8)).is_none(), "a peer never heard is absent");
    }

    #[test]
    fn the_latest_hearing_replaces_the_last_whole() {
        let held = Registry::default();
        held.heard(
            node(7),
            HeardReadings {
                sample: sample(Some(1_000)),
                age_at_send: Duration::from_secs(9),
                received_at: Instant::now(),
            },
        );
        held.heard(
            node(7),
            HeardReadings {
                sample: sample(Some(2_000)),
                age_at_send: Duration::from_secs(1),
                received_at: Instant::now(),
            },
        );
        let found = held.of(&node(7)).expect("the latest hearing stands");
        assert_eq!(found.sample.cpu_busy, Some(2_000));
        assert_eq!(found.age_at_send, Duration::from_secs(1));
    }
}
