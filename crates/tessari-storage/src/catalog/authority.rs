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

use tessari_types::{DatabaseId, NamespaceId, Value};

use super::user::Role;
use crate::error::{Error, Result};

const ENTITY: &str = "authority";

/// What an authority permits.
///
/// Five, and each names a different thing that can be taken away on its own.
/// The set is closed: a new kind is a new thing a store can refuse, which is a
/// decision rather than an addition.
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
}

impl Kind {
    /// Every kind, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::Read,
        Self::Write,
        Self::Manage,
        Self::Govern,
        Self::Operate,
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
        }
    }

    /// Read one back from how it is written.
    ///
    /// Case-sensitive, like every other name in this language.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|held| held.name() == text)
    }
}

/// How far an authority reaches.
///
/// [`Self::Database`] carries its namespace as well as its database, so a
/// database reach cannot be constructed without the namespace that contains it.
/// The alternative — two `Option` fields — makes "a database in no namespace"
/// a value somebody has to remember to reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reach {
    /// The whole store, every namespace in it.
    Store,
    /// One namespace, every database in it.
    Namespace(NamespaceId),
    /// One database.
    Database(NamespaceId, DatabaseId),
}

impl Reach {
    /// The reach a user's tenancy describes.
    ///
    /// `None` for a database named without a namespace, which is not a place —
    /// the caller decides whether that is corruption or a bad request, because
    /// this type cannot tell which of its callers it is answering.
    #[must_use]
    pub const fn of(namespace: Option<NamespaceId>, database: Option<DatabaseId>) -> Option<Self> {
        match (namespace, database) {
            (None, None) => Some(Self::Store),
            (Some(namespace), None) => Some(Self::Namespace(namespace)),
            (Some(namespace), Some(database)) => Some(Self::Database(namespace, database)),
            (None, Some(_)) => None,
        }
    }

    /// Whether this reach contains `other`.
    ///
    /// The only implication in this model. Downward and nothing else: the store
    /// contains a namespace, a namespace contains its databases, and a reach
    /// contains itself.
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        match (self, other) {
            (Self::Store, _) => true,
            (Self::Namespace(mine), Self::Namespace(theirs)) => mine == theirs,
            (Self::Namespace(mine), Self::Database(theirs, _)) => mine == theirs,
            (Self::Database(namespace, database), Self::Database(theirs, their_database)) => {
                namespace == theirs && database == their_database
            }
            // A database reach does not contain the namespace above it, and a
            // namespace reach does not contain the store. Written out rather
            // than left to a catch-all so that adding a reach fails to compile
            // here instead of silently answering `false`.
            (Self::Namespace(_) | Self::Database(_, _), Self::Store)
            | (Self::Database(_, _), Self::Namespace(_)) => false,
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
    #[must_use]
    pub fn permits(self, kind: Kind, reach: Reach) -> bool {
        self.kind == kind && self.reach.contains(reach)
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

    /// Every kind at one reach — the shape an owner of something has.
    #[must_use]
    pub fn every_kind_at(reach: Reach) -> Self {
        Self::of(Kind::ALL.iter().map(|kind| Authority::new(*kind, reach)))
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
/// role when it does not. Both forms are current: the role is still written, so
/// a binary that predates this can still read a record this one wrote.
///
/// # Errors
///
/// Returns [`Error::CatalogMalformed`] when the field is present and is not an
/// authority set.
pub(super) fn held_of(fields: &BTreeMap<String, Value>, role: Role, reach: Reach) -> Result<Held> {
    match fields.get(FIELD_AUTHORITIES) {
        None => Ok(Held::from_role(role, reach)),
        Some(value) => Held::from_value(value),
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

        let viewer = held_of(&fields, Role::Viewer, reach).expect("a viewer");
        assert!(viewer.permits(Kind::Read, reach));
        assert!(!viewer.permits(Kind::Write, reach));

        // The bundle the new rule forbids, preserved on purpose: an editor was
        // declared under a promise that they may define structure.
        let editor = held_of(&fields, Role::Editor, reach).expect("an editor");
        assert!(editor.permits(Kind::Read, reach));
        assert!(editor.permits(Kind::Write, reach));
        assert!(editor.permits(Kind::Manage, reach));
        assert!(!editor.permits(Kind::Govern, reach));
        assert!(!editor.permits(Kind::Operate, reach));

        let owner = held_of(&fields, Role::Owner, reach).expect("an owner");
        for kind in Kind::ALL {
            assert!(owner.permits(*kind, reach));
        }
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
        let held = held_of(&fields, Role::Editor, Reach::Namespace(PROD)).expect("the set");
        assert_eq!(held, narrowed);
        assert!(
            !held.permits(Kind::Manage, Reach::Namespace(PROD)),
            "the stored set decides, not the role the record still carries"
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
