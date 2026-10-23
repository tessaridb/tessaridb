//! `fn(…) OVER ([PARTITION BY …] [ORDER BY …]) AS name` in a projection
//! (ADR-0134 D13).

use super::Parser;

use crate::ast::{
    Aggregate, Expr, ExprKind, Fusion, Neighbour, Projection, Source, WindowFunction, Windowed,
};
use crate::error::{Error, Result};
use crate::token::{Punct, Token};
use tessari_types::Number;

/// The functions that exist only over a window, read before the expression
/// grammar sees their name: none of them is a function of one record.
const OWN: [&str; 7] = [
    "row_number",
    "rank",
    "dense_rank",
    "lag",
    "lead",
    "first_value",
    "last_value",
];

impl Parser<'_> {
    /// Whether a window function's own name and its `(` stand here.
    pub(super) fn window_function_here(&self) -> bool {
        OWN.iter().any(|name| self.peek_word(name))
            && self.follows_with(1, &Token::Punct(Punct::ParenOpen))
    }

    /// A window function by its own name, through to its `AS` name.
    pub(super) fn window_of_its_own(&mut self) -> Result<Windowed> {
        let span = self.span_here();
        let word = match self.peek() {
            Some(Token::Ident(word)) => word.to_ascii_lowercase(),
            _ => return Err(self.error_here("a window function")),
        };
        self.advance();
        self.expect_punct(Punct::ParenOpen, "`(` and the function's arguments")?;
        let function = match word.as_str() {
            "row_number" => WindowFunction::RowNumber,
            "rank" => WindowFunction::Rank,
            "dense_rank" => WindowFunction::DenseRank,
            "lag" => WindowFunction::Lag(self.neighbour()?),
            "lead" => WindowFunction::Lead(self.neighbour()?),
            "first_value" => WindowFunction::FirstValue(Box::new(self.condition()?)),
            _ => WindowFunction::LastValue(Box::new(self.condition()?)),
        };
        self.expect_punct(Punct::ParenClose, "`)` after the function's arguments")?;
        if !self.peek_word("over") {
            return Err(self.error_here(
                "`OVER (…)` — this function answers about the records around one, \
                 which the window names",
            ));
        }
        self.over(function, span)
    }

    /// A fold followed by `OVER`, which makes it a window: `count`, `sum`,
    /// `mean`, `min` and `max`, over a value or (`count` only) over `*`.
    pub(super) fn window_of_a_fold(&mut self, value: Expr) -> Result<Windowed> {
        let ExprKind::Fold {
            fold:
                fold @ (Aggregate::Count
                | Aggregate::Sum
                | Aggregate::Mean
                | Aggregate::Min
                | Aggregate::Max),
            over,
            at: None,
            ..
        } = value.kind
        else {
            return Err(Error::Unsupported {
                feature: "`OVER` after anything but `count`, `sum`, `mean`, `min`, `max` or a \
                          window function — a window computes one of those over its frame",
                span: value.span,
            });
        };
        self.over(WindowFunction::Fold { fold, over }, value.span)
    }

    /// `OVER ( [PARTITION BY e, …] [ORDER BY k, …] ) AS name`.
    fn over(&mut self, function: WindowFunction, span: crate::token::Span) -> Result<Windowed> {
        self.eat_word("over");
        self.expect_punct(Punct::ParenOpen, "`(` and the window, which may be empty")?;
        let mut partition = Vec::new();
        if self.eat_word("partition") {
            if !self.eat_word("by") {
                return Err(self.error_here("`BY` after `PARTITION`"));
            }
            partition.push(self.condition()?);
            while self.eat_punct(Punct::Comma) {
                partition.push(self.condition()?);
            }
        }
        let mut order = Vec::new();
        if self.eat_word("order") {
            if !self.eat_word("by") {
                return Err(self.error_here("`BY` after `ORDER`"));
            }
            order.push(self.ordering()?);
            while self.eat_punct(Punct::Comma) {
                order.push(self.ordering()?);
            }
        }
        self.expect_punct(Punct::ParenClose, "`)` after the window")?;
        let span = span.to(self.span_behind());
        if !self.eat_keyword(crate::token::Keyword::As) {
            return Err(Error::UnnamedProjection { span });
        }
        Ok(Windowed {
            function,
            partition,
            order,
            name: self.name()?,
            span,
        })
    }

    /// `value [, offset [, default]]` for `lag` and `lead`.
    fn neighbour(&mut self) -> Result<Neighbour> {
        let value = Box::new(self.condition()?);
        let mut offset = 1;
        let mut default = None;
        if self.eat_punct(Punct::Comma) {
            offset = match self.peek() {
                Some(Token::Number(Number::Integer(rows))) => u64::try_from(*rows)
                    .map_err(|_| self.error_here("a whole number of records, 0 or more"))?,
                _ => return Err(self.error_here("a whole number of records, 0 or more")),
            };
            self.advance();
            if self.eat_punct(Punct::Comma) {
                default = Some(Box::new(self.condition()?));
            }
        }
        Ok(Neighbour {
            value,
            offset,
            default,
        })
    }
}

impl Parser<'_> {
    /// Where a window may not stand: beside a grouping or a fold, whose rows are
    /// groups rather than records; over a search or beside `ORDER BY FUSE`,
    /// which rank records in a pass of their own; and over `[*]`, which has no
    /// one value to compute with.
    pub(super) fn check_windows(
        &self,
        windows: &[Windowed],
        projection: &Projection,
        (group, from, fusion): (&[Expr], &Source, Option<&Fusion>),
    ) -> Result<()> {
        let Some(first) = windows.first() else {
            return Ok(());
        };
        let folds = match projection {
            Projection::All => false,
            Projection::Values { values, .. } => values
                .iter()
                .any(|one| crate::parser::shape::holds_a_fold(&one.value)),
        };
        if !group.is_empty() || folds {
            return Err(Error::Unsupported {
                feature: "a window function beside `GROUP BY` or a fold — a window computes over \
                          records, and a grouped read answers with groups",
                span: first.span,
            });
        }
        if fusion.is_some() || matches!(from, Source::Search { .. }) {
            return Err(Error::Unsupported {
                feature: "a window function over a search or beside `ORDER BY FUSE`, which rank \
                          the records in a pass of their own",
                span: first.span,
            });
        }
        for window in windows {
            for expr in window.expressions() {
                crate::parser::shape::no_several(expr)?;
            }
        }
        Ok(())
    }
}
