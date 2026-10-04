use tessari_encoding::{
    Decision, Participant, TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_types::{NamespaceId, Reach, Sequence};

use super::{Recovery, recover_staging};
use crate::session::across::{AcrossAnswer, AcrossAsk};

const TRANSACTION: TransactionId = TransactionId::new([5; TRANSACTION_ID_LEN]);

fn range(id: u32) -> Reach {
    Reach::Namespace(NamespaceId::new(id))
}

/// A staging record over ranges 1 (the coordinator's, its prepare known) and
/// 2 and 3, whose prepares are not.
fn staging() -> TransactionRecord {
    TransactionRecord {
        decision: Decision::Staging,
        deadline: 9,
        participants: vec![
            Participant {
                range: range(1),
                prepared_at: Some(Sequence::new(4)),
            },
            Participant {
                range: range(2),
                prepared_at: None,
            },
            Participant {
                range: range(3),
                prepared_at: None,
            },
        ],
    }
}

#[test]
fn every_part_landed_commits_naming_where_each_landed() -> Result<(), String> {
    let mut asked = Vec::new();
    let recovered = recover_staging(TRANSACTION, &staging(), true, |to, ask| {
        asked.push((to, ask.clone()));
        Ok(AcrossAnswer::Landed(Some(Sequence::new(10))))
    });
    let Recovery::Committed(record) = recovered else {
        return Err(format!("every part landed: {recovered:?}"));
    };
    assert_eq!(record.decision, Decision::Committed);
    let landed: Vec<_> = record
        .participants
        .iter()
        .map(|participant| participant.prepared_at)
        .collect();
    assert_eq!(
        landed,
        [
            Some(Sequence::new(4)),
            Some(Sequence::new(10)),
            Some(Sequence::new(10))
        ]
    );
    // A part already known landed is not asked about again; each other one is
    // asked of its own range, as the bar it may write.
    assert_eq!(
        asked,
        [
            (
                range(2),
                AcrossAsk::Bar {
                    transaction: TRANSACTION,
                    range: range(2),
                    prevent: true
                }
            ),
            (
                range(3),
                AcrossAsk::Bar {
                    transaction: TRANSACTION,
                    range: range(3),
                    prevent: true
                }
            ),
        ]
    );
    Ok(())
}

#[test]
fn a_part_not_landed_is_barred_when_asked_to_and_only_missing_otherwise() {
    let answer = |to: Reach| {
        Ok(AcrossAnswer::Landed(
            (to == range(2)).then_some(Sequence::new(10)),
        ))
    };
    assert_eq!(
        recover_staging(TRANSACTION, &staging(), true, |to, _| answer(to)),
        Recovery::Barred(range(3))
    );
    assert_eq!(
        recover_staging(TRANSACTION, &staging(), false, |to, _| answer(to)),
        Recovery::Missing(range(3))
    );
}

#[test]
fn a_part_nobody_could_answer_for_leaves_the_outcome_unknown() {
    let recovered = recover_staging(TRANSACTION, &staging(), true, |to, _| {
        if to == range(2) {
            Err("the link fell".to_owned())
        } else {
            Ok(AcrossAnswer::Landed(None))
        }
    });
    assert!(
        matches!(&recovered, Recovery::Unknown(why) if why.contains("the link fell")),
        "{recovered:?}"
    );
    // An answer of another kind is a mismatch between builds, not a verdict.
    let recovered = recover_staging(TRANSACTION, &staging(), true, |_, _| {
        Ok(AcrossAnswer::Holding(true))
    });
    assert!(matches!(recovered, Recovery::Unknown(_)), "{recovered:?}");
}
