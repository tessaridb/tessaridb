//! What can go wrong between a script and the store.
//!
//! Every variant that can name a place does. A script is written by hand, and a
//! failure that cannot point at the words that caused it makes its author read
//! the whole thing again.

use bgv_db_ql::Span;

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure running a script.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The script could not be read.
    #[error(transparent)]
    Script(#[from] bgv_db_ql::Error),

    /// The store refused the work.
    #[error(transparent)]
    Store(#[from] bgv_db_storage::Error),

    /// A stored value could not be read back.
    #[error(transparent)]
    Encoding(#[from] bgv_db_encoding::Error),

    /// A statement needs a namespace and the session has not selected one.
    #[error("no namespace selected (at {span}) — say `USE NAMESPACE …` first")]
    NoNamespaceSelected {
        /// Where the statement is.
        span: Span,
    },

    /// A statement needs a database and the session has not selected one.
    ///
    /// An error rather than a default, because guessing which database a write
    /// belongs to is the one mistake reading the result cannot undo.
    #[error("no database selected (at {span}) — say `USE DATABASE …` first")]
    NoDatabaseSelected {
        /// Where the statement is.
        span: Span,
    },

    /// A name the catalog does not hold.
    #[error("no {entity} named {name:?} (at {span})")]
    Unknown {
        /// What kind of thing was looked for.
        entity: &'static str,
        /// The name as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// `BEGIN` inside a transaction that is already open.
    #[error("a transaction is already open (at {span})")]
    NestedTransaction {
        /// Where the second `BEGIN` is.
        span: Span,
    },

    /// `COMMIT` or `CANCEL` with nothing open.
    #[error("no transaction is open (at {span})")]
    NoOpenTransaction {
        /// Where the statement is.
        span: Span,
    },

    /// A script that opened a transaction and never closed it.
    ///
    /// The work is discarded and this is raised, rather than committed: a script
    /// that does not say `COMMIT` has not said its work is finished, and
    /// discarding it silently would hide that it ran at all.
    #[error("the script ended with a transaction still open (at {span}); its work was discarded")]
    UnclosedTransaction {
        /// Where the `BEGIN` was.
        span: Span,
    },

    /// `CREATE` over a record that is already there.
    ///
    /// Refused rather than replaced: a silent overwrite loses a record with
    /// nothing anywhere to notice, and `UPDATE` and `SET` both say replacement
    /// out loud.
    #[error("record {id} already exists (at {span}) — say `UPDATE` to replace it")]
    RecordExists {
        /// The identity as written.
        id: String,
        /// Where it was written.
        span: Span,
    },

    /// `UPDATE` over a record that is not there.
    #[error("no record {id} (at {span}) — say `CREATE` to write a new one")]
    NoSuchRecord {
        /// The identity as written.
        id: String,
        /// Where it was written.
        span: Span,
    },

    /// A table used as an edge table that was not declared as one.
    ///
    /// Refused rather than accommodated: an edge table carries an index on each
    /// endpoint, and without them a relation would be written that traversal
    /// could not find. A write nothing can read back is worse than a refusal.
    #[error("{table} is not an edge table — define it with `EDGE` (at {span})")]
    NotAnEdgeTable {
        /// The table as written.
        table: String,
        /// Where it was written.
        span: Span,
    },

    /// An edge was given properties that are not a set of named fields.
    ///
    /// An edge record already carries `out` and `in`; anything else it holds has
    /// to be named, so there is nowhere for a bare value to go. Refused where it
    /// is written rather than dropped on the way to the store.
    #[error("an edge's properties must be an object, not {found} (at {span})")]
    EdgePropertiesNotAnObject {
        /// The type that was given instead.
        found: &'static str,
        /// Where it was written.
        span: Span,
    },

    /// A range bound that no record identity can be.
    #[error("a key range is bounded by record identities (at {span})")]
    InvalidKeyBound {
        /// Where the bound was written.
        span: Span,
    },
}
