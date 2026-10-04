use super::*;

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
