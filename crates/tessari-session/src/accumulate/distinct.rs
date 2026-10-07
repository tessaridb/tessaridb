//! `approx_distinct`: a HyperLogLog sketch with an exact small set
//! (ADR-0122 C2).
//!
//! # The hash is part of the format
//!
//! A value is hashed over its **index encoding** — the bytes unique-index
//! equality is decided on — so values equal in the value system (`1`, `1.0`,
//! decimal `1.00`) hash alike. The hash is the first eight bytes of SHA-256
//! over those bytes, big-endian: fixed, seedless, and the same on every node,
//! which is what lets a leader's sketch merge into another node's. Changing it
//! changes every stored rollup sketch, so it does not change within a format.
//!
//! # Exact, then estimated
//!
//! Up to [`SMALL`] distinct hashes the state is the sorted set of them and the
//! answer is its size — exact but for a 64-bit collision. Past it the state is
//! 2¹⁴ registers, each the longest run of leading zeros seen in its bucket, and
//! the answer is the improved raw estimator of Ertl (2017), which is unbiased
//! across the whole range without a table of corrections. Its relative standard
//! error is 1.04 / √16384 ≈ 0.81 %; the declared bound is three of those.
//!
//! # Why every merge order answers the same
//!
//! The registers of a set are the register-wise maximum of the registers of
//! its parts, and the small set is a union. A merge is therefore a union until
//! the union passes [`SMALL`], and the registers of the union after that — one
//! state for one set of values, however the parts arrived.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use tessari_encoding::IndexValues;
use tessari_types::{Number, Value};

/// How many distinct hashes are counted exactly.
pub(super) const SMALL: usize = 1024;

/// Bits of the hash that choose a register.
const PRECISION: u32 = 14;

/// How many registers the estimate keeps.
const REGISTERS: usize = 16_384;

/// Bits of the hash left after the register is chosen.
const REMAINDER: u32 = u64::BITS - PRECISION;

/// The largest value a register holds: every remaining bit zero.
const TOP: u8 = 51;

/// The tag of a state holding the exact small set.
const SET_TAG: u8 = 1;

/// The tag of a state holding the registers.
const REGISTERS_TAG: u8 = 2;

/// `1 / (2 ln 2)`, the estimator's constant as the register count grows.
const ALPHA_INFINITY: f64 = 0.721_347_520_444_481_7;

/// What the method is called in the answer's note.
pub(crate) const METHOD: &str = "hll-14";

/// The declared bound on the relative error, in the answer's note.
pub(crate) const BOUND: &str = "0.025";

/// One `approx_distinct` in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Distinct {
    /// Every distinct hash so far, while there are at most [`SMALL`].
    Small(BTreeSet<u64>),
    /// The registers, once there were more.
    Registers(Vec<u8>),
}

impl Default for Distinct {
    fn default() -> Self {
        Self::Small(BTreeSet::new())
    }
}

/// The hash a value is counted under.
pub(super) fn hash(value: &Value) -> u64 {
    let encoded = IndexValues::of(std::slice::from_ref(value));
    let digest = Sha256::digest(encoded.as_slice());
    let mut first = [0_u8; 8];
    first.copy_from_slice(digest.get(..8).unwrap_or(&[0; 8]));
    u64::from_be_bytes(first)
}

impl Distinct {
    /// Count one present value.
    pub(super) fn offer(&mut self, value: &Value) {
        self.add(hash(value));
    }

    fn add(&mut self, hashed: u64) {
        match self {
            Self::Small(set) => {
                set.insert(hashed);
                if set.len() > SMALL {
                    let mut registers = vec![0; REGISTERS];
                    for held in set.iter() {
                        record(&mut registers, *held);
                    }
                    *self = Self::Registers(registers);
                }
            }
            Self::Registers(registers) => record(registers, hashed),
        }
    }

    /// Fold in another sketch, as if its values had been offered here.
    pub(super) fn absorb(&mut self, other: &Self) {
        match other {
            Self::Small(set) => {
                for held in set {
                    self.add(*held);
                }
            }
            Self::Registers(more) => {
                if let Self::Small(set) = self {
                    let mut registers = vec![0; REGISTERS];
                    for held in set.iter() {
                        record(&mut registers, *held);
                    }
                    *self = Self::Registers(registers);
                }
                if let Self::Registers(registers) = self {
                    for (held, offered) in registers.iter_mut().zip(more) {
                        *held = (*held).max(*offered);
                    }
                }
            }
        }
    }

    /// The estimate, as a whole number.
    pub(super) fn answer(&self) -> Value {
        let counted = match self {
            Self::Small(set) => i64::try_from(set.len()).unwrap_or(i64::MAX),
            Self::Registers(registers) => Number::float(estimate(registers).round())
                .as_exact_integer()
                .unwrap_or(i64::MAX),
        };
        Value::Number(Number::Integer(counted))
    }

    /// The sketch as it travels and as a rollup row keeps it.
    pub(super) fn state(&self) -> Value {
        let bytes = match self {
            Self::Small(set) => {
                let mut bytes = Vec::with_capacity(set.len().saturating_mul(8).saturating_add(1));
                bytes.push(SET_TAG);
                for held in set {
                    bytes.extend_from_slice(&held.to_be_bytes());
                }
                bytes
            }
            Self::Registers(registers) => {
                let mut bytes = Vec::with_capacity(REGISTERS.saturating_add(1));
                bytes.push(REGISTERS_TAG);
                bytes.extend_from_slice(registers);
                bytes
            }
        };
        Value::Bytes(bytes)
    }

    /// A sketch read back, or `None` for bytes that are not one.
    pub(super) fn from_state(state: &Value) -> Option<Self> {
        let Value::Bytes(bytes) = state else {
            return None;
        };
        let (tag, rest) = bytes.split_first()?;
        match *tag {
            SET_TAG => {
                let (chunks, remainder) = rest.as_chunks::<8>();
                if !remainder.is_empty() || chunks.len() > SMALL {
                    return None;
                }
                let set = chunks
                    .iter()
                    .map(|chunk| u64::from_be_bytes(*chunk))
                    .collect();
                Some(Self::Small(set))
            }
            REGISTERS_TAG if rest.len() == REGISTERS && rest.iter().all(|held| *held <= TOP) => {
                Some(Self::Registers(rest.to_vec()))
            }
            _ => None,
        }
    }
}

/// Put one hash into its register.
fn record(registers: &mut [u8], hashed: u64) {
    let index = usize::try_from(hashed >> REMAINDER).unwrap_or(0);
    // The remaining bits moved to the top; their low end is now zeros, so a
    // remainder of all zeros reads as the whole word and is capped.
    let rest = hashed << PRECISION;
    let run = u8::try_from(rest.leading_zeros().saturating_add(1)).unwrap_or(TOP);
    if let Some(held) = registers.get_mut(index) {
        *held = (*held).max(run.min(TOP));
    }
}

/// Ertl's improved raw estimate from the registers' histogram.
fn estimate(registers: &[u8]) -> f64 {
    let mut histogram = [0_u64; 52];
    for held in registers {
        if let Some(slot) = histogram.get_mut(usize::from(*held)) {
            *slot = slot.saturating_add(1);
        }
    }
    let size = super::arithmetic::count(u64::try_from(registers.len()).unwrap_or(0));
    let share = |slot: usize| super::arithmetic::count(histogram.get(slot).copied().unwrap_or(0));
    let mut z = size * tau(1.0 - share(usize::from(TOP)) / size);
    for slot in (1..usize::from(TOP)).rev() {
        z = 0.5 * (z + share(slot));
    }
    z += size * sigma(share(0) / size);
    ALPHA_INFINITY * size * size / z
}

/// `σ(x) = x + Σ x^(2^k) 2^(k−1)`, summed until it stops changing.
fn sigma(mut x: f64) -> f64 {
    if x >= 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0;
    let mut z = x;
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if z == before {
            return z;
        }
    }
}

/// `τ(x)`, the correction for registers that reached the top.
fn tau(mut x: f64) -> f64 {
    if x <= 0.0 || x >= 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if z == before {
            return z / 3.0;
        }
    }
}
