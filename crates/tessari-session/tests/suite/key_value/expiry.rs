//! A key that stops being answered at a stated instant (G035 S1.2, S1.3).
//!
//! The rule under test is the kill criterion of the goal: once the instant has
//! passed, **no** read path of the table answers with the record — a point read,
//! a `KEYS`, a table scan, a point `SELECT` and an index-served `SELECT`. Each
//! case asserts the same read before the instant as a control, because a read
//! that never answered would pass the "after" half on its own.

use std::thread;

use tessari_encoding::KeyKind;
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::Value;

use super::{Backend, PAST_SHORT, SHORT, keys, on_each_backend, opened, refused, rows, run, value};

#[test]
fn an_expired_key_is_answered_by_no_read_path_of_its_table() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TABLE items SCHEMALESS; DEFINE INDEX by_n ON items FIELDS n;",
        );
        run(
            &mut session,
            &format!("SET items:1 = {{ n: 7 }} EXPIRE {SHORT};"),
        );
        run(&mut session, "SET items:2 = { n: 7 };");

        let reads = |session: &mut tessari_session::Session<'_>| {
            (
                value(session, "GET items:1;") != Value::None,
                keys(session, "KEYS FROM items;"),
                rows(session, "SELECT * FROM items;"),
                rows(session, "SELECT * FROM items:1;"),
                rows(session, "SELECT * FROM items WHERE n = 7;"),
            )
        };
        assert_eq!(
            reads(&mut session),
            (true, 2, 2, 1, 2),
            "{}: before the instant every path answers",
            backend.name
        );
        thread::sleep(PAST_SHORT);
        assert_eq!(
            reads(&mut session),
            (false, 1, 1, 0, 1),
            "{}: after it none does, and the key without an expiry is untouched",
            backend.name
        );
    });
}

#[test]
fn ttl_answers_the_time_left_null_for_never_and_none_for_no_key() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "SET cache:'forever' = 1;");
        run(&mut session, "SET cache:'brief' = 1 EXPIRE 1h;");
        assert_eq!(
            value(&mut session, "RETURN TTL cache:'forever';"),
            Value::Null
        );
        assert_eq!(
            value(&mut session, "RETURN TTL cache:'missing';"),
            Value::None
        );
        let Value::Duration(left) = value(&mut session, "RETURN TTL cache:'brief';") else {
            panic!("{}: TTL of an expiring key is a duration", backend.name);
        };
        assert!(
            left.seconds() > 3_500 && left.seconds() <= 3_600,
            "{}: {left}",
            backend.name
        );
    });
}

#[test]
fn a_plain_set_clears_an_expiry_the_key_had() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, &format!("SET cache:'k' = 1 EXPIRE {SHORT};"));
        run(&mut session, "SET cache:'k' = 2;");
        assert_eq!(value(&mut session, "RETURN TTL cache:'k';"), Value::Null);
        thread::sleep(PAST_SHORT);
        assert_eq!(
            value(&mut session, "GET cache:'k';"),
            Value::from(2),
            "{}",
            backend.name
        );
    });
}

#[test]
fn expire_sets_an_expiry_and_answers_whether_the_key_was_there() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        assert_eq!(
            value(&mut session, "EXPIRE cache:'missing' 10s;"),
            Value::Bool(false)
        );
        run(&mut session, "SET cache:'k' = 'v';");
        assert_eq!(
            value(&mut session, &format!("EXPIRE cache:'k' {SHORT};")),
            Value::Bool(true)
        );
        assert!(matches!(
            value(&mut session, "RETURN TTL cache:'k';"),
            Value::Duration(_)
        ));
        assert_eq!(value(&mut session, "GET cache:'k';"), Value::from("v"));
        thread::sleep(PAST_SHORT);
        assert_eq!(
            value(&mut session, "GET cache:'k';"),
            Value::None,
            "{}",
            backend.name
        );
    });
}

#[test]
fn an_expire_that_is_already_past_removes_the_key() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "SET cache:'a' = 1; SET cache:'b' = 1;");
        assert_eq!(
            value(&mut session, "EXPIRE cache:'a' -1s;"),
            Value::Bool(true)
        );
        assert_eq!(
            value(&mut session, "EXPIRE cache:'b' 0s;"),
            Value::Bool(true)
        );
        assert_eq!(
            keys(&mut session, "KEYS FROM cache;"),
            0,
            "{}",
            backend.name
        );
    });
}

#[test]
fn persist_clears_an_expiry_and_answers_whether_there_was_one() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, &format!("SET cache:'k' = 1 EXPIRE {SHORT};"));
        assert_eq!(value(&mut session, "PERSIST cache:'k';"), Value::Bool(true));
        assert_eq!(
            value(&mut session, "PERSIST cache:'k';"),
            Value::Bool(false)
        );
        assert_eq!(
            value(&mut session, "PERSIST cache:'none';"),
            Value::Bool(false)
        );
        thread::sleep(PAST_SHORT);
        assert_eq!(
            value(&mut session, "GET cache:'k';"),
            Value::from(1),
            "{}",
            backend.name
        );
    });
}

#[test]
fn a_set_whose_expiry_is_not_in_the_future_is_refused_and_writes_nothing() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let why = refused(&mut session, "SET cache:'k' = 1 EXPIRE 0s;");
        assert!(
            why.contains("must be in the future"),
            "{}: {why}",
            backend.name
        );
        let why = refused(&mut session, "SET cache:'k' = 1 EXPIRE 'soon';");
        assert!(why.contains("a duration or a datetime"), "{why}");
        assert_eq!(value(&mut session, "GET cache:'k';"), Value::None);
    });
}

#[test]
fn a_transaction_sees_its_own_expiring_write_on_its_own_clock() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let answers = session
            .run(
                "BEGIN; SET cache:'t' = 1 EXPIRE 1h; SET cache:'gone' = 1; \
                 EXPIRE cache:'gone' -5s; RETURN [GET cache:'t', GET cache:'gone']; COMMIT;",
            )
            .unwrap();
        let seen = answers
            .iter()
            .find_map(|outcome| match outcome {
                tessari_session::Outcome::Value(Value::Array(pair)) => Some(pair.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(seen, vec![Value::from(1), Value::None], "{}", backend.name);
        assert!(matches!(
            value(&mut session, "RETURN TTL cache:'t';"),
            Value::Duration(_)
        ));
    });
}

#[test]
fn the_words_stay_usable_as_field_names() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TABLE t SCHEMALESS;");
        run(
            &mut session,
            "CREATE t:1 = { ttl: 5, expire: 6, persist: 7 };",
        );
        assert_eq!(
            rows(
                &mut session,
                "SELECT ttl, expire, persist FROM t WHERE ttl = 5;"
            ),
            1,
            "{}",
            backend.name
        );
    });
}

/// Every entry the one secondary index of a fresh store holds.
fn index_entries(backend: &Backend) -> usize {
    backend
        .raw
        .scan(&ScanRequest {
            keyspace: KeyKind::SecondaryIndex.keyspace(),
            range: KeyRange::prefix(&[KeyKind::SecondaryIndex.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap()
        .len()
}

/// The derived structure asserted on its own keys: an index read confirms every
/// candidate against the record, so an entry left behind is invisible from any
/// answer and only a count of the entries themselves can see it.
#[test]
fn overwriting_or_deleting_an_expired_record_leaves_no_index_entry_behind() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TABLE items SCHEMALESS; DEFINE INDEX by_n ON items FIELDS n;",
        );
        run(
            &mut session,
            &format!("SET items:1 = {{ n: 1 }} EXPIRE {SHORT};"),
        );
        run(
            &mut session,
            &format!("SET items:2 = {{ n: 2 }} EXPIRE {SHORT};"),
        );
        assert_eq!(index_entries(backend), 2, "{}: control", backend.name);
        thread::sleep(PAST_SHORT);
        run(&mut session, "SET items:1 = { n: 3 };");
        run(&mut session, "DEL items:2;");
        assert_eq!(
            index_entries(backend),
            1,
            "{}: only the overwrite's own entry remains",
            backend.name
        );
    });
}

#[test]
fn an_expiry_survives_the_store_being_closed_and_opened_again() {
    let directory = tempfile::tempdir().unwrap();
    let open = || {
        let raw = std::sync::Arc::new(
            tessari_lsm::LsmBackend::open(
                directory.path(),
                tessari_lsm::StoreConfig::new(tessari_lsm::Durability::ProcessCrashSafe),
            )
            .unwrap(),
        ) as std::sync::Arc<dyn tessari_kv::KvBackend>;
        tessari_storage::Store::open(raw).unwrap()
    };
    {
        let store = open();
        let mut session = opened(&store);
        run(&mut session, "SET cache:'k' = 1 EXPIRE 1h;");
    }
    let store = open();
    let mut session = tessari_session::Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    assert!(matches!(
        value(&mut session, "RETURN TTL cache:'k';"),
        Value::Duration(_)
    ));
}
