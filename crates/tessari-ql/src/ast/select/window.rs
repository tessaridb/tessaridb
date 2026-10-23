use super::{Expr, Name, Ordering};
use crate::ast::Aggregate;
use crate::token::Span;

/// `fn(…) OVER ([PARTITION BY …] [ORDER BY …]) AS name` — a value computed for
/// each record from the records around it (ADR-0134 D13).
///
/// Held beside the projection rather than inside it: its value is not a function
/// of one record, so no expression evaluated against one could produce it, and a
/// node in the expression tree would be a node every evaluator must refuse. It
/// answers under its name as a projected value does, computed over the records
/// the `WHERE` kept and before the read's own `ORDER BY` and `LIMIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Windowed {
    /// What it computes.
    pub function: WindowFunction,
    /// `PARTITION BY` — records with equal keys are computed together; empty is
    /// one partition holding every record.
    pub partition: Vec<Expr>,
    /// `ORDER BY` inside the parentheses — the order a partition is walked in.
    ///
    /// With an order, the frame of a fold, `first_value` and `last_value` runs
    /// from the partition's first record to the current one; without one it is
    /// the whole partition.
    pub order: Vec<Ordering>,
    /// The name it answers under, which `AS` must give.
    pub name: Name,
    /// Where the function was written.
    pub span: Span,
}

/// What a window computes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowFunction {
    /// `row_number()` — the record's position in its partition, from 1.
    RowNumber,
    /// `rank()` — the position of the first record holding the same order keys,
    /// so ties share a rank and the next rank skips.
    Rank,
    /// `dense_rank()` — as `rank`, without the skip.
    DenseRank,
    /// `lag(value [, offset [, default]])` — `value` of the record `offset` before
    /// this one in the partition, or `default` (`NONE` unwritten) where there is
    /// none.
    Lag(Neighbour),
    /// `lead(…)` — as `lag`, after.
    Lead(Neighbour),
    /// `first_value(value)` — `value` of the frame's first record.
    FirstValue(Box<Expr>),
    /// `last_value(value)` — `value` of the frame's last record.
    LastValue(Box<Expr>),
    /// `count`, `sum`, `mean`, `min` or `max` over the frame; `over` is absent
    /// for `count(*)`.
    Fold {
        /// Which fold.
        fold: Aggregate,
        /// What it folds over.
        over: Option<Box<Expr>>,
    },
}

/// The arguments of `lag` and `lead`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbour {
    /// The value read from the neighbour.
    pub value: Box<Expr>,
    /// How many records away; a whole number written in the statement, 1
    /// unwritten.
    pub offset: u64,
    /// What answers where no record is that far away.
    pub default: Option<Box<Expr>>,
}

impl WindowFunction {
    /// Every expression the function evaluates.
    #[must_use]
    pub fn expressions(&self) -> Vec<&Expr> {
        match self {
            Self::RowNumber | Self::Rank | Self::DenseRank => Vec::new(),
            Self::Lag(neighbour) | Self::Lead(neighbour) => {
                let mut found = vec![neighbour.value.as_ref()];
                found.extend(neighbour.default.as_deref());
                found
            }
            Self::FirstValue(value) | Self::LastValue(value) => vec![value.as_ref()],
            Self::Fold { over, .. } => over.as_deref().into_iter().collect(),
        }
    }

    /// Every expression the function evaluates, to rewrite in place.
    pub fn expressions_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Self::RowNumber | Self::Rank | Self::DenseRank => Vec::new(),
            Self::Lag(neighbour) | Self::Lead(neighbour) => {
                let mut found = vec![neighbour.value.as_mut()];
                found.extend(neighbour.default.as_deref_mut());
                found
            }
            Self::FirstValue(value) | Self::LastValue(value) => vec![value.as_mut()],
            Self::Fold { over, .. } => over.as_deref_mut().into_iter().collect(),
        }
    }
}

impl Windowed {
    /// Every expression the window evaluates — its function's, its partition
    /// keys and its order keys — for the walks that must see each one.
    #[must_use]
    pub fn expressions(&self) -> Vec<&Expr> {
        let mut found = self.function.expressions();
        found.extend(self.partition.iter());
        found.extend(self.order.iter().map(|ordering| &ordering.key));
        found
    }

    /// [`Self::expressions`], to rewrite in place.
    pub fn expressions_mut(&mut self) -> Vec<&mut Expr> {
        let mut found = self.function.expressions_mut();
        found.extend(self.partition.iter_mut());
        found.extend(self.order.iter_mut().map(|ordering| &mut ordering.key));
        found
    }
}
