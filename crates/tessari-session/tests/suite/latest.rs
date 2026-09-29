//! `LATEST BY <field>` — the newest record per key on a series (ADR-0088 §3,
//! G044 C4). The oracle is the same answer computed in the test from what was
//! written; the index path and the scan path must both equal it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::arithmetic_side_effects)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn opened(store: &Store, index: bool) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE SERIES readings RETAIN 36500d TIME at;",
        )
        .unwrap();
    if index {
        session
            .run("DEFINE INDEX by_sensor ON readings FIELDS sensor;")
            .unwrap();
    }
    session
}

/// `keys × each` readings in one transaction, in an order that is not time
/// order, and the newest `v` per key the writer knows it wrote.
fn write(session: &mut Session<'_>, keys: u32, each: u32) -> BTreeMap<String, i64> {
    let mut expected = BTreeMap::new();
    let mut script = String::from("BEGIN;");
    for step in 0..each {
        for key in 0..keys {
            // Written newest-first for even keys, so arrival order is not time
            // order and a read that answered the last written would be wrong.
            let minute = if key % 2 == 0 { each - 1 - step } else { step };
            let v = i64::from(key) * 1_000 + i64::from(minute);
            script.push_str(&format!(
                " CREATE readings = {{ sensor: 's{key:04}', v: {v}, \
                 at: datetime '2026-09-29T{:02}:{:02}:00Z' }};",
                minute / 60,
                minute % 60
            ));
            let newest = expected.entry(format!("s{key:04}")).or_insert(v);
            *newest = (*newest).max(v);
        }
    }
    script.push_str(" COMMIT;");
    session.run(&script).unwrap();
    expected
}

/// The newest `v` per sensor the read answered, and the path it took.
fn latest(session: &mut Session<'_>, read: &str) -> (BTreeMap<String, i64>, AccessPath) {
    let Outcome::Records { records, plan, .. } = session.run(read).unwrap().pop().unwrap() else {
        panic!("a read answers with records");
    };
    let count = records.len();
    let answered: BTreeMap<String, i64> = records
        .into_iter()
        .map(|(_, record)| {
            let Value::Object(fields) = record else {
                panic!("a record is an object");
            };
            let Value::String(sensor) = &fields["sensor"] else {
                panic!("sensor is text");
            };
            let Value::Number(v) = &fields["v"] else {
                panic!("v is a number");
            };
            (sensor.clone(), v.to_string().parse().unwrap())
        })
        .collect();
    // Collected into a map, which would absorb a duplicate: one record per key
    // is asserted on the answer itself.
    assert_eq!(count, answered.len(), "a key was answered more than once");
    (answered, plan.access)
}

#[test]
fn the_newest_record_per_key_is_answered_by_the_index_and_by_the_scan_alike() {
    for index in [true, false] {
        let store = store();
        let mut session = opened(&store, index);
        let expected = write(&mut session, 7, 20);
        let (answered, path) = latest(&mut session, "SELECT * FROM readings LATEST BY sensor;");
        assert_eq!(answered, expected, "index: {index}");
        let wanted = if index {
            AccessPath::Index
        } else {
            AccessPath::Scan
        };
        assert_eq!(path, wanted);
    }
}

#[test]
fn a_condition_chooses_among_the_records_it_admits() {
    let store = store();
    let mut session = opened(&store, true);
    write(&mut session, 3, 10);
    // Minutes 0–4 only: the newest admitted per key is minute 4, not minute 9.
    let (answered, path) = latest(
        &mut session,
        "SELECT * FROM readings WHERE at < datetime '2026-09-29T00:05:00Z' LATEST BY sensor;",
    );
    let expected: BTreeMap<String, i64> = (0..3)
        .map(|key| (format!("s{key:04}"), i64::from(key) * 1_000 + 4))
        .collect();
    assert_eq!(answered, expected);
    assert_ne!(
        path,
        AccessPath::Index,
        "a condition cannot take the index walk"
    );
}

#[test]
fn latest_is_refused_where_newest_has_no_meaning() {
    let store = store();
    let mut session = opened(&store, false);
    session.run("DEFINE COLLECTION plain;").unwrap();
    let refused =
        |session: &mut Session<'_>, read: &str| session.run(read).unwrap_err().to_string();
    let plain = refused(&mut session, "SELECT * FROM plain LATEST BY sensor;");
    assert!(plain.contains("only a series"), "{plain}");
    let grouped = refused(
        &mut session,
        "SELECT sensor, count(*) AS n FROM readings LATEST BY sensor GROUP BY sensor;",
    );
    assert!(grouped.contains("say one"), "{grouped}");
}

/// G044 C4 timing: 100 000 points over 1 000 keys, index walk against a scan.
/// Ignored in the suite for its length; run with `--ignored`.
#[test]
#[ignore = "G044 C4 evidence: 100 000 points x 1 000 keys, index walk vs scan"]
fn a_thousand_keys_are_read_in_a_thousand_seeks() {
    let store = store();
    let mut session = opened(&store, true);
    let expected = write(&mut session, 1_000, 100);
    let started = Instant::now();
    let (by_index, path) = latest(&mut session, "SELECT * FROM readings LATEST BY sensor;");
    let indexed = started.elapsed();
    assert_eq!(path, AccessPath::Index);
    let started = Instant::now();
    let (by_scan, scanned_path) = latest(
        &mut session,
        "SELECT * FROM readings WHERE v >= 0 LATEST BY sensor;",
    );
    let scanned = started.elapsed();
    assert_ne!(scanned_path, AccessPath::Index);
    assert_eq!(by_index, expected);
    assert_eq!(by_scan, expected);
    println!("G044 C4: index {indexed:?} · scan {scanned:?} · 1 000 keys, 100 000 points");
}
