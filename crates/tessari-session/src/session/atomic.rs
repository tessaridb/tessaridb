//! Several scripts in one transaction, held by the caller (ADR-0087).
//!
//! A script closes every transaction it opens — [`Session::run_with`] rolls one
//! left open back and says so — so nothing outside a script could read, decide,
//! and then write in the same transaction. A topic consumer has to: it reads a
//! group's messages, shapes them in Rust, and writes the records beside the
//! acknowledgement, and the guarantee is that all of it commits together.

use tessari_ql::{Parameters, Span, StatementKind, parse};
use tessari_storage::Transaction;

use super::{Session, settle};
use crate::effect::{Effect, admits};
use crate::error::{Error, Result};
use crate::outcome::Outcome;

/// The transaction [`Session::atomically`] holds, for running scripts in it.
pub struct Atomic<'s, 'a> {
    session: &'s mut Session<'a>,
    open: Option<(Transaction<'a>, Span)>,
}

impl Atomic<'_, '_> {
    /// Run a script inside the held transaction, its values bound from
    /// `parameters` exactly as [`Session::run_with`] binds them.
    ///
    /// # Errors
    ///
    /// [`Error::TransactionVerbInAtomic`] when the script would open, commit,
    /// cancel or rehearse a transaction itself — the caller commits this one —
    /// and otherwise as [`Session::run_with`]. A failure leaves the held
    /// transaction to be rolled back when [`Session::atomically`] returns.
    pub fn run_with(&mut self, source: &str, parameters: &Parameters) -> Result<Vec<Outcome>> {
        let store = self.session.store;
        let mut script = parse(source)?.bind(parameters)?;
        if let Some(verb) = script.statements.iter().find(|statement| {
            matches!(
                statement.kind,
                StatementKind::Begin
                    | StatementKind::Commit
                    | StatementKind::Cancel
                    | StatementKind::Verify
            )
        }) {
            return Err(Error::TransactionVerbInAtomic { span: verb.span });
        }
        if matches!(Effect::of_script(&script), Effect::Write) {
            admits(store.node_identity()?.roles, &script)?;
        }
        self.session
            .run_statements(store, &mut self.open, &mut script)
    }
}

impl Atomic<'_, '_> {
    /// Run a script that is already parsed, bound and vetted inside the held
    /// transaction — a restore, whose statements were read against the store
    /// before any of them runs.
    pub(crate) fn run_parsed(&mut self, mut script: tessari_ql::Script) -> Result<Vec<Outcome>> {
        let store = self.session.store;
        if matches!(Effect::of_script(&script), Effect::Write) {
            admits(store.node_identity()?.roles, &script)?;
        }
        self.session
            .run_statements(store, &mut self.open, &mut script)
    }
}

impl<'a> Session<'a> {
    /// Run `work` against one transaction and commit it when `work` answers
    /// `Ok`, through the commit `COMMIT` uses — so a refusal or contention reads
    /// exactly as it would from a script. On `Err`, nothing is written.
    ///
    /// The error is the caller's type, so `work` can stop with a reason of its
    /// own — a message it will not apply — and still have nothing written.
    ///
    /// # Errors
    ///
    /// Whatever `work` answered, or the commit's refusal.
    pub fn atomically<T, E: From<Error>>(
        &mut self,
        work: impl FnOnce(&mut Atomic<'_, 'a>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let transaction = self.store.begin().map_err(Error::from)?;
        let mut held = Atomic {
            session: self,
            open: Some((transaction, Span::new(0, 0))),
        };
        let answered = work(&mut held);
        let Some((transaction, _)) = held.open.take() else {
            // Unreachable while `run_with` refuses the verbs that close it; named
            // so that a change there is a refusal rather than a silent success.
            return Err(Error::TransactionVerbInAtomic {
                span: Span::new(0, 0),
            }
            .into());
        };
        match answered {
            Ok(value) => {
                settle(transaction)?;
                Ok(value)
            }
            Err(failure) => {
                transaction.rollback();
                Err(failure)
            }
        }
    }
}
