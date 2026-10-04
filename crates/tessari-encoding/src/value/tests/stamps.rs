use super::*;

#[test]
fn a_stamped_version_round_trips_both_halves() {
    let stamp = a_stamp(&[(ONE_NODE, 3), (ANOTHER_NODE, 1)]);
    let value = StampedValue::stamped(stamp.clone(), RecordValue::Present(b"payload".to_vec()));
    let decoded = StampedValue::decode(value.encode().as_slice()).unwrap();
    assert_eq!(&decoded, &value);
    assert_eq!(decoded.stamp(), &stamp);
    assert_eq!(decoded.value().payload(), b"payload");
}

/// A delete racing a write is one of the cases a multi-master range has to
/// name, so the stamp has to survive on a version that carries no payload.
/// This is the case a third enum variant could not have expressed (Q-638).
#[test]
fn a_tombstone_carries_a_stamp_too() {
    let stamp = a_stamp(&[(ANOTHER_NODE, 2)]);
    let value = StampedValue::stamped(stamp.clone(), RecordValue::Tombstone);
    let decoded = StampedValue::decode(value.encode().as_slice()).unwrap();
    assert!(decoded.value().is_tombstone());
    assert_eq!(decoded.stamp(), &stamp);
}

/// The bump is a promise that nothing already written is rewritten, and this
/// is what makes the promise checkable rather than asserted.
#[test]
fn an_unstamped_version_encodes_exactly_as_it_did_before_the_stamp() {
    for value in [
        RecordValue::Present(b"payload".to_vec()),
        RecordValue::Present(Vec::new()),
        RecordValue::Tombstone,
    ] {
        let before = value.encode();
        let after = StampedValue::new(value.clone()).encode();
        assert_eq!(after.as_slice(), before.as_slice());
        let decoded = StampedValue::decode(before.as_slice()).unwrap();
        assert_eq!(decoded.value(), &value);
        assert!(decoded.stamp().is_empty());
    }
}

/// Both readers of a stamped value have to agree, and the older one cannot
/// agree by guessing: it has never seen bit 2, so it refuses loudly instead
/// of returning a version stripped of the context that says who saw what.
#[test]
fn the_unstamped_reader_refuses_a_stamped_value_rather_than_dropping_it() {
    let encoded = StampedValue::stamped(
        a_stamp(&[(ONE_NODE, 1)]),
        RecordValue::Present(b"payload".to_vec()),
    )
    .encode();
    let error = RecordValue::decode(encoded.as_slice()).unwrap_err();
    assert!(matches!(error, Error::ReservedFlags { flags } if flags & FLAG_STAMP != 0));
}

/// Every read of a stamp binary-searches its entries, so an unordered list
/// answers the wrong node's count instead of failing. The decoder is the
/// only place the invariant can break, so it is the only place that checks.
#[test]
fn entries_out_of_node_order_are_refused_rather_than_sorted() {
    let mut bytes = vec![CODEC_VERSION, FLAG_STAMP];
    bytes.extend_from_slice(&2_u32.to_be_bytes());
    for node in [ANOTHER_NODE, ONE_NODE] {
        bytes.extend_from_slice(&node);
        bytes.extend_from_slice(&1_u64.to_be_bytes());
    }
    assert!(matches!(
        StampedValue::decode(&bytes).unwrap_err(),
        Error::StampOutOfOrder { at: 1 }
    ));
}

#[test]
fn a_stamp_cut_short_of_its_own_entry_count_is_truncated_not_short() {
    let mut bytes = vec![CODEC_VERSION, FLAG_STAMP];
    bytes.extend_from_slice(&2_u32.to_be_bytes());
    bytes.extend_from_slice(&ONE_NODE);
    bytes.extend_from_slice(&1_u64.to_be_bytes());
    assert!(matches!(
        StampedValue::decode(&bytes).unwrap_err(),
        Error::ValueTruncated { .. }
    ));
}

/// The log is the first of S1.3's three carriers, and it carries the stamp
/// by carrying the version — there is no second field to keep in step.
#[test]
fn a_stamp_survives_the_log_record_that_carries_the_version() {
    let stamp = a_stamp(&[(ONE_NODE, 7), (ANOTHER_NODE, 2)]);
    let record = LogRecord::new(vec![Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table: TableId::new(1),
        id: RecordId::from("a"),
        shard: None,
        value: StampedValue::stamped(stamp.clone(), RecordValue::Present(b"v".to_vec())),
    }]);
    let encoded = record.encode();
    let decoded = LogRecord::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(decoded.mutations()[0].value.stamp(), &stamp);
    assert_eq!(
        LogRecord::decode(encoded.as_slice())
            .unwrap()
            .encode()
            .as_slice(),
        encoded.as_slice()
    );
}
