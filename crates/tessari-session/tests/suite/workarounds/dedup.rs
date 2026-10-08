//! `DEDUPLICATE <window>` on a queue and on a topic — a repeat inside the
//! window writes nothing (ADR-0124 D8).

use std::time::Duration;

use tessari_session::{Error, Outcome};
use tessari_types::{RefusalClass, Value};

use super::{inside, refused, rows, run, store, value};

fn count(session: &mut tessari_session::Session<'_>, read: &str) -> usize {
    rows(session, read).len()
}

fn messages(session: &mut tessari_session::Session<'_>, topic: &str) -> usize {
    match run(session, &format!("READ FROM {topic};")) {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("READ FROM {topic}: {other:?}"),
    }
}

#[test]
fn a_queue_record_created_twice_inside_the_window_is_queued_once() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(&mut session, "CREATE work:'obj-1' = { key: 'a' };");
    assert_eq!(
        value(
            &mut session,
            "CREATE work:'obj-1' = { key: 'b' } RETURN AFTER;"
        ),
        Value::None,
        "a repeat answers no record"
    );
    let held = rows(&mut session, "SELECT key FROM work;");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["key"], Value::from("a"), "the first write stands");
}

#[test]
fn a_finished_queue_record_is_not_queued_again_inside_the_window() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(&mut session, "CREATE work:'obj-1' = {};");
    run(&mut session, "DELETE work:'obj-1';");
    run(&mut session, "CREATE work:'obj-1' = {};");
    assert_eq!(
        count(&mut session, "SELECT * FROM work;"),
        0,
        "done is done"
    );
}

#[test]
fn a_repeat_in_the_same_transaction_is_deduplicated_too() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(
        &mut session,
        "BEGIN; CREATE work:1 = {}; CREATE work:1 = {}; COMMIT;",
    );
    assert_eq!(count(&mut session, "SELECT * FROM work;"), 1);
}

#[test]
fn the_window_counts_from_the_first_write_and_then_lets_it_through() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 50ms;",
    );
    run(&mut session, "CREATE work:1 = {}; DELETE work:1;");
    std::thread::sleep(Duration::from_millis(120));
    run(&mut session, "CREATE work:1 = {};");
    assert_eq!(
        count(&mut session, "SELECT * FROM work;"),
        1,
        "window passed"
    );
}

#[test]
fn without_the_clause_a_queue_refuses_an_existing_identity_as_before() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s; CREATE work:1 = {};",
    );
    assert!(matches!(
        refused(&mut session, "CREATE work:1 = {};"),
        Error::RecordExists { .. }
    ));
}

#[test]
fn a_topic_message_with_a_key_seen_inside_the_window_is_not_appended() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TOPIC events DEDUPLICATE 5m ON msg_id;",
    );
    run(&mut session, "CREATE events = { msg_id: 'm-1', n: 1 };");
    run(&mut session, "CREATE events = { msg_id: 'm-1', n: 2 };");
    run(
        &mut session,
        "INSERT INTO events (msg_id, n) VALUES ('m-1', 3), ('m-2', 4);",
    );
    run(&mut session, "CREATE events = { n: 5 };");
    run(&mut session, "CREATE events = { n: 6 };");
    assert_eq!(
        messages(&mut session, "events"),
        4,
        "m-1 once, m-2 once, and both messages without a key"
    );
}

#[test]
fn a_topic_key_must_be_a_kind_an_identity_can_hold() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TOPIC events DEDUPLICATE 5m ON msg_id;",
    );
    assert!(matches!(
        refused(&mut session, "CREATE events = { msg_id: 1.5 };"),
        Error::DeduplicationKey { .. }
    ));
}

#[test]
fn the_declaration_is_refused_where_it_cannot_mean_anything() {
    let store = store();
    let mut session = inside(&store);
    for statement in [
        "DEFINE TOPIC events DEDUPLICATE 5m;",
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 0s;",
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m ON key;",
        "DEFINE TABLE plain DEDUPLICATE 5m;",
        "DEFINE TOPIC open DEDUPLICATE 5m ON k MAX BYTES 64 PUBLIC RATE 1 PER 1s;",
    ] {
        let error = refused(&mut session, statement);
        assert_eq!(error.class(), RefusalClass::Invalid, "{statement}: {error}");
    }
}

#[test]
fn the_window_is_shown_and_the_marker_table_is_not() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(&mut session, "CREATE work:1 = {};");
    let info = value(&mut session, "INFO FOR TABLE work;");
    let Value::Object(fields) = &info else {
        panic!("{info:?}")
    };
    assert_eq!(
        fields.get("deduplicate"),
        Some(&Value::Duration(tessari_types::Duration::from_seconds(300)))
    );
    let database = format!("{:?}", value(&mut session, "INFO FOR DATABASE;"));
    assert!(!database.contains("seen"), "{database}");
    let Outcome::Value(Value::String(script)) = run(&mut session, "BACKUP SCRIPT;") else {
        panic!("BACKUP SCRIPT answered something else")
    };
    assert!(script.contains("DEDUPLICATE 5m"), "{script}");
    assert!(!script.contains('\u{1}'), "{script}");
}

#[test]
fn dropping_the_queue_drops_its_markers_with_it() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(&mut session, "CREATE work:1 = {};");
    run(&mut session, "DROP QUEUE work;");
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(&mut session, "CREATE work:1 = {};");
    assert_eq!(count(&mut session, "SELECT * FROM work;"), 1);
}

#[test]
fn a_topic_script_carries_its_window_and_key_and_reads_back() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TOPIC events DEDUPLICATE 5m ON msg_id;",
    );
    let Outcome::Value(Value::String(script)) = run(&mut session, "BACKUP SCRIPT;") else {
        panic!("BACKUP SCRIPT answered something else")
    };
    assert!(script.contains("DEDUPLICATE 5m ON msg_id"), "{script}");
    let Value::Object(info) = value(&mut session, "INFO FOR TOPIC events;") else {
        panic!("INFO FOR TOPIC answered something else")
    };
    assert_eq!(info.get("deduplicate_on"), Some(&Value::from("msg_id")));
    assert_eq!(
        info.get("deduplicate"),
        Some(&Value::Duration(tessari_types::Duration::from_seconds(300)))
    );
    let restored =
        tessari_storage::Store::open(std::sync::Arc::new(tessari_kv::MemoryBackend::new())
            as std::sync::Arc<dyn tessari_kv::KvBackend>)
        .unwrap();
    let mut again = tessari_session::Session::new(&restored);
    run(&mut again, &script);
    run(&mut again, "USE NAMESPACE prod; USE DATABASE app;");
    run(
        &mut again,
        "CREATE events = { msg_id: 'a' }; CREATE events = { msg_id: 'a' };",
    );
    assert_eq!(messages(&mut again, "events"), 1);
}

#[test]
fn an_event_enqueueing_by_its_record_leaves_one_piece_of_work() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION pages; \
         DEFINE QUEUE reindex TIMEOUT 1m DEDUPLICATE 10m; \
         DEFINE EVENT reindex ON pages FOR UPDATE THEN CREATE reindex:$id = { page: $id };",
    );
    run(&mut session, "CREATE pages:1 = { v: 0 };");
    run(
        &mut session,
        "BEGIN; UPDATE pages:1 SET v = 1; UPDATE pages:1 SET v = 2; COMMIT;",
    );
    run(&mut session, "UPDATE pages:1 SET v = 3;");
    assert_eq!(count(&mut session, "SELECT * FROM reindex;"), 1);
}

#[test]
fn a_held_record_and_one_handed_back_are_not_queued_again() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE work TIMEOUT 30s DEDUPLICATE 5m;",
    );
    run(&mut session, "CREATE work:1 = { n: 1 };");
    run(&mut session, "CLAIM FROM work;");
    run(&mut session, "CREATE work:1 = { n: 2 };");
    run(&mut session, "RELEASE work:1;");
    run(&mut session, "CREATE work:1 = { n: 3 };");
    let held = rows(&mut session, "SELECT n, attempts FROM work;");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["n"], Value::from(1_i64), "the first write stands");
    assert_eq!(
        held[0]["attempts"],
        Value::from(1_i64),
        "its attempt is kept"
    );
}
