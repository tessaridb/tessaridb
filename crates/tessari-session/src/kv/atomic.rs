//! `INCR` and the conditional `SET`: writes that read the key they write.
//!
//! # Atomic because the store already is
//!
//! Each one reads the key and writes it in one transaction, and two writers
//! that read the same version and both write it are a detected write-write race
//! under the store's snapshot isolation: the first committer wins and the loser
//! writes nothing. So no increment is ever lost. What a cache user also expects
//! is that an increment does not *fail* because a neighbour got there first, so
//! when one of these runs as a statement of its own — outside `BEGIN … COMMIT`,
//! where nothing has been committed and running it again is exactly what the
//! caller would do — the session runs it again on a fresh snapshot until it
//! lands or [`CONFLICT_DEADLINE`] passes ([`retried_on_conflict`]).
//!
//! The bound is a time rather than a count because a count was measured and
//! was not enough: four writers incrementing one key on the memory backend lost
//! sixteen races in a row on 14 of 200 increments, since each attempt loses to
//! any of the other three. A deadline keeps the promise under that contention
//! and still returns the ordinary retriable refusal when a key is written in a
//! loop by more writers than it can serve.

use tessari_ql::{ArithmeticOp, Expr, RecordTarget, SetCondition, Span, StatementKind};
use tessari_storage::Transaction;
use tessari_types::{Number, Value};

use crate::arithmetic::arithmetic;
use crate::error::Result;
use crate::outcome::Outcome;
use crate::session::Session;

/// How long a lone atomic statement keeps retrying a lost race before the
/// conflict is returned to the caller — the same retriable refusal a
/// transaction gets.
pub(crate) const CONFLICT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether a statement run on its own is run again after a commit conflict.
pub(crate) const fn retried_on_conflict(kind: &StatementKind) -> bool {
    matches!(
        kind,
        StatementKind::Incr { .. }
            | StatementKind::Set {
                condition: Some(_),
                ..
            }
            // Two readers of one name write one position, so one of them loses;
            // run again, it reads after the winner's position and is given the
            // next messages instead (G037).
            | StatementKind::ReadTopic {
                consumer: Some(_),
                ..
            }
    )
}

impl Session<'_> {
    /// `INCR key [BY amount]` — answers the number the key now holds.
    pub(crate) fn increment(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        by: Option<&Expr>,
        span: Span,
    ) -> Result<Outcome> {
        let amount = match by {
            Some(by) => self.evaluate(transaction, by)?,
            None => Value::Number(Number::Integer(1)),
        };
        let (_, address) = self.writable(transaction, target)?;
        let held = self.read_key(transaction, target)?;
        // Kept across the write: an increment alters the value and not how long
        // the key lives, which is the rule the cache semantics here follow.
        let expires = transaction.expires(&address)?;
        let current = match held {
            Value::None => Value::Number(Number::Integer(0)),
            other => other,
        };
        let next = arithmetic(ArithmeticOp::Add, &current, &amount, span)?;
        self.put_record(transaction, address.clone(), next.clone(), span)?;
        if let Some(at) = expires {
            transaction.expire_pending(&address, at);
        }
        Ok(Outcome::Value(next))
    }

    /// Whether a conditional `SET`'s condition holds for the key as it is now.
    pub(crate) fn set_condition_holds(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        condition: &SetCondition,
    ) -> Result<bool> {
        let held = self.read_key(transaction, target)?;
        Ok(match condition {
            SetCondition::Absent => held == Value::None,
            SetCondition::Present => held != Value::None,
            SetCondition::Equals(expected) => {
                held != Value::None && held == self.evaluate(transaction, expected)?
            }
        })
    }
}
