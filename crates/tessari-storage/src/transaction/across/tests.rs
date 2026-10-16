//! The three records of a transaction across leaders, written on one node
//! through the commit path, with every refusal the decision asks of them.

use crate::now::Now;
use std::sync::Arc;

use tessari_encoding::{
    Decision, LogId, LogKey, LogRecord, Part, Participant, StoreKey, StoreValue,
    TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, TableId};

use crate::catalog::{Catalog, TableShape};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::RecordAddress;

const TRANSACTION: TransactionId = TransactionId::new([6; TRANSACTION_ID_LEN]);

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    /// The log this table's records are filed in.
    log: LogId,
}

impl Fixture {
    fn new() -> Result<Self> {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).now()?;
        let mut transaction = store.begin().now()?;
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns").now()?.id;
        let database = catalog.create_database(namespace, "db").now()?.id;
        let table = catalog
            .create_table(namespace, database, "t", TableShape::default())
            .now()?
            .id;
        transaction.commit().now()?;
        let mut transaction = store.begin().now()?;
        transaction.put(
            RecordAddress::new(namespace, database, table, RecordId::from("r")),
            b"old".to_vec(),
        );
        let log = transaction.commit_placed().now()?.log;
        Ok(Self {
            store,
            namespace,
            database,
            table,
            log,
        })
    }

    fn address(&self) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from("r"),
        )
    }

    fn coordinator(&self) -> Reach {
        Reach::Database(self.namespace, self.database)
    }

    fn seen(&self) -> Result<Sequence> {
        self.store.committed_tail(self.log).now()
    }

    fn prepare(&self, seen: Sequence) -> Result<crate::transaction::Committed> {
        let mut transaction = self.store.begin().now()?;
        transaction.put(self.address(), b"new".to_vec());
        transaction
            .prepare_across(TRANSACTION, self.coordinator(), seen)
            .now()
    }

    fn read(&self) -> Result<Option<Vec<u8>>> {
        self.store.begin().now()?.get(&self.address()).now()
    }

    fn record(&self, decision: Decision) -> TransactionRecord {
        TransactionRecord {
            decision,
            deadline: 0,
            participants: vec![Participant {
                range: self.coordinator(),
                prepared_at: None,
            }],
        }
    }

    fn decide(&self, decision: Decision) -> Result<crate::transaction::Committed> {
        self.store
            .begin()
            .now()?
            .decide_across(TRANSACTION, self.record(decision))
            .now()
    }

    fn resolve(&self, committed: bool) -> Result<Option<crate::transaction::Committed>> {
        self.store
            .begin()
            .now()?
            .resolve_across(TRANSACTION, committed, &[self.address()], &self.prepared())
            .now()
    }

    /// The participants as a committed record names them.
    fn prepared(&self) -> Vec<Participant> {
        vec![Participant {
            range: self.coordinator(),
            prepared_at: Some(Sequence::new(1)),
        }]
    }

    fn logged(&self, at: Sequence) -> Result<LogRecord> {
        let value = self
            .store
            .engine()
            .get(LogKey::keyspace(), &LogKey::new(self.log, at).encode())
            .now()?
            .ok_or(Error::AcrossMalformed {
                part: "test",
                problem: "no record at the position the commit answered",
            })?;
        Ok(LogRecord::decode(value.as_slice())?)
    }
}

#[test]
fn a_prepare_lands_intents_the_log_names_as_its_transactions() -> Result<()> {
    let fixture = Fixture::new()?;
    let prepared = fixture.prepare(fixture.seen()?)?;
    assert_eq!(
        fixture.read()?,
        Some(b"old".to_vec()),
        "an intent is not a value"
    );
    let logged = fixture.logged(prepared.sequence)?;
    assert!(matches!(
        logged.part_of().map(|across| &across.part),
        Some(Part::Prepare { .. })
    ));
    let carried: Vec<bool> = logged
        .mutations()
        .iter()
        .map(|mutation| {
            mutation
                .value
                .provenance()
                .is_some_and(|provenance| provenance.provisional)
        })
        .collect();
    assert_eq!(carried, vec![true]);
    Ok(())
}

#[test]
fn a_prepare_is_refused_for_a_record_written_after_what_its_node_had_seen() -> Result<()> {
    let fixture = Fixture::new()?;
    let seen = fixture.seen()?;
    // Committed on the participant after the transaction's node read position
    // `seen`: only the log walk can see it, since this prepare's own snapshot
    // is newer than the write.
    let mut other = fixture.store.begin().now()?;
    other.put(fixture.address(), b"meanwhile".to_vec());
    other.commit().now()?;
    let refused = fixture.prepare(seen);
    assert!(
        matches!(refused, Err(Error::Conflict { .. })),
        "{refused:?}"
    );
    // Control: from the newer position the same prepare is admitted.
    fixture.prepare(fixture.seen()?)?;
    Ok(())
}

#[test]
fn a_prepare_is_refused_when_the_log_no_longer_reaches_what_was_seen() -> Result<()> {
    let fixture = Fixture::new()?;
    let seen = fixture.seen()?;
    for value in [b"a".to_vec(), b"b".to_vec()] {
        let mut writer = fixture.store.begin().now()?;
        writer.put(
            RecordAddress::new(
                fixture.namespace,
                fixture.database,
                fixture.table,
                RecordId::from("elsewhere"),
            ),
            value,
        );
        writer.commit().now()?;
    }
    fixture
        .store
        .prune_log(fixture.log, Sequence::new(seen.get().saturating_add(1)))
        .now()?;
    let refused = fixture.prepare(seen);
    assert!(
        matches!(refused, Err(Error::AcrossReadTooOld { .. })),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn a_record_takes_one_outcome_and_refuses_another() -> Result<()> {
    let fixture = Fixture::new()?;
    let skipped = fixture.decide(Decision::Committed);
    assert!(
        matches!(skipped, Err(Error::AcrossDecided { decided: "absent" })),
        "a first record must be pending: {skipped:?}"
    );
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.decide(Decision::Committed)?;
    let late = fixture.decide(Decision::Aborted);
    assert!(
        matches!(
            late,
            Err(Error::AcrossDecided {
                decided: "committed"
            })
        ),
        "{late:?}"
    );
    Ok(())
}

#[test]
fn a_resolution_makes_the_value_once_and_then_finds_nothing_left() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(fixture.seen()?)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    assert!(fixture.resolve(true)?.is_some());
    assert_eq!(fixture.read()?, Some(b"new".to_vec()));
    assert!(fixture.resolve(true)?.is_none(), "idempotent");
    // And the version keeps where it came from, resolved.
    let stored = fixture
        .store
        .begin()
        .now()?
        .read_stamped_at(&fixture.address())
        .now()?
        .map(|stored| stored.provenance().cloned());
    assert!(matches!(
        stored,
        Some(Some(provenance)) if !provenance.provisional
            && provenance.transaction == TRANSACTION
            && provenance.participants == fixture.prepared()
    ));
    Ok(())
}

#[test]
fn an_aborted_resolution_leaves_the_old_value_and_frees_the_record() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(fixture.seen()?)?;
    assert!(fixture.resolve(false)?.is_some());
    assert_eq!(fixture.read()?, Some(b"old".to_vec()));
    let mut writer = fixture.store.begin().now()?;
    writer.put(fixture.address(), b"after".to_vec());
    writer.commit().now()?;
    Ok(())
}

#[test]
fn a_committed_resolution_must_say_where_every_prepare_landed() -> Result<()> {
    // Its versions carry the positions a reader decides by (D6a); one written
    // without them would be visible to nobody's satisfaction, or to everyone.
    let fixture = Fixture::new()?;
    fixture.prepare(fixture.seen()?)?;
    let unplaced = [Participant {
        range: fixture.coordinator(),
        prepared_at: None,
    }];
    for participants in [&unplaced[..], &[]] {
        let refused = fixture
            .store
            .begin()
            .now()?
            .resolve_across(TRANSACTION, true, &[fixture.address()], participants)
            .now();
        assert!(
            matches!(refused, Err(Error::AcrossMalformed { .. })),
            "{refused:?}"
        );
    }
    assert_eq!(fixture.read()?, Some(b"old".to_vec()));
    Ok(())
}

#[test]
fn a_record_found_absent_is_aborted_for_good() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.decide(Decision::Aborted)?;
    let late = fixture.decide(Decision::Pending);
    assert!(
        matches!(late, Err(Error::AcrossDecided { decided: "aborted" })),
        "a PENDING delayed past the lapse reopened the record: {late:?}"
    );
    Ok(())
}

#[test]
fn a_participant_resolves_every_intent_it_holds_without_being_told_where() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut transaction = fixture.store.begin().now()?;
    for id in ["r", "s"] {
        transaction.put(
            RecordAddress::new(
                fixture.namespace,
                fixture.database,
                fixture.table,
                RecordId::from(id),
            ),
            b"new".to_vec(),
        );
    }
    transaction
        .prepare_across(TRANSACTION, fixture.coordinator(), fixture.seen()?)
        .now()?;
    assert_eq!(fixture.store.intents_of(TRANSACTION).now()?.len(), 2);
    assert_eq!(
        fixture.store.standing_across().now()?,
        vec![(TRANSACTION, fixture.coordinator())]
    );
    let resolved = fixture
        .store
        .begin()
        .now()?
        .resolve_across(TRANSACTION, true, &[], &fixture.prepared())
        .now()?;
    assert!(resolved.is_some());
    assert_eq!(fixture.read()?, Some(b"new".to_vec()));
    assert!(
        fixture.store.intents_of(TRANSACTION).now()?.is_empty(),
        "the index went with them"
    );
    assert!(fixture.store.standing_across().now()?.is_empty());
    Ok(())
}

#[test]
fn a_pending_record_is_listed_until_it_is_decided() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.decide(Decision::Pending)?;
    assert_eq!(fixture.store.pending_across().now()?.len(), 1);
    fixture.decide(Decision::Aborted)?;
    assert!(fixture.store.pending_across().now()?.is_empty());
    Ok(())
}

mod merged;

mod folding;
