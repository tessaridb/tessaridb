//! A place inside an open transaction that its work can be rolled back to
//! (ADR-0134 D11).
//!
//! What a transaction has done and not yet committed is its buffered writes and
//! the side tables written beside them — the instants writes expire at, what a
//! write said about a record's lifetime, the topic trims. Those are what a
//! savepoint holds a copy of, and rolling back puts them back. Two things a
//! transaction does are not in that buffer: a queue claim it took, which is held
//! in the store's claim register so another transaction is kept off the record,
//! and work handed to other leaders. Neither can be given back by restoring a
//! copy, so a rollback across either is refused rather than half done.

use std::collections::{BTreeMap, HashMap};

use tessari_encoding::RecordValue;
use tessari_types::TableId;
use tessari_types::{DatabaseId, NamespaceId};

use super::{Lifetime, RecordAddress, Transaction};

/// A copy of what a transaction had done when a `SAVEPOINT` was taken.
#[derive(Debug, Clone)]
pub struct Savepoint {
    writes: BTreeMap<RecordAddress, RecordValue>,
    expiring: BTreeMap<RecordAddress, u64>,
    field_expiring: HashMap<RecordAddress, Vec<(String, u64)>>,
    lifetimes: BTreeMap<RecordAddress, Lifetime>,
    trims: HashMap<(NamespaceId, DatabaseId, TableId), u64>,
    claims: usize,
    across: bool,
}

/// Why the work since a savepoint cannot be rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreturnable {
    /// A queue claim was taken since, and the claim register holds it.
    Claim,
    /// Work was handed to other leaders since.
    Across,
}

impl Transaction<'_> {
    /// What the transaction has done so far, to roll back to later.
    #[must_use]
    pub fn savepoint(&self) -> Savepoint {
        Savepoint {
            writes: self.writes.clone(),
            expiring: self.expiring.clone(),
            field_expiring: self.field_expiring.clone(),
            lifetimes: self.lifetimes.clone(),
            trims: self.trims.clone(),
            claims: self.claiming.borrow().len(),
            across: self.across.is_some(),
        }
    }

    /// Put back what the transaction had done at `savepoint`, or say why the
    /// work since cannot be given back and change nothing.
    ///
    /// # Errors
    ///
    /// [`Unreturnable`] when a queue claim was taken or work went to other
    /// leaders since the savepoint.
    pub fn roll_back_to(&mut self, savepoint: &Savepoint) -> Result<(), Unreturnable> {
        if self.claiming.borrow().len() != savepoint.claims {
            return Err(Unreturnable::Claim);
        }
        if self.across.is_some() != savepoint.across {
            return Err(Unreturnable::Across);
        }
        self.writes.clone_from(&savepoint.writes);
        self.expiring.clone_from(&savepoint.expiring);
        self.field_expiring.clone_from(&savepoint.field_expiring);
        self.lifetimes.clone_from(&savepoint.lifetimes);
        self.trims.clone_from(&savepoint.trims);
        Ok(())
    }
}
