//! An index-served read while a transaction across leaders is part-way on
//! this node (Q-919): its readers see a committed intent its indexes do not
//! hold yet, so the read takes the checked path for that table — and only
//! that table.

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{
    Decision, Mutation, Participant, RecordValue, StampedValue, TRANSACTION_ID_LEN, TransactionId,
    TransactionRecord, encode_payload,
};
use tessari_kv::MemoryBackend;
use tessari_session::{AcrossAsk, Session};
use tessari_storage::{Catalog, Reach, Store};
use tessari_types::{RecordId, RecordRef, Sequence, Value};

const TRANSACTION: TransactionId = TransactionId::new([9; TRANSACTION_ID_LEN]);
const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// A store set up by `script`, in `prod.shop`.
fn store(script: &str) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    Session::new(&store)
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; {USE} {script}"
        ))
        .unwrap();
    store
}

/// Write `value` as `table:id` in a transaction across leaders, prepared and
/// committed and not yet resolved.
fn committed_not_resolved(store: &Store, table: &str, id: RecordId, value: Value) {
    let mut session = Session::new(store);
    let mut reading = store.begin().unwrap();
    let catalog = Catalog::new(&mut reading);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, table)
        .unwrap()
        .unwrap();
    reading.rollback();
    let range = Reach::Database(namespace, database);
    let prepared = session
        .answer_across(&AcrossAsk::Prepare {
            transaction: TRANSACTION,
            coordinator: range,
            seen: store.committed_tail(store.own_log(range).unwrap()).unwrap(),
            writes: vec![Mutation {
                namespace,
                database,
                table,
                id,
                shard: None,
                value: StampedValue::new(RecordValue::Present(encode_payload(&value).into_bytes())),
            }],
        })
        .unwrap();
    let tessari_session::AcrossAnswer::Prepared(at) = prepared else {
        panic!("{prepared:?}");
    };
    for decision in [Decision::Pending, Decision::Committed] {
        session
            .answer_across(&AcrossAsk::Decide {
                transaction: TRANSACTION,
                record: TransactionRecord {
                    decision,
                    deadline: 0,
                    participants: vec![Participant {
                        range,
                        prepared_at: Some(Sequence::new(at.get())),
                    }],
                },
            })
            .unwrap();
    }
}

fn object(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn said(session: &mut Session<'_>, statement: &str) -> String {
    format!(
        "{:?}",
        session.run(&format!("{USE} {statement}")).unwrap().last()
    )
}

#[test]
fn a_committed_intent_readers_see_is_found_by_a_read_its_index_cannot_serve() {
    // `notes:1` and `other:1` hold `band: 'old'`, each table indexed on it,
    // and a transaction across leaders sets `notes:1.band` to `'new'`.
    let store = store(
        "DEFINE COLLECTION notes; DEFINE INDEX by_band ON notes FIELDS band; \
         DEFINE COLLECTION other; DEFINE INDEX by_band ON other FIELDS band; \
         CREATE notes:1 = { band: 'old' }; CREATE other:1 = { band: 'old' };",
    );
    committed_not_resolved(
        &store,
        "notes",
        RecordId::Int(1),
        object(&[("band", Value::from("new"))]),
    );
    let mut session = Session::new(&store);
    // Readers see the intent's value; the index still holds `'old'`, so a read
    // that believed it would answer nothing.
    let found = said(&mut session, "SELECT id FROM notes WHERE band = 'new';");
    assert!(found.contains("Int(1)"), "{found}");
    let gone = said(&mut session, "SELECT id FROM notes WHERE band = 'old';");
    assert!(!gone.contains("Int(1)"), "{gone}");
    // The table the transaction never touched keeps its index.
    let other = said(
        &mut session,
        "EXPLAIN SELECT id FROM other WHERE band = 'old';",
    );
    assert!(other.contains("by_band"), "{other}");
    let notes = said(
        &mut session,
        "EXPLAIN SELECT id FROM notes WHERE band = 'new';",
    );
    assert!(!notes.contains("by_band"), "{notes}");
}

#[test]
fn a_traversal_over_edges_part_way_here_is_refused_as_retriable() {
    let store = store(
        "DEFINE COLLECTION users; DEFINE TABLE follows EDGE; \
         CREATE users:1 = { handle: 'ada' }; CREATE users:2 = { handle: 'grace' };",
    );
    let users = {
        let mut reading = store.begin().unwrap();
        let catalog = Catalog::new(&mut reading);
        let namespace = catalog.namespace_id("prod").unwrap().unwrap();
        let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
        catalog
            .table_id(namespace, database, "users")
            .unwrap()
            .unwrap()
    };
    let endpoint = |id: i64| Value::Record(RecordRef::new(users, RecordId::Int(id)));
    committed_not_resolved(
        &store,
        "follows",
        RecordId::Int(7),
        object(&[("in", endpoint(1)), ("out", endpoint(2))]),
    );
    let mut session = Session::new(&store);
    let refused = session
        .run(&format!("{USE} SELECT * FROM users:1->follows->users;"))
        .unwrap_err();
    assert!(
        matches!(refused, tessari_session::Error::AcrossSettling { .. }),
        "{refused:?}"
    );
}
