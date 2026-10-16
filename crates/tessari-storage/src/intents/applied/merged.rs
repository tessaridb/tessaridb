//! The coordinator's range in two records rather than four (ADR-0112 D13a,
//! D13b): its record begun with its own writes held as intents, and its record
//! decided with those intents resolved — each one log record, applied as a
//! follower applies it.

use tessari_encoding::{AcrossPartKey, Decision, Part, Participant, StoreKey, TransactionRecord};
use tessari_kv::AsyncKvBackend as _;
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

    async fn begin(&mut self) -> Result<()> {
        let intent = self.write(self.new_value(true));
        self.apply(Part::Begin(self.decided(Decision::Staging)), vec![intent])
            .await
    }

    /// Whether this node marks the coordinator's own part landed.
    async fn landed_here(&self) -> Result<bool> {
        let part = AcrossPartKey {
            transaction: TRANSACTION,
            range: Reach::Database(self.namespace, self.database),
        };
        Ok(self
            .store
            .backend()
            .get(AcrossPartKey::keyspace(), &part.encode())
            .await?
            .is_some())
    }
}

#[tokio::test]
async fn a_begin_lands_the_staging_record_and_its_intents_together() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let before = fixture.index_entries().await?;
    fixture.begin().await?;
    assert_eq!(
        record_of(&fixture.store).await?,
        Some(fixture.decided(Decision::Staging))
    );
    assert!(fixture.intent_left().await?, "the writes stand as intents");
    assert!(
        fixture.landed_here().await?,
        "where the part landed is marked (D6a)"
    );
    assert_eq!(
        fixture.read().await?,
        Some(doc("old")),
        "an intent is not a value"
    );
    assert_eq!(
        fixture.index_entries().await?,
        before,
        "an intent derives nothing"
    );
    Ok(())
}

#[tokio::test]
async fn a_begin_finding_its_record_already_aborted_lands_nothing() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    // A participant holding a prepare that outran the begin found the record
    // absent and aborted it (D7).
    fixture
        .apply(Part::Decide(fixture.decided(Decision::Aborted)), vec![])
        .await?;
    let refused = fixture.begin().await;
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "aborted" })),
        "{refused:?}"
    );
    assert_eq!(
        record_of(&fixture.store).await?,
        Some(fixture.decided(Decision::Aborted))
    );
    assert!(!fixture.intent_left().await?);
    assert!(!fixture.landed_here().await?);
    Ok(())
}

#[tokio::test]
async fn a_begin_carrying_a_plain_write_is_refused() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let plain = fixture.write(tessari_encoding::StampedValue::new(
        tessari_encoding::RecordValue::Present(doc("new")),
    ));
    let refused = fixture
        .apply(Part::Begin(fixture.decided(Decision::Staging)), vec![plain])
        .await;
    assert!(
        matches!(refused, Err(Error::AcrossMalformed { part: "begin", .. })),
        "{refused:?}"
    );
    assert_eq!(record_of(&fixture.store).await?, None);
    Ok(())
}

#[tokio::test]
async fn a_begin_that_does_not_stage_is_refused() -> Result<()> {
    // A begin is the parallel commit's first record (ADR-0112 D14a): a record
    // begun undecided in any other way would never commit implicitly.
    let mut fixture = Fixture::new().await?;
    let intent = fixture.write(fixture.new_value(true));
    let refused = fixture
        .apply(
            Part::Begin(fixture.decided(Decision::Pending)),
            vec![intent],
        )
        .await;
    assert!(
        matches!(refused, Err(Error::AcrossMalformed { part: "begin", .. })),
        "{refused:?}"
    );
    assert_eq!(record_of(&fixture.store).await?, None);
    assert!(!fixture.intent_left().await?);
    Ok(())
}

#[tokio::test]
async fn a_conclude_commits_the_record_and_its_intents_in_one_record() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let before = fixture.index_entries().await?;
    fixture.begin().await?;
    let resolved = fixture.write(fixture.new_value(false));
    fixture
        .apply(
            Part::Conclude(fixture.decided(Decision::Committed)),
            vec![resolved],
        )
        .await?;
    assert_eq!(
        record_of(&fixture.store).await?,
        Some(fixture.decided(Decision::Committed))
    );
    assert_eq!(fixture.read().await?, Some(doc("new")));
    assert!(
        !fixture.intent_left().await?,
        "the resolution removes its intent"
    );
    assert_ne!(
        fixture.index_entries().await?,
        before,
        "a committed value derives"
    );
    Ok(())
}

#[tokio::test]
async fn a_conclude_that_aborts_drops_the_intents() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let before = fixture.index_entries().await?;
    fixture.begin().await?;
    let named = fixture.write(fixture.new_value(true));
    fixture
        .apply(
            Part::Conclude(fixture.decided(Decision::Aborted)),
            vec![named],
        )
        .await?;
    assert_eq!(
        record_of(&fixture.store).await?,
        Some(fixture.decided(Decision::Aborted))
    );
    assert_eq!(fixture.read().await?, Some(doc("old")));
    assert!(!fixture.intent_left().await?);
    assert_eq!(fixture.index_entries().await?, before);
    Ok(())
}

#[tokio::test]
async fn a_conclude_against_a_lapsed_record_resolves_nothing() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.begin().await?;
    fixture
        .apply(Part::Decide(fixture.decided(Decision::Aborted)), vec![])
        .await?;
    let resolved = fixture.write(fixture.new_value(false));
    let refused = fixture
        .apply(
            Part::Conclude(fixture.decided(Decision::Committed)),
            vec![resolved],
        )
        .await;
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "aborted" })),
        "{refused:?}"
    );
    assert_eq!(fixture.read().await?, Some(doc("old")));
    assert!(
        fixture.intent_left().await?,
        "the abort's own resolution drops it"
    );
    Ok(())
}

#[tokio::test]
async fn a_staging_record_is_never_reopened_as_pending() -> Result<()> {
    // A staging record may be committed implicitly already (ADR-0112 D14a):
    // reopened as PENDING, a lapse could abort what was committed.
    let mut fixture = Fixture::new().await?;
    fixture.begin().await?;
    let refused = fixture
        .apply(Part::Decide(fixture.decided(Decision::Pending)), vec![])
        .await;
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "staging" })),
        "{refused:?}"
    );
    assert_eq!(
        record_of(&fixture.store).await?,
        Some(fixture.decided(Decision::Staging))
    );
    Ok(())
}
