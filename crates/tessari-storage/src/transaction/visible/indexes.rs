//! Whether a table's indexes agree with what its readers see while a
//! transaction across leaders is part-way on this node (Q-919).
//!
//! An index entry carries no version and is derived only by a committed
//! resolution. Readers see the transaction once this node holds every part of
//! it (D6a). Between the two, an index-served read that believed its index
//! would answer about a transaction its readers see differently.

use tessari_encoding::Decision;

use super::fixture::Fixture;
use crate::error::Result;

/// Whether an index may answer a read of `home`'s table, asked at the tail.
fn current(fixture: &Fixture, home: usize) -> Result<bool> {
    fixture
        .store
        .begin()?
        .indexes_are_current_for(fixture.homes[home].table)
}

#[test]
fn a_resolution_readers_cannot_see_yet_unsettles_its_table_alone() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    assert!(
        current(&fixture, 1)?,
        "nothing of it is in the second table"
    );
    fixture.resolve(0)?;
    // The first table's index holds "new" while the second part is missing,
    // so its readers still see "old".
    assert!(!current(&fixture, 0)?, "an index ahead of its readers");
    assert!(
        current(&fixture, 1)?,
        "a table the transaction has not reached"
    );
    // The second part lands: readers see the whole transaction, the first
    // table's index agrees, and the second's intent is not in its index yet.
    fixture.prepare(1)?;
    assert!(current(&fixture, 0)?, "settled once every part is here");
    assert!(
        !current(&fixture, 1)?,
        "an intent readers see and no index holds"
    );
    fixture.resolve(1)?;
    assert!(current(&fixture, 0)? && current(&fixture, 1)?);
    Ok(())
}

#[test]
fn intents_a_committed_record_makes_values_unsettle_their_tables() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    fixture.decide(Decision::Pending)?;
    // Undecided, nobody sees the intents and no index holds them: they agree.
    assert!(current(&fixture, 0)? && current(&fixture, 1)?);
    fixture.decide(Decision::Committed)?;
    assert!(!current(&fixture, 0)? && !current(&fixture, 1)?);
    fixture.resolve(0)?;
    assert!(current(&fixture, 0)?);
    assert!(!current(&fixture, 1)?);
    fixture.resolve(1)?;
    assert!(current(&fixture, 1)?);
    Ok(())
}

#[test]
fn an_aborted_transaction_never_unsettles_anything() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Aborted)?;
    assert!(current(&fixture, 0)? && current(&fixture, 1)?);
    Ok(())
}

#[test]
fn a_snapshot_behind_the_tail_is_not_current_for_any_table() -> Result<()> {
    let fixture = Fixture::new()?;
    let behind = fixture.store.begin()?;
    fixture.prepare(0)?;
    assert!(!behind.indexes_are_current_for(fixture.homes[1].table)?);
    Ok(())
}
