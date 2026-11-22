//! Row guards — which rows a grant reaches (ADR-0140).
//!
//! A grant may name its rows with a `WHERE` predicate; a table switches the
//! guards on with `ALTER TABLE … ENABLE ROW SECURITY`. This module resolves
//! the guards governing one table for one session and filters records
//! through them.
//!
//! # Why the filter sits beside the redactor and not in the plan
//!
//! A guard is a permission, not an optimisation: like the field redaction it
//! composes with, it must hold for every path that answers a record — scans,
//! index serves, identity seeks, joins, traversals. The planner already
//! re-tests index candidates against the whole condition for the same reason;
//! the guard rides the same per-record moment, after decode, before anything
//! looks at the value.
//!
//! # Why evaluation failure refuses the statement (D2)
//!
//! A stored predicate that errors — a field renamed after the grant, a type
//! the comparison refuses — must never decide on its own what is visible.
//! Failing closed means the statement stops with [`Error::RowGuardInvalid`];
//! widening and narrowing by accident are both refused, and the operator
//! sees the guard that broke.

use std::sync::Arc;

use tessari_ql::Expr;
use tessari_storage::{Catalog, Transaction, Verb};
use tessari_types::{RecordId, TableId, Value};

use super::redact::Visible;
use crate::error::{Error, Result};
use crate::session::Session;

/// The row guards governing one table for this session.
///
/// `None` is no filtering: the table's switch is off, the session is
/// anonymous, or no read grant on the table carries a guard. Several guards
/// AND together — a grant narrows, and two narrowings compose by
/// intersection (ADR-0140 D3).
pub(crate) type Rows = Option<Arc<RowGuards>>;

/// The compiled guards for one table, with what a refusal needs to say.
pub(crate) struct RowGuards {
    /// The table's name, for the refusal.
    table: String,
    /// Each guard as `(source text, compiled predicate)`, ANDed.
    guards: Vec<(String, Arc<Expr>)>,
}

impl RowGuards {
    /// Whether `record` is one of the rows the guards reach.
    ///
    /// The record is the redacted one — a field the session may not read is
    /// absent to the guard exactly as it is to the answer, so a guard cannot
    /// be read one bit at a time through a field grant it does not hold.
    async fn admits(
        &self,
        session: &Session<'_>,
        transaction: &mut Transaction<'_>,
        (id, record): (&RecordId, Value),
        visible: &Visible,
    ) -> Result<bool> {
        for (text, predicate) in &self.guards {
            match session
                .holds_for(transaction, predicate, (id, record.clone()), visible)
                .await
            {
                Ok(true) => {}
                Ok(false) => return Ok(false),
                Err(_) => {
                    return Err(Error::RowGuardInvalid {
                        table: self.table.clone(),
                        guard: text.clone(),
                        span: predicate.span,
                    });
                }
            }
        }
        Ok(true)
    }
}

impl Session<'_> {
    /// The row guards governing `table` for this session, over a transaction.
    ///
    /// Resolved once per read like [`Session::visible_in`]: what a grant names
    /// is a property of the catalog, and the catalog does not change under a
    /// read. The compiled predicates are cached per session keyed by their
    /// source text — the same text parses to the same tree, so the cache is
    /// an immutable mapping and needs no invalidation; a changed grant is a
    /// different text and a different entry.
    pub(crate) async fn rows_in(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
    ) -> Result<Rows> {
        self.guards_for(transaction, table, Verb::Read).await
    }

    /// The guards of the grants naming `verb` — reads filter their answers
    /// through the `read` guards; writes stage through the `write` ones
    /// (ADR-0140 D1), so a grant's predicate is one rule read and written
    /// alike.
    pub(crate) async fn write_guards_in(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
    ) -> Result<Rows> {
        self.guards_for(transaction, table, Verb::Write).await
    }

    async fn guards_for(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        verb: Verb,
    ) -> Result<Rows> {
        // The fast path is the table itself: a table the switch was never
        // turned on for never consults a grant, which is what keeps an
        // unguarded table's read path byte-for-byte the one it always had.
        let Some(definition) = Catalog::new(transaction).table(table).await? else {
            return Ok(None);
        };
        if !definition.row_security {
            return Ok(None);
        }
        let Some(user) = self.identity.user() else {
            return Ok(None);
        };
        let grants = Catalog::new(transaction).grants_for(user.id).await?;
        let mut guards = Vec::new();
        for grant in grants
            .iter()
            .filter(|grant| grant.table == table && grant.verbs.contains(&verb))
        {
            if let Some(text) = &grant.guard {
                guards.push((text.clone(), self.compile_guard(text)?));
            }
        }
        if guards.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(RowGuards {
            table: definition.name,
            guards,
        })))
    }

    /// Parse one guard's source text, from the session's cache when seen.
    ///
    /// The map is a `DashMap`; the guard from `get` is cloned out and dropped
    /// before parsing or inserting (never a guard across work, never a call
    /// into the map while holding one). Two sessions parsing the same text
    /// concurrently may both parse — the value is immutable, so the race is
    /// benign and the last insert wins by identical bytes.
    fn compile_guard(&self, text: &str) -> Result<Arc<Expr>> {
        if let Some(held) = self.guard_cache.get(text) {
            return Ok(Arc::clone(held.value()));
        }
        let parsed = Arc::new(tessari_ql::parse_condition(text)?);
        self.guard_cache
            .insert(text.to_owned(), Arc::clone(&parsed));
        Ok(parsed)
    }

    /// Records as this session may see them: decoded, redacted, and filtered
    /// through the table's row guards when it has any.
    ///
    /// The twin of [`Session::records_of`] for answer-producing paths; a
    /// caller with no guards (`Rows` is `None`) pays the decode alone.
    ///
    /// # Errors
    ///
    /// A decoding failure, or [`Error::RowGuardInvalid`] when a stored guard
    /// refuses to evaluate (D2 — the statement is refused, never silently
    /// widened or narrowed).
    pub(crate) async fn permitted_records_of(
        &self,
        transaction: &mut Transaction<'_>,
        found: Vec<(RecordId, Vec<u8>)>,
        visible: &Visible,
        rows: &Rows,
    ) -> Result<Vec<(RecordId, Value)>> {
        let records = self.records_of(found, visible)?;
        let Some(guards) = rows else {
            return Ok(records);
        };
        let mut kept = Vec::with_capacity(records.len());
        for (id, record) in records {
            if guards
                .admits(self, transaction, (&id, record.clone()), visible)
                .await?
            {
                kept.push((id, record));
            }
        }
        Ok(kept)
    }

    /// Whether one already-decoded record passes the table's row guards.
    ///
    /// For paths that decode outside [`Session::records_of`] — an identity
    /// seek answers the record or nothing, and a guarded-out record is
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`Error::RowGuardInvalid`] when a stored guard refuses to evaluate.
    pub(crate) async fn permitted(
        &self,
        transaction: &mut Transaction<'_>,
        id: &RecordId,
        record: Value,
        visible: &Visible,
        rows: &Rows,
    ) -> Result<Option<Value>> {
        let Some(guards) = rows else {
            return Ok(Some(record));
        };
        if guards
            .admits(self, transaction, (id, record.clone()), visible)
            .await?
        {
            return Ok(Some(record));
        }
        Ok(None)
    }

    /// Refuse when `record` is not one of the rows the write grant reaches
    /// (ADR-0140 D1).
    ///
    /// Used for the located record of an update or delete — a caller may not
    /// touch a row their guard does not reach — and for the record a write
    /// produces — an update may not move a record out of them. The record is
    /// the stored shape, before redaction: a guard is written against what
    /// the table holds, not against what one session may see.
    ///
    /// # Errors
    ///
    /// [`Error::RowGuardDenied`] when a guard excludes the record, and
    /// [`Error::RowGuardInvalid`] when a stored guard refuses to evaluate.
    pub(crate) async fn refuse_write_outside_guard(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        id: &RecordId,
        record: &Value,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let rows = self.write_guards_in(transaction, table).await?;
        let Some(guards) = rows else {
            return Ok(());
        };
        let admitted = guards
            .admits(
                self,
                transaction,
                (id, record.clone()),
                &None, // the stored record, not the redacted one
            )
            .await;
        match admitted {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error::RowGuardDenied {
                table: guards.table.clone(),
                span,
            }),
            // A stored guard that refuses to evaluate stops the write too
            // (D2), naming the guard it was.
            Err(error) => Err(error),
        }
    }
}
