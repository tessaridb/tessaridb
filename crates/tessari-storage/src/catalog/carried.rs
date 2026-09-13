//! Which subscribers a mutation is carried to.
//!
//! A subscription is a [`Reach`] (ADR-0061) and the stream is filtered by it:
//! the leader sends every sequence to every follower, and a mutation outside the
//! subscription's reach is elided rather than delivered. This module answers the
//! one question that makes that possible — *whose is this record?*
//!
//! # It is a pure function of the mutation, and that is the load-bearing choice
//!
//! Nothing here reads the store. A catalog lookup would be taken at **stream**
//! time rather than at the record's own time, so a table dropped after the
//! record was written would classify differently on a re-stream than on the
//! first pass — and a filter whose answer depends on when it ran is a filter
//! nobody can reason about. The price is paid openly below: what cannot be
//! proven from the mutation alone is [`Carried::StoreOnly`], which for a
//! selective follower means elided.
//!
//! # The identity class does not travel below store reach
//!
//! The cluster concept put users, credentials and grants in an *everywhere,
//! always* class. That was written when a subscription meant a whole node. It
//! stopped being safe the moment [`Kind::Replicate`] joined a closed set whose
//! owner role expands to every kind: every namespace owner already declared
//! gained `replicate` over their own namespace, and an *everywhere* identity
//! class would hand them every credential hash in the store — across every
//! tenancy — through a permission nobody granted them separately.
//!
//! So a user is carried by **the user's own tenancy**. A namespace's users reach
//! that namespace's follower, which is the reader such a follower needs; a
//! store-level user reaches a store-reach subscriber and nobody else. The cost
//! of the other reading is not recoverable by a later fix, because a hash that
//! reached a subscriber has reached it.
//!
//! [`Kind::Replicate`]: super::Kind

use tessari_encoding::{Mutation, RecordValue, decode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Value};

use super::system::{self, Level};
use super::{Reach, definition};
use crate::error::Result;

/// Who a mutation is carried to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Carried {
    /// It belongs to one tenancy, and travels to a subscription that reaches it.
    Within(Reach),
    /// It has no tenancy of its own and holds nothing secret, and any tenancy's
    /// schema may point at it — so it travels to every subscriber.
    ///
    /// Analyzers, and deliberately only analyzers: a name unique across the
    /// store, a filter chain, and no reference to anybody's data. Withholding
    /// one would leave a follower holding a full-text field whose analyzer it
    /// cannot resolve, which fails at read time and looks like corruption.
    Everywhere,
    /// It is the whole store's business, or its tenancy cannot be proven from
    /// the mutation alone. Either way it travels only to a subscriber at
    /// [`Reach::Store`].
    StoreOnly,
}

impl Carried {
    /// Whether a subscription over `subscription` receives this.
    pub(crate) fn reaches(self, subscription: Reach) -> bool {
        match self {
            Self::Everywhere => true,
            Self::Within(own) => subscription.contains(own),
            Self::StoreOnly => subscription == Reach::Store,
        }
    }
}

/// Who this mutation is carried to.
///
/// # Errors
///
/// Returns [`crate::Error::CatalogMalformed`] when a catalog record is present
/// and cannot be decoded. A malformed definition is refused rather than treated
/// as untenanted: answering [`Carried::StoreOnly`] would silently withhold a
/// follower's own schema, and the store would be wrong in a direction nothing
/// reports.
pub(crate) fn carried_to(mutation: &Mutation) -> Result<Carried> {
    if mutation.namespace != system::SYSTEM_NAMESPACE
        || mutation.database != system::SYSTEM_DATABASE
    {
        // Ordinary data. Its address *is* its tenancy, so this costs no decode
        // at all — which is the case that runs on every record.
        return Ok(
            match Reach::of(Some(mutation.namespace), Some(mutation.database)) {
                Some(reach) => Carried::Within(reach),
                // Unconstructible from a mutation, which always carries both.
                None => Carried::StoreOnly,
            },
        );
    }
    let present = match &mutation.value {
        RecordValue::Present(payload) => Some(decode_payload(payload)?),
        // A tombstone in the system tenancy has lost the fields its tenancy was
        // written in. Unprovable, so `StoreOnly` — a selective follower can
        // therefore keep a stale catalog entry for an object dropped in its own
        // namespace. Staleness is the safe side of this trade and disclosure is
        // not, which is the direction structural default-deny asks for.
        RecordValue::Tombstone => None,
    };
    let table = mutation.table;
    if table == system::NAMES {
        return Ok(named(mutation, present.as_ref()));
    }
    Ok(match (table, present) {
        (t, _) if t == system::ANALYZERS => Carried::Everywhere,
        (t, Some(value)) if t == system::NAMESPACES => Carried::Within(Reach::Namespace(
            definition::NamespaceDefinition::from_value(&value)?.id,
        )),
        (t, Some(value)) if t == system::DATABASES => {
            let declared = definition::DatabaseDefinition::from_value(&value)?;
            Carried::Within(Reach::Database(declared.namespace, declared.id))
        }
        (t, Some(value)) if t == system::TABLES => {
            let declared = super::TableDefinition::from_value(&value)?;
            Carried::Within(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::INDEXES => {
            let declared = definition::IndexDefinition::from_value(&value)?;
            Carried::Within(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::FIELDS => {
            let declared = super::FieldDefinition::from_value(&value)?;
            Carried::Within(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::GRAPHS => {
            let declared = super::GraphDefinition::from_value(&value)?;
            Carried::Within(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::EDGE_KINDS => {
            let declared = super::EdgeKindDefinition::from_value(&value)?;
            Carried::Within(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::CONSUMERS => Carried::Within(Reach::Namespace(
            super::ConsumerDefinition::from_value(&value)?.namespace,
        )),
        // A leadership travels to whoever holds the range it is about: a
        // follower subscribed to one namespace needs to know who leads that
        // namespace, and has no business learning who leads another's.
        (t, Some(value)) if t == system::LEADERSHIPS => {
            Carried::Within(super::LeadershipDefinition::from_value(&value)?.range)
        }
        (t, Some(value)) if t == system::USERS => {
            let declared = super::UserDefinition::from_value(&value)?;
            match Reach::of(declared.namespace, declared.database) {
                // A store-level user, or one whose tenancy names a database with
                // no namespace — which is not a place. Both stay at the store.
                Some(Reach::Store) | None => Carried::StoreOnly,
                Some(reach) => Carried::Within(reach),
            }
        }
        // Everything else, named rather than left to a catch-all so that the
        // ratchet below is a statement about a set somebody wrote down:
        //   ALLOCATORS        one counter per level, for the whole store
        //   GRANTS            a user and a table id, and no tenancy of its own
        //   REPLICAS          who else is in the cluster is the store's
        //   RECORD_SEQUENCES  keyed by table id, tenancy not in the record
        //   VAULT_ROOT        one record, the store's
        //   VAULT_AUDIT       the store's own trail
        //   RECORD_COUNTS     derived on apply and never logged; classified anyway
        //   LEADERSHIPS       a tombstone only; the range is in the value, and
        //                     nothing in this build removes a leadership row
        _ => Carried::StoreOnly,
    })
}

/// Who a qualified-name record is carried to.
///
/// The name table is what makes `USE NAMESPACE prod` and `SELECT … FROM orders`
/// resolve, so it is the mechanism behind *cannot name another tenant's tables*
/// rather than an accessory to it: withhold `staging`'s names and the reader on
/// a `prod` follower has nothing to resolve.
///
/// The key carries the tenancy for every level that has one — that is why
/// `qualify` puts the parent ids in it — so a **tombstone** classifies as
/// exactly as well as a definition, and a dropped name reaches the follower that
/// held it. A namespace's own name has no parents, so it is read from the value
/// instead and its tombstone is unprovable.
fn named(mutation: &Mutation, value: Option<&Value>) -> Carried {
    let RecordId::Text(qualified) = &mutation.id else {
        // Every name is claimed under the qualified string itself, so anything
        // else in this table is not a name this build wrote.
        return Carried::StoreOnly;
    };
    let Some((level, parents)) = super::parse_qualified(qualified) else {
        return Carried::StoreOnly;
    };
    match (level, parents.first(), parents.get(1)) {
        (Level::Namespace, _, _) => value
            .and_then(|value| definition::id_of(value, "name", "id").ok())
            .map_or(Carried::StoreOnly, |id| {
                Carried::Within(Reach::Namespace(NamespaceId::new(id)))
            }),
        (Level::Database, Some(&namespace), _) => {
            Carried::Within(Reach::Namespace(NamespaceId::new(namespace)))
        }
        (
            Level::Table | Level::Index | Level::Field | Level::Graph | Level::EdgeKind,
            Some(&namespace),
            Some(&database),
        ) => Carried::Within(Reach::Database(
            NamespaceId::new(namespace),
            DatabaseId::new(database),
        )),
        // An analyzer, a user, a replica or a consumer name: the key carries
        // no parents, because those names are unique across the store rather
        // than within a tenancy, so nothing here can prove one.
        //
        // A user's name is the case worth stating, because withholding it looks
        // like it should break sign-in on the follower and does not: `sign_in`
        // matches over the user *records*, never over this table, so a follower
        // holding its own namespace's users can sign them in with no name
        // record at all. What it cannot do is reserve a new one, which is
        // exactly what a follower has no business doing.
        _ => Carried::StoreOnly,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tessari_encoding::encode_payload;
    use tessari_types::{RecordId, TableId};

    use super::*;
    use crate::catalog::definition::{DatabaseDefinition, NamespaceDefinition};
    use crate::catalog::user::UserDefinition;

    const PROD: NamespaceId = NamespaceId::new(7);
    const SHOP: DatabaseId = DatabaseId::new(3);

    fn system(table: TableId, id: RecordId, value: Option<Value>) -> Mutation {
        Mutation {
            namespace: system::SYSTEM_NAMESPACE,
            database: system::SYSTEM_DATABASE,
            table,
            id,
            value: value.map_or(RecordValue::Tombstone, |value| {
                RecordValue::Present(encode_payload(&value).into_bytes())
            }),
        }
    }

    fn class(table: TableId, id: RecordId, value: Option<Value>) -> Carried {
        carried_to(&system(table, id, value)).unwrap()
    }

    /// Ordinary data needs no decode at all: its address is its tenancy.
    #[test]
    fn a_data_mutation_is_carried_by_its_own_address() {
        let mutation = Mutation {
            namespace: PROD,
            database: SHOP,
            table: TableId::new(4),
            id: RecordId::Int(1),
            value: RecordValue::Tombstone,
        };
        assert_eq!(
            carried_to(&mutation).unwrap(),
            Carried::Within(Reach::Database(PROD, SHOP))
        );
    }

    /// The eighteen-row ratchet — every system table has a recorded class.
    ///
    /// # A nineteenth table must be a decision, and the compiler cannot make it one
    ///
    /// `TableId` is a newtype over `u32`, so the match in `carried_to` cannot be
    /// exhaustive and a new system table would fall to its catch-all and be
    /// withheld from every selective follower — silently, because withholding
    /// looks exactly like a table that simply had no mutations.
    ///
    /// So the ratchet is written here instead, in the shape `Kind::ALL` uses one
    /// layer up: the assertion is an **equality** over the declared set and not a
    /// containment, because the failure being guarded is a set that GREW.
    #[test]
    fn every_system_table_has_a_recorded_replication_class() {
        // 1 — a namespace is carried to a subscription that reaches it.
        assert_eq!(
            class(
                system::NAMESPACES,
                RecordId::Int(7),
                Some(
                    NamespaceDefinition {
                        id: PROD,
                        name: "prod".to_owned(),
                        replication: None,
                    }
                    .to_value()
                )
            ),
            Carried::Within(Reach::Namespace(PROD))
        );

        // 2 — a database, by the namespace it names.
        assert_eq!(
            class(
                system::DATABASES,
                RecordId::Int(3),
                Some(
                    DatabaseDefinition {
                        id: SHOP,
                        namespace: PROD,
                        name: "shop".to_owned(),
                    }
                    .to_value()
                )
            ),
            Carried::Within(Reach::Database(PROD, SHOP))
        );

        // 3, 6, 7, 12, 14, 15 — the tenanted definitions. Asserted by REFUSAL on
        // a payload that is not one: what matters about these arms is that they
        // decode the record rather than default it, and an arm that had quietly
        // become a catch-all would answer `StoreOnly` here instead of erring.
        for table in [
            system::TABLES,
            system::INDEXES,
            system::FIELDS,
            system::CONSUMERS,
            system::GRAPHS,
            system::EDGE_KINDS,
        ] {
            let refused = carried_to(&system(table, RecordId::Int(1), Some(Value::Null)));
            assert!(
                refused.is_err(),
                "table {} must decode its definition rather than default it",
                table.get()
            );
        }

        // 8 — an analyzer: no tenancy, nothing secret, and any tenancy's schema
        // may point at one.
        assert_eq!(
            class(system::ANALYZERS, RecordId::Int(1), Some(Value::Null)),
            Carried::Everywhere
        );

        // 9 — a user, by the user's OWN tenancy. This is the Q-535 row.
        let namespaced = UserDefinition {
            id: 2,
            name: "prod_reader".to_owned(),
            namespace: Some(PROD),
            database: None,
            role: None,
            authorities: crate::catalog::Held::default(),
            secret: "$argon2id$".to_owned(),
        };
        assert_eq!(
            class(system::USERS, RecordId::Int(2), Some(namespaced.to_value())),
            Carried::Within(Reach::Namespace(PROD))
        );
        let store_wide = UserDefinition {
            namespace: None,
            ..namespaced
        };
        assert_eq!(
            class(system::USERS, RecordId::Int(1), Some(store_wide.to_value())),
            Carried::StoreOnly,
            "a store-level user's credential record stays at the store"
        );

        // 5, 10, 11, 13, 16, 17, 18 — the store's own business, on a payload that
        // decodes, so the answer is the classification and not a decode failure.
        for table in [
            system::ALLOCATORS,
            system::GRANTS,
            system::REPLICAS,
            system::RECORD_SEQUENCES,
            system::VAULT_ROOT,
            system::VAULT_AUDIT,
            system::RECORD_COUNTS,
        ] {
            assert_eq!(
                class(table, RecordId::Int(1), Some(Value::Null)),
                Carried::StoreOnly,
                "table {} is the store's",
                table.get()
            );
        }

        // 4 — names, below, because the key rather than the value carries it.
        // The ratchet itself: eighteen declared, and the last one is this.
        assert_eq!(
            system::RECORD_COUNTS.get(),
            18,
            "a nineteenth system table needs a row above before it can ship"
        );
    }

    /// A qualified name is classified from its KEY, so a drop classifies as well
    /// as a definition — which is why `qualify` puts the parent ids in it.
    #[test]
    fn a_qualified_name_is_carried_by_the_tenancy_in_its_key() {
        assert_eq!(
            class(
                system::NAMES,
                RecordId::Text("tb:7/3/orders".to_owned()),
                None
            ),
            Carried::Within(Reach::Database(PROD, SHOP)),
            "a dropped table's name reaches the follower that held the table"
        );
        assert_eq!(
            class(system::NAMES, RecordId::Text("db:7/shop".to_owned()), None),
            Carried::Within(Reach::Namespace(PROD))
        );
        // A namespace's own name has no parents in the key, so it is read from
        // the value — and its tombstone is therefore unprovable.
        assert_eq!(
            class(
                system::NAMES,
                RecordId::Text("ns:prod".to_owned()),
                Some(definition::number(7))
            ),
            Carried::Within(Reach::Namespace(PROD))
        );
        assert_eq!(
            class(system::NAMES, RecordId::Text("ns:prod".to_owned()), None),
            Carried::StoreOnly
        );
        // A user's name is unique across the store, so the key proves nothing.
        // Withholding it does not break sign-in, which matches over the user
        // records rather than over this table.
        assert_eq!(
            class(
                system::NAMES,
                RecordId::Text("us:prod_reader".to_owned()),
                Some(definition::number(2))
            ),
            Carried::StoreOnly
        );
    }

    /// A subscription at the store receives everything, and that is what makes
    /// whole-node replication a distinct kind rather than a union of namespaces.
    #[test]
    fn a_store_subscription_reaches_every_class() {
        for carried in [
            Carried::Everywhere,
            Carried::StoreOnly,
            Carried::Within(Reach::Namespace(PROD)),
            Carried::Within(Reach::Database(PROD, SHOP)),
        ] {
            assert!(carried.reaches(Reach::Store), "{carried:?} at store reach");
        }
        assert!(!Carried::StoreOnly.reaches(Reach::Namespace(PROD)));
        assert!(
            !Carried::Within(Reach::Namespace(NamespaceId::new(8))).reaches(Reach::Namespace(PROD))
        );
    }
}
