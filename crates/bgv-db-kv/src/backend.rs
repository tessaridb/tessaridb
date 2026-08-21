//! The backend trait every storage engine implements.

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::key::{Key, KeyRange, Value};
use crate::keyspace::Keyspace;

/// Which way a scan walks the key space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanDirection {
    /// Ascending lexicographic order.
    #[default]
    Forward,
    /// Descending lexicographic order.
    ///
    /// Present because descending index iteration is how `ORDER BY … DESC` is
    /// served without sorting the result set.
    Reverse,
}

/// A range read request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRequest {
    /// Which keyspace to read.
    pub keyspace: Keyspace,
    /// The span of keys to read.
    pub range: KeyRange,
    /// Which way to walk it.
    pub direction: ScanDirection,
    /// Stop after this many pairs. `None` reads the whole range.
    ///
    /// An unbounded scan over a large keyspace is a real hazard, so the limit is
    /// part of the request rather than something callers remember to apply
    /// afterwards.
    pub limit: Option<usize>,
}

impl ScanRequest {
    /// A forward, unlimited scan of a range.
    #[must_use]
    pub const fn new(keyspace: Keyspace, range: KeyRange) -> Self {
        Self {
            keyspace,
            range,
            direction: ScanDirection::Forward,
            limit: None,
        }
    }

    /// Walk the range in reverse.
    #[must_use]
    pub fn reversed(mut self) -> Self {
        self.direction = ScanDirection::Reverse;
        self
    }

    /// Stop after `limit` pairs.
    #[must_use]
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// An ordered key-value store.
///
/// # Contract
///
/// An implementation must guarantee all of the following. A backend that cannot
/// is not a valid backend for this engine.
///
/// 1. **Ordering.** Keys iterate in lexicographic byte order, and
///    [`ScanDirection::Reverse`] yields exactly the forward order reversed.
/// 2. **Atomicity.** [`Self::apply`] is all-or-nothing, including across
///    keyspaces and including across a crash. A partially applied batch must not
///    be observable.
/// 3. **Precondition coherence.** Preconditions are evaluated against the same
///    state the operations are applied to. A batch whose precondition fails
///    writes nothing and returns [`crate::Error::Conflict`].
/// 4. **Keyspace isolation.** A key written to one keyspace is never visible
///    from another.
/// 5. **Absence is a value.** Reading a key that does not exist returns
///    `Ok(None)`, never an error.
///
/// # What this trait deliberately does not provide
///
/// The layer above supplies each of these, and the omissions are the reason it
/// can:
///
/// - **No sequencing.** The backend assigns no version, timestamp or ordering to
///   writes. The engine owns sequence numbers, because it owns the replication
///   log (ADR-0001) and a backend inventing its own would compete with it.
/// - **No multi-statement transactions.** A batch is atomic; a transaction that
///   spans reads and writes across time is built above, out of batches and
///   preconditions.
/// - **No isolation between concurrent readers and writers** beyond what
///   individual operations give. Snapshot semantics belong to the engine's MVCC
///   layer.
/// - **No secondary indexes.** Index entries are ordinary keys in
///   [`Keyspace::INDEX`], written by the engine inside the same batch as the
///   record they point at. A backend maintains nothing automatically.
/// - **No uniqueness enforcement.** Uniqueness is a read plus a
///   [`crate::Precondition::Absent`] in the same batch.
/// - **No retention or garbage collection policy.** The mechanism to delete is
///   here; deciding what to delete and when is the engine's.
///
/// # Object safety
///
/// This trait is object-safe and is used as `Arc<dyn KvBackend>`. Backends are
/// chosen at runtime, so dynamic dispatch is the right cost.
pub trait KvBackend: Send + Sync + std::fmt::Debug {
    /// A short stable name for this backend, used in errors and logs.
    fn name(&self) -> &'static str;

    /// Read one key.
    ///
    /// Returns `Ok(None)` when the key is absent — that is a value, not a
    /// failure.
    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>>;

    /// Read a range of keys.
    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>>;

    /// Apply a batch atomically, subject to its preconditions.
    ///
    /// Returns [`crate::Error::Conflict`] and writes nothing when a precondition
    /// does not hold.
    fn apply(&self, batch: WriteBatch) -> Result<()>;

    /// Whether a key exists.
    ///
    /// Defaulted in terms of [`Self::get`]; a backend that can answer without
    /// materialising the value should override it.
    fn contains(&self, keyspace: Keyspace, key: &Key) -> Result<bool> {
        Ok(self.get(keyspace, key)?.is_some())
    }
}
