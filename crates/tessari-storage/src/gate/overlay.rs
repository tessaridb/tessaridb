//! The store as the turn's holder sees it: the engine plus what is pending.
//!
//! A commit derives its batch on the store as every earlier commit leaves it,
//! and with group commit an earlier commit may be staged or still on its way to
//! the device (`super::pending`). This backend shows those writes to the one
//! task holding the turn and to no other: every other reader, and every write,
//! goes straight to the engine.
//!
//! A read that merges copies the pending writes in its range before it reads the
//! engine, so a group landing mid-read cannot take its writes out of the view,
//! and no lock is held across the engine's await.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use tessari_kv::{
    Error, Key, KeyRange, Keyspace, KvBackend, KvFuture, Result, ScanDirection, ScanRequest, Value,
};

use super::WriteGate;
use super::pending::Bounds;

/// Pending writes inside a range: a value, or `None` for a delete.
type Within = BTreeMap<Vec<u8>, Option<Value>>;

/// A backend that shows the turn's holder the writes still pending.
#[derive(Debug)]
pub(crate) struct Overlaid {
    engine: Arc<dyn KvBackend>,
    gate: Arc<WriteGate>,
}

impl Overlaid {
    pub(crate) fn new(engine: Arc<dyn KvBackend>, gate: Arc<WriteGate>) -> Self {
        Self { engine, gate }
    }

    /// Whether this read is the turn's holder's with something pending; `false`
    /// sends it to the engine alone.
    fn overlays(&self) -> bool {
        self.gate.held_here() && !self.gate.pending.quiet()
    }

    /// What pending writes did to one key, when this read overlays them.
    fn pending_one(&self, keyspace: Keyspace, key: &Key) -> Option<Option<Value>> {
        if !self.overlays() {
            return None;
        }
        self.gate.pending.lock().lookup(keyspace, key.as_slice())
    }

    /// The pending writes inside `range`, copied out of the lock, when this read
    /// overlays them. A copy rather than the held state, so no lock is held while
    /// the engine is read: only the holder stages, so nothing is added under its
    /// read, and a group landing meanwhile cannot take writes out of a copy.
    fn pending_within(&self, keyspace: Keyspace, range: &KeyRange) -> Option<Within> {
        if !self.overlays() {
            return None;
        }
        let bounds = walkable(range)?;
        let found = self.gate.pending.lock().within(keyspace, bounds);
        (!found.is_empty()).then_some(found)
    }

    /// Read `request` from the engine with `pending` merged over it.
    async fn merged(
        &self,
        request: &ScanRequest,
        pending: Within,
        sweep: bool,
    ) -> Result<Vec<(Key, Value)>> {
        // Each pending delete can hide at most one engine pair, so this many
        // more is enough for the limit to be met from what comes back.
        let deletes = pending.values().filter(|value| value.is_none()).count();
        let mut widened = request.clone();
        widened.limit = request.limit.map(|limit| limit.saturating_add(deletes));
        let engine = if sweep {
            self.engine.sweep(&widened).await?
        } else {
            self.engine.scan(&widened).await?
        };
        Ok(merge(request, engine, pending))
    }

    /// Read `request`, merged with what is pending when this read overlays it.
    async fn read(&self, request: &ScanRequest, sweep: bool) -> Result<Vec<(Key, Value)>> {
        match self.pending_within(request.keyspace, &request.range) {
            Some(pending) => self.merged(request, pending, sweep).await,
            None if sweep => self.engine.sweep(request).await,
            None => self.engine.scan(request).await,
        }
    }
}

/// The engine's pairs with the pending writes applied, in the request's order and
/// up to its limit.
fn merge(request: &ScanRequest, engine: Vec<(Key, Value)>, pending: Within) -> Vec<(Key, Value)> {
    let limit = request.limit.unwrap_or(usize::MAX);
    let mut merged = Vec::new();
    let forward = request.direction == ScanDirection::Forward;
    let mut engine = engine.into_iter().peekable();
    let mut pending: Vec<_> = pending.into_iter().collect();
    if !forward {
        pending.reverse();
    }
    let mut pending = pending.into_iter();
    let mut next_pending = pending.next();
    while merged.len() < limit {
        let take_pending = match (engine.peek(), &next_pending) {
            (None, None) => break,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some((key, _)), Some((pending_key, _))) => {
                let order = pending_key.as_slice().cmp(key.as_slice());
                if order.is_eq() {
                    // The pending write replaces the engine's pair.
                    engine.next();
                    true
                } else {
                    order.is_lt() == forward
                }
            }
        };
        if take_pending {
            if let Some((key, Some(value))) = next_pending.take() {
                merged.push((Key::new(key), value));
            }
            next_pending = pending.next();
        } else if let Some(pair) = engine.next() {
            merged.push(pair);
        }
    }
    merged
}

impl KvBackend for Overlaid {
    fn name(&self) -> &'static str {
        self.engine.name()
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        Box::pin(async move {
            match self.pending_one(keyspace, key) {
                Some(pending) => Ok(pending),
                None => self.engine.get(keyspace, key).await,
            }
        })
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(self.read(request, false))
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(self.read(request, true))
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        Box::pin(async move {
            if !self.overlays() {
                return self.engine.first_of_each(keyspace, ranges).await;
            }
            let mut found = Vec::with_capacity(ranges.len());
            for range in ranges {
                let request = ScanRequest {
                    keyspace,
                    range: range.clone(),
                    direction: ScanDirection::Forward,
                    limit: Some(1),
                };
                found.push(self.read(&request, false).await?.into_iter().next());
            }
            Ok(found)
        })
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        Box::pin(async move {
            let pending = self.pending_within(keyspace, range);
            let mut total = self.engine.count(keyspace, range).await?;
            for (key, value) in pending.unwrap_or_default() {
                let stored = self.engine.contains(keyspace, &Key::new(key)).await?;
                match (value.is_some(), stored) {
                    (true, false) => total = total.saturating_add(1),
                    (false, true) => total = total.saturating_sub(1),
                    _ => {}
                }
            }
            Ok(total)
        })
    }

    fn apply(&self, batch: tessari_kv::WriteBatch) -> KvFuture<'_, ()> {
        self.engine.apply(batch)
    }

    // Forwarded, not defaulted: the defaults would sync every write and then
    // sync nothing, which is the engine's cost with none of the saving.
    fn apply_unsynced(&self, batch: tessari_kv::WriteBatch) -> KvFuture<'_, ()> {
        self.engine.apply_unsynced(batch)
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        self.engine.sync_applied()
    }

    fn apply_group(
        &self,
        batches: Vec<tessari_kv::WriteBatch>,
    ) -> std::pin::Pin<Box<dyn Future<Output = (usize, Result<()>)> + Send + '_>> {
        self.engine.apply_group(batches)
    }

    fn groups_writes(&self) -> bool {
        self.engine.groups_writes()
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        self.engine.background_errors()
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        Box::pin(async move {
            match self.pending_one(keyspace, key) {
                Some(pending) => Ok(pending.is_some()),
                None => self.engine.contains(keyspace, key).await,
            }
        })
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        self.engine.delete_range(keyspace, range)
    }

    fn close(self: Arc<Self>) -> KvFuture<'static, ()> {
        // Nothing is staged once the view is the last handle: every staged
        // write belongs to a commit still holding the store.
        match Arc::try_unwrap(self) {
            Ok(overlaid) => overlaid.engine.close(),
            Err(shared) => {
                let backend = shared.engine.name();
                Box::pin(async move {
                    Err(Error::Lifecycle {
                        backend,
                        reason: "the store is still in use".to_owned(),
                    })
                })
            }
        }
    }
}

/// A range as map bounds, or `None` when it can hold no key — including the
/// equal exclusive pair a map refuses to walk.
fn walkable(range: &KeyRange) -> Option<Bounds<'_>> {
    if range.is_provably_empty() {
        return None;
    }
    if let (Bound::Excluded(low), Bound::Excluded(high)) = (range.start(), range.end())
        && low == high
    {
        return None;
    }
    Some((side(range.start()), side(range.end())))
}

fn side(bound: &Bound<Key>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(key) => Bound::Included(key.as_slice()),
        Bound::Excluded(key) => Bound::Excluded(key.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}
