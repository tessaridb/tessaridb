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
fn a_node_beside_another_writer_refuses_to_write_an_in_memory_space() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE SPACE hot IN MEMORY MAX 10;");
        // A peer bound to another node that may write: this node would apply
        // that node's records, which is what a held space cannot sit beside.
        session
            .run_now(&format!(
                "DEFINE REPLICA second AT 'there:9001' NODE '{}' ROLES serving, writable;",
                "ab".repeat(16)
            ))
            .unwrap();
        let why = refused(&mut session, "SET hot:'a' = 1;");
        assert!(
            why.contains("held in memory") && why.contains("cluster"),
            "{}: {why}",
            backend.name
        );
    });
}
