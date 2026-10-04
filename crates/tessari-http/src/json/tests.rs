#![allow(clippy::panic)]

use std::{collections::BTreeMap, ops::Bound};

use tessari_types::{RecordId, RecordRef, TableId, ValueRange};
use tessaridb::{Number, Value};

use super::{Names, write};

fn span(start: Bound<Value>, end: Bound<Value>) -> Value {
    Value::Range(Box::new(ValueRange { start, end }))
}

fn at(number: i64) -> Value {
    Value::Number(Number::Integer(number))
}

fn json(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, &Names::new());
    out
}

#[test]
fn absent_and_null_are_told_apart_by_the_key() {
    // JSON has one word for both, so the encoding uses JSON's own vocabulary
    // rather than pretending: a field that is not there is not written.
    let record = Value::Object(BTreeMap::from([
        ("here".to_owned(), Value::Null),
        ("gone".to_owned(), Value::None),
    ]));
    assert_eq!(json(&record), r#"{"here":null}"#);
}

#[test]
fn a_decimal_is_quoted_and_an_integer_is_not() {
    // A JSON number is a double in every parser that matters, which is the
    // mistake `dec` exists to prevent, arriving at the last step instead of
    // the first.
    assert_eq!(json(&Value::Number(Number::Integer(42))), "42");
    let exact = "12.34".parse().expect("a decimal");
    assert_eq!(json(&Value::Number(Number::Decimal(exact))), r#""12.34""#);
}

#[test]
fn what_json_has_no_word_for_is_quoted_rather_than_invalid() {
    // A `+∞` distance is a real answer this store gives, and an unquoted one
    // produces a document most parsers reject.
    assert_eq!(
        json(&Value::Number(Number::float(f64::INFINITY))),
        r#""inf""#
    );
    assert_eq!(json(&Value::Number(Number::float(1.5))), "1.5");
}

#[test]
fn a_string_survives_what_would_break_the_document() {
    assert_eq!(json(&Value::from("a\"b\\c")), r#""a\"b\\c""#);
    assert_eq!(json(&Value::from("line\nbreak")), r#""line\nbreak""#);
    // A control character is escaped numerically, which is the form
    // that always produces a valid document.
    assert_eq!(json(&Value::from("\u{1}")), r#""\u0001""#);
}

#[test]
fn a_range_arrives_as_its_endpoints_rather_than_as_its_type() {
    // The defect this replaces: `RETURN 1..5;` answered `"range"`, so
    // nothing about the span reached the caller at all and there was
    // nothing on the other side to recover it from.
    assert_eq!(
        json(&span(Bound::Included(at(1)), Bound::Excluded(at(5)))),
        r#"{"start":{"bound":"included","value":1},"end":{"bound":"excluded","value":5}}"#
    );
}

#[test]
fn the_three_bound_kinds_stay_apart() {
    // An open end is not an end holding `null`: rendering `Unbounded` as a
    // `null` value would collide with `Bound::Included(Value::Null)`, and a
    // shape carrying only two endpoints would collapse `1..5`, `1..=5` and
    // `1..` onto one document — the same loss in a smaller form.
    let open = json(&span(Bound::Included(at(1)), Bound::Unbounded));
    let holding_null = json(&span(Bound::Included(at(1)), Bound::Included(Value::Null)));
    assert_eq!(
        open,
        r#"{"start":{"bound":"included","value":1},"end":{"bound":"unbounded"}}"#
    );
    assert_eq!(
        holding_null,
        r#"{"start":{"bound":"included","value":1},"end":{"bound":"included","value":null}}"#
    );
    assert_ne!(open, holding_null);
    assert_ne!(
        json(&span(Bound::Included(at(1)), Bound::Included(at(5)))),
        json(&span(Bound::Included(at(1)), Bound::Excluded(at(5))))
    );
}

#[test]
fn an_endpoint_keeps_the_spelling_of_its_own_type() {
    // The endpoint goes through this same encoder, so a decimal endpoint is
    // quoted for the reason every decimal on this surface is quoted.
    let exact = "12.34".parse().expect("a decimal");
    assert_eq!(
        json(&span(
            Bound::Included(Value::Number(Number::Decimal(exact))),
            Bound::Excluded(at(99))
        )),
        r#"{"start":{"bound":"included","value":"12.34"},"end":{"bound":"excluded","value":99}}"#
    );
}

#[test]
fn a_container_keeps_its_shape() {
    let nested = Value::Array(vec![
        Value::Bool(true),
        Value::Object(BTreeMap::from([("n".to_owned(), Value::from("x"))])),
    ]);
    assert_eq!(json(&nested), r#"[true,{"n":"x"}]"#);
}

#[test]
fn a_record_id_is_written_plainly_and_is_not_a_query_literal() {
    // Specification §5.7.1, "A record id is written plainly, and is NOT a
    // TessariQL literal": the id half is the id's own text, with no quoting
    // and no escaping, and the section states the consequences it buys —
    // `users:7` is the integer and the text `'7'` written identically, and a
    // client **MUST NOT** parse this string back into a typed record id.
    //
    // Pinned here because the spelling looks like a defect from inside this
    // crate and is not one. `RecordId::to_literal` exists for the CLI and for
    // the places that do round-trip; reaching for it here would silently
    // break a published surface, and this test is what says no. The four
    // spellings below were exercised against a running node on `a3c22dd`.
    let table = TableId::new(7);
    let mut names = Names::new();
    names.insert(table, "people".to_owned());
    let spell = |id: RecordId| {
        let mut out = String::new();
        write(&mut out, &Value::Record(RecordRef { table, id }), &names);
        out
    };

    assert_eq!(spell(RecordId::Int(1)), r#""people:1""#);
    // Unquoted and unescaped, space and all — §5.7.1's `users:has space` row.
    assert_eq!(spell(RecordId::from("ada smith")), r#""people:ada smith""#);
    // Undivided: the id half drops the hyphens the value form carries, which
    // §5.7.1 calls out as the row a client's uuid parser gets wrong.
    assert_eq!(
        spell(RecordId::Uuid([
            0x01, 0x91, 0xf4, 0xe2, 0x1c, 0x3a, 0x7b, 0x4d, 0x8e, 0x5f, 0x6a, 0x7b, 0x8c, 0x9d,
            0x0e, 0x1f
        ])),
        r#""people:0191f4e21c3a7b4d8e5f6a7b8c9d0e1f""#
    );
    // The mirrored row: bytes GAIN a `0x` the value form does not carry.
    assert_eq!(
        spell(RecordId::Bytes(vec![0x0a, 0x0b])),
        r#""people:0x0a0b""#
    );
}

#[test]
fn a_record_whose_table_cannot_be_named_is_visibly_not_a_name() {
    // §5.7.1: `<record T:id>` with the brackets, where `T` is the table's
    // numeric id. `<` cannot begin a table name, which is what lets a client
    // detect the form. The specification's own example is `<record 7:1>`, and
    // an empty `names` is exactly the dropped-table case that produces it.
    let mut out = String::new();
    write(
        &mut out,
        &Value::Record(RecordRef {
            table: TableId::new(7),
            id: RecordId::Int(1),
        }),
        &Names::new(),
    );
    assert_eq!(out, r#""<record 7:1>""#);
}
