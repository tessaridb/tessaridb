//! C3 — the measured evidence for the columnar path (G082 SG3 T3.3):
//! selective aggregates over 1 048 576 rows, answered through the columnar
//! scan, equal to the row mirror's answers, with the pruning figures the
//! worksheet §6.2 asks the evidence to carry.
//!
//! The timings print rather than assert: a timing assertion would be a
//! flake wearing a criterion. What is asserted is exactness (both paths
//! answer the same number), the path (the columnar plan served the read),
//! and the pruning (the selective shapes skipped most of the chunks; the
//! control skipped none).
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Note, Now, RunNow, Session};
use tessari_storage::{Catalog, FieldShape, RecordAddress, StorageKind, Store, TableShape};
use tessari_types::{DatabaseId, FieldKind, NamespaceId, Number, RecordId, TableId, Value};

const ROWS: i64 = 1_048_576;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// The two tables, committed in batches so no single commit holds the
/// whole million: `seq` monotone, `kind` cycling a hundred values (so one
/// kind selects ~1 %), every seventh record null on `kind`. `kind-99` is
/// the equality-pruning shape: it sits in one contiguous band, so most
/// chunks' blooms deny it — a uniformly scattered rare value would be
/// honestly unprunable (a chunk of 8 192 holds ~82 of any 1 % value) and
/// would measure nothing about the bloom.
fn ready(store: &Store) -> (Session<'_>, (u32, u32, u32), (u32, u32, u32)) {
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog
        .create_database(namespace.id, "analytics")
        .now()
        .unwrap();
    let shape = |storage_kind| TableShape {
        schemafull: true,
        storage_kind,
        ..TableShape::default()
    };
    let events = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            shape(Some(StorageKind::Column)),
        )
        .now()
        .unwrap();
    let mirror = catalog
        .create_table(namespace.id, database.id, "mirror", shape(None))
        .now()
        .unwrap();
    for table in [&events, &mirror] {
        catalog
            .create_field(table.id, "seq", FieldKind::Int, FieldShape::default())
            .now()
            .unwrap();
        catalog
            .create_field(table.id, "kind", FieldKind::String, FieldShape::default())
            .now()
            .unwrap();
    }
    let events = (namespace.id.get(), database.id.get(), events.id.get());
    let mirror = (namespace.id.get(), database.id.get(), mirror.id.get());
    transaction.commit().now().unwrap();

    const BATCH: i64 = 131_072;
    let mut first = 0;
    while first < ROWS {
        let mut transaction = store.begin().now().unwrap();
        for row in first..first.saturating_add(BATCH) {
            let kind = if row % 7 == 3 {
                Value::Null
            } else if (900_000..910_000).contains(&row) {
                Value::String("kind-99".to_owned())
            } else {
                Value::String(format!("kind-{}", row.rem_euclid(99)))
            };
            let payload = encode_payload(&Value::Object(BTreeMap::from([
                ("seq".to_owned(), Value::Number(Number::Integer(row))),
                ("kind".to_owned(), kind),
            ])))
            .into_bytes();
            for (namespace, database, table) in [events, mirror] {
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
        first = first.saturating_add(BATCH);
    }

    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE analytics;")
        .unwrap();
    (session, events, mirror)
}

/// The count a fold answered with, and the plan and notes it carried.
fn count(session: &mut Session<'_>, table: &str, predicate: &str) -> (i64, AccessPath, Vec<Note>) {
    let outcomes = session
        .run_now(&format!(
            "SELECT count(*) AS n FROM {table} WHERE {predicate};"
        ))
        .unwrap();
    let outcome = outcomes.last().unwrap();
    let records = outcome.records().unwrap_or_else(|| panic!("{outcome:?}"));
    let Value::Object(fields) = &records[0].1 else {
        panic!("a fold answers an object: {outcome:?}");
    };
    let count = match fields.get("n") {
        Some(Value::Number(Number::Integer(count))) => *count,
        other => panic!("the fold answers a number: {other:?}"),
    };
    (
        count,
        outcome
            .plan()
            .unwrap_or_else(|| panic!("{outcome:?}"))
            .access,
        outcome.notes().to_vec(),
    )
}

#[test]
fn c3_selective_aggregates_over_a_million_rows() {
    let store = store();
    let (mut session, _events, _mirror) = ready(&store);

    let top_one_percent = ROWS.saturating_sub(ROWS / 100);
    let shapes = [
        ("seq top-1% range", format!("seq >= {top_one_percent}")),
        ("rare-kind equality", "kind = 'kind-99'".to_owned()),
        ("control unselective", "seq >= 0".to_owned()),
    ];
    for (label, predicate) in shapes {
        let started = Instant::now();
        let (columnar_count, columnar_plan, notes) = count(&mut session, "events", &predicate);
        let columnar_ms = started.elapsed().as_millis();

        let started = Instant::now();
        let (row_count, row_plan, _) = count(&mut session, "mirror", &predicate);
        let row_ms = started.elapsed().as_millis();

        assert_eq!(
            columnar_count, row_count,
            "the paths disagreed on `{label}` ({predicate})"
        );
        assert_eq!(columnar_plan, AccessPath::Columnar, "{label}");
        assert_eq!(
            row_plan,
            AccessPath::Scan,
            "{label} (the mirror is a row table)"
        );
        let note = notes
            .iter()
            .find_map(|note| match note {
                Note::Columnar {
                    chunks_examined,
                    chunks_skipped,
                    rows_decoded,
                    rows_returned,
                } => Some((
                    *chunks_examined,
                    *chunks_skipped,
                    *rows_decoded,
                    *rows_returned,
                )),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the columnar note rode the {label} answer"));
        let (examined, skipped, decoded, returned) = note;
        match label {
            "control unselective" => assert_eq!(skipped, 0, "{label} prunes nothing"),
            _ => assert!(
                skipped > 100,
                "{label} skipped only {skipped} of {examined} chunks"
            ),
        }
        eprintln!(
            "c3 | {label:20} | rows {ROWS} | {predicate:28} | count {columnar_count:7} | \
             examined {examined:4} skipped {skipped:4} decoded {decoded:8} returned {returned:8} | \
             columnar {columnar_ms:5} ms | row {row_ms:5} ms"
        );
    }
}
