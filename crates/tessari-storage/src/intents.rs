//! Intents: a transaction across leaders' writes, held as provisional versions
//! until its record decides them (ADR-0112 D5).
//!
//! An intent is a version under the record's own key whose provenance says it
//! is provisional. Until the reading rule of ADR-0112 D6 is built, every reader
//! treats one as invisible and reads the version under it, and every writer is
//! refused while one stands — which is what the model checked in
//! `across_model` requires of both sides before anything can prepare.

use tessari_encoding::{
    AcrossPartKey, Decision, IntentOfKey, LogRecord, Part, Provenance, RecordKey, StampedValue,
    StoreKey, StoreValue, TransactionRecord, TransactionRecordKey,
};
use tessari_kv::WriteBatch;
use tessari_types::Sequence;

use crate::error::{Error, Result};
use crate::store::Store;

/// Whether a stored version is an intent rather than a value.
pub(crate) fn is_intent(version: &StampedValue) -> bool {
    version
        .provenance()
        .is_some_and(|provenance: &Provenance| provenance.provisional)
}

impl Store {
    /// The record of one transaction across leaders as this node holds it.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn transaction_record(
        &self,
        transaction: tessari_encoding::TransactionId,
    ) -> Result<Option<TransactionRecord>> {
        let key = TransactionRecordKey { transaction };
        Ok(self
            .backend()
            .get(TransactionRecordKey::keyspace(), &key.encode())?
            .map(|value| TransactionRecord::decode(value.as_slice()))
            .transpose()?)
    }

    /// Every record `transaction` holds an intent on here, from its index.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub(crate) fn intents_of(
        &self,
        transaction: tessari_encoding::TransactionId,
    ) -> Result<Vec<crate::transaction::RecordAddress>> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: IntentOfKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&IntentOfKey::prefix_of(transaction)),
            direction: tessari_kv::ScanDirection::Forward,
            limit: None,
        })?;
        found
            .into_iter()
            .map(|(key, _)| {
                let held = IntentOfKey::decode(key.as_slice())?;
                Ok(crate::transaction::RecordAddress::new(
                    held.namespace,
                    held.database,
                    held.table,
                    held.id,
                ))
            })
            .collect()
    }

    /// Every transaction across leaders with an intent standing here, and the
    /// range its record lives in — read off one of its intents, which every
    /// prepare stamped with it.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn standing_across(
        &self,
    ) -> Result<Vec<(tessari_encoding::TransactionId, tessari_types::Reach)>> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: IntentOfKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&IntentOfKey::prefix()),
            direction: tessari_kv::ScanDirection::Forward,
            limit: None,
        })?;
        let mut standing: Vec<(tessari_encoding::TransactionId, tessari_types::Reach)> = Vec::new();
        for (key, version) in found {
            let held = IntentOfKey::decode(key.as_slice())?;
            if standing
                .last()
                .is_some_and(|(seen, _)| *seen == held.transaction)
            {
                continue;
            }
            let intent = RecordKey::new(
                held.namespace,
                held.database,
                held.table,
                held.id,
                Sequence::decode(version.as_slice())?,
            );
            let Some(stored) = self
                .backend()
                .get(RecordKey::keyspace(), &intent.encode())?
            else {
                continue;
            };
            if let Some(provenance) = StampedValue::decode(stored.as_slice())?.provenance() {
                standing.push((held.transaction, provenance.coordinator));
            }
        }
        Ok(standing)
    }

    /// Every transaction record this node holds that is still `PENDING`.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn pending_across(
        &self,
    ) -> Result<Vec<(tessari_encoding::TransactionId, TransactionRecord)>> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: TransactionRecordKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&[
                tessari_encoding::KeyKind::TransactionRecord.tag()
            ]),
            direction: tessari_kv::ScanDirection::Forward,
            limit: None,
        })?;
        let mut pending = Vec::new();
        for (key, value) in found {
            let record = TransactionRecord::decode(value.as_slice())?;
            if record.decision == Decision::Pending {
                pending.push((
                    TransactionRecordKey::decode(key.as_slice())?.transaction,
                    record,
                ));
            }
        }
        Ok(pending)
    }
}

/// Whether applying `record` writes its mutations as versions.
///
/// A decision carries none, and an aborted resolution names the records whose
/// intents it drops without giving them a value.
pub(crate) fn writes_versions(record: &LogRecord) -> bool {
    !matches!(
        record.part_of().map(|across| &across.part),
        Some(Part::Decide(_) | Part::Resolve { committed: false })
    )
}

/// Whether applying `record` derives nothing from its mutations — no index
/// entry, count, adjacency, expiry, bound or topic position.
///
/// An intent is not a value yet, so nothing may be derived from it: an index
/// entry for a prepared write would answer a read the record has not decided,
/// and the read that skips its re-test would believe it. Only the committed
/// resolution derives, as an ordinary commit does.
pub(crate) fn derives_nothing(record: &LogRecord) -> bool {
    matches!(
        record.part_of().map(|across| &across.part),
        Some(Part::Prepare { .. } | Part::Decide(_) | Part::Resolve { committed: false })
    )
}

/// Add to `batch` what a record of a transaction across leaders does beyond its
/// versions: a decision writes the transaction record, a resolution deletes
/// the intents it replaces. Every record of one is checked against what it
/// claims to be, here, where leader and follower both apply it.
///
/// # Errors
///
/// [`Error::AcrossMalformed`] for a record that contradicts itself, and
/// whatever the backend or the codec returns.
pub(crate) fn settle(
    store: &Store,
    record: &LogRecord,
    batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    let Some(across) = record.part_of() else {
        return Ok(batch);
    };
    let carries = |provisional: bool| {
        record.mutations().iter().all(|mutation| {
            mutation.value.provenance().is_some_and(|provenance| {
                provenance.transaction == across.transaction
                    && provenance.provisional == provisional
                    // A resolved version says where every prepare landed (D6a).
                    && (provisional || !provenance.participants.is_empty())
            })
        })
    };
    match &across.part {
        Part::Prepare { .. } => {
            if !carries(true) {
                return Err(Error::AcrossMalformed {
                    part: "prepare",
                    problem: "a write that is not an intent of its transaction",
                });
            }
            // Where this part landed here, in the batch that lands it (D6a): a
            // reader's snapshot at or past `version` holds every intent below.
            let part = AcrossPartKey {
                transaction: across.transaction,
                range: crate::catalog::home_of(record)?,
            };
            let batch = batch.put(AcrossPartKey::keyspace(), part.encode(), version.encode());
            // Each intent indexed under its transaction, in the batch that
            // lands it (D7): what lets this node resolve its own intents later.
            Ok(record.mutations().iter().fold(batch, |batch, mutation| {
                batch.put(
                    IntentOfKey::keyspace(),
                    intent_of(across.transaction, mutation).encode(),
                    version.encode(),
                )
            }))
        }
        Part::Decide(decided) => {
            if !record.mutations().is_empty() {
                return Err(Error::AcrossMalformed {
                    part: "decide",
                    problem: "a decision that carries writes",
                });
            }
            let key = TransactionRecordKey {
                transaction: across.transaction,
            };
            // Compare-and-set on the record as it stands (D4, D7): a first
            // record is `PENDING`, a pending one may be renewed or decided, and
            // a decided one never changes — so a coordinator and a lapse racing
            // to decide record one outcome, and the loser is told it.
            let standing = store
                .backend()
                .get(TransactionRecordKey::keyspace(), &key.encode())?
                .map(|value| TransactionRecord::decode(value.as_slice()))
                .transpose()?;
            // Absent may become ABORTED as well as PENDING: a record a lapse
            // finds absent is aborted for good, so a PENDING delayed past the
            // deadline is refused rather than reopening it.
            let refused = match standing.as_ref().map(|record| record.decision) {
                None => (decided.decision == Decision::Committed).then_some("absent"),
                Some(Decision::Pending) => None,
                Some(Decision::Committed) => {
                    (decided.decision != Decision::Committed).then_some("committed")
                }
                Some(Decision::Aborted) => {
                    (decided.decision != Decision::Aborted).then_some("aborted")
                }
            };
            if let Some(decided) = refused {
                return Err(Error::AcrossDecided { decided });
            }
            Ok(batch.put(
                TransactionRecordKey::keyspace(),
                key.encode(),
                decided.encode(),
            ))
        }
        Part::Resolve { committed } => {
            if *committed && !carries(false) {
                return Err(Error::AcrossMalformed {
                    part: "resolve",
                    problem: "a committed value that is not its transaction's",
                });
            }
            let mut batch = batch;
            for mutation in record.mutations() {
                // The index names the intent's version, so the intent is
                // deleted by its own key; a node that never held it — a copy
                // seeded past the prepare — finds no index entry and deletes
                // nothing.
                let indexed = intent_of(across.transaction, mutation).encode();
                let Some(held) = store.backend().get(IntentOfKey::keyspace(), &indexed)? else {
                    continue;
                };
                let intent = RecordKey::new(
                    mutation.namespace,
                    mutation.database,
                    mutation.table,
                    mutation.id.clone(),
                    Sequence::decode(held.as_slice())?,
                );
                batch = batch
                    .delete(RecordKey::keyspace(), intent.encode())
                    .delete(IntentOfKey::keyspace(), indexed);
            }
            Ok(batch)
        }
    }
}

/// The index key of one intent.
fn intent_of(
    transaction: tessari_encoding::TransactionId,
    mutation: &tessari_encoding::Mutation,
) -> IntentOfKey {
    IntentOfKey {
        transaction,
        namespace: mutation.namespace,
        database: mutation.database,
        table: mutation.table,
        id: mutation.id.clone(),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod applied;
