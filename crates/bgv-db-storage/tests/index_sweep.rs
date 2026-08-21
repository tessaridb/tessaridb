//! The bidirectional sweep: the index and the records agree, in both directions.
//!
//! The other index tests each pin one behaviour — a write adds an entry, a change
//! removes the old one, a delete removes both. This one asks the question those
//! cannot: after an arbitrary run of writes, updates, deletes and refused unique
//! claims, does the index still describe exactly the records that are there?
//!
//! Both directions matter and they fail differently:
//!
//! - **Record → entry.** A missing entry makes an index read return *fewer* rows
//!   with no error. Since the access path is chosen by what exists, that is a
//!   query silently answering wrong.
//! - **Entry → record.** An orphan entry points at a record that no longer holds
//!   the value. Reads survive it today because every candidate is confirmed at
//!   the reader's snapshot, so the damage is wasted work and space that nothing
//!   reconciles — but the confirmation is a safety net, not a licence for the
//!   index to be wrong.
//!
//! The expected set is re-derived here from the records rather than by calling
//! the store's own projection. Comparing a function against itself proves
//! nothing; this is a second, independent statement of what the entries should
//! be.
//!
//! The workload is pseudo-random and **deterministic** — a fixed seed and a
//! multiplicative generator — so a failure is reproducible from the seed printed
//! in the assertion rather than being a story about a run nobody can repeat.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bgv_db_encoding::{
    IndexAddress, IndexValues, KeyKind, SecondaryIndexKey, StoreKey, UniqueIndexKey,
    decode_payload, encode_payload,
};
use bgv_db_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use bgv_db_storage::{
    Catalog, Error, IndexDefinition, IndexShape, RecordAddress, Store, TableShape,
};
use bgv_db_types::{DatabaseId, NamespaceId, Path, RecordId, Step, TableId, Value};

/// The seed the workload runs from. Printed by every failing assertion.
const SEED: u64 = 0x0de5_eed1_5bad_c0de;
/// How many workload steps to run.
const STEPS: u64 = 400;
/// How many distinct records the workload writes over.
const RECORDS: u64 = 40;
/// How many distinct values each indexed field draws from.
///
/// Small on purpose: collisions are what exercise a non-unique index holding
/// several records under one value, and what makes the unique index refuse.
const VALUES: u64 = 7;

/// A multiplicative congruential generator, so the workload is a function of the
/// seed and nothing else.
struct Rolls(u64);

impl Rolls {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A roll below `bound`. A zero bound yields zero rather than dividing by it.
    fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }
}

struct Fixture {
    backend: Arc<dyn KvBackend>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    indexes: Vec<IndexDefinition>,
}

impl Fixture {
    /// A table with four indexes: one unique on a single field, one not, one over
    /// a pair, and one over a **nested** value — so the sweep covers single,
    /// composite and path projection, and both key layouts.
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(Arc::clone(&backend)).unwrap();

        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "orders").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "people", TableShape::default())
            .unwrap();
        let indexes = vec![
            catalog
                .create_index(
                    table.id,
                    "by_email",
                    vec![Path::field("email")],
                    IndexShape {
                        unique: true,
                        search: false,
                        vector: None,
                    },
                )
                .unwrap(),
            catalog
                .create_index(
                    table.id,
                    "by_city",
                    vec![Path::field("city")],
                    IndexShape::default(),
                )
                .unwrap(),
            catalog
                .create_index(
                    table.id,
                    "by_city_and_name",
                    vec![Path::field("city"), Path::field("name")],
                    IndexShape::default(),
                )
                .unwrap(),
            catalog
                .create_index(
                    table.id,
                    "by_home_city",
                    vec![Path::parse("address.city").unwrap()],
                    IndexShape::default(),
                )
                .unwrap(),
        ];
        transaction.commit().unwrap();

        Self {
            backend,
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            indexes,
        }
    }

    fn at(&self, n: u64) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(format!("p{n:03}")),
        )
    }

    /// Every entry key currently in the substrate, across all four indexes.
    fn entries(&self) -> BTreeSet<Vec<u8>> {
        let mut found = BTreeSet::new();
        for index in &self.indexes {
            let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
            let kind = if index.unique {
                KeyKind::UniqueIndex
            } else {
                KeyKind::SecondaryIndex
            };
            let prefix = address.prefix(kind);
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::prefix(&prefix),
                direction: ScanDirection::Forward,
                limit: None,
            };
            for (key, _) in self.backend.scan(&request).unwrap() {
                found.insert(key.as_slice().to_vec());
            }
        }
        found
    }

    /// Every entry key the live records say should exist.
    ///
    /// Derived here rather than by calling the store's projection, so the two
    /// sides of the comparison are genuinely independent.
    fn expected_entries(&self) -> BTreeSet<Vec<u8>> {
        let transaction = self.store.begin().unwrap();
        let records = transaction
            .scan_table(self.namespace, self.database, self.table)
            .unwrap();

        let mut expected = BTreeSet::new();
        for (id, payload) in &records {
            let record = decode_payload(payload).unwrap();
            assert!(
                matches!(record, Value::Object(_)),
                "the workload only writes objects"
            );
            for index in &self.indexes {
                let mut values = Vec::with_capacity(index.fields.len());
                for path in &index.fields {
                    // Absent and `none` are the same answer: there is no value to
                    // place, so the record is not in this index at all.
                    match walk(&record, path) {
                        Some(Value::None) | None => {
                            values.clear();
                            break;
                        }
                        Some(found) => values.push(found.clone()),
                    }
                }
                if values.len() != index.fields.len() {
                    continue;
                }
                let address =
                    IndexAddress::new(index.namespace, index.database, index.table, index.id);
                let projected = IndexValues::of(&values);
                let key = if index.unique {
                    UniqueIndexKey::new(address, projected).encode()
                } else {
                    SecondaryIndexKey::new(address, projected, id.clone()).encode()
                };
                expected.insert(key.as_slice().to_vec());
            }
        }
        expected
    }
}

/// The value a path reaches, written out here rather than borrowed.
///
/// `Path::resolve` would answer the same question, and that is exactly why it is
/// not called: the store projects entries with it, so a sweep that also used it
/// would compare a function against itself — the thing the module documentation
/// says this test exists not to do.
fn walk<'v>(record: &'v Value, path: &Path) -> Option<&'v Value> {
    let Value::Object(fields) = record else {
        return None;
    };
    let mut at = fields.get(path.root())?;
    for step in path.steps() {
        at = match (step, at) {
            (Step::Field(name), Value::Object(fields)) => fields.get(name)?,
            (Step::Index(position), Value::Array(items)) => {
                items.get(usize::try_from(*position).ok()?)?
            }
            _ => return None,
        };
    }
    Some(at)
}

/// The address a record claims in the unique index.
///
/// Mostly its own, so records actually land; sometimes one drawn from the small
/// pool, so the unique index refuses a duplicate now and then. A workload where
/// every record fights for one of seven addresses ends with seven records and
/// proves almost nothing.
fn email(n: u64, rolls: &mut Rolls) -> Value {
    if rolls.below(8) == 0 {
        Value::from(format!("shared-e{}", rolls.below(VALUES)))
    } else {
        Value::from(format!("e{n:03}"))
    }
}

/// One workload step: a write, a write of a record missing a field, or a delete.
fn step(fixture: &Fixture, rolls: &mut Rolls) {
    let n = rolls.below(RECORDS);
    let address = fixture.at(n);
    let mut transaction = fixture.store.begin().unwrap();

    match rolls.below(10) {
        0..=1 => transaction.delete(address),
        2 => {
            // A record with no `city` is in neither city index, and one with no
            // `email` is in no unique index — the "not indexed at all" case, which
            // a sweep that only ever writes complete records never reaches.
            let fields = BTreeMap::from([(
                "name".to_owned(),
                Value::from(format!("n{}", rolls.below(VALUES))),
            )]);
            transaction.put(address, encode_payload(&Value::Object(fields)).into_bytes());
        }
        3 => {
            // `null` is a value and is indexed under `null`, unlike absence.
            let fields = BTreeMap::from([
                ("name".to_owned(), Value::Null),
                (
                    "city".to_owned(),
                    Value::from(format!("c{}", rolls.below(VALUES))),
                ),
                ("email".to_owned(), email(n, rolls)),
            ]);
            transaction.put(address, encode_payload(&Value::Object(fields)).into_bytes());
        }
        _ => {
            let mut fields = BTreeMap::from([
                (
                    "name".to_owned(),
                    Value::from(format!("n{}", rolls.below(VALUES))),
                ),
                (
                    "city".to_owned(),
                    Value::from(format!("c{}", rolls.below(VALUES))),
                ),
                ("email".to_owned(), email(n, rolls)),
            ]);
            // Sometimes nested, sometimes not, and sometimes nested but shaped
            // wrong — so the path index sees a value, an absence, and a route
            // that ends early, which are its three answers.
            match rolls.below(4) {
                0 => {}
                1 => {
                    fields.insert("address".to_owned(), Value::from("elsewhere"));
                }
                _ => {
                    fields.insert(
                        "address".to_owned(),
                        Value::Object(BTreeMap::from([(
                            "city".to_owned(),
                            Value::from(format!("h{}", rolls.below(VALUES))),
                        )])),
                    );
                }
            }
            transaction.put(address, encode_payload(&Value::Object(fields)).into_bytes());
        }
    }

    match transaction.commit() {
        Ok(_) => {}
        // The unique index refusing a duplicate is part of the workload, not a
        // failure of it — and a refusal that left entries behind is exactly the
        // kind of damage this sweep exists to catch, so the run continues.
        Err(Error::UniqueViolation { .. }) => {}
        Err(other) => panic!("unexpected error at seed {SEED}: {other}"),
    }
}

#[test]
fn the_index_and_the_records_agree_in_both_directions() {
    let fixture = Fixture::new();
    let mut rolls = Rolls(SEED);

    for _ in 0..STEPS {
        step(&fixture, &mut rolls);
    }

    let expected = fixture.expected_entries();
    let found = fixture.entries();

    let missing: Vec<_> = expected.difference(&found).collect();
    assert!(
        missing.is_empty(),
        "seed {SEED}: {} record(s) have no index entry — an index read would return fewer rows and raise nothing",
        missing.len()
    );

    let orphans: Vec<_> = found.difference(&expected).collect();
    assert!(
        orphans.is_empty(),
        "seed {SEED}: {} entry(s) point at a value no record holds",
        orphans.len()
    );

    // A sweep over an empty store proves nothing, and a bug that deleted every
    // entry would satisfy both assertions above.
    assert!(
        u64::try_from(found.len()).unwrap_or(u64::MAX) > RECORDS,
        "seed {SEED}: only {} entries — the workload did not exercise anything",
        found.len()
    );
}

#[test]
fn the_sweep_notices_an_entry_that_should_not_be_there() {
    // The sweep's own teeth. Without this, a sweep that compared nothing to
    // nothing would pass forever and be mistaken for coverage.
    let fixture = Fixture::new();
    let mut rolls = Rolls(SEED);
    for _ in 0..20 {
        step(&fixture, &mut rolls);
    }

    let planted = {
        let index = &fixture.indexes[1];
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        SecondaryIndexKey::new(
            address,
            IndexValues::of(&[Value::from("nowhere")]),
            RecordId::from("p999"),
        )
        .encode()
    };
    let expected = fixture.expected_entries();
    assert!(
        !expected.contains(planted.as_slice()),
        "the planted entry must not be one the records ask for"
    );

    let mut found = fixture.entries();
    found.insert(planted.as_slice().to_vec());
    assert_eq!(
        found.difference(&expected).count(),
        1,
        "the sweep must see exactly the planted orphan"
    );
}
