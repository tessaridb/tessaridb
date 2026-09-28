//! Who a session is, and what that lets it do.
//!
//! # A credential never travels in the query language
//!
//! `SIGNIN` is deliberately **not a statement**. A script is text a caller
//! composes, logs, pastes into an issue and sends through a proxy, and a
//! password in one is a password in all of those. Signing in is a method.
//!
//! `DEFINE USER … PASSWORD '…'` *is* a statement, because creating a user is
//! schema and schema is TessariQL (ADR-0003). The plaintext exists only for the
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

mod grants;
mod kinds;
mod needs;
mod password;
use tessari_ql::{Name, Password, ReachRef, Span, StatementKind, UserChange, UserGrant};
use tessari_storage::{Catalog, Held, Kind, Reach, Role, Transaction, UserDefinition};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;
pub(crate) use needs::{At, Needs};
pub(crate) use password::{hash, verifies};

/// Who a session is signed in as.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Identity {
    /// Nobody. Allowed everything on an open store and nothing on a closed one.
    #[default]
    Anonymous,
    /// A declared user.
    Signed(Box<UserDefinition>),
}

impl Identity {
    /// Refuse the statement when this identity may not run it.
    ///
    /// `open` is whether the store has any user at all.
    pub(crate) fn allows(&self, kind: &StatementKind, open: bool, span: Span) -> Result<()> {
        self.allows_needs(Needs::of(kind), open, span)
    }

    /// The same rule, for a caller that knows what it needs rather than which
    /// statement it is running.
    ///
    /// A subscription is the caller: it is not a statement and never reaches the
    /// executor, but it reads records, and "who may read" must be answered here
    /// rather than a second time somewhere else.
    pub(crate) fn allows_needs(&self, needs: Needs, open: bool, span: Span) -> Result<()> {
        match self {
            // An open store runs anything, which is what makes an empty one
            // usable at all.
            Self::Anonymous if open => Ok(()),
            // "I do not know you" — a different answer from "I know you and no",
            // and a client needs to tell them apart to know whether to sign in.
            Self::Anonymous => Err(Error::NotSignedIn { span }),
            // The kind first, so a caller who holds the authority somewhere and
            // a caller who holds it nowhere are told different things: for the
            // second the missing piece really is the authority, and telling them
            // about reach sends them to ask the wrong person.
            Self::Signed(user) if needs.unheld_by(user).is_some() => {
                let missing = needs.unheld_by(user).unwrap_or(Kind::Read);
                Err(Error::RoleForbids {
                    // A user whose set no role summarises has no role to name,
                    // and saying so is more use than naming one they do not hold.
                    role: user.role.map_or("authorities", Role::name),
                    // The kind, not the class. `a viewer may not manage` sends
                    // the reader to the authority they need; the old `may not
                    // write` sent every one of the fifteen container statements
                    // to ask for a permission that would not have helped.
                    needs: missing.name(),
                    span,
                })
            }
            // The reach, for the statements whose subject is the store and which
            // therefore have no name to check it against. A tenancy of one's own
            // is exactly what disqualifies: holding `prod.shop` means the store
            // is not yours to act on.
            Self::Signed(user) if needs.at() == At::Store && user.namespace.is_some() => {
                Err(Error::NotTheWholeStore {
                    user: user.name.clone(),
                    span,
                })
            }
            // Both questions asked and both answered.
            Self::Signed(_) => Ok(()),
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

/// What `DEFINE USER` says, carried to [`Session::define_user`] as one thing.
pub(crate) struct UserDeclaration<'a> {
    /// The name signed in with.
    pub(crate) name: &'a Name,
    /// How far the user reaches, or the store when absent.
    pub(crate) scope: Option<&'a ReachRef>,
    /// What the user may do.
    pub(crate) role: &'a UserGrant,
    /// The password, as written.
    pub(crate) password: &'a Password,
    /// Whether re-defining an existing name is accepted.
    pub(crate) if_not_exists: bool,
}

impl Session<'_> {
    /// Declare a user.
    ///
    /// The password is hashed **here** and the plaintext goes no further: what
    /// reaches the catalog, and therefore the log and every replica, is an
    /// Argon2 hash. The tenancy is resolved to ids the same way every other
    /// reference is, so a user cannot be declared against a database that does
    /// not exist.
    pub(crate) fn define_user(
        &self,
        transaction: &mut Transaction<'_>,
        declaration: UserDeclaration<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let UserDeclaration {
            name,
            scope,
            role,
            password,
            if_not_exists,
        } = declaration;
        let declared = Catalog::new(transaction)
            .users()?
            .into_iter()
            .any(|user| user.name == name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        let reach = match scope {
            None => Reach::Store,
            Some(named) => self.reach_of(transaction, named)?,
        };
        let (namespace, database) = reach.parts();
        let authorities = self.held_from(role, reach, span)?;
        // You may not declare somebody who reaches further than you do. Without
        // this, an owner of one database declares an owner of the **whole node**
        // — no `ON`, so no tenancy, so no bound — and then signs in as them.
        // Every other check on a user becomes decorative at that point, because
        // the way past them all is to mint a wider identity rather than to touch
        // an existing one. Measured before it was closed: `nina`, an owner of
        // `prod.shop`, created `mallory` with no tenancy and `mallory` could
        // list the store.
        if !self.may_reach(namespace, database) {
            return Err(Error::WiderThanYou {
                user: name.text.clone(),
                span,
            });
        }
        let secret = hash(password.expose(), span)?;
        Catalog::new(transaction).create_user(
            &name.text,
            namespace,
            database,
            &authorities,
            &secret,
        )?;
        Ok(Outcome::Done)
    }

    /// The user a grant names, refused when they are not this caller's to touch.
    ///
    /// The same `administers` check every other statement about a user makes,
    /// and it is not the `Needs` check that already ran: that one says the
    /// caller administers *something*.
    fn user_to_change(
        &self,
        transaction: &mut Transaction<'_>,
        user: &Name,
        span: Span,
    ) -> Result<UserDefinition> {
        let Some(found) = Catalog::new(transaction)
            .users()?
            .into_iter()
            .find(|found| found.name == user.text)
        else {
            return Err(Error::Unknown {
                entity: "user",
                name: user.text.clone(),
                span,
            });
        };
        if !self.administers(&found) {
            return Err(Error::NotYours {
                user: found.name.clone(),
                span,
            });
        }
        Ok(found)
    }

    /// Change one thing about a user who already exists.
    ///
    /// Read the whole definition, replace the single field the statement names,
    /// write it back. Everything else — the id, the name, the tenancy, and the
    /// grants keyed by the id — is carried through untouched, which is what
    /// makes rotating a password unable to quietly reset a role.
    ///
    /// The tenancy check is the security half, and it is not the `Administer`
    /// check that already ran: that one says the caller owns *something*. Without
    /// this one an owner of a single namespace could set the store owner's
    /// password and take the node, which is a privilege escalation dressed as
    /// routine administration.
    pub(crate) fn alter_user(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        change: &UserChange,
        span: Span,
    ) -> Result<Outcome> {
        let Some(mut user) = Catalog::new(transaction)
            .users()?
            .into_iter()
            .find(|held| held.name == name.text)
        else {
            return Err(Error::Unknown {
                entity: "user",
                name: name.text.clone(),
                span,
            });
        };
        if !self.administers(&user) {
            return Err(Error::NotYours {
                user: user.name.clone(),
                span,
            });
        }
        match change {
            UserChange::Password(password) => {
                user.secret = hash(password.expose(), span)?;
            }
            UserChange::Role(named) => {
                let Some(role) = Role::parse(&named.text) else {
                    return Err(Error::NoSuchRole {
                        name: named.text.clone(),
                        span: named.span,
                    });
                };
                // The set moves with the name, or neither moves. Leaving the set
                // behind would make `SET ROLE viewer` read as a demotion while
                // changing nothing the store consults; writing the role without
                // the set would leave the record describing authorities nobody
                // holds. Both halves under one condition is what stops either.
                if let Some(reach) = Reach::of(user.namespace, user.database) {
                    user.authorities = Held::from_role(role, reach);
                    user.role = Some(role);
                }
            }
        }
        Catalog::new(transaction).update_user(&user);
        Ok(Outcome::Done)
    }

    /// Remove a user's declaration.
    ///
    /// Dropping one that is not there is not an error: the statement asks for a
    /// store without that user, and a store without that user is what it leaves.
    ///
    /// Dropping one you do not administer **is**, and this is the most costly
    /// place that check was missing. There is deliberately no way to re-open a
    /// closed store from outside, so an owner of one database removing the
    /// store's owner does not merely exceed their authority — it locks that
    /// account out permanently, and the grants go with it. A takeover and a
    /// denial of service in one statement, and every log of it reads as routine
    /// administration.
    pub(crate) fn drop_user(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .users()?
            .into_iter()
            .find(|user| user.name == name.text);
        if let Some(user) = found {
            if !self.administers(&user) {
                return Err(Error::NotYours {
                    user: user.name.clone(),
                    span,
                });
            }
            // The grants go with the user. Leaving them would let a later user
            // allocated the same id inherit permissions nobody gave them, which
            // is the same shape of bug as a reused table id resolving a stale
            // reference — and the catalog already refuses to reuse ids for
            // exactly that reason.
            for held in Catalog::new(transaction).grants_for(user.id)? {
                Catalog::new(transaction).revoke(user.id, held.table);
            }
            Catalog::new(transaction).drop_user(&user)?;
        }
        Ok(Outcome::Done)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessari_ql::Span;

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

    #[test]
    fn a_stored_hash_carries_the_parameters_this_project_pinned() {
        // The assertion F-008 asks for, and it extends the one above by the part
        // that matters: `$argon2` says only that some Argon2 produced this. A
        // dependency upgrade that moved `Default` would keep that prefix and
        // change everything behind it, which is precisely the silent move
        // LR-DB-004 exists to catch. Written as the literal string rather than
        // interpolated from the constants: interpolating would make this test
        // agree with whatever the constants say, and agreeing with the thing
        // under test is not a check.
        let stored = hash("correct horse", Span::new(0, 1)).expect("a hash");
        assert!(
            stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "the pinned parameter set is not the one that produced this: {stored}"
        );
    }

    #[test]
    fn the_pinned_parameters_are_a_valid_set() {
        // `hasher` answers `None` rather than panicking on a parameter set the
        // crate rejects, so a bad constant would otherwise surface as every
        // sign-in refusing at run time with nothing pointing at the cause. This
        // is where that fails instead.
        assert!(super::password::hasher().is_some());
    }

    #[test]
    fn the_hash_a_missing_name_is_checked_against_carries_the_same_parameters() {
        // The sentinel in `session.rs` exists so a refusal for a name that does
        // not exist costs the same as one for a wrong password. That only holds
        // while it is a hash at *these* parameters — pinning `m`, `t` and `p`
        // and leaving a sentinel at older ones would make the two refusals
        // measurably different lengths, and the timing equalisation would be
        // decoration. The two must move together, so this fails when only one
        // does.
        assert!(
            crate::session::ABSENT_USER_HASH.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "the absent-user sentinel is not at the pinned parameters"
        );
        // And it must still parse, or the equalisation costs nothing at all
        // because `verifies` gives up before reaching the hasher. Asserted on
        // the parse and **not** on `verifies` returning false: a sentinel that
        // failed to parse would also return false, so that assertion would pass
        // for the failure it was written to catch.
        assert!(
            argon2::password_hash::PasswordHash::new(crate::session::ABSENT_USER_HASH).is_ok(),
            "the sentinel does not parse, so no work is done for a missing name"
        );
    }
}
