//! A participant's part barred by status recovery before its prepare landed
//! (ADR-0112 D14c): a prepare arriving afterwards is refused for good, and a
//! part that already landed cannot be barred.

use tessari_encoding::{AcrossBarredKey, Part, StoreKey};
use tessari_types::Reach;

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
