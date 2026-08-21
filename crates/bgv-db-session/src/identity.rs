//! Who a session is, and what that lets it do.
//!
//! # A credential never travels in the query language
//!
//! `SIGNIN` is deliberately **not a statement**. A script is text a caller
//! composes, logs, pastes into an issue and sends through a proxy, and a
//! password in one is a password in all of those. Signing in is a method.
//!
//! `DEFINE USER … PASSWORD '…'` *is* a statement, because creating a user is
//! schema and schema is bgvQL (ADR-0003). The plaintext exists only for the
//! length of that call: what reaches the catalog — and therefore the log, and
//! therefore every replica and every backup — is an Argon2 hash.
//!
//! # A store with no users is open
//!
//! Requiring a signin against an empty store locks everybody out of it with no
//! way in to fix that. So a store with no user runs anything, and **declaring
//! the first user closes it**. That rule is stated here rather than discovered.
//!
//! Once closed, it stays closed: `DEFINE USER` is **not** an exception an
//! anonymous session keeps. An exception there would be a back door anyone could
//! walk through by declaring themselves an owner, and `DROP USER` is refused for
//! the same reason — otherwise a store could be re-opened from outside by
//! removing the last user.
//!
//! The cost of that is real and belongs in the operations notes rather than
//! hidden here: a lost owner password is a restore from backup, not a recovery.
//!
//! # Enforcement is here, and that is correct
//!
//! Unlike a schema check, which SGA.T7 deferred because it must hold on the
//! replica apply path, a permission is checked once by the session that
//! executes the statement. A replica applies what a leader **already
//! authorized**, and re-checking there would need the identity in the log —
//! which is exactly where a credential should not be. Q-18 said "enforcement
//! belongs to the session and permission layer above the store" when it was
//! logged; this is that layer.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use bgv_db_ql::{Name, Span, StatementKind, TableRef};
use bgv_db_storage::{Catalog, Role, Transaction, UserDefinition};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// Who a session is signed in as.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Identity {
    /// Nobody. Allowed everything on an open store and nothing on a closed one.
    #[default]
    Anonymous,
    /// A declared user.
    Signed(Box<UserDefinition>),
}

/// Hash a password for storage.
///
/// # Errors
///
/// Returns [`Error::PasswordUnusable`] when the hasher refuses the input, which
/// it does for a password long enough to be a denial of service by itself.
pub(crate) fn hash(password: &str, span: Span) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hashed| hashed.to_string())
        .map_err(|_| Error::PasswordUnusable { span })
}

/// Whether this password produces that stored hash.
///
/// A stored hash that cannot be parsed answers **false** rather than raising:
/// the alternative is that corrupting one byte of a credential turns a refusal
/// into a five-hundred, which tells an attacker more than a refusal does.
pub(crate) fn verifies(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// What a statement needs to be allowed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Needs {
    /// Reading, which a `viewer` may do.
    Read,
    /// Writing records or defining structure, which an `editor` may do.
    Write,
    /// Declaring users, which only an `owner` may do.
    Administer,
}

impl Needs {
    /// What this statement needs.
    pub(crate) const fn of(kind: &StatementKind) -> Self {
        match kind {
            StatementKind::Select(_)
            | StatementKind::Get { .. }
            | StatementKind::Keys { .. }
            // `USE` and the transaction verbs change what the *next* statement
            // runs in rather than touching anything, and refusing them would
            // make a viewer unable to say which database it is reading.
            | StatementKind::Use { .. }
            | StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel => Self::Read,
            StatementKind::DefineUser { .. } | StatementKind::DropUser { .. } => Self::Administer,
            _ => Self::Write,
        }
    }

    /// Whether this role is enough.
    const fn granted_to(self, role: Role) -> bool {
        match self {
            Self::Read => true,
            Self::Write => matches!(role, Role::Editor | Role::Owner),
            Self::Administer => matches!(role, Role::Owner),
        }
    }
}

impl Identity {
    /// Refuse the statement when this identity may not run it.
    ///
    /// `open` is whether the store has any user at all.
    pub(crate) fn allows(&self, kind: &StatementKind, open: bool, span: Span) -> Result<()> {
        let needs = Needs::of(kind);
        match self {
            // An open store runs anything, which is what makes an empty one
            // usable at all.
            Self::Anonymous if open => Ok(()),
            // "I do not know you" — a different answer from "I know you and no",
            // and a client needs to tell them apart to know whether to sign in.
            Self::Anonymous => Err(Error::NotSignedIn { span }),
            Self::Signed(user) if needs.granted_to(user.role) => Ok(()),
            Self::Signed(user) => Err(Error::RoleForbids {
                role: user.role.name(),
                needs: match needs {
                    Needs::Read => "read",
                    Needs::Write => "write",
                    Needs::Administer => "administer",
                },
                span,
            }),
        }
    }

    /// The user, when there is one.
    pub(crate) const fn user(&self) -> Option<&UserDefinition> {
        match self {
            Self::Anonymous => None,
            Self::Signed(user) => Some(user),
        }
    }
}

impl Session<'_> {
    /// Declare a user.
    ///
    /// The password is hashed **here** and the plaintext goes no further: what
    /// reaches the catalog, and therefore the log and every replica, is an
    /// Argon2 hash. The tenancy is resolved to ids the same way every other
    /// reference is, so a user cannot be declared against a database that does
    /// not exist.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn define_user(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        scope: Option<&TableRef>,
        role: &Name,
        password: &str,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .users()?
            .into_iter()
            .any(|user| user.name == name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        let Some(role) = Role::parse(&role.text) else {
            return Err(Error::NoSuchRole {
                name: role.text.clone(),
                span,
            });
        };
        // `prod.orders` names a tenancy here the way it names a table
        // elsewhere: the qualifier is the namespace and the name the database.
        let (namespace, database) = match scope {
            None => (None, None),
            Some(named) => {
                let context = self.tenancy_of(transaction, named)?;
                (Some(context.namespace), Some(context.database))
            }
        };
        let secret = hash(password, span)?;
        Catalog::new(transaction).create_user(&name.text, namespace, database, role, &secret)?;
        Ok(Outcome::Done)
    }

    /// Remove a user's declaration.
    ///
    /// Dropping one that is not there is not an error: the statement asks for a
    /// store without that user, and a store without that user is what it leaves.
    pub(crate) fn drop_user(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .users()?
            .into_iter()
            .find(|user| user.name == name.text);
        if let Some(user) = found {
            Catalog::new(transaction).drop_user(&user)?;
        }
        Ok(Outcome::Done)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_ql::Span;

    use super::{hash, verifies};

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let stored = hash("correct horse", Span::new(0, 1)).expect("a hash");
        assert!(verifies("correct horse", &stored));
        assert!(!verifies("correct horses", &stored));
        assert!(!verifies("", &stored));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // A salt per hash, so two users with one password do not share a stored
        // value — and a leaked catalog does not say who shares a password.
        let first = hash("same", Span::new(0, 1)).expect("a hash");
        let second = hash("same", Span::new(0, 1)).expect("a hash");
        assert_ne!(first, second);
        assert!(verifies("same", &first));
        assert!(verifies("same", &second));
    }

    #[test]
    fn a_stored_hash_that_is_not_one_refuses_rather_than_failing() {
        // Corrupting a byte of a credential must not turn a refusal into a
        // five-hundred: that tells an attacker more than the refusal does.
        assert!(!verifies("anything", "not a hash"));
        assert!(!verifies("anything", ""));
    }

    #[test]
    fn the_stored_form_carries_no_plaintext() {
        let stored = hash("hunter2", Span::new(0, 1)).expect("a hash");
        assert!(!stored.contains("hunter2"), "{stored}");
        assert!(stored.starts_with("$argon2"), "{stored}");
    }
}
