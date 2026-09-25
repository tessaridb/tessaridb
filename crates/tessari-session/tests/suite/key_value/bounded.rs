//! A space with a limit (G036).

use std::collections::VecDeque;
use std::thread;

use tessari_encoding::{KeyKind, ModifiedKey, StoreKey};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_session::{Outcome, Session};
use tessari_types::{RecordId, Value};

use super::{Backend, on_each_backend, opened, refused, run, value};

fn keys_of(session: &mut Session<'_>, space: &str) -> Vec<RecordId> {
    match run(session, &format!("KEYS FROM {space};")) {
        Outcome::Keys(keys) => keys,
        other => panic!("KEYS answered {other:?}"),
    }
}

fn definition(session: &mut Session<'_>, table: &str) -> String {
    let Value::Object(fields) = value(session, &format!("INFO FOR TABLE {table};")) else {
        panic!("INFO FOR TABLE answered no object");
    };
    match fields.get("definition") {
        Some(Value::String(text)) => text.clone(),
        other => panic!("no definition: {other:?}"),
    }
}

/// Every entry of the modified-order index, as the record ids it names.
fn indexed(backend: &Backend) -> Vec<RecordId> {
    backend
        .raw
        .scan(&ScanRequest {
            keyspace: KeyKind::ModifiedOrder.keyspace(),
            range: KeyRange::prefix(&[KeyKind::ModifiedOrder.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap()
        .into_iter()
        .map(|(key, _)| ModifiedKey::decode(key.as_slice()).unwrap().id)
        .collect()
}

#[test]
fn a_space_is_written_back_with_its_own_word_and_its_limit() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE SPACE capped MAX 3; DEFINE SPACE strict MAX 2 EVICT NONE;",
        );
        assert_eq!(
            definition(&mut session, "cache"),
            "DEFINE SPACE cache;\n",
            "{}",
            backend.name
        );
        assert_eq!(
            definition(&mut session, "capped"),
            "DEFINE SPACE capped MAX 3;\n"
        );
        assert_eq!(
            definition(&mut session, "strict"),
            "DEFINE SPACE strict MAX 2 EVICT NONE;\n"
        );
        let why = refused(&mut session, "DEFINE SPACE nothing MAX 0;");
        assert!(why.contains("above zero"), "{why}");
    });
}

/// S2.1 and S2.2 against an independent model: after every commit the space
/// holds exactly the `MAX` most recently written keys, and the modified-order
/// index names exactly those keys, oldest first.
#[test]
fn a_limited_space_keeps_the_most_recently_modified_keys() {
    const MAX: usize = 4;
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, &format!("DEFINE SPACE lru MAX {MAX};"));
        let mut model: VecDeque<String> = VecDeque::new();
        let mut roll: u64 = 0x2545_f491;
        for step in 0..60 {
            roll = roll
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let key = format!("k{}", (roll >> 33) % 9);
            run(&mut session, &format!("SET lru:'{key}' = {step};"));
            model.retain(|held| *held != key);
            model.push_back(key);
            while model.len() > MAX {
                model.pop_front();
            }
            let mut expected: Vec<RecordId> =
                model.iter().map(|k| RecordId::from(k.as_str())).collect();
            let oldest_first = expected.clone();
            expected.sort();
            assert_eq!(
                keys_of(&mut session, "lru"),
                expected,
                "{} step {step}",
                backend.name
            );
            assert_eq!(
                indexed(backend),
                oldest_first,
                "{} step {step}: the index",
                backend.name
            );
        }
        run(&mut session, "SET cache:'free' = 1;");
        assert_eq!(
            indexed(backend).len(),
            MAX,
            "{}: an unlimited space has no entries",
            backend.name
        );
    });
}

#[test]
fn a_commit_never_evicts_its_own_writes_and_refuses_to_outgrow_the_limit_alone() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE SPACE lru MAX 3; SET lru:'a' = 1; SET lru:'b' = 1; SET lru:'c' = 1;",
        );
        session
            .run("BEGIN; SET lru:'x' = 1; SET lru:'y' = 1; COMMIT;")
            .unwrap();
        assert_eq!(
            keys_of(&mut session, "lru"),
            vec![
                RecordId::from("c"),
                RecordId::from("x"),
                RecordId::from("y")
            ],
            "{}",
            backend.name
        );
        // The commit rewrites the space's oldest key and adds one: the oldest
        // ENTRY of the index is the key this commit writes, and it must be
        // passed over for the next one.
        session
            .run("BEGIN; SET lru:'c' = 2; SET lru:'z' = 1; COMMIT;")
            .unwrap();
        assert_eq!(
            keys_of(&mut session, "lru"),
            vec![
                RecordId::from("c"),
                RecordId::from("y"),
                RecordId::from("z")
            ],
            "{}: the rewritten oldest key survives its own commit",
            backend.name
        );
        let why = match session.run(
            "BEGIN; SET lru:'p' = 1; SET lru:'q' = 1; SET lru:'r' = 1; SET lru:'s' = 1; COMMIT;",
        ) {
            Err(why) => why.to_string(),
            Ok(outcome) => panic!(
                "{}: four new keys in a space of three committed: {outcome:?}",
                backend.name
            ),
        };
        assert!(why.contains("limit of 3"), "{why}");
        assert_eq!(keys_of(&mut session, "lru").len(), 3);
    });
}

#[test]
fn evict_none_refuses_the_key_past_the_limit_and_nothing_else() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE SPACE strict MAX 2 EVICT NONE; SET strict:'a' = 1; SET strict:'b' = 1;",
        );
        let why = refused(&mut session, "SET strict:'c' = 1;");
        assert!(
            why.contains("space strict holds its limit of 2"),
            "{}: {why}",
            backend.name
        );
        run(&mut session, "SET strict:'a' = 2;");
        run(&mut session, "DEL strict:'a';");
        run(&mut session, "SET strict:'c' = 1;");
        assert_eq!(
            keys_of(&mut session, "strict"),
            vec![RecordId::from("b"), RecordId::from("c")],
            "{}",
            backend.name
        );
    });
}

/// The write skew the limit is enforced in the commit to prevent: writers
/// adding different keys never conflict with each other.
#[test]
fn concurrent_writers_never_take_a_space_past_its_limit() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE SPACE lru MAX 5; DEFINE SPACE strict MAX 5 EVICT NONE;",
        );
        thread::scope(|scope| {
            for writer in 0..4 {
                let store = &backend.store;
                scope.spawn(move || {
                    let mut session = Session::new(store);
                    session
                        .run("USE NAMESPACE prod; USE DATABASE app;")
                        .unwrap();
                    for n in 0..25 {
                        let _ = session.run(&format!("SET lru:'w{writer}n{n}' = 1;"));
                        let _ = session.run(&format!("SET strict:'w{writer}n{n}' = 1;"));
                    }
                });
            }
        });
        assert_eq!(keys_of(&mut session, "lru").len(), 5, "{}", backend.name);
        assert_eq!(keys_of(&mut session, "strict").len(), 5, "{}", backend.name);
    });
}
