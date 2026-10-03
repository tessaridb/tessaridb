use tessari_encoding::{
    Decision, Participant, TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, TableId};

use super::*;

const TRANSACTION: TransactionId = TransactionId::new([8; TRANSACTION_ID_LEN]);

fn address(id: i64) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        RecordId::Int(id),
    )
}

#[test]
fn every_request_travels_and_arrives_as_itself() -> Result<(), String> {
    let range = Reach::Database(NamespaceId::new(1), DatabaseId::new(2));
    let asks = [
        AcrossAsk::Prepare {
            transaction: TRANSACTION,
            coordinator: range,
            seen: Sequence::new(41),
            writes: vec![named(&address(1)), named(&address(2))],
        },
        AcrossAsk::Decide {
            transaction: TRANSACTION,
            record: TransactionRecord {
                decision: Decision::Committed,
                deadline: 7,
                participants: vec![Participant {
                    range,
                    prepared_at: Some(Sequence::new(3)),
                }],
            },
        },
        AcrossAsk::Resolve {
            transaction: TRANSACTION,
            committed: false,
            records: vec![address(1), address(2)],
        },
        AcrossAsk::Resolve {
            transaction: TRANSACTION,
            committed: true,
            records: Vec::new(),
        },
        AcrossAsk::Settle {
            transaction: TRANSACTION,
            coordinator: range,
        },
    ];
    for ask in asks {
        assert_eq!(AcrossAsk::decode(&ask.encode())?, ask);
    }
    Ok(())
}

#[test]
fn every_answer_travels_and_arrives_as_itself() -> Result<(), String> {
    for answer in [
        AcrossAnswer::Prepared(Sequence::new(5)),
        AcrossAnswer::Decided(Sequence::new(6)),
        AcrossAnswer::Resolved(Some(Sequence::new(7))),
        AcrossAnswer::Resolved(None),
        AcrossAnswer::Outcome(Decision::Pending),
        AcrossAnswer::Outcome(Decision::Committed),
        AcrossAnswer::Outcome(Decision::Aborted),
    ] {
        assert_eq!(AcrossAnswer::decode(&answer.encode())?, answer);
    }
    Ok(())
}

#[test]
fn a_plain_log_record_or_a_cut_answer_is_refused() {
    let mut plain = vec![ASK_PREPARE, 0, 0, 0, 0, 0, 0, 0, 0];
    plain.extend_from_slice(LogRecord::new(vec![named(&address(1))]).encode().as_slice());
    assert!(AcrossAsk::decode(&plain).is_err(), "names no transaction");
    assert!(
        AcrossAsk::decode(&[0, 1, 2]).is_err(),
        "shorter than a position"
    );
    assert!(AcrossAnswer::decode(&[PREPARED, 0, 0]).is_err());
    assert!(AcrossAnswer::decode(&[0x7f, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
}
