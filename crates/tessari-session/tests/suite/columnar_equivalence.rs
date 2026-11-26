//! SG6 — the differential equivalence suite : a columnar table and
//! its row mirror hold the same logical data, and every read shape the
//! language has answers EXACTLY what the row store would. Zero mismatches is
//! the bar; a mismatch is a defect in the projection or the gate, never an
//! expectation update (the differential oracle rule, the design record).
//!
//! # What the matrix covers
//!
//! Folds (count/sum/min/max/avg), GROUP BY on the partition key and off it,
//! filters (equality with the lane pin, range with chunk pruning, `!=` on the
//! row path by the design record), ORDER BY with a bound, point GETs, a window over the
//! mirror's ordering (row-path for a columnar table, still exact), string
//! folds across the codec choices, and the hostile corners: nulls every
//! seventh row, updates crossing lanes, deletes (tombstone and delete-null
//! slots), all re-run after the mutations land.
#![allow(clippy::panic, clippy::unwrap_used)]

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

/// A partitioned columnar `events` beside a row `mirror`, same rows both.
/// `seq` monotone, `kind` cycling four values with every seventh null,
/// `amount` cycling for folds, `note` drawn from four words for the codec
/// cascade.
fn ready(store: &Store, rows: i64) -> (Session<'_>, Fixture) {
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog
        .create_database(namespace.id, "analytics")
        .now()
        .unwrap();
    let mut shape = |name: &str, column: bool| {
        catalog
            .create_table(
                namespace.id,
                database.id,
                name,
                TableShape {
                    schemafull: true,
                    storage_kind: column.then_some(StorageKind::Column),
                    partitions: column.then(|| HashPartitioning {
                        count: 4,
                        by: "kind".to_owned(),
                    }),
                    ..TableShape::default()
                },
            )
            .now()
            .unwrap()
    };
    let events = shape("events", true);
    let mirror = shape("mirror", false);
    for table in [&events, &mirror] {
        for (field, kind) in [
            ("seq", FieldKind::Int),
            ("kind", FieldKind::String),
            ("amount", FieldKind::Int),
            ("note", FieldKind::String),
        ] {
            catalog
                .create_field(table.id, field, kind, FieldShape::default())
                .now()
                .unwrap();
        }
    }
    let fixture = Fixture {
        events: (namespace.id.get(), database.id.get(), events.id.get()),
        mirror: (namespace.id.get(), database.id.get(), mirror.id.get()),
    };
    // The catalog commits before the rows: the projection reads the committed
    // definition when the commit's maintain runs (the storage tests' rule).
    transaction.commit().now().unwrap();

    let words = ["alpha", "beta", "gamma", "delta"];
    let note = |row: i64| words[usize::try_from(row.rem_euclid(4)).unwrap_or(0)];
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
            (
                "amount".to_owned(),
                Value::Number(Number::Integer(row.rem_euclid(97))),
            ),
            ("note".to_owned(), Value::from(note(row))),
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

/// Run one read on both tables and return `(events_answer, mirror_answer,
/// events_access)`. The answers are compared by the caller; the access path
/// is returned so the columnar arms can assert the path they meant to take.
#[allow(clippy::type_complexity)]
fn both(
    session: &mut Session<'_>,
    events: &str,
    mirror: &str,
    read: &str,
) -> (Vec<(RecordId, Value)>, Vec<(RecordId, Value)>, AccessPath) {
    let render = |table: &str| read.replace("{t}", table);
    let events_outcome = session.run_now(&render(events)).unwrap().pop().unwrap();
    let mirror_outcome = session.run_now(&render(mirror)).unwrap().pop().unwrap();
    let access = events_outcome
        .plan()
        .map(|plan| plan.access)
        .unwrap_or(AccessPath::Record);
    let records = |outcome: Outcome| match outcome {
        Outcome::Records { records, .. } => records,
        other => panic!("expected records, got {other:?}"),
    };
    (records(events_outcome), records(mirror_outcome), access)
}

#[test]
fn folds_group_by_and_filters_answer_equal_to_the_mirror() {
    let store = store();
    let (mut session, fixture) = ready(&store, 500);

    // A fold matrix, grouped by the partition key: the columnar path folds
    // per lane and merges; the mirror folds the row way; the answers are one.
    for fold in [
        "count(*)",
        "sum(amount)",
        "min(amount)",
        "max(amount)",
        "mean(amount)",
    ] {
        let (events, mirror, _) = both(
            &mut session,
            "events",
            "mirror",
            &format!("SELECT kind, {fold} AS f FROM {{t}} GROUP BY kind;"),
        );
        assert_eq!(events, mirror, "the fold {fold} diverged");
    }

    // GROUP BY off the partition key answers through the ordinary path and
    // still equals the mirror.
    let (events, mirror, access) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT note, count(*) AS n FROM {t} GROUP BY note;",
    );
    assert_eq!(events, mirror, "the off-key group by diverged");
    assert_ne!(
        access,
        AccessPath::Columnar,
        "an off-key group by is not partition-local"
    );

    // Equality on the partition key pins a lane (the columnar path).
    let (events, mirror, access) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT * FROM {t} WHERE kind = 'kind-1' AND seq = 5;",
    );
    assert_eq!(events, mirror, "the pinned equality diverged");
    assert_eq!(
        access,
        AccessPath::Columnar,
        "the equality took the row path"
    );

    // A range folds through the pruning scan; `!=` routes to the walk by
    // the design record — both must answer equal.
    let (events, mirror, access) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT count(*) AS n FROM {t} WHERE seq >= 9000 AND seq < 9100;",
    );
    assert_eq!(events, mirror, "the range fold diverged");
    assert_eq!(access, AccessPath::Columnar, "the range took the row path");
    let (events, mirror, access) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT count(*) AS n FROM {t} WHERE kind != 'kind-9';",
    );
    assert_eq!(events, mirror, "the != fold diverged");
    assert_ne!(access, AccessPath::Columnar, "!= never prunes ");

    // String folds across the codec cascade.
    let (events, mirror, _) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT note, sum(amount) AS s FROM {t} GROUP BY note;",
    );
    assert_eq!(events, mirror, "the string fold diverged");
    let _ = fixture;
}

#[test]
fn ordered_bounded_point_and_window_reads_answer_equal() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 300);

    // ORDER BY with a bound: the order is the answer's shape, so the
    // comparison is order-sensitive.
    let (events, mirror, _) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT seq, amount FROM {t} ORDER BY seq DESC LIMIT 10;",
    );
    assert_eq!(events, mirror, "the ordered bounded read diverged");

    // A point GET is row-path for a columnar table (C7) and exact.
    let (events, mirror, access) = both(&mut session, "events", "mirror", "SELECT * FROM {t}:7;");
    assert_eq!(events, mirror, "the point get diverged");
    assert_eq!(access, AccessPath::Record, "a point get is row-path (C7)");

    // A window over the ordering the mirror shares: row-path on the columnar
    // table, and the answers still one.
    let (events, mirror, _) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT seq, row_number() OVER (ORDER BY seq) AS n FROM {t} LIMIT 8;",
    );
    assert_eq!(events, mirror, "the window diverged");
}

#[test]
fn mutations_across_lanes_keep_the_answers_one() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 400);
    // The same writes on both tables: an update crossing lanes, a delete,
    // and an update that lands on a null key.
    for table in ["events", "mirror"] {
        session
            .run_now(&format!(
                "UPDATE {table}:5 = {{ seq: 5000, kind: 'kind-3', amount: 1, note: 'alpha' }};"
            ))
            .unwrap();
        session.run_now(&format!("DELETE {table}:9;")).unwrap();
        session
            .run_now(&format!(
                "UPDATE {table}:11 = {{ seq: 11, kind: NULL, amount: 2, note: 'beta' }};"
            ))
            .unwrap();
    }
    for fold in ["count(*)", "sum(amount)"] {
        let (events, mirror, _) = both(
            &mut session,
            "events",
            "mirror",
            &format!("SELECT kind, {fold} AS f FROM {{t}} GROUP BY kind;"),
        );
        assert_eq!(events, mirror, "after the writes, the fold {fold} diverged");
    }
    let (events, mirror, _) = both(
        &mut session,
        "events",
        "mirror",
        "SELECT seq, amount FROM {t} ORDER BY seq DESC LIMIT 12;",
    );
    assert_eq!(
        events, mirror,
        "after the writes, the ordered read diverged"
    );
}

/// A `SORT BY` twin beside the mirror (ADR-0135): the same rows arrive in
/// shuffled commit order, the projection lays them out by `seq`, and every
/// read shape answers what the row mirror answers — the standing oracle,
/// asked of the sorted layout.
#[test]
fn a_sorted_columnar_table_answers_exactly_what_the_mirror_does() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                  DEFINE DATABASE analytics; USE DATABASE analytics;",
        )
        .unwrap();
    // The clause rides only a columnar table, and a schemafull declaration
    // must declare the key — both refused before anything is created.
    assert!(
        session
            .run_now("DEFINE TABLE rowish (seq int) STORE ROW SORT BY seq;")
            .is_err(),
        "SORT BY beside STORE ROW was accepted"
    );
    assert!(
        session
            .run_now("DEFINE TABLE keyless (seq int) STORE COLUMN SORT BY missing;")
            .is_err(),
        "an undeclared sort key was accepted"
    );
    session
        .run_now(
            "DEFINE TABLE sorted (seq int, kind string, amount int, note string) \
             STORE COLUMN SORT BY seq;",
        )
        .unwrap();
    session
        .run_now(
            "DEFINE TABLE unsorted (seq int, kind string, amount int, note string) \
             STORE COLUMN;",
        )
        .unwrap();
    session
        .run_now("DEFINE TABLE mirror (seq int, kind string, amount int, note string) SCHEMAFULL;")
        .unwrap();
    // Shuffled on purpose, and enough rows that chunks SEAL (the boundary is
    // 8 192 rows): a projection that ignored the clause would fill its
    // chunks in this order, and no sealed chunk could hold a tight range.
    const ROWS: i64 = 18_000;
    // The record's identity is its ARRIVAL position and `seq` is shuffled
    // against it — a commit's mutations reach the projection in identity
    // order (the write set is a BTreeMap), so a shuffle of the puts alone
    // would arrive sorted anyway. The clause is what re-orders the layout.
    // Rows land by direct puts, the fixture's own loader: thirty-six
    // thousand CREATE statements would measure the CLI's scanner, not the
    // projection (Q-404).
    let ids = {
        let mut transaction = store.begin().now().unwrap();
        let catalog = Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("prod").now().unwrap().unwrap();
        let database = catalog
            .database_id(namespace, "analytics")
            .now()
            .unwrap()
            .unwrap();
        let sorted = catalog
            .table_id(namespace, database, "sorted")
            .now()
            .unwrap()
            .unwrap();
        let unsorted = catalog
            .table_id(namespace, database, "unsorted")
            .now()
            .unwrap()
            .unwrap();
        let mirror = catalog
            .table_id(namespace, database, "mirror")
            .now()
            .unwrap()
            .unwrap();
        transaction.rollback();
        (namespace, database, sorted, unsorted, mirror)
    };
    let mut transaction = store.begin().now().unwrap();
    for position in 0..ROWS {
        let row = position.wrapping_mul(7919) % ROWS;
        let payload = encode_payload(&Value::Object(BTreeMap::from([
            ("seq".to_owned(), Value::Number(Number::Integer(row))),
            (
                "kind".to_owned(),
                Value::String(format!("kind-{}", row.rem_euclid(4))),
            ),
            (
                "amount".to_owned(),
                Value::Number(Number::Integer(row.rem_euclid(97))),
            ),
            ("note".to_owned(), Value::from("alpha")),
        ])))
        .into_bytes();
        for table in [ids.2, ids.3, ids.4] {
            transaction.put(
                RecordAddress::new(
                    NamespaceId::new(ids.0.get()),
                    DatabaseId::new(ids.1.get()),
                    TableId::new(table.get()),
                    RecordId::Int(position),
                ),
                payload.clone(),
            );
        }
    }
    transaction.commit().now().unwrap();
    for read in [
        "SELECT seq FROM {t} WHERE seq > 17850 ORDER BY seq LIMIT 5;",
        "SELECT kind, sum(amount) AS f FROM {t} GROUP BY kind;",
        "SELECT count(*) AS n FROM {t} WHERE seq >= 9000 AND seq < 9100;",
        "SELECT seq, amount FROM {t} ORDER BY seq DESC LIMIT 7;",
    ] {
        let (sorted, mirror, _) = both(&mut session, "sorted", "mirror", read);
        assert_eq!(sorted, mirror, "the sorted layout diverged on `{read}`");
    }
    // The clause took effect: a bounded range read over the sorted table
    // prunes chunks the mirror's layout cannot — the head statistics hold
    // tight ranges because the rows arrived in order.
    // The clause earns its keep: the same mid-range read decodes fewer rows
    // on the sorted layout than on the unsorted one, because its chunks hold
    // tight ranges — the head statistics skip what the shuffled layout must
    // decode. (The lane's own range cannot cut a mid-range read, so the
    // difference is the per-chunk statistics, which is the clause's work.)
    let figures = |session: &mut Session<'_>, table: &str| {
        let outcomes = session
            .run_now(&format!(
                "SELECT seq FROM {table} WHERE seq >= 9000 AND seq < 9100;"
            ))
            .unwrap();
        let last = outcomes.last().unwrap();
        let rows = last.records().unwrap().len();
        let note = last
            .notes()
            .iter()
            .find_map(|note| match note {
                tessari_session::Note::Columnar {
                    chunks_examined,
                    chunks_skipped,
                    rows_decoded,
                    ..
                } => Some((*chunks_examined, *chunks_skipped, *rows_decoded)),
                _ => None,
            })
            .expect("no columnar note on a range read");
        (rows, note)
    };
    let (sorted_rows, sorted_note) = figures(&mut session, "sorted");
    let (unsorted_rows, unsorted_note) = figures(&mut session, "unsorted");
    assert_eq!(sorted_rows, unsorted_rows, "the layouts disagree");
    assert!(
        sorted_note.2 < unsorted_note.2,
        "the sorted layout decoded no fewer rows: sorted {sorted_note:?} vs \
         unsorted {unsorted_note:?}"
    );
}
