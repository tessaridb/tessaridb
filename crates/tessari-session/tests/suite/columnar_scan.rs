//! The columnar scan's session seam (G082 SG3): a qualifying fallback read
//! on a column-store table answers through the projection, reports the
//! pruning figures, and answers exactly what the row walk would.
//!
//! The columnar table is defined through the storage catalog's internal
//! setter because the declaration grammar lands in SG5; everything else
//! here is statement-level, the way a caller will actually meet it.
//!
//! # What is asserted, and what deliberately is not
//!
//! The differential battery pins the *answer* against a row mirror holding
//! the same rows — the store's governing rule that an access path changes
//! what a read costs and never what it answers. The plan assertions pin the
//! *mechanism* only where the gate is the feature under test (a columnar
//! plan on a qualifying read, a scan plan on each refusal arm); the
//! hidden-field case pins the property — the narrow caller gets no records —
//! not the mechanism, for the reason `search_field_grants` documents.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Note, Now, Outcome, RunNow, Session};
use tessari_storage::{Catalog, FieldShape, RecordAddress, StorageKind, Store, TableShape};
use tessari_types::{DatabaseId, FieldKind, NamespaceId, TableId, Value};

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

struct Fixture {
    columnar: (u32, u32, u32),
    mirror: (u32, u32, u32),
}

/// A namespace/database with a columnar `events` table and a row `mirror`
/// table holding the same rows, so every read has its differential twin.
/// Rows: `seq` monotone, `kind` cycling four values, every seventh record
/// null on `kind`.
fn ready(store: &Store, rows: i64) -> (Session<'_>, Fixture) {
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
    let fixture = Fixture {
        columnar: (namespace.id.get(), database.id.get(), events.id.get()),
        mirror: (namespace.id.get(), database.id.get(), mirror.id.get()),
    };
    // The catalog commits before the rows: the projection reads the
    // committed definition when the commit's maintain runs (the same-batch
    // rule the index path runs under), so a definition and its first rows
    // in one commit would project nothing.
    transaction.commit().now().unwrap();
    let mut transaction = store.begin().now().unwrap();
    for row in 0..rows {
        let value = |seq: i64, kind: Value| {
            encode_payload(&Value::Object(BTreeMap::from([
                (
                    "seq".to_owned(),
                    Value::Number(tessari_types::Number::Integer(seq)),
                ),
                ("kind".to_owned(), kind),
            ])))
            .into_bytes()
        };
        let kind = if row % 7 == 3 {
            Value::Null
        } else {
            Value::String(format!("kind-{}", row.rem_euclid(4)))
        };
        for (namespace, database, table) in [fixture.columnar, fixture.mirror] {
            transaction.put(
                RecordAddress::new(
                    NamespaceId::new(namespace),
                    DatabaseId::new(database),
                    TableId::new(table),
                    tessari_types::RecordId::Int(row),
                ),
                value(row, kind.clone()),
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

/// The records a read answered with, by identity.
fn ids(session: &mut Session<'_>, script: &str) -> Vec<tessari_types::RecordId> {
    let outcomes = session.run_now(script).unwrap();
    let mut found: Vec<tessari_types::RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

/// The access path a read's answer carried.
fn access(session: &mut Session<'_>, script: &str) -> AccessPath {
    let outcomes = session.run_now(script).unwrap();
    outcomes
        .last()
        .unwrap()
        .plan()
        .unwrap_or_else(|| panic!("a plan answered with {:?}", outcomes.last()))
        .access
}

/// The columnar pruning note a read's answer carried.
fn columnar_note(session: &mut Session<'_>, script: &str) -> Note {
    let outcomes = session.run_now(script).unwrap();
    outcomes
        .last()
        .unwrap()
        .notes()
        .iter()
        .find(|note| matches!(note, Note::Columnar { .. }))
        .unwrap_or_else(|| panic!("a columnar note answered with {:?}", outcomes.last()))
        .clone()
}

/// A qualifying selective read answers through the columnar path and
/// reports the pruning figures.
#[test]
fn a_selective_equality_answers_through_the_columnar_path() {
    let store = store();
    let (mut session, _) = ready(&store, 16_400);

    assert_eq!(
        access(&mut session, "SELECT * FROM events WHERE seq = 12345;"),
        AccessPath::Columnar,
    );
    let ids = ids(&mut session, "SELECT * FROM events WHERE seq = 12345;");
    assert_eq!(ids, vec![tessari_types::RecordId::Int(12345)]);

    let note = columnar_note(&mut session, "SELECT * FROM events WHERE seq = 12345;");
    let Note::Columnar {
        chunks_examined,
        chunks_skipped,
        rows_decoded,
        rows_returned,
    } = note
    else {
        panic!("the note is columnar: {note:?}");
    };
    assert_eq!(chunks_examined, 3, "two sealed chunks and the open one");
    assert_eq!(
        chunks_skipped, 2,
        "the bloom denies the two chunks that cannot hold the value"
    );
    // 12_345 sits in the second sealed chunk (8_192..16_383): the first is
    // bloom-denied, the second decodes whole, the open chunk's min/max
    // proves it away.
    assert_eq!(rows_decoded, 8192, "only the chunk holding 12345 decoded");
    assert_eq!(rows_returned, 1);
}

/// `EXPLAIN` names the columnar path, and every gate refusal arm names the
/// scan — one gate decides for both callers.
#[test]
fn explain_reports_the_gate_either_way() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 200);
    let plan = |session: &mut Session<'_>, script: &str| -> String {
        let outcomes = session.run_now(script).unwrap();
        let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
            panic!("a plan answered with {:?}", outcomes.last());
        };
        format!("{:?}", fields.get("access").unwrap_or(&Value::None))
    };

    assert_eq!(
        plan(&mut session, "EXPLAIN SELECT * FROM events WHERE seq = 5;"),
        "String(\"columnar\")",
    );
    // An `!=` conjunct disqualifies the whole chain: a null or absent
    // field satisfies it and the projection holds no row to name such a
    // record.
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM events WHERE seq > 5 AND kind != 'kind-1';",
        ),
        "String(\"scan\")",
    );
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM events WHERE seq > 5 OR seq < 2;",
        ),
        "String(\"scan\")",
    );
    // An arithmetic side disqualifies the conjunct.
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM events WHERE seq + 1 > 5;",
        ),
        "String(\"scan\")",
    );
    // `!=` routes to the row walk — the projection holds no row for the
    // null or absent fields that satisfy it.
    assert_eq!(
        plan(&mut session, "EXPLAIN SELECT * FROM events WHERE seq != 5;"),
        "String(\"scan\")",
    );
    // A bound parameter substitutes to a literal before the planner asks,
    // and qualifies like one. (An unbound parameter is refused earlier.)
    let outcomes = session
        .run_with_now(
            "EXPLAIN SELECT * FROM events WHERE seq > $bound;",
            &tessari_ql::Parameters::from([(
                "bound".to_owned(),
                Value::Number(tessari_types::Number::Integer(5)),
            )]),
        )
        .unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    assert_eq!(
        format!("{:?}", fields.get("access").unwrap_or(&Value::None)),
        "String(\"columnar\")",
    );
    // The row mirror never qualifies.
    assert_eq!(
        plan(&mut session, "EXPLAIN SELECT * FROM mirror WHERE seq = 5;"),
        "String(\"scan\")",
    );
}

/// The differential battery: every comparison the gate accepts, mirrored
/// both ways, answers identically on the columnar table and the row table —
/// and the qualifying reads really took the columnar path.
#[test]
fn the_columnar_path_answers_what_the_row_walk_would() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 200);
    // `!=` is deliberately absent: it routes to the row walk (a null or
    // absent field satisfies it, and the projection holds no row to name
    // such a record) — the refusal arm is asserted in the EXPLAIN test.
    let predicates = [
        "seq = 5",
        "seq < 50",
        "seq <= 50",
        "seq > 150",
        "seq >= 150",
        "5 < seq",
        "kind = 'kind-1'",
        "seq >= 30 AND kind = 'kind-2'",
        "kind = 'kind-3' AND seq < 100",
    ];
    for predicate in predicates {
        let from_columnar = ids(
            &mut session,
            &format!("SELECT * FROM events WHERE {predicate};"),
        );
        let from_mirror = ids(
            &mut session,
            &format!("SELECT * FROM mirror WHERE {predicate};"),
        );
        assert_eq!(
            from_columnar, from_mirror,
            "the paths disagreed on `{predicate}`"
        );
    }
    // The controls: the battery's reads took the columnar path.
    assert_eq!(
        access(&mut session, "SELECT * FROM events WHERE kind = 'kind-1';"),
        AccessPath::Columnar,
    );
    assert_eq!(
        access(
            &mut session,
            "SELECT * FROM events WHERE seq >= 30 AND kind = 'kind-2';"
        ),
        AccessPath::Columnar,
    );
}

/// The walk's ordering contract holds on the columnar path: matches come
/// back in record-key order, exactly as the mirror orders them.
#[test]
fn matches_arrive_in_the_walks_key_order() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 200);
    let ordered = |session: &mut Session<'_>, table: &str| {
        let outcomes = session
            .run_now(&format!(
                "SELECT * FROM {table} WHERE kind = 'kind-0' AND seq >= 100;"
            ))
            .unwrap();
        outcomes
            .last()
            .unwrap()
            .records()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ordered(&mut session, "events"),
        ordered(&mut session, "mirror"),
    );
}

/// Writes through the columnar path stay exact: an update moves the row
/// the scan finds, and a delete removes it from both the old and the new
/// value's equality.
#[test]
fn updates_and_deletes_answer_exactly_through_the_path() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 200);
    session
        .run_now("UPDATE events:5 = { seq: 9999, kind: 'kind-1' };")
        .unwrap();
    assert_eq!(
        ids(&mut session, "SELECT * FROM events WHERE seq = 9999;"),
        vec![tessari_types::RecordId::Int(5)],
    );
    assert!(
        ids(&mut session, "SELECT * FROM events WHERE seq = 5;").is_empty(),
        "the old value no longer points at the row"
    );
    session.run_now("DELETE events:5;").unwrap();
    assert!(
        ids(&mut session, "SELECT * FROM events WHERE seq = 9999;").is_empty(),
        "the delete reached the projection in the same commit"
    );
}

/// A transaction reading its own write on the table is refused the
/// projection (it describes committed state) and answered by the row walk
/// instead — read-your-writes intact.
#[test]
fn a_transaction_holding_writes_is_answered_by_the_walk() {
    let store = store();
    let (mut session, _fixture) = ready(&store, 200);
    session
        .run_now(
            "BEGIN;\n\
             CREATE events:99999 = { seq: 99999, kind: 'kind-0' };\n\
             EXPLAIN SELECT * FROM events WHERE seq = 99999;\n\
             COMMIT;",
        )
        .unwrap();
    let outcomes = session
        .run_now(
            "BEGIN;\n\
             CREATE events:99998 = { seq: 99998, kind: 'kind-0' };\n\
             SELECT * FROM events WHERE seq = 99998;\n\
             COMMIT;",
        )
        .unwrap();
    let answer = outcomes
        .iter()
        .find(|outcome| outcome.records().is_some())
        .unwrap_or_else(|| panic!("the read answered with {outcomes:?}"));
    assert_eq!(
        answer
            .plan()
            .unwrap_or_else(|| panic!("a plan answered with {answer:?}"))
            .access,
        AccessPath::Scan,
        "the gate refuses a transaction holding writes"
    );
    assert_eq!(
        answer
            .records()
            .unwrap_or_else(|| panic!("records answered with {answer:?}"))
            .len(),
        1,
        "the row walk merged the transaction's own write"
    );
}

/// A grant-hidden field is never pruned on. The property: the narrow
/// caller gets no records for a predicate on the hidden field; the wide
/// caller, the control, gets them through the columnar path.
#[test]
fn a_grant_hidden_field_is_never_pruned_on() {
    let store = store();
    let (mut bootstrap, _fixture) = ready(&store, 200);
    bootstrap
        .run_now("DEFINE USER owner ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("owner", PASSWORD).now().unwrap();
    root.run_now("USE NAMESPACE prod; USE DATABASE analytics;")
        .unwrap();
    root.run_now(
        "DEFINE USER wide ON prod.analytics ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER narrow ON prod.analytics ROLE editor PASSWORD 'correct horse battery';\n\
         GRANT read ON events FIELDS seq, kind TO wide;\n\
         GRANT read ON events FIELDS seq TO narrow;",
    )
    .unwrap();

    let mut narrow = Session::new(&store);
    narrow.sign_in("narrow", PASSWORD).now().unwrap();
    narrow
        .run_now("USE NAMESPACE prod; USE DATABASE analytics;")
        .unwrap();
    assert!(
        ids(&mut narrow, "SELECT * FROM events WHERE kind = 'kind-1';").is_empty(),
        "the hidden field answered nobody"
    );

    let mut wide = Session::new(&store);
    wide.sign_in("wide", PASSWORD).now().unwrap();
    wide.run_now("USE NAMESPACE prod; USE DATABASE analytics;")
        .unwrap();
    assert_eq!(
        access(&mut wide, "SELECT * FROM events WHERE kind = 'kind-1';"),
        AccessPath::Columnar,
        "the wide caller is the control: the columnar path is in play"
    );
    assert!(
        !ids(&mut wide, "SELECT * FROM events WHERE kind = 'kind-1';").is_empty(),
        "the wide caller reads through"
    );
}
