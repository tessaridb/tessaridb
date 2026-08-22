//! Reading an index backwards, stopping at a bound.
//!
//! The session decides *when* an ordering may be taken from an index; this is
//! about what the read itself promises when it is. Three properties, and the
//! third is the reason behind a refusal that has no test above this layer.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bgv_db_encoding::encode_payload;
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::{Catalog, IndexDefinition, IndexShape, RecordAddress, Store, TableShape};
use bgv_db_types::{DatabaseId, NamespaceId, Path, RecordId, TableId, Value};

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
            .create_table(namespace.id, database.id, "users", TableShape::default())
            .unwrap();
        let index = catalog
            .create_index(
                table.id,
                "by_joined",
                vec![Path::field("joined")],
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

    fn at(&self, id: i64) -> RecordAddress {
        RecordAddress::new(self.namespace, self.database, self.table, RecordId::Int(id))
    }

    /// One record, committed on its own.
    fn write(&self, id: i64, joined: i64) {
        let mut transaction = self.store.begin().unwrap();
        let record = Value::Object(BTreeMap::from([("joined".to_owned(), Value::from(joined))]));
        transaction.put(self.at(id), encode_payload(&record).into_bytes());
        transaction.commit().unwrap();
    }

    /// The identities a bounded descending read produces, in the order it
    /// produced them.
    fn descending(&self, wanted: usize) -> Option<Vec<RecordId>> {
        let transaction = self.store.begin().unwrap();
        let found = transaction
            .records_in_descending_order(&self.index, wanted)
            .unwrap();
        let answer = found.map(|rows| rows.into_iter().map(|(id, _)| id).collect());
        transaction.rollback();
        answer
    }
}

#[test]
fn the_bound_is_filled_from_the_greatest_value_down() {
    let fixture = Fixture::new();
    for (id, joined) in [(1, 1990), (2, 1970), (3, 2010), (4, 1980)] {
        fixture.write(id, joined);
    }
    assert_eq!(
        fixture.descending(2).unwrap(),
        vec![RecordId::Int(3), RecordId::Int(1)]
    );
}

#[test]
fn the_tie_group_at_the_bound_is_drained_past_it() {
    // The walk may not stop at the bound when the entry it stopped on shares its
    // value with the next: the caller breaks ties by identity **ascending** and
    // the walk produces them descending, so cutting here would hand the caller a
    // set it cannot sort its way out of.
    let fixture = Fixture::new();
    for id in 1..=5 {
        fixture.write(id, 1900);
    }
    fixture.write(6, 2000);
    let found = fixture.descending(3).unwrap();
    assert_eq!(found[0], RecordId::Int(6));
    assert_eq!(
        found.len(),
        6,
        "the bound is three, and the group straddling it has five members: {found:?}"
    );
}

#[test]
fn an_index_that_cannot_fill_the_bound_answers_with_nothing_at_all() {
    // Not an empty answer and not an error: the records below the last entry are
    // the ones the index does not hold, so the read the caller wanted is not one
    // this can give.
    let fixture = Fixture::new();
    fixture.write(1, 1990);
    fixture.write(2, 1970);
    assert!(fixture.descending(3).is_none());
    assert_eq!(fixture.descending(2).unwrap().len(), 2);
}

#[test]
fn at_an_older_snapshot_the_entries_no_longer_describe_the_records() {
    // The fact behind a refusal the session makes and no session test can reach:
    // index entries hold the **current** state and carry no version, so a record
    // changed after a snapshot has no entry for the value that snapshot sees.
    //
    // A condition served by an index already accepts this — its candidates are
    // re-tested against the record. An ordering has none to re-test against, and
    // the position of the entry *is* the answer. So the session serves an order
    // only from the committed tail, and this is what it is avoiding.
    let fixture = Fixture::new();
    fixture.write(1, 1990);
    fixture.write(2, 1970);

    let held = fixture.store.begin().unwrap();
    // At this snapshot record 1 still holds 1990, and it is the greatest.
    assert_eq!(
        held.records_in_descending_order(&fixture.index, 2)
            .unwrap()
            .unwrap()
            .first()
            .map(|(id, _)| id.clone()),
        Some(RecordId::Int(1))
    );

    fixture.write(1, 1800);

    // Record 1 now sits under 1800 in the index while this reader still sees it
    // holding 1990. So the walk finds **both** records and puts them in the
    // wrong order: it reads 1970 first, because the only thing above it now is
    // an entry that describes a version this reader cannot see.
    //
    // Not a short answer — a *wrong* one, and one no re-test could repair, since
    // the position of the entry is the whole answer. The refusal is therefore on
    // the snapshot and not on the count.
    assert_eq!(
        held.records_in_descending_order(&fixture.index, 2)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![RecordId::Int(2), RecordId::Int(1)],
        "the reader's own records say 1990 comes before 1970"
    );
    held.rollback();
}
