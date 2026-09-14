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

use std::collections::{BTreeMap, BTreeSet};

use tessari_types::{DatabaseId, NamespaceId, Number, Value};

/// Re-exported so that `catalog::Reach` keeps resolving.
///
/// The shape moved a layer down when a log key had to carry it (a store key is
/// encoded beneath this crate, and a type cannot be named from underneath the
/// crate that defines it). Every caller in this tree reaches it through the
/// catalog, so it is re-exported here rather than re-pointed in twenty files.
pub use tessari_types::Reach;

use super::definition::number;
use super::user::Role;
use crate::error::{Error, Result};

/// The field naming which of the three shapes a stored reach carries.
const FIELD_REACH: &str = "reach";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";

const REACH_STORE: &str = "store";
const REACH_NAMESPACE: &str = "namespace";
const REACH_DATABASE: &str = "database";

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
            Self::Read | Self::Write | Self::Manage | Self::Govern | Self::Operate => true,
        }
    }
}

/// The catalog's encoding of a [`Reach`].
///
/// A trait rather than inherent methods because the shape itself lives a layer
/// below — a log key carries a reach, and store keys are encoded under this
/// crate — while this encoding raises **this** crate's malformed-catalog error
/// and belongs with the catalog that reads it. The method syntax at the call
/// sites is unchanged.
pub(crate) trait ReachCodec: Sized {
    /// This reach, as a catalog record stores it.
    fn to_value(self) -> Value;

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the stored value is not a reach.
    fn from_value(value: &Value, entity: &'static str, field: &'static str) -> Result<Reach>;
}

impl ReachCodec for Reach {
    /// This reach, as a catalog record stores it.
    ///
    /// Tagged rather than inferred from which ids are present, because
    /// [`Reach::Store`] carries no ids at all and an object with no ids would
    /// then be the same bytes as an object somebody wrote wrong. The tag makes
    /// the whole store a thing that was said rather than a thing the reader
    /// assumed.
    ///
    /// # Why the codec lives beside the type and not beside its first caller
    ///
    /// It has two callers now — a peer's subscription and a leadership's range —
    /// and a reach that encoded one way in one row and another way in the other
    /// would be two on-disk spellings of one type. Two readings of the same
    /// bytes is a thing that can disagree with itself, which is the reason the
    /// log record carries no mutation count either.
    fn to_value(self) -> Value {
        let (namespace, database) = self.parts();
        let mut fields = BTreeMap::from([(
            FIELD_REACH.to_owned(),
            Value::from(match self {
                Reach::Store => REACH_STORE,
                Reach::Namespace(_) => REACH_NAMESPACE,
                Reach::Database(_, _) => REACH_DATABASE,
            }),
        )]);
        if let Some(namespace) = namespace {
            fields.insert(FIELD_NAMESPACE.to_owned(), number(namespace.get()));
        }
        if let Some(database) = database {
            fields.insert(FIELD_DATABASE.to_owned(), number(database.get()));
        }
        Value::Object(fields)
    }

    /// Read a reach back from the value [`Self::to_value`] wrote.
    ///
    /// `entity` and `field` are carried so the refusal names the row the caller
    /// was reading rather than this type: a malformed reach is a defect in some
    /// definition, and a reader told only *reach* has to guess which one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the value is not an object, the
    /// tag is missing or unknown, or a tag's ids are absent or out of range.
    fn from_value(value: &Value, entity: &'static str, field: &'static str) -> Result<Reach> {
        let malformed = || Error::CatalogMalformed {
            entity,
            field,
            found: "reach",
        };
        let Value::Object(inner) = value else {
            return Err(Error::CatalogMalformed {
                entity,
                field,
                found: value.type_name(),
            });
        };
        let Some(Value::String(tag)) = inner.get(FIELD_REACH) else {
            return Err(malformed());
        };
        let id = |field: &'static str| -> Option<u32> {
            match inner.get(field) {
                Some(Value::Number(Number::Integer(raw))) => u32::try_from(*raw).ok(),
                _ => None,
            }
        };
        match tag.as_str() {
            REACH_STORE => Ok(Reach::Store),
            REACH_NAMESPACE => id(FIELD_NAMESPACE)
                .map(|namespace| Reach::Namespace(NamespaceId::new(namespace)))
                .ok_or_else(malformed),
            REACH_DATABASE => match (id(FIELD_NAMESPACE), id(FIELD_DATABASE)) {
                (Some(namespace), Some(database)) => Ok(Reach::Database(
                    NamespaceId::new(namespace),
                    DatabaseId::new(database),
                )),
                _ => Err(malformed()),
            },
            _ => Err(malformed()),
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
    }
    text
}

/// One authority, read back from how it is written.
fn read(text: &str) -> Option<Authority> {
    let (kind, reach) = text.split_once('@')?;
    let kind = Kind::parse(kind)?;
    let reach = match reach {
        "store" => Reach::Store,
        rest => match rest.split_once('.') {
            None => Reach::Namespace(NamespaceId::new(rest.parse().ok()?)),
            Some((namespace, database)) => Reach::Database(
                NamespaceId::new(namespace.parse().ok()?),
                DatabaseId::new(database.parse().ok()?),
            ),
        },
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
mod tests {
    use super::*;

    const PROD: NamespaceId = NamespaceId::new(3);
    const OTHER: NamespaceId = NamespaceId::new(4);
    const LIBRARY: DatabaseId = DatabaseId::new(7);
    const ARCHIVE: DatabaseId = DatabaseId::new(8);

    #[test]
    fn the_rule_a_ladder_could_not_express_is_a_value_here() {
        // The whole reason this type exists: write without manage, at a
        // namespace. Unrepresentable in any total order over roles.
        let held = Held::of([
            Authority::new(Kind::Read, Reach::Namespace(PROD)),
            Authority::new(Kind::Write, Reach::Namespace(PROD)),
        ]);
        assert!(held.permits(Kind::Write, Reach::Database(PROD, LIBRARY)));
        assert!(
            !held.permits(Kind::Manage, Reach::Namespace(PROD)),
            "writing a namespace's records must not confer creating databases in it"
        );
        assert!(!held.permits(Kind::Manage, Reach::Database(PROD, LIBRARY)));
    }

    #[test]
    fn the_only_implication_is_downward_containment() {
        let store = Held::of([Authority::new(Kind::Read, Reach::Store)]);
        assert!(store.permits(Kind::Read, Reach::Namespace(PROD)));
        assert!(store.permits(Kind::Read, Reach::Database(PROD, LIBRARY)));

        let database = Held::of([Authority::new(Kind::Read, Reach::Database(PROD, LIBRARY))]);
        assert!(!database.permits(Kind::Read, Reach::Namespace(PROD)));
        assert!(!database.permits(Kind::Read, Reach::Store));
        assert!(!database.permits(Kind::Read, Reach::Database(PROD, ARCHIVE)));
    }

    #[test]
    fn no_kind_implies_another() {
        for holding in Kind::ALL {
            let held = Held::of([Authority::new(*holding, Reach::Store)]);
            for wanted in Kind::ALL {
                assert_eq!(
                    held.permits(*wanted, Reach::Store),
                    holding == wanted,
                    "{} must answer {} only for itself",
                    holding.name(),
                    wanted.name()
                );
            }
        }
    }

    #[test]
    fn a_namespace_does_not_reach_a_sibling() {
        let held = Held::of([Authority::new(Kind::Manage, Reach::Namespace(PROD))]);
        assert!(held.permits(Kind::Manage, Reach::Database(PROD, LIBRARY)));
        assert!(!held.permits(Kind::Manage, Reach::Namespace(OTHER)));
        assert!(!held.permits(Kind::Manage, Reach::Database(OTHER, LIBRARY)));
    }

    #[test]
    fn selecting_a_container_looks_both_up_and_down_while_a_demand_looks_only_down() {
        // The asymmetry that matters, and getting it backwards locks a
        // database-scoped user out of the namespace their database is in.
        let held = Held::of([Authority::new(Kind::Read, Reach::Database(PROD, LIBRARY))]);

        // A demand is answered at the container reached, and a database reach
        // contains nothing above it.
        assert!(!held.permits(Kind::Read, Reach::Namespace(PROD)));
        assert!(!held.anything_at(Reach::Namespace(PROD)));

        // Selecting looks both ways: `prod` is a step on the way to `prod.shop`.
        assert!(held.touches(Reach::Namespace(PROD)));
        assert!(held.touches(Reach::Database(PROD, LIBRARY)));
        assert!(held.touches(Reach::Store));

        // And it still refuses a container this holder has no business in, in
        // either direction — which is the oracle it exists to close.
        assert!(!held.touches(Reach::Namespace(OTHER)));
        assert!(!held.touches(Reach::Database(PROD, ARCHIVE)));
        assert!(!held.touches(Reach::Database(OTHER, LIBRARY)));
        assert!(!Held::nothing().touches(Reach::Store));
    }

    #[test]
    fn holding_nothing_refuses_everything() {
        let held = Held::nothing();
        for kind in Kind::ALL {
            for reach in [
                Reach::Store,
                Reach::Namespace(PROD),
                Reach::Database(PROD, LIBRARY),
            ] {
                assert!(!held.permits(*kind, reach));
            }
        }
        assert!(!held.anything_at(Reach::Store));
    }

    #[test]
    fn the_top_is_every_kind_at_the_store_and_not_a_special_case() {
        let held = Held::every_kind_at(Reach::Store);
        for kind in Kind::ALL {
            assert!(held.permits(*kind, Reach::Database(PROD, LIBRARY)));
        }
    }

    #[test]
    fn selecting_a_container_asks_for_any_authority_and_not_for_read() {
        // A govern-only administrator selects the namespace they administer.
        let held = Held::of([Authority::new(Kind::Govern, Reach::Namespace(PROD))]);
        assert!(held.anything_at(Reach::Namespace(PROD)));
        assert!(held.anything_at(Reach::Database(PROD, LIBRARY)));
        assert!(!held.permits(Kind::Read, Reach::Namespace(PROD)));
        assert!(!held.anything_at(Reach::Namespace(OTHER)));
    }

    #[test]
    fn every_authority_survives_the_round_trip() {
        let mut held = Held::nothing();
        for kind in Kind::ALL {
            held.add(Authority::new(*kind, Reach::Store));
            held.add(Authority::new(*kind, Reach::Namespace(PROD)));
            held.add(Authority::new(*kind, Reach::Database(PROD, LIBRARY)));
        }
        let written = held.to_value();
        assert_eq!(
            Held::from_value(&written).expect("the set this test just wrote"),
            held
        );
    }

    #[test]
    fn the_written_form_is_the_one_documented() {
        assert_eq!(
            written(Authority::new(Kind::Read, Reach::Store)),
            "read@store"
        );
        assert_eq!(
            written(Authority::new(Kind::Write, Reach::Namespace(PROD))),
            "write@3"
        );
        assert_eq!(
            written(Authority::new(Kind::Manage, Reach::Database(PROD, LIBRARY))),
            "manage@3.7"
        );
    }

    #[test]
    fn an_authority_this_binary_does_not_know_is_corruption_and_not_a_shrug() {
        let value = Value::Array(vec![Value::from("transcend@store")]);
        assert!(
            Held::from_value(&value).is_err(),
            "an unknown kind must refuse rather than be dropped from the set"
        );
        assert!(Held::from_value(&Value::from("read@store")).is_err());
        assert!(Held::from_value(&Value::Array(vec![Value::from("read")])).is_err());
        assert!(Held::from_value(&Value::Array(vec![Value::from("read@nowhere")])).is_err());
    }

    #[test]
    fn a_record_with_no_authorities_falls_back_to_its_role() {
        let fields = BTreeMap::new();
        let reach = Reach::Database(PROD, LIBRARY);

        let viewer = held_of(&fields, Some(Role::Viewer), reach).expect("a viewer");
        assert!(viewer.permits(Kind::Read, reach));
        assert!(!viewer.permits(Kind::Write, reach));

        // The bundle the new rule forbids, preserved on purpose: an editor was
        // declared under a promise that they may define structure.
        let editor = held_of(&fields, Some(Role::Editor), reach).expect("an editor");
        assert!(editor.permits(Kind::Read, reach));
        assert!(editor.permits(Kind::Write, reach));
        assert!(editor.permits(Kind::Manage, reach));
        assert!(!editor.permits(Kind::Govern, reach));
        assert!(!editor.permits(Kind::Operate, reach));

        let owner = held_of(&fields, Some(Role::Owner), reach).expect("an owner");
        for kind in Kind::ALL {
            assert_eq!(
                owner.permits(*kind, reach),
                kind.may_be_held_at(reach),
                "an owner holds every kind this reach can hold and only those: {}",
                kind.name()
            );
        }
        // Named as well as derived. The loop above compares the bundle against
        // the rule, so both being wrong the same way would pass it; this says
        // which kind the rule is about at a reach below the store.
        assert!(
            !owner.permits(Kind::Replicate, reach),
            "an owner of one database does not hold the store's log"
        );
    }

    /// The row an older binary already wrote, and the reason the rule is asked
    /// in `permits` rather than only at the two statements that refuse it.
    ///
    /// Constructed directly because there is no longer any way to say it: every
    /// road into the set now filters or refuses. That is the point — this is the
    /// state of every store on disk that declared a namespace owner before the
    /// rule existed, and a guard that only closes the door leaves all of those
    /// standing open.
    #[test]
    fn a_stored_authority_at_a_reach_its_kind_cannot_reach_answers_nothing() {
        let reach = Reach::Namespace(PROD);
        let legacy = Held::of([
            Authority::new(Kind::Replicate, reach),
            Authority::new(Kind::Read, reach),
        ]);

        assert!(
            !legacy.permits(Kind::Replicate, reach),
            "a namespace-reach replication row from an older binary must authorise nothing"
        );
        assert!(
            !legacy.permits(Kind::Replicate, Reach::Store),
            "and it must not have been read upward into the store either"
        );
        // The neighbouring row is untouched, so this refuses one authority and
        // not the record that carries it.
        assert!(legacy.permits(Kind::Read, reach));
    }

    #[test]
    fn an_explicit_set_beats_the_role_it_was_declared_with() {
        // The migration's whole point: once a record carries a set, the role is
        // no longer what decides — otherwise narrowing an editor would be
        // impossible without deleting them.
        let narrowed = Held::of([
            Authority::new(Kind::Read, Reach::Namespace(PROD)),
            Authority::new(Kind::Write, Reach::Namespace(PROD)),
        ]);
        let fields = BTreeMap::from([(FIELD_AUTHORITIES.to_owned(), narrowed.to_value())]);
        let held = held_of(&fields, Some(Role::Editor), Reach::Namespace(PROD)).expect("the set");
        assert_eq!(held, narrowed);
        assert!(
            !held.permits(Kind::Manage, Reach::Namespace(PROD)),
            "the stored set decides, not the role the record still carries"
        );
    }

    #[test]
    fn a_role_summary_is_the_widest_that_fits_and_never_a_near_miss() {
        let reach = Reach::Database(PROD, LIBRARY);

        // Each role summarises as itself, which is what makes the two spellings
        // of a declaration one declaration.
        for role in Role::ALL {
            assert_eq!(
                Held::from_role(*role, reach).role_within(reach),
                Some(*role),
                "{role:?} must summarise as itself"
            );
        }

        // Widest that fits, not nearest. An owner's set contains a viewer's, so
        // a search that stopped at the first match would report `viewer` for a
        // user holding everything — and this is the direction that matters,
        // because the summary is what an older binary reads.
        assert_eq!(
            Held::every_kind_at(reach).role_within(reach),
            Some(Role::Owner)
        );

        // A superset of `editor` that is not `owner` still summarises as the
        // editor it contains, and never as the owner it does not.
        let more = Held::of([
            Authority::new(Kind::Read, reach),
            Authority::new(Kind::Write, reach),
            Authority::new(Kind::Manage, reach),
            Authority::new(Kind::Operate, reach),
        ]);
        assert_eq!(more.role_within(reach), Some(Role::Editor));

        // And the case the whole model exists for has no summary at all. Every
        // role begins with `read`, so a set without it fits none of them —
        // reporting the nearest would hand an older binary a read this user does
        // not hold.
        let ungovernable = Held::of([Authority::new(Kind::Manage, reach)]);
        assert_eq!(ungovernable.role_within(reach), None);
        assert_eq!(Held::nothing().role_within(reach), None);

        // A set held at a *different* reach summarises as nothing here, so a
        // namespace authority is never reported as a role over one database in
        // it. Containment runs downward through holding, not through summary.
        assert_eq!(
            Held::from_role(Role::Owner, Reach::Namespace(PROD)).role_within(reach),
            None
        );
    }

    #[test]
    fn removing_takes_only_what_was_named() {
        let mut held = Held::of([
            Authority::new(Kind::Write, Reach::Store),
            Authority::new(Kind::Write, Reach::Namespace(PROD)),
        ]);
        assert!(held.remove(&Authority::new(Kind::Write, Reach::Namespace(PROD))));
        assert!(
            held.permits(Kind::Write, Reach::Namespace(PROD)),
            "the store-wide authority still contains this reach and was not rewritten"
        );
        assert!(!held.remove(&Authority::new(Kind::Read, Reach::Store)));
    }

    #[test]
    fn a_reach_comes_from_the_tenancy_and_a_database_needs_its_namespace() {
        assert_eq!(Reach::of(None, None), Some(Reach::Store));
        assert_eq!(Reach::of(Some(PROD), None), Some(Reach::Namespace(PROD)));
        assert_eq!(
            Reach::of(Some(PROD), Some(LIBRARY)),
            Some(Reach::Database(PROD, LIBRARY))
        );
        assert_eq!(
            Reach::of(None, Some(LIBRARY)),
            None,
            "a database in no namespace is not a place"
        );
    }

    #[test]
    fn a_kind_reads_back_from_how_it_is_written() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()), Some(*kind));
        }
        assert_eq!(Kind::parse("Read"), None, "names are case-sensitive");
        assert_eq!(Kind::parse("administer"), None);
    }
}
