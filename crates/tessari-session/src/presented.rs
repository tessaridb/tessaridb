//! The certificates this node presents, as the node reports them (ADR-0108 D6, D9).
//!
//! A fact about the process rather than the store, like the sign-in budget: the
//! store does not know which surfaces exist or what they present, and the files
//! behind them are re-read while the process runs. So the session is handed a
//! reader and asks it at the moment of the report, and a renewal shows the next
//! time anybody asks.

/// One certificate this node presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presented {
    /// Which surface presents it — `peers` or `clients`.
    pub surface: &'static str,
    /// Its SHA-256 fingerprint, in the spelling `FINGERPRINT` and
    /// `REVOKE CERTIFICATE` take: 64 lower-case hexadecimal digits.
    pub fingerprint: String,
    /// When it stops being valid, in seconds since the Unix epoch; `None` when
    /// its date could not be read, which is reported as `null` rather than
    /// guessed.
    pub expires: Option<i64>,
}

/// Reads, when asked, the certificates this node presents now.
pub trait Certificates: Send + Sync + std::fmt::Debug {
    /// Every certificate presented at this moment, one per surface.
    fn presented(&self) -> Vec<Presented>;
}
