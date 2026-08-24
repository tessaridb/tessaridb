//! An index range read wider than a single fetch.
//!
//! The read resolves its entries in batches, so it has a **seam** — a point
//! where one fetch ends and the next must resume without dropping or repeating
//! the entry it stopped on. A range narrower than one batch has no seam, which
//! is why every one of these is built from the batch size itself rather than
//! from a round number: a fixture that stopped tracking the constant would go on
//! passing while testing nothing.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, IndexDefinition, IndexShape, RecordAddress, Store, TableShape};
use tessari_types::{DatabaseId, NamespaceId, Path, RecordId, TableId, Value};

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    index: IndexDefinition,
}

impl Fixture {
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(backend).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "shop").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "readings", TableShape::default())
            .unwrap();
        let index = catalog
            .create_index(
                table.id,
                "by_taken",
                vec![Path::field("taken")],
                IndexShape {
                    unique: false,
                    search: false,
                    vector: None,
                },
            )
            .unwrap();
        transaction.commit().unwrap();
        Self {
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            index,
        }
    }

    fn at(&self, id: RecordId) -> RecordAddress {
        RecordAddress::new(self.namespace, self.database, self.table, id)
    }

    /// Every record in one commit.
    ///
    /// Index entries are derived at commit, so one commit and many produce the
    /// same entries — and one is what keeps a four-hundred-record fixture from
    /// being four hundred log records.
    fn write(&self, records: impl IntoIterator<Item = (RecordId, i64)>) {
        let mut transaction = self.store.begin().unwrap();
        for (id, taken) in records {
            let record = Value::Object(BTreeMap::from([("taken".to_owned(), Value::from(taken))]));
            transaction.put(self.at(id), encode_payload(&record).into_bytes());
        }
        transaction.commit().unwrap();
    }

    /// The identities a range read produces.
    fn range(&self, lower: Option<i64>, upper: Option<i64>) -> Vec<RecordId> {
        let transaction = self.store.begin().unwrap();
        let found = transaction
            .records_in_range(
                &self.index,
                &[],
                lower.map(Value::from).as_ref(),
                upper.map(Value::from).as_ref(),
            )
            .unwrap();
        transaction.rollback();
        found.into_iter().map(|(id, _)| id).collect()
    }
}

/// Integer identities `0..count`, each with its own indexed value.
fn spread(count: usize) -> Vec<(RecordId, i64)> {
    (0..count)
        .map(|n| {
            let n = i64::try_from(n).unwrap();
            (RecordId::Int(n), n)
        })
        .collect()
}

#[test]
fn a_range_several_batches_wide_answers_every_record_in_it() {
    let fixture = Fixture::new();
    let count = RANGE_SCAN_BATCH_ENTRIES * 3 + 7;
    fixture.write(spread(count));

    let found = fixture.range(None, None);
    let expected: Vec<RecordId> = (0..i64::try_from(count).unwrap())
        .map(RecordId::Int)
        .collect();
    assert_eq!(found, expected, "a wide range lost or repeated an entry");
}

#[test]
fn a_seam_at_an_exact_multiple_of_the_batch_neither_drops_nor_repeats() {
    // Two batches exactly: the fetch that fills the second one returns a full
    // batch and there is nothing after it, which is the case a loop that decides
    // "a full batch means there is more" has to survive without answering twice.
    let fixture = Fixture::new();
    let count = RANGE_SCAN_BATCH_ENTRIES * 2;
    fixture.write(spread(count));

    let found = fixture.range(None, None);
    assert_eq!(
        found.len(),
        count,
        "an exact multiple of the batch mis-counted"
    );
    let expected: Vec<RecordId> = (0..i64::try_from(count).unwrap())
        .map(RecordId::Int)
        .collect();
    assert_eq!(found, expected);
}

#[test]
fn a_bounded_range_straddling_a_seam_answers_only_what_it_asked_for() {
    // The range is a batch and one wider and starts away from the table's own
    // start, so the walk has to resume *and* stop, and neither point is where
    // the seam falls. An earlier draft of this asked for eleven records around
    // the seam's index — a range that fits in one fetch and tests nothing, which
    // is exactly the mistake the whole file is built to avoid.
    let fixture = Fixture::new();
    fixture.write(spread(RANGE_SCAN_BATCH_ENTRIES * 3));

    let lower = 5;
    let upper = i64::try_from(RANGE_SCAN_BATCH_ENTRIES).unwrap() + 5;
    let found = fixture.range(Some(lower), Some(upper));

    // Both ends are inclusive at this layer — the condition above re-tests them.
    let expected: Vec<RecordId> = (lower..=upper).map(RecordId::Int).collect();
    assert_eq!(found, expected);
}

#[test]
fn identities_in_a_prefix_relation_survive_the_seam() {
    // Every record shares one indexed value, so the entries differ only in their
    // identity and sit next to each other — and the two at the seam are chosen
    // so that one identity's *text* is a prefix of the other's.
    //
    // Nothing is dropped because a variable-width identity is terminated when it
    // is encoded, which makes `enc("z")` differ from `enc("z0")` at the byte
    // after `z` rather than running out. That property lives in the encoder, one
    // crate away from the loop that depends on it, which is the reason the walk
    // resumes at a byte successor that needs no property at all.
    let fixture = Fixture::new();
    let mut records: Vec<(RecordId, i64)> = (0..RANGE_SCAN_BATCH_ENTRIES - 1)
        .map(|n| (RecordId::from(format!("a{n:04}").as_str()), 7))
        .collect();
    records.push((RecordId::from("z"), 7));
    records.push((RecordId::from("z0"), 7));
    fixture.write(records);

    let found = fixture.range(Some(7), Some(7));
    assert_eq!(
        found.len(),
        RANGE_SCAN_BATCH_ENTRIES + 1,
        "an identity at the seam went missing"
    );
    assert!(found.contains(&RecordId::from("z")));
    assert!(found.contains(&RecordId::from("z0")));
}

#[test]
fn an_uncommitted_write_is_folded_into_a_range_of_any_width() {
    // The fold over this transaction's own writes happens once, after the walk,
    // and a walk that now runs several times must not have made it run several
    // times too — nor stopped happening.
    let fixture = Fixture::new();
    let count = RANGE_SCAN_BATCH_ENTRIES * 2 + 3;
    fixture.write(spread(count));

    let mut transaction = fixture.store.begin().unwrap();
    let fresh = i64::try_from(count).unwrap();
    let record = Value::Object(BTreeMap::from([("taken".to_owned(), Value::from(fresh))]));
    transaction.put(
        fixture.at(RecordId::Int(fresh)),
        encode_payload(&record).into_bytes(),
    );
    transaction.delete(fixture.at(RecordId::Int(0)));

    let found = transaction
        .records_in_range(&fixture.index, &[], None, None)
        .unwrap();
    transaction.rollback();

    let ids: Vec<RecordId> = found.into_iter().map(|(id, _)| id).collect();
    assert!(
        ids.contains(&RecordId::Int(fresh)),
        "a record written in this transaction has no index entry and was lost"
    );
    assert!(
        !ids.contains(&RecordId::Int(0)),
        "a record deleted in this transaction still has an entry and came back"
    );
    assert_eq!(ids.len(), count);
}
