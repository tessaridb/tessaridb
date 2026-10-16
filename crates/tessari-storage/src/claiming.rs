//! Records a claim is taking and has not committed yet (ADR-0126 D5).
//!
//! # Why a register beside the log
//!
//! Two claims that pick the same record both write it, and snapshot isolation
//! refuses the later commit — exclusivity needs nothing more. What it costs is
//! the refused worker's round: both read the record as free because neither
//! could see the other's uncommitted hold. This register is the one thing a
//! second claim can see before the first commits, so the second walks past the
//! record and takes the next one, the queue convention a worker pool expects.
//!
//! It is advice, never the guarantee. A claim is a write and writes run on the
//! leader, so every claimant of a record asks this one register; a claim it
//! cannot see — one started under a leader that has since changed — is still
//! decided by the commit, refused as before. Not persisted, for the reason the
//! follower registers give: an entry is a fact about a live transaction, and a
//! restarted process has none.
//!
//! # Bound
//!
//! An entry exists only while its transaction does: taken during a claim's
//! walk, dropped when the transaction ends, committed or not. So the register
//! holds at most the records the open claims are taking.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

use crate::transaction::RecordAddress;

/// Which open transaction is taking each record a claim has picked.
#[derive(Debug, Default)]
pub(crate) struct Claiming {
    taking: DashMap<RecordAddress, u64>,
    /// The last token handed to a transaction; tokens start at one.
    issued: AtomicU64,
}

impl Claiming {
    /// A token no other live transaction holds.
    ///
    /// `Relaxed` is enough: the token is compared for equality and never
    /// orders anything else.
    pub(crate) fn token(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// Whether a transaction other than `token`'s is taking `address`.
    pub(crate) fn taken_elsewhere(&self, address: &RecordAddress, token: u64) -> bool {
        self.taking
            .get(address)
            .is_some_and(|held| *held.value() != token)
    }

    /// Take `address` for `token`, answering whether it is now `token`'s.
    ///
    /// One shard acquisition for the check and the insert, so two claims
    /// cannot both find the record free.
    pub(crate) fn take(&self, address: &RecordAddress, token: u64) -> bool {
        match self.taking.entry(address.clone()) {
            Entry::Vacant(free) => {
                free.insert(token);
                true
            }
            Entry::Occupied(held) => *held.get() == token,
        }
    }

    /// Give back what `token` took.
    pub(crate) fn release(&self, token: u64, taken: &[RecordAddress]) {
        for address in taken {
            self.taking.remove_if(address, |_, held| *held == token);
        }
    }
}
