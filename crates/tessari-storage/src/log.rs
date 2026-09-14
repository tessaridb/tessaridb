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
//! Applying a record homed at `H`, at log position `S`, writes, together:
//!
//! - the log record itself, at `S` in `H`'s log;
//! - every mutation it carries, as a record version at `V`;
//! - `H`'s applied position, advanced to `S`;
//! - the version position, advanced to `V`.
//!
//! Split across two batches, a crash in between is indistinguishable from
//! success, and recovery either applies a record twice or skips it forever.
//!
//! # Why the record names a log rather than the log
//!
//! `H` is the join, over the record's mutations, of where each one is carried
//! (`crate::catalog::home_of`, Q-620). A record touching one database homes
//! there; one touching two databases of a namespace homes at that namespace;
//! one carrying anything every node receives homes at the store. The partition
//! therefore follows the **replication filter** and not the admission gate — if
//! it followed the gate, a follower would never learn that its own namespace
//! exists, because the row that defines a namespace is written at the system
//! address.
//!
//! Each home counts from its own counter, which is what lets two leaders on
//! disjoint ranges both allocate a next position without agreeing on anything.
//! Nothing compares a position in one home against a position in another; a
//! reader that did would be comparing two unrelated counters.
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
use tessari_types::{Reach, Sequence};

/// The batch that applies one log record.
///
/// `home` is the log the record belongs to — the reach the partition function
/// gave it (`crate::catalog::home_of`). `at` is the record's position **in that
/// log** — the replicated fact. `version` is the version every mutation is
/// written at — this store's own fact, store-wide and deliberately not
/// partitioned. See the module header for why the last two are two parameters
/// and not one, and why the first joined them.
pub(crate) fn apply_batch(
    home: Reach,
    at: Sequence,
    version: Sequence,
    record: &LogRecord,
) -> WriteBatch {
    let applied_key = AppliedPositionKey::new(home).encode();
    let version_key = VersionPositionKey.encode();
    let previous_position = Sequence::new(at.get().saturating_sub(1));
    let previous_version = Sequence::new(version.get().saturating_sub(1));

    // A home's first record finds no position key at all, because a home does
    // not exist until something is written into it — there is no list of homes
    // to seed at open, and inventing one would mean deciding in advance which
    // databases a store will ever hold. `Absent` is the same guarantee
    // `expect_value` gives one record later: two writers racing for position 1
    // both assert it, and exactly one wins. Asserting `ZERO` instead would
    // refuse every first record, because an absent key does not satisfy a value
    // assertion.
    let first_in_this_home = at == Sequence::new(1);
    let mut batch = if first_in_this_home {
        WriteBatch::new().expect_absent(AppliedPositionKey::keyspace(), applied_key.clone())
    } else {
        WriteBatch::new().expect_value(
            AppliedPositionKey::keyspace(),
            applied_key.clone(),
            previous_position.encode(),
        )
    }
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
        LogKey::new(home, at).encode(),
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

    /// The home every record in these tests belongs to — the database its one
    /// mutation is written in, which is what `home_of` answers for it.
    const HOME: Reach = Reach::Database(NamespaceId::new(1), DatabaseId::new(1));

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
        let batch = apply_batch(HOME, Sequence::new(7), Sequence::new(4), &record());
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
    fn the_first_record_in_a_home_asserts_that_home_has_no_position_yet() {
        // A home comes into existence with its first record, so there is no
        // key to compare a value against — and an absent key does not satisfy
        // an assertion that it holds zero, which is why this is not simply the
        // same assertion one number lower.
        let batch = apply_batch(HOME, Sequence::new(1), Sequence::new(1), &record());
        let mut absent = 0_u32;
        let mut values = Vec::new();
        for precondition in batch.preconditions() {
            match precondition {
                Precondition::Absent { .. } => absent = absent.saturating_add(1),
                Precondition::ValueIs { expected, .. } => values.push(expected.clone()),
                other => panic!("expected an absence or a value assertion, got {other:?}"),
            }
        }
        assert_eq!(absent, 1, "the home's position, which does not exist yet");
        assert_eq!(
            values,
            vec![Sequence::ZERO.encode()],
            "the version counter, which is store-wide and was seeded at open"
        );
    }

    #[test]
    fn one_batch_carries_the_log_record_the_versions_and_the_position() {
        let batch = apply_batch(HOME, Sequence::new(3), Sequence::new(3), &record());
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
        let batch = apply_batch(HOME, at, version, &record());
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
        // Homed at the store, which is where `home_of` puts a record carrying
        // nothing: there is no mutation to take a narrower home from.
        let batch = apply_batch(
            Reach::Store,
            Sequence::new(2),
            Sequence::new(2),
            &LogRecord::new(Vec::new()),
        );
        assert!(batch.ops().iter().any(|op| op.keyspace() == Keyspace::META));
    }
}
