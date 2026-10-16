//! A transaction across leaders as a follower applies it: prepare, decide,
//! resolve — each one log record, each checked where it lands.

use std::sync::Arc;

use std::collections::BTreeMap;
use tessari_encoding::{
    Across, Decision, LogId, LogRecord, Mutation, Part, Participant, Provenance, RecordKey,
    RecordValue, StampedValue, StoreKey, TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};

use tessari_encoding::encode_payload;
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, ScanDirection, ScanRequest, Value as Stored,
};
use tessari_types::{DatabaseId, NamespaceId, Path, Reach, RecordId, Sequence, TableId, Value};

/// A record whose one field, `city`, is indexed.
fn doc(city: &str) -> Vec<u8> {
    let fields = BTreeMap::from([("city".to_owned(), Value::from(city))]);
    encode_payload(&Value::Object(fields)).into_bytes()
}

use crate::catalog::{Catalog, IndexShape, TableShape};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::RecordAddress;

const TRANSACTION: TransactionId = TransactionId::new([4; TRANSACTION_ID_LEN]);

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    /// The participant range's log, as a follower holds it.
    log: LogId,
    at: u64,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).await?;
        let mut transaction = store.begin().await?;
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns").await?.id;
        let database = catalog.create_database(namespace, "db").await?.id;
        let table = catalog
            .create_table(namespace, database, "t", TableShape::default())
            .await?
            .id;
        catalog
            .create_index(
                table,
                "by_city",
                vec![Path::field("city")],
                IndexShape::default(),
            )
            .await?;
        transaction.commit().await?;
        let mut transaction = store.begin().await?;
        transaction.put(
            RecordAddress::new(namespace, database, table, RecordId::from("r")),
            doc("old"),
        );
        transaction.commit().await?;
        Ok(Self {
            store,
            namespace,
            database,
            table,
            log: LogId::line(Reach::Namespace(NamespaceId::new(77))),
            at: 0,
        })
    }

    fn write(&self, value: StampedValue) -> Mutation {
        Mutation {
            namespace: self.namespace,
            database: self.database,
            table: self.table,
            id: RecordId::from("r"),
            shard: None,
            value,
        }
    }

    fn provenance(&self, provisional: bool) -> Provenance {
        Provenance {
            transaction: TRANSACTION,
            provisional,
            coordinator: Reach::Namespace(self.namespace),
            participants: if provisional {
                Vec::new()
            } else {
                // The home applying a prepare of these writes records (D6a).
                vec![Participant {
                    range: Reach::Database(self.namespace, self.database),
                    prepared_at: Some(Sequence::new(1)),
                }]
            },
        }
    }

    fn new_value(&self, provisional: bool) -> StampedValue {
        StampedValue::new(RecordValue::Present(doc("new")))
            .from_transaction(self.provenance(provisional))
    }

    async fn apply(&mut self, part: Part, mutations: Vec<Mutation>) -> Result<()> {
        let record = LogRecord::new(mutations).across(Across {
            transaction: TRANSACTION,
            part,
        });
        let at = self.at.saturating_add(1);
        self.store
            .apply_record_in(self.log, Sequence::new(at), &record)
            .await?;
        self.at = at;
        Ok(())
    }

    async fn prepare(&mut self) -> Result<()> {
        let coordinator = Reach::Namespace(self.namespace);
        self.apply(
            Part::Prepare { coordinator },
            vec![self.write(self.new_value(true))],
        )
        .await
    }

    async fn read(&self) -> Result<Option<Vec<u8>>> {
        self.store
            .begin()
            .await?
            .get(&RecordAddress::new(
                self.namespace,
                self.database,
                self.table,
                RecordId::from("r"),
            ))
            .await
    }

    /// Every entry of the index keyspace, as bytes.
    async fn index_entries(&self) -> Result<Vec<(Key, Stored)>> {
        Ok(self
            .store
            .backend()
            .scan(&ScanRequest {
                keyspace: Keyspace::INDEX,
                range: KeyRange::all(),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .await?)
    }

    /// Whether any intent is left under the record's key.
    async fn intent_left(&self) -> Result<bool> {
        let prefix = RecordKey::versions_prefix(
            self.namespace,
            self.database,
            self.table,
            &RecordId::from("r"),
        );
        let found = self
            .store
            .backend()
            .scan(&ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::prefix(&prefix),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .await?;
        for (_, value) in found {
            let stored = <StampedValue as tessari_encoding::StoreValue>::decode(value.as_slice())?;
            if super::is_intent(&stored) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The transaction's record as this node holds it.
async fn record_of(store: &Store) -> Result<Option<TransactionRecord>> {
    let key = tessari_encoding::TransactionRecordKey {
        transaction: TRANSACTION,
    };
    Ok(store
        .backend()
        .get(
            tessari_encoding::TransactionRecordKey::keyspace(),
            &key.encode(),
        )
        .await?
        .map(|value| <TransactionRecord as tessari_encoding::StoreValue>::decode(value.as_slice()))
        .transpose()?)
}

fn decided(decision: Decision) -> TransactionRecord {
    TransactionRecord {
        decision,
        deadline: 0,
        participants: vec![Participant {
            range: Reach::Namespace(NamespaceId::new(77)),
            prepared_at: Some(Sequence::new(1)),
        }],
    }
}

#[tokio::test]
async fn a_committed_transaction_becomes_the_value_and_leaves_no_intent() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.prepare().await?;
    assert_eq!(
        fixture.read().await?,
        Some(doc("old")),
        "an intent is not a value"
    );
    assert!(fixture.intent_left().await?);
    // A record is written PENDING before it is decided, as the coordinator
    // writes it.
    fixture
        .apply(Part::Decide(decided(Decision::Pending)), vec![])
        .await?;
    fixture
        .apply(Part::Decide(decided(Decision::Committed)), vec![])
        .await?;
    assert_eq!(
        record_of(&fixture.store).await?,
        Some(decided(Decision::Committed))
    );
    let resolved = fixture.write(fixture.new_value(false));
    fixture
        .apply(Part::Resolve { committed: true }, vec![resolved])
        .await?;
    assert_eq!(fixture.read().await?, Some(doc("new")));
    assert!(
        !fixture.intent_left().await?,
        "the resolution removes its intent"
    );
    Ok(())
}

#[tokio::test]
async fn an_aborted_transaction_leaves_the_old_value_and_no_intent() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.prepare().await?;
    // A record is written PENDING before it is decided, as the coordinator
    // writes it.
    fixture
        .apply(Part::Decide(decided(Decision::Pending)), vec![])
        .await?;
    fixture
        .apply(Part::Decide(decided(Decision::Aborted)), vec![])
        .await?;
    // An aborted resolution names the record; the value it carries is never
    // written.
    let named = fixture.write(fixture.new_value(true));
    fixture
        .apply(Part::Resolve { committed: false }, vec![named])
        .await?;
    assert_eq!(fixture.read().await?, Some(doc("old")));
    assert!(!fixture.intent_left().await?);
    Ok(())
}

#[tokio::test]
async fn a_prepare_carrying_a_plain_write_is_refused_and_lands_nothing() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let coordinator = Reach::Namespace(fixture.namespace);
    let plain = fixture.write(StampedValue::new(RecordValue::Present(doc("new"))));
    let refused = fixture
        .apply(Part::Prepare { coordinator }, vec![plain])
        .await;
    assert!(
        matches!(
            refused,
            Err(Error::AcrossMalformed {
                part: "prepare",
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(fixture.read().await?, Some(doc("old")));
    // The position did not move: the same record number is still free.
    fixture.prepare().await?;
    assert!(fixture.intent_left().await?);
    Ok(())
}

#[tokio::test]
async fn a_decision_carrying_writes_is_refused() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let smuggled = fixture.write(fixture.new_value(false));
    let refused = fixture
        .apply(Part::Decide(decided(Decision::Committed)), vec![smuggled])
        .await;
    assert!(
        matches!(refused, Err(Error::AcrossMalformed { part: "decide", .. })),
        "{refused:?}"
    );
    assert_eq!(record_of(&fixture.store).await?, None);
    Ok(())
}

#[tokio::test]
async fn an_intent_derives_no_index_entry_and_its_resolution_does() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let before = fixture.index_entries().await?;
    fixture.prepare().await?;
    // A record is written PENDING before it is decided, as the coordinator
    // writes it.
    fixture
        .apply(Part::Decide(decided(Decision::Pending)), vec![])
        .await?;
    fixture
        .apply(Part::Decide(decided(Decision::Committed)), vec![])
        .await?;
    assert_eq!(
        fixture.index_entries().await?,
        before,
        "a prepare and a decision leave every index byte as it was"
    );
    let resolved = fixture.write(fixture.new_value(false));
    fixture
        .apply(Part::Resolve { committed: true }, vec![resolved])
        .await?;
    assert_ne!(
        fixture.index_entries().await?,
        before,
        "the resolution moves the entry"
    );
    Ok(())
}

mod barred;
mod merged;
