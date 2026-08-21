//! Giving an answer the shape the statement asked for.
//!
//! Sorting and bounding, and nothing else. Separate from evaluating an
//! expression and from reading records because it is a third question — *which
//! of these, in what order* — and because the two rules it holds are the ones a
//! reader comes looking for.

use bgv_db_ql::Ordering;
use bgv_db_types::{RecordId, Value};

/// The records in the order the statement asked for.
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
/// appears, which is the shape this store keeps refusing.
pub(crate) fn sorted(
    records: Vec<(RecordId, Value)>,
    order: &[Ordering],
) -> Vec<(RecordId, Value)> {
    if order.is_empty() {
        return records;
    }
    // The keys are read once per record rather than inside the comparison,
    // because a sort compares a record many times and a route is walked
    // every time it is asked for.
    let mut keyed = Vec::with_capacity(records.len());
    for (id, record) in records {
        let mut keys = Vec::with_capacity(order.len());
        for key in order {
            keys.push(
                key.key
                    .path
                    .resolve(&record)
                    .cloned()
                    .unwrap_or(Value::None),
            );
        }
        keyed.push((keys, id, record));
    }

    keyed.sort_by(|left, right| {
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
        left.1.cmp(&right.1)
    });
    keyed
        .into_iter()
        .map(|(_, id, record)| (id, record))
        .collect()
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
