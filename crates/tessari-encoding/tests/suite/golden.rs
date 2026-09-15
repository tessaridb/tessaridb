//! Golden bytes.
//!
//! A codec tested only against itself passes every test after its meaning
//! changes: encode-then-decode still round-trips, and every record written by
//! the previous release now decodes into something else. These fixtures are
//! written out literally, so a change to the format has to change this file —
//! which makes it a decision rather than an accident.
//!
//! When the format legitimately changes, the rule is additive: a new codec
//! version, both decoders live, and these fixtures stay as the older version's
//! evidence until a sweep proves no such records remain.

#![allow(clippy::unwrap_used)]

use tessari_encoding::{
    AppliedPositionKey, CODEC_VERSION, FormatVersion, FormatVersionKey, KeyKind, LogId, RecordKey,
    RecordValue, StoreKey, StoreValue, TABLE_PREFIX_LEN, Writer,
};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, TableId};

fn key_bytes(id: RecordId, version: u64) -> Vec<u8> {
    RecordKey::new(
        NamespaceId::new(0x0102_0304),
        DatabaseId::new(0x0506_0708),
        TableId::new(0x090a_0b0c),
        id,
        Sequence::new(version),
    )
    .encode()
    .into_bytes()
}

#[test]
fn the_table_prefix_is_thirteen_bytes_of_tag_and_identifiers() {
    let prefix = RecordKey::table_prefix(
        NamespaceId::new(0x0102_0304),
        DatabaseId::new(0x0506_0708),
        TableId::new(0x090a_0b0c),
    );
    assert_eq!(prefix.len(), TABLE_PREFIX_LEN);
    assert_eq!(
        prefix,
        vec![
            0x01, // kind: record
            0x01, 0x02, 0x03, 0x04, // namespace
            0x05, 0x06, 0x07, 0x08, // database
            0x09, 0x0a, 0x0b, 0x0c, // table
        ]
    );
}

#[test]
fn a_record_key_with_an_integer_id_matches_its_fixture() {
    assert_eq!(
        key_bytes(RecordId::Int(1), 1),
        vec![
            0x01, // kind: record
            0x01, 0x02, 0x03, 0x04, // namespace
            0x05, 0x06, 0x07, 0x08, // database
            0x09, 0x0a, 0x0b, 0x0c, // table
            0x01, // record-id discriminant: int
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // 1, sign-flipped
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, // !1 — version 1
        ]
    );
}

#[test]
fn a_negative_integer_id_sits_below_zero() {
    let negative = key_bytes(RecordId::Int(-1), 0);
    let zero = key_bytes(RecordId::Int(0), 0);
    assert_eq!(
        &negative[13..22],
        &[0x01, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    );
    assert_eq!(
        &zero[13..22],
        &[0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert!(negative < zero);
}

#[test]
fn a_text_id_is_escaped_and_terminated() {
    assert_eq!(
        key_bytes(RecordId::from("a\u{0}b"), 0),
        vec![
            0x01, // kind: record
            0x01, 0x02, 0x03, 0x04, // namespace
            0x05, 0x06, 0x07, 0x08, // database
            0x09, 0x0a, 0x0b, 0x0c, // table
            0x02, // record-id discriminant: text
            0x61, // 'a'
            0x00, 0xff, // escaped NUL
            0x62, // 'b'
            0x00, 0x01, // terminator
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // !0 — version 0
        ]
    );
}

#[test]
fn a_uuid_id_is_sixteen_raw_bytes_with_no_terminator() {
    let bytes = key_bytes(RecordId::Uuid([0xab; 16]), 0);
    assert_eq!(bytes.len(), TABLE_PREFIX_LEN + 1 + 16 + 8);
    assert_eq!(bytes[TABLE_PREFIX_LEN], 0x03);
    assert_eq!(&bytes[14..30], &[0xab; 16]);
}

#[test]
fn singleton_meta_keys_match_their_tags() {
    assert_eq!(
        FormatVersionKey.encode().into_bytes(),
        vec![KeyKind::FormatVersion.tag()]
    );
    assert_eq!(FormatVersionKey.encode().into_bytes(), vec![0x30]);
    // The applied position stopped being a singleton when the log became
    // per-range, and stopped being one per home when a range gained two
    // writers: the tag now leads nine bytes of home and sixteen of writer.
    // `Reach::Store` writes the variant and two zeroed identifiers, which is the
    // home every record in a store written before the first of those changes
    // belongs to; `Writer::UNATTRIBUTED` is sixteen zeroes, which is what the
    // second migration attributes them to.
    let mut store_log = vec![0x31, 0x00, 0, 0, 0, 0, 0, 0, 0, 0];
    store_log.extend_from_slice(&[0; 16]);
    assert_eq!(
        AppliedPositionKey::new(LogId::unattributed(Reach::Store))
            .encode()
            .into_bytes(),
        store_log
    );
    let mut database_log = vec![0x31, 0x02, 0, 0, 0, 1, 0, 0, 0, 2];
    database_log.extend_from_slice(&[0xcd; 16]);
    assert_eq!(
        AppliedPositionKey::new(LogId::new(
            Reach::Database(NamespaceId::new(1), DatabaseId::new(2)),
            Writer::new([0xcd; 16]),
        ))
        .encode()
        .into_bytes(),
        database_log
    );
}

#[test]
fn stored_values_match_their_fixtures() {
    assert_eq!(
        RecordValue::Present(b"hi".to_vec()).encode().into_bytes(),
        vec![0x01, 0x00, b'h', b'i']
    );
    assert_eq!(
        RecordValue::Tombstone.encode().into_bytes(),
        vec![0x01, 0x01]
    );
    // A NAMED version rather than `CURRENT`, because `CURRENT` moves by design
    // and a fixture that moves with it pins nothing. This one is the on-disk
    // layout a store has to keep being able to recognise, so its bytes are the
    // thing worth freezing.
    assert_eq!(
        FormatVersion::HOMED_LOG.encode().into_bytes(),
        vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x03]
    );
    assert_eq!(
        Sequence::new(258).encode().into_bytes(),
        vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02]
    );
}

#[test]
fn the_first_byte_of_every_stored_value_is_the_codec_version() {
    assert_eq!(CODEC_VERSION, 1);
    let values = [
        RecordValue::Present(Vec::new()).encode(),
        RecordValue::Tombstone.encode(),
        FormatVersion::CURRENT.encode(),
        Sequence::ZERO.encode(),
    ];
    for value in values {
        assert_eq!(value.as_slice()[0], CODEC_VERSION);
    }
}

#[test]
fn fixtures_still_decode_to_what_they_were_written_as() {
    // The other direction: the bytes above are the contract, so decoding them
    // must yield the original values, not merely something self-consistent.
    let key = RecordKey::decode(&key_bytes(RecordId::from("a\u{0}b"), 0)).unwrap();
    assert_eq!(key.id, RecordId::from("a\u{0}b"));
    assert_eq!(key.version, Sequence::ZERO);
    assert_eq!(key.namespace, NamespaceId::new(0x0102_0304));

    assert_eq!(
        RecordValue::decode(&[0x01, 0x00, b'h', b'i']).unwrap(),
        RecordValue::Present(b"hi".to_vec())
    );
    assert_eq!(
        RecordValue::decode(&[0x01, 0x01]).unwrap(),
        RecordValue::Tombstone
    );
    // Version 1 rather than CURRENT: these are the bytes a store written before
    // the log record carried an epoch holds, and this build still opens it.
    assert_eq!(
        FormatVersion::decode(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x01]).unwrap(),
        FormatVersion::new(1)
    );
    assert!(FormatVersion::new(1).check_supported().is_ok());
}
