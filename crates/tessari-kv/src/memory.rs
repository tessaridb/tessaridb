//! In-memory backend.
//!
//! A correct, non-durable implementation of [`KvBackend`], backed by one
//! [`BTreeMap`] per keyspace behind a single lock.
//!
//! It exists for two reasons, and neither is production use: it makes the test
//! suite fast enough to run on every change, and it is the second implementation
//! that keeps the trait honest — a contract with one implementation is a
//! description of that implementation.
//!
//! One lock covers every keyspace because a batch is atomic *across* keyspaces,
//! so per-keyspace locks would have to be acquired together in a fixed order
//! anyway. That is a deadlock risk bought for concurrency this backend is not
//! meant to deliver.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::RwLock;

use crate::backend::{KvBackend, ScanDirection, ScanRequest};
use crate::batch::{WriteBatch, WriteOp};
use crate::error::{Error, Result};
use crate::key::{Key, KeyRange, Value};
use crate::keyspace::Keyspace;

const BACKEND_NAME: &str = "memory";

type Tree = BTreeMap<Key, Value>;

/// A non-durable, ordered, in-memory store.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    keyspaces: RwLock<BTreeMap<Keyspace, Tree>>,
}

impl MemoryBackend {
    /// Create an empty store with every keyspace in [`Keyspace::ALL`] present.
    ///
    /// The full set is created up front, mirroring the durable case where the
    /// region set is fixed at open and an unexpected one is an error rather than
    /// something to create on demand.
    #[must_use]
    pub fn new() -> Self {
        let mut keyspaces = BTreeMap::new();
        for keyspace in Keyspace::ALL {
            keyspaces.insert(*keyspace, Tree::new());
        }
        Self {
            keyspaces: RwLock::new(keyspaces),
        }
    }

    /// How many keys a keyspace currently holds.
    ///
    /// Intended for tests and diagnostics.
    pub fn len(&self, keyspace: Keyspace) -> Result<usize> {
        let guard = self.read_guard()?;
        let tree = guard.get(&keyspace).ok_or_else(|| Error::UnknownKeyspace {
            keyspace: keyspace.name().to_owned(),
        })?;
        Ok(tree.len())
    }

    /// Whether a keyspace holds no keys.
    pub fn is_empty(&self, keyspace: Keyspace) -> Result<bool> {
        Ok(self.len(keyspace)? == 0)
    }

    fn read_guard(&self) -> Result<std::sync::RwLockReadGuard<'_, BTreeMap<Keyspace, Tree>>> {
        self.keyspaces.read().map_err(|_| Error::Backend {
            backend: BACKEND_NAME,
            reason: "the store lock was poisoned by a panic in another thread".to_owned(),
            source: None,
        })
    }

    fn write_guard(&self) -> Result<std::sync::RwLockWriteGuard<'_, BTreeMap<Keyspace, Tree>>> {
        self.keyspaces.write().map_err(|_| Error::Backend {
            backend: BACKEND_NAME,
            reason: "the store lock was poisoned by a panic in another thread".to_owned(),
            source: None,
        })
    }
}

fn tree_of(keyspaces: &BTreeMap<Keyspace, Tree>, keyspace: Keyspace) -> Result<&Tree> {
    keyspaces
        .get(&keyspace)
        .ok_or_else(|| Error::UnknownKeyspace {
            keyspace: keyspace.name().to_owned(),
        })
}

impl KvBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        let guard = self.read_guard()?;
        Ok(tree_of(&guard, keyspace)?.get(key).cloned())
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        if request.range.is_provably_empty() {
            return Ok(Vec::new());
        }
        let guard = self.read_guard()?;
        let tree = tree_of(&guard, request.keyspace)?;

        let bounds = (
            clone_bound(request.range.start()),
            clone_bound(request.range.end()),
        );
        let limit = request.limit.unwrap_or(usize::MAX);
        let collected: Vec<(Key, Value)> = match request.direction {
            ScanDirection::Forward => tree
                .range(bounds)
                .take(limit)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            ScanDirection::Reverse => tree
                .range(bounds)
                .rev()
                .take(limit)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };
        Ok(collected)
    }

    /// Count the range without cloning a key or a value out of it.
    ///
    /// The default would answer correctly by scanning in chunks, and every pair
    /// it walked would be cloned — a `Key` and a `Value` allocated and dropped
    /// per entry to arrive at a number. Here the range is walked and counted.
    ///
    /// Overridden for the same reason [`Self::first_of_each`] is: this is the
    /// backend every test runs against, so a saving that this backend does not
    /// take is a saving the tests cannot see.
    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        if range.is_provably_empty() {
            return Ok(0);
        }
        let guard = self.read_guard()?;
        let tree = tree_of(&guard, keyspace)?;
        let bounds = (clone_bound(range.start()), clone_bound(range.end()));
        Ok(u64::try_from(tree.range(bounds).count()).unwrap_or(u64::MAX))
    }

    /// One lock acquisition for every range, rather than one each.
    ///
    /// The saving here is modest — this backend's per-scan setup is a lock and
    /// a map lookup. It is overridden anyway because the counted guard above
    /// measures round trips at this boundary, and a backend that answered the
    /// batched call by making the un-batched calls would report the batching
    /// as having no effect on the backend that every test uses.
    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        let guard = self.read_guard()?;
        let tree = tree_of(&guard, keyspace)?;
        Ok(ranges
            .iter()
            .map(|range| {
                if range.is_provably_empty() {
                    return None;
                }
                let bounds = (clone_bound(range.start()), clone_bound(range.end()));
                tree.range(bounds)
                    .next()
                    .map(|(key, value)| (key.clone(), value.clone()))
            })
            .collect())
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        let mut guard = self.write_guard()?;

        // Every precondition is checked against the pre-batch state before any
        // operation runs, so a failure leaves the store untouched.
        for precondition in batch.preconditions() {
            let tree = tree_of(&guard, precondition.keyspace())?;
            let observed = tree.get(precondition.key());
            if !precondition.is_satisfied_by(observed) {
                return Err(Error::Conflict {
                    keyspace: precondition.keyspace().name().to_owned(),
                    key: precondition.key().clone(),
                });
            }
        }

        // An unknown keyspace in any operation must also fail before mutating,
        // otherwise the batch would be partially applied.
        for op in batch.ops() {
            if !guard.contains_key(&op.keyspace()) {
                return Err(Error::UnknownKeyspace {
                    keyspace: op.keyspace().name().to_owned(),
                });
            }
        }

        for op in batch.ops() {
            let Some(tree) = guard.get_mut(&op.keyspace()) else {
                // Unreachable: the loop above proved every keyspace exists, and
                // the lock has been held throughout.
                continue;
            };
            match op {
                WriteOp::Put { key, value, .. } => {
                    tree.insert(key.clone(), value.clone());
                }
                WriteOp::Delete { key, .. } => {
                    tree.remove(key);
                }
            }
        }
        Ok(())
    }
}

fn clone_bound(bound: &Bound<Key>) -> Bound<Key> {
    match bound {
        Bound::Included(key) => Bound::Included(key.clone()),
        Bound::Excluded(key) => Bound::Excluded(key.clone()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::backend::ScanDirection;
    use crate::key::KeyRange;

    #[test]
    fn a_new_store_has_every_keyspace_and_all_are_empty() {
        let backend = MemoryBackend::new();
        for keyspace in Keyspace::ALL {
            assert!(backend.is_empty(*keyspace).unwrap(), "{keyspace} not empty");
        }
    }

    #[test]
    fn len_tracks_writes_and_deletes() {
        let backend = MemoryBackend::new();
        backend
            .apply(
                WriteBatch::new()
                    .put(
                        Keyspace::DATA,
                        Key::from_slice(b"a"),
                        Value::from_slice(b"1"),
                    )
                    .put(
                        Keyspace::DATA,
                        Key::from_slice(b"b"),
                        Value::from_slice(b"2"),
                    ),
            )
            .unwrap();
        assert_eq!(backend.len(Keyspace::DATA).unwrap(), 2);

        backend
            .apply(WriteBatch::new().delete(Keyspace::DATA, Key::from_slice(b"a")))
            .unwrap();
        assert_eq!(backend.len(Keyspace::DATA).unwrap(), 1);
    }

    #[test]
    fn contains_agrees_with_get() {
        let backend = MemoryBackend::new();
        let present = Key::from_slice(b"here");
        let absent = Key::from_slice(b"gone");
        backend
            .apply(WriteBatch::new().put(Keyspace::DATA, present.clone(), Value::from_slice(b"v")))
            .unwrap();

        assert!(backend.contains(Keyspace::DATA, &present).unwrap());
        assert!(!backend.contains(Keyspace::DATA, &absent).unwrap());
    }

    #[test]
    fn a_reverse_scan_with_a_limit_returns_the_largest_keys() {
        let backend = MemoryBackend::new();
        backend
            .apply(
                WriteBatch::new()
                    .put(
                        Keyspace::DATA,
                        Key::from_slice(b"k1"),
                        Value::from_slice(b"1"),
                    )
                    .put(
                        Keyspace::DATA,
                        Key::from_slice(b"k2"),
                        Value::from_slice(b"2"),
                    )
                    .put(
                        Keyspace::DATA,
                        Key::from_slice(b"k3"),
                        Value::from_slice(b"3"),
                    ),
            )
            .unwrap();

        let request = ScanRequest {
            keyspace: Keyspace::DATA,
            range: KeyRange::prefix(b"k"),
            direction: ScanDirection::Reverse,
            limit: Some(2),
        };
        let pairs = backend.scan(&request).unwrap();
        let keys: Vec<Key> = pairs.into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![Key::from_slice(b"k3"), Key::from_slice(b"k2")]);
    }

    #[test]
    fn the_backend_reports_its_name() {
        assert_eq!(MemoryBackend::new().name(), "memory");
    }
}
