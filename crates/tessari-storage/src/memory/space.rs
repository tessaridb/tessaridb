//! One held space's keys, in a map of its own (G079 W5c).
//!
//! Every space held in memory owns one of these, so dropping the space frees
//! its bytes by forgetting the map rather than by finding its keys among
//! everybody else's. A key here is a versioned record key, an expiry entry, a
//! modified-order entry or the space's count record, and every read is a range
//! over one record's versions or one table's keys — so the map is ordered: an
//! `scc::TreeIndex` per keyspace, read without a lock and written by the one
//! commit the write gate lets through at a time.

use std::ops::Bound;

use scc::{Guard, TreeIndex};
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, KvFuture, Precondition, ScanDirection, ScanRequest, Value,
    WriteBatch, WriteOp,
};

const NAME: &str = "memory";

/// A held space's keys, by keyspace.
#[derive(Debug)]
pub(crate) struct SpaceMemory {
    keyspaces: Vec<(Keyspace, TreeIndex<Key, Value>)>,
}

impl SpaceMemory {
    pub(crate) fn new() -> Self {
        Self {
            keyspaces: Keyspace::ALL
                .iter()
                .map(|keyspace| (*keyspace, TreeIndex::new()))
                .collect(),
        }
    }

    fn tree(&self, keyspace: Keyspace) -> tessari_kv::Result<&TreeIndex<Key, Value>> {
        self.keyspaces
            .iter()
            .find(|(held, _)| *held == keyspace)
            .map(|(_, tree)| tree)
            .ok_or_else(|| tessari_kv::Error::UnknownKeyspace {
                keyspace: keyspace.name().to_owned(),
            })
    }

    /// The pairs of `range`, in `direction`, up to `limit`.
    fn pairs(
        &self,
        keyspace: Keyspace,
        range: &KeyRange,
        direction: ScanDirection,
        limit: usize,
    ) -> tessari_kv::Result<Vec<(Key, Value)>> {
        if range.is_provably_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let tree = self.tree(keyspace)?;
        let bounds = (owned(range.start()), owned(range.end()));
        let guard = Guard::new();
        let walked = tree.range::<Key, _>(bounds, &guard);
        let copied = |(key, value): (&Key, &Value)| (key.clone(), value.clone());
        Ok(match direction {
            ScanDirection::Forward => walked.take(limit).map(copied).collect(),
            ScanDirection::Reverse => walked.rev().take(limit).map(copied).collect(),
        })
    }

    fn observed(&self, precondition: &Precondition) -> tessari_kv::Result<Option<Value>> {
        Ok(self
            .tree(precondition.keyspace())?
            .peek_with(precondition.key(), |_, value| value.clone()))
    }
}

fn owned(bound: &Bound<Key>) -> Bound<Key> {
    bound.clone()
}

impl KvBackend for SpaceMemory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        Box::pin(async move {
            Ok(self
                .tree(keyspace)?
                .peek_with(key, |_, value| value.clone()))
        })
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(async move {
            self.pairs(
                request.keyspace,
                &request.range,
                request.direction,
                request.limit.unwrap_or(usize::MAX),
            )
        })
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        Box::pin(async move {
            if range.is_provably_empty() {
                return Ok(0);
            }
            let tree = self.tree(keyspace)?;
            let guard = Guard::new();
            let counted = tree
                .range::<Key, _>((owned(range.start()), owned(range.end())), &guard)
                .count();
            Ok(u64::try_from(counted).unwrap_or(u64::MAX))
        })
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move {
            // Every precondition against the state before any write, so a
            // refused batch writes nothing.
            for precondition in batch.preconditions() {
                if !precondition.is_satisfied_by(self.observed(precondition)?.as_ref()) {
                    return Err(tessari_kv::Error::Conflict {
                        keyspace: precondition.keyspace().to_string(),
                        key: precondition.key().clone(),
                    });
                }
            }
            for op in batch.ops() {
                match op {
                    WriteOp::Put {
                        keyspace,
                        key,
                        value,
                    } => {
                        self.tree(*keyspace)?
                            .upsert_async(key.clone(), value.clone())
                            .await;
                    }
                    WriteOp::Delete { keyspace, key } => {
                        self.tree(*keyspace)?.remove_async(key).await;
                    }
                }
            }
            Ok(())
        })
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        Box::pin(async move {
            self.tree(keyspace)?
                .remove_range_async::<Key, _>((owned(range.start()), owned(range.end())))
                .await;
            Ok(())
        })
    }
}
