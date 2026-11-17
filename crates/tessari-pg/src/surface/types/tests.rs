//! The type correspondence: one row per store value — its text form, its
//! type id, and the NULL the two absences share.

use super::{oid, size_of, text_of, type_of};
use tessaridb::{Number, Value};

#[test]
fn the_two_absences_read_as_null() {
    assert_eq!(text_of(&Value::None), None);
    assert_eq!(text_of(&Value::Null), None);
    assert_eq!(type_of(&Value::None), oid::TEXT);
}

#[test]
fn booleans_spell_t_and_f() {
    assert_eq!(text_of(&Value::Bool(true)), Some("t".to_owned()));
    assert_eq!(text_of(&Value::Bool(false)), Some("f".to_owned()));
    assert_eq!(type_of(&Value::Bool(true)), oid::BOOL);
    assert_eq!(size_of(oid::BOOL), 1);
}

#[test]
fn integers_are_the_widest_kind() {
    assert_eq!(
        text_of(&Value::Number(Number::Integer(42))),
        Some("42".to_owned())
    );
    assert_eq!(type_of(&Value::Number(Number::Integer(42))), oid::INT8);
    assert_eq!(size_of(oid::INT8), 8);
}

#[test]
fn floats_and_decimals_keep_their_own_types() {
    assert_eq!(type_of(&Value::Number(Number::float(1.5))), oid::FLOAT8);
    let decimal = Number::Integer(1)
        .as_decimal()
        .expect("an integer is a decimal too");
    assert_eq!(
        type_of(&Value::Number(Number::Decimal(decimal))),
        oid::NUMERIC
    );
}

#[test]
fn strings_pass_verbatim() {
    assert_eq!(
        text_of(&Value::String("héllo".to_owned())),
        Some("héllo".to_owned())
    );
    assert_eq!(type_of(&Value::String("x".to_owned())), oid::TEXT);
}

#[test]
fn bytes_are_the_hex_form() {
    assert_eq!(
        text_of(&Value::Bytes(vec![0xde, 0xad])),
        Some("\\xdead".to_owned())
    );
    assert_eq!(type_of(&Value::Bytes(vec![])), oid::BYTEA);
}

#[test]
fn uuids_are_hyphenated() {
    let bytes = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    assert_eq!(
        text_of(&Value::Uuid(bytes)),
        Some("01234567-89ab-cdef-0123-456789abcdef".to_owned())
    );
    assert_eq!(type_of(&Value::Uuid(bytes)), oid::UUID);
    assert_eq!(size_of(oid::UUID), 16);
}

#[test]
fn objects_arrays_sets_and_ranges_answer_as_json() {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("a".to_owned(), Value::Number(1.into()));
    assert_eq!(
        text_of(&Value::Object(fields)),
        Some("{\"a\":1}".to_owned())
    );
    assert_eq!(type_of(&Value::Array(vec![Value::Bool(true)])), oid::JSONB);
    assert_eq!(
        type_of(&Value::Set(std::collections::BTreeSet::new())),
        oid::JSONB
    );
}

#[test]
fn variable_width_types_report_minus_one() {
    assert_eq!(size_of(oid::TEXT), -1);
    assert_eq!(size_of(oid::NUMERIC), -1);
    assert_eq!(size_of(oid::JSONB), -1);
    assert_eq!(size_of(oid::TIMESTAMPTZ), 8);
}

#[test]
fn timestamptz_binary_counts_micros_from_the_dialect_epoch() {
    use super::{binary_of, value_of_binary};
    // The dialect's epoch is 2000-01-01: zero micros is that instant.
    let zero = value_of_binary(oid::TIMESTAMPTZ, &0_i64.to_be_bytes()).expect("zero micros read");
    match zero {
        Value::Datetime(instant) => {
            assert_eq!(
                instant.seconds(),
                946_684_800,
                "the dialect epoch is Unix + 30 years"
            );
            assert_eq!(instant.nanos(), 0);
        }
        other => panic!("zero micros is the dialect epoch instant: {other:?}"),
    }
    // One day after the epoch, and the way back.
    let day = 86_400_000_000_i64;
    let instant = value_of_binary(oid::TIMESTAMPTZ, &day.to_be_bytes()).expect("a day reads");
    let back = binary_of(&instant, oid::TIMESTAMPTZ)
        .expect("the instant encodes")
        .expect("not null");
    assert_eq!(back, day.to_be_bytes());
    // Unix's own epoch is 30 years of micros before the dialect's.
    let unix = value_of_binary(oid::TIMESTAMPTZ, &(-946_684_800_000_000_i64).to_be_bytes())
        .expect("unix epoch reads");
    match unix {
        Value::Datetime(instant) => {
            assert_eq!(
                instant.seconds(),
                0,
                "Unix epoch is zero micros before the offset"
            );
            assert_eq!(instant.nanos(), 0);
        }
        other => panic!("the unix epoch instant: {other:?}"),
    }
}

#[test]
fn uuid_binary_is_the_sixteen_bytes() {
    use super::{binary_of, value_of_binary};
    let bytes = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    let held = value_of_binary(oid::UUID, &bytes).expect("the bytes read");
    assert_eq!(
        binary_of(&held, oid::UUID)
            .expect("encodes")
            .expect("not null"),
        bytes
    );
    assert!(
        value_of_binary(oid::UUID, &[0_u8; 15]).is_err(),
        "a short uuid refuses"
    );
}
