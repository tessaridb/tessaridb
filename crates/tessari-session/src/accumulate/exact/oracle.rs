//! An exact sum by a route that shares no arithmetic with [`super::ExactSum`]:
//! a fixed-point integer wide enough for every finite float — the smallest
//! subnormal is its unit — that adds by integer carries and rounds by reading
//! bits. Test-only; it is the oracle the exact total and the batch reference
//! fold are both held to.

#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "an oracle over fixed-size limbs whose indices are bounded by construction"
)]

/// Limbs of the oracle: 2 176 bits, past the 2 098 a float spans plus carries.
const LIMBS: usize = 34;

/// A two's-complement fixed-point number whose unit is 2^-1074.
#[derive(Clone)]
pub(crate) struct Wide([u64; LIMBS]);

impl Wide {
    pub(crate) fn zero() -> Self {
        Self([0; LIMBS])
    }

    /// Add `value`, which must be finite.
    pub(crate) fn add(&mut self, value: f64) {
        if value == 0.0 {
            return;
        }
        let bits = value.to_bits();
        let negative = bits >> 63 == 1;
        let exponent = u32::try_from((bits >> 52) & 0x7ff).unwrap();
        let fraction = bits & ((1_u64 << 52) - 1);
        // value = mantissa · 2^(shift − 1074)
        let (mantissa, shift) = if exponent == 0 {
            (fraction, 0_u32)
        } else {
            (fraction | (1_u64 << 52), exponent - 1)
        };
        let mut term = [0_u64; LIMBS];
        let limb = usize::try_from(shift / 64).unwrap();
        let offset = shift % 64;
        term[limb] = mantissa << offset;
        if offset != 0 && limb + 1 < LIMBS {
            term[limb + 1] = mantissa >> (64 - offset);
        }
        if negative {
            Self::negate(&mut term);
        }
        let mut carry = 0_u64;
        for (held, add) in self.0.iter_mut().zip(term) {
            let (sum, first) = held.overflowing_add(add);
            let (sum, second) = sum.overflowing_add(carry);
            *held = sum;
            carry = u64::from(first) + u64::from(second);
        }
    }

    fn negate(limbs: &mut [u64; LIMBS]) {
        let mut carry = 1_u64;
        for held in limbs.iter_mut() {
            let (sum, over) = (!*held).overflowing_add(carry);
            *held = sum;
            carry = u64::from(over);
        }
    }

    /// The exact value rounded to the nearest float, ties to even.
    pub(crate) fn rounded(&self) -> f64 {
        let mut magnitude = self.0;
        let negative = magnitude[LIMBS - 1] >> 63 == 1;
        if negative {
            Self::negate(&mut magnitude);
        }
        let bit = |at: usize| (magnitude[at / 64] >> (at % 64)) & 1 == 1;
        let Some(top) = (0..LIMBS * 64).rev().find(|at| bit(*at)) else {
            return 0.0;
        };
        // Below 2^53 units the value is a subnormal or the first normal binade,
        // and every one of those is a float exactly.
        let (mantissa, exponent) = if top < 53 {
            let mut mantissa = 0_u64;
            for at in (0..=top).rev() {
                mantissa = (mantissa << 1) | u64::from(bit(at));
            }
            if top < 52 {
                return f64::from_bits(mantissa) * if negative { -1.0 } else { 1.0 };
            }
            (mantissa, 1_u64)
        } else {
            let mut mantissa = 0_u64;
            for at in (top - 52..=top).rev() {
                mantissa = (mantissa << 1) | u64::from(bit(at));
            }
            let guard = bit(top - 53);
            let sticky = (0..top - 53).any(bit);
            let mut exponent = u64::try_from(top - 52).unwrap() + 1;
            if guard && (sticky || mantissa & 1 == 1) {
                mantissa += 1;
                if mantissa == 1 << 53 {
                    mantissa >>= 1;
                    exponent += 1;
                }
            }
            (mantissa, exponent)
        };
        if exponent >= 0x7ff {
            return if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        let bits = (exponent << 52) | (mantissa & ((1 << 52) - 1));
        let value = f64::from_bits(bits);
        if negative { -value } else { value }
    }
}

pub(crate) fn oracle(values: &[f64]) -> f64 {
    let mut wide = Wide::zero();
    for value in values {
        wide.add(*value);
    }
    wide.rounded()
}
