//! Which reads a materialized view can keep, and how it keeps each one.
//!
//! A view is kept current from **one table's** changes, so everything its read
//! depends on must be that table's records as of one version. A clause that
//! reads anything else — another table, a space key, the clock, a generator —
//! would let the stored rows go stale with no change in the feed to say so, and
//! is refused where the view is declared (ADR-0109 D4).

use tessari_ql::{Expr, ExprKind, Projection, Purity, Select, Source, Span, TableRef, parse_read};

use crate::error::{Error, Result};
use crate::evaluate::groups;

/// How a batch recomputes a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// One stored row per source record that passes the condition, under the
    /// record's own identity — so a batch recomputes only the records it saw
    /// change.
    PerRecord,
    /// One stored row per group of a `GROUP BY` and nothing that orders, bounds
    /// or reshapes the groups, under the group key's order-preserving bytes —
    /// so a batch recomputes only the groups its changes left and joined.
    Grouped,
    /// Anything else that folds, orders, bounds or splits: one change can move
    /// any row, so a batch recomputes the whole read.
    Whole,
}

/// A view's read, parsed and judged.
#[derive(Debug, Clone)]
pub(crate) struct Understood {
    /// The read as written.
    pub(crate) select: Select,
    /// The one table it reads.
    pub(crate) source: TableRef,
    /// Its condition, when it has one.
    pub(crate) condition: Option<Expr>,
    /// How it is kept.
    pub(crate) shape: Shape,
}

impl Understood {
    /// Parse a view's stored read and judge whether it can be kept.
    ///
    /// # Errors
    ///
    /// [`Error::MaterializedShape`] naming the clause that reads beyond one
    /// table's records, or the parse failure of a read that no longer parses.
    pub(crate) fn of(read: &str, span: Span) -> Result<Self> {
        let select = parse_read(read)?;
        let refuse = |what: &'static str| Error::MaterializedShape { what, span };
        let (source, condition) = match &select.from {
            Source::Table(table) => (table.clone(), None),
            Source::Where { table, condition } => (table.clone(), Some((**condition).clone())),
            _ => return Err(refuse("read anything but one table")),
        };
        if !select.fetch.is_empty() {
            return Err(refuse("FETCH another table's records"));
        }
        if select.version.is_some() {
            return Err(refuse("read the store as it stood (VERSION)"));
        }
        if select.staleness.is_some() || select.answered_by.is_some() {
            return Err(refuse("choose which node answers it"));
        }
        if select.timeout.is_some() {
            return Err(refuse("carry a TIMEOUT"));
        }
        if select.after.is_some() {
            return Err(refuse("resume after a record"));
        }
        if select.approximate.is_some() || select.fusion.is_some() {
            return Err(refuse("be approximate or fused"));
        }
        let mut expressions: Vec<&Expr> = Vec::new();
        expressions.extend(condition.as_ref());
        if let Projection::Values { values, .. } = &select.projection {
            expressions.extend(values.iter().map(|one| &one.value));
        }
        expressions.extend(select.group.iter());
        expressions.extend(select.order.iter().map(|ordering| &ordering.key));
        if expressions.iter().any(|expr| beyond_the_record(expr)) {
            return Err(refuse(
                "depend on anything but its records — the clock, a generator, a subquery or a key",
            ));
        }
        let reshaped = !select.order.is_empty()
            || select.limit.is_some()
            || select.start.is_some()
            || select.split.is_some()
            || select.latest.is_some()
            || select.fill.is_some()
            || select.only.is_some();
        // A fold with no `GROUP BY` is one group every change reaches, so it
        // gains nothing from being kept a group at a time.
        let shape = if reshaped || (groups(&select) && select.group.is_empty()) {
            Shape::Whole
        } else if groups(&select) {
            Shape::Grouped
        } else {
            Shape::PerRecord
        };
        Ok(Self {
            select,
            source,
            condition,
            shape,
        })
    }
}

/// Whether any part of an expression answers from something other than the
/// record it is asked about.
///
/// Exhaustive over `ExprKind` with no wildcard, for the reason the planner's own
/// walks give: a new expression kind must be a compile error here, not a node
/// this walk steps over into a wrong `false`.
fn beyond_the_record(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Call {
            function,
            arguments,
            ..
        } => function.purity() != Purity::Pure || arguments.iter().any(beyond_the_record),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => beyond_the_record(inner),
        ExprKind::Route { value, .. } => beyond_the_record(value),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            beyond_the_record(condition)
                || beyond_the_record(then)
                || otherwise.as_deref().is_some_and(beyond_the_record)
        }
        ExprKind::Coalesce(left, right)
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            beyond_the_record(left) || beyond_the_record(right)
        }
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(beyond_the_record),
        ExprKind::Object(fields) => fields.iter().any(|field| beyond_the_record(&field.value)),
        ExprKind::Range(range) => beyond_the_record(&range.start) || beyond_the_record(&range.end),
        ExprKind::Fold { over, .. } => over.as_deref().is_some_and(beyond_the_record),
        // A subquery, a space key and its expiry read data outside the record;
        // a parameter has no value once the view is stored.
        ExprKind::Select(_) | ExprKind::Get(_) | ExprKind::Ttl(_) | ExprKind::Parameter(_) => true,
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Table(_) | ExprKind::Record(_) => {
            false
        }
    }
}
