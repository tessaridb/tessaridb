//! `DEFINE FEED` — the declared capture of a table's writes into a topic
//! (FEAT-S1, plan `.bgv/projects/tessaridb/plans/2026-11-15/123001`).
//!
//! The properties under test are the ones a hand-written outbox cannot state:
//! one message per record per transaction carrying the final image (net
//! effect), the message and the write landing in ONE commit, the topic refusing
//! hand appends, and the append authorized as the writer.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Now, RunNow, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    Session::new(&store)
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
             DEFINE COLLECTION orders;",
        )
        .unwrap();
    store
}

fn inside(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The messages of a topic, as objects — each read answers
/// `{appended, position, value}`, and the envelope is the `value`.
fn messages(session: &mut Session<'_>, topic: &str) -> Vec<BTreeMap<String, Value>> {
    // LIMIT: a bare READ FROM answers its first hundred.
    let outcomes = session
        .run_now(&format!("READ FROM {topic} LIMIT 1000000;"))
        .unwrap();
    let Some(tessari_session::Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("READ FROM {topic}: {:?}", outcomes.last());
    };
    records
        .iter()
        .map(|(_, row)| match row {
            Value::Object(fields) => match fields.get("value") {
                Some(Value::Object(envelope)) => envelope.clone(),
                other => panic!("message has no envelope: {other:?}"),
            },
            other => panic!("not an object: {other:?}"),
        })
        .collect()
}

fn text_of<'a>(fields: &'a BTreeMap<String, Value>, key: &str) -> &'a str {
    match fields.get(key) {
        Some(Value::String(held)) => held,
        other => panic!("{key} is not a string: {other:?}"),
    }
}

#[test]
fn a_created_record_lands_as_one_create_message() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session
        .run_now("CREATE orders:1 = { item: 'book', total: 30 };")
        .unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 1);
    assert_eq!(text_of(&found[0], "op"), "CREATE");
    assert_eq!(found[0].get("key"), Some(&Value::from(1_i64)));
    match found[0].get("after") {
        Some(Value::Object(after)) => assert_eq!(
            after.get("item"),
            Some(&Value::from("book")),
            "after carries the record"
        ),
        other => panic!("after is not a record: {other:?}"),
    }
    assert!(!found[0].contains_key("before"), "a create has no before");
}

#[test]
fn two_writes_in_one_transaction_land_once_with_the_final_image() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    // Created and then written again inside one transaction: one CREATE
    // message carrying the final image, not a create and an update.
    session
        .run_now("BEGIN; CREATE orders:1 = { total: 10 }; UPDATE orders:1 SET total = 20; COMMIT;")
        .unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(
        found.len(),
        1,
        "one record changed twice in one transaction appears once"
    );
    assert_eq!(text_of(&found[0], "op"), "CREATE");
    match found[0].get("after") {
        Some(Value::Object(after)) => assert_eq!(after.get("total"), Some(&Value::from(20_i64))),
        other => panic!("after is not a record: {other:?}"),
    }
}

#[test]
fn two_updates_in_one_transaction_land_once() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session.run_now("CREATE orders:1 = { total: 5 };").unwrap();
    // Two updates of one record inside one transaction: one UPDATE message
    // with the before image from the snapshot and the after image as it ends.
    session
        .run_now("BEGIN; UPDATE orders:1 SET total = 10; UPDATE orders:1 SET total = 20; COMMIT;")
        .unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 2, "the earlier create lands beside the update");
    assert_eq!(text_of(&found[1], "op"), "UPDATE");
    match found[1].get("before") {
        Some(Value::Object(before)) => assert_eq!(before.get("total"), Some(&Value::from(5_i64))),
        other => panic!("before is not a record: {other:?}"),
    }
    match found[1].get("after") {
        Some(Value::Object(after)) => assert_eq!(after.get("total"), Some(&Value::from(20_i64))),
        other => panic!("after is not a record: {other:?}"),
    }
}

#[test]
fn an_updated_record_lands_as_update_with_both_images() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session.run_now("CREATE orders:1 = { total: 10 };").unwrap();
    session
        .run_now("BEGIN; UPDATE orders:1 SET total = 20; COMMIT;")
        .unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 2);
    assert_eq!(text_of(&found[1], "op"), "UPDATE");
    match found[1].get("before") {
        Some(Value::Object(before)) => assert_eq!(before.get("total"), Some(&Value::from(10_i64))),
        other => panic!("before is not a record: {other:?}"),
    }
    match found[1].get("after") {
        Some(Value::Object(after)) => assert_eq!(after.get("total"), Some(&Value::from(20_i64))),
        other => panic!("after is not a record: {other:?}"),
    }
}

#[test]
fn a_delete_lands_as_an_erase_record() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session.run_now("CREATE orders:1 = { total: 10 };").unwrap();
    session.run_now("DELETE orders:1;").unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 2);
    assert_eq!(text_of(&found[1], "op"), "DELETE");
    assert!(!found[1].contains_key("after"), "a delete has no after");
    assert!(found[1].contains_key("before"), "a delete carries before");
}

#[test]
fn created_and_deleted_in_one_transaction_lands_nothing() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session
        .run_now("BEGIN; CREATE orders:1 = { total: 10 }; DELETE orders:1; COMMIT;")
        .unwrap();
    assert!(messages(&mut session, "order_events").is_empty());
}

#[test]
fn a_hand_append_to_a_feed_owned_topic_is_refused_naming_the_feed() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    let refused = session.run_now("CREATE order_events = { forged: true };");
    let reported = format!("{refused:?}");
    assert!(
        reported.contains("orders_feed"),
        "the refusal names the feed: {reported}"
    );
    assert!(messages(&mut session, "order_events").is_empty());
}

#[test]
fn a_canceled_transaction_leaves_no_message() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session
        .run_now("BEGIN; CREATE orders:1 = { total: 10 }; CANCEL;")
        .unwrap();
    assert!(messages(&mut session, "order_events").is_empty());
}

#[test]
fn for_delete_captures_only_deletes() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "DEFINE TOPIC order_events; \
             DEFINE FEED orders_feed ON orders INTO order_events FOR DELETE;",
        )
        .unwrap();
    session.run_now("CREATE orders:1 = { total: 10 };").unwrap();
    session.run_now("UPDATE orders:1 SET total = 20;").unwrap();
    session.run_now("DELETE orders:1;").unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 1);
    assert_eq!(text_of(&found[0], "op"), "DELETE");
}

#[test]
fn image_keys_carries_no_images() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "DEFINE TOPIC order_events; \
             DEFINE FEED orders_feed ON orders INTO order_events IMAGE KEYS;",
        )
        .unwrap();
    session.run_now("CREATE orders:1 = { total: 10 };").unwrap();
    session.run_now("UPDATE orders:1 SET total = 20;").unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 2);
    for message in &found {
        assert!(!message.contains_key("before"));
        assert!(!message.contains_key("after"));
    }
    assert_eq!(text_of(&found[1], "op"), "UPDATE");
}

#[test]
fn the_capture_runs_with_the_writers_authority() {
    // A writer who may write the table and not append the topic has the
    // write refused — the capture is part of the write, exactly as an
    // event's body is.
    let store = store();
    let mut setup = inside(&store);
    setup
        .run_now(
            "DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;              DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", "correct horse battery").now().unwrap();
    root.run_now(
        "DEFINE USER clerk ON prod.shop ROLE editor PASSWORD 'correct horse battery';          USE NAMESPACE prod; USE DATABASE shop; GRANT read, write ON orders TO clerk;",
    )
    .unwrap();
    let mut clerk = Session::new(&store);
    clerk
        .sign_in("clerk", "correct horse battery")
        .now()
        .unwrap();
    clerk
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let refused = clerk.run_now("CREATE orders:1 = { v: 1 };").unwrap_err();
    assert!(matches!(refused, Error::FeedFailed { .. }), "{refused:?}");
    assert!(messages(&mut root, "order_events").is_empty());
    root.run_now("GRANT read, write ON order_events TO clerk;")
        .unwrap();
    clerk.run_now("CREATE orders:1 = { v: 1 };").unwrap();
    assert_eq!(messages(&mut root, "order_events").len(), 1);
}

#[test]
fn a_dropped_feed_stops_capturing_and_frees_its_topic() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events;")
        .unwrap();
    session.run_now("DROP FEED orders_feed ON orders;").unwrap();
    session.run_now("CREATE orders:1 = { v: 1 };").unwrap();
    assert!(messages(&mut session, "order_events").is_empty());
    // The topic is the feed's no longer: a caller may append to it again.
    session
        .run_now("CREATE order_events = { note: 'by hand' };")
        .unwrap();
    assert_eq!(messages(&mut session, "order_events").len(), 1);
}

#[test]
fn drop_feed_if_exists_accepts_a_missing_feed() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now("DROP FEED IF EXISTS nobody ON orders;")
        .unwrap();
}

#[test]
fn dropping_a_feed_nobody_defined_refuses_naming_it() {
    let store = store();
    let mut session = inside(&store);
    let refused = session.run_now("DROP FEED nobody ON orders;");
    assert!(
        format!("{:?}", refused).contains("Unknown"),
        "the refusal names the missing feed: {refused:?}"
    );
}

#[test]
fn info_for_table_lists_feeds_as_their_defining_statements() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "DEFINE TOPIC order_events;              DEFINE FEED orders_feed ON orders INTO order_events FOR UPDATE, DELETE IMAGE KEYS              WHEN $after.v > 1 WITH INITIAL SCAN;",
        )
        .unwrap();
    let outcomes = session.run_now("INFO FOR TABLE orders;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(info))) = outcomes.last() else {
        panic!("INFO FOR TABLE: {:?}", outcomes.last());
    };
    let feeds = match info.get("feeds") {
        Some(Value::Array(listed)) => listed,
        other => panic!("feeds is not a list: {other:?}"),
    };
    assert_eq!(feeds.len(), 1);
    let stated = format!("{:?}", feeds[0]);
    for expected in [
        "DEFINE FEED orders_feed ON orders INTO order_events",
        "FOR UPDATE, DELETE",
        "IMAGE KEYS",
        "WHEN $after.v > 1",
        "WITH INITIAL SCAN",
    ] {
        assert!(
            stated.contains(expected),
            "stated {stated:?} lacks {expected:?}"
        );
    }
}

#[test]
fn a_state_script_carries_feeds_and_replays_their_topic_once() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "DEFINE TOPIC order_events; DEFINE FEED orders_feed ON orders INTO order_events; \
             CREATE orders:1 = { v: 1 }; CREATE orders:2 = { v: 2 };",
        )
        .unwrap();
    let answered = session.run_now("BACKUP SCRIPT;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::String(script))) = answered.last() else {
        panic!("BACKUP SCRIPT: {:?}", outcomes_last(&answered));
    };
    assert!(script.contains("DEFINE FEED orders_feed ON orders INTO order_events"));
    // The feed's definition rides after the data: replaying the records must
    // not capture them again.
    let restored = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    Session::new(&restored).run_now(script).unwrap();
    let mut session = inside(&restored);
    assert_eq!(messages(&mut session, "order_events").len(), 2);
    // The restored feed is live: a new write is captured.
    session.run_now("CREATE orders:3 = { v: 3 };").unwrap();
    assert_eq!(messages(&mut session, "order_events").len(), 3);
}

fn outcomes_last(outcomes: &[tessari_session::Outcome]) -> &tessari_session::Outcome {
    outcomes.last().expect("an outcome")
}

#[test]
fn when_filters_the_captured_writes() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "DEFINE TOPIC order_events; \
             DEFINE FEED orders_feed ON orders INTO order_events WHEN $after.total > 100;",
        )
        .unwrap();
    session.run_now("CREATE orders:1 = { total: 30 };").unwrap();
    session
        .run_now("CREATE orders:2 = { total: 300 };")
        .unwrap();
    let found = messages(&mut session, "order_events");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].get("key"), Some(&Value::from(2_i64)));
}

// --- The initial scan (D8): a background job owned by the feed, snapshot = the a background job owned by the feed, snapshot = the
// define commit, exactly-once per key, resumable across a restart. Driven by
// `tessari_session::scan_feeds` — the function the housekeeping cadence calls.

/// One object field of a value.
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    match value {
        Value::Object(fields) => fields.get(name),
        _ => None,
    }
}

/// Run one scan tick over every feed with `WITH INITIAL SCAN`.
fn scan(store: &Store) {
    tessari_session::scan_feeds(store).now().unwrap();
}

#[test]
fn initial_scan_backfills_existing_rows_as_create_messages() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "CREATE orders:1 = { v: 10 }; CREATE orders:2 = { v: 20 }; CREATE orders:3 = { v: 30 };",
        )
        .unwrap();
    session
        .run_now(
            "DEFINE TOPIC backfill; DEFINE FEED mine ON orders INTO backfill WITH INITIAL SCAN;",
        )
        .unwrap();
    scan(&store);
    let messages = messages(&mut session, "backfill");
    assert_eq!(messages.len(), 3, "one scanned message per existing row");
    let mut carried: Vec<i64> = messages
        .iter()
        .map(|message| {
            let after = message.get("after").expect("FULL image carries after");
            match field(after, "v") {
                Some(Value::Number(tessari_types::Number::Integer(held))) => *held,
                other => panic!("after.v is not an integer: {other:?}"),
            }
        })
        .collect();
    carried.sort_unstable();
    assert_eq!(carried, vec![10, 20, 30], "the scanned images are the rows");
}

#[test]
fn initial_scan_skips_rows_changed_after_the_define() {
    let store = store();
    let mut session = inside(&store);
    session.run_now("CREATE orders:1 = { v: 1 };").unwrap();
    session
        .run_now("DEFINE TOPIC live; DEFINE FEED mine ON orders INTO live WITH INITIAL SCAN;")
        .unwrap();
    // Between the define and the scan: one row changes, one is created. Their
    // live messages carry them; the scan must not add a second per key.
    session
        .run_now("UPDATE orders:1 SET v = 2; CREATE orders:2 = { v: 2 };")
        .unwrap();
    scan(&store);
    let messages = messages(&mut session, "live");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for message in &messages {
        *counts
            .entry(format!("{:?}", message.get("key").unwrap()))
            .or_default() += 1;
    }
    for (key, count) in &counts {
        assert_eq!(*count, 1, "exactly one message per key, got {key} x{count}");
    }
    assert_eq!(counts.len(), 2, "both rows are carried once");
    let ops: Vec<&Value> = messages
        .iter()
        .map(|message| message.get("op").unwrap())
        .collect();
    assert!(
        ops.contains(&&Value::from("CREATE")),
        "the new row is a live create"
    );
    assert!(
        ops.contains(&&Value::from("UPDATE")),
        "the changed row is a live update"
    );
}

#[test]
fn initial_scan_resumes_after_a_restart_without_a_second_message() {
    let directory = tempfile::tempdir().unwrap();
    let open = || {
        let backend = tessari_lsm::TessariKvBackend::open(
            directory.path(),
            tessari_lsm::StoreConfig::new(tessari_lsm::Durability::ProcessCrashSafe),
        )
        .now()
        .unwrap();
        Store::open(Arc::new(backend)).now().unwrap()
    };
    {
        let disk = open();
        let mut session = Session::new(&disk);
        session
            .run_now(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
                 DEFINE COLLECTION orders; DEFINE TOPIC backfill;",
            )
            .unwrap();
        for at in 1..=700 {
            session
                .run_now(&format!("CREATE orders:{at} = {{ v: {at} }};"))
                .unwrap();
        }
        session
            .run_now("DEFINE FEED mine ON orders INTO backfill WITH INITIAL SCAN;")
            .unwrap();
        scan(&disk);
        // One tick covers one bounded batch — the job is not done.
        let seen = {
            let mut session = inside(&disk);
            messages(&mut session, "backfill").len()
        };
        assert!(
            seen > 0 && seen < 700,
            "one tick scans one batch, saw {seen}"
        );
    }
    let disk = open();
    scan(&disk);
    scan(&disk);
    let mut session = inside(&disk);
    let messages = messages(&mut session, "backfill");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for message in &messages {
        *counts
            .entry(format!("{:?}", message.get("key").unwrap()))
            .or_default() += 1;
    }
    let dupes: Vec<_> = counts.iter().filter(|(_, n)| **n > 1).collect();
    println!(
        "DEBUG total={} unique={} dupes={dupes:?}",
        messages.len(),
        counts.len()
    );
    assert_eq!(
        messages.len(),
        700,
        "every row exactly once across restarts"
    );
    assert!(dupes.is_empty(), "duplicate keys: {dupes:?}");
}

#[test]
fn initial_scan_is_refused_inside_an_explicit_transaction() {
    let store = store();
    let mut session = inside(&store);
    let refused = session
        .run_now("BEGIN; DEFINE TOPIC t; DEFINE FEED mine ON orders INTO t WITH INITIAL SCAN;");
    assert!(
        matches!(refused, Err(Error::FeedScanInTransaction { .. })),
        "expected FeedScanInTransaction, got {refused:?}"
    );
}

#[test]
fn drop_feed_clears_the_scan_job_and_redefine_starts_fresh() {
    let store = store();
    let mut session = inside(&store);
    session.run_now("CREATE orders:1 = { v: 1 };").unwrap();
    session
        .run_now("DEFINE TOPIC t; DEFINE FEED mine ON orders INTO t WITH INITIAL SCAN;")
        .unwrap();
    session.run_now("DROP FEED mine ON orders;").unwrap();
    scan(&store);
    assert!(
        messages(&mut session, "t").is_empty(),
        "a dropped feed has no scan to run"
    );
    session
        .run_now("DEFINE FEED again ON orders INTO t WITH INITIAL SCAN;")
        .unwrap();
    scan(&store);
    assert_eq!(messages(&mut session, "t").len(), 1, "the fresh job scans");
}

#[test]
fn initial_scan_honours_the_feeds_when_and_image_keys() {
    let store = store();
    let mut session = inside(&store);
    session
        .run_now(
            "CREATE orders:1 = { v: 1 }; CREATE orders:2 = { v: 100 }; \
             DEFINE TOPIC t; \
             DEFINE FEED big ON orders INTO t FOR CREATE IMAGE KEYS WHEN $after.v > 10 WITH INITIAL SCAN;",
        )
        .unwrap();
    scan(&store);
    let messages = messages(&mut session, "t");
    assert_eq!(messages.len(), 1, "WHEN keeps only the rows it holds for");
    assert_eq!(messages[0].get("op"), Some(&Value::from("CREATE")));
    assert!(
        !messages[0].contains_key("after") && !messages[0].contains_key("before"),
        "IMAGE KEYS carries no images"
    );
}
