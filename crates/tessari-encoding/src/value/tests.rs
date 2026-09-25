#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;

const ONE_NODE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const ANOTHER_NODE: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

fn a_stamp(nodes: &[([u8; NODE_ID_LEN], u64)]) -> CausalStamp {
    let mut stamp = CausalStamp::new();
    for (node, times) in nodes {
        for _ in 0..*times {
            stamp.advance(*node);
        }
    }
    stamp
}

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

#[test]
fn a_present_record_round_trips_its_payload() {
    let value = RecordValue::Present(b"payload".to_vec());
    let encoded = value.encode();
    assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
    assert_eq!(RecordValue::decode(encoded.as_slice()).unwrap(), value);
}

#[test]
fn a_tombstone_is_a_version_with_no_payload() {
    let encoded = RecordValue::Tombstone.encode();
    assert_eq!(encoded.as_slice(), &[CODEC_VERSION, FLAG_TOMBSTONE]);
    let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
    assert!(decoded.is_tombstone());
    assert!(decoded.payload().is_empty());
}

#[test]
fn an_empty_present_record_is_not_a_tombstone() {
    let encoded = RecordValue::Present(Vec::new()).encode();
    let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
    assert!(!decoded.is_tombstone());
}

#[test]
fn an_unknown_codec_version_is_refused_rather_than_guessed() {
    let error = RecordValue::decode(&[9, 0]).unwrap_err();
    assert!(matches!(
        error,
        Error::UnsupportedCodecVersion {
            found: 9,
            supported: CODEC_VERSION
        }
    ));
    assert_eq!(error.code(), "incompatible");
}

#[test]
fn a_reserved_flag_bit_is_refused() {
    let error = RecordValue::decode(&[CODEC_VERSION, 0b0000_0010]).unwrap_err();
    assert!(matches!(error, Error::ReservedFlags { flags: 0b10 }));
}

#[test]
fn a_tombstone_bit_on_a_meta_value_is_reserved_there() {
    let error = FormatVersion::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0, 0, 0, 1]).unwrap_err();
    assert!(matches!(error, Error::ReservedFlags { .. }));
}

#[test]
fn a_tombstone_carrying_payload_contradicts_itself() {
    let error = RecordValue::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0xAA]).unwrap_err();
    assert!(matches!(error, Error::TombstoneWithPayload { len: 1 }));
}

#[test]
fn a_value_shorter_than_its_header_is_truncated() {
    assert!(matches!(
        RecordValue::decode(&[CODEC_VERSION]).unwrap_err(),
        Error::ValueTruncated { len: 1, needed: 2 }
    ));
}

#[test]
fn the_format_version_round_trips_and_refuses_newer_stores() {
    let encoded = FormatVersion::CURRENT.encode();
    assert_eq!(
        FormatVersion::decode(encoded.as_slice()).unwrap(),
        FormatVersion::CURRENT
    );
    assert!(FormatVersion::CURRENT.check_supported().is_ok());
    // Derived from CURRENT rather than written as a literal: a version this
    // test restates is a version this test stops checking the moment the
    // format moves.
    match FormatVersion::new(99).check_supported().unwrap_err() {
        Error::UnsupportedFormatVersion { found, supported } => {
            assert_eq!(found, 99);
            assert_eq!(supported, FormatVersion::CURRENT.get());
        }
        other => panic!("expected an unsupported-format error, got {other:?}"),
    }
}

#[test]
fn a_sequence_round_trips_as_a_stored_value() {
    let sequence = Sequence::new(1_234_567);
    let encoded = sequence.encode();
    assert_eq!(Sequence::decode(encoded.as_slice()).unwrap(), sequence);
}

fn mutation(id: RecordId, value: RecordValue) -> Mutation {
    Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        id,
        shard: None,
        value: StampedValue::new(value),
    }
}

#[test]
fn a_log_record_round_trips_every_mutation_shape() {
    let record = LogRecord::new(vec![
        mutation(RecordId::Int(-42), RecordValue::Present(b"a".to_vec())),
        mutation(RecordId::from("text"), RecordValue::Tombstone),
        mutation(RecordId::Uuid([0x5a; 16]), RecordValue::Present(Vec::new())),
        mutation(
            RecordId::Bytes(vec![0x00, 0xff, 0x00]),
            RecordValue::Present(vec![0x00; 300]),
        ),
    ]);
    let encoded = record.encode();
    assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
    assert_eq!(LogRecord::decode(encoded.as_slice()).unwrap(), record);
}

#[test]
fn an_empty_log_record_round_trips_as_the_header_alone() {
    let record = LogRecord::new(Vec::new());
    assert!(record.mutations().is_empty());
    let encoded = record.encode();
    assert_eq!(encoded.as_slice(), &[CODEC_VERSION, 0]);
    assert_eq!(LogRecord::decode(encoded.as_slice()).unwrap(), record);
}

#[test]
fn mutations_are_stored_in_address_order_whatever_order_they_arrive_in() {
    // Byte-identical replay depends on the encoder being deterministic, not
    // only on the apply path being deterministic. A caller that hands over
    // an unordered collection must not be able to change the bytes.
    let ordered = LogRecord::new(vec![
        mutation(RecordId::from("a"), RecordValue::Tombstone),
        mutation(RecordId::from("b"), RecordValue::Tombstone),
        mutation(RecordId::from("c"), RecordValue::Tombstone),
    ]);
    let shuffled = LogRecord::new(vec![
        mutation(RecordId::from("c"), RecordValue::Tombstone),
        mutation(RecordId::from("a"), RecordValue::Tombstone),
        mutation(RecordId::from("b"), RecordValue::Tombstone),
    ]);
    assert_eq!(ordered, shuffled);
    assert_eq!(
        ordered.encode().as_slice(),
        shuffled.encode().as_slice(),
        "the same mutation set must encode to the same bytes"
    );
}

#[test]
fn mutations_sort_by_the_whole_address_and_not_only_by_record_id() {
    let record = LogRecord::new(vec![
        Mutation {
            namespace: NamespaceId::new(2),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("a"),
            shard: None,
            value: StampedValue::new(RecordValue::Tombstone),
        },
        Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("z"),
            shard: None,
            value: StampedValue::new(RecordValue::Tombstone),
        },
    ]);
    assert_eq!(record.mutations()[0].namespace, NamespaceId::new(1));
}

#[test]
fn a_log_record_truncated_mid_mutation_is_refused_rather_than_half_decoded() {
    let record = LogRecord::new(vec![mutation(
        RecordId::from("r"),
        RecordValue::Present(b"payload".to_vec()),
    )]);
    let full = record.encode();
    let bytes = full.as_slice();
    // From one byte past the header: a cut exactly at the header is not a
    // truncated record, it is an empty one, and that is legitimate.
    for cut in HEADER_LEN.saturating_add(1)..bytes.len() {
        assert!(
            LogRecord::decode(&bytes[..cut]).is_err(),
            "a record cut at {cut} decoded anyway"
        );
    }
}

#[test]
fn a_value_length_naming_more_bytes_than_exist_is_refused() {
    // The length comes out of the payload, so it is not trusted.
    let mut bytes = LogRecord::new(vec![mutation(
        RecordId::from("r"),
        RecordValue::Present(b"x".to_vec()),
    )])
    .encode()
    .into_bytes();
    let last = bytes.len().saturating_sub(3);
    bytes[last] = 0xff;
    assert!(LogRecord::decode(&bytes).is_err());
}

#[test]
fn a_record_at_the_first_leadership_is_byte_identical_to_one_written_before_epochs_existed() {
    // The whole point of spending a flag bit rather than widening every
    // record: a store that has never elected anybody keeps the bytes it
    // already has, so no existing log entry is rewritten and byte-identical
    // replay across builds survives the format change.
    let record = LogRecord::new(vec![mutation(
        RecordId::from("r"),
        RecordValue::Present(b"v".to_vec()),
    )]);
    assert_eq!(record.epoch(), Epoch::ZERO);
    let bytes = record.encode().into_bytes();
    assert_eq!(bytes[1], 0, "no flag bit is set at the first leadership");
    assert_eq!(
        LogRecord::decode(&bytes).unwrap(),
        record,
        "and it decodes back to itself"
    );
}

#[test]
fn a_record_carries_its_epoch_through_a_round_trip() {
    let record = LogRecord::at(
        Epoch::new(0x0102_0304_0506_0708),
        vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"v".to_vec()),
        )],
    );
    let decoded = LogRecord::decode(record.encode().as_slice()).unwrap();
    assert_eq!(decoded.epoch(), Epoch::new(0x0102_0304_0506_0708));
    assert_eq!(decoded.mutations(), record.mutations());
    assert_eq!(decoded, record);
}

#[test]
fn the_epoch_sits_in_front_of_the_mutations_so_a_reader_need_not_scan() {
    let bytes = LogRecord::at(
        Epoch::new(0x0102_0304_0506_0708),
        vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"v".to_vec()),
        )],
    )
    .encode()
    .into_bytes();
    assert_eq!(bytes[1], FLAG_EPOCH);
    assert_eq!(
        &bytes[HEADER_LEN..HEADER_LEN + 8],
        &0x0102_0304_0506_0708_u64.to_be_bytes(),
        "fixed width, big-endian, immediately after the header"
    );
}

#[test]
fn a_record_written_before_epochs_existed_decodes_as_the_first_leadership() {
    // Not a round trip: these are bytes as an older build wrote them, with
    // the flags byte clear and no epoch field at all.
    // A literal, not a round trip: these are the bytes an older build
    // wrote — flags clear, the mutations starting immediately after the
    // header — and a round trip against today's encoder could not tell the
    // difference if the decoder silently required an epoch.
    let legacy = [
        CODEC_VERSION,
        0, // flags: no epoch field follows
        0,
        0,
        0,
        1, // namespace
        0,
        0,
        0,
        2, // database
        0,
        0,
        0,
        3, // table
        0x02,
        b'r',
        0x00,
        0x01, // a string record id, terminated
        0,
        0,
        0,
        3, // the value's length
        CODEC_VERSION,
        0,
        b'v', // the value
    ];
    let decoded = LogRecord::decode(&legacy).unwrap();
    assert_eq!(decoded.epoch(), Epoch::ZERO);
    assert_eq!(decoded.mutations().len(), 1);
    assert_eq!(decoded.mutations()[0].id, RecordId::from("r"));
}

/// The bytes an unsharded record has always had (G031 S2.1, the goal's kill
/// criterion).
///
/// Encode direction and a literal, pinned before the shard field existed: a
/// record touching no split table must keep exactly these bytes, or sharding
/// would rewrite every log and backup already on disk. A round trip could not
/// see it — a codec that always wrote the new field would read itself back.
#[test]
fn a_record_touching_no_split_table_keeps_the_bytes_it_always_had() {
    let record = LogRecord::new(vec![mutation(
        RecordId::from("r"),
        RecordValue::Present(b"v".to_vec()),
    )]);
    let golden = [
        CODEC_VERSION,
        0, // flags: no epoch, no shards
        0,
        0,
        0,
        1, // namespace
        0,
        0,
        0,
        2, // database
        0,
        0,
        0,
        3, // table — and no shard after it
        0x02,
        b'r',
        0x00,
        0x01, // a string record id, terminated
        0,
        0,
        0,
        3, // the value's length
        CODEC_VERSION,
        0,
        b'v', // the value
    ];
    assert_eq!(record.encode().as_slice(), &golden[..]);
}

#[test]
fn a_record_carries_each_mutations_shard_through_a_round_trip() {
    let mut split = mutation(RecordId::from("m"), RecordValue::Present(b"v".to_vec()));
    split.shard = Some(ShardId::new(2));
    let mut plain = mutation(RecordId::from("z"), RecordValue::Tombstone);
    plain.table = TableId::new(4);
    let record = LogRecord::at(Epoch::new(9), vec![split, plain]);
    let decoded = LogRecord::decode(record.encode().as_slice()).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(decoded.mutations()[0].shard, Some(ShardId::new(2)));
    assert_eq!(
        decoded.mutations()[1].shard,
        None,
        "a mutation of a table that is not split reads back as none, not as shard 0"
    );
}

#[test]
fn the_shard_sits_after_the_table_and_the_flag_says_it_is_there() {
    let mut split = mutation(RecordId::from("r"), RecordValue::Present(b"v".to_vec()));
    split.shard = Some(ShardId::new(0x0a0b_0c0d));
    let bytes = LogRecord::new(vec![split]).encode().into_bytes();
    assert_eq!(bytes[1], FLAG_SHARDS);
    assert_eq!(
        &bytes[HEADER_LEN + 12..HEADER_LEN + 16],
        &0x0a0b_0c0d_u32.to_be_bytes(),
        "fixed width, big-endian, right after namespace, database and table"
    );
}

#[test]
fn a_record_whose_shard_field_is_cut_short_is_refused() {
    let mut split = mutation(RecordId::from("r"), RecordValue::Present(b"v".to_vec()));
    split.shard = Some(ShardId::new(1));
    let bytes = LogRecord::new(vec![split]).encode().into_bytes();
    assert!(LogRecord::decode(&bytes[..HEADER_LEN + 14]).is_err());
}

/// G034 S1.1 — the writer's commit order survives a round trip, sits after
/// the epoch so the epoch keeps its fixed offset.
#[test]
fn a_record_carries_its_writers_order_after_the_epoch() {
    let written = mutation(RecordId::from("m"), RecordValue::Present(b"v".to_vec()));
    let mut record = LogRecord::at(Epoch::new(3), vec![written]);
    record.set_order(Sequence::new(41));
    let bytes = record.encode().into_bytes();
    assert_eq!(bytes[1], FLAG_EPOCH | FLAG_ORDER);
    assert_eq!(LogRecord::epoch_in(&bytes).unwrap(), Epoch::new(3));
    assert_eq!(
        &bytes[HEADER_LEN + EPOCH_LEN..HEADER_LEN + EPOCH_LEN + ORDER_LEN],
        &41_u64.to_be_bytes()
    );
    assert_eq!(
        LogRecord::order_in(&bytes).unwrap(),
        Some(Sequence::new(41))
    );
    let decoded = LogRecord::decode(&bytes).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(decoded.order(), Some(Sequence::new(41)));
    // And a record without one reads back as none, flag clear.
    let plain = LogRecord::new(Vec::new()).encode().into_bytes();
    assert_eq!(plain[1] & FLAG_ORDER, 0);
    assert_eq!(LogRecord::order_in(&plain).unwrap(), None);
}

#[test]
fn a_record_claiming_an_order_it_did_not_write_is_truncated_not_guessed() {
    let mut bytes = LogRecord::new(Vec::new()).encode().into_bytes();
    bytes[1] = FLAG_ORDER;
    assert!(LogRecord::decode(&bytes).is_err());
    assert!(LogRecord::order_in(&bytes).is_err());
}

#[test]
fn a_record_claiming_an_epoch_it_did_not_write_is_truncated_not_guessed() {
    let mut bytes = LogRecord::new(Vec::new()).encode().into_bytes();
    bytes[1] = FLAG_EPOCH;
    assert!(
        LogRecord::decode(&bytes).is_err(),
        "the flag promises eight bytes that are not there"
    );
}

// ---- a version that expires (G035) ----

#[test]
fn an_expiring_version_round_trips_its_instant_beside_its_stamp() {
    let stamp = a_stamp(&[(ONE_NODE, 3)]);
    let value = StampedValue::stamped(stamp.clone(), RecordValue::Present(b"v".to_vec()))
        .expiring(1_700_000_000_123);
    let decoded = StampedValue::decode(value.encode().as_slice()).unwrap();
    assert_eq!(decoded.expires(), Some(1_700_000_000_123));
    assert_eq!(decoded.stamp(), &stamp);
    assert_eq!(decoded.value(), &RecordValue::Present(b"v".to_vec()));
}

#[test]
fn a_version_that_never_expires_keeps_the_bytes_it_always_had() {
    let value = StampedValue::new(RecordValue::Present(b"v".to_vec()));
    assert_eq!(value.encode().as_slice(), &[CODEC_VERSION, 0, b'v']);
}

#[test]
fn the_instant_sits_between_the_header_and_the_payload() {
    let value = StampedValue::new(RecordValue::Present(b"v".to_vec())).expiring(0x0102);
    assert_eq!(
        value.encode().as_slice(),
        &[CODEC_VERSION, FLAG_EXPIRES, 0, 0, 0, 0, 0, 0, 1, 2, b'v']
    );
}

#[test]
fn a_deletion_drops_an_instant_rather_than_writing_it() {
    let value = StampedValue::new(RecordValue::Tombstone).expiring(5);
    assert_eq!(value.encode().as_slice(), &[CODEC_VERSION, FLAG_TOMBSTONE]);
    assert_eq!(value.expires(), None);
}

#[test]
fn a_deletion_claiming_an_instant_is_refused() {
    let error = StampedValue::decode(&[
        CODEC_VERSION,
        FLAG_TOMBSTONE | FLAG_EXPIRES,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        1,
    ])
    .unwrap_err();
    assert!(matches!(error, Error::ReservedFlags { .. }), "{error:?}");
}

#[test]
fn an_instant_cut_short_is_truncation_not_a_small_number() {
    let error = StampedValue::decode(&[CODEC_VERSION, FLAG_EXPIRES, 0, 0, 1]).unwrap_err();
    assert!(matches!(error, Error::ValueTruncated { .. }), "{error:?}");
}

#[test]
fn a_version_is_gone_at_its_instant_not_after_it() {
    let value = StampedValue::new(RecordValue::Present(b"v".to_vec())).expiring(1_000);
    assert!(!value.is_expired_at(999));
    assert!(value.is_expired_at(1_000));
    assert_eq!(
        value.clone().into_visible_at(999),
        RecordValue::Present(b"v".to_vec())
    );
    assert_eq!(value.into_visible_at(1_000), RecordValue::Tombstone);
}

#[test]
fn a_version_with_no_instant_is_visible_at_every_clock() {
    let value = StampedValue::new(RecordValue::Present(b"v".to_vec()));
    assert!(!value.is_expired_at(u64::MAX));
}
