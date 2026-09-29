//! The arithmetic folds share: running sums and Welford's variance.

use super::{Running, exact, exactly};
use crate::aggregate::{approximate, present};
use crate::error::{Error, Result};
use rust_decimal::Decimal;
use tessari_ql::Span;
use tessari_types::{Number, Value};

/// Welford's running state: a count, a mean, and the sum of squared deviations.
///
/// Three numbers however long the group is, one pass, and numerically stable.
/// The textbook `E[x²] − E[x]²` form is one subtraction of two large nearly
/// equal numbers and loses every significant digit of the answer on data whose
/// spread is small relative to its magnitude — timestamps and prices, which is
/// most of what anybody takes a variance of.
///
/// Carried in `f64` and not in the exact decimal the other numeric folds use,
/// because `stddev` takes a square root and no square root is exact. A
/// `variance` that promised exactness while the `stddev` beside it could not
/// would be two answers with two different promises out of one pair of folds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Welford {
    /// How many numbers went in.
    pub(crate) counted: u64,
    /// Their running mean.
    pub(crate) mean: f64,
    /// The running sum of squared deviations from that mean.
    pub(crate) m2: f64,
}

impl Welford {
    /// The state before any number has arrived.
    pub(crate) const fn new() -> Self {
        Self {
            counted: 0,
            mean: 0.0,
            m2: 0.0,
        }
    }

    /// Fold one more number in.
    pub(crate) fn offer(&mut self, held: f64) {
        self.counted = self.counted.saturating_add(1);
        // The one lossy conversion here, and it is the divisor of the running
        // mean. Beyond 2^53 a count no longer increments exactly in `f64` — but
        // by then the sum of squared deviations it divides has lost far more,
        // so refusing the cast would buy nothing and there is no wider float.
        #[expect(
            clippy::cast_precision_loss,
            clippy::as_conversions,
            reason = "a count past 2^53 has already made every other number here meaningless"
        )]
        let counted = self.counted as f64;
        let first = held - self.mean;
        self.mean += first / counted;
        let second = held - self.mean;
        self.m2 += first * second;
    }

    /// The sample variance, or `None` when fewer than two numbers arrived.
    ///
    /// `NONE` rather than zero over one value, by the same rule `mean` follows
    /// over none: the spread of a single observation is not zero, it is a
    /// question nobody has enough data to answer, and zero would be a claim.
    pub(crate) fn variance(self) -> Option<f64> {
        if self.counted < 2 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            clippy::as_conversions,
            reason = "as above — the divisor is a count, and it is at least one here"
        )]
        let degrees = self.counted.saturating_sub(1) as f64;
        Some(self.m2 / degrees)
    }
}

/// The middle of these numbers, or `NONE` when there are none.
///
/// An even count answers the mean of the two middles, which is a value that was
/// never in the data. That is tolerable only because the fold is numeric, and it
/// is why `median` refuses the kinds `min` and `max` accept: the same rule over
/// a `datetime` or a `uuid` column would have to construct a value of a kind
/// that has no arithmetic.
///
/// # Why the answer is exact and normalised rather than the value as written
///
/// Answering with the middle value untouched — a column of integers having an
/// integer median, the way `min` and `max` keep what they were given — is the
/// obvious design and it is **not a function of the data**. This store's `Value`
/// deliberately orders and equates numbers across kinds, so `3`, `3.0` and the
/// decimal `3.0` are three equal values that are three different answers on the
/// wire; asked for the middle of those three, "the value as written" is decided
/// entirely by where a sort happened to leave them. The corpus has that row on
/// purpose and it is what caught this.
///
/// So `median` answers exactly, like `mean`, and normalises the result, so that
/// the same multiset of numbers gives the same answer whatever order the records
/// arrive in and whichever kinds they were written as. It costs the kind and buys
/// a determinism the alternative cannot state — and it is the same promise the
/// other numeric fold already makes, including the same refusal of a number no
/// exact form holds.
pub(crate) fn middle(held: &[Value], span: Span) -> Result<Value> {
    if held.is_empty() {
        // Nothing to be in the middle of, and zero would be a claim — the rule
        // `mean` already follows over an empty group.
        return Ok(Value::None);
    }
    let mut sorted = Vec::with_capacity(held.len());
    for value in held {
        sorted.push(exact(value, span)?);
    }
    sorted.sort_unstable();
    let at = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        return Ok(sorted.get(at).map_or(Value::None, exactly));
    }
    let (Some(lower), Some(upper)) = (sorted.get(at.saturating_sub(1)), sorted.get(at)) else {
        return Ok(Value::None);
    };
    let pair = lower
        .checked_add(*upper)
        .ok_or_else(|| failed("median", "a total outside the exact range", span))?;
    let averaged = pair
        .checked_div(Decimal::from(2))
        .ok_or_else(|| failed("median", "a group of no size", span))?;
    Ok(exactly(&averaged))
}

/// The number this value offers a numeric fold, or nothing when it offers none.
///
/// Absent and null are both "no value here" and every numeric fold passes over
/// them; anything else present that is not a number is refused, which is where
/// the batch fold refuses it too.
pub(crate) fn summable<'a>(
    value: &'a Value,
    fold: &'static str,
    span: Span,
) -> Result<Option<&'a Number>> {
    if !present(value) {
        return Ok(None);
    }
    let Value::Number(number) = value else {
        return Err(Error::NotSummable {
            fold,
            found: value.type_name(),
            span,
        });
    };
    Ok(Some(number))
}

/// Add one number to an exact running total, remembering a failure rather than
/// raising it.
pub(crate) fn add_exact(running: &mut Running<Decimal>, number: &Number) {
    let Ok(total) = running else {
        return;
    };
    let next = number
        .as_decimal()
        .ok_or("a number outside the exact range")
        .and_then(|held| {
            total
                .checked_add(held)
                .ok_or("a total outside the exact range")
        });
    *running = next;
}

/// Add one number to a float running total, in the order the values arrived.
pub(crate) fn add_float(running: &mut Running<f64>, number: &Number) {
    let Ok(total) = running else {
        return;
    };
    let Some(held) = approximate(number) else {
        *running = Err("a number no float can hold");
        return;
    };
    *total += held;
}

/// A failure a total carried until it turned out to be the answer.
pub(crate) fn failed(fold: &'static str, found: &'static str, span: Span) -> Error {
    Error::NotSummable { fold, found, span }
}
