//! The backend trait every storage engine implements.

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::key::{Key, KeyRange, Value};
use crate::keyspace::Keyspace;

/// How many pairs [`KvBackend::count`]'s default reads at a time.
///
/// Private, and deliberately not in `tessari-constants`: this crate's whole
/// dependency list is `thiserror`, it sits in the client's tree, and the
/// published lean-client crate count is a claim the repository's own tests
/// check. A chunk size internal to one default implementation is not worth a
/// dependency edge — it is not a knob anybody sets, only the width of a stride
/// nobody observes.
const COUNT_BATCH_ENTRIES: usize = 1024;

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
/// 6. **Batched reads agree with single ones.** [`Self::first_of_each`] answers
///    each range exactly as a forward [`Self::scan`] of that range limited to
///    one pair would. An override that seeks faster but bounds differently
///    returns a plausible pair belonging to a neighbouring range, which decodes,
///    reads sensibly and is wrong — so this is stated as a contract rather than
///    left to the override's judgement.
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

    /// The first pair of each of several ranges, in one ask.
    ///
    /// Returns one entry per range, in the order the ranges were given: the
    /// first pair in forward key order, or `None` when the range is empty.
    ///
    /// # Why this exists
    ///
    /// The layer above resolves a record by finding the newest version at or
    /// below a snapshot, which is a range bounded to one row rather than a
    /// point read. Resolving the records an index range names therefore issues
    /// one such read per record, and a backend that answers each of them
    /// independently pays a per-record setup cost — on an engine that means an
    /// iterator, and an iterator is not free to create. That cost is a function
    /// of how much state the store holds rather than of how large the answer
    /// is, which is the shape that stops being affordable quietly.
    ///
    /// Asking for many at once lets a backend set up once and seek many times.
    ///
    /// **Defaulted** in terms of [`Self::scan`], so a backend that has nothing
    /// better to offer is still correct — the default is the same work, spelled
    /// the same way, and the override is an optimisation rather than a
    /// contract. An empty slice touches the backend not at all.
    ///
    /// # Errors
    ///
    /// Returns the backend's own failure. A range that is empty is an answer,
    /// not a failure.
    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        let mut found = Vec::with_capacity(ranges.len());
        for range in ranges {
            let request = ScanRequest {
                keyspace,
                range: range.clone(),
                direction: ScanDirection::Forward,
                limit: Some(1),
            };
            found.push(self.scan(&request)?.into_iter().next());
        }
        Ok(found)
    }

    /// How many pairs a range holds.
    ///
    /// # Why this is a method and not `scan(…).len()`
    ///
    /// Because the caller wants a number and `scan` charges it for the data. A
    /// ranking asks how many documents a term is posted against; answering that
    /// through `scan` decodes every posting key **and every value** into a
    /// `Vec` so that its length can be read off — memory proportional to the
    /// corpus to learn one integer, once per query term, per query. A backend
    /// can advance an iterator instead.
    ///
    /// Neither a direction nor a limit is taken: a count is the same in either
    /// direction, and a count of at most *n* is a different question that no
    /// caller here has.
    ///
    /// **Defaulted** in terms of [`Self::scan`], so a backend with nothing
    /// better stays correct — but read in bounded batches rather than in one.
    /// The naive default would be the exact allocation this method exists to
    /// remove, sitting in the method that exists to remove it, waiting for the
    /// first backend that does not override it.
    ///
    /// # Errors
    ///
    /// Returns the backend's own failure. An empty range is `0`, not a failure.
    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        let mut total: u64 = 0;
        let mut remaining = range.clone();
        loop {
            let batch = self.scan(&ScanRequest {
                keyspace,
                range: remaining.clone(),
                direction: ScanDirection::Forward,
                limit: Some(COUNT_BATCH_ENTRIES),
            })?;
            total = total.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
            // A short batch is the end of the range.
            if batch.len() < COUNT_BATCH_ENTRIES {
                return Ok(total);
            }
            let Some((last, _)) = batch.last() else {
                return Ok(total);
            };
            remaining = remaining.resuming_after(last);
        }
    }

    /// Apply a batch atomically, subject to its preconditions.
    ///
    /// Returns [`crate::Error::Conflict`] and writes nothing when a precondition
    /// does not hold.
    fn apply(&self, batch: WriteBatch) -> Result<()>;

    /// How many background failures this backend has recorded.
    ///
    /// An engine that compacts, flushes and writes ahead does that work on its
    /// own threads, and a failure there does not surface at any call a caller
    /// makes — the store keeps answering reads while the thing that keeps it
    /// durable has stopped. It is the failure mode that is silent by
    /// construction, which is why the number exists and why something has to
    /// look at it.
    ///
    /// **Defaulted to zero**, because a backend with no background work has
    /// genuinely had no background failure — that is an answer rather than a
    /// stand-in for one. A backend that does such work overrides this.
    ///
    /// # Errors
    ///
    /// Returns the backend's own failure when the count cannot be read.
    fn background_errors(&self) -> Result<u64> {
        Ok(0)
    }

    /// Whether a key exists.
    ///
    /// Defaulted in terms of [`Self::get`]; a backend that can answer without
    /// materialising the value should override it.
    fn contains(&self, keyspace: Keyspace, key: &Key) -> Result<bool> {
        Ok(self.get(keyspace, key)?.is_some())
    }
}
