//! A snapshot of a store holding a transaction across leaders restores to what
//! a reader at its version saw (ADR-0112 D9a): whole or absent, and the log
//! above the cut applies on top of it as it did on the source.

use std::sync::Arc;

use tessari_encoding::{Decision, Part, TransactionRecord};
use tessari_kv::{KvBackend, MemoryBackend};

use super::fixture::{Fixture, read};
use crate::error::Result;
use crate::store::Store;

/// The store `source` is restored to from a snapshot of it taken now, with
/// what a reader at that snapshot's version saw of each home.
async fn restore(fixture: &Fixture) -> Result<(Store, [String; 2])> {
    let onto = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).await?;
    let seen = restore_onto(fixture, &onto).await?;
    Ok((onto, seen))
}

/// [`restore`] onto a store that may already hold some of it, as a follower
/// copying its leader's state again does.
async fn restore_onto(fixture: &Fixture, onto: &Store) -> Result<[String; 2]> {
    let mut reader = fixture.store.read_state().await?;
    let at = fixture.store.begin_at(reader.version()).await?;
    let seen = [
        read(&at, &fixture.address(0)).await?,
        read(&at, &fixture.address(1)).await?,
    ];
    while let Some(chunk) = reader.next_chunk(64).await? {
        onto.restore_state_chunk(&chunk).await?;
    }
    onto.finish_state(reader.positions(), &reader.topic_heads().await?)
        .await?;
    Ok(seen)
}

async fn reads(fixture: &Fixture, store: &Store) -> Result<[String; 2]> {
    let reading = store.begin().await?;
    Ok([
        read(&reading, &fixture.address(0)).await?,
        read(&reading, &fixture.address(1)).await?,
    ])
}

#[tokio::test]
async fn every_cut_of_the_protocol_restores_to_what_a_reader_at_it_saw() -> Result<()> {
    let fixture = Fixture::new().await?;
    for step in 0..7 {
        match step {
            0 => fixture.prepare(0).await,
            1 => fixture.decide(Decision::Pending).await,
            2 => fixture.decide(Decision::Committed).await,
            3 => fixture.resolve(0).await,
            4 => fixture.prepare(1).await,
            5 => fixture.resolve(1).await,
            _ => fixture.forget().await,
        }?;
        let (restored, seen) = restore(&fixture).await?;
        assert_eq!(reads(&fixture, &restored).await?, seen, "after step {step}");
    }
    // The last cut is the committed transaction, whole.
    let (restored, _) = restore(&fixture).await?;
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    Ok(())
}

#[tokio::test]
async fn a_transaction_part_way_at_the_cut_is_finished_by_the_log_above_it() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.prepare(0).await?;
    fixture.decide(Decision::Pending).await?;
    fixture.decide(Decision::Committed).await?;
    fixture.resolve(0).await?;
    let (restored, seen) = restore(&fixture).await?;
    assert_eq!(seen, ["old", "old"]);
    assert_eq!(reads(&fixture, &restored).await?, ["old", "old"]);
    // The first home's index holds the resolution its readers do not see yet,
    // so it is not believed there, as on the source (D6b).
    let table = fixture.homes[0].table;
    assert!(fixture.store.across_unsettled(table).await?);
    assert!(restored.across_unsettled(table).await?);
    // The second part arrives as it would have on the source.
    fixture.prepare_on(&restored, 1).await?;
    fixture.resolve_on(&restored, 1).await?;
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    assert!(restored.standing_across().await?.is_empty());
    assert!(!restored.across_unsettled(table).await?);
    Ok(())
}

#[tokio::test]
async fn an_intent_restores_as_an_intent_its_resolution_can_remove() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.prepare(0).await?;
    fixture.prepare(1).await?;
    fixture.decide(Decision::Pending).await?;
    fixture.decide(Decision::Committed).await?;
    let (restored, seen) = restore(&fixture).await?;
    assert_eq!(seen, ["new", "new"]);
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    assert_eq!(restored.standing_across().await?.len(), 1);
    fixture.resolve_on(&restored, 0).await?;
    fixture.resolve_on(&restored, 1).await?;
    assert!(restored.standing_across().await?.is_empty());
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    Ok(())
}

#[tokio::test]
async fn a_state_copied_again_onto_a_store_holding_it_changes_nothing() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.prepare(0).await?;
    fixture.prepare(1).await?;
    fixture.decide(Decision::Pending).await?;
    fixture.decide(Decision::Committed).await?;
    let (restored, _) = restore(&fixture).await?;
    let reading = restored.begin().await?;
    // Copied again over itself, as a follower re-seeding from its leader is:
    // the decided record is not decided twice, each intent stands once, and a
    // reader that began before still sees the transaction.
    restore_onto(&fixture, &restored).await?;
    assert_eq!(read(&reading, &fixture.address(1)).await?, "new");
    drop(reading);
    assert_eq!(reads(&fixture, &restored).await?, ["new", "new"]);
    fixture.resolve_on(&restored, 0).await?;
    fixture.resolve_on(&restored, 1).await?;
    assert!(restored.standing_across().await?.is_empty());
    // Nothing of the transaction is left to refuse a later write.
    let mut writing = restored.begin().await?;
    for home in 0..2 {
        writing.put(fixture.address(home), b"later".to_vec());
    }
    writing.commit().await?;
    assert_eq!(reads(&fixture, &restored).await?, ["later", "later"]);
    Ok(())
}

#[tokio::test]
async fn an_aborted_intent_copied_again_leaves_nothing_to_refuse_a_write() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.prepare(0).await?;
    fixture.decide(Decision::Pending).await?;
    let (restored, _) = restore(&fixture).await?;
    restore_onto(&fixture, &restored).await?;
    // The transaction aborts after the copies, and its intent is dropped.
    fixture
        .apply_on(
            &restored,
            0,
            Part::Decide(TransactionRecord {
                decision: Decision::Aborted,
                deadline: 0,
                participants: fixture.participants(),
            }),
            Vec::new(),
        )
        .await?;
    fixture
        .apply_on(
            &restored,
            0,
            Part::Resolve { committed: false },
            vec![fixture.write(0, true)],
        )
        .await?;
    assert_eq!(reads(&fixture, &restored).await?, ["old", "old"]);
    let mut writing = restored.begin().await?;
    writing.put(fixture.address(0), b"later".to_vec());
    writing.commit().await?;
    assert_eq!(reads(&fixture, &restored).await?, ["later", "old"]);
    Ok(())
}
