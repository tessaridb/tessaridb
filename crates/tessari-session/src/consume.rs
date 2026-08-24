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

use std::collections::BTreeSet;
use std::ops::ControlFlow;

use tessari_ql::{Expr, Projected};
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

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
/// Projects the record, evaluates the order's keys, and offers it to the bound.
///
/// # What an order key can see
///
/// **The source record, overlaid with the projection's output.** Both halves are
/// load-bearing and each was a defect on its own.
///
/// The projection has to be visible, because a key names what the caller can
/// see: `SELECT address.city AS home … ORDER BY home` reads the name the answer
/// carries rather than the route it came from, and the alias **wins** where it
/// shadows a source field of the same name.
///
/// The source has to be visible too, because a key may name a field the
/// projection dropped: `SELECT name FROM places ORDER BY geo::distance(shape,
/// $here) LIMIT 3`. Evaluating that key against the projection alone made every
/// record tie, and the read answered in whatever order the source produced —
/// with no error anywhere (Q-143).
///
/// A fallback would not do. The keys that need the source are exactly the ones
/// that never look absent: `geo::distance` and the vector distances answer `+∞`
/// for an absence rather than `NONE`, precisely so a bounded nearest-first read
/// does not put the shapeless records first.
///
/// # The overlay is not built when it is not needed
///
/// It is an allocation per record on a read path, so the decision is made **once
/// per statement**: the names the projection offers against the root names the
/// keys read. A statement ordering by something it also projects — which is most
/// of them — keeps exactly the cost it had before.
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
    /// Whether any key reads a name the projection does not offer.
    ///
    /// Decided once, from the two name sets, so the per-record path pays for the
    /// overlay only where a key actually needs it.
    keys_reach_past_the_projection: bool,
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
            keys_reach_past_the_projection: reach_past(wanted.as_deref(), &keys),
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

    fn keys_against(
        &self,
        transaction: &mut Transaction<'_>,
        record: &Value,
    ) -> Result<Vec<Value>> {
        let mut keys = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            keys.push(self.session.evaluate_in(
                transaction,
                key,
                Scope::searching(record, self.searched),
            )?);
        }
        Ok(keys)
    }
}

/// Whether any of the order's keys reads a name the projection does not offer.
///
/// `false` when there is no projection at all: nothing was dropped, so nothing
/// is out of reach.
pub(crate) fn reach_past(wanted: Option<&[Projected]>, keys: &[Expr]) -> bool {
    let Some(wanted) = wanted else {
        return false;
    };
    let offered: BTreeSet<&str> = wanted.iter().map(|one| one.name.text.as_str()).collect();
    let mut read = BTreeSet::new();
    for key in keys {
        crate::plan::roots_read(key, &mut read);
    }
    read.iter().any(|root| !offered.contains(root.as_str()))
}

/// The source record with the projection's output written over it.
///
/// The projection wins on a name they share, which is what keeps `SELECT other
/// AS name … ORDER BY name` reading the alias rather than the field it shadows.
///
/// Anything that is not a pair of objects is answered with the projection alone:
/// there is no field-wise overlay to perform, and the projection is what the
/// caller asked to see.
fn overlaid(source: Value, projected: &Value) -> Value {
    let (Value::Object(mut fields), Value::Object(over)) = (source, projected) else {
        return projected.clone();
    };
    for (name, value) in over {
        fields.insert(name.clone(), value.clone());
    }
    Value::Object(fields)
}

impl Consumer for Shaping<'_, '_> {
    fn take(
        &mut self,
        transaction: &mut Transaction<'_>,
        id: RecordId,
        record: Value,
    ) -> Result<ControlFlow<()>> {
        let Some(wanted) = &self.wanted else {
            // Already projected by a barrier stage above, so there is no source
            // left to overlay and nothing was dropped that a key could want.
            let keys = self.keys_against(transaction, &record)?;
            self.topmost.offer(keys, id, record);
            return Ok(ControlFlow::Continue(()));
        };
        let projected = self
            .session
            .project(transaction, &record, wanted, self.searched)?;
        let keys = if self.keys_reach_past_the_projection {
            self.keys_against(transaction, &overlaid(record, &projected))?
        } else {
            self.keys_against(transaction, &projected)?
        };
        self.topmost.offer(keys, id, projected);
        Ok(ControlFlow::Continue(()))
    }
}

#[cfg(test)]
mod tests {
    use tessari_ql::{Expr, ExprKind, FieldPath, Name, Projected, Span};
    use tessari_types::Path;

    use super::reach_past;

    fn somewhere() -> Span {
        Span::new(0, 1)
    }

    fn reads(field: &str) -> Expr {
        Expr {
            kind: ExprKind::Path(FieldPath {
                path: Path::field(field),
                span: somewhere(),
            }),
            span: somewhere(),
        }
    }

    fn offers(name: &str, from: &str) -> Projected {
        Projected {
            value: reads(from),
            name: Name {
                text: name.to_owned(),
                span: somewhere(),
            },
        }
    }

    #[test]
    fn a_key_naming_only_what_the_projection_offers_needs_no_overlay() {
        // The common statement, and the one that must keep its current cost:
        // it orders by a name the answer already carries.
        assert!(!reach_past(
            Some(&[offers("name", "name")]),
            &[reads("name")]
        ));
    }

    #[test]
    fn a_key_naming_a_field_the_projection_dropped_needs_the_overlay() {
        assert!(reach_past(
            Some(&[offers("name", "name")]),
            &[reads("shape")]
        ));
    }

    #[test]
    fn an_alias_counts_as_offered_under_the_name_it_answers_by() {
        // `SELECT address.city AS home … ORDER BY home` — the key names `home`,
        // which the projection offers, so no overlay and no change of behaviour.
        assert!(!reach_past(
            Some(&[offers("home", "address")]),
            &[reads("home")]
        ));
        // And the route it came from is *not* offered, so ordering by that
        // instead does need the source.
        assert!(reach_past(
            Some(&[offers("home", "address")]),
            &[reads("address")]
        ));
    }

    #[test]
    fn nothing_is_out_of_reach_when_there_is_no_projection() {
        assert!(!reach_past(None, &[reads("anything")]));
    }

    #[test]
    fn one_key_out_of_several_is_enough_to_need_the_overlay() {
        assert!(reach_past(
            Some(&[offers("name", "name")]),
            &[reads("name"), reads("shape")]
        ));
    }
}
