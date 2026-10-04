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

#[test]
fn a_part_key_round_trips_for_every_kind_of_range() -> Result<()> {
    let transaction = TransactionId::new([7; TRANSACTION_ID_LEN]);
    for range in [
        Reach::Store,
        Reach::Namespace(NamespaceId::new(2)),
        Reach::Database(NamespaceId::new(2), tessari_types::DatabaseId::new(3)),
        Reach::Shard(
            NamespaceId::new(2),
            tessari_types::DatabaseId::new(3),
            tessari_types::TableId::new(4),
            tessari_types::ShardId::new(5),
        ),
    ] {
        let key = AcrossPartKey { transaction, range };
        let bytes = key.encode();
        assert_eq!(bytes.as_slice()[0], 0x52);
        assert_eq!(AcrossPartKey::decode(bytes.as_slice())?, key);
    }
    Ok(())
}

#[test]
fn a_barred_part_key_round_trips_beside_the_landed_one() -> Result<()> {
    let transaction = TransactionId::new([7; TRANSACTION_ID_LEN]);
    let range = Reach::Namespace(NamespaceId::new(2));
    let barred = AcrossBarredKey { transaction, range };
    let bytes = barred.encode();
    assert_eq!(bytes.as_slice()[0], 0x54);
    assert_eq!(AcrossBarredKey::decode(bytes.as_slice())?, barred);
    // Its own kind: a barred part never reads as a landed one.
    assert!(AcrossPartKey::decode(bytes.as_slice()).is_err());
    Ok(())
}

#[test]
fn an_unsettled_table_key_round_trips_and_groups_by_table() -> Result<()> {
    let key = |table: u32, fill: u8| AcrossUnsettledKey {
        table: tessari_types::TableId::new(table),
        transaction: TransactionId::new([fill; TRANSACTION_ID_LEN]),
    };
    for held in [key(3, 0x01), key(3, 0xfe)] {
        let bytes = held.encode();
        assert_eq!(bytes.as_slice()[0], 0x53);
        assert_eq!(AcrossUnsettledKey::decode(bytes.as_slice())?, held);
        // Every transaction unsettled in one table shares the table's prefix,
        // which is the one seek a read asks with.
        assert!(bytes.as_slice().starts_with(&AcrossUnsettledKey::prefix_of(
            tessari_types::TableId::new(3)
        )));
    }
    assert!(key(3, 0xfe).encode().as_slice() < key(4, 0x01).encode().as_slice());
    Ok(())
}
