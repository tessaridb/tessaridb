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
use bgv_db_storage::{Catalog, Store, Transaction};

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
    store: &'a Store,
    namespace: Option<String>,
    database: Option<String>,
    identity: Identity,
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

    /// Refuse the statement when this session may not run it.
    ///
    /// The check is one catalog read per statement. It reads `is_open` — whether
    /// the store has any user at all — because an empty store must stay usable,
    /// and that is a property of the data rather than of the session.
    fn authorize(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: bgv_db_ql::Span,
    ) -> Result<()> {
        let mut transaction = store.begin()?;
        let open = Catalog::new(&mut transaction).is_open()?;
        transaction.rollback();
        self.identity.allows(kind, open, span)?;
        self.within_tenancy(kind, span)
    }

    /// Refuse a resolved tenancy that is not the signed-in user's own.
    ///
    /// This is the check that **cannot be walked around**, and it is here rather
    /// than at `USE` for a reason: a statement may name a database directly —
    /// `SELECT * FROM other.notes` — and never touch the session's selection at
    /// all. Every path that reaches a record first resolves a namespace and a
    /// database into ids, so refusing at that resolution refuses all of them by
    /// construction. Checking only `USE` would guard the front door of a room
    /// with two.
    ///
    /// The refusal names the **tenancy** and never says whether the record or
    /// the table exists: a refusal that leaks that has answered the question it
    /// declined.
    /// `named` is the tenancy as the author wrote it, which is what the refusal
    /// echoes back. Naming it leaks nothing they did not already type, and it is
    /// the only thing here that reads as an answer to what they asked.
    pub(crate) fn permits(
        &self,
        namespace: bgv_db_types::NamespaceId,
        database: bgv_db_types::DatabaseId,
        named: &str,
        span: bgv_db_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let outside = user.namespace.is_some_and(|own| own != namespace)
            || user.database.is_some_and(|own| own != database);
        if outside {
            return Err(Error::OutsideTenancy {
                name: named.to_owned(),
                span,
            });
        }
        Ok(())
    }

    /// Refuse a statement reaching outside the tenancy its user belongs to.
    ///
    /// This one catches `USE` specifically, which resolves no tenancy of its own
    /// — it only records a name for the statements after it. Without it a scoped
    /// user's `USE NAMESPACE other` would succeed and the refusal would arrive
    /// one statement later, naming something the author did not just write.
    fn within_tenancy(&self, kind: &StatementKind, span: bgv_db_ql::Span) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let Some(_) = user.namespace else {
            return Ok(());
        };
        // A scoped user may only work inside the namespace and database it was
        // declared in, and `USE` is where a session says which those are.
        if let StatementKind::Use {
            namespace,
            database,
        } = kind
        {
            for named in [namespace.as_ref(), database.as_ref()]
                .into_iter()
                .flatten()
            {
                if !self.names_own_tenancy(user, &named.text) {
                    return Err(Error::OutsideTenancy {
                        name: named.text.clone(),
                        span,
                    });
                }
            }
        }
        Ok(())
    }

    /// Whether this name is one of the user's own tenancy names.
    fn names_own_tenancy(&self, user: &bgv_db_storage::UserDefinition, named: &str) -> bool {
        let Ok(mut transaction) = self.store.begin() else {
            return false;
        };
        let catalog = Catalog::new(&mut transaction);
        let matches = user.namespace.is_some_and(|id| {
            catalog
                .namespace(id)
                .ok()
                .flatten()
                .is_some_and(|found| found.name == named)
        }) || user.database.is_some_and(|id| {
            catalog
                .database(id)
                .ok()
                .flatten()
                .is_some_and(|found| found.name == named)
        });
        transaction.rollback();
        matches
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
