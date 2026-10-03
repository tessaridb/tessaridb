//! The coordinator's range in two records rather than four (ADR-0112 D13a,
//! D13b): its record begun with its own writes held as intents, and its record
//! decided with those intents resolved — each one log record, applied as a
//! follower applies it.

use tessari_encoding::{AcrossPartKey, Decision, Part, Participant, StoreKey, TransactionRecord};
use tessari_types::{Reach, Sequence};

use super::{Fixture, TRANSACTION, doc, record_of};
use crate::error::{Error, Result};

impl Fixture {
    /// The record as the coordinator writes it: its first participant is the
    /// range its own writes fall in, which holds the record.
    fn decided(&self, decision: Decision) -> TransactionRecord {
        TransactionRecord {
            decision,
            deadline: 0,
            participants: vec![Participant {
                range: Reach::Database(self.namespace, self.database),
                prepared_at: Some(Sequence::new(1)),
            }],
        }
    }

    fn begin(&mut self) -> Result<()> {
        let intent = self.write(self.new_value(true));
        self.apply(Part::Begin(self.decided(Decision::Pending)), vec![intent])
    }

    /// Whether this node marks the coordinator's own part landed.
    fn landed_here(&self) -> Result<bool> {
        let part = AcrossPartKey {
            transaction: TRANSACTION,
            range: Reach::Database(self.namespace, self.database),
        };
        Ok(self
            .store
            .backend()
            .get(AcrossPartKey::keyspace(), &part.encode())?
            .is_some())
    }
}

#[test]
fn a_begin_lands_the_pending_record_and_its_intents_together() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let before = fixture.index_entries()?;
    fixture.begin()?;
    assert_eq!(
        record_of(&fixture.store)?,
        Some(fixture.decided(Decision::Pending))
    );
    assert!(fixture.intent_left()?, "the writes stand as intents");
    assert!(
        fixture.landed_here()?,
        "where the part landed is marked (D6a)"
    );
    assert_eq!(
        fixture.read()?,
        Some(doc("old")),
        "an intent is not a value"
    );
    assert_eq!(
        fixture.index_entries()?,
        before,
        "an intent derives nothing"
    );
    Ok(())
}

#[test]
fn a_begin_finding_its_record_already_aborted_lands_nothing() -> Result<()> {
    let mut fixture = Fixture::new()?;
    // A participant holding a prepare that outran the begin found the record
    // absent and aborted it (D7).
    fixture.apply(Part::Decide(fixture.decided(Decision::Aborted)), vec![])?;
    let refused = fixture.begin();
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "aborted" })),
        "{refused:?}"
    );
    assert_eq!(
        record_of(&fixture.store)?,
        Some(fixture.decided(Decision::Aborted))
    );
    assert!(!fixture.intent_left()?);
    assert!(!fixture.landed_here()?);
    Ok(())
}

#[test]
fn a_begin_carrying_a_plain_write_is_refused() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let plain = fixture.write(tessari_encoding::StampedValue::new(
        tessari_encoding::RecordValue::Present(doc("new")),
    ));
    let refused = fixture.apply(Part::Begin(fixture.decided(Decision::Pending)), vec![plain]);
    assert!(
        matches!(refused, Err(Error::AcrossMalformed { part: "begin", .. })),
        "{refused:?}"
    );
    assert_eq!(record_of(&fixture.store)?, None);
    Ok(())
}

#[test]
fn a_conclude_commits_the_record_and_its_intents_in_one_record() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let before = fixture.index_entries()?;
    fixture.begin()?;
    let resolved = fixture.write(fixture.new_value(false));
    fixture.apply(
        Part::Conclude(fixture.decided(Decision::Committed)),
        vec![resolved],
    )?;
    assert_eq!(
        record_of(&fixture.store)?,
        Some(fixture.decided(Decision::Committed))
    );
    assert_eq!(fixture.read()?, Some(doc("new")));
    assert!(!fixture.intent_left()?, "the resolution removes its intent");
    assert_ne!(
        fixture.index_entries()?,
        before,
        "a committed value derives"
    );
    Ok(())
}

#[test]
fn a_conclude_that_aborts_drops_the_intents() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let before = fixture.index_entries()?;
    fixture.begin()?;
    let named = fixture.write(fixture.new_value(true));
    fixture.apply(
        Part::Conclude(fixture.decided(Decision::Aborted)),
        vec![named],
    )?;
    assert_eq!(
        record_of(&fixture.store)?,
        Some(fixture.decided(Decision::Aborted))
    );
    assert_eq!(fixture.read()?, Some(doc("old")));
    assert!(!fixture.intent_left()?);
    assert_eq!(fixture.index_entries()?, before);
    Ok(())
}

#[test]
fn a_conclude_against_a_lapsed_record_resolves_nothing() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.begin()?;
    fixture.apply(Part::Decide(fixture.decided(Decision::Aborted)), vec![])?;
    let resolved = fixture.write(fixture.new_value(false));
    let refused = fixture.apply(
        Part::Conclude(fixture.decided(Decision::Committed)),
        vec![resolved],
    );
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "aborted" })),
        "{refused:?}"
    );
    assert_eq!(fixture.read()?, Some(doc("old")));
    assert!(
        fixture.intent_left()?,
        "the abort's own resolution drops it"
    );
    Ok(())
}
