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
async fn restored(fixture: &Fixture) -> Result<Store> {
    let onto = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).await?;
    let mut reader = fixture.store.read_state().await?;
    while let Some(chunk) = reader.next_chunk(64).await? {
        onto.restore_state_chunk(&chunk).await?;
    }
    onto.finish_state(reader.positions(), &reader.topic_heads().await?)
        .await?;
    onto.settle_restored().await?;
    Ok(onto)
}

async fn reads(fixture: &Fixture, store: &Store) -> Result<[String; 2]> {
    let reading = store.begin().await?;
    Ok([
        read(&reading, &fixture.address(0)).await?,
        read(&reading, &fixture.address(1)).await?,
    ])
}

async fn decision(store: &Store) -> Result<Option<Decision>> {
    Ok(store
        .transaction_record(TRANSACTION)
        .await?
        .map(|record| record.decision))
}

/// Both parts prepared, the record decided as `decision`.
async fn both_prepared(decision: Decision) -> Result<Fixture> {
    let fixture = Fixture::new().await?;
    fixture.prepare(0).await?;
    fixture.prepare(1).await?;
    if decision != Decision::Staging {
        fixture.decide(Decision::Pending).await?;
    }
    fixture.decide(decision).await?;
    Ok(fixture)
}

#[tokio::test]
async fn a_committed_record_leaves_no_intent_and_its_log_still_applies_on_top() -> Result<()> {
    let fixture = both_prepared(Decision::Committed).await?;
    let restored = restored(&fixture).await?;
    assert!(restored.standing_across().await?.is_empty());
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    // The source's own resolutions and forgetting arrive later, onto the same
    // positions, and change nothing a reader sees.
    fixture.resolve_on(&restored, 0).await?;
    fixture.resolve_on(&restored, 1).await?;
    fixture
        .apply_on(
            &restored,
            0,
            Part::Forget {
                coordinator: fixture.range(0),
            },
            Vec::new(),
        )
        .await?;
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    assert!(restored.standing_across().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_staging_record_holding_every_part_restores_committed() -> Result<()> {
    let fixture = both_prepared(Decision::Staging).await?;
    let restored = restored(&fixture).await?;
    // Committed implicitly the moment its last part landed (D14a): the caller
    // was told so, and the restore says the same (D14f).
    assert!(restored.standing_across().await?.is_empty());
    assert_eq!(decision(&restored).await?, Some(Decision::Committed));
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    // The source's conclusion arrives later and agrees.
    fixture
        .apply_on(
            &restored,
            0,
            Part::Decide(TransactionRecord {
                decision: Decision::Committed,
                deadline: 0,
                participants: fixture.participants(),
            }),
            Vec::new(),
        )
        .await?;
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    Ok(())
}

#[tokio::test]
async fn a_resolved_version_is_enough_when_the_record_is_gone() -> Result<()> {
    let fixture = both_prepared(Decision::Committed).await?;
    fixture.resolve(0).await?;
    fixture.forget().await?;
    let restored = restored(&fixture).await?;
    assert_eq!(decision(&restored).await?, None);
    assert!(restored.standing_across().await?.is_empty());
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    Ok(())
}

#[tokio::test]
async fn an_aborted_record_drops_its_intents() -> Result<()> {
    let fixture = both_prepared(Decision::Aborted).await?;
    let restored = restored(&fixture).await?;
    assert!(restored.standing_across().await?.is_empty());
    assert_eq!(reads(&fixture, &restored).await?, ["old", "old"]);
    let mut writing = restored.begin().await?;
    for home in 0..2 {
        writing.put(fixture.address(home), b"later".to_vec());
    }
    writing.commit().await?;
    assert_eq!(reads(&fixture, &restored).await?, ["later", "later"]);
    Ok(())
}

#[tokio::test]
async fn a_staging_record_missing_a_part_stays_for_the_log_above() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.prepare(0).await?;
    fixture.decide(Decision::Staging).await?;
    let restored = restored(&fixture).await?;
    // The missing prepare may still land on the source: nothing is decided,
    // nothing is barred, and nothing is visible.
    assert_eq!(restored.standing_across().await?.len(), 1);
    assert_eq!(decision(&restored).await?, Some(Decision::Staging));
    assert_eq!(reads(&fixture, &restored).await?, ["old", "old"]);
    fixture.prepare_on(&restored, 1).await?;
    fixture
        .apply_on(
            &restored,
            0,
            Part::Decide(TransactionRecord {
                decision: Decision::Committed,
                deadline: 0,
                participants: fixture.participants(),
            }),
            Vec::new(),
        )
        .await?;
    fixture.resolve_on(&restored, 0).await?;
    fixture.resolve_on(&restored, 1).await?;
    assert!(restored.standing_across().await?.is_empty());
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    Ok(())
}

#[tokio::test]
async fn a_pending_or_absent_record_stays_for_the_log_above() -> Result<()> {
    let absent = Fixture::new().await?;
    absent.prepare(0).await?;
    let restored_absent = restored(&absent).await?;
    assert_eq!(restored_absent.standing_across().await?.len(), 1);
    assert_eq!(decision(&restored_absent).await?, None);
    assert_eq!(reads(&absent, &restored_absent).await?, ["old", "old"]);

    let pending = Fixture::new().await?;
    pending.prepare(0).await?;
    pending.decide(Decision::Pending).await?;
    let restored = restored(&pending).await?;
    assert_eq!(restored.standing_across().await?.len(), 1);
    assert_eq!(decision(&restored).await?, Some(Decision::Pending));
    assert_eq!(reads(&pending, &restored).await?, ["old", "old"]);
    // The source later commits it, and its log applies as it would have.
    pending.prepare_on(&restored, 1).await?;
    pending
        .apply_on(
            &restored,
            0,
            Part::Decide(TransactionRecord {
                decision: Decision::Committed,
                deadline: 0,
                participants: pending.participants(),
            }),
            Vec::new(),
        )
        .await?;
    pending.resolve_on(&restored, 0).await?;
    pending.resolve_on(&restored, 1).await?;
    assert_eq!(reads(&pending, &restored).await?, ["new", "new"]);
    Ok(())
}

#[tokio::test]
async fn settling_again_changes_nothing() -> Result<()> {
    let fixture = both_prepared(Decision::Staging).await?;
    let restored = restored(&fixture).await?;
    let version = restored.committed_version().await?;
    assert_eq!(restored.settle_restored().await?, 0);
    assert_eq!(restored.committed_version().await?, version);
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    Ok(())
}
