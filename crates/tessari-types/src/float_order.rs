//! Ordering two floats without converting either to a decimal, where that is
//! certain to give the decimal comparison's answer.
//!
//! [`crate::Number`] orders by the decimal each number converts to, because that
//! is the normal form equality, hashing and the index encoder all share. The
//! conversion rounds a float to about fifteen significant digits, and to fewer
//! below `1e-13`, where the decimal's scale runs out — so two floats that differ
//! only past that point convert to one decimal and compare equal. Converting both
//! sides costs far more than the comparison: an exact nearest-neighbour read
//! spent a fifth of its time in it, ordering distances.
//!
//! The conversion is monotone, so when two floats are far enough apart that no
//! rounding can bring them together, the floats' own order IS the decimals'
//! order. [`apart`] answers only then, and leaves every other pair — equal,
//! close, tiny or beyond the decimal's range — to the conversion. [`ordered`]
//! applies it to two numbers: a float against a decimal or an integer is judged
//! on the other's float approximation, whose error is far inside the margin, and
//! two integers are simply compared, which is exact.

use core::cmp::Ordering;

use crate::Number;

/// The order of two finite numbers when it is certain without converting to
/// decimals, or `None` when only the conversion can say.
pub(crate) fn ordered(left: &Number, right: &Number) -> Option<Ordering> {
    match (left, right) {
        (Number::Integer(left), Number::Integer(right)) => Some(left.cmp(right)),
        (Number::Float(left), Number::Float(right)) => apart(*left, *right),
        (Number::Float(left), other) => apart(*left, other.as_float()?),
        (other, Number::Float(right)) => apart(other.as_float()?, *right),
        _ => None,
    }
}

/// Below this magnitude the decimal keeps fewer than sixteen significant digits.
const SMALLEST: f64 = 1e-12;

/// Above this magnitude the decimal's range is too near to reason about.
const LARGEST: f64 = 1e27;

/// How far apart, relative to the larger magnitude, two floats must be.
///
/// The conversion's rounding moves a value by at most one unit in its fifteenth
/// significant digit — about `1e-14` of it. Four orders of magnitude more is a
/// margin, not a measurement.
const APART: f64 = 1e-10;

/// The order of two finite floats when the decimal comparison is certain to agree
/// with it, or `None` when only the conversion can say.
pub(crate) fn apart(left: f64, right: f64) -> Option<Ordering> {
    let ordinary = |value: f64| value == 0.0 || (SMALLEST..=LARGEST).contains(&value.abs());
    if !(ordinary(left) && ordinary(right)) {
        return None;
    }
    let scale = left.abs().max(right.abs());
    if (left - right).abs() <= APART * scale {
        return None;
    }
    left.partial_cmp(&right)
}

#[cfg(test)]
mod tests {
    use core::cmp::Ordering;

    use rust_decimal::Decimal;

    use super::{apart, ordered};
    use crate::Number;

    /// The comparison `Number` falls back to.
    fn by_decimal(left: f64, right: f64) -> Option<Ordering> {
        Some(
            Decimal::try_from(left)
                .ok()?
                .cmp(&Decimal::try_from(right).ok()?),
        )
    }

    /// A deterministic stream of bits, so a failure names a reproducible pair.
    struct Mixer(u64);

    impl Mixer {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut mixed = self.0;
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            mixed ^ (mixed >> 31)
        }

        /// A float in `[0, 1)`.
        fn unit(&mut self) -> f64 {
            // Fifty-three random bits, the whole of a float's precision.
            f64::from_bits(0x3ff0_0000_0000_0000 | (self.next() >> 12)) - 1.0
        }

        /// A signed float whose magnitude is spread evenly over the exponents
        /// from `10^low` to `10^high`, so tiny, ordinary and huge values are
        /// drawn equally often.
        fn spread(&mut self, low: f64, high: f64) -> f64 {
            let magnitude = 10_f64.powf(low + (high - low) * self.unit());
            if self.next() & 1 == 0 {
                magnitude
            } else {
                -magnitude
            }
        }
    }

    /// How many pairs the fast path answered, so a run that never reached it
    /// cannot pass for one that agreed.
    static ANSWERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn agrees(left: f64, right: f64) {
        if let Some(fast) = apart(left, right) {
            ANSWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                Some(fast),
                by_decimal(left, right),
                "{left:e} against {right:e} ordered apart from the decimal comparison"
            );
        }
    }

    #[test]
    fn whatever_it_answers_the_decimal_comparison_answers_too() {
        let mut mixer = Mixer(41);
        for _ in 0..200_000 {
            // Across every magnitude, including below and above what the fast
            // path will answer for.
            let left = mixer.spread(-40.0, 30.0);
            agrees(left, mixer.spread(-40.0, 30.0));
            // Neighbours at every relative distance around the threshold, which
            // is where a rounding could bring two values together.
            let gap = 10_f64.powf(-17.0 + 9.0 * mixer.unit());
            agrees(left, left * (1.0 + gap));
            // A few ulps apart.
            let steps = mixer.next() % 64;
            agrees(left, f64::from_bits(left.to_bits().wrapping_add(steps)));
        }
        assert!(
            ANSWERED.load(std::sync::atomic::Ordering::Relaxed) > 50_000,
            "the fast path answered too few pairs for agreement to mean anything"
        );
    }

    #[test]
    fn two_numbers_of_any_kinds_are_ordered_as_their_decimals_are() {
        let mut mixer = Mixer(7);
        let mut answered = 0_usize;
        let mut check = |left: Number, right: Number| {
            if let Some(fast) = ordered(&left, &right) {
                answered = answered.saturating_add(1);
                let slow = left
                    .as_decimal()
                    .zip(right.as_decimal())
                    .map(|(l, r)| l.cmp(&r));
                assert_eq!(Some(fast), slow, "{left:?} against {right:?}");
            }
        };
        for _ in 0..100_000 {
            let float = mixer.spread(-40.0, 30.0);
            // A decimal near the float, at every relative distance.
            let gap = 10_f64.powf(-17.0 + 9.0 * mixer.unit());
            if let Ok(decimal) = Decimal::try_from(float * (1.0 + gap)) {
                check(Number::Float(float), Number::Decimal(decimal));
                check(Number::Decimal(decimal), Number::Float(float));
            }
            // An integer, and a float close to it.
            let whole = i64::from_ne_bytes(mixer.next().to_ne_bytes()) >> (mixer.next() % 63);
            if let Some(near) = Number::Integer(whole).as_float() {
                check(Number::Integer(whole), Number::Float(near * (1.0 + gap)));
            }
            check(Number::Integer(whole), Number::Integer(whole >> 1));
        }
        assert!(
            answered > 50_000,
            "only {answered} pairs reached the fast path"
        );
    }

    #[test]
    fn it_leaves_what_rounding_could_join_to_the_decimal() {
        // Two floats one step apart convert to one decimal and compare equal;
        // ordering them by the floats would disagree.
        let low = 0.123_456_789_012_345_67_f64;
        let high = f64::from_bits(low.to_bits().saturating_add(1));
        assert_eq!(by_decimal(low, high), Some(Ordering::Equal));
        assert_eq!(apart(low, high), None);
        // Below the decimal's scale everything is zero.
        assert_eq!(by_decimal(1e-40, 2e-40), Some(Ordering::Equal));
        assert_eq!(apart(1e-40, 2e-40), None);
        assert_eq!(apart(0.0, 1e-40), None);
    }

    #[test]
    fn distances_that_differ_are_ordered_without_the_decimal() {
        assert_eq!(apart(0.25, 0.5), Some(Ordering::Less));
        assert_eq!(apart(3.0, -3.0), Some(Ordering::Greater));
        assert_eq!(apart(0.0, 0.125), Some(Ordering::Less));
    }
}
