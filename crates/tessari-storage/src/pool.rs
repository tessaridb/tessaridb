//! A workload pool: an admission limit on the statements a classifier names,
//! as a namespace-scoped catalog row.
//!
//! For [`crate::failover`]'s reason one level down: the limit decides whose
//! statement may run, and every node has to hold the same pool or a pooled
//! cluster refuses differently on different nodes — the noisy-tenant
//! bounding the feature exists for would be an accident of which node a
//! client reached. So the pool is a catalog record, and it reaches every
//! node through the log the way the peer and policy rows do.
//!
//! # What the pool is
//!
//! The limit is the whole point — `MAX CONCURRENT n` is the one required
//! clause — and the classifier narrows what counts: one database, one user,
//! and a kind set (`reads`, `writes`, `definitions`) that defaults to all
//! three. A statement matching no declared pool runs in the namespace's
//! default pool, which names no limit and admits anything.

use tessari_types::PoolKinds;

use crate::error::{Error, Result};

/// A pool as an operator stated it, refused when its clauses do not hold
/// together. A [`Pool`] in hand is one whose relations hold because
/// [`Pool::stated`] is the only way to build one — a row read back is
/// re-checked at the catalog's `from_value` rather than trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    /// How many matching statements may run at once; the next is refused
    /// `PoolAtCapacity` naming the pool.
    max_concurrent: u64,
    /// `FOR DATABASE d`, resolved to the database's id when the row is
    /// written. `None` classifies every database in the namespace.
    database: Option<tessari_types::DatabaseId>,
    /// `FOR USER u`. `None` classifies every user.
    user: Option<String>,
    /// The statement kinds the pool bounds; [`PoolKinds::ALL`] is the absent
    /// clause's reading.
    kinds: PoolKinds,
}

impl Pool {
    /// A pool as an operator stated it.
    ///
    /// # Errors
    ///
    /// [`Error::PoolLimitNotPositive`] when the limit is zero — which the
    /// statement also refuses, so the check here is the one a read-back row
    /// earns: stored bytes are not evidence.
    pub fn stated(
        max_concurrent: u64,
        database: Option<tessari_types::DatabaseId>,
        user: Option<String>,
        kinds: PoolKinds,
    ) -> Result<Self> {
        let stated = Self {
            max_concurrent,
            database,
            user,
            kinds,
        };
        stated.holds_together()?;
        Ok(stated)
    }

    /// How many matching statements may run at once.
    #[must_use]
    pub const fn max_concurrent(&self) -> u64 {
        self.max_concurrent
    }

    /// The one database the pool classifies, if it names one.
    #[must_use]
    pub const fn database(&self) -> Option<tessari_types::DatabaseId> {
        self.database
    }

    /// The one user the pool classifies, if it names one.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The statement kinds the pool bounds.
    #[must_use]
    pub const fn kinds(&self) -> PoolKinds {
        self.kinds
    }

    fn holds_together(&self) -> Result<()> {
        if self.max_concurrent == 0 {
            return Err(Error::PoolLimitNotPositive);
        }
        Ok(())
    }
}
