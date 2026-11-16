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
