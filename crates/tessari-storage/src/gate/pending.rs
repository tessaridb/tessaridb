//! Commits staged behind the turn and landed together (G040 SG4).
//!
//! # Why a commit no longer applies its own batch
//!
//! A commit that syncs pays one device flush, and with the turn held from the
//! tail read to the apply, sixteen writers paid sixteen flushes one after the
//! other — about 180 commits a second on a disk that could carry the same bytes
//! in one flush. So a commit derives its batch under the turn, **stages** it,
//! releases the turn and waits. Whichever waiter finds no flush running lands
//! everything staged in one engine write and one sync, and wakes the others with
//! their own answers.
//!
//! # Why staged work is visible to the next derivation and to nothing else
//!
//! The next commit derives while the one before it is still on its way to the
//! device, so it must see that commit's writes: its version, its tail, its index
//! entries. [`super::Overlaid`] shows it the staged and in-flight writes — but
//! only to the task holding the turn. A reader anywhere else sees the engine
//! alone, and the engine publishes a synced write only once it is synced, so
//! nothing is readable before it is durable. Index entries, counts and term
//! statistics carry no version, which is why a design that let writes reach the
//! engine before their sync was rejected rather than tuned.
//!
//! # A group that fails refuses everything built on it
//!
//! Every staged batch was derived on the ones before it. When a group stops at a
//! batch, that batch answers its own failure and every batch after it — in the
//! group and still staged — answers the retryable conflict, because each asserted
//! a state that is now not going to exist. They derive again from what did land.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Bound;
use std::panic::{AssertUnwindSafe, resume_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use futures_util::FutureExt as _;
use tokio::sync::Notify;

use tessari_constants::{MAX_GROUP_BYTES, MAX_GROUP_COMMITS};
use tessari_kv::{Error, Key, Keyspace, KvBackend, Result, Value, WriteBatch, WriteOp};

/// What pending writes do to each key they touch: a value, or `None` for a
/// delete.
type Ops = BTreeMap<Keyspace, BTreeMap<Vec<u8>, Option<Value>>>;

/// A key range as the two bounds a map walks.
pub(super) type Bounds<'a> = (Bound<&'a [u8]>, Bound<&'a [u8]>);

/// A staged commit's claim on its answer.
#[derive(Debug)]
#[must_use = "a staged commit has landed only once its ticket is answered"]
pub(crate) struct Ticket(u64);

#[derive(Debug, Default)]
pub(super) struct Pending {
    state: Mutex<State>,
    /// Woken when a landing has recorded its answers. A waiter registers under
    /// the state lock and a landing notifies only after taking it, so no wake is
    /// lost between a waiter's check and its wait.
    settled: Notify,
    /// Whether anything is staged or in flight, readable without the lock:
    /// the turn's holder asks on every read, and almost always the answer is
    /// no. Written under the lock whenever the state changes.
    any: AtomicBool,
}

#[derive(Debug, Default)]
pub(super) struct State {
    staged: VecDeque<Member>,
    /// The writes of the group being landed.
    in_flight: Ops,
    /// The writes of the staged commits, which were derived after — and so win
    /// over — the ones in flight.
    staged_ops: Ops,
    flushing: bool,
    next: u64,
    answers: HashMap<u64, Result<()>>,
}

#[derive(Debug)]
struct Member {
    ticket: u64,
    batch: WriteBatch,
    bytes: usize,
    /// Where a refusal on this commit's behalf says it was refused.
    named: (Keyspace, Key),
}

impl Pending {
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn stage(&self, batch: WriteBatch) -> Ticket {
        let mut state = self.lock();
        let ticket = state.next;
        state.next = ticket.saturating_add(1);
        let bytes = batch.ops().iter().fold(0_usize, |sum, op| {
            let value = match op {
                WriteOp::Put { value, .. } => value.len(),
                WriteOp::Delete { .. } => 0,
            };
            sum.saturating_add(op.key().len()).saturating_add(value)
        });
        record(&mut state.staged_ops, &batch);
        let named = batch
            .preconditions()
            .first()
            .map(|precondition| (precondition.keyspace(), precondition.key().clone()))
            .or_else(|| {
                batch
                    .ops()
                    .first()
                    .map(|op| (op.keyspace(), op.key().clone()))
            })
            .unwrap_or((Keyspace::META, Key::new(Vec::new())));
        self.any.store(true, Ordering::Release);
        state.staged.push_back(Member {
            ticket,
            batch,
            bytes,
            named,
        });
        Ticket(ticket)
    }

    #[cfg(test)]
    pub(super) fn staged(&self) -> usize {
        self.lock().staged.len()
    }

    /// Wait for a staged commit's answer, landing a group when nobody else is.
    pub(super) async fn land(&self, ticket: Ticket, backend: &dyn KvBackend) -> Result<()> {
        loop {
            let notified = self.settled.notified();
            let mut notified = std::pin::pin!(notified);
            let next = {
                let mut state = self.lock();
                if let Some(answer) = state.answers.remove(&ticket.0) {
                    return answer;
                }
                if !state.flushing && !state.staged.is_empty() {
                    Next::Land(state.take_group())
                } else {
                    notified.as_mut().enable();
                    Next::Wait
                }
            };
            match next {
                Next::Land(group) => self.flush(group, backend).await,
                Next::Wait => notified.await,
            }
        }
    }

    /// Land everything pending before a writer that does not stage — a
    /// replica's apply — allocates from the store. Called holding the turn, so
    /// nothing is staged behind it.
    pub(super) async fn land_all(&self, backend: &dyn KvBackend) {
        loop {
            let notified = self.settled.notified();
            let mut notified = std::pin::pin!(notified);
            let next = {
                let mut state = self.lock();
                if state.flushing {
                    notified.as_mut().enable();
                    Next::Wait
                } else if state.staged.is_empty() {
                    return;
                } else {
                    Next::Land(state.take_group())
                }
            };
            match next {
                Next::Land(group) => self.flush(group, backend).await,
                Next::Wait => notified.await,
            }
        }
    }

    async fn flush(&self, group: Vec<Member>, backend: &dyn KvBackend) {
        let mut tickets = Vec::with_capacity(group.len());
        let mut named = Vec::with_capacity(group.len());
        let mut batches = Vec::with_capacity(group.len());
        for member in group {
            tickets.push(member.ticket);
            named.push(member.named);
            batches.push(member.batch);
        }
        let landing = Landing {
            pending: self,
            members: Some((tickets, named)),
            backend: backend.name(),
        };
        // A panic while landing is contained here: left to unwind, it would
        // leave the landing marked as running and every later commit waiting
        // for it for ever. The group's writes are answered as unknown — the
        // engine may or may not have taken them — and the panic goes on up this
        // task afterwards. A landing whose future is dropped part way is
        // answered the same way, by `Landing`'s drop.
        let outcome = AssertUnwindSafe(backend.apply_group(batches))
            .catch_unwind()
            .await;
        landing.settle(outcome);
    }

    /// Record a group's outcome: `None` when the landing was abandoned before it
    /// answered.
    fn record(
        &self,
        tickets: Vec<u64>,
        named: Vec<(Keyspace, Key)>,
        backend: &'static str,
        outcome: Option<std::thread::Result<(usize, Result<()>)>>,
    ) {
        let count = tickets.len();
        let mut state = self.lock();
        state.in_flight.clear();
        state.flushing = false;
        let (landed, outcome, panicked, unknown_reason) = match outcome {
            Some(Ok((landed, outcome))) => (landed, outcome, None, None),
            Some(Err(payload)) => (0, Ok(()), Some(payload), Some(PANICKED)),
            None => (0, Ok(()), None, Some(ABANDONED)),
        };
        let mut failure = outcome.err();
        for (index, (ticket, (keyspace, key))) in tickets.into_iter().zip(named).enumerate() {
            let answer = if let Some(reason) = unknown_reason {
                Err(unknown(backend, reason))
            } else if index < landed {
                Ok(())
            } else if let Some(stopped) = failure.take() {
                Err(stopped)
            } else {
                Err(refused(keyspace, key))
            };
            state.answers.insert(ticket, answer);
        }
        if unknown_reason.is_some() || landed < count {
            state.refuse_staged();
        }
        self.any.store(!state.is_empty(), Ordering::Release);
        drop(state);
        self.settled.notify_waiters();
        if let Some(payload) = panicked {
            resume_unwind(payload);
        }
    }

    /// Whether nothing is pending, without taking the lock.
    pub(super) fn quiet(&self) -> bool {
        !self.any.load(Ordering::Acquire)
    }
}

impl State {
    /// Whether nothing is pending, which is when the overlay has nothing to show.
    pub(super) fn is_empty(&self) -> bool {
        self.in_flight.is_empty() && self.staged_ops.is_empty()
    }

    /// What pending writes did to one key, if they touched it.
    pub(super) fn lookup(&self, keyspace: Keyspace, key: &[u8]) -> Option<Option<Value>> {
        for ops in [&self.staged_ops, &self.in_flight] {
            if let Some(found) = ops.get(&keyspace).and_then(|keys| keys.get(key)) {
                return Some(found.clone());
            }
        }
        None
    }

    /// Pending writes inside bounds, in ascending key order, the newer winning.
    pub(super) fn within(
        &self,
        keyspace: Keyspace,
        bounds: Bounds<'_>,
    ) -> BTreeMap<Vec<u8>, Option<Value>> {
        let mut found = BTreeMap::new();
        for ops in [&self.in_flight, &self.staged_ops] {
            if let Some(keys) = ops.get(&keyspace) {
                for (key, value) in keys.range::<[u8], _>(bounds) {
                    found.insert(key.clone(), value.clone());
                }
            }
        }
        found
    }

    /// Move the front of the staged queue in flight, bounded by commits and
    /// bytes counted as each one is examined.
    fn take_group(&mut self) -> Vec<Member> {
        let mut group = Vec::new();
        let mut bytes = 0_usize;
        while group.len() < MAX_GROUP_COMMITS && (group.is_empty() || bytes < MAX_GROUP_BYTES) {
            let Some(member) = self.staged.pop_front() else {
                break;
            };
            bytes = bytes.saturating_add(member.bytes);
            group.push(member);
        }
        self.flushing = true;
        if self.staged.is_empty() {
            // The whole queue goes, which is the usual case: its writes move in
            // flight as they are.
            self.in_flight = std::mem::take(&mut self.staged_ops);
        } else {
            // Rebuilt from the batches rather than split, since a staying commit
            // may have overwritten a key a commit in the group wrote.
            self.in_flight = Ops::new();
            for member in &group {
                record(&mut self.in_flight, &member.batch);
            }
            self.staged_ops = Ops::new();
            for member in &self.staged {
                record(&mut self.staged_ops, &member.batch);
            }
        }
        group
    }

    fn refuse_staged(&mut self) {
        while let Some(member) = self.staged.pop_front() {
            let (keyspace, key) = member.named;
            self.answers
                .insert(member.ticket, Err(refused(keyspace, key)));
        }
        self.staged_ops.clear();
    }
}

/// Add a batch's writes to `ops`, a later write to a key replacing an earlier.
fn record(ops: &mut Ops, batch: &WriteBatch) {
    for op in batch.ops() {
        let value = match op {
            WriteOp::Put { value, .. } => Some(value.clone()),
            WriteOp::Delete { .. } => None,
        };
        ops.entry(op.keyspace())
            .or_default()
            .insert(op.key().as_slice().to_vec(), value);
    }
}

/// Why a commit's group write leaves its landing unknown.
const PANICKED: &str = "a group write panicked, so whether it landed is unknown";
const ABANDONED: &str = "a group write was abandoned part way, so whether it landed is unknown";

/// The answer for a commit whose group write did not answer: it may have landed.
fn unknown(backend: &'static str, reason: &str) -> Error {
    Error::Backend {
        backend,
        reason: reason.to_owned(),
        source: None,
    }
}

/// What a waiter does once it has looked at the state.
enum Next {
    /// Land this group, which nobody else is landing.
    Land(Vec<Member>),
    /// Wait for the landing that is running.
    Wait,
}

/// A landing group's tickets and the keys its writes name.
type Members = (Vec<u64>, Vec<(Keyspace, Key)>);

/// A group on its way to the engine. Answering its outcome settles it; dropped
/// unsettled (the landing's future abandoned part way), it answers every
/// member unknown, so no commit waits for a landing that will never record.
struct Landing<'a> {
    pending: &'a Pending,
    members: Option<Members>,
    backend: &'static str,
}

impl Landing<'_> {
    fn settle(mut self, outcome: std::thread::Result<(usize, Result<()>)>) {
        if let Some((tickets, named)) = self.members.take() {
            self.pending
                .record(tickets, named, self.backend, Some(outcome));
        }
    }
}

impl Drop for Landing<'_> {
    fn drop(&mut self) {
        if let Some((tickets, named)) = self.members.take() {
            self.pending.record(tickets, named, self.backend, None);
        }
    }
}

/// The retryable answer for a commit derived on writes that did not land.
fn refused(keyspace: Keyspace, key: Key) -> Error {
    Error::Conflict {
        keyspace: keyspace.name().to_owned(),
        key,
    }
}
