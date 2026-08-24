//! A query builder for TessariQL that builds the **syntax**, never the text.
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
//! compare. Without that, this crate is a second dialect of TessariQL that nothing
//! is watching — free to drift from the parser until a caller's query is
//! refused in production. `tests/round_trip.rs` holds it.
//!
//! # An incomplete query does not compile
//!
//! [`Select`] carries the clauses it has in its type. A read with no source is a
//! [`Select<NoSource>`], which has no `build`, so forgetting the `FROM` is a
//! compile error rather than a failure at run time:
//!
//! ```compile_fail,E0599
//! // No `FROM`, so no `build`. This does not compile.
//! let query = tessari_query::select().build();
//! ```
//!
//! ```
//! # fn main() -> Result<(), tessari_query::Error> {
//! // The same read with its source named builds.
//! let query = tessari_query::select().from("users").build()?;
//! assert!(query.parameters.is_empty());
//! # Ok(())
//! # }
//! ```
//!
//! The `E0599` annotation records *which* failure is meant. It does **not**
//! enforce it: rustdoc checks only that the block failed to compile, and a
//! block annotated with one code still passes when it fails with another —
//! measured, not assumed, by replacing `select` with a misspelling that raises
//! `E0425` and watching the test stay green. So `compile_fail` alone proves
//! exactly one thing, that this does not compile, and cannot say why.
//!
//! What narrows it is the **pair**: the block below differs from the one above
//! by a single call, `.from("users")`, and must compile. A change that broke the
//! example for an unrelated reason would have to break it in the failing block
//! only, which is visible in six lines of text. That is weaker than a checked
//! stderr fixture and it is what the standard toolchain can express; a
//! `trybuild` tree would check the message exactly and would bring `.stderr`
//! fixtures that churn with every compiler release, for a property this size.
//!
//! # Where the typestate stops, and why it stops there
//!
//! Typestate covers the **presence of required clauses** and deliberately
//! nothing else (`ADR-0022` §4). That boundary was set by reading the compiler's
//! own output for the failing case above, end to end:
//!
//! ```text
//! error[E0599]: no method named `build` found for struct `Select<NoSource>`
//!               in the current scope
//!   = note: the method was found for `Select<Sourced>`
//! ```
//!
//! It names the **condition** — `NoSource` is the words "no source" — and it
//! names the **state that would satisfy it**. It does not name the **action**,
//! `.from(table)`; a caller has to look for what produces a `Sourced`. So the
//! message is actionable because the state type was named carefully, and not
//! because the compiler helped, which is the honest reading and the reason the
//! generics go no further than this.
//!
//! Making the compiler say `.from(table)` is possible — move `build` onto a
//! trait and attach `#[diagnostic::on_unimplemented]` — and it is **rejected**:
//! it would force every caller to import a trait to build a query, buying a
//! better error message with a worse API. An unreadable type error is worse than
//! a runtime error, and a worse API is worse than both.

#![forbid(unsafe_code)]

mod select;

pub use select::{NoSource, Query, Select, Sourced, select};
pub use tessari_ql::BinaryOp;

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
