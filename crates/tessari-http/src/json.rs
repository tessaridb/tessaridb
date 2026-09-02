//! Writing a value as JSON, and what that costs.
//!
//! JSON has six types and this store has seventeen, so the mapping is a decision
//! rather than a translation. It is written by hand rather than derived, for
//! exactly that reason: a derived encoder would make the choices below silently.
//!
//! # A decimal becomes a string
//!
//! A JSON number is a double in every parser that matters, so writing `12.34` as
//! a number gives it back as a float — which is the mistake `dec` exists to
//! prevent, arriving at the last step instead of the first. Decimals are
//! therefore quoted, and a caller who wants arithmetic parses them with a
//! decimal type of their own.
//!
//! Integers are written as numbers, because a 64-bit integer is exact in JSON up
//! to 2^53 and quoting every count to protect the ones above it would make every
//! ordinary number awkward. That limit is real and is stated here rather than
//! discovered.
//!
//! # Absent and null are told apart by the key, not the value
//!
//! `none` and `null` are different values here and JSON has one word for both.
//! Rather than pretend, the encoding uses JSON's own vocabulary: a field holding
//! `none` **is not written at all**, and a field holding `null` is written as
//! `null`. At the top level the same rule applies to the envelope's `value` key,
//! so `{"kind":"value"}` and `{"kind":"value","value":null}` are the two answers
//! a caller can tell apart.
//!
//! # Everything else keeps its own spelling
//!
//! A datetime is RFC 3339, a duration and a record reference are the text this
//! language writes them as, bytes are hex. Each is a string, and each parses
//! back through the same reader that read it from a script.
//!
//! # A range keeps its endpoints
//!
//! A range is the exception to the rule above: this language has no text form
//! that writes one back, so there is no spelling to keep. It leaves structurally
//! instead, the way a shape leaves as GeoJSON —
//! `{"start":{"bound":"included","value":1},"end":{"bound":"excluded","value":5}}`.
//!
//! The bound kind is named rather than implied, because `1..5`, `1..=5` and
//! `1..` are three different spans and a shape carrying only two endpoints
//! collapses them onto one. An unbounded end carries no `value` key at all,
//! which is the same rule `none` uses above for a thing that is not there, and
//! is what keeps an open end distinct from an end holding `null`. Each endpoint
//! goes through this same encoder, so it keeps whatever spelling its own type
//! has.
//!
//! The cost is stated rather than hidden: the object is indistinguishable from a
//! field that happens to hold an object with those keys, the same ambiguity a
//! decimal already has with a string. This surface is lossy by design; what it
//! must not do is transmit nothing.

use std::{collections::BTreeMap, ops::Bound};

// `ValueRange` is not re-exported from the front door, and the two crates name
// the same type, so a range is reached through `tessari_types` directly.
use tessari_types::ValueRange;
use tessaridb::{Geometry, Polygon, Position, TableId, Value, geojson_name};

/// What a table id is called, for the references an answer carries.
///
/// A record reference holds an **id** and the name lives in the catalog, so
/// without this a client receives `"1:2"` — indistinguishable from a reference
/// it could follow, and not one. `Db::names_in` builds it once per answer, and
/// only when the answer holds a reference at all.
pub(crate) type Names = BTreeMap<TableId, String>;

/// Append a shape as GeoJSON.
fn geometry(out: &mut String, shape: &Geometry) {
    out.push_str(r#"{"type":""#);
    out.push_str(geojson_name(shape));
    out.push_str(r#"","#);
    match shape {
        Geometry::Collection(shapes) => {
            out.push_str(r#""geometries":["#);
            for (position, held) in shapes.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                geometry(out, held);
            }
            out.push(']');
        }
        Geometry::Point(held) => {
            out.push_str(r#""coordinates":"#);
            position(out, held);
        }
        Geometry::Line(held) | Geometry::MultiPoint(held) => {
            out.push_str(r#""coordinates":"#);
            positions(out, held);
        }
        Geometry::Polygon(held) => {
            out.push_str(r#""coordinates":"#);
            polygon(out, held);
        }
        Geometry::MultiLine(lines) => {
            out.push_str(r#""coordinates":["#);
            for (at, line) in lines.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                positions(out, line);
            }
            out.push(']');
        }
        Geometry::MultiPolygon(polygons) => {
            out.push_str(r#""coordinates":["#);
            for (at, held) in polygons.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                polygon(out, held);
            }
            out.push(']');
        }
    }
    out.push('}');
}

fn position(out: &mut String, held: &Position) {
    out.push('[');
    coordinate(out, held.longitude);
    out.push(',');
    coordinate(out, held.latitude);
    out.push(']');
}

/// One coordinate.
///
/// A non-finite coordinate has no JSON spelling — the format has no `NaN` and no
/// infinity — and `null` is the only honest answer. It cannot arrive from a
/// well-formed shape; it can arrive from bytes, and this surface reports what it
/// was given rather than inventing a number.
fn coordinate(out: &mut String, value: f64) {
    if value.is_finite() {
        out.push_str(&format!("{value}"));
    } else {
        out.push_str("null");
    }
}

fn positions(out: &mut String, held: &[Position]) {
    out.push('[');
    for (at, one) in held.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        position(out, one);
    }
    out.push(']');
}

fn polygon(out: &mut String, held: &Polygon) {
    out.push('[');
    positions(out, &held.exterior.0);
    for interior in &held.interiors {
        out.push(',');
        positions(out, &interior.0);
    }
    out.push(']');
}

/// Append `value` to `out` as JSON.
pub(crate) fn write(out: &mut String, value: &Value, names: &Names) {
    match value {
        // Reached only at a position that has already decided to write
        // something; a field or an envelope key holding `none` is omitted by its
        // caller before this is called.
        Value::None | Value::Null => out.push_str("null"),
        Value::Bool(held) => out.push_str(if *held { "true" } else { "false" }),
        Value::Number(number) => number_of(out, number),
        Value::String(text) => string(out, text),
        Value::Bytes(bytes) => {
            let mut hex = String::with_capacity(bytes.len().saturating_mul(2));
            for byte in bytes {
                hex.push_str(&format!("{byte:02x}"));
            }
            string(out, &hex);
        }
        // Both use the writers that live beside their readers in
        // `tessari_types::text`, so what a client receives parses back through
        // the reader that read it from a script.
        // GeoJSON (RFC 7946), which is what every mapping tool already reads,
        // so a shape leaves this surface without needing a translation. The one
        // rule worth restating at the boundary: coordinates are
        // `[longitude, latitude]`, and the reversed order is the classic silent
        // geo bug rather than a formatting preference.
        Value::Geometry(shape) => geometry(out, shape),
        // The pattern's source, as text. This store does not execute it, and
        // rendering it as a plain string says exactly that — a client that wants
        // to run it knows it is a pattern from the field's declared type.
        Value::Regex(pattern) => string(out, pattern),
        Value::Duration(held) => string(out, &held.to_literal()),
        Value::Datetime(held) => string(out, &held.to_rfc3339()),
        Value::Uuid(_) | Value::Table(_) | Value::Record(_) => {
            string(out, &spelled(value, names));
        }
        // Structurally, because there is no text form to keep: `Value`'s own
        // `Display` writes `<range>`, and routing this through `spelled` is what
        // sent the caller the word `range` and nothing else.
        Value::Range(held) => range(out, held, names),
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                write(out, item, names);
            }
            out.push(']');
        }
        Value::Set(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                write(out, item, names);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            let mut written = 0_usize;
            for (name, held) in fields {
                // A field holding `none` is not there, which is what `none`
                // means. Writing it as `null` would erase the distinction JSON
                // cannot otherwise carry.
                if !held.is_present() {
                    continue;
                }
                if written > 0 {
                    out.push(',');
                }
                string(out, name);
                out.push(':');
                write(out, held, names);
                written = written.saturating_add(1);
            }
            out.push('}');
        }
    }
}

/// A range as its two endpoints and their bound kinds.
fn range(out: &mut String, held: &ValueRange, names: &Names) {
    out.push_str(r#"{"start":"#);
    bound(out, &held.start, names);
    out.push_str(r#","end":"#);
    bound(out, &held.end, names);
    out.push('}');
}

/// One end of a range: which kind of bound it is, and the value where there is
/// one.
///
/// An unbounded end carries no `value` key, so it cannot be confused with an end
/// holding `null`. Writing it as `null` would make `1..` and `1..=null` the same
/// document.
fn bound(out: &mut String, held: &Bound<Value>, names: &Names) {
    let (kind, value) = match held {
        Bound::Included(value) => ("included", Some(value)),
        Bound::Excluded(value) => ("excluded", Some(value)),
        Bound::Unbounded => ("unbounded", None),
    };
    out.push_str(r#"{"bound":"#);
    string(out, kind);
    if let Some(value) = value {
        out.push_str(r#","value":"#);
        write(out, value, names);
    }
    out.push('}');
}

/// A number, exact where JSON allows and quoted where it does not.
fn number_of(out: &mut String, number: &tessaridb::Number) {
    match number {
        tessaridb::Number::Integer(held) => out.push_str(&held.to_string()),
        // Quoted, because a JSON number is a double and `dec` exists to keep an
        // amount of money from being one.
        tessaridb::Number::Decimal(held) => string(out, &held.to_string()),
        tessaridb::Number::Float(held) => {
            // JSON has no infinity and no not-a-number. Writing one unquoted
            // produces a document most parsers reject, so they are spelled and
            // quoted — and a distance of `+∞` is a real answer this store gives.
            if held.is_finite() {
                out.push_str(&held.to_string());
            } else {
                string(out, &held.to_string());
            }
        }
    }
}

/// The text this language writes a value as.
fn spelled(value: &Value, names: &Names) -> String {
    match value {
        Value::Uuid(bytes) => {
            let mut text = String::with_capacity(36);
            for (position, byte) in bytes.iter().enumerate() {
                if matches!(position, 4 | 6 | 8 | 10) {
                    text.push('-');
                }
                text.push_str(&format!("{byte:02x}"));
            }
            text
        }
        // The name where the catalog has one. A table that has been dropped
        // keeps its id, the way a reference to a deleted record does — and it is
        // spelled visibly as an id rather than as a name that would not resolve.
        Value::Table(id) => names
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("<table {id}>")),
        Value::Record(reference) => names.get(&reference.table).map_or_else(
            || format!("<record {reference}>"),
            |named| format!("{named}:{}", reference.id),
        ),
        other => other.type_name().to_owned(),
    }
}

/// A JSON string, escaped, as a value rather than appended.
pub(crate) fn string_literal(text: &str) -> String {
    let mut out = String::new();
    string(&mut out, text);
    out
}

/// A JSON string, escaped.
pub(crate) fn string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything below a space has to be escaped for a document to be
            // valid, and the numeric form is the one that always works.
            control if control < ' ' => out.push_str(&format!("\\u{:04x}", u32::from(control))),
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
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
}
