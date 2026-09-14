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
//! Applying a record at log position `S` writes, together:
//!
//! - the log record itself, at `S`;
//! - every mutation it carries, as a record version at `V`;
//! - the applied position, advanced to `S`;
//! - the version position, advanced to `V`.
//!
//! Split across two batches, a crash in between is indistinguishable from
//! success, and recovery either applies a record twice or skips it forever.
//!
//! # Why the position and the version are two numbers
//!
//! They held the same value for as long as one leader decided every write, and
//! that is the only reason they were ever one. `S` is the log's — replicated,
//! compared between nodes, resumed from. `V` is this store's own — it orders
//! this store's records against each other and against the snapshot a reader
//! holds, and no other node reads it.
//!
//! Once two leaders allocate log positions from independent per-range counters,
//! one number cannot be both: a transaction opened at a store-wide "5" would
//! read one range as of its fifth record and another as of its fifth, which is
//! two unrelated moments presented as one, with no error and plausible data.
//! Nothing in the criterion that asks for a per-range sequence would catch it,
//! because that validation reads one range at a time (Q-614).
//!
//! # Why applying is guarded by the position it moves
//!
//! The batch asserts that the applied position is `S - 1` before it becomes `S`,
//! and that the version position is `V - 1` before it becomes `V`. The first
//! carries three separate guarantees, which is why it is not merely a safety
//! check:
//!
//! - a record cannot be applied out of order,
//! - a record cannot be applied twice,
//! - and a concurrent commit that claimed `S` first loses, because its own
//!   assertion about the position is what the winner invalidated.
//!
//! The second guards the version counter on its own terms rather than borrowing
//! the first one's serialization. Today that is the same protection twice, since
//! one log position admits one writer. It stops being so the moment positions
//! become per-range: two writers on disjoint ranges would then both allocate
//! `V + 1` and one would silently overwrite the other's version. A counter whose
//! safety depends on the number it was just separated from is not separated.
//!
//! # Commit is the degenerate case of replay
//!
//! A commit decides `S` locally — one node, no quorum — and then applies. A
//! replica reads a record whose `S` was decided elsewhere and applies. Both call
//! the function below. That is what keeps "single node today is the same design"
//! from being a slogan: there is no second write path that could drift.

use tessari_encoding::{
    AppliedPositionKey, LogKey, LogRecord, RecordKey, StoreKey, StoreValue, VersionPositionKey,
};
use tessari_kv::WriteBatch;
use tessari_types::Sequence;

/// The batch that applies one log record.
///
/// `at` is the record's position in the log — the replicated fact. `version` is
/// the version every mutation is written at — this store's own fact. See the
/// module header for why they are two parameters and not one.
pub(crate) fn apply_batch(at: Sequence, version: Sequence, record: &LogRecord) -> WriteBatch {
    let applied_key = AppliedPositionKey.encode();
    let version_key = VersionPositionKey.encode();
    let previous_position = Sequence::new(at.get().saturating_sub(1));
    let previous_version = Sequence::new(version.get().saturating_sub(1));

    let mut batch = WriteBatch::new()
        .expect_value(
            AppliedPositionKey::keyspace(),
            applied_key.clone(),
            previous_position.encode(),
        )
        .put(AppliedPositionKey::keyspace(), applied_key, at.encode())
        .expect_value(
            VersionPositionKey::keyspace(),
            version_key.clone(),
            previous_version.encode(),
        )
        .put(
            VersionPositionKey::keyspace(),
            version_key,
            version.encode(),
        )
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
            version,
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
    fn applying_asserts_both_the_position_and_the_version_it_moves() {
        let batch = apply_batch(Sequence::new(7), Sequence::new(4), &record());
        let expected: Vec<Vec<u8>> = batch
            .preconditions()
            .iter()
            .map(|precondition| match precondition {
                Precondition::ValueIs { expected, .. } => expected.as_slice().to_vec(),
                other => panic!("expected a value assertion, got {other:?}"),
            })
            .collect();
        assert_eq!(expected.len(), 2, "one guard each, position and version");
        assert!(
            expected.contains(&Sequence::new(6).encode().as_slice().to_vec()),
            "the position it is about to move"
        );
        assert!(
            expected.contains(&Sequence::new(3).encode().as_slice().to_vec()),
            "the version it is about to move — guarded on its own terms"
        );
    }

    #[test]
    fn the_first_record_asserts_a_store_that_has_applied_nothing() {
        let batch = apply_batch(Sequence::new(1), Sequence::new(1), &record());
        for precondition in batch.preconditions() {
            match precondition {
                Precondition::ValueIs { expected, .. } => {
                    assert_eq!(expected, &Sequence::ZERO.encode());
                }
                other => panic!("expected a value assertion, got {other:?}"),
            }
        }
    }

    #[test]
    fn one_batch_carries_the_log_record_the_versions_and_the_position() {
        let batch = apply_batch(Sequence::new(3), Sequence::new(3), &record());
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
    fn every_mutation_is_written_at_the_version_and_not_at_the_log_position() {
        // The two are given deliberately different values, because equal ones
        // were what hid the conflation for as long as it existed: while one
        // leader decided every write the log position and the record version
        // were the same number, so a test that passed either would pass.
        let at = Sequence::new(11);
        let version = Sequence::new(4);
        let batch = apply_batch(at, version, &record());
        let versions: Vec<Sequence> = batch
            .ops()
            .iter()
            .filter(|op| op.keyspace() == Keyspace::DATA)
            .map(|op| RecordKey::decode(op.key().as_slice()).unwrap().version)
            .collect();
        assert_eq!(versions, vec![version], "the record version, not the log");
        assert!(!versions.contains(&at), "the log position is not a version");
    }

    #[test]
    fn a_record_carrying_nothing_still_advances_the_position() {
        // Not something a commit produces — an empty transaction never reaches
        // the log — but a replica must be able to apply whatever it is sent.
        let batch = apply_batch(
            Sequence::new(2),
            Sequence::new(2),
            &LogRecord::new(Vec::new()),
        );
        assert!(batch.ops().iter().any(|op| op.keyspace() == Keyspace::META));
    }
}
