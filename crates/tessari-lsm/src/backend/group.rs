//! Several batches landed in one engine write and one sync.
//!
//! A group commit hands over batches that were each built on the one before
//! (G040 SG4). Each batch's preconditions are therefore judged against the store
//! as the earlier batches leave it, which this reads from the ops accepted so far
//! before falling back to the engine. Every accepted batch goes into one engine
//! batch written with the store's durability, so the group pays one sync where it
//! used to pay one per batch — and, because the engine publishes a synced write
//! only after its log is synced, nothing in the group is readable before it is
//! durable.

use std::collections::BTreeMap;

use rocksdb::WriteBatch as EngineBatch;
use tessari_kv::{Error, Keyspace, Result, Value, WriteBatch, WriteOp};

use super::LsmBackend;
use crate::Durability;
use crate::error::from_engine;

/// What the accepted batches have done to one key: written a value, or deleted it.
type Written = BTreeMap<(Keyspace, Vec<u8>), Option<Value>>;

impl LsmBackend {
    /// See [`tessari_kv::KvBackend::apply_group`].
    pub(super) fn apply_grouped(&self, batches: Vec<WriteBatch>) -> (usize, Result<()>) {
        let _writer = self.writer();
        let before = self.syncs.landed_so_far();
        let mut written = Written::new();
        let mut engine_batch = EngineBatch::default();
        let mut landed = 0_usize;
        let mut stopped = Ok(());
        for batch in &batches {
            if let Err(failure) = self.stage(batch, &mut written, &mut engine_batch) {
                stopped = Err(failure);
                break;
            }
            landed = landed.saturating_add(1);
        }
        if landed > 0 {
            if let Err(failure) = self
                .database
                .write_opt(engine_batch, &self.durability.write_options())
                .map_err(|error| from_engine(&error))
            {
                return (0, Err(failure));
            }
            self.syncs
                .landed(before, self.durability == Durability::PowerLossSafe);
        }
        (landed, stopped)
    }

    /// Check one batch against the group so far and add its ops, or refuse it
    /// whole: nothing of a refused batch reaches `engine_batch` or `written`.
    fn stage(
        &self,
        batch: &WriteBatch,
        written: &mut Written,
        engine_batch: &mut EngineBatch,
    ) -> Result<()> {
        for precondition in batch.preconditions() {
            let slot = (
                precondition.keyspace(),
                precondition.key().as_slice().to_vec(),
            );
            let observed = match written.get(&slot) {
                Some(value) => value.clone(),
                None => {
                    let region = self.region(precondition.keyspace())?;
                    self.database
                        .get_cf(region, precondition.key().as_slice())
                        .map_err(|error| from_engine(&error))?
                        .map(Value::new)
                }
            };
            if !precondition.is_satisfied_by(observed.as_ref()) {
                return Err(Error::Conflict {
                    keyspace: precondition.keyspace().name().to_owned(),
                    key: precondition.key().clone(),
                });
            }
        }
        for op in batch.ops() {
            self.region(op.keyspace())?;
        }
        for op in batch.ops() {
            let region = self.region(op.keyspace())?;
            match op {
                WriteOp::Put { key, value, .. } => {
                    engine_batch.put_cf(region, key.as_slice(), value.as_slice());
                    written.insert(
                        (op.keyspace(), key.as_slice().to_vec()),
                        Some(value.clone()),
                    );
                }
                WriteOp::Delete { key, .. } => {
                    engine_batch.delete_cf(region, key.as_slice());
                    written.insert((op.keyspace(), key.as_slice().to_vec()), None);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tessari_kv::{Key, Keyspace, KvBackend, Value, WriteBatch};

    use crate::{Durability, LsmBackend, StoreConfig};

    const REGION: Keyspace = Keyspace::META;

    fn key(text: &str) -> Key {
        Key::new(text.as_bytes().to_vec())
    }

    fn value(text: &str) -> Value {
        Value::new(text.as_bytes().to_vec())
    }

    fn open() -> (tempfile::TempDir, LsmBackend) {
        let root = tempfile::tempdir().unwrap();
        let store = LsmBackend::open(root.path().join("store"), StoreConfig::default()).unwrap();
        (root, store)
    }

    #[test]
    fn only_a_store_that_syncs_each_write_asks_for_groups() {
        let (_root, store) = open();
        assert!(store.groups_writes(), "a synced store has a sync to share");
        let root = tempfile::tempdir().unwrap();
        let unsynced = LsmBackend::open(
            root.path().join("store"),
            StoreConfig {
                durability: Durability::ProcessCrashSafe,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        assert!(
            !unsynced.groups_writes(),
            "an unsynced write has nothing to share"
        );
    }

    #[test]
    fn a_batch_may_assert_what_the_batch_before_it_wrote() {
        let (_root, store) = open();
        let first = WriteBatch::new().put(REGION, key("counter"), value("1"));
        let second = WriteBatch::new()
            .expect_value(REGION, key("counter"), value("1"))
            .put(REGION, key("counter"), value("2"));
        let (landed, outcome) = store.apply_group(vec![first, second]);
        assert_eq!(landed, 2, "{outcome:?}");
        assert!(outcome.is_ok());
        assert_eq!(
            store.get(REGION, &key("counter")).unwrap(),
            Some(value("2"))
        );
    }

    #[test]
    fn a_refused_batch_stops_the_group_and_keeps_what_came_before() {
        let (_root, store) = open();
        let first = WriteBatch::new().put(REGION, key("a"), value("1"));
        let refused =
            WriteBatch::new()
                .expect_absent(REGION, key("a"))
                .put(REGION, key("b"), value("1"));
        let after = WriteBatch::new().put(REGION, key("c"), value("1"));
        let (landed, outcome) = store.apply_group(vec![first, refused, after]);
        assert_eq!(landed, 1);
        assert!(
            matches!(outcome, Err(tessari_kv::Error::Conflict { .. })),
            "{outcome:?}"
        );
        assert_eq!(store.get(REGION, &key("a")).unwrap(), Some(value("1")));
        assert_eq!(
            store.get(REGION, &key("b")).unwrap(),
            None,
            "the refused batch landed"
        );
        assert_eq!(
            store.get(REGION, &key("c")).unwrap(),
            None,
            "a batch after the refusal landed"
        );
    }
}
