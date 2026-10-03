//! Rollups: per-window aggregates of an event-time series, kept exact by the
//! transaction that writes the raw record (ADR-0088 §6 and its W6 amendment).
//!
//! # Where a rollup lives
//!
//! The rollup is its own event-time series, ordered by `window`, so it has a
//! floor, `LATEST BY`, `ASOF` and `FILL` like any other. One row per window and
//! key; its identity is the window's UUID version 7 with the bits below the time
//! taken from a digest of the key, so the row a write has to touch is found by
//! address rather than by search.
//!
//! # Why declaring one is two commits
//!
//! The store detects write–write conflicts at commit. The declaration commits
//! alone; every raw write to an event-time series guards its table's catalog
//! entry, so one that straddles the declaration retries and sees it. The
//! backfill then commits separately, writing each row whole from the raw data:
//! a raw write landing on a row it also writes makes one of them retry, and a
//! window empty at its snapshot is created by the raw write alone.

mod maintain;

use std::collections::BTreeMap;

use tessari_encoding::decode_payload;
use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{
    Catalog, RecordAddress, RollupCompute, RollupDeclaration, RollupFold, SeriesDeclaration,
    TableKind, TableShape, Transaction,
};
use tessari_types::{Duration, IdentityKind, TableId, Value};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

pub(crate) use maintain::{Row, fold_rows, row_identity};

/// The field a rollup row carries its window's first instant in.
pub(crate) const WINDOW_FIELD: &str = "window";

/// A `DEFINE ROLLUP` statement's parts, as the parser left them.
pub(crate) struct Declared<'s> {
    pub(crate) name: &'s Name,
    pub(crate) source: &'s Name,
    pub(crate) window: Duration,
    pub(crate) by: Option<&'s Name>,
    pub(crate) computes: &'s [(Name, Option<Name>, Name)],
    pub(crate) retain: Duration,
    pub(crate) if_not_exists: bool,
}

impl Session<'_> {
    /// The first of `DEFINE ROLLUP`'s two commits: the rollup's table and its
    /// place in the source's declaration. [`Session::backfill_rollup`] is the
    /// second.
    ///
    /// # Errors
    ///
    /// [`Error::RollupNeedsSeries`], [`Error::RollupFold`], [`Error::RollupWindow`]
    /// and whatever defining the table refuses.
    pub(crate) fn define_rollup(
        &self,
        transaction: &mut Transaction<'_>,
        declared: &Declared<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let (context, source, mut series) = self.rollup_source(transaction, declared.source)?;
        if declared.window.nanos() != 0 || declared.window.seconds() <= 0 {
            return Err(Error::RollupWindow { span });
        }
        let computes = computes_of(declared.computes)?;
        let existed = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &declared.name.text)?
            .is_some();
        self.define_table(
            transaction,
            declared.name,
            TableShape {
                schemafull: false,
                kind: TableKind::Series(SeriesDeclaration {
                    retain: declared.retain,
                    time: Some(WINDOW_FIELD.to_owned()),
                    rollups: Vec::new(),
                    rollup_of: Some(source),
                }),
                identity: IdentityKind::Uuid,
                graph: None,
                conflict: None,
                split: Vec::new(),
                partition: None,
                spread: false,
            },
            declared.if_not_exists,
            span,
        )?;
        if existed {
            return Ok(Outcome::Done);
        }
        let table = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &declared.name.text)?
            .ok_or_else(|| Error::RollupNeedsSeries {
                name: declared.name.text.clone(),
                span,
            })?;
        series.rollups.push(RollupDeclaration {
            table,
            window: declared.window,
            by: declared.by.map(|by| by.text.clone()),
            computes,
        });
        Catalog::new(transaction).set_series(source, series)?;
        Ok(Outcome::Done)
    }

    /// The second commit: every row of the rollup, written whole from the raw
    /// series at this transaction's snapshot.
    ///
    /// # Errors
    ///
    /// Whatever reading the series or writing a row refuses.
    pub(crate) fn backfill_rollup(
        &self,
        transaction: &mut Transaction<'_>,
        source: &Name,
        name: &Name,
    ) -> Result<()> {
        let (context, source, series) = self.rollup_source(transaction, source)?;
        let Some(table) =
            Catalog::new(transaction).table_id(context.namespace, context.database, &name.text)?
        else {
            return Ok(());
        };
        let Some(rollup) = series.rollups.iter().find(|held| held.table == table) else {
            return Ok(());
        };
        let Some(time) = series.time.as_deref() else {
            return Ok(());
        };
        let raw = transaction.scan_table(context.namespace, context.database, source)?;
        let mut records = Vec::with_capacity(raw.len());
        for (_, payload) in raw {
            records.push(decode_payload(&payload).map_err(tessari_storage::Error::from)?);
        }
        for (key, row) in fold_rows(rollup, time, &records)? {
            let id = row_identity(key.0, &key.1);
            let address = RecordAddress::new(context.namespace, context.database, table, id);
            self.put_engine_record(transaction, address, row.into_value(rollup), name.span)?;
        }
        Ok(())
    }

    /// `DROP ROLLUP`: out of its source's declaration, then the table.
    ///
    /// # Errors
    ///
    /// [`Error::Unknown`] when no rollup has that name.
    pub(crate) fn drop_rollup(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "rollup",
            name: name.text.clone(),
            span,
        };
        let table = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let Some(source) = Catalog::new(transaction)
            .table(table)?
            .and_then(|definition| match definition.kind {
                TableKind::Series(declared) => declared.rollup_of,
                _ => None,
            })
        else {
            return Err(unknown());
        };
        if let Some(definition) = Catalog::new(transaction).table(source)?
            && let TableKind::Series(mut series) = definition.kind
        {
            series.rollups.retain(|held| held.table != table);
            Catalog::new(transaction).set_series(source, series)?;
        }
        Catalog::new(transaction).drop_table(table)?;
        Ok(Outcome::Done)
    }

    /// The source a rollup folds: an event-time series that is not itself a
    /// rollup, with its declaration.
    fn rollup_source(
        &self,
        transaction: &mut Transaction<'_>,
        source: &Name,
    ) -> Result<(Context, TableId, SeriesDeclaration)> {
        let table = TableRef {
            database: None,
            name: source.clone(),
            span: source.span,
        };
        let (context, id) = self.resolve_table(transaction, &table)?;
        match Catalog::new(transaction).table(id)?.map(|found| found.kind) {
            Some(TableKind::Series(series))
                if series.time.is_some() && series.rollup_of.is_none() =>
            {
                Ok((context, id, series))
            }
            _ => Err(Error::RollupNeedsSeries {
                name: source.text.clone(),
                span: source.span,
            }),
        }
    }
}

/// The written `COMPUTE` list, checked against the folds a rollup keeps.
fn computes_of(written: &[(Name, Option<Name>, Name)]) -> Result<Vec<RollupCompute>> {
    let mut computes = Vec::with_capacity(written.len());
    let mut names: BTreeMap<&str, ()> = BTreeMap::new();
    for (fold, of, name) in written {
        let refused = || Error::RollupFold {
            fold: fold.text.clone(),
            span: fold.span,
        };
        let parsed = RollupFold::parse(&fold.text).ok_or_else(refused)?;
        // Only `count` may fold over the records themselves.
        if of.is_none() && parsed != RollupFold::Count {
            return Err(refused());
        }
        if name.text == WINDOW_FIELD || names.insert(&name.text, ()).is_some() {
            return Err(Error::RollupFold {
                fold: name.text.clone(),
                span: name.span,
            });
        }
        computes.push(RollupCompute {
            name: name.text.clone(),
            fold: parsed,
            of: of.as_ref().map(|field| field.text.clone()),
        });
    }
    Ok(computes)
}

impl Row {
    /// The row as the record the rollup table holds.
    pub(crate) fn into_value(self, rollup: &RollupDeclaration) -> Value {
        maintain::row_value(self, rollup)
    }
}
