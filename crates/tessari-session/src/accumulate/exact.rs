//! An exact sum of floats, answered correctly rounded (ADR-0114).
//!
//! # Why exact
//!
//! Float addition is not associative, so a total built in record order is a
//! function of the order and not of the numbers: splitting the table, or folding
//! each shard on its leader and merging the parts, moves the last bits. Holding
//! the sum exactly removes the order from the answer — the same numbers give the
//! same bits however they arrive — which is what lets a fold over floats travel
//! to the shards and merge into the answer one walk gives.
//!
//! # How
//!
//! The total is a list of floats whose exact sum is the exact sum of everything
//! offered: non-overlapping, in increasing magnitude, none of them zero. Adding a
//! number walks the list with an error-free addition, keeping each rounding error
//! as a new part. On real data the list is one to three floats long; the exponent
//! range bounds it at a few dozen. The answer rounds the list once, to nearest
//! with ties to even, which is the only rounding the result ever takes.
//!
//! Infinities and not-a-number are kept apart and added as IEEE adds them, so a
//! total over `inf` is `inf` and over `inf` and `-inf` is not a number. Two
//! finite numbers whose total no float holds are recorded, and the total then
//! refuses rather than answering an infinity nobody offered.

use tessari_types::{Number, Value};

/// What overflowing the float range is called when the total is asked for.
const OUTSIDE: &str = "a total outside the float range";

/// The most parts a total from another node may carry. Non-overlapping parts
/// each cover at least one bit of the 2 098 a float spans, in practice 53, so a
/// real total has at most about forty; a state with more is refused, because
/// squaring a total costs the square of its parts.
const MOST_PARTS: usize = 64;

/// An exact running total of floats.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ExactSum {
    /// Non-overlapping parts in increasing magnitude, whose exact sum is the
    /// total; none of them is zero.
    parts: Vec<f64>,
    /// The infinities and not-a-numbers offered, added together.
    special: Option<f64>,
    /// Whether two finite numbers ever added to more than a float holds.
    overflowed: bool,
}

impl ExactSum {
    /// Add one number, exactly.
    pub(crate) fn add(&mut self, value: f64) {
        if !value.is_finite() {
            self.special = Some(self.special.map_or(value, |held| held + value));
            return;
        }
        let mut carried = value;
        let mut kept = 0_usize;
        for at in 0..self.parts.len() {
            let held = self.parts.get(at).copied().unwrap_or(0.0);
            let (larger, smaller) = if carried.abs() < held.abs() {
                (held, carried)
            } else {
                (carried, held)
            };
            let high = larger + smaller;
            if !high.is_finite() {
                self.overflowed = true;
                return;
            }
            let low = smaller - (high - larger);
            if low != 0.0 {
                if let Some(slot) = self.parts.get_mut(kept) {
                    *slot = low;
                }
                kept = kept.saturating_add(1);
            }
            carried = high;
        }
        self.parts.truncate(kept);
        if carried != 0.0 {
            self.parts.push(carried);
        }
    }

    /// Add the exact product of two numbers — the rounded product and the
    /// error a fused multiply-add recovers from it.
    pub(crate) fn add_product(&mut self, left: f64, right: f64) {
        let product = left * right;
        if !product.is_finite() && left.is_finite() && right.is_finite() {
            self.overflowed = true;
            return;
        }
        self.add(product);
        if product.is_finite() {
            self.add(left.mul_add(right, -product));
        }
    }

    /// Fold in another total, as if its numbers had been offered here.
    pub(crate) fn absorb(&mut self, other: &Self) {
        for part in &other.parts {
            self.add(*part);
        }
        if let Some(special) = other.special {
            self.add(special);
        }
        self.overflowed = self.overflowed || other.overflowed;
    }

    /// This total multiplied by `by`, exactly.
    pub(crate) fn scaled(&self, by: f64) -> Self {
        let mut scaled = Self {
            special: self.special.map(|special| special * by),
            overflowed: self.overflowed,
            ..Self::default()
        };
        for part in &self.parts {
            scaled.add_product(*part, by);
        }
        scaled
    }

    /// The square of this total, exactly.
    pub(crate) fn squared(&self) -> Self {
        let mut squared = Self {
            special: self.special.map(|special| special * special),
            overflowed: self.overflowed,
            ..Self::default()
        };
        for left in &self.parts {
            for right in &self.parts {
                squared.add_product(*left, *right);
            }
        }
        squared
    }

    /// This total with its sign turned over.
    pub(crate) fn negated(&self) -> Self {
        Self {
            parts: self.parts.iter().map(|part| -part).collect(),
            special: self.special.map(|special| -special),
            overflowed: self.overflowed,
        }
    }

    /// Whether an infinity or a not-a-number was offered.
    pub(crate) const fn has_special(&self) -> bool {
        self.special.is_some()
    }

    /// The total, rounded once to the nearest float, ties to even.
    ///
    /// # Errors
    ///
    /// When two finite numbers added to more than a float holds.
    pub(crate) fn total(&self) -> Result<f64, &'static str> {
        if let Some(special) = self.special {
            return Ok(special);
        }
        if self.overflowed {
            return Err(OUTSIDE);
        }
        let Some((&top, mut below)) = self.parts.split_last() else {
            return Ok(0.0);
        };
        let mut high = top;
        let mut low = 0.0_f64;
        while let Some((&next, rest)) = below.split_last() {
            let before = high;
            high = before + next;
            low = next - (high - before);
            below = rest;
            if low != 0.0 {
                break;
            }
        }
        // A tie the rounding above broke toward the larger part may be no tie
        // at all once the parts below it are counted: when they lean the same
        // way as the remainder, the exact total is past the halfway point and
        // rounds the other way.
        if let Some(&next) = below.last()
            && ((low < 0.0 && next < 0.0) || (low > 0.0 && next > 0.0))
        {
            let doubled = low * 2.0;
            let moved = high + doubled;
            if doubled == moved - high {
                high = moved;
            }
        }
        Ok(high)
    }

    /// What this total holds, as a value that can travel to another node.
    ///
    /// `[parts, special]`, the parts as floats and the special `NONE` when no
    /// infinity was offered; `None` when the total already overflowed, because
    /// a total that cannot answer has nothing exact to send.
    pub(crate) fn state(&self) -> Option<Value> {
        if self.overflowed {
            return None;
        }
        let parts = self
            .parts
            .iter()
            .map(|part| Value::Number(Number::Float(*part)))
            .collect();
        let special = self
            .special
            .map_or(Value::None, |special| Value::Number(Number::Float(special)));
        Some(Value::Array(vec![Value::Array(parts), special]))
    }

    /// A total another node sent as [`Self::state`].
    ///
    /// Rebuilt by adding each part rather than adopted, so a malformed list —
    /// overlapping, unordered, holding a zero — still adds up to exactly the
    /// total it states.
    pub(crate) fn from_state(state: &Value) -> Option<Self> {
        let Value::Array(held) = state else {
            return None;
        };
        let [Value::Array(parts), special] = held.as_slice() else {
            return None;
        };
        if parts.len() > MOST_PARTS {
            return None;
        }
        let mut total = Self::default();
        for part in parts {
            let Value::Number(Number::Float(part)) = part else {
                return None;
            };
            if !part.is_finite() {
                return None;
            }
            total.add(*part);
        }
        match special {
            Value::None => {}
            Value::Number(Number::Float(special)) if !special.is_finite() => {
                total.add(*special);
            }
            _ => return None,
        }
        Some(total)
    }
}

#[cfg(test)]
pub(crate) mod oracle;
#[cfg(test)]
mod tests;
