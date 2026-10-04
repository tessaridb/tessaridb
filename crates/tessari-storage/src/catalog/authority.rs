//! What a user is allowed to do, and how far it reaches.
//!
//! # Why this is a pair and not a rank
//!
//! The rule this type exists to express is *"may write records here, may not
//! create or drop databases here"*. No ranking can hold it. Two authorities —
//! changing records, and managing the containers those records live in — have to
//! be independent in **both** directions, and in a total order they cannot be:
//! put managing above writing and every manager can write, put it below and
//! every writer can manage. There is no third position, so the defect is the
//! ordering itself and adding ranks is the one repair that provably cannot work.
//!
//! So an authority is a [`Kind`] **at** a [`Reach`], and a user holds a set of
//! them. Nothing is implied by anything else except containment down the
//! tenancy: an authority over the store covers each namespace in it, and one
//! over a namespace covers each database in it. `write` does not imply `manage`,
//! `operate` does not imply `read`, and `govern` does not imply either.
//!
//! # The top is not a special case
//!
//! There is no "is the root" branch. The highest authority is holding every kind
//! at [`Reach::Store`], which is an ordinary value that the ordinary rule
//! answers. A privileged branch in the evaluator is how a model acquires a path
//! the negative tests do not cover.
//!
//! # Where the table reach lives
//!
//! Not here. A grant over one table is [`super::grant::GrantDefinition`] and
//! stays exactly as it was — it already had the shape this type is giving the
//! levels above it, and rewriting it would be a change nobody asked for.

mod reach_codec;
use std::collections::{BTreeMap, BTreeSet};

use tessari_types::{DatabaseId, NamespaceId, ShardId, TableId, Value};

/// Re-exported so that `catalog::Reach` keeps resolving.
///
/// The shape moved a layer down when a log key had to carry it (a store key is
/// encoded beneath this crate, and a type cannot be named from underneath the
/// crate that defines it). Every caller in this tree reaches it through the
/// catalog, so it is re-exported here rather than re-pointed in twenty files.
pub use tessari_types::Reach;

use super::user::Role;
use crate::error::{Error, Result};
pub(crate) use reach_codec::ReachCodec;

/// The field naming which of the three shapes a stored reach carries.
const FIELD_REACH: &str = "reach";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_TABLE: &str = "table";
const FIELD_SHARD: &str = "shard";

const REACH_STORE: &str = "store";
const REACH_NAMESPACE: &str = "namespace";
const REACH_DATABASE: &str = "database";
const REACH_SHARD: &str = "shard";

const ENTITY: &str = "authority";

/// What an authority permits.
///
/// Six, and each names a different thing that can be taken away on its own.
/// The set is closed: a new kind is a new thing a store can refuse, which is a
/// decision rather than an addition. [`Kind::Replicate`] was the sixth and is
/// the worked example of that sentence — it was added because taking the log is
/// not reading the records and not operating the node, and neither of those two
/// could be stretched to mean it without granting more than anybody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Read records.
    Read,
    /// Write records.
    Write,
    /// Create and drop the container's children — databases in a namespace,
    /// tables in a database — and define structure on them.
    Manage,
    /// Declare users and move authority around.
    Govern,
    /// Topology, replicas and the backup file: running the thing rather than
    /// using it.
    Operate,
    /// Take the log itself: subscribe as a peer and receive the store's
    /// mutations as they were written.
    ///
    /// Separate from [`Self::Read`] because the log is not the records. It
    /// carries the system tenancy as well — the definitions, and the users,
    /// credentials and grants that travel to every subscriber — so a reader who
    /// could subscribe would hold every credential hash in the store.
    ///
    /// Separate from [`Self::Operate`] because receiving the log and reading
    /// what the cluster is doing are two different permissions, and a node
    /// should be able to hold either without the other: a replica that is not
    /// an operator, an observer that is not a replica.
    Replicate,
}

impl Kind {
    /// Every kind, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::Read,
        Self::Write,
        Self::Manage,
        Self::Govern,
        Self::Operate,
        Self::Replicate,
    ];

    /// How the kind is written.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Manage => "manage",
            Self::Govern => "govern",
            Self::Operate => "operate",
            Self::Replicate => "replicate",
        }
    }

    /// Read one back from how it is written.
    ///
    /// Case-sensitive, like every other name in this language.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|held| held.name() == text)
    }

    /// Whether this kind can be held at `reach` at all.
    ///
    /// Every kind but one can be held anywhere, because every kind but one is
    /// about the container it names. [`Self::Replicate`] is not: it is the right
    /// to take the log, and the log carries the **system tenancy** — the
    /// definitions, and the users, credentials and grants that travel to every
    /// subscriber. A holder of it sees what the store *is*, not what one
    /// namespace contains, so a namespace is not a size it comes in.
    ///
    /// # This is a refusal, not a narrowing
    ///
    /// The alternative was to keep the grant sayable and defend the credentials
    /// in the stream's filter instead. That fails on its own terms: the identity
    /// class is replicated **everywhere, always** — one set of users per cluster
    /// — so there is no filter left to hide them behind. Whoever may subscribe
    /// at all sees every credential hash in the store, and the only principal
    /// for whom that discloses nothing new is one already entitled to the whole
    /// store.
    ///
    /// # The selective stream is untouched, because two things were riding here
    ///
    /// A subscription has a gate and a filter, and they were both spelled with
    /// this kind. The gate is who may open one; the filter is `over`, the reach
    /// the log is narrowed to. Only the gate moves. A store-reach holder may
    /// still subscribe over one namespace and receive only that tenancy —
    /// [`Reach::contains`] runs downward, so the store answers for a namespace
    /// inside it. What the narrowing stops being is a right a tenant holds, and
    /// what it becomes is an arrangement the cluster makes.
    #[must_use]
    pub const fn may_be_held_at(self, reach: Reach) -> bool {
        match self {
            Self::Replicate => matches!(reach, Reach::Store),
            // Written out rather than left to a catch-all, so a seventh kind
            // fails to compile here instead of silently answering `true`. The
            // set is closed and a new member is a decision (see the type's own
            // doc); this is one of the places that decision has to be made.
            //
            // A shard is a range for leading, logging and subscribing, and never
            // one an authority comes in (G031, ADR-0080): a grant over part of a
            // table's identities is a row-level permission this model does not
            // have, and one that slipped in through a reach would be it anyway.
            Self::Read | Self::Write | Self::Manage | Self::Govern | Self::Operate => {
                !matches!(reach, Reach::Shard(..))
            }
        }
    }
}

/// One authority: a kind, at a reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Authority {
    /// What it permits.
    pub kind: Kind,
    /// How far it goes.
    pub reach: Reach,
}

impl Authority {
    /// One authority.
    #[must_use]
    pub const fn new(kind: Kind, reach: Reach) -> Self {
        Self { kind, reach }
    }

    /// Whether holding this answers a demand for `kind` at `reach`.
    ///
    /// # An authority held where its kind cannot be held answers nothing
    ///
    /// The middle question looks redundant beside the two statements that refuse
    /// such a grant, and it is the one that matters most: the refusals govern
    /// what can be **said from now on**, and this governs what is **already
    /// written down**. Every namespace owner declared before [`Kind`] gained its
    /// reach rule holds `replicate` over their namespace in the catalog this
    /// moment, put there by [`Held::every_kind_at`] rather than by anybody's
    /// statement. Guarding only the statements would leave every one of those
    /// rows live and the guard decorative — an upgrade that closes a door while
    /// the ones already open stay open.
    ///
    /// Asked here rather than at each caller because this is the single
    /// predicate every authorization read funnels through, and a rule applied at
    /// call sites is a rule the next call site inherits nothing of.
    #[must_use]
    pub fn permits(self, kind: Kind, reach: Reach) -> bool {
        self.kind == kind && self.kind.may_be_held_at(self.reach) && self.reach.contains(reach)
    }
}

/// The set of authorities a user holds.
///
/// A set rather than a rank, and ordered so that two users holding the same
/// authorities serialise identically — a catalog record whose bytes depend on
/// insertion order is one that compares unequal to itself after a round trip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held(BTreeSet<Authority>);

impl Held {
    /// Hold nothing.
    ///
    /// The default, and it refuses everything — which is what makes "no
    /// authority matched" the structural answer rather than a rule somebody
    /// remembered to write.
    #[must_use]
    pub fn nothing() -> Self {
        Self(BTreeSet::new())
    }

    /// Hold exactly these.
    #[must_use]
    pub fn of(authorities: impl IntoIterator<Item = Authority>) -> Self {
        Self(authorities.into_iter().collect())
    }

    /// Every kind that can be held at one reach — the shape an owner of
    /// something has.
    ///
    /// "Every kind" is filtered rather than literal, and the filter is the whole
    /// point of putting it here. This is not a statement anybody types: it is
    /// the bundle [`Self::from_role`] hands an owner, so a kind that must not
    /// reach a namespace would arrive at every namespace owner in the store by a
    /// road with no author. One filter, and the role follows it for free.
    #[must_use]
    pub fn every_kind_at(reach: Reach) -> Self {
        Self::of(
            Kind::ALL
                .iter()
                .filter(|kind| kind.may_be_held_at(reach))
                .map(|kind| Authority::new(*kind, reach)),
        )
    }

    /// Add one.
    pub fn add(&mut self, authority: Authority) {
        self.0.insert(authority);
    }

    /// Take one away.
    ///
    /// Only the authority named, never one that contains it: revoking `write` at
    /// a database does not silently narrow a `write` held over the whole store,
    /// because a revocation that rewrites a *different* grant is one nobody can
    /// predict the effect of.
    pub fn remove(&mut self, authority: &Authority) -> bool {
        self.0.remove(authority)
    }

    /// Whether anything held answers a demand for `kind` at `reach`.
    #[must_use]
    pub fn permits(&self, kind: Kind, reach: Reach) -> bool {
        self.0.iter().any(|held| held.permits(kind, reach))
    }

    /// Whether anything at all is held at `reach` or above it.
    ///
    /// The question `USE` asks: selecting a namespace should not require the
    /// authority to *read* it — that would stop a govern-only administrator
    /// selecting the namespace they administer — but requiring nothing at all
    /// would make the statement an existence oracle over every namespace.
    #[must_use]
    pub fn anything_at(&self, reach: Reach) -> bool {
        self.0.iter().any(|held| held.reach.contains(reach))
    }

    /// Whether this container is on the path between the store and something
    /// held — at it, above it, or inside it.
    ///
    /// # Why `USE` needs both directions and [`Self::anything_at`] does not
    ///
    /// Containment runs downward: a namespace contains its databases and a
    /// database contains nothing above it. That is right for a *demand*, which
    /// must be answered **at** the container the statement reaches.
    ///
    /// Selecting is not a demand. A user scoped to `prod.shop` has to say
    /// `USE NAMESPACE prod` before they can say `USE DATABASE shop`, so the
    /// namespace is a step on the way to the only thing they hold — and asking
    /// downward containment alone refuses them their own database. Measured, not
    /// predicted: every database-scoped user in the suite was locked out of the
    /// store by exactly that.
    ///
    /// The upward direction is not a widening of authority. It permits *naming*
    /// a container, and every statement that then acts inside it is asked the
    /// ordinary question at the ordinary reach. What it rules out is the case it
    /// exists for: naming a container the caller has no business in at all, and
    /// learning from the refusal whether it is there.
    #[must_use]
    pub fn touches(&self, reach: Reach) -> bool {
        self.0
            .iter()
            .any(|held| held.reach.contains(reach) || reach.contains(held.reach))
    }

    /// What is held, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = Authority> + '_ {
        self.0.iter().copied()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The authorities a declared role stood for, at the reach it applied to.
    ///
    /// # This mapping keeps a bundle the new rule forbids, and that is correct
    ///
    /// `Editor` becomes `{read, write, manage}` — precisely the combination the
    /// separation exists to make refusable. It is not a mistake to be tidied.
    /// The rule governs what can now be **said**; every editor that already
    /// exists was declared under a promise that they may define structure, and
    /// narrowing them on upgrade is an outage delivered as a migration. What the
    /// change buys is that nobody has to accept the bundle any more.
    ///
    /// # An owner gained [`Kind::Replicate`] when the sixth kind arrived
    ///
    /// The mirror of the paragraph above — widening an existing principal on
    /// upgrade is an escalation delivered as a migration — so it was decided
    /// rather than inherited from [`Self::every_kind_at`].
    ///
    /// It stands, for two reasons and a residue. An owner **at the store**
    /// already holds `read` and `operate` there, which together are `BACKUP`:
    /// every record and every definition, in one file. The log discloses nothing
    /// to them that they could not already take, so this widens what they may
    /// *do* and not what they may *see*. And excluding it would make the kind
    /// unreachable rather than merely explicit: nobody hands out what they do
    /// not hold, so a store whose users were all declared by role could never
    /// grant `replicate` to anybody, including to itself.
    ///
    /// The residue was an owner of one **namespace**, who gained an authority
    /// that authorised nothing while the only subscription that could be served
    /// was the whole store's — and which must not, once a selective stream
    /// exists, carry the identity class with it. **That residue is now paid.**
    /// [`Kind::may_be_held_at`] makes `replicate` a thing held over the store or
    /// not at all, [`Self::every_kind_at`] filters by it, and this mapping
    /// inherits the narrowing without an edit: an owner at the store still holds
    /// every kind, an owner of a namespace no longer holds that one.
    ///
    /// Note which direction that moved. Narrowing a role on upgrade is the
    /// outage this doc warns about two paragraphs above, and this is one — a
    /// namespace owner loses an authority they were declared with. It is taken
    /// anyway because what they lose is an authority that never authorised
    /// anything, and what it buys is that the identity class can travel to every
    /// follower without a tenant being able to ask for it.
    #[must_use]
    pub fn from_role(role: Role, reach: Reach) -> Self {
        match role {
            Role::Viewer => Self::of([Authority::new(Kind::Read, reach)]),
            Role::Editor => Self::of([
                Authority::new(Kind::Read, reach),
                Authority::new(Kind::Write, reach),
                Authority::new(Kind::Manage, reach),
            ]),
            Role::Owner => Self::every_kind_at(reach),
        }
    }

    /// The widest role whose bundle this set contains, if any role does.
    ///
    /// The inverse of [`Self::from_role`], and it is a *summary* rather than a
    /// round trip: a set is written to the catalog alongside the role it can be
    /// described as, so that a binary predating the set field reads a role and
    /// under-grants rather than misreading. Widest-that-fits, never
    /// nearest — a role wider than the set would grant an older binary
    /// something the user does not hold, which is the one direction this must
    /// never fail in.
    ///
    /// `None` is the honest answer for the sets a ladder could never express —
    /// `manage` at a namespace without `read` is the case the whole model
    /// exists for. It is written as an **absent** role, which an older binary
    /// refuses to read rather than guessing at.
    #[must_use]
    pub fn role_within(&self, reach: Reach) -> Option<Role> {
        Role::ALL
            .iter()
            .rev()
            .copied()
            .find(|role| Self::from_role(*role, reach).0.is_subset(&self.0))
    }

    /// The set, as it is written to the catalog.
    ///
    /// One string per authority — `"read@store"`, `"write@3"`, `"manage@3.7"` —
    /// rather than a nested object per entry. The set is small, the grammar is
    /// closed, and a flat list of short strings is a form a person reading a
    /// catalog dump can check by eye.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|held| Value::from(written(*held).as_str()))
                .collect(),
        )
    }

    /// Read the set back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the value is not an array of
    /// authorities this binary knows. An unknown kind is corruption rather than
    /// a bad request — it was written by something that knew a kind this binary
    /// does not, and guessing would grant or refuse the wrong thing.
    pub fn from_value(value: &Value) -> Result<Self> {
        let malformed = |found: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field: "authorities",
            found,
        };
        let Value::Array(entries) = value else {
            return Err(malformed(value.type_name()));
        };
        let mut held = BTreeSet::new();
        for entry in entries {
            let Value::String(text) = entry else {
                return Err(malformed(entry.type_name()));
            };
            held.insert(read(text).ok_or_else(|| malformed("an unknown authority"))?);
        }
        Ok(Self(held))
    }
}

/// One authority, as it is written.
fn written(authority: Authority) -> String {
    let mut text = authority.kind.name().to_owned();
    text.push('@');
    match authority.reach {
        Reach::Store => text.push_str("store"),
        Reach::Namespace(namespace) => text.push_str(&namespace.get().to_string()),
        Reach::Database(namespace, database) => {
            text.push_str(&namespace.get().to_string());
            text.push('.');
            text.push_str(&database.get().to_string());
        }
        // Never held (`Kind::may_be_held_at`), and written faithfully anyway: a
        // row this could not read back would be a stored fact with no reader,
        // and one that read back as a wider reach would be a grant nobody made.
        Reach::Shard(namespace, database, table, shard) => {
            text.push_str(&format!(
                "{}.{}.{}.{}",
                namespace.get(),
                database.get(),
                table.get(),
                shard.get()
            ));
        }
    }
    text
}

/// One authority, read back from how it is written.
fn read(text: &str) -> Option<Authority> {
    let (kind, reach) = text.split_once('@')?;
    let kind = Kind::parse(kind)?;
    let reach = match reach {
        "store" => Reach::Store,
        rest => {
            let parts: Vec<&str> = rest.split('.').collect();
            match parts.as_slice() {
                [namespace] => Reach::Namespace(NamespaceId::new(namespace.parse().ok()?)),
                [namespace, database] => Reach::Database(
                    NamespaceId::new(namespace.parse().ok()?),
                    DatabaseId::new(database.parse().ok()?),
                ),
                [namespace, database, table, shard] => Reach::Shard(
                    NamespaceId::new(namespace.parse().ok()?),
                    DatabaseId::new(database.parse().ok()?),
                    TableId::new(table.parse().ok()?),
                    ShardId::new(shard.parse().ok().filter(|shard| *shard != 0)?),
                ),
                _ => return None,
            }
        }
    };
    Some(Authority::new(kind, reach))
}

/// The field a user record carries its authorities in.
///
/// Absent on every record written before this existed, which is what
/// [`Held::from_role`] is for.
pub(super) const FIELD_AUTHORITIES: &str = "authorities";

/// The authorities a stored user record stands for.
///
/// Reads the explicit set when the record carries one, and derives it from the
/// role when it does not. Both forms are current: a role is still written
/// whenever one summarises the set, so a binary that predates this can still
/// read a record this one wrote.
///
/// # Errors
///
/// Returns [`Error::CatalogMalformed`] when the field is present and is not an
/// authority set, and when a record carries **neither** — a user with no role
/// and no set says nothing about what they may do, and the safe reading of
/// nothing is not "nothing is permitted" but "this record is not intelligible".
pub(super) fn held_of(
    fields: &BTreeMap<String, Value>,
    role: Option<Role>,
    reach: Reach,
) -> Result<Held> {
    match (fields.get(FIELD_AUTHORITIES), role) {
        (Some(value), _) => Held::from_value(value),
        (None, Some(role)) => Ok(Held::from_role(role, reach)),
        (None, None) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_AUTHORITIES,
            found: "neither a role nor an authority set",
        }),
    }
}

#[cfg(test)]
mod tests;
