//! The session: what a script is run against, and what it remembers between
//! statements.
//!
//! A session remembers two things — the namespace and database `USE` selected —
//! and it remembers them **by name, not by id**. Resolving a name to an id once
//! and keeping it would leave a session pointing at a table that has since been
//! dropped and re-created under the same name, reading the wrong one with no
//! error anywhere. The lookup is one catalog read per statement, and the store
//! is the only thing entitled to say what a name currently means.

use bgv_db_ql::{Parameters, Statement, StatementKind, parse};
use bgv_db_storage::{Catalog, Store, Transaction};

use crate::effect::{Effect, admits};
use crate::error::{Error, Result};
use crate::identity::{self, Identity};
use crate::outcome::Outcome;

/// A hash to check a name that does not exist against.
///
/// A refusal for an unknown name must take about as long as one for a wrong
/// password, or the time itself says which half was wrong. This is a real Argon2
/// hash of a value nobody knows, kept so the work happens either way.
const ABSENT_USER_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$    c29tZXNhbHR2YWx1ZXNhbHQ$T8Q9M0Kdc5Cd3nZ3vFCVYD1CkPqmVWmvJcCf7EDlM2c";

/// A connection's worth of state: where statements run, and against what.
#[derive(Debug)]
pub struct Session<'a> {
    /// Visible to the crate for the same reason `identity` is.
    pub(crate) store: &'a Store,
    namespace: Option<String>,
    database: Option<String>,
    /// Visible to the crate because `authorize.rs` asks it three questions.
    pub(crate) identity: Identity,
}

impl<'a> Session<'a> {
    /// Open a session on a store, with nothing selected.
    #[must_use]
    pub const fn new(store: &'a Store) -> Self {
        Self {
            store,
            identity: Identity::Anonymous,
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
        self.run_with(source, &Parameters::new())
    }

    /// Read a script, give its parameters the values `parameters` binds, and run
    /// it.
    ///
    /// This is what [`Session::run`] does with an empty map, and it exists so a
    /// caller with a value does not have to write that value into the script
    /// text. A parameter is legal wherever a literal is and nowhere a name is,
    /// and binding happens **after** parsing — so a supplied value cannot become
    /// syntax no matter what it holds.
    ///
    /// A binding nobody used is accepted; a parameter nobody bound is refused,
    /// before the first statement runs.
    ///
    /// # Errors
    ///
    /// [`bgv_db_ql::Error::UnboundParameter`] when the script names a parameter
    /// this map has no value for, and nothing is written when it does. Otherwise
    /// as [`Session::run`].
    pub fn run_with(&mut self, source: &str, parameters: &Parameters) -> Result<Vec<Outcome>> {
        let store = self.store;
        let script = parse(source)?.bind(parameters)?;

        // Where a statement may run, asked once for the whole script and before
        // any of it runs — a script that writes must not have its first half
        // committed here and its second half refused.
        //
        // **A read pays nothing for the cluster.** The roles live in the store,
        // so consulting them costs a read, and a node standing alone would pay
        // it on every `SELECT` for an answer that is always yes. `Effect` is
        // pure, so asking it first keeps that cost on the writes it belongs to
        // (ADR-0018's `Alone`, and G008 kill criterion 3).
        if matches!(Effect::of_script(&script), Effect::Write) {
            admits(store.node_identity()?.roles, &script)?;
        }

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

    /// Sign in as `name`, if that password matches.
    ///
    /// **Not a statement**, deliberately: a script is text a caller composes,
    /// logs, pastes into an issue and sends through a proxy, and a password in
    /// one is a password in all of those.
    ///
    /// Signing in again replaces the identity rather than adding to it, so a
    /// session is one conversation with one user at a time.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SignInRefused`] for a wrong name and a wrong password
    /// alike — telling them apart tells an attacker which half to keep guessing
    /// at — and a substrate failure otherwise.
    pub fn sign_in(&mut self, name: &str, password: &str) -> Result<()> {
        let mut transaction = self.store.begin()?;
        let found = Catalog::new(&mut transaction)
            .users()?
            .into_iter()
            .find(|user| user.name == name);
        transaction.rollback();

        let Some(user) = found else {
            // The hash is still computed for a name that does not exist, so the
            // time a refusal takes does not say whether the name did.
            let _ = identity::verifies(password, ABSENT_USER_HASH);
            return Err(Error::SignInRefused);
        };
        if !identity::verifies(password, &user.secret) {
            return Err(Error::SignInRefused);
        }
        self.identity = Identity::Signed(Box::new(user));
        Ok(())
    }

    /// Forget who this session is.
    pub fn sign_out(&mut self) {
        self.identity = Identity::Anonymous;
    }

    /// One statement, inside the open transaction or in one of its own.
    fn step(
        &mut self,
        store: &'a Store,
        open: &mut Option<(Transaction<'a>, bgv_db_ql::Span)>,
        statement: &Statement,
    ) -> Result<Outcome> {
        let span = statement.span;
        self.authorize(store, &statement.kind, span)?;
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
