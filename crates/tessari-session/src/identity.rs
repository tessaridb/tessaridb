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

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use tessari_constants::{PASSWORD_HASH_LANES, PASSWORD_HASH_MEMORY_KIB, PASSWORD_HASH_PASSES};
use tessari_ql::{InfoSubject, Name, Password, Span, StatementKind, TableRef, UserChange};
use tessari_storage::{Catalog, Role, Transaction, UserDefinition};

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

/// The hasher both sides of a credential use, at parameters this project chose.
///
/// # Why not `Argon2::default()`
///
/// It was, and the values it gave were the right ones. The rule it broke is not
/// about the value: **LR-DB-004** says a default is read from the release in use
/// and recorded with it, because a default nobody read is not evidence. Inherited
/// from the crate, a `cargo update` that moved `Default` would move this store's
/// password-hashing posture with nothing in the repository, the decision records
/// or the tests saying so — and nothing would break, because a PHC string
/// carries its own parameters and old hashes keep verifying at their old cost.
/// Silence is the whole failure. The three numbers live in `tessari-constants`
/// and their reading is ADR-0043.
///
/// # Why one constructor and not two call sites
///
/// Hashing and verifying must agree, and two literal parameter sets are two
/// things that can drift. They would drift *quietly*: a verification at the wrong
/// parameters still succeeds, because the stored PHC string says what to use, so
/// the only symptom would be that new hashes stopped matching the recorded
/// intent — which nothing is looking at.
///
/// Returns `None` only if the constants are not a valid parameter set, which is
/// a condition of this repository rather than of any input, and is why
/// `the_pinned_parameters_are_a_valid_set` exists to fail in CI instead of here.
fn hasher() -> Option<Argon2<'static>> {
    Params::new(
        PASSWORD_HASH_MEMORY_KIB,
        PASSWORD_HASH_PASSES,
        PASSWORD_HASH_LANES,
        None,
    )
    .ok()
    .map(|params| Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password for storage.
///
/// # Errors
///
/// Returns [`Error::PasswordUnusable`] when the hasher refuses the input, which
/// it does for a password long enough to be a denial of service by itself.
pub(crate) fn hash(password: &str, span: Span) -> Result<String> {
    // Refused here rather than at each caller, so a path added later cannot set
    // one by forgetting to ask. An empty password is not a weak credential; it
    // is an account anybody holding the name can be.
    if password.is_empty() {
        return Err(Error::PasswordEmpty { span });
    }
    let salt = SaltString::generate(&mut OsRng);
    hasher()
        .ok_or(Error::PasswordUnusable { span })?
        .hash_password(password.as_bytes(), &salt)
        .map(|hashed| hashed.to_string())
        .map_err(|_| Error::PasswordUnusable { span })
}

/// Whether this password produces that stored hash.
///
/// A stored hash that cannot be parsed answers **false** rather than raising:
/// the alternative is that corrupting one byte of a credential turns a refusal
/// into a five-hundred, which tells an attacker more than a refusal does.
/// A hasher this build cannot construct answers **false** for the same reason,
/// which is the safe direction: no password matches anything until the
/// parameters are a set again.
pub(crate) fn verifies(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|parsed| {
        hasher().is_some_and(|argon| argon.verify_password(password.as_bytes(), &parsed).is_ok())
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
    /// Administering the **store itself** — an owner holding no tenancy.
    ///
    /// Role and reach are two axes, and the pair below is what happens when a
    /// statement's subject is the store: the role says what kind of act it is,
    /// and the reach says that the caller's own tenancy has to be the whole
    /// thing. The ordinary tenancy check cannot ask the second question here,
    /// because it compares what a statement *names* against what the caller
    /// holds — and these statements name nothing.
    ///
    /// An owner of one database satisfies [`Needs::Administer`] and must not
    /// satisfy this, or `BACKUP` hands them every record in every namespace.
    AdministerStore,
    /// Writing structure at **store level** — an editor or owner holding no
    /// tenancy.
    ///
    /// `DEFINE NAMESPACE` is the whole of it. A namespace is a *sibling* of
    /// every other namespace, so declaring one is not shaping the data you own
    /// unless the store *is* what you own — which is why the reach is required
    /// and the role is not raised. A store-wide editor may already define
    /// databases, tables and records anywhere; a namespace is strictly less
    /// than that. An editor of one database may not, and that was the defect.
    WriteStore,
}

impl Needs {
    /// What this statement needs.
    ///
    /// # Every statement is named, and there is no catch-all
    ///
    /// There used to be one — `_ => Self::Write` — and it read as the safe
    /// default, which is precisely why it was not. `READ` was added and fell
    /// through it, so a grant of `read` on a bucket could list the files and not
    /// open one; the mistake was invisible because the arm was doing exactly
    /// what it says. A catch-all mis-classifies a new statement *silently*, and
    /// a permission that is one class too strict looks like a bug in the grant
    /// rather than a bug here.
    ///
    /// So the match is exhaustive, the way `tables_named` and the conformance
    /// coverage list already are: adding a statement to the language will not
    /// compile until somebody says what it needs.
    pub(crate) const fn of(kind: &StatementKind) -> Self {
        match kind {
            // Reading **this node** is administering, and it is the one read
            // that is. Every other `SELECT` is governed by a grant on the table
            // it names, and `$node` names none — so left as `Read` it would be
            // checked by a loop over an empty list, which passes for reasons
            // unrelated to permission. That is the vacuous shape `tables_named`
            // already refuses `BACKUP` by name for.
            //
            // The answer is also not divisible: roles and endpoints are this
            // machine's position in a topology, and there is no smaller truthful
            // version of them to hand a `viewer` — the same reasoning that puts
            // `INFO FOR USER` here rather than beside the other four subjects.
            StatementKind::Select(select) if matches!(select.from, tessari_ql::Source::Node) => {
                Self::AdministerStore
            }
            // `EXPLAIN` of the same read needs the same permission, for the
            // reason `tables_named` gives it: a plan that named a source the
            // caller may not read is a disclosure wearing a diagnostic's
            // clothes.
            StatementKind::Explain(select) if matches!(select.from, tessari_ql::Source::Node) => {
                Self::AdministerStore
            }
            StatementKind::Select(_)
            | StatementKind::Get { .. }
            | StatementKind::Keys { .. }
            // Reading a file is reading. Named rather than left to the
            // catch-all, which reads as `Write` — the default that is right for
            // every statement that changes something and wrong for this one.
            | StatementKind::Read { .. }
            // Explaining a read is reading: the catalog, about a table. The
            // caller must be allowed both, and `tables_named` says which.
            | StatementKind::Explain(_)
            // `USE` and the transaction verbs change what the *next* statement
            // runs in rather than touching anything, and refusing them would
            // make a viewer unable to say which database it is reading.
            | StatementKind::Use { .. }
            | StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel => Self::Read,
            // Granting is administering: it decides what somebody else may do,
            // which is the same kind of act as declaring them.
            StatementKind::DefineUser { .. }
            | StatementKind::AlterUser { .. }
            | StatementKind::DropUser { .. }
            | StatementKind::Grant { .. }
            | StatementKind::Revoke { .. } => Self::Administer,
            // A backup is every record in the store, past every grant and every
            // tenancy boundary. There is no permission smaller than "may see all
            // of it", so the role is the whole check — and a grant can never add
            // to it, which `within_grants` says out loud rather than leaving to
            // the fact that a backup names no table.
            StatementKind::Backup { .. } => Self::AdministerStore,
            // Asking about a **user** is asking what the permission system says,
            // so it is the same kind of act as writing it. The other four
            // subjects filter — they report the tables and fields the caller may
            // already read — but this one cannot: there is no smaller truthful
            // answer about who may do what, and a partial one reads as the whole
            // answer. So it refuses, and only an owner is answered.
            StatementKind::Info {
                subject: InfoSubject::User(_) | InfoSubject::Users,
            } => Self::Administer,
            // Asking about **this node** is the `$node` read wearing a
            // statement's clothes, and it lands here for exactly the reason that
            // one did: it names no table, so the grant loop passes over it
            // vacuously, and roles and endpoints are this machine's position in
            // a topology with no smaller truthful version to hand a `viewer`.
            //
            // The peer half of its answer sharpens it rather than softening it:
            // the list of every machine holding this store's data is not a
            // description of the caller's own tables.
            StatementKind::Info {
                subject: InfoSubject::Node,
            } => Self::AdministerStore,
            // Configuring the node is administering it. Not `Write`, which is
            // where the other `DEFINE`s sit: an `editor` is expected to shape
            // the data they own, and neither what this machine is for nor which
            // other machines hold the data is that.
            StatementKind::DefineNode { .. } | StatementKind::DefineReplica { .. } => {
                Self::AdministerStore
            }
            // Declaring a consumer is administering, not writing — the same
            // reasoning that puts `DEFINE USER` here. It hands a broker address
            // and a group name to a process that will then write into somebody's
            // table with nobody watching, which is a decision about what runs
            // rather than about what the data looks like.
            //
            // `Administer` and not `AdministerStore`, because a consumer lives in
            // the database its destination lives in: an owner of `prod.shop`
            // should be able to declare what feeds `prod.shop.orders`. The reach
            // check still applies, because `tables_named` reports the
            // destination — which is the difference between this and `BACKUP`.
            StatementKind::DefineConsumer { .. } | StatementKind::DropConsumer { .. } => {
                Self::Administer
            }
            // Asking about a consumer is asking for a broker address, a group
            // name and a running position. It **refuses rather than filters**,
            // for `INFO FOR USER`'s reason: there is no smaller truthful answer
            // about what a background writer is doing, and a partial one reads as
            // the whole one.
            //
            // This arm has to be written rather than left to the `Info` catch-all
            // below, and that is worth saying out loud: the catch-all means a new
            // `InfoSubject` does **not** raise a compile error, so the ratchet
            // that protects every other statement does not protect this one. A
            // subject added and forgotten would be answered to a `viewer`.
            StatementKind::Info {
                subject: InfoSubject::Consumer(_) | InfoSubject::Consumers,
            } => Self::Administer,
            // The other four are reads of the catalog, and what they report is
            // narrowed to what the caller could have found out anyway.
            StatementKind::Info { .. } => Self::Read,
            // A namespace is a **sibling** of every other namespace, and nothing
            // contains a sibling — so declaring one is not shaping the data you
            // own, it is adding to the store's top-level list. Left with the
            // `Write` block below, an `editor` of one database could do it: a
            // caller with authority over no tenancy at all, adding one.
            StatementKind::DefineNamespace { .. } => Self::WriteStore,
            // Everything else changes something: the records, or the structure
            // they are held in. Defining and dropping sit here rather than under
            // `Administer` because an `editor` is expected to shape the data
            // they own; only deciding what *another* person may do is reserved.
            StatementKind::DefineDatabase { .. }
            | StatementKind::DefineTable { .. }
            | StatementKind::DefineSpace { .. }
            | StatementKind::DefineBucket { .. }
            | StatementKind::DefineIndex { .. }
            | StatementKind::DefineField { .. }
            | StatementKind::DefineAnalyzer { .. }
            | StatementKind::DropTable { .. }
            | StatementKind::DropIndex { .. }
            | StatementKind::RebuildIndex { .. }
            | StatementKind::DropField { .. }
            | StatementKind::Relate { .. }
            | StatementKind::Create { .. }
            | StatementKind::Update { .. }
            | StatementKind::Delete { .. }
            | StatementKind::DeleteWhere { .. }
            | StatementKind::Set { .. }
            | StatementKind::Del { .. }
            | StatementKind::Put { .. } => Self::Write,
        }
    }

    /// Whether this role is enough.
    const fn granted_to(self, role: Role) -> bool {
        match self {
            Self::Read => true,
            Self::Write => matches!(role, Role::Editor | Role::Owner),
            // The store-wide pair needs a reach as well, and that half is asked
            // in `allows_needs`, where the identity is in hand — a role on its
            // own cannot answer it.
            Self::WriteStore => matches!(role, Role::Editor | Role::Owner),
            Self::Administer | Self::AdministerStore => matches!(role, Role::Owner),
        }
    }
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
            // The role first, so an owner of a part and a viewer are told
            // different things: for the viewer the missing piece really is the
            // role, and telling them about reach sends them to ask for the
            // wrong grant.
            Self::Signed(user) if !needs.granted_to(user.role) => Err(Error::RoleForbids {
                role: user.role.name(),
                needs: match needs {
                    Needs::Read => "read",
                    Needs::Write => "write",
                    Needs::WriteStore => "write",
                    Needs::Administer | Needs::AdministerStore => "administer",
                },
                span,
            }),
            // The reach, for the statements that have no subject to check it
            // against. A tenancy of one's own is exactly what disqualifies:
            // holding `prod.shop` means the store is not yours to act on.
            Self::Signed(user)
                if matches!(needs, Needs::AdministerStore | Needs::WriteStore)
                    && user.namespace.is_some() =>
            {
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
        password: &Password,
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
        Catalog::new(transaction).create_user(&name.text, namespace, database, role, &secret)?;
        Ok(Outcome::Done)
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
                user.role = role;
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
        assert!(super::hasher().is_some());
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
