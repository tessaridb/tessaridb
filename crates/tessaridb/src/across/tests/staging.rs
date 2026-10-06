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

#[test]
fn a_bar_ends_with_its_log_and_a_late_prepare_is_refused_as_too_old() {
    // ADR-0119: recovery barred the missing part and aborted; once the barred
    // range's log is pruned past the bar, the bar is gone and the prepare
    // still in flight is refused for reading what the log no longer holds.
    let (db, _) = left_behind(&[]);
    let (home, _) = ranges(&db);
    db.session()
        .run("USE NAMESPACE prod; DEFINE DATABASE other; USE DATABASE other; DEFINE COLLECTION more;")
        .unwrap();
    let store = db.store();
    let (elsewhere, late) = {
        let mut reading = store.begin().unwrap();
        let catalog = tessari_storage::Catalog::new(&mut reading);
        let namespace = catalog.namespace_id("prod").unwrap().unwrap();
        let other = catalog.database_id(namespace, "other").unwrap().unwrap();
        let more = catalog.table_id(namespace, other, "more").unwrap().unwrap();
        reading.rollback();
        // The barred part's write, as its prepare carries it.
        let late = tessari_encoding::Mutation {
            namespace,
            database: other,
            table: more,
            id: tessari_types::RecordId::Int(9),
            shard: None,
            value: tessari_encoding::StampedValue::new(tessari_encoding::RecordValue::Present(
                tessari_encoding::encode_payload(&Value::from("late")).into_bytes(),
            )),
        };
        (Reach::Database(namespace, other), late)
    };
    // What the late prepare read: the barred range's log before the bar.
    let log = store.history_log(elsewhere).unwrap();
    let seen = store.committed_tail(log).unwrap();
    stage(&db, &[home, elsewhere], 0);
    db.settle_across().unwrap();
    assert_eq!(decision(&db), Some(Decision::Aborted));
    let bars = || store.bars_across().unwrap();
    assert_eq!(bars(), 1, "recovery barred the missing part");
    // Writes into the barred range after the bar, so its log has a tail to
    // prune under.
    db.session()
        .run("USE NAMESPACE prod; USE DATABASE other; CREATE more:1 = 'a'; CREATE more:2 = 'b';")
        .unwrap();
    let tail = store.committed_tail(log).unwrap();
    store.prune_log(log, tail).unwrap();
    assert_eq!(bars(), 0, "the bar went with the record that wrote it");
    let refused = db.session().answer_across(&AcrossAsk::Prepare {
        transaction: TRANSACTION,
        coordinator: home,
        seen,
        writes: vec![late],
    });
    assert!(
        matches!(
            refused,
            Err(tessari_session::Error::Store(
                tessari_storage::Error::AcrossReadTooOld { .. }
            ))
        ),
        "{refused:?}"
    );
}
