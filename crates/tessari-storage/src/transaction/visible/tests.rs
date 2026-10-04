//! A transaction across leaders that wrote two databases, as a follower holding
//! both applies it one part at a time: no reader sees it in part (ADR-0112
//! D6a).

use tessari_encoding::Decision;
use tessari_types::RecordId;

use super::fixture::{Fixture, read};
use crate::error::{Error, Result};
use crate::transaction::RecordAddress;

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
        matches!(
            refused,
            Err(Error::Conflict {
                with: crate::ConflictWith::Unseen(_),
                ..
            })
        ),
        "{refused:?}"
    );
    let said = refused
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(said.contains("has not all arrived"), "{said}");
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

/// What the coordinator range's leader answers a reader, in a test.
#[derive(Debug)]
struct Answering(Option<tessari_encoding::TransactionRecord>);

impl crate::Decisions for Answering {
    fn decided(
        &self,
        _: tessari_encoding::TransactionId,
        _: tessari_types::Reach,
    ) -> Option<tessari_encoding::TransactionRecord> {
        self.0.clone()
    }
}

/// ADR-0112 D13d: a reader whose copy of the record is not decided asks the
/// record's leader. The caller may already have been told T committed, and a
/// reader here must then see it; one the leader calls undecided, or that has
/// nobody to ask, does not.
#[test]
fn a_reader_asks_the_records_leader_when_its_copy_is_undecided() -> Result<()> {
    let committed = |fixture: &Fixture| tessari_encoding::TransactionRecord {
        decision: Decision::Committed,
        deadline: 0,
        participants: fixture.participants(),
    };
    for (answer, expected) in [
        (Some(Decision::Committed), "new"),
        (Some(Decision::Pending), "old"),
        (None, "old"),
    ] {
        let fixture = Fixture::new()?;
        fixture.prepare(0)?;
        fixture.prepare(1)?;
        fixture.decide(Decision::Pending)?;
        if let Some(decision) = answer {
            let record = tessari_encoding::TransactionRecord {
                decision,
                ..committed(&fixture)
            };
            fixture
                .store
                .answer_decisions_with(std::sync::Arc::new(Answering(Some(record))));
        }
        let reading = fixture.store.begin()?;
        for home in 0..2 {
            assert_eq!(
                read(&reading, &fixture.address(home))?,
                expected,
                "database {home}, the leader answering {answer:?}"
            );
        }
    }
    Ok(())
}
