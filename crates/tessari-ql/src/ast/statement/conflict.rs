//! Satellite types for statement kinds — OnConflict, ConflictAction, ConflictUpdate.
//!
//! Moved out of `statement.rs`; re-exported there, so every
//! existing path is unchanged.

use super::{Edit, Expr, Name};

/// `ON CONFLICT [(name, …)] DO …` — the arbiter clause an insert may carry.
///
/// The target names the columns the arbiter index must hold exactly, in any
/// order — the inference rule the dialect this statement speaks on the wire
/// uses. An empty target names every unique index, which only `DO NOTHING`
/// may say: an update that merges into "whatever conflicts" would have no
/// shape a caller could reason about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnConflict {
    /// The conflict target: column names, empty for `DO NOTHING` alone.
    pub target: Vec<Name>,
    /// What a collision on the arbiter does instead of refusing the row.
    pub action: ConflictAction,
}

/// The alternative a collision on the arbiter takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    /// `DO NOTHING` — the row is skipped, its siblings land.
    Nothing,
    /// `DO UPDATE SET … [WHERE …]` — the proposed row merges into the record
    /// the arbiter index already holds. Boxed so the action enum stays the
    /// size of its cheapest arm.
    Update(Box<ConflictUpdate>),
}

/// What a `DO UPDATE` action carries: the SET assignments and the optional
/// per-row condition, one payload rather than two loose fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictUpdate {
    /// The SET assignments — the same edit shape an `UPDATE` carries, so the
    /// two cannot drift.
    pub edit: Edit,
    /// An optional condition, read against the existing record with the
    /// proposed row beside it as `excluded`; a false condition skips the row
    /// rather than refusing the statement.
    pub condition: Option<Expr>,
}
