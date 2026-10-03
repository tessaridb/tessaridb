//! An intent is invisible to every reader and refuses every writer until its
//! transaction's record decides it (ADR-0112 D5). Each test has a control arm:
//! the same version written as a resolved one is read, so a test that passed
//! because the version was never reached would fail its control.

use std::sync::Arc;

use tessari_encoding::{
    Provenance, RecordValue, StampedValue, StoreKey, StoreValue, TRANSACTION_ID_LEN, TransactionId,
};
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, TableId};

use crate::catalog::{Catalog, TableShape};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::RecordAddress;

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
}

impl Fixture {
    /// A table holding `r = "old"`, then one more commit elsewhere, so that a
    /// version written at the newest position is inside every later snapshot.
    fn new() -> Result<Self> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)?;
        let mut transaction = store.begin()?;
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns")?.id;
        let database = catalog.create_database(namespace, "db")?.id;
        let table = catalog
            .create_table(namespace, database, "t", TableShape::default())?
            .id;
        transaction.commit()?;
        let fixture = Self {
            store,
            namespace,
            database,
            table,
        };
        let mut transaction = fixture.store.begin()?;
        transaction.put(fixture.address("r"), b"old".to_vec());
        transaction.commit()?;
        let mut transaction = fixture.store.begin()?;
        transaction.put(fixture.address("other"), b"x".to_vec());
        transaction.commit()?;
        Ok(fixture)
    }

    fn address(&self, id: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(id),
        )
    }

    /// Write `r = "new"` at the newest version, as an intent or as a resolved
    /// version of the same transaction.
    fn write_new(&self, provisional: bool) -> Result<()> {
        let version = self.store.committed_version()?;
        let key = tessari_encoding::RecordKey::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from("r"),
            version,
        );
        let value =
            StampedValue::new(RecordValue::Present(b"new".to_vec())).from_transaction(Provenance {
                transaction: TransactionId::new([9; TRANSACTION_ID_LEN]),
                provisional,
                coordinator: Reach::Namespace(self.namespace),
                participants: if provisional {
                    Vec::new()
                } else {
                    vec![tessari_encoding::Participant {
                        range: Reach::Namespace(self.namespace),
                        prepared_at: Some(version),
                    }]
                },
            });
        let mut batch = WriteBatch::new().put(
            tessari_encoding::RecordKey::keyspace(),
            key.encode(),
            value.encode(),
        );
        if !provisional {
            // Where its one part landed, as applying its prepare records it: a
            // resolved version is read only by a snapshot holding every part.
            let part = tessari_encoding::AcrossPartKey {
                transaction: TransactionId::new([9; TRANSACTION_ID_LEN]),
                range: Reach::Namespace(self.namespace),
            };
            batch = batch.put(
                tessari_encoding::AcrossPartKey::keyspace(),
                part.encode(),
                version.encode(),
            );
        }
        self.store.backend().apply(batch)?;
        Ok(())
    }

    /// What a fresh reader sees of `r`, by each read path.
    fn seen(&self) -> Result<[Option<Vec<u8>>; 3]> {
        let reader = self.store.begin()?;
        let single = reader.get(&self.address("r"))?;
        let batched = reader
            .get_each(&[self.address("r")])?
            .into_iter()
            .next()
            .flatten();
        let scanned = reader
            .first_records_of(self.namespace, self.database, self.table, 10)?
            .into_iter()
            .find(|(id, _)| *id == RecordId::from("r"))
            .map(|(_, payload)| payload);
        Ok([single, batched, scanned])
    }
}

#[test]
fn every_read_path_passes_over_an_intent_to_the_version_under_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write_new(true)?;
    let old = Some(b"old".to_vec());
    assert_eq!(fixture.seen()?, [old.clone(), old.clone(), old]);
    Ok(())
}

#[test]
fn control_the_same_version_resolved_is_read() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write_new(false)?;
    let new = Some(b"new".to_vec());
    assert_eq!(fixture.seen()?, [new.clone(), new.clone(), new]);
    Ok(())
}

#[test]
fn a_write_onto_a_standing_intent_is_refused_whatever_its_snapshot() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write_new(true)?;
    // Begun after the intent, so the intent is not newer than its snapshot:
    // only the intent rule can refuse it.
    let mut writer = fixture.store.begin()?;
    writer.put(fixture.address("r"), b"lost".to_vec());
    let refused = writer.commit();
    assert!(
        matches!(refused, Err(Error::Conflict { .. })),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn control_a_write_onto_a_resolved_version_commits() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write_new(false)?;
    let mut writer = fixture.store.begin()?;
    writer.put(fixture.address("r"), b"later".to_vec());
    writer.commit()?;
    Ok(())
}

#[test]
fn reclaiming_below_an_intent_keeps_the_version_every_reader_reads() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write_new(true)?;
    // Move the floor past the intent, so reclamation reaches it.
    let mut writer = fixture.store.begin()?;
    writer.put(fixture.address("later"), b"y".to_vec());
    writer.commit()?;
    fixture
        .store
        .reclaim_table(fixture.namespace, fixture.database, fixture.table)?;
    let old = Some(b"old".to_vec());
    assert_eq!(fixture.seen()?, [old.clone(), old.clone(), old]);
    Ok(())
}
