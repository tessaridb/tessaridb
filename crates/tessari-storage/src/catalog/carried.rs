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
//! # The identity class travels everywhere, always — and what had to be true first
//!
//! The cluster has **one set of users**: a node that joins holds the same
//! identities as the node it joined, so an operator who can sign in on the
//! leader can sign in on any follower. That is the concept's own text and it is
//! the instruction this module now follows.
//!
//! It has not always been safe, and the history is worth keeping because it is
//! the argument for the invariant that replaced it. This module once carried a
//! user by **the user's own tenancy**, and the reason was [`Kind::Replicate`]:
//! it had joined a closed set whose owner role expands to every kind, so every
//! namespace owner already declared held it over their own namespace. An
//! *everywhere* identity class would have handed them every credential hash in
//! the store, across every tenancy, through a permission nobody granted them
//! separately.
//!
//! What changed is the permission, not the appetite for the risk.
//! [`Kind::may_be_held_at`] makes `replicate` a thing held over the whole store
//! or not at all, so the only principal who can open **any** subscription is one
//! already entitled to every byte in the store. A follower is provisioned by the
//! cluster rather than asked for by a tenant, and the narrowing a selective
//! subscription performs is an arrangement rather than a right.
//!
//! # So the protection moved rather than being dropped
//!
//! It used to be *the hash does not arrive*. It is now *the hash arrives on a
//! node the cluster stood up, and a tenant on that node cannot read it* —
//! `Session::administers` bounds every read of a user by the reader's own
//! tenancy, on the singular form and on the listing alike. Those are different
//! defences with different failure modes, and the second is the one that has to
//! hold on a follower, so it is asserted there rather than inferred.
//!
//! One thing does not move: a hash that reached a subscriber has reached it, and
//! no later fix recovers that. The reason this direction is takeable at all is
//! that the set of subscribers shrank first.
//!
//! [`Kind::Replicate`]: super::Kind
//! [`Kind::may_be_held_at`]: super::Kind::may_be_held_at

use tessari_encoding::{LogRecord, Mutation, RecordValue, decode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Value};

use super::system::{self, Level};
use super::{Reach, definition};
use crate::error::Result;

/// Who a mutation is carried to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Carried {
    /// It belongs to one tenancy, and travels to a subscription that reaches it.
    Within(Reach),
    /// It DEFINES something at this reach — a namespace, a database, a table, a
    /// name, a leadership — and travels to a subscription inside it as well as
    /// to one containing it (Q-788, G031 S3.2).
    ///
    /// Definitions travel down the containment order and data does not travel
    /// sideways. A subscriber to one database needs the namespace's definition
    /// and name to be able to say `USE NAMESPACE` at all, and a subscriber to one
    /// shard needs its table's; carried only `Within` their own level, a
    /// narrower subscription received records nothing on it could name.
    Schema(Reach),
    /// It has no tenancy of its own and holds nothing secret, and any tenancy's
    /// schema may point at it — so it travels to every subscriber.
    ///
    /// Analyzers: a name unique across the store, a filter chain, and no
    /// reference to anybody's data. Withholding one would leave a follower
    /// holding a full-text field whose analyzer it cannot resolve, which fails
    /// at read time and looks like corruption. And the identity class — users
    /// and the grants that narrow them, which must arrive together (see
    /// [`carried_to`]).
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
            Self::Schema(own) => subscription.contains(own) || own.contains(subscription),
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
        //
        // A mutation of a split table carries its shard, stamped by the writer
        // at commit (G031, ADR-0080), so it is carried within that shard — read
        // from the record's own bytes, like everything else here.
        if let Some(shard) = mutation.shard {
            return Ok(Carried::Within(Reach::Shard(
                mutation.namespace,
                mutation.database,
                mutation.table,
                shard,
            )));
        }
        return Ok(
            match Reach::of(Some(mutation.namespace), Some(mutation.database)) {
                Some(reach) => Carried::Within(reach),
                // Unconstructible from a mutation, which always carries both.
                None => Carried::StoreOnly,
            },
        );
    }
    let present = match mutation.value.value() {
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
        (t, Some(value)) if t == system::NAMESPACES => Carried::Schema(Reach::Namespace(
            definition::NamespaceDefinition::from_value(&value)?.id,
        )),
        (t, Some(value)) if t == system::DATABASES => {
            let declared = definition::DatabaseDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.id))
        }
        (t, Some(value)) if t == system::TABLES => {
            let declared = super::TableDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::INDEXES => {
            let declared = definition::IndexDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::FIELDS => {
            let declared = super::FieldDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::GRAPHS => {
            let declared = super::GraphDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::EDGE_KINDS => {
            let declared = super::EdgeKindDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::CONSUMERS => Carried::Schema(Reach::Namespace(
            super::ConsumerDefinition::from_value(&value)?.namespace,
        )),
        // A leadership travels to whoever holds the range it is about: a
        // follower subscribed to one namespace needs to know who leads that
        // namespace, and has no business learning who leads another's.
        (t, Some(value)) if t == system::LEADERSHIPS => {
            Carried::Schema(super::LeadershipDefinition::from_value(&value)?.range)
        }
        // The identity class, and it is unconditional: every user reaches every
        // subscriber, whatever tenancy they were declared at. Not read from the
        // record at all, because there is no longer a question to ask of it —
        // classifying by tenancy is what produced the split this reversed, and
        // leaving the read in place would leave the split one edit away.
        (t, _) if t == system::USERS => Carried::Everywhere,
        // A user's grants travel with the user, for the same reason and with
        // the same unconditional rule (G033). A grant NARROWS its user — one
        // grant reduces a user to exactly what was granted — so a follower
        // holding the user without the grant holds that user UNRESTRICTED, and
        // a reader allowed one field reads the whole record there with nothing
        // in an error state. It was `StoreOnly` because a grant carries a table
        // id and no tenancy; that was right while users were classified by
        // tenancy too, and became a widening the day they travelled everywhere.
        // A revocation's tombstone reaches every node the grant did.
        (t, _) if t == system::GRANTS => Carried::Everywhere,
        // Everything else, named rather than left to a catch-all so that the
        // ratchet below is a statement about a set somebody wrote down:
        //   ALLOCATORS        one counter per level, for the whole store
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
                Carried::Schema(Reach::Namespace(NamespaceId::new(id)))
            }),
        (Level::Database, Some(&namespace), _) => {
            Carried::Schema(Reach::Namespace(NamespaceId::new(namespace)))
        }
        (
            Level::Table | Level::Index | Level::Field | Level::Graph | Level::EdgeKind,
            Some(&namespace),
            Some(&database),
        ) => Carried::Schema(Reach::Database(
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

/// Where a whole log record is filed.
///
/// [`carried_to`] answers for one mutation, and a record carries many: one
/// transaction's writes accumulate into a single set and are emitted as one
/// record, so `BEGIN; DEFINE ANALYZER …; CREATE person …; COMMIT` produces a
/// record holding an [`Carried::Everywhere`] mutation beside a
/// [`Carried::Within`] one. A partition over records therefore has to be total
/// over SETS of mutations, and this is that function.
///
/// The home is the [`Reach::join`] of what each mutation answers — the narrowest
/// reach that covers all of them. A record written entirely inside one database
/// homes there; one that touches two databases in a namespace homes at the
/// namespace; one that touches two namespaces homes at the store.
///
/// # Why `Everywhere` files at the store rather than everywhere
///
/// An analyzer travels to every subscriber, and the store log is the only log
/// every subscriber reads — so it is the only correct home for a record everyone
/// needs. The alternative, copying the record into each existing partition, is
/// unbounded in the number of partitions and puts one record at two positions,
/// which is the thing a log exists not to do. The cost is that a transaction
/// defining an analyzer alongside data files the whole record at the top; that is
/// accepted rather than optimised, because analyzer definitions are rare and the
/// filter on the way out still narrows what each subscriber sees.
///
/// An empty record homes at the store as well. A commit never produces one, but
/// a replica must be able to apply whatever it is sent, and the store is the
/// only answer that cannot be wrong for a record that says nothing.
///
/// # Errors
///
/// Returns [`crate::Error::CatalogMalformed`] when a mutation's definition is
/// present and cannot be decoded — the same refusal [`carried_to`] makes, and for
/// the same reason: a record whose home cannot be proven is filed nowhere rather
/// than filed wrongly.
pub(crate) fn home_of(record: &LogRecord) -> Result<Reach> {
    let mut home = None;
    for mutation in record.mutations() {
        let own = match carried_to(mutation)? {
            Carried::Within(reach) | Carried::Schema(reach) => reach,
            Carried::Everywhere | Carried::StoreOnly => Reach::Store,
        };
        home = Some(match home {
            Some(so_far) => Reach::join(so_far, own),
            None => own,
        });
    }
    Ok(home.unwrap_or(Reach::Store))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tessari_encoding::{StampedValue, encode_payload};
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
            shard: None,
            value: StampedValue::new(value.map_or(RecordValue::Tombstone, |value| {
                RecordValue::Present(encode_payload(&value).into_bytes())
            })),
        }
    }

    fn class(table: TableId, id: RecordId, value: Option<Value>) -> Carried {
        carried_to(&system(table, id, value)).unwrap()
    }

    fn data(namespace: NamespaceId, database: DatabaseId, id: i64) -> Mutation {
        Mutation {
            namespace,
            database,
            table: TableId::new(4),
            id: RecordId::Int(id),
            shard: None,
            value: StampedValue::new(RecordValue::Tombstone),
        }
    }

    #[test]
    fn a_record_written_inside_one_database_homes_there() {
        let record = LogRecord::new(vec![data(PROD, SHOP, 1), data(PROD, SHOP, 2)]);
        assert_eq!(home_of(&record).unwrap(), Reach::Database(PROD, SHOP));
    }

    #[test]
    fn a_record_touching_two_databases_homes_at_the_namespace_above_them() {
        let other = DatabaseId::new(9);
        let record = LogRecord::new(vec![data(PROD, SHOP, 1), data(PROD, other, 1)]);
        assert_eq!(home_of(&record).unwrap(), Reach::Namespace(PROD));
    }

    #[test]
    fn a_record_touching_two_namespaces_homes_at_the_store() {
        let elsewhere = NamespaceId::new(8);
        let record = LogRecord::new(vec![data(PROD, SHOP, 1), data(elsewhere, SHOP, 1)]);
        assert_eq!(home_of(&record).unwrap(), Reach::Store);
    }

    #[test]
    fn an_analyzer_beside_data_takes_the_whole_record_to_the_store() {
        // The case that made the home a question at all: one transaction's
        // writes are emitted as one record, so a record can hold a mutation
        // every subscriber needs beside one that belongs to a single database.
        // The store log is the only log every subscriber reads, so it is the
        // only home that cannot withhold the analyzer from somebody who needs
        // it — even though it widens the data mutation's own home.
        let analyzer = system(
            system::ANALYZERS,
            RecordId::from("english"),
            Some(Value::None),
        );
        assert_eq!(carried_to(&analyzer).unwrap(), Carried::Everywhere);
        let record = LogRecord::new(vec![data(PROD, SHOP, 1), analyzer]);
        assert_eq!(home_of(&record).unwrap(), Reach::Store);
    }

    #[test]
    fn a_record_carrying_nothing_homes_at_the_store() {
        // A commit never produces one — an empty transaction never reaches the
        // log — but a replica applies whatever it is sent, and the store is the
        // only home that cannot be wrong for a record that says nothing.
        assert_eq!(home_of(&LogRecord::new(Vec::new())).unwrap(), Reach::Store);
    }

    #[test]
    fn the_home_contains_every_mutation_it_carries() {
        // The property behind the tables above: whatever the home is, the
        // filter on the way out must let every one of the record's own
        // mutations through it. A home that failed this would drop a record's
        // own writes from the subscriber the record was filed for.
        let elsewhere = NamespaceId::new(8);
        let record = LogRecord::new(vec![
            data(PROD, SHOP, 1),
            data(PROD, DatabaseId::new(9), 1),
            data(elsewhere, SHOP, 1),
        ]);
        let home = home_of(&record).unwrap();
        for mutation in record.mutations() {
            assert!(
                carried_to(mutation).unwrap().reaches(home),
                "{home:?} does not carry a mutation of its own record"
            );
        }
    }

    /// Ordinary data needs no decode at all: its address is its tenancy.
    #[test]
    fn a_data_mutation_is_carried_by_its_own_address() {
        let mutation = Mutation {
            namespace: PROD,
            database: SHOP,
            table: TableId::new(4),
            id: RecordId::Int(1),
            shard: None,
            value: StampedValue::new(RecordValue::Tombstone),
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
                        class: None,
                    }
                    .to_value()
                )
            ),
            Carried::Schema(Reach::Namespace(PROD))
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
            Carried::Schema(Reach::Database(PROD, SHOP))
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

        // 9 — a user, wherever they were declared. The cluster has one set of
        // identities, so this row is deliberately insensitive to the tenancy in
        // the record: both spellings below are the same answer, and asserting
        // both is what says so.
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
            Carried::Everywhere
        );
        let store_wide = UserDefinition {
            namespace: None,
            ..namespaced
        };
        assert_eq!(
            class(system::USERS, RecordId::Int(1), Some(store_wide.to_value())),
            Carried::Everywhere,
            "a store-level user reaches every follower, which is what lets one be signed in there"
        );
        // And a tombstone too, which the old rule could not carry at all: a
        // deleted user is read from the value, and a value that is gone proved no
        // tenancy. A deletion that does not reach a follower leaves a credential
        // live on it after it was revoked everywhere else.
        assert_eq!(
            class(system::USERS, RecordId::Int(1), None),
            Carried::Everywhere,
            "a revocation must reach every node the credential reached"
        );

        // 5, 11, 13, 16, 17, 18 — the store's own business, on a payload that
        // decodes, so the answer is the classification and not a decode failure.
        // 10 — grants go where users go, and their revocation with them.
        assert_eq!(
            class(system::GRANTS, RecordId::Int(1), Some(Value::Null)),
            Carried::Everywhere,
            "a grant narrows its user, so a follower holding the user must hold it"
        );
        assert_eq!(
            class(system::GRANTS, RecordId::Int(1), None),
            Carried::Everywhere,
            "a revoked grant must leave every node it reached"
        );

        for table in [
            system::ALLOCATORS,
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

    /// A definition travels to a subscription inside its level as well as one
    /// containing it; data travels only to one containing it; and neither
    /// travels sideways (Q-788, G031 S3.2).
    #[test]
    fn a_definition_travels_down_and_data_does_not_travel_sideways() {
        let namespace = Reach::Namespace(PROD);
        let database = Reach::Database(PROD, SHOP);
        let shard = Reach::Shard(PROD, SHOP, TableId::new(5), tessari_types::ShardId::new(2));
        let sibling = Reach::Shard(PROD, SHOP, TableId::new(5), tessari_types::ShardId::new(3));
        assert!(Carried::Schema(namespace).reaches(database));
        assert!(Carried::Schema(database).reaches(shard));
        assert!(Carried::Schema(database).reaches(Reach::Store));
        assert!(
            !Carried::Schema(sibling).reaches(shard),
            "a sibling's definition"
        );
        assert!(
            !Carried::Within(database).reaches(shard),
            "a database's data"
        );
        assert!(Carried::Within(shard).reaches(database));
        assert!(!Carried::Within(sibling).reaches(shard));
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
            Carried::Schema(Reach::Database(PROD, SHOP)),
            "a dropped table's name reaches the follower that held the table"
        );
        assert_eq!(
            class(system::NAMES, RecordId::Text("db:7/shop".to_owned()), None),
            Carried::Schema(Reach::Namespace(PROD))
        );
        // A namespace's own name has no parents in the key, so it is read from
        // the value — and its tombstone is therefore unprovable.
        assert_eq!(
            class(
                system::NAMES,
                RecordId::Text("ns:prod".to_owned()),
                Some(definition::number(7))
            ),
            Carried::Schema(Reach::Namespace(PROD))
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
