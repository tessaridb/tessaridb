//! The pruning scan's storage-level tests (SG3 T3.2).
//!
//! These assert the scan's contract directly: what it skips (the statistics'
//! proof), what it returns (candidates — the row authority above filters
//! tombstoned rows), and that pruning on/off answers the same set (the
//! equivalence the OLAP skill calls the only proof a skipping structure is
//! safe). The seam tests (gate, redaction, ordering, the answer's counters)
//! live in the session suite.
#![allow(clippy::panic, clippy::unwrap_used, clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{ColumnFamily, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_types::{FieldId, FieldKind, RecordId, Value};

use crate::catalog::{Catalog, FieldShape, StorageKind, TableShape};
use crate::now::Now;
use crate::{
    ColumnarCompare, ColumnarPredicate, ColumnarTest, RecordAddress, Store, columnar_scan,
};

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
        test: ColumnarTest::Compare { compare, value },
    }
}

/// An IN-list predicate over bound constants (`field IN (…)`).
pub(super) fn predicate_in(family: ColumnFamily, values: Vec<Value>) -> ColumnarPredicate {
    ColumnarPredicate {
        family,
        test: ColumnarTest::In { values },
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
    // The lane statistics denied the two sealed chunks without reading a
    // head; the open chunk — never covered by them — held the answer.
    assert_eq!(
        report.chunks_examined, 1,
        "only the open chunk was examined"
    );
    assert_eq!(report.chunks_skipped, 0);
    assert_eq!(
        report.rows_decoded,
        u64::try_from(ROWS_PER_CHUNK).expect("rows fit a u64"),
        "the open chunk decoded"
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

/// SG4 T4.2a: hash partitioning — rows route by the murmur2 hash of the
/// partition field's value; each partition's chunks hold only its rows; the
/// scan pins to one partition.
#[cfg(test)]
mod partitioned {
    use super::*;
    use crate::catalog::HashPartitioning;

    /// A columnar table partitioned four ways by `seq`, `count` rows inserted.
    fn partitioned(store: &Store, count: i64) -> (u32, u32, u32) {
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
                    partitions: Some(HashPartitioning {
                        count: 4,
                        by: "seq".to_owned(),
                    }),
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
        insert_events(store, ids, count);
        ids
    }

    /// The partition a row routes to, re-derived independently of the
    /// projection: the hash the engine shares, over the payload encoding of
    /// the partition field's value.
    fn routed(value: i64, count: u32) -> u64 {
        let payload = tessari_encoding::encode_payload(&int(value)).into_bytes();
        let hashed = u64::from(tessari_encoding::murmur2(&payload) & 0x7fff_ffff);
        hashed
            .checked_rem(u64::from(count))
            .expect("the count is at least one")
    }

    /// The pinned scan: one partition's lane only.
    fn pinned_scan(
        store: &Store,
        ids: (u32, u32, u32),
        predicates: &[ColumnarPredicate],
        partition: u64,
    ) -> (Vec<RecordId>, crate::ColumnarScanReport) {
        let (ns, db, table) = typed(ids);
        crate::columnar_scan_partitioned(store, ns, db, table, predicates, true, Some(partition))
            .now()
            .expect("the scan runs")
    }

    #[test]
    fn rows_route_by_the_hash_of_the_partition_field() {
        let (_backend, store) = store();
        let ids = partitioned(&store, 500);
        let seq = family(&store, ids, "seq");

        // Every row answers from the partition its key routes to, and only
        // from there: a scan pinned to the wrong partition is empty, pinned
        // to the right one answers exactly the row.
        for row in [0_i64, 1, 42, 499] {
            let partition = routed(row, 4);
            let (hits, _) = pinned_scan(
                &store,
                ids,
                &[predicate(seq.clone(), ColumnarCompare::Equal, int(row))],
                partition,
            );
            assert_eq!(hits, vec![id(row)], "row {row} at partition {partition}");
            let (hits, _) = pinned_scan(
                &store,
                ids,
                &[predicate(seq.clone(), ColumnarCompare::Equal, int(row))],
                partition.saturating_add(1).rem_euclid(4),
            );
            assert!(hits.is_empty(), "no row in a partition it did not route to");
        }
    }

    #[test]
    fn a_partition_pin_scans_one_lane() {
        let (_backend, store) = store();
        let ids = partitioned(&store, 500);
        let seq = family(&store, ids, "seq");

        // The pinned scan reports only its lane's chunks; the unpinned scan
        // reports every lane.
        let expected: Vec<u64> = (0..500).map(|row| routed(row, 4)).collect();
        let lane = expected.iter().filter(|p| **p == 1).count();
        let (hits, pinned) = pinned_scan(
            &store,
            ids,
            &[predicate(
                seq.clone(),
                ColumnarCompare::GreaterOrEqual,
                int(0),
            )],
            1,
        );
        assert_eq!(hits.len(), lane, "the lane holds exactly its rows");
        assert!(
            pinned.chunks_examined <= 128,
            "one lane examined: {}",
            pinned.chunks_examined
        );
    }

    #[test]
    fn an_update_crossing_partitions_cows_and_reappends() {
        let (_backend, store) = store();
        let ids = partitioned(&store, 500);
        let seq = family(&store, ids, "seq");

        // A row whose new key routes elsewhere leaves its old lane (copy-on
        // write) and lands in the new one.
        let from = routed(7, 4);
        // A value no existing row holds, routing to a different lane.
        let new_value = (500..1000)
            .find(|row| routed(*row, 4) != from)
            .expect("a target lane");
        let mut transaction = store.begin().now().expect("a transaction begins");
        transaction.put(
            address(ids, id(7)),
            encode_payload(&Value::Object(BTreeMap::from([
                ("seq".to_owned(), int(new_value)),
                ("kind".to_owned(), kind(7)),
            ])))
            .into_bytes(),
        );
        transaction.commit().now().expect("the update commits");

        let (hits, _) = pinned_scan(
            &store,
            ids,
            &[predicate(
                seq.clone(),
                ColumnarCompare::Equal,
                int(new_value),
            )],
            routed(new_value, 4),
        );
        assert_eq!(hits, vec![id(7)], "the row answers from its new lane");
        let (hits, _) = pinned_scan(
            &store,
            ids,
            &[predicate(seq, ColumnarCompare::Equal, int(7))],
            from,
        );
        assert!(hits.is_empty(), "the old lane no longer holds the value");
    }
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

// ---- W1 (G099 C1): lane statistics, IN-list probes, sorted-chunk search ----

/// A namespace/database/columnar table with an int `seq` and a float
/// `measure`, committed — the NaN control's tree.
fn float_tree(store: &Store) -> (u32, u32, u32) {
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
            "readings",
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
        .create_field(table.id, "measure", FieldKind::Float, FieldShape::default())
        .now()
        .expect("measure field");
    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    transaction.commit().now().expect("the tree commits");
    ids
}

fn float(value: f64) -> Value {
    Value::Number(tessari_types::Number::float(value))
}

/// Insert `count` readings in one commit: `seq` and `measure` monotone,
/// except every `nan_at`-th row's measure, which is a NaN (disabling that
/// chunk's range statistics).
fn insert_readings(store: &Store, ids: (u32, u32, u32), count: i64, nan_at: i64) {
    let mut transaction = store.begin().now().expect("a transaction begins");
    for row in 0..count {
        let measure = if nan_at > 0 && row.checked_rem(nan_at) == Some(0) {
            float(f64::NAN)
        } else {
            // The fixture's rows are small; the conversion is exact there.
            // `From<i64> for f64` does not exist, so the test allow-lists the
            // precision lint rather than every call site inventing a parser.
            float(row as f64)
        };
        let object = BTreeMap::from([
            ("seq".to_owned(), int(row)),
            ("measure".to_owned(), measure),
        ]);
        transaction.put(
            address(ids, id(row)),
            encode_payload(&Value::Object(object)).into_bytes(),
        );
    }
    transaction.commit().now().expect("the insert commits");
}

/// An IN-list answers the union in one pass: each chunk is pruned against
/// every value, the survivors decode once, and the hits are exact.
#[test]
fn in_list_prunes_and_answers_exactly() {
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
        &[predicate_in(seq, vec![int(10_000), int(14_000)])],
        true,
    )
    .now()
    .expect("the scan runs");

    assert_eq!(hits, vec![id(10_000), id(14_000)]);
    assert_eq!(report.chunks_examined, 3, "three sealed chunks");
    assert_eq!(
        report.chunks_skipped, 2,
        "both values sit in the middle chunk; the neighbours deny both"
    );
    assert_eq!(
        report.rows_decoded,
        u64::try_from(ROWS_PER_CHUNK).expect("rows fit a u64"),
        "one chunk decoded"
    );
    assert_eq!(report.rows_returned, 2);
}

/// The IN-list equivalence arm: pruning on and off answer the same set,
/// alone and intersected with another family.
#[test]
fn in_list_pruning_matches_unpruned() {
    let (_backend, store) = store();
    let ids = tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(2)).expect("rows fit an i64");
    insert_events(&store, ids, rows);
    let seq = family(&store, ids, "seq");
    let kind_family = family(&store, ids, "kind");
    let (ns, db, table) = typed(ids);

    for predicates in [
        // All three values sit in the first chunk: the second denies every
        // one of them.
        vec![predicate_in(seq.clone(), vec![int(5), int(6), int(7)])],
        vec![
            predicate_in(seq.clone(), vec![int(5), int(6)]),
            predicate(
                kind_family,
                ColumnarCompare::Equal,
                Value::String("kind-1".to_owned()),
            ),
        ],
    ] {
        let (pruned, pruned_report) = columnar_scan(&store, ns, db, table, &predicates, true)
            .now()
            .expect("the pruned scan runs");
        let (unpruned, unpruned_report) = columnar_scan(&store, ns, db, table, &predicates, false)
            .now()
            .expect("the unpruned scan runs");
        assert_eq!(pruned, unpruned, "pruning never changes the answer");
        assert!(
            pruned_report.chunks_skipped > 0,
            "the fixture is built to prune: {predicates:?}"
        );
        assert_eq!(unpruned_report.chunks_skipped, 0);
    }
}

/// A predicate outside the lane's sealed range is denied at the lane head:
/// the pointer's lane statistics are consulted first and no chunk head is
/// read at all. Only the open chunk is examined.
#[test]
fn out_of_range_is_denied_at_the_lane() {
    let (_backend, store) = store();
    let ids = tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(3)).expect("rows fit an i64") + 5;
    insert_events(&store, ids, rows);
    let seq = family(&store, ids, "seq");
    let (ns, db, table) = typed(ids);

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(seq, ColumnarCompare::Equal, int(1_000_000))],
        true,
    )
    .now()
    .expect("the scan runs");

    assert!(hits.is_empty());
    assert_eq!(
        report.chunks_examined, 1,
        "the lane statistics denied the sealed chunks unread; the open chunk alone is examined"
    );
    assert_eq!(
        report.chunks_skipped, 1,
        "the open chunk's own head denies the value"
    );
    assert_eq!(report.rows_decoded, 0, "nothing was decoded");
}

/// Control arm: a lane whose sealed chunks lack range statistics (a NaN
/// disabled one) carries no lane statistics of its own — absence disables
/// pruning, it never guesses, so every chunk head is read as before.
#[test]
fn lane_without_range_statistics_reads_every_chunk() {
    let (_backend, store) = store();
    let ids = float_tree(&store);
    let rows = i64::try_from(ROWS_PER_CHUNK.saturating_mul(2)).expect("rows fit an i64");
    insert_readings(&store, ids, rows, 7);
    let measure = family(&store, ids, "measure");
    let (ns, db, table) = typed(ids);

    let (hits, report) = columnar_scan(
        &store,
        ns,
        db,
        table,
        &[predicate(measure, ColumnarCompare::Equal, float(999_999.0))],
        true,
    )
    .now()
    .expect("the scan runs");

    assert!(hits.is_empty());
    assert_eq!(
        report.chunks_examined, 2,
        "no lane statistics: the NaN chunk and the open chunk are both examined"
    );
    assert_eq!(
        report.chunks_skipped, report.chunks_examined,
        "each chunk's own head denies the value"
    );
}
