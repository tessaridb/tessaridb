//! A standalone restore settles every transaction across leaders whose outcome
//! its cut fixes, and leaves the rest for the log above it (ADR-0112 D9a,
//! D14f, Q-922b).

use std::sync::Arc;

use tessari_encoding::{Decision, Part, TransactionRecord};
use tessari_kv::{KvBackend, MemoryBackend};

use super::fixture::{Fixture, TRANSACTION, read};
use crate::error::Result;
use crate::store::Store;

/// The store a snapshot of `fixture` taken now restores to, settled as a
/// standalone restore settles it.
fn restored(fixture: &Fixture) -> Result<Store> {
    let onto = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)?;
    let mut reader = fixture.store.read_state()?;
    while let Some(chunk) = reader.next_chunk(64)? {
        onto.restore_state_chunk(&chunk)?;
    }
    onto.finish_state(reader.positions(), &reader.topic_heads()?)?;
    onto.settle_restored()?;
    Ok(onto)
}

fn reads(fixture: &Fixture, store: &Store) -> Result<[String; 2]> {
    let reading = store.begin()?;
    Ok([
        read(&reading, &fixture.address(0))?,
        read(&reading, &fixture.address(1))?,
    ])
}

fn decision(store: &Store) -> Result<Option<Decision>> {
    Ok(store
        .transaction_record(TRANSACTION)?
        .map(|record| record.decision))
}

/// Both parts prepared, the record decided as `decision`.
fn both_prepared(decision: Decision) -> Result<Fixture> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    if decision != Decision::Staging {
        fixture.decide(Decision::Pending)?;
    }
    fixture.decide(decision)?;
    Ok(fixture)
}

#[test]
fn a_committed_record_leaves_no_intent_and_its_log_still_applies_on_top() -> Result<()> {
    let fixture = both_prepared(Decision::Committed)?;
    let restored = restored(&fixture)?;
    assert!(restored.standing_across()?.is_empty());
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    // The source's own resolutions and forgetting arrive later, onto the same
    // positions, and change nothing a reader sees.
    fixture.resolve_on(&restored, 0)?;
    fixture.resolve_on(&restored, 1)?;
    fixture.apply_on(
        &restored,
        0,
        Part::Forget {
            coordinator: fixture.range(0),
        },
        Vec::new(),
    )?;
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    assert!(restored.standing_across()?.is_empty());
    Ok(())
}

#[test]
fn a_staging_record_holding_every_part_restores_committed() -> Result<()> {
    let fixture = both_prepared(Decision::Staging)?;
    let restored = restored(&fixture)?;
    // Committed implicitly the moment its last part landed (D14a): the caller
    // was told so, and the restore says the same (D14f).
    assert!(restored.standing_across()?.is_empty());
    assert_eq!(decision(&restored)?, Some(Decision::Committed));
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    // The source's conclusion arrives later and agrees.
    fixture.apply_on(
        &restored,
        0,
        Part::Decide(TransactionRecord {
            decision: Decision::Committed,
            deadline: 0,
            participants: fixture.participants(),
        }),
        Vec::new(),
    )?;
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    Ok(())
}

#[test]
fn a_resolved_version_is_enough_when_the_record_is_gone() -> Result<()> {
    let fixture = both_prepared(Decision::Committed)?;
    fixture.resolve(0)?;
    fixture.forget()?;
    let restored = restored(&fixture)?;
    assert_eq!(decision(&restored)?, None);
    assert!(restored.standing_across()?.is_empty());
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    Ok(())
}

#[test]
fn an_aborted_record_drops_its_intents() -> Result<()> {
    let fixture = both_prepared(Decision::Aborted)?;
    let restored = restored(&fixture)?;
    assert!(restored.standing_across()?.is_empty());
    assert_eq!(reads(&fixture, &restored)?, ["old", "old"]);
    let mut writing = restored.begin()?;
    for home in 0..2 {
        writing.put(fixture.address(home), b"later".to_vec());
    }
    writing.commit()?;
    assert_eq!(reads(&fixture, &restored)?, ["later", "later"]);
    Ok(())
}

#[test]
fn a_staging_record_missing_a_part_stays_for_the_log_above() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Staging)?;
    let restored = restored(&fixture)?;
    // The missing prepare may still land on the source: nothing is decided,
    // nothing is barred, and nothing is visible.
    assert_eq!(restored.standing_across()?.len(), 1);
    assert_eq!(decision(&restored)?, Some(Decision::Staging));
    assert_eq!(reads(&fixture, &restored)?, ["old", "old"]);
    fixture.prepare_on(&restored, 1)?;
    fixture.apply_on(
        &restored,
        0,
        Part::Decide(TransactionRecord {
            decision: Decision::Committed,
            deadline: 0,
            participants: fixture.participants(),
        }),
        Vec::new(),
    )?;
    fixture.resolve_on(&restored, 0)?;
    fixture.resolve_on(&restored, 1)?;
    assert!(restored.standing_across()?.is_empty());
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    Ok(())
}

#[test]
fn a_pending_or_absent_record_stays_for_the_log_above() -> Result<()> {
    let absent = Fixture::new()?;
    absent.prepare(0)?;
    let restored_absent = restored(&absent)?;
    assert_eq!(restored_absent.standing_across()?.len(), 1);
    assert_eq!(decision(&restored_absent)?, None);
    assert_eq!(reads(&absent, &restored_absent)?, ["old", "old"]);

    let pending = Fixture::new()?;
    pending.prepare(0)?;
    pending.decide(Decision::Pending)?;
    let restored = restored(&pending)?;
    assert_eq!(restored.standing_across()?.len(), 1);
    assert_eq!(decision(&restored)?, Some(Decision::Pending));
    assert_eq!(reads(&pending, &restored)?, ["old", "old"]);
    // The source later commits it, and its log applies as it would have.
    pending.prepare_on(&restored, 1)?;
    pending.apply_on(
        &restored,
        0,
        Part::Decide(TransactionRecord {
            decision: Decision::Committed,
            deadline: 0,
            participants: pending.participants(),
        }),
        Vec::new(),
    )?;
    pending.resolve_on(&restored, 0)?;
    pending.resolve_on(&restored, 1)?;
    assert_eq!(reads(&pending, &restored)?, ["new", "new"]);
    Ok(())
}

#[test]
fn settling_again_changes_nothing() -> Result<()> {
    let fixture = both_prepared(Decision::Staging)?;
    let restored = restored(&fixture)?;
    let version = restored.committed_version()?;
    assert_eq!(restored.settle_restored()?, 0);
    assert_eq!(restored.committed_version()?, version);
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    Ok(())
}
