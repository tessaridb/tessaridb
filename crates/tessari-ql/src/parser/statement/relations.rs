//! Edges, traversals, RELATE and key reads.

use super::Parser;

use crate::ast::{
    Direction, EdgeClause, EdgeEndpoints, EdgeOrdering, ExprKind, Hop, PathTo, RangeExpr,
    RecordTarget, Repeat, Source, StatementKind,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};
use tessari_types::Number;

impl Parser<'_> {
    /// `FROM users TO users ORDER BY at DESC` after `EDGE`, when it is there.
    ///
    /// The pair is read as a unit: `FROM` without `TO` is refused rather than
    /// read as half a declaration, because an edge table that knows only where
    /// its edges leave from could refuse nothing a permissive one accepts, and
    /// the statement would have bought its clause for nothing.
    pub(super) fn edge_clause(&mut self) -> Result<EdgeClause> {
        if !self.eat_keyword(Keyword::From) {
            return Ok(EdgeClause::Any);
        }
        let from = self.table_ref()?;
        self.expect_keyword(Keyword::To, "`TO` and the table an edge leads into")?;
        let to = self.table_ref()?;
        let order = self.edge_ordering()?;
        Ok(EdgeClause::Between(Box::new(EdgeEndpoints {
            from,
            to,
            order,
        })))
    }

    /// `ORDER BY at DESC` after an edge table's declared pair, when it is there.
    ///
    /// Read with contextual words for the reason `shape.rs` reads the same
    /// clause that way: reserving `ORDER` would take a good column name out of
    /// every table in the store to buy nothing, since only a clause word can
    /// stand in this position.
    ///
    /// The key is a single field **name**, not the routed expression a `SELECT`
    /// orders by. It becomes the endpoint index's key suffix, so it has to be
    /// something the writer can read off the edge as it places it.
    pub(super) fn edge_ordering(&mut self) -> Result<Option<EdgeOrdering>> {
        if !self.eat_word("order") {
            return Ok(None);
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `ORDER`"));
        }
        let field = self.name()?;
        // `ASC` is accepted and means nothing, exactly as it does in a `SELECT`:
        // a reader who writes the default is saying what they mean.
        let descending = if self.eat_word("desc") {
            true
        } else {
            self.eat_word("asc");
            false
        };
        Ok(Some(EdgeOrdering { field, descending }))
    }

    /// The rest of `users:1->follows`, `users:1->follows->users`, or a chain of
    /// those: `users:1->follows->users->follows->users`.
    ///
    /// **Every arrow points the same way.** Within a step a mixed pair would read
    /// as "the edges out of `a`, then whichever record their `out` names" — which
    /// is `a` again, for every edge, and is a query nobody means to write. Across
    /// steps a mixed pair asks a real question, and it is a design rather than a
    /// loosened rule; `docs/tessariql.md` §8 holds it as its own row.
    ///
    /// The loop is what keeps `a->e1->e2` unambiguous: a table read after an
    /// arrow is this step's **node**, and the walk continues only if another
    /// arrow follows it. So there is never a step with a gap where its node
    /// should be.
    pub(super) fn traversal(&mut self, from: RecordTarget, direction: Direction) -> Result<Source> {
        let mut hops = Vec::new();
        loop {
            let edges = self.table_ref()?;
            let repeat = self.segment()?;
            let Some(second) = self.arrow() else {
                // A segment is the step's landing set, and a step ending on the
                // edges has nothing to land — refused here, where the shape is
                // known, rather than answered with nothing (ADR-0143).
                if let Some(segment) = repeat {
                    return Err(Error::RepeatNeedsTarget { span: segment.span });
                }
                hops.push(Hop {
                    edges,
                    target: None,
                    repeat: None,
                });
                break;
            };
            if second != direction {
                return Err(self.error_here("an arrow pointing the same way as the first"));
            }
            let target = self.table_ref()?;
            hops.push(Hop {
                edges,
                target: Some(target),
                repeat,
            });
            match self.arrow() {
                Some(next) if next == direction => {}
                Some(_) => {
                    return Err(self.error_here("an arrow pointing the same way as the first"));
                }
                None => break,
            }
        }
        // `PATH TO <record>` before the bound: the shortest path to one record
        // rather than everything within reach (G055 W6). Contextual words, so
        // nothing that was a name before stops being one.
        let path_at = self.span_here();
        let path_to = if self.eat_word("path") {
            if !self.eat_keyword(Keyword::To) {
                return Err(self.error_here("`TO` after `PATH`"));
            }
            Some(self.record_target()?)
        } else {
            None
        };
        let depth = self.depth_bound(&hops)?;
        let path = match path_to {
            Some(to) => {
                if depth.is_none() {
                    return Err(Error::PathNeedsDepth { span: path_at });
                }
                let weight = if self.eat_word("weight") {
                    Some(self.name()?)
                } else {
                    None
                };
                // `ALL` or `K n` — every minimal-cost path, or the n best by
                // the order the single path already uses. Both together are
                // two answers to one question (ADR-0143's companion).
                let all = self.eat_word("all");
                let k = if self.eat_word("k") {
                    Some(self.k_bound()?)
                } else {
                    None
                };
                let all = if !all && self.eat_word("all") {
                    if k.is_some() {
                        return Err(Error::PathAllWithK { span: path_at });
                    }
                    true
                } else {
                    all
                };
                if all && k.is_some() {
                    return Err(Error::PathAllWithK { span: path_at });
                }
                Some(Box::new(PathTo {
                    to,
                    weight,
                    all,
                    k,
                    span: path_at,
                }))
            }
            None => None,
        };
        Ok(Source::Traverse {
            from,
            direction,
            hops,
            depth,
            path,
        })
    }

    /// One traversal arrow, consumed if it is there.
    pub(super) fn arrow(&mut self) -> Option<Direction> {
        if self.eat_punct(Punct::ArrowRight) {
            return Some(Direction::Outgoing);
        }
        if self.eat_punct(Punct::ArrowLeft) {
            return Some(Direction::Incoming);
        }
        None
    }

    /// `[*m..n]` after a step's edge kind, when it is there (ADR-0143).
    ///
    /// Both bounds are integer literals — the walk states its own length where
    /// the statement is read, the same discipline `depth_bound` holds `DEPTH`
    /// to. A negative bound refuses as the below-one or above-max shape it
    /// lands in, exactly as `DEPTH` refuses a negative.
    fn segment(&mut self) -> Result<Option<Repeat>> {
        if self.peek() != Some(&Token::Punct(Punct::BracketOpen)) {
            return Ok(None);
        }
        let span = self.span_here();
        self.advance();
        self.expect_punct(Punct::Star, "`*` and the segment's lower bound")?;
        let min = self.segment_bound("`..` and the upper bound")?;
        self.expect_punct(Punct::DotDot, "`..` between the segment's bounds")?;
        let max = self.segment_bound("`]`, then the arrow and the node it lands on")?;
        self.expect_punct(
            Punct::BracketClose,
            "`]`, then the arrow and the node it lands on",
        )?;
        if min == 0 {
            return Err(Error::RepeatBelowOne { span });
        }
        if min > max {
            return Err(Error::RepeatAboveMax { span });
        }
        Ok(Some(Repeat { min, max, span }))
    }

    /// The literal path count after `K`: a whole number, one or more.
    fn k_bound(&mut self) -> Result<u64> {
        let span = self.span_here();
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here("`K n` — a whole number of paths, written out"));
        };
        let count = u64::try_from(*count).unwrap_or(0);
        self.advance();
        if count == 0 {
            return Err(Error::KBelowOne { span });
        }
        Ok(count)
    }

    /// One integer bound of a segment. A non-integer refuses as not-a-number;
    /// a negative reads as zero and is refused by the bound's own rule, as
    /// `DEPTH` does.
    fn segment_bound(&mut self, after: &'static str) -> Result<u64> {
        let Some(Token::Number(Number::Integer(bound))) = self.peek() else {
            return Err(self.error_here(after));
        };
        let bound = u64::try_from(*bound).unwrap_or(0);
        self.advance();
        Ok(bound)
    }

    /// `RELATE users:1->follows->users:2 = { since: … }`
    ///
    /// The `= { … }` is optional, because most edges carry nothing but their two
    /// endpoints, and a required empty object would be noise on every line.
    pub(super) fn relate_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        let from = self.record_target()?;
        self.expect_punct(Punct::ArrowRight, "`->` and the edge table")?;
        let edges = self.table_ref()?;
        self.expect_punct(Punct::ArrowRight, "`->` and the record to relate to")?;
        let to = self.record_target()?;
        let value = if self.eat_punct(Punct::Equals) {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        Ok(StatementKind::Relate {
            from,
            edges,
            to,
            value,
        })
    }

    /// `KEYS FROM sessions RANGE 'a'..'m'`
    pub(super) fn keys_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        self.expect_keyword(Keyword::From, "`FROM` and the space to list")?;
        let space = self.table_ref()?;
        let mut range = None;
        let mut prefix = None;
        if self.eat_keyword(Keyword::Range) {
            range = Some(self.range()?);
        } else if self.eat_keyword(Keyword::Prefix) {
            prefix = Some(self.expression()?);
        }
        let after = if self.eat_word("after") {
            Some(self.expression()?)
        } else {
            None
        };
        let limit = self.bound("limit")?;
        Ok(StatementKind::Keys {
            space,
            range,
            prefix,
            after,
            limit,
        })
    }

    /// The range after `RANGE`, which must be one.
    pub(super) fn range(&mut self) -> Result<RangeExpr> {
        let start = self.span_here();
        let expression = self.expression()?;
        match expression.kind {
            ExprKind::Range(range) => Ok(range),
            _ => Err(Error::NotARange {
                span: start.to(self.span_behind()),
            }),
        }
    }
}
