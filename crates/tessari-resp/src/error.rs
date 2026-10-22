//! What can go wrong reading the RESP protocol.
//!
//! Every variant is a client's mistake, never the store's: a command the store
//! refuses is answered with an error reply on a connection that stays open,
//! and only bytes that cannot be read as commands at all end it.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Bytes this node cannot read as commands, or chooses not to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A field held bytes its encoding cannot carry.
    #[error("Protocol error: {field} is malformed: {why}")]
    Malformed {
        /// The field.
        field: &'static str,
        /// What was wrong with it.
        why: &'static str,
    },

    /// A length or a count went past this node's ceiling.
    ///
    /// Refused rather than allocated: a length is a number a client chose, and
    /// a client that says four gigabytes must not get four gigabytes reserved.
    #[error("Protocol error: {field} is {found}, above the ceiling of {ceiling}")]
    TooLarge {
        /// The field.
        field: &'static str,
        /// What the client said.
        found: u64,
        /// The most this node accepts.
        ceiling: u64,
    },
}
