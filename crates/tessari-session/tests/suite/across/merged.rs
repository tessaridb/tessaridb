//! The coordinator's range asked for two records rather than four (ADR-0112
//! D13a, D13b): a begin, answered as a prepare, and a conclusion, answered as
//! a decision — each judged as the user the asking node verified.

use tessari_encoding::Decision;
use tessari_session::{AcrossAnswer, AcrossAsk, Error};
use tessari_storage::{Reach, RecordAddress};
use tessari_types::{RecordId, Value};

use super::{TRANSACTION, ids, note, prepare, record, signed_in, store};

fn begin(store: &tessari_storage::Store, table: &str) -> AcrossAsk {
    let AcrossAsk::Prepare { seen, writes, .. } = prepare(store, table) else {
        unreachable!("prepare builds a prepare")
    };
    AcrossAsk::Begin {
        transaction: TRANSACTION,
        record: tessari_encoding::TransactionRecord {
            participants: record(store, Decision::Pending)
                .participants
                .into_iter()
                .map(|participant| tessari_encoding::Participant {
                    prepared_at: None,
                    ..participant
                })
                .collect(),
            ..record(store, Decision::Pending)
        },
        seen,
        writes,
    }
}

fn conclude(store: &tessari_storage::Store, decision: Decision) -> AcrossAsk {
    let (namespace, database, table) = ids(store, "notes");
    AcrossAsk::Conclude {
        transaction: TRANSACTION,
        record: record(store, decision),
        records: vec![RecordAddress::new(
            namespace,
            database,
            table,
            RecordId::Int(1),
        )],
    }
}

#[test]
fn a_begun_write_is_the_value_only_once_concluded() {
    let store = store();
    let mut owner = signed_in(&store, "root");
    let begun = owner.answer_across(&begin(&store, "notes")).unwrap();
    assert!(matches!(begun, AcrossAnswer::Prepared(_)), "{begun:?}");
    assert_eq!(note(&store), Value::from("old"), "an intent is not a value");
    assert_eq!(
        store
            .transaction_record(TRANSACTION)
            .unwrap()
            .map(|standing| standing.decision),
        Some(Decision::Pending)
    );
    let concluded = owner
        .answer_across(&conclude(&store, Decision::Committed))
        .unwrap();
    assert!(
        matches!(concluded, AcrossAnswer::Decided(_)),
        "{concluded:?}"
    );
    assert_eq!(note(&store), Value::from("new"));
    assert!(!store.holds_intents_of(TRANSACTION).unwrap());
}

#[test]
fn a_user_without_the_write_grant_cannot_begin() {
    let store = store();
    let refused = signed_in(&store, "reader").answer_across(&begin(&store, "notes"));
    assert!(
        matches!(refused, Err(Error::NotGranted { .. })),
        "{refused:?}"
    );
    assert_eq!(store.transaction_record(TRANSACTION).unwrap(), None);
}

#[test]
fn a_begin_whose_record_a_participant_aborted_is_refused() {
    let store = store();
    let mut owner = signed_in(&store, "root");
    // A participant holding a prepare that outran the begin found the record
    // absent and settled it aborted (D7).
    let (namespace, database, _) = ids(&store, "notes");
    owner
        .answer_across(&AcrossAsk::Settle {
            transaction: TRANSACTION,
            coordinator: Reach::Database(namespace, database),
        })
        .unwrap();
    let refused = owner.answer_across(&begin(&store, "notes"));
    assert!(
        matches!(
            &refused,
            Err(Error::Store(tessari_storage::Error::AcrossDecided {
                decided: "aborted"
            }))
        ),
        "{refused:?}"
    );
    assert_eq!(note(&store), Value::from("old"));
    assert!(!store.holds_intents_of(TRANSACTION).unwrap());
}
