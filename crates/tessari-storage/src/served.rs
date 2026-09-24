//! What this node's upstream last said it served it under (G031, ADR-0081).
//!
//! # Why a follower needs to know
//!
//! A follower of one shard holds that shard's records and its database's
//! definitions — every table's, split or not — so its catalog names tables whose
//! records it does not have. A read there answered from what it holds would be a
//! part presented as the whole, with nothing in an error state. Knowing the
//! reach it was served under is what lets it refuse instead.
//!
//! # Why it authorises nothing
//!
//! The grant stays on the upstream (`tessari_wire::Serving` still refuses every
//! log outside it). This is a REPORT about a collect, and it is only ever used to
//! narrow — which logs this node bothers asking for, and which reads it will
//! answer. A node that recorded a wider reach than it was granted would still be
//! refused every log outside the grant, and would refuse fewer reads of tables it
//! does not hold — which is why the upstream's answer, and nothing typed by an
//! operator, is what writes it.
//!
//! # Where it lives
//!
//! Its own `meta` key, like the log retention, and not the node identity: folding
//! it in would make a store written by this build unreadable by an older one.
//! Held in memory as well, because every read of a table asks it, and a backend
//! round trip per `SELECT` is the cost `series` exists to avoid.

use std::sync::RwLock;

use tessari_encoding::{ServedReach, ServedReachKey, StoreKey, StoreValue};
use tessari_kv::{KvBackend, WriteBatch};

use crate::catalog::Reach;
use crate::error::Result;
use crate::store::Store;

/// The in-memory copy, loaded when the store opens.
#[derive(Debug, Default)]
pub(crate) struct Served {
    held: RwLock<Option<Reach>>,
}

impl Served {
    /// Load what the store holds.
    pub(crate) fn load(backend: &dyn KvBackend) -> Result<Self> {
        let held = match backend.get(ServedReachKey::keyspace(), &ServedReachKey.encode())? {
            Some(value) => Some(ServedReach::decode(value.as_slice())?.0),
            None => None,
        };
        Ok(Self {
            held: RwLock::new(held),
        })
    }
}

impl Store {
    /// The reach this node's upstream last served it under, or `None` when it
    /// has never been served — which reads as *holds everything it has*.
    #[must_use]
    pub fn served(&self) -> Option<Reach> {
        // A poisoned lock answers "not served": the node refuses nothing it
        // would have refused before shards existed, which is the state every
        // node was in, rather than refusing everything.
        self.served_state().held.read().ok().and_then(|held| *held)
    }

    /// Record the reach the upstream said it served this node under.
    ///
    /// Written only when it moved, so a follower collecting every round does not
    /// rewrite one key every round.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure.
    pub fn record_served(&self, reach: Reach) -> Result<()> {
        if self.served() == Some(reach) {
            return Ok(());
        }
        let batch = WriteBatch::new().put(
            ServedReachKey::keyspace(),
            ServedReachKey.encode(),
            ServedReach(reach).encode(),
        );
        self.backend().apply(batch)?;
        if let Ok(mut held) = self.served_state().held.write() {
            *held = Some(reach);
        }
        Ok(())
    }
}
