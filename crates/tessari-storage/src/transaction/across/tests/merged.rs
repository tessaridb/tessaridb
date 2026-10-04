//! The coordinator's range written in two commits rather than four (ADR-0112
//! D13a, D13b): begun with its own writes, concluded with their resolution.

use tessari_encoding::{Decision, Part, TransactionRecord};
use tessari_types::Sequence;

use super::{Fixture, TRANSACTION};
use crate::error::{Error, Result};
use crate::transaction::Committed;

impl Fixture {
    fn begin(&self, seen: Sequence) -> Result<Committed> {
        let mut transaction = self.store.begin()?;
        transaction.put(self.address(), b"new".to_vec());
        transaction.begin_across(TRANSACTION, self.record(Decision::Staging), seen)
    }

    fn conclude(&self, decided: TransactionRecord) -> Result<Committed> {
        self.store
            .begin()?
            .conclude_across(TRANSACTION, decided, &[self.address()])
    }

    fn concluded(&self, decision: Decision) -> TransactionRecord {
        TransactionRecord {
            participants: self.prepared(),
            ..self.record(decision)
        }
    }
}

#[test]
fn a_begin_writes_the_record_and_the_intents_in_one_log_record() -> Result<()> {
    let fixture = Fixture::new()?;
    let begun = fixture.begin(fixture.seen()?)?;
    let logged = fixture.logged(begun.sequence)?;
    assert_eq!(
        logged.part_of().map(|across| &across.part),
        Some(&Part::Begin(fixture.record(Decision::Staging)))
    );
    assert!(logged.mutations().iter().all(|mutation| {
        mutation
            .value
            .provenance()
            .is_some_and(|provenance| provenance.provisional)
    }));
    assert_eq!(
        fixture.store.transaction_record(TRANSACTION)?,
        Some(fixture.record(Decision::Staging))
    );
    assert_eq!(fixture.read()?, Some(b"old".to_vec()));
    Ok(())
}

#[test]
fn a_begin_is_refused_for_a_record_written_after_what_its_node_had_seen() -> Result<()> {
    let fixture = Fixture::new()?;
    let seen = fixture.seen()?;
    let mut other = fixture.store.begin()?;
    other.put(fixture.address(), b"meanwhile".to_vec());
    other.commit()?;
    let refused = fixture.begin(seen);
    assert!(
        matches!(refused, Err(Error::Conflict { .. })),
        "{refused:?}"
    );
    assert_eq!(fixture.store.transaction_record(TRANSACTION)?, None);
    // Control: from the newer position the same begin is admitted.
    fixture.begin(fixture.seen()?)?;
    Ok(())
}

#[test]
fn a_begin_after_its_absent_record_was_aborted_is_refused() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.decide(Decision::Aborted)?;
    let refused = fixture.begin(fixture.seen()?);
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "aborted" })),
        "{refused:?}"
    );
    assert!(fixture.store.intents_of(TRANSACTION)?.is_empty());
    Ok(())
}

#[test]
fn a_conclude_decides_and_resolves_in_one_log_record() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.begin(fixture.seen()?)?;
    let concluded = fixture.conclude(fixture.concluded(Decision::Committed))?;
    let logged = fixture.logged(concluded.sequence)?;
    assert_eq!(
        logged.part_of().map(|across| &across.part),
        Some(&Part::Conclude(fixture.concluded(Decision::Committed)))
    );
    let carried: Vec<_> = logged
        .mutations()
        .iter()
        .filter_map(|mutation| mutation.value.provenance())
        .map(|provenance| (provenance.provisional, provenance.participants.clone()))
        .collect();
    assert_eq!(carried, vec![(false, fixture.prepared())]);
    assert_eq!(
        fixture.store.transaction_record(TRANSACTION)?,
        Some(fixture.concluded(Decision::Committed))
    );
    assert_eq!(fixture.read()?, Some(b"new".to_vec()));
    assert!(fixture.store.intents_of(TRANSACTION)?.is_empty());
    Ok(())
}

#[test]
fn a_conclude_that_aborts_frees_the_record() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.begin(fixture.seen()?)?;
    fixture.conclude(fixture.record(Decision::Aborted))?;
    assert_eq!(fixture.read()?, Some(b"old".to_vec()));
    let mut next = fixture.store.begin()?;
    next.put(fixture.address(), b"later".to_vec());
    next.commit()?;
    assert_eq!(fixture.read()?, Some(b"later".to_vec()));
    Ok(())
}

#[test]
fn a_committed_conclude_must_say_where_every_prepare_landed() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.begin(fixture.seen()?)?;
    let refused = fixture.conclude(fixture.record(Decision::Committed));
    assert!(
        matches!(
            refused,
            Err(Error::AcrossMalformed {
                part: "conclude",
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(fixture.read()?, Some(b"old".to_vec()));
    Ok(())
}

#[test]
fn a_follower_applies_the_leaders_begin_and_conclusion_as_written() -> Result<()> {
    let leader = Fixture::new()?;
    let begun = leader.begin(leader.seen()?)?;
    let concluded = leader.conclude(leader.concluded(Decision::Committed))?;
    let follower = Fixture::new()?;
    for at in [begun.sequence, concluded.sequence] {
        follower
            .store
            .apply_record_in(follower.log, at, &leader.logged(at)?)?;
    }
    assert_eq!(follower.read()?, Some(b"new".to_vec()));
    assert_eq!(
        follower.store.transaction_record(TRANSACTION)?,
        Some(leader.concluded(Decision::Committed))
    );
    assert!(follower.store.intents_of(TRANSACTION)?.is_empty());
    Ok(())
}

/// A record's leader that counts every time it is asked, answering committed.
#[derive(Debug)]
struct Counting {
    asked: std::sync::atomic::AtomicUsize,
    record: TransactionRecord,
}

impl crate::Decisions for Counting {
    fn decided(
        &self,
        _: tessari_encoding::TransactionId,
        _: tessari_types::Reach,
    ) -> Option<TransactionRecord> {
        self.asked
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(self.record.clone())
    }
}

#[test]
fn applying_a_conclusion_never_asks_the_records_leader() -> Result<()> {
    // What a commit or an apply derives is derived from this node's own copy:
    // asking a peer there would wait on the network under the write turn —
    // while that peer may be waiting for this very apply — and would derive
    // from a value this copy never held (ADR-0112 D13d is for readers).
    let leader = Fixture::new()?;
    let begun = leader.begin(leader.seen()?)?;
    let concluded = leader.conclude(leader.concluded(Decision::Committed))?;
    let follower = Fixture::new()?;
    let counting = std::sync::Arc::new(Counting {
        asked: std::sync::atomic::AtomicUsize::new(0),
        record: leader.concluded(Decision::Committed),
    });
    follower
        .store
        .answer_decisions_with(std::sync::Arc::clone(&counting) as _);
    for at in [begun.sequence, concluded.sequence] {
        follower
            .store
            .apply_record_in(follower.log, at, &leader.logged(at)?)?;
    }
    assert_eq!(
        counting.asked.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "an apply asked the record's leader"
    );
    assert_eq!(follower.read()?, Some(b"new".to_vec()));
    Ok(())
}

#[test]
fn a_bar_lands_in_the_range_and_refuses_the_prepare_after_it() -> Result<()> {
    // Status recovery barring a part whose prepare has not landed (ADR-0112
    // D14c): one log record in that range, and the prepare refused for good.
    let fixture = Fixture::new()?;
    let seen = fixture.seen()?;
    let barred = fixture
        .store
        .begin()?
        .prevent_across(TRANSACTION, fixture.coordinator())?;
    let logged = fixture.logged(barred.sequence)?;
    assert_eq!(
        logged.part_of().map(|across| &across.part),
        Some(&Part::Prevent {
            range: fixture.coordinator()
        })
    );
    assert!(logged.mutations().is_empty());
    let refused = fixture.prepare(seen);
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "barred" })),
        "{refused:?}"
    );
    assert_eq!(fixture.read()?, Some(b"old".to_vec()));
    Ok(())
}
