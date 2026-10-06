//! Intents: a transaction across leaders' writes, held as provisional versions
//! until its record decides them (ADR-0112 D5).
//!
//! An intent is a version under the record's own key whose provenance says it
//! is provisional. Until the reading rule of ADR-0112 D6 is built, every reader
//! treats one as invisible and reads the version under it, and every writer is
//! refused while one stands — which is what the model checked in
//! `across_model` requires of both sides before anything can prepare.

use tessari_encoding::{
    Across, AcrossBarredKey, AcrossPartKey, Barred, Decision, IntentOfKey, LogId, LogRecord, Part,
    Provenance, RecordKey, StampedValue, StoreKey, StoreValue, TransactionRecord,
    TransactionRecordKey,
};
use tessari_kv::WriteBatch;
use tessari_types::Sequence;

use crate::error::{Error, Result};
use crate::store::Store;

mod folding;
mod restored;
mod standing;
mod unsettled;

/// Whether a stored version is an intent rather than a value.
pub(crate) fn is_intent(version: &StampedValue) -> bool {
    version
        .provenance()
        .is_some_and(|provenance: &Provenance| provenance.provisional)
}

/// Whether applying `record` writes its mutations as versions.
///
/// A decision carries none, and an aborted resolution names the records whose
/// intents it drops without giving them a value.
pub(crate) fn writes_versions(record: &LogRecord) -> bool {
    record
        .part_of()
        .is_none_or(|across| across.part.prepares() || across.part.resolution() == Some(true))
}

/// Whether applying `record` derives nothing from its mutations — no index
/// entry, count, adjacency, expiry, bound or topic position.
///
/// An intent is not a value yet, so nothing may be derived from it: an index
/// entry for a prepared write would answer a read the record has not decided,
/// and the read that skips its re-test would believe it. Only the committed
/// resolution derives, as an ordinary commit does.
pub(crate) fn derives_nothing(record: &LogRecord) -> bool {
    record
        .part_of()
        .is_some_and(|across| across.part.resolution() != Some(true))
}

/// Add to `batch` what a record of a transaction across leaders does beyond its
/// versions: a decision writes the transaction record, a resolution deletes
/// the intents it replaces. Every record of one is checked against what it
/// claims to be, here, where leader and follower both apply it.
///
/// `written` is the log and position the record lands at, where it lands in
/// one: a bar keeps it, to end when that log is pruned past it (ADR-0119).
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
    written: Option<(LogId, Sequence)>,
) -> Result<WriteBatch> {
    let Some(across) = record.part_of() else {
        return Ok(batch);
    };
    match &across.part {
        Part::Prepare { .. } => {
            unbarred(store, across, record)?;
            let batch = prepared(store, across, record, batch, version, "prepare")?;
            unsettled::reconcile(store, across, record, batch)
        }
        Part::Decide(decided) => {
            if !record.mutations().is_empty() {
                return Err(Error::AcrossMalformed {
                    part: "decide",
                    problem: "a decision that carries writes",
                });
            }
            let batch = recorded(store, across, decided, batch)?;
            unsettled::reconcile(store, across, record, batch)
        }
        Part::Resolve { committed } => {
            let batch = resolved(store, across, record, batch, *committed, "resolve")?;
            unsettled::reconcile(store, across, record, batch)
        }
        // D13a: the record first, so a begin that finds it standing — a
        // participant aborted it while this record was on its way — writes
        // no intent either. It stages (D14a): nothing else commits implicitly.
        Part::Begin(begun) => {
            if begun.decision != Decision::Staging {
                return Err(Error::AcrossMalformed {
                    part: "begin",
                    problem: "a record begun other than staging",
                });
            }
            let batch = recorded(store, across, begun, batch)?;
            let batch = prepared(store, across, record, batch, version, "begin")?;
            unsettled::reconcile(store, across, record, batch)
        }
        // D13b: the outcome and this range's resolution, or neither.
        Part::Conclude(decided) => {
            if !decided.decision.is_decided() {
                return Err(Error::AcrossMalformed {
                    part: "conclude",
                    problem: "a record concluded undecided",
                });
            }
            let batch = recorded(store, across, decided, batch)?;
            let committed = decided.decision == Decision::Committed;
            let batch = resolved(store, across, record, batch, committed, "conclude")?;
            unsettled::reconcile(store, across, record, batch)
        }
        Part::Forget { .. } => {
            if !record.mutations().is_empty() {
                return Err(Error::AcrossMalformed {
                    part: "forget",
                    problem: "a forgetting that carries writes",
                });
            }
            // Only a decided record is forgotten (D12): forgetting a PENDING
            // one would let a lapse find it absent and abort a transaction
            // its coordinator is still deciding. An absent one was forgotten
            // already, and forgetting it again changes nothing.
            let key = TransactionRecordKey {
                transaction: across.transaction,
            };
            let standing = store
                .backend()
                .get(TransactionRecordKey::keyspace(), &key.encode())?
                .map(|value| TransactionRecord::decode(value.as_slice()))
                .transpose()?;
            if standing.is_some_and(|record| !record.decision.is_decided()) {
                return Err(Error::AcrossMalformed {
                    part: "forget",
                    problem: "a record that has not decided",
                });
            }
            Ok(batch.delete(TransactionRecordKey::keyspace(), key.encode()))
        }
        Part::Landed { range } => {
            if !record.mutations().is_empty() {
                return Err(Error::AcrossMalformed {
                    part: "landed",
                    problem: "a landed part that carries writes",
                });
            }
            // A part a snapshot held, at the version its restore gives it
            // (D9a): what a reader of the restored store asks, as a reader of
            // the source asked the prepare's own marker.
            let part = AcrossPartKey {
                transaction: across.transaction,
                range: *range,
            };
            let batch = landed(store, &part, batch, version)?;
            unsettled::reconcile(store, across, record, batch)
        }
        // D14c: a part is barred only where its prepare has not landed, and
        // then for good — the prepare meeting the bar is refused.
        Part::Prevent { range } => {
            if !record.mutations().is_empty() {
                return Err(Error::AcrossMalformed {
                    part: "prevent",
                    problem: "a bar that carries writes",
                });
            }
            let part = AcrossPartKey {
                transaction: across.transaction,
                range: *range,
            };
            if store
                .backend()
                .get(AcrossPartKey::keyspace(), &part.encode())?
                .is_some()
            {
                return Err(Error::AcrossDecided {
                    decided: "prepared",
                });
            }
            let barred = AcrossBarredKey {
                transaction: across.transaction,
                range: *range,
            }
            .encode();
            if store
                .backend()
                .get(AcrossBarredKey::keyspace(), &barred)?
                .is_some()
            {
                return Ok(batch);
            }
            Ok(batch.put(
                AcrossBarredKey::keyspace(),
                barred,
                Barred { version, written }.encode(),
            ))
        }
    }
}

/// Refuse a prepare whose part status recovery barred here (ADR-0112 D14c).
fn unbarred(store: &Store, across: &Across, record: &LogRecord) -> Result<()> {
    let barred = AcrossBarredKey {
        transaction: across.transaction,
        range: crate::catalog::home_of(record)?,
    };
    if store
        .backend()
        .get(AcrossBarredKey::keyspace(), &barred.encode())?
        .is_some()
    {
        return Err(Error::AcrossDecided { decided: "barred" });
    }
    Ok(())
}

/// Whether every write of `record` is a version of `across`'s transaction,
/// an intent or a resolved value as `provisional` says.
fn carries(across: &Across, record: &LogRecord, provisional: bool) -> bool {
    record.mutations().iter().all(|mutation| {
        mutation.value.provenance().is_some_and(|provenance| {
            provenance.transaction == across.transaction
                && provenance.provisional == provisional
                // A resolved version says where every prepare landed (D6a).
                && (provisional || !provenance.participants.is_empty())
        })
    })
}

/// Write the transaction record `decided` by compare-and-set on the record as
/// it stands (D4, D7): a first record is `PENDING`, a pending one may be
/// renewed or decided, and a decided one never changes — so a coordinator and
/// a lapse racing to decide record one outcome, and the loser is told it. A
/// begun record must find none at all.
fn recorded(
    store: &Store,
    across: &Across,
    decided: &TransactionRecord,
    batch: WriteBatch,
) -> Result<WriteBatch> {
    let key = TransactionRecordKey {
        transaction: across.transaction,
    };
    let standing = store
        .backend()
        .get(TransactionRecordKey::keyspace(), &key.encode())?
        .map(|value| TransactionRecord::decode(value.as_slice()))
        .transpose()?;
    // Absent may become ABORTED as well as PENDING: a record a lapse finds
    // absent is aborted for good, so a PENDING delayed past the deadline is
    // refused rather than reopening it.
    let refused = match standing.as_ref().map(|record| record.decision) {
        None => (decided.decision == Decision::Committed).then_some("absent"),
        Some(Decision::Pending) if matches!(across.part, Part::Begin(_)) => Some("pending"),
        // A staging record may be committed implicitly already (D14a): it is
        // decided, or written again as it stands, and never reopened.
        Some(Decision::Staging)
            if matches!(across.part, Part::Begin(_)) || decided.decision == Decision::Pending =>
        {
            Some("staging")
        }
        Some(Decision::Pending | Decision::Staging) => None,
        Some(Decision::Committed) => {
            (decided.decision != Decision::Committed).then_some("committed")
        }
        Some(Decision::Aborted) => (decided.decision != Decision::Aborted).then_some("aborted"),
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

/// Hold `record`'s writes as intents: where the part landed and each intent's
/// index entry, in the batch that lands them.
fn prepared(
    store: &Store,
    across: &Across,
    record: &LogRecord,
    batch: WriteBatch,
    version: Sequence,
    part: &'static str,
) -> Result<WriteBatch> {
    if !carries(across, record, true) {
        return Err(Error::AcrossMalformed {
            part,
            problem: "a write that is not an intent of its transaction",
        });
    }
    // Where this part landed here, in the batch that lands it (D6a): a
    // reader's snapshot at or past `version` holds every intent below.
    let landing = AcrossPartKey {
        transaction: across.transaction,
        range: crate::catalog::home_of(record)?,
    };
    let batch = landed(store, &landing, batch, version)?;
    // Each intent indexed under its transaction, in the batch that lands it
    // (D7): what lets this node resolve its own intents later. A state copied
    // onto a store that held it (D9a) lands the intent again, above the record
    // the copy rewrote; the index names the new one, and the old stays as
    // history a running snapshot may read.
    Ok(record.mutations().iter().fold(batch, |batch, mutation| {
        batch.put(
            IntentOfKey::keyspace(),
            intent_of(across.transaction, mutation).encode(),
            version.encode(),
        )
    }))
}

/// Delete the intents `record` resolves; a committed resolution's values are
/// the versions the record itself writes.
fn resolved(
    store: &Store,
    across: &Across,
    record: &LogRecord,
    mut batch: WriteBatch,
    committed: bool,
    part: &'static str,
) -> Result<WriteBatch> {
    if committed && !carries(across, record, false) {
        return Err(Error::AcrossMalformed {
            part,
            problem: "a committed value that is not its transaction's",
        });
    }
    for mutation in record.mutations() {
        // The index names the intent's version, so the intent is deleted by
        // its own key; a node that never held it — a copy seeded past the
        // prepare — finds no index entry and deletes nothing.
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

/// Mark `part` landed at `version`, unless it is marked already: the marker is
/// where the part first landed here, and moving it up — a state copied onto a
/// store that held it (D9a) — would hide the transaction from a snapshot that
/// already saw it.
fn landed(
    store: &Store,
    part: &AcrossPartKey,
    batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    let key = part.encode();
    if store
        .backend()
        .get(AcrossPartKey::keyspace(), &key)?
        .is_some()
    {
        return Ok(batch);
    }
    Ok(batch.put(AcrossPartKey::keyspace(), key, version.encode()))
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
