//! Settled part markers forgotten with reclamation (ADR-0112 D6a, Q-922): once
//! no read can name a sequence below a settled transaction's newest marker, its
//! versions drop their provenance and its markers go — so markers are bounded
//! exactly as version history is.

use tessari_encoding::{
    AcrossPartKey, Decision, KeyKind, RecordKey, StampedValue, StoreKey, StoreValue,
    TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};

use super::Fixture;
use crate::error::Result;

impl Fixture {
    /// One transaction across leaders, prepared, committed and resolved here.
    fn settled(&self, round: u8) -> Result<TransactionId> {
        let transaction = TransactionId::new([round; TRANSACTION_ID_LEN]);
        let mut writing = self.store.begin()?;
        writing.put(self.address(), vec![round]);
        writing.prepare_across(transaction, self.coordinator(), self.seen()?)?;
        for decision in [Decision::Pending, Decision::Committed] {
            self.store.begin()?.decide_across(
                transaction,
                TransactionRecord {
                    participants: self.prepared(),
                    ..self.record(decision)
                },
            )?;
        }
        self.store.begin()?.resolve_across(
            transaction,
            true,
            &[self.address()],
            &self.prepared(),
        )?;
        Ok(transaction)
    }

    /// How many entries of `kind` this store holds.
    fn count(&self, kind: KeyKind) -> Result<usize> {
        Ok(self
            .store
            .backend()
            .scan(&ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::prefix(&[kind.tag()]),
                direction: ScanDirection::Forward,
                limit: None,
            })?
            .len())
    }

    /// Whether any stored version of the table still names a transaction
    /// across leaders.
    fn names_a_transaction(&self) -> Result<bool> {
        let stored = self.store.backend().scan(&ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&RecordKey::table_prefix(
                self.namespace,
                self.database,
                self.table,
            )),
            direction: ScanDirection::Forward,
            limit: None,
        })?;
        for (_, value) in stored {
            if StampedValue::decode(value.as_slice())?
                .provenance()
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

const ROUNDS: u8 = 5;

#[test]
fn reclaiming_past_settled_transactions_forgets_their_markers() -> Result<()> {
    let fixture = Fixture::new()?;
    for round in 1..=ROUNDS {
        fixture.settled(round)?;
    }
    assert_eq!(fixture.count(AcrossPartKey::KIND)?, usize::from(ROUNDS));
    assert_eq!(fixture.count(KeyKind::ResolvedOf)?, usize::from(ROUNDS));
    assert!(fixture.names_a_transaction()?);
    let before = fixture.read()?;

    fixture
        .store
        .reclaim_table(fixture.namespace, fixture.database, fixture.table)?;

    assert_eq!(
        fixture.count(AcrossPartKey::KIND)?,
        0,
        "settled markers forgotten"
    );
    assert_eq!(fixture.count(KeyKind::ResolvedOf)?, 0);
    assert!(
        !fixture.names_a_transaction()?,
        "the surviving versions dropped their provenance with their markers"
    );
    assert_eq!(
        fixture.read()?,
        before,
        "and every reader reads what it read"
    );
    Ok(())
}

#[test]
fn a_reader_still_below_a_marker_keeps_it() -> Result<()> {
    let fixture = Fixture::new()?;
    for round in 1..=3 {
        fixture.settled(round)?;
    }
    // A reader that began here holds the floor below the last two.
    let reading = fixture.store.begin()?;
    let seen = reading.get(&fixture.address())?;
    for round in 4..=5 {
        fixture.settled(round)?;
    }
    fixture
        .store
        .reclaim_table(fixture.namespace, fixture.database, fixture.table)?;
    assert_eq!(
        fixture.count(AcrossPartKey::KIND)?,
        2,
        "the markers above the reader's snapshot stay"
    );
    assert_eq!(reading.get(&fixture.address())?, seen);
    assert_eq!(seen, Some(vec![3]));
    Ok(())
}
