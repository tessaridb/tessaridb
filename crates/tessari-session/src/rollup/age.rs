//! Age-triggered re-aggregation — `KEEP RAW FOR 7d ROLL UP BY 1h COMPUTE …`.
//!
//! Past the raw horizon, each finished window's raw points are replaced by
//! one rolled row, computed with the same folds and the same exact-state
//! discipline a `DEFINE ROLLUP` keeps (ADR-0088 §6). The rolled row lives in
//! the series itself, at the window's own place in the time-ordered keyspace,
//! and carries `rolled: true` so the pass never folds its own output.
//!
//! # Where it runs, and why there
//!
//! Housekeeping drives it, like the materialized-view pass it mirrors: a
//! window is rolled **where the range is led**, as one ordinary committed
//! transaction — so a failover mid-pass leaves either the raw window or the
//! rolled row, never half of one, and a follower converges by applying the
//! leader's bytes like any other write. A node that may not write refuses at
//! the commit before anything changes, which the caller reads as "kept where
//! the range is led" rather than as a fault.
//!
//! # Idempotence
//!
//! A window is rolled from its raw points alone, and the pass offers only
//! records without the marker to the folds. A re-run over a rolled window
//! finds the rolled row and nothing raw, writes nothing and deletes nothing —
//! so a pass that crashed after its commit and before its log line costs one
//! quiet re-scan, never a double count.

use tessari_encoding::decode_payload;
use tessari_ql::Span;
use tessari_storage::{
    Catalog, KeepRawDeclaration, RecordAddress, RollupDeclaration, Store, Window,
};
use tessari_types::Value;

use super::maintain::{first_identity, fold_rows, row_identity, row_value};
use crate::error::Result;
use crate::session::Session;

/// What one pass rolled, for the caller's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rolled {
    /// Series that had at least one window rolled.
    pub series: usize,
    /// Windows rolled.
    pub windows: usize,
    /// Raw points replaced.
    pub points: usize,
}

/// The field a rolled row carries so the pass never folds its own output.
pub(crate) const ROLLED: &str = "rolled";

/// Roll every finished, aged window of every series that declared `KEEP RAW`.
///
/// # Errors
///
/// Whatever reading the catalog, scanning a window or a fold refuses, and the
/// commit's own refusals — of which the caller tells "not led here" apart from
/// a fault, as it does for the materialized pass.
pub async fn roll_up_aged_series(store: &Store) -> Result<Rolled> {
    let mut declared = Vec::new();
    {
        let mut transaction = store.begin().await?;
        let catalog = Catalog::new(&mut transaction);
        for namespace in catalog.namespaces().await? {
            for database in catalog.databases_in(namespace.id).await? {
                for table in catalog.tables_in(namespace.id, database.id).await? {
                    if let tessari_storage::TableKind::Series(series) = &table.kind
                        && let Some(keep_raw) = &series.keep_raw
                        && let Some(time) = &series.time
                    {
                        declared.push((
                            table.namespace,
                            table.database,
                            table.id,
                            time.clone(),
                            keep_raw.clone(),
                        ));
                    }
                }
            }
        }
        transaction.rollback();
    }
    let mut rolled = Rolled::default();
    for (namespace, database, table, time, keep_raw) in declared {
        let done = roll_one(store, (namespace, database, table), &time, &keep_raw).await?;
        if done.windows > 0 {
            rolled.series = rolled.series.saturating_add(1);
        }
        rolled.windows = rolled.windows.saturating_add(done.windows);
        rolled.points = rolled.points.saturating_add(done.points);
    }
    Ok(rolled)
}

/// One series' pass: each finished window below the horizon, oldest first,
/// one committed transaction each.
async fn roll_one(
    store: &Store,
    (namespace, database, table): (
        tessari_types::NamespaceId,
        tessari_types::DatabaseId,
        tessari_types::TableId,
    ),
    time: &str,
    keep_raw: &KeepRawDeclaration,
) -> Result<Rolled> {
    let span_seconds = keep_raw.window.seconds().max(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    // The horizon window: everything before its start is old enough to roll.
    // A window still collecting raw points is left alone whatever its age,
    // because rolling half a window would freeze a partial answer.
    let horizon = now
        .saturating_sub(keep_raw.raw_for.seconds())
        .div_euclid(span_seconds)
        .saturating_mul(span_seconds);
    let mut rolled = Rolled::default();
    let mut from = first_identity(0);
    loop {
        let to = first_identity(horizon);
        let mut transaction = store.begin().await?;
        let found = transaction
            .records_between(
                namespace,
                database,
                table,
                Window {
                    from: Some(&from),
                    to: Some((&to, false)),
                },
                None,
                usize::MAX,
            )
            .await?;
        // The oldest raw point decides which window rolls next; rolled rows
        // are stepped over, so a re-run walks past its own output.
        let mut oldest: Option<i64> = None;
        for (_, payload) in &found {
            let Ok(record) = decode_payload(payload) else {
                continue;
            };
            if is_rolled(&record) {
                continue;
            }
            if let Some(at) = instant_of(&record, time) {
                let window = at.div_euclid(span_seconds).saturating_mul(span_seconds);
                oldest = Some(oldest.map_or(window, |held: i64| held.min(window)));
            }
        }
        let Some(window) = oldest else {
            transaction.rollback();
            break;
        };
        let window_from = first_identity(window);
        let window_to = first_identity(window.saturating_add(span_seconds));
        let in_window: Vec<(tessari_types::RecordId, Value)> = found
            .iter()
            .filter(|(id, _)| *id >= window_from && *id < window_to)
            .filter_map(|(id, payload)| {
                let record = decode_payload(payload).ok()?;
                (!is_rolled(&record)).then_some((id.clone(), record))
            })
            .collect();
        if in_window.is_empty() {
            transaction.rollback();
            from = window_to;
            continue;
        }
        let declaration = RollupDeclaration {
            table,
            window: keep_raw.window,
            by: None,
            computes: keep_raw.computes.clone(),
        };
        let records: Vec<Value> = in_window.iter().map(|(_, record)| record.clone()).collect();
        let rows = fold_rows(&declaration, time, &records)?;
        let session = Session::new(store);
        for row in rows.into_values() {
            let mut value = row_value(row, &declaration);
            if let Value::Object(fields) = &mut value {
                fields.insert(ROLLED.to_owned(), Value::Bool(true));
            }
            session
                .put_engine_record(
                    &mut transaction,
                    RecordAddress::new(
                        namespace,
                        database,
                        table,
                        row_identity(window, &Value::None),
                    ),
                    value,
                    Span::new(0, 0),
                )
                .await?;
        }
        for (id, _) in &in_window {
            session
                .delete_record(
                    &mut transaction,
                    RecordAddress::new(namespace, database, table, id.clone()),
                    Span::new(0, 0),
                )
                .await?;
        }
        transaction.commit().await?;
        rolled.windows = rolled.windows.saturating_add(1);
        rolled.points = rolled.points.saturating_add(in_window.len());
        from = window_to;
    }
    Ok(rolled)
}

/// Whether the record is a rolled row rather than a raw point.
fn is_rolled(record: &Value) -> bool {
    matches!(record, Value::Object(fields)
        if fields.get(ROLLED) == Some(&Value::Bool(true)))
}

/// The second a raw point was observed at, by the series' time field.
fn instant_of(record: &Value, time: &str) -> Option<i64> {
    match record {
        Value::Object(fields) => match fields.get(time) {
            Some(Value::Datetime(at)) => Some(at.seconds()),
            _ => None,
        },
        _ => None,
    }
}
