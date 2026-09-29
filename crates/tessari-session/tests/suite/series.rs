//! `DEFINE SERIES` — the statement, and what it declares.
//!
//! The floor itself is asserted in `tessari-storage`'s own suite, where a test
//! can read the substrate and prove the records below it are still there. What
//! is asserted here is the language half: that the statement declares what it
//! says, that `INFO` writes back the statement that created it, that a
//! retention which could only empty the answer is refused where it is written,
//! and that the word means what it says when it removes something.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE metrics; USE DATABASE metrics;",
        )
        .unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

fn refused(session: &mut Session<'_>, script: &str) -> String {
    match session.run(script) {
        Err(why) => why.to_string(),
        Ok(outcome) => panic!("expected a refusal, got {outcome:?}"),
    }
}

#[test]
fn a_series_is_declared_and_written_back_as_the_statement_that_made_it() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 12h;");

    let described = format!("{:?}", run(&mut session, "INFO FOR TABLE readings;"));
    assert!(
        described.contains("DEFINE SERIES readings RETAIN 12h"),
        "INFO answered with {described}"
    );
}

#[test]
fn a_retention_in_days_is_written_back_in_hours() {
    // Not a defect introduced here: `Duration::to_literal` normalises, and a
    // queue's timeout has always round-tripped the same way. Asserted rather
    // than left to be discovered, because `RETAIN 30d` is the spelling somebody
    // will actually write and `INFO` answering `720h` is a surprise worth
    // pinning to a test that says it is deliberate.
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 30d;");

    let described = format!("{:?}", run(&mut session, "INFO FOR TABLE readings;"));
    assert!(
        described.contains("DEFINE SERIES readings RETAIN 720h"),
        "INFO answered with {described}"
    );
}

#[test]
fn a_series_answers_with_a_record_written_now() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 30d;");
    run(&mut session, "CREATE readings = { celsius: 21 };");

    // The floor is thirty days back and the record is a moment old, so the
    // engine's ordinary case is that nothing is hidden at all.
    let answered = run(&mut session, "SELECT * FROM readings;");
    let Outcome::Records { records, .. } = answered else {
        panic!("a read answers with records");
    };
    assert_eq!(records.len(), 1);
}

#[test]
fn a_retention_that_could_only_empty_the_answer_is_refused_where_it_is_written() {
    let store = store();
    let mut session = opened(&store);

    for written in ["RETAIN 0s", "RETAIN -1d"] {
        let why = refused(&mut session, &format!("DEFINE SERIES readings {written};"));
        assert!(
            why.contains("leaves nothing to answer with"),
            "{written} was refused with {why}"
        );
    }
}

#[test]
fn a_series_needs_its_retention() {
    let store = store();
    let mut session = opened(&store);

    let why = refused(&mut session, "DEFINE SERIES readings;");
    assert!(why.contains("RETAIN"), "refused with {why}");
}

#[test]
fn dropping_a_series_that_is_a_plain_table_is_refused() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE readings SCHEMALESS;");

    // The word in the statement is a claim about what is being removed. A
    // `DROP SERIES` that removed a plain table would be a statement doing
    // something other than what it says.
    let why = refused(&mut session, "DROP SERIES readings;");
    assert!(why.contains("readings"), "refused with {why}");

    // And the table is still there, which is what makes the refusal a refusal
    // rather than a message printed after the fact.
    run(&mut session, "SELECT * FROM readings;");
}

/// The `at` of every record a read answered, in the order it answered them.
fn instants(outcome: Outcome) -> Vec<String> {
    let Outcome::Records { records, .. } = outcome else {
        panic!("a read answers with records");
    };
    records
        .iter()
        .map(|(_, value)| match value {
            tessari_types::Value::Object(fields) => fields["at"].to_string(),
            other => panic!("not a record: {other:?}"),
        })
        .collect()
}

#[test]
fn a_series_ordered_by_event_time_is_written_back_with_its_time_field() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 12h TIME at;");
    let described = format!("{:?}", run(&mut session, "INFO FOR TABLE readings;"));
    assert!(
        described.contains("DEFINE SERIES readings RETAIN 12h TIME at"),
        "INFO answered with {described}"
    );
}

/// G044 C2: a thousand runs, each writing the same thirty instants in a
/// different order, and each reading them back in event order — the order the
/// instants sort in, whatever order they arrived in. Two of the thirty share an
/// instant, so the answer also proves a tie is two records rather than one.
#[test]
fn late_events_land_in_event_order_whatever_order_they_arrive_in() {
    let written: Vec<String> = (0..30_u32)
        .map(|n| {
            // Seconds apart, with sub-millisecond parts, and one repeated.
            let n = if n == 29 { 7 } else { n };
            format!("2026-09-29T10:{:02}:{:02}.{:06}Z", n / 60, n % 60, n * 137)
        })
        .collect();
    let mut expected = written.clone();
    expected.sort();
    let mut state = 0x9e37_79b9_u64;
    for _ in 0..1_000 {
        let store = store();
        let mut session = opened(&store);
        run(
            &mut session,
            "DEFINE SERIES readings RETAIN 36500d TIME at;",
        );
        let mut order = written.clone();
        for index in (1..order.len()).rev() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let pick = usize::try_from(state >> 33).unwrap() % (index + 1);
            order.swap(index, pick);
        }
        for at in &order {
            run(
                &mut session,
                &format!("CREATE readings = {{ at: datetime '{at}' }};"),
            );
        }
        let answered = instants(run(&mut session, "SELECT * FROM readings;"));
        // Every instant is back, the repeated one twice, and in time order.
        let mut sorted = answered.clone();
        sorted.sort();
        assert_eq!(answered, sorted, "arrival order {order:?}");
        assert_eq!(answered.len(), expected.len());
        sorted.dedup();
        assert_eq!(sorted.len(), expected.len() - 1, "the tie was merged");
    }
}

/// G044 C10: an event identity keeps fourteen random bits, so two thousand
/// events at one instant collide by the hundred — and each is still its own
/// record, because a held identity is drawn again rather than refused.
#[test]
fn thousands_of_events_at_one_instant_are_each_kept() {
    let store = store();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE SERIES readings RETAIN 36500d TIME at;",
    );
    let script: String = (0..2_000)
        .map(|n| format!("CREATE readings = {{ at: datetime '2026-09-29T10:00:00Z', n: {n} }};\n"))
        .collect();
    session.run(&script).unwrap();
    let answered = instants(run(&mut session, "SELECT * FROM readings;"));
    assert_eq!(answered.len(), 2_000);
}

/// An instant `back` seconds before now, as a literal.
fn ago(back: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("time::from_unix({})", now.checked_sub(back).unwrap())
}

#[test]
fn the_floor_of_an_event_time_series_is_about_the_event() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 1h TIME at;");
    run(
        &mut session,
        &format!("CREATE readings = {{ at: {} }};", ago(1_800)),
    );
    let refusal = refused(
        &mut session,
        &format!("CREATE readings = {{ at: {} }};", ago(7_200)),
    );
    assert!(refusal.contains("past its retention"), "{refusal}");
    assert_eq!(
        instants(run(&mut session, "SELECT * FROM readings;")).len(),
        1
    );
}

#[test]
fn an_event_time_series_refuses_what_would_misplace_a_record() {
    let store = store();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE SERIES readings RETAIN 36500d TIME at;",
    );

    let missing = refused(&mut session, "CREATE readings = { celsius: 21 };");
    assert!(
        missing.contains("absent rather than a datetime"),
        "{missing}"
    );
    let text = refused(&mut session, "CREATE readings = { at: '2026-09-29' };");
    assert!(text.contains("string rather than a datetime"), "{text}");
    let early = refused(
        &mut session,
        "CREATE readings = { at: datetime '1969-12-31T23:59:59Z' };",
    );
    assert!(early.contains("from 1970 on"), "{early}");
    let named = refused(
        &mut session,
        "CREATE readings:uuid '0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b' = { at: datetime '2026-09-29T10:00:00Z' };",
    );
    assert!(named.contains("leave the identity out"), "{named}");

    let Outcome::Keys(keys) = run(
        &mut session,
        "CREATE readings = { at: datetime '2026-09-29T10:00:00Z', celsius: 21 };",
    ) else {
        panic!("a store-named create answers with its key");
    };
    let id = keys[0].to_literal();
    // Changing another field keeps the record where it is.
    run(
        &mut session,
        &format!("UPDATE readings:{id} SET celsius = 22;"),
    );
    let moved = refused(
        &mut session,
        &format!("UPDATE readings:{id} SET at = datetime '2026-09-29T11:00:00Z';"),
    );
    assert!(moved.contains("cannot change"), "{moved}");
}

/// G044 C11: the removal pass takes an aged record's index entries with it.
///
/// Through an ordinary read a record past the floor has no value, so index
/// upkeep keyed on the previous value would take nothing — the pass reads below
/// the floor for exactly this. Without it the entries would outlive their
/// records for good, since nothing afterwards could say which values they held.
#[test]
fn an_aged_records_index_entries_go_with_it() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE SERIES readings RETAIN 1h; DEFINE INDEX by_sensor ON readings FIELDS sensor;",
    );
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    // A UUID version 7 naming `millis`, `n` below the time.
    let named = |millis: u64, n: u64| {
        let stamp = format!("{millis:012x}");
        format!("{}-{}-7000-8000-{n:012x}", &stamp[..8], &stamp[8..])
    };
    let mut script = String::from("BEGIN;");
    for n in 0..1_000_u64 {
        script.push_str(&format!(
            " CREATE readings:uuid '{}' = {{ sensor: 's{}' }};",
            named(now - 2 * 60 * 60 * 1_000 + n, n),
            n % 10
        ));
    }
    for n in 0..10_u64 {
        script.push_str(&format!(
            " CREATE readings:uuid '{}' = {{ sensor: 's{n}' }};",
            named(now - 1_000, n)
        ));
    }
    script.push_str(" COMMIT;");
    session.run(&script).unwrap();

    let indexed = || {
        backend
            .scan(&tessari_kv::ScanRequest::new(
                tessari_kv::Keyspace::INDEX,
                tessari_kv::KeyRange::all(),
            ))
            .unwrap()
            .len()
    };
    let read = "SELECT * FROM readings WHERE sensor = 's3' USING INDEX by_sensor;";
    let before = format!("{:?}", run(&mut session, read));
    let entries = indexed();

    let (namespace, database, table) = {
        let mut transaction = store.begin().unwrap();
        let catalog = tessari_storage::Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("prod").unwrap().unwrap();
        let database = catalog.database_id(namespace, "metrics").unwrap().unwrap();
        let table = catalog
            .table_id(namespace, database, "readings")
            .unwrap()
            .unwrap();
        (namespace, database, table)
    };
    let expired = store.expire_series(namespace, database, table).unwrap();
    assert_eq!(expired.indexed, 1_000);
    assert_eq!(indexed(), entries - 1_000, "one entry per aged record gone");
    assert_eq!(format!("{:?}", run(&mut session, read)), before);
}
