//! Window functions over the records a read keeps (ADR-0134 D13).
//!
//! Computed once the `WHERE` has kept its records and before the read's own
//! order and bound, so a window's value is a fact about every record the
//! statement asked about and never about the page that happens to be answered.
//! The records are already held at this point — a read with a window is a
//! collecting read — so the cost the window adds is its own keys and one sort per
//! window, stated rather than hidden.

use tessari_ql::{Expr, WindowFunction, Windowed};
use tessari_storage::Transaction;
use tessari_types::{Number, RecordId, Value};

use super::Scope;
use crate::accumulate::Accumulator;
use crate::error::Result;
use crate::noticed::Noticed;
use crate::session::Session;

impl Session<'_> {
    /// Each window's value for each record, windows outermost, records in the
    /// order they were given.
    pub(super) async fn windowed(
        &self,
        transaction: &mut Transaction<'_>,
        windows: &[Windowed],
        records: &[(RecordId, Value)],
        noticed: &Noticed,
    ) -> Result<Vec<Vec<Value>>> {
        let mut answered = Vec::with_capacity(windows.len());
        for window in windows {
            answered.push(
                self.one_window(transaction, window, records, noticed)
                    .await?,
            );
        }
        Ok(answered)
    }

    async fn one_window(
        &self,
        transaction: &mut Transaction<'_>,
        window: &Windowed,
        records: &[(RecordId, Value)],
        noticed: &Noticed,
    ) -> Result<Vec<Value>> {
        let partition = self
            .each_record(transaction, &window.partition, records, noticed)
            .await?;
        let order_keys: Vec<Expr> = window
            .order
            .iter()
            .map(|ordering| ordering.key.clone())
            .collect();
        let keys = self
            .each_record(transaction, &order_keys, records, noticed)
            .await?;
        let arguments = self
            .each_record(
                transaction,
                &window
                    .function
                    .expressions()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                records,
                noticed,
            )
            .await?;
        // Partitions together, each in the window's order, ties by identity —
        // the same total order a read's own `ORDER BY` answers in.
        let at = |values: &'_ [Vec<Value>], record: usize| -> Vec<Value> {
            values.get(record).cloned().unwrap_or_default()
        };
        let mut walk: Vec<(Vec<Value>, Vec<Value>, &RecordId, usize)> = records
            .iter()
            .enumerate()
            .map(|(record, (id, _))| (at(&partition, record), at(&keys, record), id, record))
            .collect();
        walk.sort_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| {
                crate::shape::ranked((&left.1, left.2), (&right.1, right.2), &window.order)
            })
        });
        let mut answered = vec![Value::None; records.len()];
        let mut members: Vec<(Vec<Value>, usize)> = Vec::new();
        let mut current: Option<&[Value]> = None;
        for (partition_key, order_key, _, record) in &walk {
            if current.is_some_and(|held| held.cmp(partition_key) != core::cmp::Ordering::Equal) {
                fill(
                    &mut answered,
                    in_partition(window, &members, &arguments)?,
                    &members,
                );
                members.clear();
            }
            current = Some(partition_key.as_slice());
            members.push((order_key.clone(), *record));
        }
        if !members.is_empty() {
            fill(
                &mut answered,
                in_partition(window, &members, &arguments)?,
                &members,
            );
        }
        Ok(answered)
    }

    /// Each expression's value for each record, records outermost.
    async fn each_record(
        &self,
        transaction: &mut Transaction<'_>,
        exprs: &[Expr],
        records: &[(RecordId, Value)],
        noticed: &Noticed,
    ) -> Result<Vec<Vec<Value>>> {
        let mut folded = Vec::with_capacity(exprs.len());
        for expr in exprs {
            folded.push(self.folded(transaction, expr).await?);
        }
        let mut values = Vec::with_capacity(records.len());
        for (id, record) in records {
            let mut one = Vec::with_capacity(folded.len());
            for expr in &folded {
                let scope = Scope::of(record).identified(id).noticing(noticed);
                one.push(self.evaluate_in(transaction, expr, scope).await?);
            }
            values.push(one);
        }
        Ok(values)
    }
}

/// Each member's value written back at its record's position.
fn fill(answered: &mut [Value], values: Vec<Value>, members: &[(Vec<Value>, usize)]) {
    for ((_, record), value) in members.iter().zip(values) {
        if let Some(slot) = answered.get_mut(*record) {
            *slot = value;
        }
    }
}

/// The window's value for each member of one partition, in the walk's order.
///
/// Each member is its order keys and the position of its record among the
/// read's records, which `arguments` is indexed by. The frame of a fold,
/// `first_value` and `last_value` runs from the partition's first record to the
/// current one when the window is ordered, and is the whole partition when it
/// is not.
fn in_partition(
    window: &Windowed,
    members: &[(Vec<Value>, usize)],
    arguments: &[Vec<Value>],
) -> Result<Vec<Value>> {
    let ordered = !window.order.is_empty();
    let argument = |member: Option<&(Vec<Value>, usize)>, position: usize| -> Value {
        member
            .and_then(|(_, record)| arguments.get(*record))
            .and_then(|values| values.get(position))
            .cloned()
            .unwrap_or(Value::None)
    };
    let count = members.len();
    let mut answered = Vec::with_capacity(count);
    match &window.function {
        WindowFunction::RowNumber => {
            for position in 1..=count {
                answered.push(whole(position));
            }
        }
        WindowFunction::Rank | WindowFunction::DenseRank => {
            let dense = matches!(window.function, WindowFunction::DenseRank);
            let (mut rank, mut distinct): (usize, usize) = (1, 1);
            let mut previous: Option<&[Value]> = None;
            for (position, (keys, _)) in (1..).zip(members) {
                if previous.is_some_and(|held| {
                    crate::shape::by_keys(held, keys, &window.order) != core::cmp::Ordering::Equal
                }) {
                    rank = position;
                    distinct = distinct.saturating_add(1);
                }
                previous = Some(keys.as_slice());
                answered.push(whole(if dense { distinct } else { rank }));
            }
        }
        WindowFunction::Lag(neighbour) | WindowFunction::Lead(neighbour) => {
            let behind = matches!(window.function, WindowFunction::Lag(_));
            let offset = usize::try_from(neighbour.offset).unwrap_or(usize::MAX);
            for position in 0..count {
                let other = if behind {
                    position.checked_sub(offset)
                } else {
                    position.checked_add(offset)
                };
                answered.push(match other.and_then(|other| members.get(other)) {
                    Some(other) => argument(Some(other), 0),
                    // The default is the function's second expression, over the
                    // record whose neighbour is missing.
                    None if neighbour.default.is_some() => argument(members.get(position), 1),
                    None => Value::None,
                });
            }
        }
        WindowFunction::FirstValue(_) => {
            answered.resize(count, argument(members.first(), 0));
        }
        WindowFunction::LastValue(_) => {
            for position in 0..count {
                let last = if ordered {
                    members.get(position)
                } else {
                    members.last()
                };
                answered.push(argument(last, 0));
            }
        }
        WindowFunction::Fold { fold, over } => {
            let mut running = Accumulator::for_aggregate(*fold, window.span);
            let offered = |member: &(Vec<Value>, usize)| match over {
                Some(_) => argument(Some(member), 0),
                // `count(*)` counts the records, each one present.
                None => Value::Bool(true),
            };
            for member in members {
                running.offer(&offered(member))?;
                if ordered {
                    answered.push(running.finish()?);
                }
            }
            if !ordered {
                answered.resize(count, running.finish()?);
            }
        }
    }
    Ok(answered)
}

/// The rows with each window's value under its name, beside what the
/// projection answered.
pub(super) fn answering(
    rows: Vec<(RecordId, Value)>,
    windows: &[Windowed],
    values: Vec<Vec<Value>>,
) -> Vec<(RecordId, Value)> {
    if windows.is_empty() {
        return rows;
    }
    let mut columns: Vec<std::vec::IntoIter<Value>> =
        values.into_iter().map(Vec::into_iter).collect();
    rows.into_iter()
        .map(|(id, mut row)| {
            for (window, column) in windows.iter().zip(columns.iter_mut()) {
                if let (Value::Object(fields), Some(value)) = (&mut row, column.next()) {
                    fields.insert(window.name.text.clone(), value);
                }
            }
            (id, row)
        })
        .collect()
}

/// A count of records as the integer the language answers with.
fn whole(count: usize) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}
