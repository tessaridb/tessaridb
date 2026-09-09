//! A series table answers above its floor, and the records below it are still there.
//!
//! The engine's promise is about the **answer**, not about storage: past the
//! retention a record is not returned, and its removal is a separate act. That
//! separation is the whole safety property — a removal pass that lags, is
//! throttled or never runs costs disk and never an answer — and it is only
//! believable if a test asserts both halves at once. So every case below reads
//! the raw keyspace through the backend the store was opened on, and asserts the
//! bytes are present while the read declines to return them.
//!
//! The identities are built rather than minted, because the fixture needs a
//! record written two hours ago and the clock will not oblige. A UUID version 7
//! carries the millisecond in its leading six bytes, big-endian, which is the
//! property the whole engine rests on — so constructing one at a chosen instant
//! is not a trick around the implementation, it is the format.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tessari_encoding::{RecordKey, StoreKey, encode_payload};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_storage::{Catalog, RecordAddress, SeriesDeclaration, Store, TableKind, TableShape};
use tessari_types::{DatabaseId, Duration, IdentityKind, NamespaceId, RecordId, TableId, Value};

/// An hour, as the retention every fixture below declares.
const RETAIN: Duration = Duration::from_seconds(3_600);

/// The millisecond the test is running in.
fn now_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A UUID version 7 minted `ago` milliseconds before `base`.
///
/// `base` is the fixture's own instant rather than the clock, and that is not a
/// tidiness: computing an identity twice from `SystemTime::now()` produces two
/// different identities whenever a millisecond turns over between the two calls,
/// so a test that wrote one and then looked the other up would fail perhaps once
/// in a hundred runs, with a diff of a single byte.
///
/// The bytes below the millisecond are fixed rather than random, so a failure
/// prints the same identity twice and a reader can tell two fixtures apart.
fn identity(base: u64, ago: u64, tag: u8) -> RecordId {
    let mut bytes = [tag; 16];
    let [_, _, t0, t1, t2, t3, t4, t5] = base.saturating_sub(ago).to_be_bytes();
    bytes[0] = t0;
    bytes[1] = t1;
    bytes[2] = t2;
    bytes[3] = t3;
    bytes[4] = t4;
    bytes[5] = t5;
    // Version and variant, so the value is a UUID a strict reader accepts and
    // not merely sixteen bytes that sort correctly.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::Uuid(bytes)
}

struct Fixture {
    backend: Arc<dyn KvBackend>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    /// The instant every identity in this fixture is derived from.
    base: u64,
    /// The record two hours old, minted once.
    old: RecordId,
    /// The record a second old, minted once.
    fresh: RecordId,
}

impl Fixture {
    /// A table of `kind`, holding one record from two hours ago and one from now.
    fn holding(kind: TableKind, identity_kind: IdentityKind) -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(Arc::clone(&backend)).unwrap();

        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "metrics").unwrap();
        let table = catalog
            .create_table(
                namespace.id,
                database.id,
                "readings",
                TableShape {
                    schemafull: false,
                    kind,
                    identity: identity_kind,
                    graph: None,
                },
            )
            .unwrap();
        transaction.commit().unwrap();

        let base = now_millis();
        let held = Self {
            backend,
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            base,
            old: identity(base, OLD, 0xa1),
            fresh: identity(base, FRESH, 0xb2),
        };
        held.write(held.old.clone(), "stale");
        held.write(held.fresh.clone(), "current");
        held
    }

    fn address(&self, id: RecordId) -> RecordAddress {
        RecordAddress::new(self.namespace, self.database, self.table, id)
    }

    fn write(&self, id: RecordId, label: &str) {
        let mut transaction = self.store.begin().unwrap();
        let fields = BTreeMap::from([("label".to_owned(), Value::from(label))]);
        transaction.put(
            self.address(id),
            encode_payload(&Value::Object(fields)).into_bytes(),
        );
        transaction.commit().unwrap();
    }

    /// The labels a plain read of the table answers with, in key order.
    fn labels(&self) -> Vec<String> {
        let transaction = self.store.begin().unwrap();
        transaction
            .scan_table(self.namespace, self.database, self.table)
            .unwrap()
            .into_iter()
            .map(|(_, payload)| {
                let Value::Object(fields) = tessari_encoding::decode_payload(&payload).unwrap()
                else {
                    panic!("a record is an object");
                };
                let Some(Value::String(label)) = fields.get("label") else {
                    panic!("every record carries a label");
                };
                label.clone()
            })
            .collect()
    }

    /// How many entries the record keyspace holds for this table.
    ///
    /// Read from the backend the store was opened on, so it is the bytes on the
    /// substrate and not another read through the same rules being tested.
    fn stored_entries(&self) -> usize {
        let prefix = RecordKey::table_prefix(self.namespace, self.database, self.table);
        self.backend
            .scan(&ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::prefix(&prefix),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .unwrap()
            .len()
    }
}

/// Two hours, in milliseconds — comfortably past an hour's retention.
const OLD: u64 = 2 * 60 * 60 * 1_000;
/// One second, in milliseconds — comfortably inside it.
const FRESH: u64 = 1_000;

#[test]
fn a_series_table_does_not_answer_with_a_record_past_its_floor() {
    let fixture = Fixture::holding(
        TableKind::Series(SeriesDeclaration { retain: RETAIN }),
        IdentityKind::Uuid,
    );

    assert_eq!(fixture.labels(), vec!["current".to_owned()]);
    // The half that makes it a promise about answers rather than about storage.
    assert_eq!(
        fixture.stored_entries(),
        2,
        "both records are still on the substrate; the floor hides one rather than removing it"
    );
}

#[test]
fn a_point_read_below_the_floor_answers_nothing() {
    let fixture = Fixture::holding(
        TableKind::Series(SeriesDeclaration { retain: RETAIN }),
        IdentityKind::Uuid,
    );
    let transaction = fixture.store.begin().unwrap();

    // Naming the record directly must not step around the floor. This is the
    // path a gate applied per read shape would have missed.
    assert!(
        transaction
            .get(&fixture.address(fixture.old.clone()))
            .unwrap()
            .is_none()
    );
    assert!(
        transaction
            .get(&fixture.address(fixture.fresh.clone()))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_batched_read_below_the_floor_answers_nothing() {
    let fixture = Fixture::holding(
        TableKind::Series(SeriesDeclaration { retain: RETAIN }),
        IdentityKind::Uuid,
    );
    let transaction = fixture.store.begin().unwrap();

    // `get_each` is what an index-served read resolves through, so it carries
    // the floor separately and is asserted separately.
    let answers = transaction
        .get_each(&[
            fixture.address(fixture.old.clone()),
            fixture.address(fixture.fresh.clone()),
        ])
        .unwrap();
    assert!(answers[0].is_none());
    assert!(answers[1].is_some());
}

#[test]
fn a_record_written_below_the_floor_in_this_transaction_is_not_answered_either() {
    let fixture = Fixture::holding(
        TableKind::Series(SeriesDeclaration { retain: RETAIN }),
        IdentityKind::Uuid,
    );
    let mut transaction = fixture.store.begin().unwrap();
    let fields = BTreeMap::from([("label".to_owned(), Value::from("backfilled"))]);
    transaction.put(
        fixture.address(identity(fixture.base, OLD.saturating_add(1), 0xc3)),
        encode_payload(&Value::Object(fields)).into_bytes(),
    );

    // A transaction sees its own writes — except below the floor, or the floor
    // would be a property of who is asking rather than of the table.
    let answered: Vec<RecordId> = transaction
        .scan_table(fixture.namespace, fixture.database, fixture.table)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(answered, vec![fixture.fresh.clone()]);
}

#[test]
fn the_same_records_in_a_plain_table_are_all_answered() {
    // The falsification. Every assertion above is consistent with the fixture
    // simply never having written the old record; this is what tells the two
    // apart, and it is the same two identities through the same writes.
    let fixture = Fixture::holding(TableKind::Table, IdentityKind::Uuid);

    let mut labels = fixture.labels();
    labels.sort();
    assert_eq!(labels, vec!["current".to_owned(), "stale".to_owned()]);
    assert_eq!(fixture.stored_entries(), 2);
}
