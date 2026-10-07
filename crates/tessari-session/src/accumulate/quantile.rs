//! `approx_quantile`: a sketch with a relative error guarantee (ADR-0122 C3).
//!
//! # The guarantee
//!
//! A positive value `x` is counted in bucket `⌈log_γ x⌉`, γ = (1 + α) / (1 − α)
//! with α = 1 %, and a bucket answers `2γᵏ / (γ + 1)`. Every value in the bucket
//! lies within α of that answer, relatively — a deterministic bound, not a
//! probability — so the value at a rank is answered within α of the true one.
//! A negative value is counted the same way in a mirrored store, and a value
//! smaller in magnitude than [`ZERO_BELOW`] in a bucket of its own that answers
//! zero exactly.
//!
//! # A bounded state, and why collapsing it keeps every merge order equal
//!
//! Each sign keeps at most [`MOST`] buckets. Past that, the lowest buckets fold
//! into the lowest one kept, and the answer says so (`collapsed`): ranks that
//! fall there are no longer within α. It takes data spanning γ²⁰⁴⁸ ≈ 10¹⁷ in
//! magnitude to get there.
//!
//! Which buckets are kept depends only on the **union** of the indices seen —
//! the highest [`MOST`] of them — and the counts below fold into the lowest kept
//! one whichever order they arrived in. So offering values one at a time,
//! merging two leaders' sketches, or merging three in any order all reach one
//! state, which is what lets a gathered read equal a whole node's.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

/// The relative accuracy, α.
const ALPHA: f64 = 0.01;

/// Below this magnitude a value is counted as zero.
const ZERO_BELOW: f64 = 1e-9;

/// The most buckets one sign keeps.
pub(super) const MOST: usize = 2048;

/// The tag of a quantile sketch's state.
const TAG: u8 = 3;

/// What the method is called in the answer's note.
pub(crate) const METHOD: &str = "ddsketch";

/// The declared relative accuracy, in the answer's note.
pub(crate) const BOUND: &str = "0.01";

/// γ, the ratio between one bucket's bound and the next.
fn gamma() -> f64 {
    (1.0 + ALPHA) / (1.0 - ALPHA)
}

/// One `approx_quantile` sketch in progress.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Quantile {
    /// Counts of positive values by bucket.
    positive: BTreeMap<i64, u64>,
    /// Counts of negative values by the bucket of their magnitude.
    negative: BTreeMap<i64, u64>,
    /// How many values were counted as zero.
    zero: u64,
    /// Whether any bucket was folded into another to keep the bound.
    collapsed: bool,
}

/// The bucket a magnitude falls in; `None` for one no bucket holds.
fn bucket(magnitude: f64) -> Option<i64> {
    Number::float((magnitude.ln() / gamma().ln()).ceil()).as_exact_integer()
}

/// What a bucket answers.
fn answer_of(index: i64) -> f64 {
    let index = Number::Integer(index).as_float().unwrap_or(0.0);
    let gamma = gamma();
    2.0 * (index * gamma.ln()).exp() / (gamma + 1.0)
}

/// Fold the lowest buckets into the lowest one kept until at most [`MOST`]
/// remain; whether any was folded.
fn collapse(store: &mut BTreeMap<i64, u64>) -> bool {
    let mut folded = false;
    while store.len() > MOST {
        let Some((_, lowest)) = store.pop_first() else {
            break;
        };
        if let Some(mut next) = store.first_entry() {
            *next.get_mut() = next.get().saturating_add(lowest);
        }
        folded = true;
    }
    folded
}

impl Quantile {
    /// Count one finite number.
    pub(super) fn offer(&mut self, value: f64) {
        if value.abs() < ZERO_BELOW {
            self.zero = self.zero.saturating_add(1);
            return;
        }
        let Some(index) = bucket(value.abs()) else {
            return;
        };
        let store = if value > 0.0 {
            &mut self.positive
        } else {
            &mut self.negative
        };
        let count = store.entry(index).or_insert(0);
        *count = count.saturating_add(1);
        if collapse(store) {
            self.collapsed = true;
        }
    }

    /// Fold in another sketch, as if its values had been offered here.
    pub(super) fn absorb(&mut self, other: &Self) {
        for (mine, theirs) in [
            (&mut self.positive, &other.positive),
            (&mut self.negative, &other.negative),
        ] {
            for (index, many) in theirs {
                let count = mine.entry(*index).or_insert(0);
                *count = count.saturating_add(*many);
            }
        }
        self.zero = self.zero.saturating_add(other.zero);
        let positive = collapse(&mut self.positive);
        let negative = collapse(&mut self.negative);
        self.collapsed = self.collapsed || other.collapsed || positive || negative;
    }

    /// Whether a bucket was folded to keep the bound.
    pub(crate) const fn collapsed(&self) -> bool {
        self.collapsed
    }

    /// How many values were counted.
    fn total(&self) -> u64 {
        self.positive
            .values()
            .chain(self.negative.values())
            .fold(self.zero, |sum, many| sum.saturating_add(*many))
    }

    /// The value at `rank`, a number from 0 to 1; `NONE` over nothing.
    pub(super) fn answer(&self, rank: f64) -> Value {
        let total = self.total();
        if total == 0 {
            return Value::None;
        }
        // The value at index ⌊rank × (n − 1)⌋ of the sorted values is the one in
        // the first bucket whose running count passes rank × (n − 1).
        let wanted = rank * super::arithmetic::count(total.saturating_sub(1));
        let mut seen = 0_u64;
        let mut passes = |many: u64| {
            seen = seen.saturating_add(many);
            super::arithmetic::count(seen) > wanted
        };
        for (index, many) in self.negative.iter().rev() {
            if passes(*many) {
                return Value::Number(Number::float(-answer_of(*index)));
            }
        }
        if passes(self.zero) {
            return Value::Number(Number::float(0.0));
        }
        let mut highest = 0.0;
        for (index, many) in &self.positive {
            highest = answer_of(*index);
            if passes(*many) {
                return Value::Number(Number::float(highest));
            }
        }
        Value::Number(Number::float(highest))
    }

    /// The sketch as it travels and as a rollup row keeps it: the tag, whether
    /// it collapsed, the zero count, then each store as a count of buckets and
    /// each bucket's index and count, big-endian.
    pub(super) fn state(&self) -> Value {
        let mut bytes = vec![TAG, u8::from(self.collapsed)];
        bytes.extend_from_slice(&self.zero.to_be_bytes());
        for store in [&self.positive, &self.negative] {
            let size = u32::try_from(store.len()).unwrap_or(u32::MAX);
            bytes.extend_from_slice(&size.to_be_bytes());
            for (index, many) in store {
                bytes.extend_from_slice(&index.to_be_bytes());
                bytes.extend_from_slice(&many.to_be_bytes());
            }
        }
        Value::Bytes(bytes)
    }

    /// A sketch read back, or `None` for bytes that are not one.
    pub(super) fn from_state(state: &Value) -> Option<Self> {
        let Value::Bytes(bytes) = state else {
            return None;
        };
        let mut reader = Reader(bytes);
        if reader.take::<1>()? != [TAG] {
            return None;
        }
        let collapsed = match reader.take::<1>()? {
            [0] => false,
            [1] => true,
            _ => return None,
        };
        let zero = u64::from_be_bytes(reader.take()?);
        let mut stores = [BTreeMap::new(), BTreeMap::new()];
        for store in &mut stores {
            let size = usize::try_from(u32::from_be_bytes(reader.take()?)).ok()?;
            if size > MOST {
                return None;
            }
            for _ in 0..size {
                let index = i64::from_be_bytes(reader.take()?);
                let many = u64::from_be_bytes(reader.take()?);
                // Ascending and never empty, as a sketch writes them.
                if many == 0
                    || store
                        .last_key_value()
                        .is_some_and(|(last, _)| *last >= index)
                {
                    return None;
                }
                store.insert(index, many);
            }
        }
        if !reader.0.is_empty() {
            return None;
        }
        let [positive, negative] = stores;
        Some(Self {
            positive,
            negative,
            zero,
            collapsed,
        })
    }
}

/// Bytes read from the front.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
}
