//! A participant's half of a transaction across leaders (G053 SG3, ADR-0112),
//! on one store: the three records, written as a user the asking node
//! verified, and what this node refuses to write for them.

use std::sync::Arc;

use tessari_encoding::{
    Decision, Mutation, Participant, RecordValue, StampedValue, TRANSACTION_ID_LEN, TransactionId,
    TransactionRecord, encode_payload,
};
use tessari_kv::MemoryBackend;
use tessari_session::{AcrossAnswer, AcrossAsk, Error, Session};
use tessari_storage::{Catalog, Reach, RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value};

const TRANSACTION: TransactionId = TransactionId::new([3; TRANSACTION_ID_LEN]);
const PASSWORD: &str = "correct horse battery";

/// `prod.shop` with a collection `notes` holding `n:1 = 'old'`, a space
/// `cache`, an owner, and an editor scoped to `prod.shop` granted only `READ`
/// on `notes`.
fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut root = Session::new(&store);
    root.run(&format!(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
         DEFINE COLLECTION notes; DEFINE SPACE cache; CREATE notes:1 = 'old'; \
         DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
    ))
    .unwrap();
    root.sign_in("root", PASSWORD).unwrap();
    root.run(&format!(
        "USE NAMESPACE prod; USE DATABASE shop; \
         DEFINE USER reader ON prod.shop ROLE editor PASSWORD '{PASSWORD}'; \
         GRANT read ON notes TO reader;"
    ))
    .unwrap();
    store
}

fn ids(store: &Store, table: &str) -> (NamespaceId, DatabaseId, TableId) {
    let mut reading = store.begin().unwrap();
    let catalog = Catalog::new(&mut reading);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, table)
        .unwrap()
        .unwrap();
    (namespace, database, table)
}

fn write(store: &Store, table: &str, value: &str) -> Mutation {
    let (namespace, database, table) = ids(store, table);
    Mutation {
        namespace,
        database,
        table,
        id: RecordId::Int(1),
        shard: None,
        value: StampedValue::new(RecordValue::Present(
            encode_payload(&Value::from(value)).into_bytes(),
        )),
    }
}

fn prepare(store: &Store, table: &str) -> AcrossAsk {
    let (namespace, database, _) = ids(store, table);
    AcrossAsk::Prepare {
        transaction: TRANSACTION,
        coordinator: Reach::Database(namespace, database),
        seen: tessari_storage::Store::committed_tail(
            store,
            store.own_log(Reach::Database(namespace, database)).unwrap(),
        )
        .unwrap(),
        writes: vec![write(store, table, "new")],
    }
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

fn note(store: &Store) -> Value {
    let (namespace, database, table) = ids(store, "notes");
    let held = store
        .begin()
        .unwrap()
        .get(&RecordAddress::new(
            namespace,
            database,
            table,
            RecordId::Int(1),
        ))
        .unwrap()
        .unwrap();
    tessari_encoding::decode_payload(&held).unwrap()
}

fn record(store: &Store, decision: Decision) -> TransactionRecord {
    let (namespace, database, _) = ids(store, "notes");
    TransactionRecord {
        decision,
        deadline: 0,
        participants: vec![Participant {
            range: Reach::Database(namespace, database),
            prepared_at: Some(Sequence::new(1)),
        }],
    }
}

#[test]
fn the_three_records_commit_a_write_only_once_decided() {
    let store = store();
    let mut owner = signed_in(&store, "root");
    let prepared = owner.answer_across(&prepare(&store, "notes")).unwrap();
    assert!(matches!(prepared, AcrossAnswer::Prepared(_)));
    assert_eq!(note(&store), Value::from("old"), "an intent is not a value");
    for decision in [Decision::Pending, Decision::Committed] {
        let decided = owner
            .answer_across(&AcrossAsk::Decide {
                transaction: TRANSACTION,
                record: record(&store, decision),
            })
            .unwrap();
        assert!(matches!(decided, AcrossAnswer::Decided(_)));
    }
    let (namespace, database, table) = ids(&store, "notes");
    let resolving = AcrossAsk::Resolve {
        transaction: TRANSACTION,
        committed: true,
        records: vec![RecordAddress::new(
            namespace,
            database,
            table,
            RecordId::Int(1),
        )],
    };
    assert!(matches!(
        owner.answer_across(&resolving).unwrap(),
        AcrossAnswer::Resolved(Some(_))
    ));
    assert_eq!(note(&store), Value::from("new"));
    assert_eq!(
        owner.answer_across(&resolving).unwrap(),
        AcrossAnswer::Resolved(None),
        "a second resolution finds nothing left"
    );
}

#[test]
fn a_user_without_the_write_grant_cannot_prepare() {
    let store = store();
    let refused = signed_in(&store, "reader").answer_across(&prepare(&store, "notes"));
    assert!(
        matches!(refused, Err(Error::NotGranted { .. })),
        "{refused:?}"
    );
    assert_eq!(note(&store), Value::from("old"));
}

#[test]
fn nobody_signed_in_cannot_prepare_on_a_closed_store() {
    let store = store();
    let refused = Session::new(&store).answer_across(&prepare(&store, "notes"));
    assert!(
        matches!(refused, Err(Error::NotSignedIn { .. })),
        "{refused:?}"
    );
}

#[test]
fn a_space_is_not_written_across_leaders() {
    let store = store();
    let refused = signed_in(&store, "root").answer_across(&prepare(&store, "cache"));
    assert!(
        matches!(refused, Err(Error::AcrossKind { .. })),
        "{refused:?}"
    );
}
