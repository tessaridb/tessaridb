//! What can go wrong while building a query.

/// A query that could not be built from what was supplied.
///
/// Both variants are mistakes in the calling code rather than conditions of the
/// data, which is why they name the text they were given.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A route into a record that is not a route.
    #[error("{text:?} is not a route into a record")]
    NotARoute {
        /// The text as supplied.
        text: String,
    },

    /// A name that is not a bare identifier.
    ///
    /// Refused rather than quoted: see the crate documentation for why a name
    /// is grammar and a value is not.
    #[error("{text:?} is not a name: a table or field is letters, digits and underscores")]
    NotAName {
        /// The text as supplied.
        text: String,
    },
}

/// Result alias for building a query.
pub type Result<T> = core::result::Result<T, Error>;
