//! `UNION [ALL]`, `INTERSECT` and `EXCEPT` between reads (ADR-0134 D6).

use super::super::Parser;

use crate::ast::{Combination, CompoundTail, Select, SetOperation};
use crate::error::{Error, Result};
use crate::token::Keyword;

impl Parser<'_> {
    /// A read and every read it combines with, left to right, at the top
    /// level of a statement: a trailing `ORDER BY` after the last branch is
    /// the compound's own tail.
    pub(in crate::parser) fn select_statement(&mut self) -> Result<Select> {
        self.combined_select(true)
    }

    /// As [`Self::select_statement`], inside parentheses: the ceiling
    /// discipline on a materialised source binds unchanged, so a trailing
    /// `ORDER BY` there stays the last read's own.
    pub(in crate::parser) fn combined_select(&mut self, top: bool) -> Result<Select> {
        let mut first = self.select_single()?;
        loop {
            let span = self.span_here();
            let operation = if self.eat_word("union") {
                SetOperation::Union
            } else if self.eat_word("intersect") {
                SetOperation::Intersect
            } else if self.eat_word("except") {
                SetOperation::Except
            } else {
                break;
            };
            let all = self.eat_word("all");
            if all && operation != SetOperation::Union {
                return Err(Error::UnexpectedToken {
                    expected: "`SELECT` — `ALL` keeps duplicates, and only `UNION` makes them",
                    found: "`ALL`".to_owned(),
                    span: self.span_behind(),
                });
            }
            if self.peek_keyword() != Some(Keyword::Select) {
                return Err(self.error_here("`SELECT` and the read to combine with"));
            }
            let read = self.select_single()?;
            first.combined.push(Combination {
                operation,
                all,
                read: Box::new(read),
                span,
            });
        }
        if top {
            self.lift_compound_tail(&mut first)?;
        }
        Ok(first)
    }

    /// A trailing `ORDER BY` on the last branch of a statement-level compound
    /// is the compound's own tail: the collected answer sorts as one set. A
    /// `LIMIT` alone stays the last read's own, the rule the suite pins.
    ///
    /// The keys must name what the combined answer answers under — an output
    /// column or an ordinal. An expression would need a record to read, and
    /// the collection is values; the refusal names the rule.
    fn lift_compound_tail(&mut self, first: &mut Select) -> Result<()> {
        let Some(last) = first.combined.last_mut() else {
            return Ok(());
        };
        if last.read.order.is_empty() {
            return Ok(());
        }
        for ordering in &last.read.order {
            match &ordering.key.kind {
                crate::ast::ExprKind::Literal(tessari_types::Value::Number(
                    tessari_types::Number::Integer(held),
                )) if *held >= 1 => {}
                crate::ast::ExprKind::Path(path) if path.path.steps().is_empty() => {}
                _ => {
                    return Err(Error::CompoundOrderNotOutputName {
                        found: format!("{:?}", ordering.key.kind),
                        span: ordering.key.span,
                    });
                }
            }
        }
        first.compound_tail = Some(CompoundTail {
            ordering: std::mem::take(&mut last.read.order),
            start: last.read.start.take(),
            start_parameter: last.read.start_parameter.take(),
            limit: last.read.limit.take(),
            limit_parameter: last.read.limit_parameter.take(),
        });
        Ok(())
    }
}
