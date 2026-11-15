//! The pruning scan's storage-level tests (SG3 T3.2).
//!
//! These assert the scan's contract directly: what it skips (the statistics'
//! proof), what it returns (candidates — the row authority above filters
//! tombstoned rows), and that pruning on/off answers the same set (the
//! equivalence the OLAP skill calls the only proof a skipping structure is
//! safe). The seam tests (gate, redaction, ordering, the answer's counters)
//! live in the session suite.
#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{ColumnFamily, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_types::{FieldId, FieldKind, RecordId, Value};

use crate::catalog::{Catalog, FieldShape, StorageKind, TableShape};
use crate::now::Now;
use crate::{ColumnarCompare, ColumnarPredicate, RecordAddress, Store, columnar_scan};

use super::ROWS_PER_CHUNK;

pub(super) fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend))
        .now()
        .expect("a memory store opens");
    (backend, store)
}

/// A namespace/database/columnar table with two declared fields (`seq` int,
/// `kind` string), committed.
pub(super) fn tree(store: &Store) -> (u32, u32, u32) {
    let mut transaction = store.begin().now().expect("a transaction begins");
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().expect("namespace");
    let database = catalog
        .create_database(namespace.id, "analytics")
        .now()
        .expect("database");
    let table = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            TableShape {
                schemafull: true,
                storage_kind: Some(StorageKind::Column),
                ..TableShape::default()
            },
        )
        .now()
        .expect("table");
    catalog
        .create_field(table.id, "seq", FieldKind::Int, FieldShape::default())
        .now()
        .expect("seq field");
    catalog
        .create_field(table.id, "kind", FieldKind::String, FieldShape::default())
        .now()
        .expect("kind field");
    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    transaction.commit().now().expect("the tree commits");
    ids
}

pub(super) fn typed(
    ids: (u32, u32, u32),
) -> (
    tessari_types::NamespaceId,
    tessari_types::DatabaseId,
    tessari_types::TableId,
) {
    (
        tessari_types::NamespaceId::new(ids.0),
        tessari_types::DatabaseId::new(ids.1),
        tessari_types::TableId::new(ids.2),
    )
}

pub(super) fn address(ids: (u32, u32, u32), id: RecordId) -> RecordAddress {
    let (namespace, database, table) = typed(ids);
    RecordAddress::new(namespace, database, table, id)
}

pub(super) fn id(row: i64) -> RecordId {
    RecordId::Int(row)
}

pub(super) fn int(row: i64) -> Value {
    Value::Number(tessari_types::Number::Integer(row))
}

pub(super) fn kind(row: i64) -> Value {
    Value::String(format!("kind-{}", row % 4))
}

/// Insert `count` records in one commit: `seq` monotone, `kind` cycling
/// through four values, every seventh record null on `kind`.
pub(super) fn insert_events(store: &Store, ids: (u32, u32, u32), count: i64) {
    let mut transaction = store.begin().now().expect("a transaction begins");
    for row in 0..count {
        let mut object =
            BTreeMap::from([("seq".to_owned(), int(row)), ("kind".to_owned(), kind(row))]);
        if row % 7 == 3 {
            object.insert("kind".to_owned(), Value::Null);
        }
        transaction.put(
            address(ids, id(row)),
            encode_payload(&Value::Object(object)).into_bytes(),
        );
    }
    transaction.commit().now().expect("the insert commits");
}

/// The declared family of `name`, resolved through the catalog.
pub(super) fn family(store: &Store, ids: (u32, u32, u32), name: &str) -> ColumnFamily {
    let mut transaction = store.begin().now().expect("a transaction begins");
    let fields = Catalog::new(&mut transaction)
        .fields_on(typed(ids).0, typed(ids).1, typed(ids).2)
        .now()
        .expect("the fields read");
    transaction.commit().now().expect("the read closes");
    fields
        .into_iter()
        .find(|field| field.name == name)
        .map(|field| ColumnFamily::Id(FieldId::new(field.id.get())))
        .unwrap_or_else(|| panic!("the {name} field is declared"))
}

pub(super) fn predicate(
    family: ColumnFamily,
    compare: ColumnarCompare,
    value: Value,
) -> ColumnarPredicate {
    ColumnarPredicate {
        family,
        compare,
        value,
    }
}

/// Three sealed chunks of monotone `seq`, a selective equality, and the
/// counters proving two chunks were skipped.
#[test]
fn equality_prunes_and_answers_exactly() {
    let (_backend, store) = store();
    let ids = tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(3)).expect("rows fit an i64");
    insert_events(&store, ids, rows);
    let seq = family(&store, ids, "seq");
    let (ns, db, table) = typed(ids);

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq, ColumnarCompare::Equal, int(10_000))],
        true,
    )
    .now()
    .expect("the scan runs");

    assert_eq!(hits, vec![id(10_000)]);
    assert_eq!(report.chunks_examined, 3, "three sealed chunks");
    assert_eq!(report.chunks_skipped, 2, "the bloom denies both neighbours");
    assert_eq!(
        report.rows_decoded,
        u64::try_from(ROWS_PER_CHUNK).expect("rows fit a u64"),
        "one chunk decoded"
    );
    assert_eq!(report.rows_returned, 1);
}

/// Range predicates prune by min/max, and the boundary rows are exact.
#[test]
fn range_prunes_by_min_max() {
    let (_backend, store) = store();
    let ids = tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(3)).expect("rows fit an i64");
    insert_events(&store, ids, rows);
    let seq = family(&store, ids, "seq");
    let (ns, db, table) = typed(ids);
    let boundary = i64::try_from(ROWS_PER_CHUNK.saturating_mul(2)).expect("rows fit an i64") - 1;

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq, ColumnarCompare::Greater, int(boundary))],
        true,
    )
    .now()
    .expect("the scan runs");

    let expect: Vec<RecordId> = (boundary + 1..rows).map(id).collect();
    assert_eq!(hits, expect);
    assert_eq!(
        report.chunks_skipped, 2,
        "min/max proves the first two away"
    );
}

/// The equivalence arm: pruning on and off answer the same set.
#[test]
fn pruning_off_answers_the_same_set() {
    let (_backend, store) = store();
    let ids = tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(3)).expect("rows fit an i64");
    insert_events(&store, ids, rows);
    let seq = family(&store, ids, "seq");
    let kind_family = family(&store, ids, "kind");
    let (ns, db, table) = typed(ids);
    let predicates = [
        predicate(
            kind_family,
            ColumnarCompare::Equal,
            Value::String("kind-1".to_owned()),
        ),
        predicate(
            seq,
            ColumnarCompare::Greater,
            int(i64::try_from(ROWS_PER_CHUNK).expect("rows fit an i64")),
        ),
    ];

    let (pruned, pruned_report) = columnar_scan(&store, ns, db, table, &predicates, true)
        .now()
        .expect("the pruned scan runs");
    let (unpruned, unpruned_report) = columnar_scan(&store, ns, db, table, &predicates, false)
        .now()
        .expect("the unpruned scan runs");

    assert_eq!(pruned, unpruned, "pruning never changes the answer");
    assert!(
        pruned_report.chunks_skipped > 0,
        "the fixture is built to prune"
    );
    assert_eq!(unpruned_report.chunks_skipped, 0);
    assert!(
        unpruned_report.rows_decoded > pruned_report.rows_decoded,
        "pruning off decodes everything"
    );
}

/// An equality the column never holds: every chunk's bloom denies it, the
/// scan touches heads only, and the answer is empty.
#[test]
fn an_absent_value_is_denied_by_every_chunk() {
    let (_backend, store) = store();
    let ids = tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(2)).expect("rows fit an i64");
    insert_events(&store, ids, rows);
    let kind_family = family(&store, ids, "kind");
    let (ns, db, table) = typed(ids);

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(
            kind_family,
            ColumnarCompare::Equal,
            Value::String("kind-9".to_owned()),
        )],
        true,
    )
    .now()
    .expect("the scan runs");

    assert!(hits.is_empty());
    assert_eq!(
        report.chunks_skipped, report.chunks_examined,
        "no chunk can hold the value"
    );
    assert_eq!(report.rows_decoded, 0, "nothing was decoded");
}

/// `!=` never prunes: the counters say every chunk was decoded.
#[test]
fn not_equal_never_prunes() {
    let (_backend, store) = store();
    let ids = tree(&store);
    insert_events(&store, ids, 64);
    let seq = family(&store, ids, "seq");
    let (ns, db, table) = typed(ids);

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq, ColumnarCompare::NotEqual, int(5))],
        true,
    )
    .now()
    .expect("the scan runs");

    assert_eq!(hits.len(), 63);
    assert_eq!(report.chunks_skipped, 0);
    assert_eq!(report.rows_decoded, 64);
}

/// An update after a seal is copy-on-write: the new value answers, the old
/// value no longer does.
#[test]
fn an_updated_row_answers_at_its_new_value() {
    let (_backend, store) = store();
    let ids = tree(&store);
    insert_events(&store, ids, 64);
    let seq = family(&store, ids, "seq");
    let (ns, db, table) = typed(ids);

    let mut transaction = store.begin().now().expect("a transaction begins");
    transaction.put(
        address(ids, id(5)),
        encode_payload(&Value::Object(BTreeMap::from([
            ("seq".to_owned(), int(99_999)),
            ("kind".to_owned(), kind(5)),
        ])))
        .into_bytes(),
    );
    transaction.commit().now().expect("the update commits");

    let (hits, _) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq.clone(), ColumnarCompare::Equal, int(99_999))],
        true,
    )
    .now()
    .expect("the scan runs");
    assert_eq!(hits, vec![id(5)], "the new value points at the row");

    let (hits, _) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq, ColumnarCompare::Equal, int(5))],
        true,
    )
    .now()
    .expect("the scan runs");
    assert!(hits.is_empty(), "the sealed chunk's old row was rewritten");
}

/// Delete removes the value from the projection in the same batch: the
/// deleted record is not a candidate for its old equality, and the chunk's
/// statistics were recomputed without it (encode runs over the nulled slot).
/// The row-authority check in the session seam covers what the projection
/// cannot know — expiry — not deletes.
#[test]
fn a_deleted_row_is_not_a_candidate_for_its_old_value() {
    let (_backend, store) = store();
    let ids = tree(&store);
    insert_events(&store, ids, 64);
    let kind_family = family(&store, ids, "kind");
    let seq = family(&store, ids, "seq");
    let (ns, db, table) = typed(ids);

    let mut transaction = store.begin().now().expect("a transaction begins");
    transaction.delete(address(ids, id(9)));
    transaction.commit().now().expect("the delete commits");

    let (hits, _) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(
            kind_family,
            ColumnarCompare::Equal,
            Value::String("kind-1".to_owned()),
        )],
        true,
    )
    .now()
    .expect("the scan runs");
    assert!(
        !hits.contains(&id(9)),
        "the value left the projection with the delete"
    );

    // Its seq went with it: the chunk's min/max no longer cover 9, and the
    // sealed-chunk copy-on-write kept the surviving rows exact.
    let (hits, _) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq, ColumnarCompare::Equal, int(9))],
        true,
    )
    .now()
    .expect("the scan runs");
    assert!(hits.is_empty(), "the deleted row's seq was nulled too");
}

/// The row walk meets the columnar families that share the record prefix
/// and walks past them: every record comes back, and no chunk, map or
/// pointer key refuses the walk (the `UnknownRecordIdKind` class this
/// regression pins).
#[test]
fn the_row_walk_skips_the_columnar_families() {
    let (_backend, store) = store();
    let ids = tree(&store);
    insert_events(&store, ids, 64);
    let (ns, db, table) = typed(ids);

    let transaction = store.begin().now().expect("a transaction begins");
    let mut walk = transaction
        .walk_table(ns, db, table)
        .now()
        .expect("the walk begins");
    let mut found = Vec::new();
    while let Some(round) = walk.next_round(&transaction).now().expect("a round") {
        found.extend(round.into_iter().map(|(id, _)| id));
    }
    transaction.commit().now().expect("the read closes");
    assert_eq!(
        found,
        (0..64).map(id).collect::<Vec<_>>(),
        "the walk reads the records and skips the projection's keys"
    );
}

/// Multi-predicate scans intersect across families.
#[test]
fn families_intersect() {
    let (_backend, store) = store();
    let ids = tree(&store);
    insert_events(&store, ids, 128);
    let seq = family(&store, ids, "seq");
    let kind_family = family(&store, ids, "kind");
    let (ns, db, table) = typed(ids);

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[
            predicate(
                kind_family,
                ColumnarCompare::Equal,
                Value::String("kind-2".to_owned()),
            ),
            predicate(seq, ColumnarCompare::GreaterOrEqual, int(60)),
        ],
        true,
    )
    .now()
    .expect("the scan runs");

    // Every seventh record is null on `kind`, and a null never satisfies an
    // equality with a non-null constant.
    let expect: Vec<RecordId> = (60..128)
        .filter(|row| row % 4 == 2 && row % 7 != 3)
        .map(id)
        .collect();
    assert_eq!(hits, expect);
    assert!(report.chunks_examined >= report.chunks_skipped);
}
