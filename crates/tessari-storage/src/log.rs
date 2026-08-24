//! The ordered log, and the one path by which state changes.
//!
//! State is a deterministic function of the log. That is not a description of
//! how this store happens to be built — it is the property everything later
//! depends on, because a replication protocol replicates a log and nothing else.
//! If the write path mutated state directly and a log were kept beside it, then
//! at the moment replication arrives every one of an ordered gap-free sequence, a
//! replayable record format, a deterministic apply and a crash-safe applied
//! position would have to be retrofitted through a store that already holds
//! data.
//!
//! # One batch, and why it is one
//!
//! Applying a record at sequence `S` writes, together:
//!
//! - the log record itself, at `S`;
//! - every mutation it carries, as a record version at `S`;
//! - the applied position, advanced to `S`.
//!
//! Split across two batches, a crash in between is indistinguishable from
//! success, and recovery either applies a record twice or skips it forever.
//!
//! # Why applying is guarded by the position it moves
//!
//! The batch asserts that the applied position is `S - 1` before it becomes `S`.
//! That single precondition carries three separate guarantees, which is why it is
//! not merely a safety check:
//!
//! - a record cannot be applied out of order,
//! - a record cannot be applied twice,
//! - and a concurrent commit that claimed `S` first loses, because its own
//!   assertion about the position is what the winner invalidated.
//!
//! # Commit is the degenerate case of replay
//!
//! A commit decides `S` locally — one node, no quorum — and then applies. A
//! replica reads a record whose `S` was decided elsewhere and applies. Both call
//! the function below. That is what keeps "single node today is the same design"
//! from being a slogan: there is no second write path that could drift.

use tessari_encoding::{AppliedPositionKey, LogKey, LogRecord, RecordKey, StoreKey, StoreValue};
use tessari_kv::WriteBatch;
use tessari_types::Sequence;

/// The batch that applies one log record.
///
/// `at` is both the record's position in the log and the version every mutation
/// is written at — one ordering authority, expressed by there being one
/// parameter rather than two.
pub(crate) fn apply_batch(at: Sequence, record: &LogRecord) -> WriteBatch {
    let applied_key = AppliedPositionKey.encode();
    let previous = Sequence::new(at.get().saturating_sub(1));

    let mut batch = WriteBatch::new()
        .expect_value(
            AppliedPositionKey::keyspace(),
            applied_key.clone(),
            previous.encode(),
        )
        .put(AppliedPositionKey::keyspace(), applied_key, at.encode())
        .put(
            LogKey::keyspace(),
            LogKey::new(at).encode(),
            record.encode(),
        );

    for mutation in record.mutations() {
        let key = RecordKey::new(
            mutation.namespace,
            mutation.database,
            mutation.table,
            mutation.id.clone(),
            at,
        );
        batch = batch.put(RecordKey::keyspace(), key.encode(), mutation.value.encode());
    }
    batch
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_encoding::{Mutation, RecordValue};
    use tessari_kv::{Keyspace, Precondition};
    use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

    use super::*;

    fn record() -> LogRecord {
        LogRecord::new(vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("r"),
            value: RecordValue::Present(b"v".to_vec()),
        }])
    }

    #[test]
    fn applying_at_a_sequence_asserts_the_position_it_is_about_to_move() {
        let batch = apply_batch(Sequence::new(7), &record());
        let preconditions = batch.preconditions();
        assert_eq!(preconditions.len(), 1, "exactly one guard, on the position");
        match &preconditions[0] {
            Precondition::ValueIs { expected, .. } => {
                assert_eq!(expected, &Sequence::new(6).encode());
            }
            other => panic!("expected a value assertion, got {other:?}"),
        }
    }

    #[test]
    fn the_first_record_asserts_a_store_that_has_applied_nothing() {
        let batch = apply_batch(Sequence::new(1), &record());
        match &batch.preconditions()[0] {
            Precondition::ValueIs { expected, .. } => {
                assert_eq!(expected, &Sequence::ZERO.encode());
            }
            other => panic!("expected a value assertion, got {other:?}"),
        }
    }

    #[test]
    fn one_batch_carries_the_log_record_the_versions_and_the_position() {
        let batch = apply_batch(Sequence::new(3), &record());
        let keyspaces: Vec<Keyspace> = batch
            .ops()
            .iter()
            .map(tessari_kv::WriteOp::keyspace)
            .collect();
        assert!(keyspaces.contains(&Keyspace::META), "the applied position");
        assert!(keyspaces.contains(&Keyspace::LOG), "the log record");
        assert!(keyspaces.contains(&Keyspace::DATA), "the record version");
    }

    #[test]
    fn every_mutation_is_written_at_the_records_own_sequence() {
        let at = Sequence::new(11);
        let batch = apply_batch(at, &record());
        let versions: Vec<Sequence> = batch
            .ops()
            .iter()
            .filter(|op| op.keyspace() == Keyspace::DATA)
            .map(|op| RecordKey::decode(op.key().as_slice()).unwrap().version)
            .collect();
        assert_eq!(versions, vec![at], "the log sequence is the MVCC version");
    }

    #[test]
    fn a_record_carrying_nothing_still_advances_the_position() {
        // Not something a commit produces — an empty transaction never reaches
        // the log — but a replica must be able to apply whatever it is sent.
        let batch = apply_batch(Sequence::new(2), &LogRecord::new(Vec::new()));
        assert!(batch.ops().iter().any(|op| op.keyspace() == Keyspace::META));
    }
}
