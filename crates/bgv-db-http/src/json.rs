//! Writing a value as JSON, and what that costs.
//!
//! JSON has six types and this store has fifteen, so the mapping is a decision
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

use std::collections::BTreeMap;

use bgv_db::{TableId, Value};

/// What a table id is called, for the references an answer carries.
///
/// A record reference holds an **id** and the name lives in the catalog, so
/// without this a client receives `"1:2"` — indistinguishable from a reference
/// it could follow, and not one. `Db::names_in` builds it once per answer, and
/// only when the answer holds a reference at all.
pub(crate) type Names = BTreeMap<TableId, String>;

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
        // `bgv_db_types::text`, so what a client receives parses back through
        // the reader that read it from a script.
        Value::Duration(held) => string(out, &held.to_literal()),
        Value::Datetime(held) => string(out, &held.to_rfc3339()),
        Value::Uuid(_) | Value::Table(_) | Value::Record(_) | Value::Range(_) => {
            string(out, &spelled(value, names));
        }
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

/// A number, exact where JSON allows and quoted where it does not.
fn number_of(out: &mut String, number: &bgv_db::Number) {
    match number {
        bgv_db::Number::Integer(held) => out.push_str(&held.to_string()),
        // Quoted, because a JSON number is a double and `dec` exists to keep an
        // amount of money from being one.
        bgv_db::Number::Decimal(held) => string(out, &held.to_string()),
        bgv_db::Number::Float(held) => {
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

    use std::collections::BTreeMap;

    use bgv_db::{Number, Value};

    use super::{Names, write};

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
    fn a_container_keeps_its_shape() {
        let nested = Value::Array(vec![
            Value::Bool(true),
            Value::Object(BTreeMap::from([("n".to_owned(), Value::from("x"))])),
        ]);
        assert_eq!(json(&nested), r#"[true,{"n":"x"}]"#);
    }
}
