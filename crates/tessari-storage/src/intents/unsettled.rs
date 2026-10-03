//! Tables whose indexes and readers disagree about a transaction across
//! leaders that is part-way on this node (Q-919).
//!
//! # Why a mark, and when
//!
//! An index entry carries no version and is derived only by a committed
//! resolution, while a reader sees a committed transaction only once this node
//! holds every part of it (ADR-0112 D6a). Two states fall between them: an
//! intent readers already see, which no index holds, and a resolution this
//! node holds while a part is still missing, which the index holds and readers
//! do not see. A read that believed the index in either would answer wrongly
//! with nothing in an error state, so such a table is marked and an index read
//! of it takes the checked path.
//!
//! One rule decides the mark, recomputed whenever a record of the transaction
//! lands rather than spelled as a case per record: a table is marked while the
//! transaction is known committed here, touched the table here, and either has
//! an intent standing in it or has a part this node holds that has not landed.
//! It over-marks where readers and the index happen to agree — an intent
//! nobody sees while a part is missing — which costs that table its index
//! shortcut for as long as the transaction takes to arrive, and nothing else.

use std::collections::BTreeSet;

use tessari_encoding::{
    Across, AcrossPartKey, AcrossUnsettledKey, Decision, IntentOfKey, LogRecord, Part, Participant,
    StoreKey, StoreValue, TransactionId, TransactionRecord,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::TableId;

use crate::error::Result;
use crate::store::Store;

impl Store {
    /// Whether a transaction across leaders is part-way in this table here, so
    /// that its indexes and its readers disagree about it.
    ///
    /// # Errors
    ///
    /// Whatever the backend returns.
    pub fn across_unsettled(&self, table: TableId) -> Result<bool> {
        let found = self.backend().scan(&ScanRequest {
            keyspace: AcrossUnsettledKey::keyspace(),
            range: KeyRange::prefix(&AcrossUnsettledKey::prefix_of(table)),
            direction: ScanDirection::Forward,
            limit: Some(1),
        })?;
        Ok(!found.is_empty())
    }
}

/// Bring the marks of `across`'s transaction up to date with `record`, which
/// `batch` lands: the store as it stands plus what this batch changes.
///
/// # Errors
///
/// Whatever the backend or the codec returns.
pub(super) fn reconcile(
    store: &Store,
    across: &Across,
    record: &LogRecord,
    batch: WriteBatch,
) -> Result<WriteBatch> {
    let transaction = across.transaction;
    let resolved_committed = matches!(across.part, Part::Resolve { committed: true });
    // Only a record that can make the transaction known committed, add an
    // intent, land a part or remove an intent moves a mark.
    let decided = match &across.part {
        Part::Decide(decided) if decided.decision != Decision::Committed => return Ok(batch),
        Part::Resolve { committed: false } | Part::Forget { .. } => return Ok(batch),
        Part::Decide(decided) => Some(decided.clone()),
        Part::Prepare { .. } | Part::Resolve { .. } | Part::Landed { .. } => store
            .transaction_record(transaction)?
            .filter(|standing| standing.decision == Decision::Committed),
    };
    let marked = marks_of(store, transaction)?;
    // Committed as far as this node can tell: its record here says so, this is
    // its committed resolution, or an earlier one left a mark.
    let committed = decided.or_else(|| {
        record
            .mutations()
            .iter()
            .find_map(|mutation| mutation.value.provenance())
            .filter(|_| resolved_committed)
            .map(|provenance| committed_with(provenance.participants.clone()))
            .or_else(|| marked.first().map(|(_, standing)| standing.clone()))
    });
    let Some(committed) = committed else {
        return Ok(batch);
    };

    let landing = match across.part {
        Part::Prepare { .. } | Part::Landed { .. } => Some(crate::catalog::home_of(record)?),
        _ => None,
    };
    let missing = part_missing(store, transaction, &committed.participants, landing)?;
    let mut target = intent_tables(store, across, record)?;
    if missing {
        // What the index already holds stays ahead of the readers until the
        // missing part lands: the tables marked before, and this resolution's.
        target.extend(marked.iter().map(|(table, _)| *table));
        if resolved_committed {
            target.extend(record.mutations().iter().map(|mutation| mutation.table));
        }
    }

    let key = |table: TableId| AcrossUnsettledKey { table, transaction };
    let batch = marked
        .iter()
        .filter(|(table, _)| !target.contains(table))
        .fold(batch, |batch, (table, _)| {
            batch.delete(AcrossUnsettledKey::keyspace(), key(*table).encode())
        });
    let value = committed.encode();
    Ok(target.into_iter().fold(batch, |batch, table| {
        batch.put(
            AcrossUnsettledKey::keyspace(),
            key(table).encode(),
            value.clone(),
        )
    }))
}

/// The record of a transaction known committed only from its resolved
/// versions, which carry its participants.
fn committed_with(participants: Vec<Participant>) -> TransactionRecord {
    TransactionRecord {
        decision: Decision::Committed,
        deadline: 0,
        participants,
    }
}

/// Every table `transaction` is marked in here, with the record each mark
/// holds. A walk of the whole kind: it holds only transactions in flight.
fn marks_of(
    store: &Store,
    transaction: TransactionId,
) -> Result<Vec<(TableId, TransactionRecord)>> {
    let found = store.backend().scan(&ScanRequest {
        keyspace: AcrossUnsettledKey::keyspace(),
        range: KeyRange::prefix(&AcrossUnsettledKey::prefix()),
        direction: ScanDirection::Forward,
        limit: None,
    })?;
    let mut marked = Vec::new();
    for (key, value) in found {
        let held = AcrossUnsettledKey::decode(key.as_slice())?;
        if held.transaction == transaction {
            marked.push((held.table, TransactionRecord::decode(value.as_slice())?));
        }
    }
    Ok(marked)
}

/// Whether a range of `participants` this node holds has no part of the
/// transaction landed here, counting the one `landing` lands in this batch —
/// the same ranges a reader asks about (D6a).
fn part_missing(
    store: &Store,
    transaction: TransactionId,
    participants: &[Participant],
    landing: Option<tessari_types::Reach>,
) -> Result<bool> {
    let served = store.served();
    for participant in participants {
        if served.is_some_and(|over| !over.contains(participant.range))
            || landing == Some(participant.range)
        {
            continue;
        }
        let part = AcrossPartKey {
            transaction,
            range: participant.range,
        };
        if store
            .backend()
            .get(AcrossPartKey::keyspace(), &part.encode())?
            .is_none()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The tables with an intent of the transaction standing once `record`
/// lands: those indexed here, less the ones a resolution removes, plus the
/// ones a prepare adds.
fn intent_tables(store: &Store, across: &Across, record: &LogRecord) -> Result<BTreeSet<TableId>> {
    let mut standing: BTreeSet<Vec<u8>> = store
        .backend()
        .scan(&ScanRequest {
            keyspace: IntentOfKey::keyspace(),
            range: KeyRange::prefix(&IntentOfKey::prefix_of(across.transaction)),
            direction: ScanDirection::Forward,
            limit: None,
        })?
        .into_iter()
        .map(|(key, _)| key.as_slice().to_vec())
        .collect();
    for mutation in record.mutations() {
        let intent = super::intent_of(across.transaction, mutation)
            .encode()
            .as_slice()
            .to_vec();
        match across.part {
            Part::Prepare { .. } => {
                standing.insert(intent);
            }
            Part::Resolve { .. } => {
                standing.remove(&intent);
            }
            Part::Decide(_) | Part::Forget { .. } | Part::Landed { .. } => {}
        }
    }
    standing
        .iter()
        .map(|key| Ok(IntentOfKey::decode(key)?.table))
        .collect()
}
