//! A sketch fold over a rollup's sketch column merges the states kept beside
//! its rows (ADR-0122 C5).
//!
//! The row answers the column with an estimate, and estimates do not add: two
//! hours of 1 000 distinct users each are not 2 000. So `approx_distinct(users)`
//! over the rollup folds each row's **state** instead, read from beside the row.
//!
//! The rollup's declaration decides which columns those are — a column the
//! catalog says holds an `approx_distinct` of the same fold — so a column that
//! merely holds bytes shaped like a sketch is never read as one. A row whose
//! column the reader may not see is skipped exactly as its value would be.

use tessari_ql::{ExprKind, Select, Source, Span};
use tessari_storage::{Catalog, RollupDeclaration, TableKind, Transaction};
use tessari_types::Value;

use super::WINDOW_FIELD;
use super::maintain::sketches::{is_sketch, state_key};
use crate::accumulate::Accumulator;
use crate::error::Result;
use crate::session::Session;

/// The rollup a read folds and, per fold occurrence in walk order, the sketch
/// column it merges.
pub(crate) struct Merging {
    rollup: RollupDeclaration,
    columns: Vec<Option<String>>,
}

impl Session<'_> {
    /// What a grouping read over a rollup merges, when it folds a sketch
    /// column with its own fold; `None` for every other read.
    ///
    /// # Errors
    ///
    /// A catalog that cannot be read.
    pub(crate) fn rollup_merging(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<Merging>> {
        if !crate::evaluate::groups(select) {
            return Ok(None);
        }
        let table = match &select.from {
            Source::Table(table) | Source::Where { table, .. } => table,
            _ => return Ok(None),
        };
        // An unknown table is the source's to refuse, in its own words.
        let Ok((_, id)) = self.resolve_table(transaction, table) else {
            return Ok(None);
        };
        let Some(TableKind::Series(series)) =
            Catalog::new(transaction).table(id)?.map(|found| found.kind)
        else {
            return Ok(None);
        };
        let Some(source) = series.rollup_of else {
            return Ok(None);
        };
        let Some(TableKind::Series(raw)) = Catalog::new(transaction)
            .table(source)?
            .map(|found| found.kind)
        else {
            return Ok(None);
        };
        let Some(rollup) = raw.rollups.into_iter().find(|rollup| rollup.table == id) else {
            return Ok(None);
        };
        let columns: Vec<Option<String>> =
            crate::aggregate::occurrences(select.projection.written())
                .into_iter()
                .flatten()
                .map(|fold| sketch_column(&rollup, &fold.kind))
                .collect();
        Ok(columns
            .iter()
            .any(Option::is_some)
            .then_some(Merging { rollup, columns }))
    }
}

/// The sketch column a fold occurrence merges: a bare field the rollup keeps
/// a sketch of with this very fold.
fn sketch_column(rollup: &RollupDeclaration, kind: &ExprKind) -> Option<String> {
    let ExprKind::Fold {
        fold,
        over: Some(over),
        ..
    } = kind
    else {
        return None;
    };
    let ExprKind::Path(field) = &over.kind else {
        return None;
    };
    if !field.path.steps().is_empty() {
        return None;
    }
    rollup
        .computes
        .iter()
        .find(|compute| {
            compute.name == field.path.root()
                && is_sketch(compute.fold)
                && compute.fold.spelling() == fold.spelling()
        })
        .map(|compute| compute.name.clone())
}

impl Merging {
    /// The sketch column the occurrence at `index`, in walk order, merges.
    pub(crate) fn column(&self, index: usize) -> Option<&str> {
        self.columns.get(index)?.as_deref()
    }

    /// Merge into `accumulator` the state kept beside the row `record` is,
    /// for `column`; a row whose column is absent — never written, or not
    /// visible to the reader — contributes nothing.
    ///
    /// # Errors
    ///
    /// [`crate::Error::NotSummable`] for a row whose sketch is not kept, and
    /// whatever the rank or a read of the store refuses.
    pub(crate) fn merge_row(
        &self,
        transaction: &mut Transaction<'_>,
        accumulator: &mut Accumulator,
        (record, column): (&Value, &str),
        rank: Option<&Value>,
        span: Span,
    ) -> Result<()> {
        let Value::Object(fields) = record else {
            return Ok(());
        };
        if !fields.get(column).is_some_and(Value::is_present) {
            return Ok(());
        }
        let window = match fields.get(WINDOW_FIELD) {
            Some(Value::Datetime(at)) => at.seconds(),
            _ => 0,
        };
        let key = self
            .rollup
            .by
            .as_ref()
            .and_then(|by| fields.get(by).cloned())
            .unwrap_or(Value::None);
        let kept = transaction.rollup_state(self.rollup.table, &state_key(window, &key, column))?;
        accumulator.merge_kept(kept.as_ref(), rank, span)
    }
}
