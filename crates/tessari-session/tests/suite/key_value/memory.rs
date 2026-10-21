//! A space whose data lives in node memory only (G079 W5b, ADR-0130).
//!
//! The property each case asserts beside the answers is the one a reader of the
//! answers cannot see: nothing the space holds reaches the backend the store was
//! opened on. Every pair the backend holds is compared before and after, so a
//! record, an expiry entry, a count or a log entry landing there fails the case.

use std::sync::Arc;

use tessari_kv::{KeyRange, Keyspace, KvBackend, ScanDirection, ScanRequest};
use tessari_lsm::{Durability, StoreConfig, TessariKvBackend};
use tessari_session::{Now, Outcome, RunNow, Session};
use tessari_storage::Store;
use tessari_types::Value;

use super::{PAST_SHORT, SHORT, keys, on_each_backend, opened, refused, run, value};

/// Every pair `raw` holds, keyspace by keyspace.
fn held(raw: &Arc<dyn KvBackend>) -> Vec<(Keyspace, Vec<u8>, Vec<u8>)> {
    let mut all = Vec::new();
    for keyspace in Keyspace::ALL {
        let pairs = raw
            .scan(&ScanRequest {
                keyspace: *keyspace,
                range: KeyRange::prefix(&[]),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .now()
            .unwrap();
        for (key, stored) in pairs {
            all.push((
                *keyspace,
                key.as_slice().to_vec(),
                stored.as_slice().to_vec(),
            ));
        }
    }
    all
}

#[test]
fn an_in_memory_space_answers_as_a_space_and_writes_nothing_beneath() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 100;");
        let before = held(&backend.raw);
        run(&mut session, "SET hot:'a' = 1;");
        assert_eq!(
            value(&mut session, "INCR hot:'a' BY 2;"),
            Value::from(3),
            "{}",
            backend.name
        );
        assert_eq!(value(&mut session, "GET hot:'a';"), Value::from(3));
        assert_eq!(
            value(&mut session, "SET hot:'b' = 'x' IF ABSENT;"),
            Value::Bool(true)
        );
        assert_eq!(
            value(&mut session, "SET hot:'b' = 'y' IF ABSENT;"),
            Value::Bool(false)
        );
        run(&mut session, "SET hot:'c' = 'z' EXPIRE 1h;");
        assert!(matches!(
            value(&mut session, "RETURN TTL hot:'c';"),
            Value::Duration(_)
        ));
        assert_eq!(keys(&mut session, "KEYS FROM hot;"), 3);
        run(&mut session, "DEL hot:'c';");
        assert_eq!(keys(&mut session, "KEYS FROM hot;"), 2);
        assert_eq!(
            held(&backend.raw),
            before,
            "{}: the space's writes reached the store's backend",
            backend.name
        );
    });
}

#[test]
fn an_in_memory_space_is_bounded_and_evicts_in_memory() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 2;");
        run(&mut session, "SET hot:'a' = 1;");
        run(&mut session, "SET hot:'b' = 2;");
        run(&mut session, "SET hot:'c' = 3;");
        assert_eq!(keys(&mut session, "KEYS FROM hot;"), 2, "{}", backend.name);
        assert_eq!(value(&mut session, "GET hot:'a';"), Value::None);
        let why = refused(&mut session, "DEFINE SPACE loose IN MEMORY;");
        assert!(why.contains("MAX"), "{why}");
    });
}

#[test]
fn a_transaction_writing_memory_beside_durable_data_is_refused_whole() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        let why = refused(
            &mut session,
            "BEGIN; SET hot:'a' = 1; SET cache:'b' = 1; COMMIT;",
        );
        assert!(why.contains("in memory"), "{}: {why}", backend.name);
        assert_eq!(value(&mut session, "GET hot:'a';"), Value::None);
        assert_eq!(value(&mut session, "GET cache:'b';"), Value::None);
        // Each side on its own commits.
        run(
            &mut session,
            "BEGIN; SET hot:'a' = 1; INCR hot:'a'; COMMIT;",
        );
        assert_eq!(value(&mut session, "GET hot:'a';"), Value::from(2));
    });
}

#[test]
fn an_expired_key_of_an_in_memory_space_is_removed_by_the_removal_pass() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        run(&mut session, &format!("SET hot:'a' = 1 EXPIRE {SHORT};"));
        std::thread::sleep(PAST_SHORT);
        let lapsed = backend.store.remove_expired().now().unwrap();
        assert_eq!(lapsed.records, 1, "{}", backend.name);
        assert_eq!(keys(&mut session, "KEYS FROM hot;"), 0);
    });
}

#[test]
fn a_script_backup_carries_the_definition_and_none_of_the_data() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE SPACE hot IN MEMORY MAX 10 EVICT NONE;",
        );
        run(&mut session, "SET hot:'a' = 'only-in-memory';");
        run(
            &mut session,
            "DEFINE USER root ROLE owner PASSWORD 'backup-owner-pw';",
        );
        let mut owner = Session::new(&backend.store);
        owner.sign_in("root", "backup-owner-pw").now().unwrap();
        owner
            .run_now("USE NAMESPACE prod; USE DATABASE app;")
            .unwrap();
        let answered = owner.run_now("BACKUP SCRIPT;").unwrap();
        let script = match answered.last() {
            Some(Outcome::Value(Value::String(script))) => script.clone(),
            _ => String::new(),
        };
        assert!(
            script.contains("DEFINE SPACE hot IN MEMORY MAX 10 EVICT NONE"),
            "{}: {script}",
            backend.name
        );
        assert!(!script.contains("only-in-memory"), "{script}");
        // A state snapshot too, with a durable key as the control that the
        // bytes would show a value had it been carried.
        run(&mut owner, "SET cache:'b' = 'durable-control';");
        let answered = owner.run_now("BACKUP STATE;").unwrap();
        let snapshot = match answered.last() {
            Some(Outcome::Value(Value::Bytes(bytes))) => bytes.clone(),
            _ => Vec::new(),
        };
        let carries = |needle: &[u8]| snapshot.windows(needle.len()).any(|seen| seen == needle);
        assert!(carries(b"durable-control"), "{}: control", backend.name);
        assert!(!carries(b"only-in-memory"), "{}", backend.name);
    });
}

#[test]
fn a_restart_empties_an_in_memory_space_and_keeps_its_definition() {
    let directory = tempfile::tempdir().unwrap();
    let open = || {
        let raw = Arc::new(
            TessariKvBackend::open(
                directory.path(),
                StoreConfig::new(Durability::ProcessCrashSafe),
            )
            .now()
            .unwrap(),
        ) as Arc<dyn KvBackend>;
        Store::open(raw).now().unwrap()
    };
    {
        let store = open();
        let mut session = opened(&store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        run(&mut session, "SET hot:'a' = 1;");
    }
    let store = open();
    let mut session = Session::new(&store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    assert_eq!(value(&mut session, "GET hot:'a';"), Value::None);
    run(&mut session, "SET hot:'b' = 1;");
    run(&mut session, "SET hot:'c' = 1;");
    run(&mut session, "SET hot:'d' = 1;");
    assert_eq!(keys(&mut session, "KEYS FROM hot;"), 3);
}

#[test]
fn a_node_with_no_leadership_refuses_a_held_write_as_it_refuses_a_durable_one() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        // A peer bound to another node that may write: this node is clustered
        // and holds no leadership, so it writes nothing until it is granted one.
        session
            .run_now(&format!(
                "DEFINE REPLICA second AT 'there:9001' NODE '{}' ROLES serving, writable;",
                "ab".repeat(16)
            ))
            .unwrap();
        let held = refused(&mut session, "SET hot:'a' = 1;");
        let durable = refused(&mut session, "SET cache:'a' = 1;");
        assert!(
            held.contains("holds no leadership"),
            "{}: {held}",
            backend.name
        );
        assert_eq!(held, durable, "{}", backend.name);
    });
}

/// The keys node memory holds for the held spaces.
fn held_keys(store: &Store) -> usize {
    store.held_keys().now().unwrap()
}

#[test]
fn dropping_an_in_memory_space_frees_its_keys_and_no_other() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE warm IN MEMORY MAX 10;");
        run(&mut session, "SET warm:'kept' = 1;");
        let warm = held_keys(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        run(&mut session, "SET hot:'a' = 1;");
        run(&mut session, "INCR hot:'a';");
        run(&mut session, "SET hot:'b' = 'x' EXPIRE 1h;");
        // Control: the space's records, expiry entry and count are in memory.
        assert!(held_keys(&backend.store) > warm, "{}", backend.name);
        run(&mut session, "DROP SPACE hot;");
        assert_eq!(
            held_keys(&backend.store),
            warm,
            "{}: the dropped space's keys stayed in memory",
            backend.name
        );
        assert_eq!(value(&mut session, "GET warm:'kept';"), Value::from(1));
        // A space redefined under the name is a new one and starts empty.
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        assert_eq!(keys(&mut session, "KEYS FROM hot;"), 0);
    });
}

#[test]
fn a_reader_older_than_the_drop_keeps_the_space_until_it_ends() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        run(&mut session, "SET hot:'a' = 1;");
        let held = held_keys(&backend.store);
        // A reader whose snapshot is older than the drop: what it may read
        // stays until it ends.
        let reader = backend.store.begin().now().unwrap();
        run(&mut session, "DROP SPACE hot;");
        assert_eq!(
            held_keys(&backend.store),
            held,
            "{}: the space left memory under a reader that can still see it",
            backend.name
        );
        drop(reader);
        // The housekeeping pass releases what the reader held back.
        backend.store.remove_expired().now().unwrap();
        assert_eq!(held_keys(&backend.store), 0, "{}", backend.name);
    });
}

#[test]
fn a_cancelled_drop_of_an_in_memory_space_frees_nothing() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        run(&mut session, "SET hot:'a' = 1;");
        let held = held_keys(&backend.store);
        run(&mut session, "BEGIN; DROP SPACE hot; CANCEL;");
        backend.store.remove_expired().now().unwrap();
        assert_eq!(held_keys(&backend.store), held, "{}", backend.name);
        assert_eq!(value(&mut session, "GET hot:'a';"), Value::from(1));
        run(&mut session, "SET hot:'b' = 2;");
        assert_eq!(keys(&mut session, "KEYS FROM hot;"), 2);
    });
}

/// A store holding a space in memory in each of two namespaces, then put in a
/// cluster where it leads `prod` and a peer leads `other` (G079 W5d).
fn leading_one_of_two() -> Store {
    use tessari_encoding::Roles;
    use tessari_storage::{Catalog, LEASE_TTL, Lease, Reach};
    use tessari_types::{Epoch, NamespaceId};
    let store = Store::open(Arc::new(tessari_kv::MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE SPACE hot IN MEMORY MAX 10; SET hot:'a' = 1;\n\
         DEFINE NAMESPACE other; USE NAMESPACE other; DEFINE DATABASE app; USE DATABASE app;\n\
         DEFINE SPACE hot IN MEMORY MAX 10; SET hot:'a' = 1; DEFINE SPACE cold; SET cold:'a' = 1;\n\
         DEFINE NAMESPACE many MULTI MASTER; USE NAMESPACE many; DEFINE DATABASE app;\n\
         USE DATABASE app; DEFINE SPACE hot IN MEMORY MAX 10; SET hot:'a' = 1;",
    );
    let me = store.node_identity().now().unwrap().id;
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica(
            "second",
            "10.0.0.2:9081",
            Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            Some([9; 16]),
            None,
            None,
        )
        .now()
        .unwrap();
    catalog
        .record_leadership(Reach::Namespace(NamespaceId::new(1)), me, Epoch::new(4))
        .unwrap();
    catalog
        .record_leadership(
            Reach::Namespace(NamespaceId::new(2)),
            [9; 16],
            Epoch::new(7),
        )
        .unwrap();
    catalog
        .record_leadership(Reach::Namespace(NamespaceId::new(3)), me, Epoch::new(4))
        .unwrap();
    transaction.commit().now().unwrap();
    store
        .hold(
            Epoch::new(4),
            Lease::taken_at(std::time::Instant::now(), LEASE_TTL),
        )
        .now();
    store
}

#[test]
fn on_a_cluster_the_leader_of_its_range_writes_an_in_memory_space() {
    let store = leading_one_of_two();
    let mut session = Session::new(&store);
    run(&mut session, "USE NAMESPACE prod; USE DATABASE app;");
    run(&mut session, "SET hot:'b' = 2;");
    assert_eq!(value(&mut session, "GET hot:'b';"), Value::from(2));
}

#[test]
fn on_a_cluster_a_node_that_does_not_lead_the_range_redirects_the_write() {
    let store = leading_one_of_two();
    let mut session = Session::new(&store);
    run(&mut session, "USE NAMESPACE other; USE DATABASE app;");
    let why = refused(&mut session, "SET hot:'b' = 2;");
    assert!(
        why.contains("led by another node") && why.contains("10.0.0.2:9081"),
        "{why}"
    );
}

#[test]
fn on_a_cluster_a_namespace_two_nodes_may_write_refuses_an_in_memory_space() {
    let store = leading_one_of_two();
    let mut session = Session::new(&store);
    run(&mut session, "USE NAMESPACE many; USE DATABASE app;");
    let why = refused(&mut session, "SET hot:'b' = 2;");
    assert!(
        why.contains("held in memory") && why.contains("MULTI MASTER"),
        "{why}"
    );
}

#[test]
fn on_a_cluster_a_node_that_does_not_lead_the_range_redirects_a_read() {
    let store = leading_one_of_two();
    let mut session = Session::new(&store);
    run(&mut session, "USE NAMESPACE other; USE DATABASE app;");
    // This node holds none of the space: an empty answer here would pass for an
    // empty space.
    for read in ["GET hot:'a';", "KEYS FROM hot;", "RETURN TTL hot:'a';"] {
        let why = refused(&mut session, read);
        assert!(
            why.contains("held in memory") && why.contains("10.0.0.2:9081"),
            "{read}: {why}"
        );
    }
    // Controls: a durable space here is read here, as a follower reads, and
    // the leader answers its own held space.
    assert_eq!(value(&mut session, "GET cold:'a';"), Value::from(1));
    run(&mut session, "USE NAMESPACE prod; USE DATABASE app;");
    assert_eq!(value(&mut session, "GET hot:'a';"), Value::from(1));
}
