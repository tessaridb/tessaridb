//! SG4 — hash partitioning at the statement level (G082): a GROUP BY on the
//! partition key folds partition-locally until a final merge (C5), and a
//! multi-row INSERT lands across every partition its rows route to in one
//! commit — all or nothing (C4).
//!
//! # What is asserted, and what deliberately is not
//!
//! Every answer is a differential against a row mirror holding the same rows —
//! the store's governing rule that an access path changes what a read costs
//! and never what it answers. The plan assertions pin the mechanism: the
//! columnar path, the lane count, the per-lane shape. The atomicity tests pin
//! the batch rule: a refused or cancelled insert leaves nothing, the
//! projection included.
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Now, Outcome, RunNow, Session};
use tessari_storage::{
    Catalog, FieldShape, HashPartitioning, RecordAddress, StorageKind, Store, TableShape,
};
use tessari_types::{DatabaseId, FieldKind, NamespaceId, Number, RecordId, TableId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

struct Fixture {
    events: (u32, u32, u32),
    mirror: (u32, u32, u32),
}

/// A columnar `events` table partitioned four ways by `kind`, beside a row
/// `mirror` holding the same rows. `seq` monotone, `kind` cycling four
/// values, every seventh record null on `kind`.
fn ready(store: &Store, rows: i64) -> (Session<'_>, Fixture) {
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog
        .create_database(namespace.id, "analytics")
        .now()
        .unwrap();
    let events = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            TableShape {
                schemafull: true,
                storage_kind: Some(StorageKind::Column),
                partitions: Some(HashPartitioning {
                    count: 4,
                    by: "kind".to_owned(),
                }),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    let mirror = catalog
        .create_table(
            namespace.id,
            database.id,
            "mirror",
            TableShape {
                schemafull: true,
                storage_kind: None,
                partitions: None,
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    // The events table's seq carries an ASSERT (the mid-batch refusal test's
    // lever); the mirror's fields stay plain.
    for table in [&events, &mirror] {
        catalog
            .create_field(
                table.id,
                "seq",
                FieldKind::Int,
                FieldShape {
                    assert: (table.id == events.id).then_some(tessari_types::Assertion::Compare {
                        op: tessari_types::BinaryOp::GreaterOrEqual,
                        against: tessari_types::Operand::Literal(Value::Number(Number::Integer(0))),
                    }),
                    ..FieldShape::default()
                },
            )
            .now()
            .unwrap();
        catalog
            .create_field(table.id, "kind", FieldKind::String, FieldShape::default())
            .now()
            .unwrap();
    }
    let fixture = Fixture {
        events: (namespace.id.get(), database.id.get(), events.id.get()),
        mirror: (namespace.id.get(), database.id.get(), mirror.id.get()),
    };
    // The catalog commits before the rows (the projection reads the
    // committed definition when the commit's maintain runs).
    transaction.commit().now().unwrap();
    let mut transaction = store.begin().now().unwrap();
    for row in 0..rows {
        let kind = if row % 7 == 3 {
            Value::Null
        } else {
            Value::String(format!("kind-{}", row.rem_euclid(4)))
        };
        let payload = encode_payload(&Value::Object(BTreeMap::from([
            ("seq".to_owned(), Value::Number(Number::Integer(row))),
            ("kind".to_owned(), kind),
        ])))
        .into_bytes();
        for (namespace, database, table) in [fixture.events, fixture.mirror] {
            transaction.put(
                RecordAddress::new(
                    NamespaceId::new(namespace),
                    DatabaseId::new(database),
                    TableId::new(table),
                    RecordId::Int(row),
                ),
                payload.clone(),
            );
        }
    }
    transaction.commit().now().unwrap();
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE analytics;")
        .unwrap();
    (session, fixture)
}

/// The grouped answer a table gives, as `(group key text, count, sum)` rows
/// in the store's order.
fn grouped(session: &mut Session<'_>, table: &str) -> Vec<(String, i64, i64)> {
    let outcomes = session
        .run_now(&format!(
            "SELECT kind, count(*) AS n, sum(seq) AS s FROM {table} GROUP BY kind;"
        ))
        .unwrap();
    let outcome = outcomes.last().unwrap();
    outcome
        .records()
        .unwrap_or_else(|| panic!("records answered with {outcome:?}"))
        .iter()
        .map(|(_, record)| {
            let Value::Object(fields) = record else {
                panic!("a group answers an object: {record:?}");
            };
            let key = match fields.get("kind") {
                Some(Value::String(kind)) => format!("{kind:?}"),
                other => format!("{other:?}"),
            };
            let count = match fields.get("n") {
                Some(Value::Number(Number::Integer(count))) => *count,
                other => panic!("count answers a number: {other:?}"),
            };
            let sum = match fields.get("s") {
                Some(Value::Number(Number::Integer(sum))) => *sum,
                other => panic!("sum answers a number: {other:?}"),
            };
            (key, count, sum)
        })
        .collect()
}

/// C5: a GROUP BY on the partition key is partition-local until a final
/// merge, answers equal to the row mirror's, and the plan says so.
#[test]
fn a_group_by_on_the_partition_key_folds_per_lane() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 500);
    assert_eq!(
        grouped(&mut session, "events"),
        grouped(&mut session, "mirror"),
        "the paths disagreed"
    );

    let outcomes = session
        .run_now("EXPLAIN SELECT kind, count(*) AS n FROM events GROUP BY kind;")
        .unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    assert_eq!(
        format!("{:?}", fields.get("access").unwrap_or(&Value::None)),
        "String(\"columnar\")",
    );
    assert_eq!(
        format!("{:?}", fields.get("lanes").unwrap_or(&Value::None)),
        "Number(Integer(4))",
        "the plan says partition-local over four lanes"
    );
}

/// Null keys group the same on both paths: they routed to partition zero
/// with no key-family row, and the union of the families' rows keeps them.
#[test]
fn null_keys_group_the_same_as_the_mirror() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 500);
    let from_events = grouped(&mut session, "events");
    let from_mirror = grouped(&mut session, "mirror");
    assert!(
        from_events
            .iter()
            .any(|(key, _, _)| key.contains("Null") || key.contains("None")),
        "the null-key group is present: {from_events:?}"
    );
    assert_eq!(from_events, from_mirror);
}

/// C4: one multi-row INSERT across partitions is visible together after its
/// commit, each row answering through the lane its key routes to.
#[test]
fn a_multi_row_insert_spans_partitions_in_one_commit() {
    let store = store();
    let (mut session, fixture) = ready(&store, 100);
    session
        .run_now(
            "INSERT INTO events (seq, kind) VALUES\n\
             \t(1000, 'kind-0'), (1001, 'kind-1'), (1002, 'kind-2'), (1003, 'kind-3');",
        )
        .unwrap();
    session
        .run_now(
            "INSERT INTO mirror (seq, kind) VALUES\n\
             \t(1000, 'kind-0'), (1001, 'kind-1'), (1002, 'kind-2'), (1003, 'kind-3');",
        )
        .unwrap();

    // Every row of the batch answers, through the columnar path for the
    // partition-key equality that pins its lane.
    for row in 1000..1004 {
        let kind = format!("kind-{}", row - 1000);
        let outcomes = session
            .run_now(&format!(
                "SELECT * FROM events WHERE kind = '{kind}' AND seq = {row};"
            ))
            .unwrap();
        let outcome = outcomes.last().unwrap();
        assert_eq!(
            outcome
                .plan()
                .unwrap_or_else(|| panic!("{outcome:?}"))
                .access,
            AccessPath::Columnar,
        );
        assert_eq!(
            outcome
                .records()
                .unwrap_or_else(|| panic!("{outcome:?}"))
                .len(),
            1,
            "row {row} answers"
        );
    }
    let _ = fixture;
}

/// C4: a mid-batch refusal refuses the whole — the violating row and every
/// sibling land nowhere, the projection included.
#[test]
fn a_mid_batch_refusal_leaves_nothing() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 100);
    let refused = session.run_now(
        "INSERT INTO events (seq, kind) VALUES (2000, 'kind-0'), (-1, 'kind-1'), (2002, 'kind-2');",
    );
    assert!(
        refused.is_err(),
        "a violating row refuses the statement: {refused:?}"
    );
    for row in [2000, 2002] {
        let outcomes = session
            .run_now(&format!(
                "SELECT * FROM events WHERE seq = {row} AND kind != 'kind-9';"
            ))
            .unwrap();
        assert_eq!(
            outcomes
                .last()
                .unwrap()
                .records()
                .unwrap_or_else(|| panic!("{outcomes:?}"))
                .len(),
            0,
            "the sibling {row} landed nothing"
        );
    }
}

/// C4: CANCEL discards the batch — nothing of the insert is anywhere.
#[test]
fn cancel_leaves_nothing() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 100);
    session
        .run_now(
            "BEGIN;\n\
             INSERT INTO events (seq, kind) VALUES (3000, 'kind-0'), (3001, 'kind-1');\n\
             CANCEL;",
        )
        .unwrap();
    let outcomes = session
        .run_now("SELECT * FROM events WHERE seq >= 3000 AND seq < 3002 AND kind != 'kind-9';")
        .unwrap();
    assert!(
        outcomes
            .last()
            .unwrap()
            .records()
            .unwrap_or_else(|| panic!("{outcomes:?}"))
            .is_empty(),
        "CANCEL discarded the insert"
    );
}

/// An update crossing lanes and a delete stay exact at the statement level.
#[test]
fn updates_and_deletes_stay_exact_across_lanes() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 200);
    session
        .run_now("UPDATE events:5 = { seq: 5000, kind: 'kind-3' };")
        .unwrap();
    session.run_now("DELETE events:9;").unwrap();
    // The same writes on the mirror, so the differential compares answers
    // and not the absence of them.
    session
        .run_now("UPDATE mirror:5 = { seq: 5000, kind: 'kind-3' };")
        .unwrap();
    session.run_now("DELETE mirror:9;").unwrap();
    assert_eq!(
        grouped(&mut session, "events"),
        grouped(&mut session, "mirror"),
        "the paths disagreed after the writes"
    );
}
