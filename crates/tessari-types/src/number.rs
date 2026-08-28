//! Numbers, and how they compare to each other.
//!
//! Three kinds live under one type: an integer, a binary float, and an exact
//! decimal. They are separate kinds because they answer different questions —
//! a count, a measurement, and an amount of money are not interchangeable — and
//! collapsing them into one would either lose exactness or lose range.
//!
//! # Comparison across kinds is defined, not avoided
//!
//! The tempting shortcut is to order numbers by kind first and by value second,
//! which is total, trivial to implement, and wrong in a way nothing catches: it
//! makes `1 < 1.5` false and turns any range scan over a column holding two
//! kinds into a silent lie. So comparison here is *semantic* — `1`, `1.0` and
//! `1.00` are one value, whichever kinds they arrive as.
//!
//! The rule, stated so a caller can rely on it:
//!
//! - Integers and decimals compare **exactly**. An integer converts to a
//!   decimal without loss.
//! - A finite float compares by converting to a decimal. Within a decimal's
//!   precision this is exact; a float whose binary expansion needs more digits
//!   than a decimal carries compares by its rounded value, and that is the one
//!   approximation in this type.
//! - A float too large for a decimal to hold is ordered by its sign, which is
//!   unambiguous: nothing representable as a decimal reaches that magnitude.
//! - Infinities and not-a-number have declared places: `-∞` below every number,
//!   `+∞` above every number, and `NaN` above `+∞`. `NaN` has to go somewhere
//!   for the order to be total, and putting it at one end keeps it out of the
//!   middle of a range scan.
//!
//! # Two values are normalised on the way in
//!
//! Negative zero becomes zero, and every not-a-number becomes one canonical
//! not-a-number. Both exist so that equality and ordering agree: without the
//! first, `0.0` and `-0.0` would be unequal; without the second, two
//! not-a-numbers with different bit patterns would be unequal while both being
//! "not a number".

use core::cmp::Ordering;
use core::fmt;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive as _;

/// A number: an integer, a float, or an exact decimal.
#[derive(Debug, Clone)]
pub enum Number {
    /// A signed integer.
    Integer(i64),
    /// A binary floating-point number.
    ///
    /// Constructed through [`Number::float`], which normalises negative zero
    /// and not-a-number.
    Float(f64),
    /// An exact decimal.
    Decimal(Decimal),
}

impl Number {
    /// Wrap a float, normalising the two values that would otherwise make
    /// equality and ordering disagree.
    #[must_use]
    pub fn float(value: f64) -> Self {
        if value.is_nan() {
            return Self::Float(f64::NAN);
        }
        if value == 0.0 {
            // Catches negative zero, which compares equal to zero but does not
            // order equal to it under a total float order.
            return Self::Float(0.0);
        }
        Self::Float(value)
    }

    /// Whether this is a not-a-number float.
    #[must_use]
    pub fn is_nan(&self) -> bool {
        matches!(self, Self::Float(value) if value.is_nan())
    }

    /// Where this number sits relative to every finite number.
    fn position(&self) -> Position {
        match self {
            Self::Integer(_) | Self::Decimal(_) => Position::Finite,
            Self::Float(value) => {
                if value.is_nan() {
                    Position::NotANumber
                } else if *value == f64::INFINITY {
                    Position::AboveEverything
                } else if *value == f64::NEG_INFINITY {
                    Position::BelowEverything
                } else {
                    Position::Finite
                }
            }
        }
    }

    /// This number as an exact decimal, when one can hold it.
    ///
    /// `None` for a non-finite float, and for a finite float whose magnitude is
    /// beyond what a decimal represents.
    ///
    /// This is the **normal form comparison uses**, which is why it is public:
    /// an index encoder has to produce identical bytes for numbers that compare
    /// equal, and the only way to be sure of that is to encode the same form the
    /// comparison reduces to. Deriving the bytes independently would agree in
    /// every obvious case and disagree in the ones that matter — a float that
    /// underflows a decimal compares equal to zero here, and an encoder that
    /// used the float's own digits would place it just above zero instead.
    #[must_use]
    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Self::Integer(value) => Some(Decimal::from(*value)),
            Self::Decimal(value) => Some(*value),
            Self::Float(value) => Decimal::try_from(*value).ok(),
        }
    }

    /// This number as an integer, when it is one exactly.
    ///
    /// `None` for a number with a fractional part, for one outside the integer
    /// range, and for the three floats that sit outside the finite numbers.
    ///
    /// **A fraction is refused rather than truncated.** The language already has
    /// `math::floor`, `math::ceil` and `math::round` to say which whole number
    /// was meant, so a conversion that quietly picked one of the three would be
    /// answering a question nobody asked — and answering it with a number
    /// indistinguishable from a correct one.
    ///
    /// Decided through [`Number::as_decimal`] rather than per kind, so that "is
    /// this a whole number" cannot disagree with "does this compare equal to
    /// that whole number".
    #[must_use]
    pub fn as_exact_integer(&self) -> Option<i64> {
        let exact = self.as_decimal()?;
        if exact.fract() != Decimal::ZERO {
            return None;
        }
        exact.to_i64()
    }

    /// This number as a float.
    ///
    /// A float already is one — infinities and not-a-number included, since
    /// those are floats and nothing is lost in saying so.
    ///
    /// **A decimal converts to its nearest float**, which is the one place this
    /// type rounds, and it rounds here for the same reason the module header
    /// gives for comparison: a decimal like `19.99` has no exact float at all,
    /// so a rule demanding exactness would refuse nearly every decimal anybody
    /// holds and leave no way to express the conversion. The nearest float is
    /// what "as a float" means for a decimal.
    ///
    /// **An integer converts only when a float holds it exactly.** The
    /// asymmetry is not a matter of taste: past 2^53 consecutive integers share
    /// one float, so the answer would be a *different integer* rather than a
    /// nearby quantity — `9007199254740993` coming back as
    /// `9007199254740992` — and integers here are counts and identities, where
    /// off by one is a wrong answer and not a rounding.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            Self::Decimal(value) => value.to_f64(),
            Self::Integer(value) => {
                // 2^53: the last integer with a float of its own.
                const EXACT: i64 = 9_007_199_254_740_992;
                if *value > EXACT || *value < -EXACT {
                    return None;
                }
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the bound above is what makes this conversion exact"
                )]
                Some(*value as f64)
            }
        }
    }

    /// Whether this number sits among the ordinary numbers rather than at one of
    /// the three edges the order places outside them.
    ///
    /// Exposed because a caller that wants to precompute the projection this
    /// order is defined by has to know which numbers *have* one — and the three
    /// that do not are decided here, so asking anywhere else would be a second
    /// answer to one question.
    #[must_use]
    pub fn position_is_finite(&self) -> bool {
        self.position() == Position::Finite
    }

    /// The sign of a finite float that no decimal can hold.
    fn sign_of_huge_float(&self) -> Option<Ordering> {
        match self {
            Self::Float(value) if value.is_finite() => Some(value.total_cmp(&0.0)),
            _ => None,
        }
    }
}

/// Where a number sits on the line, before its magnitude is consulted.
///
/// Ordered so that the derived comparison is the intended one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, core::hash::Hash)]
enum Position {
    BelowEverything,
    Finite,
    AboveEverything,
    NotANumber,
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Number {}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        let positions = self.position().cmp(&other.position());
        if positions != Ordering::Equal {
            return positions;
        }
        // Same position. Only `Finite` has anything left to compare — the other
        // three each hold exactly one value.
        if self.position() != Position::Finite {
            return Ordering::Equal;
        }
        match (self.as_decimal(), other.as_decimal()) {
            (Some(left), Some(right)) => left.cmp(&right),
            // One side is a finite float beyond decimal range. Nothing a decimal
            // can hold reaches that magnitude, so its sign settles it.
            (None, Some(_)) => self.sign_of_huge_float().unwrap_or(Ordering::Equal),
            (Some(_), None) => other
                .sign_of_huge_float()
                .unwrap_or(Ordering::Equal)
                .reverse(),
            (None, None) => match (self, other) {
                (Self::Float(left), Self::Float(right)) => left.total_cmp(right),
                // Unreachable: only a float can fail to become a decimal.
                _ => Ordering::Equal,
            },
        }
    }
}

impl core::hash::Hash for Number {
    /// Hashes the value, not the kind, so that two numbers that compare equal
    /// hash equal — the contract `Eq` and `Hash` are required to keep together.
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.position().hash(state);
        match self.as_decimal() {
            Some(decimal) => decimal.normalize().hash(state),
            None => match self {
                Self::Float(value) => value.to_bits().hash(state),
                _ => 0_u8.hash(state),
            },
        }
    }
}

impl From<i64> for Number {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<Decimal> for Number {
    fn from(value: Decimal) -> Self {
        Self::Decimal(value)
    }
}

impl From<f64> for Number {
    fn from(value: f64) -> Self {
        Self::float(value)
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::Decimal(value) => write!(f, "{value}"),
        }
    }
}
