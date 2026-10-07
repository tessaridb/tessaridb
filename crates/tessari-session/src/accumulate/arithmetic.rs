//! The arithmetic folds share: running sums and the two moments a spread needs.

use super::exact::ExactSum;
use super::{Running, exact, exactly};
use crate::aggregate::{approximate, present};
use crate::error::{Error, Result};
use rust_decimal::Decimal;
use tessari_ql::Span;
use tessari_types::{Number, Value};

/// What a spread needs: a count, the exact sum of the numbers and the exact sum
/// of their squares (ADR-0114 D4).
///
/// The textbook `E[x²] − E[x]²` form loses every significant digit of the
/// answer when it is computed in floats on data whose spread is small next to
/// its magnitude — timestamps and prices, which is most of what anybody takes a
/// variance of. Here the subtraction is made on exact totals and rounded once,
/// so there is nothing to lose; and exact totals add in any order, which is what
/// lets a spread merge across shards into the bits one walk gives.
///
/// The numbers enter as the floats they convert to, because `stddev` takes a
/// square root and no square root is exact. A `variance` that promised more
/// than the float it answers while the `stddev` beside it could not would be
/// two answers with two different promises out of one pair of folds.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Moments {
    /// How many numbers went in.
    pub(crate) counted: u64,
    /// Their exact sum.
    pub(crate) total: ExactSum,
    /// The exact sum of their squares.
    pub(crate) squares: ExactSum,
}

impl Moments {
    /// Fold one more number in.
    pub(crate) fn offer(&mut self, held: f64) {
        self.counted = self.counted.saturating_add(1);
        self.total.add(held);
        self.squares.add_product(held, held);
    }

    /// Fold in the moments another walk reached.
    pub(crate) fn absorb(&mut self, other: &Self) {
        self.counted = self.counted.saturating_add(other.counted);
        self.total.absorb(&other.total);
        self.squares.absorb(&other.squares);
    }

    /// The sample variance, or `None` when fewer than two numbers arrived.
    ///
    /// `NONE` rather than zero over one value, by the same rule `mean` follows
    /// over none: the spread of a single observation is not zero, it is a
    /// question nobody has enough data to answer, and zero would be a claim.
    ///
    /// # Errors
    ///
    /// When a square or a total went past what a float holds.
    pub(crate) fn variance(&self) -> core::result::Result<Option<f64>, &'static str> {
        if self.counted < 2 {
            return Ok(None);
        }
        if self.total.has_special() || self.squares.has_special() {
            // An infinity or a not-a-number has no spread around it.
            return Ok(Some(f64::NAN));
        }
        // n·Σx² − (Σx)², formed exactly: the count enters as two halves that
        // are each a float exactly, so no part of the product is rounded.
        let (high, low) = halves(self.counted);
        let mut numerator = self.squares.scaled(high);
        numerator.absorb(&self.squares.scaled(low));
        numerator.absorb(&self.total.squared().negated());
        let numerator = numerator
            .total()
            .map_err(|_| "a spread outside the float range")?;
        let counted = count(self.counted);
        let degrees = counted * count(self.counted.saturating_sub(1));
        // Never below zero when exact; a square that fell under the smallest
        // float lost bits, and a spread that rounding left negative is none.
        Ok(Some(numerator.max(0.0) / degrees))
    }
}

/// A count as two floats, each exact, that add to it: the high 32 bits scaled
/// and the low 32 bits.
fn halves(counted: u64) -> (f64, f64) {
    let high = u32::try_from(counted >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(counted & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    (f64::from(high) * 4_294_967_296.0, f64::from(low))
}

/// A count as the nearest float — exact up to 2^53, rounded past it.
pub(crate) fn count(counted: u64) -> f64 {
    let (high, low) = halves(counted);
    high + low
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
pub(crate) fn middle(
    held: &[Value],
    runs: &std::collections::BTreeMap<Decimal, u64>,
    span: Span,
) -> Result<Value> {
    let mut counted = runs.clone();
    for value in held {
        let entry = counted.entry(exact(value, span)?).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    let size: u64 = counted
        .values()
        .fold(0, |size, many| size.saturating_add(*many));
    if size == 0 {
        // Nothing to be in the middle of, and zero would be a claim — the rule
        // `mean` already follows over an empty group.
        return Ok(Value::None);
    }
    let at = size / 2;
    let upper = ranked(&counted, at);
    if size % 2 == 1 {
        return Ok(upper.map_or(Value::None, exactly));
    }
    let (Some(lower), Some(upper)) = (ranked(&counted, at.saturating_sub(1)), upper) else {
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

/// The value at zero-based `rank` of a multiset held as value → how many.
fn ranked(counted: &std::collections::BTreeMap<Decimal, u64>, rank: u64) -> Option<&Decimal> {
    let mut below = 0_u64;
    for (value, many) in counted {
        below = below.saturating_add(*many);
        if rank < below {
            return Some(value);
        }
    }
    None
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

/// Add one number to an exact float total, remembering a failure rather than
/// raising it.
pub(crate) fn add_float(running: &mut Running<ExactSum>, number: &Number) {
    let Ok(total) = running else {
        return;
    };
    let Some(held) = approximate(number) else {
        *running = Err("a number no float can hold");
        return;
    };
    total.add(held);
}

/// A failure a total carried until it turned out to be the answer.
pub(crate) fn failed(fold: &'static str, found: &'static str, span: Span) -> Error {
    Error::NotSummable { fold, found, span }
}
