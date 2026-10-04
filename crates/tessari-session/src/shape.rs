//! Giving an answer the shape the statement asked for.
//!
//! Sorting and bounding, and nothing else. Separate from evaluating an
//! expression and from reading records because it is a third question — *which
//! of these, in what order* — and because the two rules it holds are the ones a
//! reader comes looking for.

use tessari_ql::{Fusion, Ordering, Select};

mod fused;

pub(crate) use fused::Fused;
use tessari_types::{Number, RecordId, Value};

/// A record travelling through the sort: its keys, its id, and itself.
type Keyed = (Vec<Value>, RecordId, Value);

/// Which of two records the order puts first.
///
/// **The order is the value system's** (`docs/value-system.md` §3),
/// including across types and including the absences: `none` sorts below
/// `null` sorts below every present value. That is the opposite of what a
/// *comparison* does with them — `age < 18` is false for a record with no
/// age — and deliberately so: a comparison against a non-value has no
/// answer, while a sort has to put every row somewhere, and "somewhere" is
/// better stated than left to whichever row the scan reached first.
///
/// **Ties are broken by the record's id**, which is unique, so the answer is
/// the same every time whatever access path ran. Without that, adding an
/// index would reorder equal rows — an answer that changes when an index
/// appears, which is the shape this store keeps refusing. It is also what makes
/// the order **total**, which is what lets [`Topmost`] discard a record on the
/// evidence of the ones it has already seen.
///
/// One function rather than one per consumer. A bounded collector and an
/// unbounded sort that each carried their own comparison would be two orders
/// that agree until somebody edits one of them, and the symptom would be a
/// statement answering differently with a `LIMIT` than without.
fn ranked(
    left: (&[Value], &RecordId),
    right: (&[Value], &RecordId),
    order: &[Ordering],
) -> core::cmp::Ordering {
    for (position, key) in order.iter().enumerate() {
        let Some((held, other)) = left.0.get(position).zip(right.0.get(position)) else {
            continue;
        };
        let ordered = if key.descending {
            other.cmp(held)
        } else {
            held.cmp(other)
        };
        if ordered != core::cmp::Ordering::Equal {
            return ordered;
        }
    }
    left.1.cmp(right.1)
}

/// The records an order puts first, without holding the ones it does not.
///
/// # Why a bound belongs here and needs no refusal condition
///
/// The read applies [`bounded`] to this stage's output on the very next line, so
/// for any input at all
///
/// ```text
/// bounded(sort(x), start, limit)  ==  keeping(start + limit).offer(x…).finish()
/// ```
///
/// is an identity between two **adjacent** stages, not a judgement about the
/// statement above them. That is the whole difference from the bound the planner
/// hands the source (ADR-0013 mechanism 1), which travels past the `FETCH`, the
/// projection and the grouping — any of which can change how many records exist,
/// which is why that one is a whitelist of shapes known to preserve the count.
/// Nothing sits between a sort and its bound, so there is no shape to enumerate
/// and no clause added later that could quietly shorten an answer. Grouping is
/// **already applied** when this runs: the records here are the groups, and a
/// bound on groups is the bound the caller wrote.
///
/// # What it costs, and what it does not buy
///
/// Sorting fifty thousand records to answer with ten built three vectors over
/// every record — the keyed one, the projection pass, and the final unkeying —
/// on top of the source's own. Keeping the top *n* builds none of them: an
/// ordered limit stopped costing more than the unordered read of the same table.
/// It does **not** make the read cheap. The source still hands over a vector of
/// every record it read, and that floor is a question about the source rather
/// than about the order.
///
/// # Why the keys are projected on the way in
///
/// The value system's order across numbers is defined by their decimal
/// projections, so `Number::cmp` calls `as_decimal()` on **both** sides of every
/// comparison — and for a float that is a real conversion, not an accessor. A
/// sort compares each key about `log n` times, so a two-thousand-record sort on
/// float keys performs some forty thousand conversions to answer twenty-two
/// thousand questions.
///
/// The benchmark harness found this on its first day: a nearest-neighbour read
/// over 2000 records cost 12.5 ms, of which about 10 ms was here rather than in
/// the distance it was ordering by. The give-away was that the cost did not move
/// when the query vector went from one component to eight — a length mismatch
/// short-circuits the distance, so every record scored the same and the sort had
/// nothing to order — and then jumped fivefold at the width where the keys
/// finally differed.
///
/// [`projected`] does the conversion **once per key** and hands the comparator
/// the same decimals it would have computed. It is not an approximation of the
/// order; it is the order's own definition, evaluated eagerly. The keys
/// themselves are evaluated once per record by the caller rather than inside the
/// comparison, because an expression is evaluated every time it is asked for and
/// a sort asks many times.
pub(crate) struct Topmost<'a> {
    /// The keys, in the statement's order, with their directions.
    order: &'a [Ordering],
    /// How many records survive a compaction, or all of them when unbounded.
    wanted: Option<usize>,
    /// How full the buffer is allowed to get before it compacts.
    ///
    /// Twice the bound, so a compaction discards about as many records as it
    /// keeps and the sorting is amortised across them. One is the floor: a
    /// `LIMIT 0` answers correctly either way, and without the floor it would
    /// sort a one-record buffer on every record of the table to do it.
    room: Option<usize>,
    /// The best seen so far, plus whatever has arrived since the last
    /// compaction.
    held: Vec<Keyed>,
    /// The record a cursor resumes after, keyed the same way the offered
    /// records are.
    ///
    /// Applied here rather than to the finished answer, and that placement is
    /// the whole point: filtering afterwards would let the bound keep the
    /// *first* `wanted` records — the ones before the anchor — and then throw
    /// them all away, so page two of a bounded read would come back empty. Sat
    /// in front of the bound, the bound keeps the first `wanted` records **of
    /// the page**, which is what was asked for.
    anchor: Option<(Vec<Value>, RecordId)>,
    /// The fusion the keys are branches of, when the order is `FUSE (…)`: every
    /// record is held, because a rank depends on all of them.
    fusion: Option<&'a Fusion>,
}

impl<'a> Topmost<'a> {
    /// The collector `select`'s order needs: fused when it is `FUSE (…)`,
    /// otherwise keeping `wanted`.
    pub(crate) fn of(select: &'a Select, wanted: Option<usize>) -> Self {
        match &select.fusion {
            Some(fusion) => Self {
                fusion: Some(fusion),
                ..Self::keeping(&select.order, None)
            },
            None => Self::keeping(&select.order, wanted),
        }
    }

    /// A collector for `wanted` records, or for all of them when it is `None`.
    ///
    /// The buffer is not pre-allocated. `wanted` comes from a `LIMIT` the caller
    /// wrote, so it can be any number a `u64` holds, and reserving twice an
    /// absurd one aborts the process to serve a statement that will answer with
    /// whatever the table happens to hold.
    pub(crate) fn keeping(order: &'a [Ordering], wanted: Option<usize>) -> Self {
        Self {
            order,
            wanted,
            room: wanted.map(|wanted| wanted.saturating_mul(2).max(1)),
            held: Vec::new(),
            anchor: None,
            fusion: None,
        }
    }

    /// Resume after one record: keep only what sorts strictly past it.
    ///
    /// The keys are the anchor's own, evaluated by the same stage that evaluates
    /// every other record's, so the comparison is the answer's order and not an
    /// approximation of it. An empty key list is the read that named no order,
    /// where [`ranked`] falls through to the identity and the page resumes in
    /// the store's own order.
    pub(crate) fn after(&mut self, keys: Vec<Value>, id: RecordId) {
        self.anchor = Some((keys.into_iter().map(projected).collect(), id));
    }

    /// Offer one record, which is kept only while it may still be in the answer.
    pub(crate) fn offer(&mut self, keys: Vec<Value>, id: RecordId, record: Value) {
        let keys: Vec<Value> = keys.into_iter().map(projected).collect();
        if let Some((anchor_keys, anchor_id)) = &self.anchor
            && ranked((&keys, &id), (anchor_keys, anchor_id), self.order)
                != core::cmp::Ordering::Greater
        {
            return;
        }
        self.held.push((keys, id, record));
        if self.room.is_some_and(|room| self.held.len() > room) {
            self.compact();
        }
    }

    /// The records kept, in order.
    pub(crate) fn finish(mut self) -> Vec<(RecordId, Value)> {
        if let Some(fusion) = self.fusion {
            return fused::fused(self.held, self.order, fusion)
                .into_iter()
                .map(|(id, record, _)| (id, record))
                .collect();
        }
        self.compact();
        self.held
            .into_iter()
            .map(|(_, id, record)| (id, record))
            .collect()
    }

    /// The records a fused order put first, each with its ranks; an order that
    /// is not fused answers its own records with no ranks.
    pub(crate) fn finish_fused(self) -> Vec<Fused> {
        match self.fusion {
            Some(fusion) => fused::fused(self.held, self.order, fusion),
            None => self
                .finish()
                .into_iter()
                .map(|(id, record)| (id, record, Vec::new()))
                .collect(),
        }
    }

    /// Sort what is held and drop everything that cannot reach the answer.
    ///
    /// Sound because the order is total: a record ranked below `wanted` others
    /// already seen can never rise, since nothing arriving later removes one of
    /// them.
    fn compact(&mut self) {
        let order = self.order;
        self.held
            .sort_by(|left, right| ranked((&left.0, &left.1), (&right.0, &right.1), order));
        if let Some(wanted) = self.wanted {
            self.held.truncate(wanted);
        }
    }
}

/// A value with every finite number other than a float replaced by the decimal
/// it compares as.
///
/// **This cannot change an order, and the reason is worth stating precisely.**
/// `Number::cmp` decides between two finite numbers by comparing
/// `as_decimal()` of each; `as_decimal()` of a decimal is the decimal itself.
/// So replacing a float by its own projection leaves every comparison with the
/// identical pair of operands it would have computed anyway.
///
/// A float that has **no** decimal projection — infinite, not-a-number, or
/// finite but beyond the range a decimal holds — is left exactly as it is,
/// because those are the three cases the comparator answers *without* a
/// projection, and rewriting them would be changing an answer rather than
/// precomputing one.
///
/// A float is left as it is, too, though it has a projection. Comparing two
/// floats, or a float and another number, is answered without converting either
/// whenever the two are far enough apart that the conversion could not change
/// the answer — which is nearly every pair a sort meets — so converting each
/// key up front would pay for the conversion on every record to save it on the
/// few that are close (an exact nearest-neighbour read spent a sixth of its time
/// here).
///
/// Arrays and objects are walked, since a sort key may be either.
fn projected(value: Value) -> Value {
    match value {
        Value::Number(held @ Number::Float(_)) => Value::Number(held),
        Value::Number(held) => match held.as_decimal() {
            Some(exact) if held.position_is_finite() => Value::Number(Number::Decimal(exact)),
            _ => Value::Number(held),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(projected).collect()),
        Value::Set(items) => Value::Set(items.into_iter().map(projected).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, held)| (name, projected(held)))
                .collect(),
        ),
        other => other,
    }
}

/// The window a `START` and a `LIMIT` ask for.
///
/// Applied **after** ordering, always — including when no `ORDER BY` was
/// written, because otherwise `LIMIT 10` means "the first ten the scan happened
/// to reach", which is a different answer on a replica.
pub(crate) fn bounded(
    records: Vec<(RecordId, Value)>,
    start: Option<u64>,
    limit: Option<u64>,
) -> Vec<(RecordId, Value)> {
    let mut records = records;
    if let Some(start) = start {
        // A start past the end answers with nothing rather than failing: asking
        // for page nine of an eight-page result is a state, not a mistake.
        let skip = usize::try_from(start).unwrap_or(usize::MAX);
        records = records.into_iter().skip(skip).collect();
    }
    if let Some(limit) = limit {
        let keep = usize::try_from(limit).unwrap_or(usize::MAX);
        records.truncate(keep);
    }
    records
}

#[cfg(test)]
mod tests;
