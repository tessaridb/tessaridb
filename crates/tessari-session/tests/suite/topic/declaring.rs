//! S1.1 and S1.2: declaring a topic, and what cannot be written to one.

use tessari_types::Value;

use super::super::key_value::{on_each_backend, refused, run, value};
use super::opened;

fn definition(session: &mut tessari_session::Session<'_>, table: &str) -> String {
    let Value::Object(fields) = value(session, &format!("INFO FOR TABLE {table};")) else {
        panic!("INFO FOR TABLE answered no object");
    };
    match fields.get("definition") {
        Some(Value::String(text)) => text.clone(),
        other => panic!("no definition: {other:?}"),
    }
}

#[test]
fn a_topic_is_written_back_with_its_own_word_and_clauses() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC audit PUBLIC RATE 100 PER 1m MAX BYTES 4096 RETAIN 7d;",
        );
        assert_eq!(
            definition(&mut session, "events"),
            "DEFINE TOPIC events;\n",
            "{}",
            backend.name
        );
        let written = definition(&mut session, "audit");
        assert_eq!(
            written,
            "DEFINE TOPIC audit RETAIN 168h MAX BYTES 4096 PUBLIC RATE 100 PER 1m;\n"
        );
        run(&mut session, "DEFINE DATABASE again; USE DATABASE again;");
        run(&mut session, &written);
        assert_eq!(
            definition(&mut session, "audit"),
            written,
            "{}",
            backend.name
        );
    });
}

#[test]
fn a_topic_that_holds_nothing_or_opens_without_a_size_is_refused() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        for (script, expected) in [
            ("DEFINE TOPIC t MAX BYTES 0;", "above zero"),
            ("DEFINE TOPIC t RETAIN 0s;", "above zero"),
            (
                "DEFINE TOPIC t PUBLIC RATE 0 PER 1m MAX BYTES 10;",
                "above zero",
            ),
            (
                "DEFINE TOPIC t PUBLIC RATE 5 PER 1m;",
                "must bound a message's size",
            ),
            // Not in a topic's grammar at all: one order is the point.
            ("DEFINE TOPIC t SPLIT AT 'g';", "found the name `SPLIT`"),
        ] {
            let why = refused(&mut session, script);
            assert!(why.contains(expected), "{}: {script}: {why}", backend.name);
        }
    });
}

#[test]
fn a_message_is_never_rewritten_or_deleted_and_an_identity_is_appended_once() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "CREATE events:'a' = { n: 1 };");
        for script in [
            "UPDATE events:'a' SET n = 2;",
            "UPSERT events:'a' = { n: 2 };",
            "DELETE events:'a';",
            "BEGIN; DELETE FROM events WHERE n = 1 LIMIT ALL; COMMIT;",
        ] {
            let why = refused(&mut session, script);
            assert!(
                why.contains("topic events is append-only"),
                "{}: {script}: {why}",
                backend.name
            );
        }
        let why = refused(&mut session, "CREATE events:'a' = { n: 3 };");
        assert!(!why.contains("append-only"), "{}: {why}", backend.name);
        run(&mut session, "CREATE events:'b' = { n: 4 };");
        let Value::Array(held) = value(&mut session, "RETURN (SELECT n FROM events);") else {
            panic!("no array");
        };
        assert_eq!(held.len(), 2, "{}: {held:?}", backend.name);
    });
}

#[test]
fn a_message_larger_than_its_topic_takes_is_refused_by_name() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(&mut session, "DEFINE TOPIC small MAX BYTES 64;");
        run(&mut session, "CREATE small = { n: 1 };");
        let why = refused(
            &mut session,
            &format!("CREATE small = {{ text: '{}' }};", "x".repeat(100)),
        );
        assert!(
            why.contains("topic small takes messages of at most 64 bytes"),
            "{}: {why}",
            backend.name
        );
    });
}
