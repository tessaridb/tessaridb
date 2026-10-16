//! A transaction across leaders that wrote two databases, applied one part at
//! a time as a follower holding both does — what the tests of what a reader
//! sees (ADR-0112 D6a) and of whether an index agrees with it (Q-919) share.

use std::sync::Arc;

use tessari_encoding::{
    Across, Decision, LogId, LogRecord, Mutation, Part, Participant, Provenance, RecordValue,
    StampedValue, TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, TableId};

use crate::catalog::{Catalog, TableShape};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::RecordAddress;

pub(super) const TRANSACTION: TransactionId = TransactionId::new([6; TRANSACTION_ID_LEN]);

/// One database the transaction writes: its table, and the log a follower
/// holds its range's records in.
pub(super) struct Home {
    pub(super) database: DatabaseId,
    pub(super) table: TableId,
    log: LogId,
    at: std::cell::Cell<u64>,
}

pub(super) struct Fixture {
    pub(super) store: Store,
    pub(super) namespace: NamespaceId,
    /// The coordinator's range first, as the record names it.
    pub(super) homes: [Home; 2],
}

impl Fixture {
    /// Two databases, each with a table holding `r = "old"`.
    pub(super) async fn new() -> Result<Self> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).await?;
        let mut transaction = store.begin()?;
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns").await?.id;
        let mut homes = Vec::new();
        for name in ["one", "two"] {
            let database = catalog.create_database(namespace, name).await?.id;
            let table = catalog
                .create_table(namespace, database, "t", TableShape::default())
                .await?
                .id;
            homes.push(Home {
                database,
                table,
                log: LogId::line(Reach::Database(namespace, database)),
                at: std::cell::Cell::new(0),
            });
        }
        transaction.commit().await?;
        let [first, second]: [Home; 2] = homes.try_into().map_err(|_| Error::AcrossMalformed {
            part: "fixture",
            problem: "two homes",
        })?;
        let fixture = Self {
            store,
            namespace,
            homes: [first, second],
        };
        let mut writing = fixture.store.begin()?;
        for home in 0..2 {
            writing.put(fixture.address(home), b"old".to_vec());
        }
        writing.commit().await?;
        Ok(fixture)
    }

    pub(super) fn range(&self, home: usize) -> Reach {
        Reach::Database(self.namespace, self.homes[home].database)
    }

    pub(super) fn address(&self, home: usize) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.homes[home].database,
            self.homes[home].table,
            RecordId::from("r"),
        )
    }

    pub(super) fn participants(&self) -> Vec<Participant> {
        (0..2)
            .map(|home| Participant {
                range: self.range(home),
                prepared_at: Some(Sequence::new(1)),
            })
            .collect()
    }

    /// `r = "new"` in `home`, as an intent or as resolved.
    pub(super) fn write(&self, home: usize, provisional: bool) -> Mutation {
        Mutation {
            namespace: self.namespace,
            database: self.homes[home].database,
            table: self.homes[home].table,
            id: RecordId::from("r"),
            shard: None,
            value: StampedValue::new(RecordValue::Present(b"new".to_vec())).from_transaction(
                Provenance {
                    transaction: TRANSACTION,
                    provisional,
                    coordinator: self.range(0),
                    participants: if provisional {
                        Vec::new()
                    } else {
                        self.participants()
                    },
                },
            ),
        }
    }

    /// Apply one record of the transaction in `home`'s log, as a follower does.
    pub(super) async fn apply(
        &self,
        home: usize,
        part: Part,
        mutations: Vec<Mutation>,
    ) -> Result<()> {
        self.apply_on(&self.store, home, part, mutations).await
    }

    /// [`Self::apply`] onto `store` — a copy restored from this one, which
    /// continues each log from where it stood.
    pub(super) async fn apply_on(
        &self,
        store: &Store,
        home: usize,
        part: Part,
        mutations: Vec<Mutation>,
    ) -> Result<()> {
        let record = LogRecord::new(mutations).across(Across {
            transaction: TRANSACTION,
            part,
        });
        let at = self.homes[home].at.get().saturating_add(1);
        store
            .apply_record_in(self.homes[home].log, Sequence::new(at), &record)
            .await?;
        self.homes[home].at.set(at);
        Ok(())
    }

    pub(super) async fn prepare(&self, home: usize) -> Result<()> {
        self.prepare_on(&self.store, home).await
    }

    pub(super) async fn prepare_on(&self, store: &Store, home: usize) -> Result<()> {
        let coordinator = self.range(0);
        let write = self.write(home, true);
        self.apply_on(store, home, Part::Prepare { coordinator }, vec![write])
            .await
    }

    pub(super) async fn decide(&self, decision: Decision) -> Result<()> {
        let record = TransactionRecord {
            decision,
            deadline: 0,
            participants: self.participants(),
        };
        self.apply(0, Part::Decide(record), Vec::new()).await
    }

    pub(super) async fn forget(&self) -> Result<()> {
        let coordinator = self.range(0);
        self.apply(0, Part::Forget { coordinator }, Vec::new())
            .await
    }

    pub(super) async fn resolve(&self, home: usize) -> Result<()> {
        self.resolve_on(&self.store, home).await
    }

    pub(super) async fn resolve_on(&self, store: &Store, home: usize) -> Result<()> {
        let write = self.write(home, false);
        self.apply_on(store, home, Part::Resolve { committed: true }, vec![write])
            .await
    }
}

pub(super) async fn read(
    reader: &crate::transaction::Transaction<'_>,
    address: &RecordAddress,
) -> Result<String> {
    Ok(reader.get(address).await?.map_or_else(
        || "nothing".to_owned(),
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    ))
}
