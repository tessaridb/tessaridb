//! Time for the functions that read or build one.

use super::arguments::{datetime_at, number_at};
use crate::error::{Error, Result};
use tessari_ql::{Function, Span};
use tessari_types::{Datetime, Duration, Number, Value};

/// The instant this statement is being evaluated at.
///
/// Read once, in the session, so the value that reaches the log is a value like
/// any other — a replica applies what was written rather than asking its own
/// clock and reaching a different answer. A clock before the epoch is refused
/// rather than folded to zero: a machine whose time is wrong should say so.
/// The start of the window `instant` falls in.
///
/// Windows are anchored at the **epoch**, not at the first record, so the same
/// instant lands in the same window in every query, every process and every
/// replica. A window anchored at whatever data happened to arrive first would
/// give two callers different answers to the same question, and neither would
/// notice.
///
/// Truncation is toward negative infinity — `div_euclid` and not division — so
/// an instant before the epoch lands in the window that *contains* it rather
/// than the one after it. That is the difference between a window boundary and
/// an off-by-one nobody sees until they query a date in 1969.
pub(crate) fn bucket(
    function: Function,
    instant: &tessari_types::Datetime,
    width: &tessari_types::Duration,
    span: Span,
) -> Result<Value> {
    let seconds = width.seconds();
    if seconds <= 0 && width.nanos() == 0 {
        return Err(Error::CallFailed {
            function,
            reason: "a window has to be longer than nothing",
            span,
        });
    }
    // Whole seconds only: a sub-second window is a real thing and needs the
    // nanosecond remainder in the arithmetic, which is a different function from
    // the one anybody asks for. Refused rather than rounded, because rounding
    // here would silently answer a question nobody asked.
    if width.nanos() != 0 {
        return Err(Error::CallFailed {
            function,
            reason: "a window is a whole number of seconds",
            span,
        });
    }
    let start = instant
        .seconds()
        .div_euclid(seconds)
        .saturating_mul(seconds);
    tessari_types::Datetime::new(start, 0).map_or_else(
        || {
            Err(Error::CallFailed {
                function,
                reason: "that window does not start at an instant this type holds",
                span,
            })
        },
        |held| Ok(Value::Datetime(held)),
    )
}

/// One field of the date an instant falls on.
///
/// The date is read by [`Datetime::civil`], which is the store's single answer
/// to how a second count becomes a calendar date. Deriving it here as well would
/// be a second copy of the era arithmetic, and the way two copies fail is that
/// they disagree on one day in four hundred years while both keep answering.
pub(crate) fn reading(
    function: Function,
    arguments: &[Value],
    span: Span,
    take: impl Fn(tessari_types::Civil) -> i64,
) -> Result<Value> {
    let instant = datetime_at(function, arguments, 0, span)?;
    Ok(Value::Number(Number::Integer(take(instant.civil()))))
}

/// A whole-number reading of the instant itself rather than of its civil
/// breakdown — the day-of-week family, whose arithmetic wants the day count
/// the breakdown has already spent.
pub(crate) fn instant_reading(
    function: Function,
    arguments: &[Value],
    span: Span,
    take: impl Fn(Datetime) -> Option<i64>,
) -> Result<Value> {
    let instant = datetime_at(function, arguments, 0, span)?;
    take(*instant)
        .map(|held| Value::Number(Number::Integer(held)))
        .ok_or(Error::CallFailed {
            function,
            reason: "that reading is past what the calendar arithmetic holds",
            span,
        })
}

/// The start of the calendar unit an instant falls in. `None` from the
/// reading is a start the type cannot hold, refused rather than clamped for
/// `from_unix`'s reason: a clamped instant is a moment nobody wrote.
pub(crate) fn start_of(
    function: Function,
    arguments: &[Value],
    span: Span,
    take: impl Fn(Datetime) -> Option<Datetime>,
) -> Result<Value> {
    let instant = datetime_at(function, arguments, 0, span)?;
    take(*instant)
        .map(Value::Datetime)
        .ok_or(Error::CallFailed {
            function,
            reason: "that start is past what an instant holds",
            span,
        })
}

/// The instant a second count names.
///
/// A fraction is refused rather than truncated, on the rule the casts follow:
/// `math::round` already says which whole number was meant, so choosing one here
/// would answer a question the caller did not ask. A count no instant holds is
/// refused for the same reason it is never clamped — a clamped instant is a
/// moment the author did not write, sitting in a record that will be read back.
pub(crate) fn from_unix(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let number = number_at(function, arguments, 0, span)?;
    let Some(seconds) = number.as_exact_integer() else {
        return Err(Error::CallFailed {
            function,
            reason: "a unix time is a whole number of seconds an instant holds; \
                     math::round says which whole one was meant",
            span,
        });
    };
    Ok(Value::Datetime(Datetime::from_seconds(seconds)))
}

/// `duration::from_secs(n)` — the span `n` seconds names, whole or fractional,
/// to the nanosecond (ADR-0124 D4).
///
/// Computed in exact decimal rather than in floating point, so `1.5` is exactly
/// 1 500 milliseconds; a count past what a span holds is refused, never wrapped.
pub(crate) fn from_secs(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let number = number_at(function, arguments, 0, span)?;
    let outside = || Error::CallFailed {
        function,
        reason: "that many seconds is past what a duration can hold",
        span,
    };
    let nanos = number
        .as_decimal()
        .and_then(|seconds| seconds.checked_mul(rust_decimal::Decimal::from(1_000_000_000_u32)))
        .map(|nanos| nanos.round())
        .and_then(|nanos| rust_decimal::prelude::ToPrimitive::to_i128(&nanos))
        .ok_or_else(outside)?;
    Duration::from_nanos(nanos)
        .map(Value::Duration)
        .ok_or_else(outside)
}

/// The instant this process's clock reads.
///
/// Shared with the queue engine, which needs the same instant for the same
/// reason `time::now()` gives it to a statement: a claim's deadline is computed
/// once here and **written**, so the value that reaches the log is one every
/// node agrees about rather than a computation each of them repeats against its
/// own clock.
///
/// It reports through [`Function::TimeNow`] whoever asks, because there is one
/// clock and a caller reading a failure wants to know which one could not be
/// read — not which internal path asked it.
pub(crate) fn instant(span: Span) -> Result<Datetime> {
    instant_of(std::time::SystemTime::now(), span)
}

/// The instant `reading` names: what `time::now()` answers from the clock its
/// statement read once (DB-21).
pub(crate) fn instant_of(reading: std::time::SystemTime, span: Span) -> Result<Datetime> {
    let failed = |reason: &'static str| Error::CallFailed {
        function: Function::TimeNow,
        reason,
        span,
    };
    let since = reading
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| failed("the clock is before the epoch"))?;
    let seconds = i64::try_from(since.as_secs()).map_err(|_| failed("the clock is unreadable"))?;
    Datetime::new(seconds, since.subsec_nanos()).ok_or_else(|| failed("the clock is unreadable"))
}

/// The instant as text, in UTC, by a strftime-shaped pattern.
///
/// The token set is deliberately a subset — `%Y %m %d %H %M %S` zero-padded,
/// `%f` the nanosecond remainder as nine digits, `%%` a literal percent — and
/// a token outside it is refused rather than copied through: a `%q` passed
/// verbatim would read as a formatted answer, which is the one thing a
/// formatting function must never produce. Any other character copies as it
/// stands. UTC because a [`Datetime`] has no zone; a zone is a rendering
/// choice made where the text is shown, and this text is the stored moment.
pub(crate) fn format(
    function: Function,
    instant: &Datetime,
    pattern: &str,
    span: Span,
) -> Result<Value> {
    let civil = instant.civil();
    let mut written = String::with_capacity(pattern.len().saturating_add(16));
    let mut tokens = pattern.chars();
    while let Some(held) = tokens.next() {
        if held != '%' {
            written.push(held);
            continue;
        }
        let Some(token) = tokens.next() else {
            return Err(Error::CallFailed {
                function,
                reason: "a pattern that ends on a bare %",
                span,
            });
        };
        match token {
            'Y' => written.push_str(&format!("{:04}", civil.year)),
            'm' => written.push_str(&format!("{:02}", civil.month)),
            'd' => written.push_str(&format!("{:02}", civil.day)),
            'H' => written.push_str(&format!("{:02}", civil.hour)),
            'M' => written.push_str(&format!("{:02}", civil.minute)),
            'S' => written.push_str(&format!("{:02}", civil.second)),
            'f' => written.push_str(&format!("{:09}", instant.nanos())),
            '%' => written.push('%'),
            _ => {
                return Err(Error::CallFailed {
                    function,
                    reason: "a pattern token outside %Y %m %d %H %M %S %f %%",
                    span,
                });
            }
        }
    }
    Ok(Value::String(written))
}
