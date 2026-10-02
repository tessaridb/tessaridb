//! A vector held as one byte per component, and the vector a graph node keeps.
//!
//! # Scalar quantization, per vector
//!
//! Each component is stored as a code in `0..=255` over the vector's own range:
//! `x ≈ low + code × step`, with `low` the smallest component and `step` a
//! two-hundred-and-fifty-fifth of the span. Eight bytes per component become one,
//! plus sixteen bytes for the range.
//!
//! **Per vector, not per index.** A range shared by the whole index would have
//! to be learned from the vectors and re-learned as they change — a training
//! step, and a graph that two replicas could build differently if they learned
//! at different moments. A vector's own range is a pure function of the vector,
//! so every replica writes the same codes, and no component ever falls outside
//! the range it is coded against.
//!
//! The codes are lossy, and the loss is repaid where it matters: a quantized
//! index chooses which records are tried and the records' own full-precision
//! vectors decide their order (the session rescores).

/// A vector's components as one byte each, with the range they were coded over.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantizedVector {
    /// The smallest component.
    pub low: f64,
    /// The value one code step stands for; `0` when every component is equal.
    pub step: f64,
    /// One code per component.
    pub codes: Vec<u8>,
}

impl QuantizedVector {
    /// The codes for this vector, or `None` for one that cannot be coded — empty,
    /// or holding a component that is not finite.
    #[must_use]
    pub fn of(vector: &[f64]) -> Option<Self> {
        if vector.is_empty() || vector.iter().any(|component| !component.is_finite()) {
            return None;
        }
        let low = vector.iter().copied().fold(f64::INFINITY, f64::min);
        let high = vector.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let step = (high - low) / f64::from(u8::MAX);
        if !step.is_finite() {
            // A span past the largest float: the range itself cannot be held.
            return None;
        }
        let codes = vector
            .iter()
            .map(|component| {
                if step == 0.0 {
                    0
                } else {
                    code_of((component - low) / step)
                }
            })
            .collect();
        Some(Self { low, step, codes })
    }

    /// The value a code stands for.
    #[must_use]
    pub fn component(&self, code: u8) -> f64 {
        self.low + self.step * f64::from(code)
    }

    /// The vector the codes stand for.
    #[must_use]
    pub fn to_vec(&self) -> Vec<f64> {
        self.codes
            .iter()
            .map(|code| self.component(*code))
            .collect()
    }
}

/// The nearest code to a position on the `0..=255` scale.
///
/// **Invariant the conversion rests on:** the position is
/// `(component − low) / step` for a component inside `[low, high]` and
/// `step = (high − low) / 255`, so it is finite and inside `[0, 255]` before
/// rounding; the clamp makes that true for the last ulp as well, and a float to
/// integer `as` saturates rather than wrapping in any case.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions,
    reason = "clamped to 0..=255 first; see the invariant above"
)]
fn code_of(position: f64) -> u8 {
    position.round().clamp(0.0, f64::from(u8::MAX)) as u8
}

/// The vector a graph node keeps: its components, or their codes.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredVector {
    /// Every component at full precision.
    Full(Vec<f64>),
    /// One byte per component (a `QUANTIZED` index).
    Quantized(QuantizedVector),
}

impl StoredVector {
    /// How many components it has.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Full(vector) => vector.len(),
            Self::Quantized(coded) => coded.codes.len(),
        }
    }

    /// Whether it has no components.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many bytes this vector takes in a stored node: the width, then eight
    /// bytes a component, or sixteen bytes of range and one byte a component.
    ///
    /// The encoder's own layout, stated once beside it (`VectorNode` in
    /// `vectors.rs`) and held to it by a round-trip test.
    #[must_use]
    pub fn stored_bytes(&self) -> usize {
        match self {
            Self::Full(vector) => vector.len().saturating_mul(8).saturating_add(4),
            Self::Quantized(coded) => coded.codes.len().saturating_add(20),
        }
    }

    /// The components, decoded where they are codes.
    #[must_use]
    pub fn to_vec(&self) -> Vec<f64> {
        match self {
            Self::Full(vector) => vector.clone(),
            Self::Quantized(coded) => coded.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::QuantizedVector;

    #[test]
    fn every_component_comes_back_within_half_a_step() {
        let vector = [0.25, -1.5, 3.0, 0.0, 2.999, -1.4999];
        let coded = QuantizedVector::of(&vector).expect("a finite vector codes");
        assert_eq!(coded.codes.len(), vector.len());
        let back = coded.to_vec();
        for (original, decoded) in vector.iter().zip(&back) {
            assert!(
                (original - decoded).abs() <= coded.step / 2.0 + 1e-12,
                "{original} came back as {decoded} with step {}",
                coded.step
            );
        }
    }

    #[test]
    fn the_ends_of_the_range_are_exact() {
        let coded = QuantizedVector::of(&[-2.0, 0.5, 6.0]).expect("codes");
        assert_eq!(coded.codes, vec![0, 80, 255]);
        assert_eq!(coded.component(0), -2.0);
        assert_eq!(coded.component(255), 6.0);
    }

    #[test]
    fn a_constant_vector_codes_with_no_step() {
        let coded = QuantizedVector::of(&[0.7, 0.7, 0.7]).expect("codes");
        assert_eq!(coded.step, 0.0);
        assert_eq!(coded.to_vec(), vec![0.7, 0.7, 0.7]);
    }

    #[test]
    fn a_vector_that_cannot_be_coded_is_refused() {
        assert!(QuantizedVector::of(&[]).is_none());
        assert!(QuantizedVector::of(&[1.0, f64::NAN]).is_none());
        assert!(QuantizedVector::of(&[f64::INFINITY, 1.0]).is_none());
    }

    #[test]
    fn the_same_vector_always_codes_the_same_way() {
        let vector: Vec<f64> = (0..64).map(|n| f64::from(n) * 0.37 - 5.0).collect();
        assert_eq!(QuantizedVector::of(&vector), QuantizedVector::of(&vector));
    }
}
