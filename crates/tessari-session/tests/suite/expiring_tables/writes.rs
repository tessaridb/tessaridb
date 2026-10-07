//! What a write does to the instant of a record in a table that declares expiry
//! (ADR-0122 A2, A3).

use std::sync::Arc;
use std::thread;
use std::time::Duration as Wait;

use tessari_kv::MemoryBackend;
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

use super::{field, opened, run};

/// A short lifetime, and a wait past it — real time, as the key-value suite
/// does, because the clock is the transaction's own.
const SHORT: &str = "300ms";
const PAST_SHORT: Wait = Wait::from_millis(450);

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new())).unwrap()
}

/// Whether the record carries an instant: `TTL` answers a duration for one,
/// `NULL` for a record that never expires and `NONE` for no record.
fn ttl(session: &mut Session<'_>, record: &str) -> Value {
    match run(session, &format!("RETURN TTL {record};")) {
        Outcome::Value(value) => value,
        other => panic!("TTL answered {other:?}"),
    }
}

fn expiring(session: &mut Session<'_>, record: &str) -> bool {
    matches!(ttl(session, record), Value::Duration(_))
}

fn rows(session: &mut Session<'_>, read: &str) -> usize {
    match run(session, read) {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("{read} answered {other:?}"),
    }
}

#[test]
fn a_created_record_gets_the_declared_lifetime_and_is_gone_after_it() {
    let store = store();
    let mut session = opened(&store);
    run(
        &mut session,
        &format!("DEFINE TABLE message (body string) EXPIRE AFTER {SHORT};"),
    );
    run(&mut session, "CREATE message:1 = { body: 'hi' };");
    run(&mut session, "CREATE message = { body: 'generated' };");
    run(
        &mut session,
        "INSERT INTO message (body) VALUES ('inserted');",
    );
    assert!(expiring(&mut session, "message:1"));
    assert_eq!(rows(&mut session, "SELECT * FROM message;"), 3);
    thread::sleep(PAST_SHORT);
    assert_eq!(
        rows(&mut session, "SELECT * FROM message;"),
        0,
        "every created record took the default"
    );
}

#[test]
fn a_plain_write_keeps_the_instant_whatever_its_shape() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE note (body string) EXPIRE;");
    for id in 1..=5 {
        run(
            &mut session,
            &format!("CREATE note:{id} = {{ body: 'v1' }} EXPIRE {SHORT};"),
        );
    }
    run(&mut session, "UPDATE note:1 = { body: 'whole' };");
    run(&mut session, "UPDATE note:2 MERGE { body: 'merged' };");
    run(&mut session, "UPDATE note:3 SET body = 'set';");
    run(&mut session, "UPSERT note:4 = { body: 'upserted' };");
    run(&mut session, "SET note:5 = { body: 'key-value' };");
    for id in 1..=5 {
        assert!(
            expiring(&mut session, &format!("note:{id}")),
            "note:{id} lost its instant to a plain write"
        );
    }
    thread::sleep(PAST_SHORT);
    assert_eq!(rows(&mut session, "SELECT * FROM note;"), 0);
}

#[test]
fn a_write_moves_or_clears_the_instant_only_when_it_says_so() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE COLLECTION drafts EXPIRE AFTER 1h;");
    run(&mut session, "CREATE drafts:1 = { n: 1 };");
    run(&mut session, "UPDATE drafts:1 MERGE { n: 2 } EXPIRE NONE;");
    assert_eq!(ttl(&mut session, "drafts:1"), Value::Null);
    // Cleared on purpose stays cleared through a later plain write.
    run(&mut session, "UPDATE drafts:1 MERGE { n: 3 };");
    assert_eq!(ttl(&mut session, "drafts:1"), Value::Null);
    run(
        &mut session,
        &format!("UPDATE drafts:1 MERGE {{ n: 4 }} EXPIRE {SHORT};"),
    );
    assert!(expiring(&mut session, "drafts:1"));
    run(&mut session, "PERSIST drafts:1;");
    run(&mut session, "UPDATE drafts:1 MERGE { n: 5 };");
    assert_eq!(ttl(&mut session, "drafts:1"), Value::Null);
}

#[test]
fn an_instant_set_earlier_in_the_transaction_survives_a_later_plain_write() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE note (body string) EXPIRE;");
    run(&mut session, "CREATE note:1 = { body: 'v1' };");
    run(
        &mut session,
        "BEGIN; UPDATE note:1 MERGE { body: 'v2' } EXPIRE 1h; UPDATE note:1 MERGE { body: 'v3' }; COMMIT;",
    );
    assert!(expiring(&mut session, "note:1"));
}

#[test]
fn a_record_written_over_an_expired_one_is_created_again() {
    let store = store();
    let mut session = opened(&store);
    run(
        &mut session,
        &format!("DEFINE TABLE message (body string) EXPIRE AFTER {SHORT};"),
    );
    run(&mut session, "CREATE message:1 = { body: 'first' };");
    thread::sleep(PAST_SHORT);
    run(&mut session, "CREATE message:1 = { body: 'second' };");
    assert!(
        expiring(&mut session, "message:1"),
        "the new record took the default, not the expired one's passed instant"
    );
    assert_eq!(rows(&mut session, "SELECT * FROM message;"), 1);
}

#[test]
fn a_table_that_declares_nothing_keeps_the_key_value_rule() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE items SCHEMALESS;");
    run(&mut session, "SET items:1 = { n: 1 } EXPIRE 1h;");
    assert!(expiring(&mut session, "items:1"));
    run(&mut session, "UPDATE items:1 MERGE { n: 2 };");
    assert_eq!(
        ttl(&mut session, "items:1"),
        Value::Null,
        "a plain write still clears an instant on a table that did not opt in"
    );
}

#[test]
fn a_write_is_refused_an_expiry_its_table_does_not_take_and_writes_nothing() {
    let store = store();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE TABLE plain (body string); DEFINE TABLE gone (body string) EXPIRE; \
         ALTER TABLE gone DROP EXPIRE; DEFINE TABLE note (body string) EXPIRE;",
    );
    assert!(matches!(
        session.run("CREATE plain:1 = { body: 'x' } EXPIRE 1h;"),
        Err(Error::TableDoesNotExpire { .. })
    ));
    assert!(matches!(
        session.run("CREATE gone:1 = { body: 'x' } EXPIRE 1h;"),
        Err(Error::TableDoesNotExpire { .. })
    ));
    assert!(matches!(
        session.run("CREATE note:1 = { body: 'x' } EXPIRE -1s;"),
        Err(Error::InvalidExpiry { .. })
    ));
    assert_eq!(rows(&mut session, "SELECT * FROM plain;"), 0);
    assert_eq!(rows(&mut session, "SELECT * FROM note;"), 0);
    // Clearing is never refused: it only makes a record permanent.
    run(&mut session, "CREATE gone:2 = { body: 'y' } EXPIRE NONE;");
    assert_eq!(ttl(&mut session, "gone:2"), Value::Null);
}

/// More than six days left of a seven-day lifetime: the declared default, and
/// not some other instant.
fn about_a_week(left: &Value) -> bool {
    matches!(left, Value::Duration(left) if left.seconds() > 6 * 86_400)
}

/// The outcomes of a script that holds one transaction.
fn outcomes(session: &mut Session<'_>, script: &str) -> Vec<Outcome> {
    session.run(script).unwrap()
}

#[test]
fn inside_a_transaction_a_record_answers_the_instant_its_commit_will_give_it() {
    let store = store();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE TABLE message (body string) EXPIRE AFTER 7d; DEFINE TABLE items SCHEMALESS;",
    );
    run(&mut session, "CREATE message:1 = { body: 'kept' };");
    run(&mut session, "CREATE message:3 = { body: 'kept' };");
    run(&mut session, "SET items:1 = { n: 1 } EXPIRE 1h;");
    let seen = outcomes(
        &mut session,
        "BEGIN; CREATE message:2 = { body: 'new' }; LET $created = TTL message:2;\n\
         UPDATE message:1 SET body = 'edited'; LET $kept = TTL message:1;\n\
         UPDATE message:2 SET body = 'never' EXPIRE NONE; LET $cleared = TTL message:2;\n\
         UPDATE items:1 MERGE { n: 2 }; LET $plain = TTL items:1;\n\
         RETURN { created: $created, kept: $kept, cleared: $cleared, plain: $plain }; COMMIT;",
    );
    let Some(answer) = seen.iter().find_map(|outcome| match outcome {
        Outcome::Value(value @ Value::Object(_)) => Some(value.clone()),
        _ => None,
    }) else {
        panic!("no answer in {seen:?}");
    };
    assert!(
        about_a_week(&field(&answer, "created")),
        "a create takes the default: {answer:?}"
    );
    assert!(
        about_a_week(&field(&answer, "kept")),
        "a plain write keeps the instant: {answer:?}"
    );
    assert_eq!(field(&answer, "cleared"), Value::Null, "cleared on purpose");
    assert_eq!(
        field(&answer, "plain"),
        Value::Null,
        "a table that did not opt in keeps the key-value rule"
    );
    let persisted = outcomes(
        &mut session,
        "BEGIN; UPDATE message:3 SET body = 'edited'; PERSIST message:3; COMMIT;",
    );
    assert!(
        persisted.contains(&Outcome::Value(Value::Bool(true))),
        "the edited record had an instant to clear: {persisted:?}"
    );
    assert_eq!(ttl(&mut session, "message:3"), Value::Null);
}
