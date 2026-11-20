//! The pool admission gate: a declared pool bounds the scripts its
//! classifier names, refusing the next one `PoolAtCapacity` by name (SG3 W3).
//!
//! The gate is exercised with a slot taken directly on the store — the way a
//! concurrent in-flight script holds one — so the capacity edge is tested on
//! the guard's own seam rather than by generating the load the guard exists
//! to refuse.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Now, RunNow, Session};
use tessari_storage::Store;
use tessari_types::{Number, RefusalClass, Value};

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// An owned namespace with a table and two users: `root` (the owner) and
/// `ada` (an editor the pools classify).
fn owned(store: &Store) -> Session<'_> {
    // The first user closes the open store, so `ada` is declared by `root`
    // in the second script, the way a scoped user is declared in practice.
    Session::new(store)
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE work; USE DATABASE work;
             DEFINE TABLE orders SCHEMALESS;
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).now().unwrap();
    root.run_now(
        "USE NAMESPACE prod; USE DATABASE work;
         DEFINE USER ada ON prod.work ROLE editor PASSWORD 'correct horse battery';",
    )
    .unwrap();
    root
}

/// The editor, signed in and standing in the namespace's work database.
fn ada(store: &Store) -> Session<'_> {
    let mut ada = Session::new(store);
    ada.sign_in("ada", PASSWORD).now().unwrap();
    ada.run_now("USE NAMESPACE prod; USE DATABASE work;")
        .unwrap();
    ada
}

fn object(value: &Value) -> &BTreeMap<String, Value> {
    let Value::Object(fields) = value else {
        panic!("not an object: {value:?}");
    };
    fields
}

fn integer(fields: &BTreeMap<String, Value>, name: &str) -> i64 {
    match fields.get(name) {
        Some(Value::Number(Number::Integer(held))) => *held,
        other => panic!("`{name}` is not an integer: {other:?}"),
    }
}

#[test]
fn a_bounded_user_is_refused_by_name_while_a_slot_stands_taken() {
    let store = store();
    let mut root = owned(&store);
    root.run_now("DEFINE POOL web WITH MAX CONCURRENT 1 FOR USER ada FOR READS;")
        .unwrap();
    let mut ada = ada(&store);

    // A concurrent in-flight script holds the pool's only slot.
    let held = store
        .pool_take("prod", "web", 1)
        .expect("the pool's only slot");
    let refused = ada
        .run_now("SELECT * FROM orders;")
        .expect_err("a bounded read at capacity ran");
    assert_eq!(refused.class(), RefusalClass::Conflict);
    match refused {
        tessari_session::Error::PoolAtCapacity { pool, .. } => {
            assert_eq!(pool, "prod:web", "the refusal names the pool");
        }
        other => panic!("expected PoolAtCapacity, got {other:?}"),
    }

    // The classifier's other clauses do not match: the owner is another
    // user, so the same read is admitted while ada is refused.
    root.run_now("SELECT * FROM orders;").unwrap();
    // A write is another family: the pool bounds READS, so ada's write is
    // admitted beside her refused read.
    ada.run_now("CREATE orders:1 = { placed: true };").unwrap();

    drop(held);
    ada.run_now("SELECT * FROM orders;").unwrap();
}

#[test]
fn a_definitions_pool_bounds_ddl_and_not_the_reads_beside_it() {
    let store = store();
    let mut root = owned(&store);
    root.run_now("DEFINE POOL schema WITH MAX CONCURRENT 1 FOR USER ada FOR DEFINITIONS;")
        .unwrap();
    let mut ada = ada(&store);

    let held = store
        .pool_take("prod", "schema", 1)
        .expect("the pool's only slot");
    let refused = ada
        .run_now("DEFINE TABLE notes SCHEMALESS;")
        .expect_err("a bounded definition at capacity ran");
    match refused {
        tessari_session::Error::PoolAtCapacity { pool, .. } => {
            assert_eq!(pool, "prod:schema");
        }
        other => panic!("expected PoolAtCapacity, got {other:?}"),
    }
    // The read family is not named by this pool.
    ada.run_now("SELECT * FROM orders;").unwrap();
    drop(held);
    ada.run_now("DEFINE TABLE notes SCHEMALESS;").unwrap();
}

#[test]
fn a_database_clause_bounds_only_the_database_it_names() {
    let store = store();
    let mut root = owned(&store);
    root.run_now("DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders SCHEMALESS; USE DATABASE work;
                  DEFINE POOL web WITH MAX CONCURRENT 1 FOR DATABASE shop FOR READS;")
        .unwrap();
    let mut ada = ada(&store);

    let held = store
        .pool_take("prod", "web", 1)
        .expect("the pool's only slot");
    // ada stands in `work`: the pool names another database.
    ada.run_now("SELECT * FROM orders;").unwrap();
    let mut shopping = Session::new(&store);
    shopping.sign_in("root", PASSWORD).now().unwrap();
    shopping
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let refused = shopping
        .run_now("SELECT * FROM orders;")
        .expect_err("a read in the named database at capacity ran");
    match refused {
        tessari_session::Error::PoolAtCapacity { pool, .. } => assert_eq!(pool, "prod:web"),
        other => panic!("expected PoolAtCapacity, got {other:?}"),
    }
    drop(held);
    shopping.run_now("SELECT * FROM orders;").unwrap();
}

#[test]
fn no_namespace_selected_or_no_pool_declared_admits_without_the_gate() {
    let store = store();
    let mut root = owned(&store);
    root.run_now("DEFINE POOL web WITH MAX CONCURRENT 1 FOR USER ada FOR READS;")
        .unwrap();
    // A signed-in session that selected no namespace is not classified —
    // nothing it can run matches a namespace-scoped pool.
    let mut stray = Session::new(&store);
    stray.sign_in("root", PASSWORD).now().unwrap();
    stray.run_now("INFO FOR STORE;").unwrap();
    // And a namespace with no declared pools runs in its default pool, which
    // names no limit.
    let mut plain = Session::new(&store);
    plain.sign_in("root", PASSWORD).now().unwrap();
    plain
        .run_now("DEFINE NAMESPACE solo; USE NAMESPACE solo; DEFINE DATABASE app; USE DATABASE app;
                  DEFINE TABLE orders SCHEMALESS; CREATE orders:1 = { placed: true }; SELECT * FROM orders;")
        .unwrap();
}

#[test]
fn info_for_pool_reports_the_catalog_row_with_this_nodes_counters() {
    let store = store();
    let mut root = owned(&store);
    root.run_now("DEFINE POOL web WITH MAX CONCURRENT 1 FOR USER ada FOR READS;")
        .unwrap();
    let mut ada = ada(&store);

    let held = store
        .pool_take("prod", "web", 1)
        .expect("the pool's only slot");
    let _refused = ada
        .run_now("SELECT * FROM orders;")
        .expect_err("a bounded read at capacity ran");

    let answer = root.run_now("INFO FOR POOL web;").unwrap();
    let [tessari_session::Outcome::Value(value)] = answer.as_slice() else {
        panic!("INFO FOR POOL did not answer one value: {answer:?}");
    };
    let fields = object(value);
    assert_eq!(fields.get("name"), Some(&Value::from("web")));
    assert_eq!(integer(fields, "max_concurrent"), 1);
    assert_eq!(fields.get("database"), Some(&Value::Null));
    assert_eq!(fields.get("user"), Some(&Value::from("ada")));
    assert_eq!(fields.get("reads"), Some(&Value::Bool(true)));
    assert_eq!(fields.get("writes"), Some(&Value::Bool(false)));
    assert_eq!(fields.get("definitions"), Some(&Value::Bool(false)));
    assert_eq!(integer(fields, "running"), 1, "the held slot is counted");
    assert_eq!(integer(fields, "refused"), 1, "the refusal is counted");

    // The counters are node-local facts, stated as such.
    drop(held);
    let answer = root.run_now("INFO FOR POOL web;").unwrap();
    let [tessari_session::Outcome::Value(value)] = answer.as_slice() else {
        panic!("INFO FOR POOL did not answer one value: {answer:?}");
    };
    assert_eq!(integer(object(value), "running"), 0);
    assert_eq!(integer(object(value), "refused"), 1);
}

#[test]
fn info_for_pools_lists_what_the_namespace_holds_and_bounds_who_asks() {
    let store = store();
    let mut root = owned(&store);
    root.run_now(
        "DEFINE POOL web WITH MAX CONCURRENT 1 FOR USER ada FOR READS;
         DEFINE POOL api WITH MAX CONCURRENT 4;",
    )
    .unwrap();

    let answer = root.run_now("INFO FOR POOLS;").unwrap();
    let [tessari_session::Outcome::Value(Value::Array(held))] = answer.as_slice() else {
        panic!("INFO FOR POOLS did not answer one array: {answer:?}");
    };
    assert_eq!(held.len(), 2);
    let names: Vec<_> = held
        .iter()
        .map(|row| object(row).get("name").cloned().unwrap())
        .collect();
    assert_eq!(
        names,
        [Value::from("api"), Value::from("web")],
        "listed by name, the deterministic order"
    );

    // The pool rows are namespace-admin surface, the way declaring one is:
    // a non-administrator of the namespace is refused the listing.
    let mut ada = ada(&store);
    let refused = ada
        .run_now("INFO FOR POOLS;")
        .expect_err("a non-administrator read the pool rows");
    assert_eq!(refused.class(), RefusalClass::Forbidden);

    // An unknown name is an unknown pool, not an empty answer.
    let missing = root.run_now("INFO FOR POOL absent;");
    assert!(missing.is_err(), "an unknown pool answered: {missing:?}");
}
