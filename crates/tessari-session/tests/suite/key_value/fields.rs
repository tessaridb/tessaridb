//! A hash field that stops being answered at its own instant (G079 C6,
//! ADR-0132): `HASH EXPIRE`, `HASH TTL`, `HASH PERSIST`, and which writes keep
//! a field's instant.

use tessari_session::{Error, Now, RunNow, Session};
use tessari_types::{Number, Value};

use super::{PAST_SHORT, SHORT, on_each_backend, opened, refused, run, value};

fn codes(items: &[i64]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| Value::Number(Number::Integer(*item)))
            .collect(),
    )
}

/// What `HASH TTL` answered for each field: `"left"` for a duration, `"never"`
/// for `NULL`, `"none"` for `NONE`.
fn left(session: &mut Session<'_>, script: &str) -> Vec<&'static str> {
    match value(session, script) {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Duration(_) => "left",
                Value::Null => "never",
                Value::None => "none",
                _ => "other",
            })
            .collect(),
        other => panic!("{script} answered {other:?}"),
    }
}

fn fields(session: &mut Session<'_>, key: &str) -> Vec<String> {
    match value(session, &format!("GET {key};")) {
        Value::Object(fields) => fields.into_keys().collect(),
        Value::None => Vec::new(),
        other => panic!("{key} holds {other:?}"),
    }
}

#[test]
fn a_field_is_given_an_instant_read_and_cleared_one_code_per_field() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        // No key: every field is not there.
        assert_eq!(
            value(&mut session, "HASH EXPIRE cache:'h' 'a', 'b' AT 1h;"),
            codes(&[-2, -2]),
            "{}",
            backend.name
        );
        assert_eq!(left(&mut session, "HASH TTL cache:'h' 'a';"), ["none"]);
        assert_eq!(
            value(&mut session, "HASH PERSIST cache:'h' 'a';"),
            codes(&[-2])
        );
        run(
            &mut session,
            "HASH SET cache:'h' 'a' = 1, 'b' = 2, 'c' = 3;",
        );
        assert_eq!(
            value(&mut session, "HASH EXPIRE cache:'h' 'a', 'zz' AT 1h;"),
            codes(&[1, -2])
        );
        assert_eq!(
            left(&mut session, "HASH TTL cache:'h' 'a', 'b', 'zz';"),
            ["left", "never", "none"]
        );
        // The four conditions; a field with no instant is later than any.
        assert_eq!(
            value(
                &mut session,
                "HASH EXPIRE cache:'h' 'a', 'b' AT 2h IF NONE;"
            ),
            codes(&[0, 1])
        );
        assert_eq!(
            value(
                &mut session,
                "HASH EXPIRE cache:'h' 'b', 'c' AT 3h IF SOME;"
            ),
            codes(&[1, 0])
        );
        assert_eq!(
            value(
                &mut session,
                "HASH EXPIRE cache:'h' 'a', 'c' AT 30m IF LATER;"
            ),
            codes(&[0, 0])
        );
        assert_eq!(
            value(
                &mut session,
                "HASH EXPIRE cache:'h' 'a', 'c' AT 30m IF SOONER;"
            ),
            codes(&[1, 1])
        );
        assert_eq!(
            value(&mut session, "HASH PERSIST cache:'h' 'b', 'zz';"),
            codes(&[1, -2])
        );
        assert_eq!(
            value(&mut session, "HASH PERSIST cache:'h' 'b';"),
            codes(&[-1])
        );
        assert_eq!(
            left(&mut session, "HASH TTL cache:'h' 'a', 'b', 'c';"),
            ["left", "never", "left"]
        );
        // An instant not in the future removes the field there and then.
        assert_eq!(
            value(&mut session, "HASH EXPIRE cache:'h' 'c' AT 0s;"),
            codes(&[2])
        );
        assert_eq!(fields(&mut session, "cache:'h'"), ["a", "b"]);
        assert_eq!(
            value(
                &mut session,
                "HASH EXPIRE cache:'h' 'b' AT datetime '2001-01-01T00:00:00Z';"
            ),
            codes(&[2])
        );
        assert_eq!(fields(&mut session, "cache:'h'"), ["a"]);
    });
}

#[test]
fn an_overwrite_or_a_removal_drops_a_fields_instant_and_nothing_else_does() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "HASH SET cache:'h' 'a' = 1, 'b' = 2, 'n' = 5;",
        );
        run(&mut session, "HASH EXPIRE cache:'h' 'a', 'b', 'n' AT 1h;");
        // Altered in place, or left alone, a field keeps its instant.
        run(&mut session, "HASH INCR cache:'h' 'n' BY 1;");
        run(&mut session, "HASH SET cache:'h' 'b' = 9 IF ABSENT;");
        run(&mut session, "HASH SET cache:'h' 'z' = 0;");
        assert_eq!(
            left(&mut session, "HASH TTL cache:'h' 'a', 'b', 'n', 'z';"),
            ["left", "left", "left", "never"],
            "{}",
            backend.name
        );
        // Overwritten, or removed and set again, it does not.
        run(&mut session, "HASH SET cache:'h' 'a' = 7;");
        run(&mut session, "HASH REMOVE cache:'h' 'b';");
        run(&mut session, "HASH SET cache:'h' 'b' = 8;");
        assert_eq!(
            left(&mut session, "HASH TTL cache:'h' 'a', 'b', 'n';"),
            ["never", "never", "left"]
        );
        // The key's own expiry moved or cleared leaves its fields' alone.
        run(&mut session, "EXPIRE cache:'h' 2h;");
        run(&mut session, "PERSIST cache:'h';");
        assert_eq!(left(&mut session, "HASH TTL cache:'h' 'n';"), ["left"]);
        // A whole-key SET replaces the hash and every instant in it.
        run(&mut session, "SET cache:'h' = { n: 1 };");
        assert_eq!(left(&mut session, "HASH TTL cache:'h' 'n';"), ["never"]);
    });
}

#[test]
fn a_field_past_its_instant_is_gone_and_the_last_takes_the_key() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "HASH SET cache:'h' 'a' = 1, 'b' = 2;");
        run(&mut session, "HASH SET cache:'o' 'x' = 1;");
        run(
            &mut session,
            &format!("HASH EXPIRE cache:'h' 'a' AT {SHORT}; HASH EXPIRE cache:'o' 'x' AT {SHORT};"),
        );
        std::thread::sleep(PAST_SHORT);
        assert_eq!(fields(&mut session, "cache:'h'"), ["b"], "{}", backend.name);
        assert_eq!(left(&mut session, "HASH TTL cache:'h' 'a';"), ["none"]);
        assert_eq!(value(&mut session, "GET cache:'o';"), Value::None);
        // A write after it starts from what the reader saw.
        assert_eq!(
            value(&mut session, "HASH SET cache:'h' 'a' = 3;"),
            Value::Number(Number::Integer(1))
        );
    });
}

#[test]
fn a_field_instant_is_refused_on_another_kind_and_below_format_nine() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "LIST PUSH cache:'l' BACK 'a';");
        let why = refused(&mut session, "HASH EXPIRE cache:'l' 'a' AT 1h;");
        assert!(why.contains("holds a list"), "{why}");
        let why = refused(&mut session, "HASH EXPIRE cache:'h' 'a' AT 'soon';");
        assert!(why.contains("duration or a datetime"), "{why}");
    });
    use std::sync::Arc;

    use tessari_encoding::{FormatVersion, FormatVersionKey, StoreKey, StoreValue};
    use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
    use tessari_storage::Store;

    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    drop(Store::open(Arc::clone(&backend)).now().unwrap());
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(8).encode(),
        ))
        .now()
        .unwrap();
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    let mut session = opened(&store);
    run(&mut session, "HASH SET cache:'h' 'a' = 1;");
    let why = session
        .run_now("HASH EXPIRE cache:'h' 'a' AT 1h;")
        .unwrap_err();
    assert!(
        matches!(
            why,
            Error::FormatNotFinalized {
                needs: 9,
                holds: 8,
                ..
            }
        ),
        "{why:?}"
    );
}

/// `HASH TTL` is a read: a reader granted only `read` asks it, and is refused
/// the write beside it.
#[test]
fn a_reader_asks_a_fields_time_left_and_may_not_give_one() {
    const PASSWORD: &str = "correct horse battery";
    let store = tessari_storage::Store::open(std::sync::Arc::new(tessari_kv::MemoryBackend::new()))
        .now()
        .unwrap();
    let mut owner = opened(&store);
    run(
        &mut owner,
        "HASH SET cache:'h' 'a' = 1; HASH EXPIRE cache:'h' 'a' AT 1h;\n\
         DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
    );
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).now().unwrap();
    root.run_now(
        "DEFINE USER ada ON prod.app ROLE editor PASSWORD 'correct horse battery';\n\
         USE NAMESPACE prod; USE DATABASE app; GRANT read ON cache TO ada;",
    )
    .unwrap();
    let mut ada = Session::new(&store);
    ada.sign_in("ada", PASSWORD).now().unwrap();
    ada.run_now("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    assert_eq!(left(&mut ada, "HASH TTL cache:'h' 'a';"), ["left"]);
    let why = refused(&mut ada, "HASH PERSIST cache:'h' 'a';");
    assert!(why.contains("write"), "{why}");
}
