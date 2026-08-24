//! A query builder for bgvQL that builds the **syntax**, never the text.
//!
//! # Why this is not string building
//!
//! Everything a caller supplies is a value, and every value becomes a
//! parameter — a name in the tree and an entry in the map beside it. There is no
//! point in this crate at which a caller's value is turned into query text, so
//! there is nothing to escape and no quoting rule to keep in step with the
//! lexer's. A value containing `'; DROP TABLE users; --` is a perfectly ordinary
//! string that reaches the store as a string.
//!
//! **Names are the other half, and they are checked rather than escaped.** A
//! table or field name *is* rendered as text, because it is grammar and not
//! data. So a name that is not a bare identifier is refused when it is supplied,
//! rather than quoted into something that parses. Refusing is the honest answer:
//! a name outside the identifier grammar is a mistake in the caller's code, and
//! a builder that silently made it work would be inventing a quoting convention
//! the language does not have.
//!
//! # Why the round trip is the test that matters
//!
//! A builder is only worth having if it is more correct than a string, and that
//! claim is checkable exactly once: build a query, render it, parse it back, and
//! compare. Without that, this crate is a second dialect of bgvQL that nothing
//! is watching — free to drift from the parser until a caller's query is
//! refused in production. `tests/round_trip.rs` holds it.
//!
//! # An incomplete query does not compile
//!
//! [`Select`] carries the clauses it has in its type. A read with no source is a
//! [`Select<NoSource>`], which has no `build`, so forgetting the `FROM` is a
//! compile error rather than a failure at run time. The typestate covers the
//! **presence of required clauses** and deliberately nothing else — see
//! `ADR-0022`.

#![forbid(unsafe_code)]

mod select;

pub use bgv_db_ql::BinaryOp;
pub use select::{NoSource, Query, Select, Sourced, select};

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
