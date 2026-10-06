//! A participant's part barred by status recovery before its prepare landed
//! (ADR-0112 D14c): a prepare arriving afterwards is refused for good, and a
//! part that already landed cannot be barred.

use tessari_encoding::{
    AcrossBarredKey, LogId, LogRecord, Part, RecordValue, StampedValue, StoreKey,
};
use tessari_types::{NamespaceId, Reach, Sequence};

use super::{Fixture, TRANSACTION, doc};
use crate::error::{Error, Result};

impl Fixture {
    /// The range a prepare of this fixture's write lands in.
    fn participant(&self) -> Reach {
        Reach::Database(self.namespace, self.database)
    }

    fn bar(&mut self) -> Result<()> {
        let range = self.participant();
        self.apply(Part::Prevent { range }, vec![])
    }

    fn barred_here(&self) -> Result<bool> {
        let key = AcrossBarredKey {
            transaction: TRANSACTION,
            range: self.participant(),
        };
        Ok(self
            .store
            .backend()
            .get(AcrossBarredKey::keyspace(), &key.encode())?
            .is_some())
    }
}

#[test]
fn a_barred_part_refuses_its_prepare_for_good() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.bar()?;
    assert!(fixture.barred_here()?, "the bar is marked");
    let refused = fixture.prepare();
    assert!(
        matches!(refused, Err(Error::AcrossDecided { decided: "barred" })),
        "{refused:?}"
    );
    assert!(!fixture.intent_left()?, "a refused prepare holds nothing");
    assert_eq!(fixture.read()?, Some(doc("old")));
    // Asked again — a recovery retried — the bar stands as it was.
    fixture.bar()?;
    assert!(fixture.barred_here()?);
    Ok(())
}

#[test]
fn a_part_that_landed_cannot_be_barred() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.prepare()?;
    let refused = fixture.bar();
    assert!(
        matches!(
            refused,
            Err(Error::AcrossDecided {
                decided: "prepared"
            })
        ),
        "{refused:?}"
    );
    assert!(!fixture.barred_here()?, "nothing is barred");
    assert!(fixture.intent_left()?, "the prepare stands");
    Ok(())
}

#[test]
fn a_bar_carrying_writes_is_refused() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let range = fixture.participant();
    let write = fixture.write(fixture.new_value(true));
    let refused = fixture.apply(Part::Prevent { range }, vec![write]);
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
    assert!(!fixture.barred_here()?);
    Ok(())
}

impl Fixture {
    /// How many parts are barred on this node, of any transaction.
    fn bars(&self) -> Result<usize> {
        self.store.bars_across()
    }

    /// An ordinary record after the bar, so the log has a tail to prune under.
    fn write_on(&mut self) -> Result<()> {
        let plain = self.write(StampedValue::new(RecordValue::Present(doc("later"))));
        let record = LogRecord::new(vec![plain]);
        let at = self.at.saturating_add(1);
        self.store
            .apply_record_in(self.log, Sequence::new(at), &record)?;
        self.at = at;
        Ok(())
    }
}

#[test]
fn a_bar_ends_when_its_log_is_pruned_past_the_record_that_wrote_it() -> Result<()> {
    // ADR-0119: past that record the log no longer reaches back to anything
    // the transaction read, so its prepare is refused as too old (D3a).
    let mut fixture = Fixture::new()?;
    fixture.bar()?;
    let barred_at = Sequence::new(fixture.at);
    fixture.write_on()?;
    fixture.write_on()?;
    assert_eq!(fixture.bars()?, 1);
    fixture.store.prune_log(fixture.log, barred_at)?;
    assert_eq!(fixture.bars()?, 0, "the bar went with its record");
    assert!(!fixture.barred_here()?);
    Ok(())
}

#[test]
fn a_prune_short_of_the_bar_keeps_it() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.write_on()?;
    let before = Sequence::new(fixture.at);
    fixture.bar()?;
    fixture.write_on()?;
    fixture.store.prune_log(fixture.log, before)?;
    assert_eq!(
        fixture.bars()?,
        1,
        "the record that wrote the bar is still held"
    );
    assert!(fixture.barred_here()?);
    Ok(())
}

#[test]
fn a_prune_of_another_log_keeps_the_bar() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.bar()?;
    let barred_at = Sequence::new(fixture.at);
    fixture.write_on()?;
    fixture.write_on()?;
    let elsewhere = LogId::line(Reach::Namespace(NamespaceId::new(78)));
    for at in 1..=3 {
        let plain = fixture.write(StampedValue::new(RecordValue::Present(doc("other"))));
        fixture.store.apply_record_in(
            elsewhere,
            Sequence::new(at),
            &LogRecord::new(vec![plain]),
        )?;
    }
    fixture.store.prune_log(elsewhere, barred_at)?;
    assert_eq!(
        fixture.bars()?,
        1,
        "a prune of another log says nothing of this one"
    );
    Ok(())
}

#[test]
fn each_copy_drops_its_bar_when_its_own_log_is_pruned() -> Result<()> {
    // Leader and follower apply the same records and prune on their own
    // schedules: a copy keeps its bar until its own log no longer holds the
    // record, whatever the other copy did.
    let mut leader = Fixture::new()?;
    let mut follower = Fixture::new()?;
    for copy in [&mut leader, &mut follower] {
        copy.bar()?;
        copy.write_on()?;
        copy.write_on()?;
    }
    let barred_at = Sequence::new(1);
    leader.store.prune_log(leader.log, barred_at)?;
    assert_eq!((leader.bars()?, follower.bars()?), (0, 1));
    follower.store.prune_log(follower.log, barred_at)?;
    assert_eq!((leader.bars()?, follower.bars()?), (0, 0));
    Ok(())
}
