//! Satellite types for statement kinds — Ratio, VaultKey.
//!
//! Moved out of `statement.rs`; re-exported there, so every
//! existing path is unchanged.

/// A load margin as hundredths of one (150 is 1.5), the fixed-point spelling
/// a ratio takes in the statement tree: the kind derives `Eq`, and a float
/// field would take that away from every match over statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ratio(u64);

impl Ratio {
    /// The stored hundredths, for the catalog row, the render, and every
    /// comparison: hundredths is the ratio's only spelling — a float would
    /// re-introduce the precision question the fixed-point spelling exists
    /// to close, and nothing here needs one.
    pub fn hundredths(self) -> u64 {
        self.0
    }

    /// The margin as hundredths, refused when it is not above one by at least
    /// a hundredth, or past a ceiling where a ratio is a typo rather than a
    /// policy.
    pub fn of_hundredths(hundredths: u64) -> Option<Self> {
        (101..=1_000_000_000)
            .contains(&hundredths)
            .then_some(Self(hundredths))
    }

    /// The nearest hundredths to `times`, when it is a finite margin in the
    /// range [`Self::of_hundredths`] admits.
    pub fn nearest(times: f64) -> Option<Self> {
        let hundredths = times * 100.0;
        if !hundredths.is_finite() || !(101.0..=1_000_000_000.0).contains(&hundredths) {
            return None;
        }
        // Range-checked above to a figure `u64` holds with room, and `std`
        // offers no fallible float-to-int conversion to take instead of this:
        // round-tripping the decimal spelling is the `as`-free way to land it.
        Self::of_hundredths(hundredths.round().to_string().parse::<u64>().ok()?)
    }
}

/// A 256-bit key presented for a vault in keyed custody (ADR-0136).
///
/// Built only by the vault surface from a frame field — no statement text spells one — and never printed. Cleared
/// when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct VaultKey([u8; 32]);

impl VaultKey {
    /// Hold a key that arrived as a field.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key's bytes, for the one caller that derives with them.
    #[must_use]
    pub const fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}
