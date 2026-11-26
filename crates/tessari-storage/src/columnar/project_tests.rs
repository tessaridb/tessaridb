//! End-to-end projection tests: a declared columnar table round-trips
//! writes and reads with exact answers (SG2 T2.2/T2.3).
//!
//! These run inside the crate because the read surface is internal until the
//! declaration grammar lands (SG5); the assertions compare what the chunks
//! hold against what the row path answers — the differential mirror at unit
//! scale (the design record). The store is used single-threaded through the
//! `.now()` harness; the projection itself runs inside the commit's write
//! gate, so sequential tests exercise the same path a racing one would.
#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{ColumnFamily, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_types::{FieldId, FieldKind, RecordId, Value};

use crate::catalog::{Catalog, FieldShape, StorageKind, TableKind, TableShape};
use crate::now::Now;
use crate::{RecordAddress, Store};

use super::ROWS_PER_CHUNK;
use super::read::{family_chunks, record_values};

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend))
        .now()
        .expect("a memory store opens");
    (backend, store)
}

/// A namespace/database/table tree with two declared fields, committed.
/// `schemafull` and the storage kind parameterise the table.
fn tree(store: &Store, columnar: bool, schemafull: bool) -> (u32, u32, u32) {
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
                schemafull,
                storage_kind: columnar.then_some(StorageKind::Column),
                ..TableShape::default()
            },
        )
        .now()
        .expect("table");
    if schemafull {
        catalog
            .create_field(table.id, "seq", FieldKind::Int, FieldShape::default())
            .now()
            .expect("seq field");
        catalog
            .create_field(table.id, "kind", FieldKind::String, FieldShape::default())
            .now()
            .expect("kind field");
    }
    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    transaction.commit().now().expect("the tree commits");
    ids
}

fn address(ids: (u32, u32, u32), id: RecordId) -> RecordAddress {
    RecordAddress::new(
        tessari_types::NamespaceId::new(ids.0),
        tessari_types::DatabaseId::new(ids.1),
        tessari_types::TableId::new(ids.2),
        id,
    )
}

fn typed(
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

fn id(row: i64) -> RecordId {
    RecordId::Int(row)
}

fn int(row: i64) -> Value {
    Value::Number(tessari_types::Number::Integer(row))
}

/// Insert `count` records in one commit: `seq` monotone, `kind` cycling
/// through four values, every seventh record null on `kind`.
fn insert_events(store: &Store, ids: (u32, u32, u32), count: i64) {
    let mut transaction = store.begin().now().expect("a transaction begins");
    for row in 0..count {
        let mut object = BTreeMap::from([
            ("seq".to_owned(), int(row)),
            (
                "kind".to_owned(),
                Value::String(format!("kind-{}", row % 4)),
            ),
        ]);
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

/// Read the row path's answer for one record's field.
fn row_value(store: &Store, at: &RecordAddress, field: &str) -> Value {
    let transaction = store.begin().now().expect("a transaction begins");
    let payload = transaction
        .get(at)
        .now()
        .expect("the read runs")
        .expect("the record");
    let object = match tessari_encoding::decode_payload(&payload).expect("the payload decodes") {
        Value::Object(object) => object,
        other => panic!("expected an object, found {other:?}"),
    };
    transaction.commit().now().expect("the read closes");
    object.get(field).cloned().unwrap_or(Value::Null)
}

/// The two declared families, by the field ids the tree's create_field calls
/// drew: ids 19 and 20 in a fresh store (18 catalog tables and system rows
/// precede them — the test asserts what it reads rather than assuming, so a
/// drift in system ids fails loudly here).
fn declared_families(store: &Store, ids: (u32, u32, u32)) -> Vec<(String, ColumnFamily)> {
    let mut transaction = store.begin().now().expect("a transaction begins");
    let fields = Catalog::new(&mut transaction)
        .fields_on(
            tessari_types::NamespaceId::new(ids.0),
            tessari_types::DatabaseId::new(ids.1),
            tessari_types::TableId::new(ids.2),
        )
        .now()
        .expect("the fields read");
    transaction.commit().now().expect("the read closes");
    fields
        .into_iter()
        .map(|field| (field.name.clone(), ColumnFamily::Id(field.id)))
        .collect()
}

#[test]
fn the_projection_roundtrips_exactly_against_the_row() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    insert_events(&store, ids, 500);
    let (ns, db, table) = typed(ids);

    for (name, family) in declared_families(&store, ids) {
        let chunks = family_chunks(&store, ns, db, table, &family)
            .now()
            .expect("the family reads");
        assert!(!chunks.is_empty(), "{name} has chunks");
        for (seq, chunk) in &chunks {
            for (row, record_id) in chunk.record_ids.iter().enumerate() {
                let at = address(ids, record_id.clone());
                assert_eq!(
                    chunk.values[row],
                    row_value(&store, &at, &name),
                    "{name} chunk {seq} row {row} must equal the row path"
                );
            }
        }
    }
}

#[test]
fn record_values_answer_through_the_map() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    insert_events(&store, ids, 50);
    let (ns, db, table) = typed(ids);
    let families = declared_families(&store, ids);
    let seq_family = families
        .iter()
        .find(|(name, _)| name == "seq")
        .map(|(_, family)| family.clone())
        .expect("the seq field is declared");

    let values = record_values(&store, ns, db, table, &id(12))
        .now()
        .expect("the map reads");
    assert_eq!(values.get(&seq_family), Some(&int(12)));
    // A record nobody wrote holds nothing.
    let absent = record_values(&store, ns, db, table, &id(999))
        .now()
        .expect("the map reads");
    assert!(absent.is_empty());
}

#[test]
fn updates_land_in_the_same_batch_and_replace_exactly() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    insert_events(&store, ids, 100);

    // Rewrite a third of the records, sealing some chunks in passing.
    let mut transaction = store.begin().now().expect("a transaction begins");
    for row in (0..100).step_by(3) {
        let object = BTreeMap::from([
            ("seq".to_owned(), int(row)),
            ("kind".to_owned(), Value::String("rewritten".to_owned())),
        ]);
        transaction.put(
            address(ids, id(row)),
            encode_payload(&Value::Object(object)).into_bytes(),
        );
    }
    transaction.commit().now().expect("the update commits");
    let (ns, db, table) = typed(ids);

    // Every chunk of every family still answers what the row answers — the
    // copy-on-write moved rows without losing any.
    for (name, family) in declared_families(&store, ids) {
        let chunks = family_chunks(&store, ns, db, table, &family)
            .now()
            .expect("the family reads");
        for (seq, chunk) in &chunks {
            for (row, record_id) in chunk.record_ids.iter().enumerate() {
                let at = address(ids, record_id.clone());
                assert_eq!(
                    chunk.values[row],
                    row_value(&store, &at, &name),
                    "{name} chunk {seq} row {row} agrees with the row after updates"
                );
            }
        }
    }
}

#[test]
fn deletes_leave_the_projection_for_the_tombstone_to_filter() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    insert_events(&store, ids, 40);

    let mut transaction = store.begin().now().expect("a transaction begins");
    for row in 0..20 {
        transaction.delete(address(ids, id(row)));
    }
    transaction.commit().now().expect("the delete commits");
    let (ns, db, table) = typed(ids);
    let families = declared_families(&store, ids);
    let seq_family = families
        .iter()
        .find(|(name, _)| name == "seq")
        .map(|(_, family)| family.clone())
        .expect("the seq field is declared");

    // the design record: no chunk rewrite on delete. The tombstone is the authority;
    // the projection still holds the values, and the scan (SG3) filters.
    let chunks = family_chunks(&store, ns, db, table, &seq_family)
        .now()
        .expect("the family reads");
    let rows: usize = chunks.iter().map(|(_, chunk)| chunk.record_ids.len()).sum();
    assert_eq!(rows, 40, "delete writes no chunk rewrite");

    // And the row path agrees the records are gone.
    let at = address(ids, id(7));
    let transaction = store.begin().now().expect("a transaction begins");
    assert!(transaction.get(&at).now().expect("the read runs").is_none());
    transaction.commit().now().expect("the read closes");
}

#[test]
fn cancel_lands_no_chunks() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    let (ns, db, table) = typed(ids);

    let mut transaction = store.begin().now().expect("a transaction begins");
    transaction.put(
        address(ids, id(1)),
        encode_payload(&Value::Object(BTreeMap::from([("seq".to_owned(), int(1))]))).into_bytes(),
    );
    // Not committing is the cancel: the buffered writes die with the drop.
    drop(transaction);

    let families = declared_families(&store, ids);
    let seq_family = families
        .iter()
        .find(|(name, _)| name == "seq")
        .map(|(_, family)| family.clone())
        .expect("the seq field is declared");
    assert!(
        family_chunks(&store, ns, db, table, &seq_family)
            .now()
            .expect("the family reads")
            .is_empty()
    );
}

#[test]
fn a_schemaless_table_projects_the_fields_it_has_seen() {
    let (_backend, store) = store();
    let ids = tree(&store, true, false);
    let (ns, db, table) = typed(ids);

    let mut transaction = store.begin().now().expect("a transaction begins");
    for row in 0..30 {
        let mut object = BTreeMap::from([("temperature".to_owned(), int(20 + row % 5))]);
        if row % 2 == 0 {
            object.insert(
                "humidity".to_owned(),
                Value::Number(tessari_types::Number::float(0.5)),
            );
        }
        transaction.put(
            address(ids, id(row)),
            encode_payload(&Value::Object(object)).into_bytes(),
        );
    }
    transaction.commit().now().expect("the insert commits");

    // Name-keyed families, one per seen field.
    let temperature = ColumnFamily::Name("temperature".to_owned());
    let chunks = family_chunks(&store, ns, db, table, &temperature)
        .now()
        .expect("the family reads");
    assert!(!chunks.is_empty());
    let rows: usize = chunks.iter().map(|(_, chunk)| chunk.record_ids.len()).sum();
    assert_eq!(rows, 30);

    let values = record_values(&store, ns, db, table, &id(4))
        .now()
        .expect("the map reads");
    assert!(values.contains_key(&ColumnFamily::Name("humidity".to_owned())));
}

#[test]
fn chunks_seal_at_the_row_boundary() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    let over = i64::try_from(ROWS_PER_CHUNK).expect("rows fit i64") + 10;
    insert_events(&store, ids, over);
    let (ns, db, table) = typed(ids);
    let families = declared_families(&store, ids);
    let seq_family = families
        .iter()
        .find(|(name, _)| name == "seq")
        .map(|(_, family)| family.clone())
        .expect("the seq field is declared");

    let chunks = family_chunks(&store, ns, db, table, &seq_family)
        .now()
        .expect("the family reads");
    assert_eq!(chunks.len(), 2, "one seal past the row boundary");
    assert_eq!(chunks[0].1.record_ids.len(), ROWS_PER_CHUNK);
    assert_eq!(chunks[1].1.record_ids.len(), 10);
}

#[test]
fn a_row_table_projects_nothing() {
    let (_backend, store) = store();
    let ids = tree(&store, false, true);
    insert_events(&store, ids, 10);
    let (ns, db, table) = typed(ids);

    // No pointer, no chunks — the projection only wakes for a columnar table.
    let family = ColumnFamily::Id(FieldId::new(19));
    assert!(
        family_chunks(&store, ns, db, table, &family)
            .now()
            .expect("the family reads")
            .is_empty()
    );
}

#[test]
fn the_kind_guard_refuses_a_storage_kind_on_a_kind_that_is_not_records() {
    let (_backend, store) = store();
    let mut transaction = store.begin().now().expect("a transaction begins");
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().expect("namespace");
    let database = catalog
        .create_database(namespace.id, "analytics")
        .now()
        .expect("database");
    // The default kind is a record table, so this succeeds…
    let accepted = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            TableShape {
                storage_kind: Some(StorageKind::Column),
                ..TableShape::default()
            },
        )
        .now();
    assert!(accepted.is_ok());
    // …and a queue — not a record table — refuses, named.
    let refused = catalog
        .create_table(
            namespace.id,
            database.id,
            "jobs",
            TableShape {
                storage_kind: Some(StorageKind::Column),
                kind: TableKind::Queue(crate::catalog::QueueDeclaration {
                    timeout: tessari_types::Duration::new(30, 0).expect("a duration"),
                    attempts: None,
                    priority: None,
                    not_before: None,
                    deduplicate: None,
                    dead_letter: None,
                }),
                ..TableShape::default()
            },
        )
        .now();
    assert!(matches!(
        refused,
        Err(crate::Error::StorageKindOnKind { .. })
    ));
    // Not committing is the cancel: the buffered writes die with the drop.
    drop(transaction);
}

/// C2's measurement (the design record): the encoded column chunks against the
/// row payloads for the same records, per family and overall. The assertion
/// is a real inequality — the dictionary-heavy fixture must come out smaller
/// — and the numbers print for the plan's evidence table.
#[test]
fn c2_the_column_layout_earns_its_bytes() {
    let (_backend, store) = store();
    let ids = tree(&store, true, true);
    insert_events(&store, ids, 100_000);
    let (ns, db, table) = typed(ids);

    let mut row_bytes = 0_usize;
    let transaction = store.begin().now().expect("a transaction begins");
    for row in 0..100_000_i64 {
        let payload = transaction
            .get(&address(ids, id(row)))
            .now()
            .expect("the read runs")
            .expect("the record");
        row_bytes += payload.len();
    }
    transaction.commit().now().expect("the read closes");

    let mut column_bytes = 0_usize;
    for (_, family) in declared_families(&store, ids) {
        column_bytes += family_chunks(&store, ns, db, table, &family)
            .now()
            .expect("the family reads")
            .iter()
            .map(|(_, chunk)| super::chunk::encode_chunk(&chunk.record_ids, &chunk.values).len())
            .sum::<usize>();
    }

    eprintln!("C2 fixture 100k rows: row payloads {row_bytes} B, column chunks {column_bytes} B");
    assert!(
        column_bytes < row_bytes,
        "the column layout must beat the row layout on this fixture: {column_bytes} vs {row_bytes}"
    );
}

// ---- W1 (G099 C1): the lane statistics the pointer value carries ----

use super::project::{
    LanePointer, LaneRange, SealedStat, lanes_from_bytes, lanes_to_bytes, merge_lane_range,
};

fn ptr(open: u64, range: Option<(i64, i64)>) -> LanePointer {
    LanePointer {
        open,
        range: range.map(|(min, max)| LaneRange {
            min: int(min),
            max: int(max),
        }),
        disabled: false,
    }
}

/// A lane pointer round-trips through the pointer value's bytes, with and
/// without its range statistics.
#[test]
fn lane_pointer_roundtrips_with_and_without_range() {
    let mut lanes = std::collections::BTreeMap::new();
    lanes.insert(0, ptr(3, Some((0, 24_575))));
    lanes.insert(1, ptr(0, None));
    let bytes = lanes_to_bytes(&lanes);

    let parsed = lanes_from_bytes(&bytes).expect("the pointer parses");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[&0].open, 3);
    let range = parsed[&0].range.as_ref().expect("lane 0 carries its range");
    assert_eq!(range.min, int(0));
    assert_eq!(range.max, int(24_575));
    assert!(
        parsed[&1].range.is_none(),
        "lane 1 was written without a range"
    );
    assert!(
        parsed.values().all(|lane| !lane.disabled),
        "no lane was written disabled"
    );
}

/// Pointers written before lane statistics existed — the bare eight-byte
/// form and the lane-count form — parse with no range: absence disables
/// pruning, it never guesses.
#[test]
fn legacy_pointer_bytes_have_no_range() {
    // The pre-partitioning form: one raw u64, one lane at partition 0.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&7_u64.to_be_bytes());
    let parsed = lanes_from_bytes(&legacy).expect("the legacy pointer parses");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[&0].open, 7);
    assert!(parsed[&0].range.is_none());
    assert!(
        parsed[&0].disabled,
        "a pointer from before lane statistics has unknown — disabled — history"
    );

    // The partitioning form without statistics: [count][(u32, u64)]* exact.
    let mut v1 = Vec::new();
    v1.extend_from_slice(&2_u32.to_be_bytes());
    for (partition, open) in [(0_u32, 5_u64), (3, 9)] {
        v1.extend_from_slice(&partition.to_be_bytes());
        v1.extend_from_slice(&open.to_be_bytes());
    }
    let parsed = lanes_from_bytes(&v1).expect("the v1 pointer parses");
    assert_eq!(parsed[&0].open, 5);
    assert_eq!(parsed[&3].open, 9);
    assert!(parsed.values().all(|lane| lane.range.is_none()));
    assert!(
        parsed.values().all(|lane| lane.disabled),
        "v1 pointers predate lane statistics: unknown history is disabled"
    );
}

/// A pointer value cut short refuses; a statistic read past its bytes would
/// be a wrong one, and a wrong one deletes rows.
#[test]
fn a_truncated_pointer_refuses() {
    let mut lanes = std::collections::BTreeMap::new();
    lanes.insert(0, ptr(1, Some((0, 10))));
    let bytes = lanes_to_bytes(&lanes);
    for cut in 1..bytes.len() {
        if cut == 8 {
            // Exactly eight bytes IS the pre-partitioning legacy form, not a
            // truncation of this value: it parses as a legacy pointer by the
            // legacy contract (a v2 value is always longer than eight bytes).
            continue;
        }
        assert!(
            lanes_from_bytes(&bytes[..cut]).is_err(),
            "a cut at {cut} must refuse"
        );
    }
}

/// The lane-range merge the writer runs in the commit batch: previous range
/// widened by the newly sealed chunks' ranges; an all-null chunk contributes
/// nothing; a chunk whose range a NaN disabled disables the lane's range.
#[test]
fn lane_range_merges_and_stays_absent_on_a_nan_chunk() {
    let clean: Vec<_> = (0..8).map(int).collect();
    let mut nan_values = clean.clone();
    nan_values.push(Value::Number(tessari_types::Number::float(f64::NAN)));

    let sealed = |values: &[Value]| {
        let ids: Vec<RecordId> = (0..values.len())
            .map(|row| RecordId::Int(i64::try_from(row).expect("a row count an i64 holds")))
            .collect();
        let bytes = super::chunk::encode_chunk(&ids, values);
        let (_, stats) = super::chunk::decode_head(&bytes).expect("the head reads");
        if stats.range_disabled {
            SealedStat::Disabled
        } else {
            match stats.min.zip(stats.max) {
                Some((min, max)) => SealedStat::Ranged(min, max),
                None => SealedStat::Empty,
            }
        }
    };

    let first = sealed(&clean);
    let second_values: Vec<_> = (100..108).map(int).collect();
    let second = sealed(&second_values);
    let nan_chunk = sealed(&nan_values);
    let all_null = sealed(&[Value::Null, Value::Null]);

    let grown = merge_lane_range(None, &[first.clone(), second.clone(), all_null])
        .expect("two ranged chunks give the lane a range; the all-null chunk adds nothing");
    assert_eq!(grown.min, int(0));
    assert_eq!(grown.max, int(107));

    // A NaN anywhere in the sealed set disables the lane's range.
    assert!(
        merge_lane_range(None, &[first.clone(), nan_chunk.clone()]).is_none(),
        "a NaN-disabled chunk disables the lane's range"
    );
    assert!(
        merge_lane_range(None, &[nan_chunk, second.clone()]).is_none(),
        "the disable does not depend on the chunk's position"
    );
    // And the disable wins over an existing range.
    let ranged = merge_lane_range(None, &[first, second]).expect("the lane ranges");
    assert!(
        merge_lane_range(Some(ranged), &[SealedStat::Disabled]).is_none(),
        "a NaN chunk arriving after the lane ranged disables it"
    );
}
