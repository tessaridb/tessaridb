//! Keeping a rollup exact as its series is written (ADR-0088 §6).
//!
//! An insert folds the new record into its row — `count`, `sum`, `min` and
//! `max` all merge from the row alone. A replacement or a deletion recomputes
//! the row from the raw window, because a `min` cannot be un-folded. Either way
//! the row is written in the transaction that wrote the raw record, so there is
//! no moment at which the two disagree.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use tessari_encoding::{decode_payload, encode_payload};
use tessari_ql::{Aggregate, Span};
use tessari_storage::{
    Catalog, RecordAddress, RollupDeclaration, RollupFold, TableKind, Transaction, Window,
};
use tessari_types::{Datetime, Number, RecordId, Value};

use super::WINDOW_FIELD;
use crate::accumulate::Accumulator;
use crate::error::{Error, Result};
use crate::session::Session;

/// One row being computed: its window's first second, its key, its values.
pub(crate) struct Row {
    window: i64,
    key: Value,
    values: BTreeMap<String, Accumulator>,
}

/// What a write to an event-time series with rollups needs after it lands.
pub(crate) struct Held {
    rollups: Vec<RollupDeclaration>,
    time: String,
    old: Option<Value>,
}

/// The first second of the window `at` falls in.
fn window_start(at: Datetime, seconds: i64) -> i64 {
    at.seconds().div_euclid(seconds).saturating_mul(seconds)
}

/// A rollup's fold, as the accumulator that computes it.
const fn aggregate_of(fold: RollupFold) -> Aggregate {
    match fold {
        RollupFold::Count => Aggregate::Count,
        RollupFold::Sum => Aggregate::Sum,
        RollupFold::Min => Aggregate::Min,
        RollupFold::Max => Aggregate::Max,
    }
}

/// The row a record belongs to: its window and its key, or `None` for a record
/// with no instant (which an event-time series does not hold).
fn place(rollup: &RollupDeclaration, time: &str, record: &Value) -> Option<(i64, Value)> {
    let Value::Object(fields) = record else {
        return None;
    };
    let Some(Value::Datetime(at)) = fields.get(time) else {
        return None;
    };
    let key = rollup
        .by
        .as_ref()
        .and_then(|by| fields.get(by).cloned())
        .unwrap_or(Value::None);
    Some((window_start(*at, rollup.window.seconds()), key))
}

impl Row {
    fn new(rollup: &RollupDeclaration, window: i64, key: Value) -> Self {
        let values = rollup
            .computes
            .iter()
            .map(|compute| {
                (
                    compute.name.clone(),
                    Accumulator::for_aggregate(aggregate_of(compute.fold), Span::new(0, 0)),
                )
            })
            .collect();
        Self {
            window,
            key,
            values,
        }
    }

    /// Offer one raw record to every value of the row.
    fn offer(&mut self, rollup: &RollupDeclaration, record: &Value) -> Result<()> {
        for compute in &rollup.computes {
            let offered = match (&compute.of, record) {
                (None, _) => Value::Bool(true),
                (Some(of), Value::Object(fields)) => fields.get(of).cloned().unwrap_or(Value::None),
                (Some(_), _) => Value::None,
            };
            if let Some(accumulator) = self.values.get_mut(&compute.name) {
                accumulator.offer(&offered)?;
            }
        }
        Ok(())
    }
}

/// Every row the records fold into, by window and key.
///
/// # Errors
///
/// Whatever a fold refuses — a `sum` over a value that is not a number.
pub(crate) fn fold_rows(
    rollup: &RollupDeclaration,
    time: &str,
    records: &[Value],
) -> Result<BTreeMap<(i64, Value), Row>> {
    let mut rows: BTreeMap<(i64, Value), Row> = BTreeMap::new();
    for record in records {
        let Some((window, key)) = place(rollup, time, record) else {
            continue;
        };
        rows.entry((window, key.clone()))
            .or_insert_with(|| Row::new(rollup, window, key))
            .offer(rollup, record)?;
    }
    Ok(rows)
}

/// The record a row is kept as.
pub(super) fn row_value(row: Row, rollup: &RollupDeclaration) -> Value {
    let mut fields = BTreeMap::from([(
        WINDOW_FIELD.to_owned(),
        Value::Datetime(Datetime::from_seconds(row.window)),
    )]);
    if let Some(by) = &rollup.by
        && row.key.is_present()
    {
        fields.insert(by.clone(), row.key);
    }
    for (name, accumulator) in row.values {
        if let Ok(value) = accumulator.finish()
            && value.is_present()
        {
            fields.insert(name, value);
        }
    }
    Value::Object(fields)
}

/// A row's identity: the window's UUID version 7 with the bits below its time
/// taken from a digest of the key, so a write finds its row by address.
#[must_use]
pub(crate) fn row_identity(window: i64, key: &Value) -> RecordId {
    let millis = u64::try_from(window)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .unwrap_or(0);
    let digest = Sha256::digest(encode_payload(key).into_bytes());
    let mut bytes = [0_u8; 16];
    let [_, _, t0, t1, t2, t3, t4, t5] = millis.to_be_bytes();
    bytes[..6].copy_from_slice(&[t0, t1, t2, t3, t4, t5]);
    bytes[6] = 0x70;
    bytes[8..].copy_from_slice(&digest[..8]);
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::Uuid(bytes)
}

/// The smallest identity an event-time series holds at or after `second`.
fn first_identity(second: i64) -> RecordId {
    let millis = u64::try_from(second)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .unwrap_or(0);
    let mut bytes = [0_u8; 16];
    let [_, _, t0, t1, t2, t3, t4, t5] = millis.to_be_bytes();
    bytes[..6].copy_from_slice(&[t0, t1, t2, t3, t4, t5]);
    RecordId::Uuid(bytes)
}

impl Session<'_> {
    /// Before a caller writes or deletes `address`: refuse it on a rollup, guard
    /// an event-time series' declaration, and read what the write replaces when
    /// the series has rollups to keep.
    ///
    /// # Errors
    ///
    /// [`Error::RollupIsDerived`] for a rollup's own table.
    pub(crate) fn rollups_before(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        span: Span,
    ) -> Result<Option<Held>> {
        let Some(time) = transaction.series_time(address.namespace, address.table)? else {
            return Ok(None);
        };
        let Some(definition) = Catalog::new(transaction).table(address.table)? else {
            return Ok(None);
        };
        let TableKind::Series(series) = definition.kind else {
            return Ok(None);
        };
        if series.rollup_of.is_some() {
            return Err(Error::RollupIsDerived {
                table: definition.name,
                span,
            });
        }
        transaction.guard_table_entry(address.table);
        if series.rollups.is_empty() {
            return Ok(None);
        }
        let old = match transaction.get(address)? {
            Some(bytes) => Some(decode_payload(&bytes).map_err(tessari_storage::Error::from)?),
            None => None,
        };
        Ok(Some(Held {
            rollups: series.rollups,
            time,
            old,
        }))
    }

    /// After the write or delete landed: bring every rollup's row in line.
    ///
    /// # Errors
    ///
    /// [`Error::RollupKeyCollision`] when two keys share a row identity, and
    /// whatever reading the raw window or a fold refuses.
    pub(crate) fn rollups_after(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        held: Held,
        new: Option<&Value>,
        span: Span,
    ) -> Result<()> {
        for rollup in &held.rollups {
            let rows = RecordAddress::new(
                address.namespace,
                address.database,
                rollup.table,
                RecordId::Int(0),
            );
            match (&held.old, new) {
                (None, Some(record)) => {
                    let Some((window, key)) = place(rollup, &held.time, record) else {
                        continue;
                    };
                    let row = self.stored_row(transaction, &rows, rollup, window, &key, span)?;
                    let mut row = match row {
                        Some(existing) => existing,
                        None => Row::new(rollup, window, key),
                    };
                    row.offer(rollup, record)?;
                    self.keep_row(transaction, &rows, rollup, row, span)?;
                }
                (old, new) => {
                    let mut touched = BTreeMap::new();
                    for record in [old.as_ref(), new].into_iter().flatten() {
                        if let Some(place) = place(rollup, &held.time, record) {
                            touched.insert(place, ());
                        }
                    }
                    for (window, key) in touched.into_keys() {
                        self.recompute_row(
                            transaction,
                            (address, &rows),
                            rollup,
                            &held.time,
                            (window, key),
                            span,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The row as stored, rebuilt for folding into, or `None` when there is none.
    fn stored_row(
        &self,
        transaction: &mut Transaction<'_>,
        rows: &RecordAddress,
        rollup: &RollupDeclaration,
        window: i64,
        key: &Value,
        span: Span,
    ) -> Result<Option<Row>> {
        let at = RecordAddress::new(
            rows.namespace,
            rows.database,
            rows.table,
            row_identity(window, key),
        );
        let Some(bytes) = transaction.get(&at)? else {
            return Ok(None);
        };
        let Value::Object(fields) = decode_payload(&bytes).map_err(tessari_storage::Error::from)?
        else {
            return Ok(None);
        };
        let held_key = rollup
            .by
            .as_ref()
            .and_then(|by| fields.get(by).cloned())
            .unwrap_or(Value::None);
        if &held_key != key {
            return Err(Error::RollupKeyCollision { span });
        }
        // Each kept value re-enters its accumulator as the one value folded so
        // far — a count as that many, the rest as themselves.
        let mut row = Row::new(rollup, window, key.clone());
        for compute in &rollup.computes {
            let Some(accumulator) = row.values.get_mut(&compute.name) else {
                continue;
            };
            let kept = fields.get(&compute.name).cloned().unwrap_or(Value::None);
            if compute.fold == RollupFold::Count {
                let seen = match kept {
                    Value::Number(Number::Integer(held)) => u64::try_from(held).unwrap_or(0),
                    _ => 0,
                };
                *accumulator = Accumulator::Count { seen };
            } else {
                accumulator.offer(&kept)?;
            }
        }
        Ok(Some(row))
    }

    /// Rebuild one row from the raw window it summarises, or remove it.
    fn recompute_row(
        &self,
        transaction: &mut Transaction<'_>,
        (raw, rows): (&RecordAddress, &RecordAddress),
        rollup: &RollupDeclaration,
        time: &str,
        (window, key): (i64, Value),
        span: Span,
    ) -> Result<()> {
        let from = first_identity(window);
        let to = first_identity(window.saturating_add(rollup.window.seconds()));
        let found = transaction.records_between(
            raw.namespace,
            raw.database,
            raw.table,
            Window {
                from: Some(&from),
                to: Some((&to, false)),
            },
            None,
            usize::MAX,
        )?;
        let mut records = Vec::with_capacity(found.len());
        for (_, bytes) in found {
            let record = decode_payload(&bytes).map_err(tessari_storage::Error::from)?;
            if place(rollup, time, &record).is_some_and(|(_, held)| held == key) {
                records.push(record);
            }
        }
        let at = RecordAddress::new(
            rows.namespace,
            rows.database,
            rows.table,
            row_identity(window, &key),
        );
        match fold_rows(rollup, time, &records)?.remove(&(window, key)) {
            Some(row) => self.keep_row(transaction, rows, rollup, row, span),
            None => {
                transaction.delete(at);
                Ok(())
            }
        }
    }

    /// A caller's delete of one record, keeping the series' rollups: every
    /// `DELETE` that can reach a series goes through here (ADR-0088 §6).
    ///
    /// # Errors
    ///
    /// [`Error::RollupIsDerived`] on a rollup's own table, and whatever
    /// recomputing a row refuses.
    pub(crate) fn delete_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        span: Span,
    ) -> Result<()> {
        // Events see the record that was there, and run after it is gone
        // (ADR-0110).
        let events = self.events_before(transaction, &address)?;
        let evented = events.as_ref().map(|_| address.clone());
        match self.rollups_before(transaction, &address, span)? {
            None => transaction.delete(address),
            Some(held) => {
                transaction.delete(address.clone());
                self.rollups_after(transaction, &address, held, None, span)?;
            }
        }
        if let (Some(pending), Some(at)) = (events, evented) {
            self.events_after(transaction, &at, pending, None)?;
        }
        Ok(())
    }

    /// Write a row whole, as the engine rather than as a caller.
    fn keep_row(
        &self,
        transaction: &mut Transaction<'_>,
        rows: &RecordAddress,
        rollup: &RollupDeclaration,
        row: Row,
        span: Span,
    ) -> Result<()> {
        let at = RecordAddress::new(
            rows.namespace,
            rows.database,
            rows.table,
            row_identity(row.window, &row.key),
        );
        self.put_engine_record(transaction, at, row_value(row, rollup), span)
    }
}
