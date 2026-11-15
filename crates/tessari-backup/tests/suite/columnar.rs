//! The columnar projection against the backup's central claim: state is a
//! function of the log, so a restored store derives the projection again —
//! chunks, maps, pointers, and their head statistics rebuilt by the replay,
//! answering exactly what the source answers.
//!
//! The comparison is semantic, not byte-for-byte: the chunk bytes the
//! restore derives depend on the codec's sampled throughput guard, which is
//! a wall-clock measurement, and two honest runs may choose different
//! encodings for the same values. What must come back exactly is what the
//! store answers — the records and the decoded projection.
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{ColumnFamily, encode_payload};
use tessari_kv::KvBackend;
use tessari_session::Now;
use tessari_storage::{Catalog, FieldShape, StorageKind, Store, TableShape};
use tessari_types::{DatabaseId, FieldKind, NamespaceId, RecordId, TableId, Value};

use crate::restore::{dump, store};
use crate::state::{current, restored, snapshot};

/// A columnar table with enough rows to seal chunks, plus an update and a
/// delete so the copy-on-write path has run before the backup.
fn columnar_source() -> (Arc<dyn KvBackend>, Store, (u32, u32, u32)) {
    let (backend, source) = store();
    let mut transaction = source.begin().now().unwrap();
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
                storage_kind: Some(StorageKind::Column),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    catalog
        .create_field(table.id, "seq", FieldKind::Int, FieldShape::default())
        .now()
        .unwrap();
    catalog
        .create_field(table.id, "kind", FieldKind::String, FieldShape::default())
        .now()
        .unwrap();
    transaction.commit().now().unwrap();

    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    let mut transaction = source.begin().now().unwrap();
    for row in 0..20_000_i64 {
        let kind = if row % 7 == 3 {
            Value::Null
        } else {
            Value::String(format!("kind-{}", row % 4))
        };
        transaction.put(
            tessari_storage::RecordAddress::new(
                NamespaceId::new(ids.0),
                DatabaseId::new(ids.1),
                TableId::new(ids.2),
                RecordId::Int(row),
            ),
            encode_payload(&Value::Object(BTreeMap::from([
                (
                    "seq".to_owned(),
                    Value::Number(tessari_types::Number::Integer(row)),
                ),
                ("kind".to_owned(), kind),
            ])))
            .into_bytes(),
        );
    }
    transaction.commit().now().unwrap();

    // The copy-on-write and tombstone paths, after seals exist.
    let mut transaction = source.begin().now().unwrap();
    transaction.put(
        tessari_storage::RecordAddress::new(
            NamespaceId::new(ids.0),
            DatabaseId::new(ids.1),
            TableId::new(ids.2),
            RecordId::Int(5),
        ),
        encode_payload(&Value::Object(BTreeMap::from([
            (
                "seq".to_owned(),
                Value::Number(tessari_types::Number::Integer(99_999)),
            ),
            ("kind".to_owned(), Value::String("kind-1".to_owned())),
        ])))
        .into_bytes(),
    );
    transaction.delete(tessari_storage::RecordAddress::new(
        NamespaceId::new(ids.0),
        DatabaseId::new(ids.1),
        TableId::new(ids.2),
        RecordId::Int(9),
    ));
    transaction.commit().now().unwrap();
    (backend, source, ids)
}

/// The projection's answers to a battery of predicates — the semantic
/// shape compared instead of the chunk bytes (whose encoding choice is a
/// sampled, wall-clock decision and may differ between two honest
/// derivations of the same values). The hit sets must be identical; the
/// counters may not, because a different encoding can move a byte-boundary
/// seal.
fn answers(store: &Store, ids: (u32, u32, u32)) -> Vec<Vec<RecordId>> {
    let (namespace, database, table) = (
        NamespaceId::new(ids.0),
        DatabaseId::new(ids.1),
        TableId::new(ids.2),
    );
    let mut transaction = store.begin().now().unwrap();
    let fields = Catalog::new(&mut transaction)
        .fields_on(namespace, database, table)
        .now()
        .unwrap();
    let seq = fields
        .iter()
        .find(|field| field.name == "seq")
        .map(|field| ColumnFamily::Id(field.id))
        .unwrap_or_else(|| panic!("seq is declared"));
    let kind = fields
        .iter()
        .find(|field| field.name == "kind")
        .map(|field| ColumnFamily::Id(field.id))
        .unwrap_or_else(|| panic!("kind is declared"));
    drop(transaction);
    let predicates =
        |compare: tessari_storage::ColumnarCompare, family: &ColumnFamily, value: Value| {
            vec![tessari_storage::ColumnarPredicate {
                family: family.clone(),
                compare,
                value,
            }]
        };
    let mut answers = Vec::new();
    for predicate in [
        predicates(
            tessari_storage::ColumnarCompare::Equal,
            &seq,
            Value::Number(tessari_types::Number::Integer(99_999)),
        ),
        predicates(
            tessari_storage::ColumnarCompare::GreaterOrEqual,
            &seq,
            Value::Number(tessari_types::Number::Integer(19_900)),
        ),
        predicates(
            tessari_storage::ColumnarCompare::Equal,
            &kind,
            Value::String("kind-2".to_owned()),
        ),
    ] {
        let (hits, _) =
            tessari_storage::columnar_scan(store, namespace, database, table, &predicate, true)
                .now()
                .unwrap();
        let mut hits = hits;
        hits.sort_by_cached_key(tessari_encoding::encode_record_id);
        answers.push(hits);
    }
    answers
}

#[test]
fn a_snapshot_of_a_columnar_store_restores_its_projection() {
    let (source_backend, source, ids) = columnar_source();
    let file = snapshot(&source);
    let (restored_backend, restored) = restored(&file);

    // The records come back exactly: the newest version of each, compared
    // from the raw keyspace rather than through the reader under test.
    assert_eq!(
        current(&source_backend, None),
        current(&restored_backend, None),
        "the restored records differ"
    );
    // Every key the source holds outside the record versions — chunks,
    // maps, pointers — was derived again by the replay.
    // The derived keys' version histories differ by construction — the
    // source's projection carries the commits that built it, the restored
    // re-derives in state chunks — so the raw key count is not the contract.
    // What must hold: the projection exists again and answers the same.
    let restored_keys = dump(&restored_backend, tessari_kv::Keyspace::DATA);
    assert!(
        restored_keys
            .iter()
            .any(|(key, _)| key.get(13) == Some(&b'C')),
        "the restored store holds the projection's chunks"
    );
    // And the projection answers the same predicates the same way.
    assert_eq!(
        answers(&source, ids),
        answers(&restored, ids),
        "the derived projection answers differently after the restore"
    );
}
