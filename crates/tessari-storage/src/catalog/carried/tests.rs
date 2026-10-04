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
                    acknowledge: None,
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

    // 24 — a revoked peer certificate reaches every node, whatever it
    // follows: a node that missed the row admits the peer the rest refuse.
    assert_eq!(
        class(
            system::REVOKED_CERTIFICATES,
            RecordId::from("ab"),
            Some(Value::Null)
        ),
        Carried::Everywhere,
        "a revocation must reach a follower of one namespace too"
    );
    // 25 — a removed node, likewise.
    assert_eq!(
        class(
            system::TOMBSTONED_NODES,
            RecordId::Uuid([1; 16]),
            Some(Value::Null)
        ),
        Carried::Everywhere,
        "a removal must reach a follower of one namespace too"
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
