use super::*;

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

/// A hash `{a: 1, b: 2}` as the store holds its payload.
fn hash_payload() -> Vec<u8> {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("a".to_owned(), tessari_types::Value::from(1_i64));
    fields.insert("b".to_owned(), tessari_types::Value::from(2_i64));
    crate::encode_payload(&tessari_types::Value::Object(fields))
        .as_slice()
        .to_vec()
}

fn fields_of(value: RecordValue) -> Vec<String> {
    match value {
        RecordValue::Present(payload) => match crate::decode_payload(&payload).unwrap() {
            tessari_types::Value::Object(fields) => fields.into_keys().collect(),
            other => vec![format!("not an object: {other:?}")],
        },
        RecordValue::Tombstone => Vec::new(),
    }
}

#[test]
fn a_field_instant_round_trips_beside_the_key_instant() {
    let value = StampedValue::new(RecordValue::Present(hash_payload()))
        .expiring(9_000)
        .with_field_expiry(vec![("b".to_owned(), 300), ("a".to_owned(), 100)]);
    let decoded = StampedValue::decode(value.encode().as_slice()).unwrap();
    assert_eq!(
        decoded.field_expiry(),
        &[("a".to_owned(), 100), ("b".to_owned(), 300)]
    );
    assert_eq!(decoded.expires(), Some(9_000));
    assert_eq!(decoded.value(), &RecordValue::Present(hash_payload()));
    // Without any, the bytes are what they always were.
    let plain = StampedValue::new(RecordValue::Present(hash_payload())).expiring(9_000);
    assert_eq!(
        plain.encode().as_slice(),
        plain
            .clone()
            .with_field_expiry(Vec::new())
            .encode()
            .as_slice()
    );
}

#[test]
fn a_reader_does_not_see_a_field_past_its_instant() {
    let value = StampedValue::new(RecordValue::Present(hash_payload()))
        .with_field_expiry(vec![("a".to_owned(), 100)]);
    assert_eq!(fields_of(value.clone().into_visible_at(99)), ["a", "b"]);
    assert_eq!(fields_of(value.into_visible_at(100)), ["b"]);
}

#[test]
fn a_hash_whose_every_field_passed_is_absent() {
    let value = StampedValue::new(RecordValue::Present(hash_payload()))
        .with_field_expiry(vec![("a".to_owned(), 100), ("b".to_owned(), 200)]);
    assert_eq!(fields_of(value.clone().into_visible_at(150)), ["b"]);
    assert_eq!(value.into_visible_at(200), RecordValue::Tombstone);
}

#[test]
fn a_deletion_drops_field_instants_rather_than_writing_them() {
    let value =
        StampedValue::new(RecordValue::Tombstone).with_field_expiry(vec![("a".to_owned(), 100)]);
    assert_eq!(value.encode().as_slice(), &[CODEC_VERSION, FLAG_TOMBSTONE]);
}

#[test]
fn a_field_named_twice_keeps_its_last_instant() {
    let value = StampedValue::new(RecordValue::Present(hash_payload())).with_field_expiry(vec![
        ("a".to_owned(), 100),
        ("b".to_owned(), 200),
        ("a".to_owned(), 500),
    ]);
    assert_eq!(
        value.field_expiry(),
        &[("a".to_owned(), 500), ("b".to_owned(), 200)]
    );
}
