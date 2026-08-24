//! Atomic write batches with preconditions.
//!
//! A batch is the only way to change the store. It carries the changes to make
//! and, optionally, the conditions under which making them is still correct.
//!
//! Atomicity is the whole point: the engine writes a record and the log position
//! that accounts for it in one batch, so a crash cannot leave the store holding
//! one without the other. See ADR-0001.

use crate::key::{Key, Value};
use crate::keyspace::Keyspace;

/// A single change to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Write a value, replacing any existing one.
    Put {
        /// Which keyspace to write into.
        keyspace: Keyspace,
        /// The key to write.
        key: Key,
        /// The value to store.
        value: Value,
    },
    /// Remove a key. Removing a key that is absent is not an error.
    Delete {
        /// Which keyspace to delete from.
        keyspace: Keyspace,
        /// The key to remove.
        key: Key,
    },
}

impl WriteOp {
    /// The keyspace this operation targets.
    #[must_use]
    pub const fn keyspace(&self) -> Keyspace {
        match self {
            Self::Put { keyspace, .. } | Self::Delete { keyspace, .. } => *keyspace,
        }
    }

    /// The key this operation targets.
    #[must_use]
    pub const fn key(&self) -> &Key {
        match self {
            Self::Put { key, .. } | Self::Delete { key, .. } => key,
        }
    }
}

/// A condition that must hold for the batch to be applied.
///
/// Preconditions are what make optimistic concurrency possible on a store that
/// offers no locking: read a value, decide, then write conditionally on the
/// value not having moved underneath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// The key must currently hold exactly this value.
    ValueIs {
        /// Which keyspace to check.
        keyspace: Keyspace,
        /// The key to check.
        key: Key,
        /// The value it must hold.
        expected: Value,
    },
    /// The key must currently be absent.
    Absent {
        /// Which keyspace to check.
        keyspace: Keyspace,
        /// The key that must not exist.
        key: Key,
    },
    /// The key must currently exist, whatever its value.
    Exists {
        /// Which keyspace to check.
        keyspace: Keyspace,
        /// The key that must exist.
        key: Key,
    },
}

impl Precondition {
    /// The keyspace this precondition reads.
    #[must_use]
    pub const fn keyspace(&self) -> Keyspace {
        match self {
            Self::ValueIs { keyspace, .. }
            | Self::Absent { keyspace, .. }
            | Self::Exists { keyspace, .. } => *keyspace,
        }
    }

    /// The key this precondition reads.
    #[must_use]
    pub const fn key(&self) -> &Key {
        match self {
            Self::ValueIs { key, .. } | Self::Absent { key, .. } | Self::Exists { key, .. } => key,
        }
    }

    /// Whether an observed value satisfies this precondition.
    #[must_use]
    pub fn is_satisfied_by(&self, observed: Option<&Value>) -> bool {
        match self {
            Self::ValueIs { expected, .. } => observed == Some(expected),
            Self::Absent { .. } => observed.is_none(),
            Self::Exists { .. } => observed.is_some(),
        }
    }
}

/// A set of changes applied atomically, guarded by a set of preconditions.
///
/// Every precondition is evaluated against the same state the operations are
/// applied to. If any fails, nothing is written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteBatch {
    preconditions: Vec<Precondition>,
    ops: Vec<WriteOp>,
}

impl WriteBatch {
    /// An empty batch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            preconditions: Vec::new(),
            ops: Vec::new(),
        }
    }

    /// Add a put.
    #[must_use]
    pub fn put(mut self, keyspace: Keyspace, key: Key, value: Value) -> Self {
        self.ops.push(WriteOp::Put {
            keyspace,
            key,
            value,
        });
        self
    }

    /// Add a delete.
    #[must_use]
    pub fn delete(mut self, keyspace: Keyspace, key: Key) -> Self {
        self.ops.push(WriteOp::Delete { keyspace, key });
        self
    }

    /// Require that a key currently holds a given value.
    #[must_use]
    pub fn expect_value(mut self, keyspace: Keyspace, key: Key, expected: Value) -> Self {
        self.preconditions.push(Precondition::ValueIs {
            keyspace,
            key,
            expected,
        });
        self
    }

    /// Require that a key is currently absent.
    #[must_use]
    pub fn expect_absent(mut self, keyspace: Keyspace, key: Key) -> Self {
        self.preconditions
            .push(Precondition::Absent { keyspace, key });
        self
    }

    /// Require that a key currently exists.
    #[must_use]
    pub fn expect_exists(mut self, keyspace: Keyspace, key: Key) -> Self {
        self.preconditions
            .push(Precondition::Exists { keyspace, key });
        self
    }

    /// The preconditions guarding this batch.
    #[must_use]
    pub fn preconditions(&self) -> &[Precondition] {
        &self.preconditions
    }

    /// The operations this batch applies, in order.
    #[must_use]
    pub fn ops(&self) -> &[WriteOp] {
        &self.ops
    }

    /// Whether the batch would change nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// How many operations the batch carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn key(bytes: &[u8]) -> Key {
        Key::from_slice(bytes)
    }

    fn value(bytes: &[u8]) -> Value {
        Value::from_slice(bytes)
    }

    #[test]
    fn value_is_matches_only_the_exact_value() {
        let precondition = Precondition::ValueIs {
            keyspace: Keyspace::DATA,
            key: key(b"k"),
            expected: value(b"v"),
        };
        assert!(precondition.is_satisfied_by(Some(&value(b"v"))));
        assert!(!precondition.is_satisfied_by(Some(&value(b"other"))));
        assert!(!precondition.is_satisfied_by(None));
    }

    #[test]
    fn absent_matches_only_a_missing_key() {
        let precondition = Precondition::Absent {
            keyspace: Keyspace::DATA,
            key: key(b"k"),
        };
        assert!(precondition.is_satisfied_by(None));
        assert!(!precondition.is_satisfied_by(Some(&value(b""))));
    }

    #[test]
    fn exists_matches_any_present_value_including_empty() {
        let precondition = Precondition::Exists {
            keyspace: Keyspace::DATA,
            key: key(b"k"),
        };
        assert!(precondition.is_satisfied_by(Some(&value(b""))));
        assert!(precondition.is_satisfied_by(Some(&value(b"v"))));
        assert!(!precondition.is_satisfied_by(None));
    }

    #[test]
    fn batch_preserves_operation_order() {
        let batch = WriteBatch::new()
            .put(Keyspace::DATA, key(b"a"), value(b"1"))
            .delete(Keyspace::DATA, key(b"b"))
            .put(Keyspace::LOG, key(b"c"), value(b"3"));

        assert_eq!(batch.len(), 3);
        assert!(!batch.is_empty());
        assert_eq!(batch.ops()[0].key(), &key(b"a"));
        assert_eq!(batch.ops()[1].key(), &key(b"b"));
        assert_eq!(batch.ops()[2].keyspace(), Keyspace::LOG);
    }

    #[test]
    fn an_empty_batch_is_empty() {
        assert!(WriteBatch::new().is_empty());
        assert_eq!(WriteBatch::new().len(), 0);
        assert!(WriteBatch::new().preconditions().is_empty());
    }
}
