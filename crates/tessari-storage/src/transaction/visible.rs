//! Whether a reading transaction sees a transaction across leaders (ADR-0112
//! D6a): whole, or not at all, decided once.
//!
//! # A function of the snapshot
//!
//! T is visible when this snapshot knows T committed — it holds a resolved
//! version of T, or T's record here says so — and holds T's part in every range
//! T wrote that this node holds: each part's [`AcrossPartKey`] landed at a local
//! version at or below the snapshot. One snapshot gives one answer, so every
//! read of T in a transaction agrees without asking anybody, and a node whose
//! copies lag shows T later rather than in part. Ranges this node does not hold
//! are left out: a read of them is gathered elsewhere, complete but not one
//! snapshot, as every gathered read already is.
//!
//! # Decided once
//!
//! The answer is kept for the transaction's life. T's record is node state
//! that moves under a running transaction — `PENDING` when a read first meets an
//! intent, `COMMITTED` a moment later — and a second look would show T to the
//! second read after hiding it from the first.

use tessari_encoding::{
    AcrossPartKey, Decision, Participant, Provenance, StampedValue, StoreKey, StoreValue,
};
use tessari_types::Sequence;

use super::Transaction;
use crate::error::Result;

impl Transaction<'_> {
    /// Whether a reader passes over `stored` for the version under it: an
    /// intent of a transaction this one does not see, or a version resolved
    /// from one.
    pub(crate) fn passes_over(&self, stored: &StampedValue) -> Result<bool> {
        match stored.provenance() {
            None => Ok(false),
            Some(provenance) => Ok(!self.sees(provenance)?),
        }
    }

    /// Whether this transaction sees the transaction `provenance` names.
    pub(super) fn sees(&self, provenance: &Provenance) -> Result<bool> {
        if let Some(seen) = self.decided.borrow().get(&provenance.transaction) {
            return Ok(*seen);
        }
        let seen = if provenance.provisional {
            // An intent is a value only once its record says so, and the
            // record is where its participants are written down.
            match self.store.transaction_record(provenance.transaction)? {
                Some(record) if record.decision == Decision::Committed => {
                    self.holds_every_part(provenance, &record.participants)?
                }
                _ => false,
            }
        } else {
            // A resolved version exists only after the decision, and carries
            // the participants itself.
            self.holds_every_part(provenance, &provenance.participants)?
        };
        self.decided
            .borrow_mut()
            .insert(provenance.transaction, seen);
        Ok(seen)
    }

    /// Whether this snapshot holds the transaction's part in every range of
    /// `participants` this node holds.
    fn holds_every_part(
        &self,
        provenance: &Provenance,
        participants: &[Participant],
    ) -> Result<bool> {
        let served = self.store.served();
        for participant in participants {
            if served.is_some_and(|over| !over.contains(participant.range)) {
                continue;
            }
            let part = AcrossPartKey {
                transaction: provenance.transaction,
                range: participant.range,
            };
            let landed = self
                .store
                .backend()
                .get(AcrossPartKey::keyspace(), &part.encode())?
                .map(|at| Sequence::decode(at.as_slice()))
                .transpose()?;
            if !landed.is_some_and(|at| at <= self.snapshot) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod fixture;

#[cfg(test)]
mod forgetting;

#[cfg(test)]
mod indexes;

#[cfg(test)]
mod restoring;

#[cfg(test)]
mod tests;
