//! C3 — the measured evidence for the columnar path :
//! selective aggregates over 1 048 576 rows, answered through the columnar
//! scan, equal to the row mirror's answers, with the pruning figures the
//! the design record asks the evidence to carry.
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

/// The records a read answered with, and the plan and notes it carried.
fn read(session: &mut Session<'_>, table: &str, predicate: &str) -> (usize, AccessPath, Vec<Note>) {
    let outcomes = session
        .run_now(&format!("SELECT * FROM {table} WHERE {predicate};"))
        .unwrap();
    let outcome = outcomes.last().unwrap();
    (
        outcome.records().map_or(0, |records| records.len()),
        outcome
            .plan()
            .unwrap_or_else(|| panic!("{outcome:?}"))
            .access,
        outcome.notes().to_vec(),
    )
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

/// C8 — the write path's cost, stated honestly. One transaction holds the
/// same 100 000 rows for each layout (single-variable: identical rows,
/// identical batching, only the storage kind differs); the measured figure
/// is the commit's wall time divided by the rows. Debug build, in-memory
/// store — the shape the number's barbs point at, stated beside it.
#[test]
fn c8_the_write_paths_cost_stated() {
    const WRITES: i64 = 100_000;
    /// The same count as a float, so the per-row division casts nothing.
    const WRITES_F64: f64 = 100_000.0;
    let mut seconds = Vec::new();
    for column in [false, true] {
        let store = store();
        let mut transaction = store.begin().now().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").now().unwrap();
        let database = catalog
            .create_database(namespace.id, "analytics")
            .now()
            .unwrap();
        let table = catalog
            .create_table(
                namespace.id,
                database.id,
                "events",
                TableShape {
                    schemafull: true,
                    storage_kind: column.then_some(StorageKind::Column),
                    ..TableShape::default()
                },
            )
            .now()
            .unwrap()
            .id;
        catalog
            .create_field(table, "kind", FieldKind::String, FieldShape::default())
            .now()
            .unwrap();
        transaction.commit().now().unwrap();
        let mut transaction = store.begin().now().unwrap();
        for row in 0..WRITES {
            let payload = encode_payload(&Value::Object(BTreeMap::from([(
                "kind".to_owned(),
                Value::String(format!("kind-{}", row.rem_euclid(100))),
            )])))
            .into_bytes();
            transaction.put(
                RecordAddress::new(
                    NamespaceId::new(namespace.id.get()),
                    DatabaseId::new(database.id.get()),
                    TableId::new(table.get()),
                    RecordId::Int(row),
                ),
                payload,
            );
        }
        let started = Instant::now();
        transaction.commit().now().unwrap();
        seconds.push(started.elapsed().as_secs_f64());
    }
    let row_per = seconds[0] / WRITES_F64;
    let column_per = seconds[1] / WRITES_F64;
    eprintln!(
        "c8 | write | rows {WRITES} | debug, in-memory | row {:.0} ns/row | column {:.0} ns/row | ratio {:.2}x",
        row_per * 1e9,
        column_per * 1e9,
        column_per / row_per
    );
    // The columnar write pays the projection inside the same commit; the
    // measured claim is the RATIO, not a threshold — the assertion pins
    // only that the projection landed (the commit answered), the printed
    // line is the evidence.
    assert!(column_per.is_finite() && row_per.is_finite());
}

#[test]
fn c1_lane_statistics_and_in_lists_over_a_sized_table() {
    let store = store();
    let (mut session, _events, _mirror) = ready(&store);
    let sealed: u64 = ROWS.div_euclid(8192).try_into().expect("rows fit a u64");

    // Out-of-range equality: the lane statistics deny every sealed chunk
    // without reading a head; the scan consults the open chunk alone.
    let started = Instant::now();
    let (zero_rows, zero_plan, zero_notes) = read(&mut session, "events", "seq = 2000000");
    let lane_ms = started.elapsed().as_millis();
    assert_eq!(zero_rows, 0, "no row reaches two million");
    assert_eq!(zero_plan, AccessPath::Columnar);
    let Note::Columnar {
        chunks_examined,
        chunks_skipped,
        rows_decoded,
        rows_returned,
    } = zero_notes
        .iter()
        .find(|note| matches!(note, Note::Columnar { .. }))
        .expect("the columnar note")
    else {
        panic!("the note is columnar");
    };
    assert_eq!(
        *chunks_examined, 1,
        "the lane denied {sealed} sealed chunks unread; the open chunk alone was examined"
    );
    assert_eq!(*chunks_skipped, 1, "the open chunk's head denies the value");
    assert_eq!(*rows_decoded, 0);
    assert_eq!(*rows_returned, 0);

    // An IN-list of scattered values: one scan pass answers the union, and
    // the sealed count is what the lane statistics spared on the shape
    // above. The mirror walks the whole table for every member's filter.
    let members = [
        1_000, 50_000, 200_000, 350_000, 500_000, 650_000, 800_000, 1_000_000,
    ];
    let list = members
        .iter()
        .map(|member| member.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let started = Instant::now();
    let (in_count, in_plan, in_notes) = count(&mut session, "events", &format!("seq IN [{list}]"));
    let in_ms = started.elapsed().as_millis();
    assert_eq!(in_plan, AccessPath::Columnar);
    let member_count = u64::try_from(members.len()).expect("eight members fit a u64");
    let Note::Columnar {
        chunks_examined: _,
        chunks_skipped,
        rows_decoded,
        rows_returned,
    } = in_notes
        .iter()
        .find(|note| matches!(note, Note::Columnar { .. }))
        .expect("the columnar note")
    else {
        panic!("the note is columnar");
    };
    assert_eq!(
        *rows_returned, member_count,
        "every member answers exactly its row"
    );
    assert_eq!(
        in_count,
        i64::try_from(members.len()).expect("members fit an i64")
    );
    let table_rows = u64::try_from(ROWS).expect("the row count is positive");
    // Eight probes per chunk head make the bloom false-positive on a share
    // of the chunks no member touches; the bound the mechanism promises is
    // "decodes a minority of the table", and the printed figure is the
    // measured one.
    assert!(
        *rows_decoded < table_rows.div_euclid(2),
        "the list decodes a minority of the table, not the table: {rows_decoded}"
    );

    let started = Instant::now();
    let (walk_count, walk_plan, _) = count(
        &mut session,
        "mirror",
        &members
            .iter()
            .map(|member| format!("seq = {member}"))
            .collect::<Vec<_>>()
            .join(" OR "),
    );
    let walk_ms = started.elapsed().as_millis();
    assert_eq!(walk_count, in_count, "the paths disagree on the IN-list");
    assert_eq!(walk_plan, AccessPath::Scan);

    eprintln!(
        "C1 figures ({ROWS} rows, {sealed} sealed chunks): lane deny {lane_ms} ms (1 head read, {sealed} denied unread); IN-list {in_ms} ms columnar vs {walk_ms} ms row walk, decoded {rows_decoded}, skipped {chunks_skipped}",
    );
}
