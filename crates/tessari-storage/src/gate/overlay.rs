//! The store as the turn's holder sees it: the engine plus what is pending.
//!
//! A commit derives its batch on the store as every earlier commit leaves it,
//! and with group commit an earlier commit may be staged or still on its way to
//! the device (`super::pending`). This backend shows those writes to the one
//! thread holding the turn and to no other: every other reader, and every write,
//! goes straight to the engine.
//!
//! A read that merges holds the pending state for its whole length, so a group
//! landing mid-read cannot take its writes out of the view after the engine was
//! read and before they were merged in.

use std::ops::Bound;
use std::sync::Arc;

use tessari_kv::{Key, KeyRange, Keyspace, KvBackend, Result, ScanDirection, ScanRequest, Value};

use super::WriteGate;
use super::pending::{Bounds, State};

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

    /// The pending state, when this thread holds the turn and something is
    /// pending; `None` sends the read to the engine alone.
    fn pending(&self) -> Option<std::sync::MutexGuard<'_, State>> {
        if !self.gate.held_here() || self.gate.pending.quiet() {
            return None;
        }
        let state = self.gate.pending.lock();
        (!state.is_empty()).then_some(state)
    }

    fn merged(
        &self,
        state: &State,
        request: &ScanRequest,
        read: impl FnOnce(&ScanRequest) -> Result<Vec<(Key, Value)>>,
    ) -> Result<Vec<(Key, Value)>> {
        let Some(bounds) = walkable(&request.range) else {
            return read(request);
        };
        let pending = state.within(request.keyspace, bounds);
        if pending.is_empty() {
            return read(request);
        }
        // Each pending delete can hide at most one engine pair, so this many
        // more is enough for the limit to be met from what comes back.
        let deletes = pending.values().filter(|value| value.is_none()).count();
        let mut widened = request.clone();
        widened.limit = request.limit.map(|limit| limit.saturating_add(deletes));
        let engine = read(&widened)?;
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
        Ok(merged)
    }
}

impl KvBackend for Overlaid {
    fn name(&self) -> &'static str {
        self.engine.name()
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        if let Some(state) = self.pending() {
            if let Some(pending) = state.lookup(keyspace, key.as_slice()) {
                return Ok(pending);
            }
        }
        self.engine.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        match self.pending() {
            Some(state) => self.merged(&state, request, |widened| self.engine.scan(widened)),
            None => self.engine.scan(request),
        }
    }

    fn sweep(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        match self.pending() {
            Some(state) => self.merged(&state, request, |widened| self.engine.sweep(widened)),
            None => self.engine.sweep(request),
        }
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        let Some(state) = self.pending() else {
            return self.engine.first_of_each(keyspace, ranges);
        };
        let mut found = Vec::with_capacity(ranges.len());
        for range in ranges {
            let request = ScanRequest {
                keyspace,
                range: range.clone(),
                direction: ScanDirection::Forward,
                limit: Some(1),
            };
            found.push(
                self.merged(&state, &request, |widened| self.engine.scan(widened))?
                    .into_iter()
                    .next(),
            );
        }
        Ok(found)
    }

    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        let Some(state) = self.pending() else {
            return self.engine.count(keyspace, range);
        };
        let mut total = self.engine.count(keyspace, range)?;
        let Some(bounds) = walkable(range) else {
            return Ok(total);
        };
        for (key, value) in state.within(keyspace, bounds) {
            let stored = self.engine.contains(keyspace, &Key::new(key))?;
            match (value.is_some(), stored) {
                (true, false) => total = total.saturating_add(1),
                (false, true) => total = total.saturating_sub(1),
                _ => {}
            }
        }
        Ok(total)
    }

    fn apply(&self, batch: tessari_kv::WriteBatch) -> Result<()> {
        self.engine.apply(batch)
    }

    fn apply_group(&self, batches: Vec<tessari_kv::WriteBatch>) -> (usize, Result<()>) {
        self.engine.apply_group(batches)
    }

    fn groups_writes(&self) -> bool {
        self.engine.groups_writes()
    }

    fn background_errors(&self) -> Result<u64> {
        self.engine.background_errors()
    }

    fn contains(&self, keyspace: Keyspace, key: &Key) -> Result<bool> {
        if let Some(state) = self.pending() {
            if let Some(pending) = state.lookup(keyspace, key.as_slice()) {
                return Ok(pending.is_some());
            }
        }
        self.engine.contains(keyspace, key)
    }

    fn delete_range(&self, keyspace: Keyspace, range: &KeyRange) -> Result<()> {
        self.engine.delete_range(keyspace, range)
    }
}

/// A range as map bounds, or `None` when it can hold no key — including the
/// equal exclusive pair a map refuses to walk.
fn walkable(range: &KeyRange) -> Option<Bounds<'_>> {
    if range.is_provably_empty() {
        return None;
    }
    if let (Bound::Excluded(low), Bound::Excluded(high)) = (range.start(), range.end()) {
        if low == high {
            return None;
        }
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
