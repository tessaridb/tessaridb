//! A participant's part barred by status recovery before its prepare landed
//! (ADR-0112 D14c): a prepare arriving afterwards is refused for good, and a
//! part that already landed cannot be barred.

use tessari_encoding::{
    AcrossBarredKey, LogId, LogRecord, Part, RecordValue, StampedValue, StoreKey,
};
use tessari_kv::AsyncKvBackend as _;
use tessari_types::{NamespaceId, Reach, Sequence};

use super::{Fixture, TRANSACTION, doc};
use crate::error::{Error, Result};

impl Fixture {
    /// The range a prepare of this fixture's write lands in.
    fn participant(&self) -> Reach {
        Reach::Database(self.namespace, self.database)
    }

    async fn bar(&mut self) -> Result<()> {
        let range = self.participant();
        self.apply(Part::Prevent { range }, vec![]).await
    }

    async fn barred_here(&self) -> Result<bool> {
        let key = AcrossBarredKey {
            transaction: TRANSACTION,
            range: self.participant(),
        };
        Ok(self
            .store
            .backend()
            .get(AcrossBarredKey::keyspace(), &key.encode())
            .await?
            .is_some())
    }
}

#[tokio::test]
async fn a_barred_part_refuses_its_prepare_for_good() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.bar().await?;
    assert!(fixture.barred_here().await?, "the bar is marked");
    let refused = fixture.prepare().await;
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "barred" })),
        "{refused:?}"
    );
    assert!(
        !fixture.intent_left().await?,
        "a refused prepare holds nothing"
    );
    assert_eq!(fixture.read().await?, Some(doc("old")));
    // Asked again — a recovery retried — the bar stands as it was.
    fixture.bar().await?;
    assert!(fixture.barred_here().await?);
    Ok(())
}

#[tokio::test]
async fn a_part_that_landed_cannot_be_barred() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.prepare().await?;
    let refused = fixture.bar().await;
    assert!(
        matches!(
            refused,
            Err(Error::AcrossDecided {
                decided: "prepared"
            })
        ),
        "{refused:?}"
    );
    assert!(!fixture.barred_here().await?, "nothing is barred");
    assert!(fixture.intent_left().await?, "the prepare stands");
    Ok(())
}

#[tokio::test]
async fn a_bar_carrying_writes_is_refused() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let range = fixture.participant();
    let write = fixture.write(fixture.new_value(true));
    let refused = fixture.apply(Part::Prevent { range }, vec![write]).await;
    assert!(
        matches!(
            refused,
            Err(Error::AcrossMalformed {
                part: "prevent",
                ..
            })
        ),
        "{refused:?}"
    );
    assert!(!fixture.barred_here().await?);
    Ok(())
}

impl Fixture {
    /// How many parts are barred on this node, of any transaction.
    async fn bars(&self) -> Result<usize> {
        self.store.bars_across().await
    }

    /// An ordinary record after the bar, so the log has a tail to prune under.
    async fn write_on(&mut self) -> Result<()> {
        let plain = self.write(StampedValue::new(RecordValue::Present(doc("later"))));
        let record = LogRecord::new(vec![plain]);
        let at = self.at.saturating_add(1);
        self.store
            .apply_record_in(self.log, Sequence::new(at), &record)
            .await?;
        self.at = at;
        Ok(())
    }
}

#[tokio::test]
async fn a_bar_ends_when_its_log_is_pruned_past_the_record_that_wrote_it() -> Result<()> {
    // ADR-0119: past that record the log no longer reaches back to anything
    // the transaction read, so its prepare is refused as too old (D3a).
    let mut fixture = Fixture::new().await?;
    fixture.bar().await?;
    let barred_at = Sequence::new(fixture.at);
    fixture.write_on().await?;
    fixture.write_on().await?;
    assert_eq!(fixture.bars().await?, 1);
    fixture.store.prune_log(fixture.log, barred_at).await?;
    assert_eq!(fixture.bars().await?, 0, "the bar went with its record");
    assert!(!fixture.barred_here().await?);
    Ok(())
}

#[tokio::test]
async fn a_prune_short_of_the_bar_keeps_it() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.write_on().await?;
    let before = Sequence::new(fixture.at);
    fixture.bar().await?;
    fixture.write_on().await?;
    fixture.store.prune_log(fixture.log, before).await?;
    assert_eq!(
        fixture.bars().await?,
        1,
        "the record that wrote the bar is still held"
    );
    assert!(fixture.barred_here().await?);
    Ok(())
}

#[tokio::test]
async fn a_prune_of_another_log_keeps_the_bar() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.bar().await?;
    let barred_at = Sequence::new(fixture.at);
    fixture.write_on().await?;
    fixture.write_on().await?;
    let elsewhere = LogId::line(Reach::Namespace(NamespaceId::new(78)));
    for at in 1..=3 {
        let plain = fixture.write(StampedValue::new(RecordValue::Present(doc("other"))));
        fixture
            .store
            .apply_record_in(elsewhere, Sequence::new(at), &LogRecord::new(vec![plain]))
            .await?;
    }
    fixture.store.prune_log(elsewhere, barred_at).await?;
    assert_eq!(
        fixture.bars().await?,
        1,
        "a prune of another log says nothing of this one"
    );
    Ok(())
}

#[tokio::test]
async fn each_copy_drops_its_bar_when_its_own_log_is_pruned() -> Result<()> {
    // Leader and follower apply the same records and prune on their own
    // schedules: a copy keeps its bar until its own log no longer holds the
    // record, whatever the other copy did.
    let mut leader = Fixture::new().await?;
    let mut follower = Fixture::new().await?;
    for copy in [&mut leader, &mut follower] {
        copy.bar().await?;
        copy.write_on().await?;
        copy.write_on().await?;
    }
    let barred_at = Sequence::new(1);
    leader.store.prune_log(leader.log, barred_at).await?;
    assert_eq!((leader.bars().await?, follower.bars().await?), (0, 1));
    follower.store.prune_log(follower.log, barred_at).await?;
    assert_eq!((leader.bars().await?, follower.bars().await?), (0, 0));
    Ok(())
}
