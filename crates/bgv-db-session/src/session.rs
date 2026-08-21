//! The session: what a script is run against, and what it remembers between
//! statements.
//!
//! A session remembers two things — the namespace and database `USE` selected —
//! and it remembers them **by name, not by id**. Resolving a name to an id once
//! and keeping it would leave a session pointing at a table that has since been
//! dropped and re-created under the same name, reading the wrong one with no
//! error anywhere. The lookup is one catalog read per statement, and the store
//! is the only thing entitled to say what a name currently means.

use bgv_db_ql::{Statement, StatementKind, parse};
use bgv_db_storage::{Store, Transaction};

use crate::error::{Error, Result};
use crate::outcome::Outcome;

/// A connection's worth of state: where statements run, and against what.
#[derive(Debug)]
pub struct Session<'a> {
    store: &'a Store,
    namespace: Option<String>,
    database: Option<String>,
}

impl<'a> Session<'a> {
    /// Open a session on a store, with nothing selected.
    #[must_use]
    pub const fn new(store: &'a Store) -> Self {
        Self {
            store,
            namespace: None,
            database: None,
        }
    }

    /// The namespace `USE` selected, if any.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// The database `USE` selected, if any.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Read a script and run it, returning one outcome per statement.
    ///
    /// A statement outside `BEGIN` is its own transaction. Inside one, every
    /// statement joins it, so a script may define a table and write to it and
    /// have both land or neither.
    ///
    /// # Errors
    ///
    /// Returns the first failure. Work buffered in an uncommitted transaction is
    /// discarded — nothing reaches the store until `commit`.
    pub fn run(&mut self, source: &str) -> Result<Vec<Outcome>> {
        let store = self.store;
        let script = parse(source)?;
        let mut outcomes = Vec::with_capacity(script.statements.len());
        let mut open: Option<(Transaction<'a>, bgv_db_ql::Span)> = None;

        for statement in &script.statements {
            let outcome = self.step(store, &mut open, statement)?;
            outcomes.push(outcome);
        }

        if let Some((transaction, span)) = open {
            transaction.rollback();
            return Err(Error::UnclosedTransaction { span });
        }
        Ok(outcomes)
    }

    /// One statement, inside the open transaction or in one of its own.
    fn step(
        &mut self,
        store: &'a Store,
        open: &mut Option<(Transaction<'a>, bgv_db_ql::Span)>,
        statement: &Statement,
    ) -> Result<Outcome> {
        let span = statement.span;
        match &statement.kind {
            StatementKind::Use {
                namespace,
                database,
            } => {
                // Recorded, not resolved: the namespace this names may be
                // defined by a later statement of the same transaction.
                if let Some(name) = namespace {
                    self.namespace = Some(name.text.clone());
                }
                if let Some(name) = database {
                    self.database = Some(name.text.clone());
                }
                Ok(Outcome::Done)
            }
            StatementKind::Begin => {
                if open.is_some() {
                    return Err(Error::NestedTransaction { span });
                }
                *open = Some((store.begin()?, span));
                Ok(Outcome::Done)
            }
            StatementKind::Commit => {
                let Some((transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                transaction.commit()?;
                Ok(Outcome::Done)
            }
            StatementKind::Cancel => {
                let Some((transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                transaction.rollback();
                Ok(Outcome::Done)
            }
            other => match open.as_mut() {
                Some((transaction, _)) => self.execute(transaction, other, span),
                None => {
                    let mut transaction = store.begin()?;
                    let outcome = self.execute(&mut transaction, other, span)?;
                    transaction.commit()?;
                    Ok(outcome)
                }
            },
        }
    }
}
