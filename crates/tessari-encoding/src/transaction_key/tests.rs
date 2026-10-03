use super::*;
use crate::error::Error;
use crate::value::{Decision, Participant, StoreValue};
use tessari_types::{NamespaceId, Reach, Sequence};

#[test]
fn the_key_round_trips_and_sorts_by_the_id_bytes() -> Result<()> {
    let low = TransactionRecordKey {
        transaction: TransactionId::new([0x01; TRANSACTION_ID_LEN]),
    };
    let high = TransactionRecordKey {
        transaction: TransactionId::new([0xfe; TRANSACTION_ID_LEN]),
    };
    for key in [low, high] {
        let bytes = key.encode();
        assert_eq!(bytes.as_slice()[0], 0x50);
        assert_eq!(bytes.as_slice().len(), 17);
        assert_eq!(TransactionRecordKey::decode(bytes.as_slice())?, key);
    }
    assert!(low.encode().as_slice() < high.encode().as_slice());
    Ok(())
}

#[test]
fn a_key_of_another_kind_or_length_is_refused() {
    let mut bytes = TransactionRecordKey {
        transaction: TransactionId::new([3; TRANSACTION_ID_LEN]),
    }
    .encode()
    .as_slice()
    .to_vec();
    bytes.push(0);
    assert!(
        TransactionRecordKey::decode(&bytes).is_err(),
        "trailing byte"
    );
    bytes.truncate(10);
    assert!(TransactionRecordKey::decode(&bytes).is_err(), "truncated");
    bytes[0] = 0x01;
    assert!(
        matches!(
            TransactionRecordKey::decode(&bytes),
            Err(Error::UnexpectedKind { .. })
        ),
        "another kind"
    );
}

#[test]
fn the_record_it_addresses_round_trips() -> Result<()> {
    let record = TransactionRecord {
        decision: Decision::Committed,
        deadline: 42,
        participants: vec![Participant {
            range: Reach::Namespace(NamespaceId::new(5)),
            prepared_at: Some(Sequence::new(9)),
        }],
    };
    assert_eq!(
        TransactionRecord::decode(record.encode().as_slice())?,
        record
    );
    Ok(())
}

#[test]
fn an_intent_key_round_trips_and_one_transaction_is_one_prefix() -> Result<()> {
    use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};
    let of = |transaction: u8, id: RecordId| IntentOfKey {
        transaction: TransactionId::new([transaction; TRANSACTION_ID_LEN]),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        id,
    };
    let mine = [of(5, RecordId::Int(-7)), of(5, RecordId::from("z"))];
    for key in &mine {
        let bytes = key.encode();
        assert_eq!(&IntentOfKey::decode(bytes.as_slice())?, key);
        assert!(
            bytes
                .as_slice()
                .starts_with(&IntentOfKey::prefix_of(key.transaction))
        );
        assert!(bytes.as_slice().starts_with(&IntentOfKey::prefix()));
    }
    let other = of(6, RecordId::Int(-7)).encode();
    assert!(
        !other
            .as_slice()
            .starts_with(&IntentOfKey::prefix_of(mine[0].transaction))
    );
    Ok(())
}
