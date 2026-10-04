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
