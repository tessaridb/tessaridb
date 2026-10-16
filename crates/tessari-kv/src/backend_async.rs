//! The asynchronous backend trait the store is moving onto.
//!
//! [`AsyncKvBackend`] carries the contract of [`KvBackend`], method for method, with each
//! answer delivered by a future. It is a trait object (`Arc<dyn AsyncKvBackend>`) because a
//! backend is chosen at run time and wrapped by other backends, so each method returns a boxed
//! future: an `async fn` in a trait cannot be called through `dyn`.
//!
//! Every [`KvBackend`] is also an [`AsyncKvBackend`] through the impl at the bottom of this file,
//! so the layers above can move onto this trait one at a time while every existing backend keeps
//! working. That impl answers inside the future with the synchronous call; it is transitional
//! and goes away with the synchronous trait.

use std::future::Future;
use std::pin::Pin;

use crate::backend::{KvBackend, ScanRequest};
use crate::batch::WriteBatch;
use crate::error::Result;
use crate::key::{Key, KeyRange, Value};
use crate::keyspace::Keyspace;

/// The future every [`AsyncKvBackend`] method returns: a boxed, `Send` future of a result.
pub type KvFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// An ordered key-value store answering through futures.
///
/// The contract is [`KvBackend`]'s, numbered there: ordering, atomicity, precondition coherence,
/// keyspace isolation, absence as a value, batched reads agreeing with single ones, and a range
/// delete removing exactly the range. A method's future must not block the thread that polls it.
///
/// Object-safe: no method returns `Self`, takes a generic parameter or requires `Self: Sized`.
pub trait AsyncKvBackend: Send + Sync + std::fmt::Debug {
    /// A short stable name for this backend, used in errors and logs.
    fn name(&self) -> &'static str;

    /// Read one key; `None` when it is absent.
    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>>;

    /// Read a range of keys.
    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>>;

    /// Read a range once without keeping what it reads; answers what [`Self::scan`] answers.
    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>>;

    /// The first pair of each range, in the order given.
    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>>;

    /// How many pairs a range holds.
    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64>;

    /// Apply a batch atomically, subject to its preconditions.
    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()>;

    /// [`Self::apply`] without making the write durable against power loss yet.
    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()>;

    /// Make every write landed so far durable to the level this backend promises.
    fn sync_applied(&self) -> KvFuture<'_, ()>;

    /// Apply several batches in order, each judged after the ones before; how many landed, and
    /// the failure of the batch that stopped the group.
    fn apply_group(
        &self,
        batches: Vec<WriteBatch>,
    ) -> Pin<Box<dyn Future<Output = (usize, Result<()>)> + Send + '_>>;

    /// Whether [`Self::apply_group`] lands several batches for less than one at a time.
    fn groups_writes(&self) -> bool;

    /// How many background failures this backend has recorded.
    fn background_errors(&self) -> KvFuture<'_, u64>;

    /// Whether a key exists.
    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool>;

    /// Delete exactly the keys of a range.
    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()>;
}

// Transitional: every synchronous backend answers the asynchronous trait with its synchronous
// call. Today every store call already runs on the blocking pool, so nothing here blocks a
// runtime worker that was not already blocked; the impl is removed with the synchronous trait.
impl<T: KvBackend + ?Sized> AsyncKvBackend for T {
    fn name(&self) -> &'static str {
        KvBackend::name(self)
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        Box::pin(async move { KvBackend::get(self, keyspace, key) })
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(async move { KvBackend::scan(self, request) })
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(async move { KvBackend::sweep(self, request) })
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        Box::pin(async move { KvBackend::first_of_each(self, keyspace, ranges) })
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        Box::pin(async move { KvBackend::count(self, keyspace, range) })
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move { KvBackend::apply(self, batch) })
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move { KvBackend::apply_unsynced(self, batch) })
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        Box::pin(async move { KvBackend::sync_applied(self) })
    }

    fn apply_group(
        &self,
        batches: Vec<WriteBatch>,
    ) -> Pin<Box<dyn Future<Output = (usize, Result<()>)> + Send + '_>> {
        Box::pin(async move { KvBackend::apply_group(self, batches) })
    }

    fn groups_writes(&self) -> bool {
        KvBackend::groups_writes(self)
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        Box::pin(async move { KvBackend::background_errors(self) })
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        Box::pin(async move { KvBackend::contains(self, keyspace, key) })
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        Box::pin(async move { KvBackend::delete_range(self, keyspace, range) })
    }
}

/// A shared synchronous backend answering the asynchronous trait, for a holder of
/// `Arc<dyn KvBackend>` that must hand it on as `Arc<dyn AsyncKvBackend>` (an `Arc` of one
/// trait object cannot be turned into an `Arc` of the other). Transitional with the impl
/// above, and removed with it.
#[derive(Debug)]
pub struct Synchronous(pub std::sync::Arc<dyn KvBackend>);

impl AsyncKvBackend for Synchronous {
    fn name(&self) -> &'static str {
        AsyncKvBackend::name(&*self.0)
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        AsyncKvBackend::get(&*self.0, keyspace, key)
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        AsyncKvBackend::scan(&*self.0, request)
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        AsyncKvBackend::sweep(&*self.0, request)
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        AsyncKvBackend::first_of_each(&*self.0, keyspace, ranges)
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        AsyncKvBackend::count(&*self.0, keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        AsyncKvBackend::apply(&*self.0, batch)
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        AsyncKvBackend::apply_unsynced(&*self.0, batch)
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        AsyncKvBackend::sync_applied(&*self.0)
    }

    fn apply_group(
        &self,
        batches: Vec<WriteBatch>,
    ) -> Pin<Box<dyn Future<Output = (usize, Result<()>)> + Send + '_>> {
        AsyncKvBackend::apply_group(&*self.0, batches)
    }

    fn groups_writes(&self) -> bool {
        AsyncKvBackend::groups_writes(&*self.0)
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        AsyncKvBackend::background_errors(&*self.0)
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        AsyncKvBackend::contains(&*self.0, keyspace, key)
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        AsyncKvBackend::delete_range(&*self.0, keyspace, range)
    }
}
