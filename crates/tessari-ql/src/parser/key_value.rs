//! The key-value verbs a cache needs (G035): an expiry on `SET`, `EXPIRE`,
//! `PERSIST` and `TTL`.
//!
//! # Every word here is contextual
//!
//! `expire`, `persist` and `ttl` are ordinary field names — a session table has
//! an `expire` column, a cache entry has a `ttl` — so reserving them would take
//! them from exactly the schemas that want this feature. A verb is only ever
//! read at the head of a statement, where nothing but a verb can stand, and
//! `TTL` only where a record reference follows it, where a field name followed
//! by a table name would not parse at all; so no statement that parsed before
//! changes its meaning.

use super::Parser;
use crate::ast::{Expr, ExprKind, RecordTarget, SetCondition, SpaceBound, StatementKind};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// The `SET` statement once its target and value are read: an optional
    /// condition and an optional expiry, in either order, each at most once.
    pub(super) fn set_statement(
        &mut self,
        target: RecordTarget,
        value: Expr,
    ) -> Result<StatementKind> {
        let mut expire = None;
        let mut condition = None;
        loop {
            if expire.is_none() && self.eat_word("expire") {
                expire = Some(self.expression()?);
            } else if condition.is_none() && self.eat_keyword(Keyword::If) {
                condition = Some(self.set_condition()?);
            } else {
                break;
            }
        }
        Ok(StatementKind::Set {
            target,
            value,
            expire,
            condition,
        })
    }

    /// What follows `IF` on a `SET`.
    fn set_condition(&mut self) -> Result<SetCondition> {
        if self.eat_word("absent") {
            return Ok(SetCondition::Absent);
        }
        if self.eat_word("present") {
            return Ok(SetCondition::Present);
        }
        self.expect_punct(
            Punct::Equals,
            "`ABSENT`, `PRESENT` or `=` and the value the key must hold",
        )?;
        Ok(SetCondition::Equals(self.expression()?))
    }

    /// The optional `MAX n [EVICT NONE]` after `DEFINE SPACE name` (G036).
    ///
    /// `max` and `evict` are contextual for the reason every word in this file
    /// is: they are field names in the schemas that want the feature.
    pub(super) fn space_bound(&mut self) -> Result<Option<SpaceBound>> {
        if !self.peek_word("max") {
            return Ok(None);
        }
        if matches!(
            self.peek_ahead(1),
            Some(Token::Number(tessari_types::Number::Integer(0)))
        ) {
            self.eat_word("max");
            return Err(self.error_here("a limit above zero"));
        }
        let Some(max) = self.bound("max")? else {
            return Ok(None);
        };
        let refuse = if self.eat_word("evict") {
            if !self.eat_keyword(Keyword::None) {
                return Err(self.error_here("`NONE` — the one rule a space names"));
            }
            true
        } else {
            false
        };
        Ok(Some(SpaceBound { max, refuse }))
    }

    /// `INCR <key> [BY <amount>]`, the verb already consumed.
    pub(super) fn incr_statement(&mut self) -> Result<StatementKind> {
        let target = self.record_target()?;
        let by = if self.eat_word("by") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(StatementKind::Incr { target, by })
    }

    /// `EXPIRE <key> <when>`, the verb already consumed.
    pub(super) fn expire_statement(&mut self) -> Result<StatementKind> {
        let target = self.record_target()?;
        let at = self.expression()?;
        Ok(StatementKind::Expire { target, at })
    }

    /// `PERSIST <key>`, the verb already consumed.
    pub(super) fn persist_statement(&mut self) -> Result<StatementKind> {
        Ok(StatementKind::Persist {
            target: self.record_target()?,
        })
    }

    /// Whether `TTL` stands here as the key-value read rather than as a field.
    ///
    /// Two tokens of lookahead past the word: a name, then `:`. That is a record
    /// reference and nothing else, and a field called `ttl` followed by one is
    /// not an expression this grammar ever accepted.
    pub(super) fn ttl_follows(&self) -> bool {
        self.peek_word("ttl")
            && matches!(
                self.peek_ahead(1),
                Some(Token::Ident(_) | Token::Keyword(_))
            )
            && matches!(
                self.peek_ahead(2),
                Some(Token::Punct(crate::token::Punct::Colon))
            )
    }

    /// `TTL <key>` in a value position, the lookahead already checked.
    pub(super) fn ttl_expression(&mut self) -> Result<Expr> {
        let span = self.span_here();
        self.eat_word("ttl");
        let target: RecordTarget = self.record_target()?;
        let end = target.span;
        Ok(Expr {
            kind: ExprKind::Ttl(target),
            span: span.to(end),
        })
    }
}
