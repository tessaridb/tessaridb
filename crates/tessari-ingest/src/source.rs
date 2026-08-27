//! Where a consumer's messages come from.
//!
//! The trait is deliberately two methods wide. Everything a broker client offers
//! beyond this — rebalance callbacks, metadata, seek, pause — is either the
//! client's own business or a feature this database does not expose, and a wider
//! trait would be a wider surface to keep two implementations honest across.

use std::time::Duration;

/// One message, as the runner needs it.
///
/// The partition and offset are carried even though nothing here resumes from
/// them: they are what a quarantined payload is **found** by, and what an
/// operator compares against the broker's own numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Which partition it came from.
    pub partition: i32,
    /// Where in that partition.
    pub offset: i64,
    /// The bytes, exactly as they arrived.
    pub payload: Vec<u8>,
}

/// Why a source could not do what was asked.
///
/// A string rather than an enum, because what can go wrong is the client's
/// vocabulary and not this crate's: inventing categories here would mean
/// mapping every client's failures onto a set chosen before any of them were
/// read, and the operator needs the client's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError(pub String);

impl std::fmt::Display for SourceError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}

impl std::error::Error for SourceError {}

/// Somewhere messages come from, and somewhere a position is committed.
pub trait Source: Send {
    /// The next message, waiting up to `patience` for one.
    ///
    /// `Ok(None)` means nothing arrived in that time, which is the ordinary
    /// case on a quiet topic and is not a failure.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError`] when the source itself failed.
    fn poll(&mut self, patience: Duration) -> Result<Option<Message>, SourceError>;

    /// Record that everything handed out so far has been dealt with.
    ///
    /// Called **after** the store has committed, never before. That order is the
    /// delivery guarantee, and it is the caller's to keep — a source cannot
    /// enforce it.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError`] when the commit was refused.
    fn commit(&mut self) -> Result<(), SourceError>;
}
