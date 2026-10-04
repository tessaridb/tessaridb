//! A record committed in parallel and left `STAGING` (ADR-0112 D14): the pass
//! recovers it once overdue — committed where every part landed, barred and
//! aborted where one did not — and never resolves an intent against it.

use tessari_encoding::{Participant, TransactionRecord};

use tessari_session::AcrossAsk;
use tessari_storage::Decision;
use tessari_types::{Reach, Value};

use super::{TRANSACTION, left_behind, note};
use crate::{Db, SettledAcross};

/// The record written `STAGING` over `participants`, none known landed, with
/// `deadline` — as a begin leaves it.
fn stage(db: &Db, participants: &[Reach], deadline: u64) {
    let mut session = db.session();
    for decision in [Decision::Pending, Decision::Staging] {
        session
            .answer_across(&AcrossAsk::Decide {
                transaction: TRANSACTION,
                record: TransactionRecord {
                    decision,
                    deadline,
                    participants: participants
                        .iter()
                        .map(|range| Participant {
                            range: *range,
                            prepared_at: None,
                        })
                        .collect(),
                },
            })
            .unwrap();
    }
}

/// The range `left_behind`'s intent landed in, and its namespace's.
fn ranges(db: &Db) -> (Reach, Reach) {
    let home = db.store().standing_across().unwrap()[0].1;
    let Reach::Database(namespace, _) = home else {
        unreachable!("the intent's record lives in its database: {home:?}")
    };
    (home, Reach::Namespace(namespace))
}

fn decision(db: &Db) -> Option<Decision> {
    db.store()
        .transaction_record(TRANSACTION)
        .unwrap()
        .map(|held| held.decision)
}

#[test]
fn an_overdue_staging_record_whose_every_part_landed_is_committed() {
    let (db, address) = left_behind(&[]);
    let (home, _) = ranges(&db);
    stage(&db, &[home], 0);
    let settled = db.settle_across().unwrap();
    assert_eq!(
        settled,
        SettledAcross {
            committed: 1,
            resolved: 1,
            ..SettledAcross::default()
        }
    );
    assert_eq!(decision(&db), Some(Decision::Committed));
    assert_eq!(note(&db, &address), Value::from("new"));
}

#[test]
fn an_overdue_staging_record_missing_a_part_is_barred_and_aborted() {
    let (db, address) = left_behind(&[]);
    let (home, elsewhere) = ranges(&db);
    stage(&db, &[home, elsewhere], 0);
    let settled = db.settle_across().unwrap();
    assert_eq!(
        settled,
        SettledAcross {
            aborted: 1,
            resolved: 1,
            ..SettledAcross::default()
        }
    );
    assert_eq!(decision(&db), Some(Decision::Aborted));
    assert_eq!(note(&db, &address), Value::from("old"));
    assert_eq!(
        db.store().part_landed(TRANSACTION, elsewhere).unwrap(),
        None,
        "the missing part was barred, not landed"
    );
}

#[test]
fn an_intent_under_a_staging_record_is_never_resolved_by_the_pass() {
    // The record is live, and may be committed implicitly already: neither a
    // young intent's lookup nor an old one's settle resolves it (D14c).
    let (db, address) = left_behind(&[]);
    let (home, elsewhere) = ranges(&db);
    stage(&db, &[home, elsewhere], u64::MAX);
    let now = super::super::now_millis();
    let lapse = tessari_session::across_lapse_millis(db.store()).unwrap();
    for at in [now, now.saturating_add(lapse).saturating_add(1)] {
        let settled = db.settle_across_at(at).unwrap();
        assert_eq!(settled.resolved, 0, "{settled:?}");
        assert!(db.store().holds_intents_of(TRANSACTION).unwrap());
    }
    assert_eq!(decision(&db), Some(Decision::Staging));
    assert_eq!(note(&db, &address), Value::from("old"));
}
