#![allow(clippy::unwrap_used)]

use tessari_encoding::{
    Mutation, Participant, RecordValue, StampedValue, TRANSACTION_ID_LEN, TransactionId,
    TransactionRecord, decode_payload, encode_payload,
};
use tessari_session::{AcrossAnswer, AcrossAsk, Session};
use tessari_storage::{Catalog, Decision, RecordAddress};
use tessari_types::{Reach, RecordId, Value};

use crate::{Db, SettledAcross};

const TRANSACTION: TransactionId = TransactionId::new([5; TRANSACTION_ID_LEN]);

/// A store holding `notes:1 = 'old'` with an intent of `TRANSACTION` on it
/// writing `'new'`, and the transaction's record written `decisions` in turn —
/// what a coordinator that died part-way leaves behind.
fn left_behind(decisions: &[Decision]) -> (Db, RecordAddress) {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
             USE DATABASE shop; DEFINE COLLECTION notes; CREATE notes:1 = 'old';",
        )
        .unwrap();
    let store = db.store();
    let mut reading = store.begin().unwrap();
    let catalog = Catalog::new(&mut reading);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "notes")
        .unwrap()
        .unwrap();
    reading.rollback();
    let home = Reach::Database(namespace, database);
    let mut session: Session<'_> = db.session();
    let prepared = session
        .answer_across(&AcrossAsk::Prepare {
            transaction: TRANSACTION,
            coordinator: home,
            seen: store.committed_tail(store.own_log(home).unwrap()).unwrap(),
            writes: vec![Mutation {
                namespace,
                database,
                table,
                id: RecordId::Int(1),
                shard: None,
                value: StampedValue::new(RecordValue::Present(
                    encode_payload(&Value::from("new")).into_bytes(),
                )),
            }],
        })
        .unwrap();
    let AcrossAnswer::Prepared(prepared_at) = prepared else {
        unreachable!("a prepare answers where it landed")
    };
    for decision in decisions {
        session
            .answer_across(&AcrossAsk::Decide {
                transaction: TRANSACTION,
                record: TransactionRecord {
                    decision: *decision,
                    deadline: 0,
                    participants: vec![Participant {
                        range: home,
                        prepared_at: Some(prepared_at),
                    }],
                },
            })
            .unwrap();
    }
    let address = RecordAddress::new(namespace, database, table, RecordId::Int(1));
    (db, address)
}

fn note(db: &Db, address: &RecordAddress) -> Value {
    decode_payload(&db.store().begin().unwrap().get(address).unwrap().unwrap()).unwrap()
}

#[test]
fn an_overdue_pending_record_is_aborted_and_its_intent_dropped() {
    let (db, address) = left_behind(&[Decision::Pending]);
    let settled = db.settle_across().unwrap();
    assert_eq!(
        settled,
        SettledAcross {
            aborted: 1,
            resolved: 1,
            ..SettledAcross::default()
        }
    );
    assert_eq!(note(&db, &address), Value::from("old"));
    assert!(db.store().standing_across().unwrap().is_empty());
    assert_eq!(
        db.settle_across().unwrap(),
        SettledAcross::default(),
        "a second pass finds nothing left"
    );
}

#[test]
fn a_record_nobody_wrote_is_aborted_by_the_participant_holding_its_intent() {
    let (db, address) = left_behind(&[]);
    let settled = db.settle_across().unwrap();
    assert_eq!(settled.resolved, 1, "{settled:?}");
    assert_eq!(note(&db, &address), Value::from("old"));
    assert_eq!(
        db.store()
            .transaction_record(TRANSACTION)
            .unwrap()
            .map(|held| held.decision),
        Some(Decision::Aborted)
    );
}

#[test]
fn a_committed_record_whose_resolution_was_lost_is_finished_here() {
    let (db, address) = left_behind(&[Decision::Pending, Decision::Committed]);
    let settled = db.settle_across().unwrap();
    assert_eq!(
        settled,
        SettledAcross {
            resolved: 1,
            ..SettledAcross::default()
        }
    );
    assert_eq!(note(&db, &address), Value::from("new"));
    assert!(db.store().standing_across().unwrap().is_empty());
}

#[test]
fn a_pass_publishes_what_it_left_standing_and_nothing_before_the_first() {
    let (db, _) = left_behind(&[Decision::Pending]);
    let held = db.store().health().unwrap();
    assert_eq!(
        (held.across_pending, held.across_with_intents),
        (None, None),
        "no pass has looked yet, which is not the same as none"
    );
    // The coordinator is alive and renewed its record: nothing is overdue,
    // so the pass leaves both the record and the intent standing.
    let mut record = db.store().transaction_record(TRANSACTION).unwrap().unwrap();
    record.deadline = u64::MAX;
    db.session()
        .answer_across(&AcrossAsk::Decide {
            transaction: TRANSACTION,
            record,
        })
        .unwrap();
    assert_eq!(reported(&db, "pending"), Value::Null);
    db.settle_across().unwrap();
    let held = db.store().health().unwrap();
    assert_eq!(
        (held.across_pending, held.across_with_intents),
        (Some(1), Some(1))
    );
    // `INFO FOR NODE` says the same, from the same `health()`.
    assert_eq!(reported(&db, "pending"), Value::from(1_i64));
    assert_eq!(reported(&db, "with_intents"), Value::from(1_i64));
    assert_eq!(reported(&db, "in_doubt"), Value::from(0_i64));
}

/// One figure of `INFO FOR NODE`'s `cluster.across` group — `NONE` when the
/// report, the group or the figure is missing, which no assertion here expects.
fn reported(db: &Db, figure: &str) -> Value {
    let outcomes = db.session().run("INFO FOR NODE;").unwrap();
    let object = |value: Option<&Value>| match value {
        Some(Value::Object(fields)) => Some(fields.clone()),
        _ => None,
    };
    let report = match outcomes.first() {
        Some(tessari_session::Outcome::Value(value)) => object(Some(value)),
        _ => None,
    };
    report
        .and_then(|report| object(report.get("cluster")))
        .and_then(|cluster| object(cluster.get("across")))
        .and_then(|across| across.get(figure).cloned())
        .unwrap_or(Value::None)
}

#[test]
fn a_pass_that_finishes_a_transaction_publishes_none_left() {
    let (db, _) = left_behind(&[Decision::Pending]);
    db.settle_across().unwrap();
    let held = db.store().health().unwrap();
    assert_eq!(
        (held.across_pending, held.across_with_intents),
        (Some(0), Some(0))
    );
}
