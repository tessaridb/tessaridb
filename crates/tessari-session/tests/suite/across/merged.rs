//! The coordinator's range asked for two records rather than four (ADR-0112
//! D13a, D13b): a begin, answered as a prepare, and a conclusion, answered as
//! a decision — each judged as the user the asking node verified.

use tessari_encoding::Decision;
use tessari_session::{AcrossAnswer, AcrossAsk, Error};
use tessari_storage::{Reach, RecordAddress};
use tessari_types::{RecordId, Value};

use super::{TRANSACTION, ids, note, prepare, record, signed_in, store};

pub(super) fn begin(store: &tessari_storage::Store, table: &str) -> AcrossAsk {
    let AcrossAsk::Prepare { seen, writes, .. } = prepare(store, table) else {
        unreachable!("prepare builds a prepare")
    };
    AcrossAsk::Begin {
        transaction: TRANSACTION,
        record: tessari_encoding::TransactionRecord {
            participants: record(store, Decision::Staging)
                .participants
                .into_iter()
                .map(|participant| tessari_encoding::Participant {
                    prepared_at: None,
                    ..participant
                })
                .collect(),
            ..record(store, Decision::Staging)
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
        Some(Decision::Staging)
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

#[test]
fn a_staging_record_is_answered_as_it_stands_and_never_aborted() {
    // Status recovery decides a staging record, never a settle or a reader's
    // lookup, however late (ADR-0112 D14c, D14e).
    let store = store();
    let mut owner = signed_in(&store, "root");
    owner.answer_across(&begin(&store, "notes")).unwrap();
    let (namespace, database, _) = ids(&store, "notes");
    let coordinator = Reach::Database(namespace, database);
    for asked in [
        AcrossAsk::Settle {
            transaction: TRANSACTION,
            coordinator,
        },
        AcrossAsk::Lookup {
            transaction: TRANSACTION,
            coordinator,
        },
    ] {
        let answered = owner.answer_across(&asked).unwrap();
        assert!(
            matches!(&answered, AcrossAnswer::Outcome(standing) if standing.decision == Decision::Staging),
            "{asked:?} → {answered:?}"
        );
    }
    assert_eq!(
        store
            .transaction_record(TRANSACTION)
            .unwrap()
            .map(|standing| standing.decision),
        Some(Decision::Staging),
        "nothing aborted it, its deadline of zero long past"
    );
}

#[test]
fn a_part_is_reported_where_it_landed_and_barred_where_it_did_not() {
    let store = store();
    let mut owner = signed_in(&store, "root");
    let (namespace, database, _) = ids(&store, "notes");
    let bar = |prevent| AcrossAsk::Bar {
        transaction: TRANSACTION,
        range: Reach::Database(namespace, database),
        prevent,
    };
    // Only asked: reported missing, and nothing barred.
    assert_eq!(
        owner.answer_across(&bar(false)).unwrap(),
        AcrossAnswer::Landed(None)
    );
    let prepared = owner
        .answer_across(&super::prepare(&store, "notes"))
        .unwrap();
    let AcrossAnswer::Prepared(at) = prepared else {
        panic!("{prepared:?}");
    };
    assert!(
        matches!(
            owner.answer_across(&bar(true)).unwrap(),
            AcrossAnswer::Landed(Some(_))
        ),
        "a landed part cannot be barred"
    );
    assert!(at.get() > 0);
    assert!(
        store.holds_intents_of(TRANSACTION).unwrap(),
        "the prepare stands"
    );
}

#[test]
fn a_barred_part_refuses_the_prepare_that_arrives_after_it() {
    let store = store();
    let mut owner = signed_in(&store, "root");
    let (namespace, database, _) = ids(&store, "notes");
    let barred = owner
        .answer_across(&AcrossAsk::Bar {
            transaction: TRANSACTION,
            range: Reach::Database(namespace, database),
            prevent: true,
        })
        .unwrap();
    assert_eq!(barred, AcrossAnswer::Landed(None));
    let refused = owner.answer_across(&super::prepare(&store, "notes"));
    assert!(
        matches!(
            &refused,
            Err(Error::Store(tessari_storage::Error::AcrossDecided {
                decided: "barred"
            }))
        ),
        "{refused:?}"
    );
    assert_eq!(note(&store), Value::from("old"));
}
