//! What a read does with each record its source produces.
//!
//! Push, not pull (ADR-0014): the source calls the consumer once per record, so
//! the consumer decides what to keep and the source never learns what a bound
//! is. This module holds the two consumers a read can have — one that keeps
//! every record because a later stage needs them all, and one that keeps only
//! the records an order can still put first.
//!
//! The second is the whole point. Before it existed, a page of an order no index
//! serves paid for the entire table: the source built a vector of every decoded
//! record and the ordering stage then discarded all but a handful of them
//! (Q-72). Here the discarding happens as the records arrive.

use std::ops::ControlFlow;

use bgv_db_ql::{Expr, Projected};
use bgv_db_storage::Transaction;
use bgv_db_types::{RecordId, Value};

use crate::error::Result;
use crate::evaluate::Scope;
use crate::search::Searched;
use crate::session::Session;
use crate::shape::Topmost;

/// What a source hands each record to.
///
/// The transaction is **lent** rather than held. A consumer evaluates
/// expressions — a projection, a sort key — and this store's evaluator takes
/// `&mut Transaction`; the source holds that borrow while it walks, so it must
/// pass it at the call. The consequence worth naming is that a consumer could in
/// principle write through it mid-walk. None does, and the trait is crate-local.
pub(crate) trait Consumer {
    /// How many records the source is about to hand over.
    ///
    /// Only a source that has its whole set in hand can say — a scan that has
    /// read its payloads, or an arm that built a collection to reach its
    /// context. A source whose count depends on a filter it has not run yet
    /// stays silent rather than guessing high, because an over-estimate here
    /// reserves memory this goal exists to give back.
    ///
    /// Defaulted to doing nothing, because a consumer holding a bounded few has
    /// no use for it. Wave 46 measured what ignoring it costs the consumer that
    /// does: a doubling vector and a sized one differ by 1 093 KiB over 50 000
    /// records, which was 2.4% of that read's peak.
    fn expecting(&mut self, _records: usize) {}

    /// One record.
    ///
    /// `Break` stops the source where it stands — the same mechanism a bound
    /// already uses to reach it (ADR-0013) rather than a second one beside it.
    fn take(
        &mut self,
        transaction: &mut Transaction<'_>,
        id: RecordId,
        record: Value,
    ) -> Result<ControlFlow<()>>;
}

/// Every record, held.
///
/// The consumer for a read holding a stage that must see the whole set before it
/// may emit anything: a `FETCH`, which batches every reference into one ask, or
/// a grouping, which folds many records into one.
pub(crate) struct Collecting {
    records: Vec<(RecordId, Value)>,
}

impl Collecting {
    pub(crate) fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// What the source produced, in the order it produced it.
    pub(crate) fn finish(self) -> Vec<(RecordId, Value)> {
        self.records
    }
}

impl Consumer for Collecting {
    fn expecting(&mut self, records: usize) {
        self.records.reserve(records);
    }

    fn take(
        &mut self,
        _transaction: &mut Transaction<'_>,
        id: RecordId,
        record: Value,
    ) -> Result<ControlFlow<()>> {
        self.records.push((id, record));
        Ok(ControlFlow::Continue(()))
    }
}

/// The answer's shape, applied as the source produces it.
///
/// Projects the record, evaluates the order's keys against **the projected
/// record**, and offers it to the bound. That order matters and is the one the
/// read has always used: it lets a key name what the caller can see, so
/// `SELECT address.city AS home … ORDER BY home` reads the name the answer
/// carries rather than the route it came from.
///
/// # Why it never breaks
///
/// A bound cannot stop an ordering. The record that belongs first may be the
/// last one the source produces, so every record has to be offered even though
/// almost none are kept. What falls is what is *held*, not what is read — and
/// the reading is what the index-served orders already avoid.
pub(crate) struct Shaping<'a, 's> {
    session: &'a Session<'s>,
    /// The projection, folded once, or nothing when the records arrive already
    /// projected — which is the case when a barrier stage ran before this one.
    wanted: Option<Vec<Projected>>,
    /// The order's keys, folded once: a key's constant parts are constant across
    /// every record it is applied to.
    keys: Vec<Expr>,
    searched: &'a Searched,
    topmost: Topmost<'a>,
}

impl<'a, 's> Shaping<'a, 's> {
    pub(crate) fn new(
        session: &'a Session<'s>,
        wanted: Option<Vec<Projected>>,
        keys: Vec<Expr>,
        searched: &'a Searched,
        topmost: Topmost<'a>,
    ) -> Self {
        Self {
            session,
            wanted,
            keys,
            searched,
            topmost,
        }
    }

    /// The records the order put first.
    pub(crate) fn finish(self) -> Vec<(RecordId, Value)> {
        self.topmost.finish()
    }
}

impl Consumer for Shaping<'_, '_> {
    fn take(
        &mut self,
        transaction: &mut Transaction<'_>,
        id: RecordId,
        record: Value,
    ) -> Result<ControlFlow<()>> {
        let record = match &self.wanted {
            Some(wanted) => self
                .session
                .project(transaction, &record, wanted, self.searched)?,
            None => record,
        };
        let mut keys = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            keys.push(self.session.evaluate_in(
                transaction,
                key,
                Scope::searching(&record, self.searched),
            )?);
        }
        self.topmost.offer(keys, id, record);
        Ok(ControlFlow::Continue(()))
    }
}
