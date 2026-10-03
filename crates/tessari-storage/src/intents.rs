//! Intents: a transaction across leaders' writes, held as provisional versions
//! until its record decides them (ADR-0112 D5).
//!
//! An intent is a version under the record's own key whose provenance says it
//! is provisional. Until the reading rule of ADR-0112 D6 is built, every reader
//! treats one as invisible and reads the version under it, and every writer is
//! refused while one stands — which is what the model checked in
//! `across_model` requires of both sides before anything can prepare.

use tessari_encoding::{
    LogRecord, Part, Provenance, RecordKey, StampedValue, StoreKey, StoreValue,
    TransactionRecordKey,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};

use crate::error::{Error, Result};
use crate::store::Store;

/// Whether a stored version is an intent rather than a value.
pub(crate) fn is_intent(version: &StampedValue) -> bool {
    version
        .provenance()
        .is_some_and(|provenance: Provenance| provenance.provisional)
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
pub(crate) fn settle(store: &Store, record: &LogRecord, batch: WriteBatch) -> Result<WriteBatch> {
    let Some(across) = record.part_of() else {
        return Ok(batch);
    };
    let carries = |provisional: bool| {
        record.mutations().iter().all(|mutation| {
            mutation.value.provenance().is_some_and(|provenance| {
                provenance.transaction == across.transaction
                    && provenance.provisional == provisional
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
            Ok(batch)
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
                // The intent is its record's newest version — a standing intent
                // refuses every other write — so one read finds it, or finds
                // that this node never held it.
                let prefix = RecordKey::versions_prefix(
                    mutation.namespace,
                    mutation.database,
                    mutation.table,
                    &mutation.id,
                );
                let found = store.backend().scan(&ScanRequest {
                    keyspace: RecordKey::keyspace(),
                    range: KeyRange::prefix(&prefix),
                    direction: ScanDirection::Forward,
                    limit: Some(1),
                })?;
                for (key, value) in found {
                    let stored = StampedValue::decode(value.as_slice())?;
                    let ours = stored.provenance().is_some_and(|provenance| {
                        provenance.provisional && provenance.transaction == across.transaction
                    });
                    if ours {
                        batch = batch.delete(RecordKey::keyspace(), key);
                    }
                }
            }
            Ok(batch)
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod applied;
