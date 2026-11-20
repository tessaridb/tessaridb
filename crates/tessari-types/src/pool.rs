//! The statement kinds a workload pool bounds.
//!
//! Lived in the query language's AST until the pool became a catalog row:
//! the storage crate writes and re-checks the row, and storage sits below
//! the parser, so the type lives where both can reach it.

/// The statement kinds a pool bounds, as a set rather than an enum list:
/// `FOR READS, WRITES` names two, and the absent clause reads as every kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolKinds {
    /// Reads — `SELECT` and the statement forms that only answer.
    pub reads: bool,
    /// Writes — `CREATE`/`UPDATE`/`UPSERT`/`DELETE` and relations.
    pub writes: bool,
    /// Definitions and operations — the catalog statements and the
    /// membership-shaped ones.
    pub definitions: bool,
}

impl PoolKinds {
    /// Every kind, the spelling an absent `FOR` clause reads as.
    pub const ALL: Self = Self {
        reads: true,
        writes: true,
        definitions: true,
    };
}
