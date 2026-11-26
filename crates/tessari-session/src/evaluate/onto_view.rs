//! Answering a grouped read from a kept view's stored rows (G102 S12 v1).
//!
//! A Grouped materialized view holds one row per group of its base table,
//! each carrying the group's key values and its folds. A read that groups the
//! same table by a **subset** of the view's keys, folding only what the view
//! already folded, is answered by re-folding those rows — `sum` of the stored
//! sums, `min` of the stored minima, a stored count summed — which is exact
//! for the four distributive folds and for nothing else.
//!
//! # The rewrite is an access path, not an expansion
//!
//! It runs inside the read, after the statement was authorized against the
//! table it names, and answers what that statement would have answered — the
//! differential suite pins the equality and the note reports the path. What
//! the ordering rule of `crate::view` protects is untouched: nothing here
//! lets a caller reach a table their grant refuses, because a caller whose
//! grant narrows the base table's fields or rows is never rewritten — the
//! stored rows were computed for everybody and would disclose what the grant
//! hides.
//!
//! # Every condition, or no rewrite
//!
//! The gate declines silently and the ordinary read runs; a wrong rewrite is
//! a plausible wrong number, so a shape the conditions do not cover is never
//! taken. The conditions: one plain base table; only the four distributive
//! folds, bare, over expressions the view folds verbatim; every read group
//! key a view group key the view projects; the view's condition implied by
//! the read's conjunct for conjunct, with any extra conjunct group-constant
//! (over the view's own keys, so it keeps or drops whole groups); no clause
//! that reads anything the stored rows cannot answer; the caller unrestricted
//! over the base table; no pending write to it in this transaction; and the
//! view current at the read's snapshot ([`Session::reflects`]).

mod mapping;

use tessari_ql::{Expr, ExprKind, Name, Ordering, Projected, Projection, Select, Source, TableRef};
use tessari_storage::{Catalog, TableKind, Transaction};

use self::mapping::{answerable, conjuncts_of, over_the_keys, projected_key, refolded};
use super::groups;
use crate::error::Result;
use crate::materialized::{Kept, Shape};
use crate::session::Session;

impl Session<'_> {
    /// The read rewritten onto a kept view's stored rows, and the view's name,
    /// when every soundness condition holds; `None` — never an error of its
    /// own — when any does not.
    ///
    /// # Errors
    ///
    /// Returns an error only when the catalog, the view state or the feed
    /// cannot be read.
    pub(crate) async fn onto_a_kept_view(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<(Select, String)>> {
        let Some(read) = Covered::of(select) else {
            return Ok(None);
        };
        let table = match &select.from {
            Source::Table(table) | Source::Where { table, .. } => table,
            _ => return Ok(None),
        };
        let Ok(context) = self
            .context(
                transaction,
                table.database.as_ref().map(|name| name.text.as_str()),
                table.span,
            )
            .await
        else {
            // No tenancy selected is the read's own refusal to make, not this
            // gate's; declining lets the ordinary path state it.
            return Ok(None);
        };
        let Some(base) = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &table.name.text)
            .await?
        else {
            return Ok(None);
        };
        // A grant that narrows the base table's fields or rows would be
        // answered from rows computed for everybody — the one caller this
        // rewrite must never serve.
        if self.visible_in(transaction, base).await?.is_some()
            || self.rows_in(transaction, base).await?.is_some()
        {
            return Ok(None);
        }
        // The view cannot hold this transaction's own uncommitted writes.
        if transaction.writes_in(context.namespace, context.database, base) {
            return Ok(None);
        }
        let candidates = Catalog::new(transaction)
            .tables_in(context.namespace, context.database)
            .await?;
        for definition in candidates {
            if !matches!(&definition.kind, TableKind::View(declared) if declared.materialized) {
                continue;
            }
            let Some(kept) = self.materialized_view(transaction, definition.id).await? else {
                continue;
            };
            if kept.source != base || !matches!(kept.understood.shape, Shape::Grouped) {
                continue;
            }
            let Some(rewritten) = read.onto(&kept, select, &definition.name) else {
                continue;
            };
            // Currency is asked last: it is the one condition that reads the
            // feed, and every condition before it is answered from the trees.
            let Some(state) = transaction.view_state(kept.view).await? else {
                continue;
            };
            if !state.by_group {
                continue;
            }
            if !self.reflects(&kept, &state, transaction.snapshot()).await? {
                continue;
            }
            return Ok(Some((rewritten, definition.name)));
        }
        Ok(None)
    }
}

/// A read whose shape the rewrite covers, judged before any catalog is read.
struct Covered<'a> {
    /// The read's condition's conjuncts, empty for a read with none.
    conjuncts: Vec<&'a Expr>,
}

impl<'a> Covered<'a> {
    /// The gate over the statement's own shape: every clause the stored rows
    /// cannot answer refuses here, before any I/O.
    fn of(select: &'a Select) -> Option<Self> {
        let plain = matches!(select.from, Source::Table(_) | Source::Where { .. });
        let Projection::Values { everything, values } = &select.projection else {
            return None;
        };
        let refused = !plain
            || !groups(select)
            || everything.is_some()
            || !select.omit.is_empty()
            || select.lift_scan_guard
            || select.only.is_some()
            || !select.fetch.is_empty()
            || select.split.is_some()
            || select.having.is_some()
            || !select.combined.is_empty()
            || select.compound_tail.is_some()
            || select.locking.is_some()
            || !select.windows.is_empty()
            || select.fill.is_some()
            || select.latest.is_some()
            || select.fusion.is_some()
            || select.after.is_some()
            || select.approximate.is_some()
            || select.using.is_some()
            || select.version.is_some()
            || select.staleness.is_some()
            || select.answered_by.is_some()
            || select.level.is_some();
        if refused {
            return None;
        }
        let conjuncts = match &select.from {
            Source::Where { condition, .. } => conjuncts_of(condition),
            _ => Vec::new(),
        };
        // Nothing the stored rows cannot answer may stand anywhere in what
        // the rewrite carries over: a parameter is unbound, and a subquery, a
        // key read or a TTL reads data the view never saw.
        let clean = conjuncts.iter().all(|expr| answerable(expr))
            && select.group.iter().all(answerable)
            && values.iter().all(|one| answerable(&one.value))
            && select
                .order
                .iter()
                .all(|ordering| answerable(&ordering.key));
        clean.then_some(Self { conjuncts })
    }

    /// The read rewritten onto `kept`'s rows under the name `view_name`, or
    /// `None` when this view cannot answer it.
    fn onto(&self, kept: &Kept, select: &Select, view_name: &str) -> Option<Select> {
        if !matches!(
            kept.understood.select.projection,
            Projection::Values {
                everything: None,
                ..
            }
        ) {
            return None;
        }
        // The view's condition must be implied by the read's: every conjunct
        // of the view's appears among the read's, so the rows the view kept
        // are a superset of the rows the read wants before the extra
        // conjuncts — which must be group-constant — remove whole groups.
        let view_conjuncts = kept
            .understood
            .condition
            .as_ref()
            .map(conjuncts_of)
            .unwrap_or_default();
        if view_conjuncts
            .iter()
            .any(|held| !self.conjuncts.iter().any(|asked| asked.same_shape(held)))
        {
            return None;
        }
        let extra: Vec<Expr> = self
            .conjuncts
            .iter()
            .filter(|asked| !view_conjuncts.iter().any(|held| held.same_shape(asked)))
            .map(|asked| over_the_keys(asked, &kept.understood.select))
            .collect::<Option<Vec<_>>>()?;
        // Each read group key is a view group key the view projects.
        let group: Vec<Expr> = select
            .group
            .iter()
            .map(|key| projected_key(key, &kept.understood.select))
            .collect::<Option<Vec<_>>>()?;
        // Each read value is a view group key or one of the four
        // distributive folds the view holds verbatim.
        let values: Vec<Projected> = select
            .projection
            .written()
            .iter()
            .map(|one| {
                Some(Projected {
                    value: refolded(&one.value, &kept.understood.select)?,
                    name: one.name.clone(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        // An order key is one of the read's own projected names — ordering
        // runs after projection, and the aliases are kept — or a value that
        // maps like a projected one.
        let names: Vec<&str> = select
            .projection
            .written()
            .iter()
            .map(|one| one.name.text.as_str())
            .collect();
        let order: Vec<Ordering> = select
            .order
            .iter()
            .map(|ordering| {
                let key = match &ordering.key.kind {
                    ExprKind::Path(path)
                        if path.path.steps().is_empty() && names.contains(&path.path.root()) =>
                    {
                        ordering.key.clone()
                    }
                    _ => refolded(&ordering.key, &kept.understood.select)?,
                };
                Some(Ordering {
                    key,
                    descending: ordering.descending,
                    nulls: ordering.nulls,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let view = TableRef {
            database: None,
            name: Name {
                text: view_name.to_owned(),
                span: select.span,
            },
            span: select.span,
        };
        let from = match extra.split_first() {
            None => Source::Table(view),
            Some((first, rest)) => Source::Where {
                table: view,
                condition: Box::new(rest.iter().fold(first.clone(), |left, right| Expr {
                    kind: ExprKind::And(Box::new(left), Box::new(right.clone())),
                    span: select.span,
                })),
            },
        };
        Some(Select {
            projection: Projection::Values {
                everything: None,
                values,
            },
            omit: Vec::new(),
            from,
            lift_scan_guard: false,
            only: None,
            fetch: Vec::new(),
            split: None,
            group,
            having: None,
            combined: Vec::new(),
            compound_tail: None,
            locking: None,
            windows: Vec::new(),
            fill: None,
            latest: None,
            order,
            fusion: None,
            after: None,
            approximate: None,
            start: select.start,
            limit: select.limit,
            start_parameter: None,
            limit_parameter: None,
            using: None,
            timeout: select.timeout,
            version: None,
            staleness: None,
            answered_by: None,
            level: None,
            span: select.span,
        })
    }
}
