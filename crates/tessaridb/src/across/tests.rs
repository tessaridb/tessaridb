#![allow(clippy::unwrap_used)]

use tessari_encoding::{
    Mutation, Participant, RecordValue, StampedValue, TRANSACTION_ID_LEN, TransactionId,
    TransactionRecord, decode_payload, encode_payload,
};
use tessari_session::{AcrossAsk, Session};
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
    session
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
    for decision in decisions {
        session
            .answer_across(&AcrossAsk::Decide {
                transaction: TRANSACTION,
                record: TransactionRecord {
                    decision: *decision,
                    deadline: 0,
                    participants: vec![Participant {
                        range: home,
                        prepared_at: None,
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
