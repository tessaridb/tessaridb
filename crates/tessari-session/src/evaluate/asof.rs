//! `ASOF JOIN`: each left record paired with the newest right record at or
//! before its own time, one row in and one row out (ADR-0088 §4).
//!
//! # Why the left record is kept when nothing matches
//!
//! The question an as-of join answers is "what was the other series saying at
//! this moment?", asked of every left record. A left record before the right
//! series' first sample has an answer — nothing yet — and dropping it would
//! answer a different question about fewer moments. So the contract is one row
//! per left record, with the right side absent where there was nothing; it is a
//! different word from `JOIN`, whose inner meaning is unchanged.
//!
//! # How a match is found
//!
//! Both sides are event-time series, so a record's identity is its time. The
//! right side is read once, in key order — which is time order — and filed by
//! its key; each left record then takes the last right identity at or below the
//! greatest one its own instant allows, by binary search. `n + m` reads and
//! `n log m` comparisons, the shape an ordinary join over an unindexed key has.

use std::collections::BTreeMap;

use tessari_ql::{Expr, FieldPath, JoinSide, Select};
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

use crate::condition::boolean;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::AccessPath;
use crate::plan::Plan;
use crate::session::Session;

use super::{Joined, Part, Reporting, Scope, shown};

/// One side of an as-of join, read: its records in time order and the field
/// its time is in.
struct Side {
    records: Vec<(RecordId, Value)>,
    time: String,
}

impl Session<'_> {
    /// Read `side` whole, in time order, refusing it unless it is a series
    /// ordered by event time.
    fn asof_side(
        &self,
        transaction: &mut Transaction<'_>,
        side: &JoinSide,
        select: &Select,
    ) -> Result<(Side, Context, tessari_types::TableId)> {
        let JoinSide::Table { table, .. } = side else {
            return Err(Error::AsofNeedsTime {
                side: side.name().to_owned(),
                span: select.span,
            });
        };
        let (context, id) = self.resolve_table(transaction, table)?;
        let Some(time) = transaction.series_time(context.namespace, id)? else {
            return Err(Error::AsofNeedsTime {
                side: side.name().to_owned(),
                span: select.span,
            });
        };
        self.refuse_reading_a_part(transaction, id, Part::Whole)?;
        let visible = self.visible_in(transaction, id)?;
        let found = transaction.scan_table(context.namespace, context.database, id)?;
        let records = self.records_of(found, &visible)?;
        Ok((Side { records, time }, context, id))
    }

    /// Pair every left record with the newest right record at or before it
    /// whose key matches.
    ///
    /// # Errors
    ///
    /// [`Error::AsofNeedsTime`] for a side that is not an event-time series, and
    /// whatever reading either side or testing the condition refuses.
    pub(super) fn asof_join(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        (left, right): (&JoinSide, &JoinSide),
        (left_key, right_key): (&FieldPath, &FieldPath),
        condition: Option<&Expr>,
        reporting: Reporting<'_>,
    ) -> Result<Joined> {
        let (far, _, _) = self.asof_side(transaction, right, select)?;
        let (near, _, left_table) = self.asof_side(transaction, left, select)?;
        let searched = self.searched_for(transaction, left_table, &shown(select))?;

        // Filed by key, each list already in time order because the scan was.
        let mut by_key: BTreeMap<Value, Vec<(RecordId, Value)>> = BTreeMap::new();
        for (id, record) in far.records {
            if let Some(key) = right_key.path.resolve(&record).cloned() {
                by_key.entry(key).or_default().push((id, record));
            }
        }

        let (left_name, right_name) = (left.name().to_owned(), right.name().to_owned());
        let mut rows = Vec::with_capacity(near.records.len());
        for (id, record) in near.records {
            let matched = as_of(&record, &near.time, left_key, &by_key);
            let mut fields = BTreeMap::from([(left_name.clone(), record)]);
            if let Some(found) = matched {
                fields.insert(right_name.clone(), found);
            }
            let row = Value::Object(fields);
            if let Some(condition) = condition {
                let held = self.evaluate_in(
                    transaction,
                    condition,
                    Scope::searching(&row, &searched).noticing(reporting.noticed),
                )?;
                if !boolean(&held, condition.span)? {
                    continue;
                }
            }
            rows.push((id, row));
        }
        let plan = Plan {
            shape: Some("asof"),
            ..Plan::new(AccessPath::Join)
        };
        Ok((rows, plan, searched))
    }
}

/// The newest right record at or before `record`'s time with its key.
fn as_of(
    record: &Value,
    time: &str,
    left_key: &FieldPath,
    by_key: &BTreeMap<Value, Vec<(RecordId, Value)>>,
) -> Option<Value> {
    let key = left_key.path.resolve(record)?;
    let Value::Object(fields) = record else {
        return None;
    };
    let Some(Value::Datetime(at)) = fields.get(time) else {
        return None;
    };
    let bound = crate::series::last_identity_at(*at)?;
    let candidates = by_key.get(key)?;
    let upto = candidates.partition_point(|(id, _)| *id <= bound);
    upto.checked_sub(1)
        .and_then(|at| candidates.get(at))
        .map(|(_, found)| found.clone())
}
