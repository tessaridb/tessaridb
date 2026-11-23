//! Satellite types for statement kinds — BackupForm, RollupCompute, RowGuard.
//!
//! Moved out of `statement.rs`; re-exported there, so every
//! existing path is unchanged.

use super::{Expr, Name};

/// Which backup a `BACKUP` statement answers with (ADR-0091).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupForm {
    /// `BACKUP` — the store's log: every commit, in order.
    Log,
    /// `BACKUP STATE` — the store's current state at one version.
    State,
    /// `BACKUP SCRIPT` — the store's current state as TessariQL that rebuilds it.
    Script,
}

/// One `COMPUTE fold(field) AS name` of a `DEFINE ROLLUP`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupCompute {
    /// The fold word.
    pub fold: Name,
    /// The raw field it folds; `None` for `count(*)`.
    pub of: Option<Name>,
    /// The rank `approx_quantile(field, rank)` keeps (ADR-0122 C5).
    pub rank: Option<tessari_types::Number>,
    /// The name the value answers under.
    pub name: Name,
}

/// The rows a [`StatementKind::Grant`] reaches (ADR-0140).
///
/// The source text is authoritative — it is what the catalog stores — and the
/// parsed form is what the granting statement is validated with before the
/// grant is written. Carrying both saves a second parse at every use and makes
/// a stored guard answerable byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGuard {
    /// The predicate exactly as written, without the surrounding parentheses.
    pub text: String,
    /// Its parsed form over the record's fields.
    pub predicate: Expr,
}
