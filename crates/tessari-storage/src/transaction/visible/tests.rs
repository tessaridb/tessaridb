//! A transaction across leaders that wrote two databases, as a follower holding
//! both applies it one part at a time: no reader sees it in part (ADR-0112
//! D6a).

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

const TRANSACTION: TransactionId = TransactionId::new([6; TRANSACTION_ID_LEN]);

/// One database the transaction writes: its table, and the log a follower
/// holds its range's records in.
struct Home {
    database: DatabaseId,
    table: TableId,
    log: LogId,
    at: std::cell::Cell<u64>,
}

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    /// The coordinator's range first, as the record names it.
    homes: [Home; 2],
}

impl Fixture {
    /// Two databases, each with a table holding `r = "old"`.
    fn new() -> Result<Self> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)?;
        let mut transaction = store.begin()?;
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns")?.id;
        let mut homes = Vec::new();
        for name in ["one", "two"] {
            let database = catalog.create_database(namespace, name)?.id;
            let table = catalog
                .create_table(namespace, database, "t", TableShape::default())?
                .id;
            homes.push(Home {
                database,
                table,
                log: LogId::line(Reach::Database(namespace, database)),
                at: std::cell::Cell::new(0),
            });
        }
        transaction.commit()?;
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
        writing.commit()?;
        Ok(fixture)
    }

    fn range(&self, home: usize) -> Reach {
        Reach::Database(self.namespace, self.homes[home].database)
    }

    fn address(&self, home: usize) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.homes[home].database,
            self.homes[home].table,
            RecordId::from("r"),
        )
    }

    fn participants(&self) -> Vec<Participant> {
        (0..2)
            .map(|home| Participant {
                range: self.range(home),
                prepared_at: Some(Sequence::new(1)),
            })
            .collect()
    }

    /// `r = "new"` in `home`, as an intent or as resolved.
    fn write(&self, home: usize, provisional: bool) -> Mutation {
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
    fn apply(&self, home: usize, part: Part, mutations: Vec<Mutation>) -> Result<()> {
        let record = LogRecord::new(mutations).across(Across {
            transaction: TRANSACTION,
            part,
        });
        let at = self.homes[home].at.get().saturating_add(1);
        self.store
            .apply_record_in(self.homes[home].log, Sequence::new(at), &record)?;
        self.homes[home].at.set(at);
        Ok(())
    }

    fn prepare(&self, home: usize) -> Result<()> {
        let coordinator = self.range(0);
        let write = self.write(home, true);
        self.apply(home, Part::Prepare { coordinator }, vec![write])
    }

    fn decide(&self, decision: Decision) -> Result<()> {
        let record = TransactionRecord {
            decision,
            deadline: 0,
            participants: self.participants(),
        };
        self.apply(0, Part::Decide(record), Vec::new())
    }

    fn resolve(&self, home: usize) -> Result<()> {
        let write = self.write(home, false);
        self.apply(home, Part::Resolve { committed: true }, vec![write])
    }
}

fn read(reader: &crate::transaction::Transaction<'_>, address: &RecordAddress) -> Result<String> {
    Ok(reader.get(address)?.map_or_else(
        || "nothing".to_owned(),
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

#[test]
fn a_transaction_is_seen_only_once_this_node_holds_every_part_of_it() -> Result<()> {
    let fixture = Fixture::new()?;
    // The first database's part has arrived whole — prepared, committed and
    // resolved — and the second's prepare has not.
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.resolve(0)?;
    let early = fixture.store.begin()?;
    for home in 0..2 {
        assert_eq!(
            read(&early, &fixture.address(home))?,
            "old",
            "database {home} shows part of a transaction this node holds half of"
        );
    }
    // The second part lands. A snapshot from before it keeps its answer; one
    // after it sees the whole transaction — the second database's value is
    // still an intent, which its committed record makes a value.
    fixture.prepare(1)?;
    for home in 0..2 {
        assert_eq!(read(&early, &fixture.address(home))?, "old");
    }
    let late = fixture.store.begin()?;
    for home in 0..2 {
        assert_eq!(read(&late, &fixture.address(home))?, "new");
    }
    Ok(())
}

#[test]
fn a_scan_shows_neither_part_while_one_is_missing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.resolve(0)?;
    let reading = fixture.store.begin()?;
    let scanned = reading.first_records_of(
        fixture.namespace,
        fixture.homes[0].database,
        fixture.homes[0].table,
        10,
    )?;
    assert_eq!(
        scanned
            .into_iter()
            .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<Vec<_>>(),
        ["old"]
    );
    Ok(())
}

#[test]
fn a_writer_may_not_build_on_a_version_it_could_not_see() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.resolve(0)?;
    // It read "old" where the transaction wrote "new"; writing over that would
    // lose the transaction's write.
    let mut writer = fixture.store.begin()?;
    assert_eq!(read(&writer, &fixture.address(0))?, "old");
    writer.put(fixture.address(0), b"old+1".to_vec());
    let refused = writer.commit();
    assert!(
        matches!(refused, Err(Error::Conflict { .. })),
        "{refused:?}"
    );
    // Once the node holds the whole transaction, the same write goes through.
    fixture.prepare(1)?;
    let mut writer = fixture.store.begin()?;
    assert_eq!(read(&writer, &fixture.address(0))?, "new");
    writer.put(fixture.address(0), b"new+1".to_vec());
    writer.commit()?;
    Ok(())
}

#[test]
fn a_reader_keeps_its_answer_when_the_record_commits_under_it() -> Result<()> {
    // Every part is here and the record is still PENDING when the reader
    // meets the first intent; the decision lands before it reads the second.
    // Asked again, the record would say COMMITTED and show the second write
    // after hiding the first — half the transaction in one reader.
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    fixture.decide(Decision::Pending)?;
    let reading = fixture.store.begin()?;
    assert_eq!(read(&reading, &fixture.address(0))?, "old");
    fixture.decide(Decision::Committed)?;
    assert_eq!(read(&reading, &fixture.address(1))?, "old");
    assert_eq!(read(&fixture.store.begin()?, &fixture.address(1))?, "new");
    Ok(())
}

#[test]
fn reclaiming_keeps_the_version_under_one_a_reader_may_pass_over() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.resolve(0)?;
    // Move the floor past the resolved version, so reclamation reaches the
    // one under it, which every reader here still needs (ADR-0112 D9).
    let mut writer = fixture.store.begin()?;
    writer.put(
        RecordAddress::new(
            fixture.namespace,
            fixture.homes[1].database,
            fixture.homes[1].table,
            RecordId::from("later"),
        ),
        b"y".to_vec(),
    );
    writer.commit()?;
    fixture.store.reclaim_table(
        fixture.namespace,
        fixture.homes[0].database,
        fixture.homes[0].table,
    )?;
    assert_eq!(read(&fixture.store.begin()?, &fixture.address(0))?, "old");
    Ok(())
}
