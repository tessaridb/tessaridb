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
fn restore(fixture: &Fixture) -> Result<(Store, [String; 2])> {
    let onto = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)?;
    let seen = restore_onto(fixture, &onto)?;
    Ok((onto, seen))
}

/// [`restore`] onto a store that may already hold some of it, as a follower
/// copying its leader's state again does.
fn restore_onto(fixture: &Fixture, onto: &Store) -> Result<[String; 2]> {
    let mut reader = fixture.store.read_state()?;
    let at = fixture.store.begin_at(reader.version())?;
    let seen = [
        read(&at, &fixture.address(0))?,
        read(&at, &fixture.address(1))?,
    ];
    while let Some(chunk) = reader.next_chunk(64)? {
        onto.restore_state_chunk(&chunk)?;
    }
    onto.finish_state(reader.positions(), &reader.topic_heads()?)?;
    Ok(seen)
}

fn reads(fixture: &Fixture, store: &Store) -> Result<[String; 2]> {
    let reading = store.begin()?;
    Ok([
        read(&reading, &fixture.address(0))?,
        read(&reading, &fixture.address(1))?,
    ])
}

#[test]
fn every_cut_of_the_protocol_restores_to_what_a_reader_at_it_saw() -> Result<()> {
    let fixture = Fixture::new()?;
    let steps: [&dyn Fn() -> Result<()>; 7] = [
        &|| fixture.prepare(0),
        &|| fixture.decide(Decision::Pending),
        &|| fixture.decide(Decision::Committed),
        &|| fixture.resolve(0),
        &|| fixture.prepare(1),
        &|| fixture.resolve(1),
        &|| fixture.forget(),
    ];
    for (step, run) in steps.iter().enumerate() {
        run()?;
        let (restored, seen) = restore(&fixture)?;
        assert_eq!(reads(&fixture, &restored)?, seen, "after step {step}");
    }
    // The last cut is the committed transaction, whole.
    let (restored, _) = restore(&fixture)?;
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    Ok(())
}

#[test]
fn a_transaction_part_way_at_the_cut_is_finished_by_the_log_above_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.resolve(0)?;
    let (restored, seen) = restore(&fixture)?;
    assert_eq!(seen, ["old", "old"]);
    assert_eq!(reads(&fixture, &restored)?, ["old", "old"]);
    // The first home's index holds the resolution its readers do not see yet,
    // so it is not believed there, as on the source (D6b).
    let table = fixture.homes[0].table;
    assert!(fixture.store.across_unsettled(table)?);
    assert!(restored.across_unsettled(table)?);
    // The second part arrives as it would have on the source.
    fixture.prepare_on(&restored, 1)?;
    fixture.resolve_on(&restored, 1)?;
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    assert!(restored.standing_across()?.is_empty());
    assert!(!restored.across_unsettled(table)?);
    Ok(())
}

#[test]
fn an_intent_restores_as_an_intent_its_resolution_can_remove() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    let (restored, seen) = restore(&fixture)?;
    assert_eq!(seen, ["new", "new"]);
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    assert_eq!(restored.standing_across()?.len(), 1);
    fixture.resolve_on(&restored, 0)?;
    fixture.resolve_on(&restored, 1)?;
    assert!(restored.standing_across()?.is_empty());
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    Ok(())
}

#[test]
fn a_state_copied_again_onto_a_store_holding_it_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    let (restored, _) = restore(&fixture)?;
    let reading = restored.begin()?;
    // Copied again over itself, as a follower re-seeding from its leader is:
    // the decided record is not decided twice, each intent stands once, and a
    // reader that began before still sees the transaction.
    restore_onto(&fixture, &restored)?;
    assert_eq!(read(&reading, &fixture.address(1))?, "new");
    drop(reading);
    assert_eq!(reads(&fixture, &restored)?, ["new", "new"]);
    fixture.resolve_on(&restored, 0)?;
    fixture.resolve_on(&restored, 1)?;
    assert!(restored.standing_across()?.is_empty());
    // Nothing of the transaction is left to refuse a later write.
    let mut writing = restored.begin()?;
    for home in 0..2 {
        writing.put(fixture.address(home), b"later".to_vec());
    }
    writing.commit()?;
    assert_eq!(reads(&fixture, &restored)?, ["later", "later"]);
    Ok(())
}

#[test]
fn an_aborted_intent_copied_again_leaves_nothing_to_refuse_a_write() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    let (restored, _) = restore(&fixture)?;
    restore_onto(&fixture, &restored)?;
    // The transaction aborts after the copies, and its intent is dropped.
    fixture.apply_on(
        &restored,
        0,
        Part::Decide(TransactionRecord {
            decision: Decision::Aborted,
            deadline: 0,
            participants: fixture.participants(),
        }),
        Vec::new(),
    )?;
    fixture.apply_on(
        &restored,
        0,
        Part::Resolve { committed: false },
        vec![fixture.write(0, true)],
    )?;
    assert_eq!(reads(&fixture, &restored)?, ["old", "old"]);
    let mut writing = restored.begin()?;
    writing.put(fixture.address(0), b"later".to_vec());
    writing.commit()?;
    assert_eq!(reads(&fixture, &restored)?, ["later", "old"]);
    Ok(())
}
