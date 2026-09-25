//! `SET … EXPIRE`, `EXPIRE`, `PERSIST` and `TTL`.
//!
//! # The instant is stored with the version, and a reader compares
//!
//! A key's expiry is written into its version as milliseconds since the Unix
//! epoch (`tessari-encoding`, flag `FLAG_EXPIRES`), computed **once** here on
//! the transaction's own clock. Every read path of the table then treats a
//! version whose instant has passed as absent; removing it is a separate pass
//! and correctness never waits for it.
//!
//! # The semantics follow the cache most applications already know
//!
//! A `SET` replaces the whole version and so clears an expiry the key had;
//! `EXPIRE` moves or sets one; `PERSIST` clears it; an `EXPIRE` that is zero,
//! negative or already past removes the key, as it does there; and a `SET` whose
//! `EXPIRE` is not in the future is **refused**, as it is there. `TTL` answers
//! a duration, `NULL` for a key that never expires and `NONE` for no key — this
//! store's own distinction between a value and its absence, where that system
//! answers −1 and −2.

use tessari_ql::{Expr, RecordTarget, SetCondition, Span};
use tessari_storage::Transaction;
use tessari_types::{Duration, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// Milliseconds in a second.
const MILLIS_PER_SECOND: i64 = 1_000;

/// Nanoseconds in a millisecond.
const NANOS_PER_MILLI: u32 = 1_000_000;

/// What an `EXPIRE` clause asked for, resolved against the transaction's clock.
enum Instant {
    /// Later than the clock: the key stops being answered then.
    Future(u64),
    /// Now or earlier: the key would already be gone.
    Passed,
}

impl Session<'_> {
    /// `SET key = value [IF condition] [EXPIRE when]`.
    ///
    /// Answers whether it wrote when it carries a condition, and nothing
    /// otherwise — the plain form's answer is unchanged.
    pub(crate) fn set_key(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        value: &Expr,
        expire: Option<&Expr>,
        condition: Option<&SetCondition>,
        span: Span,
    ) -> Result<Outcome> {
        // Resolved before the write, so a refused expiry writes nothing.
        let at = match expire {
            Some(expire) => match self.instant(transaction, expire)? {
                Instant::Future(at) => Some(at),
                Instant::Passed => {
                    return Err(Error::InvalidExpiry {
                        reason: "a SET's EXPIRE must be in the future",
                        span: expire.span,
                    });
                }
            },
            None => None,
        };
        let (_, address) = self.writable(transaction, target)?;
        if let Some(condition) = condition
            && !self.set_condition_holds(transaction, target, condition)?
        {
            return Ok(Outcome::Value(Value::Bool(false)));
        }
        let payload = self.evaluate(transaction, value)?;
        self.put_record(transaction, address.clone(), payload, span)?;
        if let Some(at) = at {
            transaction.expire_pending(&address, at);
        }
        Ok(match condition {
            Some(_) => Outcome::Value(Value::Bool(true)),
            None => Outcome::Done,
        })
    }

    /// `EXPIRE key when` — answers whether the key was there to expire.
    pub(crate) fn expire_key(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        when: &Expr,
    ) -> Result<Outcome> {
        let instant = self.instant(transaction, when)?;
        let (_, address) = self.writable(transaction, target)?;
        let Some(held) = transaction.get(&address)? else {
            return Ok(Outcome::Value(Value::Bool(false)));
        };
        match instant {
            Instant::Passed => transaction.delete(address),
            // A new version of the same bytes, carrying the instant: an expiry is
            // part of the version, so moving one is a write like any other and
            // reaches the log, a follower and a backup by the ordinary road.
            Instant::Future(at) => {
                transaction.put(address.clone(), held);
                transaction.expire_pending(&address, at);
            }
        }
        Ok(Outcome::Value(Value::Bool(true)))
    }

    /// `PERSIST key` — answers whether an expiry was cleared.
    pub(crate) fn persist_key(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<Outcome> {
        let (_, address) = self.writable(transaction, target)?;
        if transaction.expires(&address)?.is_none() {
            return Ok(Outcome::Value(Value::Bool(false)));
        }
        let Some(held) = transaction.get(&address)? else {
            return Ok(Outcome::Value(Value::Bool(false)));
        };
        transaction.put(address, held);
        Ok(Outcome::Value(Value::Bool(true)))
    }

    /// `TTL key` — the time left, `NULL` for never, `NONE` for no key.
    pub(crate) fn ttl_of(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<Value> {
        let (_, address) = self.address(transaction, target)?;
        if transaction.get(&address)?.is_none() {
            return Ok(Value::None);
        }
        let Some(at) = transaction.expires(&address)? else {
            return Ok(Value::Null);
        };
        let left = at.saturating_sub(transaction.clock());
        let seconds = i64::try_from(left / 1_000).unwrap_or(i64::MAX);
        let nanos = u32::try_from(left % 1_000)
            .unwrap_or(0)
            .saturating_mul(NANOS_PER_MILLI);
        Ok(Duration::new(seconds, nanos).map_or(Value::Null, Value::Duration))
    }

    /// Resolve an `EXPIRE` operand against the transaction's clock.
    fn instant(&self, transaction: &mut Transaction<'_>, when: &Expr) -> Result<Instant> {
        let refused = |reason| Error::InvalidExpiry {
            reason,
            span: when.span,
        };
        let too_far = || refused("the expiry is past what a millisecond clock can hold");
        let now = i64::try_from(transaction.clock()).map_err(|_| too_far())?;
        let at = match self.evaluate(transaction, when)? {
            Value::Duration(span) => {
                millis(span.seconds(), span.nanos()).and_then(|span| now.checked_add(span))
            }
            Value::Datetime(instant) => millis(instant.seconds(), instant.nanos()),
            _ => return Err(refused("an expiry is a duration or a datetime")),
        }
        .ok_or_else(too_far)?;
        if at <= now {
            return Ok(Instant::Passed);
        }
        Ok(Instant::Future(u64::try_from(at).map_err(|_| too_far())?))
    }
}

/// Whole milliseconds in a seconds-and-nanoseconds pair, rounded toward zero.
fn millis(seconds: i64, nanos: u32) -> Option<i64> {
    seconds
        .checked_mul(MILLIS_PER_SECOND)?
        .checked_add(i64::from(nanos / NANOS_PER_MILLI))
}
