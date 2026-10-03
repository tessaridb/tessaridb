//! Forgetting a decided record (ADR-0112 D12): the record goes, what readers
//! see does not change, and only a decided record can be forgotten.

use tessari_encoding::Decision;

use super::fixture::{Fixture, TRANSACTION, read};
use crate::error::{Error, Result};

#[test]
fn a_forgotten_record_is_gone_and_readers_still_see_the_transaction() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.prepare(1)?;
    fixture.decide(Decision::Pending)?;
    fixture.decide(Decision::Committed)?;
    fixture.resolve(0)?;
    fixture.resolve(1)?;
    fixture.forget()?;
    assert_eq!(fixture.store.transaction_record(TRANSACTION)?, None);
    let reading = fixture.store.begin()?;
    for home in 0..2 {
        assert_eq!(read(&reading, &fixture.address(home))?, "new");
    }
    // Forgetting again finds nothing and changes nothing.
    fixture.forget()?;
    Ok(())
}

#[test]
fn a_record_still_pending_is_not_forgotten() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.prepare(0)?;
    fixture.decide(Decision::Pending)?;
    let refused = fixture.forget();
    assert!(
        matches!(refused, Err(Error::AcrossMalformed { part: "forget", .. })),
        "{refused:?}"
    );
    assert_eq!(
        fixture
            .store
            .transaction_record(TRANSACTION)?
            .map(|record| record.decision),
        Some(Decision::Pending)
    );
    Ok(())
}
