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
use tessari_ql::{
    CreateTarget, Expr, ExprKind, InfoSubject, Name, Password, ReachRef, Span, StatementKind,
    UserChange, UserGrant,
};
use tessari_storage::{Authority, Catalog, Held, Kind, Reach, Role, Transaction, UserDefinition};

use crate::error::{Error, Result};
use crate::info::within;
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

/// What a statement demands: a set of authority kinds, and where they are held.
///
/// # A set, because a rank was the defect
///
/// This used to be one of five ordered classes, and the last arm of [`Needs::of`]
/// assigned `Write` to **twenty-six** statements that are three different
/// authorities: nine that change records, fifteen that create and drop the
/// containers records live in, and two — `DEFINE NAMESPACE` and `DROP NAMESPACE`
/// — whose subject is the store itself rather than anything inside it. Those are
/// the counts of the arm **as it stood at the split**, not of the tree today: the
/// `manage` arm has taken every table kind declared since. So *writing in a
/// namespace* and *creating databases in it* were one permission, and no repair
/// that kept an ordering could separate them — put managing above writing and
/// every manager writes, put it below and every writer manages, and there is no
/// third position. Splitting that arm is the whole of the owner's fourth rule.
///
/// # Exhaustive, and the new way to be wrong
///
/// [`Needs::of`] still has no catch-all, so a statement added to the language
/// cannot compile until somebody classifies it. That protects against a
/// **missing** arm and not against a **thin** one: `{write}` type-checks exactly
/// like `{read, write}`, so an arm with too few kinds is a silent privilege
/// escalation the compiler cannot see. The three sets that exist *only* because
/// something is disclosed — `BACKUP`, `CREATE`/`UPDATE`, `DEFINE KAFKA CONSUMER` —
/// each carry a negative test holding the lesser authority alone, and that test
/// is the only thing standing where the compiler cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Needs {
    /// Every kind the statement requires — all of them, never one of them.
    kinds: &'static [Kind],
    /// Where they must be held.
    at: At,
}

/// Where a demand has to be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum At {
    /// The store itself, and nothing smaller.
    ///
    /// A caller holding a tenancy of their own does not qualify however much
    /// they hold inside it, because the subject of these statements is the thing
    /// that *contains* their tenancy. An owner of one database must not satisfy
    /// this, or `BACKUP` hands them every record in every namespace.
    Store,
    /// Every container the statement reaches.
    ///
    /// The resolved tenancy of each table it names, and the session's own when
    /// it names none — which is what stops a statement naming no table from
    /// passing a per-container loop vacuously, the shape this codebase has had
    /// to refuse `BACKUP` by name for.
    Reached,
}

impl Needs {
    /// Nothing at all: the statement demands no authority.
    ///
    /// A transaction verb, which acts on nothing — the authority is demanded by
    /// the statements inside it. On a closed store a caller still has to be
    /// signed in, and that is a different question, asked in [`Identity`].
    const NOTHING: Self = Self {
        kinds: &[],
        at: At::Reached,
    };
    /// Reading records or the catalog.
    pub(crate) const READ: Self = Self {
        kinds: &[Kind::Read],
        at: At::Reached,
    };
    /// Changing records without learning anything about them.
    const WRITE: Self = Self {
        kinds: &[Kind::Write],
        at: At::Reached,
    };
    /// Changing records by a statement whose refusal discloses prior state.
    const READ_WRITE: Self = Self {
        kinds: &[Kind::Read, Kind::Write],
        at: At::Reached,
    };
    /// Creating and dropping a container's children, and shaping them.
    const MANAGE: Self = Self {
        kinds: &[Kind::Manage],
        at: At::Reached,
    };
    /// The same, where the container is the store — declaring a namespace.
    const MANAGE_STORE: Self = Self {
        kinds: &[Kind::Manage],
        at: At::Store,
    };
    /// Declaring users and moving authority around.
    const GOVERN: Self = Self {
        kinds: &[Kind::Govern],
        at: At::Reached,
    };
    /// Running the node: topology and replicas.
    const OPERATE_STORE: Self = Self {
        kinds: &[Kind::Operate],
        at: At::Store,
    };
    /// Running the node *and* seeing everything in it — the backup file.
    const READ_OPERATE_STORE: Self = Self {
        kinds: &[Kind::Read, Kind::Operate],
        at: At::Store,
    };
    /// Governing, where the thing governed is the store — the audit trail.
    ///
    /// [`Self::GOVERN`] with the container widened, as [`Self::MANAGE_STORE`] is
    /// to [`Self::MANAGE`]. Nothing new is invented: the audit trail is a
    /// question about identities, which is what `govern` answers, and it is held
    /// store-wide because a vault read is recorded before anybody knows whose
    /// tenancy it belonged to. An owner of one namespace must not satisfy it, or
    /// they read every other namespace's reads.
    const GOVERN_STORE: Self = Self {
        kinds: &[Kind::Govern],
        at: At::Store,
    };
    /// Declaring a thing that will later write on the declarer's behalf.
    const MANAGE_WRITE: Self = Self {
        kinds: &[Kind::Manage, Kind::Write],
        at: At::Reached,
    };

    /// The kinds demanded.
    pub(crate) const fn kinds(self) -> &'static [Kind] {
        self.kinds
    }

    /// Where they must be held.
    pub(crate) const fn at(self) -> At {
        self.at
    }

    /// Whether this demand is satisfied by reading alone.
    ///
    /// The one question the table-grant loop asks of it: a grant is a verb on a
    /// table, and there are two verbs. Everything that is not purely a read
    /// needs the write, which is the mapping this had before the kinds existed.
    pub(crate) const fn only_reads(self) -> bool {
        matches!(self.kinds, [Kind::Read])
    }

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
    pub(crate) fn of(kind: &StatementKind) -> Self {
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
                Self::OPERATE_STORE
            }
            // `EXPLAIN` of the same read needs the same permission, for the
            // reason `tables_named` gives it: a plan that named a source the
            // caller may not read is a disclosure wearing a diagnostic's
            // clothes.
            StatementKind::Explain(select) if matches!(select.from, tessari_ql::Source::Node) => {
                Self::OPERATE_STORE
            }
            // A binding is only as privileged as what it holds. `LET $x =
            // (SELECT * FROM $node)` reads this node's identity through an
            // expression, and reading this node is administering — so the
            // question is asked of the expression rather than of the statement
            // word, which would have answered `Read` and handed a viewer the
            // topology.
            StatementKind::Let { value, .. }
            | StatementKind::Return { value }
            | StatementKind::Throw { value }
                if holds_node_read(value) =>
            {
                Self::OPERATE_STORE
            }
            StatementKind::Let { .. }
            | StatementKind::Return { .. }
            | StatementKind::Throw { .. }
            | StatementKind::Select(_)
            | StatementKind::Get { .. }
            // Reading a secret is reading, and demanding more here would be the
            // wrong kind of caution: it would make the grant the second lock,
            // when the key already is. A caller who may read the vault and holds
            // no key is refused at decryption; a caller who holds the key and
            // may not read the vault is refused here. Neither passes by the
            // other's route (F3).
            | StatementKind::Reveal { .. }
            | StatementKind::Keys { .. }
            // Reading a file is reading. Named rather than left to the
            // catch-all, which reads as `Write` — the default that is right for
            // every statement that changes something and wrong for this one.
            | StatementKind::Read { .. }
            // Explaining a read is reading: the catalog, about a table. The
            // caller must be allowed both, and `tables_named` says which.
            | StatementKind::Explain(_)
            => Self::READ,
            // `USE` and the transaction verbs change what the *next* statement
            // runs in rather than touching anything, so they demand nothing.
            //
            // They used to demand `read`, which was harmless while every role
            // began with it and is not any more: an ingestion identity holding
            // `write` alone must be able to select its database and open a
            // transaction, and demanding a read it does not hold would make the
            // model's own headline case unusable.
            //
            // **`USE` should demand *something* at the container it names**, and
            // does not yet. Without that a store-wide holder of one namespace can
            // name any other and learn whether it exists. The check needs the
            // named container resolved, which is not what the session's tenancy
            // holds at the moment `USE` runs, so it is its own slice (Q-252).
            // A scoped user is still refused by `within_tenancy`.
            // Unsealing is running the node. It is store-wide because the key
            // it unwraps is store-wide, and it is `Operate` rather than `Manage`
            // for the same reason `DEFINE NODE` is: it changes what this process
            // can do, not what the store contains.
            StatementKind::SealVault { .. } | StatementKind::UnsealVault { .. } => {
                Self::OPERATE_STORE
            }
            StatementKind::Use { .. }
            | StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel
            | StatementKind::Verify => Self::NOTHING,
            // Governing: deciding what somebody else may do, which is the same
            // kind of act as declaring them.
            //
            // Its own kind rather than the top of a ladder, and that is what
            // makes the owner's ninth rule expressible — an administrator can
            // declare users in a namespace without holding `read` over a single
            // record in it.
            StatementKind::DefineUser { .. }
            | StatementKind::AlterUser { .. }
            | StatementKind::DropUser { .. }
            | StatementKind::Grant { .. }
            | StatementKind::Revoke { .. } => Self::GOVERN,
            // Handing out an authority is `govern`, like every other statement
            // about who may do what — **and this class is not what bounds it**.
            // The bound is `Session::may_hand_out`, which puts the caller's own
            // held set beside the reach in the statement: you must govern there,
            // and you must already hold what you are giving away.
            //
            // It has to be there rather than here because this class cannot see
            // the reach the statement names, and that reach is the whole
            // question. Until the check existed the class was `govern` at the
            // **store** — safe, and too strict by exactly the case the model was
            // asked for, since a namespace authority could not hand out
            // authority inside their own namespace.
            StatementKind::GrantAuthority { .. } | StatementKind::RevokeAuthority { .. } => {
                Self::GOVERN
            }
            // A backup is every record in the store, past every grant and every
            // tenancy boundary. There is no permission smaller than "may see all
            // of it", so the role is the whole check — and a grant can never add
            // to it, which `within_grants` says out loud rather than leaving to
            // the fact that a backup names no table.
            //
            // **`{read, operate}` and not `operate` alone**, which is the
            // correction the ladder hid. Under a rank this was the store owner's
            // class and that identity held `read` anyway, so the coincidence was
            // invisible; decomposed, an `operate`-only identity is supposed to
            // run the cluster and see no records, and a backup is every record
            // there is.
            StatementKind::Backup { .. } => Self::READ_OPERATE_STORE,
            // Asking about a **user** is asking what the permission system says,
            // so it is the same kind of act as writing it. The other four
            // subjects filter — they report the tables and fields the caller may
            // already read — but this one cannot: there is no smaller truthful
            // answer about who may do what, and a partial one reads as the whole
            // answer. So it refuses, and only an owner is answered.
            //
            // `INFO FOR ACCESS TO TABLE` is the same act asked from the other
            // end — who reaches this object rather than what this person
            // reaches — and the answer is made of the same material, so it sits
            // in the same class. A narrower class would be the mistake the two
            // above avoid: a listing of everyone who can read a table, handed to
            // somebody who may only read it, is a map of the permission system
            // drawn for a caller with no business in it.
            StatementKind::Info {
                subject: InfoSubject::User(_) | InfoSubject::Users | InfoSubject::Access(_),
            } => Self::GOVERN,
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
            } => Self::OPERATE_STORE,
            // Configuring the node is administering it. Not `Write`, which is
            // where the other `DEFINE`s sit: an `editor` is expected to shape
            // the data they own, and neither what this machine is for nor which
            // other machines hold the data is that.
            StatementKind::DefineNode { .. }
            | StatementKind::DefineReplica { .. }
            | StatementKind::DropReplica { .. } => Self::OPERATE_STORE,
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
            // **`{manage, write}`, which is the second correction the ladder
            // hid.** A consumer writes records on the declarer's behalf, later,
            // with nobody present. Demanding only the administrative half makes
            // it a privilege-escalation channel: declare a consumer, and records
            // appear in a table the declarer could not have written to
            // themselves. The rule generalises — *a statement that declares a
            // thing which will later act demands every authority that thing will
            // exercise* — and this is the store's only instance of it.
            StatementKind::DefineConsumer { .. } | StatementKind::DropConsumer { .. } => {
                Self::MANAGE_WRITE
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
            } => Self::MANAGE,
            // The audit trail, and it must be named here rather than left to
            // the arm below. That arm is a catch-all over `Info` alone, so a new
            // subject joins it silently — and this is the subject where that is
            // worst: read is what every signed-in caller has, and the trail is
            // every vault read in every tenancy of the store. Named, it demands
            // `govern` over the store itself.
            StatementKind::Info {
                subject: InfoSubject::Audit(_),
            } => Self::GOVERN_STORE,
            // The other four are reads of the catalog, and what they report is
            // narrowed to what the caller could have found out anyway.
            StatementKind::Info { .. } => Self::READ,
            // A namespace is a **sibling** of every other namespace, and nothing
            // contains a sibling — so declaring one is not shaping the data you
            // own, it is adding to the store's top-level list. Left with the
            // `Write` block below, an `editor` of one database could do it: a
            // caller with authority over no tenancy at all, adding one.
            //
            // `DROP NAMESPACE` sits here for the same reason and not one step
            // lower: removing from the store's top-level list is the same
            // authority as adding to it, and an `editor` of one database
            // undeclaring a namespace they hold no tenancy in is exactly what
            // this level exists to refuse.
            StatementKind::DefineNamespace { .. } | StatementKind::DropNamespace { .. } => {
                Self::MANAGE_STORE
            }
            // **The fifteen that were `Write`, and this line is the owner's
            // fourth rule.** Creating and dropping the containers records live
            // in is `manage`, and changing the records is `write`, and neither
            // implies the other in either direction. Under one class they were
            // the same permission: a caller allowed to write a namespace's
            // records could create and drop databases in it, and no ordering
            // over roles could have said otherwise.
            //
            // A drop keeps the same authority as the declaration it undoes
            // rather than a higher one: the person who may shape a structure is
            // the person who may unshape it, and a level that differed would
            // leave a tenant able to create what they then need somebody else to
            // remove.
            StatementKind::DefineDatabase { .. }
            | StatementKind::DropDatabase { .. }
            | StatementKind::DefineTable { .. }
            | StatementKind::DropTable { .. }
            | StatementKind::DefineSpace { .. }
            | StatementKind::DefineBucket { .. }
            | StatementKind::DefineCollection { .. }
            | StatementKind::DefineVector { .. }
            | StatementKind::DropVector { .. }
            | StatementKind::DefineGeo { .. }
            | StatementKind::DropGeo { .. }
            | StatementKind::DefineVault { .. }
            | StatementKind::DropVault { .. }
            // Declaring a queue is declaring a table, so it sits with the rest
            // of the structure statements. `CLAIM` and `RELEASE` are not here:
            // they write records, and they are classified below with the other
            // statements that do.
            | StatementKind::DefineQueue { .. }
            | StatementKind::DropQueue { .. }
            | StatementKind::DefineSeries { .. }
            | StatementKind::DropSeries { .. }
            // Declaring a view is declaring a table — it takes a name in the
            // table namespace and writes a catalog entry — so it sits with the
            // structure statements even though nothing is stored under it.
            | StatementKind::DefineView { .. }
            | StatementKind::DropView { .. }
            | StatementKind::DefineGraph { .. }
            | StatementKind::DropGraph { .. }
            | StatementKind::DefineEdge { .. }
            | StatementKind::DropEdge { .. }
            | StatementKind::DefineIndex { .. }
            | StatementKind::DropIndex { .. }
            | StatementKind::RebuildIndex { .. }
            | StatementKind::CheckTable { .. }
            | StatementKind::DefineField { .. }
            | StatementKind::DropField { .. }
            | StatementKind::AlterTable { .. }
            | StatementKind::AlterField { .. }
            | StatementKind::DefineAnalyzer { .. }
            | StatementKind::DropAnalyzer { .. } => Self::MANAGE,
            // **The writes that are also reads**, and the classification is
            // measured rather than reasoned. `CREATE t:1` on an existing
            // record refuses with *record 1 already exists* and `UPDATE t:99` on
            // an absent one with *no record 99* — each is defined by a claim
            // about prior state, so each answers a question about it.
            // `DELETE … WHERE` reads records to decide which to remove, and a
            // condition that selects is a read whether or not rows come back.
            //
            // The cost is real and is stated rather than discovered: an
            // append-only writer that wants `CREATE`'s duplicate refusal must
            // also hold `read`. Letting `write` alone run it was considered and
            // refused — an oracle over record ids is exactly what turns a
            // write-only integration credential into an enumeration tool, and
            // `UPSERT` is the supported answer that needs nothing extra.
            // Only the **addressed** create is an oracle. The caller picked the
            // identity, so the refusal answers a question they asked about
            // prior state — which is what the paragraph above is about.
            StatementKind::Create {
                target: CreateTarget::Named(_),
                ..
            }
            | StatementKind::Update { .. }
            // Both recipient statements refuse on a claim about prior state —
            // *that name is already a recipient*, *that name is not one* — so
            // each answers a question about the set before it changed it, which
            // is the property this class is measured on rather than reasoned
            // about. The refusals are deliberate (a silent revocation is the
            // worst answer `REMOVE RECIPIENT` could give), and the reading half
            // is what they cost.
            | StatementKind::AddRecipient { .. }
            | StatementKind::RemoveRecipient { .. }
            | StatementKind::DeleteWhere { .. }
            // `DELETE FROM t:a..b` reads no record — it removes by position —
            // and is still in this class, because the class is measured on what
            // the answer discloses rather than on what the statement reads. It
            // answers `removed n`, and `n` is exactly how many records existed
            // in a span the **caller** chose. That is an enumeration oracle over
            // identity ranges and a binary search away from naming them, which
            // is the same argument the addressed `CREATE` above is classified
            // by.
            | StatementKind::DeleteSpan { .. }
            // A claim writes the hold **and** answers with the record, so it
            // discloses everything a `SELECT` of the same records would. The
            // class is measured on what the answer discloses, which is the same
            // argument that puts a span delete on this line.
            | StatementKind::Claim { .. }
            // The targeted form discloses the same record by the same answer, so it
            // takes the same class — named here rather than left to a catch-all,
            // which is the mistake the release's own comment below records.
            | StatementKind::ClaimRecord { .. }
            // A release clears a hold and answers nothing about the record, so
            // it is the write half alone — and it is listed here rather than as
            // a write-only statement because the class it would otherwise take
            // is decided by a catch-all arm, and a catch-all over a statement
            // family is how the next member added gets mis-permissioned.
            | StatementKind::Release { .. } => Self::READ_WRITE,
            // **The six that are measurably silent about prior state.** Every
            // one of them answers `ok` against an absent or conflicting record,
            // so a holder of `write` alone can run them and learn nothing —
            // which is what makes a pure ingestion identity a real thing here
            // rather than a theoretical one.
            StatementKind::Upsert { .. }
            // `CREATE users = { … }` is the single-record shape of the same
            // thing `INSERT` is below: the store chose the identity, so the
            // caller cannot name the one that would conflict and cannot name
            // the next one either. Its only refusal is a store whose randomness
            // or counter is broken, which tells an attacker nothing.
            | StatementKind::Create {
                target: CreateTarget::Generated(_),
                ..
            }
            // `INSERT` writes at an identity the **store** chose, so there is no
            // claim about prior state a caller could have made and no answer
            // they could read one from: they cannot name the identity that would
            // conflict, and cannot name the next one either. That is what
            // separates it from `CREATE` two arms above, whose refusal is an
            // oracle precisely because the caller picked the identity it asks
            // about. The append-only ingestion credential this arm exists for is
            // exactly the caller `INSERT` was added for.
            | StatementKind::Insert { .. }
            | StatementKind::Delete { .. }
            | StatementKind::Relate { .. }
            | StatementKind::DeleteEdge { .. }
            | StatementKind::Set { .. }
            | StatementKind::Del { .. }
            | StatementKind::Put { .. } => Self::WRITE,
        }
    }

    /// The first demanded kind this user holds nowhere at all.
    ///
    /// # The role ladder used to answer this, and could not
    ///
    /// It asked whether a rank was high enough, so it could only say *more* or
    /// *less* — and the rule this store had to express is that writing records
    /// and managing containers are neither. The question now is whether the
    /// user's stored set contains the kind, and a set answers it directly.
    ///
    /// **This is the coarse half of two.** It asks whether the authority is held
    /// *anywhere*, which is what makes the refusal arrive with a useful message
    /// before any name is resolved. Whether it is held at the container the
    /// statement actually reaches is [`crate::Session::within_authority`], and
    /// neither is sufficient alone: this one would let a holder of `manage` over
    /// one database manage a sibling, and that one passes vacuously over a
    /// statement that names no table.
    fn unheld_by(self, user: &UserDefinition) -> Option<Kind> {
        self.kinds
            .iter()
            .copied()
            .find(|kind| !user.authorities.iter().any(|held| held.kind == *kind))
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
        scope: Option<&ReachRef>,
        role: &UserGrant,
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

    /// The reach a statement named, resolved against the catalog.
    ///
    /// The database spelling goes through `tenancy_of`, which is where the
    /// existing reach check for `DEFINE USER … ON prod.orders` lives; the
    /// namespace spelling checks the same thing one level up, because a
    /// namespace nobody may reach is not a namespace they may grant in.
    fn reach_of(&self, transaction: &mut Transaction<'_>, named: &ReachRef) -> Result<Reach> {
        match named {
            ReachRef::Store => Ok(Reach::Store),
            ReachRef::Namespace(name) => {
                let namespace = Catalog::new(transaction)
                    .namespace_id(&name.text)?
                    .ok_or_else(|| Error::Unknown {
                        entity: "namespace",
                        name: name.text.clone(),
                        span: name.span,
                    })?;
                Ok(Reach::Namespace(namespace))
            }
            ReachRef::Database(table) => {
                let context = self.tenancy_of(transaction, table)?;
                Ok(Reach::Database(context.namespace, context.database))
            }
        }
    }

    /// The set a `DEFINE USER` declares, however it spelled it.
    ///
    /// A role is a name for a set, so both spellings arrive here as one — which
    /// is what stops the two from meaning different things in different places.
    fn held_from(&self, grant: &UserGrant, reach: Reach, span: Span) -> Result<Held> {
        match grant {
            UserGrant::Role(named) => {
                let Some(role) = Role::parse(&named.text) else {
                    return Err(Error::NoSuchRole {
                        name: named.text.clone(),
                        span,
                    });
                };
                Ok(Held::from_role(role, reach))
            }
            UserGrant::Authorities(kinds) => {
                let mut held = Held::nothing();
                for named in kinds {
                    held.add(Authority::new(kind_named(named)?, reach));
                }
                Ok(held)
            }
        }
    }

    /// `GRANT manage ON NAMESPACE prod TO ada` — add to what a user holds.
    ///
    /// Adding rather than replacing, because a user reaches more than one place
    /// and a grant names one of them: replacing would make every grant a silent
    /// revocation of every other.
    pub(crate) fn grant_authority(
        &self,
        transaction: &mut Transaction<'_>,
        kinds: &[Name],
        reach: &ReachRef,
        user: &Name,
        span: Span,
    ) -> Result<Outcome> {
        // Kinds first: what the statement says is wrong with *itself* is
        // answered before anything about the store is looked up, so a mistyped
        // kind reads as a mistyped kind rather than as an unknown user.
        let kinds = kinds.iter().map(kind_named).collect::<Result<Vec<_>>>()?;
        let reach = self.reach_of(transaction, reach)?;
        self.may_hand_out(&kinds, reach, span)?;
        let mut found = self.user_to_change(transaction, user, span)?;
        // And the reach has to meet the subject's own `ON` somewhere, or the
        // grant would be stored and never usable: a declared tenancy is a second
        // confinement asked before the held set, so `GRANT read ON NAMESPACE
        // staging TO nina` — nina being of `prod` — used to return `ok` and do
        // nothing at all. Refused rather than accepted-and-inert because the
        // operator's only evidence that a grant landed is the statement not
        // complaining, and because the escalation `WiderThanYou` refuses at
        // declaration would otherwise simply move here.
        //
        // **Overlap in either direction, not containment in one.** A reach
        // inside her tenancy is usable there, and so is a reach that *contains*
        // it — `read ON STORE` granted to a user of `prod` is not inert, because
        // containment runs downward and `prod` is inside the store. Only two
        // tenancies that miss each other entirely produce a holding nothing can
        // ever consult.
        let (namespace, database) = reach.parts();
        let overlaps = within(found.namespace, found.database, namespace, database)
            || within(namespace, database, found.namespace, found.database);
        if !overlaps {
            return Err(Error::OutsideTheirTenancy {
                user: user.text.clone(),
                span,
            });
        }
        for kind in kinds {
            found.authorities.add(Authority::new(kind, reach));
        }
        self.rewrite_authorities(transaction, found);
        Ok(Outcome::Done)
    }

    /// Refuse a grant that would reach past the caller's own holdings.
    ///
    /// # The one statement that can escalate, and the two questions that stop it
    ///
    /// Every other statement is bounded by what the caller may do *now*. A grant
    /// is bounded by what somebody may do *later*, which is why it is the only
    /// place in the store where a mistake compounds: mint an identity above your
    /// own and every other check becomes decorative, because the way past them
    /// all is to be somebody else.
    ///
    /// So two questions, and they are the same rule read from both ends:
    ///
    /// 1. **Do you govern there?** Handing out authority in a namespace is an
    ///    act *in* that namespace, and `govern` is the kind that covers it.
    /// 2. **Do you hold what you are handing out?** Containment does the work:
    ///    a holder of `manage` over `prod` passes for `prod.shop` and fails for
    ///    the store, because [`Reach::contains`] runs downward and only downward.
    ///
    /// Until this existed the statement demanded `govern` at the **store**,
    /// which was safe and too strict by exactly the case the model was asked
    /// for: a namespace authority could not hand out authority inside their own
    /// namespace. The under-grant was deliberate and is now paid off — an
    /// under-grant is a support request, and an over-grant is unrecoverable.
    ///
    /// Anonymous callers are unbounded here, and that is the same rule that lets
    /// an empty store declare its first user: a store with nobody in it hides
    /// nothing from anybody, and a closed one refuses long before this.
    fn may_hand_out(&self, kinds: &[Kind], reach: Reach, span: Span) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        if !user.authorities.permits(Kind::Govern, reach) {
            return Err(Error::CannotHandOut {
                kind: Kind::Govern.name(),
                span,
            });
        }
        for kind in kinds {
            if !user.authorities.permits(*kind, reach) {
                return Err(Error::CannotHandOut {
                    kind: kind.name(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// `REVOKE manage ON NAMESPACE prod FROM ada` — take exactly what is named.
    ///
    /// Removing an authority the user does not hold is not an error: the
    /// statement asks for a user without it, and a user without it is what it
    /// leaves. That is `DROP USER`'s rule and it is the same rule, because a
    /// revocation that fails halfway through a list is worse than one that is
    /// idempotent.
    pub(crate) fn revoke_authority(
        &self,
        transaction: &mut Transaction<'_>,
        kinds: &[Name],
        reach: &ReachRef,
        user: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let kinds = kinds.iter().map(kind_named).collect::<Result<Vec<_>>>()?;
        let reach = self.reach_of(transaction, reach)?;
        // Only the first of the two questions. Taking an authority away can
        // never mint one, so holding what is being revoked is not required —
        // and requiring it would mean a namespace administrator could not clean
        // up a grant somebody above them made.
        self.may_hand_out(&[], reach, span)?;
        let mut found = self.user_to_change(transaction, user, span)?;
        for kind in kinds {
            found.authorities.remove(&Authority::new(kind, reach));
        }
        self.rewrite_authorities(transaction, found);
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

    /// Write a changed authority set back, with the role summary re-derived.
    ///
    /// Re-derived rather than carried, for `create_user`'s reason: a stored role
    /// left behind by a revocation would describe authorities the user no longer
    /// holds, and it is the field an older binary believes.
    /// A tenancy that is not a place summarises as no role at all, rather than
    /// leaving the old one behind. Nothing reachable through the language builds
    /// that record — `from_value` refuses it — but `UserDefinition` is a public
    /// struct with public fields, so the shape is constructible by a caller of
    /// this crate, and the two ways to be wrong here are not symmetric: a stale
    /// role describes authorities the user no longer holds, and it is the field
    /// an older binary believes.
    fn rewrite_authorities(&self, transaction: &mut Transaction<'_>, mut user: UserDefinition) {
        user.role = Reach::of(user.namespace, user.database)
            .and_then(|reach| user.authorities.role_within(reach));
        Catalog::new(transaction).update_user(&user);
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

/// The kind a word names, refused when it names none.
///
/// The refusal carries the whole set rather than only the rejection, because
/// there are five of them and a reader who mistyped one is a reader who does not
/// yet know which five.
fn kind_named(named: &Name) -> Result<Kind> {
    Kind::parse(&named.text).ok_or_else(|| Error::NoSuchAuthority {
        name: named.text.clone(),
        known: Kind::ALL
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>()
            .join(", "),
        span: named.span,
    })
}

/// Whether an expression reads this node's own identity anywhere inside it.
///
/// Reading `$node` is administering rather than reading (see [`Needs::of`]), and
/// an expression can carry that read down inside a group, a call argument or an
/// array. Answering the question shallowly would let the deeper spelling through
/// with a viewer's permission, which is the whole reason this walks.
fn holds_node_read(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Select(select) => matches!(select.from, tessari_ql::Source::Node),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => holds_node_read(inner),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            holds_node_read(condition)
                || holds_node_read(then)
                || otherwise.as_deref().is_some_and(holds_node_read)
        }
        ExprKind::Coalesce(left, right) => holds_node_read(left) || holds_node_read(right),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => holds_node_read(left) || holds_node_read(right),
        ExprKind::Fold { over, .. } => over.as_deref().is_some_and(holds_node_read),
        ExprKind::Call { arguments, .. } => arguments.iter().any(holds_node_read),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(holds_node_read),
        ExprKind::Object(fields) => fields.iter().any(|field| holds_node_read(&field.value)),
        ExprKind::Range(range) => holds_node_read(&range.start) || holds_node_read(&range.end),
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Path(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_) => false,
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
